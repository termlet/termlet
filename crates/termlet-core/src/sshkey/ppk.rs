//! PuTTY 私钥文件（.ppk）v2 与 v3。格式见 PuTTY 文档附录 C「PPK file format」。
//!
//! 不用 ssh-key 自带的 ppk 解析：它把 Ed25519 私钥当大端 mpint 读（PuTTY 存的是小端整数，
//! 严格的 mpint 校验还会拒绝一部分正常文件），不校验私钥与公钥是否一致，不限制 Argon2 参数，
//! 也不区分"私钥密码错误"与"文件损坏"。

use ssh_key::encoding::{Decode, Encode, Mpint};
use ssh_key::private::{
    EcdsaKeypair, EcdsaPrivateKey, Ed25519Keypair, Ed25519PrivateKey, KeypairData, RsaKeypair,
    RsaPrivateKey,
};
use ssh_key::public::{EcdsaPublicKey, KeyData};
use ssh_key::{Cipher, PrivateKey, PublicKey};
use zeroize::Zeroizing;

use super::pem::BASE64_LENIENT;
use super::{KeyError, Malformed, Unsupported, check_algorithm_name, check_public_key, legacy};

/// 文件开头。
pub(super) const MAGIC: &[u8] = b"PuTTY-User-Key-File-";

/// Argon2 参数上限，防止恶意文件让 KDF 占用过多内存或时间。PuTTYgen 默认 8192 KiB、二三十轮；
/// 内存不超过 256 MiB，内存 × 轮数不超过 8192 KiB × 1024 轮（最坏也只算几秒）。
const MAX_ARGON2_MEMORY: u64 = 1 << 18; // KiB
const MAX_ARGON2_PASSES: u64 = 1 << 10;
const MAX_ARGON2_WORK: u64 = 1 << 23; // KiB × 轮

/// v2 的 MAC 密钥前缀。
const V2_MAC_KEY_PREFIX: &[u8] = b"putty-private-key-file-mac-key";

/// 解析 ppk 私钥，加密的用 `passphrase` 解密。
pub(super) fn decode(data: &[u8], passphrase: Option<&str>) -> Result<PrivateKey, KeyError> {
    let file = PpkFile::read(data)?;
    check_algorithm_name(&file.algorithm)?;
    let public = file.public_key()?;
    // 解密前先看公钥：RSA 位数过小时不必再问私钥密码
    check_public_key(&public)?;
    file.open(public, passphrase)
}

/// 同 [`decode`]，但不检查 RSA 位数（测试中读取 512 位的 RSA 向量）。
#[cfg(test)]
pub(super) fn decode_unchecked(
    data: &[u8],
    passphrase: Option<&str>,
) -> Result<PrivateKey, KeyError> {
    let file = PpkFile::read(data)?;
    check_algorithm_name(&file.algorithm)?;
    let public = file.public_key()?;
    file.open(public, passphrase)
}

/// ppk 文件的外层字段。
struct PpkFile {
    version: u8,
    algorithm: String,
    encryption: String,
    comment: Vec<u8>,
    public: Vec<u8>,
    /// 文件中的私钥数据（加密时为密文）
    private: Zeroizing<Vec<u8>>,
    mac: Vec<u8>,
    /// v3 加密时的 Argon2 参数
    argon2: Option<Argon2Params>,
}

struct Argon2Params {
    kdf: String,
    memory: u32,
    passes: u32,
    parallelism: u32,
    salt: Vec<u8>,
}

/// 按行读取 ppk 文本：字段顺序固定，逐行核对字段名。
struct Lines<'a> {
    lines: Vec<&'a [u8]>,
    pos: usize,
}

impl<'a> Lines<'a> {
    /// 读取下一行，要求是 `name: value` 形式。
    fn field(&mut self, name: &str) -> Result<&'a [u8], Malformed> {
        let line = *self.lines.get(self.pos).ok_or(Malformed::Corrupted)?;
        let colon = line
            .iter()
            .position(|&b| b == b':')
            .ok_or(Malformed::Corrupted)?;
        if &line[..colon] != name.as_bytes() {
            return Err(Malformed::Corrupted);
        }
        self.pos += 1;
        let value = &line[colon + 1..];
        Ok(value.strip_prefix(b" ").unwrap_or(value))
    }

    fn text(&mut self, name: &str) -> Result<String, Malformed> {
        let value = self.field(name)?;
        String::from_utf8(value.to_vec()).map_err(|_| Malformed::Corrupted)
    }

    /// 读取十进制无符号整数，不超过 `max`。
    fn number(&mut self, name: &str, max: u64) -> Result<u64, Malformed> {
        let value = self.text(name)?;
        match value.parse::<u64>() {
            Ok(n) if n <= max && value.bytes().all(|b| b.is_ascii_digit()) => Ok(n),
            _ => Err(Malformed::Corrupted),
        }
    }

    /// 读取 `name: 行数` 及其后的 base64 行。
    fn blob(&mut self, name: &str) -> Result<Zeroizing<Vec<u8>>, Malformed> {
        let count = self.number(name, u64::from(u32::MAX))?;
        let count = usize::try_from(count).map_err(|_| Malformed::Corrupted)?;
        if count > self.lines.len() - self.pos {
            return Err(Malformed::Corrupted);
        }
        let mut text = Zeroizing::new(Vec::new());
        for line in &self.lines[self.pos..self.pos + count] {
            text.extend_from_slice(line.trim_ascii());
        }
        self.pos += count;
        BASE64_LENIENT
            .decode(&text)
            .map(Zeroizing::new)
            .map_err(|_| Malformed::Corrupted)
    }
}

