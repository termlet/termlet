//! sudo / doas 的密码提示：终端停在登录用户的 sudo 密码提示上时，界面提示"填入已保存的密码"，用户确认后
//! 写入保存的密码与回车。
//!
//! 只认带用户名的提示（`[sudo] … 用户名 …:`、`doas (用户名@主机) password:`），且用户名须与登录用户相同：
//! su 问的是目标用户的密码，嵌套的 ssh 问的 `user@host's password:` 是另一台主机的，都不能填。
//! 只看输出的最后一行（去掉控制序列）：提示之后又有了别的输出，就不再停在提示上。
//!
//! 用法（界面）：SSH 主机保存了密码、设置 `terminal.password_fill` 开启时，为终端建一个 [`PasswordWatch`]
//! （登录用户名），放在锁里由输出与输入两边共用：
//! - 每段输出交给 [`PasswordWatch::feed`]：返回 [`PromptEvent::Shown`] 时显示提示条，
//!   [`PromptEvent::Ended`] 时收起（sudo 等待超时回到 shell 等，否则之后按的回车会被当作"填入密码"）；
//! - 用户自己输入时调用 [`PasswordWatch::typed`]：不再填（界面自己收起提示条，不另发 `Ended`）；
//! - 用户确认填入时先 [`PasswordWatch::take`]（同一个提示只填一次），得到 `Some` 才写入 [`fill_input`]，
//!   这次写入不算用户的输入；没能写入时用 [`PasswordWatch::restore`] 恢复。

use std::sync::LazyLock;

use regex::Regex;
use zeroize::Zeroizing;

use crate::secret::SecretString;

/// 记住的最近输出超过这么长时只留最后 [`TAIL_KEEP`] 字节（只需要最后一行）。
const TAIL_MAX: usize = 2048;
const TAIL_KEEP: usize = 1024;

/// 控制序列：CSI、OSC（以 BEL 或 ST 结束）、其他 `ESC` 加一个字节的序列。
static ANSI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\x5c-_]")
        .unwrap()
});

/// 检测到的变化。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptEvent {
    /// 终端停在了登录用户的密码提示上：显示"填入已保存的密码"
    Shown,
    /// 提示已经过去（有了别的输出），而且不是因为填入或用户输入：收起提示条
    Ended,
}

/// 一个终端的密码提示检测。不是线程安全的：输出与输入在不同线程时由调用方加锁。
#[derive(Debug)]
pub struct PasswordWatch {
    user: String,
    /// 最近的输出
    tail: Vec<u8>,
    /// 正停在密码提示上、还没有填
    pending: bool,
    /// 每次有新的输出、用户输入时递增：没能写入、要恢复等待时据此确认期间状态没有变化
    seq: u64,
}

/// [`PasswordWatch::take`] 取走的等待标记，没能写入时交回 [`PasswordWatch::restore`]。
#[derive(Debug)]
#[must_use]
pub struct FillTicket {
    seq: u64,
}

