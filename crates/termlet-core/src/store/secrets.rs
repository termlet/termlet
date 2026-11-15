//! 秘密的归属与读写。

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, params};

use super::State;
use crate::db::Database;
use crate::id::{HostId, KeyId, ProfileId};
use crate::model::{Kind, SecretField};
use crate::secret::SecretString;
use crate::vault::Vault;
use crate::{CoreError, Result};

/// 秘密属于哪个条目，例如 `host/h_…`。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Owner {
    kind: Kind,
    id: String,
}

impl Owner {
    pub fn new(kind: Kind, id: impl Into<String>) -> Self {
        Self {
            kind,
            id: id.into(),
        }
    }

    pub fn host(id: &HostId) -> Self {
        Self::new(Kind::Host, id.as_str())
    }

    pub fn profile(id: &ProfileId) -> Self {
        Self::new(Kind::Profile, id.as_str())
    }

    pub fn key(id: &KeyId) -> Self {
        Self::new(Kind::Key, id.as_str())
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub(super) fn exists(&self, state: &State) -> bool {
        match self.kind {
            Kind::Host => state
                .hosts
                .get(&HostId::from_raw(self.id.clone()))
                .is_some(),
            Kind::Profile => state
                .profiles
                .get(&ProfileId::from_raw(self.id.clone()))
                .is_some(),
            Kind::Key => state.keys.get(&KeyId::from_raw(self.id.clone())).is_some(),
            _ => false,
        }
    }
}

impl std::fmt::Display for Owner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.kind.name(), self.id)
    }
}

/// 加密时的附加数据：绑定这条秘密属于谁。
fn aad(owner: &Owner, field: SecretField) -> Vec<u8> {
    raw_aad(&owner.to_string(), field.name())
}

/// 与 [`aad`] 相同，归属与字段是 `secret` 表中的原文（备份时逐行转加密用）。
pub(crate) fn raw_aad(owner: &str, field: &str) -> Vec<u8> {
    format!("termlet:secret:v1:{owner}:{field}").into_bytes()
}

pub(super) fn load_index(conn: &Connection) -> rusqlite::Result<HashSet<(String, String)>> {
    let mut stmt = conn.prepare("SELECT owner, field FROM secret")?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect()
}

pub(super) fn seal(
    vault: &Vault,
    owner: &Owner,
    field: SecretField,
    value: &SecretString,
) -> Result<Vec<u8>> {
    vault.seal(&aad(owner, field), value.expose().as_bytes())
}

pub(super) fn read(
    db: &Database,
    vault: &Vault,
    owner: &Owner,
    field: SecretField,
) -> Result<Option<SecretString>> {
    let blob: Option<Vec<u8>> = db.read(|conn| {
        conn.query_row(
            "SELECT blob FROM secret WHERE owner = ?1 AND field = ?2",
            params![owner.to_string(), field.name()],
            |row| row.get(0),
        )
        .optional()
    })?;
    let Some(blob) = blob else {
        return Ok(None);
    };
    let plain = vault.open_sealed(&aad(owner, field), &blob)?;
    let text = std::str::from_utf8(&plain).map_err(|_| CoreError::Decrypt)?;
    Ok(Some(SecretString::new(text)))
}

pub(super) fn put(
    conn: &Connection,
    owner: &Owner,
    field: SecretField,
    blob: &[u8],
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO secret (owner, field, blob, updated_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (owner, field) DO UPDATE SET blob = excluded.blob, updated_at = excluded.updated_at",
        params![owner.to_string(), field.name(), blob, crate::time::now_ms()],
    )?;
    Ok(())
}

pub(super) fn delete(conn: &Connection, owner: &Owner, field: SecretField) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM secret WHERE owner = ?1 AND field = ?2",
        params![owner.to_string(), field.name()],
    )?;
    Ok(())
}

pub(super) fn delete_owner(conn: &Connection, owner: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM secret WHERE owner = ?1", [owner])?;
    Ok(())
}
