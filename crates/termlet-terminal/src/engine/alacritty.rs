//! alacritty_terminal 后端：用 alacritty 实现内核接口（[`Kernel`]）。alacritty 的类型只在这个模块（及 [`marks`]）
//! 中出现，换内核时整个换掉。
//!
//! - 解析：vte 的 `Processor` 交给 `Term`。程序开启同步更新（DEC 2026）时 vte 先缓冲输出，等结束标记或 150 毫秒
//!   超时后再一起解析。每段输出解析前后量一次内容滚动了多少行（[`marks`]）。
//! - 应答：`Term` 发出的应答（设备属性、光标位置、窗口大小、颜色查询）与这里补的 kitty 键盘协议、深浅色切换通知的
//!   回答排在同一个队列里，引擎每解析完一段就取走写出，回答的顺序与提问相同（delta、bat 先问颜色、再用 DA1 判断
//!   终端支不支持颜色查询）。颜色查询按程序用 OSC 4 / 10 / 11 改过的颜色、没改过的按界面的配色回答。
//! - alacritty 没有或做得不对、由这里补的：格子标记的滚动计数（[`marks`]）；kitty 键盘协议的标志栈
//!   （[`crate::kitty_keyboard`]，alacritty 自带的查询回答不对、栈满时 panic，没有开启它）；深浅色切换通知
//!   （[`crate::theme_mode`]，alacritty 对 2031 的 DECRQM 回答“不支持”，这条回答丢掉）。
//! - 程序读写剪贴板（OSC 52）在配置中关闭：远端输出可以被注入，不应静默改掉本机的剪贴板。
//! - 快照：逐行读 alacritty 的格子转换成 [`Cell`]。属性的位与 alacritty 的位置相同（编译时检查），转换只是屏蔽掉
//!   不要的位；组合字符、超链接只在少数格子上有。

mod marks;

use std::ops::{Range, RangeInclusive};
use std::sync::Arc;
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Charsets, Dimensions, Row, Scroll as AlacrittyScroll};
use alacritty_terminal::index::{Column, Line, Point, Side as AlacrittySide};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::{Cell as AlacrittyCell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{Config, Osc52, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    CharsetIndex, ClearMode, Color as AnsiColor, CursorShape as AnsiCursorShape, CursorStyle,
    Handler, NamedColor, Processor, Rgb as AnsiRgb, StandardCharset, StdSyncHandler,
};
use parking_lot::Mutex;
use termlet_io::GridSize;

use self::marks::Marks;
use super::kernel::{Kernel, KernelEvents};
use crate::colors::{Color, ColorOverrides, ColorSlot, Palette, Rgb};
use crate::dump::{HiddenState, SavedCursor};
use crate::grid::{
    Cell, CellExtra, CellFlags, Cursor, CursorShape, GridDimensions, GridMark, GridPoint,
    GridSnapshot, GridText, Hyperlink, MouseEncoding, MouseTracking, Scroll, ScrollPosition,
    SelectionKind, SelectionRange, Side, TerminalModes,
};
use crate::kitty_keyboard::{KeyboardModes, KeyboardRequest};
use crate::theme_mode::{Theme, ThemeReports, ThemeRequest};

/// alacritty 回答 DECRQM 时对 2031 的“不支持”：丢掉它，由 [`ThemeRequest::ModeQuery`] 回答。
const UNSUPPORTED_THEME_REPLY: &str = "\x1b[?2031;0$y";

/// 开始同步更新（DEC 2026）的序列：为读取网格提前结束同步更新后，用它重新开始。
const BEGIN_SYNC: &[u8] = b"\x1b[?2026h";

/// 要写回远端的应答，按产生的顺序排队。
enum Reply {
    Text(String),
    /// 颜色查询：颜色的编号（0–255 与 [`NamedColor`]）与回答的格式
    Color(usize, Arc<dyn Fn(AnsiRgb) -> String + Send + Sync>),
}

type SharedReplies = Arc<Mutex<Vec<Reply>>>;

/// 把 alacritty 的事件转给引擎，应答排进队列。
#[derive(Clone)]
struct EventProxy {
    replies: SharedReplies,
    events: KernelEvents,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        match event {
            // alacritty 对 2031 回答“不支持”：由 `theme_mode` 回答
            Event::PtyWrite(text) if text == UNSUPPORTED_THEME_REPLY => {}
            Event::PtyWrite(text) => self.replies.lock().push(Reply::Text(text)),
            Event::TextAreaSizeRequest(format) => {
                let size = self.events.size();
                let window = WindowSize {
                    num_lines: size.rows,
                    num_cols: size.columns,
                    cell_width: size.cell_width,
                    cell_height: size.cell_height,
                };
                self.replies.lock().push(Reply::Text(format(window)));
            }
            Event::ColorRequest(index, format) => {
                self.replies.lock().push(Reply::Color(index, format));
            }
            Event::Title(title) => self.events.title(Some(title)),
            Event::ResetTitle => self.events.title(None),
            Event::Bell => self.events.bell(),
            Event::CursorBlinkingChange => self.events.cursor_blinking_changed(),
            // 重绘由引擎线程统一通知；剪贴板（OSC 52）已在配置中关闭；进程退出由传输通知
            Event::Wakeup
            | Event::MouseCursorDirty
            | Event::ClipboardStore(..)
            | Event::ClipboardLoad(..)
            | Event::Exit
            | Event::ChildExit(_) => {}
        }
    }
}

/// alacritty 需要的尺寸。
#[derive(Clone, Copy, Debug)]
struct TermDimensions {
    columns: usize,
    lines: usize,
}

