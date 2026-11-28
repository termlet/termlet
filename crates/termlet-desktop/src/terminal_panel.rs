//! 终端 Tab：Dock 中的一个面板，里面是一个终端视图（本地 shell 或 SSH）。
//!
//! - SSH 的 Tab 先显示"正在连接"，连上后才有终端视图；第一次就连不上时显示原因与"重新连接"。
//! - 连接意外断开时（设置中开启了自动重连）按 1、2、4……30 秒退避重试，画面与回滚保留，新的输出接在后面；
//!   网络错误以外的失败（认证失败、指纹变化等）不再重试。系统从睡眠中唤醒后立即重试。
//! - 保活超过 10 秒没有回应时顶部提示"连接没有响应"，可以立即重连；会话不断开。
//! - 会话正常结束后在终端中按 Enter（或 Ctrl+Shift+R）在原 Tab 重新打开；连接正常时 Ctrl+Shift+R 确认后用新的连接重连。
//! - 复制会话（Ctrl+Shift+D）共用这个终端的连接，不再认证。
//! - 主机设置了 tmux 时每个 Tab 连到自己的 tmux 会话，重连后回到原来的现场；关闭 Tab 时结束该会话。
//! - 命令历史、长命令结束与程序请求的通知、后台的新输出与响铃标记、登录后执行的命令、sudo 密码填入、
//!   会话日志、会话录制（见 `terminal_panel/record.rs`）、命令补全提示（见 `terminal_panel/suggest.rs`）、编码切换、
//!   网络延迟（底部状态栏）。

mod clipboard;
mod rebind;
mod record;
mod run_done;
mod suggest;

pub(crate) use record::clock_text;

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::dock::{BasePanel, Panel, PanelEvent};
use gpui_kit::component::input as text_input;
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, TestSupportExt as _, Window,
    actions, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::forward::{ProxyAddress, ProxyShare, ShareError, ShareStatus, StopReason};
use termlet_core::id::HostId;
use termlet_core::localpty;
use termlet_core::localshell::Profile;
use termlet_core::model::{Encoding, Host, Protocol, RemoteOs};
use termlet_core::rawterm::{self, RawError, RawSpec};
use termlet_core::secret::SecretString;
use termlet_core::session_log::{self, Halt, SessionLog};
use termlet_core::sftp::TerminalCwd;
use termlet_core::sudo::{self, PromptEvent};

use termlet_core::hosttools::ExecError;
use termlet_core::hosttools::{HostTools, Target};
use termlet_core::launch::LaunchTarget;
use termlet_core::monitor::{MonitorError, ResourceMonitor};
use termlet_core::quick::QuickTarget;
use termlet_core::ssh::{
    self, Connection, Health, OpenedTerminal, Prompter, SshError, TerminalSession, TerminalSpec,
    TmuxStatus, X11Notice, X11Problem, X11Status,
};
use termlet_core::store::Store;
use termlet_terminal::TerminalIo;
use termlet_terminal::{
    CloseReason, EngineOptions, GridSize, ShellEvent, TerminalEngine, TerminalInput, TerminalStyle,
    TerminalView, TerminalViewEvent,
};

use crate::host_tools::target::ToolsState;
use crate::monitor_bar::{self, BarFit, Resources};
use crate::monitor_model::{History, Levels};
use crate::output_tap::{OutputTap, TapEvent};
use crate::terminal_transfer::{TransferBar, TransferBarClosed};
use termlet_core::termxfer::LinkInfo;

use crate::app_state::AppState;
use crate::brand_icons::{self, Brand};
use crate::prose::prose;
use crate::terminal_colors::{self, SchemeLevels};
use crate::terminal_search::{OpenSearch, SearchBar, SearchBarEvent};
use crate::{local_shell, ssh_errors, t};

/// 面板在持久化布局中的名称，发布后不能改。
const PANEL_NAME: &str = "TerminalPanel";
pub(crate) const KEY_CONTEXT: &str = "TerminalPanel";

actions!(
    terminal_panel,
    [
        /// 重新连接（会话已结束时直接重连，连接正常时确认后用新的连接重连）
        Reconnect,
        /// 复制会话：共用当前终端的连接新开一个 Tab
        DuplicateSession,
        /// 关闭这个 Tab（有连接时按设置先确认）
        CloseTab,
        /// 开始 / 停止会话录制
        ToggleRecording,
    ]
);

/// 注册终端 Tab 固定的快捷键（按当前系统的习惯）。
pub fn init(cx: &mut App) {
    cx.bind_keys(key_bindings(cx.platform_info().os));
}

/// 终端 Tab 固定的快捷键：关闭 Tab，Windows、Linux 上 Ctrl+Shift+W（Ctrl+W 给 shell），macOS 上 ⌘W。
pub(crate) fn key_bindings(os: gpui_kit::OperatingSystem) -> Vec<gpui_kit::KeyBinding> {
    let combo = if os == gpui_kit::OperatingSystem::Mac {
        "cmd-w"
    } else {
        "ctrl-shift-w"
    };
    vec![gpui_kit::KeyBinding::new(
        combo,
        CloseTab,
        Some(KEY_CONTEXT),
    )]
}

/// 终端 Tab 发给工作区的请求。
pub enum TerminalPanelEvent {
    /// 关闭这个 Tab（已确认过）
    Close,
    /// 发一条系统通知（终端不在眼前时；是否发、节流由工作区决定）
    Notify {
        title: SharedString,
        body: SharedString,
    },
    /// 为这个终端的主机添加端口转发
    AddForward,
    /// 同步输入中键入的内容：由工作区分发给同步中的终端（包括自己）
    SyncTyped(Vec<u8>),
    /// 同步输入中的粘贴：由工作区检查一次后分发
    SyncPaste(String),
    /// 点了底部的状态条：让这个终端获得焦点，在主机工具中打开概览页
    OpenOverview,
}

/// 批量执行在终端里执行时每个终端最多取回的输出（主机上执行的上限相同，见 `batch_run`）。
pub(crate) const RUN_OUTPUT_LIMIT: usize = 256 << 10;

/// 在终端中执行不了的原因（[`TerminalPanel::run_command`]）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunError {
    /// 本地终端、Telnet、串口：要 bash / sh
    Unsupported,
    /// 终端没有连接
    NotConnected,
    Terminal(termlet_core::termrun::TerminalError),
}
/// 运行超过这么久的命令结束时发通知。
const NOTIFY_COMMAND_AFTER: Duration = Duration::from_secs(10);
/// 登录后执行的命令：出现提示符，或输出后这么久没有新的输出时发送，最多等这么久。
const STARTUP_QUIET: Duration = Duration::from_millis(600);
const STARTUP_POLL: Duration = Duration::from_millis(100);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
/// 测网络延迟的间隔（与资源采样相同）。
const LATENCY_INTERVAL: Duration = Duration::from_secs(3);
/// 可以切换的编码（与主机设置中的相同）。
const ENCODINGS: [(Encoding, &str, &str); 3] = [
    (Encoding::Utf8, "utf-8", "UTF-8"),
    (Encoding::Gb18030, "gb18030", "GBK / GB18030"),
    (Encoding::Big5, "big5", "Big5"),
];

/// 终端连的是什么。
#[derive(Clone, Debug)]
pub enum Session {
    /// 本地 shell：每次打开时重新读取用户的环境变量
    Local { profile: Profile },
    /// WSL 发行版：`user` 为空时是发行版的默认用户（`root` 以 root 登录）；`dir` 为起始目录（Windows 的路径，
    /// 启动参数 `--open-folder` 指定的），没有时从 Linux 中的用户主目录开始
    Wsl {
        distro: String,
        user: String,
        dir: Option<PathBuf>,
    },
    /// 保存的主机
    Host { host: HostId },
    /// 快速连接（临时，不保存）
    Quick { target: QuickTarget },
    /// 被堡垒机等外部程序调起的临时连接：只用一个通道，密码只在内存中
    Launch { target: Arc<LaunchTarget> },
}

impl Session {
    fn is_ssh(&self) -> bool {
        matches!(
            self,
            Session::Host { .. } | Session::Quick { .. } | Session::Launch { .. }
        )
    }
}

/// 会话的状态（侧边栏主机行行首的圆点：绿为已连接、黄为正在连接或重连、红为已断开）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SessionStatus {
    Disconnected,
    Connecting,
    Connected,
}

/// 怎样发起的这次连接。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Attempt {
    /// 打开 Tab、会话结束后手动重连
    Open,
    /// 断线后的自动重连（第几次）
    Auto(u32),
    /// 连接正常时手动要求用新的连接重连
    Forced,
}

/// 会话的状态。
enum State {
    /// 正在连接（打开 Tab、手动重连）
    Connecting,
    Connected,
    /// 断线后等待第 `attempt` 次自动重连
    Waiting {
        attempt: u32,
        last_error: Option<SshError>,
    },
    /// 第 `attempt` 次自动重连进行中
    Reconnecting {
        attempt: u32,
    },
    /// 连接失败，不再自动重试
    Failed(Failure),
    /// 会话结束（正常退出；断线但没有开启自动重连）
    Ended(CloseReason),
}

/// 打不开的原因。
enum Failure {
    Ssh(SshError),
    /// 本地 shell 启动失败（已说成用户能看懂的话）
    Local(SharedString),
    /// Telnet、串口打不开
    Raw(RawError),
}

impl Failure {
    fn describe(&self) -> SharedString {
        match self {
            Failure::Ssh(error) => ssh_errors::describe(error),
            Failure::Local(message) => message.clone(),
            Failure::Raw(error) => crate::raw_errors::describe(error),
        }
    }
}

pub struct TerminalPanel {
    session: Session,
    /// 保存的主机的协议（SSH、Telnet、串口）；本地终端、快速连接为 SSH 之外的不用
    protocol: Protocol,
    /// 重连时不变：tmux 会话名、连接组
    identity: TerminalSession,
    /// 记住输入过的密码，重连、复制会话时不再询问
    prompter: Prompter,
    store: Arc<Store>,
    view: Option<Entity<TerminalView>>,
    connection: Option<Arc<Connection>>,
    /// 最近一次连接用的配置（关闭 Tab 时结束 tmux 会话要用）
    spec: Option<TerminalSpec>,
    tmux: TmuxStatus,
    state: State,
    /// 连接没有响应（还没有判定断开）
    stalled: bool,
    /// 已提醒过快过期的登录证书（哪一跳的跳板机，目标主机为 `None`；失效时间）：同一张证书重连后不再提醒
    certificate_warned: Vec<(Option<String>, i64)>,
    focus: FocusHandle,
    /// Tab 上的名称（本地 shell 的名称、主机名）
    name: SharedString,
    /// 不在前台时有新输出
    activity: bool,
    /// Dock 中在前台（由 Dock 维护）
    active: bool,
    /// 在同步输入的网格中显示（与 Dock 的前台分开记：退出同步后按 Dock 当时的前台）
    in_grid: bool,
    /// 上一帧在窗口中的位置（Alt+方向键在分屏的窗格之间切换时按位置找）
    bounds: std::rc::Rc<std::cell::Cell<gpui_kit::Bounds<gpui_kit::Pixels>>>,
    search: Option<Entity<SearchBar>>,
    _search_subscription: Option<Subscription>,
    _view_subscription: Option<Subscription>,
    _focus_subscription: Option<Subscription>,
    /// 连接任务或等待重连的计时
    _connect: Option<Task<()>>,
    /// 串口强制重连：旧的会话结束（端口释放）后再打开
    reopen_after_close: bool,
    /// 正在建会话日志的文件（建好前点“停止”时丢弃它，结果不再采用）
    _log_start: Option<Task<()>>,
    /// 正在建录制文件（建好前停止时丢弃它，空文件随之删掉）；录制本身在 `tap` 中
    _record_start: Option<Task<()>>,
    /// 录制中每秒更新 Tab 上的时长
    _record_clock: Option<Task<()>>,
    _health: Option<Task<()>>,
    /// X11 转发中的说明（连不上本机的 X 服务器等），在终端中显示
    _x11: Option<Task<()>>,
    /// 正在保存或上传粘贴的截图
    _paste_image: Option<Task<()>>,
    /// 只用于这个 Tab 的配色，以及命令面板中正在预览的配色（空为没有）
    tab_scheme: String,
    scheme_preview: String,
    /// 后台的终端响过铃
    bell: bool,
    /// 会话日志与 sudo 密码提示检测要看的原始输出
    tap: Arc<OutputTap>,
    /// 正停在 sudo 密码提示上（显示提示条，按 Enter 填入）
    password_prompt: bool,
    /// 这台主机保存的登录密码（sudo 提示时填入）；没有保存或设置关闭时为 `None`
    fill_password: Option<SecretString>,
    /// 当前传输的输入端（切换编码用）
    transport: Option<Arc<dyn TerminalInput>>,
    /// 在这个 Tab 中切换过的编码；`None` 用主机设置的
    encoding: Option<Encoding>,
    /// 登录后执行的命令要等提示符：最近一次输出的时间、是否见过提示符开始
    last_output: Option<Instant>,
    prompt_seen: bool,
    /// 最近一条开始执行的命令（命令结束的通知中显示）
    last_command: Option<String>,
    /// 网络延迟（底部状态栏）；测不到时为 `None`
    latency: Option<Duration>,
    /// 没有 OSC 7 时读取终端当前目录（SFTP 跟随终端目录）；每次连上后重建
    cwd_reader: Option<Arc<TerminalCwd>>,
    wsl_cwd: Option<Arc<termlet_core::wsl::WslCwd>>,
    /// 终端中的文件传输（rz / sz、trzsz）：每次连接的字节流都经它包装；提示栏浮在终端顶部
    transfer_bar: Entity<TransferBar>,
    _transfer_closed: Subscription,
    _tap_events: Task<()>,
    _startup: Option<Task<()>>,
    _latency: Option<Task<()>>,
    /// 主机工具（标出本终端的进程、记着上次的进程读数）与资源监控（记着上次的计数），每个 Tab 一份
    host_tools: HostTools,
    monitor: ResourceMonitor,
    /// 底部状态条上的资源使用情况；各项的级别（带回差）与最近约 10 分钟的历史（概览页的曲线），切换 Tab、重连后仍在
    resources: Resources,
    levels: Levels,
    history: History,
    /// 状态条按量出的宽度显示哪几项
    bar_fit: BarFit,
    _monitor: Option<Task<()>>,
    /// 开着的主机概览小窗数（[`Self::keep_sampling`]）：不为 0 时终端不在前台也照常采样
    sampling_holds: Rc<Cell<usize>>,
    /// 连接后识别出的系统（见 [`Self::detect_os`]）：快速连接只记在这里，保存的主机另写进主机
    detected_os: Option<RemoteOs>,
    _os_probe: Option<Task<()>>,
    /// 连上后执行一次的命令（主机工具“在新 Tab 中执行”）：代替登录后命令，重连后不再执行
    run_once: Option<String>,
    /// 等上面那个命令结束（主机工具据此重读那一页）
    run_done: run_done::RunDone,
    /// 在同步输入中：键入与粘贴交给工作区分发
    sync_member: bool,
    /// 在终端中执行命令时：会话结束时丢弃，告诉执行的一方终端已关闭
    run_closed: Option<async_channel::Sender<()>>,
    /// 把本机代理借给服务器：开启着时在这里，随会话结束而停止
    proxy: Option<ProxyLend>,
    /// 命令补全提示：交给终端视图的命令历史（见 `terminal_panel/suggest.rs`）
    suggest: suggest::Suggest,
}

/// 主机概览的小窗让终端照常采样（[`TerminalPanel::keep_sampling`]）；丢弃时撤销。
pub struct SamplingHold(Rc<Cell<usize>>);

impl Drop for SamplingHold {
    fn drop(&mut self) {
        self.0.set(self.0.get().saturating_sub(1));
    }
}

/// 正在进行的“把本机代理借给服务器”。
pub(crate) struct ProxyLend {
    pub share: Arc<ProxyShare>,
    /// 最近的状态与统计
    pub status: ShareStatus,
    _watch: Task<()>,
}

impl EventEmitter<PanelEvent> for TerminalPanel {}
impl EventEmitter<TerminalPanelEvent> for TerminalPanel {}

impl Focusable for TerminalPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.view {
            Some(view) => view.focus_handle(cx),
            None => self.focus.clone(),
        }
    }
}

impl TerminalPanel {
    /// 打开一个终端：本地终端立即启动，SSH 开始连接。
    pub fn open(
        name: SharedString,
        session: Session,
        prompter: Prompter,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        Self::create(
            name,
            session,
            TerminalSession::new(),
            ssh::remembering(prompter),
            window,
            cx,
        )
    }

    /// 复制会话：同一台主机新开一个 Tab，共用这个终端的连接（连接断开了就新建）。
    pub fn duplicate(&self, window: &mut Window, cx: &mut App) -> Entity<Self> {
        Self::create(
            self.name.clone(),
            self.session.clone(),
            self.identity.duplicate(),
            self.prompter.clone(),
            window,
            cx,
        )
    }

    /// 复制会话并在其中执行一次 `command`（主机工具的跟随日志、进入容器、以 root 执行）：出现提示符后发送，
    /// 不执行登录后命令，重连后不再执行。Tab 标题为 `title`。
    pub fn duplicate_running(
        &self,
        command: String,
        title: SharedString,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let panel = Self::create(
            title,
            self.session.clone(),
            self.identity.duplicate(),
            self.prompter.clone(),
            window,
            cx,
        );
        panel.update(cx, |panel, _| panel.run_once = Some(command));
        panel
    }

    /// 主机工具看到的这个终端的情况。
    pub fn tools_state(&self) -> ToolsState {
        let supported = match &self.session {
            Session::Wsl { .. } => true,
            Session::Host { .. } | Session::Quick { .. } => self.is_ssh(),
            Session::Launch { .. } => return ToolsState::SingleChannel,
            Session::Local { .. } => false,
        };
        if !supported {
            return ToolsState::Unsupported;
        }
        if self.spec.as_ref().is_some_and(|spec| spec.single_channel) {
            return ToolsState::SingleChannel;
        }
        match &self.state {
            State::Connecting => ToolsState::Connecting,
            State::Waiting { .. } | State::Reconnecting { .. } => ToolsState::Reconnecting,
            State::Failed(_) | State::Ended(_) => ToolsState::Ended,
            State::Connected => match &self.session {
                Session::Wsl { distro, user, .. } => {
                    ToolsState::Connected(Target::wsl(distro, user))
                }
                _ => match &self.connection {
                    Some(connection) => {
                        ToolsState::Connected(Target::ssh(connection.clone(), false))
                    }
                    None => ToolsState::Connecting,
                },
            },
        }
    }

    /// 这个终端的主机工具。
    pub fn host_tools(&self) -> HostTools {
        self.host_tools.clone()
    }

    /// 资源采样的情况（主机工具的概览页显示同一份）。
    pub fn resources(&self) -> &Resources {
        &self.resources
    }

    /// 状态条上各项的级别（概览页与状态条的着色一致）。
    pub fn monitor_levels(&self) -> Levels {
        self.levels
    }

    /// 最近约 10 分钟的采样（概览页的曲线）。
    pub fn monitor_history(&self) -> &History {
        &self.history
    }

    /// 保存的 SSH 主机（主机工具网络页的“转发到本机…”只对它提供）。
    pub fn ssh_host(&self) -> Option<&HostId> {
        match &self.session {
            Session::Host { host } if self.is_ssh() => Some(host),
            _ => None,
        }
    }

    /// 连的是哪台主机（主机工具的标题与审计记录用）：保存的主机为主机名，快速连接为 `用户@地址`，WSL 为发行版。
    /// 不用 Tab 标题：从主机工具“跟随日志”等开出的 Tab 标题是“日志 · nginx”之类。
    pub fn host_label(&self, cx: &App) -> SharedString {
        match &self.session {
            Session::Host { host } => AppState::store(cx)
                .get::<Host>(host)
                .map(|r| r.doc.name.into())
                .unwrap_or_else(|| self.name.clone()),
            Session::Quick { target } if target.port == 22 => {
                format!("{}@{}", target.username, target.host).into()
            }
            Session::Quick { target } => {
                format!("{}@{}:{}", target.username, target.host, target.port).into()
            }
            Session::Wsl { distro, .. } => distro.clone().into(),
            Session::Local { .. } | Session::Launch { .. } => self.name.clone(),
        }
    }

