//! 网络页：TCP、UDP 连接与监听（有 ss 时用 `ss -tuanpe`，没有时读 /proc/net，拿不到进程名）。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::LazyLock;

use regex::Regex;

use super::exec::{Limits, Target, on_runtime, one_line, run, sh_script};
use super::scripts::MAX_CONNS;
use super::sections::{SectionExt, Sections, missing, uid_of, users, with_result};
use super::{HostTools, LIST_LIMIT, LIST_TIMEOUT, Privilege, ToolsError, scripts};
use crate::ssh::ExecResult;

/// 使用连接的进程。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnProcess {
    pub pid: u32,
    pub name: String,
}

/// 一个 TCP 或 UDP 套接字。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conn {
    /// tcp 或 udp（IPv6 也是 tcp、udp，看地址）
    pub proto: String,
    /// 用 ss 的写法：LISTEN、ESTAB、TIME-WAIT、CLOSE-WAIT、SYN-SENT、UNCONN 等
    pub state: String,
    /// 地址：IPv6 不带方括号，去掉网卡后缀；任意地址为 `0.0.0.0`、`::` 或 `*`（ss 的双栈写法）
    pub local_addr: String,
    pub local_port: u16,
    /// 没有对端时地址为 `*` 或任意地址、端口为 0
    pub peer_addr: String,
    pub peer_port: u16,
    pub recv_q: u64,
    pub send_q: u64,
    /// 使用它的进程（ss -p；不是 root 时只能看到自己的进程；读 /proc/net 时没有）
    pub processes: Vec<ConnProcess>,
    /// 套接字所属的用户，不知道时为 `None`
    pub uid: Option<u32>,
    pub user: Option<String>,
}

/// 某个协议、某个状态的连接总数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnCount {
    pub proto: String,
    pub state: String,
    pub count: usize,
}

/// 读到的来源。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnSource {
    /// `ss -tuanpe`
    Ss,
    /// 没有 ss，读 /proc/net：拿不到进程
    Proc,
}

/// 网络页的内容。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnList {
    /// 最多 3000 个
    pub conns: Vec<Conn>,
    /// 各协议、各状态的总数（不受列出上限的限制；tcp 与 tcp6 合在一起），按协议、状态排序
    pub counts: Vec<ConnCount>,
    pub total: usize,
    pub truncated: bool,
    pub source: ConnSource,
    pub uid: Option<u32>,
    /// 经免密码 sudo 读取（看得到别的用户的进程的进程名）
    pub sudo: bool,
}

/// 读取连接的命令：`sudo` 为真时先经 `sudo -n` 读 ss（有免密码 sudo 时）。
pub(crate) fn connections_command(sudo: bool) -> String {
    sh_script(
        &one_line(&scripts::conn_script()),
        &[if sudo { "sudo" } else { "" }],
    )
}

impl HostTools {
    /// 读取 TCP、UDP 连接（最多 3000 个，另有各状态的总数）。不是 root 而有免密码 sudo 时（[`Self::privilege`]，
    /// 每条连接探测一次）经 sudo 读，看得到全部进程的进程名。
    pub async fn connections(&self, target: &Target) -> Result<ConnList, ToolsError> {
        let sudo = matches!(self.privilege(target).await, Ok(Privilege::Sudo));
        let target = target.clone();
        on_runtime(async move {
            let limits = Limits {
                timeout: LIST_TIMEOUT,
                output: LIST_LIMIT,
                long: false,
            };
            let result = run(&target, &connections_command(sudo), limits).await?;
            finish_conns(&result)
        })
        .await
    }
}

/// 解析读取连接的执行结果。
pub(crate) fn finish_conns(result: &ExecResult) -> Result<ConnList, ToolsError> {
    parse_conns(&String::from_utf8_lossy(&result.stdout)).map_err(|e| with_result(e, result))
}