impl From<GridSize> for TermDimensions {
    fn from(size: GridSize) -> Self {
        Self {
            columns: size.columns.max(2) as usize,
            lines: size.rows.max(1) as usize,
        }
    }
}

impl Dimensions for TermDimensions {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.columns
    }
}

/// 终端的配置：回滚行数、光标的默认样式；不允许程序读写剪贴板（OSC 52）。
fn term_config(scrollback: usize, cursor: CursorStyle) -> Config {
    Config {
        scrolling_history: scrollback,
        default_cursor_style: cursor,
        osc52: Osc52::Disabled,
        ..Config::default()
    }
}

fn cursor_style(shape: CursorShape, blinking: bool) -> CursorStyle {
    let shape = match shape {
        CursorShape::Block => AnsiCursorShape::Block,
        CursorShape::Bar => AnsiCursorShape::Beam,
        CursorShape::Underline => AnsiCursorShape::Underline,
    };
    CursorStyle { shape, blinking }
}

/// 格子属性中与 alacritty 位置相同的位：转换时只需屏蔽掉其余的（alacritty 的 WRAPLINE 等）。
const SHARED_FLAGS: u16 = CellFlags::INVERSE.bits()
    | CellFlags::BOLD.bits()
    | CellFlags::ITALIC.bits()
    | CellFlags::UNDERLINE.bits()
    | CellFlags::WIDE.bits()
    | CellFlags::WIDE_SPACER.bits()
    | CellFlags::DIM.bits()
    | CellFlags::HIDDEN.bits()
    | CellFlags::STRIKEOUT.bits()
    | CellFlags::LEADING_WIDE_SPACER.bits()
    | CellFlags::DOUBLE_UNDERLINE.bits()
    | CellFlags::UNDERCURL.bits()
    | CellFlags::DOTTED_UNDERLINE.bits()
    | CellFlags::DASHED_UNDERLINE.bits();

const _: () = {
    assert!(Flags::INVERSE.bits() == CellFlags::INVERSE.bits());
    assert!(Flags::BOLD.bits() == CellFlags::BOLD.bits());
    assert!(Flags::ITALIC.bits() == CellFlags::ITALIC.bits());
    assert!(Flags::UNDERLINE.bits() == CellFlags::UNDERLINE.bits());
    assert!(Flags::WIDE_CHAR.bits() == CellFlags::WIDE.bits());
    assert!(Flags::WIDE_CHAR_SPACER.bits() == CellFlags::WIDE_SPACER.bits());
    assert!(Flags::DIM.bits() == CellFlags::DIM.bits());
    assert!(Flags::HIDDEN.bits() == CellFlags::HIDDEN.bits());
    assert!(Flags::STRIKEOUT.bits() == CellFlags::STRIKEOUT.bits());
    assert!(Flags::LEADING_WIDE_CHAR_SPACER.bits() == CellFlags::LEADING_WIDE_SPACER.bits());
    assert!(Flags::DOUBLE_UNDERLINE.bits() == CellFlags::DOUBLE_UNDERLINE.bits());
    assert!(Flags::UNDERCURL.bits() == CellFlags::UNDERCURL.bits());
    assert!(Flags::DOTTED_UNDERLINE.bits() == CellFlags::DOTTED_UNDERLINE.bits());
    assert!(Flags::DASHED_UNDERLINE.bits() == CellFlags::DASHED_UNDERLINE.bits());
    // 其余的位 alacritty 另有用途，转换时去掉
    assert!(Flags::WRAPLINE.bits() & SHARED_FLAGS == 0);
};

#[inline]
fn to_cell(cell: &AlacrittyCell) -> Cell {
    let mut flags = CellFlags::from_bits_retain(cell.flags.bits() & SHARED_FLAGS);
    if cell.extra.is_some() {
        if cell.zerowidth().is_some_and(|chars| !chars.is_empty()) {
            flags |= CellFlags::GRAPHEME;
        }
        if cell.hyperlink().is_some() {
            flags |= CellFlags::HYPERLINK;
        }
    }
    Cell {
        c: cell.c,
        fg: to_color(cell.fg),
        bg: to_color(cell.bg),
        flags,
    }
}

/// 格子的下划线颜色与超链接（关键帧要写出）；都没有时为 `None`。
fn to_extra(cell: &AlacrittyCell) -> Option<CellExtra> {
    cell.extra.as_ref()?;
    let extra = CellExtra {
        underline_color: cell.underline_color().map(to_color),
        hyperlink: cell.hyperlink().map(|link| Hyperlink {
            id: link.id().to_string(),
            uri: link.uri().to_string(),
        }),
    };
    (!extra.is_empty()).then_some(extra)
}

#[inline]
fn to_color(color: AnsiColor) -> Color {
    match color {
        AnsiColor::Named(named) => match named as usize {
            index @ 0..16 => Color::Indexed(index as u8),
            index if index == NamedColor::Background as usize => Color::Background,
            // 默认前景色。光标色、暗色等是 alacritty 绘制时用的，不会出现在格子里
            _ => Color::Foreground,
        },
        AnsiColor::Indexed(index) => Color::Indexed(index),
        AnsiColor::Spec(rgb) => Color::Rgb(to_rgb(rgb)),
    }
}

fn to_rgb(rgb: AnsiRgb) -> Rgb {
    Rgb {
        r: rgb.r,
        g: rgb.g,
        b: rgb.b,
    }
}

fn to_point(point: Point) -> GridPoint {
    GridPoint::new(point.line.0, point.column.0)
}

fn from_point(point: GridPoint) -> Point {
    Point::new(Line(point.line), Column(point.column))
}

fn from_side(side: Side) -> AlacrittySide {
    match side {
        Side::Left => AlacrittySide::Left,
        Side::Right => AlacrittySide::Right,
    }
}

