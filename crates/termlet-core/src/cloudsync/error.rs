//! 云同步的错误。界面按变体选用自己的文案；消息是写给日志看的中文，不含秘密。

use serde::{Deserialize, Serialize};

use crate::CoreError;
use crate::backup::BackupError;

/// 同步失败的原因。可以序列化：上次同步的错误存在本机，重启后界面照样能显示。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum SyncError {
    /// 还没有设置同步，或保险库重置后同步密码已无法解开（需要重新设置）
    #[error("还没有设置同步")]
    NotConfigured,
    /// 保险库未解锁：读不了同步密码与本机的秘密
    #[error("保险库已锁定")]
    VaultLocked,
    /// 设置不合法。`field`：`path`（`not_absolute`）、`url`（`invalid`、`has_credentials`、`insecure`）、
    /// `password`（`too_short`）、`device_name`（`too_long`）、`interval`（`out_of_range`）；
    /// S3：`endpoint`（`required`、`invalid`、`has_credentials`、`insecure`）、`region`（`invalid`、`mismatch`）、
    /// `bucket`（`required`、`invalid`、`not_found`）、`prefix`（`invalid`）、`access_key`（`required`、`invalid`）、
    /// `secret_key`（`required`）；Gist：`token`（`required`、`no_gist_scope`）、`gist`（`invalid`、`not_found`）
    #[error("同步设置不合法：{field}：{reason}")]
    Invalid { field: String, reason: String },
    /// 同步文件夹不存在或无法访问（U 盘没插、网盘目录被移走）
    #[error("同步文件夹不存在或无法访问：{path}")]
    FolderMissing { path: String },
    /// 同步文件夹不可写
    #[error("同步文件夹不可写：{path}")]
    FolderNotWritable { path: String },
    /// 无法连接同步服务器（WebDAV、S3、GitHub；网络、DNS、超时、证书）
    #[error("无法连接同步服务器：{message}")]
    Network { message: String },
    /// 账号、密码、访问密钥或令牌不正确，或没有访问权限（401 / 403）
    #[error("账号、密钥或令牌不正确，或没有访问权限")]
    AuthFailed,
    /// 服务器要求跳转：请把地址改为跳转后的地址（例如 `https://`）；S3 为区域或终端节点不对，`location` 是服务给出的
    /// 终端节点
    #[error("同步服务器要求跳转（返回 {status}）到 {location}")]
    Redirected { status: u16, location: String },
    /// 同步服务器返回了其他错误
    #[error("同步服务器返回 {status}")]
    Server { status: u16 },
    /// S3 兼容存储返回了其他错误（`error_code` 是 S3 的错误代码，例如 `InvalidArgument`）
    #[error("S3 返回 {status}：{error_code}")]
    S3 { status: u16, error_code: String },
    /// 本机时间与 S3 服务相差太多（签名有时效），请校准系统时间
    #[error("本机时间与服务器相差太多")]
    ClockSkew,
    /// 请求太频繁，被服务限制（GitHub 的速率限制、S3 的 SlowDown）；`retry_after` 是建议等待的秒数
    #[error("请求太频繁，被服务限制")]
    RateLimited {
        #[serde(default)]
        retry_after: Option<u64>,
    },
    /// 没有指定 Gist，而令牌所属的账号中有多个 Termlet 的 Gist：请指定用哪一个
    #[error("找到多个 Termlet 的 Gist：{}", .ids.join("、"))]
    GistAmbiguous { ids: Vec<String> },
    /// 加密后的同步文件超过 Gist 能存放的大小（字节）
    #[error("同步文件有 {size} 字节，超过 Gist 的上限 {limit} 字节")]
    GistTooLarge { size: u64, limit: u64 },
    /// 存储空间不足（同步位置或本机）
    #[error("存储空间不足")]
    InsufficientStorage,
    /// 同步密码错误（或同步文件被改动）
    #[error("同步密码错误，或同步文件已被改动")]
    WrongPassword,
    /// 同步位置中的文件不是 Termlet 的同步文件，或已损坏
    #[error("同步文件已损坏，或不是 Termlet 的同步文件")]
    Corrupt,
    /// 同步文件或要同步的数据超过大小上限
    #[error("同步文件过大")]
    TooLarge,
    /// 云端的同步数据来自更新版本的 Termlet：请先升级本机的 Termlet
    #[error("云端的同步数据来自更新版本的 Termlet")]
    TooNew,
    /// 同步文件在这期间被其他设备反复修改，几次重试都没能写入
    #[error("同步文件被其他设备频繁修改，请稍后再试")]
    Contended,
    /// 本机的数据在同步期间一直在修改
    #[error("本机的数据在同步期间一直在修改，请稍后再试")]
    LocalBusy,
    /// 写入其他设备的修改之前保存本机快照失败（没有写入）
    #[error("同步前保存本机快照失败：{message}")]
    Snapshot { message: String },
    /// 读写本机文件失败（同步文件夹中的文件）
    #[error("读写同步文件失败：{message}")]
    Io { message: String },
    /// 本机的数据库、保险库出错
    #[error("本机数据出错：{message}")]
    Local { message: String },
}

