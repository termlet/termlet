//! HTTP 接入的测试：真实的命令行服务（内存中的存储、模拟的界面，见 `cli::tests`）在随机的空闲端口上监听，
//! 用 reqwest 当客户端。

use std::net::IpAddr;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};

use super::*;
use crate::audit::{AuditSource, AuditStatus};
use crate::cli::tests::{Approve, Fixture, store_with};
use crate::cli::{APPROVAL_TIMEOUT, TokenView};
use crate::model::HttpScope;
use crate::testkit::{block_on, wait_until};
use crate::vault::MemoryKeyStore;

const MODERN: &str = "2026-07-28";

/// 一个监听着 HTTP 的命令行服务与客户端。
struct Env {
    f: Fixture,
    port: u16,
    token: String,
    client: reqwest::Client,
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn options(max_connections: usize) -> HttpOptions {
    HttpOptions {
        keepalive: Duration::from_millis(100),
        max_connections,
        fake_lan: FakeLan::default(),
    }
}

fn listening(port: u16) -> HttpStatus {
    HttpStatus::Listening {
        port,
        scope: HttpScope::Local,
    }
}

fn fixture(max_connections: usize) -> Fixture {
    Fixture::with_http(
        store_with(Box::new(MemoryKeyStore::default())),
        APPROVAL_TIMEOUT,
        options(max_connections),
    )
}

fn wait_status(f: &Fixture, want: &HttpStatus) {
    wait_until(&format!("HTTP 状态变为 {want:?}"), || {
        f.service.http_status() == *want
    });
}

fn token_of(f: &Fixture) -> String {
    match f.service.http_token(TokenKind::Local).unwrap() {
        TokenView::Ready(token) => token.expose().to_string(),
        other => panic!("应能看到令牌：{other:?}"),
    }
}

impl Env {
    fn new() -> Self {
        Self::with(fixture(MAX_CONNECTIONS))
    }

