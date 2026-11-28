//! 各页共用的部分：页所跟随的终端、带缓存与暂停的读取、不可用时的说明、可复制的命令、“更新于”、工具条、卡片（概览的
//! 各节，列表页的列表与详情：[`card`]、[`list_card`]、[`detail_pane`]），以及跳到别的页（[`go`]）、筛选条件的
//! 小标签、跳过来的目标不在时的说明、列表的焦点框与键盘打开的右键菜单（[`KeyMenu`]），
//! 以及行右端浮出的操作（[`row_actions`]）、行上进行中、刚完成、失败的写操作（[`RowOps`]）与信息行下失败的原因
//! （[`failure_banner`]），没有免密码 sudo 时确认框以“在终端中执行”为主按钮（[`root_first_body`]、[`root_first_footer`]）。
//!
//! 读取（[`refresh`]）：页显示时（[`activate`]）读一次；进程页另有定时刷新（[`start_poll`]，上一次返回后再计时，
//! 请求不会叠加）。页不再显示时作废进行中的读取（结果不再显示，命令照常在服务器上执行完）。结果先记进缓存，
//! 暂停期间（进程页：鼠标停在列表上、菜单或确认框打开）读到的先存着，恢复后再显示；自己操作之后的重读不受暂停影响。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogClose, DialogFooter};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, StyledExt as _, WindowExt as _, h_flex,
    v_flex,
};
use gpui_kit::{
    AnyElement, AnyView, App, AppContext as _, ClickEvent, ClipboardItem, Context, DismissEvent,
    DragMoveEvent, ElementId, Entity, EntityId, FocusHandle, Focusable as _, Hsla,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, ParentElement as _, Pixels,
    Render, ScrollHandle, SharedString, StatefulInteractiveElement as _, Styled, Subscription,
    Task, TestSupportExt as _, WeakEntity, Window, anchored, canvas, deferred, div,
    prelude::FluentBuilder as _, px, relative, rems,
};
use termlet_core::hosttools::{
    Action, ConnList, DockerInfo, ExecError, HostTools, LogLevel, LogLine, PROCESS_INTERVAL,
    Privilege, ProcessList, ServiceList, SudoState, Target, TerminalRequest, ToolsError,
    terminal_command,
};
use termlet_core::id::HostId;

use super::errors;
use super::keys::{LIST_CONTEXT, SEARCH_CONTEXT};
use super::log_model::tail_rows;
use super::model::{Lru, OverviewData, Page, Shown, clock_time, short_id};
use super::panel::HostToolsPanel;
use super::target::HostLabel;
use crate::dialog_token::DialogToken;
use crate::i18n::LocalizedPlaceholder as _;
use crate::prose::prose;
use crate::scrolling::ScrollGutter as _;
use crate::t;
use crate::terminal_panel::TerminalPanel;

/// 缓存的条数上限（每个终端每页一条）。
const CACHE_MAX: usize = 200;

/// 每个终端各页上次的结果。
pub(super) type ToolCache = Lru<(EntityId, Page), Cached>;

pub(super) fn new_cache() -> Rc<RefCell<ToolCache>> {
    Rc::new(RefCell::new(Lru::new(CACHE_MAX)))
}

/// 缓存中的一页。
#[derive(Clone)]
pub(super) enum Cached {
    Overview(Rc<OverviewData>),
    Processes(Rc<ProcessList>),
    Services(Rc<ServiceList>),
    Docker(Rc<DockerInfo>),
    Network(Rc<ConnList>),
}

/// 一页的数据：属于哪一页、怎样读取、怎样放进缓存。
pub(super) trait ToolData: Sized + 'static {
    const PAGE: Page;
    /// 这一页要标出这条连接上的权限（[`Feed::privilege`]：是 root、有免密码 sudo、都没有）
    const PRIVILEGE: bool = false;
    fn wrap(data: Rc<Self>) -> Cached;
    fn unwrap(cached: Cached) -> Option<Rc<Self>>;
    fn load(tools: HostTools, target: Target) -> impl Future<Output = Result<Self, ToolsError>>;
}

impl ToolData for OverviewData {
    const PAGE: Page = Page::Overview;
    fn wrap(data: Rc<Self>) -> Cached {
        Cached::Overview(data)
    }
    fn unwrap(cached: Cached) -> Option<Rc<Self>> {
        match cached {
            Cached::Overview(data) => Some(data),
            _ => None,
        }
    }
    /// 补充信息读不到时照常显示其余的（BusyBox、老系统缺的命令不算错误）；进程列表与进程页是同一个读取。
    async fn load(tools: HostTools, target: Target) -> Result<Self, ToolsError> {
        let details = termlet_core::monitor::details(&target).await.ok();
        let list = tools.processes(&target, super::overview::INTERVAL).await?;
        Ok(OverviewData::new(details, &list))
    }
}

impl ToolData for ProcessList {
    const PAGE: Page = Page::Processes;
    const PRIVILEGE: bool = true;
    fn wrap(data: Rc<Self>) -> Cached {
        Cached::Processes(data)
    }
    fn unwrap(cached: Cached) -> Option<Rc<Self>> {
        match cached {
            Cached::Processes(data) => Some(data),
            _ => None,
        }
    }
    async fn load(tools: HostTools, target: Target) -> Result<Self, ToolsError> {
        tools.processes(&target, PROCESS_INTERVAL).await
    }
}

impl ToolData for ServiceList {
    const PAGE: Page = Page::Services;
    const PRIVILEGE: bool = true;
    fn wrap(data: Rc<Self>) -> Cached {
        Cached::Services(data)
    }
    fn unwrap(cached: Cached) -> Option<Rc<Self>> {
        match cached {
            Cached::Services(data) => Some(data),
            _ => None,
        }
    }
    async fn load(tools: HostTools, target: Target) -> Result<Self, ToolsError> {
        tools.services(&target).await
    }
}

impl ToolData for DockerInfo {
    const PAGE: Page = Page::Docker;
    fn wrap(data: Rc<Self>) -> Cached {
        Cached::Docker(data)
    }
    fn unwrap(cached: Cached) -> Option<Rc<Self>> {
        match cached {
            Cached::Docker(data) => Some(data),
            _ => None,
        }
    }
    async fn load(tools: HostTools, target: Target) -> Result<Self, ToolsError> {
        tools.docker(&target).await
    }
}

impl ToolData for ConnList {
    const PAGE: Page = Page::Network;
    const PRIVILEGE: bool = true;
    fn wrap(data: Rc<Self>) -> Cached {
        Cached::Network(data)
    }
    fn unwrap(cached: Cached) -> Option<Rc<Self>> {
        match cached {
            Cached::Network(data) => Some(data),
            _ => None,
        }
    }
    async fn load(tools: HostTools, target: Target) -> Result<Self, ToolsError> {
        tools.connections(&target).await
    }
}

/// 面板打开过的对话框（确认、需要 root、日志）：换了终端时关掉还开着的，免得在别的终端上执行。
/// 记的是对话框的令牌，只关它们自己（上面叠着别的对话框——例如命令行工具的批准框——时不关）。
#[derive(Clone, Default)]
pub(super) struct OpenDialogs(Rc<RefCell<Vec<DialogToken>>>);

impl OpenDialogs {
    /// 在 `open_dialog` / `open_alert_dialog` 之后立即调用。
    pub fn opened(&self, window: &Window, cx: &App) {
        if let Some(token) = DialogToken::take(window, cx) {
            self.0.borrow_mut().push(token);
        }
    }

    /// 取出全部（关掉它们之前）；先打开的在前。已经关掉的也在其中，关闭时会跳过。
    pub fn take(&self) -> Vec<DialogToken> {
        std::mem::take(&mut *self.0.borrow_mut())
    }
}

/// 显示后第一次读到的进程没有 CPU（没有上一次读数，或上一次读数太旧、不拿来算，见 `HostTools::processes`）时，
/// 隔多久补读一次（进程页、概览页相同）。
pub(super) const CPU_RETRY: Duration = Duration::from_millis(1500);

/// 页所跟随的终端（换了终端时各页重建，这里不会变；只有连接随终端的状态更新）。
#[derive(Clone)]
pub(super) struct PageContext {
    pub panel: WeakEntity<HostToolsPanel>,
    pub terminal: WeakEntity<TerminalPanel>,
    pub terminal_id: EntityId,
    pub tools: HostTools,
    /// 当前的连接；没有连上（重连中等）时为 `None`
    pub target: Option<Target>,
    /// 保存的 SSH 主机（“转发到本机…”只对它提供）
    pub ssh_host: Option<HostId>,
    pub cache: Rc<RefCell<ToolCache>>,
    pub dialogs: OpenDialogs,
}

impl PageContext {
    /// 页所跟随的终端的主机（写操作的确认框写明）；面板已经换了终端时为 `None`。
    pub fn host(&self, cx: &App) -> Option<HostLabel> {
        self.panel
            .upgrade()
            .and_then(|panel| panel.read(cx).host_of(self.terminal_id))
    }
}

/// 一页的读取状态。
pub(super) struct Feed<T> {
    pub shown: Shown<T>,
    /// 上一次读取的错误（还没有数据时整页说明，有数据时在列表上方显示一行）
    pub error: Option<ToolsError>,
    pub loading: bool,
    /// 页正在显示：面板展开、是当前页、终端连接着、窗口可见
    pub active: bool,
    /// 这条连接上的权限（[`ToolData::PRIVILEGE`] 的页随读取一并取得：核心每条连接只探测一次）；还不知道时为 `None`
    pub privilege: Option<Privilege>,
    /// 每次读取加一：作废进行中的读取
    seq: u64,
    /// 自己操作之后要重读：之后读到的结果直接显示，不受暂停影响（这次重读被后来的读取——补读 CPU 等——顶掉时，
    /// 顶掉它的那次照样直接显示）
    forced: bool,
    task: Option<Task<()>>,
    poll: Option<Task<()>>,
}

impl<T: ToolData> Feed<T> {
    /// 先显示这个终端这一页的缓存。
    pub fn new(context: &PageContext) -> Self {
        let cached = context
            .cache
            .borrow()
            .get(&(context.terminal_id, T::PAGE))
            .and_then(T::unwrap);
        Self {
            shown: Shown::new(cached),
            error: None,
            loading: false,
            active: false,
            privilege: None,
            seq: 0,
            forced: false,
            task: None,
            poll: None,
        }
    }

