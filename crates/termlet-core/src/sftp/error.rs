//! SFTP 的错误：按界面要写的文案分类（没有权限、不存在、已存在、空间不足、连接断开……）。
//!
//! 消息是写给日志看的中文；界面按变体与 [`Side`]、[`Rejection`] 选用自己的文案。错误中只有路径与服务器的说明，
//! 没有秘密。

use std::io;

use russh_sftp::client::error::Error as ClientError;
use russh_sftp::protocol::StatusCode;

use super::sudo::SudoError;
use crate::ssh::SshError;

/// 出错的一侧。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// 本机
    Local,
    /// 服务器
    Remote,
}

impl Side {
    fn label(self) -> &'static str {
        match self {
            Side::Local => "本机",
            Side::Remote => "服务器上",
        }
    }
}

/// 请求本身不被接受的原因（与服务器、文件无关）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Rejection {
    #[error("不能删除根目录或初始目录")]
    RemoveRootOrHome,
    #[error("权限必须在 0–7777 之间")]
    InvalidMode,
    #[error("没有选择文件")]
    NoPaths,
    #[error("本机路径必须是绝对路径")]
    NotAbsolute,
    #[error("不支持设备路径")]
    DevicePath,
    #[error("名称无效")]
    InvalidName,
    #[error("不是普通文件")]
    NotRegularFile,
    #[error("不是目录")]
    NotDirectory,
    #[error("只能继续失败的传输")]
    NotResumable,
    #[error("只能在同一台主机的会话上继续")]
    OtherHost,
    #[error("找不到不重名的名称")]
    NoUniqueName,
    #[error("内容中有所选编码不能表示的字符")]
    Unencodable,
    /// 路径中有服务器的文件名编码（主机设置）不能表示的字符
    #[error("名称中有服务器的文件名编码不能表示的字符")]
    UnencodableName,
    #[error("当前平台不支持移到回收站")]
    TrashUnsupported,
    /// 系统不能把它移到回收站（没有回收站的位置、太大等），用户又没有确认永久删除：没有删除
    #[error("不能移到回收站，没有删除")]
    NotRecyclable,
    /// SCP 模式（服务器没有 SFTP）不支持的操作：修改权限等
    #[error("SCP 模式不支持这个操作")]
    ScpUnsupported,
}

/// 服务器没有开启 SFTP、又不能改用 SCP 的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NoSftp {
    /// 服务器上没有 scp 命令
    #[error("服务器上没有 scp 命令")]
    NoScp,
    /// 服务器不允许另开通道执行命令（堡垒机、ForceCommand 等）
    #[error("服务器不允许另开通道执行命令")]
    ExecRefused,
    /// 服务器上没有可用的 sh 或 ls（例如 Windows 的 cmd）
    #[error("服务器上没有可用的 sh 或 ls")]
    NoShell,
    /// 主机设置了“只用一个通道”：SCP 模式的每个操作都要另开通道
    #[error("这台主机只用一个通道")]
    SingleChannel,
}

