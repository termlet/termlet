//! 保险库：主密钥的保管与秘密的加解密。
//!
//! - **主密钥**是 32 字节随机数。默认放在系统钥匙串（服务名 `termlet`，账户名含数据目录 ID，两个数据目录互不覆盖）。
//!   钥匙串不可用（Linux 没有 Secret Service）时改用主密码：主密钥经主密码派生的密钥（Argon2id）加密后存在 `meta` 中，
//!   改主密码只需重新加密主密钥。便携版第一次使用时由用户选择用主密码（数据带到别的电脑上也能解开）还是
//!   这台电脑的钥匙串（[`VaultStatus::ChooseKeyStore`]）。
//! - **记住主密码**：便携版用主密码时，可以在这台电脑上记住（[`Remember`]）：主密钥另存一份在这台电脑的钥匙串中
//!   （账户名 `remembered-master-key:<数据目录 ID>`，与钥匙串模式的条目分开），启动时直接用它解锁；数据带到别的电脑上
//!   仍要主密码。改主密码时主密钥不变，记住的照常有效；对不上（别处重置过保险库等）时删掉它、照常询问主密码。
//! - **校验值**：`meta` 中存一份用主密钥加密的固定内容。启动时解不开，说明主密钥不对（数据目录被拷到了另一台电脑或另一个账户），
//!   直接提示重置保险库，而不是等到连接时才失败。
//! - **加密**：AES-256-GCM，每次随机 nonce；密文格式为 `版本(1) || nonce(12) || 密文与标签`。附加数据由调用方给出，
//!   绑定这条秘密属于谁（例如 `termlet:secret:v1:host/h_…:password`），两条秘密互换位置会解密失败。
//!
//! 环境变量 `TERMLET_VAULT_PROVIDER=file` 把主密钥放在 `run/vault-key.bin`、记住的主密钥放在
//! `run/vault-remembered-key.bin`（只用于自动化测试，不安全）。

use std::sync::{Mutex, RwLock};

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::db::{self, Database};
use crate::paths::DataDir;
use crate::secret::SecretString;
use crate::{CoreError, Result};

pub const KEY_LEN: usize = 32;
/// 主密码最少的字符数（设置、修改、重置时设的新主密码都按它检查）。
pub const MIN_PASSWORD_LEN: usize = 8;
const NONCE_LEN: usize = 12;
const BLOB_VERSION: u8 = 1;

type MasterKey = Zeroizing<[u8; KEY_LEN]>;

const META_DATA_DIR_ID: &str = "data_dir.id";
const META_MODE: &str = "vault.mode";
const META_CHECK: &str = "vault.check";
const META_WRAPPED: &str = "vault.wrapped";
const META_KDF: &str = "vault.kdf";

const CHECK_PLAINTEXT: &[u8] = b"termlet vault check v1";
const CHECK_AAD: &[u8] = b"termlet:vault-check:v1";
const WRAP_AAD: &[u8] = b"termlet:vault-wrapped-key:v1";

/// 保险库的状态。只有 [`VaultStatus::Unlocked`] 时能读写秘密。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VaultStatus {
    Unlocked,
    /// 主密码模式：需要输入主密码。
    NeedsPassword,
    /// 第一次使用但系统钥匙串不可用：需要设置主密码。
    NeedsNewPassword {
        reason: String,
    },
    /// 便携版第一次使用：选择设置主密码（数据带到别的电脑上也能解开），还是把主密钥放进这台电脑的钥匙串
    /// （不用输主密码，但带到别的电脑上解不开）。
    ChooseKeyStore,
    /// 钥匙串中没有主密钥：数据目录来自另一台电脑或另一个账户。只能重置（或回到原来的环境）。
    KeyMissing,
    /// 钥匙串中的主密钥与数据不符。只能重置（或回到原来的环境）。
    KeyMismatch,
    /// 读取钥匙串出错（例如被拒绝访问），可以重试。
    KeyringError(String),
}

/// 便携版能不能、有没有在这台电脑上记住主密钥（主密码模式下，见 [`Vault::remember`]）。只在这台电脑的钥匙串中，
/// 不进数据库，因此不同步、不进备份。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Remember {
    /// 不能记住：安装版、不是主密码模式，或这台电脑的钥匙串不可用
    Unavailable,
    /// 可以记住，还没有记住
    Available,
    /// 记住了：启动时直接用它解锁
    Remembered,
}

/// 主密钥放在哪里。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Mode {
    Keyring,
    Password,
}

/// 主密钥的保管处（系统钥匙串，或测试用的内存 / 文件）。
pub trait KeyStore: Send + Sync {
    fn load(&self) -> Result<Option<MasterKey>>;
    fn store(&self, key: &[u8; KEY_LEN]) -> Result<()>;
    fn delete(&self) -> Result<()>;
}

/// 共用的保管处（测试中多次打开同一个数据库、打开后还要查看其中的主密钥时）。
impl<T: KeyStore + ?Sized> KeyStore for std::sync::Arc<T> {
    fn load(&self) -> Result<Option<MasterKey>> {
        (**self).load()
    }

    fn store(&self, key: &[u8; KEY_LEN]) -> Result<()> {
        (**self).store(key)
    }

    fn delete(&self) -> Result<()> {
        (**self).delete()
    }
}

/// 系统钥匙串（Windows 凭据管理器、macOS 钥匙串、Linux Secret Service）。
pub struct KeyringStore {
    service: String,
    account: String,
}

impl KeyringStore {
    pub fn new(data_dir_id: &str) -> Self {
        Self {
            service: "termlet".into(),
            account: format!("master-key:{data_dir_id}"),
        }
    }

    /// 便携版在这台电脑上记住的主密钥（[`Remember`]）。与 [`KeyringStore::new`] 的条目分开：删掉对不上的记住的
    /// 主密钥时，碰不到钥匙串模式的主密钥（同一份数据的副本可能在用它）。
    pub fn remembered(data_dir_id: &str) -> Self {
        Self {
            service: "termlet".into(),
            account: format!("remembered-master-key:{data_dir_id}"),
        }
    }

    fn entry(&self) -> Result<keyring::Entry> {
        keyring::Entry::new(&self.service, &self.account)
            .map_err(|e| CoreError::Keyring(e.to_string()))
    }
}

