//! 分发的单元测试：只看一条消息的处理结果，不经过传输层（stdio 的完整流程在 `termlet-cli` 的 `mcp` 测试中，
//! HTTP 的在 `termlet-core` 的 `cli::http` 测试中）。

use serde_json::{Value, json};
use termlet_proto::{Lang, Response};

use super::*;
use crate::tools::Tool;

fn legacy(version: &'static str) -> Session {
    Session {
        version,
        client: "legacy-client".into(),
    }
}

fn meta(params: Value) -> Value {
    let mut params = params;
    params["_meta"] = json!({
        META_PROTOCOL_VERSION: "2026-07-28",
        META_CLIENT_CAPABILITIES: {},
        META_CLIENT_INFO: { "name": "modern <client>", "version": "2.0" },
    });
    params
}

fn reply(step: Step) -> Value {
    match step {
        Step::Reply(reply) => reply,
        other => panic!("应立即回答：{other:?}"),
    }
}

#[test]
fn classifies_messages() {
    assert_eq!(
        classify(json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" })),
        Ok(Message::Request {
            id: json!(1),
            method: "ping".into(),
            params: None,
        })
    );
    let note = classify(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    assert!(
        matches!(note, Ok(Message::Notification { ref method, .. }) if method == "notifications/initialized")
    );
    assert_eq!(
        classify(json!({ "jsonrpc": "2.0", "id": 9, "result": {} })),
        Ok(Message::Response)
    );
    // 批量、ID 为 null 或小数、没有方法：INVALID_REQUEST
    for bad in [
        json!([]),
        json!({ "jsonrpc": "2.0", "id": null, "method": "ping" }),
        json!({ "jsonrpc": "2.0", "id": 1.5, "method": "ping" }),
        json!({ "jsonrpc": "2.0", "id": 3 }),
    ] {
        let error = classify(bad.clone()).unwrap_err();
        assert_eq!(error_code(&error), Some(INVALID_REQUEST), "{bad}");
    }
    // 没有方法时仍带上合法的 ID
    let error = classify(json!({ "jsonrpc": "2.0", "id": 3 })).unwrap_err();
    assert_eq!(error["id"], 3);
    let error = parse(b"{oops").unwrap_err();
    assert_eq!(error_code(&error), Some(PARSE_ERROR));
    assert!(error["id"].is_null());
}

#[test]
fn initialize_negotiates_and_returns_the_session() {
    let step = request(
        &json!(1),
        "initialize",
        Some(json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "Cursor;", "version": "3" },
        })),
        None,
    );
    let Step::Initialized { reply, session } = step else {
        panic!("应握手");
    };
    assert_eq!(reply["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(
        session,
        Session {
            version: "2025-06-18",
            client: "Cursor".into(),
        }
    );
    // 不支持的版本：回答最新的旧版本
    let Step::Initialized { session, .. } = request(
        &json!(2),
        "initialize",
        Some(json!({ "protocolVersion": "2030-01-01" })),
        None,
    ) else {
        panic!("应握手");
    };
    assert_eq!(session.version, LEGACY_VERSIONS[0]);
}

#[test]
fn legacy_requests_use_the_session() {
    let session = legacy("2025-03-26");
    let list = reply(request(&json!(1), "tools/list", None, Some(&session)));
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 5);
    assert!(
        tools[0]["annotations"].is_object(),
        "2025-03-26 起有 annotations"
    );
    assert!(tools[0].get("title").is_none(), "2025-06-18 之前没有 title");
    assert!(list["result"].get("resultType").is_none());
    // 没有会话：只能 ping
    assert_eq!(
        reply(request(&json!(2), "ping", None, None))["result"],
        json!({})
    );
    let error = reply(request(&json!(3), "tools/list", None, None));
    assert_eq!(error_code(&error), Some(INVALID_PARAMS));
    // 旧协议没有 server/discover
    let error = reply(request(&json!(4), "server/discover", None, Some(&session)));
    assert_eq!(error_code(&error), Some(METHOD_NOT_FOUND));
}

