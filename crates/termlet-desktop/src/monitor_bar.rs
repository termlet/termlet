//! 终端底部的状态条：网络延迟与服务器的资源使用情况（CPU、内存、最满的磁盘、网速、负载）；右侧是编码。
//!
//! 资源由核心的 `monitor` 在终端自己的连接上另开 exec 通道采样（SSH、WSL；只用一个通道的主机说明原因、不采样），
//! 延迟是 SSH 保活的往返时间（不开通道，单通道的主机照常显示）。终端不在前台时不采样。
//!
//! - 两种样式（设置的“终端”中选，或在状态条上右键切换）：完整（各项都在，宽度够时带进度条与网速的小柱）、精简
//!   （只有数字；平时只有延迟、CPU、内存，最满的磁盘、负载偏高时才带着 ⚠ 出现，回落后再隐藏）。
//! - 每项的数字放在固定宽度的槽中、右对齐、用等宽数字，数值变化时各项不左右漂移；CPU、内存、磁盘、负载偏高时按级别
//!   着色（带回差，见 [`crate::monitor_model::Level`]）。
//! - 显示哪几项按量出的实际宽度决定（[`BarFit`]）：放不下时先收起进度条，再从优先级最低的一项起逐项收起；右端打开
//!   概览的按钮不在收起的范围内，始终在。
//! - 点状态条（或右端的按钮）让这个终端获得焦点，并在主机工具中打开概览页；每项悬停时有详情。

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, Div, ElementId, FontFeatures, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, SharedString, Styled as _, TestSupportExt as _, canvas, div,
    prelude::FluentBuilder as _, relative, rems,
};
use termlet_core::model::MonitorBar;
use termlet_core::monitor::Sample;

use crate::menu_tooltip::TooltipExt as _;
use crate::monitor_model::{
    self, BarItem, Fit, History, Level, Levels, disk_percent, format_rate, format_size,
    fullest_disk, load_text, percent, short_rate, whole_percent,
};
use crate::t;

/// 资源采样的情况。
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Resources {
    /// 不采样：设置关闭、本地终端、Telnet、串口、还没连上
    #[default]
    Off,
    /// 采样中；还没有第一次的结果时为 `None`
    Sampling(Option<Box<Sample>>),
    /// 服务器不是 Linux：不显示资源
    Unsupported,
    /// 只用一个通道的主机：不另开通道，说明原因
    SingleChannel,
}

impl Resources {
    pub fn sample(&self) -> Option<&Sample> {
        match self {
            Resources::Sampling(Some(sample)) => Some(sample),
            _ => None,
        }
    }

    /// 状态条上要占位置（采样中或要说明单通道）。
    pub fn shown(&self) -> bool {
        matches!(self, Resources::Sampling(_) | Resources::SingleChannel)
    }

    /// 还没有资源可显示时状态条上的说明（正在读取、单通道）。
    pub fn message(&self) -> Option<SharedString> {
        match self {
            Resources::Sampling(None) => Some(t!("monitor.loading")),
            Resources::SingleChannel => Some(t!("monitor.single_channel")),
            _ => None,
        }
    }
}

/// 延迟的分级颜色：<100 ms 好、<300 ms 一般、其余差；没有读数时为灰色。
pub fn latency_color(ms: Option<u128>, cx: &App) -> Hsla {
    let theme = cx.theme();
    match ms {
        Some(ms) if ms < 100 => theme.success,
        Some(ms) if ms < 300 => theme.warning,
        Some(_) => theme.danger,
        None => theme.muted_foreground.opacity(0.4),
    }
}

/// 延迟的悬停提示。
pub fn latency_hint(ms: Option<u128>) -> SharedString {
    match ms {
        Some(ms) if ms < 100 => t!("monitor.latency_good", ms = ms),
        Some(ms) if ms < 300 => t!("monitor.latency_warning", ms = ms),
        Some(ms) => t!("monitor.latency_poor", ms = ms),
        None => t!("monitor.latency_unknown"),
    }
}

/// 级别的颜色：正常时为 `None`（用周围的颜色）。
pub fn level_color(level: Level, cx: &App) -> Option<Hsla> {
    let theme = cx.theme();
    match level {
        Level::Normal => None,
        Level::Warning => Some(theme.warning),
        Level::Danger => Some(theme.danger),
    }
}

/// 网速小柱的格数：约 30 秒（每 3 秒一次采样）。
const SPARK_SLOTS: usize = 10;