impl KeyStore for KeyringStore {
    fn load(&self) -> Result<Option<MasterKey>> {
        match self.entry()?.get_secret() {
            Ok(bytes) => {
                let bytes = Zeroizing::new(bytes);
                Ok(to_key(&bytes))
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(CoreError::Keyring(e.to_string())),
        }
    }

    fn store(&self, key: &[u8; KEY_LEN]) -> Result<()> {
        self.entry()?
            .set_secret(key)
            .map_err(|e| CoreError::Keyring(e.to_string()))
    }

    fn delete(&self) -> Result<()> {
        match self.entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(CoreError::Keyring(e.to_string())),
        }
    }
}

/// 进程内存中的主密钥（单元测试用）。
#[derive(Default)]
pub struct MemoryKeyStore {
    key: Mutex<Option<[u8; KEY_LEN]>>,
    fail: bool,
}

impl MemoryKeyStore {
    /// 模拟不可用的钥匙串。
    pub fn unavailable() -> Self {
        Self {
            key: Mutex::new(None),
            fail: true,
        }
    }
}

impl KeyStore for MemoryKeyStore {
    fn load(&self) -> Result<Option<MasterKey>> {
        if self.fail {
            return Err(CoreError::Keyring("测试：钥匙串不可用".into()));
        }
        Ok(self.key.lock().unwrap().map(Zeroizing::new))
    }

    fn store(&self, key: &[u8; KEY_LEN]) -> Result<()> {
        if self.fail {
            return Err(CoreError::Keyring("测试：钥匙串不可用".into()));
        }
        *self.key.lock().unwrap() = Some(*key);
        Ok(())
    }

    fn delete(&self) -> Result<()> {
        *self.key.lock().unwrap() = None;
        Ok(())
    }
}

/// 明文存在文件中的主密钥（只用于自动化测试：多个进程共用同一个数据目录时）。
pub struct FileKeyStore {
    path: std::path::PathBuf,
}

