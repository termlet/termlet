//! SFTP：会话（目录操作、修改权限、在线编辑）、传输（上传下载、断点续传、重连后继续）、本机一侧的文件操作、
//! 跟随终端目录。
//!
//! # 用法（界面）
//!
//! - 管理器：应用启动时建一个 [`SftpManager`]（[`ManagerOptions::with_store`] 让"传输完成后自动清除"跟随设置），
//!   各窗口共用；会话与传输记录属于窗口（`owner`，界面自己的标识），窗口关闭时调用 [`SftpManager::close_owner`]。
//! - 打开会话：[`SftpManager::open_ssh`]。从终端打开时把终端的连接组（`TerminalSession::group`）放进
//!   [`SshTarget::share_group`]，共用终端的连接、不再认证；"只用一个通道"的主机此时返回 [`SftpError::SingleChannel`]，
//!   界面改为单独连接（`share_group` 为空）或提示。打开时可能要确认指纹、输入密码与验证码（经 `prompter`）。
//!   以 root 身份打开时给出 [`SshTarget::sudo`]（在 exec 通道上用 sudo 启动 sftp-server，sudo 要密码时经它询问）。
//!   服务器上的文件名不是 UTF-8 时给出 [`SshTarget::name_encoding`]（主机设置）；解码不了的字节在名称中写成
//!   私用区的转义字符（[`raw_byte`]），界面显示为 `<0xNN>`，原样用在路径中即可操作这样的文件。
//! - 服务器拒绝了 SFTP 子系统（没有开 sftp-server、堡垒机限制）时自动改用 SCP 模式（见 `scp` 模块）：
//!   [`SftpSession::protocol`] 为 [`Protocol::Scp`]，浏览、传输、在线编辑照常，修改权限返回 [`Rejection::ScpUnsupported`]，
//!   传输失败后不能从中断处接着传（`resumable` 为假，继续时从头传）；连 scp 也没有等情况打开失败（[`SftpError::NoSftp`]）。
//! - 目录操作在 [`SftpSession`] 上（`list`、`stat`、`mkdir`、`rename`、`remove`、`chmod`、`read_text`、`write_text`）。
//!   会话断线后自动重连（ID 不变），状态经 [`SftpSession::watch_state`] 或 [`SftpEvent::SessionChanged`] 得知。
//! - 传输：[`SftpManager::upload`]、[`SftpManager::download`] 立即返回新建的任务；进度经
//!   [`SftpManager::watch_transfers`] 推送（每次是全部任务的快照），一批结束时 [`SftpManager::events`] 送出
//!   [`SftpEvent::BatchEnded`]（带汇总，[`BatchReport::should_announce`] 判断要不要弹提示）。取消、继续、清除见
//!   [`SftpManager::cancel`]、[`SftpManager::resume`]、[`SftpManager::clear`]。
//! - 本机一侧：[`list_local`]、[`local_places`]、[`make_local_dir`]、[`rename_local`]、[`trash_local`]；
//!   WSL 的位置见 [`open_wsl`]、[`auto_open_allowed`]。
//! - 跟随终端目录：优先用 shell 报告的目录（OSC 7），没有时用 [`TerminalCwd::read`]。
//!
//! 全部是异步函数，必须在核心的运行时上执行（`crate::runtime::spawn`，返回的 future 都是 `Send`）；界面线程只等待结果。
//! `watch` 与 `async-channel` 的接收端与执行器无关，在 GPUI 的任务中可以直接 `await`。

mod backend;
mod cancel;
mod client;
mod codec;
mod cwd;
mod edit;
mod entry;
mod error;
mod local;
mod manager;
mod names;
mod os;
pub mod path;
mod scp;
mod session;
mod single;
mod sudo;
mod transfer;
mod walk;

pub use backend::Protocol;
pub use codec::raw_byte;
pub use cwd::{TerminalCwd, cwd_command, parse_cwd};
pub use edit::{MAX_EDIT_SIZE, TextEncoding, TextFile, WriteResult, WriteTextRequest, is_text};
pub use entry::{Entry, EntryKind, Listing, mode_string};
pub use error::{NoSftp, Rejection, SftpError, Side};
pub use local::{
    LocalEntry, LocalListing, LocalPlaces, auto_open_allowed, list_local, local_places,
    local_roots, make_local_dir, network_server, open_wsl, rename_local, trash_local,
    valid_local_name, wsl_distro, wsl_locations,
};
pub use manager::{ManagerOptions, SftpEvent, SftpManager, SshTarget};
pub use names::{MAX_NAME_LEN, PART_SUFFIX, is_reserved_name, safe_name};
pub use session::{ChmodRequest, ChmodScope, SessionId, SessionInfo, SessionState, SftpSession};
pub use single::{Transferred, get_file, put_file};
pub use sudo::{SudoAsker, SudoError, SudoPrompt};
pub use transfer::batch::{BatchId, BatchReport, BatchSummary, QUIET};
pub use transfer::{
    ClearFilter, Direction, DownloadRequest, ResumeOptions, TransferId, TransferState,
    TransferView, UploadRequest,
};

#[cfg(test)]
mod tests;
