//! 设置的“安全”页：启用、修改、停用锁定密码（立即执行，修改与停用要当前密码），以及
//! 无操作自动锁定、启动时锁定、系统锁屏时锁定三项（随设置的“保存”写入本机设置 `app_lock`，核心下一次检查即生效）；
//! 主密码模式下另有修改主密码的一节（[`MasterPassword`]）。

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::form::{field, v_form};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::select::Select;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Task, TestSupportExt as _,
    Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::applock::{IDLE_CHOICES, LockError, MAX_IDLE_MINUTES, MIN_PASSWORD_LEN};
use termlet_core::keymap::KeyAction;
use termlet_core::model::LocalSettings;
use termlet_core::secret::SecretString;

use super::master_password::MasterPassword;
use crate::app_state::AppState;
use crate::choice::{Choice, ChoiceState, choice_state, chosen};
use crate::prose::prose;
use crate::t;

/// 正在进行的密码操作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Enable,
    Change,
    Disable,
}

/// 操作的结果（渲染时翻译）。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Message {
    Enabled,
    Changed,
    Disabled,
    Mismatch,
    TooShort,
    WrongPassword,
    NotEnabled,
    Failed(String),
}

impl Message {
    fn is_error(&self) -> bool {
        !matches!(
            self,
            Message::Enabled | Message::Changed | Message::Disabled
        )
    }

    fn text(&self) -> SharedString {
        match self {
            Message::Enabled => t!("lock_ui.security.enabled"),
            Message::Changed => t!("lock_ui.security.changed"),
            Message::Disabled => t!("lock_ui.security.disabled"),
            Message::Mismatch => t!("lock_ui.security.mismatch"),
            Message::TooShort => t!("lock_ui.security.too_short", min = MIN_PASSWORD_LEN),
            Message::WrongPassword => t!("lock_ui.security.wrong_password"),
            Message::NotEnabled => t!("lock_ui.security.not_enabled"),
            Message::Failed(error) => t!("lock_ui.security.failed", error = error),
        }
    }
}

/// 启用、修改密码、停用时的输入框（进入操作时新建，完成或取消后丢弃）。
struct PasswordForm {
    mode: Mode,
    current: Option<Entity<InputState>>,
    new: Option<Entity<InputState>>,
    confirm: Option<Entity<InputState>>,
    _subscriptions: Vec<Subscription>,
}

