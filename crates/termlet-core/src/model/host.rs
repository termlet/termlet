//! 主机。

use serde::{Deserialize, Serialize};

use super::{Extra, RemoteOs, no_control_chars, trimmed, valid_os_key};
use crate::Invalid;
use crate::id::{HostId, KeyId, ProfileId};

/// 一台保存的主机。所属分组与排序在条目行上（`parent_id`、`sort_key`），不在这里。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Host {
    /// 显示名称；留空时保存为地址
    pub name: String,
    pub protocol: Protocol,
    /// 地址（域名、IPv4、IPv6）；串口为端口名（`COM3`、`/dev/ttyUSB0`）
    pub address: String,
    /// 端口；0 表示协议的默认端口（SSH 22、Telnet 23），串口不用
    pub port: u16,
    /// 用户名；留空时沿用分组的默认用户名
    pub username: String,
    pub auth: Auth,
    pub jump: Jump,
    /// 代理（经跳板机连接时不使用）；`None` 时沿用分组默认的代理命令（[`super::GroupDefaults::proxy_command`]），
    /// 分组没有设置或 [`Host::no_proxy`] 为真时不使用代理
    pub proxy: Option<Proxy>,
    /// 不使用代理，也不沿用分组默认的代理命令（`proxy` 为 `None` 时才有意义；与跳板机的"直连"相当）
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub no_proxy: bool,
    pub favorite: bool,
    pub tags: Vec<String>,
    pub note: String,
    pub terminal: TerminalOptions,
    pub ssh: SshOptions,
    /// Telnet、串口的回车、回显、退格
    pub line: LineOptions,
    pub telnet: TelnetOptions,
    pub serial: SerialOptions,
    pub cli: CliAccess,
    /// SSH 连接后识别出的系统（[`RemoteOs`] 的键，见 [`crate::osprobe`]）；没有识别过为 `None`。不是用户的设置：
    /// 由 [`crate::store::Store::set_host_os`] 写入（不改修改时间），编辑主机时保留原值；同步时单独成一条记录
    /// （`detected/host/<ID>`），不算对主机的修改。这个版本不认识的键（更新的版本加的）原样保留，当作没有识别
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Ssh,
    Telnet,
    Serial,
}

impl Protocol {
    pub fn default_port(self) -> u16 {
        match self {
            Protocol::Ssh => 22,
            Protocol::Telnet => 23,
            Protocol::Serial => 0,
        }
    }
}

/// 登录方式。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Auth {
    /// 密码：保存了就用保存的（秘密 `password`），没保存时连接时询问
    #[default]
    Password,
    /// 私钥；私钥密码在秘密 `passphrase` 中（密钥库中的密钥不需要）
    #[serde(rename_all = "camelCase")]
    Key { source: KeySource },
    /// ssh-agent / Pageant
    Agent,
    /// 凭据档案：用户名与认证由档案提供（主机自己的用户名优先）
    #[serde(rename_all = "camelCase")]
    Profile { profile: ProfileId },
    /// 沿用分组默认的凭据档案；没有时同"密码"
    Inherit,
}

/// 私钥从哪里来。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "from", rename_all = "camelCase")]
pub enum KeySource {
    /// 密钥库中的密钥（多台主机可共用）
    #[serde(rename_all = "camelCase")]
    Library { key: KeyId },
    /// 本机的私钥文件（只存路径，不读进数据库）。`certificate` 是 OpenSSH 证书文件（OpenSSH 的 `CertificateFile`）；
    /// 留空时与 OpenSSH 相同，自动找私钥文件旁同名的 `-cert.pub`（见 [`crate::sshcert`]）
    #[serde(rename_all = "camelCase")]
    File {
        path: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        certificate: String,
    },
    /// 粘贴的私钥（存在秘密 `private_key` 中）。`certificate` 是附带的 OpenSSH 证书（一行，与密钥库中的密钥相同；
    /// 证书不是秘密，与主机一起保存、同步）；空为没有
    #[serde(rename_all = "camelCase")]
    Pasted {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        certificate: String,
    },
}

impl KeySource {
    /// 本机的私钥文件，不指定证书文件（自动找同名的 `-cert.pub`）。
    pub fn file(path: impl Into<String>) -> Self {
        KeySource::File {
            path: path.into(),
            certificate: String::new(),
        }
    }

