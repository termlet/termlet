//! MobaXterm 的书签会话。
//!
//! 书签的格式（MobaXterm.ini 与导出的 `.mxtsessions` 文件相同）：
//!
//! ```text
//! [Bookmarks]             根目录；其他文件夹为 [Bookmarks_1]、[Bookmarks_2]……
//! SubRep=生产\Web         文件夹路径，层级用反斜杠分隔，根目录为空
//! ImgNum=41               文件夹图标
//! web-01= #109#0%10.0.0.1%22%root%%-1%-1%%%%%0%0%0%_ProfileDir_\.ssh\id_rsa%...#终端设置#...
//! ```
//!
//! 会话值用 `#` 分为：重连设置、图标编号（109 是 SSH 会话的默认图标，用户可以更换）、连接参数、终端设置、
//! 启动方式、备注、标签颜色。连接参数用 `%` 分隔，第一项是会话类型：0 SSH、1 Telnet、2 Rsh、3 XDMCP、4 RDP、
//! 5 VNC、6 FTP、7 SFTP、8 串口、9 文件、10 本地终端、11 浏览器、12 Mosh、13 AWS S3、14 WSL。各类型连接参数的下标：
//!
//! - SSH：1 主机、2 端口、3 用户名、7 登录后执行的命令、8/9/10 SSH 网关的主机/端口/用户名（多级网关用
//!   `__PIPE__` 分隔）、14 私钥路径、15 网关私钥路径、19 代理类型（0 不使用、1 SOCKS4、2 SOCKS5、3 HTTP，
//!   其余为 Telnet、本地命令等，本地命令的占位符写法未知，不导入）、20 代理主机、21 代理端口、22 代理用户名
//!   （不导出代理密码）、34 agent 转发（-1 为开）。
//! - Telnet：1 主机、2 端口、3 用户名（自动登录用）、6/7/8/9 SSH 网关的主机/端口/用户名/私钥、11–14 代理的
//!   类型/主机/端口/用户名（编码与 SSH 相同）。
//! - 串口：2 波特率（新版为波特率加 1000000；不大于 100000 的是旧版下拉框的序号）、3 数据位（0–3 为 5–8）、
//!   4 停止位（0 为 1 位、1 为 2 位）、5 校验（无、奇、偶、标记、空格）、6 流控（无、XON/XOFF、RTS/CTS、DSR/DTR）、
//!   8 下拉框中显示的端口（`COM3  (USB-SERIAL CH340 (COM3))`，取开头的 COM 名）。
//!
//! 终端设置也用 `%` 分隔：5 字符集的编号（15 UTF-8、18 GBK、19 Big5、40 用字体的编码，其余 Termlet 不支持）、
//! 13 终端类型（Telnet 协商的终端类型）。
//!
//! 文本中的特殊字符被转义为 `__PERCENT__`、`__DIEZE__` 等；路径可能以 `_ProfileDir_`（用户目录）、
//! `_MyDocuments_`（文档目录）、`_CurrentDrive_`（MobaXterm 所在盘符）等占位符开头。
//!
//! 导入 SSH、Telnet 与串口会话：书签文件夹作为分组，私钥路径中的占位符展开为本机路径，SSH 网关导入为跳板机
//! （与已有书签相同的网关直接引用，否则另建一台主机）。其他类型的会话跳过并计数。SSH 隧道（`[PortForwarding]`）
//! 不与会话关联，不导入。

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf, Prefix};

use crate::model::{
    Auth, Encoding, FlowControl, Host, KeySource, Parity, Protocol, Proxy, ProxyKind,
    SERIAL_BAUD_RANGE, SerialOptions, StopBits,
};

use super::files::{LocalEnv, decode_text, read_config_file};
use super::one_line;
use super::plan::{Endpoint, Hint, JumpRef, PlanBuilder, PlannedHost, clip};
use super::xshell::is_term_type;
use super::{ImportError, ImportPlan, Source, sys};

pub(super) const MOBAXTERM_REGISTRY_SUBKEY: &str = r"Software\Mobatek\MobaXterm";

/// 表示从注册表读取安装版 MobaXterm 的书签（作为导入路径使用）。
pub const MOBAXTERM_REGISTRY_PATH: &str = r"HKEY_CURRENT_USER\Software\Mobatek\MobaXterm";

// 连接参数的下标（SSH、Telnet 相同的部分）
const HOST: usize = 1;
const PORT: usize = 2;
const USER: usize = 3;
// SSH 会话
const COMMAND: usize = 7;
const KEY: usize = 14;
const AGENT_FORWARD: usize = 34;
// 串口会话
const SERIAL_BAUD: usize = 2;
const SERIAL_DATA_BITS: usize = 3;
const SERIAL_STOP_BITS: usize = 4;
const SERIAL_PARITY: usize = 5;
const SERIAL_FLOW: usize = 6;
const SERIAL_PORT: usize = 8;
// 终端设置的下标
const TERM_CHARSET: usize = 5;
const TERM_TYPE: usize = 13;
const TYPE_SSH: &str = "0";
const TYPE_TELNET: &str = "1";
const TYPE_SERIAL: &str = "8";
/// 旧版串口会话中波特率下拉框的序号对应的波特率。
const LEGACY_BAUD_RATES: [u32; 11] = [
    1200, 2400, 4800, 9600, 19200, 38400, 57600, 115200, 460800, 921600, 230400,
];

/// SSH 网关（主机、端口、用户名、私钥）与代理（类型、主机、端口、用户名）在连接参数中的下标。
struct Layout {
    gateway: [usize; 4],
    proxy: [usize; 4],
}

