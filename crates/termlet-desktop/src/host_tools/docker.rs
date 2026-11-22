//! Docker 页：容器（按 compose 项目分组，项目可以整体启停）、异常的容器、镜像、卷、网络五个子页（下划线样式的页签）；
//! 镜像、卷、网络标出使用中 / 未使用，未使用的可以删除。没有 docker 而有 podman 时用 podman。显示时读一次，之后每
//! 15 秒刷新（鼠标停在列表上、菜单或确认框打开时暂停，页不显示时停止），有刷新按钮，操作后重读。工具条上的“清理…”
//! 先列出可以清理的内容再逐类 prune（见 [`super::docker_cleanup`]；对话框开着时暂停刷新，清理完重读）。
//!
//! - 状态：行上用本地化的写法（“运行中 · 15 分钟”“已退出（1）· 2 小时前”），按列表读取时 inspect 取出的运行状态
//!   现算，与详情一致（选中的容器以详情读到的为准）；健康状态（不健康、启动中）单独成一个小标签，放在状态前面。
//! - 资源占用：容器子页显示时另读一次 `docker stats --no-stream`（与列表分开读，要 1–2 秒，不拖慢列表），行上显示
//!   CPU 与内存，按与状态条相同的阈值着色，可按 CPU、内存排序。
//! - 异常：非 0 退出用警告色，不健康、重启中、已失效用危险色；“异常”子页只列它们。选中的容器被子页、搜索藏起来时
//!   取消选中，详情只留列表中看得到的容器。
//! - 选中容器时底部显示详情（`docker container inspect`，选中时读；头部一行：名称 ● 状态 · 主要操作 · ⋯，与列表之间
//!   的分隔条可拖动）：镜像、状态与运行时长、重启策略与次数、健康检查、端口（可转发到本机）、挂载（绑定挂载可在
//!   SFTP 中打开）、网络，以及最近 20 行日志；⋯ 与右键菜单相同。
//! - 日志在中间区域的 Tab 中查看（日志查看器）；在新 Tab 中跟随日志、进入容器在终端中进行。
//! - 跳转：容器详情的“查看进程”跳到进程页、只列出这个容器内的进程；发布的端口可以“查看连接”（网络页按端口筛选）。
//!   从进程页（所属容器）、网络页（监听端口）跳过来的容器选中并滚到它，不在列表中时说明。
//! - 信息行：“Docker 29.3.0 · Compose 2.39.1 · 3 个运行中”（版本在读列表的脚本中一并取得）。
//! - 行上的操作：鼠标停在行上（或键盘焦点所在的行）时右端浮出主要操作（运行中的重启，已停止的启动；镜像、卷、网络是
//!   删除）与 ⋯（与右键菜单相同）；操作进行中时那里转圈并写“正在重启…”，这一行（项目整体操作时是项目中的各个容器）
//!   与详情中的操作不能点，完成后写“已重启”一会儿。列表够宽时（[`super::processes::WIDE_FROM`]）容器是一行一个的表格：
//!   名称、镜像、端口、CPU、内存、状态。镜像的创建时间按本机时区写。
//! - 键盘（见 [`super::keys`]）：↑ ↓ 等在容器之间移动并选中（跳过项目的标题；镜像、卷、网络移动当前行），Enter 重读
//!   详情，L 查看日志，Delete 停止运行中的容器、删除已停止的容器与未使用的镜像等（先确认），Shift+F10 打开右键菜单。

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tab::Tab;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, StyledExt as _,
    VirtualListScrollHandle, h_flex, v_flex, v_virtual_list,
};
use gpui_kit::{
    AnyElement, App, Context, Entity, EntityId, FocusHandle, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, Render, ScrollHandle, ScrollStrategy, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, TestSupportExt as _,
    WeakEntity, Window, canvas, div, prelude::FluentBuilder as _, px, relative, rems, size,
};
use termlet_core::hosttools::{
    Action, Container, ContainerDetail, ContainerOp, ContainerStats, DockerInfo, Engine, ExecError,
    HealthState, Image, LogQuery, LogResult, LogSource, MountInfo, RunState, Target,
    TerminalRequest, ToolsError,
};

use super::docker_cleanup::{self, CleanupDialog};
use super::errors;
use super::keys::{
    DeleteItem, LIST_CONTEXT, OpenItem, OpenLogs, OpenMenu, SelectFirst, SelectLast, SelectNext,
    SelectPageDown, SelectPageUp, SelectPrev,
};
use super::model::{
    BrowserPreset, ConnChip, ContainerProblem, ContainerSort, Page, Step, compact_ports,
    container_forward_preset, container_groups, container_problem, event_time, image_name,
    image_removable, is_container_running, matches_any, network_removable, page_rows, port_owners,
    published_port, short_id, step_row, volume_removable,
};
use super::panel::ActRequest;
use super::processes::WIDE_FROM;
use super::shared::{
    self, Feed, KeyMenu, Memo, OpVerb, PageContext, RowOps, Tone, ToolPage, action_item, badge,
    dot, error_line, hint_line, info_line, loading_text, notice, op_button,
};
use crate::monitor_bar::level_color;
use crate::monitor_model::{Level, PERCENT, format_uptime};
use crate::prose::prose;
use crate::sftp::list_model::{format_size, visible_name};
use crate::t;

/// 容器详情中显示的日志行数。
const TAIL_LINES: u32 = 20;
/// 页显示时的刷新间隔（上一次返回后再计时）。
const INTERVAL: Duration = Duration::from_secs(15);
/// 用键盘移动选中的容器后，停这么久再读它的详情（按住方向键时不逐个读）。
const KEY_DETAIL_DELAY: Duration = Duration::from_millis(250);
/// 行高（rem）：容器与镜像等两行（列表够宽时容器一行），项目的标题一行。
const ROW_HEIGHT: f32 = 2.5;
const WIDE_ROW_HEIGHT: f32 = 1.75;
const GROUP_HEIGHT: f32 = 1.75;
/// 列表够宽时容器表格中各列的宽度（rem）：端口、CPU、内存、状态；名称与镜像分剩下的。
const PORTS_WIDTH: f32 = 5.5;
const CPU_WIDTH: f32 = 3.5;
const MEM_WIDTH: f32 = 4.5;
const STATUS_WIDTH: f32 = 9.;

/// 从别的页跳过来要选中的容器。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ContainerTarget {
    /// 按 ID（进程的“所属容器”）
    Id(String),
    /// 发布了这个端口的容器（网络页的监听端口）：协议与主机端口
    Port(String, u16),
}

/// Docker 页的子页。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum DockerTab {
    Containers,
    /// 有异常的容器
    Problems,
    Images,
    Volumes,
    Networks,
}

impl DockerTab {
    const ALL: [DockerTab; 5] = [
        DockerTab::Containers,
        DockerTab::Problems,
        DockerTab::Images,
        DockerTab::Volumes,
        DockerTab::Networks,
    ];

    fn lists_containers(self) -> bool {
        matches!(self, DockerTab::Containers | DockerTab::Problems)
    }
}

/// 可以删除的对象。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ItemKind {
    Image,
    Volume,
    Network,
}

/// 容器列表的一行：项目的标题，或一个容器（在列表中的位置）。
enum ContainerRow {
    Group {
        /// 项目名；单独的容器为 `None`
        project: Option<String>,
        working_dir: Option<String>,
        /// 项目中运行着的容器数（不受筛选影响）
        running: usize,
        /// 项目中的全部容器（ID 与名称；不受筛选影响，整体启停用）
        containers: Vec<(String, String)>,
    },
    Container(usize),
}

/// 确认框的内容（另外写明主机与将执行的命令，见 [`DockerPage::confirm`]）。
struct Question {
    title: SharedString,
    description: Option<SharedString>,
    /// compose 项目整体操作时涉及的容器
    containers: Option<SharedString>,
    ok: SharedString,
    danger: bool,
}

/// 确认框中最多列出的容器名。
const MAX_LISTED: usize = 10;

/// 镜像、卷、网络列表的一行。
#[derive(Clone)]
struct ItemRow {
    kind: ItemKind,
    /// 删除用的 ID（卷为名称）
    id: String,
    /// 行的元素 ID：同一个镜像有几个标签时各占一行，ID 相同，要带上 `仓库:标签`
    key: String,
    name: String,
    detail: String,
    usage: SharedString,
    in_use: bool,
    removable: bool,
}

/// 资源占用：容器 ID → 读数。
type StatsMap = HashMap<String, ContainerStats>;

/// 选中的容器的详情与最近几行日志（读取中为 `None`；同一个容器重读时先留着上一次的）。
struct Detail {
    id: String,
    /// 读详情时列表中这个容器的运行状态：之后的定时刷新中它不变就不重读
    run: Option<RunState>,
    info: Option<Result<ContainerDetail, ToolsError>>,
    logs: Option<Result<LogResult, ToolsError>>,
}

pub(super) struct DockerPage {
    context: PageContext,
    feed: Feed<DockerInfo>,
    search: Entity<InputState>,
    tab: DockerTab,
    sort: ContainerSort,
    /// 选中的容器（ID）
    selected: Option<String>,
    /// 各行进行中与刚完成的操作（键为容器的 ID，镜像、卷、网络的行键）
    ops: RowOps<String>,
    /// 鼠标停在哪一行上（容器的 ID 或镜像等的行键；右端浮出这一行的操作）
    hover_row: Option<String>,
    /// 上一帧列表的宽度（够宽时容器是一行一个的表格）
    width: Rc<Cell<Pixels>>,
    scroll: VirtualListScrollHandle,
    /// 上方页签（容器、异常、镜像、卷、网络）的滚动状态：按它量页签排开的宽度
    tabs_scroll: ScrollHandle,
    /// 容器页、其他子页的行（读数、搜索、子页、排序、资源占用变了时重算）
    container_rows:
        Memo<DockerInfo, (String, DockerTab, ContainerSort, u64), Rc<Vec<ContainerRow>>>,
    item_rows: Memo<DockerInfo, (String, DockerTab), (Rc<Vec<ItemRow>>, SharedString)>,
    problems: Memo<DockerInfo, (), usize>,
    /// 资源占用（对应 `stats_of` 那一份列表）；每次读到加一 `stats_gen`
    stats: Option<Rc<StatsMap>>,
    stats_of: Option<Rc<DockerInfo>>,
    stats_gen: u64,
    stats_error: Option<ToolsError>,
    detail: Option<Detail>,
    /// 列表的焦点（键盘操作）
    focus: FocusHandle,
    key_menu: KeyMenu,
    /// 镜像、卷、网络子页的当前行（键盘移动；这些没有选中的说法）
    item_cursor: Option<String>,
    /// 从别的页跳过来、列表还没读到时要选中的容器
    pending: Option<ContainerTarget>,
    /// 跳过来的容器不在列表中时的说明
    note: Option<SharedString>,
    /// 暂停刷新：鼠标停在列表上、右键菜单（它关闭或被释放时才恢复）或确认框打开着
    hovering: bool,
    menu_open: Option<EntityId>,
    /// 右键菜单打开着的那一行（右键不改变选中：选中会让详情出现、把列表挤矮，按下位置所在的行被挤开后
    /// gpui-kit 就画不出它的菜单）：菜单打开期间这一行高亮
    menu_row: Option<String>,
    /// 下一次读到列表后连同详情与日志一起重读（点了刷新、自己操作之后）
    reload_detail: bool,
    confirming: bool,
    /// 详情区顶上的分隔条（键盘调整高度）与详情这一帧的高度
    splitter: FocusHandle,
    detail_height: Rc<Cell<Pixels>>,
    _stats: Option<Task<()>>,
    _detail: Option<Task<()>>,
    _detail_logs: Option<Task<()>>,
    /// 打开着的清理对话框（它关闭、被释放时恢复刷新）
    cleanup: Option<WeakEntity<CleanupDialog>>,
    _cleanup: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl ToolPage for DockerPage {
    type Data = DockerInfo;

    fn feed(&mut self) -> &mut Feed<DockerInfo> {
        &mut self.feed
    }

    fn context(&self) -> &PageContext {
        &self.context
    }

    fn is_paused(&self) -> bool {
        self.hovering || self.menu_open.is_some() || self.confirming || self.cleanup.is_some()
    }

    /// 列表读到后：容器子页显示着时读资源占用，选中的容器重读详情（操作之后状态变了），之后定时刷新。
    fn after_load(&mut self, cx: &mut Context<Self>) {
        if self.feed.data().is_some()
            && let Some(target) = self.pending.take()
        {
            self.reveal(&target, cx);
        }
        self.drop_hidden(cx);
        self.load_stats(cx);
        self.sync_detail_after_list(cx);
        self.sync_poll(cx);
    }
}

impl DockerPage {
    pub fn new(context: PageContext, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = shared::search_input(|| t!("tools.docker.search"), window, cx);
        let subscriptions = vec![cx.subscribe(&search, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.drop_hidden(cx);
                cx.notify();
            }
        })];
        Self {
            feed: Feed::new(&context),
            context,
            search,
            tab: DockerTab::Containers,
            sort: ContainerSort::Project,
            selected: None,
            ops: RowOps::default(),
            hover_row: None,
            width: Rc::new(Cell::new(px(0.))),
            scroll: VirtualListScrollHandle::new(),
            tabs_scroll: ScrollHandle::new(),
            container_rows: Memo::default(),
            item_rows: Memo::default(),
            problems: Memo::default(),
            stats: None,
            stats_of: None,
            stats_gen: 0,
            stats_error: None,
            detail: None,
            focus: shared::list_focus(cx),
            key_menu: KeyMenu::default(),
            item_cursor: None,
            pending: None,
            note: None,
            hovering: false,
            menu_open: None,
            menu_row: None,
            reload_detail: false,
            confirming: false,
            splitter: cx.focus_handle().tab_stop(true),
            detail_height: Rc::new(Cell::new(px(0.))),
            _stats: None,
            _detail: None,
            _detail_logs: None,
            cleanup: None,
            _cleanup: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn set_connection(&mut self, target: Option<Target>, _: &mut Context<Self>) {
        self.context.target = target;
    }

    pub fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if shared::activate(self, active, cx) && !active {
            // 不显示时不再读资源占用与详情
            self._stats = None;
            self._detail = None;
            self._detail_logs = None;
        }
        if !active {
            // 不显示时收不到鼠标移出、菜单关闭：暂停的标志随之复位，再显示时照常刷新
            self.hovering = false;
            self.menu_open = None;
            self.menu_row = None;
            self.hover_row = None;
        }
    }

