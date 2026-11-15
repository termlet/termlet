//! 端口转发规则。
//!
//! 校验与规范化：
//! - 本地、动态转发在本机监听，地址只能是回环地址（127.0.0.0/8、::1、localhost）或 0.0.0.0；空值与 localhost
//!   统一为 127.0.0.1（避免不同系统把 localhost 解析成 IPv4 / IPv6 不一致）。
//! - 远程转发在服务器上监听：空值与 localhost 为 127.0.0.1，`*` 为 0.0.0.0，其余只接受 IP 地址。监听回环以外的地址
//!   需要服务器的 GatewayPorts 允许，否则服务器只监听回环地址或拒绝。
//! - 目标是 IP 或主机名（本地转发由服务器解析，远程转发由本机解析），不允许空白与控制字符。
//! - 同一个监听地址只能有一条规则（见 [`Forward::conflicts_with`]，保存时由存储检查）。

use std::net::{IpAddr, Ipv4Addr};

use serde::{Deserialize, Serialize};

use super::{Extra, trimmed};
use crate::Invalid;
use crate::id::HostId;

/// 本机监听的默认地址：只有本机能访问。
pub const DEFAULT_BIND_HOST: &str = "127.0.0.1";
/// 所有 IPv4 网卡：局域网内的其他设备也能访问（界面提示风险）。
pub const ANY_IPV4: &str = "0.0.0.0";

