//! 被堡垒机等外部程序调起：解析它们传给 Xshell、PuTTY、SecureCRT、MobaXterm、WinSCP / Xftp 的启动参数。
//!
//! 用户在堡垒机的本地客户端设置中把这些软件的路径换成 Termlet 即可。格式按 JumpServer 客户端的启动模板
//! （github.com/jumpserver/client 的 `plugins/windows/*/connect.json`）整理；绿盟 OSMS 传 .xsh 会话文件。
//! 只解析参数、不注册 URL 协议：能传参数的是本机程序，本来就能执行任意命令，不增加攻击面。
//!
//! 按下面的顺序判断（参数不分大小写、顺序任意，以 `--` 开头的参数忽略）：
//!
//! 1. MobaXterm：`-newtab "sshpass -p<密码> ssh -o StrictHostKeyChecking=no <用户>@<地址> -p<端口>"`
//! 2. Xshell（JumpServer）：`-newtab <名称> -url ssh://<用户>:<密码>@<地址>:<端口>`
//! 3. 绿盟 OSMS：`-newtab <名称> <会话文件>.xsh`（见 [`parse_xsh`]）
//! 4. PuTTY：`-ssh <用户>@<地址> -P <端口> -pw <密码>`，用户也可以用 `-l` 给出
//! 5. SecureCRT：`/N <名称> /T /SSH2 /ACCEPTHOSTKEYS /P <端口> /PASSWORD <密码> /L <用户> <地址>`
//! 6. WinSCP、SecureFX、Xftp（`-newwin`）与裸链接：参数中第一个 `ssh://`、`sftp://`、`scp://` 链接，
//!    名称取 `/sessionname=`（或 `-sessionname=`），没有时用 `-newtab` 的值
//!
//! 被调起的连接是临时连接：不进主机列表，密码只在内存中。作者：ruan

use std::borrow::Cow;
use std::path::Path;

use zeroize::Zeroizing;

use crate::model::Encoding;
use crate::quick::QuickTarget;
use crate::secret::SecretString;

mod xsh;

#[cfg(test)]
mod tests;

pub use xsh::{MAX_XSH_SIZE, parse_xsh};

/// SSH 与 SFTP 的默认端口。
const DEFAULT_PORT: u16 = 22;
/// 地址的长度上限（字符）。
const MAX_ADDRESS: usize = 255;
/// 用户名的长度上限（字符）。
const MAX_USERNAME: usize = 255;
/// 密码、名称的长度上限（字符）。
const MAX_TEXT: usize = 4096;

/// 调起后打开什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchKind {
    /// SSH 终端
    Terminal,
    /// 文件管理（`sftp://`、`scp://` 链接）
    Sftp,
}

impl LaunchKind {
    /// 日志中的写法。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Sftp => "sftp",
        }
    }
}

/// 认出的格式，写进日志便于排查。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchSource {
    MobaXterm,
    /// Xshell 的 `-url`（JumpServer）
    Xshell,
    /// .xsh 会话文件（绿盟 OSMS）
    Xsh,
    Putty,
    SecureCrt,
    /// WinSCP、SecureFX、Xftp：带 `/sessionname=`、`-newwin`，或者是 `sftp://`、`scp://` 链接
    WinScp,
    /// 裸的 `ssh://` 链接
    Url,
}

impl LaunchSource {
    /// 日志中的写法。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MobaXterm => "mobaxterm",
            Self::Xshell => "xshell",
            Self::Xsh => "xsh",
            Self::Putty => "putty",
            Self::SecureCrt => "securecrt",
            Self::WinScp => "winscp",
            Self::Url => "url",
        }
    }
}

/// 外部程序要打开的连接。`Debug` 不显示密码（[`SecretString`] 显示为 `***`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchTarget {
    pub kind: LaunchKind,
    pub address: String,
    pub port: u16,
    pub username: String,
    /// 堡垒机给的密码（JumpServer 是一次性的令牌）；没有时连接时询问。只在内存中，不保存、不进日志
    pub password: Option<SecretString>,
    /// Tab 标题（`-newtab`、`/N`、`/sessionname=`）
    pub name: Option<String>,
    /// .xsh 中 CodePage 对应的编码；没有时按 UTF-8
    pub encoding: Option<Encoding>,
    pub source: LaunchSource,
}

