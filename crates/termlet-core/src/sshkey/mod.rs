//! 私钥的解析、转换、生成与导出，以及部署公钥的远端脚本。
//!
//! 支持导入的格式：
//! - OpenSSH（`OPENSSH PRIVATE KEY`，可带 bcrypt 私钥密码）
//! - 传统 PEM：PKCS#1（`RSA PRIVATE KEY`）、未加密的 PKCS#8（`PRIVATE KEY`）、SEC1（`EC PRIVATE KEY`），
//!   以及带 `Proc-Type: 4,ENCRYPTED` 头的旧式加密 PEM
//! - PuTTY ppk v2 与 v3（未加密或 aes256-cbc 加密；v3 的 Argon2id、Argon2i、Argon2d）
//!
//! 支持的算法：ssh-ed25519、ssh-rsa（至少 1024 位）、ecdsa-sha2-nistp256/384/521。
//! DSA、Ed448、FIDO（`sk-*`）、加密的 PKCS#8、ppk v1 等返回 [`KeyError::Unsupported`]。
//!
//! 导入后统一转为不加密的 OpenSSH 格式（[`Key::to_openssh`]），由保险库加密保存，之后连接不再需要私钥密码。

mod deploy;
mod der;
mod legacy;
mod openssh;
mod pem;
mod ppk;
#[cfg(test)]
mod tests;

pub use deploy::{
    DEPLOY_MARK_ADDED, DEPLOY_MARK_EXISTS, DeployOutcome, deploy_outcome, deploy_script,
};

use serde::{Deserialize, Serialize};
use ssh_key::private::{Ed25519Keypair, KeypairData, RsaKeypair};
use ssh_key::{EcdsaCurve, HashAlg, LineEnding, PrivateKey, PublicKey};

use crate::Invalid;
use crate::model::KeyEntry;
use crate::secret::SecretString;

/// 私钥文件的大小上限。正常私钥（含 16384 位 RSA）不超过几十 KB，限制大小避免恶意输入占用过多内存。
const MAX_KEY_SIZE: usize = 1 << 20;

/// 可接受的最小 RSA 模长。
const MIN_RSA_BITS: u32 = 1024;

/// 生成密钥时注释的最大长度（字符数）。
const MAX_COMMENT_LEN: usize = 256;

/// 默认名称（取自注释时）的最大长度（字符数）。
const MAX_DEFAULT_NAME_LEN: usize = 64;

/// 私钥的来源格式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyFormat {
    OpenSsh,
    Pem,
    Ppk,
}

impl KeyFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyFormat::OpenSsh => "openssh",
            KeyFormat::Pem => "pem",
            KeyFormat::Ppk => "ppk",
        }
    }
}

/// 可生成的密钥。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum KeyKind {
    #[default]
    Ed25519,
    Rsa3072,
    Rsa4096,
}

