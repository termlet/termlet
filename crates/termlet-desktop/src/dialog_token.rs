//! 只关闭自己打开的那个对话框。
//!
//! gpui-component 的 `window.close_dialog` 关的是最上面的对话框。异步完成（超时、后台任务结束）后再关闭时，
//! 上面可能已经叠了别的对话框（密码框、确认框……），直接关闭会关错。打开后立即用 [`DialogToken::take`] 记下它，
//! 之后用 [`DialogToken::close`] 关闭：只有它仍在最上面时才关，返回是否关了（没关时由调用方决定怎么办，例如
//! 把对话框的内容改成“已结束”，等用户自己关）。
//!
//! ```ignore
//! window.open_dialog(cx, build);
//! let token = DialogToken::take(window, cx);
//! // ……之后（同一个窗口中）
//! if let Some(token) = &token {
//!     token.close(window, cx);
//! }
//! ```
//!
//! 依据：打开对话框时焦点立即移到它的焦点句柄上（对话框里的输入框也在它下面）；上面再叠对话框时焦点移走，
//! 上面的关闭后焦点回到它；它自己关闭后焦点离开。

use std::time::Duration;

use gpui_kit::component::WindowExt as _;
use gpui_kit::{App, FocusHandle, Window};

/// 上面叠了别的对话框时，每隔多久看一次它是否回到了最上面，最多等多少次（约 10 分钟，之后由用户自己关）。
const WAIT_TICK: Duration = Duration::from_millis(200);
const WAIT_TICKS: u32 = 3000;

/// 打开的一个对话框。
#[derive(Clone)]
pub struct DialogToken(FocusHandle);

impl DialogToken {
    /// 在 `open_dialog` / `open_alert_dialog` 之后立即调用（同一次更新中，焦点还在新对话框上）。
    pub fn take(window: &Window, cx: &App) -> Option<Self> {
        window.focused(cx).map(Self)
    }

    /// 这个对话框还开着并且在最上面。
    pub fn is_top(&self, window: &mut Window, cx: &mut App) -> bool {
        window.has_active_dialog(cx) && self.contains_focus(window, cx)
    }

    /// 焦点在这个对话框里（它自己或其中的输入框等）。
    pub fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.0.contains_focused(window, cx)
    }

    /// 仍在最上面时关闭它，返回是否关闭了。
    pub fn close(&self, window: &mut Window, cx: &mut App) -> bool {
        if !self.is_top(window, cx) {
            return false;
        }
        window.close_dialog(cx);
        true
    }

    /// 关闭它：在最上面时立即关闭；上面叠了别的对话框时，等它回到最上面再关（它已经被关掉、一直等不到时作罢）。
    pub fn close_when_top(self, window: &mut Window, cx: &mut App) {
        if self.close(window, cx) {
            return;
        }
        window
            .spawn(cx, async move |cx| {
                for _ in 0..WAIT_TICKS {
                    cx.background_executor().timer(WAIT_TICK).await;
                    // 关上了，或窗口已经没了
                    if cx
                        .update(|window, cx| self.close(window, cx))
                        .unwrap_or(true)
                    {
                        return;
                    }
                }
            })
            .detach();
    }
}

/// 后台任务完成后关闭自己打开的对话框：记下了令牌时只关它（见 [`DialogToken::close_when_top`]）；
/// 没有令牌（打开时取不到焦点）时照旧关最上面的。
pub fn close_own(token: Option<&DialogToken>, window: &mut Window, cx: &mut App) {
    match token {
        Some(token) => token.clone().close_when_top(window, cx),
        None => window.close_dialog(cx),
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, Context, IntoElement, Render, TestAppContext, div};

    use super::*;

    struct Empty;

    impl Render for Empty {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    #[gpui_kit::test]
    fn closes_only_while_on_top(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, _) = cx.update(|cx| {
            gpui_kit::open_window(Default::default(), cx, |_, cx| cx.new(|_| Empty)).unwrap()
        });
        let first = cx
            .update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.open_dialog(cx, |dialog, _, _| dialog.title("first"));
                DialogToken::take(window, cx).unwrap()
            })
            .unwrap();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(first.is_top(window, cx));
            window.open_dialog(cx, |dialog, _, _| dialog.title("second"));
            window.render_frame(cx);
            assert!(!first.close(window, cx), "上面叠了别的对话框时不关");
            assert!(window.has_active_dialog(cx));
            window.close_dialog(cx);
            window.render_frame(cx);
            assert!(first.is_top(window, cx), "上面的关闭后回到最上面");
            assert!(first.close(window, cx));
            window.render_frame(cx);
            assert!(!window.has_active_dialog(cx));
            assert!(!first.close(window, cx), "已经关了");
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn closes_after_the_dialog_above_is_closed(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (handle, _) = cx.update(|cx| {
            gpui_kit::open_window(Default::default(), cx, |_, cx| cx.new(|_| Empty)).unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.open_dialog(cx, |dialog, _, _| dialog.title("first"));
            let first = DialogToken::take(window, cx).unwrap();
            window.render_frame(cx);
            window.open_dialog(cx, |dialog, _, _| dialog.title("second"));
            window.render_frame(cx);
            // 后台任务完成：上面还有一个，先不关
            first.close_when_top(window, cx);
            assert!(window.has_active_dialog(cx));
            // 用户关掉上面的那个
            window.close_dialog(cx);
            window.render_frame(cx);
        })
        .unwrap();
        cx.executor().advance_clock(WAIT_TICK * 2);
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            assert!(!window.has_active_dialog(cx), "回到最上面后关闭");
        })
        .unwrap();
    }
}