impl LaunchTarget {
    /// Tab 标题：调起时给的名称，没有时为 `用户@地址`。
    pub fn title(&self) -> String {
        match self.name.as_deref().filter(|name| !name.is_empty()) {
            Some(name) => name.to_string(),
            None => format!("{}@{}", self.username, self.address),
        }
    }

    /// 确认时显示的目标：`用户@地址:端口`（IPv6 地址加方括号）。
    pub fn display_target(&self) -> String {
        let address = if self.address.contains(':') {
            Cow::Owned(format!("[{}]", self.address))
        } else {
            Cow::Borrowed(self.address.as_str())
        };
        format!("{}@{address}:{}", self.username, self.port)
    }

    /// 写进日志的摘要：只有来源、类型、地址与端口，不含用户名与密码。
    pub fn log_summary(&self) -> String {
        format!(
            "source={} kind={} address={:?} port={}",
            self.source.as_str(),
            self.kind.as_str(),
            self.address,
            self.port
        )
    }

    /// 结构是否合法：地址 1–255 个字符，端口不为 0，
    /// 用户名不超过 255 个字符，密码与名称不超过 4096 个字符。[`parse`] 只返回合法的请求。
    pub fn is_valid(&self) -> bool {
        let len = |s: &str| s.chars().count();
        (1..=MAX_ADDRESS).contains(&len(&self.address))
            && self.port != 0
            && len(&self.username) <= MAX_USERNAME
            && self
                .password
                .as_ref()
                .is_none_or(|password| len(password.expose()) <= MAX_TEXT)
            && self
                .name
                .as_deref()
                .is_none_or(|name| len(name) <= MAX_TEXT)
    }

    /// 只用一个通道：堡垒机一般不允许在终端的连接上另开通道，给的令牌又是一次性的，
    /// 因此不共用连接、不另开通道（没有资源监控、主机工具，不能在终端旁打开 SFTP，不能复制会话）。
    pub fn single_channel(&self) -> bool {
        true
    }

    /// 对应的快速连接目标（地址、端口、用户名），据此建立连接计划。
    pub fn quick_target(&self) -> QuickTarget {
        QuickTarget {
            username: self.username.clone(),
            host: self.address.clone(),
            port: self.port,
        }
    }
}

/// 解析启动参数，认不出时返回 `None`。`args` 是程序之后的参数（可以包含应用自己的 `--` 开关，忽略），
/// `cwd` 是调起者的当前目录：.xsh 会话文件的相对路径按它解析，超过 [`MAX_XSH_SIZE`] 的不读。
pub fn parse(args: &[String], cwd: &Path) -> Option<LaunchTarget> {
    parse_with(args, |file| xsh::read_file(&cwd.join(file)))
}

/// 同 [`parse`]，.xsh 会话文件由 `read_file` 读取：参数是命令行中原样的路径，文件不存在、过大时返回 `None`。
pub fn parse_with(
    args: &[String],
    read_file: impl FnOnce(&str) -> Option<Vec<u8>>,
) -> Option<LaunchTarget> {
    parse_args(args, read_file).filter(LaunchTarget::is_valid)
}

fn parse_args(
    args: &[String],
    read_file: impl FnOnce(&str) -> Option<Vec<u8>>,
) -> Option<LaunchTarget> {
    let list: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|arg| !arg.starts_with("--"))
        .collect();
    let args = Args(&list);

    // MobaXterm：-newtab 后面是 sshpass 命令（认不出时不再试别的格式）
    let newtab = args.value_after("-newtab");
    if let Some(command) = newtab.filter(|v| v.trim_start_matches(is_space).starts_with("sshpass "))
    {
        return parse_sshpass(command);
    }
    let tab_name = match newtab {
        Some(value) if !looks_like_option(value) && !ends_with_ignore_case(value, ".xsh") => {
            clean_name(value)
        }
        _ => None,
    };

    // Xshell：-url 链接
    if let Some(url) = args.value_after("-url").filter(|url| !url.is_empty()) {
        let mut target = parse_url(url)?;
        target.name = tab_name;
        target.source = LaunchSource::Xshell;
        return Some(target);
    }
    // 绿盟 OSMS：.xsh 会话文件
    if let Some(file) = list.iter().find(|arg| ends_with_ignore_case(arg, ".xsh")) {
        let data = Zeroizing::new(read_file(file)?);
        let mut target = parse_xsh(&data)?;
        target.name = tab_name;
        return Some(target);
    }
    if args.has("-ssh") {
        return parse_putty(&args);
    }
    if args.has("/ssh2") {
        return parse_securecrt(&args);
    }

    // WinSCP、SecureFX、Xftp（-newwin）与裸链接
    let link = list.iter().find(|arg| split_scheme(arg).is_some())?;
    let mut target = parse_url(link)?;
    let session = list.iter().find(|arg| is_session_name(arg));
    target.name = session
        .and_then(|s| s.split_once('=').and_then(|(_, name)| clean_name(name)))
        .or(tab_name);
    let winscp = session.is_some() || args.has("-newwin") || target.kind == LaunchKind::Sftp;
    target.source = if winscp {
        LaunchSource::WinScp
    } else {
        LaunchSource::Url
    };
    Some(target)
}

