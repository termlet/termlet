//! WindTerm（github.com/kingToolbox/WindTerm）的会话与快捷命令。
//!
//! WindTerm 只有便携版：程序目录下的 `profiles.config` 指定数据放在哪里（`{"path": "."}` 为程序目录、`"~"` 为
//! 用户目录，也可以是其他目录），其下的 `.wind/profiles/default.v10` 是配置目录：
//!
//! - `terminal/user.sessions`：会话，JSON 数组，每个会话是键名带点的扁平对象：`session.uuid`、`session.label`、
//!   `session.protocol`（SSH、Telnet、Serial；Shell 等本机会话跳过）、`session.target`（`用户@地址` 或只有地址，
//!   串口为端口名）、`session.port`、`session.group`（各级用 `>` 分隔）、`session.description`（备注）、
//!   `session.charset`（编码）、`session.autoExecution`（登录后执行的命令）、`ssh.identityFilePath.windows` 或
//!   `ssh.identityFilePath`（私钥文件）、`serial.*`（串口参数，没有的项按 WindTerm 的默认值 9600、8、None、1、None）、
//!   `session.autoLogin`（加密的登录凭据）。只保存与默认值不同的项。代理与跳板机（`session.proxy*`）、隧道
//!   （`ssh.tunnels`）的取值无法确定，不导入，提醒用户。
//! - `user.config`：`application.masterPassword`（是否设了主密码）、`application.fingerprint`（解密用的盐）。
//! - `terminal/quickbar.config`：快捷命令，JSON 数组：`quick.label`、`quick.text`、`quick.group`、`quick.type`
//!   （只导入 `Send Text`）。文本末尾的换行表示发送后执行，与 Termlet 的快捷命令相同。
//!
//! 登录凭据的加密见 WindTerm 的 `src/Utility/Cryptographic.cpp`（Apache-2.0）：密钥材料为 PBKDF2-HMAC-SHA3-512
//! （主密码的 UTF-8，盐是 fingerprint 字符串本身，10 万次，48 字节），前 32 字节是 AES-256 的密钥、后 16 字节是
//! CBC 的 IV；`session.autoLogin` 是 Base64 的密文（PKCS#7 填充），解开是 JSON：`Password`、`PasswordEnabled`、
//! `session.user`，以及 `Public Key` 中的 `windows.path`（私钥文件）与 `windows.pass`（私钥密码）。没设主密码时
//! 用空字符串加密。主密码没有校验值，只能试着解开：一条都解不开说明主密码不对；个别解不开的（旧的主密码留下的）
//! 提醒后照常导入，连接时再输入密码。登录密码与私钥密码一并导入（存进保险库）。

use std::path::{Path, PathBuf};

use aes::Aes256;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockModeDecrypt, KeyIvInit};
use serde_json::{Map, Value};
use sha3::Sha3_512;
use zeroize::Zeroizing;

use crate::model::{
    Auth, FlowControl, Host, KeySource, Parity, Protocol, SERIAL_BAUD_RANGE, SerialOptions,
    Snippet, StopBits,
};
use crate::secret::SecretString;

use super::files::{LocalEnv, MAX_CONFIG_SIZE, read_limited};
use super::plan::{Hint, PlanBuilder, PlannedHost, clip, encoding_by_name};
use super::{ImportError, ImportPlan, Source};

/// 默认的配置名（`profiles` 下的目录）。
const PROFILE: &str = "default.v10";
/// 密钥派生的迭代次数。
const ROUNDS: u32 = 100_000;
/// WindTerm 串口的默认波特率（会话中只保存与默认值不同的项）。
const DEFAULT_BAUD: u32 = 9600;
/// 登录后执行的命令最多的行数与每行的长度（与主机的校验相同）。
const MAX_STARTUP_LINES: usize = 20;
const MAX_STARTUP_LINE: usize = 1000;

