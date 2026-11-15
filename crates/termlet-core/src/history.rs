//! 命令历史：在终端按回车时记下的命令，每台主机最近 1000 条。
//!
//! 只在本机（不同步、不进同步文件），每条命令由保险库加密保存，附加数据绑定所属的主机（换到别的主机下会解密失败）。
//! 同一条命令再次执行时只留最新的一次。保险库重置后旧的历史无法解密，一并清空。
//! 只有桌面端读取（历史窗口、快捷命令选择器、终端的命令补全提示），命令行工具与 MCP 没有读取的接口。

use rusqlite::Connection;

use crate::store::Store;
use crate::{CoreError, Result};

/// 每台主机最多保留的条数。
pub const MAX_PER_HOST: usize = 1000;
/// 太长的不记（多半是粘贴的脚本）。
const MAX_COMMAND_LEN: usize = 4096;

/// 一条历史。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    /// 数据库中的行 ID（界面上区分各条；同一条命令再次执行后换成新的行）
    pub id: i64,
    pub command: String,
    /// 最近一次执行的时间（Unix 毫秒）
    pub used_at: i64,
}

/// 历史属于谁：保存的主机为主机 ID，快速连接为 `quick:用户@地址:端口`，本地终端为 `local:<shell ID>`。
pub fn quick_key(username: &str, host: &str, port: u16) -> String {
    format!("quick:{username}@{host}:{port}")
}

pub fn local_key(shell_id: &str) -> String {
    format!("local:{shell_id}")
}

/// 加密时的附加数据：绑定所属的主机（备份时逐行转加密也用它）。
pub(crate) fn aad(owner: &str) -> Vec<u8> {
    format!("termlet:history:v1:{owner}").into_bytes()
}

/// 读出这台主机的全部历史（新的在前）。解不开的（保险库重置过）跳过。
/// 记录、删除时在写入的事务中读，两个终端同时记同一条命令也不会留下重复的。
fn load(store: &Store, conn: &Connection, owner: &str) -> rusqlite::Result<Vec<HistoryEntry>> {
    let mut stmt = conn.prepare(
        "SELECT id, command, used_at FROM command_history WHERE owner = ?1 ORDER BY used_at DESC, id DESC",
    )?;
    let rows: Vec<(i64, Vec<u8>, i64)> = stmt
        .query_map([owner], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let aad = aad(owner);
    Ok(rows
        .into_iter()
        .filter_map(|(id, blob, used_at)| {
            let plain = store.vault().open_sealed(&aad, &blob).ok()?;
            let command = String::from_utf8(plain.to_vec()).ok()?;
            Some(HistoryEntry {
                id,
                command,
                used_at,
            })
        })
        .collect())
}

/// 记下一条执行的命令。空的、太长的不记；已有同样的命令时移到最新。
pub fn record(store: &Store, owner: &str, command: &str) -> Result<()> {
    let command = command.trim();
    if command.is_empty() || command.len() > MAX_COMMAND_LEN {
        return Ok(());
    }
    let blob = store.vault().seal(&aad(owner), command.as_bytes())?;
    let now = crate::time::now_ms();
    store.database().write(|tx| {
        let existing = load(store, tx, owner)?;
        let duplicate = existing.iter().filter(|e| e.command == command);
        // 留下最新的 MAX_PER_HOST - 1 条（加上这一条）
        let stale = existing
            .iter()
            .filter(|e| e.command != command)
            .skip(MAX_PER_HOST - 1);
        for entry in duplicate.chain(stale) {
            tx.execute("DELETE FROM command_history WHERE id = ?1", [entry.id])?;
        }
        tx.execute(
            "INSERT INTO command_history (owner, command, used_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![owner, blob, now],
        )?;
        Ok(())
    })
}

/// 这台主机的历史，新的在前。
pub fn list(store: &Store, owner: &str) -> Result<Vec<HistoryEntry>> {
    store.database().read(|conn| load(store, conn, owner))
}

/// 一台主机的历史的版本：条数、行 ID 之和与最近一次执行的时间，不用解密。记录、删除、清空、恢复备份之后都会变
/// （同一条命令再次执行会删掉旧行、插入新行）；终端的命令补全提示据此判断要不要重新读取历史。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Revision {
    count: i64,
    ids: i64,
    latest: i64,
}

/// 这台主机的历史现在的版本（见 [`Revision`]）。
pub fn revision(store: &Store, owner: &str) -> Result<Revision> {
    store.database().read(|conn| {
        conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(id), 0), COALESCE(MAX(used_at), 0) FROM command_history WHERE owner = ?1",
            [owner],
            |r| {
                Ok(Revision {
                    count: r.get(0)?,
                    ids: r.get(1)?,
                    latest: r.get(2)?,
                })
            },
        )
    })
}

