//! ZModem 与本机文件：接收时保存到所选目录（重名时改名，中断时删除写了一半的文件），发送所选的文件；
//! 中止时发中止序列并丢掉对方还在发的数据。

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use termlet_io::TerminalInput;

use super::receiver::{Outcome, ReceiveFiles};
use super::sender::SendFiles;
use super::{ABORT, Config, FileInfo, Offer, Session, ZError};
use crate::termxfer::files::{CollectError, Source, collect, create_unique, safe_name};
use crate::termxfer::info::{Protocol, XferError};
use crate::termxfer::pipe::{Pipe, PipeError};
use crate::termxfer::progress::Tracker;
use crate::termxfer::transfer::Halt;

/// 写入文件的缓冲大小。
const WRITE_BUFFER: usize = 256 * 1024;

/// 一次 ZModem 传输用到的连接。
pub(crate) struct Link<'a> {
    pub pipe: &'a Pipe,
    pub out: &'a dyn TerminalInput,
    pub tracker: &'a Tracker,
    /// 线路会改写控制字符（见 [`TerminalInput::mangles_control_bytes`]）
    pub escape_ctl: bool,
}

impl Link<'_> {
    fn config(&self) -> Config {
        Config {
            escape_ctl: self.escape_ctl,
            ..Config::default()
        }
    }
}

/// 发送中止序列，远端的 rz / sz 收到后退出；之后丢掉它还在发的二进制数据，
/// 远端退出后的文本（提示、shell 的提示符）交还给终端。
pub(crate) fn abort(link: &Link<'_>) {
    link.out.write(&ABORT);
    link.pipe
        .drain_binary(Duration::from_millis(500), Duration::from_secs(5));
}

pub(crate) fn halt_of(error: ZError) -> Halt {
    match error {
        ZError::Pipe(PipeError::Cancelled) => Halt::Cancelled,
        ZError::Pipe(PipeError::Timeout) => Halt::Failed(XferError::Timeout),
        ZError::Pipe(PipeError::Closed) => Halt::Failed(XferError::Disconnected),
        ZError::Aborted => Halt::Failed(XferError::RemoteAborted),
        ZError::Local(e) => Halt::Failed(e),
        e @ (ZError::Garbage | ZError::Frame(_) | ZError::Protocol(_)) => {
            Halt::Failed(XferError::protocol(Protocol::Zmodem, e.to_string()))
        }
    }
}

/// 接收远端 sz 发来的文件，保存到 `dir`（重名时改名）。
pub(crate) fn receive(link: &Link<'_>, dir: &Path) -> Result<(), Halt> {
    if !dir.is_dir() {
        abort(link);
        return Err(Halt::Failed(XferError::BadSaveDir(dir.to_path_buf())));
    }
    let mut saver = Saver {
        dir: dir.to_path_buf(),
        tracker: link.tracker,
        current: None,
        partial: Vec::new(),
        error: None,
    };
    let mut session = Session::new(link.pipe, link.out, link.config());
    let result = session.receive(&mut saver);
    session.finish();
    let result = result
        .map_err(halt_of)
        .and_then(|()| match saver.error.take() {
            Some(e) => Err(Halt::Failed(e)),
            None => Ok(()),
        });
    if result.is_err() {
        abort(link);
        saver.remove_partial();
    }
    result
}

/// 正在写的文件。
struct Writing {
    path: PathBuf,
    name: String,
    file: BufWriter<File>,
    last: u64,
}

/// 接收时的文件操作。
struct Saver<'a> {
    dir: PathBuf,
    tracker: &'a Tracker,
    current: Option<Writing>,
    /// 还没写完的文件：中断时删除
    partial: Vec<PathBuf>,
    /// 建不了文件、收尾（写盘）出错：传输结束后报告（之后的文件都跳过）
    error: Option<XferError>,
}

