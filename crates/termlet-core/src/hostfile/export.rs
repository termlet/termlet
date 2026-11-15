//! 导出主机：Termlet 主机文件与 ssh_config。

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use zeroize::Zeroizing;

use super::{
    FORMAT_NAME, FileDoc, FileGroup, FileHost, FileKey, FileProfile, FileSecret, FileSecrets,
    HOST_SECRETS, PROFILE_SECRETS, VERSION,
};
use crate::CoreError;
use crate::backup::{BackupError, SealedKind, envelope};
use crate::id::{GroupId, HostId, KeyId, ProfileId};
use crate::model::{
    Auth, CliAuth, Forward, ForwardKind, Group, Host, Jump, KeyEntry, KeySource, Profile,
    ProfileAuth, Proxy, ProxyKind, SecretField, join_host_port,
};
use crate::secret::SecretString;
use crate::store::{Owner, Record, Store};

/// 导出的格式。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExportFormat {
    /// Termlet 主机文件（`.termlet-hosts`）
    #[default]
    Termlet,
    /// OpenSSH 的 ssh_config：只有 SSH 主机的连接参数，不含秘密
    SshConfig,
}

/// 导出的请求。
#[derive(Clone, Debug, Default)]
pub struct ExportRequest {
    /// 选中的主机（它们用到的跳板机会一并导出）
    pub hosts: Vec<HostId>,
    pub format: ExportFormat,
    /// 包含端口转发规则
    pub forwards: bool,
    /// 包含密码、私钥等秘密，并用这个密码加密整个文件（至少 8 个字符；只用于 Termlet 格式）。`None` 表示不含秘密
    pub password: Option<SecretString>,
}

/// 导出的数量（导出前预览、导出后提示）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExportSummary {
    /// 导出的主机数，其中 `added` 台是没有选中、作为跳板机一并导出的
    pub hosts: usize,
    pub added: usize,
    /// ssh_config 中没有导出的 Telnet、串口主机数
    pub skipped: usize,
    pub groups: usize,
    /// 凭据档案
    pub profiles: usize,
    /// 密钥库中的密钥
    pub keys: usize,
    /// 端口转发规则
    pub forwards: usize,
}

/// 导出前的预览：将导出的数量（不读秘密）。选中的主机不存在时返回 [`BackupError::HostNotFound`]。
pub fn summarize(store: &Store, request: &ExportRequest) -> Result<ExportSummary, BackupError> {
    let (list, added) = collect(store, &request.hosts)?;
    let mut summary = match request.format {
        ExportFormat::Termlet => {
            let doc = build(store, &list, false, request.forwards)?;
            ExportSummary {
                hosts: doc.hosts.len(),
                groups: doc.groups.len(),
                profiles: doc.profiles.len(),
                keys: doc.keys.len(),
                forwards: doc.hosts.iter().map(|h| h.forwards.len()).sum(),
                ..Default::default()
            }
        }
        ExportFormat::SshConfig => {
            let ssh: Vec<&Record<Host>> = list.iter().filter(|h| h.doc.is_ssh()).collect();
            ExportSummary {
                hosts: ssh.len(),
                skipped: list.len() - ssh.len(),
                forwards: if request.forwards {
                    forwards_of(store, ssh.iter().map(|h| &h.id)).len()
                } else {
                    0
                },
                ..Default::default()
            }
        }
    };
    summary.added = added;
    Ok(summary)
}

/// 导出到 `path`（本机的绝对路径，只有当前用户能读写；已存在时替换），返回导出的数量。
/// 含秘密的密码太短时返回 [`BackupError::PasswordTooShort`]；读秘密时保险库已锁定返回 [`BackupError::VaultLocked`]。
pub async fn export(
    store: Arc<Store>,
    path: PathBuf,
    request: ExportRequest,
) -> Result<ExportSummary, BackupError> {
    match tokio::task::spawn_blocking(move || export_blocking(&store, &path, &request)).await {
        Ok(result) => result,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => Err(BackupError::Store(e.to_string())),
    }
}

