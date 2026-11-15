//! 关键词高亮：按规则给终端输出中的关键词换前景色（错误、警告、IP 地址等）。
//!
//! - 规则按顺序匹配，匹配之间互不重叠，重叠时靠前的规则优先；一行最多 [`MAX_MATCHES_PER_LINE`] 处；
//!   空匹配跳过。设置页的预览与终端渲染用同一个 [`Highlighter::find_matches`]。
//! - 关键词按字面匹配。正则中的 `\b` 按 ASCII 的单词边界（与 JavaScript 正则相同："连接failed" 中的
//!   failed 也算整词），这样正则引擎在含中文的行上也能走快速路径。
//! - 先用合并的 `RegexSet` 判断一行命中了哪些规则，只对命中的规则逐个找位置；渲染时按逻辑行的文字缓存结果，
//!   行的内容不变就不再匹配。渲染时在可见区的快照（[`GridSnapshot`]）上匹配，不碰内核。
//! - 渲染时只给程序没有着色的文字（默认前景色、不反色）着色：一处匹配中有一个格子程序着过色就不改这一处
//!   （例如编译器已经标红的 error 保持原样）；全屏程序（备用屏幕）中不高亮。见 `element.rs`。

use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use regex::{Regex, RegexBuilder, RegexSet, RegexSetBuilder};

use crate::colors::Color;
use crate::grid::{CellFlags, GridPoint, GridSnapshot};
use crate::text::logical_lines_within;

/// 最多使用的规则数（设置中最多 50 条）。
pub const MAX_RULES: usize = 50;

/// 一行最多高亮这么多处，避免病态的正则或超长行拖慢渲染。
pub const MAX_MATCHES_PER_LINE: usize = 64;

/// 编译后的正则最大的大小（与查找相同），防止用户写出巨大的正则。
const REGEX_SIZE_LIMIT: usize = 1 << 20;

/// 高亮可选的颜色：终端配色中的颜色，具体取值按当前的配色（深浅配色各自的）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum HighlightColor {
    #[default]
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
}

impl HighlightColor {
    pub const ALL: [HighlightColor; 6] = [
        HighlightColor::Red,
        HighlightColor::Green,
        HighlightColor::Yellow,
        HighlightColor::Blue,
        HighlightColor::Magenta,
        HighlightColor::Cyan,
    ];

    /// 设置中保存的颜色名（red、green、yellow、blue、magenta、cyan，不区分大小写）；不认识时为 `None`。
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|c| c.name().eq_ignore_ascii_case(name.trim()))
    }

    pub fn name(self) -> &'static str {
        match self {
            HighlightColor::Red => "red",
            HighlightColor::Green => "green",
            HighlightColor::Yellow => "yellow",
            HighlightColor::Blue => "blue",
            HighlightColor::Magenta => "magenta",
            HighlightColor::Cyan => "cyan",
        }
    }

    /// 对应的 ANSI 普通色下标（1–6）。
    pub fn ansi_index(self) -> u8 {
        self.slot() as u8 + 1
    }

    /// 在 [`Self::ALL`] 中的位置（颜色表的下标）。
    pub fn slot(self) -> usize {
        self as usize
    }
}

/// 一条高亮规则（界面从设置转换而来）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HighlightRuleSpec {
    pub pattern: String,
    /// 按正则表达式匹配；否则按字面匹配
    pub regex: bool,
    pub case_sensitive: bool,
    pub color: HighlightColor,
    pub enabled: bool,
}

/// 规则的错误：正则写错了时的说明；关键词为空、规则已停用都不算错误（停用的规则也检查，方便在设置页提示）。
pub fn rule_error(rule: &HighlightRuleSpec) -> Option<String> {
    if rule.pattern.is_empty() {
        return None;
    }
    compile(rule).err().map(|e| e.to_string())
}

/// 规则实际使用的正则（不含大小写选项）。
fn rule_pattern(rule: &HighlightRuleSpec) -> String {
    if rule.regex {
        ascii_word_boundaries(&rule.pattern)
    } else {
        regex::escape(&rule.pattern)
    }
}

