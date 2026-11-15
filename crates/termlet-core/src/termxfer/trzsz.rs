//! trzsz 客户端（V1 协议）。协议与读行逻辑按 trzsz-go 实现（MIT License，
//! Copyright (c) 2022-2026 The Trzsz Authors，https://github.com/trzsz/trzsz-go，trzsz/transfer.go、trzsz/buffer.go；
//! 许可文本见同目录的 LICENSE-trzsz-go.txt，随安装包附上）：
//!
//! ```text
//! 远端 trz / tsz 输出触发行 → 客户端 #ACT（是否确认、能力）→ 远端 #CFG（配置）→ 逐个文件：
//! #NUM 文件数、#NAME 文件名、#SIZE 大小、#DATA 数据块（每块等对方 #SUCC 确认）、#MD5 校验 → 客户端 #EXIT 结束语。
//! ```
//!
//! 每一行是 `#类型:内容` 加换行，字符串与数据块经 zlib 压缩后 base64 编码；出错时发 `#fail`。
//! 客户端不声明协议版本时远端用 V1（逐块确认，块大小按速度自动增大到 bufsize），不使用二进制模式与 V2 起的流水线、归档。

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use data_encoding::BASE64;
use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use md5::{Digest, Md5};
use memchr::memmem;
use serde::{Deserialize, Serialize};
use termlet_io::TerminalInput;

use super::files::{
    CollectError, Source, collect, create_new_file, create_unique, make_dir_all, safe_name,
};
use super::info::{Protocol, XferError};
use super::pipe::{Pipe, PipeError};
use super::progress::Tracker;
use super::transfer::Halt;

/// `#ACT` 中声明的客户端版本。
const TRZSZ_VERSION: &str = "1.2.0";

// 远端给的参数与输出的上限（触发传输的是普通的终端输出，可以伪造，远端的参数不能全信）：
// - MAX_BLOCK_LEN 是收到的一个数据块解压后的上限：远端默认的块最大 10MB，tsz -B 可以设得更大；超过时报错
// - MAX_LINE_LEN 是一行的长度上限，与 MAX_BLOCK_LEN 匹配：不可压缩的数据经 zlib 压缩后略大于原长（每 64KB 多 5 字节），
//   再经 base64 编码为 4/3，64MB 的块约 85.3MB，取 90MB；超过时按协议错误中止，防止远端持续输出不换行的数据占满内存
// - MAX_BUF_SIZE 是发送时数据块大小的上限（#CFG 中的 bufsize 可以很大），与 trzsz 默认的块大小相同
// - 每一行的超时：#CFG 中的 timeout 为 0 或负数（不超时）时用默认值，不超过 MAX_TIMEOUT
pub(crate) const MAX_BLOCK_LEN: usize = 64 << 20;
pub(crate) const MAX_LINE_LEN: usize = 90 << 20;
const MAX_BUF_SIZE: u64 = 10 << 20;
const DEFAULT_TIMEOUT: i64 = 20;
const MAX_TIMEOUT: i64 = 300;

/// 一次 trzsz 传输用到的连接：读远端的输出、写远端的输入、记录进度。
pub(crate) struct Link<'a> {
    pub pipe: &'a Pipe,
    pub out: &'a dyn TerminalInput,
    pub tracker: &'a Tracker,
    /// 远端是 Windows 或经过 ConPTY：行以 `!\n` 结束，读行时跳过终端控制序列
    pub windows: bool,
    pub cancelled: &'a dyn Fn() -> bool,
}

/// 客户端的 `#ACT`。
#[derive(Serialize)]
struct Action<'a> {
    lang: &'a str,
    version: &'a str,
    confirm: bool,
    newline: &'a str,
    binary: bool,
    support_dir: bool,
}

/// 远端的 `#CFG`（只取用到的）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Config {
    pub quiet: bool,
    pub binary: bool,
    pub directory: bool,
    pub overwrite: bool,
    pub timeout: i64,
    pub newline: String,
    pub protocol: i64,
    pub max_buf_size: u64,
    pub tmux_output_junk: bool,
}