pub(crate) struct SecurityPage {
    idle_minutes: u32,
    idle: Entity<ChoiceState<u32>>,
    on_startup: bool,
    on_system_lock: bool,
    form: Option<PasswordForm>,
    busy: bool,
    message: Option<Message>,
    /// 修改主密码（只在主密码模式下显示）
    master: Entity<MasterPassword>,
    _task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl SecurityPage {
    pub(crate) fn new(local: &LocalSettings, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = &local.app_lock;
        let idle_minutes = settings.idle_minutes.min(MAX_IDLE_MINUTES);
        let choices = idle_choices(idle_minutes)
            .into_iter()
            .map(|minutes| Choice::new(idle_label(minutes), minutes))
            .collect();
        let idle = choice_state(choices, &idle_minutes, false, window, cx);
        let mut subscriptions = vec![cx.observe(&idle, |this, state, cx| {
            if let Some(minutes) = chosen(&state, cx) {
                this.idle_minutes = minutes;
            }
            cx.notify();
        })];
        // 别处（另一个窗口、锁屏）改了应用锁的状态
        if let Some(model) = super::lock_model(cx) {
            subscriptions.push(cx.observe(&model, |_, _, cx| cx.notify()));
        }
        Self {
            idle_minutes,
            idle,
            on_startup: settings.on_startup,
            on_system_lock: settings.on_system_lock,
            form: None,
            busy: false,
            message: None,
            master: cx.new(|_| MasterPassword::new()),
            _task: None,
            _subscriptions: subscriptions,
        }
    }

    /// 修改主密码的一节（测试用）。
    #[cfg(test)]
    pub(super) fn master(&self) -> &Entity<MasterPassword> {
        &self.master
    }

    /// 把三项设置写进本机设置（随设置对话框的“保存”）。
    pub(crate) fn apply(&self, local: &mut LocalSettings) {
        let settings = &mut local.app_lock;
        settings.idle_minutes = self.idle_minutes;
        settings.on_startup = self.on_startup;
        settings.on_system_lock = self.on_system_lock;
    }

    /// 最近一次操作的结果与是否正在进行（测试用，与界面语言无关）。
    #[cfg(test)]
    pub(super) fn snapshot(&self) -> (Option<String>, bool) {
        (self.message.as_ref().map(|m| format!("{m:?}")), self.busy)
    }

    fn start(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        let current = matches!(mode, Mode::Change | Mode::Disable)
            .then(|| masked_input(t!("lock_ui.security.current_password"), window, cx));
        let (new, confirm) = if mode == Mode::Disable {
            (None, None)
        } else {
            (
                Some(masked_input(
                    t!("lock_ui.security.new_password", min = MIN_PASSWORD_LEN),
                    window,
                    cx,
                )),
                Some(masked_input(
                    t!("lock_ui.security.confirm_password"),
                    window,
                    cx,
                )),
            )
        };
        let subscriptions = [&current, &new, &confirm]
            .into_iter()
            .flatten()
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
        if let Some(first) = current.as_ref().or(new.as_ref()) {
            first.update(cx, |state, cx| state.focus(window, cx));
        }
        self.form = Some(PasswordForm {
            mode,
            current,
            new,
            confirm,
            _subscriptions: subscriptions,
        });
        self.message = None;
        cx.notify();
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
        let Some(lock) = super::app_lock(cx) else {
            return;
        };
        let value = |input: &Option<Entity<InputState>>| {
            input
                .as_ref()
                .map(|state| state.read(cx).value().to_string())
                .unwrap_or_default()
        };
        let mode = form.mode;
        let (current, new) = (value(&form.current), value(&form.new));
        if mode != Mode::Disable
            && let Err(message) = check_new_password(&new, &value(&form.confirm))
        {
            self.message = Some(message);
            cx.notify();
            return;
        }
        let (current, new) = (SecretString::new(current), SecretString::new(new));
        tracing::info!(?mode, "应用锁：提交密码操作");
        self.busy = true;
        self.message = None;
        cx.notify();
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = match mode {
                Mode::Enable => lock.set_password(None, &new).await,
                Mode::Change => lock.set_password(Some(&current), &new).await,
                Mode::Disable => lock.disable(&current).await,
            };
            let _ = this.update_in(cx, |this, window, cx| this.finish(mode, result, window, cx));
        }));
    }

    fn finish(
        &mut self,
        mode: Mode,
        result: Result<(), LockError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.busy = false;
        match &result {
            Ok(()) => tracing::info!(?mode, "应用锁：密码操作完成"),
            Err(e) => tracing::info!(?mode, "应用锁：密码操作没有完成：{e}"),
        }
        self.message = Some(match result {
            Ok(()) => {
                self.form = None;
                match mode {
                    Mode::Enable => Message::Enabled,
                    Mode::Change => Message::Changed,
                    Mode::Disable => Message::Disabled,
                }
            }
            Err(LockError::WrongPassword) => {
                // 清空当前密码并聚焦（set_value 会聚焦）
                if let Some(current) = self.form.as_ref().and_then(|f| f.current.clone()) {
                    current.update(cx, |state, cx| state.set_value("", window, cx));
                }
                Message::WrongPassword
            }
            Err(LockError::TooShort) => Message::TooShort,
            Err(LockError::NotEnabled) => {
                self.form = None;
                Message::NotEnabled
            }
            // 存储的错误按种类说明（核心的错误文字是中文，英文界面中不直接显示）
            Err(LockError::Core(e)) => {
                tracing::warn!("应用锁的操作失败：{e}");
                Message::Failed(crate::store_errors::describe(&e).to_string())
            }
            Err(e) => {
                tracing::warn!("应用锁的操作失败：{e}");
                Message::Failed(e.to_string())
            }
        });
        super::sync_lock_model(cx);
        cx.notify();
    }

    fn render_description(&self, enabled: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = AppState::store(cx).settings();
        let os = crate::keymap::current_os(cx);
        let keymap = termlet_core::keymap::effective(&settings.keybindings, os);
        let description = match keymap.combos(KeyAction::Lock).first() {
            Some(combo) => t!("lock_ui.security.description", key = combo.label(os)),
            None => t!("lock_ui.security.description_no_key"),
        };
        h_flex()
            .gap_3()
            .items_start()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(prose(description)),
            )
            .when(enabled, |this| {
                this.child(
                    Button::new("security-lock-now")
                        .outline()
                        .small()
                        .label(t!("lock_ui.security.lock_now"))
                        .on_click(|_, _, cx| {
                            super::lock_now(cx);
                        }),
                )
            })
    }

    fn render_options(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        v_flex()
            .gap_3()
            .child(
                field()
                    .label(t!("lock_ui.security.idle"))
                    .description(prose(t!("lock_ui.security.idle_hint")))
                    .child(
                        div()
                            .w(rems(12.))
                            .child(Select::new(&self.idle).id("security-idle")),
                    ),
            )
            .child(
                Checkbox::new("security-on-startup")
                    .label(t!("lock_ui.security.on_startup"))
                    .checked(self.on_startup)
                    .on_change(cx.listener(|this, checked: &bool, _, cx| {
                        this.on_startup = *checked;
                        cx.notify();
                    })),
            )
            .child(
                Checkbox::new("security-on-system-lock")
                    .label(t!("lock_ui.security.on_system_lock"))
                    .checked(self.on_system_lock)
                    .on_change(cx.listener(|this, checked: &bool, _, cx| {
                        this.on_system_lock = *checked;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(prose(t!("lock_ui.security.settings_hint"))),
            )
    }

    fn render_password(&self, enabled: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(form) = &self.form else {
            let buttons = if enabled {
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("security-change")
                            .outline()
                            .small()
                            .label(t!("lock_ui.security.change"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.start(Mode::Change, window, cx)
                            })),
                    )
                    .child(
                        Button::new("security-disable")
                            .outline()
                            .small()
                            .label(t!("lock_ui.security.disable"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.start(Mode::Disable, window, cx)
                            })),
                    )
            } else {
                h_flex().child(
                    Button::new("security-enable")
                        .primary()
                        .small()
                        .label(t!("lock_ui.security.enable"))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.start(Mode::Enable, window, cx)),
                        ),
                )
            };
            return v_flex().child(buttons);
        };
        let input = |id: &'static str, state: &Option<Entity<InputState>>| {
            state
                .as_ref()
                .map(|state| Input::new(state).id(id).disabled(self.busy))
        };
        let submit = Button::new("security-submit")
            .small()
            .loading(self.busy)
            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)));
        let submit = match form.mode {
            Mode::Enable => submit.primary().label(t!("lock_ui.security.enable_submit")),
            Mode::Change => submit.primary().label(t!("lock_ui.security.change_submit")),
            Mode::Disable => submit.danger().label(t!("lock_ui.security.disable_submit")),
        };
        v_flex()
            .w(rems(20.))
            .gap_2()
            // 在设置对话框中，输入框里的 Enter 先成为对话框的 Confirm：在这里提交密码并拦下，
            // 不交给对话框（会保存并关闭设置，看不到结果）。不在对话框中时由 PressEnter 提交
            .on_action(
                cx.listener(|this, _: &gpui_kit::base::actions::Confirm, window, cx| {
                    this.submit(window, cx)
                }),
            )
            .children(input("security-current", &form.current))
            .children(input("security-new", &form.new))
            .children(input("security-confirm", &form.confirm))
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("security-cancel")
                            .ghost()
                            .small()
                            .label(t!("common.cancel"))
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                    )
                    .child(submit),
            )
    }
}

