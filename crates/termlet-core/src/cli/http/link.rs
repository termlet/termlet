//! 局域网中的客户端传文件：一次性传输链接 `http://<地址>:<端口>/transfer/<随机串>`。
//!
//! 局域网中的 AI 调用 `upload_file` / `download_file` 时只给服务器上的路径（[`offer`]）：检查过（主机授权、保险库，连上
//! 服务器看一眼路径，见 `cli::handler::link`）后给出链接，AI 在它那台电脑上用 curl 收发：上传 `curl -T`（PUT），下载
//! `curl -o`（GET）。Termlet 把 HTTP 的正文与服务器上的文件经 SFTP（没有时 SCP）对接（[`serve`]）：这台电脑上不落
//! 文件、碰不到这里的文件，内容也不经过 AI 的上下文。
//!
//! - 随机串：256 位系统随机数（52 个 base32 字符），链接本身就是凭据（curl 不带局域网令牌）。只存它的 SHA-256 摘要，
//!   查找时与每个链接的摘要逐个做常量时间比较；完整的地址只出现在给 AI 的工具结果中，不进日志、`Debug` 输出与审计
//!   （日志中用摘要的前 4 字节区分链接）。
//! - 绑定：主机、服务器上的路径、方向，与取链接的客户端：只有取链接的那个地址能用，审计的调用方照它记。
//! - 一次性：开始传输即作废，失败了也不能再用，要重新调用工具取新的（取的时候重新检查，说明出了什么问题）。方法
//!   不对、上传没有 Content-Length 这类还没开始的请求不算用过。从给出到开始传输最多 [`LINK_TTL`]，开始以后不限时长。
//! - 同时没用过的链接最多 [`MAX_LINKS`] 个。过期的在查找、给新链接时清掉，不另开定时器。监听停止、换端口或范围、
//!   局域网令牌重新生成后全部作废。
//! - 上传要带 Content-Length（SCP 要先知道大小，也据此判断收齐没有），先写服务器上的临时文件，收齐后改名，中途断开时
//!   删除临时文件；下载的回答带 Content-Length（正文正好是打开时的大小）与 Content-Disposition。不认 Range。一次读写
//!   [`TRANSFER_IDLE`] 等不到对方即中止。传输在单独的任务中进行：连接断开时 hyper 丢弃请求的处理，任务照样收尾、
//!   写审计。

use std::convert::Infallible;
use std::future::Future;
use std::io;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http_body_util::{BodyExt as _, Full};
use hyper::body::{Body, Bytes, Frame, Incoming};
use hyper::header::{self, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use sha2::{Digest as _, Sha256};
use termlet_mcp::Reach;
use termlet_mcp::protocol::ToolCall;
use termlet_mcp::tools::{self, Outcome};
use termlet_proto::{ErrorCode, Lang};
use tokio::io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf};
use tokio::sync::{oneshot, watch};
use tokio::time::Sleep;

use super::Inner;
use super::serve::{Conn, HttpBody};
use super::state::Rejected;
use crate::cli::error::{CliError, tr};
use crate::cli::handler::link::{self as transfer, LinkPlan};
use crate::cli::service::{Cancel, InFlight, Shared};

/// 传输链接的路径前缀（后面是随机串）。
pub(super) const TRANSFER_PATH: &str = "/transfer/";
/// 链接从给出到开始传输的有效期。
pub const LINK_TTL: Duration = Duration::from_secs(10 * 60);
/// 同时没用过的链接最多这么多。
pub const MAX_LINKS: usize = 32;
/// 一次读写等对方最多这么久（对方停住时中止）。
pub const TRANSFER_IDLE: Duration = Duration::from_secs(60);
/// 随机串的字节数。
const TOKEN_BYTES: usize = 32;
/// 下载时服务器到回答正文之间的缓冲。
const PIPE: usize = 256 << 10;
/// 下载的回答正文每次最多发这么多。
const CHUNK: usize = 64 << 10;

/// 没用过的链接。
#[derive(Default)]
pub(super) struct Links(Mutex<Vec<Link>>);

/// 一个没用过的链接：只有随机串的摘要（没有 `Debug`，链接的东西一律不打印）。
struct Link {
    digest: [u8; 32],
    created: Instant,
    /// 取链接的客户端的地址：只有它能用
    peer: IpAddr,
    /// 取链接的客户端（审计的调用方）
    callers: Vec<String>,
    plan: LinkPlan,
}