/// 参数表（已去掉 `--` 开头的），按不分大小写的开关查找。
struct Args<'a>(&'a [&'a str]);

impl<'a> Args<'a> {
    fn position(&self, flag: &str) -> Option<usize> {
        self.0.iter().position(|arg| arg.eq_ignore_ascii_case(flag))
    }

    fn has(&self, flag: &str) -> bool {
        self.position(flag).is_some()
    }

    /// 开关（第一次出现）后面的值。
    fn value_after(&self, flag: &str) -> Option<&'a str> {
        self.0.get(self.position(flag)? + 1).copied()
    }
}

/// 解析 `ssh://`、`sftp://`、`scp://` 链接（`scp` 按 SFTP）。用户名与地址的分界：
///
/// - `ssh://`（Xshell 的规则）：整个链接中最后一个 `@`。之前是用户名与密码，用户名中可以有 `/` 与 `@`
///   （JumpServer 等堡垒机把"用户@资产"写进用户名，如 `ssh://11xx@shine.com/资产名:密码@opsall.shine.com:8013`）；
/// - `sftp://`、`scp://`（WinSCP 的规则）：地址部分（到第一个 `/`、`?`、`#` 为止）中最后一个 `@`，路径中的 `@`
///   不算；地址部分没有 `@` 时（密码中有没编码的 `/`）以整个链接中最后一个 `@` 为界。用户名中不能有 `/`。
///
/// 用户名与密码以第一个 `:` 为界，各自按 URL 规则解码。`@` 之后到 `/`、`?`、`#` 为止是地址与端口，IPv6 地址写在
/// 方括号中，路径与查询参数忽略；必须有用户名，端口默认 22。结果的来源为 [`LaunchSource::Url`]，没有名称。
pub fn parse_url(raw: &str) -> Option<LaunchTarget> {
    let (kind, rest) = split_scheme(trim(raw))?;
    // 链接中不能有换行
    if rest.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
        return None;
    }
    let at = match kind {
        LaunchKind::Terminal => rest.rfind('@'),
        LaunchKind::Sftp => {
            let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
            authority.rfind('@').or_else(|| rest.rfind('@'))
        }
    };
    let host_start = at.map_or(0, |at| at + 1);
    // 去掉路径、查询参数（WinSCP 的 sftp:// 可以带初始目录，这里不用）
    let host_end = rest[host_start..]
        .find(['/', '?', '#'])
        .map_or(rest.len(), |end| host_start + end);
    let userinfo = at.map_or("", |at| &rest[..at]);
    let host_port = &rest[host_start..host_end];
    let (username, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
    let username = decode_component(username);
    let (address, port) = split_host_port(host_port)?;
    if username.is_empty() || (kind == LaunchKind::Sftp && username.contains('/')) {
        return None;
    }
    let port = match port {
        Some(port) => parse_port(port)?,
        None => DEFAULT_PORT,
    };
    let password = decode_component(password);
    Some(LaunchTarget {
        kind,
        address: address.to_string(),
        port,
        username,
        password: (!password.is_empty()).then(|| SecretString::new(password)),
        name: None,
        encoding: None,
        source: LaunchSource::Url,
    })
}

