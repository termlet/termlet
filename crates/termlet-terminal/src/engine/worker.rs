//! 引擎线程：独占内核。内核在这个线程上创建，解析、读取、修改都在这里，随引擎丢弃而在这里释放；界面线程不碰内核，
//! 只经命令与发布出来的状态、快照使用它。所以内核不需要 `Send`、`Sync`（为 libghostty-vt 留的路）。
//!
//! - 每个引擎一个线程，从创建一直到引擎被丢弃（重连换传输时不换线程）。线程等三样东西：界面的命令（优先）、传输或
//!   回放送来的输出、超时（同步更新的 150 毫秒、按回车推断命令的 2 秒）。
//! - 输出：读到一段就解析，并把已经到达的一起解析（一批最多 1MB）。两段之间有界面的命令时先处理命令，界面等结果最多
//!   等一段；处理命令前先发布已解析的部分、交给输出观察者，录制开始时的屏幕快照仍在两段之间。
//! - 发布（[`Published`]）：每处理完一批输出或一组命令，把界面常读的小状态（模式、网格大小、光标等，
//!   [`EngineState`]）与处理过的命令数写进共享区。有新内容、界面已换走上一份快照时，发出重绘通知后接着把可见区
//!   填进快照放进共享区（界面醒来的同时在填），界面绘制时直接换走（三份缓冲轮换，不复制）。内容没变时界面手里的
//!   就是最新的，不用再取。
//! - 界面读状态前先等引擎线程处理完自己之前发出的命令（读到自己的修改，例如滚动之后读可见区的位置），通常不用等。
//! - 整段操作（查找、选中的文字、读命令、网址所在的逻辑行、屏幕快照等）作为闭包（[`Command::Run`]）发给引擎线程
//!   执行，界面有界等待结果。线程之间只传引擎自己的类型。

use std::future::Future as _;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::Poll;
use std::time::Instant;

use parking_lot::{Condvar, Mutex};
use termlet_io::{CloseReason, GridSize, IoEvent, TerminalInput};

use super::kernel::{Kernel, Notices};
use super::replay::Feed;
use super::{
    EngineEvent, MAX_BATCH_BYTES, OutputObserver, PENDING_COMMAND_TIMEOUT, PROMPT_IDLE, SharedInput,
};
use crate::colors::Palette;
use crate::dump::{ScreenDump, ShellMarks, screen_dump};
use crate::grid::{
    CursorShape, GridDimensions, GridMark, GridPoint, GridSnapshot, Scroll, SelectionKind, Side,
    SnapshotParams, SnapshotStamp, TerminalModes,
};
use crate::shell_events::{
    CommandText, FoundMark, OscScanner, ShellEvent, ShellMark, is_recordable,
};
use crate::suggest::PromptInput;
use crate::text::{is_continuation, text_between, text_from};

// ------------------------------------------------------------ 共享区

/// 界面常读的状态：引擎线程每处理完一批输出或一组命令时发布一份（读它不用等引擎线程）。
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct EngineState {
    pub(super) modes: TerminalModes,
    pub(super) dimensions: GridDimensions,
    /// 光标所在的格子（网格中的位置，不按宽字符调整）
    pub(super) cursor: GridPoint,
    pub(super) cursor_blinking: bool,
    pub(super) theme_reports: bool,
    pub(super) scrollback: usize,
    /// 没有选区，或选区是空的
    pub(super) selection_empty: bool,
    /// 当前传输上见过 OSC 133 / 633 的提示符或命令标记
    pub(super) integration: bool,
}

/// 引擎线程发布给界面的内容。
#[derive(Default)]
pub(super) struct Published {
    /// 引擎线程处理过的命令数
    pub(super) applied: u64,
    /// 内容的版本（见 [`SnapshotStamp::version`]）
    pub(super) version: u64,
    pub(super) state: EngineState,
    /// 最近填好、界面还没换走的快照（`slot_full` 时有效）
    pub(super) slot: GridSnapshot,
    pub(super) slot_full: bool,
    /// 界面要一份新的快照（手里的与共享区的都不是最新的）
    pub(super) want_fill: bool,
    /// 引擎线程正在填当前版本的快照：界面等它，不另外请求
    pub(super) filling: bool,
    /// 引擎线程已退出：不用再等它
    pub(super) stopped: bool,
}

/// 引擎线程与界面的共享区：发布的内容，以及发布时通知等待者的条件变量。
#[derive(Default)]
pub(super) struct Shared {
    pub(super) published: Mutex<Published>,
    pub(super) updated: Condvar,
}

/// 引擎线程退出（含 panic）时标记出来，界面不再等它。
struct StopGuard(Arc<Shared>);

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.0.published.lock().stopped = true;
        self.0.updated.notify_all();
    }
}

// ------------------------------------------------------------ 命令