/// 颜色查询的编号对应的颜色。
fn color_slot(index: usize) -> ColorSlot {
    match index {
        0..=255 => ColorSlot::Indexed(index as u8),
        i if i == NamedColor::Background as usize => ColorSlot::Background,
        i if i == NamedColor::Cursor as usize => ColorSlot::Cursor,
        _ => ColorSlot::Foreground,
    }
}

/// 程序用 OSC 4 / 10 / 11 / 12 改过的颜色。
fn fill_overrides(overrides: &mut ColorOverrides, colors: &Colors) {
    for (index, slot) in overrides.indexed.iter_mut().enumerate() {
        *slot = colors[index].map(to_rgb);
    }
    overrides.foreground = colors[NamedColor::Foreground].map(to_rgb);
    overrides.background = colors[NamedColor::Background].map(to_rgb);
    overrides.cursor = colors[NamedColor::Cursor].map(to_rgb);
}

/// 用 alacritty_terminal 实现的内核。
pub(crate) struct AlacrittyKernel {
    term: Term<EventProxy>,
    /// 当前传输的解析状态（换上新的传输时重新开始）
    processor: Processor<StdSyncHandler>,
    marks: Marks,
    replies: SharedReplies,
    keyboard: KeyboardModes,
    theme: ThemeReports,
    /// 回答颜色查询用的配色
    palette: Palette,
    /// 光标的默认样式（设置中的，改回滚行数时要带上）
    default_cursor: CursorStyle,
    /// 测试构建中让内核 `!Send`、`!Sync`（与 libghostty-vt 的类型一样）：引擎只在自己的线程上创建与使用内核，
    /// 有哪里退回到跨线程使用它时测试就编译不过
    #[cfg(test)]
    _not_send: std::marker::PhantomData<*const ()>,
}

impl AlacrittyKernel {
    pub(crate) fn new(scrollback: usize, size: GridSize, events: KernelEvents) -> Self {
        let replies = SharedReplies::default();
        let proxy = EventProxy {
            replies: replies.clone(),
            events,
        };
        let default_cursor = CursorStyle::default();
        let term = Term::new(
            term_config(scrollback, default_cursor),
            &TermDimensions::from(size),
            proxy,
        );
        Self {
            term,
            processor: Processor::new(),
            marks: Marks::new(scrollback),
            replies,
            keyboard: KeyboardModes::default(),
            theme: ThemeReports::default(),
            palette: Palette::default_dark(),
            default_cursor,
            #[cfg(test)]
            _not_send: std::marker::PhantomData,
        }
    }

    /// 在滚动计数的前后之间修改终端。
    fn tracked(&mut self, f: impl FnOnce(&mut Processor<StdSyncHandler>, &mut Term<EventProxy>)) {
        self.marks.begin(&mut self.term);
        f(&mut self.processor, &mut self.term);
        self.marks.end(&mut self.term);
    }

    fn in_grid(&self, point: GridPoint) -> bool {
        let grid = self.term.grid();
        point.line >= -(grid.history_size() as i32)
            && point.line < grid.screen_lines() as i32
            && point.column < grid.columns()
    }

    /// 测试用：不接界面的内核。
    #[cfg(test)]
    pub(crate) fn for_test(columns: usize, lines: usize, scrollback: usize) -> Self {
        let size = GridSize::new(columns as u16, lines as u16);
        Self::new(scrollback, size, KernelEvents::detached(size))
    }

    /// 测试用：按内容直接填好网格。每行一段，`\n` 结束的行自动换行到下一行，`\r\n` 是真正的换行；
    /// 列数是最长的一行。
    #[cfg(test)]
    pub(crate) fn mock(content: &str) -> Self {
        use unicode_width::UnicodeWidthChar;

        let width = |c: char| c.width().unwrap_or(1);
        let lines: Vec<&str> = content.split('\n').collect();
        let columns = lines
            .iter()
            .map(|line| {
                line.chars()
                    .filter(|&c| c != '\r')
                    .map(width)
                    .sum::<usize>()
            })
            .max()
            .unwrap_or(0);
        let mut kernel = Self::for_test(columns, lines.len(), 100);
        let grid = kernel.term.grid_mut();
        for (index, text) in lines.iter().enumerate() {
            let line = Line(index as i32);
            if !text.ends_with('\r') && index + 1 != lines.len() {
                grid[line][Column(columns - 1)]
                    .flags
                    .insert(Flags::WRAPLINE);
            }
            let mut column = 0;
            for c in text.chars().take_while(|&c| c != '\r') {
                grid[line][Column(column)].c = c;
                if width(c) == 2 {
                    grid[line][Column(column)].flags.insert(Flags::WIDE_CHAR);
                    grid[line][Column(column + 1)]
                        .flags
                        .insert(Flags::WIDE_CHAR_SPACER);
                }
                column += width(c);
            }
        }
        kernel
    }
}

impl GridText for AlacrittyKernel {
    fn columns(&self) -> usize {
        self.term.grid().columns()
    }

    fn lines(&self) -> RangeInclusive<i32> {
        let grid = self.term.grid();
        -(grid.history_size() as i32)..=grid.screen_lines() as i32 - 1
    }

    fn wraps(&self, line: i32) -> bool {
        let grid = self.term.grid();
        let columns = grid.columns();
        columns > 0
            && line >= -(grid.history_size() as i32)
            && line < grid.screen_lines() as i32
            && grid[Line(line)][Column(columns - 1)]
                .flags
                .contains(Flags::WRAPLINE)
    }