/// SFTP 操作失败的原因。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SftpError {
    #[error("{}的路径不存在：{path}", .side.label())]
    NotFound { side: Side, path: String },
    #[error("没有权限访问{}的 {path}", .side.label())]
    PermissionDenied { side: Side, path: String },
    #[error("{}已存在：{path}", .side.label())]
    AlreadyExists { side: Side, path: String },
    #[error("{}的空间不足：{path}", .side.label())]
    NoSpace { side: Side, path: String },
    /// 文件正被别的程序使用（Windows 上删除、改名时）
    #[error("{}的 {path} 正被别的程序使用", .side.label())]
    Busy { side: Side, path: String },
    /// SFTP 连接断开（或请求长时间没有回应）；能自动重连的会话随后重连
    #[error("SFTP 连接已断开")]
    Disconnected,
    /// 服务器上打不开 SFTP：没有启用 SFTP 子系统、不允许再开通道
    #[error("服务器上无法使用 SFTP：{0}")]
    Unavailable(String),
    /// 服务器拒绝了 SFTP 子系统（没有开 sftp-server、堡垒机限制），也不能改用 SCP
    #[error("服务器没有开启 SFTP，也不能改用 SCP：{0}")]
    NoSftp(NoSftp),
    /// 主机设置了"只用一个通道"（堡垒机）：不能与终端共用连接打开文件管理
    #[error("这台主机只用一个通道，不能共用终端的连接")]
    SingleChannel,
    /// 连接或认证失败
    #[error("{0}")]
    Connect(#[from] SshError),
    /// 以 root 身份打开（sudo）失败
    #[error("{0}")]
    Sudo(SudoError),
    /// 在线编辑：文件超过上限
    #[error("文件超过 {limit} 字节，不能在线编辑：{path}")]
    TooLarge { path: String, limit: u64 },
    /// 在线编辑：不是所选编码的文本（含 NUL 字节或不是合法的编码）
    #[error("不是可以编辑的文本文件：{path}")]
    NotText { path: String },
    /// 在线编辑：保存前发现远程文件在打开后被修改（或删除）
    #[error("远程文件在打开后已被{}：{path}", changed_word(.deleted))]
    Changed { path: String, deleted: bool },
    /// 服务器不支持 posix-rename、经备份替换原文件时失败，原文件没能改回：原文件在 `backup` 中，
    /// 新内容（在线编辑保存时）在 `kept` 中
    #[error("替换 {path} 失败：{reason}；原文件已改名为 {backup}，没能改回{}", kept_note(.kept))]
    ReplaceFailed {
        path: String,
        backup: String,
        kept: Option<String>,
        reason: String,
    },
    #[error("{0}")]
    Rejected(Rejection),
    /// 其余错误：服务器或系统的说明原样带上（例如删除非空目录）
    #[error("{}的 {path}：{message}", .side.label())]
    Failed {
        side: Side,
        path: String,
        message: String,
    },
    #[error("已取消")]
    Canceled,
    #[error("SFTP 会话不存在或已关闭")]
    SessionClosed,
    #[error("传输任务不存在")]
    TransferNotFound,
}

fn changed_word(deleted: &bool) -> &'static str {
    if *deleted { "删除" } else { "修改" }
}

fn kept_note(kept: &Option<String>) -> String {
    kept.as_deref()
        .map(|k| format!("；新内容保存在 {k}"))
        .unwrap_or_default()
}

impl From<Rejection> for SftpError {
    fn from(rejection: Rejection) -> Self {
        SftpError::Rejected(rejection)
    }
}

impl SftpError {
    /// 连接断开（会话正在或将要自动重连时，失败的传输重连后自动继续）。
    pub fn is_disconnected(&self) -> bool {
        matches!(self, SftpError::Disconnected)
    }

    /// 自动重连是否值得再试：网络类的连接错误、连接断开。认证失败、需要交互、服务器不支持 SFTP 时放弃。
    pub(crate) fn is_retryable(&self) -> bool {
        match self {
            SftpError::Connect(e) => e.is_retryable(),
            SftpError::Disconnected => true,
            _ => false,
        }
    }

    pub(crate) fn failed(side: Side, path: &str, message: impl Into<String>) -> Self {
        SftpError::Failed {
            side,
            path: path.to_string(),
            message: message.into(),
        }
    }
}

/// 服务器返回的状态码（不是状态错误时为 `None`）。
pub(crate) fn status_of(error: &ClientError) -> Option<StatusCode> {
    match error {
        ClientError::Status(status) => Some(status.status_code),
        _ => None,
    }
}

pub(crate) fn is_permission(error: &ClientError) -> bool {
    status_of(error) == Some(StatusCode::PermissionDenied)
}

pub(crate) fn is_not_found(error: &ClientError) -> bool {
    status_of(error) == Some(StatusCode::NoSuchFile)
}

