//! 极简 DER 读写：只够识别 PKCS#1、PKCS#8、SEC1 私钥的结构与算法标识，
//! 以及把 PKCS#1 包成 PKCS#8 交给 russh 解析。真正的解析与校验由 russh（rsa、p256 等）完成。

use zeroize::Zeroizing;

pub(super) const INTEGER: u8 = 0x02;
#[cfg(test)]
pub(super) const BIT_STRING: u8 = 0x03;
pub(super) const OCTET_STRING: u8 = 0x04;
pub(super) const NULL: u8 = 0x05;
pub(super) const OID: u8 = 0x06;
pub(super) const SEQUENCE: u8 = 0x30;
/// `[0]`（显式标记，构造类型）
pub(super) const CONTEXT_0: u8 = 0xa0;
/// `[1]`（显式标记，构造类型）
#[cfg(test)]
pub(super) const CONTEXT_1: u8 = 0xa1;

/// 算法与曲线的 OID（DER 编码后的内容部分，不含标记与长度）。
pub(super) mod oid {
    /// rsaEncryption，1.2.840.113549.1.1.1
    pub const RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
    /// id-ecPublicKey，1.2.840.10045.2.1
    pub const EC_PUBLIC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
    /// id-dsa，1.2.840.10040.4.1
    pub const DSA: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x38, 0x04, 0x01];
    /// id-Ed25519，1.3.101.112
    pub const ED25519: &[u8] = &[0x2b, 0x65, 0x70];
    /// id-Ed448，1.3.101.113
    pub const ED448: &[u8] = &[0x2b, 0x65, 0x71];
    /// NIST P-256（secp256r1），1.2.840.10045.3.1.7
    pub const P256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
    /// NIST P-384（secp384r1），1.3.132.0.34
    pub const P384: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x22];
    /// NIST P-521（secp521r1），1.3.132.0.35
    pub const P521: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x23];
}

/// 顺序读取 DER 元素。只接受定长编码；任何越界、不认识的长度都返回 `None`。
pub(super) struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    pub(super) fn new(data: &'a [u8]) -> Self {
        Self(data)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(super) fn peek_tag(&self) -> Option<u8> {
        self.0.first().copied()
    }

    /// 读取下一个元素，返回标记与内容。
    pub(super) fn read_any(&mut self) -> Option<(u8, &'a [u8])> {
        let (&tag, rest) = self.0.split_first()?;
        let (&first, mut rest) = rest.split_first()?;
        let len = match first {
            0..=0x7f => usize::from(first),
            0x81..=0x84 => {
                let n = usize::from(first & 0x7f);
                if rest.len() < n {
                    return None;
                }
                let (bytes, after) = rest.split_at(n);
                rest = after;
                bytes
                    .iter()
                    .fold(0usize, |acc, &b| (acc << 8) | usize::from(b))
            }
            // 0x80 是不定长（BER），其余长度超过 4 字节，私钥里都不会出现
            _ => return None,
        };
        if rest.len() < len {
            return None;
        }
        let (content, after) = rest.split_at(len);
        self.0 = after;
        Some((tag, content))
    }

    /// 读取下一个元素，要求标记为 `tag`，返回内容。
    pub(super) fn read(&mut self, tag: u8) -> Option<&'a [u8]> {
        match self.read_any()? {
            (t, content) if t == tag => Some(content),
            _ => None,
        }
    }
}

/// 编码一个元素。
pub(super) fn write(tag: u8, content: &[u8]) -> Zeroizing<Vec<u8>> {
    let mut out = Zeroizing::new(Vec::with_capacity(content.len() + 6));
    out.push(tag);
    let len = content.len();
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes = len.to_be_bytes();
        let skip = bytes.iter().take_while(|&&b| b == 0).count();
        out.push(0x80 | (bytes.len() - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
    out.extend_from_slice(content);
    out
}

/// 把多个已编码的元素拼成 SEQUENCE。
pub(super) fn sequence(items: &[&[u8]]) -> Zeroizing<Vec<u8>> {
    let content = Zeroizing::new(items.concat());
    write(SEQUENCE, &content)
}

/// 把 PKCS#1 RSAPrivateKey 包成 PKCS#8 PrivateKeyInfo（版本 0，算法 rsaEncryption）。
pub(super) fn wrap_pkcs1_in_pkcs8(pkcs1: &[u8]) -> Zeroizing<Vec<u8>> {
    let version = write(INTEGER, &[0]);
    let algorithm = sequence(&[&write(OID, oid::RSA), &write(NULL, &[])]);
    let private_key = write(OCTET_STRING, pkcs1);
    sequence(&[&version, &algorithm, &private_key])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_and_read_lengths() {
        for len in [0usize, 1, 0x7f, 0x80, 0xff, 0x100, 0x1_0000] {
            let content = vec![0xab; len];
            let encoded = write(OCTET_STRING, &content);
            let mut r = Reader::new(&encoded);
            assert_eq!(r.read(OCTET_STRING), Some(&content[..]), "长度 {len}");
            assert!(r.is_empty());
        }
    }

    #[test]
    fn rejects_truncated_and_indefinite() {
        for bad in [
            &[][..],
            &[0x30],
            &[0x30, 0x05, 0x01],
            &[0x30, 0x80, 0x00, 0x00],
            &[0x30, 0x85, 0, 0, 0, 0, 1],
            &[0x30, 0x82, 0x01],
        ] {
            assert_eq!(Reader::new(bad).read_any(), None, "{bad:02x?}");
        }
        // 标记不符
        assert_eq!(Reader::new(&[0x02, 0x01, 0x00]).read(SEQUENCE), None);
    }

    #[test]
    fn pkcs8_wrapper_layout() {
        let der = wrap_pkcs1_in_pkcs8(&[0x30, 0x00]);
        let mut outer = Reader::new(&der);
        let mut info = Reader::new(outer.read(SEQUENCE).unwrap());
        assert_eq!(info.read(INTEGER), Some(&[0u8][..]));
        let mut alg = Reader::new(info.read(SEQUENCE).unwrap());
        assert_eq!(alg.read(OID), Some(oid::RSA));
        assert_eq!(alg.read(NULL), Some(&[][..]));
        assert_eq!(info.read(OCTET_STRING), Some(&[0x30u8, 0x00][..]));
        assert!(info.is_empty() && outer.is_empty());
    }
}
