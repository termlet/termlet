//! 同步输入（与 MobaXterm 的 MultiExec 相同）中按输入推算待执行的命令行，按回车前用危险命令规则检查。
//! 这里只管与命令文本有关的部分；网格排布、终端回应的过滤、光标键与括号粘贴的转换属于终端。
//!
//! 推算只认最基本的行编辑：光标移动后的编辑推算不准，所以按回车前还要读各终端屏幕上的输入行，
//! 两者一起检查（见 [`run_commands`]）。

use std::collections::HashSet;

use super::paste::{is_space, normalize_newlines, paste_lines, trim};

/// 推算的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FeedResult {
    /// 还没有执行的输入行
    pub line: String,
    /// 这次输入中按回车交给 shell 执行的各行
    pub executed: Vec<String>,
}

/// 按输入推算命令行：`line` 是之前输入的内容，`data` 是这次的输入（终端发出的字节按 UTF-8 解码）。
///
/// 回车（或换行）时这一行交给 shell 执行，放进 `executed`；Backspace 删去最后一个字符，Ctrl+W 删去最后一个词，
/// Ctrl+C、Ctrl+U、Ctrl+D 清空；方向键等控制序列与 Tab 等其余控制字符忽略。
pub fn feed_line(line: &str, data: &str) -> FeedResult {
    let chars: Vec<char> = data.chars().collect();
    let mut cur = line.to_string();
    let mut executed = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\x1b' => {
                i = skip_escape(&chars, i);
                continue;
            }
            '\r' | '\n' => executed.push(std::mem::take(&mut cur)),
            '\x7f' | '\x08' => {
                cur.pop();
            }
            '\x17' => delete_word(&mut cur),
            '\x03' | '\x15' | '\x04' => cur.clear(),
            c if c >= ' ' => cur.push(c),
            _ => {}
        }
        i += 1;
    }
    FeedResult {
        line: cur,
        executed,
    }
}

/// Ctrl+W：删去最后一个词与其后的空白；只有空白时不变。
fn delete_word(line: &mut String) {
    let body = line.trim_end_matches(is_space);
    if body.is_empty() {
        return;
    }
    let keep = body.trim_end_matches(|c| !is_space(c)).len();
    line.truncate(keep);
}

/// 跳过从 `i` 开始的控制序列：CSI（`ESC [` 参数 结束字符）、SS3（`ESC O x`）、Alt 组合（`ESC x`），返回其后的位置。
fn skip_escape(chars: &[char], i: usize) -> usize {
    match chars.get(i + 1) {
        None => i + 1,
        Some('[') => {
            let mut j = i + 2;
            while j < chars.len() && !('\x40'..='\x7e').contains(&chars[j]) {
                j += 1;
            }
            j + 1
        }
        Some('O') => i + 3,
        Some(_) => i + 2,
    }
}

/// 粘贴后的待执行内容：`line` 是之前输入的内容，`text` 是粘贴的内容。
///
/// 有终端开启了括号粘贴（`bracketed`）时，粘贴的多行内容都留在它的输入行里，下次回车时一起执行，整段计入
/// （回车前检查的是全部待执行的内容）；都没有开启时其中的换行在粘贴时就已执行（粘贴前已检查），
/// 只留最后一个换行之后的部分。
pub fn feed_paste(line: &str, text: &str, bracketed: bool) -> String {
    let body = normalize_newlines(text);
    if bracketed {
        format!("{line}{body}")
    } else {
        feed_line(line, &body).line
    }
}

/// 粘贴时已检查过危险命令（并已确认）的内容：粘贴的整段与其中各行，
/// 与 [`find_danger`](super::find_danger) 检查的相同。
///
/// 有终端会立即执行时粘贴前就检查（其余部分留在输入行里），紧接着按回车执行这些内容时不再重复确认。
/// 只在 [`PasteCheck::danger_checked_for`](super::PasteCheck::danger_checked_for) 为真时记下，下次执行后作废。
pub fn checked_paste(text: &str) -> HashSet<String> {
    let whole = trim(&normalize_newlines(text)).to_string();
    std::iter::once(whole)
        .chain(paste_lines(text).into_iter().map(String::from))
        .filter(|s| !s.is_empty())
        .collect()
}