    fn create(
        name: SharedString,
        session: Session,
        identity: TerminalSession,
        prompter: Prompter,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let store = AppState::store(cx);
        cx.new(|cx| {
            let focus = cx.focus_handle();
            // 对话框关闭时会把焦点还给打开前的元素（常是"正在连接"的占位），那时终端视图可能已经建好，
            // 面板自己拿到焦点就转交给终端（Tab 移到别的窗口时重新登记，见 `rebind`）
            let focus_subscription = Self::forward_focus(&focus, window, cx);
            let transfer_bar = cx.new(TransferBar::new);
            let host_tools = HostTools::new(&identity.key);
            // 提示栏关闭后焦点回到终端
            let transfer_closed = cx.subscribe_in(
                &transfer_bar,
                window,
                |this: &mut Self, _, _: &TransferBarClosed, window, cx| {
                    this.focus_handle(cx).focus(window, cx);
                },
            );
            let (tap, tap_events) = OutputTap::new();
            let tap_events = cx.spawn_in(window, async move |this, cx| {
                while let Ok(event) = tap_events.recv().await {
                    if this
                        .update_in(cx, |this, window, cx| this.on_tap_event(event, window, cx))
                        .is_err()
                    {
                        break;
                    }
                }
            });
            let protocol = match &session {
                Session::Host { host } => store
                    .get::<Host>(host)
                    .map(|h| h.doc.protocol)
                    .unwrap_or_default(),
                _ => Protocol::Ssh,
            };
            let mut panel = Self {
                protocol,
                session,
                identity,
                prompter,
                store,
                view: None,
                connection: None,
                spec: None,
                tmux: TmuxStatus::Off,
                state: State::Connecting,
                stalled: false,
                certificate_warned: Vec::new(),
                focus,
                name,
                activity: false,
                active: false,
                in_grid: false,
                search: None,
                _search_subscription: None,
                _view_subscription: None,
                _focus_subscription: Some(focus_subscription),
                _connect: None,
                reopen_after_close: false,
                _log_start: None,
                _record_start: None,
                _record_clock: None,
                _health: None,
                _x11: None,
                _paste_image: None,
                tab_scheme: String::new(),
                scheme_preview: String::new(),
                bounds: Default::default(),
                bell: false,
                tap,
                password_prompt: false,
                fill_password: None,
                transport: None,
                encoding: None,
                last_output: None,
                prompt_seen: false,
                last_command: None,
                latency: None,
                cwd_reader: None,
                wsl_cwd: None,
                transfer_bar,
                _transfer_closed: transfer_closed,
                _tap_events: tap_events,
                _startup: None,
                _latency: None,
                host_tools,
                monitor: ResourceMonitor::new(),
                resources: Resources::Off,
                levels: Levels::default(),
                history: History::default(),
                bar_fit: BarFit::default(),
                _monitor: None,
                sampling_holds: Rc::default(),
                proxy: None,
                detected_os: None,
                _os_probe: None,
                run_once: None,
                run_done: Default::default(),
                sync_member: false,
                run_closed: None,
                suggest: Default::default(),
            };
            tracing::info!(
                session = %panel.name,
                kind = panel.log_kind(),
                host = ?panel.host(),
                key = %panel.identity.key,
                "打开终端"
            );
            panel.start(Attempt::Open, window, cx);
            panel
        })
    }