/// `path` 可以是程序目录、数据目录（`.wind`）、配置目录（`profiles/default.v10`）或其中的 `user.sessions`。
/// 设了主密码时需要 `password`（没有时返回 [`ImportError::WindTermPasswordRequired`]）。
pub(super) fn read(
    path: &Path,
    env: &LocalEnv,
    password: Option<&SecretString>,
) -> Result<ImportPlan, ImportError> {
    let path = env.input_path(path);
    let profile = locate(&path, env).ok_or(ImportError::WindTermNotFound)?;
    let sessions = read_json(&profile.join("terminal").join("user.sessions"))?;
    // 设置与快捷命令可以没有
    let config = read_json(&profile.join("user.config")).ok();
    let quickbar = read_json(&profile.join("terminal").join("quickbar.config")).ok();
    parse(&sessions, config.as_ref(), quickbar.as_ref(), password)
}

/// 本机的数据目录：用户目录下的 `.wind`（程序目录中的数据需要用户选择）。
pub(super) fn default_path(env: &LocalEnv) -> Option<PathBuf> {
    let dir = env.home.as_ref()?.join(".wind");
    locate(&dir, env).map(|_| dir)
}

/// 找出配置目录（含 `terminal/user.sessions` 的目录）。
fn locate(path: &Path, env: &LocalEnv) -> Option<PathBuf> {
    let is_profile = |dir: &Path| dir.join("terminal").join("user.sessions").is_file();
    if path.is_file() {
        // 选的是 user.sessions 本身
        let dir = path.parent()?.parent()?;
        return is_profile(dir).then(|| dir.to_path_buf());
    }
    let mut roots = vec![path.to_path_buf()];
    if let Some(base) = profiles_config(path, env) {
        roots.push(base.join(".wind"));
    }
    roots.push(path.join(".wind"));
    for root in roots {
        if is_profile(&root) {
            return Some(root);
        }
        let profiles = root.join("profiles");
        if is_profile(&profiles.join(PROFILE)) {
            return Some(profiles.join(PROFILE));
        }
        let Ok(entries) = std::fs::read_dir(&profiles) else {
            continue;
        };
        let mut dirs: Vec<PathBuf> = entries.filter_map(|e| Some(e.ok()?.path())).collect();
        dirs.sort();
        if let Some(dir) = dirs.into_iter().find(|d| is_profile(d)) {
            return Some(dir);
        }
    }
    None
}

/// 程序目录中 `profiles.config` 指定的位置。
fn profiles_config(dir: &Path, env: &LocalEnv) -> Option<PathBuf> {
    let config = read_json(&dir.join("profiles.config")).ok()?;
    let path = config.get("path")?.as_str()?.trim();
    Some(match path {
        "" | "." => dir.to_path_buf(),
        "~" => env.home.clone()?,
        other => dir.join(env.expand_home(other)),
    })
}

fn read_json(path: &Path) -> Result<Value, ImportError> {
    let raw = read_limited(path, MAX_CONFIG_SIZE)?;
    let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&raw);
    serde_json::from_slice(raw).map_err(|e| ImportError::Syntax(format!("{}：{e}", path.display())))
}

/// 解析读出的内容：`sessions` 是 user.sessions，`config` 是 user.config，`quickbar` 是 quickbar.config。
pub(super) fn parse(
    sessions: &Value,
    config: Option<&Value>,
    quickbar: Option<&Value>,
    password: Option<&SecretString>,
) -> Result<ImportPlan, ImportError> {
    let Some(list) = sessions.as_array() else {
        return Err(ImportError::Syntax("user.sessions 不是会话的列表".into()));
    };
    let setting = |key: &str| config.and_then(|c| c.get(key));
    let master = setting("application.masterPassword")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let fingerprint = setting("application.fingerprint").and_then(Value::as_str);
    let logins: Vec<&str> = list
        .iter()
        .filter_map(|s| text(s, "session.autoLogin"))
        .filter(|data| plain_login(data).is_none())
        .collect();
    let cipher = match fingerprint {
        Some(fingerprint) if !logins.is_empty() => {
            let password = match (master, password) {
                (false, _) => "",
                (true, Some(password)) => password.expose(),
                (true, None) => return Err(ImportError::WindTermPasswordRequired),
            };
            Some(Cipher::new(password, fingerprint))
        }
        _ => None,
    };
    if master
        && let Some(cipher) = &cipher
        && !logins.iter().any(|data| cipher.decrypt(data).is_some())
    {
        return Err(ImportError::WindTermWrongPassword);
    }
    let mut plan = PlanBuilder::new(Source::WindTerm);
    for session in list {
        add_session(&mut plan, session, cipher.as_ref());
    }
    for item in quickbar.and_then(Value::as_array).into_iter().flatten() {
        add_quick_command(&mut plan, item);
    }
    Ok(plan.finish())
}