    pub fn data(&self) -> Option<Rc<T>> {
        self.shown.data.clone()
    }

    pub fn is_polling(&self) -> bool {
        self.poll.is_some()
    }
}

/// 一页：读取用的状态与上下文。
pub(super) trait ToolPage: Sized + 'static {
    type Data: ToolData;
    fn feed(&mut self) -> &mut Feed<Self::Data>;
    fn context(&self) -> &PageContext;
    /// 暂停期间读到的结果先存着（进程页：鼠标停在列表上、菜单或确认框打开）
    fn is_paused(&self) -> bool {
        false
    }
    /// 一次读取结束（结果已显示或暂存，或者失败）
    fn after_load(&mut self, _: &mut Context<Self>) {}
}

/// 一次读取的结果与这条连接上的权限（不需要、读不到时为 `None`）。
type Loaded<T> = (Result<T, ToolsError>, Option<Privilege>);

/// 开始一次读取：没有连接时为 `None`。要标出权限的页同时取得权限（核心按连接记下，第一次之后直接返回）。
fn begin_load<P: ToolPage>(
    page: &mut P,
) -> Option<(u64, impl Future<Output = Loaded<P::Data>> + use<P>)> {
    let context = page.context();
    let target = context.target.clone()?;
    let tools = context.tools.clone();
    // 核心的读取在第一次轮询之前把整个脚本的执行带在自己身上，调试构建中很大：放到堆上，读取之前在主线程的栈上
    // 来回传的只是指针（两个连在一起直接传时曾把主线程的栈撑爆）
    let data = Box::pin(P::Data::load(tools.clone(), target.clone()));
    let privilege = Box::pin(async move {
        if P::Data::PRIVILEGE {
            tools.privilege(&target).await.ok()
        } else {
            None
        }
    });
    let future = futures::future::join(data, privilege);
    let feed = page.feed();
    feed.seq += 1;
    feed.loading = true;
    Some((feed.seq, future))
}

fn finish_load<P: ToolPage>(
    page: &mut P,
    seq: u64,
    (result, privilege): Loaded<P::Data>,
    force: bool,
    cx: &mut Context<P>,
) {
    if page.feed().seq != seq {
        return;
    }
    let paused = page.is_paused();
    let (cache, key) = {
        let context = page.context();
        (context.cache.clone(), (context.terminal_id, P::Data::PAGE))
    };
    let feed = page.feed();
    feed.loading = false;
    if privilege.is_some() {
        feed.privilege = privilege;
    }
    match result {
        Ok(value) => {
            tracing::trace!(page = ?P::Data::PAGE, "主机工具：读取完成");
            let value = Rc::new(value);
            cache.borrow_mut().put(key, P::Data::wrap(value.clone()));
            feed.error = None;
            let force = force || std::mem::take(&mut feed.forced);
            feed.shown
                .arrive(value, termlet_core::time::now_ms(), paused, force);
        }
        Err(error) => {
            // 定时刷新会反复遇到同一个错误：变了才记
            if feed.error.as_ref().map(ToString::to_string) != Some(error.to_string()) {
                tracing::debug!(page = ?P::Data::PAGE, "主机工具：读取失败：{error}");
            }
            feed.error = Some(error)
        }
    }
    page.after_load(cx);
    cx.notify();
}

/// 读取一次；`force` 为自己操作之后的重读（不受暂停影响）。
pub(super) fn refresh<P: ToolPage>(page: &mut P, force: bool, cx: &mut Context<P>) {
    let Some((seq, future)) = begin_load(page) else {
        return;
    };
    if force {
        page.feed().forced = true;
    }
    let task = cx.spawn(async move |this, cx| {
        let result = future.await;
        let _ = this.update(cx, |page, cx| finish_load(page, seq, result, force, cx));
    });
    page.feed().task = Some(task);
    cx.notify();
}

/// 页正在显示时重读一次，结果直接显示（自己的操作之后：“在终端中执行”的命令结束时）；不在显示的切过去时本来就会读。
pub(super) fn refresh_if_active<P: ToolPage>(page: &mut P, cx: &mut Context<P>) {
    if page.feed().active {
        refresh(page, true, cx);
    }
}

/// 页开始或停止显示：开始时读一次，停止时作废进行中的读取并停止定时刷新。返回是否有变化。
pub(super) fn activate<P: ToolPage>(page: &mut P, active: bool, cx: &mut Context<P>) -> bool {
    if page.feed().active == active {
        return false;
    }
    page.feed().active = active;
    if active {
        refresh(page, false, cx);
    } else {
        let feed = page.feed();
        feed.seq += 1;
        feed.loading = false;
        feed.forced = false;
        feed.task = None;
        feed.poll = None;
        cx.notify();
    }
    true
}

/// 由一份读数算出的结果（筛选后的行、各筛选的数量）：读数是同一份、`key`（搜索、筛选）也没变时沿用上次的，
/// 不在每帧重新计算（终端有输出时整个窗口都会重绘）。
pub(super) struct Memo<T, K, V> {
    last: Option<(Rc<T>, K, V)>,
}

impl<T, K, V> Default for Memo<T, K, V> {
    fn default() -> Self {
        Self { last: None }
    }
}

impl<T, K: PartialEq, V: Clone> Memo<T, K, V> {
    pub fn get(&mut self, data: &Rc<T>, key: K, compute: impl FnOnce(&T, &K) -> V) -> V {
        if let Some((last, last_key, value)) = &self.last
            && Rc::ptr_eq(last, data)
            && *last_key == key
        {
            return value.clone();
        }
        let value = compute(data, &key);
        self.last = Some((data.clone(), key, value.clone()));
        value
    }
}

/// 定时刷新：每次读完再等 `interval`；到点时暂停着、或者另有读取正在进行（自己操作之后的重读）就跳过这一次——
/// 不作废进行中的那次（否则操作之后的重读被它顶掉，暂停期间读到的又先存着，结束的进程留在列表中）。页不再显示时由
/// [`activate`] 停止。
pub(super) fn start_poll<P: ToolPage>(page: &mut P, interval: Duration, cx: &mut Context<P>) {
    let task = cx.spawn(async move |this, cx| {
        loop {
            cx.background_executor().timer(interval).await;
            let Ok(next) = this.update(cx, |page, _| {
                if page.is_paused() || page.feed().loading {
                    Some(None)
                } else {
                    begin_load(page).map(Some)
                }
            }) else {
                return;
            };
            let Some(next) = next else {
                return;
            };
            let Some((seq, future)) = next else {
                continue;
            };
            let result = future.await;
            if this
                .update(cx, |page, cx| finish_load(page, seq, result, false, cx))
                .is_err()
            {
                return;
            }
        }
    });
    page.feed().poll = Some(task);
}

/// 停止定时刷新（不支持的主机）。
pub(super) fn stop_poll<P: ToolPage>(page: &mut P) {
    page.feed().poll = None;
}

/// 恢复刷新时显示暂停期间读到的结果。
pub(super) fn resume<P: ToolPage>(page: &mut P, cx: &mut Context<P>) {
    page.feed().shown.resume();
    cx.notify();
}

/// 行上的悬停提示（地址、容器名等）：右键菜单打开期间不显示（见 [`crate::menu_tooltip::quiet`]）。
pub(super) fn row_tooltip(
    text: SharedString,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    crate::menu_tooltip::quiet(move |window, cx| Tooltip::new(text.clone()).build(window, cx))
}

/// 行的右键菜单建好时调用（各页 `context_menu` 的回调中）：菜单关闭时调用 `on_close`（菜单建好之后才能订阅它的
/// 事件）。
pub(super) fn on_menu_dismiss(
    window: &Window,
    menu: &mut Context<PopupMenu>,
    on_close: impl Fn(&mut App) + 'static,
) {
    let on_close = Rc::new(on_close);
    // 菜单所在的行滚出可视区、页收起时菜单直接被释放，不发 DismissEvent；释放时再通知一次（之后执行，不在释放中更新别的实体）
    let on_release = on_close.clone();
    menu.on_release(move |_, cx| cx.defer(move |cx| on_release(cx)))
        .detach();
    menu.defer_in(window, move |_, _, cx| {
        cx.subscribe_self(move |_, _: &DismissEvent, cx| on_close(cx))
            .detach();
    });
}

// ------------------------------------------------------------ 界面

/// 卡片之间、卡片与面板左右下边缘之间的距离（rem）：与页签、工具条、信息行离面板边缘的距离相同，卡片的边与它们对齐。
pub(super) const CARD_GAP: f32 = 0.5;

/// 卡片（概览的各节，各页的列表与详情）：1px 的边框、大圆角，底色是正文的底色——浅色主题中是浮在面板底色上的白卡片，
/// 深色主题中与面板同色、靠边框分开。圆角不裁剪里面的内容：贴着边框的底色（表头）自己画圆角（[`card_inner_radius`]），
/// 列表离卡片的上下边框留出一点（[`list_card`]）。
pub(super) fn card<E: Styled>(el: E, cx: &App) -> E {
    let theme = cx.theme();
    el.bg(theme.background)
        .border_1()
        .border_color(theme.border)
        .rounded(theme.radius_lg)
}

/// 卡片里贴着边框的底色（表头）的圆角：卡片的圆角减去 1px 的边框，与边框的内缘同心。
pub(super) fn card_inner_radius(cx: &App) -> Pixels {
    (cx.theme().radius_lg - px(1.)).max(px(0.))
}

/// 列表页中的列表：一张卡片，左右离面板边缘 [`CARD_GAP`]（页的下边缘另留 [`CARD_GAP`]，见 [`list_page`]）。列表离卡片的
/// 下边框（`header` 为假时还有上边框）留出 0.25rem：选中、悬停的行是直角的，贴着边框时会从圆角里露出来；有表头时
/// 表头贴着上边框（自己画圆角，[`list_header`]）。行的左右各留 0.25rem（比面板上的 0.5rem 窄，补回卡片占去的宽度），
/// 详情的卡片相同（[`detail_pane`]）。
pub(super) fn list_card<E: Styled>(el: E, header: bool, cx: &App) -> E {
    let el = card(el.mx(rems(CARD_GAP)).pb_1(), cx);
    if header { el } else { el.pt_1() }
}

