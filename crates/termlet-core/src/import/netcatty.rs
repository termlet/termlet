//! Netcatty 在主机页「导出」得到的 CSV（`hosts_export_<日期>.csv`）。
//!
//! Netcatty 把主机保存在 Electron 的 localStorage（Chromium 的 LevelDB）中，密码等秘密用系统的 safeStorage 加密，
//! 别的程序读不了（也不该去解）；能读的只有这份导出。默认位置是“下载”“桌面”“文档”中最新的 `hosts_export_*.csv`。
//!
//! CSV 是 UTF-8（带 BOM）、RFC 4180 的写法（引号中可以有逗号与换行），按第一行的列名对应（不区分大小写）：
//! Groups、Label、Tags、Notes、Hostname/IP、Protocol、Port、Username、Password、KeyPath、Passphrase、Proxy、ProxyJump
//! （另有 LocalShell、SshInputCompatibility，不用）。没有 Hostname/IP 列时不是 Netcatty 的导出。
//!
//! - Groups 是 `/` 分隔的分组路径；Tags 用逗号（或分号、全角逗号）分隔；Notes 导入为备注。
//! - Protocol：ssh（mosh、EternalTerminal 的主机也写成 ssh）与 telnet 导入；local（本机终端）等跳过并计数。
//! - KeyPath 是本机私钥文件的路径；Netcatty 密钥库中的私钥、身份（Identity）不在导出中，这样的主机按密码认证导入。
//! - Proxy：`direct`（直连）、`http://` 与 `socks5://`（用户名、密码经百分号编码）、`command://<命令>`。
//! - ProxyJump：OpenSSH 的写法（`user@host[:port]`，多级用逗号连接），与 ssh_config 一样只导入最后一跳并提醒。
//! - 普通文本列以 `= + - @` 等开头时 Netcatty 在前面加了 `'`（防止被表格软件当成公式），这里去掉；KeyPath、
//!   Passphrase、Proxy、ProxyJump 这样开头时写成 `__netcatty_csv_<列>_v1__:` 加百分号编码，这里还原。Password 列原样。
//!
//! 秘密：Password、Passphrase 与代理的密码在 CSV 中是**明文**。界面要用户明确勾选“导入保存的密码”后才导入
//! （[`super::Source::secrets_need_consent`]）；以 `enc:v1:` 开头的是 Netcatty 导出时没能解开的密文，不导入并提醒。

use std::path::{Path, PathBuf};

use crate::model::{Auth, Host, KeySource, Protocol, Proxy, ProxyKind};
use crate::secret::SecretString;

use super::files::{LocalEnv, MAX_CONFIG_SIZE, decode_text, newest_export, read_limited};
use super::plan::{Endpoint, Hint, JumpRef, PlanBuilder, PlannedHost, clip};
use super::ssh_config::{parse_port, split_host_port};
use super::{ImportError, ImportPlan, Source};

/// Netcatty 没能解开的秘密（设备绑定的 safeStorage 密文）的前缀。
const ENCRYPTED: &str = "enc:v1:";
/// 导出文件名的开头。
const FILE_PREFIX: &str = "hosts_export_";
/// 分组的层数上限。
const MAX_GROUP_DEPTH: usize = 32;

pub(super) fn read(path: &Path, env: &LocalEnv) -> Result<ImportPlan, ImportError> {
    let path = env.input_path(path);
    let raw = read_limited(&path, MAX_CONFIG_SIZE)?;
    parse(&decode_text(&raw, encoding_rs::UTF_8), env)
}

/// 默认位置：“下载”“桌面”“文档”中修改时间最新的 `hosts_export_*.csv`；都没有时为 `None`。
pub(super) fn default_path(env: &LocalEnv) -> Option<PathBuf> {
    newest_export(env, |name| {
        name.starts_with(FILE_PREFIX) && name.ends_with(".csv")
    })
}

/// 各列在行中的位置。
struct Columns {
    groups: Option<usize>,
    label: Option<usize>,
    tags: Option<usize>,
    notes: Option<usize>,
    host: usize,
    protocol: Option<usize>,
    port: Option<usize>,
    username: Option<usize>,
    password: Option<usize>,
    key_path: Option<usize>,
    passphrase: Option<usize>,
    proxy: Option<usize>,
    proxy_jump: Option<usize>,
}

