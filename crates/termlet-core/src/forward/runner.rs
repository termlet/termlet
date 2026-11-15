//! 一条运行中的转发：本机的监听（本地、动态转发）或服务器上的监听（远程转发）、SSH 连接、断线重连、每个连接的转发。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use futures::future::BoxFuture;
use russh::ChannelOpenFailure;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::manager::{ForwardOptions, ForwardState, ForwardStatus, Inner};
use super::pipe::pipe;
use super::socks::{self, Reply};
use super::{ForwardError, SocksError, TargetError, listen};
use crate::id::{ForwardId, HostId};
use crate::model::{Forward, ForwardKind, Host, join_host_port};
use crate::ssh::prompt::PromptFuture;
use crate::ssh::{
    Answer, Connection, ConnectionPool, ForwardedChannel, Prompt, Prompter, SshError, TerminalSpec,
    connect, remembering,
};
use crate::store::Store;

/// 取消服务器上的监听（cancel-tcpip-forward）最多等多久。
const CANCEL_TIMEOUT: Duration = Duration::from_secs(5);
/// 与终端共用连接、重连需要询问时，最多连续等终端重连几次（按退避约 3 分钟）。终端已经关闭时等不到，
/// 之后按“需要交互”停止，由用户重新启动，不无限重试。
const TERMINAL_WAIT_ATTEMPTS: u32 = 10;

/// 运行阶段。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Phase {
    Starting,
    Running,
    Reconnecting {
        attempt: u32,
        last_error: Option<ForwardError>,
    },
    Stopped,
}

/// 统计（实时累加）。
#[derive(Default)]
struct Stats {
    accepted: AtomicU64,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
    connections: AtomicUsize,
}

/// 建连所需的信息（启动时确定，重连时沿用；连接计划每次建连时按主机 ID 从存储重新读取）。
struct Dial {
    /// 连接组：重连时沿用，与终端共用连接时终端与转发重连后仍共用一条；"只用一个通道"的主机为空（不进连接池）
    group: Option<String>,
    /// 记住启动时输入过的密码；启动之后不再询问
    prompter: Prompter,
    /// 与终端共用连接
    shared: bool,
    /// 随连接结束而停（不重连）
    until_disconnect: bool,
}

/// 一条 SSH 连接上的转发。连接断开、被换掉或转发停止时 `lost` 变为真，其上的连接随之关闭。
pub(crate) struct Link {
    pub(crate) connection: Arc<Connection>,
    /// 远程转发在这条连接上的登记号
    route: Option<u64>,
    lost: watch::Sender<bool>,
}

struct State {
    phase: Phase,
    last_error: Option<TargetError>,
    /// 本机监听的接受任务：停止时等它结束（端口随之释放）
    tasks: Vec<JoinHandle<()>>,
    dial: Option<Arc<Dial>>,
}

pub(crate) struct Runner {
    pub(crate) id: ForwardId,
    /// 启动时的规则内容（规则被修改后据此停止）
    pub(crate) rule: Forward,
    /// 所属窗口：与终端共用连接的随那个终端换窗口（[`super::ForwardManager::reassign_group`]）
    owner: Mutex<String>,
    started_at: i64,
    manager: Weak<Inner>,
    store: Arc<Store>,
    options: ForwardOptions,
    stats: Stats,
    stop: watch::Sender<bool>,
    pub(crate) link: watch::Sender<Option<Arc<Link>>>,
    state: Mutex<State>,
}

/// 计入"当前连接数"，丢弃时减去。
struct Tracked<'a>(&'a Runner);

impl<'a> Tracked<'a> {
    fn new(runner: &'a Runner) -> Self {
        runner.stats.connections.fetch_add(1, Ordering::SeqCst);
        runner.mark_dirty();
        Self(runner)
    }
}

impl Drop for Tracked<'_> {
    fn drop(&mut self) {
        self.0.stats.connections.fetch_sub(1, Ordering::SeqCst);
        self.0.mark_dirty();
    }
}

