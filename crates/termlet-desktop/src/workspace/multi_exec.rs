//! 同步输入（与 MobaXterm 的 MultiExec 相同）：本窗口的终端平铺成网格，键盘输入逐键发到同步中的各终端。
//!
//! - 开启时本窗口已连接的终端全部同步；之后新开的终端出现在网格里但不同步（“已排除”），断开、重连过的终端退出同步、
//!   不会自动加回。只能手动退出（快捷键、横幅、命令面板）。不改原来的 Tab 与分屏，退出后照原样显示。
//! - 只有同步中的终端会把输入分发出去：按键已按来源终端的模式编码，各终端按自己的光标键模式重新编码；停在 sudo
//!   密码提示上的终端按 Enter 时填入各自主机保存的密码。终端自己的回应、鼠标上报、滚轮不同步。
//! - 粘贴只确认一次（全部目标一起检查）；回车前推算各终端将要执行的命令，命中危险命令规则时先确认，取消则谁都不发。
//! - 输入排队按顺序处理，前一条在等确认时后面的等待；目标在处理时才确定。

use std::cell::Cell;
use std::collections::{HashSet, VecDeque};
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, Context, Entity, EntityId, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    TestSupportExt as _, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::guard::{
    Guard, PasteContext, checked_paste, feed_line, feed_paste, find_danger, inspect_paste,
    run_commands,
};

use super::Workspace;
use crate::app_state::AppState;
use crate::i18n;
use crate::prose::prose;
use crate::t;
use crate::terminal_panel::TerminalPanel;

/// 同步输入的状态（只作用于本窗口，不保存）。
pub(super) struct MultiExec {
    /// 同步中的终端
    members: Vec<EntityId>,
    /// 放大的窗格（只显示它）
    zoom: Option<EntityId>,
    /// 切到 SFTP、编辑器：照常显示它们，横幅上出现“返回网格”
    pub(super) away: bool,
    /// 推算的未执行的命令行（回车前检查危险命令用）
    line: String,
    /// 粘贴时已检查过危险命令的内容：紧接着回车执行它们时不再重复确认
    checked: Option<HashSet<String>>,
    queue: VecDeque<Pending>,
    /// 正在等确认框
    busy: bool,
}

enum Pending {
    Typed { source: EntityId, data: Vec<u8> },
    Paste { source: EntityId, text: String },
}

impl MultiExec {
    fn is_member(&self, id: EntityId) -> bool {
        self.members.contains(&id)
    }
}

/// 网格每行的窗格数：列数 ⌈√n⌉、行数 ⌈n/列⌉，每行 ⌊n/行⌋ 个，前 n % 行 行各多一个。
pub(super) fn grid_rows(n: usize) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }
    let cols = (n as f64).sqrt().ceil() as usize;
    let rows = n.div_ceil(cols);
    let base = n / rows;
    (0..rows)
        .map(|row| base + usize::from(row < n % rows))
        .collect()
}

impl Workspace {
    pub(crate) fn is_syncing(&self) -> bool {
        self.multi_exec.is_some()
    }

    /// 同步中的终端数；没有开启同步输入时为 `None`。
    pub fn synced_terminals(&self) -> Option<usize> {
        self.multi_exec.as_ref().map(|s| s.members.len())
    }

    /// 各终端屏幕上显示的文字（按打开的顺序）。
    pub fn terminal_screens(&self, cx: &App) -> Vec<String> {
        self.live_terminals()
            .iter()
            .map(|p| p.read(cx).screen_text(cx))
            .collect()
    }