impl PasswordWatch {
    /// `user` 是这个终端的登录用户；为空时什么也不检测。
    pub fn new(user: impl Into<String>) -> Self {
        Self {
            user: user.into(),
            tail: Vec::new(),
            pending: false,
            seq: 0,
        }
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    /// 处理一段输出（UTF-8，可以在任意位置切开），返回提示的变化。
    pub fn feed(&mut self, output: &[u8]) -> Option<PromptEvent> {
        if self.user.is_empty() || output.is_empty() {
            return None;
        }
        self.seq += 1;
        let start = output.len().saturating_sub(TAIL_MAX);
        self.tail.extend_from_slice(&output[start..]);
        if self.tail.len() > TAIL_MAX {
            self.tail.drain(..self.tail.len() - TAIL_KEEP);
        }
        let matched = is_password_prompt(&last_line(&self.tail), &self.user);
        let event = match (matched, self.pending) {
            (true, false) => Some(PromptEvent::Shown),
            (false, true) => Some(PromptEvent::Ended),
            _ => None,
        };
        self.pending = matched;
        event
    }

    /// 用户自己开始输入：不再填。
    pub fn typed(&mut self) {
        self.seq += 1;
        self.pending = false;
        self.tail.clear();
    }

    /// 是否正停在密码提示上、还没有填。
    pub fn is_pending(&self) -> bool {
        self.pending
    }

    /// 要填入密码时取走等待标记：同一个提示只填一次（两处同时确认时只有一处得到 `Some`）。
    /// 没有停在提示上时返回 `None`，不要写入。
    pub fn take(&mut self) -> Option<FillTicket> {
        if !self.pending {
            return None;
        }
        self.pending = false;
        Some(FillTicket { seq: self.seq })
    }

    /// 没能写入（终端正在重连、已关闭）：期间没有新的输出、用户也没有输入时恢复等待，可以再试。
    pub fn restore(&mut self, ticket: FillTicket) {
        if self.seq == ticket.seq {
            self.pending = true;
        }
    }
}

/// 填入时写进终端的内容：密码加回车。
pub fn fill_input(password: &SecretString) -> Zeroizing<Vec<u8>> {
    let mut input = Zeroizing::new(Vec::with_capacity(password.expose().len() + 1));
    input.extend_from_slice(password.expose().as_bytes());
    input.push(b'\r');
    input
}

/// 去掉控制序列后的最后一行（去掉首尾空白）。
fn last_line(output: &[u8]) -> String {
    let text = String::from_utf8_lossy(output);
    let text = ANSI.replace_all(&text, "");
    let line = match text.rfind(['\r', '\n']) {
        Some(i) => &text[i + 1..],
        None => &text,
    };
    line.trim().to_string()
}

/// 一行是不是 `user` 的 sudo / doas 密码提示：各种语言的 sudo 提示都以 `[sudo]` 开头、以冒号结尾，
/// 其中有用户名这个词。
pub fn is_password_prompt(line: &str, user: &str) -> bool {
    if !line.ends_with(':') && !line.ends_with('：') {
        return false;
    }
    let sudo = line.starts_with("[sudo]");
    let doas = line.starts_with("doas (") && line.ends_with("password:");
    if !sudo && !doas {
        return false;
    }
    line.split([' ', '\t', '(', ')', '@', ':', '：'])
        .any(|word| word == user)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_prompts_of_the_login_user() {
        for (line, want) in [
            ("[sudo] password for alice:", true),
            ("[sudo] alice 的密码：", true),
            ("[sudo] Passwort für alice:", true),
            ("[sudo] alice のパスワード:", true),
            ("doas (alice@web-1) password:", true),
            ("[sudo] password for bob:", false), // 别的用户
            ("alice@web-1's password:", false),  // 嵌套 ssh：另一台主机
            ("Password:", false),                // su：目标用户的密码
            ("[sudo] password for alice: wrong", false), // 不在提示符处
            ("echo [sudo] password for alice", false),
            ("[sudo] password for alice2:", false),
            ("[sudo] password for alice (abc) :", true),
        ] {
            assert_eq!(is_password_prompt(line, "alice"), want, "{line:?}");
        }
    }

    // 出现登录用户的 sudo 提示时通知；确认后只能填一次；用户自己输入后不再填。
    #[test]
    fn fill_once_and_not_after_typing() {
        let mut watch = PasswordWatch::new("alice");
        assert!(watch.take().is_none(), "没有提示时不应填入");
        // 提示被拆在两段输出中，带颜色
        assert_eq!(watch.feed(b"$ sudo ls\r\n\x1b[1m[sudo] password "), None);
        assert_eq!(watch.feed(b"for alice:\x1b[0m "), Some(PromptEvent::Shown));
        assert!(watch.is_pending());
        assert!(watch.take().is_some());
        assert!(watch.take().is_none(), "同一个提示只能填一次");

        // 用户自己开始输入：不再填
        assert_eq!(
            watch.feed(b"\r\n[sudo] password for alice: "),
            Some(PromptEvent::Shown)
        );
        watch.typed();
        assert!(watch.take().is_none(), "用户输入后不应再填");
    }

    // 提示过去了（sudo 等待超时回到 shell）时通知收起；
    // 由填入密码、用户输入结束的不通知；之后的输出不重复通知。
    #[test]
    fn prompt_end_is_reported_once() {
        let mut watch = PasswordWatch::new("alice");
        let mut events = Vec::new();
        let mut output = |watch: &mut PasswordWatch, s: &str| {
            if let Some(event) = watch.feed(s.as_bytes()) {
                events.push(event);
            }
        };

        output(&mut watch, "[sudo] password for alice: ");
        output(&mut watch, "\r\nsudo: timed out reading password\r\n$ ");
        assert!(watch.take().is_none(), "提示过去后不应填入");
        output(&mut watch, "ls\r\n");

        // 填入密码、用户自己输入后的输出：不通知结束
        output(&mut watch, "[sudo] password for alice: ");
        assert!(watch.take().is_some());
        output(&mut watch, "\r\n$ sudo ls\r\n");
        output(&mut watch, "[sudo] password for alice: ");
        watch.typed();
        output(&mut watch, "\r\n$ ");

        use PromptEvent::*;
        assert_eq!(events, vec![Shown, Ended, Shown, Shown]);
    }

    #[test]
    fn prompt_split_inside_characters() {
        let prompt = "\x1b[0m[sudo] alice 的密码：".as_bytes();
        let mut watch = PasswordWatch::new("alice");
        let events: Vec<_> = prompt.chunks(1).filter_map(|b| watch.feed(b)).collect();
        assert_eq!(events, vec![PromptEvent::Shown]);
    }

    #[test]
    fn restore_only_when_nothing_changed() {
        let mut watch = PasswordWatch::new("alice");
        watch.feed(b"[sudo] password for alice: ");
        let ticket = watch.take().unwrap();
        watch.restore(ticket);
        assert!(watch.is_pending(), "期间没有变化：恢复等待");

        let ticket = watch.take().unwrap();
        assert_eq!(watch.feed(b"\r\n"), None, "已经取走的不再通知结束");
        watch.restore(ticket);
        assert!(!watch.is_pending(), "期间有了新的输出：不恢复");

        watch.feed(b"\r\n[sudo] password for alice: ");
        let ticket = watch.take().unwrap();
        watch.typed();
        watch.restore(ticket);
        assert!(!watch.is_pending(), "期间用户输入了：不恢复");
    }

    #[test]
    fn long_output_keeps_only_the_tail() {
        let mut watch = PasswordWatch::new("alice");
        watch.feed(&vec![b'x'; 10_000]);
        assert!(watch.tail.len() <= TAIL_MAX);
        let mut chunk = vec![b'y'; 5000];
        chunk.extend_from_slice(b"\r\n[sudo] password for alice: ");
        assert_eq!(watch.feed(&chunk), Some(PromptEvent::Shown));
        for _ in 0..10 {
            watch.feed(&[b'z'; 700]);
        }
        assert!(watch.tail.len() <= TAIL_MAX);
        assert!(!watch.is_pending());
    }

    #[test]
    fn empty_user_detects_nothing() {
        let mut watch = PasswordWatch::new("");
        assert_eq!(watch.feed(b"[sudo] password for :"), None);
        assert!(watch.take().is_none());
    }

    #[test]
    fn last_line_strips_control_sequences() {
        assert_eq!(
            last_line(b"a\r\n\x1b[1;31m[sudo]\x1b[0m \x1b]0;t\x1b\\x:\x1bM "),
            "[sudo] x:"
        );
        assert_eq!(last_line(b"one\rtwo"), "two");
    }

    #[test]
    fn fill_input_ends_with_enter() {
        let input = fill_input(&SecretString::new("p@ss 中"));
        assert_eq!(&input[..], "p@ss 中\r".as_bytes());
    }
}