impl Runner {
    pub(crate) fn new(
        manager: &Arc<Inner>,
        id: ForwardId,
        rule: Forward,
        owner: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            id,
            rule,
            owner: Mutex::new(owner),
            started_at: crate::time::now_ms(),
            manager: Arc::downgrade(manager),
            store: manager.store.clone(),
            options: manager.options.clone(),
            stats: Stats::default(),
            stop: watch::channel(false).0,
            link: watch::channel(None).0,
            state: Mutex::new(State {
                phase: Phase::Starting,
                last_error: None,
                tasks: Vec::new(),
                dial: None,
            }),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn owner(&self) -> String {
        self.owner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(crate) fn set_owner(&self, owner: &str) {
        *self.owner.lock().unwrap_or_else(|e| e.into_inner()) = owner.to_string();
    }

    /// 与终端共用连接时终端的连接组（不共用、还没建连时为 `None`）。
    pub(crate) fn shared_group(&self) -> Option<String> {
        let state = self.state();
        let dial = state.dial.as_ref()?;
        dial.shared.then(|| dial.group.clone()).flatten()
    }

    fn mark_dirty(&self) {
        if let Some(manager) = self.manager.upgrade() {
            manager.mark_dirty();
        }
    }

    fn is_stopped(&self) -> bool {
        *self.stop.borrow()
    }

    /// 等到转发停止。
    async fn stopped(&self) {
        let mut stop = self.stop.subscribe();
        let _ = stop.wait_for(|stopped| *stopped).await;
    }

    pub(crate) fn status(&self) -> ForwardStatus {
        let state = self.state();
        ForwardStatus {
            state: match &state.phase {
                Phase::Starting => ForwardState::Starting,
                Phase::Running => ForwardState::Running,
                Phase::Reconnecting {
                    attempt,
                    last_error,
                } => ForwardState::Reconnecting {
                    attempt: *attempt,
                    last_error: last_error.clone(),
                },
                Phase::Stopped => ForwardState::Stopped,
            },
            owner: Some(self.owner()),
            connections: self.stats.connections.load(Ordering::SeqCst),
            accepted: self.stats.accepted.load(Ordering::Relaxed),
            bytes_in: self.stats.bytes_in.load(Ordering::Relaxed),
            bytes_out: self.stats.bytes_out.load(Ordering::Relaxed),
            last_error: state.last_error.clone(),
            started_at: Some(self.started_at),
        }
    }

    fn set_last_error(&self, error: TargetError) {
        tracing::debug!(id = %self.id, "端口转发连接目标失败：{error}");
        self.state().last_error = Some(error);
        self.mark_dirty();
    }

    // ------------------------------------------------------------ 启动

    /// 开始转发：本地、动态转发先监听本地地址，再连接主机；远程转发连上后请服务器监听。
    pub(crate) async fn begin(
        self: &Arc<Self>,
        share_group: Option<String>,
        prompter: Prompter,
        until_disconnect: bool,
    ) -> Result<(), ForwardError> {
        tracing::debug!(
            id = %self.id,
            kind = ?self.rule.kind,
            bind = %self.rule.bind_address(),
            target = %join_host_port(&self.rule.target_host, self.rule.target_port),
            shared = share_group.is_some(),
            "启动端口转发"
        );
        let listener = match self.rule.kind {
            ForwardKind::Remote => None,
            _ => Some(
                listen::bind(&self.rule.bind_host, self.rule.bind_port).inspect_err(|e| {
                    tracing::warn!(id = %self.id, bind = %self.rule.bind_address(), "本机监听失败：{e}")
                })?,
            ),
        };
        let host_id = self.rule.host.clone().ok_or(ForwardError::HostNotFound)?;
        let host = self
            .store
            .get::<Host>(&host_id)
            .ok_or(ForwardError::HostNotFound)?;
        if !host.doc.is_ssh() {
            return Err(ForwardError::NotSsh);
        }
        let spec = load_spec(&self.store, &host_id).await?;
        let shared = share_group.is_some();
        if shared && spec.single_channel {
            return Err(ForwardError::SingleChannel);
        }
        // 启动之后不再询问：自动重连只用这次输入过的密码（发起启动的窗口可能已经不在了）
        let interactive = Arc::new(AtomicBool::new(true));
        let gated: Prompter = {
            let interactive = interactive.clone();
            Arc::new(move |prompt: Prompt| -> PromptFuture {
                if interactive.load(Ordering::SeqCst) {
                    prompter(prompt)
                } else {
                    Box::pin(async { Answer::Cancel })
                }
            })
        };
        let group =
            (!spec.single_channel).then(|| share_group.unwrap_or_else(ConnectionPool::new_group));
        let dial = Arc::new(Dial {
            group,
            prompter: remembering(gated),
            shared,
            until_disconnect,
        });
        self.state().dial = Some(dial.clone());

        // 用户在建连期间停止时取消建连（等待用户回答的询问随之结束）
        let connection = tokio::select! {
            result = open(&self.store, &spec, &dial) => result?,
            _ = self.stopped() => return Err(ForwardError::Stopped),
        };
        interactive.store(false, Ordering::SeqCst);
        self.connected(connection, listener).await
    }

    /// 切换到新的 SSH 连接并唤醒排队的连接；远程转发先请服务器监听。已停止时返回 [`ForwardError::Stopped`]。
    async fn connected(
        self: &Arc<Self>,
        connection: Arc<Connection>,
        listener: Option<TcpListener>,
    ) -> Result<(), ForwardError> {
        let mut forwarded = None;
        let mut route = None;
        if self.rule.kind == ForwardKind::Remote {
            let port = u32::from(self.rule.bind_port);
            let (id, receiver) = connection
                .remote_forwards()
                .register(&self.rule.bind_host, port);
            match connection
                .handle()
                .tcpip_forward(self.rule.bind_host.clone(), port)
                .await
            {
                Ok(_) => {
                    route = Some(id);
                    forwarded = Some(receiver);
                }
                Err(error) => {
                    connection.remote_forwards().unregister(id);
                    return Err(match error {
                        russh::Error::RequestDenied => ForwardError::RemoteListenDenied {
                            address: self.rule.bind_address(),
                        },
                        other => ForwardError::Connect(SshError::from(other)),
                    });
                }
            }
        }
        let link = Arc::new(Link {
            connection,
            route,
            lost: watch::channel(false).0,
        });
        let stopped = {
            let mut state = self.state();
            if state.phase == Phase::Stopped {
                true
            } else {
                state.phase = Phase::Running;
                if let Some(listener) = listener {
                    let runner = self.clone();
                    state
                        .tasks
                        .push(crate::runtime::spawn(runner.accept_loop(listener)));
                }
                self.link.send_replace(Some(link.clone()));
                false
            }
        };
        if stopped {
            self.release(link).await;
            return Err(ForwardError::Stopped);
        }
        if let Some(receiver) = forwarded {
            crate::runtime::spawn(self.clone().remote_accept(link.clone(), receiver));
        }
        crate::runtime::spawn(self.clone().watch_link(link));
        self.mark_dirty();
        Ok(())
    }

    // ------------------------------------------------------------ 断线重连

    /// 等待 SSH 连接断开，随后按退避重连（随连接结束而停的转发改为停止）。重连成功后由 [`Runner::connected`] 为新的
    /// 连接另起一个，两者互相引用，所以装箱（否则编译器推不出 future 是 `Send`）。
    fn watch_link(self: Arc<Self>, link: Arc<Link>) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            tokio::select! {
                _ = link.connection.closed() => {}
                _ = self.stopped() => return,
            }
            let (lost, until_disconnect) = {
                let mut state = self.state();
                let current = self
                    .link
                    .borrow()
                    .as_ref()
                    .is_some_and(|l| Arc::ptr_eq(l, &link));
                let lost = state.phase != Phase::Stopped && current;
                let until_disconnect = state.dial.as_ref().is_some_and(|d| d.until_disconnect);
                if lost && !until_disconnect {
                    state.phase = Phase::Reconnecting {
                        attempt: 1,
                        last_error: None,
                    };
                    self.link.send_replace(None);
                }
                (lost, until_disconnect)
            };
            if !lost {
                return;
            }
            if until_disconnect {
                drop(link);
                match self.manager.upgrade() {
                    Some(manager) => manager.ended(&self).await,
                    None => self.stop().await,
                }
                return;
            }
            // 断开的连接上的通道都已失效，关闭对应的连接，让客户端尽快感知
            link.lost.send_replace(true);
            if let Some(route) = link.route {
                link.connection.remote_forwards().unregister(route);
            }
            drop(link);
            tracing::info!(id = %self.id, "端口转发的 SSH 连接已断开，开始重连");
            self.mark_dirty();
            self.reconnect().await;
        })
    }

    /// 按退避间隔重连，直到成功、停止，或遇到重试也无法解决的错误（转为失败状态）。
    ///
    /// 重连时不询问：主机指纹在启动时已确认，密码用启动时输入过的；需要验证码等交互（且没有保存 TOTP 密钥）的
    /// 主机以 [`SshError::AuthCancelled`] 停止，由用户重新启动。与终端共用连接时，需要询问说明密码是在终端里输入的，
    /// 等终端重连好后共用它的连接，过一会儿再试；连续 [`TERMINAL_WAIT_ATTEMPTS`] 次都等不到（终端已关闭）时以
    /// [`SshError::InteractionRequired`] 停止。远程转发重连后服务器暂时不能监听（旧连接的监听还没释放）时继续重试。
    async fn reconnect(self: &Arc<Self>) {
        let Some(dial) = self.state().dial.clone() else {
            return;
        };
        let mut last_error: Option<ForwardError> = None;
        let mut terminal_waits = 0u32;
        for attempt in 1u32.. {
            {
                let mut state = self.state();
                if state.phase == Phase::Stopped {
                    return;
                }
                state.phase = Phase::Reconnecting {
                    attempt,
                    last_error: last_error.clone(),
                };
            }
            self.mark_dirty();
            tokio::select! {
                _ = tokio::time::sleep(self.options.backoff(attempt)) => {}
                _ = self.stopped() => return,
            }
            let attempt_once = async {
                let host = self.rule.host.clone().ok_or(ForwardError::HostNotFound)?;
                let spec = load_spec(&self.store, &host).await?;
                let target = spec.plan.target();
                let timeout = ForwardError::Connect(SshError::Timeout {
                    host: target.address.clone(),
                    port: target.port,
                });
                tokio::time::timeout(
                    self.options.reconnect_timeout,
                    open(&self.store, &spec, &dial),
                )
                .await
                .unwrap_or(Err(timeout))
            };
            let result = tokio::select! {
                result = attempt_once => result,
                _ = self.stopped() => return,
            };
            let error = match result {
                Ok(connection) => match self.connected(connection, None).await {
                    Ok(()) => {
                        tracing::info!(id = %self.id, attempt, "端口转发已重连");
                        return;
                    }
                    Err(ForwardError::Stopped) => return,
                    Err(error) => {
                        tracing::info!(id = %self.id, attempt, "端口转发重连后服务器暂时不能监听，稍后重试：{error}");
                        error
                    }
                },
                Err(error) => {
                    if self.is_stopped() {
                        return;
                    }
                    let waiting_for_terminal = dial.shared
                        && matches!(
                            error,
                            ForwardError::Connect(
                                SshError::AuthCancelled | SshError::InteractionRequired
                            )
                        );
                    let retryable = match &error {
                        ForwardError::Connect(e) => e.is_retryable(),
                        _ => false,
                    };
                    terminal_waits = if waiting_for_terminal {
                        terminal_waits + 1
                    } else {
                        0
                    };
                    let error = if terminal_waits > TERMINAL_WAIT_ATTEMPTS {
                        tracing::info!(id = %self.id, "共用连接的终端一直没有重连，转发停止");
                        ForwardError::Connect(SshError::InteractionRequired)
                    } else {
                        error
                    };
                    let waiting_for_terminal =
                        waiting_for_terminal && terminal_waits <= TERMINAL_WAIT_ATTEMPTS;
                    if !retryable && !waiting_for_terminal {
                        if let Some(manager) = self.manager.upgrade() {
                            manager.fail(self, error).await;
                        }
                        return;
                    }
                    tracing::info!(id = %self.id, attempt, "端口转发重连失败，稍后重试：{error}");
                    error
                }
            };
            last_error = Some(error);
        }
    }

    // ------------------------------------------------------------ 停止

    /// 只发出停止的信号（管理器被丢弃时，不能等待）。
    pub(crate) fn signal_stop(&self) {
        self.stop.send_replace(true);
    }

    /// 关闭监听、全部连接，释放 SSH 连接（与终端共用时只释放自己的那一份，连接仍保留）。可重复调用；
    /// 返回时本机的端口已释放、服务器上的监听已取消。
    pub(crate) async fn stop(&self) {
        let (tasks, link) = {
            let mut state = self.state();
            if state.phase == Phase::Stopped {
                return;
            }
            state.phase = Phase::Stopped;
            state.dial = None;
            (
                std::mem::take(&mut state.tasks),
                self.link.send_replace(None),
            )
        };
        self.stop.send_replace(true);
        for task in tasks {
            let _ = task.await;
        }
        if let Some(link) = link {
            self.release(link).await;
        }
    }

    /// 不再使用这条连接：关闭其上的连接；远程转发取消服务器上的监听。
    async fn release(&self, link: Arc<Link>) {
        link.lost.send_replace(true);
        let Some(route) = link.route else {
            return;
        };
        link.connection.remote_forwards().unregister(route);
        if link.connection.is_closed() {
            return;
        }
        let cancel = link
            .connection
            .handle()
            .cancel_tcpip_forward(self.rule.bind_host.clone(), u32::from(self.rule.bind_port));
        if let Ok(Err(error)) = tokio::time::timeout(CANCEL_TIMEOUT, cancel).await {
            tracing::debug!(id = %self.id, "取消服务器上的监听失败：{error}");
        }
    }

    // ------------------------------------------------------------ 本地、动态转发

    /// 接受本地连接，直到停止。
    async fn accept_loop(self: Arc<Self>, listener: TcpListener) {
        let mut delay = Duration::ZERO;
        loop {
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                _ = self.stopped() => return,
            };
            match accepted {
                Ok((stream, peer)) => {
                    delay = Duration::ZERO;
                    let _ = stream.set_nodelay(true);
                    crate::runtime::spawn(self.clone().serve_local(stream, peer));
                }
                Err(error) => {
                    // 临时错误（例如句柄耗尽）：短暂等待后继续，与 Go 的 net/http 相同
                    delay = (delay * 2).clamp(Duration::from_millis(5), Duration::from_secs(1));
                    tracing::warn!(id = %self.id, "端口转发接受连接失败：{error}");
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = self.stopped() => return,
                    }
                }
            }
        }
    }

    /// 为一个本地连接打开 direct-tcpip 通道并双向复制。动态转发先完成 SOCKS 握手，从中得到目标地址。
    async fn serve_local(self: Arc<Self>, mut stream: TcpStream, peer: SocketAddr) {
        self.stats.accepted.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(id = %self.id, %peer, "端口转发收到连接");
        let _tracked = Tracked::new(&self);
        tokio::select! {
            _ = self.forward_local(&mut stream, peer) => {}
            _ = self.stopped() => {}
        }
    }

    async fn forward_local(&self, stream: &mut TcpStream, peer: SocketAddr) {
        let (host, port, version) = if self.rule.kind == ForwardKind::Dynamic {
            // 握手期间（含等待重连）限时，避免不发数据的连接一直占着
            let limit = self.options.queue_timeout + self.options.channel_timeout;
            match tokio::time::timeout(limit, socks::handshake(stream)).await {
                Ok(Ok(request)) => (request.host, request.port, Some(request.version)),
                Ok(Err(error)) => return self.set_last_error(error.into()),
                Err(_) => return self.set_last_error(SocksError::Timeout.into()),
            }
        } else {
            (self.rule.target_host.clone(), self.rule.target_port, None)
        };
        let target = join_host_port(&host, port);
        // 拿到的 SSH 连接恰好在断开（例如主动断开后立即接入，断开还没处理完）：与重连期间接入的连接一样，
        // 等重连好的连接再试一次
        let mut retried = false;
        let (link, channel) = loop {
            let Some(link) = self.wait_link().await else {
                // 已停止，或重连超时：关闭连接
                if let Some(version) = version {
                    let _ = socks::reply(stream, version, Reply::Failure).await;
                }
                return;
            };
            let opening = link.connection.handle().channel_open_direct_tcpip(
                host.clone(),
                u32::from(port),
                peer.ip().to_string(),
                u32::from(peer.port()),
            );
            match tokio::time::timeout(self.options.channel_timeout, opening).await {
                Ok(Ok(channel)) => break (link, channel),
                Ok(Err(_)) if !retried && link.connection.is_closed() => retried = true,
                Ok(Err(error)) => {
                    let (reply, error) = channel_error(&target, error);
                    if let Some(version) = version {
                        let _ = socks::reply(stream, version, reply).await;
                    }
                    return self.set_last_error(error);
                }
                Err(_) => {
                    if let Some(version) = version {
                        let _ = socks::reply(stream, version, Reply::TimedOut).await;
                    }
                    return self.set_last_error(TargetError::Channel {
                        target,
                        reason: "超时".into(),
                    });
                }
            }
        };
        tracing::debug!(id = %self.id, %peer, target = %target, "端口转发连接已接通");
        let remote = channel.into_stream();
        if let Some(version) = version
            && socks::reply(stream, version, Reply::Succeeded)
                .await
                .is_err()
        {
            return;
        }
        let mut lost = link.lost.subscribe();
        tokio::select! {
            _ = pipe(stream, remote, &self.stats.bytes_out, &self.stats.bytes_in) => {}
            _ = lost.wait_for(|lost| *lost) => {}
        }
        tracing::debug!(id = %self.id, %peer, target = %target, "端口转发连接结束");
    }

    /// 可用的 SSH 连接（已断开、还没切到重连的不算）；重连期间最多等待 `queue_timeout`，已停止或超时返回 `None`。
    async fn wait_link(&self) -> Option<Arc<Link>> {
        let mut link = self.link.subscribe();
        let waiting = async {
            match link
                .wait_for(|current| current.as_ref().is_some_and(|l| !l.connection.is_closed()))
                .await
            {
                Ok(current) => current.clone(),
                Err(_) => None,
            }
        };
        tokio::select! {
            result = tokio::time::timeout(self.options.queue_timeout, waiting) => result.ok().flatten(),
            _ = self.stopped() => None,
        }
    }

    // ------------------------------------------------------------ 远程转发

    /// 接收服务器转来的连接，直到这条 SSH 连接不再使用。
    async fn remote_accept(
        self: Arc<Self>,
        link: Arc<Link>,
        mut forwarded: mpsc::UnboundedReceiver<ForwardedChannel>,
    ) {
        let mut lost = link.lost.subscribe();
        loop {
            let next = tokio::select! {
                next = forwarded.recv() => next,
                _ = lost.wait_for(|lost| *lost) => None,
                _ = self.stopped() => None,
            };
            let Some(channel) = next else {
                return;
            };
            crate::runtime::spawn(self.clone().serve_remote(link.clone(), channel));
        }
    }

    /// 处理服务器上接入的一个连接：在本机连接目标并双向复制。
    async fn serve_remote(self: Arc<Self>, link: Arc<Link>, forwarded: ForwardedChannel) {
        self.stats.accepted.fetch_add(1, Ordering::Relaxed);
        let _tracked = Tracked::new(&self);
        // 先转成流：没能连上目标时丢弃即关闭通道
        let remote = forwarded.channel.into_stream();
        tracing::debug!(
            id = %self.id,
            from = %join_host_port(&forwarded.originator, forwarded.originator_port as u16),
            "服务器转来一个连接"
        );
        let target = self.rule.target_address();
        let connecting =
            TcpStream::connect((self.rule.target_host.as_str(), self.rule.target_port));
        let local = tokio::select! {
            result = tokio::time::timeout(self.options.channel_timeout, connecting) => result,
            _ = self.stopped() => return,
        };
        let local = match local {
            Ok(Ok(local)) => local,
            Ok(Err(error)) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                return self.set_last_error(TargetError::LocalRefused { target });
            }
            Ok(Err(error)) => {
                return self.set_last_error(TargetError::Local {
                    target,
                    reason: error.to_string(),
                });
            }
            Err(_) => {
                return self.set_last_error(TargetError::Local {
                    target,
                    reason: "超时".into(),
                });
            }
        };
        let _ = local.set_nodelay(true);
        let mut lost = link.lost.subscribe();
        // 发往目标（本机的服务）的计入 bytes_out，从目标收到的计入 bytes_in，与本地转发一致
        tokio::select! {
            _ = pipe(remote, local, &self.stats.bytes_out, &self.stats.bytes_in) => {}
            _ = lost.wait_for(|lost| *lost) => {}
            _ = self.stopped() => {}
        }
    }
}

