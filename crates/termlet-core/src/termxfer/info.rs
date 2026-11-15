//! 一次传输的状态（界面显示提示栏用）、用户的回答、错误。

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// 一次传输的标识（进程内唯一）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TransferId(u64);

impl TransferId {
    pub(crate) fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

impl fmt::Display for TransferId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "xfer-{}", self.0)
    }
}

/// 协议。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Protocol {
    /// ZModem：服务器上的 `sz`（发送）、`rz`（接收），lrzsz
    Zmodem,
    /// trzsz：服务器上的 `tsz`（发送）、`trz`（接收，`trz -d` 接收目录）
    Trzsz,
}

impl Protocol {
    /// 显示用的名称（ZModem、trzsz）。
    pub fn label(self) -> &'static str {
        match self {
            Protocol::Zmodem => "ZModem",
            Protocol::Trzsz => "trzsz",
        }
    }
}

/// 传输进行到哪一步。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransferState {
    /// 等待用户选择要上传的文件或保存的位置（[`super::Interceptor::answer`]）
    Waiting,
    /// 进行中
    Running,
    /// 完成（可能有被跳过的文件，见 [`TransferInfo::skipped`]）
    Done,
    /// 失败
    Failed(XferError),
    /// 用户取消、拒绝或没有回答（等待超时）
    Cancelled,
}

impl TransferState {
    /// 已经结束（完成、失败或取消）。
    pub fn is_finished(&self) -> bool {
        matches!(
            self,
            TransferState::Done | TransferState::Failed(_) | TransferState::Cancelled
        )
    }
}

/// 终端中一次文件传输的状态。每次变化经事件送出一份完整的快照。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferInfo {
    pub id: TransferId,
    pub protocol: Protocol,
    /// 本机发往服务器（服务器上运行的是 `rz` / `trz`）；否则是下载（`sz` / `tsz`）
    pub upload: bool,
    /// 上传目录（`trz -d`）：选择时可以选目录
    pub directory: bool,
    pub state: TransferState,
    /// 当前文件的名称、它已传输的字节数与大小
    pub file: String,
    pub file_bytes: u64,
    pub file_size: u64,
    /// 已完成的文件数、文件总数（不知道时为 0）
    pub files: usize,
    pub total_files: usize,
    /// 全部文件已传输的字节数与总字节数（不知道时为 0；ZModem 下载时服务器不告诉总大小）
    pub bytes: u64,
    pub total_bytes: u64,
    /// 最近几秒的平均速度（字节 / 秒），刚开始、没有在传输时为 0
    pub speed: u64,
    /// 下载保存的目录
    pub dir: Option<PathBuf>,
    /// 下载：已保存的文件与目录（本机的完整路径）
    pub saved: Vec<PathBuf>,
    /// 上传：服务器上收到的文件（名称）
    pub uploaded: Vec<String>,
    /// 被跳过的文件：上传时服务器上已有同名文件（`rz` 没有加 `-y`）、所选目录中读不了的子目录（`trz -d`）；
    /// 下载时本机建不了的文件
    pub skipped: Vec<String>,
}

impl TransferInfo {
    pub(crate) fn new(id: TransferId, protocol: Protocol, upload: bool, directory: bool) -> Self {
        Self {
            id,
            protocol,
            upload,
            directory,
            state: TransferState::Waiting,
            file: String::new(),
            file_bytes: 0,
            file_size: 0,
            files: 0,
            total_files: 0,
            bytes: 0,
            total_bytes: 0,
            speed: 0,
            dir: None,
            saved: Vec::new(),
            uploaded: Vec::new(),
            skipped: Vec::new(),
        }
    }
}

/// 用户对传输请求的回答。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// 下载：保存到这个目录
    SaveTo(PathBuf),
    /// 上传：这些文件（`trz -d` 时可以有目录）；也用于把文件拖到终端上
    Send(Vec<PathBuf>),
    /// 不传了：告诉服务器取消，服务器上的程序随之退出
    Refuse,
}

/// 本机文件操作失败的种类（界面按它选文案）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalErrorKind {
    NotFound,
    PermissionDenied,
    /// 磁盘空间不足
    NoSpace,
    /// 文件正被别的程序使用（Windows）
    Busy,
    Other,
}

impl LocalErrorKind {
    fn of(error: &io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => LocalErrorKind::NotFound,
            io::ErrorKind::PermissionDenied => LocalErrorKind::PermissionDenied,
            io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => LocalErrorKind::NoSpace,
            io::ErrorKind::ResourceBusy => LocalErrorKind::Busy,
            // Windows 的共享冲突（ERROR_SHARING_VIOLATION、ERROR_LOCK_VIOLATION）
            _ if cfg!(windows) && matches!(error.raw_os_error(), Some(32 | 33)) => {
                LocalErrorKind::Busy
            }
            _ => LocalErrorKind::Other,
        }
    }
}

