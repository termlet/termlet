//! 单文件传输，供 `termlet` 命令行工具的 upload / download（以及 MCP 的 `upload_file` / `download_file`）使用
//! （scp 的语义：覆盖已存在的同名文件）。经已有的 SSH 连接临时打开 SFTP，传完即关闭；失败、取消时删除临时文件
//! （不继续传输）。临时文件用一次性的随机名称，不与桌面端同时传输同一个文件时的临时文件冲突。
//!
//! 服务器拒绝了 SFTP 子系统（没有开 sftp-server、堡垒机限制）时与桌面端的文件面板一样改用 SCP 模式（见 `scp` 模块）：
//! 结果中的 [`Protocol`] 为 [`Protocol::Scp`]。命令行本来就不继续传输，SCP 模式的限制（没有断点续传）对它没有影响；
//! 连 scp 也没有、主机只用一个通道等不能改用时返回 [`SftpError::NoSftp`]。
//!
//! MCP 的局域网传输链接（经 HTTP 收发，这台电脑上不落文件）用另外三个：[`probe`] 在给出链接之前看一眼服务器上的路径，
//! [`put_stream`] 把请求的正文写到服务器上，[`open_reader`] 打开服务器上的文件、写进回答的正文。

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};

use super::backend::{Backend, Protocol};
use super::cancel::Cancel;
use super::client::{PipeError, is_dir, is_regular, size_of};
use super::error::{self, Rejection, SftpError, Side, status_of};
use super::manager::open_backend;
use super::names::safe_name;
use super::path;
use super::scp::{ScpFailure, scp_error};
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

/// 给出传输链接之前看到的服务器上的路径。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    /// 普通文件（符号链接按它指向的看）：下载时是它现在的大小，上传时会被覆盖
    File { size: u64 },
    /// 还不存在，所在的目录在（只有上传时）
    Missing,
    /// 是目录
    Directory,
}

/// 看一眼服务器上的 `remote`（MCP 的局域网传输链接在给出之前检查，免得对方传了才发现路径不对）：下载的要存在；
/// 上传的不存在时所在的目录要在。是目录时返回 [`Probe::Directory`]，由调用方按方向说明；别的特殊文件为
/// [`Rejection::NotRegularFile`]。
pub async fn probe(
    connection: &Arc<Connection>,
    remote: &str,
    upload: bool,
    single_channel: bool,
) -> Result<Probe, SftpError> {
    let backend = open_backend(connection, Encoding::Utf8, single_channel).await?;
    let result = probe_with(&backend, remote, upload).await;
    backend.close();
    result
}

async fn probe_with(backend: &Backend, remote: &str, upload: bool) -> Result<Probe, SftpError> {
    match backend.stat(remote).await {
        Ok(attrs) if is_dir(&attrs) => Ok(Probe::Directory),
        Ok(attrs) if is_regular(&attrs) => Ok(Probe::File {
            size: size_of(&attrs),
        }),
        Ok(_) => Err(Rejection::NotRegularFile.into()),
        Err(e) if upload && status_of(&e) == Some(StatusCode::NoSuchFile) => {
            let parent = path::parent(remote);
            match backend.stat(&parent).await {
                Ok(attrs) if is_dir(&attrs) => Ok(Probe::Missing),
                Ok(_) => Err(SftpError::NotFound {
                    side: Side::Remote,
                    path: parent,
                }),
                Err(e) => Err(error::remote(&parent, &e)),
            }
        }
        Err(e) => Err(error::remote(remote, &e)),
    }
}

