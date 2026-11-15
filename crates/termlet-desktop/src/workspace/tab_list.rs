//! 竖排的 Tab 列表（设置“通用”页的“Tab 的位置：左侧”）：主机侧栏右边独立的一列（侧栏收起时贴着窗口左边），列出这个
//! 窗口所有窗格的 Tab。
//!
//! - 停靠区照旧管理窗格与 Tab（布局、拖动分屏、保存为工作区都不变），只是窗格顶上不画横排的 Tab 栏，换成一条细标题
//!   （`dock_skin` 的 `TitleOnly`）。列表每帧从停靠区的布局读出各窗格的 Tab 与各窗格显示着哪个；每行画什么由各种 Tab
//!   自己给出（[`TabFace`]，与横排的 Tab 上的图标、名称、状态相同）。
//! - 一个窗格时不分组；几个窗格时按窗格分组（按窗格在窗口中的位置称“左侧窗格”等，说不清时按顺序编号），◀ 标出各窗格
//!   显示着的 Tab；焦点所在的窗格（焦点不在窗格里时是最近用过的）显示着的 Tab 高亮。
//! - 按主机分组（设置，默认关）：同一窗格中连同一台主机的两个以上的 Tab 归到可折叠的主机标题下，带数量；只影响显示。
//!   折叠按主机记在本机的界面状态中（与主机工具记页相同的键），各窗口、各窗格共用，重启后照旧。
//! - 点击切换并获得焦点，中键关闭，右键是与横排相同的 Tab 菜单，悬停时行尾有关闭按钮；拖动排序（放在一行的上半 / 下半
//!   插到它前面 / 后面），拖到另一窗格的组里（行或窗格的标题）；拖到窗格的内容区分屏、拖出窗口（`move_tab.rs`）与拖横排
//!   的 Tab 相同。
//! - 键盘（列表有焦点时：点列表的空白处，或“焦点进 Tab 列表”，可自定义快捷键、默认没有，命令面板中有）：↑ ↓、Home /
//!   End 移动当前行，Enter 切到那个 Tab（主机标题折叠 / 展开），Delete（macOS 上另有 ⌘⌫）关闭（照常确认），← → 折叠 /
//!   展开主机标题（在主机下的 Tab 上按 ← 回到主机标题），Shift+F10 或菜单键在当前行旁边打开右键菜单，Esc 回到终端。
//!   列表有焦点时当前行有一圈焦点框（与文件管理、主机工具的列表相同）。
//! - 宽度拖动右边缘调整并记住（本机的界面状态）；标题行的“⇤”收成只显示图标的窄栏（记住），窗口窄得放不下展开的列表
//!   时也自动用窄栏（不改记下的状态）。窄栏的图标比列表中的大一号（随界面大小缩放），图标角上的圆点是这个 Tab 最要紧的
//!   状态（与列表中的同色），悬停提示写明全部状态。窄栏上悬停时在它右边浮出展开的列表（不盖住窄栏，窄栏顶上的“⇥”照样
//!   能点），盖在终端上，不改终端的宽度；列表中的右键菜单开着、列表有焦点时不收起。

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dock::{
    BasePanelView, DockPlacement, DragPanel, InsertTarget, NodeId, PaneNode, PaneRef, PanelHandle,
    PanelId,
};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _, Size, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Axis, ClickEvent, Context, DismissEvent, DragMoveEvent,
    Entity, FocusHandle, Focusable as _, Hsla, InteractiveElement as _, IntoElement, KeyBinding,
    KeyDownEvent, MouseButton, OperatingSystem, ParentElement as _, Pixels, Render, ScrollHandle,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, TestSupportExt as _,
    Window, actions, anchored, deferred, div, prelude::FluentBuilder as _, px, rems,
};
use termlet_core::model::TabPosition;

use super::{CONTENT_MIN, KEY_CONTEXT, NewLocalTerminal, SoleTabDrag, Workspace};
use crate::app_state::AppState;
use crate::sftp::panel::SftpTarget;
use crate::t;

actions!(
    tab_list,
    [
        /// 当前行移到上一行 / 下一行 / 第一行 / 最后一行
        SelectPrev,
        SelectNext,
        SelectFirst,
        SelectLast,
        /// 切到当前行的 Tab 并让它获得焦点；当前行是主机标题时折叠 / 展开
        Activate,
        /// 关闭当前行的 Tab（照常确认）
        CloseItem,
        /// 折叠当前行的主机标题；在主机下的 Tab 上时移到它的主机标题
        Collapse,
        /// 展开当前行的主机标题；已展开时移到其中第一个 Tab
        Expand,
        /// 在当前行旁边打开右键菜单
        OpenMenu,
        /// 焦点回到终端（焦点所在的窗格显示着的 Tab）
        FocusTab,
    ]
);

/// 列表有焦点时的上下文。
pub(crate) const LIST_CONTEXT: &str = "TabList";

/// 列表的快捷键（固定的，设置的“快捷键”页中列出，不能修改）：`os` 为按哪个系统的习惯，macOS 上照 Finder 另有 ⌘⌫ 关闭。
pub(crate) fn key_bindings(os: OperatingSystem) -> Vec<KeyBinding> {
    let list = Some(LIST_CONTEXT);
    let mut bindings = vec![
        KeyBinding::new("up", SelectPrev, list),
        KeyBinding::new("down", SelectNext, list),
        KeyBinding::new("home", SelectFirst, list),
        KeyBinding::new("end", SelectLast, list),
        KeyBinding::new("enter", Activate, list),
        KeyBinding::new("delete", CloseItem, list),
        KeyBinding::new("left", Collapse, list),
        KeyBinding::new("right", Expand, list),
        KeyBinding::new("shift-f10", OpenMenu, list),
        KeyBinding::new("menu", OpenMenu, list),
        KeyBinding::new("escape", FocusTab, list),
    ];
    if os == OperatingSystem::Mac {
        bindings.push(KeyBinding::new("cmd-backspace", CloseItem, list));
    }
    bindings
}

/// 展开的 Tab 列表的宽度（100% 界面大小时；记下的宽度也按 100%，用时乘上界面的缩放倍数）。
const WIDTH_DEFAULT: f32 = 208.;
const WIDTH_MIN: f32 = 144.;
const WIDTH_MAX: f32 = 400.;
/// 拖动条获得焦点后按一次方向键调整的宽度。
const WIDTH_STEP: f32 = 16.;
/// 窄栏的宽度（rem）。
const RAIL_WIDTH: f32 = 2.5;
/// 窄栏上每个图标占的方块（rem）。
const RAIL_CELL: f32 = 2.;
/// 窄栏上图标的大小：比列表中的（`xsmall`）大一号。gpui-kit 的图标按 rem 定大小，随界面大小缩放。
pub(crate) const RAIL_ICON: Size = Size::Medium;
/// 一行的高度（rem）。
const ROW_HEIGHT: f32 = 1.75;
/// 浮出的列表画在终端之上、菜单之下（菜单的优先级见 `gpui_kit::base::POPUP_PRIORITY`）。
const OVERLAY_PRIORITY: usize = 1;

/// 竖排的 Tab 列表中一行画什么：与横排的 Tab 上相同的图标、名称与状态（各种 Tab 自己给出，见各面板的 `tab_face`）。
pub(crate) struct TabFace {
    /// 名称前的图标
    pub icon: AnyElement,
    /// 窄栏上的图标（[`RAIL_ICON`] 大小）
    pub rail_icon: AnyElement,
    /// Tab 上的名称
    pub name: SharedString,
    /// 悬停时名称下面的说明（程序设置的标题、日志的来源、文件的路径等）
    pub detail: Option<SharedString>,
    /// 名称后的状态（连接中 / 出错的圆点、新输出、铃声、记日志、录制中、同步输入等）
    pub badges: Vec<AnyElement>,
    /// 这些状态的颜色与说明，最要紧的在前：窄栏上图标角上画第一个，悬停提示中逐条写明
    pub status: Vec<TabStatus>,
    /// 按主机分组时归在哪台主机下：键与标题上写的主机名；不连主机的（本地终端、回放）为 `None`
    pub host: Option<(SharedString, SharedString)>,
}

/// Tab 的一条状态：与列表中这一行名称后的状态同义、同色。
pub(crate) struct TabStatus {
    pub color: Hsla,
    pub label: SharedString,
    /// 画成铃铛（与列表中的响铃相同）；否则是圆点
    pub bell: bool,
}

impl TabStatus {
    pub fn dot(color: Hsla, label: SharedString) -> Self {
        Self {
            color,
            label,
            bell: false,
        }
    }
}

/// 按主机分组的键：与文件管理的会话认同一台主机的方式相同（[`SftpTarget::key`]），以 root 身份打开的也算这台主机。
pub(crate) fn host_group_key(target: &SftpTarget) -> String {
    let key = target.key();
    match key.strip_suffix("#root") {
        Some(host) => host.to_string(),
        None => key,
    }
}