/// 列表页的根：下边留出 [`CARD_GAP`]（列表或详情的卡片不贴着面板的下边缘）。
pub(super) fn list_page() -> gpui_kit::Div {
    v_flex().size_full().pb(rems(CARD_GAP))
}

/// 列表的表头：一行列名，底色用表头的颜色，贴着卡片的上边框（上边的两个角跟着卡片的圆角），下面一条线。
pub(super) fn list_header(cx: &App) -> gpui_kit::Div {
    let theme = cx.theme();
    h_flex()
        .flex_none()
        .h(rems(1.75))
        .text_xs()
        .text_color(theme.muted_foreground)
        .bg(theme.list_head)
        .rounded_t(card_inner_radius(cx))
        .border_b_1()
        .border_color(theme.border)
}

/// 搜索框（打开页时建好，切换语言后提示文字跟着换）。Esc、Enter 回到列表、保留搜索的内容（见 [`super::keys`]），
/// 清空用框里的 ×。
pub(super) fn search_input(
    placeholder: impl Fn() -> SharedString + 'static,
    window: &mut Window,
    cx: &mut App,
) -> Entity<InputState> {
    cx.new(|cx| InputState::new(window, cx).localized_placeholder(placeholder, window, cx))
}

/// 列表的焦点（Tab 能到达）。
pub(super) fn list_focus(cx: &mut App) -> FocusHandle {
    cx.focus_handle().tab_stop(true)
}

/// 还没有列表（正在读取、这一页不可用的说明）时整页接住列表的焦点：Esc、切换页照常可用。
pub(super) fn focus_body(focus: &FocusHandle, body: AnyElement) -> AnyElement {
    div()
        .size_full()
        .key_context(LIST_CONTEXT)
        .track_focus(focus)
        .child(body)
        .into_any_element()
}

/// 这个终端的 Docker 页上次读到的列表（进程的容器名、网络页的容器端口用它；没有读过时为 `None`，不为此另外读取）。
pub(super) fn docker_cache(context: &PageContext) -> Option<Rc<DockerInfo>> {
    context
        .cache
        .borrow()
        .get(&(context.terminal_id, Page::Docker))
        .and_then(DockerInfo::unwrap)
}

/// 这个终端的进程页（或概览页）上次读到的进程名（服务的主进程的连接，筛选条件上写进程名）；没有读过时为 `None`。
pub(super) fn process_name(context: &PageContext, pid: u32) -> Option<String> {
    let cache = context.cache.borrow();
    let from_list = cache
        .get(&(context.terminal_id, Page::Processes))
        .and_then(ProcessList::unwrap)
        .and_then(|list| {
            list.processes
                .iter()
                .find(|p| p.pid == pid)
                .map(|p| p.name.clone())
        });
    from_list
        .or_else(|| {
            cache
                .get(&(context.terminal_id, Page::Overview))
                .and_then(OverviewData::unwrap)
                .and_then(|data| {
                    data.top
                        .iter()
                        .find(|p| p.pid == pid)
                        .map(|p| p.name.clone())
                })
        })
        .map(|name| crate::sftp::list_model::visible_name(&name))
}

/// 跳到面板的另一页（这一页随之停止显示：等这次更新结束后再切换）。
pub(super) fn go(
    context: &PageContext,
    window: &mut Window,
    cx: &mut App,
    f: impl FnOnce(&mut HostToolsPanel, &mut Window, &mut Context<HostToolsPanel>) + 'static,
) {
    let panel = context.panel.clone();
    window.defer(cx, move |window, cx| {
        let _ = panel.update(cx, |panel, cx| f(panel, window, cx));
    });
}

/// 用键盘（Shift+F10、菜单键）打开的右键菜单：与行的右键菜单是同一个，放在当前行的下面，关掉后焦点回到列表
/// （与 SFTP 的文件列表相同）。
#[derive(Default)]
pub(super) struct KeyMenu(Option<(Entity<PopupMenu>, Subscription)>);

impl KeyMenu {
    pub fn is_open(&self) -> bool {
        self.0.is_some()
    }

    /// 打开 `view` 的键盘菜单（`slot` 取出它记着的 [`KeyMenu`]）：菜单的内容要读取页的状态，在这次更新结束后再建。
    pub fn open<V: 'static>(
        window: &mut Window,
        cx: &mut Context<V>,
        focus: FocusHandle,
        slot: fn(&mut V) -> &mut KeyMenu,
        build: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) {
        let view = cx.entity().downgrade();
        window.defer(cx, move |window, cx| {
            let menu = PopupMenu::build(window, cx, move |menu, window, cx| {
                build(
                    crate::menu_fit::fit(menu, window).action_context(focus),
                    window,
                    cx,
                )
            });
            menu.focus_handle(cx).focus(window, cx);
            let _ = view.update(cx, |view, cx| {
                let dismiss =
                    cx.subscribe_in(&menu, window, move |view, _, _: &DismissEvent, _, cx| {
                        slot(view).0 = None;
                        cx.notify();
                    });
                slot(view).0 = Some((menu, dismiss));
                cx.notify();
            });
        });
    }

    /// 菜单所在的一层：放在当前行（`relative` 的元素）的下面。
    pub fn layer(&self, window: &Window) -> Option<AnyElement> {
        let (menu, _) = self.0.as_ref()?;
        let margin = rems(0.5).to_pixels(window.rem_size());
        Some(
            div()
                .absolute()
                .top_full()
                .left(rems(2.))
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
}

/// 工具条：搜索框与刷新按钮（读取中时转圈、不能再点）。
pub(super) fn toolbar(
    id: &'static str,
    search: &Entity<InputState>,
    loading: bool,
    on_refresh: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    toolbar_with(id, search, None, loading, on_refresh)
}

/// 工具条，搜索框与刷新按钮之间另有一个控件（Docker 页的排序）。
pub(super) fn toolbar_with(
    id: &'static str,
    search: &Entity<InputState>,
    extra: Option<AnyElement>,
    loading: bool,
    on_refresh: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    h_flex()
        .px_2()
        .pb_1p5()
        .gap_1()
        .child(
            div().flex_1().min_w_0().key_context(SEARCH_CONTEXT).child(
                Input::new(search)
                    .id(SharedString::from(format!("{id}-search")))
                    .small()
                    .cleanable(true),
            ),
        )
        .children(extra)
        .child(
            Button::new(SharedString::from(format!("{id}-refresh")))
                .ghost()
                .small()
                .icon(IconName::RefreshCw)
                .loading(loading)
                .disabled(loading)
                .tooltip(t!("tools.refresh"))
                .on_click(on_refresh),
        )
}

/// 一页之内的筛选页签（服务的运行中 / 已停止、Docker 的容器 / 镜像、网络的监听 / 已连接）：下划线样式，与顶部各页的
/// 分段页签分出层级。`scroll` 量页签排开的宽度（面板按它放宽最小宽度，见 [`crate::tab_fit`]）。
pub(super) fn filter_tabs(
    id: &'static str,
    scroll: &ScrollHandle,
    selected: usize,
    tabs: Vec<Tab>,
    on_click: impl Fn(&usize, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div().px_2().pb_1().child(
        TabBar::new(id)
            .underline()
            .xsmall()
            .track_scroll(scroll)
            .selected_index(selected)
            .on_click(on_click)
            .children(tabs),
    )
}

/// 信息行：左侧是说明，右侧是“更新于”，最后是权限的标记（[`privilege_marker`]，没有时不占位置）。
pub(super) fn info_line(
    left: impl IntoElement,
    marker: Option<AnyElement>,
    at: Option<i64>,
    cx: &App,
) -> impl IntoElement {
    let muted = cx.theme().muted_foreground;
    h_flex()
        .px_2()
        .pb_1()
        .gap_2()
        .text_xs()
        .text_color(muted)
        .child(div().flex_1().min_w_0().truncate().child(left))
        .when_some(at, |this, at| {
            this.child(
                div()
                    .flex_none()
                    .text_color(muted.opacity(0.8))
                    .child(t!("tools.updated_at", time = clock_time(at))),
            )
        })
        .children(marker)
}

/// 信息行尾的锁图标说明的是哪一页不能做什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RootScope {
    /// 只能结束自己的进程
    Processes,
    /// 启停服务需要 root
    Services,
    /// 只看得到自己的进程的进程名
    Network,
}

impl RootScope {
    fn key(self) -> &'static str {
        match self {
            RootScope::Processes => "processes",
            RootScope::Services => "services",
            RootScope::Network => "network",
        }
    }

    /// 锁图标的悬停说明。
    fn hint(self) -> SharedString {
        match self {
            RootScope::Processes => t!("tools.privilege.locked_processes"),
            RootScope::Services => t!("tools.privilege.locked_services"),
            RootScope::Network => t!("tools.privilege.locked_network"),
        }
    }

    /// 点开锁图标后的说明。
    fn description(self) -> SharedString {
        match self {
            RootScope::Processes => t!("tools.privilege.limited_processes"),
            RootScope::Services => t!("tools.privilege.limited_services"),
            RootScope::Network => t!("tools.privilege.limited_network"),
        }
    }
}

/// 信息行尾的权限标记：有免密码 sudo 时一个小的“sudo”（悬停说明 `sudo_hint`），没有时一个锁图标（悬停说明这一页
/// 不能做什么，点开说明怎样取得 root 权限，不执行任何操作）；是 root、还不知道时没有。
pub(super) fn privilege_marker(
    scope: RootScope,
    privilege: Option<Privilege>,
    sudo_hint: SharedString,
    context: &PageContext,
    cx: &App,
) -> Option<AnyElement> {
    match privilege? {
        Privilege::Root => None,
        Privilege::Sudo => Some(sudo_badge(
            SharedString::from(format!("tools-sudo-{}", scope.key())),
            sudo_hint,
            cx,
        )),
        Privilege::Limited(sudo) => {
            let host = context.host(cx);
            let dialogs = context.dialogs.clone();
            Some(
                Button::new(SharedString::from(format!("tools-locked-{}", scope.key())))
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(IconName::Lock).text_color(cx.theme().muted_foreground))
                    .tooltip(scope.hint())
                    .on_click(move |_, window, cx| {
                        super::dialogs::open_root_info(
                            sudo,
                            scope.description(),
                            host.clone(),
                            dialogs.clone(),
                            window,
                            cx,
                        )
                    })
                    .into_any_element(),
            )
        }
    }
}