impl PpkFile {
    /// 解析 ppk 文本结构（不解密）。
    fn read(data: &[u8]) -> Result<Self, KeyError> {
        // 统一换行：CRLF、CR 都当作 LF
        let mut text = Zeroizing::new(Vec::with_capacity(data.len()));
        let mut iter = data.iter().copied().peekable();
        while let Some(b) = iter.next() {
            if b == b'\r' {
                if iter.peek() == Some(&b'\n') {
                    iter.next();
                }
                text.push(b'\n');
            } else {
                text.push(b);
            }
        }
        let trimmed_len = text.len() - text.iter().rev().take_while(|&&b| b == b'\n').count();
        let mut r = Lines {
            lines: text[..trimmed_len].split(|&b| b == b'\n').collect(),
            pos: 0,
        };

        // 第一行：PuTTY-User-Key-File-<版本>: <算法>
        let first = r.lines[0];
        let colon = first
            .iter()
            .position(|&b| b == b':')
            .ok_or(Malformed::Corrupted)?;
        let version = first[..colon]
            .strip_prefix(MAGIC)
            .ok_or(Malformed::Corrupted)?;
        let version = match version {
            b"2" => 2,
            b"3" => 3,
            b"1" => return Err(Unsupported::PpkV1.into()),
            _ => return Err(Unsupported::PpkVersion.into()),
        };
        let algorithm = &first[colon + 1..];
        let algorithm = algorithm.strip_prefix(b" ").unwrap_or(algorithm);
        let algorithm = String::from_utf8(algorithm.to_vec()).map_err(|_| Malformed::Corrupted)?;
        r.pos = 1;

        let encryption = r.text("Encryption")?;
        if encryption != "none" && encryption != "aes256-cbc" {
            return Err(Unsupported::PpkCipher.into());
        }
        let comment = r.field("Comment")?.to_vec();
        let public = r.blob("Public-Lines")?.to_vec();
        let argon2 = if version == 3 && encryption != "none" {
            Some(read_argon2(&mut r)?)
        } else {
            None
        };
        let private = r.blob("Private-Lines")?;
        let mac = data_encoding::HEXLOWER_PERMISSIVE
            .decode(r.field("Private-MAC")?.trim_ascii())
            .map_err(|_| Malformed::Corrupted)?;
        Ok(Self {
            version,
            algorithm,
            encryption,
            comment,
            public,
            private,
            mac,
            argon2,
        })
    }

    fn encrypted(&self) -> bool {
        self.encryption != "none"
    }

    /// 解析文件中的公钥，并核对它的算法与文件头一致。
    fn public_key(&self) -> Result<PublicKey, KeyError> {
        let public = PublicKey::from_bytes(&self.public).map_err(|_| Malformed::Corrupted)?;
        if public.algorithm().as_str() != self.algorithm {
            return Err(Malformed::Corrupted.into());
        }
        Ok(public)
    }

    /// 解密并还原私钥。
    fn open(&self, public: PublicKey, passphrase: Option<&str>) -> Result<PrivateKey, KeyError> {
        let plain = self.decrypt(passphrase)?;
        private_key(self, public, &plain)
    }

    /// 校验 MAC 并返回明文私钥数据（含末尾的随机填充）。
    fn decrypt(&self, passphrase: Option<&str>) -> Result<Zeroizing<Vec<u8>>, KeyError> {
        let encrypted = self.encrypted();
        let passphrase = match passphrase {
            Some(p) if encrypted => p.as_bytes(),
            None if encrypted => return Err(KeyError::PassphraseRequired),
            _ => b"",
        };

        // v2：密钥 = SHA1(00000000 ‖ 密码) ‖ SHA1(00000001 ‖ 密码) 的前 32 字节，IV 全 0；
        //     MAC 密钥 = SHA1("putty-private-key-file-mac-key" ‖ 密码)（未加密时密码为空），HMAC-SHA1。
        // v3：Argon2 输出 80 字节 = 32 字节 AES 密钥 + 16 字节 IV + 32 字节 MAC 密钥；未加密时 MAC 密钥为空，HMAC-SHA256。
        let mut cipher_key = Zeroizing::new([0u8; 32]);
        let mut iv = [0u8; 16];
        let mac_key: Zeroizing<Vec<u8>> = match self.version {
            2 => {
                if encrypted {
                    let h0 = legacy::sha1(&[&[0, 0, 0, 0], passphrase]);
                    let h1 = legacy::sha1(&[&[0, 0, 0, 1], passphrase]);
                    cipher_key[..20].copy_from_slice(&h0);
                    cipher_key[20..].copy_from_slice(&h1[..12]);
                }
                Zeroizing::new(legacy::sha1(&[V2_MAC_KEY_PREFIX, passphrase]).to_vec())
            }
            _ => match &self.argon2 {
                Some(params) => {
                    let out = params.derive(passphrase)?;
                    cipher_key.copy_from_slice(&out[..32]);
                    iv.copy_from_slice(&out[32..48]);
                    Zeroizing::new(out[48..].to_vec())
                }
                None => Zeroizing::new(Vec::new()),
            },
        };

        let mut plain = self.private.clone();
        if encrypted {
            if plain.is_empty() || !plain.len().is_multiple_of(16) {
                return Err(Malformed::Corrupted.into());
            }
            Cipher::Aes256Cbc
                .decrypt(&cipher_key[..], &iv, &mut plain, None)
                .map_err(|_| Malformed::Corrupted)?;
        }

        // MAC 覆盖：string(算法) ‖ string(加密方式) ‖ string(注释) ‖ string(公钥) ‖ string(明文私钥，含填充)
        let mut message = Zeroizing::new(Vec::new());
        for field in [
            self.algorithm.as_bytes(),
            self.encryption.as_bytes(),
            &self.comment,
            &self.public,
            &plain,
        ] {
            field
                .encode(&mut *message)
                .map_err(|_| Malformed::Corrupted)?;
        }
        let valid = match self.version {
            2 => legacy::verify_hmac_sha1(&mac_key, &message, &self.mac),
            _ => legacy::verify_hmac_sha256(&mac_key, &message, &self.mac),
        };
        if !valid {
            return Err(if encrypted {
                KeyError::WrongPassphrase
            } else {
                Malformed::Corrupted.into()
            });
        }
        Ok(plain)
    }
}

