//! 导入的预览与执行：把导入计划写进存储。
//!
//! - 预览（[`preview`]）：计划中的主机加上"本机已有相同的主机"（协议、地址（不区分大小写）、端口、用户名都相同；
//!   已有主机没有填用户名、沿用凭据档案或分组默认值时，按沿用到的用户名比较）。
//! - 执行（[`apply`]）：只导入界面勾选的主机（按预览中的键）。导入前保存一份本机快照（保存失败只记警告）；
//!   来源中的私钥经 [`crate::keyvault::ensure`] 存入密钥库（按指纹去重）；分组按各级名称对应已有的分组，没有就新建；
//!   重复的主机按 [`OnDuplicate`] 跳过或合并；全部主机写完后再设置跳板机（被引用的主机可能排在后面）。
//!   某台主机、某把私钥失败时记在结果中，不影响其他的；整个导入的变更事件合并成一次发出。
//!
//! 新主机的认证依次取：私钥文件（来源中有它的密码时一并保存）、来源中的私钥（已存入密钥库）、来源中的密码、
//! ssh-agent；都没有时为密码认证、不保存密码（连接时询问）。随主机导入的端口转发（Xshell 的隧道）在新建、合并后添加，
//! 一律不自动启动；快捷命令（WindTerm）在主机之后添加，与已有的名称、内容都相同时不重复添加。合并时用导入的名称、分组、代理更新已有主机，保留已有的认证（选用凭据档案或沿用分组的、
//! 已有凭据的；只有导入的是私钥文件时照旧改用它）、标签、收藏、备注、跳板机（导入的带跳板机时更新）、连接与终端参数；
//! 已有主机还没有凭据时用来源中的补上。
//!
//! 密码要用户同意才导入的来源（Electerm、Netcatty，[`super::Source::secrets_need_consent`]）只补不改：跳过重复的主机时，
//! 已有主机是密码认证、还没保存密码的，补上来源中的密码（计入 [`ImportReport::passwords_filled`]）；合并时代理的密码
//! 也只在还没保存时写入。

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;

use super::{Endpoint, Hint, ImportPlan, PlannedHost};
use crate::backup::{BackupError, Backups, SnapshotReason};
use crate::id::{GroupId, HostId, KeyId};
use crate::keyvault::KeyVaultError;
use crate::model::{
    Auth, Forward, ForwardKind, Group, Host, Jump, KeySource, Profile, Protocol, SecretField,
    Snippet, join_host_port,
};
use crate::secret::SecretString;
use crate::store::{Owner, Position, Store};
use crate::vault::VaultStatus;
use crate::{CoreError, Invalid};

/// 重复的主机（协议、地址、端口、用户名都相同）的处理方式。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnDuplicate {
    /// 跳过，已有的主机不变（预览后默认不勾选重复的主机）
    #[default]
    Skip,
    /// 合并到已有的主机
    Merge,
}

/// 导入的预览：界面列出、搜索、勾选（见 [`super::select`]）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportPreview {
    pub items: Vec<PreviewItem>,
    /// 来源中跳过的会话数（RDP、VNC 等非 SSH 会话，缺少地址的会话，无法解密的记录）
    pub skipped: usize,
}

/// 预览中的一台主机。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewItem {
    /// 在本次导入中唯一的键，勾选与执行时用
    pub key: String,
    pub name: String,
    pub protocol: Protocol,
    /// 地址；串口为端口名
    pub address: String,
    /// 实际使用的端口
    pub port: u16,
    /// 实际使用的用户名（主机文件中沿用分组或凭据档案的也算上）；为空表示连接时用本机的用户名或分组的默认值
    pub username: String,
    /// 所在分组从根开始的各级名称；空表示不分组
    pub group: Vec<String>,
    pub tags: Vec<String>,
    /// 跳板机在来源中的名称
    pub jump: Option<String>,
    /// 带登录密码（会一并导入）
    pub has_password: bool,
    /// 带私钥（会存进密钥库或随主机保存）
    pub has_key: bool,
    /// 本机已有的相同主机
    pub duplicate_of: Option<HostId>,
    /// 需要提醒用户的事项（其他客户端的导入）
    pub hints: Vec<Hint>,
    /// 连接时会在本机执行的代理命令（导入前请用户确认来源可信）
    pub proxy_command: Option<String>,
    /// 登录后会在服务器上执行的命令（多行用"; "连接；主机文件）
    pub startup: Option<String>,
    /// 随主机导入的端口转发（主机文件；导入后不自动启动，合并时添加到已有的主机上，跳过时不导入）
    pub forwards: Vec<PreviewForward>,
}

