//! 完整备份与恢复、本机快照。
//!
//! - **完整备份**：数据库的一致副本（`VACUUM INTO`，含主机指纹与命令历史），整个文件用用户设的密码加密
//!   （Argon2id + AES-256-GCM，见 [`envelope`]），可以在另一台电脑上恢复。秘密与命令历史在备份时从本机保险库的主密钥
//!   转为用随机的数据密钥加密，数据密钥放在加密的说明中；恢复时再转回那台电脑的主密钥。不带走本机的 `meta`
//!   （数据目录 ID、保险库的校验值、主密码的派生参数），可选是否带走审计日志。
//! - **本机快照**：数据库的副本，不加密，其中的秘密仍是本机主密钥加密的密文，只能在这台电脑上恢复，用于误删、恢复或
//!   导入出错后回退。保存在 `{数据目录}/backups` 中：每天一份自动快照（内容没有变化时只保留最新的一份，份数由设置决定），
//!   恢复、导入、升级数据库结构、应用云同步的变更之前各保存一份，也可以手动保存（见 [`SnapshotReason`]）。
//! - **恢复**（完整备份或快照）：解压出的数据库先按迁移升级到当前结构、确认每张表都能读出、全部密文都能解开，
//!   再保存一份当前数据的快照，最后在一个事务中整体替换除 `meta` 以外的全部表（失败时回滚）并重新读入内存，
//!   存储发出全部种类的变更事件。来自更新版本（结构版本更高）的不能恢复。
//!
//! # 用法（界面）
//!
//! 启动时用存储与 `DataDir::backups_dir()` 建一个 [`Backups`]（克隆很便宜），调用一次 [`Backups::clean_temp`]，
//! 并用 [`Backups::spawn_auto`] 开始自动快照。其余都是异步函数：派生密钥（几百毫秒）、压缩、加密、读写数据库都在
//! `spawn_blocking` 中进行，必须在核心的运行时上执行（`crate::runtime::spawn`，返回的 future 是 `Send`）；
//! 同一时间只进行一项，其余等待。
//!
//! - 创建备份：界面校验两次输入的密码一致后，用保存对话框选好路径调 [`Backups::create_backup`]。
//!   创建前可以用 [`Backups::current_counts`] 显示"主机 N 台、密钥 N 把……"。
//! - 恢复备份：先 [`Backups::inspect_backup`]（没给密码时返回 [`BackupError::PasswordRequired`]）显示备份的时间、
//!   版本与数量请用户确认，再 [`Backups::restore_backup`]。主密钥丢失（保险库状态为 `KeyMissing` / `KeyMismatch`）时
//!   恢复会先重置保险库：这是在新电脑上找回数据的出路。重置在确认备份可以恢复之后才进行，此后的步骤仍可能失败
//!   （这时保险库已重置而数据没有恢复，再次恢复即可），存储会发出 `VaultChanged` 事件。
//! - 快照：[`Backups::snapshots`] 列出（从新到旧），[`Backups::snapshot`] 手动保存，[`Backups::restore_snapshot`] 恢复。
//! - 定时备份：用 [`Backups`] 建一个 [`BackupSchedule`]，调用一次 [`BackupSchedule::spawn_auto`]（见 [`schedule`]）。
//! - 恢复成功后，正在运行的端口转发等按需要停止（规则可能已不存在），各视图按存储的事件重新读取。

mod archive;
pub mod envelope;
mod error;
mod restore;
pub mod schedule;
mod snapshot;
#[cfg(test)]
pub(crate) mod testkit;
#[cfg(test)]
mod tests;

use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;
use zeroize::Zeroizing;

pub use archive::Counts;
pub use envelope::{MIN_PASSWORD_LEN, SealedKind};
pub use error::BackupError;
pub(crate) use error::{read_error, write_error};
pub use schedule::{
    BackupSchedule, Frequency, RunRecord, ScheduleConfig, ScheduleInput, ScheduleStatus,
};
pub use snapshot::{SnapshotInfo, SnapshotReason, save_upgrade_snapshot};

use crate::db::SCHEMA_VERSION;
use crate::secret::SecretString;
use crate::store::Store;
use crate::vault::VaultStatus;
use archive::{AUDIT_TABLE, ArchiveKind, DataKey, Manifest, TempFile};

/// 自动快照的间隔。
pub const AUTO_INTERVAL: Duration = Duration::from_secs(24 * 3600);
/// 启动后第一次检查自动快照的延迟，之后每小时检查一次。
const AUTO_FIRST_CHECK: Duration = Duration::from_secs(60);
const AUTO_CHECK_EVERY: Duration = Duration::from_secs(3600);

