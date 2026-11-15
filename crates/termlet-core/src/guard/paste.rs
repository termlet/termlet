//! 粘贴到终端前的检查（设置 `terminal.paste_warning`）。
//!
//! 控制字符一律去掉（与 Windows Terminal 相同：只留 Tab、换行、回车），防止粘贴的内容用 `ESC[201~`
//! 提前结束括号粘贴、或夹带其他终端控制序列；以下情况先请用户确认（确认框中可修改要粘贴的内容）：
//! - 多行，且远端没有开启括号粘贴（会逐行立即执行；开启时 shell 只把内容放进输入行，
//!   Windows Terminal 也只在此时提醒）
//! - 含零宽、双向控制等看不见的字符，或拉丁字母与西里尔、希腊字母混在一个词里（同形字，MobaXterm 的检查）
//! - 把下载的内容直接交给 shell 执行（`curl … | sh`）
//! - 命中危险命令规则（与撰写栏、CLI 相同的规则）
//!
//! 全屏程序（vim 等）中不提醒，只去掉控制字符。

use std::sync::LazyLock;

use regex::Regex;

use super::rules::{Guard, Rule};

/// 去掉控制字符后的内容。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sanitized {
    pub text: String,
    /// 去掉的控制字符数
    pub removed: usize,
}

/// C0（Tab、换行、回车除外）、DEL、C1。
fn is_control(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}' | '\u{7f}'..='\u{9f}')
}

/// 零宽、双向控制、软连字符等看不见的字符。
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{ad}'
            | '\u{200b}'..='\u{200f}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{feff}'
    )
}

/// 希腊字母、西里尔字母（与拉丁字母同形的大多在这两个区）。
fn is_greek_or_cyrillic(c: char) -> bool {
    matches!(c, '\u{370}'..='\u{3ff}' | '\u{400}'..='\u{4ff}')
}

/// 同形字检查中的词：字母、数字与 `_` `.` `-`（域名、文件名、命令名都算一个词）。
static WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\p{L}\p{N}_.-]+").expect("词的正则"));

/// 把下载的内容交给 shell 执行：`curl …`、`wget …` 之后经管道交给 `sh` / `bash` 等（可以经过 `sudo`）。
static PIPE_SHELL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?-u:\b)(?:curl|wget)(?-u:\b)[^\n|]*\|\s*(?:sudo\s+(?:-\S+\s+)*)?(?:ba|z|da|k)?sh(?-u:\b)")
        .expect("下载后交给 shell 的正则")
});

/// 同形字最多列出的词数。
const MAX_HOMOGLYPH_WORDS: usize = 5;
/// 逐行检查危险命令规则时最多检查的行数。
const MAX_DANGER_LINES: usize = 50;
/// 提示中显示的命令（见 [`one_line`]）最多的字符数。
pub const ONE_LINE_MAX: usize = 120;

/// 去掉控制字符（Tab、换行、回车除外）。
pub fn sanitize_paste(text: &str) -> Sanitized {
    let mut removed = 0;
    let text = text
        .chars()
        .filter(|&c| {
            let control = is_control(c);
            removed += usize::from(control);
            !control
        })
        .collect();
    Sanitized { text, removed }
}

/// 粘贴前需要提醒的情况。界面按变体选用文案。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PasteFinding {
    /// 去掉了控制字符
    Control { count: usize },
    /// 远端没有开启括号粘贴时的多行内容：粘贴时逐行立即执行
    Multiline { lines: usize },
    /// 看不见的字符（去重，按出现的顺序），界面写成 `U+200B`
    Invisible { chars: Vec<char> },
    /// 拉丁字母与西里尔、希腊字母混在一起的词（去重，按出现的顺序，最多 5 个）
    Homoglyph { words: Vec<String> },
    /// 把下载的内容直接交给 shell 执行（`curl … | sh`）
    PipeShell,
}