impl Saver<'_> {
    /// 删除中断时写了一半的文件（先关闭：Windows 上打开着的文件删不掉）。
    fn remove_partial(&mut self) {
        self.current = None;
        for path in self.partial.drain(..) {
            let _ = std::fs::remove_file(path);
        }
    }

    fn forget_partial(&mut self, path: &Path) {
        self.partial.retain(|p| p != path);
    }
}

impl ReceiveFiles for Saver<'_> {
    /// 在保存目录中创建文件：只取文件名的最后一级（防止 `../` 写到别处），重名时改为"名称 (1).扩展名"。
    fn accept(&mut self, info: &FileInfo) -> bool {
        if self.error.is_some() {
            return false;
        }
        let (path, file) = match create_unique(&self.dir, &safe_name(&info.name), false) {
            Ok((path, Some(file))) => (path, file),
            Ok((_, None)) => return false,
            // 没有写权限、磁盘满等：跳过这个文件（对方继续发下一个，也跳过），结束后报告原因
            Err(e) => {
                tracing::warn!("ZModem 接收：建不了文件：{e}");
                self.error = Some(e);
                return false;
            }
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.partial.push(path.clone());
        if info.files_left > 0 {
            let done = self.tracker.info().files;
            self.tracker.set_total(done + info.files_left as usize, 0);
        }
        self.tracker.progress(&name, 0, info.size, 0);
        self.current = Some(Writing {
            path,
            name,
            file: BufWriter::with_capacity(WRITE_BUFFER, file),
            last: 0,
        });
        true
    }

    fn write(&mut self, data: &[u8]) -> Result<(), ZError> {
        let Some(w) = self.current.as_mut() else {
            return Ok(());
        };
        w.file
            .write_all(data)
            .map_err(|e| ZError::Local(XferError::local(&w.path, &e)))
    }

    fn progress(&mut self, info: &FileInfo, received: u64) {
        if let Some(w) = self.current.as_mut() {
            let delta = received as i64 - w.last as i64;
            w.last = received;
            self.tracker.progress(&w.name, received, info.size, delta);
        }
    }

    fn completed(&mut self, info: &FileInfo, received: u64, outcome: Outcome) {
        let Some(mut w) = self.current.take() else {
            // 没有创建文件（accept 时跳过；出了错时不算跳过，按失败报告）
            if outcome == Outcome::Skipped && self.error.is_none() {
                self.tracker.skipped(&safe_name(&info.name));
            }
            return;
        };
        let delta = received as i64 - w.last as i64;
        self.tracker.progress(&w.name, received, info.size, delta);
        match outcome {
            Outcome::Done => {
                let flushed = w.file.flush();
                if flushed.is_ok()
                    && let Some(mtime) = info.mtime
                {
                    let _ = w.file.get_ref().set_modified(mtime);
                }
                drop(w.file);
                match flushed {
                    Ok(()) => {
                        self.forget_partial(&w.path);
                        self.tracker.saved(w.path);
                    }
                    Err(e) => {
                        let _ = std::fs::remove_file(&w.path);
                        self.forget_partial(&w.path);
                        self.error.get_or_insert(XferError::local(&w.path, &e));
                    }
                }
            }
            Outcome::Skipped | Outcome::Failed => {
                drop(w.file);
                let _ = std::fs::remove_file(&w.path);
                self.forget_partial(&w.path);
                if outcome == Outcome::Skipped {
                    self.tracker.skipped(&w.name);
                }
            }
        }
    }
}

/// 把选择的文件发给远端的 rz；ZModem 不能传目录。
pub(crate) fn send(link: &Link<'_>, paths: &[PathBuf]) -> Result<(), Halt> {
    let files = match collect(paths, false) {
        Ok((files, _)) => files,
        Err(CollectError::Directory(path)) => {
            abort(link);
            return Err(Halt::Failed(XferError::ZmodemDirectory(path)));
        }
        Err(CollectError::Other(e)) => {
            abort(link);
            return Err(Halt::Failed(e));
        }
    };
    let total: u64 = files.iter().map(|f| f.size).sum();
    link.tracker.set_total(files.len(), total);
    let mut feeder = Feeder {
        files,
        next: 0,
        current: None,
        last: 0,
        tracker: link.tracker,
    };
    let mut session = Session::new(link.pipe, link.out, link.config());
    let result = session.send(&mut feeder);
    session.finish();
    feeder.current = None;
    if let Err(e) = result {
        abort(link);
        return Err(halt_of(e));
    }
    Ok(())
}