/// 完整备份的说明（恢复前给用户确认；不含转加密用的密钥）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupSummary {
    /// Unix 毫秒
    pub created_at: i64,
    /// 创建备份的 Termlet 版本
    pub app: String,
    /// 数据库的结构版本
    pub schema: i64,
    /// 含审计日志
    pub audit: bool,
    pub counts: Counts,
}

impl BackupSummary {
    fn of(manifest: &Manifest) -> Self {
        Self {
            created_at: manifest.created_at,
            app: manifest.app.clone(),
            schema: manifest.schema,
            audit: manifest.audit,
            counts: manifest.counts,
        }
    }
}

/// 创建完整备份的选项。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BackupOptions {
    /// 包含审计日志（命令行工具的执行记录）
    pub audit: bool,
}

/// 恢复的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreOutcome {
    /// 恢复的是什么（备份或快照的说明）
    pub summary: BackupSummary,
    /// 恢复前保存的当前数据的快照（可以用它回退）
    pub snapshot: SnapshotInfo,
    /// 主密钥丢失，恢复前重置了保险库
    pub vault_reset: bool,
}

/// 完整备份、恢复与本机快照。克隆很便宜，各处共用同一个。
#[derive(Clone)]
pub struct Backups {
    inner: Arc<Inner>,
}

struct Inner {
    store: Arc<Store>,
    dir: PathBuf,
    app: String,
    /// 同一时间只进行一项（备份、恢复、快照）
    lock: Mutex<()>,
    /// 自动快照的间隔（毫秒；测试中调小）
    auto_interval_ms: AtomicI64,
}

/// 在阻塞线程上执行。
async fn blocking<T, F>(f: F) -> Result<T, BackupError>
where
    F: FnOnce() -> Result<T, BackupError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => Err(BackupError::Store(error.to_string())),
    }
}

