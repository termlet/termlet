//! 导入 Termlet 的主机文件。

use std::collections::{HashMap, HashSet};

use super::{FileDoc, FileHost, FileProfile, HostFile, PROFILE_SECRETS};
use crate::CoreError;
use crate::backup::{BackupError, Backups};
use crate::id::{GroupId, HostId, KeyId, ProfileId};
use crate::import::{
    ExistingHosts, FailureSubject, GroupIndex, HostOutcome, ImportReport, ImportedHost,
    OnDuplicate, has_credentials, move_into, set_jump, snapshot_before_import, write_secrets,
};
use crate::model::{
    Auth, CliAuth, Forward, Group, Host, Jump, KeyEntry, KeySource, Profile, ProfileAuth,
    SecretField, join_host_port,
};
use crate::secret::SecretString;
use crate::store::{Owner, Position, Store};
use crate::vault::VaultStatus;

/// 主机的认证秘密（合并到已有凭据的主机时不写）。
const AUTH_SECRETS: [SecretField; 3] = [
    SecretField::Password,
    SecretField::PrivateKey,
    SecretField::Passphrase,
];
const CLI_SECRETS: [SecretField; 3] = [
    SecretField::CliPassword,
    SecretField::CliPrivateKey,
    SecretField::CliPassphrase,
];

/// 导入文件中选中的主机（`selected` 为预览中的键）。`on_duplicate` 的含义与其他来源的导入相同：
/// 合并时用文件中的名称、分组、连接与终端设置、代理更新已有主机，保留已有的地址、用户名、凭据（已有主机没有凭据时
/// 用文件中的补上）、TOTP 密钥、标签、收藏、命令行工具设置与跳板机（文件中有跳板机时随后设置）。
///
/// 依次处理：密钥（文件中有私钥时存入密钥库，同一把只存一次；没有私钥时用本机密钥库中指纹相同的）、凭据档案
/// （本机已有名称与用户名都相同的直接使用，否则新建）、分组（按各级名称对应；本机的分组还没有默认值时写入文件中的）、
/// 主机与端口转发、跳板机（本次导入的，或地址、端口、用户名相同的已有主机；找不到时保持原来的设置）、分组的默认跳板机。
/// 某一项失败时记在结果中，不影响其他的。导入前保存一份本机快照。必须在核心的运行时上执行。
pub async fn import(
    backups: Backups,
    file: HostFile,
    selected: Vec<String>,
    on_duplicate: OnDuplicate,
) -> Result<ImportReport, BackupError> {
    let doc = file.doc;
    let selected: HashSet<String> = selected.into_iter().collect();
    let chosen: Vec<&FileHost> = doc
        .hosts
        .iter()
        .filter(|h| selected.contains(&h.id))
        .collect();
    if chosen.is_empty() {
        return Err(BackupError::NothingSelected);
    }
    let store = backups.store().clone();
    let needed = Needed::of(&doc, &chosen);
    let needs_vault = chosen.iter().any(|h| !h.secrets.is_empty())
        || needed
            .profiles
            .iter()
            .filter_map(|p| doc.profile(p))
            .any(|p| !p.secrets.is_empty())
        || needed
            .keys
            .iter()
            .filter_map(|k| doc.key(k))
            .any(|k| k.private_key.is_some());
    if needs_vault && store.vault_status() != VaultStatus::Unlocked {
        return Err(BackupError::VaultLocked);
    }
    snapshot_before_import(&backups).await;

    // 1. 密钥：文件中有私钥时存入密钥库（同一把密钥只存一次）；没有私钥时使用本机密钥库中指纹相同的密钥
    let mut report = ImportReport::default();
    let mut key_ids: HashMap<String, KeyId> = HashMap::new();
    let local_keys = store.list::<KeyEntry>();
    for key in doc.keys.iter().filter(|k| needed.keys.contains(&k.id)) {
        match &key.private_key {
            Some(private_key) if !private_key.0.is_empty() => {
                match crate::keyvault::ensure(
                    store.clone(),
                    key.name.clone(),
                    private_key.0.clone(),
                )
                .await
                {
                    Ok(id) => {
                        key_ids.insert(key.id.clone(), id);
                    }
                    Err(e) => report.fail(
                        FailureSubject::Key {
                            name: key.name.clone(),
                        },
                        e,
                    ),
                }
            }
            _ => {
                if let Some(local) = local_keys
                    .iter()
                    .find(|l| l.doc.fingerprint == key.fingerprint)
                {
                    key_ids.insert(key.id.clone(), local.id.clone());
                }
            }
        }
        // 文件中的密钥附带了证书：本机的这把密钥还没有证书时补上（与私钥对不上的不用）
        if let Some(id) = key_ids.get(&key.id)
            && !key.certificate.is_empty()
            && store
                .get::<KeyEntry>(id)
                .is_some_and(|e| e.doc.certificate.is_empty())
            && let Err(e) = crate::keyvault::attach_certificate(
                store.clone(),
                id.clone(),
                crate::keyvault::CertificateSource::Text(key.certificate.clone()),
            )
            .await
        {
            tracing::info!(key = %key.name, "文件中密钥附带的证书没有用上：{e}");
        }
    }

    let result = tokio::task::spawn_blocking(move || {
        let mut writer = Writer {
            store: &store,
            doc: &doc,
            selected: &selected,
            keys: key_ids,
            profiles: HashMap::new(),
            report: &mut report,
        };
        store.batch(|| writer.run(&needed, on_duplicate));
        report
    })
    .await;
    match result {
        Ok(report) => {
            tracing::info!(
                created = report.created,
                merged = report.merged,
                skipped = report.skipped,
                failures = report.failures.len(),
                "导入主机文件"
            );
            Ok(report)
        }
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => Err(BackupError::Store(e.to_string())),
    }
}

