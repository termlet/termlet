//! 弹出菜单开着时不显示悬停提示。
//!
//! 指针停在列表的一行上、提示出现（或正等着出现）时右键，菜单打开后提示仍会出现并浮在菜单之上，盖住菜单的第一项：
//! - GPUI 的提示（元素的 `.tooltip(...)`）按下鼠标时只收起已经出现的，等待中的照样到时出现；出现后按元素的范围
//!   判断指针还在不在上面，不看是否被菜单遮住（gpui-pre 的 `div.rs` 中也注明了这一点）。
//! - gpui-kit 组件（按钮等）的提示只在按下左键时收起。
//!
//! gpui-kit 没有现成的做法。这里记下所有弹出菜单（[`init`]），焦点在其中一个里时 [`menu_open`] 为真：挂了右键菜单的
//! 行与其中的按钮这时不挂提示（[`TooltipExt::tooltip_unless_menu`]，组件的提示用 `.when(!menu_open, …)`），
//! 菜单打开时焦点移到菜单里、窗口随之重绘，GPUI 与 gpui-kit 在这一帧去掉已有的提示；菜单关闭后照常显示。
//!
//! 主机工具的列表早有自己的一套（`host_tools::shared::row_tooltip`，菜单开着时给空的提示），竖排 Tab 列表也记着
//! 从它打开的菜单；这里的用于其余的列表（侧边栏、Tab 标题、快捷命令、SFTP 文件列表、状态条、标题栏的工具栏等）。

use gpui_kit::component::menu::PopupMenu;
use gpui_kit::{
    AnyView, App, AppContext as _, Context, DismissEvent, Focusable as _, Global, IntoElement,
    Render, StatefulInteractiveElement, WeakEntity, Window, div,
};

/// 打开着的弹出菜单：创建时加入（gpui-kit 每次打开菜单都新建一个），关闭（`DismissEvent`）时去掉。属于哪个窗口
/// 看焦点。
#[derive(Default)]
struct Menus(Vec<WeakEntity<PopupMenu>>);

impl Global for Menus {}

/// 记下之后创建的每个弹出菜单（右键菜单、下拉菜单）。
pub fn init(cx: &mut App) {
    cx.observe_new(|_: &mut PopupMenu, _, cx: &mut Context<PopupMenu>| {
        let menu = cx.entity().downgrade();
        let menus = &mut cx.default_global::<Menus>().0;
        menus.retain(|menu| menu.upgrade().is_some());
        menus.push(menu);
        // 关闭后焦点可能仍留在它上面（打开前没有焦点时 gpui-kit 不移走焦点），不能只看焦点
        cx.subscribe_self(|_, _: &DismissEvent, cx| {
            let closed = cx.entity_id();
            cx.default_global::<Menus>()
                .0
                .retain(|menu| menu.entity_id() != closed);
        })
        .detach();
    })
    .detach();
}

/// 这个窗口中有弹出菜单开着（焦点在菜单或它的子菜单里）。
pub fn menu_open(window: &Window, cx: &App) -> bool {
    cx.try_global::<Menus>().is_some_and(|menus| {
        menus.0.iter().filter_map(WeakEntity::upgrade).any(|menu| {
            let focus = menu.read(cx).focus_handle(cx);
            focus.is_focused(window) || focus.contains_focused(window, cx)
        })
    })
}

/// 包一层 GPUI 悬停提示的构建：到要显示时窗口中开着弹出菜单就显示空的。给拿不到 `window` 的地方（Tab 上的标记
/// 等）用；拿得到时用 [`TooltipExt::tooltip_unless_menu`]（菜单关上后提示马上照常）。
pub fn quiet(
    build: impl Fn(&mut Window, &mut App) -> AnyView + 'static,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    move |window, cx| {
        if menu_open(window, cx) {
            cx.new(|_| Blank).into()
        } else {
            build(window, cx)
        }
    }
}

/// 空的提示（什么也不画）。
struct Blank;

impl Render for Blank {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

pub trait TooltipExt: StatefulInteractiveElement + Sized {
    /// GPUI 的悬停提示，`menu_open`（[`menu_open`] 的结果）时不挂。
    fn tooltip_unless_menu(
        self,
        menu_open: bool,
        build: impl Fn(&mut Window, &mut App) -> AnyView + 'static,
    ) -> Self {
        if menu_open { self } else { self.tooltip(build) }
    }
}

impl<E: StatefulInteractiveElement> TooltipExt for E {}

#[cfg(test)]
mod tests {
    use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        InteractiveElement as _, ParentElement as _, Styled as _, TestAppContext,
        TestSupportExt as _, px,
    };

    use super::*;

    struct Row;

    impl Render for Row {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(
                div()
                    .id("row")
                    .test_support()
                    .w(px(200.))
                    .h(px(30.))
                    .child("row")
                    .context_menu(|menu, _, _| menu.item(PopupMenuItem::new("Open"))),
            )
        }
    }

    // 右键打开菜单后为真；按 Esc 关闭菜单后为假；另一个窗口不受影响
    #[gpui_kit::test]
    fn menu_open_follows_the_context_menu(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        cx.update(init);
        let open = |cx: &mut TestAppContext| {
            cx.update(|cx| {
                gpui_kit::open_window(Default::default(), cx, |_, cx| cx.new(|_| Row))
                    .unwrap()
                    .0
            })
        };
        let (handle, other) = (open(cx), open(cx));
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(!menu_open(window, cx));
            window.right_click("row", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(menu_open(window, cx), "右键菜单开着");
        })
        .unwrap();
        cx.update_window(other, |_, window, cx| {
            window.render_frame(cx);
            assert!(!menu_open(window, cx), "只看自己的窗口");
        })
        .unwrap();
        cx.update_window(handle, |_, window, cx| window.press("escape", cx))
            .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(!menu_open(window, cx), "菜单关闭后");
        })
        .unwrap();
    }
}