fn compile(rule: &HighlightRuleSpec) -> Result<Regex, regex::Error> {
    RegexBuilder::new(&rule_pattern(rule))
        .case_insensitive(!rule.case_sensitive)
        .size_limit(REGEX_SIZE_LIMIT)
        .build()
}

/// 字符类之外的 `\b` 换成 ASCII 的单词边界 `(?-u:\b)`（`\\b` 是反斜杠加 b，不换；`\b{start}` 等是 Rust 的写法，不换）。
fn ascii_word_boundaries(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 8);
    let mut chars = pattern.chars().peekable();
    // 字符类的嵌套层数；刚进入字符类时紧跟的 `]`（或 `^]`）是字面的
    let mut class = 0usize;
    let mut class_start = false;
    while let Some(c) = chars.next() {
        let at_class_start = std::mem::take(&mut class_start);
        match c {
            '\\' => match chars.next() {
                Some('b') if class == 0 && chars.peek() != Some(&'{') => out.push_str(r"(?-u:\b)"),
                Some(next) => {
                    out.push('\\');
                    out.push(next);
                }
                None => out.push('\\'),
            },
            '[' => {
                class += 1;
                class_start = true;
                out.push(c);
            }
            '^' if at_class_start => {
                class_start = true;
                out.push(c);
            }
            ']' if class > 0 && !at_class_start => {
                class -= 1;
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// 一条规则的错误（`index` 是它在传入的规则中的位置）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleError {
    pub index: usize,
    pub message: String,
}

/// 文本中的一处匹配：`range` 是字节范围。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HighlightMatch {
    pub range: Range<usize>,
    pub color: HighlightColor,
}

struct CompiledRule {
    regex: Regex,
    color: HighlightColor,
}

/// 编译好的高亮规则。规则不变时界面共用同一个（放在 `Arc` 中经 [`crate::TerminalStyle`] 传给各个终端）。
pub struct Highlighter {
    rules: Vec<CompiledRule>,
    /// 合并的规则：先判断一行命中了哪些（只有一条规则时不需要）
    set: Option<RegexSet>,
}

impl std::fmt::Debug for Highlighter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Highlighter({} 条规则)", self.rules.len())
    }
}

impl Highlighter {
    /// 编译规则：停用的、关键词为空的跳过；正则无效的跳过并返回错误（设置页显示）。最多用前 [`MAX_RULES`] 条。
    pub fn new(rules: &[HighlightRuleSpec]) -> (Self, Vec<RuleError>) {
        let mut compiled = Vec::new();
        let mut patterns = Vec::new();
        let mut errors = Vec::new();
        for (index, rule) in rules.iter().enumerate().take(MAX_RULES) {
            if !rule.enabled || rule.pattern.is_empty() {
                continue;
            }
            match compile(rule) {
                Ok(regex) => {
                    let pattern = rule_pattern(rule);
                    patterns.push(if rule.case_sensitive {
                        format!("(?:{pattern})")
                    } else {
                        format!("(?i:{pattern})")
                    });
                    compiled.push(CompiledRule {
                        regex,
                        color: rule.color,
                    });
                }
                Err(e) => errors.push(RuleError {
                    index,
                    message: e.to_string(),
                }),
            }
        }
        // 合并失败（例如太大）时逐条匹配
        let set = (compiled.len() > 1)
            .then(|| {
                RegexSetBuilder::new(&patterns)
                    .size_limit(REGEX_SIZE_LIMIT * 4)
                    .build()
                    .ok()
            })
            .flatten();
        (
            Self {
                rules: compiled,
                set,
            },
            errors,
        )
    }

