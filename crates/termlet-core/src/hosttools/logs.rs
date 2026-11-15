//! 服务与容器的日志：日志查看器，以及服务、容器详情中的最近几行。
//!
//! - 服务用 `journalctl -u … -o short-iso --utc --show-cursor`：每条带时区的时间（换成 Unix 毫秒，界面按本机时区显示，
//!   不再是服务器时区的原文），最后一行是读到的位置（游标）。容器用 `docker logs --timestamps`（RFC 3339 的纳秒时间）。
//! - 行数、时间范围、级别（`journalctl -p`）在服务器上筛选；时间范围用相对的秒数（`--since=-900s`、`--since 900s`），
//!   不受两边时钟、时区不同的影响。
//! - 跟随（日志查看器定时增量读取）：服务从上次的游标之后读（`--after-cursor`），容器从上次最后一条的时间之后读
//!   （`--since`，客户端再去掉不晚于它的行）。
//! - 输出只留最后 [`super::LOG_LIMIT`] 字节（在服务器上经 `tail -c`，留下最新的），超过时 [`LogResult::truncated`]。
//! - 服务名、容器 ID、游标、时间都校验后作为参数传入，不拼进脚本；日志的内容伪造不了脚本自己的标记（见 `scripts.rs`）。
//! - 错误、警告行按关键词标出（与终端默认的关键词高亮相同的一组词）；多行消息的后续行跟着第一行。

use std::sync::LazyLock;

use regex::Regex;

use super::action::UNIT_RE;
use super::docker::{Engine, ID_RE, docker_down};
use super::exec::{Limits, Target, on_runtime, one_line, run, sh_script};
use super::sections::{PrivSudo, first_line, unreadable};
use super::{HostTools, LIST_TIMEOUT, ToolsError, scripts};
use crate::ssh::ExecResult;

/// 一次最多读的行数（“全部”也受它与输出上限的约束）。
pub const MAX_LOG_LINES: u32 = 100_000;

/// 跟随时容器一次增量读取的最多行数（`--tail`：从日志末尾往前找，不必每次读整个日志文件）。
const FOLLOW_TAIL: u32 = 5_000;

/// 增量读取的最长时间（跟随中定时读，读不到时下次再读）。
const FOLLOW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// journalctl 的游标：`s=…;i=…;b=…;m=…;t=…;x=…`（十六进制与分隔符）。
static CURSOR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9A-Za-z=;_-]{1,512}$").expect("正则表达式"));

/// 错误、警告行（不区分大小写的整词，与终端默认的关键词高亮相同的一组词）。
static ERROR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(error|errors|err|fail|failed|failure|fatal|panic|panicked|critical|crit|emerg|exception|traceback|denied|refused|segfault)\b",
    )
    .expect("正则表达式")
});
static WARNING_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(warn|warning|warnings|deprecated)\b").expect("正则表达式")
});

/// 终端的控制序列（颜色、标题等，容器的日志中常见）：显示与判断级别前去掉。
static ANSI_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\x1b(\[[0-?]*[ -/]*[@-~]|\][^\x07\x1b]*(\x07|\x1b\\)?|[@-_])").expect("正则表达式")
});

/// 日志的来源。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum LogSource {
    /// systemd 服务（`unit` 例如 `nginx.service`）：不是 root 时先试免密码 sudo（能看到全部日志，[`LogResult::sudo`]）
    Service { unit: String },
    /// 容器；`sudo` 为 Docker 页经免密码 sudo 访问（[`super::DockerInfo::sudo`]），读日志同样经 sudo
    Container {
        engine: Engine,
        id: String,
        sudo: bool,
    },
}

/// 读多少行。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LogLines {
    /// 最近的这么多行（1–[`MAX_LOG_LINES`]）
    Last(u32),
    /// 全部（最多 [`MAX_LOG_LINES`] 行，并受输出上限约束）
    All,
}

/// 服务日志的级别（`journalctl -p`）。容器的日志没有级别，不起作用。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum LogPriority {
    #[default]
    All,
    /// 警告及以上（`-p warning`）
    Warning,
    /// 错误及以上（`-p err`）
    Error,
}

/// 读到的位置：跟随时下一次从它之后读（[`LogQuery::after`]）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogCursor(Cursor);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Cursor {
    /// journalctl 的游标
    Journal(String),
    /// 容器最后一条的时间（Unix 纳秒）
    Docker(i64),
}