/// 拖着 Tab 经过一行时放在它前面还是后面。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Place {
    Before,
    After,
}

/// 键盘操作的当前行：一个 Tab，或（按主机分组时）一个主机标题（窗格与主机的键）。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Cursor {
    Tab(PanelId),
    Host(NodeId, SharedString),
}

/// 竖排的 Tab 列表的状态（这个窗口的）。
pub(super) struct TabListState {
    /// 宽度的拖动条（获得焦点后可以用方向键调整宽度）
    resize_focus: FocusHandle,
    /// 列表（有焦点时可以用键盘逐行操作）
    focus: FocusHandle,
    scroll: ScrollHandle,
    rail_scroll: ScrollHandle,
    /// 列表在窗口中的左边缘（拖动调整宽度时换算）
    left: f32,
    /// 指针在窄栏上 / 在浮出的列表上
    rail_hovered: bool,
    overlay_hovered: bool,
    /// 最近一次从列表中打开的右键菜单（菜单开着时浮出的列表不收起）
    menu: Rc<RefCell<Option<FocusHandle>>>,
    /// 用键盘（Shift+F10、菜单键）打开的右键菜单：放在当前行的下面
    key_menu: Option<(Entity<PopupMenu>, Subscription)>,
    /// 键盘操作的当前行，与它上次在列表中的位置（那一行不在了时按位置就近）
    cursor: Option<Cursor>,
    cursor_ix: usize,
    /// 拖着 Tab 经过的行与放在它前面还是后面
    drop: Option<(PanelId, Place)>,
    /// 上次滚进视野的高亮的 Tab
    revealed: Option<PanelId>,
}

impl TabListState {
    pub(super) fn new(resize_focus: FocusHandle, focus: FocusHandle) -> Self {
        Self {
            resize_focus,
            focus,
            scroll: ScrollHandle::default(),
            rail_scroll: ScrollHandle::default(),
            left: 0.,
            rail_hovered: false,
            overlay_hovered: false,
            menu: Rc::default(),
            key_menu: None,
            cursor: None,
            cursor_ix: 0,
            drop: None,
            revealed: None,
        }
    }
}

/// 拖动 Tab 列表的右边缘调整宽度。
pub(super) struct TabListResize;

impl Render for TabListResize {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// 一个窗格：其中的 Tab 与显示着的是第几个。
struct Pane {
    node: NodeId,
    tabs: Vec<PanelId>,
    active: usize,
}

/// 布局树中的全部窗格（按布局的顺序）。
fn collect_panes(node: &PaneNode, out: &mut Vec<Pane>) {
    match node.kind() {
        PaneRef::Tabs { panels, active_ix } if !panels.is_empty() => out.push(Pane {
            node: node.id(),
            tabs: panels.to_vec(),
            active: active_ix.min(panels.len() - 1),
        }),
        PaneRef::Tabs { .. } => {}
        PaneRef::Split { children, .. } => {
            for child in children {
                collect_panes(child, out);
            }
        }
    }
}

/// 窗格在窗口中占的位置（以整个中间区域为 1）。
#[derive(Clone, Copy)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

fn pane_rects(node: &PaneNode, rect: Rect, out: &mut Vec<(NodeId, Rect)>) {
    match node.kind() {
        PaneRef::Tabs { .. } => out.push((node.id(), rect)),
        PaneRef::Split {
            axis,
            children,
            sizes,
        } => {
            // 有记下的大小时按它分，没有时平分
            let measured: Option<Vec<f32>> = sizes.iter().map(|s| s.map(f32::from)).collect();
            let weights: Vec<f32> = match measured {
                Some(sizes) if sizes.len() == children.len() => {
                    sizes.into_iter().map(|s| s.max(1.)).collect()
                }
                _ => vec![1.; children.len()],
            };
            let total = weights.iter().sum::<f32>().max(f32::EPSILON);
            let mut offset = 0.;
            for (child, weight) in children.iter().zip(weights) {
                let share = weight / total;
                let part = match axis {
                    Axis::Horizontal => Rect {
                        x: rect.x + rect.w * offset,
                        w: rect.w * share,
                        ..rect
                    },
                    Axis::Vertical => Rect {
                        y: rect.y + rect.h * offset,
                        h: rect.h * share,
                        ..rect
                    },
                };
                pane_rects(child, part, out);
                offset += share;
            }
        }
    }
}

/// 窗格在一个方向上的位置：靠前、居中、靠后，或占满。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Third {
    Start,
    Middle,
    End,
    Whole,
}

fn third(start: f32, len: f32) -> Third {
    if len > 0.99 {
        return Third::Whole;
    }
    let center = start + len / 2.;
    if center < 0.4 {
        Third::Start
    } else if center > 0.6 {
        Third::End
    } else {
        Third::Middle
    }
}

/// 按位置给窗格起的名字；说不清的为 `None`。
fn position_name(horizontal: Third, vertical: Third) -> Option<SharedString> {
    use Third::*;
    Some(match (horizontal, vertical) {
        (Start, Whole) => t!("tab_list.pane.left"),
        (End, Whole) => t!("tab_list.pane.right"),
        (Middle, Whole) | (Whole, Middle) => t!("tab_list.pane.middle"),
        (Whole, Start) => t!("tab_list.pane.top"),
        (Whole, End) => t!("tab_list.pane.bottom"),
        (Start, Start) => t!("tab_list.pane.top_left"),
        (End, Start) => t!("tab_list.pane.top_right"),
        (Start, End) => t!("tab_list.pane.bottom_left"),
        (End, End) => t!("tab_list.pane.bottom_right"),
        _ => return None,
    })
}

/// 各窗格的名字：按位置（“左侧窗格”“右上窗格”等）；有说不清或重名的时全部按顺序编号（“窗格 1”等）。
fn pane_names(root: &PaneNode) -> HashMap<NodeId, SharedString> {
    let mut rects = Vec::new();
    pane_rects(
        root,
        Rect {
            x: 0.,
            y: 0.,
            w: 1.,
            h: 1.,
        },
        &mut rects,
    );
    let named: Vec<Option<SharedString>> = rects
        .iter()
        .map(|(_, r)| position_name(third(r.x, r.w), third(r.y, r.h)))
        .collect();
    let distinct = named.iter().all(Option::is_some)
        && named.iter().flatten().collect::<HashSet<_>>().len() == named.len();
    rects
        .iter()
        .zip(named)
        .enumerate()
        .map(|(ix, ((node, _), name))| {
            let name = match name {
                Some(name) if distinct => name,
                _ => t!("tab_list.pane.numbered", n = ix + 1),
            };
            (*node, name)
        })
        .collect()
}

/// 列表中的一项。
enum Entry {
    /// 窗格的标题（几个窗格时）
    Pane { node: NodeId, label: SharedString },
    /// 主机的标题（按主机分组时）
    Host {
        node: NodeId,
        key: SharedString,
        label: SharedString,
        /// 其中的 Tab（按窗格中的顺序）
        members: Vec<PanelId>,
        collapsed: bool,
        /// 窗格显示着的 Tab 在其中
        shown: bool,
        /// 高亮的 Tab 在其中
        highlighted: bool,
    },
    /// 一个 Tab
    Tab {
        panel: PanelId,
        node: NodeId,
        /// 在主机标题下
        nested: bool,
        /// 窗格显示着它
        shown: bool,
    },
}

/// 这一帧的列表。
struct Listing {
    panes: Vec<Pane>,
    entries: Vec<Entry>,
    faces: HashMap<PanelId, TabFace>,
    views: HashMap<PanelId, Arc<dyn BasePanelView>>,
    /// 几个窗格
    multi: bool,
    /// 窗口里只有一个 Tab（停靠区不让拖动它，换成只能拖到另一个窗口上的载体）
    sole: bool,
    highlighted: Option<PanelId>,
    /// 列表中打开的右键菜单开着（这时各行不显示悬停提示，免得盖住菜单）
    menu_open: bool,
    /// 画焦点框的一行（列表有焦点时的当前行）
    ring: Option<usize>,
    /// 用键盘打开的右键菜单所在的一层（放在焦点框那一行的下面）
    key_menu: Option<AnyElement>,
}

/// 列表中的一项可以是键盘操作的当前行时，它是哪一行（窗格的标题不能）。
fn entry_cursor(entry: &Entry) -> Option<Cursor> {
    match entry {
        Entry::Tab { panel, .. } => Some(Cursor::Tab(*panel)),
        Entry::Host { node, key, .. } => Some(Cursor::Host(*node, key.clone())),
        Entry::Pane { .. } => None,
    }
}

/// 当前行往哪里移。
#[derive(Clone, Copy)]
enum Step {
    Prev,
    Next,
    First,
    Last,
}

impl Workspace {
    /// Tab 放在左侧。
    pub(crate) fn tabs_on_left(cx: &App) -> bool {
        AppState::store(cx).local_settings().window.tab_position == TabPosition::Left
    }

