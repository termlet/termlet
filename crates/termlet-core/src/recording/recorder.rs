//! 一个终端的录制：调用方（终端的输出旁路、界面）交来输出、输入与大小变化，后台线程按到达的时间写成 asciicast v2。
//!
//! 写入不阻塞调用方：数据复制一份交给后台线程。后台积压过多（磁盘跟不上）时丢弃新的输出，在丢弃的位置写一个标记
//! 注明丢了多少。写文件失败、达到 [`MAX_FILE_BYTES`] 时只报告一次（[`Halt`]），之后静默丢弃。后台线程每处理完一批
//! 就把缓冲写进文件：程序异常退出时文件停在最近的一批（最后一行可能不完整，读入时跳过），之前的照样能回放。
//!
//! 退出程序前调用 [`stop_all`]：停止全部录制，等后台线程写完（合计等待有上限）。

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

use super::cast::{CastHeader, CastTheme, EventData, Utf8Stream, header_line, push_event};
use crate::{CoreError, Result, platform};

/// 一个录制文件的大小上限，到了写一个标记后停止录制（避免大量输出写满磁盘；回放时整个读进内存）。
pub const MAX_FILE_BYTES: u64 = 256 << 20;
/// 已交给后台线程、还没有写出的输出的上限：磁盘跟不上时丢弃新的输出，不无限占用内存。
const MAX_BACKLOG: usize = 16 << 20;
/// 文件名中的时间。
const FILE_TIME: &str = "%Y%m%d-%H%M%S";
/// 头部的终端类型（与本地终端、SSH 申请的伪终端相同）。
const TERM: &str = "xterm-256color";

/// 开始录制时的信息。`Debug` 中屏幕内容只显示长度。
#[derive(Clone, Default)]
pub struct RecordingOptions {
    /// 主机名（临时连接为 `用户@地址`，本地终端为 shell 的名称）：文件名与头部的标题
    pub title: String,
    /// 开始时终端的列数、行数
    pub columns: u16,
    pub rows: u16,
    /// 同时记录输入（可能含密码，由用户在设置中开启）
    pub input: bool,
    /// 开始时屏幕上已有的内容（含颜色、光标位置的控制序列），写成第 0 秒的输出
    pub screen: Option<String>,
    /// 录制时的配色（asciinema 回放时据此着色）
    pub theme: Option<CastTheme>,
}

impl std::fmt::Debug for RecordingOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingOptions")
            .field("title", &self.title)
            .field("columns", &self.columns)
            .field("rows", &self.rows)
            .field("input", &self.input)
            .field("screen_bytes", &self.screen.as_ref().map(String::len))
            .finish_non_exhaustive()
    }
}

/// 录制在调用方停止之前自己停下的原因，至多报告一次。
#[derive(Debug)]
pub enum Halt {
    /// 写文件失败（磁盘已满、所在的盘被拔出等）
    Failed(CoreError),
    /// 达到 [`MAX_FILE_BYTES`]，已在文件末尾写了标记
    Full,
}

/// 一个正在进行的录制。可以在线程间共享（放进 `Arc`）：解析线程写输出，界面线程写输入与大小变化。
///
/// [`Recording::stop`] 或丢弃时停止：后台线程写完已收到的内容后关闭文件。
#[derive(Debug)]
pub struct Recording {
    path: PathBuf,
    tx: mpsc::Sender<Message>,
    shared: Arc<Shared>,
    started: Instant,
    input: bool,
    finished: Arc<Finished>,
}

#[derive(Debug, Default)]
struct Shared {
    /// 已停止、写入失败或已满：不再接收
    closed: AtomicBool,
    /// 已交给后台线程、还没有处理的字节数
    backlog: AtomicUsize,
    /// 因积压过多丢弃的字节数：下一段输出带上它，在那个位置写标记
    dropped: AtomicUsize,
}

/// 后台线程结束时置位（后台线程 panic 时同样置位）。
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

struct SetOnDrop(Arc<Finished>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.set();
    }
}

