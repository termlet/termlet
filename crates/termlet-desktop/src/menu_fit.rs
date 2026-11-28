//! 弹出菜单按窗口收高度：窗口矮、放不下时在菜单里上下滚动，放得下时与平常相同。
//!
//! gpui-kit 的弹出菜单默认没有最大高度：比窗口高时只被挪到窗口顶上、不会缩短，下面几项落在窗口外点不到（窗口矮、
//! 界面放大时的终端 Tab 菜单）。gpui-kit 的 `scrollable` 默认最高为窗口的一半（不超过 450px），平常放得下的菜单也要滚，
//! 所以最大高度按窗口给。项多的菜单（终端 Tab、终端区、SFTP 文件、主机工具各页的行等）构建时先经过 [`fit`]。
//!
//! 只在窗口放不下最长的菜单时才让菜单可以滚动：gpui-kit 的可滚动菜单总能多滚 8px（滚动区中量位置的元素占满整个
//! 滚动区，又加了一遍内边距），一直显示的滚动条会画出来、压在最宽一项的末尾。菜单有几项构建时看不到，按最长的算。

use gpui_kit::component::menu::PopupMenu;
use gpui_kit::{Context, Pixels, Window, px, rems};

/// 菜单与窗口上下边之间留出的距离（rem）：gpui-kit 让弹出菜单离窗口边各 8px，加上菜单的边框。
const WINDOW_EDGE: f32 = 1.25;

/// 最长的菜单有几项（终端 Tab 的菜单，连分隔线约 24 行）。
const TALLEST_ROWS: f32 = 24.;

/// 菜单最高为窗口的高度（减去上下边距），窗口放不下最长的菜单时在菜单里滚动。
pub fn fit(menu: PopupMenu, window: &Window) -> PopupMenu {
    let rem = window.rem_size();
    let max = window.viewport_size().height - rems(WINDOW_EDGE).to_pixels(rem);
    if max >= tallest_menu(rem) {
        return menu;
    }
    menu.scrollable(true).max_h(max)
}

/// 最长的菜单的高度：gpui-kit 的菜单项至少 26px 高，界面放大时随文字（小号字的一行）变高，项之间隔 0.125rem。
fn tallest_menu(rem: Pixels) -> Pixels {
    let row = px(26.).max(rems(1.3125).to_pixels(rem)) + rems(0.125).to_pixels(rem);
    row * TALLEST_ROWS
}

/// 包一层菜单的构建（`.context_menu(fitted(…))`）：先按窗口收高度，再照原样建。
pub fn fitted(
    build: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
    move |menu, window, cx| build(fit(menu, window), window, cx)
}

#[cfg(test)]
mod tests {
    use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AppContext as _, Bounds, Context, ElementId, InteractiveElement as _, IntoElement,
        ParentElement as _, Render, ScrollDelta, Styled as _, TestAppContext, TestSupportExt as _,
        Window, WindowBounds, WindowOptions, div, point, px, size,
    };

    use super::fitted;

    /// 右键弹出 40 项菜单的一块区域。
    struct Area;

    impl Render for Area {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(
                    div()
                        .id("area")
                        .test_support()
                        .size_full()
                        .context_menu(fitted(|menu, _, _| {
                            (0..40).fold(menu, |menu, ix| {
                                menu.item(PopupMenuItem::new(format!("第 {ix} 项")))
                            })
                        })),
                )
        }
    }

    // 菜单比窗口高时缩到窗口内（在菜单里滚动），不伸出窗口
    #[gpui_kit::test]
    fn tall_menu_stays_inside_the_window(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, _) = cx
            .update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                            Default::default(),
                            size(px(500.), px(320.)),
                        ))),
                        ..Default::default()
                    },
                    cx,
                    |_, cx| cx.new(|_| Area),
                )
            })
            .unwrap();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.right_click("area", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let menu = window.find("popup-menu").bounds();
            let viewport = window.viewport_size();
            assert!(
                menu.top() >= px(0.) && menu.bottom() <= viewport.height,
                "菜单 {menu:?} 在窗口（高 {:?}）内",
                viewport.height
            );
        })
        .unwrap();
    }

    /// 右键弹出 8 项（中间一条分隔线）菜单的一块区域。
    struct Short;

    impl Render for Short {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(
                    div()
                        .id("area")
                        .test_support()
                        .size_full()
                        .context_menu(fitted(|menu, _, _| {
                            (0..7)
                                .fold(menu, |menu, ix| {
                                    menu.item(PopupMenuItem::new(format!("第 {ix} 项")))
                                })
                                .separator()
                                .item(PopupMenuItem::new("最后一项"))
                        })),
                )
        }
    }

    // 窗口放得下时菜单不滚动：gpui-kit 的可滚动菜单总能多滚几像素，一直显示的滚动条会压在最宽一项上
    #[gpui_kit::test]
    fn short_menu_does_not_scroll_in_a_tall_window(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, _) = cx
            .update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                            Default::default(),
                            size(px(800.), px(720.)),
                        ))),
                        ..Default::default()
                    },
                    cx,
                    |_, cx| cx.new(|_| Short),
                )
            })
            .unwrap();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.right_click("area", cx);
        })
        .unwrap();
        cx.run_until_parked();
        // 最后一项（菜单项的 ID 是序号，分隔线也占一个）
        let last = || ElementId::Integer(8);
        let before = cx
            .update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.find(last()).bounds()
            })
            .unwrap();
        cx.update_window(handle, |_, window, cx| {
            window.scroll(
                "popup-menu",
                ScrollDelta::Pixels(point(px(0.), px(-50.))),
                cx,
            )
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find(last()).bounds(), before, "滚轮不动");
        })
        .unwrap();
    }
}
