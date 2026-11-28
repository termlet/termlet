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

use gpui_kit::component::input::InputState;
use gpui_kit::{App, Context, Entity, SharedString, Window, point, px};

use crate::name_dialog::{self, NameDialog};
use crate::t;

pub fn init(cx: &mut App) {
    file_list::init(cx);
    open_in_editor::init(cx);
}

/// 路径框显示 `text`，并滚到末尾：路径长时露出的是当前目录名，而不是开头的盘符、`/home`。gpui-kit 的单行输入框
/// 设值后滚回开头（与网页的输入框相同），这里设值后再滚到末尾。
pub(crate) fn show_path(
    path: &Entity<InputState>,
    text: String,
    window: &mut Window,
    cx: &mut App,
) {
    path.update(cx, |input, cx| {
        input.set_value(text, window, cx);
        scroll_path_to_end(input, cx);
    });
}

/// 路径框滚到末尾（也在离开路径框时：点进去后光标停在哪里，视野就停在哪里）。光标一并移到末尾，否则排版时视野
/// 又跟着光标回去。
pub(crate) fn scroll_path_to_end(input: &mut InputState, cx: &mut Context<InputState>) {
    let end = input.value().len();
    input.set_selected_range(end..end, cx);
    // 偏移在下一次排版时按文字的宽度收住：给一个足够远的值就是最右
    input.set_scroll_offset(point(px(-1_000_000.), px(0.)), cx);
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
