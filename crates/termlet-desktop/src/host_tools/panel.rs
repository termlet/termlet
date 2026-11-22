//! 主机工具面板：窗口右侧的停靠栏，标题“工具 · 主机名”，跟随显示着并最近获得焦点的终端（焦点移进面板、侧边栏、
//! 对话框时不变；工作区经 [`HostToolsPanel::set_target`] 告知）。五页：概览、进程、服务、Docker、网络；都在终端
//! 自己的连接上另开通道执行，不另外登录、不往服务器写文件。
//! 概览页的资源取自终端自己的资源监控（状态条用的同一份采样）；点终端底部的状态条时工作区打开这一页
//! （[`HostToolsPanel::show_page`]）。概览页右上角的按钮把跟随的终端所连主机的概览弹出成置顶的小窗
//! （[`HostToolsPanel::pop_out_overview`]，见 [`super::popout`]），小窗固定那台主机，不随面板换终端。
//!
//! - 左边缘可拖动调整宽度（双击恢复默认，获得焦点后左右方向键调整），宽度与当前页记在本机的界面状态中（另按主机记下
//!   各自上次的页：跟随到记下的主机时打开它，没有记下的用全局的上次页）；
//!   是否展开由工作区记（`host_tools_open`），面板据此决定是否读取。最小宽度随页签放宽：顶部的各页与当前页的筛选
//!   页签按当前语言的文案放不下时（英文的文案比中文长），按它们实际排开的宽度放宽（[`crate::tab_fit`]）。
//! - 换了终端时各页重建（搜索、排序、筛选、选中都重置），打开着的对话框关闭；同一个终端只是状态变了时更新连接：
//!   连上时读取，重连中显示上次的数据、不读取。切回某个终端时先显示它各页上次的结果，再重新读取。
//! - 窗口不可见（最小化、被完全挡住）时不读取；只是失焦照常读取。
//! - 不可用时（没有终端、不支持的终端、只用一个通道、连接中、已断开）用中间区域那一套空状态说明原因与下一步（没有
//!   终端时“快速连接…”与最近打开的几台主机，已断开时“重新连接”），顶部的页签置灰、不能切换。
//! - 写操作在这里执行（换了终端也照常执行完并提示结果）：各页的确认框写明主机（[`HostToolsPanel::host_of`]）与将执行
//!   的命令，进行中、成功与失败由发起的页在行上与信息行下说明（失败的原因可以展开看完整的输出；那一页不在显示时另弹
//!   提示，放在窗口右下角，不盖住面板的标题）；需要 root 时说明原因与在终端中实际执行的命令，可以改在发起操作的终端旁
//!   的新 Tab 中执行。这条连接上没有免密码 sudo 时（`Privilege::Limited`），需要 root 的操作在确认框中直接以“在终端中
//!   执行”为主按钮（[`HostToolsPanel::run_root_first`]），“仍然尝试”照常在后台执行。
//! - 监听的 HTTP 端口可以“在浏览器中打开”（[`HostToolsEvent::OpenInBrowser`]）：工作区经现有的转发规则转发到本机的
//!   空闲端口，随这条连接结束而停，再交给系统的浏览器。
//! - 服务、容器的日志在中间区域的 Tab 中查看（[`HostToolsEvent::OpenLogs`]，见 [`super::LogPanel`]）：它在打开它的终端的
//!   连接上读取，面板换了终端也照常。
//! - 各页之间互相跳转（`show_process`、`show_service`、`show_container`、`show_connections` 等）：切到目标页、选中目标
//!   （还没读到时读到后再选中，不在列表中时那一页说明），焦点进那一页的列表。
//! - 键盘（见 [`super::keys`]）：面板的上下文中 Ctrl+PageUp / PageDown 切换页、Ctrl+F 到搜索框，列表中 Esc 把焦点还给
//!   终端（[`HostToolsEvent::FocusTerminal`]）；各页的列表自己处理移动、Enter、Delete 等。

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use futures::channel::oneshot;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _, StyledExt as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    Anchor, AnyElement, App, AppContext as _, ClickEvent, Context, DragMoveEvent, Entity, EntityId,
    EventEmitter, FocusHandle, InteractiveElement as _, IntoElement, KeyDownEvent,
    ParentElement as _, Pixels, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, TestSupportExt as _, WeakEntity,
    Window, div, prelude::FluentBuilder as _, px, rems,
};
use termlet_core::hosttools::{
    Action, LogSource, SudoState, Target, TerminalRequest, ToolsError, terminal_command,
};
use termlet_core::id::HostId;
use termlet_core::model::{Host, IconStyle, Protocol};

use super::dialogs::{self, RootRequest};
use super::docker::{ContainerTarget, DockerPage};
use super::keys::{
    FocusList, FocusSearch, FocusTerminal, LIST_CONTEXT, NextPage, PANEL_CONTEXT, PrevPage,
};
use super::log_panel::LogTab;
use super::model::{self, BrowserPreset, ConnChip, ForwardPreset, Page};
use super::network::NetworkPage;
use super::overview::{Layout, OverviewPage};
use super::processes::ProcessPage;
use super::services::ServicePage;
use super::shared::{
    OpFailure, OpOutcome, OpenDialogs, PageContext, ToolCache, brief, brief_detail, new_cache,
};
use super::target::{HostLabel, ToolsState, ToolsTarget};
use super::{audit, errors};
use crate::app_state::AppState;
use crate::brand_icons;
use crate::prose::prose;
use crate::t;
use crate::tab_fit::{self, NeededWidth};
use crate::terminal_panel::TerminalPanel;

