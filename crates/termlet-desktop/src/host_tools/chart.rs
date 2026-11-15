//! 概览页的时间曲线：最近约 10 分钟，横轴按采样的时间（右端是最新的一次采样），纵轴从 0 到上端，25%、50%、75% 处有
//! 浅的网格线（网速的上端是取整的刻度，网格线也落在刻度上）。没有采样的时段、没有读数的点断开
//! （[`crate::monitor_model::segments`]），不插值；每段画折线与下方浅色的填充。用 canvas 画；鼠标停在曲线上时标出离指针
//! 最近的那次采样（一条竖线），左上角写出那一刻的时间与读数。

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    App, Bounds, Hsla, InteractiveElement as _, IntoElement, MouseMoveEvent, ParentElement as _,
    PathBuilder, Pixels, SharedString, StatefulInteractiveElement as _, Styled as _,
    TestSupportExt as _, Window, canvas, div, point, prelude::FluentBuilder as _, rems,
};

use super::model::clock_time;
use crate::monitor_model::{History, Point};

/// 一条曲线：各段（采样时间、值）与颜色。
pub(super) struct Series {
    pub segments: Vec<Vec<(i64, f64)>>,
    pub color: Hsla,
}

/// 网格线的位置（占纵轴上端的比例）。
const GRID: [f64; 3] = [0.25, 0.5, 0.75];

/// 离指针最近的采样与指针相差超过这么久时不标出（没有采样的时段）。
const NEAREST_MAX_MS: i64 = 6_000;

/// 鼠标停在曲线上时告知指针下的时刻（移出时为 `None`）。
pub(super) type OnHover = Rc<dyn Fn(Option<i64>, &mut Window, &mut App)>;

/// 曲线的高度（rem）：概览页的，与主机概览小窗中矮一些的。
pub(super) const CHART_HEIGHT: f32 = 4.;
pub(super) const COMPACT_CHART_HEIGHT: f32 = 3.;

/// 曲线图：`end` 为横轴的右端（最新一次采样的时间），`max` 为纵轴的上端，`label` 写在右上角（上端的刻度），
/// `height` 为高度（rem）。`points` 是这段时间的采样，`hover` 是指针下的时刻：标出离它最近的那次采样，左上角写
/// “时间 · `readout` 给出的读数”。
#[allow(clippy::too_many_arguments)]
pub(super) fn chart(
    id: &'static str,
    height: f32,
    series: Vec<Series>,
    points: &[Point],
    end: i64,
    max: f64,
    label: SharedString,
    hover: Option<i64>,
    readout: impl Fn(&Point) -> SharedString,
    on_hover: OnHover,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    let start = end - History::SPAN_MS;
    let nearest = hover.and_then(|at| {
        points
            .iter()
            .filter(|p| p.at >= start)
            .min_by_key(|p| (p.at - at).abs())
            .filter(|p| (p.at - at).abs() <= NEAREST_MAX_MS)
    });
    let marker = nearest.map(|p| p.at);
    let text =
        nearest.map(|p| SharedString::from(format!("{} · {}", clock_time(p.at), readout(p))));
    let bounds = Rc::new(Cell::new(Bounds::<Pixels>::default()));
    let (record, measured) = (bounds.clone(), bounds);
    let (grid, cursor) = (theme.border, theme.muted_foreground.opacity(0.6));
    let (on_move, on_leave) = (on_hover.clone(), on_hover);
    div()
        .id(id)
        .test_support()
        .relative()
        .w_full()
        .h(rems(height))
        .rounded(theme.radius)
        .bg(theme.muted.opacity(0.5))
        .overflow_hidden()
        .on_mouse_move(move |event: &MouseMoveEvent, window, cx| {
            let bounds = measured.get();
            if !bounds.contains(&event.position) || bounds.size.width <= Pixels::ZERO {
                return;
            }
            let fraction = (event.position.x - bounds.left()) / bounds.size.width;
            let at = start + (f64::from(fraction.clamp(0., 1.)) * History::SPAN_MS as f64) as i64;
            on_move(Some(at), window, cx);
        })
        .on_hover(move |hovered, window, cx| {
            if !hovered {
                on_leave(None, window, cx);
            }
        })
        .child(
            canvas(
                move |bounds, _, _| record.set(bounds),
                move |bounds, _, window, _| {
                    paint(&series, end, max, marker, (grid, cursor), bounds, window)
                },
            )
            .size_full(),
        )
        .child(
            div()
                .absolute()
                .top_0p5()
                .right_1p5()
                .text_xs()
                .whitespace_nowrap()
                .text_color(theme.muted_foreground)
                .child(label),
        )
        .when_some(text, |this, text| {
            this.child(
                div()
                    .id(SharedString::from(format!("{id}-readout")))
                    .test_support()
                    .absolute()
                    .top_0p5()
                    .left_1p5()
                    .text_xs()
                    .whitespace_nowrap()
                    .text_color(theme.foreground)
                    .child(text),
            )
        })
}

