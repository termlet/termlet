//! 把终端的内容写成控制序列：会话录制开始时先写一份当前的屏幕（[`screen_dump`]，回放从录制开始时的画面接上）；
//! 回放往前跳用的关键帧（[`checkpoint`]）写出全部内容与接着解析要用的状态，新的引擎解析它就回到当时的样子。
//!
//! - 屏幕（[`screen_dump`]）：备用屏幕中时先切到备用屏幕，再清屏、逐行写出文字与颜色（SGR）、属性，自动换行的行接着
//!   写下一行（保留软换行，复制时仍是一行），最后放好光标、按需隐藏光标。程序改过的颜色（OSC 4 等）、光标形状、
//!   超链接、下划线的颜色、终端模式（应用光标键、鼠标上报等）不写：录制开始时的画面只用来看。
//! - 关键帧（[`checkpoint`]）：回滚与屏幕一起写（多出屏幕的行随换行滚进回滚），格子连同下划线的颜色（SGR 58）与
//!   超链接（OSC 8，带上原来的 ID）；在备用屏幕中时先写主屏幕（回滚、屏幕、光标与样式），再切到备用屏幕写它的
//!   屏幕。之后写画面之外、接着解析要用到的状态（[`HiddenState`]）：制表位、DECSC 存下的光标、字符集、滚动区域与
//!   原点模式、光标位置（写满一行等着换行的照样等着）、之后写字的样式（含下划线颜色与开着的超链接）、自动换行、
//!   插入、换行时回到行首、光标的显示与形状、程序改过的颜色。shell 集成记下的命令位置（[`ShellMarks`]）写成同样的
//!   OSC 133 / 633 标记：输入开始处（B）写在原来的格子上，见过的标记、执行中的命令、633;E 给出的命令行照样接着用。
//!   不写的：标题、只影响输入的模式（应用光标键、鼠标上报、括号粘贴等，回放没有输入）。

use std::fmt::Write as _;
use std::ops::Range;

use crate::colors::{Color, ColorOverrides, Rgb};
use crate::engine::kernel::Kernel;
use crate::grid::{
    Cell, CellExtra, CellFlags, CursorShape, GridMark, GridPoint, GridSnapshot, GridText as _,
    Hyperlink,
};

/// 屏幕内容的控制序列与当时的网格大小。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScreenDump {
    pub text: String,
    pub columns: u16,
    pub rows: u16,
}

/// 关键帧要还原的、画面之外的状态（内核读出，见 [`Kernel::hidden_state`]）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HiddenState {
    /// 光标所在的格子（屏幕上的行）
    pub cursor: GridPoint,
    /// 写满了一行、等下一个字时才换行
    pub pending_wrap: bool,
    /// 之后写的字的样式
    pub pen: Cell,
    /// 之后写的字的下划线颜色与超链接
    pub pen_extra: CellExtra,
    /// DECSC 存下的光标
    pub saved: SavedCursor,
    /// G0–G3 是否为 DEC 画线字符集
    pub charsets: [bool; 4],
    /// 正在用 G1（SO）；否则是 G0
    pub shifted: bool,
    /// 滚动区域：屏幕上的行，含首不含尾
    pub scroll_region: Range<usize>,
    /// 制表位（列）；与默认的（每 8 列一个）相同时为 `None`
    pub tabs: Option<Vec<usize>>,
    /// 原点模式（DECOM）
    pub origin: bool,
    /// 自动换行（DECAWM）
    pub autowrap: bool,
    /// 插入（IRM）
    pub insert: bool,
    /// 换行时回到行首（LNM）
    pub line_feed_new_line: bool,
    pub show_cursor: bool,
    /// 程序用 DECSCUSR 设的光标形状与是否闪烁；没有设过（用默认的）为 `None`
    pub cursor_style: Option<(CursorShape, bool)>,
    /// 程序用 OSC 4 / 10 / 11 / 12 改过的颜色
    pub colors: ColorOverrides,
}

/// DECSC 存下的光标：位置、样式与字符集。
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SavedCursor {
    pub point: GridPoint,
    pub pen: Cell,
    pub pen_extra: CellExtra,
    pub charsets: [bool; 4],
}

