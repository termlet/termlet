//! 备份、快照、主机文件与导入的错误。

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::CoreError;

/// 备份与恢复、本机快照、导出与导入主机文件、导入主机、定时备份失败的原因。`Display` 是写给日志看的中文，
/// 界面按变体选用自己的文案；其中没有秘密。可以序列化：定时备份上次失败的原因存在本机，重启后界面照样能显示。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "code", content = "detail", rename_all = "snake_case")]
pub enum BackupError {
    /// 新设置的备份、导出密码太短
    #[error("密码至少 {min} 个字符")]
    PasswordTooShort { min: usize },
    /// 文件已加密，需要输入密码
    #[error("文件已加密，需要密码")]
    PasswordRequired,
    /// 密码错误，或文件被改动过
    #[error("密码错误，或文件已损坏")]
    WrongPassword,
    /// 不是 Termlet 的备份、快照或主机文件，或文件已损坏
    #[error("不是 Termlet 的备份或导出文件，或文件已损坏")]
    Invalid,
    /// 来自更新版本的 Termlet（数据库结构版本或文件格式版本 `found`，本版本最高支持 `supported`），需要先升级
    #[error("来自更新版本的 Termlet（版本 {found}，本版本最高支持 {supported}）")]
    TooNew { found: i64, supported: i64 },
    /// 选择的是导出的主机文件，应在"导入主机"中导入
    #[error("这是导出的主机文件，不是完整备份")]
    IsHostsFile,
    /// 选择的是完整备份，应在设置的"备份"中恢复
    #[error("这是完整备份，不是导出的主机文件")]
    IsFullBackup,
    /// 快照中的秘密无法用本机的主密钥解开：快照来自另一台电脑，或之后重置过保险库
    #[error("快照中的秘密无法用本机的主密钥解开")]
    ForeignSnapshot,
    /// 快照不存在（已被清理或删除）
    #[error("快照不存在")]
    SnapshotNotFound,
    /// 快照名称不正确（不是 [`super::SnapshotInfo::name`] 给出的名称）
    #[error("快照名称不正确")]
    InvalidSnapshotName,
    /// 没有选择要导出或导入的主机
    #[error("没有选择主机")]
    NothingSelected,
    /// 要导出的主机不存在（已被删除）
    #[error("主机不存在")]
    HostNotFound,
    /// 保险库已锁定（需要主密码），读写秘密的操作不能进行
    #[error("保险库已锁定")]
    VaultLocked,
    /// 系统钥匙串不可用：主密钥丢失时恢复完整备份需要先重置保险库，而钥匙串写不进新的主密钥
    #[error("系统钥匙串不可用：{0}")]
    VaultUnavailable(String),
    /// 文件不存在
    #[error("文件不存在：{}", .path.display())]
    FileNotFound { path: PathBuf },
    /// 没有权限读取或写入
    #[error("没有权限访问：{}", .path.display())]
    PermissionDenied { path: PathBuf },
    /// 读取失败
    #[error("读取 {} 失败：{reason}", .path.display())]
    ReadFailed { path: PathBuf, reason: String },
    /// 写入失败（磁盘已满、所在目录不存在等）
    #[error("写入 {} 失败：{reason}", .path.display())]
    WriteFailed { path: PathBuf, reason: String },
    /// 路径不是绝对路径
    #[error("需要绝对路径")]
    NotAbsolute,
    /// 选择的是目录
    #[error("这是一个目录：{}", .path.display())]
    IsDirectory { path: PathBuf },
    /// 文件过大，不是 Termlet 的备份或导出文件
    #[error("文件过大，不是 Termlet 的备份或导出文件")]
    TooLarge,
    /// 定时备份的文件夹不存在或无法访问（U 盘没插、网盘目录被移走）
    #[error("备份文件夹不存在或无法访问：{}", .path.display())]
    FolderMissing { path: PathBuf },
    /// 定时备份的计划不合法（保留份数、时间、星期超出范围）
    #[error("定时备份的计划不合法")]
    InvalidSchedule,
    /// 还没有设置定时备份
    #[error("还没有设置定时备份")]
    NotScheduled,
    /// 保险库重置过，定时备份的密码已无法解开：需要在设置中重新输入
    #[error("定时备份的密码已无法解开，需要重新输入")]
    SchedulePasswordLost,
    /// 存储的其他错误（数据库读写失败等）
    #[error("{0}")]
    Store(String),
}

impl From<CoreError> for BackupError {
    fn from(error: CoreError) -> Self {
        match error {
            CoreError::VaultLocked => BackupError::VaultLocked,
            CoreError::Keyring(reason) => BackupError::VaultUnavailable(reason),
            CoreError::DatabaseTooNew {
                found, supported, ..
            } => BackupError::TooNew { found, supported },
            other => BackupError::Store(other.to_string()),
        }
    }
}

impl From<rusqlite::Error> for BackupError {
    fn from(error: rusqlite::Error) -> Self {
        BackupError::Store(format!("数据库错误：{error}"))
    }
}

/// 读取 `path` 时的错误。
pub(crate) fn read_error(path: &Path, error: &io::Error) -> BackupError {
    match error.kind() {
        io::ErrorKind::NotFound => BackupError::FileNotFound {
            path: path.to_path_buf(),
        },
        io::ErrorKind::PermissionDenied => BackupError::PermissionDenied {
            path: path.to_path_buf(),
        },
        _ => BackupError::ReadFailed {
            path: path.to_path_buf(),
            reason: error.to_string(),
        },
    }
}

/// 写入 `path` 时的错误。
pub(crate) fn write_error(path: &Path, error: &io::Error) -> BackupError {
    match error.kind() {
        io::ErrorKind::PermissionDenied => BackupError::PermissionDenied {
            path: path.to_path_buf(),
        },
        _ => BackupError::WriteFailed {
            path: path.to_path_buf(),
            reason: error.to_string(),
        },
    }
}
