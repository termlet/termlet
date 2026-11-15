//! 引擎自己的网格类型：坐标、单元格、光标、选区、终端模式，以及绘制用的可见区快照。
//!
//! 视图、查找、关键词高亮、shell 集成、文本提取、鼠标与按键编码只认这里的类型；内核（现在是 alacritty_terminal，
//! 见 `engine::alacritty`）把自己的状态转换成它们，换内核时这些类型与用到它们的代码不变。
//!
//! - 坐标：[`GridPoint`] 的行号 0 是屏幕的第一行，负数是回滚中的行（-1 紧挨着屏幕）。用户往回滚动时可见区的第一行是
//!   `-display_offset`（[`GridDimensions::display_offset`]）。
//! - 绘制按快照读：[`GridSnapshot`] 是这一帧可见区（加上补全逻辑行要多取的上下几行）的一份拷贝，由引擎线程填好
//!   交给界面，之后的排版、高亮都不再碰内核。快照的缓冲在引擎线程与界面之间换来换去，反复使用（不重新分配）。
//! - 要读整个回滚的操作（查找、选中的文字、读命令）经引擎的方法发给引擎线程，在内核上按 [`GridText`] 逐行取文字。

use std::ops::{BitOr, BitOrAssign, Range, RangeInclusive};

use crate::colors::{Color, ColorOverrides};
use crate::kitty_keyboard::KeyboardFlags;
use crate::suggest::PromptInput;

/// 网格中的一个格子。`line` 是行号：0 是屏幕的第一行，负数是回滚中的行（-1 紧挨着屏幕）；`column` 从 0 开始。
/// 先按行、再按列排序。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GridPoint {
    pub line: i32,
    pub column: usize,
}

impl GridPoint {
    pub const fn new(line: i32, column: usize) -> Self {
        Self { line, column }
    }
}

/// 落在格子的左半还是右半（选区从哪一侧开始、到哪一侧为止）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// 选区的种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionKind {
    /// 按住左键拖动
    Simple,
    /// Alt 拖动的矩形
    Block,
    /// 双击选词
    Semantic,
    /// 三击选行
    Lines,
}

/// 选区覆盖的范围（含两端，`start` 不在 `end` 之后）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectionRange {
    pub start: GridPoint,
    pub end: GridPoint,
    /// 矩形选区：每一行只含 `start.column..=end.column`
    pub block: bool,
}

impl SelectionRange {
    /// 格子在选区中。
    pub fn contains(&self, point: GridPoint) -> bool {
        self.start.line <= point.line
            && self.end.line >= point.line
            && (self.start.column <= point.column || (self.start.line != point.line && !self.block))
            && (self.end.column >= point.column || (self.end.line != point.line && !self.block))
    }

    /// 画格子时是否画成选中：宽字符的后一格（占位）选中时它也算；方块光标（`block_cursor`）所在的格子在选区的
    /// 端点上时不算，免得光标被选区的颜色盖掉。
    pub fn contains_cell(
        &self,
        point: GridPoint,
        wide: bool,
        block_cursor: Option<GridPoint>,
    ) -> bool {
        if block_cursor == Some(point)
            && (self.start == point
                || self.end == point
                || (self.block
                    && ((self.start.line == point.line && self.end.column == point.column)
                        || (self.end.line == point.line && self.start.column == point.column))))
        {
            return false;
        }
        self.contains(point)
            || (wide && self.contains(GridPoint::new(point.line, point.column + 1)))
    }
}

/// 滚动可见区。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scroll {
    /// 往回（正数）或往前（负数）滚这么多行
    Delta(i32),
    PageUp,
    PageDown,
    /// 回滚的最上面
    Top,
    /// 最底部（最新的输出）
    Bottom,
    /// 滚到显示偏移为这个值（0 为底部，超出回滚时到最上面）
    Offset(usize),
}

/// 光标的形状：设置中的默认形状，也是程序用 DECSCUSR（`CSI Ps SP q`）设置的形状。程序设置的形状与闪烁优先，
/// `CSI 0 SP q`、RIS 后回到默认的。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorShape {
    /// 方块
    #[default]
    Block,
    /// 竖线
    Bar,
    /// 下划线
    Underline,
}

/// 绘制用的光标。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    /// 光标所在的格子；停在宽字符的占位格上时为宽字符本身的格子
    pub point: GridPoint,
    pub shape: CursorShape,
    /// 程序没有隐藏光标（DECTCEM）
    pub visible: bool,
}

