//! SQLite 数据库：打开、迁移、读写。
//!
//! - 只有一条连接，由互斥锁保护；写入都在事务中进行。
//! - `PRAGMA user_version` 记录结构版本。迁移只往前升，升级已有的库之前在 `backups/` 保存一份本机快照
//!   （原因为"升级"，见 [`crate::backup`]）；库的版本比本程序新（用过更新版本的 Termlet）时直接报错、不动文件。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::{CoreError, Result};

/// 当前的结构版本。
pub const SCHEMA_VERSION: i64 = 1;

/// 第 i 项把结构从版本 i 升到 i + 1。
const MIGRATIONS: &[&str] = &[include_str!("schema_v1.sql")];

/// 条目表的一行（见 `schema_v1.sql`）。
#[derive(Clone, Debug, PartialEq)]
pub struct ItemRow {
    pub id: String,
    pub parent_id: Option<String>,
    pub sort_key: String,
    pub doc: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub deleted_at: Option<i64>,
}

/// 设置表的一行。
#[derive(Clone, Debug, PartialEq)]
pub struct SettingRow {
    pub id: String,
    pub doc: String,
    pub updated_at: i64,
}

pub struct Database {
    conn: Mutex<Connection>,
    path: Option<PathBuf>,
}

impl Database {
    /// 打开（必要时创建并迁移）数据库。`backups_dir` 用来存放迁移前的备份。
    pub fn open(path: &Path, backups_dir: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        configure(&conn)?;
        migrate(&conn, Some((path, backups_dir)))?;
        Ok(Self {
            conn: Mutex::new(conn),
            path: Some(path.to_path_buf()),
        })
    }

    /// 内存中的数据库（测试用）。
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        configure(&conn)?;
        migrate(&conn, None)?;
        Ok(Self {
            conn: Mutex::new(conn),
            path: None,
        })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// 读取。
    pub fn read<R>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<R>) -> Result<R> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        Ok(f(&conn)?)
    }

    /// 在一个事务中写入；`f` 返回错误时回滚。
    pub fn write<R>(&self, f: impl FnOnce(&Transaction) -> Result<R>) -> Result<R> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction()?;
        let result = f(&tx)?;
        tx.commit()?;
        Ok(result)
    }

    /// 直接使用连接（`ATTACH` 等不能在事务中执行的操作）；期间其他读写等待。
    pub(crate) fn with_connection<R>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<R>,
    ) -> Result<R> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut conn)
    }

    pub fn meta(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.read(|conn| get_meta(conn, key))
    }

    pub fn set_meta(&self, key: &str, value: &[u8]) -> Result<()> {
        self.write(|tx| Ok(set_meta(tx, key, value)?))
    }
}

pub fn get_meta(conn: &Connection, key: &str) -> rusqlite::Result<Option<Vec<u8>>> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
        row.get(0)
    })
    .optional()
}

pub fn set_meta(conn: &Connection, key: &str, value: &[u8]) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub fn delete_meta(conn: &Connection, key: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM meta WHERE key = ?1", [key])?;
    Ok(())
}

/// 读出一张条目表的全部行（含墓碑），按排序键排列。
pub fn load_items(conn: &Connection, table: &str) -> rusqlite::Result<Vec<ItemRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, parent_id, sort_key, doc, created_at, updated_at, deleted_at
         FROM {table} ORDER BY sort_key, id"
    ))?;
    let rows = stmt.query_map([], |row| {
        Ok(ItemRow {
            id: row.get(0)?,
            parent_id: row.get(1)?,
            sort_key: row.get(2)?,
            doc: row.get(3)?,
            created_at: row.get(4)?,
            updated_at: row.get(5)?,
            deleted_at: row.get(6)?,
        })
    })?;
    rows.collect()
}

/// 写入（新建或覆盖）一行条目。
pub fn put_item(conn: &Connection, table: &str, row: &ItemRow) -> rusqlite::Result<()> {
    conn.execute(
        &format!(
            "INSERT INTO {table} (id, parent_id, sort_key, doc, created_at, updated_at, deleted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (id) DO UPDATE SET parent_id = excluded.parent_id,
               sort_key = excluded.sort_key, doc = excluded.doc, created_at = excluded.created_at,
               updated_at = excluded.updated_at, deleted_at = excluded.deleted_at"
        ),
        params![
            row.id,
            row.parent_id,
            row.sort_key,
            row.doc,
            row.created_at,
            row.updated_at,
            row.deleted_at
        ],
    )?;
    Ok(())
}

