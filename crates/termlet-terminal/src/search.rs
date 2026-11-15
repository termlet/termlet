//! 在终端（含回滚）中查找。
//!
//! 经引擎在内核上进行（在引擎线程上，界面等结果），要快：逐个逻辑行取文字匹配（复用同一个缓冲，不为每个字符记格子），
//! 只为有匹配的行再取格子换算位置；从最下面往上找，匹配太多时只留最近的。有新输出后的刷新见 [`refresh`]：
//! 回滚中的行不再变化，只重新查找原来的屏幕及之后的部分。

use regex::{Regex, RegexBuilder};

use crate::grid::{GridPoint, GridText};
use crate::text::{logical_lines, logical_start, logical_text};

/// 最多记录的匹配数（最下面、最近的这些），避免在巨大的回滚中卡住。
pub const MAX_MATCHES: usize = 10_000;

/// 查找条件。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchQuery {
    pub text: String,
    pub case_sensitive: bool,
    /// 按正则表达式查找；否则按字面查找
    pub regex: bool,
    pub whole_word: bool,
}

impl SearchQuery {
    /// 编译成正则表达式。空的查找条件返回 `Ok(None)`。
    pub fn compile(&self) -> Result<Option<Regex>, regex::Error> {
        if self.text.is_empty() {
            return Ok(None);
        }
        let mut pattern = if self.regex {
            self.text.clone()
        } else {
            regex::escape(&self.text)
        };
        if self.whole_word {
            pattern = format!(r"\b(?:{pattern})\b");
        }
        RegexBuilder::new(&pattern)
            .case_insensitive(!self.case_sensitive)
            .size_limit(1 << 20)
            .build()
            .map(Some)
    }
}

/// 一处匹配（含两端的格子）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchMatch {
    pub start: GridPoint,
    pub end: GridPoint,
}

impl SearchMatch {
    pub fn contains(&self, point: GridPoint) -> bool {
        self.start <= point && point <= self.end
    }

    /// 内容上移 `lines` 行之后的位置。
    pub(crate) fn shifted(&self, lines: i64) -> Option<Self> {
        let line = |p: GridPoint| {
            i32::try_from(p.line as i64 - lines)
                .ok()
                .map(|l| GridPoint::new(l, p.column))
        };
        Some(Self {
            start: line(self.start)?,
            end: line(self.end)?,
        })
    }
}

/// 在整个网格（回滚与屏幕）中查找，按位置排列；超过 [`MAX_MATCHES`] 处时只留最下面的。
pub fn find_all<G: GridText + ?Sized>(grid: &G, regex: &Regex) -> Vec<SearchMatch> {
    let top = *grid.lines().start();
    find_from(grid, regex, top, MAX_MATCHES)
}

/// 有新输出之后更新查找结果：自上次查找以来主屏幕的内容上移了 `scrolled` 行。回滚中的行不再变化，
/// 原来的匹配跟着上移（滚出回滚的去掉）；原来屏幕的第一行（连同它所在逻辑行在回滚中的部分）及之后的行重新查找。
/// 调用方负责判断能不能这样更新（同一块屏幕、没有重排、没有清空回滚等），不能时用 [`find_all`]。
pub fn refresh<G: GridText + ?Sized>(
    grid: &G,
    regex: &Regex,
    previous: &[SearchMatch],
    scrolled: usize,
) -> Vec<SearchMatch> {
    let lines = grid.lines();
    let (top, bottom) = (*lines.start(), *lines.end());
    let scrolled = scrolled.min(i32::MAX as usize) as i64;
    let old_screen_top = (-scrolled).max(top as i64) as i32;
    let from = logical_start(grid, old_screen_top.min(bottom), top);
    let mut matches: Vec<SearchMatch> = previous
        .iter()
        .filter_map(|m| m.shifted(scrolled))
        .filter(|m| m.start.line >= top && m.end.line < from)
        .collect();
    matches.extend(find_from(grid, regex, from, MAX_MATCHES));
    if matches.len() > MAX_MATCHES {
        matches.drain(..matches.len() - MAX_MATCHES);
    }
    matches
}