/// 预览中随主机导入的一条端口转发。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewForward {
    pub kind: ForwardKind,
    pub name: String,
    /// 监听地址（按保存时的规则规范化：空与 localhost 为 127.0.0.1，远程转发的 `*` 为 0.0.0.0）
    pub bind_host: String,
    pub bind_port: u16,
    pub target_host: String,
    pub target_port: u16,
    /// 监听回环以外的地址：本地、动态转发时局域网内的其他设备也能访问，远程转发时能访问服务器的设备都能连到本机
    pub exposed: bool,
    /// 文件中设为自动启动（导入后不会自动启动，需要用户自己开启）
    pub auto_start: bool,
}

/// 导入的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportReport {
    /// 新建的主机数
    pub created: usize,
    /// 合并到已有主机的数量
    pub merged: usize,
    /// 已存在而跳过的数量
    pub skipped: usize,
    /// 新增的快捷命令数（WindTerm）
    pub snippets: usize,
    /// 跳过的已有主机中补上了密码的数量（Electerm、Netcatty：已有主机还没保存密码时）
    pub passwords_filled: usize,
    /// 没能导入的项与原因：主机，以及随主机导入的私钥、凭据档案、分组默认值、端口转发、跳板机、快捷命令
    pub failures: Vec<ImportFailure>,
    /// 每台勾选的主机的结果（按导入的顺序）
    pub hosts: Vec<ImportedHost>,
}

impl ImportReport {
    /// 没能导入的主机数。
    pub fn failed(&self) -> usize {
        self.hosts
            .iter()
            .filter(|h| h.outcome == HostOutcome::Failed)
            .count()
    }

    pub(crate) fn fail(&mut self, subject: FailureSubject, reason: impl Into<FailureReason>) {
        let reason = reason.into();
        tracing::warn!(?subject, ?reason, "导入时有一项没能导入");
        self.failures.push(ImportFailure { subject, reason });
    }
}

/// 一台勾选的主机的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedHost {
    /// 预览中的键
    pub key: String,
    pub name: String,
    pub outcome: HostOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostOutcome {
    Created(HostId),
    Merged(HostId),
    /// 已有相同的主机，跳过（值为已有主机的 ID）
    Skipped(HostId),
    /// 没能导入（原因见 [`ImportReport::failures`]）
    Failed,
}

impl HostOutcome {
    /// 新建、合并或跳过时对应的本机主机。
    pub fn host(&self) -> Option<&HostId> {
        match self {
            HostOutcome::Created(id) | HostOutcome::Merged(id) | HostOutcome::Skipped(id) => {
                Some(id)
            }
            HostOutcome::Failed => None,
        }
    }
}

/// 没能导入的一项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportFailure {
    pub subject: FailureSubject,
    pub reason: FailureReason,
}

/// 没能导入的是什么。名称都是来源中的名称。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FailureSubject {
    /// 主机没能导入（`key` 是预览中的键）
    Host { key: String, name: String },
    /// 主机已导入，但它的密码等秘密没能保存
    Secrets { host: String },
    /// 私钥没能存入密钥库：用它的主机改用密码认证
    Key { name: String },
    /// 凭据档案没能导入（主机文件）
    Profile { name: String },
    /// 分组的默认值没能写入（主机文件）
    Group { name: String },
    /// 端口转发没能导入（主机文件）：`label` 是规则的名称，没有名称时为监听地址
    Forward { host: String, label: String },
    /// 跳板机没能设置（主机文件中形成循环、超过层数等），主机保持原来的跳板设置
    Jump { host: String },
    /// 分组的默认跳板机没能设置（主机文件）
    GroupJump { group: String },
    /// 快捷命令没能导入（WindTerm）
    Snippet { name: String },
}

/// 失败的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FailureReason {
    /// 内容不合法：哪一项、为什么（如名称过长；端口转发的监听地址被占用为 `bindPort` / `conflict`）
    Invalid(Invalid),
    /// 私钥无法存入密钥库（格式不支持、已损坏等）
    Key(KeyVaultError),
    /// 保险库已锁定
    VaultLocked,
    /// 其他错误（数据库写入失败等），写给日志看的说明
    Other(String),
}

