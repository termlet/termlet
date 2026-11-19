//! 某台主机的命令历史（主机右键菜单“命令历史…”）：搜索、写入或执行到终端、存为快捷命令、删除、清空。
//!
//! 单击写入当前终端，双击执行；当前终端连的不是这台主机时只能查看、删除、存为快捷命令。
//! 平时在终端中用 Ctrl+Shift+Space 的快捷命令选择器使用历史。

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, VirtualListScrollHandle, WindowExt as _, h_flex, v_flex,
    v_virtual_list,
};
use gpui_kit::{
    App, AppContext as _, ClickEvent, Context, ElementId, Entity, Focusable as _,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, TestSupportExt as _,
    WeakEntity, Window, div, prelude::FluentBuilder as _, px, rems, size,
};
use termlet_core::history::{self, HistoryEntry};
use termlet_core::id::HostId;
use termlet_core::model::Host;

use super::{command_input, editor};
use crate::app_state::AppState;
use crate::dialog_token::{DialogToken, close_own};
use crate::notify::{self, Notice, NoticeLine};
use crate::prose::prose;
use crate::terminal_panel::TerminalPanel;
use crate::{store_errors, t};

/// 打开 `host` 的命令历史；`target` 是当前终端（连的是这台主机时才能写入）。
pub fn open(
    host: HostId,
    target: Option<WeakEntity<TerminalPanel>>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(record) = AppState::store(cx).get::<Host>(&host) else {
        return;
    };
    let name = record.doc.name;
    let dialog_title = t!("history.title", host = name.clone());
    let view = cx.new(|cx| HistoryDialog::new(host, name, target, window, cx));
    let first = Rc::new(Cell::new(true));
    let width = rems(36.).to_pixels(window.rem_size());
    let owner = view.clone();
    window.open_dialog(cx, move |dialog, window, cx| {
        if first.replace(false) {
            view.read(cx).query.focus_handle(cx).focus(window, cx);
        }
        // 搜索框中按 Enter 不关闭
        dialog
            .title(dialog_title.clone())
            .w(width)
            .child(view.clone())
            .on_ok(|_, _, _| false)
    });
    let token = DialogToken::take(window, cx);
    let notice = owner.update(cx, |view, _| {
        view.dialog = token;
        view.notice.clone()
    });
    notify::attach(&notice, window, cx);
}

struct HistoryDialog {
    owner: String,
    host_name: SharedString,
    target: Option<WeakEntity<TerminalPanel>>,
    query: Entity<InputState>,
    entries: Vec<HistoryEntry>,
    /// 符合搜索的条目在 `entries` 中的序号（搜索或历史变化时重算，不在每帧过滤）
    shown: Vec<usize>,
    scroll: VirtualListScrollHandle,
    /// 单击后稍等再写入：期间又点了一下就是双击（执行）
    pending_click: Option<Task<()>>,
    /// 所在的对话框（写入后只关它）
    dialog: Option<DialogToken>,
    /// 写入失败、删除失败等的说明（弹出的通知会被对话框盖住，见 [`crate::notify::show`]）
    notice: Entity<NoticeLine>,
    _subscription: Subscription,
}

/// 区分单击与双击的等待时间。
const CLICK_DELAY: Duration = Duration::from_millis(250);
/// 每行的高度与列表最多显示的高度（rem）。
const ROW_HEIGHT: f32 = 1.75;
const LIST_MAX_HEIGHT: f32 = 24.;