impl Backups {
    /// `dir` 是快照目录（`DataDir::backups_dir()`），同时存放处理中的临时文件。
    pub fn new(store: Arc<Store>, dir: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                dir: dir.into(),
                app: crate::VERSION.to_string(),
                lock: Mutex::new(()),
                auto_interval_ms: AtomicI64::new(AUTO_INTERVAL.as_millis() as i64),
            }),
        }
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.inner.store
    }

    /// 快照目录（界面"打开所在文件夹"用）。
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// 删除上次异常退出时留下的临时文件（启动时调用一次）。
    pub fn clean_temp(&self) {
        snapshot::clean_temp(&self.inner.dir);
    }

    /// 当前数据的数量（创建备份前显示）。只读数据库中的计数，很快，可以在任何线程调用。
    pub fn current_counts(&self) -> Result<Counts, BackupError> {
        Ok(self.inner.store.database().read(archive::count_rows)?)
    }

    #[cfg(test)]
    fn set_auto_interval(&self, interval: Duration) {
        self.inner
            .auto_interval_ms
            .store(interval.as_millis() as i64, Ordering::Relaxed);
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, ()> {
        self.inner.lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---------------------------------------------------------------- 完整备份

    /// 创建完整备份，写到 `path`（本机的绝对路径，只有当前用户能读写；已存在时替换）。
    /// 密码少于 [`MIN_PASSWORD_LEN`] 个字符时返回 [`BackupError::PasswordTooShort`]；有秘密而保险库已锁定时返回
    /// [`BackupError::VaultLocked`]。
    pub async fn create_backup(
        &self,
        path: PathBuf,
        password: SecretString,
        options: BackupOptions,
    ) -> Result<BackupSummary, BackupError> {
        let this = self.clone();
        blocking(move || this.create_backup_blocking(&path, &password, options)).await
    }

    fn create_backup_blocking(
        &self,
        path: &Path,
        password: &SecretString,
        options: BackupOptions,
    ) -> Result<BackupSummary, BackupError> {
        envelope::check_password(password)?;
        check_output(path)?;
        let _guard = self.guard();
        let store = &self.inner.store;
        snapshot::prepare_dir(&self.inner.dir)?;
        let copy = TempFile::new(&self.inner.dir);
        self.dump(copy.path())?;

        let key = DataKey::generate();
        let (counts, schema) = {
            let mut conn = open_copy(copy.path())?;
            restore::to_data_key(&mut conn, store.vault(), &key)?;
            // 保险库的校验值、主密码的派生参数、数据目录 ID 属于本机，不带走
            conn.execute("DELETE FROM meta", [])?;
            if !options.audit && archive::table_exists(&conn, AUDIT_TABLE)? {
                conn.execute(&format!("DELETE FROM {AUDIT_TABLE}"), [])?;
            }
            // 重建文件，去掉删除的内容与替换前的密文所在的空闲页
            conn.execute_batch("VACUUM")?;
            restore::counts_and_schema(&conn)?
        };
        let mut manifest = Manifest::new(ArchiveKind::Backup, &self.inner.app);
        manifest.schema = schema;
        manifest.audit = options.audit;
        manifest.counts = counts;
        manifest.data_key = Some(key);

        let mut content = Zeroizing::new(Vec::new());
        archive::write(&mut *content, &manifest, copy.path())
            .map_err(|e| BackupError::Store(format!("打包备份失败：{e}")))?;
        let sealed = envelope::seal(SealedKind::Backup, password, &content)?;
        write_file(path, &sealed)?;
        tracing::info!(
            hosts = counts.hosts,
            audit = options.audit,
            "已创建完整备份"
        );
        Ok(BackupSummary::of(&manifest))
    }

    /// 解开完整备份，返回其中的说明（恢复前给用户确认）。没给密码时返回 [`BackupError::PasswordRequired`]；
    /// 选的是导出的主机文件时返回 [`BackupError::IsHostsFile`]。
    pub async fn inspect_backup(
        &self,
        path: PathBuf,
        password: Option<SecretString>,
    ) -> Result<BackupSummary, BackupError> {
        blocking(move || {
            let data = read_file(&path, envelope::MAX_SEALED_SIZE)?;
            let manifest = open_backup(&data, password.as_ref(), None)?;
            Ok(BackupSummary::of(&manifest))
        })
        .await
    }

    /// 用完整备份替换全部数据（见模块说明）。
    pub async fn restore_backup(
        &self,
        path: PathBuf,
        password: SecretString,
    ) -> Result<RestoreOutcome, BackupError> {
        let this = self.clone();
        blocking(move || this.restore_backup_blocking(&path, &password)).await
    }

    fn restore_backup_blocking(
        &self,
        path: &Path,
        password: &SecretString,
    ) -> Result<RestoreOutcome, BackupError> {
        let data = read_file(path, envelope::MAX_SEALED_SIZE)?;
        let _guard = self.guard();
        let store = &self.inner.store;
        snapshot::prepare_dir(&self.inner.dir)?;
        let extracted = TempFile::new(&self.inner.dir);
        let manifest = open_backup(&data, Some(password), Some(extracted.path()))?;
        drop(data);
        let key = manifest.data_key.clone().ok_or(BackupError::Invalid)?;

        let mut conn = restore::open_extracted(extracted.path())?;
        // 先确认全部密文都能用数据密钥解开（只校验），之后才可能重置保险库
        restore::verify_data_key(&mut conn, &key)?;
        let vault_reset = match store.vault_status() {
            VaultStatus::Unlocked => false,
            VaultStatus::KeyMissing | VaultStatus::KeyMismatch => {
                store.reset_vault(None)?;
                tracing::warn!("主密钥丢失，恢复完整备份前已重置保险库");
                true
            }
            VaultStatus::KeyringError(reason) => return Err(BackupError::VaultUnavailable(reason)),
            VaultStatus::NeedsPassword
            | VaultStatus::NeedsNewPassword { .. }
            | VaultStatus::ChooseKeyStore => {
                return Err(BackupError::VaultLocked);
            }
        };
        // 从数据密钥转回本机保险库的主密钥
        restore::from_data_key(&mut conn, store.vault(), &key)?;
        drop(conn);

        let before = self.take_snapshot(SnapshotReason::Restore, SnapshotReason::Restore.keep())?;
        store.replace_all(|db| restore::replace_tables(db, extracted.path(), manifest.audit))?;
        tracing::info!(
            created_at = manifest.created_at,
            hosts = manifest.counts.hosts,
            vault_reset,
            "已从完整备份恢复数据"
        );
        crate::usage::mark(crate::usage::Feature::Backup);
        Ok(RestoreOutcome {
            summary: BackupSummary::of(&manifest),
            snapshot: before,
            vault_reset,
        })
    }

    // ---------------------------------------------------------------- 本机快照

    /// 保存一份本机快照（手动保存为 [`SnapshotReason::Manual`]），按原因清理旧的。
    pub async fn snapshot(&self, reason: SnapshotReason) -> Result<SnapshotInfo, BackupError> {
        let this = self.clone();
        blocking(move || this.snapshot_blocking(reason)).await
    }

    /// 与 [`Backups::snapshot`] 相同，在当前线程上执行（核心的其他功能在阻塞线程中调用）。
    pub(crate) fn snapshot_blocking(
        &self,
        reason: SnapshotReason,
    ) -> Result<SnapshotInfo, BackupError> {
        let keep = match reason {
            SnapshotReason::Auto => self.auto_keep(),
            other => other.keep(),
        };
        let _guard = self.guard();
        self.take_snapshot(reason, keep)
    }

    /// 自动快照：设置中开启、且最近一份自动快照超过间隔（或没有）时保存新的，并按设置的份数清理旧的。
    /// 返回是否保存了新的快照。
    pub async fn auto_snapshot(&self) -> Result<bool, BackupError> {
        let this = self.clone();
        blocking(move || this.auto_snapshot_blocking()).await
    }

    fn auto_keep(&self) -> usize {
        self.inner.store.local_settings().backup.keep.clamp(1, 30) as usize
    }

    fn auto_snapshot_blocking(&self) -> Result<bool, BackupError> {
        if !self.inner.store.local_settings().backup.auto {
            return Ok(false);
        }
        let keep = self.auto_keep();
        let _guard = self.guard();
        let interval = self.inner.auto_interval_ms.load(Ordering::Relaxed);
        let now = crate::time::now_ms();
        let recent = snapshot::list(&self.inner.dir)?
            .iter()
            .any(|s| s.reason == SnapshotReason::Auto && now - s.created_at < interval);
        if recent {
            snapshot::prune(&self.inner.dir, SnapshotReason::Auto, keep)?;
            return Ok(false);
        }
        self.take_snapshot(SnapshotReason::Auto, keep)?;
        Ok(true)
    }

    /// 在核心的运行时上定时保存自动快照：启动 1 分钟后检查一次，之后每小时检查一次（是否开启、份数每次读设置）。
    /// 返回的任务一直运行，退出时 `abort` 即可。
    pub fn spawn_auto(&self) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        crate::runtime::spawn(async move {
            tokio::time::sleep(AUTO_FIRST_CHECK).await;
            loop {
                match this.auto_snapshot().await {
                    Ok(true) => tracing::info!("已保存自动快照"),
                    Ok(false) => {}
                    Err(e) => tracing::warn!("保存自动快照失败：{e}"),
                }
                tokio::time::sleep(AUTO_CHECK_EVERY).await;
            }
        })
    }

    /// 列出本机快照，从新到旧。
    pub async fn snapshots(&self) -> Result<Vec<SnapshotInfo>, BackupError> {
        let dir = self.inner.dir.clone();
        blocking(move || snapshot::list(&dir)).await
    }

    /// 用本机快照（[`SnapshotInfo::name`]）替换全部数据。快照中的秘密必须能用本机保险库的主密钥解开，
    /// 否则返回 [`BackupError::ForeignSnapshot`]。
    pub async fn restore_snapshot(&self, name: String) -> Result<RestoreOutcome, BackupError> {
        let this = self.clone();
        blocking(move || this.restore_snapshot_blocking(&name)).await
    }

    fn restore_snapshot_blocking(&self, name: &str) -> Result<RestoreOutcome, BackupError> {
        if !snapshot::valid_name(name) {
            return Err(BackupError::InvalidSnapshotName);
        }
        let path = self.inner.dir.join(name);
        let _guard = self.guard();
        let store = &self.inner.store;
        let file = std::fs::File::open(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => BackupError::SnapshotNotFound,
            _ => read_error(&path, &e),
        })?;
        let mut reader = BufReader::new(file);
        let manifest = archive::read_manifest(&mut reader)?;
        if manifest.kind != ArchiveKind::Snapshot {
            return Err(BackupError::Invalid);
        }
        check_schema(manifest.schema)?;
        let extracted = TempFile::new(&self.inner.dir);
        archive::extract(reader, extracted.path(), archive::MAX_DB_SIZE)?;
        let mut conn = restore::open_extracted(extracted.path())?;
        // 只校验：秘密由本机保险库的主密钥加密
        restore::verify_vault(&mut conn, store.vault())?;
        drop(conn);

        let before = self.take_snapshot(SnapshotReason::Restore, SnapshotReason::Restore.keep())?;
        // 快照总是含审计日志
        store.replace_all(|db| restore::replace_tables(db, extracted.path(), true))?;
        tracing::info!(name, hosts = manifest.counts.hosts, "已从本机快照恢复数据");
        Ok(RestoreOutcome {
            summary: BackupSummary::of(&manifest),
            snapshot: before,
            vault_reset: false,
        })
    }

    // ---------------------------------------------------------------- 内部

    /// 把当前数据库一致地复制到 `path`。
    fn dump(&self, path: &Path) -> Result<(), BackupError> {
        self.inner
            .store
            .database()
            .read(|conn| conn.execute("VACUUM INTO ?1", [path.to_string_lossy()]))?;
        Ok(())
    }

    /// 保存快照（调用方已持有锁）。
    fn take_snapshot(
        &self,
        reason: SnapshotReason,
        keep: usize,
    ) -> Result<SnapshotInfo, BackupError> {
        snapshot::take(&self.inner.dir, reason, keep, &self.inner.app, |copy| {
            self.dump(copy)
        })
    }
}

