//! 回放的进度：事件在回放中的时间（可以跳过长停顿）、播放与暂停、倍速、跳转。只算“该放到第几个事件”，
//! 把事件交给终端由界面去做（见桌面端的回放 Tab）。
//!
//! - 时间轴（[`Timeline`]）：跳过长停顿时，两个事件之间超过上限的停顿缩短为上限（与 asciinema 的 `idle_time_limit`
//!   相同），之后的事件随之提前。
//! - 进度（[`Playhead`]）：位置是时间轴上的秒数；已交给终端的是前 `applied` 个事件。播放中由计时器按真实时间乘倍速
//!   前进（[`Playhead::advance`]）。跳转（[`Playhead::seek`]）往后时补上中间的事件；往前时终端回不去，要从头重放到
//!   目标位置（[`Seek::Restart`]）。

use std::ops::Range;
use std::time::Duration;

use super::cast::Cast;

/// 跳过长停顿、录制文件没有给出停顿上限时用的上限（秒）。
pub const DEFAULT_IDLE_LIMIT: f64 = 2.0;
/// 可选的倍速。
pub const SPEEDS: [f64; 3] = [1., 2., 4.];

/// 事件在回放中的时间。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Timeline {
    times: Vec<f64>,
}

impl Timeline {
    /// `idle_limit` 为停顿上限（秒）；`None` 时按录制的时间。
    pub fn new(cast: &Cast, idle_limit: Option<f64>) -> Self {
        let limit = idle_limit.filter(|l| l.is_finite() && *l > 0.);
        let mut times = Vec::with_capacity(cast.events.len());
        let (mut previous, mut clock) = (0f64, 0f64);
        for event in &cast.events {
            let gap = (event.time - previous).max(0.);
            previous = event.time;
            clock += match limit {
                Some(limit) => gap.min(limit),
                None => gap,
            };
            times.push(clock);
        }
        Self { times }
    }

    pub fn len(&self) -> usize {
        self.times.len()
    }

    pub fn is_empty(&self) -> bool {
        self.times.is_empty()
    }

    /// 总时长（最后一个事件的时间）。
    pub fn duration(&self) -> f64 {
        self.times.last().copied().unwrap_or(0.)
    }

    /// 第 `index` 个事件的时间。
    pub fn time_of(&self, index: usize) -> f64 {
        self.times.get(index).copied().unwrap_or(self.duration())
    }

    /// 到 `position`（含）为止有几个事件。
    pub fn count_at(&self, position: f64) -> usize {
        self.times.partition_point(|&t| t <= position)
    }
}

/// 跳转后要做的事。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seek {
    /// 往后：把这些事件接着交给终端
    Forward(Range<usize>),
    /// 往前：清空终端，从头交给它这些事件
    Restart(Range<usize>),
}

/// 回放的进度。
#[derive(Clone, Debug)]
pub struct Playhead {
    timeline: Timeline,
    position: f64,
    applied: usize,
    playing: bool,
    speed: f64,
}

impl Playhead {
    /// 从头开始，暂停着，1 倍速。
    pub fn new(timeline: Timeline) -> Self {
        Self {
            timeline,
            position: 0.,
            applied: 0,
            playing: false,
            speed: 1.,
        }
    }

    pub fn timeline(&self) -> &Timeline {
        &self.timeline
    }

    /// 现在的位置（秒）。
    pub fn position(&self) -> f64 {
        self.position
    }

    pub fn duration(&self) -> f64 {
        self.timeline.duration()
    }

