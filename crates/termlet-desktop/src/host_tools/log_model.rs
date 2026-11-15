//! 日志查看器的纯逻辑：行数与时间范围的选项、保留的行（跟随时较早的丢掉）、按关键词筛选并标出命中的位置、时间列
//! （本机时区的短格式，可以不显示）。服务、容器详情中的最近几行用同样的写法。

use std::ops::Range;

use regex::{Regex, RegexBuilder};
use termlet_core::hosttools::{LogLevel, LogLine, LogLines};

/// 读多少行。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum LinesChoice {
    #[default]
    Last200,
    Last1000,
    All,
}

impl LinesChoice {
    pub const ALL: [LinesChoice; 3] = [
        LinesChoice::Last200,
        LinesChoice::Last1000,
        LinesChoice::All,
    ];

    pub fn lines(self) -> LogLines {
        match self {
            LinesChoice::Last200 => LogLines::Last(200),
            LinesChoice::Last1000 => LogLines::Last(1000),
            LinesChoice::All => LogLines::All,
        }
    }
}

/// 时间范围。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RangeChoice {
    #[default]
    All,
    Minutes15,
    Hour1,
    /// 本机时区的今天
    Today,
}

impl RangeChoice {
    pub const ALL: [RangeChoice; 4] = [
        RangeChoice::All,
        RangeChoice::Minutes15,
        RangeChoice::Hour1,
        RangeChoice::Today,
    ];

    /// 最近多少秒（服务器上按相对的秒数筛选）；“今天”按读取时本机的时间算。
    pub fn since_secs(self, now_ms: i64) -> Option<u64> {
        match self {
            RangeChoice::All => None,
            RangeChoice::Minutes15 => Some(15 * 60),
            RangeChoice::Hour1 => Some(3600),
            RangeChoice::Today => {
                Some(termlet_core::time::seconds_since_local_midnight(now_ms).max(1))
            }
        }
    }
}

/// 查看器最多保留的行数：跟随时超过的丢掉最早的（编辑器适合五万行以内）。
pub const MAX_KEPT: usize = 20_000;

/// 加上新读到的行，超过 [`MAX_KEPT`] 时丢掉最早的；返回丢掉了多少行。
pub fn append_lines(lines: &mut Vec<LogLine>, new: Vec<LogLine>) -> usize {
    lines.extend(new);
    let extra = lines.len().saturating_sub(MAX_KEPT);
    lines.drain(..extra);
    extra
}

/// 时间列的宽度（`11-04 09:47:12`）。
const TIME_WIDTH: usize = 14;

/// 显示的内容：正文与各处的样式（UTF-8 字节范围）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LogDisplay {
    pub text: String,
    /// 错误、警告行的内容（不含时间列）
    pub levels: Vec<(Range<usize>, LogLevel)>,
    /// 时间列
    pub times: Vec<Range<usize>>,
    /// 关键词命中的位置
    pub matches: Vec<Range<usize>>,
    /// 显示的行数（筛选之后）
    pub shown: usize,
}

/// 关键词：按空白分成几个词，每个都要出现（不区分大小写）；没有词时不筛选。
pub struct Filter {
    words: Vec<Regex>,
}

