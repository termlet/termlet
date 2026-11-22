//! MCP 的 HTTP 接入（Streamable HTTP）：应用自己在 `127.0.0.1` 上监听（监听范围为局域网时在 `0.0.0.0` 上），只有
//! `/mcp` 一个端点、只接受 POST。
//!
//! 协议的分发、工具的定义与结果的写法与 `termlet mcp`（stdio）共用 `termlet-mcp`；请求直接交给 [`super::handler`]，
//! 与命名管道共用同一个 [`Shared`]：外部 CLI 的开关、按主机授权、危险命令规则与批准、应用锁、审计都是同一套。
//!
//! # 开关与生命周期
//!
//! - 本机设置 `cli.http`（`enabled`，默认关；`port`，默认 47800，调试构建 47801；`scope`，默认只有本机）。只有
//!   “允许 AI 工具与脚本访问”（`cli.enabled`）与它**都开着**时才监听；功能关着时不开端口（命名管道不同：它一直监听、
//!   关着时回答 `cli_disabled`）。
//! - 本机设置有变化（保存设置、恢复备份）、保险库解锁时自动对照设置启停、换端口与监听范围（[`Http::refresh`] 也可以
//!   手动重试）；
//!   先绑定成功才算在监听（[`HttpStatus`]）。绑定失败按错误码区分端口被占用与没有权限 / 被系统保留，不自动换端口
//!   （AI 工具的配置中写着它），写运行日志。停止或换端口时，进行中的请求做完，空闲的连接关闭。
//! - 无窗口模式照常监听；通过鉴权的请求算作活动（[`super::CliService::last_request`]）。
//!
//! # 安全
//!
//! 接受连接时按对端地址分为本机（回环地址）与局域网（私有网段，见 [`lan`]，只在监听范围为局域网时）两种，别的地址来的
//! 连接在读请求之前关闭。每个请求在读正文之前依次检查：
//!
//! 1. `Host`：本机的连接必须是 `127.0.0.1:<端口>`、`localhost:<端口>` 或 `[::1]:<端口>`；局域网的连接必须是这条连接的
//!    本机地址或这台电脑的名字加上端口（回环的名字不认）。否则 403（防 DNS 重绑定）；
//! 2. 带 `Origin` 的一律 403：正常的 MCP 客户端不在浏览器中，浏览器中的网页一定带它；
//! 3. 路径不是 `/mcp` 时 404，方法不是 POST 时 405（`OPTIONS` 同样 405，任何回答都不带 CORS 的头）；
//! 4. `Authorization: Bearer <令牌>`：本机的连接只认本机的令牌，局域网的只认局域网的令牌（[`token::TokenKind`]），
//!    与保存的 SHA-256 摘要做常量时间比较，不对时 401（JSON 正文）；
//! 5. 正文最多 [`termlet_mcp::MAX_MESSAGE`]（8MB），同时最多 [`MAX_CONNECTIONS`] 条连接。
//!
//! 令牌见 [`token`]。被拒绝的请求在运行日志中记一条原因（同一原因每分钟最多一条；局域网来的带上对方的地址），不记
//! 内容与令牌。局域网中的客户端只有列出主机与执行命令两个工具（`termlet_mcp::Reach`）：传文件的工具读写的是这台电脑
//! 上的文件。局域网上是明文（没有 TLS），界面与文档写明只在信任的网络中使用。
//!
//! # 协议
//!
//! - 新协议（请求的 `_meta` 中带 2026-07-28）：无会话。`MCP-Protocol-Version` 头与 `_meta` 中的版本、`Mcp-Method`
//!   与正文的方法、`tools/call` 的 `Mcp-Name` 与工具名必须一致（缺少或不一致时 400，`-32020`）；不认识的方法 404。
//! - 旧协议（`initialize` 握手）：`initialize` 的回答头中下发 `MCP-Session-Id`。会话只记协议版本与客户端名，都写在
//!   ID 中（128 位随机数加上它们的 base32），不存在内存中：Termlet 重启后客户端拿旧 ID 来照样认得出。解不开的会话 ID
//!   不回 404，按没有会话处理：版本取 `MCP-Protocol-Version` 头，没有时按 2025-03-26，客户端名为空（客户端收到 404
//!   后不一定能恢复）。通知回 202。
//! - `tools/call` 回 `text/event-stream`：回答头马上发出，每 [`KEEPALIVE`] 保活一次，最后一条事件是结果。请求带
//!   `progressToken` 时保活发 `notifications/progress`（Claude Code 按 300 秒没有回答也没有进度中止调用，SSE 的注释
//!   不算），否则发一行 SSE 注释。客户端关闭这个流（保活写失败）即取消：远程命令收到 SIGTERM，审计记为已取消，
//!   不再回答。旧协议的
//!   `notifications/cancelled` 按（会话，请求 ID）取消；没有会话时只在请求 ID 唯一时取消。
//! - 其余请求回 `application/json`。不支持批量（JSON 数组）、HTTP/2 与 TLS。
//!
//! # 调用方
//!
//! `["mcp:<客户端名>", "http", <进程链…>]`：客户端名自报（旧协议取 `initialize`，新协议取每个请求的 `_meta`），只用于
//! 显示与审计；进程链按 TCP 对端查出发起连接的进程与它的祖先（[`peer`]，Windows 与 Linux，macOS 不记）。局域网的
//! 连接查不到那边的进程，写成 `["mcp:<客户端名>", "http", "lan:<对方的 IP>"]`。

