//! 存储：全部条目、设置与秘密的读写。
//!
//! - 启动时把条目全部读进内存，之后读取都在内存中进行；修改时**先写库（事务）、成功后再改内存**，然后发出变更事件。
//!   写库失败时操作返回错误、内存不变，界面不会显示一个重启后就消失的修改。
//! - 删除留下墓碑（同步需要知道哪些条目被删了），内存中的列表不含墓碑。
//! - 条目之间的引用（凭据档案、密钥库中的密钥、跳板机、端口转发的主机）在保存时检查，端口转发规则还检查
//!   监听地址是否与已有规则冲突；仍被引用的凭据档案、密钥不能删除；删除主机时一并删除它的端口转发规则与秘密。
//! - 秘密经保险库加密后存在 `secret` 表中；内存中只有"哪些条目有哪些秘密"的索引，取用时才解密。

mod collection;
mod secrets;
mod settings_store;
pub(crate) mod sync;

use std::collections::HashSet;
use std::sync::{Mutex, RwLock};

use tokio::sync::broadcast;

pub use collection::{Collection, Position, Record};
pub use secrets::Owner;
pub(crate) use secrets::raw_aad as secret_aad;

use crate::db::Database;
use crate::id::{GroupId, HostId, KeyId, ProfileId};
use crate::model::{
    Auth, CliAuth, ColorScheme, Forward, Group, Host, Item, ItemId, Jump, KeyEntry, KeySource,
    Kind, LocalSettings, MAX_JUMP_DEPTH, Profile, ProfileAuth, RemoteOs, Settings, Snippet,
    Workspace,
};
use crate::vault::{Remember, Vault, VaultStatus};
use crate::{CoreError, Invalid, Result};

/// 存储发出的变更事件。界面收到后重新读取对应的数据。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreEvent {
    /// 某种条目有新增、修改或删除
    Changed(Kind),
    SettingsChanged,
    LocalSettingsChanged,
    /// 秘密有变化（保存、删除、保险库重置）
    SecretsChanged,
    /// 保险库的状态变了（解锁、重置）
    VaultChanged,
}

/// 内存中的全部数据。
#[derive(Clone, Default)]
pub struct State {
    hosts: Collection<Host>,
    groups: Collection<Group>,
    profiles: Collection<Profile>,
    keys: Collection<KeyEntry>,
    snippets: Collection<Snippet>,
    forwards: Collection<Forward>,
    workspaces: Collection<Workspace>,
    schemes: Collection<ColorScheme>,
    settings: settings_store::SettingsState,
    /// 有哪些秘密：`(归属, 字段)`
    secrets: HashSet<(String, String)>,
}

/// 能存进 [`Store`] 的条目：知道自己在 [`State`] 中的位置。
pub trait Stored: Item {
    fn collection(state: &State) -> &Collection<Self>;
    fn collection_mut(state: &mut State) -> &mut Collection<Self>;
}

macro_rules! stored {
    ($($ty:ty => $field:ident),* $(,)?) => {$(
        impl Stored for $ty {
            fn collection(state: &State) -> &Collection<Self> {
                &state.$field
            }
            fn collection_mut(state: &mut State) -> &mut Collection<Self> {
                &mut state.$field
            }
        }
    )*};
}

stored! {
    Host => hosts,
    Group => groups,
    Profile => profiles,
    KeyEntry => keys,
    Snippet => snippets,
    Forward => forwards,
    Workspace => workspaces,
    ColorScheme => schemes,
}

pub struct Store {
    db: Database,
    vault: Vault,
    state: RwLock<State>,
    events: broadcast::Sender<StoreEvent>,
    /// 批量修改期间推迟发出的事件（去重），见 [`Store::batch`]
    deferred: Mutex<Option<Vec<StoreEvent>>>,
}

/// 从数据库读出全部数据。
fn load_state(db: &Database) -> Result<State> {
    db.read(|conn| {
        Ok(State {
            hosts: Collection::load(conn)?,
            groups: Collection::load(conn)?,
            profiles: Collection::load(conn)?,
            keys: Collection::load(conn)?,
            snippets: Collection::load(conn)?,
            forwards: Collection::load(conn)?,
            workspaces: Collection::load(conn)?,
            schemes: Collection::load(conn)?,
            settings: settings_store::SettingsState::load(conn)?,
            secrets: secrets::load_index(conn)?,
        })
    })
}

