//! ShellRS（github.com/since2006/shell-rs）的主机：本机数据目录中的 `shellrs.db`，或开启了云同步时的同步文件夹。
//!
//! 格式按 ShellRS 提交 98d471c（0.1.19 之后）的源码，只看格式、不抄代码：数据库的结构在 `crates/shellrs-hosts/src/
//! database.rs` 的说明与建表语句中，同步的格式在 `crates/shellrs-sync/FORMAT.md`。这两个 crate 是 MIT（ShellRS 的
//! 桌面程序是 GPL-3.0-only，没有参考）。
//!
//! **数据目录**（[`db`]）：`dirs::data_dir()` 下的 `shellrs`（Windows `%APPDATA%\shellrs`、macOS
//! `~/Library/Application Support/shellrs`、Linux `~/.local/share/shellrs`），便携版是程序旁的 `data`，环境变量
//! `SHELLRS_DATA_DIR` 可以指定。`shellrs.db` 中有分组、凭据、主机、跳板、端口转发、命令片段，**没有秘密**：密码、
//! 私钥密码、验证码密钥都在系统钥匙串（服务名 `shellrs`）中。Termlet 不替它去钥匙串取，那是替别的程序取它的秘密；
//! 要秘密就选同步文件夹。`settings.json` 中的同步位置是本机文件夹时，预览中提示可以改选它。
//!
//! **同步文件夹**（[`sync`]）：用户输入 ShellRS 的同步密码后解开，其中除了串口主机，还有登录密码、代理密码、凭据的
//! 密码与私钥密码、私钥本身、验证码密钥。输入密码即表示同意导入其中的秘密（与 WindTerm 的主密码相同）。同步数据是各设备
//! 最后一次同步时的内容。
//!
//! **转换**（两种来源先读成同一组数据，再转成导入计划）：
//! - 分组可嵌套；SSH、Telnet、串口主机（串口只在数据库中有）。
//! - 登录：密码；“无密码”（ShellRS 依次试免认证、SSH Agent、`~/.ssh` 的默认私钥）导入为 ssh-agent 并提醒；
//!   凭据导入成凭据档案（密码、私钥、SSH Agent），主机自己的用户名（`credential_username`）优先。凭据的私钥存进密钥库
//!   （同步文件夹中的私钥，或数据目录中不加密的私钥文件）；设了私钥密码的私钥文件按路径引用并提醒。Telnet 主机不能用
//!   档案：用的密码凭据展开成主机自己的用户名与密码。
//! - 连接方式：多级跳板只导入最后一跳（ShellRS 中跳板主机自己的连接方式在这里不生效，Termlet 是逐级向上；两者一致时
//!   不提醒）；HTTP、SOCKS5 代理。
//! - 验证码、登录后执行的命令、备注；端口转发（导入后不自动启动；本地、动态转发监听网卡地址时改为回环地址并提醒）；
//!   命令片段（嵌套的分类写成“上级 / 下级”，点击时执行的末尾补换行）。
//! - 不导入：SFTP 书签、“项目”中的本地终端、关键字高亮、主题、主机图标；Telnet 自定义的登录提示、验证码的识别文本
//!   （提醒）。

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use crate::import::files::{LocalEnv, read_limited};
use crate::import::plan::{
    Endpoint, Hint, JumpRef, KeyAlgorithm, PlanBuilder, PlanNote, PlannedHost, PlannedKey,
    PlannedProfile, PlannedProfileAuth, clip,
};
use crate::import::{ImportError, ImportPlan, Source};
use crate::model::{
    Auth, FlowControl, Forward, ForwardKind, Host, Parity, Protocol, Proxy, ProxyKind,
    SerialOptions, Snippet, StopBits,
};
use crate::secret::SecretString;
use crate::snippet_params::snippet_params;

mod db;
mod sync;

#[cfg(test)]
mod tests;

/// 私钥文件的大小上限
const MAX_KEY_FILE: u64 = 1 << 20;
/// 分组、片段分类的层数上限
const MAX_DEPTH: usize = 32;
/// 片段分类在 Termlet 中合成一层名称时的分隔
const GROUP_SEPARATOR: &str = " / ";
/// 快捷命令内容的长度上限（字符数，与存储相同）
const MAX_SNIPPET: usize = 64 * 1024;
/// 登录后执行的命令：行数与每行的长度上限（与存储相同）
const MAX_STARTUP_LINES: usize = 20;
const MAX_STARTUP_LINE: usize = 1000;

