//! 快捷命令分组的菜单（分组标题的右键菜单与悬停时的 ⋯）：在这个分组中新建、重命名分组、删除分组。
//!
//! 分组只是快捷命令上的名称，改名、删除由核心逐条改写其中的快捷命令（`Store::rename_snippet_group`、
//! `Store::delete_snippet_group`）。改成已有分组的名称时并进去并提示；删除时选择把其中的快捷命令移到未分组
//! （默认）或一并删除，确认框写明条数。本机的折叠状态与快捷命令栏选中的分组跟着改名。

use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::{
    App, AppContext as _, ClickEvent, Context, IntoElement, Render, SharedString, Styled as _,
    Task, Window,
};
use termlet_core::model::Snippet;

use super::editor;
use crate::app_state::AppState;
use crate::name_dialog::{self, NameDialog};
use crate::notify::{self, Notice};
use crate::{store_errors, t};

/// 本机界面状态中分组折叠的键（与快捷命令页相同）。
pub(super) fn collapsed_key(group: &str) -> String {
    format!("snippet-group:{group}")
}

/// 分组 `group`（有名称的）的菜单。
pub(super) fn group_menu(
    group: String,
) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
    move |menu, _, _| {
        let new_in = {
            let group = group.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                editor::open_new(group.clone(), String::new(), window, cx)
            }
        };
        let rename = {
            let group = group.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                open_rename(group.clone(), window, cx)
            }
        };
        let delete = {
            let group = group.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                confirm_delete(group.clone(), window, cx)
            }
        };
        menu.item(PopupMenuItem::new(t!("snippets.group.new_in")).on_click(new_in))
            .separator()
            .item(PopupMenuItem::new(t!("snippets.group.rename")).on_click(rename))
            .item(PopupMenuItem::new(t!("snippets.group.delete")).on_click(delete))
    }
}

/// 重命名分组。
pub(super) fn open_rename(group: String, window: &mut Window, cx: &mut App) {
    let from = group.clone();
    let submit: name_dialog::Submit = Box::new(move |to, window, cx| {
        let store = AppState::store(cx);
        match store.rename_snippet_group(&from, &to) {
            Ok(outcome) => {
                follow_rename(&from, &to, outcome.merged, cx);
                if outcome.merged {
                    notify::show(
                        Notice::success(t!("snippets.group.merged", name = to.clone())),
                        window,
                        cx,
                    );
                }
                Task::ready(Ok(()))
            }
            Err(e) => {
                tracing::warn!("重命名快捷命令分组失败：{e}");
                Task::ready(Err(store_errors::describe(&e)))
            }
        }
    });
    name_dialog::open(
        NameDialog {
            title: t!("snippets.group.rename_title"),
            value: group,
            placeholder: SharedString::default(),
            select_all: true,
            ok_label: t!("common.save"),
            input_id: "snippet-group-rename",
        },
        submit,
        window,
        cx,
    );
}

/// 改名后，本机的折叠状态与快捷命令栏选中的分组跟着改（并进已有分组时那一组的折叠状态不变）。
fn follow_rename(from: &str, to: &str, merged: bool, cx: &mut App) {
    let (old_key, new_key) = (collapsed_key(from), collapsed_key(to));
    AppState::ui(cx).update(cx, |ui, cx| {
        ui.update(
            |state| {
                if state.collapsed_groups.remove(&old_key) && !merged {
                    state.collapsed_groups.insert(new_key);
                }
                if state.snippet_bar_group.as_deref() == Some(from) {
                    state.snippet_bar_group = Some(to.to_string());
                }
            },
            cx,
        )
    });
}

/// 确认后删除分组：选择把其中的快捷命令移到未分组（默认）或一并删除。
pub(super) fn confirm_delete(group: String, window: &mut Window, cx: &mut App) {
    let store = AppState::store(cx);
    let count = store
        .list::<Snippet>()
        .iter()
        .filter(|s| s.doc.group == group)
        .count();
    if count == 0 {
        return;
    }
    let choice = cx.new(|_| DeleteChoice { keep: true, count });
    crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
        let (store, group, choice) = (store.clone(), group.clone(), choice.clone());
        alert
            .confirm()
            .title(t!("snippets.group.delete_title", name = group.clone()))
            .description(choice.clone())
            .ok_text(t!("common.delete"))
            .ok_variant(ButtonVariant::Danger)
            .cancel_text(t!("common.cancel"))
            .on_ok(move |_, window, cx| {
                let keep = choice.read(cx).keep;
                match store.delete_snippet_group(&group, keep) {
                    // 确认期间已被同步删掉或改名：目的已经达到
                    Ok(_) | Err(termlet_core::CoreError::NotFound(_)) => {}
                    Err(e) => notify::error(store_errors::describe(&e), window, cx),
                }
                true
            })
    });
}

/// 删除分组时怎样处理其中的快捷命令。
struct DeleteChoice {
    /// 移到未分组（否则一并删除）
    keep: bool,
    count: usize,
}

impl Render for DeleteChoice {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 与标题隔开一点
        crate::radio::vertical("snippet-group-delete")
            .pt_2()
            .children([
                t!("snippets.group.delete_keep", count = self.count),
                t!("snippets.group.delete_all", count = self.count),
            ])
            .selected_index(Some(if self.keep { 0 } else { 1 }))
            .on_click(cx.listener(|this, ix: &usize, _, cx| {
                this.keep = *ix == 0;
                cx.notify();
            }))
    }
}
