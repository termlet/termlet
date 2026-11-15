//! 环境变量列表与变量展开。
//!
//! 环境变量中常有密码、令牌（用户自己设置的），这里的 `Debug` 只输出变量名。

use std::fmt;

/// 环境变量名是否相同：Windows 不区分大小写，其余系统区分。
pub(crate) fn key_eq(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        eq_ignore_case(a, b)
    } else {
        a == b
    }
}

/// 不区分大小写地比较（Windows 的环境变量名）。
pub(crate) fn eq_ignore_case(a: &str, b: &str) -> bool {
    if a.is_ascii() && b.is_ascii() {
        a.eq_ignore_ascii_case(b)
    } else {
        a.to_uppercase() == b.to_uppercase()
    }
}

/// 环境变量列表：保持加入的顺序，名称按平台的规则比较（见 [`key_eq`]）。
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct Env(Vec<(String, String)>);

impl Env {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 取变量的值；有多个同名的（不应出现）时取最后一个。
    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|(k, _)| key_eq(k, key))
            .map(|(_, v)| v.as_str())
    }

    /// 取变量的值，没有时为空字符串。
    pub(crate) fn var(&self, key: &str) -> &str {
        self.get(key).unwrap_or("")
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, key: &str) -> bool {
        self.0.iter().any(|(k, _)| key_eq(k, key))
    }

    /// 设置变量：已有同名的就地替换值（保留原来的名称写法与位置），否则加在最后。
    #[cfg(any(windows, test))]
    pub(crate) fn set(&mut self, key: &str, value: impl Into<String>) {
        let value = value.into();
        match self.0.iter().position(|(k, _)| key_eq(k, key)) {
            Some(i) => {
                self.0[i].1 = value;
                // 去掉之后的同名项，保证每个名称只出现一次
                let mut index = 0;
                self.0.retain(|(k, _)| {
                    index += 1;
                    index - 1 <= i || !key_eq(k, key)
                });
            }
            None => self.0.push((key.to_string(), value)),
        }
    }

    #[cfg(any(windows, test))]
    pub(crate) fn remove(&mut self, key: &str) {
        self.0.retain(|(k, _)| !key_eq(k, key));
    }

    /// 加在最后，不检查同名（调用方已保证不重复）。
    pub(crate) fn push(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.0.push((key.into(), value.into()));
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub(crate) fn into_vec(self) -> Vec<(String, String)> {
        self.0
    }
}

impl<K: Into<String>, V: Into<String>> FromIterator<(K, V)> for Env {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Self(
            iter.into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )
    }
}

impl fmt::Debug for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(k, _)| k))
            .finish()
    }
}

/// 按当前平台的写法展开环境变量：Windows 为 `%NAME%`，其余为 `$NAME`、`${NAME}`。
#[cfg(windows)]
pub(crate) fn expand(s: &str, env: &Env) -> String {
    expand_windows(s, env)
}

/// 按当前平台的写法展开环境变量：Windows 为 `%NAME%`，其余为 `$NAME`、`${NAME}`。
#[cfg(not(windows))]
pub(crate) fn expand(s: &str, env: &Env) -> String {
    expand_unix(s, env)
}

