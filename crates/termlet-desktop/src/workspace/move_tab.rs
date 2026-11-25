//! Tab 移到别的窗口：Tab 菜单的“移到新窗口”“移到窗口 ›”，命令面板与可自定义的快捷键（默认没有），以及把 Tab
//! 拖到窗口外放下——放在另一个 Termlet 窗口上时移到那个窗口，放在别处时在放下的位置新开一个窗口。
//!
//! - 移过去的是面板实体本身（GPUI 的实体不属于哪个窗口，换个窗口渲染即可）：终端的连接、终端引擎（画面与回滚）、
//!   正在运行的程序、录制与会话日志，文件管理的会话与进行中的传输，编辑器的未保存修改与撤销记录都不断开、不丢。
//! - gpui-pre 的 `subscribe_in`、`observe_in`、`defer_in` 与 `spawn_in` 任务中的 `update_in` 按实体“当前所在的窗口”
//!   （最近一次画出它的窗口）执行，面板在新窗口画出后自动跟过去。真正绑在登记时的窗口上的是焦点的监听（`on_focus`、
//!   `on_blur`）、窗口的观察（激活、可见性）、`observe_global_in`、`spawn_in` 任务自己的窗口（对话框的队列）与输入框，
//!   由各面板的 `rebind_window` 在新窗口中重新登记、重建；连接时的询问换成新窗口的。终端、文件管理正在连接时不能移，
//!   提示原因（见各面板的 `move_blocker`）。
//! - 原来的窗口里：同步输入、主机工具跟随、最近用过的终端中去掉它；文件管理的会话与传输改属新窗口（原来的窗口关闭
//!   时不再关闭它们）。移走最后一个 Tab 后窗口照常留着、显示开始页，与关掉最后一个 Tab 相同。
//! - 跟着 Tab 走的（[`Workspace::release_moved`]）：共用移走的终端的连接的端口转发（与终端的连接同生命周期，
//!   原来的窗口里不再有这条连接的终端时改属新窗口；从侧边栏单独启动、自己连接的转发仍属启动它的窗口）；单独移走的
//!   编辑器用着的 SFTP 会话（原来的窗口里不再有用它的文件管理、编辑器时改属新窗口）。原来的窗口里还有用着它们的 Tab
//!   时仍属原来的窗口，原来的窗口关闭时再交给还在用它们的窗口（见 `workspace.rs` 的 `hand_over_on_close`）。
//! - 拖出窗口：按下鼠标后窗口捕获指针（Windows 的 `SetCapture`，macOS、X11 也是这样），拖到窗口外仍收得到移动与松开。
//!   拖到窗口外时在窗口里贴着指针离开的地方提示松开后会怎样；GPUI 的拖动预览只画在窗口之内，窗口外没有跟随指针的
//!   预览。窗口里只有一个 Tab 时停靠区不让拖动它（[`SoleTabDrag`] 代替），只能拖到另一个窗口上。

use std::sync::atomic::{AtomicU64, Ordering};

use gpui_kit::assets::IconName;
use gpui_kit::component::dock::{BasePanelView, DockPlacement, DragPanel, PanelId, panel_handle};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex};
use gpui_kit::{
    AnyElement, AnyWindowHandle, App, Bounds, ClickEvent, Context, DisplayId, DragMoveEvent,
    Entity, EntityId, Global, InteractiveElement as _, IntoElement, MouseButton, MouseUpEvent,
    ParentElement as _, Pixels, Point, SharedString, Styled as _, TestSupportExt as _, WeakEntity,
    Window, anchored, deferred, div, point, rems,
};

use termlet_core::sftp::SessionId;

use super::{Workspace, collect_tab_groups};
use crate::batch_run::BatchPanel;
use crate::editor::EditorPanel;
use crate::host_tools::LogPanel;
use crate::prose::prose;
use crate::replay::ReplayPanel;
use crate::sftp::panel::SftpPanel;
use crate::t;
use crate::terminal_panel::TerminalPanel;

/// 移到哪里。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum MoveTarget {
    /// 新开一个窗口；`at` 为放下的位置（屏幕坐标），没有时错开放在原来的窗口旁边
    NewWindow { at: Option<Point<Pixels>> },
    /// 已经打开的窗口
    Window(AnyWindowHandle),
}

