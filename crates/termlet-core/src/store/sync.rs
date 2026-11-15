//! 云同步用的批量读写（见 [`crate::cloudsync`]）。
//!
//! - [`Store::sync_snapshot`]：在一把读锁下读出全部条目、同步的设置组与秘密（秘密解密），各部分是同一时刻的内容。
//! - [`Store::sync_apply`]：在写锁下先由调用方确认本机的数据仍与读取时相同，再把同步拉取到的变更写进内存的副本、
//!   修复引用（两边都没有了的引用改为空或删除那一条、监听同一地址的端口转发只留写入的那条、分组与跳板机不成环），
//!   最后在一个事务中写库、替换内存并发出变更事件。写库失败时内存不变，什么都没写。
//!   本机写不进、本机也没有的条目（其他版本写的、本机解析不了）不算“两边都没有了”：引用它的条目、它的秘密也不写入，
//!   否则修复引用会删掉它们、改掉跳板机，并当作本机的修改传到其他设备。
//! - [`Store::sync_purge_tombstones`]：同步完成后清理已经同步出去的墓碑。
//! - 主机识别出的系统（[`Host::os`]）不是用户的修改，不在条目记录中，单独写入（[`RawChange::SetHostOs`]，不改修改时间）；
//!   写入条目时保留本机的值（[`Item::keep_observed`]）。
//!
//! 同步记录的格式与合并规则都在 `cloudsync` 中；这里只认条目、设置组与秘密。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use rusqlite::{Connection, params};
use serde_json::{Map, Value};

use super::{Collection, Record, State, Store, StoreEvent, Stored, collection, secrets};
use crate::db::{self, SettingRow};
use crate::model::{
    AppearanceSettings, Auth, CliAuth, ColorScheme, ConnectionSettings, Forward, GeneralSettings,
    Group, GuardSettings, HighlightSettings, Host, Item, ItemId, Jump, KeyEntry, KeySource,
    KeybindingSettings, Kind, Profile, ProfileAuth, Settings, SettingsSection, Snippet,
    TerminalSettings, Workspace,
};
use crate::secret::SecretString;
use crate::vault::Vault;
use crate::{CoreError, Result};

/// 对一种条目的类型执行 `$body`（`$T` 是这种条目的类型）。
macro_rules! by_kind {
    ($kind:expr, $T:ident => $body:expr) => {
        match $kind {
            Kind::Host => {
                type $T = Host;
                $body
            }
            Kind::Group => {
                type $T = Group;
                $body
            }
            Kind::Profile => {
                type $T = Profile;
                $body
            }
            Kind::Key => {
                type $T = KeyEntry;
                $body
            }
            Kind::Snippet => {
                type $T = Snippet;
                $body
            }
            Kind::Forward => {
                type $T = Forward;
                $body
            }
            Kind::Workspace => {
                type $T = Workspace;
                $body
            }
            Kind::Scheme => {
                type $T = ColorScheme;
                $body
            }
        }
    };
}

/// 一条条目（`doc` 是条目内容的 JSON）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RawItem {
    pub kind: Kind,
    pub id: String,
    pub parent: Option<String>,
    pub sort_key: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub doc: Value,
}

/// 一组同步的设置。`updated_at` 为 0 表示从没改过（数据库中没有这一行，用的是默认值）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RawSetting {
    pub id: &'static str,
    pub doc: Value,
    pub updated_at: i64,
}

/// 一个条目的全部秘密（字段名 → 明文）。`updated_at` 取各项秘密与条目本身最近的修改时间。
#[derive(Clone, PartialEq)]
pub(crate) struct RawSecrets {
    pub kind: Kind,
    pub id: String,
    pub fields: BTreeMap<String, SecretString>,
    pub updated_at: i64,
}

impl std::fmt::Debug for RawSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawSecrets")
            .field("kind", &self.kind)
            .field("id", &self.id)
            .field("fields", &self.fields.keys().collect::<Vec<_>>())
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

/// 本机的全部可同步数据（不含墓碑）。
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct RawSnapshot {
    pub items: Vec<RawItem>,
    pub settings: Vec<RawSetting>,
    pub secrets: Vec<RawSecrets>,
}

