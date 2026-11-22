//! 两代协议的分发：按每个请求的 `_meta` 协商版本的 2026-07-28（没有握手，`server/discover`），与以 `initialize`
//! 握手的 2025-11-25、2025-06-18、2025-03-26、2024-11-05（规范所说的 dual-era）。
//!
//! 这里只管一条消息：拆开（[`classify`]）、处理一个请求（[`request`]：立即回答、握手、或交给调用方执行的工具调用）、
//! 通知中的取消（[`cancelled_request`]）。读写消息、并发、取消、会话的保存由传输层（stdio、HTTP）负责。

use serde_json::{Map, Value, json};
use termlet_proto::mcp::CALLER_MARKER;

use crate::tools::{self, Outcome, Tool};

/// 按每个请求的 `_meta` 协商的版本。
pub const MODERN_VERSIONS: &[&str] = &["2026-07-28"];
/// 以 `initialize` 握手的版本，由新到旧；客户端要的不在其中时回答第一个（客户端不支持时自行断开）。
pub const LEGACY_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// HTTP 上没有会话、也没有 `MCP-Protocol-Version` 头时按这个版本（规范：2025-06-18 之前的客户端不发这个头）。
pub const HEADERLESS_VERSION: &str = "2025-03-26";

pub const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
pub const META_CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";
pub const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
pub const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

// JSON-RPC 与 MCP 的错误码
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
/// HTTP 的请求头与正文不一致，或缺少必需的请求头（2026-07-28 的 `HeaderMismatch`）
pub const HEADER_MISMATCH: i64 = -32020;
pub const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

/// 一条消息的上限：命令最多 1MB，JSON 转义后会变长。stdio 的一行、HTTP 的正文都按它。
pub const MAX_MESSAGE: usize = 8 << 20;

/// `tools/list`、`server/discover` 的结果可以缓存多久：工具随程序的版本固定。
const CACHE_TTL_MS: u64 = 60 * 60 * 1000;

/// 给 AI 的说明（`initialize`、`server/discover` 的 `instructions`）。
const INSTRUCTIONS: &str = "Termlet is the user's SSH client. These tools list the servers saved in Termlet that the user allows AI tools to use, run shell commands on them and transfer single files, with credentials that stay inside Termlet (you never see passwords or keys). Use them instead of running ssh, scp or sftp yourself. Start with list_hosts and pass a host's id or name to the other tools. Every call goes through the user's settings in Termlet: the external CLI switch, per-host access, dangerous-command rules that need the user's approval, and an audit log. If a call fails with command_denied or command_blocked, do not try to get around the rule; tell the user and let them run the command themselves.";

/// 请求没有 `_meta` 中的版本、之前也没有 `initialize` 时的说明。
const NOT_INITIALIZED: &str = "missing _meta[\"io.modelcontextprotocol/protocolVersion\"]: include it in every request (protocol 2026-07-28), or send initialize first (2025-11-25 and earlier)";

/// 一个请求按哪个版本处理。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Era {
    /// 请求的 `_meta` 中带的版本（2026-07-28 起）
    Modern(&'static str),
    /// `initialize` 握手时协商的版本
    Legacy(&'static str),
}

impl Era {
    pub fn version(self) -> &'static str {
        match self {
            Era::Modern(version) | Era::Legacy(version) => version,
        }
    }

    pub fn is_modern(self) -> bool {
        matches!(self, Era::Modern(_))
    }

    /// 工具结果的 `structuredContent`、工具的 `title`（2025-06-18 起；版本号是日期，按字符串比较即可）。
    pub fn structured(self) -> bool {
        self.version() >= "2025-06-18"
    }

    /// 工具的 `annotations`（2025-03-26 起）。
    pub fn annotations(self) -> bool {
        self.version() >= "2025-03-26"
    }
}

/// `initialize` 握手建立的会话（旧版本）：只记协议版本与客户端自报的名字。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub version: &'static str,
    pub client: String,
}

