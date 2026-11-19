//! 服务页：systemd 服务（有自定义的服务时分成“自定义服务”“系统服务”两组，各有标题与数量），可按全部、运行中、
//! 已停止、失败筛选与搜索；默认不列出单元文件不存在（not-found）的单元与已停止的静态单元，筛选条件的小标签写着藏了
//! 几个，点 × 或工具条上的开关显示全部。系统状态不是 running（degraded 等）时信息行上是一个可点的警告标签，点了看失败的
//! 服务。启动、停止、重启、重新加载、
//! 启用 / 禁用开机启动（都先确认，写明主机与将执行的命令；SSH、网络与防火墙类服务重点警告，关机、重启、休眠类单元
//! 最高级别警告）；最近日志与在新 Tab 中跟随日志。显示时读一次，有刷新按钮，操作后重读。状态的文字与圆点按各页共用
//! 的颜色表着色（运行中绿、失败红、已停止灰）。信息行尾标出这条连接上的权限，没有免密码 sudo 时各项操作带锁图标。
//!
//! 详情（头部一行：名称 ● 状态 · 启动 / 停止、重启 · ⋯，与列表之间的分隔条可拖动）中直接显示失败的原因与运行信息
//! （同一次 `systemctl show` 读到的：结果与退出码、已运行多久、重启次数、内存、CPU 时间、任务数）；选中失败的服务时
//! 另读最近几行日志显示在详情中，像 `systemctl status` 那样（时间按本机时区，与失败的时间对得上）。“查看日志”（⋯ 与
//! 右键菜单中）在中间区域的 Tab 中打开日志查看器。选中的服务被筛选、搜索藏起来时取消选中。
//!
//! - 行上的操作：鼠标停在行上（或键盘焦点所在的行）时右端浮出主要操作（运行中的重启，其余的启动）与 ⋯（与右键菜单
//!   相同）；操作进行中时那里转圈并写“正在重启…”，这一行与详情中的操作不能点，完成后写“已重启”一会儿。列表够宽时
//!   （[`super::processes::WIDE_FROM`]）每个服务一行：名称、描述、开机启动、状态。
//! - 跳转：主进程跳到进程页，“查看连接”跳到网络页并按主进程筛选；从进程页跳过来的服务不在列表中时说明。
//! - 定时器（与筛选并列的“定时器”子页，与服务同一次读出）：每个定时器一行，写出下次触发（本机时区，带“22 小时后”）、
//!   上次触发、触发的服务与状态，触发的服务失败着时另有一个红色的小标签；按下次触发的时间排序，可以搜索。详情写出
//!   日历表达式与按相对时间的条件、下次与上次的完整时间、错过时是否补做、随机延迟与单元文件，触发的服务可以点过去
//!   （服务详情中也列出触发它的定时器，可以点回来）。启动、停止、开机启用、禁用走与服务相同的流程（确认框写明主机与
//!   命令，需要 root 时的说明与在终端中执行，审计，行上的进度与完成）；“查看日志”看的是触发的服务的日志。
//! - 键盘（见 [`super::keys`]）：↑ ↓ 等移动并选中，Enter 重读失败服务的日志，L 查看日志，Delete 停止（先确认），
//!   Shift+F10 打开右键菜单。

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tab::Tab;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, StyledExt as _,
    VirtualListScrollHandle, h_flex, v_flex, v_virtual_list,
};
use gpui_kit::{
    AnyElement, App, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, Render, ScrollHandle, ScrollStrategy, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, TestSupportExt as _, Window,
    canvas, div, prelude::FluentBuilder as _, px, rems, size,
};
use termlet_core::hosttools::{
    Action, ExecError, LogQuery, LogResult, LogSource, Privilege, ServerZone, Service, ServiceList,
    ServiceOp, SudoState, Target, Timer, ToolsError,
};

use super::docker::ago;
use super::errors;
use super::keys::{
    DeleteItem, LIST_CONTEXT, OpenItem, OpenLogs, OpenMenu, SelectFirst, SelectLast, SelectNext,
    SelectPageDown, SelectPageUp, SelectPrev,
};
use super::log_panel::follow_request;
use super::model::{
    ConnChip, Page, SERVICE_OPS, ServiceFilter, ServiceRisk, ServiceRow, Step, TIMER_OPS,
    TimerClock, calendar_has_zone, event_time, event_time_at, filter_services, filter_timers,
    is_service_failed, is_service_running, is_timer_active, page_rows, service_hidden_by_default,
    service_name, service_op_applies, service_result, service_risk, service_rows, step_row,
    systemd_version, timer_name, timer_op_applies, timers_of, utc_offset_label, zone_name,
};
use super::panel::ActRequest;
use super::processes::WIDE_FROM;
use super::shared::{
    self, Feed, KeyMenu, Memo, OpVerb, PageContext, RootScope, RowOps, Tone, ToolPage, action_item,
    dot, error_line, info_line, loading_text, notice, op_button,
};
use crate::monitor_model::format_uptime;
use crate::prose::prose;
use crate::sftp::list_model::format_size;
use crate::t;

/// 失败的服务的详情中显示的日志行数。
const TAIL_LINES: u32 = 10;
/// 详情头部直接显示的操作（适用时）；其余的在 ⋯ 中。重启只对运行中的服务显示在头部。
const PRIMARY_OPS: [ServiceOp; 3] = [ServiceOp::Start, ServiceOp::Stop, ServiceOp::Restart];
/// 用键盘移动选中的服务后，停这么久再读失败服务的日志（按住方向键时不逐个读）。
const KEY_DETAIL_DELAY: Duration = Duration::from_millis(250);
/// 行高（rem）：两行（名称与状态、描述与开机启动），列表够宽时一行；分组的标题一行。
const ROW_HEIGHT: f32 = 2.5;
const WIDE_ROW_HEIGHT: f32 = 1.75;
const HEADER_HEIGHT: f32 = 1.75;

/// 选中的失败服务最近几行日志。
struct Tail {
    unit: String,
    /// 读取中为 `None`（同一个服务重读时先留着上一次的结果）
    result: Option<Result<LogResult, ToolsError>>,
}

/// 写操作的对象：服务或定时器（单元名、审计记录中的名称）。
#[derive(Clone)]
struct UnitTarget {
    unit: String,
    /// 审计记录中的名称（描述）
    label: String,
    timer: bool,
}

impl UnitTarget {
    fn service(service: &Service) -> Self {
        Self {
            unit: service.unit.clone(),
            label: service.description.clone(),
            timer: false,
        }
    }

    fn timer(timer: &Timer) -> Self {
        Self {
            unit: timer.unit.clone(),
            label: timer.description.clone(),
            timer: true,
        }
    }

    /// 界面上的名称（去掉 `.service`、`.timer`）。
    fn name(&self) -> &str {
        if self.timer {
            timer_name(&self.unit)
        } else {
            service_name(&self.unit)
        }
    }
}

pub(super) struct ServicePage {
    context: PageContext,
    feed: Feed<ServiceList>,
    search: Entity<InputState>,
    filter: ServiceFilter,
    /// 选中的服务（完整的服务名）
    selected: Option<String>,
    /// 列出全部单元（含默认不列出的 not-found 与已停止的静态单元）
    show_all: bool,
    /// 筛选、分组后的行与各筛选的数量（读数、搜索、筛选、是否列出全部变了时重算）
    rows: Memo<ServiceList, (String, ServiceFilter, bool), Rc<Vec<ServiceRow>>>,
    counts: Memo<ServiceList, bool, [usize; ServiceFilter::ALL.len()]>,
    /// 默认不列出、这时藏着的单元数（不受筛选、搜索影响）
    hidden: Memo<ServiceList, (), usize>,
    /// 各行进行中与刚完成的操作（键为服务名）
    ops: RowOps<String>,
    /// 鼠标停在哪一行上（右端浮出这一行的操作）
    hover_row: Option<String>,
    /// 上一帧列表的宽度（够宽时每个服务一行）
    width: Rc<Cell<Pixels>>,
    scroll: VirtualListScrollHandle,
    /// 上方筛选页签的滚动状态：按它量页签排开的宽度
    tabs_scroll: ScrollHandle,
    /// 选中的服务失败着时它最近几行日志
    tail: Option<Tail>,
    /// 列表的焦点（键盘操作）
    focus: FocusHandle,
    key_menu: KeyMenu,
    /// 从进程页跳过来、列表还没读到时要选中的服务
    pending: Option<String>,
    /// 跳过来的服务不在列表中时的说明
    note: Option<SharedString>,
    /// 右键菜单打开着的那一行（右键不改变选中：选中会让详情出现、把列表挤矮，按下位置所在的行被挤开后
    /// gpui-kit 就画不出它的菜单）：菜单打开期间这一行高亮
    menu_row: Option<String>,
    /// 详情区顶上的分隔条（键盘调整高度）与详情这一帧的高度
    splitter: FocusHandle,
    detail_height: Rc<Cell<Pixels>>,
    _tail: Option<Task<()>>,
    _follow: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ToolPage for ServicePage {
    type Data = ServiceList;

    fn feed(&mut self) -> &mut Feed<ServiceList> {
        &mut self.feed
    }

    fn context(&self) -> &PageContext {
        &self.context
    }

    fn after_load(&mut self, cx: &mut Context<Self>) {
        if self.feed.data().is_some()
            && let Some(unit) = self.pending.take()
        {
            self.reveal(&unit, cx);
        }
        self.drop_hidden(cx);
        self.sync_tail(None, cx);
    }
}

impl ServicePage {
    pub fn new(context: PageContext, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = shared::search_input(|| t!("tools.service.search"), window, cx);
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
            filter: ServiceFilter::All,
            selected: None,
            show_all: false,
            rows: Memo::default(),
            counts: Memo::default(),
            hidden: Memo::default(),
            ops: RowOps::default(),
            hover_row: None,
            width: Rc::new(Cell::new(px(0.))),
            scroll: VirtualListScrollHandle::new(),
            tabs_scroll: ScrollHandle::new(),
            tail: None,
            focus: shared::list_focus(cx),
            key_menu: KeyMenu::default(),
            pending: None,
            note: None,
            menu_row: None,
            splitter: cx.focus_handle().tab_stop(true),
            detail_height: Rc::new(Cell::new(px(0.))),
            _tail: None,
            _follow: None,
            _subscriptions: subscriptions,
        }
    }

    /// 选中的服务被筛选、搜索藏起来（或已不在列表中）时取消选中：详情只留列表中看得到的服务。
    fn drop_hidden(&mut self, cx: &mut Context<Self>) {
        let (Some(unit), Some(data)) = (self.selected.clone(), self.feed.data()) else {
            return;
        };
        let rows = self.shown_rows(&data, cx);
        if position(&rows, &data, &unit).is_none() {
            self.selected = None;
            self.tail = None;
            self._tail = None;
            cx.notify();
        }
    }

    /// 列出全部单元（含默认不列出的），或者只列出常用的（工具条上的开关、筛选条件的小标签）。
    fn set_show_all(&mut self, show_all: bool, cx: &mut Context<Self>) {
        self.show_all = show_all;
        self.drop_hidden(cx);
        cx.notify();
    }

    fn set_hover_row(&mut self, unit: String, hovered: bool, cx: &mut Context<Self>) {
        if shared::hover_row(&mut self.hover_row, unit, hovered) {
            cx.notify();
        }
    }

    /// 切换筛选（上方的页签）。
    fn set_filter(&mut self, filter: ServiceFilter, cx: &mut Context<Self>) {
        self.filter = filter;
        self.drop_hidden(cx);
        cx.notify();
    }

