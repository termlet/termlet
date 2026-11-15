//! 保险库关卡：保险库没有解锁时代替工作区的内容显示，处理完才能继续使用。
//!
//! Windows 上主密钥在凭据管理器中，几乎不会出现；出现的情形是主密码模式（钥匙串不可用）、
//! 数据目录来自另一台电脑或另一个账户（主密钥不在 / 不符）、读取钥匙串出错，以及便携版第一次使用时
//! 选择用主密码还是这台电脑的凭据管理器。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Window, div, prelude::FluentBuilder as _,
};
use termlet_core::CoreError;
use termlet_core::secret::SecretString;
use termlet_core::vault::{MIN_PASSWORD_LEN, VaultStatus};

use crate::app_state::AppState;
use crate::prose::prose;
use crate::t;

/// 提交失败的原因（渲染时翻译）。
#[derive(Clone, Debug, PartialEq)]
enum GateError {
    WrongPassword,
    Mismatch,
    TooShort,
    Failed(String),
}

pub struct VaultGate {
    password: Entity<InputState>,
    confirm: Entity<InputState>,
    error: Option<GateError>,
    busy: bool,
    _subscriptions: Vec<Subscription>,
}

impl VaultGate {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t!("vault.password_placeholder"))
        });
        let confirm = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t!("vault.confirm_placeholder"))
        });
        let submit_on_enter = |this: &mut Self,
                               _: &Entity<InputState>,
                               event: &InputEvent,
                               window: &mut Window,
                               cx: &mut Context<Self>| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.submit(window, cx);
            }
        };
        let subscriptions = vec![
            cx.subscribe_in(&password, window, submit_on_enter),
            cx.subscribe_in(&confirm, window, submit_on_enter),
        ];
        Self {
            password,
            confirm,
            error: None,
            busy: false,
            _subscriptions: subscriptions,
        }
    }

    fn status(cx: &gpui_kit::App) -> VaultStatus {
        AppState::store(cx).vault_status()
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let status = Self::status(cx);
        let password = SecretString::new(self.password.read(cx).value().to_string());
        let job: Box<dyn FnOnce(&termlet_core::store::Store) -> termlet_core::Result<()> + Send> =
            match status {
                VaultStatus::NeedsPassword => Box::new(move |store| store.unlock_vault(&password)),
                VaultStatus::NeedsNewPassword { .. } | VaultStatus::ChooseKeyStore => {
                    if password.expose().chars().count() < MIN_PASSWORD_LEN {
                        self.set_error(Some(GateError::TooShort), cx);
                        return;
                    }
                    if self.confirm.read(cx).value().as_ref() != password.expose() {
                        self.set_error(Some(GateError::Mismatch), cx);
                        return;
                    }
                    Box::new(move |store| store.set_vault_password(&password))
                }
                VaultStatus::KeyringError(_) => Box::new(|store| {
                    store.vault().retry(store.database()).map(|_| ())?;
                    Ok(())
                }),
                _ => return,
            };
        self.run(job, window, cx);
    }

    /// 在后台执行（Argon2 派生要几百毫秒），完成后更新错误提示。
    fn run(
        &mut self,
        job: Box<dyn FnOnce(&termlet_core::store::Store) -> termlet_core::Result<()> + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.busy = true;
        self.error = None;
        cx.notify();
        let store = AppState::store(cx);
        let task = cx.background_spawn(async move { job(&store) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                this.error = match result {
                    Ok(()) => None,
                    Err(CoreError::WrongPassword) => Some(GateError::WrongPassword),
                    Err(e) => Some(GateError::Failed(e.to_string())),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// 便携版第一次使用时选择只在这台电脑上用：主密钥放进凭据管理器（钥匙串不可用时转为设置主密码）。
    fn use_keyring(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || Self::status(cx) != VaultStatus::ChooseKeyStore {
            return;
        }
        self.run(Box::new(|store| store.choose_vault_keyring()), window, cx);
    }

    fn set_error(&mut self, error: Option<GateError>, cx: &mut Context<Self>) {
        self.error = error;
        cx.notify();
    }

    fn confirm_reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let gate = cx.entity().downgrade();
        crate::ui_scale::open_alert(window, cx, move |alert, _, _| {
            let gate = gate.clone();
            alert
                .title(t!("vault.reset_confirm.title"))
                .description(prose(t!("vault.reset_confirm.description")))
                .confirm()
                .ok_text(t!("vault.reset_confirm.ok"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, window, cx| {
                    if let Some(gate) = gate.upgrade() {
                        gate.update(cx, |gate, cx| {
                            gate.run(Box::new(|store| store.reset_vault(None)), window, cx)
                        });
                    }
                    true
                })
        });
    }

    fn render_error(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let text: SharedString = match self.error.as_ref()? {
            GateError::WrongPassword => t!("vault.error.wrong_password"),
            GateError::Mismatch => t!("vault.error.mismatch"),
            GateError::TooShort => t!("vault.error.too_short", min = MIN_PASSWORD_LEN),
            GateError::Failed(error) => t!("vault.error.failed", error = error),
        };
        Some(
            div()
                .text_sm()
                .text_color(cx.theme().danger)
                .child(prose(text)),
        )
    }
}

impl Render for VaultGate {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = Self::status(cx);
        let portable = AppState::global(cx).data_dir.is_portable();
        let (title, description) = match &status {
            VaultStatus::Unlocked => (SharedString::default(), SharedString::default()),
            // 便携版是自己选的主密码（钥匙串可能可用），不说“没有可用的钥匙串”
            VaultStatus::NeedsPassword if portable => (
                t!("vault.locked.title"),
                t!("vault.locked.description_portable"),
            ),
            VaultStatus::NeedsPassword => {
                (t!("vault.locked.title"), t!("vault.locked.description"))
            }
            VaultStatus::NeedsNewPassword { reason } => (
                t!("vault.new_password.title"),
                t!("vault.new_password.description", reason = reason),
            ),
            VaultStatus::ChooseKeyStore => {
                (t!("vault.portable.title"), t!("vault.portable.description"))
            }
            VaultStatus::KeyMissing => (
                t!("vault.key_missing.title"),
                t!("vault.key_missing.description"),
            ),
            VaultStatus::KeyMismatch => (
                t!("vault.key_mismatch.title"),
                t!("vault.key_mismatch.description"),
            ),
            VaultStatus::KeyringError(reason) => (
                t!("vault.keyring_error.title"),
                t!("vault.keyring_error.description", reason = reason),
            ),
        };
        let needs_password = matches!(
            status,
            VaultStatus::NeedsPassword
                | VaultStatus::NeedsNewPassword { .. }
                | VaultStatus::ChooseKeyStore
        );
        let needs_confirm = matches!(
            status,
            VaultStatus::NeedsNewPassword { .. } | VaultStatus::ChooseKeyStore
        );
        let choose = status == VaultStatus::ChooseKeyStore;
        let can_reset = matches!(
            status,
            VaultStatus::KeyMissing | VaultStatus::KeyMismatch | VaultStatus::KeyringError(_)
        );
        let primary_label = match status {
            VaultStatus::NeedsPassword => Some(t!("vault.unlock")),
            VaultStatus::NeedsNewPassword { .. } | VaultStatus::ChooseKeyStore => {
                Some(t!("vault.set_password"))
            }
            VaultStatus::KeyringError(_) => Some(t!("vault.retry")),
            _ => None,
        };

        // 铺满工作区的内容区再上下左右居中：外层包着它的是普通的块（不是 flex 容器），只写 flex_1 撑不开高度，
        // 内容会贴在顶上
        v_flex().size_full().items_center().justify_center().child(
            v_flex()
                .w_96()
                .gap_4()
                .child(
                    h_flex()
                        .gap_2()
                        .child(Icon::new(IconName::TriangleAlert).text_color(cx.theme().warning))
                        .child(div().text_lg().child(title)),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(prose(description)),
                )
                .when(needs_password, |this| {
                    this.child(
                        Input::new(&self.password)
                            .id("vault-password")
                            .mask_toggle()
                            .disabled(self.busy),
                    )
                })
                .when(needs_confirm, |this| {
                    this.child(
                        Input::new(&self.confirm)
                            .id("vault-confirm")
                            .disabled(self.busy),
                    )
                })
                .children(self.render_error(cx))
                .child(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            Button::new("vault-quit")
                                .ghost()
                                .label(t!("vault.quit"))
                                .on_click(|_, _, cx| cx.quit()),
                        )
                        .when(choose, |this| {
                            this.child(
                                Button::new("vault-use-keyring")
                                    .outline()
                                    .label(t!("vault.portable.this_computer"))
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.use_keyring(window, cx)
                                    })),
                            )
                        })
                        .when(can_reset, |this| {
                            this.child(
                                Button::new("vault-reset")
                                    .danger()
                                    .label(t!("vault.reset"))
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.confirm_reset(window, cx)
                                    })),
                            )
                        })
                        .when_some(primary_label, |this, label| {
                            this.child(
                                Button::new("vault-submit")
                                    .primary()
                                    .label(label)
                                    .loading(self.busy)
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.submit(window, cx)),
                                    ),
                            )
                        }),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AnyWindowHandle, AppContext as _, TestAppContext};
    use termlet_core::vault::VaultStatus;

    use crate::app_state::AppState;
    use crate::i18n::{Language, set_language};
    use crate::testkit::{open_portable_workspace, wait_until};

    /// 打开像便携版那样的主窗口（保险库还没有选保管方式）。
    fn open_portable(cx: &mut TestAppContext) -> AnyWindowHandle {
        set_language(Language::ZhCn);
        let handle = open_portable_workspace(cx);
        assert_eq!(
            cx.update(|cx| AppState::store(cx).vault_status()),
            VaultStatus::ChooseKeyStore
        );
        handle
    }

    // 便携版第一次打开先选保管方式；“只在这台电脑上使用”：主密钥放进凭据管理器，不设主密码，进入主界面
    #[gpui_kit::test]
    fn portable_first_run_can_use_this_computer(cx: &mut TestAppContext) {
        let handle = open_portable(cx);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("vault-password").is_some());
            assert!(window.try_find("vault-confirm").is_some());
            window.click("vault-use-keyring", cx);
        })
        .unwrap();
        wait_until(cx, handle, "保险库解锁", |window, cx| {
            AppState::store(cx).vault_status() == VaultStatus::Unlocked
                && window.try_find("vault-use-keyring").is_none()
        });
        assert!(!cx.update(|cx| AppState::store(cx).vault_uses_password()));
    }

    // 选主密码：两次输入一致才设置（在别的电脑上也能用主密码解开）
    #[gpui_kit::test]
    fn portable_first_run_can_set_a_master_password(cx: &mut TestAppContext) {
        let handle = open_portable(cx);
        let password = "correct horse battery";
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("vault-password", cx);
            window.input(password, cx);
            window.click("vault-confirm", cx);
            window.input(password, cx);
            window.click("vault-submit", cx);
        })
        .unwrap();
        wait_until(cx, handle, "保险库解锁", |_, cx| {
            AppState::store(cx).vault_status() == VaultStatus::Unlocked
        });
        assert!(cx.update(|cx| AppState::store(cx).vault_uses_password()));
    }
}