    /// 没有可用的规则。
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// 可用的规则数。
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// 文本中的匹配：按位置排列、互不重叠，重叠时靠前的规则优先，最多 [`MAX_MATCHES_PER_LINE`] 处。
    pub fn find_matches(&self, text: &str) -> Vec<HighlightMatch> {
        let mut found: Vec<HighlightMatch> = Vec::new();
        if self.rules.is_empty() || text.is_empty() {
            return found;
        }
        let hits = self.set.as_ref().map(|set| set.matches(text));
        'rules: for (index, rule) in self.rules.iter().enumerate() {
            if hits.as_ref().is_some_and(|hits| !hits.matched(index)) {
                continue;
            }
            for m in rule.regex.find_iter(text) {
                let range = m.range();
                if range.is_empty()
                    || found
                        .iter()
                        .any(|f| range.start < f.range.end && range.end > f.range.start)
                {
                    continue;
                }
                found.push(HighlightMatch {
                    range,
                    color: rule.color,
                });
                if found.len() >= MAX_MATCHES_PER_LINE {
                    break 'rules;
                }
            }
        }
        found.sort_by_key(|m| m.range.start);
        found
    }
}

/// 渲染用的缓存：逻辑行的文字 → 匹配。每帧只留下这一帧用到的，内容变了的行下一帧就被丢掉。
#[derive(Default)]
pub(crate) struct HighlightCache {
    /// 缓存属于哪个高亮器（规则变了就清空）
    owner: usize,
    previous: HashMap<String, Rc<[HighlightMatch]>>,
    current: HashMap<String, Rc<[HighlightMatch]>>,
}

impl HighlightCache {
    fn matches(&mut self, highlighter: &Highlighter, text: String) -> Rc<[HighlightMatch]> {
        if let Some(found) = self.current.get(&text) {
            return found.clone();
        }
        let found = self
            .previous
            .remove(&text)
            .unwrap_or_else(|| highlighter.find_matches(&text).into());
        self.current.insert(text, found.clone());
        found
    }

    fn start_frame(&mut self, highlighter: &Arc<Highlighter>) {
        let owner = Arc::as_ptr(highlighter) as usize;
        if owner != self.owner {
            self.owner = owner;
            self.previous.clear();
            self.current.clear();
        }
        self.previous = std::mem::take(&mut self.current);
    }
}

/// 可见区中每个格子的高亮颜色（`rows × columns`，存 [`HighlightColor::slot`] + 1，0 表示不高亮）；
/// 没有要高亮的时为 `None`。快照要带上跨出可见区的逻辑行（最多上下各 `rows` 行，见 `TerminalEngine::snapshot`）。
pub(crate) fn visible_highlights(
    snapshot: &GridSnapshot,
    highlighter: &Arc<Highlighter>,
    cache: &mut HighlightCache,
    rows: usize,
    columns: usize,
) -> Option<Vec<u8>> {
    if highlighter.is_empty() || snapshot.modes().alt_screen || rows == 0 {
        return None;
    }
    cache.start_frame(highlighter);
    let offset = snapshot.display_offset() as i32;
    let top = -offset;
    let bottom = top + rows as i32 - 1;
    let mut cells: Option<Vec<u8>> = None;
    // 跨出可见区的逻辑行最多再读一屏
    for mut line in logical_lines_within(snapshot, top..=bottom, rows) {
        let text = std::mem::take(&mut line.text);
        let matches = cache.matches(highlighter, text);
        for m in matches.iter() {
            let Some(range) = line.points(m.range.start, m.range.end) else {
                continue;
            };
            let (start, end) = (*range.start(), *range.end());
            if !cells_in(snapshot, start, end).all(|p| is_plain(snapshot, p)) {
                continue;
            }
            let value = m.color.slot() as u8 + 1;
            for point in cells_in(snapshot, start, end) {
                let row = point.line + offset;
                if (0..rows as i32).contains(&row) && point.column < columns {
                    let cells = cells.get_or_insert_with(|| vec![0; rows * columns]);
                    cells[row as usize * columns + point.column] = value;
                }
            }
        }
    }
    cells
}