/// 读取 `path`：同步文件夹（要 ShellRS 的同步密码 `password`，没有时返回 [`ImportError::PasswordRequired`]），
/// 或数据目录、其中的 `shellrs.db`。
pub(in crate::import) fn read(
    path: &Path,
    env: &LocalEnv,
    password: Option<&SecretString>,
) -> Result<ImportPlan, ImportError> {
    let path = env.input_path(path);
    let data = if let Some(store) = sync::locate(&path) {
        let password = password.ok_or(ImportError::PasswordRequired)?;
        sync::read(&store, password)?
    } else {
        let file = db::locate(&path).ok_or_else(|| {
            ImportError::NotShellRs("目录中没有 shellrs.db，也不是同步文件夹".into())
        })?;
        db::read(&file)?
    };
    Ok(Converter::new(&data, env).run())
}

/// 默认位置：环境变量 `SHELLRS_DATA_DIR` 指定的目录，或系统的数据目录下的 `shellrs`（其中有 `shellrs.db` 时）。
pub(in crate::import) fn default_path(env: &LocalEnv) -> Option<PathBuf> {
    env.shellrs_dir
        .iter()
        .cloned()
        .chain(env.data_dir.as_ref().map(|d| d.join("shellrs")))
        .find(|dir| dir.join(db::DATABASE_FILE).is_file())
}

/// 读出的数据，两种来源相同。ID 都是字符串（数据库中的整数 ID 也转成字符串），各种记录的 ID 互不相干。
#[derive(Default)]
struct Data {
    groups: HashMap<String, Group>,
    credentials: HashMap<String, Credential>,
    hosts: Vec<HostRecord>,
    forwards: Vec<ForwardRecord>,
    categories: HashMap<String, Category>,
    snippets: Vec<SnippetRecord>,
    secrets: Secrets,
    notes: Vec<PlanNote>,
    /// 跳过的记录（不认识的协议、缺字段的记录、读不了的设备文件）
    skipped: usize,
}

