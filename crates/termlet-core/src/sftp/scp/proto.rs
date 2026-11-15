//! SCP 协议（rcp 以来的老协议，OpenSSH 的 `scp -t` / `scp -f` 两端仍在用）：在 exec 通道上运行服务器的 scp，
//! 按行交换文件头，内容紧跟其后。
//!
//! - 应答：一个字节，0 是好，1 是出错（后面一行是说明），2 是严重错误（同上，随后断开）。
//! - 上传（服务器是接收方，`scp -t 目标`）：服务器先回 0；客户端发 `T<修改时间> 0 <访问时间> 0`（服务器据此设置时间）、
//!   `C<四位八进制权限> <大小> <名称>`，各等一个应答，再发内容与一个 0，等最后一个应答。目标不是目录时文件写到目标本身，
//!   头中的名称不用。
//! - 下载（服务器是来源，`scp -f 路径`）：客户端先发 0；服务器发 `C…` 头（前面可能有 `T…`），客户端回 0 后服务器发内容
//!   与一个应答字节，客户端再回 0。
//!
//! 只传单个文件（目录由调用方逐个处理），不支持从中间接着传。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use russh::client::Msg;
use russh::{Channel, ChannelMsg};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::sftp::cancel::Cancel;
use crate::ssh::Connection;

/// 文件头一行的上限。
const MAX_LINE: usize = 64 * 1024;
/// 等一个应答、一行文件头的上限（内容的传输不限时，连接卡住由保活发现）。
const REPLY_TIMEOUT: Duration = Duration::from_secs(120);
/// 传完后等服务器的 scp 退出的上限。
const EXIT_WAIT: Duration = Duration::from_secs(10);
/// 每次发出的数据量。
const CHUNK: usize = 64 * 1024;

/// SCP 失败的原因。
#[derive(Debug)]
pub(crate) enum ScpFailure {
    /// 服务器的 scp 报告的错误（应答 1、2 后面的说明，或退出前在标准错误中的最后一行）
    Remote(String),
    /// 服务器上没有 scp 命令
    NoScp,
    /// 服务器拒绝另开通道或执行命令
    Refused(String),
    /// 连接断开或通道意外关闭
    Lost,
    /// 对方说的不是 SCP 协议（例如登录脚本往标准输出写了东西）
    Protocol(String),
    /// 本机读写失败
    Local(std::io::Error),
    /// 文件超过上限（在线编辑）
    TooLarge,
    Canceled,
}

/// 服务器作为来源时发来的一条记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Record {
    /// `C0644 12 name`
    File { mode: u32, size: u64, name: Vec<u8> },
    /// `D0755 0 name`（只在 `-r` 时出现）
    Dir { mode: u32, name: Vec<u8> },
    /// `E`
    EndDir,
    /// `T<mtime> 0 <atime> 0`
    Time { mtime: i64, atime: i64 },
}

/// 文件头：`C<权限> <大小> <名称>\n`。名称中的换行与斜杠换成 `_`（服务器拒绝这样的名称）。
pub(crate) fn file_header(mode: u32, size: u64, name: &[u8]) -> Vec<u8> {
    let mut line = format!("C{:04o} {size} ", mode & 0o7777).into_bytes();
    line.extend(name.iter().map(|b| match b {
        b'\n' | b'/' => b'_',
        other => *other,
    }));
    line.push(b'\n');
    line
}

/// 时间头：`T<修改时间> 0 <访问时间> 0\n`（访问时间同修改时间）。
pub(crate) fn time_header(mtime: i64) -> Vec<u8> {
    let t = mtime.max(0);
    format!("T{t} 0 {t} 0\n").into_bytes()
}

