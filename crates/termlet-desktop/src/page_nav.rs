//! 对话框左侧的分页导航（设置、主机对话框）的键盘操作：gpui-kit 的侧边菜单项不能获得焦点，只用键盘切换不了分页。
//! 这里把整列包起来：可以用 Tab 到达（排在右侧内容之前），↑ ↓ 切到上一页 / 下一页，Home / End 到第一页 / 最后一页；
//! 有焦点时整列画焦点框。整列的宽度随最长的分页名放宽（[`column`]）。

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    App, Div, FocusHandle, InteractiveElement as _, KeyDownEvent, Stateful,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::FluentBuilder as _, rems,
};

/// 导航这一列（右边一条分隔线）：至少 10rem，分页名更长时（英文的比中文长）按最长的分页名放宽，不裁掉文字；
/// 右侧的内容让出这部分宽度。
pub fn column(cx: &App) -> Div {
    div()
        .flex_none()
        .min_w(rems(10.))
        .pr_3()
        .border_r_1()
        .border_color(cx.theme().border)
}

/// 按下 `key` 后的位置（`current` 为当前分页在 `len` 页中的位置）；不是导航用的键、或带了修饰键时为 `None`。
pub fn step(event: &KeyDownEvent, current: usize, len: usize) -> Option<usize> {
    let keystroke = &event.keystroke;
    let mods = &keystroke.modifiers;
    if len == 0 || mods.control || mods.alt || mods.shift || mods.platform {
        return None;
    }
    match keystroke.key.as_str() {
        "up" => Some(current.saturating_sub(1)),
        "down" => Some((current + 1).min(len - 1)),
        "home" => Some(0),
        "end" => Some(len - 1),
        _ => None,
    }
}

/// 包住导航的一列：跟踪焦点，有焦点时画焦点框（没有焦点时边框透明，切换时不挪动内容）。按键由调用方用
/// `on_key_down` 加上，在其中调用 [`step`]。比对话框矮的窗口里（界面放大后也是）分页放不下时可以上下滚动，不压到
/// 对话框底部的按钮上。
pub fn frame(id: &'static str, focus: &FocusHandle, window: &Window, cx: &App) -> Stateful<Div> {
    let focused = focus.is_focused(window);
    let theme = cx.theme();
    div()
        .id(id)
        .max_h_full()
        .overflow_y_scroll()
        .track_focus(focus)
        .rounded(theme.radius)
        .border_1()
        .border_color(gpui_kit::transparent_black())
        .when(focused, |this| this.border_color(theme.ring))
}

#[cfg(test)]
mod tests {
    use gpui_kit::{KeyDownEvent, Keystroke};

    use super::step;

    fn key(text: &'static str) -> KeyDownEvent {
        KeyDownEvent {
            keystroke: Keystroke::parse(text).unwrap(),
            is_held: false,
            prefer_character_input: false,
        }
    }

    #[test]
    fn arrows_home_and_end_move_between_pages() {
        assert_eq!(step(&key("down"), 0, 3), Some(1));
        assert_eq!(step(&key("down"), 2, 3), Some(2), "最后一页不再往下");
        assert_eq!(step(&key("up"), 0, 3), Some(0));
        assert_eq!(step(&key("end"), 0, 3), Some(2));
        assert_eq!(step(&key("home"), 2, 3), Some(0));
        assert_eq!(step(&key("tab"), 0, 3), None, "Tab 照常切换焦点");
        assert_eq!(step(&key("ctrl-down"), 0, 3), None);
        assert_eq!(step(&key("down"), 0, 0), None);
    }
}