impl KeyStore for FileKeyStore {
    fn load(&self) -> Result<Option<MasterKey>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Ok(to_key(&Zeroizing::new(bytes))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn store(&self, key: &[u8; KEY_LEN]) -> Result<()> {
        Ok(crate::platform::write_private_file(&self.path, key)?)
    }

    fn delete(&self) -> Result<()> {
        match std::fs::remove_file(&self.path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

/// 按环境变量 `TERMLET_VAULT_PROVIDER` 选择主密钥的保管处，默认是系统钥匙串。
pub fn key_store_for(data_dir: &DataDir, data_dir_id: &str) -> Box<dyn KeyStore> {
    match std::env::var("TERMLET_VAULT_PROVIDER").as_deref() {
        Ok("file") => {
            tracing::warn!(
                "TERMLET_VAULT_PROVIDER=file：主密钥以明文保存在数据目录中，只能用于测试"
            );
            Box::new(FileKeyStore {
                path: data_dir.run_dir().join("vault-key.bin"),
            })
        }
        _ => Box::new(KeyringStore::new(data_dir_id)),
    }
}

/// 便携版在这台电脑上记住主密钥的地方（[`Remember`]），与 [`key_store_for`] 一样按 `TERMLET_VAULT_PROVIDER` 选择。
pub fn remembered_key_store_for(data_dir: &DataDir, data_dir_id: &str) -> Box<dyn KeyStore> {
    match std::env::var("TERMLET_VAULT_PROVIDER").as_deref() {
        Ok("file") => Box::new(FileKeyStore {
            path: data_dir.run_dir().join("vault-remembered-key.bin"),
        }),
        _ => Box::new(KeyringStore::remembered(data_dir_id)),
    }
}

/// 数据目录的 ID：建库时随机生成，用来区分不同数据目录在钥匙串中的条目。
pub fn data_dir_id(db: &Database) -> Result<String> {
    if let Some(id) = db.meta(META_DATA_DIR_ID)? {
        return Ok(String::from_utf8_lossy(&id).into_owned());
    }
    let id = crate::id::random_token(10);
    db.set_meta(META_DATA_DIR_ID, id.as_bytes())?;
    Ok(id)
}

/// Argon2id 的参数与盐。
#[derive(Clone, Debug, Serialize, Deserialize)]
struct KdfParams {
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
    salt: Vec<u8>,
}

impl KdfParams {
    /// OWASP 推荐的 Argon2id 配置之一（64MB 内存、3 次迭代）；测试中用小参数。
    fn generate() -> Self {
        let mut salt = vec![0u8; 16];
        getrandom::fill(&mut salt).expect("系统随机数不可用");
        if cfg!(test) {
            Self {
                memory_kib: 64,
                iterations: 1,
                parallelism: 1,
                salt,
            }
        } else {
            Self {
                memory_kib: 64 * 1024,
                iterations: 3,
                parallelism: 4,
                salt,
            }
        }
    }

    fn derive(&self, password: &SecretString) -> Result<MasterKey> {
        derive_key(
            password.expose().as_bytes(),
            &self.salt,
            self.memory_kib,
            self.iterations,
            self.parallelism,
        )
    }
}

/// 用 Argon2id 从密码派生 32 字节的密钥（主密码、备份密码、同步密码共用）。
pub fn derive_key(
    password: &[u8],
    salt: &[u8],
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
) -> Result<MasterKey> {
    let params = argon2::Params::new(memory_kib, iterations, parallelism, Some(KEY_LEN))
        .map_err(|e| CoreError::DataDir(format!("Argon2 参数不合法：{e}")))?;
    let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut out = Zeroizing::new([0u8; KEY_LEN]);
    argon
        .hash_password_into(password, salt, &mut out[..])
        .map_err(|e| CoreError::DataDir(format!("Argon2 派生失败：{e}")))?;
    Ok(out)
}

/// 用 `key` 加密：`版本 || nonce || 密文`。
pub fn seal_with(key: &[u8; KEY_LEN], aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).expect("系统随机数不可用");
    seal_with_nonce(key, &nonce, aad, plaintext)
}

/// [`seal_with`]，nonce 由调用方给出（固定向量测试用；同一把密钥不能重复使用 nonce）。
pub(crate) fn seal_with_nonce(
    key: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    plaintext: &[u8],
) -> Vec<u8> {
    let cipher = Aes256Gcm::new(key.into());
    let ciphertext = cipher
        .encrypt(
            nonce.into(),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .expect("AES-GCM 加密不会失败");
    let mut blob = Vec::with_capacity(1 + NONCE_LEN + ciphertext.len());
    blob.push(BLOB_VERSION);
    blob.extend_from_slice(nonce);
    blob.extend_from_slice(&ciphertext);
    blob
}

/// [`seal_with`] 的逆操作。数据被篡改、附加数据不符或密钥不对时返回 [`CoreError::Decrypt`]。
pub fn open_with(key: &[u8; KEY_LEN], aad: &[u8], blob: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    let Some((&BLOB_VERSION, rest)) = blob.split_first() else {
        return Err(CoreError::Decrypt);
    };
    let (nonce, msg) = rest
        .split_first_chunk::<NONCE_LEN>()
        .ok_or(CoreError::Decrypt)?;
    let cipher = Aes256Gcm::new(key.into());
    cipher
        .decrypt(nonce.into(), Payload { msg, aad })
        .map(Zeroizing::new)
        .map_err(|_| CoreError::Decrypt)
}

fn to_key(bytes: &[u8]) -> Option<MasterKey> {
    let array: [u8; KEY_LEN] = bytes.try_into().ok()?;
    Some(Zeroizing::new(array))
}

fn random_key() -> MasterKey {
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    getrandom::fill(&mut key[..]).expect("系统随机数不可用");
    key
}

pub struct Vault {
    key: RwLock<Option<MasterKey>>,
    status: RwLock<VaultStatus>,
    store: Box<dyn KeyStore>,
    /// 便携版在这台电脑上记住主密钥的地方（安装版没有）
    remembered: Option<Box<dyn KeyStore>>,
    remember: RwLock<Remember>,
}

impl Vault {
    /// 打开保险库：第一次使用时生成主密钥，之后从钥匙串取出并核对校验值。
    pub fn open(db: &Database, store: Box<dyn KeyStore>) -> Result<Self> {
        Self::open_with(db, store, None)
    }

    /// 打开便携版的保险库：第一次使用时不直接放进钥匙串（钥匙串属于这台电脑，数据带到别的电脑上就解不开），
    /// 由用户选择主密码或这台电脑的钥匙串（[`VaultStatus::ChooseKeyStore`]）。之后与 [`Vault::open`] 相同，
    /// 只是用主密码时先找 `remembered` 中这台电脑记住的主密钥（[`Remember`]）。
    pub fn open_portable(
        db: &Database,
        store: Box<dyn KeyStore>,
        remembered: Box<dyn KeyStore>,
    ) -> Result<Self> {
        Self::open_with(db, store, Some(remembered))
    }

    fn open_with(
        db: &Database,
        store: Box<dyn KeyStore>,
        remembered: Option<Box<dyn KeyStore>>,
    ) -> Result<Self> {
        let portable = remembered.is_some();
        let vault = Self {
            key: RwLock::new(None),
            status: RwLock::new(VaultStatus::KeyMissing),
            store,
            remembered,
            remember: RwLock::new(Remember::Unavailable),
        };
        let check = db.meta(META_CHECK)?;
        let mode = Self::mode(db)?;
        let status = match (check, mode) {
            (None, _) if portable => VaultStatus::ChooseKeyStore,
            (None, _) => match vault.initialize_with_keyring(db) {
                Ok(()) => VaultStatus::Unlocked,
                Err(CoreError::Keyring(reason)) => VaultStatus::NeedsNewPassword { reason },
                Err(e) => return Err(e),
            },
            (Some(check), Some(Mode::Password)) => vault.open_remembered(&check),
            (Some(check), _) => match vault.store.load() {
                Ok(Some(key)) if open_with(&key, CHECK_AAD, &check).is_ok() => {
                    *vault.key.write().unwrap() = Some(key);
                    VaultStatus::Unlocked
                }
                Ok(Some(_)) => VaultStatus::KeyMismatch,
                Ok(None) => VaultStatus::KeyMissing,
                Err(e) => VaultStatus::KeyringError(e.to_string()),
            },
        };
        *vault.status.write().unwrap() = status;
        Ok(vault)
    }

    /// 主密码模式下取出这台电脑记住的主密钥（便携版）：对得上时直接解锁；对不上（别处重置过保险库等）时删掉它，
    /// 照常询问主密码；读不了钥匙串时不能记住。安装版照常询问主密码。
    fn open_remembered(&self, check: &[u8]) -> VaultStatus {
        let Some(remembered) = &self.remembered else {
            return VaultStatus::NeedsPassword;
        };
        let (status, remember) = match remembered.load() {
            Ok(Some(key)) if open_with(&key, CHECK_AAD, check).is_ok() => {
                *self.key.write().unwrap() = Some(key);
                tracing::info!("用这台电脑记住的主密钥解锁了保险库");
                (VaultStatus::Unlocked, Remember::Remembered)
            }
            Ok(Some(_)) => match remembered.delete() {
                Ok(()) => {
                    tracing::info!("这台电脑记住的主密钥与数据对不上，已删除，改为询问主密码");
                    (VaultStatus::NeedsPassword, Remember::Available)
                }
                Err(e) => {
                    tracing::warn!("这台电脑记住的主密钥与数据对不上，删除失败：{e}");
                    (VaultStatus::NeedsPassword, Remember::Unavailable)
                }
            },
            Ok(None) => (VaultStatus::NeedsPassword, Remember::Available),
            Err(e) => {
                tracing::info!("读不了这台电脑的钥匙串，不能记住主密码：{e}");
                (VaultStatus::NeedsPassword, Remember::Unavailable)
            }
        };
        *self.remember.write().unwrap() = remember;
        status
    }

    fn mode(db: &Database) -> Result<Option<Mode>> {
        Ok(db
            .meta(META_MODE)?
            .and_then(|v| serde_json::from_slice(&v).ok()))
    }

    fn initialize_with_keyring(&self, db: &Database) -> Result<()> {
        let key = random_key();
        self.store.store(&key)?;
        db.write(|tx| {
            db::set_meta(tx, META_MODE, &serde_json::to_vec(&Mode::Keyring)?)?;
            db::set_meta(tx, META_CHECK, &seal_with(&key, CHECK_AAD, CHECK_PLAINTEXT))?;
            db::delete_meta(tx, META_WRAPPED)?;
            db::delete_meta(tx, META_KDF)?;
            Ok(())
        })?;
        *self.key.write().unwrap() = Some(key);
        Ok(())
    }

    fn initialize_with_password(&self, db: &Database, password: &SecretString) -> Result<()> {
        let key = random_key();
        let params = KdfParams::generate();
        let kek = params.derive(password)?;
        db.write(|tx| {
            db::set_meta(tx, META_MODE, &serde_json::to_vec(&Mode::Password)?)?;
            db::set_meta(tx, META_KDF, &serde_json::to_vec(&params)?)?;
            db::set_meta(tx, META_WRAPPED, &seal_with(&kek, WRAP_AAD, &key[..]))?;
            db::set_meta(tx, META_CHECK, &seal_with(&key, CHECK_AAD, CHECK_PLAINTEXT))?;
            Ok(())
        })?;
        *self.key.write().unwrap() = Some(key);
        Ok(())
    }

    pub fn status(&self) -> VaultStatus {
        self.status.read().unwrap().clone()
    }

    pub fn is_unlocked(&self) -> bool {
        self.status() == VaultStatus::Unlocked
    }

    /// 重试读取钥匙串（状态为 [`VaultStatus::KeyringError`] 时）。
    pub fn retry(&self, db: &Database) -> Result<VaultStatus> {
        let check = db.meta(META_CHECK)?.ok_or(CoreError::VaultLocked)?;
        let status = match self.store.load() {
            Ok(Some(key)) if open_with(&key, CHECK_AAD, &check).is_ok() => {
                *self.key.write().unwrap() = Some(key);
                VaultStatus::Unlocked
            }
            Ok(Some(_)) => VaultStatus::KeyMismatch,
            Ok(None) => VaultStatus::KeyMissing,
            Err(e) => VaultStatus::KeyringError(e.to_string()),
        };
        *self.status.write().unwrap() = status.clone();
        Ok(status)
    }

    /// 主密码模式下解锁。
    pub fn unlock(&self, db: &Database, password: &SecretString) -> Result<()> {
        let params: KdfParams =
            serde_json::from_slice(&db.meta(META_KDF)?.ok_or(CoreError::VaultLocked)?)?;
        let wrapped = db.meta(META_WRAPPED)?.ok_or(CoreError::VaultLocked)?;
        let kek = params.derive(password)?;
        let key = open_with(&kek, WRAP_AAD, &wrapped).map_err(|_| CoreError::WrongPassword)?;
        let key = to_key(&key).ok_or(CoreError::Decrypt)?;
        let check = db.meta(META_CHECK)?.ok_or(CoreError::VaultLocked)?;
        open_with(&key, CHECK_AAD, &check).map_err(|_| CoreError::VaultKeyMismatch)?;
        *self.key.write().unwrap() = Some(key);
        *self.status.write().unwrap() = VaultStatus::Unlocked;
        crate::usage::mark(crate::usage::Feature::MasterPassword);
        Ok(())
    }

    /// 钥匙串不可用、或便携版选择用主密码时，第一次使用设置主密码。
    pub fn set_new_password(&self, db: &Database, password: &SecretString) -> Result<()> {
        if !matches!(
            self.status(),
            VaultStatus::NeedsNewPassword { .. } | VaultStatus::ChooseKeyStore
        ) {
            return Err(CoreError::VaultLocked);
        }
        self.initialize_with_password(db, password)?;
        *self.status.write().unwrap() = VaultStatus::Unlocked;
        crate::usage::mark(crate::usage::Feature::MasterPassword);
        Ok(())
    }

    /// 便携版第一次使用时选择把主密钥放进这台电脑的钥匙串。钥匙串不可用时转为设置主密码
    /// （[`VaultStatus::NeedsNewPassword`]），返回之后的状态。
    pub fn choose_keyring(&self, db: &Database) -> Result<VaultStatus> {
        if self.status() != VaultStatus::ChooseKeyStore {
            return Err(CoreError::VaultLocked);
        }
        let status = match self.initialize_with_keyring(db) {
            Ok(()) => VaultStatus::Unlocked,
            Err(CoreError::Keyring(reason)) => VaultStatus::NeedsNewPassword { reason },
            Err(e) => return Err(e),
        };
        *self.status.write().unwrap() = status.clone();
        Ok(status)
    }

    /// 是否是主密码模式（主密钥由主密码保护，不在系统钥匙串中）。
    pub fn uses_password(db: &Database) -> Result<bool> {
        Ok(Self::mode(db)? == Some(Mode::Password))
    }

    /// 便携版能不能、有没有在这台电脑上记住主密钥。
    pub fn remember_state(&self) -> Remember {
        *self.remember.read().unwrap()
    }

    /// 在这台电脑上记住主密钥（便携版主密码模式下解锁之后，[`Remember::Available`] 时）：以后在这台电脑上启动时
    /// 直接用它解锁。存的是主密钥本身（与钥匙串模式存法相同），不是主密码。
    pub fn remember(&self) -> Result<()> {
        let Some(remembered) = &self.remembered else {
            return Err(CoreError::VaultLocked);
        };
        if self.remember_state() == Remember::Unavailable {
            return Err(CoreError::VaultLocked);
        }
        let key = self
            .key
            .read()
            .unwrap()
            .clone()
            .ok_or(CoreError::VaultLocked)?;
        remembered.store(&key)?;
        *self.remember.write().unwrap() = Remember::Remembered;
        Ok(())
    }

    /// 忘记这台电脑记住的主密钥：这次照常可用，下次启动时重新询问主密码。
    pub fn forget(&self) -> Result<()> {
        let Some(remembered) = &self.remembered else {
            return Ok(());
        };
        remembered.delete()?;
        let mut remember = self.remember.write().unwrap();
        if *remember == Remember::Remembered {
            *remember = Remember::Available;
        }
        Ok(())
    }

    /// 主密码模式下修改主密码（主密钥不变，秘密不用重新加密，这台电脑记住的照常有效）。当前主密码不对时返回
    /// [`CoreError::WrongPassword`]，新主密码少于 [`MIN_PASSWORD_LEN`] 个字符时返回 `password` 字段的
    /// `too_short`。
    pub fn change_password(
        &self,
        db: &Database,
        old: &SecretString,
        new: &SecretString,
    ) -> Result<()> {
        if Self::mode(db)? != Some(Mode::Password) {
            return Err(CoreError::VaultLocked);
        }
        if new.expose().chars().count() < MIN_PASSWORD_LEN {
            return Err(crate::Invalid::new("password", "too_short").into());
        }
        self.unlock(db, old)?;
        let key = self
            .key
            .read()
            .unwrap()
            .clone()
            .ok_or(CoreError::VaultLocked)?;
        let params = KdfParams::generate();
        let kek = params.derive(new)?;
        db.write(|tx| {
            db::set_meta(tx, META_KDF, &serde_json::to_vec(&params)?)?;
            db::set_meta(tx, META_WRAPPED, &seal_with(&kek, WRAP_AAD, &key[..]))?;
            Ok(())
        })
    }

    /// 重置保险库：生成新的主密钥（`password` 为 `None` 时放进钥匙串，否则改用主密码），
    /// 并在同一个事务中执行 `clear`（由存储层删除全部解不开的秘密）。这台电脑记住的旧主密钥一并删掉。
    pub fn reset(
        &self,
        db: &Database,
        password: Option<&SecretString>,
        clear: impl FnOnce(&rusqlite::Transaction) -> Result<()>,
    ) -> Result<()> {
        let key = random_key();
        let (mode, wrapped) = match password {
            None => {
                self.store.store(&key)?;
                (Mode::Keyring, None)
            }
            Some(password) => {
                let params = KdfParams::generate();
                let kek = params.derive(password)?;
                (
                    Mode::Password,
                    Some((params, seal_with(&kek, WRAP_AAD, &key[..]))),
                )
            }
        };
        db.write(|tx| {
            clear(tx)?;
            db::set_meta(tx, META_MODE, &serde_json::to_vec(&mode)?)?;
            db::set_meta(tx, META_CHECK, &seal_with(&key, CHECK_AAD, CHECK_PLAINTEXT))?;
            match &wrapped {
                Some((params, wrapped)) => {
                    db::set_meta(tx, META_KDF, &serde_json::to_vec(params)?)?;
                    db::set_meta(tx, META_WRAPPED, wrapped)?;
                }
                None => {
                    db::delete_meta(tx, META_KDF)?;
                    db::delete_meta(tx, META_WRAPPED)?;
                }
            }
            Ok(())
        })?;
        if mode == Mode::Password {
            let _ = self.store.delete();
        }
        // 记住的是旧的主密钥
        if let Some(remembered) = &self.remembered {
            if let Err(e) = remembered.delete() {
                tracing::warn!("重置保险库时删除这台电脑记住的主密钥失败：{e}");
            }
            let mut remember = self.remember.write().unwrap();
            if mode == Mode::Keyring {
                *remember = Remember::Unavailable;
            } else if *remember == Remember::Remembered {
                *remember = Remember::Available;
            }
        }
        *self.key.write().unwrap() = Some(key);
        *self.status.write().unwrap() = VaultStatus::Unlocked;
        tracing::warn!("保险库已重置，原有的秘密已清空");
        Ok(())
    }

    /// 加密一条秘密。保险库未解锁时返回 [`CoreError::VaultLocked`]。
    pub fn seal(&self, aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
        let key = self.key.read().unwrap();
        let key = key.as_ref().ok_or(CoreError::VaultLocked)?;
        Ok(seal_with(key, aad, plaintext))
    }

    /// 解密一条秘密。
    pub fn open_sealed(&self, aad: &[u8], blob: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        let key = self.key.read().unwrap();
        let key = key.as_ref().ok_or(CoreError::VaultLocked)?;
        open_with(key, aad, blob)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::testkit::SharedKeys;

    #[test]
    fn first_open_creates_key_and_reopen_unlocks() {
        let db = Database::open_in_memory().unwrap();
        let store = SharedKeys::default();
        let vault = Vault::open(&db, Box::new(store.clone())).unwrap();
        assert_eq!(vault.status(), VaultStatus::Unlocked);
        let blob = vault.seal(b"aad", b"secret").unwrap();

        let reopened = Vault::open(&db, Box::new(store)).unwrap();
        assert_eq!(reopened.status(), VaultStatus::Unlocked);
        assert_eq!(&reopened.open_sealed(b"aad", &blob).unwrap()[..], b"secret");
    }

    #[test]
    fn wrong_aad_or_tampering_fails() {
        let db = Database::open_in_memory().unwrap();
        let vault = Vault::open(&db, Box::new(MemoryKeyStore::default())).unwrap();
        let mut blob = vault.seal(b"host/a:password", b"pw").unwrap();
        assert!(matches!(
            vault.open_sealed(b"host/b:password", &blob),
            Err(CoreError::Decrypt)
        ));
        let last = blob.len() - 1;
        blob[last] ^= 1;
        assert!(matches!(
            vault.open_sealed(b"host/a:password", &blob),
            Err(CoreError::Decrypt)
        ));
    }

    #[test]
    fn missing_or_wrong_key_is_detected() {
        let db = Database::open_in_memory().unwrap();
        Vault::open(&db, Box::new(MemoryKeyStore::default())).unwrap();
        // 另一台电脑：钥匙串里没有这把主密钥
        let vault = Vault::open(&db, Box::new(MemoryKeyStore::default())).unwrap();
        assert_eq!(vault.status(), VaultStatus::KeyMissing);
        assert!(matches!(
            vault.seal(b"a", b"b"),
            Err(CoreError::VaultLocked)
        ));

        let other = MemoryKeyStore::default();
        other.store(&[7u8; KEY_LEN]).unwrap();
        let vault = Vault::open(&db, Box::new(other)).unwrap();
        assert_eq!(vault.status(), VaultStatus::KeyMismatch);
    }

    #[test]
    fn keyring_unavailable_falls_back_to_password() {
        let db = Database::open_in_memory().unwrap();
        let vault = Vault::open(&db, Box::new(MemoryKeyStore::unavailable())).unwrap();
        assert!(matches!(
            vault.status(),
            VaultStatus::NeedsNewPassword { .. }
        ));
        let password = SecretString::new("correct horse");
        vault.set_new_password(&db, &password).unwrap();
        let blob = vault.seal(b"x", b"value").unwrap();

        let reopened = Vault::open(&db, Box::new(MemoryKeyStore::unavailable())).unwrap();
        assert_eq!(reopened.status(), VaultStatus::NeedsPassword);
        assert!(matches!(
            reopened.unlock(&db, &SecretString::new("wrong")),
            Err(CoreError::WrongPassword)
        ));
        reopened.unlock(&db, &password).unwrap();
        assert_eq!(&reopened.open_sealed(b"x", &blob).unwrap()[..], b"value");
        // 使用统计的功能名单记下用主密码解锁过
        assert!(crate::usage::features::global().contains(crate::usage::Feature::MasterPassword));

        let new_password = SecretString::new("battery staple");
        assert!(Vault::uses_password(&db).unwrap());
        // 当前主密码不对、新主密码太短时不改
        assert!(matches!(
            reopened.change_password(&db, &SecretString::new("wrong"), &new_password),
            Err(CoreError::WrongPassword)
        ));
        assert!(matches!(
            reopened.change_password(&db, &password, &SecretString::new("short")),
            Err(CoreError::Invalid(_))
        ));
        reopened.unlock(&db, &password).unwrap();
        reopened
            .change_password(&db, &password, &new_password)
            .unwrap();
        let again = Vault::open(&db, Box::new(MemoryKeyStore::unavailable())).unwrap();
        assert!(matches!(
            again.unlock(&db, &password),
            Err(CoreError::WrongPassword)
        ));
        again.unlock(&db, &new_password).unwrap();
        assert_eq!(&again.open_sealed(b"x", &blob).unwrap()[..], b"value");
    }

    // 便携版第一次使用先让用户选：用主密码时之后每次输主密码（在别的电脑上也能解开）；用这台电脑的钥匙串时
    // 与安装版相同；钥匙串不可用时转为设置主密码
    #[test]
    fn portable_vault_lets_the_user_choose() {
        let db = Database::open_in_memory().unwrap();
        let vault = Vault::open_portable(&db, memory(), memory()).unwrap();
        assert_eq!(vault.status(), VaultStatus::ChooseKeyStore);
        assert!(!vault.is_unlocked());
        let password = SecretString::new("correct horse");
        vault.set_new_password(&db, &password).unwrap();
        assert!(vault.is_unlocked() && Vault::uses_password(&db).unwrap());
        let reopened = Vault::open_portable(&db, memory(), memory()).unwrap();
        assert_eq!(reopened.status(), VaultStatus::NeedsPassword, "已经选过");

        let db = Database::open_in_memory().unwrap();
        let vault = Vault::open_portable(&db, memory(), memory()).unwrap();
        assert_eq!(vault.choose_keyring(&db).unwrap(), VaultStatus::Unlocked);
        assert!(!Vault::uses_password(&db).unwrap());
        assert!(vault.seal(b"x", b"value").is_ok());
        assert!(matches!(
            vault.choose_keyring(&db),
            Err(CoreError::VaultLocked)
        ));
        // 只在这台电脑上使用：没有主密码可记
        assert_eq!(vault.remember_state(), Remember::Unavailable);
        assert!(vault.remember().is_err());

        let db = Database::open_in_memory().unwrap();
        let vault =
            Vault::open_portable(&db, Box::new(MemoryKeyStore::unavailable()), memory()).unwrap();
        assert!(matches!(
            vault.choose_keyring(&db).unwrap(),
            VaultStatus::NeedsNewPassword { .. }
        ));
        vault.set_new_password(&db, &password).unwrap();
        assert!(vault.is_unlocked());
    }

    fn memory() -> Box<dyn KeyStore> {
        Box::new(MemoryKeyStore::default())
    }

    /// 便携版用主密码 `password` 保护的数据（第一次使用时已经选了设置主密码）。
    fn portable_with_password(password: &SecretString) -> Database {
        let db = Database::open_in_memory().unwrap();
        let vault = Vault::open_portable(&db, memory(), memory()).unwrap();
        vault.set_new_password(&db, password).unwrap();
        db
    }

    /// 像启动时那样打开便携版的保险库，`remembered` 是这台电脑记住主密钥的地方。
    fn open_portable(db: &Database, remembered: &SharedKeys) -> Vault {
        Vault::open_portable(db, memory(), Box::new(remembered.clone())).unwrap()
    }

    // 便携版用主密码时：勾选“在这台电脑上记住”解锁后，下次直接解锁；不勾不记；改主密码后仍有效；忘记后重新询问
    #[test]
    fn portable_password_can_be_remembered_on_this_computer() {
        let password = SecretString::new("correct horse");
        let db = portable_with_password(&password);
        let remembered = SharedKeys::default();

        let vault = open_portable(&db, &remembered);
        assert_eq!(vault.status(), VaultStatus::NeedsPassword);
        assert_eq!(vault.remember_state(), Remember::Available);
        assert!(
            matches!(vault.remember(), Err(CoreError::VaultLocked)),
            "解锁前没有主密钥可记"
        );
        // 不勾：照常解锁，不记
        vault.unlock(&db, &password).unwrap();
        let blob = vault.seal(b"x", b"value").unwrap();
        assert!(remembered.load().unwrap().is_none());
        let vault = open_portable(&db, &remembered);
        assert_eq!(vault.status(), VaultStatus::NeedsPassword, "没勾，照常询问");

        // 勾上：记住的是主密钥本身（不是主密码），下次直接解锁
        vault.unlock(&db, &password).unwrap();
        vault.remember().unwrap();
        assert_eq!(vault.remember_state(), Remember::Remembered);
        assert_eq!(remembered.load().unwrap().unwrap().len(), KEY_LEN);
        let vault = open_portable(&db, &remembered);
        assert_eq!(vault.status(), VaultStatus::Unlocked);
        assert_eq!(vault.remember_state(), Remember::Remembered);
        assert_eq!(&vault.open_sealed(b"x", &blob).unwrap()[..], b"value");
        assert!(Vault::uses_password(&db).unwrap(), "仍是主密码模式");

        // 改主密码：主密钥不变，记住的照常有效；别的电脑上用新的主密码
        let new_password = SecretString::new("battery staple");
        vault
            .change_password(&db, &password, &new_password)
            .unwrap();
        let vault = open_portable(&db, &remembered);
        assert_eq!(vault.status(), VaultStatus::Unlocked, "改主密码后仍记得");
        let elsewhere = open_portable(&db, &SharedKeys::default());
        assert_eq!(elsewhere.status(), VaultStatus::NeedsPassword);
        elsewhere.unlock(&db, &new_password).unwrap();

        // 忘记：这次照常可用，下次重新询问
        vault.forget().unwrap();
        assert_eq!(vault.remember_state(), Remember::Available);
        assert!(remembered.load().unwrap().is_none());
        assert!(vault.is_unlocked());
        let vault = open_portable(&db, &remembered);
        assert_eq!(vault.status(), VaultStatus::NeedsPassword);
        assert_eq!(vault.remember_state(), Remember::Available);
    }

    /// 按账户名分开存的钥匙串（像系统的凭据管理器那样，几份数据共用一个）。
    #[derive(Clone, Default)]
    struct Keyring(Arc<Mutex<std::collections::HashMap<String, [u8; KEY_LEN]>>>);

    /// 钥匙串中的一个条目，账户名与 [`KeyringStore::remembered`] 相同。
    struct KeyringEntry {
        keyring: Keyring,
        account: String,
    }

    impl Keyring {
        fn remembered(&self, db: &Database) -> Box<dyn KeyStore> {
            Box::new(KeyringEntry {
                keyring: self.clone(),
                account: KeyringStore::remembered(&data_dir_id(db).unwrap()).account,
            })
        }
    }

    impl KeyStore for KeyringEntry {
        fn load(&self) -> Result<Option<MasterKey>> {
            let keys = self.keyring.0.lock().unwrap();
            Ok(keys.get(&self.account).copied().map(Zeroizing::new))
        }

        fn store(&self, key: &[u8; KEY_LEN]) -> Result<()> {
            let mut keys = self.keyring.0.lock().unwrap();
            keys.insert(self.account.clone(), *key);
            Ok(())
        }

        fn delete(&self) -> Result<()> {
            self.keyring.0.lock().unwrap().remove(&self.account);
            Ok(())
        }
    }

    // 同一台电脑上的两份便携数据：条目按各自的数据目录 ID 区分，互不干扰；与钥匙串模式的条目也分开
    #[test]
    fn remembered_keys_of_two_portable_copies_do_not_mix() {
        let password = SecretString::new("correct horse");
        let (a, b) = (
            portable_with_password(&password),
            portable_with_password(&password),
        );
        let (id_a, id_b) = (data_dir_id(&a).unwrap(), data_dir_id(&b).unwrap());
        assert_ne!(id_a, id_b);
        assert_eq!(
            data_dir_id(&a).unwrap(),
            id_a,
            "数据目录 ID 存在数据中，移动文件夹不变"
        );
        assert_eq!(
            KeyringStore::remembered(&id_a).account,
            format!("remembered-master-key:{id_a}")
        );
        assert_ne!(
            KeyringStore::remembered(&id_a).account,
            KeyringStore::new(&id_a).account
        );

        let keyring = Keyring::default();
        let open = |db: &Database| Vault::open_portable(db, memory(), keyring.remembered(db));
        let vault = open(&a).unwrap();
        vault.unlock(&a, &password).unwrap();
        vault.remember().unwrap();
        assert_eq!(open(&a).unwrap().status(), VaultStatus::Unlocked);
        assert_eq!(
            open(&b).unwrap().status(),
            VaultStatus::NeedsPassword,
            "另一份数据没记住"
        );

        let vault = open(&b).unwrap();
        vault.unlock(&b, &password).unwrap();
        vault.remember().unwrap();
        vault.forget().unwrap();
        assert_eq!(
            open(&a).unwrap().status(),
            VaultStatus::Unlocked,
            "忘记 b 不影响 a"
        );
        assert_eq!(open(&b).unwrap().status(), VaultStatus::NeedsPassword);
    }

    // 记住的主密钥与数据对不上（别处重置过保险库等）：删掉它，照常询问主密码，不报错
    #[test]
    fn mismatched_remembered_key_is_deleted() {
        let password = SecretString::new("correct horse");
        let db = portable_with_password(&password);
        let remembered = SharedKeys::default();
        remembered.store(&[7u8; KEY_LEN]).unwrap();
        let vault = open_portable(&db, &remembered);
        assert_eq!(vault.status(), VaultStatus::NeedsPassword);
        assert_eq!(vault.remember_state(), Remember::Available);
        assert!(remembered.load().unwrap().is_none(), "删掉了");
        vault.unlock(&db, &password).unwrap();
    }

    // 重置保险库时删掉记住的旧主密钥：改用主密码时可以重新记住，改用钥匙串时没有主密码可记
    #[test]
    fn reset_deletes_the_remembered_key() {
        let password = SecretString::new("correct horse");
        let db = portable_with_password(&password);
        let remembered = SharedKeys::default();
        let vault = open_portable(&db, &remembered);
        vault.unlock(&db, &password).unwrap();
        vault.remember().unwrap();

        vault.reset(&db, Some(&password), |_| Ok(())).unwrap();
        assert!(remembered.load().unwrap().is_none());
        assert_eq!(vault.remember_state(), Remember::Available);
        assert_eq!(
            open_portable(&db, &remembered).status(),
            VaultStatus::NeedsPassword
        );

        vault.remember().unwrap();
        vault.reset(&db, None, |_| Ok(())).unwrap();
        assert!(remembered.load().unwrap().is_none());
        assert_eq!(vault.remember_state(), Remember::Unavailable);
    }

    // 安装版、钥匙串不可用时不能记住
    #[test]
    fn remembering_needs_portable_and_a_keyring() {
        let password = SecretString::new("correct horse");
        let db = Database::open_in_memory().unwrap();
        let vault = Vault::open(&db, Box::new(MemoryKeyStore::unavailable())).unwrap();
        vault.set_new_password(&db, &password).unwrap();
        let vault = Vault::open(&db, memory()).unwrap();
        assert_eq!(vault.status(), VaultStatus::NeedsPassword);
        assert_eq!(vault.remember_state(), Remember::Unavailable, "安装版");
        vault.unlock(&db, &password).unwrap();
        assert!(vault.remember().is_err());

        let db = portable_with_password(&password);
        let vault =
            Vault::open_portable(&db, memory(), Box::new(MemoryKeyStore::unavailable())).unwrap();
        assert_eq!(vault.status(), VaultStatus::NeedsPassword);
        assert_eq!(
            vault.remember_state(),
            Remember::Unavailable,
            "钥匙串不可用"
        );
        vault.unlock(&db, &password).unwrap();
        assert!(vault.remember().is_err());
    }

    // 钥匙串模式不能改主密码
    #[test]
    fn keyring_mode_has_no_password_to_change() {
        let db = Database::open_in_memory().unwrap();
        let vault = Vault::open(&db, Box::new(MemoryKeyStore::default())).unwrap();
        assert!(!Vault::uses_password(&db).unwrap());
        assert!(matches!(
            vault.change_password(
                &db,
                &SecretString::new("anything"),
                &SecretString::new("new password")
            ),
            Err(CoreError::VaultLocked)
        ));
    }

    #[test]
    fn reset_replaces_key_and_clears() {
        let db = Database::open_in_memory().unwrap();
        Vault::open(&db, Box::new(MemoryKeyStore::default())).unwrap();
        let store = SharedKeys::default();
        let vault = Vault::open(&db, Box::new(store.clone())).unwrap();
        assert_eq!(vault.status(), VaultStatus::KeyMissing);
        let mut cleared = false;
        vault
            .reset(&db, None, |_| {
                cleared = true;
                Ok(())
            })
            .unwrap();
        assert!(cleared);
        assert!(vault.is_unlocked());
        let reopened = Vault::open(&db, Box::new(store)).unwrap();
        assert!(reopened.is_unlocked());
    }

    // ------------------------------------------------------------ 固定向量
    // 下面写死的值由 aes-gcm 0.10、argon2 0.5 算出。升级加密库后必须原样通过：已保存的秘密、主密码、备份与同步文件
    // 都要能照常解开，新写出的内容旧版本也能解开。

    const FIXED_PASSWORD: &str = "correct horse battery staple";
    const FIXED_SALT: [u8; 16] = *b"termlet-fixed-01";
    const FIXED_KEY: [u8; KEY_LEN] = *b"termlet fixed master key 32bytes";
    const FIXED_AAD: &[u8] = b"termlet:secret:v1:host/h_fixed:password";
    const FIXED_PLAINTEXT: &[u8] = "p@ssw0rd-中文".as_bytes();
    /// Argon2id(FIXED_PASSWORD, FIXED_SALT)：测试用的小参数（64 KiB、1 次、1 路）与正式参数（64 MiB、3 次、4 路）
    const DERIVED_SMALL: &str = "2146bf1b10adee643e4724d4f9931898e25082832b6b47fc165b7487bedef71e";
    const DERIVED_PRODUCTION: &str =
        "4fddf5e6d01b21e9be6e0aa223902992c473b0769633ebc713e1c72c7731b447";
    /// FIXED_KEY 加密 FIXED_PLAINTEXT（nonce 为 12 个 0x03）
    const SEALED: &str =
        "01030303030303030303030303f1b9e239375b31a4389d664d725f989756d4eabb0eca732597afba16e3ed9c";
    /// 主密码模式：正式参数派生的密钥包裹 FIXED_KEY（nonce 为 12 个 0x01）、FIXED_KEY 加密的校验值（nonce 为 12 个 0x02）
    const KDF_JSON: &str = r#"{"memory_kib":65536,"iterations":3,"parallelism":4,"salt":[116,101,114,109,108,101,116,45,102,105,120,101,100,45,48,49]}"#;
    const WRAPPED: &str = "010101010101010101010101015d9bda1531b63f8393982dfb48728fb9f06575fc2fed84af1ed8aade2cca4fb84a81f6d229c79093946a7a24255846b8";
    const CHECK: &str = "010202020202020202020202024e509b61c7d0b2b0994a9d34a4cedadc344a2d19b614318c3eb9791c6363b41289e5258eba73";

    fn hex(bytes: &[u8]) -> String {
        data_encoding::HEXLOWER.encode(bytes)
    }

    fn unhex(text: &str) -> Vec<u8> {
        data_encoding::HEXLOWER.decode(text.as_bytes()).unwrap()
    }

    fn fixed_params() -> KdfParams {
        KdfParams {
            memory_kib: 64 * 1024,
            iterations: 3,
            parallelism: 4,
            salt: FIXED_SALT.to_vec(),
        }
    }

    #[test]
    fn derive_key_fixed_vectors() {
        let password = FIXED_PASSWORD.as_bytes();
        let small = derive_key(password, &FIXED_SALT, 64, 1, 1).unwrap();
        assert_eq!(hex(&small[..]), DERIVED_SMALL);
        let production = derive_key(password, &FIXED_SALT, 64 * 1024, 3, 4).unwrap();
        assert_eq!(hex(&production[..]), DERIVED_PRODUCTION);
    }

    #[test]
    fn seal_open_fixed_vector() {
        let blob = seal_with_nonce(&FIXED_KEY, &[3; NONCE_LEN], FIXED_AAD, FIXED_PLAINTEXT);
        assert_eq!(hex(&blob), SEALED);
        let plain = open_with(&FIXED_KEY, FIXED_AAD, &unhex(SEALED)).unwrap();
        assert_eq!(&plain[..], FIXED_PLAINTEXT);
        assert!(matches!(
            open_with(
                &FIXED_KEY,
                b"termlet:secret:v1:host/h_other:password",
                &unhex(SEALED)
            ),
            Err(CoreError::Decrypt)
        ));
    }

    /// 写死的数据库内容（钥匙串模式、主密码模式）照常解锁，解开写死的秘密。
    #[test]
    fn vault_fixed_vectors() {
        // 钥匙串模式：钥匙串中是 FIXED_KEY
        let db = Database::open_in_memory().unwrap();
        db.set_meta(META_MODE, br#""keyring""#).unwrap();
        db.set_meta(META_CHECK, &unhex(CHECK)).unwrap();
        let store = MemoryKeyStore::default();
        store.store(&FIXED_KEY).unwrap();
        let vault = Vault::open(&db, Box::new(store)).unwrap();
        assert_eq!(vault.status(), VaultStatus::Unlocked);
        let plain = vault.open_sealed(FIXED_AAD, &unhex(SEALED)).unwrap();
        assert_eq!(&plain[..], FIXED_PLAINTEXT);

        // 主密码模式：写出的参数、包裹的主密钥与校验值逐字节不变
        let params = fixed_params();
        assert_eq!(serde_json::to_string(&params).unwrap(), KDF_JSON);
        let kek = params.derive(&SecretString::new(FIXED_PASSWORD)).unwrap();
        let wrapped = seal_with_nonce(&kek, &[1; NONCE_LEN], WRAP_AAD, &FIXED_KEY);
        assert_eq!(hex(&wrapped), WRAPPED);
        let check = seal_with_nonce(&FIXED_KEY, &[2; NONCE_LEN], CHECK_AAD, CHECK_PLAINTEXT);
        assert_eq!(hex(&check), CHECK);

        let db = Database::open_in_memory().unwrap();
        db.set_meta(META_MODE, br#""password""#).unwrap();
        db.set_meta(META_KDF, KDF_JSON.as_bytes()).unwrap();
        db.set_meta(META_WRAPPED, &unhex(WRAPPED)).unwrap();
        db.set_meta(META_CHECK, &unhex(CHECK)).unwrap();
        let vault = Vault::open(&db, Box::new(MemoryKeyStore::unavailable())).unwrap();
        assert_eq!(vault.status(), VaultStatus::NeedsPassword);
        assert!(matches!(
            vault.unlock(&db, &SecretString::new("wrong password")),
            Err(CoreError::WrongPassword)
        ));
        vault
            .unlock(&db, &SecretString::new(FIXED_PASSWORD))
            .unwrap();
        let plain = vault.open_sealed(FIXED_AAD, &unhex(SEALED)).unwrap();
        assert_eq!(&plain[..], FIXED_PLAINTEXT);
    }
}
