//! 单文件传输，供 `termlet` 命令行工具的 upload / download（以及 MCP 的 `upload_file` / `download_file`）使用
//! （scp 的语义：覆盖已存在的同名文件）。经已有的 SSH 连接临时打开 SFTP，传完即关闭；失败、取消时删除临时文件
//! （不继续传输）。临时文件用一次性的随机名称，不与桌面端同时传输同一个文件时的临时文件冲突。
//!
//! 服务器拒绝了 SFTP 子系统（没有开 sftp-server、堡垒机限制）时与桌面端的文件面板一样改用 SCP 模式（见 `scp` 模块）：
//! 结果中的 [`Protocol`] 为 [`Protocol::Scp`]。命令行本来就不继续传输，SCP 模式的限制（没有断点续传）对它没有影响；
//! 连 scp 也没有、主机只用一个通道等不能改用时返回 [`SftpError::NoSftp`]。

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::backend::{Backend, Protocol};
use super::cancel::Cancel;
use super::client::{is_dir, is_regular};
use super::error::{self, Rejection, SftpError};
use super::manager::open_backend;
use super::names::safe_name;
use super::path;
use super::session::SessionId;
use super::transfer::download::{Source, download_from};
use super::transfer::{Direction, FileError, PartPath, Transfer, upload_file};
use crate::model::Encoding;
use crate::ssh::Connection;

/// 命令行传输的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transferred<P> {
    /// 实际写入的路径：上传时是服务器上的，下载时是本机的
    pub path: P,
    pub bytes: u64,
    /// 用的协议：服务器没有 SFTP 时为 SCP
    pub protocol: Protocol,
}

fn standalone(direction: Direction, overwrite: bool) -> Arc<Transfer> {
    Arc::new(Transfer::new(
        None,
        direction,
        SessionId::new(),
        String::new(),
        String::new(),
        String::new(),
        PathBuf::new(),
        String::new(),
        false,
        overwrite,
        0,
    ))
}

/// 取消后等传输自己收尾（删除临时文件）的上限：连接已经不通时不再等。
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

/// 执行 `work`；`stop` 先完成时取消 `cancel`，再等 `work` 收尾（最多 [`CLEANUP_TIMEOUT`]），结果为已取消。
async fn until_stopped<T>(
    work: impl Future<Output = Result<T, SftpError>>,
    cancel: &Cancel,
    stop: impl Future<Output = ()>,
) -> Result<T, SftpError> {
    tokio::pin!(work);
    tokio::select! {
        result = &mut work => result,
        _ = stop => {
            cancel.cancel();
            let _ = tokio::time::timeout(CLEANUP_TIMEOUT, work).await;
            Err(SftpError::Canceled)
        }
    }
}

/// 上传本机文件。`remote` 是已存在的目录或以 `/` 结尾时放入该目录，否则作为目标文件名。`single_channel` 是主机的
/// “只用一个通道”（这样的主机不能改用 SCP）。返回实际写入的远程路径、字节数与用的协议。`stop` 完成时取消
/// （删除远程的临时文件后返回 [`SftpError::Canceled`]）。
pub async fn put_file(
    connection: &Arc<Connection>,
    local: &Path,
    remote: &str,
    single_channel: bool,
    stop: impl Future<Output = ()>,
) -> Result<Transferred<String>, SftpError> {
    let cancel = Cancel::new();
    tracing::info!(host = %connection.label(), local = %local.display(), remote, "命令行上传开始");
    let work = put_inner(connection, local, remote, single_channel, &cancel);
    let result = until_stopped(work, &cancel, stop).await;
    match &result {
        Ok(done) => tracing::info!(
            host = %connection.label(),
            remote = %done.path,
            bytes = done.bytes,
            protocol = ?done.protocol,
            "命令行上传完成"
        ),
        Err(e) => tracing::warn!(host = %connection.label(), remote, "命令行上传失败：{e}"),
    }
    result
}

async fn put_inner(
    connection: &Arc<Connection>,
    local: &Path,
    remote: &str,
    single_channel: bool,
    cancel: &Cancel,
) -> Result<Transferred<String>, SftpError> {
    let meta = tokio::fs::metadata(local)
        .await
        .map_err(|e| error::local(local, &e))?;
    if !meta.is_file() {
        return Err(Rejection::NotRegularFile.into());
    }
    let backend = open_backend(connection, Encoding::Utf8, single_channel).await?;
    let result = put_with(&backend, local, remote, &meta, cancel).await;
    backend.close();
    result
}

