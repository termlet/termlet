//! 会话日志：把终端的输出写成纯文本文件。
//!
//! - 去掉控制序列；回车后的输出覆盖同一行（进度条只留最后的样子），退格删除前一个字符；每行前加本地时间
//!   `[YYYY-MM-DD HH:MM:SS]`。全屏程序（备用屏幕）中的输出不记录。
//! - 文件在 `<目录>/<会话名称>/<YYYYMMDD-HHMMSS>-<n>.log`：目录见 [`log_root`]，会话名称经 [`dir_name`] 清理，
//!   同一秒开始的多个日志用 n 区分。设置 `terminal.session_log` 开启时每个终端打开时自动开始记录（由界面决定）。
//! - `[Termlet] …` 是 Termlet 自己的说明：开始、停止、进入全屏程序，以及调用方传入的断线、重连等（[`SessionLog::note`]）。
//! - 一个文件最多 [`MAX_FILE_BYTES`]，到了写一条说明后不再记录。
//! - 只记输出、不记输入：sudo 等提示下输入的密码远端不回显，不会进入日志（没有另外的过滤）。
//!
//! 写入不阻塞调用方：输出复制一份交给这个日志自己的后台线程，在那里解析、写文件。后台积压过多时丢弃新的输出，
//! 并在丢弃的位置注明丢了多少。写文件失败、达到大小上限时只报告一次（[`Halt`]），之后的输出静默丢弃。
//!
//! 退出程序前调用 [`stop_all`]：停止全部日志，等后台线程写完最后的输出与“停止记录”（合计等待有上限）。

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

use crate::model::LogSettings;
use crate::paths::DataDir;
use crate::{CoreError, Result, platform};

/// 一个日志文件的大小上限，超过后不再记录（避免大量输出写满磁盘）。
pub const MAX_FILE_BYTES: u64 = 100 << 20;
/// 一行的最大长度，超过时直接写出，剩下的另起一行。
const MAX_LINE: usize = 64 << 10;
/// 已交给后台线程、还没有写出的输出的上限：磁盘跟不上时丢弃新的输出，不无限占用内存。
const MAX_BACKLOG: usize = 16 << 20;
/// 识别备用屏幕切换时最多保留的 CSI 参数长度。
const MAX_PARAMS: usize = 32;
/// 日志文件名中的时间。
const FILE_TIME: &str = "%Y%m%d-%H%M%S";
/// 每行开头的时间。
const LINE_TIME: &str = "%Y-%m-%d %H:%M:%S";

/// 会话日志的目录：设置中的 `session_log_dir`（去掉首尾空白），留空时是数据目录下的 `session-logs`。
pub fn log_root(settings: &LogSettings, data_dir: &DataDir) -> PathBuf {
    match settings.session_log_dir.trim() {
        "" => data_dir.session_logs_dir(),
        dir => PathBuf::from(dir),
    }
}

