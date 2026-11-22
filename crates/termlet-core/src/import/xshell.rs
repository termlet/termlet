//! Xshell 的会话目录（Sessions）。
//!
//! 每个 `.xsh` 文件是一个 INI 格式的会话（常见为带 BOM 的 UTF-16LE），会话名取文件名，子目录作为分组。
//! 导入 SSH、Telnet 与串口会话（`[CONNECTION]` 的 Protocol；RLOGIN、SFTP 等其余协议跳过并计数）：
//!
//! - `[CONNECTION]`：Host、Port（SSH 默认 22，Telnet 默认 23）、Description（备注）。
//! - `[CONNECTION:AUTHENTICATION]`：UserName（Telnet 用于自动登录）。UserKey 只是 Xshell 用户密钥库中的名称，
//!   无法直接沿用，导入后需要重新选择私钥；Password 由 Xshell 自己加密，不导入；登录脚本（UseExpectSend）不导入。
//! - `[TERMINAL]`：CodePage 是 Windows 代码页（65001 UTF-8、936 GBK、950 Big5，其余不支持）；Telnet 会话的
//!   Type 作为协商的终端类型。
//! - `[CONNECTION:SSH]`：端口转发 FwdReqCount 与 `FwdReq_<序号>_*`：Incoming 0 为本地转发、1 为远程转发，Source、
//!   Port 是监听的地址与端口，Host、HostPort 是目标，Description 是名称；没有目标的（动态转发）与认不出的不导入。
//!   AgentForwarding 不导入（导入后关闭）。
//! - `[CONNECTION:PROXY]`：Proxy 只是 Xshell 全局代理列表中的名称（跳板机也是其中的一种），设置不在会话文件中，
//!   不导入，导入后直连。
//! - `[CONNECTION:SERIAL]`：都是下拉框中的序号。ComPort 从 0 起（0 为 COM1）；BaudRate 按 Windows 的标准波特率
//!   （110、300……256000）；DataBits 按 5–8；StopBits 按 1、1.5、2；Parity 按无、奇、偶、标记、空格；FlowCtrl 的
//!   编号无法确定，只认 0（不流控）。除 ComPort 外的编号没有官方说明，导入后提醒核对。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::model::{
    DEFAULT_BIND_HOST, Forward, ForwardKind, Host, Parity, Protocol, SerialOptions, StopBits,
};

use super::files::{
    LocalEnv, MAX_CONFIG_SIZE, decode_text, dir_segments, read_limited, same_path, walk_files,
};
use super::plan::{Hint, PlanBuilder, PlannedHost, clip, encoding_by_code_page};
use super::{ImportError, ImportPlan, Source};

/// 串口会话中 BaudRate 的序号对应的波特率（Windows 的标准波特率，与 Xshell 的下拉框相同）。
const BAUD_RATES: [u32; 15] = [
    110, 300, 600, 1200, 2400, 4800, 9600, 14400, 19200, 38400, 56000, 57600, 115200, 128000,
    256000,
];
/// 端口转发规则最多读取的条数。
const MAX_FORWARDS: usize = 256;

pub(super) fn read(dir: &Path, env: &LocalEnv) -> Result<ImportPlan, ImportError> {
    let dir = env.input_path(dir);
    let mut plan = PlanBuilder::new(Source::Xshell);
    for (rel, file) in walk_files(&dir, "xsh")? {
        // 单个文件读不了不影响其他会话
        let Ok(raw) = read_limited(&file, MAX_CONFIG_SIZE) else {
            continue;
        };
        add_session(&mut plan, &rel, &decode_text(&raw, env.ansi));
    }
    Ok(plan.finish())
}

type Section = HashMap<String, String>;