/// 正在移动的 Tab：已从原来的停靠区取下，还没放进新窗口。
pub(crate) enum MovingTab {
    Terminal(Entity<TerminalPanel>),
    Sftp(Entity<SftpPanel>),
    Editor(Entity<EditorPanel>),
    Logs(Entity<LogPanel>),
    Replay(Entity<ReplayPanel>),
    Batch(Entity<BatchPanel>),
}

/// 移走的 Tab 用着的、可能要跟着它换窗口的东西。
enum Released {
    /// 终端的连接组：共用它的端口转发
    Terminal(String),
    /// 编辑器用着的 SFTP 会话
    Editor(SessionId),
    None,
}

impl MovingTab {
    fn released(&self, cx: &App) -> Released {
        match self {
            MovingTab::Terminal(p) => Released::Terminal(p.read(cx).connection_group().to_string()),
            MovingTab::Editor(p) => Released::Editor(p.read(cx).session_id()),
            MovingTab::Sftp(_)
            | MovingTab::Logs(_)
            | MovingTab::Replay(_)
            | MovingTab::Batch(_) => Released::None,
        }
    }

    fn entity_id(&self) -> EntityId {
        match self {
            MovingTab::Terminal(p) => p.entity_id(),
            MovingTab::Sftp(p) => p.entity_id(),
            MovingTab::Editor(p) => p.entity_id(),
            MovingTab::Logs(p) => p.entity_id(),
            MovingTab::Replay(p) => p.entity_id(),
            MovingTab::Batch(p) => p.entity_id(),
        }
    }

    /// 现在不能移的原因（终端、文件管理正在连接）。
    fn blocker(&self, cx: &App) -> Option<SharedString> {
        match self {
            MovingTab::Terminal(p) => p.read(cx).move_blocker(),
            MovingTab::Sftp(p) => p.read(cx).move_blocker(),
            MovingTab::Editor(_)
            | MovingTab::Logs(_)
            | MovingTab::Replay(_)
            | MovingTab::Batch(_) => None,
        }
    }
}

/// 窗口里只有一个 Tab 时拖动它的载体：停靠区不让拖动最后一个 Tab，也不接受这个载体，只能拖到另一个窗口上。
#[derive(Clone, Copy, Debug)]
pub(crate) struct SoleTabDrag {
    panel: PanelId,
}

impl SoleTabDrag {
    pub(crate) fn new(panel: PanelId) -> Self {
        Self { panel }
    }
}

/// 拖着 Tab 时指针到了窗口外：松开后会怎样。
#[derive(Clone, Debug, PartialEq)]
pub(super) struct DragOut {
    panel: PanelId,
    /// 指针在这个窗口中的位置（在窗口外）
    position: Point<Pixels>,
    /// 指针在屏幕上的位置
    screen: Point<Pixels>,
    target: DropTarget,
}

/// 拖到窗口外松开时的去处。
#[derive(Clone, Debug, PartialEq)]
enum DropTarget {
    /// 在放下的位置新开一个窗口
    NewWindow,
    /// 放在另一个 Termlet 窗口上：移到那个窗口
    Window(AnyWindowHandle, SharedString),
    /// 窗口里唯一的 Tab 放在了别处：不移（新开窗口没有意义）
    Nothing,
}

// ---------------------------------------------------------------- 打开着的主窗口

/// 打开着的主窗口（拖出窗口的 Tab 放在了哪个窗口上、“移到窗口 ›”列出哪些窗口）。
#[derive(Default)]
struct OpenWindows(Vec<OpenWindow>);

impl Global for OpenWindows {}

struct OpenWindow {
    handle: AnyWindowHandle,
    workspace: WeakEntity<Workspace>,
    /// 窗口在屏幕上的位置（移动、改大小后更新）
    bounds: Bounds<Pixels>,
    /// 最近一次到前台的先后（叠在一起时取最上面的；系统给不出窗口的前后次序时用）
    activated: u64,
}

impl OpenWindow {
    /// 窗口还开着时它的工作区（窗口关了而工作区还被别处拿着时不算）。
    fn live(&self, cx: &App) -> Option<Entity<Workspace>> {
        let open = cx.windows().contains(&self.handle);
        self.workspace.upgrade().filter(|_| open)
    }
}

