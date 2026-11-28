//! SFTP 的 Tab：远程栏（工具栏、提示条、文件列表），够宽时左侧是本机栏，底部是这台主机的传输列表。
//!
//! - 从终端打开时共用终端的连接（不再认证），并可以跟随终端的目录：shell 报告目录（OSC 7）时立即切换，
//!   没有报告时每 2 秒向服务器查询一次（只在显示中）。用户自己换了目录就停止跟随。
//! - 连接断开后核心自动重连，期间工具栏与列表不可用，中断的传输重连后自动继续；重连放弃后可以手动重新连接，
//!   回到原来的目录。
//! - 窄于 45rem 时默认只有远程一栏，本机栏可以临时展开；够宽时是否显示本机栏、两栏的比例记在本机的界面状态中。

mod rebind;
mod remote;

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dock::{BasePanel, Panel, PanelEvent};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, Bounds, ClickEvent, Context, DragMoveEvent, Entity,
    EventEmitter, FocusHandle, Focusable, InteractiveElement as _, IntoElement, KeyDownEvent,
    ParentElement as _, Pixels, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Task, TestSupportExt as _, WeakEntity, Window, canvas, div,
    prelude::FluentBuilder as _, px, rems,
};
use termlet_core::id::HostId;
use termlet_core::launch::LaunchTarget;
use termlet_core::model::{Encoding, RemoteOs};
use termlet_core::quick::QuickTarget;
use termlet_core::sftp::{
    Entry, Protocol, SessionState, SftpError, SftpManager, SftpSession, SshTarget, TextFile,
};
use termlet_core::ssh::{Prompter, SshError, TerminalSpec};
use termlet_core::store::Store;

use super::errors;
use super::file_list::{self, DragSource, FileList, FileListEvent};
use super::list_model;
use super::local_pane::{LocalPane, LocalPaneEvent};
use super::transfers::{TransferList, TransferListEvent};
use crate::app_state::AppState;
use crate::brand_icons::{self, Brand};
use crate::terminal_panel::TerminalPanel;
use crate::truncated::Clip;
use crate::{ssh_errors, t};

/// 面板在持久化布局中的名称，发布后不能改。
const PANEL_NAME: &str = "SftpPanel";
const KEY_CONTEXT: &str = "SftpPanel";

/// 窄于这个宽度时默认只显示远程一栏。
const NARROW_WIDTH: f32 = 45.;
/// 远程栏窄于这个宽度时，工具栏收起“上传文件夹”“编辑”。
const COMPACT_TOOLBAR: f32 = 28.;
/// 没有 OSC 7 时查询终端当前目录的间隔。
const CWD_POLL: Duration = Duration::from_secs(2);
/// 本机栏的宽度比例的范围。
const RATIO_MIN: f32 = 0.2;
const RATIO_MAX: f32 = 0.8;

/// SFTP 连的主机。
#[derive(Clone, Debug)]
pub enum SftpTarget {
    Host(HostId),
    Quick(QuickTarget),
    /// WSL 发行版（`user` 为空时是默认用户）：在发行版中运行 sftp-server
    Wsl {
        distro: String,
        user: String,
    },
    /// 被堡垒机等外部程序调起的临时连接（WinSCP、Xftp 等的参数）
    Launch(Arc<LaunchTarget>),
    /// 以 root 身份打开的 SSH 主机（经 sudo 运行 sftp-server），见 [`SftpTarget::as_root`]
    Sudo(Box<SftpTarget>),
}

impl SftpTarget {
    /// 管理器中的主机标识：同一台主机的会话可以互相继续传输。以 root 身份打开的另算一台
    /// （root 建的文件普通用户改不了，编辑器也不能换到普通用户的会话上保存）。
    pub fn key(&self) -> String {
        match self {
            SftpTarget::Host(id) => id.as_str().to_string(),
            SftpTarget::Quick(target) => format!("quick:{}", target.display()),
            SftpTarget::Wsl { distro, user } => termlet_core::wsl::sftp_host_key(distro, user),
            SftpTarget::Launch(target) => format!("launch:{}", target.display_target()),
            SftpTarget::Sudo(target) => format!("{}#root", target.key()),
        }
    }

    pub fn host(&self) -> Option<&HostId> {
        match self {
            SftpTarget::Host(id) => Some(id),
            SftpTarget::Sudo(target) => target.host(),
            SftpTarget::Quick(_) | SftpTarget::Wsl { .. } | SftpTarget::Launch(_) => None,
        }
    }

    /// 以 root 身份打开同一台主机：SSH 主机经 sudo，WSL 以 root 用户。"只用一个通道"的主机（堡垒机）、
    /// 已经是 root 的没有。
    pub fn as_root(&self, store: &Store) -> Option<SftpTarget> {
        match self {
            SftpTarget::Host(_) | SftpTarget::Quick(_) => {
                let spec = self.spec(store).ok()?;
                (!spec.single_channel).then(|| SftpTarget::Sudo(Box::new(self.clone())))
            }
            SftpTarget::Wsl { distro, user } if user != "root" => Some(SftpTarget::Wsl {
                distro: distro.clone(),
                user: "root".into(),
            }),
            SftpTarget::Wsl { .. } | SftpTarget::Launch(_) | SftpTarget::Sudo(_) => None,
        }
    }

    /// Tab 上的标志：保存的 SSH 主机识别出的系统（与终端的 Tab、侧边栏相同），WSL 发行版按名称；其余的没有。
    fn brand(&self, store: &Store) -> Option<Brand> {
        match self {
            SftpTarget::Host(id) => store
                .get::<termlet_core::model::Host>(id)
                .and_then(|host| host.doc.remote_os())
                .and_then(brand_icons::for_os),
            SftpTarget::Wsl { distro, .. } => brand_icons::for_wsl(distro),
            SftpTarget::Sudo(target) => target.brand(store),
            SftpTarget::Quick(_) | SftpTarget::Launch(_) => None,
        }
    }

    /// 服务器上文件名的编码：保存的主机按主机设置，其余的用 UTF-8（解码不了的字节照样保留）。
    fn name_encoding(&self, store: &Store) -> Encoding {
        match self {
            SftpTarget::Host(id) => store
                .get::<termlet_core::model::Host>(id)
                .map(|host| host.doc.ssh.sftp_encoding)
                .unwrap_or_default(),
            SftpTarget::Sudo(target) => target.name_encoding(store),
            SftpTarget::Quick(_) | SftpTarget::Wsl { .. } | SftpTarget::Launch(_) => Encoding::Utf8,
        }
    }

    /// 服务器的地址（Tab 菜单的“复制 IP”）；WSL 没有。
    fn address(&self, store: &Store) -> Option<String> {
        match self {
            SftpTarget::Host(id) => store
                .get::<termlet_core::model::Host>(id)
                .map(|host| host.doc.address)
                .filter(|a| !a.is_empty()),
            SftpTarget::Quick(target) => Some(target.host.clone()),
            SftpTarget::Launch(target) => Some(target.address.clone()),
            SftpTarget::Sudo(target) => target.address(store),
            SftpTarget::Wsl { .. } => None,
        }
    }