/// 从 `start` 到 `end`（含两端，按行优先）的格子。
fn cells_in(
    snapshot: &GridSnapshot,
    start: GridPoint,
    end: GridPoint,
) -> impl Iterator<Item = GridPoint> {
    let columns = snapshot.columns();
    (start.line..=end.line).flat_map(move |line| {
        let from = if line == start.line { start.column } else { 0 };
        let to = if line == end.line {
            end.column
        } else {
            columns.saturating_sub(1)
        };
        (from..=to).map(move |column| GridPoint::new(line, column))
    })
}

/// 程序没有着色的格子：默认前景色、不反色（宽字符的后半跟着前半，不看）。
fn is_plain(snapshot: &GridSnapshot, point: GridPoint) -> bool {
    snapshot.cell(point).is_some_and(|cell| {
        cell.is_spacer()
            || (cell.fg == Color::Foreground && !cell.flags.contains(CellFlags::INVERSE))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::kernel::Kernel as _;
    use crate::engine::mock_kernel;

    fn rule(pattern: &str, regex: bool, color: HighlightColor) -> HighlightRuleSpec {
        HighlightRuleSpec {
            pattern: pattern.into(),
            regex,
            case_sensitive: false,
            color,
            enabled: true,
        }
    }

    /// 与 core 的内置规则相同
    fn builtin() -> Highlighter {
        let (highlighter, errors) = Highlighter::new(&[
            rule(
                r"\b(error|errors|fail|failed|failure|fatal|panic|denied|refused|exception|critical)\b",
                true,
                HighlightColor::Red,
            ),
            rule(
                r"\b(warn|warning|warnings|deprecated)\b",
                true,
                HighlightColor::Yellow,
            ),
            rule(
                r"\b(ok|success|successful|succeeded|passed)\b",
                true,
                HighlightColor::Green,
            ),
            rule(
                r"\b(?:\d{1,3}\.){3}\d{1,3}(?:/\d{1,2})?\b",
                true,
                HighlightColor::Cyan,
            ),
        ]);
        assert!(errors.is_empty());
        highlighter
    }

    fn texts(text: &str, highlighter: &Highlighter) -> Vec<(String, HighlightColor)> {
        highlighter
            .find_matches(text)
            .into_iter()
            .map(|m| (text[m.range].to_string(), m.color))
            .collect()
    }

    #[test]
    fn builtin_rules_match_whole_words_case_insensitively() {
        let text = "ERROR: connect 10.0.0.12 refused; Warning ok errorless";
        assert_eq!(
            texts(text, &builtin()),
            vec![
                ("ERROR".into(), HighlightColor::Red),
                ("10.0.0.12".into(), HighlightColor::Cyan),
                ("refused".into(), HighlightColor::Red),
                ("Warning".into(), HighlightColor::Yellow),
                ("ok".into(), HighlightColor::Green),
            ]
        );
        // ASCII 的单词边界：紧挨着中文也算整词
        assert_eq!(
            texts("连接failed，状态ok", &builtin()),
            vec![
                ("failed".into(), HighlightColor::Red),
                ("ok".into(), HighlightColor::Green),
            ]
        );
    }

    #[test]
    fn keywords_are_literal_case_option_and_invalid_rules_are_skipped() {
        let mut strict = rule("Timeout", false, HighlightColor::Magenta);
        strict.case_sensitive = true;
        let mut disabled = rule("x", false, HighlightColor::Red);
        disabled.enabled = false;
        let (highlighter, errors) = Highlighter::new(&[
            rule("a.b", false, HighlightColor::Blue),
            strict,
            disabled,
            rule("(", true, HighlightColor::Red),
            rule("", true, HighlightColor::Red),
        ]);
        assert_eq!(highlighter.len(), 2);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].index, 3);
        let text = "axb a.b timeout Timeout";
        assert_eq!(
            texts(text, &highlighter),
            vec![
                ("a.b".into(), HighlightColor::Blue),
                ("Timeout".into(), HighlightColor::Magenta),
            ]
        );
        // 单条规则的校验
        assert!(rule_error(&rule("(", true, HighlightColor::Red)).is_some());
        assert!(
            rule_error(&rule("(", false, HighlightColor::Red)).is_none(),
            "关键词按字面"
        );
        assert!(
            rule_error(&rule("", true, HighlightColor::Red)).is_none(),
            "空的不算错误"
        );
        assert!(rule_error(&rule(r"\d+", true, HighlightColor::Red)).is_none());
    }

    #[test]
    fn earlier_rules_win_and_empty_matches_are_skipped() {
        let (highlighter, _) = Highlighter::new(&[
            rule("failed", false, HighlightColor::Red),
            rule("led", false, HighlightColor::Green),
            rule("z*", true, HighlightColor::Blue),
        ]);
        assert_eq!(
            highlighter.find_matches("failed led"),
            vec![
                HighlightMatch {
                    range: 0..6,
                    color: HighlightColor::Red
                },
                HighlightMatch {
                    range: 7..10,
                    color: HighlightColor::Green
                },
            ]
        );
    }

    #[test]
    fn matches_per_line_are_limited() {
        let (highlighter, _) = Highlighter::new(&[rule("a", false, HighlightColor::Red)]);
        assert_eq!(
            highlighter.find_matches(&"a ".repeat(100)).len(),
            MAX_MATCHES_PER_LINE
        );
    }

    #[test]
    fn word_boundary_translation() {
        assert_eq!(ascii_word_boundaries(r"\bx\b"), r"(?-u:\b)x(?-u:\b)");
        assert_eq!(ascii_word_boundaries(r"\\b"), r"\\b");
        assert_eq!(ascii_word_boundaries(r"[\b]\b"), r"[\b](?-u:\b)");
        assert_eq!(ascii_word_boundaries(r"[]\b]\b"), r"[]\b](?-u:\b)");
        assert_eq!(ascii_word_boundaries(r"\b{start}x"), r"\b{start}x");
    }

    #[test]
    fn color_names() {
        assert_eq!(
            HighlightColor::from_name("Cyan"),
            Some(HighlightColor::Cyan)
        );
        assert_eq!(HighlightColor::from_name("purple"), None);
        assert_eq!(
            HighlightColor::ALL.map(HighlightColor::ansi_index),
            [1, 2, 3, 4, 5, 6]
        );
        for color in HighlightColor::ALL {
            assert_eq!(HighlightColor::from_name(color.name()), Some(color));
        }
    }

    #[test]
    fn visible_cells_skip_colored_text_and_follow_wrapping() {
        // mock：\n 是自动换行；第二行的 error 被程序标成了红色
        let mut snapshot = GridSnapshot::default();
        mock_kernel("a big err\nor here\r\nerror red").snapshot(&mut snapshot, 3);
        let highlighter = Arc::new(builtin());
        let mut cache = HighlightCache::default();
        let columns = snapshot.columns();
        // 程序着色的格子
        snapshot.cell_mut(GridPoint::new(2, 0)).unwrap().fg = Color::Indexed(1);
        let cells = visible_highlights(&snapshot, &highlighter, &mut cache, 3, columns).unwrap();
        let at = |line: usize, column: usize| cells[line * columns + column];
        let red = HighlightColor::Red.slot() as u8 + 1;
        // 折行的 "error" 跨两行
        assert_eq!(
            (at(0, 6), at(0, 8), at(1, 0), at(1, 1)),
            (red, red, red, red)
        );
        assert_eq!(at(0, 5), 0);
        assert_eq!(at(1, 2), 0);
        assert_eq!(at(2, 0), 0, "程序着过色的不改");
        assert_eq!(at(2, 4), 0);
        // 第二帧命中缓存
        assert_eq!(cache.current.len(), 2);
        let again = visible_highlights(&snapshot, &highlighter, &mut cache, 3, columns).unwrap();
        assert_eq!(again, cells);
    }
}