/// 窗口到前台的计数。
static ACTIVATIONS: AtomicU64 = AtomicU64::new(0);

fn next_activation() -> u64 {
    ACTIVATIONS.fetch_add(1, Ordering::Relaxed) + 1
}

/// 登记新打开的主窗口（工作区建好时）。
pub(super) fn register(window: &Window, workspace: WeakEntity<Workspace>, cx: &mut App) {
    let open = cx.windows();
    let windows = &mut cx.default_global::<OpenWindows>().0;
    windows.retain(|w| w.workspace.upgrade().is_some() && open.contains(&w.handle));
    windows.push(OpenWindow {
        handle: window.window_handle(),
        workspace,
        bounds: window.bounds(),
        activated: next_activation(),
    });
}

/// 窗口移动、改了大小或到了前台：记下。
pub(super) fn note_window(window: &Window, cx: &mut App) {
    let handle = window.window_handle();
    let bounds = window.bounds();
    let active = window.is_window_active();
    if let Some(entry) = cx
        .default_global::<OpenWindows>()
        .0
        .iter_mut()
        .find(|w| w.handle == handle)
    {
        entry.bounds = bounds;
        if active {
            entry.activated = next_activation();
        }
    }
}

/// 这个窗口的工作区（还开着时）。
fn workspace_in(handle: AnyWindowHandle, cx: &App) -> Option<Entity<Workspace>> {
    cx.try_global::<OpenWindows>()?
        .0
        .iter()
        .find(|w| w.handle == handle)
        .and_then(|w| w.live(cx))
}

/// 除 `except` 以外打开着的主窗口，按打开的先后。
fn other_windows(except: AnyWindowHandle, cx: &App) -> Vec<(AnyWindowHandle, Entity<Workspace>)> {
    let Some(windows) = cx.try_global::<OpenWindows>() else {
        return Vec::new();
    };
    windows
        .0
        .iter()
        .filter(|w| w.handle != except)
        .filter_map(|w| w.live(cx).map(|ws| (w.handle, ws)))
        .collect()
}

/// 打开着的主窗口与它们的工作区，按打开的先后。
pub(super) fn open_workspaces(cx: &App) -> Vec<(AnyWindowHandle, Entity<Workspace>)> {
    cx.try_global::<OpenWindows>()
        .map(|windows| {
            windows
                .0
                .iter()
                .filter_map(|w| w.live(cx).map(|ws| (w.handle, ws)))
                .collect()
        })
        .unwrap_or_default()
}

/// 屏幕上 `screen` 处的另一个主窗口（叠在一起时取最上面的）。
fn window_at(
    screen: Point<Pixels>,
    except: AnyWindowHandle,
    cx: &App,
) -> Option<(AnyWindowHandle, Entity<Workspace>)> {
    let windows = cx.try_global::<OpenWindows>()?;
    let mut hits: Vec<&OpenWindow> = windows
        .0
        .iter()
        .filter(|w| w.handle != except && w.bounds.contains(&screen))
        .collect();
    // 系统给得出窗口的前后次序时按它（前面的在先），否则按最近到前台的
    match cx.window_stack() {
        Some(stack) => hits.sort_by_key(|w| {
            stack
                .iter()
                .position(|h| *h == w.handle)
                .unwrap_or(usize::MAX)
        }),
        None => hits.sort_by_key(|w| std::cmp::Reverse(w.activated)),
    }
    hits.into_iter().find_map(|w| {
        let workspace = w.live(cx)?;
        workspace
            .read(cx)
            .accepts_tabs()
            .then_some((w.handle, workspace))
    })
}

// ---------------------------------------------------------------- 工作区

impl Workspace {
    /// 中间区域的 Tab 数（各窗格合计）。
    pub(crate) fn tab_count(&self, cx: &App) -> usize {
        let mut groups = Vec::new();
        if let Some(tree) = self.dock_area.read(cx).layout(DockPlacement::Center) {
            collect_tab_groups(tree.root(), &mut groups);
        }
        groups.iter().map(|(panels, _)| panels.len()).sum()
    }