    /// 日志中的会话种类。
    fn log_kind(&self) -> &'static str {
        match (&self.session, self.protocol) {
            (Session::Local { .. }, _) => "local",
            (Session::Wsl { .. }, _) => "wsl",
            (Session::Quick { .. }, _) => "quick",
            (Session::Launch { .. }, _) => "launch",
            (Session::Host { .. }, Protocol::Ssh) => "ssh",
            (Session::Host { .. }, Protocol::Telnet) => "telnet",
            (Session::Host { .. }, Protocol::Serial) => "serial",
        }
    }

    /// 使用统计的功能名单：这个会话连上了（`x11`：SSH 会话开着 X11 转发）。
    fn mark_connected(&self, x11: bool) {
        use termlet_core::usage::{Feature, mark};
        mark(match (&self.session, self.protocol) {
            (Session::Local { .. }, _) => Feature::LocalShell,
            (Session::Wsl { .. }, _) => Feature::Wsl,
            (_, Protocol::Telnet) => Feature::Telnet,
            (_, Protocol::Serial) => Feature::Serial,
            (_, Protocol::Ssh) => Feature::Ssh,
        });
        if matches!(self.session, Session::Launch { .. }) {
            mark(Feature::Bastion);
        }
        if x11 {
            mark(Feature::X11);
        }
    }

    /// SSH 会话（保存的 SSH 主机、快速连接）。
    pub fn is_ssh(&self) -> bool {
        self.session.is_ssh() && self.protocol == Protocol::Ssh
    }

    /// Tab 标题前的图标，与侧边栏相同：有标志时是标志（[`Self::tab_brand`]），没有时本地终端、WSL 为终端，主机按协议。
    fn tab_icon(&self) -> IconName {
        match &self.session {
            Session::Local { .. } | Session::Wsl { .. } => IconName::SquareTerminal,
            _ => crate::host_info::protocol_icon(self.protocol),
        }
    }

    /// Tab 标题前的标志：本地终端为 shell 的，WSL 为发行版的，识别出系统的 SSH 主机为系统的（保存的主机按存储中的，
    /// 别的设备识别出、同步过来的也算）；没有时为 `None`（还没识别、Telnet、串口、没有标志的）。
    fn tab_brand(&self) -> Option<Brand> {
        match &self.session {
            Session::Local { profile } => local_shell::profile_brand(profile),
            Session::Wsl { distro, .. } => brand_icons::for_wsl(distro),
            Session::Host { .. } | Session::Quick { .. } | Session::Launch { .. } => {
                self.remote_os().and_then(brand_icons::for_os)
            }
        }
    }

    /// SSH 服务器识别出的系统（保存的主机按存储中的，别的设备识别出、同步过来的也算）；还没识别、不是 SSH 的
    /// 为 `None`。
    pub fn remote_os(&self) -> Option<RemoteOs> {
        match &self.session {
            Session::Host { host } if self.protocol == Protocol::Ssh => self
                .store
                .get::<Host>(host)
                .and_then(|h| h.doc.remote_os())
                .or(self.detected_os),
            Session::Quick { .. } => self.detected_os,
            Session::Host { .. }
            | Session::Local { .. }
            | Session::Wsl { .. }
            | Session::Launch { .. } => None,
        }
    }

    /// 登录的用户与地址（`用户@地址`，端口不是 22 时带上端口，写法见 [`crate::host_info::connection_target`]）：
    /// 主机工具的确认框写明操作的是哪台主机。SSH 开始连接后才有（按实际的连接参数，含分组的默认用户名）；本地终端、
    /// WSL、Telnet、串口为 `None`。
    pub fn login_label(&self) -> Option<SharedString> {
        let spec = self.spec.as_ref().filter(|_| !self.is_raw())?;
        let hop = spec.plan.target();
        crate::host_info::connection_target(&crate::host_info::ConnectionTarget::Ssh {
            username: &hop.username,
            address: &hop.address,
            port: hop.port,
        })
        .map(SharedString::from)
    }

    /// 连的主机的地址（Tab 菜单的“复制 IP”）；本地终端、WSL、串口没有。
    fn address(&self, cx: &App) -> Option<String> {
        match &self.session {
            Session::Host { host } if self.protocol != Protocol::Serial => AppState::store(cx)
                .get::<Host>(host)
                .map(|h| h.doc.address)
                .filter(|a| !a.is_empty()),
            Session::Quick { target } => Some(target.host.clone()),
            Session::Launch { target } => Some(target.address.clone()),
            _ => None,
        }
    }

    /// Telnet、串口会话。
    fn is_raw(&self) -> bool {
        self.session.is_ssh() && self.protocol != Protocol::Ssh
    }

    /// 正在显示时上一帧的位置；不在前台（同组里别的 Tab 在显示）时为 `None`。
    pub fn visible_bounds(&self) -> Option<gpui_kit::Bounds<gpui_kit::Pixels>> {
        let bounds = self.bounds.get();
        (self.is_shown() && bounds.size.width > gpui_kit::px(0.)).then_some(bounds)
    }

    /// Tab 上的名称（主机名、本地 shell 的名称）。
    pub fn name(&self) -> &SharedString {
        &self.name
    }

    /// 连的是哪台保存的主机。
    pub fn host(&self) -> Option<&HostId> {
        match &self.session {
            Session::Host { host } => Some(host),
            _ => None,
        }
    }

    /// 保存为工作区时记下的主机：保存的主机为主机 ID，本地终端为 `local:<shell ID>`；快速连接不保存。
    pub fn workspace_host(&self) -> Option<HostId> {
        match &self.session {
            Session::Host { host } => Some(host.clone()),
            Session::Local { profile } => Some(crate::workspaces::local_host(&profile.id)),
            Session::Wsl { distro, user, .. } => Some(crate::workspaces::wsl_host(distro, user)),
            Session::Quick { .. } | Session::Launch { .. } => None,
        }
    }

    /// SFTP 连的主机（与这个终端相同）；本地终端没有。
    pub fn sftp_target(&self) -> Option<crate::sftp::panel::SftpTarget> {
        use crate::sftp::panel::SftpTarget;
        if self.is_raw() {
            return None;
        }
        match &self.session {
            Session::Host { host } => Some(SftpTarget::Host(host.clone())),
            Session::Quick { target } => Some(SftpTarget::Quick(target.clone())),
            Session::Wsl { distro, user, .. } => Some(SftpTarget::Wsl {
                distro: distro.clone(),
                user: user.clone(),
            }),
            Session::Local { .. } => None,
            // 只用一个通道：不在终端旁另开 SFTP
            Session::Launch { .. } => None,
        }
    }

    /// 能不能复制会话、分屏：外部程序调起的临时连接只用一个通道，不能。
    pub fn can_duplicate(&self) -> bool {
        !matches!(self.session, Session::Launch { .. })
    }

    /// shell 报告的当前目录（OSC 7）；没有报告过时为 `None`。
    pub fn working_directory(&self, cx: &App) -> Option<String> {
        self.view
            .as_ref()
            .and_then(|view| view.read(cx).working_directory().map(str::to_string))
    }

    /// SFTP 共用这个终端的连接：连接着时为它的连接组（“只用一个通道”的主机由核心另开连接）。
    pub fn share_group(&self) -> Option<String> {
        (matches!(self.state, State::Connected) && self.connection.is_some())
            .then(|| self.identity.group.clone())
    }

    /// 连接组（复制会话、分屏的终端共用连接；共用它的端口转发据此跟着换窗口，见 `workspace/move_tab.rs`）。
    pub(crate) fn connection_group(&self) -> &str {
        &self.identity.group
    }

    /// 这个终端记着输入过的密码的询问入口（共用它的连接启动转发时用）。
    pub fn prompter(&self) -> Prompter {
        self.prompter.clone()
    }

    /// 连接着时的 SSH 连接（检测服务器端口等，在它上面执行命令）。
    pub fn connection(&self) -> Option<Arc<Connection>> {
        matches!(self.state, State::Connected)
            .then(|| self.connection.clone())
            .flatten()
    }

    // ------------------------------------------------------------ 把本机代理借给服务器

    /// 能不能开启“把本机代理借给服务器”：连接着的 SSH 终端，不是“只用一个通道”的主机（服务器转来的连接要另开通道）。
    pub(crate) fn can_lend_proxy(&self) -> bool {
        self.is_ssh()
            && self.connection().is_some()
            && !self.spec.as_ref().is_some_and(|spec| spec.single_channel)
    }

    /// 正在进行的“把本机代理借给服务器”。
    pub(crate) fn proxy_lend(&self) -> Option<&ProxyLend> {
        self.proxy.as_ref()
    }

    /// 开启“把本机代理借给服务器”：请服务器在 `127.0.0.1:port`（0 为由服务器选）监听，转到本机代理 `local`。
    /// 返回的任务给出结果；成功时状态条上显示，Tab 关闭、会话结束时停止。
    pub(crate) fn start_proxy_lend(
        &mut self,
        local: ProxyAddress,
        port: u16,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), ShareError>> {
        if self.proxy.is_some() {
            return Task::ready(Ok(()));
        }
        let Some(connection) = self.connection().filter(|_| self.can_lend_proxy()) else {
            return Task::ready(Err(ShareError::Disconnected));
        };
        tracing::info!(session = %self.name, local = %local, port, "开始把本机代理借给服务器");
        let job = termlet_core::runtime::spawn(ProxyShare::start(connection, local, port));
        cx.spawn(async move |this, cx| {
            let share = match job.await {
                Ok(result) => Arc::new(result?),
                Err(error) => return Err(ShareError::Other(error.to_string())),
            };
            // 终端已经关了：丢弃即停止
            this.update(cx, |this, cx| this.set_proxy_lend(share, cx))
                .map_err(|_| ShareError::Disconnected)
        })
    }

    fn set_proxy_lend(&mut self, share: Arc<ProxyShare>, cx: &mut Context<Self>) {
        let mut watch = share.watch();
        let task = cx.spawn(async move |this, cx| {
            while watch.changed().await.is_ok() {
                let status = watch.borrow_and_update().clone();
                if this
                    .update(cx, |this, cx| this.on_proxy_status(status, cx))
                    .is_err()
                {
                    return;
                }
            }
        });
        self.proxy = Some(ProxyLend {
            status: share.status(),
            share,
            _watch: task,
        });
        cx.notify();
    }

    fn on_proxy_status(&mut self, status: ShareStatus, cx: &mut Context<Self>) {
        let Some(lend) = &mut self.proxy else {
            return;
        };
        match status.stopped {
            Some(reason) => {
                tracing::info!(session = %self.name, ?reason, "本机代理已不再借给服务器");
                if reason == StopReason::ConnectionClosed
                    && let Some(view) = &self.view
                {
                    view.read(cx)
                        .engine()
                        .inject_notice(&t!("terminal.notice.proxy_stopped"));
                }
                self.proxy = None;
            }
            None => lend.status = status,
        }
        cx.notify();
    }

    /// 会话结束或换了连接（强制重连）时停止“把本机代理借给服务器”，并在终端中说明。
    fn end_proxy_lend(&mut self, cx: &mut Context<Self>) {
        if self.proxy.is_none() {
            return;
        }
        self.stop_proxy_lend(cx);
        if let Some(view) = &self.view {
            view.read(cx)
                .engine()
                .inject_notice(&t!("terminal.notice.proxy_stopped"));
        }
    }

    /// 停止“把本机代理借给服务器”（取消服务器上的监听）。
    pub(crate) fn stop_proxy_lend(&mut self, cx: &mut Context<Self>) {
        let Some(lend) = self.proxy.take() else {
            return;
        };
        tracing::info!(session = %self.name, "停止把本机代理借给服务器");
        let share = lend.share;
        termlet_core::runtime::spawn(async move { share.stop().await });
        cx.notify();
    }

    /// WSL 终端读取当前目录（没有 OSC 7 时）用的读取器；不是 WSL 终端时为 `None`。
    pub fn wsl_cwd_reader(&mut self) -> Option<Arc<termlet_core::wsl::WslCwd>> {
        let Session::Wsl { distro, user, .. } = &self.session else {
            return None;
        };
        if !matches!(self.state, State::Connected) {
            return None;
        }
        let key = self.identity.key.clone();
        Some(
            self.wsl_cwd
                .get_or_insert_with(|| Arc::new(termlet_core::wsl::WslCwd::new(distro, user, &key)))
                .clone(),
        )
    }

    /// 读取终端当前目录（没有 OSC 7 时）用的读取器与连接；没有连接时为 `None`。
    pub fn cwd_reader(&mut self) -> Option<(Arc<TerminalCwd>, Arc<Connection>)> {
        let connection = self.connection.clone()?;
        if !matches!(self.state, State::Connected) {
            return None;
        }
        let single_channel = self.spec.as_ref().is_some_and(|s| s.single_channel);
        let reader = self
            .cwd_reader
            .get_or_insert_with(|| {
                Arc::new(TerminalCwd::new(&self.identity, self.tmux, single_channel))
            })
            .clone();
        Some((reader, connection))
    }

    /// 面板自己的焦点（终端视图、查找栏都在它里面），工作区据此记下最近用过的终端。
    pub fn root_focus(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// 关闭前要不要确认：会话还开着（本地终端、WSL 的 shell 在运行，远程会话连着、正在连接或重连），并且设置中
    /// 开启了确认。shell 已退出、连接已断开的不问。
    pub fn needs_close_confirm(&self) -> bool {
        !matches!(self.state, State::Failed(_) | State::Ended(_))
            && self.store.settings().general.confirm_close
    }

    /// 连的是什么（测试中检查打开的会话）。
    #[cfg(test)]
    pub(crate) fn session(&self) -> &Session {
        &self.session
    }

    /// 本地终端或 WSL：关闭时结束的是其中运行的程序，而不是断开连接。
    pub fn is_local_session(&self) -> bool {
        !self.session.is_ssh()
    }

    /// 关闭这个 Tab：需要时先确认。
    pub fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.needs_close_confirm() {
            cx.emit(TerminalPanelEvent::Close);
            return;
        }
        let description = if self.is_local_session() {
            t!("close.tab_description_local")
        } else {
            t!("close.tab_description")
        };
        let this = cx.entity().downgrade();
        crate::close_confirm::confirm_tab_with(
            self.name.clone(),
            description,
            move |_, cx| {
                let _ = this.update(cx, |_, cx| cx.emit(TerminalPanelEvent::Close));
            },
            window,
            cx,
        );
    }

    /// 不再确认、直接关闭（一次关闭几个 Tab 时已经汇总确认过，见 [`crate::workspace::Workspace::close_tabs`]）。
    pub(crate) fn close_tab(&mut self, cx: &mut Context<Self>) {
        cx.emit(TerminalPanelEvent::Close);
    }

    fn on_close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        self.request_close(window, cx);
    }

    /// 粘贴（快捷键、右键、菜单都经过这里，先于终端视图处理）：文字先做粘贴前检查，截图上传后粘贴路径。
    /// macOS 编辑菜单的拷贝、粘贴、全选是输入框的编辑动作（[`crate::app_menu`]）：焦点在终端上时换成终端自己的动作
    /// 发出去（粘贴照样经过 [`Self::on_paste`] 的确认）；焦点在查找栏等输入框上时由输入框自己处理，到不了这里。
    fn on_menu_edit(
        &mut self,
        action: Box<dyn gpui_kit::Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &self.view {
            Some(view) if view.focus_handle(cx).contains_focused(window, cx) => {
                window.dispatch_action(action, cx)
            }
            _ => cx.propagate(),
        }
    }

    fn on_paste(
        &mut self,
        _: &termlet_terminal::view::Paste,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        if let Some(text) = item.text() {
            self.paste_text(&text, window, cx);
            return;
        }
        let image = item.entries().iter().find_map(|entry| match entry {
            gpui_kit::ClipboardEntry::Image(image) => Some(image.clone()),
            _ => None,
        });
        if let Some(image) = image {
            self.paste_image(image, window, cx);
        }
    }

    /// 粘贴文字：控制字符去掉；需要时先确认（见 [`crate::paste_guard`]）。
    pub fn paste_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.view.clone() else {
            return;
        };
        // 同步输入：由工作区对全部同步中的终端检查一次再分发
        if self.sync_member {
            cx.emit(TerminalPanelEvent::SyncPaste(text.to_string()));
            return;
        }
        let settings = self.store.settings();
        let (guard, _) = termlet_core::guard::Guard::from_settings(&settings.guard);
        let view_ref = view.read(cx);
        let context = termlet_core::guard::PasteContext {
            bracketed: view_ref.bracketed_paste(),
            full_screen: view_ref.is_alt_screen(),
            warn: settings.terminal.paste_warning,
            targets: 1,
        };
        let check = termlet_core::guard::inspect_paste(&guard, text, &context);
        if check.needs_confirm() {
            crate::paste_guard::confirm(check, cx.entity().downgrade(), window, cx);
        } else {
            self.paste_checked(&check.text, window, cx);
        }
    }

    /// 粘贴已检查（或确认）过的文字，焦点回到终端。
    pub fn paste_checked(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.view.clone() else {
            return;
        };
        view.update(cx, |view, cx| view.paste_text(text, cx));
        view.focus_handle(cx).focus(window, cx);
    }

    /// 粘贴截图：本地终端存到本机的临时目录，SSH 终端上传到服务器的 `~/.cache/termlet/paste`，然后粘贴路径。
    fn paste_image(&mut self, image: gpui_kit::Image, window: &mut Window, cx: &mut Context<Self>) {
        use termlet_core::paste_image::{self, PathStyle};
        enum Place {
            Local(PathStyle),
            Remote(Arc<Connection>),
        }
        let place = match &self.session {
            Session::Local { profile } => Place::Local(match profile.kind {
                termlet_core::localshell::ShellKind::GitBash
                | termlet_core::localshell::ShellKind::Msys2
                | termlet_core::localshell::ShellKind::Cygwin
                | termlet_core::localshell::ShellKind::Unix => PathStyle::Posix,
                _ if cfg!(windows) => PathStyle::Windows,
                _ => PathStyle::Posix,
            }),
            Session::Wsl { .. } => Place::Local(PathStyle::Wsl),
            _ => {
                // 只用一个通道的主机（堡垒机）不能再开 SFTP
                if self.spec.as_ref().is_some_and(|spec| spec.single_channel) {
                    crate::notify::warning(t!("terminal.paste_image.single_channel"), window, cx);
                    return;
                }
                match self.connection.clone() {
                    Some(connection) => Place::Remote(connection),
                    None => return,
                }
            }
        };
        let bytes = image.bytes().to_vec();
        let is_bmp = image.format() == gpui_kit::ImageFormat::Bmp;
        self._paste_image = Some(cx.spawn_in(window, async move |this, cx| {
            // 截图在 Windows 上常是 BMP（剪贴板的 CF_DIB）：先转成 PNG
            let converted = cx
                .background_executor()
                .spawn(async move { if is_bmp { to_png(&bytes) } else { Ok(bytes) } })
                .await;
            let result: Result<String, String> = match converted {
                Err(e) => Err(e),
                Ok(png) => match place {
                    Place::Local(style) => {
                        cx.background_executor()
                            .spawn(async move {
                                paste_image::save_local(&paste_image::local_dir(), &png)
                                    .map(|path| paste_image::local_path_text(&path, style))
                                    .map_err(|e| crate::store_errors::describe(&e).to_string())
                            })
                            .await
                    }
                    Place::Remote(connection) => {
                        let job = termlet_core::runtime::spawn(async move {
                            paste_image::upload(&connection, &png).await
                        });
                        match job.await {
                            Ok(Ok(path)) => Ok(paste_image::remote_path_text(&path)),
                            Ok(Err(e)) => Err(crate::store_errors::describe(&e).to_string()),
                            Err(e) => Err(e.to_string()),
                        }
                    }
                },
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this._paste_image = None;
                match result {
                    Ok(text) => this.paste_checked(&text, window, cx),
                    Err(error) => crate::notify::error(
                        t!("terminal.paste_image.failed", error = error),
                        window,
                        cx,
                    ),
                }
            });
        }));
    }

    /// 命令历史属于谁：保存的主机为主机 ID，快速连接为 `quick:用户@地址:端口`，本地终端为 `local:<shell ID>`。
    pub fn history_owner(&self) -> String {
        match &self.session {
            Session::Host { host } => host.as_str().to_string(),
            Session::Quick { target } => {
                termlet_core::history::quick_key(&target.username, &target.host, target.port)
            }
            Session::Local { profile } => termlet_core::history::local_key(&profile.id),
            Session::Wsl { distro, user, .. } => {
                termlet_core::history::local_key(&crate::workspaces::wsl_key(distro, user))
            }
            // 不记命令历史：为空
            Session::Launch { .. } => String::new(),
        }
    }

    /// 能不能写入（会话还在）。
    pub fn can_write(&self, cx: &App) -> bool {
        self.view
            .as_ref()
            .is_some_and(|view| view.read(cx).closed().is_none())
    }

    /// 把快捷命令、历史命令写入终端（见 [`crate::snippets::command_input`]）。多行命令只写入而终端没有开启
    /// 括号粘贴时不写，返回说明。
    pub fn write_command(
        &mut self,
        input: crate::snippets::CommandInput,
        cx: &mut Context<Self>,
    ) -> Result<(), SharedString> {
        let Some(view) = self.view.clone() else {
            return Err(t!("snippets.no_terminal"));
        };
        if view.read(cx).closed().is_some() {
            return Err(t!("snippets.no_terminal"));
        }
        match input {
            crate::snippets::CommandInput::Type(data) => {
                view.update(cx, |view, cx| view.write_input(data.as_bytes(), cx))
            }
            crate::snippets::CommandInput::Paste(text) => {
                if !view.read(cx).bracketed_paste() {
                    return Err(t!("snippets.multiline_insert"));
                }
                view.update(cx, |view, cx| view.paste_text(&text, cx))
            }
        }
        Ok(())
    }

    /// 侧边栏主机行上显示的状态。
    pub fn status(&self) -> SessionStatus {
        match &self.state {
            State::Connected if self.stalled => SessionStatus::Connecting,
            State::Connected => SessionStatus::Connected,
            State::Connecting | State::Waiting { .. } | State::Reconnecting { .. } => {
                SessionStatus::Connecting
            }
            State::Failed(_) | State::Ended(_) => SessionStatus::Disconnected,
        }
    }

    /// （重新）建立会话。`Attempt::Forced`：会话还在时重新启动，新的建好前旧的照常可用，失败时保留旧的。
    fn start(&mut self, attempt: Attempt, window: &mut Window, cx: &mut Context<Self>) {
        let forced = matches!(attempt, Attempt::Forced) && matches!(self.state, State::Connected);
        match self.session.clone() {
            Session::Local { profile } => {
                // 读取环境变量、创建 ConPTY 与进程放到后台线程：在界面线程上创建时 Windows 会在其间回调窗口，
                // 而那时应用正被这次点击占用
                let size = self.grid_size(cx);
                if !forced {
                    self.state = State::Connecting;
                }
                let job = cx.background_executor().spawn(async move {
                    local_shell::launch(&profile)
                        .map_err(|e| local_shell::describe_error(&e))
                        .and_then(|launch| {
                            localpty::spawn(launch, size).map_err(|e| {
                                tracing::error!("打开本地终端失败：{e}");
                                t!("terminal.open_failed", error = e.to_string())
                            })
                        })
                });
                self._connect = Some(cx.spawn_in(window, async move |this, cx| {
                    let opened = job.await;
                    let _ = this.update_in(cx, |this, window, cx| {
                        this._connect = None;
                        match opened {
                            Ok(io) => {
                                tracing::info!(session = %this.name, restarted = forced, "本地终端已启动");
                                this.note_restarted(cx);
                                this.state = State::Connected;
                                this.mark_connected(false);
                                let io = this.wrap_io(io, false, cx);
                                this.attach(io, window, cx);
                            }
                            Err(message) => this.on_start_failed(message, forced, window, cx),
                        }
                        cx.notify();
                    });
                }));
            }
            Session::Wsl { distro, user, dir } => {
                // 与本地终端相同：读取环境变量、创建 ConPTY 放到后台线程
                let size = self.grid_size(cx);
                let key = self.identity.key.clone();
                if !forced {
                    self.state = State::Connecting;
                }
                let job = cx.background_executor().spawn(async move {
                    termlet_core::wsl::open_terminal(&distro, &user, dir.as_deref(), &key, size)
                        .map_err(|e| crate::wsl_errors::describe(&e))
                });
                self._connect = Some(cx.spawn_in(window, async move |this, cx| {
                    let opened = job.await;
                    let _ = this.update_in(cx, |this, window, cx| {
                        this._connect = None;
                        match opened {
                            Ok(io) => {
                                tracing::info!(session = %this.name, restarted = forced, "WSL 终端已启动");
                                this.note_restarted(cx);
                                this.state = State::Connected;
                                this.mark_connected(false);
                                let io = this.wrap_io(io, false, cx);
                                this.transport = Some(io.input.clone());
                                this.attach(io, window, cx);
                                // 主机工具在新 Tab 中执行的命令
                                if this.run_once.is_some() {
                                    this.schedule_startup(TmuxStatus::Off, cx);
                                }
                                let target = match &this.session {
                                    Session::Wsl { distro, user, .. } => {
                                        Some(Target::wsl(distro, user))
                                    }
                                    _ => None,
                                };
                                if let Some(target) = target {
                                    this.start_monitor(target, cx);
                                }
                            }
                            Err(message) => this.on_start_failed(message, forced, window, cx),
                        }
                        cx.notify();
                    });
                }));
            }
            Session::Host { .. } | Session::Quick { .. } | Session::Launch { .. } => {
                self.connect(attempt, window, cx)
            }
        }
        cx.notify();
    }

    /// 本地 shell、WSL 重新启动（已有画面时）：新的 shell 从普通状态开始，会话日志也从普通状态接着记。
    /// 在接上新的传输之前调用。
    fn note_restarted(&self, cx: &App) {
        let Some(view) = &self.view else {
            return;
        };
        let engine = view.read(cx).engine();
        engine.reset_modes();
        engine.inject_notice(&t!("terminal.notice.restarted"));
        if let Some(log) = self.tap.log() {
            log.reset(false);
            log.note(&t!("terminal.log.restarted"));
        }
    }

    /// 本地 shell、WSL 启动失败：强制重新启动时原来的会话还在就保持原样。
    fn on_start_failed(
        &mut self,
        message: SharedString,
        forced: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::warn!(session = %self.name, kind = self.log_kind(), forced, "终端启动失败：{message}");
        if forced && matches!(self.state, State::Connected) {
            crate::notify::error(t!("terminal.restart_failed", error = message), window, cx);
        } else {
            self.state = State::Failed(Failure::Local(message));
        }
    }

    fn grid_size(&self, cx: &App) -> GridSize {
        self.view
            .as_ref()
            .map(|view| view.read(cx).engine().size())
            .unwrap_or_else(|| GridSize::new(80, 24))
    }

    fn spec(&self) -> Result<TerminalSpec, SshError> {
        let mut spec = match &self.session {
            Session::Host { host } => TerminalSpec::for_host(&self.store, host),
            Session::Quick { target } => Ok(TerminalSpec::for_quick(&self.store, target)),
            Session::Launch { target } => Ok(crate::launch::spec(&self.store, target)),
            Session::Local { .. } | Session::Wsl { .. } => {
                Err(SshError::Config("不是 SSH 会话".into()))
            }
        }?;
        // 在这个 Tab 中切换过编码：重连后的新 shell 也用它
        if let Some(encoding) = self.encoding {
            spec.encoding = encoding;
        }
        Ok(spec)
    }

    /// 连接 SSH：在核心的运行时上连接（或共用连接组里的连接）、打开终端，完成后接到视图上。
    fn connect(&mut self, attempt: Attempt, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_raw() {
            self.connect_raw(attempt, window, cx);
            return;
        }
        let spec = match self.spec() {
            Ok(spec) => spec,
            Err(error) => {
                tracing::warn!(session = %self.name, "连接参数有误：{error}");
                self.state = State::Failed(Failure::Ssh(error));
                return;
            }
        };
        let target = spec.plan.target();
        tracing::debug!(
            session = %self.name,
            kind = self.log_kind(),
            ?attempt,
            address = %target.address,
            port = target.port,
            user = %target.username,
            hops = spec.plan.hops.len(),
            tmux = spec.tmux,
            single_channel = spec.single_channel,
            "开始连接"
        );
        let identity = match attempt {
            Attempt::Auto(n) => {
                self.state = State::Reconnecting { attempt: n };
                self.identity.clone()
            }
            // 不再与别的终端共用旧连接（连上后才换组，失败时 SFTP、复制会话仍用原来的连接）；
            // 连上之前旧的会话照常可用
            Attempt::Forced => self.identity.with_new_group(),
            Attempt::Open => {
                self.state = State::Connecting;
                self.identity.clone()
            }
        };
        self.spec = Some(spec.clone());
        let store = self.store.clone();
        let new_identity = identity.clone();
        let size = self.grid_size(cx);
        let prompter = self.prompter.clone();
        let job = termlet_core::runtime::spawn(async move {
            ssh::open_terminal(store, &spec, &identity, size, prompter).await
        });
        self._connect = Some(cx.spawn_in(window, async move |this, cx| {
            let result = match job.await {
                Ok(result) => result,
                Err(e) => Err(SshError::Protocol(format!("连接任务异常结束：{e}"))),
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this._connect = None;
                match result {
                    Ok(opened) => {
                        this.identity = new_identity;
                        this.on_connected(opened, window, cx)
                    }
                    Err(error) => this.on_connect_failed(error, attempt, window, cx),
                }
                cx.notify();
            });
        }));
    }

    /// 连接 Telnet、串口：在核心的运行时上打开，完成后接到视图上。Telnet 经跳板机时可能要询问。
    fn connect_raw(&mut self, attempt: Attempt, window: &mut Window, cx: &mut Context<Self>) {
        let Session::Host { host } = &self.session else {
            return;
        };
        let mut spec = match RawSpec::for_host(&self.store, host) {
            Ok(spec) => spec,
            Err(error) => {
                tracing::warn!(session = %self.name, "连接参数有误：{error}");
                self.state = State::Failed(Failure::Raw(error));
                return;
            }
        };
        tracing::debug!(session = %self.name, kind = self.log_kind(), ?attempt, "开始连接");
        // 在这个 Tab 中切换过编码：重连后沿用
        if let Some(encoding) = self.encoding {
            spec.config.encoding = encoding;
        }
        let forced = matches!(attempt, Attempt::Forced) && matches!(self.state, State::Connected);
        match attempt {
            Attempt::Auto(n) => self.state = State::Reconnecting { attempt: n },
            // 强制重连（Telnet）：连上之前旧的会话照常可用
            Attempt::Forced if forced => {}
            Attempt::Open | Attempt::Forced => self.state = State::Connecting,
        }
        let store = self.store.clone();
        let size = self.grid_size(cx);
        let prompter = self.prompter.clone();
        let options = ssh::connect_options(&store.settings());
        let job = termlet_core::runtime::spawn(async move {
            rawterm::open(store, spec, size, prompter, &options).await
        });
        self._connect = Some(cx.spawn_in(window, async move |this, cx| {
            let result = match job.await {
                Ok(result) => result,
                Err(e) => Err(RawError::Connect(SshError::Protocol(format!(
                    "连接任务异常结束：{e}"
                )))),
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this._connect = None;
                match result {
                    Ok(raw) => {
                        tracing::info!(
                            session = %this.name,
                            kind = this.log_kind(),
                            ?attempt,
                            reconnected = this.view.is_some(),
                            "已连接"
                        );
                        let io = this.wrap_io(raw.io, false, cx);
                        this.transport = Some(io.input.clone());
                        if let Some(view) = &this.view {
                            let engine = view.read(cx).engine();
                            engine.reset_modes();
                            engine.inject_notice(&t!("terminal.notice.reconnected"));
                            if let Some(log) = this.tap.log() {
                                log.reset(false);
                                log.note(&t!("terminal.log.reconnected"));
                            }
                        }
                        this.state = State::Connected;
                        this.mark_connected(false);
                        this.attach(io, window, cx);
                    }
                    Err(error) => {
                        tracing::warn!(session = %this.name, ?attempt, "连接失败：{error}");
                        match attempt {
                            Attempt::Auto(n) if error.is_retryable() => {
                                this.schedule_reconnect(n + 1, None, window, cx)
                            }
                            // 强制重连失败：原来的会话还在就保持原样
                            Attempt::Forced if forced && matches!(this.state, State::Connected) => {
                                crate::notify::error(
                                    t!(
                                        "terminal.reconnect_failed",
                                        error = crate::raw_errors::describe(&error)
                                    ),
                                    window,
                                    cx,
                                );
                            }
                            _ => this.state = State::Failed(Failure::Raw(error)),
                        }
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn on_connected(
        &mut self,
        opened: OpenedTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let OpenedTerminal {
            connection,
            io,
            tmux,
            x11,
        } = opened;
        // 先经文件传输的检测包装：sudo 密码填入等直接写远端的地方也用包装后的输入（传输期间被拦下）
        let io = self.wrap_io(io, tmux == TmuxStatus::Attached, cx);
        // 强制重连换了连接：原来连接上借出的本机代理不再属于这个会话
        self.end_proxy_lend(cx);
        self.transport = Some(io.input.clone());
        self.watch_password();
        if let Some(log) = self.tap.log()
            && self.view.is_some()
        {
            log.reset(false);
            log.note(&t!("terminal.log.reconnected"));
        }
        self.schedule_startup(tmux, cx);
        self.start_latency(connection.clone(), cx);
        let single_channel = self.spec.as_ref().is_some_and(|spec| spec.single_channel);
        tracing::info!(
            session = %self.name,
            kind = self.log_kind(),
            ?tmux,
            single_channel,
            reconnected = self.view.is_some(),
            "已连接"
        );
        if single_channel {
            self._monitor = None;
            self.resources = if self.store.settings().terminal.monitor {
                Resources::SingleChannel
            } else {
                Resources::Off
            };
        } else {
            self.start_monitor(Target::ssh(connection.clone(), false), cx);
            self.detect_os(connection.clone(), cx);
        }
        if let Some(view) = &self.view {
            let engine = view.read(cx).engine();
            // 新的 shell 从普通状态开始；回到 tmux 会话时由 tmux 重绘整个画面
            if tmux != TmuxStatus::Attached {
                engine.reset_modes();
            }
            engine.inject_notice(&if tmux == TmuxStatus::Attached {
                t!("terminal.notice.reconnected_tmux")
            } else {
                t!("terminal.notice.reconnected")
            });
        }
        self.tmux = tmux;
        self.cwd_reader = None;
        self.state = State::Connected;
        self.mark_connected(matches!(x11, X11Status::Forwarding { .. }));
        self.stalled = false;
        self.watch_health(&connection, window, cx);
        self._x11 = matches!(x11, X11Status::Forwarding { .. })
            .then(|| self.watch_x11(&connection, window, cx));
        self.connection = Some(connection);
        self.attach(io, window, cx);
        let notice = match tmux {
            TmuxStatus::Unavailable => Some(t!("terminal.notice.tmux_unavailable")),
            TmuxStatus::Unchecked => Some(t!("terminal.notice.tmux_unchecked")),
            TmuxStatus::Off | TmuxStatus::Attached => None,
        };
        if let (Some(notice), Some(view)) = (notice, &self.view) {
            view.read(cx).engine().inject_notice(&notice);
        }
        if let (Some(notice), Some(view)) = (x11_status_notice(&x11), &self.view) {
            view.read(cx).engine().inject_notice(&notice);
        }
        self.warn_certificate_expiry(cx);
    }

    /// 用证书登录而证书快过期（剩不到一小时）：在终端中提醒一次（同一张证书重连后不再提醒）；经过的跳板机用的证书
    /// 快过期时同样提醒，写明是哪一跳。
    fn warn_certificate_expiry(&mut self, cx: &mut Context<Self>) {
        let (Some(connection), Some(view)) = (&self.connection, self.view.clone()) else {
            return;
        };
        let now = termlet_core::sshcert::now_secs();
        let expiring = expiring_certificates(
            connection.jump_certificate_expiries(),
            connection.certificate_expiry(),
            &self.certificate_warned,
            now,
        );
        for (jump, expiry) in expiring {
            let minutes = termlet_core::sshcert::minutes_left(expiry, now);
            let time = crate::ssh_errors::cert_time(expiry);
            tracing::info!(session = %self.name, jump = jump.is_some(), minutes, "登录用的证书快过期，提醒");
            let notice = match &jump {
                Some(jump) => t!(
                    "terminal.notice.jump_certificate_expiring",
                    jump = jump,
                    time = time,
                    minutes = minutes
                ),
                None => t!(
                    "terminal.notice.certificate_expiring",
                    time = time,
                    minutes = minutes
                ),
            };
            view.read(cx).engine().inject_notice(&notice);
            self.certificate_warned.push((jump, expiry));
        }
    }

    /// X11 转发中的说明（第一个 X 程序连不上本机的 X 服务器时等）：显示在终端中。
    fn watch_x11(
        &self,
        connection: &Arc<Connection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let mut notices = connection.x11_notices();
        cx.spawn_in(window, async move |this, cx| {
            while notices.changed().await.is_ok() {
                let Some((_, notice)) = notices.borrow_and_update().clone() else {
                    continue;
                };
                let text = x11_notice(&notice);
                if this
                    .update(cx, |this, cx| {
                        tracing::info!(session = %this.name, ?notice, "X11 转发的说明");
                        if let Some(view) = &this.view {
                            view.read(cx).engine().inject_notice(&text);
                        }
                    })
                    .is_err()
                {
                    return;
                }
            }
        })
    }

    fn on_connect_failed(
        &mut self,
        error: SshError,
        attempt: Attempt,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::warn!(session = %self.name, ?attempt, "连接失败：{error}");
        match attempt {
            // 自动重连：网络类的错误继续重试，其他的（认证失败、指纹变化……）停下来告诉用户
            Attempt::Auto(n) if error.is_retryable() => {
                self.schedule_reconnect(n + 1, Some(error), window, cx)
            }
            // 强制重连失败：原来的会话还在就保持原样
            Attempt::Forced if matches!(self.state, State::Connected) => {
                crate::notify::error(
                    t!(
                        "terminal.reconnect_failed",
                        error = ssh_errors::describe(&error)
                    ),
                    window,
                    cx,
                );
            }
            _ => self.state = State::Failed(Failure::Ssh(error)),
        }
    }

    /// 等待后进行第 `attempt` 次自动重连。
    fn schedule_reconnect(
        &mut self,
        attempt: u32,
        last_error: Option<SshError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delay = ssh::reconnect_delay(attempt);
        tracing::debug!(
            session = %self.name,
            attempt,
            delay_ms = delay.as_millis() as u64,
            "等待自动重连"
        );
        self.state = State::Waiting {
            attempt,
            last_error,
        };
        self.connection = None;
        self._health = None;
        self.stalled = false;
        self._connect = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if matches!(this.state, State::Waiting { .. }) {
                    this.connect(Attempt::Auto(attempt), window, cx);
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    /// 连接是否有响应：跟着更新顶部的提示。
    fn watch_health(
        &mut self,
        connection: &Arc<Connection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut health = connection.health();
        self._health = Some(cx.spawn_in(window, async move |this, cx| {
            while health.changed().await.is_ok() {
                let stalled = *health.borrow_and_update() == Health::Stalled;
                if this
                    .update(cx, |this, cx| {
                        if this.stalled != stalled {
                            if stalled {
                                tracing::info!(session = %this.name, "连接没有响应，等待恢复");
                            } else {
                                tracing::info!(session = %this.name, "连接恢复响应");
                            }
                        }
                        this.stalled = stalled;
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
        }));
    }

    /// 系统从睡眠中唤醒：等待重连的立即重连，连接着的立即探测一次（尽快发现断线）。
    pub fn on_resume(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        tracing::debug!(session = %self.name, "系统从睡眠中唤醒");
        match &self.state {
            State::Waiting { attempt, .. } => {
                let attempt = *attempt;
                self.connect(Attempt::Auto(attempt), window, cx);
                cx.notify();
            }
            State::Connected => {
                if let Some(connection) = &self.connection {
                    connection.probe();
                }
            }
            _ => {}
        }
    }

    /// 每次连接的字节流先经文件传输的检测包装（本地终端经过 ConPTY，ZModem 不能用；tmux 中同样不能用）。
    fn wrap_io(&self, io: TerminalIo, tmux: bool, cx: &App) -> TerminalIo {
        let conpty =
            cfg!(windows) && matches!(self.session, Session::Local { .. } | Session::Wsl { .. });
        let io = self
            .transfer_bar
            .read(cx)
            .interceptor()
            .wrap(io, LinkInfo { conpty, tmux });
        // 录制要知道终端大小的变化
        TerminalIo {
            output: io.output,
            input: self.tap.wrap_input(io.input),
        }
    }

    /// 把会话接到终端视图上：第一次时创建视图，重连时接到原来的视图（旧的传输随之关闭）。
    fn attach(&mut self, io: TerminalIo, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = &self.view {
            // 换了传输（强制重连、重新启动）：进行中的那次执行写的是旧的传输，按“已关闭”结束
            self.run_closed = None;
            view.update(cx, |view, cx| view.attach(io, cx));
            self.auto_record(window, cx);
            return;
        }
        let settings = self.store.settings();
        let engine = TerminalEngine::new(
            io,
            EngineOptions {
                scrollback: settings.terminal.scrollback as usize,
                size: GridSize::new(80, 24),
            },
        );
        let style = self.style(cx);
        let forward = self.sync_member;
        let view = cx.new(|cx| {
            let mut view = TerminalView::new(engine, style, window, cx);
            view.set_forward_input(forward);
            view
        });
        self._view_subscription = Some(cx.subscribe_in(
            &view,
            window,
            |this, _, event, window, cx| match event {
                TerminalViewEvent::Output => {
                    this.last_output = Some(Instant::now());
                    if !this.is_shown()
                        && !this.activity
                        && this.store.settings().terminal.tab_activity
                    {
                        this.activity = true;
                        cx.notify();
                    }
                }
                TerminalViewEvent::Bell => {
                    if !this.is_shown() && !this.bell && this.store.settings().terminal.tab_activity
                    {
                        this.bell = true;
                        cx.notify();
                    }
                }
                TerminalViewEvent::Shell(event) => this.on_shell_event(event.clone(), cx),
                TerminalViewEvent::Typed(data) => {
                    cx.emit(TerminalPanelEvent::SyncTyped(data.clone()))
                }
                TerminalViewEvent::RestartRequested => this.reconnect_now(window, cx),
                TerminalViewEvent::Closed(reason) => this.on_closed(reason.clone(), window, cx),
                TerminalViewEvent::TitleChanged => cx.notify(),
                TerminalViewEvent::SuggestionAccepted => {
                    termlet_core::usage::mark(termlet_core::usage::Feature::CommandSuggest)
                }
                TerminalViewEvent::ClipboardWritten(text) => {
                    let key = cx.entity_id().as_u64() as usize;
                    clipboard::written(this.name.clone(), text, key, window, cx)
                }
                TerminalViewEvent::ClipboardTooLarge(bytes) => {
                    let key = cx.entity_id().as_u64() as usize;
                    clipboard::too_large(this.name.clone(), *bytes, key, window, cx)
                }
                _ => {}
            },
        ));
        if self.active || self.focus.is_focused(window) {
            view.focus_handle(cx).focus(window, cx);
        }
        view.read(cx)
            .engine()
            .set_output_observer(Some(self.tap.observer()));
        self.view = Some(view);
        self.reset_suggestions(cx);
        if settings.terminal.session_log {
            self.start_session_log(window, cx);
        }
        self.auto_record(window, cx);
    }

    // ------------------------------------------------------------ 终端功能

    fn on_shell_event(&mut self, event: ShellEvent, cx: &mut Context<Self>) {
        match event {
            ShellEvent::PromptStart => {
                self.prompt_seen = true;
                self.refresh_suggestions(cx);
            }
            ShellEvent::CommandStart { command } => self.last_command = command.map(|c| c.0),
            ShellEvent::CommandEntered(command) => {
                if !self.store.settings().terminal.command_history {
                    return;
                }
                // 加密后写进数据库：放到后台线程（外部程序调起的临时连接不记），与命令补全提示读历史排成一队
                self.record_history(command.0, cx);
            }
            ShellEvent::CommandFinished {
                exit_code,
                duration,
            } => {
                self.run_command_finished(cx);
                if duration < NOTIFY_COMMAND_AFTER {
                    return;
                }
                let what = self
                    .last_command
                    .take()
                    .map(|c| termlet_core::guard::one_line(&c, 60))
                    .unwrap_or_else(|| self.name.to_string());
                let title = match exit_code {
                    Some(code) if code != 0 => t!("notify.command_failed", command = what),
                    _ => t!("notify.command_done", command = what),
                };
                let took = format_duration(duration);
                let body = match exit_code {
                    Some(code) => t!("notify.took_code", duration = took, code = code),
                    None => t!("notify.took", duration = took),
                };
                cx.emit(TerminalPanelEvent::Notify { title, body });
            }
            ShellEvent::Notification { title, body } => {
                let title = if title.trim().is_empty() {
                    self.name.clone()
                } else {
                    title.into()
                };
                cx.emit(TerminalPanelEvent::Notify {
                    title,
                    body: body.into(),
                });
            }
            // 当前目录：SFTP 跟随终端目录时直接从终端视图读取
            // 跟随这个终端目录的 SFTP 观察着它
            ShellEvent::WorkingDirectory(_) => cx.notify(),
        }
    }

    fn on_tap_event(&mut self, event: TapEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            TapEvent::Password(PromptEvent::Shown) => self.password_prompt = true,
            TapEvent::Password(PromptEvent::Ended) => self.password_prompt = false,
            TapEvent::LogHalted(halt) => {
                let message = match halt {
                    Halt::Full => t!("terminal.log.full"),
                    Halt::Failed(e) => t!(
                        "terminal.log.failed",
                        error = crate::store_errors::describe(&e)
                    ),
                };
                self.tap.set_log(None);
                crate::notify::warning(message, window, cx);
            }
            TapEvent::RecordingHalted(halt) => self.on_recording_halted(halt, window, cx),
        }
        cx.notify();
    }

    /// sudo 密码填入：保存的主机用密码登录、保存了密码，并且设置中开启了填入时，检测登录用户的密码提示。
    fn watch_password(&mut self) {
        let enabled = self.store.settings().terminal.password_fill
            && matches!(self.session, Session::Host { .. });
        let fill = self.spec.as_ref().filter(|_| enabled).and_then(|spec| {
            let target = spec.plan.target();
            target
                .saved_password()
                .map(|password| (target.username.clone(), password.clone()))
        });
        self.tap
            .watch_password(fill.as_ref().map(|(user, _)| user.clone()));
        self.fill_password = fill.map(|(_, password)| password);
        self.password_prompt = false;
    }

    /// 停在 sudo 密码提示上时按 Enter：填入保存的密码（直接写给远端，不算用户的输入）；按别的键表示自己输入。
    /// 只管焦点在终端上时的按键（查找栏等里的 Enter 不算）。
    fn on_key_down_capture(
        &mut self,
        event: &gpui_kit::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 同步输入中 Enter 由各终端自己判断是否填入 sudo 密码（见 deliver_typed）
        if self.sync_member || !self.password_prompt || !self.tap.password_pending() {
            return;
        }
        let on_terminal = self
            .view
            .as_ref()
            .is_some_and(|view| view.focus_handle(cx).is_focused(window));
        if !on_terminal {
            return;
        }
        let m = &event.keystroke.modifiers;
        let plain = !m.control && !m.alt && !m.platform;
        if event.keystroke.key == "enter" && plain && !m.shift {
            cx.stop_propagation();
            self.fill_sudo_password(cx);
        } else if plain {
            self.tap.typed();
            self.password_prompt = false;
            cx.notify();
        }
    }

    /// 正在显示（Dock 中在前台，或在同步输入的网格中）。
    pub fn is_shown(&self) -> bool {
        self.active || self.in_grid
    }

    /// 现在是否采样资源：终端显示着，或者这台主机的概览小窗开着。
    pub(crate) fn samples_now(&self) -> bool {
        self.is_shown() || self.sampling_holds.get() > 0
    }

    /// 主机概览的小窗开着：终端不在前台时也照常采样（间隔与状态条相同；断开时照常停止、重连后继续）。返回的
    /// [`SamplingHold`] 丢弃后恢复原来的规则（终端不在前台时不采样）。
    pub fn keep_sampling(&self) -> SamplingHold {
        self.sampling_holds.set(self.sampling_holds.get() + 1);
        SamplingHold(self.sampling_holds.clone())
    }

    /// 在同步输入的网格中。
    pub fn is_in_grid(&self) -> bool {
        self.in_grid
    }

    /// 同步输入的网格中显示（不在 Dock 的前台也显示）：资源采样、新输出标记按显示处理；不改焦点。
    pub fn set_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        self.in_grid = shown;
        if shown {
            self.activity = false;
            self.bell = false;
        }
        cx.notify();
    }

    /// 加入、退出同步输入。
    pub fn set_sync_member(&mut self, member: bool, cx: &mut Context<Self>) {
        self.sync_member = member;
        if let Some(view) = &self.view {
            view.update(cx, |view, _| view.set_forward_input(member));
        }
        cx.notify();
    }

    pub fn is_sync_member(&self) -> bool {
        self.sync_member
    }

    /// 连接着、可以接收输入（同步输入只发给这样的终端）。
    pub fn is_live(&self, cx: &App) -> bool {
        matches!(self.state, State::Connected)
            && self
                .view
                .as_ref()
                .is_some_and(|view| view.read(cx).closed().is_none())
    }

    /// 远端开启了括号粘贴。
    pub fn bracketed_paste(&self, cx: &App) -> bool {
        self.view
            .as_ref()
            .is_some_and(|view| view.read(cx).bracketed_paste())
    }

    /// 在全屏程序（vim 等）中。
    pub fn is_alt_screen(&self, cx: &App) -> bool {
        self.view
            .as_ref()
            .is_some_and(|view| view.read(cx).is_alt_screen())
    }

    /// 屏幕上显示的文字。
    pub fn screen_text(&self, cx: &App) -> String {
        self.view
            .as_ref()
            .map(|view| view.read(cx).screen_text())
            .unwrap_or_default()
    }

    /// 屏幕上当前的输入行（回车前检查危险命令用）。
    pub fn input_line(&self, cx: &App) -> Option<String> {
        self.view
            .as_ref()
            .and_then(|view| view.read(cx).input_line())
    }

    /// 同步输入分发来的键入：停在 sudo 密码提示上时 Enter 填入这台主机保存的密码，其余按本终端的模式写入。
    pub fn deliver_typed(&mut self, data: &[u8], cx: &mut Context<Self>) {
        let Some(view) = self.view.clone() else {
            return;
        };
        if data == b"\r"
            && self.password_prompt
            && self.tap.password_pending()
            && self.fill_password.is_some()
        {
            self.fill_sudo_password(cx);
            return;
        }
        if self.password_prompt {
            self.tap.typed();
            self.password_prompt = false;
            cx.notify();
        }
        view.update(cx, |view, cx| view.write_typed(data, cx));
    }

    /// 撰写栏发送的命令：写给远端（与用户打字相同，记进命令历史）。
    pub fn send_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(view) = &self.view {
            view.update(cx, |view, cx| view.write_input(text.as_bytes(), cx));
        }
    }

    /// 在这个终端中执行一条命令并取回输出与退出码（批量执行的“已打开的终端”）；`canceled` 完成时停止（向终端发送
    /// Ctrl+C）。
    pub fn run_command(
        &mut self,
        command: String,
        canceled: impl std::future::Future<Output = ()> + Send + 'static,
        cx: &mut Context<Self>,
    ) -> Task<Result<termlet_core::termrun::TerminalOutput, RunError>> {
        if matches!(self.session, Session::Local { .. }) || self.is_raw() {
            return Task::ready(Err(RunError::Unsupported));
        }
        if !self.is_live(cx) {
            return Task::ready(Err(RunError::NotConnected));
        }
        let task = self.run_in_terminal(command, RUN_OUTPUT_LIMIT, canceled, cx);
        cx.background_spawn(async move { task.await.map_err(RunError::Terminal) })
    }

    /// 在这个终端中执行一条命令（见 [`termlet_core::termrun`]；撰写栏、`termlet exec --terminal`）：输出最多 `limit`
    /// 字节，`canceled` 完成时向终端发送 Ctrl+C。没有连接、全屏程序中、正在传输文件、已有一次执行时返回“忙”。
    pub fn run_in_terminal(
        &mut self,
        command: String,
        limit: usize,
        canceled: impl std::future::Future<Output = ()> + Send + 'static,
        cx: &mut Context<Self>,
    ) -> Task<Result<termlet_core::termrun::TerminalOutput, termlet_core::termrun::TerminalError>>
    {
        use termlet_core::termrun::{TermRun, TerminalError};
        let Some(transport) = self.transport.clone().filter(|_| self.is_live(cx)) else {
            return Task::ready(Err(TerminalError::Busy));
        };
        if self.is_alt_screen(cx)
            || self.transfer_bar.read(cx).is_transferring()
            || self.tap.running()
        {
            return Task::ready(Err(TerminalError::Busy));
        }
        tracing::debug!(session = %self.name, limit, "在终端中执行一条命令");
        let run = TermRun::new(limit);
        self.tap.set_run(Some(run.clone()));
        let (closed_tx, closed_rx) = async_channel::bounded::<()>(1);
        self.run_closed = Some(closed_tx);
        let job = run.execute(
            move |bytes| {
                transport.write(bytes);
                true
            },
            &command,
            self.current_encoding(),
            canceled,
            async move {
                let _ = closed_rx.recv().await;
            },
        );
        cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, _| {
                match &result {
                    Ok(output) => tracing::debug!(
                        session = %this.name,
                        exit_code = ?output.exit_code,
                        bytes = output.output.len(),
                        "在终端中执行完成"
                    ),
                    Err(e) => tracing::debug!(session = %this.name, ?e, "在终端中执行没有完成"),
                }
                this.tap.set_run(None);
                this.run_closed = None;
            });
            result
        })
    }

    /// 同步输入分发来的粘贴（已检查过）：按本终端的括号粘贴模式写入。
    pub fn deliver_paste(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(view) = &self.view {
            view.update(cx, |view, cx| view.paste_text(text, cx));
        }
    }

    fn fill_sudo_password(&mut self, cx: &mut Context<Self>) {
        let (Some(password), Some(view)) = (self.fill_password.clone(), self.view.clone()) else {
            return;
        };
        if self.tap.take_fill().is_none() {
            return;
        }
        let input = sudo::fill_input(&password);
        view.read(cx).engine().write(&input);
        tracing::info!(session = %self.name, "已填入保存的 sudo 密码");
        self.password_prompt = false;
        cx.notify();
    }

    /// 是否正在记录会话日志（文件还在建时也算）。
    pub fn is_logging(&self) -> bool {
        self.tap.log().is_some() || self._log_start.is_some()
    }

    /// 开始记录会话日志：在后台建目录与文件。
    pub fn start_session_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_logging() || self.view.is_none() {
            return;
        }
        let local = self.store.local_settings();
        let root = session_log::log_root(&local.logs, &AppState::global(cx).data_dir);
        let title = self.name.to_string();
        let on_halt = self.tap.halt_reporter();
        let start = cx
            .background_executor()
            .spawn(async move { SessionLog::start(&root, &title, on_halt) });
        self._log_start = Some(cx.spawn_in(window, async move |this, cx| {
            let result = start.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(log) => {
                    this._log_start = None;
                    tracing::info!(session = %this.name, path = %log.path().display(), "开始记录会话日志");
                    termlet_core::usage::mark(termlet_core::usage::Feature::SessionLog);
                    let log = Arc::new(log);
                    // 正在全屏程序中：画面不记录，回到 shell 后再记
                    if let Some(view) = &this.view
                        && view.read(cx).is_alt_screen()
                    {
                        log.reset(true);
                    }
                    this.tap.set_log(Some(log));
                    cx.notify();
                }
                Err(e) => {
                    this._log_start = None;
                    cx.notify();
                    crate::notify::error(
                        t!(
                            "terminal.log.failed",
                            error = crate::store_errors::describe(&e)
                        ),
                        window,
                        cx,
                    )
                }
            });
        }));
        cx.notify();
    }

    /// 停止记录会话日志（文件还在建时取消）。
    pub fn stop_session_log(&mut self, cx: &mut Context<Self>) {
        if self.is_logging() {
            tracing::info!(session = %self.name, "停止记录会话日志");
        }
        self._log_start = None;
        self.tap.set_log(None);
        cx.notify();
    }

    /// 登录后执行的命令（主机设置，每行一条）：出现提示符，或输出后 600 毫秒没有新的输出时发送，最多等 15 秒。
    /// 回到原来的 tmux 会话时不发送（命令已经在里面执行过）。
    fn schedule_startup(&mut self, tmux: TmuxStatus, cx: &mut Context<Self>) {
        self._startup = None;
        // 主机工具要执行的命令：只执行一次，代替登录后命令
        let run_once = self.run_once.is_some();
        let lines: Vec<String> = if let Some(command) = self.run_once.take() {
            vec![command]
        } else {
            let Session::Host { host } = &self.session else {
                return;
            };
            if tmux == TmuxStatus::Attached {
                return;
            }
            let Some(record) = self.store.get::<Host>(host) else {
                return;
            };
            record
                .doc
                .terminal
                .startup
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect()
        };
        if lines.is_empty() {
            return;
        }
        self.prompt_seen = false;
        self.last_output = None;
        let started = Instant::now();
        self._startup = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(STARTUP_POLL).await;
                let ready = this.update(cx, |this, _| {
                    this.prompt_seen
                        || this
                            .last_output
                            .is_some_and(|t| t.elapsed() >= STARTUP_QUIET)
                        || started.elapsed() >= STARTUP_TIMEOUT
                });
                match ready {
                    Ok(true) => break,
                    Ok(false) => continue,
                    Err(_) => return,
                }
            }
            let _ = this.update(cx, |this, cx| {
                this._startup = None;
                if let Some(view) = &this.view {
                    // 直接写给远端：不算用户的输入（不进命令历史）
                    let text = format!("{}\r", lines.join("\r"));
                    view.read(cx).engine().write(text.as_bytes());
                }
                if run_once {
                    this.run_sent(cx);
                }
            });
        }));
    }

    /// 底部状态条的网络延迟：终端在前台时每 3 秒测一次往返时间（设置中可以关闭）。
    fn start_latency(&mut self, connection: Arc<Connection>, cx: &mut Context<Self>) {
        self.latency = None;
        if !self.store.settings().terminal.latency {
            self._latency = None;
            return;
        }
        self._latency = Some(cx.spawn(async move |this, cx| {
            loop {
                // 不在前台时不测
                let Ok(active) = this.update(cx, |this, _| this.is_shown()) else {
                    return;
                };
                if active {
                    let ping = connection.clone();
                    let rtt = termlet_core::runtime::spawn(async move { ping.ping().await })
                        .await
                        .ok()
                        .flatten();
                    if this
                        .update(cx, |this, cx| {
                            this.latency = rtt;
                            cx.notify();
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                cx.background_executor().timer(LATENCY_INTERVAL).await;
            }
        }));
    }

    /// SSH 连接成功后（含重连）识别远程系统：设置中开着时，在这条连接上另开通道执行一条只读命令
    /// （[`termlet_core::osprobe`]，同一条连接只执行一次），不经过终端；只用一个通道的主机不调用。识别出来时记下：
    /// 保存的主机写进主机（与原来相同时不写，不算修改主机），快速连接只用于这个 Tab 的图标。认不出、出错、超时时
    /// 什么都不做。
    fn detect_os(&mut self, connection: Arc<Connection>, cx: &mut Context<Self>) {
        if !matches!(self.session, Session::Host { .. } | Session::Quick { .. })
            || !self.store.settings().connection.detect_os
        {
            return;
        }
        self._os_probe = Some(cx.spawn(async move |this, cx| {
            let Some(os) = termlet_core::osprobe::detect(connection).await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.detected_os = Some(os);
                if let Session::Host { host } = &this.session
                    && let Err(e) = this.store.set_host_os(host, Some(os))
                {
                    tracing::debug!(session = %this.name, "记下识别出的系统失败：{e}");
                }
                cx.notify();
            });
        }));
    }

    /// 底部状态条的资源使用情况：终端在前台时每 3 秒采样一次（上一次返回后再计时；设置中可以关闭）；这台主机的概览
    /// 弹出成小窗时（[`Self::keep_sampling`]）终端不在前台也照常采样。每次的结果更新各项的级别、记进历史（概览页的
    /// 曲线）。服务器不是 Linux 时不再采样；只用一个通道时说明原因；其余错误保留上次的数据，下次再试。
    fn start_monitor(&mut self, target: Target, cx: &mut Context<Self>) {
        self._monitor = None;
        if !self.store.settings().terminal.monitor {
            self.resources = Resources::Off;
            return;
        }
        self.resources = Resources::Sampling(None);
        self.monitor.reset();
        let monitor = self.monitor.clone();
        self._monitor = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok(active) = this.update(cx, |this, _| this.samples_now()) else {
                    return;
                };
                if active {
                    let result = monitor.sample(&target).await;
                    let stop = this.update(cx, |this, cx| {
                        let stop = match result {
                            Ok(sample) => {
                                this.levels.update(&sample);
                                this.history.push(&sample);
                                this.resources = Resources::Sampling(Some(Box::new(sample)));
                                false
                            }
                            Err(MonitorError::Unsupported) => {
                                this.resources = Resources::Unsupported;
                                true
                            }
                            Err(MonitorError::Exec(ExecError::SingleChannel)) => {
                                this.resources = Resources::SingleChannel;
                                true
                            }
                            Err(e) => {
                                tracing::debug!("资源采样失败：{e}");
                                false
                            }
                        };
                        cx.notify();
                        stop
                    });
                    if stop.unwrap_or(true) {
                        return;
                    }
                }
                cx.background_executor()
                    .timer(termlet_core::monitor::SAMPLE_INTERVAL)
                    .await;
            }
        }));
    }

    /// 当前的编码（SSH、Telnet、串口会话）。
    fn current_encoding(&self) -> Encoding {
        self.encoding
            .or_else(|| self.spec.as_ref().map(|spec| spec.encoding))
            .or_else(|| {
                self.host()
                    .and_then(|id| self.store.get::<Host>(id))
                    .map(|r| r.doc.terminal.encoding)
            })
            .unwrap_or_default()
    }

    /// 切换远端的编码：立即生效，重连后的新 shell 也用它。
    pub fn set_encoding(&mut self, encoding: Encoding, cx: &mut Context<Self>) {
        tracing::info!(session = %self.name, ?encoding, "切换编码");
        self.encoding = Some(encoding);
        if let (Some(transport), Some((_, name, _))) = (
            &self.transport,
            ENCODINGS.iter().find(|(e, _, _)| *e == encoding),
        ) {
            transport.set_encoding(name);
        }
        cx.notify();
    }

    /// 底部状态条：网络延迟、资源使用情况与编码。SSH 会话都有；WSL 只有资源；Telnet、串口只有编码。
    /// 延迟与资源所在的区域点击后在主机工具中打开概览页，右键切换完整 / 精简（见 [`monitor_bar`]）。
    fn render_status_bar(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !matches!(self.state, State::Connected) {
            return None;
        }
        let ssh = self.is_ssh();
        let show_latency = ssh && self.store.settings().terminal.latency;
        let has_encoding = ssh || self.is_raw();
        let has_summary = show_latency || self.resources.shown();
        if !has_summary && !has_encoding {
            return None;
        }
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let border = theme.border;
        let id = cx.entity_id().as_u64() as usize;
        // 状态条的右键菜单、编码的下拉菜单开着时按钮不挂悬停提示（会浮在菜单上，见 crate::menu_tooltip）
        let menu_open = crate::menu_tooltip::menu_open(window, cx);
        let encoding_button = has_encoding.then(|| {
            let encoding = self.current_encoding();
            let label = ENCODINGS
                .iter()
                .find(|(e, _, _)| *e == encoding)
                .map(|(_, _, label)| *label)
                .unwrap_or("UTF-8");
            let this = cx.entity().downgrade();
            Button::new(("encoding", id))
                .ghost()
                .xsmall()
                .label(label)
                .when(!menu_open, |this| this.tooltip(t!("terminal.encoding")))
                .dropdown_menu(move |menu, _, _| {
                    ENCODINGS.iter().fold(menu, |menu, (value, _, label)| {
                        let this = this.clone();
                        let value = *value;
                        menu.item(
                            gpui_kit::component::menu::PopupMenuItem::new(*label)
                                .checked(value == encoding)
                                .on_click(move |_, _, cx| {
                                    let _ =
                                        this.update(cx, |this, cx| this.set_encoding(value, cx));
                                }),
                        )
                    })
                })
        });
        let summary = has_summary.then(|| self.render_monitor_summary(show_latency, window, cx));
        let proxy_button = self.proxy.as_ref().map(|lend| {
            let terminal = cx.entity();
            Button::new(("proxy-lend", id))
                .ghost()
                .xsmall()
                .icon(IconName::Globe)
                .label(t!("terminal.proxy.bar", port = lend.share.port()))
                .when(!menu_open, |this| {
                    this.tooltip(t!(
                        "terminal.proxy.bar_tooltip",
                        port = lend.share.port(),
                        local = lend.share.local().display()
                    ))
                })
                .on_click(move |_, window, cx| {
                    crate::proxy_share::open(terminal.clone(), window, cx)
                })
        });
        Some(
            h_flex()
                .h(rems(1.5))
                .flex_none()
                .text_xs()
                .text_color(muted)
                .border_t_1()
                .border_color(border)
                .children(summary)
                .when(!has_summary, |el| el.child(div().flex_1()))
                .when_some(proxy_button, |el, button| {
                    el.child(
                        div()
                            .h_full()
                            .flex()
                            .items_center()
                            .px_1()
                            .border_l_1()
                            .border_color(border)
                            .child(button),
                    )
                })
                .when_some(encoding_button, |el, button| {
                    el.child(
                        div()
                            .h_full()
                            .flex()
                            .items_center()
                            .px_1()
                            .border_l_1()
                            .border_color(border)
                            .child(button),
                    )
                })
                .into_any_element(),
        )
    }

    /// 状态条上延迟与资源的区域：按量出的宽度显示前几项（[`BarFit`]）；点它打开主机工具的概览，右键切换样式。
    fn render_monitor_summary(
        &self,
        show_latency: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = cx.entity_id().as_u64() as usize;
        let style = self.store.settings().terminal.monitor_bar;
        let latency = show_latency.then(|| self.latency.map(|rtt| rtt.as_millis()));
        let items = monitor_bar::items(latency, &self.resources, self.levels, &self.history, style);
        let fit = self.bar_fit.current();
        // 精简样式只有数字，不带进度条与网速的小柱
        let meters = fit.meters && style == termlet_core::model::MonitorBar::Full;
        let gap = rems(0.75).to_pixels(window.rem_size());
        let shown: Vec<AnyElement> = items
            .iter()
            .take(fit.count)
            .map(|item| monitor_bar::item(item, meters, id, cx))
            .collect();
        let message = self.resources.message();
        let hover = cx.theme().secondary_hover;
        let this = cx.entity().downgrade();
        let area = h_flex()
            .id(("monitor-bar", id))
            .test_support()
            .flex_1()
            .min_w_0()
            .h_full()
            .pl_2()
            .hover(|s| s.bg(hover))
            .on_click(cx.listener(|_, _, _, cx| cx.emit(TerminalPanelEvent::OpenOverview)))
            .child(
                h_flex()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .gap(gap)
                    .overflow_hidden()
                    .children(shown)
                    .children(message.map(|message| {
                        div()
                            .min_w_0()
                            .truncate()
                            .child(crate::prose::prose(message))
                    }))
                    .child(self.bar_fit.measure_row(&items, cx))
                    .child(self.bar_fit.measure_area(gap)),
            )
            .context_menu(move |menu, _, _| {
                let set = |this: gpui_kit::WeakEntity<TerminalPanel>, style| {
                    move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| {
                        let _ = this.update(cx, |this, _| this.set_monitor_bar(style));
                    }
                };
                menu.item(
                    gpui_kit::component::menu::PopupMenuItem::new(t!("monitor.bar.style_full"))
                        .checked(style == termlet_core::model::MonitorBar::Full)
                        .on_click(set(this.clone(), termlet_core::model::MonitorBar::Full)),
                )
                .item(
                    gpui_kit::component::menu::PopupMenuItem::new(t!("monitor.bar.style_compact"))
                        .checked(style == termlet_core::model::MonitorBar::Compact)
                        .on_click(set(this.clone(), termlet_core::model::MonitorBar::Compact)),
                )
                .separator()
                .item(
                    gpui_kit::component::menu::PopupMenuItem::new(t!("monitor.bar.overview"))
                        .icon(IconName::PanelRightOpen)
                        .on_click({
                            let this = this.clone();
                            move |_, _, cx| {
                                let _ = this
                                    .update(cx, |_, cx| cx.emit(TerminalPanelEvent::OpenOverview));
                            }
                        }),
                )
            });
        area.into_any_element()
    }

    /// 状态条的样式（完整、精简）：与设置中的是同一项，改了就保存（各终端随设置的变化重画）。
    fn set_monitor_bar(&self, style: termlet_core::model::MonitorBar) {
        if let Err(e) = self
            .store
            .update_settings(|s| s.terminal.monitor_bar = style)
        {
            tracing::warn!("保存状态条的样式失败：{e}");
        }
    }

    /// sudo 密码提示条。
    fn render_password_prompt(&self, cx: &mut Context<Self>) -> Option<gpui_kit::AnyElement> {
        if !self.password_prompt {
            return None;
        }
        let theme = cx.theme();
        Some(
            h_flex()
                .px_3()
                .py_1()
                .gap_2()
                .text_sm()
                .bg(theme.muted)
                .child(
                    Icon::new(IconName::KeyRound)
                        .small()
                        .text_color(theme.primary),
                )
                .child(div().flex_1().child(prose(t!("terminal.sudo.prompt"))))
                .child(
                    Button::new("sudo-fill")
                        .primary()
                        .xsmall()
                        .label(t!("terminal.sudo.fill"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.fill_sudo_password(cx);
                            if let Some(view) = &this.view {
                                view.focus_handle(cx).focus(window, cx);
                            }
                        })),
                )
                .child(
                    Button::new("sudo-ignore")
                        .ghost()
                        .xsmall()
                        .label(t!("terminal.sudo.ignore"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.tap.typed();
                            this.password_prompt = false;
                            if let Some(view) = &this.view {
                                view.focus_handle(cx).focus(window, cx);
                            }
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }

    /// 会话结束了：SSH 意外断开时按设置自动重连。
    fn on_closed(&mut self, reason: CloseReason, window: &mut Window, cx: &mut Context<Self>) {
        self.stalled = false;
        self._health = None;
        self._x11 = None;
        // 借出的本机代理属于这个会话：会话结束（含断线）即停止，重连后要用时重新开启
        self.end_proxy_lend(cx);
        self._latency = None;
        self.latency = None;
        self._monitor = None;
        self.resources = Resources::Off;
        self._startup = None;
        self.run_closed = None;
        self.password_prompt = false;
        // 串口强制重连：旧的会话结束了，端口已释放，现在重新打开
        if std::mem::take(&mut self.reopen_after_close) {
            self.connection = None;
            self.start(Attempt::Open, window, cx);
            cx.notify();
            return;
        }
        // 会话结束（含断线后等重连）：录制随之结束，写完文件
        if self.is_recording() {
            let why = match &reason {
                CloseReason::Lost(_) => t!("terminal.record.reason_lost"),
                _ => t!("terminal.record.reason_ended"),
            };
            self.stop_recording(Some(why), window, cx);
        }
        let auto = self.store.settings().connection.auto_reconnect;
        tracing::info!(session = %self.name, kind = self.log_kind(), ?reason, auto_reconnect = auto, "会话结束");
        match &reason {
            // Telnet 网络断开、串口拔掉同样自动重连（串口插回即恢复）
            CloseReason::Lost(detail) if (self.is_ssh() || self.is_raw()) && auto => {
                tracing::info!(session = %self.name, "连接已断开，自动重连：{detail}");
                if let Some(log) = self.tap.log() {
                    log.note(&t!("terminal.log.disconnected"));
                }
                if let Some(view) = &self.view {
                    view.read(cx)
                        .engine()
                        .inject_notice(&t!("terminal.notice.disconnected"));
                }
                self.schedule_reconnect(1, None, window, cx);
            }
            _ => {
                self.connection = None;
                self.state = State::Ended(reason);
            }
        }
        cx.notify();
    }

    /// 现在点“重新连接”会立即重连（会话已结束、连接失败、正在等下一次重连）；正在连接、重连中、连接着时为 `false`。
    pub fn can_reconnect_now(&self) -> bool {
        matches!(
            self.state,
            State::Waiting { .. } | State::Failed(_) | State::Ended(_)
        )
    }

    /// 从侧边栏切过来时：会话已断开就重新连接。
    pub fn ensure_connected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reconnect_now(window, cx);
    }

    /// 立即重新连接（会话已结束、连接失败、正在等待重连时）。
    fn reconnect_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(
            self.state,
            State::Waiting { .. } | State::Failed(_) | State::Ended(_)
        ) {
            tracing::info!(session = %self.name, kind = self.log_kind(), "立即重新连接");
        }
        match &self.state {
            State::Connecting | State::Reconnecting { .. } | State::Connected => {}
            State::Waiting { attempt, .. } => {
                let attempt = *attempt;
                self.connect(Attempt::Auto(attempt), window, cx);
            }
            State::Failed(_) | State::Ended(_) => {
                self.connection = None;
                self.start(Attempt::Open, window, cx);
            }
        }
        cx.notify();
    }

    /// Ctrl+Shift+R：会话还在时先确认，再用新的连接重连（本地终端重新启动）。
    fn on_reconnect_action(&mut self, _: &Reconnect, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.state, State::Connected) {
            self.reconnect_now(window, cx);
            return;
        }
        let (title, description) = if self.is_ssh() {
            (
                t!("terminal.confirm_reconnect.title", name = self.name.clone()),
                if self.tmux == TmuxStatus::Attached {
                    t!("terminal.confirm_reconnect.description_tmux")
                } else {
                    t!("terminal.confirm_reconnect.description")
                },
            )
        } else {
            (
                t!("terminal.confirm_restart.title", name = self.name.clone()),
                t!("terminal.confirm_restart.description"),
            )
        };
        let this = cx.entity().downgrade();
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let this = this.clone();
            alert
                .confirm()
                .title(title.clone())
                .description(prose(description.clone()))
                .ok_text(t!("terminal.reconnect"))
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, window, cx| {
                    let _ = this.update(cx, |this, cx| this.force_reconnect(window, cx));
                    true
                })
        });
    }

    /// 用新的连接重连（SSH、Telnet），或重新启动本地 shell、WSL：新的建好前旧的照常可用，失败时保留旧的。
    /// 串口是独占打开的：先关掉旧的，等它结束后再打开。
    fn force_reconnect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        tracing::info!(session = %self.name, kind = self.log_kind(), "手动重新连接");
        if !matches!(self.state, State::Connected) {
            self.reconnect_now(window, cx);
        } else if self.is_raw() && self.protocol == Protocol::Serial {
            if !self.reopen_after_close
                && let Some(view) = &self.view
            {
                self.reopen_after_close = true;
                view.read(cx).engine().close();
            }
        } else if self._connect.is_none() {
            self.start(Attempt::Forced, window, cx);
        }
        cx.notify();
    }

    /// 指纹变化后核对过：删除旧记录再重新连接（会重新询问新的指纹）。
    fn forget_host_key_and_retry(
        &mut self,
        host: String,
        port: u16,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::info!(session = %self.name, host = %host, port, "删除旧的主机指纹后重新连接");
        if let Err(e) = ssh::KnownHosts::new(self.store.database()).forget(&host, port) {
            tracing::error!("删除主机指纹失败：{e}");
        }
        self.reconnect_now(window, cx);
    }

    fn open_search(&mut self, _: &OpenSearch, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.view.clone() else {
            return;
        };
        let bar = match &self.search {
            Some(bar) => bar.clone(),
            None => {
                let bar = cx.new(|cx| SearchBar::new(view, window, cx));
                self._search_subscription =
                    Some(
                        cx.subscribe_in(&bar, window, |this, _, event, window, cx| match event {
                            SearchBarEvent::Closed => {
                                this.search = None;
                                this._search_subscription = None;
                                if let Some(view) = &this.view {
                                    view.focus_handle(cx).focus(window, cx);
                                }
                                cx.notify();
                            }
                        }),
                    );
                self.search = Some(bar.clone());
                bar
            }
        };
        bar.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /// 按设置、界面深浅与各级配色生成这个终端的样式。
    fn style(&self, cx: &App) -> TerminalStyle {
        let mut levels = match &self.session {
            Session::Host { host } => SchemeLevels::for_host(&self.store, host),
            _ => SchemeLevels::default(),
        };
        levels.tab = self.tab_scheme.clone();
        levels.preview = self.scheme_preview.clone();
        terminal_colors::terminal_style(
            &self.store,
            &self.store.settings(),
            cx.theme().is_dark(),
            &levels,
        )
    }

    /// 设置、外观、主机或分组的配色变了：重新应用样式与回滚的行数。
    pub fn restyle(&mut self, cx: &mut Context<Self>) {
        let terminal = self.store.settings().terminal;
        self.transfer_bar
            .read(cx)
            .interceptor()
            .set_enabled(terminal.file_transfer);
        let style = self.style(cx);
        if let Some(view) = &self.view {
            view.update(cx, |view, cx| {
                view.set_style(style, cx);
                view.set_scrollback(terminal.scrollback as usize, cx);
            });
        }
        self.sync_suggestions(cx);
        self.sync_monitoring(cx);
    }

    /// 设置中打开、关闭了资源监控或网络延迟：连接着的终端立即开始或停止。
    fn sync_monitoring(&mut self, cx: &mut Context<Self>) {
        let terminal = self.store.settings().terminal;
        if !terminal.latency {
            self._latency = None;
            self.latency = None;
        } else if self._latency.is_none()
            && let Some(connection) = self.connection()
        {
            self.start_latency(connection, cx);
        }
        if !terminal.monitor {
            self._monitor = None;
            self.resources = Resources::Off;
        } else if self.resources == Resources::Off
            && let ToolsState::Connected(target) = self.tools_state()
        {
            self.start_monitor(target, cx);
        }
        cx.notify();
    }

    /// 只用于这个 Tab 的配色（空为取消）。
    pub fn set_tab_scheme(&mut self, scheme: String, cx: &mut Context<Self>) {
        self.tab_scheme = scheme;
        self.restyle(cx);
    }

    /// 命令面板中预览配色（空为结束预览）。
    pub fn set_scheme_preview(&mut self, scheme: String, cx: &mut Context<Self>) {
        if self.scheme_preview != scheme {
            self.scheme_preview = scheme;
            self.restyle(cx);
        }
    }

    pub fn tab_scheme(&self) -> &str {
        &self.tab_scheme
    }

    /// 终端下方横幅的文字与按钮（没有时为 `None`）。
    fn banner(&self) -> Option<(SharedString, Option<SharedString>)> {
        let reconnect = Some(t!("terminal.reconnect"));
        match &self.state {
            State::Connected => None,
            State::Connecting => Some((t!("terminal.reconnecting"), None)),
            State::Reconnecting { attempt } => {
                Some((t!("terminal.waiting", attempt = attempt), None))
            }
            State::Waiting {
                attempt,
                last_error,
            } => {
                let mut text = t!("terminal.waiting", attempt = attempt).to_string();
                if let Some(error) = last_error {
                    text.push(' ');
                    text.push_str(&ssh_errors::describe(error));
                }
                Some((text.into(), Some(t!("terminal.reconnect_now"))))
            }
            State::Failed(failure) => Some((failure.describe(), reconnect)),
            State::Ended(reason) => {
                let ssh = self.is_ssh();
                let text = match reason {
                    CloseReason::Exited(Some(code)) if ssh => {
                        t!("terminal.session_ended_with_code", code = code)
                    }
                    CloseReason::Exited(None) if ssh => t!("terminal.session_ended"),
                    CloseReason::Exited(Some(code)) => t!("terminal.exited_with_code", code = code),
                    CloseReason::Exited(None) => t!("terminal.exited"),
                    CloseReason::Lost(_) => t!("terminal.lost"),
                    CloseReason::Failed(reason) => t!("terminal.failed", reason = reason),
                };
                let hint = if ssh {
                    t!("terminal.press_enter_to_reconnect")
                } else {
                    t!("terminal.press_enter_to_restart")
                };
                Some((format!("{text} {hint}").into(), reconnect))
            }
        }
    }

    /// 还没有终端视图时（第一次连接）的整页状态。
    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let body =
            match &self.state {
                State::Failed(failure) => {
                    let host_key_changed = match failure {
                        Failure::Ssh(SshError::HostKeyChanged { host, port, .. }) => {
                            Some((host.clone(), *port))
                        }
                        _ => None,
                    };
                    v_flex()
                        .items_center()
                        .gap_3()
                        .max_w_128()
                        .child(Icon::new(IconName::CircleX).text_color(cx.theme().danger))
                        .child(div().text_base().child(prose(t!(
                            "terminal.connect_failed",
                            name = self.name.clone()
                        ))))
                        .child(
                            div()
                                .id("terminal-failure-reason")
                                .test_support()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(prose(failure.describe())),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .when_some(host_key_changed, |this, (host, port)| {
                                    this.child(
                                        Button::new("forget-host-key")
                                            .danger()
                                            .label(t!("terminal.forget_host_key"))
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.forget_host_key_and_retry(
                                                    host.clone(),
                                                    port,
                                                    window,
                                                    cx,
                                                )
                                            })),
                                    )
                                })
                                .child(
                                    Button::new("reconnect")
                                        .primary()
                                        .label(t!("terminal.reconnect"))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.reconnect_now(window, cx)
                                        })),
                                ),
                        )
                }
                _ => v_flex().items_center().gap_3().child(Spinner::new()).child(
                    div().text_sm().child(prose(if self.is_ssh() {
                        t!("terminal.connecting", name = self.name.clone())
                    } else {
                        t!("terminal.starting", name = self.name.clone())
                    })),
                ),
            };
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .bg(cx.theme().background)
            .child(body)
    }

    fn render_stalled(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // 浮在终端文字上：先铺一层不透明的背景
        div().bg(cx.theme().background).child(
            h_flex()
                .px_3()
                .py_1()
                .gap_2()
                .justify_between()
                .text_sm()
                .bg(cx.theme().warning.opacity(0.15))
                .child(prose(t!("terminal.stalled")))
                .child(
                    Button::new("reconnect-stalled")
                        .ghost()
                        .label(t!("terminal.reconnect_now"))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.force_reconnect(window, cx)),
                        ),
                ),
        )
    }
}