/// `#CFG` 的 JSON：没有的项保持客户端的默认值。
#[derive(Deserialize)]
struct RemoteConfig {
    quiet: Option<bool>,
    binary: Option<bool>,
    directory: Option<bool>,
    overwrite: Option<bool>,
    timeout: Option<i64>,
    newline: Option<String>,
    protocol: Option<i64>,
    bufsize: Option<i64>,
    tmux_output_junk: Option<bool>,
}

/// 传目录时 `#NAME` 中的文件描述（与 trzsz-go 的 sourceFile 相同的 JSON）。
#[derive(Serialize, Deserialize, Default)]
struct FileDesc {
    #[serde(default)]
    path_id: usize,
    #[serde(default)]
    path_name: Vec<String>,
    #[serde(default)]
    is_dir: bool,
    #[serde(default)]
    archive: bool,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    perm: Option<u32>,
}

pub(crate) fn encode_bytes(b: &[u8]) -> String {
    let mut z = ZlibEncoder::new(Vec::with_capacity(b.len() / 2 + 64), Compression::default());
    // 写进内存不会失败
    let _ = z.write_all(b);
    let compressed = z.finish().unwrap_or_default();
    BASE64.encode(&compressed)
}

/// 一行最多是一个数据块：限制解压后的长度，防止压缩率很高的异常数据占满内存；超过时报错，不截断后继续。
pub(crate) fn decode_bytes(s: &[u8]) -> Result<Vec<u8>, XferError> {
    let raw = BASE64
        .decode(s)
        .map_err(|e| XferError::protocol(Protocol::Trzsz, format!("base64 解码失败：{e}")))?;
    let mut out = Vec::new();
    ZlibDecoder::new(&raw[..])
        .take(MAX_BLOCK_LEN as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| XferError::protocol(Protocol::Trzsz, format!("zlib 解压失败：{e}")))?;
    if out.len() > MAX_BLOCK_LEN {
        return Err(XferError::BlockTooLarge);
    }
    Ok(out)
}

fn failed(error: XferError) -> Halt {
    Halt::Failed(error)
}

fn protocol_error(detail: impl Into<String>) -> Halt {
    failed(XferError::protocol(Protocol::Trzsz, detail))
}

/// 远端发来的错误或结束语、远端按了 Ctrl+C：不用再回复远端。
fn from_remote(halt: &Halt) -> bool {
    matches!(
        halt,
        Halt::Failed(XferError::Remote(_) | XferError::RemoteInterrupted)
    )
}

pub(crate) struct Conn<'a> {
    link: Link<'a>,
    pub cfg: Config,
}

impl<'a> Conn<'a> {
    pub(crate) fn new(link: Link<'a>) -> Self {
        let newline = if link.windows { "!\n" } else { "\n" };
        Self {
            link,
            cfg: Config {
                quiet: false,
                binary: false,
                directory: false,
                overwrite: false,
                timeout: DEFAULT_TIMEOUT,
                newline: newline.into(),
                protocol: 0,
                max_buf_size: MAX_BUF_SIZE,
                tmux_output_junk: false,
            },
        }
    }

    /// 读一行的截止时间（`#CFG` 中的 timeout 已限制在 1 到 MAX_TIMEOUT 秒之间，见 recv_config）。
    fn deadline(&self) -> Instant {
        Instant::now() + Duration::from_secs(self.cfg.timeout.max(1) as u64)
    }

    /// 发送一行（取消由读取一侧判断：取消后仍要能回复远端不确认、出错）。
    fn send_line(&self, typ: &str, payload: &str) {
        let line = format!("#{typ}:{payload}{}", self.cfg.newline);
        self.link.out.write(line.as_bytes());
    }

    fn send_string(&self, typ: &str, s: &str) {
        self.send_line(typ, &encode_bytes(s.as_bytes()));
    }

    fn send_int(&self, typ: &str, v: i64) {
        self.send_line(typ, &v.to_string());
    }

    /// 读下一段远端输出：用户取消时返回 [`Halt::Cancelled`]。
    fn next(&self, deadline: Instant) -> Result<Vec<u8>, Halt> {
        self.link.pipe.next(Some(deadline)).map_err(|e| match e {
            PipeError::Cancelled => Halt::Cancelled,
            PipeError::Timeout => failed(XferError::Timeout),
            PipeError::Closed => failed(XferError::Disconnected),
        })
    }