struct Group {
    name: String,
    parent: Option<String>,
    sort_order: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CredentialKind {
    Password,
    Key,
    Agent,
}

struct Credential {
    name: String,
    kind: CredentialKind,
    username: String,
    /// 私钥文件（只有数据库中有；同步中的私钥在秘密里）
    key_path: Option<String>,
}

struct HostRecord {
    id: String,
    /// 预览中的键（数据库中的 `public_id`，同步记录的 ID）
    key: String,
    name: String,
    address: String,
    port: u16,
    group: Option<String>,
    sort_order: i64,
    protocol: Protocol,
    auth: Login,
    username: String,
    /// 用凭据时以这个用户名登录（不用凭据的用户名）
    credential_username: Option<String>,
    route: Route,
    notes: String,
    /// Telnet 自定义了登录或密码提示
    telnet_prompts: bool,
    /// 开了验证码自动填写：`Some(识别文本)`，识别文本为空表示常见的说法
    totp: Option<String>,
    login_commands: String,
    serial: Option<SerialOptions>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Login {
    Password,
    NoPassword,
    Credential(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Route {
    Direct,
    /// 跳板主机的 ID，第一跳在前；`None` 是已删除的主机
    Jump(Vec<Option<String>>),
    Proxy {
        kind: ProxyKind,
        host: String,
        port: u16,
        username: String,
    },
}

struct ForwardRecord {
    host: String,
    name: String,
    kind: ForwardKind,
    bind_host: String,
    bind_port: u16,
    target_host: String,
    target_port: u16,
    auto_start: bool,
    sort_order: i64,
}

struct Category {
    name: String,
    parent: Option<String>,
    sort_order: i64,
}

struct SnippetRecord {
    category: Option<String>,
    name: String,
    command: String,
    run_on_click: bool,
    sort_order: i64,
}

/// 同步文件夹中的秘密（数据库中没有）。
#[derive(Default)]
struct Secrets {
    /// （用户名、地址（小写）、端口）→ 登录密码，同一地址、端口、用户名的主机共用
    passwords: HashMap<(String, String, u16), SecretString>,
    /// 同上，代理的密码（按代理的用户名、地址、端口）
    proxies: HashMap<(String, String, u16), SecretString>,
    /// 凭据 ID → 密码凭据的密码
    credential_passwords: HashMap<String, SecretString>,
    /// 凭据 ID → 私钥密码
    passphrases: HashMap<String, SecretString>,
    /// 凭据 ID → 私钥文件的全文
    private_keys: HashMap<String, SecretString>,
    /// 主机 ID → 验证码的密钥
    totp: HashMap<String, SecretString>,
}

fn endpoint_key(user: &str, host: &str, port: u16) -> (String, String, u16) {
    (user.to_string(), host.trim().to_lowercase(), port)
}

impl Secrets {
    fn password(&self, user: &str, host: &str, port: u16) -> Option<&SecretString> {
        self.passwords.get(&endpoint_key(user, host, port))
    }

    fn proxy(&self, user: &str, host: &str, port: u16) -> Option<&SecretString> {
        self.proxies.get(&endpoint_key(user, host, port))
    }
}

/// 一台跳板主机在计划中的样子：名称、终点（地址、端口、实际的用户名）与它自己的连接方式。
struct JumpTarget<'a> {
    name: &'a str,
    endpoint: Endpoint,
    route: &'a Route,
}

/// 把读出的数据转成导入计划。
struct Converter<'a> {
    data: &'a Data,
    env: &'a LocalEnv,
    plan: PlanBuilder,
    /// 凭据 ID → 计划中的档案下标与用它的主机要带上的提醒
    profiles: HashMap<String, (usize, Vec<Hint>)>,
    /// 计划中已用的档案（名称、用户名）：ShellRS 中名称相同的两条凭据，后一条的名称加序号
    profile_names: HashSet<(String, String)>,
    /// 私钥指纹 → 密钥库中的下标（同一把私钥只存一次）
    fingerprints: HashMap<String, usize>,
    /// 主机 ID → 计划中的下标
    hosts: HashMap<&'a str, usize>,
}

impl<'a> Converter<'a> {
    fn new(data: &'a Data, env: &'a LocalEnv) -> Self {
        Self {
            data,
            env,
            plan: PlanBuilder::new(Source::ShellRs),
            profiles: HashMap::new(),
            profile_names: HashSet::new(),
            fingerprints: HashMap::new(),
            hosts: HashMap::new(),
        }
    }

    fn run(mut self) -> ImportPlan {
        let data = self.data;
        // 跳板主机：按 ID 找到它的名称、终点与它自己的连接方式（只有 SSH 主机能当跳板）
        let targets: HashMap<&str, JumpTarget> = data
            .hosts
            .iter()
            .filter(|h| h.protocol == Protocol::Ssh && !h.address.trim().is_empty())
            .map(|h| {
                let name = if h.name.trim().is_empty() {
                    h.address.trim()
                } else {
                    h.name.trim()
                };
                let target = JumpTarget {
                    name,
                    endpoint: Endpoint::new(&h.address, h.port, &self.username_of(h)),
                    route: &h.route,
                };
                (h.id.as_str(), target)
            })
            .collect();
        // 按 ShellRS 列表中的顺序：同一层中分组在前，分组、主机各按自己的顺序
        let mut hosts: Vec<(Vec<(u8, i64, String)>, &HostRecord)> = data
            .hosts
            .iter()
            .map(|h| {
                let mut order = self.group_order(h.group.as_deref());
                order.push((1, h.sort_order, h.name.to_lowercase()));
                (order, h)
            })
            .collect();
        hosts.sort_by(|a, b| a.0.cmp(&b.0));
        for (_, h) in hosts {
            match self.host(h, &targets) {
                Some(item) => {
                    let index = self.plan.push(item);
                    self.hosts.insert(&h.id, index);
                }
                None => self.plan.skip(1),
            }
        }
        self.forwards();
        self.snippets();
        for note in &data.notes {
            self.plan.note(note.clone());
        }
        self.plan.skip(data.skipped);
        self.plan.finish()
    }

    /// 主机实际登录用的用户名：用凭据时是 `credential_username`，没有时是凭据的。
    fn username_of(&self, h: &HostRecord) -> String {
        match &h.auth {
            Login::Credential(id) => h
                .credential_username
                .clone()
                .filter(|u| !u.trim().is_empty())
                .or_else(|| self.data.credentials.get(id).map(|c| c.username.clone()))
                .unwrap_or_default(),
            _ => h.username.clone(),
        }
    }