/// 把 `reader` 中正好 `size` 字节（HTTP 请求的正文，长度来自 Content-Length）写成服务器上的 `remote`（文件的完整
/// 路径，已有的文件被覆盖）：先写同目录下的一次性临时文件，收齐后改名；没收齐（对方断开、停住）、出错、取消时删除
/// 临时文件，服务器上的文件保持原样。`reader` 那一端的问题报告为 [`Side::Local`] 的 [`SftpError::Failed`]。
/// `progress` 加上确认写入的字节数。`stop` 完成时取消。返回实际写入的路径、字节数与用的协议。
pub async fn put_stream<R: AsyncRead + Unpin>(
    connection: &Arc<Connection>,
    reader: &mut R,
    size: u64,
    remote: &str,
    single_channel: bool,
    progress: &AtomicU64,
    stop: impl Future<Output = ()>,
) -> Result<Transferred<String>, SftpError> {
    let cancel = Cancel::new();
    tracing::info!(host = %connection.label(), remote, size, "经传输链接上传开始");
    let work = async {
        let backend = open_backend(connection, Encoding::Utf8, single_channel).await?;
        let written = match &backend {
            Backend::Sftp(client) => {
                super::transfer::upload::upload_stream(
                    client, &cancel, reader, size, remote, progress,
                )
                .await
            }
            Backend::Scp(scp) => {
                super::transfer::scp::upload_stream(scp, &cancel, reader, size, remote, progress)
                    .await
            }
        };
        let result = match written {
            Ok(target) => {
                let resolved = match &backend {
                    Backend::Sftp(client) => client.realpath(&target).await.ok(),
                    Backend::Scp(scp) => scp
                        .realpath_dir(&path::parent(&target))
                        .await
                        .ok()
                        .map(|dir| path::join(&dir, &path::base_name(&target))),
                };
                Ok(Transferred {
                    path: resolved.unwrap_or(target),
                    bytes: size,
                    protocol: backend.protocol(),
                })
            }
            Err(e) => Err(e),
        };
        backend.close();
        result
    };
    let result = until_stopped(work, &cancel, stop).await;
    match &result {
        Ok(done) => tracing::info!(
            host = %connection.label(),
            remote = %done.path,
            bytes = done.bytes,
            protocol = ?done.protocol,
            "经传输链接上传完成"
        ),
        Err(e) => tracing::warn!(host = %connection.label(), remote, "经传输链接上传失败：{e}"),
    }
    result
}

/// 打开服务器上的 `remote`（要是普通文件），取得它现在的大小（HTTP 回答的 Content-Length），之后用
/// [`RemoteReader::send_to`] 写进回答的正文。
pub async fn open_reader(
    connection: &Arc<Connection>,
    remote: &str,
    single_channel: bool,
) -> Result<RemoteReader, SftpError> {
    let backend = open_backend(connection, Encoding::Utf8, single_channel).await?;
    let attrs = match backend.stat(remote).await {
        Ok(attrs) => attrs,
        Err(e) => {
            backend.close();
            return Err(error::remote(remote, &e));
        }
    };
    if !is_regular(&attrs) {
        backend.close();
        return Err(Rejection::NotRegularFile.into());
    }
    let size = size_of(&attrs);
    tracing::info!(host = %connection.label(), remote, size, "经传输链接下载开始");
    Ok(RemoteReader {
        backend,
        host: connection.label().to_string(),
        remote: remote.to_string(),
        size,
    })
}

/// 打开了的服务器上的文件（经 HTTP 发出）。丢弃时关闭 SFTP（或 SCP）。
pub struct RemoteReader {
    backend: Backend,
    /// 主机（运行日志用）
    host: String,
    remote: String,
    size: u64,
}

impl RemoteReader {
    /// 打开时的大小：只发这么多。
    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn protocol(&self) -> Protocol {
        self.backend.protocol()
    }

