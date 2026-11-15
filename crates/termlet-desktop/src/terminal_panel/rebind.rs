//! 终端的 Tab 移到别的窗口（见 `workspace/move_tab.rs`）：面板实体本身移过去，会话与连接、终端引擎（画面与回滚）、
//! 正在运行的程序、会话日志与录制、主机工具的读数都不动。
//!
//! gpui-pre 的 `subscribe_in`、`observe_in` 与 `spawn_in` 任务中的 `update_in` 按实体“当前所在的窗口”（最近一次画出它的
//! 窗口）执行，在新窗口画出后自动跟过去；真正绑在登记时的窗口上的是焦点的监听（`on_focus`、`on_blur`）与窗口的观察
//! （激活、可见性），以及输入框：这里在新窗口中重新登记、重建。连接时的询问（密码、主机指纹）改在新窗口中弹出。
//!
//! 连接中（第一次连接、断线后的重连、强制重连）不能移：进行中的连接可能正停在原来窗口的询问上，连上时视图也可能建在
//! 原来的窗口里。

use gpui_kit::{Context, FocusHandle, Focusable as _, SharedString, Subscription, Window};
use termlet_core::ssh::{self, Prompter};

use super::{State, TerminalPanel};
use crate::t;
use crate::terminal_search::OpenSearch;

impl TerminalPanel {
    /// 现在不能移到别的窗口的原因（正在连接）；能移时为 `None`。
    pub(crate) fn move_blocker(&self) -> Option<SharedString> {
        let waiting = matches!(self.state, State::Waiting { .. });
        let connecting = matches!(self.state, State::Connecting | State::Reconnecting { .. })
            || (self._connect.is_some() && !waiting);
        connecting.then(|| t!("tab_window.blocked_connecting"))
    }

    /// 已从原来窗口的停靠区取下、放进 `window` 之前调用：焦点的监听在新窗口中重新登记，查找栏重建（沿用查找条件），
    /// 之后的连接在新窗口中询问（`prompter` 是新窗口的询问入口）。
    pub(crate) fn rebind_window(
        &mut self,
        prompter: Prompter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 在原来的窗口中输入过、记住的密码随原来的询问入口一起放下：之后重连时再问一次（保存了密码的不受影响）
        self.prompter = ssh::remembering(prompter);
        self._focus_subscription = Some(Self::forward_focus(&self.focus, window, cx));
        if let Some(view) = self.view.clone() {
            view.update(cx, |view, cx| view.rebind_window(window, cx));
        }
        if self.search.take().is_some() {
            self._search_subscription = None;
            self.open_search(&OpenSearch, window, cx);
        }
        cx.notify();
    }

    /// 面板自己拿到焦点时转交给终端视图。
    pub(super) fn forward_focus(
        focus: &FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.on_focus(focus, window, |this: &mut Self, window, cx| {
            if let Some(view) = &this.view {
                view.focus_handle(cx).focus(window, cx);
            }
        })
    }

    /// 屏幕与回滚中 `text` 出现的次数（测试核对移到别的窗口后回滚还在）。
    #[cfg(test)]
    pub(crate) fn count_in_scrollback(&self, text: &str, cx: &mut Context<Self>) -> usize {
        let Some(view) = self.view.clone() else {
            return 0;
        };
        let query = termlet_terminal::SearchQuery {
            text: text.into(),
            case_sensitive: true,
            regex: false,
            whole_word: false,
        };
        let total = view
            .update(cx, |view, cx| view.set_search(Some(query), cx))
            .map(|summary| summary.total)
            .unwrap_or(0);
        view.update(cx, |view, cx| {
            let _ = view.set_search(None, cx);
        });
        total
    }
}