/// 读取 v3 加密文件的 Argon2 参数。
fn read_argon2(r: &mut Lines<'_>) -> Result<Argon2Params, KeyError> {
    let kdf = r.text("Key-Derivation")?;
    let memory = r.number("Argon2-Memory", u64::from(u32::MAX))?;
    let passes = r.number("Argon2-Passes", u64::from(u32::MAX))?;
    let parallelism = r.number("Argon2-Parallelism", u64::from(u8::MAX))?;
    let salt = data_encoding::HEXLOWER_PERMISSIVE
        .decode(r.field("Argon2-Salt")?)
        .map_err(|_| Malformed::Corrupted)?;
    // Argon2 规范要求 parallelism ≥ 1、memory ≥ 8 × parallelism、passes ≥ 1
    if parallelism < 1 || passes < 1 || memory < 8 * parallelism {
        return Err(Malformed::Corrupted.into());
    }
    if memory > MAX_ARGON2_MEMORY || passes > MAX_ARGON2_PASSES || memory * passes > MAX_ARGON2_WORK
    {
        return Err(Unsupported::KdfTooCostly.into());
    }
    // 上面已限定范围，转换不会溢出
    Ok(Argon2Params {
        kdf,
        memory: memory as u32,
        passes: passes as u32,
        parallelism: parallelism as u32,
        salt,
    })
}

impl Argon2Params {
    /// 派生 80 字节密钥材料。
    fn derive(&self, passphrase: &[u8]) -> Result<Zeroizing<[u8; 80]>, KeyError> {
        let algorithm = match self.kdf.as_str() {
            "Argon2id" => argon2::Algorithm::Argon2id,
            "Argon2i" => argon2::Algorithm::Argon2i,
            "Argon2d" => argon2::Algorithm::Argon2d,
            _ => return Err(Unsupported::PpkKdf.into()),
        };
        let params = argon2::Params::new(self.memory, self.passes, self.parallelism, Some(80))
            .map_err(|_| Malformed::Corrupted)?;
        let mut out = Zeroizing::new([0u8; 80]);
        argon2::Argon2::new(algorithm, argon2::Version::V0x13, params)
            .hash_password_into(passphrase, &self.salt, &mut out[..])
            .map_err(|_| Malformed::Corrupted)?;
        Ok(out)
    }
}

/// 结合公钥与明文私钥数据还原私钥。私钥数据末尾可能有随机填充，读完需要的字段后忽略剩余部分。
fn private_key(file: &PpkFile, public: PublicKey, plain: &[u8]) -> Result<PrivateKey, KeyError> {
    let mut r = plain;
    let corrupted = |_| Malformed::Corrupted;
    let data = match public.key_data() {
        KeyData::Rsa(rsa) => {
            // mpint d、mpint p、mpint q、mpint iqmp；e、n 在公钥里
            let d = Mpint::decode(&mut r).map_err(corrupted)?;
            let p = Mpint::decode(&mut r).map_err(corrupted)?;
            let q = Mpint::decode(&mut r).map_err(corrupted)?;
            let iqmp = Mpint::decode(&mut r).map_err(corrupted)?;
            let private = RsaPrivateKey::new(d, iqmp, p, q).map_err(|_| Malformed::Corrupted)?;
            KeypairData::Rsa(
                RsaKeypair::new(rsa.clone(), private).map_err(|_| Malformed::Corrupted)?,
            )
        }
        KeyData::Ed25519(ed) => {
            // string 私钥：RFC 8032 的 32 字节种子，PuTTY 按小端整数存储，高位（末尾）的 0 字节可能被省略
            let seed = Zeroizing::new(Vec::<u8>::decode(&mut r).map_err(corrupted)?);
            if seed.is_empty() || seed.len() > Ed25519PrivateKey::BYTE_SIZE {
                return Err(Malformed::Corrupted.into());
            }
            let mut padded = Zeroizing::new([0u8; Ed25519PrivateKey::BYTE_SIZE]);
            padded[..seed.len()].copy_from_slice(&seed);
            let pair = Ed25519Keypair::from(Ed25519PrivateKey::from_bytes(&padded));
            if pair.public != *ed {
                return Err(Malformed::Corrupted.into());
            }
            KeypairData::Ed25519(pair)
        }
        KeyData::Ecdsa(ec) => {
            // mpint 私钥标量；公钥点在公钥里。私钥与公钥是否一致在 finish 中用签名验证
            let d = Mpint::decode(&mut r).map_err(corrupted)?;
            let d = d.as_positive_bytes().ok_or(Malformed::Corrupted)?;
            let d = &d[d.iter().take_while(|&&b| b == 0).count()..];
            if d.is_empty() {
                return Err(Malformed::Corrupted.into());
            }
            KeypairData::Ecdsa(ecdsa_keypair(ec, d).ok_or(Malformed::Corrupted)?)
        }
        // check_algorithm_name 已拒绝其他算法
        _ => return Err(Unsupported::Algorithm.into()),
    };
    let comment = String::from_utf8_lossy(&file.comment).into_owned();
    PrivateKey::new(data, comment).map_err(|_| Malformed::Corrupted.into())
}

