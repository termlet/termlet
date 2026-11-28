//! 弹出菜单按窗口收高度：比窗口高时在菜单里上下滚动，放得下时与平常相同。
//!
//! gpui-kit 的弹出菜单默认没有最大高度：比窗口高时只被挪到窗口顶上、不会缩短，下面几项落在窗口外点不到（窗口矮、
//! 界面放大时的终端 Tab 菜单）。gpui-kit 的 `scrollable` 默认最高为窗口的一半（不超过 450px），平常放得下的菜单也要滚，
//! 所以最大高度按窗口给。项多的菜单（终端 Tab、终端区、SFTP 文件、主机工具各页的行等）构建时先经过 [`fit`]。

use gpui_kit::component::menu::PopupMenu;
use gpui_kit::{Context, Window, rems};

/// 菜单与窗口上下边之间留出的距离（rem）：gpui-kit 让弹出菜单离窗口边各 8px，加上菜单的边框。
const WINDOW_EDGE: f32 = 1.25;

/// 菜单最高为窗口的高度（减去上下边距），放不下时在菜单里滚动。
pub fn fit(menu: PopupMenu, window: &Window) -> PopupMenu {
    let max = window.viewport_size().height - rems(WINDOW_EDGE).to_pixels(window.rem_size());
    menu.scrollable(true).max_h(max)
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
        AppContext as _, Bounds, Context, InteractiveElement as _, IntoElement, ParentElement as _,
        Render, Styled as _, TestAppContext, TestSupportExt as _, Window, WindowBounds,
        WindowOptions, div, px, size,
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
}
