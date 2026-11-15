//! 主窗口记住位置、大小与是否最大化（本机的界面状态 `main_window`，不同步、不进备份）。
//!
//! - 记：窗口关闭时（关闭按钮、菜单的“关闭窗口”、退出时关闭全部窗口）记下这个窗口，多个窗口时最后关闭的为准；
//!   移动、改大小、最大化后停下 [`SETTLE`] 也记一次（界面状态随后照常写回文件），异常退出时不至于全丢；退出时把还没
//!   写的立即写回。全屏按最大化记（还原后的大小照记）：全屏多是临时看一下，下次一打开就全屏（macOS 还会单独占一个
//!   桌面）反倒意外。
//! - 恢复（[`restore`]）：放回原来那块屏幕，标题栏要有一段在屏幕的可见区域里（[`MIN_VISIBLE_WIDTH`] ×
//!   [`MIN_VISIBLE_HEIGHT`]），否则在那块屏幕上居中；屏幕找不到了（副屏拔掉）放到主屏中间；宽高不超过屏幕的可见区域。
//!   上次最大化的先按还原后的大小放好再最大化（GPUI 打开 `WindowBounds::Maximized` 就是这样做的），取消最大化后
//!   有合适的大小。
//! - 第一次打开：默认 1280 × 800 放在主屏中间，主屏的可见区域放不下时按它的九成缩小，但不小于最小尺寸 720 × 480
//!   （屏幕本身比这还小时以屏幕为准）。
//!
//! 坐标是逻辑像素，按窗口所在的显示器换算（GPUI 在 Windows 上各显示器按自己的缩放换算，macOS 上相对屏幕的左上角），
//! 所以连同显示器的 UUID 一起记，恢复时把位置交给同一块屏幕（`WindowOptions::display_id`）。

use std::time::Duration;

use gpui_kit::{App, Bounds, DisplayId, Pixels, Window, WindowBounds, point, px, size};
use termlet_core::ui_state::WindowPlacement;

use crate::app_state::AppState;

/// 默认大小。
pub(crate) const DEFAULT_SIZE: (f32, f32) = (1280., 800.);
/// 最小尺寸（也是主窗口的 `window_min_size`）。
pub(crate) const MIN_SIZE: (f32, f32) = (720., 480.);
/// 默认大小放不下时占可见区域的比例。
const FIT_RATIO: f32 = 0.9;
/// 标题栏那一条的高度：检查它在不在屏幕里。
const TITLE_STRIP: f32 = 32.;
/// 标题栏至少有这么宽、这么高露在屏幕的可见区域里，才算拿得到（能拖回来）。
pub(crate) const MIN_VISIBLE_WIDTH: f32 = 120.;
pub(crate) const MIN_VISIBLE_HEIGHT: f32 = 16.;
/// 移动、改大小停下这么久后记一次。
pub(crate) const SETTLE: Duration = Duration::from_millis(500);

/// 矩形（逻辑像素）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    fn right(&self) -> f32 {
        self.x + self.w
    }

    fn bottom(&self) -> f32 {
        self.y + self.h
    }

    /// 两个矩形相交部分的宽、高（不相交时为 0）。
    fn overlap(&self, other: &Rect) -> (f32, f32) {
        let w = self.right().min(other.right()) - self.x.max(other.x);
        let h = self.bottom().min(other.bottom()) - self.y.max(other.y);
        (w.max(0.), h.max(0.))
    }

    /// 在 `area` 中居中放一个 `w` × `h` 的矩形。
    fn centered_in(area: &Rect, w: f32, h: f32) -> Rect {
        Rect {
            x: area.x + (area.w - w) / 2.,
            y: area.y + (area.h - h) / 2.,
            w,
            h,
        }
    }
}

/// 一块屏幕：显示器的 UUID（取不到时为 `None`）、可见区域（不含任务栏、程序坞），是不是主屏。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Screen {
    pub id: Option<String>,
    pub visible: Rect,
    pub primary: bool,
}

/// 恢复的结果：放在第几块屏幕（`screens` 中的下标；没有屏幕信息时为 `None`），还原后的矩形，要不要最大化。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Restored {
    pub screen: Option<usize>,
    pub rect: Rect,
    pub maximized: bool,
}