/// 由公钥点与（去掉前导 0 的）私钥标量组成 ECDSA 密钥对；标量超出曲线长度时返回 `None`。
fn ecdsa_keypair(public: &EcdsaPublicKey, d: &[u8]) -> Option<EcdsaKeypair> {
    fn padded<const N: usize>(d: &[u8]) -> Option<Zeroizing<[u8; N]>> {
        let mut out = Zeroizing::new([0u8; N]);
        out.get_mut(N.checked_sub(d.len())?..)?.copy_from_slice(d);
        Some(out)
    }
    Some(match public {
        EcdsaPublicKey::NistP256(point) => EcdsaKeypair::NistP256 {
            public: *point,
            private: EcdsaPrivateKey::from(*padded::<32>(d)?),
        },
        EcdsaPublicKey::NistP384(point) => EcdsaKeypair::NistP384 {
            public: *point,
            private: EcdsaPrivateKey::from(*padded::<48>(d)?),
        },
        EcdsaPublicKey::NistP521(point) => EcdsaKeypair::NistP521 {
            public: *point,
            private: EcdsaPrivateKey::from(*padded::<66>(d)?),
        },
    })
}

#[cfg(test)]
mod tests {
    use ssh_key::HashAlg;
    use ssh_key::private::EcdsaKeypair;

    use super::*;
    use crate::sshkey::tests::{pass, read_pub, testdata};
    use crate::sshkey::{Key, KeyFormat, parse};

    /// 真实 PuTTY 生成的测试向量，来源见 testdata/README.md。
    const VECTORS: [(&str, &str, &str, &str); 10] = [
        // PuTTY 官方 test/cryptsuite.py testPPKLoadSave：（文件，密码（空为未加密），期望公钥，注释）
        (
            "putty/ed25519-v2.ppk",
            "",
            "putty/ed25519.pub",
            "ed25519-key-20200105",
        ),
        (
            "putty/ed25519-v2-aes.ppk",
            "test-passphrase",
            "putty/ed25519.pub",
            "ed25519-key-20200105",
        ),
        (
            "putty/ed25519-v3.ppk",
            "",
            "putty/ed25519.pub",
            "ed25519-key-20200105",
        ),
        (
            "putty/ed25519-v3-argon2id.ppk",
            "test-passphrase",
            "putty/ed25519.pub",
            "ed25519-key-20200105",
        ),
        // github.com/kayrus/putty 测试中由 puttygen 生成的文件
        (
            "kayrus/ed25519-v2-aes.ppk",
            "testkey",
            "kayrus/ed25519.pub",
            "a@b",
        ),
        (
            "kayrus/ecdsa256-v2-aes.ppk",
            "testkey",
            "kayrus/ecdsa256.pub",
            "a@b",
        ),
        (
            "kayrus/ecdsa384-v2-aes.ppk",
            "testkey",
            "kayrus/ecdsa384.pub",
            "a@b",
        ),
        (
            "kayrus/ecdsa521-v2-aes.ppk",
            "testkey",
            "kayrus/ecdsa521.pub",
            "a@b",
        ),
        ("kayrus/rsa2048-v3.ppk", "", "kayrus/rsa2048.pub", "a@b"),
        (
            "kayrus/rsa2048-v3-argon2id.ppk",
            "testkey",
            "kayrus/rsa2048.pub",
            "a@b",
        ),
    ];

    fn text(name: &str) -> String {
        String::from_utf8(testdata(name)).unwrap()
    }

    fn parse_str(data: &str, passphrase: &str) -> Result<Key, KeyError> {
        parse(data.as_bytes(), Some(&pass(passphrase)))
    }