    /// 分组从根开始的各级（名称，上级）；上级不存在、成环或超过层数时停在那里。
    fn group_chain(&self, id: Option<&str>) -> Vec<&'a Group> {
        let groups = &self.data.groups;
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut current = id;
        while let Some(id) = current {
            if !seen.insert(id) || chain.len() >= MAX_DEPTH {
                break;
            }
            let Some(group) = groups.get(id) else {
                break;
            };
            chain.push(group);
            current = group.parent.as_deref();
        }
        chain.reverse();
        chain
    }

    /// 排序用：各级分组的（0，顺序，名称）。
    fn group_order(&self, id: Option<&str>) -> Vec<(u8, i64, String)> {
        self.group_chain(id)
            .into_iter()
            .map(|g| (0, g.sort_order, g.name.to_lowercase()))
            .collect()
    }

    /// 一台主机；没有地址时为 `None`（跳过并计数）。
    fn host(
        &mut self,
        h: &'a HostRecord,
        targets: &HashMap<&str, JumpTarget>,
    ) -> Option<PlannedHost> {
        let address = h.address.trim();
        if address.is_empty() {
            return None;
        }
        let name = match h.name.trim() {
            "" => address,
            name => name,
        };
        let host = Host {
            name: clip(name, 128),
            protocol: h.protocol,
            address: address.to_string(),
            port: if h.protocol == Protocol::Serial {
                0
            } else {
                h.port
            },
            note: clip(h.notes.trim(), 2000),
            ..Default::default()
        };
        let mut item = PlannedHost::new(h.key.clone(), host);
        let path: Vec<&str> = self
            .group_chain(h.group.as_deref())
            .into_iter()
            .map(|g| g.name.as_str())
            .collect();
        item.group = self.plan.group(&path);
        match h.protocol {
            Protocol::Ssh => self.ssh(h, &mut item),
            Protocol::Telnet => self.telnet(h, &mut item),
            Protocol::Serial => {
                item.host.serial = h.serial.clone().unwrap_or_default();
                return Some(item);
            }
        }
        self.route(h, &mut item, targets);
        Some(item)
    }

    fn ssh(&mut self, h: &HostRecord, item: &mut PlannedHost) {
        let secrets = &self.data.secrets;
        match &h.auth {
            Login::Password => {
                item.host.username = h.username.clone();
                if let Some(password) = secrets.password(&h.username, &h.address, h.port) {
                    item.set_password(password.clone());
                }
            }
            Login::NoPassword => {
                item.host.username = h.username.clone();
                item.host.auth = Auth::Agent;
                item.hints.push(Hint::NoPasswordLogin);
            }
            // 凭据已删除时与 ShellRS 合并后的修补相同：密码登录、没有用户名
            Login::Credential(id) => {
                if let Some(credential) = self.data.credentials.get(id) {
                    let (index, hints) = self.profile(id, credential);
                    item.host.username = h.credential_username.clone().unwrap_or_default();
                    item.set_profile(index, &credential.username);
                    item.hints.extend(hints);
                }
            }
        }
        if let Some(prompt) = &h.totp {
            match secrets.totp.get(&h.id) {
                Some(secret) if crate::totp::Totp::parse(secret.expose()).is_ok() => {
                    item.set_totp(secret.clone());
                }
                _ => item.hints.push(Hint::TotpSecretMissing),
            }
            if !prompt.trim().is_empty() {
                item.hints.push(Hint::TotpPromptIgnored);
            }
        }
        item.host.terminal.startup = startup(&h.login_commands);
    }

    /// Telnet：自动登录用主机自己的用户名与密码（Termlet 的 Telnet 不能用凭据档案，密码凭据展开到主机）。
    fn telnet(&mut self, h: &HostRecord, item: &mut PlannedHost) {
        let secrets = &self.data.secrets;
        match &h.auth {
            Login::Password => {
                item.host.username = h.username.clone();
                if let Some(password) = secrets.password(&h.username, &h.address, h.port) {
                    item.set_password(password.clone());
                }
            }
            // 不自动登录：Termlet 有用户名就会自动填写，所以不带用户名
            Login::NoPassword => {}
            Login::Credential(id) => {
                if let Some(credential) = self.data.credentials.get(id) {
                    item.host.username = self.username_of(h);
                    if credential.kind == CredentialKind::Password
                        && let Some(password) = secrets.credential_passwords.get(id)
                    {
                        item.set_password(password.clone());
                    }
                }
            }
        }
        if h.telnet_prompts {
            item.hints.push(Hint::TelnetPromptsIgnored);
        }
    }

