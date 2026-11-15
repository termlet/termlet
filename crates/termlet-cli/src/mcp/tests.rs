//! MCP 服务的单元测试：假的 Termlet（记下收到的请求、按设定回答），从字节流读消息，检查写出的每一行。

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use termlet_proto::{
    ErrorBody, ErrorCode, ExecResponse, ExitCode, HostSummary, MAX_STDIN_COMMAND, Request, Response,
};

use super::*;

type Reply = dyn Fn(&Request, &AbortSlot) -> Result<Response, Failure> + Send + Sync;

/// 假的 Termlet。
struct Fake {
    requests: Mutex<Vec<Request>>,
    reply: Box<Reply>,
}

impl Fake {
    fn new(
        reply: impl Fn(&Request, &AbortSlot) -> Result<Response, Failure> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::default(),
            reply: Box::new(reply),
        })
    }

    /// 什么请求都回答一台主机、一次执行结果。
    fn ok() -> Arc<Self> {
        Self::new(|request, _| {
            Ok(match request {
                Request::ListHosts { .. } => Response::Hosts(vec![HostSummary {
                    id: "h_1".into(),
                    name: "web".into(),
                    host: "10.0.0.5".into(),
                    port: 22,
                    username: "deploy".into(),
                    protocol: "SSH".into(),
                }]),
                Request::Exec(_) => Response::Exec(ExecResponse {
                    exit_code: 0,
                    stdout: "up 3 days\n".into(),
                    stderr: String::new(),
                    truncated: false,
                    duration_ms: 20,
                }),
                _ => Response::Ok,
            })
        })
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

impl Backend for Fake {
    fn call(&self, request: Request, slot: &AbortSlot) -> Result<Response, Failure> {
        self.requests.lock().unwrap().push(request.clone());
        (self.reply)(&request, slot)
    }
}

#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// 把这些行交给服务（结束输入），等工具调用完成，返回写出的消息。
fn run_lines(backend: Arc<Fake>, input: &str) -> Vec<Value> {
    let buffer = Buffer::default();
    let server = Arc::new(Server::new(
        backend,
        Box::new(buffer.clone()),
        Lang::En,
        vec!["claude.exe".into()],
    ));
    server.serve(input.as_bytes());
    server.join();
    let bytes = buffer.0.lock().unwrap().clone();
    String::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn run(backend: Arc<Fake>, input: &[Value]) -> Vec<Value> {
    let text: String = input.iter().map(|m| format!("{m}\n")).collect();
    run_lines(backend, &text)
}

/// 这个 ID 的回答（只能有一条）。
fn reply(messages: &[Value], id: impl Into<Value>) -> Value {
    let id = id.into();
    let found: Vec<&Value> = messages.iter().filter(|m| m["id"] == id).collect();
    assert_eq!(found.len(), 1, "ID {id} 的回答：{messages:?}");
    found[0].clone()
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
            "clientInfo": { "name": "test-client", "version": "1.0" },
        }),
    )
}

/// 新版本的请求：`_meta` 中带版本、能力与客户端。
fn modern(id: i64, method: &str, mut params: Value) -> Value {
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": { "name": "modern-client", "version": "2.0" },
    });
    request(id, method, params)
}

fn call(id: i64, name: &str, arguments: Value) -> Value {
    request(
        id,
        "tools/call",
        json!({ "name": name, "arguments": arguments }),
    )
}

fn text_of(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap()
}