impl From<CoreError> for FailureReason {
    fn from(error: CoreError) -> Self {
        match error {
            CoreError::Invalid(invalid) => FailureReason::Invalid(invalid),
            CoreError::VaultLocked => FailureReason::VaultLocked,
            CoreError::Key(key) => FailureReason::Key(KeyVaultError::Key(key)),
            other => FailureReason::Other(other.to_string()),
        }
    }
}

impl From<KeyVaultError> for FailureReason {
    fn from(error: KeyVaultError) -> Self {
        match error {
            KeyVaultError::VaultLocked => FailureReason::VaultLocked,
            KeyVaultError::Invalid(invalid) => FailureReason::Invalid(invalid),
            other => FailureReason::Key(other),
        }
    }
}

// ---------------------------------------------------------------- 预览

/// 导入计划的预览：标出本机已有的相同主机。只读内存，很快，可以在界面线程调用。
pub fn preview(store: &Store, plan: &ImportPlan) -> ImportPreview {
    let existing = ExistingHosts::load(store);
    let items = plan
        .hosts
        .iter()
        .map(|h| PreviewItem {
            key: h.key.clone(),
            name: h.host.name.clone(),
            protocol: h.host.protocol,
            address: h.host.address.clone(),
            port: h.host.effective_port(),
            username: h.host.username.clone(),
            group: h
                .group
                .map(|g| plan.group_path(g).into_iter().map(String::from).collect())
                .unwrap_or_default(),
            tags: h.host.tags.clone(),
            jump: h.jump.as_ref().map(|j| j.name.clone()),
            has_password: h.has_password(),
            has_key: h.key().is_some(),
            duplicate_of: existing.find(h.host.protocol, &h.endpoint()).cloned(),
            hints: h.hints.clone(),
            proxy_command: proxy_command(&h.host),
            startup: startup_line(&h.host),
            forwards: h.forwards.iter().map(preview_forward).collect(),
        })
        .collect();
    ImportPreview {
        items,
        skipped: plan.skipped,
    }
}

/// 主机的代理命令（写成一行）。
pub(crate) fn proxy_command(host: &Host) -> Option<String> {
    host.proxy
        .as_ref()
        .filter(|p| p.kind == crate::model::ProxyKind::Command)
        .map(|p| one_line(&p.command))
}

/// 登录后执行的命令写成一行（多行用"; "连接）。
pub(crate) fn startup_line(host: &Host) -> Option<String> {
    let lines: Vec<&str> = host.startup_commands().map(str::trim).collect();
    (!lines.is_empty()).then(|| one_line(&lines.join("; ")))
}

/// 转发规则在预览中的样子：监听地址按保存时的规则规范化（不合法的按原样显示，导入时会失败），
/// 判断是否监听回环以外的地址。
pub(crate) fn preview_forward(fw: &Forward) -> PreviewForward {
    let mut normalized = fw.clone();
    normalized.host = Some(HostId::from_raw("preview"));
    let (bind_host, target_host) = match normalized.normalize() {
        Ok(()) => (normalized.bind_host, normalized.target_host),
        Err(_) => (fw.bind_host.clone(), fw.target_host.clone()),
    };
    let bind = bind_host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']');
    let exposed = !bind.is_empty()
        && !bind.eq_ignore_ascii_case("localhost")
        && bind.parse::<IpAddr>().map_or(true, |ip| !ip.is_loopback());
    PreviewForward {
        kind: fw.kind,
        name: fw.name.clone(),
        bind_host,
        bind_port: fw.bind_port,
        target_host,
        target_port: fw.target_port,
        exposed,
        auto_start: fw.auto_start,
    }
}

/// 去掉控制字符（显示、写进配置文件的内容不能跨行）。
pub(crate) fn one_line(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| if c < ' ' || c == '\u{7f}' { ' ' } else { c })
        .collect()
}

// ---------------------------------------------------------------- 执行