const SSH_LAYOUT: Layout = Layout {
    gateway: [8, 9, 10, 15],
    proxy: [19, 20, 21, 22],
};
const TELNET_LAYOUT: Layout = Layout {
    gateway: [6, 7, 8, 9],
    proxy: [11, 12, 13, 14],
};
const HOP_SEP: &str = "__PIPE__";
const DEFAULT_USER: &str = "<default>";

/// `path` 可以是 MobaXterm.ini、导出的 `.mxtsessions` 文件、便携版的程序目录（其中的 MobaXterm.ini），
/// 或 [`MOBAXTERM_REGISTRY_PATH`]（安装版保存在注册表中的书签）。
pub(super) fn read(path: &Path, env: &LocalEnv) -> Result<ImportPlan, ImportError> {
    let mut paths = MobaPaths {
        home: env.home.as_ref().map(|p| p.to_string_lossy().into_owned()),
        documents: env
            .documents
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        drive: env.system_drive.clone(),
    };
    if paths.home.is_none() {
        paths.documents = None;
    }
    if path.to_str().is_some_and(is_registry_path) {
        return Ok(parse(&sys::read_mobaxterm_registry()?, &paths));
    }
    let (raw, file) = read_config_file(path, "MobaXterm.ini", env)?;
    // 便携版的 _CurrentDrive_ 是 MobaXterm 所在的盘符，即配置文件所在的盘符
    if let Some(drive) = std::path::absolute(&file).ok().and_then(|f| drive_of(&f)) {
        paths.drive = drive;
    }
    Ok(parse(&decode_text(&raw, env.ansi), &paths))
}

fn drive_of(path: &Path) -> Option<String> {
    match path.components().next()? {
        Component::Prefix(p) => match p.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => Some((d as char).to_string()),
            _ => None,
        },
        _ => None,
    }
}

fn is_registry_path(p: &str) -> bool {
    let p = p.trim();
    p.eq_ignore_ascii_case(MOBAXTERM_REGISTRY_PATH)
        || p.eq_ignore_ascii_case(&format!(r"HKCU\{MOBAXTERM_REGISTRY_SUBKEY}"))
}

/// 展开路径占位符所需的本机目录。
pub(super) struct MobaPaths {
    pub home: Option<String>,
    pub documents: Option<String>,
    /// 盘符字母（不含冒号），用于 `_CurrentDrive_`
    pub drive: String,
}

impl MobaPaths {
    /// 展开路径开头的占位符，返回展开后的路径与无法识别的占位符。
    fn expand(&self, path: &str) -> (String, Option<String>) {
        let replacements = [
            ("_ProfileDir_", self.home.as_deref()),
            ("_MyDocuments_", self.documents.as_deref()),
            (
                "_CurrentDrive_",
                Some(self.drive.as_str()).filter(|d| !d.is_empty()),
            ),
        ];
        let mut path = path.to_string();
        for (name, value) in replacements {
            if let (Some(value), Some(rest)) = (value, path.strip_prefix(name)) {
                path = format!("{value}{rest}");
                break;
            }
        }
        let unknown = leading_placeholder(&path).map(str::to_string);
        (path, unknown)
    }
}

/// 开头的 `_Name_` 形式的占位符。
fn leading_placeholder(path: &str) -> Option<&str> {
    let rest = path.strip_prefix('_')?;
    let len = rest.find(|c: char| !c.is_ascii_alphabetic())?;
    (len > 0 && rest[len..].starts_with('_')).then(|| &path[..len + 2])
}

struct Hop {
    host: String,
    port: u16,
    user: String,
    key: String,
}

