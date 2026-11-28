//! 导出私钥：私钥离开加密的密钥库以文件保存，先说明风险并要求确认；可以设私钥密码（两次输入一致）。
//! 选好保存位置后导出为 OpenSSH 格式（只有当前用户可以读写），已有同名文件时先确认覆盖。

use std::path::PathBuf;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Task, WeakEntity, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::id::KeyId;
use termlet_core::keyvault::{self, ExportRequest, KeyVaultError};
use termlet_core::model::KeyEntry;
use termlet_core::secret::SecretString;

use super::errors;
use crate::app_state::AppState;
use crate::notify::{self, Notice};
use crate::prose::{ProseLabel as _, prose};
use crate::t;
use crate::ui_scale::DialogTitleExt as _;

pub fn open(id: KeyId, window: &mut Window, cx: &mut App) {
    let Some(record) = AppState::store(cx).get::<KeyEntry>(&id) else {
        return;
    };
    let title = t!("keys.export_title", name = record.doc.name.clone());
    let form = cx.new(|cx| ExportForm::new(id, record.doc, window, cx));
    let width = rems(30.).to_pixels(window.rem_size());
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .titled(title.clone())
            .w(width)
            .overlay_closable(false)
            .child(form.clone())
    });
}

/// 把公钥（authorized_keys 格式的一行）导出到文件：系统的保存对话框，默认文件名是 `<密钥名>.pub`；文件已存在时
/// 先确认再覆盖（与导出私钥相同）。公钥不是秘密，不用像导出私钥那样先确认风险。
pub fn export_public(id: KeyId, window: &mut Window, cx: &mut App) {
    let Some(record) = AppState::store(cx).get::<KeyEntry>(&id) else {
        return;
    };
    let file_name = public_file_name(&record.doc.name, &record.doc.algorithm);
    let picked = cx.prompt_for_new_path(&dirs_home(), Some(&file_name));
    window
        .spawn(cx, async move |cx| {
            let Ok(Ok(Some(path))) = picked.await else {
                return;
            };
            let _ = cx.update(|window, cx| save_public(id, path, false, window, cx));
        })
        .detach();
}

/// 写出公钥；文件已存在（`overwrite` 为假时）先确认。完成后提示写到了哪里（[`crate::notify::show`]：从查看公钥的
/// 对话框导出时写在对话框底部）。
fn save_public(id: KeyId, path: PathBuf, overwrite: bool, window: &mut Window, cx: &mut App) {
    tracing::info!(key = %id, path = %path.display(), overwrite, "导出公钥");
    let job = termlet_core::runtime::spawn(keyvault::export_public_key(
        AppState::store(cx),
        id.clone(),
        path,
        overwrite,
    ));
    window
        .spawn(cx, async move |cx| {
            let result = job.await;
            let _ = cx.update(|window, cx| match result {
                Ok(Ok(path)) => notify::show(
                    Notice::success(t!(
                        "keys.public_exported",
                        path = path.display().to_string()
                    )),
                    window,
                    cx,
                ),
                Ok(Err(KeyVaultError::FileExists { path })) => {
                    let shown = path.display().to_string();
                    crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
                        let (id, path) = (id.clone(), path.clone());
                        alert
                            .confirm()
                            .title(t!("keys.export_overwrite", path = shown.clone()))
                            .ok_text(t!("common.ok"))
                            .ok_variant(ButtonVariant::Danger)
                            .cancel_text(t!("common.cancel"))
                            .on_ok(move |_, window, cx| {
                                save_public(id.clone(), path.clone(), true, window, cx);
                                true
                            })
                    });
                }
                Ok(Err(error)) => notify::show(Notice::error(errors::describe(&error)), window, cx),
                Err(e) => notify::show(Notice::error(e.to_string()), window, cx),
            });
        })
        .detach();
}

/// 公钥文件的默认名称：`<密钥名>.pub`；名称中文件名不能用的字符换成 `_`，名称为空时用 `id_` 加类型。
fn public_file_name(name: &str, algorithm: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches(['.', ' ']);
    let stem = if cleaned.is_empty() {
        default_file_name(algorithm)
    } else {
        cleaned.to_string()
    };
    format!("{stem}.pub")
}