fn paint(
    series: &[Series],
    end: i64,
    max: f64,
    marker: Option<i64>,
    (grid, cursor): (Hsla, Hsla),
    bounds: Bounds<Pixels>,
    window: &mut Window,
) {
    if max <= 0. {
        return;
    }
    let start = end - History::SPAN_MS;
    let span = History::SPAN_MS as f32;
    // 线宽随界面缩放；顶部留出右上角刻度的一行（曲线到顶时不压住它），底部留出线宽（线不被裁掉一半）
    let line = window.rem_size() * 0.1;
    let top = bounds.top() + window.rem_size() * 1.125;
    let bottom = bounds.bottom() - line;
    let height = bottom - top;
    let x =
        |at: i64| bounds.left() + bounds.size.width * ((at - start) as f32 / span).clamp(0., 1.);
    let y = |value: f64| bottom - height * (value / max).clamp(0., 1.) as f32;
    // 网格线：细、浅，画在曲线下面
    for fraction in GRID {
        let mut path = PathBuilder::stroke(line * 0.5);
        path.move_to(point(bounds.left(), y(max * fraction)));
        path.line_to(point(bounds.right(), y(max * fraction)));
        if let Ok(path) = path.build() {
            window.paint_path(path, grid);
        }
    }
    for s in series {
        for segment in &s.segments {
            let points: Vec<_> = segment
                .iter()
                .filter(|(at, _)| *at >= start)
                .map(|(at, value)| point(x(*at), y(*value)))
                .collect();
            let (Some(first), Some(last)) = (points.first(), points.last()) else {
                continue;
            };
            if points.len() > 1 {
                let mut fill = PathBuilder::fill();
                fill.move_to(point(first.x, bounds.bottom()));
                for p in &points {
                    fill.line_to(*p);
                }
                fill.line_to(point(last.x, bounds.bottom()));
                fill.close();
                if let Ok(path) = fill.build() {
                    window.paint_path(path, s.color.opacity(0.15));
                }
            }
            let mut stroke = PathBuilder::stroke(line);
            if points.len() == 1 {
                // 前后都断开的一个点：画一小段横线
                stroke.move_to(point(first.x - line * 1.5, first.y));
                stroke.line_to(point(first.x + line * 1.5, first.y));
            } else {
                stroke.move_to(*first);
                for p in &points[1..] {
                    stroke.line_to(*p);
                }
            }
            if let Ok(path) = stroke.build() {
                window.paint_path(path, s.color);
            }
        }
    }
    // 指针下的那次采样：一条竖线（读数在左上角）
    if let Some(at) = marker {
        let mut path = PathBuilder::stroke(line * 0.75);
        path.move_to(point(x(at), top));
        path.line_to(point(x(at), bounds.bottom()));
        if let Ok(path) = path.build() {
            window.paint_path(path, cursor);
        }
    }
}
