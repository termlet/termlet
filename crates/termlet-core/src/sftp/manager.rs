//! SFTP 管理器：全部会话与传输任务。
//!
//! 会话与传输属于某个窗口（`owner`，界面自己的标识）：窗口关闭时关闭它的会话、删除它已结束的传输记录。别的窗口还在
//! 用的会话（Tab 移到了别的窗口）由界面先交给那个窗口（[`SftpManager::reassign_owner`]、[`SftpManager::hand_over`]）。
//! 传输列表的变化经 [`SftpManager::watch_transfers`] 推送（合并到每 100 毫秒最多一次，传输进行中每 250 毫秒
//! 更新一次进度与速度）；一批传输结束、会话状态变化经 [`SftpManager::events`] 送出。

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, watch};

use super::SftpError;
use super::backend::Backend;
use super::client::{CHANNEL_CLOSED, SftpClient};
use super::error::NoSftp;
use super::scp::ScpRemote;
use super::session::{Link, Redial, SessionId, SessionInfo, SessionState, SftpSession};
use super::sudo::{self, SudoAsker, SudoAuth, SudoError};
use super::transfer::batch::{BatchBook, BatchReport};
use super::transfer::{Transfer, TransferView};
use crate::model::Encoding;
use crate::ssh::prompt::PromptFuture;
use crate::ssh::{
    Connection, ConnectionPool, Prompt, Prompter, SshError, TerminalSpec, connect, remembering,
};
use crate::store::Store;

/// 管理器的参数。
#[derive(Clone)]
pub struct ManagerOptions {
    /// 每个会话同时进行的传输数
    pub per_session: usize,
    /// 每个窗口保留的已结束传输记录数
    pub keep_finished: usize,
    /// 是否自动清除成功与已取消的传输记录（设置"传输完成后自动清除"），每次需要时调用；为空时不自动清除
    pub auto_clear: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// 自动清除时，成功或已取消的记录结束后还保留的时间（让用户看到结果）
    pub auto_clear_delay: Duration,
    /// 断线后自动重连：按 base、2×base……max 退避，最多持续 `reconnect_for`
    pub reconnect_base: Duration,
    pub reconnect_max: Duration,
    pub reconnect_for: Duration,
    /// 传输因断线失败后，等会话标记为正在重连的最长时间
    pub lost_wait: Duration,
    /// 关闭会话前，等它的传输删除临时文件的最长时间（连接没有响应时不再等）
    pub cleanup_wait: Duration,
}

impl Default for ManagerOptions {
    fn default() -> Self {
        Self {
            per_session: 3,
            keep_finished: 200,
            auto_clear: None,
            auto_clear_delay: Duration::from_secs(3),
            reconnect_base: Duration::from_secs(1),
            reconnect_max: Duration::from_secs(30),
            reconnect_for: Duration::from_secs(600),
            lost_wait: Duration::from_secs(2),
            cleanup_wait: Duration::from_secs(3),
        }
    }
}

impl ManagerOptions {
    /// 默认参数，自动清除按存储中的设置（`TerminalSettings::transfer_auto_clear`，每次需要时读取）。
    pub fn with_store(store: &Arc<Store>) -> Self {
        let store = store.clone();
        Self {
            auto_clear: Some(Arc::new(move || {
                store.settings().terminal.transfer_auto_clear
            })),
            ..Self::default()
        }
    }
}

impl fmt::Debug for ManagerOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ManagerOptions")
            .field("per_session", &self.per_session)
            .field("keep_finished", &self.keep_finished)
            .field("auto_clear", &self.auto_clear.is_some())
            .finish_non_exhaustive()
    }
}

/// 管理器送出的事件。
#[derive(Clone, Debug)]
pub enum SftpEvent {
    /// 会话的连接状态变了
    SessionChanged { id: SessionId, state: SessionState },
    /// 一台主机上的一批传输全部结束（成功、失败、取消，或已从列表中移除）：界面据此弹出汇总提示
    BatchEnded(BatchReport),
}

