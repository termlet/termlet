//! 检测服务器端口：列出服务器上正在监听的 TCP 端口，供界面建议本地转发。
//!
//! 在终端所在的连接上执行一条只读命令：优先用 iproute2 的 `ss`（`-H` 不输出表头，旧版本不支持时去掉），
//! 没有 `ss` 时用 `netstat`（net-tools 或 busybox）。不需要 root，只是看不到其他用户的进程名。

use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

use crate::ssh::{Connection, SshError, exec};

/// 列出监听端口的脚本。
pub const PORTS_SCRIPT: &str = "ss -ltnpH 2>/dev/null || ss -ltnp 2>/dev/null || netstat -ltnp 2>/dev/null || netstat -ltn 2>/dev/null";

/// 输出的保留上限。
const OUTPUT_LIMIT: usize = 256 * 1024;
/// 执行的最长时间。
const TIMEOUT: Duration = Duration::from_secs(10);

/// 服务器上一个正在监听的 TCP 端口。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListeningPort {
    /// 监听地址：0.0.0.0 / :: 表示全部地址
    pub address: String,
    pub port: u16,
    /// 监听的进程；只有当前用户自己的进程（或以 root 登录时）才能看到
    pub process: Option<String>,
    pub pid: Option<u32>,
}

impl ListeningPort {
    /// 本地转发到这个端口时的目标地址：监听全部地址的用 127.0.0.1 访问，其余用监听的地址。
    pub fn target_host(&self) -> &str {
        match self.address.as_str() {
            "0.0.0.0" | "::" => "127.0.0.1",
            other => other,
        }
    }
}

/// 经 `sh -c` 执行 [`PORTS_SCRIPT`] 的命令：exec 请求交给用户的登录 shell 解释，登录 shell 是 fish、tcsh 时
/// `||`、`2>/dev/null` 的写法不同。脚本中没有单引号，可以直接包在单引号里。
pub fn ports_command() -> String {
    format!("sh -c '{PORTS_SCRIPT}' sh")
}

/// 列出 `connection` 所在服务器上正在监听的 TCP 端口（按端口、地址排序并去重）。
///
/// 用终端的连接（`OpenedTerminal::connection`）执行，不需要再登录；"只用一个通道"的主机不能另开通道，界面不提供这项。
pub async fn listening_ports(connection: &Connection) -> Result<Vec<ListeningPort>, SshError> {
    let result = exec(connection, &ports_command(), OUTPUT_LIMIT, TIMEOUT).await?;
    Ok(parse_ports(&String::from_utf8_lossy(&result.stdout)))
}