/// 单元格的属性。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct CellFlags(u16);

impl CellFlags {
    pub const INVERSE: Self = Self(1 << 0);
    pub const BOLD: Self = Self(1 << 1);
    pub const ITALIC: Self = Self(1 << 2);
    pub const UNDERLINE: Self = Self(1 << 3);
    /// 格子里除了 `c` 还有组合字符（零宽字符、emoji 的 ZWJ 序列等），见 [`GridSnapshot::zerowidth`]
    pub const GRAPHEME: Self = Self(1 << 4);
    /// 宽字符（中日韩文字、emoji 等），占这一格与后一格
    pub const WIDE: Self = Self(1 << 5);
    /// 宽字符后一格的占位
    pub const WIDE_SPACER: Self = Self(1 << 6);
    /// 暗淡（SGR 2）
    pub const DIM: Self = Self(1 << 7);
    /// 隐藏（SGR 8）
    pub const HIDDEN: Self = Self(1 << 8);
    pub const STRIKEOUT: Self = Self(1 << 9);
    /// 行尾放不下宽字符时留下的占位（宽字符换到了下一行）
    pub const LEADING_WIDE_SPACER: Self = Self(1 << 10);
    pub const DOUBLE_UNDERLINE: Self = Self(1 << 11);
    /// 波浪下划线
    pub const UNDERCURL: Self = Self(1 << 12);
    pub const DOTTED_UNDERLINE: Self = Self(1 << 13);
    pub const DASHED_UNDERLINE: Self = Self(1 << 14);
    /// 格子带超链接（OSC 8），链接地址用 `TerminalEngine::hyperlink_at` 取
    pub const HYPERLINK: Self = Self(1 << 15);

    /// 各种下划线
    pub const ALL_UNDERLINES: Self = Self(
        Self::UNDERLINE.0
            | Self::DOUBLE_UNDERLINE.0
            | Self::UNDERCURL.0
            | Self::DOTTED_UNDERLINE.0
            | Self::DASHED_UNDERLINE.0,
    );
    /// 两种宽字符的占位
    pub const SPACERS: Self = Self(Self::WIDE_SPACER.0 | Self::LEADING_WIDE_SPACER.0);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn bits(self) -> u16 {
        self.0
    }

    /// 由位构造（内核转换时用，不检查）。
    pub(crate) const fn from_bits_retain(bits: u16) -> Self {
        Self(bits)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
}

impl BitOr for CellFlags {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl BitOrAssign for CellFlags {
    fn bitor_assign(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

/// 一个格子：字符、前景背景色与属性。组合字符在快照里另放（[`GridSnapshot::zerowidth`]），格子本身可以按值复制。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: CellFlags,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            c: ' ',
            fg: Color::Foreground,
            bg: Color::Background,
            flags: CellFlags::empty(),
        }
    }
}

impl Cell {
    /// 宽字符：占这一格与后一格。
    pub fn is_wide(&self) -> bool {
        self.flags.contains(CellFlags::WIDE)
    }

    /// 宽字符的占位格：不画、不取文字。
    pub fn is_spacer(&self) -> bool {
        self.flags.intersects(CellFlags::SPACERS)
    }

    /// 占几格：宽字符 2，占位 0，其余 1。
    pub fn width(&self) -> usize {
        if self.is_spacer() {
            0
        } else if self.is_wide() {
            2
        } else {
            1
        }
    }
}

/// 格子上少见的样式：下划线的颜色（SGR 58）与超链接（OSC 8）。只有少数格子有，绘制用不到（超链接的地址按需另取），
/// 只有关键帧要原样写出（见 `crate::dump::checkpoint`），只在 `Kernel::snapshot_lines` 时读出。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CellExtra {
    pub(crate) underline_color: Option<Color>,
    pub(crate) hyperlink: Option<Hyperlink>,
}

impl CellExtra {
    pub(crate) fn is_empty(&self) -> bool {
        self.underline_color.is_none() && self.hyperlink.is_none()
    }
}

/// 超链接（OSC 8）：程序给的 ID（没给时内核自己编一个）与地址。ID 相同的相邻格子是同一个链接。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Hyperlink {
    pub(crate) id: String,
    pub(crate) uri: String,
}