/// 按记下的位置（没有就是第一次打开）与现在的屏幕算出窗口打开的位置。
pub(crate) fn restore(saved: Option<&WindowPlacement>, screens: &[Screen]) -> Restored {
    if let Some(restored) = saved.and_then(|saved| restore_saved(saved, screens, MIN_SIZE)) {
        return restored;
    }
    let primary = primary_screen(screens);
    Restored {
        screen: primary,
        rect: default_rect(primary.map(|i| &screens[i].visible)),
        maximized: false,
    }
}

/// 主屏在 `screens` 中的下标（没有标出主屏时为第一块；没有屏幕信息时为 `None`）。
pub(crate) fn primary_screen(screens: &[Screen]) -> Option<usize> {
    screens
        .iter()
        .position(|s| s.primary)
        .or((!screens.is_empty()).then_some(0))
}

/// 按记下的位置放窗口（规则见模块说明），宽高不小于 `min`（屏幕比它还小时以屏幕为准）；记下的不对（不是有限的数、
/// 宽高不是正数）时为 `None`。主窗口与主机概览的小窗共用。
pub(crate) fn restore_saved(
    saved: &WindowPlacement,
    screens: &[Screen],
    min: (f32, f32),
) -> Option<Restored> {
    let valid = [saved.x, saved.y, saved.width, saved.height]
        .iter()
        .all(|v| v.is_finite())
        && saved.width > 0.
        && saved.height > 0.;
    if !valid {
        return None;
    }
    let primary = primary_screen(screens);
    let rect = Rect {
        x: saved.x,
        y: saved.y,
        w: saved.width,
        h: saved.height,
    };
    // 原来那块屏幕；没记下（或取不到 UUID）时按位置找交集最大的
    let screen = saved
        .display
        .as_ref()
        .and_then(|id| screens.iter().position(|s| s.id.as_ref() == Some(id)))
        .or_else(|| {
            screens
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let (w, h) = rect.overlap(&s.visible);
                    (i, w * h)
                })
                .filter(|(_, area)| *area > 0.)
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(i, _)| i)
        });
    let Some(index) = screen.or(primary) else {
        // 没有屏幕信息：原样交给 GPUI（它会检查）
        return Some(Restored {
            screen: None,
            rect,
            maximized: saved.maximized,
        });
    };
    let visible = &screens[index].visible;
    let (min_w, min_h) = (min.0.min(visible.w), min.1.min(visible.h));
    let w = rect.w.min(visible.w).max(min_w);
    let h = rect.h.min(visible.h).max(min_h);
    let sized = Rect { w, h, ..rect };
    let rect = if screen.is_some() && title_visible(&sized, visible) {
        sized
    } else {
        Rect::centered_in(visible, w, h)
    };
    Some(Restored {
        screen: Some(index),
        rect,
        maximized: saved.maximized,
    })
}

/// 第一次打开的大小与位置：默认大小放在 `visible`（主屏的可见区域）中间，放不下时按九成缩小，不小于最小尺寸
/// （屏幕比最小尺寸还小时以屏幕为准）。
fn default_rect(visible: Option<&Rect>) -> Rect {
    let (w, h) = DEFAULT_SIZE;
    let Some(visible) = visible else {
        return Rect { x: 0., y: 0., w, h };
    };
    if w <= visible.w && h <= visible.h {
        return Rect::centered_in(visible, w, h);
    }
    // 有一边放不下：两边都不超过可见区域的九成（不贴着屏幕边），不小于最小尺寸，不超过屏幕
    let fit = |default: f32, min: f32, available: f32| {
        default.min(available * FIT_RATIO).max(min).min(available)
    };
    Rect::centered_in(
        visible,
        fit(w, MIN_SIZE.0, visible.w),
        fit(h, MIN_SIZE.1, visible.h),
    )
}

