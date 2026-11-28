//! 侧边栏的主机页：搜索（也是快速连接的入口）、工作区、本机的终端、收藏、分组树与未分组的主机。
//!
//! - 双击主机或选中后按 Enter：新开一个 Tab 连接；点行首的状态圆点：切到它已打开的终端。
//! - 双击工作区打开其中的全部会话；双击本机的终端打开它。
//! - 分组与"工作区""本机""收藏""主机"几节默认全部折叠（新建的、同步过来的分组也是），单击切换；本机的界面状态
//!   记的是展开着的（`expanded_groups`），一操作就记下，重启后保持；删掉的分组在下次折叠、展开时从中清掉。
//! - 拖动主机或分组调整顺序、放进别的分组（拖到"未分组"移出分组）；指向折叠的分组停留片刻自动展开，放进折叠的
//!   分组后展开它。新建、编辑主机或分组后（主机对话框、分组对话框、复制主机）展开它所在的各级分组并选中它
//!   （[`HostTree::reveal`]）。
//! - 键盘：上下选择（折叠着的分组里的主机不在行中，选不到），左右折叠 / 展开（或到上级 / 第一个子项），Enter 打开，
//!   空格切换折叠，F2 编辑，Delete 删除。
//! - 搜索时平铺匹配的主机、列出匹配的工作区与本机的终端（不折叠），折叠状态不变，清空搜索后照旧；搜索时不能折叠。
//! - 搜索框中按 Enter：能认出地址时作为临时连接打开；Esc 清空。

use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _, StyledExt as _, VirtualListScrollHandle, h_flex, v_flex,
    v_virtual_list,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, ClickEvent, Context, DragMoveEvent, Entity, EventEmitter,
    FocusHandle, Focusable, InteractiveElement as _, IntoElement, KeyBinding, MouseButton,
    ParentElement as _, Pixels, Render, ScrollStrategy, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, TestSupportExt as _, Window,
    actions, div, prelude::FluentBuilder as _, px, rems, size,
};
use termlet_core::id::{GroupId, HostId, WorkspaceId};
use termlet_core::localshell::Profile;
use termlet_core::model::{Group, Host, IconStyle, Kind, Protocol, RemoteOs, Workspace};
use termlet_core::quick::{self, QuickTarget};
use termlet_core::store::{Record, Store, StoreEvent};

use super::rows::{
    Dragged, DropPlace, DropTarget, FAVORITES_SECTION, HOSTS_SECTION, LOCAL_SECTION, LocalEntry,
    Move, Row, RowKey, RowKind, RowOptions, SECTION_PREFIX, WORKSPACES_SECTION, build_rows,
    drop_target, plan_move,
};
use crate::app_state::AppState;
use crate::brand_icons;
use crate::i18n::LocalizedPlaceholder as _;
use crate::local_shell::LocalTarget;
use crate::prose::prose;
use crate::scrolling::ScrollGutter as _;
use crate::settings::{self, Page as SettingsPage};
use crate::terminal_panel::SessionStatus;
use crate::truncated::truncated;
use crate::ui_state::UiStateModel;
use crate::{group_dialog, host_dialog, local_shell, store_errors, t, workspaces};

actions!(
    host_tree,
    [
        SelectPrevious,
        SelectNext,
        /// 折叠选中的分组，或选中上级分组
        CollapseOrParent,
        /// 展开选中的分组，或选中第一个子项
        ExpandOrChild,
        /// 打开选中的主机，或切换分组的折叠
        Activate,
        ToggleGroup,
        EditSelected,
        DeleteSelected,
    ]
);

const KEY_CONTEXT: &str = "HostTree";
/// 拖到折叠的分组上停留这么久后自动展开。
const AUTO_EXPAND: Duration = Duration::from_millis(600);

pub fn init(cx: &mut App) {
    cx.bind_keys(key_bindings(cx.platform_info().os));
}

/// 主机树的快捷键：`os` 为按哪个系统的习惯。macOS 的键盘上没有向后删除键（`delete` 要按 Fn+⌫），另照 Finder 用 ⌘⌫ 删除。
pub(crate) fn key_bindings(os: gpui_kit::OperatingSystem) -> Vec<KeyBinding> {
    let context = Some(KEY_CONTEXT);
    let mut bindings = vec![
        KeyBinding::new("up", SelectPrevious, context),
        KeyBinding::new("down", SelectNext, context),
        KeyBinding::new("left", CollapseOrParent, context),
        KeyBinding::new("right", ExpandOrChild, context),
        KeyBinding::new("enter", Activate, context),
        KeyBinding::new("space", ToggleGroup, context),
        KeyBinding::new("f2", EditSelected, context),
        KeyBinding::new("delete", DeleteSelected, context),
    ];
    if os == gpui_kit::OperatingSystem::Mac {
        bindings.push(KeyBinding::new("cmd-backspace", DeleteSelected, context));
    }
    bindings
}

/// 主机树请求工作区做的事。
pub enum HostTreeEvent {
    /// 新开一个 Tab 连接这台主机
    OpenHost(HostId),
    /// 快速连接（临时，不保存）
    OpenQuick(QuickTarget),
    /// 切到这台主机已打开的终端（都已断开时重新连接）
    RevealHost(HostId),
    /// 打开保存的工作区
    OpenWorkspace(WorkspaceId),
    /// 打开本机的终端（设置中的写法：`local:<shell ID>` 或 `wsl:<发行版>`）
    OpenLocal(String),
    /// 打开 WSL 发行版的文件管理（`user` 为空时用默认用户）
    OpenWslSftp { distro: String, user: String },
    /// 查看这台主机的命令历史
    OpenHistory(HostId),
    /// 为这台主机打开 SFTP 文件管理（新连接、新 Tab）
    OpenSftp(HostId),
    /// 以 root 身份（经 sudo）为这台主机打开 SFTP（新连接、新 Tab）
    OpenSftpRoot(HostId),
}

/// 一台主机已打开的终端的情况（行首的圆点）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostStatus {
    /// 多个终端时取最好的
    pub status: SessionStatus,
    pub count: usize,
}

pub struct HostTree {
    store: Arc<Store>,
    ui: Entity<UiStateModel>,
    search: Entity<InputState>,
    /// 搜索框的内容能认出的快速连接地址
    quick: Option<QuickTarget>,
    groups: Vec<Record<Group>>,
    hosts: Vec<Record<Host>>,
    workspaces: Vec<Record<Workspace>>,
    /// 检测到的本地 shell（不含自定义的）；`None` 为还在检测
    detected: Option<Vec<Profile>>,
    /// 检测到的 WSL 发行版
    wsl: Vec<String>,
    rows: Vec<Row>,
    /// 生成行时用的展开集合（本机界面状态中的 `expanded_groups`）：界面状态变了而它没变时不用重新生成
    expanded: BTreeSet<String>,
    selected: Option<RowKey>,
    focus: FocusHandle,
    scroll: VirtualListScrollHandle,
    statuses: HashMap<HostId, HostStatus>,
    show_address: bool,
    /// 系统与 shell 标志的颜色：设置“通用”中的“图标”（彩色 / 单色）
    icon_style: IconStyle,
    /// 正在拖动本树中的条目（显示"未分组"作为放置位置）
    dragging: bool,
    drop_target: Option<DropTarget>,
    /// 窗口中开着弹出菜单（右键菜单等）：这一帧行中的按钮不挂悬停提示（行自己的提示经 [`crate::menu_tooltip::quiet`]）
    menu_open: bool,
    _auto_expand: Option<Task<()>>,
    _detect: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<HostTreeEvent> for HostTree {}

impl Focusable for HostTree {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl HostTree {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let app = AppState::global(cx);
        let store_model = app.store.clone();
        let ui = app.ui.clone();
        let store = store_model.read(cx).store().clone();
        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .localized_placeholder(|| t!("hosts.search.placeholder"), window, cx)
                .clean_on_escape()
        });
        let subscriptions = vec![
            cx.subscribe_in(
                &search,
                window,
                |this, input, event, window, cx| match event {
                    InputEvent::Change => {
                        let query = input.read(cx).value().to_string();
                        this.quick = quick::parse(&query);
                        this.rebuild(cx);
                    }
                    InputEvent::PressEnter { .. } => {
                        if let Some(target) = this.quick.take() {
                            cx.emit(HostTreeEvent::OpenQuick(target));
                            input.update(cx, |input, cx| input.set_value("", window, cx));
                            this.rebuild(cx);
                        }
                    }
                    _ => {}
                },
            ),
            // 别的窗口折叠、展开了分组（界面状态是共用的）：跟着变
            cx.observe(&ui, |this, ui, cx| {
                if ui.read(cx).state().expanded_groups != this.expanded {
                    this.rebuild(cx);
                }
            }),
            cx.subscribe(
                &store_model,
                |this, _, event: &StoreEvent, cx| match event {
                    StoreEvent::Changed(Kind::Host | Kind::Group | Kind::Workspace)
                    | StoreEvent::LocalSettingsChanged => this.rebuild(cx),
                    StoreEvent::SettingsChanged => {
                        let settings = this.store.settings();
                        this.show_address = settings.general.show_host_address;
                        this.icon_style = settings.appearance.icons;
                        this.rebuild(cx);
                    }
                    _ => {}
                },
            ),
        ];
        let settings = store.settings();
        let show_address = settings.general.show_host_address;
        let icon_style = settings.appearance.icons;
        let mut tree = Self {
            store,
            ui,
            search,
            quick: None,
            groups: Vec::new(),
            hosts: Vec::new(),
            workspaces: Vec::new(),
            detected: None,
            wsl: Vec::new(),
            rows: Vec::new(),
            expanded: BTreeSet::new(),
            selected: None,
            focus: cx.focus_handle(),
            scroll: VirtualListScrollHandle::new(),
            statuses: HashMap::new(),
            show_address,
            icon_style,
            dragging: false,
            drop_target: None,
            menu_open: false,
            _auto_expand: None,
            _detect: None,
            _subscriptions: subscriptions,
        };
        // 检测本地 shell 要读注册表等，在后台进行（启动时已预先检测，通常直接拿到缓存）
        let detector = local_shell::detector(cx);
        let distros = local_shell::wsl_distros(cx);
        tree._detect = Some(cx.spawn(async move |this, cx| {
            let (found, wsl) = cx
                .background_executor()
                .spawn(async move { (detector.list(false), distros.list()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.detected = Some(found.into_iter().filter(|p| !p.is_custom()).collect());
                this.wsl = wsl;
                this.rebuild(cx);
            });
        }));
        tree.rebuild(cx);
        tree
    }