    /// 启停服务要 root 而这条连接上没有免密码 sudo：操作带终端图标，确认框的主按钮是“在终端中执行”。
    fn locked(&self, data: &ServiceList) -> bool {
        data.uid != Some(0) && matches!(self.feed.privilege, Some(Privilege::Limited(_)))
    }

    /// 没有免密码 sudo 时 sudo 的情况（[`Self::locked`]）：确认框据此写出在终端中执行的命令。
    fn root_first(&self) -> Option<SudoState> {
        let data = self.feed.data()?;
        match self.feed.privilege {
            Some(Privilege::Limited(sudo)) if data.uid != Some(0) => Some(sudo),
            _ => None,
        }
    }

    pub fn set_connection(&mut self, target: Option<Target>, _: &mut Context<Self>) {
        self.context.target = target;
    }

    pub fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        shared::activate(self, active, cx);
    }

    /// 上方筛选页签按文案排开比可见区域宽出多少（[`crate::tab_fit::overflow`]）：面板据此放宽最小宽度。
    pub fn tabs_overflow(&self) -> Option<Pixels> {
        crate::tab_fit::overflow(&self.tabs_scroll, ServiceFilter::ALL.len())
    }

    /// 列表的焦点（面板切到这一页、跳过来时放进来）。
    pub fn list_focus(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// 搜索框（列表显示着时才有：还在读取、不可用时没有搜索框）。
    pub fn search(&self) -> Option<Entity<InputState>> {
        self.feed.data().map(|_| self.search.clone())
    }

    fn select(&mut self, unit: &str, cx: &mut Context<Self>) {
        self.select_after(unit, None, cx);
    }

    /// 选中一个服务，失败着时 `delay` 之后读它的日志（键盘移动时：停下来才读）。
    fn select_after(&mut self, unit: &str, delay: Option<Duration>, cx: &mut Context<Self>) {
        self.note = None;
        if self.selected.as_deref() != Some(unit) {
            self.selected = Some(unit.to_string());
            self.sync_tail(delay, cx);
            self.scroll_to_selected(cx);
            cx.notify();
        }
    }

    /// 右键菜单打开、关闭（换了一行的菜单时前一个的关闭不清掉后一个）。
    fn set_menu_row(&mut self, unit: Option<String>, cx: &mut Context<Self>) {
        if self.menu_row != unit {
            self.menu_row = unit;
            cx.notify();
        }
    }

    fn menu_row_closed(&mut self, unit: &str, cx: &mut Context<Self>) {
        if self.menu_row.as_deref() == Some(unit) {
            self.set_menu_row(None, cx);
        }
    }

    /// 把选中的服务滚进可见范围：详情出现、读到日志变高后列表变矮，点中的行可能被挤到详情下面
    /// （下一帧布局时按那时的高度滚）。
    fn scroll_to_selected(&mut self, cx: &App) {
        let (Some(data), Some(unit)) = (self.feed.data(), self.selected.clone()) else {
            return;
        };
        let rows = self.shown_rows(&data, cx);
        if let Some(ix) = position(&rows, &data, &unit) {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
    }

    /// 筛选、搜索、分组后的行（分组的标题与服务在列表中的位置；定时器子页是定时器）。
    fn shown_rows(&mut self, data: &Rc<ServiceList>, cx: &App) -> Rc<Vec<ServiceRow>> {
        let query = self.search.read(cx).value().to_string();
        self.rows.get(
            data,
            (query, self.filter, self.show_all),
            |data, (query, filter, show_all)| {
                if *filter == ServiceFilter::Timers {
                    return Rc::new(
                        filter_timers(&data.timers, query)
                            .into_iter()
                            .map(ServiceRow::Timer)
                            .collect(),
                    );
                }
                let rows = filter_services(&data.services, *filter, *show_all, query);
                Rc::new(service_rows(&data.services, &rows))
            },
        )
    }

    /// 跳到定时器子页并选中这个定时器（服务详情中触发它的定时器）：清空搜索。
    fn focus_timer(&mut self, unit: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.search
            .update(cx, |search, cx| search.set_value("", window, cx));
        self.filter = ServiceFilter::Timers;
        self.note = None;
        self.reveal(unit, cx);
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// 定时器详情中触发的服务：切到“全部”并选中它（默认不列出的静态单元先列出全部）。
    fn show_triggered(&mut self, unit: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_unit(unit.to_string(), window, cx);
        self.focus.focus(window, cx);
    }

    /// 从进程页跳过来（进程的“所属服务”）：清空搜索、看全部，选中并滚到这个服务；还没有数据时读到后再选中。
    pub fn focus_unit(&mut self, unit: String, window: &mut Window, cx: &mut Context<Self>) {
        self.search
            .update(cx, |search, cx| search.set_value("", window, cx));
        self.filter = ServiceFilter::All;
        self.note = None;
        if self.feed.data().is_some() {
            self.reveal(&unit, cx);
        } else {
            self.pending = Some(unit);
        }
        cx.notify();
    }

    /// 选中并滚到这个服务（是默认不列出的单元时——进程所属的静态单元等——先列出全部）；不在显示着的列表（可能是
    /// 缓存）中而正在读取时读到后再找，不在列表中时说明。
    fn reveal(&mut self, unit: &str, cx: &mut Context<Self>) {
        let Some(data) = self.feed.data() else {
            return;
        };
        if !self.show_all
            && data
                .services
                .iter()
                .any(|s| s.unit == unit && service_hidden_by_default(s))
        {
            self.show_all = true;
        }
        let rows = self.shown_rows(&data, cx);
        match position(&rows, &data, unit) {
            Some(ix) => {
                self.select(unit, cx);
                self.scroll.scroll_to_item(ix, ScrollStrategy::Center);
            }
            None if self.feed.loading => self.pending = Some(unit.to_string()),
            None => {
                self.note = Some(if unit.ends_with(".timer") {
                    t!(
                        "tools.jump.timer_missing",
                        name = timer_name(unit).to_string()
                    )
                } else {
                    t!(
                        "tools.jump.service_missing",
                        name = service_name(unit).to_string()
                    )
                });
                cx.notify();
            }
        }
    }

    // ------------------------------------------------------------ 键盘

    /// ↑ ↓ 等：移动并选中，滚动跟随。
    fn move_cursor(&mut self, step: Step, window: &mut Window, cx: &mut Context<Self>) {
        let Some(data) = self.feed.data() else {
            return;
        };
        let rows = self.shown_rows(&data, cx);
        let current = self
            .selected
            .as_deref()
            .and_then(|unit| position(&rows, &data, unit));
        let row = f32::from(rems(self.row_height(window)).to_pixels(window.rem_size()));
        let page = page_rows(f32::from(self.scroll.bounds().size.height), row);
        let selectable =
            |ix: usize| matches!(rows[ix], ServiceRow::Service(_) | ServiceRow::Timer(_));
        let Some(ix) = step_row(current, rows.len(), step, page, selectable) else {
            return;
        };
        let unit = match rows[ix] {
            ServiceRow::Service(i) => data.services[i].unit.clone(),
            ServiceRow::Timer(i) => data.timers[i].unit.clone(),
            ServiceRow::Header { .. } => return,
        };
        self.select_after(&unit, Some(KEY_DETAIL_DELAY), cx);
        self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        cx.notify();
    }

    /// 选中的服务。
    fn selected_service(&self) -> Option<Service> {
        let unit = self.selected.as_deref()?;
        let data = self.feed.data()?;
        data.services.iter().find(|s| s.unit == unit).cloned()
    }

    /// 选中的定时器。
    fn selected_timer(&self) -> Option<Timer> {
        let unit = self.selected.as_deref()?;
        let data = self.feed.data()?;
        data.timers.iter().find(|t| t.unit == unit).cloned()
    }

    /// Enter：重读失败服务最近的日志；没有选中的时选中第一行。
    fn open_item(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_some() {
            self.sync_tail(None, cx);
        } else {
            self.move_cursor(Step::First, window, cx);
        }
    }

    /// Delete：停止选中的服务或定时器（运行着时，先确认）。
    fn delete_item(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(service) = self.selected_service()
            && service_op_applies(&service, ServiceOp::Stop)
            && !self.ops.busy(&service.unit)
        {
            self.confirm_op(&UnitTarget::service(&service), ServiceOp::Stop, window, cx);
        } else if let Some(timer) = self.selected_timer()
            && timer_op_applies(&timer, ServiceOp::Stop)
            && !self.ops.busy(&timer.unit)
        {
            self.confirm_op(&UnitTarget::timer(&timer), ServiceOp::Stop, window, cx);
        }
    }

    /// L：查看选中的服务（定时器触发的服务）的日志。
    fn logs_item(&mut self, cx: &mut Context<Self>) {
        if let Some(service) = self.selected_service() {
            self.open_logs(&service.unit, cx);
        } else if let Some(timer) = self.selected_timer()
            && !timer.activates.is_empty()
        {
            self.open_logs(&timer.activates, cx);
        }
    }

    /// Shift+F10、菜单键：在选中的行下面打开它的右键菜单。
    fn open_key_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let locked = self.feed.data().is_some_and(|data| self.locked(&data));
        if let Some(service) = self.selected_service() {
            let build = self.menu_builder(&service, locked, cx);
            KeyMenu::open(
                window,
                cx,
                self.focus.clone(),
                |page| &mut page.key_menu,
                build,
            );
        } else if let Some(timer) = self.selected_timer() {
            let build = self.timer_menu_builder(&timer, locked, cx);
            KeyMenu::open(
                window,
                cx,
                self.focus.clone(),
                |page| &mut page.key_menu,
                build,
            );
        }
    }

    /// 选中的服务失败着时读它最近几行日志（每次读到列表后重读：操作之后日志有了新的内容）；是否经 sudo 与日志对话框
    /// 是同一个判断（同一个脚本）。选中的不是失败的服务时丢掉。`delay` 之后再读（键盘移动时）。
    fn sync_tail(&mut self, delay: Option<Duration>, cx: &mut Context<Self>) {
        let failed = self.selected.clone().filter(|unit| {
            self.feed.data().is_some_and(|data| {
                data.services
                    .iter()
                    .any(|s| &s.unit == unit && is_service_failed(s))
            })
        });
        let Some(unit) = failed else {
            self.tail = None;
            self._tail = None;
            return;
        };
        if self.tail.as_ref().is_none_or(|tail| tail.unit != unit) {
            self.tail = Some(Tail {
                unit: unit.clone(),
                result: None,
            });
        }
        let Some(target) = self.context.target.clone() else {
            // 终端正在重连：连上后列表重读时再读
            if let Some(tail) = self.tail.as_mut().filter(|tail| tail.result.is_none()) {
                tail.result = Some(Err(ToolsError::Exec(ExecError::Disconnected)));
            }
            return;
        };
        let tools = self.context.tools.clone();
        let request = LogQuery::recent(LogSource::Service { unit: unit.clone() }, TAIL_LINES);
        self._tail = Some(cx.spawn(async move |this, cx| {
            if let Some(delay) = delay {
                cx.background_executor().timer(delay).await;
            }
            let result = tools.logs(&target, request).await;
            let _ = this.update(cx, |page, cx| {
                if let Some(tail) = page.tail.as_mut().filter(|tail| tail.unit == unit) {
                    let first = tail.result.is_none();
                    tail.result = Some(result);
                    if first {
                        page.scroll_to_selected(cx);
                    }
                    cx.notify();
                }
            });
        }));
    }

    /// 确认后执行：一般的写明影响；SSH、网络与防火墙类重点警告；关机、重启、休眠类单元最高级别（定时器都是一般的）。
    fn confirm_op(
        &mut self,
        target: &UnitTarget,
        op: ServiceOp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = target.name().to_string();
        let (risk, title, hint) = if target.timer {
            (
                ServiceRisk::Confirm,
                timer_confirm_title(op, &name),
                timer_hint(op),
            )
        } else {
            (
                service_risk(&target.unit, op),
                confirm_title(op, &name),
                op_hint(op),
            )
        };
        let host = self.context.host(cx);
        let action = Action::Service {
            op,
            unit: target.unit.clone(),
        };
        // 不是 root 时经免密码 sudo 执行（与审计记录的写法一致）；这条连接上没有免密码 sudo 时主按钮是“在终端中执行”，
        // 写出在终端中执行的命令，“仍然尝试”照常在后台执行
        let sudo = self.feed.data().is_some_and(|data| data.uid != Some(0));
        let root_first = self.root_first();
        let command = action.command_line_with(sudo).unwrap_or_default();
        let danger = risk != ServiceRisk::Confirm || op == ServiceOp::Stop;
        let page = cx.entity().downgrade();
        let dialogs = self.context.dialogs.clone();
        let target = target.clone();
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, cx| {
            let theme = cx.theme();
            let body =
                v_flex()
                    .id(SharedString::from(format!(
                        "tools-service-risk-{}",
                        risk.key()
                    )))
                    .test_support()
                    .gap_1p5()
                    .text_xs()
                    .children(host.as_ref().map(|host| shared::confirm_host(host, cx)))
                    .child(match risk {
                        ServiceRisk::Power => div()
                            .font_medium()
                            .text_color(theme.danger)
                            .child(prose(t!("tools.service.power_risk", name = name.clone()))),
                        ServiceRisk::Connection => div()
                            .font_medium()
                            .text_color(theme.warning)
                            .child(prose(t!(
                                "tools.service.connection_risk",
                                name = name.clone()
                            ))),
                        ServiceRisk::Confirm => div().child(prose(hint.clone())),
                    })
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
                let (ok_page, ok_target) = (page.clone(), target.clone());
                let (try_page, try_target) = (page.clone(), target.clone());
                let (run_page, run_target) = (page.clone(), target.clone());
                return alert
                    .on_ok(move |_, window, cx| {
                        let _ = ok_page.update(cx, |page, cx| {
                            page.act_in_terminal(&ok_target, op, sudo, window, cx)
                        });
                        true
                    })
                    .footer(shared::root_first_footer(
                        danger,
                        move |window, cx| {
                            let _ = try_page
                                .update(cx, |page, cx| page.act(&try_target, op, window, cx));
                        },
                        move |window, cx| {
                            let _ = run_page.update(cx, |page, cx| {
                                page.act_in_terminal(&run_target, op, sudo, window, cx)
                            });
                        },
                        |_| {},
                    ));
            }
            let page = page.clone();
            let target = target.clone();
            alert
                .confirm()
                .ok_text(op_label(op))
                .ok_variant(if danger {
                    ButtonVariant::Danger
                } else {
                    ButtonVariant::Primary
                })
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, window, cx| {
                    let _ = page.update(cx, |page, cx| page.act(&target, op, window, cx));
                    true
                })
        });
        dialogs.opened(window, cx);
    }

    /// 写操作的请求（失败的提示、需要 root 时的说明、审计）。
    fn request(&self, target: &UnitTarget, op: ServiceOp) -> ActRequest {
        let name = target.name().to_string();
        ActRequest {
            action: Action::Service {
                op,
                unit: target.unit.clone(),
            },
            title: if target.timer {
                timer_action(op, &name)
            } else {
                op_action(op, &name)
            },
            label: target.label.clone(),
            docker: false,
            page: Page::Services,
        }
    }

    /// 没有免密码 sudo 时的主按钮：在发起操作的终端旁的新 Tab 中执行（不先在后台尝试）。
    fn act_in_terminal(
        &mut self,
        target: &UnitTarget,
        op: ServiceOp,
        sudo: SudoState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request = self.request(target, op);
        let context = self.context.clone();
        let _ = self.context.panel.update(cx, |panel, cx| {
            panel.run_root_first(&context, request, sudo, window, cx)
        });
    }

    fn act(
        &mut self,
        target: &UnitTarget,
        op: ServiceOp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request = self.request(target, op);
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
            vec![target.unit.clone()],
            op_verb(op),
            done,
            |page, _, cx| shared::refresh(page, false, cx),
            cx,
        );
    }

    /// 在中间区域的 Tab 中查看服务（`unit`）的日志。
    fn open_logs(&self, unit: &str, cx: &mut Context<Self>) {
        let context = self.context.clone();
        let source = LogSource::Service {
            unit: unit.to_string(),
        };
        let name = service_name(unit).to_string();
        let _ = self
            .context
            .panel
            .update(cx, |panel, cx| panel.open_logs(&context, source, name, cx));
    }

    /// 在新 Tab 中跟随日志（右键菜单）。是否带 sudo 与日志查看器同一个判断：按读日志的脚本（不是 root 时先试免密码
    /// sudo）读一行，经 sudo 读到时跟随也用 sudo；详情中已经读过这个服务的日志时直接用那次的结果。
    fn follow_logs(&mut self, service: &Service, window: &mut Window, cx: &mut Context<Self>) {
        let unit = service.unit.clone();
        let title = t!("tools.tab_logs", name = service_name(&unit));
        let known = self
            .tail
            .as_ref()
            .filter(|tail| tail.unit == unit)
            .and_then(|tail| tail.result.as_ref()?.as_ref().ok())
            .map(|logs| logs.sudo);
        let target = self.context.target.clone();
        let (Some(target), None) = (target, known) else {
            self.run_follow(&unit, known.unwrap_or(false), title, window, cx);
            return;
        };
        let tools = self.context.tools.clone();
        let request = LogQuery::recent(LogSource::Service { unit: unit.clone() }, 1);
        self._follow = Some(cx.spawn_in(window, async move |this, cx| {
            let sudo = tools
                .logs(&target, request)
                .await
                .is_ok_and(|logs| logs.sudo);
            let _ = this.update_in(cx, |page, window, cx| {
                page.run_follow(&unit, sudo, title, window, cx)
            });
        }));
    }

    fn run_follow(
        &self,
        unit: &str,
        sudo: bool,
        title: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request = follow_request(
            &LogSource::Service {
                unit: unit.to_string(),
            },
            sudo,
        );
        let context = self.context.clone();
        let _ = self.context.panel.update(cx, |panel, cx| {
            panel.run_request(&context, request, title, window, cx)
        });
    }

    /// 行的右键菜单（鼠标右键与键盘打开的、详情头部的 ⋯ 都是同一个）：适用的操作（停止用警示样式，没有免密码 sudo
    /// 时带锁图标），查看与跟随日志，主进程与它的连接。
    fn menu_builder(
        &self,
        service: &Service,
        locked: bool,
        cx: &mut Context<Self>,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
        let page = cx.entity().downgrade();
        let menu_service = service.clone();
        let busy = self.ops.busy(&service.unit);
        let context = self.context.clone();
        move |menu, window, cx| {
            let (page_close, unit) = (page.clone(), menu_service.unit.clone());
            let _ = page.update(cx, |page, cx| page.set_menu_row(Some(unit.clone()), cx));
            shared::on_menu_dismiss(window, cx, move |cx| {
                let _ = page_close.update(cx, |page, cx| page.menu_row_closed(&unit, cx));
            });
            let mut menu = menu;
            for op in SERVICE_OPS
                .into_iter()
                .filter(|op| service_op_applies(&menu_service, *op))
            {
                let item =
                    action_item(op_label(op), op_icon(op), op == ServiceOp::Stop, locked, cx);
                let (page, target) = (page.clone(), UnitTarget::service(&menu_service));
                menu = menu.item(item.disabled(busy).on_click(move |_, window, cx| {
                    let _ = page.update(cx, |page, cx| page.confirm_op(&target, op, window, cx));
                }));
            }
            let (logs_page, logs_unit) = (page.clone(), menu_service.unit.clone());
            let (follow_page, follow_service) = (page.clone(), menu_service.clone());
            let mut menu = menu
                .separator()
                .item(
                    PopupMenuItem::new(t!("tools.logs.view"))
                        .icon(IconName::FileText)
                        .on_click(move |_, _, cx| {
                            let _ = logs_page.update(cx, |page, cx| page.open_logs(&logs_unit, cx));
                        }),
                )
                .item(
                    PopupMenuItem::new(t!("tools.follow_logs"))
                        .icon(IconName::SquareTerminal)
                        .on_click(move |_, window, cx| {
                            let _ = follow_page.update(cx, |page, cx| {
                                page.follow_logs(&follow_service, window, cx)
                            });
                        }),
                );
            if let Some(pid) = menu_service.main_pid {
                let (process_context, conn_context) = (context.clone(), context.clone());
                let name = shared::process_name(&context, pid);
                menu = menu
                    .separator()
                    .item(
                        PopupMenuItem::new(t!("tools.service.show_main_process"))
                            .icon(IconName::Cpu)
                            .on_click(move |_, window, cx| {
                                shared::go(
                                    &process_context,
                                    window,
                                    cx,
                                    move |panel, window, cx| panel.show_process(pid, window, cx),
                                );
                            }),
                    )
                    .item(
                        PopupMenuItem::new(t!("tools.service.show_connections"))
                            .icon(IconName::Network)
                            .on_click(move |_, window, cx| {
                                let name = name.clone();
                                shared::go(&conn_context, window, cx, move |panel, window, cx| {
                                    panel.show_connections(ConnChip::Pid { pid, name }, window, cx)
                                });
                            }),
                    );
            }
            menu
        }
    }

    /// 定时器的右键菜单（鼠标右键与键盘打开的、行右端与详情头部的 ⋯ 都是同一个）：适用的操作（停止用警示样式，没有
    /// 免密码 sudo 时带锁图标），触发的服务的日志，跳到触发的服务。
    fn timer_menu_builder(
        &self,
        timer: &Timer,
        locked: bool,
        cx: &mut Context<Self>,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
        let page = cx.entity().downgrade();
        let menu_timer = timer.clone();
        let busy = self.ops.busy(&timer.unit);
        move |menu, window, cx| {
            let (page_close, unit) = (page.clone(), menu_timer.unit.clone());
            let _ = page.update(cx, |page, cx| page.set_menu_row(Some(unit.clone()), cx));
            shared::on_menu_dismiss(window, cx, move |cx| {
                let _ = page_close.update(cx, |page, cx| page.menu_row_closed(&unit, cx));
            });
            let mut menu = menu;
            for op in TIMER_OPS
                .into_iter()
                .filter(|op| timer_op_applies(&menu_timer, *op))
            {
                let item =
                    action_item(op_label(op), op_icon(op), op == ServiceOp::Stop, locked, cx);
                let (page, target) = (page.clone(), UnitTarget::timer(&menu_timer));
                menu = menu.item(item.disabled(busy).on_click(move |_, window, cx| {
                    let _ = page.update(cx, |page, cx| page.confirm_op(&target, op, window, cx));
                }));
            }
            let triggered = menu_timer.activates.clone();
            if triggered.ends_with(".service") {
                let name = service_name(&triggered).to_string();
                let (logs_page, logs_unit) = (page.clone(), triggered.clone());
                let jump_page = page.clone();
                menu = menu
                    .separator()
                    .item(
                        PopupMenuItem::new(t!("tools.timer.view_logs", name = name.clone()))
                            .icon(IconName::FileText)
                            .on_click(move |_, _, cx| {
                                let _ =
                                    logs_page.update(cx, |page, cx| page.open_logs(&logs_unit, cx));
                            }),
                    )
                    .item(
                        PopupMenuItem::new(t!("tools.timer.show_service", name = name))
                            .icon(IconName::ServerCog)
                            .on_click(move |_, window, cx| {
                                let unit = triggered.clone();
                                // 等菜单关掉之后再切换（切换会重建列表）
                                let page = jump_page.clone();
                                window.defer(cx, move |window, cx| {
                                    let _ = page.update(cx, |page, cx| {
                                        page.show_triggered(&unit, window, cx)
                                    });
                                });
                            }),
                    );
            }
            menu
        }
    }

    // ------------------------------------------------------------ 渲染

    /// 行高（rem）：列表够宽时每个服务一行。
    fn row_height(&self, window: &Window) -> f32 {
        if self.wide(window) {
            WIDE_ROW_HEIGHT
        } else {
            ROW_HEIGHT
        }
    }

    /// 列表够宽（[`WIDE_FROM`]）：每个服务一行（名称、描述、开机启动、状态）。
    fn wide(&self, window: &Window) -> bool {
        self.width.get() >= rems(WIDE_FROM).to_pixels(window.rem_size())
    }

    /// 分组的标题：“自定义服务 3”“系统服务 112”。
    fn render_header_row(&self, custom: bool, count: usize, cx: &App) -> AnyElement {
        let theme = cx.theme();
        h_flex()
            .id(if custom {
                "tools-service-group-custom"
            } else {
                "tools-service-group-system"
            })
            .test_support()
            .size_full()
            .px_1()
            .gap_1p5()
            .text_xs()
            .bg(theme.muted.opacity(0.4))
            .child(div().font_medium().child(if custom {
                t!("tools.service.group_custom")
            } else {
                t!("tools.service.group_system")
            }))
            .child(
                div()
                    .text_color(theme.muted_foreground)
                    .child(count.to_string()),
            )
            .into_any_element()
    }

    fn render_row(
        &self,
        service: &Service,
        locked: bool,
        wide: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.selected.as_deref() == Some(service.unit.as_str());
        let ring = selected && (self.focus.is_focused(window) || self.key_menu.is_open());
        let menu_layer = selected.then(|| self.key_menu.layer(window)).flatten();
        let menu = self.menu_builder(service, locked, cx);
        let tone = service_tone(service);
        let (dot_color, state_color) = (tone.dot(cx), tone.color(cx));
        let unit = service.unit.clone();
        let menu_row = self.menu_row.as_deref() == Some(unit.as_str());
        let hovered = self.hover_row.as_deref() == Some(unit.as_str());
        let theme = cx.theme();
        let (muted, row_bg) = (
            theme.muted_foreground,
            if selected {
                theme.list_active
            } else {
                theme.list_hover
            },
        );
        // 状态的位置：操作进行中、刚完成时说明；鼠标停在行上、键盘焦点在这一行、菜单打开着时浮出主要操作与 ⋯，盖在
        // 状态所在的一行的右端（状态文字让出来，不露出半截）
        let op = self.ops.get(&unit);
        let actions = match op {
            Some(_) => None,
            None if hovered || menu_row || ring => {
                let op = if is_service_running(service) {
                    ServiceOp::Restart
                } else {
                    ServiceOp::Start
                };
                let target = UnitTarget::service(service);
                let primary =
                    shared::row_action_button(
                        SharedString::from(format!("tools-service-row-{}-{unit}", op_key(op))),
                        op_icon(op),
                        shared::op_tip(op_label(op), locked),
                        false,
                        false,
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.confirm_op(&target, op, window, cx)
                    }))
                    .into_any_element();
                let more = shared::more_button(
                    SharedString::from(format!("tools-service-row-more-{unit}")),
                    self.menu_builder(service, locked, cx),
                    cx,
                );
                Some(vec![primary, more])
            }
            None => None,
        }
        .map(|buttons| {
            shared::row_actions(
                SharedString::from(format!("tools-service-actions-{unit}")),
                row_bg,
                buttons,
                cx,
            )
        });
        let theme = cx.theme();
        let description: SharedString = service.description.clone().into();
        let name = div()
            .min_w_0()
            .truncate()
            .child(service_name(&service.unit).to_string());
        let covered = actions.is_some();
        let state = match op {
            Some(op) => shared::op_status(
                SharedString::from(format!("tools-service-op-{unit}")),
                op,
                cx,
            ),
            None => div()
                .id(SharedString::from(format!(
                    "tools-service-state-{}",
                    service.unit
                )))
                .test_support()
                .flex_none()
                .text_color(if covered {
                    theme.transparent
                } else {
                    state_color
                })
                .child(active_label(&service.active_state))
                .into_any_element(),
        };
        let description = div()
            .id(SharedString::from(format!(
                "tools-service-description-{}",
                service.unit
            )))
            .test_support()
            .flex_1()
            .min_w_0()
            .truncate()
            .text_color(muted.opacity(0.8))
            .child(prose(description.clone()))
            .tooltip(shared::row_tooltip(description));
        let boot = (!service.unit_file_state.is_empty()).then(|| {
            div()
                .flex_none()
                .text_color(muted.opacity(0.7))
                .child(boot_label(&service.unit_file_state))
        });
        let select_unit = unit.clone();
        let hover_unit = unit.clone();
        let row = v_flex()
            .id(SharedString::from(format!(
                "tools-service-{}",
                service.unit
            )))
            .test_support()
            .size_full()
            .justify_center()
            .px_1()
            .text_xs()
            .cursor_default()
            .when(selected, |el| el.bg(theme.list_active))
            .when(!selected && menu_row, |el| el.bg(theme.list_hover))
            .when(!selected, |el| el.hover(|s| s.bg(theme.list_hover)));
        let row = shared::ring(row, ring, cx)
            .children(menu_layer)
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                this.set_hover_row(hover_unit.clone(), *hovered, cx)
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.focus.focus(window, cx);
                this.select(&select_unit, cx)
            }));
        // 够宽时一行：● 名称 · 描述 · 开机启动 · 状态；否则两行：● 名称 … 状态 / 描述 … 开机启动
        let row = if wide {
            row.child(
                h_flex()
                    .relative()
                    .gap_1p5()
                    .child(dot(dot_color))
                    .child(name.flex_none().max_w(rems(14.)))
                    .child(description)
                    .children(boot)
                    .child(state)
                    .children(actions),
            )
        } else {
            row.child(
                h_flex()
                    .relative()
                    .gap_1p5()
                    .child(dot(dot_color))
                    .child(name)
                    .child(div().flex_1())
                    .child(state)
                    .children(actions),
            )
            .child(h_flex().pl_3().gap_1p5().child(description).children(boot))
        };
        row.context_menu(menu).into_any_element()
    }

    /// 定时器的一行。够宽时一行：● 名称 · 下次与上次 · 触发的服务 · 状态；否则两行：● 名称 … 状态 / 下次与上次 …
    /// 触发的服务。触发的服务失败着时状态前另有一个红色的小标签。
    #[allow(clippy::too_many_arguments)]
    fn render_timer_row(
        &self,
        timer: &Timer,
        data: &ServiceList,
        locked: bool,
        wide: bool,
        now: i64,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let unit = timer.unit.clone();
        let selected = self.selected.as_deref() == Some(unit.as_str());
        let ring = selected && (self.focus.is_focused(window) || self.key_menu.is_open());
        let menu_layer = selected.then(|| self.key_menu.layer(window)).flatten();
        let menu = self.timer_menu_builder(timer, locked, cx);
        let tone = timer_tone(timer);
        let (dot_color, state_color) = (tone.dot(cx), tone.color(cx));
        let menu_row = self.menu_row.as_deref() == Some(unit.as_str());
        let hovered = self.hover_row.as_deref() == Some(unit.as_str());
        let theme = cx.theme();
        let (muted, row_bg) = (
            theme.muted_foreground,
            if selected {
                theme.list_active
            } else {
                theme.list_hover
            },
        );
        let op = self.ops.get(&unit);
        let actions = match op {
            Some(_) => None,
            None if hovered || menu_row || ring => {
                let op = if is_timer_active(timer) {
                    ServiceOp::Stop
                } else {
                    ServiceOp::Start
                };
                let target = UnitTarget::timer(timer);
                let primary =
                    shared::row_action_button(
                        SharedString::from(format!("tools-timer-row-{}-{unit}", op_key(op))),
                        op_icon(op),
                        shared::op_tip(op_label(op), locked),
                        op == ServiceOp::Stop,
                        false,
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.confirm_op(&target, op, window, cx)
                    }))
                    .into_any_element();
                let more = shared::more_button(
                    SharedString::from(format!("tools-timer-row-more-{unit}")),
                    self.timer_menu_builder(timer, locked, cx),
                    cx,
                );
                Some(vec![primary, more])
            }
            None => None,
        }
        .map(|buttons| {
            shared::row_actions(
                SharedString::from(format!("tools-timer-actions-{unit}")),
                row_bg,
                buttons,
                cx,
            )
        });
        let covered = actions.is_some();
        let theme = cx.theme();
        let state = match op {
            Some(op) => {
                shared::op_status(SharedString::from(format!("tools-timer-op-{unit}")), op, cx)
            }
            None => div()
                .id(SharedString::from(format!("tools-timer-state-{unit}")))
                .test_support()
                .flex_none()
                .text_color(if covered {
                    theme.transparent
                } else {
                    state_color
                })
                .child(timer_state_label(timer))
                .into_any_element(),
        };
        // 触发的服务失败着：一个红色的小标签（盖住时让出来）
        let failed = (!covered && triggered_failed(timer, data)).then(|| {
            shared::status_badge(t!("tools.timer.service_failed"), Tone::Danger, cx)
                .id(SharedString::from(format!("tools-timer-failed-{unit}")))
                .test_support()
        });
        let name = div()
            .min_w_0()
            .truncate()
            .child(timer_name(&timer.unit).to_string());
        let schedule: SharedString = schedule_text(timer, now).into();
        let schedule = div()
            .id(SharedString::from(format!("tools-timer-schedule-{unit}")))
            .test_support()
            .flex_1()
            .min_w_0()
            .truncate()
            .text_color(muted.opacity(0.8))
            .child(schedule)
            .tooltip(shared::row_tooltip(schedule_tooltip(timer).into()));
        let triggers = (!timer.activates.is_empty()).then(|| {
            div()
                .flex_none()
                .max_w(rems(10.))
                .truncate()
                .text_color(muted.opacity(0.7))
                .child(format!("→ {}", service_name(&timer.activates)))
        });
        let select_unit = unit.clone();
        let hover_unit = unit.clone();
        let row = v_flex()
            .id(SharedString::from(format!("tools-timer-{unit}")))
            .test_support()
            .size_full()
            .justify_center()
            .px_1()
            .text_xs()
            .cursor_default()
            .when(selected, |el| el.bg(theme.list_active))
            .when(!selected && menu_row, |el| el.bg(theme.list_hover))
            .when(!selected, |el| el.hover(|s| s.bg(theme.list_hover)));
        let row = shared::ring(row, ring, cx)
            .children(menu_layer)
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                this.set_hover_row(hover_unit.clone(), *hovered, cx)
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.focus.focus(window, cx);
                this.select(&select_unit, cx)
            }));
        let row = if wide {
            row.child(
                h_flex()
                    .relative()
                    .gap_1p5()
                    .child(dot(dot_color))
                    .child(name.flex_none().max_w(rems(12.)))
                    .child(schedule)
                    .children(triggers)
                    .children(failed)
                    .child(state)
                    .children(actions),
            )
        } else {
            row.child(
                h_flex()
                    .relative()
                    .gap_1p5()
                    .child(dot(dot_color))
                    .child(name)
                    .child(div().flex_1())
                    .children(failed)
                    .child(state)
                    .children(actions),
            )
            .child(h_flex().pl_3().gap_1p5().child(schedule).children(triggers))
        };
        row.context_menu(menu).into_any_element()
    }

    /// 定时器的详情：头部一行（名称 ● 状态 · 启动 / 停止 · ⋯），下面是说明、开机启用、触发的服务（可以点过去，失败着时
    /// 写明）、计划（日历表达式与按相对时间的条件）、下次与上次触发的时间、错过时补做、随机延迟与单元文件。
    fn render_timer_detail(
        &self,
        timer: &Timer,
        data: &ServiceList,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let now = termlet_core::time::now_ms();
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
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
        let mut rows = vec![row(
            t!("tools.service.description"),
            text(timer.description.clone()),
        )];
        rows.push(row(
            t!("tools.service.boot"),
            text(if timer.unit_file_state.is_empty() {
                "-".into()
            } else {
                boot_label(&timer.unit_file_state).to_string()
            }),
        ));
        if !timer.activates.is_empty() {
            // 按钮按自己的宽度放在值那一列的开头，去掉水平内边距：文字与同一列的其他值左对齐
            let triggered = timer.activates.clone();
            let failed = data
                .services
                .iter()
                .find(|s| s.unit == triggered)
                .filter(|s| is_service_failed(s))
                .map(|s| {
                    let result = service_result(s).unwrap_or_else(|| s.result.clone());
                    div()
                        .id("tools-timer-service-failed")
                        .test_support()
                        .text_color(danger)
                        .child(prose(t!(
                            "tools.timer.service_failed_result",
                            result = result
                        )))
                });
            rows.push(row(
                t!("tools.timer.triggers"),
                v_flex()
                    .child(
                        h_flex().child(
                            Button::new("tools-timer-service-link")
                                .ghost()
                                .xsmall()
                                .px_0()
                                .label(triggered.clone())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.show_triggered(&triggered, window, cx)
                                })),
                        ),
                    )
                    .children(failed)
                    .into_any_element(),
            ));
        }
        let plans: Vec<String> = timer
            .calendar
            .iter()
            .map(|c| format!("OnCalendar={c}"))
            .chain(timer.monotonic.iter().cloned())
            .collect();
        if !plans.is_empty() {
            // 日历表达式（没有写明时区的）按服务器的时区解释：写明是哪个时区
            let calendar_zone = data
                .zone
                .as_ref()
                .filter(|_| timer.calendar.iter().any(|c| !calendar_has_zone(c)))
                .map(|zone| {
                    div()
                        .id("tools-timer-calendar-zone")
                        .test_support()
                        .text_color(muted)
                        .child(prose(t!(
                            "tools.timer.calendar_zone",
                            zone = zone_text(zone)
                        )))
                });
            rows.push(row(
                t!("tools.timer.plan"),
                v_flex()
                    .child(
                        v_flex()
                            .id("tools-timer-plan")
                            .test_support()
                            .font_family(mono.clone())
                            .children(plans.into_iter().map(|p| div().child(p))),
                    )
                    .children(calendar_zone)
                    .into_any_element(),
            ));
        }
        if let Some(zone) = &data.zone {
            rows.push(row(t!("tools.timer.zone"), zone_detail(zone, now, cx)));
        }
        // 下次与上次：服务器那时的时间（与本机不同时带上偏移，下面另写本机的时间）
        let when = |id: &'static str,
                    at: Option<i64>,
                    offset: Option<i32>,
                    rel: SharedString,
                    none: SharedString| {
            let Some(at) = at else {
                return div().id(id).test_support().child(none).into_any_element();
            };
            let (time, local) = timer_time_full(at, offset);
            v_flex()
                .id(id)
                .test_support()
                .child(prose(t!("tools.timer.time_rel", time = time, rel = rel)))
                .children(local.map(|local| {
                    div()
                        .text_color(muted)
                        .child(prose(t!("tools.timer.local_time", time = local)))
                }))
                .into_any_element()
        };
        rows.push(row(
            t!("tools.timer.next"),
            when(
                "tools-timer-next",
                timer.next,
                timer.next_offset,
                timer.next.map(|at| until(at - now)).unwrap_or_default(),
                t!("tools.timer.no_next"),
            ),
        ));
        rows.push(row(
            t!("tools.timer.last"),
            when(
                "tools-timer-last",
                timer.last,
                timer.last_offset,
                timer.last.map(|at| ago(now - at)).unwrap_or_default(),
                t!("tools.timer.never"),
            ),
        ));
        if timer.persistent {
            rows.push(row(
                t!("tools.timer.missed"),
                div()
                    .child(prose(t!("tools.timer.persistent")))
                    .into_any_element(),
            ));
        }
        if !timer.randomized_delay.is_empty() {
            rows.push(row(
                t!("tools.timer.random_delay"),
                text(t!("tools.timer.up_to", delay = timer.randomized_delay.clone()).to_string()),
            ));
        }
        if let Some(path) = &timer.fragment_path {
            rows.push(row(
                t!("tools.service.unit_file"),
                div()
                    .font_family(mono)
                    .child(path.clone())
                    .into_any_element(),
            ));
        }
        // 头部：名称 ● 状态 · 启动 / 停止（操作进行中时不能点） · ⋯（开机启用、禁用，日志，跳到服务）
        let locked = self.locked(data);
        let busy = self.ops.busy(&timer.unit);
        let actions: Vec<AnyElement> =
            [ServiceOp::Start, ServiceOp::Stop]
                .into_iter()
                .filter(|op| timer_op_applies(timer, *op))
                .map(|op| {
                    let target = UnitTarget::timer(timer);
                    op_button(
                        SharedString::from(format!("tools-timer-detail-{}", op_key(op))),
                        op_icon(op),
                        op_label(op),
                        op == ServiceOp::Stop,
                        locked,
                    )
                    .disabled(busy)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.confirm_op(&target, op, window, cx)
                    }))
                    .into_any_element()
                })
                .collect();
        let more = shared::more_button(
            "tools-timer-detail-more",
            self.timer_menu_builder(timer, locked, cx),
            cx,
        );
        let header = shared::detail_header(
            timer.unit.clone().into(),
            Some((timer_state_label(timer), timer_tone(timer))),
            actions,
            Some(more),
            cx,
        );
        shared::detail_pane(
            "tools-timer-detail",
            &self.splitter,
            &self.detail_height,
            header,
            rows,
            window,
            cx,
        )
    }

    fn render_detail(
        &self,
        data: &ServiceList,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let unit = self.selected.as_deref()?;
        if let Some(timer) = data.timers.iter().find(|t| t.unit == unit) {
            return Some(self.render_timer_detail(timer, data, window, cx));
        }
        let service = data.services.iter().find(|s| s.unit == unit)?.clone();
        let now = termlet_core::time::now_ms();
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
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
        let state = state_line(&service, now);
        // 失败的原因：结果与退出码（或信号），失败的时间
        let failure = is_service_failed(&service)
            .then(|| service_result(&service))
            .flatten()
            .map(|result| match service.state_changed {
                Some(at) => t!(
                    "tools.service.failed_result",
                    result = result,
                    time = event_time(at, now, crate::sftp::file_list::date_style())
                ),
                None => t!("tools.service.failed_result_no_time", result = result),
            });
        let mut rows = vec![
            row(
                t!("tools.service.description"),
                text(service.description.clone()),
            ),
            row(t!("tools.service.state"), text(state)),
            row(
                t!("tools.service.boot"),
                text(if service.unit_file_state.is_empty() {
                    "-".into()
                } else {
                    boot_label(&service.unit_file_state).to_string()
                }),
            ),
        ];
        // 触发它的定时器：可以点过去
        let timers = timers_of(&data.timers, &service.unit);
        if !timers.is_empty() {
            let links: Vec<AnyElement> = timers
                .iter()
                .map(|timer| {
                    let unit = timer.unit.clone();
                    let next = timer
                        .next
                        .map(|at| {
                            t!(
                                "tools.timer.next_short",
                                time = timer_time_short(at, timer.next_offset, now)
                            )
                        })
                        .unwrap_or_default();
                    h_flex()
                        .gap_1p5()
                        .child(
                            Button::new(SharedString::from(format!("tools-service-timer-{unit}")))
                                .ghost()
                                .xsmall()
                                .px_0()
                                .label(timer_name(&unit).to_string())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.focus_timer(&unit, window, cx)
                                })),
                        )
                        .child(div().text_color(muted).child(next))
                        .into_any_element()
                })
                .collect();
            rows.push(row(
                t!("tools.service.timers"),
                v_flex().children(links).into_any_element(),
            ));
        }
        if let Some(pid) = service.main_pid {
            // 按钮按自己的宽度放在值那一列的开头，去掉水平内边距：文字与同一列的其他值左对齐（主进程的连接在 ⋯ 中）
            let process_context = self.context.clone();
            rows.push(row(
                t!("tools.service.main_pid"),
                h_flex()
                    .child(
                        Button::new("tools-service-main-pid")
                            .ghost()
                            .xsmall()
                            .px_0()
                            .label(pid.to_string())
                            .on_click(move |_, window, cx| {
                                shared::go(
                                    &process_context,
                                    window,
                                    cx,
                                    move |panel, window, cx| panel.show_process(pid, window, cx),
                                );
                            }),
                    )
                    .into_any_element(),
            ));
        }
        let usage = usage_text(&service);
        if !usage.is_empty() {
            rows.push(row(
                t!("tools.service.usage"),
                div()
                    .id("tools-service-usage")
                    .test_support()
                    .child(usage)
                    .into_any_element(),
            ));
        }
        if let Some(path) = &service.fragment_path {
            rows.push(row(
                t!("tools.service.unit_file"),
                div()
                    .font_family(mono)
                    .child(path.clone())
                    .into_any_element(),
            ));
        }
        // 头部：名称 ● 状态 · 主要操作（运行中的停止、重启，其余的启动；操作进行中时不能点） · ⋯（其余操作、日志、
        // 主进程与连接）
        let locked = self.locked(data);
        let busy = self.ops.busy(&service.unit);
        let actions: Vec<AnyElement> =
            SERVICE_OPS
                .into_iter()
                .filter(|op| PRIMARY_OPS.contains(op) && service_op_applies(&service, *op))
                .filter(|op| *op != ServiceOp::Restart || is_service_running(&service))
                .map(|op| {
                    let target = UnitTarget::service(&service);
                    op_button(
                        SharedString::from(format!("tools-service-detail-{}", op_key(op))),
                        op_icon(op),
                        op_label(op),
                        op == ServiceOp::Stop,
                        locked,
                    )
                    .disabled(busy)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.confirm_op(&target, op, window, cx)
                    }))
                    .into_any_element()
                })
                .collect();
        let more = shared::more_button(
            "tools-service-detail-more",
            self.menu_builder(&service, locked, cx),
            cx,
        );
        let header = shared::detail_header(
            service.unit.clone().into(),
            Some((active_label(&service.active_state), service_tone(&service))),
            actions,
            Some(more),
            cx,
        );
        let mut body: Vec<AnyElement> = failure
            .map(|failure| {
                div()
                    .id("tools-service-failure")
                    .test_support()
                    .pb_1()
                    .font_medium()
                    .text_color(danger)
                    .child(prose(failure))
                    .into_any_element()
            })
            .into_iter()
            .collect();
        body.extend(rows);
        body.extend(self.render_tail(&service.unit, cx));
        Some(shared::detail_pane(
            "tools-service-detail",
            &self.splitter,
            &self.detail_height,
            header,
            body,
            window,
            cx,
        ))
    }

    /// 信息行左侧：系统状态（不是 running 时是一个可点的警告标签，点了看失败的服务）· systemd 的版本（只写
    /// “systemd 257”）。版本是次要的：放不下时整个换到看不见的第二行（这一行只有一行高、超出的不画），不截成
    /// “system…”；状态在前，总看得到。
    fn render_system_info(&self, data: &ServiceList, cx: &mut Context<Self>) -> AnyElement {
        let version = systemd_version(&data.version);
        let state = (!data.system_state.is_empty()).then(|| {
            let state = t!(
                "tools.service.system_state",
                state = data.system_state.clone()
            );
            if data.system_state == "running" {
                div()
                    .id("tools-service-state")
                    .test_support()
                    .flex_none()
                    .whitespace_nowrap()
                    .child(state)
                    .into_any_element()
            } else {
                Button::new("tools-service-degraded")
                    .warning()
                    .outline()
                    .xsmall()
                    .compact()
                    .label(state)
                    .tooltip(t!("tools.service.show_failed"))
                    .on_click(
                        cx.listener(|this, _, _, cx| this.set_filter(ServiceFilter::Failed, cx)),
                    )
                    .into_any_element()
            }
        });
        let has_state = state.is_some();
        let version = (!version.is_empty()).then(|| {
            h_flex()
                .id("tools-service-version")
                .test_support()
                .flex_none()
                .gap_1()
                .whitespace_nowrap()
                .when(has_state, |this| this.child("·"))
                .child(version)
        });
        h_flex()
            .w_full()
            .flex_wrap()
            .max_h(rems(1.25))
            .overflow_hidden()
            .gap_1()
            .children(state)
            .children(version)
            .into_any_element()
    }

    /// 失败的服务最近几行日志（读取中、读不到时说明）。
    fn render_tail(&self, unit: &str, cx: &App) -> Option<AnyElement> {
        let tail = self.tail.as_ref().filter(|tail| tail.unit == unit)?;
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let note = |text: SharedString| div().text_color(muted).child(prose(text));
        let (label, body) = match &tail.result {
            None => (
                t!("tools.service.recent_logs", n = TAIL_LINES),
                note(t!("tools.loading")).into_any_element(),
            ),
            Some(Err(error)) => (
                t!("tools.service.recent_logs", n = TAIL_LINES),
                note(errors::describe(error)).into_any_element(),
            ),
            Some(Ok(logs)) => {
                let label = if logs.sudo {
                    t!("tools.service.recent_logs_sudo", n = TAIL_LINES)
                } else {
                    t!("tools.service.recent_logs", n = TAIL_LINES)
                };
                // 可能因为权限看不全：看不到时代替“没有日志”，看到一部分时写在日志上方
                let limited = |text: SharedString| {
                    div()
                        .id("tools-service-tail-limited")
                        .test_support()
                        .text_color(theme.warning)
                        .child(prose(text))
                };
                let body = match (logs.lines.is_empty(), logs.limited) {
                    (true, true) => limited(t!("tools.logs.limited_none")).into_any_element(),
                    (true, false) => note(t!("tools.logs.empty")).into_any_element(),
                    (false, true) => v_flex()
                        .gap_1()
                        .child(limited(t!("tools.logs.limited_some")))
                        .child(shared::log_lines(&logs.lines, cx))
                        .into_any_element(),
                    (false, false) => shared::log_lines(&logs.lines, cx),
                };
                (label, body)
            }
        };
        Some(
            v_flex()
                .id("tools-service-tail")
                .test_support()
                .pt_1()
                .gap_0p5()
                .child(div().text_color(muted).child(label))
                .child(
                    div()
                        .px_1p5()
                        .py_1()
                        .rounded(theme.radius)
                        .bg(theme.muted.opacity(0.5))
                        .child(body),
                )
                .into_any_element(),
        )
    }
}

