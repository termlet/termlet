//! 格子标记的滚动计数：记下某个格子，之后输出滚动了也能找回它（与 xterm.js 的 marker 相同），
//! 命令历史用它记住输入从哪里开始。
//!
//! alacritty 不报告内容滚动了多少行，回滚满了以后回滚的行数也不再变化。这里在每段输出解析前后量一次：
//! 屏幕内容上移时，alacritty 会把不为 0 的显示偏移（display_offset）加上同样的行数，好让用户正在看的内容不动。
//! 所以解析前把偏移临时设成一个可以增加的值（用户在底部时设成 1），解析后看它增加了多少，再恢复成用户原来的位置。
//! 这些都在引擎线程上一次做完，界面看不到临时的偏移。
//!
//! 标记记的是"绝对行号" = 屏幕行号 + 累计上移的行数，滚动前后不变。改变终端大小（重排）、重置、清空回滚之后，
//! 之前的标记都失效。只在主屏幕上使用标记：备用屏幕没有回滚，全屏程序中也不记命令。

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::{Term, TermMode};

use crate::grid::{GridMark, GridPoint, ScrollPosition};

/// 正在量的一段：解析前的偏移。
#[derive(Clone, Copy, Debug)]
enum Counting {
    /// 量显示偏移的增加：`base` 是解析前设成的偏移，`user` 是用户原来的偏移
    Offset { base: usize, user: usize },
    /// 还没有回滚（偏移不能离开 0）：回滚还没满，量回滚行数的增加
    History { base: usize },
}

/// 主屏幕内容的滚动计数（每个终端一份，只在引擎线程上使用）。
#[derive(Debug)]
pub(super) struct Marks {
    /// 回滚的最大行数（显示偏移的上限：偏移到了上限就数不准了）
    max_history: usize,
    /// 主屏幕内容累计上移的行数
    scrolled: i64,
    /// 布局的代数：重排、重置之后加一，之前的标记失效
    layout: u64,
    /// 量到一半：解析中途切到了备用屏幕，主屏幕回来后再量
    counting: Option<Counting>,
}

impl Marks {
    pub fn new(max_history: usize) -> Self {
        Self {
            max_history,
            scrolled: 0,
            layout: 0,
            counting: None,
        }
    }

    /// 回滚的最大行数。
    pub fn max_history(&self) -> usize {
        self.max_history
    }

    /// 回滚的最大行数改变了。
    pub fn set_max_history(&mut self, lines: usize) {
        self.max_history = lines;
    }

    /// 一段输出解析之前。
    pub fn begin<T: EventListener>(&mut self, term: &mut Term<T>) {
        if self.counting.is_some() || term.mode().contains(TermMode::ALT_SCREEN) {
            return;
        }
        let grid = term.grid();
        let user = grid.display_offset();
        let history = grid.history_size();
        // 偏移不能超过回滚行数，增加到上限（回滚的最大行数）后不再增加：在最顶上时先退一行
        let base = match user {
            0 => history.min(1),
            u if u >= history => history.saturating_sub(1),
            u => u,
        };
        self.counting = Some(if base == 0 {
            Counting::History { base: history }
        } else {
            term.scroll_display(Scroll::Delta(base as i32 - user as i32));
            Counting::Offset { base, user }
        });
    }

    /// 一段输出解析之后：记下内容上移了多少行，恢复用户的显示位置。
    pub fn end<T: EventListener>(&mut self, term: &mut Term<T>) {
        if term.mode().contains(TermMode::ALT_SCREEN) {
            return;
        }
        let Some(counting) = self.counting.take() else {
            return;
        };
        let moved = self.moved(term, counting);
        if let Counting::Offset { user, .. } = counting {
            let grid = term.grid();
            let now = grid.display_offset();
            // 用户在底部时留在底部；否则与 alacritty 自己的行为相同，跟着内容上移
            let target = if user == 0 {
                0
            } else {
                (user + moved.unwrap_or(0)).min(grid.history_size())
            };
            term.scroll_display(Scroll::Delta(target as i32 - now as i32));
        }
        match moved {
            Some(lines) => self.scrolled += lines as i64,
            // 偏移或回滚变少了：清空了回滚或重置了终端
            None => self.invalidate(),
        }
    }