impl Render for SecurityPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (danger, success, border) = (theme.danger, theme.success, theme.border);
        let available = super::app_lock(cx).is_some();
        let enabled = super::is_enabled(cx);
        // 主密码模式下才有主密码可改（主密钥在系统钥匙串中时没有）
        let master = AppState::store(cx)
            .vault_uses_password()
            .then(|| field().child(self.master.clone()));
        let mut form = v_form().child(field().child(self.render_description(enabled, cx)));
        if !available {
            return form
                .child(
                    field().child(
                        div()
                            .id("security-unavailable")
                            .test_support()
                            .text_sm()
                            .text_color(danger)
                            .child(prose(t!("lock_ui.security.unavailable"))),
                    ),
                )
                .children(master);
        }
        if enabled {
            form = form.child(field().child(self.render_options(cx)));
        }
        form.child(
            field().child(
                v_flex()
                    .gap_2()
                    .pt_3()
                    .border_t_1()
                    .border_color(border)
                    .child(self.render_password(enabled, cx))
                    .when_some(self.message.clone(), |this, message| {
                        this.child(
                            div()
                                .id("security-message")
                                .test_support()
                                .text_xs()
                                .text_color(if message.is_error() { danger } else { success })
                                .child(message.text()),
                        )
                    }),
            ),
        )
        .children(master)
    }
}