fn export_blocking(
    store: &Store,
    path: &std::path::Path,
    request: &ExportRequest,
) -> Result<ExportSummary, BackupError> {
    let password = match request.format {
        ExportFormat::Termlet => request.password.as_ref().filter(|p| !p.is_empty()),
        ExportFormat::SshConfig => None,
    };
    if let Some(password) = password {
        envelope::check_password(password)?;
    }
    crate::backup::check_output(path)?;
    let summary = summarize(store, request)?;
    let (list, _) = collect(store, &request.hosts)?;
    let data: Zeroizing<Vec<u8>> = match request.format {
        ExportFormat::SshConfig => {
            Zeroizing::new(ssh_config(store, &list, request.forwards).into_bytes())
        }
        ExportFormat::Termlet => {
            let doc = build(store, &list, password.is_some(), request.forwards)?;
            let mut json = Zeroizing::new(
                serde_json::to_vec_pretty(&doc).map_err(|e| BackupError::Store(e.to_string()))?,
            );
            json.push(b'\n');
            match password {
                Some(password) => {
                    Zeroizing::new(envelope::seal(SealedKind::Hosts, password, &json)?)
                }
                None => json,
            }
        }
    };
    crate::backup::write_file(path, &data)?;
    tracing::info!(
        hosts = summary.hosts,
        added = summary.added,
        secrets = password.is_some(),
        "已导出主机"
    );
    Ok(summary)
}

/// 主机实际使用的跳板机：经另一台主机，或"沿用分组"时最近的分组默认跳板机（是自己时忽略）。
fn effective_jump(store: &Store, record: &Record<Host>) -> Option<HostId> {
    match &record.doc.jump {
        Jump::Direct => None,
        Jump::Via { host } => Some(host.clone()),
        Jump::Inherit => store
            .group_chain(record.parent.clone().map(GroupId::from_raw).as_ref())
            .iter()
            .find_map(|g| g.doc.defaults.jump.clone())
            .filter(|jump| jump != &record.id),
    }
}

/// 要导出的主机（按主机列表的顺序）：选中的主机，加上它们用到的跳板机（逐级，含分组默认的跳板机）。
/// 第二项是没有选中、作为跳板机加入的主机数。
fn collect(store: &Store, ids: &[HostId]) -> Result<(Vec<Record<Host>>, usize), BackupError> {
    if ids.is_empty() {
        return Err(BackupError::NothingSelected);
    }
    let all = store.list::<Host>();
    let by_id: HashMap<&HostId, &Record<Host>> = all.iter().map(|h| (&h.id, h)).collect();
    let mut want: HashSet<HostId> = HashSet::new();
    let mut queue: Vec<HostId> = Vec::new();
    for id in ids {
        if !by_id.contains_key(id) {
            return Err(BackupError::HostNotFound);
        }
        if want.insert(id.clone()) {
            queue.push(id.clone());
        }
    }
    let mut added = 0;
    while let Some(id) = queue.pop() {
        if let Some(jump) = effective_jump(store, by_id[&id])
            && by_id.contains_key(&jump)
            && want.insert(jump.clone())
        {
            added += 1;
            queue.push(jump);
        }
    }
    let list = all
        .iter()
        .filter(|h| want.contains(&h.id))
        .cloned()
        .collect();
    Ok((list, added))
}

/// 经这些主机的端口转发规则（按规则列表的顺序）。
fn forwards_of<'a>(store: &Store, hosts: impl Iterator<Item = &'a HostId>) -> Vec<Record<Forward>> {
    let hosts: HashSet<&HostId> = hosts.collect();
    store
        .list::<Forward>()
        .into_iter()
        .filter(|f| f.doc.host.as_ref().is_some_and(|h| hosts.contains(h)))
        .collect()
}

