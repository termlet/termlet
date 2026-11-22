//! 服务的生命周期与连接：监听、握手、逐个处理请求；客户端断开或服务停止时取消进行中的请求。

use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use termlet_proto::endpoint::{Endpoint, unavailable_marker_text};
use termlet_proto::frame::{self, HEADER_LEN};
use termlet_proto::{
    ClientHello, ErrorBody, ErrorCode, Lang, MAX_REQUEST, Request, Response, Welcome, negotiate,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::error::tr;
use super::events::CliEvent;
use super::handler;
use super::http::{self, Http, HttpOptions, HttpStatus, TokenKind, TokenView};
use super::listen::{Conn, Listener};
use crate::audit::AuditLog;
use crate::paths::DataDir;
use crate::secret::SecretString;
use crate::store::Store;

/// 等待用户批准的最长时间。
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);

/// 握手的时限：连上后这么久不发握手消息的连接直接关闭。
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// 握手消息的上限。
const MAX_HELLO: usize = 64 << 10;
/// 超过上限的请求读出丢弃（再回答"请求过大"）的上限；更大的直接断开。
const MAX_DISCARD: usize = 64 << 20;
/// 交给界面的事件的队列长度。
const EVENT_QUEUE: usize = 64;

/// 服务的配置。
pub struct CliConfig {
    pub data_dir: DataDir,
    pub store: Arc<Store>,
    /// 与审计对话框共用同一份
    pub audit: Arc<AuditLog>,
    /// 等待批准的最长时间（测试中调短）
    pub approval_timeout: Duration,
    /// 开始时是否已锁定（应用锁的状态，之后用 [`CliService::set_app_locked`] 同步）：
    /// 开始监听的那一刻起就按它处理，启动时锁定的应用不会有放行的空档
    pub locked: bool,
    /// MCP 的 HTTP 接入的参数（测试中调短保活）
    pub(crate) http: HttpOptions,
}

impl CliConfig {
    pub fn new(data_dir: DataDir, store: Arc<Store>, audit: Arc<AuditLog>) -> Self {
        Self {
            data_dir,
            store,
            audit,
            approval_timeout: APPROVAL_TIMEOUT,
            locked: false,
            http: HttpOptions::default(),
        }
    }
}

/// 各连接共用的状态。
pub(super) struct Shared {
    pub store: Arc<Store>,
    pub audit: Arc<AuditLog>,
    pub events: async_channel::Sender<CliEvent>,
    /// 应用是否已锁定
    pub locked: watch::Sender<bool>,
    /// 服务正在停止
    pub shutdown: watch::Sender<bool>,
    pub approval_timeout: Duration,
    /// 正在处理的请求数
    in_flight: AtomicUsize,
    /// HTTP 上的 MCP 请求按哪种语言写说明（界面语言映射来的；命名管道上的请求用命令行工具握手时给的语言）
    english: AtomicBool,
    /// 最近一次请求的时间（命名管道上的请求、通过鉴权的 HTTP 请求）
    last_request: Mutex<Instant>,
}

impl Shared {
    pub(super) fn lang(&self) -> Lang {
        if self.english.load(Ordering::Relaxed) {
            Lang::En
        } else {
            Lang::Zh
        }
    }

    /// 记下收到了一个请求（无窗口模式据此计算空闲）。
    pub(super) fn touch(&self) {
        *self.last_request.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }
}

/// 运行中的命令行服务。丢弃时停止。
pub struct CliService {
    shared: Arc<Shared>,
    endpoint: Endpoint,
    accept: Option<JoinHandle<()>>,
    /// MCP 的 HTTP 接入（按设置启停）
    http: Http,
}

impl CliService {
    /// 开始监听（不阻塞，监听与请求都在核心的运行时上）。返回服务与界面接收事件的一端。
    ///
    /// 调用方应持有数据目录的单实例锁。监听失败时在 `run/cli.unavailable` 中写下原因（命令行工具据此说明）并返回错误，
    /// 应用照常运行；成功时删除上次留下的标记。
    pub fn start(config: CliConfig) -> io::Result<(Self, async_channel::Receiver<CliEvent>)> {
        let endpoint = Endpoint::for_data_dir(config.data_dir.root());
        let marker = endpoint.unavailable_marker();
        let listener = {
            let _guard = crate::runtime::runtime().enter();
            Listener::bind(&endpoint)
        };
        let listener = match listener {
            Ok(listener) => listener,
            Err(e) => {
                tracing::warn!(endpoint = %endpoint.display(), "监听命令行通道失败，termlet 命令行工具不可用：{e}");
                let text = unavailable_marker_text(std::process::id(), &e.to_string());
                if let Err(werr) = crate::platform::write_private_file(&marker, text.as_bytes()) {
                    tracing::warn!("写入命令行不可用的标记失败：{werr}");
                }
                return Err(e);
            }
        };
        let _ = std::fs::remove_file(&marker);
        let (events, receiver) = async_channel::bounded(EVENT_QUEUE);
        let shared = Arc::new(Shared {
            store: config.store,
            audit: config.audit,
            events,
            locked: watch::channel(config.locked).0,
            shutdown: watch::channel(false).0,
            approval_timeout: config.approval_timeout,
            in_flight: AtomicUsize::new(0),
            english: AtomicBool::new(false),
            last_request: Mutex::new(Instant::now()),
        });
        let accept = crate::runtime::spawn(accept_loop(shared.clone(), listener));
        tracing::info!(endpoint = %endpoint.display(), "命令行服务已启动");
        let http = Http::start(shared.clone(), config.http);
        Ok((
            Self {
                shared,
                endpoint,
                accept: Some(accept),
                http,
            },
            receiver,
        ))
    }

