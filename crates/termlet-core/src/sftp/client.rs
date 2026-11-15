//! SFTP 客户端：在任意字节流（SSH 的 sftp 子系统通道、WSL 中的服务程序、测试中的内存管道）上收发请求。
//! 文件名在客户端与字节流之间按服务器的编码转换（见 [`super::codec`]）。
//!
//! 直接用 russh-sftp 的 [`RawSftpSession`]：并发的读写请求自己调度（上传出错后要知道哪些块已经确认写入）、
//! 保留服务器的状态码（区分不存在、没有权限、不支持）、posix-rename 扩展。流结束（连接断开、服务器上的
//! SFTP 子系统退出）时 [`SftpClient::closed`] 完成。
//!
//! 必须在 tokio 运行时中建立（russh-sftp 在当前运行时上开收发任务）。

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use futures::StreamExt;
use futures::stream::{FuturesOrdered, FuturesUnordered};
use russh_sftp::client::error::Error as ClientError;
use russh_sftp::client::rawsession::Limits;
use russh_sftp::client::{Config, RawSftpSession};
use russh_sftp::protocol::{FileAttributes, OpenFlags, Packet, Status, StatusCode};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::watch;

use super::SftpError;
use super::cancel::Cancel;
use super::error::status_of;
use crate::model::Encoding;

/// 一个读写请求的数据量。
const CHUNK: usize = 32 * 1024;
/// 同时在途的读写请求数：高延迟的链路上也能跑满带宽。
const DEPTH: usize = 64;
/// 单个请求等待回应的上限。在途的数据最多 2 MB，慢到每秒十几 KB 的链路也不会误判超时；
/// 连接卡住时通常由 SSH 保活更早发现。
const REQUEST_TIMEOUT_SECS: u64 = 120;
/// 建立 SFTP 会话（协商版本）的上限：服务器拒绝了子系统时通道不会有回应。
const INIT_TIMEOUT: Duration = Duration::from_secs(30);
const POSIX_RENAME: &str = "posix-rename@openssh.com";
/// 版本协商前服务器就关闭了通道（拒绝了子系统，或服务器上没有 sftp-server 程序）时的说明。
pub(crate) const CHANNEL_CLOSED: &str = "服务器关闭了 SFTP 通道";
const STATVFS: &str = "statvfs@openssh.com";
const LIMITS: &str = "limits@openssh.com";

pub(crate) type ClientResult<T> = Result<T, ClientError>;

/// 目录中的一项（不含 `.` 与 `..`）。
pub(crate) struct DirItem {
    pub name: String,
    /// 服务器给的 `ls -l` 式描述（OpenSSH 的 sftp-server 带有属主、属组的名称）
    pub longname: String,
    /// 不跟随符号链接的属性
    pub attrs: FileAttributes,
}

/// 并发读写中断的位置与原因。
pub(crate) struct PipeFailure {
    /// 从起点到这里的数据已经确认写入（上传时之后的块可能也写了，但中间有空洞）
    pub safe: u64,
    pub error: PipeError,
}

pub(crate) enum PipeError {
    Canceled,
    /// 远程的读写失败
    Remote(ClientError),
    /// 本机的读写失败
    Local(io::Error),
    /// 超过读取的上限
    TooLarge,
}

pub(crate) struct SftpClient {
    raw: RawSftpSession,
    extensions: HashMap<String, String>,
    read_len: u32,
    write_len: usize,
    lost: watch::Receiver<bool>,
}

