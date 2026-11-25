//! 内核的接口：解析远端的输出，保存网格、回滚、光标与终端模式，回答程序的提问，管理选区。引擎（[`super`]）只经这个
//! 接口使用内核；现在由 alacritty_terminal 实现（[`super::alacritty`]），以后可以换成 libghostty-vt。
//!
//! 哪些属于内核、哪些在内核之外：
//!
//! - 内核：VT 解析；网格、回滚、光标、终端模式；程序改过的颜色（OSC 4 / 10 / 11 / 12）；标题与响铃；终端的应答
//!   （设备属性、光标位置、窗口大小、颜色查询）；同步更新（DEC 2026）的缓冲与超时；选区与选中的文字；超链接（OSC 8）；
//!   跟着滚动的格子标记；kitty 键盘协议的标志栈；深浅色切换通知（DEC 2031）的开关与查询；程序写剪贴板（OSC 52，
//!   只写不读，见 [`crate::clipboard`]）。其中 alacritty 没有或做得不对的由 alacritty 后端自己补：格子标记的滚动计数、kitty 键盘协议的标志栈
//!   （[`crate::kitty_keyboard`]）、2031（[`crate::theme_mode`]）。
//! - 内核之外（引擎与视图，换内核时不变）：shell 集成（OSC 133 / 633 / 7 / 9 / 777 / 1337）的扫描与命令历史、
//!   查找、关键词高亮、网址识别、文本提取的逻辑行、按键与鼠标的编码、配色的解析与对比度、绘制、
//!   会话日志等输出旁路。
//! - 扫描 shell 集成标记的 [`crate::shell_events::OscScanner`] 顺带找出 kitty 键盘协议的请求、RIS 与 2031 的请求，
//!   引擎原样转给内核（[`Kernel::keyboard_request`]、[`Kernel::theme_request`]），省得 alacritty 后端为它们再扫一遍；
//!   换成自己解析这些序列的内核后，后端忽略这两个调用即可。
//!
//! 线程：内核只在引擎线程上创建与使用（见 `engine/worker.rs`），界面不碰它：绘制读引擎线程填好的 [`GridSnapshot`]，
//! 其余调用作为命令发给引擎线程执行。所以内核不需要 `Send`、`Sync`（libghostty-vt 的 Rust 类型都是 `!Send`），
//! 测试构建中 alacritty 后端特意做成 `!Send`，保证不会退回需要跨线程的写法。内核的方法都不阻塞、不调用界面；
//! 它发给引擎的通知（[`KernelEvents`]）是线程安全的。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use parking_lot::Mutex;
use termlet_io::GridSize;

use super::EngineEvent;
use crate::colors::Palette;
use crate::dump::HiddenState;
use crate::grid::{
    Cell, CellExtra, CursorShape, GridDimensions, GridMark, GridPoint, GridSnapshot, GridText,
    Scroll, ScrollPosition, SelectionKind, Side, TerminalModes,
};
use crate::kitty_keyboard::KeyboardRequest;
use crate::theme_mode::ThemeRequest;

/// 终端内核。行号、坐标的约定见 [`crate::grid`]。读文字的方法见 [`GridText`]（范围是整个回滚与屏幕）。
pub(crate) trait Kernel: GridText {
    // ------------------------------------------------------------ 解析

    /// 解析一段远端的输出。内核在解析前后记下主屏幕的内容上移了多少行（[`Self::mark_cursor`] 的标记据此跟着滚动）。
    fn advance(&mut self, bytes: &[u8]);

    /// 同步更新（DEC 2026）进行中：到这个时间还没有收到结束标记就提前显示（[`Self::stop_sync`]）。
    fn sync_deadline(&self) -> Option<Instant>;

    /// 同步更新中缓冲着、还没有解析进网格的字节数。
    fn sync_buffered(&self) -> usize;

    /// 结束同步更新（超时、传输结束）：解析缓冲着的部分。
    fn stop_sync(&mut self);

    /// 读取网格之前先解析同步更新中缓冲着的部分，再重新开始同步更新（画面仍然等结束标记或超时一起显示）；
    /// 返回提前解析了的字节数（没有在同步更新中时为 0）。
    fn sync_now(&mut self) -> usize;

