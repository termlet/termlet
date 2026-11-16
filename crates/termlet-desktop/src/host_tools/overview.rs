//! 概览页（第一页）：跟随的终端所连主机的资源——系统与内核、运行时间，CPU（总体、核数、负载、iowait、steal；每核的
//! 使用率是一排细竖条，64 核也只占一行，偏载一眼看得出，点开看每核的数值）、CPU 占用最高的进程（点了跳到进程页并
//! 选中）、内存与交换区、网络（默认不列虚拟网卡与没有收发过的网卡，可展开全部）、各磁盘（按占用排序，带 inode）。
//! 曲线有 25%、50%、75% 的浅网格线，鼠标停在上面时写出那一刻的时间与读数（见 [`super::chart`]）；曲线的说明在 CPU
//! 标题旁 ⓘ 的悬停提示中。
//!
//! - CPU、内存、网络、磁盘取自终端自己的资源监控（状态条用的同一份采样，每 3 秒一次；终端不在前台时不采样），这一页
//!   不为它们另外执行命令；CPU、内存、网速最近约 10 分钟的曲线来自终端记下的历史，没有采样的时段如实留空。着色与状态条
//!   一致（终端记下的级别，带回差）。
//! - 这一页显示时另读系统与内核、inode 与进程列表（[`OverviewData`]，进程列表与进程页是同一个读取），每 [`INTERVAL`]
//!   一次；不显示时不读。进程的 CPU 是与上一次读数之差，上一次读数太旧（超过两个间隔）时不算；这时与第一次读取
//!   一样没有 CPU，稍后补读一次（与进程页相同）。
//! - 设置中关闭了资源监控、服务器不是 Linux、服务器不让另开通道时说明原因。
//! - 键盘（见 [`super::keys`]）：这一页有焦点时 ↑ ↓ 等在 CPU 最高的进程之间移动（页滚到它们那里），Enter 跳到进程页。
//! - 面板中每一节是一张卡片（[`section`]）。
//! - 右上角的按钮把这台主机的概览弹出成置顶的小窗（[`super::popout`]）。小窗中是同一个页（[`Layout::Window`]）的
//!   紧凑版式（不分卡片，只用间距）：CPU、内存、网速的矮曲线与当前值，最满的磁盘，CPU 最高的 3 个进程；窗口够高时
//!   另显示系统与运行时间、CPU 的核数与负载、每核的竖条、交换区、网速的图例、全部磁盘与 5 个进程，够宽时进程行另显示
//!   PID 与内存（[`Fit`]）。小窗中的进程不能点、没有键盘操作；只在采样变了时重画（终端每次输出都会通知）。

use std::mem::{Discriminant, discriminant};
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::assets::IconName;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, ClickEvent, Context, Div, FocusHandle, FontFeatures, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, TestSupportExt as _, Window,
    div, prelude::FluentBuilder as _, relative, rems,
};
use termlet_core::hosttools::{ExecError, Target, ToolsError};
use termlet_core::monitor::{Details, Sample};

use super::chart::{CHART_HEIGHT, COMPACT_CHART_HEIGHT, OnHover, Series, chart};
use super::errors;
use super::keys::{
    LIST_CONTEXT, OpenItem, SelectFirst, SelectLast, SelectNext, SelectPageDown, SelectPageUp,
    SelectPrev,
};
use super::model::{OverviewData, Step, TOP_PROCESSES, format_cpu, step_row};
use super::shared::{self, Feed, PageContext, ToolPage, loading_text};
use crate::app_state::AppState;
use crate::monitor_bar::{Resources, level_color};
use crate::monitor_model::{
    History, Level, Levels, PERCENT, disk_percent, disks_by_usage, format_rate, format_size,
    format_uptime, percent, rate_ceiling, segments, shown_ifaces, whole_percent,
};
use crate::prose::prose;
use crate::sftp::list_model::visible_name;
use crate::t;
use crate::terminal_panel::TerminalPanel;

/// 这一页显示时另读补充信息与进程列表的间隔（上一次的进程读数比它的两倍还早时不拿来算 CPU）。
pub(super) const INTERVAL: Duration = Duration::from_secs(10);
/// CPU 占用最高的进程是第几节（系统、CPU 之后）。
const TOP_SECTION: usize = 2;
/// 主机概览小窗中列出的 CPU 最高的进程数（窗口够高时列出全部 [`super::model::TOP_PROCESSES`] 个）。
const COMPACT_TOP: usize = 3;

/// 概览显示在哪里。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Layout {
    /// 主机工具面板的一页
    Panel,
    /// 主机概览的小窗（[`super::popout`]）：紧凑的版式，按窗口的大小多显示一些（[`Fit`]）
    Window,
}

/// 主机概览小窗按窗口的大小显示多少。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Fit {
    /// 窗口够高：另显示系统与运行时间、CPU 的核数与负载、每核的竖条、交换区、网速的图例、全部磁盘与全部进程
    pub tall: bool,
    /// 窗口够宽：进程行另显示 PID 与内存
    pub wide: bool,
}