/// 建立 SSH 连接：取连接组里还能用的连接（与终端共用），没有时新建；"只用一个通道"的主机总是单独连接。
async fn open(
    store: &Arc<Store>,
    spec: &TerminalSpec,
    dial: &Dial,
) -> Result<Arc<Connection>, ForwardError> {
    let result = match &dial.group {
        None => {
            connect(
                store.clone(),
                &spec.plan,
                dial.prompter.clone(),
                &spec.connect,
            )
            .await
        }
        Some(group) => {
            ConnectionPool::global()
                .acquire(group, || {
                    connect(
                        store.clone(),
                        &spec.plan,
                        dial.prompter.clone(),
                        &spec.connect,
                    )
                })
                .await
        }
    };
    result.map_err(ForwardError::Connect)
}

/// 主机的连接计划与选项：要读出并解密保存的凭据（读数据库），放在阻塞线程上。
async fn load_spec(store: &Arc<Store>, host: &HostId) -> Result<TerminalSpec, ForwardError> {
    let (store, host) = (store.clone(), host.clone());
    match tokio::task::spawn_blocking(move || TerminalSpec::for_host(&store, &host)).await {
        Ok(result) => result.map_err(ForwardError::Connect),
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => Err(ForwardError::Connect(SshError::Vault(error.to_string()))),
    }
}

/// 打开 direct-tcpip 通道失败：给 SOCKS 客户端的回复与记下的原因。
fn channel_error(target: &str, error: russh::Error) -> (Reply, TargetError) {
    let target = target.to_string();
    match error {
        russh::Error::ChannelOpenFailure(ChannelOpenFailure::ConnectFailed) => {
            (Reply::HostUnreachable, TargetError::Unreachable { target })
        }
        russh::Error::ChannelOpenFailure(ChannelOpenFailure::AdministrativelyProhibited) => {
            (Reply::NotAllowed, TargetError::Prohibited { target })
        }
        other => (
            Reply::Failure,
            TargetError::Channel {
                target,
                reason: other.to_string(),
            },
        ),
    }
}
