//! 本机快照：数据库的副本（见 [`super::archive`]），不加密，秘密仍是本机主密钥加密的密文，只能在这台电脑上恢复。
//!
//! 保存在 `{数据目录}/backups` 中，文件名为 `snapshot-年月日-时分秒-原因.termlet-snapshot`。
//! 每个原因各自保留最近的若干份（自动快照的份数由设置决定）；自动快照与最新的一份自动快照内容相同时，
//! 删除旧的那份（只保留最新的时间）。

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;

use chrono::TimeZone as _;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::archive::{self, ArchiveKind, Counts, Manifest, TempFile};
use super::{BackupError, error};

const PREFIX: &str = "snapshot-";
const EXTENSION: &str = ".termlet-snapshot";

/// 快照的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SnapshotReason {
    /// 每天自动保存
    Auto,
    /// 用户手动保存
    Manual,
    /// 恢复（完整备份或快照）之前
    Restore,
    /// 导入主机之前
    Import,
    /// 升级数据库结构（新版本第一次启动）之前
    Upgrade,
    /// 应用云同步拉取到的变更之前
    Sync,
}

impl SnapshotReason {
    pub const ALL: [SnapshotReason; 6] = [
        SnapshotReason::Auto,
        SnapshotReason::Manual,
        SnapshotReason::Restore,
        SnapshotReason::Import,
        SnapshotReason::Upgrade,
        SnapshotReason::Sync,
    ];

    /// 稳定的名称（文件名、日志中使用）。
    pub fn name(self) -> &'static str {
        match self {
            SnapshotReason::Auto => "auto",
            SnapshotReason::Manual => "manual",
            SnapshotReason::Restore => "restore",
            SnapshotReason::Import => "import",
            SnapshotReason::Upgrade => "upgrade",
            SnapshotReason::Sync => "sync",
        }
    }

    /// 这个原因的快照保留的份数；自动快照的份数由设置决定（见 [`crate::model::BackupSettings`]）。
    pub fn keep(self) -> usize {
        match self {
            SnapshotReason::Auto => crate::model::BackupSettings::default().keep as usize,
            SnapshotReason::Manual => 10,
            SnapshotReason::Restore | SnapshotReason::Import | SnapshotReason::Sync => 5,
            SnapshotReason::Upgrade => 3,
        }
    }
}

/// 一份本机快照。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotInfo {
    /// 文件名（恢复时传回）
    pub name: String,
    /// Unix 毫秒
    pub created_at: i64,
    pub reason: SnapshotReason,
    /// 文件大小（字节）
    pub size: u64,
    /// 保存时的 Termlet 版本
    pub app: String,
    pub counts: Counts,
}

/// 快照的文件名是否合法（只能是本模块生成的名称，不能带路径）。
pub(super) fn valid_name(name: &str) -> bool {
    name.starts_with(PREFIX)
        && name.ends_with(EXTENSION)
        && !name.contains(['/', '\\', ':'])
        && Path::new(name).file_name().and_then(|n| n.to_str()) == Some(name)
}

/// 准备快照目录（只有当前用户能访问；限制不了时照常使用，与数据目录的处理一致）。
pub(super) fn prepare_dir(dir: &Path) -> Result<(), BackupError> {
    std::fs::create_dir_all(dir).map_err(|e| error::write_error(dir, &e))?;
    if let Err(e) = crate::platform::restrict_dir(dir) {
        tracing::warn!(dir = %dir.display(), "无法限制快照目录的访问权限：{e}");
    }
    Ok(())
}

