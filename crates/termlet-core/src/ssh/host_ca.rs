//! 信任的主机 CA（OpenSSH known_hosts 中的 `@cert-authority` 行）：它签发的主机证书在第一次连接时免去指纹确认。
//!
//! - 本机设置（[`crate::model::LocalConnectionSettings::trusted_host_cas`]），不同步；可以从 known_hosts 文件导入。
//! - 只有这台主机适用某个信任的 CA 时，连接才请求证书类型的主机密钥（`host_key_certificates`），
//!   没有设置 CA 的用户行为不变。
//! - 服务器出示的主机证书要：由适用于这台主机的信任 CA 签发（签名正确）、是主机证书、在有效期内、
//!   允许的主机名中有连接用的地址（主机证书必须列出主机名，与 OpenSSH 相同）、没有关键选项。
//!   都满足时接受，不询问也不记录指纹；任何一项不满足时与 OpenSSH 相同，退回按证书里的主机密钥核对指纹
//!   （已记录的照常比较，未知的照常询问）。
//!
//! 主机名模式的规则与 OpenSSH 的 `match_hostname` 相同：逗号分隔，`*`、`?` 通配，`!` 开头的是排除（匹配到排除的
//! 整行不适用），不区分大小写；22 以外的端口按 `[host]:port` 匹配；`|1|salt|hash` 形式的散列主机名也认。

use hmac::{Hmac, KeyInit, Mac as _};
use ssh_key::certificate::CertType;
use ssh_key::{Certificate, HashAlg, PublicKey};

use crate::model::TrustedHostCa;
use crate::sshcert::{CertInfo, Validity};
use crate::store::Store;

/// 解析一行 CA：`@cert-authority <主机名模式> <算法> <base64> [注释]`。`added_at` 由调用方填。
pub fn parse_line(line: &str) -> Result<TrustedHostCa, HostCaError> {
    let line = line.trim();
    let rest = line
        .strip_prefix("@cert-authority")
        .filter(|r| r.starts_with(char::is_whitespace))
        .ok_or(HostCaError::NotCertAuthority)?
        .trim_start();
    let (hosts, key) = rest
        .split_once(char::is_whitespace)
        .ok_or(HostCaError::InvalidKey)?;
    let key = key.trim();
    new_ca(hosts, key)
}

/// 由主机名模式与 CA 公钥（OpenSSH 格式一行）组成一条；检查两者都有效。
pub fn new_ca(hosts: &str, public_key: &str) -> Result<TrustedHostCa, HostCaError> {
    let hosts = hosts.trim();
    if hosts.is_empty() || hosts.contains(char::is_whitespace) {
        return Err(HostCaError::InvalidHosts);
    }
    let public_key = public_key.trim();
    let key = PublicKey::from_openssh(public_key).map_err(|_| HostCaError::InvalidKey)?;
    if public_key
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .contains("-cert-")
    {
        return Err(HostCaError::InvalidKey);
    }
    Ok(TrustedHostCa {
        hosts: hosts.to_string(),
        public_key: key.to_openssh().unwrap_or_else(|_| public_key.to_string()),
        added_at: 0,
    })
}

/// CA 公钥（OpenSSH 格式一行）的算法与 SHA256 指纹（显示用）；解析不了时为 `None`。
pub fn describe_key(public_key: &str) -> Option<(String, String)> {
    let key = PublicKey::from_openssh(public_key.trim()).ok()?;
    Some((
        key.algorithm().as_str().to_string(),
        key.fingerprint(HashAlg::Sha256).to_string(),
    ))
}

/// 从 known_hosts 文件的内容中找出 `@cert-authority` 行；其他行（普通的主机密钥、`@revoked`、注释）忽略。
/// 返回解析出的 CA 与解析不了的 `@cert-authority` 行数。
pub fn parse_known_hosts(text: &str) -> (Vec<TrustedHostCa>, usize) {
    let mut cas = Vec::new();
    let mut invalid = 0;
    for line in text.lines().map(str::trim) {
        if !line.starts_with("@cert-authority") {
            continue;
        }
        match parse_line(line) {
            Ok(ca) => cas.push(ca),
            Err(_) => invalid += 1,
        }
    }
    (cas, invalid)
}

