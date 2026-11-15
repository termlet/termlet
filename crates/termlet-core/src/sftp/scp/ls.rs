//! 解析 `ls -ln` 的输出（SCP 模式下浏览目录、查看属性用）。
//!
//! 按探测到的写法执行（见 [`LsStyle`]），解析时按字段的样子认时间的写法，不依赖探测的结果：
//!
//! - GNU coreutils：`ls -lnAb --time-style=+%s`，时间是 Unix 秒；`-b` 把名称中的空格、控制字符与非 ASCII 字节写成
//!   `\ `、`\n`、`\344` 等，按字节还原，名称与符号链接的 ` -> ` 分得开。
//! - BusyBox：`ls -lnA --full-time`，时间是 `2026-11-09 04:55:44 +0000`（GNU 的 `--full-time` 另带小数秒）；
//!   名称原样输出，符号链接按长度（链接的大小就是目标的字节数）切开名称与目标。
//! - 都不支持时 `ls -lnA`：时间是 `Nov  9 04:55`（今年内）或 `Nov  9  2025`，只精确到分钟或日。
//!
//! 命令都带 `TZ=UTC0`，时间按 UTC 换算。设备文件的大小一栏是 `主设备号, 次设备号`，大小记为 0。
//! 解析不了的行（`total`、BusyBox 把不能显示的字节写成的 `?` 之外的乱行、GNU 对悬空链接写的 `l?????????`）跳过。

use chrono::Datelike as _;

use super::super::client::{S_IFDIR, S_IFLNK, S_IFREG};

/// 服务器上 `ls` 的写法。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LsStyle {
    /// GNU coreutils
    Gnu,
    /// 支持 `--full-time` 的（BusyBox）
    FullTime,
    /// 只有基本的 `-l`
    Plain,
}

impl LsStyle {
    /// 列出目录内容（不含 `.` 与 `..`）的命令；`follow` 时按符号链接指向的目标列出。
    pub(crate) fn list_command(self, follow: bool) -> String {
        self.command(if follow { "-lnAL" } else { "-lnA" })
    }

    /// 查看一项本身的命令（后面接路径）；`follow` 时跟随符号链接。
    pub(crate) fn stat_command(self, follow: bool) -> String {
        self.command(if follow { "-dlnL" } else { "-dln" })
    }

    fn command(self, flags: &str) -> String {
        match self {
            LsStyle::Gnu => format!("LC_ALL=C TZ=UTC0 ls {flags}b --time-style=+%s"),
            // BusyBox 在 C 区域设置下把非 ASCII 字节都写成 ?，UTF-8 下原样输出
            LsStyle::FullTime => format!("LC_ALL=C.UTF-8 TZ=UTC0 ls {flags} --full-time"),
            LsStyle::Plain => format!("LC_ALL=C.UTF-8 TZ=UTC0 ls {flags}"),
        }
    }

    /// 名称是否经 `-b` 转义。
    fn escaped(self) -> bool {
        self == LsStyle::Gnu
    }
}

/// `ls -l` 的一行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LsLine {
    /// 含类型位的模式（`S_IFREG | 0o644` 等）
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    /// 修改时间（Unix 秒）
    pub mtime: i64,
    /// 名称（服务器上的字节）
    pub name: Vec<u8>,
    /// 符号链接的目标
    pub target: Option<Vec<u8>>,
}

/// 解析一行；`now` 是现在的 Unix 秒（没有年份的时间按它推算年份）。
pub(crate) fn parse_line(line: &[u8], style: LsStyle, now: i64) -> Option<LsLine> {
    let mut rest = line.strip_suffix(b"\r").unwrap_or(line);
    let perms = token(&mut rest)?;
    let mode = parse_mode(perms)?;
    let _links: u64 = number(token(&mut rest)?)?;
    let uid: u32 = number(token(&mut rest)?)?;
    let gid: u32 = number(token(&mut rest)?)?;
    let mut size_field = token(&mut rest)?;
    let size = if let Some(major) = size_field.strip_suffix(b",") {
        // 设备文件：`主, 次`
        let _: u64 = number(major)?;
        let _: u64 = number(token(&mut rest)?)?;
        0
    } else if let Some(pos) = size_field.iter().position(|b| *b == b',') {
        // 少数写法把设备号连在一起：`1,3`
        let _: u64 = number(&size_field[..pos])?;
        size_field = &size_field[pos + 1..];
        let _: u64 = number(size_field)?;
        0
    } else {
        number(size_field)?
    };
    let mtime = parse_time(&mut rest, now)?;
    // 时间与名称之间正好一个空格；名称本身可以以空格开头（没有转义时）
    let name_part = rest.strip_prefix(b" ")?;
    if name_part.is_empty() {
        return None;
    }
    let is_link = mode & 0o170000 == S_IFLNK;
    let (name, target) = if style.escaped() {
        let (name, target) = if is_link {
            match find(name_part, b" -> ") {
                Some(at) => (&name_part[..at], Some(&name_part[at + 4..])),
                None => (name_part, None),
            }
        } else {
            (name_part, None)
        };
        (unescape(name), target.map(unescape))
    } else if is_link {
        split_link(name_part, size)
    } else {
        (name_part.to_vec(), None)
    };
    if name.is_empty() {
        return None;
    }
    Some(LsLine {
        mode,
        uid,
        gid,
        size,
        mtime,
        name,
        target,
    })
}

