//! 资源监控的纯逻辑，状态条（[`crate::monitor_bar`]）与主机工具的概览页共用：使用率的分级（带回差，不在阈值附近来回
//! 闪烁）、最满的磁盘、最近约 10 分钟的历史（环形缓冲，每个终端一份）、状态条按宽度收起哪几项、数值的写法。

use std::collections::VecDeque;

use gpui_kit::SharedString;
use termlet_core::monitor::{Disk, NetIface, SAMPLE_INTERVAL, Sample, is_virtual_iface};

use crate::t;

// ------------------------------------------------------------ 分级

/// 使用率的级别。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    #[default]
    Normal,
    Warning,
    Danger,
}

/// 分级的阈值：达到进入线才升级，降到退出线以下才降级（回差）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thresholds {
    pub warning: f64,
    pub warning_exit: f64,
    pub danger: f64,
    pub danger_exit: f64,
}

/// CPU、内存、磁盘的使用率（%）：≥75 警告、降到 70 以下退出；≥90 危险、降到 85 以下退出。
pub const PERCENT: Thresholds = Thresholds {
    warning: 75.,
    warning_exit: 70.,
    danger: 90.,
    danger_exit: 85.,
};

/// 1 分钟平均负载与核数之比：≥1 警告、降到 0.9 以下退出；≥2 危险、降到 1.8 以下退出。
pub const LOAD_RATIO: Thresholds = Thresholds {
    warning: 1.,
    warning_exit: 0.9,
    danger: 2.,
    danger_exit: 1.8,
};

impl Level {
    /// 按上一次的级别与这次的值得出新的级别。
    pub fn next(self, value: f64, t: Thresholds) -> Level {
        if value >= t.danger || (self == Level::Danger && value >= t.danger_exit) {
            Level::Danger
        } else if value >= t.warning || (self != Level::Normal && value >= t.warning_exit) {
            Level::Warning
        } else {
            Level::Normal
        }
    }

    /// 不看上一次的级别（概览页中其余磁盘、各核的进度条）。
    pub fn of(value: f64, t: Thresholds) -> Level {
        Level::Normal.next(value, t)
    }

    pub fn is_alert(self) -> bool {
        self != Level::Normal
    }
}

/// 一个终端状态条上各项的级别：每次采样后更新（记在终端上，切换 Tab 不丢）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Levels {
    pub cpu: Level,
    pub memory: Level,
    /// 最满的磁盘
    pub disk: Level,
    pub load: Level,
}

impl Levels {
    pub fn update(&mut self, sample: &Sample) {
        if let Some(cpu) = sample.cpu {
            self.cpu = self.cpu.next(cpu, PERCENT);
        }
        self.memory = self
            .memory
            .next(percent(sample.mem_used, sample.mem_total), PERCENT);
        self.disk = match fullest_disk(&sample.disks) {
            Some(disk) => self.disk.next(disk_percent(disk), PERCENT),
            None => Level::Normal,
        };
        self.load = match load_ratio(sample) {
            Some(ratio) => self.load.next(ratio, LOAD_RATIO),
            None => Level::Normal,
        };
    }
}

/// 1 分钟平均负载与核数之比；不知道核数时为 `None`。
pub fn load_ratio(sample: &Sample) -> Option<f64> {
    (sample.cores > 0).then(|| sample.load[0] / f64::from(sample.cores))
}

// ------------------------------------------------------------ 磁盘与网卡

pub fn percent(used: u64, total: u64) -> f64 {
    if total == 0 {
        0.
    } else {
        100. * used as f64 / total as f64
    }
}

pub fn disk_percent(disk: &Disk) -> f64 {
    percent(disk.used, disk.total)
}

/// 最满的磁盘（占用比例最高；一样时取前面的，根目录在最前）。采样只列出真实的设备与根目录（见核心的 `monitor`），
/// tmpfs、overlay（容器的根目录除外）、loop 设备、光驱不在其中。
pub fn fullest_disk(disks: &[Disk]) -> Option<&Disk> {
    disks
        .iter()
        .fold(None, |best: Option<&Disk>, disk| match best {
            Some(best) if disk_percent(best) >= disk_percent(disk) => Some(best),
            _ => Some(disk),
        })
}

