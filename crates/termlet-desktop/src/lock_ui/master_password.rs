//! 设置“安全”页的“主密码”一节：修改主密码。只在主密码模式下显示（系统钥匙串不可用时，保存的密码、私钥等由
//! 主密码保护、每次启动时输入）；主密钥在系统钥匙串中时没有主密码，不显示。
//!
//! 主密钥不变，保存的秘密不用重新加密（[`termlet_core::store::Store::change_vault_password`]，几百毫秒的密钥派生在
//! 后台执行）。只影响这台电脑：云同步用的是同步密码，其他电脑有各自的保险库（钥匙串或它们自己的主密码），不用跟着改；
//! 备份、快照不带主密码，恢复时也不会把主密码改回去。

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Subscription, Task, TestSupportExt as _, Window, div,
    prelude::FluentBuilder as _, rems,
};
use termlet_core::CoreError;
use termlet_core::secret::SecretString;
use termlet_core::vault::MIN_PASSWORD_LEN;

use crate::app_state::AppState;
use crate::prose::prose;
use crate::t;

/// 操作的结果（渲染时翻译）。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Message {
    Changed,
    Mismatch,
    TooShort,
    WrongPassword,
    Failed(String),
}

impl Message {
    fn is_error(&self) -> bool {
        *self != Message::Changed
    }

    fn text(&self) -> SharedString {
        match self {
            Message::Changed => t!("lock_ui.master.changed"),
            Message::Mismatch => t!("lock_ui.security.mismatch"),
            Message::TooShort => t!("lock_ui.master.too_short", min = MIN_PASSWORD_LEN),
            Message::WrongPassword => t!("lock_ui.master.wrong_password"),
            Message::Failed(error) => t!("lock_ui.security.failed", error = error),
        }
    }
}

/// 修改主密码时的三个输入框（开始修改时新建，完成或取消后丢弃）。
struct Form {
    current: Entity<InputState>,
    new: Entity<InputState>,
    confirm: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

pub(crate) struct MasterPassword {
    form: Option<Form>,
    busy: bool,
    message: Option<Message>,
    _task: Option<Task<()>>,
}

impl MasterPassword {
    pub(crate) fn new() -> Self {
        Self {
            form: None,
            busy: false,
            message: None,
            _task: None,
        }
    }

