//! 给用户的错误与警告提示：弹出提示的同时写一条日志，排查问题时能从日志中对上用户当时看到了什么。
//!
//! 成功、普通信息的提示不经这里（不写日志）。提示的文字本来就是给用户看的，不含秘密。

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::{App, SharedString, Window};

/// 出错的提示（日志为 warn）。
pub fn error(message: impl Into<SharedString>, window: &mut Window, cx: &mut App) {
    let message = message.into();
    tracing::warn!(%message, "提示用户：出错");
    window.push_notification(Notification::error(message), cx);
}

/// 警告的提示（日志为 info：多是当前用不了某个操作）。
pub fn warning(message: impl Into<SharedString>, window: &mut Window, cx: &mut App) {
    let message = message.into();
    tracing::info!(%message, "提示用户：警告");
    window.push_notification(Notification::warning(message), cx);
}