pub mod lan;
mod peer;
mod serve;
mod state;
#[cfg(test)]
mod tests;
pub mod token;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::{Semaphore, broadcast, watch};
use tokio::task::JoinHandle;

use termlet_mcp::Reach;

use super::service::Shared;
use crate::forward::ForwardError;
use crate::model::HttpScope;
use crate::store::StoreEvent;

pub use token::{TokenKind, TokenView};

/// 同时最多的连接数：超出的连接回答 503 后关闭。
pub const MAX_CONNECTIONS: usize = 64;
/// `tools/call` 的 SSE 回答多久发一次保活。
pub const KEEPALIVE: Duration = Duration::from_secs(15);

/// HTTP 服务实际的状态（不是设置对话框中的草稿）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpStatus {
    /// 没有监听：设置中关着（或“允许 AI 工具与脚本访问”关着），或服务已停止
    Off,
    /// 正在监听：只有本机时在 `127.0.0.1:<port>` 上，局域网时在 `0.0.0.0:<port>` 上
    Listening { port: u16, scope: HttpScope },
    /// 要监听，但没能监听
    Failed {
        port: u16,
        scope: HttpScope,
        error: HttpError,
    },
}

/// 监听失败的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpError {
    /// 端口被别的程序占用（`AddrInUse`、`WSAEADDRINUSE`）
    PortInUse,
    /// 没有权限，或端口被系统保留（`WSAEACCES`、`EACCES`；Windows 上 Hyper-V、WSL 会保留一些端口范围）
    Denied,
    /// 端口为 0
    InvalidPort,
    /// 其他原因（系统的说明）
    Other(String),
}

/// 可调的参数（测试中调短保活）。
#[derive(Clone, Debug)]
pub(crate) struct HttpOptions {
    pub keepalive: Duration,
    pub max_connections: usize,
    /// 测试用，平时为空
    pub fake_lan: FakeLan,
}

impl Default for HttpOptions {
    fn default() -> Self {
        Self {
            keepalive: KEEPALIVE,
            max_connections: MAX_CONNECTIONS,
            fake_lan: FakeLan::default(),
        }
    }
}

/// 测试中不真的监听局域网（Windows 上监听 `0.0.0.0` 会在桌面上弹出防火墙的提示），把回环上的连接当作来自局域网。
#[derive(Clone, Debug, Default)]
pub(crate) struct FakeLan {
    /// 监听范围为局域网时实际监听的地址（测试中是 127.0.0.1）
    pub bind: Option<IpAddr>,
    /// 从这些回环地址来的连接当作来自对应的地址（测试的客户端以 127.0.0.x 作源地址）
    pub peers: Vec<(IpAddr, IpAddr)>,
    /// 换过对端的连接当作连到这个本机地址
    pub local: Option<IpAddr>,
}

impl FakeLan {
    /// 测试中换掉对端与本机地址（平时原样）。
    fn apply(&self, peer: SocketAddr, local: SocketAddr) -> (SocketAddr, SocketAddr) {
        match self.peers.iter().find(|(real, _)| *real == peer.ip()) {
            Some((_, fake)) => (
                SocketAddr::new(*fake, peer.port()),
                SocketAddr::new(self.local.unwrap_or(local.ip()), local.port()),
            ),
            None => (peer, local),
        }
    }
}

/// HTTP 服务：对照设置启停（由 [`super::CliService`] 持有）。
pub(super) struct Http {
    inner: Arc<Inner>,
    watcher: JoinHandle<()>,
}