#[test]
fn modern_requests_carry_the_version() {
    let discover = reply(request(
        &json!(1),
        "server/discover",
        Some(meta(json!({}))),
        None,
    ));
    assert_eq!(
        discover["result"]["supportedVersions"],
        json!(MODERN_VERSIONS)
    );
    assert_eq!(discover["result"]["resultType"], "complete");
    // 带 _meta 的请求不用会话（即使有）
    let list = reply(request(
        &json!(2),
        "tools/list",
        Some(meta(json!({}))),
        Some(&legacy("2024-11-05")),
    ));
    assert_eq!(list["result"]["tools"][0]["title"], "List Termlet hosts");
    assert_eq!(list["result"]["cacheScope"], "public");

    let mut unsupported = meta(json!({}));
    unsupported["_meta"][META_PROTOCOL_VERSION] = json!("2099-01-01");
    let error = reply(request(&json!(3), "tools/list", Some(unsupported), None));
    assert_eq!(error_code(&error), Some(UNSUPPORTED_PROTOCOL_VERSION));
    assert_eq!(error["error"]["data"]["requested"], "2099-01-01");

    assert_eq!(meta_version(Some(&meta(json!({})))), Some("2026-07-28"));
    assert_eq!(meta_version(Some(&json!({ "_meta": {} }))), None);
    assert_eq!(meta_version(None), None);
}

#[test]
fn tool_calls_are_handed_to_the_transport() {
    let step = request(
        &json!("a"),
        "tools/call",
        Some(meta(
            json!({ "name": "run_command", "arguments": { "host": "web" } }),
        )),
        None,
    );
    let Step::Call(call) = step else {
        panic!("应交给传输层执行");
    };
    assert_eq!(call.tool, Tool::RunCommand);
    assert_eq!(call.era, Era::Modern("2026-07-28"));
    assert_eq!(call.client, "modern client", "客户端名经过清洗");
    assert_eq!(call.arguments["host"], "web");
    assert_eq!(
        call.callers(["http".to_string(), "Cursor.exe".to_string()]),
        ["mcp:modern client", "http", "Cursor.exe"]
    );
    // 结果写成这个请求的回答：新协议带 resultType
    let outcome = crate::tools::outcome(
        Tool::ListHosts,
        false,
        Ok(Response::Hosts(Vec::new())),
        Lang::En,
        Reach::Local,
    );
    let answer = call.reply(outcome);
    assert_eq!(answer["id"], "a");
    assert_eq!(answer["result"]["resultType"], "complete");
    assert_eq!(answer["result"]["isError"], false);

    // 外层参数不对：JSON-RPC 错误
    for params in [
        json!({}),
        json!({ "name": "teleport" }),
        json!({ "name": "list_hosts", "arguments": [1] }),
    ] {
        let error = reply(request(
            &json!(1),
            "tools/call",
            Some(params),
            Some(&legacy("2025-11-25")),
        ));
        assert_eq!(error_code(&error), Some(INVALID_PARAMS));
    }
    let error = reply(request(&json!(1), "ping", Some(json!([1])), None));
    assert_eq!(error_code(&error), Some(INVALID_PARAMS));
}

#[test]
fn cancellation_and_callers() {
    assert_eq!(
        cancelled_request(
            "notifications/cancelled",
            Some(&json!({ "requestId": 7, "reason": "Esc" }))
        )
        .as_deref(),
        Some("7")
    );
    assert_eq!(
        cancelled_request(
            "notifications/cancelled",
            Some(&json!({ "requestId": "x" }))
        )
        .as_deref(),
        Some("\"x\""),
        "与记下进行中的调用时一样用 JSON 的写法"
    );
    assert_eq!(cancelled_request("notifications/initialized", None), None);
    assert_eq!(caller_marker(""), "mcp");
    assert_eq!(caller_marker("codex"), "mcp:codex");
    assert_eq!(legacy_version("2025-06-18"), Some("2025-06-18"));
    assert_eq!(legacy_version("2026-07-28"), None);
}

#[test]
fn era_features_follow_the_version() {
    assert!(Era::Legacy("2025-06-18").structured());
    assert!(!Era::Legacy("2025-03-26").structured());
    assert!(Era::Legacy("2025-03-26").annotations());
    assert!(!Era::Legacy("2024-11-05").annotations());
    assert!(Era::Modern("2026-07-28").structured());
}