/// 读出条目的秘密（`fields` 中有值的）。解不开的（已损坏，本来就用不了）跳过并记警告；保险库锁定时报错。
fn read_secrets(
    store: &Store,
    owner: &Owner,
    fields: &[SecretField],
) -> Result<FileSecrets, BackupError> {
    let mut out = FileSecrets::default();
    for &field in fields {
        if !store.has_secret(owner, field) {
            continue;
        }
        match store.secret(owner, field) {
            Ok(Some(value)) if !value.is_empty() => out.insert(field, value),
            Ok(_) => {}
            Err(CoreError::Decrypt) => {
                tracing::warn!(owner = %owner, field = field.name(), "秘密无法解密，没有导出");
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(out)
}

fn library_key(source: &KeySource) -> Option<&KeyId> {
    match source {
        KeySource::Library { key } => Some(key),
        _ => None,
    }
}

/// 生成 Termlet 主机文件的内容。
fn build(
    store: &Store,
    list: &[Record<Host>],
    with_secrets: bool,
    forwards: bool,
) -> Result<FileDoc, BackupError> {
    let exported: HashSet<&HostId> = list.iter().map(|h| &h.id).collect();
    let groups = store.list::<Group>();
    let by_group: HashMap<&GroupId, &Record<Group>> = groups.iter().map(|g| (&g.id, g)).collect();

    // 主机所在分组及其上级分组
    let mut used_groups: HashSet<GroupId> = HashSet::new();
    for host in list {
        let mut next = host.parent.clone().map(GroupId::from_raw);
        while let Some(id) = next {
            let Some(group) = by_group.get(&id) else {
                break;
            };
            if !used_groups.insert(id) {
                break;
            }
            next = group.parent.clone().map(GroupId::from_raw);
        }
    }

    let mut used_profiles: BTreeSet<ProfileId> = BTreeSet::new();
    let mut used_keys: BTreeSet<KeyId> = BTreeSet::new();
    let mut doc = FileDoc {
        format: FORMAT_NAME.into(),
        version: VERSION,
        exported_at: crate::time::now_ms(),
        app: crate::VERSION.into(),
        secrets: with_secrets,
        ..Default::default()
    };

    for group in groups.iter().filter(|g| used_groups.contains(&g.id)) {
        let mut defaults = group.doc.defaults.clone();
        if let Some(profile) = &defaults.profile {
            used_profiles.insert(profile.clone());
        }
        // 默认的跳板机没有导出（没有主机用它）时不写
        defaults.jump = defaults.jump.filter(|j| exported.contains(j));
        doc.groups.push(FileGroup {
            id: group.id.to_string(),
            name: group.doc.name.clone(),
            parent: group
                .parent
                .clone()
                .filter(|p| used_groups.contains(&GroupId::from_raw(p.clone()))),
            defaults,
        });
    }

    let rules = if forwards {
        forwards_of(store, list.iter().map(|h| &h.id))
    } else {
        Vec::new()
    };
    for host in list {
        match &host.doc.auth {
            Auth::Profile { profile } => {
                used_profiles.insert(profile.clone());
            }
            Auth::Key { source } => used_keys.extend(library_key(source).cloned()),
            _ => {}
        }
        if let Some(credential) = &host.doc.cli.credential
            && let CliAuth::Key { source } = &credential.auth
        {
            used_keys.extend(library_key(source).cloned());
        }
        let secrets = if with_secrets {
            read_secrets(store, &Owner::host(&host.id), &HOST_SECRETS)?
        } else {
            FileSecrets::default()
        };
        doc.hosts.push(FileHost {
            id: host.id.to_string(),
            group: host
                .parent
                .clone()
                .filter(|p| used_groups.contains(&GroupId::from_raw(p.clone()))),
            host: host.doc.clone(),
            secrets,
            forwards: rules
                .iter()
                .filter(|r| r.doc.host.as_ref() == Some(&host.id))
                .map(|r| Forward {
                    host: None,
                    ..r.doc.clone()
                })
                .collect(),
        });
    }

    for id in &used_profiles {
        let Some(profile) = store.get::<Profile>(id) else {
            continue;
        };
        if let ProfileAuth::Key { source } = &profile.doc.auth {
            used_keys.extend(library_key(source).cloned());
        }
        let secrets = if with_secrets {
            read_secrets(store, &Owner::profile(id), &PROFILE_SECRETS)?
        } else {
            FileSecrets::default()
        };
        doc.profiles.push(FileProfile {
            id: id.to_string(),
            profile: profile.doc,
            secrets,
        });
    }

    for id in &used_keys {
        let Some(key) = store.get::<KeyEntry>(id) else {
            continue;
        };
        let private_key = if with_secrets {
            read_secrets(store, &Owner::key(id), &[SecretField::PrivateKey])?
                .get(SecretField::PrivateKey)
                .cloned()
                .map(FileSecret)
        } else {
            None
        };
        doc.keys.push(FileKey {
            id: id.to_string(),
            name: key.doc.name,
            algorithm: key.doc.algorithm,
            fingerprint: key.doc.fingerprint,
            public_key: key.doc.public_key,
            certificate: key.doc.certificate,
            private_key,
        });
    }
    Ok(doc)
}

// ---------------------------------------------------------------- ssh_config

/// 生成 OpenSSH 的 ssh_config：只含 SSH 主机的连接参数（不含密码），跳板机写成 `ProxyJump`。
fn ssh_config(store: &Store, list: &[Record<Host>], forwards: bool) -> String {
    let mut aliases: HashMap<&HostId, String> = HashMap::new();
    let mut used: HashSet<String> = HashSet::new();
    for host in list.iter().filter(|h| h.doc.is_ssh()) {
        let base = alias(&host.doc.name, &host.doc.address);
        let mut name = base.clone();
        let mut n = 2;
        while used.contains(&name.to_lowercase()) {
            name = format!("{base}-{n}");
            n += 1;
        }
        used.insert(name.to_lowercase());
        aliases.insert(&host.id, name);
    }
    let rules = if forwards {
        forwards_of(store, list.iter().map(|h| &h.id))
    } else {
        Vec::new()
    };

    let mut out = String::new();
    out.push_str(&format!(
        "# Termlet 导出的主机（{}）\n",
        crate::time::format_local(crate::time::now_ms())
    ));
    out.push_str(
        "# 只含连接参数，不含密码；使用 Termlet 密钥库中密钥的主机需要自行指定 IdentityFile\n",
    );
    for host in list.iter().filter(|h| h.doc.is_ssh()) {
        let doc = &host.doc;
        let chain = store.group_chain(host.parent.clone().map(GroupId::from_raw).as_ref());
        out.push('\n');
        if !chain.is_empty() {
            let path: Vec<&str> = chain.iter().rev().map(|g| g.doc.name.as_str()).collect();
            out.push_str(&format!("# 分组：{}\n", one_line(&path.join("/"))));
        }
        out.push_str(&format!("Host {}\n", aliases[&host.id]));
        option(&mut out, "HostName", &doc.address);
        if doc.effective_port() != 22 {
            option(&mut out, "Port", &doc.effective_port().to_string());
        }
        option(
            &mut out,
            "User",
            &crate::import::effective_username(store, doc, host.parent.as_deref()),
        );
        // 私钥：主机自己的，或用的凭据档案的
        let profile = match &doc.auth {
            Auth::Profile { profile } => Some(profile.clone()),
            Auth::Inherit => chain.iter().find_map(|g| g.doc.defaults.profile.clone()),
            _ => None,
        }
        .and_then(|id| store.get::<Profile>(&id));
        let source = match (&doc.auth, &profile) {
            (Auth::Profile { .. } | Auth::Inherit, Some(p)) => match &p.doc.auth {
                ProfileAuth::Key { source } => Some(source.clone()),
                _ => None,
            },
            (Auth::Key { source }, _) => Some(source.clone()),
            _ => None,
        };
        match source {
            Some(KeySource::Library { key }) => {
                if let Some(key) = store.get::<KeyEntry>(&key) {
                    out.push_str(&format!(
                        "    # 使用 Termlet 密钥库中的密钥：{}（{}）\n",
                        one_line(&key.doc.name),
                        key.doc.fingerprint
                    ));
                }
            }
            Some(KeySource::File { path, certificate }) => {
                option(&mut out, "IdentityFile", &path);
                if !certificate.trim().is_empty() {
                    option(&mut out, "CertificateFile", &certificate);
                }
            }
            Some(KeySource::Pasted { .. }) => {
                out.push_str("    # 使用保存在 Termlet 中的私钥：需要自行指定 IdentityFile\n");
            }
            None => {}
        }
        let jump = effective_jump(store, host).and_then(|j| aliases.get(&j));
        if let Some(jump) = jump {
            option(&mut out, "ProxyJump", jump);
        }
        // 经跳板机连接时主机自己的代理不生效（与 Termlet 相同；OpenSSH 中 ProxyJump 与 ProxyCommand 也只能二选一）。
        // 主机没有自己的代理时写出沿用的分组代理命令
        let inherited = (doc.proxy.is_none() && !doc.no_proxy)
            .then(|| {
                chain.iter().find_map(|g| {
                    let command = g.doc.defaults.proxy_command.trim();
                    (!command.is_empty()).then(|| Proxy {
                        kind: ProxyKind::Command,
                        command: command.to_string(),
                        ..Default::default()
                    })
                })
            })
            .flatten();
        if let (Some(proxy), None) = (doc.proxy.as_ref().or(inherited.as_ref()), jump) {
            match proxy.kind {
                ProxyKind::Command => {
                    out.push_str(&format!("    ProxyCommand {}\n", one_line(&proxy.command)));
                }
                kind => {
                    let scheme = match kind {
                        ProxyKind::Socks5 => "socks5",
                        ProxyKind::Socks4 => "socks4",
                        ProxyKind::Http => "http",
                        _ => "https",
                    };
                    out.push_str(&format!(
                        "    # 代理：{scheme}://{}（OpenSSH 没有对应的选项，需要自行配置 ProxyCommand）\n",
                        join_host_port(&one_line(&proxy.address), proxy.port)
                    ));
                }
            }
        }
        if doc.ssh.agent_forward {
            option(&mut out, "ForwardAgent", "yes");
        }
        if doc.ssh.x11_forward {
            option(&mut out, "ForwardX11", "yes");
            // 写明：有的发行版（Debian、Ubuntu）把 ForwardX11Trusted 的默认值改成了 yes
            option(
                &mut out,
                "ForwardX11Trusted",
                if doc.ssh.x11_trusted { "yes" } else { "no" },
            );
        }
        if doc.ssh.legacy_algorithms {
            out.push_str("    # Termlet 中开启了\"兼容旧设备\"：OpenSSH 需要自行添加旧算法（KexAlgorithms、Ciphers、HostKeyAlgorithms）\n");
        }
        for rule in rules
            .iter()
            .filter(|r| r.doc.host.as_ref() == Some(&host.id))
        {
            let rule = &rule.doc;
            let bind = rule.bind_address();
            match rule.kind {
                ForwardKind::Dynamic => out.push_str(&format!("    DynamicForward {bind}\n")),
                ForwardKind::Remote => out.push_str(&format!(
                    "    RemoteForward {bind} {}\n",
                    rule.target_address()
                )),
                ForwardKind::Local => out.push_str(&format!(
                    "    LocalForward {bind} {}\n",
                    rule.target_address()
                )),
            }
        }
    }
    out
}

/// 主机名称转为 ssh_config 的 `Host` 别名：空白、控制字符与通配符换成 `-`，去掉首尾的 `-`；为空时用地址。
pub(super) fn alias(name: &str, address: &str) -> String {
    let replaced: String = name
        .trim()
        .chars()
        .map(|c| {
            if c <= ' ' || c == '\u{7f}' || "*?!,#\"'\\".contains(c) {
                '-'
            } else {
                c
            }
        })
        .collect();
    let trimmed = replaced.trim_matches('-');
    if trimmed.is_empty() {
        address.to_string()
    } else {
        trimmed.to_string()
    }
}

/// 写一行配置，值为空时不写；含空白的值加引号。
fn option(out: &mut String, key: &str, value: &str) {
    let value = one_line(value);
    if value.is_empty() {
        return;
    }
    if value.contains([' ', '\t']) {
        out.push_str(&format!("    {key} \"{value}\"\n"));
    } else {
        out.push_str(&format!("    {key} {value}\n"));
    }
}

fn one_line(s: &str) -> String {
    crate::import::one_line(s)
}