pub fn load_settings(conn: &Connection, table: &str) -> rusqlite::Result<Vec<SettingRow>> {
    let mut stmt = conn.prepare(&format!("SELECT id, doc, updated_at FROM {table}"))?;
    let rows = stmt.query_map([], |row| {
        Ok(SettingRow {
            id: row.get(0)?,
            doc: row.get(1)?,
            updated_at: row.get(2)?,
        })
    })?;
    rows.collect()
}

pub fn put_setting(conn: &Connection, table: &str, row: &SettingRow) -> rusqlite::Result<()> {
    conn.execute(
        &format!(
            "INSERT INTO {table} (id, doc, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (id) DO UPDATE SET doc = excluded.doc, updated_at = excluded.updated_at"
        ),
        params![row.id, row.doc, row.updated_at],
    )?;
    Ok(())
}

/// 打开从备份、快照中解压出的数据库文件（不是正在使用的数据库）：不用 WAL，按迁移升级到当前结构（不另做备份）。
/// 结构版本比本程序新时返回 [`CoreError::DatabaseTooNew`]，文件不是数据库时返回 [`CoreError::Database`]。
pub(crate) fn open_detached(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let _: String = conn.query_row("PRAGMA journal_mode = DELETE", [], |row| row.get(0))?;
    migrate(&conn, None)?;
    Ok(conn)
}

fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // 内存数据库不支持 WAL，返回 memory，忽略
    let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

fn migrate(conn: &Connection, backup: Option<(&Path, &Path)>) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let supported = MIGRATIONS.len() as i64;
    debug_assert_eq!(supported, SCHEMA_VERSION);
    if version > supported {
        return Err(CoreError::DatabaseTooNew {
            path: backup.map(|(p, _)| p.to_path_buf()).unwrap_or_default(),
            found: version,
            supported,
        });
    }
    if version == supported {
        return Ok(());
    }
    if version > 0
        && let Some((_, backups_dir)) = backup
    {
        backup_before_migration(conn, backups_dir, version)?;
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", index as i64 + 1)?;
        tx.commit()?;
        tracing::info!(version = index + 1, "数据库结构已升级");
    }
    Ok(())
}

/// 升级已有的库之前保存一份本机快照（原因为"升级"，保留最近 3 份），可以在设置的"备份"中恢复。
fn backup_before_migration(conn: &Connection, backups_dir: &Path, version: i64) -> Result<()> {
    let info = crate::backup::save_upgrade_snapshot(conn, backups_dir)
        .map_err(|e| CoreError::DataDir(format!("升级数据库结构前保存快照失败：{e}")))?;
    tracing::info!(snapshot = %info.name, from = version, "升级数据库结构前已保存快照");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_database_has_current_schema() {
        let db = Database::open_in_memory().unwrap();
        let version: i64 = db
            .read(|c| c.query_row("PRAGMA user_version", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn newer_database_is_rejected_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("data.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", SCHEMA_VERSION + 5)
                .unwrap();
        }
        let err = Database::open(&path, &tmp.path().join("backups"))
            .err()
            .unwrap();
        assert!(
            matches!(err, CoreError::DatabaseTooNew { found, .. } if found == SCHEMA_VERSION + 5)
        );
    }

    #[test]
    fn items_round_trip() {
        let db = Database::open_in_memory().unwrap();
        let row = ItemRow {
            id: "h_1".into(),
            parent_id: Some("g_1".into()),
            sort_key: "V".into(),
            doc: "{\"name\":\"web\"}".into(),
            created_at: 1,
            updated_at: 2,
            deleted_at: None,
        };
        db.write(|tx| Ok(put_item(tx, "host", &row)?)).unwrap();
        let mut changed = row.clone();
        changed.updated_at = 3;
        changed.doc = "{}".into();
        changed.deleted_at = Some(3);
        db.write(|tx| Ok(put_item(tx, "host", &changed)?)).unwrap();
        let rows = db.read(|c| load_items(c, "host")).unwrap();
        assert_eq!(rows, vec![changed]);
    }

    #[test]
    fn meta_round_trip() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(db.meta("x").unwrap(), None);
        db.set_meta("x", b"1").unwrap();
        db.set_meta("x", b"2").unwrap();
        assert_eq!(db.meta("x").unwrap().as_deref(), Some(&b"2"[..]));
    }
}