    /// 打开 HTTP（随机的空闲端口），等它监听。
    fn with(f: Fixture) -> Self {
        let port = free_port();
        f.store
            .update_local_settings(|s| {
                s.cli.http.enabled = true;
                s.cli.http.port = port;
            })
            .unwrap();
        wait_status(&f, &listening(port));
        let token = token_of(&f);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        Self {
            f,
            port,
            token,
            client,
        }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    /// 带令牌、Accept 的 POST，另加 `extra` 中的请求头。
    fn post(&self, body: &Value, extra: &[(&str, &str)]) -> reqwest::RequestBuilder {
        let mut headers = HeaderMap::new();
        headers.insert(
            "accept",
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        headers.insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {}", self.token)).unwrap(),
        );
        for (name, value) in extra {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        self.client
            .post(self.url())
            .headers(headers)
            .body(body.to_string())
    }

    /// 发一个请求，返回（状态，回答头，正文）。
    fn send(&self, request: reqwest::RequestBuilder) -> (u16, HeaderMap, String) {
        block_on(async {
            let response = request.send().await.unwrap();
            let status = response.status().as_u16();
            let headers = response.headers().clone();
            (status, headers, response.text().await.unwrap())
        })
    }

    fn json(&self, body: &Value, extra: &[(&str, &str)]) -> (u16, Value) {
        let (status, _, text) = self.send(self.post(body, extra));
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// 新协议的请求：`_meta` 与对应的请求头都带上（不发出）。
    /// 新协议的请求：`_meta` 与对应的请求头都带上（不发出）。`params` 中已有的 `_meta`（如 `progressToken`）保留。
    fn modern_request(&self, id: i64, method: &str, mut params: Value) -> reqwest::RequestBuilder {
        let given = params.get("_meta").cloned();
        params["_meta"] = json!({
            "io.modelcontextprotocol/protocolVersion": MODERN,
            "io.modelcontextprotocol/clientCapabilities": {},
            "io.modelcontextprotocol/clientInfo": { "name": "modern-http", "version": "1" },
        });
        if let Some(Value::Object(given)) = given {
            for (key, value) in given {
                params["_meta"][key] = value;
            }
        }
        let name = params["name"].as_str().unwrap_or_default().to_string();
        let body = request(id, method, params);
        let mut extra = vec![("mcp-protocol-version", MODERN), ("mcp-method", method)];
        if method == "tools/call" {
            extra.push(("mcp-name", name.as_str()));
        }
        self.post(&body, &extra)
    }

    fn modern(&self, id: i64, method: &str, params: Value) -> (u16, HeaderMap, String) {
        self.send(self.modern_request(id, method, params))
    }
}

fn request(id: i64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn initialize(id: i64, version: &str) -> Value {
    request(
        id,
        "initialize",
        json!({
            "protocolVersion": version,
            "capabilities": {},
            "clientInfo": { "name": "legacy-http", "version": "1" },
        }),
    )
}

/// SSE 正文中的事件（`data:` 行）。
fn events(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect()
}

fn port_closed(port: u16) -> bool {
    std::net::TcpStream::connect(("127.0.0.1", port)).is_err()
}

// 两个开关都开着才监听；关掉任一个即停止；换端口；第一次打开时生成令牌
#[test]
fn listens_only_when_both_switches_are_on() {
    let f = fixture(MAX_CONNECTIONS);
    assert_eq!(f.service.http_status(), HttpStatus::Off, "默认关闭");
    assert_eq!(
        f.service.http_token(TokenKind::Local).unwrap(),
        TokenView::Missing
    );
    let port = free_port();
    f.store
        .update_local_settings(|s| {
            s.cli.enabled = false;
            s.cli.http.enabled = true;
            s.cli.http.port = port;
        })
        .unwrap();
    // 外部 CLI 关着：不监听
    let status = block_on(f.service.refresh_http()).unwrap();
    assert_eq!(status, HttpStatus::Off);
    assert!(port_closed(port));
    assert_eq!(
        f.service.http_token(TokenKind::Local).unwrap(),
        TokenView::Missing
    );

    f.store
        .update_local_settings(|s| s.cli.enabled = true)
        .unwrap();
    wait_status(&f, &listening(port));
    assert!(!port_closed(port));
    let token = token_of(&f);
    assert!(token.starts_with("termlet_mcp_"));

    // 换端口：新端口监听，旧的关闭；令牌不变
    let other = free_port();
    f.store
        .update_local_settings(|s| s.cli.http.port = other)
        .unwrap();
    wait_status(&f, &listening(other));
    assert!(port_closed(port));
    assert_eq!(token_of(&f), token);

    // 关掉 HTTP：停止监听；再打开：同一端口立即可以重新监听
    f.store
        .update_local_settings(|s| s.cli.http.enabled = false)
        .unwrap();
    wait_status(&f, &HttpStatus::Off);
    assert!(port_closed(other));
    f.store
        .update_local_settings(|s| s.cli.http.enabled = true)
        .unwrap();
    wait_status(&f, &listening(other));

    // 服务停止：端口关闭
    let Fixture { service, .. } = f;
    service.stop();
    wait_until("服务停止后端口关闭", || port_closed(other));
}

// 端口被占用：报告占用（不自动换端口）；让出后重试即可监听
#[test]
fn port_in_use_is_reported() {
    let f = fixture(MAX_CONNECTIONS);
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    f.store
        .update_local_settings(|s| {
            s.cli.http.enabled = true;
            s.cli.http.port = port;
        })
        .unwrap();
    wait_status(
        &f,
        &HttpStatus::Failed {
            port,
            scope: HttpScope::Local,
            error: HttpError::PortInUse,
        },
    );
    drop(taken);
    let status = block_on(f.service.refresh_http()).unwrap();
    assert_eq!(status, listening(port));

    f.store
        .update_local_settings(|s| s.cli.http.port = 0)
        .unwrap();
    wait_status(
        &f,
        &HttpStatus::Failed {
            port: 0,
            scope: HttpScope::Local,
            error: HttpError::InvalidPort,
        },
    );
    assert!(port_closed(port), "换端口失败时也停止旧的");
}

// Host、Origin、令牌、路径与方法：读正文之前检查；回答不带 CORS 的头；被拒绝的请求不算活动
#[test]
fn rejects_before_reading_the_body() {
    let env = Env::new();
    let ping = request(1, "ping", json!({}));
    let before = env.f.service.last_request();
    std::thread::sleep(Duration::from_millis(20));

    let host = format!("evil.example:{}", env.port);
    let (status, body) = env.json(&ping, &[("host", &host)]);
    assert_eq!(status, 403, "{body}");
    assert!(body["id"].is_null() && body["error"]["message"].is_string());
    let (status, _) = env.json(&ping, &[("origin", "https://evil.example")]);
    assert_eq!(status, 403);
    let (status, _) = env.json(&ping, &[("origin", "null")]);
    assert_eq!(status, 403);

    // 没有令牌、令牌不对：401，JSON 正文，先不带 WWW-Authenticate
    let anonymous = env
        .client
        .post(env.url())
        .header("accept", "application/json, text/event-stream")
        .body(ping.to_string());
    let (status, headers, text) = env.send(anonymous);
    assert_eq!(status, 401);
    assert!(headers.get("www-authenticate").is_none());
    let body: Value = serde_json::from_str(&text).unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Bearer")
    );
    let (status, _) = env.json(&ping, &[("authorization", "Bearer termlet_mcp_wrong")]);
    assert_eq!(status, 401);
    let (status, _) = env.json(&ping, &[("authorization", "Basic abc")]);
    assert_eq!(status, 401);
    assert_eq!(env.f.service.last_request(), before, "被拒绝的请求不算活动");

    // 只有 /mcp、只接受 POST；OPTIONS 也是 405，不带 CORS 的头
    for method in [
        reqwest::Method::GET,
        reqwest::Method::DELETE,
        reqwest::Method::OPTIONS,
    ] {
        let (status, headers, _) = env.send(
            env.client
                .request(method.clone(), env.url())
                .header("authorization", format!("Bearer {}", env.token))
                .header("access-control-request-method", "POST"),
        );
        assert_eq!(status, 405, "{method}");
        assert_eq!(headers.get("allow").unwrap(), "POST");
        assert!(
            headers
                .keys()
                .all(|k| !k.as_str().starts_with("access-control-")),
            "{headers:?}"
        );
    }
    let (status, _, _) = env.send(
        env.client
            .post(format!("http://127.0.0.1:{}/other", env.port))
            .body(ping.to_string()),
    );
    assert_eq!(status, 404);

    // 本机的三种写法都可以
    for host in ["localhost", "LOCALHOST", "[::1]", "127.0.0.1"] {
        let host = format!("{host}:{}", env.port);
        let (status, body) = env.json(&ping, &[("host", &host)]);
        assert_eq!(status, 200, "{host}: {body}");
        assert_eq!(body["result"], json!({}));
    }
    assert!(
        env.f.service.last_request() > before,
        "通过鉴权的请求算作活动"
    );
}

// 正文的上限：Content-Length 超过 8MB 的不读正文，直接 413
#[test]
fn oversized_bodies_are_rejected() {
    use std::io::{Read as _, Write as _};
    let env = Env::new();
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", env.port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n",
        env.port,
        env.token,
        termlet_mcp::MAX_MESSAGE + 1
    )
    .unwrap();
    let mut head = [0u8; 12];
    stream.read_exact(&mut head).unwrap();
    assert_eq!(&head, b"HTTP/1.1 413");
}

// 重新生成令牌：立即生效
#[test]
fn regenerated_token_takes_effect_immediately() {
    let mut env = Env::new();
    let ping = request(1, "ping", json!({}));
    assert_eq!(env.json(&ping, &[]).0, 200);
    let new = env
        .f
        .service
        .regenerate_http_token(TokenKind::Local)
        .unwrap();
    assert_eq!(env.json(&ping, &[]).0, 401, "旧令牌随即失效");
    env.token = new.expose().to_string();
    assert_eq!(env.json(&ping, &[]).0, 200);
    assert_eq!(token_of(&env.f), env.token);
}

// 旧协议：initialize 下发会话 ID，之后按会话的版本回答；不认识的会话按请求头补一个，不回 404；通知 202
#[test]
fn legacy_protocol_with_sessions() {
    let env = Env::new();
    let (status, headers, text) = env.send(env.post(&initialize(1, "2025-03-26"), &[]));
    assert_eq!(status, 200);
    assert_eq!(headers["content-type"], "application/json");
    let session = headers["mcp-session-id"].to_str().unwrap().to_string();
    assert_eq!(session.split_once('.').unwrap().0.len(), 26, "{session}");
    let init: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");

    let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    let (status, _, text) = env.send(env.post(&note, &[("mcp-session-id", &session)]));
    assert_eq!((status, text.as_str()), (202, ""));

    // 会话记着 2025-03-26：有 annotations、没有 title
    let list = request(2, "tools/list", json!({}));
    let (status, body) = env.json(&list, &[("mcp-session-id", &session)]);
    assert_eq!(status, 200);
    let tools = body["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 4);
    assert!(tools[0]["annotations"].is_object() && tools[0].get("title").is_none());

    // 不认识的会话：版本取请求头
    let (status, body) = env.json(
        &list,
        &[
            ("mcp-session-id", "unknown-session"),
            ("mcp-protocol-version", "2025-11-25"),
        ],
    );
    assert_eq!(status, 200);
    assert!(body["result"]["tools"][0]["title"].is_string());
    // 没有会话、也没有版本头：按 2025-03-26
    let (status, body) = env.json(&list, &[]);
    assert_eq!(status, 200);
    assert!(body["result"]["tools"][0].get("title").is_none());
    // 不支持的版本头：400
    let (status, body) = env.json(&list, &[("mcp-protocol-version", "2024-01-01")]);
    assert_eq!(status, 400);
    assert_eq!(
        body["error"]["code"],
        termlet_mcp::UNSUPPORTED_PROTOCOL_VERSION
    );
    assert_eq!(body["id"], 2);
    // 新版本的版本头却没有 _meta：请求头与正文不一致
    let (status, body) = env.json(&list, &[("mcp-protocol-version", MODERN)]);
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], termlet_mcp::HEADER_MISMATCH);

    // 旧协议中不认识的方法：200（旧客户端把 404 当作会话已结束）
    let (status, body) = env.json(&request(3, "resources/list", json!({})), &[]);
    assert_eq!(status, 200);
    assert_eq!(body["error"]["code"], termlet_mcp::METHOD_NOT_FOUND);
    // 批量、不是 JSON：400
    let (status, _, _) = env.send(env.post(&json!([request(4, "ping", json!({}))]), &[]));
    assert_eq!(status, 400);
    let (status, _, _) = env.send(
        env.client
            .post(env.url())
            .header("authorization", format!("Bearer {}", env.token))
            .body("{oops"),
    );
    assert_eq!(status, 400);
    // 客户端发来的回答：202
    let (status, _, _) =
        env.send(env.post(&json!({ "jsonrpc": "2.0", "id": 9, "result": {} }), &[]));
    assert_eq!(status, 202);
}

// 新协议：请求头与正文必须一致；不认识的方法 404；不支持的版本 400
#[test]
fn modern_protocol_checks_headers() {
    let env = Env::new();
    let (status, headers, text) = env.modern(1, "server/discover", json!({}));
    assert_eq!(status, 200, "{text}");
    assert!(headers.get("mcp-session-id").is_none(), "新协议没有会话");
    let discover: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(discover["result"]["supportedVersions"], json!([MODERN]));

    let (status, _, text) = env.modern(2, "resources/list", json!({}));
    assert_eq!(status, 404);
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["error"]["code"], termlet_mcp::METHOD_NOT_FOUND);

    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": MODERN,
        "io.modelcontextprotocol/clientCapabilities": {},
    });
    let list = request(3, "tools/list", json!({ "_meta": meta }));
    for (extra, why) in [
        (vec![("mcp-method", "tools/list")], "缺少版本头"),
        (vec![("mcp-protocol-version", MODERN)], "缺少方法头"),
        (
            vec![
                ("mcp-protocol-version", "2025-11-25"),
                ("mcp-method", "tools/list"),
            ],
            "版本头不一致",
        ),
        (
            vec![
                ("mcp-protocol-version", MODERN),
                ("mcp-method", "tools/call"),
            ],
            "方法头不一致",
        ),
    ] {
        let (status, body) = env.json(&list, &extra);
        assert_eq!(status, 400, "{why}");
        assert_eq!(body["error"]["code"], termlet_mcp::HEADER_MISMATCH, "{why}");
        assert_eq!(body["id"], 3);
    }
    let call = request(
        4,
        "tools/call",
        json!({ "name": "list_hosts", "arguments": {}, "_meta": meta }),
    );
    let (status, body) = env.json(
        &call,
        &[
            ("mcp-protocol-version", MODERN),
            ("mcp-method", "tools/call"),
            ("mcp-name", "run_command"),
        ],
    );
    assert_eq!(
        (status, &body["error"]["code"]),
        (400, &json!(termlet_mcp::HEADER_MISMATCH))
    );

    let mut unsupported = meta.clone();
    unsupported["io.modelcontextprotocol/protocolVersion"] = json!("2099-01-01");
    let (status, body) = env.json(
        &request(5, "tools/list", json!({ "_meta": unsupported })),
        &[
            ("mcp-protocol-version", "2099-01-01"),
            ("mcp-method", "tools/list"),
        ],
    );
    assert_eq!(status, 400);
    assert_eq!(
        body["error"]["code"],
        termlet_mcp::UNSUPPORTED_PROTOCOL_VERSION
    );
}

