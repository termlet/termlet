//! shell 集成：从终端输出中找出 shell 报告的标记——OSC 133 / 633（VS Code）的提示符与命令标记、
//! OSC 7 / 633;P / 1337 的当前目录、OSC 9 / 777 的通知。
//!
//! - [`OscScanner`] 在交给内核之前旁观原始字节，只记下每个标记在这段字节中的位置，不改动字节。
//!   shell 集成在内核之外（见 `engine::kernel`），这些 OSC 由这里自己解析（alacritty 也不处理它们）；
//!   序列可以在任意字节处被切开、分到两次输出中。
//! - 结束符与 vte 相同：BEL，或 ESC（ST 是 `ESC \`，vte 读到 ESC 就结束 OSC）；CAN、SUB 取消序列。
//!   载荷超过 [`MAX_PAYLOAD`] 的丢弃，不关心的 OSC（标题、超链接、剪贴板……）不缓存。
//! - 另外报告切换到备用屏幕的 CSI：没有 shell 集成时，按回车推断的命令要在切换之前读出；以及 kitty 键盘协议的
//!   请求（`CSI > u`、`CSI < u`、`CSI = u`、`CSI ? u`）与 RIS（`ESC c`），引擎转给内核（见 [`crate::kitty_keyboard`]）；
//!   深浅色切换通知的请求（`CSI ? 2031 h / l`、`CSI ? 996 n`、`CSI ? 2031 $ p`，见 [`crate::theme_mode`]）。
//! - 引擎按标记的位置把一批输出切开分段解析，在 B（输入开始）时记下光标所在的格子，在 C（开始执行）时
//!   从那里读出命令，见 `engine.rs`。

use std::fmt;
use std::time::Duration;

use crate::kitty_keyboard::KeyboardRequest;
use crate::theme_mode::ThemeRequest;

/// 一个 OSC 的载荷最多缓存的字节数：超过时丢弃整个序列（不截断）。命令文本（633;E）更长时退回从网格中读取。
pub const MAX_PAYLOAD: usize = 16 * 1024;

const BEL: u8 = 0x07;
const ESC: u8 = 0x1b;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;

/// 命令文本：可能含密码等秘密，`Debug` 只打印长度。
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CommandText(pub String);

impl CommandText {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CommandText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CommandText({} 个字符)", self.0.chars().count())
    }
}

/// shell 集成事件（引擎经 [`crate::EngineEvent::Shell`] 发出）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellEvent {
    /// 提示符开始（OSC 133 / 633 的 A）
    PromptStart,
    /// 命令开始执行（C）。`command` 是这条命令：633;E 给出的，或从网格中输入开始（B）处读到的；
    /// 读不到时（没有 B、B 所在的行已经滚出回滚、其间改变过终端大小）为 `None`
    CommandStart { command: Option<CommandText> },
    /// 命令结束（D）：之前有过 C 才报告；`duration` 是从 C 到 D 的时间
    CommandFinished {
        exit_code: Option<i32>,
        duration: Duration,
    },
    /// 用户执行了一条要记进命令历史的命令（已按 [`is_recordable`] 筛选）：
    /// - 有 shell 集成时在 C 到达时；shell 从不输出 C（只输出 A、B）时在下一个提示符开始时；
    /// - 没有 shell 集成时按回车推断：用户经 [`crate::TerminalView::write_input`]（键盘、粘贴、输入法）输入，
    ///   回车后等回显换行再读，提前输入、全屏程序中的、Ctrl+C 放弃的不记。
    ///
    /// 用户在这个终端里输入之前到达的不报告（例如登录后由应用自动执行的命令）。
    CommandEntered(CommandText),
    /// 程序请求的通知（OSC 9、OSC 777;notify）；OSC 9 没有标题，`title` 为空
    Notification { title: String, body: String },
    /// shell 报告的当前目录（OSC 7、633;P;Cwd=、1337;CurrentDir=）
    WorkingDirectory(String),
}