/// 后台线程还没结束的录制：退出前据此停止全部录制、等它们写完（[`stop_all`]）。
static RUNNING: Mutex<Vec<Running>> = Mutex::new(Vec::new());

#[derive(Clone)]
struct Running {
    tx: mpsc::Sender<Message>,
    shared: Arc<Shared>,
    finished: Arc<Finished>,
}

fn running() -> MutexGuard<'static, Vec<Running>> {
    RUNNING.lock().unwrap_or_else(|e| e.into_inner())
}

/// 停止全部还在进行的录制，等它们的后台线程写完（退出程序前调用），合计最多等 `timeout`。返回是否都已写完。
pub fn stop_all(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let list = running().clone();
    for recording in &list {
        request_stop(&recording.tx, &recording.shared);
    }
    let unfinished = list
        .iter()
        .filter(|recording| !recording.finished.wait_until(deadline))
        .count();
    running().retain(|recording| !recording.finished.is_set());
    if unfinished > 0 {
        tracing::warn!(unfinished, "退出前还有录制没有写完");
    }
    unfinished == 0
}

fn request_stop(tx: &mpsc::Sender<Message>, shared: &Shared) {
    if !shared.closed.swap(true, Ordering::Relaxed) {
        tracing::info!("停止录制");
        let _ = tx.send(Message::Stop);
    }
}

/// 交给后台线程的内容（不实现 `Debug`：输入里可能有密码）。
enum Message {
    Output {
        at: Instant,
        data: Vec<u8>,
        dropped_before: usize,
    },
    Input {
        at: Instant,
        data: Vec<u8>,
    },
    Resize {
        at: Instant,
        columns: u16,
        rows: u16,
    },
    Stop,
}

/// 建好了、还没有开始的录制文件（[`Recording::create`]）。丢弃时删掉这个空文件（建文件期间用户取消了）。
#[derive(Debug)]
pub struct PendingRecording {
    path: PathBuf,
    file: Option<File>,
}

impl PendingRecording {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 开始录制：把头部与开始时的屏幕写进缓冲，交给后台线程写进文件（这里不等文件操作，界面线程可以在拿到屏幕的
    /// 同一时刻调用）。`on_halt` 在录制自己停下时（写入失败、达到大小上限）由后台线程调用，至多一次；调用方停止的
    /// 不调用。
    pub fn begin(
        self,
        options: RecordingOptions,
        on_halt: impl FnOnce(Halt) + Send + 'static,
    ) -> Recording {
        self.begin_with(options, MAX_FILE_BYTES, Box::new(on_halt))
    }

    fn begin_with(
        mut self,
        options: RecordingOptions,
        limit: u64,
        on_halt: Box<dyn FnOnce(Halt) + Send>,
    ) -> Recording {
        let file = self.file.take().expect("录制文件只开始一次");
        let path = std::mem::take(&mut self.path);
        let now = chrono::Local::now();
        let started = Instant::now();
        let title: String = options
            .title
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let header = CastHeader {
            width: options.columns.max(1),
            height: options.rows.max(1),
            timestamp: Some(now.timestamp()),
            title: (!title.trim().is_empty()).then_some(title),
            term: Some(TERM.into()),
            theme: options.theme.clone(),
            ..CastHeader::default()
        };
        let mut worker = Worker {
            out: BufWriter::with_capacity(64 << 10, file),
            started,
            size: 0,
            limit,
            error: None,
            full: false,
            reported: false,
            output: Utf8Stream::default(),
            input: Utf8Stream::default(),
            grid: (header.width, header.height),
            clock: 0.,
            line: String::new(),
            shared: Arc::new(Shared::default()),
            on_halt: Some(on_halt),
        };
        worker.write_line(&header_line(&header));
        if let Some(screen) = options.screen.as_deref().filter(|s| !s.is_empty()) {
            worker.event(started, EventData::Output(screen.to_string()));
        }
        tracing::info!(path = %path.display(), input = options.input, "开始录制");

        let shared = worker.shared.clone();
        let (tx, rx) = mpsc::channel();
        let finished = Arc::new(Finished::default());
        let on_exit = SetOnDrop(finished.clone());
        let spawned = std::thread::Builder::new()
            .name("recording".into())
            .spawn(move || {
                let _on_exit = on_exit;
                worker.run(rx);
            });
        if let Err(e) = spawned {
            // 线程起不来（资源耗尽）：当作已停止，文件里只有头部
            tracing::error!("无法启动录制线程：{e}");
            shared.closed.store(true, Ordering::Relaxed);
            finished.set();
        }
        let mut list = running();
        list.retain(|r| !r.finished.is_set());
        list.push(Running {
            tx: tx.clone(),
            shared: shared.clone(),
            finished: finished.clone(),
        });
        drop(list);
        Recording {
            path,
            tx,
            shared,
            started,
            input: options.input,
            finished,
        }
    }
}

