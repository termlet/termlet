//! 弹出菜单开着时不显示悬停提示。
//!
//! 指针停在列表的一行上、提示出现（或正等着出现）时右键，菜单打开后提示仍会出现并浮在菜单之上，盖住菜单的第一项：
//! - GPUI 的提示（元素的 `.tooltip(...)`）按下鼠标时只收起已经出现的，等待中的照样到时出现；出现后按元素的范围
//!   判断指针还在不在上面，不看是否被菜单遮住（gpui-pre 的 `div.rs` 中也注明了这一点）。
//! - gpui-kit 组件（按钮等）的提示只在按下左键时收起；下拉菜单开着时，旁边按钮的提示也会浮在菜单上。
//!
//! gpui-kit 没有现成的做法。这里记下所有弹出菜单（[`init`]），焦点在其中一个里时 [`menu_open`] 为真。挂了右键菜单的
//! 行、Tab 标题与其中的按钮，以及挨着下拉菜单、右键菜单的工具条（查找栏、传输栏、状态条等）：
//! - GPUI 的提示用 [`quiet`] 包一层：每次画提示时看一遍，菜单开着时什么也不画，关上后照常；
//! - gpui-kit 组件的提示用 `.when(!menu_open, …)`：菜单打开时焦点移到菜单里、窗口随之重绘，gpui-kit 在这一帧收起
//!   已有的提示，菜单关闭后照常显示。

use std::rc::Rc;

use gpui_kit::component::menu::PopupMenu;
use gpui_kit::{
    AnyView, App, AppContext as _, Context, DismissEvent, Focusable as _, Global, IntoElement,
    Render, WeakEntity, Window, div,
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

type Build = Rc<dyn Fn(&mut Window, &mut App) -> AnyView>;

/// 包一层 GPUI 悬停提示的构建（`.tooltip(quiet(...))`）：窗口中开着弹出菜单时提示什么也不画，菜单关上后画 `build`
/// 建的提示。提示随窗口每帧重画、每次都看一遍：右键时正等着出现的提示在菜单开着时出现，是空的；菜单关上后指针
/// 仍停在元素上时随即显示，不用移出再移入。
pub fn quiet(
    build: impl Fn(&mut Window, &mut App) -> AnyView + 'static,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let build: Build = Rc::new(build);
    move |_, cx| {
        let build = build.clone();
        cx.new(|_| Quiet { build, shown: None }).into()
    }
}

/// `tooltip` 是 [`quiet`] 包过的提示（给别处的测试确认用了它）。
#[cfg(test)]
pub fn is_quiet(tooltip: AnyView) -> bool {
    tooltip.downcast::<Quiet>().is_ok()
}

/// [`quiet`] 的提示。
struct Quiet {
    build: Build,
    /// 建好的提示（第一次在菜单关着时画时建）
    shown: Option<AnyView>,
}

impl Render for Quiet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if menu_open(window, cx) {
            return div().into_any_element();
        }
        let build = self.build.clone();
        self.shown
            .get_or_insert_with(|| build(window, cx))
            .clone()
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        InteractiveElement as _, ParentElement as _, StatefulInteractiveElement as _, Styled as _,
        TestAppContext, TestSupportExt as _, px,
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
                    .tooltip(quiet(|_, cx| cx.new(|_| Tip).into()))
                    .context_menu(|menu, _, _| menu.item(PopupMenuItem::new("Open"))),
            )
        }
    }

    /// 测试用的提示：能按 ID 找到。
    struct Tip;

    impl Render for Tip {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().id("tip").test_support().child("tip")
        }
    }

    fn open(cx: &mut TestAppContext) -> gpui_kit::AnyWindowHandle {
        cx.update(|cx| {
            gpui_kit::open_window(Default::default(), cx, |_, cx| cx.new(|_| Row))
                .unwrap()
                .0
        })
    }

    // 右键打开菜单后为真；按 Esc 关闭菜单后为假；另一个窗口不受影响
    #[gpui_kit::test]
    fn menu_open_follows_the_context_menu(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        cx.update(init);
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

    // 右键时正等着出现的提示在菜单开着时出现，是空的；菜单关上后指针没动，提示随即显示
    #[gpui_kit::test]
    fn quiet_tooltip_shows_once_the_menu_closes(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        cx.update(init);
        let handle = open(cx);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.hover("row", cx);
            window.right_click("row", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(menu_open(window, cx), "右键菜单开着");
            assert!(window.try_find("tip").is_none(), "菜单开着时提示是空的");
        })
        .unwrap();
        // 不经键盘与鼠标关上菜单（按键、按下鼠标时 GPUI 自己会收起提示）
        let menu = cx
            .read(|cx| cx.global::<Menus>().0.iter().find_map(WeakEntity::upgrade))
            .expect("开着的菜单");
        cx.update(|cx| menu.update(cx, |_, cx| cx.emit(DismissEvent)));
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(!menu_open(window, cx), "菜单关上了");
            assert!(window.try_find("tip").is_some(), "提示随即显示");
        })
        .unwrap();
    }
}