    /// 应用锁定、解锁时调用：锁定期间一律返回 `app_locked`，正在等待批准的请求立即结束。
    pub fn set_app_locked(&self, locked: bool) {
        self.shared.locked.send_replace(locked);
    }

    /// 正在处理的请求数（无窗口模式据此判断是否空闲：长时间的执行、传输进行中不退出）。
    pub fn in_flight(&self) -> usize {
        self.shared.in_flight.load(Ordering::SeqCst)
    }

    /// 最近一次请求的时间：命名管道上的请求与通过鉴权的 HTTP 请求（无窗口模式据此空闲退出）。
    pub fn last_request(&self) -> Instant {
        *self
            .shared
            .last_request
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// 界面语言变了时调用：HTTP 上的 MCP 请求的说明按它写（简体、繁体中文用中文，其余用英文；默认中文）。
    /// 命名管道上的请求仍按命令行工具自己的语言。
    pub fn set_language(&self, lang: Lang) {
        self.shared
            .english
            .store(lang == Lang::En, Ordering::Relaxed);
    }

    /// MCP 的 HTTP 接入实际的状态（正在监听的端口、没有监听、失败的原因）。
    pub fn http_status(&self) -> HttpStatus {
        self.http.status()
    }

    /// 订阅 [`Self::http_status`] 的变化（本机设置保存后自动启停、换端口，结果经这里送达）。
    pub fn subscribe_http(&self) -> watch::Receiver<HttpStatus> {
        self.http.subscribe()
    }

    /// 现在对照一次设置（例如端口被占用、对方让出后重试）；返回的任务完成时给出状态，在任何执行器上都能等。
    /// 保存本机设置后不用调用：服务自己会对照。
    pub fn refresh_http(&self) -> JoinHandle<HttpStatus> {
        self.http.refresh()
    }

    /// 界面显示、复制用的 HTTP 令牌（本机的或局域网的；读数据库与保险库，在后台执行器上调用）。
    pub fn http_token(&self, kind: TokenKind) -> crate::Result<TokenView> {
        http::token::view(&self.shared.store, kind)
    }

    /// 重新生成 HTTP 令牌（本机的或局域网的，另一个不变），立即生效（用旧令牌的 AI 工具随即被拒绝），返回新令牌。
    /// 保险库锁定时返回 [`crate::CoreError::VaultLocked`]。在后台执行器上调用。
    pub fn regenerate_http_token(&self, kind: TokenKind) -> crate::Result<SecretString> {
        http::token::regenerate(&self.shared.store, kind)
    }

    /// 监听的通道。
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// 停止：不再接受连接，进行中的请求被取消（远程命令收到 SIGTERM）。
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.shared.shutdown.send_replace(true);
        if let Some(accept) = self.accept.take() {
            accept.abort();
            self.http.stop();
        }
    }
}

impl Drop for CliService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl std::fmt::Debug for CliService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CliService")
            .field("endpoint", &self.endpoint.display())
            .finish_non_exhaustive()
    }
}

async fn accept_loop(shared: Arc<Shared>, mut listener: Listener) {
    let mut shutdown = shared.shutdown.subscribe();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(conn) => {
                    crate::runtime::spawn(serve(shared.clone(), conn));
                }
                Err(e) => {
                    tracing::warn!("接受命令行连接失败：{e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            // 只要结果的有无：`wait_for` 返回的读锁不能跨过其他分支中的 await
            _ = async { shutdown.wait_for(|s| *s).await.is_ok() } => break,
        }
    }
}

/// 一个正在处理的请求：丢弃时（处理完，或连接任务被取消）计数减一。
pub(super) struct InFlight<'a>(&'a Shared);

