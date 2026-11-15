//! 回放往前跳用的关键帧：重放时每送过一段输出（[`SPACING`] 起）就请引擎线程存一份引擎的全部内容与状态
//! （[`ReplayFeed::checkpoint`]，写成控制序列）。往前跳时新的引擎先解析不晚于目标的最近一份，再接着送之后的事件，
//! 不用从头重放；往后跳得很远、中间有关键帧时同样从关键帧开始（比把中间的输出都送一遍快）。
//!
//! - 只在控制序列的边界上要（[`at_ground`]：录制中被切开的转义序列接不上）；引擎在同步更新（DEC 2026）中途不给，
//!   这一份就没有，到下一段再要。
//! - 关键帧是一路放过去（播放、往后跳、从关键帧重放）时顺带存的：没放到过的位置没有，往前跳到那里之前的部分照样
//!   从最近的关键帧（或从头）重放。
//! - 内存：一份关键帧的大小与回滚中的内容相当（5000 行带颜色的日志约 0.5MB）。全部关键帧合计超过 [`BUDGET`] 时
//!   隔一份删一份，之后的间隔加倍，所以总是不超过它；从关键帧重放最多要解析一个间隔的输出。

use std::collections::BTreeSet;
use std::ops::Range;

use termlet_core::recording::cast::{Cast, EventData};
use termlet_terminal::{ReplayFeed, ScreenDump};

/// 两份关键帧之间至少隔这么多输出（字节；发布构建中解析 4MB 带颜色的输出约 30 毫秒）。
pub(super) const SPACING: usize = 4 << 20;
/// 全部关键帧合计的上限（字节）。
pub(super) const BUDGET: usize = 64 << 20;
/// 一次送进引擎的输出最多这么多（连续的小段输出合并后再送）。
const FEED_CHUNK: usize = 256 << 10;
/// 判断边界时最多往回看这么多输出：其中没有 ESC 等能打断转义序列的字节时，当作纯文字。
const GROUND_LOOKBACK: usize = 64 << 10;

/// 一份关键帧：前 `index` 个事件送进去之后引擎的样子。
pub(super) struct Keyframe {
    pub index: usize,
    pub dump: ScreenDump,
}

/// 一个回放的关键帧。
pub(super) struct Keyframes {
    /// 按位置排列
    frames: Vec<Keyframe>,
    /// 合计的大小
    bytes: usize,
    spacing: usize,
    budget: usize,
    /// 已经请引擎存、还没拿到的位置
    pending: BTreeSet<usize>,
    /// 上一份关键帧（已有的、请求了的，或开始重放的位置）之后送进引擎的输出字节数，与那个位置
    since: usize,
    since_index: usize,
}

impl Default for Keyframes {
    fn default() -> Self {
        Self::new(SPACING, BUDGET)
    }
}

impl Keyframes {
    pub(super) fn new(spacing: usize, budget: usize) -> Self {
        Self {
            frames: Vec::new(),
            bytes: 0,
            spacing: spacing.max(1),
            budget,
            pending: BTreeSet::new(),
            since: 0,
            since_index: 0,
        }
    }

    /// 不晚于第 `index` 个事件的最近一份（正好在 `index` 的也算）。
    pub(super) fn before(&self, index: usize) -> Option<&Keyframe> {
        let at = self.frames.partition_point(|k| k.index <= index);
        at.checked_sub(1).map(|i| &self.frames[i])
    }

    /// 从第 `index` 个事件开始往引擎里送（从头为 0，从关键帧重放时为它的位置）。
    pub(super) fn restart_at(&mut self, index: usize) {
        self.since = 0;
        self.since_index = index;
    }

    /// 拿到了 `index` 处的关键帧（引擎没给时为 `None`）。
    pub(super) fn finish(&mut self, index: usize, dump: Option<ScreenDump>) {
        self.pending.remove(&index);
        let Some(dump) = dump else {
            return;
        };
        let at = self.frames.partition_point(|k| k.index < index);
        if self.frames.get(at).is_some_and(|k| k.index == index) {
            return;
        }
        self.bytes += dump.text.len();
        self.frames.insert(at, Keyframe { index, dump });
        while self.bytes > self.budget && self.frames.len() > 1 {
            self.thin_out();
        }
        if self.bytes > self.budget {
            // 一份就超过上限（回滚极长）：不存
            self.frames.clear();
            self.bytes = 0;
        }
    }