impl Columns {
    /// 按列名（不区分大小写，认 Motty 也认的别名）找各列；没有地址列时为 `None`。
    fn find(header: &[String]) -> Option<Self> {
        let names: Vec<String> = header
            .iter()
            .map(|h| h.trim().trim_start_matches('\u{feff}').to_lowercase())
            .collect();
        let find = |aliases: &[&str]| names.iter().position(|n| aliases.contains(&n.as_str()));
        Some(Self {
            groups: find(&["groups", "group"]),
            label: find(&["label", "name"]),
            tags: find(&["tags"]),
            notes: find(&["notes"]),
            host: find(&["hostname/ip", "hostname", "host", "ip"])?,
            protocol: find(&["protocol"]),
            port: find(&["port"]),
            username: find(&["username", "user"]),
            password: find(&["password"]),
            key_path: find(&["keypath"]),
            passphrase: find(&["passphrase"]),
            proxy: find(&["proxy"]),
            proxy_jump: find(&["proxyjump"]),
        })
    }
}

pub(super) fn parse(text: &str, env: &LocalEnv) -> Result<ImportPlan, ImportError> {
    let mut rows = parse_csv(text).into_iter();
    let columns = rows
        .next()
        .and_then(|header| Columns::find(&header))
        .ok_or(ImportError::NotNetcatty)?;
    let mut plan = PlanBuilder::new(Source::Netcatty);
    for row in rows {
        if row.iter().all(|cell| cell.trim().is_empty()) {
            continue;
        }
        add_row(&mut plan, &columns, &row, env);
    }
    Ok(plan.finish())
}

fn add_row(plan: &mut PlanBuilder, c: &Columns, row: &[String], env: &LocalEnv) {
    let cell = |i: Option<usize>| i.and_then(|i| row.get(i)).map_or("", String::as_str);
    let text = |i: Option<usize>| unguard(cell(i)).trim().to_string();
    let marked = |i: Option<usize>, tag: &str| unmark(cell(i), tag).trim().to_string();

    let address = text(Some(c.host));
    let address = address
        .strip_prefix('[')
        .and_then(|a| a.strip_suffix(']'))
        .unwrap_or(&address)
        .to_string();
    let protocol = match text(c.protocol).to_ascii_lowercase().as_str() {
        "" | "ssh" | "ssh2" | "ssh-2" | "mosh" | "et" => Protocol::Ssh,
        "telnet" => Protocol::Telnet,
        // 本机终端、串口、插件的连接
        _ => {
            plan.skip(1);
            return;
        }
    };
    if address.is_empty() {
        plan.skip(1);
        return;
    }
    let label = text(c.label);
    let name = if label.is_empty() {
        address.clone()
    } else {
        label
    };
    let mut tags: Vec<String> = Vec::new();
    for tag in text(c.tags).split([',', ';', '，']) {
        let tag = tag.trim();
        if !tag.is_empty() && tags.len() < 20 && !tags.iter().any(|t| t == tag) {
            tags.push(clip(tag, 32));
        }
    }
    let host = Host {
        name: clip(&name, 128),
        protocol,
        address,
        port: parse_port(&text(c.port)).unwrap_or(0),
        username: text(c.username),
        tags,
        note: clip(&text(c.notes).replace("\r\n", "\n"), 2000),
        ..Default::default()
    };
    let group_path = text(c.groups);
    let segments: Vec<&str> = group_path
        .split('/')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .take(MAX_GROUP_DEPTH)
        .collect();
    let key = if segments.is_empty() {
        host.name.clone()
    } else {
        format!("{}/{}", segments.join("/"), host.name)
    };
    let mut item = PlannedHost::new(key, host);
    item.group = plan.group(&segments);

    let key_path = marked(c.key_path, "keypath");
    if protocol == Protocol::Ssh && !key_path.is_empty() {
        item.host.auth = Auth::Key {
            source: KeySource::file(env.expand_home(&key_path)),
        };
        let passphrase = marked(c.passphrase, "passphrase");
        if !passphrase.is_empty() && !passphrase.starts_with(ENCRYPTED) {
            item.set_passphrase(SecretString::new(passphrase));
        }
    } else {
        // 密码列原样（Netcatty 不给它加 `'`）
        let password = cell(c.password);
        if password.starts_with(ENCRYPTED) {
            item.hints.push(Hint::SavedPasswordEncrypted);
        } else if !password.is_empty() {
            item.set_password(SecretString::new(password));
        }
    }

    if protocol == Protocol::Ssh {
        let jump = marked(c.proxy_jump, "proxyjump");
        let hops: Vec<&str> = jump
            .split(',')
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .collect();
        if let Some(last) = hops.last() {
            match jump_ref(last) {
                Some(jump) => item.jump = Some(jump),
                None => item.hints.push(Hint::JumpMissingUsername {
                    name: last.to_string(),
                }),
            }
            if hops.len() > 1 {
                item.hints.push(Hint::MultiHopJump {
                    chain: hops.iter().map(|h| h.to_string()).collect(),
                });
            }
        }
    }
    add_proxy(&mut item, &marked(c.proxy, "proxy"));
    if protocol == Protocol::Ssh && item.host.username.is_empty() {
        item.hints.push(Hint::MissingUsername);
    }
    plan.push(item);
}

