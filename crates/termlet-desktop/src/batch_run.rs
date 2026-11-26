//! 批量执行 Tab：在一批主机或几个已打开的终端里执行同一条命令，看合并后的结果。撰写栏、同步输入、快捷命令只管
//! “发送”（结果在各终端里看），要结果的在这里“执行”。
//!
//! - 目标：“主机”按分组、标签、搜索勾选（[`hosts`]），不打开终端、新建连接执行，凭据与打开终端时相同（不要求“允许
//!   AI 工具与脚本使用”）；不能询问，指纹没确认、要输入密码或验证码的为连不上，可以在终端中打开它。“已打开的终端”
//!   在终端里执行（要 bash / sh，终端停在提示符），沿用当前目录与环境，经堡垒机进去的主机用它；撰写栏的“执行并对比”
//!   就是打开这种目标、立即执行。
//! - 命令可以多行；⚡ 插入快捷命令（带参数的先填写）；Ctrl+Enter（macOS ⌘↩）执行，在首行按 ↑、末行按 ↓ 翻看这个 Tab
//!   执行过的命令。
//! - 执行前按危险命令规则检查一次，确认框写明台数；同时最多 10 台，不限时，可以停止。
//! - 结果（[`results`]）：进度一行，按“退出码 + 输出”分组，没有成功的列出原因；可以重试没有成功的、复制结果。只留
//!   最近一次，不保存。
//! - 每台一条审计记录（来源“批量执行”，同一批带同一个批 ID）；使用统计记 `batch_exec`（[`run`]）。
//! - 非模态：执行期间照常用终端；Tab 可以移到别的窗口；还在执行时关闭先确认，关闭即停止。

mod hosts;
mod results;
mod results_view;
mod run;
#[cfg(test)]
mod tests;

use std::time::Instant;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::dock::{BasePanel, Panel, PanelEvent};
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::radio::RadioGroup;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, ClickEvent, ClipboardItem, Context, Entity, EntityId, EventEmitter,
    FocusHandle, Focusable, InteractiveElement as _, IntoElement, KeystrokeEvent,
    ParentElement as _, Render, ScrollHandle, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, TestSupportExt as _, WeakEntity, Window, div,
    prelude::FluentBuilder as _,
};
use termlet_core::guard::{Guard, find_danger};
use termlet_core::id::HostId;
use tokio::sync::watch;

use crate::app_state::AppState;
use crate::i18n::LocalizedPlaceholder as _;
use crate::keymap::pressed;
use crate::prose::prose;
use crate::t;
use crate::terminal_panel::TerminalPanel;
use results::Results;
pub(crate) use results::TargetKey;

const PANEL_NAME: &str = "BatchPanel";

/// 执行过的命令最多记这么多条。
const HISTORY_MAX: usize = 50;

/// 执行的目标是哪一种。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TargetKind {
    /// 主机：新建连接执行，不打开终端
    Hosts,
    /// 已打开的终端：在终端里执行
    Terminals,
}

pub(crate) enum BatchPanelEvent {
    /// 关闭这个 Tab（已确认或不用确认）
    Close,
}

/// 正在执行的一次。收结果的任务不归它管（停止后还要收完、把开始了的写进审计日志），Tab 关掉后自己结束。
struct Running {
    /// 置为真（或丢掉）时停止：主机上的命令收到 SIGTERM，终端里的收到 Ctrl+C
    stop: watch::Sender<bool>,
    started: Instant,
}

pub(crate) struct BatchPanel {
    focus: FocusHandle,
    kind: TargetKind,
    /// 选中的主机（按选择时树中的顺序）
    hosts: Vec<HostId>,
    terminals: Vec<WeakEntity<TerminalPanel>>,
    input: Entity<TextareaState>,
    history: Vec<String>,
    /// 正在翻看的执行记录；`None` 为在最新之后（编辑中的内容）
    cursor: Option<usize>,
    /// 最近一次的结果
    results: Option<Results>,
    running: Option<Running>,
    /// 第几次执行（见 [`run`]）
    generation: u64,
    scroll: ScrollHandle,
    _intercept: Subscription,
    _window_focus: Option<Subscription>,
}

impl EventEmitter<PanelEvent> for BatchPanel {}
impl EventEmitter<BatchPanelEvent> for BatchPanel {}