    #[test]
    fn vectors() {
        for (file, passphrase, public, comment) in VECTORS {
            let data = testdata(file);
            let (fingerprint, line) = read_pub(public);
            let expected = PublicKey::from_openssh(&line).unwrap();

            let key =
                parse(&data, Some(&pass(passphrase))).unwrap_or_else(|e| panic!("{file}: {e}"));
            assert_eq!(key.format(), KeyFormat::Ppk, "{file}");
            assert_eq!(key.comment(), comment, "{file}");
            assert_eq!(key.algorithm(), expected.algorithm().as_str(), "{file}");
            assert_eq!(key.fingerprint(), fingerprint, "{file}");
            assert_eq!(key.public_key(), line, "{file}");
            let sig = key
                .ssh_private_key()
                .sign("t", HashAlg::Sha256, b"m")
                .unwrap();
            expected.verify("t", b"m", &sig).unwrap();

            // 转成 OpenSSH 后再解析，公钥与注释不变
            let again = parse(key.to_openssh().unwrap().expose().as_bytes(), None).unwrap();
            assert_eq!(again.format(), KeyFormat::OpenSsh, "{file}");
            assert_eq!(
                (again.fingerprint(), again.comment()),
                (fingerprint.clone(), comment),
                "{file}"
            );

            if passphrase.is_empty() {
                // 未加密时忽略密码
                parse(&data, Some(&pass("whatever"))).unwrap();
            } else {
                assert_eq!(
                    parse(&data, None).unwrap_err(),
                    KeyError::PassphraseRequired,
                    "{file}"
                );
                let wrong = pass(&format!("{passphrase}x"));
                assert_eq!(
                    parse(&data, Some(&wrong)).unwrap_err(),
                    KeyError::WrongPassphrase,
                    "{file}"
                );
            }
        }
    }

    /// PuTTY 官方四个文件是同一把密钥的不同保存形式，解出的私钥必须完全相同。
    #[test]
    fn putty_files_same_key() {
        let keys: Vec<Key> = VECTORS[..4]
            .iter()
            .map(|(file, passphrase, _, _)| {
                parse(&testdata(file), Some(&pass(passphrase))).unwrap()
            })
            .collect();
        for key in &keys[1..] {
            assert!(key.ssh_private_key().key_data() == keys[0].ssh_private_key().key_data());
        }
    }

    /// kayrus 的 RSA v2 向量只有 512 位：parse 按策略拒绝，但底层解析结果必须正确。
    #[test]
    fn rsa512() {
        let data = testdata("kayrus/rsa512-v2-aes.ppk");
        // 位数在解密前就检查，不必先问私钥密码
        for passphrase in [None, Some(pass("testkey"))] {
            assert_eq!(
                parse(&data, passphrase.as_ref()).unwrap_err(),
                Unsupported::RsaTooSmall { bits: 512 }.into()
            );
        }

        let (fingerprint, line) = read_pub("kayrus/rsa512.pub");
        let key = decode_unchecked(&data, Some("testkey")).unwrap();
        assert_eq!(key.fingerprint(HashAlg::Sha256).to_string(), fingerprint);
        assert_eq!(key.public_key().to_openssh().unwrap(), line);
        assert_eq!(
            decode_unchecked(&data, Some("wrong")).unwrap_err(),
            KeyError::WrongPassphrase
        );

        // 未加密的 v2 RSA：MAC 校验要通过
        let key = decode_unchecked(&testdata("kayrus/rsa512b-v2.ppk"), None).unwrap();
        assert_eq!(key.comment().as_str_lossy(), "a@b");
        assert_eq!(key.algorithm().as_str(), "ssh-rsa");
    }

    #[test]
    fn dsa_unsupported() {
        let data = testdata("kayrus/dsa-v2-aes.ppk");
        for passphrase in [None, Some(pass("testkey"))] {
            assert_eq!(
                parse(&data, passphrase.as_ref()).unwrap_err(),
                Unsupported::Dsa.into()
            );
        }
    }

    #[test]
    fn crlf_and_bom() {
        let crlf = text("putty/ed25519-v3-argon2id.ppk").replace('\n', "\r\n");
        parse_str(&format!("\u{feff}{crlf}"), "test-passphrase").unwrap();
        // 只有 CR 的换行（旧 Mac）也能读
        let cr = text("putty/ed25519-v3.ppk").replace('\n', "\r");
        parse_str(&cr, "").unwrap();
    }