/// 导入计划中勾选的主机（`selected` 为 [`PreviewItem::key`]）。见模块说明。
///
/// 没有勾选时返回 [`BackupError::NothingSelected`]；要保存密码、私钥而保险库已锁定时返回
/// [`BackupError::VaultLocked`]（什么都不写）。其余的失败记在结果中。必须在核心的运行时上执行。
pub async fn apply(
    backups: Backups,
    plan: ImportPlan,
    selected: Vec<String>,
    on_duplicate: OnDuplicate,
) -> Result<ImportReport, BackupError> {
    let selected: HashSet<String> = selected.into_iter().collect();
    let chosen: Vec<&PlannedHost> = plan
        .hosts
        .iter()
        .filter(|h| selected.contains(&h.key))
        .collect();
    if chosen.is_empty() {
        return Err(BackupError::NothingSelected);
    }
    let store = backups.store().clone();
    let needs_vault = chosen.iter().any(|h| {
        h.has_password() || h.has_proxy_password() || h.passphrase().is_some() || h.key().is_some()
    });
    if needs_vault && store.vault_status() != VaultStatus::Unlocked {
        return Err(BackupError::VaultLocked);
    }
    let needed_keys: Vec<usize> = {
        let mut seen = HashSet::new();
        chosen
            .iter()
            .filter_map(|h| h.key())
            .filter(|k| seen.insert(*k))
            .collect()
    };
    snapshot_before_import(&backups).await;

    let mut report = ImportReport::default();
    let mut key_ids: HashMap<usize, KeyId> = HashMap::new();
    for index in needed_keys {
        let Some(key) = plan.keys.get(index) else {
            continue;
        };
        match crate::keyvault::ensure(store.clone(), key.name.clone(), key.private_key().clone())
            .await
        {
            Ok(id) => {
                key_ids.insert(index, id);
            }
            Err(e) => report.fail(
                FailureSubject::Key {
                    name: key.name.clone(),
                },
                e,
            ),
        }
    }

    let result = tokio::task::spawn_blocking(move || {
        write_plan(
            &store,
            &plan,
            &selected,
            on_duplicate,
            &key_ids,
            &mut report,
        );
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
                "导入主机"
            );
            Ok(report)
        }
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => Err(BackupError::Store(e.to_string())),
    }
}

/// 导入前保存一份本机快照（导入的结果不满意时可以回退）；保存失败只记警告。
pub(crate) async fn snapshot_before_import(backups: &Backups) {
    let backups = backups.clone();
    let saved =
        tokio::task::spawn_blocking(move || backups.snapshot_blocking(SnapshotReason::Import))
            .await;
    match saved {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!("导入前保存快照失败：{e}"),
        Err(e) => tracing::warn!("导入前保存快照失败：{e}"),
    }
}

fn write_plan(
    store: &Store,
    plan: &ImportPlan,
    selected: &HashSet<String>,
    on_duplicate: OnDuplicate,
    key_ids: &HashMap<usize, KeyId>,
    report: &mut ImportReport,
) {
    // 密码要用户同意才导入的来源：已有主机的秘密只补不改
    let fill_only = plan.source.secrets_need_consent();
    store.batch(|| {
        let mut groups = GroupIndex::load(store);
        let mut existing = ExistingHosts::load(store);
        // 跳板机在全部主机写完后再设置：被引用的主机可能排在后面
        let mut jumps: Vec<(HostId, String, Endpoint)> = Vec::new();
        for planned in plan.hosts.iter().filter(|h| selected.contains(&h.key)) {
            let name = planned.host.name.clone();
            let path = planned
                .group
                .map(|g| plan.group_path(g))
                .unwrap_or_default();
            let outcome = match groups.ensure(store, &path) {
                Err(e) => {
                    report.fail(
                        FailureSubject::Host {
                            key: planned.key.clone(),
                            name: name.clone(),
                        },
                        e,
                    );
                    HostOutcome::Failed
                }
                Ok(group) => {
                    let key = planned.key().and_then(|k| key_ids.get(&k).cloned());
                    let doc = planned.to_host(key, None);
                    write_host(
                        store,
                        planned,
                        doc,
                        group,
                        (on_duplicate, fill_only),
                        &mut existing,
                        report,
                    )
                }
            };
            if let HostOutcome::Created(id) | HostOutcome::Merged(id) = &outcome {
                write_forwards(store, id, planned, report);
                if let Some(jump) = &planned.jump {
                    jumps.push((id.clone(), name.clone(), jump.endpoint.clone()));
                }
            }
            report.hosts.push(ImportedHost {
                key: planned.key.clone(),
                name,
                outcome,
            });
        }
        for (id, name, endpoint) in jumps {
            // 跳板机没有导入（没有勾选等）时保持原来的跳板设置
            let Some(target) = existing.find(Protocol::Ssh, &endpoint).cloned() else {
                continue;
            };
            if target == id {
                continue;
            }
            match set_jump(store, &id, &target) {
                Ok(()) => {}
                // 形成循环或超过层数上限时保持原来的跳板设置，而不是让整个导入失败
                Err(CoreError::Invalid(_)) => {}
                Err(e) => report.fail(FailureSubject::Jump { host: name }, e),
            }
        }
        write_snippets(store, &plan.snippets, report);
    });
}

