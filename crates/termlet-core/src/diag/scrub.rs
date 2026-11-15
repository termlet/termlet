//! 去掉诊断信息、日志与崩溃记录中的个人信息与秘密（崩溃报告用的是同一套规则）。
//!
//! - 用户目录换成 `~`，系统用户名换成 `<user>`（太短的用户名会误伤正常内容，不处理：ASCII 少于 3 个字符、
//!   非 ASCII 少于 2 个字符）；
//! - 网址（`file://` 除外）换成 `<url>`，`user@host` 换成 `<user>@<host>`（Go 模块路径中的 `@v1.2.3` 是版本号，保留）；
//! - IP 地址换成 `<ip>`（`127.x` 与 `0.0.0.0` 除外），IPv6 含 `::` 压缩写法、zone 与方括号写法；
//! - 三级及以上的域名换成 `<host>`（只有一级的主机名无法与普通单词区分，不处理；最后一段是常见文件扩展名的不算）；
//! - 秘密（日志本来就不记秘密，这里再保险一次）：私钥块换成 `<private key>`，`password=…`、
//!   `token: …` 这类写法的值与 `Bearer …` 换成 `<redacted>`。
//!
//! 词的边界：Rust 的 `regex` 中 `\b`、`\w` 默认按 Unicode 判断，"主机10.0.0.5"这类中文紧挨地址的写法
//! 会被当成一个词而漏掉，这里一律按 ASCII 判断（与 Go 的 RE2 相同）；`Add::new` 这类 Rust 路径不当作 IPv6。

use std::sync::LazyLock;

use regex::{Captures, Regex};
use serde_json::Value;

/// 崩溃说明的长度上限（字符）。
pub const MAX_MESSAGE_CHARS: usize = 500;
/// 调用栈的长度上限（字符）。
pub const MAX_STACK_CHARS: usize = 8000;

/// IPv6 地址中的一组。
const H16: &str = "[0-9a-f]{1,4}";

static PRIVATE_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----(?s:.*?)(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
    )
    .expect("私钥块的正则")
});
/// `Bearer xxx`、`Basic xxx`（HTTP 认证头的值）。
static AUTH_SCHEME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?-u:\b)(bearer|basic)(\s+)[A-Za-z0-9._~+/=-]{8,}").expect("认证头的正则")
});
/// `password=xxx`、`"token": "xxx"`、`secret: xxx` 这类写法的值。
static SECRET_FIELD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)(?-u:\b)([0-9A-Za-z_]*?(?:password|passwd|passphrase|pwd|secret|token|api[_-]?key|access[_-]?key|private[_-]?key|authorization))",
        r#"(["']?\s*[=:]\s*)"#,
        r#"("(?:[^"\\]|\\.)*"|'[^']*'|[^\s,;&"'}\]]+)"#,
    ))
    .expect("秘密字段的正则")
});
static URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?-u:\b)[a-z][a-z0-9+.-]*://[^\s)'"`\\]+"#).expect("网址的正则")
});
static USER_HOST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[0-9A-Za-z_.+-]+@[0-9A-Za-z_-]+(?:\.[0-9A-Za-z_-]+)*").expect("user@host 的正则")
});
static MODULE_VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^@v[0-9]+\.[0-9]+").expect("模块版本的正则"));
static IPV4: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?-u:\b)(?:[0-9]{1,3}\.){3}[0-9]{1,3}(?-u:\b)").expect("IPv4 的正则")
});
/// IPv6 地址的 zone（`%eth0`）。
const ZONE: &str = r"(?:%[0-9A-Za-z_.-]+)?";
/// IPv6：完整写法、`::` 压缩写法、方括号中的写法。第 1 组只在"`h16:` 若干组后接 `::` 结尾"的写法时匹配
/// （`fe80::`），要求后面不是字母数字（`Add::new` 不算）。
static IPV6: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        concat!(
            r"(?i)(?:",
            r"(?-u:\b)(?:{h}:){{1,7}}(?::{h}){{1,7}}(?-u:\b)",
            r"|(?-u:\b)(?:{h}:){{3,7}}{h}(?-u:\b)",
            r"|(?-u:\b)((?:{h}:){{1,7}}:)",
            r"){z}",
            r"|\[[0-9a-f:]*::[0-9a-f:]*{z}\]",
        ),
        h = H16,
        z = ZONE,
    ))
    .expect("IPv6 的正则")
});
/// 以 `::` 开头的 IPv6（`::1`、`::ffff:a00:1`），要求前面不是字母数字（`Foo::Add` 不算）。Rust 的 `regex` 不允许 ASCII 的
/// `\B`（可能落在多字节字符中间），所以匹配前面的那个字符再原样放回。
static IPV6_LEADING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)(^|[^0-9A-Za-z_])::(?:{h}:){{0,6}}{h}(?-u:\b){z}",
        h = H16,
        z = ZONE,
    ))
    .expect("IPv6 的正则")
});
static DOMAIN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?-u:\b)(?:[a-z0-9-]+\.){2,}([a-z]{2,})(?-u:\b)").expect("域名的正则")
});
/// 崩溃说明中的 `host:port`。
static HOST_PORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?-u:\b)[0-9A-Za-z_-]+(?:\.[0-9A-Za-z_-]+)*:[0-9]{2,5}(?-u:\b)")
        .expect("host:port 的正则")
});
/// 崩溃说明中引号里的内容。
static QUOTED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("\"[^\"\\n]*\"|'[^'\\n]*'|`[^`\\n]*`").expect("引号内容的正则"));
static IDENTIFIER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^.[A-Za-z_$][0-9A-Za-z_$]*.$").expect("标识符的正则"));
/// JSON 文本中的字符串（含键名），是键时连同冒号与后面的标量值（字符串、数字、`true`、`false`）：合法的 JSON 中
/// 字符串之外没有引号，后面跟着冒号的字符串一定是键，从左到右匹配不会错位。
static JSON_STRING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r#"("(?:[^"\\]|\\.)*")"#,
        r#"(?:(\s*:\s*)("(?:[^"\\]|\\.)*"|-?[0-9][0-9.eE+-]*|true|false))?"#,
    ))
    .expect("JSON 字符串的正则")
});

