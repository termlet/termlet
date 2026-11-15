//! PEM 文本：OpenSSH 私钥（交给 [`super::openssh`]），以及 PKCS#1、PKCS#8、SEC1 与旧式加密 PEM。
//!
//! PEM 文本按 Go `encoding/pem` 的宽松规则读取（忽略前后内容与正文中的空白、CRLF 换行）。
//! DER 的解析与校验交给 russh（内部用 rsa、p256 等库）；这里只识别外层结构与算法，
//! 把不支持的算法与"内容损坏"区分开。

use std::sync::LazyLock;

use data_encoding::Encoding;
use ssh_key::{Cipher, PrivateKey};
use zeroize::Zeroizing;

use super::der::{self, Reader, oid};
use super::{KeyError, KeyFormat, Malformed, Unsupported, legacy, openssh};

/// 标准 base64，要求补齐，但不检查末尾多余的位（与 Go 的 `base64.StdEncoding` 相同）。
pub(super) static BASE64_LENIENT: LazyLock<Encoding> = LazyLock::new(|| {
    let mut spec = data_encoding::BASE64.specification();
    spec.check_trailing_bits = false;
    spec.encoding().expect("base64 规格有效")
});

/// 解析 PEM 文本中的私钥。
pub(super) fn decode(
    data: &[u8],
    passphrase: Option<&str>,
) -> Result<(PrivateKey, KeyFormat), KeyError> {
    let block = pick_block(data).ok_or(Malformed::Corrupted)?;
    let kind = match block.label.as_str() {
        "OPENSSH PRIVATE KEY" => {
            return Ok((
                openssh::decode(&block.body, passphrase)?,
                KeyFormat::OpenSsh,
            ));
        }
        "RSA PRIVATE KEY" => DerKind::Pkcs1,
        "PRIVATE KEY" => DerKind::Pkcs8,
        "EC PRIVATE KEY" => DerKind::Sec1,
        "ENCRYPTED PRIVATE KEY" => return Err(Unsupported::EncryptedPkcs8.into()),
        "DSA PRIVATE KEY" => return Err(Unsupported::Dsa.into()),
        "PUBLIC KEY" | "RSA PUBLIC KEY" | "EC PUBLIC KEY" | "SSH2 PUBLIC KEY" | "CERTIFICATE" => {
            return Err(Malformed::NotPrivateKey.into());
        }
        _ => return Err(Unsupported::PemType.into()),
    };
    Ok((decode_der(&block, kind, passphrase)?, KeyFormat::Pem))
}

/// 一个 PEM 块。
struct Block {
    label: String,
    /// 头部字段（`Proc-Type`、`DEK-Info` 等），键与值都去掉了首尾空白
    headers: Vec<(String, String)>,
    body: Zeroizing<Vec<u8>>,
}