/// 把会话名称转为可用作目录名的形式：Windows 文件名中不能用的字符与控制字符换成 `_`，最多 64 个字符，
/// 去掉首尾空白与末尾的点，避开设备名（`CON`、`nul.txt` 等）；什么也不剩时为 `session`。
pub fn dir_name(title: &str) -> String {
    let mapped: String = title
        .chars()
        .map(|c| {
            if c.is_control() || r#"<>:"/\|?*"#.contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let mut name: String = mapped.trim().chars().take(64).collect();
    let keep = name.trim_end_matches([' ', '.']).len();
    name.truncate(keep);
    if name.is_empty() {
        // 包括只有点的名称（`.`、`..`）
        return "session".into();
    }
    if is_reserved(&name) {
        name.insert(0, '_');
    }
    name
}

/// Windows 的保留设备名（不区分大小写，带扩展名也算）：写这样的文件会写到设备上。其他平台上同样避开，保持一致。
fn is_reserved(name: &str) -> bool {
    let base = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_uppercase();
    matches!(
        base.as_bytes(),
        b"CON"
            | b"PRN"
            | b"AUX"
            | b"NUL"
            | [b'C', b'O', b'M', b'1'..=b'9']
            | [b'L', b'P', b'T', b'1'..=b'9']
    )
}

/// 日志在调用方停止之前自己停下的原因，至多报告一次。
#[derive(Debug)]
pub enum Halt {
    /// 写文件失败（磁盘已满、所在的盘被拔出等），之后的输出丢弃
    Failed(CoreError),
    /// 达到 [`MAX_FILE_BYTES`]，已在日志末尾注明
    Full,
}

/// 一个正在记录的会话日志。可以在线程间共享（放进 `Arc`）：解析线程写输出，界面线程写说明。
///
/// [`SessionLog::stop`] 或丢弃时停止：后台线程写完已收到的输出、写一行"停止记录"后结束。
#[derive(Debug)]
pub struct SessionLog {
    path: PathBuf,
    tx: mpsc::Sender<Message>,
    shared: Arc<Shared>,
    /// 测试中等这个日志写完（[`SessionLog::wait_stopped`]）；退出前等全部日志见 [`stop_all`]
    #[cfg(test)]
    finished: Arc<Finished>,
}

#[derive(Debug, Default)]
struct Shared {
    /// 已停止、写入失败或已满：不再接收输出
    closed: AtomicBool,
    /// 已交给后台线程、还没有处理的字节数
    backlog: AtomicUsize,
    /// 因积压过多丢弃的字节数：下一段交给后台线程的输出带上它，在那个位置注明
    dropped: AtomicUsize,
}

/// 后台线程结束时置位（[`stop_all`] 等它；后台线程 panic 时同样置位）。
#[derive(Debug, Default)]
struct Finished {
    done: Mutex<bool>,
    changed: Condvar,
}

impl Finished {
    fn set(&self) {
        *self.done.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.changed.notify_all();
    }

    fn is_set(&self) -> bool {
        *self.done.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 等到置位，最多等到 `deadline`；返回是否已置位。
    fn wait_until(&self, deadline: Instant) -> bool {
        let mut done = self.done.lock().unwrap_or_else(|e| e.into_inner());
        while !*done {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            done = self
                .changed
                .wait_timeout(done, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        true
    }
}

/// 后台线程结束（包括 panic）时置位 [`Finished`]。
struct SetOnDrop(Arc<Finished>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.set();
    }
}

/// 后台线程还没结束的日志：退出前据此停止全部日志、等它们写完（[`stop_all`]）。关闭窗口时其中的终端随之丢弃、
/// 日志已经停止，后台线程却可能还在写，所以不能只停还开着的终端的日志。
static RUNNING: Mutex<Vec<Running>> = Mutex::new(Vec::new());

/// [`RUNNING`] 中的一项。其中的 `tx` 让通道一直连着也无妨：后台线程总是由“停止”消息或自己停下而结束，
/// 不靠通道断开（丢弃 [`SessionLog`] 时会发“停止”，自己停下时已经结束）。
#[derive(Clone)]
struct Running {
    tx: mpsc::Sender<Message>,
    shared: Arc<Shared>,
    finished: Arc<Finished>,
}

fn running() -> MutexGuard<'static, Vec<Running>> {
    RUNNING.lock().unwrap_or_else(|e| e.into_inner())
}

/// 停止全部还在记录的会话日志，等它们的后台线程写完已收到的输出与“停止记录”（退出程序前调用，免得丢掉
/// 最后的输出），合计最多等 `timeout`。返回是否都已写完。
pub fn stop_all(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let list = running().clone();
    for log in &list {
        request_stop(&log.tx, &log.shared);
    }
    let unfinished = list
        .iter()
        .filter(|log| !log.finished.wait_until(deadline))
        .count();
    running().retain(|log| !log.finished.is_set());
    if unfinished > 0 {
        tracing::warn!(unfinished, "退出前还有会话日志没有写完");
    }
    unfinished == 0
}

/// 停止记录（[`SessionLog::stop`] 与 [`stop_all`] 共用）：之后的写入不再生效，后台线程写完已收到的输出后结束。
fn request_stop(tx: &mpsc::Sender<Message>, shared: &Shared) {
    if !shared.closed.swap(true, Ordering::Relaxed) {
        tracing::info!("停止记录会话日志");
        let _ = tx.send(Message::Stop);
    }
}

#[derive(Debug)]
enum Message {
    Output {
        data: Vec<u8>,
        dropped_before: usize,
    },
    Note(String),
    Reset {
        alt_screen: bool,
    },
    Stop,
}

impl SessionLog {
    /// 开始记录：在 `root`（见 [`log_root`]）下按会话名称 `title` 建目录、新建日志文件，写入开头的说明，
    /// 返回之后就可以写入。`title` 是主机名称（临时连接为 `用户@地址`，本地终端为 shell 的名称），
    /// 既是子目录名，也写在日志开头。
    ///
    /// 创建目录与文件是同步的文件操作（网络盘上可能较慢），界面应在后台执行器上调用；之后的写入都不阻塞。
    /// `on_halt` 在日志自己停下时（写入失败、达到大小上限）由后台线程调用，至多一次；调用方停止的不调用。
    pub fn start(
        root: &Path,
        title: &str,
        on_halt: impl FnOnce(Halt) + Send + 'static,
    ) -> Result<Self> {
        Self::start_with(root, title, MAX_FILE_BYTES, Box::new(on_halt))
    }

    fn start_with(
        root: &Path,
        title: &str,
        limit: u64,
        on_halt: Box<dyn FnOnce(Halt) + Send>,
    ) -> Result<Self> {
        let dir = root.join(dir_name(title));
        prepare_dir(root)?;
        prepare_dir(&dir)?;
        let stamp = chrono::Local::now().format(FILE_TIME).to_string();
        let (path, file) = create_file(&dir, &stamp)?;
        tracing::info!(path = %path.display(), limit, "开始记录会话日志");
        let mut file = LogFile::new(file, limit);
        let title: String = title
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        file.write_lines([note_line(&format!("开始记录 {title}"))]);
        file.flush();
        if let Some(e) = file.error.take() {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(context(e, format!("写入会话日志 {} 失败", path.display())));
        }

        let shared = Arc::new(Shared::default());
        let (tx, rx) = mpsc::channel();
        let finished = Arc::new(Finished::default());
        let on_exit = SetOnDrop(finished.clone());
        let worker = Worker {
            parser: Parser::default(),
            file,
            lines: Vec::new(),
            shared: shared.clone(),
            on_halt: Some(on_halt),
        };
        std::thread::Builder::new()
            .name("session-log".into())
            .spawn(move || {
                let _on_exit = on_exit;
                worker.run(rx);
            })
            .map_err(CoreError::Io)?;
        let mut list = running();
        list.retain(|log| !log.finished.is_set());
        list.push(Running {
            tx: tx.clone(),
            shared: shared.clone(),
            finished: finished.clone(),
        });
        drop(list);
        Ok(Self {
            path,
            tx,
            shared,
            #[cfg(test)]
            finished,
        })
    }

    /// 日志文件的路径（界面显示"正在记录到 …"、打开所在的文件夹）。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 是否已不再记录：调用方停止了，或写入失败、达到大小上限。
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Relaxed)
    }

    /// 记下一段输出（已转换为 UTF-8，可以在任意位置切开，包括多字节字符与控制序列的中间）。
    /// 不阻塞：复制一份交给后台线程；积压过多时丢弃。
    pub fn write(&self, output: &[u8]) {
        if output.is_empty() || self.is_closed() {
            return;
        }
        let len = output.len();
        if self.shared.backlog.fetch_add(len, Ordering::Relaxed) + len > MAX_BACKLOG {
            self.shared.backlog.fetch_sub(len, Ordering::Relaxed);
            self.shared.dropped.fetch_add(len, Ordering::Relaxed);
            return;
        }
        let dropped_before = self.shared.dropped.swap(0, Ordering::Relaxed);
        let message = Message::Output {
            data: output.to_vec(),
            dropped_before,
        };
        if self.tx.send(message).is_err() {
            // 后台线程已经结束（写入失败、已满）
            self.shared.backlog.fetch_sub(len, Ordering::Relaxed);
        }
    }

    /// 写一行 Termlet 自己的说明（`[Termlet] 连接已断开，正在重连` 等）；之前还没有换行的输出先写出。
    pub fn note(&self, text: &str) {
        if !self.is_closed() {
            let _ = self.tx.send(Message::Note(text.into()));
        }
    }

    /// 重置解析状态：终端换了传输（重连到新的 shell，旧连接留下的半个控制序列不再有效、备用屏幕已退出），
    /// 或开始记录时终端已在全屏程序中（`alt_screen` 为真，之后的输出等退出全屏程序再记录）。
    pub fn reset(&self, alt_screen: bool) {
        if !self.is_closed() {
            let _ = self.tx.send(Message::Reset { alt_screen });
        }
    }

    /// 停止记录：之后的写入不再生效（共享这个日志的其他线程也一样），后台线程写完已收到的输出后结束。不阻塞。
    pub fn stop(&self) {
        request_stop(&self.tx, &self.shared);
    }

    /// 等这个日志的后台线程结束，最多等 `timeout`；返回是否已结束（测试中等它写完；退出前用 [`stop_all`]）。
    #[cfg(test)]
    pub(crate) fn wait_stopped(&self, timeout: Duration) -> bool {
        self.finished.wait_until(Instant::now() + timeout)
    }
}

impl Drop for SessionLog {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 加上说明的 I/O 错误（保留错误种类，界面据此选用文案）。
fn context(error: io::Error, what: String) -> CoreError {
    CoreError::Io(io::Error::new(error.kind(), format!("{what}：{error}")))
}

/// 创建目录；是这次新建的就限制为只有当前用户能访问（已有的目录不动：可能是用户自己选的公共目录）。
/// 限制失败只记警告。
fn prepare_dir(dir: &Path) -> Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(dir)
        .map_err(|e| context(e, format!("创建会话日志目录 {} 失败", dir.display())))?;
    if let Err(e) = platform::restrict_dir(dir) {
        tracing::warn!(dir = %dir.display(), "无法限制会话日志目录的访问权限：{e}");
    }
    Ok(())
}

/// 新建 `<stamp>-<n>.log`：n 从 1 开始，已存在时加一（同一秒开始的多个会话）。
fn create_file(dir: &Path, stamp: &str) -> Result<(PathBuf, File)> {
    for n in 1..=1000 {
        let path = dir.join(format!("{stamp}-{n}.log"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(context(e, format!("创建会话日志 {} 失败", path.display())));
            }
        }
    }
    Err(CoreError::Io(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "同一秒内开始的会话日志太多",
    )))
}