/// 按随机串找链接的结果。
enum Lookup {
    Unknown,
    Expired,
    Found { upload: bool, peer: IpAddr },
}

impl Links {
    fn lock(&self) -> MutexGuard<'_, Vec<Link>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 还能不能再给一个（先清掉过期的）。
    fn has_room(&self, ttl: Duration) -> bool {
        let mut list = self.lock();
        purge(&mut list, ttl);
        list.len() < MAX_LINKS
    }

    /// 加一个；满了时返回假。
    fn add(&self, link: Link, ttl: Duration) -> bool {
        let mut list = self.lock();
        purge(&mut list, ttl);
        if list.len() >= MAX_LINKS {
            return false;
        }
        list.push(link);
        true
    }

    /// 找链接，不取走（过期的去掉）。
    fn find(&self, digest: &[u8; 32], ttl: Duration) -> Lookup {
        let mut list = self.lock();
        let lookup = match position(&list, digest) {
            None => Lookup::Unknown,
            Some(i) if expired(&list[i], ttl) => {
                list.remove(i);
                Lookup::Expired
            }
            Some(i) => Lookup::Found {
                upload: list[i].plan.upload,
                peer: list[i].peer,
            },
        };
        purge(&mut list, ttl);
        lookup
    }

    /// 取走链接（开始传输，之后不能再用）。
    fn take(&self, digest: &[u8; 32], ttl: Duration) -> Option<Link> {
        let mut list = self.lock();
        let at = position(&list, digest)?;
        let link = list.remove(at);
        (!expired(&link, ttl)).then_some(link)
    }

    /// 全部作废，返回作废了几个。
    pub(super) fn clear(&self) -> usize {
        let mut list = self.lock();
        let n = list.len();
        list.clear();
        n
    }
}

fn expired(link: &Link, ttl: Duration) -> bool {
    link.created.elapsed() >= ttl
}

fn purge(list: &mut Vec<Link>, ttl: Duration) {
    list.retain(|link| !expired(link, ttl));
}

