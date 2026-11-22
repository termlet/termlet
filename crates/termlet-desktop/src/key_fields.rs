//! 一组私钥字段：来源（密钥库、私钥文件、粘贴的内容）与私钥密码；私钥文件可另指定 OpenSSH 证书文件
//! （留空时连接时自动找同名的 `-cert.pub`），粘贴的私钥可附带证书（粘贴证书的一行或从文件读入，保存时检查是
//! 用户证书、与私钥配对），密钥库的密钥附带证书时说明会先出示证书。
//! 主机认证、命令行工具专用凭据、凭据档案都用它。

use gpui_kit::component::button::Button;
use gpui_kit::component::form::field;
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::radio::RadioGroup;
use gpui_kit::component::select::Select;
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, IntoElement, ParentElement as _,
    PathPromptOptions, SharedString, Styled as _, Window, div, prelude::FluentBuilder as _,
};
use termlet_core::id::KeyId;
use termlet_core::model::{KeyEntry, KeySource};
use termlet_core::secret::SecretString;
use termlet_core::ssh::SecretEdit;
use termlet_core::sshcert::{self, CertError, CertKind};
use termlet_core::sshkey::{self, KeyError};
use termlet_core::store::Store;

use crate::choice::{Choice, ChoiceState, choice_state, chosen};
use crate::key_dialogs::errors::describe_cert;
use crate::prose::prose;
use crate::t;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyFrom {
    Library,
    File,
    Pasted,
}

const FROMS: [KeyFrom; 3] = [KeyFrom::Library, KeyFrom::File, KeyFrom::Pasted];

pub struct KeyFields {
    pub from: KeyFrom,
    library: Entity<ChoiceState<Option<KeyId>>>,
    file: Entity<InputState>,
    /// 私钥文件的证书文件（可选）
    certificate: Entity<InputState>,
    pasted: Entity<TextareaState>,
    /// 粘贴的私钥附带的证书（可选，一行 OpenSSH 证书）
    pasted_certificate: Entity<InputState>,
    pub passphrase: Entity<InputState>,
    /// 已保存过粘贴的私钥（留空保持不变）
    saved_pasted: bool,
}

