//! 用密码加密的文件：完整备份、含秘密的主机文件，以及以后云同步的数据文件。
//!
//! 格式：
//!
//! ```text
//! termlet-sealed/1\n
//! {"kind":"backup","kdf":"argon2id","time":3,"memory":65536,"threads":4,"salt":"…"}\n
//! 版本(1) ‖ nonce(12) ‖ AES-256-GCM 的密文与标签
//! ```
//!
//! - 前两行是明文：用文本编辑器打开能看出是什么文件。两行整体作为附加数据参与校验，改动后无法解开；
//!   类型以外的元数据（时间、数量）都在密文中。
//! - 密钥由用户的密码经 Argon2id（随机 16 字节盐）派生，参数与保险库的主密码相同（64 MB 内存、3 次迭代、4 线程），
//!   与本机的主密钥无关，换一台电脑也能解开；忘记密码就无法解开。
//! - 文件头中的派生参数在认证之前就要使用（先派生密钥才能校验文件），能把文件递给用户、能写同步目录的人都能改。
//!   只接受与本程序写出的相近的范围（内存最多 4 倍、迭代次数 × 内存最多 8 倍），构造的文件不能让每次读取
//!   分配大量内存、耗费很长时间。

use data_encoding::BASE64;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::BackupError;
use crate::secret::SecretString;
use crate::vault;

const MAGIC: &[u8] = b"termlet-sealed/1\n";

/// 备份、导出密码的最小长度。
pub const MIN_PASSWORD_LEN: usize = 8;

/// 读取加密文件的大小上限，防止误选超大文件耗尽内存。
pub const MAX_SEALED_SIZE: u64 = 512 << 20;

/// 文件头一行的长度上限。
const MAX_HEADER_LEN: usize = 4096;

/// 加密文件中的内容类型（在明文的文件头中）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SealedKind {
    /// 完整备份
    Backup,
    /// 导出的主机文件（含秘密）
    Hosts,
    /// 云同步的数据文件
    Sync,
}

impl SealedKind {
    fn name(self) -> &'static str {
        match self {
            SealedKind::Backup => "backup",
            SealedKind::Hosts => "hosts",
            SealedKind::Sync => "sync",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        [SealedKind::Backup, SealedKind::Hosts, SealedKind::Sync]
            .into_iter()
            .find(|k| k.name() == name)
    }
}

/// 派生参数。测试中用小参数（与保险库相同），校验的下限随之放低。
const DEFAULT_TIME: u32 = if cfg!(test) { 1 } else { 3 };
const DEFAULT_MEMORY_KIB: u32 = if cfg!(test) {
    MIN_MEMORY_KIB
} else {
    64 * 1024
};
const DEFAULT_THREADS: u32 = if cfg!(test) { 1 } else { 4 };

const MIN_MEMORY_KIB: u32 = if cfg!(test) { 8 } else { 8 * 1024 };
const MAX_TIME: u32 = 10;
const MAX_MEMORY_KIB: u32 = 4 * 64 * 1024;
/// 迭代次数 × 内存（KiB）的上限
const MAX_COST: u64 = 8 * 3 * 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Header {
    kind: String,
    kdf: String,
    time: u32,
    /// KiB
    memory: u32,
    threads: u32,
    /// base64
    salt: String,
}

impl Header {
    fn new(kind: SealedKind) -> Self {
        let mut salt = [0u8; 16];
        getrandom::fill(&mut salt).expect("系统随机数不可用");
        Self {
            kind: kind.name().into(),
            kdf: "argon2id".into(),
            time: DEFAULT_TIME,
            memory: DEFAULT_MEMORY_KIB,
            threads: DEFAULT_THREADS,
            salt: BASE64.encode(&salt),
        }
    }

    /// 派生参数是否在可以接受的范围内；返回盐。
    fn valid_salt(&self) -> Option<Vec<u8>> {
        let salt = BASE64.decode(self.salt.as_bytes()).ok()?;
        let ok = self.kdf == "argon2id"
            && (8..=64).contains(&salt.len())
            && (1..=MAX_TIME).contains(&self.time)
            && (MIN_MEMORY_KIB..=MAX_MEMORY_KIB).contains(&self.memory)
            && u64::from(self.time) * u64::from(self.memory) <= MAX_COST
            && (1..=16).contains(&self.threads);
        ok.then_some(salt)
    }

    fn derive(
        &self,
        salt: &[u8],
        password: &SecretString,
    ) -> Result<Zeroizing<[u8; 32]>, BackupError> {
        vault::derive_key(
            password.expose().as_bytes(),
            salt,
            self.memory,
            self.time,
            self.threads,
        )
        .map_err(|_| BackupError::Invalid)
    }
}

/// 数据是否为加密文件（看开头的标记）。
pub fn is_sealed(data: &[u8]) -> bool {
    data.starts_with(MAGIC)
}

/// 校验新设置的密码（至少 [`MIN_PASSWORD_LEN`] 个字符）。
pub fn check_password(password: &SecretString) -> Result<(), BackupError> {
    if password.expose().chars().count() < MIN_PASSWORD_LEN {
        return Err(BackupError::PasswordTooShort {
            min: MIN_PASSWORD_LEN,
        });
    }
    Ok(())
}