/// 打开经 SSH 的 SFTP 会话需要的信息。
pub struct SshTarget {
    pub store: Arc<Store>,
    /// 主机的标识：保存的主机用 `HostId`，快速连接等用界面自己的标识。同一台主机的会话可以互相继续传输
    pub host: String,
    /// 连接计划与选项（与终端相同，见 [`TerminalSpec::for_host`]）
    pub spec: TerminalSpec,
    /// 与终端共用连接：终端的连接组（`TerminalSession::group`）；为空时单独连接。
    /// "只用一个通道"的主机不能共用（返回 [`SftpError::SingleChannel`]）
    pub share_group: Option<String>,
    /// 确认指纹、输入密码与验证码。只在打开时询问；自动重连时只用这次输入过的密码，需要再问时放弃重连
    pub prompter: Prompter,
    /// 所属窗口
    pub owner: String,
    /// 以 root 身份打开：在 exec 通道上用 sudo 启动 sftp-server（代替 SFTP 子系统，见 `super::sudo`），
    /// sudo 要密码时经它询问（同样只在打开时；自动重连时用这次用过的密码）。为空时照常打开 SFTP 子系统。
    /// "只用一个通道"的主机不支持（[`SudoError::SingleChannel`]）
    pub sudo: Option<SudoAsker>,
    /// 服务器上文件名的编码（主机设置中的 `ssh.sftp_encoding`）：列表、传输、编辑、改名、删除都按它转换，
    /// 解码不了的字节保留原样（见 [`super::raw_byte`]）
    pub name_encoding: Encoding,
}

/// SFTP 管理器。可以克隆，克隆的是同一个管理器。
#[derive(Clone)]
pub struct SftpManager(pub(crate) Arc<Inner>);

pub(crate) struct Inner {
    /// 自己的弱引用（后台任务用）
    me: Weak<Inner>,
    pub(crate) options: ManagerOptions,
    pub(crate) sessions: Mutex<HashMap<SessionId, Arc<SftpSession>>>,
    pub(crate) transfers: Mutex<Vec<Arc<Transfer>>>,
    pub(crate) batches: Mutex<BatchBook>,
    snapshot: watch::Sender<Arc<[TransferView]>>,
    dirty: Arc<Notify>,
    publisher: AtomicBool,
    subscribers: Mutex<Vec<async_channel::Sender<SftpEvent>>>,
}

impl SftpManager {
    pub fn new(options: ManagerOptions) -> Self {
        Self(Arc::new_cyclic(|me| Inner {
            me: me.clone(),
            options,
            sessions: Mutex::default(),
            transfers: Mutex::default(),
            batches: Mutex::default(),
            snapshot: watch::channel(Arc::from(Vec::new())).0,
            dirty: Arc::new(Notify::new()),
            publisher: AtomicBool::new(false),
            subscribers: Mutex::default(),
        }))
    }

    /// 订阅事件（一批传输结束、会话状态变化）。每次调用得到一个独立的接收端，不再接收时丢弃即可。
    pub fn events(&self) -> async_channel::Receiver<SftpEvent> {
        let (tx, rx) = async_channel::unbounded();
        self.0.subscribers.lock().unwrap().push(tx);
        rx
    }

    /// 订阅传输列表（全部窗口的，按创建顺序）：有变化时收到新的快照。
    pub fn watch_transfers(&self) -> watch::Receiver<Arc<[TransferView]>> {
        self.0.ensure_publisher();
        self.0.snapshot.subscribe()
    }

    /// 经 SSH 打开 SFTP。共用终端的连接时不再认证；共用的连接上打不开 SFTP（例如超过服务器允许的会话数）时改用新连接。
    pub async fn open_ssh(&self, target: SshTarget) -> Result<Arc<SftpSession>, SftpError> {
        let SshTarget {
            store,
            host,
            spec,
            share_group,
            prompter,
            owner,
            sudo,
            name_encoding,
        } = target;
        if spec.single_channel && sudo.is_some() {
            return Err(SudoError::SingleChannel.into());
        }
        if spec.single_channel && share_group.is_some() {
            return Err(SftpError::SingleChannel);
        }
        // 打开之后不再询问：自动重连只用这次输入过的密码
        let interactive = Arc::new(AtomicBool::new(true));
        let gated: Prompter = {
            let interactive = interactive.clone();
            Arc::new(move |prompt: Prompt| -> PromptFuture {
                if interactive.load(Ordering::SeqCst) {
                    prompter(prompt)
                } else {
                    Box::pin(async { crate::ssh::Answer::Cancel })
                }
            })
        };
        let prompter = remembering(gated);
        let sudo = sudo.map(|asker| Arc::new(SudoAuth::new(asker, &spec, interactive.clone())));
        let spec = Arc::new(spec);
        let shared = share_group.is_some();
        let root = sudo.is_some();
        tracing::debug!(host = %host, owner = %owner, shared, root, "打开 SFTP 会话");
        let first_group = share_group.unwrap_or_else(ConnectionPool::new_group);
        let opener = Opener {
            store,
            spec,
            prompter,
            sudo,
            name_encoding,
        };
        let link = match opener.open(&first_group).await {
            Err(SftpError::Unavailable(reason)) if shared => {
                tracing::info!(host = %host, "共用的连接上无法打开 SFTP，改用新连接：{reason}");
                opener.open(&ConnectionPool::new_group()).await
            }
            other => other,
        }
        .inspect_err(|e| tracing::warn!(host = %host, "打开 SFTP 会话失败：{e}"))?;
        interactive.store(false, Ordering::SeqCst);
        // 重连时沿用这个连接组：同组的终端重连后共用同一条连接，只认证一次
        let group = link.group.clone().unwrap_or_default();
        let opener = Arc::new(opener);
        let redial: Redial = Arc::new(move || {
            let opener = opener.clone();
            let group = group.clone();
            Box::pin(async move {
                match opener.open(&group).await {
                    // 与终端共用连接时，需要询问（密码是在终端里输入的）就等终端重连好后共用它的连接，过一会儿再试
                    Err(SftpError::Connect(
                        SshError::AuthCancelled | SshError::InteractionRequired,
                    )) if shared => Err(SftpError::Disconnected),
                    other => other,
                }
            })
        });
        Ok(self.0.add(host, owner, link, Some(redial)).await)
    }

