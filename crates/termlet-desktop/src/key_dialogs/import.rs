//! 导入私钥：私钥文件或粘贴的内容（OpenSSH、PEM、PuTTY .ppk），已加密时再填私钥密码。
//! 私钥解密后加密保存在本机的保险库中，之后连接不再需要私钥密码。

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::radio::RadioGroup;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, Entity, Focusable as _, IntoElement, ParentElement as _,
    PathPromptOptions, Render, SharedString, Styled as _, Subscription, Task, Window, div,
    prelude::FluentBuilder as _, rems,
};
use termlet_core::keyvault::{self, ImportRequest, ImportSource, Imported, KeyVaultError};
use termlet_core::secret::SecretString;
use termlet_core::sshkey::KeyError;

use super::errors;
use crate::app_state::AppState;
use crate::dialog_token::{DialogToken, close_own};
use crate::prose::prose;
use crate::t;

pub fn open(window: &mut Window, cx: &mut App) {
    let form = cx.new(|cx| ImportForm::new(window, cx));
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
            .title(t!("keys.import"))
            .w(width)
            .overlay_closable(false)
            .child(form.clone())
            .on_ok(ok)
    });
    let token = DialogToken::take(window, cx);
    owner.update(cx, |form, _| form.dialog = token);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    File,
    Pasted,
}

struct ImportForm {
    name: Entity<InputState>,
    from: Source,
    path: Entity<InputState>,
    pasted: Entity<TextareaState>,
    passphrase: Entity<InputState>,
    /// 私钥已加密：显示私钥密码框
    needs_passphrase: bool,
    busy: bool,
    error: Option<SharedString>,
    /// 这个表单所在的对话框（完成后只关它）
    dialog: Option<DialogToken>,
    _task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ImportForm {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder(t!("keys.import_name")));
        let path = cx.new(|cx| InputState::new(window, cx).placeholder(t!("keys.import_path")));
        let pasted = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(6)
                .placeholder(t!("keys.import_paste_placeholder"))
        });
        let passphrase = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t!("keys.passphrase"))
        });
        let subscriptions = vec![
            cx.subscribe(&path, |this: &mut Self, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.error = None;
                    cx.notify();
                }
            }),
            cx.observe(&pasted, |this: &mut Self, _, cx| {
                this.error = None;
                cx.notify();
            }),
        ];
        Self {
            name,
            from: Source::File,
            path,
            pasted,
            passphrase,
            needs_passphrase: false,
            busy: false,
            error: None,
            dialog: None,
            _task: None,
            _subscriptions: subscriptions,
        }
    }

    fn content_empty(&self, cx: &App) -> bool {
        match self.from {
            Source::File => self.path.read(cx).value().trim().is_empty(),
            Source::Pasted => self.pasted.read(cx).value().trim().is_empty(),
        }
    }

    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                let text = path.to_string_lossy().to_string();
                this.path
                    .update(cx, |input, cx| input.set_value(text, window, cx));
            });
        })
        .detach();
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.content_empty(cx) {
            return;
        }
        let source = match self.from {
            Source::File => ImportSource::File(PathBuf::from(self.path.read(cx).value().trim())),
            Source::Pasted => {
                ImportSource::Text(SecretString::new(self.pasted.read(cx).value().to_string()))
            }
        };
        let passphrase = self.passphrase.read(cx).value().to_string();
        let request = ImportRequest {
            name: self.name.read(cx).value().trim().to_string(),
            source,
            passphrase: (!passphrase.is_empty()).then(|| SecretString::new(passphrase)),
        };
        let store = AppState::store(cx);
        self.busy = true;
        self.error = None;
        tracing::info!(
            name = %request.name,
            pasted = matches!(request.source, ImportSource::Text(_)),
            with_passphrase = request.passphrase.is_some(),
            "导入私钥"
        );
        let job = termlet_core::runtime::spawn(keyvault::import(store, request));
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(Ok(imported @ (Imported::Added(_) | Imported::Restored(_)))) => {
                        tracing::info!(
                            restored = matches!(imported, Imported::Restored(_)),
                            "已导入私钥"
                        );
                        this.busy = true;
                        close_own(this.dialog.as_ref(), window, cx);
                    }
                    Ok(Err(KeyVaultError::Key(KeyError::PassphraseRequired))) => {
                        // 私钥已加密：出现私钥密码框并获得焦点
                        this.needs_passphrase = true;
                        this.passphrase.focus_handle(cx).focus(window, cx);
                        if !this.passphrase.read(cx).value().is_empty() {
                            this.error = Some(t!("keys.error.passphrase_required"));
                        }
                    }
                    Ok(Err(error)) => {
                        tracing::warn!("导入私钥失败：{error}");
                        if matches!(error, KeyVaultError::Key(KeyError::WrongPassphrase)) {
                            this.needs_passphrase = true;
                        }
                        this.error = Some(errors::describe(&error));
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }
}

impl Render for ImportForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let froms = [Source::File, Source::Pasted];
        let from = self.from;
        let radio = RadioGroup::horizontal("key-import-from")
            .children([t!("host.key.file"), t!("host.key.pasted")])
            .selected_index(froms.iter().position(|f| *f == from))
            .on_change(cx.listener(move |this, ix: &usize, window, cx| {
                this.from = froms[*ix];
                this.error = None;
                // 换了来源：焦点到对应的输入框
                match this.from {
                    Source::File => this.path.focus_handle(cx).focus(window, cx),
                    Source::Pasted => this.pasted.focus_handle(cx).focus(window, cx),
                }
                cx.notify();
            }));
        let source = match from {
            Source::File => h_flex()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .child(Input::new(&self.path).id("key-import-path")),
                )
                .child(
                    Button::new("key-import-browse")
                        .outline()
                        .label(t!("keys.browse"))
                        .on_click(cx.listener(|this, _, window, cx| this.browse(window, cx))),
                )
                .into_any_element(),
            Source::Pasted => Textarea::new(&self.pasted)
                .font_family(theme.mono_font_family.clone())
                .into_any_element(),
        };
        let empty = self.content_empty(cx);
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(prose(t!("keys.import_hint"))),
            )
            .child(Input::new(&self.name).id("key-import-name"))
            .child(radio)
            .child(source)
            .when(self.needs_passphrase, |this| {
                this.child(
                    Input::new(&self.passphrase)
                        .id("key-import-passphrase")
                        .mask_toggle(),
                )
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(div().text_sm().text_color(danger).child(prose(error)))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("key-import-cancel")
                            .ghost()
                            .label(t!("common.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("key-import-submit")
                            .primary()
                            .label(t!("keys.import_submit"))
                            .loading(self.busy)
                            .disabled(self.busy || empty)
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}