    /// 隔一份删一份（留下第 1、3、5…… 份），之后的间隔加倍。
    fn thin_out(&mut self) {
        let mut keep = false;
        self.frames.retain(|_| {
            keep = !keep;
            keep
        });
        self.bytes = self.frames.iter().map(|k| k.dump.text.len()).sum();
        self.spacing = self.spacing.saturating_mul(2);
        tracing::debug!(
            frames = self.frames.len(),
            bytes = self.bytes,
            spacing = self.spacing,
            "回放的关键帧超过内存上限，隔一份删一份"
        );
    }

    /// 前 `index` 个事件送完时该不该在这里要一份：离上一份（已有的、请求了的）的输出够了间隔，并且在控制序列的
    /// 边界上。
    fn due(&mut self, cast: &Cast, index: usize) -> bool {
        if self.since < self.spacing {
            return false;
        }
        // 这一段里已经有（或正在要）一份：从它算起
        let existing = self
            .before(index)
            .map(|k| k.index)
            .into_iter()
            .chain(self.pending.range(..=index).next_back().copied())
            .filter(|&at| at > self.since_index)
            .max();
        if let Some(at) = existing {
            self.since = 0;
            self.since_index = at;
            return false;
        }
        at_ground(cast, index)
    }

    #[cfg(test)]
    pub(super) fn positions(&self) -> Vec<usize> {
        self.frames.iter().map(|k| k.index).collect()
    }

    pub(super) fn len(&self) -> usize {
        self.frames.len()
    }

    pub(super) fn bytes(&self) -> usize {
        self.bytes
    }
}

/// 等引擎存好的关键帧（[`ReplayFeed::checkpoint`]）。
pub(super) type Pending =
    std::pin::Pin<Box<dyn std::future::Future<Output = Option<ScreenDump>> + Send>>;

/// 把 `range` 中的事件送进引擎：连续的输出合并成块，大小变化按顺序夹在中间；输入、标记不影响画面。途中按间隔请
/// 引擎存关键帧（在请求的位置先把之前的输出送完），每请求一份调用 `requested(位置, 等它)`，结果交给
/// [`Keyframes::finish`]。
pub(super) fn feed_events(
    feed: &ReplayFeed,
    cast: &Cast,
    range: Range<usize>,
    keyframes: &mut Keyframes,
    mut requested: impl FnMut(usize, Pending),
) {
    let mut chunk = Vec::new();
    let start = range.start;
    for (offset, event) in cast
        .events
        .get(range)
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        match &event.data {
            EventData::Output(text) => {
                chunk.extend_from_slice(text.as_bytes());
                keyframes.since += text.len();
                if chunk.len() >= FEED_CHUNK {
                    feed.output(std::mem::take(&mut chunk));
                }
            }
            EventData::Resize { columns, rows } => {
                if !chunk.is_empty() {
                    feed.output(std::mem::take(&mut chunk));
                }
                feed.resize(*columns, *rows);
            }
            EventData::Input(_) | EventData::Marker(_) | EventData::Exit(_) => continue,
        }
        let index = start + offset + 1;
        if keyframes.due(cast, index) {
            if !chunk.is_empty() {
                feed.output(std::mem::take(&mut chunk));
            }
            keyframes.pending.insert(index);
            keyframes.since = 0;
            keyframes.since_index = index;
            requested(index, Box::pin(feed.checkpoint()));
        }
    }
    if !chunk.is_empty() {
        feed.output(chunk);
    }
}

/// `range` 中的事件一共有多少输出（字节）。
pub(super) fn output_bytes(cast: &Cast, range: Range<usize>) -> usize {
    cast.events
        .get(range)
        .unwrap_or_default()
        .iter()
        .map(|event| match &event.data {
            EventData::Output(text) => text.len(),
            _ => 0,
        })
        .sum()
}

/// 解析到哪种序列中了（只分辨序列在哪里结束，与 VT 解析器的状态机相同）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Parse {
    Ground,
    Escape,
    /// ESC 之后的中间字节（`ESC ( 0` 等）
    EscapeIntermediate,
    Csi,
    /// OSC：BEL 或 ST 结束
    Osc,
    /// DCS、SOS、PM、APC：ST 结束
    String,
}