/// 同步要写入本机的一项变更。
#[derive(Clone, Debug)]
pub(crate) enum RawChange {
    /// 新建或覆盖一条条目（内容解析不了、校验不通过时不写入）
    PutItem(RawItem),
    /// 删除一条条目（留下墓碑）。它的秘密、引用它的条目由各自的变更处理，剩下的悬空引用在写入后修复
    DeleteItem { kind: Kind, id: String },
    /// 替换一组设置；`keep_local` 中的键保留本机当前的值（只属于本机的项）
    PutSetting {
        id: String,
        doc: Value,
        updated_at: i64,
        keep_local: &'static [&'static str],
    },
    /// 整组替换一个条目的秘密
    PutSecrets(RawSecrets),
    /// 删除一个条目的全部秘密
    DeleteSecrets { kind: Kind, id: String },
    /// 写入（`None` 为清除）一台主机识别出的系统（[`Host::os`]，键的格式已检查）。不改主机的修改时间；在其余变更之后
    /// 写入（主机可能是这次才新建的），主机不在时不写（不算失败）
    SetHostOs { id: String, os: Option<String> },
    /// 云端有这条条目，但内容本机认不出（其他版本写的）：不写入，记为失败（见模块说明）
    Unreadable { kind: Kind, id: String },
}

/// [`Store::sync_apply`] 的结果。
#[derive(Debug)]
pub(crate) enum ApplyOutcome {
    /// 读取之后本机的数据被修改了（校验没有通过），什么都没写
    LocalChanged,
    /// 已写入；`failed` 是没能写入的变更（下标）：本机不认识的设置组、解析不了或不合法的条目内容，
    /// 以及引用了这样的条目（本机又没有它）的条目与秘密
    Applied { failed: BTreeSet<usize> },
}

impl Store {
    /// 读出全部可同步的数据。秘密要解密，保险库未解锁时返回 [`CoreError::VaultLocked`]。
    pub(crate) fn sync_snapshot(&self) -> Result<RawSnapshot> {
        let state = self.read();
        self.db
            .with_connection(|conn| snapshot(&state, conn, &self.vault))
    }