impl Drop for TerminalPanel {
    /// 关闭 Tab：结束它的 tmux 会话（断线重连期间也要结束，用新的连接）。终端视图丢弃时关闭通道或结束本地进程。
    fn drop(&mut self) {
        tracing::info!(session = %self.name, kind = self.log_kind(), "关闭终端");
        self.drop_recording();
        if self.tmux != TmuxStatus::Attached {
            return;
        }
        let Some(spec) = self.spec.take() else {
            return;
        };
        let store = self.store.clone();
        let key = self.identity.key.clone();
        let connection = self.connection.take();
        crate::cli_bridge::spawn_cleanup(async move {
            if let Err(e) = ssh::kill_tmux(store, &spec, &key, connection).await {
                tracing::info!("没能结束 tmux 会话 {key}：{e}");
            }
        });
    }
}

/// 要提醒的快过期的登录证书：经过的跳板机的（从第一跳起，带跳板机的名称）在前，目标主机的（`None`）在后
/// （与命令行工具按同一个判断，见 [`termlet_core::sshcert::expiring`]）；已经提醒过的（同一跳、同一个失效时间）
/// 不再提醒。
fn expiring_certificates(
    jumps: Vec<(String, i64)>,
    target: Option<i64>,
    warned: &[(Option<String>, i64)],
    now: i64,
) -> Vec<(Option<String>, i64)> {
    termlet_core::sshcert::expiring(jumps, target, now)
        .into_iter()
        .filter(|(jump, expiry)| !warned.iter().any(|(j, e)| j == jump && e == expiry))
        .collect()
}

