//! SFTP 文件管理：双栏（本机、远程）、传输列表、修改权限、在线编辑的入口；SSH 主机可以经 sudo 以 root 身份打开。

pub(crate) mod batch_notice;
pub(crate) mod errors;
pub(crate) mod file_icons;
mod file_icons_table;
pub(crate) mod file_list;
pub(crate) mod list_model;
pub(crate) mod local_pane;
pub(crate) mod open_in_editor;
pub(crate) mod panel;
pub(crate) mod perm;
pub(crate) mod perm_dialog;
pub(crate) mod runnable;
mod sudo_dialog;
#[cfg(test)]
mod tests;
pub(crate) mod transfers;

use gpui_kit::{App, SharedString, Window};

use crate::name_dialog::{self, NameDialog};
use crate::t;

pub fn init(cx: &mut App) {
    file_list::init(cx);
    open_in_editor::init(cx);
}

/// 输入名称（新建文件夹、重命名）：有原名时打开后全选。
pub(crate) fn prompt_name(
    title: SharedString,
    value: String,
    input_id: &'static str,
    submit: name_dialog::Submit,
    window: &mut Window,
    cx: &mut App,
) {
    name_dialog::open(
        NameDialog {
            title,
            select_all: !value.is_empty(),
            value,
            placeholder: SharedString::default(),
            ok_label: t!("common.ok"),
            input_id,
        },
        submit,
        window,
        cx,
    );
}

/// 显示用的名称列表（确认删除时）：用“、”连接，太多时只列前几个。
pub(crate) fn join_names(names: &[String]) -> String {
    const SHOWN: usize = 5;
    let mut shown: Vec<String> = names
        .iter()
        .take(SHOWN)
        .map(|n| list_model::visible_name(n))
        .collect();
    if names.len() > SHOWN {
        shown.push(t!("sftp.names_more", count = names.len() - SHOWN).to_string());
    }
    shown.join(&t!("sftp.name_separator"))
}
