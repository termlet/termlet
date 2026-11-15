//! asciicast：asciinema 的终端录制格式（<https://docs.asciinema.org/manual/asciicast/v2/>）。
//!
//! - 写出 v2：第一行是头部（JSON 对象：版本、宽高、开始时间、标题、终端类型、配色），之后每行一个事件
//!   `[秒, 类型, 数据]`，时间从开始录制算起。类型：`o` 输出、`i` 输入、`r` 大小变化（`列x行`）、`m` 标记。
//!   数据是 JSON 字符串，必须是完整的 UTF-8：输出可以在任意位置切开，被切开的多字节字符由 [`Utf8Stream`] 留到下一段。
//! - 读入 v1（整个文件是一个 JSON 对象，`stdout` 中是相对上一帧的间隔）、v2、v3（大小在头部的 `term` 中，事件的时间
//!   是相对上一个事件的间隔，`#` 开头的行是注释，另有 `x` 退出）。都统一成 [`Cast`]：时间换算成从开始算起的秒数，
//!   不递减；宽高限制在 [`MAX_COLUMNS`] × [`MAX_ROWS`] 之内（防止恶意文件让终端分配巨大的网格）。
//! - 录制中途异常结束（程序崩溃、断电）时最后一行可能不完整：读入时跳过解析不了的行并计数（[`Cast::skipped`]），
//!   不当作错误。

use std::fmt::Write as _;
use std::io::{self, BufRead};

use serde::Deserialize;
use serde_json::{Map, Value};

/// 录制文件的扩展名。
pub const EXTENSION: &str = "cast";
/// 写出的版本。
pub const VERSION: u8 = 2;
/// 打开录制文件的大小上限：整个文件读进内存回放。
pub const MAX_READ_BYTES: u64 = 512 << 20;
/// 回放时网格的上限（头部与大小变化事件中更大的按上限）。
pub const MAX_COLUMNS: u16 = 1000;
pub const MAX_ROWS: u16 = 500;

/// 录制时的终端配色（asciinema 回放时据此着色）：`#rrggbb`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CastTheme {
    pub foreground: String,
    pub background: String,
    /// 16 色（8 或 16 个）
    pub palette: Vec<String>,
}

/// 录制文件的头部（各版本统一后的）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CastHeader {
    /// 开始时终端的列数、行数
    pub width: u16,
    pub height: u16,
    /// 开始录制的时间（Unix 秒）
    pub timestamp: Option<i64>,
    pub title: Option<String>,
    pub command: Option<String>,
    /// 录制者设的停顿上限（秒）：回放时更长的停顿缩短到它
    pub idle_time_limit: Option<f64>,
    /// 终端类型（`TERM`）
    pub term: Option<String>,
    pub theme: Option<CastTheme>,
}

/// 一个事件的内容。`Debug` 中输入只显示长度（可能含密码）。
#[derive(Clone, PartialEq)]
pub enum EventData {
    /// 终端的输出（`o`）
    Output(String),
    /// 用户的输入（`i`）
    Input(String),
    /// 终端大小变了（`r`）
    Resize { columns: u16, rows: u16 },
    /// 标记（`m`）：录制者在这里做的记号
    Marker(String),
    /// 程序退出（v3 的 `x`）
    Exit(i32),
}

impl std::fmt::Debug for EventData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EventData::Output(text) => f.debug_tuple("Output").field(text).finish(),
            EventData::Input(text) => write!(f, "Input({} 字节)", text.len()),
            EventData::Resize { columns, rows } => write!(f, "Resize({columns}x{rows})"),
            EventData::Marker(label) => f.debug_tuple("Marker").field(label).finish(),
            EventData::Exit(status) => write!(f, "Exit({status})"),
        }
    }
}

/// 一个事件：`time` 是从开始算起的秒数。
#[derive(Clone, Debug, PartialEq)]
pub struct CastEvent {
    pub time: f64,
    pub data: EventData,
}

/// 读入的录制文件。
#[derive(Clone, Debug, Default)]
pub struct Cast {
    /// 文件的版本（1、2、3）
    pub version: u8,
    pub header: CastHeader,
    /// 按时间排列
    pub events: Vec<CastEvent>,
    /// 解析不了而跳过的行（异常结束时不完整的最后一行等）
    pub skipped: usize,
}

impl Cast {
    /// 最后一个事件的时间（秒）。
    pub fn duration(&self) -> f64 {
        self.events.last().map_or(0., |e| e.time)
    }

