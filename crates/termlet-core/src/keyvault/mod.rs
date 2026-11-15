//! 密钥库的服务层：导入、生成、导出、改名、删除私钥，部署公钥。
//!
//! 私钥的解析、生成、格式转换与部署脚本在 [`crate::sshkey`]；这里把它们与存储接起来：
//! 私钥解密后统一转为不加密的 OpenSSH 格式，作为密钥条目的秘密 `private_key` 经保险库加密保存，
//! 之后连接不再需要私钥密码；条目（[`KeyEntry`]）中只有名称、算法、指纹、公钥等公开信息。
//!
//! # 用法（界面）
//!
//! 全部是异步函数：解析加密的私钥（bcrypt、Argon2）、生成 RSA、加密导出都耗 CPU，读写数据库会阻塞，
//! 内部都放在 `spawn_blocking` 中，因此必须在核心的运行时上执行（`crate::runtime::spawn`，返回的 future 是 `Send`）。
//!
//! - 导入：[`import`]（文件或粘贴的文本 + 可选的私钥密码）。加密的私钥没给密码时返回
//!   `Key(KeyError::PassphraseRequired)`，界面请用户输入后再调一次。同一把密钥（指纹相同）已导入时返回
//!   [`KeyVaultError::AlreadyExists`]；它的私钥已经不可用时（保险库重置过）补上并返回 [`Imported::Restored`]。
//! - 生成：[`generate`]（Ed25519、RSA 3072 / 4096）。
//! - 导出：[`export_private_key`]（OpenSSH 格式，可设私钥密码，文件只有当前用户可读写；界面先请用户确认风险）、
//!   [`export_public_key`]。目标已存在时返回 [`KeyVaultError::FileExists`]，确认后带 `overwrite` 再调。
//! - 改名、删除：[`rename`]、[`delete`]（仍被主机、凭据档案使用时返回 [`KeyVaultError::InUse`]，[`usage`] 列出是哪些）。
//! - 证书：[`attach_certificate`]（文件或粘贴的一行 OpenSSH 用户证书，必须是这把密钥的）、[`detach_certificate`]。
//!   从文件导入私钥时，私钥文件旁同名的 `-cert.pub` 是这把密钥的证书就一并附带。证书不是秘密，存在条目中，
//!   随条目同步；用这把密钥登录时先出示证书（见 `ssh::auth`）。
//! - 部署公钥：[`fn@deploy`]（逐台主机调用），可选验证通过后改用这把密钥（[`use_library_key`]）。
//!
//! 错误的每个变体都对应界面的一条文案；私钥解析失败的细节在 [`KeyError`]（`Unsupported`、`Malformed` 的各变体）。

mod deploy;
#[cfg(test)]
mod tests;

pub use crate::sshkey::{DeployOutcome, KeyKind};
pub use deploy::{
    DEPLOY_OUTPUT_LIMIT, DEPLOY_TIMEOUT, DeployReport, DeployRequest, deploy, use_library_key,
};

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::id::{HostId, KeyId, ProfileId};
use crate::model::{Auth, CliAuth, Host, KeyEntry, KeySource, Profile, ProfileAuth, SecretField};
use crate::secret::SecretString;
use crate::ssh::SshError;
use crate::sshcert::{self, CertError};
use crate::sshkey::{self, Key, KeyError, Malformed};
use crate::store::{Owner, Position, Store};
use crate::{CoreError, Invalid};

/// 名称的最大长度（字符数）。
pub const MAX_NAME_LEN: usize = 64;

/// 私钥文件的大小上限（与 [`sshkey::parse`] 相同，读文件前先检查）。
const MAX_FILE_SIZE: u64 = 1 << 20;