/// 为导入的主机添加来源中的转发规则：已有同类型、同监听地址的规则不重复添加；与本机其他规则的监听地址冲突时
/// 跳过并记下。导入的规则一律不自动启动，由用户确认后在端口转发中自己开启。
fn write_forwards(store: &Store, id: &HostId, planned: &PlannedHost, report: &mut ImportReport) {
    if planned.forwards.is_empty() {
        return;
    }
    let existing: Vec<Forward> = store
        .list::<Forward>()
        .into_iter()
        .filter(|f| f.doc.host.as_ref() == Some(id))
        .map(|f| f.doc)
        .collect();
    for rule in &planned.forwards {
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
        if let Err(e) = store.create(doc, None, Position::Last) {
            let label = if rule.name.trim().is_empty() {
                join_host_port(&rule.bind_host, rule.bind_port)
            } else {
                rule.name.clone()
            };
            report.fail(
                FailureSubject::Forward {
                    host: planned.host.name.clone(),
                    label,
                },
                e,
            );
        }
    }
}

/// 添加快捷命令：与已有的（以及本次已添加的）名称、内容都相同时跳过。
fn write_snippets(store: &Store, snippets: &[Snippet], report: &mut ImportReport) {
    if snippets.is_empty() {
        return;
    }
    let mut seen: HashSet<(String, String)> = store
        .list::<Snippet>()
        .into_iter()
        .map(|s| (s.doc.name.trim().to_string(), s.doc.content))
        .collect();
    for snippet in snippets {
        if !seen.insert((snippet.name.trim().to_string(), snippet.content.clone())) {
            continue;
        }
        match store.create(snippet.clone(), None, Position::Last) {
            Ok(_) => report.snippets += 1,
            Err(e) => report.fail(
                FailureSubject::Snippet {
                    name: snippet.name.clone(),
                },
                e,
            ),
        }
    }
}

/// 写一台主机：新建，或按 `on_duplicate` 跳过 / 合并到已有的相同主机（`fill_only`：已有主机的秘密只补不改，见模块说明）。
fn write_host(
    store: &Store,
    planned: &PlannedHost,
    doc: Host,
    group: Option<GroupId>,
    (on_duplicate, fill_only): (OnDuplicate, bool),
    existing: &mut ExistingHosts,
    report: &mut ImportReport,
) -> HostOutcome {
    let failure = || FailureSubject::Host {
        key: planned.key.clone(),
        name: planned.host.name.clone(),
    };
    let endpoint = planned.endpoint();
    if let Some(dup) = existing.find(doc.protocol, &endpoint).cloned() {
        if on_duplicate == OnDuplicate::Skip {
            report.skipped += 1;
            if fill_only {
                fill_password(store, &dup, planned, &doc, report);
            }
            return HostOutcome::Skipped(dup);
        }
        return match merge_planned(store, &dup, planned, doc, group, fill_only) {
            Ok(secrets) => {
                report.merged += 1;
                write_secrets(store, &dup, &planned.host.name, secrets, report);
                HostOutcome::Merged(dup)
            }
            Err(e) => {
                report.fail(failure(), e);
                HostOutcome::Failed
            }
        };
    }
    let mut secrets = auth_secrets(planned, &doc.auth);
    if doc.proxy.as_ref().is_some_and(|p| !p.username.is_empty())
        && let Some(password) = planned.proxy_password()
    {
        secrets.push((SecretField::ProxyPassword, password.clone()));
    }
    let protocol = doc.protocol;
    match store.create(doc, group.map(|g| g.to_string()), Position::Last) {
        Ok(id) => {
            report.created += 1;
            existing.insert(protocol, endpoint, id.clone());
            write_secrets(store, &id, &planned.host.name, secrets, report);
            HostOutcome::Created(id)
        }
        Err(e) => {
            report.fail(failure(), e);
            HostOutcome::Failed
        }
    }
}