    /// 输出的总字节数。
    pub fn output_bytes(&self) -> usize {
        self.events
            .iter()
            .map(|e| match &e.data {
                EventData::Output(text) => text.len(),
                _ => 0,
            })
            .sum()
    }
}

/// 读不了的原因。
#[derive(Debug, thiserror::Error)]
pub enum CastError {
    #[error("读取录制文件失败：{0}")]
    Io(#[from] io::Error),
    /// 第一行不是 asciicast 的头部
    #[error("不是 asciicast 录制文件")]
    NotCast,
    #[error("不支持的 asciicast 版本 {0}")]
    Version(u64),
    #[error("录制文件太大")]
    TooLarge,
}

// ------------------------------------------------------------ 写出

/// v2 的头部（一行 JSON，不含换行）。
pub fn header_line(header: &CastHeader) -> String {
    let mut map = Map::new();
    map.insert("version".into(), VERSION.into());
    map.insert("width".into(), header.width.into());
    map.insert("height".into(), header.height.into());
    if let Some(timestamp) = header.timestamp {
        map.insert("timestamp".into(), timestamp.into());
    }
    if let Some(limit) = header.idle_time_limit {
        map.insert("idle_time_limit".into(), limit.into());
    }
    if let Some(command) = &header.command {
        map.insert("command".into(), command.clone().into());
    }
    if let Some(title) = &header.title {
        map.insert("title".into(), title.clone().into());
    }
    // 环境变量只写终端类型：shell 的其他环境变量可能含秘密
    if let Some(term) = &header.term {
        let mut env = Map::new();
        env.insert("TERM".into(), term.clone().into());
        map.insert("env".into(), Value::Object(env));
    }
    if let Some(theme) = &header.theme {
        let mut value = Map::new();
        value.insert("fg".into(), theme.foreground.clone().into());
        value.insert("bg".into(), theme.background.clone().into());
        value.insert("palette".into(), theme.palette.join(":").into());
        map.insert("theme".into(), Value::Object(value));
    }
    Value::Object(map).to_string()
}

/// 把一个事件写成一行（不含换行）接在 `out` 后面：时间保留到微秒。
pub fn push_event(out: &mut String, time: f64, data: &EventData) {
    let (code, text): (&str, std::borrow::Cow<'_, str>) = match data {
        EventData::Output(text) => ("o", text.as_str().into()),
        EventData::Input(text) => ("i", text.as_str().into()),
        EventData::Resize { columns, rows } => ("r", format!("{columns}x{rows}").into()),
        EventData::Marker(label) => ("m", label.as_str().into()),
        EventData::Exit(status) => ("x", status.to_string().into()),
    };
    let time = if time.is_finite() { time.max(0.) } else { 0. };
    let _ = write!(out, "[{time:.6}, \"{code}\", ");
    // 字符串的转义（控制字符写成 \u001b 等）交给 serde_json；写进 String 不会失败
    out.push_str(&Value::String(text.into_owned()).to_string());
    out.push(']');
}

/// 把字节流切成完整的 UTF-8 文字：末尾被切开的多字节字符留到下一段，不合法的字节换成 U+FFFD。
#[derive(Debug, Default)]
pub struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    /// 接上一段字节，返回其中完整的文字（可能为空）。
    pub fn push(&mut self, bytes: &[u8]) -> String {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(bytes);
        let mut out = String::with_capacity(data.len());
        let mut rest = data.as_slice();
        loop {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    out.push_str(text);
                    break;
                }
                Err(e) => {
                    let (valid, after) = rest.split_at(e.valid_up_to());
                    // valid_up_to 之前一定是合法的 UTF-8
                    out.push_str(std::str::from_utf8(valid).unwrap_or_default());
                    match e.error_len() {
                        Some(len) => {
                            out.push(char::REPLACEMENT_CHARACTER);
                            rest = &after[len..];
                        }
                        // 末尾不完整：留到下一段
                        None => {
                            self.pending = after.to_vec();
                            break;
                        }
                    }
                }
            }
        }
        out
    }

    /// 结束：留着的不完整字符换成 U+FFFD（没有时为空）。
    pub fn finish(&mut self) -> String {
        if std::mem::take(&mut self.pending).is_empty() {
            String::new()
        } else {
            char::REPLACEMENT_CHARACTER.to_string()
        }
    }
}

// ------------------------------------------------------------ 读入

/// 从文件读入（整个读进内存）。
pub fn read_file(path: &std::path::Path) -> Result<Cast, CastError> {
    let file = std::fs::File::open(path)?;
    if file.metadata()?.len() > MAX_READ_BYTES {
        return Err(CastError::TooLarge);
    }
    parse(io::BufReader::new(file))
}