/// 从 `from`（一个逻辑行的第一行）到底部查找：从最下面的逻辑行往上，最多 `limit` 处，结果按位置排列。
fn find_from<G: GridText + ?Sized>(
    grid: &G,
    regex: &Regex,
    from: i32,
    limit: usize,
) -> Vec<SearchMatch> {
    let mut found = Vec::new();
    let mut text = String::new();
    let mut spans = Vec::new();
    let mut end = *grid.lines().end();
    while end >= from && found.len() < limit {
        let start = logical_start(grid, end, from);
        logical_text(grid, start, end, &mut text);
        spans.clear();
        if !text.is_empty() {
            spans.extend(regex.find_iter(&text).map(|m| (m.start(), m.end())));
        }
        if !spans.is_empty()
            && let Some(line) = logical_lines(grid, start..=start).into_iter().next()
        {
            // 同一行中也从后往前收，整体反转后按位置排列
            for &(s, e) in spans.iter().rev() {
                if let Some(range) = line.points(s, e) {
                    found.push(SearchMatch {
                        start: *range.start(),
                        end: *range.end(),
                    });
                    if found.len() >= limit {
                        break;
                    }
                }
            }
        }
        end = start - 1;
    }
    found.reverse();
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::kernel::Kernel;
    use crate::engine::{mock_kernel, test_kernel};
    use crate::grid::ScrollPosition;

    fn query(text: &str) -> SearchQuery {
        SearchQuery {
            text: text.into(),
            ..Default::default()
        }
    }

    #[test]
    fn literal_search_is_case_insensitive_by_default() {
        let term = mock_kernel("Error: x\r\nno error here\r\nERRORS");
        let regex = query("error").compile().unwrap().unwrap();
        assert_eq!(find_all(&term, &regex).len(), 3);
        let strict = SearchQuery {
            case_sensitive: true,
            ..query("error")
        };
        assert_eq!(
            find_all(&term, &strict.compile().unwrap().unwrap()).len(),
            1
        );
    }

    #[test]
    fn whole_word_and_regex() {
        let term = mock_kernel("error errors\r\nip 10.0.0.1");
        let word = SearchQuery {
            whole_word: true,
            ..query("error")
        };
        assert_eq!(find_all(&term, &word.compile().unwrap().unwrap()).len(), 1);
        let ip = SearchQuery {
            regex: true,
            ..query(r"\d+\.\d+\.\d+\.\d+")
        };
        let matches = find_all(&term, &ip.compile().unwrap().unwrap());
        assert_eq!(
            matches,
            vec![SearchMatch {
                start: GridPoint::new(1, 3),
                end: GridPoint::new(1, 10),
            }]
        );
    }

    #[test]
    fn match_across_wrapped_rows() {
        let term = mock_kernel("abcde\nfghij");
        let matches = find_all(&term, &query("defg").compile().unwrap().unwrap());
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].start, GridPoint::new(0, 3));
        assert_eq!(matches[0].end, GridPoint::new(1, 1));
    }

    /// 带滚动计数的终端（内核在每段输出前后量一次，与引擎中相同）。
    struct Counted<K>(K);

    impl<K: Kernel> Counted<K> {
        fn feed(&mut self, text: &str) {
            self.0.advance(text.as_bytes());
        }

        fn position(&self) -> Option<ScrollPosition> {
            self.0.scroll_position()
        }
    }

    fn counted(columns: usize, lines: usize, scrollback: usize) -> Counted<impl Kernel> {
        Counted(test_kernel(columns, lines, scrollback))
    }

    #[test]
    fn refresh_after_output_equals_a_full_search() {
        let regex = query("err").compile().unwrap().unwrap();
        let mut t = counted(10, 4, 30);
        t.feed("err 1\r\nok\r\nerr 2\r\n");
        let mut matches = find_all(&t.0, &regex);
        let mut position = t.position().unwrap();
        let mut steps = vec![
            // 跨过屏幕顶的自动换行的长行、屏幕上被改写的行
            "a long line that wraps err twice err\r\n",
            "\x1b[2Aok ok ok\r\n\r\nerr 3\r\n",
            "no match\r\n",
            "",
        ];
        // 回滚满了，最早的匹配滚出回滚（一段中滚动超过回滚大小时数不准，界面改为整个重新查找）
        steps.extend(std::iter::repeat_n("err x\r\nmore err\r\n", 20));
        for output in steps {
            t.feed(output);
            let now = t.position().unwrap();
            let scrolled = now.lines_since(position).unwrap();
            matches = refresh(&t.0, &regex, &matches, scrolled);
            assert_eq!(matches, find_all(&t.0, &regex), "输出 {output:?} 之后");
            position = now;
        }
        assert_eq!(t.0.dimensions().history_size, 30);
    }

    #[test]
    fn too_many_matches_keeps_the_latest() {
        let mut t = counted(10, 3, MAX_MATCHES + 200);
        t.feed(&"x\r\n".repeat(MAX_MATCHES + 100));
        let matches = find_all(&t.0, &query("x").compile().unwrap().unwrap());
        assert_eq!(matches.len(), MAX_MATCHES);
        let last = matches.last().unwrap().start.line;
        assert_eq!(last, 1, "最后一处是最近输出的那一行");
        assert_eq!(matches[0].start.line, last - MAX_MATCHES as i32 + 1);
    }

    /// 查找的耗时：`cargo test -p termlet-terminal --release search_throughput -- --ignored --nocapture`
    #[test]
    #[ignore = "吞吐测试，手动运行"]
    fn search_throughput() {
        use std::time::Instant;

        let line = |i: usize| {
            format!(
                "\x1b[32m2026-10-15 12:00:00\x1b[0m INFO 服务启动完成 request_id={i:016x} path=/api/v1/hosts status=200 {}\r\n",
                "x".repeat(100)
            )
        };
        let mut t = counted(200, 50, 100_000);
        t.feed(&(0..100_050).map(line).collect::<String>());
        assert_eq!(t.0.dimensions().history_size, 100_000);
        {
            let started = Instant::now();
            let mut text = String::new();
            let mut total = 0;
            for line in -100_000..50 {
                logical_text(&t.0, line, line, &mut text);
                total += text.len();
            }
            println!(
                "只取文字：{total} 字节，{:.1} 毫秒",
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
        for text in ["no-such-text", "status=200", "request_id=00000000000001"] {
            let regex = query(text).compile().unwrap().unwrap();
            let started = Instant::now();
            let found = find_all(&t.0, &regex);
            println!(
                "全量查找 {text:?}：{} 处，{:.1} 毫秒",
                found.len(),
                started.elapsed().as_secs_f64() * 1000.0
            );
            // 有新输出后的刷新（每 300 毫秒一次）：又来了 500 行
            let before = t.position().unwrap();
            t.feed(&(0..500).map(line).collect::<String>());
            let scrolled = t.position().unwrap().lines_since(before).unwrap();
            let started = Instant::now();
            let refreshed = refresh(&t.0, &regex, &found, scrolled);
            println!(
                "  新输出 {scrolled} 行后刷新：{} 处，{:.2} 毫秒",
                refreshed.len(),
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
    }

    #[test]
    fn invalid_regex_is_an_error_and_empty_is_none() {
        let bad = SearchQuery {
            regex: true,
            ..query("(")
        };
        assert!(bad.compile().is_err());
        assert!(query("").compile().unwrap().is_none());
    }
}
