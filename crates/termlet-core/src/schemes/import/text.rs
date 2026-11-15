//! 按文本处理的小解析器：JSONC 的注释与尾随逗号、TOML 子集、INI，以及配色文件的解码。
//!
//! 只处理配色所需的部分，坏输入只会得到空结果，不会出错；按行处理时跳过超长的行。

use std::collections::HashMap;

use encoding_rs::{Encoding, GBK, UTF_16BE, UTF_16LE};

/// 按行解析的格式中，超过这个长度（字节）的行直接跳过。
pub(super) const MAX_LINE: usize = 4096;

// ---- JSONC ----

/// 去掉 JSONC 中的注释与尾随逗号，字符串中的内容不动：`//` 与 `/* */` 注释、行首的 `#` 注释
/// （Tokyo Night 等项目的移植文件开头有 `#` 注释）、`}` 或 `]` 前的逗号（逗号与括号之间可以隔着注释）。
/// Windows Terminal 的 `settings.json` 就是 JSONC。
pub(crate) fn strip_jsonc(text: &str) -> String {
    strip_trailing_commas(&strip_comments(text))
}

/// 从 `i`（双引号）开始的 JSON 字符串的结束位置（结束的双引号之后）；没有结束时为文本末尾。
fn json_string_end(b: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return j + 1,
            _ => j += 1,
        }
    }
    b.len()
}

fn strip_comments(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut line_start = true;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        if c == b'"' {
            let end = json_string_end(b, i);
            out.extend_from_slice(&b[i..end]);
            i = end;
            line_start = false;
        } else if (c == b'/' && next == Some(b'/')) || (c == b'#' && line_start) {
            // 行注释：保留换行
            match b[i..].iter().position(|&x| x == b'\n') {
                Some(n) => i += n,
                None => break,
            }
        } else if c == b'/' && next == Some(b'*') {
            match find(&b[i + 2..], b"*/") {
                Some(n) => {
                    out.push(b' ');
                    i += 2 + n + 2;
                }
                None => break,
            }
        } else {
            out.push(c);
            match c {
                b'\n' => line_start = true,
                b' ' | b'\t' | b'\r' => {}
                _ => line_start = false,
            }
            i += 1;
        }
    }
    // 只在 ASCII 字符处切分，结果仍是合法的 UTF-8
    String::from_utf8(out).unwrap_or_default()
}

fn strip_trailing_commas(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let end = json_string_end(b, i);
                out.extend_from_slice(&b[i..end]);
                i = end;
                continue;
            }
            b',' => {
                let rest = b[i + 1..]
                    .iter()
                    .find(|&&x| !matches!(x, b' ' | b'\t' | b'\r' | b'\n'));
                if matches!(rest, Some(b'}' | b']')) {
                    i += 1;
                    continue;
                }
            }
            _ => {}
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_default()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ---- 通用 ----

/// 按顶层逗号切分（引号、括号内的逗号不切），去掉各段两端的空白与空段。
pub(super) fn split_top_level(s: &str) -> Vec<&str> {
    let b = s.as_bytes();
    let mut parts = Vec::new();
    let mut depth = 0i64;
    let mut quote = 0u8;
    let mut start = 0;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if quote != 0 {
            if c == quote {
                quote = 0;
            } else if c == b'\\' && quote == b'"' {
                i += 1;
            }
        } else {
            match c {
                b'"' | b'\'' => quote = c,
                b'[' | b'{' => depth += 1,
                b']' | b'}' => depth -= 1,
                b',' if depth == 0 => {
                    parts.push(&s[start..i]);
                    start = i + 1;
                }
                _ => {}
            }
        }
        i += 1;
    }
    parts.push(&s[start..]);
    parts
        .into_iter()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect()
}

/// 去掉开头与结尾各一个引号（`"` 或 `'`）。
pub(super) fn strip_quote_chars(s: &str) -> &str {
    let s = s.strip_prefix(['"', '\'']).unwrap_or(s);
    s.strip_suffix(['"', '\'']).unwrap_or(s)
}

// ---- TOML 子集 ----

/// TOML 拍平后最多的路径数。
const MAX_TOML_PATHS: usize = 10_000;

