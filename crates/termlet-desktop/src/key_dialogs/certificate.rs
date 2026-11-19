//! 密钥附带的 OpenSSH 证书：在“查看公钥”中显示证书的信息（类型、签发的 CA、序号、标识、有效期、允许的用户名、
//! 扩展项与关键选项），附带（证书文件或粘贴的一行）、替换、去掉。
//!
//! 证书不是秘密，与密钥条目一起保存、同步；用这把密钥登录时先出示证书。

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::radio::RadioGroup;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _, WindowExt as _, h_flex,
    v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement as _, PathPromptOptions, Render, SharedString, Styled as _,
    Subscription, Task, TestSupportExt as _, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::id::KeyId;
use termlet_core::keyvault::{self, CertificateSource};
use termlet_core::sshcert::{self, CertInfo, CertKind, Validity};

use super::errors;
use crate::app_state::AppState;
use crate::dialog_token::{DialogToken, close_own};
use crate::prose::prose;
use crate::t;

/// 证书的生效、失效时间：完整到秒。
fn full_time(secs: i64) -> String {
    termlet_core::time::format_seconds(
        secs.saturating_mul(1000),
        crate::sftp::file_list::date_style(),
    )
}

/// “查看公钥”中证书的一节：没有证书时是说明与“附带证书”，有证书时是证书的信息与“替换”“去掉”。
pub fn section(id: &KeyId, certificate: &str, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let title = div().text_sm().font_medium().child(t!("keys.certificate"));
    let parsed = (!certificate.is_empty())
        .then(|| sshcert::parse(certificate).ok())
        .flatten();
    let Some(parsed) = parsed else {
        let attach = id.clone();
        return v_flex()
            .gap_1()
            .child(
                h_flex().gap_2().child(div().flex_1().child(title)).child(
                    Button::new("key-cert-attach")
                        .outline()
                        .small()
                        .label(t!("keys.certificate_attach"))
                        .on_click(move |_, window, cx| open_attach(attach.clone(), window, cx)),
                ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(prose(t!("keys.certificate_none"))),
            )
            .into_any_element();
    };
    let info = CertInfo::of(&parsed);
    let line = |label: SharedString, value: AnyElement| {
        h_flex()
            .gap_2()
            .items_start()
            .text_xs()
            .child(div().flex_none().w(rems(7.)).text_color(muted).child(label))
            .child(div().flex_1().min_w_0().child(value))
    };
    let text = |value: String| div().child(value).into_any_element();
    let mono = |value: String| {
        div()
            .font_family(theme.mono_font_family.clone())
            .child(value)
            .into_any_element()
    };
    let kind = match info.kind {
        CertKind::User => t!("keys.cert.kind_user"),
        CertKind::Host => t!("keys.cert.kind_host"),
    };
    let start = info
        .valid_after
        .map(full_time)
        .unwrap_or_else(|| t!("keys.cert.no_start").to_string());
    let end = info
        .valid_before
        .map(full_time)
        .unwrap_or_else(|| t!("keys.cert.no_end").to_string());
    let status = match info.validity_at(sshcert::now_secs()) {
        Validity::Expired { .. } => Some((t!("keys.cert.expired"), theme.danger)),
        Validity::NotYetValid { .. } => Some((t!("keys.cert.not_yet_valid"), theme.warning)),
        Validity::Valid { .. } => None,
    };
    let range = if info.valid_after.is_none() && info.valid_before.is_none() {
        t!("keys.cert.forever")
    } else {
        t!("keys.cert.validity_range", from = start, until = end)
    };
    let validity = h_flex()
        .gap_2()
        .flex_wrap()
        .child(range)
        .when_some(status, |this, (label, color)| {
            this.child(div().text_color(color).child(label))
        })
        .into_any_element();
    let options = |items: &[(String, String)]| {
        if items.is_empty() {
            return t!("keys.cert.none").to_string();
        }
        items
            .iter()
            .map(|(name, value)| {
                if value.is_empty() {
                    name.clone()
                } else {
                    format!("{name}={value}")
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let principals = if info.principals.is_empty() {
        t!("keys.cert.any_principal").to_string()
    } else {
        info.principals.join(", ")
    };
    let (replace, remove) = (id.clone(), id.clone());
    // 标题行带操作（对话框不高，证书的信息用小字）
    let header = h_flex()
        .gap_2()
        .child(div().flex_1().child(title))
        .child(
            Button::new("key-cert-replace")
                .outline()
                .small()
                .label(t!("keys.certificate_replace"))
                .on_click(move |_, window, cx| open_attach(replace.clone(), window, cx)),
        )
        .child(
            Button::new("key-cert-remove")
                .ghost()
                .small()
                .label(t!("keys.certificate_remove"))
                .on_click(move |_, window, cx| detach(remove.clone(), window, cx)),
        );
    v_flex()
        .gap_1()
        .child(header)
        .child(line(t!("keys.type"), text(kind.to_string())))
        .child(line(
            t!("keys.cert.ca"),
            mono(format!("{} {}", info.ca_algorithm, info.ca_fingerprint)),
        ))
        .child(line(t!("keys.cert.serial"), text(info.serial.to_string())))
        .when(!info.key_id.is_empty(), |this| {
            this.child(line(t!("keys.cert.key_id"), text(info.key_id.clone())))
        })
        .child(line(t!("keys.cert.validity"), validity))
        .child(line(t!("keys.cert.principals"), text(principals)))
        .child(line(
            t!("keys.cert.extensions"),
            text(options(&info.extensions)),
        ))
        .child(line(
            t!("keys.cert.critical_options"),
            text(options(&info.critical_options)),
        ))
        .into_any_element()
}

/// 去掉证书（不确认：可以重新附带）。出错时说明写在查看公钥的对话框底部（[`crate::notify::show`]）。
fn detach(id: KeyId, window: &mut Window, cx: &mut App) {
    let store = AppState::store(cx);
    tracing::info!(id = %id, "去掉密钥附带的证书");
    let job = termlet_core::runtime::spawn(keyvault::detach_certificate(store, id));
    window
        .spawn(cx, async move |cx| {
            let result = job.await;
            let _ = cx.update(|window, cx| match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => crate::notify::show(
                    crate::notify::Notice::error(errors::describe(&error)),
                    window,
                    cx,
                ),
                Err(error) => {
                    crate::notify::show(crate::notify::Notice::error(error.to_string()), window, cx)
                }
            });
        })
        .detach();
}

/// 附带（或替换）证书的对话框。
pub fn open_attach(id: KeyId, window: &mut Window, cx: &mut App) {
    let form = cx.new(|cx| AttachForm::new(id, window, cx));
    let first = Rc::new(Cell::new(true));
    let width = rems(30.).to_pixels(window.rem_size());
    let owner = form.clone();
    window.open_dialog(cx, move |dialog, window, cx| {
        if first.replace(false) {
            form.read(cx).path.focus_handle(cx).focus(window, cx);
        }
        let ok = {
            let form = form.clone();
            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                form.update(cx, |form, cx| form.submit(window, cx));
                false
            }
        };
        dialog
            .title(t!("keys.certificate_attach"))
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

struct AttachForm {
    id: KeyId,
    from: Source,
    path: Entity<InputState>,
    pasted: Entity<TextareaState>,
    busy: bool,
    error: Option<SharedString>,
    dialog: Option<DialogToken>,
    _task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl AttachForm {
    fn new(id: KeyId, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let path =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("keys.certificate_path")));
        let pasted = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(4)
                .placeholder("ssh-ed25519-cert-v01@openssh.com AAAA…")
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
            id,
            from: Source::File,
            path,
            pasted,
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
            Source::File => {
                CertificateSource::File(PathBuf::from(self.path.read(cx).value().trim()))
            }
            Source::Pasted => CertificateSource::Text(self.pasted.read(cx).value().to_string()),
        };
        let store = AppState::store(cx);
        self.busy = true;
        self.error = None;
        tracing::info!(id = %self.id, pasted = matches!(source, CertificateSource::Text(_)), "附带证书");
        let job = termlet_core::runtime::spawn(keyvault::attach_certificate(
            store,
            self.id.clone(),
            source,
        ));
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(Ok(())) => {
                        this.busy = true;
                        close_own(this.dialog.as_ref(), window, cx);
                    }
                    Ok(Err(error)) => {
                        tracing::info!("附带证书失败：{error}");
                        this.error = Some(errors::describe(&error));
                    }
                    Err(error) => this.error = Some(error.to_string().into()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }
}

impl Render for AttachForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let froms = [Source::File, Source::Pasted];
        let from = self.from;
        let radio = RadioGroup::horizontal("key-cert-from")
            .children([
                t!("keys.certificate_from_file"),
                t!("keys.certificate_from_paste"),
            ])
            .selected_index(froms.iter().position(|f| *f == from))
            .on_change(cx.listener(move |this, ix: &usize, window, cx| {
                this.from = froms[*ix];
                this.error = None;
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
                        .child(Input::new(&self.path).id("key-cert-path")),
                )
                .child(
                    Button::new("key-cert-browse")
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
                    .child(prose(t!("keys.certificate_attach_hint"))),
            )
            .child(radio)
            .child(source)
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .id("key-cert-error")
                        .test_support()
                        .text_sm()
                        .text_color(danger)
                        .child(prose(error)),
                )
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("key-cert-cancel")
                            .ghost()
                            .label(t!("common.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("key-cert-submit")
                            .primary()
                            .label(t!("keys.certificate_attach_submit"))
                            .loading(self.busy)
                            .disabled(self.busy || empty)
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}