impl Focusable for BatchPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl BatchPanel {
    /// 目标是主机。
    pub(crate) fn for_hosts(hosts: Vec<HostId>, window: &mut Window, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let mut this = Self::new(TargetKind::Hosts, window, cx);
            this.hosts = hosts;
            this
        })
    }

    /// 目标是已打开的终端（撰写栏的“执行并对比”，随后 [`Self::run_in_terminals`]）。
    pub(crate) fn for_terminals(window: &mut Window, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| Self::new(TargetKind::Terminals, window, cx))
    }

    fn new(kind: TargetKind, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 12)
                .localized_placeholder(|| t!("batch.placeholder"), window, cx)
        });
        let this = cx.entity().downgrade();
        let intercept = cx.intercept_keystrokes(move |event: &KeystrokeEvent, window, cx| {
            let _ = this.update(cx, |this, cx| this.on_keystroke(event, window, cx));
        });
        let mut panel = Self {
            focus: cx.focus_handle(),
            kind,
            hosts: Vec::new(),
            terminals: Vec::new(),
            input,
            history: Vec::new(),
            cursor: None,
            results: None,
            running: None,
            generation: 0,
            scroll: ScrollHandle::new(),
            _intercept: intercept,
            _window_focus: None,
        };
        panel.watch_focus(window, cx);
        panel
    }

    /// Tab 获得焦点时交给命令输入框（输入框自己的处理登记在打开它的窗口上，移到别的窗口后在这里补上）。
    fn watch_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.input.clone();
        self._window_focus = Some(cx.on_focus(&self.focus, window, move |_, window, cx| {
            input.update(cx, |input, cx| input.focus(window, cx));
        }));
    }

    /// Tab 移到别的窗口（见 `workspace/move_tab.rs`）：命令、结果与进行中的执行都不变，焦点的处理在新窗口中重新登记。
    pub(crate) fn rebind_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.watch_focus(window, cx);
        cx.notify();
    }

    pub(crate) fn kind(&self) -> TargetKind {
        self.kind
    }

    pub(crate) fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// 换成这几个终端与命令，立即执行（撰写栏的“执行并对比”在已有的 Tab 中再执行一次）。
    pub(crate) fn run_in_terminals(
        &mut self,
        terminals: Vec<WeakEntity<TerminalPanel>>,
        command: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.running.is_some() {
            return;
        }
        self.kind = TargetKind::Terminals;
        self.terminals = terminals;
        self.input
            .update(cx, |s, cx| s.set_value(command.to_string(), window, cx));
        self.execute(window, cx);
    }

    /// Tab 上的名称：“批量执行 · 4 台主机”“批量执行 · 3 个终端”。
    pub(crate) fn title_text(&self, cx: &App) -> SharedString {
        match self.kind {
            TargetKind::Hosts => match self.hosts.len() {
                0 => t!("batch.title"),
                count => t!("batch.tab_hosts", count = count),
            },
            TargetKind::Terminals => match self.live_terminals(cx).len() {
                0 => t!("batch.title"),
                count => t!("batch.tab_terminals", count = count),
            },
        }
    }

    /// 不再确认、直接关闭（一次关闭几个 Tab 时）；还在执行的随之停止。
    pub(crate) fn close_tab(&mut self, cx: &mut Context<Self>) {
        self.running = None;
        cx.emit(BatchPanelEvent::Close);
    }

    /// 点了 ×：还在执行时先确认（关闭即停止；与未保存的文件一样总要确认，不受“关闭前确认”的设置影响）。
    pub(crate) fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.running.is_none() {
            self.close_tab(cx);
            return;
        }
        let this = cx.entity().downgrade();
        let title = t!("close.tab_title", name = self.title_text(cx));
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let this = this.clone();
            alert
                .confirm()
                .title(title.clone())
                .description(prose(t!("batch.close_running")))
                .ok_text(t!("batch.stop_and_close"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, _, cx| {
                    let _ = this.update(cx, |this, cx| this.close_tab(cx));
                    true
                })
        });
    }

    // ------------------------------------------------------------ 目标

    /// 还在的、选中的终端。
    fn live_terminals(&self, cx: &App) -> Vec<Entity<TerminalPanel>> {
        self.terminals
            .iter()
            .filter_map(|t| t.upgrade())
            .filter(|t| t.read(cx).is_live(cx))
            .collect()
    }

    /// 换一种目标。换到“已打开的终端”时还没有选终端的，默认选这个窗口里连接着的全部终端。
    fn set_kind(&mut self, kind: TargetKind, window: &Window, cx: &mut Context<Self>) {
        self.kind = kind;
        if kind == TargetKind::Terminals
            && self.live_terminals(cx).is_empty()
            && let Some(workspace) = crate::workspace::Workspace::in_window(window, cx)
        {
            self.terminals = workspace
                .read(cx)
                .live_terminals()
                .into_iter()
                .filter(|t| t.read(cx).is_live(cx))
                .map(|t| t.downgrade())
                .collect();
        }
        cx.notify();
    }

    /// 打开“选择主机”。
    pub(crate) fn pick_hosts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let on_done =
            std::rc::Rc::new(move |ids: Vec<HostId>, window: &mut Window, cx: &mut App| {
                let _ = this.update(cx, |this, cx| {
                    this.hosts = ids;
                    this.input.update(cx, |s, cx| s.focus(window, cx));
                    cx.notify();
                });
            });
        hosts::open(self.hosts.clone(), on_done, window, cx);
    }

    /// 勾选或取消一个终端。
    fn toggle_terminal(&mut self, panel: &Entity<TerminalPanel>, cx: &mut Context<Self>) {
        let id = panel.entity_id();
        let before = self.terminals.len();
        self.terminals.retain(|t| t.entity_id() != id);
        if self.terminals.len() == before {
            self.terminals.push(panel.downgrade());
        }
        cx.notify();
    }

    // ------------------------------------------------------------ 命令

    /// 命令输入框有焦点时：Ctrl+Enter（macOS ⌘↩）执行；在首行按 ↑、末行按 ↓ 翻看执行过的命令。
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
        let os = cx.platform_info().os;
        let m = keystroke.modifiers;
        let plain = !m.control && !m.alt && !m.shift && !m.platform;
        let run = if os == gpui_kit::OperatingSystem::Mac {
            "cmd-enter"
        } else {
            "ctrl-enter"
        };
        match keystroke.key.as_str() {
            _ if pressed(keystroke, run, os) => self.execute(window, cx),
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

    /// 翻看执行记录：`step` 为 -1 时更早，1 时更新；翻过最新一条后为空。
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

    fn remember(&mut self, command: &str) {
        self.history.retain(|h| h != command);
        self.history.push(command.to_string());
        if self.history.len() > HISTORY_MAX {
            self.history.remove(0);
        }
        self.cursor = None;
    }

    /// ⚡ 插入快捷命令：在快捷命令选择器中选一条（带参数的先填写），插入到光标处。
    fn insert_snippet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.input.clone();
        crate::snippets::picker::open_insert(
            move |text, window, cx| {
                let text = text.trim_end_matches(['\r', '\n']).to_string();
                input.update(cx, |s, cx| {
                    s.replace(text, window, cx);
                    s.focus(window, cx);
                });
            },
            window,
            cx,
        );
    }

    /// 要执行的命令：去掉末尾的空白；没有内容时为 `None`。
    fn command(&self, cx: &App) -> Option<String> {
        let value = self.input.read(cx).value();
        let command = value.trim_end();
        (!command.trim().is_empty()).then(|| command.to_string())
    }

    /// 能执行：有命令、有目标、没有在执行。
    fn can_execute(&self, cx: &App) -> bool {
        self.running.is_none()
            && self.command(cx).is_some()
            && match self.kind {
                TargetKind::Hosts => !self.hosts.is_empty(),
                TargetKind::Terminals => !self.live_terminals(cx).is_empty(),
            }
    }

    // ------------------------------------------------------------ 执行

    /// 执行并对比：先查危险命令规则（命中时确认，写明台数），再在全部目标上执行。
    pub(crate) fn execute(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_execute(cx) {
            return;
        }
        let Some(command) = self.command(cx) else {
            return;
        };
        let targets = match self.kind {
            TargetKind::Hosts => run::Targets::Hosts(self.hosts.clone()),
            TargetKind::Terminals => run::Targets::Terminals(self.live_terminals(cx)),
        };
        self.confirm_and_run(command, targets, window, cx);
    }

    /// 只在没有成功的目标上再执行一次同一条命令。
    fn retry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.running.is_some() {
            return;
        }
        let Some(results) = &self.results else {
            return;
        };
        let command = results.command.to_string();
        let keys = results.retry_keys();
        if keys.is_empty() {
            return;
        }
        let hosts: Vec<HostId> = keys
            .iter()
            .filter_map(|k| match k {
                TargetKey::Host(id) => Some(id.clone()),
                TargetKey::Terminal(_) => None,
            })
            .collect();
        let terminals: Vec<Entity<TerminalPanel>> = self
            .terminals
            .iter()
            .filter_map(|t| t.upgrade())
            .filter(|t| keys.contains(&TargetKey::Terminal(t.entity_id())))
            .collect();
        let targets = if hosts.is_empty() {
            run::Targets::Terminals(terminals)
        } else {
            run::Targets::Hosts(hosts)
        };
        self.confirm_and_run(command, targets, window, cx);
    }

    fn confirm_and_run(
        &mut self,
        command: String,
        targets: run::Targets,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = targets.runnable(cx);
        if count == 0 {
            // 选中的都不能执行（Telnet、串口主机）：照样列出结果，写明原因
            self.start(command, targets, false, window, cx);
            return;
        }
        let settings = AppState::store(cx).settings();
        let guard = Guard::from_settings(&settings.guard).0;
        let Some(danger) = find_danger(&guard, &command) else {
            self.start(command, targets, false, window, cx);
            return;
        };
        tracing::info!(rule = %danger.rule.pattern(), targets = count, "批量执行：命中危险命令规则，先确认");
        let rule = crate::i18n::rule_description(&danger.rule);
        let rule = if rule.is_empty() {
            danger.rule.pattern().to_string()
        } else {
            rule.to_string()
        };
        let title = match &targets {
            run::Targets::Hosts(_) => t!("batch.danger_hosts", count = count),
            run::Targets::Terminals(_) => t!("batch.danger_terminals", count = count),
        };
        let this = cx.entity().downgrade();
        let pending = std::rc::Rc::new(std::cell::RefCell::new(Some((command, targets))));
        let input = self.input.clone();
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let (this, pending, input) = (this.clone(), pending.clone(), input.clone());
            alert
                .confirm()
                .title(title.clone())
                .description(prose(t!(
                    "compose.danger_hint",
                    command = danger.line.clone(),
                    rule = rule.clone()
                )))
                .ok_text(t!("multi_exec.run_anyway"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, window, cx| {
                    if let Some((command, targets)) = pending.borrow_mut().take() {
                        let _ = this.update(cx, |this, cx| {
                            this.start(command, targets, true, window, cx)
                        });
                    }
                    true
                })
                .on_close(move |_, window, cx| {
                    input.update(cx, |s, cx| s.focus(window, cx));
                })
        });
    }

    /// 停止：还在执行的收到 SIGTERM（主机）或 Ctrl+C（终端），没开始的不再开始；都标为已取消。
    fn stop(&mut self, cx: &mut Context<Self>) {
        let Some(running) = self.running.take() else {
            return;
        };
        tracing::info!("批量执行：停止");
        running.stop.send_replace(true);
        if let Some(results) = self.results.as_mut() {
            results.cancel_pending();
            results.elapsed = Some(running.started.elapsed());
        }
        cx.notify();
    }

    fn copy_results(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(results) = &self.results else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(results.copy_text()));
        window.push_notification(Notification::success(t!("batch.copied")), cx);
    }

    // ------------------------------------------------------------ 渲染

    fn render_target(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let running = self.running.is_some();
        let kinds = [TargetKind::Hosts, TargetKind::Terminals];
        let picker = match self.kind {
            TargetKind::Hosts => {
                let label = match self.hosts.len() {
                    0 => t!("batch.pick_hosts"),
                    count => t!("batch.hosts", count = count),
                };
                Button::new("batch-target-hosts")
                    .outline()
                    .small()
                    .icon(IconName::Server)
                    .label(label)
                    .disabled(running)
                    .on_click(cx.listener(|this, _, window, cx| this.pick_hosts(window, cx)))
                    .into_any_element()
            }
            TargetKind::Terminals => {
                let live = self.live_terminals(cx);
                let label = if live.is_empty() {
                    t!("batch.pick_terminals")
                } else {
                    let names: Vec<SharedString> =
                        live.iter().map(|t| t.read(cx).name().clone()).collect();
                    names.join(&t!("batch.name_separator")).into()
                };
                let candidates: Vec<Entity<TerminalPanel>> =
                    crate::workspace::Workspace::in_window(window, cx)
                        .map(|w| w.read(cx).live_terminals())
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|t| t.read(cx).is_live(cx))
                        .collect();
                let chosen: Vec<EntityId> = live.iter().map(|t| t.entity_id()).collect();
                let this = cx.entity().downgrade();
                Button::new("batch-target-terminals")
                    .outline()
                    .small()
                    .icon(IconName::SquareTerminal)
                    .label(label)
                    .disabled(running)
                    .dropdown_menu(move |menu: PopupMenu, _, cx| {
                        if candidates.is_empty() {
                            return menu
                                .item(PopupMenuItem::new(t!("batch.no_terminals")).disabled(true));
                        }
                        let mut menu = menu.label(t!("compose.pick"));
                        for panel in &candidates {
                            let (this, target) = (this.clone(), panel.clone());
                            menu = menu.item(
                                PopupMenuItem::new(panel.read(cx).name().clone())
                                    .checked(chosen.contains(&panel.entity_id()))
                                    .on_click(move |_, _, cx| {
                                        let _ = this.update(cx, |this, cx| {
                                            this.toggle_terminal(&target, cx)
                                        });
                                    }),
                            );
                        }
                        menu
                    })
                    .into_any_element()
            }
        };
        let hint = match self.kind {
            TargetKind::Hosts => t!("batch.hosts_hint"),
            TargetKind::Terminals => t!("batch.terminals_hint"),
        };
        h_flex()
            .gap_3()
            .flex_wrap()
            .items_center()
            .child(
                // 单选组自己占满一行：收到内容的宽度，选主机的按钮紧跟在后面
                div().flex_none().child(
                    RadioGroup::horizontal("batch-target-kind")
                        .children([t!("batch.target_hosts"), t!("batch.target_terminals")])
                        .selected_index(kinds.iter().position(|k| *k == self.kind))
                        .disabled(running)
                        .on_change(cx.listener(move |this, ix: &usize, window, cx| {
                            if let Some(kind) = kinds.get(*ix) {
                                this.set_kind(*kind, window, cx);
                            }
                        })),
                ),
            )
            .child(picker)
            .child(
                div()
                    .min_w_0()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(prose(hint)),
            )
    }

    fn render_command(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let running = self.running.is_some();
        let run_key = termlet_core::keymap::label_of(
            if cx.platform_info().os == gpui_kit::OperatingSystem::Mac {
                "cmd-enter"
            } else {
                "ctrl-enter"
            },
            crate::keymap::current_os(cx),
        );
        v_flex()
            .gap_2()
            .child(
                Textarea::new(&self.input)
                    .small()
                    .font_family(termlet_terminal::fonts::DEFAULT_FAMILY),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("batch-insert-snippet")
                            .ghost()
                            .small()
                            .icon(Icon::new(IconName::Zap).text_color(theme.warning))
                            .label(t!("batch.insert_snippet"))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.insert_snippet(window, cx)),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(prose(t!("batch.command_hint", key = run_key))),
                    )
                    .when(running, |this| {
                        this.child(
                            Button::new("batch-stop")
                                .outline()
                                .small()
                                .icon(IconName::Square)
                                .label(t!("batch.stop"))
                                .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                        )
                    })
                    .child(
                        Button::new("batch-run")
                            .primary()
                            .small()
                            .icon(IconName::GitCompare)
                            .label(t!("compose.compare"))
                            .loading(running)
                            .disabled(!self.can_execute(cx))
                            .on_click(cx.listener(|this, _, window, cx| this.execute(window, cx))),
                    ),
            )
    }
}

