//! 导出选中的主机（Termlet 自己的主机文件 `.termlet-hosts`，或 OpenSSH 的 ssh_config），以及导入 Termlet 的主机文件。
//!
//! 与完整备份（[`crate::backup`]）不同，导出只含选中的主机与它们用到的东西：跳板机（含沿用分组默认的跳板机）逐级一并导出，
//! 用到的凭据档案、密钥库中的密钥（公钥与指纹）、所在分组及上级分组（名称与默认值）、端口转发规则随主机导出。
//! 默认不含密码、私钥、TOTP 密钥等秘密，文件是可以直接阅读、编辑的 JSON；选择包含秘密时必须设置密码，
//! 整个文件用密码加密（见 [`crate::backup::envelope`]）。主机、凭据档案的内容按条目原样写出（`Host`、`Profile` 的 JSON），
//! 其中引用的 ID（凭据档案、密钥、跳板机、分组）是导出时的 ID，导入时换成本机的。
//!
//! 导入时复用"导入主机"的预览、去重（协议、地址、端口、实际的用户名都相同的视为同一台）与跳过 / 合并（见 [`fn@import`]）。
//! 导入的主机不允许命令行工具访问（需要在主机设置中重新勾选），预览中标出连接时在本机执行的代理命令与登录后执行的命令，
//! 并列出随主机导入的端口转发（合并时添加到已有的主机上）；导入的规则不自动启动，需要用户自己开启。
//!
//! # 用法（界面）
//!
//! 导出：按分组或主机勾选，选格式、是否包含秘密（含秘密时界面校验两次输入的密码一致）、是否包含端口转发，
//! [`summarize`] 显示将导出的数量（含一并导出的跳板机），用保存对话框选好路径后调 [`fn@export`]。
//! 导入：[`read`] 读文件（加密的没给密码时返回 `PasswordRequired`，界面请用户输入后再读），[`preview`] 列出主机，
//! 勾选后 [`fn@import`]。都是异步函数（派生密钥、读写数据库在阻塞线程中），必须在核心的运行时上执行。

mod export;
mod import;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

pub use export::{ExportFormat, ExportRequest, ExportSummary, export, summarize};
pub use import::import;

use crate::backup::{BackupError, SealedKind, envelope};
use crate::import::{Endpoint, ExistingHosts, Hint, ImportPreview, PreviewItem};
use crate::model::{
    Auth, Forward, GroupDefaults, Host, Jump, KeySource, Profile, ProfileAuth, SecretField,
};
use crate::secret::SecretString;
use crate::store::Store;

/// 文件中 `format` 字段的值。
const FORMAT_NAME: &str = "termlet-hosts";
/// 文件格式的版本。
const VERSION: i64 = 1;
/// 读取的大小上限。
const MAX_FILE_SIZE: u64 = 64 << 20;

/// 主机的全部秘密字段（含秘密的文件中按名称写出）。
const HOST_SECRETS: [SecretField; 8] = [
    SecretField::Password,
    SecretField::PrivateKey,
    SecretField::Passphrase,
    SecretField::Totp,
    SecretField::ProxyPassword,
    SecretField::CliPassword,
    SecretField::CliPrivateKey,
    SecretField::CliPassphrase,
];
/// 凭据档案的秘密字段。
const PROFILE_SECRETS: [SecretField; 3] = [
    SecretField::Password,
    SecretField::PrivateKey,
    SecretField::Passphrase,
];

/// 读出的主机文件（导入前预览、导入时用）。其中的秘密只有核心能取出，`Debug` 不显示。
#[derive(Clone, Debug)]
pub struct HostFile {
    doc: FileDoc,
}

impl HostFile {
    /// 导出的时间（Unix 毫秒）。
    pub fn exported_at(&self) -> i64 {
        self.doc.exported_at
    }

    /// 导出时的 Termlet 版本。
    pub fn app(&self) -> &str {
        &self.doc.app
    }