    /// 粘贴的私钥，不附带证书。
    pub fn pasted() -> Self {
        KeySource::Pasted {
            certificate: String::new(),
        }
    }
}

/// 跳板机。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum Jump {
    /// 沿用分组默认的跳板机；没有时直连
    #[default]
    Inherit,
    /// 直连，不沿用分组的跳板机
    Direct,
    /// 经另一台主机（它自己的跳板机继续向上，最多 5 层）
    #[serde(rename_all = "camelCase")]
    Via { host: HostId },
}

/// 最多经过几层跳板机。
pub const MAX_JUMP_DEPTH: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Proxy {
    pub kind: ProxyKind,
    pub address: String,
    pub port: u16,
    /// 用户名；密码在秘密 `proxy_password` 中
    pub username: String,
    /// 代理命令（`kind` 为 command 时），与 OpenSSH 的 ProxyCommand 相同：占位符 `%h` 主机、`%p` 端口、
    /// `%r` 用户名、`%n` 主机的名称、`%%` 百分号（启动方式见 [`crate::ssh::proxy_command`]）
    pub command: String,
}

impl Default for Proxy {
    fn default() -> Self {
        Self {
            kind: ProxyKind::Socks5,
            address: String::new(),
            port: 1080,
            username: String::new(),
            command: String::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyKind {
    #[default]
    Socks5,
    Socks4,
    Http,
    Https,
    Command,
}

/// 服务器或设备使用的编码。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Encoding {
    #[default]
    #[serde(rename = "utf-8")]
    Utf8,
    /// 兼容 GBK、GB2312，老的网络设备常用
    #[serde(rename = "gb18030")]
    Gb18030,
    #[serde(rename = "big5")]
    Big5,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TerminalOptions {
    pub encoding: Encoding,
    /// 登录后自动执行的命令（SSH，每行一条）：出现提示符后发送，重连到新的 shell 后再发送一次
    pub startup: String,
    /// 终端连到各自的 tmux 会话，重连后回到原来的现场（SSH）
    pub tmux: bool,
    /// 固定的终端配色（配色 ID）；留空沿用分组的，再没有时用全局设置
    pub color_scheme: String,
    /// 连接后自动开始会话录制（见 [`crate::recording`]）
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub record: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SshOptions {
    /// 兼容旧设备：在默认算法之后追加 aes-cbc、3des-cbc、diffie-hellman-group1-sha1 等旧算法
    pub legacy_algorithms: bool,
    /// 终端转发本机的 ssh-agent
    pub agent_forward: bool,
    /// 只用一个通道（堡垒机）：不在终端的连接上另开通道（监控、主机工具、SFTP、复制会话都不共用这条连接）
    pub single_channel: bool,
    /// 服务器上文件名的编码（SFTP）：GBK、Big5 等时列表、传输、编辑、改名、删除都按它转换
    pub sftp_encoding: Encoding,
    /// 终端转发 X11：服务器上的图形程序显示在本机的 X 服务器上（见 [`crate::ssh::x11`]）
    pub x11_forward: bool,
    /// X11 按受信任方式转发（OpenSSH 的 `ForwardX11Trusted yes`、`ssh -Y`）：服务器上的程序可以访问本机 X 服务器上的
    /// 全部窗口与输入；默认不受信任（`ssh -X`），由本机的 xauth 生成受限的 cookie
    pub x11_trusted: bool,
    /// 不认识的字段（更新的版本加的）：原样保留，写回时不丢
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Enter {
    /// CR LF（Telnet 默认，与 PuTTY 的 Telnet New Line 相同）
    #[default]
    Crlf,
    /// CR（串口默认）
    Cr,
    Lf,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LocalEcho {
    /// 服务器不回显时在本地显示输入（Telnet 默认）
    #[default]
    Auto,
    On,
    /// 串口默认
    Off,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backspace {
    /// 0x7F
    #[default]
    Del,
    /// 0x08（部分老设备只认 Ctrl+H）
    Bs,
}

/// Telnet、串口的行规程；`None` 表示用协议的默认值。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LineOptions {
    pub enter: Option<Enter>,
    pub local_echo: Option<LocalEcho>,
    pub backspace: Backspace,
}

impl LineOptions {
    pub fn enter_for(&self, protocol: Protocol) -> Enter {
        self.enter.unwrap_or(match protocol {
            Protocol::Serial => Enter::Cr,
            _ => Enter::Crlf,
        })
    }

    pub fn local_echo_for(&self, protocol: Protocol) -> LocalEcho {
        self.local_echo.unwrap_or(match protocol {
            Protocol::Serial => LocalEcho::Off,
            _ => LocalEcho::Auto,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TelnetOptions {
    /// 协商的终端类型
    pub term_type: String,
}

impl Default for TelnetOptions {
    fn default() -> Self {
        Self {
            term_type: "xterm-256color".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SerialOptions {
    /// 50–20000000
    pub baud: u32,
    /// 5–8
    pub data_bits: u8,
    pub parity: Parity,
    pub stop_bits: StopBits,
    pub flow_control: FlowControl,
    /// 设备的 USB 标识：端口号变了（换了 USB 口）时据此找回端口
    pub usb: Option<UsbId>,
}

impl Default for SerialOptions {
    fn default() -> Self {
        Self {
            baud: 115_200,
            data_bits: 8,
            parity: Parity::None,
            stop_bits: StopBits::One,
            flow_control: FlowControl::None,
            usb: None,
        }
    }
}

/// 波特率的范围。
pub const SERIAL_BAUD_RANGE: std::ops::RangeInclusive<u32> = 50..=20_000_000;
/// 串口名的最大长度。
const MAX_SERIAL_PORT: usize = 128;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Parity {
    #[default]
    None,
    Odd,
    Even,
    Mark,
    Space,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopBits {
    #[default]
    #[serde(rename = "1")]
    One,
    #[serde(rename = "1.5")]
    OnePointFive,
    #[serde(rename = "2")]
    Two,
}

/// 流控。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FlowControl {
    /// 不流控（DTR、RTS 保持有效），大多数设备的控制台口
    #[default]
    None,
    /// 硬件流控（RTS / CTS）
    RtsCts,
    /// 软件流控（XON / XOFF）
    XonXoff,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UsbId {
    pub vid: String,
    pub pid: String,
    pub serial_number: String,
}

/// 命令行工具（`termlet`）的访问权限。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CliAccess {
    /// 允许命令行访问（默认不允许）
    pub allowed: bool,
    /// CLI 使用单独的凭据（推荐服务器上的低权限账号）；`None` 表示用主机自己的
    pub credential: Option<CliCredential>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CliCredential {
    /// 用户名；留空用主机的
    pub username: String,
    /// 认证（秘密在 `cli_password`、`cli_private_key`、`cli_passphrase` 中）
    pub auth: CliAuth,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum CliAuth {
    #[default]
    Password,
    #[serde(rename_all = "camelCase")]
    Key {
        source: KeySource,
    },
    Agent,
}

const MAX_NAME: usize = 128;
const MAX_NOTE: usize = 2000;
const MAX_TAG: usize = 32;
const MAX_TAGS: usize = 20;
const MAX_STARTUP_LINE: usize = 1000;
const MAX_STARTUP_LINES: usize = 20;
/// 代理命令的最大长度（字符）
pub(crate) const MAX_PROXY_COMMAND: usize = 2000;

impl Host {
    /// 实际使用的端口。
    pub fn effective_port(&self) -> u16 {
        if self.port == 0 {
            self.protocol.default_port()
        } else {
            self.port
        }
    }

    pub fn is_ssh(&self) -> bool {
        self.protocol == Protocol::Ssh
    }

    /// 显示用的名称：没填名称时（对话框中还没保存的主机）用地址，与保存时的规则相同。
    pub fn display_name(&self) -> &str {
        match self.name.trim() {
            "" => self.address.trim(),
            name => name,
        }
    }

    /// 识别出的系统；没有识别过、键不认识时为 `None`。只有 SSH 主机识别（Telnet、串口不识别）。
    pub fn remote_os(&self) -> Option<RemoteOs> {
        self.os.as_deref().and_then(RemoteOs::from_key)
    }

    /// 是否匹配搜索关键词（名称、地址、用户名、标签，不区分大小写）。
    pub fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        [&self.name, &self.address, &self.username]
            .into_iter()
            .chain(self.tags.iter())
            .any(|field| field.to_lowercase().contains(&query))
    }

    pub(crate) fn normalize(&mut self) -> Result<(), Invalid> {
        trimmed(&mut self.name, "name", MAX_NAME)?;
        trimmed(&mut self.address, "address", 255)?;
        trimmed(&mut self.username, "username", 128)?;
        trimmed(&mut self.note, "note", MAX_NOTE)?;
        match self.protocol {
            Protocol::Ssh | Protocol::Telnet => {
                if self.address.is_empty() || self.address.contains([' ', '/', '\\']) {
                    return Err(Invalid::new("address", "invalid"));
                }
            }
            Protocol::Serial => {
                let valid = !self.address.is_empty()
                    && self.address.len() <= MAX_SERIAL_PORT
                    && self.address.chars().all(|c| {
                        c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '\\' | ':' | '-')
                    });
                if !valid {
                    return Err(Invalid::new("address", "invalid_serial_port"));
                }
                self.port = 0;
            }
        }
        no_control_chars(&self.username, "username")?;
        if self.name.is_empty() {
            self.name = self.address.clone();
        }
        self.drop_ssh_only();
        self.normalize_tags()?;
        self.terminal.startup = normalize_startup(&self.terminal.startup)?;
        if let Some(proxy) = &mut self.proxy {
            trimmed(&mut proxy.address, "proxy.address", 255)?;
            trimmed(&mut proxy.username, "proxy.username", 128)?;
            trimmed(&mut proxy.command, "proxy.command", MAX_PROXY_COMMAND)?;
            no_control_chars(&proxy.command, "proxy.command")?;
            match proxy.kind {
                ProxyKind::Command if proxy.command.is_empty() => {
                    return Err(Invalid::new("proxy.command", "required"));
                }
                ProxyKind::Command => {}
                _ if proxy.address.is_empty() => {
                    return Err(Invalid::new("proxy.address", "required"));
                }
                _ if proxy.port == 0 => return Err(Invalid::new("proxy.port", "invalid")),
                _ => {}
            }
            // 有自己的代理时不沿用分组的，"不使用代理"没有意义
            self.no_proxy = false;
        }
        let term_type = &mut self.telnet.term_type;
        trimmed(term_type, "telnet.termType", 40)?;
        if term_type.is_empty() {
            *term_type = TelnetOptions::default().term_type;
        }
        if !term_type
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
        {
            return Err(Invalid::new("telnet.termType", "invalid"));
        }
        let serial = &mut self.serial;
        if serial.baud == 0 {
            serial.baud = SerialOptions::default().baud;
        }
        if !SERIAL_BAUD_RANGE.contains(&serial.baud) {
            return Err(Invalid::new("serial.baud", "invalid"));
        }
        if !(5..=8).contains(&serial.data_bits) {
            return Err(Invalid::new("serial.dataBits", "invalid"));
        }
        if let Some(usb) = &serial.usb {
            let valid = |s: &str| {
                s.len() <= 64
                    && s.chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            };
            if !valid(&usb.vid) || !valid(&usb.pid) || !valid(&usb.serial_number) {
                return Err(Invalid::new("serial.usb", "invalid"));
            }
        }
        if let Some(credential) = &mut self.cli.credential {
            trimmed(&mut credential.username, "cli.username", 128)?;
        }
        // 识别结果不是用户输入的：格式不对（数据被改坏）时当作没有识别，不报错
        if self.os.as_deref().is_some_and(|key| !valid_os_key(key)) {
            self.os = None;
        }
        Ok(())
    }

    /// Telnet、串口只保留用得到的设置：去掉 SSH 专有的（tmux、登录后执行的命令、`ssh` 中的选项、
    /// 命令行工具的访问）；认证只用密码（Telnet 看到登录提示时自动填写，可以不保存）。
    /// 串口另外没有用户名、代理与跳板机。
    fn drop_ssh_only(&mut self) {
        if self.is_ssh() {
            return;
        }
        self.auth = Auth::Password;
        self.terminal.tmux = false;
        self.terminal.startup.clear();
        self.ssh = SshOptions::default();
        self.cli = CliAccess::default();
        if self.protocol == Protocol::Serial {
            self.username.clear();
            self.proxy = None;
            self.no_proxy = false;
            self.jump = Jump::Direct;
        }
    }

    fn normalize_tags(&mut self) -> Result<(), Invalid> {
        let mut tags: Vec<String> = Vec::new();
        for tag in &self.tags {
            let tag = tag.trim();
            if tag.is_empty() {
                continue;
            }
            if tag.chars().count() > MAX_TAG {
                return Err(Invalid::new("tags", "too_long"));
            }
            if !tags.iter().any(|t| t.eq_ignore_ascii_case(tag)) {
                tags.push(tag.to_string());
            }
        }
        if tags.len() > MAX_TAGS {
            return Err(Invalid::new("tags", "too_many"));
        }
        self.tags = tags;
        Ok(())
    }

    /// 登录后自动执行的命令（去掉空行）。
    pub fn startup_commands(&self) -> impl Iterator<Item = &str> {
        self.terminal
            .startup
            .lines()
            .filter(|line| !line.trim().is_empty())
    }
}

/// 登录后执行的命令：去掉行尾空白与首尾空行，限制行数与每行长度，不允许控制字符。
fn normalize_startup(text: &str) -> Result<String, Invalid> {
    let normalized = text.replace("\r\n", "\n");
    let mut lines = Vec::new();
    for line in normalized.split('\n') {
        let line = line.trim_end_matches([' ', '\t', '\r']);
        if line.chars().count() > MAX_STARTUP_LINE {
            return Err(Invalid::new("terminal.startup", "line_too_long"));
        }
        no_control_chars(line, "terminal.startup")?;
        lines.push(line);
    }
    let joined = lines.join("\n");
    let text = joined.trim_matches('\n').to_string();
    if !text.is_empty() && text.lines().count() > MAX_STARTUP_LINES {
        return Err(Invalid::new("terminal.startup", "too_many_lines"));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ssh(address: &str) -> Host {
        Host {
            address: address.into(),
            ..Default::default()
        }
    }

    #[test]
    fn name_defaults_to_address_and_port_to_protocol() {
        let mut host = ssh(" 10.0.0.1 ");
        host.normalize().unwrap();
        assert_eq!(host.name, "10.0.0.1");
        assert_eq!(host.effective_port(), 22);
        host.protocol = Protocol::Telnet;
        assert_eq!(host.effective_port(), 23);
    }

    #[test]
    fn invalid_addresses_are_rejected() {
        for bad in ["", "a b", "a/b", "a\\b"] {
            assert_eq!(
                ssh(bad).normalize().unwrap_err(),
                Invalid::new("address", "invalid")
            );
        }
        let mut serial = Host {
            protocol: Protocol::Serial,
            address: r"\\.\COM10".into(),
            port: 22,
            ..Default::default()
        };
        serial.normalize().unwrap();
        assert_eq!(serial.port, 0);
    }

    #[test]
    fn tags_are_trimmed_and_deduplicated() {
        let mut host = ssh("h");
        host.tags = vec![" prod ".into(), "PROD".into(), "".into(), "db".into()];
        host.normalize().unwrap();
        assert_eq!(host.tags, vec!["prod", "db"]);
    }

    #[test]
    fn startup_commands_are_cleaned() {
        let mut host = ssh("h");
        host.terminal.startup = "\n cd /srv  \r\n\nls\t\n\n".into();
        host.normalize().unwrap();
        assert_eq!(host.terminal.startup, " cd /srv\n\nls");
        assert_eq!(
            host.startup_commands().collect::<Vec<_>>(),
            vec![" cd /srv", "ls"]
        );
        host.terminal.startup = "echo \u{1b}[31m".into();
        assert!(host.normalize().is_err());
    }

    #[test]
    fn json_is_camel_case_and_keeps_unknown_fields() {
        let json = r#"{"name":"web","address":"w","auth":{"type":"key","source":{"from":"library","key":"k_1"}},
                       "jump":{"mode":"via","host":"h_2"},"futureOption":{"x":1}}"#;
        let host: Host = serde_json::from_str(json).unwrap();
        assert_eq!(
            host.auth,
            Auth::Key {
                source: KeySource::Library {
                    key: KeyId::from_raw("k_1")
                }
            }
        );
        assert_eq!(
            host.jump,
            Jump::Via {
                host: HostId::from_raw("h_2")
            }
        );
        let back = serde_json::to_value(&host).unwrap();
        assert_eq!(back["futureOption"]["x"], 1);
        assert_eq!(back["terminal"]["encoding"], "utf-8");
        assert!(back.get("legacy_algorithms").is_none());
        assert_eq!(back["ssh"]["legacyAlgorithms"], false);
    }

    // 粘贴的私钥：以前保存的（没有证书）照样读，没有证书时写出的与以前相同；附带的证书往返不变
    #[test]
    fn pasted_key_certificate_is_optional() {
        let old: KeySource = serde_json::from_str(r#"{"from":"pasted"}"#).unwrap();
        assert_eq!(old, KeySource::pasted());
        assert_eq!(serde_json::to_string(&old).unwrap(), r#"{"from":"pasted"}"#);
        let with = KeySource::Pasted {
            certificate: "ssh-ed25519-cert-v01@openssh.com AAAA".into(),
        };
        let json = serde_json::to_string(&with).unwrap();
        assert!(
            json.contains(r#""certificate":"ssh-ed25519-cert"#),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<KeySource>(&json).unwrap(), with);
    }

    // SFTP 文件名的编码：往返不变；没有这个字段的数据（之前保存的、同步来的）读成 UTF-8
    #[test]
    fn sftp_encoding_round_trips_and_defaults_to_utf8() {
        let host = Host {
            address: "old-server".into(),
            ssh: SshOptions {
                sftp_encoding: Encoding::Gb18030,
                ..Default::default()
            },
            ..Default::default()
        };
        let json = serde_json::to_value(&host).unwrap();
        assert_eq!(json["ssh"]["sftpEncoding"], "gb18030");
        let back: Host = serde_json::from_value(json).unwrap();
        assert_eq!(back, host);
        let old: Host =
            serde_json::from_str(r#"{"address":"w","ssh":{"singleChannel":true}}"#).unwrap();
        assert_eq!(old.ssh.sftp_encoding, Encoding::Utf8);
        assert!(old.ssh.single_channel);
        assert!(
            serde_json::from_str::<Host>(r#"{"address":"a","ssh":{"sftpEncoding":"latin1"}}"#)
                .is_err()
        );
    }

    // SSH 选项中不认识的字段（更新的版本加的）读入再写出后还在，认识的字段照常读写
    #[test]
    fn ssh_options_keep_unknown_fields() {
        let json = r#"{"address":"w","ssh":{"agentForward":true,"sftpEncoding":"big5",
                       "futureField":{"x":[1,2]},"futureFlag":true}}"#;
        let host: Host = serde_json::from_str(json).unwrap();
        assert!(host.ssh.agent_forward);
        assert_eq!(host.ssh.sftp_encoding, Encoding::Big5);
        assert_eq!(host.ssh.extra.len(), 2);
        let back = serde_json::to_value(&host).unwrap();
        assert_eq!(back["ssh"]["futureField"]["x"][1], 2);
        assert_eq!(back["ssh"]["futureFlag"], true);
        assert_eq!(back["ssh"]["sftpEncoding"], "big5");
        assert!(back.get("futureField").is_none(), "留在 ssh 中，不跑到顶层");
        let again: Host = serde_json::from_value(back).unwrap();
        assert_eq!(again, host);
    }

    // 识别出的系统：没有时不写出这个字段（之前保存的主机内容不变）；不认识的键原样保留、当作没有识别；
    // 格式不对的在保存前去掉
    #[test]
    fn detected_os_round_trips_and_unknown_keys_are_kept() {
        let host: Host = serde_json::from_str(r#"{"address":"w"}"#).unwrap();
        assert_eq!(host.os, None);
        assert!(serde_json::to_value(&host).unwrap().get("os").is_none());

        let host: Host = serde_json::from_str(r#"{"address":"w","os":"alpine"}"#).unwrap();
        assert_eq!(host.remote_os(), Some(RemoteOs::Alpine));
        assert_eq!(serde_json::to_value(&host).unwrap()["os"], "alpine");

        let mut newer: Host = serde_json::from_str(r#"{"address":"w","os":"nixos"}"#).unwrap();
        assert_eq!(newer.remote_os(), None);
        newer.normalize().unwrap();
        assert_eq!(newer.os.as_deref(), Some("nixos"));
        assert!(newer.extra.is_empty());

        for bad in ["Ubuntu", "a b", ""] {
            let mut host = Host {
                address: "w".into(),
                os: Some(bad.into()),
                ..Default::default()
            };
            host.normalize().unwrap();
            assert_eq!(host.os, None, "{bad}");
        }
        // 不是字符串时读取失败（与其他字段相同）
        assert!(serde_json::from_str::<Host>(r#"{"address":"w","os":1}"#).is_err());
    }

    // 代理与 X11 的设置：自己有代理时"不使用代理"没有意义；没有设置时不写出 noProxy（之前保存的主机内容不变）；
    // 代理命令不能有换行
    #[test]
    fn proxy_and_x11_options() {
        let mut host = ssh("h");
        host.no_proxy = true;
        host.ssh.x11_forward = true;
        host.ssh.x11_trusted = true;
        host.normalize().unwrap();
        let json = serde_json::to_value(&host).unwrap();
        assert_eq!(json["noProxy"], true);
        assert_eq!(json["ssh"]["x11Forward"], true);
        assert_eq!(json["ssh"]["x11Trusted"], true);
        assert_eq!(serde_json::from_value::<Host>(json).unwrap(), host);

        host.proxy = Some(Proxy {
            kind: ProxyKind::Command,
            command: " nc %h %p ".into(),
            ..Default::default()
        });
        host.normalize().unwrap();
        assert!(!host.no_proxy);
        assert_eq!(host.proxy.as_ref().unwrap().command, "nc %h %p");
        assert!(
            serde_json::to_value(&host)
                .unwrap()
                .get("noProxy")
                .is_none()
        );

        host.proxy.as_mut().unwrap().command = "nc %h\n%p".into();
        assert_eq!(
            host.normalize().unwrap_err(),
            Invalid::new("proxy.command", "control_characters")
        );
        let old: Host =
            serde_json::from_str(r#"{"address":"w","ssh":{"agentForward":true}}"#).unwrap();
        assert!(!old.no_proxy && !old.ssh.x11_forward && !old.ssh.x11_trusted);
    }

    #[test]
    fn matches_name_address_user_and_tags() {
        let host = Host {
            name: "Web".into(),
            address: "10.1.1.1".into(),
            username: "deploy".into(),
            tags: vec!["Prod".into()],
            ..Default::default()
        };
        assert!(host.matches("web"));
        assert!(host.matches("10.1"));
        assert!(host.matches("DEP"));
        assert!(host.matches("prod"));
        assert!(!host.matches("db"));
    }

    // 默认端口 23、行规程取 Telnet 的默认值；可以经跳板机，只用密码
    #[test]
    fn telnet_host_defaults_and_ssh_only_settings() {
        let mut host = Host {
            protocol: Protocol::Telnet,
            address: "10.0.0.2".into(),
            username: "admin".into(),
            auth: Auth::Agent,
            jump: Jump::Via {
                host: HostId::from_raw("h_jump"),
            },
            terminal: TerminalOptions {
                encoding: Encoding::Gb18030,
                startup: "ls".into(),
                tmux: true,
                ..Default::default()
            },
            ssh: SshOptions {
                agent_forward: true,
                ..Default::default()
            },
            cli: CliAccess {
                allowed: true,
                credential: Some(CliCredential::default()),
            },
            ..Default::default()
        };
        host.normalize().unwrap();
        assert_eq!(host.name, "10.0.0.2");
        assert_eq!(host.effective_port(), 23);
        assert_eq!(host.line.enter_for(host.protocol), Enter::Crlf);
        assert_eq!(host.line.local_echo_for(host.protocol), LocalEcho::Auto);
        assert_eq!(host.line.backspace, Backspace::Del);
        assert_eq!(host.telnet.term_type, "xterm-256color");
        assert_eq!(host.terminal.encoding, Encoding::Gb18030);
        // 跳板机与用户名保留（自动登录用）；SSH 专有的去掉，认证只能是密码
        assert!(matches!(host.jump, Jump::Via { .. }));
        assert_eq!(host.username, "admin");
        assert_eq!(host.auth, Auth::Password);
        assert!(!host.terminal.tmux && host.terminal.startup.is_empty());
        assert_eq!(host.ssh, SshOptions::default());
        assert_eq!(host.cli, CliAccess::default());

        host.telnet.term_type = "bad term".into();
        assert_eq!(
            host.normalize().unwrap_err(),
            Invalid::new("telnet.termType", "invalid")
        );
        // 不认识的取值在读取时就被拒绝
        for bad in [
            r#"{"address":"a","protocol":"rdp"}"#,
            r#"{"address":"a","terminal":{"encoding":"latin1"}}"#,
            r#"{"address":"a","line":{"enter":"crcr"}}"#,
            r#"{"address":"a","line":{"localEcho":"maybe"}}"#,
            r#"{"address":"a","line":{"backspace":"^H"}}"#,
        ] {
            assert!(serde_json::from_str::<Host>(bad).is_err(), "{bad}");
        }
    }

    // 串口没有端口号、用户名、代理与跳板机，参数补全默认值
    #[test]
    fn serial_host_defaults_and_validation() {
        let mut host = Host {
            name: "console".into(),
            protocol: Protocol::Serial,
            address: "COM3".into(),
            port: 22,
            username: "ignored".into(),
            proxy: Some(Proxy {
                address: "p".into(),
                ..Default::default()
            }),
            jump: Jump::Inherit,
            serial: SerialOptions {
                baud: 0,
                usb: Some(UsbId {
                    vid: "1A86".into(),
                    pid: "7523".into(),
                    serial_number: String::new(),
                }),
                ..Default::default()
            },
            terminal: TerminalOptions {
                tmux: true,
                ..Default::default()
            },
            ..Default::default()
        };
        host.normalize().unwrap();
        assert_eq!(host.port, 0);
        assert!(host.username.is_empty() && host.proxy.is_none());
        assert_eq!(host.jump, Jump::Direct);
        assert!(!host.terminal.tmux);
        assert_eq!(host.line.enter_for(host.protocol), Enter::Cr);
        assert_eq!(host.line.local_echo_for(host.protocol), LocalEcho::Off);
        assert_eq!(host.serial.baud, 115_200);
        assert_eq!(host.serial.data_bits, 8);
        assert_eq!(host.serial.parity, Parity::None);
        assert_eq!(host.serial.stop_bits, StopBits::One);
        assert_eq!(host.serial.flow_control, FlowControl::None);

        let serial = |address: &str, options: SerialOptions| Host {
            protocol: Protocol::Serial,
            address: address.into(),
            serial: options,
            ..Default::default()
        };
        let mut ok = serial(r"\\.\COM12", SerialOptions::default());
        ok.normalize().unwrap();
        let mut ok = serial("/dev/ttyUSB0", SerialOptions::default());
        ok.normalize().unwrap();
        let long = "C".repeat(129);
        for (address, options, field) in [
            ("", SerialOptions::default(), "address"),
            ("COM3; rm", SerialOptions::default(), "address"),
            (long.as_str(), SerialOptions::default(), "address"),
            (
                "COM3",
                SerialOptions {
                    baud: 10,
                    ..Default::default()
                },
                "serial.baud",
            ),
            (
                "COM3",
                SerialOptions {
                    baud: 20_000_001,
                    ..Default::default()
                },
                "serial.baud",
            ),
            (
                "COM3",
                SerialOptions {
                    data_bits: 9,
                    ..Default::default()
                },
                "serial.dataBits",
            ),
            (
                "COM3",
                SerialOptions {
                    usb: Some(UsbId {
                        vid: "1A 86".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                "serial.usb",
            ),
        ] {
            let err = serial(address, options).normalize().unwrap_err();
            assert_eq!(err.field, field, "{address}");
        }
        for bad in [
            r#"{"protocol":"serial","address":"COM3","serial":{"parity":"x"}}"#,
            r#"{"protocol":"serial","address":"COM3","serial":{"stopBits":"3"}}"#,
            r#"{"protocol":"serial","address":"COM3","serial":{"flowControl":"dtr"}}"#,
        ] {
            assert!(serde_json::from_str::<Host>(bad).is_err(), "{bad}");
        }
        let json = serde_json::to_value(serial(
            "COM3",
            SerialOptions {
                flow_control: FlowControl::RtsCts,
                stop_bits: StopBits::OnePointFive,
                ..Default::default()
            },
        ))
        .unwrap();
        assert_eq!(json["serial"]["flowControl"], "rtscts");
        assert_eq!(json["serial"]["stopBits"], "1.5");
        // 旧数据没有流控：默认不流控
        let old: Host = serde_json::from_str(
            r#"{"protocol":"serial","address":"COM3","serial":{"baud":9600}}"#,
        )
        .unwrap();
        assert_eq!(old.serial.flow_control, FlowControl::None);
        assert_eq!(old.serial.data_bits, 8);
    }
}
