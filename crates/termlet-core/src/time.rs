//! 时间：数据库与同步一律用 Unix 毫秒。

use std::time::{SystemTime, UNIX_EPOCH};

/// 现在的 Unix 毫秒。系统时间早于 1970 年时返回 0。
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Unix 毫秒显示成本地时间 `YYYY-MM-DD HH:MM`（界面列表用）；超出范围时为空。
pub fn format_local(ms: i64) -> String {
    use chrono::TimeZone as _;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// 日期时间的写法（按界面语言）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DateStyle {
    /// 简体中文、韩文：`2026-11-03 14:22`
    Dash,
    /// 繁體中文、日文：`2026/11/03 14:22`
    Slash,
    /// 英文：`Nov 3, 2026 14:22`
    English,
}

/// 文件的修改时间（紧凑，文件列表用）：今年的写月日与时分（`11-03 14:22`），往年的只写年月日（`2025-11-03`），
/// “今年”按本地时间；`ms` 不大于 0（没有时间）或超出范围时为空。
pub fn format_compact(ms: i64, now_ms: i64, style: DateStyle) -> String {
    format_compact_in(ms, now_ms, style, &chrono::Local)
}

/// 完整到秒：`2026-11-03 14:22:05`（本地时间，悬停提示用）；没有时间时为空。
pub fn format_seconds(ms: i64, style: DateStyle) -> String {
    format_seconds_in(ms, style, &chrono::Local)
}

/// [`format_compact`] 按指定的时区（测试用固定的时区）。
pub fn format_compact_in<Tz: chrono::TimeZone>(
    ms: i64,
    now_ms: i64,
    style: DateStyle,
    tz: &Tz,
) -> String
where
    Tz::Offset: std::fmt::Display,
{
    use chrono::Datelike as _;
    let (Some(time), Some(now)) = (at(ms, tz), at(now_ms.max(1), tz)) else {
        return String::new();
    };
    let format = match (time.year() == now.year(), style) {
        (true, DateStyle::Dash) => "%m-%d %H:%M",
        (true, DateStyle::Slash) => "%m/%d %H:%M",
        (true, DateStyle::English) => "%b %-d %H:%M",
        (false, DateStyle::Dash) => "%Y-%m-%d",
        (false, DateStyle::Slash) => "%Y/%m/%d",
        (false, DateStyle::English) => "%b %-d, %Y",
    };
    time.format(format).to_string()
}

/// [`format_seconds`] 按指定的时区（测试用固定的时区）。
pub fn format_seconds_in<Tz: chrono::TimeZone>(ms: i64, style: DateStyle, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let Some(time) = at(ms, tz) else {
        return String::new();
    };
    let format = match style {
        DateStyle::Dash => "%Y-%m-%d %H:%M:%S",
        DateStyle::Slash => "%Y/%m/%d %H:%M:%S",
        DateStyle::English => "%b %-d, %Y %H:%M:%S",
    };
    time.format(format).to_string()
}

/// 日志中的时间（本地时间，宽度固定，便于对齐）：`11-04 09:47:12`；没有时间时为空。
pub fn format_log_time(ms: i64) -> String {
    format_log_time_in(ms, &chrono::Local)
}

/// [`format_log_time`] 按指定的时区（测试用固定的时区）。
pub fn format_log_time_in<Tz: chrono::TimeZone>(ms: i64, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    at(ms, tz)
        .map(|t| t.format("%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

/// RFC 3339 的本地时间，到秒、带时区偏移：`2026-11-19T20:30:00+08:00`（命令行工具的 JSON 输出用：程序读得懂，
/// 人也看得懂）；没有时间时为空。
pub fn format_rfc3339_local(ms: i64) -> String {
    format_rfc3339_in(ms, &chrono::Local)
}

/// [`format_rfc3339_local`] 按指定的时区（测试用固定的时区）。
pub fn format_rfc3339_in<Tz: chrono::TimeZone>(ms: i64, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    at(ms, tz)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, false))
        .unwrap_or_default()
}

/// `now_ms` 所在的本地日期开始了多少秒（“今天”的日志用）。
pub fn seconds_since_local_midnight(now_ms: i64) -> u64 {
    seconds_since_midnight_in(now_ms, &chrono::Local)
}

/// [`seconds_since_local_midnight`] 按指定的时区（测试用固定的时区）。
pub fn seconds_since_midnight_in<Tz: chrono::TimeZone>(now_ms: i64, tz: &Tz) -> u64 {
    use chrono::Timelike as _;
    at(now_ms, tz).map_or(0, |t| u64::from(t.num_seconds_from_midnight()))
}

/// 本机在 `ms` 那一刻相对 UTC 的偏移（秒，东为正）；超出范围时为 `None`。
pub fn local_offset(ms: i64) -> Option<i32> {
    use chrono::{Offset as _, TimeZone as _};
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.offset().fix().local_minus_utc())
}