    #[test]
    fn corrupted() {
        let plain = text("putty/ed25519-v3.ppk");
        let truncated = &plain[..plain.find("Private-Lines").unwrap()];
        let cases = [
            // MAC 被改：未加密文件视为损坏
            (
                "mac",
                plain.replacen("Private-MAC: 816c", "Private-MAC: 916c", 1),
            ),
            // 公钥被改（换成另一把 ed25519 公钥）：MAC 不匹配
            (
                "public",
                plain.replacen(
                    "AAAAC3NzaC1lZDI1NTE5AAAAIHJCszOH",
                    "AAAAC3NzaC1lZDI1NTE5AAAAIHJCszOI",
                    1,
                ),
            ),
            ("truncated", truncated.to_string()),
            (
                "bad-lines",
                plain.replacen("Public-Lines: 2", "Public-Lines: 99", 1),
            ),
            (
                "bad-count",
                plain.replacen("Public-Lines: 2", "Public-Lines: -2", 1),
            ),
            ("bad-base64", plain.replacen("JH1W", "JH1", 1)),
            ("bad-field", plain.replacen("Comment:", "Kommentar:", 1)),
            (
                "bad-mac-hex",
                plain.replacen("Private-MAC: 816c", "Private-MAC: zz6c", 1),
            ),
            (
                "no-colon",
                plain.replacen(
                    "PuTTY-User-Key-File-3: ssh-ed25519",
                    "PuTTY-User-Key-File-3",
                    1,
                ),
            ),
        ];
        for (name, data) in cases {
            assert_eq!(
                parse_str(&data, "").unwrap_err(),
                Malformed::Corrupted.into(),
                "{name}"
            );
        }

        let unsupported = [
            (
                "v1",
                plain.replacen("PuTTY-User-Key-File-3", "PuTTY-User-Key-File-1", 1),
                Unsupported::PpkV1,
            ),
            (
                "v4",
                plain.replacen("PuTTY-User-Key-File-3", "PuTTY-User-Key-File-4", 1),
                Unsupported::PpkVersion,
            ),
            (
                "encryption",
                plain.replacen("Encryption: none", "Encryption: aes128-ctr", 1),
                Unsupported::PpkCipher,
            ),
            (
                "ed448",
                plain.replacen("ssh-ed25519", "ssh-ed448", 1),
                Unsupported::Ed448,
            ),
        ];
        for (name, data, want) in unsupported {
            assert_eq!(parse_str(&data, "").unwrap_err(), want.into(), "{name}");
        }

        // Argon2 参数过大时直接拒绝，不执行 KDF
        let enc = text("putty/ed25519-v3-argon2id.ppk");
        let pw = "test-passphrase";
        for (from, to, want) in [
            (
                "Argon2-Memory: 8192",
                "Argon2-Memory: 4194304",
                KeyError::from(Unsupported::KdfTooCostly),
            ),
            (
                "Argon2-Passes: 13",
                "Argon2-Passes: 2000",
                Unsupported::KdfTooCostly.into(),
            ),
            // 内存与轮数各自不超过上限，乘积超过
            (
                "Argon2-Memory: 8192\nArgon2-Passes: 13",
                "Argon2-Memory: 262144\nArgon2-Passes: 1000",
                Unsupported::KdfTooCostly.into(),
            ),
            (
                "Argon2-Parallelism: 1",
                "Argon2-Parallelism: 0",
                Malformed::Corrupted.into(),
            ),
            (
                "Argon2-Parallelism: 1",
                "Argon2-Parallelism: 256",
                Malformed::Corrupted.into(),
            ),
            (
                "Argon2-Memory: 8192",
                "Argon2-Memory: 4",
                Malformed::Corrupted.into(),
            ),
            (
                "Key-Derivation: Argon2id",
                "Key-Derivation: scrypt",
                Unsupported::PpkKdf.into(),
            ),
            (
                "Argon2-Salt: 37c3",
                "Argon2-Salt: 37c",
                Malformed::Corrupted.into(),
            ),
        ] {
            assert_eq!(
                parse_str(&enc.replacen(from, to, 1), pw).unwrap_err(),
                want,
                "{to}"
            );
        }
        // 加密的私钥数据长度不是 16 的整数倍
        let short = enc.replacen(
            "amviz4sVUBN64jLO3gt4HGXJosUArghc4Soi7aVVLb2Tir5Baj0OQClorycuaPRd",
            "amviz4sVUBN64jLO3gt4HGXJosUArghc4Soi7aVVLb2Tir5Baj0OQClo",
            1,
        );
        assert_eq!(
            parse_str(&short, pw).unwrap_err(),
            Malformed::Corrupted.into()
        );
    }

