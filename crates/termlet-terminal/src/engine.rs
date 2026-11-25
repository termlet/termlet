//! 终端引擎：引擎线程独占内核，把传输送来的字节交给内核解析；界面经引擎的方法读取与操作。
//!
//! - 内核（解析、网格、回滚、模式、选区、应答）只经 `kernel::Kernel` 接口使用，现在由 alacritty_terminal
//!   实现（`engine/alacritty.rs`）；哪些属于内核、哪些在内核之外见 `engine/kernel.rs`。视图拿不到内核的类型：
//!   绘制读可见区的快照（[`TerminalEngine::snapshot`]），查找、选中的文字等要读整个回滚的操作经引擎的方法执行。
//! - 线程（见 `engine/worker.rs`）：每个终端一个引擎线程，内核在这个线程上创建、只在这里使用（内核不需要 `Send`）。
//!   界面线程不碰内核：常读的状态（模式、网格大小、光标等）读引擎线程发布的一份；绘制用的快照由引擎线程填好、界面
//!   换走；滚动、选区、按键的记录等作为命令发给引擎线程（不等）；要结果的（查找、选中的文字等）发过去有界等待。
//!   界面读状态与快照前先等引擎线程执行完自己之前发出的命令，读到的总包含自己的修改。
//! - 引擎线程读到一批输出就解析，并把同一时刻已经到达的输出一起解析（最多 1MB），然后通知界面重绘；通知是合并的：
//!   界面还没处理上一个通知时不再发新的。两段输出之间先处理界面的命令。
//! - 程序开启同步更新（DEC 2026）时内核先缓冲输出，等结束标记或超时后再一起解析，期间不重绘，
//!   全屏程序的画面不会闪。
//! - 通道是有界的：解析跟不上时传输那边发送会等待，自然对 SSH 窗口与 PTY 形成背压。
//! - shell 集成：解析前用 [`crate::shell_events::OscScanner`] 找出 OSC 133 / 633 等标记，按标记的位置把输出
//!   切开分段解析，在 B（输入开始）时给光标所在的格子做标记，在 C（开始执行）时从那里读出命令，见 [`ShellEvent`]。
//!   没有 shell 集成时按回车推断命令（`TerminalEngine::user_input`）。命令补全提示从 B 处读到光标处，得到正在
//!   输入的命令（见 [`crate::suggest`]）。
//! - 终端自己的应答（设备属性、光标位置、窗口大小、颜色查询）由内核按产生的顺序排队，每解析完一段就依次写出。
//! - 扫描器顺带找出 kitty 键盘协议、深浅色切换通知（DEC 2031）的请求与 RIS，在序列之后转给内核
//!   （见 [`crate::kitty_keyboard`]、[`crate::theme_mode`]）；视图编码按键时读取当前的标志。
//! - 回放录制的会话用不接传输的引擎（[`TerminalEngine::replay`]），输出由调用方按录制的顺序送进来，解析的路径与
//!   实时的终端相同（见 `engine/replay.rs`）。

mod alacritty;
pub(crate) mod kernel;
mod replay;
mod worker;

pub use replay::ReplayFeed;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, MutexGuard};
use regex::Regex;

use termlet_io::{CloseReason, GridSize, TerminalInput, TerminalIo};

use self::kernel::{Kernel, KernelEvents, Notices};
use self::worker::{Command, EngineState, Links, Published, Shared, Source, Worker};
use crate::colors::Palette;
use crate::dump::ScreenDump;
pub use crate::grid::CursorShape;
use crate::grid::{
    GridDimensions, GridPoint, GridSnapshot, GridText, Scroll, ScrollPosition, SelectionKind, Side,
    SnapshotParams, SnapshotStamp, TerminalModes,
};
use crate::kitty_keyboard::KeyboardFlags;
use crate::search::{self, SearchMatch};
use crate::shell_events::ShellEvent;
use crate::suggest::PromptInput;
use crate::text::{LogicalLine, logical_lines};

/// 现在使用的内核。
type Backend = alacritty::AlacrittyKernel;

/// 一批最多解析的字节数：解析满一批就发布、通知界面重绘（大量输出时画面也持续更新）。
const MAX_BATCH_BYTES: usize = 1 << 20;

/// 没有 shell 集成时，第一次按键前终端至少空闲这么久，才认为光标停在提示符后面。
const PROMPT_IDLE: Duration = Duration::from_millis(100);

/// 没有 shell 集成时，按回车后远端迟迟没有换行（例如命令在等待输入），过这么久也读一次命令。
const PENDING_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

/// 界面等引擎线程（读状态、取快照、要结果的操作）最多等这么久。引擎线程只在解析一段输出、执行一次查找这类操作
/// 时忙，平时不到 1 毫秒；万一卡住，界面照常响应，读到的是之前的内容。
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// 下一个引擎的编号。
static NEXT_ENGINE: AtomicU64 = AtomicU64::new(1);

/// 观察远端原始输出的回调（会话日志、sudo 密码提示检测），见 [`TerminalEngine::set_output_observer`]。
pub type OutputObserver = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// 引擎发给界面的事件。
#[derive(Clone)]
pub enum EngineEvent {
    /// 有新的输出，需要重绘
    Wakeup,
    /// 窗口标题（OSC 0 / 2）变了，用 [`TerminalEngine::take_title`] 取最新的。合并发送：界面取走之前不再发新的
    Title,
    /// 响铃。合并发送：界面调用 [`TerminalEngine::clear_bell`] 之前不再发新的
    Bell,
    /// 光标闪烁的开关变了
    CursorBlinkingChange,
    /// shell 集成的事件（提示符、命令开始与结束、通知、当前目录、要记进历史的命令）
    Shell(ShellEvent),
    /// 程序要写剪贴板（OSC 52），见 [`crate::clipboard`]
    Clipboard(crate::ClipboardRequest),
    /// 会话结束。`transport` 是结束的那个传输的序号：界面收到时已经换上新传输（重连）的，忽略它，
    /// 见 [`TerminalEngine::is_current_transport`]
    Closed { reason: CloseReason, transport: u64 },
}

impl std::fmt::Debug for EngineEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineEvent::Wakeup => write!(f, "Wakeup"),
            EngineEvent::Title => write!(f, "Title"),
            EngineEvent::Bell => write!(f, "Bell"),
            EngineEvent::CursorBlinkingChange => write!(f, "CursorBlinkingChange"),
            // 命令文本的 Debug 只有长度（可能含密码等秘密）
            EngineEvent::Shell(e) => write!(f, "Shell({e:?})"),
            // 剪贴板的文字同样只有长度
            EngineEvent::Clipboard(request) => write!(f, "Clipboard({request:?})"),
            EngineEvent::Closed { reason, transport } => {
                write!(f, "Closed({reason:?}, transport {transport})")
            }
        }
    }
}

/// 引擎的选项。
#[derive(Clone, Debug)]
pub struct EngineOptions {
    /// 回滚的行数
    pub scrollback: usize,
    pub size: GridSize,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            scrollback: 5000,
            size: GridSize::default(),
        }
    }
}

/// 查找的结果（[`TerminalEngine::search`]）。
pub(crate) struct Found {
    /// 全部匹配，按位置排列
    pub matches: Vec<SearchMatch>,
    /// 查找时主屏幕滚动计数的读数：下次据此只重新查找有变化的部分
    pub position: Option<ScrollPosition>,
    /// 只重新查找了有变化的部分时，自上次以来内容上移的行数；整个重新查找时为 `None`
    pub scrolled: Option<usize>,
}

/// 当前的输入句柄：重连后换成新传输的（网格与回滚保留）。界面写用户的输入，引擎线程写终端的应答。
type SharedInput = Arc<Mutex<Arc<dyn TerminalInput>>>;

/// 一个终端的引擎。丢弃时关闭传输，引擎线程随之退出（内核在引擎线程上释放）。
pub struct TerminalEngine {
    /// 引擎的编号：快照据此认出是不是这个引擎填的（回放跳回前面时视图换上新的引擎）
    id: u64,
    commands: async_channel::Sender<Command<Backend>>,
    /// 发出的命令数：读状态、取快照前等引擎线程处理到这里
    sent: Mutex<u64>,
    shared: Arc<Shared>,
    /// 最近一次发给引擎线程的快照参数
    params: Mutex<SnapshotParams>,
    input: SharedInput,
    events: async_channel::Receiver<EngineEvent>,
    dirty: Arc<AtomicBool>,
    size: Arc<Mutex<GridSize>>,
    /// 当前传输的序号：重连换上新传输后，旧传输的输出不再解析、结束也不再报告
    generation: Arc<AtomicU64>,
    observer: Arc<Mutex<Option<OutputObserver>>>,
    notices: Arc<Notices>,
}

/// 重连到新的 shell 前退出断线前全屏程序留下的模式，否则提示与新的提示符会画在备用屏幕里、方向键失灵：
/// 备用屏幕；滚动区域（DECSTBM，保存、恢复光标，`CSI r` 会把光标移到左上角）；字符集（G0、G1 设为 ASCII，
/// 切回 G0，否则提示符显示成制表符字形）；插入模式、原点模式、自动换行；应用光标键与小键盘；鼠标上报；
/// 括号粘贴；焦点上报；隐藏光标与光标形状；文字属性。
const RESET_MODES: &str = "\x1b[?1049l\x1b7\x1b[r\x1b8\x1b(B\x1b)B\x0f\x1b[4l\x1b[?6l\x1b[?7h\x1b[?1l\x1b>\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?2004l\x1b[?1004l\x1b[?25h\x1b[0 q\x1b[0m";

/// 屏幕上显示的文字（自动换行的多行拼成一行，各行以换行分隔）。
fn screen_text(grid: &impl Kernel) -> String {
    let last = grid.dimensions().screen_lines as i32 - 1;
    logical_lines(grid, 0..=last)
        .into_iter()
        .map(|line| line.text)
        .collect::<Vec<_>>()
        .join("\n")
}

impl TerminalEngine {
    pub fn new(io: TerminalIo, options: EngineOptions) -> Self {
        let source = Source::Transport {
            output: io.output,
            generation: 1,
        };
        let engine = Self::start("terminal-engine", io.input.clone(), &options, source);
        io.input.resize(options.size);
        engine
    }