/// 关键帧要接着用的 shell 集成状态（引擎线程上的，见 `engine/worker.rs`）：从关键帧接着解析时照样认出命令。
/// 与用户输入有关的（按回车推断的命令、输入过没有）不在其中：回放没有输入。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ShellMarks {
    /// 见过 OSC 133 / 633 的提示符或命令标记
    pub integration: bool,
    /// 命令按标记记录（见过 B 或 633;E）
    pub marked_input: bool,
    /// 当前提示符的输入开始处（B）
    pub input: Option<GridMark>,
    /// 见过 C（之后没有 C 就出现新提示符的不再记录）
    pub saw_executed: bool,
    /// 633;E 给出、还没用掉的命令行
    pub explicit: Option<String>,
    /// 命令在执行（C 之后还没有 D）
    pub running: bool,
}

/// 默认的制表位：每 8 列一个（第 0 列不算，制表只往后跳）。
pub(crate) fn default_tabs(columns: usize) -> Vec<usize> {
    (8..columns).step_by(8).collect()
}

/// 格子的样式（SGR 参数决定的部分，含下划线的颜色）。
#[derive(Clone, Copy, PartialEq, Eq)]
struct Pen {
    fg: Color,
    bg: Color,
    flags: CellFlags,
    /// 下划线的颜色（SGR 58）；`None` 为与文字同色
    underline: Option<Color>,
}

impl Default for Pen {
    fn default() -> Self {
        Self {
            fg: Color::Foreground,
            bg: Color::Background,
            flags: CellFlags::empty(),
            underline: None,
        }
    }
}

/// 写进 SGR 的属性（宽字符、组合字符、超链接等不算）。
const STYLE_FLAGS: [(CellFlags, &str); 11] = [
    (CellFlags::BOLD, "1"),
    (CellFlags::DIM, "2"),
    (CellFlags::ITALIC, "3"),
    (CellFlags::UNDERLINE, "4"),
    (CellFlags::DOUBLE_UNDERLINE, "4:2"),
    (CellFlags::UNDERCURL, "4:3"),
    (CellFlags::DOTTED_UNDERLINE, "4:4"),
    (CellFlags::DASHED_UNDERLINE, "4:5"),
    (CellFlags::INVERSE, "7"),
    (CellFlags::HIDDEN, "8"),
    (CellFlags::STRIKEOUT, "9"),
];

impl Pen {
    fn of(cell: &Cell, extra: Option<&CellExtra>) -> Self {
        let mut flags = CellFlags::empty();
        for (flag, _) in STYLE_FLAGS {
            if cell.flags.contains(flag) {
                flags |= flag;
            }
        }
        Self {
            fg: cell.fg,
            bg: cell.bg,
            flags,
            underline: extra.and_then(|e| e.underline_color),
        }
    }

    /// 从默认样式设到这个样式的 SGR（总是先 `0` 复位）。
    fn write_sgr(&self, out: &mut String) {
        out.push_str("\x1b[0");
        for (flag, code) in STYLE_FLAGS {
            if self.flags.contains(flag) {
                out.push(';');
                out.push_str(code);
            }
        }
        color_sgr(out, self.fg, 30, 90, 38);
        color_sgr(out, self.bg, 40, 100, 48);
        match self.underline {
            Some(Color::Indexed(n)) => {
                let _ = write!(out, ";58;5;{n}");
            }
            Some(Color::Rgb(rgb)) => {
                let _ = write!(out, ";58;2;{};{};{}", rgb.r, rgb.g, rgb.b);
            }
            // 下划线的颜色只能是编号或 RGB
            Some(Color::Foreground | Color::Background) | None => {}
        }
        out.push('m');
    }
}

/// 开始（`Some`，带上原来的 ID：相邻的格子仍是同一个链接）或结束（`None`）超链接（OSC 8）。
fn write_link(out: &mut String, link: Option<&Hyperlink>) {
    match link {
        Some(link) => {
            let _ = write!(out, "\x1b]8;id={};{}\x07", link.id, link.uri);
        }
        None => out.push_str("\x1b]8;;\x07"),
    }
}

/// 写的时候终端现在的样式与开着的超链接：之后的字有变化时才写。
#[derive(Default)]
struct Writer {
    pen: Pen,
    link: Option<Hyperlink>,
}

impl Writer {
    /// 之后的字用这个样式与超链接。
    fn style(&mut self, pen: Pen, link: Option<&Hyperlink>, out: &mut String) {
        if pen != self.pen {
            pen.write_sgr(out);
            self.pen = pen;
        }
        if link != self.link.as_ref() {
            write_link(out, link);
            self.link = link.cloned();
        }
    }