// tools/call 用 SSE 回答，最后一条事件是结果；调用方是 mcp:<客户端> + http + 发起连接的进程，审计来源是 MCP
#[test]
fn tool_calls_answer_over_sse_and_are_audited() {
    let env = Env::new();
    env.f.add_dead_host("web", true);
    let (status, headers, text) = env.modern(
        1,
        "tools/call",
        json!({ "name": "run_command", "arguments": { "host": "web", "command": "uptime" } }),
    );
    assert_eq!(status, 200);
    assert_eq!(headers["content-type"], "text/event-stream");
    assert_eq!(headers["x-accel-buffering"], "no");
    let first = events(&text);
    assert_eq!(first.len(), 1, "{text}");
    let result = &first[0]["result"];
    assert_eq!(first[0]["id"], 1);
    assert_eq!(result["isError"], true);
    assert_eq!(result["resultType"], "complete");
    assert!(
        result["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("[connect_failed]"),
        "{result}"
    );

    let entry = env.f.entries()[0].clone();
    assert_eq!(entry.source, AuditSource::Mcp);
    assert_eq!(entry.error_code, "connect_failed");
    let callers: Vec<&str> = entry.caller.split(" < ").collect();
    assert_eq!(
        callers[..2],
        ["mcp:modern-http", "http"],
        "{}",
        entry.caller
    );
    assert!(crate::audit::mcp_over_http(&entry.caller));
    if cfg!(any(windows, target_os = "linux")) {
        // 发起连接的是本测试进程
        let me = std::env::current_exe().unwrap();
        let me = me.file_name().unwrap().to_string_lossy().to_lowercase();
        assert!(
            callers
                .get(2)
                .is_some_and(|c| me.starts_with(&c.to_lowercase())),
            "{}",
            entry.caller
        );
    }

    // 旧协议的调用：客户端名来自握手（经会话）
    let (_, headers, _) = env.send(env.post(&initialize(1, "2025-11-25"), &[]));
    let session = headers["mcp-session-id"].to_str().unwrap().to_string();
    let call = request(
        2,
        "tools/call",
        json!({ "name": "list_hosts", "arguments": {} }),
    );
    let (status, _, text) = env.send(env.post(&call, &[("mcp-session-id", &session)]));
    assert_eq!(status, 200);
    let legacy = events(&text);
    assert_eq!(
        legacy[0]["result"]["structuredContent"]["hosts"][0]["name"],
        "web"
    );
    assert!(legacy[0]["result"].get("resultType").is_none());

    // 参数不对：工具执行错误，不发请求
    let (_, _, text) = env.modern(
        3,
        "tools/call",
        json!({ "name": "run_command", "arguments": { "command": "ls" } }),
    );
    let text = events(&text)[0]["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(text.starts_with("[bad_request]"), "{text}");
    assert_eq!(env.f.entries().len(), 1);
}

// 客户端的 Accept 不收 SSE：做完再回答 JSON
#[test]
fn tool_calls_fall_back_to_json() {
    let env = Env::new();
    let call = request(
        1,
        "tools/call",
        json!({ "name": "list_hosts", "arguments": {} }),
    );
    let (status, headers, text) = env.send(env.post(&call, &[("accept", "application/json")]));
    assert_eq!(status, 200);
    assert_eq!(headers["content-type"], "application/json");
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["result"]["isError"], false);
}

// 请求带 progressToken 时，保活改为进度通知（SSE 中的一条事件），否则是注释（见下一个测试）
#[test]
fn progress_replaces_the_keepalive() {
    let env = Env::new();
    env.f.add_dead_host("web", true);
    env.f.desktop.set_approve(Approve::Ignore);
    let request = env.modern_request(
        1,
        "tools/call",
        json!({
            "name": "run_command",
            "arguments": { "host": "web", "command": "reboot" },
            "_meta": { "progressToken": "p-1" },
        }),
    );
    let first = block_on(async {
        let mut response = request.send().await.unwrap();
        let mut text = String::new();
        // 两次保活：进度递增
        while text.matches("notifications/progress").count() < 2 {
            let chunk = tokio::time::timeout(Duration::from_secs(5), response.chunk())
                .await
                .expect("应有进度")
                .unwrap()
                .unwrap();
            text.push_str(&String::from_utf8_lossy(&chunk));
        }
        text
    });
    assert!(!first.contains(": keepalive"), "{first}");
    let notes = events(&first);
    assert_eq!(notes.len(), 2, "{first}");
    for note in &notes {
        assert_eq!(note["method"], "notifications/progress");
        assert_eq!(note["params"]["progressToken"], "p-1");
        assert!(note.get("id").is_none());
    }
    assert!(
        notes[0]["params"]["message"]
            .as_str()
            .unwrap()
            .starts_with("已运行")
    );
    wait_until("审计记下取消", || {
        env.f
            .entries()
            .first()
            .is_some_and(|e| e.error_code == "canceled")
    });
}

// 会话 ID 自带版本与客户端名：换一个服务（相当于 Termlet 重启后）照样认得出，审计中有客户端名
#[test]
fn session_ids_survive_a_restart() {
    let before = Env::new();
    let (_, headers, _) = before.send(before.post(&initialize(1, "2025-06-18"), &[]));
    let session = headers["mcp-session-id"].to_str().unwrap().to_string();
    drop(before);

    let after = Env::new();
    after.f.add_dead_host("web", true);
    let call = request(
        2,
        "tools/call",
        json!({ "name": "run_command", "arguments": { "host": "web", "command": "uptime" } }),
    );
    let (status, _, text) = after.send(after.post(&call, &[("mcp-session-id", &session)]));
    assert_eq!(status, 200);
    // 2025-06-18 起有 structuredContent：版本也取自会话 ID
    let result = &events(&text)[0]["result"];
    assert_eq!(result["isError"], true);
    assert!(result["structuredContent"].is_object(), "{result}");
    let caller = after.f.entries()[0].caller.clone();
    assert!(caller.starts_with("mcp:legacy-http < http"), "{caller}");
}

// SSE：回答头马上发出，等批准期间定时保活；客户端关闭流即取消（批准框关闭，审计记为已取消）
#[test]
fn closing_the_stream_cancels_the_call() {
    let env = Env::new();
    env.f.add_dead_host("web", true);
    env.f.desktop.set_approve(Approve::Ignore);
    let request = env.modern_request(
        1,
        "tools/call",
        json!({ "name": "run_command", "arguments": { "host": "web", "command": "reboot" } }),
    );
    let (call_status, keepalive) = block_on(async {
        // 等批准时回答头已经到了
        let mut response = tokio::time::timeout(Duration::from_secs(5), request.send())
            .await
            .expect("回答头应马上发出")
            .unwrap();
        let status = response.status().as_u16();
        let first = tokio::time::timeout(Duration::from_secs(5), response.chunk())
            .await
            .expect("应有保活")
            .unwrap()
            .unwrap();
        drop(response);
        (status, String::from_utf8_lossy(&first).into_owned())
    });
    assert_eq!(call_status, 200);
    assert!(keepalive.starts_with(':'), "{keepalive:?}");
    assert_eq!(env.f.desktop.asked(), ["power"]);
    wait_until("审计记下取消", || {
        env.f
            .entries()
            .first()
            .is_some_and(|e| e.error_code == "canceled" && e.status == AuditStatus::Failed)
    });
    wait_until("请求结束", || env.f.service.in_flight() == 0);
}

// 旧协议的 notifications/cancelled：按（会话，请求 ID）取消，流结束、没有结果
#[test]
fn cancel_notification_stops_the_call() {
    let env = Env::new();
    env.f.add_dead_host("web", true);
    env.f.desktop.set_approve(Approve::Ignore);
    let (_, headers, _) = env.send(env.post(&initialize(1, "2025-11-25"), &[]));
    let session = headers["mcp-session-id"].to_str().unwrap().to_string();
    let call = request(
        5,
        "tools/call",
        json!({ "name": "run_command", "arguments": { "host": "web", "command": "reboot" } }),
    );
    let text = block_on(async {
        let mut response = env
            .post(&call, &[("mcp-session-id", &session)])
            .send()
            .await
            .unwrap();
        let mut text = String::new();
        // 等到批准框弹出（保活说明调用在进行）
        while env.f.desktop.asked().is_empty() {
            let chunk = response.chunk().await.unwrap().unwrap();
            text.push_str(&String::from_utf8_lossy(&chunk));
        }
        let cancel = json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": { "requestId": 5, "reason": "user pressed Esc" },
        });
        let status = env
            .post(&cancel, &[("mcp-session-id", &session)])
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, 202);
        while let Some(chunk) = tokio::time::timeout(Duration::from_secs(5), response.chunk())
            .await
            .expect("流应结束")
            .unwrap()
        {
            text.push_str(&String::from_utf8_lossy(&chunk));
        }
        text
    });
    assert!(events(&text).is_empty(), "被取消的请求不再回答：{text}");
    wait_until("审计记下取消", || {
        env.f
            .entries()
            .first()
            .is_some_and(|e| e.error_code == "canceled")
    });
}