/// 按占用从高到低排（一样时保持原来的顺序）。
pub fn disks_by_usage(disks: &[Disk]) -> Vec<&Disk> {
    let mut sorted: Vec<&Disk> = disks.iter().collect();
    sorted.sort_by(|a, b| disk_percent(b).total_cmp(&disk_percent(a)));
    sorted
}

/// 概览页默认列出的网卡：不含虚拟网卡（veth、网桥等）与从没有收发过的网卡；`all` 时全部列出。
/// 返回列出的网卡与隐藏了几个。
pub fn shown_ifaces(net: &[NetIface], all: bool) -> (Vec<&NetIface>, usize) {
    let shown: Vec<&NetIface> = net
        .iter()
        .filter(|iface| {
            all || !(is_virtual_iface(&iface.name) || iface.rx_total == 0 && iface.tx_total == 0)
        })
        .collect();
    let hidden = net.len() - shown.len();
    (shown, hidden)
}

// ------------------------------------------------------------ 历史

/// 历史中的一次采样。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    /// 采样时间（Unix 毫秒）
    pub at: i64,
    pub cpu: Option<f64>,
    /// 内存使用率（%）
    pub memory: f64,
    /// 收发速率（字节 / 秒，不含虚拟网卡）
    pub rx: Option<f64>,
    pub tx: Option<f64>,
}

/// 状态条网速小柱纵轴上端的下限（字节 / 秒）。
pub const SPARK_FLOOR: f64 = 32. * 1024.;

/// 相邻两次采样相隔超过它时，中间断开（终端不在前台、窗口最小化、连接断开期间没有采样，如实留空，不连线）。
/// 采样每 3 秒一次、上一次返回后再计时，再加上执行的时间，留 3 倍的余量。
pub const GAP_MS: i64 = 3 * SAMPLE_INTERVAL.as_millis() as i64;

/// 最近约 10 分钟的采样：每个终端一份（切换 Tab 再回来仍在，重连后接着记），最多 [`History::CAPACITY`] 个点。
#[derive(Clone, Debug, Default)]
pub struct History {
    points: VecDeque<Point>,
}

impl History {
    /// 10 分钟、每 3 秒一次
    pub const CAPACITY: usize = 200;
    /// 保留的时长（毫秒）
    pub const SPAN_MS: i64 = 10 * 60 * 1000;

    pub fn push(&mut self, sample: &Sample) {
        // 时间倒退（系统时间被改）时之前的点没法再按时间排：从头记
        if self.points.back().is_some_and(|last| last.at >= sample.at) {
            self.points.clear();
        }
        self.points.push_back(Point {
            at: sample.at,
            cpu: sample.cpu,
            memory: percent(sample.mem_used, sample.mem_total),
            rx: sample.rx_rate,
            tx: sample.tx_rate,
        });
        while self.points.len() > Self::CAPACITY
            || self
                .points
                .front()
                .is_some_and(|first| sample.at - first.at > Self::SPAN_MS)
        {
            self.points.pop_front();
        }
    }

    pub fn points(&self) -> &VecDeque<Point> {
        &self.points
    }

    pub fn last_at(&self) -> Option<i64> {
        self.points.back().map(|p| p.at)
    }