impl Drop for PendingRecording {
    fn drop(&mut self) {
        if self.file.take().is_some() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl Recording {
    /// 在 `root`（见 [`super::recordings_root`]）下建目录与录制文件，文件名由主机名 `title` 与现在的时间组成。
    /// 同步的文件操作（网络盘上可能较慢），界面应在后台执行器上调用；拿到屏幕后再 [`PendingRecording::begin`]。
    pub fn create(root: &Path, title: &str) -> Result<PendingRecording> {
        prepare_dir(root)?;
        let name = crate::session_log::dir_name(title);
        let stamp = chrono::Local::now().format(FILE_TIME).to_string();
        let (path, file) = create_file(root, &name, &stamp)?;
        Ok(PendingRecording {
            path,
            file: Some(file),
        })
    }

    /// 建文件并立即开始（[`Self::create`] 与 [`PendingRecording::begin`] 连在一起）。
    pub fn start(
        root: &Path,
        options: RecordingOptions,
        on_halt: impl FnOnce(Halt) + Send + 'static,
    ) -> Result<Self> {
        Ok(Self::create(root, &options.title)?.begin(options, on_halt))
    }

    /// 录制文件的路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 已经录了多久。
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// 是否同时记录输入。
    pub fn records_input(&self) -> bool {
        self.input
    }

    /// 是否已不再录制：调用方停止了，或写入失败、达到大小上限。
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Relaxed)
    }

    /// 记下一段远端的输出（UTF-8，可以在任意位置切开）。不阻塞：复制一份交给后台线程；积压过多时丢弃。
    pub fn output(&self, data: &[u8]) {
        if data.is_empty() || self.is_closed() {
            return;
        }
        let len = data.len();
        if self.shared.backlog.fetch_add(len, Ordering::Relaxed) + len > MAX_BACKLOG {
            self.shared.backlog.fetch_sub(len, Ordering::Relaxed);
            self.shared.dropped.fetch_add(len, Ordering::Relaxed);
            return;
        }
        let dropped_before = self.shared.dropped.swap(0, Ordering::Relaxed);
        let message = Message::Output {
            at: Instant::now(),
            data: data.to_vec(),
            dropped_before,
        };
        if self.tx.send(message).is_err() {
            self.shared.backlog.fetch_sub(len, Ordering::Relaxed);
        }
    }

    /// 记下用户的输入；没有开启“同时录制输入”时什么也不做。
    pub fn input(&self, data: &[u8]) {
        if self.input && !data.is_empty() && !self.is_closed() {
            let _ = self.tx.send(Message::Input {
                at: Instant::now(),
                data: data.to_vec(),
            });
        }
    }

    /// 终端大小变了（与上一次相同时不记）。
    pub fn resize(&self, columns: u16, rows: u16) {
        if !self.is_closed() {
            let _ = self.tx.send(Message::Resize {
                at: Instant::now(),
                columns,
                rows,
            });
        }
    }

    /// 停止录制：之后的写入不再生效，后台线程写完已收到的内容后关闭文件。不阻塞。
    pub fn stop(&self) {
        request_stop(&self.tx, &self.shared);
    }