    /// 定时刷新：显示中时进行（第一次读完之后开始；暂停期间到点时跳过），没有 Docker、没有权限时不再自动刷新；
    /// 恢复时先显示暂停期间读到的结果。
    fn sync_poll(&mut self, cx: &mut Context<Self>) {
        if !self.is_paused() {
            shared::resume(self, cx);
        }
        let unavailable = matches!(
            self.feed.error,
            Some(
                ToolsError::NoDocker
                    | ToolsError::NeedRoot { .. }
                    | ToolsError::Exec(ExecError::SingleChannel)
            )
        );
        if unavailable {
            shared::stop_poll(self);
        } else if self.feed.active && !self.feed.is_polling() && !self.feed.loading {
            shared::start_poll(self, INTERVAL, cx);
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

    fn set_hover_row(&mut self, key: String, hovered: bool, cx: &mut Context<Self>) {
        if shared::hover_row(&mut self.hover_row, key, hovered) {
            cx.notify();
        }
    }

    /// 列表够宽（[`WIDE_FROM`]）：容器是一行一个的表格。
    fn wide(&self, window: &Window) -> bool {
        self.width.get() >= rems(WIDE_FROM).to_pixels(window.rem_size())
    }

    fn set_menu_open(&mut self, menu: EntityId, id: String, cx: &mut Context<Self>) {
        if self.menu_open != Some(menu) {
            self.menu_open = Some(menu);
            self.menu_row = Some(id);
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

    /// 选中的容器被子页（异常）、搜索藏起来时取消选中：详情只留列表中看得到的容器。镜像、卷、网络子页没有详情，不动。
    fn drop_hidden(&mut self, cx: &mut Context<Self>) {
        if !self.tab.lists_containers() {
            return;
        }
        let (Some(id), Some(info)) = (self.selected.clone(), self.feed.data()) else {
            return;
        };
        let rows = self.shown_container_rows(&info, cx);
        let shown = rows
            .iter()
            .any(|row| matches!(row, ContainerRow::Container(ix) if info.containers[*ix].id == id));
        if !shown {
            self.selected = None;
            self.detail = None;
            self._detail = None;
            self._detail_logs = None;
            cx.notify();
        }
    }

    /// 切换子页。
    fn set_tab(&mut self, tab: DockerTab, cx: &mut Context<Self>) {
        self.tab = tab;
        self.item_cursor = None;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        self.drop_hidden(cx);
        self.load_stats(cx);
        cx.notify();
    }

    /// 上方页签按文案排开比可见区域宽出多少（[`crate::tab_fit::overflow`]）：面板据此放宽最小宽度。
    pub fn tabs_overflow(&self) -> Option<Pixels> {
        crate::tab_fit::overflow(&self.tabs_scroll, DockerTab::ALL.len())
    }

    // ------------------------------------------------------------ 资源占用与详情

    /// 读资源占用：页显示着、是容器（或异常）子页、有运行中的容器，且这一份列表还没有读过时。与列表分开读：
    /// 列表先显示，读数到了再补上。
    fn load_stats(&mut self, cx: &mut Context<Self>) {
        if !self.feed.active || !self.tab.lists_containers() {
            return;
        }
        let Some(info) = self.feed.data() else {
            return;
        };
        if self
            .stats_of
            .as_ref()
            .is_some_and(|of| Rc::ptr_eq(of, &info))
        {
            return;
        }
        self.stats_of = Some(info.clone());
        if !info.containers.iter().any(|c| c.state == "running") {
            self.stats = None;
            self.stats_error = None;
            self.stats_gen += 1;
            return;
        }
        let Some(target) = self.context.target.clone() else {
            return;
        };
        let tools = self.context.tools.clone();
        let (engine, sudo) = (info.engine, info.sudo);
        self._stats = Some(cx.spawn(async move |this, cx| {
            let result = tools.container_stats(&target, engine, sudo).await;
            let _ = this.update(cx, |page, cx| {
                // 读的时候列表已经换了：下一次再读
                if !page
                    .stats_of
                    .as_ref()
                    .is_some_and(|of| Rc::ptr_eq(of, &info))
                {
                    return;
                }
                match result {
                    Ok(list) => {
                        let map: StatsMap = info
                            .containers
                            .iter()
                            .filter_map(|c| {
                                list.iter()
                                    .find(|s| s.is_for(&c.id))
                                    .map(|s| (c.id.clone(), s.clone()))
                            })
                            .collect();
                        page.stats = Some(Rc::new(map));
                        page.stats_error = None;
                    }
                    Err(error) => {
                        tracing::debug!("主机工具：读资源占用失败：{error}");
                        page.stats_error = Some(error);
                    }
                }
                page.stats_gen += 1;
                cx.notify();
            });
        }));
    }

    fn select(&mut self, id: &str, cx: &mut Context<Self>) {
        self.select_after(id, None, cx);
    }

    /// 这个容器是选中的、详情已经读到时，详情中的运行状态（比列表新：列表与详情写同样的时长）。
    fn detail_run(&self, id: &str) -> Option<RunState> {
        let detail = self.detail.as_ref().filter(|d| d.id == id)?;
        let d = detail.info.as_ref()?.as_ref().ok()?;
        Some(RunState {
            status: d.status.clone(),
            started_at: d.started_at,
            finished_at: d.finished_at,
            exit_code: d.exit_code,
            health: d.health.as_ref().and_then(|h| health_state(&h.status)),
            oom_killed: d.oom_killed,
        })
    }

    /// 选中一个容器，`delay` 之后读它的详情（键盘移动时：停下来才读）。
    fn select_after(&mut self, id: &str, delay: Option<Duration>, cx: &mut Context<Self>) {
        self.note = None;
        if self.selected.as_deref() != Some(id) {
            self.selected = Some(id.to_string());
            self.sync_detail(delay, cx);
            self.scroll_to_selected(cx);
            cx.notify();
        }
    }

    /// 把选中的容器滚进可见范围：详情出现、读到内容变高后列表变矮，点中的行可能被挤到详情下面
    /// （下一帧布局时按那时的高度滚）。
    fn scroll_to_selected(&mut self, cx: &App) {
        let (Some(info), Some(id)) = (self.feed.data(), self.selected.clone()) else {
            return;
        };
        let rows = self.shown_container_rows(&info, cx);
        if let Some(ix) = rows
            .iter()
            .position(|r| matches!(r, ContainerRow::Container(i) if info.containers[*i].id == id))
        {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
    }

    /// 列表的焦点（面板切到这一页、跳过来时放进来）。
    pub fn list_focus(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// 搜索框（列表显示着时才有：还在读取、不可用时没有搜索框）。
    pub fn search(&self) -> Option<Entity<InputState>> {
        self.feed.data().map(|_| self.search.clone())
    }

    /// 容器子页（或异常子页）的行（读数、搜索、子页、排序、资源占用变了时重算）。
    fn shown_container_rows(&mut self, info: &Rc<DockerInfo>, cx: &App) -> Rc<Vec<ContainerRow>> {
        let query = self.search.read(cx).value().to_string();
        let stats = self.stats.clone();
        self.container_rows.get(
            info,
            (query, self.tab, self.sort, self.stats_gen),
            |info, (query, tab, sort, _)| {
                Rc::new(container_rows(info, query, *tab, *sort, stats.as_deref()))
            },
        )
    }

    /// 镜像、卷、网络子页的行与没有内容时的说明。
    fn shown_item_rows(
        &mut self,
        info: &Rc<DockerInfo>,
        cx: &App,
    ) -> (Rc<Vec<ItemRow>>, SharedString) {
        let query = self.search.read(cx).value().to_string();
        self.item_rows
            .get(info, (query, self.tab), |info, (query, tab)| {
                let (items, empty) = item_rows(info, query, *tab);
                (Rc::new(items), empty)
            })
    }

    /// 从别的页跳过来：切到容器子页、清空搜索，选中并滚到这个容器；还没有数据时读到后再选中。
    pub fn focus_container(
        &mut self,
        target: ContainerTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search
            .update(cx, |search, cx| search.set_value("", window, cx));
        self.tab = DockerTab::Containers;
        self.note = None;
        if self.feed.data().is_some() {
            self.reveal(&target, cx);
            self.load_stats(cx);
        } else {
            self.pending = Some(target);
        }
        cx.notify();
    }

    /// 选中并滚到这个容器；不在显示着的列表（可能是缓存）中而正在读取时读到后再找，不在列表中时说明。
    fn reveal(&mut self, target: &ContainerTarget, cx: &mut Context<Self>) {
        let Some(info) = self.feed.data() else {
            return;
        };
        let found = match target {
            ContainerTarget::Id(id) => info.containers.iter().position(|c| &c.id == id),
            ContainerTarget::Port(proto, port) => port_owners(&info.containers)
                .get(&(proto.clone(), *port))
                .copied(),
        };
        let Some(found) = found else {
            if self.feed.loading {
                self.pending = Some(target.clone());
                return;
            }
            self.note = Some(match target {
                ContainerTarget::Id(id) => {
                    t!(
                        "tools.jump.container_missing",
                        id = short_id(id).to_string()
                    )
                }
                ContainerTarget::Port(_, port) => t!("tools.jump.port_missing", port = port),
            });
            cx.notify();
            return;
        };
        let id = info.containers[found].id.clone();
        self.select(&id, cx);
        let rows = self.shown_container_rows(&info, cx);
        if let Some(ix) = rows
            .iter()
            .position(|r| matches!(r, ContainerRow::Container(i) if *i == found))
        {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Center);
        }
    }

    // ------------------------------------------------------------ 键盘

    /// ↑ ↓ 等：容器子页在容器之间移动并选中（跳过项目的标题），镜像、卷、网络移动当前行；滚动跟随。
    fn move_cursor(&mut self, step: Step, window: &mut Window, cx: &mut Context<Self>) {
        let Some(info) = self.feed.data() else {
            return;
        };
        let rem = window.rem_size();
        let viewport = f32::from(self.scroll.bounds().size.height);
        if self.tab.lists_containers() {
            let rows = self.shown_container_rows(&info, cx);
            let current = self.selected.as_deref().and_then(|id| {
                rows.iter().position(
                    |r| matches!(r, ContainerRow::Container(i) if info.containers[*i].id == id),
                )
            });
            let page = page_rows(viewport, f32::from(rems(ROW_HEIGHT).to_pixels(rem)));
            let selectable = |ix: usize| matches!(rows[ix], ContainerRow::Container(_));
            let Some(ix) = step_row(current, rows.len(), step, page, selectable) else {
                return;
            };
            if let ContainerRow::Container(i) = rows[ix] {
                let id = info.containers[i].id.clone();
                self.select_after(&id, Some(KEY_DETAIL_DELAY), cx);
            }
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        } else {
            let (items, _) = self.shown_item_rows(&info, cx);
            let current = self
                .item_cursor
                .as_deref()
                .and_then(|key| items.iter().position(|item| item.key == key));
            let page = page_rows(viewport, f32::from(rems(ROW_HEIGHT).to_pixels(rem)));
            let Some(ix) = step_row(current, items.len(), step, page, |_| true) else {
                return;
            };
            self.item_cursor = Some(items[ix].key.clone());
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    /// 选中的容器（容器、异常子页）。
    fn selected_container(&self) -> Option<(Rc<DockerInfo>, Container)> {
        let info = self.feed.data()?;
        let id = self.selected.as_deref()?;
        let c = info.containers.iter().find(|c| c.id == id)?.clone();
        Some((info, c))
    }

    /// 镜像、卷、网络子页的当前行。
    fn cursor_item(&mut self, cx: &App) -> Option<(Engine, ItemRow)> {
        let info = self.feed.data()?;
        let key = self.item_cursor.clone()?;
        let (items, _) = self.shown_item_rows(&info, cx);
        let item = items.iter().find(|item| item.key == key)?.clone();
        Some((info.engine, item))
    }

    /// Enter：重读选中的容器的详情；没有选中的时选中第一个。
    fn open_item(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.tab.lists_containers() {
            return;
        }
        if self.selected.is_some() {
            self.sync_detail(None, cx);
        } else {
            self.move_cursor(Step::First, window, cx);
        }
    }

    /// L：查看选中的容器的日志。
    fn logs_item(&mut self, cx: &mut Context<Self>) {
        if self.tab.lists_containers()
            && let Some((info, c)) = self.selected_container()
        {
            self.open_logs(&info, &c, cx);
        }
    }

    /// Delete：运行中的容器停止，已停止的删除；镜像、卷、网络未使用的删除（都先确认；操作进行中的不再确认）。
    fn delete_item(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tab.lists_containers() {
            if let Some((info, c)) = self
                .selected_container()
                .filter(|(_, c)| !self.ops.busy(&c.id))
            {
                let op = if is_container_running(&c) {
                    ContainerOp::Stop
                } else {
                    ContainerOp::Remove
                };
                self.container_action(info.engine, vec![(c.id, c.name)], op, None, window, cx);
            }
        } else if let Some((engine, item)) = self.cursor_item(cx)
            && item.removable
            && !self.ops.busy(&item.key)
        {
            self.remove_item(engine, &item, window, cx);
        }
    }

    /// Shift+F10、菜单键：在选中的容器（镜像、卷、网络的当前行）下面打开它的右键菜单。
    fn open_key_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.tab.lists_containers() {
            if let Some((engine, item)) = self.cursor_item(cx) {
                let build = self.item_menu_builder(engine, &item, cx);
                KeyMenu::open(
                    window,
                    cx,
                    self.focus.clone(),
                    |page| &mut page.key_menu,
                    build,
                );
            }
            return;
        }
        if let Some((info, c)) = self.selected_container() {
            let build = self.menu_builder(&info, &c, cx);
            KeyMenu::open(
                window,
                cx,
                self.focus.clone(),
                |page| &mut page.key_menu,
                build,
            );
        }
    }

    /// 点了刷新、自己操作之后（含清理之后）：连同选中的容器的详情与最近日志一起重读。
    pub(super) fn reload(&mut self, cx: &mut Context<Self>) {
        self.reload_detail = true;
        shared::refresh(self, false, cx);
    }

    /// 读到列表后：选中的容器的运行状态（状态、启动与结束时间、退出码、健康）变了、上次没读到详情，或者是点了
    /// 刷新、自己操作之后的重读，才重读详情与最近日志——定时刷新不每次都读。容器已不在列表中时取消选中。
    fn sync_detail_after_list(&mut self, cx: &mut Context<Self>) {
        let reload = std::mem::take(&mut self.reload_detail);
        let current = match (&self.detail, self.feed.data()) {
            (Some(detail), Some(info)) => {
                self.selected.as_deref() == Some(detail.id.as_str())
                    && detail_is_current(detail, &info)
            }
            _ => false,
        };
        if reload || !current {
            self.sync_detail(None, cx);
        }
    }

    /// 读选中的容器的详情与最近几行日志（选中时、读到列表后状态变了时）；容器已不在列表中时取消选中。`delay` 之后
    /// 再读（键盘移动时）。
    fn sync_detail(&mut self, delay: Option<Duration>, cx: &mut Context<Self>) {
        let Some(info) = self.feed.data() else {
            return;
        };
        let Some((id, run)) = self
            .selected
            .as_ref()
            .and_then(|id| info.containers.iter().find(|c| &c.id == id))
            .map(|c| (c.id.clone(), c.run.clone()))
        else {
            self.selected = None;
            self.detail = None;
            self._detail = None;
            self._detail_logs = None;
            return;
        };
        match self.detail.as_mut().filter(|d| d.id == id) {
            Some(detail) => detail.run = run,
            None => {
                self.detail = Some(Detail {
                    id: id.clone(),
                    run,
                    info: None,
                    logs: None,
                })
            }
        }
        let Some(target) = self.context.target.clone() else {
            if let Some(detail) = self.detail.as_mut().filter(|d| d.info.is_none()) {
                detail.info = Some(Err(ToolsError::Exec(ExecError::Disconnected)));
            }
            return;
        };
        let tools = self.context.tools.clone();
        let (engine, sudo) = (info.engine, info.sudo);
        let (inspect_tools, inspect_target, inspect_id) =
            (tools.clone(), target.clone(), id.clone());
        self._detail = Some(cx.spawn(async move |this, cx| {
            if let Some(delay) = delay {
                cx.background_executor().timer(delay).await;
            }
            let result = inspect_tools
                .container_detail(&inspect_target, engine, &inspect_id, sudo)
                .await;
            let _ = this.update(cx, |page, cx| {
                if let Some(detail) = page.detail.as_mut().filter(|d| d.id == inspect_id) {
                    let first = detail.info.is_none();
                    detail.info = Some(result);
                    if first {
                        page.scroll_to_selected(cx);
                    }
                    cx.notify();
                }
            });
        }));
        let query = LogQuery::recent(
            LogSource::Container {
                engine,
                id: id.clone(),
                sudo,
            },
            TAIL_LINES,
        );
        self._detail_logs = Some(cx.spawn(async move |this, cx| {
            if let Some(delay) = delay {
                cx.background_executor().timer(delay).await;
            }
            let result = tools.logs(&target, query).await;
            let _ = this.update(cx, |page, cx| {
                if let Some(detail) = page.detail.as_mut().filter(|d| d.id == id) {
                    let first = detail.logs.is_none();
                    detail.logs = Some(result);
                    if first {
                        page.scroll_to_selected(cx);
                    }
                    cx.notify();
                }
            });
        }));
    }

    // ------------------------------------------------------------ 操作

    /// 执行操作，之后重读：`keys` 这几行（容器的 ID、镜像等的行键）在进行中不能再点。经 sudo 访问 Docker 时，权限不足
    /// 的说明另提示加入 docker 组。
    fn perform(
        &mut self,
        request: ActRequest,
        keys: Vec<String>,
        verb: OpVerb,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let context = self.context.clone();
        let Ok(done) = self
            .context
            .panel
            .update(cx, |panel, cx| panel.perform(&context, request, window, cx))
        else {
            return;
        };
        shared::track_op(
            self,
            |page| &mut page.ops,
            keys,
            verb,
            done,
            |page, _, cx| page.reload(cx),
            cx,
        );
    }

    /// 容器（或 compose 项目的全部容器）的启动、停止、重启、删除：启动不确认，其余先确认。
    fn container_action(
        &mut self,
        engine: Engine,
        containers: Vec<(String, String)>,
        op: ContainerOp,
        project: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((_, first)) = containers.first() else {
            return;
        };
        let name = project.clone().unwrap_or_else(|| first.clone());
        let shown = visible_name(&name);
        let title = match &project {
            Some(_) => project_action(op, &shown),
            None => container_action_title(op, &shown),
        };
        let request = ActRequest {
            action: Action::Container {
                engine,
                op,
                ids: containers.iter().map(|(id, _)| id.clone()).collect(),
            },
            title,
            label: match &project {
                Some(project) => format!("compose: {project}"),
                None => name.clone(),
            },
            docker: true,
            page: Page::Docker,
        };
        let keys: Vec<String> = containers.iter().map(|(id, _)| id.clone()).collect();
        let verb = container_op_verb(op);
        if op == ContainerOp::Start {
            self.perform(request, keys, verb, window, cx);
            return;
        }
        let confirm = match &project {
            Some(_) => confirm_project(op, &shown, containers.len()),
            None => confirm_container(op, &shown),
        };
        let listed = project.is_some().then(|| {
            let mut names: Vec<String> = containers
                .iter()
                .take(MAX_LISTED)
                .map(|(_, name)| visible_name(name))
                .collect();
            if containers.len() > MAX_LISTED {
                names.push("…".into());
            }
            t!(
                "tools.docker.project_containers",
                names = names.join(&t!("tools.list_separator"))
            )
        });
        // 后果：停止后它提供的服务不可用、重启期间短暂中断、删除不可恢复
        let description = match (op, project.is_some()) {
            (ContainerOp::Remove, _) => Some(t!("tools.docker.remove_irreversible")),
            (ContainerOp::Stop, false) => Some(t!("tools.docker.stop_effect")),
            (ContainerOp::Stop, true) => Some(t!("tools.docker.project_stop_effect")),
            (ContainerOp::Restart, false) => Some(t!("tools.docker.restart_effect")),
            (ContainerOp::Restart, true) => Some(t!("tools.docker.project_restart_effect")),
            (ContainerOp::Start, _) => None,
        };
        let question = Question {
            title: confirm,
            description,
            containers: listed,
            ok: container_op_label(op),
            danger: matches!(op, ContainerOp::Remove | ContainerOp::Stop),
        };
        self.confirm(question, request, keys, verb, window, cx);
    }

    /// 删除镜像、卷、网络（只有未使用的）。
    fn remove_item(
        &mut self,
        engine: Engine,
        item: &ItemRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shown = visible_name(&item.name);
        let ids = vec![item.id.clone()];
        let (title, action) = match item.kind {
            ItemKind::Image => (
                t!("tools.docker.remove_image_action", name = shown.clone()),
                Action::RemoveImages { engine, ids },
            ),
            ItemKind::Volume => (
                t!("tools.docker.remove_volume_action", name = shown.clone()),
                Action::RemoveVolumes { engine, names: ids },
            ),
            ItemKind::Network => (
                t!("tools.docker.remove_network_action", name = shown.clone()),
                Action::RemoveNetworks { engine, ids },
            ),
        };
        let confirm = match item.kind {
            ItemKind::Image => t!("tools.docker.confirm_remove_image", name = shown),
            ItemKind::Volume => t!("tools.docker.confirm_remove_volume", name = shown),
            ItemKind::Network => t!("tools.docker.confirm_remove_network", name = shown),
        };
        let request = ActRequest {
            action,
            title,
            label: item.name.clone(),
            docker: true,
            page: Page::Docker,
        };
        let question = Question {
            title: confirm,
            description: Some(t!("tools.docker.remove_irreversible")),
            containers: None,
            ok: t!("tools.docker.remove"),
            danger: true,
        };
        self.confirm(
            question,
            request,
            vec![item.key.clone()],
            OpVerb::Remove,
            window,
            cx,
        );
    }

    /// 确认后执行：正文写明主机、后果、涉及的容器与将执行的命令（Docker 页经免密码 sudo 读取时，操作同样经 sudo；
    /// 容器等的 ID 只写前 12 位，审计照记完整的）。
    fn confirm(
        &mut self,
        question: Question,
        request: ActRequest,
        keys: Vec<String>,
        verb: OpVerb,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Question {
            title,
            description,
            containers,
            ok,
            danger,
        } = question;
        let host = self.context.host(cx);
        let sudo = self.feed.data().is_some_and(|info| info.sudo);
        let command = shared::confirm_line(&request.action, sudo);
        let page = cx.entity().downgrade();
        let dialogs = self.context.dialogs.clone();
        let request = Rc::new(std::cell::RefCell::new(Some((request, keys))));
        self.set_confirming(true, cx);
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, cx| {
            let (page, request) = (page.clone(), request.clone());
            let page_close = page.clone();
            let body = v_flex()
                .id("tools-docker-confirm")
                .test_support()
                .gap_1p5()
                .text_xs()
                .children(host.as_ref().map(|host| shared::confirm_host(host, cx)))
                .children(description.clone().map(|text| {
                    div()
                        .id("tools-docker-confirm-effect")
                        .test_support()
                        .child(prose(text))
                }))
                .children(containers.clone().map(prose))
                .child(shared::confirm_command(
                    "tools-confirm-command",
                    t!("tools.confirm.will_run"),
                    &command,
                    cx,
                ));
            alert
                .confirm()
                .title(title.clone())
                .description(body)
                .ok_text(ok.clone())
                .ok_variant(if danger {
                    ButtonVariant::Danger
                } else {
                    ButtonVariant::Primary
                })
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, window, cx| {
                    if let Some((request, keys)) = request.borrow_mut().take() {
                        let _ = page
                            .update(cx, |page, cx| page.perform(request, keys, verb, window, cx));
                    }
                    true
                })
                .on_close(move |_, _, cx| {
                    let _ = page_close.update(cx, |page, cx| page.set_confirming(false, cx));
                })
        });
        dialogs.opened(window, cx);
    }

    /// 在中间区域的 Tab 中查看日志。
    fn open_logs(&self, info: &DockerInfo, c: &Container, cx: &mut Context<Self>) {
        let context = self.context.clone();
        let source = LogSource::Container {
            engine: info.engine,
            id: c.id.clone(),
            sudo: info.sudo,
        };
        let name = c.name.clone();
        let _ = self
            .context
            .panel
            .update(cx, |panel, cx| panel.open_logs(&context, source, name, cx));
    }

    fn follow_logs(
        &self,
        info: &DockerInfo,
        c: &Container,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request = TerminalRequest::ContainerLogs {
            engine: info.engine,
            id: c.id.clone(),
            sudo: info.sudo,
        };
        let title = t!("tools.tab_logs", name = c.name.clone());
        self.run(request, title, window, cx);
    }

    fn open_shell(
        &self,
        info: &DockerInfo,
        c: &Container,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request = TerminalRequest::ContainerShell {
            engine: info.engine,
            id: c.id.clone(),
            sudo: info.sudo,
        };
        let title = t!("tools.tab_shell", name = c.name.clone());
        self.run(request, title, window, cx);
    }

    fn run(
        &self,
        request: TerminalRequest,
        title: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let context = self.context.clone();
        let _ = self.context.panel.update(cx, |panel, cx| {
            panel.run_request(&context, request, title, window, cx)
        });
    }

    fn set_sort(&mut self, sort: ContainerSort, cx: &mut Context<Self>) {
        if self.sort != sort {
            self.sort = sort;
            self.scroll.scroll_to_item(0, ScrollStrategy::Top);
            cx.notify();
        }
    }

    // ------------------------------------------------------------ 渲染

    fn render_group(
        &self,
        engine: Engine,
        row: &ContainerRow,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ContainerRow::Group {
            project,
            working_dir,
            running,
            containers,
        } = row
        else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let key = project.clone().unwrap_or_default();
        let busy = self.ops.any_busy(containers.iter().map(|(id, _)| id));
        let (running, total) = (*running, containers.len());
        let op_button = |op: ContainerOp,
                         icon: IconName,
                         tooltip: SharedString,
                         cx: &mut Context<Self>| {
            let (project, containers) = (project.clone(), containers.clone());
            // 停止项目是破坏性操作：图标用危险色（与详情、菜单一致）
            let icon = match op {
                ContainerOp::Stop => Icon::new(icon).text_color(danger),
                _ => Icon::new(icon),
            };
            Button::new(SharedString::from(format!(
                "tools-project-{}-{key}",
                container_op_key(op)
            )))
            .ghost()
            .small()
            .icon(icon)
            .tooltip(tooltip)
            .disabled(busy)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.container_action(engine, containers.clone(), op, project.clone(), window, cx)
            }))
        };
        // 只显示当前能做的：全部运行着时不能再启动，全部停着时不能停止、重启
        let has = project.is_some();
        h_flex()
            .id(SharedString::from(format!("tools-container-group-{key}")))
            .test_support()
            .size_full()
            .px_1()
            .gap_1()
            .text_xs()
            .bg(theme.muted.opacity(0.4))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_medium()
                    .child(prose(match project {
                        Some(project) => visible_name(project).into(),
                        None => t!("tools.docker.standalone"),
                    })),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(muted)
                    .child(format!("{running}/{total}")),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(muted.opacity(0.7))
                    .children(working_dir.clone()),
            )
            .when(has && running < total, |this| {
                this.child(op_button(
                    ContainerOp::Start,
                    IconName::Play,
                    t!("tools.docker.project_start"),
                    cx,
                ))
            })
            .when(has && running > 0, |this| {
                this.child(op_button(
                    ContainerOp::Stop,
                    IconName::Square,
                    t!("tools.docker.project_stop"),
                    cx,
                ))
                .child(op_button(
                    ContainerOp::Restart,
                    IconName::RotateCcw,
                    t!("tools.docker.project_restart"),
                    cx,
                ))
            })
            .into_any_element()
    }