/// 解析 TOML 的一个子集：`[表]`、`键 = 值`（含点号键与行内表），结果按小写的点号路径拍平。
/// 表数组（`[[...]]`，如 Alacritty 的 `indexed_colors`）中的键不需要，跳过。
pub(super) fn parse_toml_lite(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    // None 表示在表数组中
    let mut table = Some(String::new());
    for line in text.split('\n') {
        if line.len() > MAX_LINE {
            continue;
        }
        let s = strip_toml_comment(line).trim();
        if s.is_empty() {
            continue;
        }
        if s.starts_with("[[") {
            table = None;
            continue;
        }
        if let Some(name) = s
            .strip_prefix('[')
            .and_then(|r| r.strip_suffix(']'))
            .filter(|n| !n.is_empty() && !n.contains(']'))
        {
            table = Some(toml_key(name));
            continue;
        }
        let Some(prefix) = &table else {
            continue;
        };
        if let Some(eq) = s.find('=').filter(|&eq| eq > 0) {
            toml_put(&mut out, prefix, &s[..eq], &s[eq + 1..]);
        }
    }
    out
}

/// 去掉引号外的 `#` 及其后的内容。
fn strip_toml_comment(line: &str) -> &str {
    let mut quote = 0u8;
    for (i, c) in line.bytes().enumerate() {
        if quote != 0 {
            if c == quote {
                quote = 0;
            }
        } else if c == b'"' || c == b'\'' {
            quote = c;
        } else if c == b'#' {
            return &line[..i];
        }
    }
    line
}

/// 键规范为小写的点号路径，各段去掉两端的空白与引号。
fn toml_key(key: &str) -> String {
    key.split('.')
        .map(|p| strip_quote_chars(p.trim()).to_lowercase())
        .collect::<Vec<_>>()
        .join(".")
}

/// 记下一个键值；值是行内表时展开（用显式的栈，不递归）。
fn toml_put(out: &mut HashMap<String, String>, prefix: &str, key: &str, value: &str) {
    let mut work = vec![(prefix.to_string(), key, value)];
    while let Some((prefix, key, value)) = work.pop() {
        let key = toml_key(key);
        let path = match (prefix.is_empty(), key.is_empty()) {
            (true, _) => key,
            (false, true) => prefix,
            (false, false) => format!("{prefix}.{key}"),
        };
        let v = value.trim();
        if let Some(body) = v.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
            // 倒序入栈，按书写顺序处理
            for part in split_top_level(body).into_iter().rev() {
                if let Some(eq) = part.find('=').filter(|&eq| eq > 0) {
                    work.push((path.clone(), &part[..eq], &part[eq + 1..]));
                }
            }
            continue;
        }
        if out.len() < MAX_TOML_PATHS {
            out.insert(path, unquote(v).to_string());
        }
    }
}

/// 去掉成对的引号。
fn unquote(v: &str) -> &str {
    let s = v.trim();
    let b = s.as_bytes();
    if b.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[b.len() - 1] == b[0] {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

// ---- INI ----

pub(super) struct IniSection<'a> {
    pub name: &'a str,
    pub entries: HashMap<String, &'a str>,
}

/// INI：`[段]` 与 `键=值`，键转为小写；`;` 与 `#` 开头的行是注释。第一段是段头之前的内容（名称为空）。
pub(super) fn parse_ini(text: &str) -> Vec<IniSection<'_>> {
    let mut sections = vec![IniSection {
        name: "",
        entries: HashMap::new(),
    }];
    for line in text.split('\n') {
        let t = line.trim();
        if t.is_empty() || t.starts_with(';') || t.starts_with('#') {
            continue;
        }
        if let Some(name) = t.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            sections.push(IniSection {
                name: name.trim(),
                entries: HashMap::new(),
            });
            continue;
        }
        if let Some(eq) = t.find('=').filter(|&eq| eq > 0)
            && let Some(cur) = sections.last_mut()
        {
            cur.entries
                .insert(t[..eq].trim().to_lowercase(), t[eq + 1..].trim());
        }
    }
    sections
}

// ---- 解码 ----

/// 把读到的配色文件字节解码为文本：识别 UTF-8 BOM、UTF-16 LE / BE BOM；没有 BOM 时按零字节的分布判断
/// 无 BOM 的 UTF-16，否则按 UTF-8，不是合法 UTF-8 时按 GBK（中文版 Xshell 导出的 `.xcs`）。
pub fn decode_scheme_file(bytes: &[u8]) -> String {
    decode_with_fallback(bytes, GBK)
}