    /// 连接目标（Tab 的悬停提示，见 [`crate::host_info::connection_target`]）：保存的主机按主机的设置，快速连接、
    /// 被调起的按连接的参数，WSL 为发行版（以 root 登录的写成 `root@发行版`）；经 sudo 以 root 身份打开的在后面
    /// 标上 root（与 Tab 上的标记相同）。
    pub(super) fn target_label(&self, store: &Store) -> Option<String> {
        use crate::host_info::{ConnectionTarget, connection_target};
        match self {
            SftpTarget::Host(id) => crate::host_info::host_target(store, id),
            SftpTarget::Quick(target) => connection_target(&ConnectionTarget::Ssh {
                username: &target.username,
                address: &target.host,
                port: target.port,
            }),
            SftpTarget::Launch(target) => connection_target(&ConnectionTarget::Ssh {
                username: &target.username,
                address: &target.address,
                port: target.port,
            }),
            SftpTarget::Wsl { distro, user } => {
                connection_target(&ConnectionTarget::Wsl { distro, user })
            }
            SftpTarget::Sudo(target) => target
                .target_label(store)
                .map(|label| format!("{label} · {}", t!("sftp.root_badge"))),
        }
    }

    /// 以 root 身份打开的（Tab 标题与工具栏上标出）。
    pub fn is_root(&self) -> bool {
        match self {
            SftpTarget::Sudo(_) => true,
            SftpTarget::Wsl { user, .. } => user == "root",
            SftpTarget::Host(_) | SftpTarget::Quick(_) | SftpTarget::Launch(_) => false,
        }
    }

    pub(crate) fn spec(&self, store: &Store) -> Result<TerminalSpec, SshError> {
        match self {
            SftpTarget::Host(id) => TerminalSpec::for_host(store, id),
            SftpTarget::Quick(target) => Ok(TerminalSpec::for_quick(store, target)),
            SftpTarget::Launch(target) => Ok(crate::launch::spec(store, target)),
            SftpTarget::Sudo(target) => target.spec(store),
            SftpTarget::Wsl { .. } => Err(SshError::Config("不是 SSH 主机".into())),
        }
    }
}

/// 用着的 SFTP 会话：面板与它打开的编辑器共用，最后一个放下时关闭会话（同时取消它的传输）。
pub struct SessionLease {
    session: Arc<SftpSession>,
    manager: SftpManager,
}

impl SessionLease {
    pub fn session(&self) -> &Arc<SftpSession> {
        &self.session
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        let manager = self.manager.clone();
        let id = self.session.id().clone();
        termlet_core::runtime::spawn(async move {
            if let Err(e) = manager.close(&id).await {
                tracing::info!("关闭 SFTP 会话 {id} 时出错：{e}");
            }
        });
    }
}

/// 正在打开的会话：面板在打开完成前被关闭（等待它的任务被丢弃）、或换成了新的一次打开时，
/// 打开完成后随即关闭它，不留在管理器中（也不占着连接）。
struct Opening<E: Send + 'static> {
    job: Option<tokio::task::JoinHandle<Result<Arc<SftpSession>, E>>>,
    manager: SftpManager,
}

impl<E: Send + 'static> Opening<E> {
    fn new(
        job: tokio::task::JoinHandle<Result<Arc<SftpSession>, E>>,
        manager: SftpManager,
    ) -> Self {
        Self {
            job: Some(job),
            manager,
        }
    }

    /// 等打开完成；等待期间被丢弃时由 `Drop` 收尾。
    async fn finish(mut self) -> Result<Result<Arc<SftpSession>, E>, tokio::task::JoinError> {
        let result = match self.job.as_mut() {
            Some(job) => job.await,
            None => unreachable!("打开只等一次"),
        };
        self.job = None;
        result
    }
}

impl<E: Send + 'static> Drop for Opening<E> {
    fn drop(&mut self) {
        let Some(job) = self.job.take() else {
            return;
        };
        let manager = self.manager.clone();
        termlet_core::runtime::spawn(async move {
            if let Ok(Ok(session)) = job.await {
                close_session(&manager, &session).await;
            }
        });
    }
}

/// 关闭没有人用的会话（打开完成时面板已经不在）。
async fn close_session(manager: &SftpManager, session: &SftpSession) {
    let id = session.id().clone();
    if let Err(e) = manager.close(&id).await {
        tracing::info!("关闭 SFTP 会话 {id} 时出错：{e}");
    }
}

/// 打开完成时面板已经不在（`update` 没有执行）：关闭这个会话。
fn discard_unused(manager: &SftpManager, session: Arc<SftpSession>) {
    let manager = manager.clone();
    termlet_core::runtime::spawn(async move { close_session(&manager, &session).await });
}

/// SFTP 面板发给工作区的请求。
pub enum SftpPanelEvent {
    /// 关闭这个 Tab（已确认过）
    Close,
    /// 在编辑器中打开读取到的远程文件
    OpenEditor {
        lease: Rc<SessionLease>,
        file: TextFile,
    },
    /// 以 root 身份打开同一台主机的 SFTP（新 Tab；跟随着终端时同样跟随它）
    OpenAsRoot,
    /// 切到这个终端（“在终端中查找”写进命令之后）
    ShowTerminal(Entity<TerminalPanel>),
}

/// 连接的状态。
#[derive(Clone, Debug, PartialEq)]
enum Status {
    Connecting,
    Ready,
    /// 连接断开，核心正在自动重连
    Reconnecting,
    /// 已断开（重连放弃或会话已关闭）：可以手动重新连接
    Lost,
    /// 打开失败
    Failed(SharedString),
}

/// 跟随的终端。
struct Link {
    terminal: WeakEntity<TerminalPanel>,
    follow: bool,
    /// 终端的当前目录；暂时不知道时为 `None`
    cwd: Option<String>,
    /// shell 报告过目录（OSC 7）：不再查询
    reported: bool,
    /// 上一次跟过去的终端目录：终端的目录变了才再跟（打不开的不反复重试；路径写法与列出的不同时也不反复加载）
    followed: Option<String>,
    _observe: Subscription,
    _release: Subscription,
}

/// 拖动两栏之间的分隔条。
struct SplitterDrag;

impl Render for SplitterDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

pub struct SftpPanel {
    name: SharedString,
    target: SftpTarget,
    /// 所属窗口（会话与传输按窗口区分）
    owner: String,
    store: Arc<Store>,
    prompter: Prompter,
    /// 下一次连接的询问改在别的窗口弹出（编辑器在另一个窗口中要重新连接时，见 [`Self::ensure_connected`]）
    ask_once: Option<Prompter>,
    manager: SftpManager,
    lease: Option<Rc<SessionLease>>,
    status: Status,
    link: Option<Link>,
    /// 显示中的目录（解析后的绝对路径）；还没加载过时为空
    dir: String,
    /// 列表中的条目（权限、链接等列表行没有的信息）
    entries: Vec<Entry>,
    path: Entity<InputState>,
    list: Entity<FileList>,
    loading: bool,
    /// 正在读取要编辑的文件
    reading: bool,
    /// 每次加载的编号：只采用最后一次的结果
    seq: u64,
    error: Option<SharedString>,
    /// 关掉了 SCP 模式的说明条（这个 Tab 内不再显示）
    scp_notice_closed: bool,
    local: Entity<LocalPane>,
    transfers: Entity<TransferList>,
    focus: FocusHandle,
    splitter_focus: FocusHandle,
    active: bool,
    /// 上一帧的位置（判断宽窄、是否在显示）与远程栏的宽度（工具栏收起）
    bounds: Rc<Cell<Bounds<Pixels>>>,
    remote_width: Rc<Cell<Pixels>>,
    /// 窄的窗格中临时展开了本机栏
    narrow_open: bool,
    _connect: Option<Task<()>>,
    /// 以 root 身份打开时回答 sudo 的密码询问
    _sudo: Option<Task<()>>,
    /// 上面那个任务读的询问队列与这台主机保存的登录密码：Tab 移到别的窗口时在新窗口中接着回答
    sudo_answers: Option<(
        async_channel::Receiver<super::sudo_dialog::SudoRequest>,
        Option<termlet_core::secret::SecretString>,
    )>,
    _state: Option<Task<()>>,
    _load: Option<Task<()>>,
    _read: Option<Task<()>>,
    _poll: Option<Task<()>>,
    _cwd_now: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PanelEvent> for SftpPanel {}
impl EventEmitter<SftpPanelEvent> for SftpPanel {}

impl Focusable for SftpPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.list.focus_handle(cx)
    }
}