/// 发送时的文件操作。
struct Feeder<'a> {
    files: Vec<Source>,
    next: usize,
    current: Option<(File, PathBuf)>,
    last: u64,
    tracker: &'a Tracker,
}

impl Feeder<'_> {
    fn file(&mut self) -> Result<(&mut File, &Path), ZError> {
        match self.current.as_mut() {
            Some((file, path)) => Ok((file, path)),
            None => Err(ZError::Protocol("没有正在发送的文件".into())),
        }
    }
}

fn local(path: &Path, e: &std::io::Error) -> ZError {
    ZError::Local(XferError::local(path, e))
}

impl SendFiles for Feeder<'_> {
    fn next_file(&mut self) -> Result<Option<Offer>, ZError> {
        self.current = None;
        while self.next < self.files.len() {
            let src = self.files[self.next].clone();
            self.next += 1;
            // 打不开（被删除、没有权限）的跳过
            let Ok(file) = File::open(&src.abs) else {
                self.tracker.skipped(src.name());
                continue;
            };
            let Ok(meta) = file.metadata() else {
                self.tracker.skipped(src.name());
                continue;
            };
            let rest = &self.files[self.next - 1..];
            let offer = Offer {
                name: src.name().to_string(),
                size: meta.len(),
                mtime: meta.modified().ok(),
                // 权限按普通文件 644 发送：Windows 上的权限位没有意义
                mode: 0o100644,
                files_left: rest.len() as u32,
                bytes_left: rest.iter().map(|f| f.size).sum(),
            };
            self.current = Some((file, src.abs.clone()));
            self.last = 0;
            self.tracker.progress(&offer.name, 0, offer.size, 0);
            return Ok(Some(offer));
        }
        Ok(None)
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize, ZError> {
        let (file, path) = self.file()?;
        loop {
            match file.read(buf) {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(local(path, &e)),
            }
        }
    }

    fn seek(&mut self, pos: u64) -> Result<(), ZError> {
        let (file, path) = self.file()?;
        file.seek(SeekFrom::Start(pos))
            .map(|_| ())
            .map_err(|e| local(path, &e))
    }

    fn crc32(&mut self, len: u64) -> Result<u32, ZError> {
        let (file, path) = self.file()?;
        let path = path.to_path_buf();
        let current = file.stream_position().map_err(|e| local(&path, &e))?;
        file.seek(SeekFrom::Start(0))
            .map_err(|e| local(&path, &e))?;
        let mut crc = flate2::Crc::new();
        let mut buf = vec![0u8; 64 * 1024];
        let mut left = if len == 0 { u64::MAX } else { len };
        while left > 0 {
            let want = (buf.len() as u64).min(left) as usize;
            let n = file.read(&mut buf[..want]).map_err(|e| local(&path, &e))?;
            if n == 0 {
                break;
            }
            crc.update(&buf[..n]);
            left -= n as u64;
        }
        file.seek(SeekFrom::Start(current))
            .map_err(|e| local(&path, &e))?;
        Ok(crc.sum())
    }

    fn progress(&mut self, offer: &Offer, sent: u64) {
        let delta = sent as i64 - self.last as i64;
        self.last = sent;
        self.tracker.progress(&offer.name, sent, offer.size, delta);
    }

    fn completed(&mut self, offer: &Offer, sent: u64, outcome: Outcome) {
        self.progress(offer, sent);
        self.current = None;
        match outcome {
            Outcome::Done => self.tracker.uploaded(&offer.name),
            // 远端拒收（例如已有同名文件，rz 没有加 -y）
            Outcome::Skipped => self.tracker.skipped(&offer.name),
            Outcome::Failed => {}
        }
    }
}