/// 一次读取。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogQuery {
    pub source: LogSource,
    pub lines: LogLines,
    /// 只读最近这么多秒的（`--since`），`None` 为不限
    pub since_secs: Option<u64>,
    pub priority: LogPriority,
    /// 增量读取：从这个位置之后读（[`LogResult::cursor`]）。给出时不看 `lines`、`since_secs`
    pub after: Option<LogCursor>,
}

impl LogQuery {
    /// 最近 `lines` 行（服务、容器详情中的几行）。
    pub fn recent(source: LogSource, lines: u32) -> Self {
        Self {
            source,
            lines: LogLines::Last(lines),
            since_secs: None,
            priority: LogPriority::All,
            after: None,
        }
    }
}

/// 一行的级别（按关键词）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LogLevel {
    #[default]
    Normal,
    Warning,
    Error,
}

/// 日志的一行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogLine {
    /// 时间（Unix 毫秒）；多行消息的后续行、读不出时间的行为 `None`
    pub at: Option<i64>,
    /// 去掉时间的内容（服务另去掉主机名：`nginx[123]: …`）；多行消息的后续行按第一行的内容对齐
    pub text: String,
    pub level: LogLevel,
}

/// 读到的日志。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogResult {
    pub lines: Vec<LogLine>,
    /// 输出超过上限，前面的（较早的）被截掉了
    pub truncated: bool,
    /// 经免密码 sudo 读取（服务能看到全部日志）
    pub sudo: bool,
    /// 服务的日志可能因为权限看不全：journalctl 说权限不够，或者以普通用户读取（不是 root、没有可用的免密码 sudo）
    /// 而不在 systemd-journal、adm、wheel 组中（只能看到自己的）
    pub limited: bool,
    /// 读到的位置（跟随时下一次从它之后读）；读不到时为上一次的
    pub cursor: Option<LogCursor>,
}

/// 读取的命令行（参数已校验）。
pub(crate) fn logs_command(query: &LogQuery) -> Result<String, ToolsError> {
    let argv = logs_argv(query)?;
    let script = match query.source {
        LogSource::Service { .. } => scripts::service_log_script(),
        LogSource::Container { .. } => scripts::read_script(),
    };
    Ok(sh_script(&one_line(&script), &argv))
}

