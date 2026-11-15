//! 主机概览的小窗：把主机工具的概览弹出成一个独立的系统窗口，标题“<主机名> · 概览”，内容是概览页的紧凑版式
//! （[`OverviewPage`] 的 [`Layout::Window`]：CPU、内存、网速的矮曲线与当前值，最满的磁盘，CPU 最高的几个进程；窗口
//! 拉大时多显示一些）。从概览页右上角的按钮、命令面板打开（[`pop_out`]）。
//!
//! - 固定一台主机：跟着弹出时主机工具面板跟随的那个终端，不随当前终端切换；每台主机最多一个（键同面板记下的页，
//!   [`host_key`]），再弹出时把已有的拿到前面（不抢焦点）。
//! - 置顶：默认始终在其他窗口之上，标题栏上的图钉取消、恢复（[`crate::topmost`]；Linux 上不支持，没有图钉）。
//! - 采样：小窗显示着时终端不在前台也照常采样（[`TerminalPanel::keep_sampling`]，间隔与状态条相同），小窗最小化、
//!   关掉后恢复原来的规则；补充信息与进程在小窗显示着时每 10 秒读一次（与概览页相同）。连接断开时显示“已断开”
//!   （终端自己停止采样）与“重新连接”（与终端的重新连接相同，在终端所在的窗口中进行，询问也在那里），重连后继续；
//!   终端的 Tab 关闭时（含关闭它所在的主窗口）小窗随之关闭，所有主窗口都关了、隐藏到托盘时也一起关闭
//!   （[`close_all`]）。
//! - 记住：位置、大小与是否置顶按主机记在本机的界面状态中（`overviewWindows`）：移动、改大小停下片刻后，切换置顶、
//!   关闭时记下；恢复的规则与主窗口相同（[`window_state::restore_saved`]）。大小按 100% 界面大小记、用时乘上界面的
//!   缩放倍数（与侧边栏宽度相同），开着时换了界面大小窗口也按比例跟着变（[`scaled_placement`]）。第一次打开时放在
//!   弹出它的窗口所在屏幕的右上角，大小按界面大小放大。界面大小与深浅跟随主程序（主题是全局的）。
//! - 不抢焦点：打开、拿到前面时都不激活窗口；小窗中没有终端的输入，也没有键盘操作。
//! - 任务栏与程序坞：Windows 上单独占一个任务栏按钮（与 Termlet 的窗口归在一组，Alt+Tab 中也有：取消置顶后被盖住、
//!   最小化时从这里找回，与 Teams、Outlook 弹出的窗口相同）；macOS 上没有单独的程序坞图标，在 Termlet 的“窗口”菜单中
//!   列出；Linux 上与主窗口同一个 app_id，由桌面环境归组。
//! - 应用锁定时只显示“Termlet 已锁定”。

use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Selectable as _, Sizable as _, StyledExt as _, TitleBar, h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, AnyWindowHandle, App, AppContext as _, Bounds, Context, DisplayId, Entity, Global,
    InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render, SharedString,
    Styled as _, Subscription, Task, TestSupportExt as _, TitlebarOptions, WeakEntity, Window,
    WindowBounds, WindowOptions, div, px, size,
};
use termlet_core::ui_state::{OverviewWindow as SavedWindow, WindowPlacement};

use super::overview::{Layout, OverviewPage};
use super::panel::host_key;
use super::shared::{OpenDialogs, PageContext, ToolCache};
use super::target::{ToolsState, ToolsTarget};
use crate::app_state::AppState;
use crate::t;
use crate::terminal_panel::{SamplingHold, TerminalPanel};
use crate::window_state::{self, Rect, Restored};