/// 渲染的测量：`cargo test -p termlet-terminal --release highlight_frame -- --ignored --nocapture`
#[cfg(test)]
mod bench {
    use super::*;
    use crate::engine::kernel::Kernel as _;
    use crate::engine::test_kernel;
    use std::time::Instant;

    fn rules(extra: usize) -> Vec<HighlightRuleSpec> {
        let rule = |pattern: &str, regex: bool, color| HighlightRuleSpec {
            pattern: pattern.into(),
            regex,
            case_sensitive: false,
            color,
            enabled: true,
        };
        let mut rules = vec![
            rule(
                r"\b(error|errors|fail|failed|failure|fatal|panic|denied|refused|exception|critical)\b",
                true,
                HighlightColor::Red,
            ),
            rule(
                r"\b(warn|warning|warnings|deprecated)\b",
                true,
                HighlightColor::Yellow,
            ),
            rule(
                r"\b(ok|success|successful|succeeded|passed)\b",
                true,
                HighlightColor::Green,
            ),
            rule(
                r"\b(?:\d{1,3}\.){3}\d{1,3}(?:/\d{1,2})?\b",
                true,
                HighlightColor::Cyan,
            ),
        ];
        for i in 0..extra {
            if i % 2 == 0 {
                rules.push(rule(&format!("keyword{i}"), false, HighlightColor::Blue));
            } else {
                rules.push(rule(
                    &format!(r"\bhost-{i}-\d+\b"),
                    true,
                    HighlightColor::Magenta,
                ));
            }
        }
        rules
    }