/// 状态条上一项要显示的内容。
#[derive(Clone, Debug)]
pub struct ItemView {
    pub item: BarItem,
    level: Level,
    /// 精简样式中偏高时才出现的项：带 ⚠
    alert: bool,
    content: Content,
    /// 悬停时的详情（每行一句）
    tip: Vec<SharedString>,
}

#[derive(Clone, Debug)]
enum Content {
    Latency(Option<u128>),
    /// CPU、内存、磁盘：标签、挂载点（不是根目录的磁盘）、使用率
    Usage {
        label: SharedString,
        mount: Option<SharedString>,
        value: Option<f64>,
    },
    Net {
        rx: Option<f64>,
        tx: Option<f64>,
        spark: Vec<Option<f32>>,
    },
    Load {
        load: f64,
        cores: u32,
    },
}

/// 状态条上的各项（按优先级从高到低）。`latency` 为 `None` 时不显示延迟（设置关闭、不是 SSH），`Some(None)` 是还没有
/// 测到。没有资源的读数时只有延迟。
pub fn items(
    latency: Option<Option<u128>>,
    resources: &Resources,
    levels: Levels,
    history: &History,
    style: MonitorBar,
) -> Vec<ItemView> {
    let mut items = Vec::new();
    if let Some(ms) = latency {
        items.push(ItemView {
            item: BarItem::Latency,
            level: Level::Normal,
            alert: false,
            content: Content::Latency(ms),
            tip: vec![latency_hint(ms)],
        });
    }
    let Some(sample) = resources.sample() else {
        return items;
    };
    let compact = style == MonitorBar::Compact;
    let level_tip = |level: Level| match level {
        Level::Normal => None,
        Level::Warning => Some(t!("monitor.tip.warning")),
        Level::Danger => Some(t!("monitor.tip.danger")),
    };

    let one_core = sample.cores == 1;
    let mut cpu_tip = vec![match sample.cpu {
        Some(cpu) if one_core => t!("monitor.tip.cpu_one_core", value = format!("{cpu:.0}")),
        Some(cpu) => t!(
            "monitor.tip.cpu",
            value = format!("{cpu:.0}"),
            cores = sample.cores
        ),
        None if one_core => t!("monitor.tip.cpu_unknown_one_core"),
        None => t!("monitor.tip.cpu_unknown", cores = sample.cores),
    }];
    if let (Some(iowait), Some(steal)) = (sample.iowait, sample.steal) {
        cpu_tip.push(t!(
            "monitor.tip.cpu_wait",
            iowait = format!("{iowait:.0}"),
            steal = format!("{steal:.0}")
        ));
    }
    cpu_tip.extend(level_tip(levels.cpu));
    items.push(ItemView {
        item: BarItem::Cpu,
        level: levels.cpu,
        alert: false,
        content: Content::Usage {
            label: t!("monitor.bar.cpu"),
            mount: None,
            value: sample.cpu,
        },
        tip: cpu_tip,
    });

    let memory = percent(sample.mem_used, sample.mem_total);
    let mut memory_tip = vec![t!(
        "monitor.tip.memory",
        used = format_size(sample.mem_used as f64),
        total = format_size(sample.mem_total as f64),
        percent = format!("{memory:.0}")
    )];
    if sample.swap_total > 0 {
        memory_tip.push(t!(
            "monitor.detail_swap",
            used = format_size(sample.swap_used as f64),
            total = format_size(sample.swap_total as f64)
        ));
    }
    memory_tip.extend(level_tip(levels.memory));
    items.push(ItemView {
        item: BarItem::Memory,
        level: levels.memory,
        alert: false,
        content: Content::Usage {
            label: t!("monitor.bar.memory"),
            mount: None,
            value: Some(memory),
        },
        tip: memory_tip,
    });

    if let Some(disk) = fullest_disk(&sample.disks)
        && (!compact || levels.disk.is_alert())
    {
        let value = disk_percent(disk);
        let mut tip = vec![t!(
            "monitor.tip.disk",
            mount = disk.mount.clone(),
            used = format_size(disk.used as f64),
            total = format_size(disk.total as f64),
            percent = format!("{value:.0}")
        )];
        tip.extend(level_tip(levels.disk));
        items.push(ItemView {
            item: BarItem::Disk,
            level: levels.disk,
            alert: compact,
            // 根目录只写百分比（“磁盘 / 4%”里的“/”读起来像分隔符），别的挂载点写在标签后面
            content: Content::Usage {
                label: t!("monitor.bar.disk"),
                mount: (disk.mount != "/").then(|| disk.mount.clone().into()),
                value: Some(value),
            },
            tip,
        });
    }

    if !compact {
        items.push(ItemView {
            item: BarItem::Net,
            level: Level::Normal,
            alert: false,
            content: Content::Net {
                rx: sample.rx_rate,
                tx: sample.tx_rate,
                spark: history.spark(SPARK_SLOTS),
            },
            tip: vec![
                t!(
                    "monitor.tip.net",
                    rx = format_rate(sample.rx_rate),
                    tx = format_rate(sample.tx_rate)
                ),
                t!("monitor.tip.net_note"),
            ],
        });
    }

    if !compact || levels.load.is_alert() {
        let [a, b, c] = sample.load.map(|v| format!("{v:.2}"));
        let mut tip = vec![if one_core {
            t!("monitor.tip.load_one_core", a = a, b = b, c = c)
        } else {
            t!(
                "monitor.tip.load",
                a = a,
                b = b,
                c = c,
                cores = sample.cores
            )
        }];
        tip.extend(level_tip(levels.load));
        items.push(ItemView {
            item: BarItem::Load,
            level: levels.load,
            alert: compact,
            content: Content::Load {
                load: sample.load[0],
                cores: sample.cores,
            },
            tip,
        });
    }
    items
}

