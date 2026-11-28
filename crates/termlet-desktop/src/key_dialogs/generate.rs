//! 生成密钥：Ed25519（推荐）或 RSA 3072 / 4096，可选注释；新密钥直接存入密钥库，生成后打开它的公钥。

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::radio::RadioGroup;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, Entity, Focusable as _, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Task, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::keyvault::{self, GenerateRequest, KeyKind};

use super::errors;
use crate::app_state::AppState;
use crate::choice::{Choice, ChoiceState, choice_state, chosen};
use crate::dialog_token::{DialogToken, close_own};
use crate::prose::prose;
use crate::t;

pub fn open(window: &mut Window, cx: &mut App) {
    let form = cx.new(|cx| GenerateForm::new(window, cx));
    let first = Rc::new(Cell::new(true));
    let width = rems(30.).to_pixels(window.rem_size());
    let owner = form.clone();
    window.open_dialog(cx, move |dialog, window, cx| {
        if first.replace(false) {
            form.read(cx).name.focus_handle(cx).focus(window, cx);
        }
        let ok = {
            let form = form.clone();
            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                form.update(cx, |form, cx| form.submit(window, cx));
                false
            }
        };
        dialog
            .title(t!("keys.generate"))
            .w(width)
            .overlay_closable(false)
            .child(form.clone())
            .on_ok(ok)
    });
    let token = DialogToken::take(window, cx);
    owner.update(cx, |form, _| form.dialog = token);
}

struct GenerateForm {
    name: Entity<InputState>,
    rsa: bool,
    bits: Entity<ChoiceState<u32>>,
    comment: Entity<InputState>,
    busy: bool,
    error: Option<SharedString>,
    /// 这个表单所在的对话框（完成后只关它）
    dialog: Option<DialogToken>,
    _task: Option<Task<()>>,
}

impl GenerateForm {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder(t!("keys.generate_name")));
        let comment =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("keys.generate_comment")));
        let bits = choice_state(
            vec![Choice::new("3072", 3072u32), Choice::new("4096", 4096u32)],
            &3072,
            false,
            window,
            cx,
        );
        Self {
            name,
            rsa: false,
            bits,
            comment,
            busy: false,
            error: None,
            dialog: None,
            _task: None,
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let kind = if !self.rsa {
            KeyKind::Ed25519
        } else if chosen(&self.bits, cx) == Some(4096) {
            KeyKind::Rsa4096
        } else {
            KeyKind::Rsa3072
        };
        let request = GenerateRequest {
            name: self.name.read(cx).value().trim().to_string(),
            kind,
            comment: self.comment.read(cx).value().trim().to_string(),
        };
        let store = AppState::store(cx);
        self.busy = true;
        self.error = None;
        tracing::info!(name = %request.name, ?kind, "生成密钥");
        let job = termlet_core::runtime::spawn(keyvault::generate(store, request));
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(Ok(id)) => {
                        tracing::info!(key = %id, "已生成密钥");
                        this.busy = true;
                        close_own(this.dialog.as_ref(), window, cx);
                        // 生成后直接看公钥（要加到服务器上）
                        super::view::open(id, window, cx);
                    }
                    Ok(Err(error)) => {
                        tracing::warn!("生成密钥失败：{error}");
                        this.error = Some(errors::describe(&error))
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }
}

impl Render for GenerateForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let radio = RadioGroup::horizontal("key-generate-kind")
            .children(["Ed25519", "RSA"])
            .selected_index(Some(usize::from(self.rsa)))
            .on_change(cx.listener(|this, ix: &usize, _, cx| {
                this.rsa = *ix == 1;
                cx.notify();
            }));
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(prose(t!("keys.generate_hint"))),
            )
            .child(Input::new(&self.name).id("key-generate-name"))
            .child(
                h_flex()
                    .gap_3()
                    .child(div().text_sm().child(t!("keys.type")))
                    .child(radio)
                    .when(self.rsa, |this| {
                        this.child(div().text_sm().child(t!("keys.rsa_bits")))
                            .child(
                                div().w(rems(7.)).child(
                                    crate::choice::select(&self.bits).id("key-generate-bits"),
                                ),
                            )
                    }),
            )
            .child(Input::new(&self.comment).id("key-generate-comment"))
            .when_some(self.error.clone(), |this, error| {
                this.child(div().text_sm().text_color(danger).child(prose(error)))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("key-generate-cancel")
                            .ghost()
                            .label(t!("common.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("key-generate-submit")
                            .primary()
                            .label(if self.busy {
                                t!("keys.generating")
                            } else {
                                t!("keys.generate_submit")
                            })
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}
