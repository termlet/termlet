//! 恢复的内部步骤：检查解压出的数据库、转换加密列、整体替换当前数据。

use std::path::Path;

use rusqlite::Connection;
use zeroize::Zeroizing;

use super::BackupError;
use super::archive::{self, AUDIT_TABLE, DataKey};
use crate::db::{self, Database};
use crate::model::{
    AppLockSettings, BackupSettings, CliSettings, Kind, LocalConnectionSettings, LogSettings,
    SettingsSection, UsageSettings, WindowSettings,
};
use crate::vault::{self, Vault};

/// 恢复时保留本机的表：数据目录 ID、保险库的模式与校验值、主密码的派生参数（以后的同步状态也属于本机）。
const LOCAL_TABLES: &[&str] = &["meta"];

/// 恢复时保留本机当前值的本机设置组：外部命令行的开关与授权、应用锁、自动快照、日志与崩溃报告、使用统计、窗口
/// （关闭时最小化到托盘）、本机的连接设置（X 显示、确认过的代理命令）。恢复一份旧备份不应悄悄重新打开用户已经关掉的
/// 命令行访问、崩溃报告、使用统计，关掉应用锁，改变这台电脑关窗口的行为，或让这台电脑不经确认就执行备份中的代理命令
/// （备份可能来自另一台电脑）。本地终端的列表随备份恢复。
fn kept_local_settings() -> [&'static str; 7] {
    [
        CliSettings::ID,
        AppLockSettings::ID,
        BackupSettings::ID,
        LogSettings::ID,
        UsageSettings::ID,
        WindowSettings::ID,
        LocalConnectionSettings::ID,
    ]
}

/// 加密列中的一项：秘密（`secret` 表，归属与字段）或命令历史（`command_history` 表，行 ID 与归属）。
pub(super) enum Sealed<'a> {
    Secret { owner: &'a str, field: &'a str },
    History { id: i64, owner: &'a str },
}

impl Sealed<'_> {
    /// 本机保险库加密时的附加数据（与存储、命令历史相同）。
    pub fn vault_aad(&self) -> Vec<u8> {
        match self {
            Sealed::Secret { owner, field } => crate::store::secret_aad(owner, field),
            Sealed::History { owner, .. } => crate::history::aad(owner),
        }
    }

    /// 完整备份中用数据密钥加密时的附加数据：密文不能被挪到别的记录。
    fn backup_aad(&self) -> Vec<u8> {
        match self {
            Sealed::Secret { owner, field } => {
                format!("termlet:backup:v1:secret:{owner}:{field}").into_bytes()
            }
            Sealed::History { id, owner } => {
                format!("termlet:backup:v1:history:{id}:{owner}").into_bytes()
            }
        }
    }
}

/// 对一项加密数据的处理。
pub(super) enum Rewrite {
    /// 不改（只校验）
    Keep,
    /// 换成新的密文
    Replace(Vec<u8>),
    /// 删除这一行
    Remove,
}