/// 保存一份快照：`dump` 把当前数据库一致地复制到给定的路径（`VACUUM INTO`），之后写成快照并按 `keep` 清理
/// 同一原因的旧快照。
pub(super) fn take(
    dir: &Path,
    reason: SnapshotReason,
    keep: usize,
    app: &str,
    dump: impl FnOnce(&Path) -> Result<(), BackupError>,
) -> Result<SnapshotInfo, BackupError> {
    prepare_dir(dir)?;
    let copy = TempFile::new(dir);
    dump(copy.path())?;
    let mut manifest = Manifest::new(ArchiveKind::Snapshot, app);
    let (counts, schema) = archive::inspect_copy(copy.path())?;
    manifest.counts = counts;
    manifest.schema = schema;
    manifest.reason = Some(reason);
    manifest.audit = true;
    manifest.digest = Some(archive::digest(copy.path())?);

    // 自动快照只与最新的一份比较：内容相同时用新的替换它（新的写好之后才删除，写入失败时旧的还在）
    let duplicate = if reason == SnapshotReason::Auto {
        list(dir)?
            .into_iter()
            .find(|s| s.reason == reason)
            .filter(|latest| {
                read_manifest(&dir.join(&latest.name)).is_ok_and(|m| m.digest == manifest.digest)
            })
    } else {
        None
    };

    let base = format!(
        "{PREFIX}{}-{}",
        chrono::Local
            .timestamp_millis_opt(manifest.created_at)
            .single()
            .map(|t| t.format("%Y%m%d-%H%M%S").to_string())
            .unwrap_or_else(|| manifest.created_at.to_string()),
        reason.name()
    );
    let mut name = format!("{base}{EXTENSION}");
    let mut n = 2;
    while dir.join(&name).exists() {
        name = format!("{base}-{n}{EXTENSION}");
        n += 1;
    }
    let path = dir.join(&name);
    let part = dir.join(format!("{name}.part"));
    let written = (|| {
        let file = File::create(&part)?;
        let mut out = BufWriter::new(file);
        archive::write(&mut out, &manifest, copy.path())?;
        out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        let _ = crate::platform::restrict_file(&part);
        std::fs::rename(&part, &path)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&part);
        return Err(error::write_error(&path, &e));
    }
    if let Some(duplicate) = duplicate {
        let _ = std::fs::remove_file(dir.join(&duplicate.name));
    }
    // 新快照已经写好：旧的删不掉（例如被别的程序占用）只记警告，不让恢复、同步因此失败
    if let Err(e) = prune(dir, reason, keep) {
        tracing::warn!(reason = reason.name(), "清理旧快照失败：{e}");
    }
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    tracing::info!(name = %name, reason = reason.name(), hosts = counts.hosts, "已保存本机快照");
    Ok(SnapshotInfo {
        name,
        created_at: manifest.created_at,
        reason,
        size,
        app: manifest.app,
        counts,
    })
}

/// 读出快照文件中的说明（不解压数据库）。
pub(super) fn read_manifest(path: &Path) -> Result<Manifest, BackupError> {
    let file = File::open(path).map_err(|e| error::read_error(path, &e))?;
    archive::read_manifest(&mut BufReader::new(file))
}

/// 只保留同一原因最新的 `keep` 份快照（`keep` 小于 1 时按 1）。
pub(super) fn prune(dir: &Path, reason: SnapshotReason, keep: usize) -> Result<(), BackupError> {
    let keep = keep.max(1);
    for old in list(dir)?
        .into_iter()
        .filter(|s| s.reason == reason)
        .skip(keep)
    {
        let path = dir.join(&old.name);
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(error::write_error(&path, &e));
        }
    }
    Ok(())
}

/// 列出 `dir` 中的快照，从新到旧；读不出说明的文件跳过。目录不存在时为空。
pub(super) fn list(dir: &Path) -> Result<Vec<SnapshotInfo>, BackupError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(error::read_error(dir, &e)),
    };
    let mut list = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !valid_name(&name) || !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let Ok(manifest) = read_manifest(&entry.path()) else {
            continue;
        };
        let (ArchiveKind::Snapshot, Some(reason)) = (manifest.kind, manifest.reason) else {
            continue;
        };
        list.push(SnapshotInfo {
            name,
            created_at: manifest.created_at,
            reason,
            size: entry.metadata().map(|m| m.len()).unwrap_or(0),
            app: manifest.app,
            counts: manifest.counts,
        });
    }
    list.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.name.cmp(&a.name))
    });
    Ok(list)
}

/// 删除上次异常退出时留下的临时文件。
pub(super) fn clean_temp(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(archive::TEMP_PREFIX)
            || (name.starts_with(PREFIX) && name.ends_with(".part"))
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// 升级数据库结构之前，用正在打开的连接为它保存一份快照（原因为"升级"）。
pub fn save_upgrade_snapshot(conn: &Connection, dir: &Path) -> Result<SnapshotInfo, BackupError> {
    let reason = SnapshotReason::Upgrade;
    take(dir, reason, reason.keep(), crate::VERSION, |copy| {
        conn.execute("VACUUM INTO ?1", [copy.to_string_lossy()])?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_validated() {
        assert!(valid_name("snapshot-20261019-231500-auto.termlet-snapshot"));
        assert!(valid_name(
            "snapshot-20261019-231500-auto-2.termlet-snapshot"
        ));
        for bad in [
            "../x.termlet-snapshot",
            "snapshot-x.txt",
            r"snapshot-..\x.termlet-snapshot",
            "snapshot-../x.termlet-snapshot",
            "snapshot-c:x.termlet-snapshot",
            "x.termlet-snapshot",
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn listing_skips_unreadable_files_and_cleans_temp() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path()
                .join("snapshot-20000101-000000-manual.termlet-snapshot"),
            b"not a snapshot",
        )
        .unwrap();
        std::fs::write(dir.path().join(".tmp-abc.db"), b"x").unwrap();
        std::fs::write(dir.path().join("snapshot-x.termlet-snapshot.part"), b"x").unwrap();
        assert!(list(dir.path()).unwrap().is_empty());
        clean_temp(dir.path());
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["snapshot-20000101-000000-manual.termlet-snapshot"]);
        assert!(list(&dir.path().join("missing")).unwrap().is_empty());
    }
}