/// 解析书签内容（已转为文本）。
pub(super) fn parse(text: &str, paths: &MobaPaths) -> ImportPlan {
    let mut plan = PlanBuilder::new(Source::MobaXterm);
    // 带 SSH 网关的会话（主机下标）及其网关
    let mut gateways: Vec<(usize, Vec<Hop>)> = Vec::new();
    let mut in_bookmarks = false;
    let mut folder: Vec<String> = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') {
            continue;
        }
        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_bookmarks = bookmark_number(section.trim()).is_some();
            folder.clear();
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if !in_bookmarks {
            continue;
        }
        let name = unescape(name.trim());
        let value = value.trim();
        if name.eq_ignore_ascii_case("SubRep") {
            folder = folder_of(value);
            continue;
        }
        if name.eq_ignore_ascii_case("ImgNum") {
            continue;
        }

        let fields: Vec<&str> = value.split('#').collect();
        if fields.len() < 3 || !is_digits(fields[1]) {
            continue; // 不是会话
        }
        let params: Vec<&str> = fields[2].split('%').collect();
        if !is_digits(params[0]) {
            continue;
        }
        let protocol = match params[0].trim() {
            TYPE_SSH => Protocol::Ssh,
            TYPE_TELNET => Protocol::Telnet,
            TYPE_SERIAL => Protocol::Serial,
            // RDP、VNC、SFTP 等
            _ => {
                plan.skip(1);
                continue;
            }
        };
        let address = match protocol {
            Protocol::Serial => serial_port(&param(&params, SERIAL_PORT)),
            _ => Some(param(&params, HOST)).filter(|a| !a.is_empty()),
        };
        let Some(address) = address else {
            plan.skip(1); // 缺少地址
            continue;
        };
        let mut username = match protocol {
            Protocol::Serial => String::new(),
            _ => param(&params, USER),
        };
        if username == DEFAULT_USER {
            username.clear();
        }
        let host = Host {
            name: if name.is_empty() {
                address.clone()
            } else {
                name.clone()
            },
            protocol,
            address,
            port: match protocol {
                Protocol::Serial => 0,
                _ => port_of(&param(&params, PORT), protocol.default_port()),
            },
            username,
            ..Default::default()
        };
        let key = if folder.is_empty() {
            name.clone()
        } else {
            format!("{}/{name}", folder.join("/"))
        };
        let mut item = PlannedHost::new(key, host);
        item.group = plan.group(&folder);
        // Telnet 没有用户名时在登录提示处输入，不提醒
        if protocol == Protocol::Ssh && item.host.username.is_empty() {
            item.hints.push(Hint::MissingUsername);
        }
        let terminal: Vec<&str> = fields
            .get(3)
            .map_or_else(Vec::new, |t| t.split('%').collect());
        if let Some((encoding, charset)) = charset(raw_param(&terminal, TERM_CHARSET)) {
            item.set_encoding(encoding, charset);
        }
        let layout = match protocol {
            Protocol::Ssh => {
                ssh_options(&mut item, &params, paths);
                Some(SSH_LAYOUT)
            }
            Protocol::Telnet => {
                let term = param(&terminal, TERM_TYPE);
                if is_term_type(&term) {
                    item.host.telnet.term_type = term;
                }
                Some(TELNET_LAYOUT)
            }
            Protocol::Serial => {
                item.host.serial = serial_options(&params, &mut item.hints);
                None
            }
        };
        let mut hops_found = Vec::new();
        if let Some(layout) = layout {
            add_proxy(&mut item, &params, layout.proxy);
            hops_found = hops(&params, layout.gateway, paths);
            if let Some(h) = hops_found.iter().find(|h| h.user.is_empty()) {
                item.hints.push(Hint::JumpMissingUsername {
                    name: h.host.clone(),
                });
            }
        }
        let index = plan.push(item);
        if !hops_found.is_empty() {
            gateways.push((index, hops_found));
        }
    }

    // SSH 网关：与已有书签相同（地址、端口、用户名）时直接引用，否则另建一台主机。
    // 多级网关依次串联：第二级经第一级连接，会话经最后一级连接
    let mut index: HashMap<Endpoint, usize> = HashMap::new();
    for (i, h) in plan.hosts().iter().enumerate() {
        index.entry(h.endpoint()).or_insert(i);
    }
    for (session, hops) in gateways {
        let mut prev: Option<JumpRef> = None;
        for hop in hops {
            let endpoint = Endpoint::new(&hop.host, hop.port, &hop.user);
            let target = match index.get(&endpoint) {
                Some(&i) => i,
                None => {
                    let s = &plan.hosts()[session];
                    let (session_name, group) = (s.host.name.clone(), s.group);
                    let mut host = Host {
                        name: endpoint_name(&hop.host, hop.port, &hop.user),
                        address: hop.host.clone(),
                        port: hop.port,
                        username: hop.user.clone(),
                        ..Default::default()
                    };
                    if !hop.key.is_empty() {
                        host.auth = Auth::Key {
                            source: KeySource::file(hop.key),
                        };
                    }
                    let key = format!(
                        "gateway:{}@{}:{}",
                        hop.user,
                        hop.host.to_lowercase(),
                        hop.port
                    );
                    let mut gw = PlannedHost::new(key, host);
                    gw.group = group;
                    gw.jump = prev.clone();
                    gw.hints.push(Hint::GatewayFor {
                        session: session_name,
                    });
                    if hop.user.is_empty() {
                        gw.hints.push(Hint::MissingUsername);
                    }
                    let i = plan.push(gw);
                    index.insert(endpoint.clone(), i);
                    i
                }
            };
            prev = Some(JumpRef::new(
                plan.hosts()[target].host.name.clone(),
                endpoint,
            ));
        }
        plan.host_mut(session).jump = prev;
    }
    plan.finish()
}

/// SSH 会话的私钥、登录后执行的命令与 agent 转发（不导入，提醒）。
fn ssh_options(item: &mut PlannedHost, params: &[&str], paths: &MobaPaths) {
    let key_path = param(params, KEY);
    if !key_path.is_empty() {
        let (path, unknown) = paths.expand(&key_path);
        item.host.auth = Auth::Key {
            source: KeySource::file(path),
        };
        if let Some(placeholder) = unknown {
            item.hints
                .push(Hint::UnknownPathPlaceholder { placeholder });
        }
    }
    let command = one_line(&param(params, COMMAND));
    if !command.is_empty() {
        item.host.terminal.startup = clip(&command, 1000);
    }
    if param(params, AGENT_FORWARD) == "-1" {
        item.hints.push(Hint::AgentForwardDisabled);
    }
}