    /// 能不能放进 Tab：保险库与应用锁都开着。
    fn accepts_tabs(&self) -> bool {
        self.vault_gate.is_none() && self.lock_screen.is_none()
    }

    /// Tab 上的名称。
    fn tab_title(&self, panel: PanelId, cx: &App) -> Option<SharedString> {
        let is = |id: EntityId| PanelId::from(id) == panel;
        if let Some(p) = self
            .live_terminals()
            .into_iter()
            .find(|p| is(p.entity_id()))
        {
            return Some(p.read(cx).name().clone());
        }
        if let Some(p) = self.live_sftps().into_iter().find(|p| is(p.entity_id())) {
            return Some(p.read(cx).name().clone());
        }
        if let Some(p) = self.live_editors().into_iter().find(|p| is(p.entity_id())) {
            let path = p.read(cx).remote_path().to_string();
            return Some(termlet_core::sftp::path::base_name(&path).into());
        }
        if let Some(p) = self.live_logs().into_iter().find(|p| is(p.entity_id())) {
            return Some(p.read(cx).title_text());
        }
        if let Some(p) = self.live_batches().into_iter().find(|p| is(p.entity_id())) {
            return Some(p.read(cx).title_text(cx));
        }
        self.live_replays()
            .into_iter()
            .find(|p| is(p.entity_id()))
            .map(|p| p.read(cx).title_text())
    }

    /// 这个窗口在“移到窗口 ›”中的名称：第一个窗格中显示着的 Tab，有几个 Tab 时带上个数；没有 Tab 时是“空窗口”。
    pub(crate) fn window_label(&self, cx: &App) -> SharedString {
        let mut groups = Vec::new();
        if let Some(tree) = self.dock_area.read(cx).layout(DockPlacement::Center) {
            collect_tab_groups(tree.root(), &mut groups);
        }
        let count: usize = groups.iter().map(|(panels, _)| panels.len()).sum();
        let name = groups
            .first()
            .and_then(|(panels, active)| panels.get(*active).or(panels.first()))
            .and_then(|panel| self.tab_title(*panel, cx));
        match (name, count) {
            (Some(name), 1) => name,
            (Some(name), count) if count > 1 => {
                t!("tab_window.window_many", name = name, count = count)
            }
            _ => t!("tab_window.window_empty"),
        }
    }

    /// 这个 Tab 的面板（在这个窗口中时）。
    fn moving_tab(&self, panel: PanelId) -> Option<MovingTab> {
        let is = |id: EntityId| PanelId::from(id) == panel;
        if let Some(p) = self
            .live_terminals()
            .into_iter()
            .find(|p| is(p.entity_id()))
        {
            return Some(MovingTab::Terminal(p));
        }
        if let Some(p) = self.live_sftps().into_iter().find(|p| is(p.entity_id())) {
            return Some(MovingTab::Sftp(p));
        }
        if let Some(p) = self.live_editors().into_iter().find(|p| is(p.entity_id())) {
            return Some(MovingTab::Editor(p));
        }
        if let Some(p) = self.live_logs().into_iter().find(|p| is(p.entity_id())) {
            return Some(MovingTab::Logs(p));
        }
        if let Some(p) = self.live_batches().into_iter().find(|p| is(p.entity_id())) {
            return Some(MovingTab::Batch(p));
        }
        self.live_replays()
            .into_iter()
            .find(|p| is(p.entity_id()))
            .map(MovingTab::Replay)
    }

    /// 把 Tab 移到别的窗口（见模块说明）。窗口里只有这一个 Tab 时不移到新窗口（没有意义）；现在不能移时提示原因。
    pub(crate) fn move_tab(
        &mut self,
        panel: PanelId,
        target: MoveTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.moving_tab(panel) else {
            return;
        };
        if matches!(target, MoveTarget::NewWindow { .. }) && self.tab_count(cx) < 2 {
            crate::notify::warning(t!("tab_window.only_tab"), window, cx);
            return;
        }
        if let Some(reason) = tab.blocker(cx) {
            crate::notify::warning(reason, window, cx);
            return;
        }
        if let MoveTarget::Window(handle) = target {
            let ready = handle != window.window_handle()
                && workspace_in(handle, cx).is_some_and(|w| w.read(cx).accepts_tabs());
            if !ready {
                crate::notify::warning(t!("tab_window.window_gone"), window, cx);
                return;
            }
        }
        let bounds = match target {
            MoveTarget::NewWindow { at } => Some(new_window_bounds(window, at, cx)),
            MoveTarget::Window(_) => None,
        };
        tracing::info!(panel = ?panel, ?target, "Tab 移到别的窗口");
        self.detach_tab(&tab, window, cx);
        let source = (window.window_handle(), cx.entity().downgrade());
        // 这次事件（菜单的点击、拖动的结束）处理完再放进去
        cx.defer(move |cx| hand_over(tab, target, bounds, source, cx));
    }