// 同时的连接数有上限：超出的回答 503；空出来后照常
#[test]
fn connection_limit() {
    let env = Env::with(fixture(2));
    let idle: Vec<std::net::TcpStream> = (0..2)
        .map(|_| std::net::TcpStream::connect(("127.0.0.1", env.port)).unwrap())
        .collect();
    std::thread::sleep(Duration::from_millis(100));
    let ping = request(1, "ping", json!({}));
    let busy = block_on(async {
        env.post(&ping, &[])
            .send()
            .await
            .map(|r| r.status().as_u16())
    });
    assert!(matches!(busy, Ok(503)), "{busy:?}");
    drop(idle);
    wait_until("连接空出来", || {
        block_on(async {
            env.post(&ping, &[])
                .send()
                .await
                .map(|r| r.status().as_u16())
        })
        .is_ok_and(|s| s == 200)
    });
}

// 工具结果的说明按界面语言（简体、繁体中文用中文，其余用英文）
#[test]
fn messages_follow_the_ui_language() {
    let env = Env::new();
    let call = |env: &Env| {
        let (_, _, text) = env.modern(
            1,
            "tools/call",
            json!({ "name": "run_command", "arguments": { "command": "ls" } }),
        );
        events(&text)[0]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert!(call(&env).contains("缺少参数 host"), "默认中文");
    env.f.service.set_language(termlet_proto::Lang::En);
    assert!(call(&env).contains("missing argument: host"));
}

// 保险库没有解锁时：照常监听、校验令牌（解锁后生成过的）；还没有令牌时不生成、请求一律 401
#[test]
fn locked_vault_keeps_serving() {
    let f = Fixture::with_http(crate::testkit::locked_store(), APPROVAL_TIMEOUT, options(4));
    let port = free_port();
    f.store
        .update_local_settings(|s| {
            s.cli.http.enabled = true;
            s.cli.http.port = port;
        })
        .unwrap();
    wait_status(&f, &listening(port));
    assert_eq!(
        f.service.http_token(TokenKind::Local).unwrap(),
        TokenView::Missing
    );
    assert!(f.service.regenerate_http_token(TokenKind::Local).is_err());
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let status = block_on(async {
        client
            .post(format!("http://127.0.0.1:{port}/mcp"))
            .header("authorization", "Bearer termlet_mcp_x")
            .body(request(1, "ping", json!({})).to_string())
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    });
    assert_eq!(status, 401);
}

// ------------------------------------------------------------ 局域网

/// 测试中当作局域网中的电脑的源地址（回环地址：不监听局域网、不弹防火墙的提示）与它代表的地址。
const LAN_SOURCE: [u8; 4] = [127, 0, 0, 2];
const LAN_PEER: [u8; 4] = [192, 168, 1, 23];
/// 当作公网来的连接
const PUBLIC_SOURCE: [u8; 4] = [127, 0, 0, 3];
/// 局域网的连接连到的本机地址
const LAN_LOCAL: [u8; 4] = [192, 168, 1, 5];

/// 监听范围为局域网（实际仍在 127.0.0.1 上），从 127.0.0.2 来的连接当作 192.168.1.23、127.0.0.3 来的当作 8.8.8.8。
fn lan_fixture() -> Fixture {
    let mut options = options(MAX_CONNECTIONS);
    options.fake_lan = FakeLan {
        bind: Some(IpAddr::from([127, 0, 0, 1])),
        peers: vec![
            (IpAddr::from(LAN_SOURCE), IpAddr::from(LAN_PEER)),
            (IpAddr::from(PUBLIC_SOURCE), IpAddr::from([8, 8, 8, 8])),
        ],
        local: Some(IpAddr::from(LAN_LOCAL)),
    };
    Fixture::with_http(
        store_with(Box::new(MemoryKeyStore::default())),
        APPROVAL_TIMEOUT,
        options,
    )
}

/// 以 `source` 为源地址、不复用连接的客户端（换了监听范围后旧连接会被关闭）。
fn client_from(source: [u8; 4]) -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .local_address(IpAddr::from(source))
        .pool_max_idle_per_host(0)
        .build()
        .unwrap()
}