/// 展开 `%NAME%` 形式的环境变量（名称不区分大小写）；没有定义的保持原样（与 cmd 相同）。
#[cfg(any(windows, test))]
pub(crate) fn expand_windows(s: &str, env: &Env) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        let Some(len) = rest[start + 1..].find('%') else {
            break;
        };
        let name = &rest[start + 1..start + 1 + len];
        let value = env
            .0
            .iter()
            .rev()
            .find(|(k, _)| eq_ignore_case(k, name))
            .map(|(_, v)| v.as_str());
        match value {
            Some(value) if !name.is_empty() => {
                out.push_str(&rest[..start]);
                out.push_str(value);
                rest = &rest[start + len + 2..];
            }
            _ => {
                // 不是变量：保留第一个 %，从它后面继续找
                out.push_str(&rest[..=start]);
                rest = &rest[start + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// 展开 `$NAME`、`${NAME}` 形式的环境变量（与 shell 的写法相同，没有定义的展开为空）：
/// `$` 后不是名称时原样保留，`${}`、未闭合的 `${` 去掉。
#[cfg(any(not(windows), test))]
pub(crate) fn expand_unix(s: &str, env: &Env) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    // 尚未复制到 out 的部分从 copied 开始
    let mut copied = 0;
    let mut j = 0;
    while j < bytes.len() {
        if bytes[j] == b'$' && j + 1 < bytes.len() {
            out.push_str(&s[copied..j]);
            let (name, width) = shell_name(&s[j + 1..]);
            match name {
                Some(name) => out.push_str(env.var(name)),
                // 语法不对：去掉这几个字符
                None if width > 0 => {}
                // `$` 后面不是名称：原样保留
                None => out.push('$'),
            }
            j += width;
            copied = j + 1;
        }
        j += 1;
    }
    out.push_str(&s[copied.min(s.len())..]);
    out
}

/// `$` 之后的变量名与它占的字节数；名称为 `None`、宽度不为 0 表示语法不对。
#[cfg(any(not(windows), test))]
fn shell_name(s: &str) -> (Option<&str>, usize) {
    let bytes = s.as_bytes();
    let special = |c: u8| b"*#$@!?-0123456789".contains(&c);
    if bytes[0] == b'{' {
        if bytes.len() > 2 && special(bytes[1]) && bytes[2] == b'}' {
            return (Some(&s[1..2]), 3);
        }
        return match bytes.iter().skip(1).position(|&c| c == b'}') {
            Some(0) => (None, 2),
            Some(i) => (Some(&s[1..i + 1]), i + 2),
            None => (None, 1),
        };
    }
    if special(bytes[0]) {
        return (Some(&s[..1]), 1);
    }
    let len = bytes
        .iter()
        .take_while(|&&c| c == b'_' || c.is_ascii_alphanumeric())
        .count();
    if len == 0 {
        (None, 0)
    } else {
        (Some(&s[..len]), len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_replaces_in_place_and_debug_hides_values() {
        let mut env: Env = [("PATH", "/bin"), ("SECRET_TOKEN", "hunter2")]
            .into_iter()
            .collect();
        env.set("PATH", "/usr/bin");
        env.set("NEW", "1");
        assert_eq!(env.get("PATH"), Some("/usr/bin"));
        assert_eq!(env.iter().next(), Some(("PATH", "/usr/bin")));
        assert_eq!(env.var("MISSING"), "");
        let debug = format!("{env:?}");
        assert!(
            debug.contains("SECRET_TOKEN") && !debug.contains("hunter2"),
            "{debug}"
        );
        env.remove("NEW");
        assert!(!env.contains("NEW"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_keys_are_case_insensitive() {
        let mut env: Env = [("Path", r"C:\a"), ("PATH", r"C:\dup")]
            .into_iter()
            .collect();
        env.set("path", r"C:\b");
        assert_eq!(env.iter().collect::<Vec<_>>(), [("Path", r"C:\b")]);
    }

    #[test]
    fn expand_windows_cases() {
        let env: Env = [
            ("USERPROFILE", r"C:\Users\ruan"),
            ("Name", "x"),
            ("EMPTY", ""),
        ]
        .into_iter()
        .collect();
        let cases = [
            (r"%USERPROFILE%\work", r"C:\Users\ruan\work"),
            ("%userprofile%", r"C:\Users\ruan"),
            ("100% %NOPE% %name%", "100% %NOPE% x"),
            ("a%", "a%"),
            ("%%", "%%"),
            ("[%EMPTY%]", "[]"),
            ("%中文%路径", "%中文%路径"),
        ];
        for (input, want) in cases {
            assert_eq!(
                expand_windows(input, &env),
                want,
                "expand_windows({input:?})"
            );
        }
    }

    #[test]
    fn expand_unix_cases() {
        let env: Env = [("HOME", "/home/ruan"), ("X", "1"), ("1", "one")]
            .into_iter()
            .collect();
        let cases = [
            ("$HOME/work/${X}", "/home/ruan/work/1"),
            ("$NOPE-x", "-x"),
            ("cost $ 5", "cost $ 5"),
            ("end$", "end$"),
            ("${}x", "x"),
            ("${unclosed", "unclosed"),
            ("$1${1}", "oneone"),
            ("中$X文", "中1文"),
        ];
        for (input, want) in cases {
            assert_eq!(expand_unix(input, &env), want, "expand_unix({input:?})");
        }
    }
}