    /// 读出文件写进 `sink`，正好 [`Self::size`] 字节（对方按 Content-Length 判断收齐没有）：文件在打开之后变长的
    /// 部分不发；变短了时失败。`sink` 那一端的问题（对方断开、停住）报告为 [`Side::Local`] 的
    /// [`SftpError::Failed`]。`progress` 加上发出的字节数。`stop` 完成时取消。返回发出的字节数。
    pub async fn send_to<W: AsyncWrite + Unpin>(
        &self,
        sink: &mut W,
        progress: &AtomicU64,
        stop: impl Future<Output = ()>,
    ) -> Result<u64, SftpError> {
        let cancel = Cancel::new();
        let remote = self.remote.as_str();
        let work = async {
            let mut capped = Capped {
                inner: &mut *sink,
                left: self.size,
            };
            let read = match &self.backend {
                Backend::Sftp(client) => {
                    let handle = client
                        .open(remote, OpenFlags::READ, FileAttributes::empty())
                        .await
                        .map_err(|e| error::remote(remote, &e))?;
                    let read = client
                        .read_into(&handle, 0, &mut capped, &cancel, progress, None)
                        .await;
                    let _ = client.close_handle(&handle).await;
                    read.map(|_| ()).map_err(|f| match f.error {
                        PipeError::Remote(e) => error::remote(remote, &e),
                        PipeError::Local(e) => {
                            SftpError::failed(Side::Local, remote, e.to_string())
                        }
                        PipeError::Canceled => SftpError::Canceled,
                        PipeError::TooLarge => SftpError::failed(Side::Remote, remote, "超过上限"),
                    })
                }
                Backend::Scp(scp) => scp
                    .download(remote, &mut capped, &cancel, progress, None)
                    .await
                    .map(|_| ())
                    .map_err(|f| match f {
                        ScpFailure::Local(e) => {
                            SftpError::failed(Side::Local, remote, e.to_string())
                        }
                        other => scp_error(remote, other),
                    }),
            };
            // 发够了：写满之后的错误是让读的一方停下（文件在打开之后变长了），不算失败
            let left = capped.left;
            if left == 0 {
                sink.flush()
                    .await
                    .map_err(|e| SftpError::failed(Side::Local, remote, e.to_string()))?;
                return Ok(self.size);
            }
            read?;
            Err(SftpError::failed(
                Side::Remote,
                remote,
                format!(
                    "文件在下载期间变短了（打开时 {} 字节，只读到 {}）",
                    self.size,
                    self.size - left
                ),
            ))
        };
        let result = until_stopped(work, &cancel, stop).await;
        match &result {
            Ok(bytes) => tracing::info!(host = %self.host, remote, bytes, "经传输链接下载完成"),
            Err(e) => tracing::warn!(host = %self.host, remote, "经传输链接下载失败：{e}"),
        }
        result
    }
}

impl Drop for RemoteReader {
    fn drop(&mut self) {
        self.backend.close();
    }
}

/// 最多写 `left` 字节的写入端：写满后再写就出错，让读的一方停下。
struct Capped<'a, W> {
    inner: &'a mut W,
    left: u64,
}

impl<W: AsyncWrite + Unpin> AsyncWrite for Capped<'_, W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.left == 0 && !buf.is_empty() {
            return Poll::Ready(Err(io::Error::other("已写够打开时的大小")));
        }
        let take = buf
            .len()
            .min(usize::try_from(this.left).unwrap_or(usize::MAX));
        let written = Pin::new(&mut *this.inner).poll_write(cx, &buf[..take]);
        if let Poll::Ready(Ok(n)) = written {
            this.left -= n as u64;
        }
        written
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 写满打开时的大小后再写就出错，之前的照常写进去（分几次写也一样）
    #[test]
    fn capped_writes_stop_at_the_size() {
        crate::testkit::block_on(async {
            let mut out = Vec::new();
            let mut capped = Capped {
                inner: &mut out,
                left: 5,
            };
            capped.write_all(b"abc").await.unwrap();
            assert!(capped.write_all(b"defg").await.is_err(), "超出的部分");
            assert_eq!(capped.left, 0);
            assert!(capped.write_all(b"h").await.is_err());
            capped.write_all(b"").await.unwrap();
            assert_eq!(out, b"abcde");
        });
    }
}
