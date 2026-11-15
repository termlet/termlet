//! 锁屏：应用锁定时代替工作区窗口的全部内容（含标题栏，窗口仍可拖动、最小化、关闭）。
//!
//! - 主会话在锁定时让 `Workspace::render` 整个返回锁屏，不要把它放进工作区的元素树：否则工作区的动作与快捷键
//!   会经由焦点路径照样生效。终端视图不渲染，输入进不了会话；会话、传输与转发在后台继续。
//! - 对话框、通知、提示框由窗口的根画在内容之后，锁屏用 `deferred` 画在它们之上（优先级在对话框之上、弹出菜单
//!   之下：锁屏自己的输入框菜单照常显示，背景中的菜单在焦点移到锁屏时关闭）。焦点移到密码框，Tab 只在锁屏内
//!   循环，Esc 关不掉；对话框原样还在，解锁后 [`LockScreen::restore_focus`] 把焦点还给锁定前的元素。
//! - 密码框 Enter 解锁；输错时清空并提示（返回前核心已经等过输错后的等待，期间按钮显示为忙）。Windows Hello 可用时
//!   显示“使用 Windows Hello 解锁”；用户取消不提示。

use std::future::Future;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, FocusTrapElement as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement as _, Render, SharedString, Styled as _, Subscription, Task,
    TestSupportExt as _, WeakFocusHandle, Window, deferred, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::applock::{LockError, Verification};
use termlet_core::secret::SecretString;

use crate::prose::prose;
use crate::t;

/// 锁屏的绘制优先级：对话框为 10 起、弹出菜单 100、悬停提示 200。
const LOCK_PRIORITY: usize = 90;

/// 解锁失败的原因（渲染时翻译）。
#[derive(Clone, Debug, PartialEq)]
enum UnlockError {
    WrongPassword,
    SystemFailed,
    Failed(String),
}

/// 锁屏（每个锁定的窗口一个，锁定时新建、解锁后丢弃）。
pub(crate) struct LockScreen {
    focus: FocusHandle,
    password: Entity<InputState>,
    error: Option<UnlockError>,
    /// 正在解锁（验证密码、等系统验证）
    busy: bool,
    /// Windows Hello 可用（检查完之前为 false）
    system: bool,
    /// 点了“退出 Termlet…”，等确认
    confirm_quit: bool,
    /// 锁定前获得焦点的元素
    previous_focus: Option<WeakFocusHandle>,
    _check: Task<()>,
    _unlock: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl LockScreen {
    /// 新建锁屏并把焦点移到密码框（在窗口锁定时调用，记下原来的焦点）。
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let previous_focus = window.focused(cx).map(|handle| handle.downgrade());
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t!("lock_ui.screen.password"))
        });
        password.update(cx, |state, cx| state.focus(window, cx));
        let subscriptions = vec![cx.subscribe_in(
            &password,
            window,
            |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.unlock(window, cx),
                // “解锁”按钮随密码框是否为空启用
                InputEvent::Change => cx.notify(),
                _ => {}
            },
        )];
        let check = match super::app_lock(cx) {
            Some(lock) => cx.spawn(async move |this, cx| {
                let available = lock.system_available().await;
                let _ = this.update(cx, |this, cx| {
                    this.system = available;
                    cx.notify();
                });
            }),
            None => Task::ready(()),
        };
        Self {
            focus: cx.focus_handle(),
            password,
            error: None,
            busy: false,
            system: false,
            confirm_quit: false,
            previous_focus,
            _check: check,
            _unlock: None,
            _subscriptions: subscriptions,
        }
    }

    /// 解锁后把焦点还给锁定前的元素（还在时）。主会话在去掉锁屏、重新显示内容时调用。
    pub(crate) fn restore_focus(&self, window: &mut Window, cx: &mut App) {
        if let Some(handle) = self
            .previous_focus
            .as_ref()
            .and_then(WeakFocusHandle::upgrade)
        {
            window.focus(&handle, cx);
        }
    }

    /// 显示退出的确认（锁定时从托盘菜单退出），与点“退出 Termlet…”相同。
    pub(crate) fn ask_quit(&mut self, cx: &mut Context<Self>) {
        self.confirm_quit = true;
        cx.notify();
    }

    /// 密码框中的内容与是否正在解锁（测试用）。
    #[cfg(test)]
    pub(super) fn snapshot(&self, cx: &App) -> (SharedString, bool) {
        (self.password.read(cx).value(), self.busy)
    }

    /// 点了空白处（鼠标按下的冒泡阶段，子元素之后）：输入框、按钮已经接过焦点时不管；否则不让焦点陷阱的容器
    /// 拿走焦点（容器跟踪焦点，默认会在点击时聚焦），焦点不在锁屏里时放回密码框。
    fn keep_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.default_prevented() {
            return;
        }
        window.prevent_default();
        if !self.focus.contains_focused(window, cx) {
            self.password
                .update(cx, |state, cx| state.focus(window, cx));
        }
    }

    fn unlock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let value = self.password.read(cx).value();
        if value.is_empty() {
            return;
        }
        let Some(lock) = super::app_lock(cx) else {
            return;
        };
        let password = SecretString::new(value.to_string());
        tracing::debug!("锁屏：用锁定密码解锁");
        self.run(async move { lock.unlock(&password).await }, window, cx);
    }

    fn unlock_with_system(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(lock) = super::app_lock(cx) else {
            return;
        };
        let hwnd = super::window_hwnd(window);
        tracing::debug!("锁屏：用系统验证（Windows Hello）解锁");
        let message = t!("lock_ui.screen.system_reason").to_string();
        self.run(
            async move { lock.unlock_with_system(hwnd, &message).await },
            window,
            cx,
        );
    }

    fn run(
        &mut self,
        job: impl Future<Output = Result<(), LockError>> + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.busy = true;
        self.error = None;
        self.confirm_quit = false;
        cx.notify();
        self._unlock = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| this.finish(result, window, cx));
        }));
    }

    fn finish(
        &mut self,
        result: Result<(), LockError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.busy = false;
        // 只记结果，不记输入的密码
        match &result {
            Ok(()) => tracing::info!("锁屏：解锁成功"),
            Err(e) => tracing::info!("锁屏：没有解锁：{e}"),
        }
        self.error = match result {
            // 解锁了，或别处停用了应用锁：主会话随锁定状态去掉锁屏
            Ok(()) | Err(LockError::NotEnabled) => None,
            Err(LockError::System(Verification::Canceled)) => None,
            Err(LockError::System(_)) => Some(UnlockError::SystemFailed),
            Err(LockError::WrongPassword) => Some(UnlockError::WrongPassword),
            Err(e) => {
                tracing::warn!("解锁失败：{e}");
                Some(UnlockError::Failed(e.to_string()))
            }
        };
        match self.error {
            Some(UnlockError::WrongPassword | UnlockError::Failed(_)) => {
                // 清空并把焦点放回密码框（set_value 会聚焦）
                self.password
                    .update(cx, |state, cx| state.set_value("", window, cx));
            }
            _ => self
                .password
                .update(cx, |state, cx| state.focus(window, cx)),
        }
        cx.notify();
        super::sync_lock_model(cx);
    }

    fn error_text(&self) -> Option<SharedString> {
        Some(match self.error.as_ref()? {
            UnlockError::WrongPassword => t!("lock_ui.screen.wrong_password"),
            UnlockError::SystemFailed => t!("lock_ui.screen.system_failed"),
            UnlockError::Failed(error) => t!("lock_ui.screen.failed", error = error),
        })
    }

    fn render_quit(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        if !self.confirm_quit {
            return v_flex().child(
                Button::new("lock-screen-quit")
                    .ghost()
                    .label(t!("lock_ui.screen.quit"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.confirm_quit = true;
                        cx.notify();
                    })),
            );
        }
        v_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(prose(t!("lock_ui.screen.quit_confirm"))),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("lock-screen-quit-cancel")
                            .ghost()
                            .label(t!("common.cancel"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm_quit = false;
                                this.password
                                    .update(cx, |state, cx| state.focus(window, cx));
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("lock-screen-quit-ok")
                            .danger()
                            .label(t!("lock_ui.screen.quit_ok"))
                            .on_click(|_, _, cx| cx.quit()),
                    ),
            )
    }
}