impl SyncError {
    pub(crate) fn invalid(field: &str, reason: &str) -> Self {
        SyncError::Invalid {
            field: field.into(),
            reason: reason.into(),
        }
    }

    /// 稳定的错误代码（日志、CLI 用）。
    pub fn code(&self) -> &'static str {
        match self {
            SyncError::NotConfigured => "not_configured",
            SyncError::VaultLocked => "vault_locked",
            SyncError::Invalid { .. } => "invalid",
            SyncError::FolderMissing { .. } => "folder_missing",
            SyncError::FolderNotWritable { .. } => "folder_not_writable",
            SyncError::Network { .. } => "network",
            SyncError::AuthFailed => "auth_failed",
            SyncError::Redirected { .. } => "redirected",
            SyncError::Server { .. } => "server",
            SyncError::S3 { .. } => "s3",
            SyncError::ClockSkew => "clock_skew",
            SyncError::RateLimited { .. } => "rate_limited",
            SyncError::GistAmbiguous { .. } => "gist_ambiguous",
            SyncError::GistTooLarge { .. } => "gist_too_large",
            SyncError::InsufficientStorage => "insufficient_storage",
            SyncError::WrongPassword => "wrong_password",
            SyncError::Corrupt => "corrupt",
            SyncError::TooLarge => "too_large",
            SyncError::TooNew => "too_new",
            SyncError::Contended => "contended",
            SyncError::LocalBusy => "local_busy",
            SyncError::Snapshot { .. } => "snapshot",
            SyncError::Io { .. } => "io",
            SyncError::Local { .. } => "local",
        }
    }

    /// 读写本机文件的错误：空间不足单独归类。
    pub(crate) fn io(error: &std::io::Error) -> Self {
        if error.kind() == std::io::ErrorKind::StorageFull {
            return SyncError::InsufficientStorage;
        }
        SyncError::Io {
            message: error.to_string(),
        }
    }
}

impl From<CoreError> for SyncError {
    fn from(error: CoreError) -> Self {
        match error {
            CoreError::VaultLocked => SyncError::VaultLocked,
            CoreError::Database(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::DiskFull =>
            {
                SyncError::InsufficientStorage
            }
            CoreError::Io(e) => SyncError::io(&e),
            other => SyncError::Local {
                message: other.to_string(),
            },
        }
    }
}

impl From<BackupError> for SyncError {
    fn from(error: BackupError) -> Self {
        match error {
            BackupError::WrongPassword | BackupError::PasswordRequired => SyncError::WrongPassword,
            BackupError::Invalid => SyncError::Corrupt,
            BackupError::PasswordTooShort { .. } => SyncError::invalid("password", "too_short"),
            BackupError::TooLarge => SyncError::TooLarge,
            BackupError::VaultLocked => SyncError::VaultLocked,
            other => SyncError::Snapshot {
                message: other.to_string(),
            },
        }
    }
}

impl From<serde_json::Error> for SyncError {
    fn from(error: serde_json::Error) -> Self {
        SyncError::Local {
            message: error.to_string(),
        }
    }
}
