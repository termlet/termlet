//! 撰写栏（Ctrl+Shift+M）：在主区底部写好命令，一次发送到多个终端，或在各终端中执行并对比输出。
//!
//! - 发送到：当前 Tab（显示着的全部窗格，同步输入的网格中是网格里的全部）、同一主机、全部终端，或自选。
//! - Enter 发送（每行加回车，不用括号粘贴），Shift+Enter 换行，↑ ↓ 翻看这次打开以来发送过的命令（最多 50 条），Esc 关闭。
//! - 发送前总是检查危险命令规则（与粘贴、CLI 共用），命中时先确认。
//! - “执行并对比”交给批量执行 Tab（[`crate::batch_run`]）：在选中的终端里执行，结果在那个 Tab 中分组对比。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, h_flex};
use gpui_kit::{
    AppContext as _, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeystrokeEvent, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, WeakEntity, Window, div,
};
use termlet_core::guard::{Guard, find_danger};

use crate::app_state::AppState;
use crate::i18n;
use crate::i18n::LocalizedPlaceholder as _;
use crate::prose::prose;
use crate::t;
use crate::terminal_panel::TerminalPanel;
use crate::workspace::Workspace;

/// 发送过的命令最多记这么多条。
const HISTORY_MAX: usize = 50;

/// 发送到哪些终端。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    View,
    Host,
    All,
    Custom(Vec<EntityId>),
}

pub enum ComposeEvent {
    /// 关闭撰写栏（Esc、×）：焦点还给当前终端
    Close,
}

pub struct ComposeBar {
    workspace: WeakEntity<Workspace>,
    input: Entity<TextareaState>,
    mode: Mode,
    history: Vec<String>,
    /// 正在翻看的历史；`None` 为在最新之后（编辑中的内容）
    cursor: Option<usize>,
    _intercept: Subscription,
}

impl EventEmitter<ComposeEvent> for ComposeBar {}