/// 窗口的标识：会话与传输记录属于窗口。
pub fn window_owner(window: &Window) -> String {
    format!("window-{}", window.window_handle().window_id().as_u64())
}

impl SftpPanel {
    /// 打开 SFTP。`terminal` 为从终端打开时的终端：共用它的连接，并跟随它的目录。
    pub fn open(
        name: SharedString,
        target: SftpTarget,
        terminal: Option<Entity<TerminalPanel>>,
        prompter: Prompter,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let owner = window_owner(window);
            let path = cx.new(|cx| InputState::new(window, cx));
            let panel_id = cx.entity_id();
            let list = cx.new(|cx| {
                FileList::new(
                    "remote-list",
                    DragSource::Remote(panel_id),
                    t!("sftp.empty"),
                    cx,
                )
            });
            let local = cx.new(|cx| LocalPane::new(panel_id, window, cx));
            let transfers = cx.new(|cx| TransferList::new(owner.clone(), target.key(), window, cx));
            let weak: WeakEntity<Self> = cx.entity().downgrade();
            let find = weak.clone();
            list.update(cx, |list, _| {
                list.set_menu(move |menu, window, cx| match weak.upgrade() {
                    Some(panel) => panel.read(cx).build_menu(menu, &panel, window, cx),
                    None => menu,
                });
                list.set_find_in_terminal(move |query, cx| {
                    find.upgrade()
                        .is_some_and(|panel| panel.read(cx).find_command(query, cx).is_some())
                });
            });
            let subscriptions =
                Self::subscribe_children(&list, &local, &transfers, &path, window, cx);
            let mut panel = Self {
                name,
                target,
                owner,
                store: AppState::store(cx),
                prompter,
                ask_once: None,
                manager: AppState::sftp(cx),
                lease: None,
                status: Status::Connecting,
                link: None,
                dir: String::new(),
                entries: Vec::new(),
                path,
                list,
                loading: false,
                reading: false,
                seq: 0,
                error: None,
                scp_notice_closed: false,
                local,
                transfers,
                focus: cx.focus_handle(),
                splitter_focus: cx.focus_handle(),
                active: false,
                bounds: Default::default(),
                remote_width: Rc::new(Cell::new(px(0.))),
                narrow_open: false,
                _connect: None,
                _sudo: None,
                sudo_answers: None,
                _state: None,
                _load: None,
                _read: None,
                _poll: None,
                _cwd_now: None,
                _subscriptions: subscriptions,
            };
            if let Some(terminal) = terminal {
                panel.link_terminal(&terminal, window, cx);
            }
            panel.connect(window, cx);
            panel
        })
    }

    pub fn name(&self) -> &SharedString {
        &self.name
    }

    /// 从 `path` 开始（主机工具中 Docker 的绑定挂载）：不再跟随终端的目录；还没连上时连上后打开它。
    pub fn start_at(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(link) = &mut self.link {
            link.follow = false;
        }
        if self.ready() {
            self.load(path, true, window, cx);
        } else {
            self.dir = path;
        }
        cx.notify();
    }

    pub fn target(&self) -> &SftpTarget {
        &self.target
    }

    /// 跟随的终端（还在时）。
    pub fn linked_terminal(&self) -> Option<Entity<TerminalPanel>> {
        self.link.as_ref().and_then(|l| l.terminal.upgrade())
    }

    /// 现在的会话（编辑器断开后重新取用）；没有连接着时为 `None`。
    pub fn current_lease(&self) -> Option<Rc<SessionLease>> {
        self.ready().then(|| self.lease.clone()).flatten()
    }

    /// 已断开（重连放弃、打开失败）时重新连接；编辑器要保存时用。询问（密码、指纹、sudo 的密码）在 `window`（编辑器
    /// 所在的窗口，Tab 移到了别的窗口时不是这个面板的）中弹出。
    pub fn ensure_connected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.status, Status::Lost | Status::Failed(_)) {
            self.ask_once = Some(AppState::prompter_for(window, cx));
            self.reconnect(window, cx);
        }
    }

    /// 现在的会话的标识（窗口关闭时把还在用的会话交给别的窗口，见 `workspace.rs`）。
    pub(crate) fn session_id(&self) -> Option<termlet_core::sftp::SessionId> {
        self.session().map(|session| session.id().clone())
    }

    /// 正在连接（打开、重新连接中）。
    pub fn is_connecting(&self) -> bool {
        self.status == Status::Connecting
    }

    /// 这个 Tab 的会话上未完成的传输数（关闭前确认）。
    pub fn pending_transfers(&self, cx: &App) -> usize {
        self.session_transfers(cx).len()
    }

    fn session_transfers(&self, cx: &App) -> Vec<termlet_core::sftp::TransferId> {
        match self.session() {
            Some(session) => self.transfers.read(cx).pending_on(session.id()),
            None => Vec::new(),
        }
    }

    /// 关闭这个 Tab：先取消会话上未完成的传输。同一台主机的编辑器还用着这个会话时会话不随 Tab 关闭，
    /// 传输不能留在后台继续（之后编辑器关闭时又被悄悄取消），与关闭前确认的“将中断”一致。
    pub(crate) fn close_tab(&mut self, cx: &mut Context<Self>) {
        let pending = self.session_transfers(cx);
        tracing::info!(sftp = %self.name, pending = pending.len(), "关闭 SFTP");
        for id in pending {
            if let Err(e) = self.manager.cancel(&id) {
                tracing::info!(sftp = %self.name, "关闭 Tab 时取消传输 {id} 失败：{e}");
            }
        }
        cx.emit(SftpPanelEvent::Close);
    }

    /// 关闭前要不要确认：连接着（或正在连接、重连），并且设置中开启了确认。
    pub fn needs_close_confirm(&self) -> bool {
        self.is_live() && self.store.settings().general.confirm_close
    }

    /// 连接着（或正在连接、重连）：关闭会断开。
    fn is_live(&self) -> bool {
        matches!(
            self.status,
            Status::Connecting | Status::Ready | Status::Reconnecting
        )
    }

    fn ready(&self) -> bool {
        self.status == Status::Ready
    }

    fn session(&self) -> Option<Arc<SftpSession>> {
        self.lease.as_ref().map(|lease| lease.session.clone())
    }

    /// 服务器没有 SFTP，改用了 SCP（没有断点续传，不能修改权限）。
    pub(crate) fn is_scp(&self) -> bool {
        self.lease
            .as_ref()
            .is_some_and(|lease| lease.session.protocol() == Protocol::Scp)
    }

    // ------------------------------------------------------------ 连接

    /// 打开会话：有跟随的终端时先试共用它的连接，“只用一个通道”的主机改为单独连接。
    fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let SftpTarget::Wsl { distro, user } = self.target.clone() {
            self.connect_wsl(distro, user, window, cx);
            return;
        }
        let spec = match self.target.spec(&self.store) {
            Ok(spec) => spec,
            Err(error) => {
                tracing::warn!(sftp = %self.name, "SFTP 的连接参数有误：{error}");
                self.status = Status::Failed(ssh_errors::describe(&error));
                cx.notify();
                return;
            }
        };
        let share = self
            .linked_terminal()
            .and_then(|terminal| terminal.read(cx).share_group());
        let root = self.target.is_root();
        tracing::debug!(sftp = %self.name, host = %self.target.key(), share = share.is_some(), root, "打开 SFTP 会话");
        self.status = Status::Connecting;
        self.error = None;
        let manager = self.manager.clone();
        let store = self.store.clone();
        let host = self.target.key();
        let prompter = self
            .ask_once
            .take()
            .unwrap_or_else(|| self.prompter.clone());
        let owner = self.owner.clone();
        let sudo = root.then(|| self.sudo_asker(&spec, window, cx));
        let name_encoding = self.target.name_encoding(&self.store);
        let job = termlet_core::runtime::spawn(async move {
            let target = |share_group: Option<String>| SshTarget {
                store: store.clone(),
                host: host.clone(),
                spec: spec.clone(),
                share_group,
                prompter: prompter.clone(),
                owner: owner.clone(),
                sudo: sudo.clone(),
                name_encoding,
            };
            match manager.open_ssh(target(share.clone())).await {
                Err(SftpError::SingleChannel) if share.is_some() => {
                    manager.open_ssh(target(None)).await
                }
                other => other,
            }
        });
        let opening = Opening::new(job, self.manager.clone());
        let manager = self.manager.clone();
        self._connect = Some(cx.spawn_in(window, async move |this, cx| {
            let mut result = Some(match opening.finish().await {
                Ok(result) => result,
                Err(e) => Err(SftpError::Unavailable(format!("连接任务异常结束：{e}"))),
            });
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(result) = result.take() else {
                    return;
                };
                this._connect = None;
                match result {
                    Ok(session) => this.on_connected(session, window, cx),
                    Err(error) => {
                        tracing::warn!(sftp = %this.name, "打开 SFTP 失败：{error}");
                        this.status = Status::Failed(errors::describe(&error));
                        this.sync_children(cx);
                    }
                }
                cx.notify();
            });
            if let Some(Ok(session)) = result {
                discard_unused(&manager, session);
            }
        }));
        cx.notify();
    }

    /// 以 root 身份打开时回答 sudo 的密码询问：在这个窗口中逐个弹出对话框（保存了登录密码时可以一键使用）。
    fn sudo_asker(
        &mut self,
        spec: &TerminalSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> termlet_core::sftp::SudoAsker {
        let (queue, requests) = async_channel::unbounded();
        let saved = spec.plan.target().saved_password().cloned();
        self.sudo_answers = Some((requests.clone(), saved.clone()));
        self._sudo = Some(Self::answer_sudo(requests, saved, window, cx));
        super::sudo_dialog::asker(queue)
    }

    /// WSL 的文件管理：在发行版中运行 sftp-server（没有安装时说明安装命令）。
    fn connect_wsl(
        &mut self,
        distro: String,
        user: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.status = Status::Connecting;
        self.error = None;
        tracing::debug!(sftp = %self.name, distro = %distro, user = %user, "打开 WSL 的文件管理");
        let manager = self.manager.clone();
        let owner = self.owner.clone();
        let job = termlet_core::runtime::spawn(async move {
            termlet_core::wsl::open_sftp(&manager, &distro, &user, &owner, None).await
        });
        let opening = Opening::new(job, self.manager.clone());
        let manager = self.manager.clone();
        self._connect = Some(cx.spawn_in(window, async move |this, cx| {
            let mut result = Some(opening.finish().await);
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(result) = result.take() else {
                    return;
                };
                this._connect = None;
                match result {
                    Ok(Ok(session)) => this.on_connected(session, window, cx),
                    Ok(Err(error)) => {
                        tracing::warn!(sftp = %this.name, "打开 WSL 的文件管理失败：{error}");
                        this.status = Status::Failed(crate::wsl_errors::describe(&error));
                        this.sync_children(cx);
                    }
                    Err(e) => {
                        tracing::warn!(sftp = %this.name, "打开 WSL 的文件管理的任务异常结束：{e}");
                        this.status = Status::Failed(e.to_string().into());
                        this.sync_children(cx);
                    }
                }
                cx.notify();
            });
            if let Some(Ok(Ok(session))) = result {
                discard_unused(&manager, session);
            }
        }));
        cx.notify();
    }

    fn on_connected(
        &mut self,
        session: Arc<SftpSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut state = session.watch_state();
        self._state = Some(cx.spawn_in(window, async move |this, cx| {
            while state.changed().await.is_ok() {
                let state = state.borrow_and_update().clone();
                if this
                    .update_in(cx, |this, window, cx| this.on_state(state, window, cx))
                    .is_err()
                {
                    return;
                }
            }
        }));
        let home = session.home().to_string();
        tracing::info!(sftp = %self.name, session = %session.id(), home = %home, "SFTP 已连接");
        termlet_core::usage::mark(match self.target {
            SftpTarget::Wsl { .. } => termlet_core::usage::Feature::Wsl,
            _ => termlet_core::usage::Feature::Sftp,
        });
        self.lease = Some(Rc::new(SessionLease {
            session,
            manager: self.manager.clone(),
        }));
        self.status = Status::Ready;
        self.sync_children(cx);
        // 跟随中且知道终端的目录时直接去那里；重新连接时回到原来的目录
        let following = self.link.as_mut().filter(|l| l.follow).and_then(|l| {
            l.followed = l.cwd.clone();
            l.cwd.clone()
        });
        let start = following
            .or_else(|| (!self.dir.is_empty()).then(|| self.dir.clone()))
            .unwrap_or(home);
        self.load(start, false, window, cx);
        self.query_cwd_now(window, cx);
    }

    fn on_state(&mut self, state: SessionState, window: &mut Window, cx: &mut Context<Self>) {
        match state {
            SessionState::Connected => {
                if self.status == Status::Reconnecting {
                    tracing::info!(sftp = %self.name, "SFTP 已重新连接");
                    self.status = Status::Ready;
                    self.refresh(window, cx);
                }
            }
            SessionState::Reconnecting { .. } => {
                if self.status == Status::Ready {
                    tracing::info!(sftp = %self.name, "SFTP 连接断开，正在自动重连");
                    self.status = Status::Reconnecting;
                }
            }
            SessionState::Closed { error } => {
                if let Some(error) = error {
                    tracing::info!(sftp = %self.name, "SFTP 会话已关闭：{error}");
                }
                self.status = Status::Lost;
                self.loading = false;
            }
        }
        self.sync_children(cx);
        cx.notify();
    }

    /// 连接状态变了：列表、本机栏的上传、传输列表的继续跟着可用或不可用。
    fn sync_children(&mut self, cx: &mut Context<Self>) {
        let ready = self.ready();
        let session = self.session().map(|s| s.id().clone());
        let reconnecting = self.status == Status::Reconnecting;
        self.list
            .update(cx, |list, cx| list.set_disabled(!ready, cx));
        self.local
            .update(cx, |local, cx| local.set_remote_ready(ready, cx));
        self.transfers
            .update(cx, |list, cx| list.set_session(session, reconnecting, cx));
    }

    /// 可以手动重新连接：已断开（重连放弃、会话已关闭）或打开失败。
    fn can_reconnect(&self) -> bool {
        matches!(self.status, Status::Lost | Status::Failed(_))
    }

    /// 重新连接（断开、打开失败后）：用新的会话，回到原来的目录。
    fn reconnect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_reconnect() {
            return;
        }
        tracing::info!(sftp = %self.name, "SFTP 手动重新连接");
        self._state = None;
        self.lease = None;
        self.connect(window, cx);
    }

    // ------------------------------------------------------------ 加载目录

    /// 打开目录。`user` 为用户自己换的目录（与终端的目录不同时停止跟随）。
    pub(crate) fn load(
        &mut self,
        path: String,
        user: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session() else {
            return;
        };
        self.seq += 1;
        let seq = self.seq;
        self.loading = true;
        tracing::debug!(sftp = %self.name, path = %path, user, "SFTP 打开目录");
        let job = termlet_core::runtime::spawn(async move { session.list(&path).await });
        self._load = Some(cx.spawn_in(window, async move |this, cx| {
            let result = match job.await {
                Ok(result) => result,
                Err(e) => Err(SftpError::Unavailable(format!("任务异常结束：{e}"))),
            };
            let _ = this.update_in(cx, |this, window, cx| {
                if this.seq != seq {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(listing) => {
                        this.apply_listing(listing, user, window, cx);
                        // 加载期间终端又换了目录（那时加载中，没有跟过去）
                        this.sync_follow(window, cx);
                    }
                    // 断线由会话状态处理（自动重连后刷新）
                    Err(error) if error.is_disconnected() => {}
                    Err(error) => {
                        tracing::warn!(sftp = %this.name, "SFTP 打开目录失败：{error}");
                        this.error = Some(errors::describe(&error));
                        this.restore_path(window, cx);
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(crate) fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.dir.is_empty() {
            self.load(self.dir.clone(), false, window, cx);
        }
    }

    /// 新建、改名后刷新，选中名为 `name` 的新项（被筛选筛掉时先清空筛选）。
    fn refresh_to(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.list
            .update(cx, |list, _| list.reveal_after_refresh(name));
        self.refresh(window, cx);
    }

    fn apply_listing(
        &mut self,
        listing: termlet_core::sftp::Listing,
        user: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let same = listing.path == self.dir;
        let rows = listing.entries.iter().map(remote::row_of).collect();
        let dir = listing.path.clone();
        // 根目录没有上一级
        let parent = termlet_core::sftp::path::parent(&dir) != dir;
        self.list.update(cx, |list, cx| {
            if same {
                list.refresh_entries(rows, cx)
            } else {
                list.set_entries(dir.clone(), rows, parent, cx)
            }
        });
        // 用户正在路径框中输入时，后台完成的加载不覆盖输入的内容
        let editing = self.path.focus_handle(cx).is_focused(window)
            && self.path.read(cx).value().as_ref() != self.dir.as_str();
        self.entries = listing.entries;
        self.dir = listing.path;
        self.error = None;
        if !editing {
            super::show_path(&self.path, self.dir.clone(), window, cx);
        }
        if user
            && let Some(link) = &mut self.link
            && link.follow
            && link.cwd.as_deref() != Some(self.dir.as_str())
        {
            link.follow = false;
        }
    }

    /// 路径框恢复为当前目录。
    fn restore_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        super::show_path(&self.path, self.dir.clone(), window, cx);
    }

    fn on_path_event(
        &mut self,
        input: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::PressEnter { .. } => {
                let text = input.read(cx).value().trim().to_string();
                let path = if text.is_empty() {
                    "/".to_string()
                } else {
                    text
                };
                self.load(path, true, window, cx);
                self.list.focus_handle(cx).focus(window, cx);
            }
            // 离开时有没提交的输入：恢复为当前目录；没改过的滚回末尾，露出当前目录名
            InputEvent::Blur if input.read(cx).value().as_ref() != self.dir.as_str() => {
                self.restore_path(window, cx);
            }
            InputEvent::Blur => input.update(cx, super::scroll_path_to_end),
            _ => {}
        }
    }

    // ------------------------------------------------------------ 跟随终端目录

    fn link_terminal(
        &mut self,
        terminal: &Entity<TerminalPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let observe = cx.observe_in(terminal, window, |this, terminal, window, cx| {
            let cwd = terminal.read(cx).working_directory(cx);
            if let (Some(link), Some(cwd)) = (&mut this.link, cwd)
                && link.cwd.as_deref() != Some(cwd.as_str())
            {
                link.cwd = Some(cwd);
                link.reported = true;
                this.sync_follow(window, cx);
                cx.notify();
            }
        });
        let release = cx.observe_release(terminal, |this, _, cx| {
            this.link = None;
            this._poll = None;
            cx.notify();
        });
        let cwd = terminal.read(cx).working_directory(cx);
        self.link = Some(Link {
            terminal: terminal.downgrade(),
            follow: true,
            reported: cwd.is_some(),
            cwd,
            followed: None,
            _observe: observe,
            _release: release,
        });
        // 没有 OSC 7 的终端：显示中、跟随中时定时查询
        self._poll = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(CWD_POLL).await;
                // 目录变化时正好在加载、开着对话框而没跟过去的，这时补上
                if this
                    .update_in(cx, |this, window, cx| this.sync_follow(window, cx))
                    .is_err()
                {
                    return;
                }
                let Ok(job) = this.update(cx, |this, cx| this.cwd_query(cx)) else {
                    return;
                };
                let Some(job) = job else {
                    continue;
                };
                let result = job.await;
                if this
                    .update_in(cx, |this, window, cx| this.apply_cwd(result, window, cx))
                    .is_err()
                {
                    return;
                }
            }
        }));
    }

    /// 连上后立即查询一次终端的目录（不等定时查询）。
    fn query_cwd_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(job) = self.cwd_query(cx) else {
            return;
        };
        self._cwd_now = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| this.apply_cwd(result, window, cx));
        }));
    }

    fn apply_cwd(
        &mut self,
        result: Result<Result<Option<String>, SftpError>, tokio::task::JoinError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(link) = &mut self.link else {
            return;
        };
        match result {
            Ok(Ok(Some(cwd))) => link.cwd = Some(cwd),
            Ok(Ok(None)) => link.cwd = None,
            // 查询失败：保持原来的
            _ => return,
        }
        self.sync_follow(window, cx);
        cx.notify();
    }

    /// 要不要查询终端的目录：跟随中、在显示、终端没有报告过目录。
    fn cwd_query(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<tokio::task::JoinHandle<Result<Option<String>, SftpError>>> {
        let link = self.link.as_ref()?;
        if !link.follow || link.reported || !self.visible() || !self.ready() {
            return None;
        }
        let terminal = link.terminal.upgrade()?;
        // WSL 终端在发行版中查询
        if let Some(reader) = terminal.update(cx, |terminal, _| terminal.wsl_cwd_reader()) {
            return Some(termlet_core::runtime::spawn(async move {
                reader
                    .read()
                    .await
                    .map_err(|e| SftpError::Unavailable(e.to_string()))
            }));
        }
        let (reader, connection) = terminal.update(cx, |terminal, _| terminal.cwd_reader())?;
        Some(termlet_core::runtime::spawn(async move {
            reader.read(&connection).await
        }))
    }

    /// 跟随中、终端的目录与显示的不同时切过去（对话框打开期间暂停，免得目录在操作中途变了）。正在加载、
    /// 开着对话框时跳过的，在加载完成后、下一次定时查询时补上；打不开的目录不反复重试。
    fn sync_follow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(link) = &mut self.link else {
            return;
        };
        if !link.follow || self.status != Status::Ready || self.loading {
            return;
        }
        if window.has_active_dialog(cx) {
            return;
        }
        if let Some(cwd) = link.cwd.clone()
            && cwd != self.dir
            && link.followed.as_ref() != Some(&cwd)
        {
            link.followed = Some(cwd.clone());
            self.load(cwd, false, window, cx);
        }
    }

    fn toggle_follow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(link) = &mut self.link {
            link.follow = !link.follow;
            tracing::debug!(sftp = %self.name, follow = link.follow, "跟随终端目录");
            if link.follow {
                // 重新跟随：回到终端的目录（即使之前跟过）
                link.followed = None;
                self.sync_follow(window, cx);
            }
            cx.notify();
        }
    }

    // ------------------------------------------------------------ 两栏

    fn visible(&self) -> bool {
        self.active && self.bounds.get().size.width > px(0.)
    }

    fn is_narrow(&self, window: &Window) -> bool {
        let width = self.bounds.get().size.width;
        width > px(0.) && width < rems(NARROW_WIDTH).to_pixels(window.rem_size())
    }

    fn shows_local(&self, window: &Window, cx: &App) -> bool {
        if self.bounds.get().size.width <= px(0.) {
            return false;
        }
        if self.is_narrow(window) {
            self.narrow_open
        } else {
            !AppState::global(cx).ui.read(cx).state().sftp_local_hidden
        }
    }

    fn toggle_local(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_narrow(window) {
            self.narrow_open = !self.narrow_open;
            cx.notify();
        } else {
            AppState::ui(cx).update(cx, |ui, cx| {
                ui.update(|s| s.sftp_local_hidden = !s.sftp_local_hidden, cx)
            });
        }
    }

    fn local_ratio(&self, cx: &App) -> f32 {
        AppState::global(cx)
            .ui
            .read(cx)
            .state()
            .sftp_local_ratio
            .unwrap_or(0.5)
            .clamp(RATIO_MIN, RATIO_MAX)
    }

    fn set_ratio(&mut self, ratio: Option<f32>, cx: &mut Context<Self>) {
        let ratio = ratio.map(|r| (r.clamp(RATIO_MIN, RATIO_MAX) * 100.).round() / 100.);
        AppState::ui(cx).update(cx, |ui, cx| ui.update(|s| s.sftp_local_ratio = ratio, cx));
    }

    fn on_splitter_drag(
        &mut self,
        event: &DragMoveEvent<SplitterDrag>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let bounds = event.bounds;
        if bounds.size.width <= px(0.) {
            return;
        }
        let ratio =
            f32::from(event.event.position.x - bounds.origin.x) / f32::from(bounds.size.width);
        self.set_ratio(Some(ratio), cx);
    }

    fn on_splitter_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let step = match event.keystroke.key.as_str() {
            "left" => -0.05,
            "right" => 0.05,
            _ => return,
        };
        let ratio = self.local_ratio(cx) + step;
        self.set_ratio(Some(ratio), cx);
        cx.stop_propagation();
    }

    // ------------------------------------------------------------ 事件

    fn on_list_event(
        &mut self,
        _: &Entity<FileList>,
        event: &FileListEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.ready() {
            return;
        }
        match event {
            FileListEvent::Open(row) => self.open_row(row, window, cx),
            FileListEvent::Up => self.go_up(window, cx),
            FileListEvent::Rename(row) => self.rename(row.clone(), window, cx),
            FileListEvent::Refresh => {
                let dir = self.dir.clone();
                self.load(dir, true, window, cx)
            }
            FileListEvent::Delete => self.confirm_delete(window, cx),
            FileListEvent::CopyPaths => self.copy_paths(window, cx),
            FileListEvent::PasteFiles => self.paste_files(window, cx),
            FileListEvent::Drop { files, into } => {
                if files.source == DragSource::Local {
                    let paths = files
                        .names
                        .iter()
                        .map(|name| std::path::Path::new(&files.dir).join(name))
                        .collect();
                    self.upload(paths, into.clone(), window, cx);
                }
            }
            FileListEvent::DropExternal { paths, into } => {
                self.upload(paths.clone(), into.clone(), window, cx)
            }
            FileListEvent::NewFolder => self.new_folder(window, cx),
            FileListEvent::FocusPath => self.focus_path(window, cx),
            FileListEvent::FindInTerminal(query) => self.find_in_terminal(query, window, cx),
        }
    }

    /// “在终端中查找”写进跟随着的终端的命令（见 [`list_model::find_command`]），与写给哪个终端。终端不能用
    /// （没有跟随的、断开了、在运行全屏程序、服务器是 Windows，没有 find）或名称中有写不进终端的字符时为 `None`，
    /// 筛选框中不显示这个按钮。
    fn find_command(&self, query: &str, cx: &App) -> Option<(Entity<TerminalPanel>, String)> {
        let link = self.link.as_ref()?;
        let terminal = link.terminal.upgrade()?;
        let usable = {
            let terminal = terminal.read(cx);
            terminal.is_live(cx)
                && !terminal.is_alt_screen(cx)
                && terminal.remote_os() != Some(RemoteOs::Windows)
        };
        // 终端就在显示的目录中时从 . 找起，命令短一些
        let dir = if link.cwd.as_deref() == Some(self.dir.as_str()) {
            "."
        } else {
            self.dir.as_str()
        };
        let command = list_model::find_command(dir, query).filter(|_| usable)?;
        Some((terminal, command))
    }

    /// 把 find 命令写进跟随着的终端（不执行，写在终端里已有的输入之后），切到终端，由用户按 Enter。
    fn find_in_terminal(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some((terminal, command)) = self.find_command(query, cx) else {
            return;
        };
        tracing::debug!(sftp = %self.name, "在终端中查找");
        let input = crate::snippets::command_input(&command, Some(false));
        match terminal.update(cx, |terminal, cx| terminal.write_command(input, cx)) {
            Ok(()) => cx.emit(SftpPanelEvent::ShowTerminal(terminal)),
            Err(message) => crate::notify::error(message, window, cx),
        }
    }

    /// 远程栏、本机栏的列表的焦点（测试中核对 Tab 切换）。
    #[cfg(test)]
    pub(crate) fn list_focus(&self, cx: &App) -> (FocusHandle, FocusHandle) {
        (self.list.focus_handle(cx), self.local.focus_handle(cx))
    }

    /// 本机栏的列表（测试中核对选择）。
    #[cfg(test)]
    pub(crate) fn local_list(&self, cx: &App) -> Entity<FileList> {
        self.local.read(cx).list()
    }

    /// 本机栏的路径框（测试中核对显示的位置）。
    #[cfg(test)]
    pub(crate) fn local_path(&self, cx: &App) -> Entity<gpui_kit::component::input::InputState> {
        self.local.read(cx).path_box()
    }

    /// 编辑路径（Ctrl+L、Alt+D）：路径框取得焦点，选中其中的全部文字。
    fn focus_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.path.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });
    }

    /// 路径框中按 Esc：放弃没有提交的输入，回到列表。
    fn path_to_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.restore_path(window, cx);
        self.list.focus_handle(cx).focus(window, cx);
    }

    /// Tab：在本机栏与远程栏的列表之间切换；只有远程一栏时交给默认的焦点切换。
    fn switch_pane(
        &mut self,
        _: &file_list::SwitchPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.shows_local(window, cx) {
            cx.propagate();
            return;
        }
        let remote = self.list.focus_handle(cx);
        if remote.contains_focused(window, cx) {
            self.local.focus_handle(cx).focus(window, cx);
        } else {
            remote.focus(window, cx);
        }
    }

    fn on_local_event(
        &mut self,
        _: &Entity<LocalPane>,
        event: &LocalPaneEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            LocalPaneEvent::Upload(paths) => {
                if self.ready() {
                    self.upload(paths.clone(), None, window, cx)
                }
            }
            LocalPaneEvent::DropRemote { files, into } => {
                if self.ready() && files.source == DragSource::Remote(cx.entity_id()) {
                    let paths = files
                        .names
                        .iter()
                        .map(|name| termlet_core::sftp::path::join(&files.dir, name))
                        .collect();
                    self.download_to(paths, into.clone(), cx);
                }
            }
        }
    }

    /// 上传到当前目录、下载到本机栏当前目录的任务完成后，刷新对应的列表。
    fn on_transfer_event(
        &mut self,
        _: &Entity<TransferList>,
        event: &TransferListEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let TransferListEvent::Finished(item) = event;
        match item.direction {
            termlet_core::sftp::Direction::Upload => {
                if termlet_core::sftp::path::parent(&item.remote) == self.dir && self.ready() {
                    self.refresh(window, cx);
                }
            }
            termlet_core::sftp::Direction::Download => {
                let local_dir = self.local.read(cx).dir().map(|d| d.to_path_buf());
                if let (Some(dir), Some(parent)) = (local_dir, item.local.parent())
                    && same_dir(&dir, parent)
                {
                    self.local.update(cx, |local, cx| local.refresh(window, cx));
                }
            }
        }
    }

    // ------------------------------------------------------------ 关闭

    /// 关闭这个 Tab：有未完成的传输时总是确认；否则按设置，连接着时确认。
    pub fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pending = self.pending_transfers(cx);
        let confirm = pending > 0 || self.needs_close_confirm();
        if !confirm {
            self.close_tab(cx);
            return;
        }
        let this = cx.entity().downgrade();
        let on_confirm = move |_: &mut Window, cx: &mut App| {
            let _ = this.update(cx, |this, cx| this.close_tab(cx));
        };
        if pending > 0 {
            crate::close_confirm::confirm_tab_with(
                self.name.clone(),
                t!("sftp.close_transfers", count = pending),
                on_confirm,
                window,
                cx,
            );
        } else {
            crate::close_confirm::confirm_tab(self.name.clone(), on_confirm, window, cx);
        }
    }
}