/// 详情中的状态：运行状态（子状态与它的说法不同时括注，“运行中（running）”；英文中两者相同时只写一次），加载状态
/// （不是 loaded 时），运行中的已运行多久。
fn state_line(service: &Service, now: i64) -> String {
    let label = active_label(&service.active_state);
    let mut state =
        if service.sub_state.is_empty() || service.sub_state.eq_ignore_ascii_case(&label) {
            label.to_string()
        } else {
            t!(
                "tools.service.state_sub",
                state = label,
                sub = service.sub_state.clone()
            )
            .to_string()
        };
    if service.load_state != "loaded" {
        state.push_str(&format!(" · {}", service.load_state));
    }
    if service.active_state == "active"
        && let Some(since) = service.active_since
    {
        state.push_str(&format!(
            " · {}",
            format_uptime((now - since) as f64 / 1000.)
        ));
    }
    state
}

/// 运行信息：内存、CPU 时间、任务数、重启次数（读不到的不写）。
fn usage_text(service: &Service) -> String {
    let mut parts: Vec<SharedString> = Vec::new();
    if let Some(bytes) = service.memory {
        parts.push(t!("tools.service.memory", size = format_size(bytes)));
    }
    if let Some(nsec) = service.cpu_nsec {
        parts.push(cpu_time_text(nsec));
    }
    match service.tasks {
        Some(1) => parts.push(t!("tools.service.tasks_one")),
        Some(n) => parts.push(t!("tools.service.tasks", n = n)),
        None => {}
    }
    match service.restarts {
        Some(1) => parts.push(t!("tools.service.restarts_one")),
        Some(n) if n > 1 => parts.push(t!("tools.service.restarts", n = n)),
        _ => {}
    }
    parts.join(" · ")
}

