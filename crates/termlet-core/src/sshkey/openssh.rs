//! OpenSSH 私钥（`openssh-key-v1`）。解析与解密由 ssh-key 完成；这里先读外层结构，
//! 把不支持的算法、加密方式在要私钥密码之前就区分出来，并把 ssh-key 的错误归到 [`KeyError`]。

use ssh_key::encoding::{Decode, Reader};
use ssh_key::{Cipher, Error as SshKeyError, PrivateKey};

use super::{KeyError, Malformed, Unsupported, check_algorithm_name, check_public_key};

const MAGIC: &[u8] = b"openssh-key-v1\0";

/// bcrypt 轮数上限。ssh-keygen 默认 16 轮，`-a 100` 已算很高；限制轮数避免恶意文件长时间占用 CPU（1024 轮约十秒）。
const MAX_BCRYPT_ROUNDS: u32 = 1 << 10;

/// 解析 OpenSSH 私钥的二进制内容（PEM 解码之后），加密的用 `passphrase` 解密。
pub(super) fn decode(bin: &[u8], passphrase: Option<&str>) -> Result<PrivateKey, KeyError> {
    let header = read_header(bin).ok_or(Malformed::Corrupted)?;
    if header.keys != 1 {
        return Err(Malformed::Corrupted.into());
    }
    check_algorithm_name(&header.algorithm)?;

    let encrypted = match (header.cipher.as_str(), header.kdf.as_str()) {
        ("none", "none") => false,
        // 未加密却声明了 KDF：结构不对
        ("none", _) => return Err(Malformed::Corrupted.into()),
        (cipher, "bcrypt") => {
            if !can_decrypt(cipher) {
                return Err(Unsupported::OpenSshCipher.into());
            }
            let rounds = bcrypt_rounds(&header.kdf_options).ok_or(Malformed::Corrupted)?;
            if rounds > MAX_BCRYPT_ROUNDS {
                return Err(Unsupported::KdfTooCostly.into());
            }
            true
        }
        _ => return Err(Unsupported::OpenSshCipher.into()),
    };

    let key = PrivateKey::from_bytes(bin).map_err(|_| Malformed::Corrupted)?;
    // 解密前先看公钥：RSA 位数过小时不必再问私钥密码
    check_public_key(key.public_key())?;
    if !encrypted {
        return Ok(key);
    }
    let passphrase = passphrase.ok_or(KeyError::PassphraseRequired)?;
    key.decrypt(passphrase).map_err(|err| match err {
        // 两个 checkint 不一致（CTR、CBC），或 GCM / ChaCha20-Poly1305 的认证标签不对
        SshKeyError::Crypto => KeyError::WrongPassphrase,
        _ => Malformed::Corrupted.into(),
    })
}

/// 外层结构中用得到的字段。
struct Header {
    cipher: String,
    kdf: String,
    kdf_options: Vec<u8>,
    keys: u32,
    /// 第一把公钥的算法名
    algorithm: String,
}

fn read_header(bin: &[u8]) -> Option<Header> {
    let mut r = bin.strip_prefix(MAGIC)?;
    let cipher = String::decode(&mut r).ok()?;
    let kdf = String::decode(&mut r).ok()?;
    let kdf_options = Vec::<u8>::decode(&mut r).ok()?;
    let keys = u32::decode(&mut r).ok()?;
    let public = Vec::<u8>::decode(&mut r).ok()?;
    let algorithm = String::decode(&mut public.as_slice()).ok()?;
    Some(Header {
        cipher,
        kdf,
        kdf_options,
        keys,
        algorithm,
    })
}

/// bcrypt 的参数：string salt、uint32 rounds。
fn bcrypt_rounds(options: &[u8]) -> Option<u32> {
    let mut r = options;
    Vec::<u8>::decode(&mut r).ok()?;
    let rounds = u32::decode(&mut r).ok()?;
    r.is_finished().then_some(rounds)
}

/// ssh-key 能解密的加密算法（3des-cbc 需要另开特性，没有开）。
fn can_decrypt(name: &str) -> bool {
    matches!(
        Cipher::new(name),
        Ok(Cipher::Aes128Cbc
            | Cipher::Aes192Cbc
            | Cipher::Aes256Cbc
            | Cipher::Aes128Ctr
            | Cipher::Aes192Ctr
            | Cipher::Aes256Ctr
            | Cipher::Aes128Gcm
            | Cipher::Aes256Gcm
            | Cipher::ChaCha20Poly1305)
    )
}