/// 以 root 身份打开的标记（Tab 标题中的 `compact`，工具栏中的稍大）：悬停时说明影响范围（Tab 的右键菜单开着时
/// 不显示，菜单关上后照常，见 [`crate::menu_tooltip::quiet`]）。
fn root_badge(id: &'static str, compact: bool) -> impl IntoElement {
    let tag = gpui_kit::component::tag::Tag::danger().child(t!("sftp.root_badge"));
    div()
        .id(id)
        .test_support()
        .flex_none()
        .tooltip(crate::menu_tooltip::quiet(|window, cx| {
            gpui_kit::component::tooltip::Tooltip::new(t!("sftp.root_hint")).build(window, cx)
        }))
        .child(if compact { tag.small() } else { tag })
}

/// SCP 模式的标记：悬停时说明受限之处。
fn scp_badge(id: &'static str) -> impl IntoElement {
    div()
        .id(id)
        .test_support()
        .flex_none()
        .tooltip(|window, cx| {
            gpui_kit::component::tooltip::Tooltip::new(t!("sftp.scp_hint")).build(window, cx)
        })
        .child(gpui_kit::component::tag::Tag::warning().child(t!("sftp.scp_badge")))
}

/// 两个本机目录是否相同（Windows 不区分大小写，忽略末尾的分隔符）。
fn same_dir(a: &std::path::Path, b: &std::path::Path) -> bool {
    let norm = |p: &std::path::Path| {
        let text = p.to_string_lossy();
        let text = text.trim_end_matches(['\\', '/']).to_string();
        if cfg!(windows) {
            text.replace('/', "\\").to_lowercase()
        } else {
            text
        }
    };
    norm(a) == norm(b)
}