    /// 切换 Tab 的位置（命令面板、列表标题的右键菜单）：顶部 ↔ 左侧，存进本机设置。
    pub(crate) fn toggle_tab_position(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = if Self::tabs_on_left(cx) {
            TabPosition::Top
        } else {
            TabPosition::Left
        };
        tracing::info!(position = ?next, "切换 Tab 的位置");
        let result = AppState::store(cx).update_local_settings(|s| s.window.tab_position = next);
        if let Err(e) = result {
            tracing::error!("保存设置失败：{e}");
            crate::notify::error(
                t!("settings.save_failed", error = e.to_string()),
                window,
                cx,
            );
        }
        cx.notify();
    }

    /// 打开 / 关闭按主机分组（列表标题的右键菜单）。
    fn toggle_group_by_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = AppState::store(cx);
        let on = !store.local_settings().window.group_tabs_by_host;
        tracing::debug!(on, "按主机分组");
        if let Err(e) = store.update_local_settings(|s| s.window.group_tabs_by_host = on) {
            tracing::error!("保存设置失败：{e}");
            crate::notify::error(
                t!("settings.save_failed", error = e.to_string()),
                window,
                cx,
            );
        }
        cx.notify();
    }

    fn set_tab_list_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        tracing::debug!(collapsed, "收起 / 展开 Tab 列表");
        AppState::ui(cx).update(cx, |ui, cx| {
            ui.update(|s| s.tab_list_collapsed = collapsed, cx)
        });
        self.tab_list.rail_hovered = false;
        self.tab_list.overlay_hovered = false;
    }

    /// 每个 Tab 的一行画什么。
    fn tab_faces(&self, cx: &mut Context<Self>) -> HashMap<PanelId, TabFace> {
        let mut faces = HashMap::new();
        for p in self.live_terminals() {
            let face = p.update(cx, |p, cx| p.tab_face(cx));
            faces.insert(PanelId::from(p.entity_id()), face);
        }
        for p in self.live_sftps() {
            faces.insert(PanelId::from(p.entity_id()), p.read(cx).tab_face(cx));
        }
        for p in self.live_editors() {
            faces.insert(PanelId::from(p.entity_id()), p.read(cx).tab_face(cx));
        }
        for p in self.live_logs() {
            faces.insert(PanelId::from(p.entity_id()), p.read(cx).tab_face(cx));
        }
        for p in self.live_replays() {
            faces.insert(PanelId::from(p.entity_id()), p.read(cx).tab_face(cx));
        }
        faces
    }

    /// 高亮的 Tab：焦点所在的窗格（焦点不在窗格里时是最近用过的终端所在的，再没有时是第一个）显示着的 Tab。
    fn highlighted_tab(&self, window: &Window, cx: &App) -> Option<PanelId> {
        self.current_tab_group(window, cx)
            .and_then(|(panels, active)| panels.get(active).copied())
    }

    /// 这一帧的列表。
    fn tab_listing(
        &mut self,
        group_by_host: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Listing {
        let mut panes = Vec::new();
        let mut names = HashMap::new();
        let mut views = HashMap::new();
        {
            let dock = self.dock_area.read(cx);
            if let Some(tree) = dock.layout(DockPlacement::Center) {
                collect_panes(tree.root(), &mut panes);
                if panes.len() > 1 {
                    names = pane_names(tree.root());
                }
                for id in tree.panels() {
                    if let Some(view) = dock.panel(id) {
                        views.insert(id, view.clone());
                    }
                }
            }
        }
        let highlighted = self.highlighted_tab(window, cx);
        let menu_open = self.tab_list_menu_open(window, cx);
        let mut faces = self.tab_faces(cx);
        // 不是这几种 Tab 的（不会有）：用面板的名字
        for (id, view) in &views {
            faces.entry(*id).or_insert_with(|| TabFace {
                icon: Icon::new(IconName::File).xsmall().into_any_element(),
                rail_icon: Icon::new(IconName::File)
                    .with_size(RAIL_ICON)
                    .into_any_element(),
                name: SharedString::from(view.panel_name(cx)),
                detail: None,
                badges: Vec::new(),
                status: Vec::new(),
                host: None,
            });
        }
        let multi = panes.len() > 1;
        let total: usize = panes.iter().map(|p| p.tabs.len()).sum();
        let ui = AppState::global(cx).ui.read(cx).state();
        let mut entries = Vec::new();
        for pane in &panes {
            let node = pane.node;
            let shown_tab = pane.tabs.get(pane.active).copied();
            if multi {
                entries.push(Entry::Pane {
                    node,
                    label: names.get(&node).cloned().unwrap_or_default(),
                });
            }
            let host_of = |id: &PanelId| {
                faces
                    .get(id)
                    .and_then(|f| f.host.clone())
                    .filter(|_| group_by_host)
            };
            let mut counts: HashMap<SharedString, usize> = HashMap::new();
            for tab in &pane.tabs {
                if let Some((key, _)) = host_of(tab) {
                    *counts.entry(key).or_default() += 1;
                }
            }
            let mut done = HashSet::new();
            for tab in &pane.tabs {
                let group = host_of(tab).filter(|(key, _)| counts.get(key).copied() >= Some(2));
                let Some((key, label)) = group else {
                    entries.push(Entry::Tab {
                        panel: *tab,
                        node,
                        nested: false,
                        shown: shown_tab == Some(*tab),
                    });
                    continue;
                };
                if !done.insert(key.clone()) {
                    continue;
                }
                let members: Vec<PanelId> = pane
                    .tabs
                    .iter()
                    .filter(|t| host_of(t).is_some_and(|(k, _)| k == key))
                    .copied()
                    .collect();
                let collapsed = ui.tab_host_collapsed(&key);
                entries.push(Entry::Host {
                    node,
                    key,
                    label,
                    members: members.clone(),
                    collapsed,
                    shown: shown_tab.is_some_and(|s| members.contains(&s)),
                    highlighted: highlighted.is_some_and(|h| members.contains(&h)),
                });
                if !collapsed {
                    entries.extend(members.into_iter().map(|panel| Entry::Tab {
                        panel,
                        node,
                        nested: true,
                        shown: shown_tab == Some(panel),
                    }));
                }
            }
        }
        Listing {
            panes,
            entries,
            faces,
            views,
            multi,
            sole: total == 1,
            highlighted,
            menu_open,
            ring: None,
            key_menu: None,
        }
    }

    /// 从列表中打开的右键菜单还开着（焦点在菜单里）。
    fn tab_list_menu_open(&self, window: &Window, cx: &App) -> bool {
        self.tab_list
            .menu
            .borrow()
            .as_ref()
            .is_some_and(|f| f.contains_focused(window, cx))
    }

    // ------------------------------------------------------------ 宽度

    /// 展开的列表的宽度范围（像素）：最小宽度到最大宽度，并给主区域留出空间（窗口太窄时以最小宽度为准）。
    /// `sidebar` 为左边主机侧栏占的宽度。
    fn tab_list_range(&self, sidebar: f32, window: &Window) -> (f32, f32) {
        let scale = crate::ui_scale::factor(window);
        let min = WIDTH_MIN * scale;
        let window_width = f32::from(window.viewport_size().width);
        let max = (WIDTH_MAX * scale)
            .min(window_width - sidebar - CONTENT_MIN * scale)
            .max(min);
        (min, max)
    }

    /// 展开的列表的宽度：记下的宽度按界面大小缩放，限制在 [`Self::tab_list_range`] 之内。
    fn tab_list_width(&self, sidebar: f32, window: &Window, cx: &App) -> Pixels {
        let saved = AppState::global(cx)
            .ui
            .read(cx)
            .state()
            .tab_list_width
            .unwrap_or(WIDTH_DEFAULT);
        let (min, max) = self.tab_list_range(sidebar, window);
        px((saved * crate::ui_scale::factor(window))
            .clamp(min, max)
            .round())
    }

    /// 窄栏上浮出的列表的宽度：盖在终端上、不占布局，不必给主区域留空间，按记下的宽度，只是不超出窗口。
    fn tab_overlay_width(&self, sidebar: f32, window: &Window, cx: &App) -> Pixels {
        let scale = crate::ui_scale::factor(window);
        let saved = AppState::global(cx)
            .ui
            .read(cx)
            .state()
            .tab_list_width
            .unwrap_or(WIDTH_DEFAULT);
        // 浮在窄栏的右边
        let rail = rems(RAIL_WIDTH).to_pixels(window.rem_size());
        let room = f32::from(window.viewport_size().width) - sidebar - f32::from(rail);
        px((saved * scale)
            .clamp(WIDTH_MIN * scale, WIDTH_MAX * scale)
            .min(room)
            .round())
    }

    /// 窗口窄得放不下展开的列表（最窄时也给主区域留不出 [`CONTENT_MIN`]）：自动用窄栏。
    fn tab_list_squeezed(&self, sidebar: f32, window: &Window) -> bool {
        let scale = crate::ui_scale::factor(window);
        let window_width = f32::from(window.viewport_size().width);
        window_width - sidebar - WIDTH_MIN * scale < CONTENT_MIN * scale
    }

    /// 记下展开的列表的宽度（像素，换算成 100% 的界面大小时的宽度）。
    fn save_tab_list_width(&self, width: f32, window: &Window, cx: &mut Context<Self>) {
        let width = (width / crate::ui_scale::factor(window)).round();
        AppState::ui(cx).update(cx, |ui, cx| {
            ui.update(|s| s.tab_list_width = Some(width), cx)
        });
    }

    pub(super) fn on_tab_list_resize(
        &mut self,
        event: &DragMoveEvent<TabListResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let left = self.tab_list.left;
        let (min, max) = self.tab_list_range(left, window);
        let width = (f32::from(event.event.position.x) - left).clamp(min, max);
        self.save_tab_list_width(width, window, cx);
    }

    /// 拖动条获得焦点时：← 变窄、→ 变宽。
    fn on_tab_list_resize_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let step = match event.keystroke.key.as_str() {
            "left" => -WIDTH_STEP,
            "right" => WIDTH_STEP,
            _ => return,
        };
        let left = self.tab_list.left;
        let (min, max) = self.tab_list_range(left, window);
        let step = step * crate::ui_scale::factor(window);
        let width = (f32::from(self.tab_list_width(left, window, cx)) + step).clamp(min, max);
        self.save_tab_list_width(width, window, cx);
        cx.stop_propagation();
    }

    // ------------------------------------------------------------ 操作

    /// 切到这个 Tab 并让它获得焦点（点了列表中的一行）。
    fn show_listed_tab(&mut self, panel: PanelId, window: &mut Window, cx: &mut Context<Self>) {
        // 键盘操作的当前行跟着走：之后焦点进列表时从这个 Tab 接着移动
        self.tab_list.cursor = Some(Cursor::Tab(panel));
        let is = |id: gpui_kit::EntityId| PanelId::from(id) == panel;
        if let Some(p) = self
            .live_terminals()
            .into_iter()
            .find(|p| is(p.entity_id()))
        {
            self.show_terminal(&p, window, cx);
        } else if let Some(p) = self.live_sftps().into_iter().find(|p| is(p.entity_id())) {
            self.show_sftp(&p, window, cx);
        } else if let Some(p) = self.live_editors().into_iter().find(|p| is(p.entity_id())) {
            self.show_editor(&p, window, cx);
        } else if let Some(p) = self.live_logs().into_iter().find(|p| is(p.entity_id())) {
            self.show_logs(&p, window, cx);
        } else if let Some(p) = self.live_replays().into_iter().find(|p| is(p.entity_id())) {
            self.show_replay(&p, window, cx);
        } else {
            self.select_tab_panel(panel, window, cx);
        }
        cx.notify();
    }

    /// 拖动后显示移过去的 Tab 并让它获得焦点。停靠区在下一轮才告诉各窗格的面板谁显示着（原来的窗格新显示的终端会拿走
    /// 焦点），之后再切一次。
    fn show_moved_tab(&mut self, panel: PanelId, window: &mut Window, cx: &mut Context<Self>) {
        self.show_listed_tab(panel, window, cx);
        cx.spawn_in(window, async move |this, cx| {
            let _ = this.update_in(cx, |this, window, cx| {
                this.show_listed_tab(panel, window, cx)
            });
        })
        .detach();
    }

    /// 拖着 Tab 经过一行：指针在上半时放在它前面，下半时放在它后面。
    fn on_row_drag_move(
        &mut self,
        panel: PanelId,
        event: &DragMoveEvent<DragPanel>,
        cx: &mut Context<Self>,
    ) {
        if !event.bounds.contains(&event.event.position) {
            return;
        }
        let height = f32::from(event.bounds.size.height).max(1.);
        let y = f32::from(event.event.position.y - event.bounds.origin.y) / height;
        let place = if y < 0.5 { Place::Before } else { Place::After };
        if self.tab_list.drop != Some((panel, place)) {
            self.tab_list.drop = Some((panel, place));
            cx.notify();
        }
    }

    /// `panel` 所在的窗格与其中的 Tab。
    fn pane_of(&self, panel: PanelId, cx: &App) -> Option<(NodeId, Vec<PanelId>)> {
        let tree = self.dock_area.read(cx).layout(DockPlacement::Center)?;
        let node = tree.find_panel_node(panel)?;
        match tree.find_node(node)?.kind() {
            PaneRef::Tabs { panels, .. } => Some((node, panels.to_vec())),
            PaneRef::Split { .. } => None,
        }
    }

    /// 拖着的 Tab 放在一行上：插到它前面或后面（可以是另一个窗格），显示它并让它获得焦点。
    fn drop_on_row(
        &mut self,
        drag: &DragPanel,
        target: PanelId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let place = self
            .tab_list
            .drop
            .take()
            .filter(|(panel, _)| *panel == target)
            .map_or(Place::Before, |(_, place)| place);
        let moving = drag.panel();
        cx.notify();
        if moving == target {
            return;
        }
        let Some((node, panels)) = self.pane_of(target, cx) else {
            return;
        };
        let Some(at) = panels.iter().position(|p| *p == target) else {
            return;
        };
        let mut ix = at + usize::from(place == Place::After);
        // 同一个窗格中往后挪：先取下它，后面的位置前移一格
        if panels
            .iter()
            .position(|p| *p == moving)
            .is_some_and(|from| from < ix)
        {
            ix -= 1;
        }
        tracing::debug!(?moving, ?target, ?place, "在 Tab 列表中拖动 Tab");
        let insert = InsertTarget::Tabs {
            node,
            ix: Some(ix),
            activate: true,
        };
        self.dock_area
            .update(cx, |dock, cx| dock.move_panel(moving, insert, window, cx));
        self.show_moved_tab(moving, window, cx);
    }

    /// 拖着的 Tab 放在窗格的标题上：放到这个窗格的最后。
    fn drop_on_pane(
        &mut self,
        drag: &DragPanel,
        node: NodeId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.tab_list.drop = None;
        let moving = drag.panel();
        tracing::debug!(?moving, "Tab 拖到了列表中的窗格标题上");
        let insert = InsertTarget::Tabs {
            node,
            ix: None,
            activate: true,
        };
        self.dock_area
            .update(cx, |dock, cx| dock.move_panel(moving, insert, window, cx));
        self.show_moved_tab(moving, window, cx);
    }

    /// 折叠 / 展开一台主机的分组：记在本机的界面状态中（各窗口、各窗格共用，重启后照旧）。
    fn set_host_group_collapsed(&mut self, key: &str, collapsed: bool, cx: &mut Context<Self>) {
        tracing::debug!(collapsed, "折叠 / 展开 Tab 列表中的主机分组");
        AppState::ui(cx).update(cx, |ui, cx| {
            ui.update(|s| s.set_tab_host_collapsed(key, collapsed), cx)
        });
        cx.notify();
    }

    fn toggle_host_group(&mut self, key: &str, cx: &mut Context<Self>) {
        let collapsed = AppState::global(cx)
            .ui
            .read(cx)
            .state()
            .tab_host_collapsed(key);
        self.set_host_group_collapsed(key, !collapsed, cx);
    }

    // ------------------------------------------------------------ 键盘

    /// 焦点进 Tab 列表（“焦点进 Tab 列表”的快捷键、命令面板），当前行从高亮的 Tab 开始；Tab 放在顶部时什么也不做。
    pub(super) fn focus_tab_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !Self::tabs_on_left(cx) || crate::lock_ui::is_locked(cx) {
            return;
        }
        tracing::debug!("焦点进 Tab 列表");
        self.tab_list.cursor = self.highlighted_tab(window, cx).map(Cursor::Tab);
        self.tab_list.focus.focus(window, cx);
        cx.notify();
    }

    /// 列表有焦点，或用键盘打开的菜单开着：画出当前行的焦点框，窄栏上浮出的列表不收起。
    fn tab_list_focused(&self, window: &Window) -> bool {
        self.tab_list.focus.is_focused(window) || self.tab_list.key_menu.is_some()
    }

    /// 当前行在这一帧的列表中的位置。记下的那一行不在了时：折进了主机标题的 Tab 是它的主机标题，否则按上次的位置
    /// 就近（关掉的 Tab 的下一行补上来）；还没有当前行时是高亮的 Tab（折叠着时是它的主机标题），再没有时是第一行。
    fn cursor_position(&self, listing: &Listing) -> Option<usize> {
        let entries = &listing.entries;
        let find = |cursor: &Cursor| {
            entries
                .iter()
                .position(|e| entry_cursor(e).as_ref() == Some(cursor))
        };
        let folded_into = |panel: PanelId| {
            entries.iter().position(|e| {
                matches!(e, Entry::Host { members, collapsed: true, .. } if members.contains(&panel))
            })
        };
        let selectable = |ix: &usize| entry_cursor(&entries[*ix]).is_some();
        let found = match &self.tab_list.cursor {
            Some(cursor) => find(cursor)
                .or_else(|| match cursor {
                    Cursor::Tab(panel) => folded_into(*panel),
                    Cursor::Host(..) => None,
                })
                .or_else(|| {
                    let ix = self.tab_list.cursor_ix.min(entries.len().checked_sub(1)?);
                    (0..=ix)
                        .rev()
                        .chain(ix + 1..entries.len())
                        .find(|i| (selectable)(i))
                }),
            None => listing
                .highlighted
                .and_then(|panel| find(&Cursor::Tab(panel)).or_else(|| folded_into(panel))),
        };
        found.or_else(|| (0..entries.len()).find(|i| (selectable)(i)))
    }

    /// 键盘操作用的这一帧的列表与当前行的位置。当前行是推算出来的（还没有记下、那一行不在了）时记下它：之后的操作
    /// （例如展开主机标题后再按 →）从同一行接着走。
    fn keyboard_listing(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> (Listing, Option<usize>) {
        let group_by_host = AppState::store(cx)
            .local_settings()
            .window
            .group_tabs_by_host;
        let listing = self.tab_listing(group_by_host, window, cx);
        let at = self.cursor_position(&listing);
        if let Some(ix) = at {
            self.tab_list.cursor = entry_cursor(&listing.entries[ix]);
            self.tab_list.cursor_ix = ix;
        }
        (listing, at)
    }

    /// 当前行移到列表中的第 `ix` 项，滚进视野。
    fn set_cursor(&mut self, listing: &Listing, ix: usize, cx: &mut Context<Self>) {
        let Some(cursor) = listing.entries.get(ix).and_then(entry_cursor) else {
            return;
        };
        self.tab_list.cursor = Some(cursor);
        self.tab_list.cursor_ix = ix;
        self.tab_list.scroll.scroll_to_item(ix);
        cx.notify();
    }

    /// ↑ ↓、Home、End：移动当前行（跳过窗格的标题）。
    fn select_in_tab_list(&mut self, step: Step, window: &mut Window, cx: &mut Context<Self>) {
        let (listing, at) = self.keyboard_listing(window, cx);
        let rows: Vec<usize> = (0..listing.entries.len())
            .filter(|ix| entry_cursor(&listing.entries[*ix]).is_some())
            .collect();
        let (Some(&first), Some(&last)) = (rows.first(), rows.last()) else {
            return;
        };
        let target = match (step, at) {
            (Step::First, _) => first,
            (Step::Last, _) => last,
            (Step::Prev, Some(at)) => rows
                .iter()
                .rev()
                .find(|&&ix| ix < at)
                .copied()
                .unwrap_or(at),
            (Step::Next, Some(at)) => rows.iter().find(|&&ix| ix > at).copied().unwrap_or(at),
            (Step::Prev | Step::Next, None) => first,
        };
        self.set_cursor(&listing, target, cx);
    }

    /// Enter：切到当前行的 Tab 并让它获得焦点；当前行是主机标题时折叠 / 展开。
    fn activate_in_tab_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (listing, at) = self.keyboard_listing(window, cx);
        match at.and_then(|ix| listing.entries.get(ix)) {
            Some(Entry::Tab { panel, .. }) => {
                let panel = *panel;
                self.show_listed_tab(panel, window, cx);
            }
            Some(Entry::Host { key, .. }) => {
                let key = key.clone();
                self.toggle_host_group(&key, cx);
            }
            _ => {}
        }
    }

    /// Delete：关闭当前行的 Tab（各自的确认照旧）；之后焦点仍在列表上，当前行是补上来的下一行。
    fn close_in_tab_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (listing, at) = self.keyboard_listing(window, cx);
        let Some((ix, Entry::Tab { panel, .. })) =
            at.and_then(|ix| listing.entries.get(ix).map(|e| (ix, e)))
        else {
            return;
        };
        let panel = *panel;
        tracing::debug!("在 Tab 列表中用键盘关闭 Tab");
        self.tab_list.cursor = Some(Cursor::Tab(panel));
        self.tab_list.cursor_ix = ix;
        self.request_close_tab(panel, window, cx);
        // 窗格接着显示的 Tab 下一轮才收到“显示着”的通知，终端这时会拿走焦点：之后把焦点还给列表（弹出了确认框时不抢）
        cx.spawn_in(window, async move |this, cx| {
            let _ = this.update_in(cx, |this, window, cx| {
                if !window.has_active_dialog(cx) && Self::tabs_on_left(cx) {
                    this.tab_list.focus.focus(window, cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// ← →：折叠 / 展开当前行的主机标题；在主机下的 Tab 上按 ← 移到主机标题，在展开的主机标题上按 → 移到其中第一个
    /// Tab。
    fn fold_in_tab_list(&mut self, expand: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (listing, at) = self.keyboard_listing(window, cx);
        let Some(at) = at else {
            return;
        };
        match &listing.entries[at] {
            Entry::Host { key, collapsed, .. } => match (expand, *collapsed) {
                (false, false) | (true, true) => {
                    let key = key.clone();
                    self.set_host_group_collapsed(&key, !expand, cx);
                }
                (true, false) => self.set_cursor(&listing, at + 1, cx),
                (false, true) => {}
            },
            Entry::Tab { nested: true, .. } if !expand => {
                if let Some(host) = listing.entries[..at]
                    .iter()
                    .rposition(|e| matches!(e, Entry::Host { .. }))
                {
                    self.set_cursor(&listing, host, cx);
                }
            }
            Entry::Tab { .. } | Entry::Pane { .. } => {}
        }
    }

    /// Shift+F10、菜单键：在当前行的下面打开这个 Tab 的右键菜单（与右键相同，先切到这个 Tab）。菜单关掉时没选菜单项、
    /// 或选的项没把焦点移到别处，焦点回到列表。
    fn open_menu_in_tab_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (listing, at) = self.keyboard_listing(window, cx);
        let Some(Entry::Tab { panel, .. }) = at.and_then(|ix| listing.entries.get(ix)) else {
            return;
        };
        let panel = *panel;
        let Some(view) = listing.views.get(&panel).cloned() else {
            return;
        };
        if PanelHandle::of(&view).is_none() {
            return;
        }
        tracing::debug!("在 Tab 列表中用键盘打开 Tab 菜单");
        let panel_focus = view.focus_handle(cx);
        // 菜单中的操作（分屏、重新连接等）派发给显示着的面板
        self.show_listed_tab(panel, window, cx);
        let this = cx.entity().downgrade();
        window.defer(cx, move |window, cx| {
            let menu =
                PopupMenu::build(window, cx, move |menu, window, cx| {
                    match PanelHandle::of(&view) {
                        Some(handle) => handle.dropdown_menu(menu, window, cx),
                        None => menu,
                    }
                });
            let focus = menu.focus_handle(cx);
            focus.focus(window, cx);
            let _ = this.update(cx, |this, cx| {
                let dismissed = cx.subscribe_in(
                    &menu,
                    window,
                    move |this, _, _: &DismissEvent, window, cx| {
                        this.tab_list.key_menu = None;
                        let back = window.focused(cx).is_none_or(|f| f == panel_focus);
                        if back && Self::tabs_on_left(cx) {
                            this.tab_list.focus.focus(window, cx);
                        }
                        cx.notify();
                    },
                );
                *this.tab_list.menu.borrow_mut() = Some(focus.clone());
                this.tab_list.key_menu = Some((menu, dismissed));
                cx.notify();
            });
            // 切过去的 Tab 下一轮才收到“显示着”的通知，终端这时会拿走焦点：之后把焦点还给菜单
            window
                .spawn(cx, async move |cx| {
                    let _ = cx.update(|window, cx| focus.focus(window, cx));
                })
                .detach();
        });
    }

    /// Esc：焦点回到终端（焦点所在的窗格显示着的 Tab）。
    fn leave_tab_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.highlighted_tab(window, cx) {
            Some(panel) => self.select_tab_panel(panel, window, cx),
            None => self.focus.focus(window, cx),
        }
        cx.notify();
    }

    /// 用键盘打开的菜单所在的一层：放在当前行（`relative` 的元素）的下面。
    fn key_menu_layer(&self, window: &Window) -> Option<AnyElement> {
        let (menu, _) = self.tab_list.key_menu.as_ref()?;
        let margin = rems(0.5).to_pixels(window.rem_size());
        Some(
            div()
                .absolute()
                .top_full()
                .left(rems(1.))
                .child(
                    deferred(
                        anchored()
                            .snap_to_window_with_margin(margin)
                            .child(menu.clone()),
                    )
                    .with_priority(gpui_kit::base::POPUP_PRIORITY),
                )
                .into_any_element(),
        )
    }

    // ------------------------------------------------------------ 渲染

    /// 竖排的 Tab 列表（Tab 放在左侧时，在主机侧栏右边）。`sidebar` 为左边主机侧栏占的宽度。
    pub(super) fn render_tab_list(
        &mut self,
        sidebar: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.tab_list.left = sidebar;
        let group_by_host = AppState::store(cx)
            .local_settings()
            .window
            .group_tabs_by_host;
        let squeezed = self.tab_list_squeezed(sidebar, window);
        let collapsed = AppState::global(cx).ui.read(cx).state().tab_list_collapsed;
        let width = self.tab_list_width(sidebar, window, cx);
        if !(collapsed || squeezed) {
            self.tab_list.rail_hovered = false;
            self.tab_list.overlay_hovered = false;
            let mut listing = self.tab_listing(group_by_host, window, cx);
            self.mark_cursor(&mut listing, window);
            let theme = cx.theme();
            return div()
                .id("vtab-column")
                .test_support()
                .relative()
                .flex_none()
                .w(width)
                .h_full()
                .bg(theme.sidebar)
                .border_r_1()
                .border_color(theme.sidebar_border)
                .child(self.render_expanded(&mut listing, false, squeezed, cx))
                .child(self.render_tab_list_resize(cx))
                .into_any_element();
        }
        let menu_open = self.tab_list_menu_open(window, cx);
        let show_overlay = self.tab_list.rail_hovered
            || self.tab_list.overlay_hovered
            || menu_open
            || self.tab_list_focused(window);
        if !show_overlay {
            self.tab_list.overlay_hovered = false;
        }
        let mut listing = self.tab_listing(group_by_host, window, cx);
        let rail = self.render_rail(&mut listing, squeezed, cx);
        let overlay_width = self.tab_overlay_width(sidebar, window, cx);
        let overlay = show_overlay.then(|| {
            let mut listing = self.tab_listing(group_by_host, window, cx);
            self.mark_cursor(&mut listing, window);
            let theme = cx.theme();
            let (bg, border) = (theme.popover, theme.border);
            // 浮在窄栏的右边，不盖住窄栏（窄栏顶上的“⇥”、图标照样能点）
            deferred(
                div()
                    .id("vtab-overlay")
                    .test_support()
                    .absolute()
                    .top_0()
                    .left(rems(RAIL_WIDTH))
                    .h_full()
                    .w(overlay_width)
                    .bg(bg)
                    .border_r_1()
                    .border_color(border)
                    .shadow_lg()
                    .occlude()
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        this.tab_list.overlay_hovered = *hovered;
                        cx.notify();
                    }))
                    .child(self.render_expanded(&mut listing, true, squeezed, cx)),
            )
            .with_priority(OVERLAY_PRIORITY)
        });
        let theme = cx.theme();
        div()
            .id("vtab-rail")
            .test_support()
            .relative()
            .flex_none()
            .w(rems(RAIL_WIDTH))
            .h_full()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.tab_list.rail_hovered = *hovered;
                cx.notify();
            }))
            .child(rail)
            .children(overlay)
            .into_any_element()
    }

    /// 列表有焦点时标出当前行（画焦点框），用键盘打开的菜单放在它下面。
    fn mark_cursor(&self, listing: &mut Listing, window: &Window) {
        if !self.tab_list_focused(window) {
            return;
        }
        listing.ring = self.cursor_position(listing);
        listing.key_menu = self.key_menu_layer(window);
    }

    /// 展开的列表：标题行、各行与“+ 新建”。`overlay` 为窄栏上浮出的；`squeezed` 为窗口太窄、自动用窄栏。
    fn render_expanded(
        &mut self,
        listing: &mut Listing,
        overlay: bool,
        squeezed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // 高亮的 Tab 变了：滚进视野
        let highlight_ix = listing.entries.iter().position(
            |e| matches!(e, Entry::Tab { panel, .. } if Some(*panel) == listing.highlighted),
        );
        if self.tab_list.revealed != listing.highlighted {
            self.tab_list.revealed = listing.highlighted;
            if let Some(ix) = highlight_ix {
                self.tab_list.scroll.scroll_to_item(ix);
            }
        }
        let entries = std::mem::take(&mut listing.entries);
        let rows: Vec<AnyElement> = entries
            .into_iter()
            .enumerate()
            .map(|(ix, entry)| self.render_entry(ix, entry, listing, cx))
            .collect();
        let scroll = self.tab_list.scroll.clone();
        let focus = self.tab_list.focus.clone();
        v_flex()
            .size_full()
            .child(self.render_list_header(overlay, squeezed, cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        // 点空白处时焦点进列表（点行时切到那个 Tab，见 `render_tab_row`），之后可以用键盘逐行操作
                        v_flex()
                            .id("vtab-list")
                            .test_support()
                            .key_context(LIST_CONTEXT)
                            .track_focus(&focus)
                            .on_action(cx.listener(|this, _: &SelectPrev, window, cx| {
                                this.select_in_tab_list(Step::Prev, window, cx)
                            }))
                            .on_action(cx.listener(|this, _: &SelectNext, window, cx| {
                                this.select_in_tab_list(Step::Next, window, cx)
                            }))
                            .on_action(cx.listener(|this, _: &SelectFirst, window, cx| {
                                this.select_in_tab_list(Step::First, window, cx)
                            }))
                            .on_action(cx.listener(|this, _: &SelectLast, window, cx| {
                                this.select_in_tab_list(Step::Last, window, cx)
                            }))
                            .on_action(cx.listener(|this, _: &Activate, window, cx| {
                                this.activate_in_tab_list(window, cx)
                            }))
                            .on_action(cx.listener(|this, _: &CloseItem, window, cx| {
                                this.close_in_tab_list(window, cx)
                            }))
                            .on_action(cx.listener(|this, _: &Collapse, window, cx| {
                                this.fold_in_tab_list(false, window, cx)
                            }))
                            .on_action(cx.listener(|this, _: &Expand, window, cx| {
                                this.fold_in_tab_list(true, window, cx)
                            }))
                            .on_action(cx.listener(|this, _: &OpenMenu, window, cx| {
                                this.open_menu_in_tab_list(window, cx)
                            }))
                            .on_action(cx.listener(|this, _: &FocusTab, window, cx| {
                                this.leave_tab_list(window, cx)
                            }))
                            .size_full()
                            .track_scroll(&scroll)
                            .overflow_y_scroll()
                            .px_1()
                            .pb_1()
                            .children(rows),
                    )
                    .vertical_scrollbar(&scroll),
            )
            .child(
                h_flex().flex_none().px_1().py_1().child(
                    Button::new("vtab-new")
                        .ghost()
                        .small()
                        .icon(IconName::Plus)
                        .label(t!("tab_list.new"))
                        .tooltip_with_action(
                            t!("title_bar.new_local_terminal"),
                            &NewLocalTerminal,
                            Some(KEY_CONTEXT),
                        )
                        .on_click(
                            cx.listener(|this, _, window, cx| this.open_local_terminal(window, cx)),
                        ),
                ),
            )
            .into_any_element()
    }

    /// 标题行：“Tab”与收起（“⇤”）/ 展开（“⇥”，窄栏上浮出的列表中；窗口太窄时没有）的按钮；右键可以切换按主机分组、
    /// 把 Tab 放回顶部。
    fn render_list_header(
        &self,
        overlay: bool,
        squeezed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let toggle = match (overlay, squeezed) {
            (false, _) => Some(
                Button::new("vtab-collapse")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ArrowLeftToLine)
                    .tooltip(t!("tab_list.collapse"))
                    .on_click(cx.listener(|this, _, _, cx| this.set_tab_list_collapsed(true, cx))),
            ),
            (true, false) => Some(
                Button::new("vtab-expand")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ArrowRightToLine)
                    .tooltip(t!("tab_list.expand"))
                    .on_click(cx.listener(|this, _, _, cx| this.set_tab_list_collapsed(false, cx))),
            ),
            (true, true) => None,
        };
        let group_by_host = AppState::store(cx)
            .local_settings()
            .window
            .group_tabs_by_host;
        let this = cx.entity().downgrade();
        h_flex()
            .id("vtab-header")
            .test_support()
            .flex_none()
            .h(rems(2.))
            .pl_2()
            .pr_1()
            .gap_1()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(muted)
                    .child(t!("tab_list.title")),
            )
            .children(toggle)
            .context_menu(move |menu, _, _| {
                let (group, top) = (this.clone(), this.clone());
                menu.item(
                    PopupMenuItem::new(t!("tab_list.group_by_host"))
                        .checked(group_by_host)
                        .on_click(move |_, window, cx| {
                            let _ = group.update(cx, |w, cx| w.toggle_group_by_host(window, cx));
                        }),
                )
                .item(PopupMenuItem::new(t!("tab_list.move_to_top")).on_click(
                    move |_, window, cx| {
                        let _ = top.update(cx, |w, cx| w.toggle_tab_position(window, cx));
                    },
                ))
            })
            .into_any_element()
    }

    /// 列表中的第 `ix` 项。
    fn render_entry(
        &mut self,
        ix: usize,
        entry: Entry,
        listing: &mut Listing,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ring = listing.ring == Some(ix);
        match entry {
            Entry::Pane { node, label } => self.render_pane_header(node, label, cx),
            Entry::Host {
                node,
                key,
                label,
                members,
                collapsed,
                shown,
                highlighted,
            } => {
                let theme = cx.theme();
                let (muted, primary) = (theme.muted_foreground, theme.primary);
                let (active, hover, radius) = (theme.list_active, theme.list_hover, theme.radius);
                let ring_color = theme.ring;
                let mark = listing.multi && collapsed && shown;
                let count = members.len();
                let id = SharedString::from(format!("vtab-host-{}-{key}", node.as_u64()));
                let tip = if collapsed {
                    t!("tab_list.host_expand")
                } else {
                    t!("tab_list.host_collapse")
                };
                h_flex()
                    .id(id)
                    .test_support()
                    .role(gpui_kit::Role::Button)
                    .aria_label(label.clone())
                    .aria_expanded(!collapsed)
                    .flex_none()
                    .w_full()
                    .min_w_0()
                    .h(rems(ROW_HEIGHT))
                    .pl_2()
                    .pr_1()
                    .gap_1()
                    .rounded(radius)
                    .text_sm()
                    .map(|el| {
                        if collapsed && highlighted {
                            el.bg(active)
                        } else {
                            el.hover(|s| s.bg(hover))
                        }
                    })
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .xsmall()
                        .text_color(muted),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(label))
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(muted)
                            .child(format!("({count})")),
                    )
                    .when(mark, |el| el.child(shown_mark(None, primary)))
                    .when(ring, |el| el.border_1().border_color(ring_color))
                    // 点主机标题不让列表拿走焦点（焦点照旧在终端上）
                    .on_any_mouse_down(|_, window, _| window.prevent_default())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tab_list.cursor = Some(Cursor::Host(node, key.clone()));
                        this.toggle_host_group(&key, cx)
                    }))
                    .into_any_element()
            }
            Entry::Tab {
                panel,
                node,
                nested,
                shown,
            } => {
                let menu = if ring { listing.key_menu.take() } else { None };
                self.render_tab_row(panel, node, nested, shown, ring, menu, listing, cx)
            }
        }
    }

    /// 窗格的标题（几个窗格时）：别的窗格的 Tab 拖到这里放到这个窗格的最后。
    fn render_pane_header(
        &self,
        node: NodeId,
        label: SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        h_flex()
            .id(("vtab-pane", node.as_u64() as usize))
            .test_support()
            .role(gpui_kit::Role::Heading)
            .aria_label(label.clone())
            .flex_none()
            .w_full()
            .min_w_0()
            .h(rems(1.5))
            .px_2()
            .pt_1()
            .text_xs()
            .text_color(muted)
            .child(div().min_w_0().truncate().child(label))
            .drag_over::<DragPanel>(|s, _, _, cx| s.bg(cx.theme().drop_target))
            .on_drop(cx.listener(move |this, drag: &DragPanel, window, cx| {
                this.drop_on_pane(drag, node, window, cx)
            }))
            .into_any_element()
    }

    /// 一个 Tab 的一行：图标、名称（放不下时截断，悬停看完整的）、状态、◀（几个窗格时，窗格显示着它）与悬停时的关闭
    /// 按钮。
    #[allow(clippy::too_many_arguments)]
    fn render_tab_row(
        &mut self,
        panel: PanelId,
        node: NodeId,
        nested: bool,
        shown: bool,
        ring: bool,
        menu: Option<AnyElement>,
        listing: &mut Listing,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = panel.as_u64() as usize;
        let Some(face) = listing.faces.remove(&panel) else {
            return div().into_any_element();
        };
        let view = listing.views.get(&panel).cloned();
        let highlighted = listing.highlighted == Some(panel);
        let mark = listing.multi && shown;
        let drop_place = self
            .tab_list
            .drop
            .filter(|(p, _)| *p == panel && cx.has_active_drag())
            .map(|(_, place)| place);
        let tip: SharedString = match &face.detail {
            Some(detail) => format!("{}\n{detail}", face.name).into(),
            None => face.name.clone(),
        };
        let theme = cx.theme();
        let (active, hover, radius) = (theme.list_active, theme.list_hover, theme.radius);
        let (foreground, tab_foreground) = (theme.foreground, theme.tab_foreground);
        let (primary, drag_border, ring_color) = (theme.primary, theme.drag_border, theme.ring);
        let row = h_flex()
            .id(("vtab", id))
            .test_support()
            .role(gpui_kit::Role::Tab)
            .aria_label(face.name.clone())
            .aria_selected(highlighted)
            .group("vtab-row")
            .relative()
            .flex_none()
            .w_full()
            .min_w_0()
            .h(rems(ROW_HEIGHT))
            .pl(rems(if nested { 1.5 } else { 0.5 }))
            .pr_1()
            .gap_1p5()
            .rounded(radius)
            .text_sm()
            .text_color(if highlighted {
                foreground
            } else {
                tab_foreground
            })
            .map(|el| {
                if highlighted {
                    el.bg(active)
                } else {
                    el.hover(|s| s.bg(hover))
                }
            })
            .when(!listing.menu_open, |el| {
                el.tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            })
            .child(div().flex_none().flex().items_center().child(face.icon))
            .child(div().flex_1().min_w_0().truncate().child(face.name))
            .children(
                face.badges
                    .into_iter()
                    .map(|badge| div().flex_none().flex().items_center().child(badge)),
            )
            .when(mark, |el| el.child(shown_mark(Some(id), primary)))
            .when(ring, |el| el.border_1().border_color(ring_color))
            .children(menu)
            // 悬停时行尾的关闭按钮：盖在状态与 ◀ 上（垫着这一行的底色），不占名称的地方
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .right_0()
                    .pl_1()
                    .pr_1()
                    .flex()
                    .items_center()
                    .rounded(radius)
                    .bg(if highlighted { active } else { hover })
                    .invisible()
                    .group_hover("vtab-row", |s| s.visible())
                    .child(
                        Button::new(("vtab-close", id))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tab_stop(false)
                            .tooltip(t!("panel.close"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.request_close_tab(panel, window, cx);
                            })),
                    ),
            )
            // 插到前面 / 后面的插入线（2 像素的指示线是物理边界）
            .when_some(drop_place, |el, place| {
                let line = div()
                    .absolute()
                    .left_1()
                    .right_1()
                    .h(px(2.))
                    .bg(drag_border);
                el.child(match place {
                    Place::Before => line.top_0(),
                    Place::After => line.bottom_0(),
                })
            })
            // 按下鼠标时不让列表拿走焦点：点击切过去后焦点在那个 Tab 上，右键菜单、中键关闭也不经过列表
            .on_any_mouse_down(|_, window, _| window.prevent_default())
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.show_listed_tab(panel, window, cx)
            }))
            // 右键时先切到这个 Tab：菜单中的操作（分屏、重新连接等）派发给显示着的终端
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, window, cx| this.show_listed_tab(panel, window, cx)),
            )
            .on_aux_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if event.is_middle_click() {
                    cx.stop_propagation();
                    this.request_close_tab(panel, window, cx);
                }
            }))
            .on_drag_move::<DragPanel>(cx.listener(
                move |this, event: &DragMoveEvent<DragPanel>, _, cx| {
                    this.on_row_drag_move(panel, event, cx)
                },
            ))
            .on_drop(cx.listener(move |this, drag: &DragPanel, window, cx| {
                this.drop_on_row(drag, panel, window, cx)
            }));
        let row = match view.clone() {
            Some(view) if listing.sole => row.on_drag(
                SoleTabDrag::new(panel),
                crate::dock_skin::start_sole_tab_drag(view),
            ),
            Some(view) => row.on_drag(
                DragPanel::new(panel, node),
                crate::dock_skin::start_tab_drag(view),
            ),
            None => row,
        };
        // 右键是这个 Tab 的菜单（与横排的 Tab 相同）；记下菜单，开着时浮出的列表不收起
        let menu_focus = self.tab_list.menu.clone();
        row.context_menu(move |menu, window, cx| {
            let focus = menu.focus_handle(cx);
            *menu_focus.borrow_mut() = Some(focus.clone());
            // 右键时切过去的 Tab 下一轮才收到“显示着”的通知，终端这时会拿走焦点：之后把焦点还给菜单（键盘照常操作菜单）
            window
                .spawn(cx, async move |cx| {
                    let _ = cx.update(|window, cx| focus.focus(window, cx));
                })
                .detach();
            match view.as_ref().and_then(PanelHandle::of) {
                Some(handle) => handle.dropdown_menu(menu, window, cx),
                None => menu,
            }
        })
        .into_any_element()
    }

    /// 只显示图标的窄栏：每个 Tab 一个图标（几个窗格时各窗格之间有分隔线），图标角上是它最要紧的状态，悬停提示写明
    /// 名称与全部状态；点击切换，中键关闭；悬停时在右边浮出展开的列表。
    fn render_rail(
        &mut self,
        listing: &mut Listing,
        squeezed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (active, hover, radius, border) = (
            theme.list_active,
            theme.list_hover,
            theme.radius,
            theme.border,
        );
        let rail_bg = theme.sidebar;
        let mut items: Vec<AnyElement> = Vec::new();
        let panes = std::mem::take(&mut listing.panes);
        for (ix, pane) in panes.iter().enumerate() {
            if ix > 0 {
                items.push(
                    div()
                        .flex_none()
                        .w(rems(1.25))
                        .h(px(1.))
                        .my_1()
                        .bg(border)
                        .into_any_element(),
                );
            }
            for panel in &pane.tabs {
                let Some(face) = listing.faces.remove(panel) else {
                    continue;
                };
                let panel = *panel;
                let id = panel.as_u64() as usize;
                let highlighted = listing.highlighted == Some(panel);
                let tip = rail_tooltip(&face.name, &face.status);
                let menu_open = listing.menu_open;
                items.push(
                    div()
                        .id(("vtab-rail", id))
                        .test_support()
                        .role(gpui_kit::Role::Tab)
                        .aria_label(face.name.clone())
                        .aria_selected(highlighted)
                        .relative()
                        .flex_none()
                        .size(rems(RAIL_CELL))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(radius)
                        .map(|el| {
                            if highlighted {
                                el.bg(active)
                            } else {
                                el.hover(|s| s.bg(hover))
                            }
                        })
                        .when(!menu_open, |el| {
                            el.tooltip(move |window, cx| {
                                Tooltip::new(tip.clone()).build(window, cx)
                            })
                        })
                        .child(face.rail_icon)
                        .children(face.status.first().map(|status| {
                            rail_status(id, status, if highlighted { active } else { rail_bg })
                        }))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.show_listed_tab(panel, window, cx)
                        }))
                        .on_aux_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                            if event.is_middle_click() {
                                cx.stop_propagation();
                                this.request_close_tab(panel, window, cx);
                            }
                        }))
                        .into_any_element(),
                );
            }
        }
        let scroll = self.tab_list.rail_scroll.clone();
        v_flex()
            .size_full()
            .items_center()
            .child(
                h_flex()
                    .flex_none()
                    .h(rems(2.))
                    .items_center()
                    .when(!squeezed, |el| {
                        el.child(
                            Button::new("vtab-rail-expand")
                                .ghost()
                                .xsmall()
                                .icon(IconName::ArrowRightToLine)
                                .tooltip(t!("tab_list.expand"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_tab_list_collapsed(false, cx)
                                })),
                        )
                    }),
            )
            .child(
                div().relative().flex_1().min_h_0().w_full().child(
                    v_flex()
                        .id("vtab-rail-list")
                        .size_full()
                        .track_scroll(&scroll)
                        .overflow_y_scroll()
                        .items_center()
                        .gap_0p5()
                        .children(items),
                ),
            )
            .child(
                div().flex_none().py_1().child(
                    Button::new("vtab-rail-new")
                        .ghost()
                        .small()
                        .icon(IconName::Plus)
                        .tooltip_with_action(
                            t!("title_bar.new_local_terminal"),
                            &NewLocalTerminal,
                            Some(KEY_CONTEXT),
                        )
                        .on_click(
                            cx.listener(|this, _, window, cx| this.open_local_terminal(window, cx)),
                        ),
                ),
            )
            .into_any_element()
    }

    /// 展开的列表右边缘的拖动区域：骑在分隔线上，比线宽，不占布局。
    fn render_tab_list_resize(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let drag = cx.theme().drag_border;
        div()
            .id("vtab-resize")
            .test_support()
            .track_focus(&self.tab_list.resize_focus)
            .absolute()
            .top_0()
            .bottom_0()
            .right(-rems(0.25))
            .w_2()
            .cursor_col_resize()
            .hover(|s| s.bg(drag.opacity(0.5)))
            .focus(|s| s.bg(drag))
            .tooltip(|window, cx| Tooltip::new(t!("tab_list.resize")).build(window, cx))
            .on_key_down(cx.listener(Self::on_tab_list_resize_key))
            .on_drag(TabListResize, |_, _, _, cx| cx.new(|_| TabListResize))
            .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                this.tab_list.resize_focus.focus(window, cx);
                if event.click_count() >= 2 {
                    AppState::ui(cx)
                        .update(cx, |ui, cx| ui.update(|s| s.tab_list_width = None, cx));
                }
            }))
    }
}