impl Block {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// 取出私钥所在的块：有多个块时（例如证书与私钥放在一起）优先取第一个私钥块，否则取第一个块。
/// 一个完整的块都没有时返回 `None`。
fn pick_block(data: &[u8]) -> Option<Block> {
    let mut first = None;
    let mut rest = data;
    while let Some((block, after)) = next_block(rest) {
        if block.label.ends_with("PRIVATE KEY") {
            return Some(block);
        }
        first.get_or_insert(block);
        rest = after;
    }
    first
}

/// 读取 `data` 中的下一个块，返回块与其后的内容。
fn next_block(data: &[u8]) -> Option<(Block, &[u8])> {
    const BEGIN: &[u8] = b"-----BEGIN ";
    const DASHES: &[u8] = b"-----";
    let mut search = data;
    loop {
        let start = find(search, BEGIN)?;
        let after_begin = &search[start + BEGIN.len()..];
        let (line, rest) = split_line(after_begin);
        // BEGIN 行形如 `-----BEGIN 类型-----`，行尾只能有空白
        let Some(label_end) = find(line, DASHES) else {
            search = after_begin;
            continue;
        };
        if !line[label_end + DASHES.len()..].trim_ascii().is_empty() {
            search = after_begin;
            continue;
        }
        let label = String::from_utf8_lossy(&line[..label_end]).into_owned();

        // 头部：紧跟 BEGIN 行、含冒号的行
        let mut headers = Vec::new();
        let mut body_start = rest;
        loop {
            let (line, next) = split_line(body_start);
            let Some(colon) = line.iter().position(|&b| b == b':') else {
                break;
            };
            let key = String::from_utf8_lossy(line[..colon].trim_ascii()).into_owned();
            let value = String::from_utf8_lossy(line[colon + 1..].trim_ascii()).into_owned();
            headers.push((key, value));
            body_start = next;
        }

        let end_marker = [b"-----END ", label.as_bytes(), DASHES].concat();
        let Some(end) = find(body_start, &end_marker) else {
            search = after_begin;
            continue;
        };
        let text: Zeroizing<Vec<u8>> = Zeroizing::new(
            body_start[..end]
                .iter()
                .copied()
                .filter(|b| !b.is_ascii_whitespace())
                .collect(),
        );
        let Ok(body) = BASE64_LENIENT.decode(&text) else {
            search = after_begin;
            continue;
        };
        let after = &body_start[end + end_marker.len()..];
        return Some((
            Block {
                label,
                headers,
                body: Zeroizing::new(body),
            },
            after,
        ));
    }
}

/// 拆出第一行（不含换行符，去掉行尾的 `\r`）与其后的内容。
fn split_line(data: &[u8]) -> (&[u8], &[u8]) {
    let (line, rest) = match data.iter().position(|&b| b == b'\n') {
        Some(i) => (&data[..i], &data[i + 1..]),
        None => (data, &data[data.len()..]),
    };
    (line.strip_suffix(b"\r").unwrap_or(line), rest)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// PEM 块中 DER 的种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DerKind {
    /// PKCS#1 RSAPrivateKey
    Pkcs1,
    /// PKCS#8 PrivateKeyInfo（未加密）
    Pkcs8,
    /// SEC1 ECPrivateKey
    Sec1,
}

/// 外层结构检查的结果：不支持的算法，或根本不是这种结构。
enum Rejected {
    Unsupported(Unsupported),
    Garbage,
}

/// 解析 PKCS#1、PKCS#8、SEC1；旧式加密 PEM 先解密。
fn decode_der(
    block: &Block,
    kind: DerKind,
    passphrase: Option<&str>,
) -> Result<PrivateKey, KeyError> {
    let encrypted = block
        .header("Proc-Type")
        .is_some_and(|v| v.contains("ENCRYPTED"));
    let plain = if encrypted {
        let passphrase = passphrase.ok_or(KeyError::PassphraseRequired)?;
        let dek_info = block.header("DEK-Info").ok_or(Malformed::Corrupted)?;
        decrypt_legacy(&block.body, dek_info, passphrase)?
    } else {
        block.body.clone()
    };

    // 旧式加密 PEM 没有完整性校验，密码错误时解出的是随机数据：结构不对一律视为密码错误
    let broken = || -> KeyError {
        if encrypted {
            KeyError::WrongPassphrase
        } else {
            Malformed::Corrupted.into()
        }
    };
    match inspect(kind, &plain) {
        Ok(()) => {}
        Err(Rejected::Unsupported(reason)) => return Err(reason.into()),
        Err(Rejected::Garbage) => return Err(broken()),
    }
    let der = match kind {
        DerKind::Pkcs1 => der::wrap_pkcs1_in_pkcs8(&plain),
        DerKind::Pkcs8 | DerKind::Sec1 => plain,
    };
    russh::keys::pkcs8::decode_pkcs8(&der, None).map_err(|_| broken())
}

/// 检查 DER 的外层结构与算法标识。
fn inspect(kind: DerKind, data: &[u8]) -> Result<(), Rejected> {
    let mut outer = Reader::new(data);
    let mut seq = Reader::new(outer.read(der::SEQUENCE).ok_or(Rejected::Garbage)?);
    if !outer.is_empty() {
        return Err(Rejected::Garbage);
    }
    let version = seq.read(der::INTEGER).ok_or(Rejected::Garbage)?;
    match kind {
        DerKind::Pkcs1 => match version {
            [0] => seq.read(der::INTEGER).map(|_| ()).ok_or(Rejected::Garbage),
            // 版本 1 是多素数 RSA
            [1] => Err(Rejected::Unsupported(Unsupported::Algorithm)),
            _ => Err(Rejected::Garbage),
        },
        DerKind::Pkcs8 => {
            if !matches!(version, [0] | [1]) {
                return Err(Rejected::Garbage);
            }
            let mut algorithm = Reader::new(seq.read(der::SEQUENCE).ok_or(Rejected::Garbage)?);
            let id = algorithm.read(der::OID).ok_or(Rejected::Garbage)?;
            seq.read(der::OCTET_STRING).ok_or(Rejected::Garbage)?;
            let unsupported = match id {
                oid::RSA | oid::ED25519 => return Ok(()),
                oid::EC_PUBLIC_KEY => match algorithm.read(der::OID) {
                    Some(curve) => return check_curve(curve),
                    None => Unsupported::Curve,
                },
                oid::ED448 => Unsupported::Ed448,
                oid::DSA => Unsupported::Dsa,
                // RSA-PSS、X25519、X448 等
                _ => Unsupported::Algorithm,
            };
            Err(Rejected::Unsupported(unsupported))
        }
        DerKind::Sec1 => {
            if version != [1] {
                return Err(Rejected::Garbage);
            }
            seq.read(der::OCTET_STRING).ok_or(Rejected::Garbage)?;
            if seq.peek_tag() != Some(der::CONTEXT_0) {
                return Ok(());
            }
            let mut params = Reader::new(seq.read(der::CONTEXT_0).ok_or(Rejected::Garbage)?);
            match params.peek_tag() {
                Some(der::OID) => check_curve(params.read(der::OID).ok_or(Rejected::Garbage)?),
                // 完整的曲线参数（不是曲线名），russh 按素数长度识别 P-256/384/521
                Some(der::SEQUENCE) => Ok(()),
                _ => Err(Rejected::Garbage),
            }
        }
    }
}

fn check_curve(curve: &[u8]) -> Result<(), Rejected> {
    match curve {
        oid::P256 | oid::P384 | oid::P521 => Ok(()),
        _ => Err(Rejected::Unsupported(Unsupported::Curve)),
    }
}

/// 旧式加密 PEM 的加密算法（RFC 1421 / OpenSSL 的 `DEK-Info`），都是 CBC 模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LegacyCipher {
    Des,
    DesEde3,
    Aes128,
    Aes192,
    Aes256,
}