impl HistoryDialog {
    fn new(
        host: HostId,
        host_name: String,
        target: Option<WeakEntity<TerminalPanel>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder(t!("history.search")));
        let subscription = cx.subscribe(&query, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.refilter(cx);
            }
        });
        // 只有当前终端连的就是这台主机时才写入
        let owner = host.as_str().to_string();
        let target = target.filter(|t| {
            t.upgrade()
                .is_some_and(|p| p.read(cx).history_owner() == owner)
        });
        let mut this = Self {
            owner,
            host_name: host_name.into(),
            target,
            query,
            entries: Vec::new(),
            shown: Vec::new(),
            scroll: VirtualListScrollHandle::new(),
            pending_click: None,
            dialog: None,
            notice: NoticeLine::new(cx),
            _subscription: subscription,
        };
        this.reload(cx);
        this
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.entries = history::list(&AppState::store(cx), &self.owner).unwrap_or_default();
        self.refilter(cx);
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        let query = self.query.read(cx).value().trim().to_lowercase();
        self.shown = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| query.is_empty() || e.command.to_lowercase().contains(&query))
            .map(|(ix, _)| ix)
            .collect();
        cx.notify();
    }

    fn send(&mut self, command: &str, run: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.target.as_ref().and_then(|t| t.upgrade()) else {
            return;
        };
        match panel.update(cx, |panel, cx| {
            panel.write_command(command_input(command, Some(run)), cx)
        }) {
            Ok(()) => {
                tracing::info!(session = %panel.read(cx).name(), run, "命令历史：写入终端");
                close_own(self.dialog.as_ref(), window, cx);
                panel.read(cx).focus_handle(cx).focus(window, cx);
            }
            Err(message) => notify::show(Notice::warning(message), window, cx),
        }
    }

    fn delete(&mut self, command: &str, window: &mut Window, cx: &mut Context<Self>) {
        // 刚单击过的那一条还没写入：删除后不再写入
        self.pending_click = None;
        tracing::info!(owner = %self.owner, "删除一条命令历史");
        if let Err(e) = history::delete(&AppState::store(cx), &self.owner, command) {
            notify::show(Notice::error(store_errors::describe(&e)), window, cx);
        }
        self.reload(cx);
    }

    fn confirm_clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_click = None;
        let this = cx.entity().downgrade();
        let owner = self.owner.clone();
        let name = self.host_name.clone();
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let this = this.clone();
            let owner = owner.clone();
            alert
                .confirm()
                .title(t!("history.clear_confirm", host = name.clone()))
                .ok_text(t!("settings.clear_history_ok"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, window, cx| {
                    tracing::info!(owner = %owner, "清空这台主机的命令历史");
                    if let Err(e) = history::clear(&AppState::store(cx), Some(&owner)) {
                        notify::show(Notice::error(store_errors::describe(&e)), window, cx);
                    }
                    let _ = this.update(cx, |this, cx| this.reload(cx));
                    true
                })
        });
    }

    fn render_entry(
        &self,
        entry: &HistoryEntry,
        can_send: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let command = entry.command.clone();
        let id = |name: &str| ElementId::from(SharedString::from(format!("{name}-{}", entry.id)));
        let (send_command, save_command, delete_command) =
            (command.clone(), command.clone(), command.clone());
        h_flex()
            .id(id("history-entry"))
            .test_support()
            .group("history-entry")
            .h(rems(ROW_HEIGHT))
            .gap_1()
            .px_2()
            .rounded(theme.radius)
            .hover(|s| s.bg(theme.list_hover))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .font_family(termlet_terminal::fonts::DEFAULT_FAMILY)
                    .child(command.clone()),
            )
            .child(
                h_flex()
                    .invisible()
                    .group_hover("history-entry", |s| s.visible())
                    .child(
                        Button::new(id("history-save"))
                            .ghost()
                            .small()
                            .icon(IconName::Zap)
                            .tooltip(t!("history.save_as_snippet"))
                            .on_click(move |_, window, cx| {
                                cx.stop_propagation();
                                editor::open_new(
                                    String::new(),
                                    format!("{save_command}\n"),
                                    window,
                                    cx,
                                );
                            }),
                    )
                    .child(
                        Button::new(id("history-delete"))
                            .ghost()
                            .small()
                            .icon(IconName::Trash)
                            .tooltip(t!("history.delete"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.delete(&delete_command, window, cx);
                            })),
                    ),
            )
            .when(can_send, |this| {
                this.on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    // 单击写入、双击执行
                    if event.click_count() >= 2 {
                        this.pending_click = None;
                        this.send(&send_command, true, window, cx);
                        return;
                    }
                    let command = send_command.clone();
                    this.pending_click = Some(cx.spawn_in(window, async move |this, cx| {
                        cx.background_executor().timer(CLICK_DELAY).await;
                        let _ = this.update_in(cx, |this, window, cx| {
                            this.pending_click = None;
                            this.send(&command, false, window, cx);
                        });
                    }));
                }))
            })
    }
}

impl Render for HistoryDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let can_send = self
            .target
            .as_ref()
            .and_then(|t| t.upgrade())
            .is_some_and(|p| p.read(cx).can_write(cx));
        let enabled = AppState::store(cx).settings().terminal.command_history;
        let hint = if !enabled {
            Some(t!("history.disabled"))
        } else if !can_send {
            Some(t!("history.not_this_host"))
        } else {
            None
        };
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.query).id("history-search").small()),
                    )
                    .child(
                        Button::new("history-clear")
                            .ghost()
                            .small()
                            .icon(IconName::Trash)
                            .tooltip(t!("history.clear"))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.confirm_clear(window, cx)),
                            ),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(hint.unwrap_or_else(|| t!("history.hint"))),
            )
            .child(if self.shown.is_empty() {
                div()
                    .py_6()
                    .text_center()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(prose(t!("history.empty")))
                    .into_any_element()
            } else {
                // 最多 1000 条：只渲染看得见的行
                let row_height = rems(ROW_HEIGHT).to_pixels(window.rem_size());
                let height = (row_height * self.shown.len() as f32)
                    .min(rems(LIST_MAX_HEIGHT).to_pixels(window.rem_size()));
                let sizes = Rc::new(vec![size(px(0.), row_height); self.shown.len()]);
                div()
                    .id("history-list")
                    .h(height)
                    .child(
                        v_virtual_list(
                            cx.entity(),
                            "history-rows",
                            sizes,
                            move |this, range, _, cx| {
                                range
                                    .filter_map(|ix| {
                                        this.shown.get(ix).and_then(|e| this.entries.get(*e))
                                    })
                                    .map(|e| this.render_entry(e, can_send, cx).into_any_element())
                                    .collect::<Vec<_>>()
                            },
                        )
                        .track_scroll(&self.scroll)
                        .size_full(),
                    )
                    .into_any_element()
            })
            .when(self.notice.read(cx).is_shown(), |this| {
                this.child(div().text_xs().child(self.notice.clone()))
            })
    }
}