/// 密钥库操作失败的原因。`Display` 是写给日志看的中文，界面按变体选用自己的文案；其中没有秘密。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyVaultError {
    /// 私钥无法使用：已加密而没给密码、密码错误、不支持的种类、内容无效（见 [`KeyError`] 的各变体）
    #[error("{0}")]
    Key(KeyError),
    /// 输入不合法：`name` required / too_long，生成时的 `comment` too_long / control_characters
    #[error("内容不合法：{0}")]
    Invalid(Invalid),
    /// 这把密钥（指纹相同）已经在密钥库中
    #[error("这把密钥已经导入过（{name}）")]
    AlreadyExists { key: KeyId, name: String },
    #[error("密钥不存在")]
    NotFound,
    #[error("主机不存在")]
    HostNotFound,
    /// 部署的主机不是 SSH 主机
    #[error("主机不是 SSH 主机")]
    NotSsh,
    /// 密钥库中这把密钥的私钥已经不可用（保险库重置过、密文损坏）：重新导入同一把私钥即可补上
    #[error("密钥库中没有这把密钥的私钥")]
    PrivateKeyMissing,
    /// 仍被使用，不能删除
    #[error("还有 {hosts} 台主机、{profiles} 个凭据档案使用这把密钥")]
    InUse { hosts: usize, profiles: usize },
    /// 保险库已锁定（需要主密码）
    #[error("保险库已锁定")]
    VaultLocked,
    /// 导入：私钥文件不存在
    #[error("私钥文件不存在：{}", .path.display())]
    FileNotFound { path: PathBuf },
    /// 导入：没有权限读取私钥文件；导出：没有权限写入
    #[error("没有权限访问：{}", .path.display())]
    PermissionDenied { path: PathBuf },
    /// 导入：读取私钥文件失败
    #[error("读取 {} 失败：{reason}", .path.display())]
    ReadFailed { path: PathBuf, reason: String },
    /// 导出：路径不是绝对路径
    #[error("导出路径必须是绝对路径")]
    NotAbsolute,
    /// 导出：文件已存在（确认后带 `overwrite` 再导出）
    #[error("文件已存在：{}", .path.display())]
    FileExists { path: PathBuf },
    /// 导出：同名目录已存在
    #[error("同名目录已存在：{}", .path.display())]
    IsDirectory { path: PathBuf },
    /// 导出：所在目录不存在
    #[error("目录不存在：{}", .path.display())]
    DirectoryMissing { path: PathBuf },
    /// 导出：写入失败
    #[error("写入 {} 失败：{reason}", .path.display())]
    WriteFailed { path: PathBuf, reason: String },
    /// 部署：用主机保存的凭据连接、登录失败（未确认指纹时为 [`SshError::HostKeyRejected`]）
    #[error("{0}")]
    Connect(SshError),
    /// 部署：写入 `~/.ssh/authorized_keys` 失败；带上退出码与标准错误（没有时为标准输出）
    #[error("写入 ~/.ssh/authorized_keys 失败（退出码 {exit_code:?}）：{detail}")]
    DeployFailed {
        exit_code: Option<u32>,
        detail: String,
    },
    /// 部署：超过 [`DEPLOY_TIMEOUT`] 没有完成
    #[error("部署超时")]
    Timeout,
    /// 附带证书：内容不是证书或已损坏
    #[error("证书不可用：{0}")]
    Certificate(CertError),
    /// 附带证书：证书不是给这把密钥签发的
    #[error("证书与这把私钥对不上")]
    CertificateMismatch,
    /// 附带证书：这是主机证书，不能用来登录
    #[error("这是主机证书，不能用来登录")]
    HostCertificate,
    /// 存储的其他错误（数据库读写失败等）
    #[error("{0}")]
    Store(String),
}

impl From<CoreError> for KeyVaultError {
    fn from(error: CoreError) -> Self {
        match error {
            CoreError::VaultLocked => KeyVaultError::VaultLocked,
            CoreError::Invalid(invalid) => KeyVaultError::Invalid(invalid),
            CoreError::Key(key) => KeyVaultError::Key(key),
            other => KeyVaultError::Store(other.to_string()),
        }
    }
}

impl From<KeyError> for KeyVaultError {
    fn from(error: KeyError) -> Self {
        match error {
            KeyError::Invalid(invalid) => KeyVaultError::Invalid(invalid),
            other => KeyVaultError::Key(other),
        }
    }
}

impl From<Invalid> for KeyVaultError {
    fn from(invalid: Invalid) -> Self {
        KeyVaultError::Invalid(invalid)
    }
}

/// 导入的私钥从哪里来。
#[derive(Clone, Debug)]
pub enum ImportSource {
    /// 粘贴的文本
    Text(SecretString),
    /// 本机的私钥文件
    File(PathBuf),
}

/// 导入私钥的请求。
#[derive(Clone, Debug)]
pub struct ImportRequest {
    /// 为空时依次使用：私钥中的注释、文件名（从文件导入时）、"类型 + 指纹前几位"
    pub name: String,
    pub source: ImportSource,
    /// 加密的私钥的密码；未加密的私钥忽略
    pub passphrase: Option<SecretString>,
}