    /// 把当前 Tab 移到新窗口（命令面板、快捷键）：焦点所在的窗格中显示着的 Tab。
    pub(super) fn move_current_tab_to_new_window(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((panels, active)) = self.current_tab_group(window, cx) else {
            return;
        };
        if let Some(panel) = panels.get(active).copied() {
            self.move_tab(panel, MoveTarget::NewWindow { at: None }, window, cx);
        }
    }

    /// 从这个窗口取下 Tab：不再观察它，同步输入、主机工具、最近用过的终端中去掉它，从停靠区移除（面板实体由调用方
    /// 拿着，不会关闭）。
    fn detach_tab(&mut self, tab: &MovingTab, window: &mut Window, cx: &mut Context<Self>) {
        let id = tab.entity_id();
        match tab {
            MovingTab::Terminal(panel) => {
                panel.update(cx, |p, cx| {
                    p.set_sync_member(false, cx);
                    p.set_shown(false, cx);
                });
                self.terminal_watch.remove(&id);
                self.terminals.retain(|p| p.entity_id() != id);
                self.recent_terminals.retain(|p| p.entity_id() != id);
                self.host_tools.update(cx, |tools, _| tools.forget(id));
                let panel = panel.clone();
                self.dock_area
                    .update(cx, |dock, cx| dock.remove_panel(panel, window, cx));
                self.prune_sync(window, cx);
                self.refresh_host_statuses(cx);
                self.sync_tools_target(window, cx);
            }
            MovingTab::Sftp(panel) => {
                self.sftp_watch.remove(&id);
                self.sftps.retain(|p| p.entity_id() != id);
                let panel = panel.clone();
                self.dock_area
                    .update(cx, |dock, cx| dock.remove_panel(panel, window, cx));
            }
            MovingTab::Editor(panel) => {
                self.editor_watch.remove(&id);
                self.editors.retain(|p| p.entity_id() != id);
                let panel = panel.clone();
                self.dock_area
                    .update(cx, |dock, cx| dock.remove_panel(panel, window, cx));
            }
            MovingTab::Logs(panel) => {
                self.log_watch.remove(&id);
                self.logs.retain(|p| p.entity_id() != id);
                let panel = panel.clone();
                self.dock_area
                    .update(cx, |dock, cx| dock.remove_panel(panel, window, cx));
            }
            MovingTab::Replay(panel) => {
                self.replay_watch.remove(&id);
                self.replays.retain(|p| p.entity_id() != id);
                let panel = panel.clone();
                self.dock_area
                    .update(cx, |dock, cx| dock.remove_panel(panel, window, cx));
            }
            MovingTab::Batch(panel) => {
                self.batch_watch.remove(&id);
                self.batches.retain(|p| p.entity_id() != id);
                let panel = panel.clone();
                self.dock_area
                    .update(cx, |dock, cx| dock.remove_panel(panel, window, cx));
            }
        }
        cx.notify();
    }

    /// Tab 已移到窗口 `to`（见模块说明）：共用它的连接的端口转发、它用着的 SFTP 会话，这个窗口里不再有别的 Tab 用时
    /// 改属新窗口。
    fn release_moved(&mut self, moved: &Released, to: &str, cx: &mut Context<Self>) {
        match moved {
            Released::Terminal(group) => {
                let still = self
                    .live_terminals()
                    .iter()
                    .any(|t| t.read(cx).connection_group() == group);
                if !still {
                    self.forwards.reassign_group(&self.owner, group, to);
                }
            }
            Released::Editor(session) => {
                if !self.uses_session(session, cx) {
                    tracing::debug!(session = %session, to, "编辑器的 SFTP 会话跟着 Tab 改属新窗口");
                    self.sftp.reassign_owner(session, to);
                }
            }
            Released::None => {}
        }
    }