    /// 等后台线程结束（文件已写完、关闭），最多等 `timeout`；返回是否已结束。会阻塞，界面应在后台执行器上调用。
    pub fn wait_stopped(&self, timeout: Duration) -> bool {
        self.finished.wait_until(Instant::now() + timeout)
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        self.stop();
    }
}

fn context(error: io::Error, what: String) -> CoreError {
    CoreError::Io(io::Error::new(error.kind(), format!("{what}：{error}")))
}

/// 创建目录；是这次新建的就限制为只有当前用户能访问（已有的目录不动：可能是用户自己选的公共目录）。
fn prepare_dir(dir: &Path) -> Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(dir)
        .map_err(|e| context(e, format!("创建录制目录 {} 失败", dir.display())))?;
    if let Err(e) = platform::restrict_dir(dir) {
        tracing::warn!(dir = %dir.display(), "无法限制录制目录的访问权限：{e}");
    }
    Ok(())
}

/// 新建 `<name>-<stamp>.cast`，已存在时加 `-2`、`-3`（同一秒开始的多个录制）。
fn create_file(dir: &Path, name: &str, stamp: &str) -> Result<(PathBuf, File)> {
    for n in 1..=1000 {
        let file_name = match n {
            1 => format!("{name}-{stamp}.{}", super::cast::EXTENSION),
            n => format!("{name}-{stamp}-{n}.{}", super::cast::EXTENSION),
        };
        let path = dir.join(file_name);
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
                return Err(context(e, format!("创建录制文件 {} 失败", path.display())));
            }
        }
    }
    Err(CoreError::Io(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "同一秒内开始的录制太多",
    )))
}

/// 后台线程：把收到的内容写成事件行。
struct Worker {
    out: BufWriter<File>,
    started: Instant,
    size: u64,
    limit: u64,
    error: Option<io::Error>,
    full: bool,
    /// 已经报告过停下（失败或已满）
    reported: bool,
    output: Utf8Stream,
    input: Utf8Stream,
    /// 最近记下的终端大小（相同的大小变化不重复记）
    grid: (u16, u16),
    /// 上一个事件的时间：事件来自不同的线程，到达的先后与时间可能略有出入，时间不递减
    clock: f64,
    /// 复用的行缓冲
    line: String,
    shared: Arc<Shared>,
    on_halt: Option<Box<dyn FnOnce(Halt) + Send>>,
}

impl Worker {
    fn run(mut self, rx: mpsc::Receiver<Message>) {
        // 先把头部与开始时的屏幕写进文件
        self.flush();
        loop {
            // 等下一条消息；已经到达的一并处理完再写进文件
            let mut stop = match rx.recv() {
                Ok(message) => self.handle(message),
                Err(_) => true,
            };
            while !stop && !self.halted() {
                match rx.try_recv() {
                    Ok(message) => stop = self.handle(message),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => stop = true,
                }
            }
            if stop {
                self.close();
            }
            self.flush();
            if let Some(halt) = self.take_halt() {
                self.shared.closed.store(true, Ordering::Relaxed);
                if let Halt::Failed(e) = &halt {
                    tracing::warn!("录制文件写入失败，停止录制：{e}");
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
                at,
                data,
                dropped_before,
            } => {
                self.shared.backlog.fetch_sub(data.len(), Ordering::Relaxed);
                if dropped_before > 0 {
                    self.event(at, EventData::Marker(dropped_note(dropped_before)));
                }
                let text = self.output.push(&data);
                if !text.is_empty() {
                    self.event(at, EventData::Output(text));
                }
            }
            Message::Input { at, data } => {
                let text = self.input.push(&data);
                if !text.is_empty() {
                    self.event(at, EventData::Input(text));
                }
            }
            Message::Resize { at, columns, rows } => {
                if (columns, rows) != self.grid && columns > 0 && rows > 0 {
                    self.grid = (columns, rows);
                    self.event(at, EventData::Resize { columns, rows });
                }
            }
            Message::Stop => return true,
        }
        false
    }

    /// 停止：写出被切开、还没有输出的半个字符与丢弃的说明。
    fn close(&mut self) {
        let at = Instant::now();
        let rest = self.output.finish();
        if !rest.is_empty() {
            self.event(at, EventData::Output(rest));
        }
        let rest = self.input.finish();
        if !rest.is_empty() {
            self.event(at, EventData::Input(rest));
        }
        let dropped = self.shared.dropped.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            self.event(at, EventData::Marker(dropped_note(dropped)));
        }
    }

