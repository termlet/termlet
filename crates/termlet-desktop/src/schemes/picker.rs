//! 命令面板的“切换终端配色…”：搜索配色，上下选择时当前终端实时预览，Enter 确定，Esc 还原
//! （与 Windows Terminal 命令面板的配色切换相同）。
//!
//! 确定时勾选了“只用于这个 Tab”就只换这个 Tab 的配色；否则改设置中界面当前深浅对应的槽，并去掉这个 Tab 的
//! 临时配色。没有打开的终端时也能用（只改设置，没有预览）。选择器关闭（Esc、点击外部、确定）时都结束预览。

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::base::actions::Cancel;
use gpui_kit::component::IndexPath;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::command::{Command, CommandItem, CommandState};
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex};
use gpui_kit::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    TestSupportExt as _, WeakEntity, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::schemes::SchemeOrigin;

use super::{SchemeEntry, SchemeSwatch, builtin_entries, custom_entries, effective_scheme};
use crate::app_state::AppState;
use crate::prose::prose;
use crate::t;
use crate::terminal_colors::SchemeLevels;
use crate::terminal_panel::TerminalPanel;

/// 打开配色选择器；`panel` 是打开命令面板时焦点所在的终端（在它上面预览）。
pub fn open(panel: Option<WeakEntity<TerminalPanel>>, window: &mut Window, cx: &mut App) {
    let picker = cx.new(|cx| SchemePicker::new(panel, window, cx));
    let first_focus = Rc::new(Cell::new(true));
    let width = rems(36.).to_pixels(window.rem_size());
    window.open_dialog(cx, move |dialog, window, cx| {
        if first_focus.replace(false) {
            let state = picker.read(cx).state.clone();
            state.update(cx, |state, cx| state.focus(window, cx));
        }
        let cancel = picker.downgrade();
        dialog
            .p_0()
            .close_button(false)
            .w(width)
            // 点击外部：还原（Esc 由选择器自己处理）
            .on_cancel(move |_, _, cx| {
                let _ = cancel.update(cx, |picker, cx| picker.end_preview(cx));
                true
            })
            .child(picker.clone())
    });
}

struct SchemePicker {
    state: Entity<CommandState>,
    /// 全部配色：内置的在前，自定义的在后
    entries: Vec<SchemeEntry>,
    panel: Option<WeakEntity<TerminalPanel>>,
    /// 打开时终端（没有终端时为全局设置）实际用的配色，列表中打勾
    current: String,
    origin: SchemeOrigin,
    /// 选中的配色（`entries` 中的位置），终端上预览它；打开时是当前的配色（不用预览）
    highlighted: Option<usize>,
    tab_only: bool,
    /// 第一次渲染后把选择移到当前的配色
    select_current: bool,
}

impl SchemePicker {
    fn new(
        panel: Option<WeakEntity<TerminalPanel>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = AppState::store(cx);
        let custom = custom_entries(&store);
        let mut entries = builtin_entries();
        entries.extend(custom.iter().cloned());
        let live = panel.as_ref().and_then(|p| p.upgrade());
        let levels = match &live {
            Some(panel) => {
                let panel = panel.read(cx);
                let mut levels = panel
                    .host()
                    .map(|host| SchemeLevels::for_host(&store, host))
                    .unwrap_or_default();
                levels.tab = panel.tab_scheme().to_string();
                levels
            }
            None => SchemeLevels::default(),
        };
        let (current, origin) = effective_scheme(
            &levels,
            &store.settings().terminal,
            cx.theme().is_dark(),
            &custom,
        );
        // 选择器关闭后（不论怎样关闭）终端不再预览
        cx.on_release(|picker: &mut Self, cx| picker.end_preview(cx))
            .detach();
        let highlighted = entries.iter().position(|e| e.id == current);
        Self {
            state: cx.new(|cx| CommandState::new(window, cx)),
            entries,
            panel: live.map(|p| p.downgrade()),
            current,
            origin,
            highlighted,
            tab_only: false,
            select_current: true,
        }
    }

    fn panel(&self) -> Option<Entity<TerminalPanel>> {
        self.panel.as_ref().and_then(|p| p.upgrade())
    }

    /// 选择移到了第 `row` 套：在终端上预览。
    fn preview(&mut self, row: usize, cx: &mut Context<Self>) {
        if self.highlighted == Some(row) {
            return;
        }
        let Some(entry) = self.entries.get(row) else {
            return;
        };
        self.highlighted = Some(row);
        if let Some(panel) = self.panel() {
            let id = entry.id.clone();
            panel.update(cx, |panel, cx| panel.set_scheme_preview(id, cx));
        }
        cx.notify();
    }

    /// 结束预览，终端回到原来的配色（确定后则是新的配色）。
    fn end_preview(&mut self, cx: &mut App) {
        if let Some(panel) = self.panel() {
            panel.update(cx, |panel, cx| panel.set_scheme_preview(String::new(), cx));
        }
    }