    /// "本机"一节：没有隐藏的本地终端（本地 shell 在前，WSL 发行版在后），标出平时显示的（默认的一项与设置中选了
    /// 显示在侧边栏的）；其余的只在搜索时列出。
    fn local_entries(&self) -> Vec<LocalEntry> {
        let Some(detected) = &self.detected else {
            return Vec::new();
        };
        let shells = self.store.local_settings().shells;
        let targets = local_shell::visible_targets(detected.clone(), self.wsl.clone(), &shells);
        let default = targets
            .iter()
            .find(|t| t.key() == shells.default)
            .or(targets.first())
            .map(LocalTarget::key);
        let shown = |t: &LocalTarget| {
            let key = t.key();
            default.as_ref() == Some(&key) || shells.sidebar.contains(&key)
        };
        targets
            .iter()
            .map(|t| LocalEntry {
                key: t.key(),
                name: t.name().to_string(),
                brand: t.brand(),
                is_default: default.as_ref() == Some(&t.key()),
                wsl: matches!(t, LocalTarget::Wsl(_)),
                shown: shown(t),
            })
            .collect()
    }

    pub fn set_statuses(&mut self, statuses: HashMap<HostId, HostStatus>, cx: &mut Context<Self>) {
        if self.statuses != statuses {
            self.statuses = statuses;
            cx.notify();
        }
    }