/// 打开备份用的数据库副本（不用 WAL，改动后整理）。
fn open_copy(path: &Path) -> Result<Connection, BackupError> {
    let conn = Connection::open(path)?;
    let _: String = conn.query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0))?;
    Ok(conn)
}

fn check_schema(schema: i64) -> Result<(), BackupError> {
    if schema > SCHEMA_VERSION {
        return Err(BackupError::TooNew {
            found: schema,
            supported: SCHEMA_VERSION,
        });
    }
    Ok(())
}

/// 解开完整备份，返回说明；`extract_to` 不为空时把其中的数据库解压到这个文件。
fn open_backup(
    data: &[u8],
    password: Option<&SecretString>,
    extract_to: Option<&Path>,
) -> Result<Manifest, BackupError> {
    if !envelope::is_sealed(data) {
        // 没有加密的主机文件（JSON）
        if looks_like_hosts_file(data) {
            return Err(BackupError::IsHostsFile);
        }
        return Err(BackupError::Invalid);
    }
    match envelope::peek_kind(data) {
        Some(SealedKind::Backup) => {}
        Some(SealedKind::Hosts) => return Err(BackupError::IsHostsFile),
        _ => return Err(BackupError::Invalid),
    }
    let (_, payload) = envelope::open(data, password)?;
    let mut reader = &payload[..];
    let manifest = archive::read_manifest(&mut reader)?;
    if manifest.kind != ArchiveKind::Backup || manifest.data_key.is_none() {
        return Err(BackupError::Invalid);
    }
    check_schema(manifest.schema)?;
    if let Some(dest) = extract_to {
        archive::extract(reader, dest, archive::MAX_DB_SIZE)?;
    }
    Ok(manifest)
}