impl BasePanel for BatchPanel {
    fn panel_name(&self) -> &'static str {
        PANEL_NAME
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        if active {
            self.input.update(cx, |input, cx| input.focus(window, cx));
        }
    }
}

impl BatchPanel {
    /// 竖排的 Tab 列表中这个 Tab 的一行：与横排的 Tab 上相同的图标与名称，执行中有圆点；不归在哪台主机下。
    pub(crate) fn tab_face(&self, cx: &App) -> crate::workspace::TabFace {
        let running = self.running.is_some();
        crate::workspace::TabFace {
            icon: Icon::new(IconName::Layers)
                .xsmall()
                .text_color(cx.theme().info)
                .into_any_element(),
            rail_icon: Icon::new(IconName::Layers)
                .with_size(crate::workspace::RAIL_ICON)
                .text_color(cx.theme().info)
                .into_any_element(),
            name: self.title_text(cx),
            detail: self.results.as_ref().map(|r| r.command.clone()),
            badges: running
                .then(|| {
                    div()
                        .size_1p5()
                        .rounded_full()
                        .bg(cx.theme().info)
                        .into_any_element()
                })
                .into_iter()
                .collect(),
            status: running
                .then(|| crate::workspace::TabStatus::dot(cx.theme().info, t!("batch.tab_running")))
                .into_iter()
                .collect(),
            host: None,
        }
    }
}