/// 把 SFTP 客户端的错误转换为带路径的 [`SftpError`]（远程一侧）。
pub(crate) fn remote(path: &str, error: &ClientError) -> SftpError {
    let path = path.to_string();
    match error {
        // 文件名的转换代替服务器回答的（见 codec）
        ClientError::Status(status) if status.error_message == super::codec::UNENCODABLE => {
            Rejection::UnencodableName.into()
        }
        ClientError::Status(status) => match status.status_code {
            StatusCode::NoSuchFile => SftpError::NotFound {
                side: Side::Remote,
                path,
            },
            StatusCode::PermissionDenied => SftpError::PermissionDenied {
                side: Side::Remote,
                path,
            },
            StatusCode::NoConnection | StatusCode::ConnectionLost => SftpError::Disconnected,
            StatusCode::OpUnsupported => SftpError::Failed {
                side: Side::Remote,
                path,
                message: "服务器不支持这个操作".into(),
            },
            _ => SftpError::Failed {
                side: Side::Remote,
                path,
                message: if status.error_message.is_empty() {
                    status.status_code.to_string()
                } else {
                    status.error_message.clone()
                },
            },
        },
        ClientError::Limited(message) => SftpError::Failed {
            side: Side::Remote,
            path,
            message: message.clone(),
        },
        // 连接断开时等待中的请求得到"发送端已丢弃"、会话关闭后的请求得到"会话已关闭"；请求超时也按断开处理
        ClientError::IO(_)
        | ClientError::Timeout
        | ClientError::UnexpectedBehavior(_)
        | ClientError::UnexpectedPacket => SftpError::Disconnected,
    }
}

/// 把本机文件操作的错误转换为带路径的 [`SftpError`]。
pub(crate) fn local(path: impl AsRef<std::path::Path>, error: &io::Error) -> SftpError {
    let path = path.as_ref().display().to_string();
    let side = Side::Local;
    match error.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => {
            SftpError::NotFound { side, path }
        }
        io::ErrorKind::PermissionDenied => SftpError::PermissionDenied { side, path },
        io::ErrorKind::AlreadyExists | io::ErrorKind::DirectoryNotEmpty => {
            SftpError::AlreadyExists { side, path }
        }
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => {
            SftpError::NoSpace { side, path }
        }
        io::ErrorKind::ResourceBusy => SftpError::Busy { side, path },
        _ if is_sharing_violation(error) => SftpError::Busy { side, path },
        _ => SftpError::Failed {
            side,
            path,
            message: error.to_string(),
        },
    }
}

/// Windows 的共享冲突（ERROR_SHARING_VIOLATION、ERROR_LOCK_VIOLATION）：文件被别的程序打开着。
fn is_sharing_violation(error: &io::Error) -> bool {
    cfg!(windows) && matches!(error.raw_os_error(), Some(32 | 33))
}

#[cfg(test)]
mod tests {
    use russh_sftp::protocol::Status;

    use super::*;

    fn status(code: StatusCode, message: &str) -> ClientError {
        ClientError::Status(Status {
            id: 1,
            status_code: code,
            error_message: message.into(),
            language_tag: "en-US".into(),
        })
    }

    #[test]
    fn maps_remote_errors_by_status() {
        assert_eq!(
            remote("/a", &status(StatusCode::NoSuchFile, "")),
            SftpError::NotFound {
                side: Side::Remote,
                path: "/a".into()
            }
        );
        assert!(matches!(
            remote("/a", &status(StatusCode::PermissionDenied, "")),
            SftpError::PermissionDenied { .. }
        ));
        assert_eq!(
            remote(
                "/a",
                &ClientError::UnexpectedBehavior("sender dropped".into())
            ),
            SftpError::Disconnected
        );
        assert_eq!(remote("/a", &ClientError::Timeout), SftpError::Disconnected);
        match remote("/d", &status(StatusCode::Failure, "Directory not empty")) {
            SftpError::Failed { message, .. } => assert_eq!(message, "Directory not empty"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn maps_local_errors_by_kind() {
        let e = |kind| local("C:/x", &io::Error::from(kind));
        assert!(matches!(
            e(io::ErrorKind::NotFound),
            SftpError::NotFound {
                side: Side::Local,
                ..
            }
        ));
        assert!(matches!(
            e(io::ErrorKind::PermissionDenied),
            SftpError::PermissionDenied { .. }
        ));
        assert!(matches!(
            e(io::ErrorKind::AlreadyExists),
            SftpError::AlreadyExists { .. }
        ));
        assert!(matches!(
            e(io::ErrorKind::StorageFull),
            SftpError::NoSpace { .. }
        ));
        let text = SftpError::ReplaceFailed {
            path: "/c".into(),
            backup: "/.c.bak".into(),
            kept: Some("/.c.tmp".into()),
            reason: "x".into(),
        }
        .to_string();
        assert!(
            text.contains("/.c.bak") && text.contains("/.c.tmp"),
            "{text}"
        );
    }
}
