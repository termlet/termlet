//! 一条 HTTP 连接与其上的请求：检查、读正文、分发，`tools/call` 用 SSE 回答。

use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt as _, Full, LengthLimitError, Limited};
use hyper::body::{Body, Bytes, Frame, Incoming};
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::Value;
use termlet_mcp::protocol::{self, Message, Session, Step, ToolCall};
use termlet_mcp::{
    HEADER_MISMATCH, INVALID_REQUEST, MAX_MESSAGE, UNSUPPORTED_PROTOCOL_VERSION, tools,
};
use termlet_proto::Lang;
use termlet_proto::mcp::CALLER_HTTP;
use tokio::net::TcpStream;
use tokio::sync::{OnceCell, mpsc, watch};

use super::state::Rejected;
use super::{Inner, peer, token};
use crate::cli::handler;
use crate::cli::service::{Cancel, InFlight, Shared};

type HttpBody = BoxBody<Bytes, Infallible>;

/// 读一个请求头的时限：也是空闲的长连接多久后关闭（占着连接数的上限）。
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);

const SESSION_HEADER: &str = "mcp-session-id";
const VERSION_HEADER: &str = "mcp-protocol-version";
const METHOD_HEADER: &str = "mcp-method";
const NAME_HEADER: &str = "mcp-name";

/// 一条连接：对端、本机地址、发起连接的进程链（第一次调用工具时才查）。
pub(super) struct Conn {
    peer: SocketAddr,
    local: SocketAddr,
    port: u16,
    callers: OnceCell<Vec<String>>,
}

impl Conn {
    pub fn new(peer: SocketAddr, local: SocketAddr, port: u16) -> Arc<Self> {
        Arc::new(Self {
            peer,
            local,
            port,
            callers: OnceCell::new(),
        })
    }

    async fn callers(&self) -> Vec<String> {
        self.callers
            .get_or_init(|| async {
                let (peer, local) = (self.peer, self.local);
                tokio::task::spawn_blocking(move || peer::process_chain(peer, local))
                    .await
                    .unwrap_or_default()
            })
            .await
            .clone()
    }
}

/// 处理一条连接，直到对方关闭、出错或服务停止（停止时进行中的请求做完再关闭）。
pub(super) async fn connection(
    inner: Arc<Inner>,
    stream: TcpStream,
    conn: Arc<Conn>,
    mut stop: watch::Receiver<bool>,
) {
    let _ = stream.set_nodelay(true);
    let mut shutdown = inner.shared.shutdown.subscribe();
    let service = hyper::service::service_fn(move |request| {
        let inner = inner.clone();
        let conn = conn.clone();
        async move { Ok::<_, Infallible>(handle(&inner, &conn, request).await) }
    });
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_TIMEOUT);
    let connection = builder.serve_connection(TokioIo::new(stream), service);
    tokio::pin!(connection);
    tokio::select! {
        result = connection.as_mut() => {
            if let Err(e) = result {
                tracing::debug!("MCP 的 HTTP 连接结束：{e}");
            }
            return;
        }
        _ = async { stop.wait_for(|s| *s).await.is_ok() } => {}
        _ = async { shutdown.wait_for(|s| *s).await.is_ok() } => {}
    }
    connection.as_mut().graceful_shutdown();
    let _ = connection.await;
}

/// 连接数已满：回答 503 后关闭。先等对方发完请求头（请求还没发出就收到回答，客户端会当作协议错误），回答后读完
/// 对方发来的再关（免得对方收到 RST 而看不到回答）。
pub(super) async fn busy(mut stream: TcpStream) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let reply: &[u8] =
        b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        let mut head = Vec::new();
        let mut buf = [0u8; 4096];
        while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 64 << 10 {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            head.extend_from_slice(&buf[..n]);
        }
        stream.write_all(reply).await?;
        stream.shutdown().await?;
        let mut sink = [0u8; 4096];
        while stream.read(&mut sink).await? > 0 {}
        Ok::<_, std::io::Error>(())
    })
    .await;
}