/// 跳过的已有主机是密码认证、还没保存密码时，补上来源中的密码（导入的也是密码认证时）。
fn fill_password(
    store: &Store,
    id: &HostId,
    planned: &PlannedHost,
    doc: &Host,
    report: &mut ImportReport,
) {
    let Some(password) = planned.password() else {
        return;
    };
    let Some(old) = store.get::<Host>(id) else {
        return;
    };
    if doc.auth != Auth::Password
        || old.doc.auth != Auth::Password
        || store.has_secret(&Owner::host(id), SecretField::Password)
    {
        return;
    }
    let failures = report.failures.len();
    write_secrets(
        store,
        id,
        &planned.host.name,
        vec![(SecretField::Password, password.clone())],
        report,
    );
    if report.failures.len() == failures {
        report.passwords_filled += 1;
    }
}

/// 合并到已有的主机（见模块说明），返回要保存的秘密（`fill_only` 时已保存的代理密码不覆盖）。
fn merge_planned(
    store: &Store,
    id: &HostId,
    planned: &PlannedHost,
    doc: Host,
    group: Option<GroupId>,
    fill_only: bool,
) -> Result<Vec<(SecretField, SecretString)>, CoreError> {
    let old = store
        .get::<Host>(id)
        .ok_or(CoreError::NotFound(crate::model::Kind::Host.name()))?;
    let mut next = old.doc.clone();
    next.name = doc.name.clone();
    let mut secrets = Vec::new();
    // 私钥文件照旧更新；其余来源中的凭据只用于补上还没有凭据的主机。选用凭据档案（或沿用分组的）的主机保持不变
    let key_file = matches!(
        doc.auth,
        Auth::Key {
            source: KeySource::File { .. }
        }
    );
    let keep_auth = matches!(old.doc.auth, Auth::Profile { .. } | Auth::Inherit)
        || (!key_file && has_credentials(store, id, &old.doc));
    if !keep_auth {
        next.auth = doc.auth.clone();
        secrets.extend(auth_secrets(planned, &doc.auth));
    }
    // 导入的有代理时更新，没有时保留已有的
    if let Some(proxy) = &doc.proxy {
        next.proxy = Some(proxy.clone());
        if !proxy.username.is_empty()
            && let Some(password) = planned.proxy_password()
            && !(fill_only && store.has_secret(&Owner::host(id), SecretField::ProxyPassword))
        {
            secrets.push((SecretField::ProxyPassword, password.clone()));
        }
    }
    store.update(id, next)?;
    move_into(store, id, old.parent.as_deref(), group)?;
    Ok(secrets)
}

/// 认证用到的来源中的秘密：密码认证时的登录密码，私钥文件的密码。
fn auth_secrets(planned: &PlannedHost, auth: &Auth) -> Vec<(SecretField, SecretString)> {
    let secret = match auth {
        Auth::Password => planned
            .password()
            .map(|p| (SecretField::Password, p.clone())),
        Auth::Key {
            source: KeySource::File { .. },
        } => planned
            .passphrase()
            .map(|p| (SecretField::Passphrase, p.clone())),
        _ => None,
    };
    secret.into_iter().collect()
}

/// 导入的有分组时把已有的主机移进去，没有时留在原来的分组。
pub(crate) fn move_into(
    store: &Store,
    id: &HostId,
    current: Option<&str>,
    group: Option<GroupId>,
) -> Result<(), CoreError> {
    match group {
        Some(group) if current != Some(group.as_str()) => {
            store.move_to::<Host>(id, Some(group.to_string()), Position::Last)
        }
        _ => Ok(()),
    }
}

/// 保存主机的秘密；失败时记下（主机本身已导入）。
pub(crate) fn write_secrets(
    store: &Store,
    id: &HostId,
    name: &str,
    secrets: Vec<(SecretField, SecretString)>,
    report: &mut ImportReport,
) {
    let owner = Owner::host(id);
    for (field, value) in secrets {
        if let Err(e) = store.set_secret(&owner, field, Some(&value)) {
            report.fail(
                FailureSubject::Secrets {
                    host: name.to_string(),
                },
                e,
            );
            return;
        }
    }
}

/// 已有主机是否已有凭据：私钥、ssh-agent、凭据档案（含沿用分组的），或保存了密码。
pub(crate) fn has_credentials(store: &Store, id: &HostId, host: &Host) -> bool {
    match host.auth {
        Auth::Password => store.has_secret(&Owner::host(id), SecretField::Password),
        _ => true,
    }
}