/// 加入一个会话：`rel` 是相对于会话目录的各级名称（最后一个是文件名），`text` 是文件内容。
pub(super) fn add_session(plan: &mut PlanBuilder, rel: &[String], text: &str) {
    let ini = parse_ini(text);
    let empty = Section::new();
    let section = |name: &str| ini.get(name).unwrap_or(&empty);
    let field = |section: &Section, key: &str| section.get(key).cloned().unwrap_or_default();
    let conn = section("CONNECTION");
    let auth = section("CONNECTION:AUTHENTICATION");
    let terminal = section("TERMINAL");
    let serial = section("CONNECTION:SERIAL");
    let protocol = match field(conn, "Protocol").to_ascii_uppercase().as_str() {
        "" | "SSH" => Protocol::Ssh,
        "TELNET" => Protocol::Telnet,
        "SERIAL" => Protocol::Serial,
        // RLOGIN、SFTP、FTP 等
        _ => {
            plan.skip(1);
            return;
        }
    };
    let address = match protocol {
        Protocol::Serial => match field(serial, "ComPort").parse::<u32>() {
            Ok(n) if n < 256 => format!("COM{}", n + 1),
            _ => String::new(),
        },
        _ => field(conn, "Host"),
    };
    if address.is_empty() {
        plan.skip(1);
        return;
    }
    let file_name = rel.last().map_or("", |s| s.as_str());
    let name = Path::new(file_name)
        .file_stem()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    let host = Host {
        name,
        protocol,
        address,
        port: field(conn, "Port")
            .parse::<u16>()
            .ok()
            .filter(|&p| p > 0 && protocol != Protocol::Serial)
            .unwrap_or(protocol.default_port()),
        username: field(auth, "UserName"),
        note: clip(&field(conn, "Description"), 2000),
        ..Default::default()
    };
    let mut item = PlannedHost::new(rel.join("/"), host);
    item.group = plan.group(dir_segments(rel));

    // CodePage 为 -1（系统默认）或没有时按 UTF-8
    if let Ok(page) = field(terminal, "CodePage").parse::<u32>() {
        item.set_encoding(encoding_by_code_page(page), &format!("CP{page}"));
    }
    match protocol {
        Protocol::Ssh => {
            let user_key = field(auth, "UserKey");
            if !user_key.is_empty() {
                item.hints.push(Hint::PrivateKeyNotImported {
                    name: Some(user_key),
                });
            }
            let ssh = section("CONNECTION:SSH");
            add_forwards(&mut item, ssh);
            if field(ssh, "AgentForwarding") == "1" {
                item.hints.push(Hint::AgentForwardDisabled);
            }
        }
        Protocol::Telnet => {
            let term = field(terminal, "Type");
            if is_term_type(&term) {
                item.host.telnet.term_type = term;
            }
        }
        Protocol::Serial => {
            item.host.serial = serial_options(serial);
            item.hints.push(Hint::SerialSettingsUnverified);
        }
    }
    let script_count = field(auth, "ExpectSend_Count").parse::<u32>().unwrap_or(0);
    if field(auth, "UseExpectSend") == "1" && script_count > 0 {
        item.hints.push(Hint::LoginScriptNotImported);
    }
    let proxy = field(section("CONNECTION:PROXY"), "Proxy");
    if !proxy.is_empty() && protocol != Protocol::Serial {
        item.hints
            .push(Hint::ProxyNotImported { name: Some(proxy) });
    }
    plan.push(item);
}

/// `[CONNECTION:SSH]` 中的端口转发；没有导入的条数记为提醒。
fn add_forwards(item: &mut PlannedHost, ssh: &Section) {
    let count = ssh
        .get("FwdReqCount")
        .and_then(|c| c.parse::<usize>().ok())
        .unwrap_or(0)
        .min(MAX_FORWARDS);
    let mut skipped = 0;
    for i in 0..count {
        let get = |key: &str| {
            ssh.get(&format!("FwdReq_{i}_{key}"))
                .cloned()
                .unwrap_or_default()
        };
        let port = |key: &str| get(key).parse::<u16>().ok().filter(|&p| p > 0);
        let kind = match get("Incoming").as_str() {
            "0" => Some(ForwardKind::Local),
            "1" => Some(ForwardKind::Remote),
            _ => None,
        };
        let target_host = get("Host");
        // 没有目标的是动态转发
        let (Some(kind), Some(bind_port), Some(target_port), false) =
            (kind, port("Port"), port("HostPort"), target_host.is_empty())
        else {
            skipped += 1;
            continue;
        };
        let source = get("Source");
        item.forwards.push(Forward {
            kind,
            name: clip(&get("Description"), 64),
            bind_host: if source.is_empty() {
                DEFAULT_BIND_HOST.into()
            } else {
                source
            },
            bind_port,
            target_host,
            target_port,
            ..Default::default()
        });
    }
    if skipped > 0 {
        item.hints
            .push(Hint::ForwardsNotImported { count: skipped });
    }
}