    /// 开启或退出同步输入（Ctrl+Shift+I）。
    pub(super) fn toggle_multi_exec(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.multi_exec.is_some() {
            self.exit_multi_exec(window, cx);
            return;
        }
        if self.vault_gate.is_some() {
            return;
        }
        let terminals = self.live_terminals();
        let live: Vec<Entity<TerminalPanel>> = terminals
            .iter()
            .filter(|p| p.read(cx).is_live(cx))
            .cloned()
            .collect();
        if live.len() < 2 {
            crate::notify::error(t!("multi_exec.need_two"), window, cx);
            return;
        }
        let current = self.focused_terminal(window, cx);
        for panel in &terminals {
            panel.update(cx, |p, cx| p.set_shown(true, cx));
        }
        for panel in &live {
            panel.update(cx, |p, cx| p.set_sync_member(true, cx));
        }
        tracing::info!(
            members = live.len(),
            terminals = terminals.len(),
            "开启同步输入"
        );
        termlet_core::usage::mark(termlet_core::usage::Feature::SyncInput);
        self.multi_exec = Some(MultiExec {
            members: live.iter().map(|p| p.entity_id()).collect(),
            zoom: None,
            away: false,
            line: String::new(),
            checked: None,
            queue: VecDeque::new(),
            busy: false,
        });
        // 焦点在 SFTP、编辑器上时交给第一个同步中的终端
        let target = current.unwrap_or_else(|| live[0].clone());
        target.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /// 退出同步输入：终端回到原来的 Tab 与分屏（是否在前台以 Dock 当前的为准，同步期间可能切过 Tab）。
    pub(super) fn exit_multi_exec(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.multi_exec.take().is_none() {
            return;
        }
        tracing::info!("退出同步输入");
        let current = self.command_target(window, cx);
        for panel in self.live_terminals() {
            panel.update(cx, |p, cx| {
                p.set_sync_member(false, cx);
                p.set_shown(false, cx);
            });
        }
        if let Some(panel) = current {
            panel.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    /// 单个终端加入或退出同步；没有连接着的不能加入。
    fn set_sync(&mut self, panel: &Entity<TerminalPanel>, member: bool, cx: &mut Context<Self>) {
        let live = panel.read(cx).is_live(cx);
        let id = panel.entity_id();
        let Some(state) = self.multi_exec.as_mut() else {
            return;
        };
        if member && !live {
            return;
        }
        state.members.retain(|m| *m != id);
        if member {
            state.members.push(id);
        }
        panel.update(cx, |p, cx| p.set_sync_member(member, cx));
        cx.notify();
    }

    /// 全部同步 / 全部排除 / 反选（只涉及连接着的终端）。
    fn sync_bulk(&mut self, op: Bulk, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.multi_exec.as_ref() else {
            return;
        };
        let members = state.members.clone();
        for panel in self.live_terminals() {
            if !panel.read(cx).is_live(cx) {
                continue;
            }
            let member = members.contains(&panel.entity_id());
            let next = match op {
                Bulk::All => true,
                Bulk::None => false,
                Bulk::Invert => !member,
            };
            if next != member {
                self.set_sync(&panel, next, cx);
            }
        }
        self.focus_current_terminal(window, cx);
    }

    fn focus_current_terminal(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = self.command_target(window, cx) {
            panel.read(cx).focus_handle(cx).focus(window, cx);
        }
    }

    /// 终端状态变了：断开、重连、结束的退出同步（不自动加回）；没有终端了就退出同步输入。
    pub(super) fn prune_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.multi_exec.is_none() {
            return;
        }
        let terminals = self.live_terminals();
        if terminals.is_empty() {
            self.exit_multi_exec(window, cx);
            return;
        }
        let ids: Vec<EntityId> = terminals.iter().map(|p| p.entity_id()).collect();
        let dropped: Vec<Entity<TerminalPanel>> = terminals
            .iter()
            .filter(|p| p.read(cx).is_sync_member() && !p.read(cx).is_live(cx))
            .cloned()
            .collect();
        for panel in &dropped {
            panel.update(cx, |p, cx| p.set_sync_member(false, cx));
        }
        if let Some(state) = self.multi_exec.as_mut() {
            state
                .members
                .retain(|m| ids.contains(m) && !dropped.iter().any(|p| p.entity_id() == *m));
            if state.zoom.is_some_and(|z| !ids.contains(&z)) {
                state.zoom = None;
            }
        }
        // 同步期间新开的终端：显示在网格里（不在 Dock 的前台也算显示）
        for panel in &terminals {
            if !panel.read(cx).is_in_grid() {
                panel.update(cx, |p, cx| p.set_shown(true, cx));
            }
        }
    }

    // ------------------------------------------------------------ 分发

    /// 同步中的终端键入了内容。
    pub(super) fn on_sync_typed(
        &mut self,
        source: &Entity<TerminalPanel>,
        data: Vec<u8>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let source = source.entity_id();
        match self.multi_exec.as_mut() {
            Some(state) => state.queue.push_back(Pending::Typed { source, data }),
            None => return,
        }
        self.pump_sync(window, cx);
    }

    /// 同步中的终端粘贴。
    pub(super) fn on_sync_paste(
        &mut self,
        source: &Entity<TerminalPanel>,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let source = source.entity_id();
        match self.multi_exec.as_mut() {
            Some(state) => state.queue.push_back(Pending::Paste { source, text }),
            None => return,
        }
        self.pump_sync(window, cx);
    }

    /// 发给谁：来源同步中时是同步中、仍连接着的全部终端（含来源），否则只有来源。
    fn sync_targets(&self, source: EntityId, cx: &App) -> Vec<Entity<TerminalPanel>> {
        let terminals = self.live_terminals();
        let member = self
            .multi_exec
            .as_ref()
            .is_some_and(|s| s.is_member(source));
        terminals
            .into_iter()
            .filter(|p| {
                if member {
                    self.multi_exec
                        .as_ref()
                        .is_some_and(|s| s.is_member(p.entity_id()))
                        && p.read(cx).is_live(cx)
                } else {
                    p.entity_id() == source
                }
            })
            .collect()
    }

    /// 按顺序处理排队的输入；要确认时停下，确认框关闭后继续。
    fn pump_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        loop {
            let Some(state) = self.multi_exec.as_mut() else {
                return;
            };
            if state.busy {
                return;
            }
            let Some(next) = state.queue.pop_front() else {
                return;
            };
            match next {
                Pending::Typed { source, data } => self.sync_typed(source, data, window, cx),
                Pending::Paste { source, text } => self.sync_paste(source, text, window, cx),
            }
        }
    }

    fn sync_typed(
        &mut self,
        source: EntityId,
        data: Vec<u8>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let targets = self.sync_targets(source, cx);
        if targets.len() <= 1 {
            for panel in &targets {
                panel.update(cx, |p, cx| p.deliver_typed(&data, cx));
            }
            return;
        }
        let Some(state) = self.multi_exec.as_ref() else {
            return;
        };
        let text = String::from_utf8_lossy(&data);
        let fed = feed_line(&state.line, &text);
        let all_full_screen = targets.iter().all(|p| p.read(cx).is_alt_screen(cx));
        if fed.executed.is_empty() || all_full_screen {
            if let Some(state) = self.multi_exec.as_mut() {
                state.line = fed.line;
            }
            deliver_typed(&targets, &data, cx);
            return;
        }
        // 回车：推算的内容与（只按了回车时）各终端屏幕上的输入行一起检查
        let screen: Vec<String> = if data == b"\r" {
            targets
                .iter()
                .map(|p| p.read(cx).input_line(cx).unwrap_or_default())
                .collect()
        } else {
            Vec::new()
        };
        let commands = run_commands(&fed.executed, &screen, state.checked.as_ref());
        let guard = guard(cx);
        let danger = commands.iter().find_map(|c| find_danger(&guard, c));
        let Some(danger) = danger else {
            if let Some(state) = self.multi_exec.as_mut() {
                state.line = fed.line;
                state.checked = None;
            }
            deliver_typed(&targets, &data, cx);
            return;
        };
        // 命中危险命令：确认后发给全部目标，取消则谁都不发（各终端输入行里的命令还在）
        let count = targets.len();
        let rule = rule_description(&danger.rule);
        let ok = Rc::new(Cell::new(false));
        let this = cx.entity().downgrade();
        let line = fed.line;
        if let Some(state) = self.multi_exec.as_mut() {
            state.busy = true;
        }
        let weak_targets: Vec<_> = targets.iter().map(|p| p.downgrade()).collect();
        let source_panel = self
            .live_terminals()
            .into_iter()
            .find(|p| p.entity_id() == source)
            .map(|p| p.downgrade());
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let (ok_flag, closed_flag) = (ok.clone(), ok.clone());
            let (this, line, data) = (this.clone(), line.clone(), data.clone());
            let (targets, source_panel) = (weak_targets.clone(), source_panel.clone());
            alert
                .confirm()
                .title(t!("multi_exec.danger_title", count = count))
                .description(prose(t!(
                    "multi_exec.danger_hint",
                    command = danger.line.clone(),
                    rule = rule.clone()
                )))
                .ok_text(t!("multi_exec.run_anyway"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, _, _| {
                    ok_flag.set(true);
                    true
                })
                .on_close(move |_, window, cx| {
                    let confirmed = closed_flag.get();
                    let targets: Vec<_> = targets.iter().filter_map(|p| p.upgrade()).collect();
                    let source = source_panel.as_ref().and_then(|p| p.upgrade());
                    let _ = this.update(cx, |this, cx| {
                        if let Some(state) = this.multi_exec.as_mut() {
                            state.busy = false;
                            if confirmed {
                                state.line = line.clone();
                                state.checked = None;
                            }
                        }
                        if confirmed {
                            deliver_typed(&targets, &data, cx);
                        }
                        if let Some(source) = &source {
                            source.read(cx).focus_handle(cx).focus(window, cx);
                        }
                        this.pump_sync(window, cx);
                    });
                })
        });
    }