/// Tab 的悬停提示中显示的程序标题（OSC 0 / 2，如 `user@host: ~`）：Tab 上写的是创建时的名称，程序设置的标题
/// 只作补充；空的、与名称相同的不显示，ConPTY 设的 shell 程序完整路径也不显示（不如 shell 的名称好认）。
fn program_title(name: &str, title: Option<&str>) -> Option<String> {
    let title = title?.trim();
    let is_shell_path = title.to_ascii_lowercase().ends_with(".exe") && title.contains(['\\', '/']);
    (!title.is_empty() && !is_shell_path && title != name).then(|| title.to_string())
}

impl BasePanel for TerminalPanel {
    fn panel_name(&self) -> &'static str {
        PANEL_NAME
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.active = active;
        if active {
            self.activity = false;
            self.bell = false;
            self.focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }
}

impl TerminalPanel {
    /// 程序设置的标题（见 [`program_title`]）：Tab 的悬停提示中的说明。
    fn program_title(&self, cx: &App) -> Option<SharedString> {
        program_title(
            &self.name,
            self.view.as_ref().and_then(|view| view.read(cx).title()),
        )
        .map(SharedString::from)
    }

    /// 连接目标（见 [`crate::host_info::connection_target`]）：保存的主机按主机的设置，快速连接、被调起的按连接的
    /// 参数，WSL 为发行版；本地终端没有。与 Tab 的名称相同时不再写（快速连接的名称就是 `用户@地址`）。
    fn tab_target(&self) -> Option<SharedString> {
        use crate::host_info::{ConnectionTarget, connection_target};
        let target = match &self.session {
            Session::Host { host } => crate::host_info::host_target(&self.store, host),
            Session::Quick { target } => connection_target(&ConnectionTarget::Ssh {
                username: &target.username,
                address: &target.host,
                port: target.port,
            }),
            Session::Launch { target } => connection_target(&ConnectionTarget::Ssh {
                username: &target.username,
                address: &target.address,
                port: target.port,
            }),
            Session::Wsl { distro, user, .. } => {
                connection_target(&ConnectionTarget::Wsl { distro, user })
            }
            Session::Local { .. } => connection_target(&ConnectionTarget::Local),
        };
        target
            .filter(|target| target.as_str() != self.name.as_ref())
            .map(SharedString::from)
    }

