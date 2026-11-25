//! 设置页中的分节：一节的标题，低频的一节可以展开、收起（默认收起：一页默认显示的控件不超过 12 个，见 CLAUDE.md 的
//! “防臃肿”）。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, StyledExt as _, h_flex};
use gpui_kit::{
    App, ClickEvent, Div, ElementId, ParentElement as _, SharedString, Styled as _, Window, div,
    prelude::FluentBuilder as _,
};

/// 一节的标题；`first` 时是页上的第一节，不画与上一节之间的分隔线。
pub(crate) fn heading(title: SharedString, first: bool, cx: &App) -> Div {
    div()
        .when(!first, |this| {
            this.pt_3().border_t_1().border_color(cx.theme().border)
        })
        .text_sm()
        .font_medium()
        .child(title)
}

/// 可以展开、收起的一节的标题：箭头与标题，点了展开或收起。`divider` 时与上一节之间有分隔线（一节中的一部分收起时
/// 不画）。`id` 是标题按钮的元素 ID。
pub(crate) fn fold(
    id: impl Into<ElementId>,
    title: SharedString,
    open: bool,
    divider: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Div {
    div()
        .when(divider, |this| {
            this.pt_3().border_t_1().border_color(cx.theme().border)
        })
        // 放在一行里：按钮只占文字的宽度、靠左，不撑满整行
        .child(
            h_flex().child(
                Button::new(id)
                    .text()
                    .small()
                    .icon(if open {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .label(title)
                    .on_click(on_toggle),
            ),
        )
}