    fn sync_paste(
        &mut self,
        source: EntityId,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let targets = self.sync_targets(source, cx);
        if targets.is_empty() {
            return;
        }
        let context = PasteContext {
            bracketed: targets.iter().all(|p| p.read(cx).bracketed_paste(cx)),
            full_screen: targets.iter().all(|p| p.read(cx).is_alt_screen(cx)),
            warn: AppState::store(cx).settings().terminal.paste_warning,
            targets: targets.len(),
        };
        let check = inspect_paste(&guard(cx), &text, &context);
        if !check.needs_confirm() {
            self.finish_paste(&targets, &check.text, check.danger_checked, cx);
            return;
        }
        if let Some(state) = self.multi_exec.as_mut() {
            state.busy = true;
        }
        let this = cx.entity().downgrade();
        let weak_targets: Vec<_> = targets.iter().map(|p| p.downgrade()).collect();
        let source_panel = targets
            .iter()
            .find(|p| p.entity_id() == source)
            .map(|p| p.downgrade());
        let count = targets.len();
        crate::paste_guard::confirm_with(
            check.clone(),
            (count > 1).then(|| t!("multi_exec.paste_targets", count = count)),
            Box::new(move |result, window, cx| {
                let targets: Vec<_> = weak_targets.iter().filter_map(|p| p.upgrade()).collect();
                let source = source_panel.as_ref().and_then(|p| p.upgrade());
                let _ = this.update(cx, |this, cx| {
                    if let Some(state) = this.multi_exec.as_mut() {
                        state.busy = false;
                    }
                    if let Some(text) = result.filter(|t| !t.is_empty()) {
                        let checked = check.danger_checked_for(&text);
                        this.finish_paste(&targets, &text, checked, cx);
                    }
                    if let Some(source) = &source {
                        source.read(cx).focus_handle(cx).focus(window, cx);
                    }
                    this.pump_sync(window, cx);
                });
            }),
            window,
            cx,
        );
    }

