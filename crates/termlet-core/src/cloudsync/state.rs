//! 本机的同步状态：都存在 `meta` 表中（属于这台电脑：不进完整备份，恢复备份、快照时也不被替换，
//! 所以恢复后的数据与基准不同，按本机的修改上传，不会被同步改回去）。
//!
//! | 键 | 内容 |
//! |---|---|
//! | `sync.config` | [`SyncConfig`]（JSON，不含密码） |
//! | `sync.secrets` | 同步密码与同步位置的凭据（WebDAV 密码、S3 秘密访问密钥、GitHub 令牌），经保险库加密 |
//! | `sync.device` | 本机的设备 ID |
//! | `sync.bases` | 上次同步成功时各条记录的版本与内容摘要（合并的基准） |
//! | `sync.wrote` | 本机写到云端的记录（发现写入丢失用） |
//! | `sync.state` | 上次同步的时间、结果、错误、等待用户决定的事、设备列表 |
//!
//! 保险库重置后同步密码无法再解开：读取时发现解不开就删除全部 `sync.` 键，需要重新设置同步（同步位置中的数据不动，
//! 重新设置后其中的秘密会恢复到本机）。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::merge::Bases;
use super::{Conflict, Device, Pending, SyncConfig, SyncError, SyncReport};
use crate::CoreError;
use crate::db::{self, Database};
use crate::secret::SecretString;
use crate::store::Store;

const META_CONFIG: &str = "sync.config";
const META_SECRETS: &str = "sync.secrets";
const META_DEVICE: &str = "sync.device";
const META_BASES: &str = "sync.bases";
const META_WROTE: &str = "sync.wrote";
const META_STATE: &str = "sync.state";
const ALL_KEYS: [&str; 6] = [
    META_CONFIG,
    META_SECRETS,
    META_DEVICE,
    META_BASES,
    META_WROTE,
    META_STATE,
];

/// 加密同步密码时的附加数据。
const SECRETS_AAD: &[u8] = b"termlet:sync:v1:secrets";

/// 同步密码与同步位置的凭据（WebDAV 密码、S3 的秘密访问密钥、GitHub 的个人访问令牌）。
#[derive(Clone)]
pub(crate) struct Secrets {
    pub password: SecretString,
    pub credential: SecretString,
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secrets(***)")
    }
}

/// 存储的格式。凭据的键沿用最初只有 WebDAV 时的名称 `webdav_password`（新旧版本互相读得懂）。
#[derive(Deserialize)]
struct SecretsWire {
    password: String,
    #[serde(default)]
    webdav_password: String,
}

/// 本机写到云端的记录，用来发现写入丢失：`at` 是上次写入时同步文件中本机的 `lastSync`，`keys` 是各记录最后一次
/// 由本机写到云端的时间，`deleted` 是本机从云端删除、还没有确认在云端的记录（时间都是 Unix 毫秒）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Wrote {
    pub at: i64,
    #[serde(default)]
    pub keys: BTreeMap<String, i64>,
    #[serde(default)]
    pub deleted: BTreeMap<String, Deletion>,
}

/// 本机从云端删除的一条记录：删除时云端的版本与删除的时间。写入丢失后云端仍是这个版本时重新删除，
/// 不是这个版本（其他设备之后修改了它）时保留修改。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Deletion {
    pub v: String,
    pub at: i64,
}

/// 上次同步的结果等（`sync.state`）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct Persisted {
    pub last_sync: Option<i64>,
    pub last_error: Option<SyncError>,
    pub last_report: Option<SyncReport>,
    pub pending: Option<Pending>,
    /// 最近一次有冲突的同步中的冲突（重启后不能再改用另一边）
    pub conflicts: Vec<Conflict>,
    pub devices: Vec<Device>,
}

fn read_json<T: serde::de::DeserializeOwned>(
    db: &Database,
    key: &str,
) -> Result<Option<T>, SyncError> {
    match db.meta(key)? {
        None => Ok(None),
        // 损坏时当作没有
        Some(raw) => Ok(serde_json::from_slice(&raw).ok()),
    }
}

pub(crate) fn load_config(db: &Database) -> Result<Option<SyncConfig>, SyncError> {
    read_json(db, META_CONFIG)
}

