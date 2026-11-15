//! 设置中的“工具栏”：标题栏右侧显示哪些按钮、按什么顺序（本机设置，不同步）；“恢复默认”。规则见
//! [`crate::toolbar`]。标题栏按钮的右键菜单“自定义工具栏…”打开这一页，“隐藏这个按钮”立即保存、不经过这里。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, TestSupportExt as _, Window, div, prelude::FluentBuilder as _,
};
use termlet_core::model::{LocalSettings, ToolbarButton};

use crate::prose::prose;
use crate::t;
use crate::toolbar::{self, Entry, ToolbarItem};

pub(super) struct ToolbarPage {
    entries: Vec<Entry>,
    /// 设置中不认识的按钮（更新的版本加的）：保存时原样附在后面
    unknown: Vec<ToolbarButton>,
}

impl ToolbarPage {
    pub(super) fn new(saved: &[ToolbarButton]) -> Self {
        Self {
            entries: toolbar::resolve(saved),
            unknown: toolbar::unknown(saved),
        }
    }

    /// 写进本机设置的草稿。
    pub(super) fn apply(&self, local: &mut LocalSettings) {
        local.window.toolbar = toolbar::to_saved(&self.entries, &self.unknown);
    }

    fn set_shown(&mut self, item: ToolbarItem, shown: bool, cx: &mut Context<Self>) {
        if let Some(entry) = self.entries.iter_mut().find(|(i, _)| *i == item) {
            entry.1 = shown;
        }
        cx.notify();
    }

    fn move_item(&mut self, item: ToolbarItem, up: bool, cx: &mut Context<Self>) {
        if toolbar::move_entry(&mut self.entries, item, up) {
            cx.notify();
        }
    }

    fn reset(&mut self, cx: &mut Context<Self>) {
        tracing::debug!("工具栏恢复默认（保存后生效）");
        self.entries = toolbar::defaults();
        cx.notify();
    }

    fn render_row(&self, ix: usize, (item, shown): Entry, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let id = |name: &str| SharedString::from(format!("toolbar-{name}-{}", item.id()));
        let last = ix + 1 == self.entries.len();
        h_flex()
            .id(id("row"))
            .px_3()
            .py_2()
            .gap_2()
            .items_center()
            .when(!last, |row| row.border_b_1().border_color(theme.border))
            .child(
                div().flex_1().min_w_0().child(
                    Checkbox::new(id("show"))
                        .checked(shown)
                        .accessibility_label(item.title())
                        // 图标在左，名称与出现的条件上下对齐
                        .child(
                            h_flex()
                                .gap_2()
                                .items_start()
                                .child(Icon::new(item.icon()).small().text_color(muted))
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .gap_1()
                                        .child(item.title())
                                        .when_some(item.condition(), |el, condition| {
                                            el.child(
                                                div()
                                                    .text_xs()
                                                    .text_color(muted)
                                                    .child(prose(condition)),
                                            )
                                        }),
                                ),
                        )
                        .on_change(cx.listener(move |this, checked: &bool, _, cx| {
                            this.set_shown(item, *checked, cx)
                        })),
                ),
            )
            .child(
                Button::new(id("up"))
                    .ghost()
                    .xsmall()
                    .icon(IconName::ArrowUp)
                    .tooltip(t!("common.move_up"))
                    .disabled(ix == 0)
                    .on_click(cx.listener(move |this, _, _, cx| this.move_item(item, true, cx))),
            )
            .child(
                Button::new(id("down"))
                    .ghost()
                    .xsmall()
                    .icon(IconName::ArrowDown)
                    .tooltip(t!("common.move_down"))
                    .disabled(last)
                    .on_click(cx.listener(move |this, _, _, cx| this.move_item(item, false, cx))),
            )
            .into_any_element()
    }
}

impl Render for ToolbarPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<_> = self
            .entries
            .clone()
            .into_iter()
            .enumerate()
            .map(|(ix, entry)| self.render_row(ix, entry, cx))
            .collect();
        let is_default = self.entries == toolbar::defaults();
        let theme = cx.theme();
        v_flex()
            .id("settings-toolbar")
            .test_support()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(prose(t!("toolbar.settings_hint"))),
            )
            .child(
                v_flex()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .children(rows),
            )
            .child(
                h_flex().child(
                    Button::new("toolbar-reset")
                        .outline()
                        .small()
                        .icon(IconName::RotateCcw)
                        .label(t!("toolbar.reset"))
                        .disabled(is_default)
                        .on_click(cx.listener(|this, _, _, cx| this.reset(cx))),
                ),
            )
    }
}