/// 读日志的命令（不含 sudo：服务由脚本判断；容器按 [`LogSource::Container::sudo`] 加 `sudo -n`）。
pub(crate) fn logs_argv(query: &LogQuery) -> Result<Vec<String>, ToolsError> {
    let invalid = |reason: &str| Err(ToolsError::Invalid(reason.to_string()));
    if let LogLines::Last(n) = query.lines
        && !(1..=MAX_LOG_LINES).contains(&n)
    {
        return invalid(&format!("行数应为 1–{MAX_LOG_LINES}"));
    }
    match &query.source {
        LogSource::Service { unit } => {
            if !UNIT_RE.is_match(unit) {
                return invalid("服务名不正确");
            }
            let mut argv: Vec<String> = [
                "journalctl",
                "-u",
                unit.as_str(),
                "--no-pager",
                "-q",
                "--utc",
                "-o",
                "short-iso",
                "--show-cursor",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            match query.priority {
                LogPriority::All => {}
                LogPriority::Warning => argv.extend(["-p".into(), "4".into()]),
                LogPriority::Error => argv.extend(["-p".into(), "3".into()]),
            }
            match &query.after {
                Some(LogCursor(Cursor::Journal(cursor))) => {
                    if !CURSOR_RE.is_match(cursor) {
                        return invalid("游标不正确");
                    }
                    argv.push(format!("--after-cursor={cursor}"));
                }
                Some(LogCursor(Cursor::Docker(_))) => return invalid("游标不是服务日志的"),
                // 有时间范围时不用 -n：journalctl 同时给出两者时显示的是范围内最早的 n 条，
                // 这里要的是最新的，读完后在客户端只留最后 n 行
                None => match query.since_secs {
                    Some(secs) => argv.push(format!("--since=-{}s", secs.max(1))),
                    None => {
                        let n = match query.lines {
                            LogLines::Last(n) => n,
                            LogLines::All => MAX_LOG_LINES,
                        };
                        argv.extend(["-n".into(), n.to_string()]);
                    }
                },
            }
            Ok(argv)
        }
        LogSource::Container { engine, id, sudo } => {
            if !ID_RE.is_match(id) {
                return invalid("容器 ID 不正确");
            }
            let mut argv: Vec<String> = Vec::new();
            if *sudo {
                argv.extend(["sudo".into(), "-n".into()]);
            }
            argv.extend([
                engine.command().to_string(),
                "logs".into(),
                "--timestamps".into(),
            ]);
            match &query.after {
                Some(LogCursor(Cursor::Docker(nanos))) => {
                    argv.extend(["--tail".into(), FOLLOW_TAIL.to_string()]);
                    argv.extend(["--since".into(), rfc3339_nanos(nanos.saturating_add(1))]);
                }
                Some(LogCursor(Cursor::Journal(_))) => return invalid("游标不是容器日志的"),
                None => {
                    // docker 先取最后 n 行再按时间筛选，两者可以同时给出；全部时不给 --tail（podman 不认 all）
                    if let LogLines::Last(n) = query.lines {
                        argv.extend(["--tail".into(), n.to_string()]);
                    }
                    if let Some(secs) = query.since_secs {
                        argv.extend(["--since".into(), format!("{}s", secs.max(1))]);
                    }
                }
            }
            argv.push(id.clone());
            Ok(argv)
        }
    }
}

/// Unix 纳秒写成 RFC 3339（UTC，9 位小数）：docker、podman 的 `--since` 都认。
fn rfc3339_nanos(nanos: i64) -> String {
    chrono::DateTime::from_timestamp(
        nanos.div_euclid(1_000_000_000),
        nanos.rem_euclid(1_000_000_000) as u32,
    )
    .map(|t| t.format("%Y-%m-%dT%H:%M:%S%.9fZ").to_string())
    .unwrap_or_default()
}

/// [`scripts::READ_BODY`] 的输出：命令的输出与退出码；`cut` 为输出超过上限、前面被截掉了。
pub(crate) struct ReadOutput {
    pub code: u32,
    pub output: String,
    pub cut: bool,
}

impl ReadOutput {
    /// 解析读取的输出（第一行 `@@sudo` 已去掉的部分）；最后一行不是 `@@rc` 时为 `None`（脚本没能执行）。`bytes` 是
    /// 经 `tail -c` 的那部分的字节数：达到上限时前面被截掉了，第一行可能不完整，去掉。
    pub fn parse(out: &str, bytes: usize) -> Option<Self> {
        let body = out.strip_suffix('\n').unwrap_or(out);
        let (output, last) = body.rsplit_once('\n')?;
        let code = last.strip_prefix("@@rc ")?.trim().parse().ok()?;
        let cut = bytes >= scripts::READ_TAIL;
        let output = if cut {
            output.split_once('\n').map_or("", |(_, rest)| rest)
        } else {
            output
        };
        Some(ReadOutput {
            code,
            output: output.to_string(),
            cut,
        })
    }
}

/// journalctl 因为权限打不开日志文件时的说明（它不翻译这些说明）。
const JOURNAL_DENIED: &str = "insufficient permissions";

/// 解析读取日志的结果。
pub(crate) fn parse_logs(query: &LogQuery, result: &ExecResult) -> Result<LogResult, ToolsError> {
    let out = String::from_utf8_lossy(&result.stdout);
    let (sudo, limited, rest, bytes) = match query.source {
        LogSource::Service { .. } => {
            // 第一行：@@sudo 结果 UID 组（没有用 sudo 时结果为空，不在能读全部日志的组中时组为空）
            let (head, rest) = out.split_once('\n').ok_or_else(|| unreadable(result))?;
            let words: Vec<&str> = head
                .strip_prefix("@@sudo")
                .ok_or_else(|| unreadable(result))?
                .split_whitespace()
                .collect();
            let sudo = words.iter().any(|w| PrivSudo::parse(w) == PrivSudo::Used);
            let root = words.contains(&"0");
            let journal = words.contains(&"journal");
            let bytes = result.stdout.len().saturating_sub(head.len() + 1);
            (sudo, !sudo && !root && !journal, rest, bytes)
        }
        LogSource::Container { sudo, .. } => (sudo, false, out.as_ref(), result.stdout.len()),
    };
    let read = ReadOutput::parse(rest, bytes).ok_or_else(|| unreadable(result))?;
    // journalctl 一个日志文件也打不开（“No journal files were opened due to insufficient permissions.”，以错误退出）：
    // 不算读取失败，当作因为权限看不到
    let denied =
        matches!(query.source, LogSource::Service { .. }) && read.output.contains(JOURNAL_DENIED);
    let limited = limited || denied;
    if read.code != 0 && !denied {
        if matches!(query.source, LogSource::Container { .. }) && docker_down(&read.output) {
            return Err(ToolsError::DockerDown);
        }
        return Err(ToolsError::LogsFailed(first_line(&read.output)));
    }
    let (mut lines, cursor) = match query.source {
        LogSource::Service { .. } => parse_journal(&read.output),
        LogSource::Container { .. } => parse_docker_logs(&read.output),
    };
    if denied {
        lines.retain(|(_, line)| !line.text.contains(JOURNAL_DENIED));
    }
    // 增量读取：去掉不晚于上次最后一条的（podman 的 --since 可能只到秒）
    if let Some(LogCursor(Cursor::Docker(after))) = &query.after {
        lines.retain(|(nanos, _)| nanos.is_none_or(|nanos| nanos > *after));
    }
    // 服务有时间范围时服务器上没有按行数取（见 logs_argv），在这里只留最后 n 行（不从多行消息的中间开始）
    if matches!(query.source, LogSource::Service { .. })
        && query.after.is_none()
        && query.since_secs.is_some()
        && let LogLines::Last(n) = query.lines
        && lines.len() > n as usize
    {
        lines.drain(..lines.len() - n as usize);
        let continued = lines
            .iter()
            .take_while(|(nanos, _)| nanos.is_none())
            .count();
        lines.drain(..continued);
    }
    // 容器：游标是读到的最晚的时间（见 parse_docker_logs）
    let cursor = cursor
        .or_else(|| {
            lines
                .iter()
                .filter_map(|line| line.0)
                .max()
                .filter(|_| matches!(query.source, LogSource::Container { .. }))
                .map(|nanos| LogCursor(Cursor::Docker(nanos)))
        })
        .or_else(|| query.after.clone());
    Ok(LogResult {
        lines: lines.into_iter().map(|(_, line)| line).collect(),
        truncated: read.cut,
        sudo,
        limited,
        cursor,
    })
}

/// 解析 journalctl 的输出（`-o short-iso --utc --show-cursor -q`）：`时间 主机 标识[PID]: 消息`，多行消息的后续行以
/// 空格缩进到消息开始的位置；最后一行 `-- cursor: …` 是游标；其余以 `-- ` 开头的说明行跳过。
/// 返回每行（带 Unix 纳秒的时间，只用于增量读取的比较）与游标。
fn parse_journal(text: &str) -> (Vec<(Option<i64>, LogLine)>, Option<LogCursor>) {
    let mut lines: Vec<(Option<i64>, LogLine)> = Vec::new();
    let mut cursor = None;
    // 上一条消息在原文中开始的列（后续行的缩进）、在去掉时间与主机后的列，以及它的级别
    let mut header: Option<(usize, usize, LogLevel)> = None;
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(rest) = raw.strip_prefix("-- cursor: ") {
            let rest = rest.trim();
            if CURSOR_RE.is_match(rest) {
                cursor = Some(LogCursor(Cursor::Journal(rest.to_string())));
            }
            continue;
        }
        if raw.starts_with("-- ") || raw.is_empty() {
            continue;
        }
        if raw.starts_with([' ', '\t']) {
            // 后续行：去掉 journalctl 的缩进，再按去掉时间与主机后的消息位置缩进
            let (indent, column, level) = header.unwrap_or((0, 0, LogLevel::Normal));
            let spaces = raw.len() - raw.trim_start_matches(' ').len();
            let content = clean(&raw[spaces.min(indent)..]);
            let level = level.max(level_of(&content));
            let text = format!("{}{content}", " ".repeat(column));
            lines.push((
                None,
                LogLine {
                    at: None,
                    text,
                    level,
                },
            ));
            continue;
        }
        let parsed = raw.split_once(' ').and_then(|(time, rest)| {
            let nanos = parse_time(time)?;
            // 去掉主机名；消息在“标识[PID]: ”之后
            let rest = rest.split_once(' ').map_or("", |(_, rest)| rest);
            Some((nanos, rest, raw.len() - rest.len()))
        });
        match parsed {
            Some((nanos, rest, removed)) => {
                // 原文中的缩进按原文的位置去掉；对齐按去掉控制序列后的位置
                let indent = removed + rest.find(": ").map_or(0, |at| at + 2);
                let text = clean(rest);
                let level = level_of(&text);
                header = Some((indent, text.find(": ").map_or(0, |at| at + 2), level));
                lines.push((
                    Some(nanos),
                    LogLine {
                        at: Some(nanos.div_euclid(1_000_000)),
                        text,
                        level,
                    },
                ));
            }
            None => {
                header = None;
                let text = clean(raw);
                lines.push((
                    None,
                    LogLine {
                        at: None,
                        level: level_of(&text),
                        text,
                    },
                ));
            }
        }
    }
    (lines, cursor)
}