/// 支持的旧版本中与 `version` 相同的一个（`MCP-Protocol-Version` 头里的版本）。
pub fn legacy_version(version: &str) -> Option<&'static str> {
    LEGACY_VERSIONS.iter().copied().find(|v| *v == version)
}

/// 拆开的一条 JSON-RPC 消息。
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    Request {
        id: Value,
        method: String,
        params: Option<Value>,
    },
    Notification {
        method: String,
        params: Option<Value>,
    },
    /// 客户端对服务端请求的回答：这里不发请求，忽略
    Response,
}

impl Message {
    /// 请求、通知的方法名。
    pub fn method(&self) -> Option<&str> {
        match self {
            Message::Request { method, .. } | Message::Notification { method, .. } => Some(method),
            Message::Response => None,
        }
    }

    pub fn params(&self) -> Option<&Value> {
        match self {
            Message::Request { params, .. } | Message::Notification { params, .. } => {
                params.as_ref()
            }
            Message::Response => None,
        }
    }
}

/// 解析一条消息（JSON 文本）；不合法时为要回答的 JSON-RPC 错误。
pub fn parse(bytes: &[u8]) -> Result<Message, Value> {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(value) => classify(value),
        Err(e) => Err(error_message(
            &Value::Null,
            PARSE_ERROR,
            &format!("Parse error: {e}"),
            None,
        )),
    }
}

/// 拆开一条消息；不合法时为要回答的 JSON-RPC 错误（能取出 ID 时带上）。
pub fn classify(message: Value) -> Result<Message, Value> {
    let Value::Object(mut message) = message else {
        // JSON-RPC 的批量（数组）从 2025-06-18 起不再支持
        return Err(error_message(
            &Value::Null,
            INVALID_REQUEST,
            "expected one JSON-RPC message object (batches are not supported)",
            None,
        ));
    };
    let id = message.remove("id");
    let method = match message.remove("method") {
        Some(Value::String(method)) => method,
        None if message.contains_key("result") || message.contains_key("error") => {
            return Ok(Message::Response);
        }
        _ => {
            let id = id.filter(valid_id).unwrap_or(Value::Null);
            return Err(error_message(
                &id,
                INVALID_REQUEST,
                "invalid JSON-RPC message",
                None,
            ));
        }
    };
    let params = message.remove("params");
    match id {
        None => Ok(Message::Notification { method, params }),
        Some(id) if valid_id(&id) => Ok(Message::Request { id, method, params }),
        Some(_) => Err(error_message(
            &Value::Null,
            INVALID_REQUEST,
            "id must be a string or an integer",
            None,
        )),
    }
}

fn valid_id(id: &Value) -> bool {
    id.is_string() || id.is_i64() || id.is_u64()
}

/// 请求的 `_meta` 中带的协议版本（新协议的请求才有；不是字符串时为空串，处理时报错）。
pub fn meta_version(params: Option<&Value>) -> Option<&str> {
    let meta = params?.get("_meta")?.as_object()?;
    let version = meta.get(META_PROTOCOL_VERSION)?;
    Some(version.as_str().unwrap_or_default())
}

/// 处理一个请求的结果。
#[derive(Debug)]
pub enum Step {
    /// 立即回答
    Reply(Value),
    /// `initialize`：回答，传输层记住这次握手
    Initialized { reply: Value, session: Session },
    /// `tools/call`：参数的外层已检查，由传输层执行（[`ToolCall`]）后回答
    Call(ToolCall),
}

/// 一个要执行的工具调用。
#[derive(Debug)]
pub struct ToolCall {
    pub id: Value,
    pub era: Era,
    pub tool: Tool,
    pub arguments: Map<String, Value>,
    /// 客户端自报的名字（清洗过，可能为空）
    pub client: String,
}

impl ToolCall {
    /// 工具的结果写成这个请求的回答。
    pub fn reply(&self, outcome: Outcome) -> Value {
        result_message(
            &self.id,
            finish(self.era, outcome.into_result(self.era.structured())),
        )
    }