/// 远端要求上报哪些鼠标事件（DEC 1000 / 1002 / 1003，互斥）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseTracking {
    /// 不上报
    #[default]
    Off,
    /// 按下与松开（1000）
    Click,
    /// 另加按住键时的移动（1002）
    Drag,
    /// 另加所有的移动（1003）
    Motion,
}

/// 鼠标上报的编码（DEC 1005 / 1006，互斥）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseEncoding {
    /// 坐标加 32 后作为一个字节
    #[default]
    Default,
    /// 坐标按 UTF-8 编码（1005）
    Utf8,
    /// SGR（1006）：`CSI < 按键 ; 列 ; 行 M/m`
    Sgr,
}

/// 程序设置的终端模式（只读的一份拷贝）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TerminalModes {
    /// 在备用屏幕中（vim、htop 等全屏程序）
    pub alt_screen: bool,
    /// 显示光标（DECTCEM）
    pub show_cursor: bool,
    /// 应用光标键（DECCKM）：方向键发 `ESC O x`
    pub app_cursor: bool,
    /// 括号粘贴（DEC 2004）
    pub bracketed_paste: bool,
    /// 回车发 CR LF（LNM）
    pub line_feed_new_line: bool,
    /// 备用屏幕中滚轮转成方向键（DEC 1007）
    pub alternate_scroll: bool,
    pub mouse_tracking: MouseTracking,
    pub mouse_encoding: MouseEncoding,
    /// 程序在所在的屏幕上开启的 kitty 键盘协议标志
    pub keyboard: KeyboardFlags,
}

/// 网格的大小与可见区的位置。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GridDimensions {
    pub columns: usize,
    /// 屏幕的行数
    pub screen_lines: usize,
    /// 回滚中现有的行数
    pub history_size: usize,
    /// 用户往回滚动了多少行（0 为底部）
    pub display_offset: usize,
}

/// 网格中一个格子的标记：之后输出滚动了也能找回它（内核负责跟着滚动），命令历史用它记住输入从哪里开始。
/// 改变终端大小（重排）、重置、清空回滚之后失效。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GridMark {
    /// 绝对行号：屏幕行号 + 做标记时主屏幕内容累计上移的行数
    pub(crate) line: i64,
    pub(crate) column: usize,
    /// 布局的代数：重排、重置之后加一，之前的标记失效
    pub(crate) layout: u64,
}

/// 主屏幕滚动计数的读数，查找据此只重新查找有变化的部分。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScrollPosition {
    pub(crate) scrolled: i64,
    pub(crate) layout: u64,
}

impl ScrollPosition {
    /// 从 `earlier` 到现在内容上移的行数；之间重排、重置、清空过回滚时为 `None`。
    pub fn lines_since(&self, earlier: ScrollPosition) -> Option<usize> {
        if self.layout != earlier.layout {
            return None;
        }
        usize::try_from(self.scrolled - earlier.scrolled).ok()
    }
}

/// 按行读网格中的文字：查找、关键词高亮、链接识别、读命令都在逻辑行上做（见 [`crate::text`]）。
/// 内核（整个回滚与屏幕）与快照（可见区及多取的几行）都实现它。
pub trait GridText {
    fn columns(&self) -> usize;

    /// 能读的行（含两端）。
    fn lines(&self) -> RangeInclusive<i32>;

    /// 这一行在行尾自动换行：下一行是它的延续。范围之外的行为 `false`。
    fn wraps(&self, line: i32) -> bool;

    /// 把第 `line` 行 `columns` 范围内格子的文字接到 `text` 后面：跳过宽字符的占位格，带上组合字符；
    /// 给出 `cells` 时记下每个字符在 `text` 中的字节偏移与它的格子。
    fn push_text(
        &self,
        line: i32,
        columns: Range<usize>,
        text: &mut String,
        cells: Option<&mut Vec<(usize, GridPoint)>>,
    );
}

/// 快照的参数：为补全跨出可见区的逻辑行最多多取的行数，是否同时读出 shell 提示符后正在输入的命令。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SnapshotParams {
    pub(crate) context: usize,
    pub(crate) prompt_input: bool,
}

/// 快照的来历：哪个引擎、内容的哪个版本、按什么参数填的。界面据此判断手里的快照是不是最新的（是就不再取）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SnapshotStamp {
    pub(crate) engine: u64,
    /// 引擎线程上内容的版本：输出、滚动、选区等改变了可见区时加一
    pub(crate) version: u64,
    pub(crate) params: SnapshotParams,
}