/// 界面发给引擎线程的命令，按发出的顺序执行（界面读状态、取快照前会等它们执行完）。
pub(super) enum Command<K> {
    /// 接上新的传输（重连）
    Attach {
        output: async_channel::Receiver<IoEvent>,
        input: Arc<dyn TerminalInput>,
        generation: u64,
    },
    SetPalette(Palette),
    Resize(GridSize),
    /// 快照的参数变了
    SetParams(SnapshotParams),
    /// 界面要一份新的快照（共享区中的 `want_fill` 已设好，这条只为叫醒引擎线程）
    Fill,
    /// 显示一段本地生成的文字
    Inject(String),
    /// 另起一行显示一条暗色的提示
    InjectNotice(String),
    Scroll(Scroll),
    ScrollTo(GridPoint),
    /// 回到最底部并取消选区（用户输入时）
    ScrollToBottomAndDeselect,
    ClearBuffer,
    StartSelection(SelectionKind, GridPoint, Side),
    UpdateSelection(GridPoint, Side),
    SelectAll,
    ClearSelection,
    /// 用户的输入写给远端之前：回到最底部、取消选区，记下输入（一条命令，按键只叫醒引擎线程一次）
    UserInput(Vec<u8>),
    /// 在引擎线程上执行一段操作（整段的读取，或要返回结果的修改），结果由闭包自己送回
    Run(Box<dyn FnOnce(&mut Worker<K>) + Send>),
}

// ------------------------------------------------------------ shell 集成的状态

/// 没有 shell 集成时，按回车推断的命令从哪里开始。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum InputStart {
    #[default]
    None,
    /// 这一行已经开始输入，但无法确定从哪里开始（不记录）
    Unknown,
    At(GridMark),
}

/// shell 集成与命令历史的状态（只在引擎线程上）。
#[derive(Default)]
pub(super) struct ShellState {
    /// 用户在这个终端里输入过：之前到达的命令不记进历史（例如登录后由应用自动执行的命令）。之后不再复位
    armed: bool,
    /// 当前传输上见过 OSC 133 / 633 的提示符或命令标记
    integration: bool,
    /// 见过 B 或 633;E：命令按标记记录，不再按回车推断
    marked_input: bool,
    /// 当前提示符的输入开始处（B）
    input: Option<GridMark>,
    /// 用户在当前提示符后输入过、还没有按回车（命令补全提示只在这时出现）；提示符开始（A）、按回车或 Ctrl+C 时复位
    typed: bool,
    /// 按回车推断：这一行的输入开始处
    start: InputStart,
    /// 按回车推断：已经按了回车、等回显的那一行
    pending: Option<GridMark>,
    /// 最近一次有输出的时间
    last_output: Option<Instant>,
}

impl ShellState {
    /// 换了新的传输（重连到新的 shell）：shell 集成重新判断，未完成的命令作废。
    fn reset_session(&mut self) {
        self.integration = false;
        self.marked_input = false;
        self.input = None;
        self.typed = false;
        self.start = InputStart::None;
        self.pending = None;
    }

    /// 命令补全提示用：shell 提示符后正在输入的命令（条件见 [`crate::suggest`]）。在备用屏幕中时标记读不出来
    /// （[`Kernel::resolve`] 为 `None`），全屏程序中不会有。
    pub(super) fn prompt_input(&self, kernel: &impl Kernel) -> Option<PromptInput> {
        if !self.integration || !self.marked_input || !self.typed {
            return None;
        }
        let from = kernel.resolve(self.input.as_ref()?)?;
        let cursor = kernel.cursor_point();
        // 光标在输入开始处所在的逻辑行上（之间都是自动换行），例如不在多行命令的续行提示符后
        if cursor < from || (from.line..cursor.line).any(|line| !kernel.wraps(line)) {
            return None;
        }
        // 光标之后还有文字：光标不在行尾，或远端的 shell 自己画了提示
        if !text_from(kernel, cursor, None).is_empty() {
            return None;
        }
        let text = text_between(kernel, from, cursor);
        (!text.trim().is_empty()).then_some(PromptInput { text, cursor })
    }

    /// 从标记处读到逻辑行的末尾；`stop_at_cursor` 时最多读到光标处。
    fn read(&self, kernel: &impl Kernel, mark: &GridMark, stop_at_cursor: bool) -> Option<String> {
        let from = kernel.resolve(mark)?;
        let stop = stop_at_cursor.then(|| kernel.cursor_point());
        Some(text_from(kernel, from, stop))
    }

    /// 读出按回车推断的那一行，符合命令历史的规则时返回。
    fn take_pending(&mut self, kernel: &impl Kernel) -> Option<CommandText> {
        let mark = self.pending.take()?;
        let text = self.read(kernel, &mark, false)?;
        is_recordable(&text).then_some(CommandText(text))
    }

    /// 光标已经离开按了回车的那一行（到了新的一行，不是自动换行的续行）：回显都已到达。
    fn pending_done(&self, kernel: &impl Kernel) -> bool {
        let Some(mark) = &self.pending else {
            return false;
        };
        let Some(point) = kernel.resolve(mark) else {
            return true;
        };
        let cursor = kernel.cursor_point().line;
        cursor > point.line && !is_continuation(kernel, cursor)
    }

    /// 屏幕上当前的输入行：有 shell 集成时从 B 处读起，否则从推断出的输入开始处读起，读到行尾；无法确定时为 `None`。
    pub(super) fn input_line(&self, kernel: &impl Kernel) -> Option<String> {
        let mark = if self.marked_input {
            self.input?
        } else {
            match self.start {
                InputStart::At(mark) => mark,
                InputStart::None | InputStart::Unknown => return None,
            }
        };
        self.read(kernel, &mark, false)
    }