    /// 已交给终端的事件数。
    pub fn applied(&self) -> usize {
        self.applied
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn speed(&self) -> f64 {
        self.speed
    }

    /// 已放到末尾。
    pub fn at_end(&self) -> bool {
        self.applied >= self.timeline.len() && self.position >= self.duration()
    }

    pub fn set_speed(&mut self, speed: f64) {
        if speed.is_finite() && speed > 0. {
            self.speed = speed;
        }
    }

    /// 开始播放。已在末尾时从头开始：返回从头重放要做的（清空终端）。
    pub fn play(&mut self) -> Option<Seek> {
        let restart = self.at_end().then(|| self.seek(0.));
        self.playing = true;
        restart
    }

    pub fn pause(&mut self) {
        self.playing = false;
    }

    /// 播放中过了真实时间 `elapsed`：位置前进 `elapsed × 倍速`，返回这期间到期、要交给终端的事件；
    /// 到末尾时停下。暂停时什么也不做。
    pub fn advance(&mut self, elapsed: Duration) -> Range<usize> {
        if !self.playing {
            return self.applied..self.applied;
        }
        let duration = self.duration();
        self.position = (self.position + elapsed.as_secs_f64() * self.speed).min(duration);
        let due = self.timeline.count_at(self.position);
        let range = self.applied..due.max(self.applied);
        self.applied = range.end;
        if self.position >= duration && self.applied >= self.timeline.len() {
            self.playing = false;
        }
        range
    }

    /// 离下一个事件还有多久（真实时间，已按倍速换算）；暂停或已没有下一个事件时为 `None`。
    pub fn until_next(&self) -> Option<Duration> {
        if !self.playing || self.applied >= self.timeline.len() {
            return None;
        }
        let gap = (self.timeline.time_of(self.applied) - self.position).max(0.) / self.speed;
        Some(Duration::from_secs_f64(gap))
    }

    /// 跳到 `position`（秒，限制在时间轴内）。
    pub fn seek(&mut self, position: f64) -> Seek {
        let position = if position.is_finite() {
            position.clamp(0., self.duration())
        } else {
            0.
        };
        let target = self.timeline.count_at(position);
        self.position = position;
        let seek = if target >= self.applied {
            Seek::Forward(self.applied..target)
        } else {
            Seek::Restart(0..target)
        };
        self.applied = target;
        seek
    }

    /// 换时间轴（切换跳过长停顿）：已交给终端的事件不变（画面不变），位置换算到新的时间轴上。
    pub fn set_timeline(&mut self, timeline: Timeline) {
        let (old_from, old_to) = self.gap_bounds(&self.timeline);
        let (new_from, new_to) = self.gap_bounds(&timeline);
        // 位置在上一个已放的事件与下一个事件之间：按在这段停顿中的比例换算
        let ratio = if old_to > old_from {
            ((self.position - old_from) / (old_to - old_from)).clamp(0., 1.)
        } else {
            0.
        };
        self.position = new_from + (new_to - new_from) * ratio;
        self.timeline = timeline;
    }

    /// 现在所在的停顿：上一个已放的事件与下一个事件的时间。
    fn gap_bounds(&self, timeline: &Timeline) -> (f64, f64) {
        let from = match self.applied {
            0 => 0.,
            n => timeline.time_of(n - 1),
        };
        let to = if self.applied < timeline.len() {
            timeline.time_of(self.applied)
        } else {
            from
        };
        (from, to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::cast::{CastEvent, EventData};

    fn cast(times: &[f64]) -> Cast {
        Cast {
            events: times
                .iter()
                .map(|&time| CastEvent {
                    time,
                    data: EventData::Output("x".into()),
                })
                .collect(),
            ..Cast::default()
        }
    }

    // 跳过长停顿：超过上限的停顿缩短为上限，之后的事件随之提前
    #[test]
    fn idle_limit_shortens_long_pauses() {
        let cast = cast(&[0.5, 1.0, 61.0, 61.5]);
        let full = Timeline::new(&cast, None);
        assert_eq!(full.duration(), 61.5);
        let skipped = Timeline::new(&cast, Some(2.0));
        assert_eq!(skipped.time_of(2), 3.0);
        assert_eq!(skipped.duration(), 3.5);
        assert_eq!(skipped.count_at(1.0), 2);
        assert_eq!(skipped.count_at(0.4), 0);
    }

    // 倍速：同样的真实时间放得更远；到末尾自动停下
    #[test]
    fn speed_scales_playback() {
        let mut head = Playhead::new(Timeline::new(&cast(&[1.0, 2.0, 3.0, 4.0]), None));
        assert!(head.play().is_none());
        assert_eq!(head.advance(Duration::from_millis(1500)), 0..1);
        assert_eq!(head.until_next(), Some(Duration::from_millis(500)));
        head.set_speed(4.);
        assert_eq!(head.until_next(), Some(Duration::from_millis(125)));
        assert_eq!(head.advance(Duration::from_millis(500)), 1..3);
        assert_eq!(head.position(), 3.5);
        assert_eq!(head.advance(Duration::from_secs(10)), 3..4);
        assert!(!head.is_playing() && head.at_end());
        // 在末尾再播放：从头开始
        assert_eq!(head.play(), Some(Seek::Restart(0..0)));
        assert!(head.is_playing());
        assert_eq!(head.position(), 0.);
    }

    // 往后跳补上中间的事件；往前跳从头重放到目标位置
    #[test]
    fn seek_forward_and_back() {
        let mut head = Playhead::new(Timeline::new(&cast(&[1.0, 2.0, 3.0, 4.0]), None));
        assert_eq!(head.seek(2.5), Seek::Forward(0..2));
        assert_eq!(head.seek(3.0), Seek::Forward(2..3));
        assert_eq!(head.seek(0.5), Seek::Restart(0..0));
        assert_eq!(head.seek(99.), Seek::Forward(0..4));
        assert_eq!(head.position(), 4.0);
        assert_eq!(head.seek(1.0), Seek::Restart(0..1));
        assert_eq!(head.seek(f64::NAN), Seek::Restart(0..0));
        // 暂停时不前进
        assert_eq!(head.advance(Duration::from_secs(5)), 0..0);
        assert_eq!(head.until_next(), None);
    }

    // 切换跳过长停顿：已放的事件不变，位置按在停顿中的比例换算
    #[test]
    fn switching_timeline_keeps_the_frame() {
        let cast = cast(&[1.0, 101.0]);
        let mut head = Playhead::new(Timeline::new(&cast, None));
        head.seek(51.0);
        assert_eq!(head.applied(), 1);
        head.set_timeline(Timeline::new(&cast, Some(2.0)));
        assert_eq!(head.applied(), 1);
        assert_eq!(head.position(), 2.0);
        assert_eq!(head.duration(), 3.0);
    }
}