impl Store {
    /// 读出数据库中的全部数据。
    pub fn open(db: Database, vault: Vault) -> Result<Self> {
        let state = load_state(&db)?;
        let (events, _) = broadcast::channel(256);
        Ok(Self {
            db,
            vault,
            state: RwLock::new(state),
            events,
            deferred: Mutex::new(None),
        })
    }

    /// 订阅变更事件。
    pub fn subscribe(&self) -> broadcast::Receiver<StoreEvent> {
        self.events.subscribe()
    }

    fn emit(&self, event: StoreEvent) {
        {
            let mut deferred = self.deferred.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(pending) = deferred.as_mut() {
                if !pending.contains(&event) {
                    pending.push(event);
                }
                return;
            }
        }
        // 没有订阅者时发送失败，忽略
        let _ = self.events.send(event);
    }

    /// 在 `f` 执行期间推迟变更事件，结束后（含 panic 时）每种事件只发一次：导入大量主机时界面只重建一次，
    /// 不会因为事件积压被丢弃而漏掉。可以嵌套，最外层结束时才发出。期间其他线程的修改的事件同样推迟。
    pub(crate) fn batch<R>(&self, f: impl FnOnce() -> R) -> R {
        struct Flush<'a> {
            store: &'a Store,
            outer: bool,
        }
        impl Drop for Flush<'_> {
            fn drop(&mut self) {
                if !self.outer {
                    return;
                }
                let pending = self
                    .store
                    .deferred
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                for event in pending.into_iter().flatten() {
                    let _ = self.store.events.send(event);
                }
            }
        }
        let outer = {
            let mut deferred = self.deferred.lock().unwrap_or_else(|e| e.into_inner());
            deferred.is_none() && {
                *deferred = Some(Vec::new());
                true
            }
        };
        let _flush = Flush { store: self, outer };
        f()
    }

    /// 用 `replace` 整体替换数据库的内容后重新读入内存，并发出全部种类的变更事件（恢复备份、快照时用）。
    /// 替换期间持有写锁，其他读写等待，不会读到一半新一半旧的数据；`replace` 失败时内存不变。
    pub(crate) fn replace_all(&self, replace: impl FnOnce(&Database) -> Result<()>) -> Result<()> {
        let mut state = self.write();
        replace(&self.db)?;
        *state = load_state(&self.db)?;
        drop(state);
        for kind in Kind::ALL {
            self.emit(StoreEvent::Changed(kind));
        }
        self.emit(StoreEvent::SettingsChanged);
        self.emit(StoreEvent::LocalSettingsChanged);
        self.emit(StoreEvent::SecretsChanged);
        Ok(())
    }

    pub fn vault_status(&self) -> VaultStatus {
        self.vault.status()
    }

    pub fn vault(&self) -> &Vault {
        &self.vault
    }

    pub fn database(&self) -> &Database {
        &self.db
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(|e| e.into_inner())
    }

    // ---------------------------------------------------------------- 条目

    /// 一种条目的全部（不含墓碑），按上级与排序键排列。
    pub fn list<T: Stored>(&self) -> Vec<Record<T>> {
        T::collection(&self.read()).sorted()
    }

    pub fn get<T: Stored>(&self, id: &T::Id) -> Option<Record<T>> {
        T::collection(&self.read()).get(id).cloned()
    }

    /// 新建条目，返回它的 ID。`parent` 是所属分组等上级，`position` 是在同级中的位置。
    pub fn create<T: Stored>(
        &self,
        mut doc: T,
        parent: Option<String>,
        position: Position<T::Id>,
    ) -> Result<T::Id> {
        doc.normalize()?;
        let mut state = self.write();
        check_references(&state, &doc, None, parent.as_deref())?;
        let record = T::collection(&state).new_record(doc, parent, &position);
        let id = record.id.clone();
        self.db
            .write(|tx| collection::put(tx, &record))
            .inspect_err(|e| tracing::error!(kind = T::KIND.name(), "保存新条目失败：{e}"))?;
        tracing::debug!(kind = T::KIND.name(), id = %id.as_str(), parent = ?record.parent, "新建条目");
        T::collection_mut(&mut state).insert(record);
        drop(state);
        self.emit(StoreEvent::Changed(T::KIND));
        Ok(id)
    }

    /// 修改条目的内容（上级与位置不变）。
    pub fn update<T: Stored>(&self, id: &T::Id, mut doc: T) -> Result<()> {
        doc.normalize()?;
        let mut state = self.write();
        let mut record = T::collection(&state)
            .get(id)
            .cloned()
            .ok_or(CoreError::NotFound(T::KIND.name()))?;
        check_references(&state, &doc, Some(id.as_str()), record.parent.as_deref())?;
        doc.keep_observed(&record.doc);
        if record.doc == doc {
            return Ok(());
        }
        record.doc = doc;
        record.updated_at = crate::time::now_ms();
        self.db
            .write(|tx| collection::put(tx, &record))
            .inspect_err(
                |e| tracing::error!(kind = T::KIND.name(), id = %id.as_str(), "保存条目失败：{e}"),
            )?;
        tracing::debug!(kind = T::KIND.name(), id = %id.as_str(), "修改条目");
        T::collection_mut(&mut state).insert(record);
        drop(state);
        self.emit(StoreEvent::Changed(T::KIND));
        Ok(())
    }

    /// 记下 SSH 连接后识别出的系统（见 [`crate::osprobe`]），返回是否有变化（与原来相同时不写）。
    ///
    /// 不算对主机的修改：不改修改时间，同步时是单独的一条记录（见 `cloudsync` 的 `detected/…`），不会与其他设备上
    /// 对这台主机的修改冲突。主机已删除时返回 [`CoreError::NotFound`]。
    pub fn set_host_os(&self, id: &HostId, os: Option<RemoteOs>) -> Result<bool> {
        let key = os.map(|os| os.key().to_string());
        let mut state = self.write();
        let mut record = state
            .hosts
            .get(id)
            .cloned()
            .ok_or(CoreError::NotFound(Kind::Host.name()))?;
        if record.doc.os == key {
            return Ok(false);
        }
        record.doc.os = key;
        self.db
            .write(|tx| collection::put(tx, &record))
            .inspect_err(|e| tracing::error!(id = %id.as_str(), "保存识别出的系统失败：{e}"))?;
        tracing::debug!(id = %id.as_str(), os = ?record.doc.os, "记下识别出的系统");
        state.hosts.insert(record);
        drop(state);
        self.emit(StoreEvent::Changed(Kind::Host));
        Ok(true)
    }

    /// 把条目移到 `parent` 下的 `position`（拖动排序、移进别的分组）。
    pub fn move_to<T: Stored>(
        &self,
        id: &T::Id,
        parent: Option<String>,
        position: Position<T::Id>,
    ) -> Result<()> {
        let mut state = self.write();
        let mut record = T::collection(&state)
            .get(id)
            .cloned()
            .ok_or(CoreError::NotFound(T::KIND.name()))?;
        check_parent(&state, T::KIND, Some(id.as_str()), parent.as_deref())?;
        record.sort_key = T::collection(&state).key_for(parent.as_deref(), &position, Some(id));
        record.parent = parent;
        record.updated_at = crate::time::now_ms();
        self.db
            .write(|tx| collection::put(tx, &record))
            .inspect_err(
                |e| tracing::error!(kind = T::KIND.name(), id = %id.as_str(), "移动条目失败：{e}"),
            )?;
        tracing::debug!(kind = T::KIND.name(), id = %id.as_str(), parent = ?record.parent, "移动条目");
        T::collection_mut(&mut state).insert(record);
        drop(state);
        self.emit(StoreEvent::Changed(T::KIND));
        Ok(())
    }

    /// 删除条目。主机连同它的端口转发规则、秘密与命令历史，经它连接的主机、分组改为不经跳板机；
    /// 分组连同子分组（其中的主机移出分组）；仍被引用的凭据档案、密钥不能删除。
    pub fn delete<T: Stored>(&self, id: &T::Id) -> Result<()> {
        let mut state = self.write();
        if T::collection(&state).get(id).is_none() {
            return Err(CoreError::NotFound(T::KIND.name()));
        }
        let plan = DeletePlan::build(&state, T::KIND, id.as_str())?;
        let now = crate::time::now_ms();
        self.db
            .write(|tx| plan.apply_db(tx, &state, now))
            .inspect_err(
                |e| tracing::error!(kind = T::KIND.name(), id = %id.as_str(), "删除条目失败：{e}"),
            )?;
        let kinds = plan.apply_memory(&mut state, now);
        tracing::info!(
            kind = T::KIND.name(),
            id = %id.as_str(),
            affected = ?kinds.iter().map(|k| k.name()).collect::<Vec<_>>(),
            secrets = plan.secret_owners.len(),
            "删除条目"
        );
        drop(state);
        for kind in kinds {
            self.emit(StoreEvent::Changed(kind));
        }
        if !plan.secret_owners.is_empty() {
            self.emit(StoreEvent::SecretsChanged);
        }
        Ok(())
    }

    /// 复制一台主机（服务器列表的“复制”）：内容与登录用的秘密照抄，名称为 `name`，放在原来那台的后面（同一分组）。
    ///
    /// 不复制的：收藏（副本是新的一项）、命令行工具的访问授权与它单独的凭据（副本改了地址后不应自动允许 AI 工具
    /// 连接）、端口转发规则（两台同时连上时本机端口会冲突）、命令历史。秘密先全部读出（保险库锁着时不留下半份
    /// 副本），写入失败时删掉刚建的副本。
    pub fn duplicate_host(&self, id: &HostId, name: String) -> Result<HostId> {
        use crate::model::SecretField;
        const COPIED: [SecretField; 5] = [
            SecretField::Password,
            SecretField::PrivateKey,
            SecretField::Passphrase,
            SecretField::Totp,
            SecretField::ProxyPassword,
        ];
        let record = self
            .get::<Host>(id)
            .ok_or(CoreError::NotFound(Kind::Host.name()))?;
        let owner = Owner::host(id);
        let mut secrets = Vec::new();
        for field in COPIED {
            if self.has_secret(&owner, field)
                && let Some(value) = self.secret(&owner, field)?
            {
                secrets.push((field, value));
            }
        }
        let mut host = record.doc;
        host.name = name;
        host.favorite = false;
        host.cli = Default::default();
        let copy = self.create(host, record.parent, Position::After(id.clone()))?;
        let copy_owner = Owner::host(&copy);
        for (field, value) in &secrets {
            if let Err(e) = self.set_secret(&copy_owner, *field, Some(value)) {
                let _ = self.delete::<Host>(&copy);
                return Err(e);
            }
        }
        tracing::info!(from = %id.as_str(), to = %copy.as_str(), secrets = secrets.len(), "复制主机");
        Ok(copy)
    }

    // ---------------------------------------------------------------- 秘密

    /// 是否保存了这项秘密（不需要解锁保险库）。
    pub fn has_secret(&self, owner: &Owner, field: crate::model::SecretField) -> bool {
        self.read()
            .secrets
            .contains(&(owner.to_string(), field.name().to_string()))
    }

    /// 取出一项秘密（解密）。
    pub fn secret(
        &self,
        owner: &Owner,
        field: crate::model::SecretField,
    ) -> Result<Option<crate::secret::SecretString>> {
        secrets::read(&self.db, &self.vault, owner, field)
    }

    /// 保存（`Some`）或删除（`None`）一项秘密。
    pub fn set_secret(
        &self,
        owner: &Owner,
        field: crate::model::SecretField,
        value: Option<&crate::secret::SecretString>,
    ) -> Result<()> {
        let mut state = self.write();
        if !owner.exists(&state) {
            return Err(CoreError::NotFound(owner.kind().name()));
        }
        let key = (owner.to_string(), field.name().to_string());
        // 只记是哪一项秘密，不记内容
        tracing::debug!(owner = %owner, field = field.name(), saved = value.is_some(), "保存秘密");
        match value {
            Some(value) => {
                let blob = secrets::seal(&self.vault, owner, field, value)?;
                self.db
                    .write(|tx| Ok(secrets::put(tx, owner, field, &blob)?))?;
                state.secrets.insert(key);
            }
            None => {
                self.db.write(|tx| Ok(secrets::delete(tx, owner, field)?))?;
                state.secrets.remove(&key);
            }
        }
        drop(state);
        self.emit(StoreEvent::SecretsChanged);
        Ok(())
    }

    // ---------------------------------------------------------------- 保险库

    /// 主密码模式下解锁。
    pub fn unlock_vault(&self, password: &crate::secret::SecretString) -> Result<()> {
        self.vault
            .unlock(&self.db, password)
            .inspect_err(|e| tracing::info!("用主密码解锁保险库失败：{e}"))?;
        tracing::info!("保险库已用主密码解锁");
        self.emit(StoreEvent::VaultChanged);
        Ok(())
    }

    /// 钥匙串不可用、第一次使用时设置主密码。
    pub fn set_vault_password(&self, password: &crate::secret::SecretString) -> Result<()> {
        self.vault.set_new_password(&self.db, password)?;
        tracing::info!("已设置保险库的主密码");
        self.emit(StoreEvent::VaultChanged);
        Ok(())
    }

    /// 便携版第一次使用时选择把主密钥放进这台电脑的钥匙串（钥匙串不可用时转为设置主密码）。
    pub fn choose_vault_keyring(&self) -> Result<()> {
        let status = self.vault.choose_keyring(&self.db)?;
        tracing::info!(?status, "便携版的保险库选择用这台电脑的钥匙串");
        self.emit(StoreEvent::VaultChanged);
        Ok(())
    }

    /// 主密钥是否由主密码保护（系统钥匙串不可用时）；读不出时当作不是。
    pub fn vault_uses_password(&self) -> bool {
        Vault::uses_password(&self.db).unwrap_or(false)
    }

    /// 便携版能不能、有没有在这台电脑上记住主密钥（主密码模式下）。
    pub fn vault_remember(&self) -> Remember {
        self.vault.remember_state()
    }

    /// 在这台电脑上记住主密钥（便携版主密码模式下解锁之后）：以后在这台电脑上启动时直接解锁。
    pub fn remember_vault_key(&self) -> Result<()> {
        self.vault
            .remember()
            .inspect_err(|e| tracing::warn!("没能在这台电脑上记住主密钥：{e}"))?;
        tracing::info!("已在这台电脑上记住主密钥");
        self.emit(StoreEvent::VaultChanged);
        Ok(())
    }

    /// 忘记这台电脑记住的主密钥：下次启动时重新询问主密码。
    pub fn forget_vault_key(&self) -> Result<()> {
        self.vault
            .forget()
            .inspect_err(|e| tracing::warn!("忘记这台电脑记住的主密钥失败：{e}"))?;
        tracing::info!("已忘记这台电脑记住的主密钥");
        self.emit(StoreEvent::VaultChanged);
        Ok(())
    }

    /// 主密码模式下修改主密码：主密钥不变，保存的秘密不用重新加密。只影响这台电脑：云同步、备份与快照
    /// 都不带主密码（同步文件用同步密码加密，恢复备份时保留本机的保险库）。
    pub fn change_vault_password(
        &self,
        old: &crate::secret::SecretString,
        new: &crate::secret::SecretString,
    ) -> Result<()> {
        self.vault
            .change_password(&self.db, old, new)
            .inspect_err(|e| tracing::info!("修改主密码失败：{e}"))?;
        tracing::info!("已修改保险库的主密码");
        Ok(())
    }

    /// 重置保险库：保留主机等数据，清空全部秘密（它们已无法解密）。
    pub fn reset_vault(&self, password: Option<&crate::secret::SecretString>) -> Result<()> {
        tracing::warn!(
            with_password = password.is_some(),
            "重置保险库：清空全部秘密与命令历史"
        );
        let mut state = self.write();
        self.vault.reset(&self.db, password, |tx| {
            tx.execute("DELETE FROM secret", [])?;
            // 命令历史也是用旧的主密钥加密的
            tx.execute("DELETE FROM command_history", [])?;
            Ok(())
        })?;
        state.secrets.clear();
        drop(state);
        self.emit(StoreEvent::VaultChanged);
        self.emit(StoreEvent::SecretsChanged);
        Ok(())
    }

    // ---------------------------------------------------------------- 设置

    pub fn settings(&self) -> Settings {
        self.read().settings.synced.clone()
    }

    pub fn local_settings(&self) -> LocalSettings {
        self.read().settings.local.clone()
    }

    /// 修改设置：只写入有变化的组。
    pub fn update_settings(&self, edit: impl FnOnce(&mut Settings)) -> Result<()> {
        let mut state = self.write();
        let mut next = state.settings.synced.clone();
        edit(&mut next);
        let rows = settings_store::changed_rows(&state.settings.synced, &next)?;
        if rows.is_empty() {
            return Ok(());
        }
        tracing::info!(groups = ?rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), "保存设置");
        self.db.write(|tx| {
            for row in &rows {
                crate::db::put_setting(tx, "setting", row)?;
            }
            Ok(())
        })?;
        state.settings.synced = next;
        drop(state);
        self.emit(StoreEvent::SettingsChanged);
        Ok(())
    }

    pub fn update_local_settings(&self, edit: impl FnOnce(&mut LocalSettings)) -> Result<()> {
        let mut state = self.write();
        let mut next = state.settings.local.clone();
        edit(&mut next);
        let rows = settings_store::changed_local_rows(&state.settings.local, &next)?;
        if rows.is_empty() {
            return Ok(());
        }
        tracing::info!(groups = ?rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), "保存本机设置");
        self.db.write(|tx| {
            for row in &rows {
                crate::db::put_setting(tx, "local_setting", row)?;
            }
            Ok(())
        })?;
        state.settings.local = next;
        drop(state);
        self.emit(StoreEvent::LocalSettingsChanged);
        Ok(())
    }

    // ---------------------------------------------------------------- 分组默认值

    /// 从 `group` 开始逐级向上的分组（自己在最前），遇到不存在的分组或循环时停止。
    pub fn group_chain(&self, group: Option<&GroupId>) -> Vec<Record<Group>> {
        let state = self.read();
        let mut chain = Vec::new();
        let mut next = group.cloned();
        while let Some(id) = next {
            if chain.iter().any(|g: &Record<Group>| g.id == id) {
                break;
            }
            let Some(record) = state.groups.get(&id) else {
                break;
            };
            next = record.parent.clone().map(GroupId::from_raw);
            chain.push(record.clone());
        }
        chain
    }
}