/// 扫描器在输出中找到的一个标记。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellMark {
    /// A：提示符开始
    PromptStart,
    /// B：提示符结束、输入开始
    InputStart,
    /// C：命令开始执行
    CommandExecuted,
    /// D\[;退出码\]：命令结束
    CommandFinished(Option<i32>),
    /// 633;E;命令行：VS Code 的 shell 集成直接给出的命令（已还原转义）
    CommandLine(CommandText),
    WorkingDirectory(String),
    Notification {
        title: String,
        body: String,
    },
    /// 进入备用屏幕（`CSI ? 1049 / 1047 / 47 h`，位置在结尾字符之前）：引擎在切换之前读出按回车推断的命令，
    /// 例如 `vim a.txt` 的回显与 vim 切换屏幕在同一段输出中
    EnterAlternateScreen,
    /// kitty 键盘协议的请求，或 RIS（[`KeyboardRequest::Reset`]），位置在序列之后
    KeyboardMode(KeyboardRequest),
    /// 深浅色切换通知的请求，位置在序列之后
    ThemeMode(ThemeRequest),
}

/// 命令历史记录的命令：多于一个字符，不以空白开头（与 bash 的 ignorespace 相同：以空格开头的命令不记）。
pub fn is_recordable(command: &str) -> bool {
    command.chars().nth(1).is_some() && !command.starts_with(char::is_whitespace)
}

impl ShellMark {
    /// 是不是提示符与命令的标记（133 / 633），即 shell 集成是否开启。
    pub fn is_prompt_mark(&self) -> bool {
        matches!(
            self,
            ShellMark::PromptStart
                | ShellMark::InputStart
                | ShellMark::CommandExecuted
                | ShellMark::CommandFinished(_)
                | ShellMark::CommandLine(_)
        )
    }
}

/// 找到的标记与它在这段字节中的位置：在这个位置切开时，前面的字节已经含有整个序列
/// （BEL 结束的切在 BEL 之后；ESC 结束的切在 ESC 之前，这时 vte 还在 OSC 中，之后的字节照常结束它）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundMark {
    pub offset: usize,
    pub mark: ShellMark,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    /// 刚读到 ESC
    Escape,
    /// 在关心的 OSC 中，记下载荷
    Osc,
    /// 在不关心的（或超长的）OSC 中，等它结束
    Skip,
    /// 在 CSI 中，记下参数（只关心切换备用屏幕的）
    Csi,
}

/// CSI 参数最多记下的字节数，更长的不是要找的序列。
const MAX_CSI_PARAMS: usize = 24;

/// OSC 扫描器：每个终端（传输）一个，跨输出保留状态。
#[derive(Debug)]
pub struct OscScanner {
    state: State,
    payload: Vec<u8>,
    /// 载荷中已经读到了 OSC 编号后的分号
    numbered: bool,
}

