//! 更新下载的进度：已下载 / 总大小、速度与剩余时间的格式化（纯函数），以及按最近几秒的下载量算速度的
//! [`DownloadMeter`]。标题栏的提示与“关于”页共用。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use gpui_kit::SharedString;

use crate::sftp::list_model::format_size;
use crate::t;

/// 速度按最近这么久的下载量算（滑动平均）。
const SPEED_WINDOW: Duration = Duration::from_secs(5);
/// 下载开始这么久之后才显示剩余时间：刚开始的速度不准。
const ETA_AFTER: Duration = Duration::from_secs(3);
/// 至少隔这么久的两次读数才算速度（太短时偶然的快慢影响太大）。
const MIN_SPAN: Duration = Duration::from_millis(500);

const UNITS: [(&str, f64); 4] = [
    ("B", 1.),
    ("KB", 1024.),
    ("MB", 1024. * 1024.),
    ("GB", 1024. * 1024. * 1024.),
];

/// 按 `bytes` 的大小选单位（1024 进制）。
fn unit_for(bytes: u64) -> (&'static str, f64) {
    UNITS
        .iter()
        .rev()
        .find(|(_, size)| bytes as f64 >= *size)
        .copied()
        .unwrap_or(UNITS[0])
}

/// 按单位写一个数：字节写整数，其余保留一位小数。
fn in_unit(bytes: u64, (unit, size): (&str, f64)) -> String {
    if unit == "B" {
        bytes.to_string()
    } else {
        format!("{:.1}", bytes as f64 / size)
    }
}

/// 已下载 / 总大小：总大小已知时两个数用总大小的单位（“8.1 / 18.3 MB”），未知时只写已下载的（“8.1 MB”）。
pub(crate) fn format_progress(downloaded: u64, total: Option<u64>) -> String {
    match total {
        Some(total) => {
            let unit = unit_for(total);
            format!(
                "{} / {} {}",
                in_unit(downloaded, unit),
                in_unit(total, unit),
                unit.0
            )
        }
        None => format_size(downloaded),
    }
}

/// 速度（每秒字节数）：“2.4 MB/s”。
pub(crate) fn format_speed(bytes_per_second: f64) -> String {
    format!("{}/s", format_size(bytes_per_second.max(0.) as u64))
}

/// 剩余时间：不到一分钟写秒（至少 1 秒），不到一小时写分钟（向上取整），否则写小时与分钟。
pub(crate) fn format_eta(remaining: Duration) -> SharedString {
    let seconds = remaining.as_secs_f64().ceil().max(1.) as u64;
    if seconds < 60 {
        t!("lock_ui.update.eta_seconds", count = seconds)
    } else if seconds < 3600 {
        t!("lock_ui.update.eta_minutes", count = seconds.div_ceil(60))
    } else {
        t!(
            "lock_ui.update.eta_hours",
            hours = seconds / 3600,
            minutes = seconds % 3600 / 60
        )
    }
}

/// 下载速度与剩余时间：记下最近几秒的读数（时间与已下载的字节数），速度取这段时间的平均。
#[derive(Debug, Default)]
pub(crate) struct DownloadMeter {
    /// 这次下载开始的时间（第一次读数）
    started: Option<Instant>,
    samples: VecDeque<(Instant, u64)>,
}

impl DownloadMeter {
    /// 记下一次读数。已下载的字节数变少（重新下载）时从头算。
    pub(crate) fn record(&mut self, now: Instant, downloaded: u64) {
        if self
            .samples
            .back()
            .is_some_and(|&(_, last)| downloaded < last)
        {
            self.reset();
        }
        self.started.get_or_insert(now);
        self.samples.push_back((now, downloaded));
        // 留一个刚好在窗口之外的读数，速度覆盖整个窗口
        while self.samples.len() > 2
            && self
                .samples
                .get(1)
                .is_some_and(|&(at, _)| now.duration_since(at) >= SPEED_WINDOW)
        {
            self.samples.pop_front();
        }
    }

