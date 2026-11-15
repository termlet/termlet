//! 从网格中取文字：按"逻辑行"取（自动换行的多行拼成一行），并记下每个字符所在的格子，
//! 查找、关键字高亮、链接识别都在逻辑行上做正则匹配，再把结果换回网格坐标。
//!
//! 只经 [`GridText`] 逐行取文字：内核（整个回滚与屏幕）与绘制用的快照（可见区）都可以。
//! 不用内核自带的查找（alacritty 的 `RegexSearch` 的 DFA 不支持 `\b`，`^` 在第一行之后也会失效）。

use std::ops::RangeInclusive;

use crate::grid::{GridPoint, GridText};

/// 一个逻辑行：文字与每个字符（按字节偏移）对应的格子。
#[derive(Debug, Default)]
pub struct LogicalLine {
    pub text: String,
    /// `cells[i]` 是 `text` 中第 i 个字符的格子与它在 `text` 中的字节偏移
    cells: Vec<(usize, GridPoint)>,
}

impl LogicalLine {
    /// 字节范围 `[start, end)` 对应的格子范围（含两端）。范围为空时返回 `None`。
    pub fn points(&self, start: usize, end: usize) -> Option<RangeInclusive<GridPoint>> {
        if start >= end {
            return None;
        }
        let first = self.cells.partition_point(|(offset, _)| *offset < start);
        let last = self
            .cells
            .partition_point(|(offset, _)| *offset < end)
            .checked_sub(1)?;
        let start = self.cells.get(first)?.1;
        let end = self.cells.get(last)?.1;
        Some(start..=end)
    }

    /// 某个格子处字符的字节偏移。
    pub fn offset_of(&self, point: GridPoint) -> Option<usize> {
        self.cells
            .iter()
            .find(|(_, p)| *p == point)
            .map(|(offset, _)| *offset)
    }
}

/// 网格中 `lines` 范围内（含两端，行号可为负，表示回滚中的行）的逻辑行。
/// 范围边缘被截断的逻辑行向外补全，保证每个逻辑行都完整。
pub fn logical_lines<G: GridText + ?Sized>(
    grid: &G,
    lines: RangeInclusive<i32>,
) -> Vec<LogicalLine> {
    logical_lines_within(grid, lines, usize::MAX)
}

/// 同 [`logical_lines`]，但向范围之外补全时最多各补 `extra` 行（关键字高亮每帧取可见区，
/// 不为一个占满整个回滚的超长行读完全部内容）。
pub fn logical_lines_within<G: GridText + ?Sized>(
    grid: &G,
    lines: RangeInclusive<i32>,
    extra: usize,
) -> Vec<LogicalLine> {
    let available = grid.lines();
    let top = *available.start();
    let bottom = *available.end();
    let columns = grid.columns();
    let mut start = (*lines.start()).max(top);
    let end = (*lines.end()).min(bottom);
    let extra = extra.min(i32::MAX as usize) as i32;
    let limit_top = start.saturating_sub(extra).max(top);
    let bottom = end.saturating_add(extra).min(bottom);
    // 向上补全：上一行以自动换行结尾，说明当前行是它的延续
    while start > limit_top && grid.wraps(start - 1) {
        start -= 1;
    }

    let mut result = Vec::new();
    let mut current = LogicalLine::default();
    let mut line = start;
    while line <= bottom {
        grid.push_text(
            line,
            0..columns,
            &mut current.text,
            Some(&mut current.cells),
        );
        let continues = grid.wraps(line);
        if !continues {
            // 去掉行尾的空格（终端用空格填满一行）
            let trimmed = current.text.trim_end_matches(' ').len();
            current.text.truncate(trimmed);
            current.cells.retain(|(offset, _)| *offset < trimmed);
            result.push(std::mem::take(&mut current));
            if line >= end {
                break;
            }
        }
        line += 1;
    }
    if !current.text.is_empty() {
        result.push(current);
    }
    result
}

/// 从 `start` 到 `end`（含两端）这几行组成的逻辑行的文字，写进 `text`（先清空）；与 [`logical_lines`] 给出的
/// 文字相同（字节偏移一致），但不记每个字符的格子。查找逐行扫描整个回滚时用它，只为有匹配的行再取格子。
pub fn logical_text<G: GridText + ?Sized>(grid: &G, start: i32, end: i32, text: &mut String) {
    text.clear();
    let columns = grid.columns();
    for line in start..=end {
        grid.push_text(line, 0..columns, text, None);
    }
    if !grid.wraps(end) {
        let trimmed = text.trim_end_matches(' ').len();
        text.truncate(trimmed);
    }
}

/// 逻辑行的第一行：从 `line` 向上，直到上一行不是以自动换行结尾，或到了 `top`。
pub fn logical_start<G: GridText + ?Sized>(grid: &G, line: i32, top: i32) -> i32 {
    let mut start = line;
    while start > top && grid.wraps(start - 1) {
        start -= 1;
    }
    start
}