    /// 换上了新的传输：丢掉解析到一半的序列与同步更新缓冲着的字节，关闭 kitty 键盘协议与深浅色切换通知
    /// （新的 shell 还没有开启它们）。网格、回滚、选区保留。
    fn new_session(&mut self);

    /// 显示一段本地生成的文字（含控制序列，例如重连的提示）：不是远端的输出，其中的提问不回答。
    fn inject(&mut self, bytes: &[u8]);

    /// 取走解析中产生的、要写回远端的应答（按产生的顺序拼在一起）。
    fn take_replies(&mut self) -> Option<String>;

    /// kitty 键盘协议的请求或 RIS（扫描器在序列之后报告）：按当时所在的屏幕更新标志栈，查询的回答排进应答。
    fn keyboard_request(&mut self, request: KeyboardRequest);

    /// 深浅色切换通知（DEC 2031）的请求：更新开关，查询的回答排进应答。
    fn theme_request(&mut self, request: ThemeRequest);

    // ------------------------------------------------------------ 设置

    /// 回答颜色查询、判断深浅用的配色（程序改过的颜色优先）；配色的深浅变了且程序开启了深浅色切换通知时，
    /// 返回要发给远端的通知。
    fn set_palette(&mut self, palette: &Palette) -> Option<String>;

    /// 改变网格的大小（之前的格子标记失效）。
    fn resize(&mut self, size: GridSize);

    /// 回滚的最大行数。
    fn scrollback(&self) -> usize;

    /// 改变回滚的最大行数，返回是否有变化：变小时截掉最早的历史，显示位置随之收进剩下的回滚中。
    fn set_scrollback(&mut self, lines: usize) -> bool;

    /// 改变光标的默认样式，返回是否有变化（程序用 DECSCUSR 设置过的照样优先）。
    fn set_default_cursor(&mut self, shape: CursorShape, blinking: bool) -> bool;

    // ------------------------------------------------------------ 状态

    fn modes(&self) -> TerminalModes;

    fn dimensions(&self) -> GridDimensions;

    /// 光标所在的格子（网格中的位置，不按宽字符调整）。
    fn cursor_point(&self) -> GridPoint;

    /// 现在的光标形状：程序用 DECSCUSR 设置的优先，否则是默认的。
    fn cursor_shape(&self) -> CursorShape;

    /// 现在的光标是否闪烁：程序用 DECSCUSR、DECSET 12 设置的优先，否则按默认样式。
    fn cursor_blinking(&self) -> bool;

    /// 程序是否开启了深浅色切换通知（DEC 2031）。
    fn theme_reports_enabled(&self) -> bool;

    /// 格子上的超链接（OSC 8）的地址。
    fn hyperlink_at(&self, point: GridPoint) -> Option<String>;

    /// 把可见区填进 `snapshot`（复用它的缓冲）。`context` 是为补全跨出可见区的逻辑行最多多取的行数：
    /// 可见区的第一行是上一行的延续时向上多取，最后一行自动换行时向下多取，各最多 `context` 行。
    fn snapshot(&self, snapshot: &mut GridSnapshot, context: usize);

    // ------------------------------------------------------------ 可见区与选区

    fn scroll_display(&mut self, scroll: Scroll);

    /// 格子不在可见区中时滚动到能看见它。
    fn scroll_to(&mut self, point: GridPoint);

    /// 清空回滚与屏幕，光标所在的行成为第一行；格子标记跟着上移。
    fn clear_buffer(&mut self);

    fn start_selection(&mut self, kind: SelectionKind, point: GridPoint, side: Side);

    /// 把选区的另一端移到这里；没有选区时返回 `false`。
    fn update_selection(&mut self, point: GridPoint, side: Side) -> bool;

    /// 选中全部（含回滚）。
    fn select_all(&mut self);

    fn clear_selection(&mut self);

    /// 没有选区，或选区是空的（只是点了一下）。
    fn selection_is_empty(&self) -> bool;

    /// 选中的文字。
    fn selection_text(&self) -> Option<String>;

    // ------------------------------------------------------------ 格子标记

    /// 给光标所在的格子做标记；在备用屏幕中时为 `None`（只在主屏幕上使用标记）。
    fn mark_cursor(&self) -> Option<GridMark>;

    /// 标记现在的位置；已经失效、滚出回滚、在备用屏幕中时为 `None`。
    fn resolve(&self, mark: &GridMark) -> Option<GridPoint>;