    /// 按了回车、在等回显的那一行（测试用）。
    #[cfg(test)]
    pub(super) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }
}

/// 有 shell 集成时命令的进行情况（每个传输一份）。
#[derive(Default)]
struct CommandMarks {
    /// shell 输出过 C：没收到 C 就出现了新的提示符，说明那一行被 Ctrl+C 等取消了，不记录
    saw_executed: bool,
    /// 633;E 给出的命令行
    explicit: Option<CommandText>,
    /// 命令开始执行（C）的时间：D 到达时算出耗时
    started: Option<Instant>,
}

// ------------------------------------------------------------ 输出从哪里来

/// 输出的来源。
pub(super) enum Source {
    /// 传输已结束（等重连），或回放已停止
    None,
    /// 第 `generation` 个传输
    Transport {
        output: async_channel::Receiver<IoEvent>,
        generation: u64,
    },
    /// 回放：调用方按录制的顺序送来的输出与大小变化；`cancelled` 后不再处理剩下的
    Replay {
        feed: async_channel::Receiver<Feed>,
        cancelled: Arc<AtomicBool>,
    },
}

/// 来源送来的一项。
enum Incoming {
    Output(Vec<u8>),
    /// 会话结束（传输报告的）
    Closed(CloseReason),
    /// 回放中终端大小变了
    Resize(GridSize),
    /// 回放：之前送来的都处理完时通知
    Idle(async_channel::Sender<()>),
    /// 回放：之前送来的都处理完时写一份关键帧
    Checkpoint(async_channel::Sender<Option<ScreenDump>>),
    /// 通道关闭了
    Gone,
}

impl From<IoEvent> for Incoming {
    fn from(event: IoEvent) -> Self {
        match event {
            IoEvent::Output(bytes) => Incoming::Output(bytes),
            IoEvent::Closed(reason) => Incoming::Closed(reason),
        }
    }
}

impl From<Feed> for Incoming {
    fn from(feed: Feed) -> Self {
        match feed {
            Feed::Output(bytes) => Incoming::Output(bytes),
            Feed::Resize(size) => Incoming::Resize(size),
            Feed::Idle(done) => Incoming::Idle(done),
            Feed::Checkpoint(done) => Incoming::Checkpoint(done),
        }
    }
}

impl Source {
    /// 已经到达的下一项；没有时为 `None`。
    fn try_recv(&self) -> Option<Incoming> {
        use async_channel::TryRecvError;
        fn map<T: Into<Incoming>>(next: Result<T, TryRecvError>) -> Option<Incoming> {
            match next {
                Ok(item) => Some(item.into()),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Closed) => Some(Incoming::Gone),
            }
        }
        match self {
            Source::None => None,
            Source::Transport { output, .. } => map(output.try_recv()),
            Source::Replay { feed, .. } => map(feed.try_recv()),
        }
    }

    /// 等下一项；没有来源时一直等（由命令或超时叫醒）。
    async fn recv(&self) -> Incoming {
        match self {
            Source::None => std::future::pending().await,
            Source::Transport { output, .. } => {
                output.recv().await.map_or(Incoming::Gone, Into::into)
            }
            Source::Replay { feed, .. } => feed.recv().await.map_or(Incoming::Gone, Into::into),
        }
    }

    fn is_replay(&self) -> bool {
        matches!(self, Source::Replay { .. })
    }
}

enum Next<K> {
    Command(Command<K>),
    Incoming(Incoming),
    Timeout,
    /// 引擎已丢弃
    Exit,
}

// ------------------------------------------------------------ 引擎线程

/// 引擎线程需要的、与界面共用的东西（都可以跨线程）。
pub(super) struct Links<K> {
    pub(super) id: u64,
    pub(super) commands: async_channel::Receiver<Command<K>>,
    pub(super) source: Source,
    pub(super) input: SharedInput,
    pub(super) events: async_channel::Sender<EngineEvent>,
    pub(super) dirty: Arc<AtomicBool>,
    pub(super) size: Arc<Mutex<GridSize>>,
    /// 当前传输的序号：界面重连时先改它，引擎线程据此不再解析旧传输的输出
    pub(super) generation: Arc<AtomicU64>,
    pub(super) observer: Arc<Mutex<Option<OutputObserver>>>,
    pub(super) notices: Arc<Notices>,
    pub(super) shared: Arc<Shared>,
}

/// 启动引擎线程：内核由 `make` 在这个线程上创建，之后只在这里使用（`K` 不需要 `Send`）。
pub(super) fn spawn<K, F>(name: &str, make: F, links: Links<K>)
where
    K: Kernel + 'static,
    F: FnOnce() -> K + Send + 'static,
{
    let shared = links.shared.clone();
    let spawned = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let _stop = StopGuard(links.shared.clone());
            Worker::new(make(), links).run();
        });
    if let Err(e) = spawned {
        tracing::error!("无法启动终端引擎线程：{e}");
        shared.published.lock().stopped = true;
    }
}