fn text<'a>(object: &'a Value, key: &str) -> Option<&'a str> {
    object
        .get(key)?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// 数字或写成文字的数字。
fn number(object: &Value, key: &str) -> Option<f64> {
    match object.get(key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn add_session(plan: &mut PlanBuilder, session: &Value, cipher: Option<&Cipher>) {
    let protocol = match text(session, "session.protocol").map(str::to_ascii_lowercase) {
        None => Protocol::Ssh,
        Some(p) if p == "ssh" => Protocol::Ssh,
        Some(p) if p == "telnet" => Protocol::Telnet,
        Some(p) if p == "serial" => Protocol::Serial,
        // Shell、Tmux 等本机会话
        Some(_) => {
            plan.skip(1);
            return;
        }
    };
    let target = text(session, "session.target").unwrap_or_default();
    let (username, address) = match (protocol, target.rfind('@')) {
        (Protocol::Serial, _) | (_, None) => ("", target),
        (_, Some(at)) => (&target[..at], &target[at + 1..]),
    };
    let address = address.trim();
    if address.is_empty() || (protocol == Protocol::Serial && !is_serial_port(address)) {
        plan.skip(1);
        return;
    }
    let label = text(session, "session.label").unwrap_or(address);
    let host = Host {
        name: clip(label, 128),
        protocol,
        address: address.to_string(),
        port: number(session, "session.port")
            .filter(|p| (1.0..=f64::from(u16::MAX)).contains(p) && protocol != Protocol::Serial)
            .map_or(protocol.default_port(), |p| p as u16),
        username: username.trim().to_string(),
        note: clip(
            text(session, "session.description").unwrap_or_default(),
            2000,
        ),
        ..Default::default()
    };
    let key = text(session, "session.uuid").unwrap_or(label);
    let mut item = PlannedHost::new(key, host);
    let group: Vec<&str> = text(session, "session.group")
        .map(|g| g.split('>').collect())
        .unwrap_or_default();
    item.group = plan.group(&group);
    if let Some(charset) = text(session, "session.charset") {
        item.set_encoding(encoding_by_name(charset), charset);
    }
    match protocol {
        Protocol::Ssh => {
            let key_file = text(session, "ssh.identityFilePath.windows")
                .or_else(|| text(session, "ssh.identityFilePath"));
            if let Some(path) = key_file {
                item.host.auth = Auth::Key {
                    source: KeySource::file(path),
                };
            }
            if let Some(commands) = text(session, "session.autoExecution") {
                item.host.terminal.startup = startup(commands);
            }
            let tunnels = text(session, "ssh.tunnels").map_or(0, |t| {
                t.split(';').filter(|rule| !rule.trim().is_empty()).count()
            });
            if tunnels > 0 {
                item.hints
                    .push(Hint::ForwardsNotImported { count: tunnels });
            }
        }
        Protocol::Telnet => {}
        Protocol::Serial => item.host.serial = serial_options(session, &mut item.hints),
    }
    if protocol != Protocol::Serial {
        add_proxy_hint(&mut item, session);
        if let Some(data) = text(session, "session.autoLogin") {
            let login = plain_login(data).or_else(|| cipher?.decrypt(data));
            match login {
                Some(login) => apply_login(&mut item, &login),
                None => item.hints.push(Hint::SavedPasswordUnreadable),
            }
        }
    }
    if protocol == Protocol::Ssh && item.host.username.is_empty() {
        item.hints.push(Hint::MissingUsername);
    }
    plan.push(item);
}

/// 代理与跳板机不导入：设置了跳板机（`session.proxy` 为 JumpServers 等、`session.proxyJumpServer<n>`）时提醒跳板机，
/// 设置了其他代理时提醒代理不支持。
fn add_proxy_hint(item: &mut PlannedHost, session: &Value) {
    let proxy = text(session, "session.proxy").filter(|p| !p.eq_ignore_ascii_case("none"));
    let jump_servers =
        (1..=4).any(|n| text(session, &format!("session.proxyJumpServer{n}")).is_some());
    if jump_servers || proxy.is_some_and(|p| p.to_ascii_lowercase().contains("jump")) {
        item.hints.push(Hint::JumpNotImported);
    } else if proxy.is_some() {
        item.hints.push(Hint::UnsupportedProxy);
    }
}

/// 明文保存的凭据（JSON 对象），作为兜底。
fn plain_login(data: &str) -> Option<Map<String, Value>> {
    match serde_json::from_str(data).ok()? {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

/// 解开的凭据：用户名（会话的目标中没有时）、登录密码（`PasswordEnabled` 不为 false 时）、私钥文件与它的密码。
fn apply_login(item: &mut PlannedHost, login: &Map<String, Value>) {
    let field = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    if item.host.username.is_empty()
        && let Some(user) = field(login.get("session.user"))
    {
        item.host.username = user;
    }
    let enabled = login
        .get("PasswordEnabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if enabled && let Some(password) = login.get("Password").and_then(Value::as_str) {
        item.set_password(SecretString::new(password));
    }
    if item.host.protocol != Protocol::Ssh {
        return;
    }
    let key = login.get("Public Key");
    if let Some(path) = field(key.and_then(|k| k.get("windows.path"))) {
        if !matches!(item.host.auth, Auth::Key { .. }) {
            item.host.auth = Auth::Key {
                source: KeySource::file(path),
            };
        }
        if let Some(passphrase) = key
            .and_then(|k| k.get("windows.pass"))
            .and_then(Value::as_str)
        {
            item.set_passphrase(SecretString::new(passphrase));
        }
    }
}

/// 登录后执行的命令：去掉控制字符，限制行数与每行的长度（超出的部分不导入）。
fn startup(commands: &str) -> String {
    commands
        .lines()
        .map(|line| {
            let line: String = line.chars().filter(|c| !c.is_control()).collect();
            clip(line.trim_end(), MAX_STARTUP_LINE)
        })
        .filter(|line| !line.trim().is_empty())
        .take(MAX_STARTUP_LINES)
        .collect::<Vec<_>>()
        .join("\n")
}

/// 串口参数；认不出的取值按默认导入并提醒核对。
fn serial_options(session: &Value, hints: &mut Vec<Hint>) -> SerialOptions {
    let mut unknown = false;
    let defaults = SerialOptions::default();
    let baud = match number(session, "serial.baudRate") {
        None => DEFAULT_BAUD,
        Some(b) if b.fract() == 0.0 && SERIAL_BAUD_RANGE.contains(&(b as u32)) => b as u32,
        Some(_) => {
            unknown = true;
            DEFAULT_BAUD
        }
    };
    let data_bits = match number(session, "serial.dataBits") {
        None => 8,
        Some(b) if (5.0..=8.0).contains(&b) && b.fract() == 0.0 => b as u8,
        Some(_) => {
            unknown = true;
            8
        }
    };
    let lower = |key: &str| text(session, key).map(str::to_ascii_lowercase);
    let parity = match lower("serial.parity").as_deref() {
        None | Some("none") => Parity::None,
        Some("odd") => Parity::Odd,
        Some("even") => Parity::Even,
        Some("mark") => Parity::Mark,
        Some("space") => Parity::Space,
        Some(_) => {
            unknown = true;
            Parity::None
        }
    };
    let stop_bits = match number(session, "serial.stopBits") {
        None => StopBits::One,
        Some(1.0) => StopBits::One,
        Some(1.5) => StopBits::OnePointFive,
        Some(2.0) => StopBits::Two,
        Some(_) => {
            unknown = true;
            StopBits::One
        }
    };
    let flow_control = match lower("serial.flowControl") {
        None => FlowControl::None,
        Some(flow) if flow == "none" => FlowControl::None,
        Some(flow) if flow.contains("rts") => FlowControl::RtsCts,
        Some(flow) if flow.contains("xon") => FlowControl::XonXoff,
        Some(_) => {
            unknown = true;
            FlowControl::None
        }
    };
    if unknown {
        hints.push(Hint::SerialSettingsUnverified);
    }
    SerialOptions {
        baud,
        data_bits,
        parity,
        stop_bits,
        flow_control,
        ..defaults
    }
}

/// 串口名（与保存主机时的校验相同：`COM3`、`/dev/ttyUSB0` 等）。
fn is_serial_port(name: &str) -> bool {
    name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '\\' | ':' | '-'))
}

/// 一条快捷命令：只导入发送文字的（`Send Text`），没有内容的跳过。
fn add_quick_command(plan: &mut PlanBuilder, item: &Value) {
    if text(item, "quick.type").is_some_and(|t| !t.eq_ignore_ascii_case("Send Text")) {
        return;
    }
    // 内容不去掉首尾空白：末尾的换行表示发送后执行
    let Some(content) = item.get("quick.text").and_then(Value::as_str) else {
        return;
    };
    if content.trim().is_empty() || content.chars().count() > 64 * 1024 {
        return;
    }
    plan.add_snippet(Snippet {
        name: clip(text(item, "quick.label").unwrap_or_default(), 128),
        content: content.to_string(),
        group: clip(text(item, "quick.group").unwrap_or_default(), 64),
        ..Default::default()
    });
}

/// 解开 `session.autoLogin` 用的密钥与 IV（见模块说明）。
struct Cipher(Zeroizing<[u8; 48]>);

impl Cipher {
    fn new(password: &str, fingerprint: &str) -> Self {
        let mut material = Zeroizing::new([0u8; 48]);
        pbkdf2::pbkdf2_hmac::<Sha3_512>(
            password.as_bytes(),
            fingerprint.as_bytes(),
            ROUNDS,
            &mut material[..],
        );
        Self(material)
    }

    /// 解开一条凭据；不是 Base64、密钥不对（填充不对）、解出的不是 JSON 对象时为 `None`。
    fn decrypt(&self, data: &str) -> Option<Map<String, Value>> {
        let mut buf = Zeroizing::new(data_encoding::BASE64.decode(data.trim().as_bytes()).ok()?);
        let (key, iv) = self.0.split_at(32);
        let decryptor = cbc::Decryptor::<Aes256>::new_from_slices(key, iv).ok()?;
        let plain = decryptor.decrypt_padded::<Pkcs7>(&mut buf[..]).ok()?;
        match serde_json::from_slice(plain).ok()? {
            Value::Object(map) => Some(map),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::import::files::tests::{env, write};
    use crate::model::Encoding;

    /// 测试数据中的 fingerprint 与密文由 Python 的 hashlib.pbkdf2_hmac('sha3_512', …) 与 cryptography 的
    /// AES-256-CBC（PKCS#7）生成，与这里的实现互相独立。
    const FINGERPRINT: &str =
        "rH1oBgV4xqPaj7vD/rVWYOpULez+HT+Ni2PTS8HMD7ecRpdktifOTfzhha6+NcBaOfW9eqV9+tyETaz/u3wH4g==";
    const MASTER: &str = "主密码 pw";
    /// 主密码 MASTER：{"PasswordEnabled":true,"Password":"p@ss 密码","session.user":"root"}
    const LOGIN_PASSWORD: &str = "CZkFrey3x1GLLGAtz4CMDRwO1W/i2s+u7p3FgLLk3CRnFrnVzR6zR6v3QtjW+wjTd9o8dLrlnTR4sgJIh+dEUesw1ujpCvPiIp+qnqlSDLo=";
    /// 主密码 MASTER：{"Public Key":{"windows.path":"C:\\keys\\id_ed25519","windows.pass":"kp"},"session.user":"deploy"}
    const LOGIN_KEY: &str = "SnenBNFsIeRAGhjXRPuB9C2JSHfgxx8k50ddlHaghlObiXwvz+O1oCE3YDngFnLW9qwicMdp0YTWwZZny23n9X8zqizIVZ8/T7JzzpZ4ynLAVoXjtmvDtWwbVGSjNBu6BepHRJs5F/kMOxJeiRynFQ==";
    /// 主密码 MASTER：{"PasswordEnabled":false,"Password":"old"}
    const LOGIN_DISABLED: &str = "CZkFrey3x1GLLGAtz4CMDQ3ssnWCiWgAF1IKCENRXsTeDLw7h2kOySbVG5pUspAT";
    /// 旧的主密码"旧密码"：{"PasswordEnabled":true,"Password":"x"}
    const LOGIN_OLD: &str = "M7/nHqPdAgZ7LSOGpo0Jjcev9QYVf9iMvUKcXmZJfMizl4EFx5Q2k8GOQgyiV/Us";
    /// 没设主密码（空字符串）：{"PasswordEnabled":true,"Password":"telnet-pw","session.user":"admin"}
    const LOGIN_EMPTY: &str = "hmhajfQQY0gzwM/QoG9ufnOw4D0+fLy3mBfiCdpPdpLWAIVKz2WkkXT6KalKzPMKLMydiirDz1EvUr1FjTMqFi8F7aAs/vI0afomgKdjqOU=";

    fn config(master: bool) -> Value {
        json!({
            "application.fingerprint": FINGERPRINT,
            "application.masterPassword": master,
        })
    }

    fn secret(s: &str) -> SecretString {
        SecretString::new(s)
    }

    fn key_file(h: &PlannedHost) -> Option<&str> {
        match &h.host.auth {
            Auth::Key {
                source: KeySource::File { path, .. },
            } => Some(path),
            _ => None,
        }
    }

    fn sessions() -> Value {
        json!([
            {
                "session.uuid": "u-web",
                "session.label": "订单库",
                "session.protocol": "SSH",
                "session.target": "root@10.0.0.1",
                "session.port": 2222,
                "session.group": "生产>数据库",
                "session.description": "主库",
                "session.charset": "GBK",
                "session.autoExecution": "cd /srv\n\nls -la\u{1b}",
                "ssh.tunnels": "proxy:Dynamic(localhost),19999,Any;web:Local,8080,Any;",
                "session.autoLogin": LOGIN_PASSWORD,
            },
            {
                "session.uuid": "u-key",
                "session.label": "部署",
                "session.target": "10.0.0.2",
                "session.autoLogin": LOGIN_KEY,
            },
            {
                "session.uuid": "u-telnet",
                "session.label": "交换机",
                "session.protocol": "Telnet",
                "session.target": "admin@10.0.0.70",
                "session.autoLogin": LOGIN_DISABLED,
            },
            {
                "session.uuid": "u-serial",
                "session.label": "控制台",
                "session.protocol": "Serial",
                "session.target": "COM3",
                "serial.baudRate": 115200,
                "serial.parity": "Even",
                "serial.stopBits": 2,
                "serial.flowControl": "RTS/CTS",
            },
            { "session.label": "本机", "session.protocol": "Shell", "session.target": "cmd" },
            { "session.label": "没有地址", "session.protocol": "SSH" },
            {
                "session.uuid": "u-old",
                "session.label": "旧密码",
                "session.target": "u@10.0.0.3",
                "ssh.identityFilePath.windows": "C:\\keys\\old",
                "ssh.identityFilePath": "/home/u/.ssh/old",
                "session.proxy": "JumpServers",
                "session.autoLogin": LOGIN_OLD,
            },
            {
                "session.uuid": "u-proxy",
                "session.label": "代理",
                "session.target": "u@10.0.0.4",
                "session.proxy": "Socks5",
                "session.proxyHost": "127.0.0.1",
            },
        ])
    }

    #[test]
    fn master_password_is_required_and_checked() {
        let sessions = sessions();
        let config = config(true);
        assert!(matches!(
            parse(&sessions, Some(&config), None, None),
            Err(ImportError::WindTermPasswordRequired)
        ));
        assert!(matches!(
            parse(&sessions, Some(&config), None, Some(&secret("不对"))),
            Err(ImportError::WindTermWrongPassword)
        ));
    }

    #[test]
    fn sessions_with_the_master_password() {
        let plan = parse(
            &sessions(),
            Some(&config(true)),
            None,
            Some(&secret(MASTER)),
        )
        .unwrap();
        assert_eq!(plan.source, Source::WindTerm);
        assert_eq!(plan.skipped, 2, "本机会话与没有地址的会话跳过");
        let find = |key: &str| plan.hosts.iter().find(|h| h.key == key).unwrap();

        let web = find("u-web");
        assert_eq!(
            (
                web.host.name.as_str(),
                web.host.protocol,
                web.host.address.as_str(),
                web.host.port,
                web.host.username.as_str(),
                web.host.note.as_str(),
                web.host.terminal.encoding,
                web.host.terminal.startup.as_str(),
            ),
            (
                "订单库",
                Protocol::Ssh,
                "10.0.0.1",
                2222,
                "root",
                "主库",
                Encoding::Gb18030,
                "cd /srv\nls -la",
            )
        );
        assert_eq!(plan.group_path(web.group.unwrap()), ["生产", "数据库"]);
        assert_eq!(web.password().unwrap().expose(), "p@ss 密码");
        assert_eq!(web.hints, [Hint::ForwardsNotImported { count: 2 }]);

        // 用户名、私钥文件与它的密码在加密的凭据中
        let key = find("u-key");
        assert_eq!(
            (key.host.username.as_str(), key.host.port, key_file(key)),
            ("deploy", 22, Some(r"C:\keys\id_ed25519"))
        );
        assert_eq!(key.passphrase().unwrap().expose(), "kp");
        assert!(!key.has_password() && key.hints.is_empty());

        // 关闭了密码的不导入密码；Telnet 没有用户名也不提醒
        let telnet = find("u-telnet");
        assert_eq!(
            (
                telnet.host.protocol,
                telnet.host.port,
                telnet.host.username.as_str()
            ),
            (Protocol::Telnet, 23, "admin")
        );
        assert!(!telnet.has_password() && telnet.hints.is_empty());

        let serial = find("u-serial");
        assert_eq!(
            (
                serial.host.protocol,
                serial.host.address.as_str(),
                serial.host.port
            ),
            (Protocol::Serial, "COM3", 0)
        );
        assert_eq!(
            serial.host.serial,
            SerialOptions {
                baud: 115200,
                parity: Parity::Even,
                stop_bits: StopBits::Two,
                flow_control: FlowControl::RtsCts,
                ..Default::default()
            }
        );
        assert!(serial.hints.is_empty());

        // 旧的主密码留下的凭据解不开：提醒，照常导入；Windows 的私钥路径优先
        let old = find("u-old");
        assert_eq!(key_file(old), Some(r"C:\keys\old"));
        assert!(!old.has_password());
        assert_eq!(
            old.hints,
            [Hint::JumpNotImported, Hint::SavedPasswordUnreadable]
        );
        let proxy = find("u-proxy");
        assert_eq!(proxy.hints, [Hint::UnsupportedProxy]);
        assert!(proxy.host.proxy.is_none());
    }

    #[test]
    fn without_a_master_password_the_empty_password_is_used() {
        let sessions = json!([
            {
                "session.uuid": "t",
                "session.protocol": "telnet",
                "session.target": "10.0.0.70",
                "session.port": "2323",
                "session.autoLogin": LOGIN_EMPTY,
            },
            // 明文保存的凭据也认
            {
                "session.uuid": "p",
                "session.target": "10.0.0.8",
                "session.autoLogin": "{\"Password\":\"plain\",\"session.user\":\"ops\"}",
            },
        ]);
        let plan = parse(&sessions, Some(&config(false)), None, None).unwrap();
        let telnet = &plan.hosts[0];
        assert_eq!(
            (
                telnet.host.name.as_str(),
                telnet.host.port,
                telnet.host.username.as_str()
            ),
            ("10.0.0.70", 2323, "admin")
        );
        assert_eq!(telnet.password().unwrap().expose(), "telnet-pw");
        let plain = &plan.hosts[1];
        assert_eq!(plain.host.username, "ops");
        assert_eq!(plain.password().unwrap().expose(), "plain");
        // 没有 fingerprint 时解不开，提醒后照常导入
        let plan = parse(&sessions, None, None, None).unwrap();
        assert_eq!(plan.hosts[0].hints, [Hint::SavedPasswordUnreadable]);
        assert!(!plan.hosts[0].has_password());
        assert!(plan.hosts[1].has_password());
        assert!(matches!(
            parse(&json!({}), None, None, None),
            Err(ImportError::Syntax(_))
        ));
    }

    #[test]
    fn serial_defaults_and_unknown_values() {
        let serial = |extra: Value| {
            let mut session = json!({ "session.protocol": "Serial", "session.target": "COM7" });
            session
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let plan = parse(&json!([session]), None, None, None).unwrap();
            let host = &plan.hosts[0];
            (host.host.serial.clone(), host.hints.clone())
        };
        // 没有的项按 WindTerm 的默认值
        assert_eq!(
            serial(json!({})),
            (
                SerialOptions {
                    baud: 9600,
                    ..Default::default()
                },
                vec![]
            )
        );
        let (options, hints) = serial(json!({
            "serial.baudRate": "57600",
            "serial.dataBits": 7,
            "serial.stopBits": 1.5,
            "serial.flowControl": "XON/XOFF",
            "serial.parity": "Space",
        }));
        assert_eq!(
            (
                options.baud,
                options.data_bits,
                options.stop_bits,
                options.flow_control,
                options.parity
            ),
            (
                57600,
                7,
                StopBits::OnePointFive,
                FlowControl::XonXoff,
                Parity::Space
            )
        );
        assert!(hints.is_empty());
        let (options, hints) = serial(json!({
            "serial.baudRate": 3,
            "serial.parity": "奇",
            "serial.flowControl": "DSR/DTR",
        }));
        assert_eq!(
            (options.baud, options.parity, options.flow_control),
            (9600, Parity::None, FlowControl::None)
        );
        assert_eq!(hints, [Hint::SerialSettingsUnverified]);
        // 不像串口名的跳过
        let plan = parse(
            &json!([{ "session.protocol": "Serial", "session.target": "COM 3; x" }]),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!((plan.hosts.len(), plan.skipped), (0, 1));
    }

    #[test]
    fn quick_commands_become_snippets() {
        let quickbar = json!([
            { "quick.label": "重启", "quick.text": "systemctl restart app\n", "quick.group": "运维", "quick.type": "Send Text" },
            { "quick.label": "看日志", "quick.text": "tail -f /var/log/syslog" },
            { "quick.label": "其他", "quick.text": "x", "quick.type": "Run Script" },
            { "quick.label": "空的", "quick.text": "  " },
        ]);
        let plan = parse(&json!([]), None, Some(&quickbar), None).unwrap();
        let snippets: Vec<_> = plan
            .snippets
            .iter()
            .map(|s| (s.name.as_str(), s.content.as_str(), s.group.as_str()))
            .collect();
        assert_eq!(
            snippets,
            [
                ("重启", "systemctl restart app\n", "运维"),
                ("看日志", "tail -f /var/log/syslog", ""),
            ]
        );
    }

    #[test]
    fn finds_the_profile_directory() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("WindTerm_2.7.0");
        let profile = app.join(".wind").join("profiles").join("default.v10");
        let sessions = br#"[{"session.uuid":"a","session.target":"u@10.0.0.9"}]"#;
        write(&profile.join("terminal").join("user.sessions"), sessions);
        write(
            &profile.join("terminal").join("quickbar.config"),
            br#"[{"quick.label":"ls","quick.text":"ls\n"}]"#,
        );
        write(&app.join("profiles.config"), br#"{ "path" : "." }"#);
        let env = env();
        for path in [
            app.clone(),
            app.join(".wind"),
            profile.clone(),
            profile.join("terminal").join("user.sessions"),
        ] {
            let plan = read(&path, &env, None).unwrap();
            assert_eq!(
                (plan.hosts.len(), plan.snippets.len()),
                (1, 1),
                "{}",
                path.display()
            );
        }
        // profiles.config 指向别处；配置名不是 default.v10 时也能找到
        let other = dir.path().join("data");
        write(
            &other
                .join(".wind")
                .join("profiles")
                .join("work")
                .join("terminal")
                .join("user.sessions"),
            sessions,
        );
        let portable = dir.path().join("portable");
        let target = other.display().to_string().replace('\\', "\\\\");
        write(
            &portable.join("profiles.config"),
            format!("{{\"path\":\"{target}\"}}").as_bytes(),
        );
        assert_eq!(read(&portable, &env, None).unwrap().hosts.len(), 1);
        assert!(matches!(
            read(&dir.path().join("missing"), &env, None),
            Err(ImportError::WindTermNotFound)
        ));
        // 本机的默认位置：用户目录下的 .wind
        let mut env = env;
        env.home = Some(app.clone());
        assert_eq!(default_path(&env), Some(app.join(".wind")));
        env.home = Some(dir.path().join("nobody"));
        assert_eq!(default_path(&env), None);
    }
}
