//! 把密钥库的错误说成用户能理解的话。

use gpui_kit::SharedString;
use termlet_core::keyvault::KeyVaultError;
use termlet_core::sshcert::CertError;
use termlet_core::sshkey::{KeyError, Malformed, Unsupported};

use crate::{ssh_errors, store_errors, t};

pub fn describe(error: &KeyVaultError) -> SharedString {
    match error {
        KeyVaultError::Key(error) => describe_key(error),
        KeyVaultError::Invalid(invalid) => match (invalid.field, invalid.reason) {
            ("name", "too_long") => t!("keys.error.name_too_long"),
            ("name", "required") => t!("keys.error.name_required"),
            ("comment", "too_long") => t!("keys.error.comment_too_long"),
            ("comment", _) => t!("keys.error.comment_control"),
            _ => store_errors::describe_invalid(invalid),
        },
        KeyVaultError::AlreadyExists { name, .. } => t!("keys.error.exists", name = name),
        KeyVaultError::NotFound => t!("keys.error.not_found"),
        KeyVaultError::HostNotFound => t!("keys.error.host_not_found"),
        KeyVaultError::NotSsh => t!("keys.error.not_ssh"),
        KeyVaultError::PrivateKeyMissing => t!("keys.error.private_key_missing"),
        KeyVaultError::InUse { hosts, profiles } => {
            t!("keys.error.in_use", hosts = hosts, profiles = profiles)
        }
        KeyVaultError::VaultLocked => t!("store.error.vault_locked"),
        KeyVaultError::FileNotFound { path } => {
            t!("keys.error.file_not_found", path = path.display())
        }
        KeyVaultError::PermissionDenied { path } => {
            t!("keys.error.permission", path = path.display())
        }
        KeyVaultError::ReadFailed { path, reason } => {
            t!(
                "keys.error.read_failed",
                path = path.display(),
                reason = reason
            )
        }
        KeyVaultError::NotAbsolute => t!("keys.error.not_absolute"),
        KeyVaultError::FileExists { path } => t!("keys.error.file_exists", path = path.display()),
        KeyVaultError::IsDirectory { path } => {
            t!("keys.error.is_directory", path = path.display())
        }
        KeyVaultError::DirectoryMissing { path } => {
            t!("keys.error.directory_missing", path = path.display())
        }
        KeyVaultError::WriteFailed { path, reason } => {
            t!(
                "keys.error.write_failed",
                path = path.display(),
                reason = reason
            )
        }
        KeyVaultError::Connect(error) => ssh_errors::describe(error),
        KeyVaultError::DeployFailed { exit_code, detail } => match exit_code {
            Some(code) => t!(
                "keys.error.deploy_failed_code",
                code = code,
                detail = detail
            ),
            None => t!("keys.error.deploy_failed", detail = detail),
        },
        KeyVaultError::Timeout => t!("keys.error.timeout"),
        KeyVaultError::Certificate(error) => {
            t!("keys.error.certificate", reason = describe_cert(error))
        }
        KeyVaultError::CertificateMismatch => t!("keys.error.certificate_mismatch"),
        KeyVaultError::HostCertificate => t!("keys.error.host_certificate"),
        KeyVaultError::Store(reason) => t!("store.error.failed", error = reason),
    }
}

/// 证书内容的问题（附带证书、连接时读证书）。
pub fn describe_cert(error: &CertError) -> SharedString {
    match error {
        CertError::Empty => t!("keys.cert.error.empty"),
        CertError::TooLarge => t!("keys.cert.error.too_large"),
        CertError::NotCertificate => t!("keys.cert.error.not_certificate"),
        CertError::Malformed => t!("keys.cert.error.malformed"),
    }
}

pub fn describe_key(error: &KeyError) -> SharedString {
    match error {
        KeyError::PassphraseRequired => t!("keys.error.passphrase_required"),
        KeyError::WrongPassphrase => t!("keys.error.wrong_passphrase"),
        KeyError::Unsupported(kind) => {
            let what: SharedString = match kind {
                Unsupported::Dsa => t!("keys.unsupported.dsa"),
                Unsupported::Ed448 => t!("keys.unsupported.ed448"),
                Unsupported::SecurityKey => t!("keys.unsupported.security_key"),
                Unsupported::RsaTooSmall { bits } => {
                    t!("keys.unsupported.rsa_too_small", bits = bits)
                }
                Unsupported::Curve => t!("keys.unsupported.curve"),
                Unsupported::Algorithm => t!("keys.unsupported.algorithm"),
                Unsupported::EncryptedPkcs8 => t!("keys.unsupported.encrypted_pkcs8"),
                Unsupported::PemCipher => t!("keys.unsupported.pem_cipher"),
                Unsupported::PemType => t!("keys.unsupported.pem_type"),
                Unsupported::OpenSshCipher => t!("keys.unsupported.openssh_cipher"),
                Unsupported::PpkV1 => t!("keys.unsupported.ppk_v1"),
                Unsupported::PpkVersion => t!("keys.unsupported.ppk_version"),
                Unsupported::PpkCipher => t!("keys.unsupported.ppk_cipher"),
                Unsupported::PpkKdf => t!("keys.unsupported.ppk_kdf"),
                Unsupported::KdfTooCostly => t!("keys.unsupported.kdf_too_costly"),
                Unsupported::Ssh1 => t!("keys.unsupported.ssh1"),
                Unsupported::SshCom => t!("keys.unsupported.ssh_com"),
            };
            t!("keys.error.unsupported", what = what)
        }
        KeyError::Malformed(reason) => match reason {
            Malformed::Empty => t!("keys.malformed.empty"),
            Malformed::TooLarge => t!("keys.malformed.too_large"),
            Malformed::Unrecognized => t!("keys.malformed.unrecognized"),
            Malformed::NotPrivateKey => t!("keys.malformed.not_private_key"),
            Malformed::Corrupted => t!("keys.malformed.corrupted"),
        },
        KeyError::Invalid(invalid) => store_errors::describe_invalid(invalid),
        KeyError::Internal(reason) => t!("keys.error.internal", reason = reason),
    }
}
