//! 主机指纹：首次连接时请用户确认并记下，之后每次连接核对。
//!
//! 一个地址可以记多种算法的密钥。连接时把已记录的那种算法排在最前面请求（与 OpenSSH 相同），
//! 服务器新增了别的类型的密钥不会被误报为指纹变化；只有同一种算法的密钥变了才拒绝连接。

use rusqlite::{OptionalExtension, params};
use russh::keys::{HashAlg, PublicKey};

use crate::Result;
use crate::db::Database;

/// 核对的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostKeyStatus {
    /// 与记录一致
    Known,
    /// 没有这种算法的记录（首次连接，或服务器换了类型）
    Unknown,
    /// 同一种算法的密钥与记录不同
    Changed { known_fingerprint: String },
}

/// 指纹的显示格式：`SHA256:…`。
pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// 一条记录（设置中的“主机指纹”列表）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownHostEntry {
    pub host: String,
    pub port: u16,
    pub algorithm: String,
    /// `SHA256:…`；公钥解析不了时为 `?`
    pub fingerprint: String,
    /// 记录的时间（Unix 毫秒）
    pub added_at: i64,
}

/// 已记录的主机指纹（数据库中的 `known_host` 表）。
pub struct KnownHosts<'a> {
    db: &'a Database,
}

impl<'a> KnownHosts<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    pub fn check(&self, host: &str, port: u16, key: &PublicKey) -> Result<HostKeyStatus> {
        let algorithm = key.algorithm().as_str().to_string();
        let stored: Option<String> = self.db.read(|conn| {
            conn.query_row(
                "SELECT key FROM known_host WHERE host = ?1 AND port = ?2 AND algorithm = ?3",
                params![host.to_ascii_lowercase(), port, algorithm],
                |row| row.get(0),
            )
            .optional()
        })?;
        let Some(stored) = stored else {
            return Ok(HostKeyStatus::Unknown);
        };
        let offered = key.to_openssh().unwrap_or_default();
        if same_key(&stored, &offered) {
            return Ok(HostKeyStatus::Known);
        }
        let known_fingerprint = PublicKey::from_openssh(&stored)
            .map(|k| fingerprint(&k))
            .unwrap_or_else(|_| "?".into());
        Ok(HostKeyStatus::Changed { known_fingerprint })
    }

    /// 记下这台主机这种算法的密钥；已有记录时不替换（两个连接同时首次确认，先记下的为准，
    /// 后一个连接下次核对时如果不一致会报指纹变化）。
    pub fn remember(&self, host: &str, port: u16, key: &PublicKey) -> Result<()> {
        let algorithm = key.algorithm().as_str().to_string();
        let openssh = key
            .to_openssh()
            .map_err(|e| crate::CoreError::DataDir(format!("编码公钥失败：{e}")))?;
        tracing::info!(host, port, algorithm = %algorithm, fingerprint = %fingerprint(key), "记下主机指纹");
        self.db.write(|tx| {
            tx.execute(
                "INSERT INTO known_host (host, port, algorithm, key, added_at) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (host, port, algorithm) DO NOTHING",
                params![host.to_ascii_lowercase(), port, algorithm, openssh, crate::time::now_ms()],
            )?;
            Ok(())
        })
    }

    /// 删除这台主机的全部记录（指纹变化后，向管理员核对过再"删除旧记录并重新连接"）。
    pub fn forget(&self, host: &str, port: u16) -> Result<()> {
        tracing::info!(host, port, "删除这台主机的全部指纹记录");
        self.db.write(|tx| {
            tx.execute(
                "DELETE FROM known_host WHERE host = ?1 AND port = ?2",
                params![host.to_ascii_lowercase(), port],
            )?;
            Ok(())
        })
    }

    /// 全部记录，按地址、端口、算法排列。
    pub fn list(&self) -> Result<Vec<KnownHostEntry>> {
        let rows: Vec<(String, u16, String, String, i64)> = self.db.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT host, port, algorithm, key, added_at FROM known_host ORDER BY host, port, algorithm",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
            })?;
            rows.collect()
        })?;
        Ok(rows
            .into_iter()
            .map(|(host, port, algorithm, key, added_at)| KnownHostEntry {
                fingerprint: PublicKey::from_openssh(&key)
                    .map(|k| fingerprint(&k))
                    .unwrap_or_else(|_| "?".into()),
                host,
                port,
                algorithm,
                added_at,
            })
            .collect())
    }

    /// 删除一条记录（一种算法的密钥）。
    pub fn forget_key(&self, host: &str, port: u16, algorithm: &str) -> Result<()> {
        tracing::info!(host, port, algorithm, "删除一条主机指纹记录");
        self.db.write(|tx| {
            tx.execute(
                "DELETE FROM known_host WHERE host = ?1 AND port = ?2 AND algorithm = ?3",
                params![host.to_ascii_lowercase(), port, algorithm],
            )?;
            Ok(())
        })
    }

    /// 这台主机已记录的算法（按记录时间排列），连接时优先请求它们。
    pub fn algorithms(&self, host: &str, port: u16) -> Result<Vec<String>> {
        self.db.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT algorithm FROM known_host WHERE host = ?1 AND port = ?2 ORDER BY added_at",
            )?;
            let rows =
                stmt.query_map(params![host.to_ascii_lowercase(), port], |row| row.get(0))?;
            rows.collect()
        })
    }
}