    /// 跳板与代理（SSH、Telnet 相同）。
    fn route(&self, h: &HostRecord, item: &mut PlannedHost, targets: &HashMap<&str, JumpTarget>) {
        match &h.route {
            Route::Direct => {}
            Route::Jump(hops) => {
                // 某一跳是已删除的主机（或不是 SSH 主机）时直连
                let found: Option<Vec<&JumpTarget>> = hops
                    .iter()
                    .map(|hop| hop.as_deref().and_then(|id| targets.get(id)))
                    .collect();
                let Some(target) = found.as_ref().and_then(|f| f.last()) else {
                    if !hops.is_empty() {
                        item.hints.push(Hint::JumpNotFound);
                    }
                    return;
                };
                // 跳板主机自己的跳板正是前面几跳时，逐级向上与 ShellRS 的整条路相同，不用提醒
                if hops.len() > 1 && *target.route != Route::Jump(hops[..hops.len() - 1].to_vec()) {
                    let chain = found.iter().flatten().map(|t| t.name.to_string());
                    item.hints.push(Hint::MultiHopJump {
                        chain: chain.collect(),
                    });
                }
                item.jump = Some(JumpRef::new(target.name, target.endpoint.clone()));
            }
            Route::Proxy {
                kind,
                host,
                port,
                username,
            } => {
                item.host.proxy = Some(Proxy {
                    kind: *kind,
                    address: host.clone(),
                    port: *port,
                    username: username.clone(),
                    ..Default::default()
                });
                if !username.is_empty() {
                    match self.data.secrets.proxy(username, host, *port) {
                        Some(password) => item.set_proxy_password(password.clone()),
                        None => item.hints.push(Hint::ProxyPasswordRequired),
                    }
                }
            }
        }
    }

    /// 凭据对应的档案（第一次用到时加入计划），与用它的主机要带上的提醒。
    fn profile(&mut self, id: &str, credential: &Credential) -> (usize, Vec<Hint>) {
        if let Some(found) = self.profiles.get(id) {
            return found.clone();
        }
        let mut hints = Vec::new();
        let auth = match credential.kind {
            CredentialKind::Password => PlannedProfileAuth::Password,
            CredentialKind::Agent => PlannedProfileAuth::Agent,
            CredentialKind::Key => self.key_auth(id, credential, &mut hints),
        };
        let name = self.unique_profile_name(credential);
        let mut profile = PlannedProfile::new(&name, &credential.username, auth);
        if credential.kind == CredentialKind::Password
            && let Some(password) = self.data.secrets.credential_passwords.get(id)
        {
            profile.set_password(password.clone());
        }
        let index = self.plan.add_profile(profile);
        self.profiles.insert(id.to_string(), (index, hints.clone()));
        (index, hints)
    }

    /// 档案的名称：同一次导入中（名称、用户名）重复时加序号，免得写入时被当成同一个档案。
    fn unique_profile_name(&mut self, credential: &Credential) -> String {
        let base = match credential.name.trim() {
            "" => credential.username.trim(),
            name => name,
        };
        let username = credential.username.trim().to_string();
        let mut name = base.to_string();
        let mut n = 2;
        while !self.profile_names.insert((name.clone(), username.clone())) {
            name = format!("{base} ({n})");
            n += 1;
        }
        name
    }