impl KeyFields {
    /// `source` 是原来的私钥来源；`saved_pasted` / `saved_passphrase` 表示已保存过粘贴的私钥 / 私钥密码。
    pub fn new<T: 'static>(
        source: Option<&KeySource>,
        saved_pasted: bool,
        saved_passphrase: bool,
        store: &Store,
        window: &mut Window,
        cx: &mut Context<T>,
    ) -> Self {
        let from = match source {
            Some(KeySource::Library { .. }) => KeyFrom::Library,
            Some(KeySource::File { .. }) => KeyFrom::File,
            Some(KeySource::Pasted { .. }) => KeyFrom::Pasted,
            None if store.list::<KeyEntry>().is_empty() => KeyFrom::File,
            None => KeyFrom::Library,
        };
        let selected = match source {
            Some(KeySource::Library { key }) => Some(key.clone()),
            _ => None,
        };
        let (path, certificate) = match source {
            Some(KeySource::File { path, certificate }) => (path.clone(), certificate.clone()),
            _ => (String::new(), String::new()),
        };
        let pasted_certificate = match source {
            Some(KeySource::Pasted { certificate }) => certificate.clone(),
            _ => String::new(),
        };
        let saved_pasted = saved_pasted && from == KeyFrom::Pasted;
        Self {
            from,
            library: choice_state(key_choices(store), &selected, true, window, cx),
            // 私钥文件有标签与“浏览…”（见 render），不要占位文字
            file: cx.new(|cx| InputState::new(window, cx).default_value(path)),
            certificate: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(t!("host.key.certificate_placeholder"))
                    .default_value(certificate)
            }),
            pasted: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .rows(4)
                    .placeholder(if saved_pasted {
                        t!("host.secret.keep")
                    } else {
                        SharedString::from("-----BEGIN OPENSSH PRIVATE KEY-----")
                    })
            }),
            pasted_certificate: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(t!("host.key.pasted_certificate_placeholder"))
                    .default_value(pasted_certificate)
            }),
            passphrase: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder(if saved_passphrase {
                        t!("host.key.passphrase_keep")
                    } else {
                        t!("host.key.passphrase_placeholder")
                    })
            }),
            saved_pasted,
        }
    }

    /// 选定的私钥来源；粘贴的私钥写进 `pasted_edit`（留空时保持已保存的）。
    pub fn collect(
        &self,
        pasted_edit: &mut SecretEdit,
        cx: &App,
    ) -> Result<KeySource, SharedString> {
        match self.from {
            KeyFrom::Library => chosen(&self.library, cx)
                .flatten()
                .map(|key| KeySource::Library { key })
                .ok_or_else(|| t!("host.error.key_library_required")),
            KeyFrom::File => {
                let path = self.file.read(cx).value().trim().to_string();
                if path.is_empty() {
                    return Err(t!("host.error.key_file_required"));
                }
                let certificate = self.certificate.read(cx).value().trim().to_string();
                Ok(KeySource::File { path, certificate })
            }
            KeyFrom::Pasted => {
                let text = self.pasted.read(cx).value().trim().to_string();
                if text.is_empty() && !self.saved_pasted {
                    return Err(t!("host.error.key_pasted_required"));
                }
                let certificate = self.checked_certificate(&text, cx)?;
                *pasted_edit = if text.is_empty() {
                    SecretEdit::Keep
                } else {
                    SecretEdit::Set(SecretString::new(text))
                };
                Ok(KeySource::Pasted { certificate })
            }
        }
    }

    /// 粘贴的私钥附带的证书：要是用户证书，这次粘贴了没有加密的私钥时还要与它配对；返回规范的一行，没填时为空。
    /// 加密的私钥（解密要跑 KDF，不在界面线程上做）、保留已保存的私钥时不对照：连接时对不上会说明。
    fn checked_certificate(&self, key_text: &str, cx: &App) -> Result<String, SharedString> {
        let text = self.pasted_certificate.read(cx).value().trim().to_string();
        if text.is_empty() {
            return Ok(String::new());
        }
        let invalid = |e: &CertError| t!("keys.error.certificate", reason = describe_cert(e));
        let certificate = sshcert::parse(&text).map_err(|e| invalid(&e))?;
        if sshcert::CertInfo::of(&certificate).kind != CertKind::User {
            return Err(t!("keys.error.host_certificate"));
        }
        // 私钥认不出时由私钥的检查说明（见 [`check_private_key`]）
        if !key_text.is_empty()
            && let Ok(key) = sshkey::parse(key_text.as_bytes(), None)
            && !sshcert::matches_public_key(&certificate, &key.public_key())
        {
            return Err(t!("keys.error.certificate_mismatch"));
        }
        certificate
            .to_openssh()
            .map_err(|_| invalid(&CertError::Malformed))
    }

    /// 选私钥文件：填进路径（不读文件，连接时再读，与手填的路径相同）。
    fn browse_file<T: 'static>(
        fields: fn(&mut T) -> &mut KeyFields,
        window: &mut Window,
        cx: &mut Context<T>,
    ) {
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
                tracing::debug!("选了私钥文件");
                fields(this).file.update(cx, |input, cx| {
                    input.set_value(path.display().to_string(), window, cx)
                });
            });
        })
        .detach();
    }

    /// 粘贴的私钥附带的证书从文件读入（`-cert.pub`）：读到的是证书时填进规范的一行，否则填进原文（保存时说明）。
    fn browse_certificate<T: 'static>(
        fields: fn(&mut T) -> &mut KeyFields,
        window: &mut Window,
        cx: &mut Context<T>,
    ) {
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
            let read = {
                let path = path.clone();
                cx.background_spawn(async move { sshcert::read_file(&path) })
                    .await
            };
            let _ = this.update_in(cx, |this, window, cx| match read {
                Ok(text) => {
                    let line = sshcert::parse(&text)
                        .ok()
                        .and_then(|c| c.to_openssh().ok())
                        .unwrap_or_else(|| text.trim().to_string());
                    tracing::debug!("从文件读入粘贴的私钥附带的证书");
                    fields(this)
                        .pasted_certificate
                        .update(cx, |input, cx| input.set_value(line, window, cx));
                }
                // 写在所在的对话框（主机、凭据档案）底部：弹出的通知会被对话框盖住
                Err(e) => crate::notify::show(
                    crate::notify::Notice::error(t!(
                        "keys.error.read_failed",
                        path = path.display().to_string(),
                        reason = e.to_string()
                    )),
                    window,
                    cx,
                ),
            });
        })
        .detach();
    }

    /// 来源的单选与对应的字段。`fields` 从所有者中取出这一组字段（改来源时用），`changed` 在改了之后调用。
    pub fn render<T: 'static>(
        &self,
        id: &'static str,
        store: &Store,
        fields: fn(&mut T) -> &mut KeyFields,
        changed: fn(&mut T, &mut Context<T>),
        cx: &mut Context<T>,
    ) -> AnyElement {
        let from = self.from;
        let radio = RadioGroup::horizontal(id)
            .children([
                t!("host.key.library"),
                t!("host.key.file"),
                t!("host.key.pasted"),
            ])
            .selected_index(FROMS.iter().position(|f| *f == from))
            .on_change(cx.listener(move |this, ix: &usize, _, cx| {
                fields(this).from = FROMS[*ix];
                changed(this, cx);
                cx.notify();
            }));
        let (muted, danger) = (cx.theme().muted_foreground, cx.theme().danger);
        let source = match from {
            KeyFrom::Library if store.list::<KeyEntry>().is_empty() => div()
                .text_sm()
                .text_color(muted)
                .child(prose(t!("host.key.library_empty")))
                .into_any_element(),
            KeyFrom::Library => field()
                .label(t!("host.key.library_key"))
                .child(
                    v_flex()
                        .gap_1()
                        .child(Select::new(&self.library))
                        .when_some(self.library_certificate_note(store, cx), |this, note| {
                            this.child(div().text_xs().text_color(muted).child(prose(note)))
                        }),
                )
                .into_any_element(),
            KeyFrom::File => v_flex()
                .gap_3()
                .child(
                    field().label(t!("host.key.file")).child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().min_w_0().child(
                                Input::new(&self.file).id(SharedString::from(format!("{id}-file"))),
                            ))
                            .child(
                                Button::new(SharedString::from(format!("{id}-file-browse")))
                                    .outline()
                                    .label(t!("keys.browse"))
                                    .on_click(cx.listener(move |_, _, window, cx| {
                                        Self::browse_file(fields, window, cx)
                                    })),
                            ),
                    ),
                )
                .child(
                    field()
                        .label(t!("host.key.certificate"))
                        .child(Input::new(&self.certificate)),
                )
                .into_any_element(),
            KeyFrom::Pasted => v_flex()
                .gap_3()
                .child(
                    field()
                        .label(t!("host.key.pasted_content"))
                        .child(Textarea::new(&self.pasted)),
                )
                .child(
                    field()
                        .label(t!("host.key.pasted_certificate_label"))
                        .child(
                            v_flex()
                                .gap_1()
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(div().flex_1().min_w_0().child(
                                            Input::new(&self.pasted_certificate).id(
                                                SharedString::from(format!("{id}-certificate")),
                                            ),
                                        ))
                                        .child(
                                            Button::new(SharedString::from(format!(
                                                "{id}-certificate-browse"
                                            )))
                                            .outline()
                                            .label(t!("keys.browse"))
                                            .on_click(cx.listener(move |_, _, window, cx| {
                                                Self::browse_certificate(fields, window, cx)
                                            })),
                                        ),
                                )
                                .when_some(
                                    self.pasted_certificate_note(cx),
                                    |this, (note, warn)| {
                                        let color = if warn { danger } else { muted };
                                        this.child(
                                            div().text_xs().text_color(color).child(prose(note)),
                                        )
                                    },
                                ),
                        ),
                )
                .into_any_element(),
        };
        v_flex()
            .gap_3()
            .child(field().label(t!("host.key.source")).child(radio))
            .child(source)
            .when(from != KeyFrom::Library, |this| {
                this.child(
                    field()
                        .label(t!("host.key.passphrase"))
                        .child(Input::new(&self.passphrase).mask_toggle()),
                )
            })
            .into_any_element()
    }

    /// 粘贴的私钥附带证书时的说明与是否是提醒（证书读不出、是主机证书、已过期）；没填时为 `None`。
    fn pasted_certificate_note(&self, cx: &App) -> Option<(SharedString, bool)> {
        let text = self.pasted_certificate.read(cx).value().trim().to_string();
        if text.is_empty() {
            return None;
        }
        let certificate = match sshcert::parse(&text) {
            Ok(certificate) => certificate,
            Err(e) => {
                return Some((
                    t!("keys.error.certificate", reason = describe_cert(&e)),
                    true,
                ));
            }
        };
        let info = sshcert::CertInfo::of(&certificate);
        Some(if info.kind != CertKind::User {
            (t!("keys.error.host_certificate"), true)
        } else if info
            .valid_before
            .is_some_and(|at| at <= sshcert::now_secs())
        {
            (t!("host.key.pasted_certificate_expired"), true)
        } else {
            (t!("host.key.pasted_certificate"), false)
        })
    }

    /// 选中的密钥库密钥附带证书时的说明（过期的提醒替换）。
    fn library_certificate_note(&self, store: &Store, cx: &App) -> Option<SharedString> {
        let key = chosen(&self.library, cx).flatten()?;
        let entry = store.get::<KeyEntry>(&key)?;
        let certificate = sshcert::parse(&entry.doc.certificate).ok()?;
        let expired = sshcert::CertInfo::of(&certificate)
            .valid_before
            .is_some_and(|at| at <= sshcert::now_secs());
        Some(if expired {
            t!("host.key.library_certificate_expired")
        } else {
            t!("host.key.library_certificate")
        })
    }
}