    /// 容器的右键菜单（鼠标右键与键盘打开的、详情头部的 ⋯ 都是同一个）：启停、日志、进入容器、查看进程、复制 ID、
    /// 删除（停止、删除用警示样式）。打开期间暂停刷新。
    fn menu_builder(
        &self,
        info: &Rc<DockerInfo>,
        c: &Container,
        cx: &mut Context<Self>,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
        let page = cx.entity().downgrade();
        let (info, container) = (info.clone(), c.clone());
        let running = is_container_running(c);
        let busy = self.ops.busy(&c.id);
        let context = self.context.clone();
        move |menu, window, cx| {
            let menu_id = cx.entity_id();
            let row = container.id.clone();
            let _ = page.update(cx, |page, cx| page.set_menu_open(menu_id, row, cx));
            let page_close = page.clone();
            shared::on_menu_dismiss(window, cx, move |cx| {
                let _ = page_close.update(cx, |page, cx| page.menu_closed(menu_id, cx));
            });
            let c = container.clone();
            let engine = info.engine;
            let one = vec![(c.id.clone(), c.name.clone())];
            let act = |op: ContainerOp| {
                let (page, one) = (page.clone(), one.clone());
                move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                    let _ = page.update(cx, |page, cx| {
                        page.container_action(engine, one.clone(), op, None, window, cx)
                    });
                }
            };
            let mut menu = if running {
                menu.item(
                    action_item(
                        t!("tools.docker.op_stop"),
                        IconName::Square,
                        true,
                        false,
                        cx,
                    )
                    .disabled(busy)
                    .on_click(act(ContainerOp::Stop)),
                )
                .item(
                    PopupMenuItem::new(t!("tools.docker.op_restart"))
                        .icon(IconName::RotateCcw)
                        .disabled(busy)
                        .on_click(act(ContainerOp::Restart)),
                )
            } else {
                menu.item(
                    PopupMenuItem::new(t!("tools.docker.op_start"))
                        .icon(IconName::Play)
                        .disabled(busy)
                        .on_click(act(ContainerOp::Start)),
                )
            };
            let (logs_page, logs_info, logs_c) = (page.clone(), info.clone(), c.clone());
            let (follow_page, follow_info, follow_c) = (page.clone(), info.clone(), c.clone());
            menu = menu
                .separator()
                .item(
                    PopupMenuItem::new(t!("tools.logs.view"))
                        .icon(IconName::ScrollText)
                        .on_click(move |_, _, cx| {
                            let _ = logs_page
                                .update(cx, |page, cx| page.open_logs(&logs_info, &logs_c, cx));
                        }),
                )
                .item(
                    PopupMenuItem::new(t!("tools.follow_logs"))
                        .icon(IconName::SquareTerminal)
                        .on_click(move |_, window, cx| {
                            let _ = follow_page.update(cx, |page, cx| {
                                page.follow_logs(&follow_info, &follow_c, window, cx)
                            });
                        }),
                );
            if running {
                let (shell_page, shell_c, shell_info) = (page.clone(), c.clone(), info.clone());
                menu = menu.item(
                    PopupMenuItem::new(t!("tools.docker.shell"))
                        .icon(IconName::Terminal)
                        .on_click(move |_, window, cx| {
                            let _ = shell_page.update(cx, |page, cx| {
                                page.open_shell(&shell_info, &shell_c, window, cx)
                            });
                        }),
                );
            }
            let (processes_context, processes_c) = (context.clone(), c.clone());
            menu = menu.item(
                PopupMenuItem::new(t!("tools.docker.show_processes"))
                    .icon(IconName::Cpu)
                    .on_click(move |_, window, cx| {
                        let (id, name) = (processes_c.id.clone(), visible_name(&processes_c.name));
                        shared::go(&processes_context, window, cx, move |panel, window, cx| {
                            panel.show_container_processes(id, name, window, cx)
                        });
                    }),
            );
            let copy_id = c.id.clone();
            menu = menu.item(
                PopupMenuItem::new(t!("tools.docker.copy_id"))
                    .icon(IconName::Copy)
                    .on_click(move |_, window, cx| {
                        shared::copy_text(
                            copy_id.clone(),
                            short_id(&copy_id).to_string(),
                            window,
                            cx,
                        )
                    }),
            );
            if !running {
                menu = menu.separator().item(
                    action_item(
                        t!("tools.docker.op_remove"),
                        IconName::Trash,
                        true,
                        false,
                        cx,
                    )
                    .disabled(busy)
                    .on_click(act(ContainerOp::Remove)),
                );
            }
            menu
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_container(
        &self,
        info: &Rc<DockerInfo>,
        c: &Container,
        stats: Option<&ContainerStats>,
        wide: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.selected.as_deref() == Some(c.id.as_str());
        let ring = selected && (self.focus.is_focused(window) || self.key_menu.is_open());
        let menu_layer = selected.then(|| self.key_menu.layer(window)).flatten();
        let menu = self.menu_builder(info, c, cx);
        // 选中的容器以详情读到的运行状态为准（比列表新），行与详情的时长一致
        let run = self.detail_run(&c.id).or_else(|| c.run.clone());
        let now = termlet_core::time::now_ms();
        let status = row_status(c, run.as_ref(), now);
        let status_color = state_tone(c, run.as_ref()).color(cx);
        let health = run
            .as_ref()
            .and_then(|run| run.health)
            .or_else(|| c.health())
            .and_then(|health| health_badge(health, cx));
        let problem = container_problem(c);
        let dot_color = match problem {
            Some(problem) => problem_tone(problem).color(cx),
            None => state_tone(c, run.as_ref()).dot(cx),
        };
        let menu_row = self.menu_row.as_deref() == Some(c.id.as_str());
        let hovered = self.hover_row.as_deref() == Some(c.id.as_str());
        let theme = cx.theme();
        let row_bg = if selected {
            theme.list_active
        } else {
            theme.list_hover
        };
        // 状态的位置：操作进行中、刚完成时说明；鼠标停在行上、键盘焦点在这一行、菜单打开着时浮出主要操作与 ⋯，盖在
        // 状态所在的一行的右端（状态文字让出来，不露出半截）
        let op = self.ops.get(&c.id);
        let actions = match op {
            Some(_) => None,
            None if hovered || menu_row || ring => {
                let op = if is_container_running(c) {
                    ContainerOp::Restart
                } else {
                    ContainerOp::Start
                };
                let engine = info.engine;
                let one = vec![(c.id.clone(), c.name.clone())];
                let primary = shared::row_action_button(
                    SharedString::from(format!(
                        "tools-container-row-{}-{}",
                        container_op_key(op),
                        c.id
                    )),
                    container_op_icon(op),
                    container_op_label(op),
                    false,
                    false,
                    window,
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.container_action(engine, one.clone(), op, None, window, cx)
                }))
                .into_any_element();
                let more = shared::more_button(
                    SharedString::from(format!("tools-container-row-more-{}", c.id)),
                    self.menu_builder(info, c, cx),
                    window,
                    cx,
                );
                Some(vec![primary, more])
            }
            None => None,
        }
        .map(|buttons| {
            shared::row_actions(
                SharedString::from(format!("tools-container-actions-{}", c.id)),
                row_bg,
                buttons,
                cx,
            )
        });
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let id: SharedString = c.id.clone().into();
        let ports = compact_ports(&c.ports);
        let status_tip: SharedString = match problem {
            Some(problem) => format!("{}\n{}", problem_label(problem), c.status).into(),
            None => c.status.clone().into(),
        };
        let select_id = c.id.clone();
        let hover_id = c.id.clone();
        let name = div()
            .id(SharedString::from(format!("tools-container-name-{}", c.id)))
            .flex_1()
            .min_w_0()
            .truncate()
            .child(visible_name(&c.name))
            .tooltip(shared::row_tooltip(id));
        let health = health.map(|badge| {
            badge
                .id(SharedString::from(format!(
                    "tools-container-health-{}",
                    c.id
                )))
                .test_support()
        });
        let covered = actions.is_some();
        let status = match op {
            Some(op) => shared::op_status(
                SharedString::from(format!("tools-container-op-{}", c.id)),
                op,
                cx,
            ),
            None => div()
                .id(SharedString::from(format!(
                    "tools-container-status-{}",
                    c.id
                )))
                .test_support()
                .truncate()
                .text_color(if covered {
                    theme.transparent
                } else {
                    status_color
                })
                .child(status)
                .tooltip(shared::row_tooltip(status_tip))
                .into_any_element(),
        };
        let row = v_flex()
            .id(SharedString::from(format!("tools-container-{}", c.id)))
            .test_support()
            .size_full()
            .justify_center()
            .pl_3()
            .pr_1()
            .text_xs()
            .cursor_default()
            .when(selected, |el| el.bg(theme.list_active))
            .when(!selected && menu_row, |el| el.bg(theme.list_hover))
            .when(!selected, |el| el.hover(|s| s.bg(theme.list_hover)));
        let row = shared::ring(row, ring, cx)
            .children(menu_layer)
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                this.set_hover_row(hover_id.clone(), *hovered, cx)
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.focus.focus(window, cx);
                this.select(&select_id, cx)
            }));
        let row = if wide {
            // 一行：● 名称（健康状态） | 镜像 | 端口 | CPU | 内存 | 状态（与表头对齐）
            let color = |value: Option<f64>| {
                value
                    .and_then(|v| level_color(Level::of(v, PERCENT), cx))
                    .unwrap_or(muted)
            };
            let (cpu, cpu_color, mem, mem_color) = match stats {
                Some(s) => (
                    cpu_text(s.cpu),
                    color(s.cpu),
                    s.mem_used.map(format_size).unwrap_or_else(|| "-".into()),
                    color(s.mem_percent),
                ),
                None => ("-".into(), muted, "-".into(), muted),
            };
            let ports_tip: SharedString = c.ports.clone().into();
            row.child(
                h_flex()
                    .relative()
                    .gap_2()
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1p5()
                            .child(dot(dot_color))
                            .child(name)
                            .children(health),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "tools-container-image-{}",
                                c.id
                            )))
                            .test_support()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(muted)
                            .child(c.image.clone())
                            .tooltip(shared::row_tooltip(c.image.clone().into())),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "tools-container-ports-{}",
                                c.id
                            )))
                            .test_support()
                            .flex_none()
                            .w(rems(PORTS_WIDTH))
                            .truncate()
                            .text_color(muted)
                            .child(ports)
                            .when(!c.ports.is_empty(), |this| {
                                this.tooltip(shared::row_tooltip(ports_tip))
                            }),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(rems(CPU_WIDTH))
                            .text_right()
                            .text_color(cpu_color)
                            .child(cpu),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(rems(MEM_WIDTH))
                            .text_right()
                            .text_color(mem_color)
                            .child(mem),
                    )
                    .child(div().flex_none().w(rems(STATUS_WIDTH)).child(status))
                    .children(actions),
            )
        } else {
            let detail = if ports.is_empty() {
                c.image.clone()
            } else {
                format!("{} · {ports}", c.image)
            };
            let detail_tip: SharedString = if c.ports.is_empty() {
                c.image.clone().into()
            } else {
                format!("{}\n{}", c.image, c.ports).into()
            };
            row.child(
                h_flex()
                    .relative()
                    .gap_1p5()
                    .child(dot(dot_color))
                    .child(name)
                    .children(health)
                    .child(div().flex_none().max_w(relative(0.5)).child(status))
                    .children(actions),
            )
            .child(
                h_flex()
                    .pl_3()
                    .gap_1p5()
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "tools-container-detail-{}",
                                c.id
                            )))
                            .test_support()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(muted.opacity(0.8))
                            .child(detail)
                            .tooltip(shared::row_tooltip(detail_tip)),
                    )
                    .children(stats.map(|s| self.render_usage(&c.id, s, cx))),
            )
        };
        row.context_menu(menu).into_any_element()
    }

    /// 列表够宽时容器表格的表头（与 [`Self::render_container`] 一行的各列对齐）。
    fn render_table_header(&self, cx: &App) -> AnyElement {
        shared::list_header(cx)
            .id("tools-container-table-header")
            .test_support()
            .pl_3()
            .pr_1()
            .gap_2()
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1p5()
                    .child(div().flex_none().size_1p5())
                    .child(div().truncate().child(t!("tools.docker.col_name"))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(t!("tools.docker.col_image")),
            )
            .child(
                div()
                    .flex_none()
                    .w(rems(PORTS_WIDTH))
                    .truncate()
                    .child(t!("tools.docker.col_ports")),
            )
            .child(
                div()
                    .flex_none()
                    .w(rems(CPU_WIDTH))
                    .text_right()
                    .child("CPU"),
            )
            .child(
                div()
                    .flex_none()
                    .w(rems(MEM_WIDTH))
                    .text_right()
                    .truncate()
                    .child(t!("tools.docker.col_memory")),
            )
            .child(
                div()
                    .flex_none()
                    .w(rems(STATUS_WIDTH))
                    .truncate()
                    .child(t!("tools.docker.col_status")),
            )
            .into_any_element()
    }

    /// 行上的资源占用：CPU 与内存（按与状态条相同的阈值着色），悬停看明细。
    fn render_usage(&self, id: &str, s: &ContainerStats, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let color = |value: Option<f64>| {
            value
                .and_then(|v| level_color(Level::of(v, PERCENT), cx))
                .unwrap_or(muted)
        };
        let tip: SharedString = usage_parts(s)
            .join(
                "
",
            )
            .into();
        h_flex()
            .id(SharedString::from(format!("tools-container-usage-{id}")))
            .test_support()
            .flex_none()
            .gap_1()
            .child(
                Icon::new(IconName::Cpu)
                    .xsmall()
                    .text_color(muted.opacity(0.7)),
            )
            .child(div().text_color(color(s.cpu)).child(cpu_text(s.cpu)))
            .child(
                Icon::new(IconName::MemoryStick)
                    .xsmall()
                    .text_color(muted.opacity(0.7)),
            )
            .child(
                div()
                    .text_color(color(s.mem_percent))
                    .child(s.mem_used.map(format_size).unwrap_or_else(|| "-".into())),
            )
            .tooltip(shared::row_tooltip(tip))
            .into_any_element()
    }

    fn render_containers(
        &mut self,
        info: &Rc<DockerInfo>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rem = window.rem_size();
        let wide = self.wide(window);
        let rows = self.shown_container_rows(info, cx);
        if rows.is_empty() {
            return shared::empty_text(
                if self.tab == DockerTab::Problems {
                    t!("tools.docker.no_problems")
                } else {
                    t!("tools.docker.no_containers")
                },
                cx,
            );
        }
        let group_height = rems(GROUP_HEIGHT).to_pixels(rem);
        let row_height = rems(if wide { WIDE_ROW_HEIGHT } else { ROW_HEIGHT }).to_pixels(rem);
        let sizes = Rc::new(
            rows.iter()
                .map(|r| match r {
                    ContainerRow::Group { .. } => size(px(0.), group_height),
                    ContainerRow::Container(_) => size(px(0.), row_height),
                })
                .collect::<Vec<_>>(),
        );
        let info = info.clone();
        let stats = self.stats.clone();
        v_virtual_list(
            cx.entity(),
            "tools-container-rows",
            sizes,
            move |this, range, window, cx| {
                range
                    .filter_map(|ix| rows.get(ix))
                    .map(|row| match row {
                        ContainerRow::Group { .. } => this.render_group(info.engine, row, cx),
                        ContainerRow::Container(ix) => {
                            let c = &info.containers[*ix];
                            let usage = stats.as_ref().and_then(|s| s.get(&c.id));
                            this.render_container(&info, c, usage, wide, window, cx)
                        }
                    })
                    .collect::<Vec<_>>()
            },
        )
        .track_scroll(&self.scroll)
        .size_full()
        .into_any_element()
    }

    /// 选中的容器的详情（底部，按内容的高度，最高 45%，超出时滚动）。
    fn render_detail(
        &self,
        info: &Rc<DockerInfo>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.tab.lists_containers() {
            return None;
        }
        let id = self.selected.as_deref()?;
        let c = info.containers.iter().find(|c| c.id == id)?.clone();
        let detail = self.detail.as_ref().filter(|d| d.id == id);
        let loaded = detail.and_then(|d| d.info.as_ref()?.as_ref().ok());
        let stats = self.stats.as_ref().and_then(|s| s.get(id));
        let now = termlet_core::time::now_ms();
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let mono = theme.mono_font_family.clone();
        let row = |label: SharedString, value: AnyElement| {
            h_flex()
                .items_start()
                .gap_3()
                .child(div().flex_none().w(rems(5.)).text_color(muted).child(label))
                .child(div().flex_1().min_w_0().child(value))
                .into_any_element()
        };
        let text = |value: String| div().child(value).into_any_element();
        let note = |value: SharedString| {
            div()
                .text_color(muted)
                .child(prose(value))
                .into_any_element()
        };
        let problem = container_problem(&c);
        let mut rows: Vec<AnyElement> = vec![row(
            t!("tools.docker.detail_image"),
            text(
                loaded
                    .map(|d| d.image.clone())
                    .filter(|i| !i.is_empty())
                    .unwrap_or_else(|| c.image.clone()),
            ),
        )];
        rows.push(row(
            t!("tools.docker.detail_state"),
            text(state_text(&c, loaded, now).to_string()),
        ));
        if let Some(s) = stats {
            // CPU、内存各自整段换行（不在“（0.2%）”中间断开）
            rows.push(row(
                t!("tools.docker.detail_usage"),
                h_flex()
                    .flex_wrap()
                    .gap_x_3()
                    .children(
                        usage_parts(s)
                            .into_iter()
                            .map(|part| div().whitespace_nowrap().child(part)),
                    )
                    .into_any_element(),
            ));
        }
        match detail.and_then(|d| d.info.as_ref()) {
            None => rows.push(note(t!("tools.loading"))),
            Some(Err(error)) => rows.push(
                div()
                    .text_color(theme.danger)
                    .child(prose(t!(
                        "tools.docker.detail_failed",
                        detail = failure_detail(error)
                    )))
                    .into_any_element(),
            ),
            Some(Ok(d)) => {
                rows.push(row(
                    t!("tools.docker.detail_restart"),
                    text(restart_text(d).to_string()),
                ));
                if let Some(health) = &d.health {
                    let mut lines = vec![div().child(health_text(health)).into_any_element()];
                    if let Some(last) = &health.last {
                        let time = last
                            .at
                            .map(|at| event_time(at, now, crate::sftp::file_list::date_style()))
                            .unwrap_or_default();
                        lines.push(
                            div()
                                .text_color(muted)
                                .child(t!(
                                    "tools.docker.health_last",
                                    time = time,
                                    code = last.exit_code
                                ))
                                .into_any_element(),
                        );
                        if !last.output.is_empty() {
                            lines.push(
                                div()
                                    .font_family(mono.clone())
                                    .text_color(muted)
                                    .line_clamp(4)
                                    .child(last.output.clone())
                                    .into_any_element(),
                            );
                        }
                    }
                    rows.push(row(
                        t!("tools.docker.detail_health"),
                        v_flex()
                            .id("tools-container-health")
                            .test_support()
                            .gap_0p5()
                            .children(lines)
                            .into_any_element(),
                    ));
                }
                if !d.ports.is_empty() {
                    rows.push(row(
                        t!("tools.docker.detail_ports"),
                        self.render_ports(&c, d, cx),
                    ));
                }
                if !d.mounts.is_empty() {
                    rows.push(row(
                        t!("tools.docker.detail_mounts"),
                        self.render_mounts(d, cx),
                    ));
                }
                if !d.networks.is_empty() {
                    rows.push(row(
                        t!("tools.docker.detail_networks"),
                        v_flex()
                            .children(d.networks.iter().map(|n| {
                                let address = [n.ip.as_str(), n.ipv6.as_str()]
                                    .into_iter()
                                    .filter(|a| !a.is_empty())
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                let line = if address.is_empty() {
                                    visible_name(&n.name)
                                } else {
                                    format!("{}  {address}", visible_name(&n.name))
                                };
                                div().child(line)
                            }))
                            .into_any_element(),
                    ));
                }
            }
        }
        // 头部：名称 ● 状态 · 主要操作 · ⋯（日志、进入容器、查看进程、复制 ID 等，与右键菜单相同）
        let run = self.detail_run(&c.id).or_else(|| c.run.clone());
        let status =
            header_status(&c, run.as_ref()).map(|text| (text, state_tone(&c, run.as_ref())));
        let actions = self.render_detail_buttons(info, &c, cx);
        let more = shared::more_button(
            "tools-container-detail-more",
            self.menu_builder(info, &c, cx),
            window,
            cx,
        );
        let header = shared::detail_header(
            visible_name(&c.name).into(),
            status,
            actions,
            Some(more),
            cx,
        );
        let mut body: Vec<AnyElement> = problem
            .map(|p| {
                div()
                    .id("tools-container-problem")
                    .test_support()
                    .pb_1()
                    .font_medium()
                    .text_color(problem_tone(p).color(cx))
                    .child(prose(problem_label(p)))
                    .into_any_element()
            })
            .into_iter()
            .collect();
        body.extend(rows);
        body.extend(detail.map(|d| self.render_tail(d, cx)));
        Some(shared::detail_pane(
            "tools-container-detail",
            &self.splitter,
            &self.detail_height,
            header,
            body,
            window,
            cx,
        ))
    }

    /// 端口：发布到主机的写成“主机端口 → 容器端口/协议”，TCP 的可以转发到本机（保存的 SSH 主机）；没有发布的写明。
    fn render_ports(
        &self,
        c: &Container,
        d: &ContainerDetail,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let forwardable = self.context.ssh_host.is_some();
        v_flex()
            .id("tools-container-ports")
            .test_support()
            .children(d.ports.iter().map(|p| {
                let Some(text) = published_port(p) else {
                    let port = format!("{}/{}", p.container_port, p.proto);
                    return div()
                        .text_color(cx.theme().muted_foreground)
                        .child(prose(t!("tools.docker.port_internal", port = port)))
                        .into_any_element();
                };
                let preset = forwardable
                    .then(|| container_forward_preset(p, &c.name))
                    .flatten();
                let key = format!(
                    "{}-{}-{}",
                    p.container_port,
                    p.proto,
                    p.host_port.unwrap_or(0)
                );
                // 发布的端口：本机这个端口上的连接（网络页按端口筛选）
                let connections = p.host_port.map(|port| {
                    let context = self.context.clone();
                    Button::new(SharedString::from(format!("tools-container-conns-{key}")))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Network)
                        .tooltip(t!("tools.docker.port_connections", port = port))
                        .on_click(move |_, window, cx| {
                            shared::go(&context, window, cx, move |panel, window, cx| {
                                panel.show_connections(ConnChip::Port(port), window, cx)
                            });
                        })
                });
                // 在浏览器中打开：发布的 TCP 端口都有（容器发布的多是网页），按端口猜 http 或 https
                let browser = preset.clone().map(|preset| {
                    let context = self.context.clone();
                    let preset = BrowserPreset::new(preset);
                    Button::new(SharedString::from(format!("tools-container-browser-{key}")))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Globe)
                        .tooltip(t!(
                            "tools.network.open_browser_hint",
                            port = preset.forward.port
                        ))
                        .on_click(move |_, _, cx| {
                            let preset = preset.clone();
                            let _ = context.panel.update(cx, |panel, cx| {
                                panel.open_in_browser(&context, preset, cx)
                            });
                        })
                });
                h_flex()
                    .gap_1()
                    .child(div().child(text))
                    .children(preset.map(|preset| {
                        let context = self.context.clone();
                        Button::new(SharedString::from(format!("tools-container-forward-{key}")))
                            .ghost()
                            .xsmall()
                            .icon(IconName::ArrowRightLeft)
                            .tooltip(t!("tools.network.forward_local"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let (context, preset) = (context.clone(), preset.clone());
                                let _ = this
                                    .context
                                    .panel
                                    .update(cx, |panel, cx| panel.forward(&context, preset, cx));
                            }))
                    }))
                    .children(browser)
                    .children(connections)
                    .into_any_element()
            }))
            .into_any_element()
    }

    /// 挂载：主机上的路径 → 容器中的路径；绑定挂载可以在 SFTP 中打开，卷写明名称。
    fn render_mounts(&self, d: &ContainerDetail, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .id("tools-container-mounts")
            .test_support()
            .gap_0p5()
            .children(d.mounts.iter().enumerate().map(|(ix, m)| {
                let line = mount_text(m);
                let sftp = (m.kind == "bind" && m.source.starts_with('/')).then(|| {
                    let path = m.source.clone();
                    let context = self.context.clone();
                    Button::new(SharedString::from(format!("tools-container-sftp-{ix}")))
                        .ghost()
                        .xsmall()
                        .icon(IconName::FolderOpen)
                        .tooltip(t!("tools.docker.open_in_sftp"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let (context, path) = (context.clone(), path.clone());
                            let _ = this
                                .context
                                .panel
                                .update(cx, |panel, cx| panel.open_sftp(&context, path, cx));
                        }))
                });
                h_flex()
                    .items_start()
                    .gap_1()
                    .child(div().min_w_0().child(line))
                    .when(m.read_only, |this| {
                        this.child(badge(t!("tools.docker.mount_read_only"), cx))
                    })
                    .children(sftp)
                    .into_any_element()
            }))
            .into_any_element()
    }

    /// 详情头部的主要操作：运行中的停止、重启，已停止的启动、删除（停止、删除用警示样式）。其余在 ⋯ 中。
    fn render_detail_buttons(
        &self,
        info: &Rc<DockerInfo>,
        c: &Container,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let engine = info.engine;
        let ops: &[ContainerOp] = if is_container_running(c) {
            &[ContainerOp::Stop, ContainerOp::Restart]
        } else {
            &[ContainerOp::Start, ContainerOp::Remove]
        };
        ops.iter()
            .map(|&op| {
                let one = vec![(c.id.clone(), c.name.clone())];
                op_button(
                    SharedString::from(format!("tools-container-detail-{}", container_op_key(op))),
                    container_op_icon(op),
                    container_op_label(op),
                    matches!(op, ContainerOp::Stop | ContainerOp::Remove),
                    false,
                )
                .disabled(self.ops.busy(&c.id))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.container_action(engine, one.clone(), op, None, window, cx)
                }))
                .into_any_element()
            })
            .collect()
    }

    /// 最近几行日志（读取中、读不到时说明）。
    fn render_tail(&self, detail: &Detail, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let note = |text: SharedString| div().text_color(muted).child(prose(text));
        let body = match &detail.logs {
            None => note(t!("tools.loading")).into_any_element(),
            Some(Err(error)) => note(errors::describe(error)).into_any_element(),
            Some(Ok(logs)) if logs.lines.is_empty() => {
                note(t!("tools.logs.empty")).into_any_element()
            }
            Some(Ok(logs)) => shared::log_lines(&logs.lines, cx),
        };
        v_flex()
            .id("tools-container-tail")
            .test_support()
            .pt_1()
            .gap_0p5()
            .child(
                div()
                    .text_color(muted)
                    .child(t!("tools.docker.recent_logs", n = TAIL_LINES)),
            )
            .child(
                div()
                    .px_1p5()
                    .py_1()
                    .rounded(theme.radius)
                    .bg(theme.muted.opacity(0.5))
                    .child(body),
            )
            .into_any_element()
    }

    /// 镜像、卷、网络的右键菜单（鼠标右键与键盘打开的、行右端的 ⋯ 是同一个）：复制名称、复制 ID（镜像、网络），删除
    /// （警示样式；在用、内置的、操作进行中的不能点）。打开期间暂停刷新。
    fn item_menu_builder(
        &self,
        engine: Engine,
        item: &ItemRow,
        cx: &mut Context<Self>,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
        let page = cx.entity().downgrade();
        let item = item.clone();
        let busy = self.ops.busy(&item.key);
        move |menu, window, cx| {
            let menu_id = cx.entity_id();
            let key = item.key.clone();
            let _ = page.update(cx, |page, cx| page.set_menu_open(menu_id, key, cx));
            let page_close = page.clone();
            shared::on_menu_dismiss(window, cx, move |cx| {
                let _ = page_close.update(cx, |page, cx| page.menu_closed(menu_id, cx));
            });
            let name = visible_name(&item.name);
            let mut menu = menu.item(
                PopupMenuItem::new(t!("tools.docker.copy_name"))
                    .icon(IconName::Copy)
                    .on_click(move |_, window, cx| {
                        shared::copy_text(name.clone(), name.clone(), window, cx)
                    }),
            );
            if item.kind != ItemKind::Volume {
                let id = item.id.clone();
                menu = menu.item(
                    PopupMenuItem::new(t!("tools.docker.copy_id"))
                        .icon(IconName::Copy)
                        .on_click(move |_, window, cx| {
                            shared::copy_text(id.clone(), short_id(&id).to_string(), window, cx)
                        }),
                );
            }
            let (remove_page, target) = (page.clone(), item.clone());
            menu.separator().item(
                action_item(
                    t!("tools.docker.op_remove"),
                    IconName::Trash,
                    true,
                    false,
                    cx,
                )
                .disabled(!item.removable || busy)
                .on_click(move |_, window, cx| {
                    let _ = remove_page
                        .update(cx, |page, cx| page.remove_item(engine, &target, window, cx));
                }),
            )
        }
    }

    fn render_item(
        &self,
        engine: Engine,
        item: &ItemRow,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cursor = self.item_cursor.as_deref() == Some(item.key.as_str());
        let ring = cursor && (self.focus.is_focused(window) || self.key_menu.is_open());
        let menu_layer = cursor.then(|| self.key_menu.layer(window)).flatten();
        let menu_row = self.menu_row.as_deref() == Some(item.key.as_str());
        let hovered = self.hover_row.as_deref() == Some(item.key.as_str());
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let row_bg = if cursor {
            theme.list_active
        } else {
            theme.list_hover
        };
        let key = item.key.clone();
        // 行右端：鼠标停在行上、键盘焦点在这一行、菜单打开着时浮出删除与 ⋯（在用的不能删除），对着整行居中，“在用”的
        // 说明让出来；删除进行中、刚完成时在“在用”的位置说明
        let op = self.ops.get(&key);
        let actions = match op {
            Some(_) => None,
            None if hovered || menu_row || ring => {
                let target = item.clone();
                let remove = shared::row_action_button(
                    SharedString::from(format!("tools-remove-{key}")),
                    IconName::Trash,
                    if item.removable {
                        t!("tools.docker.remove")
                    } else {
                        t!("tools.docker.in_use_cannot_remove")
                    },
                    true,
                    !item.removable,
                    window,
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.remove_item(engine, &target, window, cx)
                }))
                .into_any_element();
                let more = shared::more_button(
                    SharedString::from(format!("tools-item-more-{key}")),
                    self.item_menu_builder(engine, item, cx),
                    window,
                    cx,
                );
                Some(vec![remove, more])
            }
            None => None,
        }
        .map(|buttons| {
            shared::row_actions(
                SharedString::from(format!("tools-item-actions-{key}")),
                row_bg,
                buttons,
                cx,
            )
        });
        let theme = cx.theme();
        let covered = actions.is_some();
        let usage = match op {
            Some(op) => {
                shared::op_status(SharedString::from(format!("tools-item-op-{key}")), op, cx)
            }
            None => div()
                .flex_none()
                // 在用、内置是附带信息（灰）；未使用的可以删除，用正文的颜色
                .text_color(match (covered, item.in_use) {
                    (true, _) => theme.transparent,
                    (false, true) => muted,
                    (false, false) => theme.foreground,
                })
                .child(item.usage.clone())
                .into_any_element(),
        };
        let cursor_key = key.clone();
        let hover_key = key.clone();
        let row = v_flex()
            .id(SharedString::from(format!("tools-item-{key}")))
            .test_support()
            .size_full()
            .justify_center()
            .px_1()
            .text_xs()
            .cursor_default()
            .when(cursor, |el| el.bg(theme.list_active))
            .when(!cursor && menu_row, |el| el.bg(theme.list_hover))
            .when(!cursor, |el| el.hover(|s| s.bg(theme.list_hover)));
        shared::ring(row, ring, cx)
            .children(menu_layer)
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                this.set_hover_row(hover_key.clone(), *hovered, cx)
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.focus.focus(window, cx);
                this.item_cursor = Some(cursor_key.clone());
                cx.notify();
            }))
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(visible_name(&item.name)),
                    )
                    .child(usage),
            )
            .child(
                div()
                    .id(SharedString::from(format!("tools-item-detail-{key}")))
                    .test_support()
                    .truncate()
                    .text_color(muted.opacity(0.8))
                    .child(item.detail.clone()),
            )
            .children(actions)
            .context_menu(self.item_menu_builder(engine, item, cx))
            .into_any_element()
    }

    fn render_items(
        &mut self,
        info: &Rc<DockerInfo>,
        rem: Pixels,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (items, empty) = self.shown_item_rows(info, cx);
        if items.is_empty() {
            return shared::empty_text(empty, cx);
        }
        let row_height = rems(ROW_HEIGHT).to_pixels(rem);
        let sizes = Rc::new(vec![size(px(0.), row_height); items.len()]);
        let engine = info.engine;
        v_virtual_list(
            cx.entity(),
            "tools-item-rows",
            sizes,
            move |this, range, window, cx| {
                range
                    .filter_map(|ix| items.get(ix))
                    .map(|item| this.render_item(engine, item, window, cx))
                    .collect::<Vec<_>>()
            },
        )
        .track_scroll(&self.scroll)
        .size_full()
        .into_any_element()
    }

    /// 打开清理的对话框（工具条上的“清理…”）：打开期间暂停刷新，关闭（对话框被释放）后恢复。
    fn open_cleanup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.cleanup.as_ref().is_some_and(|c| c.upgrade().is_some()) {
            return;
        }
        let view = docker_cleanup::open(self.context.clone(), cx.entity().downgrade(), window, cx);
        self._cleanup = Some(cx.observe_release(&view, |page, _, cx| page.cleanup_closed(cx)));
        self.cleanup = Some(view.downgrade());
        self.sync_poll(cx);
        cx.notify();
    }

    /// 清理的对话框关闭了：恢复刷新（显示暂停期间读到的结果，包括清理之后的重读）。
    fn cleanup_closed(&mut self, cx: &mut Context<Self>) {
        self.cleanup = None;
        self._cleanup = None;
        self.sync_poll(cx);
        cx.notify();
    }

    /// 工具条上的“清理…”（各子页都有）。
    fn render_cleanup_button(&self, cx: &mut Context<Self>) -> AnyElement {
        Button::new("tools-docker-cleanup")
            .ghost()
            .small()
            .icon(IconName::BrushCleaning)
            .tooltip(t!("tools.cleanup.open"))
            .on_click(cx.listener(|this, _, window, cx| this.open_cleanup(window, cx)))
            .into_any_element()
    }

    /// 容器子页工具条上的排序（按项目、CPU、内存）。
    fn render_sort(&self, cx: &mut Context<Self>) -> AnyElement {
        let sort = self.sort;
        let page = cx.entity().downgrade();
        Button::new("tools-docker-sort")
            .ghost()
            .small()
            .icon(IconName::ArrowDownUp)
            .selected(sort != ContainerSort::Project)
            .tooltip(t!("tools.docker.sort"))
            .dropdown_menu(move |menu, _, _| {
                ContainerSort::ALL.into_iter().fold(menu, |menu, choice| {
                    let page = page.clone();
                    menu.item(
                        PopupMenuItem::new(sort_label(choice))
                            .checked(choice == sort)
                            .on_click(move |_, _, cx| {
                                let _ = page.update(cx, |page, cx| page.set_sort(choice, cx));
                            }),
                    )
                })
            })
            .into_any_element()
    }
}