impl Default for OscScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl OscScanner {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            payload: Vec::new(),
            numbered: false,
        }
    }

    /// 扫描一段输出，把找到的标记追加到 `found`（位置相对这段字节）。
    pub fn scan(&mut self, bytes: &[u8], found: &mut Vec<FoundMark>) {
        let mut i = 0;
        while i < bytes.len() {
            match self.state {
                State::Ground => match bytes[i..].iter().position(|&b| b == ESC) {
                    Some(n) => {
                        i += n + 1;
                        self.state = State::Escape;
                    }
                    None => return,
                },
                State::Escape => {
                    let b = bytes[i];
                    i += 1;
                    self.state = match b {
                        b']' => {
                            self.payload.clear();
                            self.numbered = false;
                            State::Osc
                        }
                        b'[' => {
                            self.payload.clear();
                            State::Csi
                        }
                        b'c' => {
                            found.push(FoundMark {
                                offset: i,
                                mark: ShellMark::KeyboardMode(KeyboardRequest::Reset),
                            });
                            State::Ground
                        }
                        // 与 vte 相同：ESC 之后的 C0 控制字符照常执行，仍在 ESC 状态
                        ESC | 0x00..=0x17 | 0x19 | 0x1c..=0x1f => State::Escape,
                        _ => State::Ground,
                    };
                }
                State::Osc | State::Skip => {
                    let b = bytes[i];
                    match b {
                        BEL => {
                            self.finish(i + 1, found);
                            self.state = State::Ground;
                        }
                        ESC => {
                            self.finish(i, found);
                            self.state = State::Escape;
                        }
                        CAN | SUB => self.state = State::Ground,
                        // 与 vte 相同：OSC 中的其他控制字符被忽略
                        0x00..=0x06 | 0x08..=0x17 | 0x19 | 0x1c..=0x1f => {}
                        _ if self.state == State::Osc => self.push(b),
                        _ => {}
                    }
                    i += 1;
                }
                State::Csi => {
                    let b = bytes[i];
                    match b {
                        // 参数与中间字节
                        0x20..=0x3f => {
                            if self.payload.len() < MAX_CSI_PARAMS {
                                self.payload.push(b);
                            } else {
                                self.payload.clear();
                                self.payload.push(b'!');
                            }
                        }
                        0x40..=0x7e => {
                            if b == b'h' && enters_alternate_screen(&self.payload) {
                                found.push(FoundMark {
                                    offset: i,
                                    mark: ShellMark::EnterAlternateScreen,
                                });
                            }
                            if b == b'u'
                                && let Some(request) = KeyboardRequest::parse(&self.payload)
                            {
                                found.push(FoundMark {
                                    offset: i + 1,
                                    mark: ShellMark::KeyboardMode(request),
                                });
                            } else if let Some(request) = ThemeRequest::parse(&self.payload, b) {
                                found.push(FoundMark {
                                    offset: i + 1,
                                    mark: ShellMark::ThemeMode(request),
                                });
                            }
                            self.payload.clear();
                            self.state = State::Ground;
                        }
                        ESC => self.state = State::Escape,
                        CAN | SUB => self.state = State::Ground,
                        // 与 vte 相同：CSI 中的 C0 控制字符照常执行，DEL 忽略
                        0x00..=0x1f | 0x7f => {}
                        _ => self.state = State::Ground,
                    }
                    i += 1;
                }
            }
        }
    }

    fn push(&mut self, b: u8) {
        if self.payload.len() >= MAX_PAYLOAD {
            self.skip();
            return;
        }
        if !self.numbered {
            if b == b';' {
                if !matches!(
                    &self.payload[..],
                    b"7" | b"9" | b"133" | b"633" | b"777" | b"1337"
                ) {
                    self.skip();
                    return;
                }
                self.numbered = true;
            } else if !b.is_ascii_digit() || self.payload.len() >= 4 {
                self.skip();
                return;
            }
        }
        self.payload.push(b);
        // iTerm2 的 1337 还用来传图片（很长）：只关心 CurrentDir=
        const CURRENT_DIR: &[u8] = b"1337;CurrentDir=";
        let len = self.payload.len();
        if self.payload.starts_with(b"1337;")
            && len <= CURRENT_DIR.len()
            && self.payload[..] != CURRENT_DIR[..len]
        {
            self.skip();
        }
    }

    fn skip(&mut self) {
        self.state = State::Skip;
        self.payload.clear();
    }

    fn finish(&mut self, offset: usize, found: &mut Vec<FoundMark>) {
        if self.state == State::Osc
            && self.numbered
            && let Some(mark) = parse(&self.payload)
        {
            found.push(FoundMark { offset, mark });
        }
        self.payload.clear();
        self.numbered = false;
    }
}

/// `CSI ? 参数 h` 的参数中有没有切换到备用屏幕的（1049、1047、47）。
fn enters_alternate_screen(params: &[u8]) -> bool {
    params.strip_prefix(b"?").is_some_and(|rest| {
        rest.split(|&b| b == b';')
            .any(|p| matches!(p, b"1049" | b"1047" | b"47"))
    })
}