/// 发一个请求，返回（状态，正文）；连接被关闭时为 `None`。
fn lan_send(
    client: &reqwest::Client,
    port: u16,
    host: &str,
    token: &str,
    body: &Value,
    extra: &[(&str, &str)],
) -> Option<(u16, String)> {
    block_on(async {
        let mut request = client
            .post(format!("http://127.0.0.1:{port}/mcp"))
            .header("host", host)
            .header("accept", "application/json, text/event-stream")
            .header("authorization", format!("Bearer {token}"))
            .body(body.to_string());
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        let response = request.send().await.ok()?;
        let status = response.status().as_u16();
        Some((status, response.text().await.unwrap_or_default()))
    })
}

fn lan_token_of(f: &Fixture) -> String {
    match f.service.http_token(TokenKind::Lan).unwrap() {
        TokenView::Ready(token) => token.expose().to_string(),
        other => panic!("应能看到局域网令牌：{other:?}"),
    }
}

// 监听范围为局域网：生成局域网令牌；按对端地址选令牌、核对 Host；局域网中的客户端只有两个工具，调用传文件的工具得到
// 说明而不交给 Termlet；审计的调用方是 lan:<IP>；不在私有网段的连接直接关闭；改回只有本机后局域网的连接被关闭
#[test]
fn lan_clients_are_separated_by_peer_address() {
    let f = lan_fixture();
    f.add_dead_host("web", true);
    let port = free_port();
    f.store
        .update_local_settings(|s| {
            s.cli.http.enabled = true;
            s.cli.http.port = port;
            s.cli.http.scope = HttpScope::Lan;
        })
        .unwrap();
    wait_status(
        &f,
        &HttpStatus::Listening {
            port,
            scope: HttpScope::Lan,
        },
    );
    let local_token = token_of(&f);
    let lan_token = lan_token_of(&f);
    assert!(
        lan_token.starts_with("termlet_mcp_lan_"),
        "局域网令牌的前缀"
    );
    assert_ne!(local_token, lan_token);

    let ping = request(1, "ping", json!({}));
    let lan = client_from(LAN_SOURCE);
    let lan_host = format!("192.168.1.5:{port}");
    // 局域网来的：只认局域网令牌
    let (status, body) = lan_send(&lan, port, &lan_host, &local_token, &ping, &[]).unwrap();
    assert_eq!(status, 401, "本机的令牌不能用在局域网：{body}");
    assert!(body.contains("LAN token"), "{body}");
    let (status, _) = lan_send(&lan, port, &lan_host, &lan_token, &ping, &[]).unwrap();
    assert_eq!(status, 200);
    // Host：这条连接的本机地址或这台电脑的名字；回环的名字、别的地址不认
    for bad in [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("192.168.1.6:{port}"),
    ] {
        let (status, _) = lan_send(&lan, port, &bad, &lan_token, &ping, &[]).unwrap();
        assert_eq!(status, 403, "{bad}");
    }
    if let Some(name) = lan::host_names().first() {
        let host = format!("{name}:{port}");
        let (status, _) = lan_send(&lan, port, &host, &lan_token, &ping, &[]).unwrap();
        assert_eq!(status, 200, "{host}");
    }
    // Origin 照旧一律拒绝
    let origin = [("origin", "http://192.168.1.23")];
    let (status, _) = lan_send(&lan, port, &lan_host, &lan_token, &ping, &origin).unwrap();
    assert_eq!(status, 403);

    // 本机来的：照旧只认本机的令牌与回环的名字
    let local = client_from([127, 0, 0, 1]);
    let local_host = format!("127.0.0.1:{port}");
    let send_local =
        |token: &str, body: &Value| lan_send(&local, port, &local_host, token, body, &[]).unwrap();
    assert_eq!(send_local(&local_token, &ping).0, 200);
    assert_eq!(
        send_local(&lan_token, &ping).0,
        401,
        "局域网令牌不能用在本机"
    );

    // 局域网中的客户端：说明与工具列表
    let send_lan = |body: &Value| lan_send(&lan, port, &lan_host, &lan_token, body, &[]).unwrap();
    let init: Value = serde_json::from_str(&send_lan(&initialize(1, "2025-11-25")).1).unwrap();
    let instructions = init["result"]["instructions"].as_str().unwrap();
    assert!(instructions.contains("local network"), "{init}");
    let list = request(2, "tools/list", json!({}));
    let tool_names = |text: &str| -> Vec<String> {
        let body: Value = serde_json::from_str(text).unwrap();
        body["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        tool_names(&send_lan(&list).1),
        ["list_hosts", "run_command"]
    );
    assert_eq!(
        tool_names(&send_local(&local_token, &list).1).len(),
        4,
        "本机照旧四个"
    );

    // 直接调用传文件的工具：工具执行错误，不交给 Termlet（没有审计记录）
    let download = request(
        3,
        "tools/call",
        json!({
            "name": "download_file",
            "arguments": { "host": "web", "remote_path": "/etc/hostname", "local_path": "/tmp/x" },
        }),
    );
    let (status, text) = send_lan(&download);
    assert_eq!(status, 200);
    let result = &events(&text)[0]["result"];
    assert_eq!(result["isError"], true);
    let message = result["content"][0]["text"].as_str().unwrap();
    assert!(message.starts_with("[lan_unavailable]"), "{result}");
    assert!(f.entries().is_empty(), "没有交给 Termlet");

    // 执行命令：审计的调用方是 mcp < http < lan:<IP>，没有进程链
    let run = request(
        4,
        "tools/call",
        json!({ "name": "run_command", "arguments": { "host": "web", "command": "uptime" } }),
    );
    assert_eq!(send_lan(&run).0, 200);
    let entry = f.entries()[0].clone();
    assert_eq!(entry.source, AuditSource::Mcp);
    assert_eq!(entry.caller, "mcp < http < lan:192.168.1.23");
    assert_eq!(
        crate::audit::mcp_lan_peer(&entry.caller),
        Some("192.168.1.23")
    );

    // 说明中不提局域网中的客户端没有的工具：命令过长时不叫它先用 upload_file 上传
    let huge = request(
        5,
        "tools/call",
        json!({
            "name": "run_command",
            "arguments": { "host": "web", "command": "x".repeat(termlet_proto::MAX_STDIN_COMMAND + 1) },
        }),
    );
    let (_, text) = send_lan(&huge);
    let message = events(&text)[0]["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(message.starts_with("[bad_request]"), "{message}");
    assert!(
        message.contains("heredoc") && !message.contains("upload_file"),
        "{message}"
    );
    let (_, text) = lan_send(&local, port, &local_host, &local_token, &huge, &[]).unwrap();
    assert!(
        events(&text)[0]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("upload_file"),
        "本机的客户端照旧"
    );

    // 不在私有网段的连接：不读请求，直接关闭
    let public = client_from(PUBLIC_SOURCE);
    assert!(
        lan_send(&public, port, &lan_host, &lan_token, &ping, &[]).is_none(),
        "连接应被关闭"
    );

    // 改回只有本机：重新监听；局域网来的连接被关闭，本机照常
    f.store
        .update_local_settings(|s| s.cli.http.scope = HttpScope::Local)
        .unwrap();
    wait_status(&f, &listening(port));
    assert!(lan_send(&lan, port, &lan_host, &lan_token, &ping, &[]).is_none());
    assert_eq!(send_local(&local_token, &ping).0, 200);
    assert_eq!(lan_token_of(&f), lan_token, "局域网令牌保留");
}

// 局域网令牌只在监听范围为局域网时生成，可以单独重新生成（本机的不变）
#[test]
fn lan_token_is_generated_and_regenerated_separately() {
    let f = fixture(MAX_CONNECTIONS);
    let port = free_port();
    f.store
        .update_local_settings(|s| {
            s.cli.http.enabled = true;
            s.cli.http.port = port;
        })
        .unwrap();
    wait_status(&f, &listening(port));
    assert_eq!(
        f.service.http_token(TokenKind::Lan).unwrap(),
        TokenView::Missing,
        "只有本机时不生成"
    );
    let local = token_of(&f);
    let new = f.service.regenerate_http_token(TokenKind::Lan).unwrap();
    assert!(new.expose().starts_with("termlet_mcp_lan_"));
    assert_eq!(token_of(&f), local, "本机的令牌不变");
}

// 真的在 0.0.0.0 上监听（只在 Linux 上跑：Windows 上会在桌面上弹出防火墙的提示；本机在 testing/linux 的容器中验证）。
// 本机经回环照常；有私有网段的网卡时，经它的地址连接要局域网令牌
#[cfg(target_os = "linux")]
#[test]
fn lan_scope_listens_on_all_ipv4_interfaces() {
    let f = fixture(MAX_CONNECTIONS);
    let port = free_port();
    f.store
        .update_local_settings(|s| {
            s.cli.http.enabled = true;
            s.cli.http.port = port;
            s.cli.http.scope = HttpScope::Lan;
        })
        .unwrap();
    wait_status(
        &f,
        &HttpStatus::Listening {
            port,
            scope: HttpScope::Lan,
        },
    );
    let ping = request(1, "ping", json!({}));
    let local_token = token_of(&f);
    let lan_token = lan_token_of(&f);
    let local = client_from([127, 0, 0, 1]);
    let host = format!("127.0.0.1:{port}");
    let (status, _) = lan_send(&local, port, &host, &local_token, &ping, &[]).unwrap();
    assert_eq!(status, 200);
    let Some(address) = lan::addresses().into_iter().next() else {
        eprintln!("没有私有网段的网卡，跳过经局域网地址的连接");
        return;
    };
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let send = |token: &str| {
        block_on(async {
            client
                .post(format!("http://{}:{port}/mcp", address.ip))
                .header("accept", "application/json, text/event-stream")
                .header("authorization", format!("Bearer {token}"))
                .body(ping.to_string())
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        })
    };
    assert_eq!(send(&lan_token), 200, "经 {} 连接", address.ip);
    assert_eq!(send(&local_token), 401, "本机的令牌不能用在局域网");
}
