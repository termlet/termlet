//! 会话背后的两种做法：SFTP 子系统（[`SftpClient`]），或服务器没有 SFTP 时的 SCP 模式（[`ScpRemote`]）。
//! 目录操作与属性两者都有，结果与错误的形式相同；文件内容的读写各走各的（见传输与在线编辑）。

use std::sync::Arc;

use russh_sftp::protocol::FileAttributes;

use super::client::{ClientResult, DirItem, SftpClient};
use super::scp::ScpRemote;

/// 会话用的协议。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Protocol {
    #[default]
    Sftp,
    /// 服务器没有 SFTP，改用 SCP：没有断点续传，不能修改权限
    Scp,
}

/// 会话背后的客户端。
#[derive(Clone)]
pub(crate) enum Backend {
    Sftp(Arc<SftpClient>),
    Scp(Arc<ScpRemote>),
}

impl Backend {
    pub(crate) fn protocol(&self) -> Protocol {
        match self {
            Backend::Sftp(_) => Protocol::Sftp,
            Backend::Scp(_) => Protocol::Scp,
        }
    }

    /// 等到流结束（连接断开、服务器上的 SFTP 子系统退出）或关闭。
    pub(crate) async fn closed(&self) {
        match self {
            Backend::Sftp(client) => client.closed().await,
            Backend::Scp(remote) => remote.closed().await,
        }
    }

    pub(crate) fn close(&self) {
        match self {
            Backend::Sftp(client) => client.close(),
            Backend::Scp(remote) => remote.close(),
        }
    }

    pub(crate) async fn stat(&self, p: &str) -> ClientResult<FileAttributes> {
        match self {
            Backend::Sftp(client) => client.stat(p).await,
            Backend::Scp(remote) => remote.stat(p).await,
        }
    }

    pub(crate) async fn lstat(&self, p: &str) -> ClientResult<FileAttributes> {
        match self {
            Backend::Sftp(client) => client.lstat(p).await,
            Backend::Scp(remote) => remote.lstat(p).await,
        }
    }

    pub(crate) async fn read_dir(&self, p: &str) -> ClientResult<Vec<DirItem>> {
        match self {
            Backend::Sftp(client) => client.read_dir(p).await,
            Backend::Scp(remote) => remote.read_dir(p).await,
        }
    }

    /// 绝对路径（展开符号链接）。SCP 模式下只用于目录。
    pub(crate) async fn realpath(&self, p: &str) -> ClientResult<String> {
        match self {
            Backend::Sftp(client) => client.realpath(p).await,
            Backend::Scp(remote) => remote.realpath_dir(p).await,
        }
    }

    pub(crate) async fn mkdir(&self, p: &str) -> ClientResult<()> {
        match self {
            Backend::Sftp(client) => client.mkdir(p).await,
            Backend::Scp(remote) => remote.mkdir(p).await,
        }
    }

    /// 删除一个文件（传输的临时文件）。
    pub(crate) async fn remove(&self, p: &str) -> ClientResult<()> {
        match self {
            Backend::Sftp(client) => client.remove(p).await,
            Backend::Scp(remote) => remote.remove_file(p).await,
        }
    }

    /// 改名（目标已存在时由调用方先拒绝）。
    pub(crate) async fn rename(&self, from: &str, to: &str) -> ClientResult<()> {
        match self {
            Backend::Sftp(client) => client.rename(from, to).await,
            Backend::Scp(remote) => remote.rename(from, to).await,
        }
    }
}
