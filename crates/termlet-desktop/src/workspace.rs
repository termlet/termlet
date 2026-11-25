//! 主窗口外壳：标题栏、侧边栏（主机、凭据档案）、Tab 区（Dock）与开始页。

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant};
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::dock::{
    DockArea, DockLayout, DockPlacement, DockSkin, InsertTarget, PaneNode, PaneRef, PanelId,
    PanelStyle, panel_handle,
};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Placement, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, ClickEvent, Context, DragMoveEvent, Entity, EntityId, FocusHandle,
    Focusable as _, InteractiveElement as _, IntoElement, KeyBinding, KeyDownEvent, MouseButton,
    ParentElement as _, Pixels, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, TestSupportExt as _, WeakEntity, Window, actions, div,
    prelude::FluentBuilder as _, px,
};
use termlet_core::id::{HostId, SnippetId, WorkspaceId};
use termlet_core::model::{
    Host, Kind, LayoutNode, SessionKind, Settings, Snippet, ThemeSetting,
    Workspace as SavedWorkspace,
};
use termlet_core::quick::QuickTarget;
use termlet_core::sftp::SftpEvent;
use termlet_core::ssh::Prompter;
use termlet_core::store::StoreEvent;

use crate::app_state::AppState;
use crate::compose::{ComposeBar, ComposeEvent, Mode as ComposeMode};
use crate::editor::{EditorPanel, EditorPanelEvent};
use crate::host_tools::{
    HostToolsEvent, HostToolsPage, HostToolsPanel, LogPanel, LogPanelEvent, LogTab, ToolsTarget,
};
use crate::local_shell::LocalTarget;
use crate::prompt_dialog::{self, PromptRequest};
use crate::prose::prose;
use crate::replay::ReplayPanel;
use crate::sftp::panel::{SessionLease, SftpPanel, SftpPanelEvent, SftpTarget, window_owner};
use crate::sidebar::Sidebar;
use crate::sidebar::host_tree::{HostStatus, HostTreeEvent};
use crate::snippets::{self, list::SnippetListEvent};
use crate::tab_fit::NeededWidth;
use crate::terminal_panel::{
    DuplicateSession, Session, SessionStatus, TerminalPanel, TerminalPanelEvent,
};
use crate::vault_gate::VaultGate;
use crate::{appearance, local_shell, settings, t, workspaces};

actions!(
    workspace,
    [
        /// 打开默认的本地终端
        NewLocalTerminal,
        /// 隐藏 / 显示侧边栏
        ToggleSidebar,
        /// 全屏 / 退出全屏
        ToggleFullscreen,
        /// 命令面板：搜索主机、已打开的 Tab 与各种操作
        OpenCommandPalette,
        /// 向右 / 向下分屏：新窗格共用当前终端的连接
        SplitRight,
        SplitDown,
        /// 在分屏的窗格之间切换
        FocusPaneLeft,
        FocusPaneRight,
        FocusPaneUp,
        FocusPaneDown,
        /// 把当前的分屏布局保存为工作区
        SaveAsWorkspace,
        /// 快捷命令选择器：搜索并发送快捷命令与命令历史
        OpenCommandPicker,
        /// 在当前终端右侧打开 SFTP：共用它的连接，跟随它的目录
        OpenSftpBeside,
        /// 开启 / 退出同步输入
        ToggleMultiExec,
        /// 打开 / 关闭撰写栏
        ToggleCompose,
        /// 打开撰写栏（命令面板：只打开不关闭）
        OpenCompose,
        /// 打开 / 收起主机工具面板
        ToggleHostTools,
        /// 打开主机工具的概览页，焦点进面板（键盘操作的入口）
        OpenHostToolsOverview,
        /// 把当前终端所连主机的概览弹出成置顶的小窗（命令面板）
        PopOutHostOverview,
        /// 打开新窗口
        NewWindow,
        /// 切换到同一组的下一个 / 上一个 Tab（循环）
        NextTab,
        PrevTab,
        /// 切换到同一组的第 1～9 个 Tab
        ActivateTab1,
        ActivateTab2,
        ActivateTab3,
        ActivateTab4,
        ActivateTab5,
        ActivateTab6,
        ActivateTab7,
        ActivateTab8,
        ActivateTab9,
        /// 打开设置（macOS 上 ⌘,）
        OpenSettings,
        /// 打开设置的“关于”页（macOS 菜单的“关于 Termlet”）
        OpenAbout,
        /// 在侧边栏中显示当前终端的主机（命令面板）
        RevealHostInSidebar,
        /// 把当前 Tab 移到新窗口（命令面板、可自定义的快捷键，默认没有）
        MoveTabToNewWindow,
        /// 焦点进竖排的 Tab 列表（Tab 放在左侧时；命令面板、可自定义的快捷键，默认没有）
        FocusTabList,
    ]
);

mod close_tabs;
#[cfg(test)]
mod editor_encoding_tests;
mod move_tab;
#[cfg(test)]
mod move_tab_tests;
mod multi_exec;
#[cfg(test)]
mod popout_tests;
mod replay_tabs;
mod startup;
mod tab_list;
#[cfg(test)]
mod tab_list_tests;
mod title_bar;
#[cfg(test)]
mod tools_follow_tests;

pub(crate) use close_tabs::close_tab_items;
pub(crate) use move_tab::SoleTabDrag;
pub(crate) use tab_list::{
    RAIL_ICON, TabFace, TabStatus, host_group_key, key_bindings as tab_list_key_bindings,
};

/// Tab 菜单中的“在侧栏中显示主机”（终端、文件管理的 Tab 连的是保存的主机时）。
pub(crate) fn reveal_host_item(host: HostId) -> gpui_kit::component::menu::PopupMenuItem {
    gpui_kit::component::menu::PopupMenuItem::new(t!("panel.menu.reveal_host")).on_click(
        move |_, window, cx| {
            if let Some(workspace) = Workspace::in_window(window, cx) {
                workspace.update(cx, |workspace, cx| {
                    workspace.show_host_in_sidebar(&host, window, cx)
                });
            }
        },
    )
}

pub(crate) const KEY_CONTEXT: &str = "Workspace";

/// 检查系统是否从睡眠中唤醒的间隔：两次检查之间墙上时间走得比这多出很多，说明中间睡眠过。
const RESUME_CHECK: Duration = Duration::from_secs(5);
const RESUME_GAP: Duration = Duration::from_secs(20);

/// 两条系统通知之间至少隔这么久。
const NOTIFY_INTERVAL: Duration = Duration::from_secs(2);

/// 侧边栏的宽度（用户拖出来的宽度，记在本机的界面状态中）。这几个宽度与记下的宽度都按 100% 的界面大小，用时乘上
/// 界面的缩放倍数（[`crate::ui_scale::factor`]）。
const SIDEBAR_DEFAULT: f32 = 256.;
/// 最窄的宽度：中文的四个页签（主机、快捷命令、密钥、转发）正好占满默认的宽度；英文等文案更长时按页签实际排开的
/// 宽度放宽（[`Workspace::sidebar_range`]）
const SIDEBAR_MIN: f32 = SIDEBAR_DEFAULT;
/// 拖动条获得焦点后按一次方向键调整的宽度。
const SIDEBAR_STEP: f32 = 16.;
const SIDEBAR_MAX: f32 = 560.;
/// 右侧的主区域至少留这么宽
const CONTENT_MIN: f32 = 400.;

/// 注册工作区固定的快捷键（可自定义的由 [`crate::keymap`] 按设置注册），按当前系统的习惯。
pub fn init(cx: &mut App) {
    let os = cx.platform_info().os;
    cx.bind_keys(key_bindings(os));
    cx.bind_keys(tab_list_key_bindings(os));
}

/// 工作区固定的快捷键：`os` 为按哪个系统的习惯。Windows、Linux 上 Alt+方向键在分屏的窗格之间切换；macOS 上照 iTerm2
/// 用 ⌥⌘+方向键（只带 Option 的组合留给输入），⌘, 打开设置。
pub(crate) fn key_bindings(os: gpui_kit::OperatingSystem) -> Vec<KeyBinding> {
    let context = Some(KEY_CONTEXT);
    if os == gpui_kit::OperatingSystem::Mac {
        return vec![
            KeyBinding::new("alt-cmd-left", FocusPaneLeft, context),
            KeyBinding::new("alt-cmd-right", FocusPaneRight, context),
            KeyBinding::new("alt-cmd-up", FocusPaneUp, context),
            KeyBinding::new("alt-cmd-down", FocusPaneDown, context),
            KeyBinding::new("cmd-,", OpenSettings, context),
        ];
    }
    vec![
        // 小键盘的加减号：Windows 上报告为 alt-shift-+ / alt-shift--（不经键盘映射），只有小键盘能按出来
        KeyBinding::new("alt-shift-+", SplitRight, context),
        KeyBinding::new("alt-shift--", SplitDown, context),
        KeyBinding::new("alt-left", FocusPaneLeft, context),
        KeyBinding::new("alt-right", FocusPaneRight, context),
        KeyBinding::new("alt-up", FocusPaneUp, context),
        KeyBinding::new("alt-down", FocusPaneDown, context),
    ]
}

/// 系统通知的点击回调平台上只有一个（再注册会整体替换）：在应用级注册一次，按 tag 找到发出它的窗口。
#[derive(Default)]
struct NotificationRoutes(Vec<(gpui_kit::AnyWindowHandle, WeakEntity<Workspace>)>);

impl gpui_kit::Global for NotificationRoutes {}

fn route_notifications(
    handle: gpui_kit::AnyWindowHandle,
    workspace: WeakEntity<Workspace>,
    cx: &mut App,
) {
    if !cx.has_global::<NotificationRoutes>() {
        cx.set_global(NotificationRoutes::default());
        cx.on_system_notification_response(|response, cx| {
            let routes: Vec<_> = cx
                .global::<NotificationRoutes>()
                .0
                .iter()
                .filter_map(|(h, w)| w.upgrade().map(|w| (*h, w)))
                .collect();
            // 发出它的终端所在的窗口；不是终端发的（SFTP 的汇总等）或终端已关闭时，最后打开的窗口
            let owner = routes
                .iter()
                .find(|(_, w)| w.read(cx).notified_terminal(&response.tag).is_some())
                .or(routes.last())
                .cloned();
            if let Some((handle, workspace)) = owner {
                let _ = handle.update(cx, |_, window, cx| {
                    workspace.update(cx, |this, cx| {
                        this.on_notification_clicked(&response.tag, window, cx)
                    })
                });
            }
        });
    }
    let routes = &mut cx.global_mut::<NotificationRoutes>().0;
    routes.retain(|(_, w)| w.upgrade().is_some());
    routes.push((handle, workspace));
}

/// 拖动侧边栏边缘调整宽度。
struct SidebarResize;