/// 解析一个 OSC 的载荷（`编号;内容`）。
fn parse(payload: &[u8]) -> Option<ShellMark> {
    let semi = payload.iter().position(|&b| b == b';')?;
    let (number, rest) = (&payload[..semi], &payload[semi + 1..]);
    match number {
        b"133" => prompt_mark(rest, false),
        b"633" => prompt_mark(rest, true),
        b"7" => cwd_from_file_url(&String::from_utf8_lossy(rest)).map(ShellMark::WorkingDirectory),
        b"9" => notification_9(&String::from_utf8_lossy(rest)),
        b"777" => notification_777(&String::from_utf8_lossy(rest)),
        b"1337" => {
            let text = String::from_utf8_lossy(rest);
            let path = text.strip_prefix("CurrentDir=")?;
            path.starts_with('/')
                .then(|| ShellMark::WorkingDirectory(path.to_string()))
        }
        _ => None,
    }
}

/// OSC 133 / 633 的标记：A、B、C、D\[;退出码\]；633 另有 E;命令行\[;nonce\] 与 P;Cwd=目录。
fn prompt_mark(rest: &[u8], vscode: bool) -> Option<ShellMark> {
    let mut fields = rest.split(|&b| b == b';');
    let kind = fields.next()?;
    Some(match kind {
        b"A" => ShellMark::PromptStart,
        b"B" => ShellMark::InputStart,
        b"C" => ShellMark::CommandExecuted,
        b"D" => ShellMark::CommandFinished(fields.next().and_then(exit_code)),
        // 命令行中的分号、控制字符写作 \xHH，所以按分号切开后第二段就是整个命令行
        b"E" if vscode => {
            ShellMark::CommandLine(CommandText(unescape_vscode(fields.next().unwrap_or(&[]))))
        }
        b"P" if vscode => {
            let property = unescape_vscode(fields.next()?);
            let cwd = property.strip_prefix("Cwd=")?;
            if cwd.is_empty() {
                return None;
            }
            ShellMark::WorkingDirectory(cwd.to_string())
        }
        _ => return None,
    })
}