    pub fn host_count(&self) -> usize {
        self.doc.hosts.len()
    }
}

/// 文件的内容。各项之间用 `ref`（导出时的 ID）互相引用。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileDoc {
    format: String,
    version: i64,
    /// Unix 毫秒
    #[serde(default)]
    exported_at: i64,
    #[serde(default)]
    app: String,
    /// 含秘密
    #[serde(default)]
    secrets: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    groups: Vec<FileGroup>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    profiles: Vec<FileProfile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    keys: Vec<FileKey>,
    #[serde(default)]
    hosts: Vec<FileHost>,
}

/// 分组：名称、上级与默认值（默认的凭据档案、跳板机是文件中的 `ref`）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileGroup {
    #[serde(rename = "ref")]
    id: String,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
    #[serde(default)]
    defaults: GroupDefaults,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileProfile {
    #[serde(rename = "ref")]
    id: String,
    profile: Profile,
    #[serde(default, skip_serializing_if = "FileSecrets::is_empty")]
    secrets: FileSecrets,
}

/// 密钥库中的一把密钥。没有私钥时，导入的电脑上密钥库中有同一把密钥（指纹相同）就使用它。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileKey {
    #[serde(rename = "ref")]
    id: String,
    name: String,
    #[serde(default)]
    algorithm: String,
    fingerprint: String,
    #[serde(default)]
    public_key: String,
    /// 附带的 OpenSSH 证书（不是秘密）
    #[serde(default, skip_serializing_if = "String::is_empty")]
    certificate: String,
    /// 不加密的 OpenSSH 私钥（只在含秘密的文件中）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    private_key: Option<FileSecret>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileHost {
    #[serde(rename = "ref")]
    id: String,
    /// 所在分组（`groups` 中的 `ref`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    group: Option<String>,
    host: Host,
    #[serde(default, skip_serializing_if = "FileSecrets::is_empty")]
    secrets: FileSecrets,
    /// 经这台主机的端口转发（`host` 字段为空）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    forwards: Vec<Forward>,
}

/// 文件中的一项秘密：`Debug` 不显示，丢弃时清零。
#[derive(Clone, Debug, Default)]
struct FileSecret(SecretString);

impl Serialize for FileSecret {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0.expose())
    }
}

impl<'de> Deserialize<'de> for FileSecret {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self(SecretString::new(String::deserialize(deserializer)?)))
    }
}

/// 一个条目的秘密：字段名（`password`、`private_key`……）→ 值。不认识的字段忽略。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
struct FileSecrets(BTreeMap<String, FileSecret>);

impl FileSecrets {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn get(&self, field: SecretField) -> Option<&SecretString> {
        self.0
            .get(field.name())
            .map(|s| &s.0)
            .filter(|s| !s.is_empty())
    }

    fn has(&self, field: SecretField) -> bool {
        self.get(field).is_some()
    }

    fn insert(&mut self, field: SecretField, value: SecretString) {
        self.0.insert(field.name().to_string(), FileSecret(value));
    }

    /// 指定字段中有值的。
    fn pick(&self, fields: &[SecretField]) -> Vec<(SecretField, SecretString)> {
        fields
            .iter()
            .filter_map(|&f| self.get(f).map(|v| (f, v.clone())))
            .collect()
    }
}

impl FileDoc {
    fn group(&self, id: &str) -> Option<&FileGroup> {
        self.groups.iter().find(|g| g.id == id)
    }

    fn host(&self, id: &str) -> Option<&FileHost> {
        self.hosts.iter().find(|h| h.id == id)
    }

    fn profile(&self, id: &str) -> Option<&FileProfile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    fn key(&self, id: &str) -> Option<&FileKey> {
        self.keys.iter().find(|k| k.id == id)
    }