/// 小的“sudo”标记（经免密码 sudo 执行、读取；悬停看说明）。
pub(super) fn sudo_badge(id: SharedString, hint: SharedString, cx: &App) -> AnyElement {
    let theme = cx.theme();
    div()
        .id(id)
        .test_support()
        .flex_none()
        .px_1()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .text_color(theme.muted_foreground)
        .font_family(theme.mono_font_family.clone())
        .child("sudo")
        .tooltip(move |window, cx| Tooltip::new(hint.clone()).build(window, cx))
        .into_any_element()
}

/// 一行说明（不是 root 时能做什么等）。
pub(super) fn hint_line(text: SharedString, color: Hsla) -> impl IntoElement {
    div()
        .px_2()
        .pb_1()
        .text_xs()
        .text_color(color)
        .child(prose(text))
}

/// 已有数据、刷新失败时列表上方的一行红字。
pub(super) fn error_line(error: Option<&ToolsError>, cx: &App) -> Option<impl IntoElement> {
    error.map(|error| hint_line(errors::describe(error), cx.theme().danger))
}

/// 还没有数据时：正在读取。
pub(super) fn loading_text(cx: &App) -> AnyElement {
    div()
        .p_4()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(prose(t!("tools.loading")))
        .into_any_element()
}

/// 列表为空时的说明。
pub(super) fn empty_text(text: SharedString, cx: &App) -> AnyElement {
    div()
        .p_4()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(prose(text))
        .into_any_element()
}

/// 复制文字并提示（在“需要 root 权限”等对话框中复制时写在对话框里，见 [`crate::notify::show`]）。
pub(super) fn copy_text(text: String, shown: String, window: &mut Window, cx: &mut App) {
    cx.write_to_clipboard(ClipboardItem::new_string(text));
    crate::notify::show(
        crate::notify::Notice::success(t!("tools.copied", text = shown)),
        window,
        cx,
    );
}

/// 可复制的命令。
pub(super) fn copy_command(id: &'static str, command: &'static str, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    h_flex()
        .gap_1()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .px_1p5()
                .py_0p5()
                .rounded(theme.radius)
                .bg(theme.muted)
                .text_color(theme.foreground)
                .font_family(theme.mono_font_family.clone())
                .child(command),
        )
        .child(
            Button::new(id)
                .ghost()
                .small()
                .icon(IconName::Copy)
                .tooltip(t!("tools.copy_command"))
                .on_click(move |_, window, cx| {
                    copy_text(command.to_string(), command.to_string(), window, cx)
                }),
        )
}

/// 加入 docker 组的命令。
pub(super) const DOCKER_GROUP_COMMAND: &str = "sudo usermod -aG docker $USER";
/// 启动 Docker 的命令。
const DOCKER_START_COMMAND: &str = "sudo systemctl start docker";

/// 写操作确认框正文的第一行：操作哪台主机（“主机：web（termlet@10.0.0.5）”）；名称就是登录的用户与地址时
/// （快速连接）不重复。
pub(super) fn confirm_host(host: &HostLabel, cx: &App) -> impl IntoElement {
    let text = match host.login.as_ref().filter(|login| **login != host.name) {
        Some(login) => t!(
            "tools.confirm.host_login",
            name = host.name.clone(),
            login = login.clone()
        ),
        None => t!("tools.confirm.host", name = host.name.clone()),
    };
    // 正文的其余部分是灰色的：主机用正文的颜色，一眼看得到是哪台
    div()
        .id("tools-confirm-host")
        .test_support()
        .font_medium()
        .text_color(cx.theme().foreground)
        .child(prose(text))
}

/// 确认框中将执行的命令（灰色等宽，与审计记录的写法一致）：`label` 为“将执行：”等说明，命令太长时只显示开头几行。
pub(super) fn confirm_command(
    id: &'static str,
    label: SharedString,
    command: &str,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    h_flex()
        .id(id)
        .test_support()
        .items_start()
        .gap_1()
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(div().flex_none().child(label))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .line_clamp(4)
                .font_family(theme.mono_font_family.clone())
                .child(command.to_string()),
        )
}

/// 带圆点的一项说明（需要 root 时的几种办法）。
pub(super) fn bullet(child: impl IntoElement) -> impl IntoElement {
    h_flex()
        .items_start()
        .gap_1p5()
        .child(div().flex_none().child("•"))
        .child(div().flex_1().min_w_0().child(child))
}

/// 一页不可用（或读取失败、还没有数据）时的说明与下一步：不是 Linux、没有 systemd、没有 Docker、Docker 没有运行、
/// 没有权限、只能用一个通道；其他错误显示原因。`docker` 为 Docker 页（没有权限时另提示加入 docker 组）。
pub(super) fn notice(
    error: &ToolsError,
    docker: bool,
    on_retry: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let (kind, title, body): (&str, SharedString, AnyElement) = match error {
        ToolsError::Unsupported => (
            "unsupported",
            t!("tools.unavailable.unsupported"),
            div()
                .child(prose(t!("tools.unavailable_hint.unsupported")))
                .into_any_element(),
        ),
        ToolsError::NoSystemd => (
            "no_systemd",
            t!("tools.unavailable.no_systemd"),
            div()
                .child(prose(t!("tools.unavailable_hint.no_systemd")))
                .into_any_element(),
        ),
        ToolsError::NoDocker => (
            "no_docker",
            t!("tools.unavailable.no_docker"),
            div()
                .child(prose(t!("tools.unavailable_hint.no_docker")))
                .into_any_element(),
        ),
        ToolsError::DockerDown => (
            "docker_down",
            t!("tools.unavailable.docker_down"),
            v_flex()
                .gap_1()
                .child(prose(t!("tools.docker_down_hint")))
                .child(copy_command(
                    "tools-copy-docker-start",
                    DOCKER_START_COMMAND,
                    cx,
                ))
                .into_any_element(),
        ),
        // WSL 中的 docker 是 Docker Desktop 的集成：在 Windows 一侧启动，不给 systemctl 的命令
        ToolsError::DockerDesktopDown => (
            "docker_desktop_down",
            t!("tools.unavailable.docker_down"),
            div()
                .child(prose(t!("tools.docker_desktop_hint")))
                .into_any_element(),
        ),
        ToolsError::NeedRoot { sudo, .. } => (
            "need_root",
            if docker {
                t!("tools.unavailable.need_root_docker")
            } else {
                t!("tools.unavailable.need_root")
            },
            v_flex()
                .gap_1p5()
                .child(prose(errors::sudo_text(*sudo)))
                .when(docker, |this| {
                    this.child(bullet(
                        v_flex()
                            .gap_1()
                            .child(prose(t!("tools.root.docker_group")))
                            .child(copy_command(
                                "tools-copy-docker-group",
                                DOCKER_GROUP_COMMAND,
                                cx,
                            )),
                    ))
                })
                .child(bullet(prose(t!("tools.root.login"))))
                .child(bullet(prose(t!("tools.root.sudoers"))))
                .into_any_element(),
        ),
        ToolsError::Exec(ExecError::SingleChannel) => (
            "single_channel",
            t!("tools.unavailable.single_channel"),
            div()
                .child(prose(t!("tools.unavailable_hint.single_channel")))
                .into_any_element(),
        ),
        other => (
            "error",
            t!("tools.load_failed"),
            div()
                .text_color(theme.danger)
                .child(prose(errors::describe(other)))
                .into_any_element(),
        ),
    };
    let retry = kind != "single_channel";
    v_flex()
        .id(SharedString::from(format!("tools-notice-{kind}")))
        .test_support()
        .p_4()
        .gap_2()
        .text_xs()
        .child(
            div()
                .text_sm()
                .font_medium()
                .text_color(theme.foreground)
                .child(title),
        )
        .child(
            div()
                .text_color(muted)
                .line_height(gpui_kit::relative(1.5))
                .child(body),
        )
        .when(retry, |this| {
            this.child(
                h_flex().child(
                    Button::new("tools-notice-retry")
                        .outline()
                        .xsmall()
                        .icon(IconName::RefreshCw)
                        .label(t!("tools.refresh"))
                        .on_click(on_retry),
                ),
            )
        })
        .into_any_element()
}

/// 详情中的最近几行日志（服务、容器）：时间（本机时区的短格式）与内容，错误、警告行着色；没有时间的行（多行消息的
/// 后续行）与上面的内容对齐。
pub(super) fn log_lines(lines: &[LogLine], cx: &App) -> AnyElement {
    let theme = cx.theme();
    let (muted, danger, warning) = (theme.muted_foreground, theme.danger, theme.warning);
    v_flex()
        .font_family(theme.mono_font_family.clone())
        .children(tail_rows(lines).into_iter().map(|(time, line)| {
            let color = match line.level {
                LogLevel::Error => Some(danger),
                LogLevel::Warning => Some(warning),
                LogLevel::Normal => None,
            };
            // 没有时间的行用同样宽的看不见的文字占住时间列
            let (time, time_color) = if time.is_empty() {
                ("00-00 00:00:00".to_string(), theme.transparent)
            } else {
                (time, muted)
            };
            h_flex()
                .items_start()
                .gap_2()
                .child(div().flex_none().text_color(time_color).child(time))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .when_some(color, |this, color| this.text_color(color))
                        .child(line.text.clone()),
                )
        }))
        .into_any_element()
}

/// 状态的语义：各页的状态文字、圆点与标签共用这一张颜色表。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tone {
    /// 运行中、健康（绿）
    Good,
    /// 警告、非 0 退出（黄）
    Warning,
    /// 失败、不健康、破坏性操作（红）
    Danger,
    /// 已停止，以及在用、内置、静态等附带信息（次要文字色）
    Muted,
}

impl Tone {
    pub fn color(self, cx: &App) -> Hsla {
        let theme = cx.theme();
        match self {
            Tone::Good => theme.success,
            Tone::Warning => theme.warning,
            Tone::Danger => theme.danger,
            Tone::Muted => theme.muted_foreground,
        }
    }