/// 测试用的读取。
#[cfg(test)]
impl Workspace {
    /// 列表有焦点（键盘操作中）。
    pub(super) fn tab_list_has_focus(&self, window: &Window) -> bool {
        self.tab_list.focus.is_focused(window)
    }

    /// 键盘操作的当前行：Tab 为 `Ok`，主机标题为 `Err(主机的键)`；还没有当前行时为 `None`。
    pub(super) fn tab_list_cursor(&self) -> Option<Result<PanelId, SharedString>> {
        self.tab_list.cursor.clone().map(|cursor| match cursor {
            Cursor::Tab(panel) => Ok(panel),
            Cursor::Host(_, key) => Err(key),
        })
    }
}

/// 窄栏上图标的悬停提示：名称，下面逐行是全部状态。
fn rail_tooltip(name: &SharedString, status: &[TabStatus]) -> SharedString {
    std::iter::once(name.clone())
        .chain(status.iter().map(|s| s.label.clone()))
        .collect::<Vec<_>>()
        .join("\n")
        .into()
}

/// 窄栏上图标右上角的状态：圆点（响铃是铃铛），垫一圈底色与图标隔开。
fn rail_status(tab: usize, status: &TabStatus, ground: Hsla) -> AnyElement {
    let mark = div()
        .id(("vtab-rail-status", tab))
        .test_support()
        .absolute()
        .top(rems(0.125))
        .right(rems(0.125))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(ground);
    if status.bell {
        mark.child(
            Icon::new(IconName::Bell)
                .with_size(Size::XSmall)
                .text_color(status.color),
        )
        .into_any_element()
    } else {
        mark.p(rems(0.0625))
            .child(div().size(rems(0.5)).rounded_full().bg(status.color))
            .into_any_element()
    }
}