    /// 密钥凭据的认证：同步中的私钥（连同私钥密码）或数据库中不加密的私钥文件存进密钥库；加密的私钥文件按路径引用。
    fn key_auth(
        &mut self,
        id: &str,
        credential: &Credential,
        hints: &mut Vec<Hint>,
    ) -> PlannedProfileAuth {
        let secrets = &self.data.secrets;
        let name = credential.name.trim().to_string();
        if let Some(text) = secrets.private_keys.get(id) {
            let passphrase = secrets.passphrases.get(id);
            return match self.library_key(text.expose().as_bytes(), passphrase, &name) {
                Ok(key) => PlannedProfileAuth::Key { key },
                Err(_) => {
                    hints.push(Hint::LibraryKeyUnusable { name });
                    PlannedProfileAuth::Password
                }
            };
        }
        let Some(path) = credential
            .key_path
            .as_deref()
            .filter(|p| !p.trim().is_empty())
        else {
            // 同步来的密钥凭据，私钥还没同步上去
            hints.push(Hint::LibraryKeyMissing);
            return PlannedProfileAuth::Password;
        };
        let path = self.env.expand_home(path.trim());
        let Ok(raw) = read_limited(Path::new(&path), MAX_KEY_FILE) else {
            hints.push(Hint::LibraryKeyMissing);
            return PlannedProfileAuth::Password;
        };
        let raw = zeroize::Zeroizing::new(raw);
        match self.library_key(&raw, None, &name) {
            Ok(key) => PlannedProfileAuth::Key { key },
            // 私钥密码在钥匙串里：按路径引用，连接时输入
            Err(crate::sshkey::KeyError::PassphraseRequired) => {
                hints.push(Hint::EncryptedKeyFile);
                PlannedProfileAuth::KeyFile { path }
            }
            Err(_) => {
                hints.push(Hint::LibraryKeyUnusable { name });
                PlannedProfileAuth::KeyFile { path }
            }
        }
    }

    /// 解开私钥，转成不加密的 OpenSSH 格式加入密钥库（按指纹，同一把只存一次），返回下标。
    fn library_key(
        &mut self,
        data: &[u8],
        passphrase: Option<&SecretString>,
        name: &str,
    ) -> Result<usize, crate::sshkey::KeyError> {
        let key = crate::sshkey::parse(data, passphrase)?;
        let fingerprint = key.fingerprint();
        if let Some(&index) = self.fingerprints.get(&fingerprint) {
            return Ok(index);
        }
        let private_key = key.to_openssh()?;
        let algorithm = match key.algorithm() {
            "ssh-ed25519" => KeyAlgorithm::Ed25519,
            a if a.starts_with("ecdsa") => KeyAlgorithm::Ecdsa,
            _ => KeyAlgorithm::Rsa,
        };
        let name = match name {
            "" => key.comment().to_string(),
            name => name.to_string(),
        };
        let index = self
            .plan
            .add_key(PlannedKey::new(name, algorithm, private_key));
        self.fingerprints.insert(fingerprint, index);
        Ok(index)
    }

    /// 端口转发：加到经由的 SSH 主机上，导入后不自动启动（预览中标出 ShellRS 中设为随启动开启的）。
    fn forwards(&mut self) {
        let mut forwards: Vec<&ForwardRecord> = self.data.forwards.iter().collect();
        forwards.sort_by_key(|f| (f.sort_order, f.name.to_lowercase()));
        for f in forwards {
            let Some(&index) = self.hosts.get(f.host.as_str()) else {
                continue;
            };
            let item = self.plan.host_mut(index);
            if item.host.protocol != Protocol::Ssh {
                continue;
            }
            let mut bind_host = f.bind_host.trim().to_string();
            if f.kind != ForwardKind::Remote && !is_local_bind(&bind_host) {
                item.hints.push(Hint::ForwardBindLoopback {
                    from: bind_host.clone(),
                });
                bind_host = "127.0.0.1".into();
            }
            item.forwards.push(Forward {
                kind: f.kind,
                name: clip(f.name.trim(), 64),
                bind_host,
                bind_port: f.bind_port,
                target_host: f.target_host.clone(),
                target_port: f.target_port,
                auto_start: f.auto_start,
                ..Default::default()
            });
        }
    }