/// 等宽数字（OpenType `tnum`）：数值变化时宽度不变。
fn tabular() -> FontFeatures {
    FontFeatures(Arc::new(vec![("tnum".into(), 1)]))
}

/// 数字的槽：至少 `width` 宽、右对齐（数字更长时照样放下，只是变宽）。
fn slot(text: impl Into<SharedString>, width: f32, color: Option<Hsla>) -> Div {
    h_flex()
        .flex_none()
        .min_w(rems(width))
        .justify_end()
        .font_features(tabular())
        .when_some(color, |el, color| el.text_color(color))
        .child(text.into())
}

/// 使用率的小进度条（状态条中，按级别着色）。
fn small_meter(value: f64, level: Level, cx: &App) -> Div {
    let theme = cx.theme();
    let fill = level_color(level, cx).unwrap_or(theme.muted_foreground.opacity(0.55));
    div()
        .flex_none()
        .w(rems(2.))
        .h_1()
        .rounded_full()
        .bg(theme.border)
        .child(
            div()
                .h_full()
                .rounded_full()
                .bg(fill)
                .w(relative((value.clamp(0., 100.) / 100.) as f32)),
        )
}

/// 网速的小柱：最近约 30 秒，按格排开，没有采样的格留空；不着色。
fn spark(bars: &[Option<f32>], cx: &App) -> Div {
    let color = cx.theme().muted_foreground.opacity(0.6);
    h_flex()
        .flex_none()
        .h_3()
        .items_end()
        .gap_px()
        // 有采样的格至少有一点高：空闲时是一条看得见的平线，没有采样的格是空的
        .children(bars.iter().map(|bar| {
            div()
                .w_0p5()
                .h(relative(bar.unwrap_or(0.)))
                .when(bar.is_some(), |el| el.min_h_0p5().bg(color))
        }))
}

/// 一项的样子：`meters` 时带进度条（与网速的小柱）。不带 ID 与提示，量宽度的一排也用它。
fn render_item(item: &ItemView, meters: bool, cx: &App) -> Div {
    let color = level_color(item.level, cx);
    let row = h_flex().flex_none().gap_1().whitespace_nowrap();
    let row = row.when(item.alert, |el| {
        el.child(
            Icon::new(IconName::TriangleAlert)
                .xsmall()
                .text_color(color.unwrap_or(cx.theme().warning)),
        )
    });
    match &item.content {
        Content::Latency(ms) => row
            .child(
                div()
                    .flex_none()
                    .size_1p5()
                    .rounded_full()
                    .bg(latency_color(*ms, cx)),
            )
            .child(t!("monitor.bar.latency"))
            .child(slot(
                match ms {
                    Some(ms) => format!("{ms} ms"),
                    None => "- ms".into(),
                },
                2.25,
                None,
            )),
        Content::Usage {
            label,
            mount,
            value,
        } => row
            .child(label.clone())
            .when_some(mount.clone(), |el, mount| {
                el.child(div().max_w(rems(5.)).truncate().child(mount))
            })
            .when(meters, |el| {
                el.child(small_meter(value.unwrap_or(0.), item.level, cx))
            })
            .child(slot(whole_percent(*value), 2., color)),
        Content::Net {
            rx,
            tx,
            spark: bars,
        } => row
            // 箭头与数字在同一个槽中右对齐：两组之间留空，读起来是“↓收 ↑发”两对
            .child(slot(format!("↓{}", short_rate(*rx)), 3., None))
            .child(slot(format!("↑{}", short_rate(*tx)), 3., None))
            .when(meters, |el| el.child(div().pl_1().child(spark(bars, cx)))),
        Content::Load { load, cores } => {
            row.child(t!("monitor.bar.load"))
                .child(slot(load_text(*load, *cores), 2.5, color))
        }
    }
}