/// 面板请工作区做的事。
pub enum HostToolsEvent {
    /// 在共用 `terminal` 的连接的新 Tab 中执行 `command`（跟随日志、进入容器、需要 root 时在终端中执行），
    /// Tab 标题为 `title`。`terminal` 已经确认还在；不是 SSH、WSL 终端时工作区提示 `tools.source_gone`、不执行。
    /// `refresh` 为命令结束后要重读的页（需要 root 时的操作，见 [`refresh_after_run`]）
    RunInTerminal {
        terminal: WeakEntity<TerminalPanel>,
        command: String,
        title: SharedString,
        refresh: Option<Page>,
    },
    /// 网络页的“转发到本机…”：打开新建本地转发的对话框，经由 `host`，目标为 `target_host:port`，本机也监听
    /// `port`，名称为 `name`（可能为空），可以共用 `terminal` 的连接
    ForwardLocal {
        host: HostId,
        terminal: WeakEntity<TerminalPanel>,
        port: u16,
        target_host: String,
        name: String,
    },
    /// 网络页、Docker 详情的“在浏览器中打开”：经由 `host` 把 `target_host:port` 转发到本机的空闲端口（已有同样的转发
    /// 规则时沿用它），共用 `terminal` 的连接、随连接结束而停，建好后用系统的浏览器打开（`https` 时用 https）；
    /// `name` 是新建的规则的名称（可能为空）
    OpenInBrowser {
        host: HostId,
        terminal: WeakEntity<TerminalPanel>,
        target_host: String,
        port: u16,
        https: bool,
        name: String,
    },
    /// 在中间区域的 Tab 中查看服务或容器的日志（同一个已打开时切过去）
    OpenLogs(LogTab),
    /// 为 `terminal` 打开 SFTP（共用它的连接），从 `path` 开始（Docker 的绑定挂载）
    OpenSftp {
        terminal: WeakEntity<TerminalPanel>,
        path: String,
    },
    /// 收起面板（焦点回到当前终端）
    Close,
    /// 焦点还给面板跟随的终端（列表中按 Esc），面板照常展开
    FocusTerminal,
    /// 没有终端时的“快速连接…”：打开命令面板（可以输入 用户@地址）
    QuickConnect,
    /// 没有终端时列出的最近的主机：打开它
    OpenHost(HostId),
}

/// 面板不可用（各页不显示、页签置灰）的情况：说明与下一步用中间区域那一套空状态的样式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unavailable {
    /// 没有终端：快速连接、最近的主机
    NoTerminal,
    /// 本地终端、Telnet、串口
    Unsupported,
    /// 只用一个通道的主机、外部程序调起的临时连接
    SingleChannel,
    Connecting,
    /// 会话已结束、连接失败：重新连接
    Ended,
}

impl Unavailable {
    fn key(self) -> &'static str {
        match self {
            Unavailable::NoTerminal => "no_terminal",
            Unavailable::Unsupported => "unsupported",
            Unavailable::SingleChannel => "single_channel",
            Unavailable::Connecting => "connecting",
            Unavailable::Ended => "ended",
        }
    }
}

/// 没有终端时列出的最近的主机数。
const RECENT_SHOWN: usize = 3;

/// 一个写操作。
pub(super) struct ActRequest {
    pub action: Action,
    /// 操作的说明（“重启服务 nginx”）：失败的提示、需要 root 时的说明、在终端中执行时的 Tab 标题
    pub title: SharedString,
    /// 审计记录中的名称（进程名、容器名等）
    pub label: String,
    /// Docker 的操作：需要 root 时另提示加入 docker 组
    pub docker: bool,
    /// 发起操作的页：成功时那一页还显示着（同一个终端、面板展开）就由它在行上说明，不再弹出提示
    pub page: Page,
}

/// 跟随的终端的各页。
struct Pages {
    id: EntityId,
    overview: Entity<OverviewPage>,
    processes: Entity<ProcessPage>,
    services: Entity<ServicePage>,
    docker: Entity<DockerPage>,
    network: Entity<NetworkPage>,
}

/// 记下各台主机上次看的页（与主机概览小窗）用的键：保存的主机按 ID，快速连接等没有保存的按登录的用户与地址，再没有
/// 时按名称。
pub(super) fn host_key(target: &ToolsTarget) -> String {
    match (&target.ssh_host, &target.login) {
        (Some(host), _) => host.as_str().to_string(),
        (None, Some(login)) => login.to_string(),
        (None, None) => target.name.to_string(),
    }
}

/// 两次告知的是不是同一个情况：同一个终端、名称、主机与状态，连着时是同一条连接。
fn same_target(a: Option<&ToolsTarget>, b: Option<&ToolsTarget>) -> bool {
    let (Some(a), Some(b)) = (a, b) else {
        return a.is_none() && b.is_none();
    };
    let same_state = match (&a.state, &b.state) {
        (ToolsState::Unsupported, ToolsState::Unsupported)
        | (ToolsState::SingleChannel, ToolsState::SingleChannel)
        | (ToolsState::Connecting, ToolsState::Connecting)
        | (ToolsState::Reconnecting, ToolsState::Reconnecting)
        | (ToolsState::Ended, ToolsState::Ended) => true,
        (ToolsState::Connected(a), ToolsState::Connected(b)) => match (a, b) {
            (
                Target::Ssh {
                    connection: a,
                    single_channel: sa,
                },
                Target::Ssh {
                    connection: b,
                    single_channel: sb,
                },
            ) => std::sync::Arc::ptr_eq(a, b) && sa == sb,
            (
                Target::Wsl {
                    distro: da,
                    user: ua,
                },
                Target::Wsl {
                    distro: db,
                    user: ub,
                },
            ) => da == db && ua == ub,
            _ => false,
        },
        _ => false,
    };
    same_state && a.id == b.id && a.name == b.name && a.login == b.login && a.ssh_host == b.ssh_host
}

/// 拖动左边缘调整宽度。
struct ResizeDrag;

impl Render for ResizeDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