/// 选中的主机用到的凭据档案与密钥（文件中的 `ref`）。
struct Needed {
    profiles: HashSet<String>,
    keys: HashSet<String>,
    /// 选中的主机所在的分组及其上级分组
    groups: HashSet<String>,
}

impl Needed {
    fn of(doc: &FileDoc, chosen: &[&FileHost]) -> Self {
        let mut needed = Needed {
            profiles: HashSet::new(),
            keys: HashSet::new(),
            groups: HashSet::new(),
        };
        let library = |source: &KeySource| match source {
            KeySource::Library { key } => Some(key.as_str().to_string()),
            _ => None,
        };
        for host in chosen {
            for group in doc.group_chain(host.group.as_deref()) {
                needed.groups.insert(group.id.clone());
                if let Some(profile) = &group.defaults.profile {
                    needed.profiles.insert(profile.as_str().to_string());
                }
            }
            match &host.host.auth {
                Auth::Profile { profile } => {
                    needed.profiles.insert(profile.as_str().to_string());
                }
                Auth::Key { source } => needed.keys.extend(library(source)),
                _ => {}
            }
            if let Some(credential) = &host.host.cli.credential
                && let CliAuth::Key { source } = &credential.auth
            {
                needed.keys.extend(library(source));
            }
        }
        for profile in doc
            .profiles
            .iter()
            .filter(|p| needed.profiles.contains(&p.id))
        {
            if let ProfileAuth::Key { source } = &profile.profile.auth {
                needed.keys.extend(library(source));
            }
        }
        needed
    }
}

struct Writer<'a> {
    store: &'a Store,
    doc: &'a FileDoc,
    selected: &'a HashSet<String>,
    /// 文件中的密钥 `ref` → 本机密钥库中的 ID
    keys: HashMap<String, KeyId>,
    /// 文件中的凭据档案 `ref` → 本机的 ID
    profiles: HashMap<String, ProfileId>,
    report: &'a mut ImportReport,
}