    /// 按 PuTTY 的 ppk_save 算法生成 ppk（填充取未填充私钥数据的 SHA-1，与 PuTTY 一致，因此输出是确定的）。
    /// 先与真实 PuTTY 文件逐字节比对证明它正确，再用它构造没有现成向量的变体。
    #[allow(clippy::too_many_arguments)]
    fn encode_ppk(
        version: u8,
        algorithm: &str,
        comment: &str,
        public: &[u8],
        private: &[u8],
        passphrase: &str,
        kdf: &str,
        salt: &[u8],
    ) -> String {
        let (memory, passes, parallelism) = (8192u32, 13u32, 1u32);
        let (encryption, block) = if passphrase.is_empty() {
            ("none", 1)
        } else {
            ("aes256-cbc", 16)
        };
        let mut padded = private.to_vec();
        let hash = legacy::sha1(&[private]);
        let pad = private.len().div_ceil(block) * block - private.len();
        padded.extend_from_slice(&hash[..pad]);

        let mut key = [0u8; 32];
        let mut iv = [0u8; 16];
        let mac_key = if version == 2 {
            if !passphrase.is_empty() {
                let h0 = legacy::sha1(&[&[0, 0, 0, 0], passphrase.as_bytes()]);
                let h1 = legacy::sha1(&[&[0, 0, 0, 1], passphrase.as_bytes()]);
                key.copy_from_slice(&[&h0[..], &h1[..12]].concat());
            }
            let mac_pass = if passphrase.is_empty() {
                ""
            } else {
                passphrase
            };
            legacy::sha1(&[V2_MAC_KEY_PREFIX, mac_pass.as_bytes()]).to_vec()
        } else if passphrase.is_empty() {
            Vec::new()
        } else {
            let algorithm = match kdf {
                "Argon2id" => argon2::Algorithm::Argon2id,
                "Argon2i" => argon2::Algorithm::Argon2i,
                _ => argon2::Algorithm::Argon2d,
            };
            let params = argon2::Params::new(memory, passes, parallelism, Some(80)).unwrap();
            let mut out = [0u8; 80];
            argon2::Argon2::new(algorithm, argon2::Version::V0x13, params)
                .hash_password_into(passphrase.as_bytes(), salt, &mut out)
                .unwrap();
            key.copy_from_slice(&out[..32]);
            iv.copy_from_slice(&out[32..48]);
            out[48..].to_vec()
        };
        let mut message = Vec::new();
        for field in [
            algorithm.as_bytes(),
            encryption.as_bytes(),
            comment.as_bytes(),
            public,
            &padded,
        ] {
            field.encode(&mut message).unwrap();
        }
        let mac = legacy::hmac(version, &mac_key, &message);
        if !passphrase.is_empty() {
            Cipher::Aes256Cbc.encrypt(&key, &iv, &mut padded).unwrap();
        }

        let lines = |bytes: &[u8]| -> Vec<String> {
            let encoded = data_encoding::BASE64.encode(bytes);
            encoded
                .as_bytes()
                .chunks(64)
                .map(|c| String::from_utf8(c.to_vec()).unwrap())
                .collect()
        };
        let hex = |bytes: &[u8]| data_encoding::HEXLOWER.encode(bytes);
        let mut out = format!(
            "PuTTY-User-Key-File-{version}: {algorithm}\nEncryption: {encryption}\nComment: {comment}\n"
        );
        let public_lines = lines(public);
        out += &format!(
            "Public-Lines: {}\n{}\n",
            public_lines.len(),
            public_lines.join("\n")
        );
        if version == 3 && !passphrase.is_empty() {
            out += &format!(
                "Key-Derivation: {kdf}\nArgon2-Memory: {memory}\nArgon2-Passes: {passes}\nArgon2-Parallelism: {parallelism}\nArgon2-Salt: {}\n",
                hex(salt)
            );
        }
        let private_lines = lines(&padded);
        out += &format!(
            "Private-Lines: {}\n{}\n",
            private_lines.len(),
            private_lines.join("\n")
        );
        out += &format!("Private-MAC: {}\n", hex(&mac));
        out
    }

    /// 未加密 v3 文件中的字段（私钥数据即明文）。
    fn plain_v3() -> PpkFile {
        PpkFile::read(&testdata("putty/ed25519-v3.ppk")).unwrap()
    }

    const SALT: [u8; 16] = [
        0x37, 0xc3, 0x91, 0x1b, 0xfe, 0xfc, 0x8c, 0x1d, 0x11, 0xec, 0x57, 0x96, 0x27, 0xd2, 0xb3,
        0xd9,
    ];
    const PASS: &str = "test-passphrase";

    /// 固定向量：三种 Argon2 派生的 80 字节（由 argon2 0.5 算出），升级加密库后必须不变。
    /// Argon2id 另由上面真实 PuTTY 生成的文件验证；Argon2i、Argon2d 没有现成的文件。
    #[test]
    fn argon2_fixed_vectors() {
        for (kdf, expected) in [
            (
                "Argon2id",
                "6fadb5e3c17fec1610c0b460cc4f990ae17ab6f2b067a89313ecdeb16312cba10fc119fbdf3f2d9fbe74acad10e7c6aed63464a0b2de1b5397bb725f185557c0225b1e14abb86db2c1e2ef939eed3aee",
            ),
            (
                "Argon2i",
                "b980759be2dd0dd4a7335bf4adede0087699592a5253be0312df688b3b372770c4ebcb714a435d76127dc5182a3b58363d66d59601224bbc9a60ce0de082c83b86a6d2951b2159b8fde687ba2a979083",
            ),
            (
                "Argon2d",
                "0346138cb20601249e28c025a8aa9678f38df8478127e77eb0cc987e13edd7a1ebca252e206c3e5734dbd94bdd4cd37373ce330e5aa18307160b536e0dccf0059a6a5a5e0bad055392f62801bdafb82a",
            ),
        ] {
            let params = Argon2Params {
                kdf: kdf.into(),
                memory: 8192,
                passes: 13,
                parallelism: 1,
                salt: SALT.to_vec(),
            };
            let out = params.derive(PASS.as_bytes()).unwrap();
            assert_eq!(data_encoding::HEXLOWER.encode(&out[..]), expected, "{kdf}");
        }
    }

    #[test]
    fn argon2_variants() {
        let f = plain_v3();
        let comment = String::from_utf8(f.comment.clone()).unwrap();
        let enc = |version: u8, passphrase: &str, kdf: &str| {
            encode_ppk(
                version,
                &f.algorithm,
                &comment,
                &f.public,
                &f.private,
                passphrase,
                kdf,
                &SALT,
            )
        };

        // 先证明生成器与真实 PuTTY 输出逐字节一致
        for (file, got) in [
            ("putty/ed25519-v3.ppk", enc(3, "", "")),
            ("putty/ed25519-v3-argon2id.ppk", enc(3, PASS, "Argon2id")),
            ("putty/ed25519-v2.ppk", enc(2, "", "")),
            ("putty/ed25519-v2-aes.ppk", enc(2, PASS, "")),
        ] {
            assert_eq!(got, text(file), "生成器输出与 {file} 不一致");
        }

        let (fingerprint, _) = read_pub("putty/ed25519.pub");
        // Argon2i、Argon2d 也支持
        for kdf in ["Argon2i", "Argon2d"] {
            let data = enc(3, PASS, kdf);
            assert_eq!(
                parse_str(&data, PASS).unwrap().fingerprint(),
                fingerprint,
                "{kdf}"
            );
            assert_eq!(
                parse_str(&data, "wrong").unwrap_err(),
                KeyError::WrongPassphrase,
                "{kdf}"
            );
        }
    }