/// 解析 `docker logs --timestamps` 的输出：`时间 内容`（时间是 RFC 3339 的纳秒时间）。TTY 容器的行尾有 `\r`，去掉。
/// docker 分别写出容器的标准输出与标准错误，两者交错时先后不一定按时间：都有时间时按时间排好（稳定排序），游标取
/// 最晚的时间，下一次不会重复读到。
fn parse_docker_logs(text: &str) -> (Vec<(Option<i64>, LogLine)>, Option<LogCursor>) {
    let mut lines = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if raw.is_empty() {
            continue;
        }
        let (time, rest) = raw.split_once(' ').unwrap_or((raw, ""));
        let (nanos, content) = match parse_time(time) {
            Some(nanos) => (Some(nanos), rest),
            None => (None, raw),
        };
        let content = clean(content.strip_suffix('\r').unwrap_or(content));
        lines.push((
            nanos,
            LogLine {
                at: nanos.map(|n| n.div_euclid(1_000_000)),
                level: level_of(&content),
                text: content,
            },
        ));
    }
    if lines.iter().all(|(nanos, _)| nanos.is_some()) {
        lines.sort_by_key(|(nanos, _)| *nanos);
    }
    (lines, None)
}

/// 带时区的 ISO 8601 时间换成 Unix 纳秒：`2026-11-04T01:47:12+0000`（旧的 journalctl）、`…+00:00`、
/// `2026-11-04T09:49:04.944387787Z`（docker）、`…+08:00`（podman）。不是这样的时间时为 `None`。
pub(crate) fn parse_time(text: &str) -> Option<i64> {
    if text.len() < 20 || text.as_bytes().get(10) != Some(&b'T') {
        return None;
    }
    // +0000 → +00:00
    let normalized;
    let bytes = text.as_bytes();
    let text = if bytes.len() >= 5
        && matches!(bytes[bytes.len() - 5], b'+' | b'-')
        && bytes[bytes.len() - 4..].iter().all(u8::is_ascii_digit)
    {
        let (head, zone) = text.split_at(text.len() - 2);
        normalized = format!("{head}:{zone}");
        normalized.as_str()
    } else {
        text
    };
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()?
        .timestamp_nanos_opt()
}