impl Focusable for ComposeBar {
    fn focus_handle(&self, cx: &gpui_kit::App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

/// 拆成要执行的各行：去掉行尾空白、空行，行首缩进保留。
pub fn compose_lines(text: &str) -> Vec<String> {
    text.split('\n')
        .map(|line| line.trim_end().to_string())
        .filter(|line| !line.trim().is_empty())
        .collect()
}

impl ComposeBar {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 6)
                .localized_placeholder(|| t!("compose.placeholder"), window, cx)
        });
        let this = cx.entity().downgrade();
        let intercept = cx.intercept_keystrokes(move |event: &KeystrokeEvent, window, cx| {
            let _ = this.update(cx, |this, cx| this.on_keystroke(event, window, cx));
        });
        Self {
            workspace,
            input,
            mode: Mode::View,
            history: Vec::new(),
            cursor: None,
            _intercept: intercept,
        }
    }

    /// 把内容放进输入框（快捷命令选择器 Ctrl+Enter），替换原来的内容。
    pub fn set_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = text.trim_end_matches(['\r', '\n']).to_string();
        self.input.update(cx, |s, cx| {
            s.set_value(text, window, cx);
            s.focus(window, cx);
        });
        self.cursor = None;
    }

    /// 输入框有焦点时：Enter 发送、Esc 关闭、在首行按 ↑ / 末行按 ↓ 翻看历史。
    fn on_keystroke(
        &mut self,
        event: &KeystrokeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.input.focus_handle(cx).is_focused(window) {
            return;
        }
        let keystroke = &event.keystroke;
        let m = keystroke.modifiers;
        let plain = !m.control && !m.alt && !m.shift && !m.platform;
        match keystroke.key.as_str() {
            "enter" if !m.shift && !m.alt && !m.control => self.send(window, cx),
            "escape" if plain => cx.emit(ComposeEvent::Close),
            "up" if plain && self.input.read(cx).cursor_position().line == 0 => {
                if !self.recall(-1, window, cx) {
                    return;
                }
            }
            "down" if plain => {
                let state = self.input.read(cx);
                let last = state.value().matches('\n').count() as u32;
                if state.cursor_position().line < last || !self.recall(1, window, cx) {
                    return;
                }
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    /// 翻看历史：`step` 为 -1 时更早，1 时更新；翻过最新一条后为空。
    fn recall(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let next = match (self.cursor, step) {
            (None, -1) if !self.history.is_empty() => Some(self.history.len() - 1),
            (Some(0), -1) | (None, _) => return false,
            (Some(i), -1) => Some(i - 1),
            (Some(i), _) if i + 1 < self.history.len() => Some(i + 1),
            (Some(_), _) => None,
        };
        self.cursor = next;
        let text = next.map(|i| self.history[i].clone()).unwrap_or_default();
        self.input.update(cx, |s, cx| s.set_value(text, window, cx));
        true
    }

    fn remember(&mut self, text: String) {
        self.history.retain(|h| *h != text);
        self.history.push(text);
        if self.history.len() > HISTORY_MAX {
            self.history.remove(0);
        }
        self.cursor = None;
    }

    /// 当前的目标。
    fn targets(&self, window: &Window, cx: &gpui_kit::App) -> Vec<Entity<TerminalPanel>> {
        self.workspace
            .upgrade()
            .map(|w| w.read(cx).compose_targets(&self.mode, window, cx))
            .unwrap_or_default()
    }

    fn lines(&self, cx: &gpui_kit::App) -> Vec<String> {
        compose_lines(&self.input.read(cx).value())
    }

    /// 发送：先检查危险命令；成功后记入历史、清空输入框。
    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let lines = self.lines(cx);
        let targets = self.targets(window, cx);
        if lines.is_empty() || targets.is_empty() {
            return;
        }
        let this = cx.entity().downgrade();
        let checked = lines.clone();
        self.confirm_danger(&checked, targets.len(), window, cx, move |window, cx| {
            // 命令内容不写进日志
            tracing::info!(lines = lines.len(), targets = targets.len(), "撰写栏：发送");
            termlet_core::usage::mark(termlet_core::usage::Feature::Compose);
            let data: String = lines.iter().map(|l| format!("{l}\r")).collect();
            for panel in &targets {
                panel.update(cx, |p, cx| p.send_text(&data, cx));
            }
            let _ = this.update(cx, |this, cx| {
                this.remember(lines.join("\n"));
                this.input.update(cx, |s, cx| {
                    s.set_value("", window, cx);
                    s.focus(window, cx);
                });
                cx.notify();
            });
        });
    }

    /// 执行并对比：交给批量执行 Tab（目标是现在选中的终端，立即执行），结果在那里看；危险命令由它检查。
    fn compare(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let lines = self.lines(cx);
        let targets = self.targets(window, cx);
        if lines.is_empty() || targets.is_empty() {
            return;
        }
        tracing::info!(
            lines = lines.len(),
            targets = targets.len(),
            "撰写栏：执行并对比（在批量执行 Tab 中）"
        );
        termlet_core::usage::mark(termlet_core::usage::Feature::Compose);
        let command = lines.join("\n");
        self.remember(command.clone());
        self.input.update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
        let workspace = self.workspace.clone();
        // 等这次更新结束再打开（其中要更新工作区与新的 Tab）
        window.defer(cx, move |window, cx| {
            if let Some(workspace) = workspace.upgrade() {
                workspace.update(cx, |w, cx| {
                    w.run_batch_in_terminals(targets, &command, window, cx)
                });
            }
        });
    }

    /// 命中危险命令规则时先确认，确认（或没有命中）后执行 `then`。
    fn confirm_danger(
        &mut self,
        lines: &[String],
        count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Window, &mut gpui_kit::App) + 'static,
    ) {
        let settings = AppState::store(cx).settings();
        let guard = Guard::from_settings(&settings.guard).0;
        let Some(danger) = find_danger(&guard, &lines.join("\n")) else {
            // 等这次更新结束再执行（其中要更新撰写栏自己）
            window.defer(cx, then);
            return;
        };
        tracing::info!(rule = %danger.rule.pattern(), targets = count, "撰写栏：命中危险命令规则，先确认");
        let rule = i18n::rule_description(&danger.rule);
        let rule = if rule.is_empty() {
            danger.rule.pattern().to_string()
        } else {
            rule.to_string()
        };
        let then = std::rc::Rc::new(std::cell::RefCell::new(Some(then)));
        let input = self.input.clone();
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let then = then.clone();
            let input = input.clone();
            alert
                .confirm()
                .title(t!("compose.danger_title", count = count))
                .description(prose(t!(
                    "compose.danger_hint",
                    command = danger.line.clone(),
                    rule = rule.clone()
                )))
                .ok_text(t!("compose.send_anyway"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, window, cx| {
                    if let Some(then) = then.borrow_mut().take() {
                        then(window, cx);
                    }
                    true
                })
                .on_close(move |_, window, cx| {
                    input.update(cx, |s, cx| s.focus(window, cx));
                })
        });
    }

    fn mode_label(&self) -> SharedString {
        match self.mode {
            Mode::View => t!("compose.mode_view"),
            Mode::Host => t!("compose.mode_host"),
            Mode::All => t!("compose.mode_all"),
            Mode::Custom(_) => t!("compose.mode_custom"),
        }
    }

    /// 勾选或取消一个终端：以当前目标为起点增减，切到自选。
    fn toggle_target(&mut self, id: EntityId, window: &Window, cx: &mut Context<Self>) {
        let mut ids: Vec<EntityId> = self
            .targets(window, cx)
            .iter()
            .map(|p| p.entity_id())
            .collect();
        if let Some(i) = ids.iter().position(|x| *x == id) {
            ids.remove(i);
        } else {
            ids.push(id);
        }
        self.mode = Mode::Custom(ids);
        cx.notify();
    }
}

