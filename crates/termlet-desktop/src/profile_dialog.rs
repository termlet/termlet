//! 凭据档案：多台主机共用的一份用户名与认证（密码、私钥、ssh-agent）。主机的认证选"凭据档案"即可引用，
//! 分组也可以设为默认值；改一处对所有使用它的主机生效。仍被使用的档案不能删除。

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::{AlertDialog, DialogAction, DialogClose, DialogFooter};
use gpui_kit::component::form::{field, v_form};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::radio::RadioGroup;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, Entity, Focusable as _, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Task, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::id::ProfileId;
use termlet_core::model::{KeySource, Profile, ProfileAuth, SecretField};
use termlet_core::ssh::{SecretEdit, SecretEdits};
use termlet_core::store::{Owner, Position, Store};

use crate::app_state::AppState;
use crate::dialog_token::{DialogToken, close_own};
use crate::key_fields::{self, KeyFields};
use crate::notify::{self, NoticeLine};
use crate::prose::prose;
use crate::{store_errors, t};

#[derive(Clone, Copy, PartialEq, Eq)]
enum AuthKind {
    Password,
    Key,
    Agent,
}

struct ProfileForm {
    id: Option<ProfileId>,
    store: Arc<Store>,
    name: Entity<InputState>,
    username: Entity<InputState>,
    auth: AuthKind,
    password: Entity<InputState>,
    key: KeyFields,
    saved_password: bool,
    saved_private_key: bool,
    saved_passphrase: bool,
    error: Option<SharedString>,
    /// 正在后台检查粘贴的私钥，检查完再保存
    saving: bool,
    /// 所在的对话框（后台检查完私钥、保存后只关它）
    dialog: Option<DialogToken>,
    /// 读入证书文件失败等立即执行的操作的说明（见 [`crate::notify::show`]）
    notice: Entity<NoticeLine>,
    _save: Option<Task<()>>,
}

pub fn open_new(window: &mut Window, cx: &mut App) {
    open(None, window, cx);
}

pub fn open_edit(id: ProfileId, window: &mut Window, cx: &mut App) {
    open(Some(id), window, cx);
}

fn open(id: Option<ProfileId>, window: &mut Window, cx: &mut App) {
    let editing = id.is_some();
    let form = cx.new(|cx| ProfileForm::new(id, window, cx));
    let first_focus = Rc::new(Cell::new(true));
    let width = rems(32.).to_pixels(window.rem_size());
    let owner = form.clone();
    window.open_dialog(cx, move |dialog, window, cx| {
        if first_focus.replace(false) {
            form.read(cx).name.focus_handle(cx).focus(window, cx);
        }
        let save = {
            let form = form.clone();
            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                form.update(cx, |form, cx| form.save(window, cx))
            }
        };
        dialog
            .title(if editing {
                t!("profile.edit.title")
            } else {
                t!("profile.new.title")
            })
            .w(width)
            .overlay_closable(false)
            .child(form.clone())
            .on_ok(save)
            .footer(
                DialogFooter::new()
                    .child(
                        DialogClose::new()
                            .child(Button::new("profile-cancel").label(t!("common.cancel"))),
                    )
                    .child(
                        DialogAction::new().child(
                            Button::new("profile-save")
                                .primary()
                                .label(t!("common.save"))
                                .loading(form.read(cx).saving),
                        ),
                    ),
            )
    });
    let token = DialogToken::take(window, cx);
    let notice = owner.update(cx, |form, _| {
        form.dialog = token;
        form.notice.clone()
    });
    notify::attach(&notice, window, cx);
}