/// 同 [`decode_scheme_file`]，不是合法 UTF-8 时按 `fallback` 解码（本机已装客户端的文件按系统的 ANSI 代码页）。
pub(crate) fn decode_with_fallback(bytes: &[u8], fallback: &'static Encoding) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    if let Some(rest) = bytes.strip_prefix(&[0xff, 0xfe]) {
        return UTF_16LE.decode_without_bom_handling(rest).0.into_owned();
    }
    if let Some(rest) = bytes.strip_prefix(&[0xfe, 0xff]) {
        return UTF_16BE.decode_without_bom_handling(rest).0.into_owned();
    }
    if let Some(encoding) = bomless_utf16(bytes) {
        return encoding.decode_without_bom_handling(bytes).0.into_owned();
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => fallback.decode_without_bom_handling(bytes).0.into_owned(),
    }
}

/// 无 BOM 的 UTF-16：ASCII 文本的高字节全是 0。中文字符的高、低字节都可能是 0，只要求另一侧的零明显更少。
fn bomless_utf16(bytes: &[u8]) -> Option<&'static Encoding> {
    let sample = bytes.len().min(1024) & !1;
    if sample < 4 {
        return None;
    }
    let (mut even, mut odd) = (0usize, 0usize);
    for pair in bytes[..sample].as_chunks::<2>().0 {
        even += usize::from(pair[0] == 0);
        odd += usize::from(pair[1] == 0);
    }
    let half = sample / 2;
    if odd * 10 > half * 4 && even * 4 < odd {
        Some(UTF_16LE)
    } else if even * 10 > half * 4 && odd * 4 < even {
        Some(UTF_16BE)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_jsonc_keeps_strings() {
        let input = "{\"a\": \"x // y /* z */\", // 行注释\n \"b\": [1, 2, /* 块 */ ], \"c\": \"q\\\"\",\n}";
        let v: serde_json::Value = serde_json::from_str(&strip_jsonc(input)).unwrap();
        assert_eq!(v["a"], "x // y /* z */");
        assert_eq!(v["c"], "q\"");
        assert_eq!(v["b"].as_array().map(Vec::len), Some(2));
        // 没有结束的注释、字符串不会出错
        for s in [
            "{ /* 没有结束",
            "{ \"a\": \"没有结束",
            "// 只有注释",
            "\"\\",
        ] {
            let _ = strip_jsonc(s);
        }
        // 行首的 # 是注释，字符串中的不是
        let v: serde_json::Value =
            serde_json::from_str(&strip_jsonc("# 注释\n  # 缩进的注释\n{\"x\": \"#a\"}")).unwrap();
        assert_eq!(v["x"], "#a");
    }

    #[test]
    fn toml_lite() {
        let flat = parse_toml_lite(
            "[Colors.Primary] # 注释\nBackground = '#000000'\n[colors]\nnormal = { red = \"#ff0000\", x = { y = 1 } }\n[[colors.indexed_colors]]\ncolor = \"0x111111\"\n",
        );
        assert_eq!(
            flat.get("colors.primary.background").map(String::as_str),
            Some("#000000")
        );
        assert_eq!(
            flat.get("colors.normal.red").map(String::as_str),
            Some("#ff0000")
        );
        assert_eq!(flat.get("colors.normal.x.y").map(String::as_str), Some("1"));
        assert!(!flat.values().any(|v| v == "0x111111"));
        // 很深的行内表不会栈溢出
        let deep = format!("[colors]\n{}\n", "x = {".repeat(800));
        let _ = parse_toml_lite(&deep);
        let nested = format!(
            "[colors]\na = {}1{}\n",
            "{ b = ".repeat(600),
            " }".repeat(600)
        );
        let _ = parse_toml_lite(&nested);
    }

    #[test]
    fn ini_sections() {
        let ini = parse_ini("a=1\n[ Colors ]\n; 注释\nBlack = 0,0,0\n=x\n[b]\n");
        assert_eq!(ini.len(), 3);
        assert_eq!(ini[0].entries.get("a"), Some(&"1"));
        assert_eq!(ini[1].name, "Colors");
        assert_eq!(ini[1].entries.get("black"), Some(&"0,0,0"));
        assert_eq!(ini[1].entries.len(), 1);
    }

    #[test]
    fn split_and_quotes() {
        assert_eq!(
            split_top_level(" 'a,b', [1, 2], {x: 1, y: 2} ,, \"c\\\",\" "),
            ["'a,b'", "[1, 2]", "{x: 1, y: 2}", "\"c\\\",\""]
        );
        assert_eq!(strip_quote_chars("\"abc'"), "abc");
        assert_eq!(strip_quote_chars("'"), "");
        assert_eq!(unquote("'x'"), "x");
        assert_eq!(unquote("'x\""), "'x\"");
    }
}
