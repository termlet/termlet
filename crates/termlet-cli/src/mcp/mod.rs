//! `termlet mcp`：stdio 上的 MCP 服务，由 Claude Code、Claude Desktop、Cursor、Codex 等 AI 工具启动（不开端口）。
//!
//! 工具与命令行工具一一对应：`list_hosts`（`termlet ls`）、`run_command`（`termlet exec`，含 `--terminal`）、
//! `upload_file`、`download_file`。每次调用照常连到 Termlet（没有运行时按设置在后台拉起）、发同样的请求：外部 CLI 的开关、
//! 按主机授权、CLI 专用凭据、危险命令规则与批准、应用锁与保险库、审计日志都由应用端检查，凭据不出 Termlet；这里不绕过任何
//! 一道，也不另做一套。审计与批准框中的调用方前面加上 `mcp:<客户端名>`（客户端自报的名字，只用于显示）。
//!
//! # 协议
//!
//! - 支持按每个请求的 `_meta` 协商版本的 2026-07-28（没有握手，`server/discover`），也支持以 `initialize` 握手的
//!   2025-11-25、2025-06-18、2025-03-26、2024-11-05（规范所说的 dual-era）：请求带 `_meta` 中的版本时按新的方式回答，
//!   `initialize` 之后不带的按握手时协商的版本回答。
//! - 一行一条 JSON-RPC 2.0 消息（UTF-8，消息中没有换行）；标准输出只写 MCP 消息，调试信息（`TERMLET_LOG`）写标准错误。
//! - 业务上的失败（外部 CLI 已关闭、Termlet 没有运行、命令被拦截或没有获得批准、连接失败等）是工具执行错误
//!   （`isError: true`，文本中有错误码与说明，AI 据此处理）；未知的工具、参数不是对象等是 JSON-RPC 错误。
//! - 每个 `tools/call` 在单独的线程中执行（可以并发），各用一条到 Termlet 的连接；`notifications/cancelled` 关闭这条连接，
//!   Termlet 中止这次请求（远程命令收到 SIGTERM、批准框关闭，审计记为已取消），被取消的请求不再回答。
//! - 标准输入关闭时立即退出：进行中的请求随连接关闭被 Termlet 中止。
//! - 只有 stdio：不做 HTTP / SSE 传输。

mod tools;

use std::collections::HashMap;
use std::io::{self, BufRead, IsTerminal as _, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use serde_json::{Map, Value, json};
use termlet_proto::client::Abort;
use termlet_proto::mcp::CALLER_MARKER;
use termlet_proto::{ErrorCode, ExitCode, Request, Response};

use crate::app::{self, Failure};
use crate::debug::debug;
use crate::i18n::Lang;
use crate::messages;

/// 按每个请求的 `_meta` 协商的版本。
const MODERN_VERSIONS: &[&str] = &["2026-07-28"];
/// 以 `initialize` 握手的版本，由新到旧；客户端要的不在其中时回答第一个（客户端不支持时自行断开）。
const LEGACY_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";
const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

// JSON-RPC 与 MCP 的错误码
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

/// 一行消息的上限：命令最多 1MB，JSON 转义后会变长。
const MAX_MESSAGE: usize = 8 << 20;

/// `tools/list`、`server/discover` 的结果可以缓存多久：工具随程序的版本固定。
const CACHE_TTL_MS: u64 = 60 * 60 * 1000;

/// 给 AI 的说明（`initialize`、`server/discover` 的 `instructions`）。
const INSTRUCTIONS: &str = "Termlet is the user's SSH client. These tools list the servers saved in Termlet that the user allows AI tools to use, run shell commands on them and transfer single files, with credentials that stay inside Termlet (you never see passwords or keys). Use them instead of running ssh, scp or sftp yourself. Start with list_hosts and pass a host's id or name to the other tools. Every call goes through the user's settings in Termlet: the external CLI switch, per-host access, dangerous-command rules that need the user's approval, and an audit log. If a call fails with command_denied or command_blocked, do not try to get around the rule; tell the user and let them run the command themselves.";

/// 请求没有 `_meta` 中的版本、之前也没有 `initialize` 时的说明。
const NOT_INITIALIZED: &str = "missing _meta[\"io.modelcontextprotocol/protocolVersion\"]: include it in every request (protocol 2026-07-28), or send initialize first (2025-11-25 and earlier)";

/// 一个请求按哪个版本处理。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Era {
    /// 请求的 `_meta` 中带的版本（2026-07-28 起）
    Modern(&'static str),
    /// `initialize` 握手时协商的版本
    Legacy(&'static str),
}

impl Era {
    fn version(self) -> &'static str {
        match self {
            Era::Modern(version) | Era::Legacy(version) => version,
        }
    }

    fn is_modern(self) -> bool {
        matches!(self, Era::Modern(_))
    }

    /// 工具结果的 `structuredContent`、工具的 `title`（2025-06-18 起；版本号是日期，按字符串比较即可）。
    fn structured(self) -> bool {
        self.version() >= "2025-06-18"
    }

    /// 工具的 `annotations`（2025-03-26 起）。
    fn annotations(self) -> bool {
        self.version() >= "2025-03-26"
    }
}

/// `initialize` 握手建立的会话（旧版本）。
#[derive(Clone, Debug)]
struct Session {
    version: &'static str,
    client: String,
}

/// 一个进行中的工具调用的取消：连上 Termlet 前取消的不再发请求，之后取消的关闭连接。
#[derive(Default)]
pub(crate) struct AbortSlot {
    aborted: AtomicBool,
    handle: Mutex<Option<Abort>>,
}

impl AbortSlot {
    /// 连上 Termlet 后交给它中止连接的句柄；已经取消时返回假（不要再发请求）。
    pub(crate) fn arm(&self, handle: Option<Abort>) -> bool {
        let mut slot = lock(&self.handle);
        if self.aborted.load(Ordering::SeqCst) {
            return false;
        }
        *slot = handle;
        true
    }

    /// 取消：关闭到 Termlet 的连接（有的话）。
    pub(crate) fn abort(&self) {
        let handle = {
            let mut slot = lock(&self.handle);
            self.aborted.store(true, Ordering::SeqCst);
            slot.take()
        };
        if let Some(handle) = handle {
            handle.abort();
        }
    }

    pub(crate) fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::SeqCst)
    }
}