/// 这一帧可见区的快照：可见区的格子，加上关键词高亮补全跨出可见区的逻辑行时多取的上下几行（只多取自动换行
/// 连着的行，最多各 `context` 行，见 `TerminalEngine::snapshot`），以及光标、选区、程序改过的颜色、终端模式。
/// 由引擎线程填好交给界面（见 `engine/worker.rs`），界面读它时不再碰内核。
#[derive(Clone, Debug, Default)]
pub struct GridSnapshot {
    pub(crate) columns: usize,
    pub(crate) screen_lines: usize,
    pub(crate) history_size: usize,
    pub(crate) display_offset: usize,
    /// 快照中第一行的行号
    pub(crate) top: i32,
    /// 自上而下每行 `columns` 个格子
    pub(crate) cells: Vec<Cell>,
    /// 每行是否自动换行到下一行
    pub(crate) wraps: Vec<bool>,
    /// 带组合字符的格子：（在 `cells` 中的下标，在 `zerowidth` 中的起止），按下标排列
    pub(crate) graphemes: Vec<(usize, u32, u32)>,
    pub(crate) zerowidth: Vec<char>,
    /// 带下划线颜色、超链接的格子：（在 `cells` 中的下标，样式），按下标排列；只有关键帧读的快照填
    /// （`Kernel::snapshot_lines`），绘制用的快照中总是空的
    pub(crate) extras: Vec<(usize, CellExtra)>,
    pub(crate) cursor: Cursor,
    pub(crate) selection: Option<SelectionRange>,
    pub(crate) colors: ColorOverrides,
    pub(crate) modes: TerminalModes,
    /// 引擎线程填的哪一份（内核只填上面的内容，这两项由引擎线程加上）
    pub(crate) stamp: Option<SnapshotStamp>,
    /// shell 提示符后正在输入的命令（参数要求时才读，见 [`crate::suggest`]），与快照一致
    pub(crate) prompt_input: Option<PromptInput>,
}

impl GridSnapshot {
    pub fn columns(&self) -> usize {
        self.columns
    }

    /// 屏幕（可见区）的行数。
    pub fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    pub fn history_size(&self) -> usize {
        self.history_size
    }

    pub fn display_offset(&self) -> usize {
        self.display_offset
    }

    /// 可见区第 `row` 行（从 0 开始）的行号。
    pub fn line_of_row(&self, row: usize) -> i32 {
        row as i32 - self.display_offset as i32
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    pub fn selection(&self) -> Option<SelectionRange> {
        self.selection
    }

    /// 程序用 OSC 4 / 10 / 11 / 12 改过的颜色。
    pub fn colors(&self) -> &ColorOverrides {
        &self.colors
    }

    pub fn modes(&self) -> TerminalModes {
        self.modes
    }

    /// shell 提示符后正在输入的命令（命令补全提示）：要求时才读，不在输入命令时为 `None`。
    pub(crate) fn prompt_input(&self) -> Option<&PromptInput> {
        self.prompt_input.as_ref()
    }

    /// 快照中的一行；不在快照中时为 `None`。
    pub fn row(&self, line: i32) -> Option<&[Cell]> {
        let start = self.index(GridPoint::new(line, 0))?;
        self.cells.get(start..start + self.columns)
    }

    pub fn cell(&self, point: GridPoint) -> Option<&Cell> {
        self.cells.get(self.index(point)?)
    }

    /// 格子中 `c` 之后的组合字符（没有时为空）。
    pub fn zerowidth(&self, point: GridPoint) -> &[char] {
        let Some(index) = self.index(point) else {
            return &[];
        };
        match self.graphemes.binary_search_by_key(&index, |&(i, ..)| i) {
            Ok(found) => {
                let (_, start, end) = self.graphemes[found];
                &self.zerowidth[start as usize..end as usize]
            }
            Err(_) => &[],
        }
    }

    /// 格子的下划线颜色与超链接（没有时为 `None`；只有关键帧读的快照中有）。
    pub(crate) fn extra(&self, point: GridPoint) -> Option<&CellExtra> {
        let index = self.index(point)?;
        let found = self.extras.binary_search_by_key(&index, |(i, _)| *i).ok()?;
        Some(&self.extras[found].1)
    }

    /// 快照中的行数（含多取的上下几行）。
    fn rows(&self) -> usize {
        self.wraps.len()
    }

    fn index(&self, point: GridPoint) -> Option<usize> {
        let row = usize::try_from(point.line - self.top).ok()?;
        (row < self.rows() && point.column < self.columns)
            .then(|| row * self.columns + point.column)
    }

    /// 测试中改一个格子（例如模拟程序着色）。
    #[cfg(test)]
    pub(crate) fn cell_mut(&mut self, point: GridPoint) -> Option<&mut Cell> {
        let index = self.index(point)?;
        self.cells.get_mut(index)
    }
}

impl GridText for GridSnapshot {
    fn columns(&self) -> usize {
        self.columns
    }

