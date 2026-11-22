//! NyaTerm 的加密（按 NyaTerm 提交 290212c 的 `crates/nyaterm-core/src/credentials_crypto/mod.rs` 与
//! `portable_snapshot/encryption.rs`）。
//!
//! - 秘密：Base64（nonce 12 字节 ‖ AES-256-GCM 密文），密钥是 32 字节的主密钥。
//! - 主密钥以同样的格式包装成 `master_key_token`，包装密钥 = SHA-256（前缀 ‖ 材料）：前缀是
//!   `nyaterm-key-wrap-v1:`（更早的 `dragonfly-key-wrap-v1:`）；材料是主密码，没设主密码时是用户主目录的路径
//!   （便携版是 `portable.key` 的内容）。主密码本身也以这种格式保存在设置中，用“没设主密码”时的那把密钥加密。
//! - `.nya` 的外层：`NYASNAP\0`、版本 2、KDF 1（Argon2id，内存 19456 KiB、迭代 2、并行 1，各 4 字节小端）、
//!   盐 16 字节、nonce 12 字节，共 50 字节；其后是 AES-256-GCM 密文，附加数据是这 50 字节。没有这个头的更早的
//!   格式是 nonce ‖ 密文，密钥 = SHA-256（`nyaterm-cloud-snapshot-v1:` 或 `dragonfly-cloud-snapshot-v1:` ‖ 密码）。
//!   密码都先去掉首尾空白。

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::secret::SecretString;

/// 32 字节的密钥。
pub(super) type Key = Zeroizing<[u8; 32]>;

const WRAP_PREFIXES: [&[u8]; 2] = [b"nyaterm-key-wrap-v1:", b"dragonfly-key-wrap-v1:"];
pub(super) const SNAPSHOT_MAGIC: &[u8; 8] = b"NYASNAP\0";
const LEGACY_SNAPSHOT_PREFIXES: [&[u8]; 2] = [
    b"nyaterm-cloud-snapshot-v1:",
    b"dragonfly-cloud-snapshot-v1:",
];
const HEADER_LEN: usize = 50;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
/// `.nya` 外层的版本与 KDF 参数：只认 NyaTerm 写出的这一组（不按文件中的参数做任意成本的派生）
const SNAPSHOT_VERSION: u8 = 2;
const ARGON2ID: u8 = 1;
const MEMORY_KIB: u32 = 19 * 1024;
const ITERATIONS: u32 = 2;
const LANES: u32 = 1;

fn sha256(parts: &[&[u8]]) -> Key {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    Zeroizing::new(hasher.finalize().into())
}

/// AES-256-GCM 解密 `nonce ‖ 密文`。
fn open_raw(key: &[u8; 32], raw: &[u8], aad: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
    let (nonce, msg) = raw.split_first_chunk::<NONCE_LEN>()?;
    if msg.len() < TAG_LEN {
        return None;
    }
    Aes256Gcm::new(key.into())
        .decrypt(nonce.into(), Payload { msg, aad })
        .ok()
        .map(Zeroizing::new)
}

/// 用 `key` 解开 Base64（nonce ‖ 密文）。
fn open(key: &[u8; 32], token: &str) -> Option<Zeroizing<Vec<u8>>> {
    let raw = Zeroizing::new(data_encoding::BASE64.decode(token.trim().as_bytes()).ok()?);
    open_raw(key, &raw, &[])
}

/// 用材料（主密码、主目录路径或 `portable.key` 的内容）解开 `master_key_token`。两个前缀都试。
pub(super) fn unwrap_master_key(token: &str, material: &[u8]) -> Option<Key> {
    WRAP_PREFIXES.iter().find_map(|prefix| {
        let wrapping = sha256(&[prefix, material]);
        let plain = open(&wrapping, token)?;
        let key: [u8; 32] = plain.as_slice().try_into().ok()?;
        Some(Zeroizing::new(key))
    })
}

