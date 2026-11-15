//! 更新文件与远程配置的签名。
//!
//! 格式与 minisign 相同：Ed25519 对内容的 BLAKE2b-512 摘要签名（算法标记 `ED`，不接受直接对原文签名的旧格式 `Ed`），
//! 带 8 字节的密钥 ID 与可信注释，可信注释另有一个签名；可以用 `minisign -Vm <文件> -P <公钥>` 手动核对。
//! 发布时由 `scripts/release.mjs` 用离线保管的私钥签名，应用用内置的公钥（[`super::keys`]）校验。
//!
//! 密码学部分交给 `minisign-verify`；这里先检查格式（恰好四行、只接受 `ED`、签名的密钥 ID
//! 要在受信任的公钥中、内置公钥写错时整体拒绝），给出看得懂的原因，再按用途检查可信注释：
//! - 版本描述文件：`termlet-manifest <文件名> <版本>`，别的平台、旧版本的清单拿来冒充也通不过；
//! - 远程配置：`termlet-remote-config <签发时间> <失效时间>`（UTC、精确到秒的 ISO 8601），
//!   过期的、比已采用过的更早签发的由调用方忽略（[`crate::remote_config`]）。

use std::sync::LazyLock;

use data_encoding::BASE64;
use regex::Regex;

const UNTRUSTED: &str = "untrusted comment: ";
const TRUSTED: &str = "trusted comment: ";
/// 公钥的算法标记。
const KEY_ALGORITHM: &[u8] = b"Ed";
/// 签名的算法标记：对 BLAKE2b-512 摘要签名（minisign 0.8 起的默认）。
const PREHASHED: &[u8] = b"ED";
/// 远程配置签名的可信注释的开头。
const REMOTE_CONFIG_TAG: &str = "termlet-remote-config";

/// 签名校验没有通过的原因。界面一律显示“签名校验没有通过”，原因写进日志。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SignatureError {
    #[error("应用中没有内置更新公钥")]
    NoKeys,
    #[error("公钥的格式不对")]
    BadPublicKey,
    #[error("不支持的公钥算法")]
    UnsupportedKeyAlgorithm,
    #[error("签名的格式不对")]
    BadFormat,
    #[error("只接受对摘要签名的格式（ED）")]
    Legacy,
    #[error("签名所用的密钥（{0}）不受信任")]
    UntrustedKey(String),
    /// 内容、签名、可信注释或它的签名被改动过（`minisign-verify` 不区分是哪一处）。
    #[error("签名与内容不符")]
    Mismatch,
    #[error("{file} 没有签名（HTTP {status}）")]
    Missing { file: String, status: u16 },
    #[error("{0} 中没有版本号")]
    NoVersion(String),
    #[error("签名不是 {file} {version} 的（{comment}）")]
    WrongManifest {
        file: String,
        version: String,
        comment: String,
    },
    #[error("版本号与签名不符（{0}）")]
    VersionMismatch(String),
    #[error("签名不是远程配置的（{0}）")]
    NotRemoteConfig(String),
    #[error("没有校验过签名的版本信息")]
    Unverified,
    #[error("安装包与签名的版本信息不一致")]
    InstallerMismatch,
}

/// 解析后的公钥。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicKey {
    id: [u8; 8],
    /// base64 的那一行
    line: String,
}

impl PublicKey {
    /// 8 字节的密钥 ID：签名中带着它，用来找到对应的公钥。
    pub fn id(&self) -> [u8; 8] {
        self.id
    }
}

/// 密钥 ID 的显示形式，与 minisign 相同：按小端整数显示的十六进制。
pub fn key_id_hex(id: &[u8; 8]) -> String {
    id.iter().rev().map(|b| format!("{b:02X}")).collect()
}

/// 解码固定长度的 base64；字符不合法、长度不对时为 `None`。
fn decode(text: &str, len: usize) -> Option<Vec<u8>> {
    let raw = BASE64.decode(text.trim().as_bytes()).ok()?;
    (raw.len() == len).then_some(raw)
}

/// 解析公钥：minisign 公钥文件的全文，或其中 base64 的那一行。
pub fn parse_public_key(text: &str) -> Result<PublicKey, SignatureError> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with(UNTRUSTED))
        .collect();
    let [line] = lines[..] else {
        return Err(SignatureError::BadPublicKey);
    };
    let raw = decode(line, 42).ok_or(SignatureError::BadPublicKey)?;
    if &raw[..2] != KEY_ALGORITHM {
        return Err(SignatureError::UnsupportedKeyAlgorithm);
    }
    let mut id = [0u8; 8];
    id.copy_from_slice(&raw[2..10]);
    Ok(PublicKey {
        id,
        line: line.to_string(),
    })
}

