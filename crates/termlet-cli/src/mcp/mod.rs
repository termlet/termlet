//! `termlet mcp`：stdio 上的 MCP 服务，由 Claude Code、Claude Desktop、Cursor、Codex 等 AI 工具启动（不开端口）。
//!
//! 工具与命令行工具一一对应：`list_hosts`（`termlet ls`）、`run_command`（`termlet exec`，含 `--terminal`）、
//! `upload_file`、`download_file`。每次调用照常连到 Termlet（没有运行时按设置在后台拉起）、发同样的请求：外部 CLI 的开关、
//! 按主机授权、CLI 专用凭据、危险命令规则与批准、应用锁与保险库、审计日志都由应用端检查，凭据不出 Termlet；这里不绕过任何
//! 一道，也不另做一套。审计与批准框中的调用方前面加上 `mcp:<客户端名>`（客户端自报的名字，只用于显示）。
//!
//! 协议的分发、工具的定义与结果的写法在 `termlet-mcp` 中，与 Termlet 应用内的 HTTP 服务共用；这里只是 stdio 传输：
//!
//! - 一行一条 JSON-RPC 2.0 消息（UTF-8，消息中没有换行）；标准输出只写 MCP 消息，调试信息（`TERMLET_LOG`）写标准错误。
//! - 旧协议的会话（`initialize` 协商的版本与客户端名）一个进程一个。
//! - 每个 `tools/call` 在单独的线程中执行（可以并发），各用一条到 Termlet 的连接；`notifications/cancelled` 关闭这条连接，
//!   Termlet 中止这次请求（远程命令收到 SIGTERM、批准框关闭，审计记为已取消），被取消的请求不再回答。
//! - 请求的 `_meta` 中带 `progressToken` 时，执行期间每 15 秒发一条 `notifications/progress`（只用于保活：有的客户端
//!   按一段时间没有进度中止调用），回答之后不再发。
//! - 标准输入关闭时立即退出：进行中的请求随连接关闭被 Termlet 中止。

use std::collections::HashMap;
use std::io::{self, BufRead, IsTerminal as _, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;
use termlet_mcp::protocol::{self, Message, Session, Step, ToolCall};
use termlet_mcp::tools;
use termlet_mcp::{AbortSlot, Backend, Failure, INVALID_REQUEST, MAX_MESSAGE};
use termlet_proto::{ErrorCode, ExitCode, Request, Response};

use crate::app;
use crate::debug::debug;
use crate::i18n::Lang;
use crate::messages;

/// 长时间的工具调用多久发一次进度。
const PROGRESS_EVERY: Duration = Duration::from_secs(15);

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
        termlet_proto::caller::chain(),
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
    /// 进度通知的间隔（测试中调短）
    progress_every: Duration,
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
            progress_every: PROGRESS_EVERY,
        }
    }

    /// 进度通知的间隔（测试用）。
    #[cfg(test)]
    pub(crate) fn with_progress_every(mut self, every: Duration) -> Self {
        self.progress_every = every;
        self
    }

    /// 逐行读取消息并处理，直到输入结束。工具调用在后台线程中执行，返回时可能还没有完成。
    pub(crate) fn serve(self: &Arc<Self>, mut input: impl BufRead) {
        let mut line = Vec::new();
        loop {
            match read_line(&mut input, &mut line, MAX_MESSAGE) {
                Ok(Line::Complete) => self.receive(&line),
                Ok(Line::TooLong) => self.send(&protocol::error_message(
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
        match protocol::parse(line) {
            Ok(Message::Request { id, method, params }) => {
                let session = lock(&self.session).clone();
                match protocol::request(&id, &method, params, session.as_ref()) {
                    Step::Reply(reply) => self.send(&reply),
                    Step::Initialized { reply, session } => {
                        debug!(
                            "MCP 握手：客户端 {}，协议版本 {}",
                            session.client, session.version
                        );
                        *lock(&self.session) = Some(session);
                        self.send(&reply);
                    }
                    Step::Call(call) => self.call_tool(call),
                }
            }
            Ok(Message::Notification { method, params }) => self.notify(&method, params.as_ref()),
            Ok(Message::Response) => {}
            Err(reply) => self.send(&reply),
        }
    }

    /// 工具调用在后台线程中执行，完成后再回答；同一个 ID 还在进行中时报错。
    fn call_tool(self: &Arc<Self>, call: ToolCall) {
        let key = call.id.to_string();
        let slot = Arc::new(AbortSlot::default());
        {
            let mut inflight = lock(&self.inflight);
            if inflight.contains_key(&key) {
                drop(inflight);
                return self.send(&protocol::error_message(
                    &call.id,
                    INVALID_REQUEST,
                    "a request with this id is still in progress",
                    None,
                ));
            }
            inflight.insert(key.clone(), slot.clone());
        }
        debug!("MCP：调用工具 {}", call.tool.name());
        let server = self.clone();
        let callers = call.callers(self.callers.iter().cloned());
        // 做完（回答或被取消）之后不再发进度：发进度时在写出的锁内检查它
        let finished = Arc::new(AtomicBool::new(false));
        let (stop_ticker, stopped) = mpsc::channel::<()>();
        if let Some(progress) = call.progress() {
            let (server, finished) = (self.clone(), finished.clone());
            let started = Instant::now();
            std::thread::spawn(move || {
                while let Err(RecvTimeoutError::Timeout) =
                    stopped.recv_timeout(server.progress_every)
                {
                    let note = progress.notification(started.elapsed(), server.lang);
                    server.send_unless(&finished, &note);
                }
            });
        }
        let worker = std::thread::spawn(move || {
            let context = tools::Context {
                backend: &*server.backend,
                lang: server.lang,
                callers: &callers,
                slot: &slot,
            };
            let outcome = tools::call(call.tool, &call.arguments, &context);
            finished.store(true, Ordering::SeqCst);
            drop(stop_ticker);
            lock(&server.inflight).remove(&key);
            if slot.is_aborted() {
                debug!("MCP：工具 {} 已取消，不再回答", call.tool.name());
                return;
            }
            server.send(&call.reply(outcome));
        });
        let mut workers = lock(&self.workers);
        workers.retain(|worker| !worker.is_finished());
        workers.push(worker);
    }

    /// 通知：只处理取消（`notifications/initialized` 等不需要处理）。
    fn notify(&self, method: &str, params: Option<&Value>) {
        let Some(id) = protocol::cancelled_request(method, params) else {
            return;
        };
        let slot = lock(&self.inflight).get(&id).cloned();
        if let Some(slot) = slot {
            debug!("MCP：取消进行中的工具调用");
            slot.abort();
        }
    }

    /// 写一条消息（一行）。
    fn send(&self, message: &Value) {
        let out = lock(&self.out);
        Self::write_line(out, message);
    }

    /// `finished` 还没有置位时写一条消息（进度通知：回答之后不再发）。
    fn send_unless(&self, finished: &AtomicBool, message: &Value) {
        let out = lock(&self.out);
        if !finished.load(Ordering::SeqCst) {
            Self::write_line(out, message);
        }
    }

    fn write_line(mut out: MutexGuard<'_, Box<dyn Write + Send>>, message: &Value) {
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