    /// 从开始量到现在内容上移的行数；数不准（到了上限、清空了回滚或重置了终端）时为 `None`。只读，不改动终端。
    fn moved<T>(&self, term: &Term<T>, counting: Counting) -> Option<usize> {
        let grid = term.grid();
        match counting {
            Counting::Offset { base, .. } => {
                let now = grid.display_offset();
                // 到了上限：一段中上移的行数超过了回滚的大小，数不准（之前的标记也都滚出去了）
                now.checked_sub(base).filter(|_| now < self.max_history)
            }
            Counting::History { base } => {
                let now = grid.history_size();
                // 没有回滚（最大行数为 0）时无从数起，当作没有滚动
                now.checked_sub(base)
                    .filter(|_| now < self.max_history || self.max_history == 0)
            }
        }
    }

    /// 内容另外上移了 `lines` 行（界面的"清屏"）。
    pub fn add_scrolled(&mut self, lines: usize) {
        self.scrolled += lines as i64;
    }

    /// 之前的标记全部失效（改变大小等）。
    pub fn invalidate(&mut self) {
        self.layout += 1;
    }

    /// 现在的读数；在备用屏幕中、量到一半时为 `None`。
    pub fn position<T>(&self, term: &Term<T>) -> Option<ScrollPosition> {
        (!term.mode().contains(TermMode::ALT_SCREEN) && self.counting.is_none()).then_some(
            ScrollPosition {
                scrolled: self.scrolled,
                layout: self.layout,
            },
        )
    }

    /// 给主屏幕上的一个格子做标记；在备用屏幕中时为 `None`。
    pub fn mark<T>(&self, term: &Term<T>, point: GridPoint) -> Option<GridMark> {
        if term.mode().contains(TermMode::ALT_SCREEN) || self.counting.is_some() {
            return None;
        }
        Some(GridMark {
            line: point.line as i64 + self.scrolled,
            column: point.column,
            layout: self.layout,
        })
    }

    /// 光标所在的格子。
    pub fn mark_cursor<T>(&self, term: &Term<T>) -> Option<GridMark> {
        let cursor = term.grid().cursor.point;
        self.mark(term, GridPoint::new(cursor.line.0, cursor.column.0))
    }