impl ProfileForm {
    fn new(id: Option<ProfileId>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = AppState::store(cx);
        let profile = id
            .as_ref()
            .and_then(|id| store.get::<Profile>(id))
            .map(|r| r.doc)
            .unwrap_or_default();
        let owner = id.as_ref().map(Owner::profile);
        let has = |field: SecretField| owner.as_ref().is_some_and(|o| store.has_secret(o, field));
        let (saved_password, saved_private_key, saved_passphrase) = (
            has(SecretField::Password),
            has(SecretField::PrivateKey),
            has(SecretField::Passphrase),
        );
        let (auth, source) = match &profile.auth {
            ProfileAuth::Password => (AuthKind::Password, None),
            ProfileAuth::Key { source } => (AuthKind::Key, Some(source.clone())),
            ProfileAuth::Agent => (AuthKind::Agent, None),
        };
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("profile.field.name_placeholder"))
                .default_value(profile.name.clone())
        });
        let username =
            cx.new(|cx| InputState::new(window, cx).default_value(profile.username.clone()));
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(if saved_password {
                    t!("host.secret.keep")
                } else {
                    t!("host.field.password")
                })
        });
        let key = KeyFields::new(
            source.as_ref(),
            saved_private_key,
            saved_passphrase,
            &store,
            window,
            cx,
        );
        Self {
            id,
            store,
            name,
            username,
            auth,
            password,
            key,
            saved_password,
            saved_private_key,
            saved_passphrase,
            error: None,
            saving: false,
            dialog: None,
            notice: NoticeLine::new(cx),
            _save: None,
        }
    }

    fn collect(&self, cx: &App) -> Result<(Profile, SecretEdits), SharedString> {
        let value = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        // 密码原样保存（首尾的空格也是密码的一部分，与连接时的询问框一致）
        let secret = |input: &Entity<InputState>| input.read(cx).value().to_string();
        let username = value(&self.username);
        if username.is_empty() {
            return Err(t!("profile.error.username_required"));
        }
        let clear_if = |saved: bool| {
            if saved {
                SecretEdit::Clear
            } else {
                SecretEdit::Keep
            }
        };
        let set_or_keep = |v: String| {
            if v.is_empty() {
                SecretEdit::Keep
            } else {
                SecretEdit::Set(termlet_core::secret::SecretString::new(v))
            }
        };
        let mut secrets = SecretEdits::default();
        let auth = match self.auth {
            AuthKind::Password => {
                secrets.password = set_or_keep(secret(&self.password));
                secrets.private_key = clear_if(self.saved_private_key);
                secrets.passphrase = clear_if(self.saved_passphrase);
                ProfileAuth::Password
            }
            AuthKind::Agent => {
                secrets.password = clear_if(self.saved_password);
                secrets.private_key = clear_if(self.saved_private_key);
                secrets.passphrase = clear_if(self.saved_passphrase);
                ProfileAuth::Agent
            }
            AuthKind::Key => {
                secrets.password = clear_if(self.saved_password);
                let source = self.key.collect(&mut secrets.private_key, cx)?;
                if !matches!(source, KeySource::Pasted { .. }) {
                    secrets.private_key = clear_if(self.saved_private_key);
                }
                secrets.passphrase = if matches!(source, KeySource::Library { .. }) {
                    clear_if(self.saved_passphrase)
                } else {
                    set_or_keep(secret(&self.key.passphrase))
                };
                ProfileAuth::Key { source }
            }
        };
        let mut profile = self
            .id
            .as_ref()
            .and_then(|id| self.store.get::<Profile>(id))
            .map(|r| r.doc)
            .unwrap_or_default();
        profile.name = value(&self.name);
        profile.username = username;
        profile.auth = auth;
        Ok((profile, secrets))
    }

    /// 保存；成功时返回 true（关闭对话框）。有新粘贴的私钥时先在后台检查（解密要跑 KDF），返回 false，
    /// 检查通过后保存并关闭。
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.saving {
            return false;
        }
        let (profile, secrets) = match self.collect(cx) {
            Ok(collected) => collected,
            Err(message) => {
                self.error = Some(message);
                cx.notify();
                return false;
            }
        };
        let check = match (&profile.auth, &secrets.private_key) {
            (
                ProfileAuth::Key {
                    source: KeySource::Pasted { .. },
                },
                SecretEdit::Set(text),
            ) => Some((text.clone(), secrets.passphrase.clone())),
            _ => None,
        };
        let Some((text, passphrase)) = check else {
            return self.write(profile, secrets, cx);
        };
        self.saving = true;
        self.error = None;
        cx.notify();
        let job =
            cx.background_spawn(
                async move { key_fields::check_private_key(&text, &passphrase).err() },
            );
        self._save = Some(cx.spawn_in(window, async move |this, cx| {
            let failed = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.saving = false;
                if let Some(message) = failed {
                    this.error = Some(message);
                    cx.notify();
                } else if this.write(profile, secrets, cx) {
                    this.saving = true;
                    close_own(this.dialog.as_ref(), window, cx);
                }
            });
        }));
        false
    }

    /// 写进存储；成功时返回 true。
    fn write(&mut self, profile: Profile, secrets: SecretEdits, cx: &mut Context<Self>) -> bool {
        let creating = self.id.is_none();
        let name = profile.name.clone();
        let result = match &self.id {
            None => self
                .store
                .create(profile, None, Position::Last)
                .and_then(|id| {
                    // 先记下 ID：秘密写入失败后再点保存时改为更新，不会再建一份
                    self.id = Some(id.clone());
                    secrets.apply(&self.store, &Owner::profile(&id))
                }),
            Some(id) => self
                .store
                .update(id, profile)
                .and_then(|()| secrets.apply(&self.store, &Owner::profile(id))),
        };
        match result {
            Ok(()) => {
                tracing::info!(profile = ?self.id, name = %name, creating, "已保存凭据档案");
                true
            }
            Err(e) => {
                tracing::warn!(profile = ?self.id, creating, "保存凭据档案失败：{e}");
                self.error = Some(store_errors::describe(&e));
                cx.notify();
                false
            }
        }
    }
}