/// 导入的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Imported {
    /// 新增的密钥
    Added(KeyId),
    /// 这把密钥已在密钥库中，但它的私钥已经不可用（保险库重置过）：补上了，ID、名称不变，引用它的主机随之恢复可用
    Restored(KeyId),
}

impl Imported {
    pub fn id(&self) -> &KeyId {
        match self {
            Imported::Added(id) | Imported::Restored(id) => id,
        }
    }
}

/// 生成密钥的请求。
#[derive(Clone, Debug, Default)]
pub struct GenerateRequest {
    /// 为空时使用注释，注释也为空时使用"类型 + 指纹前几位"
    pub name: String,
    pub kind: KeyKind,
    /// 写在公钥行末尾，例如 `ruan@laptop`
    pub comment: String,
}

/// 导出私钥的请求。
#[derive(Clone, Debug)]
pub struct ExportRequest {
    /// 本机的绝对路径
    pub path: PathBuf,
    /// 不为空时导出的私钥用它加密（与 ssh-keygen 相同：aes256-ctr + bcrypt）
    pub passphrase: Option<SecretString>,
    /// 目标文件已存在时覆盖（界面确认之后）
    pub overwrite: bool,
}

/// 使用一把密钥的主机与凭据档案。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyUsage {
    /// 认证或命令行工具专用凭据用了这把密钥的主机
    pub hosts: Vec<HostId>,
    pub profiles: Vec<ProfileId>,
}

impl KeyUsage {
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty() && self.profiles.is_empty()
    }
}

/// 在阻塞线程上执行（解析、加密耗 CPU，读写数据库会阻塞）。
async fn blocking<T, F>(f: F) -> Result<T, KeyVaultError>
where
    F: FnOnce() -> Result<T, KeyVaultError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => Err(KeyVaultError::Store(error.to_string())),
    }
}

/// 导入私钥：解析（加密的用 `passphrase` 解密）、转为不加密的 OpenSSH 格式存入密钥库。
pub async fn import(store: Arc<Store>, request: ImportRequest) -> Result<Imported, KeyVaultError> {
    blocking(move || {
        let ImportRequest {
            name,
            source,
            passphrase,
        } = request;
        let (data, file) = match source {
            ImportSource::Text(text) => (
                zeroize::Zeroizing::new(text.expose().as_bytes().to_vec()),
                None,
            ),
            ImportSource::File(path) => (read_key_file(&path)?, Some(path)),
        };
        let key = sshkey::parse(&data, passphrase.as_ref())?;
        let mut name = name.trim().to_string();
        if name.is_empty()
            && key.comment().is_empty()
            && let Some(stem) = file.as_deref().and_then(Path::file_stem)
        {
            name = stem.to_string_lossy().into_owned();
        }
        let imported = save(&store, &name, &key)?;
        if let Some(file) = &file {
            attach_beside(&store, imported.id(), file);
        }
        Ok(imported)
    })
    .await
}

/// 附带的证书从哪里来。
#[derive(Clone, Debug)]
pub enum CertificateSource {
    /// 粘贴的一行 OpenSSH 证书
    Text(String),
    /// 证书文件（`*-cert.pub`）
    File(PathBuf),
}

/// 给密钥库中的密钥附带 OpenSSH 用户证书（替换原有的）：必须是用户证书、证书里的公钥必须是这把密钥的。
pub async fn attach_certificate(
    store: Arc<Store>,
    key: KeyId,
    source: CertificateSource,
) -> Result<(), KeyVaultError> {
    blocking(move || {
        let entry = store.get::<KeyEntry>(&key).ok_or(KeyVaultError::NotFound)?;
        let text = match source {
            CertificateSource::Text(text) => text,
            CertificateSource::File(path) => {
                sshcert::read_file(&path).map_err(|e| read_error(&path, e))?
            }
        };
        let line = checked_certificate(&text, &entry.doc.public_key)?;
        let mut doc = entry.doc;
        doc.certificate = line;
        store.update(&key, doc)?;
        tracing::info!(id = %key, "已给密钥附带证书");
        Ok(())
    })
    .await
}

/// 去掉密钥附带的证书。
pub async fn detach_certificate(store: Arc<Store>, key: KeyId) -> Result<(), KeyVaultError> {
    blocking(move || {
        let mut doc = store
            .get::<KeyEntry>(&key)
            .ok_or(KeyVaultError::NotFound)?
            .doc;
        if doc.certificate.is_empty() {
            return Ok(());
        }
        doc.certificate.clear();
        store.update(&key, doc)?;
        tracing::info!(id = %key, "已去掉密钥附带的证书");
        Ok(())
    })
    .await
}