    /// 这个窗口里的文件管理、编辑器用着这个 SFTP 会话。
    pub(super) fn uses_session(&self, session: &SessionId, cx: &App) -> bool {
        self.live_sftps()
            .iter()
            .any(|p| p.read(cx).session_id().as_ref() == Some(session))
            || self
                .live_editors()
                .iter()
                .any(|e| e.read(cx).session_id() == *session)
    }

    /// 放进别的窗口取下的 Tab：面板在这个窗口中重新登记订阅与任务，加进停靠区，切过去并让它获得焦点。
    fn adopt_tab(&mut self, tab: MovingTab, window: &mut Window, cx: &mut Context<Self>) {
        let prompter = self.prompter.clone();
        match tab {
            MovingTab::Terminal(panel) => {
                panel.update(cx, |p, cx| p.rebind_window(prompter, window, cx));
                self.add_terminal(panel.clone(), window, cx);
                self.show_terminal(&panel, window, cx);
            }
            MovingTab::Sftp(panel) => {
                let owner = self.owner.clone();
                panel.update(cx, |p, cx| p.rebind_window(owner, prompter, window, cx));
                self.watch_sftp(&panel, window, cx);
                self.add_to_dock(panel_handle(panel.clone()), window, cx);
                self.show_sftp(&panel, window, cx);
            }
            MovingTab::Editor(panel) => {
                panel.update(cx, |p, cx| p.rebind_window(window, cx));
                self.watch_editor(&panel, window, cx);
                self.add_to_dock(panel_handle(panel.clone()), window, cx);
                self.show_editor(&panel, window, cx);
            }
            MovingTab::Logs(panel) => {
                panel.update(cx, |p, cx| p.rebind_window(window, cx));
                self.watch_logs(&panel, window, cx);
                self.add_to_dock(panel_handle(panel.clone()), window, cx);
                self.show_logs(&panel, window, cx);
            }
            MovingTab::Replay(panel) => {
                panel.update(cx, |p, cx| p.rebind_window(window, cx));
                self.watch_replay(&panel, window, cx);
                self.add_to_dock(panel_handle(panel.clone()), window, cx);
                self.show_replay(&panel, window, cx);
            }
            MovingTab::Batch(panel) => {
                panel.update(cx, |p, cx| p.rebind_window(window, cx));
                self.watch_batch(&panel, window, cx);
                self.add_to_dock(panel_handle(panel.clone()), window, cx);
                self.show_batch(&panel, window, cx);
            }
        }
        cx.notify();
    }

    fn add_to_dock(
        &mut self,
        panel: std::sync::Arc<dyn BasePanelView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dock_area.update(cx, |dock, cx| {
            dock.add_panel_view(panel, DockPlacement::Center, None, window, cx)
        });
    }

    // ------------------------------------------------------------ 拖出窗口

    /// 拖着停靠区的 Tab 移动指针。
    pub(super) fn on_dock_tab_drag(
        &mut self,
        event: &DragMoveEvent<DragPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = event.dragged_item().downcast_ref::<DragPanel>() else {
            return;
        };
        self.track_drag_out(drag.panel(), false, event.event.position, window, cx);
    }

    /// 拖着窗口里唯一的 Tab 移动指针。
    pub(super) fn on_sole_tab_drag(
        &mut self,
        event: &DragMoveEvent<SoleTabDrag>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = event.dragged_item().downcast_ref::<SoleTabDrag>() else {
            return;
        };
        self.track_drag_out(drag.panel, true, event.event.position, window, cx);
    }

    /// 指针到了窗口外：记下松开后会怎样（窗口里贴着指针提示）；回到窗口里时清掉。
    fn track_drag_out(
        &mut self,
        panel: PanelId,
        sole: bool,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let viewport = Bounds::new(Point::default(), window.viewport_size());
        let out = (!viewport.contains(&position)).then(|| {
            let origin = window.bounds().origin;
            let screen = point(origin.x + position.x, origin.y + position.y);
            let target = match window_at(screen, window.window_handle(), cx) {
                Some((handle, workspace)) => {
                    DropTarget::Window(handle, workspace.read(cx).window_label(cx))
                }
                None if sole => DropTarget::Nothing,
                None => DropTarget::NewWindow,
            };
            DragOut {
                panel,
                position,
                screen,
                target,
            }
        });
        if out != self.tab_drag_out {
            self.tab_drag_out = out;
            cx.notify();
        }
    }

