//! 脚本输出的分段解析，以及各页共用的小工具（用户名表、`p` 的结果、读不到时的错误）。
//!
//! 输出按 `@@段名 参数` 分段。远端可控的内容经脚本中的 `q` 在每行前加了一个空格，不会以 `@@` 开头，伪造不了段标记
//! （见 [`super::scripts`]）；解析时每个内容行去掉开头的一个空格。

use std::collections::HashMap;

use super::{SudoState, ToolsError};
use crate::ssh::ExecResult;

/// 输出中的一段：标记行是 `@@名称 参数`，之后到下一个标记行之前的各行是内容。
#[derive(Debug, Default)]
pub(crate) struct Section {
    pub arg: String,
    pub lines: Vec<String>,
}

/// 按段名（与"名称 参数"）找到的各段。
#[derive(Debug, Default)]
pub(crate) struct Sections {
    list: Vec<Section>,
    index: HashMap<String, usize>,
}

impl Sections {
    /// 按 `@@段名` 分段，以名称为键；带参数的段另以"名称 参数"为键（例如 `proc tcp6`）。同一个键只保留第一段。
    /// 没有经 `q` 的内容去掉一个空格也不影响解析：都按空白分列或去掉首尾空白。
    pub fn parse(out: &str) -> Self {
        let mut sections = Sections::default();
        let mut current: Option<usize> = None;
        for line in out.lines() {
            if let Some(rest) = line.strip_prefix("@@")
                && !rest.is_empty()
                && !rest.starts_with([' ', '\t'])
            {
                let (name, arg) = rest.split_once(' ').unwrap_or((rest, ""));
                let at = sections.list.len();
                sections.list.push(Section {
                    arg: arg.to_string(),
                    lines: Vec::new(),
                });
                sections.index.entry(name.to_string()).or_insert(at);
                if !arg.is_empty() {
                    sections.index.entry(format!("{name} {arg}")).or_insert(at);
                }
                current = Some(at);
                continue;
            }
            if let Some(at) = current {
                let content = line.strip_prefix(' ').unwrap_or(line);
                sections.list[at].lines.push(content.to_string());
            }
        }
        sections
    }

    pub fn get(&self, key: &str) -> Option<&Section> {
        self.index.get(key).map(|&at| &self.list[at])
    }

    pub fn has(&self, key: &str) -> bool {
        self.index.contains_key(key)
    }
}

/// 段的读取：不存在的段当作空段。
pub(crate) trait SectionExt<'a> {
    /// 第一行（去掉首尾空白），没有时为空。
    fn first(self) -> &'a str;
    /// 全部内容（各行以换行连接，内容中的换行原样保留），没有时为空。
    fn text(self) -> String;
    /// 第一行解析成整数，失败时为 `None`。
    fn num(self) -> Option<i64>;
    /// 段标记之后的参数解析成整数（例如 `@@total 123`），失败时为 `None`。
    fn num_arg(self) -> Option<i64>;
    /// 段标记之后的参数，没有这个段时为空。
    fn arg(self) -> &'a str;
    /// 全部非空行。
    fn all_lines(self) -> Vec<&'a str>;
}

impl<'a> SectionExt<'a> for Option<&'a Section> {
    fn first(self) -> &'a str {
        self.and_then(|s| s.lines.first())
            .map(|l| l.trim())
            .unwrap_or("")
    }

    fn text(self) -> String {
        self.map(|s| s.lines.join("\n")).unwrap_or_default()
    }

    fn num(self) -> Option<i64> {
        self.first().parse().ok()
    }

    fn num_arg(self) -> Option<i64> {
        self.and_then(|s| s.arg.trim().parse().ok())
    }

    fn arg(self) -> &'a str {
        self.map(|s| s.arg.as_str()).unwrap_or("")
    }

    fn all_lines(self) -> Vec<&'a str> {
        self.map(|s| {
            s.lines
                .iter()
                .map(String::as_str)
                .filter(|l| !l.trim().is_empty())
                .collect()
        })
        .unwrap_or_default()
    }
}

/// 解析 `/etc/passwd` 的 `名称:UID`（`@@passwd` 段），UID → 名称（同一 UID 取第一个）。
pub(crate) fn users(section: Option<&Section>) -> HashMap<u32, String> {
    let mut map = HashMap::new();
    let Some(section) = section else {
        return map;
    };
    for line in &section.lines {
        let Some((name, id)) = line.trim().split_once(':') else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        if let Ok(uid) = id.parse::<u32>() {
            map.entry(uid).or_insert_with(|| name.to_string());
        }
    }
    map
}