/// 摘要相同的那一个：与每个链接都做常量时间比较，不提前结束（耗时与随机串、位置无关）。
fn position(list: &[Link], digest: &[u8; 32]) -> Option<usize> {
    let mut found = None;
    for (i, link) in list.iter().enumerate() {
        if super::token::constant_time_eq(&link.digest, digest) && found.is_none() {
            found = Some(i);
        }
    }
    found
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// 日志中区分链接用的名字：摘要的前 4 字节（从它推不出随机串）。
fn short_id(digest: &[u8; 32]) -> String {
    data_encoding::HEXLOWER.encode(&digest[..4])
}

/// 局域网中的客户端调用传文件的工具：检查参数、主机与服务器上的路径，给出一次性传输链接（写成工具的结果）。
/// `authority` 是客户端连来时用的 Host（已核对过是这台电脑），链接用同一个地址。
#[allow(clippy::too_many_arguments)]
pub(super) async fn offer(
    inner: &Inner,
    conn: &Conn,
    authority: &str,
    call: &ToolCall,
    callers: Vec<String>,
    lang: Lang,
    cancel: Cancel,
) -> Outcome {
    let request = match tools::link_request(call.tool, &call.arguments, lang) {
        Ok(request) => request,
        Err(outcome) => return outcome,
    };
    let ttl = inner.options.link_ttl;
    let minutes = ttl.as_secs().div_ceil(60);
    // 先看有没有空位，免得连上服务器检查完才发现给不出
    if !inner.links.has_room(ttl) {
        return tools::too_many_links(MAX_LINKS, minutes, lang);
    }
    let shared = &inner.shared;
    let peer = conn.peer_ip();
    let prepared = {
        let _busy = InFlight::enter(shared);
        transfer::prepare(
            shared,
            lang,
            &request.host,
            &request.remote,
            request.upload,
            peer,
            &callers,
            cancel,
        )
        .await
    };
    let plan = match prepared {
        Ok(plan) => plan,
        Err(error) => {
            let response = termlet_proto::Response::Error(error.body());
            return tools::outcome(call.tool, false, Ok(response), lang, Reach::Lan);
        }
    };
    let token = crate::id::random_token(TOKEN_BYTES);
    let digest = digest(&token);
    let url = format!("http://{authority}{TRANSFER_PATH}{token}");
    let outcome = tools::link_outcome(
        &tools::Link {
            url: &url,
            upload: plan.upload,
            host: &plan.host_name,
            remote: &plan.remote,
            size: plan.size,
            minutes,
        },
        lang,
    );
    let (host, upload) = (plan.host_id.clone(), plan.upload);
    let link = Link {
        digest,
        created: Instant::now(),
        peer,
        callers,
        plan,
    };
    if !inner.links.add(link, ttl) {
        return tools::too_many_links(MAX_LINKS, minutes, lang);
    }
    tracing::info!(link = %short_id(&digest), %host, upload, "给出了 MCP 的传输链接");
    outcome
}

/// `/transfer/<随机串>` 上的请求（Host、Origin 已检查；不要局域网令牌，链接本身就是凭据）。
pub(super) async fn serve(
    inner: &Arc<Inner>,
    conn: &Conn,
    token: &str,
    request: Request<Incoming>,
) -> Response<HttpBody> {
    let lang = inner.shared.lang();
    let ttl = inner.options.link_ttl;
    let digest = digest(token);
    let (upload, peer) = match inner.links.find(&digest, ttl) {
        Lookup::Found { upload, peer } => (upload, peer),
        Lookup::Unknown => {
            inner.state.reject(Rejected::Link, conn.from());
            return text(StatusCode::NOT_FOUND, unknown_text(lang));
        }
        Lookup::Expired => {
            inner.state.reject(Rejected::Link, conn.from());
            let minutes = ttl.as_secs().div_ceil(60);
            return text(
                StatusCode::GONE,
                match lang {
                    Lang::Zh => format!(
                        "传输链接已过期（给出后 {minutes} 分钟内有效）：再调用一次 upload_file 或 download_file 取新的链接。"
                    ),
                    Lang::En => format!(
                        "This transfer link has expired (links work for {minutes} minutes): call upload_file or download_file again for a new one."
                    ),
                },
            );
        }
    };
    if peer != conn.peer_ip() {
        inner.state.reject(Rejected::Link, conn.from());
        return text(
            StatusCode::FORBIDDEN,
            tr(
                lang,
                "这个传输链接是给另一台电脑的，只能从取链接的那台电脑使用。",
                "This transfer link was given to another computer and only works from there.",
            ),
        );
    }
    let (wanted, allow) = if upload {
        (Method::PUT, "PUT")
    } else {
        (Method::GET, "GET")
    };
    if request.method() != wanted {
        let mut response = text(
            StatusCode::METHOD_NOT_ALLOWED,
            if upload {
                tr(
                    lang,
                    "这是上传链接：用 curl --fail-with-body -T <本机文件> <链接> 上传（PUT）。",
                    "This is an upload link: upload with curl --fail-with-body -T <local file> <link> (PUT).",
                )
            } else {
                tr(
                    lang,
                    "这是下载链接：用 curl --fail -o <本机文件> <链接> 下载（GET）。",
                    "This is a download link: download with curl --fail -o <local file> <link> (GET).",
                )
            },
        );
        response
            .headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static(allow));
        return response;
    }
    let size = request.body().size_hint().exact();
    if upload && size.is_none() {
        return text(
            StatusCode::LENGTH_REQUIRED,
            tr(
                lang,
                "上传要带 Content-Length：用 curl -T <本机文件> <链接> 发送文件（不支持从标准输入或分块发送）。",
                "Uploads need a Content-Length: send a file with curl -T <local file> <link> (not from standard input or chunked).",
            ),
        );
    }
    let Some(link) = inner.links.take(&digest, ttl) else {
        return text(StatusCode::NOT_FOUND, unknown_text(lang));
    };
    // 开始传输算作活动
    inner.shared.touch();
    crate::usage::mark_active();
    tracing::info!(link = %short_id(&digest), upload, "MCP 的传输链接开始传输");
    let (shared, idle) = (inner.shared.clone(), inner.options.transfer_idle);
    if upload {
        let size = size.unwrap_or_default();
        receive(shared, link, request.into_body(), size, idle, lang).await
    } else {
        send(shared, link, idle, lang).await
    }
}

fn unknown_text(lang: Lang) -> String {
    tr(
        lang,
        "传输链接不存在或已经用过（每个链接只能用一次）：再调用一次 upload_file 或 download_file 取新的链接。",
        "This transfer link doesn't exist or was already used (each link works once): call upload_file or download_file again for a new one.",
    )
}