/// 引擎线程的全部状态：内核、shell 集成、当前传输的解析状态与发布用的缓冲。
pub(super) struct Worker<K> {
    pub(super) kernel: K,
    pub(super) shell: ShellState,
    links: Links<K>,
    /// 合并输出时取到的下一项非输出的内容（回放中的大小变化等），这一批解析完再处理
    held: Option<Incoming>,
    scanner: OscScanner,
    /// 这一段输出中找到的标记（复用的缓冲）
    found: Vec<FoundMark>,
    marks: CommandMarks,
    /// 当前同步更新中为读取网格提前解析了的字节数：不为 0 时同步更新结束前不重绘
    sync_carry: usize,
    /// 按回车推断：等回显的那一行到这个时间还没读出就读一次
    pending_deadline: Option<Instant>,
    /// 接上过几次新的传输（一批输出中途换了传输时这一批到此为止）
    attached: u64,
    /// 处理过的命令数
    applied: u64,
    /// 内容的版本；`changed` 时发布前加一
    version: u64,
    changed: bool,
    /// 处理完这一组命令后叫界面重绘（本地插入的文字等界面不知道的变化）
    wake_after: bool,
    params: SnapshotParams,
    /// 填快照用的缓冲（与共享区、界面手里的轮换）
    spare: GridSnapshot,
    /// 最近一次填的快照的来历
    last_fill: Option<SnapshotStamp>,
}

impl<K: Kernel> Worker<K> {
    fn new(kernel: K, links: Links<K>) -> Self {
        Self {
            kernel,
            shell: ShellState::default(),
            links,
            held: None,
            scanner: OscScanner::new(),
            found: Vec::new(),
            marks: CommandMarks::default(),
            sync_carry: 0,
            pending_deadline: None,
            attached: 0,
            applied: 0,
            version: 0,
            changed: true,
            wake_after: false,
            params: SnapshotParams::default(),
            spare: GridSnapshot::default(),
            last_fill: None,
        }
    }

    fn run(mut self) {
        // 先发布一次：界面从一开始就能读到网格的大小、模式
        self.publish(false, false);
        loop {
            match self.next() {
                Next::Exit => return,
                Next::Command(command) => {
                    self.handle(command);
                    self.serve_commands();
                }
                Next::Incoming(incoming) => self.on_incoming(incoming),
                Next::Timeout => self.on_timeout(),
            }
            while let Some(held) = self.held.take() {
                self.on_incoming(held);
            }
        }
    }