    /// 读一行（不含换行）：远端在终端里按了 Ctrl+C 时中止；`may_have_junk` 时以 `\r\n` 结尾的是终端的其他输出，跳过。
    fn read_line(&self, may_have_junk: bool, deadline: Instant) -> Result<Vec<u8>, Halt> {
        let mut line: Vec<u8> = Vec::new();
        loop {
            let mut buf = self.next(deadline)?;
            let idx = memchr::memchr(b'\n', &buf);
            if let Some(i) = idx {
                self.link.pipe.unread(&buf[i + 1..]);
                buf.truncate(i);
            }
            if buf.contains(&0x03) {
                return Err(failed(XferError::RemoteInterrupted));
            }
            if line.len() + buf.len() > MAX_LINE_LEN {
                return Err(failed(XferError::LineTooLong));
            }
            line.extend_from_slice(&buf);
            if idx.is_some() {
                if may_have_junk && line.last() == Some(&b'\r') {
                    line.clear();
                    continue;
                }
                return Ok(line);
            }
        }
    }

    /// 读一行（以 `!` 结束）：跳过终端控制序列与换行，只保留协议用到的字符；
    /// 光标移动后可能重复输出的字符去掉（与 trzsz-go 的 readLineOnWindows 相同）。
    fn read_line_windows(&self, deadline: Instant) -> Result<Vec<u8>, Halt> {
        let mut line: Vec<u8> = Vec::new();
        let mut last_byte = 0x1bu8;
        let mut skip_vt100 = false;
        let mut has_newline = false;
        let mut may_duplicate = false;
        let mut has_cursor_home = false;
        let mut pre_has_cursor_home = false;
        loop {
            let mut buf = self.next(deadline)?;
            let end = memchr::memchr(b'!', &buf);
            if let Some(e) = end {
                let mut rest = &buf[e + 1..];
                if rest.first() == Some(&b'\n') {
                    rest = &rest[1..];
                }
                self.link.pipe.unread(rest);
                buf.truncate(e);
            }
            for &b in &buf {
                if b == 0x03 {
                    return Err(failed(XferError::RemoteInterrupted));
                }
                if b == b'\n' {
                    has_newline = true;
                }
                if skip_vt100 {
                    if b.is_ascii_alphabetic() {
                        skip_vt100 = false;
                        if b == b'H' && last_byte.is_ascii_digit() {
                            may_duplicate = true;
                        }
                    }
                    if last_byte == b'[' && b == b'H' {
                        has_cursor_home = true;
                    }
                    last_byte = b;
                } else if b == 0x1b {
                    skip_vt100 = true;
                    last_byte = b;
                } else if is_trzsz_letter(b) {
                    if may_duplicate {
                        may_duplicate = false;
                        if has_newline
                            && let Some(prev) = line.last_mut()
                            && (b == *prev || pre_has_cursor_home)
                        {
                            *prev = b;
                            continue;
                        }
                    }
                    if line.len() >= MAX_LINE_LEN {
                        return Err(failed(XferError::LineTooLong));
                    }
                    line.push(b);
                    pre_has_cursor_home = has_cursor_home;
                    has_cursor_home = false;
                    has_newline = false;
                }
            }
            if end.is_some() && !line.is_empty() && !skip_vt100 {
                return Ok(line);
            }
        }
    }