    /// 最近一次操作的结果与是否正在进行（测试用，与界面语言无关）。
    #[cfg(test)]
    pub(super) fn snapshot(&self) -> (Option<String>, bool) {
        (self.message.as_ref().map(|m| format!("{m:?}")), self.busy)
    }

    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = |placeholder: SharedString, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder(placeholder)
            })
        };
        let current = input(t!("lock_ui.master.current"), window, cx);
        let new = input(t!("lock_ui.master.new", min = MIN_PASSWORD_LEN), window, cx);
        let confirm = input(t!("lock_ui.master.confirm"), window, cx);
        let subscriptions = [&current, &new, &confirm]
            .into_iter()
            .map(|input| {
                cx.subscribe_in(
                    input,
                    window,
                    |this: &mut Self, _, event: &InputEvent, window, cx| {
                        if matches!(event, InputEvent::PressEnter { .. }) {
                            this.submit(window, cx);
                        }
                    },
                )
            })
            .collect();
        current.update(cx, |state, cx| state.focus(window, cx));
        self.form = Some(Form {
            current,
            new,
            confirm,
            _subscriptions: subscriptions,
        });
        self.message = None;
        cx.notify();
    }

    /// 修改主密码正在进行。
    pub(crate) fn is_busy(&self) -> bool {
        self.busy
    }

    /// 锁定时清空输入到一半的密码（正在修改时不动）。
    pub(crate) fn clear_secrets(&mut self, cx: &mut Context<Self>) {
        if !self.busy && self.form.take().is_some() {
            self.message = None;
            cx.notify();
        }
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.form = None;
        self.message = None;
        cx.notify();
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(form) = &self.form else {
            return;
        };
        let value = |input: &Entity<InputState>| input.read(cx).value().to_string();
        let (current, new) = (value(&form.current), value(&form.new));
        if let Err(message) = check_new_password(&new, &value(&form.confirm)) {
            self.message = Some(message);
            cx.notify();
            return;
        }
        let (current, new) = (SecretString::new(current), SecretString::new(new));
        tracing::info!("修改主密码：提交");
        self.busy = true;
        self.message = None;
        cx.notify();
        let store = AppState::store(cx);
        let job = cx.background_spawn(async move { store.change_vault_password(&current, &new) });
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| this.finish(result, window, cx));
        }));
    }

    fn finish(
        &mut self,
        result: termlet_core::Result<()>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.busy = false;
        self.message = Some(match result {
            Ok(()) => {
                self.form = None;
                Message::Changed
            }
            Err(CoreError::WrongPassword) => {
                // 清空当前密码，焦点移过去重新输入
                if let Some(form) = &self.form {
                    let current = form.current.clone();
                    current.update(cx, |state, cx| {
                        state.set_value("", window, cx);
                        state.focus(window, cx);
                    });
                }
                Message::WrongPassword
            }
            Err(CoreError::Invalid(_)) => Message::TooShort,
            Err(e) => {
                tracing::warn!("修改主密码失败：{e}");
                Message::Failed(crate::store_errors::describe(&e).to_string())
            }
        });
        cx.notify();
    }

    fn render_form(&self, form: &Form, cx: &mut Context<Self>) -> impl IntoElement {
        let input = |id: &'static str, state: &Entity<InputState>| {
            Input::new(state).id(id).disabled(self.busy)
        };
        v_flex()
            .w(rems(20.))
            .gap_2()
            // 设置中输入框里的 Enter 转成 Confirm（与对话框中相同）：在这里提交并拦下
            .on_action(
                cx.listener(|this, _: &gpui_kit::base::actions::Confirm, window, cx| {
                    this.submit(window, cx)
                }),
            )
            .child(input("master-current", &form.current))
            .child(input("master-new", &form.new))
            .child(input("master-confirm", &form.confirm))
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("master-cancel")
                            .ghost()
                            .small()
                            .label(t!("common.cancel"))
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                    )
                    .child(
                        Button::new("master-submit")
                            .primary()
                            .small()
                            .loading(self.busy)
                            .label(t!("lock_ui.master.submit"))
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}

impl Render for MasterPassword {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (danger, success, muted, border) = (
            theme.danger,
            theme.success,
            theme.muted_foreground,
            theme.border,
        );
        let body = match &self.form {
            Some(form) => self.render_form(form, cx).into_any_element(),
            None => h_flex()
                .child(
                    Button::new("master-change")
                        .outline()
                        .small()
                        .label(t!("lock_ui.master.change"))
                        .on_click(cx.listener(|this, _, window, cx| this.start(window, cx))),
                )
                .into_any_element(),
        };
        v_flex()
            .id("master-password")
            .test_support()
            .gap_2()
            .pt_3()
            .border_t_1()
            .border_color(border)
            .child(
                div()
                    .text_sm()
                    .font_medium()
                    .child(t!("lock_ui.master.title")),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(prose(t!("lock_ui.master.description"))),
            )
            .child(body)
            .when_some(self.message.clone(), |this, message| {
                this.child(
                    div()
                        .id("master-message")
                        .test_support()
                        .text_xs()
                        .text_color(if message.is_error() { danger } else { success })
                        .child(message.text()),
                )
            })
    }
}

/// 新主密码的检查（核心另外检查长度）：两次输入要相同、至少 [`MIN_PASSWORD_LEN`] 个字符。
fn check_new_password(new: &str, confirm: &str) -> Result<(), Message> {
    if new != confirm {
        return Err(Message::Mismatch);
    }
    if new.chars().count() < MIN_PASSWORD_LEN {
        return Err(Message::TooShort);
    }
    Ok(())
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn new_master_password_must_match_and_be_long_enough() {
        assert_eq!(
            check_new_password("abcdefgh", "abcdefgx"),
            Err(Message::Mismatch)
        );
        assert_eq!(check_new_password("short", "short"), Err(Message::TooShort));
        // 按字符计：8 个汉字够长
        assert_eq!(
            check_new_password("八个汉字的主密码", "八个汉字的主密码"),
            Ok(())
        );
        assert_eq!(check_new_password("long enough", "long enough"), Ok(()));
    }
}