    /// 状态圆点的颜色：附带信息的圆点更淡。
    pub fn dot(self, cx: &App) -> Hsla {
        match self {
            Tone::Muted => cx.theme().muted_foreground.opacity(0.4),
            tone => tone.color(cx),
        }
    }
}

/// 右键菜单与 ⋯ 菜单中的一项：破坏性操作（停止、结束、强制结束、删除）用警示样式（文字与图标用危险色，各页一致）；
/// 需要 root 而这条连接上没有免密码 sudo 时（`locked`）文字后面带一个终端图标：确认框的主按钮是“在终端中执行”
/// （[`root_first_footer`]）。
pub(super) fn action_item(
    label: SharedString,
    icon: IconName,
    danger: bool,
    locked: bool,
    cx: &App,
) -> PopupMenuItem {
    if !danger && !locked {
        return PopupMenuItem::new(label).icon(icon);
    }
    let theme = cx.theme();
    let (red, muted) = (theme.danger, theme.muted_foreground);
    PopupMenuItem::element(move |_, _| {
        h_flex()
            .gap_1()
            .when(danger, |this| this.text_color(red))
            .child(label.clone())
            .when(locked, |this| {
                this.child(
                    Icon::new(IconName::SquareTerminal)
                        .xsmall()
                        .text_color(muted),
                )
            })
    })
    .icon(if danger {
        Icon::new(icon).text_color(red)
    } else {
        Icon::new(icon)
    })
}

/// 详情头部的操作按钮：破坏性操作用警示样式，其余为描边；需要 root 而没有免密码 sudo 时文字后面带一个终端图标，悬停
/// 说明会在终端中执行。
pub(super) fn op_button(
    id: impl Into<ElementId>,
    icon: IconName,
    label: SharedString,
    danger: bool,
    locked: bool,
) -> Button {
    let button = Button::new(id).xsmall().icon(icon).label(label);
    let button = if danger {
        button.danger()
    } else {
        button.outline()
    };
    button.when(locked, |this| {
        this.child(Icon::new(IconName::SquareTerminal).xsmall())
            .tooltip(t!("tools.op.terminal_hint"))
    })
}

/// 行右端主要操作的悬停说明：没有免密码 sudo 时写明在终端中执行（“重启（在终端中执行）”）。
pub(super) fn op_tip(label: SharedString, locked: bool) -> SharedString {
    if locked {
        t!("tools.op.in_terminal", action = label)
    } else {
        label
    }
}

// ------------------------------------------------------------ 没有免密码 sudo 时的确认框

/// 没有免密码 sudo 时，在终端中实际执行的命令：要密码时用 sudo，不允许用 sudo、没有 sudo 时用 `su -c`（与“需要
/// root 权限”对话框的主按钮相同）。
pub(super) fn root_first_request(action: &Action, sudo: SudoState) -> TerminalRequest {
    TerminalRequest::Action {
        action: action.clone(),
        root: Some(sudo),
        su: matches!(sudo, SudoState::Denied | SudoState::NotInstalled),
    }
}

/// 确认框中代替“将执行”的部分：在终端中执行的命令，与为什么在终端中执行、还可以仍然尝试的说明。
pub(super) fn root_first_body(action: &Action, sudo: SudoState, cx: &App) -> AnyElement {
    let command = terminal_command(&root_first_request(action, sudo)).unwrap_or_default();
    v_flex()
        .gap_1p5()
        .child(confirm_command(
            "tools-confirm-command",
            t!("tools.root.will_run"),
            &command,
            cx,
        ))
        .child(
            div()
                .id("tools-confirm-root-first")
                .test_support()
                .text_color(cx.theme().muted_foreground)
                .child(prose(match sudo {
                    SudoState::Password => t!("tools.confirm.root_first_sudo"),
                    SudoState::Denied | SudoState::NotInstalled => {
                        t!("tools.confirm.root_first_su")
                    }
                })),
        )
        .into_any_element()
}

/// 没有免密码 sudo 时确认框的底部：取消 · 仍然尝试（照常在后台执行，polkit 等可能允许；失败时与原来一样说明）· 在终端中
/// 执行（主按钮，破坏性操作用警示样式）。两个按钮都先关掉确认框再执行；`on_closed` 是确认框关闭时要做的（自己关掉时
/// 不经确认框的 `on_close`）。
pub(super) fn root_first_footer(
    danger: bool,
    on_try: impl Fn(&mut Window, &mut App) + 'static,
    on_terminal: impl Fn(&mut Window, &mut App) + 'static,
    on_closed: impl Fn(&mut App) + 'static,
) -> DialogFooter {
    let on_closed = Rc::new(on_closed);
    let closed = on_closed.clone();
    let terminal = Button::new("tools-confirm-terminal")
        .icon(IconName::SquareTerminal)
        .label(t!("tools.root.run"));
    let terminal = if danger {
        terminal.danger()
    } else {
        terminal.primary()
    };
    // 三个按钮一样宽（与确认框默认的两个按钮一致）
    let cell = || div().flex_1().min_w_0();
    DialogFooter::new()
        .child(
            cell().child(
                DialogClose::new().child(
                    Button::new("tools-confirm-cancel")
                        .w_full()
                        .label(t!("common.cancel")),
                ),
            ),
        )
        .child(
            cell().child(
                Button::new("tools-confirm-try")
                    .w_full()
                    .outline()
                    .label(t!("tools.confirm.try_anyway"))
                    .tooltip(t!("tools.confirm.try_anyway_hint"))
                    .on_click(move |_, window, cx| {
                        window.close_dialog(cx);
                        closed(cx);
                        on_try(window, cx);
                    }),
            ),
        )
        .child(
            cell().child(terminal.w_full().on_click(move |_, window, cx| {
                window.close_dialog(cx);
                on_closed(cx);
                on_terminal(window, cx);
            })),
        )
}

/// 状态圆点。
pub(super) fn dot(color: Hsla) -> impl IntoElement {
    div().flex_none().size_1p5().rounded_full().bg(color)
}

/// 小徽标（“自定义”“只读”等附带信息）：中性的颜色。
pub(super) fn badge(text: SharedString, cx: &App) -> gpui_kit::Div {
    let theme = cx.theme();
    div()
        .flex_none()
        .px_1()
        .rounded(theme.radius)
        .bg(theme.muted)
        .text_color(theme.muted_foreground)
        .text_xs()
        .child(prose(text))
}

/// 状态标签（健康检查失败、启动中等）：用状态的颜色，不参与截断。
pub(super) fn status_badge(text: SharedString, tone: Tone, cx: &App) -> gpui_kit::Div {
    let color = tone.color(cx);
    div()
        .flex_none()
        .px_1()
        .rounded(cx.theme().radius)
        .bg(color.opacity(0.15))
        .text_color(color)
        .text_xs()
        .whitespace_nowrap()
        .child(text)
}

/// 行上的归属标签（容器、服务）：图标说明是什么，后面是名称（中性的颜色，悬停有完整的说明）。`compact` 时（列表窄）
/// 只留图标（不收缩）；写出名称时最多占 8rem，比进程名先收缩（收缩的权重大），截短名称到只剩图标为止。
pub(super) fn tag(icon: IconName, text: SharedString, compact: bool, cx: &App) -> gpui_kit::Div {
    let theme = cx.theme();
    h_flex()
        .flex_shrink(if compact { 0. } else { 100. })
        .min_w(rems(1.25))
        .max_w(rems(8.))
        .gap_0p5()
        .px_1()
        .rounded(theme.radius)
        .bg(theme.muted)
        .text_color(theme.muted_foreground)
        .text_xs()
        .child(Icon::new(icon).xsmall().flex_none())
        .when(!compact, |this| {
            this.child(div().min_w_0().truncate().child(text))
        })
}

/// 一个筛选条件（“进程 nginx（42）”“端口 8081”“容器 web-1”“已隐藏容器内的进程”）：可见、可清除的小标签，
/// 点 × 清除。
pub(super) fn filter_chip(
    id: impl Into<SharedString>,
    label: SharedString,
    on_clear: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let id = id.into();
    let theme = cx.theme();
    h_flex()
        .id(id.clone())
        .test_support()
        .flex_shrink(1.)
        .min_w_0()
        .gap_0p5()
        .pl_1p5()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(theme.muted)
        .text_xs()
        .child(div().min_w_0().truncate().child(label))
        .child(
            Button::new(SharedString::from(format!("{id}-clear")))
                .ghost()
                .xsmall()
                .icon(IconName::Close)
                .tooltip(t!("tools.filter.clear"))
                .on_click(on_clear),
        )
        .into_any_element()
}

/// 一行筛选条件（没有时不占位置）。
pub(super) fn chip_row(chips: Vec<AnyElement>) -> Option<AnyElement> {
    (!chips.is_empty()).then(|| {
        h_flex()
            .px_2()
            .pb_1()
            .gap_1()
            .flex_wrap()
            .children(chips)
            .into_any_element()
    })
}

/// 跳过来的目标不在列表中时的说明（警示色，可以关掉）。
pub(super) fn note_line(
    text: SharedString,
    on_dismiss: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    h_flex()
        .id("tools-jump-note")
        .test_support()
        .px_2()
        .pb_1()
        .gap_1()
        .items_start()
        .text_xs()
        .text_color(cx.theme().warning)
        .child(div().flex_1().min_w_0().child(prose(text)))
        .child(
            Button::new("tools-jump-note-close")
                .ghost()
                .xsmall()
                .icon(IconName::Close)
                .tooltip(t!("tools.jump.dismiss"))
                .on_click(on_dismiss),
        )
        .into_any_element()
}

// ------------------------------------------------------------ 详情区

/// 详情的最小高度与列表至少留下的高度（rem，表头与四五行）：拖动、这一页放不下时都不越过（各页的列表以它为最小高度）。
const DETAIL_MIN: f32 = 4.;
pub(super) const LIST_MIN: f32 = 8.;
/// 列表上方的工具条、页签、信息行大约占的高度（rem）：拖动时给它们留出位置（另有两张卡片之间与页底的 [`CARD_GAP`]）。
const PAGE_HEAD: f32 = 5.;
/// 没有拖过时详情按内容的高度，最高占这一页的这么多。
const DETAIL_AUTO_MAX: f32 = 0.45;
/// 分隔条获得焦点时 ↑ ↓ 每次调整的高度（逻辑像素，与面板宽度的步长相同）。
const DETAIL_STEP: f32 = 16.;