impl<'a> Writer<'a> {
    fn run(&mut self, needed: &Needed, on_duplicate: OnDuplicate) {
        self.import_profiles(needed);
        let (group_ids, pending_jumps) = self.import_groups(needed);
        let (host_ids, imported, existing) = self.import_hosts(&group_ids, on_duplicate);

        // 跳板机：本次导入的，或地址、端口、用户名相同的已有主机；找不到时保持原来的设置
        let resolve = |reference: &str| -> Option<HostId> {
            host_ids.get(reference).cloned().or_else(|| {
                let jump = self.doc.host(reference)?;
                existing
                    .find(jump.host.protocol, &self.doc.endpoint(jump))
                    .cloned()
            })
        };
        let mut failures = Vec::new();
        for (host, id) in &imported {
            let Jump::Via { host: reference } = &host.host.jump else {
                continue;
            };
            let Some(target) = resolve(reference.as_str()) else {
                continue;
            };
            if &target == id {
                continue;
            }
            if let Err(e) = set_jump(self.store, id, &target) {
                failures.push((
                    FailureSubject::Jump {
                        host: host.host.name.clone(),
                    },
                    e,
                ));
            }
        }
        for (group, reference) in pending_jumps {
            let Some(target) = resolve(&reference) else {
                continue;
            };
            let Some(record) = self.store.get::<Group>(&group) else {
                continue;
            };
            let mut doc = record.doc;
            doc.defaults.jump = Some(target);
            if let Err(e) = self.store.update(&group, doc.clone()) {
                failures.push((FailureSubject::GroupJump { group: doc.name }, e));
            }
        }
        for (subject, e) in failures {
            self.report.fail(subject, e);
        }
    }

    /// 凭据档案：本机已有名称与用户名都相同的档案时直接使用（不改动它），否则新建。
    fn import_profiles(&mut self, needed: &Needed) {
        let existing: Vec<(String, String, ProfileId)> = self
            .store
            .list::<Profile>()
            .into_iter()
            .map(|p| {
                (
                    p.doc.name.trim().to_string(),
                    p.doc.username.trim().to_string(),
                    p.id,
                )
            })
            .collect();
        let doc: &'a FileDoc = self.doc;
        for file in doc
            .profiles
            .iter()
            .filter(|p| needed.profiles.contains(&p.id))
        {
            let name = file.profile.name.trim();
            let username = file.profile.username.trim();
            if let Some((_, _, id)) = existing.iter().find(|(n, u, _)| n == name && u == username) {
                self.profiles.insert(file.id.clone(), id.clone());
                continue;
            }
            match self.create_profile(file) {
                Ok(id) => {
                    self.profiles.insert(file.id.clone(), id);
                }
                Err(e) => self.report.fail(
                    FailureSubject::Profile {
                        name: file.profile.name.clone(),
                    },
                    e,
                ),
            }
        }
    }

    fn create_profile(&mut self, file: &FileProfile) -> Result<ProfileId, CoreError> {
        let mut doc = file.profile.clone();
        doc.auth = match &file.profile.auth {
            ProfileAuth::Key { source } => {
                match self.localize_source(source, file.secrets.has(SecretField::PrivateKey)) {
                    Some(source) => ProfileAuth::Key { source },
                    None => ProfileAuth::Password,
                }
            }
            other => other.clone(),
        };
        let created = self.store.create(doc.clone(), None, Position::Last);
        let id = match created {
            Ok(id) => id,
            // 认证不可用（例如引用的密钥没能导入）：不带凭据导入，之后补填
            Err(e) if doc.auth != ProfileAuth::Password => {
                self.report.fail(
                    FailureSubject::Profile {
                        name: file.profile.name.clone(),
                    },
                    e,
                );
                doc.auth = ProfileAuth::Password;
                return self.store.create(doc, None, Position::Last);
            }
            Err(e) => return Err(e),
        };
        let owner = Owner::profile(&id);
        for (field, value) in file.secrets.pick(&PROFILE_SECRETS) {
            if let Err(e) = self.store.set_secret(&owner, field, Some(&value)) {
                self.report.fail(
                    FailureSubject::Profile {
                        name: file.profile.name.clone(),
                    },
                    e,
                );
                break;
            }
        }
        Ok(id)
    }