    /// 主屏幕滚动计数的读数；在备用屏幕中时为 `None`。
    fn scroll_position(&self) -> Option<ScrollPosition>;

    // ------------------------------------------------------------ 关键帧（回放往前跳，见 `crate::dump::checkpoint`）

    /// 把 `top..=bottom` 行（行号见 [`crate::grid`]，可含回滚）的格子填进 `snapshot`（复用它的缓冲），与显示位置无关；
    /// 只填格子、自动换行、组合字符与下划线颜色、超链接（[`GridSnapshot::extra`]）。
    fn snapshot_lines(&self, snapshot: &mut GridSnapshot, top: i32, bottom: i32);

    /// 光标处之后写的字的样式（SGR 设的颜色与属性；格子的 `c` 不用），以及下划线的颜色与开着的超链接。
    fn cursor_pen(&self) -> (Cell, CellExtra);

    /// 画面之外、接着解析要用到的状态（滚动区域、存下的光标、字符集、制表位、模式等）。可以临时改动内核来读出
    /// 内核不直接给出的部分，返回前原样恢复。
    fn hidden_state(&mut self) -> HiddenState;

    /// 在备用屏幕中时临时换到主屏幕调用 `f`（读主屏幕的内容与光标），之后原样换回；不在备用屏幕中时不调用。
    fn with_primary_screen(&mut self, f: &mut dyn FnMut(&Self));
}

/// 合并发送的通知：界面取走之前不再发新的，大量 BEL、频繁改标题时事件不会在通道里堆积。
#[derive(Default)]
pub(crate) struct Notices {
    pub(crate) bell: AtomicBool,
    /// 最新的标题，界面还没取走；里面的 `None` 表示恢复默认标题
    pub(crate) title: Mutex<Option<Option<String>>>,
    /// 光标的形状或闪烁变了：引擎线程发布了新的状态之后再通知界面（界面收到时读到的是新的）
    pub(crate) cursor: AtomicBool,
}

/// 内核发给引擎的通知（标题、响铃、光标闪烁、写剪贴板），以及内核回答窗口大小的查询要用的大小。
#[derive(Clone)]
pub(crate) struct KernelEvents {
    events: async_channel::Sender<EngineEvent>,
    size: Arc<Mutex<GridSize>>,
    notices: Arc<Notices>,
}

impl KernelEvents {
    pub(crate) fn new(
        events: async_channel::Sender<EngineEvent>,
        size: Arc<Mutex<GridSize>>,
        notices: Arc<Notices>,
    ) -> Self {
        Self {
            events,
            size,
            notices,
        }
    }

    /// 不接界面的（测试中单独使用内核）。
    #[cfg(test)]
    pub(crate) fn detached(size: GridSize) -> Self {
        let (events, _) = async_channel::unbounded();
        Self::new(events, Arc::new(Mutex::new(size)), Arc::default())
    }

    /// 窗口标题（OSC 0 / 2）变了；`None` 表示恢复默认标题。
    pub(crate) fn title(&self, title: Option<String>) {
        let first = self.notices.title.lock().replace(title).is_none();
        if first {
            let _ = self.events.try_send(EngineEvent::Title);
        }
    }

    pub(crate) fn bell(&self) {
        if !self.notices.bell.swap(true, Ordering::AcqRel) {
            let _ = self.events.try_send(EngineEvent::Bell);
        }
    }

    /// 程序要写剪贴板（OSC 52，`text` 已解码）：逐次发给界面，空内容不发，太大的只发大小。
    pub(crate) fn clipboard(&self, text: String, primary: bool) {
        if let Some(request) = crate::ClipboardRequest::from_store(text, primary) {
            let _ = self.events.try_send(EngineEvent::Clipboard(request));
        }
    }

    /// 光标的形状或闪烁变了（DECSCUSR、DECSET 12、RIS）：记下，引擎线程发布状态后再发
    /// [`EngineEvent::CursorBlinkingChange`]。
    pub(crate) fn cursor_blinking_changed(&self) {
        self.notices.cursor.store(true, Ordering::Release);
    }

    /// 现在的大小（格子数与像素）。
    pub(crate) fn size(&self) -> GridSize {
        *self.size.lock()
    }
}
