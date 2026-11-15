//! 一次传输的状态记录与上报：状态变化立即上报，进度按间隔节流；速度按最近几秒估算。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::info::TransferInfo;

/// 上报的回调：不能阻塞。
pub(crate) type Report = Arc<dyn Fn(TransferInfo) + Send + Sync>;

/// 估算速度用的时间窗口，以及至少要有多长的样本才给出速度。
const SPEED_WINDOW: Duration = Duration::from_secs(3);
const SPEED_MIN_SPAN: Duration = Duration::from_millis(500);
const SPEED_SAMPLE: Duration = Duration::from_millis(100);

pub(crate) struct Tracker {
    state: Mutex<State>,
    every: Duration,
    report: Report,
}

struct State {
    info: TransferInfo,
    last_report: Option<Instant>,
    samples: VecDeque<(Instant, u64)>,
}

impl Tracker {
    pub(crate) fn new(info: TransferInfo, every: Duration, report: Report) -> Self {
        Self {
            state: Mutex::new(State {
                info,
                last_report: None,
                samples: VecDeque::new(),
            }),
            every,
            report,
        }
    }

    /// 当前状态的快照。
    pub(crate) fn info(&self) -> TransferInfo {
        self.state.lock().unwrap().info.clone()
    }

    /// 修改状态并上报：`force` 为假时（进度）按间隔节流。
    pub(crate) fn update(&self, force: bool, f: impl FnOnce(&mut TransferInfo)) {
        let info = {
            let mut state = self.state.lock().unwrap();
            f(&mut state.info);
            let now = Instant::now();
            let bytes = state.info.bytes;
            // 样本最多每 100 毫秒一个（进度可能每个数据块都调用一次）
            if state
                .samples
                .back()
                .is_none_or(|(at, _)| now.duration_since(*at) >= SPEED_SAMPLE)
            {
                state.samples.push_back((now, bytes));
            }
            while state
                .samples
                .front()
                .is_some_and(|(at, _)| now.duration_since(*at) > SPEED_WINDOW)
            {
                state.samples.pop_front();
            }
            if !force
                && state
                    .last_report
                    .is_some_and(|at| now.duration_since(at) < self.every)
            {
                return;
            }
            state.info.speed = match state.samples.front() {
                Some((at, first)) if now.duration_since(*at) >= SPEED_MIN_SPAN => {
                    let span = now.duration_since(*at).as_secs_f64();
                    (bytes.saturating_sub(*first) as f64 / span) as u64
                }
                _ => 0,
            };
            state.last_report = Some(now);
            state.info.clone()
        };
        (self.report)(info);
    }

    /// 当前文件的进度：`delta` 是这次新传输的字节数（ZModem 对方要求从前面重发时为负）。
    pub(crate) fn progress(&self, name: &str, file_bytes: u64, file_size: u64, delta: i64) {
        self.update(false, |i| {
            if i.file != name {
                i.file = name.to_string();
            }
            i.file_bytes = file_bytes;
            i.file_size = file_size;
            i.bytes = i.bytes.saturating_add_signed(delta);
        });
    }

    /// 下载完成一个文件或目录（本机的完整路径）。
    pub(crate) fn saved(&self, path: PathBuf) {
        self.update(true, |i| {
            i.files += 1;
            i.saved.push(path);
        });
    }

    /// 上传完成一个文件或目录（服务器上的名称）。
    pub(crate) fn uploaded(&self, name: &str) {
        self.update(true, |i| {
            i.files += 1;
            i.uploaded.push(name.to_string());
        });
    }

    /// 一个文件被跳过。
    pub(crate) fn skipped(&self, name: &str) {
        self.update(true, |i| i.skipped.push(name.to_string()));
    }

    /// 文件总数与总字节数（已知时）。
    pub(crate) fn set_total(&self, files: usize, bytes: u64) {
        self.update(true, |i| {
            i.total_files = files;
            i.total_bytes = bytes;
        });
    }
}