/// 检查证书（用户证书、与公钥配对），返回规范的一行（保存用）。
fn checked_certificate(text: &str, public_key: &str) -> Result<String, KeyVaultError> {
    let certificate = sshcert::parse(text).map_err(KeyVaultError::Certificate)?;
    if certificate.cert_type() != ssh_key::certificate::CertType::User {
        return Err(KeyVaultError::HostCertificate);
    }
    if !sshcert::matches_public_key(&certificate, public_key) {
        return Err(KeyVaultError::CertificateMismatch);
    }
    tracing::debug!(certificate = %sshcert::summary(&certificate), "证书可以附带");
    certificate
        .to_openssh()
        .map_err(|_| KeyVaultError::Certificate(CertError::Malformed))
}

/// 从文件导入私钥后：私钥文件旁有同名的 `-cert.pub` 且是这把密钥的证书时一并附带（已有证书的不替换）；
/// 不能用的忽略（只记日志）。
fn attach_beside(store: &Store, id: &KeyId, key_file: &Path) {
    let Some(path) = sshcert::find_beside(key_file) else {
        return;
    };
    let Some(record) = store.get::<KeyEntry>(id) else {
        return;
    };
    if !record.doc.certificate.is_empty() {
        return;
    }
    let checked = sshcert::read_file(&path)
        .map_err(|e| read_error(&path, e))
        .and_then(|text| checked_certificate(&text, &record.doc.public_key));
    match checked {
        Ok(line) => {
            let mut doc = record.doc;
            doc.certificate = line;
            match store.update(id, doc) {
                Ok(()) => {
                    tracing::info!(id = %id, path = %path.display(), "导入私钥时一并附带了旁边的证书")
                }
                Err(e) => tracing::warn!(id = %id, "附带证书失败：{e}"),
            }
        }
        Err(e) => tracing::info!(path = %path.display(), "私钥旁的证书不能附带：{e}"),
    }
}

/// 导入未加密的私钥（从其他工具导入主机时）：同一把密钥已在密钥库中时返回已有的那把（它的私钥已经不可用时补上），
/// 不报错、不重复。`name` 超过 64 个字符时截断。
pub async fn ensure(
    store: Arc<Store>,
    name: String,
    private_key: SecretString,
) -> Result<KeyId, KeyVaultError> {
    blocking(move || {
        let key = sshkey::parse(private_key.expose().as_bytes(), None)?;
        if let Some((id, _, intact)) = existing(&store, &key)? {
            if !intact {
                put_private_key(&store, &id, &key)?;
            }
            return Ok(id);
        }
        let name: String = name.trim().chars().take(MAX_NAME_LEN).collect();
        save(&store, &name, &key).map(|imported| imported.id().clone())
    })
    .await
}

/// 生成新密钥并存入密钥库（RSA 生成耗时较长，在阻塞线程上进行），与导入的密钥用法相同。
///
/// 作者：ruan
pub async fn generate(store: Arc<Store>, request: GenerateRequest) -> Result<KeyId, KeyVaultError> {
    blocking(move || {
        check_name_length(request.name.trim())?;
        tracing::info!(kind = ?request.kind, "生成密钥");
        let key = sshkey::generate(request.kind, &request.comment)?;
        save(&store, request.name.trim(), &key).map(|imported| imported.id().clone())
    })
    .await
}

/// 把私钥以 OpenSSH 格式写到本机文件（可选私钥密码），只有当前用户可读写，返回写入的路径。
/// 私钥离开密钥库后不再受保险库保护，界面在调用前请用户确认。
pub async fn export_private_key(
    store: Arc<Store>,
    key: KeyId,
    request: ExportRequest,
) -> Result<PathBuf, KeyVaultError> {
    blocking(move || {
        let path = export_path(&request.path)?;
        let key_id = key.clone();
        let key = private_key(&store, &key)?;
        let text = key.export_openssh(request.passphrase.as_ref())?;
        check_target(&path, request.overwrite)?;
        crate::platform::write_private_file(&path, text.expose().as_bytes())
            .map_err(|e| write_error(&path, &e))?;
        tracing::info!(id = %key_id, path = %path.display(), with_passphrase = request.passphrase.is_some(), "私钥已导出到本机文件");
        Ok(path)
    })
    .await
}