/// 第一次打开时的大小（100% 界面大小时的逻辑像素，按界面大小放大）：放得下紧凑版式的全部内容。
pub(super) const DEFAULT_SIZE: (f32, f32) = (320., 448.);
/// 第一次打开时离屏幕可见区域右上角的距离；已经开着别的小窗时依次往左下错开这么多。
const MARGIN: f32 = 16.;
const CASCADE: f32 = 32.;
/// 最小尺寸（物理边界，不随界面大小）。
const MIN_SIZE: (f32, f32) = (240., 200.);

/// 小窗所跟的终端的连接情况。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Link {
    /// 连接着（只用一个通道、服务器不是 Linux 时由概览页说明）
    Connected,
    Connecting,
    /// 断线重连中
    Reconnecting,
    /// 会话已结束、连接失败
    Ended,
}

impl Link {
    fn of(state: &ToolsState) -> Self {
        match state {
            ToolsState::Connected(_) | ToolsState::Unsupported | ToolsState::SingleChannel => {
                Link::Connected
            }
            ToolsState::Connecting => Link::Connecting,
            ToolsState::Reconnecting => Link::Reconnecting,
            ToolsState::Ended => Link::Ended,
        }
    }
}

/// 开着的小窗（每台主机最多一个）：主机的键、窗口、视图。
#[derive(Default)]
struct OpenWindows(Vec<(String, AnyWindowHandle, WeakEntity<OverviewWindow>)>);

impl Global for OpenWindows {}