fn note_line(text: &str) -> String {
    format!("[Termlet] {text}")
}

/// 后台线程：解析输出、写文件。
struct Worker {
    parser: Parser,
    file: LogFile,
    /// 解析出的整行，处理完一条消息就写进文件
    lines: Vec<String>,
    shared: Arc<Shared>,
    on_halt: Option<Box<dyn FnOnce(Halt) + Send>>,
}

impl Worker {
    fn run(mut self, rx: mpsc::Receiver<Message>) {
        loop {
            // 等下一条消息；已经到达的一并处理完再刷新文件
            let mut stop = match rx.recv() {
                Ok(message) => self.handle(message),
                Err(_) => true,
            };
            while !stop && !self.file.halted() {
                match rx.try_recv() {
                    Ok(message) => stop = self.handle(message),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => stop = true,
                }
            }
            if stop {
                self.close();
            }
            self.file.flush();
            if let Some(halt) = self.file.take_halt() {
                self.shared.closed.store(true, Ordering::Relaxed);
                if let Halt::Failed(e) = &halt {
                    tracing::warn!("会话日志写入失败，停止记录：{e}");
                }
                if let Some(on_halt) = self.on_halt.take() {
                    on_halt(halt);
                }
                return;
            }
            if stop {
                return;
            }
        }
    }