    /// 之后的字用这个格子的样式（含下划线颜色与超链接）。
    fn style_of(&mut self, cell: &Cell, extra: Option<&CellExtra>, out: &mut String) {
        let link = extra.and_then(|e| e.hyperlink.as_ref());
        self.style(Pen::of(cell, extra), link, out);
    }
}

/// 颜色的 SGR 参数：默认色不写；0–7 用 `base`，8–15 用 `bright`，其余 `extended;5;n` 或 `extended;2;r;g;b`。
fn color_sgr(out: &mut String, color: Color, base: u8, bright: u8, extended: u8) {
    match color {
        Color::Foreground | Color::Background => {}
        Color::Indexed(n @ 0..=7) => {
            let _ = write!(out, ";{}", base + n);
        }
        Color::Indexed(n @ 8..=15) => {
            let _ = write!(out, ";{}", bright + n - 8);
        }
        Color::Indexed(n) => {
            let _ = write!(out, ";{extended};5;{n}");
        }
        Color::Rgb(rgb) => {
            let _ = write!(out, ";{extended};2;{};{};{}", rgb.r, rgb.g, rgb.b);
        }
    }
}

/// 空白的格子：行尾的这些不用写（清过屏）。
fn is_blank(cell: &Cell, extra: Option<&CellExtra>) -> bool {
    cell.c == ' '
        && cell.bg == Color::Background
        && Pen::of(cell, None).flags == CellFlags::empty()
        && extra.is_none()
}

/// shell 集成的输入开始处（OSC 133;B）。
const INPUT_START: &str = "\x1b]133;B\x07";

/// 写出快照中的一行：自动换行到下一行的写满（终端随之换行，软换行保留），否则行尾的空白不写、之后换行；`last` 为
/// 最后一行（之后不换行）。`writer` 记着终端现在的样式。`input` 为这一行上 shell 集成的输入开始处（列）：写到那里时
/// 写一个 B，新的引擎在同一个格子上记下它。
fn write_row(
    snapshot: &GridSnapshot,
    line: i32,
    last: bool,
    input: Option<usize>,
    writer: &mut Writer,
    out: &mut String,
) {
    let Some(cells) = snapshot.row(line) else {
        return;
    };
    let extra = |column: usize| snapshot.extra(GridPoint::new(line, column));
    let wraps = snapshot.wraps(line) && !last;
    let end = if wraps {
        cells.len()
    } else {
        (0..cells.len())
            .rposition(|column| !is_blank(&cells[column], extra(column)))
            .map_or(0, |i| i + 1)
    };
    for (column, cell) in cells[..end].iter().enumerate() {
        if input == Some(column) {
            out.push_str(INPUT_START);
        }
        if cell.is_spacer() {
            continue;
        }
        writer.style_of(cell, extra(column), out);
        out.push(cell.c);
        if cell.flags.contains(CellFlags::GRAPHEME) {
            out.extend(snapshot.zerowidth(GridPoint::new(line, column)));
        }
    }
    // 输入开始处在写出的字之后（行尾的空白中）：光标移过去再写 B
    if let Some(column) = input.filter(|&c| c >= end) {
        if column > end {
            let _ = write!(out, "\x1b[{}C", column - end);
        }
        out.push_str(INPUT_START);
    }
    if !wraps && !last {
        out.push_str("\r\n");
    }
}

/// 把光标移到屏幕上的 `line` 行、`column` 列（CUP，从 1 起）。
fn write_cup(out: &mut String, line: usize, column: usize) {
    let _ = write!(out, "\x1b[{};{}H", line + 1, column + 1);
}

/// 把快照中的屏幕（显示偏移须为 0，即快照的就是屏幕）写成控制序列。
pub(crate) fn screen_dump(snapshot: &GridSnapshot) -> ScreenDump {
    let columns = snapshot.columns();
    let rows = snapshot.screen_lines();
    let mut out = String::with_capacity(columns * rows + 64);
    if snapshot.modes().alt_screen {
        out.push_str("\x1b[?1049h");
    }
    out.push_str("\x1b[0m\x1b[H\x1b[2J");
    let mut writer = Writer::default();
    for row in 0..rows {
        let last = row + 1 == rows;
        write_row(snapshot, row as i32, last, None, &mut writer, &mut out);
    }
    let cursor = snapshot.cursor();
    out.push_str("\x1b[0m");
    write_cup(
        &mut out,
        cursor.point.line.max(0) as usize,
        cursor.point.column,
    );
    if !cursor.visible {
        out.push_str("\x1b[?25l");
    }
    ScreenDump {
        text: out,
        columns: columns.min(u16::MAX as usize) as u16,
        rows: rows.min(u16::MAX as usize) as u16,
    }
}