    /// 写入同步拉取到的变更（见模块说明）。`verify` 在写锁下拿到本机当前的数据，返回 false 表示读取之后被修改过。
    pub(crate) fn sync_apply(
        &self,
        verify: impl FnOnce(&RawSnapshot) -> bool,
        changes: &[RawChange],
    ) -> Result<ApplyOutcome> {
        let mut state = self.write();
        let current = self
            .db
            .with_connection(|conn| snapshot(&state, conn, &self.vault))?;
        if !verify(&current) {
            tracing::info!("同步写入前本机数据又变了，这次不写");
            return Ok(ApplyOutcome::LocalChanged);
        }
        drop(current);

        let now = crate::time::now_ms();
        let mut work = state.clone();
        let mut edits = SecretEdits::default();
        let mut setting_times: HashMap<&'static str, i64> = HashMap::new();
        let mut failed = blocked_changes(&state, changes);
        let mut put_forwards = Vec::new();
        for (index, change) in changes.iter().enumerate() {
            if failed.contains(&index) {
                continue;
            }
            let ok = match change {
                RawChange::PutItem(item) => {
                    let ok = by_kind!(item.kind, T => put_item::<T>(&mut work, item));
                    if ok && item.kind == Kind::Forward {
                        put_forwards.push(item.id.clone());
                    }
                    ok
                }
                RawChange::DeleteItem { kind, id } => {
                    by_kind!(*kind, T => remove_item::<T>(&mut work, id, now));
                    true
                }
                RawChange::PutSetting {
                    id,
                    doc,
                    updated_at,
                    keep_local,
                } => match put_setting(&mut work.settings.synced, id, doc, keep_local) {
                    Some(id) => {
                        setting_times.insert(id, *updated_at);
                        true
                    }
                    None => false,
                },
                RawChange::PutSecrets(raw) => {
                    let owner = owner_name(raw.kind, &raw.id);
                    for field in fields_of(&work, &owner) {
                        if !raw.fields.contains_key(&field) {
                            edits.remove(&mut work, &owner, &field);
                        }
                    }
                    for (field, value) in &raw.fields {
                        edits.set(&mut work, &owner, field, value.clone(), raw.updated_at);
                    }
                    true
                }
                RawChange::DeleteSecrets { kind, id } => {
                    edits.remove_owner(&mut work, &owner_name(*kind, id));
                    true
                }
                // 等条目都写好再写
                RawChange::SetHostOs { .. } => true,
                RawChange::Unreadable { .. } => false,
            };
            if !ok {
                failed.insert(index);
            }
        }
        for (index, change) in changes.iter().enumerate() {
            if let RawChange::SetHostOs { id, os } = change
                && !failed.contains(&index)
            {
                set_host_os(&mut work, id, os.clone());
            }
        }
        drop_bind_conflicts(&mut work, &put_forwards, now);
        repair(&mut work, &mut edits, now);

        let kinds: Vec<Kind> = Kind::ALL
            .into_iter()
            .filter(
                |kind| by_kind!(*kind, T => differs(T::collection(&state), T::collection(&work))),
            )
            .collect();
        let setting_rows = changed_settings(
            &state.settings.synced,
            &work.settings.synced,
            &setting_times,
            now,
        )?;
        let secrets_changed = !edits.is_empty();
        self.db.write(|tx| {
            for kind in &kinds {
                by_kind!(*kind, T => write_diff::<T>(tx, T::collection(&state), T::collection(&work), now)?);
            }
            for row in &setting_rows {
                db::put_setting(tx, "setting", row)?;
            }
            for (owner, field) in &edits.delete {
                tx.execute(
                    "DELETE FROM secret WHERE owner = ?1 AND field = ?2",
                    params![owner, field],
                )?;
            }
            for ((owner, field), (value, at)) in &edits.set {
                let blob = self.vault.seal(
                    &secrets::raw_aad(owner, field),
                    value.expose().as_bytes(),
                )?;
                tx.execute(
                    "INSERT INTO secret (owner, field, blob, updated_at) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT (owner, field) DO UPDATE SET blob = excluded.blob, updated_at = excluded.updated_at",
                    params![owner, field, blob, at],
                )?;
            }
            Ok(())
        })?;
        *state = work;
        drop(state);
        tracing::info!(
            changes = changes.len(),
            failed = failed.len(),
            kinds = ?kinds.iter().map(|k| k.name()).collect::<Vec<_>>(),
            settings = setting_rows.len(),
            secrets_set = edits.set.len(),
            secrets_deleted = edits.delete.len(),
            "同步写入本机"
        );
        if !failed.is_empty() {
            tracing::warn!(
                failed = failed.len(),
                "同步来的部分变更没能写入（不认识或解析不了），保持本机原样"
            );
        }
        for kind in kinds {
            self.emit(StoreEvent::Changed(kind));
        }
        if !setting_rows.is_empty() {
            self.emit(StoreEvent::SettingsChanged);
        }
        if secrets_changed {
            self.emit(StoreEvent::SecretsChanged);
        }
        Ok(ApplyOutcome::Applied { failed })
    }

    /// 删除 `keep` 返回 false 的墓碑（数据库中的行与内存中的记录），返回删除了几个。
    pub(crate) fn sync_purge_tombstones(&self, keep: impl Fn(Kind, &str) -> bool) -> Result<usize> {
        let mut state = self.write();
        let mut purge: Vec<(Kind, String)> = Vec::new();
        for kind in Kind::ALL {
            by_kind!(kind, T => {
                for id in T::collection(&state).tombstone_ids() {
                    if !keep(kind, id) {
                        purge.push((kind, id.clone()));
                    }
                }
            });
        }
        if purge.is_empty() {
            return Ok(0);
        }
        self.db.write(|tx| {
            for (kind, id) in &purge {
                tx.execute(
                    &format!(
                        "DELETE FROM {} WHERE id = ?1 AND deleted_at IS NOT NULL",
                        kind.table()
                    ),
                    [id],
                )?;
            }
            Ok(())
        })?;
        for (kind, id) in &purge {
            by_kind!(*kind, T => T::collection_mut(&mut state).forget_tombstone(id));
        }
        Ok(purge.len())
    }
}

// ---------------------------------------------------------------- 读取