/// 默认的文件名：`id_` 加类型（ECDSA 统一为 `id_ecdsa`）。
fn default_file_name(algorithm: &str) -> String {
    let kind = algorithm.strip_prefix("ssh-").unwrap_or(algorithm);
    if kind.starts_with("ecdsa") {
        "id_ecdsa".into()
    } else {
        format!("id_{kind}")
    }
}

struct ExportForm {
    id: KeyId,
    key: KeyEntry,
    passphrase: Entity<InputState>,
    repeat: Entity<InputState>,
    acknowledged: bool,
    busy: bool,
    error: Option<SharedString>,
    /// 导出成功后的路径
    exported: Option<PathBuf>,
    _task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ExportForm {
    fn new(id: KeyId, key: KeyEntry, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let passphrase = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t!("keys.export_passphrase"))
        });
        let repeat = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t!("keys.export_repeat"))
        });
        let subscriptions = [&passphrase, &repeat]
            .into_iter()
            .map(|state| {
                cx.subscribe(state, |_: &mut Self, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                })
            })
            .collect();
        Self {
            id,
            key,
            passphrase,
            repeat,
            acknowledged: false,
            busy: false,
            error: None,
            exported: None,
            _task: None,
            _subscriptions: subscriptions,
        }
    }

    /// 两次输入的密码：都填了而不一致时为 `Err(true)`（显示提示），只填了一次为 `Err(false)`。
    fn passphrase(&self, cx: &App) -> Result<Option<SecretString>, bool> {
        let first = self.passphrase.read(cx).value().to_string();
        let second = self.repeat.read(cx).value().to_string();
        if first.is_empty() && second.is_empty() {
            return Ok(None);
        }
        if first == second {
            return Ok(Some(SecretString::new(first)));
        }
        Err(!first.is_empty() && !second.is_empty())
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(passphrase) = self.passphrase(cx) else {
            return;
        };
        if !self.acknowledged || self.busy {
            return;
        }
        let dir = dirs_home();
        let picked = cx.prompt_for_new_path(&dir, Some(&default_file_name(&self.key.algorithm)));
        self.busy = true;
        self.error = None;
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let path = match picked.await {
                Ok(Ok(Some(path))) => path,
                _ => {
                    let _ = this.update(cx, |this, cx| {
                        this.busy = false;
                        cx.notify();
                    });
                    return;
                }
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.export(path, passphrase, false, window, cx)
            });
        }));
        cx.notify();
    }

    fn export(
        &mut self,
        path: PathBuf,
        passphrase: Option<SecretString>,
        overwrite: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = AppState::store(cx);
        let request = ExportRequest {
            path: path.clone(),
            passphrase: passphrase.clone(),
            overwrite,
        };
        tracing::info!(key = %self.id, path = %path.display(), overwrite, "导出私钥");
        let job = termlet_core::runtime::spawn(keyvault::export_private_key(
            store,
            self.id.clone(),
            request,
        ));
        self.busy = true;
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(Ok(path)) => this.exported = Some(path),
                    Ok(Err(KeyVaultError::FileExists { path })) => {
                        this.confirm_overwrite(path, passphrase, window, cx)
                    }
                    Ok(Err(error)) => {
                        tracing::warn!("导出私钥失败：{error}");
                        this.error = Some(errors::describe(&error))
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn confirm_overwrite(
        &mut self,
        path: PathBuf,
        passphrase: Option<SecretString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let form: WeakEntity<Self> = cx.entity().downgrade();
        let shown = path.display().to_string();
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let (form, path, passphrase) = (form.clone(), path.clone(), passphrase.clone());
            alert
                .confirm()
                .title(t!("keys.export_overwrite", path = shown.clone()))
                .ok_text(t!("common.ok"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, window, cx| {
                    let _ = form.update(cx, |form, cx| {
                        form.export(path.clone(), passphrase.clone(), true, window, cx)
                    });
                    true
                })
        });
    }
}

/// 保存对话框默认打开的目录：用户的 `.ssh` 目录（存在时），否则主目录。
fn dirs_home() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default();
    let ssh = home.join(".ssh");
    if ssh.is_dir() { ssh } else { home }
}