/// 保存前检查条目引用的其他条目。
fn check_references<T: Stored>(
    state: &State,
    doc: &T,
    self_id: Option<&str>,
    parent: Option<&str>,
) -> Result<(), Invalid> {
    check_parent(state, T::KIND, self_id, parent)?;
    let any: &dyn std::any::Any = doc;
    if let Some(host) = any.downcast_ref::<Host>() {
        check_host(state, host, self_id)?;
    } else if let Some(profile) = any.downcast_ref::<Profile>() {
        if let ProfileAuth::Key { source } = &profile.auth {
            check_key_source(state, source, "auth.source")?;
        }
    } else if let Some(group) = any.downcast_ref::<Group>() {
        if let Some(profile) = &group.defaults.profile
            && state.profiles.get(profile).is_none()
        {
            return Err(Invalid::new("defaults.profile", "not_found"));
        }
        if let Some(jump) = &group.defaults.jump {
            match state.hosts.get(jump) {
                None => return Err(Invalid::new("defaults.jump", "not_found")),
                Some(record) if !record.doc.is_ssh() => {
                    return Err(Invalid::new("defaults.jump", "not_ssh"));
                }
                Some(_) => {}
            }
        }
    } else if let Some(forward) = any.downcast_ref::<Forward>() {
        let host = forward
            .host
            .as_ref()
            .and_then(|id| state.hosts.get(id))
            .ok_or(Invalid::new("host", "not_found"))?;
        if !host.doc.is_ssh() {
            return Err(Invalid::new("host", "not_ssh"));
        }
        // 同一个监听地址只能有一条规则（0.0.0.0 与 IPv4 地址同端口也冲突）；界面要说明被哪条规则占用时
        // 用 `Forward::conflicts_with` 自己查找
        if state
            .forwards
            .iter()
            .any(|other| Some(other.id.as_str()) != self_id && forward.conflicts_with(&other.doc))
        {
            return Err(Invalid::new("bindPort", "conflict"));
        }
    }
    Ok(())
}