/// 用密码加密 `payload`，返回完整的文件内容。派生密钥耗时（约几百毫秒），在阻塞线程上调用。
pub fn seal(
    kind: SealedKind,
    password: &SecretString,
    payload: &[u8],
) -> Result<Vec<u8>, BackupError> {
    check_password(password)?;
    let header = Header::new(kind);
    let salt = header.valid_salt().ok_or(BackupError::Invalid)?;
    let key = header.derive(&salt, password)?;
    let line = serde_json::to_vec(&header).map_err(|e| BackupError::Store(e.to_string()))?;
    let mut head = MAGIC.to_vec();
    head.extend_from_slice(&line);
    head.push(b'\n');
    let body = vault::seal_with(&key, &head, payload);
    head.extend_from_slice(&body);
    Ok(head)
}

/// 文件头：明文的类型与派生参数，以及文件头的字节数。
fn header(data: &[u8]) -> Result<(Header, SealedKind, usize), BackupError> {
    let rest = data.strip_prefix(MAGIC).ok_or(BackupError::Invalid)?;
    let end = rest
        .iter()
        .take(MAX_HEADER_LEN + 1)
        .position(|&b| b == b'\n')
        .ok_or(BackupError::Invalid)?;
    let header: Header = serde_json::from_slice(&rest[..end]).map_err(|_| BackupError::Invalid)?;
    let kind = SealedKind::from_name(&header.kind).ok_or(BackupError::Invalid)?;
    Ok((header, kind, MAGIC.len() + end + 1))
}

/// 不解密，只看文件头中的类型；不是加密文件或文件头损坏时为 `None`。
pub fn peek_kind(data: &[u8]) -> Option<SealedKind> {
    header(data).ok().map(|(_, kind, _)| kind)
}