/// 添加 CA 失败的原因。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HostCaError {
    #[error("不是 @cert-authority 行")]
    NotCertAuthority,
    #[error("主机名模式无效")]
    InvalidHosts,
    #[error("CA 公钥无效")]
    InvalidKey,
}

/// 连接、核对主机密钥时用的主机名：22 端口用地址本身，其他端口写成 `[host]:port`（与 known_hosts 相同）。
pub fn host_key_name(host: &str, port: u16) -> String {
    let host = host.to_ascii_lowercase();
    if port == 22 {
        host
    } else {
        format!("[{host}]:{port}")
    }
}

/// 主机名模式（逗号分隔）是否适用于这台主机。
pub fn matches(patterns: &str, host: &str, port: u16) -> bool {
    let name = host_key_name(host, port);
    let mut positive = false;
    for pattern in patterns.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (negated, pattern) = match pattern.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, pattern),
        };
        let hit = if pattern.starts_with("|1|") {
            hashed_matches(pattern, &name)
        } else {
            glob(&pattern.to_ascii_lowercase(), &name)
        };
        if hit {
            if negated {
                return false;
            }
            positive = true;
        }
    }
    positive
}

/// `|1|<base64 salt>|<base64 HMAC-SHA1(salt, name)>`（`ssh-keygen -H` 散列过的主机名）。
fn hashed_matches(pattern: &str, name: &str) -> bool {
    let mut parts = pattern.trim_start_matches("|1|").split('|');
    let (Some(salt), Some(hash), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let (Ok(salt), Ok(hash)) = (
        data_encoding::BASE64.decode(salt.as_bytes()),
        data_encoding::BASE64.decode(hash.as_bytes()),
    ) else {
        return false;
    };
    let Ok(mut mac) = <Hmac<sha1::Sha1> as KeyInit>::new_from_slice(&salt) else {
        return false;
    };
    mac.update(name.as_bytes());
    mac.verify_slice(&hash).is_ok()
}

/// 通配符匹配：`*` 任意多个字符，`?` 一个字符。
fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || (p[pi] != '*' && p[pi] == t[ti])) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// 适用于这台主机的信任 CA（公钥）。
pub fn trusted_for(cas: &[TrustedHostCa], host: &str, port: u16) -> Vec<PublicKey> {
    cas.iter()
        .filter(|ca| matches(&ca.hosts, host, port))
        .filter_map(|ca| PublicKey::from_openssh(&ca.public_key).ok())
        .collect()
}

/// 存储中适用于这台主机的信任 CA。
pub fn trusted_in(store: &Store, host: &str, port: u16) -> Vec<PublicKey> {
    trusted_for(
        &store.local_settings().connection.trusted_host_cas,
        host,
        port,
    )
}

/// 本机信任的主机 CA。
pub fn list(store: &Store) -> Vec<TrustedHostCa> {
    store.local_settings().connection.trusted_host_cas
}

/// 两条是不是同一个 CA、同样的主机名模式（公钥比较时忽略注释）。
fn same(a: &TrustedHostCa, b: &TrustedHostCa) -> bool {
    let key = |s: &str| s.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
    a.hosts == b.hosts && key(&a.public_key) == key(&b.public_key)
}

/// 添加信任的 CA（已有同样的一条时不重复）；返回新加了几条。
pub fn add(store: &Store, cas: Vec<TrustedHostCa>) -> crate::Result<usize> {
    let mut added = 0;
    let now = crate::time::now_ms();
    store.update_local_settings(|s| {
        let list = &mut s.connection.trusted_host_cas;
        for mut ca in cas {
            if list.iter().any(|old| same(old, &ca)) {
                continue;
            }
            ca.added_at = now;
            tracing::info!(hosts = %ca.hosts, "信任主机 CA");
            list.push(ca);
            added += 1;
        }
    })?;
    Ok(added)
}