/// 怎么把请求交给 Termlet（测试中换成假的）。
pub(crate) trait Backend: Send + Sync {
    /// 连上 Termlet 发一个请求，等它回答；`slot` 被取消时中止。连不上、通信失败时为 `Err`（说明已按语言写好）。
    fn call(&self, request: Request, slot: &AbortSlot) -> Result<Response, Failure>;
}

/// 连到正在运行（或按设置拉起）的 Termlet，与命令行工具相同。
struct AppBackend {
    lang: Lang,
}

impl Backend for AppBackend {
    fn call(&self, request: Request, slot: &AbortSlot) -> Result<Response, Failure> {
        let mut client = app::connect(self.lang)?;
        if !slot.arm(client.abort_handle().ok()) {
            return Err(Failure {
                exit: ExitCode::Failed,
                code: ErrorCode::Canceled.as_str().into(),
                message: String::new(),
            });
        }
        client
            .call(&request)
            .map_err(|e| app::call_failure(&e, self.lang))
    }
}

/// `termlet mcp`：在标准输入输出上提供服务，标准输入关闭时返回退出码。
pub fn main(args: &[String]) -> i32 {
    let lang = crate::i18n::current();
    if !args.is_empty() {
        eprintln!("termlet: {}", messages::USAGE_MCP.get(lang));
        return ExitCode::Usage.into();
    }
    let stdin = io::stdin();
    if stdin.is_terminal() {
        eprintln!("{}", messages::MCP_INTERACTIVE.get(lang));
    }
    let server = Arc::new(Server::new(
        Arc::new(AppBackend { lang }),
        Box::new(io::stdout()),
        lang,
        crate::caller::chain(),
    ));
    debug!("MCP 服务已启动");
    server.serve(stdin.lock());
    // 不等进行中的请求：进程退出时连接随之关闭，Termlet 中止它们（AI 工具已不再读取结果）
    debug!("标准输入已关闭，MCP 服务退出");
    ExitCode::Success.into()
}

pub(crate) struct Server {
    backend: Arc<dyn Backend>,
    out: Mutex<Box<dyn Write + Send>>,
    lang: Lang,
    /// 本进程的祖先进程名（启动 MCP 服务的 AI 工具在其中），由近到远
    callers: Vec<String>,
    session: Mutex<Option<Session>>,
    /// 进行中的工具调用：请求 ID（JSON 的写法）→ 取消
    inflight: Mutex<HashMap<String, Arc<AbortSlot>>>,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

impl Server {
    pub(crate) fn new(
        backend: Arc<dyn Backend>,
        out: Box<dyn Write + Send>,
        lang: Lang,
        callers: Vec<String>,
    ) -> Self {
        Self {
            backend,
            out: Mutex::new(out),
            lang,
            callers,
            session: Mutex::new(None),
            inflight: Mutex::default(),
            workers: Mutex::default(),
        }
    }

    /// 逐行读取消息并处理，直到输入结束。工具调用在后台线程中执行，返回时可能还没有完成。
    pub(crate) fn serve(self: &Arc<Self>, mut input: impl BufRead) {
        let mut line = Vec::new();
        loop {
            match read_line(&mut input, &mut line, MAX_MESSAGE) {
                Ok(Line::Complete) => self.receive(&line),
                Ok(Line::TooLong) => self.send(&error_message(
                    &Value::Null,
                    INVALID_REQUEST,
                    "message too large",
                    None,
                )),
                Ok(Line::End) => break,
                Err(e) => {
                    debug!("读取标准输入失败：{e}");
                    break;
                }
            }
        }
    }