    /// 从分组 `start` 开始逐级向上的分组（自己在最前），遇到不存在的分组或循环时停止。
    fn group_chain(&self, start: Option<&str>) -> Vec<&FileGroup> {
        let mut chain: Vec<&FileGroup> = Vec::new();
        let mut seen = HashSet::new();
        let mut next = start;
        while let Some(id) = next {
            let Some(group) = self.group(id) else { break };
            if !seen.insert(id) {
                break;
            }
            chain.push(group);
            next = group.parent.as_deref();
        }
        chain
    }

    /// 分组从根开始的各级名称。
    fn group_path(&self, start: Option<&str>) -> Vec<String> {
        let mut names: Vec<String> = self
            .group_chain(start)
            .iter()
            .map(|g| g.name.clone())
            .collect();
        names.reverse();
        names
    }

    /// 主机用到的凭据档案：主机选用的，或"沿用分组"时最近的分组默认档案。
    fn host_profile(&self, host: &FileHost) -> Option<&FileProfile> {
        let id = match &host.host.auth {
            Auth::Profile { profile } => Some(profile.as_str().to_string()),
            Auth::Inherit => self
                .group_chain(host.group.as_deref())
                .iter()
                .find_map(|g| g.defaults.profile.as_ref().map(|p| p.as_str().to_string())),
            _ => None,
        }?;
        self.profile(&id)
    }

    /// 文件中主机实际使用的用户名：主机自己的 > 凭据档案的 > 所在分组（逐级向上）默认的。
    fn effective_user(&self, host: &FileHost) -> String {
        let own = host.host.username.trim();
        if !own.is_empty() {
            return own.to_string();
        }
        if let Some(profile) = self.host_profile(host) {
            let username = profile.profile.username.trim();
            if !username.is_empty() {
                return username.to_string();
            }
        }
        self.group_chain(host.group.as_deref())
            .iter()
            .map(|g| g.defaults.username.trim())
            .find(|u| !u.is_empty())
            .unwrap_or_default()
            .to_string()
    }

    /// 文件中主机的跳板机（含沿用分组默认的）；是自己或不在文件中时为 `None`。
    fn effective_jump(&self, host: &FileHost) -> Option<&FileHost> {
        let id = match &host.host.jump {
            Jump::Direct => None,
            Jump::Via { host } => Some(host.as_str().to_string()),
            Jump::Inherit => self
                .group_chain(host.group.as_deref())
                .iter()
                .find_map(|g| g.defaults.jump.as_ref().map(|j| j.as_str().to_string())),
        }?;
        if id == host.id {
            return None;
        }
        self.host(&id)
    }

    /// 判断重复用的键。
    fn endpoint(&self, host: &FileHost) -> Endpoint {
        Endpoint::new(
            &host.host.address,
            host.host.effective_port(),
            &self.effective_user(host),
        )
    }

    fn key_has_private(&self, source: &KeySource, own: &FileSecrets, field: SecretField) -> bool {
        match source {
            KeySource::Library { key } => self
                .key(key.as_str())
                .is_some_and(|k| k.private_key.as_ref().is_some_and(|p| !p.0.is_empty())),
            KeySource::Pasted { .. } => own.has(field),
            KeySource::File { .. } => false,
        }
    }

    /// 主机带登录密码（自己的，或用的凭据档案的）。
    fn has_password(&self, host: &FileHost) -> bool {
        match (&host.host.auth, self.host_profile(host)) {
            (Auth::Profile { .. } | Auth::Inherit, Some(profile)) => {
                profile.profile.auth == ProfileAuth::Password
                    && profile.secrets.has(SecretField::Password)
            }
            (Auth::Password | Auth::Inherit, None) => host.secrets.has(SecretField::Password),
            _ => false,
        }
    }