    fn halted(&self) -> bool {
        self.error.is_some() || self.full
    }

    /// 写一个事件；超过大小上限时改写一个标记并停下。
    fn event(&mut self, at: Instant, data: EventData) {
        if self.halted() {
            return;
        }
        let time = at.saturating_duration_since(self.started).as_secs_f64();
        self.clock = self.clock.max(time);
        let mut line = std::mem::take(&mut self.line);
        line.clear();
        push_event(&mut line, self.clock, &data);
        if self.size + line.len() as u64 + 1 > self.limit {
            self.full = true;
            line.clear();
            push_event(
                &mut line,
                self.clock,
                &EventData::Marker(format!(
                    "录制文件已超过 {}MB，之后的内容不再记录",
                    self.limit >> 20
                )),
            );
        }
        self.write_line(&line);
        self.line = line;
    }

    fn write_line(&mut self, line: &str) {
        if self.error.is_some() {
            return;
        }
        match self
            .out
            .write_all(line.as_bytes())
            .and_then(|()| self.out.write_all(b"\n"))
        {
            Ok(()) => self.size += line.len() as u64 + 1,
            Err(e) => self.error = Some(e),
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
                format!("写入录制文件失败：{e}"),
            ))),
            None if self.full => Halt::Full,
            None => return None,
        };
        self.reported = true;
        self.full = true;
        Some(halt)
    }
}

fn dropped_note(bytes: usize) -> String {
    format!("写入跟不上，这里丢弃了约 {} KB 输出", bytes.div_ceil(1024))
}

#[cfg(test)]
mod tests {
    use super::super::cast::{self, Cast};
    use super::*;

    fn options(title: &str) -> RecordingOptions {
        RecordingOptions {
            title: title.into(),
            columns: 80,
            rows: 24,
            ..RecordingOptions::default()
        }
    }

    fn read(path: &Path) -> Cast {
        cast::read_file(path).unwrap()
    }