    /// 在已连上的字节流上打开 SFTP 会话（例如 WSL 发行版中的 sftp-server）；流结束时会话自动关闭，不重连。
    pub async fn open_stream<S>(
        &self,
        host: impl Into<String>,
        owner: impl Into<String>,
        stream: S,
    ) -> Result<Arc<SftpSession>, SftpError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let host = host.into();
        tracing::debug!(host = %host, "在字节流上打开 SFTP 会话");
        let client = SftpClient::connect(stream, Encoding::Utf8)
            .await
            .inspect_err(|e| tracing::warn!(host = %host, "打开 SFTP 会话失败：{e}"))?;
        let link = Link {
            backend: Backend::Sftp(Arc::new(client)),
            connection: None,
            group: None,
        };
        Ok(self.0.add(host, owner.into(), link, None).await)
    }

    /// 打开的会话。
    pub fn session(&self, id: &SessionId) -> Option<Arc<SftpSession>> {
        self.0.sessions.lock().unwrap().get(id).cloned()
    }

    /// 会话的摘要；`owner` 非空时只返回该窗口的。
    pub fn sessions(&self, owner: Option<&str>) -> Vec<SessionInfo> {
        let mut list: Vec<SessionInfo> = self
            .0
            .sessions
            .lock()
            .unwrap()
            .values()
            .filter(|s| owner.is_none_or(|o| s.owner() == o))
            .map(|s| s.info())
            .collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    /// 把会话交给另一个窗口（界面把 Tab 移到别的窗口时）：会话与它的传输记录改属 `owner`，原来的窗口关闭时不再
    /// 关闭、删除它们；还没结束的传输移进新窗口的一批，结束时的汇总送到新窗口。会话已关闭时什么也不做。
    pub fn reassign_owner(&self, id: &SessionId, owner: &str) {
        let Some(session) = self.session(id) else {
            return;
        };
        let previous = session.set_owner(owner);
        if previous == owner {
            return;
        }
        tracing::debug!(session = %id, from = %previous, to = %owner, "SFTP 会话换了所属窗口");
        self.0.reassign_transfers(id, &previous, owner);
    }

    /// 窗口 `from` 关闭时，别的窗口还在用的会话（移走的编辑器、文件管理的 Tab 用着它）交给那个窗口 `to`：只换会话的
    /// 所属窗口，会话不关闭；`from` 在这个会话上还没结束的传输照常随窗口关闭而取消（关闭前已确认过）。会话已经不属于
    /// `from` 时什么也不做。返回是否交了出去。
    pub fn hand_over(&self, id: &SessionId, from: &str, to: &str) -> bool {
        let Some(session) = self.session(id) else {
            return false;
        };
        if session.owner() != from {
            return false;
        }
        session.set_owner(to);
        let pending: Vec<_> = self
            .0
            .transfers
            .lock()
            .unwrap()
            .iter()
            .filter(|t| {
                let s = t.lock();
                s.session == *id && s.owner == from && !s.phase.finished()
            })
            .map(|t| t.id.clone())
            .collect();
        for transfer in &pending {
            let _ = self.cancel(transfer);
        }
        tracing::debug!(session = %id, from, to, canceled = pending.len(), "窗口关闭，SFTP 会话交给还在用它的窗口");
        true
    }

    /// 关闭会话，并取消它的全部传输。
    pub async fn close(&self, id: &SessionId) -> Result<(), SftpError> {
        if self.session(id).is_none() {
            return Err(SftpError::SessionClosed);
        }
        self.0.remove_session(id, None).await;
        Ok(())
    }

    /// 关闭某个窗口（为空时是全部窗口）的全部会话，返回关闭的个数。
    ///
    /// 窗口关闭后它的传输记录再也用不上，所以先删除这些已结束的记录与它们留下的临时文件，再关闭会话：
    /// 远程的临时文件要趁连接还在时删除。关闭会话时被取消的任务随后也删除记录；超过等待时间仍没结束的
    /// （连接没有响应）在结束时随即删除。
    pub async fn close_owner(&self, owner: Option<&str>) -> usize {
        let inner = &self.0;
        inner.clear_within(owner).await;
        let ids: Vec<SessionId> = self.sessions(owner).into_iter().map(|s| s.id).collect();
        for id in &ids {
            inner.remove_session(id, None).await;
        }
        inner.mark_orphans(owner);
        inner.clear_within(owner).await;
        ids.len()
    }

    /// 关闭全部会话（退出时）。
    pub async fn close_all(&self) {
        self.close_owner(None).await;
    }
}