// ------------------------------------------------------------ 关键帧

/// 一次读出的行数：填快照的缓冲按它复用，回滚很长时不一次占用大量内存。
const CHUNK_LINES: i32 = 512;

/// 关键帧：内核现在的全部内容（回滚与屏幕，在备用屏幕中时连同主屏幕）、画面之外的状态与 shell 集成记下的命令位置
/// 写成控制序列（见模块说明）。同样回滚上限、这个大小的新引擎解析它之后，接着解析同样的输出，结果与这个引擎相同。
pub(crate) fn checkpoint(kernel: &mut impl Kernel, shell: &ShellMarks) -> ScreenDump {
    let state = kernel.hidden_state();
    let alt_screen = kernel.modes().alt_screen;
    // 输入开始处只在主屏幕上（在备用屏幕中时换到主屏幕读）
    let mut input = None;
    if let Some(mark) = shell.input {
        if alt_screen {
            kernel.with_primary_screen(&mut |primary| input = primary.resolve(&mark));
        } else {
            input = kernel.resolve(&mark);
        }
    }
    let mut buffer = GridSnapshot::default();
    let mut writer = Writer::default();
    let mut out = String::new();
    write_shell_marks(shell, input.is_some(), &mut out);
    if alt_screen {
        kernel.with_primary_screen(&mut |primary| {
            write_buffer(primary, input, &mut buffer, &mut writer, &mut out);
            // 主屏幕的光标与样式：切到备用屏幕时存下，切回来时接着用（备用屏幕的光标也从它开始）
            let cursor = primary.cursor_point();
            write_cup(&mut out, cursor.line.max(0) as usize, cursor.column);
            let (pen, extra) = primary.cursor_pen();
            writer.style_of(&pen, Some(&extra), &mut out);
        });
        out.push_str("\x1b[?1049h");
        write_buffer(&*kernel, None, &mut buffer, &mut writer, &mut out);
    } else {
        write_buffer(&*kernel, input, &mut buffer, &mut writer, &mut out);
    }
    write_state(&state, &*kernel, &mut buffer, &mut writer, &mut out);
    if let Some(command) = &shell.explicit {
        let _ = write!(out, "\x1b]633;E;{}\x07", escape_vscode(command));
    }
    let dimensions = kernel.dimensions();
    ScreenDump {
        text: out,
        columns: dimensions.columns.min(u16::MAX as usize) as u16,
        rows: dimensions.screen_lines.min(u16::MAX as usize) as u16,
    }
}

/// 写在内容之前的 shell 集成标记：新的引擎什么标记都没见过，按 [`ShellMarks`] 补上。`input` 为输入开始处还在（写
/// 内容时在那个格子上写 B）。
///
/// - 按标记记命令，但没有输入开始处、也没有 633;E 的命令行（提示符开始之后、B 之前）：B 再 A。
/// - 见过 C：C（命令开始执行），不在执行中时再 D；没见过 C 而见过别的标记：D（只说明开启了 shell 集成）。
///
/// C 会清掉输入开始处，所以这些写在内容之前；B 写在内容中，633;E 写在最后（B 会清掉它）。
fn write_shell_marks(shell: &ShellMarks, input: bool, out: &mut String) {
    let prompt = shell.marked_input && !input && shell.explicit.is_none();
    if prompt {
        out.push_str(INPUT_START);
        out.push_str("\x1b]133;A\x07");
    }
    if shell.saw_executed {
        out.push_str("\x1b]133;C\x07");
        if !shell.running {
            out.push_str("\x1b]133;D\x07");
        }
    } else if shell.integration && !prompt {
        out.push_str("\x1b]133;D\x07");
    }
}