/// 第一行是 UID（`id -u` 的输出）。
pub(crate) fn uid_of(section: Option<&Section>) -> Option<u32> {
    section.num().and_then(|n| u32::try_from(n).ok())
}

/// 脚本中 `p` 报告的 sudo 情况。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PrivSudo {
    /// 没有用 sudo（直接执行成功，或失败的原因不是权限）
    NotTried,
    /// 用免密码 sudo 执行了
    Used,
    /// 需要 root，sudo 帮不上
    Root(SudoState),
}

impl PrivSudo {
    pub fn parse(word: &str) -> Self {
        match word.trim() {
            "used" => PrivSudo::Used,
            "password" => PrivSudo::Root(SudoState::Password),
            "denied" => PrivSudo::Root(SudoState::Denied),
            "none" => PrivSudo::Root(SudoState::NotInstalled),
            _ => PrivSudo::NotTried,
        }
    }
}

/// `p` 的输出：退出码、sudo 的情况与命令的输出。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PrivResult {
    pub code: u32,
    pub sudo: PrivSudo,
    pub output: String,
}

impl PrivResult {
    /// 解析 `p` 的输出；没有 `@@rc` 时为 `None`（脚本没能执行，例如没有 sh）。
    /// `@@out` 之后是命令的输出，其中以 `@@` 开头的行伪造不了前面的结果。
    pub fn parse(out: &str) -> Option<Self> {
        let (head, body) = out.split_once("@@out\n").unwrap_or((out, ""));
        let mut code = None;
        let mut sudo = PrivSudo::NotTried;
        for line in head.split('\n') {
            if let Some(rest) = line.strip_prefix("@@rc ") {
                if let Ok(n) = rest.trim().parse() {
                    code = Some(n);
                }
            } else if let Some(rest) = line.strip_prefix("@@sudo") {
                sudo = PrivSudo::parse(rest);
            }
        }
        Some(PrivResult {
            code: code?,
            sudo,
            output: body.strip_suffix('\n').unwrap_or(body).to_string(),
        })
    }

    /// 权限不足且没有可用的 sudo 时返回 [`ToolsError::NeedRoot`]。
    pub fn root_failure(&self) -> Option<ToolsError> {
        match self.sudo {
            PrivSudo::Root(sudo) if self.code != 0 => Some(ToolsError::NeedRoot {
                sudo,
                output: self.output.clone(),
            }),
            _ => None,
        }
    }
}

/// 标准错误摘要的长度上限（字节）。
const SUMMARY_LIMIT: usize = 300;

/// 去掉首尾空白，最多 `SUMMARY_LIMIT` 字节（不切断字符）。
pub(crate) fn summary(text: &str) -> String {
    let text = text.trim();
    let mut end = text.len().min(SUMMARY_LIMIT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// 第一行（去掉首尾空白，最多 300 字节）。
pub(crate) fn first_line(text: &str) -> String {
    let text = text.trim();
    summary(text.split('\n').next().unwrap_or(""))
}

/// 脚本没有输出预期的段时的错误：带上退出码与标准错误的开头，便于排查。
pub(crate) fn unreadable(result: &ExecResult) -> ToolsError {
    ToolsError::Unreadable {
        exit_code: result.exit_code,
        stderr: summary(&String::from_utf8_lossy(&result.stderr)),
    }
}

/// 解析时发现缺少预期的段：调用方换成带执行结果的 [`unreadable`]。
pub(crate) fn missing() -> ToolsError {
    ToolsError::Unreadable {
        exit_code: None,
        stderr: String::new(),
    }
}

/// 解析的错误中，缺少预期的段（[`missing`]）换成带执行结果的 [`unreadable`]；解析时已经说明了原因的（带着远端的
/// 错误输出，例如服务页连不上 systemd）原样保留。
pub(crate) fn with_result(error: ToolsError, result: &ExecResult) -> ToolsError {
    match error {
        ToolsError::Unreadable {
            exit_code: None,
            ref stderr,
        } if stderr.is_empty() => unreadable(result),
        other => other,
    }
}