/// 读取设置与密码。没有设置时返回 [`SyncError::NotConfigured`]；保险库未解锁时返回 [`SyncError::VaultLocked`]；
/// 保险库重置过（密码解不开）时删除全部同步状态，返回 [`SyncError::NotConfigured`]。
pub(crate) fn load(store: &Store) -> Result<(SyncConfig, Secrets), SyncError> {
    let db = store.database();
    let config = load_config(db)?.ok_or(SyncError::NotConfigured)?;
    let blob = db.meta(META_SECRETS)?.ok_or(SyncError::NotConfigured)?;
    let plain = match store.vault().open_sealed(SECRETS_AAD, &blob) {
        Ok(plain) => plain,
        Err(CoreError::Decrypt) => {
            tracing::warn!("保险库重置过，同步密码已无法解开：停止同步，需要重新设置");
            clear(db)?;
            return Err(SyncError::NotConfigured);
        }
        Err(e) => return Err(e.into()),
    };
    let wire: SecretsWire = serde_json::from_slice(&plain).map_err(|_| SyncError::NotConfigured)?;
    let secrets = Secrets {
        password: SecretString::new(wire.password),
        credential: SecretString::new(wire.webdav_password),
    };
    Ok((config, secrets))
}

pub(crate) fn save_config(db: &Database, config: &SyncConfig) -> Result<(), SyncError> {
    db.set_meta(META_CONFIG, &serde_json::to_vec(config)?)?;
    Ok(())
}

/// 保存新的设置与密码，重置基准与写入记录（下次同步按首次同步处理）、状态（只留同步位置中的设备列表）。
pub(crate) fn save_setup(
    store: &Store,
    config: &SyncConfig,
    secrets: &Secrets,
    persisted: &Persisted,
) -> Result<String, SyncError> {
    let mut plain = Zeroizing::new(String::from("{\"password\":"));
    plain.push_str(&Zeroizing::new(serde_json::to_string(
        secrets.password.expose(),
    )?));
    plain.push_str(",\"webdav_password\":");
    plain.push_str(&Zeroizing::new(serde_json::to_string(
        secrets.credential.expose(),
    )?));
    plain.push('}');
    let blob = store.vault().seal(SECRETS_AAD, plain.as_bytes())?;
    let config = serde_json::to_vec(config)?;
    let state = serde_json::to_vec(persisted)?;
    let device = device_id(store.database())?;
    store.database().write(|tx| {
        db::set_meta(tx, META_CONFIG, &config)?;
        db::set_meta(tx, META_SECRETS, &blob)?;
        db::set_meta(tx, META_STATE, &state)?;
        db::delete_meta(tx, META_BASES)?;
        db::delete_meta(tx, META_WROTE)?;
        Ok(())
    })?;
    Ok(device)
}

/// 删除全部同步状态（停止同步、保险库重置后）。同步位置中的文件不动。
pub(crate) fn clear(db: &Database) -> Result<(), SyncError> {
    db.write(|tx| {
        for key in ALL_KEYS {
            db::delete_meta(tx, key)?;
        }
        Ok(())
    })?;
    Ok(())
}

/// 本机的设备 ID（首次使用时生成）。
pub(crate) fn device_id(db: &Database) -> Result<String, SyncError> {
    if let Some(raw) = db.meta(META_DEVICE)?
        && !raw.is_empty()
    {
        return Ok(String::from_utf8_lossy(&raw).into_owned());
    }
    let id = crate::id::random_token(8);
    db.set_meta(META_DEVICE, id.as_bytes())?;
    Ok(id)
}

pub(crate) fn existing_device_id(db: &Database) -> Option<String> {
    db.meta(META_DEVICE)
        .ok()
        .flatten()
        .filter(|raw| !raw.is_empty())
        .map(|raw| String::from_utf8_lossy(&raw).into_owned())
}

pub(crate) fn load_bases(db: &Database) -> Result<Bases, SyncError> {
    Ok(read_json(db, META_BASES)?.unwrap_or_default())
}

/// 读取本机写到云端的记录；还没有写过、重新设置过或损坏时 `at` 为 0（不检查写入丢失）。
pub(crate) fn load_wrote(db: &Database) -> Result<Wrote, SyncError> {
    Ok(read_json(db, META_WROTE)?.unwrap_or_default())
}

/// 保存同步的基准；`wrote` 不为空时一并保存（本机这次写到了云端）。
pub(crate) fn save_bases(
    db: &Database,
    bases: &Bases,
    wrote: Option<&Wrote>,
) -> Result<(), SyncError> {
    let bases = serde_json::to_vec(bases)?;
    let wrote = wrote.map(serde_json::to_vec).transpose()?;
    db.write(|tx| {
        db::set_meta(tx, META_BASES, &bases)?;
        if let Some(wrote) = &wrote {
            db::set_meta(tx, META_WROTE, wrote)?;
        }
        Ok(())
    })?;
    Ok(())
}

pub(crate) fn load_persisted(db: &Database) -> Persisted {
    read_json(db, META_STATE).ok().flatten().unwrap_or_default()
}

pub(crate) fn save_persisted(db: &Database, persisted: &Persisted) -> Result<(), SyncError> {
    db.set_meta(META_STATE, &serde_json::to_vec(persisted)?)?;
    Ok(())
}