/// 拖动列表与详情之间的分隔条。
pub(super) struct DetailDrag;

impl Render for DetailDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// 拖过的详情高度（像素）：记下的是 100% 界面大小时的高度，用时乘上界面的缩放倍数（与面板宽度、侧边栏宽度相同，
/// 换了界面大小后与界面一起缩放）；没有拖过（按内容的高度）时为 `None`。
fn detail_height(window: &Window, cx: &App) -> Option<f32> {
    saved_detail_height(cx).map(|h| h * crate::ui_scale::factor(window))
}

/// 记下的详情高度（100% 界面大小时的像素）。
fn saved_detail_height(cx: &App) -> Option<f32> {
    crate::app_state::AppState::global(cx)
        .ui
        .read(cx)
        .state()
        .host_tools_detail_height
        .filter(|h| h.is_finite() && *h > 0.)
}

/// 记下详情的高度：`height` 是这时的像素，换算成 100% 界面大小时的高度再记；`None` 为恢复按内容的高度。
fn set_detail_height(height: Option<f32>, window: &Window, cx: &mut App) {
    let scale = crate::ui_scale::factor(window);
    let height = height.map(|h| (h / scale).round());
    crate::app_state::AppState::ui(cx).update(cx, |ui, cx| {
        ui.update(|s| s.host_tools_detail_height = height, cx)
    });
}

/// 拖动分隔条（各页的根元素上调用）：指针在两张卡片之间空隙的中间，详情卡片的上边缘在指针下面半个空隙，下边缘离页的
/// 下边缘一个空隙；高度不小于 [`DETAIL_MIN`]，给列表留出 [`LIST_MIN`]。
pub(super) fn on_detail_drag(event: &DragMoveEvent<DetailDrag>, window: &mut Window, cx: &mut App) {
    let rem = window.rem_size();
    let bounds = event.bounds;
    let min = f32::from(rems(DETAIL_MIN).to_pixels(rem));
    let max =
        f32::from(bounds.size.height - rems(LIST_MIN + PAGE_HEAD + 2. * CARD_GAP).to_pixels(rem))
            .max(min);
    let height = f32::from(
        bounds.origin.y + bounds.size.height
            - event.event.position.y
            - rems(1.5 * CARD_GAP).to_pixels(rem),
    );
    set_detail_height(Some(height.clamp(min, max)), window, cx);
}

/// 列表下方的详情：另一张卡片，与列表的卡片之间隔着 [`CARD_GAP`]。这道空隙是可拖动的分隔条（不占布局；双击恢复按
/// 内容的高度，获得焦点后 ↑ ↓ 调整），卡片里是一行头部（[`detail_header`]，下面一条线，不随属性滚动），下面是属性
/// （超出时滚动）。拖过时用记下的高度（各页共用，记在本机的界面状态中，按 100% 界面大小记、随界面大小缩放；这一页
/// 放不下时先收详情，列表至少留 [`LIST_MIN`]），没有拖过时按内容的高度，最高占这一页的 45%。`measured` 记下这一帧的
/// 高度（键盘调整从它开始）。
pub(super) fn detail_pane(
    id: &'static str,
    splitter: &FocusHandle,
    measured: &Rc<Cell<Pixels>>,
    header: AnyElement,
    body: Vec<AnyElement>,
    window: &Window,
    cx: &App,
) -> AnyElement {
    let drag = cx.theme().drag_border;
    let height = detail_height(window, cx);
    let record = measured.clone();
    let start = measured.clone();
    // 占满两张卡片之间的空隙
    let handle = div()
        .id(SharedString::from(format!("{id}-splitter")))
        .test_support()
        .track_focus(splitter)
        .absolute()
        .left_0()
        .right_0()
        .top(-rems(CARD_GAP))
        .h(rems(CARD_GAP))
        .cursor_row_resize()
        .hover(|s| s.bg(drag.opacity(0.5)))
        .focus(|s| s.bg(drag))
        .tooltip(|window, cx| Tooltip::new(t!("tools.detail.resize")).build(window, cx))
        .on_key_down(move |event: &KeyDownEvent, window, cx| {
            let step = match event.keystroke.key.as_str() {
                "up" => DETAIL_STEP,
                "down" => -DETAIL_STEP,
                _ => return,
            };
            let rem = window.rem_size();
            let min = f32::from(rems(DETAIL_MIN).to_pixels(rem));
            let step = step * crate::ui_scale::factor(window);
            let current = detail_height(window, cx).unwrap_or(f32::from(start.get()));
            set_detail_height(Some((current + step).max(min)), window, cx);
            cx.stop_propagation();
        })
        .on_drag(DetailDrag, |_, _, _, cx| cx.new(|_| DetailDrag))
        .on_click({
            let splitter = splitter.clone();
            move |event: &ClickEvent, window, cx| {
                splitter.focus(window, cx);
                if event.click_count() >= 2 {
                    set_detail_height(None, window, cx);
                }
            }
        });
    card(v_flex(), cx)
        .relative()
        .mx(rems(CARD_GAP))
        .mt(rems(CARD_GAP))
        .map(|this| match height {
            Some(height) => this.h(px(height)).flex_shrink(1.).min_h(rems(DETAIL_MIN)),
            None => this.flex_none().max_h(relative(DETAIL_AUTO_MAX)),
        })
        .child(
            canvas(
                move |bounds, _, _| record.set(bounds.size.height),
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .child(header)
        .child(
            v_flex().id(id).test_support().flex_1().min_h_0().child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .pl_1()
                    .scrollbar_gutter()
                    .py_1p5()
                    .gap_0p5()
                    .text_xs()
                    .overflow_y_scrollbar()
                    .id(SharedString::from(format!("{id}-body")))
                    .children(body),
            ),
        )
        .child(handle)
        .into_any_element()
}

/// 详情的头部一行：名称 ● 状态 · 主要操作 · ⋯（次要操作的菜单，与行的右键菜单相同），下面一条线与属性分开（与列表的
/// 表头同高）。窄的时候先截短状态、再截短名称，按钮总看得到。
pub(super) fn detail_header(
    name: SharedString,
    status: Option<(SharedString, Tone)>,
    actions: Vec<AnyElement>,
    more: Option<AnyElement>,
    cx: &App,
) -> AnyElement {
    h_flex()
        .flex_none()
        .px_1()
        .py_1()
        .gap_1p5()
        .border_b_1()
        .border_color(cx.theme().border)
        .text_xs()
        .child(
            div()
                .flex_shrink(1.)
                .min_w(rems(3.))
                .truncate()
                .font_medium()
                .child(name),
        )
        .when_some(status, |this, (text, tone)| {
            this.child(
                h_flex()
                    .flex_shrink(4.)
                    .min_w_0()
                    .gap_1()
                    .child(dot(tone.dot(cx)))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(tone.color(cx))
                            .child(text),
                    ),
            )
        })
        .child(div().flex_1())
        .children(actions)
        .children(more)
        .into_any_element()
}

/// 详情头部与行右端的 ⋯：次要操作的菜单（与行的右键菜单是同一个）。窗口中开着右键菜单时不挂悬停提示（按钮的
/// 提示只在按下左键时收起，会浮在菜单上，见 [`crate::menu_tooltip`]）。
pub(super) fn more_button(
    id: impl Into<ElementId>,
    build: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    window: &Window,
    cx: &App,
) -> AnyElement {
    Button::new(id)
        .ghost()
        .xsmall()
        .icon(IconName::Ellipsis)
        .when(!crate::menu_tooltip::menu_open(window, cx), |this| {
            this.tooltip(t!("tools.detail.more"))
        })
        .dropdown_menu(build)
        .into_any_element()
}

/// 列表有焦点时当前行的焦点框（与 SFTP 的文件列表相同）。
pub(super) fn ring<E: Styled>(el: E, on: bool, cx: &App) -> E {
    if on {
        el.border_1().border_color(cx.theme().ring)
    } else {
        el
    }
}

// ------------------------------------------------------------ 行上的操作

/// 写操作：行上进行中、刚完成时的说法。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OpVerb {
    Start,
    Stop,
    Restart,
    Reload,
    Enable,
    Disable,
    /// 结束进程（TERM）
    Term,
    /// 强制结束进程（KILL）
    Kill,
    Remove,
}

impl OpVerb {
    /// 进行中：“正在重启…”
    pub fn busy(self) -> SharedString {
        match self {
            OpVerb::Start => t!("tools.op.busy_start"),
            OpVerb::Stop => t!("tools.op.busy_stop"),
            OpVerb::Restart => t!("tools.op.busy_restart"),
            OpVerb::Reload => t!("tools.op.busy_reload"),
            OpVerb::Enable => t!("tools.op.busy_enable"),
            OpVerb::Disable => t!("tools.op.busy_disable"),
            OpVerb::Term => t!("tools.op.busy_term"),
            OpVerb::Kill => t!("tools.op.busy_kill"),
            OpVerb::Remove => t!("tools.op.busy_remove"),
        }
    }

    /// 刚完成：“已重启”
    pub fn done(self) -> SharedString {
        match self {
            OpVerb::Start => t!("tools.op.done_start"),
            OpVerb::Stop => t!("tools.op.done_stop"),
            OpVerb::Restart => t!("tools.op.done_restart"),
            OpVerb::Reload => t!("tools.op.done_reload"),
            OpVerb::Enable => t!("tools.op.done_enable"),
            OpVerb::Disable => t!("tools.op.done_disable"),
            OpVerb::Term => t!("tools.op.done_term"),
            OpVerb::Kill => t!("tools.op.done_kill"),
            OpVerb::Remove => t!("tools.op.done_remove"),
        }
    }

    /// 失败：“重启失败”
    pub fn failed(self) -> SharedString {
        match self {
            OpVerb::Start => t!("tools.op.failed_start"),
            OpVerb::Stop => t!("tools.op.failed_stop"),
            OpVerb::Restart => t!("tools.op.failed_restart"),
            OpVerb::Reload => t!("tools.op.failed_reload"),
            OpVerb::Enable => t!("tools.op.failed_enable"),
            OpVerb::Disable => t!("tools.op.failed_disable"),
            OpVerb::Term => t!("tools.op.failed_term"),
            OpVerb::Kill => t!("tools.op.failed_kill"),
            OpVerb::Remove => t!("tools.op.failed_remove"),
        }
    }
}