/// 请求的处理被丢弃（连接断开）时取消传输：放在请求的处理中，下载时放进回答的正文（发完了就不再取消）。
struct Gone {
    sender: Arc<watch::Sender<bool>>,
    armed: bool,
}

impl Drop for Gone {
    fn drop(&mut self) {
        if self.armed {
            self.sender.send_replace(true);
        }
    }
}

/// 传输的取消：返回放在请求一侧的 [`Gone`]、传输的任务要一直拿着的发送端（发送端没有了也算取消），与 [`Cancel`]。
fn watch_gone(shared: &Shared) -> (Gone, Arc<watch::Sender<bool>>, Cancel) {
    let (sender, receiver) = watch::channel(false);
    let sender = Arc::new(sender);
    let gone = Gone {
        sender: sender.clone(),
        armed: true,
    };
    let cancel = Cancel::new(receiver, shared.shutdown.subscribe());
    (gone, sender, cancel)
}

/// 上传：请求的正文经 SFTP 写到服务器上，回答传完的说明（curl 打印出来）。
async fn receive(
    shared: Arc<Shared>,
    link: Link,
    body: Incoming,
    size: u64,
    idle: Duration,
    lang: Lang,
) -> Response<HttpBody> {
    let (mut gone, keep, cancel) = watch_gone(&shared);
    let (done_tx, done_rx) = oneshot::channel();
    crate::runtime::spawn(async move {
        let _keep = keep;
        let _busy = InFlight::enter(&shared);
        let mut reader = BodyReader {
            body,
            chunk: Bytes::new(),
            idle: Idle::new(idle),
        };
        let progress = AtomicU64::new(0);
        let result = transfer::upload(
            &shared,
            lang,
            &link.plan,
            link.peer,
            &link.callers,
            &mut reader,
            size,
            &progress,
            cancel,
        )
        .await;
        let _ = done_tx.send(result);
    });
    let result = done_rx.await;
    gone.armed = false;
    match result {
        Ok(Ok(done)) => text(StatusCode::OK, tools::transfer_text(&done, true, lang)),
        Ok(Err(error)) => failure(&error, lang),
        Err(_) => failure(&CliError::code(ErrorCode::Internal), lang),
    }
}