/// 不再信任这一条。
pub fn remove(store: &Store, ca: &TrustedHostCa) -> crate::Result<()> {
    tracing::info!(hosts = %ca.hosts, "不再信任主机 CA");
    store.update_local_settings(|s| s.connection.trusted_host_cas.retain(|old| !same(old, ca)))
}

/// 从 known_hosts 文件导入的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KnownHostsImport {
    /// 新加的
    pub added: usize,
    /// 已经有的
    pub existing: usize,
    /// 解析不了的 `@cert-authority` 行
    pub invalid: usize,
}

/// 从 known_hosts 文件（OpenSSH 的 `~/.ssh/known_hosts` 或 `/etc/ssh/ssh_known_hosts`）导入其中的 `@cert-authority` 行。
pub fn import_known_hosts(
    store: &Store,
    path: &std::path::Path,
) -> Result<KnownHostsImport, KnownHostsReadError> {
    let metadata = std::fs::metadata(path).map_err(KnownHostsReadError::from)?;
    // known_hosts 通常不过几百 KB，限制大小避免误选大文件
    if metadata.len() > 16 << 20 {
        return Err(KnownHostsReadError::TooLarge);
    }
    let bytes = std::fs::read(path).map_err(KnownHostsReadError::from)?;
    let (cas, invalid) = parse_known_hosts(&String::from_utf8_lossy(&bytes));
    let found = cas.len();
    let added = add(store, cas).map_err(|e| KnownHostsReadError::Store(e.to_string()))?;
    tracing::info!(path = %path.display(), found, added, invalid, "从 known_hosts 导入主机 CA");
    Ok(KnownHostsImport {
        added,
        existing: found - added,
        invalid,
    })
}

/// 读 known_hosts 文件失败的原因。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KnownHostsReadError {
    #[error("文件不存在")]
    NotFound,
    #[error("文件过大")]
    TooLarge,
    #[error("读取失败：{0}")]
    Io(String),
    #[error("保存失败：{0}")]
    Store(String),
}

impl From<std::io::Error> for KnownHostsReadError {
    fn from(error: std::io::Error) -> Self {
        match error.kind() {
            std::io::ErrorKind::NotFound => KnownHostsReadError::NotFound,
            _ => KnownHostsReadError::Io(error.to_string()),
        }
    }
}

/// 主机证书没有被接受的原因（日志用；接下来按证书里的主机密钥核对指纹）。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HostCertIssue {
    #[error("不是主机证书")]
    NotHostCertificate,
    #[error("签发它的 CA（{0}）不在这台主机适用的信任列表中")]
    UntrustedCa(String),
    #[error("CA 的签名不对")]
    BadSignature,
    #[error("证书还没有生效")]
    NotYetValid,
    #[error("证书已过期")]
    Expired,
    #[error("证书没有列出 {0}")]
    Principal(String),
    #[error("证书带有关键选项")]
    CriticalOptions,
}