impl Drop for Inner {
    /// 管理器不再被使用（没有调用 `close_all` 就丢弃了）：关闭全部会话，后台的等待随之结束。
    fn drop(&mut self) {
        for session in self.sessions.get_mut().unwrap().values() {
            session.closing.cancel();
            session.shutdown();
        }
    }
}

/// 在 SSH 连接上打开 SFTP 子系统（服务器上的文件名是 `encoding` 编码的）。
pub(crate) async fn open_subsystem(
    connection: &Connection,
    encoding: Encoding,
) -> Result<SftpClient, SftpError> {
    match request_subsystem(connection, encoding).await? {
        Subsystem::Opened(client) => Ok(client),
        Subsystem::Refused(reason) => Err(SftpError::Unavailable(reason)),
    }
}

/// 等服务器回答子系统请求的上限：没有回答（不守规矩的服务器）时照常协商版本。
const SUBSYSTEM_REPLY: Duration = Duration::from_secs(15);

/// 打开 SFTP 子系统的结果：服务器拒绝了子系统（没有开 sftp-server、堡垒机限制），或子系统一启动就退出了
/// （服务器上没有 sftp-server 程序）时单独报告，文件管理改用 SCP。
enum Subsystem {
    Opened(SftpClient),
    Refused(String),
}

async fn request_subsystem(
    connection: &Connection,
    encoding: Encoding,
) -> Result<Subsystem, SftpError> {
    let mut channel = connection
        .handle()
        .channel_open_session()
        .await
        .map_err(|e| SftpError::Unavailable(format!("打开通道失败：{e}")))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| SftpError::Unavailable(format!("请求 SFTP 子系统失败：{e}")))?;
    // 等服务器的回答：拒绝时不用等版本协商的超时
    let answer = tokio::time::timeout(SUBSYSTEM_REPLY, async {
        loop {
            match channel.wait().await {
                Some(russh::ChannelMsg::Success) => return true,
                Some(russh::ChannelMsg::Failure | russh::ChannelMsg::Close) | None => {
                    return false;
                }
                Some(_) => {}
            }
        }
    })
    .await;
    if answer == Ok(false) {
        let _ = channel.close().await;
        return Ok(Subsystem::Refused("服务器拒绝了 SFTP 子系统".into()));
    }
    match SftpClient::connect(channel.into_stream(), encoding).await {
        Ok(client) => Ok(Subsystem::Opened(client)),
        Err(SftpError::Unavailable(reason)) if reason == CHANNEL_CLOSED => {
            Ok(Subsystem::Refused(reason))
        }
        Err(error) => Err(error),
    }
}

/// 打开（与重连）经 SSH 的 SFTP 要用的东西。
struct Opener {
    store: Arc<Store>,
    spec: Arc<TerminalSpec>,
    prompter: Prompter,
    /// 以 root 身份打开
    sudo: Option<Arc<SudoAuth>>,
    name_encoding: Encoding,
}