/// 下载：打开服务器上的文件后回答（带 Content-Length），正文经缓冲边读边发。
async fn send(shared: Arc<Shared>, link: Link, idle: Duration, lang: Lang) -> Response<HttpBody> {
    let (gone, keep, cancel) = watch_gone(&shared);
    let (ready_tx, ready_rx) = oneshot::channel();
    let (writer, reader) = tokio::io::duplex(PIPE);
    let disposition = content_disposition(&link.plan.remote);
    crate::runtime::spawn(async move {
        let _keep = keep;
        let _busy = InFlight::enter(&shared);
        let mut sink = IdleWriter {
            inner: writer,
            idle: Idle::new(idle),
        };
        let progress = AtomicU64::new(0);
        transfer::download(
            &shared,
            lang,
            &link.plan,
            link.peer,
            &link.callers,
            ready_tx,
            &mut sink,
            &progress,
            cancel,
        )
        .await;
    });
    let size = match ready_rx.await {
        Ok(Ok(size)) => size,
        Ok(Err(error)) => return failure(&error, lang),
        Err(_) => return failure(&CliError::code(ErrorCode::Internal), lang),
    };
    let body = PipeBody {
        reader,
        buf: vec![0; CHUNK].into_boxed_slice(),
        left: size,
        gone,
    };
    let mut response = Response::new(body.boxed());
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    headers.insert(header::CONTENT_DISPOSITION, disposition);
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// `attachment; filename="<ASCII 的名字>"; filename*=UTF-8''<百分号编码>`（RFC 6266）：`curl -OJ` 与浏览器据此起名。
fn content_disposition(remote: &str) -> HeaderValue {
    let name = crate::sftp::path::base_name(remote);
    let ascii: String = name
        .chars()
        .map(|c| {
            if c == ' ' || (c.is_ascii_graphic() && c != '"' && c != '\\') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut encoded = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
            encoded.push(char::from(b));
        } else {
            encoded.push_str(&format!("%{b:02X}"));
        }
    }
    HeaderValue::from_str(&format!(
        "attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}"
    ))
    .unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

/// 纯文本的回答（curl 的 `--fail-with-body` 打印出来）。
fn text(status: StatusCode, body: String) -> Response<HttpBody> {
    let mut response = Response::new(Full::new(Bytes::from(format!("{body}\n"))).boxed());
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// 传输失败的回答：`[错误码] 说明`（与工具结果中的相同）。
fn failure(error: &CliError, lang: Lang) -> Response<HttpBody> {
    text(
        status_of(error.code),
        tools::failure_text(error.code.as_str(), &error.message, lang),
    )
}

/// 错误码的 HTTP 状态。服务器那边的问题（连不上、认证、SFTP）是 502：Termlet 在这里是网关。
fn status_of(code: ErrorCode) -> StatusCode {
    match code {
        ErrorCode::HostNotFound | ErrorCode::PathNotFound | ErrorCode::NotFound => {
            StatusCode::NOT_FOUND
        }
        ErrorCode::HostNotAllowed | ErrorCode::PermissionDenied | ErrorCode::CliDisabled => {
            StatusCode::FORBIDDEN
        }
        ErrorCode::AppLocked
        | ErrorCode::VaultLocked
        | ErrorCode::VaultNeedsSetup
        | ErrorCode::VaultKeyLost
        | ErrorCode::Canceled => StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::BadRequest => StatusCode::BAD_REQUEST,
        ErrorCode::PathExists => StatusCode::CONFLICT,
        ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_GATEWAY,
    }
}

/// 等对方的时限：一次读写一直等不到（对方停住）时出错；每次有进展后重新计时，不读写的时候不算。
struct Idle {
    after: Duration,
    sleep: Option<Pin<Box<Sleep>>>,
}

impl Idle {
    fn new(after: Duration) -> Self {
        Self { after, sleep: None }
    }

    /// 有进展：下次等的时候重新计时。
    fn progressed(&mut self) {
        self.sleep = None;
    }

    /// 这次要等：从第一次等起计时，到时出错。
    fn wait<T>(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<T>> {
        let after = self.after;
        let sleep = self
            .sleep
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(after)));
        match sleep.as_mut().poll(cx) {
            Poll::Ready(()) => {
                Poll::Ready(Err(io::Error::new(io::ErrorKind::TimedOut, "对方停住了")))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// HTTP 请求的正文当作 `AsyncRead`（上传）：对方断开时读出错，一直等不到数据时（[`Idle`]）也出错。
struct BodyReader {
    body: Incoming,
    chunk: Bytes,
    idle: Idle,
}

impl AsyncRead for BodyReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if !this.chunk.is_empty() {
                let n = this.chunk.len().min(buf.remaining());
                buf.put_slice(&this.chunk[..n]);
                this.chunk = this.chunk.slice(n..);
                this.idle.progressed();
                return Poll::Ready(Ok(()));
            }
            match Pin::new(&mut this.body).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data() {
                        this.chunk = data;
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Err(io::Error::new(io::ErrorKind::ConnectionAborted, e)));
                }
                Poll::Ready(None) => {
                    this.idle.progressed();
                    return Poll::Ready(Ok(()));
                }
                Poll::Pending => return this.idle.wait(cx),
            }
        }
    }
}

/// 下载时写进回答正文的一端：对方断开（正文被丢弃）时写出错，对方一直不收时（[`Idle`]）也出错。
struct IdleWriter {
    inner: DuplexStream,
    idle: Idle,
}

impl AsyncWrite for IdleWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_write(cx, buf) {
            Poll::Ready(result) => {
                this.idle.progressed();
                Poll::Ready(result)
            }
            Poll::Pending => this.idle.wait(cx),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_flush(cx) {
            Poll::Ready(result) => {
                this.idle.progressed();
                Poll::Ready(result)
            }
            Poll::Pending => this.idle.wait(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// 下载的回答正文：从缓冲中读出发给对方。发的一端出错、提前结束时正文跟着结束，对方按 Content-Length 发现没收齐；
/// 还没发完就被 hyper 丢弃（连接断开）时经 [`Gone`] 取消传输（发完后 hyper 随即丢弃它，不算）。
struct PipeBody {
    reader: DuplexStream,
    buf: Box<[u8]>,
    /// 还要发的字节数（Content-Length）
    left: u64,
    gone: Gone,
}

impl Body for PipeBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        let this = self.get_mut();
        let mut buf = ReadBuf::new(&mut this.buf);
        match Pin::new(&mut this.reader).poll_read(cx, &mut buf) {
            Poll::Ready(Ok(())) if buf.filled().is_empty() => Poll::Ready(None),
            Poll::Ready(Ok(())) => {
                let data = Bytes::copy_from_slice(buf.filled());
                this.left = this.left.saturating_sub(data.len() as u64);
                if this.left == 0 {
                    this.gone.armed = false;
                }
                Poll::Ready(Some(Ok(Frame::data(data))))
            }
            Poll::Ready(Err(_)) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(token: &str, peer: [u8; 4], upload: bool) -> Link {
        Link {
            digest: digest(token),
            created: Instant::now(),
            peer: IpAddr::from(peer),
            callers: vec!["mcp:test".into()],
            plan: LinkPlan {
                host_id: crate::id::HostId::from_raw("h1"),
                host_name: "web".into(),
                remote: "/srv/a".into(),
                upload,
                size: None,
            },
        }
    }

    // 按摘要找，找到的不取走；取走后再找不到；过期的找到时去掉、取不走
    #[test]
    fn links_are_found_by_digest_and_taken_once() {
        let links = Links::default();
        let ttl = Duration::from_secs(600);
        assert!(links.add(link("aaa", [192, 168, 1, 23], true), ttl));
        assert!(links.add(link("bbb", [192, 168, 1, 24], false), ttl));
        assert!(matches!(
            links.find(&digest("bbb"), ttl),
            Lookup::Found { upload: false, peer } if peer == IpAddr::from([192, 168, 1, 24])
        ));
        assert!(matches!(links.find(&digest("ccc"), ttl), Lookup::Unknown));
        let taken = links.take(&digest("aaa"), ttl).unwrap();
        assert_eq!(taken.plan.remote, "/srv/a");
        assert!(links.take(&digest("aaa"), ttl).is_none(), "只能用一次");
        assert!(matches!(links.find(&digest("aaa"), ttl), Lookup::Unknown));

        // 有效期为 0：都已过期
        let now = Duration::ZERO;
        assert!(matches!(links.find(&digest("bbb"), now), Lookup::Expired));
        assert!(matches!(links.find(&digest("bbb"), ttl), Lookup::Unknown));
        assert!(links.add(link("ddd", [10, 0, 0, 1], true), ttl));
        assert!(links.take(&digest("ddd"), now).is_none(), "过期的取不走");
        assert_eq!(links.clear(), 0);
    }

    // 同时没用过的链接有上限；过期的不占位；全部作废
    #[test]
    fn unused_links_are_capped() {
        let links = Links::default();
        let ttl = Duration::from_secs(600);
        for i in 0..MAX_LINKS {
            assert!(links.add(link(&i.to_string(), [10, 0, 0, 1], true), ttl));
        }
        assert!(!links.has_room(ttl));
        assert!(!links.add(link("one more", [10, 0, 0, 1], true), ttl));
        assert!(links.has_room(Duration::ZERO), "过期的不占位");
        assert!(links.add(link("x", [10, 0, 0, 1], true), ttl));
        assert_eq!(links.clear(), 1);
    }

    #[test]
    fn content_disposition_names_the_file() {
        assert_eq!(
            content_disposition("/var/log/app.log"),
            "attachment; filename=\"app.log\"; filename*=UTF-8''app.log"
        );
        assert_eq!(
            content_disposition("/srv/备份 1.tar.gz"),
            "attachment; filename=\"__ 1.tar.gz\"; filename*=UTF-8''%E5%A4%87%E4%BB%BD%201.tar.gz"
        );
        assert_eq!(
            content_disposition("a\"b\\c"),
            "attachment; filename=\"a_b_c\"; filename*=UTF-8''a%22b%5Cc"
        );
    }

    #[test]
    fn errors_map_to_http_statuses() {
        assert_eq!(status_of(ErrorCode::PathNotFound), StatusCode::NOT_FOUND);
        assert_eq!(status_of(ErrorCode::HostNotAllowed), StatusCode::FORBIDDEN);
        assert_eq!(
            status_of(ErrorCode::AppLocked),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(status_of(ErrorCode::ConnectFailed), StatusCode::BAD_GATEWAY);
        assert_eq!(status_of(ErrorCode::AuthFailed), StatusCode::BAD_GATEWAY);
    }
}