    /// 状态条网速小柱：从最新的采样往前 `slots` 次采样（约 3 秒一次），每格是收发速率的合计相对纵轴上端的高度（0–1）。
    /// 上端是这几格中最大的按 [`rate_ceiling`] 取整，且至少 [`SPARK_FLOOR`]：SSH 连接本身与采样每秒就有一两 KB，
    /// 空闲时近乎一条平线，有明显的流量才升起来。没有采样的时段（相隔超过 [`GAP_MS`]）按缺了几次留出空格（`None`），
    /// 不插值；不够 `slots` 格时左边补空格。
    pub fn spark(&self, slots: usize) -> Vec<Option<f32>> {
        let interval = SAMPLE_INTERVAL.as_millis() as f64;
        let mut bars: Vec<Option<f64>> = Vec::with_capacity(slots);
        let mut newer: Option<i64> = None;
        for point in self.points.iter().rev() {
            if bars.len() >= slots {
                break;
            }
            if let Some(newer) = newer
                && newer - point.at > GAP_MS
            {
                let missing = ((newer - point.at) as f64 / interval).round() as usize - 1;
                for _ in 0..missing.min(slots - bars.len()) {
                    bars.push(None);
                }
                if bars.len() >= slots {
                    break;
                }
            }
            let rate = match (point.rx, point.tx) {
                (None, None) => None,
                (rx, tx) => Some(rx.unwrap_or(0.) + tx.unwrap_or(0.)),
            };
            bars.push(rate);
            newer = Some(point.at);
        }
        bars.resize(slots, None);
        bars.reverse();
        let peak = bars.iter().flatten().fold(0_f64, |max, v| max.max(*v));
        let max = rate_ceiling(peak).max(SPARK_FLOOR);
        bars.into_iter()
            .map(|bar| bar.map(|v| (v / max).clamp(0., 1.) as f32))
            .collect()
    }
}

/// 一条曲线的各段（每点为时间与值）：`value` 取不到值的点、相隔超过 [`GAP_MS`] 的两点之间断开。
pub fn segments<'a>(
    points: impl IntoIterator<Item = &'a Point>,
    value: impl Fn(&Point) -> Option<f64>,
) -> Vec<Vec<(i64, f64)>> {
    let mut segments: Vec<Vec<(i64, f64)>> = Vec::new();
    let mut current: Vec<(i64, f64)> = Vec::new();
    for point in points {
        let gap = current.last().is_some_and(|(at, _)| point.at - at > GAP_MS);
        match value(point) {
            Some(v) if !gap => current.push((point.at, v)),
            Some(v) => {
                segments.push(std::mem::take(&mut current));
                current.push((point.at, v));
            }
            None => segments.push(std::mem::take(&mut current)),
        }
    }
    segments.push(current);
    segments.retain(|s| !s.is_empty());
    segments
}

// ------------------------------------------------------------ 状态条的取舍

/// 状态条上的一项，按优先级从高到低：宽度不够时从后面的开始收起。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BarItem {
    Latency,
    Cpu,
    Memory,
    Disk,
    Net,
    Load,
}

impl BarItem {
    pub fn key(self) -> &'static str {
        match self {
            BarItem::Latency => "latency",
            BarItem::Cpu => "cpu",
            BarItem::Memory => "memory",
            BarItem::Disk => "disk",
            BarItem::Net => "net",
            BarItem::Load => "load",
        }
    }
}

/// 状态条显示哪几项：前 `count` 项，是否带进度条（与网速的小折线）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fit {
    pub count: usize,
    pub meters: bool,
}

/// 按量出的宽度决定显示哪几项：`widths` 是各项（按优先级）带进度条与不带时的宽度，`gap` 是项之间的间距。
/// 全部带进度条放得下时都带；放不下时先都收成纯数字，再从优先级最低的一项起逐项收起。
pub fn fit(widths: &[(f32, f32)], available: f32, gap: f32) -> Fit {
    let total = |n: usize, full: bool| -> f32 {
        let items: f32 = widths[..n]
            .iter()
            .map(|(with, without)| if full { *with } else { *without })
            .sum();
        items + gap * n.saturating_sub(1) as f32
    };
    let n = widths.len();
    if total(n, true) <= available {
        return Fit {
            count: n,
            meters: true,
        };
    }
    let count = (0..=n)
        .rev()
        .find(|&k| total(k, false) <= available)
        .unwrap_or(0);
    Fit {
        count,
        meters: false,
    }
}

// ------------------------------------------------------------ 数值的写法