/// 只修改主机的跳板机（存储校验存在、不形成循环、层数上限）。
pub(crate) fn set_jump(store: &Store, id: &HostId, jump: &HostId) -> Result<(), CoreError> {
    let mut doc = store
        .get::<Host>(id)
        .ok_or(CoreError::NotFound(crate::model::Kind::Host.name()))?
        .doc;
    doc.jump = Jump::Via { host: jump.clone() };
    store.update(id, doc)
}

/// 已有主机实际使用的用户名：主机自己的 > 凭据档案的（含沿用分组默认的档案）> 分组逐级向上的默认用户名；
/// 都没有时为空（与连接时的解析相同，只是不取本机的用户名）。
pub(crate) fn effective_username(store: &Store, host: &Host, parent: Option<&str>) -> String {
    let own = host.username.trim();
    if !own.is_empty() {
        return own.to_string();
    }
    let chain = store.group_chain(parent.map(GroupId::from_raw).as_ref());
    let profile = match &host.auth {
        Auth::Profile { profile } => Some(profile.clone()),
        Auth::Inherit => chain.iter().find_map(|g| g.doc.defaults.profile.clone()),
        _ => None,
    };
    if let Some(profile) = profile.and_then(|id| store.get::<Profile>(&id)) {
        let username = profile.doc.username.trim();
        if !username.is_empty() {
            return username.to_string();
        }
    }
    chain
        .iter()
        .map(|g| g.doc.defaults.username.trim())
        .find(|u| !u.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// 本机已有的主机，按协议与地址、端口、用户名查找（相同的有多台时取主机列表中的第一台）。
pub(crate) struct ExistingHosts {
    /// 键的协议用它的序号（`Protocol` 没有实现 `Hash`）
    by_endpoint: HashMap<(u8, Endpoint), HostId>,
}

impl ExistingHosts {
    pub fn load(store: &Store) -> Self {
        let mut by_endpoint = HashMap::new();
        for record in store.list::<Host>() {
            let username = effective_username(store, &record.doc, record.parent.as_deref());
            let endpoint =
                Endpoint::new(&record.doc.address, record.doc.effective_port(), &username);
            by_endpoint
                .entry((record.doc.protocol as u8, endpoint))
                .or_insert(record.id);
        }
        Self { by_endpoint }
    }

    pub fn find(&self, protocol: Protocol, endpoint: &Endpoint) -> Option<&HostId> {
        self.by_endpoint.get(&(protocol as u8, endpoint.clone()))
    }

    pub fn insert(&mut self, protocol: Protocol, endpoint: Endpoint, id: HostId) {
        self.by_endpoint
            .entry((protocol as u8, endpoint))
            .or_insert(id);
    }
}

/// 本机的分组，按上级与名称查找；找不到时新建（名称完全相同才算同一个分组）。
pub(crate) struct GroupIndex {
    by_name: HashMap<(Option<String>, String), GroupId>,
}

impl GroupIndex {
    pub fn load(store: &Store) -> Self {
        let mut by_name = HashMap::new();
        for record in store.list::<Group>() {
            by_name
                .entry((record.parent.clone(), record.doc.name.clone()))
                .or_insert(record.id);
        }
        Self { by_name }
    }

    /// 按各级名称找到或新建分组，返回最后一级；名称去掉首尾空白，空名称跳过；没有名称时为 `None`（不分组）。
    pub fn ensure<S: AsRef<str>>(
        &mut self,
        store: &Store,
        path: &[S],
    ) -> Result<Option<GroupId>, CoreError> {
        let mut parent: Option<GroupId> = None;
        for name in path {
            let name = name.as_ref().trim();
            if name.is_empty() {
                continue;
            }
            let key = (parent.as_ref().map(|p| p.to_string()), name.to_string());
            let id = match self.by_name.get(&key) {
                Some(id) => id.clone(),
                None => {
                    let id = store.create(
                        Group {
                            name: name.to_string(),
                            ..Default::default()
                        },
                        key.0.clone(),
                        Position::Last,
                    )?;
                    self.by_name.insert(key, id.clone());
                    id
                }
            };
            parent = Some(id);
        }
        Ok(parent)
    }
}

#[cfg(test)]
mod tests;