/// 偏移为 `offset` 秒的固定时区（别的机器上的时间，例如主机工具按服务器的时区显示定时器）；超出范围时用 UTC。
fn fixed(offset: i32) -> chrono::FixedOffset {
    chrono::FixedOffset::east_opt(offset)
        .unwrap_or_else(|| chrono::FixedOffset::east_opt(0).unwrap())
}

/// [`format_compact`] 按偏移为 `offset` 秒的时区。
pub fn format_compact_at(ms: i64, now_ms: i64, style: DateStyle, offset: i32) -> String {
    format_compact_in(ms, now_ms, style, &fixed(offset))
}

/// [`format_seconds`] 按偏移为 `offset` 秒的时区。
pub fn format_seconds_at(ms: i64, style: DateStyle, offset: i32) -> String {
    format_seconds_in(ms, style, &fixed(offset))
}

/// 时分 `HH:MM`，按偏移为 `offset` 秒的时区；没有时间时为空。
pub fn clock_at(ms: i64, offset: i32) -> String {
    at(ms, &fixed(offset))
        .map(|t| t.format("%H:%M").to_string())
        .unwrap_or_default()
}

/// 两个时刻在偏移为 `offset` 秒的时区中是同一天；有一个没有时间时为假。
pub fn same_day_at(a: i64, b: i64, offset: i32) -> bool {
    let tz = fixed(offset);
    match (at(a, &tz), at(b, &tz)) {
        (Some(a), Some(b)) => a.date_naive() == b.date_naive(),
        _ => false,
    }
}

/// `ms` 在 `tz` 中的时刻；不大于 0（没有时间）或超出范围时为 `None`。
fn at<Tz: chrono::TimeZone>(ms: i64, tz: &Tz) -> Option<chrono::DateTime<Tz>> {
    (ms > 0)
        .then(|| tz.timestamp_millis_opt(ms).single())
        .flatten()
}

#[cfg(test)]
mod tests {
    use chrono::{FixedOffset, TimeZone as _, Utc};

    use super::*;

    /// 北京时间（UTC+8）的某一刻的 Unix 毫秒。
    fn beijing(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
        FixedOffset::east_opt(8 * 3600)
            .unwrap()
            .with_ymd_and_hms(y, mo, d, h, mi, s)
            .unwrap()
            .timestamp_millis()
    }

    #[test]
    fn compact_dates_drop_the_year_or_the_time() {
        let tz = FixedOffset::east_opt(8 * 3600).unwrap();
        let now = beijing(2026, 11, 3, 15, 0, 0);
        let this_year = beijing(2026, 11, 3, 14, 22, 5);
        let last_year = beijing(2025, 11, 3, 14, 22, 5);
        let compact = |ms, style| format_compact_in(ms, now, style, &tz);
        assert_eq!(compact(this_year, DateStyle::Dash), "11-03 14:22");
        assert_eq!(compact(this_year, DateStyle::Slash), "11/03 14:22");
        assert_eq!(compact(this_year, DateStyle::English), "Nov 3 14:22");
        assert_eq!(compact(last_year, DateStyle::Dash), "2025-11-03");
        assert_eq!(compact(last_year, DateStyle::Slash), "2025/11/03");
        assert_eq!(compact(last_year, DateStyle::English), "Nov 3, 2025");
        // 时钟不准、来自将来的也按年份判断
        let next_year = beijing(2027, 1, 2, 8, 0, 0);
        assert_eq!(compact(next_year, DateStyle::Dash), "2027-01-02");
        // 没有时间：空
        assert_eq!(compact(0, DateStyle::Dash), "");
        assert_eq!(compact(-5, DateStyle::English), "");
        assert_eq!(format_seconds_in(0, DateStyle::Dash, &tz), "");
    }