    fn outputs(cast: &Cast) -> String {
        cast.events
            .iter()
            .filter_map(|e| match &e.data {
                EventData::Output(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    // 开始写头部与开始时的屏幕；输出被切在多字节字符中间也完整；大小变化去重；停止后文件完整
    #[test]
    fn records_output_resize_and_screen() {
        let tmp = tempfile::tempdir().unwrap();
        let mut opts = options("web-1: prod");
        opts.screen = Some("\x1b[H\x1b[2J$ ".into());
        let recording = Recording::start(tmp.path(), opts, |_| panic!("不应停下")).unwrap();
        let name = recording
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            name.starts_with("web-1_ prod-") && name.ends_with(".cast"),
            "{name}"
        );
        let bytes = "你好\r\n".as_bytes();
        recording.output(&bytes[..2]);
        recording.output(&bytes[2..]);
        recording.resize(80, 24);
        recording.resize(100, 30);
        recording.resize(100, 30);
        recording.input(b"secret\r");
        recording.stop();
        assert!(recording.wait_stopped(Duration::from_secs(5)));
        let cast = read(recording.path());
        assert_eq!((cast.header.width, cast.header.height), (80, 24));
        assert_eq!(cast.header.title.as_deref(), Some("web-1: prod"));
        assert_eq!(cast.header.term.as_deref(), Some("xterm-256color"));
        assert_eq!(cast.skipped, 0);
        assert_eq!(outputs(&cast), "\x1b[H\x1b[2J$ 你好\r\n");
        assert_eq!(cast.events[0].time, 0.);
        let resizes: Vec<_> = cast
            .events
            .iter()
            .filter(|e| matches!(e.data, EventData::Resize { .. }))
            .collect();
        assert_eq!(resizes.len(), 1);
        // 默认不录输入
        assert!(
            !cast
                .events
                .iter()
                .any(|e| matches!(e.data, EventData::Input(_)))
        );
        // 停止后再写不生效
        recording.output(b"late");
        assert_eq!(outputs(&read(recording.path())), "\x1b[H\x1b[2J$ 你好\r\n");
    }

    // 开启录制输入时记 i 事件；末尾被切开的半个字符在停止时写成 U+FFFD
    #[test]
    fn input_is_recorded_only_when_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let mut opts = options("db");
        opts.input = true;
        let recording = Recording::start(tmp.path(), opts, |_| panic!("不应停下")).unwrap();
        assert!(recording.records_input());
        recording.input(b"ls\r");
        recording.output(&"中".as_bytes()[..1]);
        // 丢弃即停止（关闭 Tab 时）：等后台线程写完
        let finished = recording.finished.clone();
        let path = recording.path().to_path_buf();
        drop(recording);
        assert!(finished.wait_until(Instant::now() + Duration::from_secs(5)));
        let cast = read(&path);
        assert!(
            cast.events
                .iter()
                .any(|e| e.data == EventData::Input("ls\r".into()))
        );
        assert_eq!(outputs(&cast), "\u{fffd}");
    }

    // 达到大小上限：写一个标记后停下，报告一次；文件仍能完整读入
    #[test]
    fn stops_when_file_is_full() {
        let tmp = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let recording = Recording::create(tmp.path(), "full").unwrap().begin_with(
            options("full"),
            600,
            Box::new(move |halt| {
                let _ = tx.send(matches!(halt, Halt::Full));
            }),
        );
        for _ in 0..20 {
            recording.output(&[b'x'; 64]);
        }
        assert!(rx.recv_timeout(Duration::from_secs(5)).unwrap());
        assert!(recording.wait_stopped(Duration::from_secs(5)));
        assert!(recording.is_closed());
        let cast = read(recording.path());
        assert_eq!(cast.skipped, 0);
        assert!(matches!(
            cast.events.last().unwrap().data,
            EventData::Marker(_)
        ));
    }

    // 异常结束（没有停止就退出、最后一行写了一半）：之前的内容照样能读
    #[test]
    fn abnormal_end_keeps_earlier_events() {
        let tmp = tempfile::tempdir().unwrap();
        let recording = Recording::start(tmp.path(), options("crash"), |_| {}).unwrap();
        recording.output(b"before crash\r\n");
        recording.stop();
        assert!(recording.wait_stopped(Duration::from_secs(5)));
        let mut file = OpenOptions::new()
            .append(true)
            .open(recording.path())
            .unwrap();
        file.write_all(b"[9.5, \"o\", \"half a li").unwrap();
        drop(file);
        let cast = read(recording.path());
        assert_eq!(cast.skipped, 1);
        assert_eq!(outputs(&cast), "before crash\r\n");
    }

    // 建好文件后取消（没有开始）：删掉空文件
    #[test]
    fn cancelled_pending_file_is_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let pending = Recording::create(tmp.path(), "web").unwrap();
        let path = pending.path().to_path_buf();
        assert!(path.exists());
        drop(pending);
        assert!(!path.exists());
    }

    #[test]
    fn same_second_recordings_get_different_names() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, _) = create_file(tmp.path(), "web", "20261109-120000").unwrap();
        let (b, _) = create_file(tmp.path(), "web", "20261109-120000").unwrap();
        assert_eq!(a.file_name().unwrap(), "web-20261109-120000.cast");
        assert_eq!(b.file_name().unwrap(), "web-20261109-120000-2.cast");
    }

    #[test]
    fn start_fails_when_directory_cannot_be_created() {
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("file");
        std::fs::write(&blocker, b"x").unwrap();
        let err = Recording::start(&blocker.join("sub"), options("x"), |_| {}).unwrap_err();
        assert!(matches!(err, CoreError::Io(_)));
    }
}