    /// 处理一条消息，返回是否要停止。
    fn handle(&mut self, message: Message) -> bool {
        match message {
            Message::Output {
                data,
                dropped_before,
            } => {
                self.shared.backlog.fetch_sub(data.len(), Ordering::Relaxed);
                if dropped_before > 0 {
                    self.parser
                        .note(&mut self.lines, &dropped_note(dropped_before));
                }
                self.parser.feed(&data, &mut self.lines);
            }
            Message::Note(text) => self.parser.note(&mut self.lines, &text),
            Message::Reset { alt_screen } => self.parser.reset(alt_screen, &mut self.lines),
            Message::Stop => return true,
        }
        self.file.write_lines(self.lines.drain(..));
        false
    }

    /// 停止：写出还没有换行的内容与"停止记录"。
    fn close(&mut self) {
        let dropped = self.shared.dropped.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            self.parser.note(&mut self.lines, &dropped_note(dropped));
        }
        self.parser.note(&mut self.lines, "停止记录");
        self.file.write_lines(self.lines.drain(..));
    }
}

fn dropped_note(bytes: usize) -> String {
    format!("写入跟不上，这里丢弃了约 {} KB 输出", bytes.div_ceil(1024))
}

/// 日志文件：每行加时间，统计大小，记下第一次出错。
struct LogFile {
    out: BufWriter<File>,
    size: u64,
    limit: u64,
    error: Option<io::Error>,
    full: bool,
    /// 已经报告过停下（失败或已满）
    reported: bool,
}

impl LogFile {
    fn new(file: File, limit: u64) -> Self {
        Self {
            out: BufWriter::with_capacity(32 << 10, file),
            size: 0,
            limit,
            error: None,
            full: false,
            reported: false,
        }
    }

    fn halted(&self) -> bool {
        self.error.is_some() || self.full
    }

    /// 写入整行；同一批的行用同一个时间（它们几乎同时到达）。
    fn write_lines(&mut self, lines: impl IntoIterator<Item = String>) {
        let mut stamp: Option<String> = None;
        for text in lines {
            if self.halted() {
                return;
            }
            let stamp =
                stamp.get_or_insert_with(|| chrono::Local::now().format(LINE_TIME).to_string());
            let text = if self.size + text.len() as u64 > self.limit {
                self.full = true;
                note_line(&format!(
                    "日志已超过 {}MB，之后的输出不再记录",
                    self.limit >> 20
                ))
            } else {
                text
            };
            match writeln!(self.out, "[{stamp}] {text}") {
                Ok(()) => self.size += (stamp.len() + text.len() + 4) as u64,
                Err(e) => self.error = Some(e),
            }
        }
    }

    fn flush(&mut self) {
        if self.error.is_none()
            && let Err(e) = self.out.flush()
        {
            self.error = Some(e);
        }
    }

    /// 第一次停下时返回原因（之后不再返回）。
    fn take_halt(&mut self) -> Option<Halt> {
        if self.reported {
            return None;
        }
        let halt = match self.error.take() {
            Some(e) => Halt::Failed(CoreError::Io(io::Error::new(
                e.kind(),
                format!("写入会话日志失败：{e}"),
            ))),
            None if self.full => Halt::Full,
            None => return None,
        };
        self.reported = true;
        // 出错后不再写：error 已取走，用 full 挡住之后的写入
        self.full = true;
        Some(halt)
    }
}

/// 解析控制序列的状态。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Escape,
    /// `ESC` 之后的中间字节（`ESC ( B` 选择字符集等），到终止字节结束
    EscapeIntermediate,
    Csi,
    Osc,
    OscEscape,
    /// DCS、SOS、PM、APC，以 ST 结束
    Str,
    StrEscape,
}