    fn push_text(
        &self,
        line: i32,
        columns: Range<usize>,
        text: &mut String,
        mut cells: Option<&mut Vec<(usize, GridPoint)>>,
    ) {
        let grid = self.term.grid();
        let end = columns.end.min(grid.columns());
        if columns.start >= end {
            return;
        }
        let row = &grid[Line(line)];
        for (offset, cell) in row[Column(columns.start)..Column(end)].iter().enumerate() {
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            if let Some(cells) = cells.as_deref_mut() {
                cells.push((text.len(), GridPoint::new(line, columns.start + offset)));
            }
            text.push(cell.c);
            if let Some(zerowidth) = cell.zerowidth() {
                text.extend(zerowidth);
            }
        }
    }
}

impl Kernel for AlacrittyKernel {
    fn advance(&mut self, bytes: &[u8]) {
        self.tracked(|processor, term| processor.advance(term, bytes));
    }

    fn sync_deadline(&self) -> Option<Instant> {
        self.processor.sync_timeout().sync_timeout()
    }

    fn sync_buffered(&self) -> usize {
        self.processor.sync_bytes_count()
    }

    fn stop_sync(&mut self) {
        self.tracked(|processor, term| processor.stop_sync(term));
    }

    fn sync_now(&mut self) -> usize {
        let buffered = self.processor.sync_bytes_count();
        if buffered == 0 {
            return 0;
        }
        self.stop_sync();
        // 标记处 vte 在 OSC 之中（ESC 结束的标记切在 ESC 之前）或已回到初始状态，这里的 ESC 正常结束它
        self.processor.advance(&mut self.term, BEGIN_SYNC);
        buffered
    }

    fn new_session(&mut self) {
        self.processor = Processor::new();
        self.keyboard.clear();
        self.theme.reset();
    }

    fn inject(&mut self, bytes: &[u8]) {
        let mut processor: Processor<StdSyncHandler> = Processor::new();
        self.marks.begin(&mut self.term);
        processor.advance(&mut self.term, bytes);
        self.marks.end(&mut self.term);
        // 本地的文字不是远端的提问，不回答
        self.replies.lock().clear();
    }

    fn take_replies(&mut self) -> Option<String> {
        let replies = std::mem::take(&mut *self.replies.lock());
        if replies.is_empty() {
            return None;
        }
        let mut out = String::new();
        let mut overrides: Option<ColorOverrides> = None;
        for reply in replies {
            match reply {
                Reply::Text(text) => out.push_str(&text),
                Reply::Color(index, format) => {
                    let overrides = overrides.get_or_insert_with(|| {
                        let mut overrides = ColorOverrides::default();
                        fill_overrides(&mut overrides, self.term.colors());
                        overrides
                    });
                    let rgb = self.palette.query(color_slot(index), overrides);
                    out.push_str(&format(AnsiRgb {
                        r: rgb.r,
                        g: rgb.g,
                        b: rgb.b,
                    }));
                }
            }
        }
        Some(out)
    }

    fn keyboard_request(&mut self, request: KeyboardRequest) {
        let alt = self.term.mode().contains(TermMode::ALT_SCREEN);
        let reply = self.keyboard.apply(alt, request);
        // RIS 同样关闭深浅色切换通知
        if request == KeyboardRequest::Reset {
            self.theme.reset();
        }
        if let Some(reply) = reply {
            self.replies.lock().push(Reply::Text(reply));
        }
    }

    fn theme_request(&mut self, request: ThemeRequest) {
        if let Some(reply) = self.theme.apply(request) {
            self.replies.lock().push(Reply::Text(reply));
        }
    }

    fn set_palette(&mut self, palette: &Palette) -> Option<String> {
        if self.palette != *palette {
            self.palette = palette.clone();
        }
        self.theme
            .set_theme(Theme::of_background(palette.background))
    }

    fn resize(&mut self, size: GridSize) {
        self.term.resize(TermDimensions::from(size));
        // 重排后之前记下的位置都不再可靠（shell 收到大小变化后通常会重画提示符，重新给出 B）
        self.marks.invalidate();
    }

    fn scrollback(&self) -> usize {
        self.marks.max_history()
    }

    fn set_scrollback(&mut self, lines: usize) -> bool {
        if self.marks.max_history() == lines {
            return false;
        }
        self.term
            .set_options(term_config(lines, self.default_cursor));
        self.marks.set_max_history(lines);
        true
    }

    fn set_default_cursor(&mut self, shape: CursorShape, blinking: bool) -> bool {
        let style = cursor_style(shape, blinking);
        if self.default_cursor == style {
            return false;
        }
        self.default_cursor = style;
        self.term
            .set_options(term_config(self.marks.max_history(), style));
        true
    }