pub struct HostToolsPanel {
    target: Option<ToolsTarget>,
    page: Page,
    pages: Option<Pages>,
    cache: Rc<RefCell<ToolCache>>,
    dialogs: OpenDialogs,
    /// 面板展开着（工作区记在界面状态中）
    open: bool,
    /// 窗口可见（没有最小化、没有被完全挡住）
    visible: bool,
    resize_focus: FocusHandle,
    /// 面板不可用（没有终端、不支持等）时焦点放在空状态上：Esc 照常可用
    focus: FocusHandle,
    /// 顶部各页的页签的滚动状态：按它量页签排开的宽度
    tabs_scroll: ScrollHandle,
    /// 页签全部显示所需的面板宽度
    tabs_width: NeededWidth,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<HostToolsEvent> for HostToolsPanel {}

impl HostToolsPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let ui = AppState::ui(cx);
        let state = ui.read(cx).state();
        let page = Page::from_saved(state.host_tools_page.as_deref());
        let open = state.host_tools_open;
        let subscriptions = vec![
            cx.observe(&ui, |this, ui, cx| {
                let open = ui.read(cx).state().host_tools_open;
                if this.open != open {
                    this.open = open;
                    this.sync_active(cx);
                }
                // 宽度变了
                cx.notify();
            }),
            cx.observe_window_visibility(window, |this, visibility, _, cx| {
                let visible = visibility.is_visible();
                if this.visible != visible {
                    this.visible = visible;
                    this.sync_active(cx);
                }
            }),
        ];
        Self {
            target: None,
            page,
            pages: None,
            cache: new_cache(),
            dialogs: OpenDialogs::default(),
            open,
            visible: window.is_visible(),
            resize_focus: cx.focus_handle().tab_stop(true),
            focus: cx.focus_handle(),
            tabs_scroll: ScrollHandle::new(),
            tabs_width: NeededWidth::default(),
            _subscriptions: subscriptions,
        }
    }

    /// 跟随的终端或它的状态变了（工作区在焦点所在的终端变化、终端连接状态变化时调用，可以频繁调用）。
    /// 换了终端时各页重建、打开着的对话框关闭；同一个终端只更新状态。
    pub fn set_target(
        &mut self,
        target: Option<ToolsTarget>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 工作区在焦点、终端状态变化时都会告知，多数时候什么也没变：不重建、不重绘
        if same_target(self.target.as_ref(), target.as_ref()) {
            return;
        }
        let changed = self.target.as_ref().map(|t| t.id) != target.as_ref().map(|t| t.id);
        if changed {
            tracing::debug!(target = ?target.as_ref().map(|t| t.name.to_string()), "主机工具跟随的终端变了");
            // 从最上面的开始关：关掉一个后下面的回到最上面
            for dialog in self.dialogs.take().into_iter().rev() {
                dialog.close(window, cx);
            }
            self.pages = None;
            // 打开这台主机上次看的页；没有记下时用全局的上次页
            if let Some(target) = &target {
                let state = AppState::global(cx).ui.read(cx).state();
                let saved = state
                    .host_tools_page_of(&host_key(target))
                    .or(state.host_tools_page.as_deref());
                self.page = Page::from_saved(saved);
            }
        }
        self.target = target;
        self.ensure_pages(window, cx);
        if !changed && let Some(pages) = &self.pages {
            let connection = self.target.as_ref().and_then(|t| t.state.target().cloned());
            pages
                .overview
                .update(cx, |page, cx| page.set_connection(connection.clone(), cx));
            pages
                .processes
                .update(cx, |page, cx| page.set_connection(connection.clone(), cx));
            pages
                .services
                .update(cx, |page, cx| page.set_connection(connection.clone(), cx));
            pages
                .docker
                .update(cx, |page, cx| page.set_connection(connection.clone(), cx));
            pages
                .network
                .update(cx, |page, cx| page.set_connection(connection, cx));
        }
        self.sync_active(cx);
        cx.notify();
    }

    /// `terminal` 的主机（写操作的确认框写明）：面板跟随的就是它时为 `Some`。
    pub(super) fn host_of(&self, terminal: EntityId) -> Option<HostLabel> {
        self.target
            .as_ref()
            .filter(|t| t.id == terminal)
            .map(|t| HostLabel {
                name: t.name.clone(),
                login: t.login.clone(),
            })
    }

    /// 把跟随的终端所连主机的概览弹出成置顶的小窗（概览页右上角的按钮、命令面板）；这台主机的小窗已经开着时拿到
    /// 前面。没有终端、终端没有主机工具时什么也不做。
    pub fn pop_out_overview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.target.clone() else {
            return;
        };
        if matches!(
            target.state,
            ToolsState::Unsupported | ToolsState::SingleChannel
        ) {
            return;
        }
        super::popout::pop_out(&target, self.cache.clone(), window, cx);
    }

    /// 终端关闭了：清除它各页的缓存。
    pub fn forget(&mut self, terminal: EntityId) {
        self.cache.borrow_mut().forget(|(id, _)| *id == terminal);
    }

    /// 跟随的终端能用主机工具（SSH、WSL，且不是只用一个通道）时建好它的各页。
    fn ensure_pages(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = &self.target else {
            return;
        };
        if self.pages.is_some()
            || matches!(
                target.state,
                ToolsState::Unsupported | ToolsState::SingleChannel
            )
        {
            return;
        }
        let context = PageContext {
            panel: cx.entity().downgrade(),
            terminal: target.terminal.clone(),
            terminal_id: target.id,
            tools: target.tools.clone(),
            target: target.state.target().cloned(),
            ssh_host: target.ssh_host.clone(),
            cache: self.cache.clone(),
            dialogs: self.dialogs.clone(),
        };
        self.pages = Some(Pages {
            id: target.id,
            overview: cx.new(|cx| OverviewPage::new(context.clone(), Layout::Panel, window, cx)),
            processes: cx.new(|cx| ProcessPage::new(context.clone(), window, cx)),
            services: cx.new(|cx| ServicePage::new(context.clone(), window, cx)),
            docker: cx.new(|cx| DockerPage::new(context.clone(), window, cx)),
            network: cx.new(|cx| NetworkPage::new(context, window, cx)),
        });
    }

    /// 各页是否在显示：面板展开、窗口可见、终端连接着、是当前页。
    fn sync_active(&mut self, cx: &mut Context<Self>) {
        let Some(pages) = &self.pages else {
            return;
        };
        let connected = self
            .target
            .as_ref()
            .is_some_and(|t| t.state.target().is_some() && t.id == pages.id);
        let shown = self.open && self.visible && connected;
        let page = self.page;
        pages.overview.update(cx, |p, cx| {
            p.set_active(shown && page == Page::Overview, cx)
        });
        pages.processes.update(cx, |p, cx| {
            p.set_active(shown && page == Page::Processes, cx)
        });
        pages.services.update(cx, |p, cx| {
            p.set_active(shown && page == Page::Services, cx)
        });
        pages
            .docker
            .update(cx, |p, cx| p.set_active(shown && page == Page::Docker, cx));
        pages
            .network
            .update(cx, |p, cx| p.set_active(shown && page == Page::Network, cx));
    }

    fn set_page(&mut self, page: Page, cx: &mut Context<Self>) {
        if self.page == page {
            return;
        }
        self.page = page;
        let key = page.key().to_string();
        let host = self.target.as_ref().map(host_key);
        AppState::ui(cx).update(cx, |ui, cx| {
            ui.update(
                |s| {
                    if let Some(host) = &host {
                        s.note_host_tools_page(host, &key);
                    }
                    s.host_tools_page = Some(key);
                },
                cx,
            )
        });
        self.sync_active(cx);
        cx.notify();
    }

    /// 切到某一页（点状态条时打开概览页）。
    pub fn show_page(&mut self, page: Page, cx: &mut Context<Self>) {
        self.set_page(page, cx);
    }

    /// 切到某一页并把焦点放进它的列表（“打开主机工具的概览”）；面板不可用（没有终端、不支持等）时焦点放在空状态上，
    /// Esc 照常可用。
    pub fn show_page_focused(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        self.set_page(page, cx);
        self.focus_page(window, cx);
    }

    /// 焦点进当前页的列表。
    pub(super) fn focus_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = match (&self.pages, self.unavailable()) {
            (Some(pages), None) => match self.page {
                Page::Overview => pages.overview.read(cx).list_focus(),
                Page::Processes => pages.processes.read(cx).list_focus(),
                Page::Services => pages.services.read(cx).list_focus(),
                Page::Docker => pages.docker.read(cx).list_focus(),
                Page::Network => pages.network.read(cx).list_focus(),
            },
            _ => self.focus.clone(),
        };
        focus.focus(window, cx);
        cx.notify();
    }

    /// Ctrl+PageUp / PageDown：切到上一页、下一页（循环），焦点进那一页的列表。面板不可用时页签置灰，不切换。
    fn switch_page(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.unavailable().is_some() {
            return;
        }
        let len = Page::ALL.len();
        let ix = self.page.index();
        let next = if forward {
            (ix + 1) % len
        } else {
            (ix + len - 1) % len
        };
        self.set_page(Page::ALL[next], cx);
        self.focus_page(window, cx);
    }

    /// Ctrl+F、/：焦点到当前页的搜索框（概览页没有搜索框）。
    fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pages) = self.pages.as_ref().filter(|_| self.unavailable().is_none()) else {
            return;
        };
        let search = match self.page {
            Page::Overview => None,
            Page::Processes => pages.processes.read(cx).search(),
            Page::Services => pages.services.read(cx).search(),
            Page::Docker => pages.docker.read(cx).search(),
            Page::Network => pages.network.read(cx).search(),
        };
        if let Some(search) = search {
            search.update(cx, |search, cx| search.focus(window, cx));
        }
    }

    /// 跳到进程页并选中这个进程（网络页的进程、服务的主进程、概览页的进程）。
    pub(super) fn show_process(&mut self, pid: u32, window: &mut Window, cx: &mut Context<Self>) {
        self.set_page(Page::Processes, cx);
        if let Some(pages) = &self.pages {
            pages
                .processes
                .update(cx, |page, cx| page.focus_pid(pid, window, cx));
        }
        self.focus_page(window, cx);
    }

    /// 跳到进程页，只列出这个容器内的进程（容器详情的“查看进程”）；`label` 是筛选条件上显示的容器名。
    pub(super) fn show_container_processes(
        &mut self,
        id: String,
        label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_page(Page::Processes, cx);
        if let Some(pages) = &self.pages {
            pages
                .processes
                .update(cx, |page, cx| page.filter_container(id, label, window, cx));
        }
        self.focus_page(window, cx);
    }

    /// 跳到服务页并选中这个服务（进程的“所属服务”）。
    pub(super) fn show_service(
        &mut self,
        unit: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_page(Page::Services, cx);
        if let Some(pages) = &self.pages {
            pages
                .services
                .update(cx, |page, cx| page.focus_unit(unit, window, cx));
        }
        self.focus_page(window, cx);
    }

    /// 跳到 Docker 页的容器子页并选中这个容器（进程的“所属容器”）。
    pub(super) fn show_container(
        &mut self,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_page(Page::Docker, cx);
        if let Some(pages) = &self.pages {
            pages.docker.update(cx, |page, cx| {
                page.focus_container(ContainerTarget::Id(id), window, cx)
            });
        }
        self.focus_page(window, cx);
    }

    /// 跳到 Docker 页并选中发布了这个端口的容器（网络页的监听端口）。
    pub(super) fn show_container_port(
        &mut self,
        proto: String,
        port: u16,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_page(Page::Docker, cx);
        if let Some(pages) = &self.pages {
            pages.docker.update(cx, |page, cx| {
                page.focus_container(ContainerTarget::Port(proto, port), window, cx)
            });
        }
        self.focus_page(window, cx);
    }

    /// 跳到网络页，按进程或端口筛选（进程的“查看连接”、服务主进程的连接、容器端口的连接）。
    pub(super) fn show_connections(
        &mut self,
        chip: ConnChip,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_page(Page::Network, cx);
        if let Some(pages) = &self.pages {
            pages
                .network
                .update(cx, |page, cx| page.set_chip(chip, window, cx));
        }
        self.focus_page(window, cx);
    }

    // ------------------------------------------------------------ 写操作与在终端中执行

    /// 执行写操作并说明结果：发起的页由返回的接收端得到结果（成功、失败的原因，需要 root 时打开说明、算作没有执行），
    /// 在行上与信息行下说明；那一页不在显示时另弹提示。页已重建（换了终端）时照常执行完并提示。
    pub(super) fn perform(
        &mut self,
        context: &PageContext,
        request: ActRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<OpOutcome> {
        let (done, receiver) = oneshot::channel();
        let Some(target) = context.target.clone() else {
            let inline = self.shows_page(context.terminal_id, request.page);
            let failure =
                OpFailure::new(request.title.clone(), t!("tools.error.disconnected"), None);
            self.announce_failure(&failure, inline, window, cx);
            let _ = done.send(OpOutcome::Failed(failure));
            return receiver;
        };
        let tools = context.tools.clone();
        let terminal = context.terminal.clone();
        let host = self.host_name(context);
        let label = self.host_of(context.terminal_id);
        let terminal_id = context.terminal_id;
        let command = request.action.command_line().unwrap_or_default();
        let log = crate::cli_bridge::audit(cx);
        cx.spawn_in(window, async move |this, cx| {
            let started = Instant::now();
            let result = tools.act(&target, request.action.clone()).await;
            audit::record(
                log,
                audit::action_entry(&host, &command, &request.label, &result, started.elapsed()),
            );
            let outcome = this
                .update_in(cx, |this, window, cx| {
                    let inline = this.shows_page(terminal_id, request.page);
                    this.report(request, terminal, (host, label), result, inline, window, cx)
                })
                .unwrap_or(OpOutcome::Aborted);
            let _ = done.send(outcome);
        })
        .detach();
        receiver
    }

    /// 发起操作的页还显示着：面板展开、窗口可见，跟随的还是那个终端、当前页就是那一页（成功时由那一页在行上说明）。
    fn shows_page(&self, terminal: EntityId, page: Page) -> bool {
        self.open
            && self.visible
            && self.page == page
            && self.unavailable().is_none()
            && self.pages.as_ref().is_some_and(|p| p.id == terminal)
    }

    /// 写操作的结果：成功时发起的页还显示着（`inline`）就由它在行上写“已重启”，否则轻提示一下（自动消失）；失败时
    /// 由发起的页在行上与信息行下说明原因（不在显示时另弹提示，见 [`Self::announce_failure`]），需要 root 时打开说明。
    /// `host` 是审计记录的主机名与确认框写明的主机。
    #[allow(clippy::too_many_arguments)]
    fn report(
        &mut self,
        request: ActRequest,
        terminal: WeakEntity<TerminalPanel>,
        (host, label): (String, Option<HostLabel>),
        result: Result<termlet_core::hosttools::ActionResult, ToolsError>,
        inline: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> OpOutcome {
        let mut fail = |reason: SharedString, detail: Option<String>, this: &mut Self| {
            let failure = OpFailure::new(request.title.clone(), reason, detail);
            this.announce_failure(&failure, inline, window, cx);
            OpOutcome::Failed(failure)
        };
        match result {
            Ok(result) if result.ok => {
                tracing::info!(action = %request.title, host = %host, "主机工具：操作完成");
                if !inline {
                    window.push_notification(
                        Notification::success(t!("tools.action_done", action = request.title)),
                        cx,
                    );
                }
                OpOutcome::Done
            }
            Ok(result) => {
                // 远端的输出只显示给用户，不写进日志
                tracing::warn!(action = %request.title, host = %host, exit_code = result.exit_code, "主机工具：操作失败");
                // 原因写输出的第一行（“Job for nginx.service failed …”），完整的输出可以展开看
                let full = result.output.trim();
                let first = full.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                let reason = brief(first);
                if reason.is_empty() {
                    fail(t!("tools.exit_code", code = result.exit_code), None, self)
                } else {
                    fail(reason.into(), Some(brief_detail(full)), self)
                }
            }
            Err(ToolsError::NeedRoot { sudo, .. }) => {
                tracing::info!(action = %request.title, host = %host, ?sudo, "主机工具：操作需要 root");
                dialogs::open_root(
                    RootRequest {
                        sudo,
                        action: request.action,
                        docker: request.docker,
                        title: request.title,
                        label: request.label,
                        terminal,
                        host,
                        host_label: label,
                        page: request.page,
                    },
                    cx.entity().downgrade(),
                    self.dialogs.clone(),
                    window,
                    cx,
                );
                OpOutcome::Aborted
            }
            // 写操作超时：命令可能已经发出，结果未知
            Err(ToolsError::Exec(termlet_core::hosttools::ExecError::Timeout)) => {
                tracing::warn!(action = %request.title, host = %host, "主机工具：操作超时，结果未知");
                fail(t!("tools.error.action_timeout"), None, self)
            }
            Err(error) => {
                tracing::warn!(action = %request.title, host = %host, "主机工具：操作失败：{error}");
                fail(errors::describe(&error), None, self)
            }
        }
    }

    /// 操作失败：发起的页还显示着（`inline`）时由它在行上与信息行下说明，不弹提示；否则弹出提示，放在窗口右下角
    /// （面板在右侧，右上角的提示会盖住面板的标题）。提示写进日志（经 `notify` 的同样写法），远端的输出不写。
    fn announce_failure(
        &mut self,
        failure: &OpFailure,
        inline: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if inline {
            return;
        }
        tracing::warn!(action = %failure.action, "提示用户：主机工具的操作失败");
        window.push_notification(
            Notification::error(t!(
                "tools.action_failed",
                action = failure.action.clone(),
                output = failure.reason.clone()
            ))
            .placement(Anchor::BottomRight),
            cx,
        );
    }

    /// 这条连接上没有免密码 sudo 时，确认框的主按钮“在终端中执行”：不先在后台尝试，直接在发起操作的终端旁的新 Tab
    /// 中执行（sudo 的密码在那里输入；不允许用 sudo、没有 sudo 时用 `su -c`），记审计。
    pub(super) fn run_root_first(
        &mut self,
        context: &PageContext,
        request: ActRequest,
        sudo: SudoState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let terminal_request = super::shared::root_first_request(&request.action, sudo);
        let root = RootRequest {
            sudo,
            action: request.action,
            docker: request.docker,
            title: request.title,
            label: request.label,
            terminal: context.terminal.clone(),
            host: self.host_name(context),
            host_label: self.host_of(context.terminal_id),
            page: request.page,
        };
        tracing::info!(action = %root.title, ?sudo, "主机工具：没有免密码 sudo，在终端中执行");
        self.run_root(&root, terminal_request, window, cx);
    }

    /// 需要 root 时的“在终端中执行”：只用发起操作的终端，记审计。
    pub(super) fn run_root(
        &mut self,
        root: &RootRequest,
        request: TerminalRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match terminal_command(&request) {
            Ok(command) => {
                // 先确认发起的终端还在：不在时命令不会发出，也就不记“交给终端执行”
                if root.terminal.upgrade().is_none() {
                    crate::notify::warning(t!("tools.source_gone"), window, cx);
                    return;
                }
                audit::record(
                    crate::cli_bridge::audit(cx),
                    audit::terminal_entry(&root.host, &command, &root.label),
                );
                let title = t!("tools.tab_run", action = root.title.clone());
                self.run_in_terminal(
                    root.terminal.clone(),
                    command,
                    title,
                    Some(root.page),
                    window,
                    cx,
                );
            }
            Err(error) => crate::notify::error(errors::describe(&error), window, cx),
        }
    }

    /// 在页所跟随的终端旁的新 Tab 中执行（跟随日志、进入容器）。
    pub(super) fn run_request(
        &mut self,
        context: &PageContext,
        request: TerminalRequest,
        title: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match terminal_command(&request) {
            Ok(command) => {
                self.run_in_terminal(context.terminal.clone(), command, title, None, window, cx)
            }
            Err(error) => crate::notify::error(errors::describe(&error), window, cx),
        }
    }

    /// 发起的终端已关闭时提示、不执行。`refresh` 为命令结束后要重读的页。
    fn run_in_terminal(
        &mut self,
        terminal: WeakEntity<TerminalPanel>,
        command: String,
        title: SharedString,
        refresh: Option<Page>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if terminal.upgrade().is_none() {
            crate::notify::warning(t!("tools.source_gone"), window, cx);
            return;
        }
        cx.emit(HostToolsEvent::RunInTerminal {
            terminal,
            command,
            title,
            refresh,
        });
    }

    /// “在终端中执行”的命令结束后（见 [`refresh_after_run`]）：跟随的还是发起操作的终端时重读那一页。
    pub(crate) fn refresh_page_of(
        &mut self,
        terminal: EntityId,
        page: Page,
        cx: &mut Context<Self>,
    ) {
        let Some(pages) = self.pages.as_ref().filter(|p| p.id == terminal) else {
            return;
        };
        tracing::debug!(?page, "主机工具：在终端中执行的命令结束，重读这一页");
        use super::shared::refresh_if_active;
        match page {
            Page::Overview => pages.overview.update(cx, refresh_if_active),
            Page::Processes => pages.processes.update(cx, refresh_if_active),
            Page::Services => pages.services.update(cx, refresh_if_active),
            Page::Docker => pages.docker.update(cx, refresh_if_active),
            Page::Network => pages.network.update(cx, refresh_if_active),
        }
    }

    /// 网络页的“转发到本机…”。
    pub(super) fn forward(
        &mut self,
        context: &PageContext,
        preset: ForwardPreset,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = context.ssh_host.clone() else {
            return;
        };
        cx.emit(HostToolsEvent::ForwardLocal {
            host,
            terminal: context.terminal.clone(),
            port: preset.port,
            target_host: preset.target_host,
            name: preset.name,
        });
    }

    /// “在浏览器中打开”：交给工作区转发到本机并打开（只对保存的 SSH 主机提供）。
    pub(super) fn open_in_browser(
        &mut self,
        context: &PageContext,
        preset: BrowserPreset,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = context.ssh_host.clone() else {
            return;
        };
        cx.emit(HostToolsEvent::OpenInBrowser {
            host,
            terminal: context.terminal.clone(),
            target_host: preset.forward.target_host,
            port: preset.forward.port,
            https: preset.https,
            name: preset.forward.name,
        });
    }

    /// 查看服务或容器的日志：在中间区域打开日志查看器的 Tab（`name` 为 Tab 标题中的名称）。终端已关闭时由工作区提示。
    pub(super) fn open_logs(
        &mut self,
        context: &PageContext,
        source: LogSource,
        name: String,
        cx: &mut Context<Self>,
    ) {
        let host = self
            .target
            .as_ref()
            .filter(|t| t.id == context.terminal_id)
            .map(|t| t.name.clone())
            .unwrap_or_default();
        cx.emit(HostToolsEvent::OpenLogs(LogTab {
            terminal: context.terminal.clone(),
            terminal_id: context.terminal_id,
            host,
            name,
            source,
        }));
    }

    /// 在 SFTP 中打开服务器上的目录（Docker 的绑定挂载）：新 Tab，共用页所跟随的终端的连接。
    pub(super) fn open_sftp(
        &mut self,
        context: &PageContext,
        path: String,
        cx: &mut Context<Self>,
    ) {
        cx.emit(HostToolsEvent::OpenSftp {
            terminal: context.terminal.clone(),
            path,
        });
    }

    /// 审计记录的主机：页所跟随的终端的名称。
    fn host_name(&self, context: &PageContext) -> String {
        self.target
            .as_ref()
            .filter(|t| t.id == context.terminal_id)
            .map(|t| t.name.to_string())
            .unwrap_or_default()
    }

    // ------------------------------------------------------------ 宽度

    /// 面板的宽度（像素）：记下的宽度按界面大小缩放，限制在最小与最大之间，并给左侧留出空间。
    fn width(&self, window: &Window, cx: &App) -> f32 {
        let saved = model::saved_width(AppState::global(cx).ui.read(cx).state().host_tools_width);
        self.clamp_width(saved * crate::ui_scale::factor(window), window)
    }

    /// 把宽度（像素）限制在 [`model::clamp_width`] 的范围内。最小宽度为 [`model::WIDTH_MIN`]，页签按当前语言的文案
    /// 放不下时放宽到放得下。
    fn clamp_width(&self, width: f32, window: &Window) -> f32 {
        let scale = crate::ui_scale::factor(window);
        let min = (model::WIDTH_MIN * scale).max(self.tabs_width.get().unwrap_or(0.));
        model::clamp_width(width, f32::from(window.viewport_size().width), min, scale)
    }

    /// 顶部各页的页签与当前页的筛选页签（显示着时）中，放不下的部分最大的那个（[`tab_fit::overflow`]）。
    pub(super) fn tabs_overflow(&self) -> impl FnOnce(&App) -> Option<Pixels> + 'static {
        let pages_scroll = self.tabs_scroll.clone();
        let page = self.page;
        let shown = match (self.unavailable(), &self.pages) {
            (None, Some(pages)) => Some((
                pages.services.clone(),
                pages.docker.clone(),
                pages.network.clone(),
            )),
            _ => None,
        };
        move |cx| {
            let pages = tab_fit::overflow(&pages_scroll, Page::ALL.len());
            let filter = shown.and_then(|(services, docker, network)| match page {
                Page::Overview | Page::Processes => None,
                Page::Services => services.read(cx).tabs_overflow(),
                Page::Docker => docker.read(cx).tabs_overflow(),
                Page::Network => network.read(cx).tabs_overflow(),
            });
            match (pages, filter) {
                (Some(pages), Some(filter)) => Some(pages.max(filter)),
                (pages, filter) => pages.or(filter),
            }
        }
    }

    /// 记下宽度（像素，换算成 100% 的界面大小时的宽度）；`None` 为恢复默认。
    fn set_width(&mut self, width: Option<f32>, window: &Window, cx: &mut Context<Self>) {
        let scale = crate::ui_scale::factor(window);
        let width = width.map(|w| (w / scale).round());
        AppState::ui(cx).update(cx, |ui, cx| ui.update(|s| s.host_tools_width = width, cx));
    }

    /// 向左拖变宽：宽度为面板右边缘到指针的距离。
    fn on_resize_drag(
        &mut self,
        event: &DragMoveEvent<ResizeDrag>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let right = event.bounds.origin.x + event.bounds.size.width;
        let width = self.clamp_width(f32::from(right - event.event.position.x), window);
        self.set_width(Some(width), window, cx);
    }

    /// 拖动条获得焦点时：← 变宽、→ 变窄。
    fn on_resize_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let step = match event.keystroke.key.as_str() {
            "left" => model::WIDTH_STEP,
            "right" => -model::WIDTH_STEP,
            _ => return,
        };
        let step = step * crate::ui_scale::factor(window);
        let width = self.clamp_width(self.width(window, cx) + step, window);
        self.set_width(Some(width), window, cx);
        cx.stop_propagation();
    }

    // ------------------------------------------------------------ 渲染

    /// 面板不可用的情况：没有终端、不支持、只用一个通道、连接中、已断开。
    fn unavailable(&self) -> Option<Unavailable> {
        let Some(target) = &self.target else {
            return Some(Unavailable::NoTerminal);
        };
        match target.state {
            ToolsState::Unsupported => Some(Unavailable::Unsupported),
            ToolsState::SingleChannel => Some(Unavailable::SingleChannel),
            ToolsState::Connecting => Some(Unavailable::Connecting),
            ToolsState::Ended => Some(Unavailable::Ended),
            ToolsState::Reconnecting | ToolsState::Connected(_) => None,
        }
    }

    /// 已断开时的“重新连接”：跟随的终端立即重新连接（面板随它的状态更新）。
    fn reconnect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(terminal) = self.target.as_ref().and_then(|t| t.terminal.upgrade()) {
            terminal.update(cx, |terminal, cx| terminal.ensure_connected(window, cx));
        }
    }

    /// 最近打开的、还在主机列表中的 SSH 主机（没有终端时列出，最近的在前）。
    fn recent_hosts(cx: &App) -> Vec<(HostId, SharedString)> {
        let store = AppState::store(cx);
        AppState::global(cx)
            .ui
            .read(cx)
            .state()
            .recent_hosts
            .iter()
            .filter_map(|id| {
                let id = HostId::from_raw(id.clone());
                let host = store.get::<Host>(&id)?;
                (host.doc.protocol == Protocol::Ssh).then(|| (id, host.doc.name.clone().into()))
            })
            .take(RECENT_SHOWN)
            .collect()
    }

    /// 面板不可用时的空状态：与中间区域没有终端时相同的样式（图标、标题、说明、按钮）。没有终端时给“快速连接…”与
    /// 最近的几台主机，已断开时给“重新连接”。整块接住面板的焦点（Esc 照常把焦点还给终端）。
    fn render_empty(&self, kind: Unavailable, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let name = self
            .target
            .as_ref()
            .map(|t| t.name.clone())
            .unwrap_or_default();
        let (icon, title, hint): (Option<IconName>, SharedString, SharedString) = match kind {
            Unavailable::NoTerminal => (
                Some(IconName::SquareTerminal),
                t!("tools.empty.no_terminal"),
                t!("tools.notice.no_terminal"),
            ),
            Unavailable::Unsupported => (
                Some(IconName::Info),
                t!("tools.empty.unsupported"),
                t!("tools.notice.not_supported_terminal"),
            ),
            Unavailable::SingleChannel => (
                Some(IconName::Cable),
                t!("tools.unavailable.single_channel"),
                t!("tools.unavailable_hint.single_channel"),
            ),
            Unavailable::Connecting => (
                None,
                t!("tools.empty.connecting", name = name),
                t!("tools.notice.connecting"),
            ),
            Unavailable::Ended => (
                Some(IconName::Unplug),
                t!("tools.empty.ended"),
                t!("tools.empty.ended_hint"),
            ),
        };
        let mut buttons: Vec<AnyElement> = Vec::new();
        match kind {
            Unavailable::NoTerminal => {
                buttons.push(
                    Button::new("host-tools-quick-connect")
                        .small()
                        .icon(IconName::Search)
                        .label(t!("tools.empty.quick_connect"))
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(HostToolsEvent::QuickConnect)))
                        .into_any_element(),
                );
                for (id, name) in Self::recent_hosts(cx) {
                    let open = id.clone();
                    buttons.push(
                        Button::new(SharedString::from(format!(
                            "host-tools-recent-{}",
                            id.as_str()
                        )))
                        .small()
                        .outline()
                        .icon(IconName::Server)
                        .label(name)
                        .tooltip(t!("tools.empty.open_recent"))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(HostToolsEvent::OpenHost(open.clone()))
                        }))
                        .into_any_element(),
                    );
                }
            }
            Unavailable::Ended => buttons.push(
                Button::new("host-tools-reconnect")
                    .small()
                    .primary()
                    .label(t!("terminal.reconnect"))
                    .on_click(cx.listener(|this, _, window, cx| this.reconnect(window, cx)))
                    .into_any_element(),
            ),
            _ => {}
        }
        let figure = match icon {
            Some(icon) => Icon::new(icon)
                .size_8()
                .text_color(muted)
                .into_any_element(),
            None => Spinner::new().into_any_element(),
        };
        v_flex()
            .id("host-tools-notice")
            .test_support()
            .key_context(LIST_CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .px_4()
            .child(
                v_flex()
                    .id(SharedString::from(format!(
                        "host-tools-empty-{}",
                        kind.key()
                    )))
                    .test_support()
                    .max_w_80()
                    .items_center()
                    .gap_2()
                    .child(figure)
                    .child(div().text_base().text_center().child(prose(title)))
                    .child(
                        div()
                            .text_sm()
                            .text_center()
                            .text_color(muted)
                            .child(prose(hint)),
                    )
                    .when(!buttons.is_empty(), |this| {
                        this.child(
                            h_flex()
                                .flex_wrap()
                                .justify_center()
                                .gap_2()
                                .pt_2()
                                .children(buttons),
                        )
                    }),
            )
            .into_any_element()
    }

    /// 左边缘的拖动条：骑在边框上，比线宽，不占布局。
    fn render_resizer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let drag = theme.drag_border;
        div()
            .id("host-tools-resizer")
            .test_support()
            .track_focus(&self.resize_focus)
            .absolute()
            .top_0()
            .bottom_0()
            .left(-rems(0.25))
            .w_2()
            .cursor_col_resize()
            .hover(|s| s.bg(drag.opacity(0.5)))
            .focus(|s| s.bg(drag))
            .tooltip(|window, cx| Tooltip::new(t!("tools.resize")).build(window, cx))
            .on_key_down(cx.listener(Self::on_resize_key))
            .on_drag(ResizeDrag, |_, _, _, cx| cx.new(|_| ResizeDrag))
            .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                this.resize_focus.focus(window, cx);
                if event.click_count() >= 2 {
                    this.set_width(None, window, cx);
                }
            }))
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let title = match &self.target {
            Some(target) => t!("tools.title", name = target.name.clone()),
            None => t!("tools.title_empty"),
        };
        h_flex()
            .flex_none()
            .h(rems(2.25))
            .pl_3()
            .pr_1()
            .gap_1()
            .child(
                div()
                    .id("host-tools-title")
                    .test_support()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .font_medium()
                    .child(title),
            )
            .child(
                Button::new("host-tools-close")
                    .ghost()
                    .small()
                    .icon(IconName::PanelRightClose)
                    .tooltip(t!("tools.close"))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(HostToolsEvent::Close))),
            )
    }

    /// 顶部各页的分段页签；面板不可用时置灰、不能点，也不标出当前页。
    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let disabled = self.unavailable().is_some();
        let bar = TabBar::new("host-tools-pages")
            .segmented()
            .small()
            .track_scroll(&self.tabs_scroll);
        let bar = if disabled {
            bar
        } else {
            bar.selected_index(self.page.index())
        };
        div().flex_none().px_2().pb_1p5().child(
            bar.on_click(cx.listener(|this, ix: &usize, _, cx| {
                if this.unavailable().is_none()
                    && let Some(page) = Page::ALL.get(*ix)
                {
                    this.set_page(*page, cx);
                }
            }))
            .child(
                Tab::new()
                    .label(t!("tools.page.overview"))
                    .disabled(disabled),
            )
            .child(
                Tab::new()
                    .label(t!("tools.page.processes"))
                    .disabled(disabled),
            )
            .child(
                Tab::new()
                    .label(t!("tools.page.services"))
                    .disabled(disabled),
            )
            .child(Self::docker_tab(cx).disabled(disabled))
            .child(
                Tab::new()
                    .label(t!("tools.page.network"))
                    .disabled(disabled),
            ),
        )
    }

    /// Docker 页的页签：名称前配 Docker 的标志（单色时跟着页签文字的颜色，选中与否都一样；彩色时是品牌色）。
    fn docker_tab(cx: &App) -> Tab {
        let label = t!("tools.page.docker");
        let Some(brand) = brand_icons::docker() else {
            return Tab::new().label(label);
        };
        let icon = match AppState::store(cx).settings().appearance.icons {
            IconStyle::Mono => gpui_kit::component::Icon::empty().path(brand.path()),
            style => {
                let theme = cx.theme();
                brand_icons::icon(
                    brand,
                    brand_icons::color(
                        brand,
                        style,
                        theme.muted_foreground,
                        theme.tab_bar_segmented,
                        theme.foreground,
                    ),
                )
            }
        };
        Tab::new()
            .aria_label(label.clone())
            .child(h_flex().gap_1().child(icon.small()).child(label))
    }

    fn render_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match (self.unavailable(), &self.pages) {
            (None, Some(pages)) => match self.page {
                Page::Overview => pages.overview.clone().into_any_element(),
                Page::Processes => pages.processes.clone().into_any_element(),
                Page::Services => pages.services.clone().into_any_element(),
                Page::Docker => pages.docker.clone().into_any_element(),
                Page::Network => pages.network.clone().into_any_element(),
            },
            (kind, _) => self.render_empty(kind.unwrap_or(Unavailable::NoTerminal), cx),
        };
        v_flex().flex_1().min_h_0().child(content)
    }
}