    /// 分组：按各级名称建立；本机的分组还没有设置默认值时，写入文件中的默认用户名、凭据档案与终端配色
    /// （跳板机在主机导入后设置）。返回文件中的分组 `ref` → 本机的 ID，与待设置的默认跳板机。
    fn import_groups(
        &mut self,
        needed: &Needed,
    ) -> (HashMap<String, GroupId>, Vec<(GroupId, String)>) {
        let doc: &'a FileDoc = self.doc;
        let mut index = GroupIndex::load(self.store);
        let mut ids = HashMap::new();
        let mut pending = Vec::new();
        for file in doc.groups.iter().filter(|g| needed.groups.contains(&g.id)) {
            let path = doc.group_path(Some(&file.id));
            let id = match index.ensure(self.store, &path) {
                Ok(Some(id)) => id,
                Ok(None) => continue,
                Err(e) => {
                    self.report.fail(
                        FailureSubject::Group {
                            name: file.name.clone(),
                        },
                        e,
                    );
                    continue;
                }
            };
            ids.insert(file.id.clone(), id.clone());
            let Some(local) = self.store.get::<Group>(&id) else {
                continue;
            };
            let current = &local.doc.defaults;
            let empty = current.username.trim().is_empty()
                && current.profile.is_none()
                && current.jump.is_none()
                && current.color_scheme.is_empty();
            let wanted = &file.defaults;
            let has_defaults = !wanted.username.trim().is_empty()
                || wanted.profile.is_some()
                || wanted.jump.is_some()
                || !wanted.color_scheme.is_empty();
            if !empty || !has_defaults {
                continue;
            }
            let mut doc = local.doc.clone();
            doc.defaults.username = wanted.username.clone();
            doc.defaults.profile = wanted
                .profile
                .as_ref()
                .and_then(|p| self.profiles.get(p.as_str()).cloned());
            doc.defaults.color_scheme = wanted.color_scheme.clone();
            if let Err(e) = self.store.update(&id, doc) {
                self.report.fail(
                    FailureSubject::Group {
                        name: file.name.clone(),
                    },
                    e,
                );
                continue;
            }
            if let Some(jump) = &wanted.jump {
                pending.push((id, jump.as_str().to_string()));
            }
        }
        (ids, pending)
    }

