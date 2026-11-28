//! 粘贴前检查的确认框（设置“粘贴前检查”）：列出原因，要粘贴的内容可以修改，确认后再粘贴。
//!
//! 检查本身在核心（[`termlet_core::guard::inspect_paste`]）：控制字符一律去掉；会立即执行的多行内容、
//! 危险命令、下载后交给 shell、看不见的字符、同形字先确认。

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, StyledExt as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, Focusable as _, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, WeakEntity, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::guard::{PasteCheck, PasteFinding};

use crate::i18n;
use crate::prose::prose;
use crate::t;
use crate::terminal_panel::TerminalPanel;
use crate::ui_scale::DialogTitleExt as _;

/// 确认框关闭后的回调：确认时为（可能改过的）内容，取消、关闭时为 `None`。
pub type OnPasteDone = Box<dyn FnOnce(Option<String>, &mut Window, &mut App)>;

/// 显示确认框；确认后把（可能改过的）内容粘贴到 `target`。
pub fn confirm(
    check: PasteCheck,
    target: WeakEntity<TerminalPanel>,
    window: &mut Window,
    cx: &mut App,
) {
    confirm_with(
        check,
        None,
        Box::new(move |text, window, cx| {
            if let (Some(text), Some(panel)) = (text, target.upgrade()) {
                panel.update(cx, |panel, cx| panel.paste_checked(&text, window, cx));
            }
        }),
        window,
        cx,
    );
}

/// 显示确认框，结果交给 `done`（同步输入粘贴到多个终端时用，`note` 说明粘贴到几个终端）。
pub fn confirm_with(
    check: PasteCheck,
    note: Option<SharedString>,
    done: OnPasteDone,
    window: &mut Window,
    cx: &mut App,
) {
    let reasons = reasons(&check);
    let danger = check.danger.is_some();
    let done = Rc::new(std::cell::RefCell::new(Some(done)));
    let form = cx.new(|cx| {
        let text = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(6)
                .default_value(check.text.clone())
        });
        PasteForm {
            reasons,
            note,
            danger,
            text,
            done: done.clone(),
        }
    });
    let first = Rc::new(Cell::new(true));
    let width = rems(34.).to_pixels(window.rem_size());
    window.open_dialog(cx, move |dialog, window, cx| {
        if first.replace(false) {
            form.read(cx).text.focus_handle(cx).focus(window, cx);
        }
        let done = done.clone();
        dialog
            .titled(t!("paste.title"))
            .w(width)
            .overlay_closable(false)
            .child(form.clone())
            // Esc、右上角关闭：当作取消（确认时回调已经取走）
            .on_close(move |_, window, cx| {
                if let Some(done) = done.borrow_mut().take() {
                    done(None, window, cx);
                }
            })
    });
}

/// 确认框中列出的原因。
fn reasons(check: &PasteCheck) -> Vec<SharedString> {
    let mut out = Vec::new();
    if let Some(danger) = &check.danger {
        let rule = i18n::rule_description(&danger.rule);
        let rule = if rule.is_empty() {
            danger.rule.pattern()
        } else {
            rule
        };
        out.push(t!(
            "paste.danger",
            command = danger.line.clone(),
            rule = rule
        ));
    }
    for finding in &check.findings {
        out.push(match finding {
            PasteFinding::Multiline { lines } => t!("paste.multiline", count = lines),
            PasteFinding::PipeShell => t!("paste.pipe_shell"),
            PasteFinding::Invisible { chars } => {
                let chars = chars
                    .iter()
                    .map(|c| format!("U+{:04X}", *c as u32))
                    .collect::<Vec<_>>()
                    .join(", ");
                t!("paste.invisible", chars = chars)
            }
            PasteFinding::Homoglyph { words } => t!("paste.homoglyph", words = words.join(", ")),
            PasteFinding::Control { count } => t!("paste.control", count = count),
        });
    }
    out
}

struct PasteForm {
    reasons: Vec<SharedString>,
    /// 粘贴到多个终端时的说明
    note: Option<SharedString>,
    /// 有危险命令：原因的圆点为红色，“粘贴”按钮为警示样式
    danger: bool,
    text: Entity<TextareaState>,
    done: Rc<std::cell::RefCell<Option<OnPasteDone>>>,
}

impl PasteForm {
    fn paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.text.read(cx).value().to_string();
        if text.is_empty() {
            return;
        }
        let done = self.done.borrow_mut().take();
        window.close_dialog(cx);
        if let Some(done) = done {
            done(Some(text), window, cx);
        }
    }

    /// “取消”按钮：程序关闭对话框不会触发 `on_close`，在这里回调（同步输入等着它才继续）。
    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let done = self.done.borrow_mut().take();
        window.close_dialog(cx);
        if let Some(done) = done {
            done(None, window, cx);
        }
    }
}

impl Render for PasteForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .gap_3()
            .when_some(self.note.clone(), |el, note| {
                el.child(div().text_sm().font_medium().child(prose(note)))
            })
            .child(div().text_sm().child(prose(t!("paste.hint"))))
            .child(v_flex().gap_1().children(self.reasons.iter().map(|reason| {
                h_flex()
                    .gap_2()
                    .items_start()
                    .text_sm()
                    .text_color(if self.danger {
                        theme.danger
                    } else {
                        theme.warning
                    })
                    .child("•")
                    .child(div().flex_1().min_w_0().child(prose(reason.clone())))
            })))
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(t!("paste.content")),
                    )
                    .child(
                        Textarea::new(&self.text)
                            .font_family(termlet_terminal::fonts::DEFAULT_FAMILY),
                    ),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .pt_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("paste-cancel")
                            .label(t!("common.cancel"))
                            .on_click(cx.listener(|this, _, window, cx| this.cancel(window, cx))),
                    )
                    .child(
                        Button::new("paste-confirm")
                            .with_variant(if self.danger {
                                ButtonVariant::Danger
                            } else {
                                ButtonVariant::Primary
                            })
                            .label(t!("paste.confirm"))
                            .disabled(self.text.read(cx).value().is_empty())
                            .on_click(cx.listener(|this, _, window, cx| this.paste(window, cx))),
                    ),
            )
    }
}