fn snapshot(state: &State, conn: &Connection, vault: &Vault) -> Result<RawSnapshot> {
    let mut out = RawSnapshot::default();
    for kind in Kind::ALL {
        by_kind!(kind, T => push_items(T::collection(state), &mut out.items)?);
    }
    let times: HashMap<String, i64> = db::load_settings(conn, "setting")?
        .into_iter()
        .map(|row| (row.id, row.updated_at))
        .collect();
    for (id, doc) in sections_json(&state.settings.synced)? {
        out.settings.push(RawSetting {
            id,
            doc,
            updated_at: times.get(id).copied().unwrap_or(0),
        });
    }

    let mut stmt = conn.prepare("SELECT owner, field, blob, updated_at FROM secret")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Vec<u8>>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;
    let mut owners: BTreeMap<String, RawSecrets> = BTreeMap::new();
    for row in rows {
        let (owner, field, blob, at) = row?;
        let Some((kind, id)) = parse_owner(&owner) else {
            continue;
        };
        // 归属的条目已经不在（不会出现，除非数据库被手工改过）：不同步
        let Some(owner_at) = by_kind!(kind, T => updated_at_of::<T>(state, id)) else {
            continue;
        };
        let plain = vault.open_sealed(&secrets::raw_aad(&owner, &field), &blob)?;
        let text = std::str::from_utf8(&plain).map_err(|_| CoreError::Decrypt)?;
        let entry = owners.entry(owner.clone()).or_insert_with(|| RawSecrets {
            kind,
            id: id.to_string(),
            fields: BTreeMap::new(),
            updated_at: owner_at,
        });
        entry.fields.insert(field, SecretString::new(text));
        entry.updated_at = entry.updated_at.max(at);
    }
    out.secrets = owners.into_values().collect();
    Ok(out)
}

fn push_items<T: Item>(collection: &Collection<T>, out: &mut Vec<RawItem>) -> Result<()> {
    for record in collection.iter() {
        out.push(RawItem {
            kind: T::KIND,
            id: record.id.as_str().to_string(),
            parent: record.parent.clone(),
            sort_key: record.sort_key.clone(),
            created_at: record.created_at,
            updated_at: record.updated_at,
            doc: serde_json::to_value(&record.doc)?,
        });
    }
    Ok(())
}

fn updated_at_of<T: Stored>(state: &State, id: &str) -> Option<i64> {
    T::collection(state)
        .get(&T::Id::from_raw(id.to_string()))
        .map(|r| r.updated_at)
}

fn exists<T: Stored>(state: &State, id: &str) -> bool {
    updated_at_of::<T>(state, id).is_some()
}

/// 同步的设置组（组名与内容）。
fn sections_json(s: &Settings) -> Result<Vec<(&'static str, Value)>> {
    Ok(vec![
        (GeneralSettings::ID, serde_json::to_value(&s.general)?),
        (AppearanceSettings::ID, serde_json::to_value(&s.appearance)?),
        (TerminalSettings::ID, serde_json::to_value(&s.terminal)?),
        (HighlightSettings::ID, serde_json::to_value(&s.highlight)?),
        (ConnectionSettings::ID, serde_json::to_value(&s.connection)?),
        (GuardSettings::ID, serde_json::to_value(&s.guard)?),
        (
            KeybindingSettings::ID,
            serde_json::to_value(&s.keybindings)?,
        ),
    ])
}

/// 秘密的归属：`host/h_…` → (种类, ID)。
fn parse_owner(owner: &str) -> Option<(Kind, &str)> {
    let (kind, id) = owner.split_once('/')?;
    let kind = Kind::ALL.into_iter().find(|k| k.name() == kind)?;
    Some((kind, id))
}

fn owner_name(kind: Kind, id: &str) -> String {
    format!("{}/{id}", kind.name())
}

fn fields_of(state: &State, owner: &str) -> Vec<String> {
    state
        .secrets
        .iter()
        .filter(|(o, _)| o == owner)
        .map(|(_, f)| f.clone())
        .collect()
}

// ---------------------------------------------------------------- 写不进的条目