/// 一个请求：先检查（读正文之前），再分发。
async fn handle(
    inner: &Arc<Inner>,
    conn: &Arc<Conn>,
    request: Request<Incoming>,
) -> Response<HttpBody> {
    let headers = request.headers();
    if !host_allowed(headers, conn.port) {
        inner.state.reject(Rejected::Host);
        return rpc_error(
            StatusCode::FORBIDDEN,
            "Forbidden: the Host header must be 127.0.0.1, localhost or [::1] with this port",
        );
    }
    if headers.contains_key(header::ORIGIN) {
        inner.state.reject(Rejected::Origin);
        return rpc_error(
            StatusCode::FORBIDDEN,
            "Forbidden: requests from web pages (with an Origin header) are not accepted",
        );
    }
    if request.uri().path() != "/mcp" {
        return empty(StatusCode::NOT_FOUND);
    }
    if request.method() != Method::POST {
        let mut response = empty(StatusCode::METHOD_NOT_ALLOWED);
        response
            .headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("POST"));
        return response;
    }
    // 先不带 WWW-Authenticate：带了的话 Devin、opencode、Zed 会转去走 OAuth（第 4 步用真实的客户端实测后再定）
    if !authorized(inner, headers).await {
        inner.state.reject(Rejected::Token);
        return rpc_error(
            StatusCode::UNAUTHORIZED,
            "Unauthorized: send the token from Termlet (Settings → AI & automation) as Authorization: Bearer <token>",
        );
    }
    let too_large = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|len| len > MAX_MESSAGE as u64);
    if too_large {
        inner.state.reject(Rejected::TooLarge);
        return rpc_error(StatusCode::PAYLOAD_TOO_LARGE, "message too large");
    }
    let (parts, body) = request.into_parts();
    let bytes = match Limited::new(body, MAX_MESSAGE).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) if e.is::<LengthLimitError>() => {
            inner.state.reject(Rejected::TooLarge);
            return rpc_error(StatusCode::PAYLOAD_TOO_LARGE, "message too large");
        }
        Err(e) => {
            tracing::debug!("读取 MCP 的 HTTP 请求失败：{e}");
            return rpc_error(StatusCode::BAD_REQUEST, "failed to read the request body");
        }
    };
    // 通过鉴权的请求算作活动（无窗口模式的空闲计时、使用统计）
    inner.shared.touch();
    crate::usage::mark_active();
    match protocol::parse(&bytes) {
        Err(reply) => json(StatusCode::BAD_REQUEST, &reply),
        Ok(Message::Response) => empty(StatusCode::ACCEPTED),
        Ok(Message::Notification { method, params }) => {
            if let Some(id) = protocol::cancelled_request(&method, params.as_ref()) {
                let session = header_str(&parts.headers, SESSION_HEADER)
                    .filter(|s| inner.state.session(s).is_some());
                let canceled = inner.state.cancel(session, &id);
                tracing::debug!(canceled, "MCP：取消进行中的工具调用");
            }
            empty(StatusCode::ACCEPTED)
        }
        Ok(Message::Request { id, method, params }) => {
            dispatch(inner, conn, &parts.headers, id, method, params).await
        }
    }
}