/// 私钥操作失败的原因。`Display` 是写给日志看的中文；界面按变体选用自己的文案。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    /// 私钥已加密，但没有提供私钥密码
    #[error("私钥已加密，需要私钥密码")]
    PassphraseRequired,
    #[error("私钥密码错误")]
    WrongPassphrase,
    #[error("不支持的私钥：{0}")]
    Unsupported(Unsupported),
    #[error("私钥内容无效：{0}")]
    Malformed(Malformed),
    /// 生成密钥时的输入不合法（注释过长、含控制字符）
    #[error(transparent)]
    Invalid(#[from] Invalid),
    /// 加密库内部错误，正常情况下不会发生
    #[error("处理私钥失败：{0}")]
    Internal(&'static str),
}

/// 不支持的私钥种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Unsupported {
    #[error("DSA 密钥")]
    Dsa,
    #[error("Ed448 密钥")]
    Ed448,
    /// FIDO 安全密钥（`sk-*`），私钥在硬件里，文件中只有句柄
    #[error("FIDO 安全密钥")]
    SecurityKey,
    #[error("RSA 密钥只有 {bits} 位，至少需要 1024 位")]
    RsaTooSmall { bits: u32 },
    /// NIST P-256 / P-384 / P-521 以外的椭圆曲线
    #[error("不支持的椭圆曲线")]
    Curve,
    /// 其他算法（X25519、RSA-PSS、不认识的算法名等）
    #[error("不支持的算法")]
    Algorithm,
    /// 加密的 PKCS#8（`ENCRYPTED PRIVATE KEY`），需先用 ssh-keygen 或 openssl 转换
    #[error("加密的 PKCS#8 私钥")]
    EncryptedPkcs8,
    /// 旧式加密 PEM 用了不认识的加密算法（`DEK-Info`）
    #[error("旧式加密 PEM 的加密算法")]
    PemCipher,
    /// 不认识的 PEM 类型
    #[error("PEM 类型")]
    PemType,
    /// OpenSSH 私钥用了不认识的加密算法或密钥派生算法
    #[error("OpenSSH 私钥的加密算法")]
    OpenSshCipher,
    /// PuTTY ppk v1，格式过旧，需用新版 PuTTYgen 重新保存
    #[error("PuTTY ppk v1")]
    PpkV1,
    #[error("未知的 ppk 版本")]
    PpkVersion,
    #[error("ppk 的加密方式")]
    PpkCipher,
    #[error("ppk 的密钥派生算法")]
    PpkKdf,
    /// 密钥派生的参数过大（Argon2 内存、轮数，bcrypt 轮数），防止恶意文件占用过多内存或时间
    #[error("密钥派生参数过大")]
    KdfTooCostly,
    /// SSH-1 私钥
    #[error("SSH-1 私钥")]
    Ssh1,
    /// ssh.com（SECSH）格式，需先用 PuTTYgen 或 ssh-keygen 转换
    #[error("ssh.com 格式私钥")]
    SshCom,
}

/// 私钥内容无效的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Malformed {
    #[error("内容为空")]
    Empty,
    #[error("文件过大")]
    TooLarge,
    #[error("无法识别的私钥格式")]
    Unrecognized,
    /// 公钥或证书，不是私钥
    #[error("这是公钥或证书，不是私钥")]
    NotPrivateKey,
    /// 结构损坏、编码错误、校验失败，或私钥与公钥不一致
    #[error("内容已损坏")]
    Corrupted,
}

impl From<Unsupported> for KeyError {
    fn from(value: Unsupported) -> Self {
        KeyError::Unsupported(value)
    }
}

impl From<Malformed> for KeyError {
    fn from(value: Malformed) -> Self {
        KeyError::Malformed(value)
    }
}