#[cfg(test)]
impl DockerPage {
    pub(super) fn selected_id(&self) -> Option<String> {
        self.selected.clone()
    }

    pub(super) fn note(&self) -> Option<SharedString> {
        self.note.clone()
    }

    /// 打开着的清理对话框。
    pub(super) fn cleanup_dialog(&self) -> Option<Entity<CleanupDialog>> {
        self.cleanup.as_ref().and_then(WeakEntity::upgrade)
    }

    /// 记着打开着的清理对话框（关闭、释放后清除）。
    pub(super) fn cleanup_open(&self) -> bool {
        self.cleanup.is_some()
    }

    /// 测试中直接放进资源占用（不连服务器）。
    pub(super) fn set_test_stats(&mut self, list: Vec<ContainerStats>, cx: &mut Context<Self>) {
        let info = self.feed.data();
        let map: StatsMap = info
            .iter()
            .flat_map(|info| info.containers.iter())
            .filter_map(|c| {
                list.iter()
                    .find(|s| s.is_for(&c.id))
                    .map(|s| (c.id.clone(), s.clone()))
            })
            .collect();
        self.stats_of = info;
        self.stats = Some(Rc::new(map));
        self.stats_gen += 1;
        cx.notify();
    }

    /// 测试中直接放进选中的容器的详情与最近几行日志。
    pub(super) fn set_test_detail(
        &mut self,
        detail: ContainerDetail,
        logs: LogResult,
        cx: &mut Context<Self>,
    ) {
        self._detail = None;
        self._detail_logs = None;
        let run = (self.feed.data().iter())
            .flat_map(|info| info.containers.iter())
            .find(|c| c.id == detail.id)
            .and_then(|c| c.run.clone());
        self.detail = Some(Detail {
            id: detail.id.clone(),
            run,
            info: Some(Ok(detail)),
            logs: Some(Ok(logs)),
        });
        cx.notify();
    }
}