/// 分发一个请求：新协议核对请求头与正文，旧协议找会话（找不到时按请求头补一个）。
async fn dispatch(
    inner: &Arc<Inner>,
    conn: &Arc<Conn>,
    headers: &HeaderMap,
    id: Value,
    method: String,
    params: Option<Value>,
) -> Response<HttpBody> {
    if method == "initialize" {
        return match protocol::request(&id, &method, params, None) {
            Step::Initialized { reply, session } => {
                tracing::debug!(client = %session.client, version = session.version, "MCP 的 HTTP 握手");
                let session_id = inner.state.create_session(session);
                let mut response = json(StatusCode::OK, &reply);
                if let Ok(value) = HeaderValue::from_str(&session_id) {
                    response.headers_mut().insert(SESSION_HEADER, value);
                }
                response
            }
            Step::Reply(reply) => json(status_of(&reply, false), &reply),
            Step::Call(call) => tool_call(inner, conn, headers, None, call).await,
        };
    }
    let modern = protocol::meta_version(params.as_ref()).map(str::to_string);
    let (session_id, session) = match &modern {
        Some(version) => {
            if let Err(message) = check_modern_headers(headers, version, &method, params.as_ref()) {
                let reply = protocol::error_message(&id, HEADER_MISMATCH, &message, None);
                return json(StatusCode::BAD_REQUEST, &reply);
            }
            (None, None)
        }
        None => match legacy_session(inner, headers) {
            Ok(found) => found,
            Err((code, message, data)) => {
                let reply = protocol::error_message(&id, code, &message, data);
                return json(StatusCode::BAD_REQUEST, &reply);
            }
        },
    };
    match protocol::request(&id, &method, params, session.as_ref()) {
        Step::Reply(reply) | Step::Initialized { reply, .. } => {
            json(status_of(&reply, modern.is_some()), &reply)
        }
        Step::Call(call) => tool_call(inner, conn, headers, session_id, call).await,
    }
}

/// 新协议的请求头：`MCP-Protocol-Version` 与 `_meta` 中的版本、`Mcp-Method` 与方法、`tools/call` 的 `Mcp-Name` 与工具名
/// 必须一致（缺少也不行）。不一致时为说明。
fn check_modern_headers(
    headers: &HeaderMap,
    version: &str,
    method: &str,
    params: Option<&Value>,
) -> Result<(), String> {
    let check = |name: &str, label: &str, body: Option<&str>, decode: bool| {
        let value = header_str(headers, name)
            .ok_or_else(|| format!("Header mismatch: missing or malformed {label} header"))?;
        let value = if decode {
            decode_sentinel(value)
                .ok_or_else(|| format!("Header mismatch: malformed {label} header"))?
        } else {
            value.to_string()
        };
        match body {
            Some(body) if body == value => Ok(()),
            Some(body) => Err(format!(
                "Header mismatch: {label} header value '{value}' does not match body value '{body}'"
            )),
            None => Err(format!(
                "Header mismatch: {label} header value '{value}' has no matching value in the body"
            )),
        }
    };
    check(VERSION_HEADER, "MCP-Protocol-Version", Some(version), false)?;
    check(METHOD_HEADER, "Mcp-Method", Some(method), false)?;
    if method == "tools/call" {
        let name = params.and_then(|p| p.get("name")).and_then(Value::as_str);
        check(NAME_HEADER, "Mcp-Name", name, true)?;
    }
    Ok(())
}

/// `Mcp-Name` 的值：`=?base64?…?=` 写的先解码（规范：不能直接写成请求头的名字用这种写法）。
fn decode_sentinel(value: &str) -> Option<String> {
    let Some(encoded) = value
        .strip_prefix("=?base64?")
        .and_then(|v| v.strip_suffix("?="))
    else {
        return Some(value.to_string());
    };
    let bytes = data_encoding::BASE64.decode(encoded.as_bytes()).ok()?;
    String::from_utf8(bytes).ok()
}

/// 旧协议的请求的会话：认识的会话 ID 用它记住的；否则（没有、Termlet 重启过、被挤掉了）不回 404，按没有会话处理：
/// 版本取 `MCP-Protocol-Version` 头，没有时按 2025-03-26，客户端名为空。请求头中的版本不支持时为 400 的错误。
fn legacy_session(
    inner: &Inner,
    headers: &HeaderMap,
) -> Result<(Option<String>, Option<Session>), (i64, String, Option<Value>)> {
    let version = match header_str(headers, VERSION_HEADER) {
        None => protocol::HEADERLESS_VERSION,
        Some(value) => match protocol::legacy_version(value) {
            Some(version) => version,
            None if protocol::MODERN_VERSIONS.contains(&value) => {
                return Err((
                    HEADER_MISMATCH,
                    format!(
                        "Header mismatch: MCP-Protocol-Version {value} requires _meta[\"{}\"] in the request",
                        protocol::META_PROTOCOL_VERSION
                    ),
                    None,
                ));
            }
            None => {
                let mut supported: Vec<&str> = protocol::MODERN_VERSIONS.to_vec();
                supported.extend(protocol::LEGACY_VERSIONS);
                return Err((
                    UNSUPPORTED_PROTOCOL_VERSION,
                    "Unsupported protocol version".into(),
                    Some(serde_json::json!({ "supported": supported, "requested": value })),
                ));
            }
        },
    };
    if let Some(id) = header_str(headers, SESSION_HEADER)
        && let Some(session) = inner.state.session(id)
    {
        return Ok((Some(id.to_string()), Some(session)));
    }
    Ok((
        None,
        Some(Session {
            version,
            client: String::new(),
        }),
    ))
}