impl Fit {
    /// 够高、够宽的界限（rem，含标题栏）：界面放大后 rem 跟着变大，同样大的窗口显示得少一些。
    pub const TALL: f32 = 38.;
    pub const WIDE: f32 = 24.;

    pub fn of(window: &Window) -> Self {
        let size = window.viewport_size();
        let rem = window.rem_size();
        Self {
            tall: size.height / rem >= Self::TALL,
            wide: size.width / rem >= Self::WIDE,
        }
    }
}

/// 终端的资源采样到了哪一次（采样的情况、有没有读数、最近一次采样的时间）：小窗只在它变了时重画。
type SamplingMark = (Discriminant<Resources>, bool, Option<i64>);

fn sampling_mark(terminal: &TerminalPanel) -> SamplingMark {
    let resources = terminal.resources();
    (
        discriminant(resources),
        matches!(resources, Resources::Sampling(Some(_))),
        terminal.monitor_history().last_at(),
    )
}

pub(super) struct OverviewPage {
    context: PageContext,
    layout: Layout,
    /// 小窗上次画出时的采样（[`Layout::Window`]）
    mark: Option<SamplingMark>,
    feed: Feed<OverviewData>,
    /// 展开了每核使用率
    show_cores: bool,
    /// 列出全部网卡（含虚拟网卡与没有收发过的）
    all_ifaces: bool,
    cpu_retried: bool,
    /// 整页的焦点（键盘在 CPU 最高的进程之间移动）与滚动
    focus: FocusHandle,
    scroll: ScrollHandle,
    /// CPU 最高的进程中的当前行
    cursor: Option<u32>,
    /// 鼠标停在哪条曲线上（曲线的 ID）与指针下的时刻
    chart_hover: Option<(&'static str, i64)>,
    _cpu_retry: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ToolPage for OverviewPage {
    type Data = OverviewData;

    fn feed(&mut self) -> &mut Feed<OverviewData> {
        &mut self.feed
    }

    fn context(&self) -> &PageContext {
        &self.context
    }

    fn after_load(&mut self, cx: &mut Context<Self>) {
        self.retry_cpu(cx);
        self.sync_poll(cx);
    }
}

impl OverviewPage {
    pub fn new(
        context: PageContext,
        layout: Layout,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // 终端每次采样后重画（采样结果记在终端上）；小窗只在采样变了时重画，不跟着终端的每次输出
        let subscriptions = context
            .terminal
            .upgrade()
            .map(|terminal| {
                cx.observe(&terminal, |this, terminal, cx| {
                    if this.layout == Layout::Window {
                        let mark = sampling_mark(terminal.read(cx));
                        if this.mark == Some(mark) {
                            return;
                        }
                        this.mark = Some(mark);
                    }
                    cx.notify()
                })
            })
            .into_iter()
            .collect();
        Self {
            feed: Feed::new(&context),
            context,
            layout,
            mark: None,
            show_cores: false,
            all_ifaces: false,
            cpu_retried: false,
            focus: shared::list_focus(cx),
            scroll: ScrollHandle::new(),
            cursor: None,
            chart_hover: None,
            _cpu_retry: None,
            _subscriptions: subscriptions,
        }
    }

    /// 整页的焦点（面板切到这一页、“打开主机工具的概览”时放进来）。
    pub fn list_focus(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// ↑ ↓ 等：在 CPU 最高的进程之间移动（页滚到那一行）。
    fn move_cursor(&mut self, step: Step, cx: &mut Context<Self>) {
        let Some(data) = self.feed.data() else {
            return;
        };
        let current = self
            .cursor
            .and_then(|pid| data.top.iter().position(|p| p.pid == pid));
        let Some(ix) = step_row(current, data.top.len(), step, data.top.len(), |_| true) else {
            return;
        };
        self.cursor = Some(data.top[ix].pid);
        self.scroll.scroll_to_item(TOP_SECTION);
        cx.notify();
    }

    /// 曲线的悬停：告知指针下的时刻（移出时为 `None`）。
    fn on_chart_hover(&self, id: &'static str, cx: &Context<Self>) -> OnHover {
        let page = cx.entity().downgrade();
        Rc::new(move |at, _, cx| {
            let _ = page.update(cx, |page, cx| {
                let hover = at.map(|at| (id, at));
                let changed = match (page.chart_hover, hover) {
                    (Some((a, _)), None) => a == id,
                    (old, new) => old != new,
                };
                if changed {
                    page.chart_hover = hover;
                    cx.notify();
                }
            });
        })
    }

    /// 指针下的时刻（鼠标停在 `id` 这条曲线上时）。
    fn hover_at(&self, id: &str) -> Option<i64> {
        self.chart_hover
            .filter(|(of, _)| *of == id)
            .map(|(_, at)| at)
    }

    /// Enter：跳到当前行的进程；没有当前行时到第一行。
    fn open_item(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let listed = self.cursor.filter(|pid| {
            self.feed
                .data()
                .is_some_and(|d| d.top.iter().any(|p| p.pid == *pid))
        });
        match listed {
            Some(pid) => self.show_process(pid, window, cx),
            None => self.move_cursor(Step::First, cx),
        }
    }

    pub fn set_connection(&mut self, target: Option<Target>, _: &mut Context<Self>) {
        self.context.target = target;
    }

    pub fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if shared::activate(self, active, cx) && active {
            self.cpu_retried = false;
        }
        if !active {
            self._cpu_retry = None;
        }
    }

    /// 定时读取：显示中时进行（第一次读完之后开始）；不支持的主机不再读。
    fn sync_poll(&mut self, cx: &mut Context<Self>) {
        let unsupported = matches!(
            self.feed.error,
            Some(ToolsError::Unsupported | ToolsError::Exec(ExecError::SingleChannel))
        );
        if unsupported {
            shared::stop_poll(self);
        } else if self.feed.active && !self.feed.is_polling() && !self.feed.loading {
            shared::start_poll(self, INTERVAL, cx);
        }
    }

    /// 读到的进程都没有 CPU（第一次读取，或上一次读数太旧、不拿来算）：稍后补读一次，按最近这一小段时间算；
    /// 补读之后仍然没有时不再补读，等读到 CPU 之后才重新计（与进程页相同）。
    fn retry_cpu(&mut self, cx: &mut Context<Self>) {
        if !self.feed.data().is_some_and(|data| data.cpu_pending) {
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

    /// 把这台主机的概览弹出成置顶的小窗（已经开着时拿到前面）。
    fn pop_out(&self, window: &mut Window, cx: &mut Context<Self>) {
        shared::go(&self.context, window, cx, |panel, window, cx| {
            panel.pop_out_overview(window, cx)
        });
    }

    /// 跳到进程页并选中（这一页随之停止显示：等这次更新结束后再切换）。
    fn show_process(&self, pid: u32, window: &mut Window, cx: &mut Context<Self>) {
        shared::go(&self.context, window, cx, move |panel, window, cx| {
            panel.show_process(pid, window, cx)
        });
    }

    // ------------------------------------------------------------ 渲染

    /// 不能显示资源时的说明（与其他页不可用时的说明同样的版式；主机概览小窗的说明也用它）。
    pub(super) fn notice(
        kind: &'static str,
        title: SharedString,
        body: SharedString,
        action: Option<Button>,
        cx: &App,
    ) -> AnyElement {
        let theme = cx.theme();
        v_flex()
            .id(SharedString::from(format!("tools-overview-{kind}")))
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
                    .text_color(theme.muted_foreground)
                    .line_height(relative(1.5))
                    .child(prose(body)),
            )
            .children(action.map(|button| h_flex().child(button)))
            .into_any_element()
    }

    /// 系统与内核、运行时间；面板中右边是弹出小窗的按钮。
    fn render_system(
        &self,
        sample: &Sample,
        details: Option<&Details>,
        cx: &mut Context<Self>,
    ) -> Div {
        let muted = cx.theme().muted_foreground;
        let title = details
            .and_then(|d| d.system.clone().or_else(|| d.kernel.clone()))
            .unwrap_or_else(|| "Linux".into());
        let mut parts: Vec<SharedString> = Vec::new();
        if let Some(details) = details {
            parts.extend(
                details
                    .kernel
                    .as_ref()
                    .filter(|k| Some(*k) != details.system.as_ref())
                    .map(|k| visible_name(k).into()),
            );
            parts.extend(details.arch.as_ref().map(|a| visible_name(a).into()));
        }
        parts.push(format_uptime(sample.uptime));
        let info = v_flex()
            .flex_1()
            .min_w_0()
            .gap_0p5()
            .child(
                div()
                    .id("tools-overview-system")
                    .test_support()
                    .font_medium()
                    .text_color(cx.theme().foreground)
                    .child(visible_name(&title)),
            )
            .child(facts("tools-overview-facts", parts, muted));
        h_flex()
            .items_start()
            .gap_2()
            .child(info)
            .when(self.layout == Layout::Panel, |el| {
                el.child(
                    Button::new("tools-overview-popout")
                        .ghost()
                        .xsmall()
                        .icon(IconName::PictureInPicture2)
                        .tooltip(t!("tools.popout.open"))
                        .on_click(cx.listener(|this, _, window, cx| this.pop_out(window, cx))),
                )
            })
    }

    /// CPU；`fit` 为主机概览小窗的大小（面板中为 `None`）。
    fn render_cpu(
        &self,
        sample: &Sample,
        levels: Levels,
        history: &History,
        fit: Option<Fit>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let panel = fit.is_none();
        let tall = fit.is_none_or(|f| f.tall);
        let muted = cx.theme().muted_foreground;
        let [a, b, c] = sample.load.map(|v| format!("{v:.2}"));
        let mut cpu_facts = vec![
            if sample.cores == 1 {
                t!("tools.overview.cores_one")
            } else {
                t!("tools.overview.cores_count", cores = sample.cores)
            },
            t!("tools.overview.load", a = a, b = b, c = c),
        ];
        if let (Some(iowait), Some(steal)) = (sample.iowait, sample.steal) {
            cpu_facts.push(t!("tools.overview.iowait", value = format!("{iowait:.0}")));
            cpu_facts.push(t!("tools.overview.steal", value = format!("{steal:.0}")));
        }
        let cores = sample.per_core.clone();
        let show_cores = panel && self.show_cores && !cores.is_empty();
        // 标题旁的 ⓘ：曲线的说明（悬停看）
        let info = div()
            .id("tools-overview-history-hint")
            .test_support()
            .flex()
            .child(
                Icon::new(IconName::Info)
                    .xsmall()
                    .text_color(muted.opacity(0.8)),
            )
            .tooltip(|window, cx| {
                Tooltip::new(t!("tools.overview.history_hint")).build(window, cx)
            });
        let toggle_cores = cx.listener(move |this: &mut Self, _: &ClickEvent, _, cx| {
            if panel {
                this.show_cores = !this.show_cores;
                cx.notify();
            }
        });
        v_flex()
            .id("tools-overview-cpu")
            .test_support()
            .gap_1()
            .child(heading_with(
                "CPU".into(),
                Some(info.into_any_element()),
                Some(value_text(whole_percent(sample.cpu), levels.cpu, cx)),
                cx,
            ))
            .when(panel, |el| {
                el.child(meter(sample.cpu.unwrap_or(0.), levels.cpu, cx))
            })
            .when(tall, |el| {
                el.child(facts("tools-overview-cpu-facts", cpu_facts, muted))
            })
            .child(history_chart(
                "tools-overview-cpu-chart",
                chart_height(fit),
                vec![Series {
                    segments: segments(history.points(), |p| p.cpu),
                    color: cx.theme().chart_2,
                }],
                history,
                100.,
                "100%".into(),
                self.hover_at("tools-overview-cpu-chart"),
                |p| whole_percent(p.cpu).into(),
                self.on_chart_hover("tools-overview-cpu-chart", cx),
                cx,
            ))
            // 每核：一排细竖条（面板中点了展开每核的数值）
            .when(tall && cores.len() > 1, |el| {
                el.child(core_bars(&cores, toggle_cores, cx))
            })
            .when(panel && !cores.is_empty(), |el| {
                el.child(
                    h_flex().child(
                        Button::new("tools-overview-cores")
                            .ghost()
                            .xsmall()
                            .icon(if show_cores {
                                IconName::ChevronUp
                            } else {
                                IconName::ChevronDown
                            })
                            .label(t!("tools.overview.cores"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_cores = !this.show_cores;
                                cx.notify();
                            })),
                    ),
                )
            })
            .when(show_cores, |el| {
                el.child(div().grid().grid_cols(2).gap_x_3().gap_y_1().children(
                    cores.iter().enumerate().map(|(ix, usage)| {
                        let level = Level::of(*usage, PERCENT);
                        // 核的编号就是它在 /proc/stat 中的名称 cpuN
                        h_flex()
                            .id(("tools-overview-core", ix))
                            .test_support()
                            .gap_1p5()
                            .child(
                                div()
                                    .flex_none()
                                    .w(rems(2.5))
                                    .whitespace_nowrap()
                                    .text_color(muted)
                                    .child(format!("cpu{ix}")),
                            )
                            .child(div().flex_1().min_w_0().child(meter(*usage, level, cx)))
                            .child(
                                div()
                                    .flex_none()
                                    .min_w(rems(2.))
                                    .text_right()
                                    .font_features(tabular())
                                    .child(value_text(whole_percent(Some(*usage)), level, cx)),
                            )
                    }),
                ))
            })
    }

    fn render_memory(
        &self,
        sample: &Sample,
        levels: Levels,
        history: &History,
        fit: Option<Fit>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let tall = fit.is_none_or(|f| f.tall);
        let muted = cx.theme().muted_foreground;
        let used = percent(sample.mem_used, sample.mem_total);
        v_flex()
            .id("tools-overview-memory")
            .test_support()
            .gap_1()
            .child(heading(
                t!("monitor.detail_memory"),
                Some(value_text(
                    t!(
                        "monitor.detail_used",
                        used = format_size(sample.mem_used as f64),
                        total = format_size(sample.mem_total as f64),
                        percent = format!("{used:.0}")
                    )
                    .to_string(),
                    levels.memory,
                    cx,
                )),
                cx,
            ))
            .when(fit.is_none(), |el| el.child(meter(used, levels.memory, cx)))
            .when(tall && sample.swap_total > 0, |el| {
                el.child(facts(
                    "tools-overview-swap",
                    vec![t!(
                        "monitor.detail_swap",
                        used = format_size(sample.swap_used as f64),
                        total = format_size(sample.swap_total as f64)
                    )],
                    muted,
                ))
            })
            .child(history_chart(
                "tools-overview-memory-chart",
                chart_height(fit),
                vec![Series {
                    segments: segments(history.points(), |p| Some(p.memory)),
                    color: cx.theme().chart_2,
                }],
                history,
                100.,
                "100%".into(),
                self.hover_at("tools-overview-memory-chart"),
                |p| whole_percent(Some(p.memory)).into(),
                self.on_chart_hover("tools-overview-memory-chart", cx),
                cx,
            ))
    }

    fn render_net(
        &self,
        sample: &Sample,
        history: &History,
        fit: Option<Fit>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let panel = fit.is_none();
        let tall = fit.is_none_or(|f| f.tall);
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let (rx_color, tx_color) = (theme.chart_2, theme.chart_1);
        let peak = history
            .points()
            .iter()
            .flat_map(|p| [p.rx, p.tx])
            .flatten()
            .fold(0., f64::max);
        let max = rate_ceiling(peak);
        let (ifaces, hidden) = if panel {
            shown_ifaces(&sample.net, self.all_ifaces)
        } else {
            (Vec::new(), 0)
        };
        let legend = |color: Hsla, label: SharedString| {
            h_flex()
                .whitespace_nowrap()
                .gap_1()
                .child(div().size_1p5().rounded_full().bg(color))
                .child(label)
        };
        v_flex()
            .id("tools-overview-net")
            .test_support()
            .gap_1()
            .child(heading(
                t!("monitor.detail_net"),
                Some(
                    div()
                        .flex_none()
                        .whitespace_nowrap()
                        .font_features(tabular())
                        .child(t!(
                            "monitor.net",
                            rx = format_rate(sample.rx_rate),
                            tx = format_rate(sample.tx_rate)
                        ))
                        .into_any_element(),
                ),
                cx,
            ))
            .when(tall, |el| {
                el.child(
                    h_flex()
                        .gap_3()
                        .text_color(muted)
                        .child(legend(rx_color, t!("tools.overview.download")))
                        .child(legend(tx_color, t!("tools.overview.upload"))),
                )
            })
            .child(history_chart(
                "tools-overview-net-chart",
                chart_height(fit),
                vec![
                    Series {
                        segments: segments(history.points(), |p| p.rx),
                        color: rx_color,
                    },
                    Series {
                        segments: segments(history.points(), |p| p.tx),
                        color: tx_color,
                    },
                ],
                history,
                max,
                format_rate(Some(max)).into(),
                self.hover_at("tools-overview-net-chart"),
                |p| {
                    t!(
                        "monitor.net",
                        rx = format_rate(p.rx),
                        tx = format_rate(p.tx)
                    )
                },
                self.on_chart_hover("tools-overview-net-chart", cx),
                cx,
            ))
            .children(ifaces.into_iter().map(|iface| {
                h_flex()
                    .id(SharedString::from(format!(
                        "tools-overview-iface-{}",
                        iface.name
                    )))
                    .test_support()
                    .justify_between()
                    .gap_2()
                    .child(div().min_w_0().truncate().child(visible_name(&iface.name)))
                    .child(
                        div()
                            .flex_none()
                            .whitespace_nowrap()
                            .text_color(muted)
                            .font_features(tabular())
                            .child(t!(
                                "monitor.net",
                                rx = format_rate(iface.rx_rate),
                                tx = format_rate(iface.tx_rate)
                            )),
                    )
            }))
            .when(panel && (hidden > 0 || self.all_ifaces), |el| {
                let label = if self.all_ifaces {
                    t!("tools.overview.ifaces_physical")
                } else if sample.net.len() == 1 {
                    t!("tools.overview.ifaces_all_one")
                } else {
                    t!("tools.overview.ifaces_all", count = sample.net.len())
                };
                el.child(
                    h_flex().child(
                        Button::new("tools-overview-ifaces")
                            .ghost()
                            .xsmall()
                            .label(label)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.all_ifaces = !this.all_ifaces;
                                cx.notify();
                            })),
                    ),
                )
            })
    }

    /// 各磁盘（按占用排序）；主机概览小窗中只有最满的那块，窗口够高时全部。
    fn render_disks(
        &self,
        sample: &Sample,
        details: Option<&Details>,
        fit: Option<Fit>,
        cx: &App,
    ) -> Option<impl IntoElement> {
        if sample.disks.is_empty() {
            return None;
        }
        let tall = fit.is_none_or(|f| f.tall);
        let mut disks = disks_by_usage(&sample.disks);
        if !tall {
            disks.truncate(1);
        }
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let mono = theme.mono_font_family.clone();
        Some(
            v_flex()
                .id("tools-overview-disks")
                .test_support()
                .gap_1p5()
                .when(tall, |el| {
                    el.child(heading(t!("monitor.detail_disks"), None, cx))
                })
                .children(disks.into_iter().map(|disk| {
                    let used = disk_percent(disk);
                    let level = Level::of(used, PERCENT);
                    let inodes = details
                        .filter(|_| tall)
                        .and_then(|d| d.inodes.iter().find(|i| i.mount == disk.mount))
                        .map(|i| percent(i.used, i.total));
                    v_flex()
                        .id(SharedString::from(format!(
                            "tools-overview-disk-{}",
                            disk.mount
                        )))
                        .test_support()
                        .gap_0p5()
                        .child(
                            h_flex()
                                .justify_between()
                                .gap_2()
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .font_family(mono.clone())
                                        .child(visible_name(&disk.mount)),
                                )
                                .child(
                                    div().flex_none().child(value_text(
                                        t!(
                                            "monitor.detail_used",
                                            used = format_size(disk.used as f64),
                                            total = format_size(disk.total as f64),
                                            percent = format!("{used:.0}")
                                        )
                                        .to_string(),
                                        level,
                                        cx,
                                    )),
                                ),
                        )
                        .child(meter(used, level, cx))
                        .when_some(inodes, |el, inodes| {
                            let level = Level::of(inodes, PERCENT);
                            el.child(
                                div()
                                    .whitespace_nowrap()
                                    .text_color(level_color(level, cx).unwrap_or(muted))
                                    .child(t!(
                                        "tools.overview.inodes",
                                        percent = format!("{inodes:.0}")
                                    )),
                            )
                        })
                })),
        )
    }

    /// CPU 占用最高的进程；主机概览小窗中列出 [`COMPACT_TOP`] 个（窗口够高时全部），不能点，窗口够宽时才有 PID 与
    /// 内存。
    fn render_top(
        &self,
        window: &Window,
        fit: Option<Fit>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let panel = fit.is_none();
        let wide = fit.is_none_or(|f| f.wide);
        let count = match fit {
            Some(fit) if !fit.tall => COMPACT_TOP,
            _ => TOP_PROCESSES,
        };
        let focused = panel && self.focus.is_focused(window);
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let hover = theme.list_hover;
        let ring = theme.ring;
        let updated = self
            .feed
            .shown
            .at
            .map(|at| t!("tools.updated_at", time = super::model::clock_time(at)));
        let body: AnyElement = match (self.feed.data(), &self.feed.error) {
            (Some(data), _) if data.cpu_pending => div()
                .text_color(muted)
                .child(prose(t!("tools.overview.top_pending")))
                .into_any_element(),
            (Some(data), _) => v_flex()
                .children(data.top.iter().take(count).map(|process| {
                    let pid = process.pid;
                    let cursor = focused && self.cursor == Some(pid);
                    h_flex()
                        .id(("tools-overview-proc", pid as usize))
                        .test_support()
                        .gap_2()
                        .px_1()
                        .py_0p5()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(if cursor { ring } else { theme.transparent })
                        .when(panel, |el| el.hover(|s| s.bg(hover)))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(visible_name(&process.name)),
                        )
                        .when(wide, |el| {
                            el.child(
                                div()
                                    .flex_none()
                                    .whitespace_nowrap()
                                    .text_color(muted)
                                    .font_features(tabular())
                                    .child(pid.to_string()),
                            )
                        })
                        .child(
                            div()
                                .flex_none()
                                .min_w(rems(3.))
                                .whitespace_nowrap()
                                .text_right()
                                .font_features(tabular())
                                .child(format_cpu(process.cpu)),
                        )
                        .when(wide, |el| {
                            el.child(
                                div()
                                    .flex_none()
                                    .min_w(rems(4.5))
                                    .whitespace_nowrap()
                                    .text_right()
                                    .text_color(muted)
                                    .font_features(tabular())
                                    .child(format_size(process.rss as f64)),
                            )
                        })
                        .when(panel, |el| {
                            el.on_click(cx.listener(move |this, _, window, cx| {
                                this.show_process(pid, window, cx)
                            }))
                        })
                }))
                .when(panel, |el| {
                    el.child(
                        div()
                            .pt_0p5()
                            .px_1()
                            .text_color(muted.opacity(0.8))
                            .child(prose(t!("tools.overview.top_hint"))),
                    )
                })
                .into_any_element(),
            (None, Some(error)) => div()
                .text_color(theme.danger)
                .child(prose(errors::describe(error)))
                .into_any_element(),
            (None, None) => div()
                .text_color(muted)
                .child(prose(t!("tools.loading")))
                .into_any_element(),
        };
        v_flex()
            .id("tools-overview-top")
            .test_support()
            .gap_1()
            .child(heading(
                t!("tools.overview.top"),
                updated.map(|text| {
                    div()
                        .flex_none()
                        .whitespace_nowrap()
                        .text_color(muted)
                        .child(text)
                        .into_any_element()
                }),
                cx,
            ))
            .when(
                self.feed.data().is_some() && self.feed.error.is_some(),
                |el| el.children(shared::error_line(self.feed.error.as_ref(), cx)),
            )
            .child(body)
    }
}