impl Render for ExportForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, warning, danger, success) = (
            theme.muted_foreground,
            theme.warning,
            theme.danger,
            theme.success,
        );
        if let Some(path) = self.exported.clone() {
            let shown = path.display().to_string();
            return v_flex()
                .gap_3()
                .child(
                    div()
                        .text_sm()
                        .text_color(success)
                        .child(t!("keys.exported", path = shown)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(prose(t!("keys.exported_hint"))),
                )
                .child(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("key-export-reveal")
                                .outline()
                                .label(t!("sftp.show_in_folder"))
                                .on_click(move |_, _, cx| cx.reveal_path(&path)),
                        )
                        .child(
                            Button::new("key-export-close")
                                .primary()
                                .label(t!("common.close"))
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        ),
                )
                .into_any_element();
        }
        let mismatch = matches!(self.passphrase(cx), Err(true));
        let ready = self.acknowledged && self.passphrase(cx).is_ok() && !self.busy;
        let has_first = !self.passphrase.read(cx).value().is_empty();
        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .items_start()
                    .gap_2()
                    .p_2()
                    .rounded(theme.radius)
                    .bg(warning.opacity(0.12))
                    .text_sm()
                    .text_color(warning)
                    .child(Icon::new(IconName::TriangleAlert).small().flex_none())
                    .child(prose(t!("keys.export_warning"))),
            )
            .child(
                Input::new(&self.passphrase)
                    .id("key-export-passphrase")
                    .mask_toggle(),
            )
            .when(has_first, |this| {
                this.child(
                    Input::new(&self.repeat)
                        .id("key-export-repeat")
                        .mask_toggle(),
                )
            })
            .when(mismatch, |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(danger)
                        .child(t!("keys.export_mismatch")),
                )
            })
            .child(
                Checkbox::new("key-export-ack")
                    .prose_label(t!("keys.export_acknowledge"))
                    .checked(self.acknowledged)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.acknowledged = *checked;
                        cx.notify();
                    })),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(div().text_sm().text_color(danger).child(prose(error)))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("key-export-cancel")
                            .ghost()
                            .label(t!("common.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("key-export-submit")
                            .primary()
                            .label(t!("keys.export_submit"))
                            .loading(self.busy)
                            .disabled(!ready)
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 公钥文件的默认名称：密钥名加 .pub，文件名不能用的字符换掉，名称为空时用类型
    #[test]
    fn public_key_file_names() {
        assert_eq!(public_file_name("部署", "ssh-ed25519"), "部署.pub");
        assert_eq!(
            public_file_name(" web/prod: key? ", "ssh-rsa"),
            "web_prod_ key_.pub"
        );
        assert_eq!(
            public_file_name("a\\b*c\"d<e>f|g", "ssh-rsa"),
            "a_b_c_d_e_f_g.pub"
        );
        assert_eq!(public_file_name("...", "ssh-ed25519"), "id_ed25519.pub");
        assert_eq!(public_file_name("", "ecdsa-sha2-nistp256"), "id_ecdsa.pub");
    }

    // 从查看公钥的对话框导出：写到了哪里写在对话框底部，不弹出会被对话框盖住的通知
    #[gpui_kit::test]
    fn exported_public_key_is_reported_inside_the_dialog(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        use termlet_core::keyvault::GenerateRequest;
        use termlet_core::sshkey::KeyKind;

        use crate::notify::shown;

        let handle = crate::testkit::open_workspace(cx);
        crate::i18n::set_language(crate::i18n::Language::ZhCn);
        let store = cx.update(|cx| AppState::store(cx));
        let id = termlet_core::runtime::runtime()
            .block_on(keyvault::generate(
                store,
                GenerateRequest {
                    name: "deploy".into(),
                    kind: KeyKind::Ed25519,
                    comment: String::new(),
                },
            ))
            .unwrap();
        let dir = std::env::temp_dir().join(format!(
            "termlet-export-public-{}",
            termlet_core::id::random_token(8)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("deploy.pub");
        let toasts = cx
            .update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                super::super::view::open(id.clone(), window, cx);
                window.render_frame(cx);
                let toasts = window.notifications(cx).len();
                save_public(id.clone(), path.clone(), false, window, cx);
                toasts
            })
            .unwrap();
        crate::testkit::poll_until(cx, "导出完成", |cx| {
            cx.update_window(handle, |_, window, cx| shown(window, cx).is_some())
                .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let notice = shown(window, cx).unwrap();
            assert_eq!(notice.level, crate::notify::Level::Success);
            assert!(notice.text.contains("deploy.pub"), "{}", notice.text);
            assert_eq!(window.notifications(cx).len(), toasts, "不弹出通知");
            assert!(window.try_find("dialog-notice").is_some());
        })
        .unwrap();
        assert!(path.is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
