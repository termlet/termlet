//! SCP 模式：服务器上打不开 SFTP 子系统（没有开 sftp-server、堡垒机限制等）时，文件管理退回 SCP。
//!
//! - 浏览与查看属性用只读的 `ls`（[`ls`] 模块；GNU 与 BusyBox 的写法都认），新建目录、改名、删除用 `mkdir`、`mv`、`rm`，
//!   都经 `sh -c` 执行（登录 shell 是 fish、tcsh 时也一样），路径作为 `$1`、`$2` 传入、不拼进脚本，按主机设置的
//!   文件名编码转成字节。
//! - 上传、下载、在线编辑的读写用 SCP 协议（[`proto`]）：一次一个文件，没有断点续传；上传先写同目录下的临时文件，
//!   完成后 `mv` 成正式文件名（覆盖已有文件时沿用它的权限）。
//! - 不支持的：修改权限、断点续传（失败后从头传）、只用一个通道的主机（每个操作都要另开通道）。
//!
//! 打开时先探测：`ls` 的写法与有没有 `scp`。服务器连 `scp` 也没有、不允许另开通道执行命令时，打开失败并说明原因
//! （[`super::NoSftp`]）。结果的错误与 SFTP 客户端的一样（[`ClientError`] 的状态码），会话、传输的错误处理照常。

pub(crate) mod ls;
pub(crate) mod proto;

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh_sftp::client::error::Error as ClientError;
use russh_sftp::protocol::{FileAttributes, Status, StatusCode};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;
use tokio::time::Instant;

use super::SftpError;
use super::cancel::Cancel;
use super::client::{ClientResult, DirItem, entry_name_ok};
use super::codec::{UNENCODABLE, decode_name, encode_name};
use super::entry::{Entry, Listing};
use super::error::NoSftp;
use super::path;
use crate::model::Encoding;
use crate::ssh::{Connection, ExecFailure, ExecResult, exec_until};
use ls::{LsLine, LsStyle};
pub(crate) use proto::ScpFailure;

/// 一般命令（列目录等）的时限。
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// 删除目录可能较久。
const REMOVE_TIMEOUT: Duration = Duration::from_secs(600);
/// 列目录输出的上限。
const LIST_LIMIT: usize = 64 << 20;
/// 其他命令输出的上限。
const SMALL_LIMIT: usize = 64 * 1024;
/// 输出中这一行之后才是脚本的输出（之前的是登录脚本可能打印的文字）。
const BEGIN: &str = "termlet-begin";

/// 脚本的退出码：不存在、没有权限、不是目录。
const EXIT_NOT_FOUND: u32 = 2;
const EXIT_DENIED: u32 = 3;
const EXIT_NOT_DIR: u32 = 4;

/// 经 SCP 与 shell 命令操作服务器上的文件。
pub(crate) struct ScpRemote {
    connection: Mutex<Option<Arc<Connection>>>,
    /// 关闭（[`ScpRemote::close`]）后为真
    lost: watch::Sender<bool>,
    encoding: Encoding,
    ls: LsStyle,
    home: String,
}

impl std::fmt::Debug for ScpRemote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScpRemote")
            .field("ls", &self.ls)
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}

/// 把字节写成 shell 的单引号字符串（与 [`crate::hosttools::sh_quote`] 相同的写法，按字节）。
fn quote_bytes(bytes: &[u8], out: &mut Vec<u8>) {
    out.push(b'\'');
    for &b in bytes {
        match b {
            b'\'' => out.extend_from_slice(br"'\''"),
            b'\\' => out.extend_from_slice(br"'\\'"),
            other => out.push(other),
        }
    }
    out.push(b'\'');
}