/// 没有转义时切开 `名称 -> 目标`：目标的长度是链接的大小；对不上时按第一个 ` -> `。
fn split_link(part: &[u8], size: u64) -> (Vec<u8>, Option<Vec<u8>>) {
    let size = usize::try_from(size).unwrap_or(usize::MAX);
    if let Some(cut) = part.len().checked_sub(size.saturating_add(4))
        && &part[cut..cut + 4] == b" -> "
        && cut > 0
    {
        return (part[..cut].to_vec(), Some(part[cut + 4..].to_vec()));
    }
    match find(part, b" -> ") {
        Some(at) if at > 0 => (part[..at].to_vec(), Some(part[at + 4..].to_vec())),
        _ => (part.to_vec(), None),
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 取下一个以空格分开的字段（跳过前面的空格），`rest` 停在字段后面的空格上。
fn token<'a>(rest: &mut &'a [u8]) -> Option<&'a [u8]> {
    let start = rest.iter().position(|b| *b != b' ')?;
    let tail = &rest[start..];
    let end = tail.iter().position(|b| *b == b' ').unwrap_or(tail.len());
    *rest = &tail[end..];
    Some(&tail[..end])
}

fn number<T: std::str::FromStr>(field: &[u8]) -> Option<T> {
    std::str::from_utf8(field).ok()?.parse().ok()
}

/// `-rwxr-xr-x`、`drwxrwxrwt`、`lrwxrwxrwx`、`crw-rw----+`（ACL）、`-rw-r--r--.`（SELinux）等。
fn parse_mode(field: &[u8]) -> Option<u32> {
    if field.len() < 10 {
        return None;
    }
    let kind = match field[0] {
        b'-' => S_IFREG,
        b'd' => S_IFDIR,
        b'l' => S_IFLNK,
        b'c' => 0o020000,
        b'b' => 0o060000,
        b'p' => 0o010000,
        b's' => 0o140000,
        _ => return None,
    };
    let mut bits = 0u32;
    // 三组：读、写、执行（执行位上可能是 s/S、t/T）
    for (group, special) in [(0usize, 0o4000u32), (1, 0o2000), (2, 0o1000)] {
        let at = 1 + group * 3;
        let shift = (2 - group) * 3;
        match field[at] {
            b'r' => bits |= 0o4 << shift,
            b'-' => {}
            _ => return None,
        }
        match field[at + 1] {
            b'w' => bits |= 0o2 << shift,
            b'-' => {}
            _ => return None,
        }
        let exec = field[at + 2];
        let set = if group == 2 { b't' } else { b's' };
        match exec {
            b'x' => bits |= 0o1 << shift,
            b'-' => {}
            c if c == set => bits |= (0o1 << shift) | special,
            c if c == set.to_ascii_uppercase() => bits |= special,
            _ => return None,
        }
    }
    // 第 11 个字符只能是 ACL、扩展属性、SELinux 的标记
    if field.len() > 11 || (field.len() == 11 && !matches!(field[10], b'+' | b'.' | b'@')) {
        return None;
    }
    Some(kind | bits)
}