/// 名称的最大长度（字符数）。
const MAX_NAME_LEN: usize = 64;
/// 目标主机名的最大长度（DNS 名称的上限）。
const MAX_TARGET_LEN: usize = 253;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Forward {
    pub kind: ForwardKind,
    /// 经由哪台主机（删除主机时一并删除规则）
    pub host: Option<HostId>,
    pub name: String,
    /// 本地转发、动态转发为本机的监听地址（回环地址或 0.0.0.0）；远程转发为服务器上的监听地址
    pub bind_host: String,
    pub bind_port: u16,
    /// 目标（本地转发从服务器的角度访问；远程转发从本机的角度访问）；动态转发没有
    pub target_host: String,
    pub target_port: u16,
    /// Termlet 启动后（第一个窗口打开时）自动启动
    pub auto_start: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for Forward {
    fn default() -> Self {
        Self {
            kind: ForwardKind::Local,
            host: None,
            name: String::new(),
            bind_host: DEFAULT_BIND_HOST.into(),
            bind_port: 0,
            target_host: String::new(),
            target_port: 0,
            auto_start: false,
            extra: Extra::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForwardKind {
    /// ssh -L
    #[default]
    Local,
    /// ssh -D（本机 SOCKS 代理）
    Dynamic,
    /// ssh -R
    Remote,
}

impl Forward {
    /// 校验并规范化：补默认值、统一监听地址与目标地址的写法。
    ///
    /// 失败的字段与原因：`name` too_long；`host` required；`bindHost` not_local（本地、动态转发不是回环地址或
    /// 0.0.0.0）/ not_ip（远程转发不是 IP）；`bindPort` invalid；`targetHost` required / too_long / invalid；
    /// `targetPort` invalid。
    pub(crate) fn normalize(&mut self) -> Result<(), Invalid> {
        trimmed(&mut self.name, "name", MAX_NAME_LEN)?;
        if self.host.is_none() {
            return Err(Invalid::new("host", "required"));
        }
        self.bind_host = match self.kind {
            ForwardKind::Remote => normalize_remote_bind(&self.bind_host)?,
            _ => normalize_local_bind(&self.bind_host)?,
        };
        if self.bind_port == 0 {
            return Err(Invalid::new("bindPort", "invalid"));
        }
        if self.kind == ForwardKind::Dynamic {
            self.target_host.clear();
            self.target_port = 0;
            return Ok(());
        }
        self.target_host = normalize_target(&self.target_host)?;
        if self.target_port == 0 {
            return Err(Invalid::new("targetPort", "invalid"));
        }
        Ok(())
    }

    /// 本机监听所有网卡：局域网内的其他设备无需认证即可经本机访问目标。
    pub fn listens_on_all_interfaces(&self) -> bool {
        self.kind != ForwardKind::Remote && self.bind_host == ANY_IPV4
    }

    /// 监听地址 `host:port`（IPv6 带方括号）。
    pub fn bind_address(&self) -> String {
        join_host_port(&self.bind_host, self.bind_port)
    }

    /// 目标地址 `host:port`（IPv6 带方括号）；动态转发没有，为空。
    pub fn target_address(&self) -> String {
        if self.kind == ForwardKind::Dynamic {
            return String::new();
        }
        join_host_port(&self.target_host, self.target_port)
    }

    /// 两条规则能否同时使用（都已规范化）：端口相同且地址相同，或其中一个是 0.0.0.0、另一个是 IPv4 地址
    /// （0.0.0.0 已包含全部 IPv4 地址）时冲突。本地、动态转发监听在本机，彼此冲突；远程转发监听在各自的服务器上，
    /// 只与同一台主机的远程转发冲突。
    pub fn conflicts_with(&self, other: &Forward) -> bool {
        let remote = self.kind == ForwardKind::Remote;
        if self.bind_port != other.bind_port || remote != (other.kind == ForwardKind::Remote) {
            return false;
        }
        if remote && self.host != other.host {
            return false;
        }
        if self.bind_host == other.bind_host {
            return true;
        }
        let is_v4 = |h: &str| h.parse::<Ipv4Addr>().is_ok();
        (self.bind_host == ANY_IPV4 && is_v4(&other.bind_host))
            || (other.bind_host == ANY_IPV4 && is_v4(&self.bind_host))
    }
}

/// `host:port`，IPv6 地址加方括号。
pub fn join_host_port(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// 去掉首尾空白与 IPv6 的方括号。
fn strip(host: &str) -> &str {
    let host = host.trim();
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
}

/// 本机的监听地址：回环地址或 0.0.0.0；IPv6 的任意地址（::）、局域网地址、主机名都不接受。
fn normalize_local_bind(host: &str) -> Result<String, Invalid> {
    let host = strip(host);
    if host.is_empty() || host.eq_ignore_ascii_case("localhost") {
        return Ok(DEFAULT_BIND_HOST.into());
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) if ip.is_unspecified() => Ok(ANY_IPV4.into()),
        Ok(ip) if ip.is_loopback() => Ok(ip.to_string()),
        _ => Err(Invalid::new("bindHost", "not_local")),
    }
}

/// 服务器上的监听地址：空值与 localhost 为 127.0.0.1，`*` 为 0.0.0.0，其余只接受 IP 地址。
fn normalize_remote_bind(host: &str) -> Result<String, Invalid> {
    let host = strip(host);
    if host.is_empty() || host.eq_ignore_ascii_case("localhost") {
        return Ok(DEFAULT_BIND_HOST.into());
    }
    if host == "*" {
        return Ok(ANY_IPV4.into());
    }
    host.parse::<IpAddr>()
        .map(|ip| ip.to_string())
        .map_err(|_| Invalid::new("bindHost", "not_ip"))
}

/// 目标地址：IP 或主机名（字母、数字、`.`、`-`、`_`）。
fn normalize_target(host: &str) -> Result<String, Invalid> {
    let host = strip(host);
    if host.is_empty() {
        return Err(Invalid::new("targetHost", "required"));
    }
    if host.len() > MAX_TARGET_LEN {
        return Err(Invalid::new("targetHost", "too_long"));
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(ip.to_string());
    }
    if host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        Ok(host.to_string())
    } else {
        Err(Invalid::new("targetHost", "invalid"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Forward {
        Forward {
            host: Some(HostId::from_raw("h_1")),
            bind_port: 8080,
            target_host: "127.0.0.1".into(),
            target_port: 80,
            ..Default::default()
        }
    }

    fn rule(kind: ForwardKind, host: &str, bind_host: &str, port: u16) -> Forward {
        Forward {
            kind,
            host: Some(HostId::from_raw(host)),
            bind_host: bind_host.into(),
            bind_port: port,
            ..Default::default()
        }
    }

    #[test]
    fn local_forward_needs_loopback_or_any() {
        let mut rule = Forward {
            bind_port: 5432,
            target_port: 5432,
            ..base()
        };
        rule.normalize().unwrap();
        rule.bind_host = "192.168.1.5".into();
        assert_eq!(rule.normalize().unwrap_err().reason, "not_local");
        rule.bind_host = "0.0.0.0".into();
        rule.normalize().unwrap();
        assert!(rule.listens_on_all_interfaces());
    }

    #[test]
    fn dynamic_forward_drops_target() {
        let mut rule = Forward {
            kind: ForwardKind::Dynamic,
            bind_port: 1080,
            target_host: "x".into(),
            target_port: 1,
            ..base()
        };
        rule.normalize().unwrap();
        assert!(rule.target_host.is_empty());
        assert_eq!(rule.target_port, 0);
        assert_eq!(rule.target_address(), "");
    }

    /// 可以接受的写法与规范化的结果。
    #[test]
    fn normalize_accepts() {
        let cases: [(&str, fn(&mut Forward), &str, &str); 7] = [
            ("默认监听 127.0.0.1", |_| {}, "127.0.0.1", "127.0.0.1"),
            (
                "localhost 统一为 127.0.0.1",
                |r| r.bind_host = "localhost".into(),
                "127.0.0.1",
                "127.0.0.1",
            ),
            (
                "其他 IPv4 回环地址",
                |r| r.bind_host = "127.0.0.2".into(),
                "127.0.0.2",
                "127.0.0.1",
            ),
            (
                "IPv6 回环地址（带方括号）",
                |r| r.bind_host = "[::1]".into(),
                "::1",
                "127.0.0.1",
            ),
            (
                "0.0.0.0",
                |r| r.bind_host = "0.0.0.0".into(),
                "0.0.0.0",
                "127.0.0.1",
            ),
            (
                "目标为主机名",
                |r| r.target_host = " db.internal_1 ".into(),
                "127.0.0.1",
                "db.internal_1",
            ),
            (
                "目标为 IPv6",
                |r| r.target_host = "[fe80::1]".into(),
                "127.0.0.1",
                "fe80::1",
            ),
        ];
        for (name, mutate, bind, target) in cases {
            let mut rule = base();
            rule.bind_host = String::new();
            mutate(&mut rule);
            rule.normalize().unwrap_or_else(|e| panic!("{name}：{e}"));
            assert_eq!(rule.bind_host, bind, "{name}");
            assert_eq!(rule.target_host, target, "{name}");
            assert_eq!(rule.kind, ForwardKind::Local, "{name}");
        }
    }

    /// 不接受的写法。
    #[test]
    fn normalize_rejects() {
        let cases: [(&str, fn(&mut Forward), &str, &str); 11] = [
            ("缺少主机", |r| r.host = None, "host", "required"),
            (
                "监听局域网地址",
                |r| r.bind_host = "192.168.1.10".into(),
                "bindHost",
                "not_local",
            ),
            (
                "监听 IPv6 任意地址",
                |r| r.bind_host = "::".into(),
                "bindHost",
                "not_local",
            ),
            (
                "监听主机名",
                |r| r.bind_host = "example.com".into(),
                "bindHost",
                "not_local",
            ),
            ("本地端口为 0", |r| r.bind_port = 0, "bindPort", "invalid"),
            (
                "目标端口为 0",
                |r| r.target_port = 0,
                "targetPort",
                "invalid",
            ),
            (
                "目标为空",
                |r| r.target_host = String::new(),
                "targetHost",
                "required",
            ),
            (
                "目标含空格",
                |r| r.target_host = "a b".into(),
                "targetHost",
                "invalid",
            ),
            (
                "目标含控制字符",
                |r| r.target_host = "a\nb".into(),
                "targetHost",
                "invalid",
            ),
            (
                "目标过长",
                |r| r.target_host = "a".repeat(254),
                "targetHost",
                "too_long",
            ),
            ("名称过长", |r| r.name = "名".repeat(65), "name", "too_long"),
        ];
        for (name, mutate, field, reason) in cases {
            let mut rule = base();
            mutate(&mut rule);
            assert_eq!(rule.normalize(), Err(Invalid::new(field, reason)), "{name}");
        }
        // 端口超出 65535 由类型保证（u16）；名称正好 64 个字符可以
        let mut rule = Forward {
            name: "名".repeat(64),
            ..base()
        };
        rule.normalize().unwrap();
    }

    #[test]
    fn bind_conflicts() {
        let local = |h: &str, p: u16| rule(ForwardKind::Local, "h1", h, p);
        let cases = [
            (local("127.0.0.1", 80), local("127.0.0.1", 80), true),
            (local("127.0.0.1", 80), local("127.0.0.1", 81), false),
            (local("127.0.0.1", 80), local("127.0.0.2", 80), false),
            (local("0.0.0.0", 80), local("127.0.0.1", 80), true),
            (local("127.0.0.1", 80), local("0.0.0.0", 80), true),
            (local("0.0.0.0", 80), local("::1", 80), false),
        ];
        for (a, b, want) in cases {
            assert_eq!(
                a.conflicts_with(&b),
                want,
                "{} 与 {}",
                a.bind_address(),
                b.bind_address()
            );
        }
        // 动态转发也监听在本机，与本地转发冲突
        let dynamic = rule(ForwardKind::Dynamic, "h2", "127.0.0.1", 80);
        assert!(dynamic.conflicts_with(&local("127.0.0.1", 80)));
    }

    /// 远程转发的规范化与冲突（存储中的冲突见 forward 模块的测试）。
    #[test]
    fn remote_rules() {
        let mut remote = Forward {
            kind: ForwardKind::Remote,
            bind_host: "*".into(),
            target_host: "localhost".into(),
            target_port: 3000,
            ..base()
        };
        remote.normalize().unwrap();
        assert_eq!(remote.bind_host, ANY_IPV4);
        assert_eq!(remote.target_host, "localhost");
        assert!(
            !remote.listens_on_all_interfaces(),
            "远程转发监听在服务器上"
        );

        let mut named = Forward {
            bind_host: "example.com".into(),
            ..remote.clone()
        };
        assert_eq!(
            named.normalize(),
            Err(Invalid::new("bindHost", "not_ip")),
            "服务器上的监听地址应为 IP"
        );
        let mut empty = Forward {
            bind_host: " ".into(),
            ..remote.clone()
        };
        empty.normalize().unwrap();
        assert_eq!(empty.bind_host, DEFAULT_BIND_HOST);

        let local = rule(ForwardKind::Local, "h1", "127.0.0.1", 8080);
        let remote = rule(ForwardKind::Remote, "h1", "127.0.0.1", 8080);
        let other = rule(ForwardKind::Remote, "h2", "127.0.0.1", 8080);
        assert!(!local.conflicts_with(&remote));
        assert!(!remote.conflicts_with(&other));
        assert!(remote.conflicts_with(&remote));
    }

    #[test]
    fn addresses_bracket_ipv6() {
        let mut rule = Forward {
            bind_host: "::1".into(),
            target_host: "fe80::1".into(),
            ..base()
        };
        rule.normalize().unwrap();
        assert_eq!(rule.bind_address(), "[::1]:8080");
        assert_eq!(rule.target_address(), "[fe80::1]:80");
        assert_eq!(join_host_port("db", 5432), "db:5432");
    }
}