/// 字节数：不足 1 KB 为 `N B`，其余按 1024 进位、保留 1 位小数。
pub fn format_size(bytes: f64) -> String {
    if !bytes.is_finite() || bytes < 0. {
        return "-".into();
    }
    if bytes < 1024. {
        return format!("{bytes:.0} B");
    }
    let units = ["KB", "MB", "GB", "TB"];
    let mut value = bytes / 1024.;
    let mut unit = 0;
    while value >= 1024. && unit < units.len() - 1 {
        value /= 1024.;
        unit += 1;
    }
    format!("{value:.1} {}", units[unit])
}

/// 每秒字节数；没有读数时为 `-`。
pub fn format_rate(rate: Option<f64>) -> String {
    match rate {
        Some(rate) => format!("{}/s", format_size(rate)),
        None => "-".into(),
    }
}

/// 状态条上的网速：K、M、G（按 1024 进位）保留 1 位小数，不写 /s（`0.8K`、`12.3M`）；没有读数时为 `-`。
/// 不到 999.95 的数用这一级，再大进一级，最多 6 个字符（`999.9K`）。
pub fn short_rate(rate: Option<f64>) -> String {
    let Some(rate) = rate.filter(|r| r.is_finite() && *r >= 0.) else {
        return "-".into();
    };
    let mut value = rate / 1024.;
    for unit in ["K", "M", "G"] {
        if value < 999.95 {
            return format!("{value:.1}{unit}");
        }
        value /= 1024.;
    }
    format!("{value:.1}T")
}

/// 网速曲线纵轴的上端：不小于 `peak` 的 1、2、5 × 10ⁿ（按 1024 进位的单位，KB/s、MB/s…），至少 1 KB/s。
pub fn rate_ceiling(peak: f64) -> f64 {
    let peak = if peak.is_finite() { peak.max(0.) } else { 0. };
    let mut unit = 1024_f64;
    while peak > unit * 1000. && unit < 1024_f64.powi(4) {
        unit *= 1024.;
    }
    let value = peak / unit;
    let mut step = 1.;
    loop {
        for factor in [1., 2., 5.] {
            if factor * step >= value {
                return factor * step * unit;
            }
        }
        step *= 10.;
    }
}

/// 取整的百分比；没有读数时为 `-`。
pub fn whole_percent(value: Option<f64>) -> String {
    match value {
        Some(value) => format!("{value:.0}%"),
        None => "-".into(),
    }
}

/// 负载与核数：`0.4/16`（负载保留 1 位小数）；不知道核数时只有负载。
pub fn load_text(load: f64, cores: u32) -> String {
    if cores == 0 {
        format!("{load:.1}")
    } else {
        format!("{load:.1}/{cores}")
    }
}

/// 运行时间：取最大的两级（天与小时、小时与分钟，或只有分钟）。
pub fn format_uptime(seconds: f64) -> SharedString {
    let total = seconds.max(0.) as u64;
    let (days, hours, minutes) = (total / 86_400, total % 86_400 / 3600, total % 3600 / 60);
    if days > 0 {
        t!("monitor.uptime_days", days = days, hours = hours)
    } else if hours > 0 {
        t!("monitor.uptime_hours", hours = hours, minutes = minutes)
    } else {
        t!("monitor.uptime_minutes", minutes = minutes)
    }
}