impl<'a> InFlight<'a> {
    pub(super) fn enter(shared: &'a Shared) -> Self {
        shared.in_flight.fetch_add(1, Ordering::SeqCst);
        Self(shared)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 请求的取消：客户端断开或服务停止。
#[derive(Clone)]
pub(super) struct Cancel {
    gone: watch::Receiver<bool>,
    shutdown: watch::Receiver<bool>,
}

impl Cancel {
    /// `gone`：请求的发起方已离开（连接断开、客户端取消）；`shutdown`：服务正在停止。
    pub(super) fn new(gone: watch::Receiver<bool>, shutdown: watch::Receiver<bool>) -> Self {
        Self { gone, shutdown }
    }

    /// 等到需要取消（发送端没有了也算）。
    pub async fn wait(&mut self) {
        tokio::select! {
            _ = async { self.gone.wait_for(|g| *g).await.is_ok() } => {}
            _ = async { self.shutdown.wait_for(|s| *s).await.is_ok() } => {}
        }
    }
}

/// 读帧的结果。
enum ReadError {
    /// 对方在帧的开头关闭了连接
    Closed,
    Io,
    /// 超过上限（已读出丢弃）
    TooLarge,
}

async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    max: usize,
) -> Result<Vec<u8>, ReadError> {
    let mut header = [0u8; HEADER_LEN];
    let mut filled = 0;
    while filled < HEADER_LEN {
        match reader.read(&mut header[filled..]).await {
            Ok(0) if filled == 0 => return Err(ReadError::Closed),
            Ok(0) | Err(_) => return Err(ReadError::Io),
            Ok(n) => filled += n,
        }
    }
    let len = frame::payload_len(header);
    if len > max {
        if len > MAX_DISCARD {
            return Err(ReadError::Io);
        }
        let mut limited = reader.take(len as u64);
        let drained = tokio::io::copy(&mut limited, &mut tokio::io::sink()).await;
        return match drained {
            Ok(n) if n == len as u64 => Err(ReadError::TooLarge),
            _ => Err(ReadError::Io),
        };
    }
    let mut payload = vec![0u8; len];
    reader
        .read_exact(&mut payload)
        .await
        .map_err(|_| ReadError::Io)?;
    Ok(payload)
}

async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, response: &Response) -> io::Result<()> {
    let bytes = frame::encode(response)?;
    writer.write_all(&bytes).await?;
    writer.flush().await
}

fn error_response(code: ErrorCode, message: String) -> Response {
    Response::Error(ErrorBody::new(code, message))
}

/// 一条连接：握手，然后逐个处理请求。读取在单独的任务中进行，客户端断开时立即取消进行中的请求。
async fn serve(shared: Arc<Shared>, conn: Box<dyn Conn>) {
    let (mut reader, mut writer) = tokio::io::split(conn);
    let hello = match tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut reader, MAX_HELLO)).await
    {
        Ok(Ok(payload)) => payload,
        _ => return,
    };
    let hello: ClientHello = match serde_json::from_slice(&hello) {
        Ok(hello) => hello,
        Err(e) => {
            let _ = write_frame(
                &mut writer,
                &error_response(ErrorCode::BadRequest, format!("握手消息无效：{e}")),
            )
            .await;
            return;
        }
    };
    let lang = hello.lang;
    let Some(protocol) = negotiate(hello.protocol) else {
        let message = format!(
            "{}（termlet {}，Termlet {}）",
            tr(
                lang,
                "命令行工具与 Termlet 的版本不兼容，请重新安装 Termlet",
                "the command-line tool is not compatible with this Termlet; reinstall Termlet"
            ),
            hello.protocol,
            termlet_proto::PROTOCOL_VERSION
        );
        let _ = write_frame(
            &mut writer,
            &error_response(ErrorCode::ProtocolMismatch, message),
        )
        .await;
        return;
    };
    let welcome = Response::Welcome(Welcome {
        protocol,
        app_version: crate::VERSION.to_string(),
    });
    if write_frame(&mut writer, &welcome).await.is_err() {
        return;
    }
    tracing::debug!(client = %hello.client, protocol, "命令行工具已连接");

    // 读取在单独的任务中进行：处理请求期间也能发现客户端断开
    let (frames, mut incoming) = mpsc::channel(4);
    let (gone_tx, gone) = watch::channel(false);
    let reader_task = crate::runtime::spawn(async move {
        loop {
            let frame = read_frame(&mut reader, MAX_REQUEST).await;
            let last = matches!(frame, Err(ReadError::Closed | ReadError::Io));
            if frames.send(frame).await.is_err() || last {
                break;
            }
        }
        gone_tx.send_replace(true);
    });
    let cancel = Cancel {
        gone,
        shutdown: shared.shutdown.subscribe(),
    };

    while let Some(frame) = incoming.recv().await {
        let response = match frame {
            Ok(payload) => match serde_json::from_slice::<Request>(&payload) {
                Ok(request) => {
                    shared.touch();
                    let _busy = InFlight::enter(&shared);
                    // 处理命令行工具的请求算作今天用过 Termlet（使用统计；命令行工具本身不发）
                    crate::usage::mark_active();
                    handler::handle(&shared, lang, request, cancel.clone()).await
                }
                Err(e) => error_response(
                    ErrorCode::BadRequest,
                    format!("{}：{e}", tr(lang, "请求无效", "invalid request")),
                ),
            },
            Err(ReadError::TooLarge) => error_response(
                ErrorCode::BadRequest,
                tr(
                    lang,
                    "请求过大（超过 2MB），未执行；较大的脚本请先 termlet upload 到服务器再执行",
                    "the request is too large (over 2MB) and was not run; upload larger scripts with termlet upload first",
                ),
            ),
            Err(ReadError::Closed | ReadError::Io) => break,
        };
        if write_frame(&mut writer, &response).await.is_err() {
            break;
        }
    }
    reader_task.abort();
}