/// 三级域名的最后一段是这些时多半是文件名（`termlet.2026-10-20.log`），不当作域名。
const FILE_EXTENSIONS: &[&str] = &[
    "log", "txt", "json", "db", "rs", "exe", "dll", "toml", "yml", "yaml", "md", "sql", "png",
    "pem", "ppk", "pub", "tmp", "bak", "zip", "gz",
];

/// 秘密字段的键名：[`Scrubber::value`] 中这些键的值整个换掉。
const SECRET_KEYS: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "secret",
    "token",
    "apikey",
    "accesskey",
    "privatekey",
    "authorization",
];

/// 去掉个人信息与秘密，见模块说明。
#[derive(Clone, Debug, Default)]
pub struct Scrubber {
    home: Option<Regex>,
    user: Option<Regex>,
}

impl Scrubber {
    /// 按用户目录与系统用户名创建；二者为空时不处理对应的部分。
    pub fn new(home: &str, username: &str) -> Self {
        Self {
            home: home_pattern(home),
            user: user_pattern(username),
        }
    }

    /// 按当前用户创建（Windows 上用户名去掉 `DOMAIN\` 前缀）。
    pub fn current_user() -> Self {
        let home = dirs::home_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        Self::new(&home, &current_username())
    }

    /// 去掉文本中的个人信息与秘密。
    pub fn text(&self, text: &str) -> String {
        let out = replace_domains(&self.base(text));
        self.replace_user(&out)
    }