/// 核对服务器出示的主机证书（`now` 为 Unix 秒）。通过时返回签发它的 CA 的指纹。
pub fn check(
    certificate: &Certificate,
    host: &str,
    trusted: &[PublicKey],
    now: i64,
) -> Result<String, HostCertIssue> {
    if certificate.cert_type() != CertType::Host {
        return Err(HostCertIssue::NotHostCertificate);
    }
    let ca = certificate.signature_key();
    let ca_fingerprint = ca.fingerprint(HashAlg::Sha256);
    if !trusted.iter().any(|k| k.key_data() == ca) {
        return Err(HostCertIssue::UntrustedCa(ca_fingerprint.to_string()));
    }
    certificate
        .verify_signature()
        .map_err(|_| HostCertIssue::BadSignature)?;
    let info = CertInfo::of(certificate);
    match info.validity_at(now) {
        Validity::NotYetValid { .. } => return Err(HostCertIssue::NotYetValid),
        Validity::Expired { .. } => return Err(HostCertIssue::Expired),
        Validity::Valid { .. } => {}
    }
    // 主机证书必须列出主机名（OpenSSH 不接受不限主机名的主机证书）
    let host = host.to_ascii_lowercase();
    if !info
        .principals
        .iter()
        .any(|p| p.to_ascii_lowercase() == host)
    {
        return Err(HostCertIssue::Principal(host));
    }
    if !info.critical_options.is_empty() {
        return Err(HostCertIssue::CriticalOptions);
    }
    Ok(ca_fingerprint.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sshcert::{CertKind, issue};
    use crate::sshkey::{KeyKind, generate};

    fn key() -> ssh_key::PrivateKey {
        generate(KeyKind::Ed25519, "")
            .unwrap()
            .into_ssh_private_key()
    }

    #[test]
    fn parse_cert_authority_lines() {
        let ca = key();
        let public = ca.public_key().to_openssh().unwrap();
        let entry = parse_line(&format!(
            "@cert-authority *.example.com,!bad.example.com {public} corp CA"
        ))
        .unwrap();
        assert_eq!(entry.hosts, "*.example.com,!bad.example.com");
        assert!(entry.public_key.starts_with("ssh-ed25519 AAAA"));
        assert_eq!(
            parse_line(&format!("*.example.com {public}")),
            Err(HostCaError::NotCertAuthority)
        );
        assert_eq!(
            parse_line("@cert-authority *.example.com ssh-ed25519 AAAAbroken"),
            Err(HostCaError::InvalidKey)
        );
        assert_eq!(
            parse_line("@cert-authority"),
            Err(HostCaError::NotCertAuthority)
        );
        assert_eq!(new_ca(" ", &public), Err(HostCaError::InvalidHosts));

        let text = format!(
            "# 注释\nweb.example.com {public}\n@cert-authority *.corp {public}\n@revoked * {public}\n@cert-authority x ssh-rsa AAAA\n"
        );
        let (cas, invalid) = parse_known_hosts(&text);
        assert_eq!(cas.len(), 1);
        assert_eq!(cas[0].hosts, "*.corp");
        assert_eq!(invalid, 1);
    }

    #[test]
    fn host_patterns() {
        assert!(matches("*.example.com", "web.example.com", 22));
        assert!(
            matches("*.EXAMPLE.com", "Web.Example.com", 22),
            "不区分大小写"
        );
        assert!(
            !matches("*.example.com", "web.example.com", 2222),
            "其他端口按 [host]:port"
        );
        assert!(matches("[*.example.com]:2222", "web.example.com", 2222));
        assert!(matches("[127.0.0.1]:2227", "127.0.0.1", 2227));
        assert!(matches("10.0.0.?,db", "db", 22));
        assert!(
            !matches("*,!bad.example.com", "bad.example.com", 22),
            "排除优先"
        );
        assert!(matches("*,!bad.example.com", "good.example.com", 22));
        assert!(!matches("!web", "web", 22));
        assert!(!matches("", "web", 22));

        // 散列过的主机名（ssh-keygen -H）
        let salt = [3u8; 20];
        let mut mac = <Hmac<sha1::Sha1> as KeyInit>::new_from_slice(&salt).unwrap();
        mac.update(b"web.example.com");
        let hash = mac.finalize().into_bytes();
        let hashed = format!(
            "|1|{}|{}",
            data_encoding::BASE64.encode(&salt),
            data_encoding::BASE64.encode(&hash)
        );
        assert!(matches(&hashed, "web.example.com", 22));
        assert!(!matches(&hashed, "db.example.com", 22));
    }

    // 主机证书：信任的 CA 签发、在有效期内、列出了主机名、没有关键选项才通过
    #[test]
    fn host_certificate_checks() {
        let (ca, other_ca, host_key) = (key(), key(), key());
        let trusted = [ca.public_key().clone()];
        let now = 1_800_000_000;
        let cert = |ca: &ssh_key::PrivateKey, kind, names: &[&str], after: u64, before: u64| {
            issue(
                ca,
                host_key.public_key().key_data(),
                kind,
                names,
                after,
                before,
            )
        };
        let good = cert(&ca, CertKind::Host, &["web.example.com"], 0, u64::MAX);
        assert_eq!(
            check(&good, "WEB.example.com", &trusted, now),
            Ok(ca.public_key().fingerprint(HashAlg::Sha256).to_string())
        );
        assert_eq!(
            check(&good, "db.example.com", &trusted, now),
            Err(HostCertIssue::Principal("db.example.com".into()))
        );
        assert!(matches!(
            check(
                &cert(&other_ca, CertKind::Host, &["web.example.com"], 0, u64::MAX),
                "web.example.com",
                &trusted,
                now
            ),
            Err(HostCertIssue::UntrustedCa(_))
        ));
        assert_eq!(
            check(
                &cert(&ca, CertKind::User, &["web.example.com"], 0, u64::MAX),
                "web.example.com",
                &trusted,
                now
            ),
            Err(HostCertIssue::NotHostCertificate)
        );
        assert_eq!(
            check(
                &cert(&ca, CertKind::Host, &["web.example.com"], 0, now as u64 - 1),
                "web.example.com",
                &trusted,
                now
            ),
            Err(HostCertIssue::Expired)
        );
        assert_eq!(
            check(
                &cert(&ca, CertKind::Host, &[], 0, u64::MAX),
                "web.example.com",
                &trusted,
                now
            ),
            Err(HostCertIssue::Principal("web.example.com".into())),
            "不限主机名的主机证书不接受"
        );

        let cas = vec![
            TrustedHostCa {
                hosts: "*.example.com".into(),
                public_key: ca.public_key().to_openssh().unwrap(),
                added_at: 0,
            },
            TrustedHostCa {
                hosts: "other".into(),
                public_key: other_ca.public_key().to_openssh().unwrap(),
                added_at: 0,
            },
        ];
        assert_eq!(trusted_for(&cas, "web.example.com", 22).len(), 1);
        assert!(trusted_for(&cas, "web.example.com", 2222).is_empty());
    }

    // 添加不重复（注释不同也算同一条）、删除、从 known_hosts 文件导入
    #[test]
    fn add_remove_and_import() {
        let store = crate::testkit::memory_store();
        let (a, b) = (key(), key());
        let line = |k: &ssh_key::PrivateKey, hosts: &str, comment: &str| {
            format!(
                "@cert-authority {hosts} {} {comment}",
                k.public_key().to_openssh().unwrap()
            )
        };
        let first = parse_line(&line(&a, "*.corp", "one")).unwrap();
        assert_eq!(add(&store, vec![first.clone()]).unwrap(), 1);
        assert_eq!(
            add(
                &store,
                vec![parse_line(&line(&a, "*.corp", "two")).unwrap()]
            )
            .unwrap(),
            0
        );
        assert!(list(&store)[0].added_at > 0);
        assert_eq!(trusted_in(&store, "web.corp", 22).len(), 1);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        std::fs::write(
            &path,
            format!(
                "{}\n{}\nweb.corp ssh-ed25519 AAAA\n@cert-authority bad\n",
                line(&a, "*.corp", ""),
                line(&b, "[db]:2222", "db CA")
            ),
        )
        .unwrap();
        assert_eq!(
            import_known_hosts(&store, &path).unwrap(),
            KnownHostsImport {
                added: 1,
                existing: 1,
                invalid: 1
            }
        );
        assert_eq!(trusted_in(&store, "db", 2222).len(), 1);
        assert_eq!(
            import_known_hosts(&store, &dir.path().join("missing")),
            Err(KnownHostsReadError::NotFound)
        );

        remove(&store, &first).unwrap();
        assert_eq!(list(&store).len(), 1);
        assert!(trusted_in(&store, "web.corp", 22).is_empty());
    }
}