/// 定时刷新读到列表后，选中的容器的详情还能沿用：上次读到了（或者正在读），列表中它的运行状态（状态、启动与结束
/// 时间、退出码、健康）没变。
fn detail_is_current(detail: &Detail, info: &DockerInfo) -> bool {
    !matches!(detail.info, Some(Err(_)))
        && info
            .containers
            .iter()
            .any(|c| c.id == detail.id && c.run == detail.run)
}

/// 容器页（或异常子页）的行：按 compose 项目分组，单独的容器在后；按 CPU、内存排序时组内与各组按读数从高到低。
/// 组的标题按项目中的全部容器（整体启停涉及的），不受搜索、筛选影响。
fn container_rows(
    info: &DockerInfo,
    query: &str,
    tab: DockerTab,
    sort: ContainerSort,
    stats: Option<&StatsMap>,
) -> Vec<ContainerRow> {
    let usage = |c: &Container| -> Option<f64> {
        let s = stats?.get(&c.id)?;
        match sort {
            ContainerSort::Project => None,
            ContainerSort::Cpu => s.cpu,
            ContainerSort::Memory => s.mem_used.map(|b| b as f64),
        }
    };
    let key: Option<&dyn Fn(&Container) -> Option<f64>> = match sort {
        ContainerSort::Project => None,
        _ => Some(&usage),
    };
    let mut rows: Vec<ContainerRow> = Vec::new();
    for group in container_groups(&info.containers, query, tab == DockerTab::Problems, key) {
        let members: Vec<&Container> = info
            .containers
            .iter()
            .filter(|c| c.project.as_deref() == group.project)
            .collect();
        rows.push(ContainerRow::Group {
            project: group.project.map(str::to_string),
            working_dir: group.working_dir.map(str::to_string),
            running: members.iter().filter(|c| is_container_running(c)).count(),
            containers: members
                .iter()
                .map(|c| (c.id.clone(), c.name.clone()))
                .collect(),
        });
        for c in &group.containers {
            if let Some(ix) = info.containers.iter().position(|x| x.id == c.id) {
                rows.push(ContainerRow::Container(ix));
            }
        }
    }
    rows
}