/// 检查签名文件的格式，返回统一成 LF 的全文与签名的密钥 ID。
fn parse_signature(text: &str) -> Result<(String, [u8; 8]), SignatureError> {
    let normalized = text.replace("\r\n", "\n");
    let mut lines: Vec<&str> = normalized.split('\n').collect();
    while lines.len() > 4 && lines.last() == Some(&"") {
        lines.pop();
    }
    let [untrusted, signature, trusted, global] = lines[..] else {
        return Err(SignatureError::BadFormat);
    };
    if !untrusted.starts_with(UNTRUSTED) || !trusted.starts_with(TRUSTED) {
        return Err(SignatureError::BadFormat);
    }
    let raw = decode(signature, 74).ok_or(SignatureError::BadFormat)?;
    decode(global, 64).ok_or(SignatureError::BadFormat)?;
    if &raw[..2] != PREHASHED {
        return Err(SignatureError::Legacy);
    }
    let mut id = [0u8; 8];
    id.copy_from_slice(&raw[2..10]);
    let text = [untrusted, signature, trusted, global].join("\n");
    Ok((text, id))
}

/// 校验签名，返回可信注释。`keys` 为受信任的公钥（minisign 格式）。
///
/// 签名与内容、可信注释与签名都要对得上；其中有一把公钥写错时整体拒绝，不会悄悄跳过。
pub fn verify_signature(
    message: &[u8],
    signature_text: &str,
    keys: &[impl AsRef<str>],
) -> Result<String, SignatureError> {
    if keys.is_empty() {
        return Err(SignatureError::NoKeys);
    }
    let (text, key_id) = parse_signature(signature_text)?;
    let keys = keys
        .iter()
        .map(|k| parse_public_key(k.as_ref()))
        .collect::<Result<Vec<_>, _>>()?;
    let key = keys
        .iter()
        .find(|k| k.id == key_id)
        .ok_or_else(|| SignatureError::UntrustedKey(key_id_hex(&key_id)))?;
    let public = minisign_verify::PublicKey::from_base64(&key.line)
        .map_err(|_| SignatureError::BadPublicKey)?;
    let signature =
        minisign_verify::Signature::decode(&text).map_err(|_| SignatureError::BadFormat)?;
    public
        .verify(message, &signature, false)
        .map_err(|e| match e {
            minisign_verify::Error::InvalidSignature => SignatureError::Mismatch,
            minisign_verify::Error::UnexpectedKeyId => {
                SignatureError::UntrustedKey(key_id_hex(&key_id))
            }
            minisign_verify::Error::UnexpectedAlgorithm
            | minisign_verify::Error::UnsupportedAlgorithm
            | minisign_verify::Error::UnsupportedLegacyMode => SignatureError::Legacy,
            _ => SignatureError::BadFormat,
        })?;
    // 返回校验过的那份可信注释（minisign-verify 解析出的）
    Ok(signature.trusted_comment().to_string())
}

/// 版本描述文件（`latest.yml` 等）的可信注释：绑定文件名与版本。
pub fn manifest_comment(file: &str, version: &str) -> String {
    format!("termlet-manifest {file} {version}")
}

/// 版本描述文件中的版本号（最外层的 `version`）。
pub fn manifest_version(text: &str) -> Option<String> {
    static VERSION: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?m)^version:[ \t]*['"]?([^'"\s]+)"#).unwrap());
    VERSION.captures(text).map(|c| c[1].to_string())
}

/// 校验版本描述文件：签名有效，可信注释中的文件名与版本和内容一致。返回版本号。
pub fn verify_manifest(
    text: &str,
    signature_text: &str,
    file: &str,
    keys: &[impl AsRef<str>],
) -> Result<String, SignatureError> {
    let comment = verify_signature(text.as_bytes(), signature_text, keys)?;
    let version = manifest_version(text).ok_or_else(|| SignatureError::NoVersion(file.into()))?;
    if comment != manifest_comment(file, &version) {
        return Err(SignatureError::WrongManifest {
            file: file.into(),
            version,
            comment,
        });
    }
    Ok(version)
}

/// 远程配置签名中的签发与失效时间（Unix 毫秒）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Validity {
    pub issued: i64,
    pub expires: i64,
}

/// 远程配置 `update.json` 的可信注释，带签发与失效的时间（发布脚本按同样的格式写；测试中用它签名）。
#[cfg(test)]
pub(crate) fn remote_config_comment(issued_ms: i64, expires_ms: i64) -> String {
    format!(
        "{REMOTE_CONFIG_TAG} {} {}",
        iso_seconds(issued_ms),
        iso_seconds(expires_ms)
    )
}