    /// 重新读取主机与分组，生成行；选中的行还在时保持选中。
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        self.groups = self.store.list::<Group>();
        self.hosts = self.store.list::<Host>();
        self.workspaces = self.store.list::<Workspace>();
        let locals = self.local_entries();
        let query = self.search.read(cx).value().to_string();
        self.expanded = self.ui.read(cx).state().expanded_groups.clone();
        self.rows = build_rows(
            &self.groups,
            &self.hosts,
            &RowOptions {
                expanded: &self.expanded,
                workspaces: &self.workspaces,
                locals: &locals,
                query: &query,
                show_address: self.show_address,
                dragging: self.dragging,
            },
        );
        if let Some(selected) = &self.selected
            && !self.rows.iter().any(|r| &r.key == selected)
        {
            self.selected = None;
        }
        cx.notify();
    }

    fn row_index(&self, key: &RowKey) -> Option<usize> {
        self.rows.iter().position(|r| &r.key == key)
    }

    fn selected_row(&self) -> Option<&Row> {
        self.selected
            .as_ref()
            .and_then(|key| self.rows.iter().find(|r| &r.key == key))
    }

    fn select(&mut self, key: RowKey, cx: &mut Context<Self>) {
        if let Some(ix) = self.row_index(&key) {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Center);
        }
        self.selected = Some(key);
        cx.notify();
    }

    fn set_collapsed(&mut self, group: &GroupId, collapsed: bool, cx: &mut Context<Self>) {
        self.set_folded(&[group.as_str()], collapsed, cx);
    }

    /// 折叠或展开分组与节（键为分组 ID 或节的键），立即记进本机的界面状态。顺带清掉已经删掉的分组，集合不会越积
    /// 越多（还没读到分组时不清）。搜索时行不按折叠状态列出，不改。
    fn set_folded(&mut self, keys: &[&str], collapsed: bool, cx: &mut Context<Self>) {
        if self.searching(cx) {
            return;
        }
        let groups: HashSet<String> = self
            .store
            .list::<Group>()
            .into_iter()
            .map(|g| g.id.as_str().to_string())
            .collect();
        self.ui.update(cx, |ui, cx| {
            ui.update(
                |state| {
                    for key in keys {
                        if collapsed {
                            state.expanded_groups.remove(*key);
                        } else {
                            state.expanded_groups.insert(key.to_string());
                        }
                    }
                    if !groups.is_empty() {
                        state
                            .expanded_groups
                            .retain(|key| key.starts_with(SECTION_PREFIX) || groups.contains(key));
                    }
                },
                cx,
            )
        });
        self.rebuild(cx);
    }

    fn searching(&self, cx: &App) -> bool {
        !self.search.read(cx).value().trim().is_empty()
    }

    fn is_collapsed(&self, group: &GroupId, cx: &App) -> bool {
        !self
            .ui
            .read(cx)
            .state()
            .expanded_groups
            .contains(group.as_str())
    }

    /// 在侧边栏中显示一台主机（终端、文件管理 Tab 菜单与命令面板的“在侧栏中显示主机”）：搜索着时先清空搜索（结果里
    /// 可能没有它），再展开它所在的各级分组、选中并滚动到它（[`Self::reveal`]），焦点移到主机列表（可以接着用方向键、
    /// Enter）。
    pub fn show_host(&mut self, host: HostId, window: &mut Window, cx: &mut Context<Self>) {
        if self.searching(cx) {
            self.search
                .update(cx, |input, cx| input.set_value("", window, cx));
            self.quick = None;
        }
        self.reveal(
            RowKey::Host {
                id: host,
                favorite: false,
            },
            cx,
        );
        self.focus.focus(window, cx);
    }

    /// 让一行看得到（新建、编辑、复制主机或分组，保存工作区后）：展开它所在的各级分组与"主机"一节（工作区展开
    /// "工作区"一节），选中并滚动到它。主机看的是分组树中的那一行（不是收藏里的）；分组本身不展开。搜索时只选中
    /// （看不看得到取决于搜索词）。
    pub fn reveal(&mut self, key: RowKey, cx: &mut Context<Self>) {
        if let RowKey::Workspace(_) = &key {
            self.set_folded(&[WORKSPACES_SECTION], false, cx);
            self.rebuild(cx);
            self.select(key, cx);
            return;
        }
        let groups = self.store.list::<Group>();
        let parent_of = |id: &str| {
            groups
                .iter()
                .find(|g| g.id.as_str() == id)
                .and_then(|g| g.parent.clone())
        };
        let mut parent = match &key {
            RowKey::Host { id, .. } => self.store.get::<Host>(id).and_then(|h| h.parent),
            RowKey::Group(id) => parent_of(id.as_str()),
            _ => None,
        };
        let mut ancestors: Vec<String> = Vec::new();
        // 上级不存在的分组当作顶层；防止数据里有环
        while let Some(id) = parent.filter(|p| groups.iter().any(|g| g.id.as_str() == p)) {
            if ancestors.contains(&id) {
                break;
            }
            parent = parent_of(&id);
            ancestors.push(id);
        }
        let mut keys: Vec<&str> = ancestors.iter().map(String::as_str).collect();
        keys.push(HOSTS_SECTION);
        self.set_folded(&keys, false, cx);
        let key = match key {
            RowKey::Host { id, .. } => RowKey::Host {
                id,
                favorite: false,
            },
            other => other,
        };
        // 新建的主机、分组也要在行中（存储的变化事件稍后才到）
        self.rebuild(cx);
        self.select(key, cx);
    }

    fn toggle_group(&mut self, group: &GroupId, cx: &mut Context<Self>) {
        let collapsed = self.is_collapsed(group, cx);
        self.set_collapsed(group, !collapsed, cx);
    }

    /// 节的标题行对应的折叠键。
    fn section_key(key: &RowKey) -> Option<&'static str> {
        match key {
            RowKey::Workspaces => Some(WORKSPACES_SECTION),
            RowKey::Local => Some(LOCAL_SECTION),
            RowKey::Favorites => Some(FAVORITES_SECTION),
            RowKey::Hosts => Some(HOSTS_SECTION),
            _ => None,
        }
    }

    fn toggle_section(&mut self, key: &RowKey, cx: &mut Context<Self>) {
        if let Some(section) = Self::section_key(key) {
            let expanded = self.ui.read(cx).state().expanded_groups.contains(section);
            self.set_folded(&[section], expanded, cx);
        }
    }

    fn group_of_host(&self, host: &HostId) -> Option<GroupId> {
        let parent = self.hosts.iter().find(|h| &h.id == host)?.parent.clone()?;
        self.groups
            .iter()
            .any(|g| g.id.as_str() == parent)
            .then(|| GroupId::from_raw(parent))
    }

    fn parent_of_group(&self, group: &GroupId) -> Option<GroupId> {
        let parent = self
            .groups
            .iter()
            .find(|g| &g.id == group)?
            .parent
            .clone()?;
        self.groups
            .iter()
            .any(|g| g.id.as_str() == parent)
            .then(|| GroupId::from_raw(parent))
    }

    // ------------------------------------------------------------ 键盘

    fn move_selection(&mut self, step: isize, cx: &mut Context<Self>) {
        let selectable: Vec<usize> = (0..self.rows.len())
            .filter(|ix| !self.rows[*ix].is_section())
            .collect();
        if selectable.is_empty() {
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|key| self.row_index(key))
            .and_then(|ix| selectable.iter().position(|s| *s == ix));
        let next = match current {
            None if step > 0 => 0,
            None => selectable.len() - 1,
            Some(pos) => (pos as isize + step).clamp(0, selectable.len() as isize - 1) as usize,
        };
        let key = self.rows[selectable[next]].key.clone();
        self.select(key, cx);
    }

    fn on_select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, cx);
    }

    fn on_select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, cx);
    }

    fn on_collapse_or_parent(
        &mut self,
        _: &CollapseOrParent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.selected_row().cloned() else {
            return;
        };
        let parent = match (&row.key, &row.kind) {
            (
                key,
                RowKind::Heading {
                    collapsed: false, ..
                },
            ) => {
                self.toggle_section(key, cx);
                return;
            }
            (RowKey::Workspace(_), _) => {
                self.select(RowKey::Workspaces, cx);
                return;
            }
            (RowKey::LocalShell(_), _) => {
                self.select(RowKey::Local, cx);
                return;
            }
            (
                RowKey::Group(id),
                RowKind::Group {
                    collapsed: false, ..
                },
            ) => {
                self.set_collapsed(id, true, cx);
                return;
            }
            (RowKey::Host { favorite: true, .. }, _) => {
                self.select(RowKey::Favorites, cx);
                return;
            }
            (RowKey::Group(id), _) => self.parent_of_group(id),
            (
                RowKey::Host {
                    id,
                    favorite: false,
                },
                _,
            ) => self.group_of_host(id),
            _ => return,
        };
        match parent {
            Some(parent) => self.select(RowKey::Group(parent), cx),
            // 最外层的分组、主机：回到"主机"标题（有的话）
            None if self.rows.iter().any(|r| r.key == RowKey::Hosts) => {
                self.select(RowKey::Hosts, cx)
            }
            None => {}
        }
    }

    fn on_expand_or_child(&mut self, _: &ExpandOrChild, _: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row().cloned() else {
            return;
        };
        if let RowKind::Heading { collapsed, .. } = row.kind {
            if collapsed {
                self.toggle_section(&row.key, cx);
            } else {
                self.move_selection(1, cx);
            }
            return;
        }
        if let (
            RowKey::Group(id),
            RowKind::Group {
                collapsed, count, ..
            },
        ) = (&row.key, &row.kind)
        {
            if *collapsed {
                self.set_collapsed(id, false, cx);
            } else if *count > 0
                || self
                    .groups
                    .iter()
                    .any(|g| g.parent.as_deref() == Some(id.as_str()))
            {
                self.move_selection(1, cx);
            }
        }
    }

    fn on_activate(&mut self, _: &Activate, _: &mut Window, cx: &mut Context<Self>) {
        match self.selected.clone() {
            Some(RowKey::Host { id, .. }) => cx.emit(HostTreeEvent::OpenHost(id)),
            Some(RowKey::Group(id)) => self.toggle_group(&id, cx),
            Some(RowKey::Workspace(id)) => cx.emit(HostTreeEvent::OpenWorkspace(id)),
            Some(RowKey::LocalShell(id)) => cx.emit(HostTreeEvent::OpenLocal(id)),
            Some(key) if Self::section_key(&key).is_some() => self.toggle_section(&key, cx),
            _ => {}
        }
    }

    fn on_toggle_group(&mut self, _: &ToggleGroup, _: &mut Window, cx: &mut Context<Self>) {
        match self.selected.clone() {
            Some(RowKey::Group(id)) => self.toggle_group(&id, cx),
            Some(key) if Self::section_key(&key).is_some() => self.toggle_section(&key, cx),
            _ => {}
        }
    }

    fn on_edit_selected(&mut self, _: &EditSelected, window: &mut Window, cx: &mut Context<Self>) {
        match self.selected.clone() {
            Some(RowKey::Host { id, .. }) => host_dialog::open_edit(id, window, cx),
            Some(RowKey::Group(id)) => group_dialog::open_edit(id, window, cx),
            Some(RowKey::Workspace(id)) => workspaces::prompt_rename(id, window, cx),
            Some(RowKey::LocalShell(_) | RowKey::Local) => {
                settings::open(SettingsPage::Local, window, cx)
            }
            _ => {}
        }
    }

    fn on_delete_selected(
        &mut self,
        _: &DeleteSelected,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.selected.clone() {
            Some(RowKey::Host { id, .. }) => host_dialog::confirm_delete(id, window, cx),
            Some(RowKey::Group(id)) => group_dialog::confirm_delete(id, window, cx),
            Some(RowKey::Workspace(id)) => workspaces::confirm_delete(id, window, cx),
            _ => {}
        }
    }

    // ------------------------------------------------------------ 拖放

    fn on_row_drag_move(
        &mut self,
        row_ix: usize,
        event: &DragMoveEvent<Dragged>,
        cx: &mut Context<Self>,
    ) {
        if !event.bounds.contains(&event.event.position) {
            return;
        }
        let Some(row) = self.rows.get(row_ix) else {
            return;
        };
        let height = f32::from(event.bounds.size.height).max(1.);
        let y = f32::from(event.event.position.y - event.bounds.origin.y) / height;
        let target = drop_target(event.drag(cx), row, y, &self.groups);
        if target == self.drop_target {
            return;
        }
        // 指向折叠的分组、处于"放进去"的位置：停留片刻后展开
        self._auto_expand = match (&target, &row.kind) {
            (
                Some(DropTarget {
                    row: RowKey::Group(id),
                    place: DropPlace::Into,
                }),
                RowKind::Group {
                    collapsed: true, ..
                },
            ) => {
                let id = id.clone();
                Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(AUTO_EXPAND).await;
                    let _ = this.update(cx, |this, cx| {
                        let still = this
                            .drop_target
                            .as_ref()
                            .is_some_and(|t| t.row == RowKey::Group(id.clone()));
                        if still && cx.has_active_drag() {
                            this.set_collapsed(&id, false, cx);
                        }
                    });
                }))
            }
            _ => None,
        };
        self.drop_target = target;
        cx.notify();
    }

    fn on_drop(&mut self, dragged: &Dragged, window: &mut Window, cx: &mut Context<Self>) {
        let target = self.drop_target.take();
        self.dragging = false;
        self._auto_expand = None;
        if let Some(target) = target
            && let Some(plan) = plan_move(dragged, &target, &self.groups, &self.hosts)
        {
            let result = match plan {
                Move::Host {
                    id,
                    parent,
                    position,
                } => {
                    tracing::info!(host = %id, parent = ?parent, "拖动主机");
                    // 放进折叠的分组后展开它，看得到放到了哪里
                    if let Some(parent) = &parent {
                        self.set_collapsed(parent, false, cx);
                    }
                    self.store.move_to::<Host>(
                        &id,
                        parent.map(|p| p.as_str().to_string()),
                        position,
                    )
                }
                Move::Group {
                    id,
                    parent,
                    position,
                } => {
                    tracing::info!(group = %id, parent = ?parent, "拖动分组");
                    // 放进折叠的分组后展开它，看得到放到了哪里
                    if let Some(parent) = &parent {
                        self.set_collapsed(parent, false, cx);
                    }
                    self.store.move_to::<Group>(
                        &id,
                        parent.map(|p| p.as_str().to_string()),
                        position,
                    )
                }
            };
            if let Err(e) = result {
                crate::notify::error(store_errors::describe(&e), window, cx);
            }
        }
        self.rebuild(cx);
    }

    fn start_drag(&mut self, cx: &mut Context<Self>) {
        if !self.dragging {
            self.dragging = true;
            self.rebuild(cx);
        }
    }

    /// 拖动结束而没有放在树上（放到了别处、按了 Esc）。
    fn finish_drag_if_cancelled(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dragging && !cx.has_active_drag() {
            cx.defer_in(window, |this, _, cx| {
                if this.dragging && !cx.has_active_drag() {
                    this.dragging = false;
                    this.drop_target = None;
                    this._auto_expand = None;
                    this.rebuild(cx);
                }
            });
        }
    }

    // ------------------------------------------------------------ 菜单

    fn toggle_favorite(&mut self, id: &HostId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(record) = self.store.get::<Host>(id) else {
            return;
        };
        let mut host = record.doc;
        host.favorite = !host.favorite;
        if let Err(e) = self.store.update(id, host) {
            crate::notify::error(store_errors::describe(&e), window, cx);
        }
    }

    /// 复制一台主机：副本叫“名称 - 副本”，放在它后面并选中（内容与秘密的规则见 [`Store::duplicate_host`]）。
    fn duplicate(&mut self, id: &HostId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(record) = self.store.get::<Host>(id) else {
            return;
        };
        let taken: std::collections::HashSet<String> = self
            .store
            .list::<Host>()
            .into_iter()
            .map(|h| h.doc.name)
            .collect();
        let name = copy_name(&record.doc.name, &taken);
        match self.store.duplicate_host(id, name) {
            Ok(copy) => self.reveal(
                RowKey::Host {
                    id: copy,
                    favorite: false,
                },
                cx,
            ),
            Err(e) => crate::notify::error(store_errors::describe(&e), window, cx),
        }
    }

    /// 主机的右键菜单；Telnet、串口主机（`ssh` 为假）没有 SFTP、命令历史，串口没有“复制 IP”。"只用一个通道"的主机
    /// （`single_channel`）不能以 root 身份打开 SFTP。
    fn host_menu(
        this: gpui_kit::WeakEntity<Self>,
        id: HostId,
        favorite: bool,
        ssh: bool,
        single_channel: bool,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu {
        move |menu, _, cx| {
            // 打开菜单时读取，地址是最新的
            let address = AppState::store(cx)
                .get::<Host>(&id)
                .map(|h| h.doc)
                .filter(|h| h.protocol != Protocol::Serial && !h.address.is_empty())
                .map(|h| h.address);
            let duplicate = {
                let (this, id) = (this.clone(), id.clone());
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let _ = this.update(cx, |this, cx| this.duplicate(&id, window, cx));
                }
            };
            let open = {
                let (this, id) = (this.clone(), id.clone());
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    let _ = this.update(cx, |_, cx| cx.emit(HostTreeEvent::OpenHost(id.clone())));
                }
            };
            let edit = {
                let id = id.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    host_dialog::open_edit(id.clone(), window, cx)
                }
            };
            let star = {
                let (this, id) = (this.clone(), id.clone());
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let _ = this.update(cx, |this, cx| this.toggle_favorite(&id, window, cx));
                }
            };
            let delete = {
                let id = id.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    host_dialog::confirm_delete(id.clone(), window, cx)
                }
            };
            let history = {
                let (this, id) = (this.clone(), id.clone());
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    let _ =
                        this.update(cx, |_, cx| cx.emit(HostTreeEvent::OpenHistory(id.clone())));
                }
            };
            let sftp = {
                let (this, id) = (this.clone(), id.clone());
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    let _ = this.update(cx, |_, cx| cx.emit(HostTreeEvent::OpenSftp(id.clone())));
                }
            };
            let sftp_root = {
                let (this, id) = (this.clone(), id.clone());
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    let _ =
                        this.update(cx, |_, cx| cx.emit(HostTreeEvent::OpenSftpRoot(id.clone())));
                }
            };
            let export = {
                let id = id.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    crate::backup_ui::open_export(vec![id.clone()], window, cx)
                }
            };
            let menu = menu.item(PopupMenuItem::new(t!("hosts.menu.open")).on_click(open));
            let menu = if ssh {
                let menu = menu.item(PopupMenuItem::new(t!("sftp.open")).on_click(sftp));
                let menu = if single_channel {
                    menu
                } else {
                    menu.item(PopupMenuItem::new(t!("sftp.open_root")).on_click(sftp_root))
                };
                menu.item(PopupMenuItem::new(t!("history.open")).on_click(history))
            } else {
                menu
            };
            let menu = match address {
                Some(address) => menu.item(crate::copy_address_item(address)),
                None => menu,
            };
            menu.separator()
                .item(
                    PopupMenuItem::new(t!("hosts.menu.edit"))
                        .action(Box::new(EditSelected))
                        .on_click(edit),
                )
                .item(PopupMenuItem::new(t!("hosts.menu.duplicate")).on_click(duplicate))
                .item(
                    PopupMenuItem::new(if favorite {
                        t!("hosts.menu.unfavorite")
                    } else {
                        t!("hosts.menu.favorite")
                    })
                    .on_click(star),
                )
                .item(PopupMenuItem::new(t!("hosts.menu.export")).on_click(export))
                .separator()
                .item(
                    PopupMenuItem::new(t!("hosts.menu.delete"))
                        .action(Box::new(DeleteSelected))
                        .on_click(delete),
                )
        }
    }

    fn group_menu(
        id: GroupId,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu {
        move |menu, _, _| {
            let new_host = {
                let id = id.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    host_dialog::open_new(Some(id.clone()), window, cx)
                }
            };
            let new_group = {
                let id = id.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    group_dialog::open_new(Some(id.clone()), window, cx)
                }
            };
            let edit = {
                let id = id.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    group_dialog::open_edit(id.clone(), window, cx)
                }
            };
            let delete = {
                let id = id.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    group_dialog::confirm_delete(id.clone(), window, cx)
                }
            };
            let export = {
                let id = id.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let hosts = hosts_under(&AppState::store(cx), &id);
                    crate::backup_ui::open_export(hosts, window, cx)
                }
            };
            menu.item(PopupMenuItem::new(t!("hosts.menu.new_host_in_group")).on_click(new_host))
                .item(PopupMenuItem::new(t!("hosts.menu.new_subgroup")).on_click(new_group))
                .separator()
                .item(
                    PopupMenuItem::new(t!("hosts.menu.edit_group"))
                        .action(Box::new(EditSelected))
                        .on_click(edit),
                )
                .item(PopupMenuItem::new(t!("hosts.menu.export")).on_click(export))
                .separator()
                .item(
                    PopupMenuItem::new(t!("hosts.menu.delete_group"))
                        .action(Box::new(DeleteSelected))
                        .on_click(delete),
                )
        }
    }

    fn workspace_menu(
        this: gpui_kit::WeakEntity<Self>,
        id: WorkspaceId,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu {
        move |menu, _, _| {
            let open = {
                let (this, id) = (this.clone(), id.clone());
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    let _ = this.update(cx, |_, cx| {
                        cx.emit(HostTreeEvent::OpenWorkspace(id.clone()))
                    });
                }
            };
            let rename = {
                let id = id.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    workspaces::prompt_rename(id.clone(), window, cx)
                }
            };
            let delete = {
                let id = id.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    workspaces::confirm_delete(id.clone(), window, cx)
                }
            };
            menu.item(PopupMenuItem::new(t!("workspace.open")).on_click(open))
                .separator()
                .item(
                    PopupMenuItem::new(t!("workspace.rename"))
                        .action(Box::new(EditSelected))
                        .on_click(rename),
                )
                .item(
                    PopupMenuItem::new(t!("workspace.delete"))
                        .action(Box::new(DeleteSelected))
                        .on_click(delete),
                )
        }
    }

    fn local_menu(
        this: gpui_kit::WeakEntity<Self>,
        key: String,
        is_default: bool,
        shown: bool,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu {
        move |menu, _, _| {
            let open = {
                let (this, key) = (this.clone(), key.clone());
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    let _ = this.update(cx, |_, cx| cx.emit(HostTreeEvent::OpenLocal(key.clone())));
                }
            };
            let make_default = {
                let (this, key) = (this.clone(), key.clone());
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let _ = this.update(cx, |this, cx| this.set_default_local(&key, window, cx));
                }
            };
            let hide = {
                let (this, key) = (this.clone(), key.clone());
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let _ = this.update(cx, |this, cx| this.hide_local(&key, window, cx));
                }
            };
            let show = {
                let (this, key) = (this.clone(), key.clone());
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let _ = this.update(cx, |this, cx| this.show_local(&key, window, cx));
                }
            };
            let manage = |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                settings::open(SettingsPage::Local, window, cx)
            };
            let menu = menu.item(PopupMenuItem::new(t!("hosts.local.open")).on_click(open));
            // WSL：文件管理（默认用户与 root）
            let menu = match key.strip_prefix("wsl:") {
                Some(distro) => {
                    let sftp = |user: &'static str| {
                        let (this, distro) = (this.clone(), distro.to_string());
                        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                            let _ = this.update(cx, |_, cx| {
                                cx.emit(HostTreeEvent::OpenWslSftp {
                                    distro: distro.clone(),
                                    user: user.to_string(),
                                })
                            });
                        }
                    };
                    menu.item(PopupMenuItem::new(t!("hosts.local.sftp")).on_click(sftp("")))
                        .item(
                            PopupMenuItem::new(t!("hosts.local.sftp_root")).on_click(sftp("root")),
                        )
                        .separator()
                }
                None => menu,
            };
            let menu = if is_default {
                menu
            } else {
                let menu = menu
                    .item(PopupMenuItem::new(t!("hosts.local.set_default")).on_click(make_default));
                if shown {
                    menu.item(PopupMenuItem::new(t!("hosts.local.hide")).on_click(hide))
                } else {
                    menu.item(PopupMenuItem::new(t!("hosts.local.show")).on_click(show))
                }
            };
            menu.separator()
                .item(PopupMenuItem::new(t!("hosts.local.manage")).on_click(manage))
        }
    }

    /// 设为 Ctrl+Shift+T 打开的默认本地终端。
    fn set_default_local(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let key = key.to_string();
        let result = self.store.update_local_settings(|s| s.shells.default = key);
        self.report(result, window, cx);
    }

    /// 从侧边栏"本机"中移除（仍可从设置中的本地终端打开）。
    fn show_local(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let result = self.store.update_local_settings(|s| {
            if !s.shells.sidebar.iter().any(|k| k == key) {
                s.shells.sidebar.push(key.to_string());
            }
        });
        self.report(result, window, cx);
    }

    fn hide_local(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let result = self
            .store
            .update_local_settings(|s| s.shells.sidebar.retain(|k| k != key));
        self.report(result, window, cx);
    }

    fn report(
        &self,
        result: termlet_core::Result<()>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(e) = result {
            crate::notify::error(store_errors::describe(&e), window, cx);
        }
    }

    // ------------------------------------------------------------ 渲染

    fn row_height(&self, row: &Row, rem: Pixels) -> Pixels {
        let rems_high = match &row.kind {
            RowKind::Section | RowKind::Heading { .. } => 1.75,
            RowKind::Workspace { .. } | RowKind::LocalShell { .. } => 1.75,
            RowKind::Group { .. } => 1.75,
            RowKind::Host {
                address: Some(_), ..
            } => 2.5,
            RowKind::Host { .. } => 1.75,
        };
        rems(rems_high).to_pixels(rem)
    }

    fn render_row(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let row = self.rows[ix].clone();
        // 先做好主机的图标与状态圆点（要可变地借用 cx，之后再取主题）
        let host_icon = match (&row.key, &row.kind) {
            (
                RowKey::Host { id, .. },
                RowKind::Host {
                    protocol,
                    os,
                    address,
                    ..
                },
            ) => Some(self.render_host_icon(
                id,
                HostIcon {
                    protocol: *protocol,
                    os: *os,
                    two_lines: address.is_some(),
                },
                self.statuses.get(id).copied(),
                cx,
            )),
            _ => None,
        };
        let selected = self.selected.as_ref() == Some(&row.key);
        // 搜索结果是平铺的，看不出分组：不能拖动
        let searching = !self.search.read(cx).value().trim().is_empty();
        let target = self
            .drop_target
            .as_ref()
            .filter(|t| t.row == row.key && cx.has_active_drag())
            .map(|t| t.place);
        let theme = cx.theme();
        let element_id = SharedString::from(match &row.key {
            RowKey::Workspaces => "row-workspaces".to_string(),
            RowKey::Workspace(id) => format!("row-workspace-{id}"),
            RowKey::Local => "row-local".to_string(),
            RowKey::LocalShell(key) => format!("row-{key}"),
            RowKey::Hosts => "row-hosts".to_string(),
            RowKey::Favorites => "row-favorites".to_string(),
            RowKey::TopLevel => "row-top-level".to_string(),
            RowKey::Group(id) => format!("row-group-{id}"),
            RowKey::Host { id, favorite } => {
                format!("row-host-{id}{}", if *favorite { "-fav" } else { "" })
            }
        });
        let indent = rems(0.375 + row.depth as f32);
        let mut el =
            h_flex()
                .id(element_id)
                .test_support()
                .relative()
                .size_full()
                .pl(indent)
                .pr_2()
                .gap_1()
                .rounded(theme.radius)
                .when(selected, |el| el.bg(theme.list_active))
                .when(!selected && !row.is_section(), |el| {
                    el.hover(|s| s.bg(theme.list_hover))
                })
                .when(target == Some(DropPlace::Into), |el| {
                    el.bg(theme.drop_target)
                        .border_1()
                        .border_color(theme.drag_border)
                })
                .on_drag_move::<Dragged>(cx.listener(
                    move |this, event: &DragMoveEvent<Dragged>, _, cx| {
                        this.on_row_drag_move(ix, event, cx)
                    },
                ))
                .on_drop(cx.listener(|this, dragged: &Dragged, window, cx| {
                    this.on_drop(dragged, window, cx)
                }))
                // 右键的行就是菜单作用的行：菜单开着时按 Delete、F2 也作用于它（菜单自己拿着焦点，这里不抢）
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener({
                        let key = row.key.clone();
                        move |this, _, _, cx| {
                            if this.selected.as_ref() != Some(&key) {
                                this.selected = Some(key.clone());
                                cx.notify();
                            }
                        }
                    }),
                );
        // 插到前面 / 后面的插入线（2 像素的指示线是物理边界）
        if let Some(place @ (DropPlace::Before | DropPlace::After)) = target {
            let line = div()
                .absolute()
                .left(indent)
                .right_0()
                .h(px(2.))
                .bg(theme.drag_border);
            el = el.child(if place == DropPlace::Before {
                line.top_0()
            } else {
                line.bottom_0()
            });
        }
        match row.kind {
            RowKind::Section => {
                el
                    // 拖动时最下面的放置位置：画成一个框
                    .when(
                        row.key == RowKey::TopLevel && target != Some(DropPlace::Into),
                        |el| el.border_1().border_color(theme.border),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(t!("hosts.drop_top_level")),
                    )
                    .into_any_element()
            }
            RowKind::Heading { count, collapsed } => {
                let key = row.key.clone();
                let label = match key {
                    RowKey::Workspaces => t!("workspace.section"),
                    RowKey::Favorites => t!("hosts.section.favorites"),
                    RowKey::Hosts => t!("hosts.section.hosts"),
                    _ => t!("hosts.local.section"),
                };
                let el = el
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .small()
                        .text_color(theme.muted_foreground),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(label),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(count.to_string()),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.focus.focus(window, cx);
                        this.selected = Some(key.clone());
                        this.toggle_section(&key, cx);
                    }));
                if row.key == RowKey::Local {
                    el.context_menu(|menu, _, _| {
                        menu.item(PopupMenuItem::new(t!("hosts.local.manage")).on_click(
                            |_, window, cx| settings::open(SettingsPage::Local, window, cx),
                        ))
                    })
                    .into_any_element()
                } else {
                    el.into_any_element()
                }
            }
            RowKind::Workspace { name, panes } => {
                let RowKey::Workspace(id) = row.key.clone() else {
                    unreachable!()
                };
                let this = cx.entity().downgrade();
                // 与主机行一样空出折叠箭头的位置，图标对成一列
                el.child(div().flex_none().size_3p5())
                    .child(
                        Icon::new(IconName::LayoutDashboard)
                            .small()
                            .text_color(theme.muted_foreground),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(rems(NAME_TEXT))
                            .child(name),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(rems(SECONDARY_TEXT))
                            .text_color(theme.muted_foreground)
                            .child(t!("workspace.panes", count = panes)),
                    )
                    .tooltip(crate::menu_tooltip::quiet(|window, cx| {
                        Tooltip::new(t!("workspace.open_hint")).build(window, cx)
                    }))
                    .on_click(cx.listener({
                        let key = row.key.clone();
                        let id = id.clone();
                        move |this, event: &ClickEvent, window, cx| {
                            this.focus.focus(window, cx);
                            this.selected = Some(key.clone());
                            if event.click_count() >= 2 {
                                cx.emit(HostTreeEvent::OpenWorkspace(id.clone()));
                            }
                            cx.notify();
                        }
                    }))
                    .context_menu(Self::workspace_menu(this, id))
                    .into_any_element()
            }
            RowKind::LocalShell {
                name,
                brand,
                is_default,
                wsl,
                shown,
            } => {
                let RowKey::LocalShell(key) = row.key.clone() else {
                    unreachable!()
                };
                let this = cx.entity().downgrade();
                let distro = key.strip_prefix("wsl:").map(str::to_string);
                el.child(div().flex_none().size_3p5())
                    .child(
                        brand_icons::icon_or(
                            brand,
                            IconName::SquareTerminal,
                            self.icon_style,
                            theme.muted_foreground,
                            theme.sidebar,
                            cx,
                        )
                        .small(),
                    )
                    // 本地终端的名称放不下时截断，悬停看全名
                    .child(
                        truncated(SharedString::from(format!("local-shell-name-{key}")), name)
                            .text_size(rems(NAME_TEXT)),
                    )
                    .when(wsl, |el| {
                        el.child(
                            div()
                                .flex_none()
                                .text_size(rems(SECONDARY_TEXT))
                                .text_color(theme.muted_foreground)
                                .child("WSL"),
                        )
                    })
                    .child(div().flex_1())
                    .when(is_default, |el| {
                        el.child(
                            div()
                                .flex_none()
                                .text_size(rems(SECONDARY_TEXT))
                                .text_color(theme.muted_foreground)
                                .child(t!("hosts.local.default")),
                        )
                    })
                    // 悬停时出现：WSL 的文件管理
                    .group("local-row")
                    .when_some(distro, |el, distro| {
                        el.child(
                            div()
                                .flex_none()
                                .invisible()
                                .group_hover("local-row", |s| s.visible())
                                .child(
                                    Button::new(SharedString::from(format!("row-{key}-sftp")))
                                        .ghost()
                                        .small()
                                        .icon(IconName::FolderOpen)
                                        .tab_stop(false)
                                        .when(!self.menu_open, |this| {
                                            this.tooltip(t!("hosts.local.sftp"))
                                        })
                                        .on_click(cx.listener(move |_, _, _, cx| {
                                            cx.stop_propagation();
                                            cx.emit(HostTreeEvent::OpenWslSftp {
                                                distro: distro.clone(),
                                                user: String::new(),
                                            });
                                        })),
                                ),
                        )
                    })
                    .on_click(cx.listener({
                        let row_key = row.key.clone();
                        let key = key.clone();
                        move |this, event: &ClickEvent, window, cx| {
                            this.focus.focus(window, cx);
                            this.selected = Some(row_key.clone());
                            if event.click_count() >= 2 {
                                cx.emit(HostTreeEvent::OpenLocal(key.clone()));
                            }
                            cx.notify();
                        }
                    }))
                    .context_menu(Self::local_menu(this, key, is_default, shown))
                    .into_any_element()
            }
            RowKind::Group {
                name,
                count,
                collapsed,
            } => {
                let RowKey::Group(id) = row.key.clone() else {
                    unreachable!()
                };
                let drag_label = SharedString::from(name.clone());
                el.child(
                    Icon::new(if collapsed {
                        IconName::ChevronRight
                    } else {
                        IconName::ChevronDown
                    })
                    .small()
                    .text_color(theme.muted_foreground),
                )
                .child(
                    Icon::new(if collapsed {
                        IconName::Folder
                    } else {
                        IconName::FolderOpen
                    })
                    .small()
                    .text_color(theme.muted_foreground),
                )
                // 分组名放不下时截断，悬停看全名（主机行的提示另有地址等）
                .child(
                    truncated(SharedString::from(format!("group-name-{id}")), name)
                        .flex_1()
                        .font_medium()
                        .text_size(rems(NAME_TEXT)),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(rems(SECONDARY_TEXT))
                        .text_color(theme.muted_foreground)
                        .child(count.to_string()),
                )
                .on_click(cx.listener({
                    let id = id.clone();
                    move |this, _, window, cx| {
                        this.focus.focus(window, cx);
                        this.selected = Some(RowKey::Group(id.clone()));
                        this.toggle_group(&id, cx);
                    }
                }))
                .when(!searching, |el| {
                    el.on_drag(Dragged::Group(id.clone()), {
                        let this = cx.entity().downgrade();
                        move |_, _, _, cx| {
                            let _ = this.update(cx, |this, cx| this.start_drag(cx));
                            cx.new(|_| DragPreview(drag_label.clone()))
                        }
                    })
                })
                .context_menu(crate::menu_fit::fitted(Self::group_menu(id)))
                .into_any_element()
            }
            RowKind::Host {
                name,
                os,
                address,
                tags,
                favorite,
                endpoint,
                ..
            } => {
                let RowKey::Host { id, .. } = row.key.clone() else {
                    unreachable!()
                };
                // Telnet、串口主机没有 SFTP、命令历史
                let record = self.hosts.iter().find(|h| h.id == id);
                let ssh =
                    record.is_none_or(|h| h.doc.protocol == termlet_core::model::Protocol::Ssh);
                let single_channel = record.is_some_and(|h| h.doc.ssh.single_channel);
                let drag_label = SharedString::from(name.clone());
                let this = cx.entity().downgrade();
                let two_lines = address.is_some();
                el.child(
                    // 图标与文字整体上下居中：单行时对着名称，显示地址时对着两行文字的整体。名称为主（正常颜色），
                    // 地址与标签为次（小一号、弱化的颜色）
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .children(host_icon)
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .truncate()
                                        .text_size(rems(NAME_TEXT))
                                        .line_height(rems(if two_lines {
                                            NAME_LINE_TWO
                                        } else {
                                            NAME_LINE
                                        }))
                                        .child(name),
                                )
                                .when_some(address, |this, address| {
                                    this.child(
                                        h_flex()
                                            .gap_1()
                                            .min_w_0()
                                            .text_size(rems(SECONDARY_TEXT))
                                            .line_height(rems(SECONDARY_LINE))
                                            .text_color(theme.muted_foreground)
                                            .child(div().truncate().child(address))
                                            .children(tags.into_iter().map(|tag| {
                                                div().flex_none().child(format!("#{tag}"))
                                            })),
                                    )
                                }),
                        ),
                )
                .when(favorite, |el| {
                    el.child(
                        Icon::new(IconName::StarFill)
                            .small()
                            .text_color(theme.warning),
                    )
                })
                // 悬停时出现：打开 SFTP
                .group("host-row")
                .when(ssh, |el| {
                    el.child(
                        div()
                            .flex_none()
                            .invisible()
                            .group_hover("host-row", |s| s.visible())
                            .child(
                                Button::new(SharedString::from(format!("row-host-sftp-{id}")))
                                    .ghost()
                                    .small()
                                    .icon(IconName::FolderOpen)
                                    .tab_stop(false)
                                    .when(!self.menu_open, |this| this.tooltip(t!("sftp.open")))
                                    .on_click(cx.listener({
                                        let id = id.clone();
                                        move |_, _, _, cx| {
                                            cx.stop_propagation();
                                            cx.emit(HostTreeEvent::OpenSftp(id.clone()));
                                        }
                                    })),
                            ),
                    )
                })
                .tooltip(crate::menu_tooltip::quiet({
                    // 识别出系统时带上系统的名称（不认识的标志也知道是什么）
                    let endpoint = SharedString::from(match os {
                        Some(os) => t!(
                            "hosts.endpoint.with_os",
                            endpoint = endpoint,
                            os = os.name()
                        )
                        .to_string(),
                        None => endpoint,
                    });
                    move |window, cx| Tooltip::new(endpoint.clone()).build(window, cx)
                }))
                .on_click(cx.listener({
                    let key = row.key.clone();
                    let id = id.clone();
                    move |this, event: &ClickEvent, window, cx| {
                        this.focus.focus(window, cx);
                        this.selected = Some(key.clone());
                        if event.click_count() >= 2 {
                            cx.emit(HostTreeEvent::OpenHost(id.clone()));
                        }
                        cx.notify();
                    }
                }))
                .when(!searching, |el| {
                    el.on_drag(Dragged::Host(id.clone()), {
                        let this = this.clone();
                        move |_, _, _, cx| {
                            let _ = this.update(cx, |this, cx| this.start_drag(cx));
                            cx.new(|_| DragPreview(drag_label.clone()))
                        }
                    })
                })
                .context_menu(crate::menu_fit::fitted(Self::host_menu(
                    this,
                    id,
                    favorite,
                    ssh,
                    single_channel,
                )))
                .into_any_element()
            }
        }
    }

    /// 主机行首的图标：识别出系统的 SSH 主机是系统的标志（[`brand_icons::for_os`]），其余（还没识别、Telnet、串口、
    /// 没有标志的系统）按协议（[`crate::host_info::protocol_icon`]）。前面空出分组折叠箭头的位置，同一层的图标、名称与
    /// 分组的对齐（资源管理器、VS Code 的做法）；显示地址（两行）时图标放大到与两行文字相称，多出的宽度从箭头的位置
    /// 扣掉，名称仍与单行时对齐。打开着的主机在图标右下角加状态圆点（绿为已连接、黄为正在连接或重连、红为已断开），
    /// 点它切换到这台主机的终端。
    fn render_host_icon(
        &self,
        id: &HostId,
        host: HostIcon,
        status: Option<HostStatus>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let size = if host.two_lines {
            ICON_TWO_LINES
        } else {
            ICON_ONE_LINE
        };
        let slot = div().relative().flex_none().size(rems(size)).child(
            brand_icons::icon_or(
                host.os.and_then(brand_icons::for_os),
                crate::host_info::protocol_icon(host.protocol),
                self.icon_style,
                theme.muted_foreground,
                theme.sidebar,
                cx,
            )
            .size(rems(size)),
        );
        let icon = |slot| {
            h_flex()
                .flex_none()
                .gap_1()
                // 折叠箭头的位置（与分组行的箭头一样大，图标放大时相应变窄）
                .child(div().flex_none().w(rems(ICON_ONE_LINE * 2. - size)))
                .child(slot)
                .into_any_element()
        };
        let Some(status) = status else {
            return icon(slot);
        };
        let ring = theme.sidebar;
        let dot_offset = if host.two_lines { 0.0625 } else { 0.1875 };
        let state = status.status;
        let (color, tip) = match state {
            SessionStatus::Connected => (
                theme.success,
                t!("hosts.status.connected", count = status.count),
            ),
            SessionStatus::Connecting => (theme.warning, t!("hosts.status.connecting")),
            SessionStatus::Disconnected => (theme.danger, t!("hosts.status.disconnected")),
        };
        let id = id.clone();
        let slot = slot.child(
            div()
                .id(SharedString::from(format!("status-{id}")))
                .test_support()
                .absolute()
                // 压在图标的右下角；放大的图标本身够大，少探出一些，免得挨着右边的地址
                .right(-rems(dot_offset))
                .bottom(-rems(dot_offset))
                .size_2()
                // 不只靠颜色：已连接是实心圆点，正在连接是空心圆圈，已断开是方块（色弱也分得出）
                .map(|this| match state {
                    SessionStatus::Connected => this
                        .rounded_full()
                        .bg(color)
                        // 与侧边栏同色的一圈，压在图标上也看得清
                        .border_1()
                        .border_color(ring),
                    SessionStatus::Connecting => {
                        this.rounded_full().bg(ring).border_2().border_color(color)
                    }
                    SessionStatus::Disconnected => this.bg(color).border_1().border_color(ring),
                })
                .tooltip(crate::menu_tooltip::quiet(move |window, cx| {
                    Tooltip::new(tip.clone()).build(window, cx)
                }))
                .on_click(cx.listener(move |_, _, _, cx| {
                    cx.stop_propagation();
                    cx.emit(HostTreeEvent::RevealHost(id.clone()));
                })),
        );
        icon(slot)
    }

    fn render_toolbar(&self) -> impl IntoElement {
        h_flex()
            .gap_1()
            .px_2()
            .pt_2()
            .pb_1()
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&self.search)
                        .id("host-search")
                        .small()
                        .cleanable(true),
                ),
            )
            .child(
                Button::new("new-host")
                    .ghost()
                    .small()
                    .icon(IconName::Plus)
                    .tooltip(t!("hosts.toolbar.new_host"))
                    .on_click(|_, window, cx| host_dialog::open_new(None, window, cx)),
            )
            .child(
                Button::new("new-group")
                    .ghost()
                    .small()
                    .icon(IconName::FolderPlus)
                    .tooltip(t!("hosts.toolbar.new_group"))
                    .on_click(|_, window, cx| group_dialog::open_new(None, window, cx)),
            )
            .child(
                Button::new("hosts-more")
                    .ghost()
                    .small()
                    .icon(IconName::Ellipsis)
                    .tooltip(t!("hosts.toolbar.more"))
                    .dropdown_menu(|menu, _, _| {
                        menu.item(
                            PopupMenuItem::new(t!("hosts.toolbar.import")).on_click(
                                |_, window, cx| crate::backup_ui::open_import(window, cx),
                            ),
                        )
                        .item(
                            PopupMenuItem::new(t!("hosts.toolbar.export")).on_click(
                                |_, window, cx| {
                                    crate::backup_ui::open_export(Vec::new(), window, cx)
                                },
                            ),
                        )
                    }),
            )
    }

    fn render_quick_connect(
        &self,
        target: &QuickTarget,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let target = target.clone();
        let label = t!("hosts.quick_connect", target = target.display());
        h_flex()
            .id("quick-connect-row")
            .mx_2()
            .px_2()
            .py_1()
            .gap_2()
            .rounded(cx.theme().radius)
            .bg(cx.theme().list_active)
            .text_sm()
            .child(
                Icon::new(IconName::Zap)
                    .small()
                    .text_color(cx.theme().success),
            )
            .child(div().flex_1().min_w_0().truncate().child(label))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("Enter"),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.emit(HostTreeEvent::OpenQuick(target.clone()));
                this.search
                    .update(cx, |input, cx| input.set_value("", window, cx));
                this.quick = None;
                this.rebuild(cx);
            }))
    }

    /// 没有主机、没有搜到时的说明。还没有主机时“还没有主机。”与怎样新建各占一行：连在一起时侧边栏窄，
    /// 按避头尾换行会把“机。”单独带到第二行。
    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let searching = !self.search.read(cx).value().trim().is_empty();
        v_flex()
            .id("host-tree-empty")
            .test_support()
            .p_4()
            .gap_1()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .map(|this| {
                if searching {
                    this.child(prose(t!("hosts.empty.no_match")))
                } else {
                    // 从别的客户端搬过来的人先找导入：放在空状态里，不用先找到“…”菜单
                    this.child(prose(t!("hosts.empty.no_hosts")))
                        .child(prose(t!("hosts.empty.no_hosts_hint")))
                        .child(
                            h_flex().pt_2().child(
                                Button::new("hosts-empty-import")
                                    .outline()
                                    .small()
                                    .label(t!("hosts.import_button"))
                                    .on_click(|_, window, cx| {
                                        crate::backup_ui::open_import(window, cx)
                                    }),
                            ),
                        )
                }
            })
    }
}