impl Opener {
    /// 连接（或取连接组 `group` 里还能用的连接）并打开 SFTP。
    async fn open(&self, group: &str) -> Result<Link, SftpError> {
        open_link(
            &self.store,
            &self.spec,
            group,
            &self.prompter,
            self.sudo.as_deref(),
            self.name_encoding,
        )
        .await
    }
}

/// 连接（或取连接组里还能用的连接）并打开 SFTP（`sudo` 时以 root 身份）。"只用一个通道"的主机总是单独连接、
/// 不进连接池。
async fn open_link(
    store: &Arc<Store>,
    spec: &TerminalSpec,
    group: &str,
    prompter: &Prompter,
    sudo: Option<&SudoAuth>,
    encoding: Encoding,
) -> Result<Link, SftpError> {
    let connection = if spec.single_channel {
        connect(store.clone(), &spec.plan, prompter.clone(), &spec.connect).await?
    } else {
        ConnectionPool::global()
            .acquire(group, || {
                connect(store.clone(), &spec.plan, prompter.clone(), &spec.connect)
            })
            .await?
    };
    let backend = match sudo {
        Some(auth) => Backend::Sftp(Arc::new(sudo::open(&connection, auth, encoding).await?)),
        None => open_backend(&connection, encoding, spec.single_channel).await?,
    };
    Ok(Link {
        backend,
        connection: Some(connection),
        group: (!spec.single_channel).then(|| group.to_string()),
    })
}

/// 在 SSH 连接上打开 SFTP 子系统；服务器拒绝了子系统时改用 SCP 模式（见 `scp` 模块）。`single_channel` 的主机
/// 不能改用（SCP 模式的每个操作都要另开通道），返回 [`NoSftp::SingleChannel`]。
pub(crate) async fn open_backend(
    connection: &Arc<Connection>,
    encoding: Encoding,
    single_channel: bool,
) -> Result<Backend, SftpError> {
    match request_subsystem(connection, encoding).await? {
        Subsystem::Opened(client) => Ok(Backend::Sftp(Arc::new(client))),
        Subsystem::Refused(reason) => {
            tracing::info!(host = %connection.label(), "打不开 SFTP（{reason}），试试 SCP");
            if single_channel {
                return Err(SftpError::NoSftp(NoSftp::SingleChannel));
            }
            Ok(Backend::Scp(Arc::new(
                ScpRemote::open(connection.clone(), encoding).await?,
            )))
        }
    }
}

impl Inner {
    /// 登记会话：初始目录取服务端的当前目录（登录后的主目录）。
    pub(crate) async fn add(
        self: &Arc<Self>,
        host: String,
        owner: String,
        link: Link,
        redial: Option<Redial>,
    ) -> Arc<SftpSession> {
        let home = match &link.backend {
            Backend::Sftp(client) => match client.realpath(".").await {
                Ok(home) if !home.is_empty() => home,
                _ => "/".to_string(),
            },
            Backend::Scp(remote) => remote.home().to_string(),
        };
        let session = Arc::new(SftpSession::new(self, host, owner, home, link, redial));
        self.sessions
            .lock()
            .unwrap()
            .insert(session.id.clone(), session.clone());
        tracing::info!(id = %session.id, host = %session.host, "SFTP 会话已打开");
        let weak = Arc::downgrade(self);
        let watched = session.clone();
        crate::runtime::spawn(async move { watch_session(weak, watched).await });
        session
    }

    /// 关闭会话：先取消它的传输，等它们处理完（让出传输位置、删除临时文件）再关闭连接；连接没有响应时最多等 `cleanup_wait`。
    pub(crate) async fn remove_session(&self, id: &SessionId, error: Option<SftpError>) {
        let Some(session) = self.sessions.lock().unwrap().remove(id) else {
            return;
        };
        session.closing.cancel();
        let deadline = tokio::time::Instant::now() + self.options.cleanup_wait;
        while (session.slots.available_permits() < session.per_session || self.pending_on(id))
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        session.shutdown();
        session.set_state(SessionState::Closed { error });
        tracing::info!(id = %id, "SFTP 会话已关闭");
        self.mark_dirty();
    }

    /// 会话的状态变了：通知订阅者，传输列表中的"等待重连"随之变化。
    pub(crate) fn session_changed(&self, id: &SessionId, state: SessionState) {
        self.emit(SftpEvent::SessionChanged {
            id: id.clone(),
            state,
        });
        self.mark_dirty();
    }