/// `tools/call`：在后台执行，用 SSE 回答（回答头马上发出，定时保活，最后一条事件是结果）；客户端关闭这个流即取消。
/// 客户端的 `Accept` 不收 SSE 时做完再回答 JSON。
async fn tool_call(
    inner: &Arc<Inner>,
    conn: &Arc<Conn>,
    headers: &HeaderMap,
    session: Option<String>,
    call: ToolCall,
) -> Response<HttpBody> {
    let shared = inner.shared.clone();
    let lang = shared.lang();
    let mut rest = vec![CALLER_HTTP.to_string()];
    rest.extend(conn.callers().await);
    let callers = call.callers(rest);
    let (gone_tx, gone) = watch::channel(false);
    let gone_tx = Arc::new(gone_tx);
    let tracked = inner
        .state
        .track(session, call.id.to_string(), gone_tx.clone());
    let cancel = Cancel::new(gone.clone(), shared.shutdown.subscribe());
    tracing::debug!(tool = call.tool.name(), "MCP 的 HTTP：调用工具");
    let work = run(shared, call, callers, lang, cancel, gone);
    if !accepts_event_stream(headers) {
        let reply = work.await;
        drop(tracked);
        return match reply {
            Some(reply) => json(StatusCode::OK, &reply),
            None => empty(StatusCode::ACCEPTED),
        };
    }
    let (tx, rx) = mpsc::channel::<Bytes>(8);
    let keepalive = inner.options.keepalive;
    crate::runtime::spawn(async move {
        let _tracked = tracked;
        tokio::pin!(work);
        let mut ticker =
            tokio::time::interval_at(tokio::time::Instant::now() + keepalive, keepalive);
        let mut closed = false;
        let reply = loop {
            tokio::select! {
                reply = &mut work => break reply,
                _ = ticker.tick(), if !closed => {
                    // 发不出去（客户端没在读）时跳过这一次；连接已关闭时取消
                    if let Err(mpsc::error::TrySendError::Closed(_)) =
                        tx.try_send(Bytes::from_static(b": keepalive\n\n"))
                    {
                        closed = true;
                        gone_tx.send_replace(true);
                    }
                }
                _ = tx.closed(), if !closed => {
                    closed = true;
                    gone_tx.send_replace(true);
                }
            }
        };
        if let Some(reply) = reply
            && !closed
        {
            let event = format!("event: message\ndata: {reply}\n\n");
            let _ = tx.send(Bytes::from(event)).await;
        }
    });
    let mut response = Response::new(SseBody(rx).boxed());
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

/// 执行一个工具调用：检查参数、交给处理命令行请求的同一套代码、写出结果。被取消（客户端断开）时为 `None`：不再回答。
async fn run(
    shared: Arc<Shared>,
    call: ToolCall,
    callers: Vec<String>,
    lang: Lang,
    cancel: Cancel,
    gone: watch::Receiver<bool>,
) -> Option<Value> {
    let request = match tools::request(call.tool, &call.arguments, lang, &callers) {
        Ok(request) => request,
        Err(outcome) => return Some(call.reply(outcome)),
    };
    let terminal = tools::in_terminal(&request);
    let response = {
        let _busy = InFlight::enter(&shared);
        handler::handle(&shared, lang, request, cancel).await
    };
    if *gone.borrow() {
        tracing::debug!(
            tool = call.tool.name(),
            "MCP 的 HTTP：工具调用已取消，不再回答"
        );
        return None;
    }
    Some(call.reply(tools::outcome(call.tool, terminal, Ok(response), lang)))
}

/// `Host` 是本机的这个端口（只有一个 `Host` 头）。
fn host_allowed(headers: &HeaderMap, port: u16) -> bool {
    let mut hosts = headers.get_all(header::HOST).iter();
    let (Some(host), None) = (hosts.next(), hosts.next()) else {
        return false;
    };
    let Some((name, host_port)) = host.to_str().ok().and_then(|h| h.rsplit_once(':')) else {
        return false;
    };
    host_port == port.to_string()
        && (name == "127.0.0.1" || name == "[::1]" || name.eq_ignore_ascii_case("localhost"))
}

/// `Authorization: Bearer <令牌>` 是否正确（读数据库，在后台线程上）。
async fn authorized(inner: &Inner, headers: &HeaderMap) -> bool {
    let Some(presented) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token.trim().to_string())
    else {
        return false;
    };
    let store = inner.shared.store.clone();
    tokio::task::spawn_blocking(move || token::verify(&store, &presented))
        .await
        .unwrap_or(false)
}