/// 写成 VS Code 的转义（`crate::shell_events` 还原它）：反斜杠写两个，分号与控制字符写作 `\xHH`。
fn escape_vscode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str(r"\\"),
            ';' => out.push_str(r"\x3b"),
            c if c.is_ascii_control() => {
                let _ = write!(out, r"\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// 回滚与屏幕的全部行，从第一行写起，多出屏幕的随换行滚进回滚。关键帧写给新的引擎（屏幕本来就是空的，切到备用
/// 屏幕时也会清空），不清屏：alacritty 对空屏幕清屏（ED 2）时也往回滚里推一行空行。`input` 为 shell 集成的输入
/// 开始处（在这些行中时）。
fn write_buffer(
    kernel: &impl Kernel,
    input: Option<GridPoint>,
    buffer: &mut GridSnapshot,
    writer: &mut Writer,
    out: &mut String,
) {
    let dimensions = kernel.dimensions();
    let top = -(dimensions.history_size as i32);
    let bottom = dimensions.screen_lines as i32 - 1;
    // SGR 0 不结束超链接：切到备用屏幕时开着的超链接跟着光标的样式过来，照旧记着
    out.push_str("\x1b[0m\x1b[H");
    writer.pen = Pen::default();
    let mut line = top;
    while line <= bottom {
        let end = (line + CHUNK_LINES - 1).min(bottom);
        kernel.snapshot_lines(buffer, line, end);
        for l in line..=end {
            let at = input.filter(|p| p.line == l).map(|p| p.column);
            write_row(buffer, l, l == bottom, at, writer, out);
        }
        line = end + 1;
    }
    writer.style(Pen::default(), None, out);
}

/// 设字符集：与 `from` 不同的那几个（`ESC ( 0` 等为画线字符集，`ESC ( B` 为 ASCII）。
fn write_charsets(out: &mut String, charsets: &[bool; 4], from: &[bool; 4]) {
    for (index, (&special, &before)) in charsets.iter().zip(from).enumerate() {
        if special != before {
            out.push('\x1b');
            out.push(['(', ')', '*', '+'][index]);
            out.push(if special { '0' } else { 'B' });
        }
    }
}

fn hex_rgb(rgb: Rgb) -> String {
    format!("rgb:{:02x}/{:02x}/{:02x}", rgb.r, rgb.g, rgb.b)
}

/// 画面之外的状态（见模块说明），写在内容之后：滚动区域、原点模式会把光标移走，最后才放好光标。
fn write_state(
    state: &HiddenState,
    kernel: &impl Kernel,
    buffer: &mut GridSnapshot,
    writer: &mut Writer,
    out: &mut String,
) {
    let rows = kernel.dimensions().screen_lines;
    // 制表位：先全部清掉再逐个设
    if let Some(tabs) = &state.tabs {
        out.push_str("\x1b[3g");
        for &column in tabs {
            write_cup(out, 0, column);
            out.push_str("\x1bH");
        }
    }
    // DECSC 存下的光标：放好位置、样式与字符集后存下
    let ascii = [false; 4];
    if state.saved != SavedCursor::default() {
        write_charsets(out, &state.saved.charsets, &ascii);
        write_cup(
            out,
            state.saved.point.line.max(0) as usize,
            state.saved.point.column,
        );
        writer.style_of(&state.saved.pen, Some(&state.saved.pen_extra), out);
        out.push_str("\x1b7");
        write_charsets(out, &state.charsets, &state.saved.charsets);
    } else {
        write_charsets(out, &state.charsets, &ascii);
    }
    if state.shifted {
        out.push('\x0e');
    }
    // 滚动区域与原点模式（都会把光标移到原点）
    let region = &state.scroll_region;
    if region.start != 0 || region.end != rows {
        let _ = write!(out, "\x1b[{};{}r", region.start + 1, region.end);
    }
    if state.origin {
        out.push_str("\x1b[?6h");
    }
    // 光标：原点模式中的行从滚动区域的第一行算起
    let top = if state.origin { region.start } else { 0 };
    let line = state.cursor.line.max(0) as usize;
    let mut column = state.cursor.column;
    let mut rewrite = None;
    if state.pending_wrap && state.autowrap {
        // 等着换行：把行尾的字（宽字符从它的第一格起）重写一遍，终端照样等着换行
        kernel.snapshot_lines(buffer, line as i32, line as i32);
        let mut at = GridPoint::new(line as i32, column);
        if buffer.cell(at).is_some_and(Cell::is_spacer) && column > 0 {
            at.column -= 1;
        }
        if let Some(cell) = buffer.cell(at).filter(|c| !c.is_spacer()) {
            column = at.column;
            let mut text = String::new();
            text.push(cell.c);
            if cell.flags.contains(CellFlags::GRAPHEME) {
                text.extend(buffer.zerowidth(at));
            }
            rewrite = Some((*cell, buffer.extra(at).cloned(), text));
        }
    }
    write_cup(out, line.saturating_sub(top), column);
    if let Some((cell, extra, text)) = rewrite {
        writer.style_of(&cell, extra.as_ref(), out);
        out.push_str(&text);
    }
    writer.style_of(&state.pen, Some(&state.pen_extra), out);
    if !state.autowrap {
        out.push_str("\x1b[?7l");
    }
    if state.insert {
        out.push_str("\x1b[4h");
    }
    if state.line_feed_new_line {
        out.push_str("\x1b[20h");
    }
    if !state.show_cursor {
        out.push_str("\x1b[?25l");
    }
    if let Some((shape, blinking)) = state.cursor_style {
        let code = match shape {
            CursorShape::Block => 1,
            CursorShape::Underline => 3,
            CursorShape::Bar => 5,
        } + u8::from(!blinking);
        let _ = write!(out, "\x1b[{code} q");
    }
    // 程序改过的颜色
    for (index, rgb) in state.colors.indexed.iter().enumerate() {
        if let Some(rgb) = rgb {
            let _ = write!(out, "\x1b]4;{index};{}\x07", hex_rgb(*rgb));
        }
    }
    for (code, rgb) in [
        (10, state.colors.foreground),
        (11, state.colors.background),
        (12, state.colors.cursor),
    ] {
        if let Some(rgb) = rgb {
            let _ = write!(out, "\x1b]{code};{}\x07", hex_rgb(rgb));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::test_kernel;

    /// 先在一个内核上显示 `input`，把屏幕写成控制序列，再在一个新的内核上显示它：两边的屏幕应该相同。
    fn round_trip(
        input: &str,
        columns: usize,
        lines: usize,
    ) -> (GridSnapshot, GridSnapshot, String) {
        let mut source = test_kernel(columns, lines, 100);
        source.advance(input.as_bytes());
        let mut before = GridSnapshot::default();
        source.snapshot(&mut before, 0);
        let dump = screen_dump(&before);
        let mut target = test_kernel(columns, lines, 100);
        target.advance(dump.text.as_bytes());
        let mut after = GridSnapshot::default();
        target.snapshot(&mut after, 0);
        (before, after, dump.text)
    }

    fn screen(snapshot: &GridSnapshot) -> Vec<Vec<Cell>> {
        (0..snapshot.screen_lines() as i32)
            .map(|line| snapshot.row(line).unwrap().to_vec())
            .collect()
    }

    // 颜色、属性、中文、自动换行、光标都原样还原
    #[test]
    fn screen_round_trips() {
        let input = concat!(
            "\x1b[1;31mred\x1b[0m \x1b[38;5;200mpink\x1b[48;2;1;2;3m bg \x1b[0m\r\n",
            "\x1b[4m下划线\x1b[0m 中文\r\n",
            "0123456789abcdefghijklmnopqrstuvwxyz",
            "\x1b[5;3H",
        );
        let (before, after, text) = round_trip(input, 20, 6);
        assert_eq!(screen(&before), screen(&after), "{text:?}");
        assert!(after.wraps(2), "自动换行保留为软换行");
        assert_eq!(after.cursor().point, GridPoint::new(4, 2));
        assert!(after.cursor().visible);
    }

    // 备用屏幕中：回放先切到备用屏幕；隐藏的光标仍隐藏
    #[test]
    fn alternate_screen_and_hidden_cursor() {
        let (before, after, text) = round_trip("shell$ \x1b[?1049h\x1b[?25l\x1b[2;2Hvim", 20, 4);
        assert!(text.starts_with("\x1b[?1049h"));
        assert!(after.modes().alt_screen);
        assert!(!after.cursor().visible);
        assert_eq!(screen(&before), screen(&after));
    }

    /// 一行：格子、各格的下划线颜色与超链接、是否自动换行。
    type Line = (Vec<Cell>, Vec<Option<CellExtra>>, bool);

    /// 回滚与屏幕的全部行。
    fn lines_of(kernel: &impl Kernel) -> Vec<Line> {
        let d = kernel.dimensions();
        let (top, bottom) = (-(d.history_size as i32), d.screen_lines as i32 - 1);
        let mut snapshot = GridSnapshot::default();
        kernel.snapshot_lines(&mut snapshot, top, bottom);
        (top..=bottom)
            .map(|l| {
                let extras = (0..d.columns)
                    .map(|c| snapshot.extra(GridPoint::new(l, c)).cloned())
                    .collect();
                (snapshot.row(l).unwrap().to_vec(), extras, snapshot.wraps(l))
            })
            .collect()
    }

    /// 整个内核的内容与光标（在备用屏幕中时连同主屏幕的），比较关键帧前后用。
    #[allow(clippy::type_complexity)]
    fn everything(
        kernel: &mut impl Kernel,
    ) -> (
        Vec<Line>,
        Option<(Vec<Line>, GridPoint, (Cell, CellExtra))>,
        GridPoint,
    ) {
        let mut primary = None;
        kernel.with_primary_screen(&mut |k| {
            primary = Some((lines_of(k), k.cursor_point(), k.cursor_pen()));
        });
        (lines_of(kernel), primary, kernel.cursor_point())
    }

    /// 在一个内核上解析 `before`，写成关键帧，在新的内核上解析关键帧；之后两边都接着解析 `after`：两边的全部内容、
    /// 光标与画面之外的状态都应该相同。
    fn continues_alike(before: &str, after: &str, columns: usize, lines: usize) -> String {
        let mut source = test_kernel(columns, lines, 50);
        source.advance(before.as_bytes());
        let frame = checkpoint(&mut source, &ShellMarks::default());
        assert_eq!(
            (frame.columns as usize, frame.rows as usize),
            (columns, lines)
        );
        let mut target = test_kernel(columns, lines, 50);
        target.advance(frame.text.as_bytes());
        assert_eq!(
            source.hidden_state(),
            target.hidden_state(),
            "关键帧之后的状态：{:?}",
            frame.text
        );
        assert_eq!(
            everything(&mut source),
            everything(&mut target),
            "{:?}",
            frame.text
        );
        source.advance(after.as_bytes());
        target.advance(after.as_bytes());
        assert_eq!(source.hidden_state(), target.hidden_state());
        assert_eq!(
            everything(&mut source),
            everything(&mut target),
            "{:?}",
            frame.text
        );
        frame.text
    }

    // 回滚、软换行、颜色与样式都在；接着输出时样式、光标接得上
    #[test]
    fn checkpoint_keeps_history_and_pen() {
        let mut before = String::new();
        for i in 0..30 {
            before.push_str(&format!("\x1b[3{}mline {i}\x1b[0m 中文\r\n", i % 8));
        }
        before.push_str("0123456789abcdefghijkl\x1b[1;4;38;2;9;8;7mbold");
        let text = continues_alike(&before, "-tail\r\nnext", 10, 4);
        assert!(text.contains("line 20"), "回滚在关键帧中");
        // 写满一行、等着换行：接着写的字换到下一行
        continues_alike("\x1b[2J\x1b[H0123456789", "X", 10, 4);
        continues_alike("\x1b[2J\x1b[H01234567中", "X", 10, 4);
    }

    // 滚动区域、原点模式、存下的光标、字符集、制表位、插入、隐藏光标、光标形状、改过的颜色
    #[test]
    fn checkpoint_keeps_hidden_state() {
        let before = concat!(
            "header\r\n",
            "\x1b[2;4r",              // 滚动区域 2–4 行
            "\x1b[?6h",               // 原点模式
            "\x1b[2;3H\x1b[31m\x1b7", // 存下光标（区域中第 2 行），红色
            "\x1b[0;1m",
            "\x1b)0\x1b+0",                        // G1、G3 画线
            "\x0e",                                // 用 G1
            "\x1b[3g\x1b[1;4H\x1bH\x1b[1;7H\x1bH", // 制表位只有 3、6 列
            "\x1b[4h",                             // 插入
            "\x1b[?25l\x1b[4 q",
            "\x1b]4;1;rgb:01/02/03\x07\x1b]11;rgb:10/20/30\x07",
            "\x1b[1;1Hqx",
        );
        let after = concat!(
            "\x0f\tA\tB\r\n\n\n\nscrolled", // 回到 G0；按制表位跳；在区域中滚动
            "\x1b8C",                       // 回到存下的光标，用存下的样式
            "\x1b[?6l\x1b[r\x1b[10;1Hbottom",
        );
        let text = continues_alike(before, after, 12, 6);
        assert!(text.contains("\x1b[2;4r"));
        // 读出的状态与设的相同，读的时候临时改动的网格原样放回
        let mut kernel = test_kernel(12, 6, 50);
        kernel.advance(before.as_bytes());
        let mut screen_before = GridSnapshot::default();
        kernel.snapshot(&mut screen_before, 0);
        let state = kernel.hidden_state();
        assert_eq!(state.scroll_region, 1..4);
        assert_eq!(state.tabs, Some(vec![3, 6]));
        assert_eq!(state.charsets, [false, true, false, true]);
        assert!(state.shifted && state.origin && state.insert && !state.show_cursor);
        assert_eq!(state.saved.point, GridPoint::new(2, 2));
        assert_eq!(state.saved.pen.fg, Color::Indexed(1));
        assert!(state.pen.flags.contains(CellFlags::BOLD));
        assert_eq!(state.cursor, GridPoint::new(1, 2));
        assert_eq!(state.cursor_style, Some((CursorShape::Underline, false)));
        assert_eq!(
            state.colors.background,
            Some(Rgb {
                r: 16,
                g: 32,
                b: 48
            })
        );
        let mut screen_after = GridSnapshot::default();
        kernel.snapshot(&mut screen_after, 0);
        assert_eq!(screen(&screen_before), screen(&screen_after));
        assert_eq!(screen_before.cursor(), screen_after.cursor());
    }

    // 备用屏幕：主屏幕的回滚、内容与光标都在，退出备用屏幕后回到主屏幕
    #[test]
    fn checkpoint_keeps_primary_under_alternate_screen() {
        let mut before = String::new();
        for i in 0..12 {
            before.push_str(&format!("shell {i}\r\n"));
        }
        before.push_str("$ vim\x1b[32m\x1b[?1049h\x1b[2;2Hediting\x1b[1;3r");
        continues_alike(&before, "\x1b[3;1H\nmore\x1b[r\x1b[?1049lback", 12, 4);
    }

    // 超链接（程序给了 ID 的、没给的，地址中有分号的）与下划线的颜色：格子上的、行尾只带下划线颜色的空格、之后写字
    // 的样式中开着的、DECSC 存下的都在
    #[test]
    fn checkpoint_keeps_hyperlinks_and_underline_colors() {
        let before = concat!(
            "\x1b]8;;https://a.example/x;y\x07link\x1b]8;;\x07 ",
            "\x1b]8;id=doc;https://b.example\x07two\x1b]8;;\x07\r\n",
            "\x1b[4;58;5;196mred\x1b[59m plain\x1b[24;58;2;1;2;3m  \x1b[0m\r\n",
            "\x1b]8;id=doc;https://b.example\x07\x1b[58;5;3m\x1b7",
            "\x1b[0m\x1b]8;;\x07end ",
            "\x1b]8;;https://c.example\x07\x1b[4:3;58;2;9;8;7m",
        );
        let text = continues_alike(before, "more\x1b8saved", 20, 4);
        for part in [
            "58;5;196",
            ";58;2;1;2;3",
            "id=doc;https://b.example",
            "https://a.example/x;y",
        ] {
            assert!(text.contains(part), "{part}：{text:?}");
        }
        let mut kernel = test_kernel(20, 4, 50);
        kernel.advance(text.as_bytes());
        assert_eq!(
            kernel.hyperlink_at(GridPoint::new(0, 1)).as_deref(),
            Some("https://a.example/x;y")
        );
        assert_eq!(kernel.hyperlink_at(GridPoint::new(0, 4)), None);
    }

    // 备用屏幕中：进入前开着的超链接与下划线颜色跟着光标的样式过来，退出后主屏幕接着用
    #[test]
    fn checkpoint_keeps_links_across_the_alternate_screen() {
        continues_alike(
            "$ \x1b]8;id=p;https://p.example\x07\x1b[58;5;4mls\x1b[?1049hvim",
            " more\x1b[?1049l back\x1b]8;;\x07 done",
            16,
            3,
        );
    }

    // 默认状态的关键帧很短：没有多余的状态
    #[test]
    fn checkpoint_of_plain_screen_is_compact() {
        let text = continues_alike("hello", " world", 20, 3);
        assert!(!text.contains("\x1b7") && !text.contains("\x1b[3g") && !text.contains(";3r"));
        assert!(
            !text.contains("\x1b]"),
            "没有超链接、shell 集成的标记：{text:?}"
        );
    }

    // 命令行写成 VS Code 的转义，shell 集成的扫描还原出原样
    #[test]
    fn explicit_command_line_round_trips() {
        let command = "echo a;b \\ \t中文\x1b";
        let mut found = Vec::new();
        let text = format!("\x1b]633;E;{}\x07", escape_vscode(command));
        crate::shell_events::OscScanner::new().scan(text.as_bytes(), &mut found);
        assert_eq!(
            found.pop().map(|m| m.mark),
            Some(crate::shell_events::ShellMark::CommandLine(
                crate::shell_events::CommandText(command.into())
            ))
        );
    }
}