/// 开头像是 Termlet 导出的主机文件（JSON 中的格式名）。
pub(crate) fn looks_like_hosts_file(data: &[u8]) -> bool {
    let head = &data[..data.len().min(256)];
    head.windows(15).any(|w| w == b"\"termlet-hosts\"")
}

/// 读取用户选择的文件（不超过 `limit` 字节）。
pub(crate) fn read_file(path: &Path, limit: u64) -> Result<Vec<u8>, BackupError> {
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return Err(BackupError::NotAbsolute);
    }
    let metadata = std::fs::metadata(path).map_err(|e| read_error(path, &e))?;
    if metadata.is_dir() {
        return Err(BackupError::IsDirectory {
            path: path.to_path_buf(),
        });
    }
    if metadata.len() > limit {
        return Err(BackupError::TooLarge);
    }
    let file = std::fs::File::open(path).map_err(|e| read_error(path, &e))?;
    let mut data = Vec::with_capacity(metadata.len() as usize);
    file.take(limit + 1)
        .read_to_end(&mut data)
        .map_err(|e| read_error(path, &e))?;
    if data.len() as u64 > limit {
        return Err(BackupError::TooLarge);
    }
    Ok(data)
}

/// 检查要写入的路径：绝对路径、不是目录。
pub(crate) fn check_output(path: &Path) -> Result<(), BackupError> {
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return Err(BackupError::NotAbsolute);
    }
    if path.is_dir() {
        return Err(BackupError::IsDirectory {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

/// 写入用户选择的文件（只有当前用户能读写；先写临时文件再替换，不会留下写了一半的文件）。
pub(crate) fn write_file(path: &Path, data: &[u8]) -> Result<(), BackupError> {
    check_output(path)?;
    crate::platform::write_private_file(path, data).map_err(|e| write_error(path, &e))
}