/// 一行上的写操作：进行中、刚完成，或刚失败。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RowOp {
    Busy(OpVerb),
    Done(OpVerb),
    Failed(OpVerb),
}

/// 刚完成的操作在行上显示多久（之后照常显示重读到的状态）。
pub(super) const DONE_SHOWN: Duration = Duration::from_secs(3);
/// 失败在行上显示多久（原因留在信息行下，关掉为止）。
pub(super) const FAILED_SHOWN: Duration = Duration::from_secs(8);

/// 写操作的结果（[`HostToolsPanel::perform`] 交给发起的页）。
#[derive(Clone, Debug)]
pub(super) enum OpOutcome {
    Done,
    Failed(OpFailure),
    /// 没有执行（需要 root：打开了说明，可以改在终端中执行）：行上恢复原样
    Aborted,
}

/// 失败的写操作：说什么、为什么。
#[derive(Clone, Debug, PartialEq)]
pub(super) struct OpFailure {
    /// 操作的说明（“重启服务 nginx”）
    pub action: SharedString,
    /// 原因：命令输出的开头（[`brief`]），或错误的说明
    pub reason: SharedString,
    /// 完整的输出（比原因长时可以展开看）
    pub detail: Option<String>,
}

impl OpFailure {
    pub fn new(action: SharedString, reason: SharedString, detail: Option<String>) -> Self {
        // 原因已经是全部时不另给详情
        let detail = detail.filter(|d| d.trim() != reason.trim());
        Self {
            action,
            reason,
            detail,
        }
    }
}

/// 信息行下失败的原因：展开着时显示完整的输出。
struct FailureNote {
    failure: OpFailure,
    expanded: bool,
}

/// 各行进行中、刚完成与刚失败的写操作（键为行的标识：PID、服务名、容器 ID、镜像等的键）：进行中时行上状态的位置转圈并写
/// “正在重启…”，这一行（与它的详情）的操作不能点；成功后写“已重启”一会儿（[`DONE_SHOWN`]），失败时写“重启失败”一会儿
/// （[`FAILED_SHOWN`]），原因写在信息行下（[`failure_banner`]，关掉或开始下一个操作为止）。每次操作一个序号：同一行上
/// 后来的操作不被先前那次的结束清掉。
pub(super) struct RowOps<K> {
    ops: HashMap<K, (RowOp, u64)>,
    seq: u64,
    failure: Option<FailureNote>,
}

impl<K> Default for RowOps<K> {
    fn default() -> Self {
        Self {
            ops: HashMap::new(),
            seq: 0,
            failure: None,
        }
    }
}

impl<K: Eq + Hash + Clone> RowOps<K> {
    pub fn get(&self, key: &K) -> Option<RowOp> {
        self.ops.get(key).map(|(op, _)| *op)
    }

    /// 信息行下说明着的失败。
    pub fn failure(&self) -> Option<&OpFailure> {
        self.failure.as_ref().map(|note| &note.failure)
    }

    fn failure_expanded(&self) -> bool {
        self.failure.as_ref().is_some_and(|note| note.expanded)
    }

    fn toggle_failure(&mut self) {
        if let Some(note) = self.failure.as_mut() {
            note.expanded = !note.expanded;
        }
    }

    fn dismiss_failure(&mut self) {
        self.failure = None;
    }

    /// 直接放进信息行下失败的原因（测试）。
    #[cfg(test)]
    pub fn set_failure(&mut self, failure: OpFailure) {
        self.failure = Some(FailureNote {
            failure,
            expanded: false,
        });
    }

    /// 这一行的操作正在进行（操作按钮、菜单项不能点）。
    pub fn busy(&self, key: &K) -> bool {
        matches!(self.get(key), Some(RowOp::Busy(_)))
    }

    /// 其中有一行的操作正在进行（compose 项目的整体操作）。
    pub fn any_busy<'a>(&self, mut keys: impl Iterator<Item = &'a K>) -> bool
    where
        K: 'a,
    {
        keys.any(|key| self.busy(key))
    }

    fn start(&mut self, keys: &[K], verb: OpVerb) -> u64 {
        self.seq += 1;
        // 开始下一个操作：上一次失败的原因不再留着
        self.failure = None;
        for key in keys {
            self.ops.insert(key.clone(), (RowOp::Busy(verb), self.seq));
        }
        self.seq
    }

    fn finish(&mut self, keys: &[K], seq: u64, outcome: &OpOutcome) {
        for key in keys {
            if let Some((op, at)) = self.ops.get_mut(key)
                && *at == seq
            {
                match (*op, outcome) {
                    (RowOp::Busy(verb), OpOutcome::Done) => *op = RowOp::Done(verb),
                    (RowOp::Busy(verb), OpOutcome::Failed(_)) => *op = RowOp::Failed(verb),
                    _ => {
                        self.ops.remove(key);
                    }
                }
            }
        }
        if let OpOutcome::Failed(failure) = outcome {
            self.failure = Some(FailureNote {
                failure: failure.clone(),
                expanded: false,
            });
        }
    }

    fn clear(&mut self, keys: &[K], seq: u64) {
        for key in keys {
            if self.ops.get(key).is_some_and(|(_, at)| *at == seq) {
                self.ops.remove(key);
            }
        }
    }
}

/// 一次写操作在行上的进度：`keys` 这几行先标为进行中，`done` 得到结果后标为完成或失败（没有执行时恢复）并调用
/// `after`（重读列表等；参数为是否成功），[`DONE_SHOWN`]、[`FAILED_SHOWN`] 之后清掉行上的标记。
pub(super) fn track_op<V: 'static, K: Eq + Hash + Clone + 'static>(
    page: &mut V,
    slot: fn(&mut V) -> &mut RowOps<K>,
    keys: Vec<K>,
    verb: OpVerb,
    done: futures::channel::oneshot::Receiver<OpOutcome>,
    after: impl FnOnce(&mut V, bool, &mut Context<V>) + 'static,
    cx: &mut Context<V>,
) {
    let seq = slot(page).start(&keys, verb);
    cx.notify();
    cx.spawn(async move |this, cx| {
        let outcome = done.await.unwrap_or(OpOutcome::Aborted);
        let ok = matches!(outcome, OpOutcome::Done);
        let finished = this.update(cx, |page, cx| {
            slot(page).finish(&keys, seq, &outcome);
            after(page, ok, cx);
            cx.notify();
        });
        let shown = match outcome {
            OpOutcome::Done => DONE_SHOWN,
            OpOutcome::Failed(_) => FAILED_SHOWN,
            OpOutcome::Aborted => return,
        };
        if finished.is_err() {
            return;
        }
        cx.background_executor().timer(shown).await;
        let _ = this.update(cx, |page, cx| {
            slot(page).clear(&keys, seq);
            cx.notify();
        });
    })
    .detach();
}

/// 行上状态的位置在操作进行中、刚完成时的样子：转圈与“正在重启…”（次要文字色），或勾与“已重启”（成功色）。
pub(super) fn op_status(id: impl Into<ElementId>, op: RowOp, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let row = h_flex()
        .id(id.into())
        .test_support()
        .flex_none()
        .gap_1()
        .whitespace_nowrap();
    match op {
        RowOp::Busy(verb) => row
            .text_color(theme.muted_foreground)
            .child(Spinner::new().xsmall().color(theme.muted_foreground))
            .child(verb.busy()),
        RowOp::Done(verb) => row
            .text_color(Tone::Good.color(cx))
            .child(Icon::new(IconName::Check).xsmall())
            .child(verb.done()),
        RowOp::Failed(verb) => row
            .text_color(Tone::Danger.color(cx))
            .child(Icon::new(IconName::CircleX).xsmall())
            .child(verb.failed()),
    }
    .into_any_element()
}

/// 信息行下失败的原因（各页同一个样子，没有失败时不占位置）：“重启服务 nginx 失败”与原因的开头（最多两行），完整的
/// 输出比原因长时可以展开（等宽、可滚动）；可以复制输出、关掉。`slot` 取出页记着的 [`RowOps`]。
pub(super) fn failure_banner<V: 'static, K: Eq + Hash + Clone + 'static>(
    ops: &RowOps<K>,
    slot: fn(&mut V) -> &mut RowOps<K>,
    cx: &mut Context<V>,
) -> Option<AnyElement> {
    let failure = ops.failure()?.clone();
    let expanded = ops.failure_expanded();
    let theme = cx.theme();
    let (danger, muted) = (theme.danger, theme.muted_foreground);
    let full = failure
        .detail
        .clone()
        .unwrap_or_else(|| failure.reason.to_string());
    let copy = full.clone();
    let head = h_flex()
        .items_start()
        .gap_1()
        .child(
            Icon::new(IconName::CircleX)
                .xsmall()
                .flex_none()
                .text_color(danger),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .font_medium()
                .text_color(danger)
                .child(prose(t!(
                    "tools.failure.title",
                    action = failure.action.clone()
                ))),
        )
        .when(failure.detail.is_some(), |this| {
            this.child(
                Button::new("tools-failure-toggle")
                    .ghost()
                    .xsmall()
                    .icon(if expanded {
                        IconName::ChevronUp
                    } else {
                        IconName::ChevronDown
                    })
                    .tooltip(if expanded {
                        t!("tools.failure.collapse")
                    } else {
                        t!("tools.failure.expand")
                    })
                    .on_click(cx.listener(move |page, _, _, cx| {
                        slot(page).toggle_failure();
                        cx.notify();
                    })),
            )
        })
        .child(
            Button::new("tools-failure-copy")
                .ghost()
                .xsmall()
                .icon(IconName::Copy)
                .tooltip(t!("tools.failure.copy"))
                .on_click(move |_, window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                    window.push_notification(Notification::success(t!("tools.failure.copied")), cx);
                }),
        )
        .child(
            Button::new("tools-failure-dismiss")
                .ghost()
                .xsmall()
                .icon(IconName::Close)
                .tooltip(t!("tools.failure.dismiss"))
                .on_click(cx.listener(move |page, _, _, cx| {
                    slot(page).dismiss_failure();
                    cx.notify();
                })),
        );
    let body = if expanded {
        div()
            .id("tools-failure-detail")
            .test_support()
            .rounded(theme.radius)
            .bg(theme.muted.opacity(0.5))
            .overflow_hidden()
            .child(
                div()
                    .max_h(rems(10.))
                    .px_1p5()
                    .py_1()
                    .text_color(theme.foreground)
                    .font_family(theme.mono_font_family.clone())
                    .overflow_y_scrollbar()
                    .id("tools-failure-text")
                    .child(full),
            )
            .into_any_element()
    } else {
        div()
            .id("tools-failure-reason")
            .test_support()
            .pl_4()
            .line_clamp(2)
            .text_color(muted)
            .child(failure.reason.clone())
            .into_any_element()
    };
    Some(
        v_flex()
            .id("tools-failure")
            .test_support()
            .mx_2()
            .mb_1()
            .px_1p5()
            .py_1()
            .gap_0p5()
            .rounded(theme.radius)
            .border_1()
            .border_color(danger.opacity(0.4))
            .bg(danger.opacity(0.06))
            .text_xs()
            .child(head)
            .child(body)
            .into_any_element(),
    )
}