impl Parse {
    fn step(self, byte: u8) -> Self {
        match byte {
            0x1b => return Parse::Escape,
            // CAN、SUB 在任何状态下都中止序列
            0x18 | 0x1a => return Parse::Ground,
            _ => {}
        }
        match self {
            Parse::Ground => Parse::Ground,
            Parse::Escape => match byte {
                0x20..=0x2f => Parse::EscapeIntermediate,
                b'[' => Parse::Csi,
                b']' => Parse::Osc,
                b'P' | b'X' | b'^' | b'_' => Parse::String,
                0x30..=0x7e => Parse::Ground,
                _ => Parse::Escape,
            },
            Parse::EscapeIntermediate => match byte {
                0x30..=0x7e => Parse::Ground,
                _ => Parse::EscapeIntermediate,
            },
            Parse::Csi => match byte {
                0x40..=0x7e => Parse::Ground,
                _ => Parse::Csi,
            },
            Parse::Osc if byte == 0x07 => Parse::Ground,
            Parse::Osc => Parse::Osc,
            Parse::String => Parse::String,
        }
    }
}

/// 前 `end` 个事件的输出送完时不在转义序列中间（关键帧要在这样的边界上存）。ESC、CAN、SUB 在任何状态下都重新开始，
/// 所以只要从最后一个这样的字节往后看；往回 [`GROUND_LOOKBACK`] 字节内都没有时当作纯文字。
pub(super) fn at_ground(cast: &Cast, end: usize) -> bool {
    let outputs = || {
        cast.events[..end.min(cast.events.len())]
            .iter()
            .rev()
            .filter_map(|event| match &event.data {
                EventData::Output(text) => Some(text.as_bytes()),
                _ => None,
            })
    };
    let mut seen = 0;
    let mut found = None;
    for (back, bytes) in outputs().enumerate() {
        if let Some(at) = bytes.iter().rposition(|b| matches!(b, 0x1b | 0x18 | 0x1a)) {
            found = Some((back, at));
            break;
        }
        seen += bytes.len();
        if seen >= GROUND_LOOKBACK {
            return true;
        }
    }
    let Some((back, at)) = found else {
        return true;
    };
    let tail: Vec<&[u8]> = outputs().take(back + 1).collect();
    let mut state = Parse::Ground;
    for (i, bytes) in tail.iter().rev().enumerate() {
        let from = if i == 0 { at } else { 0 };
        for &byte in &bytes[from..] {
            state = state.step(byte);
        }
    }
    state == Parse::Ground
}

#[cfg(test)]
mod tests {
    use termlet_core::recording::cast::{CastEvent, CastHeader};
    use termlet_terminal::{EngineOptions, GridSize, TerminalEngine};

    use super::*;

    fn cast_of(width: u16, height: u16, events: Vec<EventData>) -> Cast {
        Cast {
            version: 2,
            header: CastHeader {
                width,
                height,
                ..CastHeader::default()
            },
            events: events
                .into_iter()
                .enumerate()
                .map(|(i, data)| CastEvent {
                    time: i as f64 * 0.1,
                    data,
                })
                .collect(),
            skipped: 0,
        }
    }

    fn output(text: &str) -> EventData {
        EventData::Output(text.to_string())
    }

    // 边界：转义序列被切开时不在边界上，接上的那一段之后才是；OSC 等字符串序列要等结束；CAN 中止序列
    #[test]
    fn ground_follows_escape_sequences() {
        let cast = cast_of(
            10,
            3,
            vec![
                output("plain text"),
                output("\x1b[3"),
                output("1mred"),
                output("\x1b]0;title"),
                output(" still title\x07"),
                EventData::Resize {
                    columns: 20,
                    rows: 5,
                },
                output("\x1bP1$r"),
                output("data\x1b\\"),
                output("\x1b[?1"),
                output("\x18after"),
                output("\x1b"),
            ],
        );
        let at = |end| at_ground(&cast, end);
        assert!(at(0) && at(1));
        assert!(!at(2), "CSI 切开了");
        assert!(at(3));
        assert!(!at(4), "OSC 还没结束");
        assert!(at(5) && at(6), "BEL 结束 OSC；大小变化不影响");
        assert!(!at(7), "DCS 还没结束");
        assert!(at(8), "ST 结束 DCS");
        assert!(!at(9));
        assert!(at(10), "CAN 中止序列");
        assert!(!at(11), "最后是 ESC");
    }