/// 标题栏那一条有足够的一段在可见区域里（拿得到、能拖回来）。
fn title_visible(rect: &Rect, visible: &Rect) -> bool {
    let strip = Rect {
        h: rect.h.min(TITLE_STRIP),
        ..*rect
    };
    let (w, h) = strip.overlap(visible);
    w >= MIN_VISIBLE_WIDTH.min(rect.w) && h >= MIN_VISIBLE_HEIGHT.min(strip.h)
}

// ------------------------------------------------------------------ 与 GPUI 之间

/// 现在的各块屏幕（与 `cx.displays()` 的顺序相同）。
pub(crate) fn screens(cx: &App) -> Vec<Screen> {
    let primary = cx.primary_display().map(|d| d.id());
    cx.displays()
        .iter()
        .map(|d| {
            let visible = d.visible_bounds();
            Screen {
                id: d.uuid().ok().map(|u| u.to_string()),
                visible: Rect {
                    x: f32::from(visible.origin.x),
                    y: f32::from(visible.origin.y),
                    w: f32::from(visible.size.width),
                    h: f32::from(visible.size.height),
                },
                primary: Some(d.id()) == primary,
            }
        })
        .collect()
}

/// 算出的位置（屏幕的下标按 [`screens`] 的顺序）换成 GPUI 的窗口位置与显示器。
pub(crate) fn window_bounds(restored: &Restored, cx: &App) -> (Bounds<Pixels>, Option<DisplayId>) {
    let r = restored.rect;
    let bounds = Bounds {
        origin: point(px(r.x), px(r.y)),
        size: size(px(r.w), px(r.h)),
    };
    let display = restored
        .screen
        .and_then(|i| cx.displays().get(i).map(|d| d.id()));
    (bounds, display)
}

/// 主窗口打开时的位置与大小、放在哪块屏幕上（按本机界面状态中记下的）。
pub(crate) fn initial_bounds(cx: &App) -> (WindowBounds, Option<DisplayId>) {
    let saved = cx
        .try_global::<AppState>()
        .and_then(|app| app.ui.read(cx).state().main_window.clone());
    let restored = restore(saved.as_ref(), &screens(cx));
    let (bounds, display) = window_bounds(&restored, cx);
    let bounds = if restored.maximized {
        WindowBounds::Maximized(bounds)
    } else {
        WindowBounds::Windowed(bounds)
    };
    (bounds, display)
}

/// 这个窗口现在的位置（最大化、全屏时是还原后的），连同所在的显示器。
pub(crate) fn placement_of(window: &Window, cx: &App) -> Option<WindowPlacement> {
    let (bounds, maximized) = match window.window_bounds() {
        WindowBounds::Windowed(bounds) => (bounds, false),
        // 全屏按最大化记，见模块说明
        WindowBounds::Maximized(bounds) | WindowBounds::Fullscreen(bounds) => (bounds, true),
    };
    let placement = WindowPlacement {
        x: f32::from(bounds.origin.x),
        y: f32::from(bounds.origin.y),
        width: f32::from(bounds.size.width),
        height: f32::from(bounds.size.height),
        maximized,
        display: window
            .display(cx)
            .and_then(|d| d.uuid().ok())
            .map(|u| u.to_string()),
    };
    let valid = [placement.x, placement.y, placement.width, placement.height]
        .iter()
        .all(|v| v.is_finite())
        && placement.width > 0.
        && placement.height > 0.;
    valid.then_some(placement)
}

/// 记下这个窗口的位置（界面状态随后写回文件）。
pub(crate) fn remember(window: &Window, cx: &mut App) {
    let Some(placement) = placement_of(window, cx) else {
        return;
    };
    let Some(ui) = cx.try_global::<AppState>().map(|app| app.ui.clone()) else {
        return;
    };
    ui.update(cx, |ui, cx| {
        ui.update(|state| state.main_window = Some(placement), cx)
    });
}