impl SftpClient {
    /// 在字节流上建立 SFTP 会话；服务器上的文件名是 `encoding` 编码的。
    pub(crate) async fn connect<S>(stream: S, encoding: Encoding) -> Result<Self, SftpError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (lost_tx, lost) = watch::channel(false);
        let stream = Watched {
            inner: super::codec::wrap(stream, encoding),
            lost: Arc::new(lost_tx),
        };
        let mut raw = RawSftpSession::new_with_config(
            stream,
            Config {
                request_timeout_secs: REQUEST_TIMEOUT_SECS,
                ..Config::default()
            },
        );
        let mut ended = lost.clone();
        let init = async {
            tokio::select! {
                biased;
                result = raw.init() => result.map_err(|e| SftpError::Unavailable(e.to_string())),
                // 服务器拒绝了子系统：通道随即关闭
                _ = ended.wait_for(|v| *v) => {
                    Err(SftpError::Unavailable(CHANNEL_CLOSED.into()))
                }
            }
        };
        let version = match tokio::time::timeout(INIT_TIMEOUT, init).await {
            Ok(result) => result?,
            Err(_) => {
                return Err(SftpError::Unavailable(
                    "服务器没有回应 SFTP 的版本协商".into(),
                ));
            }
        };
        let extensions = version.extensions;
        let mut read_len = CHUNK as u32;
        let mut write_len = CHUNK;
        if extensions.get(LIMITS).is_some_and(|v| v == "1")
            && let Ok(Ok(limits)) = tokio::time::timeout(INIT_TIMEOUT, raw.limits()).await
        {
            let limits = Limits::from(limits);
            if let Some(n) = limits.read_len {
                read_len = read_len.min(n.min(u32::MAX as u64) as u32).max(1);
            }
            if let Some(n) = limits.write_len {
                write_len = write_len.min(n.min(usize::MAX as u64) as usize).max(1);
            }
            raw.set_limits(limits);
        }
        Ok(Self {
            raw,
            extensions,
            read_len,
            write_len,
            lost,
        })
    }

    /// 执行一个请求；流已经结束时立即失败（russh-sftp 在连接断开的瞬间发出的请求可能永远等不到回应，只能等超时）。
    async fn guard<T>(&self, request: impl Future<Output = ClientResult<T>>) -> ClientResult<T> {
        tokio::select! {
            biased;
            result = request => result,
            _ = self.closed() => Err(lost()),
        }
    }

    /// 等到流结束。
    pub(crate) async fn closed(&self) {
        let mut lost = self.lost.clone();
        let _ = lost.wait_for(|v| *v).await;
    }

    /// 关闭会话（服务器上的子系统随之退出）。
    pub(crate) fn close(&self) {
        let _ = self.raw.close_session();
    }

    pub(crate) fn has_extension(&self, name: &str) -> bool {
        self.extensions.contains_key(name)
    }

    /// posix-rename 失败是不是因为服务器不支持这个扩展：返回"不支持的操作"，或者服务器没有声明支持它。
    /// 只有这时才能退回"经备份替换"；别的原因（没有权限、临时故障）失败后再动原文件，原文件可能就丢了。
    pub(crate) fn rename_unsupported(&self, error: &ClientError) -> bool {
        status_of(error) == Some(StatusCode::OpUnsupported) || !self.has_extension(POSIX_RENAME)
    }

    pub(crate) async fn realpath(&self, path: &str) -> ClientResult<String> {
        let name = self.guard(self.raw.realpath(path)).await?;
        name.files
            .into_iter()
            .next()
            .map(|f| f.filename)
            .ok_or_else(|| ClientError::UnexpectedBehavior("realpath 没有返回路径".into()))
    }

    /// 属性（跟随符号链接）。
    pub(crate) async fn stat(&self, path: &str) -> ClientResult<FileAttributes> {
        Ok(self.guard(self.raw.stat(path)).await?.attrs)
    }

    /// 属性（不跟随符号链接）。
    pub(crate) async fn lstat(&self, path: &str) -> ClientResult<FileAttributes> {
        Ok(self.guard(self.raw.lstat(path)).await?.attrs)
    }

    pub(crate) async fn fstat(&self, handle: &str) -> ClientResult<FileAttributes> {
        Ok(self.guard(self.raw.fstat(handle)).await?.attrs)
    }

    pub(crate) async fn setstat(&self, path: &str, attrs: FileAttributes) -> ClientResult<()> {
        self.guard(self.raw.setstat(path, attrs)).await.map(|_| ())
    }

    pub(crate) async fn fsetstat(&self, handle: &str, attrs: FileAttributes) -> ClientResult<()> {
        self.guard(self.raw.fsetstat(handle, attrs))
            .await
            .map(|_| ())
    }

    pub(crate) async fn read_dir(&self, path: &str) -> ClientResult<Vec<DirItem>> {
        let handle = self.guard(self.raw.opendir(path)).await?.handle;
        let mut items = Vec::new();
        let result = loop {
            match self.guard(self.raw.readdir(handle.as_str())).await {
                Ok(name) => items.extend(
                    name.files
                        .into_iter()
                        .filter(|f| {
                            let ok = entry_name_ok(&f.filename);
                            if !ok && f.filename != "." && f.filename != ".." {
                                tracing::warn!(dir = %path, name = ?f.filename, "服务器返回了不合法的文件名，跳过");
                            }
                            ok
                        })
                        .map(|f| DirItem {
                            name: f.filename,
                            longname: f.longname,
                            attrs: f.attrs,
                        }),
                ),
                Err(e) if status_of(&e) == Some(StatusCode::Eof) => break Ok(()),
                Err(e) => break Err(e),
            }
        };
        let _ = self.guard(self.raw.close(handle)).await;
        result.map(|()| items)
    }

    pub(crate) async fn mkdir(&self, path: &str) -> ClientResult<()> {
        self.guard(self.raw.mkdir(path, FileAttributes::empty()))
            .await
            .map(|_| ())
    }

    pub(crate) async fn rmdir(&self, path: &str) -> ClientResult<()> {
        self.guard(self.raw.rmdir(path)).await.map(|_| ())
    }

    pub(crate) async fn remove(&self, path: &str) -> ClientResult<()> {
        self.guard(self.raw.remove(path)).await.map(|_| ())
    }

    /// 普通的改名（SFTP v3：目标已存在时失败）。
    pub(crate) async fn rename(&self, from: &str, to: &str) -> ClientResult<()> {
        self.guard(self.raw.rename(from, to)).await.map(|_| ())
    }

    /// 覆盖已有文件的原子改名（posix-rename@openssh.com）；服务器不支持时返回"不支持的操作"。
    pub(crate) async fn posix_rename(&self, from: &str, to: &str) -> ClientResult<()> {
        if !self.has_extension(POSIX_RENAME) {
            return Err(unsupported());
        }
        let mut data = Vec::with_capacity(8 + from.len() + to.len());
        for text in [from, to] {
            data.extend_from_slice(&(text.len() as u32).to_be_bytes());
            data.extend_from_slice(text.as_bytes());
        }
        match self.guard(self.raw.extended(POSIX_RENAME, data)).await? {
            Packet::Status(status) if status.status_code == StatusCode::Ok => Ok(()),
            Packet::Status(status) => Err(ClientError::Status(status)),
            _ => Err(ClientError::UnexpectedPacket),
        }
    }

    /// 新建指向 `target` 的符号链接 `link`（测试中准备远程的文件）。按 OpenSSH 的参数顺序（与协议草案相反：先目标、后链接）。
    #[cfg(test)]
    pub(crate) async fn symlink(&self, target: &str, link: &str) -> ClientResult<()> {
        self.guard(self.raw.symlink(target, link)).await.map(|_| ())
    }

    /// `path` 所在文件系统的可用空间（字节）；服务器不支持 statvfs 扩展时为 `None`。
    pub(crate) async fn free_space(&self, path: &str) -> Option<u64> {
        if self.extensions.get(STATVFS).is_none_or(|v| v != "2") {
            return None;
        }
        let info = self.guard(self.raw.statvfs(path)).await.ok()?;
        Some(info.blocks_avail.saturating_mul(info.fragment_size.max(1)))
    }

    pub(crate) async fn open(
        &self,
        path: &str,
        flags: OpenFlags,
        attrs: FileAttributes,
    ) -> ClientResult<String> {
        Ok(self.guard(self.raw.open(path, flags, attrs)).await?.handle)
    }

    pub(crate) async fn close_handle(&self, handle: &str) -> ClientResult<()> {
        self.guard(self.raw.close(handle)).await.map(|_| ())
    }

    /// 把 `source` 的内容从偏移 `start` 起写入远程文件 `handle`，最多 [`DEPTH`] 个写请求同时在途。
    /// 每确认写入一块就加到 `progress`。返回写到的位置；出错时停止读取、等在途的请求都有结果，
    /// 报告确认写入的连续部分的末尾（之后的块可能也写了，但中间有空洞）。
    pub(crate) async fn write_from<R: AsyncRead + Unpin>(
        &self,
        handle: &str,
        start: u64,
        source: &mut R,
        stop: &Cancel,
        progress: &AtomicU64,
    ) -> Result<u64, PipeFailure> {
        let raw = &self.raw;
        let mut next = start;
        let mut inflight = FuturesUnordered::new();
        // 已发出、还没确认写入的块（失败的也留在这里）
        let mut unconfirmed = BTreeSet::new();
        let mut error: Option<PipeError> = None;
        let mut eof = false;
        loop {
            while error.is_none() && !eof && inflight.len() < DEPTH {
                if stop.is_canceled() {
                    return Err(PipeFailure {
                        safe: start,
                        error: PipeError::Canceled,
                    });
                }
                let mut buf = vec![0u8; self.write_len];
                match read_full(source, &mut buf).await {
                    Ok(n) => {
                        if n < buf.len() {
                            eof = true;
                        }
                        if n > 0 {
                            buf.truncate(n);
                            let offset = next;
                            next += n as u64;
                            unconfirmed.insert(offset);
                            inflight.push(async move {
                                let result = raw.write(handle, offset, buf).await;
                                (offset, n, result)
                            });
                        }
                    }
                    Err(e) => error = Some(PipeError::Local(e)),
                }
            }
            if inflight.is_empty() {
                break;
            }
            let item = tokio::select! {
                item = inflight.next() => item,
                _ = stop.cancelled() => {
                    return Err(PipeFailure { safe: start, error: PipeError::Canceled });
                }
                // 在途的请求可能等不到回应：确认写入的只到最早的在途块之前
                _ = self.closed() => {
                    error.get_or_insert(PipeError::Remote(lost()));
                    break;
                }
            };
            let Some((offset, n, result)) = item else {
                break;
            };
            match result {
                Ok(_) => {
                    unconfirmed.remove(&offset);
                    progress.fetch_add(n as u64, Ordering::Relaxed);
                }
                Err(e) => {
                    error.get_or_insert(PipeError::Remote(e));
                }
            }
        }
        match error {
            None => Ok(next),
            Some(error) => Err(PipeFailure {
                safe: unconfirmed.first().copied().unwrap_or(next),
                error,
            }),
        }
    }

    /// 从偏移 `start` 起读出远程文件 `handle` 的内容写进 `sink`（按顺序），最多 [`DEPTH`] 个读请求同时在途。
    /// 写进 `sink` 的字节数加到 `progress`；`limit` 是总长度（从 0 算起）的上限。返回读到的末尾。
    /// 出错时报告已经写进 `sink` 的连续部分的末尾（调用方负责 flush）。
    pub(crate) async fn read_into<W: AsyncWrite + Unpin>(
        &self,
        handle: &str,
        start: u64,
        sink: &mut W,
        stop: &Cancel,
        progress: &AtomicU64,
        limit: Option<u64>,
    ) -> Result<u64, PipeFailure> {
        let raw = &self.raw;
        let len = self.read_len;
        let mut next = start;
        let mut written = start;
        let mut pending = FuturesOrdered::new();
        let fail = |safe, error| PipeFailure { safe, error };
        let read_at = |offset: u64, want: u32| async move {
            (offset, want, raw.read(handle, offset, want).await)
        };
        loop {
            while pending.len() < DEPTH {
                let offset = next;
                next += u64::from(len);
                pending.push_back(read_at(offset, len));
            }
            let item = tokio::select! {
                item = pending.next() => item,
                _ = stop.cancelled() => return Err(fail(written, PipeError::Canceled)),
                _ = self.closed() => return Err(fail(written, PipeError::Remote(lost()))),
            };
            let Some((offset, want, result)) = item else {
                break;
            };
            if offset != written {
                continue;
            }
            match result {
                Ok(data) if data.data.is_empty() => break,
                Ok(data) => {
                    let n = data.data.len() as u64;
                    if n > u64::from(want) {
                        // 回应比请求的长：之后的偏移都对不上
                        return Err(fail(
                            written,
                            PipeError::Remote(ClientError::UnexpectedBehavior(
                                "服务器返回的数据比请求的多".into(),
                            )),
                        ));
                    }
                    if limit.is_some_and(|limit| written + n > limit) {
                        return Err(fail(written, PipeError::TooLarge));
                    }
                    if let Err(e) = sink.write_all(&data.data).await {
                        return Err(fail(written, PipeError::Local(e)));
                    }
                    written += n;
                    progress.fetch_add(n, Ordering::Relaxed);
                    if n < u64::from(want) {
                        // 读到的比请求的少（服务器限制了单次读取的长度，或到了文件末尾）：只补上缺口，
                        // 在途的其余请求照常使用（全部丢掉重发会让下行流量成倍增加）
                        pending.push_front(read_at(written, want - n as u32));
                    }
                }
                Err(e) if status_of(&e) == Some(StatusCode::Eof) => break,
                Err(e) => return Err(fail(written, PipeError::Remote(e))),
            }
            if stop.is_canceled() {
                return Err(fail(written, PipeError::Canceled));
            }
        }
        if let Err(e) = sink.flush().await {
            return Err(fail(written, PipeError::Local(e)));
        }
        Ok(written)
    }
}