impl Render for HostTree {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.finish_drag_if_cancelled(window, cx);
        self.menu_open = crate::menu_tooltip::menu_open(window, cx);
        let rem = window.rem_size();
        let sizes: Rc<Vec<gpui_kit::Size<Pixels>>> = Rc::new(
            self.rows
                .iter()
                .map(|row| size(px(0.), self.row_height(row, rem)))
                .collect(),
        );
        // 还没有主机（只有工作区、本机两节）时，在它们下面给出提示
        let no_hosts = self.hosts.is_empty()
            && self.groups.is_empty()
            && self.search.read(cx).value().trim().is_empty();
        let total: Pixels = sizes.iter().map(|s| s.height).sum();
        let list = if self.rows.is_empty() {
            self.render_empty(cx).into_any_element()
        } else {
            let rows = v_virtual_list(
                cx.entity(),
                "host-tree-rows",
                sizes,
                |this, range, _, cx| range.map(|ix| this.render_row(ix, cx)).collect::<Vec<_>>(),
            )
            .track_scroll(&self.scroll)
            .pl_1()
            .scrollbar_gutter();
            if no_hosts {
                v_flex()
                    .size_full()
                    .child(rows.w_full().h(total))
                    .child(self.render_empty(cx))
                    .into_any_element()
            } else {
                rows.size_full().into_any_element()
            }
        };
        v_flex()
            .size_full()
            .child(self.render_toolbar())
            .when_some(self.quick.clone(), |this, target| {
                this.child(self.render_quick_connect(&target, cx))
            })
            .child(
                div()
                    .id("host-tree")
                    .test_support()
                    .key_context(KEY_CONTEXT)
                    .track_focus(&self.focus)
                    .flex_1()
                    .min_h_0()
                    .pt_1()
                    .on_action(cx.listener(Self::on_select_previous))
                    .on_action(cx.listener(Self::on_select_next))
                    .on_action(cx.listener(Self::on_collapse_or_parent))
                    .on_action(cx.listener(Self::on_expand_or_child))
                    .on_action(cx.listener(Self::on_activate))
                    .on_action(cx.listener(Self::on_toggle_group))
                    .on_action(cx.listener(Self::on_edit_selected))
                    .on_action(cx.listener(Self::on_delete_selected))
                    .child(list)
                    .vertical_scrollbar(&self.scroll),
            )
    }
}