    /// 读一行 `#类型:内容`，返回内容；类型不符时，远端的 `#fail`、`#FAIL`、`#EXIT` 转为远端的错误。
    pub(crate) fn recv(&self, expect: &str, may_have_junk: bool) -> Result<Vec<u8>, Halt> {
        let deadline = self.deadline();
        let mut line = if self.link.windows {
            self.read_line_windows(deadline)?
        } else {
            self.read_line(may_have_junk || self.cfg.tmux_output_junk, deadline)?
        };
        if may_have_junk || self.link.windows || self.cfg.tmux_output_junk {
            let marker = format!("#{expect}:");
            if let Some(i) = memmem::rfind(&line, marker.as_bytes()) {
                line.drain(..i);
            } else if let Some(i) = memchr::memrchr(b'#', &line)
                && i > 0
            {
                line.drain(..i);
            }
        }
        let colon = memchr::memchr(b':', &line);
        let (Some(colon), true) = (colon, line.len() >= 2 && line[0] == b'#') else {
            let shown = String::from_utf8_lossy(&line[..line.len().min(80)]).into_owned();
            return Err(protocol_error(format!("收到的不是协议的行：{shown:?}")));
        };
        let typ = String::from_utf8_lossy(&line[1..colon]).into_owned();
        let payload = line.split_off(colon + 1);
        if typ == expect {
            return Ok(payload);
        }
        match typ.as_str() {
            "fail" | "FAIL" | "EXIT" => {
                let message = decode_bytes(&payload).unwrap_or(payload);
                Err(failed(XferError::Remote(
                    String::from_utf8_lossy(&message).trim().to_string(),
                )))
            }
            _ => Err(protocol_error(format!("应为 {expect}，收到 {typ}"))),
        }
    }

    fn recv_bytes(&self, typ: &str, may_have_junk: bool) -> Result<Vec<u8>, Halt> {
        let payload = self.recv(typ, may_have_junk)?;
        decode_bytes(&payload).map_err(failed)
    }

    fn recv_string(&self, typ: &str, may_have_junk: bool) -> Result<String, Halt> {
        Ok(String::from_utf8_lossy(&self.recv_bytes(typ, may_have_junk)?).into_owned())
    }

    fn recv_int(&self, typ: &str) -> Result<i64, Halt> {
        let payload = self.recv(typ, false)?;
        String::from_utf8_lossy(&payload)
            .trim()
            .parse()
            .map_err(|_| protocol_error(format!("{typ} 不是数字")))
    }

    fn check_int(&self, want: i64) -> Result<(), Halt> {
        let got = self.recv_int("SUCC")?;
        if got != want {
            return Err(protocol_error(format!("校验失败：{got} ≠ {want}")));
        }
        Ok(())
    }

    /// 回复是否接受这次传输；远端是 Windows（或经过 ConPTY）时声明行以 `!\n` 结束。
    pub(crate) fn send_action(&self, confirm: bool) {
        let action = serde_json::to_string(&Action {
            lang: "go",
            version: TRZSZ_VERSION,
            confirm,
            newline: &self.cfg.newline,
            binary: false,
            support_dir: true,
        })
        .unwrap_or_default();
        self.send_string("ACT", &action);
    }

    pub(crate) fn recv_config(&mut self) -> Result<(), Halt> {
        let s = self.recv_string("CFG", true)?;
        let remote: RemoteConfig = serde_json::from_str(&s)
            .map_err(|e| protocol_error(format!("#CFG 不是正确的 JSON：{e}")))?;
        let mut cfg = self.cfg.clone();
        cfg.quiet = remote.quiet.unwrap_or(cfg.quiet);
        cfg.binary = remote.binary.unwrap_or(cfg.binary);
        cfg.directory = remote.directory.unwrap_or(cfg.directory);
        cfg.overwrite = remote.overwrite.unwrap_or(cfg.overwrite);
        cfg.protocol = remote.protocol.unwrap_or(cfg.protocol);
        cfg.tmux_output_junk = remote.tmux_output_junk.unwrap_or(cfg.tmux_output_junk);
        // 我方每行都以它结尾：只接受协议定义的两种，否则远端（或伪造触发行的输出）能借它往 shell 里写命令
        if let Some(newline) = remote.newline {
            if newline != "\n" && newline != "!\n" {
                return Err(protocol_error(format!("#CFG 的换行不对：{newline:?}")));
            }
            cfg.newline = newline;
        }
        if cfg.binary {
            return Err(failed(XferError::UnsupportedTrzsz("二进制模式")));
        }
        if cfg.protocol > 1 {
            return Err(failed(XferError::UnsupportedTrzsz("新版协议")));
        }
        cfg.max_buf_size = match remote.bufsize {
            Some(size) if size > 0 && (size as u64) <= MAX_BUF_SIZE => size as u64,
            _ => MAX_BUF_SIZE,
        };
        cfg.timeout = match remote.timeout.unwrap_or(cfg.timeout) {
            t if t <= 0 => DEFAULT_TIMEOUT,
            t if t > MAX_TIMEOUT => MAX_TIMEOUT,
            t => t,
        };
        self.cfg = cfg;
        Ok(())
    }