/// 跳板机的一跳 `user@host[:port]`；没有用户名时为 `None`。
fn jump_ref(hop: &str) -> Option<JumpRef> {
    let (user, host_port) = hop.rsplit_once('@')?;
    if user.trim().is_empty() {
        return None;
    }
    let (host, port) = match split_host_port(host_port) {
        Some((host, port)) => (host, parse_port(port).unwrap_or(22)),
        None => (
            host_port
                .strip_prefix('[')
                .and_then(|h| h.strip_suffix(']'))
                .unwrap_or(host_port),
            22,
        ),
    };
    Some(JumpRef::new(hop, Endpoint::new(host, port, user)))
}

/// 代理列：`direct`、`http://[user[:pass]@]host:port`、`socks5://…`、`command://<命令>`；空为沿用 Netcatty 的分组或
/// 全局设置（导出时已展开分组的，全局的不导出），与 `direct` 一样导入为直连。
fn add_proxy(item: &mut PlannedHost, value: &str) {
    if value.is_empty() || value.eq_ignore_ascii_case("direct") {
        return;
    }
    let Some((scheme, rest)) = value.split_once("://") else {
        item.hints.push(Hint::UnsupportedProxy);
        return;
    };
    let (kind, default_port) = match scheme.to_ascii_lowercase().as_str() {
        "command" => {
            item.host.proxy = Some(Proxy {
                kind: ProxyKind::Command,
                command: rest.trim().to_string(),
                ..Default::default()
            });
            item.hints.push(Hint::ProxyCommandImported);
            return;
        }
        "http" => (ProxyKind::Http, 8080),
        "socks5" | "socks5h" | "socks" => (ProxyKind::Socks5, 1080),
        _ => {
            item.hints.push(Hint::UnsupportedProxy);
            return;
        }
    };
    let rest = rest.trim_end_matches('/');
    let (user_info, host_port) = match rest.rsplit_once('@') {
        Some((user_info, host_port)) => (Some(user_info), host_port),
        None => (None, rest),
    };
    let (address, port) = match split_host_port(host_port) {
        Some((host, port)) => (host.to_string(), parse_port(port)),
        None => (
            host_port
                .strip_prefix('[')
                .and_then(|h| h.strip_suffix(']'))
                .unwrap_or(host_port)
                .to_string(),
            None,
        ),
    };
    if address.is_empty() {
        item.hints.push(Hint::UnsupportedProxy);
        return;
    }
    let port = port.unwrap_or_else(|| {
        item.hints
            .push(Hint::ProxyPortDefaulted { port: default_port });
        default_port
    });
    let (username, password) = match user_info.map(|u| u.split_once(':').unwrap_or((u, ""))) {
        Some((user, password)) => (percent_decode(user), percent_decode(password)),
        None => (String::new(), String::new()),
    };
    if !username.is_empty() && !password.is_empty() {
        item.set_proxy_password(SecretString::new(password));
    }
    item.host.proxy = Some(Proxy {
        kind,
        address,
        port,
        username,
        ..Default::default()
    });
}