/// 状态条上的一项：带 ID（`monitor-<项>`）与悬停的详情（`menu_open` 时不挂，见 [`crate::menu_tooltip`]）。
pub fn item(item: &ItemView, meters: bool, panel: usize, menu_open: bool, cx: &App) -> AnyElement {
    let tip = item.tip.clone();
    let id = ElementId::from((
        SharedString::from(format!("monitor-{}", item.item.key())),
        panel,
    ));
    render_item(item, meters, cx)
        .id(id)
        .test_support()
        .tooltip_unless_menu(menu_open, move |window, cx| {
            let tip = tip.clone();
            Tooltip::element(move |_, _| {
                v_flex()
                    .text_xs()
                    .children(tip.iter().map(|line| div().child(line.clone())))
            })
            .build(window, cx)
        })
        .into_any_element()
}

/// 状态条按量出的宽度决定显示哪几项（量宽度的做法同 [`crate::tab_fit`]）：每帧把各项带进度条与不带时的样子各排一份
/// （[`Self::measure_row`]，隐藏、不占布局），排版后量出它们与可用的宽度（[`Self::measure_area`]），算出的结果与这一帧
/// 用的不同时再画一帧。克隆的是同一份。
#[derive(Clone)]
pub struct BarFit(Rc<FitState>);

struct FitState {
    /// 各项（按优先级）带进度条与不带时的宽度
    widths: RefCell<Vec<(f32, f32)>>,
    /// 这一帧用的结果
    shown: Cell<Fit>,
}

impl Default for BarFit {
    fn default() -> Self {
        // 还没量过时全部带进度条（放不下的被裁掉，下一帧就按量出的宽度排）
        Self(Rc::new(FitState {
            widths: RefCell::default(),
            shown: Cell::new(Fit {
                count: usize::MAX,
                meters: true,
            }),
        }))
    }
}

impl BarFit {
    pub fn current(&self) -> Fit {
        self.0.shown.get()
    }

    /// 量宽度的一排（隐藏、不占布局）：每项依次放带进度条与不带的两份。放在各项所在的区域中、[`Self::measure_area`]
    /// 之前（先排它、先量它）。
    pub fn measure_row(&self, items: &[ItemView], cx: &App) -> impl IntoElement + use<> {
        let state = self.0.clone();
        h_flex()
            .absolute()
            .top_0()
            .left_0()
            .invisible()
            .children(
                items
                    .iter()
                    .flat_map(|item| [render_item(item, true, cx), render_item(item, false, cx)]),
            )
            .on_children_prepainted(move |bounds, _, _| {
                *state.widths.borrow_mut() = bounds
                    .chunks(2)
                    .map(|pair| {
                        let full = f32::from(pair[0].size.width);
                        let bare = pair.get(1).map_or(full, |b| f32::from(b.size.width));
                        (full, bare)
                    })
                    .collect();
            })
    }

    /// 放在各项所在的区域中（绝对定位、铺满，不占布局）：排版后按这一区域的宽度与项之间的间距 `gap` 算出显示哪几项，
    /// 变了时再画一帧。
    pub fn measure_area(&self, gap: Pixels) -> impl IntoElement + use<> {
        let state = self.0.clone();
        canvas(
            move |bounds, window, _| {
                let fit = monitor_model::fit(
                    &state.widths.borrow(),
                    f32::from(bounds.size.width),
                    f32::from(gap),
                );
                if state.shown.replace(fit) != fit {
                    window.on_next_frame(|window, _| window.refresh());
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full()
    }
}