/// 累计的 CPU 时间：一分钟以内写秒（一位小数），一小时以内写分秒，再长写时分。
fn cpu_time_text(nsec: u64) -> SharedString {
    let seconds = nsec as f64 / 1e9;
    let whole = nsec / 1_000_000_000;
    if seconds < 0.05 {
        t!("tools.service.cpu_seconds", n = "<0.1")
    } else if seconds < 60. {
        t!("tools.service.cpu_seconds", n = format!("{seconds:.1}"))
    } else if whole < 3600 {
        t!("tools.service.cpu_minutes", m = whole / 60, s = whole % 60)
    } else {
        t!(
            "tools.service.cpu_hours",
            h = whole / 3600,
            m = whole % 3600 / 60
        )
    }
}

impl Render for ServicePage {
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
        let rows = self.shown_rows(&data, cx);
        let muted = cx.theme().muted_foreground;
        // 各筛选的数量不受搜索影响（默认不列出的单元不算）
        let counts = self.counts.get(&data, self.show_all, |data, show_all| {
            ServiceFilter::ALL.map(|f| match f {
                ServiceFilter::Timers => data.timers.len(),
                f => filter_services(&data.services, f, *show_all, "").len(),
            })
        });
        let hidden = self.hidden.get(&data, (), |data, _| {
            data.services
                .iter()
                .filter(|s| service_hidden_by_default(s))
                .count()
        });
        let filter_ix = ServiceFilter::ALL
            .iter()
            .position(|f| *f == self.filter)
            .unwrap_or(0);
        let locked = self.locked(&data);
        let wide = self.wide(window);
        let timers_tab = self.filter == ServiceFilter::Timers;
        let now = termlet_core::time::now_ms();
        let info = match data.zone.as_ref().filter(|_| timers_tab) {
            // 定时器子页：时间按服务器的时区（与本机不同时写出本机的偏移；放不下时截短，悬停看全部）
            Some(zone) => {
                let text = zone_info(zone, now);
                div()
                    .id("tools-timer-zone-info")
                    .test_support()
                    .truncate()
                    .child(text.clone())
                    .tooltip(move |window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(text.clone()).build(window, cx)
                    })
                    .into_any_element()
            }
            None => self.render_system_info(&data, cx),
        };
        let body = if rows.is_empty() {
            shared::empty_text(
                if timers_tab {
                    t!("tools.timer.empty")
                } else {
                    t!("tools.service.empty")
                },
                cx,
            )
        } else {
            let rem = window.rem_size();
            let row_height = rems(self.row_height(window)).to_pixels(rem);
            let header_height = rems(HEADER_HEIGHT).to_pixels(rem);
            let sizes = Rc::new(
                rows.iter()
                    .map(|row| match row {
                        ServiceRow::Header { .. } => size(px(0.), header_height),
                        ServiceRow::Service(_) | ServiceRow::Timer(_) => size(px(0.), row_height),
                    })
                    .collect::<Vec<_>>(),
            );
            let data_for_rows = data.clone();
            v_virtual_list(
                cx.entity(),
                "tools-service-rows",
                sizes,
                move |this, range, window, cx| {
                    range
                        .filter_map(|ix| rows.get(ix).cloned())
                        .map(|row| match row {
                            ServiceRow::Header { custom, count } => {
                                this.render_header_row(custom, count, cx)
                            }
                            ServiceRow::Service(i) => this.render_row(
                                &data_for_rows.services[i],
                                locked,
                                wide,
                                window,
                                cx,
                            ),
                            ServiceRow::Timer(i) => this.render_timer_row(
                                &data_for_rows.timers[i],
                                &data_for_rows,
                                locked,
                                wide,
                                now,
                                window,
                                cx,
                            ),
                        })
                        .collect::<Vec<_>>()
                },
            )
            .track_scroll(&self.scroll)
            .size_full()
            .into_any_element()
        };
        // 默认不列出的单元：工具条上的开关，藏着时一个可清除的小标签写着藏了几个
        let show_all = self.show_all;
        let show_toggle = Button::new("tools-service-show-all")
            .ghost()
            .small()
            .icon(if show_all {
                IconName::Eye
            } else {
                IconName::EyeOff
            })
            .selected(show_all)
            .tooltip(if show_all {
                t!("tools.service.hide_rare")
            } else {
                t!("tools.service.show_all")
            })
            .on_click(cx.listener(move |this, _, _, cx| this.set_show_all(!show_all, cx)))
            .into_any_element();
        // 定时器子页没有默认不列出的单元：不显示这个开关与小标签
        let show_toggle = (!timers_tab).then_some(show_toggle);
        let chips = (!timers_tab && !show_all && hidden > 0).then(|| {
            shared::filter_chip(
                "tools-service-chip-hidden",
                if hidden == 1 {
                    t!("tools.service.chip_hidden_one")
                } else {
                    t!("tools.service.chip_hidden", count = hidden)
                },
                cx.listener(|this, _, _, cx| this.set_show_all(true, cx)),
                cx,
            )
        });
        let measured = self.width.clone();
        let entity = cx.entity().downgrade();
        let threshold = rems(WIDE_FROM).to_pixels(window.rem_size());
        let marker = shared::privilege_marker(
            RootScope::Services,
            self.feed.privilege.filter(|_| data.uid != Some(0)),
            t!("tools.privilege.sudo"),
            &self.context,
            cx,
        );
        let loading = self.feed.loading;
        let this = cx.entity().downgrade();
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
                "tools-service",
                &self.search,
                show_toggle,
                loading,
                move |_, _, cx| {
                    let _ = this.update(cx, |page, cx| shared::refresh(page, false, cx));
                },
            ))
            .child(shared::filter_tabs(
                "tools-service-filter",
                &self.tabs_scroll,
                filter_ix,
                ServiceFilter::ALL
                    .into_iter()
                    .zip(counts)
                    .map(|(filter, count)| {
                        Tab::new()
                            .label(filter_label(filter))
                            .suffix(div().text_color(muted).child(count.to_string()))
                    })
                    .collect(),
                cx.listener(|this, ix: &usize, _, cx| {
                    if let Some(filter) = ServiceFilter::ALL.get(*ix) {
                        this.set_filter(*filter, cx);
                    }
                }),
            ))
            .children(shared::chip_row(chips.into_iter().collect()))
            .child(info_line(info, marker, self.feed.shown.at, cx))
            .children(error_line(self.feed.error.as_ref(), cx))
            .children(shared::failure_banner(
                &self.ops,
                |page: &mut Self| &mut page.ops,
                cx,
            ))
            .children(note)
            .child(
                shared::list_card(div(), false, cx)
                    .id("tools-service-list")
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
                    .flex_1()
                    .min_h(rems(shared::LIST_MIN))
                    .child(
                        // 宽度跨过每个服务一行的界限时重新渲染
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
                    .child(body)
                    .vertical_scrollbar(&self.scroll),
            )
            .children(self.render_detail(&data, window, cx))
            .into_any_element()
    }
}

