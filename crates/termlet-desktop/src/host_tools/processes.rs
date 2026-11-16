//! 进程页：读 /proc 得到的进程列表，显示时每 5 秒刷新（鼠标停在列表上、右键菜单或确认框打开时暂停，
//! 避免行跳动后点错进程），可排序、搜索；行首的小图标标出这个终端自己的 shell 及其子进程；选中后在底部显示详情
//! （头部一行：名称 ● 状态 · 结束 / 强制结束 · ⋯，与列表之间的分隔条可拖动）；结束（TERM）、强制结束（KILL），
//! 按风险写确认文案。选中的进程被搜索、筛选藏起来时取消选中，详情只留列表中看得到的进程。
//!
//! - 归属：行上标出所在的容器与所属的 systemd 服务（读 `/proc/<pid>/cgroup`，见核心的 `hosttools::cgroup`）；名称列
//!   优先，列表窄时标签只留图标（悬停看全名）。容器名取 Docker 页的读数，没有时这一页自己读一次 `docker ps`（只在有
//!   带容器归属的进程时；经 sudo 与否与 Docker 页的判断相同），都没有时写短 ID。可以隐藏容器内的进程，或者只看一个
//!   容器内的进程（容器详情的“查看进程”），筛选条件显示为可清除的小标签。
//! - 权限：信息行尾标出这条连接上的权限（有免密码 sudo 时“sudo”，没有时锁图标，点开说明）；没有 sudo 时结束别的
//!   用户的进程的菜单项与按钮带锁图标（照常尝试，失败时说明）。
//! - 跳转：详情与右键菜单中可以跳到所属的服务、所属的容器、这个进程的网络连接；从别的页跳过来的进程已不在列表中时说明。
//! - 列：CPU 带 %（单核为 100%，表头悬停有说明）；列表够宽时（[`WIDE_FROM`]）另有命令行一列（次要文字色，截断，悬停
//!   看全文）。可以合并同名进程（工具条上的开关，默认关）：同名的合成一行，写出个数与 CPU、内存的合计，点开展开。
//! - 树形（工具条上的开关，默认关；与合并同名进程只能选一个，打开一个时关掉另一个）：按父子关系缩进，有子进程的行首
//!   有 ▸ / ▾，点了收起、展开（收起时选中的子进程换成它）；排序在同一层内进行。父进程不在列表中、比子进程晚启动（PID
//!   被重新使用）的进程在最上层，读取不一致连成的环在 PID 最小处断开（见 [`super::model::process_parents`]）。搜索与
//!   筛选时列出命中的进程与它们的各级父进程（父进程只是为了看出位置，用次要文字色，不算在个数中），命中的进程所在的
//!   分支自动展开。收起了哪些按进程（PID 与启动时刻）记着，刷新后照旧。
//! - 按用户筛选：工具条上的下拉菜单列出现有的用户与各自的进程数，选中后筛选条件显示为可清除的小标签；右键菜单中
//!   “只看这个用户的进程”同样。
//! - 行上的操作：鼠标停在行上（或键盘焦点所在的行）时右端浮出“结束”与 ⋯（与右键菜单相同）；结束进行中时那里转圈并写
//!   “正在结束…”，这一行的操作不能点，完成后写“已结束”，随后重读。
//! - 键盘（见 [`super::keys`]）：↑ ↓ 等移动并选中（详情稍后再读，按住方向键时不逐个读），Enter 重读详情（在合并的
//!   一行上展开、收起），← → 在树形与合并的一组中收起、展开（已收起时到父进程，已展开时到第一个子进程），Delete 结束，
//!   Shift+F10 打开右键菜单。

use std::cell::Cell;
use std::collections::{BTreeSet, HashSet};
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, StyledExt as _,
    VirtualListScrollHandle, h_flex, v_flex, v_virtual_list,
};
use gpui_kit::{
    AnyElement, App, Context, ElementId, Entity, EntityId, FocusHandle, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, Pixels, Render, ScrollStrategy, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, TestSupportExt as _, Window,
    canvas, div, prelude::FluentBuilder as _, px, rems, size,
};
use termlet_core::hosttools::{
    Action, ContainerNames, DockerInfo, ExecError, PROCESS_INTERVAL, Privilege, Process,
    ProcessDetail, ProcessList, Signal, SudoState, Target, ToolsError,
};

use super::keys::{
    CollapseItem, DeleteItem, ExpandItem, LIST_CONTEXT, OpenItem, OpenMenu, SelectFirst,
    SelectLast, SelectNext, SelectPageDown, SelectPageUp, SelectPrev,
};
use super::model::{
    ConnChip, Page, ProcFilter, ProcGroup, ProcKey, ProcSort, ProcessRisk, SortOrder, Step,
    TreeRow, ancestors, container_label, cpu_pending, descendants, fill_cpu, filter_processes,
    format_cpu, group_processes, page_rows, proc_key, process_parents, process_risk, process_tree,
    process_user, process_users, service_name, sort_processes, step_row,
};
use super::panel::{ActRequest, HostToolsPanel};
use super::shared::{
    self, Feed, KeyMenu, OpVerb, PageContext, RootScope, RowOps, Tone, ToolPage, action_item, dot,
    error_line, info_line, loading_text, notice, op_button,
};
use crate::prose::prose;
use crate::sftp::list_model::{format_size, visible_name};
use crate::t;

/// 结束进程之后等一会儿再重读（进程退出需要时间）。
const AFTER_KILL: Duration = Duration::from_millis(800);
/// 详情中最多列出的子进程数。
const MAX_CHILDREN: usize = 20;
/// 用键盘移动选中的进程后，停这么久再读它的详情（按住方向键时不逐个读）。
const KEY_DETAIL_DELAY: Duration = Duration::from_millis(250);
/// 行高（rem）。
const ROW_HEIGHT: f32 = 1.5;

/// 次要的列在列表宽度够时才显示（rem，量的是卡片里列表的宽度：行的左右各留 0.25rem，列之间 0.375rem）。各个界限
/// 让名称一列在这一列出现时与列表放进卡片之前一样宽（用户一列出现时 5rem）：默认宽度的面板中照样有用户一列。
const USER_COLUMN_FROM: f32 = 23.;
const STATE_COLUMN_FROM: f32 = 27.;
/// 列表宽度够时归属标签写出容器名、服务名，否则只留图标。
const TAG_TEXT_FROM: f32 = 29.;
/// 列表宽度够时另有命令行一列（rem；服务页、Docker 页的“宽面板”也按它）。
pub(super) const WIDE_FROM: f32 = 35.5;
/// CPU 一列的宽度（rem）：放得下“100.0%”。
const CPU_WIDTH: f32 = 3.5;
/// 树形中每一层缩进的宽度（rem；列表窄、不显示用户一列时用小的），最多缩进这么多层（再深的与它对齐，名称不被挤掉）。
const TREE_STEP: f32 = 0.625;
const TREE_STEP_NARROW: f32 = 0.5;
const TREE_MAX_DEPTH: usize = 8;
/// 树形时次要的列（用户、状态、命令行等）晚这么多（rem）才显示：名称一列要放缩进，默认宽度的面板中名称不被截短。
const TREE_COLUMN_SHIFT: f32 = 6.;
/// 用户筛选的下拉菜单最高的高度（rem），用户多时在里面滚动。
const USER_MENU_HEIGHT: f32 = 20.;

/// 显示哪些次要的列，归属标签是否写出名称。
#[derive(Clone, Copy)]
struct Columns {
    user: bool,
    state: bool,
    tag_text: bool,
    command: bool,
}

/// 列表的一行：一个进程（在列表中的位置；`nested` 为展开的一组中的成员），合并的同名进程的一组（两个以上），或树形
/// 中的一个进程。
#[derive(Clone)]
enum ProcRow {
    Process { ix: usize, nested: bool },
    Group(Rc<ProcGroup>),
    Tree(TreeRow),
}

impl ProcRow {
    fn pid(&self, data: &ProcessList) -> Option<u32> {
        match self {
            ProcRow::Process { ix, .. } => Some(data.processes[*ix].pid),
            ProcRow::Tree(row) => Some(data.processes[row.ix].pid),
            ProcRow::Group(_) => None,
        }
    }

    fn group(&self) -> Option<&str> {
        match self {
            ProcRow::Group(group) => Some(group.name.as_str()),
            ProcRow::Process { .. } | ProcRow::Tree(_) => None,
        }
    }
}

/// 行首在名称一列里面的缩进：没有、合并的一组中的成员，或树形中的一层层（与 ▸ / ▾）。
#[derive(Clone, Copy)]
enum Indent {
    None,
    Nested,
    Tree(TreeRow),
}

/// 列表的显示方式（工具条上的两个开关只能打开一个）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(super) enum ProcView {
    #[default]
    List,
    /// 合并同名进程
    Merged,
    /// 按父子关系的树形
    Tree,
}

/// 筛选、排序、合并的条件：读数、搜索、排序、筛选、显示方式与展开、收起了哪些（每次展开、收起加一）。
type RowsKey = (
    Rc<ProcessList>,
    String,
    SortOrder,
    ProcFilter,
    ProcView,
    u64,
);