/// 解析或生成的私钥（未加密）。`Debug` 只显示算法、指纹与来源格式。
#[derive(Clone)]
pub struct Key {
    inner: PrivateKey,
    format: KeyFormat,
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Key")
            .field("algorithm", &self.algorithm())
            .field("fingerprint", &self.fingerprint())
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

/// 解析私钥文本（文件内容或粘贴的内容）。
///
/// 加密的私钥需要 `passphrase`：没有提供（`None` 或空）时返回 [`KeyError::PassphraseRequired`]，
/// 错误时返回 [`KeyError::WrongPassphrase`]；未加密的私钥忽略 `passphrase`。
pub fn parse(data: &[u8], passphrase: Option<&SecretString>) -> Result<Key, KeyError> {
    if data.len() > MAX_KEY_SIZE {
        return Err(Malformed::TooLarge.into());
    }
    // 去掉 UTF-8 BOM（Windows 记事本保存的文件常见）与首尾空白
    let data = data.strip_prefix(b"\xef\xbb\xbf").unwrap_or(data);
    let data = data.trim_ascii();
    if data.is_empty() {
        return Err(Malformed::Empty.into());
    }
    let passphrase = passphrase
        .map(SecretString::expose)
        .filter(|p| !p.is_empty());

    let (inner, format) = if data.starts_with(ppk::MAGIC) {
        (ppk::decode(data, passphrase)?, KeyFormat::Ppk)
    } else if data.starts_with(b"SSH PRIVATE KEY FILE FORMAT 1.1") {
        return Err(Unsupported::Ssh1.into());
    } else if data.starts_with(b"---- BEGIN SSH2 ENCRYPTED PRIVATE KEY ----") {
        return Err(Unsupported::SshCom.into());
    } else if contains(data, b"-----BEGIN ") {
        pem::decode(data, passphrase)?
    } else if data.starts_with(b"---- BEGIN SSH2 PUBLIC KEY ----") || looks_like_public_key(data) {
        return Err(Malformed::NotPrivateKey.into());
    } else {
        return Err(Malformed::Unrecognized.into());
    };
    finish(inner, format)
}

/// 生成新的私钥。`comment` 去掉首尾空白后写入私钥与公钥行末尾，不超过 256 个字符，不能含换行等控制字符。
///
/// 作者：ruan
pub fn generate(kind: KeyKind, comment: &str) -> Result<Key, KeyError> {
    let comment = comment.trim();
    if comment.chars().count() > MAX_COMMENT_LEN {
        return Err(Invalid::new("comment", "too_long").into());
    }
    if comment.chars().any(char::is_control) {
        return Err(Invalid::new("comment", "control_characters").into());
    }
    let mut rng = rng();
    let data = match kind {
        KeyKind::Ed25519 => KeypairData::from(Ed25519Keypair::random(&mut rng)),
        KeyKind::Rsa3072 | KeyKind::Rsa4096 => {
            let bits = if kind == KeyKind::Rsa3072 { 3072 } else { 4096 };
            let pair = RsaKeypair::random(&mut rng, bits)
                .map_err(|_| KeyError::Internal("生成 RSA 密钥"))?;
            KeypairData::from(pair)
        }
    };
    let inner = PrivateKey::new(data, comment).map_err(|_| KeyError::Internal("生成密钥"))?;
    Ok(Key {
        inner,
        format: KeyFormat::OpenSsh,
    })
}

impl Key {
    /// 来源格式；生成的密钥为 OpenSSH。
    pub fn format(&self) -> KeyFormat {
        self.format
    }

    /// 公钥算法名，例如 ssh-ed25519、ssh-rsa、ecdsa-sha2-nistp256。
    pub fn algorithm(&self) -> &'static str {
        match self.inner.key_data() {
            KeypairData::Ed25519(_) => "ssh-ed25519",
            KeypairData::Rsa(_) => "ssh-rsa",
            KeypairData::Ecdsa(pair) => match pair.curve() {
                EcdsaCurve::NistP256 => "ecdsa-sha2-nistp256",
                EcdsaCurve::NistP384 => "ecdsa-sha2-nistp384",
                EcdsaCurve::NistP521 => "ecdsa-sha2-nistp521",
            },
            // finish 已拒绝其他类型
            _ => "",
        }
    }

    /// 位数：RSA 为模长，Ed25519 为 256，ECDSA 为曲线位数（与 `ssh-keygen -l` 相同）。
    pub fn bits(&self) -> u32 {
        match self.inner.key_data() {
            KeypairData::Rsa(pair) => rsa_bits(pair),
            KeypairData::Ecdsa(pair) => match pair.curve() {
                EcdsaCurve::NistP256 => 256,
                EcdsaCurve::NistP384 => 384,
                EcdsaCurve::NistP521 => 521,
            },
            _ => 256,
        }
    }

    /// 注释（OpenSSH、ppk 中的注释；PEM 没有注释，为空）。不含换行。
    pub fn comment(&self) -> &str {
        self.inner.comment().as_str_lossy()
    }

    /// SHA256 指纹，格式同 `ssh-keygen -l`（`SHA256:…`）。
    pub fn fingerprint(&self) -> String {
        self.inner.fingerprint(HashAlg::Sha256).to_string()
    }