/// 检查（已去掉控制字符的）内容，返回需要提醒的情况，为空表示可以直接粘贴。
/// `bracketed` 表示远端开启了括号粘贴，`removed` 是 [`sanitize_paste`] 去掉的控制字符数。
///
/// 不检查危险命令规则，见 [`inspect_paste`]。
pub fn analyze_paste(text: &str, bracketed: bool, removed: usize) -> Vec<PasteFinding> {
    let mut findings = Vec::new();
    if removed > 0 {
        findings.push(PasteFinding::Control { count: removed });
    }
    if !bracketed {
        // 末尾的换行不算多出一行；中间的空行算
        let lines = normalize_newlines(text)
            .trim_end_matches('\n')
            .split('\n')
            .count();
        if lines > 1 {
            findings.push(PasteFinding::Multiline { lines });
        }
    }
    let mut invisible = Vec::new();
    for c in text.chars().filter(|&c| is_invisible(c)) {
        if !invisible.contains(&c) {
            invisible.push(c);
        }
    }
    if !invisible.is_empty() {
        findings.push(PasteFinding::Invisible { chars: invisible });
    }
    let mut words: Vec<String> = Vec::new();
    for w in WORD.find_iter(text).map(|m| m.as_str()) {
        let mixed =
            w.chars().any(|c| c.is_ascii_alphabetic()) && w.chars().any(is_greek_or_cyrillic);
        if mixed && !words.iter().any(|x| x == w) {
            words.push(w.to_string());
        }
    }
    if !words.is_empty() {
        words.truncate(MAX_HOMOGLYPH_WORDS);
        findings.push(PasteFinding::Homoglyph { words });
    }
    if PIPE_SHELL.is_match(text) {
        findings.push(PasteFinding::PipeShell);
    }
    findings
}

/// 空白：与 JavaScript 正则的 `\s` 相同，含 U+FEFF。
pub(super) fn is_space(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

/// 去掉首尾空白（见 [`is_space`]）。
pub(super) fn trim(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// 换行统一为 `\n`（`\r\n` 与单独的 `\r` 都算换行）。
pub(super) fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// 逐行检查危险命令规则用的行：去掉首尾空白与空行，最多 50 行。
pub fn paste_lines(text: &str) -> Vec<&str> {
    text.split(['\r', '\n'])
        .map(trim)
        .filter(|line| !line.is_empty())
        .take(MAX_DANGER_LINES)
        .collect()
}

/// 多行内容在提示中显示成一行：换行写作 ⏎，超过 `max` 个字符时截断（以 … 结尾）。
pub fn one_line(text: &str, max: usize) -> String {
    let joined = paste_lines(text).join(" ⏎ ");
    if joined.chars().count() <= max {
        return joined;
    }
    let mut out: String = joined.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// 命中的危险命令规则。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Danger {
    /// 提示中显示的命令：能定位到某一行时是那一行，否则是整段（见 [`one_line`]）
    pub line: String,
    pub rule: Rule,
}

/// 用危险命令规则检查要执行的内容（粘贴、同步输入、撰写栏共用）。
///
/// 逐行检查（去掉空行，最多 50 行），多行时还检查完整的内容：跨行的命令（`case` 语句的各个分支、
/// 交给 shell 执行的 heredoc）只有整段能认出。能定位到某一行时优先报那一行。
pub fn find_danger(guard: &Guard, text: &str) -> Option<Danger> {
    find_danger_with(text, |command| guard.check(command).cloned())
        .map(|(line, rule)| Danger { line, rule })
}

fn find_danger_with<R>(text: &str, mut test: impl FnMut(&str) -> Option<R>) -> Option<(String, R)> {
    let lines = paste_lines(text);
    for line in &lines {
        if let Some(hit) = test(line) {
            return Some((line.to_string(), hit));
        }
    }
    if lines.len() > 1 {
        return test(text).map(|hit| (one_line(text, ONE_LINE_MAX), hit));
    }
    None
}

/// 粘贴时终端的状态与设置。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PasteContext {
    /// 远端开启了括号粘贴；同步输入中粘贴到多个终端时，要所有终端都开启了才算
    pub bracketed: bool,
    /// 在全屏程序（vim 等）中：只去掉控制字符，不提醒；同步输入中要所有终端都在全屏程序中才算
    pub full_screen: bool,
    /// 设置 `terminal.paste_warning`
    pub warn: bool,
    /// 粘贴到的终端数：普通粘贴为 1，同步输入中为同步中的终端数
    pub targets: usize,
}

/// 粘贴前检查的结果：[`needs_confirm`](Self::needs_confirm) 时先显示确认框（列出原因，内容可以修改），
/// 否则直接粘贴 `text`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PasteCheck {
    /// 去掉控制字符后的内容：要粘贴的、确认框中显示的
    pub text: String,
    pub findings: Vec<PasteFinding>,
    pub danger: Option<Danger>,
    /// 粘贴前检查过危险命令规则：同步输入据此在紧接着的回车时不再重复确认，
    /// 见 [`checked_paste`](super::checked_paste)
    pub danger_checked: bool,
}