/// 退出前：还有窗口开着（锁屏的“退出”、重启并安装等直接退出的）时记下最前面的那个，然后把界面状态立即写回。
pub(crate) fn save_on_quit(cx: &mut App) {
    // 当前窗口在前；不是主窗口（主机概览的小窗）时往前找最近的主窗口
    let active = cx.active_window();
    let mut handles = cx.windows();
    handles.sort_by_key(|handle| Some(*handle) == active);
    for handle in handles.into_iter().rev() {
        let remembered = handle
            .update(cx, |_, window, cx| {
                let main = crate::Workspace::in_window(window, cx).is_some();
                if main {
                    remember(window, cx);
                }
                main
            })
            .unwrap_or(false);
        if remembered {
            break;
        }
    }
    if let Some(ui) = cx.try_global::<AppState>().map(|app| app.ui.clone()) {
        ui.update(cx, |ui, _| ui.flush());
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{AppContext as _, TestAppContext};

    use super::*;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    /// 主屏 1920 × 1080（任务栏占去下面 40），右边一块 2560 × 1440 的副屏。
    fn screens() -> Vec<Screen> {
        vec![
            Screen {
                id: Some("main".into()),
                visible: rect(0., 0., 1920., 1040.),
                primary: true,
            },
            Screen {
                id: Some("side".into()),
                visible: rect(1920., 0., 2560., 1400.),
                primary: false,
            },
        ]
    }

    fn saved(x: f32, y: f32, w: f32, h: f32, display: &str) -> WindowPlacement {
        WindowPlacement {
            x,
            y,
            width: w,
            height: h,
            maximized: false,
            display: Some(display.into()),
        }
    }

    #[test]
    fn first_launch_centers_the_default_size_on_the_primary_screen() {
        let r = restore(None, &screens());
        assert_eq!(r.screen, Some(0));
        assert_eq!(r.rect, rect(320., 120., 1280., 800.));
        assert!(!r.maximized);
        // 主屏放不下 1280 × 800：按可见区域的九成
        let small = [Screen {
            id: None,
            visible: rect(0., 0., 1280., 720.),
            primary: true,
        }];
        assert_eq!(restore(None, &small).rect, rect(64., 36., 1152., 648.));
        // 九成比最小尺寸还小：用最小尺寸；屏幕比最小尺寸还小：以屏幕为准
        let tiny = [Screen {
            id: None,
            visible: rect(0., 0., 760., 500.),
            primary: true,
        }];
        assert_eq!(restore(None, &tiny).rect, rect(20., 10., 720., 480.));
        let smaller = [Screen {
            id: None,
            visible: rect(0., 0., 640., 400.),
            primary: true,
        }];
        assert_eq!(restore(None, &smaller).rect, rect(0., 0., 640., 400.));
        // 没有屏幕信息：默认大小，交给 GPUI
        assert_eq!(restore(None, &[]).rect, rect(0., 0., 1280., 800.));
        assert_eq!(restore(None, &[]).screen, None);
    }

    #[test]
    fn saved_placement_comes_back_on_its_screen() {
        let side = saved(2100., 150., 1400., 900., "side");
        let r = restore(Some(&side), &screens());
        assert_eq!(
            (r.screen, r.rect),
            (Some(1), rect(2100., 150., 1400., 900.))
        );
        // 最大化的：放好还原后的位置，再最大化
        let maximized = WindowPlacement {
            maximized: true,
            ..side.clone()
        };
        let r = restore(Some(&maximized), &screens());
        assert!(r.maximized);
        assert_eq!(r.rect, rect(2100., 150., 1400., 900.));
        // 部分在屏幕外，但标题栏有一段露着：照原样
        let partly = saved(1700., 500., 1000., 700., "main");
        assert_eq!(
            restore(Some(&partly), &screens()).rect,
            rect(1700., 500., 1000., 700.)
        );
        // 没记下显示器（取不到 UUID）：按位置找交集最大的那块屏幕
        let unknown = WindowPlacement {
            display: None,
            ..side
        };
        assert_eq!(restore(Some(&unknown), &screens()).screen, Some(1));
    }

    #[test]
    fn off_screen_or_unplugged_comes_back_to_the_middle() {
        // 整个在屏幕外（右下很远）：在原来那块屏幕上居中
        let far = saved(9000., 9000., 1000., 700., "main");
        assert_eq!(
            restore(Some(&far), &screens()).rect,
            rect(460., 170., 1000., 700.)
        );
        // 标题栏在屏幕上沿之外（只露出下半截）：拿不到标题栏，居中
        let above = saved(200., -500., 1000., 700., "main");
        assert_eq!(
            restore(Some(&above), &screens()).rect,
            rect(460., 170., 1000., 700.)
        );
        // 只露出左边一小条（不到 120）：居中
        let edge = saved(1850., 100., 1000., 700., "main");
        assert_eq!(
            restore(Some(&edge), &screens()).rect,
            rect(460., 170., 1000., 700.)
        );
        // 副屏拔掉了：放到主屏中间，大小按主屏收
        let gone = saved(2100., 150., 2400., 1300., "side");
        let r = restore(Some(&gone), &screens()[..1]);
        assert_eq!((r.screen, r.rect), (Some(0), rect(0., 0., 1920., 1040.)));
        let gone = saved(2100., 150., 1400., 900., "side");
        let r = restore(Some(&gone), &screens()[..1]);
        assert_eq!(r.rect, rect(260., 70., 1400., 900.));
    }

    #[test]
    fn size_is_capped_by_the_screen_and_floored_by_the_minimum() {
        // 比屏幕大：宽高收到可见区域以内（位置还在屏幕里就不动）
        let huge = saved(0., 0., 3000., 2000., "main");
        assert_eq!(
            restore(Some(&huge), &screens()).rect,
            rect(0., 0., 1920., 1040.)
        );
        // 比最小尺寸小（旧数据、手改的）：放大到最小尺寸
        let small = saved(100., 100., 300., 200., "main");
        assert_eq!(
            restore(Some(&small), &screens()).rect,
            rect(100., 100., 720., 480.)
        );
        // 屏幕很小：以屏幕为准
        let tiny = [Screen {
            id: Some("main".into()),
            visible: rect(0., 0., 640., 400.),
            primary: true,
        }];
        assert_eq!(
            restore(Some(&small), &tiny).rect,
            rect(100., 100., 640., 400.)
        );
        // 宽高不对的记录当作没有
        let broken = WindowPlacement {
            width: f32::NAN,
            ..saved(0., 0., 1000., 700., "main")
        };
        assert_eq!(
            restore(Some(&broken), &screens()),
            restore(None, &screens())
        );
        let zero = WindowPlacement::default();
        assert_eq!(restore(Some(&zero), &screens()), restore(None, &screens()));
    }

    /// 关闭窗口时记下它的位置（多个窗口时最后关闭的为准）；下次打开主窗口按记下的放。
    #[gpui_kit::test]
    fn closing_a_window_remembers_it(cx: &mut TestAppContext) {
        let first = crate::testkit::open_workspace(cx);
        let second = cx
            .update(|cx| {
                gpui_kit::open_window(
                    gpui_kit::WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: point(px(40.), px(60.)),
                            size: size(px(900.), px(600.)),
                        })),
                        ..Default::default()
                    },
                    cx,
                    |window, cx| cx.new(|cx| crate::Workspace::new(window, cx)),
                )
            })
            .unwrap()
            .0;
        cx.run_until_parked();
        let placement = |cx: &mut TestAppContext| {
            cx.update(|cx| AppState::ui(cx).read(cx).state().main_window.clone())
        };
        assert_eq!(placement(cx), None);
        for handle in [second, first] {
            cx.update_window(handle, |_, window, cx| {
                window.activate_window();
                window.dispatch_action(Box::new(crate::app_menu::CloseWindow), cx)
            })
            .unwrap();
            cx.run_until_parked();
            if handle == second {
                let saved = placement(cx).expect("关闭后记下");
                assert_eq!(
                    (saved.x, saved.y, saved.width, saved.height, saved.maximized),
                    (40., 60., 900., 600., false)
                );
                assert!(saved.display.is_some(), "连同显示器");
            }
        }
        let saved = placement(cx).unwrap();
        assert_eq!((saved.width, saved.height), (1024., 640.), "最后关闭的为准");
        let (bounds, display) = cx.update(|cx| initial_bounds(cx));
        assert_eq!(
            bounds,
            WindowBounds::Windowed(Bounds {
                origin: point(px(0.), px(0.)),
                size: size(px(1024.), px(640.)),
            })
        );
        assert!(display.is_some());
    }
}