impl Panel for BatchPanel {
    fn title(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let id = cx.entity_id().as_u64() as usize;
        // 右键菜单开着时关闭按钮不挂悬停提示（会浮在菜单上，见 crate::menu_tooltip）
        let menu_open = crate::menu_tooltip::menu_open(window, cx);
        let tip = self
            .results
            .as_ref()
            .map(|r| r.command.clone())
            .unwrap_or_else(|| self.title_text(cx));
        h_flex()
            .gap_1()
            .child(
                h_flex()
                    .id(("batch-tab-title", id))
                    .test_support()
                    .gap_1()
                    .tooltip(crate::menu_tooltip::quiet(move |window, cx| {
                        Tooltip::new(tip.clone()).build(window, cx)
                    }))
                    // 右键 Tab 打开这个 Tab 的菜单（Tab 栏上没有“…”，见 dock_skin）
                    .context_menu({
                        let this = cx.entity().downgrade();
                        move |menu, window, cx| match this.upgrade() {
                            Some(panel) => {
                                panel.update(cx, |panel, cx| panel.dropdown_menu(menu, window, cx))
                            }
                            None => menu,
                        }
                    })
                    .child(
                        Icon::new(IconName::Layers)
                            .xsmall()
                            .text_color(cx.theme().info),
                    )
                    .child(self.title_text(cx)),
            )
            .when(self.running.is_some(), |this| {
                this.child(div().size_1p5().rounded_full().bg(cx.theme().info))
            })
            .child(
                // 自己的关闭按钮（Dock 自带的不显示）
                Button::new(("batch-tab-close", id))
                    .ghost()
                    .small()
                    .icon(IconName::Close)
                    .tab_stop(false)
                    .when(!menu_open, |this| this.tooltip(t!("panel.close")))
                    .on_click(cx.listener(|this, _, window, cx| {
                        cx.stop_propagation();
                        this.request_close(window, cx);
                    })),
            )
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }

    /// Tab 菜单（右键 Tab 打开）：停止、重试没有成功的、复制结果、关闭与关闭几个 Tab。
    fn dropdown_menu(
        &mut self,
        menu: PopupMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PopupMenu {
        let this = cx.entity().downgrade();
        let act = |f: fn(&mut BatchPanel, &mut Window, &mut Context<BatchPanel>)| {
            let this = this.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let _ = this.update(cx, |this, cx| f(this, window, cx));
            }
        };
        let running = self.running.is_some();
        let retryable = !running
            && self
                .results
                .as_ref()
                .is_some_and(|r| !r.retry_keys().is_empty());
        let menu = menu
            .item(
                PopupMenuItem::new(t!("batch.stop"))
                    .icon(IconName::Square)
                    .disabled(!running)
                    .on_click(act(|this, _, cx| this.stop(cx))),
            )
            .item(
                PopupMenuItem::new(t!("batch.retry"))
                    .icon(IconName::RotateCw)
                    .disabled(!retryable)
                    .on_click(act(|this, window, cx| this.retry(window, cx))),
            )
            .item(
                PopupMenuItem::new(t!("batch.copy"))
                    .icon(IconName::Copy)
                    .disabled(self.results.is_none())
                    .on_click(act(|this, window, cx| this.copy_results(window, cx))),
            )
            .separator()
            .item(
                PopupMenuItem::new(t!("panel.close"))
                    .on_click(act(|this, window, cx| this.request_close(window, cx))),
            );
        crate::workspace::close_tab_items(menu, cx.entity_id(), window, cx).separator()
    }
}

impl Render for BatchPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().border;
        let results = self.render_results(window, cx);
        v_flex()
            .id("batch-panel")
            .test_support()
            .track_focus(&self.focus)
            .size_full()
            .bg(cx.theme().background)
            .child(
                v_flex()
                    .flex_none()
                    .gap_3()
                    .p_3()
                    .border_b_1()
                    .border_color(border)
                    .child(self.render_target(window, cx))
                    .child(self.render_command(cx)),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        results
                            .id("batch-results")
                            .test_support()
                            .size_full()
                            .track_scroll(&self.scroll)
                            .overflow_y_scroll(),
                    )
                    .vertical_scrollbar(&self.scroll),
            )
    }
}