    /// 横排的 Tab 的悬停提示：程序设置的标题与连接目标，各占一行（名称就写在 Tab 上，不再重复）；都没有时不提示。
    fn tab_tooltip(&self, cx: &App) -> Option<SharedString> {
        crate::workspace::tooltip_lines([
            self.program_title(cx).as_ref(),
            self.tab_target().as_ref(),
        ])
    }

    /// Tab 标题前的图标（与 SFTP 的 Tab 一样，同一台主机的终端与 SFTP 的 Tab 同名）。
    fn tab_icon_element(&self, cx: &App) -> Icon {
        brand_icons::icon_or(
            self.tab_brand(),
            self.tab_icon(),
            self.store.settings().appearance.icons,
            cx.theme().muted_foreground,
            cx.theme().tab_bar,
            cx,
        )
        .xsmall()
    }

    /// Tab 名称后的状态：连接中 / 出错的圆点、后台的新输出、铃声、记会话日志、录制中、同步输入（横排的 Tab 与竖排的
    /// Tab 列表共用）。
    fn tab_badges(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let theme = cx.theme();
        let status = match &self.state {
            State::Connecting | State::Reconnecting { .. } | State::Waiting { .. } => {
                Some(theme.warning)
            }
            State::Connected if self.stalled => Some(theme.warning),
            State::Failed(_) | State::Ended(CloseReason::Lost(_)) => Some(theme.danger),
            State::Connected | State::Ended(_) => None,
        };
        let dot = |color| div().size_1p5().rounded_full().bg(color).into_any_element();
        let mut badges = Vec::new();
        badges.extend(status.map(dot));
        if self.activity && !self.bell {
            badges.push(dot(theme.primary));
        }
        if self.bell {
            badges.push(
                Icon::new(IconName::Bell)
                    .xsmall()
                    .text_color(theme.warning)
                    .into_any_element(),
            );
        }
        if self.is_logging() {
            badges.push(dot(theme.danger));
        }
        badges.extend(self.render_record_badge(cx));
        // 同步输入中：琥珀色的方点
        if self.sync_member {
            badges.push(
                div()
                    .size_1p5()
                    .rounded_sm()
                    .bg(cx.theme().warning)
                    .into_any_element(),
            );
        }
        badges
    }

    /// 竖排的 Tab 列表中这个 Tab 的状态（与 [`Self::tab_badges`] 同义、同色），最要紧的在前：连接的问题、连接中、
    /// 响铃、录制中、新输出、记会话日志、同步输入。
    fn tab_status(&self, cx: &Context<Self>) -> Vec<crate::workspace::TabStatus> {
        use crate::workspace::TabStatus;
        let theme = cx.theme();
        let mut status = Vec::new();
        match &self.state {
            State::Failed(_) | State::Ended(CloseReason::Lost(_)) => status.push(TabStatus::dot(
                theme.danger,
                t!("tab_list.status.disconnected"),
            )),
            State::Connecting | State::Reconnecting { .. } | State::Waiting { .. } => status.push(
                TabStatus::dot(theme.warning, t!("tab_list.status.connecting")),
            ),
            State::Connected if self.stalled => {
                status.push(TabStatus::dot(theme.warning, t!("tab_list.status.stalled")))
            }
            State::Connected | State::Ended(_) => {}
        }
        if self.bell {
            status.push(TabStatus {
                bell: true,
                ..TabStatus::dot(theme.warning, t!("tab_list.status.bell"))
            });
        }
        if self.is_recording() {
            status.push(TabStatus::dot(
                theme.danger,
                t!("tab_list.status.recording"),
            ));
        }
        if self.activity && !self.bell {
            status.push(TabStatus::dot(
                theme.primary,
                t!("tab_list.status.activity"),
            ));
        }
        if self.is_logging() {
            status.push(TabStatus::dot(theme.danger, t!("tab_list.status.logging")));
        }
        if self.sync_member {
            status.push(TabStatus::dot(theme.warning, t!("tab_list.status.sync")));
        }
        status
    }

    /// 按主机分组时归在哪台主机下（见 [`crate::workspace::TabFace::host`]）：与文件管理的会话按同一台主机认（以 root
    /// 身份打开的也算这台）；本地终端不算。
    pub(crate) fn tab_host(&self, cx: &App) -> Option<(SharedString, SharedString)> {
        let key = match (self.sftp_target(), &self.session) {
            (Some(target), _) => crate::workspace::host_group_key(&target),
            (None, Session::Host { host }) => host.as_str().to_string(),
            (None, _) => return None,
        };
        Some((key.into(), self.host_label(cx)))
    }

    /// 竖排的 Tab 列表中这个 Tab 的一行：与横排的 Tab 上相同的图标、名称与状态。
    pub(crate) fn tab_face(&self, cx: &Context<Self>) -> crate::workspace::TabFace {
        crate::workspace::TabFace {
            icon: self.tab_icon_element(cx).into_any_element(),
            rail_icon: self
                .tab_icon_element(cx)
                .with_size(crate::workspace::RAIL_ICON)
                .into_any_element(),
            name: self.name.clone(),
            detail: self.program_title(cx),
            target: self.tab_target(),
            badges: self.tab_badges(cx),
            status: self.tab_status(cx),
            host: self.tab_host(cx),
        }
    }
}