    /// 松开鼠标：拖到窗口外的 Tab 按记下的去处移动（这次事件处理完、拖动结束之后）。
    pub(super) fn on_tab_drag_end(
        &mut self,
        event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(out) = self.tab_drag_out.take() else {
            return;
        };
        cx.notify();
        let viewport = Bounds::new(Point::default(), window.viewport_size());
        if event.button != MouseButton::Left
            || viewport.contains(&event.position)
            || !cx.has_active_drag()
        {
            return;
        }
        let target = match out.target {
            DropTarget::NewWindow => MoveTarget::NewWindow {
                at: Some(out.screen),
            },
            DropTarget::Window(handle, _) => MoveTarget::Window(handle),
            DropTarget::Nothing => return,
        };
        tracing::debug!(panel = ?out.panel, ?target, "Tab 拖到了窗口外");
        cx.defer_in(window, move |this, window, cx| {
            this.move_tab(out.panel, target, window, cx)
        });
    }

    /// 拖到窗口外时的提示：在窗口里贴着指针离开的地方。拖动已经结束（例如在别处松开了）时不显示。
    pub(super) fn render_drag_out_hint(&self, window: &Window, cx: &App) -> Option<AnyElement> {
        let out = self
            .tab_drag_out
            .as_ref()
            .filter(|_| cx.has_active_drag())?;
        let text = match &out.target {
            DropTarget::NewWindow => t!("tab_window.drop_new"),
            DropTarget::Window(_, name) => t!("tab_window.drop_window", name = name.clone()),
            DropTarget::Nothing => t!("tab_window.drop_sole"),
        };
        let size = window.viewport_size();
        let anchor = point(
            clamp(out.position.x, Pixels::ZERO, size.width),
            clamp(out.position.y, Pixels::ZERO, size.height),
        );
        let margin = rems(0.5).to_pixels(window.rem_size());
        let theme = cx.theme();
        Some(
            deferred(
                anchored()
                    .position(anchor)
                    .snap_to_window_with_margin(margin)
                    .child(
                        h_flex()
                            .id("tab-drag-out-hint")
                            .test_support()
                            .max_w(rems(20.))
                            .gap_2()
                            .px_3()
                            .py_2()
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.popover)
                            .text_sm()
                            .text_color(theme.popover_foreground)
                            .shadow_md()
                            .child(
                                Icon::new(IconName::AppWindow)
                                    .small()
                                    .text_color(theme.muted_foreground),
                            )
                            .child(div().min_w_0().child(prose(text))),
                    ),
            )
            .with_priority(gpui_kit::base::POPUP_PRIORITY)
            .into_any_element(),
        )
    }
}

/// 新窗口的位置与所在的显示器：与原来的窗口一样大；放在 `at`（拖出窗口时放下的位置，指针落在新窗口的标题栏左边），
/// 移进那块屏幕的可见区域；没有 `at` 时错开放在原来的窗口旁边。
fn new_window_bounds(
    window: &Window,
    at: Option<Point<Pixels>>,
    cx: &App,
) -> (Bounds<Pixels>, Option<DisplayId>) {
    let current = window.window_bounds().get_bounds();
    let size = current.size;
    let here = window.display(cx);
    let Some(at) = at else {
        let offset = gpui_kit::px(32.);
        let origin = point(current.origin.x + offset, current.origin.y + offset);
        return (Bounds { origin, size }, here.map(|d| d.id()));
    };
    let rem = window.rem_size();
    let grab = point(rems(6.).to_pixels(rem), rems(1.).to_pixels(rem));
    let mut origin = point(at.x - grab.x, at.y - grab.y);
    let display = cx
        .displays()
        .into_iter()
        .find(|d| d.bounds().contains(&at))
        .or(here);
    if let Some(display) = &display {
        let visible = display.visible_bounds();
        let right = visible.origin.x + visible.size.width - size.width;
        let bottom = visible.origin.y + visible.size.height - size.height;
        origin.x = clamp(origin.x, visible.origin.x, right);
        origin.y = clamp(origin.y, visible.origin.y, bottom);
    }
    (Bounds { origin, size }, display.map(|d| d.id()))
}