/// 用“没设主密码”时的材料解开设置中保存的文本（主密码）。
pub(super) fn settings_secret(token: &str, material: &[u8]) -> Option<Zeroizing<String>> {
    WRAP_PREFIXES.iter().find_map(|prefix| {
        let key = sha256(&[prefix, material]);
        let plain = open(&key, token)?;
        String::from_utf8(plain.to_vec()).ok().map(Zeroizing::new)
    })
}

/// 用主密钥解开一项秘密；不是这个格式或解不开时为 `None`。
pub(super) fn decrypt_secret(master: &Key, token: &str) -> Option<SecretString> {
    let plain = open(master, token)?;
    std::str::from_utf8(&plain).ok().map(SecretString::new)
}

/// 看起来是否是秘密的密文（Base64 解出 nonce 与认证标签那么长）：NyaTerm 读旧数据时也这样区分明文与密文。
pub(super) fn looks_encrypted(value: &str) -> bool {
    data_encoding::BASE64
        .decode(value.trim().as_bytes())
        .is_ok_and(|raw| raw.len() >= NONCE_LEN + TAG_LEN)
}

/// 解开 `.nya` 的外层失败的原因。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum EnvelopeError {
    /// 密码不对（认证失败）
    WrongPassword,
    /// 不认识的版本或 KDF 参数、长度不够
    Unsupported,
}

/// 解开 `.nya` 的外层，得到里面的 zip（或 redb）。
pub(super) fn open_snapshot(
    password: &str,
    data: &[u8],
) -> Result<Zeroizing<Vec<u8>>, EnvelopeError> {
    let password = password.trim();
    if data.starts_with(SNAPSHOT_MAGIC) {
        if data.len() < HEADER_LEN + TAG_LEN
            || data[8] != SNAPSHOT_VERSION
            || data[9] != ARGON2ID
            || data[10..14] != MEMORY_KIB.to_le_bytes()
            || data[14..18] != ITERATIONS.to_le_bytes()
            || data[18..22] != LANES.to_le_bytes()
        {
            return Err(EnvelopeError::Unsupported);
        }
        let key = argon2id(password, &data[22..38]).ok_or(EnvelopeError::Unsupported)?;
        let (header, body) = data.split_at(HEADER_LEN);
        let nonce: &[u8; NONCE_LEN] = header[38..50].try_into().expect("12 字节");
        return Aes256Gcm::new((&*key).into())
            .decrypt(
                nonce.into(),
                Payload {
                    msg: body,
                    aad: header,
                },
            )
            .map(Zeroizing::new)
            .map_err(|_| EnvelopeError::WrongPassword);
    }
    if data.len() < NONCE_LEN + TAG_LEN {
        return Err(EnvelopeError::Unsupported);
    }
    LEGACY_SNAPSHOT_PREFIXES
        .iter()
        .find_map(|prefix| open_raw(&sha256(&[prefix, password.as_bytes()]), data, &[]))
        .ok_or(EnvelopeError::WrongPassword)
}

fn argon2id(password: &str, salt: &[u8]) -> Option<Key> {
    let params = argon2::Params::new(MEMORY_KIB, ITERATIONS, LANES, Some(32)).ok()?;
    let mut key = Zeroizing::new([0u8; 32]);
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
        .hash_password_into(password.as_bytes(), salt, &mut key[..])
        .ok()?;
    Some(key)
}

/// 测试用：按 NyaTerm 的方式加密（构造样例）。
#[cfg(test)]
pub(super) mod seal {
    use super::*;

    fn seal_raw(key: &[u8; 32], nonce: [u8; NONCE_LEN], plain: &[u8], aad: &[u8]) -> Vec<u8> {
        let mut out = nonce.to_vec();
        out.extend(
            Aes256Gcm::new(key.into())
                .encrypt(&nonce.into(), Payload { msg: plain, aad })
                .unwrap(),
        );
        out
    }

