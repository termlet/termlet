//! 读取旧格式私钥用到的散列、MAC 与分组密码：SHA-1、HMAC-SHA1 / HMAC-SHA256（ppk），
//! MD5 的 `EVP_BytesToKey`、DES / 3DES-EDE 的 CBC 解密（旧式加密 PEM）。
//!
//! 只用于解开用户已有的旧文件，不用于新的加密。AES-CBC 用 ssh-key 的 [`ssh_key::Cipher`]。

use cbc::cipher::block_padding::NoPadding;
use cbc::cipher::{BlockCipherDecrypt, BlockModeDecrypt, KeyIvInit};
use des::{Des, TdesEde3};
use hmac::{Hmac, KeyInit, Mac};
use md5::Md5;
use sha1::{Digest as _, Sha1};
use ssh_key::sha2::Sha256;
use zeroize::Zeroizing;

/// SHA-1，依次散列 `parts` 拼接后的内容。
pub(super) fn sha1(parts: &[&[u8]]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

/// 校验 HMAC-SHA1，比较耗时与内容无关。
pub(super) fn verify_hmac_sha1(key: &[u8], message: &[u8], mac: &[u8]) -> bool {
    verify_hmac::<Hmac<Sha1>>(key, message, mac)
}

/// 校验 HMAC-SHA256，比较耗时与内容无关。
pub(super) fn verify_hmac_sha256(key: &[u8], message: &[u8], mac: &[u8]) -> bool {
    verify_hmac::<Hmac<Sha256>>(key, message, mac)
}

fn verify_hmac<M: Mac + KeyInit>(key: &[u8], message: &[u8], mac: &[u8]) -> bool {
    // HMAC 接受任意长度的密钥，new_from_slice 不会失败
    let Ok(mut hmac) = <M as KeyInit>::new_from_slice(key) else {
        return false;
    };
    hmac.update(message);
    hmac.verify_slice(mac).is_ok()
}

/// OpenSSL 的 `EVP_BytesToKey`（MD5，迭代 1 次）：D₁ = MD5(密码 ‖ 盐)，Dᵢ = MD5(Dᵢ₋₁ ‖ 密码 ‖ 盐)，取前 `len` 字节。
pub(super) fn evp_bytes_to_key(passphrase: &[u8], salt: &[u8], len: usize) -> Zeroizing<Vec<u8>> {
    let mut key = Zeroizing::new(Vec::with_capacity(len + 16));
    let mut previous: Option<Zeroizing<[u8; 16]>> = None;
    while key.len() < len {
        let mut hasher = Md5::new();
        if let Some(prev) = &previous {
            hasher.update(&prev[..]);
        }
        hasher.update(passphrase);
        hasher.update(salt);
        let digest = Zeroizing::new(<[u8; 16]>::from(hasher.finalize()));
        key.extend_from_slice(&digest[..]);
        previous = Some(digest);
    }
    key.truncate(len);
    key
}

/// DES-CBC 解密（8 字节密钥），不处理填充。密钥、IV 或数据长度不对时返回 `false`。
pub(super) fn des_cbc_decrypt(key: &[u8], iv: &[u8], buf: &mut [u8]) -> bool {
    cbc_decrypt::<Des>(key, iv, buf)
}

/// 3DES-EDE-CBC 解密（24 字节密钥 K1 ‖ K2 ‖ K3），不处理填充。密钥、IV 或数据长度不对时返回 `false`。
pub(super) fn des_ede3_cbc_decrypt(key: &[u8], iv: &[u8], buf: &mut [u8]) -> bool {
    cbc_decrypt::<TdesEde3>(key, iv, buf)
}

fn cbc_decrypt<C: BlockCipherDecrypt + KeyInit>(key: &[u8], iv: &[u8], buf: &mut [u8]) -> bool {
    match cbc::Decryptor::<C>::new_from_slices(key, iv) {
        Ok(decryptor) => decryptor.decrypt_padded::<NoPadding>(buf).is_ok(),
        Err(_) => false,
    }
}

/// 测试用：HMAC-SHA1 / HMAC-SHA256，用于构造 ppk。
#[cfg(test)]
pub(super) fn hmac(version: u8, key: &[u8], message: &[u8]) -> Vec<u8> {
    fn compute<M: Mac + KeyInit>(key: &[u8], message: &[u8]) -> Vec<u8> {
        let mut hmac = <M as KeyInit>::new_from_slice(key).unwrap();
        hmac.update(message);
        hmac.finalize().into_bytes().to_vec()
    }
    if version == 2 {
        compute::<Hmac<Sha1>>(key, message)
    } else {
        compute::<Hmac<Sha256>>(key, message)
    }
}

/// 测试用：3DES-EDE-CBC 加密（不处理填充），用于构造旧式加密 PEM。
#[cfg(test)]
pub(super) fn des_ede3_cbc_encrypt(key: &[u8], iv: &[u8], buf: &mut [u8]) {
    use cbc::cipher::BlockModeEncrypt;
    let len = buf.len();
    cbc::Encryptor::<TdesEde3>::new_from_slices(key, iv)
        .unwrap()
        .encrypt_padded::<NoPadding>(buf, len)
        .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        data_encoding::HEXLOWER.encode(bytes)
    }

    fn unhex(s: &str) -> Vec<u8> {
        data_encoding::HEXLOWER_PERMISSIVE
            .decode(s.as_bytes())
            .unwrap()
    }

    #[test]
    fn sha1_parts() {
        // FIPS 180-4 示例；分段输入与整段相同
        assert_eq!(
            hex(&sha1(&[b"abc"])),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(sha1(&[b"a", b"", b"bc"]), sha1(&[b"abc"]));
    }

    #[test]
    fn hmac_verify() {
        // RFC 2202、RFC 4231 的第二组
        let message = b"what do ya want for nothing?";
        let sha1 = unhex("effcdf6ae5eb2fa2d27416d5f184df9c259a7c79");
        let sha256 = unhex("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
        assert!(verify_hmac_sha1(b"Jefe", message, &sha1));
        assert!(verify_hmac_sha256(b"Jefe", message, &sha256));
        assert!(!verify_hmac_sha1(b"jefe", message, &sha1));
        assert!(!verify_hmac_sha256(b"Jefe", message, &sha256[..31]));
        // 空密钥（未加密的 ppk v3）
        assert!(verify_hmac_sha256(b"", message, &hmac(3, b"", message)));
    }

    #[test]
    fn evp_bytes_to_key_matches_openssl() {
        // openssl enc -aes-256-cbc -k secret -S 0102030405060708 -P -md md5
        let key = evp_bytes_to_key(b"secret", &unhex("0102030405060708"), 32 + 16);
        assert_eq!(
            hex(&key[..32]),
            "c9e5a1bd216dbe1317e230cef48f38ee7f0e17ad64022144bccec4a1aa2879ab"
        );
        assert_eq!(hex(&key[32..]), "e24b32bbbc4ef02ecbcb6576523ad893");
    }

    #[test]
    fn des_cbc() {
        // FIPS 81 的 CBC 示例："Now is the time for all "
        let mut buf = unhex("e5c7cdde872bf27c43e934008c389c0f683788499a7c05f6");
        assert!(des_cbc_decrypt(
            &unhex("0123456789abcdef"),
            &unhex("1234567890abcdef"),
            &mut buf
        ));
        assert_eq!(buf, b"Now is the time for all ");
    }

    #[test]
    fn des_ede3_cbc() {
        // NIST SP 800-67 的示例（第一块，IV 为 0 时 CBC 与 ECB 相同）
        let key = unhex("0123456789abcdef23456789abcdef01456789abcdef0123");
        let mut buf = unhex("a826fd8ce53b855f");
        assert!(des_ede3_cbc_decrypt(&key, &[0; 8], &mut buf));
        assert_eq!(buf, b"The qufc");

        // CBC 往返
        let iv = unhex("0011223344556677");
        let mut buf = b"0123456789abcdef01234567".to_vec();
        des_ede3_cbc_encrypt(&key, &iv, &mut buf);
        assert!(des_ede3_cbc_decrypt(&key, &iv, &mut buf));
        assert_eq!(buf, b"0123456789abcdef01234567");

        // 密钥、IV、数据长度不对
        assert!(!des_ede3_cbc_decrypt(&key[..16], &iv, &mut buf));
        assert!(!des_ede3_cbc_decrypt(&key, &iv[..4], &mut buf));
        assert!(!des_ede3_cbc_decrypt(&key, &iv, &mut buf[..7]));
    }
}