/// 限制在 `min` 到 `max` 之间；`max` 比 `min` 小（窗口比屏幕还大）时取 `min`。
fn clamp(value: Pixels, min: Pixels, max: Pixels) -> Pixels {
    let value = if value > max { max } else { value };
    if value < min { min } else { value }
}

/// 把取下的 Tab 放进目标窗口（新窗口在这里打开），切过去并把那个窗口带到前台。放不进去时（目标窗口在这期间关了、
/// 新窗口打不开）放回原来的窗口。
fn hand_over(
    tab: MovingTab,
    target: MoveTarget,
    bounds: Option<(Bounds<Pixels>, Option<DisplayId>)>,
    source: (AnyWindowHandle, WeakEntity<Workspace>),
    cx: &mut App,
) {
    let destination = match (target, bounds) {
        (MoveTarget::Window(handle), _) => workspace_in(handle, cx).map(|w| (handle, w)),
        (MoveTarget::NewWindow { .. }, Some((bounds, display))) => {
            crate::cli_bridge::open_window_at(bounds, display, cx)
        }
        (MoveTarget::NewWindow { .. }, None) => None,
    };
    let released = tab.released(cx);
    let mut tab = Some(tab);
    if let Some((handle, workspace)) = destination {
        let _ = handle.update(cx, |_, window, cx| {
            if let Some(tab) = tab.take() {
                workspace.update(cx, |w, cx| w.adopt_tab(tab, window, cx));
            }
            window.activate_window();
        });
        if tab.is_none() {
            let to = workspace.read(cx).owner.clone();
            if let Some(from) = source.1.upgrade() {
                from.update(cx, |w, cx| w.release_moved(&released, &to, cx));
            }
        }
    }
    if let Some(tab) = tab.take() {
        tracing::warn!("Tab 没能放进目标窗口，放回原来的窗口");
        let (handle, workspace) = source;
        if let Some(workspace) = workspace.upgrade() {
            let _ = handle.update(cx, |_, window, cx| {
                workspace.update(cx, |w, cx| w.adopt_tab(tab, window, cx))
            });
        }
    }
}

/// Tab 菜单中的“移到新窗口”与“移到窗口 ›”（有别的窗口时），各种 Tab 共用（见 [`super::close_tab_items`]）。窗口
/// 里只有这一个 Tab 时“移到新窗口”不可用。
pub(crate) fn move_tab_items(
    menu: PopupMenu,
    workspace: &Entity<Workspace>,
    panel: PanelId,
    window: &mut Window,
    cx: &mut App,
) -> PopupMenu {
    let alone = workspace.read(cx).tab_count(cx) < 2;
    let move_to = |target: MoveTarget| {
        let workspace = workspace.downgrade();
        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            let _ = workspace.update(cx, |w, cx| w.move_tab(panel, target, window, cx));
        }
    };
    let menu = menu.separator().item(
        PopupMenuItem::new(t!("tab_window.move_to_new"))
            .disabled(alone)
            .on_click(move_to(MoveTarget::NewWindow { at: None })),
    );
    let others: Vec<(AnyWindowHandle, SharedString)> = other_windows(window.window_handle(), cx)
        .into_iter()
        .filter(|(_, w)| w.read(cx).accepts_tabs())
        .map(|(handle, w)| (handle, w.read(cx).window_label(cx)))
        .collect();
    if others.is_empty() {
        return menu;
    }
    let items: Vec<PopupMenuItem> = others
        .into_iter()
        .map(|(handle, label)| {
            PopupMenuItem::new(label).on_click(move_to(MoveTarget::Window(handle)))
        })
        .collect();
    let submenu = PopupMenu::build(window, cx, move |menu, _, _| {
        items.into_iter().fold(menu, |menu, item| menu.item(item))
    });
    menu.item(PopupMenuItem::submenu(
        t!("tab_window.move_to_window"),
        submenu,
    ))
}