/// 校验远程配置的签名，返回签名中的签发与失效时间（是否过期由调用方判断）。
pub fn verify_remote_config(
    text: &[u8],
    signature_text: &str,
    keys: &[impl AsRef<str>],
) -> Result<Validity, SignatureError> {
    let comment = verify_signature(text, signature_text, keys)?;
    let parts: Vec<&str> = comment.split(' ').collect();
    let times = match parts[..] {
        [tag, issued, expires] if tag == REMOTE_CONFIG_TAG => {
            parse_iso_seconds(issued).zip(parse_iso_seconds(expires))
        }
        _ => None,
    };
    match times {
        Some((issued, expires)) if issued < expires => Ok(Validity { issued, expires }),
        _ => Err(SignatureError::NotRemoteConfig(comment)),
    }
}

/// Unix 毫秒写成 UTC、精确到秒的 ISO 8601（`2026-10-15T08:00:00Z`）；超出范围时为空。
pub fn iso_seconds(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

/// 解析 [`iso_seconds`] 的格式；格式不对、日期不存在时为 `None`。
pub fn parse_iso_seconds(text: &str) -> Option<i64> {
    static ISO: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$").unwrap());
    if !ISO.is_match(text) {
        return None;
    }
    chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%SZ")
        .ok()
        .map(|t| t.and_utc().timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::TestKey;
    use super::*;

    fn keys() -> (TestKey, TestKey, Vec<String>) {
        let primary = TestKey::random();
        let backup = TestKey::random();
        let keys = vec![primary.public_key(), backup.public_key()];
        (primary, backup, keys)
    }

    /// 改动签名文件中的某一行。
    fn edit_line(sig: &str, index: usize, edit: impl FnOnce(&str) -> String) -> String {
        let mut lines: Vec<String> = sig.split('\n').map(str::to_string).collect();
        lines[index] = edit(&lines[index]);
        lines.join("\n")
    }

    /// 翻转 base64 解码后第 `offset` 个字节的最低位。
    fn flip_byte(b64: &str, offset: usize) -> String {
        let mut raw = BASE64.decode(b64.as_bytes()).unwrap();
        raw[offset] ^= 1;
        BASE64.encode(&raw)
    }

    const MANIFEST: &str = "version: 1.2.0\nfiles:\n  - url: termlet-1.2.0-win-x64.exe\n    sha512: AAAA\n    size: 1\npath: termlet-1.2.0-win-x64.exe\nsha512: AAAA\n";

    #[test]
    fn signs_and_verifies_with_either_key() {
        let (primary, backup, keys) = keys();
        let sig = primary.sign(b"hello", "trusted text");
        assert_eq!(
            verify_signature(b"hello", &sig, &keys).unwrap(),
            "trusted text"
        );
        // CRLF 换行的签名文件同样接受
        assert_eq!(
            verify_signature(b"hello", &sig.replace('\n', "\r\n"), &keys).unwrap(),
            "trusted text"
        );
        let by_backup = backup.sign(b"hello", "from backup");
        assert_eq!(
            verify_signature(b"hello", &by_backup, &keys).unwrap(),
            "from backup"
        );
    }

    #[test]
    fn any_tampering_fails() {
        let (primary, _, keys) = keys();
        let sig = primary.sign(b"hello", "trusted text");
        let mismatch = Err(SignatureError::Mismatch);
        assert_eq!(verify_signature(b"hellO", &sig, &keys), mismatch);
        let sig_flipped = edit_line(&sig, 1, |l| flip_byte(l, 40));
        assert_eq!(verify_signature(b"hello", &sig_flipped, &keys), mismatch);
        let forged = edit_line(&sig, 2, |_| "trusted comment: forged".into());
        assert_eq!(verify_signature(b"hello", &forged, &keys), mismatch);
        let global_flipped = edit_line(&sig, 3, |l| flip_byte(l, 0));
        assert_eq!(verify_signature(b"hello", &global_flipped, &keys), mismatch);
    }

    #[test]
    fn untrusted_or_missing_keys_fail() {
        let (_, _, keys) = keys();
        let other = TestKey::random();
        let sig = other.sign(b"hello", "x");
        assert_eq!(
            verify_signature(b"hello", &sig, &keys),
            Err(SignatureError::UntrustedKey(key_id_hex(&other.id())))
        );
        let none: [&str; 0] = [];
        assert_eq!(
            verify_signature(b"hello", &sig, &none),
            Err(SignatureError::NoKeys)
        );
        // 同一个密钥 ID 配上别的公钥（冒充）：找到了 ID，但签名对不上
        let forged = TestKey::with_id(other.id()).public_key();
        assert_eq!(
            verify_signature(b"hello", &sig, &[forged]),
            Err(SignatureError::Mismatch)
        );
        assert_eq!(
            key_id_hex(&[0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]),
            "EFCDAB8967452301"
        );
    }

    #[test]
    fn rejects_legacy_format() {
        let (primary, _, keys) = keys();
        let sig = primary.sign(b"hello", "x");
        let legacy = edit_line(&sig, 1, |l| {
            let mut raw = BASE64.decode(l.as_bytes()).unwrap();
            raw[1] = b'd';
            BASE64.encode(&raw)
        });
        assert_eq!(
            verify_signature(b"hello", &legacy, &keys),
            Err(SignatureError::Legacy)
        );
    }

    #[test]
    fn rejects_malformed_signatures_and_keys() {
        let (primary, _, keys) = keys();
        let sig = primary.sign(b"hello", "x");
        let three_lines = sig.split('\n').take(3).collect::<Vec<_>>().join("\n");
        let bad = [
            String::new(),
            "not a signature".into(),
            three_lines,
            edit_line(&sig, 0, |_| "comment: x".into()),
            edit_line(&sig, 2, |_| "comment: x".into()),
            edit_line(&sig, 1, |l| format!("{l}!")),
            edit_line(&sig, 1, |l| l[4..].to_string()),
            edit_line(&sig, 3, |_| "AAAA".into()),
            format!("{sig}extra line\n"),
        ];
        for s in &bad {
            assert_eq!(
                verify_signature(b"hello", s, &keys),
                Err(SignatureError::BadFormat),
                "{s:?}"
            );
        }
        let long = format!("{}AA", keys[0]);
        for k in ["", "RWQ", long.as_str(), "untrusted comment: x"] {
            assert!(parse_public_key(k).is_err(), "{k:?}");
        }
        // 内置公钥写错时整体拒绝，不会悄悄跳过
        let mut with_garbage = vec!["garbage".to_string()];
        with_garbage.extend(keys);
        assert_eq!(
            verify_signature(b"hello", &sig, &with_garbage),
            Err(SignatureError::BadPublicKey)
        );
    }

    #[test]
    fn public_key_may_be_a_whole_minisign_file() {
        let key = TestKey::random();
        let file = format!(
            "untrusted comment: minisign public key {}\r\n{}\r\n",
            key_id_hex(&key.id()),
            key.public_key()
        );
        assert_eq!(parse_public_key(&file).unwrap().id(), key.id());
        // 签名用的私钥是 Ed25519，标记写成 ED 的公钥不接受
        let mut raw = BASE64.decode(key.public_key().as_bytes()).unwrap();
        raw[1] = b'D';
        assert_eq!(
            parse_public_key(&BASE64.encode(&raw)),
            Err(SignatureError::UnsupportedKeyAlgorithm)
        );
    }

    // 固定的样例（用 Node 的实现签出，与 minisign、Go 的 aead.dev/minisign 双向核对过）：
    // 同样的私钥与内容，签名逐字节不变，发布脚本与应用的格式一致
    const FIXED_PUBLIC_KEY: &str = "RWQBAgMEBQYHCOpKbGPinFIKvvVQexMuxfmVR3auvr57kkIe6mkURtIs";
    const FIXED_SIGNATURE: &str = "untrusted comment: test\nRUQBAgMEBQYHCPqS5E59BgU+P2kcY4vRS0+DAo9LVq7/E6QRA+hjTrCvQoS1ug6zKt5EH4J2UU1WKUhjMDAPXLhYS9xgoQlSvAE=\ntrusted comment: termlet-test\n13sM3nu5p4qt/5u4osdwc0ME2X21WGqN4b5c4btGYP6vmUOMornq598yYOgx+fht0FQaUbNDgtOKOxcQfh3IDA==\n";

    #[test]
    fn fixed_sample_matches_old_implementation() {
        let key = TestKey::from_seed([7; 32], [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(key.public_key(), FIXED_PUBLIC_KEY);
        assert_eq!(
            key.sign_with(b"termlet", "termlet-test", "test"),
            FIXED_SIGNATURE
        );
        assert_eq!(
            verify_signature(b"termlet", FIXED_SIGNATURE, &[FIXED_PUBLIC_KEY]).unwrap(),
            "termlet-test"
        );
    }

    #[test]
    fn builtin_keys_parse() {
        let ids: Vec<String> = super::super::keys::UPDATE_PUBLIC_KEYS
            .iter()
            .map(|k| key_id_hex(&parse_public_key(k).unwrap().id()))
            .collect();
        assert_eq!(ids, ["70538B50651A6352", "1274237273808A74"]);
    }

    #[test]
    fn manifest_comment_binds_file_and_version() {
        let (primary, _, keys) = keys();
        let sig = primary.sign(
            MANIFEST.as_bytes(),
            &manifest_comment("latest.yml", "1.2.0"),
        );
        assert_eq!(
            verify_manifest(MANIFEST, &sig, "latest.yml", &keys).unwrap(),
            "1.2.0"
        );
        // 别的平台的文件
        assert!(matches!(
            verify_manifest(MANIFEST, &sig, "latest-mac.yml", &keys),
            Err(SignatureError::WrongManifest { .. })
        ));
        // 旧版本的签名配上改过版本号的内容：内容变了，签名不符
        let newer = MANIFEST.replace("version: 1.2.0", "version: 9.9.9");
        assert_eq!(
            verify_manifest(&newer, &sig, "latest.yml", &keys),
            Err(SignatureError::Mismatch)
        );
        // 签名本身有效，但可信注释的版本与内容不一致
        let mismatch = primary.sign(newer.as_bytes(), &manifest_comment("latest.yml", "1.2.0"));
        assert!(matches!(
            verify_manifest(&newer, &mismatch, "latest.yml", &keys),
            Err(SignatureError::WrongManifest { .. })
        ));
        let no_version = "files: []\n";
        let sig = primary.sign(no_version.as_bytes(), &manifest_comment("latest.yml", ""));
        assert_eq!(
            verify_manifest(no_version, &sig, "latest.yml", &keys),
            Err(SignatureError::NoVersion("latest.yml".into()))
        );
        assert_eq!(
            manifest_version("a: 1\r\nversion: '2.0.0-beta.1'\r\n").as_deref(),
            Some("2.0.0-beta.1")
        );
        // 缩进的 version（files 中的字段）不算
        assert_eq!(manifest_version("files:\n  version: 1.0.0\n"), None);
    }

    #[test]
    fn remote_config_validity() {
        let (primary, _, keys) = keys();
        let text = b"{\"disabled\":true}\n";
        let issued = parse_iso_seconds("2026-10-15T08:00:00Z").unwrap() + 123;
        let expires = parse_iso_seconds("2027-01-13T08:00:00Z").unwrap();
        let comment = remote_config_comment(issued, expires);
        assert_eq!(
            comment,
            "termlet-remote-config 2026-10-15T08:00:00Z 2027-01-13T08:00:00Z"
        );
        assert_eq!(
            verify_remote_config(text, &primary.sign(text, &comment), &keys).unwrap(),
            Validity {
                issued: issued - 123,
                expires
            }
        );
        // 不能拿版本描述文件的签名冒充
        let wrong = primary.sign(text, &manifest_comment("latest.yml", "1.0.0"));
        assert!(matches!(
            verify_remote_config(text, &wrong, &keys),
            Err(SignatureError::NotRemoteConfig(_))
        ));
    }

    #[test]
    fn remote_config_times_must_be_valid() {
        let (primary, _, keys) = keys();
        let text = b"{}\n";
        for comment in [
            "termlet-remote-config",
            "termlet-remote-config 2026-10-15T08:00:00Z",
            "termlet-remote-config 2026-10-15T08:00:00Z 2027-01-13T08:00:00Z extra",
            "termlet-remote-config 2026-10-15 2027-01-13",
            "termlet-remote-config 2026-13-45T08:00:00Z 2027-01-13T08:00:00Z",
            "termlet-remote-config 2026-02-30T08:00:00Z 2027-01-13T08:00:00Z",
            "termlet-remote-config 2026-10-15T08:00:00Z 2026-10-15T08:00:00Z",
            "termlet-remote-config 2027-01-13T08:00:00Z 2026-10-15T08:00:00Z",
            "termlet-remote-config  2026-10-15T08:00:00Z 2027-01-13T08:00:00Z",
            "termlet-remote-configX 2026-10-15T08:00:00Z 2027-01-13T08:00:00Z",
            "termlet-remote-config +2026-10-15T08:00:00Z 2027-01-13T08:00:00Z",
        ] {
            let sig = primary.sign(text, comment);
            assert!(
                matches!(
                    verify_remote_config(text, &sig, &keys),
                    Err(SignatureError::NotRemoteConfig(_))
                ),
                "{comment}"
            );
        }
        assert_eq!(iso_seconds(0), "1970-01-01T00:00:00Z");
        assert_eq!(parse_iso_seconds("1970-01-01T00:00:01Z"), Some(1000));
    }
}