#[cfg(test)]
impl ServicePage {
    pub(super) fn selected_unit(&self) -> Option<String> {
        self.selected.clone()
    }

    pub(super) fn note(&self) -> Option<SharedString> {
        self.note.clone()
    }

    /// 直接放进信息行下失败的原因。
    pub(super) fn set_test_failure(
        &mut self,
        action: &str,
        reason: &str,
        detail: &str,
        cx: &mut Context<Self>,
    ) {
        self.ops.set_failure(shared::OpFailure::new(
            action.to_string().into(),
            reason.to_string().into(),
            Some(detail.to_string()),
        ));
        cx.notify();
    }

    /// 直接放进选中的服务的最近日志。
    pub(super) fn set_test_tail(&mut self, logs: LogResult, cx: &mut Context<Self>) {
        self._tail = None;
        self.tail = self.selected.clone().map(|unit| Tail {
            unit,
            result: Some(Ok(logs)),
        });
        cx.notify();
    }
}

fn filter_label(filter: ServiceFilter) -> SharedString {
    match filter {
        ServiceFilter::All => t!("tools.service.filter_all"),
        ServiceFilter::Running => t!("tools.service.filter_running"),
        ServiceFilter::Stopped => t!("tools.service.filter_stopped"),
        ServiceFilter::Failed => t!("tools.service.filter_failed"),
        ServiceFilter::Timers => t!("tools.service.filter_timers"),
    }
}