/// 逐条处理 `conn`（解压出的数据库或备份用的副本）中的加密列，在一个事务中完成。
pub(super) fn rewrite(
    conn: &mut Connection,
    mut convert: impl FnMut(&Sealed<'_>, &[u8]) -> Result<Rewrite, BackupError>,
) -> Result<(), BackupError> {
    let tx = conn.transaction()?;
    let secrets: Vec<(String, String, Vec<u8>)> = {
        let mut stmt = tx.prepare("SELECT owner, field, blob FROM secret")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (owner, field, blob) in &secrets {
        match convert(&Sealed::Secret { owner, field }, blob)? {
            Rewrite::Keep => {}
            Rewrite::Replace(new) => {
                tx.execute(
                    "UPDATE secret SET blob = ?1 WHERE owner = ?2 AND field = ?3",
                    rusqlite::params![new, owner, field],
                )?;
            }
            Rewrite::Remove => {
                tx.execute(
                    "DELETE FROM secret WHERE owner = ?1 AND field = ?2",
                    rusqlite::params![owner, field],
                )?;
            }
        }
    }
    let history: Vec<(i64, String, Vec<u8>)> = {
        let mut stmt = tx.prepare("SELECT id, owner, command FROM command_history")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (id, owner, blob) in &history {
        match convert(&Sealed::History { id: *id, owner }, blob)? {
            Rewrite::Keep => {}
            Rewrite::Replace(new) => {
                tx.execute(
                    "UPDATE command_history SET command = ?1 WHERE id = ?2",
                    rusqlite::params![new, id],
                )?;
            }
            Rewrite::Remove => {
                tx.execute("DELETE FROM command_history WHERE id = ?1", [id])?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

/// 备份时：本机保险库的密文 → 数据密钥的密文。解不开的（已损坏，本来就用不了）不带走，记一条警告。
pub(super) fn to_data_key(
    conn: &mut Connection,
    vault: &Vault,
    key: &DataKey,
) -> Result<(), BackupError> {
    let mut dropped = 0usize;
    rewrite(conn, |item, blob| {
        match vault.open_sealed(&item.vault_aad(), blob) {
            Ok(plain) => Ok(Rewrite::Replace(vault::seal_with(
                &key.0,
                &item.backup_aad(),
                &plain,
            ))),
            Err(crate::CoreError::Decrypt) => {
                dropped += 1;
                Ok(Rewrite::Remove)
            }
            Err(e) => Err(e.into()),
        }
    })?;
    if dropped > 0 {
        tracing::warn!(
            dropped,
            "有秘密或命令历史无法用本机的主密钥解开，没有放进备份"
        );
    }
    Ok(())
}

/// 恢复完整备份前：确认全部密文都能用数据密钥解开（只校验，不改动）。
pub(super) fn verify_data_key(conn: &mut Connection, key: &DataKey) -> Result<(), BackupError> {
    rewrite(conn, |item, blob| {
        vault::open_with(&key.0, &item.backup_aad(), blob)
            .map(|_| Rewrite::Keep)
            .map_err(|_| BackupError::Invalid)
    })
}

/// 恢复完整备份：数据密钥的密文 → 本机保险库的密文。
pub(super) fn from_data_key(
    conn: &mut Connection,
    vault: &Vault,
    key: &DataKey,
) -> Result<(), BackupError> {
    rewrite(conn, |item, blob| {
        let plain: Zeroizing<Vec<u8>> =
            vault::open_with(&key.0, &item.backup_aad(), blob).map_err(|_| BackupError::Invalid)?;
        Ok(Rewrite::Replace(vault.seal(&item.vault_aad(), &plain)?))
    })
}

/// 恢复快照前：确认全部密文都能用本机保险库的主密钥解开（只校验）。
pub(super) fn verify_vault(conn: &mut Connection, vault: &Vault) -> Result<(), BackupError> {
    rewrite(conn, |item, blob| {
        match vault.open_sealed(&item.vault_aad(), blob) {
            Ok(_) => Ok(Rewrite::Keep),
            Err(crate::CoreError::Decrypt) => Err(BackupError::ForeignSnapshot),
            Err(e) => Err(e.into()),
        }
    })
}

/// 打开解压出的数据库：按迁移升级到当前结构，并确认每张条目表都能读出。
pub(super) fn open_extracted(path: &Path) -> Result<Connection, BackupError> {
    let conn = match db::open_detached(path) {
        Ok(conn) => conn,
        Err(crate::CoreError::DatabaseTooNew {
            found, supported, ..
        }) => return Err(BackupError::TooNew { found, supported }),
        Err(e) => {
            tracing::warn!("备份中的数据库无法读取：{e}");
            return Err(BackupError::Invalid);
        }
    };
    let readable = (|| -> rusqlite::Result<()> {
        for kind in Kind::ALL {
            db::load_items(&conn, kind.table())?;
        }
        for table in [
            "setting",
            "local_setting",
            "secret",
            "command_history",
            "known_host",
        ] {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| {
                r.get::<_, i64>(0)
            })?;
        }
        Ok(())
    })();
    if let Err(e) = readable {
        tracing::warn!("备份中的数据库不完整：{e}");
        return Err(BackupError::Invalid);
    }
    Ok(conn)
}

/// 在一个事务中用 `src`（已升级到当前结构的数据库文件）替换当前数据库中除本机的表以外的全部表。
/// `with_audit` 为 false 时保留本机的审计日志。
pub(super) fn replace_tables(db: &Database, src: &Path, with_audit: bool) -> crate::Result<()> {
    db.with_connection(|conn| {
        conn.execute(
            "ATTACH DATABASE ?1 AS src",
            [src.to_string_lossy().into_owned()],
        )?;
        let result = copy_tables(conn, with_audit);
        let detached = conn.execute("DETACH DATABASE src", []);
        result?;
        detached?;
        Ok(())
    })
}

fn copy_tables(conn: &mut Connection, with_audit: bool) -> crate::Result<()> {
    let tx = conn.transaction()?;
    let tables: Vec<String> = {
        let mut stmt = tx.prepare(
            "SELECT name FROM main.sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for table in &tables {
        if LOCAL_TABLES.contains(&table.as_str()) || (table == AUDIT_TABLE && !with_audit) {
            continue;
        }
        let columns = common_columns(&tx, table)?;
        let quoted = quote(table);
        // 本机设置中保留的几组（ID 是代码中的常量，直接写进语句）
        let filter = if table == "local_setting" {
            let ids: Vec<String> = kept_local_settings()
                .iter()
                .map(|id| format!("'{id}'"))
                .collect();
            format!(" WHERE id NOT IN ({})", ids.join(", "))
        } else {
            String::new()
        };
        tx.execute(&format!("DELETE FROM main.{quoted}{filter}"), [])?;
        if columns.is_empty() {
            continue;
        }
        let list = columns.join(", ");
        tx.execute(
            &format!("INSERT INTO main.{quoted} ({list}) SELECT {list} FROM src.{quoted}{filter}"),
            [],
        )
        .map_err(|e| crate::CoreError::DataDir(format!("恢复 {table} 失败：{e}")))?;
    }
    tx.commit()?;
    Ok(())
}

/// 两边都有的列（已加引号）。两边都已迁移到同一版本，正常情况下完全相同；备份中缺少整张表时为空。
fn common_columns(conn: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    let columns = |schema: &str| -> rusqlite::Result<Vec<String>> {
        let mut stmt = conn.prepare("SELECT name FROM pragma_table_info(?1, ?2)")?;
        let rows = stmt.query_map([table, schema], |r| r.get(0))?;
        rows.collect()
    };
    let src = columns("src")?;
    Ok(columns("main")?
        .into_iter()
        .filter(|c| src.contains(c))
        .map(|c| quote(&c))
        .collect())
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// 统计 `conn` 中的数量与结构版本。
pub(super) fn counts_and_schema(conn: &Connection) -> Result<(archive::Counts, i64), BackupError> {
    let counts = archive::count_rows(conn)?;
    let schema: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    Ok((counts, schema))
}