#[test]
fn legacy_handshake_negotiates_the_version() {
    let out = run(
        Fake::ok(),
        &[
            initialize(1, "2025-06-18"),
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            request(2, "tools/list", json!({})),
            request(3, "ping", json!({})),
        ],
    );
    assert_eq!(out.len(), 3, "通知不回答：{out:?}");
    let init = &reply(&out, 1)["result"];
    assert_eq!(init["protocolVersion"], "2025-06-18");
    assert!(init["capabilities"]["tools"].is_object());
    assert_eq!(init["serverInfo"]["name"], "termlet");
    assert_eq!(init["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(
        init["instructions"]
            .as_str()
            .unwrap()
            .contains("list_hosts")
    );
    assert!(init.get("resultType").is_none(), "旧版本不带 resultType");
    let tools = &reply(&out, 2)["result"]["tools"];
    assert_eq!(tools.as_array().unwrap().len(), 4);
    assert!(tools[0]["annotations"].is_object());
    assert_eq!(reply(&out, 3)["result"], json!({}));

    // 不支持的版本：回答支持的最新版本；最早的版本没有 annotations
    let out = run(Fake::ok(), &[initialize(1, "1999-01-01")]);
    assert_eq!(reply(&out, 1)["result"]["protocolVersion"], "2025-11-25");
    let out = run(
        Fake::ok(),
        &[
            initialize(1, "2024-11-05"),
            request(2, "tools/list", json!({})),
        ],
    );
    assert!(
        reply(&out, 2)["result"]["tools"][0]
            .get("annotations")
            .is_none()
    );
}

#[test]
fn requests_before_initialize() {
    let out = run(
        Fake::ok(),
        &[
            request(1, "ping", json!({})),
            request(2, "tools/list", json!({})),
            call(3, "list_hosts", json!({})),
        ],
    );
    assert_eq!(reply(&out, 1)["result"], json!({}), "握手前可以 ping");
    for id in [2, 3] {
        let error = &reply(&out, id)["error"];
        assert_eq!(error["code"], INVALID_PARAMS);
        assert!(error["message"].as_str().unwrap().contains("initialize"));
    }
}

#[test]
fn modern_requests_carry_the_version() {
    let fake = Fake::ok();
    let out = run(
        fake.clone(),
        &[
            modern(1, "server/discover", json!({})),
            modern(2, "tools/list", json!({})),
            modern(
                3,
                "tools/call",
                json!({ "name": "list_hosts", "arguments": {} }),
            ),
        ],
    );
    let discover = &reply(&out, 1)["result"];
    assert_eq!(discover["resultType"], "complete");
    assert_eq!(discover["supportedVersions"], json!(["2026-07-28"]));
    assert!(discover["capabilities"]["tools"].is_object());
    assert_eq!(discover["cacheScope"], "public");
    assert!(discover["ttlMs"].as_u64().unwrap() > 0);
    assert_eq!(
        discover["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "termlet"
    );
    let list = &reply(&out, 2)["result"];
    assert_eq!(list["resultType"], "complete");
    assert!(list["ttlMs"].is_u64() && list["cacheScope"].is_string());
    assert_eq!(list["tools"][1]["name"], "run_command");
    let called = &reply(&out, 3)["result"];
    assert_eq!(called["resultType"], "complete");
    assert_eq!(called["isError"], false);
    assert_eq!(called["structuredContent"]["hosts"][0]["name"], "web");
    assert!(text_of(called).contains("\"id\":\"h_1\""));
    assert_eq!(fake.requests().len(), 1);
}

#[test]
fn modern_version_and_metadata_errors() {
    let mut unsupported = modern(1, "tools/list", json!({}));
    unsupported["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"] = json!("2099-01-01");
    let mut no_capabilities = modern(2, "tools/list", json!({}));
    no_capabilities["params"]["_meta"]
        .as_object_mut()
        .unwrap()
        .remove("io.modelcontextprotocol/clientCapabilities");
    let out = run(
        Fake::ok(),
        &[
            unsupported,
            no_capabilities,
            initialize(3, "2025-11-25"),
            // 握手之后不带 _meta：按旧版本处理，没有 server/discover
            request(4, "server/discover", json!({})),
        ],
    );
    let error = &reply(&out, 1)["error"];
    assert_eq!(error["code"], UNSUPPORTED_PROTOCOL_VERSION);
    assert_eq!(error["data"]["supported"], json!(["2026-07-28"]));
    assert_eq!(error["data"]["requested"], "2099-01-01");
    assert_eq!(reply(&out, 2)["error"]["code"], INVALID_PARAMS);
    assert_eq!(reply(&out, 4)["error"]["code"], METHOD_NOT_FOUND);
}

#[test]
fn json_rpc_errors() {
    let out = run_lines(
        Fake::ok(),
        &[
            "{not json".to_string(),
            "[]".into(),
            json!({ "jsonrpc": "2.0", "id": 9, "result": {} }).to_string(),
            initialize(1, "2025-11-25").to_string(),
            request(2, "resources/list", json!({})).to_string(),
            call(3, "teleport", json!({})).to_string(),
            request(
                4,
                "tools/call",
                json!({ "name": "list_hosts", "arguments": [1] }),
            )
            .to_string(),
            request(5, "tools/call", json!({})).to_string(),
            json!({ "jsonrpc": "2.0", "id": null, "method": "ping" }).to_string(),
            json!({ "jsonrpc": "2.0", "id": 6, "method": "ping", "params": [1] }).to_string(),
            String::new(),
        ]
        .join("\n"),
    );
    let without_id: Vec<&Value> = out.iter().filter(|m| m["id"].is_null()).collect();
    assert_eq!(without_id.len(), 3, "解析失败、批量、id 为 null：{out:?}");
    assert_eq!(without_id[0]["error"]["code"], PARSE_ERROR);
    assert_eq!(without_id[1]["error"]["code"], INVALID_REQUEST);
    assert_eq!(without_id[2]["error"]["code"], INVALID_REQUEST);
    assert!(out.iter().all(|m| m["id"] != 9), "客户端发来的回答不回答");
    assert_eq!(reply(&out, 2)["error"]["code"], METHOD_NOT_FOUND);
    let unknown = &reply(&out, 3)["error"];
    assert_eq!(unknown["code"], INVALID_PARAMS);
    assert_eq!(unknown["message"], "Unknown tool: teleport");
    assert_eq!(reply(&out, 4)["error"]["code"], INVALID_PARAMS);
    assert_eq!(reply(&out, 5)["error"]["code"], INVALID_PARAMS);
    assert_eq!(reply(&out, 6)["error"]["code"], INVALID_PARAMS);
}

// 参数不对是工具执行错误（AI 可以改了重试），不发请求
#[test]
fn arguments_are_checked_before_calling_termlet() {
    let fake = Fake::ok();
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("missing.txt").display().to_string();
    let dir = tmp.path().display().to_string();
    let cases = [
        (
            call(2, "run_command", json!({ "command": "ls" })),
            "missing argument: host",
        ),
        (
            call(3, "run_command", json!({ "host": "web", "command": 5 })),
            "argument command must be a string",
        ),
        (
            call(4, "run_command", json!({ "host": "web", "command": "  " })),
            "argument command must not be empty",
        ),
        (
            call(5, "run_command", json!({ "host": "web", "cmd": "ls" })),
            "unknown argument cmd (this tool takes: host, command, terminal)",
        ),
        (
            call(
                6,
                "run_command",
                json!({ "host": "web", "command": "ls", "terminal": "yes" }),
            ),
            "argument terminal must be true or false",
        ),
        (
            call(
                7,
                "upload_file",
                json!({ "host": "web", "local_path": "a.txt", "remote_path": "/tmp/" }),
            ),
            "local_path must be an absolute path on this computer: a.txt",
        ),
        (
            call(
                8,
                "upload_file",
                json!({ "host": "web", "local_path": missing, "remote_path": "/tmp/" }),
            ),
            "[path_not_found]",
        ),
        (
            call(
                9,
                "upload_file",
                json!({ "host": "web", "local_path": dir, "remote_path": "/tmp/" }),
            ),
            "is a directory",
        ),
        (
            call(
                10,
                "download_file",
                json!({ "host": "web", "remote_path": "/etc/hosts", "local_path": "." }),
            ),
            "local_path must be an absolute path",
        ),
        (
            call(
                11,
                "run_command",
                json!({ "host": "web", "command": "x".repeat(MAX_STDIN_COMMAND + 1) }),
            ),
            "exceeds 1MB",
        ),
    ];
    let mut input = vec![initialize(1, "2025-11-25")];
    input.extend(cases.iter().map(|(message, _)| message.clone()));
    let out = run(fake.clone(), &input);
    for (message, want) in &cases {
        let result = &reply(&out, message["id"].clone())["result"];
        assert_eq!(result["isError"], true, "{message}");
        assert!(
            text_of(result).contains(want),
            "{want}: {}",
            text_of(result)
        );
    }
    assert_eq!(
        reply(&out, 2)["result"]["structuredContent"]["error"],
        "bad_request"
    );
    assert!(fake.requests().is_empty(), "参数不对时不发请求");
}

// 请求与命令行工具的相同，调用方换成 MCP 的；执行结果是文本，不带 structuredContent
#[test]
fn calls_become_cli_requests() {
    let fake = Fake::ok();
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("a.txt");
    std::fs::write(&file, "hi").unwrap();
    let out = run(
        fake.clone(),
        &[
            initialize(1, "2025-06-18"),
            call(2, "list_hosts", json!({ "query": "web" })),
            call(
                3,
                "run_command",
                json!({ "host": "web", "command": "uptime", "terminal": true }),
            ),
            call(
                4,
                "upload_file",
                json!({ "host": "web", "local_path": file.display().to_string(), "remote_path": "/tmp/" }),
            ),
        ],
    );
    let exec = &reply(&out, 3)["result"];
    assert_eq!(exec["isError"], false);
    assert_eq!(
        text_of(exec),
        "exit code 0, took 0.0s\n--- output ---\nup 3 days\n"
    );
    assert!(exec.get("structuredContent").is_none());
    let requests = fake.requests();
    let callers = vec!["mcp:test-client".to_string(), "claude.exe".to_string()];
    assert!(requests.contains(&Request::ListHosts {
        query: "web".into()
    }));
    assert!(
        requests.contains(&Request::Exec(termlet_proto::ExecRequest {
            host: "web".into(),
            command: "uptime".into(),
            terminal: true,
            caller: callers.clone(),
        }))
    );
    assert!(
        requests.contains(&Request::Upload(termlet_proto::TransferRequest {
            host: "web".into(),
            local: file.display().to_string(),
            remote: "/tmp/".into(),
            caller: callers,
        }))
    );

    // 2025-06-18 之前没有 structuredContent
    let out = run(
        Fake::ok(),
        &[
            initialize(1, "2025-03-26"),
            call(2, "list_hosts", json!({})),
        ],
    );
    assert!(reply(&out, 2)["result"].get("structuredContent").is_none());
}

// Termlet 拒绝（外部 CLI 已关闭、未获批准）、连不上：工具执行错误，带错误码与说明
#[test]
fn failures_are_tool_errors() {
    let fake = Fake::new(|request, _| match request {
        Request::ListHosts { .. } => Err(Failure {
            exit: ExitCode::CliDisabled,
            code: "app_not_running".into(),
            message: "open Termlet first".into(),
        }),
        Request::Exec(exec) if exec.host == "nope" => {
            Ok(Response::Error(ErrorBody::new(ErrorCode::HostNotFound, "")))
        }
        _ => Ok(Response::Error(ErrorBody::new(
            ErrorCode::CommandDenied,
            "denied by the user (Shutdown or reboot (power))",
        ))),
    });
    let out = run(
        fake,
        &[
            initialize(1, "2025-11-25"),
            call(2, "list_hosts", json!({})),
            call(
                3,
                "run_command",
                json!({ "host": "web", "command": "reboot" }),
            ),
            call(4, "run_command", json!({ "host": "nope", "command": "ls" })),
        ],
    );
    // 提到命令行用法的说明换成对应的工具
    assert_eq!(
        text_of(&reply(&out, 4)["result"]),
        "[host_not_found] host not found; call list_hosts to see the available hosts"
    );
    let not_running = &reply(&out, 2)["result"];
    assert_eq!(not_running["isError"], true);
    assert_eq!(
        text_of(not_running),
        "[app_not_running] Termlet is not running: open Termlet first"
    );
    assert_eq!(not_running["structuredContent"]["error"], "app_not_running");
    let denied = text_of(&reply(&out, 3)["result"]).to_string();
    assert!(denied.starts_with("[command_denied]"), "{denied}");
    assert!(
        denied.contains("do not try to bypass it") && denied.contains("(power)"),
        "{denied}"
    );
}

// 取消：关闭到 Termlet 的连接，不再回答；同一个 ID 还在进行中时再发报错
#[test]
fn cancellation_stops_the_call_without_a_reply() {
    let fake = Fake::new(|_, slot| {
        // 当作已连上 Termlet：等到被取消
        if slot.arm(None) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !slot.is_aborted() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        assert!(slot.is_aborted(), "应当被取消");
        Ok(Response::Hosts(Vec::new()))
    });
    let out = run(
        fake,
        &[
            initialize(1, "2025-11-25"),
            call(7, "list_hosts", json!({})),
            call(7, "list_hosts", json!({})),
            json!({
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": { "requestId": 7, "reason": "user pressed Esc" },
            }),
            request(8, "ping", json!({})),
        ],
    );
    let seven: Vec<&Value> = out.iter().filter(|m| m["id"] == 7).collect();
    assert_eq!(seven.len(), 1, "{out:?}");
    assert_eq!(
        seven[0]["error"]["code"], INVALID_REQUEST,
        "同一个 ID 还在进行中"
    );
    assert_eq!(reply(&out, 8)["result"], json!({}));
}

#[test]
fn reads_lines_with_a_limit() {
    let mut input: &[u8] = b"one\r\ntoo long line\nlast";
    let mut buf = Vec::new();
    assert_eq!(read_line(&mut input, &mut buf, 8).unwrap(), Line::Complete);
    assert_eq!(buf, b"one\r");
    assert_eq!(read_line(&mut input, &mut buf, 8).unwrap(), Line::TooLong);
    assert_eq!(read_line(&mut input, &mut buf, 8).unwrap(), Line::Complete);
    assert_eq!(buf, b"last", "最后一行没有换行");
    assert_eq!(read_line(&mut input, &mut buf, 8).unwrap(), Line::End);
    // 跨越缓冲区的长行
    let long = format!("{}\nok\n", "x".repeat(100));
    let mut reader = io::BufReader::with_capacity(7, long.as_bytes());
    assert_eq!(read_line(&mut reader, &mut buf, 50).unwrap(), Line::TooLong);
    assert_eq!(
        read_line(&mut reader, &mut buf, 50).unwrap(),
        Line::Complete
    );
    assert_eq!(buf, b"ok");
}

#[test]
fn client_names_are_sanitized() {
    assert_eq!(
        client_name(Some(&json!({ "name": "claude-code" }))),
        "claude-code"
    );
    assert_eq!(
        client_name(Some(&json!({ "name": " a < b; rm -rf / " }))),
        "a  b rm -rf"
    );
    assert_eq!(
        client_name(Some(&json!({ "name": "x".repeat(100) }))).len(),
        48
    );
    assert_eq!(client_name(None), "");
    assert_eq!(client_name(Some(&json!({ "name": 5 }))), "");
}