    /// 标记现在的位置；已经失效、滚出回滚、在备用屏幕中时为 `None`。量到一半（解析中途切到了备用屏幕）、临时换回
    /// 主屏幕读的（关键帧），按到现在为止上移的行数算。
    pub fn resolve<T>(&self, term: &Term<T>, mark: &GridMark) -> Option<GridPoint> {
        if mark.layout != self.layout || term.mode().contains(TermMode::ALT_SCREEN) {
            return None;
        }
        let scrolled = match self.counting {
            None => self.scrolled,
            Some(counting) => self.scrolled + self.moved(term, counting)? as i64,
        };
        let grid = term.grid();
        let line = mark.line - scrolled;
        let top = -(grid.history_size() as i64);
        let bottom = grid.screen_lines() as i64;
        (top <= line && line < bottom && mark.column < grid.columns())
            .then(|| GridPoint::new(line as i32, mark.column))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    struct Size(usize, usize);

    impl Dimensions for Size {
        fn total_lines(&self) -> usize {
            self.1
        }
        fn screen_lines(&self) -> usize {
            self.1
        }
        fn columns(&self) -> usize {
            self.0
        }
    }

    fn term(scrollback: usize) -> Term<VoidListener> {
        let config = Config {
            scrolling_history: scrollback,
            ..Config::default()
        };
        Term::new(config, &Size(10, 3), VoidListener)
    }

    fn feed(term: &mut Term<VoidListener>, marks: &mut Marks, text: &str) {
        let mut processor: Processor<StdSyncHandler> = Processor::new();
        marks.begin(term);
        processor.advance(term, text.as_bytes());
        marks.end(term);
    }

    #[test]
    fn mark_follows_scrolling_even_when_history_is_full() {
        // 回滚只有 5 行：很快就满了，之后回滚的行数不再变化
        let mut term = term(5);
        let mut marks = Marks::new(5);
        feed(&mut term, &mut marks, "a\r\nb\r\n$ ");
        let mark = marks.mark_cursor(&term).unwrap();
        let at = |line| Some(GridPoint::new(line, 2));
        assert_eq!(marks.resolve(&term, &mark), at(2));
        feed(&mut term, &mut marks, "ls\r\n");
        assert_eq!(marks.resolve(&term, &mark), at(1));
        feed(&mut term, &mut marks, "x\r\ny\r\n");
        assert_eq!(marks.resolve(&term, &mark), at(-1));
        feed(&mut term, &mut marks, "1\r\n2\r\n3\r\n");
        assert_eq!(term.grid().history_size(), 5, "回滚已满");
        assert_eq!(marks.resolve(&term, &mark), at(-4));
        feed(&mut term, &mut marks, "4\r\n");
        assert_eq!(marks.resolve(&term, &mark), at(-5));
        assert_eq!(term.grid()[Line(-5)][Column(2)].c, 'l');
        assert_eq!(term.grid().display_offset(), 0, "显示位置恢复到底部");
        // 滚出回滚
        feed(&mut term, &mut marks, "5\r\n");
        assert_eq!(marks.resolve(&term, &mark), None);
    }

    #[test]
    fn too_many_lines_in_one_segment_invalidates() {
        let mut term = term(5);
        let mut marks = Marks::new(5);
        feed(&mut term, &mut marks, "a\r\nb\r\nc\r\nd\r\n$ ");
        let mark = marks.mark_cursor(&term).unwrap();
        feed(&mut term, &mut marks, &"x\r\n".repeat(8));
        assert_eq!(marks.resolve(&term, &mark), None);
        assert_eq!(term.grid().display_offset(), 0);
    }

    #[test]
    fn user_scroll_position_is_kept() {
        let mut term = term(100);
        let mut marks = Marks::new(100);
        feed(&mut term, &mut marks, "1\r\n2\r\n3\r\n4\r\n5\r\n");
        term.scroll_display(Scroll::Delta(2));
        let mark = marks.mark(&term, GridPoint::new(0, 0)).unwrap();
        feed(&mut term, &mut marks, "6\r\n7\r\n");
        // 与 alacritty 自己的行为相同：用户看的内容不动
        assert_eq!(term.grid().display_offset(), 4);
        assert_eq!(marks.resolve(&term, &mark), Some(GridPoint::new(-2, 0)));
    }

    #[test]
    fn alternate_screen_and_clear_history() {
        let mut term = term(100);
        let mut marks = Marks::new(100);
        feed(&mut term, &mut marks, "1\r\n2\r\n3\r\n$ ");
        let mark = marks.mark_cursor(&term).unwrap();
        let before = marks.position(&term).unwrap();
        // 一段中进入备用屏幕：主屏幕回来后再量
        feed(&mut term, &mut marks, "\r\n\x1b[?1049hfull\r\nscreen\r\n");
        assert_eq!(marks.resolve(&term, &mark), None);
        feed(&mut term, &mut marks, "\x1b[?1049l\r\n");
        assert_eq!(marks.resolve(&term, &mark), Some(GridPoint::new(0, 2)));
        assert_eq!(term.grid().display_offset(), 0);
        // 清空回滚：之前的标记失效
        feed(&mut term, &mut marks, "\x1b[3J");
        assert_eq!(marks.resolve(&term, &mark), None);
        assert_eq!(marks.position(&term).unwrap().lines_since(before), None);
    }

    #[test]
    fn position_counts_lines_scrolled() {
        let mut term = term(100);
        let mut marks = Marks::new(100);
        feed(&mut term, &mut marks, "1\r\n2\r\n");
        let before = marks.position(&term).unwrap();
        feed(&mut term, &mut marks, "3\r\n4\r\n5\r\n");
        assert_eq!(marks.position(&term).unwrap().lines_since(before), Some(3));
        feed(&mut term, &mut marks, "\x1b[?1049h");
        assert_eq!(marks.position(&term), None, "备用屏幕中没有读数");
    }
}