impl Panel for TerminalPanel {
    fn title(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Tab 上写创建时的名称（主机名、本地 shell 等，与 SFTP 的 Tab 一致），不随远端程序设置的标题变化；
        // 程序设置的标题与连接目标在悬停提示中（也是无障碍的描述）
        let tooltip = self.tab_tooltip(cx);
        // 右键菜单开着时关闭按钮不挂悬停提示（会浮在菜单上，见 crate::menu_tooltip）
        let menu_open = crate::menu_tooltip::menu_open(window, cx);
        // 自己的关闭按钮（Dock 自带的不显示）：有连接时按设置先确认
        let close_id = ("tab-close", cx.entity_id().as_u64() as usize);
        h_flex()
            .id(("tab-title", cx.entity_id().as_u64() as usize))
            .test_support()
            .gap_1()
            // 点 Tab 标题与点终端区域一样让终端获得焦点（Tab 自己按下时不往外传，所在的那一格不会拿到焦点）：
            // 分屏时点另一格显示着的 Tab，主机工具、快捷命令随之跟到那一格。拖动、中键关闭、右键菜单不受影响
            .on_mouse_down(
                gpui_kit::MouseButton::Left,
                cx.listener(|this, _, window, cx| this.focus_handle(cx).focus(window, cx)),
            )
            .when_some(tooltip, |this, tooltip| {
                this.aria_description(tooltip.clone())
                    .tooltip(crate::menu_tooltip::quiet(move |window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(tooltip.clone())
                            .build(window, cx)
                    }))
            })
            // 右键 Tab 打开这个 Tab 的菜单（Tab 栏上没有“…”，见 dock_skin）
            .context_menu({
                let this = cx.entity().downgrade();
                move |menu, window, cx| {
                    // 项多：窗口矮时在菜单里滚动，不伸出窗口
                    let menu = crate::menu_fit::fit(menu, window);
                    match this.upgrade() {
                        Some(panel) => {
                            panel.update(cx, |panel, cx| panel.dropdown_menu(menu, window, cx))
                        }
                        None => menu,
                    }
                }
            })
            // 与 SFTP 的 Tab 一样，标题前有图标（同一台主机的终端与 SFTP 的 Tab 同名）
            .child(self.tab_icon_element(cx))
            .child(self.name.clone())
            .children(self.tab_badges(cx))
            .child(
                gpui_kit::component::button::Button::new(close_id)
                    .ghost()
                    .small()
                    .icon(gpui_kit::assets::IconName::Close)
                    .tab_stop(false)
                    .when(!menu_open, |this| {
                        this.tooltip_with_action(t!("panel.close"), &CloseTab, Some(KEY_CONTEXT))
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        cx.stop_propagation();
                        this.request_close(window, cx);
                    })),
            )
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }

    /// Tab 菜单（右键 Tab 打开）中的项：派发与快捷键相同的动作（在这个终端里处理）。
    fn dropdown_menu(
        &mut self,
        menu: gpui_kit::component::menu::PopupMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::component::menu::PopupMenu {
        use crate::workspace::{SaveAsWorkspace, SplitDown, SplitRight};
        let menu = menu.action_context(self.focus_handle(cx));
        let menu = if self.is_ssh() && self.can_duplicate() {
            menu.menu(t!("panel.menu.duplicate"), Box::new(DuplicateSession))
        } else {
            menu
        };
        let menu = if self.can_duplicate() {
            menu.menu(t!("panel.menu.split_right"), Box::new(SplitRight))
                .menu(t!("panel.menu.split_down"), Box::new(SplitDown))
        } else {
            menu
        };
        // 临时连接：保存到主机列表（被调起的预填名称、地址、端口、用户名、密码；快速连接预填地址、端口、用户名）
        let draft = match &self.session {
            Session::Launch { target } => {
                Some((crate::launch::host_draft(target), target.password.clone()))
            }
            Session::Quick { target } => Some((crate::launch::quick_host_draft(target), None)),
            _ => None,
        };
        let menu = match draft {
            Some((host, password)) => menu.item(
                gpui_kit::component::menu::PopupMenuItem::new(t!("launch.save_as_host")).on_click(
                    move |_, window, cx| {
                        crate::host_dialog::open_prefilled(
                            host.clone(),
                            password.clone(),
                            window,
                            cx,
                        )
                    },
                ),
            ),
            None => menu,
        };
        let menu = match self.address(cx) {
            Some(address) => menu.item(crate::copy_address_item(address)),
            None => menu,
        };
        // 保存的主机（SSH、Telnet、串口）：在侧边栏中显示它；本地终端、临时连接没有
        let menu = match self.host() {
            Some(host) => menu.item(crate::workspace::reveal_host_item(host.clone())),
            None => menu,
        };
        let menu = menu
            .menu(t!("panel.menu.save_workspace"), Box::new(SaveAsWorkspace))
            .menu_with_check(
                t!("multi_exec.title"),
                self.sync_member,
                Box::new(crate::workspace::ToggleMultiExec),
            )
            .separator()
            .menu(t!("panel.menu.reconnect"), Box::new(Reconnect));
        let this = cx.entity().downgrade();
        let logging = self.is_logging();
        let toggle_log = {
            let this = this.clone();
            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                let _ = this.update(cx, |this, cx| {
                    if this.is_logging() {
                        this.stop_session_log(cx)
                    } else {
                        this.start_session_log(window, cx)
                    }
                });
            }
        };
        let mut menu = menu.item(
            gpui_kit::component::menu::PopupMenuItem::new(if logging {
                t!("panel.menu.stop_log")
            } else {
                t!("panel.menu.start_log")
            })
            .on_click(toggle_log),
        );
        // 会话录制：录制中可以停止（文件还在建时同样算录制中）
        {
            let this = this.clone();
            menu = menu.item(
                gpui_kit::component::menu::PopupMenuItem::new(if self.is_recording() {
                    t!("panel.menu.stop_record")
                } else {
                    t!("panel.menu.start_record")
                })
                .disabled(!self.can_record())
                .on_click(move |_, window, cx| {
                    let _ = this.update(cx, |this, cx| this.toggle_recording(window, cx));
                }),
            );
        }
        if let Some(path) = self.recording_path() {
            menu = menu.item(
                gpui_kit::component::menu::PopupMenuItem::new(t!("panel.menu.reveal_record"))
                    .on_click(move |_, _, cx| cx.reveal_path(&path)),
            );
        }
        if logging {
            let this = this.clone();
            menu = menu.item(
                gpui_kit::component::menu::PopupMenuItem::new(t!("panel.menu.reveal_log"))
                    .on_click(move |_, _, cx| {
                        let path = this
                            .upgrade()
                            .and_then(|panel| panel.read(cx).tap.log())
                            .map(|log| log.path().to_path_buf());
                        if let Some(path) = path {
                            cx.reveal_path(&path);
                        }
                    }),
            );
        }
        if let Session::Host { host } = &self.session
            && self.is_ssh()
        {
            let (this, host) = (this.clone(), host.clone());
            let forward = this.clone();
            let connected = self.share_group().is_some();
            menu = menu
                .item(
                    gpui_kit::component::menu::PopupMenuItem::new(t!("history.open")).on_click(
                        move |_, window, cx| {
                            crate::snippets::history::open(
                                host.clone(),
                                Some(this.clone()),
                                window,
                                cx,
                            )
                        },
                    ),
                )
                .item(
                    gpui_kit::component::menu::PopupMenuItem::new(t!("forward.add_for_host"))
                        .disabled(!connected)
                        .on_click(move |_, _, cx| {
                            let _ =
                                forward.update(cx, |_, cx| cx.emit(TerminalPanelEvent::AddForward));
                        }),
                );
        }
        // 把本机代理借给服务器：连接着的 SSH 终端（“只用一个通道”的主机不行）；开启着时打开它的命令片段与“停止”
        if self.is_ssh() {
            let running = self.proxy.is_some();
            let terminal = this.clone();
            menu = menu.item(
                gpui_kit::component::menu::PopupMenuItem::new(if running {
                    t!("terminal.proxy.menu_running")
                } else {
                    t!("terminal.proxy.menu")
                })
                .disabled(!running && !self.can_lend_proxy())
                .on_click(move |_, window, cx| {
                    if let Some(terminal) = terminal.upgrade() {
                        crate::proxy_share::open(terminal, window, cx);
                    }
                }),
            );
        }
        // 配色：打开选择器（可以只用于这个 Tab）；用着临时配色时可以清除
        {
            let this = this.clone();
            menu = menu.separator().item(
                gpui_kit::component::menu::PopupMenuItem::new(t!("panel.menu.scheme")).on_click(
                    move |_, window, cx| {
                        crate::schemes::picker::open(Some(this.clone()), window, cx)
                    },
                ),
            );
        }
        if !self.tab_scheme.is_empty() {
            let this = this.clone();
            menu = menu.item(
                gpui_kit::component::menu::PopupMenuItem::new(t!("panel.menu.clear_scheme"))
                    .on_click(move |_, _, cx| {
                        let _ = this.update(cx, |this, cx| this.set_tab_scheme(String::new(), cx));
                    }),
            );
        }
        // 远端的编码：与终端内的右键菜单一样收在子菜单里
        if self.is_ssh() {
            let current = self.current_encoding();
            let items: Vec<gpui_kit::component::menu::PopupMenuItem> = ENCODINGS
                .iter()
                .map(|(value, _, label)| {
                    let this = this.clone();
                    let value = *value;
                    gpui_kit::component::menu::PopupMenuItem::new(*label)
                        .checked(value == current)
                        .on_click(move |_, _, cx| {
                            let _ = this.update(cx, |this, cx| this.set_encoding(value, cx));
                        })
                })
                .collect();
            let submenu =
                gpui_kit::component::menu::PopupMenu::build(window, cx, move |menu, _, _| {
                    items.into_iter().fold(menu, |menu, item| menu.item(item))
                });
            menu = menu.item(gpui_kit::component::menu::PopupMenuItem::submenu(
                t!("terminal.encoding"),
                submenu,
            ));
        }
        let menu = menu.separator().menu(t!("panel.close"), Box::new(CloseTab));
        crate::workspace::close_tab_items(menu, cx.entity_id(), window, cx).separator()
    }
}

impl Render for TerminalPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bounds = self.bounds.clone();
        let root = v_flex()
            .id("terminal-panel")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .on_action(cx.listener(Self::on_reconnect_action))
            .on_action(cx.listener(Self::on_close_tab))
            .on_action(cx.listener(|this, _: &ToggleRecording, window, cx| {
                this.toggle_recording(window, cx)
            }))
            .on_action(cx.listener(|this, _: &text_input::Copy, window, cx| {
                this.on_menu_edit(Box::new(termlet_terminal::view::Copy), window, cx)
            }))
            .on_action(cx.listener(|this, _: &text_input::Paste, window, cx| {
                this.on_menu_edit(Box::new(termlet_terminal::view::Paste), window, cx)
            }))
            .on_action(cx.listener(|this, _: &text_input::SelectAll, window, cx| {
                this.on_menu_edit(Box::new(termlet_terminal::view::SelectAll), window, cx)
            }))
            .capture_action(cx.listener(Self::on_paste))
            .capture_key_down(cx.listener(Self::on_key_down_capture))
            .child(
                gpui_kit::canvas(move |area, _, _| bounds.set(area), |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            );
        let Some(view) = self.view.clone() else {
            return root.child(self.render_status(cx)).into_any_element();
        };
        let banner = self.banner();
        let stalled = self.stalled && matches!(self.state, State::Connected);
        let password_prompt = self.render_password_prompt(cx);
        let status_bar = self.render_status_bar(window, cx);
        let menu_mode =
            self.store.settings().terminal.right_click == termlet_core::model::RightClick::Menu;
        let area = div()
            .id("terminal-area")
            .relative()
            .flex_1()
            .min_h_0()
            .child(view)
            // 浮在终端上方，不改变终端的大小（否则远端的全屏程序要重绘）
            .when(stalled, |this| {
                this.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .child(self.render_stalled(cx)),
                )
            })
            .when_some(self.search.clone(), |this, bar| {
                this.child(div().absolute().top_2().right_4().child(bar))
            })
            // 文件传输的提示栏浮在顶部；服务器在等待接收文件时可以把文件拖到终端上作答
            .when(self.transfer_bar.read(cx).is_visible(), |this| {
                this.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .child(self.transfer_bar.clone()),
                )
            })
            // 不在等待接收文件时，拖来的会话录制（.cast）打开回放
            .can_drop({
                let bar = self.transfer_bar.downgrade();
                move |dragged, _, cx| {
                    let Some(paths) = dragged.downcast_ref::<gpui_kit::ExternalPaths>() else {
                        return false;
                    };
                    bar.upgrade()
                        .is_some_and(|bar| bar.read(cx).interceptor().accepts_drop().is_some())
                        || paths
                            .paths()
                            .iter()
                            .any(|p| termlet_core::recording::is_cast_path(p))
                }
            })
            .on_drop(
                cx.listener(|this, paths: &gpui_kit::ExternalPaths, window, cx| {
                    let paths = paths.paths().to_vec();
                    if this
                        .transfer_bar
                        .read(cx)
                        .interceptor()
                        .accepts_drop()
                        .is_some()
                    {
                        this.transfer_bar
                            .update(cx, |bar, cx| bar.drop_files(paths, cx));
                    } else if let Some(workspace) =
                        crate::workspace::Workspace::in_window(window, cx)
                    {
                        workspace.update(cx, |workspace, cx| {
                            workspace.open_replay_files(&paths, window, cx)
                        });
                    }
                }),
            );
        // 设置“终端中右键”为弹出菜单时：右键弹出复制、粘贴、全选、查找、清屏、编码（终端视图不处理右键）
        let area = if menu_mode {
            let focus = self.focus_handle(cx);
            let this = cx.entity().downgrade();
            let ssh = self.is_ssh();
            let current = self.current_encoding();
            area.context_menu(move |menu, window, cx| {
                let menu = crate::menu_fit::fit(menu, window)
                    .action_context(focus.clone())
                    .menu(
                        t!("terminal.menu.copy"),
                        Box::new(termlet_terminal::view::Copy),
                    )
                    .menu(
                        t!("terminal.menu.paste"),
                        Box::new(termlet_terminal::view::Paste),
                    )
                    .menu(
                        t!("terminal.menu.select_all"),
                        Box::new(termlet_terminal::view::SelectAll),
                    )
                    .separator()
                    .menu(t!("terminal.menu.find"), Box::new(OpenSearch))
                    .menu(
                        t!("terminal.menu.clear"),
                        Box::new(termlet_terminal::view::Clear),
                    );
                if !ssh {
                    return menu;
                }
                let this = this.clone();
                menu.separator()
                    .submenu(t!("terminal.encoding"), window, cx, move |menu, _, _| {
                        ENCODINGS.iter().fold(menu, |menu, (value, _, label)| {
                            let this = this.clone();
                            let value = *value;
                            menu.item(
                                gpui_kit::component::menu::PopupMenuItem::new(*label)
                                    .checked(value == current)
                                    .on_click(move |_, _, cx| {
                                        let _ = this
                                            .update(cx, |this, cx| this.set_encoding(value, cx));
                                    }),
                            )
                        })
                    })
            })
            .into_any_element()
        } else {
            area.into_any_element()
        };
        root.on_action(cx.listener(Self::open_search))
            .child(area)
            .children(password_prompt)
            .children(status_bar)
            .when_some(banner, |this, (message, button)| {
                this.child(
                    h_flex()
                        .px_3()
                        .py_1()
                        .gap_2()
                        .justify_between()
                        .text_sm()
                        .bg(cx.theme().muted)
                        .text_color(cx.theme().muted_foreground)
                        .child(prose(message))
                        .when_some(button, |this, label| {
                            this.child(
                                Button::new("reconnect-banner")
                                    .ghost()
                                    .label(label)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.reconnect_now(window, cx)
                                    })),
                            )
                        }),
                )
            })
            .into_any_element()
    }
}

/// 图片转成 PNG（剪贴板中的 BMP 截图）。
fn to_png(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let image = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
    let mut png = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(png)
}

/// 命令用时：不到一分钟写秒，否则写分和秒。
fn format_duration(duration: Duration) -> SharedString {
    let seconds = duration.as_secs_f64().round() as u64;
    if seconds < 60 {
        t!("notify.seconds", count = seconds)
    } else {
        t!("notify.minutes", m = seconds / 60, s = seconds % 60)
    }
}

/// 打开终端时 X11 转发没有成功的说明。
fn x11_status_notice(status: &X11Status) -> Option<SharedString> {
    match status {
        X11Status::Off | X11Status::Forwarding { .. } => None,
        X11Status::Refused => Some(t!("terminal.notice.x11_refused")),
        X11Status::Unavailable(problem) => Some(x11_problem(problem)),
    }
}

/// X11 转发中的说明。
fn x11_notice(notice: &X11Notice) -> SharedString {
    match notice {
        X11Notice::NoServer { display, reason } => t!(
            "terminal.notice.x11_no_server",
            display = display,
            reason = reason
        ),
        X11Notice::CookieFailed(problem) => x11_problem(problem),
    }
}

fn x11_problem(problem: &X11Problem) -> SharedString {
    match problem {
        X11Problem::BadDisplay(display) => {
            t!("terminal.notice.x11_bad_display", display = display)
        }
        X11Problem::NoXauth => t!("terminal.notice.x11_no_xauth"),
        X11Problem::XauthFailed(reason) => {
            t!("terminal.notice.x11_xauth_failed", reason = reason)
        }
    }
}

#[cfg(test)]
impl TerminalPanel {
    /// 测试用：当作依次采到了这些样本（与真实的采样一样更新级别、记进历史）。
    pub(crate) fn feed_samples(
        &mut self,
        samples: impl IntoIterator<Item = termlet_core::monitor::Sample>,
        cx: &mut Context<Self>,
    ) {
        for sample in samples {
            self.levels.update(&sample);
            self.history.push(&sample);
            self.resources = Resources::Sampling(Some(Box::new(sample)));
        }
        cx.notify();
    }