impl Render for OverviewPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = self.render_body(window, cx);
        // 主机概览的小窗中进程不能点，也没有键盘操作
        if self.layout == Layout::Window {
            return div().id("tools-overview-page").size_full().child(body);
        }
        div()
            .id("tools-overview-page")
            .size_full()
            .key_context(LIST_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &SelectPrev, _, cx| this.move_cursor(Step::Prev, cx)))
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.move_cursor(Step::Next, cx)))
            .on_action(
                cx.listener(|this, _: &SelectPageUp, _, cx| this.move_cursor(Step::First, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectPageDown, _, cx| this.move_cursor(Step::Last, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectFirst, _, cx| this.move_cursor(Step::First, cx)),
            )
            .on_action(cx.listener(|this, _: &SelectLast, _, cx| this.move_cursor(Step::Last, cx)))
            .on_action(cx.listener(|this, _: &OpenItem, window, cx| this.open_item(window, cx)))
            .child(body)
    }
}

impl OverviewPage {
    fn render_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(terminal) = self.context.terminal.upgrade() else {
            return loading_text(cx);
        };
        let (resources, levels, history) = {
            let terminal = terminal.read(cx);
            (
                terminal.resources().clone(),
                terminal.monitor_levels(),
                terminal.monitor_history().clone(),
            )
        };
        let sample = match resources {
            Resources::Sampling(Some(sample)) => sample,
            Resources::Unsupported => {
                return Self::notice(
                    "unsupported",
                    t!("tools.overview.unsupported"),
                    t!("tools.overview.unsupported_hint"),
                    None,
                    cx,
                );
            }
            Resources::SingleChannel => {
                return Self::notice(
                    "single_channel",
                    t!("tools.unavailable.single_channel"),
                    t!("tools.unavailable_hint.single_channel"),
                    None,
                    cx,
                );
            }
            Resources::Off if !AppState::store(cx).settings().terminal.monitor => {
                let turn_on = Button::new("tools-overview-monitor-on")
                    .outline()
                    .xsmall()
                    .label(t!("tools.overview.monitor_on"))
                    .on_click(|_, _, cx| {
                        if let Err(e) =
                            AppState::store(cx).update_settings(|s| s.terminal.monitor = true)
                        {
                            tracing::warn!("打开资源监控失败：{e}");
                        }
                    });
                return Self::notice(
                    "monitor_off",
                    t!("tools.overview.monitor_off"),
                    t!("tools.overview.monitor_off_hint"),
                    Some(turn_on),
                    cx,
                );
            }
            _ => return loading_text(cx),
        };
        let data = self.feed.data();
        let details = data.as_ref().and_then(|d| d.details.as_ref());
        let content = match self.layout {
            // 各节是一张卡片，是滚动区域的直接子元素：键盘移到 CPU 最高的进程时滚到那一节（[`TOP_SECTION`]）
            Layout::Panel => {
                let sections = [
                    self.render_system(&sample, details, cx).into_any_element(),
                    self.render_cpu(&sample, levels, &history, None, cx)
                        .into_any_element(),
                    self.render_top(window, None, cx).into_any_element(),
                    self.render_memory(&sample, levels, &history, None, cx)
                        .into_any_element(),
                    self.render_net(&sample, &history, None, cx)
                        .into_any_element(),
                ]
                .into_iter()
                .chain(
                    self.render_disks(&sample, details, None, cx)
                        .map(IntoElement::into_any_element),
                )
                .collect::<Vec<_>>();
                v_flex()
                    .px(rems(shared::CARD_GAP))
                    .pt_1()
                    .pb(rems(shared::CARD_GAP))
                    .gap(rems(shared::CARD_GAP))
                    .children(sections.into_iter().map(|content| section(content, cx)))
            }
            // 小窗：曲线在前，进程在最后；窗口放不下时整个滚动
            Layout::Window => {
                let fit = Fit::of(window);
                v_flex()
                    .px_3()
                    .pt_2()
                    .pb_3()
                    .gap_3()
                    .when(fit.tall, |el| {
                        el.child(self.render_system(&sample, details, cx))
                    })
                    .child(self.render_cpu(&sample, levels, &history, Some(fit), cx))
                    .child(self.render_memory(&sample, levels, &history, Some(fit), cx))
                    .child(self.render_net(&sample, &history, Some(fit), cx))
                    .children(self.render_disks(&sample, details, Some(fit), cx))
                    .child(self.render_top(window, Some(fit), cx))
            }
        };
        // 滚动状态由这一页持有：键盘移到最下面的进程时滚过去
        div()
            .relative()
            .size_full()
            .child(
                content
                    .id("tools-overview")
                    .test_support()
                    .size_full()
                    .track_scroll(&self.scroll)
                    .overflow_y_scroll()
                    .text_xs(),
            )
            .vertical_scrollbar(&self.scroll)
            .into_any_element()
    }
}