/// 开着的小窗（窗口还在的）。
fn live(cx: &App) -> Vec<(String, AnyWindowHandle, Entity<OverviewWindow>)> {
    let windows = cx.windows();
    cx.try_global::<OpenWindows>()
        .map(|open| {
            open.0
                .iter()
                .filter(|(_, handle, _)| windows.contains(handle))
                .filter_map(|(host, handle, view)| {
                    view.upgrade().map(|v| (host.clone(), *handle, v))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 把 `target` 所连主机的概览弹出成小窗（`cache` 为主机工具面板各页的缓存：先显示上次读到的进程），`source` 为弹出
/// 它的窗口（第一次打开时放在它所在的屏幕上）。这台主机的小窗已经开着时把它拿到前面（不抢焦点）。返回小窗。
pub(super) fn pop_out(
    target: &ToolsTarget,
    cache: Rc<RefCell<ToolCache>>,
    source: &Window,
    cx: &mut App,
) -> Option<Entity<OverviewWindow>> {
    let host = host_key(target);
    let open = live(cx);
    if let Some((_, handle, view)) = open.iter().find(|(h, _, _)| *h == host) {
        tracing::debug!(host = %target.name, "主机概览的小窗已经开着，拿到前面");
        let pinned = view.update(cx, |view, _| {
            view.raised += 1;
            view.pinned
        });
        crate::topmost::apply(*handle, pinned, true, cx);
        return Some(view.clone());
    }
    target.terminal.upgrade()?;
    let saved = AppState::ui(cx)
        .read(cx)
        .state()
        .overview_window(&host)
        .cloned();
    let pinned = saved.as_ref().is_none_or(|s| s.pinned);
    let display = source.display(cx).map(|d| d.id());
    let scale = f32::from(crate::ui_scale::current_percent(cx)) / 100.;
    let placement = saved
        .and_then(|s| s.placement)
        .map(|p| scaled_placement(&p, scale));
    let (bounds, display) = initial_bounds(placement.as_ref(), display, open.len(), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        display_id: display,
        // 任务栏、Alt+Tab 中显示的标题
        titlebar: Some(TitlebarOptions {
            title: Some(t!("tools.popout.title", name = target.name.clone())),
            ..TitleBar::title_bar_options()
        }),
        // 不抢焦点：用户接着在终端中输入
        focus: false,
        // 窗口的最小尺寸是物理边界，不随界面缩放
        window_min_size: Some(size(px(MIN_SIZE.0), px(MIN_SIZE.1))),
        app_id: Some(crate::APP_ID.into()),
        ..TitleBar::window_options()
    };
    let opened = gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| OverviewWindow::new(host.clone(), target, cache, pinned, window, cx))
    });
    match opened {
        Ok((handle, view)) => {
            tracing::info!(host = %target.name, pinned, "弹出主机概览的小窗");
            let open = cx.default_global::<OpenWindows>();
            open.0.retain(|(_, _, view)| view.upgrade().is_some());
            open.0.push((host, handle, view.downgrade()));
            if pinned {
                crate::topmost::apply(handle, true, false, cx);
            }
            Some(view)
        }
        Err(e) => {
            tracing::error!("无法打开主机概览的小窗：{e}");
            None
        }
    }
}

/// 关闭全部小窗（主窗口都关了、隐藏到托盘时），关之前记下位置。
pub(crate) fn close_all(cx: &mut App) {
    let open = live(cx);
    if open.is_empty() {
        return;
    }
    tracing::debug!(count = open.len(), "关闭主机概览的小窗");
    for (_, handle, view) in open {
        let _ = handle.update(cx, |_, window, cx| {
            view.update(cx, |view, cx| view.remember(window, cx));
            window.remove_window();
        });
    }
}

/// 小窗打开的位置与大小：记下的按主窗口的规则放回；没有时放在 `display`（弹出它的窗口所在的屏幕）的右上角，
/// 已经开着 `open` 个小窗时依次错开（[`first_rect`]）。
fn initial_bounds(
    saved: Option<&WindowPlacement>,
    display: Option<DisplayId>,
    open: usize,
    cx: &App,
) -> (Bounds<Pixels>, Option<DisplayId>) {
    let screens = window_state::screens(cx);
    let restored = saved
        .and_then(|saved| window_state::restore_saved(saved, &screens, MIN_SIZE))
        .map(|restored| Restored {
            // 小窗不最大化打开
            maximized: false,
            ..restored
        })
        .unwrap_or_else(|| {
            let screen = display
                .and_then(|id| cx.displays().iter().position(|d| d.id() == id))
                .or_else(|| window_state::primary_screen(&screens));
            let scale = f32::from(crate::ui_scale::current_percent(cx)) / 100.;
            Restored {
                screen,
                rect: first_rect(screen.map(|i| &screens[i].visible), scale, open),
                maximized: false,
            }
        });
    window_state::window_bounds(&restored, cx)
}

/// 记下的位置换一种缩放倍数：大小乘上 `scale`，位置不变（记的时候除以当时的倍数，用的时候乘上现在的）。
pub(super) fn scaled_placement(placement: &WindowPlacement, scale: f32) -> WindowPlacement {
    WindowPlacement {
        width: placement.width * scale,
        height: placement.height * scale,
        ..placement.clone()
    }
}

/// 第一次打开的矩形：默认大小乘上界面的缩放倍数 `scale`（不超过 `visible`，即屏幕的可见区域），贴着可见区域的右上角
/// 留出边距；已经开着 `open` 个小窗时往左下错开，免得正好盖住。没有屏幕信息时放在原点。
pub(super) fn first_rect(visible: Option<&Rect>, scale: f32, open: usize) -> Rect {
    let (w, h) = (DEFAULT_SIZE.0 * scale, DEFAULT_SIZE.1 * scale);
    let Some(v) = visible else {
        return Rect { x: 0., y: 0., w, h };
    };
    let (w, h) = (w.min(v.w), h.min(v.h));
    let shift = CASCADE * (open % 8) as f32;
    Rect {
        x: (v.x + v.w - w - MARGIN - shift).max(v.x),
        y: (v.y + MARGIN + shift).min(v.y + v.h - h).max(v.y),
        w,
        h,
    }
}

/// 一台主机的概览小窗（窗口的根视图）。
pub struct OverviewWindow {
    /// 主机的键（[`host_key`]）：每台主机最多一个、记下位置用
    host: String,
    /// 主机名（标题）
    name: SharedString,
    terminal: WeakEntity<TerminalPanel>,
    page: Entity<OverviewPage>,
    /// 始终在其他窗口之上
    pinned: bool,
    link: Link,
    /// 窗口可见（没有最小化、没有被完全挡住）：可见时才让终端照常采样、读进程
    visible: bool,
    /// 让终端在不在前台时也照常采样（小窗可见时）
    hold: Option<SamplingHold>,
    /// 系统标题栏（任务栏、Alt+Tab）上的标题：换了界面语言时跟着改
    os_title: SharedString,
    /// 弹出时已经开着、被拿到前面的次数
    raised: usize,
    /// 界面的缩放倍数（相对 100%）：换了界面大小时窗口按比例跟着变
    scale: f32,
    _placement: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl OverviewWindow {
    fn new(
        host: String,
        target: &ToolsTarget,
        cache: Rc<RefCell<ToolCache>>,
        pinned: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // 与面板中的概览页读同样的数据，但不属于哪个面板：进程不能点，也就用不到面板
        let context = PageContext {
            panel: WeakEntity::new_invalid(),
            terminal: target.terminal.clone(),
            terminal_id: target.id,
            tools: target.tools.clone(),
            target: target.state.target().cloned(),
            ssh_host: target.ssh_host.clone(),
            cache,
            dialogs: OpenDialogs::default(),
        };
        let page = cx.new(|cx| OverviewPage::new(context, Layout::Window, window, cx));
        let mut subscriptions = vec![
            cx.observe_window_visibility(window, |this, visibility, _, cx| {
                this.visible = visibility.is_visible();
                this.sync(cx);
            }),
            // 移动、改大小停下片刻后记下位置
            cx.observe_window_bounds(window, |this, window, cx| {
                this._placement = Some(cx.spawn_in(window, async move |this, cx| {
                    cx.background_executor().timer(window_state::SETTLE).await;
                    let _ = this.update_in(cx, |this, window, cx| this.remember(window, cx));
                }));
            }),
        ];
        if let Some(terminal) = target.terminal.upgrade() {
            subscriptions.push(cx.observe(&terminal, |this, _, cx| this.sync(cx)));
            // 终端的 Tab 关了（含关闭它所在的主窗口）：小窗随之关闭
            subscriptions.push(
                cx.observe_release_in(&terminal, window, |this, _, window, cx| {
                    tracing::debug!(host = %this.name, "终端已关闭，关闭主机概览的小窗");
                    this.remember(window, cx);
                    window.remove_window();
                }),
            );
        }
        if let Some(lock) = crate::lock_ui::lock_model(cx) {
            subscriptions.push(cx.observe(&lock, |_, _, cx| cx.notify()));
        }
        // 点了关闭按钮：记下位置
        let this = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            let _ = this.update(cx, |this, cx| this.remember(window, cx));
            true
        });
        let os_title = t!("tools.popout.title", name = target.name.clone());
        let mut this = Self {
            host,
            name: target.name.clone(),
            terminal: target.terminal.clone(),
            page,
            pinned,
            link: Link::of(&target.state),
            visible: window.is_visible(),
            hold: None,
            os_title,
            raised: 0,
            scale: f32::from(crate::ui_scale::current_percent(cx)) / 100.,
            _placement: None,
            _subscriptions: subscriptions,
        };
        this.sync(cx);
        this
    }

    /// 按终端现在的情况更新：概览页的连接与是否读取（连着且小窗可见时），终端是否照常采样（小窗可见时；断开时终端
    /// 自己停止，重连后继续），显示的说明。终端每次通知都调用，没有变化时什么也不做。
    fn sync(&mut self, cx: &mut Context<Self>) {
        let Some(terminal) = self.terminal.upgrade() else {
            return;
        };
        let state = terminal.read(cx).tools_state();
        let link = Link::of(&state);
        let connection = state.target().cloned();
        let active = link == Link::Connected && self.visible;
        self.page.update(cx, |page, cx| {
            page.set_connection(connection, cx);
            page.set_active(active, cx);
        });
        if self.visible != self.hold.is_some() {
            self.hold = self.visible.then(|| terminal.read(cx).keep_sampling());
        }
        if self.link != link {
            tracing::debug!(host = %self.name, ?link, "主机概览的小窗：连接情况变了");
            self.link = link;
            cx.notify();
        }
    }

    /// 记下这台主机的小窗的位置、大小（按 100% 界面大小）与是否置顶（界面状态随后写回文件）。
    fn remember(&self, window: &Window, cx: &mut App) {
        let Some(placement) = window_state::placement_of(window, cx) else {
            return;
        };
        let scale = crate::ui_scale::factor(window);
        let saved = SavedWindow {
            host: self.host.clone(),
            placement: Some(scaled_placement(&placement, 1. / scale)),
            pinned: self.pinned,
        };
        AppState::ui(cx).update(cx, |ui, cx| {
            ui.update(|s| s.note_overview_window(saved), cx)
        });
    }

    /// 换了界面大小：窗口按比例跟着变（不超出屏幕），之后记下。
    fn rescale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let scale = crate::ui_scale::factor(window);
        if (scale - self.scale).abs() < 0.001 {
            return;
        }
        let ratio = scale / self.scale;
        self.scale = scale;
        let current = window.viewport_size();
        let mut width = f32::from(current.width) * ratio;
        let mut height = f32::from(current.height) * ratio;
        if let Some(display) = window.display(cx) {
            let visible = display.visible_bounds().size;
            width = width.min(f32::from(visible.width));
            height = height.min(f32::from(visible.height));
        }
        tracing::debug!(host = %self.name, scale, "主机概览的小窗：界面大小变了，窗口跟着变");
        window.resize(size(px(width), px(height)));
    }

    /// 断开时的“重新连接”：与终端的重新连接相同，在终端所在的主窗口中进行（询问在那里弹出）。
    fn reconnect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(terminal) = self.terminal.upgrade() else {
            return;
        };
        tracing::info!(host = %self.name, "主机概览的小窗：重新连接");
        match crate::workspace::window_of_terminal(terminal.entity_id(), cx) {
            Some(handle) => cx.defer(move |cx| {
                let _ = handle.update(cx, |_, window, cx| {
                    terminal.update(cx, |terminal, cx| terminal.ensure_connected(window, cx))
                });
            }),
            None => terminal.update(cx, |terminal, cx| terminal.ensure_connected(window, cx)),
        }
    }

    /// “重新连接”的按钮：终端现在点了会立即重连时才有（正在连接、重连中时没有）。
    fn reconnect_button(&self, cx: &mut Context<Self>) -> Option<Button> {
        let terminal = self.terminal.upgrade()?;
        terminal.read(cx).can_reconnect_now().then(|| {
            Button::new("overview-window-reconnect")
                .small()
                .primary()
                .label(t!("terminal.reconnect"))
                .on_click(cx.listener(|this, _, window, cx| this.reconnect(window, cx)))
        })
    }

    /// 标题栏上的图钉：取消、恢复置顶（记下）。
    fn toggle_pin(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pinned = !self.pinned;
        tracing::debug!(host = %self.name, pinned = self.pinned, "主机概览的小窗：切换置顶");
        self.remember(window, cx);
        crate::topmost::apply(window.window_handle(), self.pinned, false, cx);
        cx.notify();
    }

    fn render_title_bar(&self, title: SharedString, cx: &mut Context<Self>) -> impl IntoElement {
        let pin = crate::topmost::supported(cx.platform_info().os).then(|| {
            let tip = if self.pinned {
                t!("tools.popout.unpin")
            } else {
                t!("tools.popout.pin")
            };
            // 标题栏整条是拖动窗口的区域：按钮放进不拖动的容器（否则 Windows 上真实的点击会被当成拖动窗口）
            div().occlude().child(
                Button::new("overview-window-pin")
                    .ghost()
                    .small()
                    .icon(if self.pinned {
                        IconName::Pin
                    } else {
                        IconName::PinOff
                    })
                    .selected(self.pinned)
                    .tooltip(tip)
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_pin(window, cx))),
            )
        });
        // 标题放在绝对定位的一层里：标题栏的这一段不肯比内容窄（gpui-kit 的标题栏不收缩），长的主机名会把右边的
        // 窗口按钮挤出窗口；绝对定位的不算进内容的宽度，放不下时截断
        crate::ui_scale::title_bar().child(
            h_flex()
                .w_full()
                .h_full()
                .gap_1()
                .pr_1()
                .child(
                    div().relative().flex_1().min_w_0().h_full().child(
                        h_flex()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left_0()
                            .right_0()
                            .child(
                                div()
                                    .id("overview-window-title")
                                    .test_support()
                                    .w_full()
                                    .truncate()
                                    .text_sm()
                                    .font_medium()
                                    .child(title),
                            ),
                    ),
                )
                .children(pin),
        )
    }

    /// 内容：连着时是概览页，否则说明情况；应用锁定时什么也不显示。
    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        if crate::lock_ui::is_locked(cx) {
            return OverviewPage::notice(
                "locked",
                t!("lock_ui.screen.title"),
                t!("tools.popout.locked_hint"),
                None,
                cx,
            );
        }
        match self.link {
            Link::Connected => self.page.clone().into_any_element(),
            Link::Connecting => OverviewPage::notice(
                "connecting",
                t!("tools.empty.connecting", name = self.name.clone()),
                t!("tools.popout.connecting_hint"),
                None,
                cx,
            ),
            Link::Reconnecting => OverviewPage::notice(
                "disconnected",
                t!("tools.popout.disconnected"),
                t!("tools.popout.reconnecting_hint"),
                self.reconnect_button(cx),
                cx,
            ),
            Link::Ended => OverviewPage::notice(
                "ended",
                t!("tools.popout.disconnected"),
                t!("tools.popout.ended_hint"),
                self.reconnect_button(cx),
                cx,
            ),
        }
    }
}

