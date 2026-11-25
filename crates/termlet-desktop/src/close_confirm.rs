//! 关闭终端（本地终端、WSL 也算）、文件管理的 Tab 或窗口前确认（设置“关闭终端、文件管理的 Tab 或窗口前确认”），
//! 一次关闭几个 Tab（“关闭其他 Tab”“关闭所有 Tab”）、退出 Termlet（macOS 菜单的“退出 Termlet”、⌘Q）前汇总确认一次。
//! 确认框中可以勾选“以后不再询问”，勾选后关掉这项设置（未完成的传输、未保存的文件照样确认）。

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Window, div, rems,
};

use crate::app_state::AppState;
use crate::prose::{ProseLabel as _, prose};
use crate::t;

type OnConfirm = Rc<dyn Fn(&mut Window, &mut App)>;

/// 关闭的范围：确认框的说明按它措辞。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseScope {
    /// 关闭这个窗口
    Window,
    /// 一次关闭几个 Tab
    Tabs,
    /// 退出 Termlet（所有窗口）
    Quit,
}

/// 关闭窗口（或几个 Tab、退出）前要说明的：未完成的传输、未保存的文件、连接着的终端或文件管理（按设置）、打开着的本地终端、
/// 还在执行的批量执行。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CloseSummary {
    pub pending: usize,
    pub unsaved: usize,
    pub connected: bool,
    pub local: bool,
    /// 还在执行的批量执行（关闭即停止）
    pub running: usize,
}

impl CloseSummary {
    /// 有要说明的，关闭前先确认。
    pub fn needs_confirm(&self) -> bool {
        self.pending > 0 || self.unsaved > 0 || self.connected || self.local || self.running > 0
    }

    /// 两个窗口合在一起（退出时汇总所有窗口）。
    pub fn merge(self, other: Self) -> Self {
        Self {
            pending: self.pending + other.pending,
            unsaved: self.unsaved + other.unsaved,
            connected: self.connected || other.connected,
            local: self.local || other.local,
            running: self.running + other.running,
        }
    }

    /// 确认框中的说明，按关闭的范围措辞。
    pub fn description(&self, scope: CloseScope) -> SharedString {
        let quit = scope == CloseScope::Quit;
        let mut parts: Vec<SharedString> = Vec::new();
        if self.connected {
            parts.push(match scope {
                CloseScope::Window => t!("close.window_description"),
                CloseScope::Tabs => t!("close.tabs_description"),
                CloseScope::Quit => t!("close.quit_description"),
            });
        }
        if self.local {
            parts.push(match scope {
                CloseScope::Window => t!("close.window_description_local"),
                CloseScope::Tabs => t!("close.tabs_description_local"),
                CloseScope::Quit => t!("close.quit_description_local"),
            });
        }
        if self.pending > 0 {
            parts.push(if quit {
                t!("close.quit_transfers", count = self.pending)
            } else {
                t!("sftp.close_transfers", count = self.pending)
            });
        }
        if self.unsaved > 0 {
            parts.push(if quit {
                t!("close.quit_unsaved", count = self.unsaved)
            } else {
                t!("editor.close_window_unsaved", count = self.unsaved)
            });
        }
        if self.running > 0 {
            parts.push(t!("close.batch_running", count = self.running));
        }
        parts.join(&t!("close.sentence_separator")).into()
    }
}

/// 确认关闭 Tab。
pub fn confirm_tab(
    name: SharedString,
    on_confirm: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    open(
        t!("close.tab_title", name = name),
        t!("close.tab_description"),
        t!("close.confirm"),
        Rc::new(on_confirm),
        window,
        cx,
    );
}

/// 确认关闭 Tab，说明换成 `description`（例如还有未完成的传输）。
pub fn confirm_tab_with(
    name: SharedString,
    description: SharedString,
    on_confirm: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    open(
        t!("close.tab_title", name = name),
        description,
        t!("close.confirm"),
        Rc::new(on_confirm),
        window,
        cx,
    );
}

/// 确认关闭窗口：`description` 说明关闭后会怎样（连接断开、传输中断……）。
pub fn confirm_window(
    description: SharedString,
    on_confirm: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    open(
        t!("close.window_title"),
        description,
        t!("close.confirm"),
        Rc::new(on_confirm),
        window,
        cx,
    );
}

/// 确认一次关闭 `count` 个 Tab（“关闭其他 Tab”“关闭所有 Tab”）：`description` 汇总这些 Tab（[`CloseSummary::description`]）。
pub fn confirm_tabs(
    count: usize,
    description: SharedString,
    on_confirm: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    open(
        t!("close.tabs_title", count = count),
        description,
        t!("close.confirm"),
        Rc::new(on_confirm),
        window,
        cx,
    );
}

/// 确认退出 Termlet：`description` 汇总所有窗口（[`CloseSummary::description`]）。
pub fn confirm_quit(
    description: SharedString,
    on_confirm: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    open(
        t!("close.quit_title"),
        description,
        t!("close.quit_confirm"),
        Rc::new(on_confirm),
        window,
        cx,
    );
}

fn open(
    title: SharedString,
    description: SharedString,
    confirm_label: SharedString,
    on_confirm: OnConfirm,
    window: &mut Window,
    cx: &mut App,
) {
    let form = cx.new(|_| CloseForm {
        description,
        confirm_label,
        dont_ask: false,
        on_confirm,
    });
    let width = rems(26.).to_pixels(window.rem_size());
    window.open_dialog(cx, move |dialog, _, _| {
        dialog.title(title.clone()).w(width).child(form.clone())
    });
}

struct CloseForm {
    description: SharedString,
    /// 确认按钮：关闭、退出
    confirm_label: SharedString,
    dont_ask: bool,
    on_confirm: OnConfirm,
}

impl CloseForm {
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dont_ask {
            let store = AppState::store(cx);
            if let Err(e) = store.update_settings(|s| s.general.confirm_close = false) {
                tracing::error!("保存设置失败：{e}");
            }
        }
        window.close_dialog(cx);
        (self.on_confirm)(window, cx);
    }
}

impl Render for CloseForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(prose(self.description.clone())),
            )
            .child(
                Checkbox::new("close-dont-ask")
                    .prose_label(t!("close.dont_ask"))
                    .checked(self.dont_ask)
                    .on_change(cx.listener(|this, checked: &bool, _, cx| {
                        this.dont_ask = *checked;
                        cx.notify();
                    })),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("close-cancel")
                            .label(t!("common.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("close-confirm")
                            .with_variant(ButtonVariant::Danger)
                            .label(self.confirm_label.clone())
                            .on_click(cx.listener(|this, _, window, cx| this.confirm(window, cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{CloseScope, CloseSummary};
    use crate::t;

    // 一次关闭几个 Tab：连接着的、本地终端、传输、未保存的文件各说一句，说的是“这些 Tab”而不是窗口
    #[test]
    fn tabs_description_covers_every_kind() {
        let summary = CloseSummary {
            pending: 2,
            unsaved: 1,
            connected: true,
            local: true,
            running: 1,
        };
        let text = summary.description(CloseScope::Tabs);
        for part in [
            t!("close.tabs_description"),
            t!("close.tabs_description_local"),
            t!("sftp.close_transfers", count = 2),
            t!("editor.close_window_unsaved", count = 1),
            t!("close.batch_running", count = 1),
        ] {
            assert!(text.contains(part.as_ref()), "{text}");
        }
        assert!(!text.contains(t!("close.window_description").as_ref()));
    }
}