pub(super) struct ProcessPage {
    context: PageContext,
    feed: Feed<ProcessList>,
    search: Entity<InputState>,
    sort: SortOrder,
    /// 搜索之外的筛选（隐藏容器内的进程、只看一个容器内的进程）
    filter: ProcFilter,
    /// 只看一个容器时，筛选条件上显示的容器名
    container_name: Option<String>,
    /// 筛选、排序（合并、树形）后的行、命中搜索与筛选的进程数（树形中为了看出位置而列出的父进程不算）与它们对应的
    /// 条件
    rows: Rc<Vec<ProcRow>>,
    rows_matched: usize,
    rows_key: Option<RowsKey>,
    /// 显示方式（合并同名进程、树形：工具条上的开关），合并时展开了的组（进程名）、树形中收起了的进程，与它们的
    /// 变化次数
    view: ProcView,
    expanded: HashSet<String>,
    collapsed: HashSet<ProcKey>,
    fold_gen: u64,
    /// 键盘移到的合并的一行（这时没有选中的进程）
    group_cursor: Option<String>,
    selected: Option<u32>,
    /// 选中进程的详情：选中时读一次，之后不随刷新更新
    detail: Option<(u32, Option<Result<ProcessDetail, ToolsError>>)>,
    /// 从别处跳过来、数据还没读到时要选中的进程
    focus_pending: Option<u32>,
    hovering: bool,
    /// 打开着的右键菜单（它关闭或被释放时才恢复刷新；换了一行的菜单时不被前一个的关闭误清）
    menu_open: Option<EntityId>,
    /// 右键菜单打开着的那一行（右键不改变选中：选中会让详情出现、把列表挤矮，按下位置所在的行被挤开后
    /// gpui-kit 就画不出它的菜单）：菜单打开期间这一行高亮
    menu_row: Option<u32>,
    /// 鼠标停在哪一行上（右端浮出这一行的操作）
    hover_row: Option<u32>,
    /// 各行进行中与刚完成的结束进程
    ops: RowOps<u32>,
    confirming: bool,
    cpu_retried: bool,
    scroll: VirtualListScrollHandle,
    /// 上一帧列表的宽度（决定显示哪些列）
    width: Rc<Cell<Pixels>>,
    /// 列表的焦点（键盘操作）
    focus: FocusHandle,
    key_menu: KeyMenu,
    /// 列表有焦点时滚动跟随到的当前行（进程与它在列表中的位置；刷新后位置变了再跟过去）
    followed: Option<(u32, usize)>,
    /// 从别的页跳过来的进程不在列表中时的说明
    note: Option<SharedString>,
    /// 这一页自己读到的容器名（还没有 Docker 页的读数时）
    names: Option<Rc<ContainerNames>>,
    /// 上次为哪些不知道名称的容器读过（同一组只读一次）
    names_asked: Option<BTreeSet<String>>,
    /// 读不到容器名（没有 Docker、没有权限）：不再读
    names_off: bool,
    /// 详情区顶上的分隔条（键盘调整高度）与详情这一帧的高度
    splitter: FocusHandle,
    detail_height: Rc<Cell<Pixels>>,
    _detail: Option<Task<()>>,
    _cpu_retry: Option<Task<()>>,
    _after_kill: Option<Task<()>>,
    _names: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ToolPage for ProcessPage {
    type Data = ProcessList;

    fn feed(&mut self) -> &mut Feed<ProcessList> {
        &mut self.feed
    }

    fn context(&self) -> &PageContext {
        &self.context
    }

    fn is_paused(&self) -> bool {
        self.hovering || self.menu_open.is_some() || self.confirming
    }

    fn after_load(&mut self, cx: &mut Context<Self>) {
        if self.feed.data().is_some()
            && let Some(pid) = self.focus_pending.take()
        {
            self.reveal(pid, cx);
        }
        self.drop_hidden(cx);
        self.sync_names(cx);
        self.fill_paused_cpu();
        self.retry_cpu(cx);
        self.sync_poll(cx);
    }
}

impl ProcessPage {
    pub fn new(context: PageContext, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = shared::search_input(|| t!("tools.process.search"), window, cx);
        let subscriptions = vec![cx.subscribe(&search, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.unfold_matches(cx);
                this.drop_hidden(cx);
                cx.notify();
            }
        })];
        Self {
            feed: Feed::new(&context),
            context,
            search,
            sort: SortOrder::default(),
            filter: ProcFilter::default(),
            container_name: None,
            rows: Rc::default(),
            rows_matched: 0,
            rows_key: None,
            view: ProcView::List,
            expanded: HashSet::new(),
            collapsed: HashSet::new(),
            fold_gen: 0,
            group_cursor: None,
            selected: None,
            detail: None,
            focus_pending: None,
            hovering: false,
            menu_open: None,
            menu_row: None,
            hover_row: None,
            ops: RowOps::default(),
            confirming: false,
            cpu_retried: false,
            scroll: VirtualListScrollHandle::new(),
            width: Rc::new(Cell::new(px(0.))),
            focus: shared::list_focus(cx),
            key_menu: KeyMenu::default(),
            followed: None,
            note: None,
            names: None,
            names_asked: None,
            names_off: false,
            splitter: cx.focus_handle().tab_stop(true),
            detail_height: Rc::new(Cell::new(px(0.))),
            _detail: None,
            _cpu_retry: None,
            _after_kill: None,
            _names: None,
            _subscriptions: subscriptions,
        }
    }

    /// 选中的进程被搜索、筛选藏起来（或已不在列表中）时取消选中：详情只留列表中看得到的进程。
    fn drop_hidden(&mut self, cx: &mut Context<Self>) {
        let Some(data) = self.feed.data() else {
            return;
        };
        let rows = self.rows(&data, cx);
        if let Some(name) = &self.group_cursor
            && !rows.iter().any(|r| r.group() == Some(name.as_str()))
        {
            self.group_cursor = None;
            cx.notify();
        }
        let Some(pid) = self.selected else {
            return;
        };
        if !rows.iter().any(|r| r.pid(&data) == Some(pid)) {
            self.selected = None;
            self.detail = None;
            self._detail = None;
            self.followed = None;
            cx.notify();
        }
    }

    /// 进程所在容器的名称：有带容器归属的进程，而 Docker 页的读数与上次读到的名称中都没有它们时，读一次
    /// `docker ps`（经 sudo 与否与 Docker 页的判断相同）。同一组不知道名称的容器只读一次；没有 Docker、没有权限时
    /// 不再读（写短 ID）。
    fn sync_names(&mut self, cx: &mut Context<Self>) {
        if self.names_off || self._names.is_some() {
            return;
        }
        let Some(data) = self.feed.data() else {
            return;
        };
        let docker = shared::docker_cache(&self.context);
        let unknown: BTreeSet<String> = data
            .processes
            .iter()
            .filter_map(|p| p.container.clone())
            .filter(|id| {
                let in_docker = docker
                    .as_ref()
                    .is_some_and(|info| info.containers.iter().any(|c| &c.id == id));
                let named = self
                    .names
                    .as_ref()
                    .is_some_and(|names| names.names.contains_key(id));
                !in_docker && !named
            })
            .collect();
        if unknown.is_empty() || self.names_asked.as_ref() == Some(&unknown) {
            return;
        }
        let Some(target) = self.context.target.clone() else {
            return;
        };
        self.names_asked = Some(unknown);
        let tools = self.context.tools.clone();
        self._names = Some(cx.spawn(async move |this, cx| {
            let result = tools.container_names(&target).await;
            let _ = this.update(cx, |page, cx| {
                page._names = None;
                match result {
                    Ok(names) => page.names = Some(Rc::new(names)),
                    // 连接断开、超时等：等不知道名称的容器有了变化再读
                    Err(ToolsError::Exec(_)) => {}
                    Err(error) => {
                        tracing::debug!("主机工具：读不到容器名，进程页写短 ID：{error}");
                        page.names_off = true;
                    }
                }
                cx.notify();
            });
        }));
    }

    /// 容器的名称（Docker 页的读数优先，其次是这一页读到的，都没有时为短 ID）。
    fn container_name(&self, id: &str, docker: Option<&DockerInfo>) -> String {
        container_label(id, docker, self.names.as_ref().map(|n| &n.names))
    }

    pub fn set_connection(&mut self, target: Option<Target>, cx: &mut Context<Self>) {
        let reconnected = self.context.target.is_none() && target.is_some();
        self.context.target = target;
        // 重连中选中的进程：详情还没读到，连上后补读
        if reconnected && let Some((pid, None)) = self.detail {
            self.load_detail(pid, None, cx);
        }
    }

    pub fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if shared::activate(self, active, cx) && active {
            self.cpu_retried = false;
        }
        if !active {
            self._cpu_retry = None;
            // 不显示时收不到鼠标移出、菜单关闭：暂停的标志随之复位，再显示时照常刷新
            self.hovering = false;
            self.menu_open = None;
            self.menu_row = None;
            self.hover_row = None;
        }
    }

    /// 定时刷新：显示中时进行（第一次读完之后开始；暂停期间到点时跳过）；恢复时先显示暂停期间读到的结果。
    fn sync_poll(&mut self, cx: &mut Context<Self>) {
        if !self.is_paused() {
            shared::resume(self, cx);
        }
        // 不支持的主机不再自动刷新
        let unsupported = matches!(
            self.feed.error,
            Some(ToolsError::Unsupported | ToolsError::Exec(ExecError::SingleChannel))
        );
        if unsupported {
            shared::stop_poll(self);
        } else if self.feed.active && !self.feed.is_polling() && !self.feed.loading {
            shared::start_poll(self, PROCESS_INTERVAL, cx);
        }
    }

    fn set_hovering(&mut self, hovering: bool, cx: &mut Context<Self>) {
        if self.hovering != hovering {
            self.hovering = hovering;
            if !hovering {
                self.hover_row = None;
            }
            self.sync_poll(cx);
            cx.notify();
        }
    }

    fn set_hover_row(&mut self, pid: u32, hovered: bool, cx: &mut Context<Self>) {
        if shared::hover_row(&mut self.hover_row, pid, hovered) {
            cx.notify();
        }
    }

    fn set_menu_open(&mut self, menu: EntityId, pid: u32, cx: &mut Context<Self>) {
        if self.menu_open != Some(menu) {
            self.menu_open = Some(menu);
            self.menu_row = Some(pid);
            self.sync_poll(cx);
            cx.notify();
        }
    }

    fn menu_closed(&mut self, menu: EntityId, cx: &mut Context<Self>) {
        if self.menu_open == Some(menu) {
            self.menu_open = None;
            self.menu_row = None;
            self.sync_poll(cx);
            cx.notify();
        }
    }

    fn set_confirming(&mut self, confirming: bool, cx: &mut Context<Self>) {
        if self.confirming != confirming {
            self.confirming = confirming;
            self.sync_poll(cx);
            cx.notify();
        }
    }

    /// 全部进程都没有 CPU 读数（第一次读取，或上一次读数太旧、不拿来算，例如暂停刷新很久之后）：稍后补读一次；
    /// 补读之后仍然没有时不再补读，等读到 CPU 之后才重新计。
    /// 暂停期间（鼠标停在列表上等）补读到了 CPU：就地填进显示着的行，不重新排序、不增删行（免得行在鼠标下跳动）；
    /// 完整的结果仍等恢复刷新时显示。排序、筛选的缓存改记在填好的这一份上，行的顺序沿用。
    fn fill_paused_cpu(&mut self) {
        let (Some(shown), Some(fresh)) = (self.feed.data(), self.feed.shown.pending()) else {
            return;
        };
        let Some(filled) = fill_cpu(&shown, fresh) else {
            return;
        };
        let filled = Rc::new(filled);
        if let Some(key) = self
            .rows_key
            .as_mut()
            .filter(|key| Rc::ptr_eq(&key.0, &shown))
        {
            key.0 = filled.clone();
        }
        self.feed.shown.patch(filled);
    }

    fn retry_cpu(&mut self, cx: &mut Context<Self>) {
        let Some(data) = self.feed.data() else {
            return;
        };
        if !cpu_pending(&data) {
            self.cpu_retried = false;
            return;
        }
        if self.cpu_retried || !self.feed.active {
            return;
        }
        self.cpu_retried = true;
        self._cpu_retry = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(shared::CPU_RETRY).await;
            let _ = this.update(cx, |page, cx| {
                if page.feed.active {
                    shared::refresh(page, false, cx);
                }
            });
        }));
    }

    /// 筛选、排序（合并、树形）后的行：数据、搜索、排序、筛选、显示方式或展开、收起变了时重新计算。合并时只有一个的
    /// 照常是一行，两个以上的合成一行，展开时成员跟在下面；树形见 [`process_tree`]。
    fn rows(&mut self, data: &Rc<ProcessList>, cx: &App) -> Rc<Vec<ProcRow>> {
        let query = self.search.read(cx).value().to_string();
        let fresh = self
            .rows_key
            .as_ref()
            .is_some_and(|(shown, q, sort, filter, view, fold)| {
                Rc::ptr_eq(shown, data)
                    && *q == query
                    && *sort == self.sort
                    && *filter == self.filter
                    && *view == self.view
                    && *fold == self.fold_gen
            });
        if !fresh {
            let mut ixs = filter_processes(&data.processes, &query, &self.filter);
            self.rows_matched = ixs.len();
            let rows = match self.view {
                ProcView::Merged => {
                    sort_processes(&data.processes, &mut ixs, self.sort);
                    let mut rows = Vec::new();
                    for group in group_processes(&data.processes, &ixs, self.sort) {
                        if let [ix] = group.members[..] {
                            rows.push(ProcRow::Process { ix, nested: false });
                            continue;
                        }
                        let open = self.expanded.contains(&group.name);
                        let members = group.members.clone();
                        rows.push(ProcRow::Group(Rc::new(group)));
                        if open {
                            rows.extend(
                                members
                                    .into_iter()
                                    .map(|ix| ProcRow::Process { ix, nested: true }),
                            );
                        }
                    }
                    rows
                }
                ProcView::Tree => {
                    let parents = process_parents(&data.processes);
                    process_tree(&data.processes, &parents, &ixs, self.sort, &self.collapsed)
                        .into_iter()
                        .map(ProcRow::Tree)
                        .collect()
                }
                ProcView::List => {
                    sort_processes(&data.processes, &mut ixs, self.sort);
                    ixs.into_iter()
                        .map(|ix| ProcRow::Process { ix, nested: false })
                        .collect()
                }
            };
            self.rows = Rc::new(rows);
            self.rows_key = Some((
                data.clone(),
                query,
                self.sort,
                self.filter.clone(),
                self.view,
                self.fold_gen,
            ));
        }
        self.rows.clone()
    }

    /// 切换显示方式（工具条上的合并、树形两个开关，打开一个时关掉另一个；再点一下回到列表）：合并时各组都收起，树形时
    /// 全部展开（命中搜索与筛选的分支总是展开）；选中的进程在收起的一组中时取消选中。
    fn set_view(&mut self, view: ProcView, cx: &mut Context<Self>) {
        self.view = view;
        self.expanded.clear();
        self.collapsed.clear();
        self.fold_gen += 1;
        self.group_cursor = None;
        self.drop_hidden(cx);
        cx.notify();
    }

    /// 展开、收起合并的一组（点那一行、在它上面按 Enter 或 ← →）。
    fn toggle_group(&mut self, name: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(name) {
            self.expanded.insert(name.to_string());
        }
        self.fold_gen += 1;
        self.drop_hidden(cx);
        cx.notify();
    }

    /// 树形中收起、展开一个进程的子进程（点行首的 ▸ / ▾、← →）。收起时选中的进程在其中的，改为选中它。
    fn set_collapsed(&mut self, pid: u32, collapse: bool, cx: &mut Context<Self>) {
        let Some(data) = self.feed.data() else {
            return;
        };
        let Some(ix) = data.processes.iter().position(|p| p.pid == pid) else {
            return;
        };
        let key = proc_key(&data.processes[ix]);
        let changed = if collapse {
            self.collapsed.insert(key)
        } else {
            self.collapsed.remove(&key)
        };
        if !changed {
            return;
        }
        if collapse
            && let Some(selected) = self.selected.filter(|s| *s != pid)
            && let Some(at) = data.processes.iter().position(|p| p.pid == selected)
            && ancestors(&process_parents(&data.processes), &[at]).contains(&ix)
        {
            self.select(pid, cx);
        }
        self.fold_gen += 1;
        self.drop_hidden(cx);
        cx.notify();
    }

    /// 树形中搜索、筛选变了：命中的进程所在的分支都展开（不被收起的父进程藏起来）。
    fn unfold_matches(&mut self, cx: &App) {
        if self.view != ProcView::Tree || self.collapsed.is_empty() {
            return;
        }
        let Some(data) = self.feed.data() else {
            return;
        };
        let query = self.search.read(cx).value().to_string();
        if query.trim().is_empty() && self.filter == ProcFilter::default() {
            return;
        }
        let matched = filter_processes(&data.processes, &query, &self.filter);
        let parents = process_parents(&data.processes);
        let before = self.collapsed.len();
        for ix in ancestors(&parents, &matched) {
            self.collapsed.remove(&proc_key(&data.processes[ix]));
        }
        if self.collapsed.len() != before {
            self.fold_gen += 1;
        }
    }

    /// 进程所在的一组（合并时）或它的各级父进程（树形时）收起着：展开它们（跳到这个进程时）。
    fn expand_for(&mut self, pid: u32) {
        let Some(data) = self.feed.data() else {
            return;
        };
        let Some(ix) = data.processes.iter().position(|p| p.pid == pid) else {
            return;
        };
        match self.view {
            ProcView::Merged => {
                if self.expanded.insert(data.processes[ix].name.clone()) {
                    self.fold_gen += 1;
                }
            }
            ProcView::Tree => {
                let before = self.collapsed.len();
                for parent in ancestors(&process_parents(&data.processes), &[ix]) {
                    self.collapsed.remove(&proc_key(&data.processes[parent]));
                }
                if self.collapsed.len() != before {
                    self.fold_gen += 1;
                }
            }
            ProcView::List => {}
        }
    }

    /// 按用户筛选（工具条上的下拉菜单、右键菜单的“只看这个用户的进程”；`None` 为全部用户）。
    fn set_user(&mut self, user: Option<String>, cx: &mut Context<Self>) {
        self.filter.user = user;
        self.unfold_matches(cx);
        self.drop_hidden(cx);
        if self.selected.is_some() {
            self.scroll_to_selected(cx);
        } else {
            self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        }
        cx.notify();
    }

    /// 列表的焦点（面板切到这一页、跳过来时放进来）。
    pub fn list_focus(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// 搜索框（列表显示着时才有：还在读取、不可用时没有搜索框）。
    pub fn search(&self) -> Option<Entity<InputState>> {
        self.feed.data().map(|_| self.search.clone())
    }

    fn on_sort(&mut self, key: ProcSort, cx: &mut Context<Self>) {
        self.sort = self.sort.toggled(key);
        cx.notify();
    }

    /// 选中一个进程，读一次它的详情。
    fn select(&mut self, pid: u32, cx: &mut Context<Self>) {
        self.select_after(pid, None, cx);
    }

    /// 选中一个进程，`delay` 之后读它的详情（键盘移动时：停下来才读）。
    fn select_after(&mut self, pid: u32, delay: Option<Duration>, cx: &mut Context<Self>) {
        self.note = None;
        self.group_cursor = None;
        if self.selected == Some(pid) {
            return;
        }
        self.selected = Some(pid);
        self.load_detail(pid, delay, cx);
        self.scroll_to_selected(cx);
    }

    /// 把选中的进程滚进可见范围：详情出现、读到内容变高后列表变矮，点中的行可能被挤到详情下面
    /// （下一帧布局时按那时的高度滚；列表没有焦点时 [`Self::follow_selected`] 不管）。
    fn scroll_to_selected(&mut self, cx: &App) {
        let (Some(data), Some(pid)) = (self.feed.data(), self.selected) else {
            return;
        };
        let rows = self.rows(&data, cx);
        if let Some(ix) = rows.iter().position(|r| r.pid(&data) == Some(pid)) {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
            self.followed = Some((pid, ix));
        }
    }

    /// 读一次 `pid` 的详情（`delay` 之后）；没有连接时等连上后再读（[`Self::set_connection`]）。
    fn load_detail(&mut self, pid: u32, delay: Option<Duration>, cx: &mut Context<Self>) {
        self.detail = Some((pid, None));
        self._detail = None;
        if let Some(target) = self.context.target.clone() {
            let tools = self.context.tools.clone();
            self._detail = Some(cx.spawn(async move |this, cx| {
                if let Some(delay) = delay {
                    cx.background_executor().timer(delay).await;
                }
                let result = tools.process_detail(&target, pid).await;
                let _ = this.update(cx, |page, cx| {
                    if page.selected == Some(pid) {
                        page.detail = Some((pid, Some(result)));
                        page.scroll_to_selected(cx);
                        cx.notify();
                    }
                });
            }));
        }
        cx.notify();
    }

    /// 详情中的父进程、子进程：选中并滚到它；被搜索、筛选藏起来时先清除它们（否则选中之后又被取消）。
    fn jump_to(&mut self, pid: u32, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(data) = self.feed.data() {
            let rows = self.rows(&data, cx);
            if !rows.iter().any(|r| r.pid(&data) == Some(pid)) {
                self.search
                    .update(cx, |search, cx| search.set_value("", window, cx));
                self.filter = ProcFilter::default();
                self.container_name = None;
                self.expand_for(pid);
            }
        }
        self.jump(pid, cx);
    }

    /// 选中并滚到这一行；之后的刷新不再跟着跳。
    fn jump(&mut self, pid: u32, cx: &mut Context<Self>) {
        self.select(pid, cx);
        if let Some(data) = self.feed.data() {
            let rows = self.rows(&data, cx);
            if let Some(ix) = rows.iter().position(|r| r.pid(&data) == Some(pid)) {
                self.scroll.scroll_to_item(ix, ScrollStrategy::Center);
                self.followed = Some((pid, ix));
            }
        }
    }

    /// 从别的页跳过来的进程：在列表中时选中（被筛选条件藏起来时先清除筛选）；不在显示着的列表（可能是缓存）中而
    /// 正在读取时读到后再找，已不在时说明。
    fn reveal(&mut self, pid: u32, cx: &mut Context<Self>) {
        let Some(data) = self.feed.data() else {
            return;
        };
        let Some(process) = data.processes.iter().find(|p| p.pid == pid) else {
            if self.feed.loading {
                self.focus_pending = Some(pid);
                return;
            }
            self.selected = None;
            self.detail = None;
            self.note = Some(t!("tools.jump.process_gone", pid = pid));
            cx.notify();
            return;
        };
        if !self.filter.matches(process) {
            self.filter = ProcFilter::default();
            self.container_name = None;
        }
        self.expand_for(pid);
        self.jump(pid, cx);
    }

    /// 从别的页跳过来：清空搜索，选中并滚到这一行；还没有数据时读到后再选中。
    pub fn focus_pid(&mut self, pid: u32, window: &mut Window, cx: &mut Context<Self>) {
        self.search
            .update(cx, |search, cx| search.set_value("", window, cx));
        self.rows_key = None;
        self.note = None;
        if self.feed.data().is_some() {
            self.reveal(pid, cx);
        } else {
            self.focus_pending = Some(pid);
        }
    }

    /// 只列出这个容器内的进程（容器详情的“查看进程”）：清空搜索，筛选条件显示为小标签。
    pub fn filter_container(
        &mut self,
        id: String,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search
            .update(cx, |search, cx| search.set_value("", window, cx));
        // 选中的进程不在这个容器中时取消选中：详情不留着一个列表中看不到的进程
        let inside = self.selected.is_some_and(|pid| {
            self.feed.data().is_some_and(|data| {
                data.processes
                    .iter()
                    .any(|p| p.pid == pid && p.container.as_ref() == Some(&id))
            })
        });
        if !inside {
            self.selected = None;
            self.detail = None;
            self._detail = None;
        }
        self.filter = ProcFilter {
            container: Some(id),
            ..ProcFilter::default()
        };
        self.container_name = Some(name);
        self.unfold_matches(cx);
        self.focus_pending = None;
        self.note = None;
        self.followed = None;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    /// 隐藏 / 显示容器内的进程（工具条上的开关；只看一个容器时先清除它）。
    fn set_hide_containers(&mut self, hide: bool, cx: &mut Context<Self>) {
        let hidden = hide
            && self.selected.is_some_and(|pid| {
                self.feed.data().is_some_and(|data| {
                    data.processes
                        .iter()
                        .any(|p| p.pid == pid && p.container.is_some())
                })
            });
        if hidden {
            self.selected = None;
            self.detail = None;
            self._detail = None;
        }
        self.filter.hide_containers = hide;
        self.filter.container = None;
        self.container_name = None;
        self.unfold_matches(cx);
        cx.notify();
    }

    fn clear_container(&mut self, cx: &mut Context<Self>) {
        self.filter.container = None;
        self.container_name = None;
        cx.notify();
    }

    // ------------------------------------------------------------ 键盘

    /// ↑ ↓ 等：移动并选中，滚动跟随（详情停下来才读）。
    fn move_cursor(&mut self, step: Step, window: &mut Window, cx: &mut Context<Self>) {
        let Some(data) = self.feed.data() else {
            return;
        };
        let rows = self.rows(&data, cx);
        let current = match (&self.group_cursor, self.selected) {
            (Some(name), _) => rows.iter().position(|r| r.group() == Some(name.as_str())),
            (None, Some(pid)) => rows.iter().position(|r| r.pid(&data) == Some(pid)),
            (None, None) => None,
        };
        let row = f32::from(rems(ROW_HEIGHT).to_pixels(window.rem_size()));
        let page = page_rows(f32::from(self.scroll.bounds().size.height), row);
        let Some(ix) = step_row(current, rows.len(), step, page, |_| true) else {
            return;
        };
        self.cursor_to(&data, &rows, ix, cx);
    }

    /// 当前行移到第 `row` 行并选中（合并的一行没有选中的进程），滚动跟随。
    fn cursor_to(
        &mut self,
        data: &ProcessList,
        rows: &[ProcRow],
        row: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = rows.get(row) else {
            return;
        };
        match target {
            ProcRow::Group(group) => self.focus_group(group.name.clone(), cx),
            _ => {
                if let Some(pid) = target.pid(data) {
                    self.select_after(pid, Some(KEY_DETAIL_DELAY), cx);
                    self.followed = Some((pid, row));
                }
            }
        }
        self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
        cx.notify();
    }

    /// ←：树形中收起当前行（有子进程且展开着时），否则移到父进程；合并时收起当前的一组，在展开的一组的成员上时移到
    /// 那一组。
    fn collapse_item(&mut self, cx: &mut Context<Self>) {
        let Some(data) = self.feed.data() else {
            return;
        };
        let rows = self.rows(&data, cx);
        if let Some(name) = self.group_cursor.clone() {
            if self.expanded.contains(&name) {
                self.toggle_group(&name, cx);
            }
            return;
        }
        let Some(pid) = self.selected else {
            return;
        };
        let Some(at) = rows.iter().position(|r| r.pid(&data) == Some(pid)) else {
            return;
        };
        match &rows[at] {
            ProcRow::Tree(row) if row.children > 0 && !row.collapsed => {
                self.set_collapsed(pid, true, cx)
            }
            ProcRow::Tree(row) => {
                let parent = row.parent.and_then(|parent| {
                    rows.iter()
                        .position(|r| matches!(r, ProcRow::Tree(t) if t.ix == parent))
                });
                if let Some(parent) = parent {
                    self.cursor_to(&data, &rows, parent, cx);
                }
            }
            ProcRow::Process { nested: true, ix } => {
                let name = data.processes[*ix].name.as_str();
                if let Some(group) = rows.iter().position(|r| r.group() == Some(name)) {
                    self.cursor_to(&data, &rows, group, cx);
                }
            }
            _ => {}
        }
    }

    /// →：树形中展开当前行（收起着时），已展开时移到第一个子进程；合并时展开当前的一组，已展开时移到第一个成员。
    fn expand_item(&mut self, cx: &mut Context<Self>) {
        let Some(data) = self.feed.data() else {
            return;
        };
        let rows = self.rows(&data, cx);
        if let Some(name) = self.group_cursor.clone() {
            if !self.expanded.contains(&name) {
                self.toggle_group(&name, cx);
            } else if let Some(at) = rows.iter().position(|r| r.group() == Some(name.as_str())) {
                self.cursor_to(&data, &rows, at + 1, cx);
            }
            return;
        }
        let Some(pid) = self.selected else {
            return;
        };
        let Some(at) = rows.iter().position(|r| r.pid(&data) == Some(pid)) else {
            return;
        };
        if let ProcRow::Tree(row) = &rows[at]
            && row.children > 0
        {
            if row.collapsed {
                self.set_collapsed(pid, false, cx);
            } else {
                self.cursor_to(&data, &rows, at + 1, cx);
            }
        }
    }

    /// 当前行移到合并的一行：没有选中的进程（详情收起）。
    fn focus_group(&mut self, name: String, cx: &mut Context<Self>) {
        self.group_cursor = Some(name);
        self.selected = None;
        self.detail = None;
        self._detail = None;
        self.followed = None;
        self.note = None;
        cx.notify();
    }

    /// Enter：重读选中的进程的详情，在合并的一行上展开、收起；没有当前行时选中第一行。
    fn open_item(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(name) = self.group_cursor.clone() {
            self.toggle_group(&name, cx);
            return;
        }
        match self.selected {
            Some(pid) => self.load_detail(pid, None, cx),
            None => self.move_cursor(Step::First, window, cx),
        }
    }

    /// Delete：结束选中的进程（先确认；正在结束时不再确认）。
    fn delete_item(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pid) = self.selected.filter(|pid| !self.ops.busy(pid)) {
            self.confirm_terminate(pid, Signal::Term, window, cx);
        }
    }

    /// Shift+F10、菜单键：在选中的行下面打开它的右键菜单。
    fn open_key_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(data) = self.feed.data() else {
            return;
        };
        let Some(process) = self
            .selected
            .and_then(|pid| data.processes.iter().find(|p| p.pid == pid))
        else {
            return;
        };
        let docker = shared::docker_cache(&self.context);
        let build = self.menu_builder(process, &data, docker.as_deref(), cx);
        KeyMenu::open(
            window,
            cx,
            self.focus.clone(),
            |page| &mut page.key_menu,
            build,
        );
    }

    /// 结束这个进程要 root 而这条连接上没有免密码 sudo（别的用户的进程）：菜单项与按钮带终端图标，确认框的主按钮是
    /// “在终端中执行”。
    fn locked(&self, process: &Process, data: &ProcessList) -> bool {
        self.root_first(process, data).is_some()
    }

    /// 没有免密码 sudo 时 sudo 的情况（[`Self::locked`]）：确认框据此写出在终端中执行的命令。
    fn root_first(&self, process: &Process, data: &ProcessList) -> Option<SudoState> {
        match self.feed.privilege {
            Some(Privilege::Limited(sudo)) if process.uid != data.uid && data.uid != Some(0) => {
                Some(sudo)
            }
            _ => None,
        }
    }

    /// 行的右键菜单（鼠标右键与键盘打开的、行右端与详情头部的 ⋯ 都是同一个）：结束、强制结束（正在结束时不能点），
    /// 跳到所属的服务、容器与连接，复制 PID。
    fn menu_builder(
        &self,
        process: &Process,
        data: &ProcessList,
        docker: Option<&DockerInfo>,
        cx: &mut Context<Self>,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
        let page = cx.entity().downgrade();
        let pid = process.pid;
        let busy = self.ops.busy(&pid);
        let name = visible_name(&process.name);
        let service = process.service.clone();
        let container = process
            .container
            .clone()
            .map(|id| (self.container_name(&id, docker), id));
        let locked = self.locked(process, data);
        let context = self.context.clone();
        // 只看这个用户的进程（已经按它筛选着时没有）
        let user = Some(process_user(process))
            .filter(|user| !user.is_empty() && self.filter.user.as_ref() != Some(user));
        move |menu, window, cx| {
            let menu_id = cx.entity_id();
            let _ = page.update(cx, |page, cx| page.set_menu_open(menu_id, pid, cx));
            let page_close = page.clone();
            shared::on_menu_dismiss(window, cx, move |cx| {
                let _ = page_close.update(cx, |page, cx| page.menu_closed(menu_id, cx));
            });
            let (term, kill) = (page.clone(), page.clone());
            let mut menu = menu
                .item(
                    action_item(
                        t!("tools.process.term_menu"),
                        IconName::Square,
                        true,
                        locked,
                        cx,
                    )
                    .disabled(busy)
                    .on_click(move |_, window, cx| {
                        let _ = term.update(cx, |page, cx| {
                            page.confirm_terminate(pid, Signal::Term, window, cx)
                        });
                    }),
                )
                .item(
                    action_item(
                        t!("tools.process.kill_menu"),
                        IconName::Skull,
                        true,
                        locked,
                        cx,
                    )
                    .disabled(busy)
                    .on_click(move |_, window, cx| {
                        let _ = kill.update(cx, |page, cx| {
                            page.confirm_terminate(pid, Signal::Kill, window, cx)
                        });
                    }),
                )
                .separator();
            if let Some(unit) = service.clone() {
                let context = context.clone();
                menu = menu.item(
                    PopupMenuItem::new(t!(
                        "tools.process.show_service",
                        name = service_name(&unit).to_string()
                    ))
                    .icon(IconName::Server)
                    .on_click(move |_, window, cx| {
                        let unit = unit.clone();
                        shared::go(&context, window, cx, move |panel, window, cx| {
                            panel.show_service(unit, window, cx)
                        });
                    }),
                );
            }
            if let Some((label, id)) = container.clone() {
                let context = context.clone();
                menu = menu.item(
                    PopupMenuItem::new(t!("tools.process.show_container", name = label))
                        .icon(IconName::Container)
                        .on_click(move |_, window, cx| {
                            let id = id.clone();
                            shared::go(&context, window, cx, move |panel, window, cx| {
                                panel.show_container(id, window, cx)
                            });
                        }),
                );
            }
            let (conn_context, conn_name) = (context.clone(), name.clone());
            menu = menu.item(
                PopupMenuItem::new(t!("tools.process.show_connections"))
                    .icon(IconName::Network)
                    .on_click(move |_, window, cx| {
                        let name = Some(conn_name.clone());
                        shared::go(&conn_context, window, cx, move |panel, window, cx| {
                            panel.show_connections(ConnChip::Pid { pid, name }, window, cx)
                        });
                    }),
            );
            if let Some(user) = user.clone() {
                let page = page.clone();
                menu = menu.item(
                    PopupMenuItem::new(t!("tools.process.only_user", user = visible_name(&user)))
                        .icon(IconName::User)
                        .on_click(move |_, _, cx| {
                            let user = user.clone();
                            let _ = page.update(cx, |page, cx| page.set_user(Some(user), cx));
                        }),
                );
            }
            menu.separator().item(
                PopupMenuItem::new(t!("tools.process.copy_pid"))
                    .icon(IconName::Copy)
                    .on_click(move |_, window, cx| Self::copy_pid(pid, window, cx)),
            )
        }
    }

    /// 确认后结束（TERM）或强制结束（KILL）：按风险写确认文案。
    fn confirm_terminate(
        &mut self,
        pid: u32,
        signal: Signal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(data) = self.feed.data() else {
            return;
        };
        let Some(process) = data.processes.iter().find(|p| p.pid == pid).cloned() else {
            return;
        };
        let risk = process_risk(&process);
        let children = descendants(&data.processes, pid).len();
        let kill = signal == Signal::Kill;
        let name = visible_name(&process.name);
        let user = display_user(&process);
        let title = if kill {
            t!("tools.process.kill_title", name = name.clone(), pid = pid)
        } else {
            t!("tools.process.term_title", name = name.clone(), pid = pid)
        };
        let danger = kill || risk != ProcessRisk::Normal;
        let host = self.context.host(cx);
        // 不是 root 时结束别的用户的进程要经免密码 sudo（与审计记录的写法一致）；这条连接上没有免密码 sudo 时主按钮是
        // “在终端中执行”，写出在终端中执行的命令，“仍然尝试”照常在后台执行
        let sudo = data.uid != Some(0) && process.uid != data.uid;
        let root_first = self.root_first(&process, &data);
        let action = kill_action(&process, signal);
        let command = action.command_line_with(sudo).unwrap_or_default();
        let page = cx.entity().downgrade();
        let dialogs = self.context.dialogs.clone();
        self.set_confirming(true, cx);
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, cx| {
            let theme = cx.theme();
            let body = v_flex()
                .id(SharedString::from(format!(
                    "tools-kill-risk-{}",
                    risk.key()
                )))
                .test_support()
                .gap_1p5()
                .text_xs()
                .children(host.as_ref().map(|host| shared::confirm_host(host, cx)))
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(prose(t!("tools.process.kill_user", user = user.clone())))
                        .when(children > 0, |this| {
                            this.child(prose(if children == 1 {
                                t!("tools.process.kill_child_one")
                            } else {
                                t!("tools.process.kill_children", count = children)
                            }))
                        }),
                )
                .when_some(risk_text(risk), |this, text| {
                    this.child(
                        div()
                            .font_medium()
                            .text_color(theme.warning)
                            .child(prose(text)),
                    )
                })
                // 结束（TERM）与强制结束（KILL）的后果
                .child(
                    div()
                        .id(if kill {
                            "tools-kill-effect-kill"
                        } else {
                            "tools-kill-effect-term"
                        })
                        .test_support()
                        .child(prose(if kill {
                            t!("tools.process.kill_effect")
                        } else {
                            t!("tools.process.term_effect")
                        })),
                )
                .child(match root_first {
                    Some(sudo) => shared::root_first_body(&action, sudo, cx),
                    None => shared::confirm_command(
                        "tools-confirm-command",
                        t!("tools.confirm.will_run"),
                        &command,
                        cx,
                    )
                    .into_any_element(),
                });
            let alert = alert.title(title.clone()).description(body);
            if let Some(sudo) = root_first {
                let (ok_page, ok_process) = (page.clone(), process.clone());
                let (try_page, try_process) = (page.clone(), process.clone());
                let (run_page, run_process) = (page.clone(), process.clone());
                let (close_page, closed_page) = (page.clone(), page.clone());
                return alert
                    .on_ok(move |_, window, cx| {
                        let _ = ok_page.update(cx, |page, cx| {
                            page.terminate_in_terminal(&ok_process, signal, sudo, window, cx)
                        });
                        true
                    })
                    .on_close(move |_, _, cx| {
                        let _ = close_page.update(cx, |page, cx| page.set_confirming(false, cx));
                    })
                    .footer(shared::root_first_footer(
                        danger,
                        move |window, cx| {
                            let _ = try_page.update(cx, |page, cx| {
                                page.terminate(&try_process, signal, window, cx)
                            });
                        },
                        move |window, cx| {
                            let _ = run_page.update(cx, |page, cx| {
                                page.terminate_in_terminal(&run_process, signal, sudo, window, cx)
                            });
                        },
                        move |cx| {
                            let _ =
                                closed_page.update(cx, |page, cx| page.set_confirming(false, cx));
                        },
                    ));
            }
            let (page_ok, page_close) = (page.clone(), page.clone());
            let process = process.clone();
            alert
                .confirm()
                .ok_text(if kill {
                    t!("tools.process.kill")
                } else {
                    t!("tools.process.term")
                })
                .ok_variant(if danger {
                    ButtonVariant::Danger
                } else {
                    ButtonVariant::Primary
                })
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, window, cx| {
                    let _ =
                        page_ok.update(cx, |page, cx| page.terminate(&process, signal, window, cx));
                    true
                })
                .on_close(move |_, _, cx| {
                    let _ = page_close.update(cx, |page, cx| page.set_confirming(false, cx));
                })
        });
        dialogs.opened(window, cx);
    }

    /// 结束进程的请求（失败的提示、需要 root 时的说明、审计）。
    fn kill_request(process: &Process, signal: Signal) -> ActRequest {
        let user = display_user(process);
        let name = visible_name(&process.name);
        let title = if signal == Signal::Kill {
            t!("tools.process.kill_action", name = name, pid = process.pid)
        } else {
            t!("tools.process.term_action", name = name, pid = process.pid)
        };
        ActRequest {
            action: kill_action(process, signal),
            title,
            label: format!("{} ({user})", process.name),
            docker: false,
            page: Page::Processes,
        }
    }

    /// 没有免密码 sudo 时的主按钮：在发起操作的终端旁的新 Tab 中结束它（不先在后台尝试）；稍后重读。
    fn terminate_in_terminal(
        &mut self,
        process: &Process,
        signal: Signal,
        sudo: SudoState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request = Self::kill_request(process, signal);
        let context = self.context.clone();
        let _ = self.context.panel.update(cx, |panel, cx| {
            panel.run_root_first(&context, request, sudo, window, cx)
        });
    }

    /// 结束进程：带上列表中的启动时刻（列表可能已过时，PID 被别的进程重新使用时核心不执行）。
    /// 不论成败，稍后重读（直接显示，不受鼠标停在列表上的暂停影响）。
    fn terminate(
        &mut self,
        process: &Process,
        signal: Signal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request = Self::kill_request(process, signal);
        let context = self.context.clone();
        let Ok(done) = self
            .context
            .panel
            .update(cx, |panel, cx| panel.perform(&context, request, window, cx))
        else {
            return;
        };
        let verb = if signal == Signal::Kill {
            OpVerb::Kill
        } else {
            OpVerb::Term
        };
        shared::track_op(
            self,
            |page| &mut page.ops,
            vec![process.pid],
            verb,
            done,
            |page, _, cx| {
                page._after_kill = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(AFTER_KILL).await;
                    let _ = this.update(cx, |page, cx| shared::refresh(page, true, cx));
                }));
            },
            cx,
        );
    }

    fn copy_pid(pid: u32, window: &mut Window, cx: &mut App) {
        shared::copy_text(pid.to_string(), pid.to_string(), window, cx);
    }

    // ------------------------------------------------------------ 渲染

    fn render_header(&self, columns: Columns, cx: &mut Context<Self>) -> impl IntoElement {
        let sort = self.sort;
        let cell = |key: ProcSort, label: SharedString, cx: &mut Context<Self>| {
            let active = sort.key == key;
            let text: SharedString = match (active, sort.desc) {
                (true, true) => format!("{label} ↓").into(),
                (true, false) => format!("{label} ↑").into(),
                (false, _) => label,
            };
            let foreground = cx.theme().foreground;
            div()
                .id(SharedString::from(format!("tools-sort-{}", sort_key(key))))
                .test_support()
                .cursor_default()
                .truncate()
                .when(active, |this| this.text_color(foreground))
                .hover(move |s| s.text_color(foreground))
                .on_click(cx.listener(move |this, _, _, cx| this.on_sort(key, cx)))
                .child(prose(text))
        };
        shared::list_header(cx)
            .px_1()
            .gap_1p5()
            .child(div().flex_1().min_w_0().child(cell(
                ProcSort::Name,
                t!("tools.process.col_name"),
                cx,
            )))
            .when(columns.command, |this| {
                this.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(t!("tools.process.col_command")),
                )
            })
            .child(h_flex().flex_none().w(rems(3.5)).justify_end().child(cell(
                ProcSort::Pid,
                "PID".into(),
                cx,
            )))
            .when(columns.user, |this| {
                this.child(div().flex_none().w(rems(5.)).child(cell(
                    ProcSort::User,
                    t!("tools.process.col_user"),
                    cx,
                )))
            })
            .when(columns.state, |this| {
                this.child(
                    div()
                        .flex_none()
                        .w(rems(4.))
                        .truncate()
                        .child(t!("tools.process.col_state")),
                )
            })
            .child(h_flex().flex_none().w(rems(4.)).justify_end().child(cell(
                ProcSort::Mem,
                t!("tools.process.col_mem"),
                cx,
            )))
            .child(h_flex().flex_none().w(rems(CPU_WIDTH)).justify_end().child(
                cell(ProcSort::Cpu, "CPU".into(), cx).tooltip(|window, cx| {
                    Tooltip::new(t!("tools.process.cpu_hint")).build(window, cx)
                }),
            ))
    }

    /// 树形中行首的一格：有子进程时是 ▸（收起着）或 ▾，点了收起、展开（不改变选中）；没有子进程时空着，名称照样对齐。
    fn fold_cell(&self, pid: u32, row: TreeRow, cx: &mut Context<Self>) -> AnyElement {
        let cell = h_flex()
            .id(("tools-process-fold", pid as usize))
            .test_support()
            .flex_none()
            .w(rems(0.75))
            .justify_center();
        if row.children == 0 {
            return cell.into_any_element();
        }
        let muted = cx.theme().muted_foreground;
        let collapse = !row.collapsed;
        cell.child(
            Icon::new(if row.collapsed {
                IconName::ChevronRight
            } else {
                IconName::ChevronDown
            })
            .xsmall()
            .text_color(muted),
        )
        // 在上面按下不选中这一行
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(cx.listener(move |this, _, window, cx| {
            this.focus.focus(window, cx);
            this.set_collapsed(pid, collapse, cx);
        }))
        .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_row(
        &mut self,
        process: &Process,
        indent: Indent,
        data: &ProcessList,
        columns: Columns,
        docker: Option<&DockerInfo>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pid = process.pid;
        // 名称一列里面的缩进（占位，不改变各列的宽度）：展开的一组中的成员一格，树形按层级（太深的不再加）
        let step = if columns.user {
            TREE_STEP
        } else {
            TREE_STEP_NARROW
        };
        let (indent_width, tree) = match indent {
            Indent::None => (0., None),
            Indent::Nested => (0.75, None),
            Indent::Tree(row) => (step * row.depth.min(TREE_MAX_DEPTH) as f32, Some(row)),
        };
        let fold = tree.map(|row| self.fold_cell(pid, row, cx));
        // 只是为了看出命中的进程在哪里而列出的父进程：名称用次要文字色
        let context = tree.is_some_and(|row| row.context);
        let selected = self.selected == Some(pid);
        let ring = selected && (self.focus.is_focused(window) || self.key_menu.is_open());
        let menu_layer = selected.then(|| self.key_menu.layer(window)).flatten();
        // 所在的容器、所属的服务：比进程名先收缩，列表窄时只留图标，悬停看完整的说明
        let owner_tags = [
            process.container.as_ref().map(|id| {
                let label = self.container_name(id, docker);
                (
                    "tools-process-container",
                    IconName::Container,
                    label.clone(),
                    t!(
                        "tools.process.tag_container_hint",
                        name = label,
                        id = id.clone()
                    ),
                )
            }),
            process.service.as_ref().map(|unit| {
                (
                    "tools-process-service",
                    IconName::ServerCog,
                    service_name(unit).to_string(),
                    t!("tools.process.tag_service_hint", unit = unit.clone()),
                )
            }),
        ]
        .into_iter()
        .flatten()
        .map(|(id, icon, text, hint)| {
            shared::tag(icon, text.into(), !columns.tag_text, cx)
                .id((id, pid as usize))
                .test_support()
                .tooltip(shared::row_tooltip(hint))
        })
        .collect::<Vec<_>>();
        let menu = self.menu_builder(process, data, docker, cx);
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let tone = state_tone(process.state);
        let state_color = match tone {
            Tone::Muted => theme.transparent,
            tone => tone.color(cx),
        };
        let name = visible_name(&process.name);
        // 行首的一格：这个终端的 shell 与其中运行的程序是一个终端的小图标（悬停看说明，运行中等状态照样着色），
        // 其余是状态的圆点；宽度固定，名称都对齐
        let lead = if process.in_terminal {
            let hint = if process.terminal_shell {
                t!("tools.process.self_shell_hint")
            } else {
                t!("tools.process.self_hint")
            };
            let color = match tone {
                Tone::Muted if process.terminal_shell => theme.foreground,
                Tone::Muted => muted,
                tone => tone.color(cx),
            };
            div()
                .id(("tools-process-self", pid as usize))
                .test_support()
                .flex()
                .child(
                    Icon::new(IconName::SquareTerminal)
                        .xsmall()
                        .text_color(color),
                )
                .tooltip(shared::row_tooltip(hint))
                .into_any_element()
        } else {
            dot(state_color).into_any_element()
        };
        let state = process.state;
        let menu_row = self.menu_row == Some(pid);
        let hovered = self.hover_row == Some(pid);
        let row_bg = if selected {
            theme.list_active
        } else {
            theme.list_hover
        };
        // 行右端：结束进行中、刚完成时说明；鼠标停在行上、键盘焦点在这一行、菜单打开着时浮出“结束”与 ⋯
        let op = self.ops.get(&pid);
        let actions = match op {
            Some(op) => Some(vec![shared::op_status(
                ("tools-process-op", pid as usize),
                op,
                cx,
            )]),
            None if hovered || menu_row || ring => {
                let locked = self.locked(process, data);
                let term = shared::row_action_button(
                    ("tools-process-row-term", pid as usize),
                    IconName::Square,
                    shared::op_tip(t!("tools.process.term"), locked),
                    true,
                    false,
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.confirm_terminate(pid, Signal::Term, window, cx)
                }))
                .into_any_element();
                let more = shared::more_button(
                    ("tools-process-row-more", pid as usize),
                    self.menu_builder(process, data, docker, cx),
                );
                Some(vec![term, more])
            }
            None => None,
        }
        .map(|buttons| {
            shared::row_actions(("tools-process-actions", pid as usize), row_bg, buttons, cx)
        });
        let command: SharedString = process
            .cmdline
            .as_deref()
            .map(visible_name)
            .unwrap_or_default()
            .into();
        let theme = cx.theme();
        let row = h_flex()
            .id(("tools-process-row", pid as usize))
            .test_support()
            .size_full()
            .px_1()
            .gap_1p5()
            .text_xs()
            .cursor_default()
            // 只为看出位置而列出的父进程：整行用次要文字色
            .when(context, |el| el.text_color(muted))
            .when(selected, |el| el.bg(theme.list_active))
            .when(!selected && menu_row, |el| el.bg(theme.list_hover))
            .when(!selected, |el| el.hover(|s| s.bg(theme.list_hover)));
        shared::ring(row, ring, cx)
            .children(menu_layer)
            .on_hover(
                cx.listener(move |this, hovered: &bool, _, cx| {
                    this.set_hover_row(pid, *hovered, cx)
                }),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.focus.focus(window, cx);
                this.select(pid, cx)
            }))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .gap_1()
                    .when(indent_width > 0., |this| {
                        this.child(div().flex_none().w(rems(indent_width)))
                    })
                    .children(fold)
                    .child(
                        h_flex()
                            .flex_none()
                            .w(rems(0.75))
                            .justify_center()
                            .child(lead),
                    )
                    // 名称优先：照自己的宽度显示（太长的截在 8rem），标签放在剩下的位置；位置不够时标签先截短名称、
                    // 只剩图标，再截短进程名（至少留 4rem）
                    .child(
                        div()
                            .id(("tools-process-name", pid as usize))
                            .test_support()
                            .flex_shrink(1.)
                            .min_w(rems(4.))
                            .max_w(rems(8.))
                            .truncate()
                            .when(context, |this| this.text_color(muted))
                            .child(name),
                    )
                    .children(owner_tags),
            )
            .when(columns.command, |this| {
                this.child(
                    div()
                        .id(("tools-process-command", pid as usize))
                        .test_support()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(muted)
                        .child(command.clone())
                        .when(!command.is_empty(), |this| {
                            this.tooltip(shared::row_tooltip(command))
                        }),
                )
            })
            .child(
                div()
                    .flex_none()
                    .w(rems(3.5))
                    .text_right()
                    .text_color(muted)
                    .child(pid.to_string()),
            )
            .when(columns.user, |this| {
                this.child(
                    div()
                        .flex_none()
                        .w(rems(5.))
                        .truncate()
                        .text_color(muted)
                        .child(display_user(process)),
                )
            })
            .when(columns.state, |this| {
                this.child(
                    div()
                        .id(("tools-process-state", pid as usize))
                        .flex_none()
                        .w(rems(4.))
                        .truncate()
                        .text_color(muted)
                        .child(state_label(state).unwrap_or_else(|| state.to_string().into()))
                        .tooltip(shared::row_tooltip(state.to_string().into())),
                )
            })
            .child(
                div()
                    .flex_none()
                    .w(rems(4.))
                    .text_right()
                    .child(format_size(process.rss)),
            )
            .child(
                div()
                    .flex_none()
                    .w(rems(CPU_WIDTH))
                    .text_right()
                    .child(format_cpu(process.cpu)),
            )
            .children(actions)
            .context_menu(menu)
            .into_any_element()
    }

    /// 合并的同名进程的一行：▸ 名称，PID 一列写个数（×12），内存与 CPU 是合计（用户都一样时写出用户）；点了展开、收起。
    /// 合计按显示着的读数现算（暂停刷新时补读到的 CPU 就地填进显示着的读数，组的先后不变，合计跟着变）。
    fn render_group_row(
        &mut self,
        group: &ProcGroup,
        data: &ProcessList,
        columns: Columns,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.expanded.contains(&group.name);
        let cursor = self.group_cursor.as_deref() == Some(group.name.as_str());
        let ring = cursor && self.focus.is_focused(window);
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let users: BTreeSet<String> = group
            .members
            .iter()
            .map(|&ix| display_user(&data.processes[ix]))
            .collect();
        let user = match users.len() {
            1 => users.into_iter().next().unwrap_or_default(),
            _ => "-".to_string(),
        };
        let members = || group.members.iter().map(|&ix| &data.processes[ix]);
        let rss: u64 = members().map(|p| p.rss).sum();
        let cpu = members()
            .filter_map(|p| p.cpu)
            .reduce(|a, b| a + b)
            .map(|cpu| (cpu * 10.).round() / 10.);
        let name = group.name.clone();
        let row = h_flex()
            .id(SharedString::from(format!(
                "tools-process-group-{}",
                group.name
            )))
            .test_support()
            .size_full()
            .px_1()
            .gap_1p5()
            .text_xs()
            .cursor_default()
            .when(cursor, |el| el.bg(theme.list_active))
            .when(!cursor, |el| el.hover(|s| s.bg(theme.list_hover)));
        shared::ring(row, ring, cx)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.focus.focus(window, cx);
                this.focus_group(name.clone(), cx);
                this.toggle_group(&name, cx);
            }))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .gap_1()
                    .child(
                        h_flex().flex_none().w(rems(0.75)).justify_center().child(
                            Icon::new(if open {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .xsmall()
                            .text_color(muted),
                        ),
                    )
                    .child(div().min_w_0().truncate().child(visible_name(&group.name))),
            )
            .when(columns.command, |this| this.child(div().flex_1()))
            .child(
                div()
                    .id(SharedString::from(format!(
                        "tools-process-group-count-{}",
                        group.name
                    )))
                    .test_support()
                    .flex_none()
                    .w(rems(3.5))
                    .text_right()
                    .text_color(muted)
                    .child(format!("×{}", group.members.len())),
            )
            .when(columns.user, |this| {
                this.child(
                    div()
                        .flex_none()
                        .w(rems(5.))
                        .truncate()
                        .text_color(muted)
                        .child(user),
                )
            })
            .when(columns.state, |this| {
                this.child(div().flex_none().w(rems(4.)))
            })
            .child(
                div()
                    .flex_none()
                    .w(rems(4.))
                    .text_right()
                    .child(format_size(rss)),
            )
            .child(
                div()
                    .flex_none()
                    .w(rems(CPU_WIDTH))
                    .text_right()
                    .child(format_cpu(cpu)),
            )
            .into_any_element()
    }

    /// 工具条上的按用户筛选：下拉菜单列出“全部用户”与现有的各个用户（进程多的在前，写着进程数），选中的打勾；
    /// 用户多时菜单在里面滚动。筛选着时按钮显示为选中。
    fn render_user_menu(&self, data: &ProcessList, cx: &mut Context<Self>) -> AnyElement {
        let users = Rc::new(process_users(&data.processes));
        let current = self.filter.user.clone();
        let page = cx.entity().downgrade();
        Button::new("tools-process-users")
            .ghost()
            .small()
            .icon(IconName::Users)
            .selected(current.is_some())
            .tooltip(t!("tools.process.filter_user"))
            .dropdown_menu(move |menu, window, _| {
                let max_h = rems(USER_MENU_HEIGHT).to_pixels(window.rem_size());
                let all = page.clone();
                let menu = menu.scrollable(true).max_h(max_h).item(
                    PopupMenuItem::new(t!("tools.process.all_users"))
                        .checked(current.is_none())
                        .on_click(move |_, _, cx| {
                            let _ = all.update(cx, |page, cx| page.set_user(None, cx));
                        }),
                );
                users.iter().fold(menu.separator(), |menu, (user, count)| {
                    let page = page.clone();
                    let pick = user.clone();
                    menu.item(
                        PopupMenuItem::new(t!(
                            "tools.process.user_item",
                            user = visible_name(user),
                            count = *count
                        ))
                        .checked(current.as_ref() == Some(user))
                        .on_click(move |_, _, cx| {
                            let user = pick.clone();
                            let _ = page.update(cx, |page, cx| page.set_user(Some(user), cx));
                        }),
                    )
                })
            })
            .into_any_element()
    }

    /// 列表有焦点时滚动跟随当前行：选中的进程在列表中的位置变了（刷新后重新排序、键盘移动）才滚过去，
    /// 用滚轮滚开之后不拉回来。
    fn follow_selected(&mut self, data: &ProcessList, rows: &[ProcRow], window: &Window) {
        if !self.focus.is_focused(window) {
            return;
        }
        let Some(pid) = self.selected else {
            return;
        };
        let Some(ix) = rows.iter().position(|r| r.pid(data) == Some(pid)) else {
            return;
        };
        if self.followed != Some((pid, ix)) {
            self.followed = Some((pid, ix));
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
    }

    fn render_list(
        &mut self,
        data: &Rc<ProcessList>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let rem = window.rem_size();
        let width = self.width.get();
        // 树形时名称一列还要放缩进：次要的列晚一些才显示
        let shift = if self.view == ProcView::Tree {
            TREE_COLUMN_SHIFT
        } else {
            0.
        };
        let thresholds = [
            USER_COLUMN_FROM,
            STATE_COLUMN_FROM,
            TAG_TEXT_FROM,
            WIDE_FROM,
        ]
        .map(|from| rems(from + shift).to_pixels(rem));
        let [user_from, state_from, tag_from, wide_from] = thresholds;
        let columns = Columns {
            user: width >= user_from,
            state: width >= state_from,
            tag_text: width >= tag_from,
            command: width >= wide_from,
        };
        let rows = self.rows(data, cx);
        self.follow_selected(data, &rows, window);
        let row_height = rems(ROW_HEIGHT).to_pixels(rem);
        let sizes = Rc::new(vec![size(px(0.), row_height); rows.len()]);
        let data_for_rows = data.clone();
        let docker = shared::docker_cache(&self.context);
        let body = if rows.is_empty() {
            shared::empty_text(t!("tools.process.empty"), cx)
        } else {
            v_virtual_list(
                cx.entity(),
                "tools-process-rows",
                sizes,
                move |this, range, window, cx| {
                    range
                        .filter_map(|ix| rows.get(ix).cloned())
                        .map(|row| match row {
                            ProcRow::Process { ix, nested } => this.render_row(
                                &data_for_rows.processes[ix],
                                if nested { Indent::Nested } else { Indent::None },
                                &data_for_rows,
                                columns,
                                docker.as_deref(),
                                window,
                                cx,
                            ),
                            ProcRow::Tree(tree) => this.render_row(
                                &data_for_rows.processes[tree.ix],
                                Indent::Tree(tree),
                                &data_for_rows,
                                columns,
                                docker.as_deref(),
                                window,
                                cx,
                            ),
                            ProcRow::Group(group) => {
                                this.render_group_row(&group, &data_for_rows, columns, window, cx)
                            }
                        })
                        .collect::<Vec<_>>()
                },
            )
            .track_scroll(&self.scroll)
            .size_full()
            .into_any_element()
        };
        let measured = self.width.clone();
        let entity = cx.entity().downgrade();
        shared::list_card(v_flex(), true, cx)
            .id("tools-process-list")
            .test_support()
            .key_context(LIST_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &SelectPrev, window, cx| {
                this.move_cursor(Step::Prev, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectNext, window, cx| {
                this.move_cursor(Step::Next, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectPageUp, window, cx| {
                this.move_cursor(Step::PageUp, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectPageDown, window, cx| {
                this.move_cursor(Step::PageDown, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectFirst, window, cx| {
                this.move_cursor(Step::First, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectLast, window, cx| {
                this.move_cursor(Step::Last, window, cx)
            }))
            .on_action(cx.listener(|this, _: &OpenItem, window, cx| this.open_item(window, cx)))
            .on_action(cx.listener(|this, _: &CollapseItem, _, cx| this.collapse_item(cx)))
            .on_action(cx.listener(|this, _: &ExpandItem, _, cx| this.expand_item(cx)))
            .on_action(cx.listener(|this, _: &DeleteItem, window, cx| this.delete_item(window, cx)))
            .on_action(cx.listener(|this, _: &OpenMenu, window, cx| this.open_key_menu(window, cx)))
            .relative()
            .flex_1()
            .min_h(rems(shared::LIST_MIN))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| this.set_hovering(*hovered, cx)))
            .child(
                // 宽度跨过显示 / 隐藏列的界限时重新渲染
                canvas(
                    move |bounds, _, cx| {
                        let old = measured.replace(bounds.size.width);
                        if thresholds
                            .iter()
                            .any(|t| (old >= *t) != (bounds.size.width >= *t))
                        {
                            let _ = entity.update(cx, |_, cx| cx.notify());
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(self.render_header(columns, cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(body)
                    .vertical_scrollbar(&self.scroll),
            )
    }

    fn render_detail(
        &self,
        data: &ProcessList,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let pid = self.selected?;
        let process = data.processes.iter().find(|p| p.pid == pid)?;
        let theme = cx.theme();
        let (muted, warning) = (theme.muted_foreground, theme.warning);
        let mono = theme.mono_font_family.clone();
        let detail = self
            .detail
            .as_ref()
            .filter(|(p, _)| *p == pid)
            .and_then(|(_, d)| d.as_ref());
        let loaded = detail.and_then(|d| d.as_ref().ok());
        let gone = matches!(detail, Some(Err(ToolsError::NoProcess)));
        let parent = data.processes.iter().find(|p| p.pid == process.ppid);
        let children: Vec<&Process> = data.processes.iter().filter(|p| p.ppid == pid).collect();
        let row = |label: SharedString, value: AnyElement| {
            h_flex()
                .items_start()
                .gap_3()
                .child(div().flex_none().w(rems(6.)).text_color(muted).child(label))
                .child(div().flex_1().min_w_0().child(value))
                .into_any_element()
        };
        let text = |value: String| div().child(value).into_any_element();
        // 按钮按自己的宽度放在值那一列的开头，去掉水平内边距：文字与同一列的其他值左对齐
        let link = |id: ElementId, target: &Process, cx: &mut Context<Self>| {
            let target_pid = target.pid;
            h_flex()
                .child(
                    Button::new(id)
                        .ghost()
                        .xsmall()
                        .px_0()
                        .label(format!("{} ({})", visible_name(&target.name), target.pid))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.jump_to(target_pid, window, cx)
                        })),
                )
                .into_any_element()
        };
        let user = match (&process.user, process.uid) {
            (Some(user), Some(uid)) => format!("{user} ({uid})"),
            _ => display_user(process),
        };
        let parent_value = match parent {
            Some(parent) => link(
                ("tools-process-parent", parent.pid as usize).into(),
                parent,
                cx,
            ),
            None => text(process.ppid.to_string()),
        };
        let mut rows = vec![
            row(t!("tools.process.col_user"), text(user)),
            row(t!("tools.process.parent"), parent_value),
        ];
        if !children.is_empty() {
            let mut links: Vec<AnyElement> = children
                .iter()
                .take(MAX_CHILDREN)
                .map(|c| link(("tools-process-child", c.pid as usize).into(), c, cx))
                .collect();
            if children.len() > MAX_CHILDREN {
                links.push(div().child("…").into_any_element());
            }
            rows.push(row(
                t!("tools.process.children"),
                h_flex()
                    .flex_wrap()
                    .gap_x_3()
                    .children(links)
                    .into_any_element(),
            ));
        }
        // 所属的服务、所在的容器：跳到对应的页（这个进程的连接在 ⋯ 中）
        let go = |id: &'static str, label: String, f: Jump| {
            let context = self.context.clone();
            h_flex()
                .child(
                    Button::new(id)
                        .ghost()
                        .xsmall()
                        .px_0()
                        .label(label)
                        .on_click(move |_, window, cx| {
                            let f = f.clone();
                            shared::go(&context, window, cx, move |panel, window, cx| {
                                f(panel, window, cx)
                            });
                        }),
                )
                .into_any_element()
        };
        if let Some(unit) = process.service.clone() {
            let label = service_name(&unit).to_string();
            rows.push(row(
                t!("tools.process.service"),
                go(
                    "tools-process-service-link",
                    label,
                    Rc::new(move |panel, window, cx| panel.show_service(unit.clone(), window, cx)),
                ),
            ));
        }
        let docker = shared::docker_cache(&self.context);
        if let Some(id) = process.container.clone() {
            let label = self.container_name(&id, docker.as_deref());
            rows.push(row(
                t!("tools.process.container"),
                go(
                    "tools-process-container-link",
                    label,
                    Rc::new(move |panel, window, cx| panel.show_container(id.clone(), window, cx)),
                ),
            ));
        }
        rows.push(row(
            t!("tools.process.started"),
            text(
                process
                    .start
                    .filter(|ms| *ms > 0)
                    .map(termlet_core::time::format_local)
                    .unwrap_or_else(|| "-".into()),
            ),
        ));
        rows.push(row(
            t!("tools.process.cpu_time"),
            text(t!("tools.process.seconds", n = process.cpu_time).to_string()),
        ));
        rows.push(row(
            t!("tools.process.threads"),
            text(process.threads.to_string()),
        ));
        rows.push(row(
            t!("tools.process.memory"),
            text(format!(
                "{} / {}",
                format_size(process.rss),
                format_size(process.vsize)
            )),
        ));
        rows.push(row(
            t!("tools.process.nice"),
            text(format!("{} / {}", process.nice, process.priority)),
        ));
        if let Some(exe) = loaded.and_then(|d| d.exe.as_deref()) {
            rows.push(row(t!("tools.process.exe"), text(visible_name(exe))));
        }
        if let Some(cwd) = loaded.and_then(|d| d.cwd.as_deref()) {
            rows.push(row(t!("tools.process.cwd"), text(visible_name(cwd))));
        }
        // 读取中为“…”；读不到（进程已结束等）或为空时为“-”
        let command = match detail {
            Some(Ok(d)) if !d.command.is_empty() => visible_name(&d.command),
            Some(_) => "-".to_string(),
            None => "…".to_string(),
        };
        rows.push(row(
            t!("tools.process.command"),
            div()
                .id("tools-process-command")
                .test_support()
                .font_family(mono)
                .child(command)
                .into_any_element(),
        ));
        // 头部：名称 ● 状态 · 结束 / 强制结束（正在结束时不能点）· ⋯（跳转、复制等，与右键菜单相同）
        let locked = self.locked(process, data);
        let busy = self.ops.busy(&pid);
        let actions = vec![
            op_button(
                "tools-process-detail-term",
                IconName::Square,
                t!("tools.process.term"),
                true,
                locked,
            )
            .disabled(busy)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.confirm_terminate(pid, Signal::Term, window, cx)
            }))
            .into_any_element(),
            op_button(
                "tools-process-detail-kill",
                IconName::Skull,
                t!("tools.process.kill"),
                true,
                locked,
            )
            .disabled(busy)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.confirm_terminate(pid, Signal::Kill, window, cx)
            }))
            .into_any_element(),
        ];
        let more = shared::more_button(
            "tools-process-detail-more",
            self.menu_builder(process, data, docker.as_deref(), cx),
        );
        let status = state_label(process.state).map(|label| (label, state_tone(process.state)));
        let header = shared::detail_header(
            format!("{} ({pid})", visible_name(&process.name)).into(),
            status,
            actions,
            Some(more),
            cx,
        );
        let mut body: Vec<AnyElement> = Vec::new();
        if gone {
            body.push(
                div()
                    .text_color(warning)
                    .child(t!("tools.process.gone"))
                    .into_any_element(),
            );
        }
        body.extend(rows);
        Some(shared::detail_pane(
            "tools-process-detail",
            &self.splitter,
            &self.detail_height,
            header,
            body,
            window,
            cx,
        ))
    }
}

#[cfg(test)]
impl ProcessPage {
    pub(super) fn selected(&self) -> Option<u32> {
        self.selected
    }

    pub(super) fn sort(&self) -> SortOrder {
        self.sort
    }

    pub(super) fn query(&self, cx: &App) -> String {
        self.search.read(cx).value().to_string()
    }

    pub(super) fn filter(&self) -> ProcFilter {
        self.filter.clone()
    }

    pub(super) fn key_menu_open(&self) -> bool {
        self.key_menu.is_open()
    }

    pub(super) fn note(&self) -> Option<SharedString> {
        self.note.clone()
    }

    pub(super) fn view(&self) -> ProcView {
        self.view
    }

    /// 树形中这个进程的一行：是不是只为看出位置而列出的父进程（`Some(true)`）；不在显示着的行中时为 `None`。
    pub(super) fn tree_context(&mut self, pid: u32, cx: &App) -> Option<bool> {
        let data = self.feed.data()?;
        self.rows(&data, cx).iter().find_map(|row| match row {
            ProcRow::Tree(tree) if data.processes[tree.ix].pid == pid => Some(tree.context),
            _ => None,
        })
    }

    /// 当作读到了一份结果（与读取结束时相同：暂停着时先存着）。
    pub(super) fn arrive_for_test(&mut self, list: ProcessList, cx: &mut Context<Self>) {
        let paused = self.is_paused();
        self.feed.shown.arrive(Rc::new(list), 0, paused, false);
        self.after_load(cx);
        cx.notify();
    }

    /// 显示着的各进程的 CPU。
    pub(super) fn cpus(&self) -> Vec<(u32, Option<f64>)> {
        self.feed
            .shown
            .data
            .as_ref()
            .map(|d| d.processes.iter().map(|p| (p.pid, p.cpu)).collect())
            .unwrap_or_default()
    }

    /// 列表中这个进程的命令行。
    pub(super) fn cmdline_of(&self, pid: u32) -> Option<String> {
        self.feed
            .shown
            .data
            .as_ref()?
            .processes
            .iter()
            .find(|p| p.pid == pid)?
            .cmdline
            .clone()
    }

    pub(super) fn pids(&self) -> Vec<u32> {
        self.feed
            .shown
            .data
            .as_ref()
            .map(|d| d.processes.iter().map(|p| p.pid).collect())
            .unwrap_or_default()
    }

    /// 正在读取。
    pub(super) fn is_loading(&self) -> bool {
        self.feed.loading
    }
}

impl Render for ProcessPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(data) = self.feed.data() else {
            let body = match &self.feed.error {
                Some(error) => {
                    let this = cx.entity().downgrade();
                    notice(
                        error,
                        false,
                        move |_, _, cx| {
                            let _ = this.update(cx, |page, cx| shared::refresh(page, false, cx));
                        },
                        cx,
                    )
                }
                None => loading_text(cx),
            };
            return shared::focus_body(&self.focus, body);
        };
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let filtered = self.filter != ProcFilter::default();
        // 筛选后的进程数（合并时数组中的进程，不数合并的那一行；树形中为了看出位置而列出的父进程不算）
        self.rows(&data, cx);
        let shown = self.rows_matched;
        let count = if filtered && data.total == 1 {
            t!("tools.process.count_filtered_of_one", shown = shown)
        } else if filtered {
            t!(
                "tools.process.count_filtered",
                shown = shown,
                total = data.total
            )
        } else if data.truncated {
            t!(
                "tools.process.truncated",
                shown = data.processes.len(),
                total = data.total
            )
        } else if data.total == 1 {
            t!("tools.process.count_one")
        } else {
            t!("tools.process.count", count = data.total)
        };
        let paused = self.is_paused();
        let summary = h_flex()
            .id("tools-process-count")
            .test_support()
            .gap_2()
            .child(count)
            .when(paused, |this| {
                this.child(
                    div()
                        .text_color(muted.opacity(0.7))
                        .child(t!("tools.paused")),
                )
            });
        let marker = shared::privilege_marker(
            RootScope::Processes,
            self.feed.privilege,
            t!("tools.privilege.sudo"),
            &self.context,
            cx,
        );
        let loading = self.feed.loading;
        let this = cx.entity().downgrade();
        // 显示方式：树形、合并同名进程（只能打开一个，再点一下回到列表）；按用户筛选；有容器内的进程（或者正隐藏着）
        // 时另有隐藏它们的开关
        let view = self.view;
        let toggle_view = move |target: ProcView| {
            if view == target {
                ProcView::List
            } else {
                target
            }
        };
        let tree_toggle = Button::new("tools-process-tree")
            .ghost()
            .small()
            .icon(IconName::ListTree)
            .selected(view == ProcView::Tree)
            .tooltip(if view == ProcView::Tree {
                t!("tools.process.untree")
            } else {
                t!("tools.process.tree")
            })
            .on_click(
                cx.listener(move |this, _, _, cx| this.set_view(toggle_view(ProcView::Tree), cx)),
            );
        let merge = view == ProcView::Merged;
        let merge_toggle = Button::new("tools-process-merge")
            .ghost()
            .small()
            .icon(IconName::Layers)
            .selected(merge)
            .tooltip(if merge {
                t!("tools.process.unmerge")
            } else {
                t!("tools.process.merge")
            })
            .on_click(
                cx.listener(move |this, _, _, cx| this.set_view(toggle_view(ProcView::Merged), cx)),
            );
        let user_menu = self.render_user_menu(&data, cx);
        let hide = self.filter.hide_containers;
        let hide_toggle =
            (hide || data.processes.iter().any(|p| p.container.is_some())).then(|| {
                Button::new("tools-process-hide-containers")
                    .ghost()
                    .small()
                    .icon(IconName::Container)
                    .selected(hide)
                    .tooltip(if hide {
                        t!("tools.process.show_containers")
                    } else {
                        t!("tools.process.hide_containers")
                    })
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.set_hide_containers(!hide, cx)),
                    )
            });
        let toggle = h_flex()
            .gap_1()
            .child(tree_toggle)
            .child(merge_toggle)
            .child(user_menu)
            .children(hide_toggle)
            .into_any_element();
        let mut chips = Vec::new();
        if let Some(user) = &self.filter.user {
            chips.push(shared::filter_chip(
                "tools-process-chip-user",
                t!("tools.process.chip_user", user = visible_name(user)),
                cx.listener(|this, _, _, cx| this.set_user(None, cx)),
                cx,
            ));
        }
        if let Some(name) = &self.container_name {
            chips.push(shared::filter_chip(
                "tools-process-chip-container",
                t!("tools.process.chip_container", name = name.clone()),
                cx.listener(|this, _, _, cx| this.clear_container(cx)),
                cx,
            ));
        } else if hide {
            chips.push(shared::filter_chip(
                "tools-process-chip-hidden",
                t!("tools.process.chip_hidden"),
                cx.listener(|this, _, _, cx| this.set_hide_containers(false, cx)),
                cx,
            ));
        }
        let note = self.note.clone().map(|note| {
            shared::note_line(
                note,
                cx.listener(|this, _, _, cx| {
                    this.note = None;
                    cx.notify();
                }),
                cx,
            )
        });
        shared::list_page()
            .on_drag_move(shared::on_detail_drag)
            .child(shared::toolbar_with(
                "tools-process",
                &self.search,
                Some(toggle),
                loading,
                move |_, _, cx| {
                    let _ = this.update(cx, |page, cx| shared::refresh(page, false, cx));
                },
            ))
            .children(shared::chip_row(chips))
            .child(info_line(summary, marker, self.feed.shown.at, cx))
            .children(error_line(self.feed.error.as_ref(), cx))
            .children(shared::failure_banner(
                &self.ops,
                |page: &mut Self| &mut page.ops,
                cx,
            ))
            .children(note)
            .child(self.render_list(&data, window, cx))
            .children(self.render_detail(&data, window, cx))
            .into_any_element()
    }
}