/// 多行脚本合成一行（换行与下一行的缩进换成一个空格）：登录 shell 是 tcsh 时单引号内不能换行。
/// 因此脚本的每行都以分号、`then`、`else` 等结尾。
fn one_line(script: &str) -> String {
    script
        .split('\n')
        .map(|line| line.trim_start_matches([' ', '\t']))
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

/// `sh -c '脚本' sh '参数'…` 的命令行（脚本先合成一行，见 [`one_line`]）。
pub(crate) fn sh_command(script: &str, args: &[&[u8]]) -> Vec<u8> {
    let mut out = b"sh -c ".to_vec();
    quote_bytes(one_line(script).as_bytes(), &mut out);
    out.extend_from_slice(b" sh");
    for arg in args {
        out.push(b' ');
        quote_bytes(arg, &mut out);
    }
    out
}

/// 服务器上的命令行：`程序 参数 '路径'`（scp 两端）。
fn program_command(program: &str, path: &[u8]) -> Vec<u8> {
    let mut out = program.as_bytes().to_vec();
    out.push(b' ');
    quote_bytes(path, &mut out);
    out
}

fn status(code: StatusCode, message: impl Into<String>) -> ClientError {
    ClientError::Status(Status {
        id: 0,
        status_code: code,
        error_message: message.into(),
        language_tag: "en-US".into(),
    })
}

fn lost() -> ClientError {
    ClientError::UnexpectedBehavior("SCP 连接已断开".into())
}

/// 命令失败的说明（标准错误的最后一行）按内容换成状态码：不存在、没有权限，其余原样带上。
pub(crate) fn status_from_message(message: &str) -> ClientError {
    let lower = message.to_ascii_lowercase();
    if lower.contains("no such file") || lower.contains("not found") {
        status(StatusCode::NoSuchFile, message)
    } else if lower.contains("permission denied") || lower.contains("operation not permitted") {
        status(StatusCode::PermissionDenied, message)
    } else {
        status(StatusCode::Failure, message)
    }
}

/// 标准错误中最后一行有内容的，去掉 `ls: ` 之类的前缀后作为说明。
fn last_line(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    text.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("命令执行失败")
        .to_string()
}

/// 脚本的输出中 [`BEGIN`] 之后的部分；没有时为 `None`。
fn after_begin(stdout: &[u8]) -> Option<&[u8]> {
    let marker = format!("{BEGIN}\n");
    let at = stdout
        .windows(marker.len())
        .position(|w| w == marker.as_bytes())?;
    Some(&stdout[at + marker.len()..])
}

/// `ls -l` 的一行换成 SFTP 的属性。
fn attrs_of(line: &LsLine) -> FileAttributes {
    let mtime = u32::try_from(line.mtime.max(0)).unwrap_or(u32::MAX);
    FileAttributes {
        size: Some(line.size),
        uid: Some(line.uid),
        gid: Some(line.gid),
        permissions: Some(line.mode),
        atime: Some(mtime),
        mtime: Some(mtime),
        ..FileAttributes::empty()
    }
}

/// 解析 `ls -l` 的多行输出（按行切开，解析不了的跳过）。
fn parse_listing(output: &[u8], style: LsStyle, now: i64) -> Vec<LsLine> {
    output
        .split(|b| *b == b'\n')
        .filter_map(|line| ls::parse_line(line, style, now))
        .collect()
}

impl ScpRemote {
    /// 探测服务器（`ls` 的写法、有没有 scp、初始目录）并打开。
    pub(crate) async fn open(
        connection: Arc<Connection>,
        encoding: Encoding,
    ) -> Result<Self, SftpError> {
        let script = format!(
            "echo {BEGIN}; pwd -P 2>/dev/null || pwd;
            if LC_ALL=C ls -dlnb --time-style=+%s / >/dev/null 2>&1; then echo gnu;
            elif ls -dln --full-time / >/dev/null 2>&1; then echo full;
            elif ls -dln / >/dev/null 2>&1; then echo plain;
            else echo none; fi;
            if command -v scp >/dev/null 2>&1; then echo scp; else echo noscp; fi"
        );
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        let result = exec_until(
            &connection,
            sh_command(&script, &[]),
            None,
            SMALL_LIMIT,
            deadline,
        )
        .await
        .map_err(|failure| match failure {
            ExecFailure::OpenRefused(_) | ExecFailure::ExecRefused => {
                SftpError::NoSftp(NoSftp::ExecRefused)
            }
            ExecFailure::Timeout => {
                SftpError::Unavailable("探测 SCP 的命令没有在时限内结束".into())
            }
            ExecFailure::Other(error) if connection.is_closed() => SftpError::Connect(error),
            ExecFailure::Other(error) => SftpError::Unavailable(error.to_string()),
        })?;
        let Some(output) = after_begin(&result.stdout) else {
            tracing::info!(
                host = %connection.label(),
                exit_code = ?result.exit_code,
                "服务器上没有可用的 sh，不能改用 SCP"
            );
            return Err(SftpError::NoSftp(NoSftp::NoShell));
        };
        let lines: Vec<&[u8]> = output.split(|b| *b == b'\n').collect();
        let home = lines
            .first()
            .map(|l| decode_name(l, encoding))
            .filter(|h| h.starts_with('/'))
            .unwrap_or_else(|| "/".into());
        let ls = match lines.get(1).copied() {
            Some(b"gnu") => LsStyle::Gnu,
            Some(b"full") => LsStyle::FullTime,
            Some(b"plain") => LsStyle::Plain,
            _ => return Err(SftpError::NoSftp(NoSftp::NoShell)),
        };
        if lines.get(2).copied() != Some(&b"scp"[..]) {
            tracing::info!(host = %connection.label(), "服务器上没有 scp，不能改用 SCP");
            return Err(SftpError::NoSftp(NoSftp::NoScp));
        }
        tracing::info!(host = %connection.label(), ?ls, "服务器没有 SFTP，改用 SCP");
        Ok(Self {
            connection: Mutex::new(Some(connection)),
            lost: watch::channel(false).0,
            encoding,
            ls,
            home,
        })
    }

    pub(crate) fn home(&self) -> &str {
        &self.home
    }

    /// 等到关闭或连接断开（等待期间持有连接，关闭时放开）。
    pub(crate) async fn closed(&self) {
        let mut lost = self.lost.subscribe();
        let Some(connection) = self.connection.lock().unwrap().clone() else {
            return;
        };
        tokio::select! {
            _ = connection.closed() => {}
            _ = lost.wait_for(|v| *v) => {}
        }
    }

    /// 关闭：之后的操作都失败，放开连接。
    pub(crate) fn close(&self) {
        self.connection.lock().unwrap().take();
        self.lost.send_replace(true);
    }

    fn connection(&self) -> ClientResult<Arc<Connection>> {
        if *self.lost.borrow() {
            return Err(lost());
        }
        self.connection
            .lock()
            .unwrap()
            .clone()
            .filter(|c| !c.is_closed())
            .ok_or_else(lost)
    }

    /// 路径按服务器的文件名编码转成字节。
    fn encode(&self, p: &str) -> ClientResult<Vec<u8>> {
        encode_name(p, self.encoding).ok_or_else(|| status(StatusCode::Failure, UNENCODABLE))
    }

    /// 执行脚本，路径作为参数；返回结果（退出码不是 0 时也返回）。
    async fn run(
        &self,
        script: &str,
        paths: &[&str],
        limit: usize,
        timeout: Duration,
    ) -> ClientResult<ExecResult> {
        let connection = self.connection()?;
        let args: Vec<Vec<u8>> = paths
            .iter()
            .map(|p| self.encode(p))
            .collect::<ClientResult<_>>()?;
        let refs: Vec<&[u8]> = args.iter().map(Vec::as_slice).collect();
        let command = sh_command(script, &refs);
        exec_until(&connection, command, None, limit, Instant::now() + timeout)
            .await
            .map_err(|failure| match failure {
                ExecFailure::Timeout => ClientError::Timeout,
                ExecFailure::OpenRefused(reason) => status(
                    StatusCode::Failure,
                    format!("服务器拒绝打开通道：{reason:?}"),
                ),
                ExecFailure::ExecRefused => status(StatusCode::Failure, "服务器拒绝执行命令"),
                ExecFailure::Other(_) if connection.is_closed() => lost(),
                ExecFailure::Other(error) => status(StatusCode::Failure, error.to_string()),
            })
    }

    /// 执行只要成功与否的命令：退出码按脚本的约定换成状态码，其余按标准错误的说明。
    async fn run_ok(&self, script: &str, paths: &[&str], timeout: Duration) -> ClientResult<()> {
        let result = self.run(script, paths, SMALL_LIMIT, timeout).await?;
        check(&result)
    }

    /// 一项的属性：`follow` 时跟随符号链接（悬空链接为不存在）。
    async fn stat_with(&self, p: &str, follow: bool) -> ClientResult<FileAttributes> {
        let script = format!("echo {BEGIN}; {} \"$1\"", self.ls.stat_command(follow));
        let result = self
            .run(&script, &[p], SMALL_LIMIT, COMMAND_TIMEOUT)
            .await?;
        check(&result)?;
        let output = after_begin(&result.stdout)
            .ok_or_else(|| status(StatusCode::Failure, "ls 的输出不对"))?;
        parse_listing(output, self.ls, crate::time::now_ms() / 1000)
            .first()
            .map(attrs_of)
            .ok_or_else(|| status(StatusCode::Failure, "看不懂 ls 的输出"))
    }

    pub(crate) async fn stat(&self, p: &str) -> ClientResult<FileAttributes> {
        self.stat_with(p, true).await
    }

    pub(crate) async fn lstat(&self, p: &str) -> ClientResult<FileAttributes> {
        self.stat_with(p, false).await
    }

    /// 列出目录：先进入目录（不存在、不是目录、没有权限分别报告），输出解析后的绝对路径与 `ls` 的结果；
    /// `follow` 时另外按链接的目标列一遍（悬空的链接不在其中）。
    async fn list_raw(
        &self,
        dir: &str,
        follow: bool,
    ) -> ClientResult<(String, Vec<LsLine>, Vec<LsLine>)> {
        let mut script = format!(
            "cd \"$1\" 2>/dev/null || {{ if [ ! -e \"$1\" ]; then exit {EXIT_NOT_FOUND}; elif [ ! -d \"$1\" ]; then exit {EXIT_NOT_DIR}; else exit {EXIT_DENIED}; fi; }};
            [ -r . ] || exit {EXIT_DENIED};
            echo {BEGIN}; pwd -P; echo /; {} 2>/dev/null",
            self.ls.list_command(false)
        );
        if follow {
            script.push_str(&format!(
                "; echo /; {} 2>/dev/null",
                self.ls.list_command(true)
            ));
        }
        script.push_str("; true");
        let result = self
            .run(&script, &[dir], LIST_LIMIT, COMMAND_TIMEOUT)
            .await?;
        check(&result)?;
        if result.truncated {
            return Err(status(StatusCode::Failure, "目录中的项太多"));
        }
        let output = after_begin(&result.stdout)
            .ok_or_else(|| status(StatusCode::Failure, "ls 的输出不对"))?;
        let mut sections = output.split(|b| *b == b'\n').peekable();
        let resolved = sections
            .next()
            .map(|l| decode_name(l, self.encoding))
            .filter(|p| p.starts_with('/'))
            .unwrap_or_else(|| path::clean(dir));
        let now = crate::time::now_ms() / 1000;
        let mut plain = Vec::new();
        let mut followed = Vec::new();
        let mut section = 0;
        for line in sections {
            if line == b"/" {
                section += 1;
                continue;
            }
            let Some(parsed) = ls::parse_line(line, self.ls, now) else {
                continue;
            };
            match section {
                1 => plain.push(parsed),
                2 => followed.push(parsed),
                _ => {}
            }
        }
        Ok((resolved, plain, followed))
    }

    /// 解码后能用的名称（不合法的跳过）。
    fn name_of(&self, line: &LsLine, dir: &str) -> Option<String> {
        let name = decode_name(&line.name, self.encoding);
        if entry_name_ok(&name) {
            Some(name)
        } else {
            tracing::warn!(dir = %dir, name = ?name, "ls 列出了不合法的文件名，跳过");
            None
        }
    }

    /// 目录中的项（属性不跟随符号链接），目录遍历用。
    pub(crate) async fn read_dir(&self, dir: &str) -> ClientResult<Vec<DirItem>> {
        let (_, plain, _) = self.list_raw(dir, false).await?;
        Ok(plain
            .iter()
            .filter_map(|line| {
                Some(DirItem {
                    name: self.name_of(line, dir)?,
                    longname: String::new(),
                    attrs: attrs_of(line),
                })
            })
            .collect())
    }

    /// 列出目录（文件管理的列表）：符号链接按目标判断是否为目录，权限也取目标的。
    pub(crate) async fn list(&self, dir: &str) -> ClientResult<Listing> {
        let (resolved, plain, followed) = self.list_raw(dir, true).await?;
        let targets: HashMap<&[u8], FileAttributes> = followed
            .iter()
            .map(|line| (line.name.as_slice(), attrs_of(line)))
            .collect();
        let entries = plain
            .iter()
            .filter_map(|line| {
                let name = self.name_of(line, &resolved)?;
                let mut entry = Entry::from_attrs(name, &attrs_of(line));
                if entry.is_link() {
                    entry.resolve_link(targets.get(line.name.as_slice()));
                }
                Some(entry)
            })
            .collect();
        Ok(Listing {
            path: resolved,
            entries,
        })
    }

    /// 目录的绝对路径（展开符号链接）。
    pub(crate) async fn realpath_dir(&self, dir: &str) -> ClientResult<String> {
        let script = format!(
            "cd \"$1\" 2>/dev/null || {{ if [ ! -e \"$1\" ]; then exit {EXIT_NOT_FOUND}; elif [ ! -d \"$1\" ]; then exit {EXIT_NOT_DIR}; else exit {EXIT_DENIED}; fi; }}; echo {BEGIN}; pwd -P"
        );
        let result = self
            .run(&script, &[dir], SMALL_LIMIT, COMMAND_TIMEOUT)
            .await?;
        check(&result)?;
        let output = after_begin(&result.stdout).unwrap_or_default();
        let line = output.split(|b| *b == b'\n').next().unwrap_or_default();
        let resolved = decode_name(line, self.encoding);
        Ok(if resolved.starts_with('/') {
            resolved
        } else {
            path::clean(dir)
        })
    }

    /// 符号链接最终指向的路径（`readlink -f`；没有这个命令时为原路径）。
    pub(crate) async fn resolve_link(&self, p: &str) -> ClientResult<String> {
        let script = format!("echo {BEGIN}; readlink -f \"$1\" 2>/dev/null || echo \"$1\"");
        let result = self
            .run(&script, &[p], SMALL_LIMIT, COMMAND_TIMEOUT)
            .await?;
        let output = after_begin(&result.stdout).unwrap_or_default();
        let line = output.split(|b| *b == b'\n').next().unwrap_or_default();
        let resolved = decode_name(line, self.encoding);
        Ok(if resolved.starts_with('/') {
            resolved
        } else {
            p.to_string()
        })
    }

    pub(crate) async fn mkdir(&self, p: &str) -> ClientResult<()> {
        self.run_ok("LC_ALL=C mkdir \"$1\"", &[p], COMMAND_TIMEOUT)
            .await
    }

    /// 逐级创建目录（已存在的跳过）。
    pub(crate) async fn mkdir_all(&self, p: &str) -> ClientResult<()> {
        self.run_ok("LC_ALL=C mkdir -p \"$1\"", &[p], COMMAND_TIMEOUT)
            .await
    }

    /// 删除一个文件（不存在不算错）。
    pub(crate) async fn remove_file(&self, p: &str) -> ClientResult<()> {
        self.run_ok("LC_ALL=C rm -f \"$1\"", &[p], COMMAND_TIMEOUT)
            .await
    }

    /// 递归删除（符号链接只删链接本身）。
    pub(crate) async fn remove_tree(&self, p: &str) -> ClientResult<()> {
        self.run_ok(
            &format!(
                "[ -e \"$1\" ] || [ -L \"$1\" ] || exit {EXIT_NOT_FOUND}; LC_ALL=C rm -rf \"$1\""
            ),
            &[p],
            REMOVE_TIMEOUT,
        )
        .await
    }

    /// 改名或移动（调用方先确认目标不存在）。
    pub(crate) async fn rename(&self, from: &str, to: &str) -> ClientResult<()> {
        self.run_ok(
            &format!(
                "[ -e \"$1\" ] || [ -L \"$1\" ] || exit {EXIT_NOT_FOUND}; LC_ALL=C mv \"$1\" \"$2\""
            ),
            &[from, to],
            COMMAND_TIMEOUT,
        )
        .await
    }

    /// 上传完成后：临时文件 `part` 设好权限 `mode`（有时）再替换成 `target`（`mv -f`，原子地覆盖）。
    pub(crate) async fn commit_upload(
        &self,
        part: &str,
        target: &str,
        mode: Option<u32>,
    ) -> ClientResult<()> {
        let script = match mode {
            Some(mode) => format!(
                "chmod {:o} \"$1\" 2>/dev/null; LC_ALL=C mv -f \"$1\" \"$2\"",
                mode & 0o7777
            ),
            None => "LC_ALL=C mv -f \"$1\" \"$2\"".to_string(),
        };
        self.run_ok(&script, &[part, target], COMMAND_TIMEOUT).await
    }

    /// 把 `reader` 中 `size` 字节写成服务器上的 `target`（SCP），权限 `mode`、修改时间 `mtime`。
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn upload<R: AsyncRead + Unpin>(
        &self,
        target: &str,
        mode: u32,
        mtime: Option<i64>,
        size: u64,
        reader: &mut R,
        ctx: &Cancel,
        progress: &AtomicU64,
    ) -> Result<(), ScpFailure> {
        let connection = self.connection().map_err(|_| ScpFailure::Lost)?;
        let encoded = self
            .encode(target)
            .map_err(|_| ScpFailure::Remote(UNENCODABLE.into()))?;
        let name = encode_name(&path::base_name(target), self.encoding).unwrap_or_default();
        proto::send_file(
            &connection,
            program_command("scp -t", &encoded),
            &name,
            mode,
            mtime,
            size,
            reader,
            ctx,
            progress,
        )
        .await
    }

    /// 读服务器上的 `p`（SCP）写进 `writer`，返回（权限，大小）；超过 `limit` 时不读。
    pub(crate) async fn download<W: AsyncWrite + Unpin>(
        &self,
        p: &str,
        writer: &mut W,
        ctx: &Cancel,
        progress: &AtomicU64,
        limit: Option<u64>,
    ) -> Result<(u32, u64), ScpFailure> {
        let connection = self.connection().map_err(|_| ScpFailure::Lost)?;
        let encoded = self
            .encode(p)
            .map_err(|_| ScpFailure::Remote(UNENCODABLE.into()))?;
        proto::fetch_file(
            &connection,
            program_command("scp -f", &encoded),
            writer,
            ctx,
            progress,
            limit,
        )
        .await
    }
}

/// 命令的退出码换成结果：0 成功；脚本约定的不存在、没有权限、不是目录；其余按标准错误的说明。
fn check(result: &ExecResult) -> ClientResult<()> {
    match result.exit_code {
        Some(0) => Ok(()),
        Some(EXIT_NOT_FOUND) if result.stderr.is_empty() => {
            Err(status(StatusCode::NoSuchFile, "No such file or directory"))
        }
        Some(EXIT_DENIED) if result.stderr.is_empty() => {
            Err(status(StatusCode::PermissionDenied, "Permission denied"))
        }
        Some(EXIT_NOT_DIR) if result.stderr.is_empty() => {
            Err(status(StatusCode::Failure, "Not a directory"))
        }
        _ => Err(status_from_message(&last_line(&result.stderr))),
    }
}

/// SCP 的失败换成带路径的 [`SftpError`]。
pub(crate) fn scp_error(p: &str, failure: ScpFailure) -> SftpError {
    use super::error::Side;
    match failure {
        ScpFailure::Remote(message) if message == UNENCODABLE => {
            super::error::Rejection::UnencodableName.into()
        }
        ScpFailure::Remote(message) => {
            let lower = message.to_ascii_lowercase();
            if lower.contains("no space left") || lower.contains("quota exceeded") {
                SftpError::NoSpace {
                    side: Side::Remote,
                    path: p.to_string(),
                }
            } else {
                super::error::remote(p, &status_from_message(&message))
            }
        }
        ScpFailure::NoScp => SftpError::NoSftp(NoSftp::NoScp),
        ScpFailure::Refused(reason) => SftpError::failed(Side::Remote, p, reason),
        ScpFailure::Lost => SftpError::Disconnected,
        ScpFailure::Protocol(reason) => SftpError::failed(Side::Remote, p, reason),
        ScpFailure::Local(error) => SftpError::failed(Side::Local, p, error.to_string()),
        ScpFailure::TooLarge => SftpError::TooLarge {
            path: p.to_string(),
            limit: super::MAX_EDIT_SIZE,
        },
        ScpFailure::Canceled => SftpError::Canceled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_quote_paths_as_bytes() {
        let cmd = sh_command("ls \"$1\";\n  echo x", &[b"/a b/it's", b"/c\\d"]);
        assert_eq!(
            String::from_utf8(cmd).unwrap(),
            r#"sh -c 'ls "$1"; echo x' sh '/a b/it'\''s' '/c'\\'d'"#
        );
        // GBK 编码的路径原样成为参数的字节
        let gbk = encode_name("/数据", Encoding::Gb18030).unwrap();
        let cmd = program_command("scp -t", &gbk);
        assert_eq!(&cmd[..8], b"scp -t '");
        assert_eq!(&cmd[8..cmd.len() - 1], gbk.as_slice());
    }

    #[test]
    fn exit_codes_and_messages() {
        let result = |code, stderr: &str| ExecResult {
            exit_code: Some(code),
            stderr: stderr.as_bytes().to_vec(),
            ..Default::default()
        };
        let code = |r: ClientResult<()>| super::super::error::status_of(&r.unwrap_err());
        assert!(check(&result(0, "")).is_ok());
        assert_eq!(code(check(&result(2, ""))), Some(StatusCode::NoSuchFile));
        assert_eq!(
            code(check(&result(3, ""))),
            Some(StatusCode::PermissionDenied)
        );
        assert_eq!(
            code(check(&result(
                1,
                "ls: cannot access '/x': No such file or directory\n"
            ))),
            Some(StatusCode::NoSuchFile)
        );
        assert_eq!(
            code(check(&result(
                1,
                "mkdir: can't create directory '/x': Permission denied"
            ))),
            Some(StatusCode::PermissionDenied)
        );
        match check(&result(1, "rm: cannot remove '/x': Read-only file system")) {
            Err(ClientError::Status(s)) => {
                assert_eq!(
                    s.error_message,
                    "rm: cannot remove '/x': Read-only file system"
                )
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            scp_error(
                "/x",
                ScpFailure::Remote("/x: No space left on device".into())
            ),
            SftpError::NoSpace { .. }
        ));
        assert!(matches!(
            scp_error("/x", ScpFailure::Remote("/x: Permission denied".into())),
            SftpError::PermissionDenied { .. }
        ));
        assert_eq!(
            scp_error("/x", ScpFailure::NoScp),
            SftpError::NoSftp(NoSftp::NoScp)
        );
    }

    #[test]
    fn output_after_marker() {
        assert_eq!(
            after_begin(b"motd\ntermlet-begin\n/home/a\n"),
            Some(&b"/home/a\n"[..])
        );
        assert_eq!(after_begin(b"nothing"), None);
    }
}