impl BasePanel for SftpPanel {
    fn panel_name(&self) -> &'static str {
        PANEL_NAME
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.active = active;
        if active {
            self.list.focus_handle(cx).focus(window, cx);
            self.sync_follow(window, cx);
        }
        cx.notify();
    }
}

impl SftpPanel {
    /// Tab 名称前的图标：文件夹说明这是文件管理（同一台主机的终端与 SFTP 的 Tab 同名），后面是系统的标志（有的话）。
    fn tab_icons(&self, cx: &App) -> Vec<AnyElement> {
        let theme = cx.theme();
        let mut icons = vec![
            Icon::new(IconName::FolderOpen)
                .xsmall()
                .text_color(theme.info)
                .into_any_element(),
        ];
        if let Some(brand) = self.target.brand(&self.store) {
            let style = self.store.settings().appearance.icons;
            let color = brand_icons::color(
                brand,
                style,
                theme.muted_foreground,
                theme.tab_bar,
                theme.foreground,
            );
            icons.push(brand_icons::icon(brand, color).xsmall().into_any_element());
        }
        icons
    }

    /// Tab 名称后的状态：以 root 身份打开的标记、连接中 / 断开的圆点。
    fn tab_badges(&self, cx: &App) -> Vec<AnyElement> {
        let status = match &self.status {
            Status::Connecting | Status::Reconnecting => Some(cx.theme().warning),
            Status::Lost | Status::Failed(_) => Some(cx.theme().danger),
            Status::Ready => None,
        };
        let mut badges = Vec::new();
        if self.target.is_root() {
            badges.push(root_badge("sftp-tab-root-badge", true).into_any_element());
        }
        if let Some(color) = status {
            badges.push(div().size_1p5().rounded_full().bg(color).into_any_element());
        }
        badges
    }