impl LegacyCipher {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "DES-CBC" => Self::Des,
            "DES-EDE3-CBC" => Self::DesEde3,
            "AES-128-CBC" => Self::Aes128,
            "AES-192-CBC" => Self::Aes192,
            "AES-256-CBC" => Self::Aes256,
            _ => return None,
        })
    }

    fn key_len(self) -> usize {
        match self {
            Self::Des => 8,
            Self::DesEde3 | Self::Aes192 => 24,
            Self::Aes128 => 16,
            Self::Aes256 => 32,
        }
    }

    fn block_size(self) -> usize {
        match self {
            Self::Des | Self::DesEde3 => 8,
            _ => 16,
        }
    }
}

/// 解密旧式加密 PEM 的正文：密钥由 OpenSSL 的 `EVP_BytesToKey`（MD5、1 次、盐为 IV 前 8 字节）派生，
/// CBC 解密后去掉 PKCS#7 填充。填充不对说明密码错误（与 Go `x509.DecryptPEMBlock` 相同）。
fn decrypt_legacy(
    body: &[u8],
    dek_info: &str,
    passphrase: &str,
) -> Result<Zeroizing<Vec<u8>>, KeyError> {
    let (name, iv_hex) = dek_info.split_once(',').ok_or(Malformed::Corrupted)?;
    let cipher = LegacyCipher::from_name(name.trim()).ok_or(Unsupported::PemCipher)?;
    let iv = data_encoding::HEXLOWER_PERMISSIVE
        .decode(iv_hex.trim().as_bytes())
        .map_err(|_| Malformed::Corrupted)?;
    let block_size = cipher.block_size();
    if iv.len() != block_size || body.is_empty() || !body.len().is_multiple_of(block_size) {
        return Err(Malformed::Corrupted.into());
    }
    let key = legacy::evp_bytes_to_key(passphrase.as_bytes(), &iv[..8], cipher.key_len());
    let mut plain = Zeroizing::new(body.to_vec());
    let ok = match cipher {
        LegacyCipher::Des => legacy::des_cbc_decrypt(&key, &iv, &mut plain),
        LegacyCipher::DesEde3 => legacy::des_ede3_cbc_decrypt(&key, &iv, &mut plain),
        LegacyCipher::Aes128 => Cipher::Aes128Cbc
            .decrypt(&key, &iv, &mut plain, None)
            .is_ok(),
        LegacyCipher::Aes192 => Cipher::Aes192Cbc
            .decrypt(&key, &iv, &mut plain, None)
            .is_ok(),
        LegacyCipher::Aes256 => Cipher::Aes256Cbc
            .decrypt(&key, &iv, &mut plain, None)
            .is_ok(),
    };
    if !ok {
        return Err(Malformed::Corrupted.into());
    }
    let pad = usize::from(plain[plain.len() - 1]);
    if pad == 0
        || pad > block_size
        || plain[plain.len() - pad..]
            .iter()
            .any(|&b| usize::from(b) != pad)
    {
        return Err(KeyError::WrongPassphrase);
    }
    let len = plain.len() - pad;
    plain.truncate(len);
    Ok(plain)
}