    /// 出错后告诉远端（不是远端发来的错误时）：先丢掉远端还在发的数据，再发 `#fail`（远端会显示原因并退出）。
    fn fail(&self, halt: &Halt) {
        if from_remote(halt) {
            return;
        }
        self.link
            .pipe
            .drain(Duration::from_millis(300), Duration::from_secs(3));
        let message = match halt {
            Halt::Cancelled => "Stopped".to_string(),
            Halt::Failed(e) => e.to_string(),
        };
        self.send_line("fail", &encode_bytes(message.as_bytes()));
    }

    fn cancelled(&self) -> bool {
        (self.link.cancelled)()
    }
}

/// 不接受这次传输（用户拒绝、没有回答）。
pub(crate) fn refuse(link: Link<'_>) {
    Conn::new(link).send_action(false);
}

/// 接收远端 tsz 发来的文件（或目录），保存到 `dir`。
pub(crate) fn receive(link: Link<'_>, dir: &Path) -> Result<(), Halt> {
    let mut c = Conn::new(link);
    if !dir.is_dir() {
        c.send_action(false);
        return Err(failed(XferError::BadSaveDir(dir.to_path_buf())));
    }
    c.send_action(true);
    let mut created = Vec::new();
    let result = receive_files(&mut c, dir, &mut created);
    if let Err(halt) = &result {
        let halt = if c.cancelled() {
            &Halt::Cancelled
        } else {
            halt
        };
        c.fail(halt);
        // 中断时删除这次新建的文件与目录（只有这次新建的，见 prepare_target）
        for path in created.iter().rev() {
            let _ = if path.is_dir() {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            };
        }
    }
    result
}

fn receive_files(c: &mut Conn<'_>, dir: &Path, created: &mut Vec<PathBuf>) -> Result<(), Halt> {
    c.recv_config()?;
    let num = c.recv_int("NUM")?;
    c.send_int("SUCC", num);
    c.link.tracker.set_total(num.max(0) as usize, 0);
    // 传目录时，同一项（path_id）的顶层名称只改一次名
    let mut top_names: HashMap<usize, String> = HashMap::new();
    let mut saved: Vec<String> = Vec::new();
    for _ in 0..num {
        let name = c.recv_bytes("NAME", false)?;
        let target = prepare_target(c, dir, &name, &mut top_names, created)?;
        c.send_string("SUCC", &target.display);
        if let Some(file) = target.file {
            receive_file(c, &target.path, file)?;
        } else {
            c.link.tracker.saved(target.path.clone());
        }
        if !saved.contains(&target.display) {
            saved.push(target.display);
        }
    }
    c.send_string("EXIT", &saved_message(&saved, &dir.display().to_string()));
    Ok(())
}

/// 按 `#NAME` 新建的本机文件或目录。
struct Target {
    path: PathBuf,
    /// 回复给远端的名称（顶层名称）
    display: String,
    /// 新建的文件；目录为 `None`
    file: Option<File>,
}