/// 行上进行中、刚完成时的说法。
fn op_verb(op: ServiceOp) -> OpVerb {
    match op {
        ServiceOp::Start => OpVerb::Start,
        ServiceOp::Stop => OpVerb::Stop,
        ServiceOp::Restart => OpVerb::Restart,
        ServiceOp::Reload => OpVerb::Reload,
        ServiceOp::Enable => OpVerb::Enable,
        ServiceOp::Disable => OpVerb::Disable,
    }
}

/// 服务在显示的行中的位置（跳过分组的标题）。
fn position(rows: &[ServiceRow], data: &ServiceList, unit: &str) -> Option<usize> {
    rows.iter().position(|row| match row {
        ServiceRow::Service(ix) => data.services[*ix].unit == unit,
        ServiceRow::Timer(ix) => data.timers[*ix].unit == unit,
        ServiceRow::Header { .. } => false,
    })
}

/// 元素 ID 用的操作名。
fn op_key(op: ServiceOp) -> &'static str {
    match op {
        ServiceOp::Start => "start",
        ServiceOp::Stop => "stop",
        ServiceOp::Restart => "restart",
        ServiceOp::Reload => "reload",
        ServiceOp::Enable => "enable",
        ServiceOp::Disable => "disable",
    }
}

fn op_icon(op: ServiceOp) -> IconName {
    match op {
        ServiceOp::Start => IconName::Play,
        ServiceOp::Stop => IconName::Square,
        ServiceOp::Restart => IconName::RotateCcw,
        ServiceOp::Reload => IconName::RefreshCw,
        ServiceOp::Enable => IconName::Power,
        ServiceOp::Disable => IconName::PowerOff,
    }
}