impl Render for ComposeBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let targets = self.targets(window, cx);
        let count = targets.len();
        let empty = self.lines(cx).is_empty() || count == 0;
        let candidates = self
            .workspace
            .upgrade()
            .map(|w| w.read(cx).compose_candidates(cx))
            .unwrap_or_default();
        let target_ids: Vec<EntityId> = targets.iter().map(|p| p.entity_id()).collect();
        let this = cx.entity().downgrade();
        let mode = self.mode.clone();
        let picker = Button::new("compose-targets")
            .outline()
            .small()
            .label(t!(
                "compose.send_to",
                mode = self.mode_label(),
                count = count
            ))
            .dropdown_menu(move |menu, _, _| {
                let set = |m: Mode| {
                    let this = this.clone();
                    move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut gpui_kit::App| {
                        let _ = this.update(cx, |this, cx| {
                            this.mode = m.clone();
                            cx.notify();
                        });
                    }
                };
                let mut menu = menu
                    .item(
                        PopupMenuItem::new(t!("compose.mode_view"))
                            .checked(mode == Mode::View)
                            .on_click(set(Mode::View)),
                    )
                    .item(
                        PopupMenuItem::new(t!("compose.mode_host"))
                            .checked(mode == Mode::Host)
                            .on_click(set(Mode::Host)),
                    )
                    .item(
                        PopupMenuItem::new(t!("compose.mode_all"))
                            .checked(mode == Mode::All)
                            .on_click(set(Mode::All)),
                    )
                    .separator()
                    .label(t!("compose.pick"));
                for (id, title) in &candidates {
                    let (this, id) = (this.clone(), *id);
                    menu = menu.item(
                        PopupMenuItem::new(title.clone())
                            .checked(target_ids.contains(&id))
                            .on_click(move |_, window, cx| {
                                let _ =
                                    this.update(cx, |this, cx| this.toggle_target(id, window, cx));
                            }),
                    );
                }
                menu
            });
        h_flex()
            .id("compose-bar")
            .flex_none()
            .items_end()
            .gap_2()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.sidebar)
            .text_xs()
            .child(Icon::new(IconName::Send).small().text_color(theme.warning))
            .child(picker)
            .child(
                div().flex_1().min_w_0().child(
                    Textarea::new(&self.input)
                        .small()
                        .font_family(termlet_terminal::fonts::DEFAULT_FAMILY),
                ),
            )
            .child(
                Button::new("compose-compare")
                    .outline()
                    .small()
                    .icon(IconName::GitCompare)
                    .label(t!("compose.compare"))
                    .tooltip(t!("compose.compare_hint"))
                    .disabled(empty)
                    .on_click(cx.listener(|this, _, window, cx| this.compare(window, cx))),
            )
            .child(
                Button::new("compose-send")
                    .primary()
                    .small()
                    .label(t!("compose.send"))
                    .disabled(empty)
                    .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
            )
            .child(
                Button::new("compose-close")
                    .ghost()
                    .small()
                    .icon(IconName::Close)
                    .tooltip(t!("common.close"))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(ComposeEvent::Close))),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::compose_lines;

    #[test]
    fn lines_drop_blank_and_trailing_space_but_keep_indent() {
        assert_eq!(
            compose_lines("ls  \r\n\n   \n  cd /tmp\nuptime"),
            ["ls", "  cd /tmp", "uptime"]
        );
        assert!(compose_lines(" \n\t").is_empty());
    }
}