    /// 引擎与它的线程：内核在引擎线程上创建，输出从 `source` 来。
    fn start(
        name: &str,
        input: Arc<dyn TerminalInput>,
        options: &EngineOptions,
        source: Source,
    ) -> Self {
        let (events_tx, events) = async_channel::unbounded();
        let (commands, commands_rx) = async_channel::unbounded();
        let engine = Self {
            id: NEXT_ENGINE.fetch_add(1, Ordering::Relaxed),
            commands,
            sent: Mutex::new(0),
            shared: Arc::default(),
            params: Mutex::default(),
            input: Arc::new(Mutex::new(input)),
            events,
            dirty: Arc::default(),
            size: Arc::new(Mutex::new(options.size)),
            generation: Arc::new(AtomicU64::new(1)),
            observer: Arc::default(),
            notices: Arc::default(),
        };
        let kernel_events = KernelEvents::new(
            events_tx.clone(),
            engine.size.clone(),
            engine.notices.clone(),
        );
        let (scrollback, size) = (options.scrollback, options.size);
        let links = Links {
            id: engine.id,
            commands: commands_rx,
            source,
            input: engine.input.clone(),
            events: events_tx,
            dirty: engine.dirty.clone(),
            size: engine.size.clone(),
            generation: engine.generation.clone(),
            observer: engine.observer.clone(),
            notices: engine.notices.clone(),
            shared: engine.shared.clone(),
        };
        worker::spawn(
            name,
            move || Backend::new(scrollback, size, kernel_events),
            links,
        );
        engine
    }

    // ------------------------------------------------------------ 与引擎线程

    /// 发出一条命令，返回发出过的命令数。引擎线程已退出时什么也不做。
    fn send(&self, command: Command<Backend>) -> u64 {
        let mut sent = self.sent.lock();
        if self.commands.try_send(command).is_ok() {
            *sent += 1;
        }
        *sent
    }