impl Render for ProfileForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let kinds = [AuthKind::Password, AuthKind::Key, AuthKind::Agent];
        let radio = RadioGroup::horizontal("profile-auth")
            .children([
                t!("host.auth.password"),
                t!("host.auth.key"),
                t!("host.auth.agent"),
            ])
            .selected_index(kinds.iter().position(|k| *k == self.auth))
            .on_change(cx.listener(move |this, ix: &usize, _, cx| {
                this.auth = kinds[*ix];
                cx.notify();
            }));
        let store = self.store.clone();
        let auth_fields = match self.auth {
            // 与私钥的字段一样标签在上方
            AuthKind::Password => field()
                .label(t!("host.field.password"))
                .child(Input::new(&self.password).mask_toggle())
                .into_any_element(),
            AuthKind::Key => self.key.render(
                "profile-key-from",
                &store,
                |this: &mut Self| &mut this.key,
                |_, _| {},
                cx,
            ),
            AuthKind::Agent => div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(prose(t!("host.auth.agent_hint")))
                .into_any_element(),
        };
        v_flex()
            .gap_3()
            .child(
                v_form()
                    .child(
                        field()
                            .label(t!("profile.field.name"))
                            .required(true)
                            .child(Input::new(&self.name).id("profile-name")),
                    )
                    .child(
                        field()
                            .label(t!("host.field.username"))
                            .required(true)
                            .child(Input::new(&self.username).id("profile-username")),
                    )
                    .child(field().label(t!("host.field.auth")).child(radio))
                    .child(field().child(auth_fields)),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(prose(error)),
                )
            })
            .when(self.notice.read(cx).is_shown(), |this| {
                this.child(div().text_sm().child(self.notice.clone()))
            })
    }
}

/// 确认后删除凭据档案；仍被主机或分组使用时拒绝并说明。
pub fn confirm_delete(id: ProfileId, window: &mut Window, cx: &mut App) {
    let store = AppState::store(cx);
    let Some(profile) = store.get::<Profile>(&id) else {
        return;
    };
    let name = profile.doc.name;
    crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
        let store = store.clone();
        let id = id.clone();
        alert
            .confirm()
            .title(t!("profile.delete.title", name = name.clone()))
            .ok_text(t!("common.delete"))
            .ok_variant(ButtonVariant::Danger)
            .cancel_text(t!("common.cancel"))
            .on_ok(move |_, window, cx| {
                tracing::info!(id = %id, "删除凭据档案");
                if let Err(e) = store.delete::<Profile>(&id) {
                    crate::notify::error(store_errors::describe(&e), window, cx);
                }
                true
            })
    });
}