    /// 不在下载了：清空。
    pub(crate) fn reset(&mut self) {
        self.started = None;
        self.samples.clear();
    }

    /// 最近几秒的平均速度（每秒字节数）；读数还不够时为 `None`。
    pub(crate) fn speed(&self) -> Option<f64> {
        let (&(first_at, first), &(last_at, last)) = (self.samples.front()?, self.samples.back()?);
        let span = last_at.duration_since(first_at);
        (span >= MIN_SPAN).then(|| (last - first) as f64 / span.as_secs_f64())
    }

    /// 剩余时间：总大小已知、下载开始几秒之后、有速度时才有。
    pub(crate) fn eta(&self, total: Option<u64>) -> Option<Duration> {
        let total = total?;
        let started = self.started?;
        let &(last_at, downloaded) = self.samples.back()?;
        if last_at.duration_since(started) < ETA_AFTER {
            return None;
        }
        let speed = self.speed().filter(|speed| *speed > 0.)?;
        Some(Duration::from_secs_f64(
            total.saturating_sub(downloaded) as f64 / speed,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{self, Language};

    // 已下载 / 总大小用总大小的单位；速度写每秒；总大小未知时只写已下载的
    #[test]
    fn sizes_and_speed_are_readable() {
        let mb = 1024 * 1024;
        assert_eq!(
            format_progress(8 * mb + mb / 10, Some(18 * mb + 3 * mb / 10)),
            "8.1 / 18.3 MB"
        );
        assert_eq!(format_progress(300 * 1024, Some(18 * mb)), "0.3 / 18.0 MB");
        assert_eq!(format_progress(512, Some(900)), "512 / 900 B");
        assert_eq!(format_progress(5 * mb, None), "5.0 MB");
        assert_eq!(format_speed(2.4 * mb as f64), "2.4 MB/s");
        assert_eq!(format_speed(800.), "800 B/s");
    }

    // 剩余时间：秒（至少 1 秒）、分钟（向上取整）、小时与分钟
    #[test]
    fn eta_is_rounded_up() {
        let original = i18n::language();
        i18n::set_language(Language::ZhCn);
        assert_eq!(format_eta(Duration::from_millis(300)), "剩余约 1 秒");
        assert_eq!(format_eta(Duration::from_secs(42)), "剩余约 42 秒");
        assert_eq!(format_eta(Duration::from_secs(61)), "剩余约 2 分钟");
        assert_eq!(
            format_eta(Duration::from_secs(2 * 3600 + 5 * 60)),
            "剩余约 2 小时 5 分钟"
        );
        i18n::set_language(Language::En);
        assert_eq!(format_eta(Duration::from_secs(42)), "About 42 s left");
        i18n::set_language(original);
    }

    // 速度取最近几秒的平均；开始几秒内不给剩余时间；重新下载（字节数变少）时从头算
    #[test]
    fn meter_averages_recent_samples() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut meter = DownloadMeter::default();
        meter.record(at(0), 0);
        assert_eq!(meter.speed(), None, "只有一次读数");
        meter.record(at(1000), 1_000_000);
        assert_eq!(meter.speed(), Some(1_000_000.));
        assert_eq!(meter.eta(Some(10_000_000)), None, "开始不到几秒");
        meter.record(at(4000), 4_000_000);
        assert_eq!(meter.eta(Some(10_000_000)), Some(Duration::from_secs(6)));
        assert_eq!(meter.eta(None), None, "总大小未知");
        // 后来变快：窗口之外的读数丢掉，速度跟着变
        meter.record(at(9000), 14_000_000);
        meter.record(at(10_000), 16_000_000);
        assert_eq!(meter.speed(), Some(2_000_000.), "最近 6 秒的平均");
        meter.record(at(11_000), 100);
        assert_eq!(meter.speed(), None, "重新下载时从头算");
        meter.reset();
        assert_eq!(meter.eta(Some(1)), None);
    }
}
