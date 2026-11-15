//! 自动登录：在输出中看到登录、密码提示时各填写一次（与 SecureCRT、Xshell 的 Telnet 自动登录相同）；
//! 密码错误后再次出现的提示不再填写，交给用户。

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::bytes::Regex;

use crate::secret::SecretString;

/// 自动登录等待提示的最长时间。
pub(super) const LOGIN_WINDOW: Duration = Duration::from_secs(60);

/// 检查提示时看的输出长度（最近的这么多字节）。
const TAIL: usize = 256;

/// 登录提示：Linux 的 "host login:"，网络设备的 "Username:"、"User name:"。
static USER_PROMPT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:login|user ?name|用户名)\s*[:：]\s*$").unwrap());
/// 密码提示："Password:"、"Passcode:"、"密码："。
static PASS_PROMPT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:password|passcode|密码)\s*[:：]\s*$").unwrap());
/// 颜色等控制序列：匹配提示前去掉。
static CSI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap());

/// 要填写的内容（不含回车）。
pub(super) enum Reply {
    Username(String),
    Password(SecretString),
}

impl Reply {
    pub(super) fn text(&self) -> &str {
        match self {
            Reply::Username(user) => user,
            Reply::Password(password) => password.expose(),
        }
    }
}

/// 自动登录的进度。
pub(super) struct AutoLogin {
    username: String,
    password: Option<SecretString>,
    /// 已填过用户名（或不需要填），只等密码提示
    user_done: bool,
    tail: Vec<u8>,
    deadline: Instant,
}

impl AutoLogin {
    /// 用户名、密码都为空时不自动登录（`None`）。
    pub(super) fn new(
        username: &str,
        password: Option<&SecretString>,
        now: Instant,
    ) -> Option<Self> {
        let password = password.filter(|p| !p.is_empty()).cloned();
        if username.is_empty() && password.is_none() {
            return None;
        }
        Some(Self {
            username: username.to_string(),
            user_done: username.is_empty(),
            password,
            tail: Vec::new(),
            deadline: now + LOGIN_WINDOW,
        })
    }

    /// 检查新的输出（已解码为 UTF-8），返回要填写的内容；`done` 表示自动登录已结束。
    pub(super) fn feed(&mut self, out: &[u8], now: Instant) -> (Option<Reply>, bool) {
        if now > self.deadline {
            return (None, true);
        }
        self.tail.extend_from_slice(out);
        if self.tail.len() > TAIL {
            self.tail.drain(..self.tail.len() - TAIL);
        }
        let text = CSI.replace_all(&self.tail, &b""[..]);
        // 没有要求输入用户名、直接询问密码的设备（只设置了线路密码）
        if let Some(password) = &self.password
            && PASS_PROMPT.is_match(&text)
        {
            return (Some(Reply::Password(password.clone())), true);
        }
        if !self.user_done && USER_PROMPT.is_match(&text) {
            self.user_done = true;
            self.tail.clear();
            let done = self.password.is_none();
            return (Some(Reply::Username(self.username.clone())), done);
        }
        (None, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(reply: &Option<Reply>) -> Option<&str> {
        reply.as_ref().map(|r| r.text())
    }

    #[test]
    fn prompts() {
        let now = Instant::now();
        let password = SecretString::new("pw");
        let mut login = AutoLogin::new("admin", Some(&password), now).unwrap();
        let (reply, done) = login.feed(b"\r\nLogin authentication\r\n\r\nUser name: ", now);
        assert_eq!((text(&reply), done), (Some("admin"), false));
        // 用户名已填过：再出现登录提示不再填写
        let (reply, _) = login.feed(b"\r\nlogin: ", now);
        assert!(reply.is_none());
        let (reply, done) = login.feed(b"\x1b[1m\xe5\xaf\x86\xe7\xa0\x81\xef\xbc\x9a\x1b[0m ", now);
        assert_eq!(
            (text(&reply), done),
            (Some("pw"), true),
            "中文的“密码：”与颜色"
        );

        // 只有线路密码的设备：直接问密码
        let mut login = AutoLogin::new("", Some(&password), now).unwrap();
        let (reply, done) = login.feed(b"\r\nPasscode:", now);
        assert_eq!((text(&reply), done), (Some("pw"), true));

        // 只有用户名
        let mut login = AutoLogin::new("root", None, now).unwrap();
        let (reply, done) = login.feed(b"host login:", now);
        assert_eq!((text(&reply), done), (Some("root"), true));

        assert!(AutoLogin::new("", None, now).is_none());
        assert!(AutoLogin::new("", Some(&SecretString::new("")), now).is_none());
    }

    #[test]
    fn gives_up_after_the_window() {
        let now = Instant::now();
        let mut login = AutoLogin::new("admin", None, now).unwrap();
        let (reply, done) = login.feed(b"login:", now + LOGIN_WINDOW + Duration::from_secs(1));
        assert!(reply.is_none() && done);
    }

    #[test]
    fn prompt_must_be_at_the_end() {
        let now = Instant::now();
        let mut login = AutoLogin::new("admin", None, now).unwrap();
        let (reply, done) = login.feed(b"Last login: Mon Oct 19 on pts/0\r\n$ ", now);
        assert!(reply.is_none() && !done);
        // 长输出只看最后 256 字节
        let mut long = vec![b'x'; 4096];
        long.extend(b"Username:");
        let (reply, _) = login.feed(&long, now);
        assert_eq!(text(&reply), Some("admin"));
    }
}