/// 曲线的高度（rem）：主机概览小窗中矮一些。
fn chart_height(fit: Option<Fit>) -> f32 {
    match fit {
        Some(_) => COMPACT_CHART_HEIGHT,
        None => CHART_HEIGHT,
    }
}

/// 等宽数字（OpenType `tnum`）。
fn tabular() -> FontFeatures {
    FontFeatures(std::sync::Arc::new(vec![("tnum".into(), 1)]))
}

/// 面板中的一节：一张卡片（[`shared::card`]），内容离边框 0.75rem。
fn section(content: AnyElement, cx: &App) -> Div {
    shared::card(div().p_3(), cx).child(content)
}

/// 一节的标题：左边是名称，右边是读数。
fn heading(title: SharedString, right: Option<AnyElement>, cx: &App) -> Div {
    heading_with(title, None, right, cx)
}

/// 一节的标题，名称后面另有一个小元素（ⓘ）。
fn heading_with(
    title: SharedString,
    after: Option<AnyElement>,
    right: Option<AnyElement>,
    cx: &App,
) -> Div {
    h_flex()
        .justify_between()
        .gap_2()
        .child(
            h_flex()
                .gap_1()
                .child(
                    div()
                        .font_medium()
                        .text_color(cx.theme().foreground)
                        .child(title),
                )
                .children(after),
        )
        .children(right)
}