/// 时间：Unix 秒（GNU 的 `+%s`）、`2026-11-09 04:55:44[.小数] +0000`，或 `Nov  9 04:55` / `Nov  9  2025`。
fn parse_time(rest: &mut &[u8], now: i64) -> Option<i64> {
    let first = token(rest)?;
    let text = std::str::from_utf8(first).ok()?;
    if let Ok(seconds) = text.parse::<i64>() {
        return Some(seconds);
    }
    if text.len() == 10 && text.as_bytes()[4] == b'-' {
        let date = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()?;
        let clock = std::str::from_utf8(token(rest)?).ok()?;
        let clock = clock.split('.').next()?;
        let time = chrono::NaiveTime::parse_from_str(clock, "%H:%M:%S").ok()?;
        let zone = std::str::from_utf8(token(rest)?).ok()?;
        let offset = parse_offset(zone)?;
        return Some(date.and_time(time).and_utc().timestamp() - offset);
    }
    let month = month_of(text)?;
    let day: u32 = number(token(rest)?)?;
    let third = std::str::from_utf8(token(rest)?).ok()?;
    if let Some((h, m)) = third.split_once(':') {
        let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
        let year = chrono::DateTime::from_timestamp(now, 0)?
            .date_naive()
            .year();
        let at = |year: i32| {
            chrono::NaiveDate::from_ymd_opt(year, month, day)
                .and_then(|d| d.and_hms_opt(h, m, 0))
                .map(|t| t.and_utc().timestamp())
        };
        // 没有年份的是最近半年内的：算出来比现在晚一天以上的是去年的
        let this_year = at(year)?;
        return Some(if this_year > now + 86_400 {
            at(year - 1)?
        } else {
            this_year
        });
    }
    let year: i32 = third.parse().ok()?;
    chrono::NaiveDate::from_ymd_opt(year, month, day)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|t| t.and_utc().timestamp())
}

/// `+0800`、`-0530` 换成秒。
fn parse_offset(zone: &str) -> Option<i64> {
    let (sign, digits) = match zone.as_bytes().first()? {
        b'+' => (1, &zone[1..]),
        b'-' => (-1, &zone[1..]),
        _ => return None,
    };
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hours: i64 = digits[..2].parse().ok()?;
    let minutes: i64 = digits[2..].parse().ok()?;
    Some(sign * (hours * 3600 + minutes * 60))
}

fn month_of(text: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    MONTHS
        .iter()
        .position(|m| m.eq_ignore_ascii_case(text))
        .map(|i| i as u32 + 1)
}

