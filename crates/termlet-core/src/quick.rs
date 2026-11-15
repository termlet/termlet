//! 快速连接：在搜索框或命令面板中输入地址直接连接（作为临时连接打开，可另存为主机）。
//!
//! 认得的写法：`user@host`、`user@host:port`、`user@[ipv6]:port`、`ssh://user@host:port`、`ssh user@host -p 2222`。
//! 必须带用户名（与 Tabby、Termius 相同）；用户名中可以有 `@`（JumpServer 的 `用户@资产`），取最后一个 `@` 之后为地址。

/// 快速连接的目标。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuickTarget {
    pub username: String,
    pub host: String,
    pub port: u16,
}

impl QuickTarget {
    /// 显示用的写法：`user@host` 或 `user@host:port`（IPv6 加方括号）。
    pub fn display(&self) -> String {
        format!("{}@{}", self.username, self.host_display())
    }

    /// 不带用户名的写法：`host` 或 `host:port`（IPv6 加方括号）。
    pub fn host_display(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == 22 {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

/// 解析快速连接的地址；认不出时返回 `None`。
pub fn parse(input: &str) -> Option<QuickTarget> {
    let mut text = input.trim().to_string();
    let mut port: Option<u32> = None;

    // ssh user@host -p 2222（-p 也可以写在前面）
    let lower = text.to_ascii_lowercase();
    if lower.starts_with("ssh ") || lower.starts_with("ssh\t") {
        let words: Vec<&str> = text.split_whitespace().skip(1).collect();
        let mut rest = Vec::new();
        let mut i = 0;
        while i < words.len() {
            let word = words[i];
            if word == "-p" {
                i += 1;
                port = Some(words.get(i)?.parse().ok()?);
            } else if let Some(value) = word.strip_prefix("-p").filter(|v| !v.is_empty()) {
                port = Some(value.parse().ok()?);
            } else if word.starts_with('-') {
                return None;
            } else {
                rest.push(word);
            }
            i += 1;
        }
        if rest.len() != 1 {
            return None;
        }
        text = rest[0].to_string();
    }

    // 按字节切片前先确认是字符边界：搜索框里输入中文时也会调用这里
    if text
        .get(..6)
        .is_some_and(|p| p.eq_ignore_ascii_case("ssh://"))
    {
        text = text[6..].to_string();
    }
    let text = text.strip_suffix('/').unwrap_or(&text);
    let at = text.rfind('@')?;
    if at == 0 {
        return None;
    }
    let username = &text[..at];
    let host_part = &text[at + 1..];
    if username.chars().any(|c| c.is_whitespace() || c == '/') || username.chars().count() > 128 {
        return None;
    }

    let host = if let Some(rest) = host_part.strip_prefix('[') {
        let close = rest.find(']')?;
        let host = &rest[..close];
        let after = &rest[close + 1..];
        if let Some(p) = after.strip_prefix(':') {
            port = port.or(Some(p.parse().ok()?));
        } else if !after.is_empty() {
            return None;
        }
        host.to_string()
    } else {
        let (host, p) = match host_part.split_once(':') {
            Some((host, p)) => (host, Some(p)),
            None => (host_part, None),
        };
        if let Some(p) = p {
            if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            port = port.or(Some(p.parse().ok()?));
        }
        host.to_string()
    };
    if !valid_host(&host) {
        return None;
    }
    let port = port.unwrap_or(22);
    if !(1..=65535).contains(&port) {
        return None;
    }
    Some(QuickTarget {
        username: username.to_string(),
        host,
        port: port as u16,
    })
}

fn valid_host(host: &str) -> bool {
    if host.is_empty() {
        return false;
    }
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() == 4
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 3 && p.chars().all(|c| c.is_ascii_digit()))
    {
        return parts
            .iter()
            .all(|p| p.parse::<u16>().is_ok_and(|v| v <= 255));
    }
    if host.contains(':') {
        // IPv6（已去掉方括号）
        return host
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.');
    }
    parts.iter().all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(username: &str, host: &str, port: u16) -> Option<QuickTarget> {
        Some(QuickTarget {
            username: username.into(),
            host: host.into(),
            port,
        })
    }

    #[test]
    fn recognized_forms() {
        assert_eq!(parse("root@10.0.0.1"), target("root", "10.0.0.1", 22));
        assert_eq!(
            parse(" deploy@web-1.example.com:2222 "),
            target("deploy", "web-1.example.com", 2222)
        );
        assert_eq!(
            parse("ssh://u@[fe80::1]:2200/"),
            target("u", "fe80::1", 2200)
        );
        assert_eq!(parse("ssh -p 2022 admin@db"), target("admin", "db", 2022));
        assert_eq!(parse("ssh admin@db -p2023"), target("admin", "db", 2023));
        assert_eq!(
            parse("JMS-1@corp@jms.example.com:2222").unwrap().username,
            "JMS-1@corp"
        );
    }

    #[test]
    fn unrecognized_forms() {
        for s in [
            "web-1",
            "root@",
            "@host",
            "root@ho st",
            "root@host:99999",
            "root@256.1.1.1",
            "ssh -i k root@h",
            "ls -la",
            "db测试",
            "测试主机",
            "ssh测",
        ] {
            assert_eq!(parse(s), None, "{s}");
        }
    }

    #[test]
    fn display_form() {
        assert_eq!(target("u", "h", 22).unwrap().display(), "u@h");
        assert_eq!(
            target("u", "fe80::1", 2200).unwrap().display(),
            "u@[fe80::1]:2200"
        );
    }
}