/// `[CONNECTION:SERIAL]` 中的串口参数（见模块说明）。
fn serial_options(serial: &Section) -> SerialOptions {
    let index = |key: &str| serial.get(key).and_then(|v| v.parse::<usize>().ok());
    let defaults = SerialOptions::default();
    SerialOptions {
        baud: index("BaudRate")
            .and_then(|i| BAUD_RATES.get(i).copied())
            .unwrap_or(defaults.baud),
        data_bits: index("DataBits")
            .filter(|&i| i <= 3)
            .map_or(defaults.data_bits, |i| 5 + i as u8),
        parity: match index("Parity") {
            Some(1) => Parity::Odd,
            Some(2) => Parity::Even,
            Some(3) => Parity::Mark,
            Some(4) => Parity::Space,
            _ => Parity::None,
        },
        stop_bits: match index("StopBits") {
            Some(1) => StopBits::OnePointFive,
            Some(2) => StopBits::Two,
            _ => StopBits::One,
        },
        ..defaults
    }
}

/// 可以作为 Telnet 协商的终端类型（与保存主机时的校验相同）。
pub(super) fn is_term_type(term: &str) -> bool {
    !term.is_empty()
        && term.len() <= 40
        && term
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
}

/// 解析 INI 内容：小节名区分大小写，键值去掉首尾空白，`;`、`#` 开头的行是注释。
fn parse_ini(text: &str) -> HashMap<String, HashMap<String, String>> {
    let mut sections: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            sections.entry(name.to_string()).or_default();
            current = Some(name.to_string());
        } else if let (Some(section), Some((k, v))) = (&current, line.split_once('=')) {
            sections
                .entry(section.clone())
                .or_default()
                .insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    sections
}

/// 会话目录的候选：NetSarang 用户数据目录（含版本号这一级）下的 `Xshell\Sessions`。
/// `registry` 取 Xshell 选项中自定义的用户数据目录（按版本号）。
pub(super) fn default_candidates(
    env: &LocalEnv,
    registry: impl Fn(&str) -> Option<PathBuf>,
) -> Vec<PathBuf> {
    data_dirs(env, registry)
        .into_iter()
        .map(|d| d.join("Xshell").join("Sessions"))
        .collect()
}