    /// 去掉 JSON 文本中的个人信息：只处理其中的字符串（含键名），按解码后的内容处理再重新编码，数字、`true`、
    /// `null` 与结构不变（用户名是纯数字的工号、`true`、`null` 时也不会把 JSON 弄坏），原样未变的字符串保留原来的
    /// 转义写法。秘密字段（`password` 等键，与 [`Scrubber::value`] 相同）的字符串、数字、布尔值换成 `"<redacted>"`
    /// （键与值分开时文本规则看不出它们连在一起）。`text` 不是合法的 JSON 时按 [`Scrubber::text`] 处理。
    pub fn json(&self, text: &str) -> String {
        let trimmed = text.trim_start();
        let maybe_json = trimmed.starts_with('{') || trimmed.starts_with('[');
        if !maybe_json || serde_json::from_str::<serde::de::IgnoredAny>(text).is_err() {
            return self.text(text);
        }
        JSON_STRING
            .replace_all(text, |c: &Captures<'_>| {
                let key = self.json_string(&c[1]);
                let (Some(colon), Some(value)) = (c.get(2), c.get(3)) else {
                    return key;
                };
                let secret = serde_json::from_str::<String>(&c[1]).is_ok_and(|k| is_secret_key(&k));
                let value = if secret {
                    "\"<redacted>\"".to_string()
                } else if value.as_str().starts_with('"') {
                    self.json_string(value.as_str())
                } else {
                    value.as_str().to_string()
                };
                format!("{key}{}{value}", colon.as_str())
            })
            .into_owned()
    }

    /// [`Scrubber::json`] 中的一个字符串（带引号）：按解码后的内容处理再重新编码，没有变化时保留原来的写法。
    fn json_string(&self, quoted: &str) -> String {
        let Ok(value) = serde_json::from_str::<String>(quoted) else {
            return quoted.to_string();
        };
        let clean = self.text(&value);
        if clean == value {
            return quoted.to_string();
        }
        serde_json::to_string(&clean).unwrap_or_else(|_| quoted.to_string())
    }

    /// 去掉 JSON 值中的个人信息：所有字符串与键名按 [`Scrubber::text`] 处理，秘密字段（`password` 等键）的值
    /// 换成 `<redacted>`。
    pub fn value(&self, value: &mut Value) {
        match value {
            Value::String(s) => {
                let clean = self.text(s);
                if clean != *s {
                    *s = clean;
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|v| self.value(v)),
            Value::Object(map) => {
                let entries = std::mem::take(map);
                for (key, mut v) in entries {
                    if is_secret_key(&key) && !v.is_null() {
                        v = Value::String("<redacted>".into());
                    } else {
                        self.value(&mut v);
                    }
                    map.insert(self.text(&key), v);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }

    /// 崩溃说明：在 [`Scrubber::text`] 的基础上再去掉引号里的内容（标识符除外）与 `host:port`，超长时截断。
    pub fn message(&self, text: &str) -> String {
        let out = self.base(text);
        let out = QUOTED.replace_all(&out, |c: &Captures<'_>| {
            let m = &c[0];
            if IDENTIFIER.is_match(m) {
                m.to_string()
            } else {
                let q = &m[..1];
                format!("{q}…{q}")
            }
        });
        let out = HOST_PORT.replace_all(&out, "<host>:<port>");
        let out = replace_domains(&out);
        truncate(&self.replace_user(&out), MAX_MESSAGE_CHARS, "…")
    }

    /// 调用栈：保留函数名、文件位置与模块版本，去掉用户目录、用户名、地址等；超长时截断。不做域名一项
    /// （函数名形如 `a.b.c`）。
    pub fn stack(&self, text: &str) -> String {
        let out = self.replace_user(&self.base(text));
        truncate(&out, MAX_STACK_CHARS, "\n…")
    }

    /// 只去掉私钥块（跨行的内容逐行处理之前先整体处理一遍）。
    pub(crate) fn private_keys(&self, text: &str) -> String {
        PRIVATE_KEY.replace_all(text, "<private key>").into_owned()
    }

    /// 秘密、用户目录、网址、`user@host`、IP 地址（不含域名与用户名两项）。
    fn base(&self, text: &str) -> String {
        let mut out = PRIVATE_KEY.replace_all(text, "<private key>").into_owned();
        out = AUTH_SCHEME
            .replace_all(&out, "${1}${2}<redacted>")
            .into_owned();
        out = SECRET_FIELD
            .replace_all(&out, "${1}${2}<redacted>")
            .into_owned();
        if let Some(home) = &self.home {
            out = home.replace_all(&out, "~").into_owned();
        }
        out = URL
            .replace_all(&out, |c: &Captures<'_>| {
                if c[0].to_ascii_lowercase().starts_with("file://") {
                    c[0].to_string()
                } else {
                    "<url>".to_string()
                }
            })
            .into_owned();
        out = USER_HOST
            .replace_all(&out, |c: &Captures<'_>| {
                let m = &c[0];
                let at = m.find('@').unwrap_or(0);
                if MODULE_VERSION.is_match(&m[at..]) {
                    m.to_string()
                } else {
                    "<user>@<host>".to_string()
                }
            })
            .into_owned();
        out = IPV4
            .replace_all(&out, |c: &Captures<'_>| {
                let m = &c[0];
                if m.starts_with("127.") || m == "0.0.0.0" {
                    m.to_string()
                } else {
                    "<ip>".to_string()
                }
            })
            .into_owned();
        let haystack = out;
        let out = IPV6.replace_all(&haystack, |c: &Captures<'_>| {
            let whole = c.get(0).expect("整个匹配");
            // `Add::new`：以 `::` 结尾的写法后面紧跟字母数字时不是地址
            if c.get(1).is_some()
                && haystack[whole.end()..]
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_')
            {
                whole.as_str().to_string()
            } else {
                "<ip>".to_string()
            }
        });
        IPV6_LEADING.replace_all(&out, "${1}<ip>").into_owned()
    }

    fn replace_user(&self, text: &str) -> String {
        match &self.user {
            Some(user) => user.replace_all(text, "<user>").into_owned(),
            None => text.to_string(),
        }
    }
}

