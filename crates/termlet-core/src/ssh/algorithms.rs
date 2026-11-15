//! 算法偏好：默认只用安全的算法；"兼容旧设备"时在后面追加旧算法（仍优先协商安全的）。
//! 主机密钥算法把已记录的那种排在最前，避免服务器新增密钥类型被误报为指纹变化。

use std::borrow::Cow;

use russh::keys::Algorithm;
use russh::{Preferred, cipher, kex, mac};

/// 默认的密钥交换：在 russh 的安全列表中补上 ECDH（OpenSSH 的默认列表有它，部分服务器不支持 curve25519）。
fn default_kex() -> Vec<kex::Name> {
    let mut list: Vec<kex::Name> = Preferred::DEFAULT.kex.iter().copied().collect();
    let position = list
        .iter()
        .position(|n| *n == kex::DH_GEX_SHA256)
        .unwrap_or(list.len());
    for (offset, name) in [
        kex::ECDH_SHA2_NISTP256,
        kex::ECDH_SHA2_NISTP384,
        kex::ECDH_SHA2_NISTP521,
    ]
    .into_iter()
    .enumerate()
    {
        list.insert(position + offset, name);
    }
    list
}

/// 一跳的算法偏好。`known` 是这台主机已记录的主机密钥算法名。
pub fn preferred(legacy: bool, known: &[String]) -> Preferred {
    let mut kex = default_kex();
    let mut cipher: Vec<cipher::Name> = Preferred::DEFAULT.cipher.to_vec();
    let mut mac: Vec<mac::Name> = Preferred::DEFAULT.mac.to_vec();
    let mut key: Vec<Algorithm> = Preferred::DEFAULT.key.to_vec();

    if legacy {
        // 扩展标记要留在最后，旧算法插在它们前面
        let extensions = kex
            .iter()
            .position(|n| *n == kex::EXTENSION_SUPPORT_AS_CLIENT)
            .unwrap_or(kex.len());
        for (offset, name) in [kex::DH_G14_SHA1, kex::DH_GEX_SHA1, kex::DH_G1_SHA1]
            .into_iter()
            .enumerate()
        {
            kex.insert(extensions + offset, name);
        }
        cipher.extend([
            cipher::AES_128_CBC,
            cipher::AES_192_CBC,
            cipher::AES_256_CBC,
            cipher::TRIPLE_DES_CBC,
        ]);
        mac.extend([mac::HMAC_SHA1_ETM, mac::HMAC_SHA1]);
        key.push(Algorithm::Dsa);
    }

    // 已记录的算法排在最前。RSA 的公钥记为 ssh-rsa（密钥类型），请求时展开成三种签名算法，
    // 仍优先 SHA-2，避免被降级为 SHA-1 签名，也避免不再接受 ssh-rsa 的服务器改给别的类型
    let known: Vec<&str> = known
        .iter()
        .flat_map(|name| match name.as_str() {
            "ssh-rsa" => vec!["rsa-sha2-512", "rsa-sha2-256", "ssh-rsa"],
            other => vec![other],
        })
        .collect();
    for name in known.iter().rev() {
        if let Some(position) = key.iter().position(|a| a.as_str() == *name) {
            let algorithm = key.remove(position);
            key.insert(0, algorithm);
        }
    }

    Preferred {
        kex: Cow::Owned(kex),
        key: Cow::Owned(key),
        cipher: Cow::Owned(cipher),
        mac: Cow::Owned(mac),
        ..Preferred::DEFAULT
    }
}

/// 请求主机证书（这台主机适用某个信任的 CA 时）：证书类型按主机密钥算法的顺序（已记录的在前），
/// 不含 DSA 与 SHA-1 签名的 RSA。russh 把它们排在普通的主机密钥算法前面。
pub fn with_host_certificates(mut preferred: Preferred) -> Preferred {
    let certificates: Vec<Algorithm> = preferred
        .key
        .iter()
        .filter(|a| {
            matches!(
                a,
                Algorithm::Ed25519 | Algorithm::Ecdsa { .. } | Algorithm::Rsa { hash: Some(_) }
            )
        })
        .cloned()
        .collect();
    preferred.host_key_certificates = Cow::Owned(certificates);
    preferred
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::keys::{EcdsaCurve, HashAlg};

    #[test]
    fn legacy_algorithms_are_appended() {
        let normal = preferred(false, &[]);
        assert!(!normal.cipher.contains(&cipher::AES_128_CBC));
        assert!(normal.kex.contains(&kex::ECDH_SHA2_NISTP256));
        let legacy = preferred(true, &[]);
        assert_eq!(
            legacy.cipher[0],
            cipher::CHACHA20_POLY1305,
            "安全的算法仍在前面"
        );
        assert!(legacy.cipher.contains(&cipher::TRIPLE_DES_CBC));
        assert!(legacy.kex.contains(&kex::DH_G1_SHA1));
        let ext = legacy
            .kex
            .iter()
            .position(|n| *n == kex::EXTENSION_SUPPORT_AS_CLIENT)
            .unwrap();
        let old = legacy
            .kex
            .iter()
            .position(|n| *n == kex::DH_G1_SHA1)
            .unwrap();
        assert!(old < ext, "扩展标记留在最后");
    }

    #[test]
    fn known_algorithm_comes_first() {
        let p = preferred(false, &["rsa-sha2-512".into()]);
        assert_eq!(
            p.key[0],
            Algorithm::Rsa {
                hash: Some(HashAlg::Sha512)
            }
        );
        // RSA 记录为 ssh-rsa：SHA-2 签名仍排在 SHA-1 前面
        let p = preferred(false, &["ssh-rsa".into()]);
        assert_eq!(
            &p.key[..3],
            &[
                Algorithm::Rsa {
                    hash: Some(HashAlg::Sha512)
                },
                Algorithm::Rsa {
                    hash: Some(HashAlg::Sha256)
                },
                Algorithm::Rsa { hash: None },
            ]
        );
        let p = preferred(false, &["ecdsa-sha2-nistp256".into()]);
        assert_eq!(
            p.key[0],
            Algorithm::Ecdsa {
                curve: EcdsaCurve::NistP256
            }
        );
    }
}