/// 解析服务器发来的一行记录（不含换行）。
pub(crate) fn parse_record(line: &[u8]) -> Result<Record, String> {
    let bad = || format!("看不懂的 SCP 记录：{}", String::from_utf8_lossy(line));
    let (&kind, rest) = line.split_first().ok_or_else(bad)?;
    match kind {
        b'C' | b'D' => {
            let mut parts = rest.splitn(3, |b| *b == b' ');
            let mode = parts
                .next()
                .and_then(|m| std::str::from_utf8(m).ok())
                .filter(|m| !m.is_empty() && m.len() <= 5)
                .and_then(|m| u32::from_str_radix(m, 8).ok())
                .ok_or_else(bad)?;
            let size: u64 = parts
                .next()
                .and_then(|s| std::str::from_utf8(s).ok())
                .and_then(|s| s.parse().ok())
                .ok_or_else(bad)?;
            let name = parts.next().filter(|n| !n.is_empty()).ok_or_else(bad)?;
            Ok(if kind == b'C' {
                Record::File {
                    mode,
                    size,
                    name: name.to_vec(),
                }
            } else {
                Record::Dir {
                    mode,
                    name: name.to_vec(),
                }
            })
        }
        b'E' if rest.is_empty() => Ok(Record::EndDir),
        b'T' => {
            let text = std::str::from_utf8(rest).map_err(|_| bad())?;
            let fields: Vec<i64> = text
                .split(' ')
                .map(|f| f.parse::<i64>())
                .collect::<Result<_, _>>()
                .map_err(|_| bad())?;
            match fields[..] {
                [mtime, _, atime, _] => Ok(Record::Time { mtime, atime }),
                _ => Err(bad()),
            }
        }
        _ => Err(bad()),
    }
}

/// 服务器上运行 scp 的通道。没有正常结束时（出错、取消、调用方不再等待）在后台关闭。
pub(crate) struct ScpChannel {
    channel: Option<Channel<Msg>>,
    /// 收到还没读的标准输出
    buf: Vec<u8>,
    pos: usize,
    /// 标准错误的最后一部分
    stderr: Vec<u8>,
    exit: Option<u32>,
    /// 服务器不会再发数据（EOF 或关闭）
    ended: bool,
}

impl ScpChannel {
    /// 打开会话通道并执行 `command`。
    pub(crate) async fn open(
        connection: &Connection,
        command: Vec<u8>,
    ) -> Result<Self, ScpFailure> {
        let channel = match connection.handle().channel_open_session().await {
            Ok(channel) => channel,
            Err(russh::Error::ChannelOpenFailure(reason)) => {
                return Err(ScpFailure::Refused(format!("{reason:?}")));
            }
            Err(_) if connection.is_closed() => return Err(ScpFailure::Lost),
            Err(error) => return Err(ScpFailure::Refused(error.to_string())),
        };
        let mut this = Self {
            channel: Some(channel),
            buf: Vec::new(),
            pos: 0,
            stderr: Vec::new(),
            exit: None,
            ended: false,
        };
        let sent = this.channel().exec(true, command).await;
        if sent.is_err() {
            return Err(ScpFailure::Lost);
        }
        Ok(this)
    }

    fn channel(&mut self) -> &mut Channel<Msg> {
        self.channel.as_mut().expect("通道还没有关闭")
    }