    /// 等引擎线程执行完此前发出的命令并发布（读到的总包含自己的修改），返回发布的内容（持有共享区的锁）。
    /// 没有未执行的命令时不等。
    fn published(&self) -> MutexGuard<'_, Published> {
        let sent = *self.sent.lock();
        let deadline = Instant::now() + REPLY_TIMEOUT;
        let mut published = self.shared.published.lock();
        while (published.applied < sent || published.version == 0) && !published.stopped {
            if self
                .shared
                .updated
                .wait_until(&mut published, deadline)
                .timed_out()
            {
                tracing::warn!("等终端引擎线程超时");
                break;
            }
        }
        published
    }

    /// 等引擎线程执行完此前发出的命令。
    fn wait_applied(&self) {
        drop(self.published());
    }

    /// 界面常读的状态（模式、网格大小、光标等），见 [`Self::published`]。
    fn state(&self) -> EngineState {
        self.published().state
    }

    /// 在引擎线程上执行 `f`，等它的结果（在此前发出的命令之后执行）。引擎线程已退出或超时时为 `None`。
    fn call<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Worker<Backend>) -> R + Send + 'static,
    ) -> Option<R> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.send(Command::Run(Box::new(move |worker| {
            let _ = tx.send(f(worker));
        })));
        match rx.recv_timeout(REPLY_TIMEOUT) {
            Ok(result) => Some(result),
            Err(RecvTimeoutError::Timeout) => {
                tracing::warn!("等终端引擎线程的结果超时");
                None
            }
            Err(RecvTimeoutError::Disconnected) => None,
        }
    }

    // ------------------------------------------------------------ 传输与设置

    /// 接上新的传输（重连）：网格、回滚与选区都保留，之后的输出接着显示。
    /// 上一个传输还没有结束时（强制重连）它的输出不再显示、终端的应答不再写给新传输、结束也不再报告。
    pub fn attach(&self, io: TerminalIo) {
        // 先换代：引擎线程解析旧传输的每段输出前都看一眼，此后旧传输的输出不再解析、结束不再报告
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        tracing::debug!(transport = generation, "终端接上新的传输");
        self.send(Command::Attach {
            output: io.output,
            input: io.input,
            generation,
        });
        // 等引擎线程换上：之后的输入写给新传输，键盘协议、shell 集成等已复位
        self.wait_applied();
    }

    /// `transport` 是不是当前传输的序号（[`EngineEvent::Closed`] 带的）。
    pub fn is_current_transport(&self, transport: u64) -> bool {
        self.generation.load(Ordering::Acquire) == transport
    }

    /// 回答颜色查询（OSC 4 / 10 / 11 / 12）用的配色；程序改过的颜色优先。背景在深浅之间变了、程序开启了
    /// 深浅色切换通知（DEC 2031）时告诉远端（见 [`crate::theme_mode`]）。返回时通知已写出。
    pub fn set_palette(&self, palette: &Palette) {
        self.send(Command::SetPalette(palette.clone()));
        self.wait_applied();
    }

    /// 取走最新的标题（[`EngineEvent::Title`] 之后调用）；没有新标题时为 `None`，里面的 `None` 表示恢复默认标题。
    pub fn take_title(&self) -> Option<Option<String>> {
        self.notices.title.lock().take()
    }

    /// 界面处理了响铃（之后再响时再通知）。
    pub fn clear_bell(&self) {
        self.notices.bell.store(false, Ordering::Release);
    }

    /// 把一段本地生成的文字（含控制序列）直接显示在终端里，不发给远端，例如重连的提示。
    pub fn inject(&self, text: &str) {
        self.send(Command::Inject(text.to_owned()));
    }

    /// 观察远端的原始输出：引擎线程每收到一段输出（一个 `IoEvent::Output`）就调用一次，传入传输送来的原样字节
    /// （本地插入的提示不算）。在这一批输出解析完、发布了新的状态之后调用，不复制数据；回调要很轻（例如交给后台
    /// 线程）。重连换上新传输后继续生效；`None` 取消。
    pub fn set_output_observer(&self, observer: Option<OutputObserver>) {
        *self.observer.lock() = observer;
    }

    /// 把屏幕现在的内容写成控制序列（会话录制开始时先写它，回放从这个画面接上），并在同一时刻调用 `then`：此前收到的
    /// 远端输出都已在内容中、也都已交给输出观察者；此后的输出在 `then` 返回之后才交给观察者。录制在 `then` 中接到
    /// 观察者上，就能从这里接上，不重不漏。
    ///
    /// 用户往回翻着时取的仍是屏幕（底部），显示位置不变；同步更新（DEC 2026）中缓冲着的输出先解析进来。引擎线程在
    /// 两段输出之间取屏幕，然后等 `then` 返回再接着解析，所以 `then` 中不要再调用这个引擎的方法（会等到超时）。
    pub fn screen_dump_then<R>(&self, then: impl FnOnce(ScreenDump) -> R) -> R {
        let (dump_tx, dump_rx) = std::sync::mpsc::sync_channel(1);
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel::<()>(1);
        self.send(Command::Run(Box::new(move |worker| {
            if dump_tx.send(worker.screen_dump()).is_ok() {
                // 等调用方在 `then` 中接好观察者（它放开 `done_tx` 时返回）：这期间不解析新的输出
                let _ = done_rx.recv_timeout(REPLY_TIMEOUT);
            }
        })));
        let dump = dump_rx.recv_timeout(REPLY_TIMEOUT).unwrap_or_else(|_| {
            tracing::warn!("没有取到终端的屏幕");
            let size = self.size();
            ScreenDump {
                text: String::new(),
                columns: size.columns,
                rows: size.rows,
            }
        });
        let result = then(dump);
        drop(done_tx);
        result
    }

    /// 程序设置的终端模式（应用光标键、括号粘贴、鼠标上报、kitty 键盘协议的标志等）。
    pub fn modes(&self) -> TerminalModes {
        self.state().modes
    }

    /// 网格的大小、回滚的行数与可见区的位置。
    pub fn dimensions(&self) -> GridDimensions {
        self.state().dimensions
    }

    /// 是否在备用屏幕中（vim、htop 等全屏程序）。
    pub fn is_alt_screen(&self) -> bool {
        self.modes().alt_screen
    }

    /// 程序是否开启了深浅色切换通知（DEC 2031）。
    pub fn theme_reports_enabled(&self) -> bool {
        self.state().theme_reports
    }

    /// 程序在所在的屏幕上开启的 kitty 键盘协议标志；没有开启时为空，按键照旧编码。
    pub fn keyboard_flags(&self) -> KeyboardFlags {
        self.modes().keyboard
    }

    // ------------------------------------------------------------ 读网格

    /// 把这一帧的可见区填进 `snapshot`（绘制时调用），之后读快照不再碰内核。`context` 是为补全跨出可见区的逻辑行
    /// （关键词高亮按逻辑行匹配）最多上下各多取的行数，不需要时为 0。
    ///
    /// 快照由引擎线程填：内容没变时 `snapshot` 已是最新的，什么也不做；引擎线程预先填好了（有新输出、界面要重绘时）
    /// 就与它交换缓冲，不复制；都不是最新的才请引擎线程现在填（它最多先解析完手上的一段）。
    pub fn snapshot(&self, snapshot: &mut GridSnapshot, context: usize) {
        self.fill_snapshot(
            snapshot,
            SnapshotParams {
                context,
                prompt_input: false,
            },
        );
    }

    /// 同 [`Self::snapshot`]，另外读出 shell 提示符后正在输入的命令（命令补全提示，见 [`crate::suggest`]），
    /// 放在 [`GridSnapshot::prompt_input`] 中，与快照一致。
    pub(crate) fn snapshot_with_prompt_input(&self, snapshot: &mut GridSnapshot, context: usize) {
        self.fill_snapshot(
            snapshot,
            SnapshotParams {
                context,
                prompt_input: true,
            },
        );
    }

    fn fill_snapshot(&self, snapshot: &mut GridSnapshot, params: SnapshotParams) {
        {
            let mut current = self.params.lock();
            if *current != params {
                *current = params;
                self.send(Command::SetParams(params));
            }
        }
        let deadline = Instant::now() + REPLY_TIMEOUT;
        let mut sent = *self.sent.lock();
        // 请引擎线程现在填时内容的版本：之后填的都能用（输出不断时不追最新的那一版）
        let mut requested: Option<u64> = None;
        let mut published = self.shared.published.lock();
        loop {
            if published.stopped {
                return;
            }
            if published.applied >= sent && published.version > 0 {
                let latest = SnapshotStamp {
                    engine: self.id,
                    version: published.version,
                    params,
                };
                if snapshot.stamp == Some(latest) {
                    return;
                }
                let since = requested.unwrap_or(published.version);
                let usable = published.slot_full
                    && published.slot.stamp.is_some_and(|stamp| {
                        stamp.engine == self.id && stamp.params == params && stamp.version >= since
                    });
                if usable {
                    std::mem::swap(snapshot, &mut published.slot);
                    published.slot_full = false;
                    return;
                }
                // 引擎线程正在填（发了重绘通知之后才填，界面醒来时可能还没填完）：等它
                if requested.is_none() && !published.filling {
                    requested = Some(published.version);
                    published.want_fill = true;
                    drop(published);
                    sent = self.send(Command::Fill);
                    published = self.shared.published.lock();
                    continue;
                }
            }
            if self
                .shared
                .updated
                .wait_until(&mut published, deadline)
                .timed_out()
            {
                tracing::warn!("等终端引擎线程的快照超时");
                return;
            }
        }
    }

    /// shell 提示符后正在输入的命令（接受命令补全提示时），见 [`crate::suggest`]。
    pub(crate) fn prompt_input(&self) -> Option<PromptInput> {
        self.call(|worker| worker.shell.prompt_input(&worker.kernel))
            .flatten()
    }

    /// 光标所在的格子（输入法候选框跟随光标）。
    pub(crate) fn cursor_point(&self) -> GridPoint {
        self.state().cursor
    }

    /// 屏幕上显示的文字（自动换行的多行拼成一行，各行以换行分隔）。
    pub fn screen_text(&self) -> String {
        self.call(|worker| screen_text(&worker.kernel))
            .unwrap_or_default()
    }

    /// 回滚与屏幕的全部文字（写法同 [`Self::screen_text`]；比较两个引擎的内容用）。
    pub fn buffer_text(&self) -> String {
        self.call(|worker| {
            let kernel = &worker.kernel;
            let lines = kernel.lines();
            logical_lines(kernel, lines)
                .into_iter()
                .map(|line| line.text)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
    }

    /// 格子所在的逻辑行（网址识别）；格子不在网格中时为 `None`。
    pub(crate) fn logical_line_at(&self, point: GridPoint) -> Option<LogicalLine> {
        self.call(move |worker| {
            let kernel = &worker.kernel;
            if !kernel.lines().contains(&point.line) {
                return None;
            }
            logical_lines(kernel, point.line..=point.line)
                .into_iter()
                .find(|line| line.offset_of(point).is_some())
        })
        .flatten()
    }

    /// 格子上的超链接（OSC 8）的地址。
    pub(crate) fn hyperlink_at(&self, point: GridPoint) -> Option<String> {
        self.call(move |worker| worker.kernel.hyperlink_at(point))
            .flatten()
    }

    /// 在整个网格（回滚与屏幕）中查找。给出上次的结果与当时的读数时，回滚中的行没变（同一块主屏幕、没有重排或
    /// 清空回滚）就只查新增的部分（见 [`search::refresh`]），否则整个重新查找。
    pub(crate) fn search(
        &self,
        regex: &Regex,
        previous: Option<(ScrollPosition, Vec<SearchMatch>)>,
    ) -> Found {
        let regex = regex.clone();
        self.call(move |worker| {
            let kernel = &worker.kernel;
            let position = kernel.scroll_position();
            let scrolled = previous
                .as_ref()
                .zip(position)
                .and_then(|((before, _), now)| now.lines_since(*before));
            let matches = match (scrolled, &previous) {
                (Some(lines), Some((_, matches))) => {
                    search::refresh(kernel, &regex, matches, lines)
                }
                _ => search::find_all(kernel, &regex),
            };
            Found {
                matches,
                position,
                scrolled,
            }
        })
        .unwrap_or(Found {
            matches: Vec::new(),
            position: None,
            scrolled: None,
        })
    }

    // ------------------------------------------------------------ 可见区与选区

    pub(crate) fn scroll_display(&self, scroll: Scroll) {
        self.send(Command::Scroll(scroll));
    }

    /// 格子不在可见区中时滚动到能看见它。
    pub(crate) fn scroll_to(&self, point: GridPoint) {
        self.send(Command::ScrollTo(point));
    }

    /// 回到最底部并取消选区（用户输入时）。
    pub(crate) fn scroll_to_bottom_and_deselect(&self) {
        self.send(Command::ScrollToBottomAndDeselect);
    }

    /// 清空回滚与屏幕，提示符所在的行成为第一行（与 xterm.js 的 clear 相同）；命令历史记下的输入开始处跟着移动。
    pub(crate) fn clear_buffer(&self) {
        self.send(Command::ClearBuffer);
    }

    pub(crate) fn start_selection(&self, kind: SelectionKind, point: GridPoint, side: Side) {
        self.send(Command::StartSelection(kind, point, side));
    }

    /// 把选区的另一端移到这里（拖动选区时，不等结果）；没有选区时什么也不做。
    pub(crate) fn update_selection(&self, point: GridPoint, side: Side) {
        self.send(Command::UpdateSelection(point, side));
    }

    /// 同 [`Self::update_selection`]，返回有没有选区（没有时没有改动）。
    pub(crate) fn extend_selection(&self, point: GridPoint, side: Side) -> bool {
        self.call(move |worker| worker.update_selection(point, side))
            .unwrap_or(false)
    }

    /// 选中全部（含回滚）。
    pub(crate) fn select_all(&self) {
        self.send(Command::SelectAll);
    }

    pub(crate) fn clear_selection(&self) {
        self.send(Command::ClearSelection);
    }

    /// 没有选区，或选区是空的（只是点了一下）。
    pub(crate) fn selection_is_empty(&self) -> bool {
        self.state().selection_empty
    }

    /// 选中的文字。
    pub(crate) fn selection_text(&self) -> Option<String> {
        self.call(|worker| worker.kernel.selection_text()).flatten()
    }

    // ------------------------------------------------------------ shell 集成与命令历史

    /// 当前会话的 shell 是否开启了 shell 集成（见过 OSC 133 / 633 的提示符或命令标记）。重连后重新判断。
    /// 界面据此提示"让命令历史更准确"，执行登录后的命令时也可以据此等提示符。
    pub fn has_shell_integration(&self) -> bool {
        self.state().integration
    }

    /// 用户的输入（[`crate::TerminalView::write_input`] 在发给远端之前调用，一条命令，不等引擎线程）：回到最底部、
    /// 取消选区，记下"用户输入过"，没有 shell 集成时据此推断命令：
    ///
    /// - 输入开始的位置：上一次回车之后第一次按键时光标所在处（此时提示符已显示、按键的回显尚未到达）；
    /// - 按回车后不立即读取（远端的回显可能还没到），等光标移到新的一行（不是自动换行的续行）时再从输入开始处
    ///   读到行尾；迟迟没有换行时引擎线程在 2 秒后读一次；
    /// - 提前输入（上一条命令的回车还没回显、或者终端刚有输出，提示符可能还没出现）时无法确定命令从哪里开始，
    ///   这一行不记录——宁可漏记，也不记错；
    /// - 不记录：全屏程序（备用屏幕）中的输入、不回显的输入（例如密码，读到的是空）、Ctrl+C / Ctrl+D 放弃的行、
    ///   粘贴的多行内容，以及不符合 [`crate::shell_events::is_recordable`] 的。
    ///
    /// 命令在发给远端之前到达引擎线程，引擎线程先处理命令再解析输出，回显一定在它之后。
    pub(crate) fn user_input(&self, data: &[u8]) {
        self.send(Command::UserInput(data.to_vec()));
    }

    /// 屏幕上当前的输入行（同步输入按回车前检查危险命令）：有 shell 集成时从 B 处读起，否则从推断出的输入
    /// 开始处读起，读到行尾（含自动换行的续行）；全屏程序中、无法确定从哪里开始时为 `None`。
    pub fn input_line(&self) -> Option<String> {
        self.call(|worker| worker.shell.input_line(&worker.kernel))
            .flatten()
    }

    /// 退出断线前的程序留下的终端模式（重连到新的 shell 时），见 `RESET_MODES`。
    pub fn reset_modes(&self) {
        self.inject(RESET_MODES);
    }

    /// 在终端里另起一行显示一条暗色的提示（光标不在行首时先换行）。
    pub fn inject_notice(&self, text: &str) {
        self.send(Command::InjectNotice(text.to_owned()));
    }

    /// 引擎事件（界面在异步任务中逐个接收）。
    pub fn events(&self) -> async_channel::Receiver<EngineEvent> {
        self.events.clone()
    }

    /// 界面处理了重绘通知（之后有新输出时再通知）。
    pub fn clear_dirty(&self) {
        self.dirty.store(false, Ordering::Release);
    }

    /// 写给远端（在调用方的线程上直接交给传输，不经引擎线程）。
    pub fn write(&self, data: &[u8]) {
        if !data.is_empty() {
            self.input.lock().write(data);
        }
    }

    pub fn size(&self) -> GridSize {
        *self.size.lock()
    }

    /// 改变终端大小（大小没变时什么也不做）。网格在引擎线程上改，在告诉传输之前发出：远端因此重画的输出一定在它
    /// 之后解析。
    pub fn resize(&self, size: GridSize) {
        {
            let mut current = self.size.lock();
            if *current == size {
                return;
            }
            let grid_changed = current.columns != size.columns || current.rows != size.rows;
            *current = size;
            if !grid_changed {
                return;
            }
        }
        tracing::debug!(columns = size.columns, rows = size.rows, "终端大小变化");
        self.send(Command::Resize(size));
        self.input.lock().resize(size);
    }

    /// 关闭当前的传输（Tab 关闭时由丢弃引擎触发）。
    pub fn close(&self) {
        tracing::debug!("关闭终端的传输");
        self.input.lock().close();
    }

    /// 回滚的行数。
    pub fn scrollback(&self) -> usize {
        self.state().scrollback
    }

    /// 改变回滚的行数（设置变了），返回是否有变化。变小时截掉最早的历史，显示位置随之收进剩下的回滚中；
    /// 截掉的标记找不回，其余的标记仍然有效（截掉的只是最上面的行，剩下的行号不变）。
    pub fn set_scrollback(&self, lines: usize) -> bool {
        let changed = self
            .call(move |worker| worker.set_scrollback(lines))
            .unwrap_or(false);
        if changed {
            tracing::debug!(lines, "终端回滚行数变化");
        }
        changed
    }

    /// 改变光标的默认样式（设置变了），返回是否有变化。程序用 DECSCUSR 设置过的照样优先，`CSI 0 SP q`、RIS 后
    /// 才用新的默认值。
    pub fn set_default_cursor(&self, shape: CursorShape, blinking: bool) -> bool {
        let changed = self
            .call(move |worker| worker.set_default_cursor(shape, blinking))
            .unwrap_or(false);
        if changed {
            tracing::debug!(?shape, blinking, "光标的默认样式变化");
        }
        changed
    }

    /// 现在的光标是否闪烁：程序用 DECSCUSR、DECSET 12 设置的优先，否则按默认样式。
    pub fn cursor_blinking(&self) -> bool {
        self.state().cursor_blinking
    }
}

impl Drop for TerminalEngine {
    fn drop(&mut self) {
        self.close();
    }
}

/// 测试用：不接界面的内核（查找、文本提取等单独测试内核上的操作）。
#[cfg(test)]
pub(crate) fn test_kernel(columns: usize, lines: usize, scrollback: usize) -> impl Kernel {
    Backend::for_test(columns, lines, scrollback)
}

/// 测试用：按内容直接填好网格的内核。每行一段，`\n` 结束的行自动换行到下一行，`\r\n` 是真正的换行；
/// 列数是最长的一行。
#[cfg(test)]
pub(crate) fn mock_kernel(content: &str) -> impl Kernel {
    Backend::mock(content)
}

/// 只在测试中用的读取（在引擎线程上读内核）。
#[cfg(test)]
impl TerminalEngine {
    /// 格子中的字符（宽字符的占位格为空格）。
    fn char_at(&self, line: i32, column: usize) -> char {
        self.call(move |worker| {
            let mut text = String::new();
            worker
                .kernel
                .push_text(line, column..column + 1, &mut text, None);
            text.chars().next().unwrap_or(' ')
        })
        .unwrap_or(' ')
    }

    /// 一行的文字（去掉行尾的空格）。
    fn row_text(&self, line: i32) -> String {
        self.call(move |worker| {
            let kernel = &worker.kernel;
            let mut text = String::new();
            kernel.push_text(line, 0..kernel.columns(), &mut text, None);
            text.trim_end().to_string()
        })
        .unwrap_or_default()
    }

    fn cursor_shape(&self) -> CursorShape {
        self.call(|worker| worker.kernel.cursor_shape())
            .unwrap_or_default()
    }

    /// 按了回车、有一行在等回显（见 [`Self::user_input`]）。
    fn pending_command(&self) -> bool {
        self.call(|worker| worker.shell.has_pending())
            .unwrap_or(false)
    }

    /// 回滚与屏幕的全部格子，连同各格的下划线颜色与超链接的地址（内核自己编的超链接 ID 各个引擎不同，不比）。
    #[allow(clippy::type_complexity)]
    fn cells_with_extras(
        &self,
    ) -> Vec<(
        GridPoint,
        crate::grid::Cell,
        Option<crate::colors::Color>,
        Option<String>,
    )> {
        self.call(|worker| {
            let kernel = &worker.kernel;
            let d = kernel.dimensions();
            let (top, bottom) = (-(d.history_size as i32), d.screen_lines as i32 - 1);
            let mut snapshot = GridSnapshot::default();
            kernel.snapshot_lines(&mut snapshot, top, bottom);
            let mut cells = Vec::new();
            for line in top..=bottom {
                for column in 0..d.columns {
                    let point = GridPoint::new(line, column);
                    let extra = snapshot.extra(point);
                    cells.push((
                        point,
                        *snapshot.cell(point).unwrap(),
                        extra.and_then(|e| e.underline_color),
                        extra.and_then(|e| e.hyperlink.as_ref().map(|l| l.uri.clone())),
                    ));
                }
            }
            cells
        })
        .unwrap_or_default()
    }

    /// shell 集成记下的状态，输入开始处换成它现在所在的格子（两个引擎的标记计数不同，比格子）。
    fn shell_state(&self) -> (crate::dump::ShellMarks, Option<GridPoint>) {
        self.call(|worker| {
            let mut marks = worker.shell_marks();
            let mut at = None;
            if let Some(mark) = marks.input.take() {
                if worker.kernel.modes().alt_screen {
                    worker
                        .kernel
                        .with_primary_screen(&mut |k| at = k.resolve(&mark));
                } else {
                    at = worker.kernel.resolve(&mark);
                }
            }
            (marks, at)
        })
        .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use termlet_io::IoEvent;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<u8>>);

    impl TerminalInput for Recorder {
        fn write(&self, data: &[u8]) {
            self.0.lock().extend_from_slice(data);
        }
        fn resize(&self, _: GridSize) {}
    }

    fn engine() -> (
        TerminalEngine,
        async_channel::Sender<IoEvent>,
        Arc<Recorder>,
    ) {
        let (tx, rx) = async_channel::bounded(16);
        let recorder = Arc::new(Recorder::default());
        let engine = TerminalEngine::new(
            TerminalIo {
                output: rx,
                input: recorder.clone(),
            },
            EngineOptions::default(),
        );
        (engine, tx, recorder)
    }

    fn wait_for(engine: &TerminalEngine, mut done: impl FnMut(&EngineEvent) -> bool) {
        let events = engine.events();
        futures::executor::block_on(async {
            let timeout = async_io::Timer::after(Duration::from_secs(5));
            futures::pin_mut!(timeout);
            loop {
                let next = events.recv();
                futures::pin_mut!(next);
                match futures::future::select(next, timeout.as_mut()).await {
                    futures::future::Either::Left((Ok(event), _)) => {
                        if done(&event) {
                            return;
                        }
                    }
                    _ => panic!("等待引擎事件超时"),
                }
            }
        });
    }

    fn cell(engine: &TerminalEngine, line: i32, column: usize) -> char {
        engine.char_at(line, column)
    }

    #[test]
    fn output_is_parsed_and_wakes_ui() {
        let (engine, tx, _) = engine();
        tx.send_blocking(IoEvent::Output(b"hi \x1b[31mred".to_vec()))
            .unwrap();
        wait_for(&engine, |e| matches!(e, EngineEvent::Wakeup));
        assert_eq!(cell(&engine, 0, 0), 'h');
        assert_eq!(cell(&engine, 0, 3), 'r');
    }

    #[test]
    fn device_status_report_is_answered() {
        let (engine, tx, recorder) = engine();
        tx.send_blocking(IoEvent::Output(b"ab\x1b[6n".to_vec()))
            .unwrap();
        wait_for(&engine, |e| matches!(e, EngineEvent::Wakeup));
        assert_eq!(&*recorder.0.lock(), b"\x1b[1;3R");
    }

    #[test]
    fn kitty_keyboard_modes_are_kept_per_screen_and_queries_answered_in_order() {
        let (engine, tx, recorder) = engine();
        let output = |bytes: &[u8]| {
            tx.send_blocking(IoEvent::Output(bytes.to_vec())).unwrap();
            wait_for(&engine, |e| matches!(e, EngineEvent::Wakeup));
            engine.clear_dirty();
        };
        // 没有开启时回答 0；查询与设备属性的回答按提问的顺序（crossterm 据此判断支不支持）
        output(b"\x1b[?u\x1b[c");
        let replies = std::mem::take(&mut *recorder.0.lock());
        assert!(replies.starts_with(b"\x1b[?0u\x1b[?"), "{replies:?}");
        assert_eq!(engine.keyboard_flags(), KeyboardFlags::NONE);
        // 压栈、修改、查询
        output(b"\x1b[>1u\x1b[=4;2u\x1b[?u");
        assert_eq!(engine.keyboard_flags(), KeyboardFlags::from_bits(5));
        assert_eq!(&*std::mem::take(&mut *recorder.0.lock()), b"\x1b[?5u");
        // 备用屏幕有自己的栈：同步更新中先切换屏幕再压栈也记在备用屏幕上
        output(b"\x1b[?2026h\x1b[?1049h\x1b[>31u\x1b[?2026l");
        assert_eq!(engine.keyboard_flags(), KeyboardFlags::from_bits(31));
        output(b"\x1b[?1049l");
        assert_eq!(engine.keyboard_flags(), KeyboardFlags::from_bits(5));
        // 大量压栈不出错（栈的大小有上限）
        output(&b"\x1b[>3u".repeat(5000));
        assert_eq!(engine.keyboard_flags(), KeyboardFlags::from_bits(3));
        output(&b"\x1b[<u".repeat(5000));
        assert_eq!(engine.keyboard_flags(), KeyboardFlags::NONE);
        // RIS 清空
        output(b"\x1b[>1u\x1bc");
        assert_eq!(engine.keyboard_flags(), KeyboardFlags::NONE);
        // 换上新的传输时清空
        output(b"\x1b[>1u");
        let (tx2, rx2) = async_channel::bounded(16);
        engine.attach(TerminalIo {
            output: rx2,
            input: recorder.clone(),
        });
        assert_eq!(engine.keyboard_flags(), KeyboardFlags::NONE);
        drop(tx2);
    }

    // 深浅色切换通知（DEC 2031）：开启后配色的深浅变了才通知；查询深浅、DECRQM 按提问的顺序回答（alacritty 的
    // “不支持”不再发出）；关闭、RIS、换上新的传输后不再通知
    #[test]
    fn theme_changes_are_reported_while_enabled() {
        let (engine, tx, recorder) = engine();
        let output = |bytes: &[u8]| {
            tx.send_blocking(IoEvent::Output(bytes.to_vec())).unwrap();
            wait_for(&engine, |e| matches!(e, EngineEvent::Wakeup));
            engine.clear_dirty();
        };
        let take = || String::from_utf8(std::mem::take(&mut *recorder.0.lock())).unwrap();
        engine.set_palette(&Palette::default_dark());
        // 没有开启：DECRQM 回答“关闭”（不是 alacritty 的“不支持”），查询照样回答，与设备属性的回答按顺序
        output(b"\x1b[?2031$p\x1b[?996n\x1b[c");
        let replies = take();
        assert!(
            replies.starts_with("\x1b[?2031;2$y\x1b[?997;1n\x1b[?"),
            "{replies:?}"
        );
        assert!(!replies.contains(";0$y"), "{replies:?}");
        engine.set_palette(&Palette::default_light());
        assert_eq!(take(), "", "没有开启时不通知");
        // 开启：深浅变了才通知
        output(b"\x1b[?2031h\x1b[?2031$p");
        assert!(engine.theme_reports_enabled());
        assert_eq!(take(), "\x1b[?2031;1$y");
        engine.set_palette(&Palette::default_light());
        assert_eq!(take(), "", "深浅没变");
        engine.set_palette(&Palette::default_dark());
        assert_eq!(take(), "\x1b[?997;1n");
        engine.set_palette(&Palette::default_light());
        assert_eq!(take(), "\x1b[?997;2n");
        output(b"\x1b[?996n");
        assert_eq!(take(), "\x1b[?997;2n");
        // 关闭后不再通知
        output(b"\x1b[?2031l");
        assert!(!engine.theme_reports_enabled());
        engine.set_palette(&Palette::default_dark());
        assert_eq!(take(), "");
        // 与切换屏幕写在一起也认；RIS 关闭
        output(b"\x1b[?1049;2031h");
        assert!(engine.theme_reports_enabled());
        output(b"\x1bc");
        assert!(!engine.theme_reports_enabled());
        engine.set_palette(&Palette::default_light());
        assert_eq!(take(), "");
        // 换上新的传输时关闭
        output(b"\x1b[?2031h");
        let (tx2, rx2) = async_channel::bounded(16);
        engine.attach(TerminalIo {
            output: rx2,
            input: recorder.clone(),
        });
        assert!(!engine.theme_reports_enabled());
        engine.set_palette(&Palette::default_dark());
        assert_eq!(take(), "");
        drop(tx2);
    }

    // 光标的默认样式（设置）：程序用 DECSCUSR 设置的优先，`CSI 0 SP q`、RIS 后回到默认的；改回滚行数不丢默认样式
    #[test]
    fn default_cursor_yields_to_decscusr_until_reset() {
        let (engine, tx, _) = engine();
        let output = |bytes: &[u8]| {
            tx.send_blocking(IoEvent::Output(bytes.to_vec())).unwrap();
            wait_for(&engine, |e| matches!(e, EngineEvent::Wakeup));
            engine.clear_dirty();
        };
        let cursor = || engine.cursor_shape();
        assert_eq!(cursor(), CursorShape::Block);
        assert!(!engine.cursor_blinking());

        assert!(engine.set_default_cursor(CursorShape::Bar, true));
        assert!(!engine.set_default_cursor(CursorShape::Bar, true), "没变");
        assert_eq!(cursor(), CursorShape::Bar);
        assert!(engine.cursor_blinking());

        // 稳定的下划线（Ps = 4）
        output(b"\x1b[4 q");
        assert_eq!(cursor(), CursorShape::Underline);
        assert!(!engine.cursor_blinking());
        // 程序设置过的不受默认值的变化影响
        engine.set_default_cursor(CursorShape::Block, true);
        assert_eq!(cursor(), CursorShape::Underline);
        // Ps = 0：回到默认的
        output(b"\x1b[0 q");
        assert_eq!(cursor(), CursorShape::Block);
        assert!(engine.cursor_blinking());

        // 闪烁的竖线（Ps = 5），RIS 后回到默认的
        output(b"\x1b[5 q");
        assert_eq!(cursor(), CursorShape::Bar);
        output(b"\x1bc");
        assert_eq!(cursor(), CursorShape::Block);
        assert!(engine.cursor_blinking());

        // 改回滚行数时默认样式还在
        engine.set_default_cursor(CursorShape::Underline, false);
        assert!(engine.set_scrollback(100));
        assert_eq!(cursor(), CursorShape::Underline);
        assert!(!engine.cursor_blinking());
    }

    #[test]
    fn title_and_close_are_reported() {
        let (engine, tx, _) = engine();
        tx.send_blocking(IoEvent::Output(b"\x1b]0;web-01\x07".to_vec()))
            .unwrap();
        wait_for(&engine, |e| matches!(e, EngineEvent::Title));
        assert_eq!(engine.take_title(), Some(Some("web-01".into())));
        assert_eq!(engine.take_title(), None);
        tx.send_blocking(IoEvent::Closed(CloseReason::Exited(Some(0))))
            .unwrap();
        wait_for(&engine, |e| {
            matches!(
                e,
                EngineEvent::Closed {
                    reason: CloseReason::Exited(Some(0)),
                    ..
                }
            )
        });
    }

    #[test]
    fn synchronized_update_is_flushed_after_timeout() {
        let (engine, tx, _) = engine();
        // 开始同步更新但不结束：超时后也要显示出来
        tx.send_blocking(IoEvent::Output(b"\x1b[?2026hstuck".to_vec()))
            .unwrap();
        // 开始标记本身先解析、先唤醒一次；像界面一样清掉通知标记，等超时后的那次唤醒
        let started = Instant::now();
        wait_for(&engine, |e| {
            if matches!(e, EngineEvent::Wakeup) {
                engine.clear_dirty();
            }
            cell(&engine, 0, 0) == 's'
        });
        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "应等到超时才显示"
        );
    }

    #[test]
    fn attach_continues_after_close() {
        let (engine, tx, _) = engine();
        tx.send_blocking(IoEvent::Output(b"first".to_vec()))
            .unwrap();
        tx.send_blocking(IoEvent::Closed(CloseReason::Failed("断开".into())))
            .unwrap();
        wait_for(&engine, |e| matches!(e, EngineEvent::Closed { .. }));
        let (tx2, rx2) = async_channel::bounded(16);
        let recorder = Arc::new(Recorder::default());
        engine.attach(TerminalIo {
            output: rx2,
            input: recorder.clone(),
        });
        engine.clear_dirty();
        tx2.send_blocking(IoEvent::Output(b"\r\nsecond".to_vec()))
            .unwrap();
        wait_for(&engine, |e| matches!(e, EngineEvent::Wakeup));
        assert_eq!(cell(&engine, 0, 0), 'f', "旧的内容保留");
        assert_eq!(cell(&engine, 1, 0), 's');
        engine.write(b"x");
        assert_eq!(&*recorder.0.lock(), b"x", "输入发给新的传输");
    }

    #[test]
    fn superseded_transport_is_ignored() {
        let (engine, tx, _) = engine();
        // 强制重连：旧传输还没有结束就换上新的
        let (tx2, rx2) = async_channel::bounded(16);
        engine.attach(TerminalIo {
            output: rx2,
            input: Arc::new(Recorder::default()),
        });
        let _ = tx.send_blocking(IoEvent::Output(b"old".to_vec()));
        let _ = tx.send_blocking(IoEvent::Closed(CloseReason::Lost("断开".into())));
        tx2.send_blocking(IoEvent::Output(b"new".to_vec())).unwrap();
        wait_for(&engine, |_| cell(&engine, 0, 0) == 'n');
        tx2.send_blocking(IoEvent::Closed(CloseReason::Exited(Some(0))))
            .unwrap();
        // 收到的结束只有新传输的
        wait_for(&engine, |e| match e {
            EngineEvent::Closed { reason, transport } => {
                assert_eq!(reason, &CloseReason::Exited(Some(0)));
                assert!(engine.is_current_transport(*transport));
                true
            }
            _ => false,
        });
        assert_eq!(cell(&engine, 0, 3), ' ', "旧传输的输出没有写进来");
    }

    #[test]
    fn closed_before_reconnect_belongs_to_the_old_transport() {
        let (engine, tx, _) = engine();
        let events = engine.events();
        tx.send_blocking(IoEvent::Closed(CloseReason::Lost("断开".into())))
            .unwrap();
        let transport = loop {
            if let Ok(EngineEvent::Closed { transport, .. }) = events.recv_blocking() {
                break transport;
            }
        };
        assert!(engine.is_current_transport(transport));
        // 界面还没处理这个结束就重连了：处理时据序号认出它属于旧传输
        let (_tx2, rx2) = async_channel::bounded(16);
        engine.attach(TerminalIo {
            output: rx2,
            input: Arc::new(Recorder::default()),
        });
        assert!(!engine.is_current_transport(transport));
    }

    #[test]
    fn color_queries_are_answered_in_order_with_overrides() {
        let (engine, tx, recorder) = engine();
        engine.set_palette(&Palette::default_light());
        // 先问背景色再问设备属性（delta、bat 的做法）：回答的顺序与提问相同
        tx.send_blocking(IoEvent::Output(b"\x1b]11;?\x07\x1b[c".to_vec()))
            .unwrap();
        eventually(|| recorder.0.lock().ends_with(b"c"));
        assert_eq!(
            String::from_utf8(recorder.0.lock().clone()).unwrap(),
            "\x1b]11;rgb:ffff/ffff/ffff\x07\x1b[?6c"
        );
        recorder.0.lock().clear();
        // 程序改过的颜色优先
        tx.send_blocking(IoEvent::Output(
            b"\x1b]11;rgb:01/02/03\x07\x1b]4;1;?\x07\x1b]11;?\x07".to_vec(),
        ))
        .unwrap();
        eventually(|| recorder.0.lock().iter().filter(|&&b| b == 0x07).count() == 2);
        assert_eq!(
            String::from_utf8(recorder.0.lock().clone()).unwrap(),
            "\x1b]4;1;rgb:c7c7/2424/3a3a\x07\x1b]11;rgb:0101/0202/0303\x07"
        );
    }

    #[test]
    fn bells_and_titles_are_coalesced() {
        let (engine, tx, _) = engine();
        let mut output = "\x07".repeat(1000);
        for i in 0..100 {
            output.push_str(&format!("\x1b]0;title {i}\x07"));
        }
        output.push_str("end");
        tx.send_blocking(IoEvent::Output(output.into_bytes()))
            .unwrap();
        eventually(|| cell(&engine, 0, 0) == 'e');
        let events = engine.events();
        let mut bells = 0;
        let mut titles = 0;
        while let Ok(event) = events.try_recv() {
            match event {
                EngineEvent::Bell => bells += 1,
                EngineEvent::Title => titles += 1,
                _ => {}
            }
        }
        assert_eq!((bells, titles), (1, 1));
        assert_eq!(engine.take_title(), Some(Some("title 99".into())));
        // 界面处理过之后再响、再改标题时照常通知
        engine.clear_bell();
        tx.send_blocking(IoEvent::Output(b"\x07\x1b]0;x\x07".to_vec()))
            .unwrap();
        let mut seen = (false, false);
        wait_for(&engine, |e| {
            match e {
                EngineEvent::Bell => seen.0 = true,
                EngineEvent::Title => seen.1 = true,
                _ => {}
            }
            seen == (true, true)
        });
    }

    #[test]
    fn reset_modes_restores_scroll_region_charset_and_insert_mode() {
        let (engine, _tx) = small_engine(10, 4, 100);
        // 断线前的程序：滚动区域只有前两行、DEC 制表符字符集、插入模式
        engine.inject("\x1b[1;2r\x1b(0\x1b[4h");
        engine.reset_modes();
        engine.inject("\x1b[4;1Hab\x1b[4;1Hq");
        assert_eq!(
            cell(&engine, 3, 0),
            'q',
            "字符集回到 ASCII，且是覆盖而不是插入"
        );
        assert_eq!(cell(&engine, 3, 1), 'b');
        // 最后一行换行时整屏上移（滚动区域已复位；没复位时光标在区域之外，不滚动）
        engine.inject("\r\n");
        assert_eq!(cell(&engine, 2, 0), 'q');
    }

    #[test]
    fn notice_starts_on_its_own_line() {
        let (engine, tx, _) = engine();
        tx.send_blocking(IoEvent::Output(b"$ ".to_vec())).unwrap();
        wait_for(&engine, |_| cell(&engine, 0, 0) == '$');
        engine.inject_notice("ok");
        assert_eq!(cell(&engine, 1, 0), 'o');
        assert_eq!(engine.cursor_point().line, 2);
        engine.inject("\x1b[?1049h");
        engine.reset_modes();
        assert!(!engine.modes().alt_screen);
    }

    #[test]
    fn resize_changes_grid() {
        let (engine, _tx, _) = engine();
        engine.resize(GridSize::new(100, 30));
        let dimensions = engine.dimensions();
        assert_eq!(dimensions.columns, 100);
        assert_eq!(dimensions.screen_lines, 30);
    }

    /// 快照中一行的文字（去掉行尾的空格）。
    fn snapshot_row(snapshot: &GridSnapshot, line: i32) -> String {
        let mut text = String::new();
        snapshot.push_text(line, 0..snapshot.columns(), &mut text, None);
        text.trim_end().to_string()
    }

    // 快照由引擎线程填好交给界面：有新输出时是新的；界面自己的修改（滚动、选区、大小，都不等引擎线程）之后马上取的
    // 快照已包含它；内容没变时手里的那份就是最新的；另拿一个空的缓冲也能取到；多取的行数变了时重新填
    #[test]
    fn snapshot_follows_output_and_own_changes() {
        let (engine, tx) = small_engine(10, 3, 100);
        let mut snapshot = GridSnapshot::default();
        send(&tx, &numbered_lines(0..10));
        eventually(|| engine.row_text(1) == "line 9");
        engine.snapshot(&mut snapshot, 0);
        assert_eq!(snapshot_row(&snapshot, 0), "line 8");
        assert_eq!(snapshot_row(&snapshot, 1), "line 9");
        let stamp = snapshot.stamp;
        engine.snapshot(&mut snapshot, 0);
        assert_eq!(snapshot.stamp, stamp, "没有变化时还是这一份");

        engine.scroll_display(Scroll::Delta(2));
        engine.snapshot(&mut snapshot, 0);
        assert_eq!(snapshot.display_offset(), 2);
        assert_eq!(snapshot_row(&snapshot, -2), "line 6");

        engine.start_selection(SelectionKind::Simple, GridPoint::new(-2, 0), Side::Left);
        engine.update_selection(GridPoint::new(-2, 3), Side::Right);
        engine.snapshot(&mut snapshot, 0);
        let selection = snapshot.selection().expect("有选区");
        assert_eq!(
            (selection.start, selection.end),
            (GridPoint::new(-2, 0), GridPoint::new(-2, 3))
        );
        assert_eq!(engine.selection_text().as_deref(), Some("line"));

        let mut other = GridSnapshot::default();
        engine.snapshot(&mut other, 0);
        assert_eq!(other.selection(), snapshot.selection());
        assert_eq!(snapshot_row(&other, -2), "line 6");

        engine.snapshot(&mut snapshot, 3);
        assert_eq!(snapshot.stamp.map(|stamp| stamp.params.context), Some(3));
        engine.resize(GridSize::new(12, 4));
        engine.snapshot(&mut snapshot, 0);
        assert_eq!((snapshot.columns(), snapshot.screen_lines()), (12, 4));
    }

    #[test]
    fn output_observer_sees_raw_bytes_across_reconnects() {
        let (engine, tx, _) = engine();
        let seen = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink = seen.clone();
        engine.set_output_observer(Some(Arc::new(move |bytes: &[u8]| {
            sink.lock().extend_from_slice(bytes)
        })));
        tx.send_blocking(IoEvent::Output(b"a\x1b[?1049h".to_vec()))
            .unwrap();
        eventually(|| seen.lock().as_slice() == b"a\x1b[?1049h");
        assert!(engine.is_alt_screen());
        engine.inject("本地的提示");
        tx.send_blocking(IoEvent::Closed(CloseReason::Lost("断开".into())))
            .unwrap();
        wait_for(&engine, |e| matches!(e, EngineEvent::Closed { .. }));
        // 重连后继续生效
        let (tx2, rx2) = async_channel::bounded(16);
        engine.attach(TerminalIo {
            output: rx2,
            input: Arc::new(Recorder::default()),
        });
        tx2.send_blocking(IoEvent::Output(b"\x1b[?1049lb".to_vec()))
            .unwrap();
        eventually(|| seen.lock().ends_with(b"\x1b[?1049lb"));
        assert_eq!(seen.lock().len(), 9 + 9, "本地插入的不算");
        assert!(!engine.is_alt_screen());
        engine.set_output_observer(None);
    }

    // ------------------------------------------------------------ shell 集成

    const A: &str = "\x1b]133;A\x07";
    const B: &str = "\x1b]133;B\x07";
    const C: &str = "\x1b]133;C\x07";

    fn small_engine(
        columns: u16,
        rows: u16,
        scrollback: usize,
    ) -> (TerminalEngine, async_channel::Sender<IoEvent>) {
        let (tx, rx) = async_channel::bounded(16);
        let engine = TerminalEngine::new(
            TerminalIo {
                output: rx,
                input: Arc::new(Recorder::default()),
            },
            EngineOptions {
                scrollback,
                size: GridSize::new(columns, rows),
            },
        );
        (engine, tx)
    }

    fn send(tx: &async_channel::Sender<IoEvent>, text: &str) {
        tx.send_blocking(IoEvent::Output(text.as_bytes().to_vec()))
            .unwrap();
    }

    /// 收集 shell 事件，直到 `done` 为真（含那一个）。
    fn shell_events(
        engine: &TerminalEngine,
        done: impl Fn(&ShellEvent) -> bool,
    ) -> Vec<ShellEvent> {
        let mut seen = Vec::new();
        wait_for(engine, |e| match e {
            EngineEvent::Shell(event) => {
                seen.push(event.clone());
                done(event)
            }
            _ => false,
        });
        seen
    }

    fn finished(e: &ShellEvent) -> bool {
        matches!(e, ShellEvent::CommandFinished { .. })
    }

    fn entered(events: &[ShellEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|e| match e {
                ShellEvent::CommandEntered(c) => Some(c.as_str()),
                _ => None,
            })
            .collect()
    }

    fn started(events: &[ShellEvent]) -> Vec<Option<&str>> {
        events
            .iter()
            .filter_map(|e| match e {
                ShellEvent::CommandStart { command } => Some(command.as_ref().map(|c| c.as_str())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn command_text_and_exit_code_from_osc_133() {
        let (engine, tx) = small_engine(80, 24, 100);
        assert!(!engine.has_shell_integration());
        engine.user_input(b"l");
        send(&tx, &format!("{A}ruan@web:~$ {B}"));
        send(&tx, "ls -la /tmp\r\n");
        send(&tx, &format!("{C}total 0\r\n\x1b]133;D;2\x07"));
        let events = shell_events(&engine, finished);
        assert_eq!(events[0], ShellEvent::PromptStart);
        assert_eq!(started(&events), vec![Some("ls -la /tmp")]);
        assert_eq!(entered(&events), vec!["ls -la /tmp"]);
        assert!(matches!(
            events.last(),
            Some(ShellEvent::CommandFinished {
                exit_code: Some(2),
                ..
            })
        ));
        assert!(engine.has_shell_integration());
    }

    #[test]
    fn wrapped_command_at_the_bottom_with_full_history() {
        // 20 列 4 行、回滚 3 行：回滚很快就满了，提示符在最底行，长命令折行、回车都会让屏幕上移
        let (engine, tx) = small_engine(20, 4, 3);
        engine.user_input(b"x");
        send(&tx, &"old output\r\n".repeat(10));
        let command = "echo 0123456789abcdefghij-0123456789";
        // 一个字符一段输出，像逐键回显
        send(&tx, &format!("{A}$ {B}"));
        for c in command.chars() {
            send(&tx, &c.to_string());
        }
        send(&tx, &format!("\r\n{C}0123\r\n\x1b]133;D;0\x07"));
        let events = shell_events(&engine, finished);
        assert_eq!(started(&events), vec![Some(command)]);
        assert_eq!(entered(&events), vec![command]);
    }

    #[test]
    fn marks_inside_synchronized_update() {
        let (engine, tx) = small_engine(40, 5, 100);
        engine.user_input(b"x");
        // 提示符整个在同步更新中画出（B 到达时 vte 还没有把提示符写进网格）
        send(&tx, &format!("\x1b[?2026h{A}\x1b[2K\r> {B}\x1b[?2026l"));
        send(&tx, "make\r\n");
        // C 也在同步更新中，且同步更新没有在这批输出中结束
        send(&tx, &format!("\x1b[?2026h{C}building"));
        let events = shell_events(&engine, |e| matches!(e, ShellEvent::CommandStart { .. }));
        assert_eq!(started(&events), vec![Some("make")]);
        // 同步更新照常等到结束（这里是超时）才显示
        eventually(|| cell(&engine, 1, 0) == 'b');
    }

    #[test]
    fn vscode_command_line_shell_without_c_and_cancelled_lines() {
        let (engine, tx) = small_engine(80, 24, 100);
        engine.user_input(b"x");
        // 633;E 给出的命令行优先（屏幕上的是 shell 画的别的样子）
        send(
            &tx,
            "\x1b]633;A\x07$ \x1b]633;B\x07gs\r\n\x1b]633;E;git status\\x3b echo \\\\;n1\x07\x1b]633;C\x07",
        );
        let events = shell_events(&engine, |e| matches!(e, ShellEvent::CommandStart { .. }));
        assert_eq!(started(&events), vec![Some("git status; echo \\")]);
        assert_eq!(entered(&events), vec!["git status; echo \\"]);

        // 只有 A、B 的 shell：下一个提示符开始时补记
        let (engine, tx) = small_engine(80, 24, 100);
        engine.user_input(b"x");
        send(&tx, &format!("{A}$ {B}uptime\r\n up 3 days\r\n{A}$ {B}"));
        let events = shell_events(&engine, |e| matches!(e, ShellEvent::CommandEntered(_)));
        assert_eq!(entered(&events), vec!["uptime"]);

        // 输出过 C 的 shell：没有 C 就出现新的提示符说明那一行被取消了；以空格开头的、只有一个字符的不记
        let (engine, tx) = small_engine(80, 24, 100);
        engine.user_input(b"x");
        send(
            &tx,
            &format!("{A}$ {B}ls\r\n{C}{A}$ {B}rm -rf x^C\r\n{A}$ {B} mysql -psecret\r\n{C}"),
        );
        send(
            &tx,
            &format!("{A}$ {B}w\r\n{C}{A}$ {B}pwd\r\n{C}\x1b]133;D\x07"),
        );
        let events = shell_events(&engine, finished);
        assert_eq!(entered(&events), vec!["ls", "pwd"]);
        assert_eq!(
            started(&events),
            vec![Some("ls"), Some(" mysql -psecret"), Some("w"), Some("pwd")]
        );
    }

    #[test]
    fn commands_before_user_input_are_not_entered() {
        let (engine, tx) = small_engine(80, 24, 100);
        // 登录后由应用自动执行的命令：用户还没有输入过
        send(&tx, &format!("{A}$ {B}cd /srv\r\n{C}\x1b]133;D;0\x07"));
        let events = shell_events(&engine, finished);
        assert_eq!(started(&events), vec![Some("cd /srv")]);
        assert!(entered(&events).is_empty());
    }

    #[test]
    fn notifications_and_working_directory() {
        let (engine, tx) = small_engine(80, 24, 100);
        send(
            &tx,
            "\x1b]7;file://web/home/ruan/my%20app\x07\x1b]9;4;1;50\x07\x1b]777;notify;部署;完成\x1b\\",
        );
        let events = shell_events(&engine, |e| matches!(e, ShellEvent::Notification { .. }));
        assert_eq!(
            events,
            vec![
                ShellEvent::WorkingDirectory("/home/ruan/my app".into()),
                ShellEvent::Notification {
                    title: "部署".into(),
                    body: "完成".into()
                },
            ]
        );
        // 只有通知、目录不算开启了 shell 集成
        assert!(!engine.has_shell_integration());
    }

    /// 等到条件成立（解析在后台线程进行）。
    fn eventually(condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "等待超时");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn cursor_column(engine: &TerminalEngine) -> usize {
        engine.cursor_point().column
    }

    /// 没有 shell 集成：等终端空闲后逐键输入，远端逐键回显；等回显都解析完。
    fn type_line(engine: &TerminalEngine, tx: &async_channel::Sender<IoEvent>, line: &str) {
        std::thread::sleep(PROMPT_IDLE + Duration::from_millis(50));
        let start = cursor_column(engine);
        for c in line.chars() {
            let key = c.to_string();
            engine.user_input(key.as_bytes());
            send(tx, &key);
        }
        eventually(|| cursor_column(engine) == start + line.chars().count());
    }

    #[test]
    fn commands_are_inferred_without_shell_integration() {
        let (engine, tx) = small_engine(80, 24, 100);
        send(&tx, "$ ");
        type_line(&engine, &tx, "uname -a");
        engine.user_input(b"\r");
        assert!(engine.pending_command(), "按了回车，等回显");
        send(&tx, "\r\nLinux\r\n$ ");
        let events = shell_events(&engine, |e| matches!(e, ShellEvent::CommandEntered(_)));
        assert_eq!(entered(&events), vec!["uname -a"]);

        // 以空格开头的不记；屏幕上的输入行（同步输入检查危险命令）
        type_line(&engine, &tx, " secret");
        assert_eq!(engine.input_line().as_deref(), Some(" secret"));
        engine.user_input(b"\r");
        // 回显与切换到备用屏幕在同一段：切换之前读出
        send(&tx, "\r\n$ ");
        type_line(&engine, &tx, "vim a.txt");
        engine.user_input(b"\r");
        send(&tx, "\r\n\x1b[?1049h\x1b[H\x1b[2J~\r\n~");
        let events = shell_events(&engine, |e| matches!(e, ShellEvent::CommandEntered(_)));
        assert_eq!(entered(&events), vec!["vim a.txt"]);
        // 全屏程序中的输入不记
        eventually(|| engine.modes().alt_screen);
        engine.user_input(b"i");
        assert_eq!(engine.input_line(), None);
    }

    #[test]
    fn pending_command_is_read_after_timeout() {
        let (engine, tx) = small_engine(80, 24, 100);
        send(&tx, "$ ");
        type_line(&engine, &tx, "read x");
        let pressed = Instant::now();
        engine.user_input(b"\r");
        // 远端一直没有换行：引擎线程在超时后读一次
        let events = shell_events(&engine, |e| matches!(e, ShellEvent::CommandEntered(_)));
        assert_eq!(entered(&events), vec!["read x"]);
        assert!(pressed.elapsed() >= PENDING_COMMAND_TIMEOUT, "超时后才读");
    }

    // ------------------------------------------------------------ 命令补全提示

    /// 正在输入的命令（命令补全提示读到的）。
    fn prompt_text(engine: &TerminalEngine) -> Option<String> {
        engine.prompt_input().map(|input| input.text)
    }

    /// 用户键入、远端回显，等光标到 `column`。
    fn type_echoed(
        engine: &TerminalEngine,
        tx: &async_channel::Sender<IoEvent>,
        text: &str,
        column: usize,
    ) {
        engine.user_input(text.as_bytes());
        send(tx, text);
        eventually(|| cursor_column(engine) == column);
    }

    // 只在 shell 提示符后（B 之后、C 之前）、用户在这个提示符后输入过、光标在行尾时有；末尾的空格保留
    #[test]
    fn prompt_input_follows_the_shell_prompt() {
        let (engine, tx) = small_engine(40, 6, 100);
        // 没有 shell 集成：不提示
        send(&tx, "$ ");
        type_echoed(&engine, &tx, "ls", 4);
        assert_eq!(prompt_text(&engine), None);

        send(&tx, &format!("\r\n{A}$ {B}"));
        eventually(|| engine.has_shell_integration() && engine.cursor_point().line == 1);
        assert_eq!(prompt_text(&engine), None, "这个提示符后还没有输入");
        type_echoed(&engine, &tx, "git ", 6);
        assert_eq!(prompt_text(&engine).as_deref(), Some("git "));
        let input = engine.prompt_input().unwrap();
        assert_eq!(input.cursor, GridPoint::new(1, 6));
        type_echoed(&engine, &tx, "s", 7);

        // 光标移到中间：之后有文字，不提示；回到行尾又有
        engine.user_input(b"\x1b[D");
        send(&tx, "\x08");
        eventually(|| cursor_column(&engine) == 6);
        assert_eq!(prompt_text(&engine), None);
        engine.user_input(b"\x1b[C");
        send(&tx, "\x1b[C");
        eventually(|| cursor_column(&engine) == 7);
        assert_eq!(prompt_text(&engine).as_deref(), Some("git s"));

        // 按了回车：回显到达之前就不再提示
        engine.user_input(b"\r");
        assert_eq!(prompt_text(&engine), None);
        // 命令执行中（C 之后）的输入不算
        send(&tx, &format!("\r\n{C}"));
        eventually(|| engine.cursor_point().line == 2);
        type_echoed(&engine, &tx, "yes", 3);
        assert_eq!(prompt_text(&engine), None);
        // 新的提示符：输入之后才有
        send(&tx, &format!("\r\n\x1b]133;D;0\x07{A}$ {B}"));
        eventually(|| engine.cursor_point() == GridPoint::new(3, 2));
        assert_eq!(prompt_text(&engine), None);
        type_echoed(&engine, &tx, "make", 6);
        assert_eq!(prompt_text(&engine).as_deref(), Some("make"));
        // zsh 重画提示符（reset-prompt）只再输出 B：输入中的命令照样有
        send(&tx, &format!("\r\x1b[K% {B}make"));
        eventually(|| cell(&engine, 3, 0) == '%');
        assert_eq!(prompt_text(&engine).as_deref(), Some("make"));
        // Ctrl+C 放弃这一行
        engine.user_input(b"\x03");
        assert_eq!(prompt_text(&engine), None);
    }

    // 不回显的输入（读到的是空）、全屏程序、远端的 shell 自己画的提示（光标之后有文字）、改变大小之后都没有
    #[test]
    fn prompt_input_is_absent_for_hidden_input_full_screen_and_resize() {
        let (engine, tx) = small_engine(40, 6, 100);
        send(&tx, &format!("{A}$ {B}"));
        eventually(|| engine.has_shell_integration() && cursor_column(&engine) == 2);
        engine.user_input(b"secret");
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(prompt_text(&engine), None, "没有回显");
        // zsh-autosuggestions 这样把提示画进网格：光标之后有文字
        send(&tx, "gi\x1b[2mt status\x1b[0m\x1b[8D");
        eventually(|| cursor_column(&engine) == 4);
        assert_eq!(prompt_text(&engine), None);
        send(&tx, "\x1b[K");
        eventually(|| prompt_text(&engine).as_deref() == Some("gi"));
        // 全屏程序（没有 C 的 shell 直接切到备用屏幕）
        send(&tx, "\x1b[?1049h\x1b[H");
        eventually(|| engine.is_alt_screen() && cursor_column(&engine) == 0);
        type_echoed(&engine, &tx, "abc", 3);
        assert_eq!(prompt_text(&engine), None);
        send(&tx, "\x1b[?1049l");
        eventually(|| !engine.is_alt_screen());
        assert_eq!(prompt_text(&engine).as_deref(), Some("gi"));
        // 改变大小之后输入开始处的标记失效
        engine.resize(GridSize::new(30, 6));
        assert_eq!(prompt_text(&engine), None);
    }

    // 自动换行的长命令与宽字符：从 B 读到光标；多行命令的续行提示符后（真正的换行）没有
    #[test]
    fn prompt_input_spans_wrapped_lines_and_wide_characters() {
        let (engine, tx) = small_engine(10, 6, 100);
        send(&tx, &format!("{A}$ {B}"));
        eventually(|| engine.has_shell_integration() && cursor_column(&engine) == 2);
        // “$ echo 中”占 9 格，“文”换到下一行（行尾留下占位），“测试 a”接着
        engine.user_input("echo 中文测试 a".as_bytes());
        send(&tx, "echo 中文测试 a");
        eventually(|| engine.cursor_point() == GridPoint::new(1, 8));
        assert_eq!(prompt_text(&engine).as_deref(), Some("echo 中文测试 a"));
        // 续行（PS2）：不在输入开始处所在的逻辑行上
        engine.user_input(b"\\");
        send(&tx, "\\\r\n> ");
        eventually(|| engine.cursor_point() == GridPoint::new(2, 2));
        engine.user_input(b"b");
        send(&tx, "b");
        eventually(|| cursor_column(&engine) == 3);
        assert_eq!(prompt_text(&engine), None);
    }

    // ------------------------------------------------------------ 回滚的行数

    fn row_text(engine: &TerminalEngine, line: i32) -> String {
        engine.row_text(line)
    }

    fn numbered_lines(range: std::ops::Range<usize>) -> String {
        range.map(|i| format!("line {i}\r\n")).collect()
    }

    // 设置中改了回滚的行数：改小时截掉最早的历史、显示位置收进剩下的回滚，改大后能保留更多
    #[test]
    fn scrollback_can_change_while_open() {
        let (engine, tx) = small_engine(20, 5, 100);
        let history = || engine.dimensions().history_size;
        // 屏幕 5 行：最后一行是光标所在的空行，line 56–59 在屏幕上，之前的 56 行在回滚中
        send(&tx, &numbered_lines(0..60));
        eventually(|| history() == 56);
        engine.scroll_display(Scroll::Delta(50));

        assert!(engine.set_scrollback(10));
        assert_eq!(engine.scrollback(), 10);
        assert_eq!(history(), 10);
        assert_eq!(row_text(&engine, -10), "line 46", "留下最近的 10 行");
        assert_eq!(row_text(&engine, 0), "line 56", "屏幕上的行不变");
        assert_eq!(
            engine.dimensions().display_offset,
            10,
            "显示位置不超出剩下的回滚"
        );
        assert!(!engine.set_scrollback(10), "行数没变时什么也不做");

        assert!(engine.set_scrollback(100));
        send(&tx, &numbered_lines(60..200));
        eventually(|| row_text(&engine, 3) == "line 199");
        assert_eq!(history(), 100, "改大后保留到新的行数");
        assert_eq!(row_text(&engine, -100), "line 96");
    }
}

/// 吞吐基准：`cargo test -p termlet-terminal --release throughput -- --ignored --nocapture`
#[cfg(test)]
mod bench {
    use super::*;
    use std::time::Duration;
    use termlet_io::IoEvent;

    struct Sink;

    impl TerminalInput for Sink {
        fn write(&self, _: &[u8]) {}
        fn resize(&self, _: GridSize) {}
    }

    #[test]
    #[ignore = "吞吐测试，手动运行"]
    fn throughput() {
        // 每行带颜色与中文，类似日志输出
        let line = "\x1b[32m2026-10-15 12:00:00\x1b[0m INFO  服务启动完成 request_id=0123456789abcdef path=/api/v1/hosts status=200\r\n";
        let chunk: Vec<u8> = line.repeat(64 * 1024 / line.len()).into_bytes();
        let total_mb = 200usize;
        let chunks = total_mb * 1024 * 1024 / chunk.len();
        let (tx, rx) = async_channel::bounded(64);
        let engine = TerminalEngine::new(
            TerminalIo {
                output: rx,
                input: Arc::new(Sink),
            },
            EngineOptions {
                scrollback: 10_000,
                size: GridSize::new(200, 50),
            },
        );
        let started = Instant::now();
        for _ in 0..chunks {
            tx.send_blocking(IoEvent::Output(chunk.clone())).unwrap();
        }
        tx.send_blocking(IoEvent::Closed(CloseReason::Exited(Some(0))))
            .unwrap();
        let events = engine.events();
        loop {
            match events.recv_blocking() {
                Ok(EngineEvent::Closed { .. }) => break,
                Ok(EngineEvent::Wakeup) => engine.clear_dirty(),
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let elapsed = started.elapsed().max(Duration::from_millis(1));
        let mb = (chunks * chunk.len()) as f64 / 1024.0 / 1024.0;
        println!(
            "解析 {mb:.0} MB 用时 {:.2} 秒，{:.0} MB/s",
            elapsed.as_secs_f64(),
            mb / elapsed.as_secs_f64()
        );
    }

    /// 写入时原样回显的传输（像远端的 shell 回显按键）。
    struct Echo(async_channel::Sender<IoEvent>);

    impl TerminalInput for Echo {
        fn write(&self, data: &[u8]) {
            let _ = self.0.try_send(IoEvent::Output(data.to_vec()));
        }
        fn resize(&self, _: GridSize) {}
    }

    /// 微秒数的中位数、p90、p99。
    fn percentiles(samples: &mut [f64]) -> String {
        samples.sort_by(|a, b| a.total_cmp(b));
        let at = |p: f64| samples[((samples.len() - 1) as f64 * p).round() as usize];
        format!(
            "中位数 {:.1} µs，p90 {:.1} µs，p99 {:.1} µs",
            at(0.5),
            at(0.9),
            at(0.99)
        )
    }

    /// 延迟：`cargo test -p termlet-terminal --release --lib latency -- --ignored --nocapture`
    ///
    /// - 输出到画面：一段输出送进传输的通道，到界面收到重绘通知并拿到含它的快照（200×50，满屏日志）；
    /// - 按键回显：界面处理一次按键（回到底部、记下输入、写给传输）的耗时，以及到回显出现在快照中的总耗时；
    /// - 改了状态马上读：发一条滚动的命令再读可见区的位置（界面要等引擎线程执行完它，一次线程往返）。
    #[test]
    #[ignore = "延迟测量，手动运行"]
    fn latency() {
        let (tx, rx) = async_channel::bounded(64);
        let engine = TerminalEngine::new(
            TerminalIo {
                output: rx,
                input: Arc::new(Echo(tx.clone())),
            },
            EngineOptions {
                scrollback: 10_000,
                size: GridSize::new(200, 50),
            },
        );
        let events = engine.events();
        let mut snapshot = GridSnapshot::default();
        let line = "\x1b[32m2026-10-15 12:00:00\x1b[0m INFO  服务启动完成 request_id=0123456789abcdef path=/api/v1/hosts status=200\r\n";
        // 等这次输出的重绘通知，取快照
        let frame = |snapshot: &mut GridSnapshot| {
            loop {
                match events.recv_blocking() {
                    Ok(EngineEvent::Wakeup) => break,
                    Ok(_) => {}
                    Err(_) => panic!("引擎已关闭"),
                }
            }
            engine.clear_dirty();
            engine.snapshot(snapshot, 0);
        };
        tx.send_blocking(IoEvent::Output(line.repeat(100).into_bytes()))
            .unwrap();
        frame(&mut snapshot);

        let rounds = 2000;
        let mut output = Vec::with_capacity(rounds);
        for i in 0..rounds {
            let started = Instant::now();
            tx.send_blocking(IoEvent::Output(format!("{i} {line}").into_bytes()))
                .unwrap();
            frame(&mut snapshot);
            output.push(started.elapsed().as_secs_f64() * 1e6);
        }
        let mut key = Vec::with_capacity(rounds);
        let mut echo = Vec::with_capacity(rounds);
        for i in 0..rounds {
            let data: &[u8] = if i % 100 == 99 { b"\r\n" } else { b"a" };
            let started = Instant::now();
            // 与视图的 write_input 相同
            engine.user_input(data);
            engine.write(data);
            key.push(started.elapsed().as_secs_f64() * 1e6);
            frame(&mut snapshot);
            echo.push(started.elapsed().as_secs_f64() * 1e6);
        }
        println!("输出到画面：{}", percentiles(&mut output));
        println!("按键（界面线程上）：{}", percentiles(&mut key));
        println!("按键到回显出现在快照中：{}", percentiles(&mut echo));
        // 界面自己改了状态之后马上读（滚动后读可见区的位置）：等引擎线程执行完这条命令
        let mut own = Vec::with_capacity(rounds);
        for _ in 0..rounds {
            // 像界面上的操作一样隔开（引擎线程已经睡下，要叫醒它）
            std::thread::sleep(Duration::from_millis(1));
            let started = Instant::now();
            engine.scroll_display(Scroll::Delta(0));
            std::hint::black_box(engine.dimensions());
            own.push(started.elapsed().as_secs_f64() * 1e6);
        }
        println!("改了状态马上读：{}", percentiles(&mut own));
    }
}