/// 还原 GNU `ls -b` 的转义：`\ `、`\\`、`\n` 等与八进制的 `\344`。
fn unescape(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let b = text[i];
        if b != b'\\' || i + 1 == text.len() {
            out.push(b);
            i += 1;
            continue;
        }
        let next = text[i + 1];
        if (b'0'..=b'7').contains(&next) {
            let mut value = 0u32;
            let mut j = i + 1;
            while j < text.len() && j < i + 4 && (b'0'..=b'7').contains(&text[j]) {
                value = value * 8 + u32::from(text[j] - b'0');
                j += 1;
            }
            out.push(value as u8);
            i = j;
            continue;
        }
        out.push(match next {
            b'n' => b'\n',
            b't' => b'\t',
            b'r' => b'\r',
            b'a' => 0x07,
            b'b' => 0x08,
            b'f' => 0x0c,
            b'v' => 0x0b,
            b'e' => 0x1b,
            other => other,
        });
        i += 2;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-09 12:00:00 UTC
    const NOW: i64 = 1_794_225_600;

    fn parse(line: &str, style: LsStyle) -> LsLine {
        parse_line(line.as_bytes(), style, NOW).unwrap_or_else(|| panic!("解析失败：{line}"))
    }

    #[test]
    fn gnu_lines() {
        let a = parse("-rw-r--r-- 1 0 0 0 1794200144 a\\ b", LsStyle::Gnu);
        assert_eq!(a.mode, S_IFREG | 0o644);
        assert_eq!((a.uid, a.gid, a.size, a.mtime), (0, 0, 0, 1_794_200_144));
        assert_eq!(a.name, b"a b");
        let link = parse(
            "lrwxrwxrwx 1 1000 1000 3 1794200144 lnk\\ x -> a\\ b",
            LsStyle::Gnu,
        );
        assert_eq!(link.mode, S_IFLNK | 0o777);
        assert_eq!(link.name, b"lnk x");
        assert_eq!(link.target.as_deref(), Some(&b"a b"[..]));
        // 名称中的 " -> " 被转义，不会当成链接
        let arrow = parse("-rw-r--r-- 1 0 0 0 1794200144 c\\ ->\\ d", LsStyle::Gnu);
        assert_eq!(arrow.name, b"c -> d");
        assert_eq!(arrow.target, None);
        let utf8 = parse(
            "-rw-r--r-- 1 0 0 12 1794200144 \\344\\270\\255\\346\\226\\207",
            LsStyle::Gnu,
        );
        assert_eq!(utf8.name, "中文".as_bytes());
        assert_eq!(
            parse("-rw-r--r-- 1 0 0 0 1794200144 e\\\\f\\nx", LsStyle::Gnu).name,
            b"e\\f\nx"
        );
        let dev = parse("crw-rw-rw- 1 0 0 1, 3 1793020882 null", LsStyle::Gnu);
        assert_eq!((dev.mode, dev.size), (0o020000 | 0o666, 0));
        let sticky = parse("drwxrwxrwt 9 0 0 4096 1794200000 tmp", LsStyle::Gnu);
        assert_eq!(sticky.mode, S_IFDIR | 0o1777);
        let suid = parse("-rwsr-x--- 1 0 0 10 1794200000 su", LsStyle::Gnu);
        assert_eq!(suid.mode, S_IFREG | 0o4750);
        let selinux = parse("-rw-r--r--. 1 0 0 10 1794200000 f", LsStyle::Gnu);
        assert_eq!(selinux.mode, S_IFREG | 0o644);
        // 悬空链接（-L 时）与 total 行跳过
        for bad in ["l????????? ? ? ? ?            ? broken", "total 620", ""] {
            assert_eq!(parse_line(bad.as_bytes(), LsStyle::Gnu, NOW), None, "{bad}");
        }
    }

    #[test]
    fn busybox_full_time_lines() {
        let a = parse(
            "-rw-r--r--    1 0        0                0 2026-11-09 04:55:44 +0000 a b",
            LsStyle::FullTime,
        );
        assert_eq!(a.name, b"a b");
        assert_eq!(a.mtime, 1_794_200_144);
        // 原样输出的名称：链接按大小切开，名称中的 " -> " 不会切错
        let link = parse(
            "lrwxrwxrwx    1 0        0                6 2026-11-09 04:55:44 +0000 x -> y -> a -> b",
            LsStyle::FullTime,
        );
        assert_eq!(link.name, b"x -> y");
        assert_eq!(link.target.as_deref(), Some(&b"a -> b"[..]));
        let lead = parse(
            "-rw-r--r--    1 0        0                0 2026-11-09 04:55:44 +0000  lead",
            LsStyle::FullTime,
        );
        assert_eq!(lead.name, b" lead");
        let utf8 = parse(
            "-rw-r--r--    1 1000     1000            42 2026-11-09 04:55:44 +0800 中文",
            LsStyle::FullTime,
        );
        assert_eq!(utf8.name, "中文".as_bytes());
        assert_eq!(utf8.mtime, 1_794_200_144 - 8 * 3600);
        // GNU 的 --full-time 带小数秒
        let gnu = parse(
            "-rw-r--r-- 1 0 0 5 2025-12-17 07:03:17.123456789 +0000 f",
            LsStyle::FullTime,
        );
        assert_eq!(gnu.mtime, 1_765_954_997);
        let dev = parse(
            "crw-rw-rw-    1 0        0           1,   3 2026-10-26 13:21:22 +0000 null",
            LsStyle::FullTime,
        );
        assert_eq!(dev.size, 0);
        assert_eq!(dev.name, b"null");
    }

    #[test]
    fn plain_lines() {
        // 今年内：只有时分
        let recent = parse(
            "-rw-r--r--    1 0        0               10 Nov  9 04:55 a.txt",
            LsStyle::Plain,
        );
        assert_eq!(recent.mtime, 1_794_200_100);
        // 比现在晚一天以上的是去年的
        let last_year = parse(
            "-rw-r--r--    1 0        0               10 Dec 20 10:00 b",
            LsStyle::Plain,
        );
        assert_eq!(
            chrono::DateTime::from_timestamp(last_year.mtime, 0)
                .unwrap()
                .year(),
            2025
        );
        let old = parse(
            "-rw-r--r--    1 0        0           581349 Oct 10  2025 moduli",
            LsStyle::Plain,
        );
        assert_eq!(old.mtime, 1_760_054_400);
        assert_eq!(old.size, 581_349);
    }

    #[test]
    fn commands_by_style() {
        assert_eq!(
            LsStyle::Gnu.list_command(false),
            "LC_ALL=C TZ=UTC0 ls -lnAb --time-style=+%s"
        );
        assert_eq!(
            LsStyle::FullTime.stat_command(true),
            "LC_ALL=C.UTF-8 TZ=UTC0 ls -dlnL --full-time"
        );
        assert_eq!(
            LsStyle::Plain.list_command(true),
            "LC_ALL=C.UTF-8 TZ=UTC0 ls -lnAL"
        );
    }
}