impl Render for OverviewWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = t!("tools.popout.title", name = self.name.clone());
        if title != self.os_title {
            window.set_window_title(&title);
            self.os_title = title.clone();
        }
        // 换了界面大小：画完这一帧再改窗口的大小
        if (crate::ui_scale::factor(window) - self.scale).abs() >= 0.001 {
            cx.defer_in(window, |this, window, cx| this.rescale(window, cx));
        }
        let theme = cx.theme();
        let (background, foreground) = (theme.background, theme.foreground);
        v_flex()
            .id("overview-window")
            .test_support()
            .size_full()
            .bg(background)
            .text_color(foreground)
            .child(self.render_title_bar(title, cx))
            .child(div().flex_1().min_h_0().child(self.render_body(cx)))
    }
}

#[cfg(test)]
impl OverviewWindow {
    /// 窗口的标题（任务栏、Alt+Tab 中显示的）。
    pub(super) fn title(&self) -> SharedString {
        self.os_title.clone()
    }

    pub(super) fn is_pinned(&self) -> bool {
        self.pinned
    }

    pub(super) fn raised(&self) -> usize {
        self.raised
    }

    pub(super) fn terminal_id(&self) -> Option<gpui_kit::EntityId> {
        self.terminal.upgrade().map(|t| t.entity_id())
    }

    pub(super) fn is_sampling(&self) -> bool {
        self.hold.is_some()
    }
}

/// 开着的小窗：主机的键与窗口（测试看）。
#[cfg(test)]
pub(super) fn open_windows(cx: &App) -> Vec<(String, AnyWindowHandle, Entity<OverviewWindow>)> {
    live(cx)
}
