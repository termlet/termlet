//! 会话录制：把终端的输出连同时间写成 asciicast v2（`.cast`，可以用 asciinema 回放），在 Termlet 中回放。
//!
//! - 格式的写出与读入见 [`cast`]；录制见 [`Recording`]（每个终端一个，后台线程写文件）；回放的进度见 [`player`]。
//! - 录制只取远端的原始输出（终端解析线程的输出旁路，与会话日志相同），开始时先写一份当前屏幕的内容，回放从录制
//!   开始时的画面接上；终端大小变化记为 `r` 事件。
//! - 默认不录输入：输入里可能有密码。用户在设置中开启“同时录制输入”后才记 `i` 事件（只记用户的键入与粘贴，
//!   应用代填的 sudo 密码不记）。输出里同样可能有敏感内容，开始录制时界面会提示。
//! - 头部只写宽高、开始时间、标题（主机名）、终端类型与配色；不写环境变量（可能含秘密）。
//! - 文件在 `<目录>/<主机名>-<YYYYMMDD-HHMMSS>.cast`：目录见 [`recordings_root`]，主机名经 [`crate::session_log::dir_name`]
//!   清理，同一秒开始的多个录制加 `-2`、`-3`。

pub mod cast;
pub mod player;
mod recorder;

use std::path::PathBuf;

use crate::model::LogSettings;
use crate::paths::DataDir;

pub use recorder::{Halt, MAX_FILE_BYTES, PendingRecording, Recording, RecordingOptions, stop_all};

/// 录制文件的目录：设置中的 `recording_dir`（去掉首尾空白），留空时是数据目录下的 `recordings`。
pub fn recordings_root(settings: &LogSettings, data_dir: &DataDir) -> PathBuf {
    match settings.recording_dir.trim() {
        "" => data_dir.recordings_dir(),
        dir => PathBuf::from(dir),
    }
}

/// 路径是不是录制文件（按扩展名，不区分大小写）：拖进窗口的文件据此打开回放。
pub fn is_cast_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(cast::EXTENSION))
}