/// 串口参数（见模块说明）；没有对应设置的流控（DSR/DTR）按不流控导入并提醒。
fn serial_options(params: &[&str], hints: &mut Vec<Hint>) -> SerialOptions {
    let number = |i: usize| param(params, i).parse::<u32>().ok();
    let defaults = SerialOptions::default();
    let baud = match number(SERIAL_BAUD) {
        Some(value) if value > 100_000 => value.checked_sub(1_000_000),
        Some(index) => LEGACY_BAUD_RATES.get(index as usize).copied(),
        None => None,
    };
    let flow_control = match number(SERIAL_FLOW) {
        Some(1) => FlowControl::XonXoff,
        Some(2) => FlowControl::RtsCts,
        Some(0) | None => FlowControl::None,
        Some(_) => {
            hints.push(Hint::SerialSettingsUnverified);
            FlowControl::None
        }
    };
    SerialOptions {
        baud: baud
            .filter(|b| SERIAL_BAUD_RANGE.contains(b))
            .unwrap_or(defaults.baud),
        data_bits: number(SERIAL_DATA_BITS)
            .filter(|&i| i <= 3)
            .map_or(defaults.data_bits, |i| 5 + i as u8),
        stop_bits: match number(SERIAL_STOP_BITS) {
            Some(1) => StopBits::Two,
            _ => StopBits::One,
        },
        parity: match number(SERIAL_PARITY) {
            Some(1) => Parity::Odd,
            Some(2) => Parity::Even,
            Some(3) => Parity::Mark,
            Some(4) => Parity::Space,
            _ => Parity::None,
        },
        flow_control,
        ..defaults
    }
}

/// 下拉框中显示的端口（`COM3  (USB-SERIAL CH340 (COM3))`）开头的 COM 名；不是 COM 口（命名管道等）时为 `None`。
fn serial_port(text: &str) -> Option<String> {
    let name = text.split_whitespace().next()?.to_ascii_uppercase();
    let digits = name.strip_prefix("COM")?;
    (!digits.is_empty() && digits.len() <= 3 && digits.bytes().all(|b| b.is_ascii_digit()))
        .then_some(name)
}

/// 终端设置中字符集的编号：没有设置（或用字体的编码）时为 `None`；否则是 Termlet 对应的编码（不支持时为 `None`）
/// 与它在 MobaXterm 中的名称。
fn charset(number: &str) -> Option<(Option<Encoding>, &'static str)> {
    const NAMES: [&str; 40] = [
        "ISO-8859-1",
        "ISO-8859-2",
        "ISO-8859-3",
        "ISO-8859-4",
        "ISO-8859-5",
        "ISO-8859-6",
        "ISO-8859-7",
        "ISO-8859-8",
        "ISO-8859-9",
        "ISO-8859-10",
        "ISO-8859-11",
        "ISO-8859-13",
        "ISO-8859-14",
        "ISO-8859-15",
        "ISO-8859-16",
        "UTF-8",
        "KOI8-R",
        "KOI8-U",
        "GBK",
        "Big5",
        "SJIS",
        "eucKR",
        "CP850",
        "Win1252",
        "Win1251",
        "Win1250",
        "Win1253",
        "Win1254",
        "Win1255",
        "Win1256",
        "Win1257",
        "Win1258",
        "CP437",
        "CP620",
        "CP819",
        "CP852",
        "CP878",
        "HP-ROMAN8",
        "VSCII",
        "DEC-MCS",
    ];
    let name = NAMES.get(number.trim().parse::<usize>().ok()?)?;
    let encoding = match *name {
        "UTF-8" => Some(Encoding::Utf8),
        "GBK" => Some(Encoding::Gb18030),
        "Big5" => Some(Encoding::Big5),
        _ => None,
    };
    Some((encoding, name))
}

/// 代理（类型 1 SOCKS4、2 SOCKS5、3 HTTP）；其他类型不支持，导入后直连。`at` 是类型、主机、端口、用户名的下标。
fn add_proxy(item: &mut PlannedHost, params: &[&str], at: [usize; 4]) {
    let [proxy_type, proxy_host, proxy_port, proxy_user] = at;
    let kind = param(params, proxy_type);
    let address = param(params, proxy_host);
    let (kind, default_port) = match kind.as_str() {
        "" | "0" => return,
        "1" if !address.is_empty() => (ProxyKind::Socks4, 1080),
        "2" if !address.is_empty() => (ProxyKind::Socks5, 1080),
        "3" if !address.is_empty() => (ProxyKind::Http, 8080),
        _ => {
            item.hints.push(Hint::UnsupportedProxy);
            return;
        }
    };
    let port = param(params, proxy_port)
        .parse::<u16>()
        .ok()
        .filter(|&p| p > 0);
    if port.is_none() {
        item.hints
            .push(Hint::ProxyPortDefaulted { port: default_port });
    }
    let username = param(params, proxy_user);
    if !username.is_empty() && kind != ProxyKind::Socks4 {
        item.hints.push(Hint::ProxyPasswordRequired);
    }
    item.host.proxy = Some(Proxy {
        kind,
        address,
        port: port.unwrap_or(default_port),
        username,
        command: String::new(),
    });
}

/// SSH 网关（可能有多级）。`at` 是主机、端口、用户名、私钥的下标。
fn hops(params: &[&str], at: [usize; 4], paths: &MobaPaths) -> Vec<Hop> {
    let split = |i| raw_param(params, i).split(HOP_SEP).collect::<Vec<_>>();
    let [host, port, user, key] = at;
    let (hosts, ports, users, keys) = (split(host), split(port), split(user), split(key));
    let at = |list: &[&str], i: usize| {
        list.get(i)
            .map_or_else(String::new, |s| unescape(s).trim().to_string())
    };
    let mut out = Vec::new();
    for i in 0..hosts.len() {
        let host = at(&hosts, i);
        if host.is_empty() {
            continue;
        }
        let key = at(&keys, i);
        out.push(Hop {
            host,
            port: port_of(&at(&ports, i), 22),
            user: at(&users, i),
            key: if key.is_empty() {
                key
            } else {
                paths.expand(&key).0
            },
        });
    }
    out
}