impl PasteCheck {
    pub fn needs_confirm(&self) -> bool {
        !self.findings.is_empty() || self.danger.is_some()
    }

    /// 最终粘贴 `text`（确认框中可能改过）时，粘贴的内容是否检查过危险命令规则：改过的视为没有检查过。
    pub fn danger_checked_for(&self, text: &str) -> bool {
        self.danger_checked && text == self.text
    }
}

/// 粘贴前的检查：去掉控制字符；设置开启提醒时，检查 [`analyze_paste`] 的各种情况
/// （`curl … | sh` 只在粘贴后会立即执行时提醒）。
///
/// 粘贴后会立即执行（没有开启括号粘贴、且含换行）时还检查危险命令规则：设置开启提醒，或同步输入中粘贴到多个终端时
/// （后者不受设置影响）。留在输入行里的内容不在这里检查：同步输入在回车时检查
/// （见 [`run_commands`](super::run_commands)）。
pub fn inspect_paste(guard: &Guard, text: &str, ctx: &PasteContext) -> PasteCheck {
    let Sanitized { text, removed } = sanitize_paste(text);
    let executes = !ctx.bracketed && text.contains(['\r', '\n']);
    let checks = executes && (ctx.warn || ctx.targets > 1);
    if ctx.full_screen || (!ctx.warn && !checks) {
        return PasteCheck {
            text,
            findings: Vec::new(),
            danger: None,
            danger_checked: false,
        };
    }
    let findings = if ctx.warn {
        analyze_paste(&text, ctx.bracketed, removed)
            .into_iter()
            .filter(|f| executes || *f != PasteFinding::PipeShell)
            .collect()
    } else {
        Vec::new()
    };
    let danger = if checks {
        find_danger(guard, &text)
    } else {
        None
    };
    PasteCheck {
        text,
        findings,
        danger,
        danger_checked: checks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_removes_control_chars() {
        // 保留 Tab、换行、回车
        assert_eq!(
            sanitize_paste("a\tb\r\nc\u{1b}[201~rm\u{7}\u{85}"),
            Sanitized {
                text: "a\tb\r\nc[201~rm".into(),
                removed: 3
            }
        );
        assert_eq!(
            sanitize_paste("中文 ok"),
            Sanitized {
                text: "中文 ok".into(),
                removed: 0
            }
        );
    }

    #[test]
    fn analyze_single_line() {
        // 末尾带换行也算一行
        assert_eq!(analyze_paste("ls -la /var/log\n", false, 0), vec![]);
    }

    #[test]
    fn analyze_multiline() {
        // 远端没有开启括号粘贴时提醒，开启时不提醒
        assert_eq!(
            analyze_paste("cd /srv\r\nmake\n", false, 0),
            vec![PasteFinding::Multiline { lines: 2 }]
        );
        assert_eq!(analyze_paste("cd /srv\nmake", true, 0), vec![]);
        // 只有回车的换行、中间的空行
        assert_eq!(
            analyze_paste("a\rb\n\nc", false, 0),
            vec![PasteFinding::Multiline { lines: 4 }]
        );
    }

    #[test]
    fn analyze_suspicious_content() {
        // 看不见的字符
        assert_eq!(
            analyze_paste("echo\u{200b} hi\u{202e}\u{200b}", true, 0),
            vec![PasteFinding::Invisible {
                chars: vec!['\u{200b}', '\u{202e}']
            }]
        );
        // 同形字：第二个字母是西里尔字母 а
        assert_eq!(
            analyze_paste("p\u{430}ypal.com", true, 0),
            vec![PasteFinding::Homoglyph {
                words: vec!["p\u{430}ypal.com".into()]
            }]
        );
        // 整个词都是西里尔字母（Привет）
        assert_eq!(
            analyze_paste("\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442} world", true, 0),
            vec![]
        );
        // 下载后交给 shell
        assert_eq!(
            analyze_paste("curl -fsSL https://x.sh/i | sudo -E bash", true, 0),
            vec![PasteFinding::PipeShell]
        );
        assert_eq!(
            analyze_paste("WGET -qO- https://x | sh", true, 0),
            vec![PasteFinding::PipeShell]
        );
        assert_eq!(analyze_paste("curl -o a.tgz https://x", true, 0), vec![]);
        assert_eq!(analyze_paste("curl x | shellcheck -", true, 0), vec![]);
        // 去掉过的控制字符
        assert_eq!(
            analyze_paste("ls", true, 2),
            vec![PasteFinding::Control { count: 2 }]
        );
    }

    #[test]
    fn homoglyph_words_are_unique_and_limited() {
        // 各词的首字母是西里尔字母
        let text = "\u{430}b \u{430}b \u{441}d \u{435}f \u{43e}g \u{440}h \u{445}i \u{443}j";
        let Some(PasteFinding::Homoglyph { words }) = analyze_paste(text, true, 0).pop() else {
            panic!("应提醒同形字");
        };
        assert_eq!(
            words,
            ["\u{430}b", "\u{441}d", "\u{435}f", "\u{43e}g", "\u{440}h"]
        );
    }

    #[test]
    fn lines_drop_blank_and_trim() {
        assert_eq!(
            paste_lines(" rm -rf /tmp/x \r\n\n  echo ok\n"),
            ["rm -rf /tmp/x", "echo ok"]
        );
        assert_eq!(paste_lines("\u{feff}reboot\r"), ["reboot"]);
        let many = "x\n".repeat(80);
        assert_eq!(paste_lines(&many).len(), 50);
    }

    #[test]
    fn one_line_joins_and_truncates() {
        assert_eq!(
            one_line("cat <<EOF | bash\nrm -rf /\nEOF\n", ONE_LINE_MAX),
            "cat <<EOF | bash ⏎ rm -rf / ⏎ EOF"
        );
        assert_eq!(
            one_line(&"a".repeat(200), 10),
            format!("{}…", "a".repeat(9))
        );
        // 按字符截断，不切开中文
        assert_eq!(one_line("重启服务器", 3), "重启…");
        assert_eq!(one_line("重启", 2), "重启");
    }

    /// 模拟规则：整段或某一行含 reboot 时命中；跨行的命令只有整段能认出。
    fn mock(seen: &mut Vec<String>) -> impl FnMut(&str) -> Option<&'static str> + '_ {
        let wrapped = Regex::new(r"rm -rf \\\n\s*/").unwrap();
        move |command| {
            seen.push(command.to_string());
            if command.contains("reboot") {
                Some("power")
            } else if wrapped.is_match(command) {
                Some("rm-root")
            } else {
                None
            }
        }
    }

    #[test]
    fn find_danger_prefers_line() {
        // 能定位到某一行时显示那一行
        let mut seen = Vec::new();
        assert_eq!(
            find_danger_with("echo ok\nsudo reboot\n", mock(&mut seen)),
            Some(("sudo reboot".into(), "power"))
        );
        assert_eq!(seen, ["echo ok", "sudo reboot"]);
    }

    #[test]
    fn find_danger_whole_text() {
        // 逐行都没有命中时检查完整的多行内容，只有整段能认出的命令显示整段
        let mut seen = Vec::new();
        assert_eq!(
            find_danger_with("rm -rf \\\n  /", mock(&mut seen)),
            Some(("rm -rf \\ ⏎ /".into(), "rm-root"))
        );
        assert_eq!(seen, ["rm -rf \\", "/", "rm -rf \\\n  /"]);
    }

    #[test]
    fn find_danger_single_line_and_empty() {
        // 单行只检查一次；空内容与没有命中时为 None
        let mut seen = Vec::new();
        assert_eq!(find_danger_with("  ls -la  ", mock(&mut seen)), None);
        assert_eq!(seen, ["ls -la"]);
        seen.clear();
        assert_eq!(find_danger_with("\n \n", mock(&mut seen)), None);
        assert!(seen.is_empty());
    }

    #[test]
    fn find_danger_with_rules() {
        let guard = Guard::default();
        let danger = find_danger(&guard, "echo ok\n  sudo reboot\n").expect("应命中");
        assert_eq!(danger.line, "sudo reboot");
        assert_eq!(danger.rule.id(), "power");
        // case 语句：分支所在的行单独检查时认不出，整段能认出
        let script = "case $1 in\n  stop) reboot ;;\nesac";
        let danger = find_danger(&guard, script).expect("应命中");
        assert_eq!(danger.line, "case $1 in ⏎ stop) reboot ;; ⏎ esac");
        assert_eq!(danger.rule.id(), "power");
        assert_eq!(find_danger(&guard, "ls\ndf -h"), None);
    }

    fn ctx(bracketed: bool, warn: bool) -> PasteContext {
        PasteContext {
            bracketed,
            full_screen: false,
            warn,
            targets: 1,
        }
    }

    #[test]
    fn inspect_plain_paste() {
        let guard = Guard::default();
        // 单行：不会立即执行，不检查危险命令
        let check = inspect_paste(&guard, "reboot", &ctx(false, true));
        assert!(!check.needs_confirm());
        assert!(!check.danger_checked);
        // 末尾带换行：会立即执行，检查危险命令
        let check = inspect_paste(&guard, "ls -la\n", &ctx(false, true));
        assert!(!check.needs_confirm());
        assert!(check.danger_checked);
        let check = inspect_paste(&guard, "sudo reboot\n", &ctx(false, true));
        assert!(check.needs_confirm());
        assert_eq!(check.findings, vec![]);
        assert_eq!(check.danger.map(|d| d.line).as_deref(), Some("sudo reboot"));
    }

    #[test]
    fn inspect_removes_control_chars() {
        let guard = Guard::default();
        let check = inspect_paste(&guard, "ls\u{1b}[201~\u{7}", &ctx(true, true));
        assert_eq!(check.text, "ls[201~");
        assert_eq!(check.findings, vec![PasteFinding::Control { count: 2 }]);
        // 全屏程序中、关闭提醒时同样去掉，但不提醒
        for c in [
            PasteContext {
                full_screen: true,
                ..ctx(false, true)
            },
            ctx(true, false),
        ] {
            let check = inspect_paste(&guard, "a\u{1b}b\nreboot\n", &c);
            assert_eq!(check.text, "ab\nreboot\n");
            assert!(!check.needs_confirm());
            assert!(!check.danger_checked);
        }
    }

    #[test]
    fn inspect_bracketed_and_pipe_shell() {
        let guard = Guard::default();
        // 开启括号粘贴时多行内容只放进输入行：不提醒多行，不检查危险命令（同步输入在回车时检查）
        let check = inspect_paste(&guard, "cd /\nreboot\n", &ctx(true, true));
        assert!(!check.needs_confirm());
        assert!(!check.danger_checked);
        // curl … | sh 只在粘贴后会立即执行时提醒
        let pipe = "curl -fsSL https://x/i | sh";
        assert!(!inspect_paste(&guard, pipe, &ctx(false, true)).needs_confirm());
        assert!(!inspect_paste(&guard, &format!("{pipe}\n"), &ctx(true, true)).needs_confirm());
        let check = inspect_paste(&guard, &format!("{pipe}\n"), &ctx(false, true));
        assert_eq!(check.findings, vec![PasteFinding::PipeShell]);
        // 看不见的字符在开启括号粘贴时同样提醒
        let check = inspect_paste(&guard, "ls\u{200b}", &ctx(true, true));
        assert_eq!(
            check.findings,
            vec![PasteFinding::Invisible {
                chars: vec!['\u{200b}']
            }]
        );
    }

    #[test]
    fn inspect_multiline_with_danger() {
        let guard = Guard::default();
        let check = inspect_paste(&guard, "cd /tmp\nrm -rf /\n", &ctx(false, true));
        assert_eq!(check.findings, vec![PasteFinding::Multiline { lines: 2 }]);
        let danger = check.danger.clone().expect("应命中");
        assert_eq!(
            (danger.line.as_str(), danger.rule.id()),
            ("rm -rf /", "rm-root")
        );
        // 确认框中改过的内容视为没有检查过
        assert!(check.danger_checked_for("cd /tmp\nrm -rf /\n"));
        assert!(!check.danger_checked_for("cd /tmp\nrm -rf /tmp/x\n"));
    }

    #[test]
    fn inspect_sync_input_always_checks_danger() {
        let guard = Guard::default();
        // 同步输入中粘贴到多个终端、会立即执行时，关闭提醒也检查危险命令（只检查危险命令）
        let sync = PasteContext {
            targets: 3,
            ..ctx(false, false)
        };
        let check = inspect_paste(&guard, "echo \u{200b}\nreboot\n", &sync);
        assert_eq!(check.findings, vec![]);
        assert_eq!(
            check.danger.map(|d| d.rule.id().to_string()).as_deref(),
            Some("power")
        );
        assert!(check.danger_checked);
        let check = inspect_paste(&guard, "ls\ndf\n", &sync);
        assert!(!check.needs_confirm());
        assert!(check.danger_checked);
        // 只粘贴到一个终端时按设置
        let check = inspect_paste(&guard, "reboot\n", &ctx(false, false));
        assert!(!check.needs_confirm());
        assert!(!check.danger_checked);
    }
}