/// 镜像、卷、网络子页的行与没有内容时的说明。
fn item_rows(info: &DockerInfo, query: &str, tab: DockerTab) -> (Vec<ItemRow>, SharedString) {
    let usage = |in_use: bool, containers: usize| {
        if !in_use {
            t!("tools.docker.unused")
        } else if containers == 1 {
            t!("tools.docker.in_use_by_one")
        } else if containers > 1 {
            t!("tools.docker.in_use_by", count = containers)
        } else {
            t!("tools.docker.in_use")
        }
    };
    let join = |parts: &[&str]| {
        parts
            .iter()
            .filter(|p| !p.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" · ")
    };
    // 镜像的创建时间按本机时区写（换算不了时照写原文）
    let now = termlet_core::time::now_ms();
    let created = |im: &Image| match im.created_at {
        Some(at) => {
            termlet_core::time::format_compact(at, now, crate::sftp::file_list::date_style())
        }
        None => im.created.clone(),
    };
    match tab {
        DockerTab::Images => (
            info.images
                .iter()
                .filter(|im| matches_any(query, &[&im.repository, &im.tag, &im.id]))
                .map(|im| ItemRow {
                    kind: ItemKind::Image,
                    id: im.id.clone(),
                    key: format!("image-{}-{}:{}", im.id, im.repository, im.tag),
                    name: image_name(im),
                    detail: join(&[&im.size, &created(im)]),
                    usage: usage(im.in_use, im.containers),
                    in_use: im.in_use,
                    removable: image_removable(im),
                })
                .collect(),
            t!("tools.docker.no_images"),
        ),
        DockerTab::Volumes => (
            info.volumes
                .iter()
                .filter(|v| matches_any(query, &[&v.name, &v.driver]))
                .map(|v| ItemRow {
                    kind: ItemKind::Volume,
                    id: v.name.clone(),
                    key: format!("volume-{}", v.name),
                    name: v.name.clone(),
                    detail: join(&[&v.driver, &v.mountpoint]),
                    usage: usage(v.in_use, v.containers),
                    in_use: v.in_use,
                    removable: volume_removable(v),
                })
                .collect(),
            t!("tools.docker.no_volumes"),
        ),
        DockerTab::Networks | DockerTab::Containers | DockerTab::Problems => (
            info.networks
                .iter()
                .filter(|n| matches_any(query, &[&n.name, &n.driver, &n.id]))
                .map(|n| ItemRow {
                    kind: ItemKind::Network,
                    id: n.id.clone(),
                    key: format!("network-{}", n.id),
                    name: n.name.clone(),
                    detail: join(&[&n.driver, &n.scope]),
                    usage: if n.builtin {
                        t!("tools.docker.builtin")
                    } else {
                        usage(n.in_use, n.containers)
                    },
                    in_use: n.in_use || n.builtin,
                    removable: network_removable(n),
                })
                .collect(),
            t!("tools.docker.no_networks"),
        ),
    }
}