/// 本机的 MobaXterm 书签：优先找含书签的 MobaXterm.ini（安装版位于"文档\MobaXterm"或 `%APPDATA%\MobaXterm`），
/// 其次是注册表。便携版的配置文件在程序目录，需要用户选择。
pub(super) fn default_path(
    env: &LocalEnv,
    registry_has_bookmarks: fn() -> bool,
) -> Option<PathBuf> {
    let (path, bookmarks) = pick_ini(&ini_candidates(env));
    if !bookmarks && registry_has_bookmarks() {
        return Some(PathBuf::from(MOBAXTERM_REGISTRY_PATH));
    }
    path
}

fn ini_candidates(env: &LocalEnv) -> Vec<PathBuf> {
    let ini = |dir: &Path| dir.join("MobaXterm").join("MobaXterm.ini");
    let mut out = Vec::new();
    out.extend(env.documents.as_deref().map(ini));
    out.extend(env.home.as_ref().map(|h| ini(&h.join("Documents"))));
    out.extend(env.config_dir.as_deref().map(ini));
    out
}

/// 第一个含书签的配置文件；都不含书签时为第一个存在的文件。
fn pick_ini(candidates: &[PathBuf]) -> (Option<PathBuf>, bool) {
    let mut first = None;
    for c in candidates {
        let Ok(raw) = super::files::read_limited(c, super::files::MAX_CONFIG_SIZE) else {
            continue;
        };
        if raw.windows(10).any(|w| w == b"[Bookmarks") {
            return (Some(c.clone()), true);
        }
        first.get_or_insert_with(|| c.clone());
    }
    (first, false)
}

/// 书签小节（`Bookmarks`、`Bookmarks_N`，不区分大小写）的编号：`Bookmarks` 为 0，不是书签小节时为 `None`。
fn bookmark_number(section: &str) -> Option<u64> {
    let head = section.get(..9)?;
    if !head.eq_ignore_ascii_case("Bookmarks") {
        return None;
    }
    match &section[9..] {
        "" => Some(0),
        rest => {
            let digits = rest.strip_prefix('_')?;
            (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
                .then(|| digits.parse().unwrap_or(0))
        }
    }
}

/// 选出书签小节并按编号排序（注册表中只读这些子键）。
#[cfg(any(windows, test))]
pub(super) fn sort_bookmark_sections(names: Vec<String>) -> Vec<String> {
    let mut out: Vec<(u64, String)> = names
        .into_iter()
        .filter_map(|n| bookmark_number(&n).map(|num| (num, n)))
        .collect();
    out.sort_by_key(|(num, _)| *num);
    out.into_iter().map(|(_, n)| n).collect()
}

/// SubRep（反斜杠分隔）转为分组的各级名称。
fn folder_of(value: &str) -> Vec<String> {
    unescape(value)
        .split('\\')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// 还原 MobaXterm 转义的特殊字符。
fn unescape(s: &str) -> String {
    const ESCAPES: [(&str, &str); 6] = [
        ("__PERCENT__", "%"),
        ("__DIEZE__", "#"),
        ("__PTVIRG__", ";"),
        ("__DBLQUO__", "\""),
        ("__PIPE__", "|"),
        ("__EQUAL__", "="),
    ];
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    'outer: while !rest.is_empty() {
        if rest.starts_with("__") {
            for (from, to) in ESCAPES {
                if let Some(r) = rest.strip_prefix(from) {
                    out.push_str(to);
                    rest = r;
                    continue 'outer;
                }
            }
        }
        let c = rest.chars().next().unwrap_or_default();
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

fn raw_param<'a>(params: &[&'a str], i: usize) -> &'a str {
    params.get(i).copied().unwrap_or("")
}

fn param(params: &[&str], i: usize) -> String {
    unescape(raw_param(params, i)).trim().to_string()
}

fn port_of(s: &str, default: u16) -> u16 {
    s.trim()
        .parse::<u16>()
        .ok()
        .filter(|&p| p > 0)
        .unwrap_or(default)
}