    /// OpenSSH 格式的一行公钥（authorized_keys 格式，不带换行），有注释时附在末尾。
    pub fn public_key(&self) -> String {
        // 公钥编码只会因缓冲区长度出错，这里由 ssh-key 自己分配，不会失败
        self.inner.public_key().to_openssh().unwrap_or_default()
    }

    /// 转成不加密的 OpenSSH 私钥（保留注释），用于存入保险库。
    pub fn to_openssh(&self) -> Result<SecretString, KeyError> {
        let text = self
            .inner
            .to_openssh(LineEnding::LF)
            .map_err(|_| KeyError::Internal("序列化 OpenSSH 私钥"))?;
        Ok(SecretString::new(text.as_str()))
    }

    /// 导出为 OpenSSH 格式（保留注释）。提供 `passphrase`（非空）时用它加密，
    /// 与 ssh-keygen 相同（aes256-ctr + bcrypt 16 轮）。
    pub fn export_openssh(
        &self,
        passphrase: Option<&SecretString>,
    ) -> Result<SecretString, KeyError> {
        let Some(passphrase) = passphrase.filter(|p| !p.is_empty()) else {
            return self.to_openssh();
        };
        let encrypted = self
            .inner
            .encrypt(&mut rng(), passphrase.expose())
            .map_err(|_| KeyError::Internal("加密 OpenSSH 私钥"))?;
        let text = encrypted
            .to_openssh(LineEnding::LF)
            .map_err(|_| KeyError::Internal("序列化 OpenSSH 私钥"))?;
        Ok(SecretString::new(text.as_str()))
    }

    /// 没有指定名称时的默认名称：注释；没有注释时用"类型 + 指纹前几位"区分（例如 `ed25519 AbCdEfGh`）。
    /// 不超过 64 个字符。
    pub fn default_name(&self) -> String {
        let comment = self.comment();
        if !comment.is_empty() {
            return comment.chars().take(MAX_DEFAULT_NAME_LEN).collect();
        }
        let fingerprint = self.fingerprint();
        let hash = fingerprint.strip_prefix("SHA256:").unwrap_or(&fingerprint);
        let algorithm = self.algorithm();
        let algorithm = algorithm.strip_prefix("ssh-").unwrap_or(algorithm);
        format!("{algorithm} {}", hash.chars().take(8).collect::<String>())
    }

    /// 生成密钥库条目（只有公开信息）。`name` 去掉首尾空白后为空时使用 [`Key::default_name`]。
    pub fn to_key_entry(&self, name: &str) -> KeyEntry {
        let name = name.trim();
        KeyEntry {
            name: if name.is_empty() {
                self.default_name()
            } else {
                name.to_string()
            },
            algorithm: self.algorithm().to_string(),
            bits: self.bits(),
            fingerprint: self.fingerprint(),
            public_key: self.public_key(),
            ..KeyEntry::default()
        }
    }

    /// 私钥（测试中签名、比较；连接时用 [`Key::into_ssh_private_key`]）。
    #[cfg(test)]
    pub(crate) fn ssh_private_key(&self) -> &PrivateKey {
        &self.inner
    }

    pub fn into_ssh_private_key(self) -> PrivateKey {
        self.inner
    }
}

/// 密码学安全的随机数源（系统随机数）。系统随机数不可用时 panic，与保险库相同。
fn rng() -> ssh_key::rand_core::UnwrapErr<ssh_key::getrandom::SysRng> {
    ssh_key::rand_core::UnwrapErr(ssh_key::getrandom::SysRng)
}

/// 解析完成后的统一处理：拒绝不支持的算法与过小的 RSA，确认私钥可用（签名能被公钥验证），清理注释。
fn finish(mut inner: PrivateKey, format: KeyFormat) -> Result<Key, KeyError> {
    check_public_key(inner.public_key())?;
    check_usable(&inner)?;
    let comment = clean_comment(&String::from_utf8_lossy(inner.comment().as_bytes()));
    inner.set_comment(comment);
    Ok(Key { inner, format })
}