fn masked_input(
    placeholder: SharedString,
    window: &mut Window,
    cx: &mut App,
) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .masked(true)
            .placeholder(placeholder)
    })
}

/// 新密码的检查（核心另外检查长度）：两次输入要相同、至少 [`MIN_PASSWORD_LEN`] 个字符。
fn check_new_password(new: &str, confirm: &str) -> Result<(), Message> {
    if new != confirm {
        return Err(Message::Mismatch);
    }
    if new.chars().count() < MIN_PASSWORD_LEN {
        return Err(Message::TooShort);
    }
    Ok(())
}

/// “无操作自动锁定”的选项：固定的几项，当前值（例如从备份恢复来的 10 分钟）不在其中时也列出，按大小排列。
fn idle_choices(current: u32) -> Vec<u32> {
    let mut choices = IDLE_CHOICES.to_vec();
    if !choices.contains(&current) {
        choices.push(current);
        choices.sort_unstable();
    }
    choices
}

fn idle_label(minutes: u32) -> SharedString {
    if minutes == 0 {
        t!("lock_ui.security.idle_never")
    } else {
        t!("lock_ui.security.idle_minutes", count = minutes)
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn new_password_must_match_and_be_long_enough() {
        assert_eq!(check_new_password("abc", "abd"), Err(Message::Mismatch));
        assert_eq!(check_new_password("abc", "abc"), Err(Message::TooShort));
        // 按字符计：6 个汉字够长
        assert_eq!(check_new_password("锁定密码六位", "锁定密码六位"), Ok(()));
        assert_eq!(check_new_password("secret", "secret"), Ok(()));
    }

    #[test]
    fn idle_choices_keep_an_unlisted_current_value() {
        assert_eq!(idle_choices(15), vec![0, 5, 15, 30, 60]);
        assert_eq!(idle_choices(10), vec![0, 5, 10, 15, 30, 60]);
        assert_eq!(idle_choices(120), vec![0, 5, 15, 30, 60, 120]);
    }

    #[test]
    fn idle_labels() {
        let original = crate::i18n::language();
        crate::i18n::set_language(crate::i18n::Language::ZhCn);
        assert_eq!(idle_label(0), "从不");
        assert_eq!(idle_label(5), "5 分钟");
        crate::i18n::set_language(crate::i18n::Language::En);
        assert_eq!(idle_label(30), "30 min");
        crate::i18n::set_language(original);
    }
}