async fn put_with(
    backend: &Backend,
    local: &Path,
    remote: &str,
    meta: &std::fs::Metadata,
    cancel: &Cancel,
) -> Result<Transferred<String>, SftpError> {
    let name = local
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut target = remote.to_string();
    if target.is_empty()
        || target.ends_with('/')
        || backend.stat(&target).await.is_ok_and(|a| is_dir(&a))
    {
        target = path::join(&target, &name);
    }
    // 先写入临时文件再改名：传输失败时不破坏远程已有的同名文件；不继续传输，失败时删除临时文件
    let t = standalone(Direction::Upload, true);
    let result = match backend {
        Backend::Sftp(client) => {
            upload_file(None, client, &t, cancel, local, &target, meta, false).await
        }
        Backend::Scp(scp) => {
            super::transfer::scp::upload_file(None, scp, &t, cancel, local, &target, meta, false)
                .await
        }
    };
    if let Err(e) = result {
        let part = t
            .lock()
            .part
            .as_ref()
            .and_then(|p| p.remote_part().map(str::to_string));
        if let Some(part) = part {
            let _ = backend.remove(&part).await;
        }
        return Err(e.into_inner());
    }
    let resolved = match backend {
        Backend::Sftp(client) => client.realpath(&target).await.ok(),
        // SCP 模式只能解析目录：解析所在的目录再接上文件名
        Backend::Scp(scp) => scp
            .realpath_dir(&path::parent(&target))
            .await
            .ok()
            .map(|dir| path::join(&dir, &path::base_name(&target))),
    };
    Ok(Transferred {
        path: resolved.unwrap_or(target),
        bytes: t.done.load(Ordering::Relaxed),
        protocol: backend.protocol(),
    })
}

/// 下载远程文件。`local` 是已存在的目录时放入该目录，否则作为目标文件名（上级目录必须存在）。`single_channel` 同
/// [`put_file`]。返回实际写入的本机路径、字节数与用的协议。`stop` 完成时取消（删除本机的临时文件后返回
/// [`SftpError::Canceled`]）。
pub async fn get_file(
    connection: &Arc<Connection>,
    remote: &str,
    local: &Path,
    single_channel: bool,
    stop: impl Future<Output = ()>,
) -> Result<Transferred<PathBuf>, SftpError> {
    let cancel = Cancel::new();
    tracing::info!(host = %connection.label(), remote, local = %local.display(), "命令行下载开始");
    let work = get_inner(connection, remote, local, single_channel, &cancel);
    let result = until_stopped(work, &cancel, stop).await;
    match &result {
        Ok(done) => tracing::info!(
            host = %connection.label(),
            local = %done.path.display(),
            bytes = done.bytes,
            protocol = ?done.protocol,
            "命令行下载完成"
        ),
        Err(e) => tracing::warn!(host = %connection.label(), remote, "命令行下载失败：{e}"),
    }
    result
}

async fn get_inner(
    connection: &Arc<Connection>,
    remote: &str,
    local: &Path,
    single_channel: bool,
    cancel: &Cancel,
) -> Result<Transferred<PathBuf>, SftpError> {
    let backend = open_backend(connection, Encoding::Utf8, single_channel).await?;
    let result = get_with(&backend, remote, local, cancel).await;
    backend.close();
    result
}

async fn get_with(
    backend: &Backend,
    remote: &str,
    local: &Path,
    cancel: &Cancel,
) -> Result<Transferred<PathBuf>, SftpError> {
    let attrs = backend
        .stat(remote)
        .await
        .map_err(|e| error::remote(remote, &e))?;
    if !is_regular(&attrs) {
        return Err(Rejection::NotRegularFile.into());
    }
    let target = match tokio::fs::metadata(local).await {
        Ok(meta) if meta.is_dir() => local.join(safe_name(&path::base_name(remote))),
        _ => {
            let parent = local.parent().unwrap_or(Path::new("."));
            tokio::fs::metadata(parent)
                .await
                .map_err(|e| error::local(parent, &e))?;
            local.to_path_buf()
        }
    };
    let t = standalone(Direction::Download, true);
    let source = Source::of(backend);
    match download_from(source, &t, cancel, remote, &target, &attrs, false).await {
        Ok(saved) => Ok(Transferred {
            path: saved,
            bytes: t.done.load(Ordering::Relaxed),
            protocol: backend.protocol(),
        }),
        Err(e) => {
            // 只删本次建的临时文件（没建成时什么也不删）
            let part = t.lock().part.as_ref().map(|p| p.part.clone());
            if let Some(PartPath::Local(part)) = part {
                let _ = tokio::fs::remove_file(part).await;
            }
            Err(match e {
                FileError::Unreadable(e) | FileError::Error(e) => e,
            })
        }
    }
}