    /// 连接目标（见 [`SftpTarget::target_label`]）：横排的 Tab 的悬停提示（名称就写在 Tab 上，不再重复），竖排时在
    /// 名称下面；与名称相同时不再写（快速连接的名称就是 `用户@地址`）。
    fn tab_target(&self) -> Option<SharedString> {
        self.target
            .target_label(&self.store)
            .filter(|target| target.as_str() != self.name.as_ref())
            .map(SharedString::from)
    }

    /// 竖排的 Tab 列表中这个 Tab 的一行：与横排的 Tab 上相同的图标、名称与状态；按主机分组时与同一台主机的终端归在
    /// 一起。
    pub(crate) fn tab_face(&self, cx: &App) -> crate::workspace::TabFace {
        let icons = self.tab_icons(cx);
        let theme = cx.theme();
        let status = match &self.status {
            Status::Connecting | Status::Reconnecting => Some(crate::workspace::TabStatus::dot(
                theme.warning,
                t!("tab_list.status.connecting"),
            )),
            Status::Lost | Status::Failed(_) => Some(crate::workspace::TabStatus::dot(
                theme.danger,
                t!("tab_list.status.disconnected"),
            )),
            Status::Ready => None,
        };
        crate::workspace::TabFace {
            icon: h_flex().gap_1().children(icons).into_any_element(),
            rail_icon: Icon::new(IconName::FolderOpen)
                .with_size(crate::workspace::RAIL_ICON)
                .text_color(theme.info)
                .into_any_element(),
            name: self.name.clone(),
            detail: None,
            target: self.tab_target(),
            badges: self.tab_badges(cx),
            status: status.into_iter().collect(),
            host: Some((
                crate::workspace::host_group_key(&self.target).into(),
                self.name.clone(),
            )),
        }
    }
}