/// 不能写入的变更（下标）：写不进、本机也没有的条目（内容解析不了、不合法、本机认不出），
/// 以及引用了这样的条目的条目与秘密（逐层传递：被挡下的条目本机也没有时，引用它的同样挡下）。
fn blocked_changes(state: &State, changes: &[RawChange]) -> BTreeSet<usize> {
    let mut blocked = BTreeSet::new();
    // 本机没有、这次也写不进的条目
    let mut missing: HashSet<(Kind, String)> = HashSet::new();
    let mut refs: Vec<(usize, Kind, &String, Vec<(Kind, String)>)> = Vec::new();
    for (index, change) in changes.iter().enumerate() {
        let (kind, id) = match change {
            RawChange::PutItem(item) => match references(item) {
                Some(targets) => {
                    refs.push((index, item.kind, &item.id, targets));
                    continue;
                }
                None => (item.kind, &item.id),
            },
            RawChange::Unreadable { kind, id } => (*kind, id),
            _ => continue,
        };
        blocked.insert(index);
        if !exists_of(state, kind, id) {
            missing.insert((kind, id.clone()));
        }
    }
    // 逐层传递，直到没有新挡下的
    loop {
        let mut grew = false;
        for (index, kind, id, targets) in &refs {
            if !blocked.contains(index) && targets.iter().any(|t| missing.contains(t)) {
                blocked.insert(*index);
                if !exists_of(state, *kind, id) {
                    missing.insert((*kind, (*id).clone()));
                }
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    for (index, change) in changes.iter().enumerate() {
        let owner = match change {
            RawChange::PutSecrets(RawSecrets { kind, id, .. }) => (*kind, id.clone()),
            RawChange::SetHostOs { id, .. } => (Kind::Host, id.clone()),
            _ => continue,
        };
        if missing.contains(&owner) {
            blocked.insert(index);
        }
    }
    blocked
}

fn exists_of(state: &State, kind: Kind, id: &str) -> bool {
    by_kind!(kind, T => exists::<T>(state, id))
}

/// 条目引用的其他条目（写入后修复引用时检查的那些）；内容解析不了、不合法时返回 `None`。
fn references(item: &RawItem) -> Option<Vec<(Kind, String)>> {
    let mut out = Vec::new();
    let library = |source: &KeySource, out: &mut Vec<(Kind, String)>| {
        if let KeySource::Library { key } = source {
            out.push((Kind::Key, key.to_string()));
        }
    };
    match item.kind {
        Kind::Host => {
            let host = parse_item::<Host>(item)?;
            out.extend(item.parent.clone().map(|p| (Kind::Group, p)));
            match &host.auth {
                Auth::Profile { profile } => out.push((Kind::Profile, profile.to_string())),
                Auth::Key { source } => library(source, &mut out),
                _ => {}
            }
            if let Some(credential) = &host.cli.credential
                && let CliAuth::Key { source } = &credential.auth
            {
                library(source, &mut out);
            }
            if let Jump::Via { host } = &host.jump {
                out.push((Kind::Host, host.to_string()));
            }
        }
        Kind::Group => {
            let group = parse_item::<Group>(item)?;
            out.extend(item.parent.clone().map(|p| (Kind::Group, p)));
            out.extend(
                group
                    .defaults
                    .profile
                    .iter()
                    .map(|p| (Kind::Profile, p.to_string())),
            );
            out.extend(
                group
                    .defaults
                    .jump
                    .iter()
                    .map(|h| (Kind::Host, h.to_string())),
            );
        }
        Kind::Profile => {
            if let ProfileAuth::Key { source } = &parse_item::<Profile>(item)?.auth {
                library(source, &mut out);
            }
        }
        Kind::Forward => {
            let forward = parse_item::<Forward>(item)?;
            out.extend(forward.host.iter().map(|h| (Kind::Host, h.to_string())));
        }
        kind => by_kind!(kind, T => parse_item::<T>(item).map(|_| ()))?,
    }
    Some(out)
}

// ---------------------------------------------------------------- 写入内存的副本

/// 解析并校验一条条目的内容；解析不了、不合法时返回 `None`。
fn parse_item<T: Stored>(item: &RawItem) -> Option<T> {
    let mut doc = serde_json::from_value::<T>(item.doc.clone()).ok()?;
    doc.normalize().ok()?;
    Some(doc)
}

fn put_item<T: Stored>(work: &mut State, item: &RawItem) -> bool {
    let Some(mut doc) = parse_item::<T>(item) else {
        return false;
    };
    let id = T::Id::from_raw(item.id.clone());
    // 条目记录中没有识别出的系统等字段（它们单独同步）：保留本机的
    if let Some(current) = T::collection(work).get(&id) {
        doc.keep_observed(&current.doc);
    }
    T::collection_mut(work).insert(Record {
        id,
        parent: item.parent.clone(),
        sort_key: item.sort_key.clone(),
        doc,
        created_at: item.created_at,
        updated_at: item.updated_at,
    });
    true
}

/// 写入主机识别出的系统，修改时间不变；主机不在（两边都删除了、这次删除）时不写。
fn set_host_os(work: &mut State, id: &str, os: Option<String>) {
    let id = crate::id::HostId::from_raw(id.to_string());
    if let Some(record) = work.hosts.get(&id)
        && record.doc.os != os
    {
        let mut record = record.clone();
        record.doc.os = os;
        work.hosts.insert(record);
    }
}

fn remove_item<T: Stored>(work: &mut State, id: &str, now: i64) {
    if exists::<T>(work, id) {
        T::collection_mut(work).remove(id, now);
    }
}

/// 替换一组设置，返回组名；不认识的组、内容解析不了时返回 `None`。
fn put_setting(
    settings: &mut Settings,
    id: &str,
    doc: &Value,
    keep_local: &[&str],
) -> Option<&'static str> {
    let Value::Object(remote) = doc else {
        return None;
    };
    fn merge<S: SettingsSection>(
        target: &mut S,
        remote: &Map<String, Value>,
        keep: &[&str],
    ) -> Option<&'static str> {
        let Ok(Value::Object(local)) = serde_json::to_value(&*target) else {
            return None;
        };
        let mut merged = remote.clone();
        for key in keep {
            match local.get(*key) {
                Some(value) => merged.insert((*key).to_string(), value.clone()),
                None => merged.remove(*key),
            };
        }
        let section = serde_json::from_value::<S>(Value::Object(merged)).ok()?;
        *target = section;
        Some(S::ID)
    }
    if id == GeneralSettings::ID {
        merge(&mut settings.general, remote, keep_local)
    } else if id == AppearanceSettings::ID {
        merge(&mut settings.appearance, remote, keep_local)
    } else if id == TerminalSettings::ID {
        merge(&mut settings.terminal, remote, keep_local)
    } else if id == HighlightSettings::ID {
        merge(&mut settings.highlight, remote, keep_local)
    } else if id == ConnectionSettings::ID {
        merge(&mut settings.connection, remote, keep_local)
    } else if id == GuardSettings::ID {
        merge(&mut settings.guard, remote, keep_local)
    } else if id == KeybindingSettings::ID {
        merge(&mut settings.keybindings, remote, keep_local)
    } else {
        None
    }
}

/// 要写库的秘密：`(归属, 字段)` → 明文与修改时间；要删除的 `(归属, 字段)`。同时维护副本中的索引。
#[derive(Default)]
struct SecretEdits {
    set: BTreeMap<(String, String), (SecretString, i64)>,
    delete: BTreeSet<(String, String)>,
}

impl SecretEdits {
    fn is_empty(&self) -> bool {
        self.set.is_empty() && self.delete.is_empty()
    }

    fn set(&mut self, work: &mut State, owner: &str, field: &str, value: SecretString, at: i64) {
        let key = (owner.to_string(), field.to_string());
        self.delete.remove(&key);
        work.secrets.insert(key.clone());
        self.set.insert(key, (value, at));
    }

    fn remove(&mut self, work: &mut State, owner: &str, field: &str) {
        let key = (owner.to_string(), field.to_string());
        self.set.remove(&key);
        if work.secrets.remove(&key) {
            self.delete.insert(key);
        }
    }

    fn remove_owner(&mut self, work: &mut State, owner: &str) {
        for field in fields_of(work, owner) {
            self.remove(work, owner, &field);
        }
    }
}

/// 写入的端口转发规则与本机另一条规则监听同一个地址（各自新建的）：删除本机的那条。
fn drop_bind_conflicts(work: &mut State, put: &[String], now: i64) {
    for id in put {
        let Some(rule) = work
            .forwards
            .get(&crate::id::ForwardId::from_raw(id.clone()))
            .cloned()
        else {
            continue;
        };
        let others: Vec<String> = work
            .forwards
            .iter()
            .filter(|other| other.id != rule.id && rule.doc.conflicts_with(&other.doc))
            .map(|other| other.id.to_string())
            .collect();
        for other in others {
            work.forwards.remove(&other, now);
        }
    }
}

/// 修复悬空的引用（引用的条目两边都没有了）：能为空的改为空或改回默认，不能为空的（端口转发的主机、秘密的归属）
/// 删除这一条；分组的上级、跳板机成环时从 ID 最小的那条断开。修复过的条目算作本机的修改（之后上传）。
fn repair(work: &mut State, edits: &mut SecretEdits, now: i64) {
    // 删除一条可能带出新的悬空引用，最多重复几轮
    for _ in 0..8 {
        let mut changed = repair_groups(work, now);
        changed |= repair_hosts(work, now);
        changed |= repair_profiles(work, now);
        changed |= repair_forwards(work, now);
        changed |= repair_secrets(work, edits);
        if !changed {
            return;
        }
    }
}

fn repair_groups(work: &mut State, now: i64) -> bool {
    let mut fixed: Vec<Record<Group>> = Vec::new();
    for record in work.groups.iter() {
        let mut next = record.clone();
        if next.parent.as_ref().is_some_and(|p| {
            work.groups
                .get(&crate::id::GroupId::from_raw(p.clone()))
                .is_none()
        }) {
            next.parent = None;
        }
        if next
            .doc
            .defaults
            .profile
            .as_ref()
            .is_some_and(|p| work.profiles.get(p).is_none())
        {
            next.doc.defaults.profile = None;
        }
        if next
            .doc
            .defaults
            .jump
            .as_ref()
            .is_some_and(|h| work.hosts.get(h).is_none())
        {
            next.doc.defaults.jump = None;
        }
        if &next != record {
            fixed.push(next);
        }
    }
    // 上级成环（两边各自把一个分组移进了另一个）：从环中 ID 最小的分组断开
    let parents: HashMap<String, String> = work
        .groups
        .iter()
        .filter_map(|g| Some((g.id.to_string(), g.parent.clone()?)))
        .collect();
    for id in cycle_breakers(&parents) {
        let record = fixed
            .iter()
            .position(|r| r.id.as_str() == id)
            .map(|i| fixed.remove(i))
            .or_else(|| {
                work.groups
                    .get(&crate::id::GroupId::from_raw(id.clone()))
                    .cloned()
            });
        if let Some(mut record) = record {
            record.parent = None;
            fixed.push(record);
        }
    }
    let changed = !fixed.is_empty();
    for mut record in fixed {
        record.updated_at = now;
        work.groups.insert(record);
    }
    changed
}

fn repair_hosts(work: &mut State, now: i64) -> bool {
    let key_missing = |work: &State, source: &KeySource| matches!(source, KeySource::Library { key } if work.keys.get(key).is_none());
    let mut fixed: Vec<Record<Host>> = Vec::new();
    for record in work.hosts.iter() {
        let mut next = record.clone();
        if next.parent.as_ref().is_some_and(|p| {
            work.groups
                .get(&crate::id::GroupId::from_raw(p.clone()))
                .is_none()
        }) {
            next.parent = None;
        }
        let auth_broken = match &next.doc.auth {
            Auth::Profile { profile } => work.profiles.get(profile).is_none(),
            Auth::Key { source } => key_missing(work, source),
            _ => false,
        };
        if auth_broken {
            next.doc.auth = Auth::Password;
        }
        if let Some(credential) = next.doc.cli.credential.as_mut()
            && let CliAuth::Key { source } = &credential.auth
            && key_missing(work, source)
        {
            credential.auth = CliAuth::Password;
        }
        if let Jump::Via { host } = &next.doc.jump
            && !work.hosts.get(host).is_some_and(|h| h.doc.is_ssh())
        {
            next.doc.jump = Jump::Inherit;
        }
        if &next != record {
            fixed.push(next);
        }
    }
    // 跳板机成环：从环中 ID 最小的主机断开
    let jumps: HashMap<String, String> = work
        .hosts
        .iter()
        .filter_map(|h| match &h.doc.jump {
            Jump::Via { host } => Some((h.id.to_string(), host.to_string())),
            _ => None,
        })
        .collect();
    for id in cycle_breakers(&jumps) {
        let record = fixed
            .iter()
            .position(|r| r.id.as_str() == id)
            .map(|i| fixed.remove(i))
            .or_else(|| {
                work.hosts
                    .get(&crate::id::HostId::from_raw(id.clone()))
                    .cloned()
            });
        if let Some(mut record) = record {
            record.doc.jump = Jump::Inherit;
            fixed.push(record);
        }
    }
    let changed = !fixed.is_empty();
    for mut record in fixed {
        record.updated_at = now;
        work.hosts.insert(record);
    }
    changed
}

fn repair_profiles(work: &mut State, now: i64) -> bool {
    let fixed: Vec<Record<Profile>> = work
        .profiles
        .iter()
        .filter(|p| {
            matches!(&p.doc.auth, ProfileAuth::Key { source: KeySource::Library { key } } if work.keys.get(key).is_none())
        })
        .map(|p| {
            let mut next = p.clone();
            next.doc.auth = ProfileAuth::Password;
            next.updated_at = now;
            next
        })
        .collect();
    let changed = !fixed.is_empty();
    for record in fixed {
        work.profiles.insert(record);
    }
    changed
}

fn repair_forwards(work: &mut State, now: i64) -> bool {
    let orphans: Vec<String> = work
        .forwards
        .iter()
        .filter(|f| {
            f.doc
                .host
                .as_ref()
                .is_none_or(|h| work.hosts.get(h).is_none())
        })
        .map(|f| f.id.to_string())
        .collect();
    for id in &orphans {
        work.forwards.remove(id, now);
    }
    !orphans.is_empty()
}

fn repair_secrets(work: &mut State, edits: &mut SecretEdits) -> bool {
    let owners: BTreeSet<String> = work.secrets.iter().map(|(o, _)| o.clone()).collect();
    let mut changed = false;
    for owner in owners {
        let alive = parse_owner(&owner)
            .is_some_and(|(kind, id)| by_kind!(kind, T => exists::<T>(work, id)));
        if !alive {
            edits.remove_owner(work, &owner);
            changed = true;
        }
    }
    changed
}

/// `links` 是"条目 → 它指向的条目"（上级分组、跳板机）。返回需要断开的条目：每个环中 ID 最小的一个。
fn cycle_breakers(links: &HashMap<String, String>) -> Vec<String> {
    let mut breakers = BTreeSet::new();
    let mut ids: Vec<&String> = links.keys().collect();
    ids.sort();
    for start in ids {
        let mut seen: HashSet<&String> = HashSet::new();
        let mut path: Vec<&String> = Vec::new();
        let mut next = Some(start);
        while let Some(id) = next {
            if !seen.insert(id) {
                // 回到了走过的条目：从它开始的那一段是环
                if let Some(pos) = path.iter().position(|p| *p == id)
                    && let Some(min) = path[pos..].iter().min()
                {
                    breakers.insert((*min).clone());
                }
                break;
            }
            path.push(id);
            next = links.get(id);
        }
    }
    breakers.into_iter().collect()
}

// ---------------------------------------------------------------- 写库

fn differs<T: Item>(old: &Collection<T>, new: &Collection<T>) -> bool {
    old.len() != new.len()
        || new.iter().any(|r| old.get(&r.id) != Some(r))
        || old.iter().any(|r| new.get(&r.id).is_none())
}

fn write_diff<T: Item>(
    tx: &Connection,
    old: &Collection<T>,
    new: &Collection<T>,
    now: i64,
) -> Result<()> {
    for record in new.iter() {
        if old.get(&record.id) != Some(record) {
            collection::put(tx, record)?;
        }
    }
    for record in old.iter() {
        if new.get(&record.id).is_none() {
            let at = new.deleted_at(record.id.as_str()).unwrap_or(now);
            collection::put_tombstone(tx, T::KIND, record.id.as_str(), at)?;
        }
    }
    Ok(())
}

/// 内容有变化的设置组的行；同步写入的组用云端的修改时间，其余（不会出现）用现在。
fn changed_settings(
    old: &Settings,
    new: &Settings,
    times: &HashMap<&'static str, i64>,
    now: i64,
) -> Result<Vec<SettingRow>> {
    let old = sections_json(old)?;
    let new = sections_json(new)?;
    let mut rows = Vec::new();
    for ((id, before), (_, after)) in old.iter().zip(new.iter()) {
        if before != after {
            rows.push(SettingRow {
                id: (*id).to_string(),
                doc: serde_json::to_string(after)?,
                updated_at: times.get(id).copied().unwrap_or(now),
            });
        }
    }
    Ok(rows)
}