    /// 审计与批准框中的调用方：`mcp:<客户端名>`（没有名字时为 `mcp`），后面是传输层给的（进程链等）。
    pub fn callers(&self, rest: impl IntoIterator<Item = String>) -> Vec<String> {
        std::iter::once(caller_marker(&self.client))
            .chain(rest)
            .collect()
    }
}

/// 调用方的第一项：`mcp:<客户端名>`，客户端没有报名字时只有 `mcp`。
pub fn caller_marker(client: &str) -> String {
    if client.is_empty() {
        CALLER_MARKER.to_string()
    } else {
        format!("{CALLER_MARKER}:{client}")
    }
}

/// 处理一个请求。`session` 是旧协议下握手时记住的会话（请求带 `_meta` 中的版本时不用它）：stdio 一个进程一个；
/// HTTP 按会话 ID 找，找不到时由传输层按请求头补一个。没有会话的旧协议请求只能 `ping`。
pub fn request(id: &Value, method: &str, params: Option<Value>, session: Option<&Session>) -> Step {
    let params = match params {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(params)) => params,
        Some(_) => {
            return Step::Reply(error_message(
                id,
                INVALID_PARAMS,
                "params must be an object",
                None,
            ));
        }
    };
    if method == "initialize" {
        return initialize(id, &params);
    }
    let meta = params
        .get("_meta")
        .and_then(Value::as_object)
        .filter(|meta| meta.contains_key(META_PROTOCOL_VERSION));
    let (era, client) = match meta {
        Some(meta) => match modern_era(meta) {
            Ok(found) => found,
            Err(e) => return Step::Reply(error_message(id, e.code, &e.message, e.data)),
        },
        None => match session {
            Some(session) => (Era::Legacy(session.version), session.client.clone()),
            // 握手前的 ping（旧版本允许）
            None if method == "ping" => return Step::Reply(result_message(id, json!({}))),
            None => {
                return Step::Reply(error_message(id, INVALID_PARAMS, NOT_INITIALIZED, None));
            }
        },
    };
    let reply = |result| Step::Reply(result_message(id, finish(era, result)));
    match method {
        "server/discover" if era.is_modern() => reply(discover()),
        "ping" => reply(Map::new()),
        "tools/list" => {
            let mut result = Map::new();
            result.insert(
                "tools".into(),
                tools::list(era.annotations(), era.structured()),
            );
            if era.is_modern() {
                cacheable(&mut result);
            }
            reply(result)
        }
        "tools/call" => call_tool(id, era, client, &params),
        _ => Step::Reply(error_message(
            id,
            METHOD_NOT_FOUND,
            &format!("Method not found: {method}"),
            None,
        )),
    }
}

/// 旧版本的握手：支持客户端要的版本时用它，否则回答支持的最新版本。
fn initialize(id: &Value, params: &Map<String, Value>) -> Step {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let version = legacy_version(requested).unwrap_or(LEGACY_VERSIONS[0]);
    let client = client_name(params.get("clientInfo"));
    Step::Initialized {
        reply: result_message(
            id,
            json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": server_info(),
                "instructions": INSTRUCTIONS,
            }),
        ),
        session: Session { version, client },
    }
}

fn call_tool(id: &Value, era: Era, client: String, params: &Map<String, Value>) -> Step {
    let invalid = |message: &str| Step::Reply(error_message(id, INVALID_PARAMS, message, None));
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return invalid("params.name (the tool to call) must be a string");
    };
    let Some(tool) = Tool::parse(name) else {
        return invalid(&format!("Unknown tool: {name}"));
    };
    let arguments = match params.get("arguments") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(arguments)) => arguments.clone(),
        Some(_) => return invalid("params.arguments must be an object"),
    };
    Step::Call(ToolCall {
        id: id.clone(),
        era,
        tool,
        arguments,
        client,
    })
}