fn op_label(op: ServiceOp) -> SharedString {
    match op {
        ServiceOp::Start => t!("tools.service.op_start"),
        ServiceOp::Stop => t!("tools.service.op_stop"),
        ServiceOp::Restart => t!("tools.service.op_restart"),
        ServiceOp::Reload => t!("tools.service.op_reload"),
        ServiceOp::Enable => t!("tools.service.op_enable"),
        ServiceOp::Disable => t!("tools.service.op_disable"),
    }
}

/// 操作的说明（失败的提示、需要 root 时的说明）。
fn op_action(op: ServiceOp, name: &str) -> SharedString {
    match op {
        ServiceOp::Start => t!("tools.service.action_start", name = name),
        ServiceOp::Stop => t!("tools.service.action_stop", name = name),
        ServiceOp::Restart => t!("tools.service.action_restart", name = name),
        ServiceOp::Reload => t!("tools.service.action_reload", name = name),
        ServiceOp::Enable => t!("tools.service.action_enable", name = name),
        ServiceOp::Disable => t!("tools.service.action_disable", name = name),
    }
}

fn confirm_title(op: ServiceOp, name: &str) -> SharedString {
    match op {
        ServiceOp::Start => t!("tools.service.confirm_start", name = name),
        ServiceOp::Stop => t!("tools.service.confirm_stop", name = name),
        ServiceOp::Restart => t!("tools.service.confirm_restart", name = name),
        ServiceOp::Reload => t!("tools.service.confirm_reload", name = name),
        ServiceOp::Enable => t!("tools.service.confirm_enable", name = name),
        ServiceOp::Disable => t!("tools.service.confirm_disable", name = name),
    }
}

fn op_hint(op: ServiceOp) -> SharedString {
    match op {
        ServiceOp::Start => t!("tools.service.hint_start"),
        ServiceOp::Stop => t!("tools.service.hint_stop"),
        ServiceOp::Restart => t!("tools.service.hint_restart"),
        ServiceOp::Reload => t!("tools.service.hint_reload"),
        ServiceOp::Enable => t!("tools.service.hint_enable"),
        ServiceOp::Disable => t!("tools.service.hint_disable"),
    }
}

/// 定时器操作的确认框标题。
fn timer_confirm_title(op: ServiceOp, name: &str) -> SharedString {
    match op {
        ServiceOp::Start => t!("tools.timer.confirm_start", name = name),
        ServiceOp::Stop => t!("tools.timer.confirm_stop", name = name),
        ServiceOp::Enable => t!("tools.timer.confirm_enable", name = name),
        ServiceOp::Disable => t!("tools.timer.confirm_disable", name = name),
        ServiceOp::Restart | ServiceOp::Reload => confirm_title(op, name),
    }
}

/// 定时器操作的后果。
fn timer_hint(op: ServiceOp) -> SharedString {
    match op {
        ServiceOp::Start => t!("tools.timer.hint_start"),
        ServiceOp::Stop => t!("tools.timer.hint_stop"),
        ServiceOp::Enable => t!("tools.timer.hint_enable"),
        ServiceOp::Disable => t!("tools.timer.hint_disable"),
        ServiceOp::Restart | ServiceOp::Reload => op_hint(op),
    }
}

/// 定时器操作的说明（失败的提示、需要 root 时的说明、审计）。
fn timer_action(op: ServiceOp, name: &str) -> SharedString {
    match op {
        ServiceOp::Start => t!("tools.timer.action_start", name = name),
        ServiceOp::Stop => t!("tools.timer.action_stop", name = name),
        ServiceOp::Enable => t!("tools.timer.action_enable", name = name),
        ServiceOp::Disable => t!("tools.timer.action_disable", name = name),
        ServiceOp::Restart | ServiceOp::Reload => op_action(op, name),
    }
}

/// 定时器状态的颜色：在计时（等待下次触发、触发中）绿，失败红，已停止与不会再触发的一次性定时器灰。
fn timer_tone(timer: &Timer) -> Tone {
    match (timer.active_state.as_str(), timer.sub_state.as_str()) {
        ("failed", _) => Tone::Danger,
        ("active", "elapsed") => Tone::Muted,
        ("active", _) => Tone::Good,
        _ => Tone::Muted,
    }
}

/// 定时器的状态：等待中、触发中、已触发（一次性的，不会再触发）、已停止、失败；不认识的原样显示。
fn timer_state_label(timer: &Timer) -> SharedString {
    match (timer.active_state.as_str(), timer.sub_state.as_str()) {
        ("active", "running") => t!("tools.timer.state_running"),
        ("active", "elapsed") => t!("tools.timer.state_elapsed"),
        ("active", _) => t!("tools.timer.state_waiting"),
        ("inactive", _) => t!("tools.service.active_inactive"),
        ("failed", _) => t!("tools.service.active_failed"),
        (other, _) => other.to_string().into(),
    }
}

/// 行上的计划：“下次 03:06（22 小时后）· 上次 3 小时前”；时间是服务器那时的时间，与本机不同时带上服务器的偏移
/// （“下次 03:06 UTC+8”）；不会再触发、没有触发过的照写。
fn schedule_text(timer: &Timer, now: i64) -> String {
    let next = match timer.next {
        Some(at) => t!(
            "tools.timer.row_next",
            time = timer_time_short(at, timer.next_offset, now),
            rel = until(at - now)
        ),
        None => t!("tools.timer.no_next"),
    };
    let last = match timer.last {
        Some(at) => t!("tools.timer.row_last", rel = ago(now - at)),
        None => t!("tools.timer.never"),
    };
    format!("{next} · {last}")
}

/// 行上计划的悬停说明：下次、上次触发的完整时间（服务器的时间，与本机不同时括注本机的时间）。
fn schedule_tooltip(timer: &Timer) -> String {
    let full = |at: i64, offset: Option<i32>| {
        let (time, local) = timer_time_full(at, offset);
        match local {
            Some(local) => t!("tools.timer.with_local", time = time, local = local).to_string(),
            None => time,
        }
    };
    let next = match timer.next {
        Some(at) => t!("tools.timer.tip_next", time = full(at, timer.next_offset)),
        None => t!("tools.timer.no_next"),
    };
    let last = match timer.last {
        Some(at) => t!("tools.timer.tip_last", time = full(at, timer.last_offset)),
        None => t!("tools.timer.never"),
    };
    format!("{next}\n{last}")
}

/// 定时器的一个时刻在行上的写法：服务器那时的时间（服务器的今天只写时分），与本机不同时带上服务器的偏移
/// （`03:00 UTC+8`）；不知道服务器的偏移时按本机的时间。
fn timer_time_short(at: i64, offset: Option<i32>, now: i64) -> String {
    let clock = TimerClock::at(at, offset);
    let time = event_time_at(at, now, crate::sftp::file_list::date_style(), clock.offset);
    match clock.offset.filter(|_| clock.differs) {
        Some(offset) => format!("{time} {}", utc_offset_label(offset)),
        None => time,
    }
}