    /// 测试用：直接设置资源采样的情况（不支持、单通道、关闭）。
    pub(crate) fn set_resources(&mut self, resources: Resources, cx: &mut Context<Self>) {
        self.resources = resources;
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Workspace;
    use crate::testkit::poll_until;

    // 快过期的证书：跳板机的在前、写明哪一跳，目标主机的在后；还早的、提醒过的（同一跳同一张证书）不再提醒，
    // 换了新证书再提醒
    #[test]
    fn expiring_certificates_of_jumps_and_target() {
        let now = 1_000_000;
        let soon = now + 600;
        let later = now + 5 * 3600;
        let jumps = vec![("bastion".to_string(), soon), ("inner".to_string(), later)];
        assert_eq!(
            expiring_certificates(jumps.clone(), Some(soon), &[], now),
            [(Some("bastion".to_string()), soon), (None, soon)]
        );
        let warned = [(Some("bastion".to_string()), soon), (None, soon)];
        assert!(expiring_certificates(jumps.clone(), Some(soon), &warned, now).is_empty());
        assert_eq!(
            expiring_certificates(vec![("bastion".to_string(), soon + 60)], None, &warned, now),
            [(Some("bastion".to_string()), soon + 60)],
            "换了新证书"
        );
        assert!(expiring_certificates(Vec::new(), Some(later), &[], now).is_empty());
    }

    // Tab 上写创建时的名称；程序设置的标题只放在悬停提示中，没有意义的不放
    #[test]
    fn program_title_only_supplements_the_tab_name() {
        assert_eq!(
            program_title("web-01", Some("root@web-01: ~")).as_deref(),
            Some("root@web-01: ~")
        );
        assert_eq!(program_title("web-01", None), None);
        assert_eq!(program_title("web-01", Some("  ")), None);
        assert_eq!(program_title("web-01", Some("web-01")), None, "与名称相同");
        assert_eq!(
            program_title(
                "PowerShell",
                Some(r"C:\Program Files\PowerShell\7\pwsh.exe")
            ),
            None,
            "ConPTY 设的 shell 程序路径"
        );
        assert_eq!(
            program_title("PowerShell", Some("vim notes.txt")).as_deref(),
            Some("vim notes.txt")
        );
    }

    // 横排的 Tab：悬停提示（无障碍的描述同样）写连接目标，名称写在 Tab 上、不再重复：保存的 SSH 主机写
    // `用户@地址:端口`，Telnet 写 `地址:端口`；快速连接的名称就是 `用户@地址:端口`，不再写；本地终端没有提示。主机的
    // 地址连不上（打开很快失败，不影响提示）
    #[gpui_kit::test]
    fn horizontal_tabs_show_the_connection_target(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        use termlet_core::store::Position;
        let handle = crate::testkit::open_workspace(cx);
        crate::i18n::set_language(crate::i18n::Language::ZhCn);
        let store = cx.update(|cx| AppState::store(cx));
        let ssh = store
            .create(
                Host {
                    name: "web".into(),
                    address: "127.0.0.1".into(),
                    port: 1,
                    username: "alice".into(),
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
        let telnet = store
            .create(
                Host {
                    name: "switch".into(),
                    protocol: Protocol::Telnet,
                    address: "127.0.0.1".into(),
                    port: 1,
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
        let quick = QuickTarget {
            username: "bob".into(),
            host: "::1".into(),
            port: 1,
        };
        let local = cx
            .update_window(handle, |_, window, cx| {
                let workspace = Workspace::in_window(window, cx).expect("主窗口");
                workspace.update(cx, |w, cx| {
                    w.open_host(&ssh, window, cx);
                    w.open_host(&telnet, window, cx);
                    w.open_quick(quick, window, cx);
                    w.open_local_terminal(window, cx);
                });
                let local = workspace.read(cx).live_terminals().pop().expect("本地终端");
                // 不真的启动本地 shell：没有程序设置的标题
                local.update(cx, |panel, _| panel._connect = None);
                local
            })
            .unwrap();
        cx.run_until_parked();
        let description = |cx: &mut gpui_kit::TestAppContext, name: &str| {
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                let workspace = Workspace::in_window(window, cx).expect("主窗口");
                let panel = workspace
                    .read(cx)
                    .live_terminals()
                    .into_iter()
                    .find(|p| p.read(cx).name().as_ref() == name)
                    .unwrap_or_else(|| panic!("找不到 Tab：{name}"));
                window
                    .find(("tab-title", panel.entity_id().as_u64() as usize))
                    .description()
                    .map(str::to_string)
            })
            .unwrap()
        };
        assert_eq!(description(cx, "web").as_deref(), Some("alice@127.0.0.1:1"));
        assert_eq!(description(cx, "switch").as_deref(), Some("127.0.0.1:1"));
        assert_eq!(description(cx, "bob@[::1]:1"), None, "与名称相同");
        let local_name = cx.update(|cx| local.read(cx).name().to_string());
        assert_eq!(description(cx, &local_name), None, "本地终端");
    }

    // 界面大小只管界面（rem）：放大到 150% 后窗口的 rem 跟着变，已经打开的终端的字号不变
    #[gpui_kit::test]
    fn ui_scale_leaves_the_terminal_font_alone(cx: &mut gpui_kit::TestAppContext) {
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        cx.update(crate::init);
        cx.update(AppState::install_in_memory);
        let (handle, workspace) = cx.update(|cx| {
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| Workspace::new(window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| w.open_local_terminal(window, cx))
        })
        .unwrap();
        let panel = cx
            .update(|cx| workspace.read(cx).live_terminals().pop())
            .expect("打开了本地终端");
        let font_size = |cx: &mut gpui_kit::TestAppContext| {
            cx.update(|cx| {
                let view = panel.read(cx).view.clone()?;
                Some(view.read(cx).style().font_size)
            })
        };
        let rem = |cx: &mut gpui_kit::TestAppContext| {
            use gpui_kit::test::TestWindowExt as _;
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.rem_size()
            })
            .unwrap()
        };
        poll_until(cx, "本地终端启动", |cx| font_size(cx).is_some());
        let before = font_size(cx);
        assert_eq!(before, Some(gpui_kit::px(13.)), "默认字号");
        assert_eq!(rem(cx), gpui_kit::px(16.));

        cx.update(|cx| {
            AppState::store(cx)
                .update_local_settings(|s| s.window.ui_scale = 150)
                .unwrap()
        });
        cx.run_until_parked();
        assert_eq!(rem(cx), gpui_kit::px(24.));
        assert_eq!(font_size(cx), before);
    }

    // 第一次连接就失败的整页：原因照避头尾规则换行，句末的“。”不会单独落到行首。原因是 N 个“甲”、“。”与 N - 1 个
    // “乙”（一行正好放 N 个字）时，GPUI 自己换行是“甲…甲”“。乙…乙”两行；避头尾把最后一个“甲”带到下一行，要三行
    #[gpui_kit::test]
    fn failure_reason_keeps_punctuation_off_line_starts(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        let handle = crate::testkit::open_workspace(cx);
        let panel = cx
            .update_window(handle, |_, window, cx| {
                let workspace = Workspace::in_window(window, cx).expect("主窗口");
                workspace.update(cx, |w, cx| w.open_local_terminal(window, cx));
                let panel = workspace.read(cx).live_terminals().pop().expect("终端");
                // 不真的启动本地 shell：停在还没有终端画面的状态
                panel.update(cx, |panel, _| panel._connect = None);
                panel
            })
            .unwrap();
        // 整页宽 32rem，原因的字号 0.875rem；测试的文字系统每个字宽 0.6em
        let n = cx
            .update_window(handle, |_, window, _| {
                let rem = f32::from(window.rem_size());
                (32. * rem / (0.875 * rem * 0.6)).floor() as usize
            })
            .unwrap();
        let mut reason_height = |reason: String| {
            cx.update(|cx| {
                panel.update(cx, |panel, cx| {
                    panel.state = State::Failed(Failure::Local(reason.into()));
                    cx.notify();
                })
            });
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.find("terminal-failure-reason").bounds().size.height
            })
            .unwrap()
        };
        let line = reason_height("甲".repeat(n));
        assert_eq!(
            reason_height("甲".repeat(n + 1)),
            line * 2.,
            "一行放 {n} 个字"
        );
        let reason = format!("{}。{}", "甲".repeat(n), "乙".repeat(n - 1));
        assert_eq!(reason_height(reason), line * 3.);
    }

    // 设置中改了历史行数：已经打开的终端立即按新的行数保留历史，不必重新打开
    #[gpui_kit::test]
    fn scrollback_setting_applies_to_open_terminals(cx: &mut gpui_kit::TestAppContext) {
        // 本地终端的伪终端与引擎线程是真实的系统线程
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        cx.update(crate::init);
        cx.update(AppState::install_in_memory);
        let (handle, workspace) = cx.update(|cx| {
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| Workspace::new(window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| w.open_local_terminal(window, cx))
        })
        .unwrap();
        let panel = cx
            .update(|cx| workspace.read(cx).live_terminals().pop())
            .expect("打开了本地终端");
        let scrollback = |cx: &mut gpui_kit::TestAppContext| {
            cx.update(|cx| {
                let view = panel.read(cx).view.clone()?;
                Some(view.read(cx).engine().scrollback())
            })
        };
        // 本地 shell 在后台启动，启动好后才建终端视图
        poll_until(cx, "本地终端启动", |cx| scrollback(cx).is_some());
        let before = cx.update(|cx| AppState::store(cx).settings().terminal.scrollback);
        assert_eq!(scrollback(cx), Some(before as usize));

        cx.update(|cx| {
            AppState::store(cx)
                .update_settings(|s| s.terminal.scrollback = 20_000)
                .unwrap()
        });
        // 设置的变更事件经异步任务转发给工作区
        cx.run_until_parked();
        assert_eq!(scrollback(cx), Some(20_000));
    }

    /// 焦点在终端上时，shell 要用的按键原样到达终端，不被 gpui-kit 的 Root（非 macOS 上 Ctrl+C 绑给了输入框的
    /// 复制）、我们的快捷键或菜单动作抢走：Ctrl+C 等控制键、没被用作快捷键的 Ctrl+Shift+字母、Tab、Shift+Tab、Esc，
    /// 以及 Windows、Linux 上当 Meta 的 Alt+字母。打开转交输入看写进终端的字节，与终端自己的编码一致。
    #[gpui_kit::test]
    fn shell_keys_reach_the_terminal(cx: &mut gpui_kit::TestAppContext) {
        use std::cell::RefCell;
        use std::rc::Rc;

        use gpui_kit::test::TestWindowExt as _;
        use termlet_terminal::keys::{self, AltKey, KeyEventKind};
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        cx.update(crate::init);
        cx.update(AppState::install_in_memory);
        let (handle, workspace) = cx.update(|cx| {
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| Workspace::new(window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| w.open_local_terminal(window, cx))
        })
        .unwrap();
        let panel = cx
            .update(|cx| workspace.read(cx).live_terminals().pop())
            .expect("打开了本地终端");
        poll_until(cx, "本地终端启动", |cx| {
            cx.update(|cx| panel.read(cx).view.is_some())
        });
        let view = cx.update(|cx| panel.read(cx).view.clone().unwrap());
        let typed = Rc::new(RefCell::new(Vec::<u8>::new()));
        let _typed = cx.update(|cx| {
            let typed = typed.clone();
            cx.subscribe(&view, move |_, event: &TerminalViewEvent, _| {
                if let TerminalViewEvent::Typed(bytes) = event {
                    typed.borrow_mut().extend_from_slice(bytes);
                }
            })
        });
        cx.update(|cx| view.update(cx, |view, _| view.set_forward_input(true)));
        let os = cx.update(|cx| cx.platform_info().os);
        let mut specs = vec![
            "ctrl-c",
            "ctrl-a",
            "ctrl-e",
            "ctrl-k",
            "ctrl-u",
            "ctrl-w",
            "ctrl-r",
            "ctrl-d",
            "ctrl-z",
            "ctrl-l",
            "ctrl-v",
            "ctrl-x",
            "ctrl-]",
            "ctrl-\\",
            "ctrl-shift-x",
            "ctrl-shift-z",
            "tab",
            "shift-tab",
            "escape",
        ];
        if os != gpui_kit::OperatingSystem::Mac {
            specs.extend(["alt-b", "alt-f", "alt-d", "alt-."]);
        }
        for spec in specs {
            typed.borrow_mut().clear();
            cx.update_window(handle, |_, window, cx| {
                view.read(cx).focus_handle(cx).focus(window, cx);
                window.press(spec, cx);
            })
            .unwrap();
            cx.run_until_parked();
            let expected = cx.update(|cx| {
                let view = view.read(cx);
                let mode = view.engine().modes();
                let keystroke = gpui_kit::Keystroke::parse(spec).unwrap();
                keys::encode_key(
                    &keystroke,
                    mode,
                    view.engine().keyboard_flags(),
                    KeyEventKind::Press,
                    AltKey::for_os(os),
                )
                .map(|key| key.bytes.to_vec())
            });
            assert!(expected.is_some(), "{spec} 应能编码");
            assert_eq!(
                Some(typed.borrow().clone()),
                expected,
                "{spec} 原样到达终端"
            );
        }
        // 最要紧的一个写明字节：Ctrl+C 是中断
        typed.borrow_mut().clear();
        cx.update_window(handle, |_, window, cx| {
            view.read(cx).focus_handle(cx).focus(window, cx);
            window.press("ctrl-c", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(typed.borrow().as_slice(), b"\x03");
    }

    /// 打开一个本地终端（本地终端不采样），当作采到了 `samples`：状态条按它们显示。窗口宽 `width`、高 640。
    fn terminal_with_samples(
        cx: &mut gpui_kit::TestAppContext,
        width: f32,
        samples: Vec<termlet_core::monitor::Sample>,
    ) -> (
        gpui_kit::AnyWindowHandle,
        Entity<Workspace>,
        Entity<TerminalPanel>,
    ) {
        use gpui_kit::{Bounds, WindowBounds, WindowOptions, px, size};
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        cx.update(crate::init);
        cx.update(AppState::install_in_memory);
        cx.update(|cx| cx.set_reduce_motion(true));
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                Default::default(),
                size(px(width), px(640.)),
            ))),
            ..Default::default()
        };
        let (handle, workspace) = cx.update(|cx| {
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| Workspace::new(window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| w.open_local_terminal(window, cx))
        })
        .unwrap();
        let panel = cx
            .update(|cx| workspace.read(cx).live_terminals().pop())
            .expect("打开了本地终端");
        poll_until(cx, "本地终端启动", |cx| {
            cx.update(|cx| matches!(panel.read(cx).state, State::Connected))
        });
        cx.update(|cx| panel.update(cx, |panel, cx| panel.feed_samples(samples, cx)));
        render(cx, handle);
        (handle, workspace, panel)
    }

    /// 画几帧：状态条第一帧量出宽度，下一帧按它排。
    fn render(cx: &mut gpui_kit::TestAppContext, handle: gpui_kit::AnyWindowHandle) {
        use gpui_kit::test::TestWindowExt as _;
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            for _ in 0..3 {
                window.render_frame(cx);
            }
        })
        .unwrap();
    }

    /// 一次采样：CPU、内存、最满的磁盘（/data）的使用率，收的速率与负载；网卡有 eth0 与一块 veth。
    fn bar_sample(
        at: i64,
        cpu: f64,
        memory: u64,
        disk: u64,
        rx: f64,
        load: f64,
    ) -> termlet_core::monitor::Sample {
        use termlet_core::monitor::{Disk, NetIface};
        let mounted = |mount: &str, used: u64| Disk {
            mount: mount.into(),
            filesystem: "/dev/vda1".into(),
            total: 100 << 30,
            used: used << 30,
        };
        termlet_core::monitor::Sample {
            cpu: Some(cpu),
            mem_used: memory << 20,
            mem_total: 100 << 20,
            disks: vec![mounted("/", 10), mounted("/data", disk)],
            net: ["eth0", "veth01"]
                .map(|name| NetIface {
                    name: name.into(),
                    rx_total: 1,
                    tx_total: 1,
                    rx_rate: Some(rx),
                    tx_rate: Some(rx / 4.),
                })
                .to_vec(),
            rx_rate: Some(rx),
            tx_rate: Some(rx / 4.),
            load: [load, load, load],
            ..crate::monitor_model::test_sample(at)
        }
    }

    /// 状态条：各项的数字在固定宽度的槽中，数值变了各项不左右漂移；偏高时记下级别；窄时先收起进度条、再从优先级低的
    /// 一项起收起，显示的各项都在状态条内、不被裁。
    #[gpui_kit::test]
    fn status_bar_keeps_slots_and_folds_by_width(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        use gpui_kit::{px, size};
        let samples = vec![
            bar_sample(0, 12., 45, 41, 829., 0.4),
            bar_sample(3000, 12., 45, 41, 829., 0.4),
        ];
        let (handle, _, panel) = terminal_with_samples(cx, 1600., samples);
        let id = panel.entity_id().as_u64() as usize;
        let items = [
            "monitor-cpu",
            "monitor-memory",
            "monitor-disk",
            "monitor-net",
            "monitor-load",
        ];
        let lefts = |cx: &mut gpui_kit::TestAppContext| {
            cx.update_window(handle, |_, window, _| {
                items.map(|item| window.find((item, id)).bounds().left())
            })
            .unwrap()
        };
        let before = lefts(cx);
        assert_eq!(
            cx.update(|cx| panel.read(cx).bar_fit.current()),
            crate::monitor_model::Fit {
                count: 5,
                meters: true
            },
            "宽时全部带进度条"
        );
        // 数值变了（位数相同）：各项的位置不变；CPU、内存、磁盘、负载偏高
        cx.update(|cx| {
            panel.update(cx, |panel, cx| {
                panel.feed_samples([bar_sample(6000, 87., 92, 96, 9.9 * 1024. * 1024., 4.)], cx)
            })
        });
        render(cx, handle);
        assert_eq!(lefts(cx), before, "各项不随数值漂移");
        let levels = cx.update(|cx| panel.read(cx).levels);
        use crate::monitor_model::Level;
        assert_eq!(
            (levels.cpu, levels.memory, levels.disk, levels.load),
            (Level::Warning, Level::Danger, Level::Danger, Level::Warning)
        );

        // 窄：收起进度条，可能再收起后面的几项；显示的总是优先级高的前几项，都不被裁，状态条在终端内
        cx.simulate_window_resize(handle, size(px(720.), px(640.)));
        render(cx, handle);
        let fit = cx.update(|cx| panel.read(cx).bar_fit.current());
        assert!(!fit.meters, "{fit:?}");
        cx.update_window(handle, |_, window, cx| {
            let area = panel.read(cx).bounds.get();
            let bar = window.find(("monitor-bar", id)).bounds();
            assert!(
                bar.right() <= area.right() && bar.left() >= area.left(),
                "{bar:?} {area:?}"
            );
            let shown: Vec<bool> = items
                .iter()
                .map(|item| window.try_find((*item, id)).is_some())
                .collect();
            assert!(shown[0], "CPU 总在");
            assert!(
                shown.windows(2).all(|w| w[0] || !w[1]),
                "收起的是优先级低的：{shown:?}"
            );
            for item in items
                .iter()
                .filter(|item| window.try_find((**item, id)).is_some())
            {
                let bounds = window.find((*item, id)).bounds();
                assert!(bounds.right() <= bar.right(), "{item} 不被裁");
            }
        })
        .unwrap();
    }

    /// 点状态条：这个终端获得焦点，主机工具打开并切到概览页。本地终端用不了主机工具：面板显示不可用
    /// 的空状态（页签置灰、不能切换），切到别的页只能由程序做（工作区的状态条、命令面板就是这样做的）。
    #[gpui_kit::test]
    fn clicking_the_status_bar_opens_the_overview(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;

        use crate::host_tools::HostToolsPage;
        let (handle, workspace, panel) =
            terminal_with_samples(cx, 1280., vec![bar_sample(0, 5., 10, 10, 100., 0.1)]);
        let id = panel.entity_id().as_u64() as usize;
        let ui_state = |cx: &mut gpui_kit::TestAppContext| {
            cx.update(|cx| {
                let state = AppState::ui(cx).read(cx).state();
                (state.host_tools_open, state.host_tools_page.clone())
            })
        };
        let target = "monitor-bar";
        // 先在主机工具中切到进程页，再收起；焦点不在终端上
        if !ui_state(cx).0 {
            cx.update_window(handle, |_, window, cx| {
                window.click("toggle-host-tools", cx)
            })
            .unwrap();
            render(cx, handle);
        }
        cx.update(|cx| {
            workspace
                .read(cx)
                .host_tools_panel()
                .update(cx, |tools, cx| {
                    tools.show_page(HostToolsPage::Processes, cx)
                })
        });
        cx.update_window(handle, |_, window, cx| {
            window.click("toggle-host-tools", cx)
        })
        .unwrap();
        render(cx, handle);
        cx.update_window(handle, |_, window, cx| window.blur(cx))
            .unwrap();
        render(cx, handle);
        assert_eq!(ui_state(cx), (false, Some("processes".to_string())));
        cx.update_window(handle, |_, window, cx| {
            window.click((target, id), cx);
        })
        .unwrap();
        render(cx, handle);
        assert_eq!(
            ui_state(cx),
            (true, Some("overview".to_string())),
            "点 {target} 打开概览"
        );
        cx.update_window(handle, |_, window, cx| {
            assert!(
                panel.read(cx).focus_handle(cx).is_focused(window),
                "点 {target} 后终端获得焦点"
            );
            assert!(
                window.try_find("host-tools-empty-unsupported").is_some(),
                "本地终端：面板说明不可用"
            );
        })
        .unwrap();
    }

    /// 精简：平时只有 CPU、内存（本地终端没有延迟）；最满的磁盘、负载偏高时才出现（带回差，回落后隐藏）。右键状态条切回
    /// 完整，保存进设置。
    #[gpui_kit::test]
    fn compact_bar_shows_disk_and_load_only_when_high(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        use termlet_core::model::MonitorBar;
        let (handle, _, panel) =
            terminal_with_samples(cx, 1280., vec![bar_sample(0, 5., 10, 50, 100., 0.1)]);
        cx.update(|cx| {
            AppState::store(cx)
                .update_settings(|s| s.terminal.monitor_bar = MonitorBar::Compact)
                .unwrap()
        });
        render(cx, handle);
        let id = panel.entity_id().as_u64() as usize;
        let shown = |cx: &mut gpui_kit::TestAppContext| {
            cx.update_window(handle, |_, window, _| {
                [
                    "monitor-cpu",
                    "monitor-memory",
                    "monitor-disk",
                    "monitor-net",
                    "monitor-load",
                ]
                .map(|item| window.try_find((item, id)).is_some())
            })
            .unwrap()
        };
        assert_eq!(shown(cx), [true, true, false, false, false]);
        let mut at = 3000;
        let mut feed = |cx: &mut gpui_kit::TestAppContext, disk: u64, load: f64| {
            at += 3000;
            let sample = bar_sample(at, 5., 10, disk, 100., load);
            cx.update(|cx| panel.update(cx, |panel, cx| panel.feed_samples([sample], cx)));
            render(cx, handle);
        };
        // 4 核：负载 9 超过 2 倍核数
        feed(cx, 92, 9.);
        assert_eq!(
            shown(cx),
            [true, true, true, false, true],
            "磁盘、负载偏高时出现"
        );
        feed(cx, 80, 3.8);
        assert_eq!(
            shown(cx),
            [true, true, true, false, true],
            "没降到退出线以下：仍在"
        );
        feed(cx, 60, 1.);
        assert_eq!(shown(cx), [true, true, false, false, false], "回落后隐藏");

        // 右键状态条：切回完整（第一项）
        cx.update_window(handle, |_, window, cx| {
            window.right_click(("monitor-bar", id), cx);
        })
        .unwrap();
        render(cx, handle);
        cx.update_window(handle, |_, window, cx| {
            window.within("popup-menu").click(0usize, cx);
        })
        .unwrap();
        render(cx, handle);
        assert_eq!(
            cx.update(|cx| AppState::store(cx).settings().terminal.monitor_bar),
            MonitorBar::Full
        );
        assert_eq!(shown(cx), [true, true, true, true, true]);
    }

    /// macOS 上 Option 加字母：照系统的习惯交给文字输入（终端不编码）；设置中开启“Option 键作为 Meta 键”后，已经打开的
    /// 终端立即改为发送 ESC 前缀加上不按 Option 时的字符。
    #[gpui_kit::test]
    fn option_as_meta_follows_the_setting(cx: &mut gpui_kit::TestAppContext) {
        use std::cell::RefCell;
        use std::rc::Rc;

        cx.executor().allow_parking();
        cx.set_platform_info(gpui_kit::PlatformInfo {
            os: gpui_kit::OperatingSystem::Mac,
            target: gpui_kit::PlatformTarget::Native,
        });
        cx.update(gpui_kit::init);
        cx.update(crate::init);
        cx.update(AppState::install_in_memory);
        let (handle, workspace) = cx.update(|cx| {
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| Workspace::new(window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| w.open_local_terminal(window, cx))
        })
        .unwrap();
        let panel = cx
            .update(|cx| workspace.read(cx).live_terminals().pop())
            .expect("打开了本地终端");
        poll_until(cx, "本地终端启动", |cx| {
            cx.update(|cx| panel.read(cx).view.is_some())
        });
        let view = cx.update(|cx| panel.read(cx).view.clone().unwrap());
        let typed = Rc::new(RefCell::new(Vec::<u8>::new()));
        let _typed = cx.update(|cx| {
            let typed = typed.clone();
            cx.subscribe(&view, move |_, event: &TerminalViewEvent, _| {
                if let TerminalViewEvent::Typed(bytes) = event {
                    typed.borrow_mut().extend_from_slice(bytes);
                }
            })
        });
        cx.update(|cx| view.update(cx, |view, _| view.set_forward_input(true)));
        // ⌥B：macOS 给出的输入文字是 ∫
        let option_b = gpui_kit::Keystroke {
            key_char: Some("∫".into()),
            ..gpui_kit::Keystroke::parse("alt-b").unwrap()
        };
        let press = |cx: &mut gpui_kit::TestAppContext| {
            typed.borrow_mut().clear();
            cx.update_window(handle, |_, window, cx| {
                view.read(cx).focus_handle(cx).focus(window, cx);
                window.dispatch_keystroke(option_b.clone(), cx);
            })
            .unwrap();
            cx.run_until_parked();
            typed.borrow().clone()
        };
        assert_eq!(press(cx), "∫".as_bytes(), "默认照系统的习惯输入组合字符");
        cx.update(|cx| {
            AppState::store(cx)
                .update_settings(|s| s.terminal.option_as_meta = true)
                .unwrap()
        });
        // 设置的变更事件经异步任务转发给工作区，再重新给各终端应用样式
        cx.run_until_parked();
        assert_eq!(press(cx), b"\x1bb", "开启后发送 ESC 前缀");
    }
}