/// 按 `#NAME` 新建本机的文件或目录。不理会远端的覆盖选项（`tsz -y`）：触发传输的是普通的终端输出，可以伪造。
/// 顶层名称已存在时一律改名，只写入这次新建的文件与目录（传目录时都在这次新建的顶层目录中），
/// 出错时删除的也只有它们（记入 `created` 的是新建的顶层文件与目录）。
fn prepare_target(
    c: &Conn<'_>,
    dir: &Path,
    name: &[u8],
    top_names: &mut HashMap<usize, String>,
    created: &mut Vec<PathBuf>,
) -> Result<Target, Halt> {
    if !c.cfg.directory {
        let (path, file) = create_unique(dir, &safe_name(name), false).map_err(failed)?;
        created.push(path.clone());
        return Ok(Target {
            display: base_name(&path),
            path,
            file,
        });
    }
    let desc: FileDesc = serde_json::from_slice(name)
        .ok()
        .filter(|d: &FileDesc| !d.path_name.is_empty())
        .ok_or_else(|| protocol_error("文件描述不正确"))?;
    if desc.archive {
        return Err(failed(XferError::UnsupportedTrzsz("归档传输")));
    }
    let top = match top_names.get(&desc.path_id) {
        Some(top) => top.clone(),
        None => {
            // 顶层是所选的文件或目录：新建（已有同名的改名），之后这一项的内容都写在它里面
            let top_file = desc.path_name.len() == 1 && !desc.is_dir;
            let (path, file) =
                create_unique(dir, &safe_name(desc.path_name[0].as_bytes()), !top_file)
                    .map_err(failed)?;
            created.push(path.clone());
            let top = base_name(&path);
            top_names.insert(desc.path_id, top.clone());
            if desc.path_name.len() == 1 {
                return Ok(Target {
                    path,
                    display: top,
                    file,
                });
            }
            top
        }
    };
    let mut path = dir.join(&top);
    for part in &desc.path_name[1..] {
        path.push(safe_name(part.as_bytes()));
    }
    if desc.is_dir {
        make_dir_all(&path).map_err(|e| failed(XferError::local(&path, &e)))?;
        return Ok(Target {
            path,
            display: top,
            file: None,
        });
    }
    if let Some(parent) = path.parent() {
        make_dir_all(parent).map_err(|e| failed(XferError::local(parent, &e)))?;
    }
    // 同一个文件重复出现时不覆盖前一个
    let file = create_new_file(&path).map_err(|e| failed(XferError::local(&path, &e)))?;
    Ok(Target {
        path,
        display: top,
        file: Some(file),
    })
}

fn base_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 接收一个文件的大小、数据与 MD5，写入 `file`（prepare_target 新建的文件）。
fn receive_file(c: &Conn<'_>, path: &Path, mut file: File) -> Result<(), Halt> {
    let size = c.recv_int("SIZE")?;
    c.send_int("SUCC", size);
    let size = size.max(0) as u64;
    let name = base_name(path);
    let mut hash = Md5::new();
    let mut step = 0u64;
    c.link.tracker.progress(&name, 0, size, 0);
    while step < size {
        let payload = c.recv("DATA", false)?;
        let data = decode_bytes(&payload).map_err(failed)?;
        file.write_all(&data)
            .map_err(|e| failed(XferError::local(path, &e)))?;
        hash.update(&data);
        step += data.len() as u64;
        c.link
            .tracker
            .progress(&name, step, size, data.len() as i64);
        c.send_int("SUCC", data.len() as i64);
    }
    let digest = hash.finalize();
    let want = c.recv_bytes("MD5", false)?;
    if want[..] != digest[..] {
        return Err(failed(XferError::Checksum));
    }
    c.send_line("SUCC", &encode_bytes(&digest));
    drop(file);
    c.link.tracker.saved(path.to_path_buf());
    Ok(())
}

/// 把选择的文件（`trz -d` 时可以是目录）发给远端。
pub(crate) fn send(link: Link<'_>, paths: &[PathBuf], directory: bool) -> Result<(), Halt> {
    let mut c = Conn::new(link);
    let (files, skipped) = match collect(paths, directory) {
        Ok(found) => found,
        Err(CollectError::Directory(path)) => {
            c.send_action(false);
            return Err(failed(XferError::TrzNeedsDirectory(path)));
        }
        Err(CollectError::Other(e)) => {
            c.send_action(false);
            return Err(failed(e));
        }
    };
    c.send_action(true);
    // 读不了的子目录（没有展开，见 collect）在结果中显示为跳过
    for name in &skipped {
        c.link.tracker.skipped(name);
    }
    let result = send_files(&mut c, files);
    if let Err(halt) = &result {
        let halt = if c.cancelled() {
            &Halt::Cancelled
        } else {
            halt
        };
        c.fail(halt);
    }
    result
}