/// 详情中跳到别的页的按钮做的事。
type Jump = Rc<dyn Fn(&mut HostToolsPanel, &mut Window, &mut Context<HostToolsPanel>)>;

/// 结束进程的操作：带上列表中的启动时刻（执行前核对，见 [`Action::Kill`]）。
fn kill_action(process: &Process, signal: Signal) -> Action {
    Action::Kill {
        pid: process.pid,
        signal,
        start_ticks: Some(process.start_ticks),
    }
}

/// 进程的用户：用户名，没有时为 UID，都没有时为“-”。
fn display_user(process: &Process) -> String {
    let user = process_user(process);
    if user.is_empty() { "-".into() } else { user }
}

/// 排序列的元素 ID 后缀。
fn sort_key(key: ProcSort) -> &'static str {
    match key {
        ProcSort::Name => "name",
        ProcSort::Pid => "pid",
        ProcSort::User => "user",
        ProcSort::Mem => "mem",
        ProcSort::Cpu => "cpu",
    }
}

/// 状态字母的颜色：运行中绿，不可中断、停止黄，僵尸红，其余没有颜色。
fn state_tone(state: char) -> Tone {
    match state {
        'R' => Tone::Good,
        'D' | 'T' | 't' => Tone::Warning,
        'Z' | 'X' => Tone::Danger,
        _ => Tone::Muted,
    }
}

/// 状态字母的说明；不认识的为 `None`（原样显示字母）。
fn state_label(state: char) -> Option<SharedString> {
    Some(match state {
        'R' => t!("tools.process.state_running"),
        'S' => t!("tools.process.state_sleeping"),
        'D' => t!("tools.process.state_uninterruptible"),
        'Z' => t!("tools.process.state_zombie"),
        'T' => t!("tools.process.state_stopped"),
        't' => t!("tools.process.state_traced"),
        'I' => t!("tools.process.state_idle"),
        'X' => t!("tools.process.state_dead"),
        'W' => t!("tools.process.state_paging"),
        _ => return None,
    })
}

/// 结束进程有风险时的说明。
fn risk_text(risk: ProcessRisk) -> Option<SharedString> {
    match risk {
        ProcessRisk::Init => Some(t!("tools.process.risk_init")),
        ProcessRisk::Shell => Some(t!("tools.process.risk_shell")),
        ProcessRisk::InTerminal => Some(t!("tools.process.risk_self")),
        ProcessRisk::Sshd => Some(t!("tools.process.risk_sshd")),
        ProcessRisk::Normal => None,
    }
}