/// `Accept` 中有 SSE（或没写 `Accept`、写了通配）。
fn accepts_event_stream(headers: &HeaderMap) -> bool {
    let mut values = headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .peekable();
    if values.peek().is_none() {
        return true;
    }
    values.flat_map(|v| v.split(',')).any(|item| {
        let media = item.split(';').next().unwrap_or_default().trim();
        media.eq_ignore_ascii_case("text/event-stream")
            || media == "*/*"
            || media.eq_ignore_ascii_case("text/*")
    })
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

/// JSON-RPC 回答的 HTTP 状态：协议层面的错误 400；新协议中不认识的方法 404（与旧的 HTTP+SSE 服务的 404 靠正文区分）；
/// 其余（含旧协议的方法不存在：旧客户端把 404 当作会话已结束）200。
fn status_of(reply: &Value, modern: bool) -> StatusCode {
    match protocol::error_code(reply) {
        Some(
            protocol::PARSE_ERROR
            | protocol::INVALID_REQUEST
            | protocol::HEADER_MISMATCH
            | protocol::UNSUPPORTED_PROTOCOL_VERSION,
        ) => StatusCode::BAD_REQUEST,
        Some(protocol::METHOD_NOT_FOUND) if modern => StatusCode::NOT_FOUND,
        _ => StatusCode::OK,
    }
}

fn json(status: StatusCode, value: &Value) -> Response<HttpBody> {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let mut response = Response::new(Full::new(Bytes::from(bytes)).boxed());
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

/// 被拒绝的请求：没有 ID 的 JSON-RPC 错误。
fn rpc_error(status: StatusCode, message: &str) -> Response<HttpBody> {
    json(
        status,
        &protocol::error_message(&Value::Null, INVALID_REQUEST, message, None),
    )
}

fn empty(status: StatusCode) -> Response<HttpBody> {
    let mut response = Response::new(Full::new(Bytes::new()).boxed());
    *response.status_mut() = status;
    response
}

/// SSE 回答的正文：后台任务经通道写进来；hyper 丢弃它（连接关闭）时通道随之关闭，任务据此取消调用。
struct SseBody(mpsc::Receiver<Bytes>);

impl Body for SseBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.0
            .poll_recv(cx)
            .map(|item| item.map(|bytes| Ok(Frame::data(bytes))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn host_must_be_this_machine_and_port() {
        for ok in [
            "127.0.0.1:47801",
            "localhost:47801",
            "LocalHost:47801",
            "[::1]:47801",
        ] {
            assert!(host_allowed(&headers(&[("host", ok)]), 47801), "{ok}");
        }
        for bad in [
            "127.0.0.1:47800",
            "127.0.0.1",
            "evil.example:47801",
            "127.0.0.1.evil.example:47801",
            "[::2]:47801",
            "",
        ] {
            assert!(!host_allowed(&headers(&[("host", bad)]), 47801), "{bad}");
        }
        assert!(!host_allowed(&HeaderMap::new(), 47801), "没有 Host");
        let twice = headers(&[("host", "127.0.0.1:47801"), ("host", "127.0.0.1:47801")]);
        assert!(!host_allowed(&twice, 47801), "两个 Host");
    }

    #[test]
    fn modern_headers_must_match_the_body() {
        let params = serde_json::json!({ "name": "list_hosts" });
        let good = headers(&[
            ("mcp-protocol-version", "2026-07-28"),
            ("mcp-method", "tools/call"),
            ("mcp-name", "list_hosts"),
        ]);
        assert!(check_modern_headers(&good, "2026-07-28", "tools/call", Some(&params)).is_ok());
        // 工具名可以用 base64 的写法
        let encoded = headers(&[
            ("mcp-protocol-version", "2026-07-28"),
            ("mcp-method", "tools/call"),
            ("mcp-name", "=?base64?bGlzdF9ob3N0cw==?="),
        ]);
        assert!(check_modern_headers(&encoded, "2026-07-28", "tools/call", Some(&params)).is_ok());
        for (bad, why) in [
            (
                headers(&[("mcp-method", "tools/call"), ("mcp-name", "list_hosts")]),
                "缺少版本",
            ),
            (
                headers(&[
                    ("mcp-protocol-version", "2025-11-25"),
                    ("mcp-method", "tools/call"),
                    ("mcp-name", "list_hosts"),
                ]),
                "版本不一致",
            ),
            (
                headers(&[
                    ("mcp-protocol-version", "2026-07-28"),
                    ("mcp-method", "tools/list"),
                    ("mcp-name", "list_hosts"),
                ]),
                "方法不一致",
            ),
            (
                headers(&[
                    ("mcp-protocol-version", "2026-07-28"),
                    ("mcp-method", "tools/call"),
                ]),
                "缺少工具名",
            ),
            (
                headers(&[
                    ("mcp-protocol-version", "2026-07-28"),
                    ("mcp-method", "tools/call"),
                    ("mcp-name", "run_command"),
                ]),
                "工具名不一致",
            ),
            (
                headers(&[
                    ("mcp-protocol-version", "2026-07-28"),
                    ("mcp-method", "tools/call"),
                    ("mcp-name", "=?base64?!!?="),
                ]),
                "base64 写错",
            ),
        ] {
            let error = check_modern_headers(&bad, "2026-07-28", "tools/call", Some(&params));
            assert!(error.unwrap_err().starts_with("Header mismatch"), "{why}");
        }
        // 只有 tools/call 要工具名
        let list = headers(&[
            ("mcp-protocol-version", "2026-07-28"),
            ("mcp-method", "tools/list"),
        ]);
        assert!(check_modern_headers(&list, "2026-07-28", "tools/list", None).is_ok());
    }

    #[test]
    fn accept_header_decides_sse() {
        assert!(accepts_event_stream(&HeaderMap::new()), "没写 Accept");
        assert!(accepts_event_stream(&headers(&[(
            "accept",
            "application/json, text/event-stream"
        )])));
        assert!(accepts_event_stream(&headers(&[("accept", "*/*")])));
        assert!(!accepts_event_stream(&headers(&[(
            "accept",
            "application/json"
        )])));
    }

    #[test]
    fn status_follows_the_error() {
        let ok = protocol::result_message(&Value::from(1), Value::Null);
        assert_eq!(status_of(&ok, true), StatusCode::OK);
        let error = |code| protocol::error_message(&Value::from(1), code, "x", None);
        assert_eq!(
            status_of(&error(protocol::METHOD_NOT_FOUND), true),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status_of(&error(protocol::METHOD_NOT_FOUND), false),
            StatusCode::OK
        );
        assert_eq!(
            status_of(&error(protocol::UNSUPPORTED_PROTOCOL_VERSION), true),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status_of(&error(protocol::INVALID_PARAMS), true),
            StatusCode::OK
        );
    }
}