/// 解析读取连接的输出。
pub(crate) fn parse_conns(out: &str) -> Result<ConnList, ToolsError> {
    let sections = Sections::parse(out);
    if sections.has("unsupported") {
        return Err(ToolsError::Unsupported);
    }
    let names = users(sections.get("passwd"));
    let mut conns = Vec::new();
    let mut counts: Vec<ConnCount> = Vec::new();
    let source = if let Some(ss) = sections.get("ss") {
        for line in join_ss_lines(&ss.lines) {
            if let Some(conn) = parse_ss_line(&line) {
                conns.push(conn);
            }
        }
        for line in sections.get("counts").all_lines() {
            if let [proto, state, count] = line.split_whitespace().collect::<Vec<_>>()[..]
                && let Ok(count) = count.parse()
            {
                counts.push(ConnCount {
                    proto: proto.to_string(),
                    state: state.to_string(),
                    count,
                });
            }
        }
        ConnSource::Ss
    } else {
        let mut found = false;
        for proto in ["tcp", "tcp6", "udp", "udp6"] {
            let Some(section) = sections.get(&format!("proc {proto}")) else {
                continue;
            };
            found = true;
            let base = proto.trim_end_matches('6');
            for line in &section.lines {
                if let Some(conn) = parse_proc_net_line(line, base) {
                    conns.push(conn);
                }
            }
            for line in sections.get(&format!("count {proto}")).all_lines() {
                if let [code, count] = line.split_whitespace().collect::<Vec<_>>()[..]
                    && let Ok(count) = count.parse()
                {
                    add_count(&mut counts, base, &proc_state(base, code), count);
                }
            }
        }
        if !found {
            return Err(missing());
        }
        ConnSource::Proc
    };
    let mut total: usize = counts.iter().map(|c| c.count).sum();
    conns.truncate(MAX_CONNS);
    total = total.max(conns.len());
    for conn in &mut conns {
        conn.user = conn.uid.and_then(|uid| names.get(&uid).cloned());
    }
    counts.sort_by(|a, b| a.proto.cmp(&b.proto).then_with(|| a.state.cmp(&b.state)));
    Ok(ConnList {
        truncated: total > conns.len(),
        conns,
        counts,
        total,
        source,
        uid: uid_of(sections.get("uid")),
        sudo: sections.get("sudo").arg().trim() == "used",
    })
}

/// 把同一协议、同一状态的计数合并（tcp 与 tcp6 合在一起）。
fn add_count(list: &mut Vec<ConnCount>, proto: &str, state: &str, count: usize) {
    if let Some(existing) = list
        .iter_mut()
        .find(|c| c.proto == proto && c.state == state)
    {
        existing.count += count;
        return;
    }
    list.push(ConnCount {
        proto: proto.to_string(),
        state: state.to_string(),
        count,
    });
}

/// 把 ss 的输出还原成每个套接字一行：ss 不转义进程名，名称中的换行把一行拆成几行，不以协议（tcp、udp）
/// 或表头（Netid）开头的行接到上一行（以换行连接，名称中保留换行）。
fn join_ss_lines(lines: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in lines {
        let head = line.split(' ').next().unwrap_or("");
        if let Some(last) = out.last_mut()
            && !matches!(head, "tcp" | "udp" | "Netid")
        {
            last.push('\n');
            last.push_str(line);
            continue;
        }
        out.push(line.clone());
    }
    out
}

/// ss -p 的进程：`("名称",pid=123,fd=4)`，旧版为 `("名称",123,4)`。
static USERS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\("((?:[^"\\]|\\.)*)",(?:pid=)?(\d+)"#).expect("正则表达式"));

/// 解析 `ss -tuanpe` 的一行：协议 状态 Recv-Q Send-Q 本地地址:端口 对端地址:端口 \[users:(...)\] \[uid:N\] \[ino:N\] …
pub(crate) fn parse_ss_line(line: &str) -> Option<Conn> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 6 || !matches!(fields[0], "tcp" | "udp") {
        return None;
    }
    let (local_addr, local_port) = split_host_port(fields[4])?;
    let (peer_addr, peer_port) = split_host_port(fields[5])?;
    let mut conn = Conn {
        proto: fields[0].to_string(),
        state: fields[1].to_string(),
        local_addr,
        local_port,
        peer_addr,
        peer_port,
        recv_q: fields[2].parse().unwrap_or(0),
        send_q: fields[3].parse().unwrap_or(0),
        processes: Vec::new(),
        uid: None,
        user: None,
    };
    // 进程：从原文中取（进程名中可能有换行、连续的空格）；uid:、ino: 只从最后一个进程之后的部分取
    // （进程名中可能写着 uid:0）
    let mut tail = fields[6..].join(" ");
    if let Some(at) = line.find("users:(") {
        let users = &line[at..];
        let mut last_end = None;
        for caps in USERS_RE.captures_iter(users) {
            last_end = caps.get(0).map(|m| m.end());
            let Ok(pid) = caps[2].parse::<u32>() else {
                continue;
            };
            let name = caps[1].replace(r#"\""#, "\"");
            if !conn.processes.iter().any(|p| p.pid == pid) {
                conn.processes.push(ConnProcess { pid, name });
            }
        }
        if let Some(end) = last_end {
            tail = users[end..].to_string();
        }
    }
    let mut extended = false;
    for token in tail.split_whitespace() {
        if let Some(uid) = token.strip_prefix("uid:") {
            if let Ok(uid) = uid.parse() {
                conn.uid = Some(uid);
            }
        } else if token.starts_with("ino:") {
            extended = true;
        }
    }
    // ss -e 只在 UID 不为 0 时输出 uid:
    if extended && conn.uid.is_none() {
        conn.uid = Some(0);
    }
    Some(conn)
}