    pub(crate) fn emit(&self, event: SftpEvent) {
        self.subscribers
            .lock()
            .unwrap()
            .retain(|tx| tx.try_send(event.clone()).is_ok());
    }

    /// 传输列表有变化：唤醒发布任务。
    pub(crate) fn mark_dirty(&self) {
        self.ensure_publisher();
        self.dirty.notify_one();
    }

    fn ensure_publisher(&self) {
        if self.publisher.swap(true, Ordering::SeqCst) {
            return;
        }
        crate::runtime::spawn(publish_loop(self.me.clone(), self.dirty.clone()));
    }

    pub(crate) fn auto_clear(&self) -> bool {
        self.options.auto_clear.as_ref().is_some_and(|f| f())
    }

    /// 会话是否正在自动重连。
    pub(crate) fn reconnecting(&self, id: &SessionId) -> bool {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|s| s.is_reconnecting())
    }

    /// 发布一次快照，推进各批传输。
    fn publish(&self) -> bool {
        let views = self.views(None, true);
        let running = views
            .iter()
            .any(|v| v.state == super::transfer::TransferState::Running);
        let ended = self
            .batches
            .lock()
            .unwrap()
            .advance(&views, crate::time::now_ms());
        self.snapshot.send_replace(Arc::from(views));
        for report in ended {
            self.emit(SftpEvent::BatchEnded(report));
        }
        running
    }
}

/// 发布任务：有变化时（最多每 100 毫秒一次）、传输进行中每 250 毫秒发布一次快照。只持有弱引用，管理器丢弃后结束。
async fn publish_loop(inner: Weak<Inner>, dirty: Arc<Notify>) {
    let mut running = false;
    loop {
        let wait = if running {
            Duration::from_millis(250)
        } else {
            Duration::from_secs(5)
        };
        let _ = tokio::time::timeout(wait, dirty.notified()).await;
        let Some(manager) = inner.upgrade() else {
            return;
        };
        running = manager.publish();
        drop(manager);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 等待连接断开：能自动重连的会话重连（会话 ID 不变），否则关闭会话。
async fn watch_session(manager: Weak<Inner>, session: Arc<SftpSession>) {
    loop {
        let backend = session.backend();
        tokio::select! {
            _ = backend.closed() => {}
            _ = session.closing.cancelled() => return,
        }
        drop(backend);
        if session.closing.is_canceled() {
            return;
        }
        let Some(redial) = session.redial.clone() else {
            if let Some(manager) = manager.upgrade() {
                manager
                    .remove_session(&session.id, Some(SftpError::Disconnected))
                    .await;
            }
            return;
        };
        let Some(options) = manager.upgrade().map(|m| m.options.clone()) else {
            return;
        };
        session.mark_reconnecting(1);
        tracing::info!(id = %session.id, "SFTP 连接已断开，正在重连");
        let deadline = tokio::time::Instant::now() + options.reconnect_for;
        let mut attempt: u32 = 1;
        loop {
            let delay = options
                .reconnect_base
                .checked_mul(1u32.checked_shl(attempt - 1).unwrap_or(u32::MAX))
                .unwrap_or(options.reconnect_max)
                .min(options.reconnect_max);
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = session.closing.cancelled() => return,
            }
            let result = match tokio::time::timeout(Duration::from_secs(60), redial()).await {
                Ok(result) => result,
                Err(_) => Err(SftpError::Disconnected),
            };
            match result {
                Ok(link) => {
                    if session.closing.is_canceled() {
                        link.backend.close();
                        return;
                    }
                    session.replace_link(link);
                    tracing::info!(id = %session.id, attempt, "SFTP 已重连");
                    // 先继续因断线失败的传输、再清除"正在重连"：两者之间读到的传输列表中，这些任务不会显示为失败
                    if let Some(manager) = manager.upgrade() {
                        manager.resume_after_reconnect(&session);
                    }
                    session.set_state(SessionState::Connected);
                    break;
                }
                Err(error) => {
                    if !error.is_retryable() || tokio::time::Instant::now() >= deadline {
                        tracing::info!(id = %session.id, "SFTP 重连失败，放弃：{error}");
                        if let Some(manager) = manager.upgrade() {
                            manager.remove_session(&session.id, Some(error)).await;
                        }
                        return;
                    }
                    tracing::info!(id = %session.id, attempt, "SFTP 重连失败，稍后重试：{error}");
                    attempt += 1;
                    session.mark_reconnecting(attempt);
                }
            }
        }
    }
}