    /// 粘贴给全部目标，更新推算的命令行。
    fn finish_paste(
        &mut self,
        targets: &[Entity<TerminalPanel>],
        text: &str,
        danger_checked: bool,
        cx: &mut Context<Self>,
    ) {
        let bracketed = targets.iter().any(|p| p.read(cx).bracketed_paste(cx));
        if let Some(state) = self.multi_exec.as_mut() {
            state.line = feed_paste(&state.line, text, bracketed);
            state.checked = danger_checked.then(|| checked_paste(text));
        }
        for panel in targets {
            panel.update(cx, |p, cx| p.deliver_paste(text, cx));
        }
    }

    // ------------------------------------------------------------ 渲染

    /// 同步输入的横幅：同步中的终端数、返回网格、全部同步 / 全部排除 / 反选、退出。
    pub(super) fn render_sync_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = self.multi_exec.as_ref()?;
        let theme = cx.theme();
        let count = state.members.len();
        let button =
            |id: &'static str, label: SharedString| Button::new(id).ghost().xsmall().label(label);
        Some(
            h_flex()
                .id("multi-exec-bar")
                .test_support()
                .flex_none()
                .px_3()
                .py_1()
                .gap_2()
                .text_sm()
                .bg(theme.warning.opacity(0.15))
                .border_b_1()
                .border_color(theme.warning.opacity(0.4))
                .child(
                    Icon::new(IconName::LayoutGrid)
                        .small()
                        .text_color(theme.warning),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(prose(if count > 0 {
                            t!("multi_exec.banner", count = count)
                        } else {
                            t!("multi_exec.banner_none")
                        })),
                )
                .when(state.away, |el| {
                    el.child(
                        button("multi-exec-back", t!("multi_exec.back_to_grid")).on_click(
                            cx.listener(|this, _, window, cx| this.back_to_grid(window, cx)),
                        ),
                    )
                })
                .child(button("multi-exec-all", t!("multi_exec.all")).on_click(
                    cx.listener(|this, _, window, cx| this.sync_bulk(Bulk::All, window, cx)),
                ))
                .child(button("multi-exec-none", t!("multi_exec.none")).on_click(
                    cx.listener(|this, _, window, cx| this.sync_bulk(Bulk::None, window, cx)),
                ))
                .child(
                    button("multi-exec-invert", t!("multi_exec.invert")).on_click(
                        cx.listener(|this, _, window, cx| this.sync_bulk(Bulk::Invert, window, cx)),
                    ),
                )
                .child(
                    Button::new("multi-exec-exit")
                        .outline()
                        .xsmall()
                        .label(t!("multi_exec.exit"))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.exit_multi_exec(window, cx)),
                        ),
                )
                .into_any_element(),
        )
    }

    /// 从 SFTP、编辑器回到网格：激活第一个同步中的终端（没有时第一个终端）。
    fn back_to_grid(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let terminals = self.live_terminals();
        let Some(state) = self.multi_exec.as_mut() else {
            return;
        };
        state.away = false;
        let target = terminals
            .iter()
            .find(|p| state.members.contains(&p.entity_id()))
            .or(terminals.first())
            .cloned();
        if let Some(panel) = target {
            panel.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    /// 网格：本窗口的全部终端按打开的顺序平铺，每个窗格有标题栏；放大时只显示那一个。
    pub(super) fn render_sync_grid(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(state) = self.multi_exec.as_ref() else {
            return div().into_any_element();
        };
        let terminals = self.live_terminals();
        let panes: Vec<Entity<TerminalPanel>> = match state.zoom {
            Some(zoom) => terminals
                .into_iter()
                .filter(|p| p.entity_id() == zoom)
                .collect(),
            None => terminals,
        };
        let border = cx.theme().border;
        let mut rest = panes.into_iter();
        let rows: Vec<AnyElement> = grid_rows(rest.len())
            .into_iter()
            .map(|count| {
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .children((0..count).filter_map(|i| {
                        rest.next().map(|panel| {
                            div()
                                .flex_1()
                                .min_w_0()
                                .h_full()
                                .when(i > 0, |el| el.border_l_1().border_color(border))
                                .child(self.render_pane(&panel, window, cx))
                        })
                    }))
                    .into_any_element()
            })
            .collect();
        v_flex()
            .id("multi-exec-grid")
            .test_support()
            .size_full()
            .children(rows.into_iter().enumerate().map(|(i, row)| {
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .when(i > 0, |el| el.border_t_1().border_color(border))
                    .child(row)
            }))
            .into_any_element()
    }

    fn render_pane(
        &self,
        panel: &Entity<TerminalPanel>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(state) = self.multi_exec.as_ref() else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let id = panel.entity_id();
        let key = id.as_u64() as usize;
        let member = state.is_member(id);
        let live = panel.read(cx).is_live(cx);
        let zoomed = state.zoom == Some(id);
        let focused = panel.read(cx).focus_handle(cx).contains_focused(window, cx);
        let (label, hint) = if member {
            (t!("multi_exec.on"), t!("multi_exec.exclude"))
        } else if live {
            (t!("multi_exec.off"), t!("multi_exec.include"))
        } else {
            (t!("multi_exec.offline"), t!("multi_exec.offline_hint"))
        };
        let sync_button = {
            let weak = panel.downgrade();
            let button = Button::new(("pane-sync", key))
                .xsmall()
                .label(label)
                .tooltip(hint)
                .disabled(!member && !live)
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    if let Some(panel) = weak.upgrade() {
                        this.set_sync(&panel, !member, cx);
                    }
                    this.focus_current_terminal(window, cx);
                }));
            if member {
                button.with_variant(ButtonVariant::Warning)
            } else {
                button.outline()
            }
        };
        let toggle_zoom = move |this: &mut Workspace, cx: &mut Context<Workspace>| {
            if let Some(state) = this.multi_exec.as_mut() {
                state.zoom = if state.zoom == Some(id) {
                    None
                } else {
                    Some(id)
                };
            }
            cx.notify();
        };
        let close = {
            let weak = panel.downgrade();
            move |window: &mut Window, cx: &mut App| {
                if let Some(panel) = weak.upgrade() {
                    panel.update(cx, |p, cx| p.request_close(window, cx));
                }
            }
        };
        let header = h_flex()
            .id(("pane-title", key))
            .flex_none()
            .h(rems(1.5))
            .px_2()
            .gap_2()
            .text_xs()
            .bg(if focused { theme.accent } else { theme.sidebar })
            .border_b_1()
            .border_color(theme.border)
            .child(
                Icon::new(IconName::SquareTerminal)
                    .xsmall()
                    .text_color(theme.muted_foreground),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(panel.read(cx).name().clone()),
            )
            .child(sync_button)
            .child(
                Button::new(("pane-zoom", key))
                    .ghost()
                    .small()
                    .icon(if zoomed {
                        IconName::Minimize
                    } else {
                        IconName::Maximize
                    })
                    .tooltip(if zoomed {
                        t!("multi_exec.unzoom")
                    } else {
                        t!("multi_exec.zoom")
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        toggle_zoom(this, cx)
                    })),
            )
            .child(
                Button::new(("pane-close", key))
                    .ghost()
                    .small()
                    .icon(IconName::Close)
                    .tooltip(t!("multi_exec.close_pane"))
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        close(window, cx)
                    }),
            )
            .on_click(
                cx.listener(move |this, event: &gpui_kit::ClickEvent, _, cx| {
                    if event.click_count() >= 2 {
                        toggle_zoom(this, cx);
                    }
                }),
            );
        v_flex()
            .size_full()
            .when(member, |el| {
                el.border_1().border_color(theme.warning.opacity(0.6))
            })
            .child(header)
            .child(div().flex_1().min_h_0().child(panel.clone()))
            .into_any_element()
    }
}