/// 比较两个 OpenSSH 格式的公钥（忽略注释）。
fn same_key(a: &str, b: &str) -> bool {
    let core = |s: &str| {
        let mut parts = s.split_whitespace();
        (
            parts.next().map(str::to_string),
            parts.next().map(str::to_string),
        )
    };
    core(a) == core(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
    const KEY_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGb7ZbvaQbEFhE0Iqy+bNe0Vj3/5iKG5DC8W0l7OT+jq";

    #[test]
    fn unknown_then_known_then_changed() {
        let db = Database::open_in_memory().unwrap();
        let known = KnownHosts::new(&db);
        let a = PublicKey::from_openssh(KEY_A).unwrap();
        let b = PublicKey::from_openssh(KEY_B).unwrap();
        assert_eq!(known.check("Web", 22, &a).unwrap(), HostKeyStatus::Unknown);
        known.remember("Web", 22, &a).unwrap();
        assert_eq!(known.check("web", 22, &a).unwrap(), HostKeyStatus::Known);
        assert_eq!(
            known.check("web", 2222, &a).unwrap(),
            HostKeyStatus::Unknown
        );
        match known.check("web", 22, &b).unwrap() {
            HostKeyStatus::Changed { known_fingerprint } => {
                assert_eq!(known_fingerprint, fingerprint(&a));
            }
            other => panic!("应为指纹变化：{other:?}"),
        }
        assert_eq!(known.algorithms("web", 22).unwrap(), vec!["ssh-ed25519"]);
        // 已有记录时不替换
        known.remember("web", 22, &b).unwrap();
        assert_eq!(known.check("web", 22, &a).unwrap(), HostKeyStatus::Known);
        known.forget("web", 22).unwrap();
        assert_eq!(known.check("web", 22, &b).unwrap(), HostKeyStatus::Unknown);
    }

    #[test]
    fn list_and_forget_one_key() {
        let db = Database::open_in_memory().unwrap();
        let known = KnownHosts::new(&db);
        let a = PublicKey::from_openssh(KEY_A).unwrap();
        known.remember("Web", 22, &a).unwrap();
        known.remember("db", 2222, &a).unwrap();
        let list = known.list().unwrap();
        assert_eq!(
            list.iter()
                .map(|e| (e.host.as_str(), e.port))
                .collect::<Vec<_>>(),
            [("db", 2222), ("web", 22)]
        );
        assert_eq!(list[0].fingerprint, fingerprint(&a));
        assert_eq!(list[0].algorithm, "ssh-ed25519");
        known.forget_key("WEB", 22, "ssh-ed25519").unwrap();
        assert_eq!(known.list().unwrap().len(), 1);
    }
}