impl Render for LockScreen {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger, background, foreground) = (
            theme.muted_foreground,
            theme.danger,
            theme.background,
            theme.foreground,
        );
        let empty = self.password.read(cx).value().is_empty();
        let form = v_flex()
            .w(rems(18.))
            .gap_4()
            .items_center()
            .text_center()
            .child(crate::brand::logo(rems(4.)))
            .child(
                div()
                    .text_lg()
                    .font_semibold()
                    .child(t!("lock_ui.screen.title")),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(t!("lock_ui.screen.hint")),
            )
            .child(
                div().w_full().child(
                    Input::new(&self.password)
                        .id("lock-screen-password")
                        .disabled(self.busy),
                ),
            )
            .when_some(self.error_text(), |this, error| {
                this.child(
                    div()
                        .id("lock-screen-error")
                        .test_support()
                        .text_xs()
                        .text_color(danger)
                        .child(prose(error)),
                )
            })
            .child(
                Button::new("lock-screen-unlock")
                    .primary()
                    .w_full()
                    .label(t!("lock_ui.screen.unlock"))
                    .loading(self.busy)
                    .disabled(empty)
                    .on_click(cx.listener(|this, _, window, cx| this.unlock(window, cx))),
            )
            .when(self.system, |this| {
                this.child(
                    Button::new("lock-screen-system")
                        .outline()
                        .w_full()
                        .icon(IconName::FingerprintPattern)
                        .label(t!("lock_ui.screen.windows_hello"))
                        .disabled(self.busy)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.unlock_with_system(window, cx)),
                        ),
                )
            })
            .child(self.render_quit(cx));
        div().size_full().child(
            deferred(
                // 焦点陷阱用自己的元素 ID，锁屏的 ID 放在外面一层
                div().id("lock-screen").test_support().size_full().child(
                    v_flex()
                        .size_full()
                        .occlude()
                        // 点在锁屏的空白处（下面有对话框时也一样）：焦点留在锁屏里
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| this.keep_focus(window, cx)),
                        )
                        .bg(background)
                        .text_color(foreground)
                        .child(crate::ui_scale::title_bar())
                        .child(
                            v_flex()
                                .flex_1()
                                .min_h_0()
                                .items_center()
                                .justify_center()
                                .child(form),
                        )
                        .focus_trap("lock-screen-trap", &self.focus),
                ),
            )
            .with_priority(LOCK_PRIORITY),
        )
    }
}