/// 每核的使用率：一排细竖条（从下往上按使用率填，偏高时按级别着色），64 核也只占一行；悬停看这一核的数值，点了
/// `on_click`（展开每核的数值）。
fn core_bars(
    cores: &[f64],
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    h_flex()
        .id("tools-overview-core-bars")
        .test_support()
        .on_click(on_click)
        .h(rems(1.25))
        .gap_0p5()
        .children(cores.iter().enumerate().map(|(ix, usage)| {
            let level = Level::of(*usage, PERCENT);
            let tip: SharedString = format!("cpu{ix} {}", whole_percent(Some(*usage))).into();
            div()
                .id(("tools-overview-core-bar", ix))
                .test_support()
                .relative()
                .flex_1()
                .h_full()
                .rounded_sm()
                .bg(theme.muted)
                .overflow_hidden()
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom_0()
                        .h(relative((usage.clamp(0., 100.) / 100.) as f32))
                        .bg(level_color(level, cx).unwrap_or(theme.chart_2)),
                )
                .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
        }))
        .into_any_element()
}

/// 一行“数值加单位”的短语（“16 核”“负载 0.52 0.40 0.31”“已运行 9 天 6 小时”）：每段不在中间断开，放不下时在段与段
/// 之间换行；段之间的“·”跟在前一段后面（间隔号不放在行首）。一段比整行还长时截断（很长的内核版本）。
fn facts(id: &'static str, parts: Vec<SharedString>, color: Hsla) -> impl IntoElement {
    let last = parts.len().saturating_sub(1);
    h_flex()
        .id(id)
        .test_support()
        .flex_wrap()
        .gap_x_1()
        .text_color(color)
        .children(parts.into_iter().enumerate().map(move |(ix, part)| {
            h_flex()
                .min_w_0()
                .max_w_full()
                .gap_1()
                .child(div().min_w_0().truncate().child(part))
                .when(ix < last, |el| el.child(div().flex_none().child("·")))
        }))
}