/// 去掉终端的控制序列，其余的控制字符（制表符除外）换成 U+FFFD：不在界面上显示成乱码，也不影响按词判断级别。
fn clean(text: &str) -> String {
    let text = ANSI_RE.replace_all(text, "");
    if !text.chars().any(|c| c.is_control() && c != '\t') {
        return text.into_owned();
    }
    text.chars()
        .map(|c| {
            if c.is_control() && c != '\t' {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect()
}

/// 一行的级别（按关键词）。
pub fn level_of(text: &str) -> LogLevel {
    if ERROR_RE.is_match(text) {
        LogLevel::Error
    } else if WARNING_RE.is_match(text) {
        LogLevel::Warning
    } else {
        LogLevel::Normal
    }
}

impl HostTools {
    /// 读取服务或容器的日志（日志查看器、详情中的最近几行；跟随时给出上次的 [`LogResult::cursor`] 增量读取）。
    /// 在新的终端中跟随见 [`super::terminal_command`]。
    pub async fn logs(&self, target: &Target, query: LogQuery) -> Result<LogResult, ToolsError> {
        let command = logs_command(&query)?;
        let target = target.clone();
        tracing::debug!(source = ?query.source, follow = query.after.is_some(), "主机工具读取日志");
        on_runtime(async move {
            // 增量读取通常很快，不占长命令的名额（不拖慢列表与操作）
            let limits = Limits {
                timeout: if query.after.is_some() {
                    FOLLOW_TIMEOUT
                } else {
                    LIST_TIMEOUT
                },
                output: scripts::READ_TAIL + 4096,
                long: query.after.is_none(),
            };
            let result = run(&target, &command, limits).await?;
            parse_logs(&query, &result)
        })
        .await
    }
}

#[cfg(test)]
impl LogCursor {
    pub(crate) fn journal(cursor: &str) -> Self {
        LogCursor(Cursor::Journal(cursor.to_string()))
    }

    pub(crate) fn docker(nanos: i64) -> Self {
        LogCursor(Cursor::Docker(nanos))
    }
}