    /// 等下一件事：界面的命令优先，其次是输出，最后是超时。
    fn next(&self) -> Next<K> {
        let deadline = match (self.kernel.sync_deadline(), self.pending_deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let commands = self.links.commands.recv();
        let incoming = self.links.source.recv();
        let mut commands = std::pin::pin!(commands);
        let mut incoming = std::pin::pin!(incoming);
        let mut timer = deadline.map(async_io::Timer::at);
        futures::executor::block_on(std::future::poll_fn(|cx| {
            if let Poll::Ready(command) = commands.as_mut().poll(cx) {
                return Poll::Ready(match command {
                    Ok(command) => Next::Command(command),
                    Err(_) => Next::Exit,
                });
            }
            if let Poll::Ready(incoming) = incoming.as_mut().poll(cx) {
                return Poll::Ready(Next::Incoming(incoming));
            }
            if let Some(timer) = timer.as_mut()
                && Pin::new(timer).poll(cx).is_ready()
            {
                return Poll::Ready(Next::Timeout);
            }
            Poll::Pending
        }))
    }

    /// 处理排着的命令，然后发布。
    fn serve_commands(&mut self) {
        while let Ok(command) = self.links.commands.try_recv() {
            self.handle(command);
        }
        // 界面自己的修改（滚动、选区等）之后它会重绘：先填好快照，但不另发重绘通知；本地插入的文字界面不知道，要通知
        let redraw = self.changed;
        let wake = std::mem::take(&mut self.wake_after);
        self.publish(redraw, wake);
    }

    fn handle(&mut self, command: Command<K>) {
        self.applied += 1;
        match command {
            Command::Attach {
                output,
                input,
                generation,
            } => self.attach(output, input, generation),
            Command::SetPalette(palette) => {
                if let Some(notice) = self.kernel.set_palette(&palette) {
                    tracing::debug!(?notice, "终端配色的深浅变了，通知远端");
                    self.links.input.lock().write(notice.as_bytes());
                }
            }
            Command::Resize(size) => {
                self.kernel.resize(size);
                self.changed = true;
            }
            Command::SetParams(params) => {
                if self.params != params {
                    self.params = params;
                    self.changed = true;
                }
            }
            Command::Fill => {}
            Command::Inject(text) => self.inject(text.as_bytes()),
            Command::InjectNotice(text) => {
                let at_line_start = self.kernel.cursor_point().column == 0;
                let newline = if at_line_start { "" } else { "\r\n" };
                self.inject(format!("{newline}\x1b[2m{text}\x1b[0m\r\n").as_bytes());
            }
            Command::Scroll(scroll) => {
                self.kernel.scroll_display(scroll);
                self.changed = true;
            }
            Command::ScrollTo(point) => {
                self.kernel.scroll_to(point);
                self.changed = true;
            }
            Command::ScrollToBottomAndDeselect => self.scroll_to_bottom_and_deselect(),
            Command::ClearBuffer => {
                self.kernel.clear_buffer();
                self.changed = true;
            }
            Command::StartSelection(kind, point, side) => {
                self.kernel.start_selection(kind, point, side);
                self.changed = true;
            }
            Command::UpdateSelection(point, side) => {
                self.update_selection(point, side);
            }
            Command::SelectAll => {
                self.kernel.select_all();
                self.changed = true;
            }
            Command::ClearSelection => {
                self.kernel.clear_selection();
                self.changed = true;
            }
            Command::UserInput(data) => {
                self.scroll_to_bottom_and_deselect();
                self.note_input(&data);
            }
            Command::Run(run) => run(self),
        }
    }

    fn on_incoming(&mut self, incoming: Incoming) {
        if let Source::Replay { cancelled, .. } = &self.links.source
            && cancelled.load(Ordering::Acquire)
        {
            // 丢弃了回放的输入端：剩下的不再处理（等它的 idle 随之结束）
            self.links.source = Source::None;
            return;
        }
        if !self.is_current() {
            self.drop_superseded();
            return;
        }
        match incoming {
            Incoming::Output(bytes) => self.on_output(bytes),
            Incoming::Closed(reason) => self.finish(reason),
            Incoming::Gone => {
                if self.links.source.is_replay() {
                    self.links.source = Source::None;
                } else {
                    self.finish(CloseReason::Failed("传输已关闭".into()));
                }
            }
            Incoming::Resize(size) => {
                *self.links.size.lock() = size;
                self.kernel.resize(size);
                self.changed = true;
                self.publish(true, true);
            }
            Incoming::Idle(done) => {
                self.publish(false, false);
                let _ = done.try_send(());
            }
            Incoming::Checkpoint(done) => {
                let _ = done.try_send(self.checkpoint());
            }
        }
    }

    /// 一批输出：先解析这一段，再把已经到达的一起解析（最多 [`MAX_BATCH_BYTES`]），然后发布、交给观察者、叫界面重绘。
    fn on_output(&mut self, first: Vec<u8>) {
        // 有观察者时留下解析过的各段，发布之后再交给它（只是移动，不复制）
        let observer = self.links.observer.lock().clone();
        let mut observed = Vec::new();
        let mut processed = 0;
        let mut closed = None;
        let attached = self.attached;
        let mut next = Some(first);
        while let Some(bytes) = next.take() {
            // 界面换上了新的传输：旧传输的输出不再解析
            if !self.is_current() {
                self.drop_superseded();
                break;
            }
            self.feed(&bytes);
            processed += bytes.len();
            if observer.is_some() {
                observed.push(bytes);
            }
            if processed >= MAX_BATCH_BYTES {
                break;
            }
            // 两段之间先处理界面的命令（界面在等快照、查找结果等）。先发布已解析的部分、交给观察者：屏幕快照（录制
            // 开始时）在两段之间取，此前的输出都已交给观察者
            if !self.links.commands.is_empty() {
                self.publish(false, false);
                observe(observer.as_ref(), &mut observed);
                self.serve_commands();
                if self.attached != attached {
                    break;
                }
            }
            match self.links.source.try_recv() {
                Some(Incoming::Output(more)) => next = Some(more),
                Some(Incoming::Closed(reason)) => closed = Some(reason),
                Some(other) => self.held = Some(other),
                None => {}
            }
        }
        self.shell.last_output = Some(Instant::now());
        let buffered = self.kernel.sync_buffered();
        // 全部输出都在同步更新中时不重绘，等同步结束
        let redraw = self.sync_carry == 0 && buffered < processed;
        self.publish(redraw, redraw);
        observe(observer.as_ref(), &mut observed);
        if let Some(reason) = closed {
            self.finish(reason);
        }
    }

    fn on_timeout(&mut self) {
        let now = Instant::now();
        if self.kernel.sync_deadline().is_some_and(|at| at <= now) {
            self.kernel.stop_sync();
            self.flush_replies();
            self.sync_carry = 0;
            self.changed = true;
            self.publish(true, true);
        }
        if self.pending_deadline.is_some_and(|at| at <= now) {
            self.pending_deadline = None;
            self.flush_pending_command();
            self.publish(false, false);
        }
    }

    /// 传输结束了。
    fn finish(&mut self, reason: CloseReason) {
        let Source::Transport { generation, .. } = self.links.source else {
            return;
        };
        // 界面已经换上了新的传输：旧传输的结束不报告
        if !self.is_current() {
            self.drop_superseded();
            return;
        }
        self.links.source = Source::None;
        if self.kernel.sync_buffered() > 0 {
            self.kernel.stop_sync();
            self.flush_replies();
            self.changed = true;
        }
        self.publish(true, true);
        tracing::debug!(transport = generation, ?reason, "终端的传输结束");
        let _ = self.links.events.try_send(EngineEvent::Closed {
            reason,
            transport: generation,
        });
    }

    /// 界面已经换上了新的传输：不再读旧的（它的输出不再解析、结束不再报告），等 [`Command::Attach`]。
    fn drop_superseded(&mut self) {
        if let Source::Transport { generation, .. } = self.links.source {
            tracing::debug!(transport = generation, "不再读旧的传输（已换上新的传输）");
        }
        self.links.source = Source::None;
    }

    /// 来源仍然有效：不是已被界面换下的传输。
    fn is_current(&self) -> bool {
        match self.links.source {
            Source::Transport { generation, .. } => {
                self.links.generation.load(Ordering::Acquire) == generation
            }
            Source::None | Source::Replay { .. } => true,
        }
    }

    fn attach(
        &mut self,
        output: async_channel::Receiver<IoEvent>,
        input: Arc<dyn TerminalInput>,
        generation: u64,
    ) {
        self.shell.reset_session();
        // 新的 shell 还没有开启键盘协议与深浅色切换通知
        self.kernel.new_session();
        let previous = std::mem::replace(&mut *self.links.input.lock(), input.clone());
        previous.close();
        input.resize(*self.links.size.lock());
        self.links.source = Source::Transport { output, generation };
        self.scanner = OscScanner::new();
        self.marks = CommandMarks::default();
        self.sync_carry = 0;
        self.pending_deadline = None;
        self.attached += 1;
        self.changed = true;
    }

    // ------------------------------------------------------------ 发布

    /// 界面常读的状态。
    fn state(&self) -> EngineState {
        let kernel = &self.kernel;
        EngineState {
            modes: kernel.modes(),
            dimensions: kernel.dimensions(),
            cursor: kernel.cursor_point(),
            cursor_blinking: kernel.cursor_blinking(),
            theme_reports: kernel.theme_reports_enabled(),
            scrollback: kernel.scrollback(),
            selection_empty: kernel.selection_is_empty(),
            integration: self.shell.integration,
        }
    }

    /// 发布状态与处理过的命令数，`wake` 时通知界面重绘。要填快照时（内容变了、`redraw` 且界面已换走上一份，或者
    /// 界面要一份新的）先发布状态、标出正在填、通知界面，再填好放进共享区：界面醒来的时间与填快照重叠，醒来时多半
    /// 已经填好，没填好就等这一份，不再另外请求。
    fn publish(&mut self, redraw: bool, wake: bool) {
        if std::mem::take(&mut self.changed) {
            self.version += 1;
        }
        let stamp = SnapshotStamp {
            engine: self.links.id,
            version: self.version,
            params: self.params,
        };
        let state = self.state();
        let fill = {
            let mut published = self.links.shared.published.lock();
            let in_slot = published.slot_full && published.slot.stamp == Some(stamp);
            let fill = !in_slot
                && (published.want_fill
                    || (redraw && !published.slot_full && self.last_fill != Some(stamp)));
            published.applied = self.applied;
            published.version = self.version;
            published.state = state;
            published.filling = fill;
            fill
        };
        self.links.shared.updated.notify_all();
        // 光标的形状或闪烁变了：界面收到时读到的已是新的
        if self.links.notices.cursor.swap(false, Ordering::AcqRel) {
            let _ = self
                .links
                .events
                .try_send(EngineEvent::CursorBlinkingChange);
        }
        if wake {
            self.wake();
        }
        if fill {
            self.kernel.snapshot(&mut self.spare, self.params.context);
            self.spare.prompt_input = if self.params.prompt_input {
                self.shell.prompt_input(&self.kernel)
            } else {
                None
            };
            self.spare.stamp = Some(stamp);
            self.last_fill = Some(stamp);
            {
                let mut published = self.links.shared.published.lock();
                std::mem::swap(&mut self.spare, &mut published.slot);
                published.slot_full = true;
                published.want_fill = false;
                published.filling = false;
            }
            self.links.shared.updated.notify_all();
        }
    }

    /// 通知界面重绘。合并发送：界面还没处理上一个通知时不再发新的。
    fn wake(&self) {
        if !self.links.dirty.swap(true, Ordering::AcqRel) {
            let _ = self.links.events.try_send(EngineEvent::Wakeup);
        }
    }

    // ------------------------------------------------------------ 解析

    /// 解析一段输出：按其中的 shell 集成标记切开，逐段解析并在每个标记处处理。
    fn feed(&mut self, bytes: &[u8]) {
        self.changed = true;
        let mut found = std::mem::take(&mut self.found);
        found.clear();
        self.scanner.scan(bytes, &mut found);
        let mut at = 0;
        for mark in found.drain(..) {
            self.advance(&bytes[at..mark.offset]);
            at = mark.offset;
            self.on_mark(mark.mark);
        }
        self.found = found;
        self.advance(&bytes[at..]);
    }

    /// 解析一段（不含标记），写出这一段产生的应答。
    fn advance(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.kernel.advance(bytes);
        self.changed = true;
        self.flush_replies();
        if self.kernel.sync_deadline().is_none() {
            self.sync_carry = 0;
        }
        // 按回车推断：回显都已到达时读出命令
        if self.shell.pending_done(&self.kernel)
            && let Some(command) = self.shell.take_pending(&self.kernel)
        {
            self.send(ShellEvent::CommandEntered(command));
        }
    }

    /// 按顺序写出内核排队的应答（回放的输入端丢弃它们）。
    fn flush_replies(&mut self) {
        if let Some(out) = self.kernel.take_replies() {
            self.links.input.lock().write(out.as_bytes());
        }
    }

    /// 同步更新（DEC 2026）中内核先缓冲输出，网格与光标还没有更新：读取网格之前先解析缓冲的部分，
    /// 再重新开始同步更新，画面仍然等结束标记（或超时）一起显示。
    fn sync_now(&mut self) {
        let buffered = self.kernel.sync_now();
        if buffered > 0 {
            self.changed = true;
            self.flush_replies();
            self.sync_carry += buffered;
        }
    }

    /// 发出 shell 集成的事件。先发布状态：界面处理事件时读到的（是否开启了 shell 集成等）已包含它。
    fn send(&mut self, event: ShellEvent) {
        self.publish(false, false);
        let _ = self.links.events.try_send(EngineEvent::Shell(event));
    }

    /// 从输入开始处（B）读出命令；`stop_at_cursor` 时最多读到光标处。
    fn read_input(&mut self, stop_at_cursor: bool) -> Option<CommandText> {
        let input = self.shell.input?;
        self.sync_now();
        let text = self.shell.read(&self.kernel, &input, stop_at_cursor)?;
        (!text.is_empty()).then_some(CommandText(text))
    }

    /// 要记进历史的命令：用户输入过、符合规则。
    fn enter(&mut self, command: Option<&CommandText>) {
        if let Some(command) = command
            && self.shell.armed
            && is_recordable(command.as_str())
        {
            self.send(ShellEvent::CommandEntered(command.clone()));
        }
    }

    fn on_mark(&mut self, mark: ShellMark) {
        if mark.is_prompt_mark() {
            self.shell.integration = true;
        }
        match mark {
            ShellMark::PromptStart => {
                let pending = self.shell.input.is_some();
                // shell 从不输出 C（只输出 A、B）：命令在下一个提示符开始时补记
                if pending && !self.marks.saw_executed {
                    let command = match self.marks.explicit.take() {
                        Some(text) => Some(text),
                        None => self.read_input(false),
                    };
                    self.enter(command.as_ref());
                }
                self.shell.input = None;
                self.shell.typed = false;
                self.marks.started = None;
                self.send(ShellEvent::PromptStart);
            }
            ShellMark::InputStart => {
                self.sync_now();
                let shell = &mut self.shell;
                shell.marked_input = true;
                shell.start = InputStart::None;
                shell.pending = None;
                // 不复位 typed：zsh 等重画提示符（reset-prompt）时只再输出 B，输入中的命令还在
                shell.input = self.kernel.mark_cursor();
                self.marks.explicit = None;
            }
            ShellMark::CommandExecuted => {
                self.marks.saw_executed = true;
                let has_input = self.shell.input.is_some();
                let command = match self.marks.explicit.take() {
                    // 633;E 给出了空的命令行：用户没有输入，不再从网格中读
                    Some(text) => (!text.0.is_empty()).then_some(text),
                    None if has_input => self.read_input(true),
                    None => None,
                };
                self.shell.input = None;
                self.enter(command.as_ref());
                self.marks.started = Some(Instant::now());
                self.send(ShellEvent::CommandStart { command });
            }
            ShellMark::CommandFinished(exit_code) => {
                if let Some(started) = self.marks.started.take() {
                    self.send(ShellEvent::CommandFinished {
                        exit_code,
                        duration: started.elapsed(),
                    });
                }
            }
            ShellMark::CommandLine(text) => {
                self.shell.marked_input = true;
                self.marks.explicit = Some(text);
            }
            ShellMark::WorkingDirectory(path) => self.send(ShellEvent::WorkingDirectory(path)),
            ShellMark::Notification { title, body } => {
                self.send(ShellEvent::Notification { title, body })
            }
            ShellMark::EnterAlternateScreen => {
                // 切换之前读出按回车推断的命令（例如 vim 的回显与切换屏幕在同一段输出中）
                self.shell.start = InputStart::None;
                if let Some(command) = self.shell.take_pending(&self.kernel) {
                    self.send(ShellEvent::CommandEntered(command));
                }
            }
            ShellMark::KeyboardMode(request) => {
                // 同步更新中先解析缓冲的部分，所在的屏幕才是最新的（同一段里可能先切换到备用屏幕再压栈）
                self.sync_now();
                self.kernel.keyboard_request(request);
                self.flush_replies();
            }
            ShellMark::ThemeMode(request) => {
                // 回答排在之前的提问（同步更新中缓冲着的）之后
                self.sync_now();
                self.kernel.theme_request(request);
                self.flush_replies();
            }
        }
    }

    // ------------------------------------------------------------ 界面的操作

    /// 显示一段本地生成的文字（不是远端的输出，其中的提问不回答），并叫界面重绘。
    fn inject(&mut self, bytes: &[u8]) {
        self.kernel.inject(bytes);
        self.changed = true;
        self.wake_after = true;
    }

    /// 回到最底部并取消选区。每次按键都会做：已在底部、没有选区时可见区不变，不必重新填快照。
    fn scroll_to_bottom_and_deselect(&mut self) {
        let scrolled = self.kernel.dimensions().display_offset > 0;
        let selected = !self.kernel.selection_is_empty();
        if scrolled {
            self.kernel.scroll_display(Scroll::Bottom);
        }
        self.kernel.clear_selection();
        self.changed |= scrolled || selected;
    }

    /// 把选区的另一端移到这里；没有选区时返回 `false`。
    pub(super) fn update_selection(&mut self, point: GridPoint, side: Side) -> bool {
        let updated = self.kernel.update_selection(point, side);
        self.changed |= updated;
        updated
    }

    /// 改变回滚的行数，返回是否有变化。
    pub(super) fn set_scrollback(&mut self, lines: usize) -> bool {
        let changed = self.kernel.set_scrollback(lines);
        self.changed |= changed;
        changed
    }

    /// 改变光标的默认样式，返回是否有变化。
    pub(super) fn set_default_cursor(&mut self, shape: CursorShape, blinking: bool) -> bool {
        let changed = self.kernel.set_default_cursor(shape, blinking);
        self.changed |= changed;
        changed
    }

    /// 用户的输入（规则见 `TerminalEngine::user_input`）。按了回车、有一行在等回显时，到
    /// [`PENDING_COMMAND_TIMEOUT`] 还没读出就读一次。
    fn note_input(&mut self, data: &[u8]) {
        let shell = &mut self.shell;
        shell.armed = true;
        // 命令补全提示：按了回车（提交、粘贴的多行）、Ctrl+C、Ctrl+D 之后不再提示，等下一个提示符后再输入
        let typed = !data
            .iter()
            .any(|b| matches!(b, b'\r' | b'\n' | 0x03 | 0x04));
        // 快照中带着正在输入的命令时，它随之变化
        self.changed |= self.params.prompt_input && shell.typed != typed;
        shell.typed = typed;
        if shell.marked_input {
            return;
        }
        if self.kernel.modes().alt_screen {
            shell.start = InputStart::None;
            return;
        }
        if data == b"\r" {
            let start = std::mem::take(&mut shell.start);
            if let InputStart::At(mark) = start {
                shell.pending = Some(mark);
                self.pending_deadline = Some(Instant::now() + PENDING_COMMAND_TIMEOUT);
            }
            return;
        }
        if data == b"\x03" || data == b"\x04" || data.contains(&b'\r') {
            shell.start = InputStart::None;
            return;
        }
        if shell.start != InputStart::None {
            return;
        }
        let busy = shell.pending.is_some()
            || shell
                .last_output
                .is_some_and(|at| at.elapsed() < PROMPT_IDLE);
        shell.start = match self.kernel.mark_cursor() {
            Some(mark) if !busy => InputStart::At(mark),
            _ => InputStart::Unknown,
        };
    }

    /// 按了回车后远端迟迟没有换行：现在读出这一行。
    fn flush_pending_command(&mut self) {
        if let Some(command) = self.shell.take_pending(&self.kernel) {
            self.send(ShellEvent::CommandEntered(command));
        }
    }

    /// 屏幕现在的内容写成控制序列（见 `TerminalEngine::screen_dump_then`）：同步更新中缓冲着的输出先解析进来，
    /// 用户往回翻着时取的仍是屏幕（底部），显示位置不变。
    pub(super) fn screen_dump(&mut self) -> ScreenDump {
        self.sync_now();
        let offset = self.kernel.dimensions().display_offset;
        if offset > 0 {
            self.kernel.scroll_display(Scroll::Bottom);
        }
        let mut snapshot = GridSnapshot::default();
        self.kernel.snapshot(&mut snapshot, 0);
        if offset > 0 {
            self.kernel.scroll_display(Scroll::Offset(offset));
        }
        screen_dump(&snapshot)
    }

    /// 回放的关键帧（见 `crate::dump::checkpoint`）。同步更新（DEC 2026）中缓冲着还没解析的输出时不写（那一段要等
    /// 结束标记一起解析，关键帧接不上），由调用方在之后的位置再要。
    fn checkpoint(&mut self) -> Option<ScreenDump> {
        if self.kernel.sync_buffered() > 0 {
            return None;
        }
        let shell = self.shell_marks();
        Some(crate::dump::checkpoint(&mut self.kernel, &shell))
    }

    /// 关键帧要接着用的 shell 集成状态。
    pub(super) fn shell_marks(&self) -> ShellMarks {
        ShellMarks {
            integration: self.shell.integration,
            marked_input: self.shell.marked_input,
            input: self.shell.input,
            saw_executed: self.marks.saw_executed,
            explicit: self.marks.explicit.as_ref().map(|c| c.0.clone()),
            running: self.marks.started.is_some(),
        }
    }
}

/// 把这一批中已解析的各段交给输出观察者。
fn observe(observer: Option<&OutputObserver>, observed: &mut Vec<Vec<u8>>) {
    if let Some(observer) = observer {
        for chunk in observed.drain(..) {
            observer(&chunk);
        }
    }
}