/// 连接已断开（本机判断出来的）。
fn lost() -> ClientError {
    ClientError::UnexpectedBehavior("SFTP 连接已断开".into())
}

/// 服务器不支持的操作（本机判断出来的）。
fn unsupported() -> ClientError {
    ClientError::Status(Status {
        id: 0,
        status_code: StatusCode::OpUnsupported,
        error_message: "服务器不支持 posix-rename".into(),
        language_tag: "zh-CN".into(),
    })
}

/// 读满 `buf`，读到文件末尾时返回实际读到的长度。
async fn read_full<R: AsyncRead + Unpin>(source: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = source.read(&mut buf[filled..]).await?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

/// 包住底层的流：读到结束、出错或被丢弃时报告"已断开"。
struct Watched<S> {
    inner: S,
    lost: Arc<watch::Sender<bool>>,
}

impl<S> Drop for Watched<S> {
    fn drop(&mut self) {
        self.lost.send_replace(true);
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Watched<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let wanted = buf.remaining() > 0;
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        match &result {
            Poll::Ready(Ok(())) if wanted && buf.filled().len() == before => {
                self.lost.send_replace(true);
            }
            Poll::Ready(Err(_)) => {
                self.lost.send_replace(true);
            }
            _ => {}
        }
        result
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Watched<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Err(_)) = &result {
            self.lost.send_replace(true);
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// 文件类型位。
pub(crate) const S_IFMT: u32 = 0o170000;
pub(crate) const S_IFDIR: u32 = 0o040000;
pub(crate) const S_IFREG: u32 = 0o100000;
pub(crate) const S_IFLNK: u32 = 0o120000;

/// 属性中的文件类型（russh-sftp 自带的判断把块设备当成目录、把符号链接当成普通文件，不能用）。
pub(crate) fn file_type(attrs: &FileAttributes) -> u32 {
    attrs.permissions.unwrap_or(S_IFREG) & S_IFMT
}

pub(crate) fn is_dir(attrs: &FileAttributes) -> bool {
    file_type(attrs) == S_IFDIR
}

/// 目录项的名称能否使用：不是 `.` `..`、不为空、不含 `/` 与 NUL。恶意的服务器返回 `x/..`、`../../etc` 这样的名称时，
/// 拼成路径会让目录下载不停地遍历、递归删除越出所选的目录。
pub(crate) fn entry_name_ok(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0'])
}

pub(crate) fn is_regular(attrs: &FileAttributes) -> bool {
    file_type(attrs) == S_IFREG
}

pub(crate) fn is_symlink(attrs: &FileAttributes) -> bool {
    file_type(attrs) == S_IFLNK
}

/// 权限位（含 setuid、setgid、粘滞位）。
pub(crate) fn perm_bits(attrs: &FileAttributes) -> u32 {
    attrs.permissions.unwrap_or(0) & 0o7777
}

pub(crate) fn size_of(attrs: &FileAttributes) -> u64 {
    attrs.size.unwrap_or(0)
}

pub(crate) fn mtime_of(attrs: &FileAttributes) -> i64 {
    attrs.mtime.map(i64::from).unwrap_or(0)
}

pub(crate) fn perm_attrs(perm: u32) -> FileAttributes {
    FileAttributes {
        permissions: Some(perm & 0o7777),
        ..FileAttributes::empty()
    }
}

pub(crate) fn size_attrs(size: u64) -> FileAttributes {
    FileAttributes {
        size: Some(size),
        ..FileAttributes::empty()
    }
}

/// 修改时间（访问时间设为同一个值）。
pub(crate) fn time_attrs(mtime: i64) -> FileAttributes {
    let t = mtime.clamp(0, u32::MAX as i64) as u32;
    FileAttributes {
        atime: Some(t),
        mtime: Some(t),
        ..FileAttributes::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::entry_name_ok;

    #[test]
    fn entry_names_with_separators_are_rejected() {
        for ok in ["a.txt", ".bashrc", "...", "名 字", r"a\b"] {
            assert!(entry_name_ok(ok), "{ok:?}");
        }
        for bad in ["", ".", "..", "x/..", "../../etc", "a\0b"] {
            assert!(!entry_name_ok(bad), "{bad:?}");
        }
    }
}