/// 去掉 Netcatty 为防止被当成公式而加在前面的 `'`（后面是 `= + - @`、制表符或回车时）。
fn unguard(value: &str) -> &str {
    match value.strip_prefix('\'') {
        Some(rest) if rest.starts_with(['=', '+', '-', '@', '\t', '\r']) => rest,
        _ => value,
    }
}

/// 还原 `__netcatty_csv_<tag>_v1__:` 加百分号编码的写法；不是这种写法时同 [`unguard`]。
fn unmark(value: &str, tag: &str) -> String {
    let marker = format!("__netcatty_csv_{tag}_v1__:");
    match value.strip_prefix(&marker) {
        Some(encoded) => percent_decode(encoded),
        None => unguard(value).to_string(),
    }
}

/// 百分号编码（`encodeURIComponent`）还原；不合法的 `%` 原样保留，不是 UTF-8 的字节换成替换字符。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            )
        {
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// RFC 4180 的 CSV：逗号分隔，双引号包住的字段中可以有逗号、换行，`""` 是一个引号；行以 CRLF 或 LF 结束。
/// 引号外面的引号（不规范的写法）也按开始引号处理，不会失败。
fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' => quoted = true,
            ',' => row.push(std::mem::take(&mut field)),
            '\r' if chars.peek() == Some(&'\n') => {}
            '\r' | '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            other => field.push(other),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;
    use crate::import::files::tests::env;

    fn testdata() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/import/testdata/netcatty/hosts_export_2026-11-09.csv")
    }

    fn proxy_of(h: &PlannedHost) -> Option<(ProxyKind, &str, u16, &str)> {
        h.host
            .proxy
            .as_ref()
            .map(|p| (p.kind, p.address.as_str(), p.port, p.username.as_str()))
    }

    #[test]
    fn csv_quotes_newlines_and_bom() {
        let rows =
            parse_csv("\u{feff}a,b\r\n\"x,1\",\"two\r\nlines\"\r\n\"say \"\"hi\"\"\",\r\nlast");
        assert_eq!(
            rows,
            vec![
                vec!["\u{feff}a".to_string(), "b".into()],
                vec!["x,1".into(), "two\r\nlines".into()],
                vec!["say \"hi\"".into(), String::new()],
                vec!["last".into()],
            ]
        );
        assert!(parse_csv("").is_empty());
    }

    #[test]
    fn guards_and_markers_are_undone() {
        assert_eq!(unguard("'=calc"), "=calc");
        assert_eq!(unguard("'-dash"), "-dash");
        assert_eq!(unguard("'plain"), "'plain");
        assert_eq!(
            unmark("__netcatty_csv_keypath_v1__:%3D%2Fkeys%2Fid", "keypath"),
            "=/keys/id"
        );
        assert_eq!(unmark("'@x", "proxy"), "@x");
        assert_eq!(percent_decode("p%40ss%E4%B8%AD%zz"), "p@ss中%zz");
    }

    #[test]
    fn exported_hosts() {
        let plan = read(&testdata(), &env()).unwrap();
        assert_eq!(plan.source, Source::Netcatty);
        // local 主机跳过
        assert_eq!((plan.hosts.len(), plan.skipped), (6, 1));
        let by_name = |name: &str| plan.hosts.iter().find(|h| h.host.name == name).unwrap();

        let web = by_name("Web");
        assert_eq!(
            (
                web.host.address.as_str(),
                web.host.port,
                web.host.username.as_str()
            ),
            ("web.example.com", 2222, "deploy")
        );
        assert_eq!(plan.group_path(web.group.unwrap()), ["Project", "Prod"]);
        assert_eq!(web.host.tags, ["prod", "web"]);
        assert_eq!(web.host.note, "two\nlines");
        assert_eq!(web.password().unwrap().expose(), "hunter2");
        assert_eq!(
            proxy_of(web),
            Some((ProxyKind::Socks5, "127.0.0.1", 1080, "u"))
        );
        assert_eq!(web.proxy_password().unwrap().expose(), "p@ss");
        // 多级跳板只导入最后一跳，跳板机在同一文件中
        let jump = web.jump.as_ref().unwrap();
        assert_eq!(jump.name, "ops@bastion.example.com");
        assert_eq!(plan.hosts[jump.host.unwrap()].host.name, "Bastion");
        assert_eq!(
            web.hints,
            [Hint::MultiHopJump {
                chain: vec![
                    "alice@gw.example.com:2200".into(),
                    "ops@bastion.example.com".into()
                ]
            }]
        );

        let v6 = by_name("v6");
        assert_eq!(v6.host.address, "2001:db8::1");
        let expected = env().expand_home("~/.ssh/id_ed25519");
        assert_eq!(
            v6.host.auth,
            Auth::Key {
                source: KeySource::file(expected)
            }
        );
        assert_eq!(v6.passphrase().unwrap().expose(), "open sesame");
        assert!(!v6.has_password(), "用私钥文件时不取密码");
        assert_eq!(v6.host.proxy, None, "direct 为直连");

        let switch = by_name("Switch");
        assert_eq!(
            (
                switch.host.protocol,
                switch.host.port,
                switch.host.username.as_str()
            ),
            (Protocol::Telnet, 2323, "admin")
        );
        assert_eq!(switch.password().unwrap().expose(), "sw-pass");
        assert!(switch.hints.is_empty(), "Telnet 不提醒缺少用户名");

        // 防公式的 `'` 去掉，密码列原样
        let calc = by_name("=calc");
        assert_eq!(calc.host.username, "-dash");
        assert_eq!(calc.password().unwrap().expose(), "'=rawpw");
        let command = calc.host.proxy.as_ref().unwrap();
        assert_eq!(
            (command.kind, command.command.as_str()),
            (ProxyKind::Command, "cloudflared access ssh --hostname %h")
        );
        assert_eq!(calc.hints, [Hint::ProxyCommandImported]);

        // 导出时没能解开的密码不导入；代理端口缺省时按默认值并提醒
        let locked = by_name("locked");
        assert!(!locked.has_password());
        assert_eq!(
            locked.hints,
            [
                Hint::SavedPasswordEncrypted,
                Hint::ProxyPortDefaulted { port: 8080 },
                Hint::MissingUsername
            ]
        );
        assert_eq!(
            proxy_of(locked),
            Some((ProxyKind::Http, "proxy.lan", 8080, ""))
        );
        assert!(plan.hosts.iter().all(|h| h.key().is_none()));
    }

    #[test]
    fn not_a_netcatty_export() {
        assert!(matches!(
            parse("name,address\nweb,10.0.0.1\n", &env()),
            Err(ImportError::NotNetcatty)
        ));
        assert!(matches!(parse("", &env()), Err(ImportError::NotNetcatty)));
        // 列名的别名、缺少的列
        let plan = parse("Host,User\n10.0.0.1,root\n,\n", &env()).unwrap();
        assert_eq!(plan.hosts.len(), 1);
        assert_eq!(plan.hosts[0].host.name, "10.0.0.1");
        assert_eq!(plan.hosts[0].host.port, 0);
    }

    #[test]
    fn default_path_picks_the_newest_export() {
        let dir = tempfile::tempdir().unwrap();
        let older = dir.path().join("hosts_export_2026-01-01.csv");
        let newer = dir.path().join("Hosts_Export_2026-02-01.CSV");
        std::fs::write(&older, "x").unwrap();
        std::fs::write(&newer, "x").unwrap();
        std::fs::write(dir.path().join("other.csv"), "x").unwrap();
        let old_time = SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&older)
            .unwrap()
            .set_modified(old_time)
            .unwrap();
        let mut env = env();
        assert_eq!(default_path(&env), None);
        env.downloads = Some(dir.path().to_path_buf());
        assert_eq!(default_path(&env), Some(newer));
    }
}