    /// 等进行中的工具调用都完成（测试用）。
    #[cfg(test)]
    pub(crate) fn join(&self) {
        let workers = std::mem::take(&mut *lock(&self.workers));
        for worker in workers {
            let _ = worker.join();
        }
    }

    fn receive(self: &Arc<Self>, line: &[u8]) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.iter().all(u8::is_ascii_whitespace) {
            return;
        }
        match serde_json::from_slice::<Value>(line) {
            Ok(message) => self.dispatch(message),
            Err(e) => self.send(&error_message(
                &Value::Null,
                PARSE_ERROR,
                &format!("Parse error: {e}"),
                None,
            )),
        }
    }

    fn dispatch(self: &Arc<Self>, message: Value) {
        let Value::Object(mut message) = message else {
            // JSON-RPC 的批量（数组）从 2025-06-18 起不再支持
            return self.send(&error_message(
                &Value::Null,
                INVALID_REQUEST,
                "expected one JSON-RPC message object per line (batches are not supported)",
                None,
            ));
        };
        let id = message.remove("id");
        let method = match message.remove("method") {
            Some(Value::String(method)) => method,
            // 客户端对服务端请求的回答：这里不发请求，忽略
            None if message.contains_key("result") || message.contains_key("error") => return,
            _ => {
                let id = id.filter(valid_id).unwrap_or(Value::Null);
                return self.send(&error_message(
                    &id,
                    INVALID_REQUEST,
                    "invalid JSON-RPC message",
                    None,
                ));
            }
        };
        let params = message.remove("params");
        match id {
            None => self.notify(&method, params.as_ref()),
            Some(id) if valid_id(&id) => {
                if let Some(reply) = self.request(&id, &method, params) {
                    self.send(&reply);
                }
            }
            Some(_) => self.send(&error_message(
                &Value::Null,
                INVALID_REQUEST,
                "id must be a string or an integer",
                None,
            )),
        }
    }