    /// 主机与端口转发。返回文件中的主机 `ref` → 本机的 ID（新建、合并或跳过的已有主机）、新建与合并的主机、
    /// 已有主机的索引（含本次新建的）。
    fn import_hosts(
        &mut self,
        group_ids: &HashMap<String, GroupId>,
        on_duplicate: OnDuplicate,
    ) -> (
        HashMap<String, HostId>,
        Vec<(&'a FileHost, HostId)>,
        ExistingHosts,
    ) {
        let doc: &'a FileDoc = self.doc;
        let selected: &'a HashSet<String> = self.selected;
        let mut existing = ExistingHosts::load(self.store);
        let mut index = GroupIndex::load(self.store);
        let mut host_ids = HashMap::new();
        let mut imported = Vec::new();
        for host in doc.hosts.iter().filter(|h| selected.contains(&h.id)) {
            let name = host.host.name.clone();
            let failure = || FailureSubject::Host {
                key: host.id.clone(),
                name: name.clone(),
            };
            // 文件中的分组已在前面建立；文件中没有列出的分组按名称再找一次
            let group = match host.group.as_deref() {
                None => Ok(None),
                Some(reference) => match group_ids.get(reference) {
                    Some(id) => Ok(Some(id.clone())),
                    None => index.ensure(self.store, &doc.group_path(Some(reference))),
                },
            };
            let group = match group {
                Ok(group) => group,
                Err(e) => {
                    self.report.fail(failure(), e);
                    self.push(host, HostOutcome::Failed);
                    continue;
                }
            };
            let endpoint = doc.endpoint(host);
            let protocol = host.host.protocol;
            let (local, secrets) = self.localize_host(host);
            let outcome = match existing.find(protocol, &endpoint).cloned() {
                Some(dup) if on_duplicate == OnDuplicate::Skip => {
                    self.report.skipped += 1;
                    host_ids.insert(host.id.clone(), dup.clone());
                    HostOutcome::Skipped(dup)
                }
                Some(dup) => match self.merge(&dup, local, secrets, group) {
                    Ok(secrets) => {
                        self.report.merged += 1;
                        write_secrets(self.store, &dup, &name, secrets, self.report);
                        HostOutcome::Merged(dup)
                    }
                    Err(e) => {
                        self.report.fail(failure(), e);
                        HostOutcome::Failed
                    }
                },
                None => match self
                    .store
                    .create(local, group.map(|g| g.to_string()), Position::Last)
                {
                    Ok(id) => {
                        self.report.created += 1;
                        existing.insert(protocol, endpoint, id.clone());
                        write_secrets(self.store, &id, &name, secrets, self.report);
                        HostOutcome::Created(id)
                    }
                    Err(e) => {
                        self.report.fail(failure(), e);
                        HostOutcome::Failed
                    }
                },
            };
            if let HostOutcome::Created(id) | HostOutcome::Merged(id) = &outcome {
                host_ids.insert(host.id.clone(), id.clone());
                imported.push((host, id.clone()));
                self.import_forwards(id, host);
            }
            self.push(host, outcome);
        }
        (host_ids, imported, existing)
    }

    fn push(&mut self, host: &FileHost, outcome: HostOutcome) {
        self.report.hosts.push(ImportedHost {
            key: host.id.clone(),
            name: host.host.name.clone(),
            outcome,
        });
    }

    /// 文件中的密钥来源换成本机的：密钥库中的密钥换成本机的 ID（没能导入时为 `None`），粘贴的私钥要文件中有私钥。
    fn localize_source(&self, source: &KeySource, has_pasted: bool) -> Option<KeySource> {
        match source {
            KeySource::Library { key } => self
                .keys
                .get(key.as_str())
                .map(|key| KeySource::Library { key: key.clone() }),
            KeySource::File { .. } => Some(source.clone()),
            KeySource::Pasted { .. } => has_pasted.then(|| source.clone()),
        }
    }

    /// 文件中的主机换成新建本机主机的内容与要保存的秘密。导入的主机不允许命令行工具访问；跳板机随后设置。
    fn localize_host(&self, file: &FileHost) -> (Host, Vec<(SecretField, SecretString)>) {
        let mut host = file.host.clone();
        let mut secrets = file.secrets.pick(&super::HOST_SECRETS);
        host.cli.allowed = false;
        // 别人给的文件中开启的 agent 转发会让对方的服务器借用本机的 agent，导入后关闭（预览中提示）
        host.ssh.agent_forward = false;
        host.auth = match &file.host.auth {
            Auth::Profile { profile } => match self.profiles.get(profile.as_str()) {
                Some(id) => Auth::Profile {
                    profile: id.clone(),
                },
                None => Auth::Password,
            },
            Auth::Key { source } => {
                match self.localize_source(source, file.secrets.has(SecretField::PrivateKey)) {
                    Some(source) => Auth::Key { source },
                    None => Auth::Password,
                }
            }
            other => other.clone(),
        };
        if let Jump::Via { .. } = host.jump {
            host.jump = Jump::Inherit;
        }
        if let Some(credential) = &mut host.cli.credential {
            // 命令行工具专用凭据要有可用的密码或私钥（ssh-agent 不导入）
            let auth = match &credential.auth {
                CliAuth::Agent => None,
                CliAuth::Password => file
                    .secrets
                    .has(SecretField::CliPassword)
                    .then_some(CliAuth::Password),
                CliAuth::Key { source } => self
                    .localize_source(source, file.secrets.has(SecretField::CliPrivateKey))
                    .map(|source| CliAuth::Key { source }),
            };
            match auth {
                Some(auth) => credential.auth = auth,
                None => {
                    host.cli.credential = None;
                    secrets.retain(|(f, _)| !CLI_SECRETS.contains(f));
                }
            }
        }
        (host, secrets)
    }

    /// 合并到已有的主机，返回要保存的秘密。
    fn merge(
        &self,
        id: &HostId,
        mut next: Host,
        mut secrets: Vec<(SecretField, SecretString)>,
        group: Option<GroupId>,
    ) -> Result<Vec<(SecretField, SecretString)>, CoreError> {
        let old = self
            .store
            .get::<Host>(id)
            .ok_or(CoreError::NotFound(crate::model::Kind::Host.name()))?;
        next.address = old.doc.address.clone();
        next.port = old.doc.port;
        next.username = old.doc.username.clone();
        next.protocol = old.doc.protocol;
        next.tags = old.doc.tags.clone();
        next.favorite = old.doc.favorite;
        next.cli = old.doc.cli.clone();
        next.jump = old.doc.jump.clone();
        if next.note.trim().is_empty() {
            next.note = old.doc.note.clone();
        }
        secrets.retain(|(f, _)| !CLI_SECRETS.contains(f));
        if has_credentials(self.store, id, &old.doc) {
            next.auth = old.doc.auth.clone();
            secrets.retain(|(f, _)| !AUTH_SECRETS.contains(f));
        }
        let owner = Owner::host(id);
        if self.store.has_secret(&owner, SecretField::Totp) {
            secrets.retain(|(f, _)| *f != SecretField::Totp);
        }
        if next.proxy.is_none() {
            next.proxy = old.doc.proxy.clone();
            secrets.retain(|(f, _)| *f != SecretField::ProxyPassword);
        }
        self.store.update(id, next)?;
        move_into(self.store, id, old.parent.as_deref(), group)?;
        Ok(secrets)
    }

    /// 为导入的主机添加文件中的转发规则：已有同类型、同监听地址的规则不重复添加；与本机其他规则的监听地址冲突时
    /// 跳过并记下。导入的规则一律不自动启动（文件中设为自动启动的也是）：文件可能来自别人，合并时规则还会挂到本机
    /// 已有的主机上、用本机的凭据连接，由用户确认后在端口转发中自己开启。
    fn import_forwards(&mut self, id: &HostId, file: &FileHost) {
        if file.forwards.is_empty() {
            return;
        }
        let existing: Vec<Forward> = self
            .store
            .list::<Forward>()
            .into_iter()
            .filter(|f| f.doc.host.as_ref() == Some(id))
            .map(|f| f.doc)
            .collect();
        for rule in &file.forwards {
            let doc = Forward {
                host: Some(id.clone()),
                auto_start: false,
                ..rule.clone()
            };
            let mut normalized = doc.clone();
            if normalized.normalize().is_ok()
                && existing.iter().any(|r| {
                    r.kind == normalized.kind
                        && r.bind_host == normalized.bind_host
                        && r.bind_port == normalized.bind_port
                })
            {
                continue;
            }
            if let Err(e) = self.store.create(doc, None, Position::Last) {
                let label = if rule.name.trim().is_empty() {
                    join_host_port(&rule.bind_host, rule.bind_port)
                } else {
                    rule.name.clone()
                };
                self.report.fail(
                    FailureSubject::Forward {
                        host: file.host.name.clone(),
                        label,
                    },
                    e,
                );
            }
        }
    }
}
