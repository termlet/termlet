//! 快捷命令：侧边栏的“快捷命令”页（[`list`]，拖动排序见 [`drag`]、分组的菜单见 [`groups`]）、编辑对话框（[`editor`]）、
//! 在终端中按 Ctrl+Shift+Space 打开的选择器（[`picker`]，也列出这台主机的命令历史），以及写入终端的方式。
//!
//! 快捷命令内容末尾带换行时发送后立即执行；`{{名称}}`、`{{名称:默认值}}` 是发送前填写的参数
//! （见 [`termlet_core::snippet_params`]）。

pub(crate) mod bar;
mod drag;
pub(crate) mod editor;
mod groups;
pub(crate) mod history;
pub(crate) mod list;
#[cfg(test)]
mod list_tests;
pub(crate) mod picker;

use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::{App, Window};
use termlet_core::id::SnippetId;
use termlet_core::model::Snippet;

use crate::app_state::AppState;
use crate::{store_errors, t};

/// 写入终端的方式。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandInput {
    /// 当作按键发送（执行时各行以回车结束）
    Type(String),
    /// 按终端的括号粘贴写入（多行只写入不执行）
    Paste(String),
}

/// 命令写入终端的内容。`run` 为 `None` 时按末尾有没有换行判断（快捷命令保存时的“执行”设置）：
/// 执行时各行都以回车结束，与按回车相同（换行在 Windows 的本地 shell 中不一定执行）。
/// 只写入时单行原样写入；多行要用括号粘贴（shell 只把它放进输入行），否则其中的换行会让前几行立即执行。
pub fn command_input(text: &str, run: Option<bool>) -> CommandInput {
    let run = run.unwrap_or_else(|| text.ends_with(['\r', '\n']));
    let body = text.trim_end_matches(['\r', '\n']);
    if run {
        let lines = body.replace("\r\n", "\r").replace('\n', "\r");
        return CommandInput::Type(format!("{lines}\r"));
    }
    if body.contains(['\r', '\n']) {
        CommandInput::Paste(body.to_string())
    } else {
        CommandInput::Type(body.to_string())
    }
}

/// 确认后删除快捷命令。
pub fn confirm_delete(id: SnippetId, window: &mut Window, cx: &mut App) {
    let store = AppState::store(cx);
    let Some(record) = store.get::<Snippet>(&id) else {
        return;
    };
    let name = record.doc.name;
    crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
        let store = store.clone();
        let id = id.clone();
        alert
            .confirm()
            .title(t!("snippets.delete_title", name = name.clone()))
            .ok_text(t!("common.delete"))
            .ok_variant(ButtonVariant::Danger)
            .cancel_text(t!("common.cancel"))
            .on_ok(move |_, window, cx| {
                tracing::info!(id = %id, "删除快捷命令");
                match store.delete::<Snippet>(&id) {
                    // 确认期间已被同步或命令行删掉：目的已经达到
                    Ok(_) | Err(termlet_core::CoreError::NotFound(_)) => {}
                    Err(e) => crate::notify::error(store_errors::describe(&e), window, cx),
                }
                true
            })
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_ends_every_line_with_enter() {
        assert_eq!(
            command_input("ls -la\n", None),
            CommandInput::Type("ls -la\r".into())
        );
        assert_eq!(
            command_input("cd /tmp\r\nls\n", None),
            CommandInput::Type("cd /tmp\rls\r".into())
        );
        assert_eq!(
            command_input("uptime", Some(true)),
            CommandInput::Type("uptime\r".into())
        );
    }

    #[test]
    fn insert_keeps_single_line_and_pastes_multiple() {
        assert_eq!(
            command_input("df -h", None),
            CommandInput::Type("df -h".into())
        );
        assert_eq!(
            command_input("df -h\n", Some(false)),
            CommandInput::Type("df -h".into())
        );
        assert_eq!(
            command_input("a\nb\n", Some(false)),
            CommandInput::Paste("a\nb".into())
        );
    }
}