    fn modes(&self) -> TerminalModes {
        let mode = *self.term.mode();
        let alt_screen = mode.contains(TermMode::ALT_SCREEN);
        let mouse_tracking = if mode.contains(TermMode::MOUSE_MOTION) {
            MouseTracking::Motion
        } else if mode.contains(TermMode::MOUSE_DRAG) {
            MouseTracking::Drag
        } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
            MouseTracking::Click
        } else {
            MouseTracking::Off
        };
        let mouse_encoding = if mode.contains(TermMode::SGR_MOUSE) {
            MouseEncoding::Sgr
        } else if mode.contains(TermMode::UTF8_MOUSE) {
            MouseEncoding::Utf8
        } else {
            MouseEncoding::Default
        };
        TerminalModes {
            alt_screen,
            show_cursor: mode.contains(TermMode::SHOW_CURSOR),
            app_cursor: mode.contains(TermMode::APP_CURSOR),
            bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
            line_feed_new_line: mode.contains(TermMode::LINE_FEED_NEW_LINE),
            alternate_scroll: mode.contains(TermMode::ALTERNATE_SCROLL),
            mouse_tracking,
            mouse_encoding,
            keyboard: self.keyboard.flags(alt_screen),
        }
    }

    fn dimensions(&self) -> GridDimensions {
        let grid = self.term.grid();
        GridDimensions {
            columns: grid.columns(),
            screen_lines: grid.screen_lines(),
            history_size: grid.history_size(),
            display_offset: grid.display_offset(),
        }
    }

    fn cursor_point(&self) -> GridPoint {
        to_point(self.term.grid().cursor.point)
    }

    fn cursor_shape(&self) -> CursorShape {
        match self.term.cursor_style().shape {
            AnsiCursorShape::Beam => CursorShape::Bar,
            AnsiCursorShape::Underline => CursorShape::Underline,
            // 程序用 DECSCUSR 只能设成方块、下划线、竖线；空心与隐藏是 alacritty 绘制时用的
            AnsiCursorShape::Block | AnsiCursorShape::HollowBlock | AnsiCursorShape::Hidden => {
                CursorShape::Block
            }
        }
    }

    fn cursor_blinking(&self) -> bool {
        self.term.cursor_style().blinking
    }

    fn theme_reports_enabled(&self) -> bool {
        self.theme.enabled()
    }

    fn hyperlink_at(&self, point: GridPoint) -> Option<String> {
        if !self.in_grid(point) {
            return None;
        }
        self.term.grid()[from_point(point)]
            .hyperlink()
            .map(|link| link.uri().to_string())
    }

    fn snapshot(&self, snapshot: &mut GridSnapshot, context: usize) {
        let grid = self.term.grid();
        let columns = grid.columns();
        let screen_lines = grid.screen_lines();
        let history_size = grid.history_size();
        let display_offset = grid.display_offset();
        let view_top = -(display_offset as i32);
        let view_bottom = view_top + screen_lines as i32 - 1;
        let context = context.min(screen_lines + history_size) as i32;
        // 补全跨出可见区的逻辑行：上一行自动换行到可见区的第一行时向上多取，最后一行自动换行时向下多取
        let mut top = view_top;
        while view_top - top < context && self.wraps(top - 1) {
            top -= 1;
        }
        let mut bottom = view_bottom;
        while bottom - view_bottom < context
            && bottom < screen_lines as i32 - 1
            && self.wraps(bottom)
        {
            bottom += 1;
        }

        snapshot.columns = columns;
        snapshot.screen_lines = screen_lines;
        snapshot.history_size = history_size;
        snapshot.display_offset = display_offset;
        self.fill_rows(snapshot, top, bottom, false);

        // 光标停在宽字符的占位格上时画在宽字符上
        let mut cursor = grid.cursor.point;
        if grid[cursor].flags.contains(Flags::WIDE_CHAR_SPACER) {
            cursor.column -= 1;
        }
        snapshot.cursor = Cursor {
            point: to_point(cursor),
            shape: self.cursor_shape(),
            visible: self.term.mode().contains(TermMode::SHOW_CURSOR),
        };
        snapshot.selection = self
            .term
            .selection
            .as_ref()
            .and_then(|selection| selection.to_range(&self.term))
            .map(|range| SelectionRange {
                start: to_point(range.start),
                end: to_point(range.end),
                block: range.is_block,
            });
        fill_overrides(&mut snapshot.colors, self.term.colors());
        snapshot.modes = self.modes();
    }

    fn scroll_display(&mut self, scroll: Scroll) {
        let scroll = match scroll {
            Scroll::Delta(lines) => AlacrittyScroll::Delta(lines),
            Scroll::PageUp => AlacrittyScroll::PageUp,
            Scroll::PageDown => AlacrittyScroll::PageDown,
            Scroll::Top => AlacrittyScroll::Top,
            Scroll::Bottom => AlacrittyScroll::Bottom,
            Scroll::Offset(offset) => {
                let current = self.term.grid().display_offset();
                let target = offset.min(i32::MAX as usize) as i32;
                AlacrittyScroll::Delta(target - current as i32)
            }
        };
        self.term.scroll_display(scroll);
    }

    fn scroll_to(&mut self, point: GridPoint) {
        self.term.scroll_to_point(from_point(point));
    }

    fn clear_buffer(&mut self) {
        let cursor_line = self.term.grid().cursor.point.line.0.max(0) as usize;
        self.term.scroll_display(AlacrittyScroll::Bottom);
        if cursor_line > 0 {
            self.term.scroll_up(cursor_line);
            self.term.goto_line(0);
            // 提示符所在的行上移了：命令历史记下的输入开始处跟着移动
            self.marks.add_scrolled(cursor_line);
        }
        self.term.clear_screen(ClearMode::Saved);
        self.term.selection = None;
    }

    fn start_selection(&mut self, kind: SelectionKind, point: GridPoint, side: Side) {
        let kind = match kind {
            SelectionKind::Simple => SelectionType::Simple,
            SelectionKind::Block => SelectionType::Block,
            SelectionKind::Semantic => SelectionType::Semantic,
            SelectionKind::Lines => SelectionType::Lines,
        };
        self.term.selection = Some(Selection::new(kind, from_point(point), from_side(side)));
    }

    fn update_selection(&mut self, point: GridPoint, side: Side) -> bool {
        match self.term.selection.as_mut() {
            Some(selection) => {
                selection.update(from_point(point), from_side(side));
                true
            }
            None => false,
        }
    }

    fn select_all(&mut self) {
        let grid = self.term.grid();
        let top = Point::new(Line(-(grid.history_size() as i32)), Column(0));
        let bottom = Point::new(
            Line(grid.screen_lines() as i32 - 1),
            Column(grid.columns().saturating_sub(1)),
        );
        let mut selection = Selection::new(SelectionType::Simple, top, AlacrittySide::Left);
        selection.update(bottom, AlacrittySide::Right);
        self.term.selection = Some(selection);
    }

    fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    fn selection_is_empty(&self) -> bool {
        self.term
            .selection
            .as_ref()
            .is_none_or(|selection| selection.is_empty())
    }

    fn selection_text(&self) -> Option<String> {
        self.term.selection_to_string()
    }

    fn mark_cursor(&self) -> Option<GridMark> {
        self.marks.mark_cursor(&self.term)
    }

    fn resolve(&self, mark: &GridMark) -> Option<GridPoint> {
        self.marks.resolve(&self.term, mark)
    }

    fn scroll_position(&self) -> Option<ScrollPosition> {
        self.marks.position(&self.term)
    }

    fn snapshot_lines(&self, snapshot: &mut GridSnapshot, top: i32, bottom: i32) {
        let grid = self.term.grid();
        let top = top.max(-(grid.history_size() as i32));
        let bottom = bottom.min(grid.screen_lines() as i32 - 1);
        snapshot.columns = grid.columns();
        snapshot.screen_lines = grid.screen_lines();
        snapshot.history_size = grid.history_size();
        snapshot.display_offset = grid.display_offset();
        self.fill_rows(snapshot, top, bottom, true);
    }

    fn cursor_pen(&self) -> (Cell, CellExtra) {
        let template = &self.term.grid().cursor.template;
        (to_cell(template), to_extra(template).unwrap_or_default())
    }

    fn hidden_state(&mut self) -> HiddenState {
        let mode = *self.term.mode();
        let scroll_region = self.probe_scroll_region();
        let tabs = self.probe_tabs();
        let shifted = self.probe_shifted();
        let grid = self.term.grid();
        let cursor = &grid.cursor;
        let saved = &grid.saved_cursor;
        let style = self.term.cursor_style();
        let mut colors = ColorOverrides::default();
        fill_overrides(&mut colors, self.term.colors());
        HiddenState {
            cursor: to_point(cursor.point),
            pending_wrap: cursor.input_needs_wrap,
            pen: to_cell(&cursor.template),
            pen_extra: to_extra(&cursor.template).unwrap_or_default(),
            saved: SavedCursor {
                point: to_point(saved.point),
                pen: to_cell(&saved.template),
                pen_extra: to_extra(&saved.template).unwrap_or_default(),
                charsets: special_charsets(&saved.charsets),
            },
            charsets: special_charsets(&cursor.charsets),
            shifted,
            scroll_region,
            tabs,
            origin: mode.contains(TermMode::ORIGIN),
            autowrap: mode.contains(TermMode::LINE_WRAP),
            insert: mode.contains(TermMode::INSERT),
            line_feed_new_line: mode.contains(TermMode::LINE_FEED_NEW_LINE),
            show_cursor: mode.contains(TermMode::SHOW_CURSOR),
            cursor_style: (style != self.default_cursor)
                .then(|| (self.cursor_shape(), style.blinking)),
            colors,
        }
    }

    fn with_primary_screen(&mut self, f: &mut dyn FnMut(&Self)) {
        if !self.term.mode().contains(TermMode::ALT_SCREEN) {
            return;
        }
        // 切回备用屏幕时 alacritty 会清空它、把主屏幕存下的光标设成主屏幕的光标（进入备用屏幕时本来就是这样，
        // 期间主屏幕不变）：先留一份备用屏幕（没有回滚，很小），切回后放回去
        let alt = self.term.grid().clone();
        let selection = self.term.selection.take();
        self.term.swap_alt();
        f(self);
        self.term.swap_alt();
        *self.term.grid_mut() = alt;
        self.term.selection = selection;
    }
}