/// 把终端输出变成纯文本行。
#[derive(Debug, Default)]
struct Parser {
    line: Vec<u8>,
    state: State,
    /// 收到回车：之后的输出覆盖这一行（紧跟换行时不算）
    cr: bool,
    /// 在备用屏幕中（全屏程序），输出不记录
    alt: bool,
    /// 正在解析的 CSI 的参数（只用来识别备用屏幕的切换）
    params: Vec<u8>,
    /// 这个 CSI 有中间字节（不是 DEC 私有模式的设置）
    intermediate: bool,
}

impl Parser {
    /// 处理一段输出，整行加到 `out`。
    fn feed(&mut self, data: &[u8], out: &mut Vec<String>) {
        for &b in data {
            match self.state {
                State::Ground => self.ground(b, out),
                State::Escape => {
                    self.state = match b {
                        b'[' => {
                            self.params.clear();
                            self.intermediate = false;
                            State::Csi
                        }
                        b']' => State::Osc,
                        b'P' | b'X' | b'^' | b'_' => State::Str,
                        0x20..=0x2f => State::EscapeIntermediate,
                        0x1b => State::Escape,
                        // RIS（完全复位，`reset` 的输出）：退出备用屏幕、清屏
                        b'c' => {
                            self.set_alt(false, out);
                            self.line.clear();
                            self.cr = false;
                            State::Ground
                        }
                        _ => State::Ground,
                    }
                }
                State::EscapeIntermediate => match b {
                    0x20..=0x2f => {}
                    0x1b => self.state = State::Escape,
                    _ => self.state = State::Ground,
                },
                State::Csi => match b {
                    0x30..=0x3f => {
                        if self.params.len() < MAX_PARAMS {
                            self.params.push(b);
                        }
                    }
                    0x20..=0x2f => self.intermediate = true,
                    0x40..=0x7e => {
                        self.state = State::Ground;
                        self.csi_final(b, out);
                    }
                    // 控制序列被新的 ESC 打断
                    0x1b => self.state = State::Escape,
                    _ => {}
                },
                State::Osc => match b {
                    0x07 => self.state = State::Ground,
                    0x1b => self.state = State::OscEscape,
                    _ => {}
                },
                State::OscEscape => {
                    self.state = if b == b'\\' {
                        State::Ground
                    } else {
                        State::Osc
                    }
                }
                State::Str => {
                    if b == 0x1b {
                        self.state = State::StrEscape;
                    }
                }
                State::StrEscape => {
                    self.state = if b == b'\\' {
                        State::Ground
                    } else {
                        State::Str
                    }
                }
            }
        }
    }

    fn ground(&mut self, b: u8, out: &mut Vec<String>) {
        match b {
            0x1b => self.state = State::Escape,
            // 全屏程序中只解析控制序列（等它退出备用屏幕）
            _ if self.alt => {}
            b'\n' => self.emit(out),
            b'\r' => self.cr = true,
            0x08 => pop_char(&mut self.line),
            b'\t' | 0x20..=0x7e | 0x80..=0xff => {
                if self.cr {
                    self.line.clear();
                    self.cr = false;
                }
                self.line.push(b);
                if self.line.len() >= MAX_LINE {
                    self.emit_long(out);
                }
            }
            _ => {}
        }
    }

    /// CSI 结束：`CSI ? 1049 h` 等进入、`… l` 退出备用屏幕（47、1047、1049 都算）。
    fn csi_final(&mut self, b: u8, out: &mut Vec<String>) {
        if self.intermediate || !matches!(b, b'h' | b'l') {
            return;
        }
        let Some(params) = self.params.strip_prefix(b"?") else {
            return;
        };
        if params
            .split(|&c| c == b';')
            .any(|p| matches!(p, b"47" | b"1047" | b"1049"))
        {
            self.set_alt(b == b'h', out);
        }
    }

    fn set_alt(&mut self, alt: bool, out: &mut Vec<String>) {
        if alt == self.alt {
            return;
        }
        if alt {
            self.emit_pending(out);
            out.push(note_line("进入全屏程序，期间的输出不记录"));
        }
        self.line.clear();
        self.cr = false;
        self.alt = alt;
    }

    /// 写一行说明：之前还没有换行的内容先写出。
    fn note(&mut self, out: &mut Vec<String>, text: &str) {
        self.emit_pending(out);
        out.push(note_line(text));
    }

    fn reset(&mut self, alt_screen: bool, out: &mut Vec<String>) {
        self.emit_pending(out);
        self.state = State::Ground;
        self.params.clear();
        self.cr = false;
        self.set_alt(alt_screen, out);
    }

    fn emit(&mut self, out: &mut Vec<String>) {
        self.cr = false;
        let text = String::from_utf8_lossy(&self.line);
        out.push(text.trim_end_matches([' ', '\t']).to_string());
        self.line.clear();
    }

    /// 写出还没有换行的内容（例如提示符）。
    fn emit_pending(&mut self, out: &mut Vec<String>) {
        if !self.line.is_empty() {
            self.emit(out);
        }
    }