#[derive(Clone, Copy)]
enum Bulk {
    All,
    None,
    Invert,
}

fn deliver_typed(targets: &[Entity<TerminalPanel>], data: &[u8], cx: &mut App) {
    for panel in targets {
        panel.update(cx, |p, cx| p.deliver_typed(data, cx));
    }
}

/// 危险命令规则（与粘贴、CLI 共用设置中的规则）。
fn guard(cx: &App) -> Guard {
    Guard::from_settings(&AppState::store(cx).settings().guard).0
}

/// 规则的说明（按界面语言；自定义规则没有说明时用规则本身）。
fn rule_description(rule: &termlet_core::guard::Rule) -> String {
    let text = i18n::rule_description(rule);
    if text.is_empty() {
        rule.pattern().to_string()
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gpui_kit::{App, AppContext as _, TestAppContext};

    use super::grid_rows;
    use crate::app_state::AppState;
    use crate::workspace::Workspace;

    fn wait(cx: &mut TestAppContext, what: &str, mut done: impl FnMut(&mut App) -> bool) {
        for _ in 0..400 {
            cx.run_until_parked();
            if cx.update(&mut done) {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("等待超时：{what}");
    }

    // 同步期间切过 Tab：退出后显示与否以 Dock 当时的前台为准，不按开启时的样子
    #[gpui_kit::test]
    fn exiting_follows_the_tab_the_dock_shows(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        cx.update(crate::init);
        cx.update(AppState::install_in_memory);
        let (handle, workspace) = cx.update(|cx| {
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| Workspace::new(window, cx))
            })
            .unwrap()
        });
        for _ in 0..2 {
            cx.update_window(handle, |_, window, cx| {
                workspace.update(cx, |w, cx| w.open_local_terminal(window, cx))
            })
            .unwrap();
        }
        wait(cx, "两个终端都启动", |cx| {
            let terminals = workspace.read(cx).live_terminals();
            terminals.len() == 2 && terminals.iter().all(|p| p.read(cx).is_live(cx))
        });
        let terminals = cx.update(|cx| workspace.read(cx).live_terminals());
        let (first, second) = (terminals[0].clone(), terminals[1].clone());
        cx.update(|cx| {
            assert!(!first.read(cx).is_shown() && second.read(cx).is_shown());
        });
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| {
                w.toggle_multi_exec(window, cx);
                assert!(w.multi_exec.is_some());
                // 从侧边栏、通知切到第一个终端
                w.show_terminal(&first, window, cx);
            })
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(
                first.read(cx).is_shown() && second.read(cx).is_shown(),
                "网格中都显示"
            );
        });
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| w.toggle_multi_exec(window, cx))
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(first.read(cx).is_shown(), "Dock 前台是第一个");
            assert!(!second.read(cx).is_shown());
        });
    }

    #[test]
    fn grid_rows_spread_panes_evenly() {
        assert!(grid_rows(0).is_empty());
        assert_eq!(grid_rows(1), [1]);
        assert_eq!(grid_rows(2), [2]);
        assert_eq!(grid_rows(3), [2, 1]);
        assert_eq!(grid_rows(4), [2, 2]);
        assert_eq!(grid_rows(5), [3, 2]);
        assert_eq!(grid_rows(7), [3, 2, 2]);
        assert_eq!(grid_rows(10), [4, 3, 3]);
    }

    // 右键 Tab 打开它的菜单（Tab 栏上没有“…”按钮）
    #[gpui_kit::test]
    fn right_clicking_a_tab_opens_its_menu(cx: &mut TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        cx.update(crate::init);
        cx.update(AppState::install_in_memory);
        let (handle, workspace) = cx.update(|cx| {
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| Workspace::new(window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| w.open_local_terminal(window, cx))
        })
        .unwrap();
        wait(cx, "终端启动", |cx| {
            workspace.read(cx).live_terminals().len() == 1
        });
        let id =
            cx.update(|cx| workspace.read(cx).live_terminals()[0].entity_id().as_u64() as usize);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("popup-menu").is_none());
            window.right_click(("tab-title", id), cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("popup-menu").is_some(), "右键 Tab 弹出菜单");
        })
        .unwrap();
    }

    // SSH 主机的 Tab 菜单中有“复制 IP”（主机连不上也一样）；本地终端的没有
    #[gpui_kit::test]
    fn host_tab_menu_copies_the_address(cx: &mut TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        use termlet_core::model::Host;
        use termlet_core::store::Position;
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        cx.update(crate::init);
        cx.update(AppState::install_in_memory);
        let host = cx.update(|cx| {
            AppState::store(cx)
                .create(
                    Host {
                        name: "web".into(),
                        address: "127.0.0.1".into(),
                        port: 1,
                        username: "alice".into(),
                        ..Default::default()
                    },
                    None,
                    Position::Last,
                )
                .unwrap()
        });
        let (handle, workspace) = cx.update(|cx| {
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| Workspace::new(window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| {
                w.open_host(&host, window, cx);
                w.open_local_terminal(window, cx);
            })
        })
        .unwrap();
        wait(cx, "两个 Tab", |cx| {
            workspace.read(cx).live_terminals().len() == 2
        });
        let labels = |cx: &mut TestAppContext, ix: usize| {
            let id = cx
                .update(|cx| workspace.read(cx).live_terminals()[ix].entity_id().as_u64() as usize);
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.right_click(("tab-title", id), cx);
            })
            .unwrap();
            cx.run_until_parked();
            let labels: Vec<String> = cx
                .update_window(handle, |_, window, cx| {
                    window.render_frame(cx);
                    let menu = window.within("popup-menu");
                    (0..30usize)
                        .map_while(|i| {
                            menu.try_find(i)
                                .map(|item| item.label().map(str::to_string))
                        })
                        .flatten()
                        .collect()
                })
                .unwrap();
            cx.update_window(handle, |_, window, cx| window.press("escape", cx))
                .unwrap();
            cx.run_until_parked();
            labels
        };
        let copy_ip = crate::t!("hosts.menu.copy_ip").to_string();
        let (ssh, local): (Vec<_>, Vec<_>) = cx.update(|cx| {
            workspace
                .read(cx)
                .live_terminals()
                .iter()
                .enumerate()
                .map(|(ix, t)| (ix, t.read(cx).is_ssh()))
                .partition(|(_, ssh)| *ssh)
        });
        assert!(
            labels(cx, ssh[0].0).contains(&copy_ip),
            "SSH 的 Tab 有复制 IP"
        );
        assert!(!labels(cx, local[0].0).contains(&copy_ip), "本地终端没有");
    }

    // Tab 栏是自己画的（见 dock_skin），点 Tab 仍能切换过去
    #[gpui_kit::test]
    fn clicking_a_tab_shows_its_terminal(cx: &mut TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        cx.update(crate::init);
        cx.update(AppState::install_in_memory);
        let (handle, workspace) = cx.update(|cx| {
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| Workspace::new(window, cx))
            })
            .unwrap()
        });
        for _ in 0..2 {
            cx.update_window(handle, |_, window, cx| {
                workspace.update(cx, |w, cx| w.open_local_terminal(window, cx))
            })
            .unwrap();
        }
        wait(cx, "两个终端启动", |cx| {
            workspace.read(cx).live_terminals().len() == 2
        });
        let first = cx.update(|cx| workspace.read(cx).live_terminals()[0].clone());
        let id = first.entity_id().as_u64() as usize;
        let focused = |cx: &mut TestAppContext| {
            cx.update_window(handle, |_, window, cx| {
                workspace.read(cx).focused_terminal(window, cx)
            })
            .unwrap()
        };
        cx.run_until_parked();
        assert_ne!(focused(cx), Some(first.clone()), "新开的 Tab 在前面");
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click(("tab-title", id), cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(focused(cx), Some(first), "点 Tab 切换到它");
    }

    // 关闭窗口：有运行中的本地终端时先确认，说明的是会结束其中的程序；关掉设置后不再问
    #[gpui_kit::test]
    fn closing_a_window_with_a_local_terminal_asks_first(cx: &mut TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        cx.update(crate::init);
        cx.update(AppState::install_in_memory);
        let (handle, workspace) = cx.update(|cx| {
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| Workspace::new(window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| w.open_local_terminal(window, cx))
        })
        .unwrap();
        wait(cx, "终端启动", |cx| {
            workspace.read(cx).live_terminals().len() == 1
        });
        let close = cx
            .update_window(handle, |_, window, cx| {
                workspace.update(cx, |w, cx| w.should_close(window, cx))
            })
            .unwrap();
        assert!(!close, "先确认");
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("close-confirm").is_some());
            window.click("close-cancel", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            AppState::store(cx)
                .update_settings(|s| s.general.confirm_close = false)
                .unwrap()
        });
        let close = cx
            .update_window(handle, |_, window, cx| {
                workspace.update(cx, |w, cx| w.should_close(window, cx))
            })
            .unwrap();
        assert!(close, "关掉设置后直接关闭");
    }

    // 默认的快捷键：Ctrl+Tab / Ctrl+Shift+Tab 循环切换，Ctrl+Alt+数字切到第几个，没有那么多个时不动
    #[gpui_kit::test]
    fn tab_shortcuts_switch_within_the_group(cx: &mut TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        cx.update(crate::init);
        cx.update(AppState::install_in_memory);
        let (handle, workspace) = cx.update(|cx| {
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| Workspace::new(window, cx))
            })
            .unwrap()
        });
        for _ in 0..3 {
            cx.update_window(handle, |_, window, cx| {
                workspace.update(cx, |w, cx| w.open_local_terminal(window, cx))
            })
            .unwrap();
        }
        wait(cx, "三个终端启动", |cx| {
            workspace.read(cx).live_terminals().len() == 3
        });
        let terminals = cx.update(|cx| workspace.read(cx).live_terminals());
        let focused = |cx: &mut TestAppContext| {
            cx.run_until_parked();
            cx.update_window(handle, |_, window, cx| {
                let focused = workspace.read(cx).focused_terminal(window, cx);
                terminals.iter().position(|t| Some(t) == focused.as_ref())
            })
            .unwrap()
        };
        let press = |cx: &mut TestAppContext, key: &str| {
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.press(key, cx);
            })
            .unwrap();
        };
        assert_eq!(focused(cx), Some(2), "最后打开的在前面");
        press(cx, "ctrl-tab");
        assert_eq!(focused(cx), Some(0), "最后一个之后回到第一个");
        press(cx, "ctrl-alt-2");
        assert_eq!(focused(cx), Some(1));
        press(cx, "ctrl-shift-tab");
        assert_eq!(focused(cx), Some(0));
        press(cx, "ctrl-shift-tab");
        assert_eq!(focused(cx), Some(2), "第一个之前是最后一个");
        press(cx, "ctrl-alt-9");
        assert_eq!(focused(cx), Some(2), "只有三个 Tab 时第九个不动");
    }
}