    // 跨年按本地时间：同一刻在 UTC+8 已是新年（只写月日），在 UTC 还是去年（写年月日）
    #[test]
    fn this_year_is_decided_by_local_time() {
        let now = beijing(2026, 1, 1, 8, 30, 0); // UTC 2026-01-01 00:30
        let file = beijing(2026, 1, 1, 7, 50, 0); // UTC 2025-12-31 23:50
        let beijing_tz = FixedOffset::east_opt(8 * 3600).unwrap();
        assert_eq!(
            format_compact_in(file, now, DateStyle::Dash, &beijing_tz),
            "01-01 07:50"
        );
        assert_eq!(
            format_compact_in(file, now, DateStyle::Dash, &Utc),
            "2025-12-31"
        );
        // 现在在 UTC 是新年、文件是旧年
        let now_utc = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 30, 0)
            .unwrap()
            .timestamp_millis();
        assert_eq!(now_utc, now);
    }

    // 悬停提示：完整到秒
    #[test]
    fn full_dates_to_the_second() {
        let tz = FixedOffset::east_opt(8 * 3600).unwrap();
        let ms = beijing(2026, 11, 3, 14, 22, 5);
        let full = |style| format_seconds_in(ms, style, &tz);
        assert_eq!(full(DateStyle::Dash), "2026-11-03 14:22:05");
        assert_eq!(full(DateStyle::Slash), "2026/11/03 14:22:05");
        assert_eq!(full(DateStyle::English), "Nov 3, 2026 14:22:05");
    }

    // 日志的时间按本机时区：服务器以 UTC 写出的 01:47:12 在 UTC+8 是 09:47:12；“今天”从本地的零点算起
    #[test]
    fn log_times_follow_the_local_zone() {
        let tz = FixedOffset::east_opt(8 * 3600).unwrap();
        let utc = Utc
            .with_ymd_and_hms(2026, 11, 4, 1, 47, 12)
            .unwrap()
            .timestamp_millis();
        assert_eq!(format_log_time_in(utc, &tz), "11-04 09:47:12");
        assert_eq!(format_log_time_in(utc, &Utc), "11-04 01:47:12");
        assert_eq!(format_log_time_in(0, &tz), "");
        assert_eq!(format_rfc3339_in(utc, &tz), "2026-11-04T09:47:12+08:00");
        assert_eq!(format_rfc3339_in(utc, &Utc), "2026-11-04T01:47:12+00:00");
        assert_eq!(format_rfc3339_in(0, &tz), "");
        assert_eq!(seconds_since_midnight_in(utc, &tz), 9 * 3600 + 47 * 60 + 12);
        assert_eq!(seconds_since_midnight_in(utc, &Utc), 3600 + 47 * 60 + 12);
    }

    // 按偏移的时区（服务器的时间）：UTC 19:00 在 UTC+8 是第二天的 03:00，在 UTC-5 是当天的 14:00
    #[test]
    fn times_at_an_offset() {
        let ms = Utc
            .with_ymd_and_hms(2026, 11, 10, 19, 0, 0)
            .unwrap()
            .timestamp_millis();
        let now = Utc
            .with_ymd_and_hms(2026, 11, 10, 18, 0, 0)
            .unwrap()
            .timestamp_millis();
        assert_eq!(clock_at(ms, 8 * 3600), "03:00");
        assert_eq!(clock_at(ms, -5 * 3600), "14:00");
        assert_eq!(
            format_seconds_at(ms, DateStyle::Dash, 8 * 3600),
            "2026-11-11 03:00:00"
        );
        assert_eq!(
            format_compact_at(ms, now, DateStyle::Dash, 8 * 3600),
            "11-11 03:00"
        );
        assert!(same_day_at(ms, now, -5 * 3600));
        assert!(same_day_at(ms, now, 0));
        // 现在是北京时间 11-11 02:00，触发在 03:00：同一天
        assert!(same_day_at(ms, now, 8 * 3600));
        assert!(!same_day_at(ms, now + 12 * 3_600_000, -5 * 3600));
        assert!(!same_day_at(0, now, 0));
        assert_eq!(clock_at(0, 0), "");
        assert!(local_offset(ms).is_some());
    }
}