/// 从 `from` 读到它所在逻辑行的末尾（含自动换行的续行），去掉行尾的空白；
/// 给出 `stop` 且它在这一段之中时读到它之前为止。命令历史从输入开始处读取命令。
pub fn text_from<G: GridText + ?Sized>(
    grid: &G,
    from: GridPoint,
    stop: Option<GridPoint>,
) -> String {
    let bottom = *grid.lines().end();
    let columns = grid.columns();
    let mut text = String::new();
    let mut line = from.line;
    let mut column = from.column;
    loop {
        // 停止的位置在这一行、且在读的位置之后（或就在这里）：读到它之前为止
        if let Some(stop) = stop.filter(|s| s.line == line && s.column >= column) {
            grid.push_text(line, column..stop.column, &mut text, None);
            break;
        }
        grid.push_text(line, column..columns, &mut text, None);
        if line >= bottom || !grid.wraps(line) {
            break;
        }
        line += 1;
        column = 0;
    }
    text.truncate(text.trim_end().len());
    text
}

/// 从 `from` 到 `to` 之前（不含 `to` 所在的格子）的文字，跨行时依次接上（调用方保证中间都是自动换行的续行）；
/// 不去掉末尾的空白。命令补全提示读光标之前已输入的内容。
pub fn text_between<G: GridText + ?Sized>(grid: &G, from: GridPoint, to: GridPoint) -> String {
    let columns = grid.columns();
    let mut text = String::new();
    for line in from.line..=to.line {
        let start = if line == from.line { from.column } else { 0 };
        let end = if line == to.line { to.column } else { columns };
        grid.push_text(line, start..end, &mut text, None);
    }
    text
}

/// 这一行是不是上一行自动换行的延续。
pub fn is_continuation<G: GridText + ?Sized>(grid: &G, line: i32) -> bool {
    line > *grid.lines().start() && grid.wraps(line - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::mock_kernel as mock;

    #[test]
    fn wrapped_rows_join_into_one_logical_line() {
        // mock：\n 是自动换行，\r\n 是真正的换行
        let grid = mock("hello\nworld\r\nnext");
        let lines = logical_lines(&grid, 0..=2);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "helloworld");
        assert_eq!(lines[1].text, "next");
        let range = lines[0].points(3, 7).unwrap();
        assert_eq!(*range.start(), GridPoint::new(0, 3));
        assert_eq!(*range.end(), GridPoint::new(1, 1));
    }

    #[test]
    fn partial_range_is_extended_to_whole_logical_line() {
        let grid = mock("hello\nworld\r\nnext");
        let lines = logical_lines(&grid, 1..=1);
        assert_eq!(lines[0].text, "helloworld");
    }

    #[test]
    fn extension_can_be_limited() {
        // 一个逻辑行占 4 行：从第 2 行起最多向外补 1 行
        let grid = mock("aaaa\nbbbb\ncccc\ndddd");
        let lines = logical_lines_within(&grid, 2..=2, 1);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "bbbbccccdddd");
    }

    #[test]
    fn logical_text_matches_logical_lines() {
        let grid = mock("hello\nworld  \r\n中文 x\r\nwrap\nped");
        for line in logical_lines(&grid, 0..=4) {
            let start = line.cells.first().unwrap().1.line;
            let end = line.cells.last().unwrap().1.line;
            let mut text = String::new();
            logical_text(&grid, logical_start(&grid, end, 0), end, &mut text);
            assert_eq!(text, line.text);
            assert_eq!(logical_start(&grid, end, 0), start);
        }
        assert_eq!(logical_start(&grid, 1, 1), 1, "不越过 top");
    }

    #[test]
    fn text_from_a_point_to_the_end_of_its_logical_line() {
        let grid = mock("$ echo\nhello \r\nnext");
        let at = GridPoint::new;
        assert_eq!(text_from(&grid, at(0, 2), None), "echohello");
        assert_eq!(text_from(&grid, at(0, 2), Some(at(1, 2))), "echohe");
        // 停止的位置不在这一段中：读到行尾
        assert_eq!(text_from(&grid, at(1, 1), Some(at(0, 0))), "ello");
        assert!(is_continuation(&grid, 1));
        assert!(!is_continuation(&grid, 0));
        assert!(!is_continuation(&grid, 2));
    }

    // 跨行接上、保留末尾的空格，不含终点的格子；宽字符的占位格跳过
    #[test]
    fn text_between_two_points() {
        let grid = mock("$ git s\ntat 中\r\nnext");
        let at = GridPoint::new;
        assert_eq!(text_between(&grid, at(0, 2), at(0, 6)), "git ");
        assert_eq!(text_between(&grid, at(0, 2), at(1, 3)), "git stat");
        assert_eq!(text_between(&grid, at(0, 2), at(1, 6)), "git stat 中");
        assert_eq!(text_between(&grid, at(0, 2), at(0, 2)), "");
    }
}