impl Render for DockerPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(info) = self.feed.data() else {
            let body = match &self.feed.error {
                Some(error) => {
                    let this = cx.entity().downgrade();
                    notice(
                        error,
                        true,
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
        let (muted, danger) = {
            let theme = cx.theme();
            (theme.muted_foreground, theme.danger)
        };
        let problems = self.problems.get(&info, (), |info, _| {
            info.containers
                .iter()
                .filter(|c| container_problem(c).is_some())
                .count()
        });
        let counts = [
            info.containers.len(),
            problems,
            info.images.len(),
            info.volumes.len(),
            info.networks.len(),
        ];
        let tab_ix = DockerTab::ALL
            .iter()
            .position(|t| *t == self.tab)
            .unwrap_or(0);
        // 引擎与版本 · Compose 的版本 · 运行中的个数（暂停刷新时写明；面板窄、截短时悬停看全文）；经免密码 sudo 访问时
        // 行尾一个“sudo”
        let engine = engine_line(&info);
        let info_text = h_flex()
            .id("tools-docker-info")
            .test_support()
            .gap_2()
            .child(engine.clone())
            .tooltip(shared::row_tooltip(engine))
            .when(self.is_paused(), |this| {
                this.child(
                    div()
                        .text_color(muted.opacity(0.7))
                        .child(t!("tools.paused")),
                )
            });
        let marker = info
            .sudo
            .then(|| shared::sudo_badge("tools-sudo-docker".into(), t!("tools.docker.sudo"), cx));
        let rem = window.rem_size();
        let lists_containers = self.tab.lists_containers();
        let body = if lists_containers {
            self.render_containers(&info, window, cx)
        } else {
            self.render_items(&info, rem, cx)
        };
        let table_header =
            (lists_containers && self.wide(window)).then(|| self.render_table_header(cx));
        let measured = self.width.clone();
        let entity = cx.entity().downgrade();
        let threshold = rems(WIDE_FROM).to_pixels(rem);
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
        let detail = self.render_detail(&info, window, cx);
        let stats_error = self
            .stats_error
            .as_ref()
            .filter(|_| lists_containers)
            .map(|error| {
                hint_line(
                    t!("tools.docker.stats_failed", detail = failure_detail(error)),
                    muted.opacity(0.8),
                )
            });
        let sort = lists_containers.then(|| self.render_sort(cx));
        let tools = h_flex()
            .gap_1()
            .children(sort)
            .child(self.render_cleanup_button(cx))
            .into_any_element();
        let loading = self.feed.loading;
        let this = cx.entity().downgrade();
        let has_header = table_header.is_some();
        shared::list_page()
            .on_drag_move(shared::on_detail_drag)
            .child(shared::toolbar_with(
                "tools-docker",
                &self.search,
                Some(tools),
                loading,
                move |_, _, cx| {
                    let _ = this.update(cx, |page, cx| page.reload(cx));
                },
            ))
            .child(shared::filter_tabs(
                "tools-docker-tabs",
                &self.tabs_scroll,
                tab_ix,
                DockerTab::ALL
                    .into_iter()
                    .zip(counts)
                    .map(|(tab, count)| {
                        // 有异常时数量用危险色
                        let color = if tab == DockerTab::Problems && count > 0 {
                            danger
                        } else {
                            muted
                        };
                        Tab::new()
                            .label(tab_label(tab))
                            .suffix(div().text_color(color).child(count.to_string()))
                    })
                    .collect(),
                cx.listener(|this, ix: &usize, _, cx| {
                    if let Some(tab) = DockerTab::ALL.get(*ix) {
                        this.set_tab(*tab, cx);
                    }
                }),
            ))
            .child(info_line(info_text, marker, self.feed.shown.at, cx))
            .children(error_line(self.feed.error.as_ref(), cx))
            .children(shared::failure_banner(
                &self.ops,
                |page: &mut Self| &mut page.ops,
                cx,
            ))
            .children(stats_error)
            .children(note)
            .child(
                shared::list_card(div(), has_header, cx)
                    .id("tools-docker-list")
                    .test_support()
                    .key_context(LIST_CONTEXT)
                    .track_focus(&self.focus)
                    .on_hover(
                        cx.listener(|this, hovered: &bool, _, cx| this.set_hovering(*hovered, cx)),
                    )
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
                    .on_action(
                        cx.listener(|this, _: &OpenItem, window, cx| this.open_item(window, cx)),
                    )
                    .on_action(cx.listener(|this, _: &OpenLogs, _, cx| this.logs_item(cx)))
                    .on_action(
                        cx.listener(|this, _: &DeleteItem, window, cx| {
                            this.delete_item(window, cx)
                        }),
                    )
                    .on_action(
                        cx.listener(|this, _: &OpenMenu, window, cx| {
                            this.open_key_menu(window, cx)
                        }),
                    )
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(rems(shared::LIST_MIN))
                    .child(
                        // 宽度跨过容器表格的界限时重新渲染
                        canvas(
                            move |bounds, _, cx| {
                                let old = measured.replace(bounds.size.width);
                                if (old >= threshold) != (bounds.size.width >= threshold) {
                                    let _ = entity.update(cx, |_, cx| cx.notify());
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .children(table_header)
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .child(body)
                            .vertical_scrollbar(&self.scroll),
                    ),
            )
            .children(detail)
            .into_any_element()
    }
}

/// 信息行的开头：“Docker 29.3.0 · Compose 2.39.1 · 3 个运行中”（读不到的版本不写）。
fn engine_line(info: &DockerInfo) -> SharedString {
    let engine = match info.engine {
        Engine::Docker => "Docker",
        Engine::Podman => "Podman",
    };
    let mut parts: Vec<SharedString> = vec![match &info.version {
        Some(version) => format!("{engine} {version}").into(),
        None => engine.into(),
    }];
    if let Some(compose) = &info.compose {
        parts.push(format!("Compose {compose}").into());
    }
    let running = info
        .containers
        .iter()
        .filter(|c| c.state == "running")
        .count();
    parts.push(if running == 1 {
        t!("tools.docker.running_one")
    } else {
        t!("tools.docker.running_count", count = running)
    });
    parts.join(" · ").into()
}

/// 行上进行中、刚完成时的说法。
fn container_op_verb(op: ContainerOp) -> OpVerb {
    match op {
        ContainerOp::Start => OpVerb::Start,
        ContainerOp::Stop => OpVerb::Stop,
        ContainerOp::Restart => OpVerb::Restart,
        ContainerOp::Remove => OpVerb::Remove,
    }
}

fn tab_label(tab: DockerTab) -> SharedString {
    match tab {
        DockerTab::Containers => t!("tools.docker.containers"),
        DockerTab::Problems => t!("tools.docker.problems"),
        DockerTab::Images => t!("tools.docker.images"),
        DockerTab::Volumes => t!("tools.docker.volumes"),
        DockerTab::Networks => t!("tools.docker.networks"),
    }
}

fn sort_label(sort: ContainerSort) -> SharedString {
    match sort {
        ContainerSort::Project => t!("tools.docker.sort_project"),
        ContainerSort::Cpu => t!("tools.docker.sort_cpu"),
        ContainerSort::Memory => t!("tools.docker.sort_memory"),
    }
}

/// 异常的颜色：不健康、重启中、已失效用危险色，非 0 退出用警告色。
fn problem_tone(problem: ContainerProblem) -> Tone {
    if problem.is_danger() {
        Tone::Danger
    } else {
        Tone::Warning
    }
}

/// 容器的状态（读到的运行状态，没有时为列表中的 State）。
fn status_of<'a>(c: &'a Container, run: Option<&'a RunState>) -> &'a str {
    run.map(|run| run.status.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(c.state.as_str())
}

/// 状态文字的颜色：运行中绿，非 0 退出黄，重启中、已失效红，其余（正常退出、已创建、已暂停）灰。健康状态另有标签。
fn state_tone(c: &Container, run: Option<&RunState>) -> Tone {
    match status_of(c, run) {
        "running" => Tone::Good,
        "restarting" | "dead" => Tone::Danger,
        "exited" => match run.and_then(|run| run.exit_code).or_else(|| c.exit_code()) {
            Some(code) if code != 0 => Tone::Warning,
            _ => Tone::Muted,
        },
        _ => Tone::Muted,
    }
}

/// 详情中健康检查的状态。
fn health_state(status: &str) -> Option<HealthState> {
    match status {
        "healthy" => Some(HealthState::Healthy),
        "unhealthy" => Some(HealthState::Unhealthy),
        "starting" => Some(HealthState::Starting),
        _ => None,
    }
}

/// 健康状态的小标签：不健康（危险色）、启动中（警告色）；健康的不标。
fn health_badge(health: HealthState, cx: &App) -> Option<gpui_kit::Div> {
    match health {
        HealthState::Unhealthy => Some(shared::status_badge(
            t!("tools.docker.badge_unhealthy"),
            Tone::Danger,
            cx,
        )),
        HealthState::Starting => Some(shared::status_badge(
            t!("tools.docker.badge_starting"),
            Tone::Warning,
            cx,
        )),
        HealthState::Healthy => None,
    }
}

/// 一段时长的简短写法（行上的“运行中 · 15 分钟”）：取最大的单位。
pub(super) fn short_duration(ms: i64) -> SharedString {
    let seconds = (ms / 1000).max(0);
    if seconds < 60 {
        t!("tools.docker.for_under_minute")
    } else if seconds < 3600 {
        t!("tools.docker.for_minutes", n = seconds / 60)
    } else if seconds < 86_400 {
        t!("tools.docker.for_hours", n = seconds / 3600)
    } else {
        t!("tools.docker.for_days", n = seconds / 86_400)
    }
}

/// 多久之前（行上的“已退出（1）· 2 小时前”）：取最大的单位。
pub(super) fn ago(ms: i64) -> SharedString {
    let seconds = (ms / 1000).max(0);
    if seconds < 60 {
        t!("tools.docker.ago_now")
    } else if seconds < 3600 {
        t!("tools.docker.ago_minutes", n = seconds / 60)
    } else if seconds < 86_400 {
        t!("tools.docker.ago_hours", n = seconds / 3600)
    } else {
        t!("tools.docker.ago_days", n = seconds / 86_400)
    }
}

/// 行上的状态（与详情的写法一致，按运行状态现算）：运行中写已运行多久（“运行中 · 15 分钟”），已退出写退出码与多久
/// 之前（“已退出（1）· 2 小时前”），其余按状态的说明；读不到运行状态时退回 Docker 的原文。
fn row_status(c: &Container, run: Option<&RunState>, now: i64) -> SharedString {
    let Some(run) = run else {
        return c.status.clone().into();
    };
    match status_of(c, Some(run)) {
        "running" => match run.started_at {
            Some(at) => t!(
                "tools.docker.state_running_for",
                duration = short_duration(now - at)
            ),
            None => t!("tools.docker.row_running"),
        },
        "exited" => match (run.exit_code.or_else(|| c.exit_code()), run.finished_at) {
            (Some(code), Some(at)) => t!(
                "tools.docker.row_exited_ago",
                code = code,
                ago = ago(now - at)
            ),
            (Some(code), None) => t!("tools.docker.row_exited", code = code),
            (None, _) => t!("tools.docker.row_exited_plain"),
        },
        status => header_status(c, Some(run)).unwrap_or_else(|| status.to_string().into()),
    }
}

/// 详情头部的状态（不写时长，时长在下面的“状态”中）：运行中、已退出（1）、已暂停等；认不出时为 `None`。
fn header_status(c: &Container, run: Option<&RunState>) -> Option<SharedString> {
    Some(match status_of(c, run) {
        "running" => t!("tools.docker.row_running"),
        "exited" => match run.and_then(|run| run.exit_code).or_else(|| c.exit_code()) {
            Some(code) => t!("tools.docker.row_exited", code = code),
            None => t!("tools.docker.row_exited_plain"),
        },
        "paused" => t!("tools.docker.state_paused"),
        "restarting" => t!("tools.docker.state_restarting"),
        "created" => t!("tools.docker.row_created"),
        "dead" => t!("tools.docker.state_dead"),
        _ => return None,
    })
}

fn problem_label(problem: ContainerProblem) -> SharedString {
    match problem {
        ContainerProblem::Exited(code) => t!("tools.docker.problem_exited", code = code),
        ContainerProblem::Unhealthy => t!("tools.docker.problem_unhealthy"),
        ContainerProblem::Restarting => t!("tools.docker.problem_restarting"),
        ContainerProblem::Dead => t!("tools.docker.problem_dead"),
    }
}

/// CPU 使用率（一位小数），没有读数时为“-”。
fn cpu_text(cpu: Option<f64>) -> String {
    cpu.map(|c| format!("{c:.1}%"))
        .unwrap_or_else(|| "-".into())
}

/// 资源占用的明细：CPU（按单核计）与内存（用量 / 上限，占上限的百分比）。
fn usage_parts(s: &ContainerStats) -> [SharedString; 2] {
    let memory = match (s.mem_used, s.mem_limit) {
        (Some(used), Some(limit)) => match s.mem_percent {
            Some(percent) => t!(
                "tools.docker.memory_of_limit",
                used = format_size(used),
                limit = format_size(limit),
                percent = format!("{percent:.1}")
            )
            .to_string(),
            None => format!("{} / {}", format_size(used), format_size(limit)),
        },
        (Some(used), None) => format_size(used),
        _ => "-".into(),
    };
    [
        t!("tools.docker.usage_cpu", cpu = cpu_text(s.cpu)),
        t!("tools.docker.usage_memory", memory = memory),
    ]
}

/// 读不到资源占用、详情的原因：Docker 给出的错误原样写（不再套“读取容器失败：”，文案自己说了读不到什么），
/// 其余按错误说明。
fn failure_detail(error: &ToolsError) -> SharedString {
    match error {
        ToolsError::DockerFailed(detail) => detail.clone().into(),
        other => errors::describe(other),
    }
}

/// 状态：运行中写已运行多久；已退出写退出码与时间（因内存不足被结束时写明）；其余按状态的说明。详情还没读到时用列表
/// 读取时的运行状态（不用 Docker 那一刻的原文，与行上的时长一致）。
fn state_text(c: &Container, d: Option<&ContainerDetail>, now: i64) -> SharedString {
    let run = c.run.as_ref();
    let status = d.map_or(status_of(c, run), |d| d.status.as_str());
    match status {
        // 运行时长的文案自带“已运行”（与服务详情相同）
        "running" => match d
            .and_then(|d| d.started_at)
            .or_else(|| run.and_then(|r| r.started_at))
        {
            Some(at) => t!(
                "tools.docker.state_running_for",
                duration = format_uptime((now - at) as f64 / 1000.)
            ),
            None => c.status.clone().into(),
        },
        "exited" => {
            let code = d.and_then(|d| d.exit_code).or_else(|| c.exit_code());
            let time = d
                .and_then(|d| d.finished_at)
                .or_else(|| run.and_then(|r| r.finished_at))
                .map(|at| event_time(at, now, crate::sftp::file_list::date_style()));
            let text = match (code, time) {
                (Some(code), Some(time)) => {
                    t!("tools.docker.state_exited_at", code = code, time = time)
                }
                (Some(code), None) => t!("tools.docker.state_exited", code = code),
                _ => c.status.clone().into(),
            };
            if d.map_or(run.is_some_and(|r| r.oom_killed), |d| d.oom_killed) {
                format!("{text} · {}", t!("tools.docker.oom_killed")).into()
            } else {
                text
            }
        }
        "paused" => t!("tools.docker.state_paused"),
        "restarting" => t!("tools.docker.state_restarting"),
        "created" => t!("tools.docker.state_created"),
        "dead" => t!("tools.docker.state_dead"),
        _ => c.status.clone().into(),
    }
}

/// 重启策略与自动重启的次数。
fn restart_text(d: &ContainerDetail) -> SharedString {
    let policy = match d.restart_policy.as_str() {
        "always" => t!("tools.docker.restart_always"),
        "unless-stopped" => t!("tools.docker.restart_unless_stopped"),
        "on-failure" if d.max_retries == 1 => t!("tools.docker.restart_on_failure_once"),
        "on-failure" if d.max_retries > 0 => {
            t!("tools.docker.restart_on_failure_max", n = d.max_retries)
        }
        "on-failure" => t!("tools.docker.restart_on_failure"),
        "no" | "" => t!("tools.docker.restart_no"),
        other => other.to_string().into(),
    };
    match d.restart_count {
        0 => policy,
        1 => format!("{policy} · {}", t!("tools.docker.restart_count_one")).into(),
        n => format!("{policy} · {}", t!("tools.docker.restart_count", n = n)).into(),
    }
}

fn health_text(health: &termlet_core::hosttools::Health) -> SharedString {
    match health.status.as_str() {
        "healthy" => t!("tools.docker.health_healthy"),
        "unhealthy" if health.failing_streak == 1 => t!("tools.docker.health_unhealthy_one"),
        "unhealthy" => t!("tools.docker.health_unhealthy", n = health.failing_streak),
        "starting" => t!("tools.docker.health_starting"),
        other => other.to_string().into(),
    }
}

/// 一个挂载：主机路径（卷写名称）→ 容器中的路径（只读另用小标签标出）。
fn mount_text(m: &MountInfo) -> String {
    let source = match (&m.name, m.kind.as_str()) {
        (Some(name), "volume") => {
            t!("tools.docker.mount_volume", name = visible_name(name)).to_string()
        }
        _ if m.source.is_empty() => m.kind.clone(),
        _ => visible_name(&m.source),
    };
    format!("{source} → {}", visible_name(&m.destination))
}

fn container_op_key(op: ContainerOp) -> &'static str {
    match op {
        ContainerOp::Start => "start",
        ContainerOp::Stop => "stop",
        ContainerOp::Restart => "restart",
        ContainerOp::Remove => "remove",
    }
}

fn container_op_icon(op: ContainerOp) -> IconName {
    match op {
        ContainerOp::Start => IconName::Play,
        ContainerOp::Stop => IconName::Square,
        ContainerOp::Restart => IconName::RotateCcw,
        ContainerOp::Remove => IconName::Trash,
    }
}

fn container_op_label(op: ContainerOp) -> SharedString {
    match op {
        ContainerOp::Start => t!("tools.docker.op_start"),
        ContainerOp::Stop => t!("tools.docker.op_stop"),
        ContainerOp::Restart => t!("tools.docker.op_restart"),
        ContainerOp::Remove => t!("tools.docker.op_remove"),
    }
}

fn container_action_title(op: ContainerOp, name: &str) -> SharedString {
    match op {
        ContainerOp::Start => t!("tools.docker.container_start_action", name = name),
        ContainerOp::Stop => t!("tools.docker.container_stop_action", name = name),
        ContainerOp::Restart => t!("tools.docker.container_restart_action", name = name),
        ContainerOp::Remove => t!("tools.docker.container_remove_action", name = name),
    }
}

/// 项目整体操作的说明（项目没有删除）。
fn project_action(op: ContainerOp, name: &str) -> SharedString {
    match op {
        ContainerOp::Start => t!("tools.docker.project_start_action", name = name),
        ContainerOp::Restart => t!("tools.docker.project_restart_action", name = name),
        ContainerOp::Stop | ContainerOp::Remove => {
            t!("tools.docker.project_stop_action", name = name)
        }
    }
}

fn confirm_container(op: ContainerOp, name: &str) -> SharedString {
    match op {
        ContainerOp::Remove => t!("tools.docker.confirm_container_remove", name = name),
        ContainerOp::Restart => t!("tools.docker.confirm_container_restart", name = name),
        ContainerOp::Stop | ContainerOp::Start => {
            t!("tools.docker.confirm_container_stop", name = name)
        }
    }
}

fn confirm_project(op: ContainerOp, name: &str, count: usize) -> SharedString {
    match (op, count) {
        (ContainerOp::Restart, 1) => t!("tools.docker.confirm_project_restart_one", name = name),
        (ContainerOp::Restart, _) => {
            t!(
                "tools.docker.confirm_project_restart",
                name = name,
                count = count
            )
        }
        (_, 1) => t!("tools.docker.confirm_project_stop_one", name = name),
        _ => t!(
            "tools.docker.confirm_project_stop",
            name = name,
            count = count
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{Language, set_language};

    fn container(state: &str, status: &str) -> Container {
        Container {
            id: format!("{:0>64}", 1),
            name: "web".into(),
            image: "nginx".into(),
            state: state.into(),
            status: status.into(),
            ports: String::new(),
            created: String::new(),
            project: None,
            service: None,
            working_dir: None,
            run: None,
        }
    }

    /// 状态一行：运行时长的文案自带“已运行”，不再重复；已退出写退出码与时间，因内存不足被结束时写明。每种语言。
    #[test]
    fn state_line_says_each_thing_once() {
        let now = 1_793_756_832_000;
        let running = ContainerDetail {
            status: "running".into(),
            started_at: Some(now - 58 * 60 * 1000),
            ..Default::default()
        };
        let exited = ContainerDetail {
            status: "exited".into(),
            exit_code: Some(137),
            finished_at: Some(now - 60_000),
            oom_killed: true,
            ..Default::default()
        };
        let c = container("running", "Up 58 minutes");
        for (language, want) in [
            (Language::ZhCn, "运行中 · 已运行 58 分钟"),
            (Language::En, "Running · Up 58 min"),
            (Language::ZhTw, "執行中 · 已執行 58 分鐘"),
            (Language::Ja, "実行中 · 稼働 58 分"),
            (Language::Ko, "실행 중 · 가동 58분"),
        ] {
            set_language(language);
            assert_eq!(state_text(&c, Some(&running), now), want);
            let time = event_time(now - 60_000, now, crate::sftp::file_list::date_style());
            let want_exited = match language {
                Language::ZhCn => format!("已退出（退出码 137），{time} · 因内存不足被结束"),
                Language::En => {
                    format!("Exited (exit code 137) at {time} · killed for running out of memory")
                }
                Language::ZhTw => format!("已結束（結束代碼 137），{time} · 因記憶體不足被結束"),
                Language::Ja => {
                    format!("終了（終了コード 137）、{time} · メモリ不足により強制終了")
                }
                Language::Ko => format!("종료됨(종료 코드 137), {time} · 메모리 부족으로 종료됨"),
            };
            assert_eq!(
                state_text(&container("exited", "Exited (137)"), Some(&exited), now),
                want_exited
            );
        }
        set_language(Language::ZhCn);
        // 读不到详情：Docker 的原因只说一次
        let failed = t!(
            "tools.docker.detail_failed",
            detail = failure_detail(&ToolsError::DockerFailed("No such container: x".into()))
        );
        assert_eq!(failed, "读不到详情：No such container: x");
    }

    /// 信息行：引擎与版本、Compose 的版本、运行中的个数；读不到版本时只写引擎。三种语言的单复数。
    #[test]
    fn engine_line_names_versions_and_running_count() {
        let info = |version: Option<&str>, compose: Option<&str>, running: usize| DockerInfo {
            engine: Engine::Docker,
            sudo: false,
            version: version.map(Into::into),
            compose: compose.map(Into::into),
            containers: (0..3)
                .map(|i| Container {
                    id: format!("{i:0>64}"),
                    ..container(if i < running { "running" } else { "exited" }, "")
                })
                .collect(),
            images: Vec::new(),
            volumes: Vec::new(),
            networks: Vec::new(),
        };
        set_language(Language::ZhCn);
        assert_eq!(
            engine_line(&info(Some("29.3.0"), Some("2.39.1"), 2)),
            "Docker 29.3.0 · Compose 2.39.1 · 2 个运行中"
        );
        assert_eq!(engine_line(&info(None, None, 0)), "Docker · 0 个运行中");
        set_language(Language::En);
        assert_eq!(
            engine_line(&info(Some("29.3.0"), None, 1)),
            "Docker 29.3.0 · 1 running"
        );
        set_language(Language::ZhCn);
    }

    /// 行上的状态：按列表读取时的运行状态现算的本地化写法（不用 docker ps 那一刻的原文），时长取最大的单位；读不到
    /// 运行状态时退回原文。状态文字的颜色：运行中绿、非 0 退出黄、正常退出灰、重启中红。三种语言。
    #[test]
    fn row_status_is_localized_and_live() {
        let now = 1_793_756_832_000;
        let running = RunState {
            status: "running".into(),
            started_at: Some(now - 15 * 60_000),
            ..Default::default()
        };
        let exited = RunState {
            status: "exited".into(),
            exit_code: Some(1),
            finished_at: Some(now - 2 * 3_600_000 - 60_000),
            ..Default::default()
        };
        let c = container("running", "Up Less than a second");
        for (language, want_running, want_exited) in [
            (Language::ZhCn, "运行中 · 15 分钟", "已退出（1）· 2 小时前"),
            (Language::En, "Running · 15 min", "Exited (1) · 2 h ago"),
            (Language::ZhTw, "執行中 · 15 分鐘", "已結束（1）· 2 小時前"),
        ] {
            set_language(language);
            assert_eq!(row_status(&c, Some(&running), now), want_running);
            assert_eq!(row_status(&c, Some(&exited), now), want_exited);
        }
        set_language(Language::ZhCn);
        assert_eq!(
            row_status(&c, None, now),
            "Up Less than a second",
            "读不到运行状态时退回原文"
        );
        let just = RunState {
            started_at: Some(now - 20_000),
            ..running.clone()
        };
        assert_eq!(row_status(&c, Some(&just), now), "运行中 · 不到 1 分钟");
        let days = RunState {
            started_at: Some(now - 3 * 86_400_000),
            ..running.clone()
        };
        assert_eq!(row_status(&c, Some(&days), now), "运行中 · 3 天");
        let created = RunState {
            status: "created".into(),
            ..Default::default()
        };
        assert_eq!(row_status(&c, Some(&created), now), "已创建");
        assert_eq!(state_tone(&c, Some(&running)), Tone::Good);
        assert_eq!(state_tone(&c, Some(&exited)), Tone::Warning);
        let clean = RunState {
            exit_code: Some(0),
            ..exited.clone()
        };
        assert_eq!(state_tone(&c, Some(&clean)), Tone::Muted);
        let restarting = RunState {
            status: "restarting".into(),
            ..Default::default()
        };
        assert_eq!(state_tone(&c, Some(&restarting)), Tone::Danger);
    }

    /// 定时刷新读到列表后：选中的容器的运行状态没变时沿用详情，状态、启动时间、退出码、健康任一变了，或者上次
    /// 没读到时重读。
    #[test]
    fn detail_is_reread_only_when_the_state_changes() {
        let run = RunState {
            status: "running".into(),
            started_at: Some(1_000),
            health: Some(HealthState::Healthy),
            ..Default::default()
        };
        let detail = |run: RunState, info| Detail {
            id: container("running", "").id,
            run: Some(run),
            info,
            logs: None,
        };
        let list = |run: RunState| DockerInfo {
            engine: Engine::Docker,
            sudo: false,
            version: None,
            compose: None,
            containers: vec![Container {
                run: Some(run),
                ..container("running", "Up 1 hour")
            }],
            images: Vec::new(),
            volumes: Vec::new(),
            networks: Vec::new(),
        };
        let read = || Some(Ok(ContainerDetail::default()));
        // 状态没变：读到了、正在读都沿用
        assert!(detail_is_current(
            &detail(run.clone(), read()),
            &list(run.clone())
        ));
        assert!(detail_is_current(
            &detail(run.clone(), None),
            &list(run.clone())
        ));
        // 上次没读到
        let failed = Some(Err(ToolsError::Exec(ExecError::Disconnected)));
        assert!(!detail_is_current(
            &detail(run.clone(), failed),
            &list(run.clone())
        ));
        // 状态、启动时间、退出码、健康任一变了
        for changed in [
            RunState {
                status: "exited".into(),
                ..run.clone()
            },
            RunState {
                started_at: Some(2_000),
                ..run.clone()
            },
            RunState {
                exit_code: Some(1),
                ..run.clone()
            },
            RunState {
                health: Some(HealthState::Unhealthy),
                ..run.clone()
            },
        ] {
            assert!(!detail_is_current(
                &detail(run.clone(), read()),
                &list(changed)
            ));
        }
        // 容器已不在列表中
        let mut gone = list(run.clone());
        gone.containers.clear();
        assert!(!detail_is_current(&detail(run, read()), &gone));
    }
}