/// 读入 asciicast（v1、v2、v3）。
pub fn parse(mut reader: impl BufRead) -> Result<Cast, CastError> {
    let mut first = String::new();
    // 跳过开头的空行
    while first.trim().is_empty() {
        first.clear();
        if reader.read_line(&mut first)? == 0 {
            return Err(CastError::NotCast);
        }
    }
    let first_line = first.trim_start_matches('\u{feff}').trim();
    match serde_json::from_str::<Value>(first_line) {
        Ok(Value::Object(header)) => match header.get("version").and_then(Value::as_u64) {
            Some(1) => parse_v1(first_line),
            Some(2) => Ok(parse_lines(2, &header, reader)),
            Some(3) => Ok(parse_lines(3, &header, reader)),
            Some(other) => Err(CastError::Version(other)),
            None => Err(CastError::NotCast),
        },
        // 第一行不是完整的 JSON：v1 可以排成多行，读完整个文件再试
        _ => {
            let mut text = first;
            reader.read_to_string(&mut text)?;
            parse_v1(text.trim_start_matches('\u{feff}'))
        }
    }
}

/// 列数、行数限制在上限之内（至少 1）。
fn clamp_size(columns: u64, rows: u64) -> (u16, u16) {
    (
        columns.clamp(1, MAX_COLUMNS as u64) as u16,
        rows.clamp(1, MAX_ROWS as u64) as u16,
    )
}