    /// 主机带私钥（自己粘贴的、密钥库中的，或用的凭据档案的）。
    fn has_key(&self, host: &FileHost) -> bool {
        match (&host.host.auth, self.host_profile(host)) {
            (Auth::Profile { .. } | Auth::Inherit, Some(profile)) => match &profile.profile.auth {
                ProfileAuth::Key { source } => {
                    self.key_has_private(source, &profile.secrets, SecretField::PrivateKey)
                }
                _ => false,
            },
            (Auth::Key { source }, _) => {
                self.key_has_private(source, &host.secrets, SecretField::PrivateKey)
            }
            _ => false,
        }
    }
}

/// 读取主机文件（不超过 64 MB）。加密的文件没给密码时返回 [`BackupError::PasswordRequired`]，密码错误时返回
/// [`BackupError::WrongPassword`]；选的是完整备份时返回 [`BackupError::IsFullBackup`]。
pub async fn read(path: PathBuf, password: Option<SecretString>) -> Result<HostFile, BackupError> {
    match tokio::task::spawn_blocking(move || {
        let data = Zeroizing::new(crate::backup::read_file(&path, MAX_FILE_SIZE)?);
        parse(&data, password.as_ref())
    })
    .await
    {
        Ok(result) => result,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => Err(BackupError::Store(e.to_string())),
    }
}

/// 解析主机文件的内容（加密的用 `password` 解开）。
pub fn parse(data: &[u8], password: Option<&SecretString>) -> Result<HostFile, BackupError> {
    if data.len() as u64 > MAX_FILE_SIZE {
        return Err(BackupError::TooLarge);
    }
    let payload = if envelope::is_sealed(data) {
        match envelope::peek_kind(data) {
            Some(SealedKind::Hosts) => {}
            Some(SealedKind::Backup) => return Err(BackupError::IsFullBackup),
            _ => return Err(BackupError::Invalid),
        }
        Some(envelope::open(data, password)?.1)
    } else {
        None
    };
    let text = payload.as_deref().map_or(data, |p| &p[..]);

    #[derive(Deserialize)]
    struct Head {
        #[serde(default)]
        format: String,
        #[serde(default)]
        version: i64,
    }
    let head: Head = serde_json::from_slice(text).map_err(|_| BackupError::Invalid)?;
    if head.format != FORMAT_NAME {
        return Err(BackupError::Invalid);
    }
    if head.version > VERSION {
        return Err(BackupError::TooNew {
            found: head.version,
            supported: VERSION,
        });
    }
    if head.version < 1 {
        return Err(BackupError::Invalid);
    }
    let doc: FileDoc = serde_json::from_slice(text).map_err(|e| {
        tracing::warn!("主机文件的内容无法解析：{e}");
        BackupError::Invalid
    })?;
    Ok(HostFile { doc })
}

/// 主机文件的预览：标出本机已有的相同主机。只读内存，很快，可以在界面线程调用。
pub fn preview(store: &Store, file: &HostFile) -> ImportPreview {
    let doc = &file.doc;
    let existing = ExistingHosts::load(store);
    let items = doc
        .hosts
        .iter()
        .map(|h| PreviewItem {
            key: h.id.clone(),
            name: h.host.name.clone(),
            protocol: h.host.protocol,
            address: h.host.address.clone(),
            port: h.host.effective_port(),
            username: doc.effective_user(h),
            group: doc.group_path(h.group.as_deref()),
            tags: h.host.tags.clone(),
            jump: doc.effective_jump(h).map(|j| j.host.name.clone()),
            has_password: doc.has_password(h),
            has_key: doc.has_key(h),
            profile: doc.host_profile(h).map(|p| p.profile.name.clone()),
            duplicate_of: existing.find(h.host.protocol, &doc.endpoint(h)).cloned(),
            hints: if h.host.ssh.agent_forward {
                vec![Hint::AgentForwardDisabled]
            } else {
                Vec::new()
            },
            proxy_command: crate::import::proxy_command(&h.host),
            startup: crate::import::startup_line(&h.host),
            forwards: h
                .forwards
                .iter()
                .map(crate::import::preview_forward)
                .collect(),
        })
        .collect();
    ImportPreview {
        items,
        ..Default::default()
    }
}