/// 把公钥（authorized_keys 格式的一行）写到本机文件，返回写入的路径。
pub async fn export_public_key(
    store: Arc<Store>,
    key: KeyId,
    path: PathBuf,
    overwrite: bool,
) -> Result<PathBuf, KeyVaultError> {
    blocking(move || {
        let path = export_path(&path)?;
        let entry = store.get::<KeyEntry>(&key).ok_or(KeyVaultError::NotFound)?;
        check_target(&path, overwrite)?;
        std::fs::write(&path, format!("{}\n", entry.doc.public_key.trim()))
            .map_err(|e| write_error(&path, &e))?;
        tracing::info!(id = %key, path = %path.display(), "公钥已导出到本机文件");
        Ok(path)
    })
    .await
}

/// 修改名称（去掉首尾空白后不能为空，不超过 64 个字符）。
pub async fn rename(store: Arc<Store>, key: KeyId, name: String) -> Result<(), KeyVaultError> {
    blocking(move || {
        let name = name.trim();
        if name.is_empty() {
            return Err(Invalid::new("name", "required").into());
        }
        check_name_length(name)?;
        let mut entry = store
            .get::<KeyEntry>(&key)
            .ok_or(KeyVaultError::NotFound)?
            .doc;
        entry.name = name.to_string();
        store.update(&key, entry)?;
        Ok(())
    })
    .await
}

/// 删除密钥及其私钥；仍被主机、凭据档案使用时拒绝。
pub async fn delete(store: Arc<Store>, key: KeyId) -> Result<(), KeyVaultError> {
    blocking(move || {
        if store.get::<KeyEntry>(&key).is_none() {
            return Err(KeyVaultError::NotFound);
        }
        let usage = usage(&store, &key);
        if !usage.is_empty() {
            return Err(KeyVaultError::InUse {
                hosts: usage.hosts.len(),
                profiles: usage.profiles.len(),
            });
        }
        store.delete::<KeyEntry>(&key)?;
        tracing::info!(id = %key, "密钥已删除");
        Ok(())
    })
    .await
}

/// 使用这把密钥的主机（认证方式或命令行工具专用凭据）与凭据档案。只读内存，可以在任何线程调用。
pub fn usage(store: &Store, key: &KeyId) -> KeyUsage {
    let uses = |source: &KeySource| matches!(source, KeySource::Library { key: k } if k == key);
    let hosts = store
        .list::<Host>()
        .into_iter()
        .filter(|h| {
            matches!(&h.doc.auth, Auth::Key { source } if uses(source))
                || h.doc
                    .cli
                    .credential
                    .as_ref()
                    .is_some_and(|c| matches!(&c.auth, CliAuth::Key { source } if uses(source)))
        })
        .map(|h| h.id)
        .collect();
    let profiles = store
        .list::<Profile>()
        .into_iter()
        .filter(|p| matches!(&p.doc.auth, ProfileAuth::Key { source } if uses(source)))
        .map(|p| p.id)
        .collect();
    KeyUsage { hosts, profiles }
}

// ---------------------------------------------------------------- 内部

fn check_name_length(name: &str) -> Result<(), KeyVaultError> {
    if name.chars().count() > MAX_NAME_LEN {
        return Err(Invalid::new("name", "too_long").into());
    }
    Ok(())
}

/// 读文件失败的原因。
fn read_error(path: &Path, error: io::Error) -> KeyVaultError {
    match error.kind() {
        io::ErrorKind::NotFound => KeyVaultError::FileNotFound {
            path: path.to_path_buf(),
        },
        io::ErrorKind::PermissionDenied => KeyVaultError::PermissionDenied {
            path: path.to_path_buf(),
        },
        _ => KeyVaultError::ReadFailed {
            path: path.to_path_buf(),
            reason: error.to_string(),
        },
    }
}

/// 读私钥文件（先看大小，过大的不读）。
fn read_key_file(path: &Path) -> Result<zeroize::Zeroizing<Vec<u8>>, KeyVaultError> {
    let fail = |error: io::Error| read_error(path, error);
    let metadata = std::fs::metadata(path).map_err(fail)?;
    if metadata.is_dir() {
        return Err(KeyVaultError::ReadFailed {
            path: path.to_path_buf(),
            reason: "这是一个目录".into(),
        });
    }
    if metadata.len() > MAX_FILE_SIZE {
        return Err(KeyError::Malformed(Malformed::TooLarge).into());
    }
    std::fs::read(path)
        .map(zeroize::Zeroizing::new)
        .map_err(fail)
}