impl Filter {
    pub fn new(query: &str) -> Self {
        let words = query
            .split_whitespace()
            .filter_map(|word| {
                RegexBuilder::new(&regex::escape(word))
                    .case_insensitive(true)
                    .build()
                    .ok()
            })
            .collect();
        Self { words }
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// 每个词都出现在 `text` 中。
    pub fn matches(&self, text: &str) -> bool {
        self.words.iter().all(|word| word.is_match(text))
    }

    /// 各词在 `text` 中出现的位置（按开始的位置排序，重叠的合并）。
    pub fn ranges(&self, text: &str) -> Vec<Range<usize>> {
        let mut ranges: Vec<Range<usize>> = self
            .words
            .iter()
            .flat_map(|word| word.find_iter(text).map(|m| m.range()))
            .filter(|r| !r.is_empty())
            .collect();
        ranges.sort_by_key(|r| (r.start, r.end));
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
        for range in ranges {
            match merged.last_mut() {
                Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
                _ => merged.push(range),
            }
        }
        merged
    }
}

/// 按关键词筛选、加上时间列（`time_of` 把 Unix 毫秒写成本机时区的短格式），得到显示的内容。多行消息的后续行
/// 跟着它的第一行显示或不显示。
pub fn build_display(
    lines: &[LogLine],
    filter: &Filter,
    show_time: bool,
    time_of: impl Fn(i64) -> String,
) -> LogDisplay {
    let mut out = LogDisplay::default();
    // 当前这条消息（第一行与后续行）是否显示
    let mut keep = true;
    for (ix, line) in lines.iter().enumerate() {
        if line.at.is_some() || ix == 0 || !continues(line) {
            keep = filter.is_empty() || message_matches(lines, ix, filter);
        }
        if !keep {
            continue;
        }
        if out.shown > 0 {
            out.text.push('\n');
        }
        out.shown += 1;
        if show_time {
            let start = out.text.len();
            match line.at {
                Some(ms) => {
                    let time = time_of(ms);
                    out.text.push_str(&format!("{time:<TIME_WIDTH$}"));
                    out.times.push(start..out.text.len());
                }
                None => out.text.push_str(&" ".repeat(TIME_WIDTH)),
            }
            out.text.push(' ');
        }
        let start = out.text.len();
        out.text.push_str(&line.text);
        let end = out.text.len();
        if line.level != LogLevel::Normal && end > start {
            out.levels.push((start..end, line.level));
        }
        out.matches.extend(
            filter
                .ranges(&line.text)
                .into_iter()
                .map(|r| r.start + start..r.end + start),
        );
    }
    out
}

/// 多行消息的后续行（服务日志：没有时间、以空格开头）。
fn continues(line: &LogLine) -> bool {
    line.at.is_none() && line.text.starts_with(' ')
}

/// 从第 `ix` 行开始的这条消息（连同后续行）中有没有每个词：多行的异常也能按其中的词找到。
fn message_matches(lines: &[LogLine], ix: usize, filter: &Filter) -> bool {
    let mut text = lines[ix].text.clone();
    for line in lines[ix + 1..].iter().take_while(|l| continues(l)) {
        text.push('\n');
        text.push_str(&line.text);
    }
    filter.matches(&text)
}

/// 服务、容器详情中的最近几行：时间（本机时区）与内容。
pub fn tail_rows(lines: &[LogLine]) -> Vec<(String, &LogLine)> {
    lines
        .iter()
        .map(|line| {
            let time = line
                .at
                .map(termlet_core::time::format_log_time)
                .unwrap_or_default();
            (time, line)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(at: Option<i64>, text: &str, level: LogLevel) -> LogLine {
        LogLine {
            at,
            text: text.into(),
            level,
        }
    }

    fn sample() -> Vec<LogLine> {
        vec![
            line(Some(1), "app[7]: started", LogLevel::Normal),
            line(Some(2), "app[7]: ERROR boom", LogLevel::Error),
            line(None, "        at Main.run", LogLevel::Error),
            line(Some(3), "app[7]: Warning: slow", LogLevel::Warning),
        ]
    }

    fn time(ms: i64) -> String {
        format!("t{ms}")
    }

    /// 不筛选、显示时间：时间列宽度固定，后续行留空对齐；错误、警告行的范围只含内容。
    #[test]
    fn display_with_times() {
        let display = build_display(&sample(), &Filter::new(""), true, time);
        let lines: Vec<&str> = display.text.split('\n').collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "t1             app[7]: started");
        assert_eq!(lines[2], "                       at Main.run");
        assert_eq!(display.shown, 4);
        assert_eq!(display.times.len(), 3);
        assert_eq!(&display.text[display.times[1].clone()], "t2            ");
        let (range, level) = &display.levels[0];
        assert_eq!(&display.text[range.clone()], "app[7]: ERROR boom");
        assert_eq!(*level, LogLevel::Error);
        assert_eq!(display.levels.len(), 3);
    }

    /// 不显示时间；按关键词筛选（不区分大小写，几个词都要有），多行消息整条显示或不显示，命中处标出。
    #[test]
    fn filter_keeps_whole_messages() {
        let display = build_display(&sample(), &Filter::new("main"), false, time);
        assert_eq!(display.text, "app[7]: ERROR boom\n        at Main.run");
        assert_eq!(display.shown, 2);
        assert_eq!(display.matches.len(), 1);
        assert_eq!(&display.text[display.matches[0].clone()], "Main");
        assert!(display.times.is_empty());

        let display = build_display(&sample(), &Filter::new("APP slow"), false, time);
        assert_eq!(display.text, "app[7]: Warning: slow");
        let hits: Vec<&str> = display
            .matches
            .iter()
            .map(|r| &display.text[r.clone()])
            .collect();
        assert_eq!(hits, ["app", "slow"]);

        let display = build_display(&sample(), &Filter::new("nothing"), true, time);
        assert_eq!((display.text.as_str(), display.shown), ("", 0));
    }

    /// 命中的位置重叠时合并；正则的特殊字符按字面。
    #[test]
    fn filter_ranges_merge_and_are_literal() {
        let filter = Filter::new("ab bc");
        assert_eq!(filter.ranges("xabcx"), vec![1..4]);
        let filter = Filter::new("a.b");
        assert!(!filter.matches("axb"));
        assert!(filter.matches("A.B"));
    }

    /// 跟随时超过上限丢掉最早的。
    #[test]
    fn appended_lines_are_capped() {
        let mut lines = vec![line(Some(1), "a", LogLevel::Normal); MAX_KEPT - 1];
        let dropped = append_lines(&mut lines, vec![line(Some(2), "b", LogLevel::Normal); 3]);
        assert_eq!(dropped, 2);
        assert_eq!(lines.len(), MAX_KEPT);
        assert_eq!(lines.last().unwrap().text, "b");
    }

    /// 时间范围：相对的秒数，“今天”从本机的零点算起（至少 1 秒）。
    #[test]
    fn ranges_are_relative_seconds() {
        assert_eq!(RangeChoice::All.since_secs(0), None);
        assert_eq!(RangeChoice::Minutes15.since_secs(0), Some(900));
        assert_eq!(RangeChoice::Hour1.since_secs(0), Some(3600));
        let now = termlet_core::time::now_ms();
        let today = RangeChoice::Today.since_secs(now).unwrap();
        assert!((1..=86_400).contains(&today));
        assert_eq!(LinesChoice::All.lines(), LogLines::All);
        assert_eq!(LinesChoice::Last1000.lines(), LogLines::Last(1000));
    }
}