    fn lines(&self) -> RangeInclusive<i32> {
        self.top..=self.top + self.rows() as i32 - 1
    }

    fn wraps(&self, line: i32) -> bool {
        usize::try_from(line - self.top)
            .ok()
            .and_then(|row| self.wraps.get(row))
            .copied()
            .unwrap_or(false)
    }

    fn push_text(
        &self,
        line: i32,
        columns: Range<usize>,
        text: &mut String,
        mut cells: Option<&mut Vec<(usize, GridPoint)>>,
    ) {
        let Some(row) = self.row(line) else {
            return;
        };
        let end = columns.end.min(row.len());
        if columns.start >= end {
            return;
        }
        for (offset, cell) in row[columns.start..end].iter().enumerate() {
            if cell.is_spacer() {
                continue;
            }
            let point = GridPoint::new(line, columns.start + offset);
            if let Some(cells) = cells.as_deref_mut() {
                cells.push((text.len(), point));
            }
            text.push(cell.c);
            if cell.flags.contains(CellFlags::GRAPHEME) {
                text.extend(self.zerowidth(point));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_contains_follows_kind() {
        let at = GridPoint::new;
        let simple = SelectionRange {
            start: at(0, 5),
            end: at(2, 2),
            block: false,
        };
        assert!(simple.contains(at(1, 0)) && simple.contains(at(0, 9)));
        assert!(!simple.contains(at(0, 4)) && !simple.contains(at(2, 3)));
        let block = SelectionRange {
            block: true,
            start: at(0, 2),
            end: at(2, 5),
        };
        assert!(block.contains(at(1, 3)));
        assert!(!block.contains(at(1, 6)) && !block.contains(at(1, 1)));
        // 宽字符的占位选中时宽字符也算；方块光标在端点上时不算
        assert!(simple.contains_cell(at(0, 4), true, None));
        assert!(!simple.contains_cell(at(0, 5), false, Some(at(0, 5))));
        assert!(simple.contains_cell(at(0, 6), false, Some(at(0, 6))));
    }

    #[test]
    fn snapshot_reads_rows_and_graphemes() {
        let cell = |c| Cell {
            c,
            ..Cell::default()
        };
        let mut snapshot = GridSnapshot {
            columns: 3,
            screen_lines: 2,
            top: -1,
            cells: vec![
                cell('a'),
                cell('b'),
                cell('c'),
                Cell {
                    flags: CellFlags::WIDE | CellFlags::GRAPHEME,
                    ..cell('中')
                },
                Cell {
                    flags: CellFlags::WIDE_SPACER,
                    ..cell(' ')
                },
                cell('d'),
            ],
            wraps: vec![true, false],
            graphemes: vec![(3, 0, 1)],
            zerowidth: vec!['\u{301}'],
            ..GridSnapshot::default()
        };
        assert_eq!(snapshot.lines(), -1..=0);
        assert!(snapshot.wraps(-1) && !snapshot.wraps(0) && !snapshot.wraps(5));
        let mut text = String::new();
        let mut cells = Vec::new();
        snapshot.push_text(-1, 0..3, &mut text, None);
        snapshot.push_text(0, 0..3, &mut text, Some(&mut cells));
        assert_eq!(text, "abc中\u{301}d");
        assert_eq!(
            cells,
            vec![(3, GridPoint::new(0, 0)), (8, GridPoint::new(0, 2))]
        );
        assert_eq!(snapshot.zerowidth(GridPoint::new(-1, 0)), &[] as &[char]);
        assert!(snapshot.cell(GridPoint::new(1, 0)).is_none());
        snapshot.cell_mut(GridPoint::new(0, 2)).unwrap().c = 'e';
        assert_eq!(snapshot.row(0).unwrap()[2].c, 'e');
    }
}