/// G0–G3 是否为 DEC 画线字符集。
fn special_charsets(charsets: &Charsets) -> [bool; 4] {
    [
        CharsetIndex::G0,
        CharsetIndex::G1,
        CharsetIndex::G2,
        CharsetIndex::G3,
    ]
    .map(|index| charsets[index] == StandardCharset::SpecialCharacterAndLineDrawing)
}

/// 读出滚动区域时放在每行第一格的记号（私用区的字符，第 `line` 行用第 `line` 个）。
fn region_marker(line: usize) -> char {
    char::from_u32(0xF_0000 + line as u32).unwrap_or('\u{FFFD}')
}

/// 填快照的行；关键帧要读、alacritty 不直接给出的状态（[`Kernel::hidden_state`]）：临时改动网格读出，再原样放回。
impl AlacrittyKernel {
    /// 屏幕上的行 `top..=bottom` 填进快照（见 [`Kernel::snapshot`]、[`Kernel::snapshot_lines`]）；`extras` 时连同
    /// 下划线颜色与超链接（关键帧用，绘制用不到）。
    fn fill_rows(&self, snapshot: &mut GridSnapshot, top: i32, bottom: i32, extras: bool) {
        let grid = self.term.grid();
        let columns = grid.columns();
        snapshot.top = top;
        snapshot.cells.clear();
        snapshot.wraps.clear();
        snapshot.graphemes.clear();
        snapshot.zerowidth.clear();
        snapshot.extras.clear();
        if bottom < top {
            return;
        }
        let rows = (bottom - top + 1) as usize;
        snapshot.cells.reserve(rows * columns);
        snapshot.wraps.reserve(rows);
        for line in top..=bottom {
            let row = &grid[Line(line)][..Column(columns)];
            snapshot
                .wraps
                .push(row[columns - 1].flags.contains(Flags::WRAPLINE));
            let base = snapshot.cells.len();
            snapshot.cells.extend(row.iter().map(to_cell));
            // 组合字符另放（只有少数格子有）
            for (column, cell) in snapshot.cells[base..].iter().enumerate() {
                if cell.flags.contains(CellFlags::GRAPHEME) {
                    let start = snapshot.zerowidth.len() as u32;
                    let chars = row[column].zerowidth().unwrap_or_default();
                    snapshot.zerowidth.extend_from_slice(chars);
                    let end = snapshot.zerowidth.len() as u32;
                    snapshot.graphemes.push((base + column, start, end));
                }
            }
            if extras {
                for (column, cell) in row.iter().enumerate() {
                    if let Some(extra) = to_extra(cell) {
                        snapshot.extras.push((base + column, extra));
                    }
                }
            }
        }
    }