/// 链接的协议与 `://` 之后的部分；不是 `ssh://`、`sftp://`、`scp://` 时为 `None`。
fn split_scheme(s: &str) -> Option<(LaunchKind, &str)> {
    let (scheme, rest) = s.split_once("://")?;
    let kind = if scheme.eq_ignore_ascii_case("ssh") {
        LaunchKind::Terminal
    } else if scheme.eq_ignore_ascii_case("sftp") || scheme.eq_ignore_ascii_case("scp") {
        LaunchKind::Sftp
    } else {
        return None;
    };
    Some((kind, rest))
}

/// `[IPv6]:端口` 或 `地址:端口`（端口可省略）。
fn split_host_port(s: &str) -> Option<(&str, Option<&str>)> {
    let bracketed = || {
        let (host, after) = s.strip_prefix('[')?.split_once(']')?;
        let port = match after {
            "" => None,
            _ => Some(digits(after.strip_prefix(':')?)?),
        };
        (!host.is_empty()).then_some((host, port))
    };
    let plain = || {
        let (host, port) = match s.split_once(':') {
            Some((host, port)) => (host, Some(digits(port)?)),
            None => (s, None),
        };
        (!host.is_empty()).then_some((host, port))
    };
    bracketed().or_else(plain)
}

/// 非空且只有十进制数字时原样返回。
fn digits(s: &str) -> Option<&str> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then_some(s)
}

/// 端口：十进制整数 1–65535（前后的空白忽略）。
fn parse_port(s: &str) -> Option<u16> {
    digits(trim(s))?
        .parse::<u16>()
        .ok()
        .filter(|&port| port != 0)
}

