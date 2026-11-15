//! 密钥库中的密钥。私钥本身（统一转为不加密的 OpenSSH 格式）在秘密 `private_key` 中，这里只有展示用的公开信息，
//! 以及附带的 OpenSSH 证书（证书不是秘密，与条目一起明文保存、同步）。

use serde::{Deserialize, Serialize};

use super::{Extra, trimmed};
use crate::Invalid;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct KeyEntry {
    pub name: String,
    /// 算法，例如 ssh-ed25519、ssh-rsa、ecdsa-sha2-nistp256
    pub algorithm: String,
    /// RSA 的位数等，展示用
    pub bits: u32,
    /// SHA256 指纹（`SHA256:…`）
    pub fingerprint: String,
    /// OpenSSH 格式的公钥（一行）
    pub public_key: String,
    /// 附带的 OpenSSH 用户证书（`ssh-ed25519-cert-v01@openssh.com AAAA…` 一行，与私钥配对过）；没有时为空。
    /// 用这把密钥登录时先出示证书（见 [`crate::keyvault::attach_certificate`]）
    #[serde(skip_serializing_if = "String::is_empty")]
    pub certificate: String,
    #[serde(flatten)]
    pub extra: Extra,
}

impl KeyEntry {
    pub(crate) fn normalize(&mut self) -> Result<(), Invalid> {
        trimmed(&mut self.name, "name", 128)?;
        if self.name.is_empty() {
            return Err(Invalid::new("name", "required"));
        }
        if self.public_key.trim().is_empty() {
            return Err(Invalid::new("publicKey", "required"));
        }
        let certificate = self.certificate.trim();
        if certificate.len() != self.certificate.len() {
            self.certificate = certificate.to_string();
        }
        if self.certificate.len() > crate::sshcert::MAX_TEXT_LEN || self.certificate.contains('\n')
        {
            return Err(Invalid::new("certificate", "invalid"));
        }
        Ok(())
    }
}