    /// 屏幕的各行（读状态时临时改动之前留一份）。
    fn screen_rows(&self) -> Vec<Row<AlacrittyCell>> {
        let grid = self.term.grid();
        (0..grid.screen_lines())
            .map(|line| grid[Line(line as i32)].clone())
            .collect()
    }

    fn restore_rows(&mut self, rows: Vec<Row<AlacrittyCell>>) {
        let grid = self.term.grid_mut();
        for (line, row) in rows.into_iter().enumerate() {
            grid[Line(line as i32)] = row;
        }
    }

    /// 滚动区域（alacritty 不给读）：每行第一格放上记号，让区域向下滚一行（只动区域中的行、不碰回滚），区域中的行
    /// 记号都变了；之后把屏幕、选区原样放回。
    fn probe_scroll_region(&mut self) -> Range<usize> {
        let lines = self.term.grid().screen_lines();
        let rows = self.screen_rows();
        let selection = self.term.selection.take();
        let grid = self.term.grid_mut();
        for line in 0..lines {
            grid[Line(line as i32)][Column(0)].c = region_marker(line);
        }
        self.term.scroll_down(1);
        let grid = self.term.grid();
        let moved: Vec<usize> = (0..lines)
            .filter(|&line| grid[Line(line as i32)][Column(0)].c != region_marker(line))
            .collect();
        self.restore_rows(rows);
        self.term.selection = selection;
        match (moved.first(), moved.last()) {
            (Some(&start), Some(&end)) => start..end + 1,
            _ => 0..lines,
        }
    }

    /// 制表位（alacritty 不给读）：光标放到行首逐个制表，记下停的列（最后一列不算：制表到不了更远）；之后把光标与这
    /// 一行原样放回。与默认的相同时为 `None`。
    fn probe_tabs(&mut self) -> Option<Vec<usize>> {
        let columns = self.term.grid().columns();
        let cursor = self.term.grid().cursor.clone();
        let row = self.term.grid()[cursor.point.line].clone();
        {
            let grid = self.term.grid_mut();
            grid.cursor.point.column = Column(0);
            grid.cursor.input_needs_wrap = false;
        }
        let mut stops = Vec::new();
        loop {
            let before = self.term.grid().cursor.point.column.0;
            self.term.put_tab(1);
            let after = self.term.grid().cursor.point.column.0;
            if after <= before {
                break;
            }
            stops.push(after);
        }
        let grid = self.term.grid_mut();
        grid[cursor.point.line] = row;
        grid.cursor = cursor;
        let last = columns.saturating_sub(1);
        stops.retain(|&column| column < last);
        let mut default = crate::dump::default_tabs(columns);
        default.retain(|&column| column < last);
        (stops != default).then_some(stops)
    }