/// 传输失败的原因。消息是写给日志看的中文；界面按变体选用自己的文案（带上其中的路径、名称）。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum XferError {
    /// 本地终端与 WSL 经过 Windows 的伪终端（ConPTY），它会改写二进制数据：改用 SFTP 或 trzsz
    #[error(
        "本地终端与 WSL 中不支持 ZModem（Windows 的伪终端会改写二进制数据），请改用 SFTP 或 trzsz"
    )]
    ZmodemOverConpty,
    /// tmux 不转发 ZModem 的数据（tmux 的维护者明确不支持）：改用 trzsz 或 SFTP
    #[error("tmux 中不支持 ZModem（tmux 不转发它的数据），请改用 trzsz（trz / tsz）或 SFTP")]
    ZmodemInTmux,
    /// ZModem 不能上传目录：改用 `trz -d` 或 SFTP
    #[error("ZModem 不能上传目录，请改用 trzsz 的 trz -d 或 SFTP：{}", .0.display())]
    ZmodemDirectory(PathBuf),
    /// 选了目录，但服务器上运行的是不带 `-d` 的 `trz`
    #[error("要上传目录请在服务器上运行 trz -d：{}", .0.display())]
    TrzNeedsDirectory(PathBuf),
    /// 保存位置不存在或不是目录
    #[error("保存位置不是可用的目录：{}", .0.display())]
    BadSaveDir(PathBuf),
    /// 选择的不是普通文件（设备文件、管道等）
    #[error("不是普通文件：{}", .0.display())]
    NotRegularFile(PathBuf),
    /// 读写本机文件失败
    #[error("读写本机文件 {} 失败：{message}", .path.display())]
    Local {
        path: PathBuf,
        kind: LocalErrorKind,
        message: String,
    },
    /// 保存目录中同名的文件太多，找不到不重名的名称
    #[error("找不到不重名的文件名：{0}")]
    NoUniqueName(String),
    /// 服务器要求覆盖（`trz -y`）时，所选的文件中有重名的
    #[error("有重名的文件：{0}")]
    DuplicateName(String),
    /// 服务器上的程序报告的错误（trzsz 的 `#fail`，原样带上它的说明）
    #[error("服务器报告：{0}")]
    Remote(String),
    /// 服务器上按了 Ctrl+C（trzsz）
    #[error("已中断（服务器上按了 Ctrl+C）")]
    RemoteInterrupted,
    /// 服务器上的程序中止了传输（ZModem 收到中止序列）
    #[error("服务器上的程序中止了传输")]
    RemoteAborted,
    /// 等待服务器的数据超时
    #[error("等待服务器的数据超时")]
    Timeout,
    /// 传输中终端断开（连接断开、程序退出）
    #[error("终端已断开")]
    Disconnected,
    /// 校验失败：文件不完整（trzsz 的 MD5）
    #[error("校验失败，文件不完整")]
    Checksum,
    /// trzsz 的一行超过上限：服务器上用 `tsz -B` 调小了块大小也能解决
    #[error("trzsz 协议错误：一行超过上限 {} MB（数据块过大时可在服务器上用 tsz -B 调小块大小，例如 tsz -B 10M）", super::trzsz::MAX_LINE_LEN >> 20)]
    LineTooLong,
    /// trzsz 的数据块解压后超过上限
    #[error("trzsz 数据块解压后超过上限 {} MB（可在服务器上用 tsz -B 调小块大小，例如 tsz -B 10M）", super::trzsz::MAX_BLOCK_LEN >> 20)]
    BlockTooLarge,
    /// 服务器要求的 trzsz 功能 Termlet 不支持（二进制模式、新版协议、归档传输）
    #[error("服务器要求的 trzsz 功能 Termlet 不支持：{0}")]
    UnsupportedTrzsz(&'static str),
    /// 协议数据不对（不是预期的帧或行、校验和错误、重试次数用完）
    #[error("{} 协议错误：{detail}", .protocol.label())]
    Protocol { protocol: Protocol, detail: String },
    /// 回答或取消时：没有这个传输，或它已不在等待回答
    #[error("没有在等待回答的传输")]
    NotWaiting,
    /// 回答的种类与传输不符（下载却给了要上传的文件等）
    #[error("回答与传输的方向不符")]
    WrongAnswer,
}

impl XferError {
    pub(crate) fn local(path: impl Into<PathBuf>, error: &io::Error) -> Self {
        XferError::Local {
            path: path.into(),
            kind: LocalErrorKind::of(error),
            message: error.to_string(),
        }
    }

    pub(crate) fn protocol(protocol: Protocol, detail: impl Into<String>) -> Self {
        XferError::Protocol {
            protocol,
            detail: detail.into(),
        }
    }
}