    /// 由密钥对构造未加密的 v3 ppk（私钥部分由调用方给出）。
    fn ppk_with_private(public: &PublicKey, private: &[u8]) -> String {
        let blob = public.key_data().encode_vec().unwrap();
        encode_ppk(
            3,
            public.algorithm().as_str(),
            "edge",
            &blob,
            private,
            "",
            "",
            &[],
        )
    }

    fn ssh_string(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        bytes.encode(&mut out).unwrap();
        out
    }

    /// Ed25519 种子按小端存储：末尾的 0 字节被省略、开头是 0 字节（不是合法 mpint）时都能读；与公钥不符时拒绝。
    #[test]
    fn ed25519_seed_encoding() {
        for (name, seed) in [
            ("trailing-zero", {
                let mut s = [0x5au8; 32];
                s[31] = 0;
                s
            }),
            ("leading-zero", {
                let mut s = [0x11u8; 32];
                s[0] = 0;
                s
            }),
            ("high-bit", [0xf0u8; 32]),
        ] {
            let pair = Ed25519Keypair::from_seed(&seed);
            let public = PublicKey::from(KeyData::Ed25519(pair.public));
            let stored = if name == "trailing-zero" {
                &seed[..31]
            } else {
                &seed[..]
            };
            let data = ppk_with_private(&public, &ssh_string(stored));
            let key = parse_str(&data, "").unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(
                key.fingerprint(),
                public.fingerprint(HashAlg::Sha256).to_string(),
                "{name}"
            );
        }

        // 私钥与公钥不是同一把
        let a = Ed25519Keypair::from_seed(&[1; 32]);
        let public = PublicKey::from(KeyData::Ed25519(a.public));
        let data = ppk_with_private(&public, &ssh_string(&[2; 32]));
        assert_eq!(
            parse_str(&data, "").unwrap_err(),
            Malformed::Corrupted.into()
        );
        // 种子超过 32 字节、为空
        for bad in [&[1u8; 33][..], &[]] {
            let data = ppk_with_private(&public, &ssh_string(bad));
            assert_eq!(
                parse_str(&data, "").unwrap_err(),
                Malformed::Corrupted.into()
            );
        }
    }

    /// ECDSA、RSA 的私钥与公钥不一致时拒绝（MAC 本身是对的）。
    #[test]
    fn mismatched_private_key() {
        let mut rng = crate::sshkey::rng();
        let a = EcdsaKeypair::random(&mut rng, ssh_key::EcdsaCurve::NistP256).unwrap();
        let b = EcdsaKeypair::random(&mut rng, ssh_key::EcdsaCurve::NistP256).unwrap();
        let public = PublicKey::from(KeyData::Ecdsa(EcdsaPublicKey::from(&a)));
        let scalar = |pair: &EcdsaKeypair| {
            let mut out = Vec::new();
            Mpint::from_positive_bytes(pair.private_key_bytes())
                .encode(&mut out)
                .unwrap();
            out
        };
        parse_str(&ppk_with_private(&public, &scalar(&a)), "").unwrap();
        assert_eq!(
            parse_str(&ppk_with_private(&public, &scalar(&b)), "").unwrap_err(),
            Malformed::Corrupted.into()
        );
        // 标量为 0、超过曲线长度
        let zero = ssh_string(&[]);
        assert_eq!(
            parse_str(&ppk_with_private(&public, &zero), "").unwrap_err(),
            Malformed::Corrupted.into()
        );
        let mut long = Vec::new();
        Mpint::from_positive_bytes(&[0x7f; 33])
            .encode(&mut long)
            .unwrap();
        assert_eq!(
            parse_str(&ppk_with_private(&public, &long), "").unwrap_err(),
            Malformed::Corrupted.into()
        );

        // RSA：d 换成另一把密钥的
        let file = PpkFile::read(&testdata("kayrus/rsa2048-v3.ppk")).unwrap();
        let mut r = &file.private[..];
        let d = Mpint::decode(&mut r).unwrap();
        let p = Mpint::decode(&mut r).unwrap();
        let q = Mpint::decode(&mut r).unwrap();
        let iqmp = Mpint::decode(&mut r).unwrap();
        let mut bad_d = d.as_positive_bytes().unwrap().to_vec();
        bad_d[10] ^= 1;
        let mut private = Vec::new();
        for m in [&Mpint::from_positive_bytes(&bad_d), &p, &q, &iqmp] {
            m.encode(&mut private).unwrap();
        }
        let public = PublicKey::from_bytes(&file.public).unwrap();
        assert_eq!(
            parse_str(&ppk_with_private(&public, &private), "").unwrap_err(),
            Malformed::Corrupted.into()
        );
    }
}