/// 测试用的一次采样：4 核、CPU 10%、内存 40%、没有磁盘与网卡，`at` 为采样时间。
#[cfg(test)]
pub(crate) fn test_sample(at: i64) -> Sample {
    Sample {
        cpu: Some(10.),
        iowait: None,
        steal: None,
        per_core: Vec::new(),
        cores: 4,
        load: [0.5, 0.4, 0.3],
        uptime: 100.,
        mem_total: 100,
        mem_used: 40,
        swap_total: 0,
        swap_used: 0,
        disks: Vec::new(),
        net: Vec::new(),
        rx_rate: Some(1000.),
        tx_rate: Some(24.),
        at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(at: i64) -> Sample {
        test_sample(at)
    }

    fn disk(mount: &str, used: u64) -> Disk {
        Disk {
            mount: mount.into(),
            filesystem: "/dev/sda1".into(),
            total: 100,
            used,
        }
    }

    /// 回差：75 进入警告，降到 70 以下才退出；90 进入危险，降到 85 以下才退出（落回警告或正常）。
    #[test]
    fn levels_have_hysteresis() {
        let steps = [
            (74., Level::Normal),
            (75., Level::Warning),
            (71., Level::Warning),
            (69.9, Level::Normal),
            (89., Level::Warning),
            (90., Level::Danger),
            (86., Level::Danger),
            (84., Level::Warning),
            (95., Level::Danger),
            (50., Level::Normal),
        ];
        let mut level = Level::Normal;
        for (value, expected) in steps {
            level = level.next(value, PERCENT);
            assert_eq!(level, expected, "{value}");
        }
        assert_eq!(Level::of(80., PERCENT), Level::Warning);
        assert_eq!(
            Level::of(72., PERCENT),
            Level::Normal,
            "不看上一次时按进入线"
        );
    }

    /// 负载按核数：负载 ≥ 核数警告，≥ 2 倍危险，同样带回差；不知道核数时不分级。
    #[test]
    fn load_levels_follow_cores() {
        let mut levels = Levels::default();
        let mut s = sample(0);
        s.cores = 16;
        s.load = [16., 0., 0.];
        levels.update(&s);
        assert_eq!(levels.load, Level::Warning);
        s.load = [15., 0., 0.];
        levels.update(&s);
        assert_eq!(levels.load, Level::Warning, "0.94 倍，没降到 0.9 以下");
        s.load = [33., 0., 0.];
        levels.update(&s);
        assert_eq!(levels.load, Level::Danger);
        s.load = [30., 0., 0.];
        levels.update(&s);
        assert_eq!(levels.load, Level::Danger, "1.875 倍，没降到 1.8 以下");
        s.load = [10., 0., 0.];
        levels.update(&s);
        assert_eq!(levels.load, Level::Normal);
        s.cores = 0;
        s.load = [100., 0., 0.];
        levels.update(&s);
        assert_eq!(levels.load, Level::Normal);
        // 第一次采样没有 CPU：CPU 的级别不变
        levels.cpu = Level::Warning;
        s.cpu = None;
        levels.update(&s);
        assert_eq!(levels.cpu, Level::Warning);
    }

    /// 最满的磁盘按占用比例选，一样时取前面的（根目录）；按占用排序时一样的保持原来的顺序。
    #[test]
    fn fullest_disk_by_ratio() {
        assert!(fullest_disk(&[]).is_none());
        let disks = [
            disk("/", 40),
            disk("/data", 87),
            disk("/boot", 87),
            disk("/srv", 10),
        ];
        assert_eq!(fullest_disk(&disks).unwrap().mount, "/data");
        let same = [disk("/", 50), disk("/data", 50)];
        assert_eq!(fullest_disk(&same).unwrap().mount, "/");
        let mounts: Vec<&str> = disks_by_usage(&disks)
            .iter()
            .map(|d| d.mount.as_str())
            .collect();
        assert_eq!(mounts, ["/data", "/boot", "/", "/srv"]);
        // 总量是 0 的不算满
        let empty = [
            Disk {
                total: 0,
                ..disk("/x", 0)
            },
            disk("/", 1),
        ];
        assert_eq!(fullest_disk(&empty).unwrap().mount, "/");
    }

    /// 概览页默认不列出虚拟网卡与从没收发过的网卡，可以展开全部。
    #[test]
    fn hidden_interfaces() {
        let iface = |name: &str, total: u64| NetIface {
            name: name.into(),
            rx_total: total,
            tx_total: 0,
            rx_rate: None,
            tx_rate: None,
        };
        let net = [
            iface("eth0", 5),
            iface("docker0", 5),
            iface("veth1", 5),
            iface("eth1", 0),
        ];
        let (shown, hidden) = shown_ifaces(&net, false);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].name, "eth0");
        assert_eq!(hidden, 3);
        let (all, hidden) = shown_ifaces(&net, true);
        assert_eq!((all.len(), hidden), (4, 0));
    }

    /// 环形缓冲：最多 200 个点、最长 10 分钟，旧的从前面丢掉；时间倒退时从头记。
    #[test]
    fn history_is_a_ring_buffer() {
        let mut history = History::default();
        for i in 0..250 {
            history.push(&sample(i * 1000));
        }
        assert_eq!(history.points().len(), History::CAPACITY);
        assert_eq!(history.points().front().unwrap().at, 50_000);
        assert_eq!(history.last_at(), Some(249_000));
        // 隔了 9 分钟再来一次：超过 10 分钟的点丢掉
        history.push(&sample(249_000 + 9 * 60_000));
        assert_eq!(
            history.points().front().unwrap().at,
            789_000 - History::SPAN_MS
        );
        assert_eq!(history.points().front().unwrap().memory, 40.);
        // 系统时间被往回调
        history.push(&sample(1000));
        assert_eq!(history.points().len(), 1);
    }

    /// 曲线在没有值的点、相隔太久的两点之间断开（不连线、不插值）。
    #[test]
    fn segments_break_at_gaps() {
        let point = |at: i64, cpu: Option<f64>| Point {
            at,
            cpu,
            memory: 0.,
            rx: None,
            tx: None,
        };
        let points = [
            point(0, None),
            point(3_000, Some(1.)),
            point(6_000, Some(2.)),
            // 终端不在前台 1 分钟
            point(66_000, Some(3.)),
            point(69_000, Some(4.)),
            point(72_000, None),
            point(75_000, Some(5.)),
        ];
        let got = segments(&points, |p| p.cpu);
        assert_eq!(
            got,
            vec![
                vec![(3_000, 1.), (6_000, 2.)],
                vec![(66_000, 3.), (69_000, 4.)],
                vec![(75_000, 5.)],
            ]
        );
        assert!(segments(&[], |p: &Point| p.cpu).is_empty());
    }

    /// 网速小折线：最新的在右端；没有采样的时段按缺了几次留出空格；按最大的一格（至少 1 KB/s）缩放。
    #[test]
    fn spark_leaves_gaps_empty() {
        let mut history = History::default();
        let mut s = sample(0);
        s.rx_rate = None;
        s.tx_rate = None;
        history.push(&s);
        for (at, rate) in [(3_000, 2048.), (6_000, 1024.), (18_000, 512.)] {
            let mut s = sample(at);
            s.rx_rate = Some(rate);
            s.tx_rate = Some(0.);
            history.push(&s);
        }
        // 6 秒到 18 秒之间缺了 3 次；第一次采样没有网速；左边补空格。都是零星流量：按下限 32 KB/s 算，近乎平线
        assert_eq!(
            history.spark(8),
            [
                None,
                None,
                Some(0.0625),
                Some(0.03125),
                None,
                None,
                None,
                Some(0.015625)
            ]
        );
        assert_eq!(history.spark(2), [None, Some(0.015625)], "只取最近的几格");
        // 有明显的流量：按最近几格的峰值取整（100 KB/s）缩放
        let mut busy = History::default();
        for (at, rate) in [
            (0, 10. * 1024.),
            (3_000, 100. * 1024.),
            (6_000, 50. * 1024.),
        ] {
            let mut s = sample(at);
            s.rx_rate = Some(rate * 0.75);
            s.tx_rate = Some(rate * 0.25);
            busy.push(&s);
        }
        assert_eq!(busy.spark(3), [Some(0.1), Some(1.), Some(0.5)]);
        // 峰值 70 KB/s：上端取整到 100 KB/s
        let mut s = sample(9_000);
        s.rx_rate = Some(70. * 1024.);
        s.tx_rate = Some(0.);
        busy.push(&s);
        assert_eq!(busy.spark(1), [Some(0.7)]);
        assert_eq!(History::default().spark(3), vec![None, None, None]);
    }

    /// 宽度够时全部带进度条；不够时先收成纯数字，再从后往前逐项收起。
    #[test]
    fn fit_drops_meters_then_items() {
        let widths = [
            (50., 50.),
            (90., 60.),
            (90., 60.),
            (90., 60.),
            (120., 90.),
            (70., 70.),
        ];
        let gap = 10.;
        // 带进度条共 510 + 50
        assert_eq!(
            fit(&widths, 560., gap),
            Fit {
                count: 6,
                meters: true
            }
        );
        assert_eq!(
            fit(&widths, 559., gap),
            Fit {
                count: 6,
                meters: false
            }
        );
        // 不带共 390 + 50
        assert_eq!(
            fit(&widths, 440., gap),
            Fit {
                count: 6,
                meters: false
            }
        );
        assert_eq!(
            fit(&widths, 439., gap),
            Fit {
                count: 5,
                meters: false
            }
        );
        assert_eq!(
            fit(&widths, 280., gap),
            Fit {
                count: 4,
                meters: false
            }
        );
        assert_eq!(
            fit(&widths, 120., gap),
            Fit {
                count: 2,
                meters: false
            }
        );
        assert_eq!(
            fit(&widths, 50., gap),
            Fit {
                count: 1,
                meters: false
            }
        );
        assert_eq!(
            fit(&widths, 10., gap),
            Fit {
                count: 0,
                meters: false
            }
        );
        assert_eq!(
            fit(&[], 10., gap),
            Fit {
                count: 0,
                meters: true
            }
        );
    }

    #[test]
    fn number_formats() {
        assert_eq!(format_size(0.), "0 B");
        assert_eq!(format_size(1023.), "1023 B");
        assert_eq!(format_size(1024.), "1.0 KB");
        assert_eq!(format_size(1.5 * 1024. * 1024.), "1.5 MB");
        assert_eq!(format_size(-1.), "-");
        assert_eq!(format_rate(None), "-");
        assert_eq!(format_rate(Some(300.)), "300 B/s");
        assert_eq!(short_rate(Some(829.)), "0.8K");
        assert_eq!(short_rate(Some(0.)), "0.0K");
        assert_eq!(short_rate(Some(999.9 * 1024.)), "999.9K");
        assert_eq!(short_rate(Some(999.96 * 1024.)), "1.0M");
        assert_eq!(short_rate(Some(12.34 * 1024. * 1024.)), "12.3M");
        assert_eq!(short_rate(Some(3. * 1024. * 1024. * 1024.)), "3.0G");
        assert_eq!(short_rate(None), "-");
        assert_eq!(whole_percent(Some(37.6)), "38%");
        assert_eq!(whole_percent(None), "-");
        assert_eq!(load_text(0.42, 16), "0.4/16");
        assert_eq!(load_text(12.35, 0), "12.3");
        // 网速曲线的上端
        let kb = 1024.;
        assert_eq!(rate_ceiling(0.), kb);
        assert_eq!(rate_ceiling(300.), kb);
        assert_eq!(rate_ceiling(1.5 * kb), 2. * kb);
        assert_eq!(rate_ceiling(320. * kb), 500. * kb);
        assert_eq!(rate_ceiling(800. * kb), 1000. * kb);
        assert_eq!(rate_ceiling(1500. * kb), 2. * kb * kb);
        assert_eq!(rate_ceiling(f64::NAN), kb);
    }

    #[test]
    fn uptime_shows_the_two_largest_units() {
        crate::i18n::set_language(crate::i18n::Language::ZhCn);
        assert_eq!(
            format_uptime(3. * 86_400. + 5. * 3600. + 7. * 60.),
            "已运行 3 天 5 小时"
        );
        assert_eq!(format_uptime(2. * 3600. + 9. * 60.), "已运行 2 小时 9 分钟");
        assert_eq!(format_uptime(59.), "已运行 0 分钟");
    }
}