    /// 一行太长：在最后一个完整的字符后断开写出，没写完的多字节字符留到下一行。
    fn emit_long(&mut self, out: &mut Vec<String>) {
        let rest = self.line.split_off(complete_prefix(&self.line));
        self.emit(out);
        self.line = rest;
    }
}

/// 删除最后一个字符（UTF-8 的多字节字符整个删除）。
fn pop_char(line: &mut Vec<u8>) {
    while let Some(b) = line.pop() {
        if b & 0xc0 != 0x80 {
            break;
        }
    }
}

/// `bytes` 末尾不完整的 UTF-8 字符之前的长度（末尾完整时为全长）。
fn complete_prefix(bytes: &[u8]) -> usize {
    let len = bytes.len();
    let mut start = len;
    while start > 0 && len - start < 4 {
        start -= 1;
        if bytes[start] & 0xc0 != 0x80 {
            break;
        }
    }
    let need = match bytes.get(start) {
        Some(b) if b & 0x80 == 0 => 1,
        Some(b) if b & 0xe0 == 0xc0 => 2,
        Some(b) if b & 0xf0 == 0xe0 => 3,
        Some(b) if b & 0xf8 == 0xf0 => 4,
        _ => return len,
    };
    if start + need > len { start } else { len }
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;

    use regex::Regex;

    use super::*;

    static STAMP: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)^\[\d{4}-\d\d-\d\d \d\d:\d\d:\d\d\] ").unwrap());

    /// 读出日志：每行都应带时间，返回去掉时间后的内容。
    fn read_log(path: &Path) -> String {
        let data = fs::read_to_string(path).unwrap();
        assert_eq!(
            STAMP.find_iter(&data).count(),
            data.matches('\n').count(),
            "每行都应带时间：\n{data}"
        );
        STAMP.replace_all(&data, "").into_owned()
    }

    fn parse(chunks: &[&[u8]]) -> Vec<String> {
        let mut parser = Parser::default();
        let mut out = Vec::new();
        for chunk in chunks {
            parser.feed(chunk, &mut out);
        }
        parser.emit_pending(&mut out);
        out
    }

    const SAMPLE: &[u8] =
        b"hello\r\nprog 10%\rprog 100%\r\nfoo\x08o\r\n\x1b[31mred\x1b]0;title\x07\x1b[0m\r\n\
\x1b[?1049hvim-screen\r\nmore\x1b[?1049l$ ";

    fn sample_lines() -> Vec<String> {
        [
            "hello",
            "prog 100%",
            "foo",
            "red",
            "[Termlet] 进入全屏程序，期间的输出不记录",
            "$",
        ]
        .map(String::from)
        .to_vec()
    }

    // 去掉控制序列，回车覆盖同一行，退格删除，全屏程序的输出不记录，每行带时间。
    #[test]
    fn writes_plain_text_lines_with_time() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("logs");
        let log = SessionLog::start(&root, "web-1", |_| panic!("不应停下")).unwrap();
        let path = log.path().to_path_buf();
        assert_eq!(path.parent().unwrap(), root.join("web-1"));
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            Regex::new(r"^\d{8}-\d{6}-1\.log$").unwrap().is_match(&name),
            "{name}"
        );
        log.write(
            b"hello\r\nprog 10%\rprog 100%\r\nfoo\x08o\r\n\x1b[31mred\x1b]0;title\x07\x1b[0m\r\n",
        );
        log.write(b"\x1b[?1049hvim-screen");
        log.write(b"\x1b[?1049l$ ");
        log.stop();
        assert!(log.wait_stopped(Duration::from_secs(5)));
        assert!(log.is_closed());
        let want = "[Termlet] 开始记录 web-1\nhello\nprog 100%\nfoo\nred\n\
[Termlet] 进入全屏程序，期间的输出不记录\n$\n[Termlet] 停止记录\n";
        assert_eq!(read_log(&path), want);
        // 停止后的写入不生效
        log.write(b"late\r\n");
        assert_eq!(read_log(&path), want);
    }

    // 输出可以在任意位置切开：多字节字符、控制序列的中间
    #[test]
    fn output_split_anywhere_gives_same_lines() {
        assert_eq!(parse(&[SAMPLE]), sample_lines());
        let bytes: Vec<&[u8]> = SAMPLE.chunks(1).collect();
        assert_eq!(parse(&bytes), sample_lines());
        let text = "中文输出 ✓ 😀\r\n\x1b[1;32m绿色\x1b[0m\r\n".as_bytes();
        let want = vec!["中文输出 ✓ 😀".to_string(), "绿色".to_string()];
        for cut in 0..text.len() {
            assert_eq!(
                parse(&[&text[..cut], &text[cut..]]),
                want,
                "在 {cut} 处切开"
            );
        }
    }

    #[test]
    fn strips_other_control_sequences() {
        // 选择字符集（tput sgr0 的输出）、DCS、APC、以 ST 结束的 OSC、铃声、DEL
        let lines = parse(&[
            b"\x1b(Bplain\x1b[m\x07\x7f\r\n\x1bPq#0;2;0;0;0\x1b\\dcs\r\n\x1b_apc\x1b\\x\x1b]8;;http://a\x1b\\link\x1b]8;;\x1b\\\r\n",
        ]);
        assert_eq!(lines, vec!["plain", "dcs", "xlink"]);
        // 制表符保留，行末空白去掉
        assert_eq!(parse(&[b"a\tb \t \r\n"]), vec!["a\tb"]);
        // 不是备用屏幕的私有模式（光标显示、括号粘贴）不影响记录
        assert_eq!(parse(&[b"\x1b[?25l\x1b[?2004hok\r\n"]), vec!["ok"]);
    }

    #[test]
    fn backspace_removes_whole_character() {
        assert_eq!(parse(&["中文\x08\r\n".as_bytes()]), vec!["中"]);
        assert_eq!(parse(&[b"ab\x08\x08\x08c\r\n"]), vec!["c"]);
        // 被切开的多字节字符之后才退格
        let text = "é\x08x\r\n".as_bytes();
        assert_eq!(parse(&[&text[..1], &text[1..]]), vec!["x"]);
    }

    #[test]
    fn alt_screen_inside_one_chunk() {
        let lines = parse(&[
            b"before\r\n\x1b[?1049hjunk\r\n\x1b[2J\x1b[?1049lafter\r\n\x1b[?47hx\x1b[?47l\x1b[?1047hy\x1b[?1047l",
        ]);
        let note = "[Termlet] 进入全屏程序，期间的输出不记录";
        assert_eq!(lines, vec!["before", note, "after", note, note]);
        // 一次设置多个模式
        assert_eq!(
            parse(&[b"p$ \x1b[?1;1049hvim\x1b[?1049;1l$ ok\r\n"]),
            vec!["p$", note, "$ ok"]
        );
    }

    // 全屏程序被强制结束后执行 reset：输出里只有 RIS，没有退出备用屏幕的序列，之后照常记录
    #[test]
    fn full_reset_leaves_alt_screen() {
        let note = "[Termlet] 进入全屏程序，期间的输出不记录";
        assert_eq!(
            parse(&[b"$ vim\r\n\x1b[?1049hjunk\r\n", b"\x1bc$ ok\r\n"]),
            vec!["$ vim", note, "$ ok"]
        );
        // 不在全屏程序中时清掉还没换行的内容
        assert_eq!(parse(&[b"half\x1bcnew\r\n"]), vec!["new"]);
    }

    #[test]
    fn reset_starts_over() {
        let mut parser = Parser::default();
        let mut out = Vec::new();
        // 开始记录时已在全屏程序中
        parser.reset(true, &mut out);
        parser.feed(b"vim junk\x1b[?1049l$ ls\r\n", &mut out);
        // 旧连接留下半个控制序列，重连后重置
        parser.feed(b"partial\x1b]0;tit", &mut out);
        parser.reset(false, &mut out);
        parser.feed(b"new shell\r\n", &mut out);
        assert_eq!(
            out,
            vec![
                "[Termlet] 进入全屏程序，期间的输出不记录",
                "$ ls",
                "partial",
                "new shell"
            ]
        );
    }

    #[test]
    fn long_line_is_split_at_character_boundary() {
        let mut data = vec![b'a'; MAX_LINE - 1];
        data.extend_from_slice("中b\r\n".as_bytes());
        let lines = parse(&[&data]);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), MAX_LINE - 1);
        assert_eq!(lines[1], "中b");
    }

    #[test]
    fn complete_prefix_finds_cut_point() {
        let s = "a中😀".as_bytes();
        assert_eq!(complete_prefix(s), s.len());
        assert_eq!(complete_prefix(&s[..2]), 1);
        assert_eq!(complete_prefix(&s[..3]), 1);
        assert_eq!(complete_prefix(&s[..4]), 4);
        assert_eq!(complete_prefix(&s[..7]), 4);
        assert_eq!(complete_prefix(b""), 0);
    }

    #[test]
    fn notes_flush_pending_output() {
        let tmp = tempfile::tempdir().unwrap();
        let log = SessionLog::start(tmp.path(), "web-1", |_| panic!("不应停下")).unwrap();
        log.write(b"$ ");
        log.note("连接已断开，正在重连");
        log.reset(false);
        log.note("已重新连接");
        log.write(b"$ uptime\r\n");
        let path = log.path().to_path_buf();
        // 丢弃即停止
        drop(log);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !read_log(&path).ends_with("停止记录\n") {
            assert!(std::time::Instant::now() < deadline, "停止后应写完");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            read_log(&path),
            "[Termlet] 开始记录 web-1\n$\n[Termlet] 连接已断开，正在重连\n[Termlet] 已重新连接\n\
$ uptime\n[Termlet] 停止记录\n"
        );
    }

    #[test]
    fn stops_when_file_is_full() {
        let tmp = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let log = SessionLog::start_with(
            tmp.path(),
            "web-1",
            200,
            Box::new(move |halt| tx.send(halt).unwrap()),
        )
        .unwrap();
        for i in 0..20 {
            log.write(format!("line {i}\r\n").as_bytes());
        }
        let halt = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(halt, Halt::Full), "{halt:?}");
        assert!(log.wait_stopped(Duration::from_secs(5)));
        assert!(log.is_closed());
        let text = read_log(log.path());
        assert!(
            text.ends_with("[Termlet] 日志已超过 0MB，之后的输出不再记录\n"),
            "{text}"
        );
        assert!(!text.contains("line 19"));
        assert!(fs::metadata(log.path()).unwrap().len() < 300);
        // 只报告一次
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    }

    #[test]
    fn dropped_output_is_noted_where_it_happened() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, file) = create_file(tmp.path(), "20261016-120000").unwrap();
        let mut worker = Worker {
            parser: Parser::default(),
            file: LogFile::new(file, MAX_FILE_BYTES),
            lines: Vec::new(),
            shared: Arc::default(),
            on_halt: None,
        };
        worker.shared.backlog.store(7, Ordering::Relaxed);
        worker.handle(Message::Output {
            data: b"a\r\nb".to_vec(),
            dropped_before: 0,
        });
        worker.handle(Message::Output {
            data: b"c\r\n".to_vec(),
            dropped_before: 3000,
        });
        worker.close();
        worker.file.flush();
        assert_eq!(worker.shared.backlog.load(Ordering::Relaxed), 0);
        let path = tmp.path().join("20261016-120000-1.log");
        assert_eq!(
            read_log(&path),
            "a\nb\n[Termlet] 写入跟不上，这里丢弃了约 3 KB 输出\nc\n[Termlet] 停止记录\n"
        );
    }

    #[test]
    fn same_second_logs_get_different_numbers() {
        let tmp = tempfile::tempdir().unwrap();
        let (first, _) = create_file(tmp.path(), "20261016-120000").unwrap();
        let (second, _) = create_file(tmp.path(), "20261016-120000").unwrap();
        assert_eq!(first.file_name().unwrap(), "20261016-120000-1.log");
        assert_eq!(second.file_name().unwrap(), "20261016-120000-2.log");
    }

    #[test]
    fn start_fails_when_directory_cannot_be_created() {
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("file");
        fs::write(&blocker, b"x").unwrap();
        let error = SessionLog::start(&blocker, "web-1", |_| {}).unwrap_err();
        assert!(matches!(error, CoreError::Io(_)), "{error:?}");
        assert!(error.to_string().contains("创建会话日志目录"), "{error}");
    }

    #[test]
    fn new_directories_are_private() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("session-logs");
        let log = SessionLog::start(&root, "web-1", |_| {}).unwrap();
        assert!(platform::is_private_dir(&root));
        assert!(platform::is_private_dir(&root.join("web-1")));
        log.stop();
        assert!(log.wait_stopped(Duration::from_secs(5)));
    }

    #[test]
    fn directory_names_are_sanitized() {
        assert_eq!(dir_name("web-1"), "web-1");
        assert_eq!(dir_name(r#"a<b>c:d"e/f\g|h?i*j"#), "a_b_c_d_e_f_g_h_i_j");
        assert_eq!(dir_name("tab\there\n"), "tab_here_");
        assert_eq!(dir_name("  name. . "), "name");
        assert_eq!(dir_name(""), "session");
        assert_eq!(dir_name(".."), "session");
        assert_eq!(dir_name("   "), "session");
        assert_eq!(dir_name("con"), "_con");
        assert_eq!(dir_name("NUL.txt"), "_NUL.txt");
        assert_eq!(dir_name("com1"), "_com1");
        assert_eq!(dir_name("com10"), "com10");
        assert_eq!(dir_name("console"), "console");
        assert_eq!(dir_name("root@10.0.0.1"), "root@10.0.0.1");
        assert_eq!(dir_name("WSL Ubuntu"), "WSL Ubuntu");
        let long = "长".repeat(70);
        assert_eq!(dir_name(&long).chars().count(), 64);
        // 截断后末尾的点、空格也去掉
        let dotted = format!("{}. x", "a".repeat(63));
        assert_eq!(dir_name(&dotted), "a".repeat(63));
    }

    /// 解析线程与界面线程共用一个日志（编译时检查）。
    fn _session_log_is_shareable() {
        fn assert_shareable<T: Send + Sync>() {}
        assert_shareable::<SessionLog>();
    }

    #[test]
    fn log_root_defaults_to_data_dir() {
        let data = DataDir::at("/data/termlet");
        let mut settings = LogSettings::default();
        assert_eq!(log_root(&settings, &data), data.session_logs_dir());
        settings.session_log_dir = "  D:/logs  ".into();
        assert_eq!(log_root(&settings, &data), PathBuf::from("D:/logs"));
    }
}