static SS_PROCESS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"users:\(\("([^"]+)",pid=(\d+)"#).expect("正则表达式"));

/// 解析 `ss` 或 `netstat` 的输出，按端口、地址排序并去重。
///
/// ```text
/// ss：      LISTEN 0 4096 127.0.0.1:5432 0.0.0.0:* users:(("postgres",pid=812,fd=6))
/// netstat： tcp 0 0 0.0.0.0:22 0.0.0.0:* LISTEN 1/sshd -D -e
/// ```
pub fn parse_ports(output: &str) -> Vec<ListeningPort> {
    let mut ports: Vec<ListeningPort> = Vec::new();
    for line in output.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (local, process_field) = if fields.len() >= 5 && fields[0] == "LISTEN" {
            (fields[3], fields[4..].join(" "))
        } else if fields.len() >= 6 && fields[0].starts_with("tcp") && fields[5] == "LISTEN" {
            (
                fields[3],
                fields.get(6..).map(|f| f.join(" ")).unwrap_or_default(),
            )
        } else {
            continue; // 表头、UDP、非监听状态
        };
        let Some((address, port)) = split_listen_address(local) else {
            continue;
        };
        let (process, pid) = if let Some(caps) = SS_PROCESS.captures(&process_field) {
            (Some(caps[1].to_string()), caps[2].parse().ok())
        } else {
            // netstat：PID/程序名（看不到时为 -）
            match process_field
                .split_whitespace()
                .next()
                .and_then(|f| f.split_once('/'))
            {
                Some((pid, name)) => (
                    Some(name.to_string()).filter(|n| !n.is_empty()),
                    pid.parse().ok(),
                ),
                None => (None, None),
            }
        };
        if ports.iter().any(|p| p.address == address && p.port == port) {
            continue;
        }
        ports.push(ListeningPort {
            address,
            port,
            process,
            pid,
        });
    }
    ports.sort_by(|a, b| a.port.cmp(&b.port).then_with(|| a.address.cmp(&b.address)));
    ports
}

/// 拆分监听地址：去掉 IPv6 的方括号与网卡后缀（`%lo`），`*` 统一为 0.0.0.0。
fn split_listen_address(text: &str) -> Option<(String, u16)> {
    let (host, port) = text.rsplit_once(':')?;
    let port: u16 = port.parse().ok().filter(|p| *p > 0)?;
    let host = host.strip_prefix('[').unwrap_or(host);
    let host = host.strip_suffix(']').unwrap_or(host);
    let host = host.split('%').next().unwrap_or(host);
    let host = match host {
        "*" | "" => "0.0.0.0",
        other => other,
    };
    let host = host
        .parse::<std::net::IpAddr>()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|_| host.to_string());
    Some((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(address: &str, port: u16, process: Option<&str>, pid: Option<u32>) -> ListeningPort {
        ListeningPort {
            address: address.into(),
            port,
            process: process.map(Into::into),
            pid,
        }
    }

    #[test]
    fn parses_ss() {
        let out = r#"LISTEN 0      4096   127.0.0.53%lo:53         0.0.0.0:*    users:(("systemd-resolve",pid=601,fd=14))
LISTEN 0      128          0.0.0.0:22         0.0.0.0:*
LISTEN 0      244        127.0.0.1:5432       0.0.0.0:*    users:(("postgres",pid=812,fd=6))
LISTEN 0      511                *:80               *:*    users:(("nginx",pid=900,fd=7),("nginx",pid=901,fd=7))
LISTEN 0      128             [::]:22            [::]:*
LISTEN 0      128            [::1]:631           [::]:*
"#;
        assert_eq!(
            parse_ports(out),
            [
                port("0.0.0.0", 22, None, None),
                port("::", 22, None, None),
                port("127.0.0.53", 53, Some("systemd-resolve"), Some(601)),
                port("0.0.0.0", 80, Some("nginx"), Some(900)),
                port("::1", 631, None, None),
                port("127.0.0.1", 5432, Some("postgres"), Some(812)),
            ]
        );
    }

    /// net-tools 与 busybox（程序名后可能带参数、被截断）；看不到进程时为 -；
    /// 表头与 UDP 跳过。
    #[test]
    fn parses_netstat() {
        let out = "Active Internet connections (only servers)
Proto Recv-Q Send-Q Local Address           Foreign Address         State       PID/Program name
tcp        0      0 0.0.0.0:22              0.0.0.0:*               LISTEN      1/sshd -D -e [liste
tcp        0      0 127.0.0.1:6379          0.0.0.0:*               LISTEN      -
tcp6       0      0 :::22                   :::*                    LISTEN      1/sshd -D -e [liste
udp        0      0 0.0.0.0:68              0.0.0.0:*                           1/dhcp
tcp        0      0 10.0.0.5:8080           0.0.0.0:*               LISTEN      4321/java
";
        assert_eq!(
            parse_ports(out),
            [
                port("0.0.0.0", 22, Some("sshd"), Some(1)),
                port("::", 22, Some("sshd"), Some(1)),
                port("127.0.0.1", 6379, None, None),
                port("10.0.0.5", 8080, Some("java"), Some(4321)),
            ]
        );
        assert!(parse_ports("").is_empty());
    }

    /// 检测端口的命令：经 sh -c 执行，脚本原样包在单引号里。
    #[test]
    fn command_wraps_script() {
        let command = ports_command();
        assert!(command.starts_with("sh -c 'ss -ltnpH"), "{command}");
        assert!(command.ends_with("' sh"), "{command}");
        assert!(!PORTS_SCRIPT.contains('\''));
        assert_eq!(
            parse_ports("LISTEN 0 128 127.0.0.1:3000 0.0.0.0:*\n"),
            [port("127.0.0.1", 3000, None, None)]
        );
    }

    #[test]
    fn target_host_for_wildcard() {
        assert_eq!(port("0.0.0.0", 1, None, None).target_host(), "127.0.0.1");
        assert_eq!(port("::", 1, None, None).target_host(), "127.0.0.1");
        assert_eq!(port("10.0.0.5", 1, None, None).target_host(), "10.0.0.5");
    }
}
