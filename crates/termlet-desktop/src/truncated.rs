//! 放不下时截断的单行文字：末尾显示省略号，悬停时显示全文；放得下时不挂提示（提示不重复界面上已经有的字）。
//!
//! 是否截断按这一帧实际排版的宽度量：画完后把文字按当时的字体、字号排一遍，比可用的宽度宽就是截断了。量的结果给
//! 下一次悬停用（提示在指针停留一会儿后才建，那时已经量过）。提示经 [`crate::menu_tooltip::quiet`]：右键菜单开着时
//! 不画，挂在有右键菜单的行上也不会盖住菜单。
//!
//! 一个元素只能挂一个悬停提示：已经有提示的元素（如按钮）不要再套 [`truncated`]。所在的一行、一个 Tab 已经有自己的
//! 提示时用 [`Clip`]：文字照样截断、量着，全文补进那个提示里。

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{
    AnyView, App, AppContext as _, Div, ElementId, Empty, InteractiveElement as _,
    ParentElement as _, Pixels, SharedString, Stateful, StatefulInteractiveElement as _,
    Styled as _, Window, canvas, div, px,
};

/// 截断显示的文字 `text`；放不下时悬停显示全文。字体、颜色等照常加在返回的元素上；不要给它加内边距（量的是它的
/// 整个宽度）。
pub fn truncated(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Stateful<Div> {
    let text: SharedString = text.into();
    let clip = Clip::default();
    let tip = crate::menu_tooltip::quiet({
        let (clip, text) = (clip.clone(), text.clone());
        move |window, cx| tooltip(clip.full(&text).cloned(), window, cx)
    });
    clip.text(text).id(id).tooltip(tip)
}

/// 量一段截断显示的文字是否放不下，悬停提示挂在别处（整行、整个 Tab 的提示）时用：那个提示在 [`Clip::full`] 有值时
/// 加上全文。每次画时新建一个，与那个提示共用。
#[derive(Clone, Default)]
pub struct Clip(Rc<Cell<bool>>);

impl Clip {
    /// 截断显示的 `text`（不挂提示），画完后量一次是否放不下。不要给它加内边距（量的是它的整个宽度）。
    pub fn text(&self, text: impl Into<SharedString>) -> Div {
        let text: SharedString = text.into();
        let (clipped, measure) = (self.0.clone(), text.clone());
        div().min_w_0().truncate().child(text).child(
            canvas(
                move |bounds, window, _| {
                    clipped.set(overflows(&measure, bounds.size.width, window));
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
    }

    /// 上次画出时放不下的话是 `text`（就是 [`Clip::text`] 显示的那段文字），否则没有。
    pub fn full<'a>(&self, text: &'a SharedString) -> Option<&'a SharedString> {
        self.0.get().then_some(text)
    }
}

/// 悬停提示的内容：有文字时是写着它的提示，没有时是空的（什么也不显示）。
pub fn tooltip(text: Option<SharedString>, window: &mut Window, cx: &mut App) -> AnyView {
    match text {
        Some(text) => Tooltip::new(text).build(window, cx),
        None => cx.new(|_| Empty).into(),
    }
}

/// `text` 按窗口当前的文字样式排成一行，比 `width` 宽。
pub(crate) fn overflows(text: &SharedString, width: Pixels, window: &mut Window) -> bool {
    if text.is_empty() {
        return false;
    }
    // 一行排版不能含换行（显示时同样排在一行）
    let line: SharedString = if text.contains(['\n', '\r']) {
        text.replace(['\n', '\r'], " ").into()
    } else {
        text.clone()
    };
    let style = window.text_style();
    let font_size = style.font_size.to_pixels(window.rem_size());
    let run = style.to_run(line.len());
    let shaped = window
        .text_system()
        .shape_line(line, font_size, &[run], None);
    // 排版的舍入误差
    shaped.width() > width + px(0.5)
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AppContext as _, Context, IntoElement, ParentElement as _, Render, SharedString,
        Styled as _, TestAppContext, Window, div, px,
    };

    use super::{Clip, overflows};

    // 长文字在窄处放不下，短文字在宽处放得下
    #[gpui_kit::test]
    fn overflow_follows_the_text_width(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, _) = cx
            .update(|cx| {
                gpui_kit::open_window(Default::default(), cx, |_, cx| cx.new(|_| gpui_kit::Empty))
            })
            .unwrap();
        cx.update_window(handle, |_, window, _| {
            let long = "alpine-devel@lists.alpinelinux.org-6165ee59.rsa.pub".into();
            assert!(overflows(&long, px(80.), window));
            assert!(!overflows(&"ab".into(), px(400.), window));
            assert!(!overflows(&"".into(), px(0.), window));
            assert!(overflows(
                &"第一行\n第二行很长很长很长".into(),
                px(20.),
                window
            ));
        })
        .unwrap();
    }

    /// 一段文字放在 `width` 宽的地方。
    struct Text {
        clip: Clip,
        width: f32,
    }

    impl Render for Text {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w(px(self.width)).child(
                self.clip
                    .text("alpine-devel@lists.alpinelinux.org-6165ee59.rsa.pub"),
            )
        }
    }

    // 画出来以后才知道：窄处截断了，提示要补上全文；宽处放得下，不补
    #[gpui_kit::test]
    fn clip_reports_the_full_text_only_when_cut(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let text = SharedString::from("alpine-devel@lists.alpinelinux.org-6165ee59.rsa.pub");
        for (width, cut) in [(80., true), (2000., false)] {
            let clip = Clip::default();
            let view = clip.clone();
            let (handle, _) = cx
                .update(|cx| {
                    gpui_kit::open_window(Default::default(), cx, move |_, cx| {
                        cx.new(|_| Text { clip: view, width })
                    })
                })
                .unwrap();
            cx.update_window(handle, |_, window, cx| window.render_frame(cx))
                .unwrap();
            assert_eq!(clip.full(&text).is_some(), cut, "宽 {width}");
        }
    }
}
