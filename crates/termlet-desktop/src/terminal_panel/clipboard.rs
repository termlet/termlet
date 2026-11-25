//! 终端中的程序经 OSC 52 写了剪贴板时的提示（写不写由终端视图决定，见 `termlet_terminal::clipboard`）。
//!
//! - 写了：不打扰的通知“web-1 写入了剪贴板（N 字）”，点“查看”看写进去的内容；同一个终端接连写入时替换上一条，
//!   不堆起来。
//! - 太大没写：提示大小与上限，免得以为复制成功了。

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _};
use gpui_kit::{
    App, ElementId, InteractiveElement as _, ParentElement as _, SharedString, Styled as _,
    TestSupportExt as _, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_terminal::{ClipboardText, MAX_CLIPBOARD_WRITE};

use crate::prose::prose;
use crate::t;

/// 通知的 ID 用的类型：同一个终端的替换上一条。
struct ClipboardNotice;

/// “查看”中最多显示这么多字：内容最大 1 MiB，整个排版太慢，剪贴板里已经是全文。
const PREVIEW_CHARS: usize = 20_000;

/// 程序写了剪贴板：`name` 是终端的名称（主机名、本地 shell），`key` 区分是哪个终端。
pub(super) fn written(
    name: SharedString,
    text: &ClipboardText,
    key: impl Into<ElementId>,
    window: &mut Window,
    cx: &mut App,
) {
    let count = text.as_str().chars().count();
    let preview: SharedString = text
        .as_str()
        .chars()
        .take(PREVIEW_CHARS)
        .collect::<String>()
        .into();
    let message = t!(
        "terminal.clipboard.written",
        name = name.clone(),
        count = count
    );
    let notification = Notification::info(message)
        .id1::<ClipboardNotice>(key)
        .action(move |_, _, cx| {
            let notification = cx.entity().downgrade();
            let (name, preview) = (name.clone(), preview.clone());
            Button::new("clipboard-written-view")
                .link()
                .label(t!("terminal.clipboard.view"))
                .on_click(move |_, window, cx| {
                    open_content(name.clone(), preview.clone(), count, window, cx);
                    let _ = notification
                        .update(cx, |notification, cx| notification.dismiss(window, cx));
                })
        });
    window.push_notification(notification, cx);
}

/// 程序要写进剪贴板的内容太大，没有写入。
pub(super) fn too_large(
    name: SharedString,
    bytes: usize,
    key: impl Into<ElementId>,
    window: &mut Window,
    cx: &mut App,
) {
    let message = t!(
        "terminal.clipboard.too_large",
        name = name,
        size = megabytes(bytes),
        limit = megabytes(MAX_CLIPBOARD_WRITE)
    );
    window.push_notification(
        Notification::warning(message).id1::<ClipboardNotice>(key),
        cx,
    );
}

/// 以 MB（1024 × 1024 字节）写的大小，最多两位小数：刚过上限的也看得出比上限大（1.05 MB 与 1 MB）。
fn megabytes(bytes: usize) -> String {
    let text = format!("{:.2}", bytes as f64 / (1024.0 * 1024.0));
    let text = text.trim_end_matches('0').trim_end_matches('.');
    format!("{text} MB")
}

/// 写进剪贴板的内容（只读；太长时只显示开头）。
fn open_content(
    name: SharedString,
    preview: SharedString,
    count: usize,
    window: &mut Window,
    cx: &mut App,
) {
    crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, cx| {
        let theme = cx.theme();
        let shown = preview.chars().count();
        alert
            .title(t!("terminal.clipboard.view_title", name = name.clone()))
            .description(
                div()
                    .id("clipboard-written-content")
                    .test_support()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .when(shown < count, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(prose(t!(
                                    "terminal.clipboard.truncated",
                                    shown = shown,
                                    total = count
                                ))),
                        )
                    })
                    .child(
                        div()
                            .id("clipboard-written-text")
                            .max_h(rems(20.))
                            .overflow_y_scrollbar()
                            .px_2()
                            .py_1p5()
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.muted)
                            .text_color(theme.foreground)
                            .text_xs()
                            .font_family(termlet_terminal::fonts::DEFAULT_FAMILY)
                            .child(preview.clone()),
                    ),
            )
            .ok_text(t!("common.close"))
    });
}

#[cfg(test)]
mod tests;