/// 粘贴的私钥在保存时就检查：认不出、不支持、私钥密码不对时说明原因。
pub fn check_private_key(text: &SecretString, passphrase: &SecretEdit) -> Result<(), SharedString> {
    let passphrase = match passphrase {
        SecretEdit::Set(p) => Some(p),
        _ => None,
    };
    match sshkey::parse(text.expose().as_bytes(), passphrase) {
        Ok(_) | Err(KeyError::PassphraseRequired) => Ok(()),
        Err(KeyError::WrongPassphrase) => Err(t!("host.error.key_wrong_passphrase")),
        Err(KeyError::Unsupported(_)) => Err(t!("host.error.key_unsupported")),
        Err(_) => Err(t!("host.error.key_invalid")),
    }
}

/// 密钥库的选项：写作"名称（类型）"，类型去掉 `ssh-` 前缀。
fn key_choices(store: &Store) -> Vec<Choice<Option<KeyId>>> {
    let mut keys = store.list::<KeyEntry>();
    keys.sort_by_key(|k| k.doc.name.to_lowercase());
    keys.into_iter()
        .map(|k| {
            let kind = k
                .doc
                .algorithm
                .strip_prefix("ssh-")
                .unwrap_or(&k.doc.algorithm)
                .to_string();
            Choice::new(
                t!("choice.name_with_detail", name = k.doc.name, detail = kind),
                Some(k.id),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use gpui_kit::{Render, TestAppContext};

    use super::*;
    use crate::app_state::AppState;
    use crate::i18n::{Language, set_language};
    use crate::testkit::open_workspace;

    /// 用户证书（给另一把 ed25519 密钥签发的，永不过期，与 `tests/ui.rs` 的相同）。
    const CERTIFICATE: &str = "ssh-ed25519-cert-v01@openssh.com AAAAIHNzaC1lZDI1NTE5LWNlcnQtdjAxQG9wZW5zc2guY29tAAAAIGQpp1S5wvgGEEY14QlbUPW8DW7Ni7RBI6xfoaCXTXkoAAAAICbe1igcT4sGwlBXnjShpoUBNNsmCXkTQ3M0Wigq9xzyAAAAAAAAAAcAAAABAAAAB3VpLXRlc3QAAAARAAAABmRlcGxveQAAAANvcHMAAAAAAAAAAP//////////AAAAAAAAAGUAAAAXcGVybWl0LWFnZW50LWZvcndhcmRpbmcAAAAAAAAAFnBlcm1pdC1wb3J0LWZvcndhcmRpbmcAAAAAAAAACnBlcm1pdC1wdHkAAAAAAAAADnBlcm1pdC11c2VyLXJjAAAAAAAAAAAAAAAzAAAAC3NzaC1lZDI1NTE5AAAAIONm4WSHmInVaSsRZw7Wwa5oEpzqhjBVuvy+flV8sr7oAAAAUwAAAAtzc2gtZWQyNTUxOQAAAEBqvf4vRXCDpYqTSAF+3xUf+FRfVzNkE9Bq87r/dRSCH7NvBXuQwhEm6Iqou0qdbyGkfpQGqyfUfRpEiBJn3J8N ui@test";

    struct Holder(KeyFields);

    impl Render for Holder {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    // 粘贴的私钥附带证书：填进规范的一行；没填时为空；不是证书、与这次粘贴的私钥对不上时说明；保留已保存的私钥时
    // 不对照；原来附带的证书编辑时照样在
    #[gpui_kit::test]
    fn pasted_key_certificate_is_checked(cx: &mut TestAppContext) {
        let handle = open_workspace(cx);
        set_language(Language::ZhCn);
        let key = sshkey::generate(sshkey::KeyKind::Ed25519, "")
            .unwrap()
            .to_openssh()
            .unwrap();
        let collect = |cx: &mut TestAppContext, saved: bool, key: &str, certificate: &str| {
            let (key, certificate) = (key.to_string(), certificate.to_string());
            cx.update_window(handle, move |_, window, cx| {
                let store = AppState::store(cx);
                let holder = cx.new(|cx| {
                    Holder(KeyFields::new(
                        Some(&KeySource::pasted()),
                        saved,
                        false,
                        &store,
                        window,
                        cx,
                    ))
                });
                holder.update(cx, |holder, cx| {
                    let fields = &holder.0;
                    fields
                        .pasted
                        .update(cx, |input, cx| input.set_value(key, window, cx));
                    fields
                        .pasted_certificate
                        .update(cx, |input, cx| input.set_value(certificate, window, cx));
                    let mut edit = SecretEdit::Keep;
                    let source = fields.collect(&mut edit, cx);
                    (source, matches!(edit, SecretEdit::Set(_)))
                })
            })
            .unwrap()
        };

        let (source, set) = collect(cx, true, "", &format!("  {CERTIFICATE}\n"));
        assert_eq!(
            source,
            Ok(KeySource::Pasted {
                certificate: CERTIFICATE.into()
            }),
            "保留已保存的私钥：不对照"
        );
        assert!(!set);
        assert_eq!(
            collect(cx, false, key.expose(), "").0,
            Ok(KeySource::pasted())
        );
        assert_eq!(
            collect(cx, false, key.expose(), CERTIFICATE).0,
            Err(t!("keys.error.certificate_mismatch"))
        );
        let not_certificate = collect(
            cx,
            true,
            "",
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICbe1igcT4sGwlBXnjShpoUBNNsmCXkTQ3M0Wigq9xzy",
        )
        .0;
        assert_eq!(
            not_certificate,
            Err(t!(
                "keys.error.certificate",
                reason = describe_cert(&CertError::NotCertificate)
            ))
        );

        let prefilled = cx
            .update_window(handle, |_, window, cx| {
                let store = AppState::store(cx);
                let source = KeySource::Pasted {
                    certificate: CERTIFICATE.into(),
                };
                let holder = cx.new(|cx| {
                    Holder(KeyFields::new(
                        Some(&source),
                        true,
                        false,
                        &store,
                        window,
                        cx,
                    ))
                });
                let fields = &holder.read(cx).0;
                (
                    fields.pasted_certificate.read(cx).value().to_string(),
                    fields.pasted_certificate_note(cx),
                )
            })
            .unwrap();
        assert_eq!(
            prefilled,
            (
                CERTIFICATE.to_string(),
                Some((t!("host.key.pasted_certificate"), false))
            )
        );
    }
}