fn string_of(map: &Map<String, Value>, key: &str) -> Option<String> {
    map.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn theme_of(value: Option<&Value>) -> Option<CastTheme> {
    let map = value?.as_object()?;
    Some(CastTheme {
        foreground: string_of(map, "fg")?,
        background: string_of(map, "bg")?,
        palette: map
            .get("palette")
            .and_then(Value::as_str)
            .map(|p| p.split(':').map(str::to_string).collect())
            .unwrap_or_default(),
    })
}

/// v2、v3 的头部。
fn header_of(version: u8, map: &Map<String, Value>) -> CastHeader {
    let (columns, rows, term, theme) = if version == 3 {
        let term = map.get("term").and_then(Value::as_object);
        let get = |key| term.and_then(|t| t.get(key)).and_then(Value::as_u64);
        (
            get("cols").unwrap_or(80),
            get("rows").unwrap_or(24),
            term.and_then(|t| string_of(t, "type")),
            theme_of(term.and_then(|t| t.get("theme"))),
        )
    } else {
        let env = map.get("env").and_then(Value::as_object);
        (
            map.get("width").and_then(Value::as_u64).unwrap_or(80),
            map.get("height").and_then(Value::as_u64).unwrap_or(24),
            env.and_then(|env| string_of(env, "TERM")),
            theme_of(map.get("theme")),
        )
    };
    let (width, height) = clamp_size(columns, rows);
    CastHeader {
        width,
        height,
        timestamp: map.get("timestamp").and_then(Value::as_i64),
        title: string_of(map, "title"),
        command: string_of(map, "command"),
        idle_time_limit: map
            .get("idle_time_limit")
            .and_then(Value::as_f64)
            .filter(|limit| limit.is_finite() && *limit > 0.),
        term,
        theme,
    }
}

/// 一个事件行：`[时间, 类型, 数据]`。
#[derive(Deserialize)]
struct RawEvent(f64, String, String);

/// 事件的内容；不认识的类型、写错的大小为 `None`。
fn event_data(code: &str, data: String) -> Option<EventData> {
    Some(match code {
        "o" => EventData::Output(data),
        "i" => EventData::Input(data),
        "m" => EventData::Marker(data),
        "x" => EventData::Exit(data.trim().parse().ok()?),
        "r" => {
            let (columns, rows) = data.trim().split_once('x')?;
            let (columns, rows) =
                clamp_size(columns.trim().parse().ok()?, rows.trim().parse().ok()?);
            EventData::Resize { columns, rows }
        }
        _ => return None,
    })
}

/// v2、v3：头部之后每行一个事件。v2 的时间从开始算起，v3 是相对上一个事件的间隔。
fn parse_lines(version: u8, header: &Map<String, Value>, reader: impl BufRead) -> Cast {
    let mut cast = Cast {
        version,
        header: header_of(version, header),
        ..Cast::default()
    };
    let mut clock = 0f64;
    for line in reader.lines() {
        let Ok(line) = line else {
            // 不是 UTF-8 等：跳过这一行
            cast.skipped += 1;
            continue;
        };
        let line = line.trim();
        if line.is_empty() || (version == 3 && line.starts_with('#')) {
            continue;
        }
        let Ok(RawEvent(time, code, data)) = serde_json::from_str::<RawEvent>(line) else {
            cast.skipped += 1;
            continue;
        };
        if !time.is_finite() {
            cast.skipped += 1;
            continue;
        }
        // 时间不递减：乱序的按前一个的时间
        clock = if version == 3 {
            clock + time.max(0.)
        } else {
            time.max(clock)
        };
        if let Some(data) = event_data(&code, data) {
            cast.events.push(CastEvent { time: clock, data });
        }
    }
    cast
}

/// v1：整个文件是一个 JSON 对象，`stdout` 中每一帧是 `[相对上一帧的间隔, 输出]`。
fn parse_v1(text: &str) -> Result<Cast, CastError> {
    #[derive(Deserialize)]
    struct V1 {
        version: u64,
        #[serde(default)]
        width: Option<u64>,
        #[serde(default)]
        height: Option<u64>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        env: Option<std::collections::HashMap<String, String>>,
        stdout: Vec<(f64, String)>,
    }
    let v1: V1 = serde_json::from_str(text).map_err(|_| CastError::NotCast)?;
    if v1.version != 1 {
        return Err(CastError::Version(v1.version));
    }
    let (width, height) = clamp_size(v1.width.unwrap_or(80), v1.height.unwrap_or(24));
    let mut clock = 0f64;
    let events = v1
        .stdout
        .into_iter()
        .map(|(delay, data)| {
            if delay.is_finite() {
                clock += delay.max(0.);
            }
            CastEvent {
                time: clock,
                data: EventData::Output(data),
            }
        })
        .collect();
    Ok(Cast {
        version: 1,
        header: CastHeader {
            width,
            height,
            title: v1.title.filter(|t| !t.is_empty()),
            command: v1.command.filter(|c| !c.is_empty()),
            term: v1.env.and_then(|env| env.get("TERM").cloned()),
            ..CastHeader::default()
        },
        events,
        skipped: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(text: &str) -> Cast {
        parse(text.as_bytes()).unwrap()
    }

    fn outputs(cast: &Cast) -> Vec<(f64, &str)> {
        cast.events
            .iter()
            .filter_map(|e| match &e.data {
                EventData::Output(text) => Some((e.time, text.as_str())),
                _ => None,
            })
            .collect()
    }

    // 写出的头部与事件能原样读回；控制字符、引号、反斜杠、中文都经 JSON 转义
    #[test]
    fn written_v2_reads_back() {
        let header = CastHeader {
            width: 120,
            height: 30,
            timestamp: Some(1_762_670_000),
            title: Some("web-1 \"prod\"".into()),
            term: Some("xterm-256color".into()),
            theme: Some(CastTheme {
                foreground: "#d4d4d8".into(),
                background: "#1e1e1f".into(),
                palette: vec!["#000000".into(); 16],
            }),
            ..CastHeader::default()
        };
        let mut text = header_line(&header);
        assert!(
            text.contains(r#""env":{"TERM":"xterm-256color"}"#),
            "{text}"
        );
        let events = [
            (
                0.0,
                EventData::Output("\x1b[1;32m你好\x1b[0m \\ \"q\"\r\n".into()),
            ),
            (
                0.25,
                EventData::Resize {
                    columns: 100,
                    rows: 40,
                },
            ),
            (1.5, EventData::Input("ls\r".into())),
            (2.000_001_4, EventData::Marker("断开".into())),
        ];
        for (time, data) in &events {
            text.push('\n');
            push_event(&mut text, *time, data);
        }
        assert!(text.contains(r#"[0.250000, "r", "100x40"]"#), "{text}");
        let cast = parse_str(&text);
        assert_eq!(cast.version, 2);
        assert_eq!(cast.header, header);
        assert_eq!(cast.skipped, 0);
        let read: Vec<_> = cast
            .events
            .iter()
            .map(|e| (e.time, e.data.clone()))
            .collect();
        let expected: Vec<_> = events
            .iter()
            .map(|(t, d)| (((t * 1e6).round() / 1e6), d.clone()))
            .collect();
        assert_eq!(read, expected);
    }

    // 多字节字符被切在两段之间：前一段留下不完整的部分，接上后一段再输出；不合法的字节换成 U+FFFD
    #[test]
    fn utf8_split_across_chunks() {
        let text = "中文😀";
        let bytes = text.as_bytes();
        let mut stream = Utf8Stream::default();
        let mut out = String::new();
        for chunk in bytes.chunks(1) {
            out.push_str(&stream.push(chunk));
        }
        assert_eq!(out, text);
        let mut stream = Utf8Stream::default();
        assert_eq!(stream.push(&bytes[..2]), "");
        assert_eq!(stream.push(&bytes[2..4]), "中");
        assert_eq!(stream.push(&bytes[4..]), "文😀");
        assert_eq!(stream.push(b"a\xffb\xe4"), "a\u{fffd}b");
        assert_eq!(stream.finish(), "\u{fffd}");
        assert_eq!(stream.finish(), "");
    }

    // 异常结束时不完整的最后一行、坏行跳过并计数；时间不递减；超大的宽高按上限
    #[test]
    fn truncated_and_malformed_lines_are_skipped() {
        let text = concat!(
            "{\"version\": 2, \"width\": 99999, \"height\": 0, \"idle_time_limit\": 1.5}\n",
            "[0.5, \"o\", \"a\"]\n",
            "\n",
            "not json\n",
            "[0.2, \"o\", \"b\"]\n",
            "[1.0, \"r\", \"5000x3\"]\n",
            "[1.1, \"q\", \"unknown\"]\n",
            "[3.0, \"o\", \"c",
        );
        let cast = parse_str(text);
        assert_eq!((cast.header.width, cast.header.height), (MAX_COLUMNS, 1));
        assert_eq!(cast.header.idle_time_limit, Some(1.5));
        assert_eq!(cast.skipped, 2);
        assert_eq!(outputs(&cast), [(0.5, "a"), (0.5, "b")]);
        assert_eq!(
            cast.events[2].data,
            EventData::Resize {
                columns: MAX_COLUMNS,
                rows: 3
            }
        );
        assert_eq!(cast.duration(), 1.0);
    }

    // 超长停顿原样读入（回放时再按停顿上限缩短）
    #[test]
    fn long_pauses_are_kept() {
        let text =
            "{\"version\":2,\"width\":80,\"height\":24}\n[0.1,\"o\",\"a\"]\n[3600.1,\"o\",\"b\"]\n";
        let cast = parse_str(text);
        assert_eq!(outputs(&cast), [(0.1, "a"), (3600.1, "b")]);
    }

    #[test]
    fn v1_and_v3_are_read() {
        let v1 = r#"{
            "version": 1, "width": 100, "height": 30, "duration": 1.5, "title": "demo",
            "env": {"TERM": "xterm", "SHELL": "/bin/bash"},
            "stdout": [[0.5, "a"], [1.0, "b"]]
        }"#;
        let cast = parse_str(v1);
        assert_eq!(cast.version, 1);
        assert_eq!((cast.header.width, cast.header.height), (100, 30));
        assert_eq!(cast.header.title.as_deref(), Some("demo"));
        assert_eq!(cast.header.term.as_deref(), Some("xterm"));
        assert_eq!(outputs(&cast), [(0.5, "a"), (1.5, "b")]);

        let v3 = concat!(
            "{\"version\":3,\"term\":{\"cols\":90,\"rows\":20,\"type\":\"xterm-256color\"},\"timestamp\":1700000000}\n",
            "# 注释\n",
            "[0.5, \"o\", \"a\"]\n",
            "[0.25, \"r\", \"100x40\"]\n",
            "[1.0, \"o\", \"b\"]\n",
            "[0.0, \"x\", \"0\"]\n",
        );
        let cast = parse_str(v3);
        assert_eq!(cast.version, 3);
        assert_eq!((cast.header.width, cast.header.height), (90, 20));
        assert_eq!(cast.header.timestamp, Some(1_700_000_000));
        assert_eq!(outputs(&cast), [(0.5, "a"), (1.75, "b")]);
        assert_eq!(cast.events.last().unwrap().data, EventData::Exit(0));
    }

    #[test]
    fn other_files_are_rejected() {
        assert!(matches!(parse("".as_bytes()), Err(CastError::NotCast)));
        assert!(matches!(
            parse("hello world\n".as_bytes()),
            Err(CastError::NotCast)
        ));
        assert!(matches!(
            parse("{\"name\": 1}\n".as_bytes()),
            Err(CastError::NotCast)
        ));
        assert!(matches!(
            parse("{\"version\": 9}\n".as_bytes()),
            Err(CastError::Version(9))
        ));
    }
}