/// 拖动时跟着指针的名称。
pub(crate) struct DragPreview(pub(crate) SharedString);

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded(cx.theme().radius)
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .text_sm()
            .text_color(cx.theme().popover_foreground)
            .child(self.0.clone())
    }
}

/// 分组及其各级子分组中的主机（导出分组用）。
pub(crate) fn hosts_under(store: &Store, group: &GroupId) -> Vec<HostId> {
    let groups = store.list::<Group>();
    let mut inside: std::collections::HashSet<String> =
        std::iter::once(group.as_str().to_string()).collect();
    // 逐层加入子分组（存储保证没有循环；限制层数防御异常数据）
    for _ in 0..64 {
        let before = inside.len();
        for g in &groups {
            if g.parent.as_ref().is_some_and(|p| inside.contains(p)) {
                inside.insert(g.id.as_str().to_string());
            }
        }
        if inside.len() == before {
            break;
        }
    }
    store
        .list::<Host>()
        .into_iter()
        .filter(|h| h.parent.as_ref().is_some_and(|p| inside.contains(p)))
        .map(|h| h.id)
        .collect()
}

/// 树中各行名称的字号（13px，比默认的 14px 小一号）与主机名称的行高；显示地址（两行）时名称的行高收紧一些，
/// 两行文字挨得近，读起来是一组。
const NAME_TEXT: f32 = 0.8125;
const NAME_LINE: f32 = 1.25;
const NAME_LINE_TWO: f32 = 1.125;
/// 次要信息（名称下方的地址与标签，行尾的数量、“WSL”“默认”）的字号与行高：11px，比名称再小一号，与弱化的颜色
/// 一起分出主次。
const SECONDARY_TEXT: f32 = 0.6875;
const SECONDARY_LINE: f32 = 1.0;
/// 行首图标的边长：单行时与分组行的图标一样大，显示地址（两行）时放大，与两行文字的高度相称。
const ICON_ONE_LINE: f32 = 0.875;
const ICON_TWO_LINES: f32 = 1.25;