impl Panel for SftpPanel {
    fn title(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let close_id = ("sftp-tab-close", cx.entity_id().as_u64() as usize);
        // 右键菜单开着时关闭按钮不挂悬停提示（会浮在菜单上，见 crate::menu_tooltip）
        let menu_open = crate::menu_tooltip::menu_open(window, cx);
        // 名称太长时截断，提示的第一行补上全名
        let (clip, name) = (Clip::default(), self.name.clone());
        let target = self.tab_target();
        h_flex()
            .id(("sftp-tab-title", cx.entity_id().as_u64() as usize))
            .test_support()
            .gap_1()
            // 悬停提示是连接目标（也是无障碍的描述）
            .when_some(target.clone(), |this, target| this.aria_description(target))
            .tooltip(crate::menu_tooltip::quiet({
                let (clip, name) = (clip.clone(), name.clone());
                move |window, cx| {
                    let text = crate::workspace::tooltip_lines([clip.full(&name), target.as_ref()]);
                    crate::truncated::tooltip(text, window, cx)
                }
            }))
            // 右键 Tab 打开这个 Tab 的菜单（Tab 栏上没有“…”，见 dock_skin）
            .context_menu({
                let this = cx.entity().downgrade();
                move |menu, window, cx| {
                    let menu = crate::menu_fit::fit(menu, window);
                    match this.upgrade() {
                        Some(panel) => {
                            panel.update(cx, |panel, cx| panel.dropdown_menu(menu, window, cx))
                        }
                        None => menu,
                    }
                }
            })
            // 文件夹图标说明这是文件管理（同一台主机的终端与 SFTP 的 Tab 同名），后面是系统的标志（有的话）
            .children(self.tab_icons(cx))
            .child(crate::dock_skin::tab_name(&clip, name))
            .children(self.tab_badges(cx))
            .child(
                Button::new(close_id)
                    .ghost()
                    .small()
                    .icon(IconName::Close)
                    .tab_stop(false)
                    .when(!menu_open, |this| this.tooltip(t!("panel.close")))
                    .on_click(cx.listener(|this, _, window, cx| {
                        cx.stop_propagation();
                        this.request_close(window, cx);
                    })),
            )
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }

    fn dropdown_menu(
        &mut self,
        menu: gpui_kit::component::menu::PopupMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::component::menu::PopupMenu {
        use gpui_kit::component::menu::PopupMenuItem;
        let this = cx.entity().downgrade();
        let act = |f: fn(&mut SftpPanel, &mut Window, &mut Context<SftpPanel>)| {
            let this = this.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let _ = this.update(cx, |this, cx| f(this, window, cx));
            }
        };
        let menu = menu
            .item(
                PopupMenuItem::new(t!("sftp.refresh"))
                    .disabled(!self.ready())
                    .on_click(act(|this, window, cx| this.refresh(window, cx))),
            )
            .item(
                // 只在已断开、打开失败时可用：连接着或正在自动重连时重新连接会放下会话，取消进行中与等待重连的传输
                PopupMenuItem::new(t!("sftp.reconnect"))
                    .disabled(!self.can_reconnect())
                    .on_click(act(|this, window, cx| this.reconnect(window, cx))),
            );
        // 以 root 身份另开一个 Tab（堡垒机“只用一个通道”、已经是 root 的没有）
        let menu = match self.target.as_root(&self.store) {
            Some(target) => menu.item(
                PopupMenuItem::new(match target {
                    SftpTarget::Wsl { .. } => t!("hosts.local.sftp_root"),
                    _ => t!("sftp.open_root"),
                })
                .on_click(act(|_, _, cx| cx.emit(SftpPanelEvent::OpenAsRoot))),
            ),
            None => menu,
        };
        // 在 VS Code、Zed 中打开当前目录（装了的、设置允许的；还不知道目录时没有）
        let menu = if self.dir.is_empty() {
            menu
        } else {
            super::open_in_editor::menu_items(menu, &self.target, self.dir.clone(), &self.store, cx)
        };
        let menu = match self.target.address(&self.store) {
            Some(address) => menu.item(crate::copy_address_item(address)),
            None => menu,
        };
        // 保存的主机（含以 root 打开的）：在侧边栏中显示它；WSL、临时连接没有
        let menu = match self.target.host() {
            Some(host) => menu.item(crate::workspace::reveal_host_item(host.clone())),
            None => menu,
        };
        let menu = menu.separator().item(
            PopupMenuItem::new(t!("panel.close"))
                .on_click(act(|this, window, cx| this.request_close(window, cx))),
        );
        crate::workspace::close_tab_items(menu, cx.entity_id(), window, cx).separator()
    }
}

impl Render for SftpPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bounds = self.bounds.clone();
        let remote_width = self.remote_width.clone();
        let show_local = self.shows_local(window, cx);
        let ratio = self.local_ratio(cx);
        let toolbar = self.render_toolbar(window, cx);
        let banner = self.render_banner(cx);
        let theme = cx.theme();
        let border = theme.border;
        let drag_border = theme.drag_border;
        let remote = v_flex()
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(
                canvas(
                    move |area, _, _| remote_width.set(area.size.width),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(toolbar)
            .children(banner)
            .child(div().flex_1().min_h_0().child(self.list.clone()));
        let body = h_flex()
            .flex_1()
            .min_h_0()
            .items_stretch()
            .when(show_local, |this| {
                this.child(
                    div()
                        .flex_none()
                        .h_full()
                        .w(gpui_kit::relative(ratio))
                        .child(self.local.clone()),
                )
                .child(
                    // 分隔线 1 像素是物理边界；两边各有加宽的拖动区域
                    div()
                        .id("sftp-splitter")
                        .track_focus(&self.splitter_focus)
                        .relative()
                        .flex_none()
                        .w(px(1.))
                        .h_full()
                        .bg(border)
                        .focus(|s| s.bg(drag_border))
                        .on_key_down(cx.listener(Self::on_splitter_key))
                        .child(
                            div()
                                .id("sftp-splitter-handle")
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .left(-rems(0.25))
                                .w(rems(0.5))
                                .cursor_col_resize()
                                .hover(|s| s.bg(drag_border.opacity(0.5)))
                                .tooltip(|window, cx| {
                                    gpui_kit::component::tooltip::Tooltip::new(t!("local.resize"))
                                        .build(window, cx)
                                })
                                .on_drag(SplitterDrag, |_, _, _, cx| cx.new(|_| SplitterDrag))
                                .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                                    this.splitter_focus.focus(window, cx);
                                    if event.click_count() >= 2 {
                                        this.set_ratio(None, cx);
                                    }
                                })),
                        ),
                )
            })
            .child(remote);
        let show_transfers = !self.transfers.read(cx).is_empty();
        v_flex()
            .id("sftp-panel")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .on_action(cx.listener(Self::switch_pane))
            .on_drag_move(cx.listener(Self::on_splitter_drag))
            .child(
                canvas(move |area, _, _| bounds.set(area), |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            )
            .child(body)
            .when(show_transfers, |this| this.child(self.transfers.clone()))
    }
}