/// 定时器的一个时刻的完整写法（详情、悬停）：服务器那时的时间（到秒），与本机不同时带上服务器的偏移，并给出本机的
/// 时间；不知道服务器的偏移时按本机的时间。
fn timer_time_full(at: i64, offset: Option<i32>) -> (String, Option<String>) {
    use termlet_core::time::{format_seconds, format_seconds_at};
    let style = crate::sftp::file_list::date_style();
    let clock = TimerClock::at(at, offset);
    match clock.offset {
        Some(offset) if clock.differs => (
            format!(
                "{} {}",
                format_seconds_at(at, style, offset),
                utc_offset_label(offset)
            ),
            Some(format_seconds(at, style)),
        ),
        Some(offset) => (format_seconds_at(at, style, offset), None),
        None => (format_seconds(at, style), None),
    }
}

/// 服务器的时区：“Asia/Shanghai（UTC+8）”；没有名称时只写偏移。
fn zone_text(zone: &ServerZone) -> SharedString {
    let offset = utc_offset_label(zone.offset);
    match zone_name(zone) {
        "" => offset.into(),
        name => t!("tools.timer.zone_value", name = name, offset = offset),
    }
}

/// 详情中的服务器时区：与本机现在的偏移不同时另写一行。
fn zone_detail(zone: &ServerZone, now: i64, cx: &App) -> AnyElement {
    let local = termlet_core::time::local_offset(now).filter(|local| *local != zone.offset);
    v_flex()
        .id("tools-timer-zone")
        .test_support()
        .child(zone_text(zone))
        .children(local.map(|local| {
            div()
                .id("tools-timer-zone-differs")
                .test_support()
                .text_color(cx.theme().warning)
                .child(prose(t!(
                    "tools.timer.zone_differs",
                    local = utc_offset_label(local)
                )))
        }))
        .into_any_element()
}

/// 定时器子页信息行上的说明：时间按服务器的时区（与本机不同时写出本机的偏移）。
fn zone_info(zone: &ServerZone, now: i64) -> SharedString {
    match termlet_core::time::local_offset(now).filter(|local| *local != zone.offset) {
        Some(local) => t!(
            "tools.timer.zone_info_differs",
            zone = zone_text(zone),
            local = utc_offset_label(local)
        ),
        None => t!("tools.timer.zone_info", zone = zone_text(zone)),
    }
}

/// 定时器触发的服务失败着（行上标出）。
fn triggered_failed(timer: &Timer, data: &ServiceList) -> bool {
    !timer.activates.is_empty()
        && data
            .services
            .iter()
            .any(|s| s.unit == timer.activates && is_service_failed(s))
}

/// 多久之后（“22 小时后”）：取最大的单位；已经到了（还没有刷新）时为“马上”。
fn until(ms: i64) -> SharedString {
    let seconds = ms / 1000;
    if seconds <= 0 {
        t!("tools.timer.in_now")
    } else if seconds < 60 {
        t!("tools.timer.in_seconds", n = seconds)
    } else if seconds < 3600 {
        t!("tools.timer.in_minutes", n = seconds / 60)
    } else if seconds < 86_400 {
        t!("tools.timer.in_hours", n = seconds / 3600)
    } else {
        t!("tools.timer.in_days", n = seconds / 86_400)
    }
}

/// 服务状态的颜色：运行中绿，正在启动、停止黄，失败红，已停止灰。
fn service_tone(service: &Service) -> Tone {
    match service.active_state.as_str() {
        "failed" => Tone::Danger,
        "activating" | "deactivating" => Tone::Warning,
        _ if is_service_running(service) => Tone::Good,
        _ => Tone::Muted,
    }
}

/// 运行状态的说明；不认识的原样显示。
fn active_label(state: &str) -> SharedString {
    match state {
        "active" => t!("tools.service.active_active"),
        "inactive" => t!("tools.service.active_inactive"),
        "failed" => t!("tools.service.active_failed"),
        "activating" => t!("tools.service.active_activating"),
        "deactivating" => t!("tools.service.active_deactivating"),
        "reloading" => t!("tools.service.active_reloading"),
        other => other.to_string().into(),
    }
}

/// 开机启动的说明（`systemctl is-enabled` 的全部状态，`-runtime` 的只到下次开机为止）；不认识的原样显示。
fn boot_label(state: &str) -> SharedString {
    match state {
        "enabled" => t!("tools.service.boot_enabled"),
        "enabled-runtime" => t!("tools.service.boot_enabled_runtime"),
        "disabled" => t!("tools.service.boot_disabled"),
        "static" => t!("tools.service.boot_static"),
        "masked" => t!("tools.service.boot_masked"),
        "masked-runtime" => t!("tools.service.boot_masked_runtime"),
        "generated" => t!("tools.service.boot_generated"),
        "indirect" => t!("tools.service.boot_indirect"),
        "alias" => t!("tools.service.boot_alias"),
        "transient" => t!("tools.service.boot_transient"),
        "linked" => t!("tools.service.boot_linked"),
        "linked-runtime" => t!("tools.service.boot_linked_runtime"),
        "bad" => t!("tools.service.boot_bad"),
        "not-found" => t!("tools.service.boot_not_found"),
        other => other.to_string().into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `systemctl is-enabled` 的全部状态都有说明（不原样显示 enabled-runtime 等）。
    #[test]
    fn every_boot_state_has_a_label() {
        for state in [
            "enabled",
            "enabled-runtime",
            "linked",
            "linked-runtime",
            "alias",
            "masked",
            "masked-runtime",
            "static",
            "indirect",
            "disabled",
            "generated",
            "transient",
            "bad",
            "not-found",
        ] {
            let label = boot_label(state);
            assert_ne!(label.as_ref(), state, "{state} 应有说明");
            assert!(!label.starts_with("tools."), "{state} 的文案缺失：{label}");
        }
        assert_eq!(boot_label("future-state").as_ref(), "future-state");
    }

    /// 状态的颜色按各页共用的颜色表：运行中绿，失败红，正在启动、停止黄，已停止灰。
    #[test]
    fn service_states_use_the_shared_tones() {
        let service = |active: &str| Service {
            active_state: active.into(),
            ..Default::default()
        };
        assert_eq!(service_tone(&service("active")), Tone::Good);
        assert_eq!(service_tone(&service("reloading")), Tone::Good);
        assert_eq!(service_tone(&service("failed")), Tone::Danger);
        assert_eq!(service_tone(&service("activating")), Tone::Warning);
        assert_eq!(service_tone(&service("inactive")), Tone::Muted);
    }

    /// 运行信息：读得到的才写；CPU 时间按长短换单位。
    #[test]
    fn usage_lists_what_is_known() {
        let service = Service {
            memory: Some(12 << 20),
            cpu_nsec: Some(3_200_000_000),
            tasks: Some(4),
            restarts: Some(5),
            ..Default::default()
        };
        let text = usage_text(&service);
        for part in ["12", "3.2", "4", "5"] {
            assert!(text.contains(part), "{text}");
        }
        assert_eq!(text.matches(" · ").count(), 3, "{text}");
        assert_eq!(usage_text(&Service::default()), "");
        assert!(cpu_time_text(125_000_000_000).contains("2"), "两分多钟");
        assert!(cpu_time_text(7_500_000_000_000).contains("2"), "两小时多");
    }

    /// 详情的状态：子状态与运行状态的说法相同时只写一次（英文的 “running (running)”），不同时括注；三种语言。
    #[test]
    fn state_line_says_the_sub_state_once() {
        use crate::i18n::{Language, set_language};
        let service = |active: &str, sub: &str| Service {
            active_state: active.into(),
            sub_state: sub.into(),
            load_state: "loaded".into(),
            ..Default::default()
        };
        set_language(Language::En);
        assert_eq!(state_line(&service("active", "running"), 0), "running");
        assert_eq!(
            state_line(&service("active", "exited"), 0),
            "running (exited)"
        );
        assert_eq!(state_line(&service("failed", "failed"), 0), "failed");
        set_language(Language::ZhCn);
        assert_eq!(
            state_line(&service("active", "running"), 0),
            "运行中（running）"
        );
        set_language(Language::ZhTw);
        assert_eq!(
            state_line(&service("inactive", "dead"), 0),
            "已停止（dead）"
        );
        set_language(Language::ZhCn);
    }

    /// 定时器的时间按服务器那时的时间：与本机不同时带上服务器的偏移，悬停另写本机的时间；相同时只写时间。
    #[test]
    fn timer_times_use_the_server_clock() {
        use crate::i18n::{Language, set_language};
        set_language(Language::ZhCn);
        let now = termlet_core::time::now_ms();
        let local = termlet_core::time::local_offset(now).unwrap();
        let server = if local > 0 {
            local - 5 * 3600
        } else {
            local + 5 * 3600
        };
        let at = now + 3_600_000;
        let timer = |offset: Option<i32>| Timer {
            unit: "backup.timer".into(),
            next: Some(at),
            next_offset: offset,
            last: Some(now - 60_000),
            last_offset: offset,
            ..Default::default()
        };
        let label = utc_offset_label(server);
        let differs = schedule_text(&timer(Some(server)), now);
        assert!(differs.contains(&label), "{differs}");
        assert!(
            differs.contains(&termlet_core::time::clock_at(at, server)),
            "服务器的时分：{differs}"
        );
        let tip = schedule_tooltip(&timer(Some(server)));
        assert!(tip.contains(&label) && tip.contains("本机"), "{tip}");
        assert_eq!(tip.lines().count(), 2, "下次、上次各一行");
        let same = schedule_text(&timer(Some(local)), now);
        assert!(!same.contains("UTC"), "与本机相同时不写偏移：{same}");
        assert!(!schedule_tooltip(&timer(Some(local))).contains("本机"));
        let unknown = schedule_text(&timer(None), now);
        assert!(
            !unknown.contains("UTC"),
            "不知道服务器的时区时按本机：{unknown}"
        );
        let (full, local_time) = timer_time_full(at, Some(server));
        assert!(full.ends_with(&label) && local_time.is_some());
    }

    /// 服务器时区的写法：有名称时“名称（偏移）”，没有时只写偏移。
    #[test]
    fn zone_text_names_the_zone() {
        use crate::i18n::{Language, set_language};
        set_language(Language::ZhCn);
        let zone = |name: &str, abbr: &str| ServerZone {
            name: name.into(),
            abbr: abbr.into(),
            offset: 8 * 3600,
        };
        assert_eq!(
            zone_text(&zone("Asia/Shanghai", "CST")).as_ref(),
            "Asia/Shanghai（UTC+8）"
        );
        assert_eq!(zone_text(&zone("", "CST")).as_ref(), "CST（UTC+8）");
        assert_eq!(zone_text(&zone("", "+08")).as_ref(), "UTC+8");
    }

    /// 英文的数量：1 与其他数量分开写（“1 task”“Restarted once”）。
    #[test]
    fn english_counts_use_the_singular_for_one() {
        use crate::i18n::{Language, set_language};
        set_language(Language::En);
        let usage = |tasks, restarts| {
            usage_text(&Service {
                tasks: Some(tasks),
                restarts: Some(restarts),
                ..Default::default()
            })
        };
        assert_eq!(usage(1, 1), "1 task · Restarted once");
        assert_eq!(usage(3, 2), "3 tasks · Restarted 2 times");
        set_language(Language::ZhCn);
    }
}