    /// 等更多的标准输出；服务器不会再发时返回 `false`。
    async fn fill(&mut self) -> Result<bool, ScpFailure> {
        if self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        }
        loop {
            if self.ended {
                return Ok(false);
            }
            let Some(message) = self.channel().wait().await else {
                self.ended = true;
                return Ok(false);
            };
            match message {
                ChannelMsg::Data { data } => {
                    self.buf.extend_from_slice(&data);
                    return Ok(true);
                }
                ChannelMsg::ExtendedData { data, .. } => {
                    self.stderr.extend_from_slice(&data);
                    let excess = self.stderr.len().saturating_sub(4096);
                    self.stderr.drain(..excess);
                }
                ChannelMsg::ExitStatus { exit_status } => self.exit = Some(exit_status),
                // 服务器拒绝执行命令
                ChannelMsg::Failure => {
                    return Err(ScpFailure::Refused("服务器拒绝执行命令".into()));
                }
                ChannelMsg::Eof | ChannelMsg::Close => self.ended = true,
                _ => {}
            }
        }
    }

    async fn read_byte(&mut self) -> Result<Option<u8>, ScpFailure> {
        if self.pos == self.buf.len() && !self.fill().await? {
            return Ok(None);
        }
        let b = self.buf[self.pos];
        self.pos += 1;
        Ok(Some(b))
    }

    /// 读一行（不含换行）；没读完就结束了时为 `None`。
    async fn read_line(&mut self) -> Result<Option<Vec<u8>>, ScpFailure> {
        let mut line = Vec::new();
        loop {
            let Some(b) = self.read_byte().await? else {
                return Ok(None);
            };
            if b == b'\n' {
                return Ok(Some(line));
            }
            if line.len() >= MAX_LINE {
                return Err(ScpFailure::Protocol("SCP 记录太长".into()));
            }
            line.push(b);
        }
    }

    /// 服务器提前结束时的原因：没有 scp、标准错误的最后一行，或连接断开。
    fn ended_failure(&self) -> ScpFailure {
        let stderr = String::from_utf8_lossy(&self.stderr);
        let last = stderr
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or_default()
            .to_string();
        let lower = last.to_ascii_lowercase();
        if self.exit == Some(127)
            || (lower.contains("scp")
                && (lower.contains("not found") || lower.contains("no such file")))
        {
            return ScpFailure::NoScp;
        }
        if !last.is_empty() {
            return ScpFailure::Remote(strip_prefix(&last).to_string());
        }
        ScpFailure::Lost
    }

    /// 等一个应答。
    async fn reply(&mut self) -> Result<(), ScpFailure> {
        let read = async {
            match self.read_byte().await? {
                Some(0) => Ok(()),
                Some(code @ (1 | 2)) => {
                    let message = self.read_line().await?.unwrap_or_default();
                    let message = String::from_utf8_lossy(&message).trim().to_string();
                    tracing::debug!(code, "SCP 应答出错：{message}");
                    Err(ScpFailure::Remote(strip_prefix(&message).to_string()))
                }
                Some(other) => {
                    // 不是应答：把这一行读完作说明（多半是登录脚本输出的文字）
                    let mut line = vec![other];
                    if let Some(rest) = self.read_line().await? {
                        line.extend(rest);
                    }
                    Err(ScpFailure::Protocol(format!(
                        "服务器的回应不是 SCP 协议：{}",
                        String::from_utf8_lossy(&line).trim()
                    )))
                }
                None => Err(self.ended_failure()),
            }
        };
        tokio::time::timeout(REPLY_TIMEOUT, read)
            .await
            .unwrap_or(Err(ScpFailure::Lost))
    }

    async fn send(&mut self, bytes: &[u8]) -> Result<(), ScpFailure> {
        match self.channel().data(bytes).await {
            Ok(()) => Ok(()),
            // 服务器的 scp 出错退出了：读出它的说明
            Err(_) => {
                while self.fill().await? {}
                Err(self.ended_failure())
            }
        }
    }

    /// 正常结束：发 EOF，等服务器的 scp 退出（最多 [`EXIT_WAIT`]），关闭通道。
    async fn finish(mut self) -> Result<(), ScpFailure> {
        let _ = self.channel().eof().await;
        let wait = async {
            while self.fill().await? {
                // 结束后不该再有数据
                self.pos = self.buf.len();
            }
            Ok::<_, ScpFailure>(())
        };
        let _ = tokio::time::timeout(EXIT_WAIT, wait).await;
        let exit = self.exit;
        if let Some(channel) = self.channel.take() {
            let _ = channel.close().await;
        }
        match exit {
            Some(0) | None => Ok(()),
            Some(_) => Err(self.ended_failure()),
        }
    }
}

impl Drop for ScpChannel {
    fn drop(&mut self) {
        if let Some(channel) = self.channel.take() {
            crate::runtime::spawn(async move {
                let _ = channel.eof().await;
                let _ = channel.close().await;
            });
        }
    }
}

/// 去掉 `scp: ` 前缀。
fn strip_prefix(message: &str) -> &str {
    message.strip_prefix("scp: ").unwrap_or(message)
}

/// 上传：执行 `command`（`scp -t 目标`），把 `reader` 中 `size` 字节的内容写成目标文件，权限 `mode`、修改时间 `mtime`。
/// 每发出一块就加到 `progress` 上；`ctx` 取消时中止（目标文件可能已写了一部分，由调用方删除）。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn send_file<R: AsyncRead + Unpin>(
    connection: &Connection,
    command: Vec<u8>,
    name: &[u8],
    mode: u32,
    mtime: Option<i64>,
    size: u64,
    reader: &mut R,
    ctx: &Cancel,
    progress: &AtomicU64,
) -> Result<(), ScpFailure> {
    let mut scp = ScpChannel::open(connection, command).await?;
    let work = async {
        scp.reply().await?;
        if let Some(mtime) = mtime {
            scp.send(&time_header(mtime)).await?;
            scp.reply().await?;
        }
        scp.send(&file_header(mode, size, name)).await?;
        scp.reply().await?;
        let mut left = size;
        let mut chunk = vec![0u8; CHUNK];
        while left > 0 {
            let want = chunk.len().min(usize::try_from(left).unwrap_or(usize::MAX));
            let n = reader
                .read(&mut chunk[..want])
                .await
                .map_err(ScpFailure::Local)?;
            if n == 0 {
                return Err(ScpFailure::Local(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "本机文件在传输期间变短了",
                )));
            }
            scp.send(&chunk[..n]).await?;
            left -= n as u64;
            progress.fetch_add(n as u64, Ordering::Relaxed);
        }
        scp.send(&[0]).await?;
        scp.reply().await?;
        Ok(())
    };
    let result = tokio::select! {
        result = work => result,
        _ = ctx.cancelled() => Err(ScpFailure::Canceled),
    };
    result?;
    scp.finish().await
}