/// 删除一条。
pub fn delete(store: &Store, owner: &str, command: &str) -> Result<()> {
    store.database().write(|tx| {
        let ids: Vec<i64> = load(store, tx, owner)?
            .into_iter()
            .filter(|e| e.command == command)
            .map(|e| e.id)
            .collect();
        if ids.is_empty() {
            return Err(CoreError::NotFound("命令历史"));
        }
        for id in &ids {
            tx.execute("DELETE FROM command_history WHERE id = ?1", [id])?;
        }
        Ok(())
    })
}

/// 清空一台主机的历史；`owner` 为 `None` 时清空全部。
pub fn clear(store: &Store, owner: Option<&str>) -> Result<()> {
    store.database().write(|tx| {
        match owner {
            Some(owner) => tx.execute("DELETE FROM command_history WHERE owner = ?1", [owner])?,
            None => tx.execute("DELETE FROM command_history", [])?,
        };
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::memory_store;

    fn commands(store: &Store, owner: &str) -> Vec<String> {
        list(store, owner)
            .unwrap()
            .into_iter()
            .map(|e| e.command)
            .collect()
    }

    #[test]
    fn newest_first_deduplicated_and_per_host() {
        let store = memory_store();
        for c in ["ls", "  df -h ", "ls", "", "uptime"] {
            record(&store, "h_web", c).unwrap();
        }
        record(&store, "h_db", "psql").unwrap();
        assert_eq!(commands(&store, "h_web"), ["uptime", "ls", "df -h"]);
        assert_eq!(commands(&store, "h_db"), ["psql"]);

        delete(&store, "h_web", "ls").unwrap();
        assert_eq!(commands(&store, "h_web"), ["uptime", "df -h"]);
        clear(&store, Some("h_web")).unwrap();
        assert!(commands(&store, "h_web").is_empty());
        assert_eq!(commands(&store, "h_db"), ["psql"]);
    }

    // 版本随记录、删除、清空变化，只看这台主机；没有变化时相同
    #[test]
    fn revision_changes_with_the_history_of_its_host() {
        let store = memory_store();
        let empty = revision(&store, "h").unwrap();
        record(&store, "h", "ls").unwrap();
        let one = revision(&store, "h").unwrap();
        assert_ne!(one, empty);
        assert_eq!(revision(&store, "h").unwrap(), one, "没有变化");
        record(&store, "other", "uptime").unwrap();
        assert_eq!(revision(&store, "h").unwrap(), one, "别的主机的不算");
        record(&store, "h", "df -h").unwrap();
        let two = revision(&store, "h").unwrap();
        assert_ne!(two, one);
        // 删掉最新的一条再记一条新的：行 ID 可能复用，最近一次执行的时间不同
        delete(&store, "h", "df -h").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        record(&store, "h", "free -m").unwrap();
        assert_ne!(revision(&store, "h").unwrap(), two);
        clear(&store, Some("h")).unwrap();
        assert_eq!(revision(&store, "h").unwrap(), empty);
    }

    #[test]
    fn concurrent_records_of_the_same_command_leave_one_entry() {
        let store = memory_store();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let store = store.clone();
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        record(&store, "h", "make build").unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(commands(&store, "h"), ["make build"]);
    }

    #[test]
    fn keeps_at_most_the_limit() {
        let store = memory_store();
        for i in 0..MAX_PER_HOST + 5 {
            record(&store, "h", &format!("echo {i}")).unwrap();
        }
        let all = commands(&store, "h");
        assert_eq!(all.len(), MAX_PER_HOST);
        assert_eq!(all[0], format!("echo {}", MAX_PER_HOST + 4));
    }

    #[test]
    fn commands_are_encrypted_and_bound_to_their_host() {
        let store = memory_store();
        record(&store, "h_a", "secret-command").unwrap();
        let blob: Vec<u8> = store
            .database()
            .read(|c| c.query_row("SELECT command FROM command_history", [], |r| r.get(0)))
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&blob).contains("secret-command"),
            "不以明文保存"
        );
        // 挪到另一台主机下解不开，读取时跳过
        store
            .database()
            .write(|tx| {
                tx.execute("UPDATE command_history SET owner = 'h_b'", [])?;
                Ok(())
            })
            .unwrap();
        assert!(commands(&store, "h_b").is_empty());
    }
}