#[cfg(test)]
impl HostToolsPanel {
    pub(super) fn page(&self) -> Page {
        self.page
    }

    pub(super) fn cache(&self) -> Rc<RefCell<ToolCache>> {
        self.cache.clone()
    }

    pub(super) fn processes(&self) -> Option<Entity<ProcessPage>> {
        self.pages.as_ref().map(|p| p.processes.clone())
    }

    pub(super) fn overview(&self) -> Option<Entity<OverviewPage>> {
        self.pages.as_ref().map(|p| p.overview.clone())
    }

    pub(super) fn docker(&self) -> Option<Entity<DockerPage>> {
        self.pages.as_ref().map(|p| p.docker.clone())
    }

    pub(super) fn services(&self) -> Option<Entity<ServicePage>> {
        self.pages.as_ref().map(|p| p.services.clone())
    }

    pub(super) fn network(&self) -> Option<Entity<NetworkPage>> {
        self.pages.as_ref().map(|p| p.network.clone())
    }

    pub(super) fn has_pages(&self) -> bool {
        self.pages.is_some()
    }

    /// 跟随的终端（工作区的测试检查面板跟随哪个终端）。
    pub(crate) fn target_id(&self) -> Option<EntityId> {
        self.target.as_ref().map(|t| t.id)
    }
}