/// 主机行首的图标要的信息（见 [`HostTree::render_host_icon`]）。
#[derive(Clone, Copy)]
struct HostIcon {
    protocol: Protocol,
    os: Option<RemoteOs>,
    two_lines: bool,
}

/// 副本的名称：“名称 - 副本”；已有同名的主机时加上编号（“名称 - 副本 (2)”，资源管理器的做法）。
fn copy_name(name: &str, taken: &std::collections::HashSet<String>) -> String {
    let first = t!("hosts.copy_name", name = name).to_string();
    if !taken.contains(&first) {
        return first;
    }
    (2..)
        .map(|n| t!("hosts.copy_name_numbered", name = name, n = n).to_string())
        .find(|candidate| !taken.contains(candidate))
        .unwrap_or(first)
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AnyWindowHandle, Bounds, TestAppContext, WindowBounds, WindowOptions, px, size,
    };
    use termlet_core::model::SecretField;
    use termlet_core::secret::SecretString;
    use termlet_core::store::{Owner, Position};

    use super::*;

    /// 弹出的菜单中各项的文案（分隔线为 `None`）。
    fn menu_labels(cx: &mut TestAppContext, handle: AnyWindowHandle) -> Vec<Option<String>> {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let menu = window.within("popup-menu");
            (0..20usize)
                .map_while(|ix| {
                    menu.try_find(ix)
                        .map(|item| item.label().map(str::to_string))
                })
                .collect()
        })
        .unwrap()
    }

    fn click_menu(cx: &mut TestAppContext, handle: AnyWindowHandle, row: &str, label: &str) {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.right_click(row.to_string(), cx);
        })
        .unwrap();
        cx.run_until_parked();
        let labels = menu_labels(cx, handle);
        let ix = labels
            .iter()
            .position(|l| l.as_deref() == Some(label))
            .unwrap_or_else(|| panic!("菜单中没有“{label}”：{labels:?}"));
        cx.update_window(handle, |_, window, cx| {
            window.within("popup-menu").click(ix, cx)
        })
        .unwrap();
        cx.run_until_parked();
    }

    // 主机的右键菜单：复制 IP 写进剪贴板；复制出的主机在它后面、带着密码、被选中
    #[gpui_kit::test]
    fn host_menu_copies_address_and_duplicates(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        cx.update(crate::init);
        cx.update(AppState::install_in_memory);
        let id = cx.update(|cx| {
            let store = AppState::store(cx);
            let id = store
                .create(
                    Host {
                        name: "web".into(),
                        address: "10.0.0.5".into(),
                        username: "deploy".into(),
                        ..Default::default()
                    },
                    None,
                    Position::Last,
                )
                .unwrap();
            store
                .set_secret(
                    &Owner::host(&id),
                    SecretField::Password,
                    Some(&SecretString::new("pw")),
                )
                .unwrap();
            id
        });
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                Default::default(),
                size(px(1024.), px(640.)),
            ))),
            ..Default::default()
        };
        let (handle, _) = cx
            .update(|cx| {
                gpui_kit::open_window(options, cx, |window, cx| {
                    cx.new(|cx| crate::Workspace::new(window, cx))
                })
            })
            .unwrap();
        cx.run_until_parked();
        crate::testkit::expand_host_tree(cx);
        let row = format!("row-host-{id}");

        click_menu(cx, handle, &row, &t!("hosts.menu.copy_ip"));
        assert_eq!(
            cx.read_from_clipboard().and_then(|c| c.text()).as_deref(),
            Some("10.0.0.5")
        );

        click_menu(cx, handle, &row, &t!("hosts.menu.duplicate"));
        let store = cx.update(|cx| AppState::store(cx).clone());
        let hosts = store.list::<Host>();
        assert_eq!(
            hosts
                .iter()
                .map(|h| h.doc.name.as_str())
                .collect::<Vec<_>>(),
            ["web", "web - 副本"]
        );
        let copy = hosts[1].id.clone();
        assert_eq!(hosts[1].doc.address, "10.0.0.5");
        assert_eq!(
            store
                .secret(&Owner::host(&copy), SecretField::Password)
                .unwrap()
                .map(|s| s.expose().to_string())
                .as_deref(),
            Some("pw")
        );
        let tree = cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.find(format!("row-host-{copy}")).visible()
        });
        assert!(tree.unwrap(), "副本出现在列表中");
    }

    // 新加的文案都在 locales 中（缺了的键会原样显示键名）
    #[test]
    fn new_texts_exist() {
        for key in [
            "hosts.drop_top_level",
            "hosts.menu.copy_ip",
            "hosts.menu.copy_hostname",
            "hosts.menu.address_copied",
            "hosts.menu.duplicate",
            "hosts.copy_name",
            "hosts.copy_name_numbered",
        ] {
            assert_ne!(t!(key).as_ref(), key, "缺少文案 {key}");
        }
    }

    // 图标的文件按名单嵌入，漏了的图标画不出来
    #[test]
    fn protocol_icons_are_bundled() {
        use gpui_kit::AssetSource as _;
        for protocol in [Protocol::Ssh, Protocol::Telnet, Protocol::Serial] {
            let path = crate::host_info::protocol_icon(protocol).path();
            assert!(
                crate::assets::AppAssets.load(&path).unwrap().is_some(),
                "{protocol:?} 的图标 {path} 没有嵌入"
            );
        }
    }

    #[test]
    fn copies_get_numbered_names() {
        let mut taken = std::collections::HashSet::new();
        assert_eq!(copy_name("web", &taken), "web - 副本");
        taken.insert("web - 副本".to_string());
        assert_eq!(copy_name("web", &taken), "web - 副本 (2)");
        taken.insert("web - 副本 (2)".to_string());
        assert_eq!(copy_name("web", &taken), "web - 副本 (3)");
    }

    fn setup(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        cx.update(crate::init);
        cx.update(AppState::install_in_memory);
    }

    /// 打开只有主机树的窗口。
    fn open_tree(cx: &mut TestAppContext) -> Entity<HostTree> {
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                Default::default(),
                size(px(320.), px(640.)),
            ))),
            ..Default::default()
        };
        let (_, tree) = cx
            .update(|cx| {
                gpui_kit::open_window(options, cx, |window, cx| {
                    cx.new(|cx| HostTree::new(window, cx))
                })
            })
            .unwrap();
        cx.run_until_parked();
        tree
    }

    fn add_group(cx: &mut TestAppContext, name: &str, parent: Option<&GroupId>) -> GroupId {
        cx.update(|cx| {
            let group = Group {
                name: name.into(),
                ..Default::default()
            };
            AppState::store(cx)
                .create(
                    group,
                    parent.map(|p| p.as_str().to_string()),
                    Position::Last,
                )
                .unwrap()
        })
    }

    fn add_host(cx: &mut TestAppContext, name: &str, parent: Option<&GroupId>) -> HostId {
        cx.update(|cx| {
            let host = Host {
                name: name.into(),
                address: "10.0.0.1".into(),
                username: "ops".into(),
                ..Default::default()
            };
            AppState::store(cx)
                .create(host, parent.map(|p| p.as_str().to_string()), Position::Last)
                .unwrap()
        })
    }

    fn rows(tree: &Entity<HostTree>, cx: &mut TestAppContext) -> Vec<Row> {
        cx.update(|cx| tree.read(cx).rows.clone())
    }

    fn has(rows: &[Row], key: &RowKey) -> bool {
        rows.iter().any(|r| &r.key == key)
    }

    fn expanded(cx: &mut TestAppContext) -> std::collections::BTreeSet<String> {
        cx.update(|cx| AppState::ui(cx).read(cx).state().expanded_groups.clone())
    }

    fn host_key(id: &HostId) -> RowKey {
        RowKey::Host {
            id: id.clone(),
            favorite: false,
        }
    }

    /// 默认全部折叠（分组、子分组与各节），展开一操作就记进本机的界面状态，重建界面后照旧；新的分组折叠着；删掉的
    /// 分组在下次折叠、展开时从集合中清掉。
    #[gpui_kit::test]
    fn everything_starts_collapsed_and_expansion_is_remembered(cx: &mut TestAppContext) {
        setup(cx);
        let prod = add_group(cx, "生产", None);
        let web = add_group(cx, "web", Some(&prod));
        let web1 = add_host(cx, "web1", Some(&web));
        let loose = add_host(cx, "loose", None);
        cx.update(|cx| {
            AppState::store(cx)
                .create(
                    Workspace {
                        name: "巡检".into(),
                        ..Default::default()
                    },
                    None,
                    Position::Last,
                )
                .unwrap()
        });
        let tree = open_tree(cx);
        let initial = rows(&tree, cx);
        assert!(has(&initial, &RowKey::Workspaces) && has(&initial, &RowKey::Hosts));
        for row in &initial {
            assert!(
                matches!(
                    row.kind,
                    RowKind::Heading {
                        collapsed: true,
                        ..
                    }
                ),
                "默认只有折叠着的各节标题：{row:?}"
            );
        }

        tree.update(cx, |tree, cx| {
            tree.toggle_section(&RowKey::Hosts, cx);
            tree.toggle_group(&prod, cx);
        });
        let opened = rows(&tree, cx);
        assert!(has(&opened, &host_key(&loose)));
        assert!(
            opened.iter().any(|r| r.key == RowKey::Group(web.clone())
                && matches!(
                    r.kind,
                    RowKind::Group {
                        collapsed: true,
                        ..
                    }
                )),
            "子分组默认也折叠"
        );
        assert!(
            !has(&opened, &host_key(&web1)),
            "折叠的子分组里的主机不在行中"
        );
        assert_eq!(
            expanded(cx),
            [HOSTS_SECTION.to_string(), prod.as_str().to_string()].into()
        );

        // 重建界面（另一个窗口的主机树读的是同一份界面状态）
        let rebuilt = open_tree(cx);
        let again = rows(&rebuilt, cx);
        assert!(has(&again, &RowKey::Group(web.clone())) && !has(&again, &host_key(&web1)));

        // 新建的分组折叠着
        let fresh = add_group(cx, "新", None);
        let inside = add_host(cx, "inside", Some(&fresh));
        cx.run_until_parked();
        let now = rows(&tree, cx);
        assert!(has(&now, &RowKey::Group(fresh.clone())));
        assert!(!has(&now, &host_key(&inside)), "新分组折叠着");

        // 删掉的分组：下次折叠、展开时从集合中清掉
        cx.update(|cx| AppState::store(cx).delete::<Group>(&prod).unwrap());
        cx.run_until_parked();
        tree.update(cx, |tree, cx| tree.toggle_group(&fresh, cx));
        assert_eq!(
            expanded(cx),
            [HOSTS_SECTION.to_string(), fresh.as_str().to_string()].into()
        );
    }

    /// 搜索时有匹配的就列出（不看折叠状态），不改记下的折叠状态，搜索时也不能折叠；清空后照旧。
    #[gpui_kit::test]
    fn search_shows_matches_without_touching_the_folds(cx: &mut TestAppContext) {
        setup(cx);
        let prod = add_group(cx, "生产", None);
        let web1 = add_host(cx, "web1", Some(&prod));
        let tree = open_tree(cx);
        let set_query = |query: &'static str, cx: &mut TestAppContext| {
            let window = cx.update(|cx| cx.windows()[0]);
            cx.update_window(window, |_, window, cx| {
                tree.update(cx, |tree, cx| {
                    tree.search
                        .update(cx, |input, cx| input.set_value(query, window, cx));
                    tree.rebuild(cx);
                })
            })
            .unwrap();
            cx.run_until_parked();
        };
        assert!(!has(&rows(&tree, cx), &host_key(&web1)));
        set_query("web", cx);
        assert!(
            has(&rows(&tree, cx), &host_key(&web1)),
            "搜索时列出匹配的主机"
        );
        tree.update(cx, |tree, cx| {
            tree.toggle_section(&RowKey::Local, cx);
            tree.toggle_section(&RowKey::Hosts, cx);
        });
        assert!(expanded(cx).is_empty(), "搜索不改记下的折叠状态");
        set_query("", cx);
        assert!(!has(&rows(&tree, cx), &host_key(&web1)), "清空后回到折叠");
    }

    /// 全部折叠时键盘只在看得到的行之间移动（折叠着的分组里的主机选不到）；→ 展开节与分组（记下），再 → 到第一个
    /// 子项。
    #[gpui_kit::test]
    fn keyboard_moves_over_visible_rows_only(cx: &mut TestAppContext) {
        setup(cx);
        let prod = add_group(cx, "生产", None);
        let web1 = add_host(cx, "web1", Some(&prod));
        // 上面有别的节时才有“主机”标题（本机的终端各平台检测到的不同，另加一个工作区）
        cx.update(|cx| {
            AppState::store(cx)
                .create(
                    Workspace {
                        name: "巡检".into(),
                        ..Default::default()
                    },
                    None,
                    Position::Last,
                )
                .unwrap()
        });
        let tree = open_tree(cx);
        let window = cx.update(|cx| cx.windows()[0]);
        let press = |cx: &mut TestAppContext, key: &str| {
            let key = key.to_string();
            cx.update_window(window, |_, window, cx| {
                tree.update(cx, |tree, cx| match key.as_str() {
                    "down" => tree.on_select_next(&SelectNext, window, cx),
                    "right" => tree.on_expand_or_child(&ExpandOrChild, window, cx),
                    _ => unreachable!(),
                })
            })
            .unwrap();
            cx.update(|cx| tree.read(cx).selected.clone())
        };
        let mut seen = Vec::new();
        for _ in 0..6 {
            seen.push(press(cx, "down"));
        }
        assert!(
            seen.iter().flatten().all(|key| matches!(
                key,
                RowKey::Workspaces | RowKey::Local | RowKey::Favorites | RowKey::Hosts
            )),
            "只选得到各节的标题：{seen:?}"
        );
        cx.update(|cx| tree.update(cx, |tree, cx| tree.select(RowKey::Hosts, cx)));
        press(cx, "right");
        assert_eq!(press(cx, "down"), Some(RowKey::Group(prod.clone())));
        press(cx, "right");
        assert_eq!(
            press(cx, "right"),
            Some(host_key(&web1)),
            "展开后 → 到第一个子项"
        );
        assert!(expanded(cx).contains(prod.as_str()) && expanded(cx).contains(HOSTS_SECTION));
    }

    /// 在折叠着的分组中新建主机：保存后展开它所在的各级分组并选中它。
    #[gpui_kit::test]
    fn saving_a_host_reveals_it_in_its_group(cx: &mut TestAppContext) {
        let handle = crate::testkit::open_workspace(cx);
        crate::i18n::set_language(crate::i18n::Language::ZhCn);
        let prod = add_group(cx, "生产", None);
        let web = add_group(cx, "web", Some(&prod));
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            crate::host_dialog::open_new(Some(web.clone()), window, cx)
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.input("web-01", cx);
            window.click("host-address", cx);
            window.input("10.0.0.5", cx);
            window.click("host-username", cx);
            window.input("deploy", cx);
            window.click("host-save", cx);
        })
        .unwrap();
        cx.run_until_parked();
        let host = cx.update(|cx| AppState::store(cx).list::<Host>()[0].clone());
        assert_eq!(host.parent.as_deref(), Some(web.as_str()));
        let open: Vec<String> = [HOSTS_SECTION, prod.as_str(), web.as_str()]
            .map(str::to_string)
            .into();
        assert!(
            open.iter().all(|key| expanded(cx).contains(key)),
            "展开各级分组"
        );
        let tree = cx
            .update_window(handle, |_, window, cx| {
                crate::Workspace::in_window(window, cx)
                    .unwrap()
                    .read(cx)
                    .host_tree(cx)
            })
            .unwrap();
        cx.update(|cx| {
            let tree = tree.read(cx);
            assert!(has(&tree.rows, &host_key(&host.id)), "新主机在行中");
            assert_eq!(tree.selected, Some(host_key(&host.id)), "选中新主机");
        });
    }

    /// 识别出系统的 SSH 主机：行中带上系统（行首换成标志，悬停提示带上系统名）；Telnet 主机、不认识的键不算。
    /// 写入识别结果后行跟着更新。显示地址时行变高、图标放大，两种都画得出。
    #[gpui_kit::test]
    fn detected_systems_show_on_host_rows(cx: &mut TestAppContext) {
        setup(cx);
        let ssh = add_host(cx, "web", None);
        let telnet = add_host(cx, "switch", None);
        let newer = add_host(cx, "nix", None);
        cx.update(|cx| {
            let store = AppState::store(cx);
            let mut doc = store.get::<Host>(&telnet).unwrap().doc;
            doc.protocol = Protocol::Telnet;
            store.update(&telnet, doc).unwrap();
            store.set_host_os(&telnet, Some(RemoteOs::Debian)).unwrap();
            let mut doc = store.get::<Host>(&newer).unwrap().doc;
            doc.os = Some("nixos".into());
            store.update(&newer, doc).unwrap();
        });
        let tree = open_tree(cx);
        tree.update(cx, |tree, cx| tree.toggle_section(&RowKey::Hosts, cx));
        let os_of = |rows: &[Row], id: &HostId| {
            rows.iter()
                .find(|r| r.key == host_key(id))
                .and_then(|r| match &r.kind {
                    RowKind::Host { os, .. } => Some(*os),
                    _ => None,
                })
                .expect("主机应在行中")
        };
        let before = rows(&tree, cx);
        assert_eq!(os_of(&before, &ssh), None, "还没识别");
        assert_eq!(os_of(&before, &telnet), None, "Telnet 不显示系统");
        assert_eq!(os_of(&before, &newer), None, "不认识的键当作没有识别");

        cx.update(|cx| AppState::store(cx).set_host_os(&ssh, Some(RemoteOs::Ubuntu)))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(os_of(&rows(&tree, cx), &ssh), Some(RemoteOs::Ubuntu));

        for show_address in [true, false] {
            cx.update(|cx| {
                AppState::store(cx).update_settings(|s| s.general.show_host_address = show_address)
            })
            .unwrap();
            cx.run_until_parked();
            let row = rows(&tree, cx)
                .into_iter()
                .find(|r| r.key == host_key(&ssh))
                .unwrap();
            assert!(
                matches!(&row.kind, RowKind::Host { address, .. } if address.is_some() == show_address)
            );
        }
    }

    /// 次要信息（地址、标签、数量）的弱化颜色在侧边栏上也够清楚：浅色、深色主题下都不低于 4.5:1，名称更清楚。
    #[gpui_kit::test]
    fn secondary_text_is_readable_on_the_sidebar(cx: &mut TestAppContext) {
        use gpui_kit::component::{Theme, ThemeMode};
        setup(cx);
        let ratio = |a: gpui_kit::Hsla, b: gpui_kit::Hsla| {
            let a = termlet_core::schemes::Rgb::parse(&crate::schemes::hex_of(a)).unwrap();
            let b = termlet_core::schemes::Rgb::parse(&crate::schemes::hex_of(b)).unwrap();
            a.contrast(b)
        };
        cx.update(|cx| {
            for mode in [ThemeMode::Light, ThemeMode::Dark] {
                Theme::change(mode, None, cx);
                let theme = cx.theme();
                let secondary = ratio(theme.muted_foreground, theme.sidebar);
                let name = ratio(theme.foreground, theme.sidebar);
                assert!(secondary >= 4.5, "{mode:?} 次要信息只有 {secondary:.2}:1");
                assert!(name > secondary, "{mode:?} 名称应比次要信息清楚");
            }
        });
    }

    /// 终端 Tab 菜单的“在侧边栏中显示主机”：侧边栏隐藏着时显示出来，清空搜索，展开所在的分组与“主机”一节，选中它；
    /// 命令面板的动作同样。本地终端的 Tab 菜单没有这一项。
    #[gpui_kit::test]
    fn tab_menu_reveals_the_host_in_the_sidebar(cx: &mut TestAppContext) {
        let handle = crate::testkit::open_workspace(cx);
        crate::i18n::set_language(crate::i18n::Language::ZhCn);
        let prod = add_group(cx, "生产", None);
        // 连不上的端口：打开 Tab 很快失败，不影响菜单
        let host = cx.update(|cx| {
            AppState::store(cx)
                .create(
                    Host {
                        name: "web".into(),
                        address: "127.0.0.1".into(),
                        port: 1,
                        username: "alice".into(),
                        ..Default::default()
                    },
                    Some(prod.to_string()),
                    Position::Last,
                )
                .unwrap()
        });
        let workspace = cx
            .update_window(handle, |_, window, cx| {
                crate::Workspace::in_window(window, cx).unwrap()
            })
            .unwrap();
        let tree = cx.update(|cx| workspace.read(cx).host_tree(cx));
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| w.open_host(&host, window, cx));
            tree.update(cx, |tree, cx| {
                tree.search
                    .update(cx, |input, cx| input.set_value("db", window, cx))
            });
        })
        .unwrap();
        cx.update(|cx| {
            AppState::ui(cx).update(cx, |ui, cx| ui.update(|s| s.sidebar_hidden = true, cx))
        });
        cx.run_until_parked();
        let panel = cx.update(|cx| workspace.read(cx).live_terminals()[0].entity_id());
        let title: gpui_kit::ElementId = ("tab-title", panel.as_u64() as usize).into();
        // 在 Tab 菜单的“主机 ›”中
        let (host_menu, label) = (t!("panel.menu.host"), t!("panel.menu.reveal_host"));
        assert!(crate::testkit::click_menu_in(
            cx,
            handle,
            title,
            Some(&host_menu),
            &label
        ));
        let check = |cx: &mut TestAppContext| {
            assert!(!cx.update(|cx| AppState::ui(cx).read(cx).state().sidebar_hidden));
            assert!(expanded(cx).contains(HOSTS_SECTION) && expanded(cx).contains(prod.as_str()));
            cx.update(|cx| {
                let tree = tree.read(cx);
                assert!(tree.search.read(cx).value().is_empty(), "清空了搜索");
                assert!(has(&tree.rows, &host_key(&host)));
                assert_eq!(tree.selected, Some(host_key(&host)));
            });
        };
        check(cx);

        // 命令面板的动作：折叠起来、取消选中后再显示
        cx.update(|cx| {
            tree.update(cx, |tree, cx| {
                tree.set_folded(&[HOSTS_SECTION, prod.as_str()], true, cx);
                tree.selected = None;
            })
        });
        cx.update_window(handle, |_, window, cx| {
            window.dispatch_action(Box::new(crate::workspace::RevealHostInSidebar), cx);
        })
        .unwrap();
        cx.run_until_parked();
        check(cx);
    }
}