/// 读数：偏高时按级别着色；不在中间断开。
fn value_text(text: impl Into<SharedString>, level: Level, cx: &App) -> AnyElement {
    div()
        .flex_none()
        .whitespace_nowrap()
        .font_features(tabular())
        .text_color(level_color(level, cx).unwrap_or(cx.theme().foreground))
        .child(text.into())
        .into_any_element()
}

/// 使用率条：正常时用图表的颜色，偏高时按级别着色。
fn meter(value: f64, level: Level, cx: &App) -> Div {
    let theme = cx.theme();
    div().h_1p5().w_full().rounded_full().bg(theme.muted).child(
        div()
            .h_full()
            .rounded_full()
            .bg(level_color(level, cx).unwrap_or(theme.chart_2))
            .w(relative((value.clamp(0., 100.) / 100.) as f32)),
    )
}

/// 最近约 10 分钟的曲线：横轴的右端是最新的一次采样；`hover` 是指针下的时刻，`readout` 写出一次采样的读数。
#[allow(clippy::too_many_arguments)]
fn history_chart(
    id: &'static str,
    height: f32,
    series: Vec<Series>,
    history: &History,
    max: f64,
    label: SharedString,
    hover: Option<i64>,
    readout: impl Fn(&crate::monitor_model::Point) -> SharedString,
    on_hover: OnHover,
    cx: &App,
) -> AnyElement {
    match history.last_at() {
        Some(end) => {
            let points: Vec<_> = history.points().iter().copied().collect();
            chart(
                id, height, series, &points, end, max, label, hover, readout, on_hover, cx,
            )
            .into_any_element()
        }
        None => div().into_any_element(),
    }
}