impl Render for HostToolsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 使用统计：面板显示着一台主机的内容（一次原子操作，记过的直接返回）
        if self.target.is_some() {
            termlet_core::usage::mark(termlet_core::usage::Feature::HostTools);
        }
        // 用户拖出的宽度，与侧边栏一样随界面大小缩放
        let width = px(self.width(window, cx));
        let reconnecting = self
            .target
            .as_ref()
            .is_some_and(|t| matches!(t.state, ToolsState::Reconnecting));
        let theme = cx.theme();
        let (border, background, foreground, warning) = (
            theme.sidebar_border,
            theme.sidebar,
            theme.sidebar_foreground,
            theme.warning,
        );
        v_flex()
            .id("host-tools")
            .test_support()
            .key_context(PANEL_CONTEXT)
            .on_action(
                cx.listener(|this, _: &NextPage, window, cx| this.switch_page(true, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &PrevPage, window, cx| this.switch_page(false, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &FocusSearch, window, cx| this.focus_search(window, cx)),
            )
            .on_action(cx.listener(|this, _: &FocusList, window, cx| this.focus_page(window, cx)))
            .on_action(
                cx.listener(|_, _: &FocusTerminal, _, cx| cx.emit(HostToolsEvent::FocusTerminal)),
            )
            .relative()
            .flex_none()
            .w(width)
            .h_full()
            .border_l_1()
            .border_color(border)
            .bg(background)
            .text_color(foreground)
            .on_drag_move(cx.listener(Self::on_resize_drag))
            .child(self.render_header(cx))
            .child(self.render_tabs(cx))
            .when(reconnecting, |this| {
                this.child(
                    div()
                        .px_3()
                        .pb_1()
                        .text_xs()
                        .text_color(warning)
                        .child(prose(t!("tools.reconnecting"))),
                )
            })
            .child(self.render_body(cx))
            // 排在页签之后：量这一帧页签排开的宽度
            .child(self.tabs_width.measure(width, self.tabs_overflow()))
            .child(self.render_resizer(cx))
    }
}