    /// 命令片段：嵌套的分类合成一层“上级 / 下级”，点击时执行的末尾补换行；随勾选的主机一起导入。
    fn snippets(&mut self) {
        // 按 ShellRS 列表中的顺序：同一层中分类在前（没有分类的片段在最外层、分类之后）
        let mut snippets: Vec<(Vec<(u8, i64, String)>, &SnippetRecord)> = self
            .data
            .snippets
            .iter()
            .map(|s| {
                let mut order: Vec<(u8, i64, String)> = self
                    .category_chain(s.category.as_deref())
                    .into_iter()
                    .map(|c| (0, c.sort_order, c.name.to_lowercase()))
                    .collect();
                order.push((1, s.sort_order, s.name.to_lowercase()));
                (order, s)
            })
            .collect();
        snippets.sort_by(|a, b| a.0.cmp(&b.0));
        for (_, s) in snippets {
            let mut content = s.command.clone();
            if content.trim().is_empty() || content.chars().count() > MAX_SNIPPET {
                continue;
            }
            if s.run_on_click && !content.ends_with(['\r', '\n']) {
                content.push('\n');
            }
            let path: Vec<&str> = self
                .category_chain(s.category.as_deref())
                .into_iter()
                .map(|c| c.name.trim())
                .collect();
            let notes = self.plan.snippet_notes_mut();
            if !snippet_params(&s.command).is_empty() {
                notes.literal_params += 1;
            }
            if path.len() > 1 {
                notes.nested_groups += 1;
            }
            self.plan.add_snippet(Snippet {
                name: clip(s.name.trim(), 128),
                content,
                group: clip(&path.join(GROUP_SEPARATOR), 64).trim().to_string(),
                ..Default::default()
            });
        }
    }

    fn category_chain(&self, id: Option<&str>) -> Vec<&'a Category> {
        let categories = &self.data.categories;
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut current = id;
        while let Some(id) = current {
            if !seen.insert(id) || chain.len() >= MAX_DEPTH {
                break;
            }
            let Some(category) = categories.get(id) else {
                break;
            };
            chain.push(category);
            current = category.parent.as_deref();
        }
        chain.reverse();
        chain
    }
}

/// 本地、动态转发在 Termlet 中只能监听回环地址或任意地址。
fn is_local_bind(host: &str) -> bool {
    let host = host.trim();
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    host.is_empty()
        || host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified())
}

/// 登录后执行的命令：去掉空行，按存储的上限截断。
fn startup(commands: &str) -> String {
    commands
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .take(MAX_STARTUP_LINES)
        .map(|l| clip(l, MAX_STARTUP_LINE))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 串口的参数（数据库中的写法）；不认识的取值时为 `None`。
fn serial_options(
    baud: i64,
    data_bits: i64,
    parity: &str,
    stop_bits: i64,
    flow: &str,
) -> Option<SerialOptions> {
    Some(SerialOptions {
        baud: u32::try_from(baud).ok()?,
        data_bits: u8::try_from(data_bits)
            .ok()
            .filter(|b| (5..=8).contains(b))?,
        parity: match parity {
            "none" => Parity::None,
            "odd" => Parity::Odd,
            "even" => Parity::Even,
            _ => return None,
        },
        stop_bits: match stop_bits {
            1 => StopBits::One,
            2 => StopBits::Two,
            _ => return None,
        },
        flow_control: match flow {
            "none" => FlowControl::None,
            "rts-cts" => FlowControl::RtsCts,
            "xon-xoff" => FlowControl::XonXoff,
            _ => return None,
        },
        usb: None,
    })
}

/// 协议的写法（数据库与同步相同）。
fn protocol(name: &str) -> Option<Protocol> {
    match name {
        "ssh" => Some(Protocol::Ssh),
        "telnet" => Some(Protocol::Telnet),
        "serial" => Some(Protocol::Serial),
        _ => None,
    }
}

fn login(auth: &str, credential: Option<String>) -> Option<Login> {
    match auth {
        "password" => Some(Login::Password),
        "no-password" => Some(Login::NoPassword),
        "credential" => credential.map(Login::Credential),
        _ => None,
    }
}

fn credential_kind(kind: &str) -> Option<CredentialKind> {
    match kind {
        "password" => Some(CredentialKind::Password),
        "key" => Some(CredentialKind::Key),
        "agent" => Some(CredentialKind::Agent),
        _ => None,
    }
}

fn proxy_kind(kind: &str) -> Option<ProxyKind> {
    match kind {
        "http" => Some(ProxyKind::Http),
        "socks5" => Some(ProxyKind::Socks5),
        _ => None,
    }
}

fn forward_kind(kind: &str) -> Option<ForwardKind> {
    match kind {
        "local" => Some(ForwardKind::Local),
        "remote" => Some(ForwardKind::Remote),
        "dynamic" => Some(ForwardKind::Dynamic),
        _ => None,
    }
}

/// 端口：1–65535。
fn port_of(value: i64) -> Option<u16> {
    u16::try_from(value).ok().filter(|&p| p > 0)
}