/// 行右端浮出的操作：主要操作与 ⋯（与右键菜单、详情头部的 ⋯ 是同一个菜单）。鼠标停在行上、键盘焦点在这一行、这一行的
/// 菜单打开着时出现，盖在状态文字上（底色与行的底色相同），不改变行高与布局；在上面按下不选中这一行（选中会让详情出现、
/// 把列表挤矮，⋯ 的菜单就没有了着落）。`row_bg` 是行此时的底色（可能是半透明的，叠在列表卡片的底色上）。
pub(super) fn row_actions(
    id: impl Into<ElementId>,
    row_bg: Hsla,
    buttons: Vec<AnyElement>,
    cx: &App,
) -> AnyElement {
    h_flex()
        .id(id.into())
        .test_support()
        .absolute()
        .top_0()
        .bottom_0()
        .right_0()
        .pl_1()
        .scrollbar_gutter()
        .gap_0p5()
        .bg(cx.theme().background.blend(row_bg))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .children(buttons)
        .into_any_element()
}

/// 行右端的主要操作：图标按钮（破坏性的用危险色的图标），悬停看名称（窗口中开着右键菜单时不挂，见
/// [`crate::menu_tooltip`]）；进行中时不能点。
pub(super) fn row_action_button(
    id: impl Into<ElementId>,
    icon: IconName,
    label: SharedString,
    danger: bool,
    busy: bool,
    window: &Window,
    cx: &App,
) -> Button {
    let icon = if danger && !busy {
        Icon::new(icon).text_color(cx.theme().danger)
    } else {
        Icon::new(icon)
    };
    Button::new(id)
        .ghost()
        .xsmall()
        .icon(icon)
        .when(!crate::menu_tooltip::menu_open(window, cx), |this| {
            this.tooltip(label)
        })
        .disabled(busy)
}

/// 鼠标停在哪一行上（行右端的操作按它显示）：移进一行时记下；移出时只清除自己（移到相邻的行时先后不定）。返回是否变了。
pub(super) fn hover_row<K: PartialEq>(slot: &mut Option<K>, key: K, hovered: bool) -> bool {
    if hovered {
        if slot.as_ref() == Some(&key) {
            return false;
        }
        *slot = Some(key);
        true
    } else if slot.as_ref() == Some(&key) {
        *slot = None;
        true
    } else {
        false
    }
}

/// 确认框中写出的将执行的命令：与审计记录的写法相同（经免密码 sudo 时带 `sudo -n`），只是容器、镜像、网络的完整 ID
/// 写成前 12 位（审计照记完整的）。
pub(super) fn confirm_line(action: &Action, sudo: bool) -> String {
    let Ok(argv) = action.argv() else {
        return String::new();
    };
    let argv: Vec<String> = argv
        .into_iter()
        .map(|arg| {
            let bare = arg.strip_prefix("sha256:").unwrap_or(&arg);
            if bare.len() == 64 && bare.bytes().all(|b| b.is_ascii_hexdigit()) {
                short_id(&arg).to_string()
            } else {
                arg
            }
        })
        .collect();
    let line = termlet_core::hosttools::display(&argv);
    if sudo {
        format!("sudo -n {line}")
    } else {
        line
    }
}

/// 失败时可以展开看的完整输出：太长时只留开头（不把很长的输出整个放进界面）。
pub(super) fn brief_detail(output: &str) -> String {
    const MAX: usize = 8000;
    let mut chars = output.chars();
    let mut shown: String = chars.by_ref().take(MAX).collect();
    if chars.next().is_some() {
        shown.push('…');
    }
    shown
}

/// 输出的开头（写操作失败时的提示）：去掉首尾空白，太长时截断。
pub(super) fn brief(output: &str) -> String {
    const MAX: usize = 600;
    let output = output.trim();
    let mut chars = output.chars();
    let mut shown: String = chars.by_ref().take(MAX).collect();
    if chars.next().is_some() {
        shown.push('…');
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 确认框中的命令：完整的 ID 写成前 12 位，其余与审计记录的写法相同；经 sudo 时带 `sudo -n`。
    #[test]
    fn confirm_line_shortens_ids() {
        use termlet_core::hosttools::{ContainerOp, Engine};
        let id = "c07d8b88b8c6b55cb13f9e92654df607a79449021ceeeb816e058a36b291dc83";
        let action = Action::Container {
            engine: Engine::Docker,
            op: ContainerOp::Stop,
            ids: vec![id.into()],
        };
        assert_eq!(confirm_line(&action, false), "docker stop c07d8b88b8c6");
        assert_eq!(
            confirm_line(&action, true),
            "sudo -n docker stop c07d8b88b8c6"
        );
        assert!(
            action.command_line().unwrap().contains(id),
            "审计照记完整的"
        );
        let image = Action::RemoveImages {
            engine: Engine::Docker,
            ids: vec![format!("sha256:{id}")],
        };
        assert!(confirm_line(&image, false).ends_with(" c07d8b88b8c6"));
    }

    /// 行上的操作：进行中、成功后标为完成、失败时标为失败并记下原因、没有执行时恢复；同一行上后来的操作不被先前那次的
    /// 结束清掉；开始下一个操作时不再留着上一次失败的原因。
    #[test]
    fn row_ops_track_each_operation() {
        let mut ops: RowOps<u32> = RowOps::default();
        let first = ops.start(&[1, 2], OpVerb::Restart);
        assert!(ops.busy(&1) && ops.busy(&2));
        assert!(ops.any_busy([3, 2].iter()));
        ops.finish(&[1, 2], first, &OpOutcome::Done);
        assert_eq!(ops.get(&1), Some(RowOp::Done(OpVerb::Restart)));
        let second = ops.start(&[2], OpVerb::Stop);
        ops.clear(&[1, 2], first);
        assert_eq!(ops.get(&1), None);
        assert_eq!(
            ops.get(&2),
            Some(RowOp::Busy(OpVerb::Stop)),
            "后来的操作不被清掉"
        );
        ops.finish(&[2], second, &OpOutcome::Aborted);
        assert_eq!(ops.get(&2), None, "没有执行时恢复原样");
        assert!(ops.failure().is_none());

        let third = ops.start(&[3], OpVerb::Stop);
        let failure = OpFailure::new(
            "停止服务 nginx".into(),
            "Job failed".into(),
            Some("Job failed\nsee journalctl".into()),
        );
        ops.finish(&[3], third, &OpOutcome::Failed(failure.clone()));
        assert_eq!(ops.get(&3), Some(RowOp::Failed(OpVerb::Stop)));
        assert!(!ops.busy(&3), "失败的行可以再操作");
        assert_eq!(ops.failure(), Some(&failure));
        ops.toggle_failure();
        assert!(ops.failure_expanded());
        ops.clear(&[3], third);
        assert_eq!(ops.get(&3), None, "行上的失败过一会儿清掉");
        assert!(ops.failure().is_some(), "原因留到关掉为止");
        ops.start(&[4], OpVerb::Start);
        assert!(ops.failure().is_none(), "开始下一个操作时不再留着");
    }

    /// 没有免密码 sudo 时在终端中执行的命令：要密码时用 sudo，不允许用 sudo、没有 sudo 时用 su。
    #[test]
    fn root_first_runs_sudo_or_su() {
        use termlet_core::hosttools::ServiceOp;
        let action = Action::Service {
            op: ServiceOp::Stop,
            unit: "nginx.service".into(),
        };
        let command = |sudo| terminal_command(&root_first_request(&action, sudo)).unwrap();
        assert_eq!(
            command(SudoState::Password),
            "sudo systemctl stop -- nginx.service"
        );
        for sudo in [SudoState::Denied, SudoState::NotInstalled] {
            let line = command(sudo);
            assert!(line.starts_with("su -c "), "{line}");
            assert!(line.contains("systemctl stop -- nginx.service"), "{line}");
        }
    }

    /// 失败的原因已经是全部输出时不另给详情。
    #[test]
    fn failure_detail_only_when_longer() {
        let same = OpFailure::new("a".into(), "boom".into(), Some(" boom\n".into()));
        assert_eq!(same.detail, None);
        let more = OpFailure::new("a".into(), "boom".into(), Some("boom\nmore".into()));
        assert_eq!(more.detail.as_deref(), Some("boom\nmore"));
    }

    /// 悬停的行：移出时只清除自己。
    #[test]
    fn hover_row_clears_only_itself() {
        let mut slot = None;
        assert!(hover_row(&mut slot, 1, true));
        assert!(!hover_row(&mut slot, 1, true));
        assert!(hover_row(&mut slot, 2, true));
        assert!(
            !hover_row(&mut slot, 1, false),
            "移到相邻的行之后才收到移出"
        );
        assert_eq!(slot, Some(2));
        assert!(hover_row(&mut slot, 2, false));
        assert_eq!(slot, None);
    }

    #[test]
    fn brief_trims_and_truncates() {
        assert_eq!(brief("  failed\n"), "failed");
        let long = "x".repeat(700);
        let shown = brief(&long);
        assert_eq!(shown.chars().count(), 601);
        assert!(shown.ends_with('…'));
    }
}