/// 用户目录的正则：分隔符 `\` 与 `/` 都匹配，JSON 中转义后的 `\\` 也匹配；以字母数字结尾时要求整词结束
/// （`/home/ab` 不匹配 `/home/abc`）。
fn home_pattern(home: &str) -> Option<Regex> {
    let home = home.trim_end_matches(['\\', '/']);
    if home.chars().count() <= 1 {
        return None;
    }
    let parts: Vec<String> = home
        .split(['\\', '/'])
        .filter(|p| !p.is_empty())
        .map(regex::escape)
        .collect();
    let mut pattern = if home.starts_with('/') {
        format!("(?i)/{}", parts.join("/"))
    } else {
        format!(r"(?i){}", parts.join(r"(?:\\\\|\\|/)"))
    };
    if home
        .chars()
        .last()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        pattern.push_str(r"(?-u:\b)");
    }
    Regex::new(&pattern).ok()
}

fn user_pattern(username: &str) -> Option<Regex> {
    let ascii = !username.is_empty()
        && username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    let len = username.chars().count();
    if (ascii && len < 3) || (!ascii && len < 2) {
        return None;
    }
    let name = regex::escape(username);
    let pattern = if ascii {
        format!(r"(?i)(?-u:\b){name}(?-u:\b)")
    } else {
        format!("(?i){name}")
    };
    Regex::new(&pattern).ok()
}

/// 当前的系统用户名（Windows 上去掉 `DOMAIN\` 前缀）。
fn current_username() -> String {
    let name = std::env::var(if cfg!(windows) { "USERNAME" } else { "USER" })
        .ok()
        .filter(|n| !n.is_empty())
        .or_else(|| {
            dirs::home_dir().and_then(|h| h.file_name().map(|n| n.to_string_lossy().into_owned()))
        })
        .unwrap_or_default();
    match name.rfind(['\\', '/']) {
        Some(i) => name[i + 1..].to_string(),
        None => name,
    }
}

fn is_secret_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    SECRET_KEYS.iter().any(|k| normalized.ends_with(k))
}

/// 三级及以上的域名换成 `<host>`（最后一段是常见文件扩展名的除外）。
fn replace_domains(text: &str) -> String {
    DOMAIN
        .replace_all(text, |c: &Captures<'_>| {
            if FILE_EXTENSIONS.contains(&c[1].to_ascii_lowercase().as_str()) {
                c[0].to_string()
            } else {
                "<host>".to_string()
            }
        })
        .into_owned()
}

/// 超过 `max` 个字符时截断并加上 `suffix`。
fn truncate(text: &str, max: usize, suffix: &str) -> String {
    match text.char_indices().nth(max) {
        Some((i, _)) => format!("{}{suffix}", &text[..i]),
        None => text.to_string(),
    }
}
