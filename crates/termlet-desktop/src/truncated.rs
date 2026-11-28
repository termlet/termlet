//! 放不下时截断的单行文字：末尾显示省略号，悬停时显示全文；放得下时不挂提示（提示不重复界面上已经有的字）。
//!
//! 是否截断按这一帧实际排版的宽度量：画完后把文字按当时的字体、字号排一遍，比可用的宽度宽就是截断了。量的结果给
//! 下一次悬停用（提示在指针停留一会儿后才建，那时已经量过）。提示经 [`crate::menu_tooltip::quiet`]：右键菜单开着时
//! 不画，挂在有右键菜单的行上也不会盖住菜单。
//!
//! 一个元素只能挂一个悬停提示：已经有提示的元素（如按钮）不要再套这个。

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{
    AppContext as _, Div, ElementId, Empty, InteractiveElement as _, ParentElement as _, Pixels,
    SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, Window, canvas, div, px,
};

/// 截断显示的文字 `text`；放不下时悬停显示全文。字体、颜色等照常加在返回的元素上；不要给它加内边距（量的是它的
/// 整个宽度）。
pub fn truncated(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Stateful<Div> {
    let text: SharedString = text.into();
    let clipped = Rc::new(Cell::new(false));
    let measured = clipped.clone();
    let measure = text.clone();
    let tip = crate::menu_tooltip::quiet({
        let text = text.clone();
        move |window, cx| {
            if clipped.get() {
                Tooltip::new(text.clone()).build(window, cx)
            } else {
                cx.new(|_| Empty).into()
            }
        }
    });
    div()
        .id(id)
        .min_w_0()
        .truncate()
        .child(text)
        .child(
            canvas(
                move |bounds, window, _| {
                    measured.set(overflows(&measure, bounds.size.width, window));
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .tooltip(tip)
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
    use gpui_kit::{AppContext as _, TestAppContext, px};

    use super::overflows;

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
}