fn check_parent(
    state: &State,
    kind: Kind,
    self_id: Option<&str>,
    parent: Option<&str>,
) -> Result<(), Invalid> {
    let Some(parent) = parent else {
        return Ok(());
    };
    match kind {
        Kind::Host | Kind::Group => {
            let mut next = Some(GroupId::from_raw(parent));
            let mut depth = 0;
            while let Some(id) = next {
                if Some(id.as_str()) == self_id && kind == Kind::Group {
                    return Err(Invalid::new("parent", "cycle"));
                }
                let record = state
                    .groups
                    .get(&id)
                    .ok_or(Invalid::new("parent", "not_found"))?;
                next = record.parent.clone().map(GroupId::from_raw);
                depth += 1;
                if depth > 64 {
                    return Err(Invalid::new("parent", "cycle"));
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn check_host(state: &State, host: &Host, self_id: Option<&str>) -> Result<(), Invalid> {
    match &host.auth {
        Auth::Profile { profile } if state.profiles.get(profile).is_none() => {
            return Err(Invalid::new("auth.profile", "not_found"));
        }
        Auth::Key { source } => check_key_source(state, source, "auth.source")?,
        _ => {}
    }
    if let Some(credential) = &host.cli.credential
        && let CliAuth::Key { source } = &credential.auth
    {
        check_key_source(state, source, "cli.auth.source")?;
    }
    if let Jump::Via { host: jump } = &host.jump {
        // 跳板链不能回到自己，也不能超过最大层数
        let mut next = Some(jump.clone());
        let mut depth = 0;
        while let Some(id) = next {
            if Some(id.as_str()) == self_id {
                return Err(Invalid::new("jump", "cycle"));
            }
            let record = state
                .hosts
                .get(&id)
                .ok_or(Invalid::new("jump", "not_found"))?;
            // 跳板机只能是 SSH 主机（Telnet、串口主机不能作为跳板机）
            if !record.doc.is_ssh() {
                return Err(Invalid::new("jump", "not_ssh"));
            }
            depth += 1;
            if depth > MAX_JUMP_DEPTH {
                return Err(Invalid::new("jump", "too_deep"));
            }
            next = match &record.doc.jump {
                Jump::Via { host } => Some(host.clone()),
                _ => None,
            };
        }
    }
    Ok(())
}

fn check_key_source(state: &State, source: &KeySource, field: &'static str) -> Result<(), Invalid> {
    match source {
        KeySource::Library { key } if state.keys.get(key).is_none() => {
            Err(Invalid::new(field, "not_found"))
        }
        KeySource::File { path, .. } if path.trim().is_empty() => {
            Err(Invalid::new(field, "required"))
        }
        _ => Ok(()),
    }
}

/// 一次删除要动到的全部条目与秘密。
#[derive(Default)]
struct DeletePlan {
    /// 留下墓碑的条目
    removed: Vec<(Kind, String)>,
    /// 移出分组的主机
    ungrouped: Vec<HostId>,
    /// 经删除的主机连接、改为沿用分组的主机
    unjumped_hosts: Vec<HostId>,
    /// 默认跳板机是删除的主机、改为不经跳板机的分组
    unjumped_groups: Vec<GroupId>,
    /// 要删除秘密的归属
    secret_owners: Vec<String>,
    /// 要删除命令历史的主机
    history_owners: Vec<String>,
}

impl DeletePlan {
    fn build(state: &State, kind: Kind, id: &str) -> Result<Self> {
        let mut plan = DeletePlan::default();
        match kind {
            Kind::Host => {
                plan.remove_host(state, &HostId::from_raw(id));
            }
            Kind::Group => {
                let mut stack = vec![id.to_string()];
                while let Some(group) = stack.pop() {
                    for child in state.groups.iter() {
                        if child.parent.as_deref() == Some(group.as_str()) {
                            stack.push(child.id.to_string());
                        }
                    }
                    for host in state.hosts.iter() {
                        if host.parent.as_deref() == Some(group.as_str()) {
                            plan.ungrouped.push(host.id.clone());
                        }
                    }
                    plan.removed.push((Kind::Group, group));
                }
            }
            Kind::Profile => {
                let profile = ProfileId::from_raw(id);
                if profile_in_use(state, &profile) {
                    return Err(Invalid::new("profile", "in_use").into());
                }
                plan.removed.push((Kind::Profile, id.to_string()));
                plan.secret_owners
                    .push(Owner::new(Kind::Profile, id).to_string());
            }
            Kind::Key => {
                let key = KeyId::from_raw(id);
                if key_in_use(state, &key) {
                    return Err(Invalid::new("key", "in_use").into());
                }
                plan.removed.push((Kind::Key, id.to_string()));
                plan.secret_owners
                    .push(Owner::new(Kind::Key, id).to_string());
            }
            other => plan.removed.push((other, id.to_string())),
        }
        Ok(plan)
    }

    fn remove_host(&mut self, state: &State, host: &HostId) {
        self.removed.push((Kind::Host, host.to_string()));
        self.secret_owners
            .push(Owner::new(Kind::Host, host.as_str()).to_string());
        self.history_owners.push(host.to_string());
        for other in state.hosts.iter() {
            if matches!(&other.doc.jump, Jump::Via { host: via } if via == host)
                && other.id != *host
            {
                self.unjumped_hosts.push(other.id.clone());
            }
        }
        for group in state.groups.iter() {
            if group.doc.defaults.jump.as_ref() == Some(host) {
                self.unjumped_groups.push(group.id.clone());
            }
        }
        for forward in state.forwards.iter() {
            if forward.doc.host.as_ref() == Some(host) {
                self.removed.push((Kind::Forward, forward.id.to_string()));
            }
        }
    }

    fn apply_db(&self, tx: &rusqlite::Transaction, state: &State, now: i64) -> Result<()> {
        for (kind, id) in &self.removed {
            collection::put_tombstone(tx, *kind, id, now)?;
        }
        for host in &self.ungrouped {
            if let Some(record) = state.hosts.get(host) {
                let mut record = record.clone();
                record.parent = None;
                record.updated_at = now;
                collection::put(tx, &record)?;
            }
        }
        for record in self.unjumped_host_records(state, now) {
            collection::put(tx, &record)?;
        }
        for record in self.unjumped_group_records(state, now) {
            collection::put(tx, &record)?;
        }
        for owner in &self.secret_owners {
            secrets::delete_owner(tx, owner)?;
        }
        for owner in &self.history_owners {
            tx.execute("DELETE FROM command_history WHERE owner = ?1", [owner])?;
        }
        Ok(())
    }

    /// 改为不经删除的主机连接之后的主机记录。
    fn unjumped_host_records(&self, state: &State, now: i64) -> Vec<Record<Host>> {
        self.unjumped_hosts
            .iter()
            .filter_map(|id| state.hosts.get(id).cloned())
            .map(|mut record| {
                record.doc.jump = Jump::Inherit;
                record.updated_at = now;
                record
            })
            .collect()
    }

    /// 去掉默认跳板机之后的分组记录。
    fn unjumped_group_records(&self, state: &State, now: i64) -> Vec<Record<Group>> {
        self.unjumped_groups
            .iter()
            .filter_map(|id| state.groups.get(id).cloned())
            .map(|mut record| {
                record.doc.defaults.jump = None;
                record.updated_at = now;
                record
            })
            .collect()
    }

    fn apply_memory(&self, state: &mut State, now: i64) -> Vec<Kind> {
        let mut kinds = Vec::new();
        for (kind, id) in &self.removed {
            match kind {
                Kind::Host => state.hosts.remove(id, now),
                Kind::Group => state.groups.remove(id, now),
                Kind::Profile => state.profiles.remove(id, now),
                Kind::Key => state.keys.remove(id, now),
                Kind::Snippet => state.snippets.remove(id, now),
                Kind::Forward => state.forwards.remove(id, now),
                Kind::Workspace => state.workspaces.remove(id, now),
                Kind::Scheme => state.schemes.remove(id, now),
            }
            if !kinds.contains(kind) {
                kinds.push(*kind);
            }
        }
        for host in &self.ungrouped {
            if let Some(record) = state.hosts.get(host).cloned() {
                let mut record = record;
                record.parent = None;
                record.updated_at = now;
                state.hosts.insert(record);
            }
        }
        for record in self.unjumped_host_records(state, now) {
            state.hosts.insert(record);
        }
        for record in self.unjumped_group_records(state, now) {
            state.groups.insert(record);
        }
        if !(self.ungrouped.is_empty() && self.unjumped_hosts.is_empty())
            && !kinds.contains(&Kind::Host)
        {
            kinds.push(Kind::Host);
        }
        if !self.unjumped_groups.is_empty() && !kinds.contains(&Kind::Group) {
            kinds.push(Kind::Group);
        }
        for owner in &self.secret_owners {
            let prefix = owner.as_str();
            state.secrets.retain(|(o, _)| o != prefix);
        }
        kinds
    }
}

fn profile_in_use(state: &State, profile: &ProfileId) -> bool {
    state
        .hosts
        .iter()
        .any(|h| matches!(&h.doc.auth, Auth::Profile { profile: p } if p == profile))
        || state
            .groups
            .iter()
            .any(|g| g.doc.defaults.profile.as_ref() == Some(profile))
}

fn key_in_use(state: &State, key: &KeyId) -> bool {
    let uses = |source: &KeySource| matches!(source, KeySource::Library { key: k } if k == key);
    state.hosts.iter().any(|h| {
        matches!(&h.doc.auth, Auth::Key { source } if uses(source))
            || h.doc
                .cli
                .credential
                .as_ref()
                .is_some_and(|c| matches!(&c.auth, CliAuth::Key { source } if uses(source)))
    }) || state
        .profiles
        .iter()
        .any(|p| matches!(&p.doc.auth, ProfileAuth::Key { source } if uses(source)))
}

#[cfg(test)]
mod tests;