/// 退出码：可带负号的十进制整数；其他写法（含空的）视为没有给出。
fn exit_code(field: &[u8]) -> Option<i32> {
    let text = std::str::from_utf8(field).ok()?;
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// 还原 VS Code 的转义：`\\` 是反斜杠，`\xHH` 是一个字节（分号、控制字符，以及部分脚本中的非 ASCII 字节）；
/// 其他反斜杠原样保留。还原后按 UTF-8 解码。
fn unescape_vscode(value: &[u8]) -> String {
    let mut out = Vec::with_capacity(value.len());
    let mut i = 0;
    while i < value.len() {
        if value[i] == b'\\' {
            if value.get(i + 1) == Some(&b'\\') {
                out.push(b'\\');
                i += 2;
                continue;
            }
            if value.get(i + 1) == Some(&b'x')
                && let (Some(high), Some(low)) = (
                    value.get(i + 2).and_then(|&b| hex(b)),
                    value.get(i + 3).and_then(|&b| hex(b)),
                )
            {
                out.push(high << 4 | low);
                i += 4;
                continue;
            }
        }
        out.push(value[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

/// OSC 7：`file://主机/路径`（按 URL 编码）。没有按 URL 编码（例如路径中有单独的 %）时按原样使用。
pub fn cwd_from_file_url(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    let path = &rest[rest.find('/')?..];
    Some(percent_decode(path).unwrap_or_else(|| path.to_string()))
}

/// 百分号解码；有不完整的 `%` 或解码后不是 UTF-8 时返回 `None`（与 decodeURIComponent 出错时相同）。
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let high = hex(*bytes.get(i + 1)?)?;
            let low = hex(*bytes.get(i + 2)?)?;
            out.push(high << 4 | low);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// OSC 9;内容（iTerm2 的通知）。数字开头的是 ConEmu 的子命令（9;4 进度、9;9 当前目录等），不是通知。
fn notification_9(text: &str) -> Option<ShellMark> {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    let conemu = digits > 0 && matches!(text.as_bytes().get(digits), None | Some(b';'));
    (!conemu).then(|| ShellMark::Notification {
        title: String::new(),
        body: text.to_string(),
    })
}

/// OSC 777;notify;标题;正文（rxvt-unicode、Windows Terminal、Ghostty）；正文中可以有分号。
fn notification_777(text: &str) -> Option<ShellMark> {
    let mut fields = text.splitn(3, ';');
    if fields.next()? != "notify" {
        return None;
    }
    let title = fields.next().unwrap_or_default().to_string();
    let body = fields.next().unwrap_or_default().to_string();
    Some(ShellMark::Notification { title, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_all(chunks: &[&[u8]]) -> Vec<(usize, usize, ShellMark)> {
        let mut scanner = OscScanner::new();
        let mut result = Vec::new();
        for (index, chunk) in chunks.iter().enumerate() {
            let mut found = Vec::new();
            scanner.scan(chunk, &mut found);
            result.extend(found.into_iter().map(|f| (index, f.offset, f.mark)));
        }
        result
    }

    fn marks(chunks: &[&[u8]]) -> Vec<ShellMark> {
        scan_all(chunks).into_iter().map(|(_, _, m)| m).collect()
    }

    #[test]
    fn prompt_marks_with_both_terminators() {
        let out = b"\x1b]133;A\x07$ \x1b]133;B\x1b\\ls\r\n\x1b]133;C\x07out\x1b]133;D;3\x1b\\";
        let found = scan_all(&[out]);
        let kinds: Vec<_> = found.iter().map(|(_, _, m)| m.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                ShellMark::PromptStart,
                ShellMark::InputStart,
                ShellMark::CommandExecuted,
                ShellMark::CommandFinished(Some(3)),
            ]
        );
        // BEL 结束的切在 BEL 之后，ESC 结束的切在 ESC 之前
        assert_eq!(found[0].1, 8);
        assert_eq!(&out[..found[0].1], b"\x1b]133;A\x07");
        assert_eq!(out[found[1].1], ESC);
        assert_eq!(&out[found[1].1..found[1].1 + 2], b"\x1b\\");
    }

    #[test]
    fn split_anywhere_gives_the_same_marks() {
        let out: &[u8] = b"a\x1b]133;A\x07$ \x1b]633;E;echo a\\x3bb\\\\c;nonce\x1b\\\x1b]7;file://h/tmp/a%20b\x07\x1b]133;D;-1\x07";
        let whole = marks(&[out]);
        assert_eq!(whole.len(), 4);
        for cut in 0..=out.len() {
            let (a, b) = out.split_at(cut);
            assert_eq!(marks(&[a, b]), whole, "在 {cut} 处切开");
        }
        // 每个字节一段
        let bytes: Vec<&[u8]> = out.chunks(1).collect();
        assert_eq!(marks(&bytes), whole);
    }

    #[test]
    fn exit_codes() {
        assert_eq!(
            marks(&[b"\x1b]133;D\x07\x1b]133;D;\x07\x1b]133;D;x1\x07\x1b]133;D;+1\x07\x1b]133;D;130\x07"]),
            vec![
                ShellMark::CommandFinished(None),
                ShellMark::CommandFinished(None),
                ShellMark::CommandFinished(None),
                ShellMark::CommandFinished(None),
                ShellMark::CommandFinished(Some(130)),
            ]
        );
    }

    #[test]
    fn vscode_escapes_and_properties() {
        let found = marks(&[
            b"\x1b]633;E;echo a\\x3bb \\\\n \\x0a \\xe4\\xb8\\xad\xe6\x96\x87 \\q;nonce\x07",
            b"\x1b]633;P;Cwd=/home/u\\x3bx\x07\x1b]633;P;IsWindows=False\x07\x1b]633;E;\x07",
        ]);
        assert_eq!(
            found,
            vec![
                ShellMark::CommandLine(CommandText("echo a;b \\n \n 中文 \\q".into())),
                ShellMark::WorkingDirectory("/home/u;x".into()),
                ShellMark::CommandLine(CommandText(String::new())),
            ]
        );
        // 133 没有 E
        assert!(marks(&[b"\x1b]133;E;ls\x07"]).is_empty());
    }

    #[test]
    fn working_directory_from_osc_7_and_1337() {
        assert_eq!(
            cwd_from_file_url("file://web01/home/ruan/my%20app").as_deref(),
            Some("/home/ruan/my app")
        );
        assert_eq!(cwd_from_file_url("file:///tmp").as_deref(), Some("/tmp"));
        assert_eq!(
            cwd_from_file_url("file://h/data/100%").as_deref(),
            Some("/data/100%")
        );
        assert_eq!(
            cwd_from_file_url("file://h/%E4%B8%AD").as_deref(),
            Some("/中")
        );
        // 解码后不是 UTF-8：按原样
        assert_eq!(cwd_from_file_url("file://h/%ff").as_deref(), Some("/%ff"));
        assert_eq!(cwd_from_file_url("http://h/x"), None);
        assert_eq!(cwd_from_file_url("file://host-only"), None);
        assert_eq!(
            marks(&[b"\x1b]1337;CurrentDir=/var/log\x07\x1b]1337;SetMark\x07\x1b]1337;CurrentDir=rel\x07"]),
            vec![ShellMark::WorkingDirectory("/var/log".into())]
        );
    }

    #[test]
    fn notifications() {
        let found = marks(&[
            "\x1b]9;构建完成\x07\x1b]9;4;1;50\x07\x1b]9;9;\"C:\\\\x\"\x07\x1b]9;10\x07".as_bytes(),
            "\x1b]9;2026 年\x07\x1b]777;notify;部署;已完成;共 3 台\x07\x1b]777;other;x\x07"
                .as_bytes(),
        ]);
        assert_eq!(
            found,
            vec![
                ShellMark::Notification {
                    title: String::new(),
                    body: "构建完成".into()
                },
                ShellMark::Notification {
                    title: String::new(),
                    body: "2026 年".into()
                },
                ShellMark::Notification {
                    title: "部署".into(),
                    body: "已完成;共 3 台".into()
                },
            ]
        );
    }

    #[test]
    fn oversized_payload_is_dropped_and_scanner_recovers() {
        let mut out = b"\x1b]9;".to_vec();
        out.extend(std::iter::repeat_n(b'x', MAX_PAYLOAD + 10));
        out.extend_from_slice(b"\x07\x1b]133;A\x07");
        let mut scanner = OscScanner::new();
        let mut found = Vec::new();
        for chunk in out.chunks(1000) {
            scanner.scan(chunk, &mut found);
        }
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].mark, ShellMark::PromptStart);
        assert!(
            scanner.payload.capacity() <= MAX_PAYLOAD + 1024,
            "不应无限缓存"
        );
    }

    #[test]
    fn other_sequences_are_ignored() {
        // 标题、超链接、剪贴板等不缓存也不报告；CAN 取消序列；ESC 后的控制字符不打断 ESC
        let found = marks(&[
            b"\x1b]0;title\x07\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\\x1b]52;c;aGk=\x07",
            b"\x1b]133;A\x18\x1b[31mred\x1b\x07]133;B\x07\x1b]1330;A\x07\x1b]133\x07",
        ]);
        assert_eq!(found, vec![ShellMark::InputStart]);
        // ESC 直接结束 OSC，紧接着的 ESC ] 是新的序列
        assert_eq!(
            marks(&[b"\x1b]0;t\x1b]133;C\x07"]),
            vec![ShellMark::CommandExecuted]
        );
    }

    #[test]
    fn alternate_screen_switch_is_found_before_its_final_byte() {
        let out = b"vim a\r\n\x1b[31m\x1b[?1049h\x1b[?25l\x1b[?1;1049h\x1b[?1049l";
        let found = scan_all(&[out]);
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|f| f.2 == ShellMark::EnterAlternateScreen));
        assert_eq!(out[found[0].1], b'h');
        assert_eq!(&out[..found[0].1], b"vim a\r\n\x1b[31m\x1b[?1049");
        // 切开也能找到
        for cut in 0..=out.len() {
            let (a, b) = out.split_at(cut);
            assert_eq!(marks(&[a, b]).len(), 2, "在 {cut} 处切开");
        }
    }

    #[test]
    fn keyboard_requests_are_found_after_the_sequence() {
        use crate::kitty_keyboard::{KeyboardFlags, SetMode};
        let out = b"a\x1b[>1u\x1b[?u\x1b[=5;2u\x1b[<u\x1b[u\x1b[>1;2m\x1bcz";
        let found = scan_all(&[out]);
        let flags = KeyboardFlags::from_bits;
        assert_eq!(
            found.iter().map(|f| f.2.clone()).collect::<Vec<_>>(),
            vec![
                ShellMark::KeyboardMode(KeyboardRequest::Push(flags(1))),
                ShellMark::KeyboardMode(KeyboardRequest::Query),
                ShellMark::KeyboardMode(KeyboardRequest::Set(flags(5), SetMode::Union)),
                ShellMark::KeyboardMode(KeyboardRequest::Pop(1)),
                ShellMark::KeyboardMode(KeyboardRequest::Reset),
            ]
        );
        // 位置在序列之后：前面的字节已含有整个序列
        assert_eq!(&out[..found[0].1], b"a\x1b[>1u");
        assert_eq!(&out[found[4].1..], b"z");
        // CSI 中的控制字符照常执行（与 vte 相同）；中间字节、超长的参数不是请求；切开也能找到
        assert_eq!(marks(&[b"\x1b[>\n1u"]).len(), 1);
        assert!(marks(&[b"\x1b[>1$u\x1b[?1049$u"]).is_empty());
        let long = format!("\x1b[>{}u", "1".repeat(40));
        assert!(marks(&[long.as_bytes()]).is_empty());
        for cut in 0..=out.len() {
            let (a, b) = out.split_at(cut);
            assert_eq!(marks(&[a, b]).len(), 5, "在 {cut} 处切开");
        }
    }

    // 深浅色切换通知的请求：开启、关闭、查询深浅、DECRQM；与切换屏幕写在一起时两个都报告（切换屏幕在结尾字符之前）
    #[test]
    fn theme_requests_are_found_after_the_sequence() {
        let out = b"a\x1b[?2031h\x1b[?996n\x1b[?2031$p\x1b[?2031l\x1b[?6n\x1b[?2004hz";
        let found = scan_all(&[out]);
        assert_eq!(
            found.iter().map(|f| f.2.clone()).collect::<Vec<_>>(),
            vec![
                ShellMark::ThemeMode(ThemeRequest::Enable),
                ShellMark::ThemeMode(ThemeRequest::Query),
                ShellMark::ThemeMode(ThemeRequest::ModeQuery),
                ShellMark::ThemeMode(ThemeRequest::Disable),
            ]
        );
        assert_eq!(&out[..found[0].1], b"a\x1b[?2031h");
        for cut in 0..=out.len() {
            let (a, b) = out.split_at(cut);
            assert_eq!(marks(&[a, b]).len(), 4, "在 {cut} 处切开");
        }
        assert_eq!(
            marks(&[b"\x1b[?1049;2031h"]),
            vec![
                ShellMark::EnterAlternateScreen,
                ShellMark::ThemeMode(ThemeRequest::Enable)
            ]
        );
    }

    #[test]
    fn recordable_commands() {
        assert!(is_recordable("ls"));
        assert!(is_recordable("中文"));
        assert!(!is_recordable("l"));
        assert!(!is_recordable(""));
        assert!(!is_recordable(" mysql -p"));
        assert!(!is_recordable("\tls"));
    }

    #[test]
    fn command_text_debug_hides_content() {
        let text = CommandText("mysql -psecret".into());
        let debug = format!(
            "{:?}",
            ShellEvent::CommandStart {
                command: Some(text)
            }
        );
        assert!(!debug.contains("secret"), "{debug}");
    }
}