    /// 确定第 `row` 套。
    fn confirm(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(row) else {
            return;
        };
        let id = entry.id.clone();
        let panel = self.panel();
        match (&panel, self.tab_only) {
            (Some(panel), true) => panel.update(cx, |panel, cx| panel.set_tab_scheme(id, cx)),
            _ => {
                // 先改设置再去掉 Tab 的临时配色与预览：终端直接换成新的配色，不会闪一下旧的
                let dark = cx.theme().is_dark();
                let result = AppState::store(cx).update_settings(|s| {
                    if dark {
                        s.terminal.scheme_dark = id;
                    } else {
                        s.terminal.scheme_light = id;
                    }
                });
                if let Err(e) = result {
                    tracing::error!("保存终端配色失败：{e}");
                    crate::notify::error(
                        t!("schemes.save_failed", error = e.to_string()),
                        window,
                        cx,
                    );
                }
                if let Some(panel) = &panel
                    && !panel.read(cx).tab_scheme().is_empty()
                {
                    panel.update(cx, |panel, cx| panel.set_tab_scheme(String::new(), cx));
                }
            }
        }
        self.end_preview(cx);
        window.close_dialog(cx);
    }

    /// 底部的说明：勾选“只用于这个 Tab”时说明不改设置；终端的配色来自主机、分组时说明改全局配色对它无效。
    fn hint(&self) -> SharedString {
        if self.tab_only {
            return t!("schemes.picker.hint_tab");
        }
        match self.origin {
            SchemeOrigin::Host if self.panel.is_some() => t!("schemes.picker.note_host"),
            SchemeOrigin::Group if self.panel.is_some() => t!("schemes.picker.note_group"),
            _ => t!("schemes.picker.hint"),
        }
    }
}

impl Render for SchemePicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.select_current {
            // 列表要先渲染一次（Command 在渲染时装入条目）才能选中
            self.select_current = false;
            if let Some(row) = self.highlighted {
                cx.defer_in(window, move |this, window, cx| {
                    this.state.update(cx, |state, cx| {
                        // 这期间已经输入了搜索词：选择交给搜索结果
                        if state.query(cx).is_empty() {
                            state.set_selected_index(
                                Some(IndexPath::new(row).section(0)),
                                window,
                                cx,
                            )
                        }
                    });
                });
            }
        }
        let items = self.entries.iter().map(|entry| {
            let entry = entry.clone();
            let detail = entry.detail();
            let current = entry.id == self.current;
            CommandItem::new()
                .label(entry.scheme.name.clone())
                .keywords([detail.clone(), SharedString::from(entry.project())])
                .checked(current)
                .child(move |_, cx| {
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(SchemeSwatch::new(&entry.scheme))
                        .child(div().flex_none().text_sm().child(entry.scheme.name.clone()))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(prose(detail.clone())),
                        )
                })
        });
        let this = cx.entity().downgrade();
        let on_select = this.clone();
        let on_query = this.clone();
        let on_tab_only = this.clone();
        let has_terminal = self.panel().is_some();
        let tab_only = self.tab_only;
        let hint = self.hint();
        let command = Command::new(&self.state)
            .placeholder(t!("schemes.picker.placeholder"))
            .bordered(false)
            .max_h(crate::command_palette::list_max_height(window))
            .items(items)
            .empty(|_, _, cx| {
                div()
                    .p_4()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(t!("schemes.no_match"))
            })
            .footer(move |_, _, cx| {
                let on_tab_only = on_tab_only.clone();
                h_flex()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .when(has_terminal, |this| {
                        this.child(
                            Checkbox::new("scheme-picker-tab-only")
                                .label(t!("schemes.picker.tab_only"))
                                .checked(tab_only)
                                .on_change(move |checked: &bool, window, cx| {
                                    let checked = *checked;
                                    let _ = on_tab_only.update(cx, |picker, cx| {
                                        picker.tab_only = checked;
                                        // 焦点回到搜索框，仍可上下选择
                                        picker
                                            .state
                                            .update(cx, |state, cx| state.focus(window, cx));
                                        cx.notify();
                                    });
                                }),
                        )
                    })
                    .child(div().flex_1().min_w_0().child(prose(hint.clone())))
            })
            .on_select(move |ix, _, cx| {
                let _ = on_select.update(cx, |picker, cx| picker.preview(ix.row, cx));
            })
            // 搜索词变了：预览搜索结果中选中的一项。列表在搜索词的变更送达之前重新渲染时，选择已经移过去，
            // 不再报告 on_select
            .on_query(move |_, _, cx| {
                let _ = on_query.update(cx, |picker, cx| {
                    if let Some(ix) = picker.state.read(cx).selected_index() {
                        picker.preview(ix.row, cx);
                    }
                });
            })
            .on_confirm(move |ix, window, cx| {
                let _ = this.update(cx, |picker, cx| picker.confirm(ix.row, window, cx));
            });
        let highlighted = self
            .highlighted
            .and_then(|row| self.entries.get(row))
            .map(|e| e.scheme.name.clone())
            .unwrap_or_default();
        div()
            .id("scheme-picker")
            .test_support()
            // 读屏与 UI 测试：选中（正在预览）的配色
            .aria_value(highlighted)
            // Esc 总是还原并关闭（不像命令面板那样先清空搜索框），在搜索框处理之前拦下
            .capture_action(cx.listener(|this, _: &Cancel, window, cx| {
                this.end_preview(cx);
                window.close_dialog(cx);
                cx.stop_propagation();
            }))
            .child(command)
    }
}