    fn frame(len: usize) -> Option<ScreenDump> {
        Some(ScreenDump {
            text: "x".repeat(len),
            columns: 10,
            rows: 3,
        })
    }

    // 按位置排列，找不晚于目标的；超过上限时隔一份删一份、间隔加倍；引擎没给的不记
    #[test]
    fn keyframes_stay_within_budget() {
        let mut keyframes = Keyframes::new(100, 35);
        keyframes.finish(30, frame(10));
        keyframes.finish(10, frame(10));
        keyframes.finish(20, None);
        keyframes.finish(20, frame(10));
        assert_eq!(keyframes.positions(), [10, 20, 30]);
        assert_eq!(keyframes.before(25).map(|k| k.index), Some(20));
        assert_eq!(keyframes.before(20).map(|k| k.index), Some(20));
        assert!(keyframes.before(9).is_none());
        keyframes.finish(40, frame(10));
        assert_eq!(keyframes.positions(), [10, 30]);
        assert_eq!(keyframes.bytes(), 20);
        assert_eq!(keyframes.spacing, 200);
        keyframes.finish(50, frame(100));
        assert!(keyframes.bytes() <= 35);
    }

    /// 一段带颜色、改大小、备用屏幕与滚动区域、转义序列被切开的录制。
    fn busy_cast() -> Cast {
        let mut events = vec![output("\x1b[H\x1b[2J$ ")];
        for i in 0..60 {
            events.push(output(&format!("\x1b[3{}mline {i:03}\x1b[0m 中文 ", i % 8)));
            events.push(output("\r\n"));
            if i == 20 {
                events.push(EventData::Resize {
                    columns: 30,
                    rows: 6,
                });
            }
            if i == 30 {
                // 全屏程序：备用屏幕、滚动区域，一段序列切在两个事件里
                events.push(output("\x1b[?1049h\x1b[2;5r\x1b[1;1Htop\x1b[5;1H"));
                events.push(output("\nscroll\x1b["));
                events.push(output("32mgreen\x1b(0qqq\x1b(B"));
            }
            if i == 40 {
                events.push(output("\x1b[r\x1b[?1049l"));
            }
        }
        cast_of(24, 5, events)
    }

    fn engine(size: GridSize) -> (TerminalEngine, ReplayFeed) {
        TerminalEngine::replay(EngineOptions {
            scrollback: 200,
            size,
        })
    }

    /// 回滚与屏幕的全部文字。
    fn all_text(engine: &TerminalEngine) -> String {
        engine.buffer_text()
    }

    // 一路放下来时存的关键帧：从任一份接着送到目标位置，与从头放到那里相同
    #[test]
    fn restoring_from_keyframes_matches_continuous_playback() {
        let cast = busy_cast();
        let start = GridSize::new(cast.header.width, cast.header.height);
        let mut keyframes = Keyframes::new(64, BUDGET);
        let (_continuous, feed) = engine(start);
        let mut waits = Vec::new();
        feed_events(
            &feed,
            &cast,
            0..cast.events.len(),
            &mut keyframes,
            |index, wait| waits.push((index, wait)),
        );
        assert!(waits.len() > 5, "按间隔要了关键帧：{}", waits.len());
        for (index, wait) in waits {
            keyframes.finish(index, futures::executor::block_on(wait));
        }
        assert!(keyframes.len() > 5);
        for target in [10, 45, 70, 90, cast.events.len()] {
            let (expected, feed) = engine(start);
            feed_events(
                &feed,
                &cast,
                0..target,
                &mut Keyframes::new(usize::MAX, 0),
                |_, _| {},
            );
            feed.wait_idle();
            let keyframe = keyframes.before(target).expect("有关键帧");
            let (restored, feed) = engine(GridSize::new(keyframe.dump.columns, keyframe.dump.rows));
            feed.output(keyframe.dump.text.clone().into_bytes());
            feed_events(
                &feed,
                &cast,
                keyframe.index..target,
                &mut Keyframes::new(usize::MAX, 0),
                |_, _| {},
            );
            feed.wait_idle();
            assert_eq!(restored.size(), expected.size(), "到第 {target} 个事件");
            assert_eq!(
                all_text(&restored),
                all_text(&expected),
                "到第 {target} 个事件"
            );
        }
    }
}