/// 通知 `notifications/cancelled` 要取消的请求 ID（JSON 的写法，与传输层记下进行中的调用时用的相同）；
/// 别的通知为 `None`（`notifications/initialized` 等不需要处理）。
pub fn cancelled_request(method: &str, params: Option<&Value>) -> Option<String> {
    if method != "notifications/cancelled" {
        return None;
    }
    params?.get("requestId").map(Value::to_string)
}

/// JSON-RPC 错误。
struct RpcError {
    code: i64,
    message: String,
    data: Option<Value>,
}

/// 带 `_meta` 中的版本的请求：检查版本与必需的字段，取出客户端的名字。
fn modern_era(meta: &Map<String, Value>) -> Result<(Era, String), RpcError> {
    let params = |message: &str| RpcError {
        code: INVALID_PARAMS,
        message: message.to_string(),
        data: None,
    };
    let Some(requested) = meta.get(META_PROTOCOL_VERSION).and_then(Value::as_str) else {
        return Err(params(
            "_meta[\"io.modelcontextprotocol/protocolVersion\"] must be a string",
        ));
    };
    let Some(&version) = MODERN_VERSIONS.iter().find(|v| **v == requested) else {
        return Err(RpcError {
            code: UNSUPPORTED_PROTOCOL_VERSION,
            message: "Unsupported protocol version".into(),
            data: Some(json!({ "supported": MODERN_VERSIONS, "requested": requested })),
        });
    };
    if !meta
        .get(META_CLIENT_CAPABILITIES)
        .is_some_and(Value::is_object)
    {
        return Err(params(
            "missing _meta[\"io.modelcontextprotocol/clientCapabilities\"]",
        ));
    }
    Ok((
        Era::Modern(version),
        client_name(meta.get(META_CLIENT_INFO)),
    ))
}

/// 客户端自报的名字（`clientInfo.name`）：只留字母、数字与 ` ._-`，最多 48 个字符，写进审计的调用方。
/// 只用于显示与审计，不参与任何判断。
pub fn client_name(info: Option<&Value>) -> String {
    let name = info
        .and_then(|info| info.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let kept: String = name
        .chars()
        .filter(|c| c.is_alphanumeric() || " ._-".contains(*c))
        .take(48)
        .collect();
    kept.trim().to_string()
}

fn server_info() -> Value {
    json!({
        "name": termlet_proto::mcp::SERVER_NAME,
        "title": "Termlet",
        "version": env!("CARGO_PKG_VERSION"),
    })
}

fn discover() -> Map<String, Value> {
    let mut result = Map::new();
    result.insert("supportedVersions".into(), json!(MODERN_VERSIONS));
    result.insert("capabilities".into(), json!({ "tools": {} }));
    result.insert("instructions".into(), json!(INSTRUCTIONS));
    cacheable(&mut result);
    result
}

/// 新版本中可缓存的结果必须带的字段：工具随程序的版本固定，不含用户的数据。
fn cacheable(result: &mut Map<String, Value>) {
    result.insert("ttlMs".into(), json!(CACHE_TTL_MS));
    result.insert("cacheScope".into(), json!("public"));
}

/// 结果：新版本加上 `resultType` 与 `_meta` 中的服务端信息。
fn finish(era: Era, mut result: Map<String, Value>) -> Value {
    if era.is_modern() {
        result.insert("resultType".into(), json!("complete"));
        let meta = result
            .entry("_meta")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(meta) = meta.as_object_mut() {
            meta.insert(META_SERVER_INFO.into(), server_info());
        }
    }
    Value::Object(result)
}

pub fn result_message(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn error_message(id: &Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut error = json!({ "code": code, "message": message });
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": error })
}

/// JSON-RPC 错误回答中的错误码（不是错误时为 `None`）。
pub fn error_code(message: &Value) -> Option<i64> {
    message.get("error")?.get("code")?.as_i64()
}

#[cfg(test)]
mod tests;