/// 用密码解开加密文件，返回内容类型与内容。
///
/// 不是加密文件、文件头损坏、派生参数超出范围时返回 [`BackupError::Invalid`]；没有密码（`None` 或空）时返回
/// [`BackupError::PasswordRequired`]；密码错误或文件被改动时返回 [`BackupError::WrongPassword`]。
pub fn open(
    data: &[u8],
    password: Option<&SecretString>,
) -> Result<(SealedKind, Zeroizing<Vec<u8>>), BackupError> {
    let (header, kind, head_len) = header(data)?;
    // 参数来自文件，认证之前只接受合理的范围
    let salt = header.valid_salt().ok_or(BackupError::Invalid)?;
    let password = password
        .filter(|p| !p.is_empty())
        .ok_or(BackupError::PasswordRequired)?;
    let key = header.derive(&salt, password)?;
    let (head, body) = data.split_at(head_len);
    let payload = vault::open_with(&key, head, body).map_err(|_| BackupError::WrongPassword)?;
    Ok((kind, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pw(s: &str) -> SecretString {
        SecretString::new(s)
    }

    /// 把文件头换成 `header`（JSON 文本），内容保持不变。
    fn with_header(data: &[u8], header: &str) -> Vec<u8> {
        let (_, _, len) = super::header(data).unwrap();
        let mut out = MAGIC.to_vec();
        out.extend_from_slice(header.as_bytes());
        out.push(b'\n');
        out.extend_from_slice(&data[len..]);
        out
    }

    #[test]
    fn seal_open_round_trip() {
        let data = seal(SealedKind::Hosts, &pw("password-1"), b"payload").unwrap();
        assert!(is_sealed(&data) && data.starts_with(b"termlet-sealed/1\n{"));
        assert_eq!(peek_kind(&data), Some(SealedKind::Hosts));
        let (kind, payload) = open(&data, Some(&pw("password-1"))).unwrap();
        assert_eq!((kind, &payload[..]), (SealedKind::Hosts, &b"payload"[..]));
        // 文件中没有明文
        assert!(!data.windows(7).any(|w| w == b"payload"));

        assert_eq!(
            open(&data, None).unwrap_err(),
            BackupError::PasswordRequired
        );
        assert_eq!(
            open(&data, Some(&pw(""))).unwrap_err(),
            BackupError::PasswordRequired
        );
        assert_eq!(
            open(&data, Some(&pw("password-2"))).unwrap_err(),
            BackupError::WrongPassword
        );
        // 明文的文件头参与校验：改动类型后无法解开
        let text = String::from_utf8_lossy(&data[..super::header(&data).unwrap().2]).into_owned();
        let line = text.lines().nth(1).unwrap();
        let tampered = with_header(&data, &line.replace("\"hosts\"", "\"sync\""));
        assert_eq!(
            open(&tampered, Some(&pw("password-1"))).unwrap_err(),
            BackupError::WrongPassword
        );
        // 不认识的类型、不是加密文件
        let unknown = with_header(&data, &line.replace("\"hosts\"", "\"other\""));
        assert_eq!(peek_kind(&unknown), None);
        assert_eq!(
            open(&unknown, Some(&pw("password-1"))).unwrap_err(),
            BackupError::Invalid
        );
        assert_eq!(
            open(b"hello", Some(&pw("password-1"))).unwrap_err(),
            BackupError::Invalid
        );
        // 内容被改动
        let mut flipped = data.clone();
        *flipped.last_mut().unwrap() ^= 1;
        assert_eq!(
            open(&flipped, Some(&pw("password-1"))).unwrap_err(),
            BackupError::WrongPassword
        );
    }

    #[test]
    fn short_passwords_are_rejected() {
        assert_eq!(
            seal(SealedKind::Backup, &pw("short"), b"x").unwrap_err(),
            BackupError::PasswordTooShort { min: 8 }
        );
        // 按字符计数
        check_password(&pw("八个汉字八个汉字")).unwrap();
    }

    // 本程序写出的参数照常接受；超出范围的在派生密钥之前拒绝
    #[test]
    fn costly_kdf_parameters_are_rejected_before_deriving() {
        let production = Header {
            kind: "sync".into(),
            kdf: "argon2id".into(),
            time: 3,
            memory: 64 * 1024,
            threads: 4,
            salt: BASE64.encode(&[1u8; 16]),
        };
        assert!(production.valid_salt().is_some(), "正式的参数应被接受");

        let data = seal(SealedKind::Sync, &pw("password-1"), b"payload").unwrap();
        let salt = BASE64.encode(&[1u8; 16]);
        for (name, time, memory, threads) in [
            ("内存 300 MiB", 3, 300 * 1024, 4),
            ("迭代 20 次", 20, 64 * 1024, 4),
            ("内存 256 MiB、迭代 9 次", 9, 256 * 1024, 4),
            ("线程过多", 3, 64 * 1024, 64),
        ] {
            let header = format!(
                r#"{{"kind":"sync","kdf":"argon2id","time":{time},"memory":{memory},"threads":{threads},"salt":"{salt}"}}"#
            );
            let start = std::time::Instant::now();
            assert_eq!(
                open(&with_header(&data, &header), Some(&pw("password-1"))).unwrap_err(),
                BackupError::Invalid,
                "{name}"
            );
            assert!(
                start.elapsed() < std::time::Duration::from_secs(2),
                "{name}：不应派生密钥"
            );
        }
        let bad_kdf = format!(
            r#"{{"kind":"sync","kdf":"scrypt","time":1,"memory":8,"threads":1,"salt":"{salt}"}}"#
        );
        assert_eq!(
            open(&with_header(&data, &bad_kdf), Some(&pw("password-1"))).unwrap_err(),
            BackupError::Invalid
        );
    }

    // 固定向量：由 aes-gcm 0.10、argon2 0.5 写出的加密文件（正式的派生参数，盐与 nonce 写死）。
    // 升级加密库后必须原样通过：旧的备份、主机文件与同步文件要能解开，新写出的文件逐字节不变。
    const FIXED_PASSWORD: &str = "correct horse battery staple";
    const FIXED_PAYLOAD: &[u8] = "termlet-archive/1\n{\"备份\":true}\n".as_bytes();
    const FIXED_HEAD: &str = concat!(
        "termlet-sealed/1\n",
        r#"{"kind":"backup","kdf":"argon2id","time":3,"memory":65536,"threads":4,"salt":"dGVybWxldC1maXhlZC0wMQ=="}"#,
        "\n"
    );
    const FIXED_BODY: &str = "010707070707070707070707076f56f405077f97402f9e3fcb303ec76be8af42d74e4d29205bb3b2dcac4ee75f573c820a116f1d757a2338f9c9c8f3cae8e5";

    #[test]
    fn fixed_vector() {
        let header = Header {
            kind: "backup".into(),
            kdf: "argon2id".into(),
            time: 3,
            memory: 64 * 1024,
            threads: 4,
            salt: BASE64.encode(b"termlet-fixed-01"),
        };
        let mut head = MAGIC.to_vec();
        head.extend(serde_json::to_vec(&header).unwrap());
        head.push(b'\n');
        assert_eq!(String::from_utf8_lossy(&head), FIXED_HEAD);
        let key = header
            .derive(&header.valid_salt().unwrap(), &pw(FIXED_PASSWORD))
            .unwrap();
        let body = vault::seal_with_nonce(&key, &[7; 12], &head, FIXED_PAYLOAD);
        assert_eq!(data_encoding::HEXLOWER.encode(&body), FIXED_BODY);

        let mut file = FIXED_HEAD.as_bytes().to_vec();
        file.extend(
            data_encoding::HEXLOWER
                .decode(FIXED_BODY.as_bytes())
                .unwrap(),
        );
        assert_eq!(peek_kind(&file), Some(SealedKind::Backup));
        let (kind, payload) = open(&file, Some(&pw(FIXED_PASSWORD))).unwrap();
        assert_eq!((kind, &payload[..]), (SealedKind::Backup, FIXED_PAYLOAD));
        assert_eq!(
            open(&file, Some(&pw("wrong password"))).unwrap_err(),
            BackupError::WrongPassword
        );
    }
}