    /// 用 `key` 加密成 Base64（nonce ‖ 密文）。
    pub fn secret(key: &[u8; 32], plain: &[u8]) -> String {
        data_encoding::BASE64.encode(&seal_raw(key, [7; NONCE_LEN], plain, &[]))
    }

    /// 用材料包装主密钥。`legacy` 时用旧前缀。
    pub fn wrap_master_key(master: &[u8; 32], material: &[u8], legacy: bool) -> String {
        let prefix = WRAP_PREFIXES[usize::from(legacy)];
        secret(&sha256(&[prefix, material]), master)
    }

    /// `.nya` 的外层（版本 2、Argon2id）。
    pub fn snapshot(password: &str, payload: &[u8]) -> Vec<u8> {
        let mut header = SNAPSHOT_MAGIC.to_vec();
        header.extend([SNAPSHOT_VERSION, ARGON2ID]);
        for value in [MEMORY_KIB, ITERATIONS, LANES] {
            header.extend(value.to_le_bytes());
        }
        let salt = [3u8; 16];
        let nonce = [5u8; NONCE_LEN];
        header.extend(salt);
        header.extend(nonce);
        let key = argon2id(password.trim(), &salt).unwrap();
        let sealed = seal_raw(&key, nonce, payload, &header);
        header.extend(&sealed[NONCE_LEN..]);
        header
    }

    /// 更早的没有文件头的外层。
    pub fn legacy_snapshot(password: &str, payload: &[u8]) -> Vec<u8> {
        seal_raw(
            &sha256(&[LEGACY_SNAPSHOT_PREFIXES[0], password.trim().as_bytes()]),
            [9; NONCE_LEN],
            payload,
            &[],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn master_key_and_secrets() {
        let master = [42u8; 32];
        for legacy in [false, true] {
            let token = seal::wrap_master_key(&master, b"C:\\Users\\me", legacy);
            let key = unwrap_master_key(&token, b"C:\\Users\\me").unwrap();
            assert_eq!(*key, master);
            assert!(unwrap_master_key(&token, b"C:\\Users\\other").is_none());
        }
        let key = Zeroizing::new(master);
        let token = seal::secret(&master, "密码".as_bytes());
        assert!(looks_encrypted(&token));
        assert!(!looks_encrypted("plain password"));
        assert_eq!(decrypt_secret(&key, &token).unwrap().expose(), "密码");
        assert!(decrypt_secret(&Zeroizing::new([1; 32]), &token).is_none());
        assert!(decrypt_secret(&key, "not base64").is_none());
    }

    #[test]
    fn settings_master_password() {
        let fallback = b"/home/me";
        let token = seal::secret(&sha256(&[WRAP_PREFIXES[0], fallback]), b"hunter2");
        assert_eq!(&*settings_secret(&token, fallback).unwrap(), "hunter2");
        assert!(settings_secret(&token, b"/home/you").is_none());
    }

    #[test]
    fn snapshot_envelopes() {
        let payload = b"PK\x03\x04 payload";
        let sealed = seal::snapshot(" secret ", payload);
        assert_eq!(&open_snapshot("secret", &sealed).unwrap()[..], payload);
        assert_eq!(
            open_snapshot("wrong", &sealed).unwrap_err(),
            EnvelopeError::WrongPassword
        );
        let mut tampered = sealed.clone();
        tampered[10] ^= 1;
        assert_eq!(
            open_snapshot("secret", &tampered).unwrap_err(),
            EnvelopeError::Unsupported,
            "KDF 参数不是 NyaTerm 写出的那一组"
        );
        let legacy = seal::legacy_snapshot("secret", payload);
        assert_eq!(&open_snapshot("secret", &legacy).unwrap()[..], payload);
        assert_eq!(
            open_snapshot("wrong", &legacy).unwrap_err(),
            EnvelopeError::WrongPassword
        );
        assert_eq!(
            open_snapshot("secret", b"short").unwrap_err(),
            EnvelopeError::Unsupported
        );
    }
}