    /// 正在用 G1（SO）还是 G0（alacritty 不给读）：在行首写一个 `q`，画线字符集中它是横线，按写出的字与 G0、G1
    /// 是否为画线字符集判断；之后把光标与这一行原样放回。
    fn probe_shifted(&mut self) -> bool {
        let cursor = self.term.grid().cursor.clone();
        let charsets = special_charsets(&cursor.charsets);
        if charsets[0] == charsets[1] {
            // G0、G1 相同：用哪个都一样
            return false;
        }
        let line = cursor.point.line;
        let row = self.term.grid()[line].clone();
        {
            let grid = self.term.grid_mut();
            grid.cursor.point.column = Column(0);
            grid.cursor.input_needs_wrap = false;
        }
        self.term.input('q');
        let special = self.term.grid()[line][Column(0)].c != 'q';
        let grid = self.term.grid_mut();
        grid[line] = row;
        grid.cursor = cursor;
        special == charsets[1]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(kernel: &AlacrittyKernel, context: usize) -> GridSnapshot {
        let mut snapshot = GridSnapshot::default();
        kernel.snapshot(&mut snapshot, context);
        snapshot
    }

    // 快照中的格子与 alacritty 的一致：字符、颜色、属性、宽字符与组合字符、超链接、光标、选区、改过的颜色、模式
    #[test]
    fn snapshot_converts_cells_and_state() {
        let mut kernel = AlacrittyKernel::for_test(10, 3, 100);
        kernel.advance(
            "\x1b[1;3;31;48;5;200mA\x1b[0m中e\u{301}\x1b]8;;https://x\x1b\\L\x1b]8;;\x1b\\\x1b[38;2;1;2;3;4:3m!\x1b[0m"
                .as_bytes(),
        );
        kernel.advance(b"\x1b]4;1;rgb:01/02/03\x07\x1b[?2004h\x1b[?1002h\x1b[?1006h\x1b[5 q");
        kernel.start_selection(SelectionKind::Simple, GridPoint::new(0, 1), Side::Left);
        kernel.update_selection(GridPoint::new(0, 2), Side::Right);
        let s = snapshot(&kernel, 0);
        let at = |column| *s.cell(GridPoint::new(0, column)).unwrap();
        let a = at(0);
        assert_eq!(a.c, 'A');
        assert_eq!(a.fg, Color::Indexed(1));
        assert_eq!(a.bg, Color::Indexed(200));
        assert!(a.flags.contains(CellFlags::BOLD | CellFlags::ITALIC));
        assert!(at(1).is_wide() && at(2).is_spacer());
        assert_eq!(at(1).width(), 2);
        assert!(at(3).flags.contains(CellFlags::GRAPHEME));
        assert_eq!(s.zerowidth(GridPoint::new(0, 3)), &['\u{301}']);
        assert!(at(4).flags.contains(CellFlags::HYPERLINK));
        assert_eq!(
            kernel.hyperlink_at(GridPoint::new(0, 4)).as_deref(),
            Some("https://x")
        );
        let bang = at(5);
        assert_eq!(bang.fg, Color::Rgb(Rgb { r: 1, g: 2, b: 3 }));
        assert!(bang.flags.contains(CellFlags::UNDERCURL));
        assert_eq!(at(6), Cell::default());
        assert_eq!(s.cursor().point, GridPoint::new(0, 6));
        assert_eq!(s.cursor().shape, CursorShape::Bar);
        assert!(s.cursor().visible);
        let selection = s.selection().unwrap();
        assert_eq!(
            (selection.start, selection.end),
            (GridPoint::new(0, 1), GridPoint::new(0, 2))
        );
        assert_eq!(
            s.colors().get(ColorSlot::Indexed(1)),
            Some(Rgb { r: 1, g: 2, b: 3 })
        );
        let modes = s.modes();
        assert!(modes.bracketed_paste && !modes.alt_screen);
        assert_eq!(modes.mouse_tracking, MouseTracking::Drag);
        assert_eq!(modes.mouse_encoding, MouseEncoding::Sgr);
        assert_eq!(kernel.selection_text().as_deref(), Some("中"));
    }

    // 只在有自动换行连着时多取可见区上下的行，最多 `context` 行
    #[test]
    fn snapshot_context_follows_wrapped_lines() {
        let mut kernel = AlacrittyKernel::for_test(4, 2, 100);
        kernel.advance(b"0123456789abcdefgh\r\nxy\r\nz");
        // 屏幕是 "xy"、"z"，回滚中是 "0123"…"gh"（-5…-1）；往回滚 3 行后可见区是 "89ab" 与 "cdef"，
        // 它们所在的逻辑行从 "0123" 开始、到 "gh" 为止
        kernel.scroll_display(Scroll::Delta(3));
        let s = snapshot(&kernel, 0);
        assert_eq!(s.lines(), -3..=-2);
        let s = snapshot(&kernel, 1);
        assert_eq!(s.lines(), -4..=-1);
        let s = snapshot(&kernel, 10);
        assert_eq!(s.lines(), -5..=-1);
        let mut text = String::new();
        for line in s.lines() {
            s.push_text(line, 0..4, &mut text, None);
        }
        assert_eq!(text.trim_end(), "0123456789abcdefgh");
        assert_eq!(s.line_of_row(0), -3);
        // 在底部时最后一行不自动换行：不向下多取
        kernel.scroll_display(Scroll::Bottom);
        assert_eq!(snapshot(&kernel, 10).lines(), 0..=1);
    }

    // 颜色查询按改过的颜色、没改过的按配色回答，与设备属性的回答按提问的顺序
    #[test]
    fn replies_are_kept_in_order() {
        let mut kernel = AlacrittyKernel::for_test(10, 3, 100);
        kernel.set_palette(&Palette::default_dark());
        kernel.advance(b"\x1b]11;?\x07\x1b[c");
        kernel.keyboard_request(KeyboardRequest::Query);
        kernel.theme_request(ThemeRequest::ModeQuery);
        assert_eq!(
            kernel.take_replies().as_deref(),
            Some("\x1b]11;rgb:1e1e/1e1e/1f1f\x07\x1b[?6c\x1b[?0u\x1b[?2031;2$y")
        );
        assert_eq!(kernel.take_replies(), None);
        // 本地插入的文字中的提问不回答
        kernel.inject(b"\x1b[c");
        assert_eq!(kernel.take_replies(), None);
    }
}

/// 取快照的耗时：`cargo test -p termlet-terminal --release snapshot_time -- --ignored --nocapture`
#[cfg(test)]
mod bench {
    use super::*;

    #[test]
    #[ignore = "耗时测量，手动运行"]
    fn snapshot_time() {
        // 200 列 50 行：256 色的前景与背景、粗体斜体下划线，中英文、emoji 混排
        let mut kernel = AlacrittyKernel::for_test(200, 50, 10_000);
        let words = [
            "status=200",
            "服务启动完成",
            "😀",
            "path=/api/v1",
            "👨‍👩‍👧",
            "ok",
        ];
        let mut text = String::new();
        for i in 0..200 {
            for j in 0..30 {
                let n = (i * 30 + j) % 256;
                let word = words[(i + j) % words.len()];
                text.push_str(&format!("\x1b[1;38;5;{n};48;5;{}m{word} ", 255 - n));
            }
            text.push_str("\x1b[0m\r\n");
        }
        kernel.advance(text.as_bytes());
        let mut snapshot = GridSnapshot::default();
        for context in [0, 50] {
            let frames = 2000;
            let started = Instant::now();
            for _ in 0..frames {
                kernel.snapshot(&mut snapshot, context);
                std::hint::black_box(&snapshot);
            }
            println!(
                "200×50 的快照（多取 {context} 行）：每次 {:.1} µs",
                started.elapsed().as_secs_f64() * 1e6 / frames as f64
            );
        }
    }
}