/// 拆开 ss 的地址：`0.0.0.0:22`、`[::]:80`、`[::ffff:1.2.3.4]:443`、`127.0.0.53%lo:53`、`[fe80::1]%eth0:546`
/// （IPv6 链路本地地址，网卡后缀在方括号之外）、`*:22`（旧版的任意地址）、`:::22`（旧版的 IPv6 任意地址）、
/// `*:*`（没有对端）。端口为 `*` 时是 0；去掉网卡后缀。
pub(crate) fn split_host_port(text: &str) -> Option<(String, u16)> {
    let (host, port) = text.rsplit_once(':')?;
    let port = match port {
        "*" => 0,
        other => other.parse::<u16>().ok()?,
    };
    // 网卡后缀（在方括号之后）
    let host = host.split('%').next().unwrap_or(host);
    let host = host.strip_prefix('[').unwrap_or(host);
    let host = host.strip_suffix(']').unwrap_or(host);
    let host = if host.is_empty() { "*" } else { host };
    Some((host.to_string(), port))
}

/// `/proc/net/tcp` 的状态码（十六进制）对应的 ss 写法；UDP 的 07 是 UNCONN（与 ss 一致）。
fn proc_state(proto: &str, code: &str) -> String {
    let code = code.to_ascii_uppercase();
    if proto == "udp" && code == "07" {
        return "UNCONN".into();
    }
    let state = match code.as_str() {
        "01" => "ESTAB",
        "02" => "SYN-SENT",
        "03" => "SYN-RECV",
        "04" => "FIN-WAIT-1",
        "05" => "FIN-WAIT-2",
        "06" => "TIME-WAIT",
        "07" => "CLOSE",
        "08" => "CLOSE-WAIT",
        "09" => "LAST-ACK",
        "0A" => "LISTEN",
        "0B" => "CLOSING",
        "0C" => "NEW-SYN-RECV",
        _ => return code,
    };
    state.to_string()
}

/// 解析 `/proc/net/{tcp,tcp6,udp,udp6}` 的一行：
/// `sl local_address rem_address st tx_queue:rx_queue tr:tm->when retrnsmt uid timeout inode …`
pub(crate) fn parse_proc_net_line(line: &str, proto: &str) -> Option<Conn> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 10 || !fields[0].ends_with(':') {
        return None;
    }
    let (local_addr, local_port) = proc_addr(fields[1])?;
    let (peer_addr, peer_port) = proc_addr(fields[2])?;
    let (send_q, recv_q) = fields[4]
        .split_once(':')
        .map(|(tx, rx)| {
            (
                u64::from_str_radix(tx, 16).unwrap_or(0),
                u64::from_str_radix(rx, 16).unwrap_or(0),
            )
        })
        .unwrap_or((0, 0));
    Some(Conn {
        proto: proto.to_string(),
        state: proc_state(proto, fields[3]),
        local_addr,
        local_port,
        peer_addr,
        peer_port,
        recv_q,
        send_q,
        processes: Vec::new(),
        uid: fields[7].parse().ok(),
        user: None,
    })
}

/// 解析 /proc/net 的"十六进制地址:十六进制端口"（地址按 32 位小端存放）。IPv4 映射的 IPv6 地址写成
/// `::ffff:a.b.c.d`（与 ss 相同），不写成 IPv4。
fn proc_addr(text: &str) -> Option<(String, u16)> {
    let (address, port) = text.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    if address.len() % 8 != 0 || !address.is_ascii() {
        return None;
    }
    let mut bytes = Vec::with_capacity(16);
    for chunk in address.as_bytes().chunks(8) {
        let word = u32::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
        // 按小端存放的 32 位数：低位字节在前
        bytes.extend_from_slice(&word.swap_bytes().to_be_bytes());
    }
    let ip = match bytes.len() {
        4 => IpAddr::V4(Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3])),
        16 => {
            let octets: [u8; 16] = bytes.try_into().ok()?;
            IpAddr::V6(Ipv6Addr::from(octets))
        }
        _ => return None,
    };
    Some((ip.to_string(), port))
}