fn is_digits(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// 没有名称的主机（SSH 网关）的显示名称。
fn endpoint_name(host: &str, port: u16, user: &str) -> String {
    let mut name = if user.is_empty() {
        host.to_string()
    } else {
        format!("{user}@{host}")
    };
    if port != 22 {
        name.push_str(&format!(":{port}"));
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::files::tests::{env, write};
    use crate::import::files::{decode_text, encoding_for_code_page};
    use crate::model::Protocol;

    const INI: &[u8] = include_bytes!("testdata/mobaxterm/MobaXterm.ini");

    fn testdata(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/import/testdata/mobaxterm")
            .join(name)
    }

    fn key_path(h: &PlannedHost) -> Option<&str> {
        match &h.host.auth {
            Auth::Key {
                source: KeySource::File { path, .. },
            } => Some(path),
            _ => None,
        }
    }

    /// 便于比较的一台主机：键、名称、地址、端口、用户名、分组路径、私钥路径、跳板机（名称与下标）、代理。
    type Row = (
        String,
        String,
        String,
        u16,
        String,
        String,
        Option<String>,
        Option<(String, Option<usize>)>,
        Option<(ProxyKind, String, u16)>,
    );

    fn rows(plan: &ImportPlan) -> Vec<Row> {
        plan.hosts
            .iter()
            .map(|h| {
                (
                    h.key.clone(),
                    h.host.name.clone(),
                    h.host.address.clone(),
                    h.host.port,
                    h.host.username.clone(),
                    h.group
                        .map_or_else(String::new, |g| plan.group_path(g).join("/")),
                    key_path(h).map(str::to_string),
                    h.jump.as_ref().map(|j| (j.name.clone(), j.host)),
                    h.host
                        .proxy
                        .as_ref()
                        .map(|p| (p.kind, p.address.clone(), p.port)),
                )
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn row(
        key: &str,
        name: &str,
        address: &str,
        port: u16,
        user: &str,
        group: &str,
        key_path: Option<&str>,
        jump: Option<(&str, usize)>,
        proxy: Option<(ProxyKind, &str, u16)>,
    ) -> Row {
        (
            key.into(),
            name.into(),
            address.into(),
            port,
            user.into(),
            group.into(),
            key_path.map(str::to_string),
            jump.map(|(n, i)| (n.to_string(), Some(i))),
            proxy.map(|(k, a, p)| (k, a.to_string(), p)),
        )
    }

    #[test]
    fn bookmarks() {
        let paths = MobaPaths {
            home: Some(r"C:\Users\me".into()),
            documents: Some(r"C:\Users\me\Documents".into()),
            drive: "D".into(),
        };
        let plan = parse(&decode_text(INI, encoding_rs::UTF_8), &paths);
        assert_eq!(
            plan.skipped, 4,
            "应跳过 RDP、VNC、SFTP 与缺少地址的会话共 4 个"
        );
        let want = vec![
            row(
                "web-01",
                "web-01",
                "10.0.1.1",
                22,
                "root",
                "",
                Some(r"C:\Users\me\.ssh\id_rsa"),
                None,
                None,
            ),
            row(
                "自定义图标",
                "自定义图标",
                "10.0.1.2",
                2222,
                "deploy",
                "",
                None,
                None,
                None,
            ),
            row(
                "生产/跳板机",
                "跳板机",
                "bastion.example.com",
                2222,
                "jump",
                "生产",
                Some(r"D:\keys\bastion"),
                None,
                None,
            ),
            // SOCKS5 代理直接导入（代理类型 2、主机、端口）
            row(
                "生产/测试#1",
                "测试#1",
                "10.0.2.1",
                22,
                "",
                "生产",
                Some(r"_AppDataDir_\keys\id"),
                None,
                Some((ProxyKind::Socks5, "proxy.local", 1080)),
            ),
            row(
                "生产/数据库/主库",
                "主库",
                "192.168.10.5",
                22,
                "dba",
                "生产/数据库",
                None,
                Some(("跳板机", 2)),
                None,
            ),
            row(
                "生产/数据库/备库",
                "备库",
                "192.168.20.5",
                22,
                "dba",
                "生产/数据库",
                None,
                Some(("ops2@gw2.example.com:2200", 8)),
                None,
            ),
            // Telnet 会话（端口 23，没有用户名时在登录提示处输入）
            row(
                "生产/数据库/Telnet 交换机",
                "Telnet 交换机",
                "10.0.0.70",
                23,
                "",
                "生产/数据库",
                None,
                None,
                None,
            ),
            row(
                "gateway:ops@gw1.example.com:22",
                "ops@gw1.example.com",
                "gw1.example.com",
                22,
                "ops",
                "生产/数据库",
                Some(r"C:\Users\me\.ssh\gw1"),
                None,
                None,
            ),
            row(
                "gateway:ops2@gw2.example.com:2200",
                "ops2@gw2.example.com:2200",
                "gw2.example.com",
                2200,
                "ops2",
                "生产/数据库",
                None,
                Some(("ops@gw1.example.com", 7)),
                None,
            ),
        ];
        assert_eq!(rows(&plan), want);

        let hints: Vec<_> = plan.hosts.iter().map(|h| h.hints.clone()).collect();
        assert_eq!(
            hints[3],
            [
                Hint::MissingUsername,
                Hint::UnknownPathPlaceholder {
                    placeholder: "_AppDataDir_".into()
                }
            ]
        );
        for gw in &hints[7..] {
            assert_eq!(
                gw,
                &[Hint::GatewayFor {
                    session: "备库".into()
                }]
            );
        }
        for (i, h) in hints.iter().enumerate() {
            if ![3, 7, 8].contains(&i) {
                assert!(h.is_empty(), "{} 不应有提醒：{h:?}", plan.hosts[i].key);
            }
        }
        // 网关的跳板引用：备库经 gw2，gw2 经 gw1
        let jump = plan.hosts[5].jump.as_ref().unwrap();
        assert_eq!(
            jump.endpoint,
            Endpoint::new("gw2.example.com", 2200, "ops2")
        );
        let telnet = &plan.hosts[6].host;
        assert_eq!(
            (telnet.protocol, telnet.telnet.term_type.as_str()),
            (Protocol::Telnet, "xterm")
        );
        assert!(
            plan.hosts
                .iter()
                .all(|h| h.host.terminal.encoding == Encoding::Utf8)
        );
    }

    #[test]
    fn from_file_or_dir() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("MobaXterm.ini"), INI);
        // 选择便携版的程序目录时读取其中的 MobaXterm.ini
        let plan = read(dir.path(), &env()).unwrap();
        assert_eq!((plan.hosts.len(), plan.skipped), (9, 4));
        for h in &plan.hosts {
            let path = key_path(h).unwrap_or_default();
            assert!(
                !path.contains("_ProfileDir_") && !path.contains("_CurrentDrive_"),
                "可识别的占位符应展开：{path}"
            );
        }
        if cfg!(windows) {
            // _CurrentDrive_ 取配置文件所在的盘符
            let bastion = key_path(&plan.hosts[2]).unwrap();
            assert!(
                bastion.ends_with(r":\keys\bastion") && bastion.len() == r"C:\keys\bastion".len()
            );
        }
        assert!(read(&dir.path().join("missing.ini"), &env()).is_err());
    }

    #[test]
    fn gbk() {
        // GBK 编码的配置文件按系统 ANSI 代码页解码；这里固定为中文 Windows 的代码页
        let mut env = env();
        env.ansi = encoding_for_code_page(936);
        let plan = read(&testdata("gbk.ini"), &env).unwrap();
        let got: Vec<_> = rows(&plan)
            .into_iter()
            .map(|r| (r.0, r.1, r.2, r.4, r.5))
            .collect();
        let want = [
            ("杭州网关", "杭州网关", "10.8.0.1", "root", ""),
            (
                "华东/杭州/订单服务-01",
                "订单服务-01",
                "10.8.1.1",
                "deploy",
                "华东/杭州",
            ),
        ];
        let want: Vec<_> = want
            .iter()
            .map(|&(k, n, a, u, g)| {
                (
                    k.to_string(),
                    n.to_string(),
                    a.to_string(),
                    u.to_string(),
                    g.to_string(),
                )
            })
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn proxies_and_hops() {
        let paths = MobaPaths {
            home: None,
            documents: None,
            drive: "C".into(),
        };
        let session = |name: &str, params: &str| format!("{name}= #109#{params}#MobaFont\n");
        let text = [
            "[bookmarks_3]\n".to_string(),
            // HTTP 代理没有端口、有用户名
            session(
                "http",
                "0%10.0.0.1%22%u%%-1%-1%%%%%0%0%0%%%-1%0%0%3%proxy%%pu",
            ),
            // SOCKS4 有用户名不需要密码
            session(
                "socks4",
                "0%10.0.0.2%22%u%%-1%-1%%%%%0%0%0%%%-1%0%0%1%proxy%1081%pu",
            ),
            // 本地命令代理不支持
            session(
                "cmd",
                "0%10.0.0.3%22%u%%-1%-1%%%%%0%0%0%%%-1%0%0%5%proxy%1%",
            ),
            // 网关没有用户名、私钥路径的占位符在没有用户目录时保留
            session(
                "gw",
                "0%10.0.0.4%22%<default>%%-1%-1%%gw.local%%%0%0%0%_ProfileDir_\\id%",
            ),
        ]
        .concat();
        let plan = parse(&text, &paths);
        let h = |i: usize| &plan.hosts[i];
        assert_eq!(
            h(0).host
                .proxy
                .as_ref()
                .map(|p| (p.kind, p.port, p.username.as_str())),
            Some((ProxyKind::Http, 8080, "pu"))
        );
        assert_eq!(
            h(0).hints,
            [
                Hint::ProxyPortDefaulted { port: 8080 },
                Hint::ProxyPasswordRequired
            ]
        );
        assert!(h(1).hints.is_empty());
        assert_eq!(h(2).hints, [Hint::UnsupportedProxy]);
        assert!(h(2).host.proxy.is_none());
        assert_eq!(h(3).host.username, "");
        assert_eq!(
            h(3).hints,
            [
                Hint::MissingUsername,
                Hint::UnknownPathPlaceholder {
                    placeholder: "_ProfileDir_".into()
                },
                Hint::JumpMissingUsername {
                    name: "gw.local".into()
                }
            ]
        );
        let gw = h(4);
        assert_eq!(
            (gw.host.name.as_str(), gw.host.username.as_str()),
            ("gw.local", "")
        );
        assert_eq!(
            gw.hints,
            [
                Hint::GatewayFor {
                    session: "gw".into()
                },
                Hint::MissingUsername
            ]
        );
        assert_eq!(h(3).jump.as_ref().and_then(|j| j.host), Some(4));
    }

    #[test]
    fn telnet_serial_and_terminal_settings() {
        let paths = MobaPaths {
            home: None,
            documents: None,
            drive: "C".into(),
        };
        let session = |name: &str, params: &str, terminal: &str| {
            format!("{name}= #98#{params}#{terminal}#0# #-1\n")
        };
        // 终端设置：下标 5 是字符集，13 是终端类型
        let term = |charset: &str, term: &str| {
            format!(
                "MobaFont%10%0%0%-1%{charset}%236,236,236%30,30,30%180,180,192%0%-1%0%%{term}%-1%-1%_Std_Colors_0_%80%24%0%1%-1%<none>%%0%0%-1"
            )
        };
        let text = [
            "[Bookmarks]\nSubRep=\n".to_string(),
            // Telnet：用户名、经 SSH 网关、SOCKS5 代理、GBK 编码
            session(
                "交换机",
                "1%10.0.0.70%2323%admin%%2%gw.local%22%ops%%0%2%proxy.local%1081%%%0%-1%0",
                &term("18", "vt100"),
            ),
            // 串口：新版的波特率（加 1000000），7 位、2 停止位、偶校验、RTS/CTS
            session(
                "控制台",
                "8%-2%1009600%2%1%2%2%2%COM3  (USB-SERIAL CH340 (COM3))",
                &term("19", "xterm"),
            ),
            // 旧版的波特率序号；DSR/DTR 流控没有对应的设置
            session("旧串口", "8%0%7%3%0%0%3%2%com12", &term("40", "xterm")),
            // 不是 COM 口、没有端口的串口会话跳过
            session("管道", "8%-2%1115200%3%0%0%1%2%\\\\.\\pipe\\vm", &term("15", "xterm")),
            session("旧格式", "8%0%7%3%0%0%1%2", &term("15", "xterm")),
            // SSH：登录后执行的命令（; 写作 __PTVIRG__）、agent 转发、不支持的字符集
            session(
                "ssh",
                "0%10.0.0.5%22%root%%-1%-1%cd /srv__PTVIRG__ ls%%%%0%0%0%%%-1%0%0%0%%1080%%0%0%1%%0%%%%0%-1%-1%-1",
                &term("23", "xterm"),
            ),
        ]
        .concat();
        let plan = parse(&text, &paths);
        assert_eq!((plan.hosts.len(), plan.skipped), (5, 2));
        let find = |name: &str| plan.hosts.iter().find(|h| h.host.name == name).unwrap();

        let telnet = find("交换机");
        assert_eq!(
            (
                telnet.host.protocol,
                telnet.host.address.as_str(),
                telnet.host.port,
                telnet.host.username.as_str(),
                telnet.host.telnet.term_type.as_str(),
                telnet.host.terminal.encoding,
            ),
            (
                Protocol::Telnet,
                "10.0.0.70",
                2323,
                "admin",
                "vt100",
                Encoding::Gb18030
            )
        );
        assert_eq!(
            telnet
                .host
                .proxy
                .as_ref()
                .map(|p| (p.kind, p.address.as_str(), p.port)),
            Some((ProxyKind::Socks5, "proxy.local", 1081))
        );
        let gateway = telnet.jump.as_ref().unwrap();
        assert_eq!(gateway.endpoint, Endpoint::new("gw.local", 22, "ops"));
        assert!(gateway.host.is_some(), "网关另建一台主机");
        assert!(telnet.hints.is_empty(), "{:?}", telnet.hints);

        let console = find("控制台");
        assert_eq!(
            (
                console.host.protocol,
                console.host.address.as_str(),
                console.host.port,
                console.host.terminal.encoding,
            ),
            (Protocol::Serial, "COM3", 0, Encoding::Big5)
        );
        assert_eq!(
            console.host.serial,
            SerialOptions {
                baud: 9600,
                data_bits: 7,
                stop_bits: StopBits::Two,
                parity: Parity::Even,
                flow_control: FlowControl::RtsCts,
                usb: None,
            }
        );
        assert!(console.hints.is_empty());
        let old = find("旧串口");
        assert_eq!(
            (
                old.host.address.as_str(),
                old.host.serial.baud,
                old.host.serial.data_bits,
                old.host.serial.flow_control
            ),
            ("COM12", 115200, 8, FlowControl::None)
        );
        assert_eq!(old.hints, [Hint::SerialSettingsUnverified]);

        let ssh = find("ssh");
        assert_eq!(ssh.host.terminal.startup, "cd /srv; ls");
        assert_eq!(ssh.host.terminal.encoding, Encoding::Utf8);
        assert_eq!(
            ssh.hints,
            [
                Hint::UnsupportedEncoding {
                    name: "Win1252".into()
                },
                Hint::AgentForwardDisabled,
            ]
        );
        assert!(!ssh.host.ssh.agent_forward);
    }

    #[test]
    fn helpers() {
        let got = sort_bookmark_sections(
            [
                "Bookmarks_10",
                "P",
                "Bookmarks",
                "C",
                "Bookmarks_2",
                "Bookmarksx",
                "Bookmarks_",
            ]
            .map(String::from)
            .to_vec(),
        );
        assert_eq!(
            got,
            ["Bookmarks", "Bookmarks_2", "Bookmarks_10"],
            "书签小节应只保留 Bookmarks* 并按编号排序（不读取密码等其他子键）"
        );
        assert!(is_registry_path(r" hkcu\Software\Mobatek\MobaXterm "));
        assert!(is_registry_path(MOBAXTERM_REGISTRY_PATH));
        assert!(!is_registry_path(r"C:\MobaXterm.ini"));
        assert_eq!(unescape("a__PERCENT__b__X__#"), "a%b__X__#");
        assert_eq!(leading_placeholder("_AppDataDir_\\x"), Some("_AppDataDir_"));
        assert_eq!(leading_placeholder("__x"), None);
        assert_eq!(leading_placeholder("_abc"), None);

        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("a.ini");
        let with_bookmarks = dir.path().join("b.ini");
        write(&plain, b"[Misc]\nx=1\n");
        assert_eq!(
            pick_ini(&[dir.path().join("none.ini"), plain.clone()]),
            (Some(plain.clone()), false),
            "没有含书签的文件时应返回第一个存在的文件"
        );
        write(&with_bookmarks, b"[Bookmarks]\nSubRep=\n");
        assert_eq!(
            pick_ini(&[plain.clone(), with_bookmarks.clone()]),
            (Some(with_bookmarks), true)
        );

        let mut env = env();
        env.documents = Some(dir.path().join("docs"));
        env.home = Some(dir.path().join("home"));
        assert_eq!(default_path(&env, || false), None);
        assert_eq!(
            default_path(&env, || true),
            Some(PathBuf::from(MOBAXTERM_REGISTRY_PATH))
        );
        let ini = dir
            .path()
            .join("docs")
            .join("MobaXterm")
            .join("MobaXterm.ini");
        write(&ini, b"[Misc]\n");
        assert_eq!(
            default_path(&env, || true),
            Some(PathBuf::from(MOBAXTERM_REGISTRY_PATH)),
            "文件中没有书签时优先注册表"
        );
        assert_eq!(default_path(&env, || false), Some(ini));
    }
}