// 进度：请求的 _meta 中有 progressToken（字符串或整数）才发；值是已经过的秒数，2025-03-26 起带说明
#[test]
fn progress_follows_the_token() {
    let mut params = meta(json!({ "name": "run_command", "arguments": {} }));
    params["_meta"]["progressToken"] = json!("tok-1");
    let Step::Call(call) = request(&json!(1), "tools/call", Some(params), None) else {
        panic!("应交给传输层执行");
    };
    let note = call
        .progress()
        .unwrap()
        .notification(std::time::Duration::from_millis(30_900), Lang::Zh);
    assert_eq!(
        note,
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": { "progressToken": "tok-1", "progress": 30, "message": "已运行 30 秒" },
        })
    );
    assert!(note.get("id").is_none(), "通知没有 ID");

    // 旧协议：整数令牌；2024-11-05 没有 message
    let legacy_call = |version: &'static str, token: Value| {
        let params = json!({ "name": "list_hosts", "_meta": { "progressToken": token } });
        match request(
            &json!(2),
            "tools/call",
            Some(params),
            Some(&legacy(version)),
        ) {
            Step::Call(call) => call,
            other => panic!("应交给传输层执行：{other:?}"),
        }
    };
    let note = legacy_call("2025-06-18", json!(7))
        .progress()
        .unwrap()
        .notification(std::time::Duration::from_secs(15), Lang::En);
    assert_eq!(note["params"]["progressToken"], 7);
    assert_eq!(note["params"]["message"], "running for 15s");
    let old = legacy_call("2024-11-05", json!(7))
        .progress()
        .unwrap()
        .notification(std::time::Duration::from_secs(15), Lang::En);
    assert!(old["params"].get("message").is_none());

    // 没有令牌、令牌不是字符串或整数：不发
    assert!(legacy_call("2025-11-25", json!(null)).progress().is_none());
    assert!(
        legacy_call("2025-11-25", json!({ "x": 1 }))
            .progress()
            .is_none()
    );
    assert!(legacy_call("2025-11-25", json!(1.5)).progress().is_none());
}

// 局域网中的客户端：说明中写明没有传文件的工具、怎样用 run_command 读写小文件；tools/list 只有两个工具；直接调用
// 传文件的工具时交给传输层的调用带着 Lan，由 refusal 给出工具执行错误（不交给 Termlet）
#[test]
fn lan_clients_see_three_tools() {
    let init = |reach| {
        let params = json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "Cursor", "version": "3" },
        });
        match request_in(&json!(1), "initialize", Some(params), None, reach) {
            Step::Initialized { reply, .. } => reply,
            other => panic!("应握手：{other:?}"),
        }
    };
    let lan = init(Reach::Lan)["result"]["instructions"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        lan.contains("local network") && lan.contains("heredoc"),
        "{lan}"
    );
    let local = init(Reach::Local)["result"]["instructions"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(!local.contains("local network"), "{local}");

    let names = |reply: &Value| -> Vec<String> {
        reply["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    };
    let session = legacy("2025-11-25");
    let listed = reply(request_in(
        &json!(2),
        "tools/list",
        None,
        Some(&session),
        Reach::Lan,
    ));
    assert_eq!(
        names(&listed),
        ["list_hosts", "run_command", "run_command_on_hosts"]
    );
    let modern = reply(request_in(
        &json!(3),
        "tools/list",
        Some(meta(json!({}))),
        None,
        Reach::Lan,
    ));
    assert_eq!(
        names(&modern),
        ["list_hosts", "run_command", "run_command_on_hosts"]
    );
    let discover = reply(request_in(
        &json!(4),
        "server/discover",
        Some(meta(json!({}))),
        None,
        Reach::Lan,
    ));
    assert!(
        discover["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("local network")
    );

    let call = |tool: &str, reach| {
        let params = json!({ "name": tool, "arguments": {} });
        match request_in(&json!(5), "tools/call", Some(params), Some(&session), reach) {
            Step::Call(call) => call,
            other => panic!("应交给传输层执行：{other:?}"),
        }
    };
    let refused = call("download_file", Reach::Lan).refusal(Lang::En).unwrap();
    assert!(refused.is_error());
    assert!(call("run_command", Reach::Lan).refusal(Lang::En).is_none());
    assert!(call("list_hosts", Reach::Lan).refusal(Lang::En).is_none());
    assert!(
        call("run_command_on_hosts", Reach::Lan)
            .refusal(Lang::En)
            .is_none()
    );
    assert!(
        call("upload_file", Reach::Local)
            .refusal(Lang::En)
            .is_none()
    );
    // 本机的客户端照旧全部五个工具
    let all = reply(request(&json!(6), "tools/list", None, Some(&session)));
    assert_eq!(names(&all).len(), 5);
}