/// ◀：窗格显示着的 Tab（几个窗格时）。
fn shown_mark(tab: Option<usize>, color: gpui_kit::Hsla) -> AnyElement {
    let mark = div().flex_none().text_xs().text_color(color).child("◀");
    match tab {
        Some(id) => mark
            .id(("vtab-shown", id))
            .test_support()
            .into_any_element(),
        None => mark.into_any_element(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 以 root 身份打开的文件管理与这台主机的终端归在同一台主机下
    #[test]
    fn host_group_key_counts_root_as_the_same_host() {
        let host = SftpTarget::Host(termlet_core::id::HostId::from_raw("h_1"));
        let root = SftpTarget::Sudo(Box::new(host.clone()));
        assert_eq!(host_group_key(&host), "h_1");
        assert_eq!(host_group_key(&root), "h_1");
    }

    // 窄栏的悬停提示：名称，下面逐行是全部状态（最要紧的在前）；没有状态时只有名称
    #[test]
    fn rail_tooltip_lists_every_status() {
        let color = gpui_kit::Hsla::default();
        let status = [
            TabStatus::dot(color, "连接已断开".into()),
            TabStatus {
                bell: true,
                ..TabStatus::dot(color, "响铃了".into())
            },
        ];
        assert_eq!(
            rail_tooltip(&"web".into(), &status),
            SharedString::from("web\n连接已断开\n响铃了")
        );
        assert_eq!(rail_tooltip(&"web".into(), &[]), SharedString::from("web"));
    }

    // 窗格按位置起名：居中的一格、四角、只分一个方向时
    #[test]
    fn pane_positions() {
        assert!(matches!(third(0., 1.), Third::Whole));
        assert!(matches!(third(0., 0.5), Third::Start));
        assert!(matches!(third(0.5, 0.5), Third::End));
        assert!(matches!(third(1. / 3., 1. / 3.), Third::Middle));
        assert_eq!(
            position_name(Third::Start, Third::Whole),
            Some(t!("tab_list.pane.left"))
        );
        assert_eq!(
            position_name(Third::End, Third::Start),
            Some(t!("tab_list.pane.top_right"))
        );
        assert_eq!(position_name(Third::Middle, Third::Start), None);
    }
}