    /// 处理一个请求：返回要立即发出的回答；工具调用在后台完成后再回答（返回 `None`）。
    fn request(self: &Arc<Self>, id: &Value, method: &str, params: Option<Value>) -> Option<Value> {
        let params = match params {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(params)) => params,
            Some(_) => {
                return Some(error_message(
                    id,
                    INVALID_PARAMS,
                    "params must be an object",
                    None,
                ));
            }
        };
        if method == "initialize" {
            return Some(self.initialize(id, &params));
        }
        let meta = params
            .get("_meta")
            .and_then(Value::as_object)
            .filter(|meta| meta.contains_key(META_PROTOCOL_VERSION));
        let (era, client) = match meta {
            Some(meta) => match modern_era(meta) {
                Ok(found) => found,
                Err(e) => return Some(error_message(id, e.code, &e.message, e.data)),
            },
            None => match lock(&self.session).clone() {
                Some(session) => (Era::Legacy(session.version), session.client),
                // 握手前的 ping（旧版本允许）
                None if method == "ping" => return Some(result_message(id, json!({}))),
                None => return Some(error_message(id, INVALID_PARAMS, NOT_INITIALIZED, None)),
            },
        };
        match method {
            "server/discover" if era.is_modern() => {
                Some(result_message(id, finish(era, discover())))
            }
            "ping" => Some(result_message(id, finish(era, Map::new()))),
            "tools/list" => {
                let mut result = Map::new();
                result.insert(
                    "tools".into(),
                    tools::list(era.annotations(), era.structured()),
                );
                if era.is_modern() {
                    cacheable(&mut result);
                }
                Some(result_message(id, finish(era, result)))
            }
            "tools/call" => self.call_tool(id, era, &client, &params),
            _ => Some(error_message(
                id,
                METHOD_NOT_FOUND,
                &format!("Method not found: {method}"),
                None,
            )),
        }
    }

    /// 旧版本的握手：支持客户端要的版本时用它，否则回答支持的最新版本。
    fn initialize(&self, id: &Value, params: &Map<String, Value>) -> Value {
        let requested = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let version = LEGACY_VERSIONS
            .iter()
            .copied()
            .find(|v| *v == requested)
            .unwrap_or(LEGACY_VERSIONS[0]);
        let client = client_name(params.get("clientInfo"));
        debug!("MCP 握手：客户端 {client}，协议版本 {version}");
        *lock(&self.session) = Some(Session { version, client });
        result_message(
            id,
            json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": server_info(),
                "instructions": INSTRUCTIONS,
            }),
        )
    }

    fn call_tool(
        self: &Arc<Self>,
        id: &Value,
        era: Era,
        client: &str,
        params: &Map<String, Value>,
    ) -> Option<Value> {
        let invalid = |message: &str| Some(error_message(id, INVALID_PARAMS, message, None));
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return invalid("params.name (the tool to call) must be a string");
        };
        let Some(tool) = tools::Tool::parse(name) else {
            return invalid(&format!("Unknown tool: {name}"));
        };
        let arguments = match params.get("arguments") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(arguments)) => arguments.clone(),
            Some(_) => return invalid("params.arguments must be an object"),
        };
        let key = id.to_string();
        let slot = Arc::new(AbortSlot::default());
        {
            let mut inflight = lock(&self.inflight);
            if inflight.contains_key(&key) {
                return Some(error_message(
                    id,
                    INVALID_REQUEST,
                    "a request with this id is still in progress",
                    None,
                ));
            }
            inflight.insert(key.clone(), slot.clone());
        }
        debug!("MCP：调用工具 {}", tool.name());
        let server = self.clone();
        let id = id.clone();
        let callers = self.callers_for(client);
        let worker = std::thread::spawn(move || {
            let context = tools::Context {
                backend: &*server.backend,
                lang: server.lang,
                callers: &callers,
                slot: &slot,
            };
            let outcome = tools::call(tool, &arguments, &context);
            lock(&server.inflight).remove(&key);
            if slot.is_aborted() {
                debug!("MCP：工具 {} 已取消，不再回答", tool.name());
                return;
            }
            server.send(&result_message(
                &id,
                finish(era, outcome.into_result(era.structured())),
            ));
        });
        let mut workers = lock(&self.workers);
        workers.retain(|worker| !worker.is_finished());
        workers.push(worker);
        None
    }

    /// 通知：只处理取消（`notifications/initialized` 等不需要处理）。
    fn notify(&self, method: &str, params: Option<&Value>) {
        if method != "notifications/cancelled" {
            return;
        }
        let Some(id) = params.and_then(|p| p.get("requestId")) else {
            return;
        };
        let slot = lock(&self.inflight).get(&id.to_string()).cloned();
        if let Some(slot) = slot {
            debug!("MCP：取消进行中的工具调用");
            slot.abort();
        }
    }

    /// 审计与批准框中的调用方：`mcp:<客户端名>` 加上本进程的祖先进程。
    fn callers_for(&self, client: &str) -> Vec<String> {
        let marker = if client.is_empty() {
            CALLER_MARKER.to_string()
        } else {
            format!("{CALLER_MARKER}:{client}")
        };
        std::iter::once(marker)
            .chain(self.callers.iter().cloned())
            .collect()
    }

    /// 写一条消息（一行）。
    fn send(&self, message: &Value) {
        let mut out = lock(&self.out);
        // serde_json 把字符串中的换行写成 \n，一条消息正好一行
        let written = serde_json::to_writer(&mut *out, message)
            .map_err(io::Error::from)
            .and_then(|()| out.write_all(b"\n"))
            .and_then(|()| out.flush());
        if let Err(e) = written {
            debug!("写标准输出失败：{e}");
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn valid_id(id: &Value) -> bool {
    id.is_string() || id.is_i64() || id.is_u64()
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
fn client_name(info: Option<&Value>) -> String {
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

fn result_message(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_message(id: &Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut error = json!({ "code": code, "message": message });
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": error })
}

/// 读一行的结果。
#[derive(Debug, PartialEq, Eq)]
enum Line {
    Complete,
    /// 超过上限（已跳过这一行）
    TooLong,
    /// 输入结束
    End,
}

/// 读一行（不含换行）到 `buf`；超过 `max` 字节的行跳过到下一个换行。输入结束于没有换行的一行时照常返回这一行。
fn read_line(input: &mut impl BufRead, buf: &mut Vec<u8>, max: usize) -> io::Result<Line> {
    buf.clear();
    let mut read_any = false;
    let mut too_long = false;
    loop {
        let available = match input.fill_buf() {
            Ok(available) => available,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            return Ok(match (read_any, too_long) {
                (false, _) => Line::End,
                (true, true) => Line::TooLong,
                (true, false) => Line::Complete,
            });
        }
        read_any = true;
        let newline = available.iter().position(|&b| b == b'\n');
        let chunk = &available[..newline.unwrap_or(available.len())];
        if !too_long {
            if buf.len() + chunk.len() > max {
                too_long = true;
                buf.clear();
            } else {
                buf.extend_from_slice(chunk);
            }
        }
        let used = newline.map_or(available.len(), |i| i + 1);
        input.consume(used);
        if newline.is_some() {
            return Ok(if too_long {
                Line::TooLong
            } else {
                Line::Complete
            });
        }
    }
}

#[cfg(test)]
mod tests;