pub(super) struct Inner {
    shared: Arc<Shared>,
    options: HttpOptions,
    status: watch::Sender<HttpStatus>,
    /// 正在监听的端口与它的停止开关；启停、换端口时持有，免得两次对照交错
    running: tokio::sync::Mutex<Option<Running>>,
    state: Arc<state::State>,
}

struct Running {
    port: u16,
    scope: HttpScope,
    stop: watch::Sender<bool>,
    accept: JoinHandle<()>,
}

impl Running {
    /// 停止监听并等监听的套接字关闭（同一端口随后可以立即重新监听）；已有的连接把进行中的请求做完再关闭。
    async fn stop(self) {
        self.stop.send_replace(true);
        let _ = self.accept.await;
    }
}

impl Http {
    /// 开始对照设置（在核心的运行时上）：现在对照一次，之后本机设置或保险库有变化时再对照。
    pub(super) fn start(shared: Arc<Shared>, options: HttpOptions) -> Self {
        // 先订阅再对照，免得漏掉中间的变化
        let events = shared.store.subscribe();
        let inner = Arc::new(Inner {
            shared,
            options,
            status: watch::channel(HttpStatus::Off).0,
            running: tokio::sync::Mutex::new(None),
            state: Arc::default(),
        });
        let watcher = crate::runtime::spawn(watch_settings(inner.clone(), events));
        Self { inner, watcher }
    }

    pub(super) fn status(&self) -> HttpStatus {
        self.inner.status.borrow().clone()
    }

    pub(super) fn subscribe(&self) -> watch::Receiver<HttpStatus> {
        self.inner.status.subscribe()
    }

    /// 现在对照一次设置（端口被占用后重试）；完成时给出状态。
    pub(super) fn refresh(&self) -> JoinHandle<HttpStatus> {
        let inner = self.inner.clone();
        crate::runtime::spawn(async move {
            inner.reconcile().await;
            inner.status.borrow().clone()
        })
    }

    /// 停止：不再监听（服务的停止开关已打开，进行中的请求随之取消）。
    pub(super) fn stop(&self) {
        self.watcher.abort();
        let inner = self.inner.clone();
        crate::runtime::spawn(async move {
            if let Some(running) = inner.running.lock().await.take() {
                running.stop().await;
            }
            inner.status.send_replace(HttpStatus::Off);
        });
    }
}

async fn watch_settings(inner: Arc<Inner>, mut events: broadcast::Receiver<StoreEvent>) {
    let mut shutdown = inner.shared.shutdown.subscribe();
    inner.reconcile().await;
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(StoreEvent::LocalSettingsChanged | StoreEvent::VaultChanged)
                | Err(broadcast::error::RecvError::Lagged(_)) => inner.reconcile().await,
                Ok(_) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = async { shutdown.wait_for(|s| *s).await.is_ok() } => break,
        }
    }
    if let Some(running) = inner.running.lock().await.take() {
        running.stop().await;
    }
    inner.status.send_replace(HttpStatus::Off);
}

impl Inner {
    /// 对照设置：该监听而没有监听（或端口、监听范围变了）时监听，不该监听时停止；该监听时没有令牌就生成一个
    /// （局域网的令牌在监听范围为局域网时才生成）。
    async fn reconcile(self: &Arc<Self>) {
        let mut running = self.running.lock().await;
        let cli = self.shared.store.local_settings().cli;
        let target = (cli.enabled && cli.http.enabled && !*self.shared.shutdown.borrow())
            .then_some((cli.http.port, cli.http.scope));
        if let Some((_, scope)) = target {
            let mut kinds = vec![TokenKind::Local];
            if scope == HttpScope::Lan {
                kinds.push(TokenKind::Lan);
            }
            for kind in kinds {
                let store = self.shared.store.clone();
                match tokio::task::spawn_blocking(move || token::ensure(&store, kind)).await {
                    Ok(Ok(true)) => tracing::info!(?kind, "已生成 MCP 的 HTTP 令牌"),
                    Ok(Ok(false)) => {}
                    Ok(Err(e)) => tracing::warn!(?kind, "生成 MCP 的 HTTP 令牌失败：{e}"),
                    Err(e) => tracing::warn!(?kind, "生成 MCP 的 HTTP 令牌失败：{e}"),
                }
            }
        }
        let current = running.as_ref().map(|r| (r.port, r.scope));
        let Some((port, scope)) = target else {
            if let Some(old) = running.take() {
                let port = old.port;
                old.stop().await;
                tracing::info!(port, "MCP 的 HTTP 服务已停止");
            }
            self.status.send_replace(HttpStatus::Off);
            return;
        };
        if current == Some((port, scope)) {
            return;
        }
        // 先停止旧的，再绑定新的：只换监听范围时端口相同，旧的不让出就绑不上（换端口失败时同样已停止旧的，状态与
        // 设置一致）
        if let Some(old) = running.take() {
            old.stop().await;
        }
        match bind(port, scope, &self.options.fake_lan) {
            Ok(listener) => {
                let (stop, stopped) = watch::channel(false);
                let accept = crate::runtime::spawn(accept_loop(
                    self.clone(),
                    listener,
                    port,
                    scope,
                    stopped,
                ));
                *running = Some(Running {
                    port,
                    scope,
                    stop,
                    accept,
                });
                match scope {
                    HttpScope::Local => {
                        tracing::info!(port, "MCP 的 HTTP 服务已在 127.0.0.1 上监听")
                    }
                    HttpScope::Lan => tracing::info!(
                        port,
                        "MCP 的 HTTP 服务已在所有 IPv4 网卡上监听（接受局域网的连接）"
                    ),
                }
                self.status
                    .send_replace(HttpStatus::Listening { port, scope });
            }
            Err(error) => {
                let failed = HttpStatus::Failed { port, scope, error };
                if *self.status.borrow() != failed {
                    tracing::warn!(port, "MCP 的 HTTP 服务没能监听：{failed:?}");
                }
                self.status.send_replace(failed);
            }
        }
    }
}