fn send_files(c: &mut Conn<'_>, mut files: Vec<Source>) -> Result<(), Halt> {
    c.recv_config()?;
    if !c.cfg.directory {
        // 远端没有要目录（trz 不带 -d）：只发文件
        files.retain(|f| !f.is_dir);
    }
    if c.cfg.overwrite {
        let mut seen = std::collections::HashSet::new();
        for f in &files {
            let p = f.rel.join("/");
            if !seen.insert(p.clone()) {
                return Err(failed(XferError::DuplicateName(p)));
            }
        }
    }
    let total: u64 = files.iter().map(|f| f.size).sum();
    c.link.tracker.set_total(files.len(), total);
    c.send_int("NUM", files.len() as i64);
    c.check_int(files.len() as i64)?;
    let mut names: Vec<String> = Vec::new();
    for f in &files {
        let name = if c.cfg.directory {
            serde_json::to_string(&FileDesc {
                path_id: f.id,
                path_name: f.rel.clone(),
                is_dir: f.is_dir,
                archive: false,
                size: f.size,
                perm: Some(f.perm),
            })
            .unwrap_or_default()
        } else {
            f.name().to_string()
        };
        c.send_string("NAME", &name);
        let remote = c.recv_string("SUCC", false)?;
        if !names.contains(&remote) {
            names.push(remote.clone());
        }
        // 服务器上的路径：顶层是服务器回复的名称（可能改了名），其下与本机相同
        let shown = if f.rel.len() > 1 {
            format!("{remote}/{}", f.rel[1..].join("/"))
        } else {
            remote
        };
        if f.is_dir {
            c.link.tracker.uploaded(&shown);
            continue;
        }
        send_file(c, f, &shown)?;
    }
    c.send_string("EXIT", &saved_message(&names, ""));
    Ok(())
}

/// 发送一个文件的大小、数据与 MD5：数据块从 1KB 开始，发得快时加倍，最大到远端给的 bufsize，慢时退回 1KB。
fn send_file(c: &Conn<'_>, f: &Source, remote: &str) -> Result<(), Halt> {
    let mut file = File::open(&f.abs).map_err(|e| failed(XferError::local(&f.abs, &e)))?;
    let size = file
        .metadata()
        .map_err(|e| failed(XferError::local(&f.abs, &e)))?
        .len();
    c.send_int("SIZE", size as i64);
    c.check_int(size as i64)?;
    let mut hash = Md5::new();
    let mut buf_size = 1024u64;
    let mut buf = vec![0u8; buf_size as usize];
    let mut step = 0u64;
    c.link.tracker.progress(f.name(), 0, size, 0);
    while step < size {
        let begin = Instant::now();
        let n = buf_size.min(size - step) as usize;
        file.read_exact(&mut buf[..n])
            .map_err(|e| failed(XferError::local(&f.abs, &e)))?;
        let data = &buf[..n];
        c.send_line("DATA", &encode_bytes(data));
        hash.update(data);
        c.check_int(n as i64)?;
        step += n as u64;
        c.link.tracker.progress(f.name(), step, size, n as i64);
        let elapsed = begin.elapsed();
        if n as u64 == buf_size
            && elapsed < Duration::from_millis(500)
            && buf_size < c.cfg.max_buf_size
        {
            buf_size = (buf_size * 2).min(c.cfg.max_buf_size);
            buf = vec![0u8; buf_size as usize];
        } else if elapsed >= Duration::from_secs(2) && buf_size > 1024 {
            buf_size = 1024;
            buf = vec![0u8; buf_size as usize];
        }
    }
    let digest = hash.finalize();
    c.send_line("MD5", &encode_bytes(&digest));
    let got = c.recv_bytes("SUCC", false)?;
    if got[..] != digest[..] {
        return Err(failed(XferError::Checksum));
    }
    c.link.tracker.uploaded(remote);
    Ok(())
}

/// 发给远端的结束语（远端显示在终端中，与 trzsz-go 的格式相同）。
fn saved_message(names: &[String], dir: &str) -> String {
    let what = if names.len() > 1 { "files" } else { "file" };
    let mut s = format!("Saved {} {what}", names.len());
    if !dir.is_empty() {
        s.push_str(&format!(" to {dir}"));
    }
    for n in names {
        s.push_str("\r\n- ");
        s.push_str(n);
    }
    s
}

fn is_trzsz_letter(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'#' | b':' | b'+' | b'/' | b'=')
}