impl Render for SidebarResize {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// 主窗口的根视图。
pub struct Workspace {
    /// 没有别的元素有焦点时（刚启动、关掉了最后一个 Tab）焦点在这里，工作区的快捷键才能用
    focus: FocusHandle,
    /// 侧边栏的拖动条（获得焦点后可以用方向键调整宽度）
    sidebar_resize_focus: FocusHandle,
    dock_area: Entity<DockArea>,
    _dock_skin: Rc<DockSkin>,
    /// Tab 放在左侧时窗格顶上只画一条细标题（每帧按设置写入，见 `dock_skin`）
    title_only: crate::dock_skin::TitleOnly,
    /// 竖排的 Tab 列表（Tab 放在左侧时，见 `tab_list.rs`）
    tab_list: tab_list::TabListState,
    sidebar: Entity<Sidebar>,
    /// 侧边栏顶部的页签按当前语言的文案全部显示所需的侧边栏宽度
    sidebar_tabs_width: NeededWidth,
    /// 标题栏的工具栏放得下几个按钮（[`crate::toolbar::Fit`]）
    toolbar_fit: crate::toolbar::Fit,
    /// 按打开的先后
    terminals: Vec<WeakEntity<TerminalPanel>>,
    /// 每个终端的观察（状态变化时更新侧边栏的圆点）
    terminal_watch: HashMap<EntityId, [Subscription; 4]>,
    /// 按打开的先后
    sftps: Vec<WeakEntity<SftpPanel>>,
    sftp_watch: HashMap<EntityId, [Subscription; 2]>,
    /// 在线编辑器，按打开的先后
    editors: Vec<WeakEntity<EditorPanel>>,
    editor_watch: HashMap<EntityId, [Subscription; 2]>,
    /// 日志查看器的 Tab
    logs: Vec<WeakEntity<LogPanel>>,
    log_watch: HashMap<EntityId, [Subscription; 2]>,
    /// 会话录制的回放 Tab
    replays: Vec<WeakEntity<ReplayPanel>>,
    replay_watch: HashMap<EntityId, [Subscription; 2]>,
    /// 这个窗口在 SFTP 管理器中的标识（会话与传输按窗口区分）
    owner: String,
    sftp: termlet_core::sftp::SftpManager,
    forwards: termlet_core::forward::ForwardManager,
    /// 获得过焦点的终端，最近的在最后（焦点不在终端里时，快捷命令、主机工具等对着其中显示着的最近一个，
    /// 见 [`Self::command_target`]）
    recent_terminals: Vec<WeakEntity<TerminalPanel>>,
    /// 已确认关闭窗口（不再询问）
    close_confirmed: bool,
    /// 上一条系统通知的时间（节流）
    last_notification: Option<std::time::Instant>,
    /// 保险库没有解锁时代替内容显示
    vault_gate: Option<Entity<VaultGate>>,
    /// 同步输入（开启时终端平铺成网格）
    multi_exec: Option<multi_exec::MultiExec>,
    /// 撰写栏（主区底部）
    compose: Option<(Entity<ComposeBar>, Subscription)>,
    /// 主机工具面板（窗口右侧，跟随焦点所在的终端）
    host_tools: Entity<HostToolsPanel>,
    /// 标题栏的云同步状态与更新提示
    sync_indicator: Entity<crate::sync_ui::SyncIndicator>,
    /// 终端区域下方的快捷命令栏（开关在界面状态中）
    snippet_bar: Entity<crate::snippets::bar::SnippetBar>,
    update_badge: Entity<crate::lock_ui::UpdateBadge>,
    /// 应用锁锁定时代替整个窗口的内容
    lock_screen: Option<Entity<crate::lock_ui::LockScreen>>,
    /// 连接时的询问（主机指纹、密码、验证码）送到这个窗口
    prompter: Prompter,
    _prompts: gpui_kit::Task<()>,
    _resume_watch: gpui_kit::Task<()>,
    /// 移动、改大小后稍后记下窗口的位置
    _placement: Option<gpui_kit::Task<()>>,
    /// 拖着 Tab 到了窗口外：松开后移到别的窗口（见 `move_tab`）
    tab_drag_out: Option<move_tab::DragOut>,
    _sftp_events: gpui_kit::Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let app = AppState::global(cx);
        let store = app.store.clone();
        let ui = app.ui.clone();
        let (dock_area, dock_skin, title_only) =
            crate::dock_skin::dock_area("main-dock", Some(1), window, cx);
        // 只有一个 Tab 时也画成 Tab 栏（与多个 Tab 时相同，下面有分隔线），不换成没有边框的标题
        dock_skin.set_panel_style(PanelStyle::TabBar, cx);
        let host_tools = cx.new(|cx| HostToolsPanel::new(window, cx));
        let sync_indicator = cx.new(crate::sync_ui::SyncIndicator::new);
        let snippet_bar = cx.new(crate::snippets::bar::SnippetBar::new);
        let lock_model = crate::lock_ui::lock_model(cx).expect("应用锁先于窗口建立");
        let update_badge = cx.new(|cx| crate::lock_ui::UpdateBadge::new(false, cx));
        // 询问按到达的顺序逐个弹出，回答（或关闭）一个再弹下一个
        let (prompt_tx, prompt_rx) = async_channel::unbounded::<PromptRequest>();
        let prompts = cx.spawn_in(window, async move |_, cx| {
            while let Ok(request) = prompt_rx.recv().await {
                let Ok(done) = cx.update(|window, cx| prompt_dialog::open(request, window, cx))
                else {
                    break;
                };
                let _ = done.await;
            }
        });
        // 系统从睡眠中唤醒后，等待重连的终端立即重连、连接着的立即探测（计时器在睡眠中不走，墙上时间会跳过一段）
        let resume_watch = cx.spawn_in(window, async move |this, cx| {
            let mut last = SystemTime::now();
            loop {
                cx.background_executor().timer(RESUME_CHECK).await;
                let now = SystemTime::now();
                let gap = now.duration_since(last).unwrap_or_default();
                last = now;
                if gap < RESUME_GAP {
                    continue;
                }
                tracing::info!(?gap, "系统从睡眠中唤醒");
                if this
                    .update_in(cx, |this: &mut Self, window, cx| this.on_resume(window, cx))
                    .is_err()
                {
                    return;
                }
            }
        });
        let sidebar = cx.new(|cx| Sidebar::new(window, cx));
        let host_tree = sidebar.read(cx).host_tree().clone();
        let snippet_list = sidebar.read(cx).snippet_list().clone();
        let subscriptions = vec![
            // 焦点丢了（获得焦点的元素被移除，例如对话框中删掉了正在输入的一行）：收回到离它最近、
            // 还在的上层（对话框、面板），Esc 仍能关闭对话框，工作区的快捷键也不会在对话框下面生效；
            // 没有这样的上层时收回到工作区
            cx.on_focus_lost(window, |this, window, cx| {
                match window.focus_lost_restore_target(cx) {
                    Some(target) => target.focus(window, cx),
                    None => this.focus.focus(window, cx),
                }
            }),
            cx.subscribe_in(&host_tools, window, |this, _, event, window, cx| {
                this.on_host_tools_event(event, window, cx)
            }),
            cx.subscribe_in(
                &sync_indicator,
                window,
                |_, _, _: &crate::sync_ui::OpenSyncSettings, window, cx| {
                    settings::open(settings::Page::Sync, window, cx)
                },
            ),
            cx.observe_in(&lock_model, window, |this, _, window, cx| {
                this.sync_lock_screen(window, cx)
            }),
            cx.subscribe_in(
                &host_tree,
                window,
                |this, _, event, window, cx| match event {
                    HostTreeEvent::OpenHost(id) => this.open_host(id, window, cx),
                    HostTreeEvent::OpenQuick(target) => this.open_quick(target.clone(), window, cx),
                    HostTreeEvent::RevealHost(id) => this.reveal_host(id, window, cx),
                    HostTreeEvent::OpenWorkspace(id) => this.open_saved_workspace(id, window, cx),
                    HostTreeEvent::OpenLocal(key) => this.open_local_key(key, window, cx),
                    HostTreeEvent::OpenWslSftp { distro, user } => {
                        this.open_wsl_sftp(distro.clone(), user.clone(), window, cx)
                    }
                    HostTreeEvent::OpenHistory(id) => {
                        let target = this.command_target(window, cx).map(|p| p.downgrade());
                        snippets::history::open(id.clone(), target, window, cx);
                    }
                    HostTreeEvent::OpenSftp(id) => this.open_host_sftp(id, window, cx),
                    HostTreeEvent::OpenSftpRoot(id) => this.open_host_sftp_root(id, window, cx),
                },
            ),
            cx.subscribe_in(&store, window, |this, _, event, window, cx| {
                this.on_store_event(event, window, cx)
            }),
            cx.subscribe_in(
                &snippet_list,
                window,
                |this, _, event: &SnippetListEvent, window, cx| match event {
                    SnippetListEvent::Send { id, run } => this.send_snippet(id, *run, window, cx),
                },
            ),
            cx.subscribe_in(
                &snippet_bar,
                window,
                |this, _, event: &crate::snippets::bar::SnippetBarEvent, window, cx| {
                    use crate::snippets::bar::SnippetBarEvent;
                    match event {
                        SnippetBarEvent::Send(id) => this.send_snippet(id, None, window, cx),
                        SnippetBarEvent::OpenList => this.show_snippet_list(cx),
                    }
                },
            ),
            // 跟随系统时，系统切换浅色 / 深色后界面跟着变
            cx.observe_window_appearance(window, |this, window, cx| {
                let settings = AppState::store(cx).settings();
                if settings.appearance.theme == ThemeSetting::System {
                    appearance::apply(&settings.appearance, Some(window), cx);
                    this.restyle_terminals(cx);
                }
            }),
            // 移动、改大小、最大化后停下片刻记下窗口的位置（异常退出时不至于全丢；关闭时另外再记）
            cx.observe_window_bounds(window, |this, window, cx| {
                move_tab::note_window(window, cx);
                this._placement = Some(cx.spawn_in(window, async move |this, cx| {
                    cx.background_executor()
                        .timer(crate::window_state::SETTLE)
                        .await;
                    let _ = this.update_in(cx, |_, window, cx| {
                        crate::window_state::remember(window, cx)
                    });
                }));
            }),
            // 窗口获得焦点：今天用过 Termlet（使用统计按 UTC 日计）；界面大小跟随系统时重读系统的文字大小
            cx.observe_window_activation(window, |_, window, cx| {
                move_tab::note_window(window, cx);
                if window.is_window_active() {
                    crate::usage_ui::mark_active();
                    crate::ui_scale::apply(cx);
                }
            }),
            cx.observe(&dock_area, |_, _, cx| cx.notify()),
            cx.observe(&ui, |_, _, cx| cx.notify()),
            // 标题栏的“N 个转发”
            crate::forwards::observe(cx, |_, _, cx| cx.notify()),
        ];
        // 点击系统通知：回到发出它的终端
        route_notifications(window.window_handle(), cx.entity().downgrade(), cx);
        // Tab 可以移进来（“移到窗口 ›”、拖到这个窗口上）
        move_tab::register(window, cx.entity().downgrade(), cx);
        // 窗口关闭：别的窗口还在用的转发与 SFTP 会话先交出去，之后 `Drop` 再停止、关闭剩下的
        cx.on_release(|this, cx| this.hand_over_on_close(cx))
            .detach();
        // 关闭有连接的窗口前确认
        let this = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            this.update(cx, |this, cx| this.should_close(window, cx))
                .unwrap_or(true)
        });
        // 一批传输结束：弹出汇总提示
        let sftp_events = AppState::sftp(cx).events();
        let sftp_events = cx.spawn_in(window, async move |this, cx| {
            while let Ok(event) = sftp_events.recv().await {
                if this
                    .update_in(cx, |this: &mut Self, window, cx| {
                        this.on_sftp_event(event, window, cx)
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
        let prompter = prompt_dialog::prompter(prompt_tx);
        AppState::register_prompter(window_owner(window), prompter.clone(), cx);
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let mut this = Self {
            focus,
            sidebar_resize_focus: cx.focus_handle().tab_stop(true),
            dock_area,
            _dock_skin: dock_skin,
            title_only,
            tab_list: tab_list::TabListState::new(
                cx.focus_handle().tab_stop(true),
                cx.focus_handle().tab_stop(true),
            ),
            sidebar,
            sidebar_tabs_width: NeededWidth::default(),
            toolbar_fit: crate::toolbar::Fit::default(),
            terminals: Vec::new(),
            terminal_watch: HashMap::new(),
            sftps: Vec::new(),
            sftp_watch: HashMap::new(),
            editors: Vec::new(),
            editor_watch: HashMap::new(),
            logs: Vec::new(),
            log_watch: HashMap::new(),
            replays: Vec::new(),
            replay_watch: HashMap::new(),
            owner: window_owner(window),
            sftp: AppState::sftp(cx),
            forwards: AppState::global(cx).forwards.read(cx).manager().clone(),
            recent_terminals: Vec::new(),
            close_confirmed: false,
            last_notification: None,
            vault_gate: None,
            multi_exec: None,
            compose: None,
            host_tools,
            sync_indicator,
            snippet_bar,
            update_badge,
            lock_screen: None,
            prompter,
            _prompts: prompts,
            _resume_watch: resume_watch,
            _placement: None,
            tab_drag_out: None,
            _sftp_events: sftp_events,
            _subscriptions: subscriptions,
        };
        tracing::info!(owner = %this.owner, "打开主窗口");
        this.sync_vault_gate(window, cx);
        // 启动时锁定：窗口一打开就是锁屏；上次运行出错时提示（多个窗口只提示一次）
        this.sync_lock_screen(window, cx);
        crate::usage_ui::mark_active();
        cx.defer_in(window, |_, window, cx| {
            crate::lock_ui::check_crashes(window, cx);
            // 使用统计的第一次检查：锁着时不检查，解锁之后再做（见 sync_lock_screen）
            crate::usage_ui::check(cx);
        });
        this
    }

    fn on_store_event(&mut self, event: &StoreEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            StoreEvent::VaultChanged => {
                tracing::info!(status = ?AppState::store(cx).vault_status(), "保险库状态变化");
                self.sync_vault_gate(window, cx)
            }
            StoreEvent::SettingsChanged => {
                let settings = AppState::store(cx).settings();
                appearance::apply(&settings.appearance, Some(window), cx);
                self.restyle_terminals(cx);
                cx.notify();
            }
            // 工具栏的按钮（本机设置）
            StoreEvent::LocalSettingsChanged => cx.notify(),
            // 主机、分组、自定义配色变了：可能影响终端的配色
            StoreEvent::Changed(Kind::Host | Kind::Group | Kind::Scheme) => {
                self.restyle_terminals(cx)
            }
            _ => {}
        }
    }

    /// 终端请求发系统通知：设置开启、终端不在眼前（Tab 没有显示或窗口不在前台）时才发，两条之间至少隔 2 秒。
    /// 点击通知回到这个终端。
    fn notify(
        &mut self,
        panel: &Entity<TerminalPanel>,
        title: SharedString,
        body: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !AppState::store(cx).settings().terminal.notify_command_done {
            return;
        }
        if window.is_window_active() && panel.read(cx).visible_bounds().is_some() {
            return;
        }
        if self
            .last_notification
            .is_some_and(|at| at.elapsed() < NOTIFY_INTERVAL)
        {
            return;
        }
        self.last_notification = Some(std::time::Instant::now());
        tracing::debug!(session = %panel.read(cx).name(), "发出系统通知");
        cx.show_system_notification(gpui_kit::SystemNotification {
            tag: format!("terminal-{}", panel.entity_id().as_u64()).into(),
            title,
            body,
            actions: Vec::new(),
        });
    }

    /// 点击了通知：回到发出它的终端。
    fn on_notification_clicked(&mut self, tag: &str, window: &mut Window, cx: &mut Context<Self>) {
        tracing::debug!(tag, "点击了系统通知");
        crate::tray::show_window(window, cx);
        if let Some(panel) = self.notified_terminal(tag) {
            self.show_terminal(&panel, window, cx);
        }
    }

    /// 发出这条通知的终端（在这个窗口中时）。
    fn notified_terminal(&self, tag: &str) -> Option<Entity<TerminalPanel>> {
        self.live_terminals()
            .into_iter()
            .find(|p| format!("terminal-{}", p.entity_id().as_u64()) == tag)
    }

    /// 关闭窗口前：有连着的远程会话、运行中的本地终端且设置开启了确认时先问一下；有未完成的传输时总会问
    /// （确认后再关闭）。
    /// 关闭这个窗口（或退出）前要说明的：连接着的终端、文件管理按设置算，未完成的传输与未保存的修改总要确认。
    pub(crate) fn close_summary(&self, cx: &App) -> crate::close_confirm::CloseSummary {
        self.close_summary_of(|_| true, cx)
    }

    /// 只关闭其中一些 Tab（`include` 选中的）前要说明的，算法与 [`Self::close_summary`] 相同。
    fn close_summary_of(
        &self,
        include: impl Fn(PanelId) -> bool,
        cx: &App,
    ) -> crate::close_confirm::CloseSummary {
        let has = |id: EntityId| include(PanelId::from(id));
        let sftps: Vec<_> = self
            .live_sftps()
            .into_iter()
            .filter(|p| has(p.entity_id()))
            .collect();
        let pending: usize = sftps.iter().map(|p| p.read(cx).pending_transfers(cx)).sum();
        let open_terminals: Vec<_> = self
            .live_terminals()
            .into_iter()
            .filter(|p| has(p.entity_id()) && p.read(cx).needs_close_confirm())
            .collect();
        let local = open_terminals.iter().any(|p| p.read(cx).is_local_session());
        let connected = open_terminals
            .iter()
            .any(|p| !p.read(cx).is_local_session())
            || sftps.iter().any(|p| p.read(cx).needs_close_confirm());
        // 有未保存的修改时总会确认（不受设置影响）
        let unsaved = self
            .live_editors()
            .iter()
            .filter(|e| has(e.entity_id()) && e.read(cx).is_dirty(cx))
            .count();
        crate::close_confirm::CloseSummary {
            pending,
            unsaved,
            connected,
            local,
        }
    }

    /// 关闭窗口前（窗口的关闭按钮、macOS 菜单的“关闭窗口”）：要说明的先确认，确认后关闭；返回现在能不能直接关。
    /// 关闭的窗口记下位置（多个窗口时最后关闭的为准）。
    pub(crate) fn should_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.close_confirmed {
            return true;
        }
        // 关闭窗口时最小化到托盘：最后一个主窗口隐藏起来，会话照常运行，不用确认
        if crate::tray::close_to_tray(window, cx) {
            return false;
        }
        let summary = self.close_summary(cx);
        if !summary.needs_confirm() {
            crate::window_state::remember(window, cx);
            return true;
        }
        tracing::info!(?summary, "关闭窗口前确认");
        let this = cx.entity().downgrade();
        crate::close_confirm::confirm_window(
            summary.description(crate::close_confirm::CloseScope::Window),
            move |window, cx| {
                let _ = this.update(cx, |this, _| this.close_confirmed = true);
                crate::window_state::remember(window, cx);
                window.remove_window();
            },
            window,
            cx,
        );
        false
    }

    /// 锁定时从托盘菜单退出：在锁屏上显示退出的确认（锁屏挡在对话框之上，也不该显示会话的情况）。
    pub(crate) fn ask_quit_on_lock_screen(&self, cx: &mut App) {
        if let Some(screen) = &self.lock_screen {
            screen.update(cx, |screen, cx| screen.ask_quit(cx));
        }
    }

    /// 应用锁锁定、解锁：锁定时整个窗口换成锁屏，解锁后焦点回到锁定前的位置。
    fn sync_lock_screen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let locked = crate::lock_ui::is_locked(cx);
        match (locked, self.lock_screen.take()) {
            (true, Some(screen)) => self.lock_screen = Some(screen),
            (true, None) => {
                self.lock_screen = Some(cx.new(|cx| crate::lock_ui::LockScreen::new(window, cx)))
            }
            (false, Some(screen)) => {
                screen.update(cx, |s, cx| s.restore_focus(window, cx));
                // 锁定期间排队的外部调起请求；使用统计的检查
                cx.defer_in(window, |this, window, cx| {
                    this.take_launches(window, cx);
                    crate::usage_ui::check(cx);
                });
            }
            (false, None) => {}
        }
        cx.notify();
    }

    fn sync_vault_gate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let unlocked = AppState::store(cx).vault().is_unlocked();
        if unlocked {
            // 勾了“自动启动”的端口转发：第一个窗口打开、保险库解锁后启动
            crate::forwards::auto_start(window, cx);
            // 外部程序调起的请求：窗口建好后再处理（可能要弹确认框）
            cx.defer_in(window, |this, window, cx| this.take_launches(window, cx));
        }
        match (unlocked, self.vault_gate.is_some()) {
            (false, false) => self.vault_gate = Some(cx.new(|cx| VaultGate::new(window, cx))),
            (true, true) => self.vault_gate = None,
            _ => {}
        }
        cx.notify();
    }

    /// 设置、外观或配色变了，重新给每个终端应用样式。
    fn restyle_terminals(&mut self, cx: &mut Context<Self>) {
        for panel in self.live_terminals() {
            panel.update(cx, |panel, cx| panel.restyle(cx));
        }
        for panel in self.live_replays() {
            panel.update(cx, |panel, cx| panel.restyle(cx));
        }
    }

    /// 修改并保存设置，成功后立即应用外观（不等变更事件，点击后马上看到效果）；保存失败时提示。
    fn save_settings(
        &mut self,
        edit: impl FnOnce(&mut Settings),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = AppState::store(cx);
        match store.update_settings(edit) {
            Ok(()) => {
                tracing::debug!("已保存设置（标题栏、命令面板中的切换）");
                appearance::apply(&store.settings().appearance, Some(window), cx);
                self.restyle_terminals(cx);
            }
            Err(e) => {
                tracing::error!("保存设置失败：{e}");
                crate::notify::error(
                    t!("settings.save_failed", error = e.to_string()),
                    window,
                    cx,
                );
            }
        }
    }

    pub(crate) fn toggle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = if cx.theme().is_dark() {
            ThemeSetting::Light
        } else {
            ThemeSetting::Dark
        };
        tracing::info!(theme = ?next, "切换浅色 / 深色");
        self.save_settings(|s| s.appearance.theme = next, window, cx);
    }

    fn on_resume(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for panel in self.live_terminals() {
            panel.update(cx, |panel, cx| panel.on_resume(window, cx));
        }
    }

    pub(crate) fn live_terminals(&self) -> Vec<Entity<TerminalPanel>> {
        self.terminals
            .iter()
            .filter_map(|panel| panel.upgrade())
            .collect()
    }

    /// 终端、SFTP 会话与未完成的传输的数量（诊断信息）。
    pub(crate) fn session_counts(&self, cx: &App) -> (usize, usize, usize) {
        let sftps = self.live_sftps();
        let pending = sftps.iter().map(|p| p.read(cx).pending_transfers(cx)).sum();
        (self.live_terminals().len(), sftps.len(), pending)
    }

    /// 未保存的编辑器与未完成的传输（退出前确认）。
    pub(crate) fn critical_work(&self, cx: &App) -> (usize, usize) {
        let unsaved = self
            .live_editors()
            .iter()
            .filter(|e| e.read(cx).is_dirty(cx))
            .count();
        let pending = self
            .live_sftps()
            .iter()
            .map(|p| p.read(cx).pending_transfers(cx))
            .sum();
        (unsaved, pending)
    }

    /// 这台保存的主机连接着的 SSH 终端，按打开的先后（`termlet exec --terminal` 用最后一个）。
    pub(crate) fn terminals_of(&self, host: &HostId, cx: &App) -> Vec<Entity<TerminalPanel>> {
        self.live_terminals()
            .into_iter()
            .filter(|p| {
                let p = p.read(cx);
                p.ssh_host() == Some(host) && p.is_live(cx)
            })
            .collect()
    }

    /// 打开着的终端 Tab 数。
    pub fn terminal_count(&self) -> usize {
        self.live_terminals().len()
    }

    /// 打开着的终端 Tab 的实体 ID（按打开的顺序）：终端中的元素 ID 带着它（如状态条的 `("monitor-bar", ID)`），
    /// 集成测试按它找。
    pub fn terminal_ids(&self) -> Vec<gpui_kit::EntityId> {
        self.live_terminals()
            .iter()
            .map(|p| p.entity_id())
            .collect()
    }

    /// 切到这个终端 Tab 并让它获得焦点。
    pub(crate) fn show_terminal(
        &mut self,
        panel: &Entity<TerminalPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel_id = PanelId::from(panel.entity_id());
        self.dock_area
            .update(cx, |dock, cx| dock.select_panel(panel_id, window, cx));
        self.set_sync_away(false, cx);
        panel.read(cx).focus_handle(cx).focus(window, cx);
        // 已经显示着（分屏中的另一格）时不会收到 Tab 激活的通知：这里就跟过去
        self.sync_tools_target(window, cx);
    }

    /// 在当前终端旁边分屏：新窗格共用它的连接（复制会话），放在 `placement` 一侧。
    pub(crate) fn split(
        &mut self,
        placement: Placement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.focused_terminal(window, cx) else {
            return;
        };
        if !self.check_duplicate(&panel, window, cx) {
            return;
        }
        let current = PanelId::from(panel.entity_id());
        let Some(node) = self
            .dock_area
            .read(cx)
            .layout(DockPlacement::Center)
            .and_then(|tree| tree.find_panel_node(current))
        else {
            return;
        };
        tracing::info!(session = %panel.read(cx).name(), ?placement, "分屏");
        termlet_core::usage::mark(termlet_core::usage::Feature::Split);
        let copy = panel.update(cx, |panel, cx| panel.duplicate(window, cx));
        self.add_terminal(copy.clone(), window, cx);
        let target = InsertTarget::Split {
            node,
            placement,
            size: None,
        };
        self.dock_area.update(cx, |dock, cx| {
            dock.move_panel(PanelId::from(copy.entity_id()), target, window, cx)
        });
        copy.read(cx).focus_handle(cx).focus(window, cx);
    }

    // ------------------------------------------------------------ 切换 Tab

    /// 焦点所在的 Tab 分组（焦点不在 Tab 中时是最近用过的终端所在的，再没有时是第一组）：
    /// 组中的面板，以及显示中的是第几个。
    fn current_tab_group(&self, window: &Window, cx: &App) -> Option<(Vec<PanelId>, usize)> {
        let dock = self.dock_area.read(cx);
        let tree = dock.layout(DockPlacement::Center)?;
        let mut groups = Vec::new();
        collect_tab_groups(tree.root(), &mut groups);
        let focused = groups.iter().position(|(panels, _)| {
            panels.iter().any(|id| {
                dock.panel(*id)
                    .is_some_and(|view| view.focus_handle(cx).contains_focused(window, cx))
            })
        });
        let recent = || {
            let id = PanelId::from(self.command_target(window, cx)?.entity_id());
            groups.iter().position(|(panels, _)| panels.contains(&id))
        };
        let ix = focused
            .or_else(recent)
            .or((!groups.is_empty()).then_some(0))?;
        Some(groups.swap_remove(ix))
    }

    /// 切到同一组中相对显示中的那个 `offset` 个位置的 Tab（到头后循环）。同步输入时终端平铺成网格，不切换。
    fn cycle_tab(&mut self, offset: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_syncing() {
            return;
        }
        let Some((panels, active)) = self.current_tab_group(window, cx) else {
            return;
        };
        if panels.len() < 2 {
            return;
        }
        let len = panels.len() as isize;
        let next = (active as isize + offset).rem_euclid(len) as usize;
        self.select_tab_panel(panels[next], window, cx);
    }

    /// 切到同一组的第 `index` 个 Tab（从 0 开始）；没有那么多个时不动。
    fn activate_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_syncing() {
            return;
        }
        if let Some(id) = self
            .current_tab_group(window, cx)
            .and_then(|(panels, _)| panels.get(index).copied())
        {
            self.select_tab_panel(id, window, cx);
        }
    }

    /// 显示这个 Tab 并让它获得焦点。
    fn select_tab_panel(&mut self, panel: PanelId, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.dock_area.read(cx).panel(panel).cloned();
        self.dock_area
            .update(cx, |dock, cx| dock.select_panel(panel, window, cx));
        if let Some(view) = view {
            view.focus_handle(cx).focus(window, cx);
        }
        self.sync_tools_target(window, cx);
        cx.notify();
    }

    /// 切到 `direction` 一侧最近的窗格（按窗格中心的位置）。
    fn focus_pane(&mut self, direction: Placement, window: &mut Window, cx: &mut Context<Self>) {
        let Some(current) = self.focused_terminal(window, cx) else {
            return;
        };
        let Some(from) = current.read(cx).visible_bounds() else {
            return;
        };
        let center =
            |b: gpui_kit::Bounds<Pixels>| (f32::from(b.center().x), f32::from(b.center().y));
        let (cx0, cy0) = center(from);
        let target = self
            .live_terminals()
            .into_iter()
            .filter(|p| p.entity_id() != current.entity_id())
            .filter_map(|p| p.read(cx).visible_bounds().map(|b| (p.clone(), center(b))))
            .filter(|(_, (x, y))| match direction {
                Placement::Left => *x < cx0 - 1.,
                Placement::Right => *x > cx0 + 1.,
                Placement::Top => *y < cy0 - 1.,
                Placement::Bottom => *y > cy0 + 1.,
            })
            .min_by(|(_, a), (_, b)| {
                let distance = |(x, y): &(f32, f32)| match direction {
                    // 主方向上的距离为主，偏离的距离加权计入
                    Placement::Left | Placement::Right => (x - cx0).abs() + 2. * (y - cy0).abs(),
                    Placement::Top | Placement::Bottom => (y - cy0).abs() + 2. * (x - cx0).abs(),
                };
                distance(a).total_cmp(&distance(b))
            })
            .map(|(p, _)| p);
        if let Some(panel) = target {
            panel.read(cx).focus_handle(cx).focus(window, cx);
            self.sync_tools_target(window, cx);
        }
    }

    fn open_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.vault_gate.is_some() {
            return;
        }
        use crate::command_palette::TabRef;
        let current = self.focused_terminal(window, cx).map(|p| p.downgrade());
        let tabs = self
            .terminals
            .iter()
            .cloned()
            .map(TabRef::Terminal)
            .chain(self.sftps.iter().cloned().map(TabRef::Sftp))
            .chain(self.editors.iter().cloned().map(TabRef::Editor))
            .chain(self.logs.iter().cloned().map(TabRef::Logs))
            .chain(self.replays.iter().cloned().map(TabRef::Replay))
            .collect();
        let syncing = self.is_syncing();
        crate::command_palette::open(cx.entity().downgrade(), current, tabs, syncing, window, cx);
    }

    /// 快捷命令、命令历史、主机工具等对着的终端：焦点所在的；焦点不在终端里（在侧边栏、主机工具、对话框中）时是
    /// 显示着的终端中最近获得过焦点的（分屏时是最近获得焦点的那一格），都没有获得过焦点时是显示着的终端中最后打开的；
    /// 中间区域没有显示终端（SFTP、编辑器的 Tab 在前台）时是最近获得过焦点的。
    fn command_target(&self, window: &Window, cx: &App) -> Option<Entity<TerminalPanel>> {
        if let Some(panel) = self.focused_terminal(window, cx) {
            return Some(panel);
        }
        let recent: Vec<Entity<TerminalPanel>> = self
            .recent_terminals
            .iter()
            .rev()
            .filter_map(|p| p.upgrade())
            .collect();
        let live = self.live_terminals();
        let shown = |p: &&Entity<TerminalPanel>| p.read(cx).is_shown();
        recent
            .iter()
            .find(shown)
            .or_else(|| live.iter().rev().find(shown))
            .or_else(|| recent.first())
            .or_else(|| live.last())
            .cloned()
    }

    /// 记下获得焦点的终端（排到最近），顺带去掉已关闭的。
    fn note_recent_terminal(&mut self, panel: &Entity<TerminalPanel>) {
        if self
            .recent_terminals
            .last()
            .is_some_and(|p| p.entity_id() == panel.entity_id())
        {
            return;
        }
        let id = panel.entity_id();
        self.recent_terminals
            .retain(|p| p.entity_id() != id && p.upgrade().is_some());
        self.recent_terminals.push(panel.downgrade());
    }

    fn open_command_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.vault_gate.is_some() {
            return;
        }
        let target = self.command_target(window, cx).map(|p| p.downgrade());
        snippets::picker::open(target, None, window, cx);
    }

    /// 发送侧边栏中点击的快捷命令；带参数的先打开选择器填写参数。
    fn send_snippet(
        &mut self,
        id: &SnippetId,
        run: Option<bool>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(snippet) = AppState::store(cx).get::<Snippet>(id) else {
            return;
        };
        let target = self.command_target(window, cx);
        if !termlet_core::snippet_params::snippet_params(&snippet.doc.content).is_empty() {
            snippets::picker::open(
                target.map(|p| p.downgrade()),
                Some((id.clone(), run)),
                window,
                cx,
            );
            return;
        }
        let Some(panel) = target else {
            crate::notify::warning(t!("snippets.no_terminal"), window, cx);
            return;
        };
        let input = snippets::command_input(&snippet.doc.content, run);
        match self.deliver_command(&panel, input, window, cx) {
            Ok(()) => {
                termlet_core::usage::mark(termlet_core::usage::Feature::QuickCommands);
                self.show_terminal(&panel, window, cx)
            }
            Err(message) => crate::notify::warning(message, window, cx),
        }
    }

    /// 快捷命令、命令历史写进终端：
    /// - 同步输入开着、这个终端在同步中时，与键入、粘贴一样交给同步输入，发给同步中的全部终端（回车前、粘贴时查危险命令，
    ///   与同步输入相同）；
    /// - 否则写进这个终端；要执行的（各行以回车结束）先查危险命令规则，命中时先确认（取消则不写）。
    ///
    /// 写不进去（多行只写入而终端不支持括号粘贴等）时返回说明；要确认时返回 `Ok`，确认后才写。
    pub(crate) fn deliver_command(
        &mut self,
        panel: &Entity<TerminalPanel>,
        input: snippets::CommandInput,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), SharedString> {
        if self.is_sync_member(panel.entity_id()) && self.synced_terminals().unwrap_or(0) > 1 {
            match input {
                snippets::CommandInput::Type(data) => {
                    self.on_sync_typed(panel, data.into_bytes(), window, cx)
                }
                snippets::CommandInput::Paste(text) => self.on_sync_paste(panel, text, window, cx),
            }
            return Ok(());
        }
        // 只写入、不执行的（不以回车结束）不查
        let danger = match &input {
            snippets::CommandInput::Type(data) if data.ends_with('\r') => {
                let guard = termlet_core::guard::Guard::from_settings(
                    &AppState::store(cx).settings().guard,
                )
                .0;
                termlet_core::guard::find_danger(&guard, &data.replace('\r', "\n"))
            }
            _ => None,
        };
        let Some(danger) = danger else {
            return panel.update(cx, |p, cx| p.write_command(input, cx));
        };
        tracing::info!(rule = %danger.rule.pattern(), "快捷命令命中危险命令规则，先确认");
        let rule = crate::i18n::rule_description(&danger.rule);
        let rule = if rule.is_empty() {
            danger.rule.pattern().to_string()
        } else {
            rule.to_string()
        };
        let target = panel.downgrade();
        let input = Rc::new(std::cell::RefCell::new(Some(input)));
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let (target, input) = (target.clone(), input.clone());
            alert
                .confirm()
                .title(t!("snippets.danger_title"))
                .description(prose(t!(
                    "compose.danger_hint",
                    command = danger.line.clone(),
                    rule = rule.clone()
                )))
                .ok_text(t!("multi_exec.run_anyway"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, window, cx| {
                    let (Some(panel), Some(input)) = (target.upgrade(), input.borrow_mut().take())
                    else {
                        return true;
                    };
                    if let Err(message) = panel.update(cx, |p, cx| p.write_command(input, cx)) {
                        crate::notify::warning(message, window, cx);
                    }
                    panel.read(cx).focus_handle(cx).focus(window, cx);
                    true
                })
        });
        Ok(())
    }

    /// 打开侧边栏的“快捷命令”页（侧边栏隐藏着时先显示出来）。
    fn show_snippet_list(&mut self, cx: &mut Context<Self>) {
        if self.sidebar_hidden(cx) {
            AppState::ui(cx).update(cx, |ui, cx| ui.update(|s| s.sidebar_hidden = false, cx));
        }
        self.sidebar.update(cx, |sidebar, cx| {
            sidebar.show_page(crate::sidebar::Page::Snippets, cx)
        });
    }

    /// 焦点所在的终端 Tab（显示着的）：焦点在终端视图上，或在面板中的其他位置（对话框关闭时还回的面板自身、
    /// 搜索栏等）。没有显示的 Tab 不算：焦点停在它的终端视图上时 `contains_focused` 也为真（同一个句柄）。
    fn focused_terminal(&self, window: &Window, cx: &App) -> Option<Entity<TerminalPanel>> {
        self.live_terminals().into_iter().find(|panel| {
            let panel = panel.read(cx);
            panel.is_shown()
                && (panel.root_focus().contains_focused(window, cx)
                    || panel.focus_handle(cx).contains_focused(window, cx))
        })
    }

    /// 复制会话：当前终端的主机新开一个 Tab，共用它的连接。
    fn duplicate_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.focused_terminal(window, cx) else {
            return;
        };
        if !self.check_duplicate(&panel, window, cx) {
            return;
        }
        tracing::info!(session = %panel.read(cx).name(), "复制会话");
        let copy = panel.update(cx, |panel, cx| panel.duplicate(window, cx));
        self.add_terminal(copy, window, cx);
    }

    /// 外部程序调起的临时连接只用一个通道，不能复制会话、分屏：提示并返回 false。
    fn check_duplicate(
        &self,
        panel: &Entity<TerminalPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let allowed = panel.read(cx).can_duplicate();
        if !allowed {
            crate::notify::warning(t!("launch.no_duplicate"), window, cx);
        }
        allowed
    }

    /// 打开默认的本地终端（可以是 WSL 发行版）。
    pub fn open_local_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = AppState::store(cx).local_settings();
        match local_shell::default_target(&settings, cx) {
            Some(LocalTarget::Shell(profile)) => {
                let name = local_shell::display_name(&profile);
                self.open_terminal(name, Session::Local { profile }, window, cx);
            }
            Some(LocalTarget::Wsl(distro)) => self.open_wsl(distro, String::new(), window, cx),
            None => crate::notify::error(t!("local_shell.none"), window, cx),
        }
    }

    /// 打开侧边栏"本机"中的一项（`local:<shell ID>` 或 `wsl:<发行版>`）。
    fn open_local_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(distro) = key.strip_prefix("wsl:") {
            self.open_wsl(distro.to_string(), String::new(), window, cx);
        } else {
            self.open_local_shell(key.strip_prefix("local:").unwrap_or(key), window, cx);
        }
    }

    /// 打开 WSL 发行版的终端（`user` 为 `root` 时以 root 登录）。
    pub fn open_wsl(
        &mut self,
        distro: String,
        user: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = crate::wsl_name(&distro, &user);
        self.open_terminal(name, Session::Wsl { distro, user }, window, cx);
    }

    /// 打开 WSL 发行版的文件管理（新 Tab，不跟随终端）。
    pub fn open_wsl_sftp(
        &mut self,
        distro: String,
        user: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = crate::wsl_name(&distro, &user);
        self.open_sftp(name, SftpTarget::Wsl { distro, user }, None, window, cx);
    }

    /// 打开指定的本地终端（侧边栏“本机”中的项）。
    pub fn open_local_shell(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let settings = AppState::store(cx).local_settings();
        let Some(profile) = local_shell::find_profile(id, &settings, cx) else {
            crate::notify::error(t!("local_shell.error.unknown"), window, cx);
            return;
        };
        let name = local_shell::display_name(&profile);
        self.open_terminal(name, Session::Local { profile }, window, cx);
    }

    /// 新开一个 Tab 连接保存的主机（记为最近打开的主机：主机工具面板没有终端时列出）。
    pub fn open_host(&mut self, id: &HostId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = AppState::store(cx).get::<Host>(id) else {
            return;
        };
        AppState::ui(cx).update(cx, |ui, cx| {
            ui.update(|s| s.note_recent_host(id.as_str()), cx)
        });
        self.open_terminal(
            host.doc.name.into(),
            Session::Host { host: id.clone() },
            window,
            cx,
        );
    }

    /// 处理排队的外部程序调起请求（保险库锁着时留在队列里）。启动后第一次处理时先按设置打开启动时打开的
    /// （[`crate::startup_open`]）。
    pub(crate) fn take_launches(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.vault_gate.is_some() || crate::lock_ui::is_locked(cx) {
            return;
        }
        self.open_startup_items(window, cx);
        let workspace = cx.entity().downgrade();
        for target in crate::launch::take(cx) {
            crate::launch::handle(target, workspace.clone(), window, cx);
        }
    }

    /// 打开外部程序调起的临时连接：WinSCP、Xftp 等的参数打开文件管理，其余打开终端。
    pub(crate) fn open_launch(
        &mut self,
        target: termlet_core::launch::LaunchTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = crate::launch::title(&target);
        let target = Arc::new(target);
        match target.kind {
            termlet_core::launch::LaunchKind::Sftp => {
                self.open_sftp(name, SftpTarget::Launch(target), None, window, cx);
            }
            termlet_core::launch::LaunchKind::Terminal => {
                self.open_terminal(name, Session::Launch { target }, window, cx)
            }
        }
        crate::tray::show_window(window, cx);
    }

    /// 快速连接：临时连接，不保存到主机列表。
    pub fn open_quick(&mut self, target: QuickTarget, window: &mut Window, cx: &mut Context<Self>) {
        let name = target.display();
        self.open_terminal(name.into(), Session::Quick { target }, window, cx);
    }

    /// 切到这台主机最近打开的、还连着的终端；都已断开时切到最近的一个并重新连接。
    fn reveal_host(&mut self, id: &HostId, window: &mut Window, cx: &mut Context<Self>) {
        let panels: Vec<_> = self
            .live_terminals()
            .into_iter()
            .rev()
            .filter(|p| p.read(cx).host() == Some(id))
            .collect();
        let target = panels
            .iter()
            .find(|p| p.read(cx).status() == SessionStatus::Connected)
            .or(panels.first())
            .cloned();
        let Some(panel) = target else {
            return;
        };
        self.show_terminal(&panel, window, cx);
        panel.update(cx, |panel, cx| panel.ensure_connected(window, cx));
    }

    /// 打开一个终端 Tab。
    pub fn open_terminal(
        &mut self,
        name: SharedString,
        session: Session,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.vault_gate.is_some() {
            return;
        }
        let panel = TerminalPanel::open(name, session, self.prompter.clone(), window, cx);
        self.add_terminal(panel, window, cx);
    }

    // ------------------------------------------------------------ SFTP

    fn live_sftps(&self) -> Vec<Entity<SftpPanel>> {
        self.sftps.iter().filter_map(|p| p.upgrade()).collect()
    }

    // ------------------------------------------------------------ 主机工具

    /// 打开或收起主机工具面板（Ctrl+Shift+E）；收起后焦点回到当前终端。
    fn toggle_host_tools(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let open = !AppState::global(cx).ui.read(cx).state().host_tools_open;
        tracing::debug!(open, "主机工具面板");
        let ui = AppState::global(cx).ui.clone();
        ui.update(cx, |ui, cx| ui.update(|s| s.host_tools_open = open, cx));
        if open {
            self.sync_tools_target(window, cx);
        } else if let Some(panel) = self.command_target(window, cx) {
            panel.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    /// “打开主机工具的概览”（Ctrl+Shift+O、命令面板）：展开面板、切到概览页，焦点进面板（之后 Ctrl+PageUp / PageDown
    /// 切换各页，Esc 回到终端）。面板跟随的终端不变。
    pub(crate) fn focus_host_tools_overview(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::debug!("打开主机工具的概览");
        let ui = AppState::global(cx).ui.clone();
        ui.update(cx, |ui, cx| ui.update(|s| s.host_tools_open = true, cx));
        self.sync_tools_target(window, cx);
        self.host_tools.update(cx, |tools, cx| {
            tools.show_page_focused(HostToolsPage::Overview, window, cx)
        });
        cx.notify();
    }

    /// “主机工具：在置顶小窗中显示概览”（命令面板）：把主机工具跟随的终端（[`Self::command_target`]）所连主机的
    /// 概览弹出成小窗，已经开着时拿到前面。面板收着也可以。
    fn pop_out_host_overview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_tools_target(window, cx);
        self.host_tools
            .update(cx, |tools, cx| tools.pop_out_overview(window, cx));
    }

    /// 点了终端底部的状态条：让这个终端获得焦点（主机工具随之跟随它），打开主机工具并切到概览页。
    fn open_overview(
        &mut self,
        panel: &Entity<TerminalPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::debug!(session = %panel.read(cx).name(), "从状态条打开概览");
        panel.read(cx).focus_handle(cx).focus(window, cx);
        let ui = AppState::global(cx).ui.clone();
        ui.update(cx, |ui, cx| ui.update(|s| s.host_tools_open = true, cx));
        self.sync_tools_target(window, cx);
        self.host_tools
            .update(cx, |tools, cx| tools.show_page(HostToolsPage::Overview, cx));
        cx.notify();
    }

    /// 主机工具跟随的终端：[`Self::command_target`]（焦点移进主机工具、侧边栏、对话框时不变）。
    ///
    /// 顺带按现在的焦点记下最近获得焦点的终端：窗口不在前台时 GPUI 不发焦点事件（`on_focus_in` 收不到，
    /// 例如投递的输入、连接中切到别的程序），只靠它会记不到。
    fn sync_tools_target(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = self.focused_terminal(window, cx) {
            self.note_recent_terminal(&panel);
        }
        let target = self.command_target(window, cx).map(|panel| {
            let p = panel.read(cx);
            ToolsTarget {
                terminal: panel.downgrade(),
                id: panel.entity_id(),
                name: p.host_label(cx),
                login: p.login_label(),
                state: p.tools_state(),
                tools: p.host_tools(),
                ssh_host: p.ssh_host().cloned(),
            }
        });
        self.host_tools
            .update(cx, |tools, cx| tools.set_target(target, window, cx));
    }

    pub(crate) fn on_host_tools_event(
        &mut self,
        event: &HostToolsEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            HostToolsEvent::RunInTerminal {
                terminal,
                command,
                title,
                refresh,
            } => self.run_beside(terminal, command, title, *refresh, window, cx),
            HostToolsEvent::OpenLogs(tab) => self.open_logs(tab.clone(), window, cx),
            HostToolsEvent::OpenSftp { terminal, path } => {
                let Some(terminal) = terminal.upgrade() else {
                    crate::notify::warning(t!("tools.source_gone"), window, cx);
                    return;
                };
                if let Some(panel) = self.open_sftp_for_terminal(terminal, window, cx) {
                    panel.update(cx, |panel, cx| panel.start_at(path.clone(), window, cx));
                }
            }
            HostToolsEvent::ForwardLocal {
                host,
                terminal,
                port,
                target_host,
                name,
            } => {
                let preset = termlet_core::model::Forward {
                    name: name.clone(),
                    bind_host: "127.0.0.1".into(),
                    bind_port: *port,
                    target_host: target_host.clone(),
                    target_port: *port,
                    ..Default::default()
                };
                let sidebar = self.sidebar.downgrade();
                let on_saved: crate::forward_dialog::OnSaved = Rc::new(move |_, cx| {
                    let _ = sidebar.update(cx, |sidebar, cx| {
                        sidebar.show_page(crate::sidebar::Page::Forwards, cx)
                    });
                });
                crate::forward_dialog::open_preset(
                    host.clone(),
                    Some(terminal.clone()),
                    preset,
                    Some(on_saved),
                    window,
                    cx,
                );
            }
            HostToolsEvent::OpenInBrowser {
                host,
                terminal,
                target_host,
                port,
                https,
                name,
            } => {
                let Some(terminal) = terminal.upgrade() else {
                    crate::notify::warning(t!("tools.source_gone"), window, cx);
                    return;
                };
                let request = crate::forwards::BrowserRequest {
                    host: host.clone(),
                    target_host: target_host.clone(),
                    port: *port,
                    https: *https,
                    name: name.clone(),
                };
                let (share_group, prompter) = {
                    let panel = terminal.read(cx);
                    (panel.share_group(), panel.prompter())
                };
                crate::forwards::open_in_browser(request, share_group, prompter, window, cx);
            }
            HostToolsEvent::Close => {
                let ui = AppState::global(cx).ui.clone();
                ui.update(cx, |ui, cx| ui.update(|s| s.host_tools_open = false, cx));
                if let Some(panel) = self.command_target(window, cx) {
                    panel.read(cx).focus_handle(cx).focus(window, cx);
                }
                cx.notify();
            }
            HostToolsEvent::FocusTerminal => {
                if let Some(panel) = self.command_target(window, cx) {
                    panel.read(cx).focus_handle(cx).focus(window, cx);
                }
            }
            HostToolsEvent::QuickConnect => self.open_command_palette(window, cx),
            HostToolsEvent::OpenHost(id) => self.open_host(id, window, cx),
        }
    }

    /// 在 `terminal` 旁的新 Tab 中执行命令（共用它的连接）：跟随日志、进入容器、需要 root 时在终端中执行。`refresh`
    /// 为命令结束后主机工具要重读的页（需要 root 的操作）：跟随着 `terminal` 的主机工具面板（在哪个窗口都算）重读它。
    fn run_beside(
        &mut self,
        terminal: &WeakEntity<TerminalPanel>,
        command: &str,
        title: &SharedString,
        refresh: Option<HostToolsPage>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(source) = terminal.upgrade() else {
            crate::notify::warning(t!("tools.source_gone"), window, cx);
            return;
        };
        let panel = source.update(cx, |p, cx| {
            p.duplicate_running(command.to_string(), title.clone(), window, cx)
        });
        if let Some(page) = refresh {
            let origin = source.entity_id();
            panel.update(cx, |p, cx| {
                p.on_run_finished(move |cx| refresh_tools_after_run(origin, page, cx), cx)
            });
        }
        self.add_terminal(panel, window, cx);
    }

    pub(crate) fn live_logs(&self) -> Vec<Entity<LogPanel>> {
        self.logs.iter().filter_map(|p| p.upgrade()).collect()
    }

    /// 在中间区域打开日志查看器：同一个终端的同一个来源已打开时切过去。
    fn open_logs(&mut self, tab: LogTab, window: &mut Window, cx: &mut Context<Self>) {
        if tab.terminal.upgrade().is_none() {
            crate::notify::warning(t!("tools.source_gone"), window, cx);
            return;
        }
        let existing = self
            .live_logs()
            .into_iter()
            .find(|p| p.read(cx).is_for(tab.terminal_id, &tab.source));
        let panel = match existing {
            Some(panel) => panel,
            None => {
                tracing::info!(source = ?tab.source, "打开日志查看器");
                let panel = LogPanel::open(tab, window, cx);
                self.watch_logs(&panel, window, cx);
                self.dock_area.update(cx, |dock, cx| {
                    dock.add_panel_view(
                        panel_handle(panel.clone()),
                        DockPlacement::Center,
                        None,
                        window,
                        cx,
                    )
                });
                panel
            }
        };
        self.show_logs(&panel, window, cx);
        cx.notify();
    }

    /// 切到这个日志查看器的 Tab 并让它获得焦点（命令面板中选了它时也用）。
    pub(crate) fn show_logs(
        &mut self,
        panel: &Entity<LogPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel_id = PanelId::from(panel.entity_id());
        self.dock_area
            .update(cx, |dock, cx| dock.select_panel(panel_id, window, cx));
        self.set_sync_away(true, cx);
        panel.read(cx).focus_handle(cx).focus(window, cx);
    }

    fn watch_logs(
        &mut self,
        panel: &Entity<LogPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.logs.retain(|p| p.upgrade().is_some());
        self.logs.push(panel.downgrade());
        let id = panel.entity_id();
        let watch = [
            cx.subscribe_in(
                panel,
                window,
                |this, panel, event: &LogPanelEvent, window, cx| match event {
                    LogPanelEvent::Close => {
                        let panel = panel.clone();
                        this.dock_area
                            .update(cx, |dock, cx| dock.remove_panel(panel, window, cx));
                    }
                    LogPanelEvent::RunInTerminal {
                        terminal,
                        command,
                        title,
                    } => this.run_beside(terminal, command, title, None, window, cx),
                },
            ),
            cx.observe_release(panel, move |this, _, _| {
                this.log_watch.remove(&id);
            }),
        ];
        self.log_watch.insert(id, watch);
    }

    // ------------------------------------------------------------ 撰写栏

    /// 打开或关闭撰写栏（快捷键再按一次关闭，焦点不还给终端）。
    fn toggle_compose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.compose.take().is_some() {
            tracing::debug!("关闭撰写栏");
            cx.notify();
            return;
        }
        self.open_compose(window, cx);
    }

    /// 打开撰写栏并让输入框获得焦点。
    pub(crate) fn open_compose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.vault_gate.is_some() {
            return;
        }
        let bar = match &self.compose {
            Some((bar, _)) => bar.clone(),
            None => {
                tracing::debug!("打开撰写栏");
                let workspace = cx.entity().downgrade();
                let bar = cx.new(|cx| ComposeBar::new(workspace, window, cx));
                let sub = cx.subscribe_in(&bar, window, |this, _, event, window, cx| match event {
                    ComposeEvent::Close => {
                        this.compose = None;
                        if let Some(panel) = this.command_target(window, cx) {
                            panel.read(cx).focus_handle(cx).focus(window, cx);
                        }
                        cx.notify();
                    }
                });
                self.compose = Some((bar.clone(), sub));
                bar
            }
        };
        bar.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /// 把内容放进撰写栏（快捷命令选择器 Ctrl+Enter），替换原来的内容。
    pub(crate) fn compose_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.open_compose(window, cx);
        if let Some((bar, _)) = &self.compose {
            bar.update(cx, |bar, cx| bar.set_text(text, window, cx));
        }
    }

    /// 撰写栏可以发送到的终端：连接着的，按打开的顺序（标题）。
    pub(crate) fn compose_candidates(&self, cx: &App) -> Vec<(EntityId, SharedString)> {
        self.live_terminals()
            .into_iter()
            .filter(|p| p.read(cx).is_live(cx))
            .map(|p| (p.entity_id(), p.read(cx).name().clone()))
            .collect()
    }

    /// 撰写栏的目标：当前 Tab（显示着的窗格；同步输入的网格中是网格里的全部）、同一主机、全部终端或自选。
    pub(crate) fn compose_targets(
        &self,
        mode: &ComposeMode,
        window: &Window,
        cx: &App,
    ) -> Vec<Entity<TerminalPanel>> {
        let live: Vec<Entity<TerminalPanel>> = self
            .live_terminals()
            .into_iter()
            .filter(|p| p.read(cx).is_live(cx))
            .collect();
        match mode {
            ComposeMode::All => live,
            // 同步输入的网格中是同步中的终端（标着“已排除”的不发，与键入相同）
            ComposeMode::View => {
                let grid = self.multi_exec.as_ref().is_some_and(|s| !s.away);
                live.into_iter()
                    .filter(|p| {
                        if grid {
                            self.is_sync_member(p.entity_id())
                        } else {
                            p.read(cx).visible_bounds().is_some()
                        }
                    })
                    .collect()
            }
            ComposeMode::Host => {
                let Some(owner) = self
                    .focused_terminal(window, cx)
                    .or_else(|| self.command_target(window, cx))
                    .map(|p| p.read(cx).history_owner())
                    .filter(|o| !o.is_empty())
                else {
                    return Vec::new();
                };
                live.into_iter()
                    .filter(|p| p.read(cx).history_owner() == owner)
                    .collect()
            }
            ComposeMode::Custom(ids) => live
                .into_iter()
                .filter(|p| ids.contains(&p.entity_id()))
                .collect(),
        }
    }

    /// 同步输入中切到 SFTP、编辑器（`away`）或回到网格。
    fn set_sync_away(&mut self, away: bool, cx: &mut Context<Self>) {
        if let Some(state) = self.multi_exec.as_mut() {
            state.away = away;
            cx.notify();
        }
    }

    /// 切到这个 SFTP Tab 并让它的文件列表获得焦点。
    pub(crate) fn show_sftp(
        &mut self,
        panel: &Entity<SftpPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel_id = PanelId::from(panel.entity_id());
        self.dock_area
            .update(cx, |dock, cx| dock.select_panel(panel_id, window, cx));
        self.set_sync_away(true, cx);
        panel.read(cx).focus_handle(cx).focus(window, cx);
    }

    /// 侧边栏中为主机打开 SFTP：新连接、新 Tab，不跟随终端。
    fn open_host_sftp(&mut self, id: &HostId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = AppState::store(cx).get::<Host>(id) else {
            return;
        };
        self.open_sftp(
            host.doc.name.into(),
            SftpTarget::Host(id.clone()),
            None,
            window,
            cx,
        );
    }

    /// 侧边栏中以 root 身份（经 sudo）为主机打开 SFTP：新连接、新 Tab。
    fn open_host_sftp_root(&mut self, id: &HostId, window: &mut Window, cx: &mut Context<Self>) {
        let store = AppState::store(cx);
        let Some(host) = store.get::<Host>(id) else {
            return;
        };
        let Some(target) = SftpTarget::Host(id.clone()).as_root(&store) else {
            return;
        };
        self.open_sftp(host.doc.name.into(), target, None, window, cx);
    }

    /// SFTP 面板的菜单中以 root 身份打开同一台主机（新 Tab）：面板跟随着终端时同样共用它的连接、跟随它的目录。
    fn open_sftp_as_root(
        &mut self,
        panel: &Entity<SftpPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = AppState::store(cx);
        let (name, target, terminal) = {
            let panel = panel.read(cx);
            (
                panel.name().clone(),
                panel.target().as_root(&store),
                panel.linked_terminal(),
            )
        };
        let Some(target) = target else {
            return;
        };
        let name = match &target {
            SftpTarget::Wsl { distro, user } => crate::wsl_name(distro, user),
            _ => name,
        };
        self.open_sftp(name, target, terminal, window, cx);
    }

    /// 为终端打开 SFTP（共用它的连接、跟随它的目录）：新 Tab。
    pub(crate) fn open_sftp_for_terminal(
        &mut self,
        terminal: Entity<TerminalPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<SftpPanel>> {
        let target = terminal.read(cx).sftp_target()?;
        let name = terminal.read(cx).name().clone();
        self.open_sftp(name, target, Some(terminal), window, cx)
    }

    /// 在当前终端右侧打开 SFTP（焦点不在终端上时用最近用过的终端）。
    pub(crate) fn open_sftp_beside(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(terminal) = self.command_target(window, cx) else {
            return;
        };
        let Some(target) = terminal.read(cx).sftp_target() else {
            return;
        };
        let Some(node) = self
            .dock_area
            .read(cx)
            .layout(DockPlacement::Center)
            .and_then(|tree| tree.find_panel_node(PanelId::from(terminal.entity_id())))
        else {
            return;
        };
        let name = terminal.read(cx).name().clone();
        let Some(panel) = self.open_sftp(name, target, Some(terminal), window, cx) else {
            return;
        };
        let target = InsertTarget::Split {
            node,
            placement: Placement::Right,
            size: None,
        };
        self.dock_area.update(cx, |dock, cx| {
            dock.move_panel(PanelId::from(panel.entity_id()), target, window, cx)
        });
    }

    /// 打开一个 SFTP Tab。
    fn open_sftp(
        &mut self,
        name: SharedString,
        target: SftpTarget,
        terminal: Option<Entity<TerminalPanel>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<SftpPanel>> {
        if self.vault_gate.is_some() {
            return None;
        }
        tracing::info!(
            name = %name,
            host = ?target.host(),
            root = target.is_root(),
            beside_terminal = terminal.is_some(),
            "打开 SFTP"
        );
        let panel = SftpPanel::open(name, target, terminal, self.prompter.clone(), window, cx);
        self.watch_sftp(&panel, window, cx);
        self.set_sync_away(true, cx);
        self.dock_area.update(cx, |dock, cx| {
            dock.add_panel_view(
                panel_handle(panel.clone()),
                DockPlacement::Center,
                None,
                window,
                cx,
            )
        });
        cx.notify();
        Some(panel)
    }

    fn watch_sftp(
        &mut self,
        panel: &Entity<SftpPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sftps.retain(|p| p.upgrade().is_some());
        self.sftps.push(panel.downgrade());
        // 同一台主机的编辑器原来的 SFTP Tab 已关闭：交给新的这个（断开后经它重新连接）
        let host = panel.read(cx).target().key();
        for editor in self.live_editors() {
            if editor.read(cx).host_key() == host && !editor.read(cx).has_sftp_panel() {
                editor.update(cx, |editor, cx| editor.set_sftp_panel(panel, cx));
            }
        }
        let id = panel.entity_id();
        let watch = [
            cx.subscribe_in(
                panel,
                window,
                |this, panel, event: &SftpPanelEvent, window, cx| match event {
                    SftpPanelEvent::Close => {
                        let panel = panel.clone();
                        this.dock_area
                            .update(cx, |dock, cx| dock.remove_panel(panel, window, cx));
                    }
                    SftpPanelEvent::OpenEditor { lease, file } => {
                        this.open_editor(panel, lease.clone(), file.clone(), window, cx)
                    }
                    SftpPanelEvent::OpenAsRoot => this.open_sftp_as_root(panel, window, cx),
                },
            ),
            cx.observe_release(panel, move |this, _, _| {
                this.sftp_watch.remove(&id);
            }),
        ];
        self.sftp_watch.insert(id, watch);
    }

    fn live_editors(&self) -> Vec<Entity<EditorPanel>> {
        self.editors.iter().filter_map(|p| p.upgrade()).collect()
    }

    /// 在编辑器中打开读取到的远程文件：同一台主机的同一个文件已打开时切过去。
    fn open_editor(
        &mut self,
        sftp: &Entity<SftpPanel>,
        lease: std::rc::Rc<SessionLease>,
        file: termlet_core::sftp::TextFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let host = lease.session().host().to_string();
        let existing = self.live_editors().into_iter().find(|e| {
            let e = e.read(cx);
            e.host_key() == host && e.remote_path() == file.path
        });
        let editor = match existing {
            Some(editor) => editor,
            None => {
                tracing::info!(host = %host, path = %file.path, bytes = file.size, "打开远程文件编辑");
                let name = sftp.read(cx).name().clone();
                let editor = EditorPanel::open(lease, sftp.downgrade(), name, file, window, cx);
                self.watch_editor(&editor, window, cx);
                self.dock_area.update(cx, |dock, cx| {
                    dock.add_panel_view(
                        panel_handle(editor.clone()),
                        DockPlacement::Center,
                        None,
                        window,
                        cx,
                    )
                });
                editor
            }
        };
        self.show_editor(&editor, window, cx);
        cx.notify();
    }

    /// 切到这个编辑器 Tab 并让它获得焦点。
    pub(crate) fn show_editor(
        &mut self,
        editor: &Entity<EditorPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel_id = PanelId::from(editor.entity_id());
        self.dock_area
            .update(cx, |dock, cx| dock.select_panel(panel_id, window, cx));
        self.set_sync_away(true, cx);
        editor.read(cx).focus_handle(cx).focus(window, cx);
    }

    fn watch_editor(
        &mut self,
        editor: &Entity<EditorPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editors.retain(|p| p.upgrade().is_some());
        self.editors.push(editor.downgrade());
        let id = editor.entity_id();
        let watch = [
            cx.subscribe_in(
                editor,
                window,
                |this, editor, event: &EditorPanelEvent, window, cx| match event {
                    EditorPanelEvent::Close => {
                        let editor = editor.clone();
                        this.dock_area
                            .update(cx, |dock, cx| dock.remove_panel(editor, window, cx));
                    }
                },
            ),
            cx.observe_release(editor, move |this, _, _| {
                this.editor_watch.remove(&id);
            }),
        ];
        self.editor_watch.insert(id, watch);
    }

    /// 为终端的主机添加端口转发：保存后切到侧边栏的“转发”页（可以勾选立即启动，共用这个终端的连接）。
    fn add_forward_for(
        &mut self,
        panel: &Entity<TerminalPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let host = panel.read(cx).host().cloned();
        let sidebar = self.sidebar.downgrade();
        let on_saved: crate::forward_dialog::OnSaved = Rc::new(move |_, cx| {
            let _ = sidebar.update(cx, |sidebar, cx| {
                sidebar.show_page(crate::sidebar::Page::Forwards, cx)
            });
        });
        crate::forward_dialog::open_new(host, Some(panel.downgrade()), Some(on_saved), window, cx);
    }

    /// 标题栏的“N 个转发”、命令面板：切到侧边栏的“转发”页（侧边栏隐藏时显示出来）。
    pub(crate) fn show_forwards(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sidebar_hidden(cx) {
            self.toggle_sidebar(window, cx);
        }
        self.sidebar.update(cx, |sidebar, cx| {
            sidebar.show_page(crate::sidebar::Page::Forwards, cx)
        });
    }

    /// 一批传输结束：这个窗口的弹出汇总提示（主机名取那台主机的 SFTP Tab 的名称）。
    fn on_sftp_event(&mut self, event: SftpEvent, window: &mut Window, cx: &mut Context<Self>) {
        let SftpEvent::BatchEnded(report) = event else {
            return;
        };
        if report.owner != self.owner {
            return;
        }
        let host = self
            .live_sftps()
            .into_iter()
            .find(|p| p.read(cx).target().key() == report.host)
            .map(|p| p.read(cx).name().clone())
            .unwrap_or_else(|| report.host.clone().into());
        crate::sftp::batch_notice::announce(&report, host, window, cx);
    }

    fn add_terminal(
        &mut self,
        panel: Entity<TerminalPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.watch_terminal(&panel, window, cx);
        self.dock_area.update(cx, |dock, cx| {
            dock.add_panel_view(panel_handle(panel), DockPlacement::Center, None, window, cx)
        });
        self.refresh_host_statuses(cx);
        cx.notify();
    }

    /// 记下新的终端并观察它（状态变化时更新侧边栏的圆点）；放进 Dock 由调用方负责。
    fn watch_terminal(
        &mut self,
        panel: &Entity<TerminalPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.terminals.retain(|panel| panel.upgrade().is_some());
        self.terminals.push(panel.downgrade());
        let id = panel.entity_id();
        let weak = panel.downgrade();
        let watch = [
            cx.subscribe_in(
                panel,
                window,
                |this, panel, event: &TerminalPanelEvent, window, cx| match event {
                    TerminalPanelEvent::Close => {
                        let panel = panel.clone();
                        this.dock_area
                            .update(cx, |dock, cx| dock.remove_panel(panel, window, cx));
                    }
                    TerminalPanelEvent::Notify { title, body } => {
                        this.notify(panel, title.clone(), body.clone(), window, cx)
                    }
                    TerminalPanelEvent::AddForward => this.add_forward_for(panel, window, cx),
                    TerminalPanelEvent::SyncTyped(data) => {
                        this.on_sync_typed(panel, data.clone(), window, cx)
                    }
                    TerminalPanelEvent::SyncPaste(text) => {
                        this.on_sync_paste(panel, text.clone(), window, cx)
                    }
                    TerminalPanelEvent::OpenOverview => this.open_overview(panel, window, cx),
                },
            ),
            cx.observe_in(panel, window, |this, _, window, cx| {
                this.refresh_host_statuses(cx);
                this.prune_sync(window, cx);
                this.sync_tools_target(window, cx);
            }),
            cx.observe_release_in(panel, window, move |this, _, window, cx| {
                this.terminal_watch.remove(&id);
                this.refresh_host_statuses(cx);
                this.host_tools.update(cx, |tools, _| tools.forget(id));
                this.sync_tools_target(window, cx);
            }),
            cx.on_focus_in(
                &panel.read(cx).root_focus(),
                window,
                move |this, window, cx| {
                    if let Some(panel) = weak.upgrade() {
                        this.note_recent_terminal(&panel);
                    }
                    this.sync_tools_target(window, cx);
                },
            ),
        ];
        self.terminal_watch.insert(panel.entity_id(), watch);
    }

    // ------------------------------------------------------------ 工作区

    /// 把中间区域的布局保存为工作区（每个窗格取正在显示的终端）。
    pub fn save_as_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let terminals = self.live_terminals();
        let sftps = self.live_sftps();
        let leaf = |id: PanelId| -> workspaces::Leaf {
            if let Some(p) = terminals
                .iter()
                .find(|p| PanelId::from(p.entity_id()) == id)
            {
                return p
                    .read(cx)
                    .workspace_host()
                    .map(|host| (SessionKind::Terminal, host));
            }
            sftps
                .iter()
                .find(|p| PanelId::from(p.entity_id()) == id)
                .and_then(|p| p.read(cx).target().host().cloned())
                .map(|host| (SessionKind::Sftp, host))
        };
        let layout = self
            .dock_area
            .read(cx)
            .layout(DockPlacement::Center)
            .and_then(|tree| workspaces::capture(tree, &leaf));
        let Some(layout) = layout else {
            crate::notify::error(t!("workspace.nothing"), window, cx);
            return;
        };
        let (panes, depth) = workspaces::measure(&layout);
        if panes > workspaces::MAX_PANES || depth > workspaces::MAX_DEPTH {
            crate::notify::error(
                t!(
                    "workspace.too_large",
                    panes = workspaces::MAX_PANES,
                    depth = workspaces::MAX_DEPTH
                ),
                window,
                cx,
            );
            return;
        }
        let default_name = self
            .focused_terminal(window, cx)
            .map(|p| p.read(cx).name().to_string())
            .unwrap_or_default();
        tracing::debug!(panes, depth, "保存为工作区：询问名称");
        workspaces::prompt_save(layout, default_name, window, cx);
    }

    /// 打开保存的工作区：中间区域是空的就按工作区排列，否则排在已有布局的右侧。
    /// 同一台主机的多个窗格共用一个连接；主机已删除的窗格跳过。
    pub(crate) fn open_saved_workspace(
        &mut self,
        id: &WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.vault_gate.is_some() {
            return;
        }
        let store = AppState::store(cx);
        let Some(record) = store.get::<SavedWorkspace>(id) else {
            return;
        };
        let layout = record.doc.layout;
        tracing::info!(workspace = %record.doc.name, "打开工作区");
        // 先按布局的先后为每个会话建好面板，再交给布局（两边按同样的顺序遍历）。
        // 先建终端，SFTP 再跟随同一台主机的第一个终端（共用它的连接）
        let mut sessions = Vec::new();
        collect_sessions(&layout, &mut sessions);
        let local = store.local_settings();
        let mut leads: HashMap<HostId, Entity<TerminalPanel>> = HashMap::new();
        let mut created: Vec<Option<Entity<TerminalPanel>>> = Vec::new();
        for (kind, host) in &sessions {
            if *kind == SessionKind::Sftp {
                created.push(None);
                continue;
            }
            let panel = if let Some(lead) = leads.get(host) {
                Some(lead.update(cx, |lead, cx| lead.duplicate(window, cx)))
            } else if let Some((distro, user)) = workspaces::wsl_of(host) {
                let name = crate::wsl_name(&distro, &user);
                Some(TerminalPanel::open(
                    name,
                    Session::Wsl { distro, user },
                    self.prompter.clone(),
                    window,
                    cx,
                ))
            } else if let Some(shell) = workspaces::local_shell_of(host) {
                local_shell::find_profile(shell, &local, cx).map(|profile| {
                    let name = local_shell::display_name(&profile);
                    TerminalPanel::open(
                        name,
                        Session::Local { profile },
                        self.prompter.clone(),
                        window,
                        cx,
                    )
                })
            } else {
                store.get::<Host>(host).map(|record| {
                    let panel = TerminalPanel::open(
                        record.doc.name.into(),
                        Session::Host { host: host.clone() },
                        self.prompter.clone(),
                        window,
                        cx,
                    );
                    leads.insert(host.clone(), panel.clone());
                    panel
                })
            };
            created.push(panel);
        }
        let panels: Vec<Entity<TerminalPanel>> = created.iter().flatten().cloned().collect();
        let mut views = Vec::new();
        let mut sftps: Vec<Entity<SftpPanel>> = Vec::new();
        for ((kind, host), terminal) in sessions.iter().zip(created) {
            let view = match kind {
                SessionKind::Terminal => terminal.map(panel_handle),
                SessionKind::Sftp => store.get::<Host>(host).map(|record| {
                    let panel = SftpPanel::open(
                        record.doc.name.into(),
                        SftpTarget::Host(host.clone()),
                        leads.get(host).cloned(),
                        self.prompter.clone(),
                        window,
                        cx,
                    );
                    sftps.push(panel.clone());
                    panel_handle(panel)
                }),
            };
            views.push(view);
        }
        let first: Option<FocusHandle> = panels
            .first()
            .map(|p| p.read(cx).focus_handle(cx))
            .or_else(|| sftps.first().map(|p| p.read(cx).focus_handle(cx)));
        let Some(first) = first else {
            crate::notify::error(t!("workspace.hosts_gone"), window, cx);
            return;
        };
        for panel in &panels {
            self.watch_terminal(panel, window, cx);
        }
        for panel in &sftps {
            self.watch_sftp(panel, window, cx);
        }
        let mut views = views.into_iter();
        let mut make = |_: SessionKind, _: &HostId| views.next().flatten();
        let dock = self.dock_area.read(cx);
        let extent = dock.bounds().size;
        let empty = dock.is_empty(DockPlacement::Center, cx);
        let opened = if empty {
            workspaces::build(&layout, extent, &mut make, cx)
        } else {
            // 已有布局在左、工作区在右，各占一半
            let half = gpui_kit::Size {
                width: extent.width / 2.,
                height: extent.height,
            };
            let existing = dock
                .layout(DockPlacement::Center)
                .and_then(|tree| workspaces::existing(dock, tree.root(), cx));
            workspaces::build(&layout, half, &mut make, cx).map(|added| match existing {
                Some(existing) => DockLayout::h_split()
                    .child(existing, None)
                    .child(added, None),
                None => added,
            })
        };
        if let Some(opened) = opened {
            self.dock_area
                .update(cx, |dock, cx| dock.set_center(opened, window, cx));
        }
        self.refresh_host_statuses(cx);
        first.focus(window, cx);
        cx.notify();
    }

    /// 侧边栏主机行行首的圆点：每台主机已打开的终端中最好的状态。
    fn refresh_host_statuses(&mut self, cx: &mut Context<Self>) {
        let mut statuses: HashMap<HostId, HostStatus> = HashMap::new();
        for panel in self.live_terminals() {
            let panel = panel.read(cx);
            let Some(host) = panel.host() else {
                continue;
            };
            let status = panel.status();
            statuses
                .entry(host.clone())
                .and_modify(|s| {
                    s.count += 1;
                    s.status = s.status.max(status);
                })
                .or_insert(HostStatus { status, count: 1 });
        }
        let tree = self.sidebar.read(cx).host_tree().clone();
        tree.update(cx, |tree, cx| tree.set_statuses(statuses, cx));
    }

    // ------------------------------------------------------------ 侧边栏

    /// 窗口中的工作区（主窗口才有）。
    pub(crate) fn in_window(window: &Window, cx: &App) -> Option<Entity<Workspace>> {
        window
            .root::<gpui_kit::base::Root>()
            .flatten()
            .and_then(|root| root.read(cx).view().clone().downcast::<Workspace>().ok())
    }

    /// 在这个窗口侧边栏的主机页中让主机或分组看得到（主机、分组对话框保存后）：切到主机页，展开它所在的各级分组
    /// 并选中它（[`crate::sidebar::host_tree::HostTree::reveal`]）。不是主窗口时什么也不做。
    pub(crate) fn reveal_in_sidebar(
        key: crate::sidebar::rows::RowKey,
        window: &Window,
        cx: &mut App,
    ) {
        if let Some(workspace) = Self::in_window(window, cx) {
            let sidebar = workspace.read(cx).sidebar.clone();
            sidebar.update(cx, |sidebar, cx| sidebar.reveal(key, cx));
        }
    }

    /// 在侧边栏中显示这台主机（终端、文件管理 Tab 菜单的“在侧栏中显示主机”与命令面板）：侧边栏隐藏着时先显示，
    /// 切到主机页，搜索着时清空搜索，展开它所在的各级分组、选中并滚动到它，焦点移到主机列表。
    pub(crate) fn show_host_in_sidebar(
        &mut self,
        host: &HostId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::debug!(%host, "在侧边栏中显示主机");
        if self.sidebar_hidden(cx) {
            AppState::ui(cx).update(cx, |ui, cx| ui.update(|s| s.sidebar_hidden = false, cx));
        }
        let host = host.clone();
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.show_host(host, window, cx));
        cx.notify();
    }

    /// 主机工具面板（测试中直接切换它的页：本地终端时页签置灰、点不动）。
    #[cfg(test)]
    pub(crate) fn host_tools_panel(&self) -> Entity<crate::host_tools::HostToolsPanel> {
        self.host_tools.clone()
    }

    /// 侧边栏的主机树（测试检查展开、选中时用）。
    #[cfg(test)]
    pub(crate) fn host_tree(&self, cx: &App) -> Entity<crate::sidebar::host_tree::HostTree> {
        self.sidebar.read(cx).host_tree().clone()
    }

    fn sidebar_hidden(&self, cx: &App) -> bool {
        AppState::global(cx).ui.read(cx).state().sidebar_hidden
    }

    fn toggle_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let hidden = !self.sidebar_hidden(cx);
        tracing::debug!(hidden, "隐藏 / 显示侧边栏");
        AppState::ui(cx).update(cx, |ui, cx| ui.update(|s| s.sidebar_hidden = hidden, cx));
        // 隐藏后焦点回到当前终端
        if hidden && let Some(panel) = self.command_target(window, cx) {
            panel.read(cx).focus_handle(cx).focus(window, cx);
        }
    }

    /// 侧边栏宽度的范围（像素）：最小宽度到最大宽度，并给主区域留出空间（窗口太窄时以最小宽度为准）。最小宽度为
    /// [`SIDEBAR_MIN`]，顶部的页签按当前语言的文案放不下时放宽到放得下。
    fn sidebar_range(&self, window: &Window) -> (f32, f32) {
        let scale = crate::ui_scale::factor(window);
        let min = (SIDEBAR_MIN * scale).max(self.sidebar_tabs_width.get().unwrap_or(0.));
        let window_width = f32::from(window.viewport_size().width);
        (
            min,
            (SIDEBAR_MAX * scale)
                .min(window_width - CONTENT_MIN * scale)
                .max(min),
        )
    }

    /// 侧边栏的宽度：记下的宽度按界面大小缩放，限制在 [`Self::sidebar_range`] 之内。
    fn sidebar_width(&self, window: &Window, cx: &App) -> Pixels {
        let saved = AppState::global(cx)
            .ui
            .read(cx)
            .state()
            .sidebar_width
            .unwrap_or(SIDEBAR_DEFAULT);
        let (min, max) = self.sidebar_range(window);
        px((saved * crate::ui_scale::factor(window))
            .clamp(min, max)
            .round())
    }

    /// 记下侧边栏的宽度（像素，换算成 100% 的界面大小时的宽度）。
    fn save_sidebar_width(&self, width: f32, window: &Window, cx: &mut Context<Self>) {
        let width = (width / crate::ui_scale::factor(window)).round();
        AppState::ui(cx).update(cx, |ui, cx| {
            ui.update(|s| s.sidebar_width = Some(width), cx)
        });
    }

    fn on_sidebar_resize(
        &mut self,
        event: &DragMoveEvent<SidebarResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (min, max) = self.sidebar_range(window);
        let width = f32::from(event.event.position.x).clamp(min, max);
        self.save_sidebar_width(width, window, cx);
    }

    /// 拖动条获得焦点时：← 变窄、→ 变宽。
    fn on_sidebar_resize_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let step = match event.keystroke.key.as_str() {
            "left" => -SIDEBAR_STEP,
            "right" => SIDEBAR_STEP,
            _ => return,
        };
        let (min, max) = self.sidebar_range(window);
        let step = step * crate::ui_scale::factor(window);
        let width = (f32::from(self.sidebar_width(window, cx)) + step).clamp(min, max);
        self.save_sidebar_width(width, window, cx);
        cx.stop_propagation();
    }

    /// 侧边栏右边缘的拖动区域：骑在分隔线上，比线宽，不占布局。
    fn render_resize_handle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let drag = cx.theme().drag_border;
        div()
            .id("sidebar-resize")
            .test_support()
            .track_focus(&self.sidebar_resize_focus)
            .absolute()
            .top_0()
            .bottom_0()
            .right(-gpui_kit::rems(0.25))
            .w_2()
            .cursor_col_resize()
            .hover(|s| s.bg(drag.opacity(0.5)))
            .focus(|s| s.bg(drag))
            .tooltip(|window, cx| Tooltip::new(t!("sidebar.resize")).build(window, cx))
            .on_key_down(cx.listener(Self::on_sidebar_resize_key))
            .on_drag(SidebarResize, |_, _, _, cx| cx.new(|_| SidebarResize))
            .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                this.sidebar_resize_focus.focus(window, cx);
                if event.click_count() >= 2 {
                    AppState::ui(cx).update(cx, |ui, cx| ui.update(|s| s.sidebar_width = None, cx));
                }
            }))
    }

    // ------------------------------------------------------------ 渲染

    fn render_start_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let hidden = self.sidebar_hidden(cx);
        // 左右留白：窗口窄、界面放大时说明换行后不贴着两边
        v_flex()
            .size_full()
            .px_6()
            .items_center()
            .justify_center()
            .gap_2()
            .child(crate::brand::logo(gpui_kit::rems(4.)).mb_2())
            .child(div().text_lg().child(t!("start.empty_title")))
            .when(!hidden, |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(prose(t!("start.empty_hint"))),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .pt_4()
                    .when(hidden, |this| {
                        this.child(
                            Button::new("start-show-sidebar")
                                .icon(IconName::PanelLeftOpen)
                                .label(t!("start.show_sidebar"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.toggle_sidebar(window, cx)
                                })),
                        )
                    })
                    .child(
                        Button::new("start-local-terminal")
                            .icon(IconName::SquareTerminal)
                            .label(t!("start.open_local_terminal"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_local_terminal(window, cx)
                            })),
                    )
                    .child(
                        Button::new("start-import-hosts")
                            .icon(IconName::Download)
                            .label(t!("hosts.import_button"))
                            .on_click(|_, window, cx| crate::backup_ui::open_import(window, cx)),
                    ),
            )
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 应用锁：整个窗口只有锁屏（工作区的动作与快捷键都不生效）
        if let Some(screen) = &self.lock_screen {
            return screen.clone().into_any_element();
        }
        // 焦点丢了（获得焦点的元素被移除）：收回到工作区，快捷键仍然可用
        if window.focused(cx).is_none() {
            let focus = self.focus.clone();
            window.defer(cx, move |window, cx| {
                if window.focused(cx).is_none() {
                    focus.focus(window, cx);
                }
            });
        }
        let locked = self.vault_gate.is_some();
        let content = match &self.vault_gate {
            Some(gate) => gate.clone().into_any_element(),
            None if self.dock_area.read(cx).is_empty(DockPlacement::Center, cx) => {
                self.render_start_page(cx).into_any_element()
            }
            None if self.multi_exec.as_ref().is_some_and(|s| !s.away) => {
                self.render_sync_grid(window, cx)
            }
            None => self.dock_area.clone().into_any_element(),
        };
        let sync_banner = if locked {
            None
        } else {
            self.render_sync_banner(cx)
        };
        let compose = self
            .compose
            .as_ref()
            .filter(|_| !locked)
            .map(|(bar, _)| bar.clone());
        let show_tools = !locked && AppState::global(cx).ui.read(cx).state().host_tools_open;
        let show_sidebar = !locked && !self.sidebar_hidden(cx);
        let show_snippet_bar = !locked && crate::snippets::bar::is_open(cx);
        let sidebar_width = self.sidebar_width(window, cx);
        // Tab 放在左侧：窗格顶上只画细标题，Tab 竖排在侧边栏右边的一列（这一帧的停靠区在下面画，先写入）
        let tabs_on_left = Self::tabs_on_left(cx);
        self.title_only.set(tabs_on_left);
        let tab_list = (tabs_on_left && !locked).then(|| {
            let left = if show_sidebar {
                f32::from(sidebar_width)
            } else {
                0.
            };
            self.render_tab_list(left, window, cx)
        });
        v_flex()
            .size_full()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &NewLocalTerminal, window, cx| {
                this.open_local_terminal(window, cx)
            }))
            .on_action(cx.listener(|this, _: &DuplicateSession, window, cx| {
                this.duplicate_session(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &ToggleSidebar, window, cx| this.toggle_sidebar(window, cx)),
            )
            .on_action(cx.listener(|_, _: &ToggleFullscreen, window, _| window.toggle_fullscreen()))
            .on_action(cx.listener(|_, _: &OpenSettings, window, cx| {
                if !crate::lock_ui::is_locked(cx) {
                    settings::open(settings::Page::General, window, cx)
                }
            }))
            .on_action(cx.listener(|_, _: &OpenAbout, window, cx| {
                if !crate::lock_ui::is_locked(cx) {
                    settings::open(settings::Page::About, window, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &OpenCommandPalette, window, cx| {
                this.open_command_palette(window, cx)
            }))
            .on_action(cx.listener(|this, _: &SaveAsWorkspace, window, cx| {
                this.save_as_workspace(window, cx)
            }))
            .on_action(cx.listener(|this, _: &OpenCommandPicker, window, cx| {
                this.open_command_picker(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &OpenSftpBeside, window, cx| {
                    this.open_sftp_beside(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &RevealHostInSidebar, window, cx| {
                let host = this
                    .command_target(window, cx)
                    .and_then(|panel| panel.read(cx).host().cloned());
                if let Some(host) = host {
                    this.show_host_in_sidebar(&host, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleMultiExec, window, cx| {
                this.toggle_multi_exec(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &ToggleCompose, window, cx| this.toggle_compose(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &OpenCompose, window, cx| this.open_compose(window, cx)),
            )
            .on_action(cx.listener(|this, _: &ToggleHostTools, window, cx| {
                this.toggle_host_tools(window, cx)
            }))
            .on_action(cx.listener(|this, _: &OpenHostToolsOverview, window, cx| {
                this.focus_host_tools_overview(window, cx)
            }))
            .on_action(cx.listener(|this, _: &PopOutHostOverview, window, cx| {
                this.pop_out_host_overview(window, cx)
            }))
            .on_action(|_: &crate::lock_ui::LockNow, _, cx| {
                crate::lock_ui::lock_now(cx);
            })
            .on_action(|_: &crate::ui_scale::ZoomInUi, window, cx| {
                crate::ui_scale::zoom(crate::ui_scale::Step::In, window, cx)
            })
            .on_action(|_: &crate::ui_scale::ZoomOutUi, window, cx| {
                crate::ui_scale::zoom(crate::ui_scale::Step::Out, window, cx)
            })
            .on_action(|_: &crate::ui_scale::ResetUiZoom, window, cx| {
                crate::ui_scale::zoom(crate::ui_scale::Step::Reset, window, cx)
            })
            .on_action(|_: &NewWindow, window, cx| {
                let near = window.window_bounds().get_bounds();
                let display = window.display(cx).map(|d| d.id());
                crate::cli_bridge::open_window_near(near, display, cx);
            })
            .on_action(cx.listener(|this, _: &MoveTabToNewWindow, window, cx| {
                this.move_current_tab_to_new_window(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &FocusTabList, window, cx| this.focus_tab_list(window, cx)),
            )
            .on_action(cx.listener(|this, _: &SplitRight, window, cx| {
                this.split(Placement::Right, window, cx)
            }))
            .on_action(cx.listener(|this, _: &NextTab, window, cx| this.cycle_tab(1, window, cx)))
            .on_action(cx.listener(|this, _: &PrevTab, window, cx| this.cycle_tab(-1, window, cx)))
            .on_action(
                cx.listener(|this, _: &ActivateTab1, window, cx| this.activate_tab(0, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ActivateTab2, window, cx| this.activate_tab(1, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ActivateTab3, window, cx| this.activate_tab(2, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ActivateTab4, window, cx| this.activate_tab(3, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ActivateTab5, window, cx| this.activate_tab(4, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ActivateTab6, window, cx| this.activate_tab(5, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ActivateTab7, window, cx| this.activate_tab(6, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ActivateTab8, window, cx| this.activate_tab(7, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ActivateTab9, window, cx| this.activate_tab(8, window, cx)),
            )
            .on_action(cx.listener(|this, _: &SplitDown, window, cx| {
                this.split(Placement::Bottom, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusPaneLeft, window, cx| {
                this.focus_pane(Placement::Left, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusPaneRight, window, cx| {
                this.focus_pane(Placement::Right, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusPaneUp, window, cx| {
                this.focus_pane(Placement::Top, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusPaneDown, window, cx| {
                this.focus_pane(Placement::Bottom, window, cx)
            }))
            .on_drag_move(cx.listener(Self::on_sidebar_resize))
            .on_drag_move(cx.listener(Self::on_tab_list_resize))
            // Tab 拖到窗口外（见 move_tab）：指针离开窗口后照样收得到移动与松开
            .on_drag_move(cx.listener(Self::on_dock_tab_drag))
            .on_drag_move(cx.listener(Self::on_sole_tab_drag))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_tab_drag_end))
            .capture_any_mouse_up(cx.listener(Self::on_tab_drag_end))
            .children(self.render_drag_out_hint(window, cx))
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_title_bar(window, cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .when(show_sidebar, |this| {
                        this.child(
                            div()
                                .relative()
                                .flex_none()
                                .w(sidebar_width)
                                .h_full()
                                .border_r_1()
                                .border_color(cx.theme().sidebar_border)
                                .child(self.sidebar.clone())
                                // 排在侧边栏之后：量这一帧页签排开的宽度
                                .child({
                                    let sidebar = self.sidebar.clone();
                                    self.sidebar_tabs_width.measure(sidebar_width, move |cx| {
                                        sidebar.read(cx).tabs_overflow()
                                    })
                                })
                                .child(self.render_resize_handle(cx)),
                        )
                    })
                    .children(tab_list)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .children(sync_banner)
                            .child(
                                div()
                                    .flex_1()
                                    .min_h_0()
                                    // 点了中间区域（终端、Tab）：这次点击处理完、焦点落定后记下焦点所在的终端
                                    // （窗口不在前台时没有焦点事件，见 sync_tools_target）
                                    .capture_any_mouse_down(cx.listener(|_, _, window, cx| {
                                        cx.defer_in(window, |this, window, cx| {
                                            this.sync_tools_target(window, cx)
                                        });
                                    }))
                                    // 拖进来的会话录制（.cast）打开回放；终端、文件管理自己接的拖放不经过这里
                                    .can_drop(|dragged, _, _| {
                                        dragged
                                            .downcast_ref::<gpui_kit::ExternalPaths>()
                                            .is_some_and(|paths| {
                                                paths.paths().iter().any(|p| {
                                                    termlet_core::recording::is_cast_path(p)
                                                })
                                            })
                                    })
                                    .on_drop(cx.listener(
                                        |this, paths: &gpui_kit::ExternalPaths, window, cx| {
                                            this.open_replay_files(paths.paths(), window, cx);
                                        },
                                    ))
                                    .child(content),
                            )
                            .when(show_snippet_bar, |el| el.child(self.snippet_bar.clone()))
                            .when_some(compose, |el, bar| el.child(bar)),
                    )
                    .when(show_tools, |el| el.child(self.host_tools.clone())),
            )
            .into_any_element()
    }
}

/// 布局树中的全部 Tab 分组（按布局的顺序）：组中的面板，以及显示中的是第几个。
fn collect_tab_groups(node: &PaneNode, out: &mut Vec<(Vec<PanelId>, usize)>) {
    match node.kind() {
        PaneRef::Tabs { panels, active_ix } => out.push((panels.to_vec(), active_ix)),
        PaneRef::Split { children, .. } => {
            for child in children {
                collect_tab_groups(child, out);
            }
        }
    }
}

/// 终端所在的主窗口（终端的 Tab 可能移过窗口）。
pub(crate) fn window_of_terminal(
    terminal: EntityId,
    cx: &App,
) -> Option<gpui_kit::AnyWindowHandle> {
    move_tab::open_workspaces(cx)
        .into_iter()
        .find(|(_, w)| {
            w.read(cx)
                .live_terminals()
                .iter()
                .any(|t| t.entity_id() == terminal)
        })
        .map(|(handle, _)| handle)
}

/// “在终端中执行”的命令结束（或它的 Tab 关了）后：各窗口中跟随着发起操作的终端 `terminal` 的主机工具面板重读发起
/// 操作的页 `page`（终端可能已经移到了别的窗口）。
pub(crate) fn refresh_tools_after_run(terminal: EntityId, page: HostToolsPage, cx: &mut App) {
    for (_, workspace) in move_tab::open_workspaces(cx) {
        let tools = workspace.read(cx).host_tools.clone();
        tools.update(cx, |tools, cx| tools.refresh_page_of(terminal, page, cx));
    }
}

impl Workspace {
    /// 窗口关闭（工作区释放）时，别的窗口还在用的交给那个窗口，不随这个窗口停止、关闭：共用那个窗口里终端的连接的
    /// 端口转发，那个窗口里的编辑器、文件管理用着的 SFTP 会话（Tab 移过去了，原来的窗口里还有用它们的 Tab 时没有
    /// 跟过去，见 `move_tab.rs`）。这个窗口在会话上还没结束的传输照常取消。
    fn hand_over_on_close(&self, cx: &App) {
        for (_, other) in move_tab::open_workspaces(cx) {
            let other = other.read(cx);
            if other.owner == self.owner {
                continue;
            }
            for terminal in other.live_terminals() {
                let group = terminal.read(cx).connection_group();
                self.forwards
                    .reassign_group(&self.owner, group, &other.owner);
            }
            let sessions = other
                .live_sftps()
                .into_iter()
                .filter_map(|p| p.read(cx).session_id())
                .chain(
                    other
                        .live_editors()
                        .into_iter()
                        .map(|e| e.read(cx).session_id()),
                );
            for session in sessions {
                self.sftp.hand_over(&session, &self.owner, &other.owner);
            }
        }
    }
}

impl Drop for Workspace {
    /// 窗口关闭：关闭它的 SFTP 会话（删除它已结束的传输记录与临时文件）、停止它启动的端口转发（别的窗口还在用的
    /// 已经交出去，见 [`Workspace::hand_over_on_close`]）。
    fn drop(&mut self) {
        tracing::info!(owner = %self.owner, "关闭主窗口");
        let owner = self.owner.clone();
        let sftp = self.sftp.clone();
        let forwards = self.forwards.clone();
        crate::cli_bridge::spawn_cleanup(async move {
            sftp.close_owner(Some(&owner)).await;
            forwards.close_owner(&owner).await;
        });
    }
}

/// 标题栏中放按钮的容器：遮挡下层的拖动区域，按钮才收得到真实的鼠标点击（见 `title_bar`）。
fn no_drag() -> gpui_kit::Div {
    div().occlude()
}

/// 布局中的会话，按先后顺序（与 [`workspaces::build`] 遍历的顺序相同）。
fn collect_sessions(node: &LayoutNode, out: &mut Vec<(SessionKind, HostId)>) {
    match node {
        LayoutNode::Session { kind, host } => out.push((*kind, host.clone())),
        LayoutNode::Split { children, .. } => children
            .iter()
            .for_each(|child| collect_sessions(child, out)),
    }
}