/// 只有本机时在 `127.0.0.1:<port>` 上监听，局域网时在 `0.0.0.0:<port>` 上（只有 IPv4：局域网中的客户端用 IPv4 地址）。
/// 与端口转发相同：Windows 上独占，Unix 上 `SO_REUSEADDR`。在核心的运行时上调用。
fn bind(port: u16, scope: HttpScope, fake: &FakeLan) -> Result<TcpListener, HttpError> {
    if port == 0 {
        return Err(HttpError::InvalidPort);
    }
    let address = match scope {
        HttpScope::Local => IpAddr::V4(Ipv4Addr::LOCALHOST),
        HttpScope::Lan => fake.bind.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
    };
    crate::forward::listen::bind(&address.to_string(), port).map_err(|e| match e {
        ForwardError::PortInUse { .. } => HttpError::PortInUse,
        ForwardError::ListenDenied { .. } => HttpError::Denied,
        other => HttpError::Other(other.to_string()),
    })
}

/// 接受连接，直到停止：按对端地址分为本机与局域网（只在监听范围为局域网时），别的连接不读请求、立即关闭；超过上限的
/// 连接回答 503 后关闭。
async fn accept_loop(
    inner: Arc<Inner>,
    listener: TcpListener,
    port: u16,
    scope: HttpScope,
    mut stop: watch::Receiver<bool>,
) {
    let permits = Arc::new(Semaphore::new(inner.options.max_connections));
    let mut shutdown = inner.shared.shutdown.subscribe();
    // 局域网的连接的 Host 可以写这台电脑的名字
    let names: Arc<[String]> = match scope {
        HttpScope::Local => Arc::new([]),
        HttpScope::Lan => lan::host_names().into(),
    };
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    let local = stream.local_addr().unwrap_or(SocketAddr::from(([127, 0, 0, 1], port)));
                    let (peer, local) = inner.options.fake_lan.apply(peer, local);
                    let reach = lan::reach_of(peer.ip())
                        .filter(|reach| *reach == Reach::Local || scope == HttpScope::Lan);
                    let Some(reach) = reach else {
                        inner.state.reject(state::Rejected::Peer, Some(peer.ip()));
                        drop(stream);
                        continue;
                    };
                    let from = (reach == Reach::Lan).then_some(peer.ip());
                    match permits.clone().try_acquire_owned() {
                        Ok(permit) => {
                            let conn = serve::Conn::new(peer, local, port, reach, names.clone());
                            let inner = inner.clone();
                            let stop = stop.clone();
                            crate::runtime::spawn(async move {
                                serve::connection(inner, stream, conn, stop).await;
                                drop(permit);
                            });
                        }
                        Err(_) => {
                            inner.state.reject(state::Rejected::TooMany, from);
                            crate::runtime::spawn(serve::busy(stream));
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("接受 MCP 的 HTTP 连接失败：{e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            _ = async { stop.wait_for(|s| *s).await.is_ok() } => break,
            _ = async { shutdown.wait_for(|s| *s).await.is_ok() } => break,
        }
    }
}