/// 指纹相同的已有密钥：`(ID, 名称, 私钥是否完好)`。私钥为空（保险库重置后）或密文无法解密时不完好；
/// 保险库锁定时返回错误。
fn existing(store: &Store, key: &Key) -> Result<Option<(KeyId, String, bool)>, KeyVaultError> {
    let fingerprint = key.fingerprint();
    let Some(record) = store
        .list::<KeyEntry>()
        .into_iter()
        .find(|r| r.doc.fingerprint == fingerprint)
    else {
        return Ok(None);
    };
    let intact = match store.secret(&Owner::key(&record.id), SecretField::PrivateKey) {
        Ok(Some(secret)) => !secret.is_empty(),
        Ok(None) | Err(CoreError::Decrypt) => false,
        Err(error) => return Err(error.into()),
    };
    Ok(Some((record.id, record.doc.name, intact)))
}

/// 把解析好的私钥存入密钥库：条目（公开信息）与秘密（不加密的 OpenSSH 私钥）。`name` 为空时用默认名称。
/// 同一把密钥已存在时返回 [`KeyVaultError::AlreadyExists`]；它的私钥已经不可用时补上后返回 [`Imported::Restored`]。
fn save(store: &Store, name: &str, key: &Key) -> Result<Imported, KeyVaultError> {
    match existing(store, key)? {
        Some((id, _, false)) => {
            put_private_key(store, &id, key)?;
            return Ok(Imported::Restored(id));
        }
        Some((id, name, true)) => return Err(KeyVaultError::AlreadyExists { key: id, name }),
        None => {}
    }
    check_name_length(name)?;
    // 先确认保险库可用：否则会留下没有私钥的条目
    let private = key.to_openssh()?;
    if store.vault_status() != crate::vault::VaultStatus::Unlocked {
        return Err(KeyVaultError::VaultLocked);
    }
    let id = store.create(key.to_key_entry(name), None, Position::Last)?;
    if let Err(error) = store.set_secret(&Owner::key(&id), SecretField::PrivateKey, Some(&private))
    {
        let _ = store.delete::<KeyEntry>(&id);
        return Err(error.into());
    }
    tracing::info!(id = %id, algorithm = key.algorithm(), "密钥已存入密钥库");
    Ok(Imported::Added(id))
}

fn put_private_key(store: &Store, id: &KeyId, key: &Key) -> Result<(), KeyVaultError> {
    let private = key.to_openssh()?;
    store.set_secret(&Owner::key(id), SecretField::PrivateKey, Some(&private))?;
    tracing::info!(id = %id, "已补上密钥库中丢失的私钥");
    Ok(())
}

/// 取出并解析密钥库中的私钥。
pub(crate) fn private_key(store: &Store, id: &KeyId) -> Result<Key, KeyVaultError> {
    if store.get::<KeyEntry>(id).is_none() {
        return Err(KeyVaultError::NotFound);
    }
    let text = match store.secret(&Owner::key(id), SecretField::PrivateKey) {
        Ok(Some(text)) if !text.is_empty() => text,
        Ok(_) | Err(CoreError::Decrypt) => return Err(KeyVaultError::PrivateKeyMissing),
        Err(error) => return Err(error.into()),
    };
    sshkey::parse(text.expose().as_bytes(), None).map_err(|_| KeyVaultError::PrivateKeyMissing)
}

fn export_path(path: &Path) -> Result<PathBuf, KeyVaultError> {
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return Err(KeyVaultError::NotAbsolute);
    }
    // 去掉 `.`、多余的分隔符（不解析符号链接）
    Ok(path.components().collect())
}

/// 导出的目标：是目录时拒绝，已存在且没有确认覆盖时拒绝。
fn check_target(path: &Path, overwrite: bool) -> Result<(), KeyVaultError> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Err(KeyVaultError::IsDirectory {
            path: path.to_path_buf(),
        }),
        Ok(_) if !overwrite => Err(KeyVaultError::FileExists {
            path: path.to_path_buf(),
        }),
        _ => Ok(()),
    }
}

fn write_error(path: &Path, error: &io::Error) -> KeyVaultError {
    match error.kind() {
        io::ErrorKind::NotFound => KeyVaultError::DirectoryMissing {
            path: path.parent().unwrap_or(path).to_path_buf(),
        },
        io::ErrorKind::PermissionDenied => KeyVaultError::PermissionDenied {
            path: path.to_path_buf(),
        },
        _ => KeyVaultError::WriteFailed {
            path: path.to_path_buf(),
            reason: error.to_string(),
        },
    }
}