/// 下载：执行 `command`（`scp -f 路径`），把文件内容写进 `writer`，返回（权限，大小）。超过 `limit` 时不读内容，
/// 返回 [`ScpFailure::TooLarge`]。每收到一块就加到 `progress` 上。
pub(crate) async fn fetch_file<W: AsyncWrite + Unpin>(
    connection: &Connection,
    command: Vec<u8>,
    writer: &mut W,
    ctx: &Cancel,
    progress: &AtomicU64,
    limit: Option<u64>,
) -> Result<(u32, u64), ScpFailure> {
    let mut scp = ScpChannel::open(connection, command).await?;
    let work = async {
        scp.send(&[0]).await?;
        let (mode, size) = loop {
            let first = tokio::time::timeout(REPLY_TIMEOUT, scp.read_byte())
                .await
                .unwrap_or(Err(ScpFailure::Lost))?;
            let Some(first) = first else {
                return Err(scp.ended_failure());
            };
            if matches!(first, 1 | 2) {
                let message = scp.read_line().await?.unwrap_or_default();
                let message = String::from_utf8_lossy(&message).trim().to_string();
                return Err(ScpFailure::Remote(strip_prefix(&message).to_string()));
            }
            let mut line = vec![first];
            match scp.read_line().await? {
                Some(rest) => line.extend(rest),
                None => return Err(scp.ended_failure()),
            }
            match parse_record(&line).map_err(ScpFailure::Protocol)? {
                Record::Time { .. } => scp.send(&[0]).await?,
                Record::File { mode, size, .. } => break (mode, size),
                Record::Dir { .. } | Record::EndDir => {
                    return Err(ScpFailure::Remote("not a regular file".into()));
                }
            }
        };
        if limit.is_some_and(|limit| size > limit) {
            return Err(ScpFailure::TooLarge);
        }
        scp.send(&[0]).await?;
        let mut left = size;
        while left > 0 {
            if scp.pos == scp.buf.len() && !scp.fill().await? {
                return Err(scp.ended_failure());
            }
            let available = &scp.buf[scp.pos..];
            let take = available
                .len()
                .min(usize::try_from(left).unwrap_or(usize::MAX));
            writer
                .write_all(&available[..take])
                .await
                .map_err(ScpFailure::Local)?;
            scp.pos += take;
            left -= take as u64;
            progress.fetch_add(take as u64, Ordering::Relaxed);
        }
        // 内容之后的应答（读文件出错时是 1 与说明）
        scp.reply().await?;
        scp.send(&[0]).await?;
        Ok((mode, size))
    };
    let result = tokio::select! {
        result = work => result,
        _ = ctx.cancelled() => Err(ScpFailure::Canceled),
    };
    let done = result?;
    scp.finish().await?;
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers() {
        assert_eq!(file_header(0o644, 12, b"a b.txt"), b"C0644 12 a b.txt\n");
        assert_eq!(file_header(0o104755, 0, b"x/y\nz"), b"C4755 0 x_y_z\n");
        assert_eq!(time_header(1_794_200_144), b"T1794200144 0 1794200144 0\n");
        assert_eq!(time_header(-5), b"T0 0 0 0\n");
    }

    #[test]
    fn records() {
        assert_eq!(
            parse_record(b"C0644 5 hello world.txt"),
            Ok(Record::File {
                mode: 0o644,
                size: 5,
                name: b"hello world.txt".to_vec()
            })
        );
        assert_eq!(
            parse_record(b"D0755 0 dir"),
            Ok(Record::Dir {
                mode: 0o755,
                name: b"dir".to_vec()
            })
        );
        assert_eq!(parse_record(b"E"), Ok(Record::EndDir));
        assert_eq!(
            parse_record(b"T1794200144 0 1794200100 0"),
            Ok(Record::Time {
                mtime: 1_794_200_144,
                atime: 1_794_200_100
            })
        );
        for bad in [
            &b""[..],
            b"C0644 x name",
            b"C0644 5",
            b"C0644 5 ",
            b"C9 5 a",
            b"T1 2 3",
            b"Ex",
            b"Xhello",
        ] {
            assert!(
                parse_record(bad).is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }
}
