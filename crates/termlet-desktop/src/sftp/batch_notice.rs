//! 一批传输结束时的提示：窗口内的提示（有下载时可以打开文件夹），窗口不在前台且这批传了很久时另发系统通知。
//! 汇总由核心给出（[`BatchReport`]），这里只管写成文案、决定发不发。

use std::time::Duration;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::{Notification, NotificationType};
use gpui_kit::{App, SharedString, Window};
use termlet_core::sftp::{BatchReport, BatchSummary};

use super::list_model::format_size;
use crate::app_state::AppState;
use crate::t;

/// 这一批传了这么久、窗口又不在前台时，另发系统通知。
const NOTIFY_AFTER: Duration = Duration::from_secs(10);

/// 汇总写成标题与说明，例如“已下载 3 项到 D:\Downloads（共 1.2 GB）”与“2 项失败，可在传输列表中继续”。
pub fn message(summary: &BatchSummary) -> (SharedString, Option<SharedString>) {
    let separator = t!("sftp.list_separator");
    let succeeded = summary.uploaded + summary.downloaded;
    let title: SharedString = if succeeded == 0 && summary.failed > 0 {
        t!("sftp.batch.all_failed", count = summary.failed)
    } else {
        let mut parts: Vec<SharedString> = Vec::new();
        if summary.uploaded > 0 {
            parts.push(match &summary.upload_dir {
                Some(dir) => t!(
                    "sftp.batch.uploaded_to",
                    count = summary.uploaded,
                    dir = dir
                ),
                None => t!("sftp.batch.uploaded", count = summary.uploaded),
            });
        }
        if summary.downloaded > 0 {
            parts.push(match &summary.download_dir {
                Some(dir) => t!(
                    "sftp.batch.downloaded_to",
                    count = summary.downloaded,
                    dir = dir.to_string_lossy()
                ),
                None => t!("sftp.batch.downloaded", count = summary.downloaded),
            });
        }
        let text = parts.join(&separator);
        if summary.bytes > 0 {
            t!(
                "sftp.batch.total",
                text = text,
                size = format_size(summary.bytes)
            )
        } else {
            text.into()
        }
    };
    let mut details: Vec<SharedString> = Vec::new();
    if summary.failed > 0 && succeeded > 0 {
        details.push(t!("sftp.batch.failed", count = summary.failed));
    }
    if summary.skipped > 0 {
        details.push(t!("sftp.batch.skipped", count = summary.skipped));
    }
    if summary.skipped_special > 0 {
        details.push(t!(
            "sftp.batch.skipped_special",
            count = summary.skipped_special
        ));
    }
    if summary.canceled > 0 {
        details.push(t!("sftp.batch.canceled", count = summary.canceled));
    }
    let description = (!details.is_empty()).then(|| details.join(&separator).into());
    (title, description)
}

/// 弹出这一批的提示；`host` 为显示用的主机名。
pub fn announce(report: &BatchReport, host: SharedString, window: &mut Window, cx: &mut App) {
    if !report.should_announce() {
        return;
    }
    let summary = &report.summary;
    let (title, description) = message(summary);
    let kind = if summary.failed > 0 {
        NotificationType::Error
    } else if summary.skipped + summary.skipped_special > 0 {
        NotificationType::Warning
    } else {
        NotificationType::Success
    };
    let mut notification = Notification::new().with_type(kind).title(title.clone());
    if let Some(description) = &description {
        notification = notification.message(description.clone());
    }
    if let Some(last) = summary.last_download.clone() {
        notification = notification.action(move |_, _, _| {
            let last = last.clone();
            Button::new("sftp-batch-open-folder")
                .link()
                .label(t!("sftp.open_folder"))
                .on_click(move |_, _, cx| cx.reveal_path(&last))
        });
    }
    window.push_notification(notification, cx);
    let notify = AppState::store(cx).settings().terminal.notify_command_done;
    if notify && !window.is_window_active() && report.elapsed >= NOTIFY_AFTER {
        let heading = if summary.failed > 0 {
            t!("sftp.batch.notify_failed", host = host)
        } else {
            t!("sftp.batch.notify_done", host = host)
        };
        let body = match description {
            Some(description) => format!("{title}\n{description}").into(),
            None => title,
        };
        cx.show_system_notification(gpui_kit::SystemNotification {
            tag: format!("sftp-{}", report.host).into(),
            title: heading,
            body,
            actions: Vec::new(),
        });
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::i18n::{self, Language};

    #[test]
    fn summary_reads_like_a_sentence() {
        i18n::set_language(Language::ZhCn);
        let summary = BatchSummary {
            downloaded: 3,
            failed: 2,
            bytes: 1536,
            download_dir: Some(PathBuf::from("D:\\Downloads")),
            ..Default::default()
        };
        let (title, description) = message(&summary);
        assert_eq!(title.as_ref(), "已下载 3 项到 D:\\Downloads（共 1.5 KB）");
        assert_eq!(description.as_deref(), Some("2 项失败，可在传输列表中继续"));

        let all_failed = BatchSummary {
            failed: 2,
            ..Default::default()
        };
        assert_eq!(
            message(&all_failed).0.as_ref(),
            "2 项传输失败，可在传输列表中继续"
        );
        assert_eq!(message(&all_failed).1, None, "全部失败时说明里不再重复");

        let mixed = BatchSummary {
            uploaded: 1,
            downloaded: 1,
            skipped: 4,
            canceled: 1,
            ..Default::default()
        };
        let (title, description) = message(&mixed);
        assert_eq!(title.as_ref(), "已上传 1 项，已下载 1 项");
        assert_eq!(
            description.as_deref(),
            Some("4 项因没有权限跳过，1 项已取消")
        );
    }
}