/// 回车时要检查的内容：推算出的待执行内容（`executed`）与各终端屏幕上的输入行（`screen`），
/// 去掉首尾空白、空的、重复的，以及粘贴时已检查过的（`checked`，见 [`checked_paste`]；按原文比较，
/// 粘贴后又编辑过的照常检查）。逐条用 [`find_danger`](super::find_danger) 检查。
pub fn run_commands(
    executed: &[String],
    screen: &[String],
    checked: Option<&HashSet<String>>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for command in executed.iter().chain(screen).map(|s| trim(s)) {
        let skip = command.is_empty()
            || out.iter().any(|x| x == command)
            || checked.is_some_and(|set| set.contains(command));
        if !skip {
            out.push(command.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::{Guard, find_danger};
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn feed_line_edits_and_executes() {
        // 回车时交给 shell 执行，退格、Ctrl+W、Ctrl+U 编辑，控制序列忽略
        let s = feed_line("", "rm -rf /tmpx");
        assert_eq!(
            s,
            FeedResult {
                line: "rm -rf /tmpx".into(),
                executed: vec![]
            }
        );
        let s = feed_line(&s.line, "\x7f\x7f");
        assert_eq!(s.line, "rm -rf /tm");
        let s = feed_line(&s.line, "\x17");
        assert_eq!(s.line, "rm -rf ");
        let s = feed_line(&s.line, "\x1b[D\x1bOA/");
        assert_eq!(s.line, "rm -rf /");
        assert_eq!(
            feed_line(&s.line, "\r"),
            FeedResult {
                line: String::new(),
                executed: strings(&["rm -rf /"])
            }
        );
        assert_eq!(feed_line("abc", "\x15"), FeedResult::default());
        assert_eq!(feed_line("abc", "\x03"), FeedResult::default());
        // 快捷命令：多行依次执行，最后不带换行的留在输入行
        assert_eq!(
            feed_line("sudo ", "ls\nuptime\ndf"),
            FeedResult {
                line: "df".into(),
                executed: strings(&["sudo ls", "uptime"])
            }
        );
        // 中文与表情按字符删除
        assert_eq!(feed_line("echo 你好😀", "\x7f").line, "echo 你好");
    }

    #[test]
    fn feed_line_edge_cases() {
        // Ctrl+W：只有空白时不变；词后的空白一起删去
        assert_eq!(feed_line("   ", "\x17").line, "   ");
        assert_eq!(feed_line("a b  ", "\x17").line, "a ");
        // Tab 等控制字符、不完整的控制序列、Alt 组合忽略
        assert_eq!(feed_line("ls", "\t\x1b").line, "ls");
        assert_eq!(feed_line("ls", "\x1b[1;5").line, "ls");
        assert_eq!(feed_line("ls", "\x1bb -la").line, "ls -la");
        assert_eq!(feed_line("", "\x1b你好").line, "好");
        // 空行同样计入（交给 shell 的是空命令）
        assert_eq!(feed_line("", "\r\r").executed, strings(&["", ""]));
    }

    #[test]
    fn feed_paste_keeps_pending() {
        // 括号粘贴：两行都留在输入行，回车时一起执行（检查的是整段，不只是最后一行）
        let pending = feed_paste("", "rm -rf /data\r\necho ok", true);
        assert_eq!(pending, "rm -rf /data\necho ok");
        assert_eq!(
            feed_line(&pending, "\r"),
            FeedResult {
                line: String::new(),
                executed: strings(&["rm -rf /data\necho ok"])
            }
        );
        assert_eq!(feed_paste("sudo ", "ls", true), "sudo ls");
        // 没有括号粘贴：换行在粘贴时已执行
        assert_eq!(feed_paste("sudo ", "ls\nuptime\ndf", false), "df");
    }

    #[test]
    fn checked_paste_is_not_checked_again() {
        // 没有括号粘贴的终端中前几行立即执行（粘贴前已检查），最后一行留到回车
        let text = "cd /data\r\nrm -rf /data";
        let checked = checked_paste(text);
        let pending = feed_paste("", text, false);
        let executed = feed_line(&pending, "\r").executed;
        assert_eq!(
            run_commands(&executed, &strings(&["rm -rf /data"]), Some(&checked)),
            Vec::<String>::new()
        );
        // 括号粘贴的终端中整段留到回车：整段与屏幕上的最后一行都已检查过
        let whole = feed_paste("", text, true);
        let executed = feed_line(&whole, "\r").executed;
        assert_eq!(
            run_commands(&executed, &strings(&["rm -rf /data"]), Some(&checked)),
            Vec::<String>::new()
        );
        // 粘贴后又输入了别的内容
        let edited = feed_line(&pending, " /home\r").executed;
        assert_eq!(
            run_commands(&edited, &strings(&["rm -rf /data /home"]), Some(&checked)),
            strings(&["rm -rf /data /home"])
        );
        // 粘贴之前已输入了一部分：整行没有检查过
        let executed = feed_line(&format!("sudo {pending}"), "\r").executed;
        assert_eq!(
            run_commands(&executed, &[], Some(&checked)),
            strings(&["sudo rm -rf /data"])
        );
        // 没有粘贴时：去掉空的与重复的
        assert_eq!(
            run_commands(&strings(&["ls ", ""]), &strings(&["ls", " df"]), None),
            strings(&["ls", "df"])
        );
    }

    #[test]
    fn checked_paste_contents() {
        let checked = checked_paste(" cd /data\r\n\nrm -rf /data \n");
        let mut items: Vec<_> = checked.into_iter().collect();
        items.sort();
        assert_eq!(
            items,
            ["cd /data", "cd /data\n\nrm -rf /data", "rm -rf /data"]
        );
        assert!(checked_paste(" \r\n").is_empty());
    }

    /// 按回车前的检查：推算出的待执行内容与屏幕上的输入行逐条检查。
    #[test]
    fn run_commands_with_rules() {
        let guard = Guard::default();
        let typed = feed_line("", "sudo rebo");
        let typed = feed_line(&typed.line, "ot\r");
        let commands = run_commands(&typed.executed, &strings(&["sudo reboot"]), None);
        assert_eq!(commands, strings(&["sudo reboot"]));
        let danger = commands.iter().find_map(|c| find_danger(&guard, c));
        assert_eq!(
            danger.map(|d| d.rule.id().to_string()).as_deref(),
            Some("power")
        );
    }
}