/// NetSarang 用户数据目录的候选：Xshell 选项中自定义的位置，以及"文档"下的默认位置。
fn data_dirs(env: &LocalEnv, registry: impl Fn(&str) -> Option<PathBuf>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for version in ["8", "7", "6", "5"] {
        let candidates = [
            registry(version),
            env.documents
                .as_ref()
                .map(|d| d.join("NetSarang Computer").join(version)),
            env.home
                .as_ref()
                .map(|h| h.join("Documents").join("NetSarang Computer").join(version)),
        ];
        for d in candidates.into_iter().flatten() {
            if !dirs.iter().any(|x| same_path(x, &d)) {
                dirs.push(d);
            }
        }
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::files::tests::{env, utf16le, write};
    use crate::model::{Encoding, FlowControl};

    #[test]
    fn sessions_directory() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("生产").join("web-01.xsh"),
            &utf16le(
                "[CONNECTION]\r\nHost=10.0.1.1\r\nPort=22022\r\nProtocol=SSH\r\n[CONNECTION:AUTHENTICATION]\r\nUserName=root\r\nUserKey=id_rsa_prod\r\n",
            ),
        );
        write(
            &dir.path().join("test.xsh"),
            b"[CONNECTION]\nHost=10.0.2.2\n[CONNECTION:AUTHENTICATION]\nUserName=dev\n",
        );
        write(
            &dir.path().join("switch.xsh"),
            b"[CONNECTION]\nHost=10.0.3.3\nProtocol=TELNET\n",
        );
        write(
            &dir.path().join("rlogin.xsh"),
            b"[CONNECTION]\nHost=10.0.4.4\nProtocol=RLOGIN\n",
        );
        write(&dir.path().join("readme.txt"), b"ignored");

        let plan = read(dir.path(), &env()).unwrap();
        assert_eq!(
            plan.hosts.len(),
            3,
            "应导入 SSH 与 Telnet 会话（跳过 RLOGIN 和非 .xsh 文件）"
        );
        assert_eq!(plan.skipped, 1);
        let prod = plan
            .hosts
            .iter()
            .find(|h| h.key == "生产/web-01.xsh")
            .unwrap();
        assert_eq!(prod.host.name, "web-01");
        assert_eq!(prod.host.address, "10.0.1.1");
        assert_eq!(prod.host.port, 22022);
        assert_eq!(prod.host.username, "root");
        assert_eq!(plan.group_path(prod.group.unwrap()), ["生产"]);
        assert_eq!(
            prod.hints,
            [Hint::PrivateKeyNotImported {
                name: Some("id_rsa_prod".into())
            }]
        );
        let test = plan.hosts.iter().find(|h| h.key == "test.xsh").unwrap();
        assert_eq!((test.host.port, test.group), (22, None));
        assert!(test.hints.is_empty());
        let switch = plan.hosts.iter().find(|h| h.key == "switch.xsh").unwrap();
        assert_eq!(
            (switch.host.protocol, switch.host.port),
            (Protocol::Telnet, 23)
        );

        assert!(read(&dir.path().join("missing"), &env()).is_err());
    }

    fn session(text: &str) -> PlannedHost {
        let mut plan = PlanBuilder::new(Source::Xshell);
        add_session(&mut plan, &["s.xsh".into()], text);
        let mut plan = plan.finish();
        assert_eq!(plan.hosts.len(), 1, "{text}");
        plan.hosts.remove(0)
    }

    #[test]
    fn ssh_fields_forwards_and_hints() {
        let h = session(
            "[SessionInfo]\nVersion=7.1\nDescription=Xshell session file\n\
             [CONNECTION]\nHost=web.example.com\nPort=2222\nProtocol=SSH\nDescription=订单服务\n\
             [CONNECTION:AUTHENTICATION]\nUserName=deploy\nAuthMethodList=01,10,20,30,40,50\n\
             UseExpectSend=1\nExpectSend_Count=1\nExpectSend_Expect_0=$\nExpectSend_Send_0=sudo -i\n\
             [CONNECTION:PROXY]\nProxy=公司代理\nStartUp=0\n\
             [CONNECTION:SSH]\nAgentForwarding=1\nFwdReqCount=5\n\
             FwdReq_0_Incoming=0\nFwdReq_0_Source=localhost\nFwdReq_0_Port=1883\nFwdReq_0_LocalOnly=0\n\
             FwdReq_0_Host=localhost\nFwdReq_0_HostPort=1883\nFwdReq_0_Description=mqtt端口\n\
             FwdReq_1_Incoming=1\nFwdReq_1_Source=\nFwdReq_1_Port=9000\nFwdReq_1_Host=127.0.0.1\nFwdReq_1_HostPort=3000\n\
             FwdReq_2_Incoming=0\nFwdReq_2_Source=localhost\nFwdReq_2_Port=1080\nFwdReq_2_Host=\nFwdReq_2_HostPort=0\n\
             FwdReq_3_Incoming=7\nFwdReq_3_Port=1\nFwdReq_3_Host=h\nFwdReq_3_HostPort=1\n\
             [TERMINAL]\nType=xterm\nCodePage=936\n",
        );
        assert_eq!(
            (
                h.host.protocol,
                h.host.address.as_str(),
                h.host.port,
                h.host.username.as_str(),
                h.host.note.as_str(),
            ),
            (Protocol::Ssh, "web.example.com", 2222, "deploy", "订单服务")
        );
        assert_eq!(h.host.terminal.encoding, Encoding::Gb18030);
        // 本地、远程转发导入；没有目标的（动态转发）、认不出的、缺少的（第 5 条）不导入
        let rules: Vec<_> = h
            .forwards
            .iter()
            .map(|f| {
                (
                    f.kind,
                    f.name.as_str(),
                    f.bind_host.as_str(),
                    f.bind_port,
                    f.target_host.as_str(),
                    f.target_port,
                    f.auto_start,
                )
            })
            .collect();
        assert_eq!(
            rules,
            [
                (
                    ForwardKind::Local,
                    "mqtt端口",
                    "localhost",
                    1883,
                    "localhost",
                    1883,
                    false
                ),
                (
                    ForwardKind::Remote,
                    "",
                    DEFAULT_BIND_HOST,
                    9000,
                    "127.0.0.1",
                    3000,
                    false
                ),
            ]
        );
        assert_eq!(
            h.hints,
            [
                Hint::ForwardsNotImported { count: 3 },
                Hint::AgentForwardDisabled,
                Hint::LoginScriptNotImported,
                Hint::ProxyNotImported {
                    name: Some("公司代理".into())
                },
            ]
        );
        assert!(!h.host.ssh.agent_forward);
    }

    #[test]
    fn telnet_and_serial_sessions() {
        let telnet = session(
            "[CONNECTION]\nHost=10.0.0.70\nProtocol=TELNET\n\
             [CONNECTION:AUTHENTICATION]\nUserName=admin\nPassword=AAECAw==\nTelnetLoginPrompt=ogin:\n\
             [TERMINAL]\nType=vt100\nCodePage=65001\n",
        );
        assert_eq!(
            (
                telnet.host.protocol,
                telnet.host.port,
                telnet.host.username.as_str(),
                telnet.host.telnet.term_type.as_str(),
                telnet.host.terminal.encoding,
            ),
            (Protocol::Telnet, 23, "admin", "vt100", Encoding::Utf8)
        );
        // Xshell 自己加密的密码不导入
        assert!(!telnet.has_password() && telnet.hints.is_empty());
        // 终端类型不合法时保持默认
        let odd =
            session("[CONNECTION]\nHost=h\nProtocol=telnet\nPort=2323\n[TERMINAL]\nType=ESC[n~\n");
        assert_eq!(odd.host.telnet.term_type, "xterm-256color");
        assert_eq!(odd.host.port, 2323);

        // SerialPort3.xsh（Xshell 5）的写法；序号之外的值按默认
        let serial = session(
            "[CONNECTION:SERIAL]\nBaudRate=11\nStopBits=0\nFlowCtrl=0\nParity=0\nDataBits=3\nComPort=2\n\
             [CONNECTION]\nProtocol=SERIAL\nPort=22\n[TERMINAL]\nCodePage=936\n",
        );
        assert_eq!(
            (
                serial.host.protocol,
                serial.host.address.as_str(),
                serial.host.port,
                serial.host.terminal.encoding,
            ),
            (Protocol::Serial, "COM3", 0, Encoding::Gb18030)
        );
        assert_eq!(
            serial.host.serial,
            SerialOptions {
                baud: 57600,
                ..Default::default()
            }
        );
        assert_eq!(serial.hints, [Hint::SerialSettingsUnverified]);
        let options = |text: &str| {
            session(&format!(
                "[CONNECTION]\nProtocol=SERIAL\n[CONNECTION:SERIAL]\nComPort=0\n{text}"
            ))
            .host
            .serial
        };
        let o = options("BaudRate=12\nDataBits=2\nStopBits=2\nParity=2\nFlowCtrl=1\n");
        assert_eq!(
            (o.baud, o.data_bits, o.stop_bits, o.parity, o.flow_control),
            (115200, 7, StopBits::Two, Parity::Even, FlowControl::None)
        );
        let o = options("BaudRate=99\nDataBits=9\nStopBits=1\nParity=4\n");
        assert_eq!(
            (o.baud, o.data_bits, o.stop_bits, o.parity),
            (115200, 8, StopBits::OnePointFive, Parity::Space)
        );
        // 没有端口号的串口会话跳过
        let mut plan = PlanBuilder::new(Source::Xshell);
        add_session(
            &mut plan,
            &["x.xsh".into()],
            "[CONNECTION]\nProtocol=SERIAL\n",
        );
        assert_eq!((plan.hosts().len(), plan.finish().skipped), (0, 1));
    }

    #[test]
    fn code_pages() {
        let encoding = |page: &str| {
            let h = session(&format!(
                "[CONNECTION]\nHost=h\n[TERMINAL]\nCodePage={page}\n"
            ));
            (h.host.terminal.encoding, h.hints)
        };
        assert_eq!(encoding("65001"), (Encoding::Utf8, vec![]));
        assert_eq!(encoding("54936"), (Encoding::Gb18030, vec![]));
        assert_eq!(encoding("950"), (Encoding::Big5, vec![]));
        // -1 是系统默认，按 UTF-8
        assert_eq!(encoding("-1"), (Encoding::Utf8, vec![]));
        assert_eq!(
            encoding("1252"),
            (
                Encoding::Utf8,
                vec![Hint::UnsupportedEncoding {
                    name: "CP1252".into()
                }]
            )
        );
    }

    #[test]
    fn default_candidates_deduplicate() {
        let mut env = env();
        let home = env.home.clone().unwrap();
        env.documents = Some(home.join("Documents"));
        let custom = home.join("Xshell-data");
        let got = default_candidates(&env, |v| (v == "7").then(|| custom.clone()));
        assert_eq!(got.len(), 5, "4 个版本的默认位置加上自定义位置：{got:?}");
        assert_eq!(
            got[0],
            home.join("Documents")
                .join("NetSarang Computer")
                .join("8")
                .join("Xshell")
                .join("Sessions")
        );
        assert_eq!(got[1], custom.join("Xshell").join("Sessions"));
    }
}