/// 按算法名判断是否支持。解密之前（ppk 文件头、OpenSSH 的公钥部分）就能拒绝，不必先问私钥密码。
fn check_algorithm_name(name: &str) -> Result<(), Unsupported> {
    match name {
        "ssh-ed25519"
        | "ssh-rsa"
        | "ecdsa-sha2-nistp256"
        | "ecdsa-sha2-nistp384"
        | "ecdsa-sha2-nistp521" => Ok(()),
        "ssh-dss" => Err(Unsupported::Dsa),
        "ssh-ed448" => Err(Unsupported::Ed448),
        name if name.starts_with("sk-") => Err(Unsupported::SecurityKey),
        name if name.starts_with("ecdsa-sha2-") => Err(Unsupported::Curve),
        _ => Err(Unsupported::Algorithm),
    }
}

/// 检查公钥的算法与 RSA 位数。
fn check_public_key(public: &PublicKey) -> Result<(), KeyError> {
    use ssh_key::public::KeyData;
    match public.key_data() {
        KeyData::Ed25519(_) | KeyData::Ecdsa(_) => Ok(()),
        KeyData::Rsa(rsa) => {
            let bits = positive_bits(rsa.n().as_positive_bytes().unwrap_or_default());
            if bits < MIN_RSA_BITS {
                Err(Unsupported::RsaTooSmall { bits }.into())
            } else {
                Ok(())
            }
        }
        other => Err(check_algorithm_name(other.algorithm().as_str())
            .err()
            .unwrap_or(Unsupported::Algorithm)
            .into()),
    }
}

/// 用私钥签名一段固定内容并用公钥验证：私钥损坏、与公钥不一致（ppk、OpenSSH 文件中两者分开存放）时失败。
/// RSA 签名时会经 rsa 库完整校验各分量（模数等于两素数之积等）。
fn check_usable(key: &PrivateKey) -> Result<(), KeyError> {
    const NAMESPACE: &str = "termlet-key-check";
    const MESSAGE: &[u8] = b"termlet sshkey self-test";
    let signature = key
        .sign(NAMESPACE, HashAlg::Sha512, MESSAGE)
        .map_err(|_| Malformed::Corrupted)?;
    key.public_key()
        .verify(NAMESPACE, MESSAGE, &signature)
        .map_err(|_| Malformed::Corrupted)?;
    Ok(())
}

/// RSA 模长（位）。
fn rsa_bits(pair: &RsaKeypair) -> u32 {
    positive_bits(pair.public().n().as_positive_bytes().unwrap_or_default())
}

/// 大端无符号整数的位数。
fn positive_bits(bytes: &[u8]) -> u32 {
    let bytes = &bytes[bytes.iter().take_while(|&&b| b == 0).count()..];
    match bytes.first() {
        None => 0,
        Some(first) => (bytes.len() as u32 - 1) * 8 + (8 - first.leading_zeros()),
    }
}

/// 去掉注释中的换行，保证公钥只有一行。
fn clean_comment(comment: &str) -> String {
    comment
        .replace("\r\n", " ")
        .replace(['\r', '\n'], " ")
        .trim()
        .to_string()
}

/// 是否像一行公钥（authorized_keys 或证书），例如 `ssh-ed25519 AAAA… comment`。
fn looks_like_public_key(data: &[u8]) -> bool {
    let mut fields = data
        .split(|b| b.is_ascii_whitespace())
        .filter(|f| !f.is_empty());
    let (Some(kind), Some(blob)) = (fields.next(), fields.next()) else {
        return false;
    };
    (kind.starts_with(b"ssh-") || kind.starts_with(b"ecdsa-") || kind.starts_with(b"sk-"))
        && blob.starts_with(b"AAAA")
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}