/// 按 URL 规则解码（与 JavaScript 的 decodeURIComponent 相同，`+` 不是空格）；`%` 后不是两位十六进制数，
/// 或解出的不是 UTF-8 时原样返回。
fn decode_component(s: &str) -> String {
    let hex = |b: Option<&u8>| b.and_then(|&b| (b as char).to_digit(16));
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let (Some(high), Some(low)) = (hex(bytes.get(i + 1)), hex(bytes.get(i + 2))) else {
                return s.to_string();
            };
            out.push((high * 16 + low) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// MobaXterm：`sshpass -p<密码> ssh -o StrictHostKeyChecking=no <用户>@<地址> -p<端口>`。
fn parse_sshpass(command: &str) -> Option<LaunchTarget> {
    let words = Zeroizing::new(split_words(command));
    if words.first().map(String::as_str) != Some("sshpass") {
        return None;
    }
    let mut password = None;
    let mut i = 1;
    while i < words.len() && words[i] != "ssh" {
        let word = words[i].as_str();
        if word == "-p" {
            i += 1;
            password = words.get(i).map(String::as_str);
        } else if let Some(value) = word.strip_prefix("-p") {
            password = Some(value);
        }
        i += 1;
    }
    let mut target = "";
    let mut port = Some(DEFAULT_PORT);
    i += 1;
    while i < words.len() {
        let word = words[i].as_str();
        if matches!(word, "-o" | "-l" | "-i") {
            i += 1;
        } else if word == "-p" {
            i += 1;
            port = words.get(i).and_then(|value| parse_port(value));
        } else if let Some(value) = word.strip_prefix("-p").and_then(digits) {
            port = parse_port(value);
        } else if !word.starts_with('-') {
            target = word;
        }
        i += 1;
    }
    let at = target.rfind('@').filter(|&at| at > 0)?;
    Some(ssh_target(
        &target[at + 1..],
        port?,
        &target[..at],
        password,
        LaunchSource::MobaXterm,
    ))
}

/// 把一行命令拆成参数（只处理空白与双引号，MobaXterm 的 sshpass 命令够用）。
fn split_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut rest = line;
    loop {
        rest = rest.trim_start_matches(is_space);
        if rest.is_empty() {
            return words;
        }
        if let Some(quoted) = rest.strip_prefix('"')
            && let Some((word, after)) = quoted.split_once('"')
        {
            words.push(word.to_string());
            rest = after;
            continue;
        }
        let end = rest.find(is_space).unwrap_or(rest.len());
        words.push(rest[..end].to_string());
        rest = &rest[end..];
    }
}

/// PuTTY：`-ssh 用户@地址 -P 端口 -pw 密码`（或 `-l 用户`）。
fn parse_putty(args: &Args<'_>) -> Option<LaunchTarget> {
    const WITH_VALUE: [&str; 5] = ["-p", "-pw", "-l", "-i", "-load"];
    let list = args.0;
    let mut target = "";
    let mut i = 0;
    while i < list.len() {
        let arg = list[i];
        if WITH_VALUE.iter().any(|flag| arg.eq_ignore_ascii_case(flag)) {
            i += 1;
        } else if !arg.starts_with('-') && (arg.contains('@') || !target.contains('@')) {
            // 地址优先取“用户@地址”的写法
            target = arg;
        }
        i += 1;
    }
    let (username, address) = match target.rfind('@') {
        Some(at) if at > 0 => (&target[..at], &target[at + 1..]),
        at => (
            args.value_after("-l").unwrap_or(""),
            &target[at.map_or(0, |at| at + 1)..],
        ),
    };
    // PuTTY 的端口是大写的 -P，与 -pw 区分
    let port = match list.iter().position(|arg| *arg == "-P") {
        Some(i) => parse_port(list.get(i + 1)?)?,
        None => DEFAULT_PORT,
    };
    if address.is_empty() || username.is_empty() {
        return None;
    }
    Some(ssh_target(
        address,
        port,
        username,
        args.value_after("-pw"),
        LaunchSource::Putty,
    ))
}

/// SecureCRT：`/N 名称 /T /SSH2 /ACCEPTHOSTKEYS /P 端口 /PASSWORD 密码 /L 用户 地址`。
fn parse_securecrt(args: &Args<'_>) -> Option<LaunchTarget> {
    const WITH_VALUE: [&str; 6] = ["/n", "/p", "/password", "/l", "/i", "/arg"];
    let list = args.0;
    let mut address = "";
    let mut i = 0;
    while i < list.len() {
        let arg = list[i];
        if WITH_VALUE.iter().any(|flag| arg.eq_ignore_ascii_case(flag)) {
            i += 1;
        } else if !arg.starts_with('/') && !arg.starts_with('-') {
            address = arg;
        }
        i += 1;
    }
    let username = args.value_after("/l").unwrap_or("");
    let port = match args.value_after("/p") {
        Some(port) => parse_port(port)?,
        None => DEFAULT_PORT,
    };
    if address.is_empty() || username.is_empty() {
        return None;
    }
    let mut target = ssh_target(
        address,
        port,
        username,
        args.value_after("/password"),
        LaunchSource::SecureCrt,
    );
    target.name = args.value_after("/n").and_then(clean_name);
    Some(target)
}

/// SSH 终端的连接请求；空密码按没有密码。
fn ssh_target(
    address: &str,
    port: u16,
    username: &str,
    password: Option<&str>,
    source: LaunchSource,
) -> LaunchTarget {
    LaunchTarget {
        kind: LaunchKind::Terminal,
        address: address.to_string(),
        port,
        username: username.to_string(),
        password: password
            .filter(|password| !password.is_empty())
            .map(SecretString::new),
        name: None,
        encoding: None,
        source,
    }
}

/// `-newtab` 的值像开关或链接时不是名称。
fn looks_like_option(s: &str) -> bool {
    s.is_empty()
        || s.starts_with('-')
        || s.starts_with('/')
        || s.split_once("://").is_some_and(|(scheme, _)| {
            !scheme.is_empty() && scheme.bytes().all(|b| b.is_ascii_alphabetic())
        })
}

/// `/sessionname=名称` 或 `-sessionname=名称`。
fn is_session_name(arg: &str) -> bool {
    const FLAG: &str = "sessionname=";
    (arg.starts_with('/') || arg.starts_with('-'))
        && arg
            .get(1..=FLAG.len())
            .is_some_and(|flag| flag.eq_ignore_ascii_case(FLAG))
}

/// 名称：去掉前后的空白与双引号；为空时为 `None`。
fn clean_name(s: &str) -> Option<String> {
    let s = trim(s);
    let s = s.strip_prefix('"').unwrap_or(s);
    let s = s.strip_suffix('"').unwrap_or(s);
    Some(trim(s).to_string()).filter(|name| !name.is_empty())
}

fn ends_with_ignore_case(s: &str, suffix: &str) -> bool {
    s.len() >= suffix.len()
        && s.as_bytes()[s.len() - suffix.len()..].eq_ignore_ascii_case(suffix.as_bytes())
}

/// 空白（与 JavaScript 的 `trim` 相同，也包括 BOM）。
fn is_space(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

fn trim(s: &str) -> &str {
    s.trim_matches(is_space)
}