    /// 200 列 50 行的一屏日志（中英文混排，部分行有关键词）的快照。
    fn screen(seed: usize) -> GridSnapshot {
        let mut term = test_kernel(200, 50, 10_000);
        let mut text = String::new();
        for i in 0..50 {
            let n = i + seed;
            let tail = match n % 4 {
                0 => "ERROR 连接 10.0.0.12 被拒绝 (connection refused)",
                1 => "WARN 配置项已弃用 deprecated",
                _ => "INFO 服务启动完成",
            };
            text.push_str(&format!(
                "2026-10-15 12:00:{:02} [{n:05}] {tail} request_id=0123456789abcdef path=/api/v1/hosts/{n} status=200\r\n",
                n % 60
            ));
        }
        term.advance(text.trim_end().as_bytes());
        let mut snapshot = GridSnapshot::default();
        term.snapshot(&mut snapshot, 50);
        snapshot
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        /// 每帧内容都不同、也不用缓存（最坏）
        Uncached,
        /// 输出滚动：每帧比上一帧多一行新内容
        Scrolling,
        /// 内容不变，只是重绘
        Unchanged,
    }

    fn per_frame(highlighter: &Arc<Highlighter>, mode: Mode) -> f64 {
        let frames = 200;
        let terms: Vec<_> = (0..frames).map(|i| screen(i * 50)).collect();
        let scrolling: Vec<_> = (0..frames).map(screen).collect();
        let mut cache = HighlightCache::default();
        let started = Instant::now();
        for i in 0..frames {
            let term = match mode {
                Mode::Uncached => {
                    cache = HighlightCache::default();
                    &terms[i]
                }
                Mode::Scrolling => &scrolling[i],
                Mode::Unchanged => &terms[0],
            };
            std::hint::black_box(visible_highlights(term, highlighter, &mut cache, 50, 200));
        }
        started.elapsed().as_secs_f64() * 1000.0 / frames as f64
    }

    #[test]
    #[ignore = "耗时测量，手动运行"]
    fn highlight_frame() {
        for extra in [0, 46] {
            let (highlighter, errors) = Highlighter::new(&rules(extra));
            assert!(errors.is_empty());
            let highlighter = Arc::new(highlighter);
            let mut without_set = Highlighter::new(&rules(extra)).0;
            without_set.set = None;
            let without_set = Arc::new(without_set);
            println!(
                "{} 条规则，每帧（200×50）：不缓存 {:.3} ms（不合并成 RegexSet {:.3} ms），输出滚动 {:.3} ms，内容不变 {:.3} ms",
                highlighter.len(),
                per_frame(&highlighter, Mode::Uncached),
                per_frame(&without_set, Mode::Uncached),
                per_frame(&highlighter, Mode::Scrolling),
                per_frame(&highlighter, Mode::Unchanged),
            );
        }
    }
}
