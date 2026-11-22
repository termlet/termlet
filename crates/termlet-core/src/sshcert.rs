//! OpenSSH 证书（`ssh-keygen -s` 签发的用户证书、主机证书）：解析、展示用的信息、有效期与允许的用户名的判断、
//! 与私钥的配对、私钥文件旁 `-cert.pub` 的查找，以及登录被拒绝时的说明。
//!
//! - 用户证书：企业用 SSH CA（Vault、Teleport、Smallstep 等）给用户的公钥签发短期证书，服务器只信任 CA
//!   （`TrustedUserCAKeys`）。用私钥登录时先出示证书，被拒绝且服务器还接受公钥时再用私钥本身（见 `ssh::auth`）。
//! - 主机证书：本机信任的主机 CA（`@cert-authority`，见 [`crate::ssh::host_ca`]）签发的，第一次连接时免去指纹确认。
//!
//! 证书不是秘密：密钥库中的证书与条目一起明文保存、同步；日志中可以写指纹、序号与有效期。

use std::path::{Path, PathBuf};

use ssh_key::certificate::CertType;
use ssh_key::public::KeyData;
use ssh_key::{Certificate, HashAlg};

/// 证书文本的长度上限（一行 OpenSSH 证书通常不到 2 KB，RSA 4096 位的 CA 与大量用户名也远小于它）。
pub const MAX_TEXT_LEN: usize = 64 * 1024;

/// 剩下不到这么久（秒）时提醒“证书快过期”。
pub const EXPIRING_SOON_SECS: i64 = 3600;

/// 证书的种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CertKind {
    /// 用户证书（登录用）
    User,
    /// 主机证书（服务器出示，证明自己的主机密钥）
    Host,
}

/// 证书解析失败的原因。`Display` 是写给日志看的中文，界面按变体选用自己的文案。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CertError {
    #[error("内容为空")]
    Empty,
    #[error("内容过大")]
    TooLarge,
    /// 是公钥或私钥，不是证书
    #[error("这不是证书（是公钥或私钥）")]
    NotCertificate,
    /// 证书的编码损坏、算法与内容不一致，或 CA 的签名不对
    #[error("证书内容无效")]
    Malformed,
}

/// 解析一张 OpenSSH 证书（`*-cert.pub` 文件的内容，或粘贴的一行）。取第一行不是空行、不是 `#` 注释的内容；
/// 去掉 UTF-8 BOM 与首尾空白。校验 CA 的签名（签名不对的当作内容无效），不判断 CA 是否可信。
pub fn parse(text: &str) -> Result<Certificate, CertError> {
    if text.len() > MAX_TEXT_LEN {
        return Err(CertError::TooLarge);
    }
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(line) = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
    else {
        return Err(CertError::Empty);
    };
    let kind = line.split_whitespace().next().unwrap_or_default();
    if !kind.ends_with("-cert-v01@openssh.com") {
        let key_like = line.starts_with("-----BEGIN")
            || line.starts_with("PuTTY-User-Key-File")
            || ["ssh-", "ecdsa-", "sk-"]
                .iter()
                .any(|p| kind.starts_with(p));
        return Err(if key_like {
            CertError::NotCertificate
        } else {
            CertError::Malformed
        });
    }
    let certificate = Certificate::from_openssh(line).map_err(|_| CertError::Malformed)?;
    certificate
        .verify_signature()
        .map_err(|_| CertError::Malformed)?;
    Ok(certificate)
}

/// 读证书文件（先看大小，过大的不读）。文件不存在等读取错误原样返回。
pub fn read_file(path: &Path) -> std::io::Result<String> {
    let metadata = std::fs::metadata(path)?;
    if metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "这是一个目录",
        ));
    }
    if metadata.len() > MAX_TEXT_LEN as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "文件过大",
        ));
    }
    std::fs::read_to_string(path)
}

/// 私钥文件旁的证书文件，按 OpenSSH 查找的顺序：`<私钥文件>-cert`、`<私钥文件>-cert.pub`
/// （`ssh -i ~/.ssh/id_ed25519` 自动出示 `~/.ssh/id_ed25519-cert.pub`）。
pub fn beside(key_path: &Path) -> [PathBuf; 2] {
    let mut cert = key_path.as_os_str().to_owned();
    cert.push("-cert");
    let mut pub_file = cert.clone();
    pub_file.push(".pub");
    [PathBuf::from(cert), PathBuf::from(pub_file)]
}

/// 私钥文件旁实际存在的证书文件（[`beside`] 中第一个是文件的）。
pub fn find_beside(key_path: &Path) -> Option<PathBuf> {
    beside(key_path).into_iter().find(|p| p.is_file())
}

/// 证书里的公钥是不是这把密钥的（证书与私钥配对）。
pub fn matches_key(certificate: &Certificate, key: &KeyData) -> bool {
    certificate.public_key() == key
}

/// 证书里的公钥是不是这一行 OpenSSH 公钥（密钥库条目的 `public_key`）。
pub fn matches_public_key(certificate: &Certificate, public_key: &str) -> bool {
    ssh_key::PublicKey::from_openssh(public_key.trim())
        .is_ok_and(|key| matches_key(certificate, key.key_data()))
}

/// 展示用的证书信息（`ssh-keygen -L` 列出的那些）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertInfo {
    pub kind: CertKind,
    /// 证书里公钥的算法（ssh-ed25519、ssh-rsa、ecdsa-sha2-nistp256……）
    pub algorithm: String,
    /// 证书里公钥的 SHA256 指纹
    pub key_fingerprint: String,
    /// 签发它的 CA 的算法与 SHA256 指纹
    pub ca_algorithm: String,
    pub ca_fingerprint: String,
    /// 序号（CA 不编号时为 0）
    pub serial: u64,
    /// 签发时填写的标识（`ssh-keygen -I`），服务器写进日志
    pub key_id: String,
    /// 允许的用户名（用户证书）或主机名（主机证书）；为空表示不限
    pub principals: Vec<String>,
    /// 生效时间（Unix 秒）；`None` 表示一直有效（`always`）
    pub valid_after: Option<i64>,
    /// 失效时间（Unix 秒）；`None` 表示永不过期（`forever`）
    pub valid_before: Option<i64>,
    /// 关键选项（`force-command`、`source-address` 等），服务器必须遵守
    pub critical_options: Vec<(String, String)>,
    /// 扩展项（`permit-pty`、`permit-port-forwarding` 等）
    pub extensions: Vec<(String, String)>,
    pub comment: String,
}

/// 有效期的状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Validity {
    /// 还没有生效（Unix 秒）
    NotYetValid { from: i64 },
    /// 有效；`until` 为 `None` 时永不过期
    Valid { until: Option<i64> },
    /// 已过期（Unix 秒）
    Expired { at: i64 },
}

impl CertInfo {
    pub fn of(certificate: &Certificate) -> Self {
        let options = |map: &ssh_key::certificate::OptionsMap| {
            map.iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<Vec<_>>()
        };
        Self {
            kind: if certificate.cert_type() == CertType::Host {
                CertKind::Host
            } else {
                CertKind::User
            },
            algorithm: certificate.algorithm().as_str().to_string(),
            key_fingerprint: certificate
                .public_key()
                .fingerprint(HashAlg::Sha256)
                .to_string(),
            ca_algorithm: certificate.signature_key().algorithm().as_str().to_string(),
            ca_fingerprint: certificate
                .signature_key()
                .fingerprint(HashAlg::Sha256)
                .to_string(),
            serial: certificate.serial(),
            key_id: certificate.key_id().to_string(),
            principals: certificate.valid_principals().to_vec(),
            valid_after: after(certificate.valid_after()),
            valid_before: before(certificate.valid_before()),
            critical_options: options(certificate.critical_options()),
            extensions: options(certificate.extensions()),
            comment: certificate.comment().to_string(),
        }
    }

    /// 在 `now`（Unix 秒）时的有效期状态。与 OpenSSH 相同：生效时间 ≤ 现在 < 失效时间。
    pub fn validity_at(&self, now: i64) -> Validity {
        if let Some(from) = self.valid_after
            && now < from
        {
            return Validity::NotYetValid { from };
        }
        match self.valid_before {
            Some(at) if now >= at => Validity::Expired { at },
            until => Validity::Valid { until },
        }
    }

    /// 证书是否允许这个名字（用户名或主机名）：没有列出任何名字时不限（用户证书；主机证书见 `ssh::host_ca`）。
    pub fn allows(&self, name: &str) -> bool {
        self.principals.is_empty() || self.principals.iter().any(|p| p == name)
    }
}

fn after(secs: u64) -> Option<i64> {
    (secs > 0).then(|| i64::try_from(secs).unwrap_or(i64::MAX))
}

fn before(secs: u64) -> Option<i64> {
    i64::try_from(secs).ok().filter(|_| secs != u64::MAX)
}

/// 现在的 Unix 秒。
pub fn now_secs() -> i64 {
    crate::time::now_ms() / 1000
}

/// 失效时间 `expires_at`（Unix 秒）在 `now` 时是否快到了（还没过期，剩下不到 [`EXPIRING_SOON_SECS`]）。
pub fn expires_soon(expires_at: i64, now: i64) -> bool {
    now < expires_at && expires_at - now < EXPIRING_SOON_SECS
}

/// 登录用的证书中快过期的（[`expires_soon`]）：经过的跳板机的（`jumps`，从第一跳起，带跳板机显示用的名称）在前，
/// 目标主机的（`target`，结果中为 `None`）在后。终端与命令行工具都按它提醒。
pub fn expiring(
    jumps: Vec<(String, i64)>,
    target: Option<i64>,
    now: i64,
) -> Vec<(Option<String>, i64)> {
    jumps
        .into_iter()
        .map(|(jump, expiry)| (Some(jump), expiry))
        .chain(target.map(|expiry| (None, expiry)))
        .filter(|(_, expiry)| expires_soon(*expiry, now))
        .collect()
}

/// 证书剩下的分钟数（提醒中的“还剩约几分钟”）：不足一分钟的算一分钟。
pub fn minutes_left(expires_at: i64, now: i64) -> i64 {
    ((expires_at - now) / 60).max(1)
}

/// 证书没能用来登录的原因（连接失败的说明）。时间都是 Unix 秒。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CertProblem {
    /// 证书已过期
    Expired { at: i64 },
    /// 证书还没有生效（也可能是本机或服务器的时间不对）
    NotYetValid { from: i64 },
    /// 登录用的用户名不在证书允许的用户名中
    Principal {
        username: String,
        principals: Vec<String>,
    },
    /// 本机看证书没有问题，服务器仍不接受：多半是服务器不信任签发它的 CA（`TrustedUserCAKeys`），
    /// 或证书已被吊销、服务器另有允许的名字（`AuthorizedPrincipalsFile`）
    Rejected { ca_fingerprint: String },
    /// 证书与私钥对不上（证书是给另一把密钥签发的）。`certificate` 是证书文件名，密钥库中的证书为空（下同）
    KeyMismatch { certificate: String },
    /// 是主机证书，不能用来登录
    HostCertificate { certificate: String },
    /// 证书文件读不了，或内容不是证书
    Unreadable {
        certificate: String,
        reason: CertReadIssue,
    },
}

/// 证书文件为什么读不了。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CertReadIssue {
    NotFound,
    /// 其他读取错误（没有权限、是目录、过大……）
    Io(String),
    /// 内容不是证书或已损坏
    Invalid(CertError),
}

impl From<std::io::Error> for CertReadIssue {
    fn from(error: std::io::Error) -> Self {
        match error.kind() {
            std::io::ErrorKind::NotFound => CertReadIssue::NotFound,
            _ => CertReadIssue::Io(error.to_string()),
        }
    }
}

impl std::fmt::Display for CertReadIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CertReadIssue::NotFound => f.write_str("文件不存在"),
            CertReadIssue::Io(reason) => f.write_str(reason),
            CertReadIssue::Invalid(error) => write!(f, "{error}"),
        }
    }
}

impl std::fmt::Display for CertProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let time = |secs: i64| crate::time::format_local(secs.saturating_mul(1000));
        let name = |certificate: &str| {
            if certificate.is_empty() {
                "密钥库中的证书".to_string()
            } else {
                format!("证书 {certificate} ")
            }
        };
        match self {
            CertProblem::Expired { at } => write!(f, "证书已于 {} 过期，请重新签发", time(*at)),
            CertProblem::NotYetValid { from } => write!(
                f,
                "证书 {} 才生效（也请检查本机与服务器的时间）",
                time(*from)
            ),
            CertProblem::Principal {
                username,
                principals,
            } => write!(
                f,
                "证书不允许以 {username} 登录（允许的用户名：{}）",
                principals.join(", ")
            ),
            CertProblem::Rejected { ca_fingerprint } => write!(
                f,
                "服务器没有接受证书：可能不信任签发它的 CA（{ca_fingerprint}），或证书已被吊销"
            ),
            CertProblem::KeyMismatch { certificate } => {
                write!(f, "{}与私钥对不上", name(certificate))
            }
            CertProblem::HostCertificate { certificate } => {
                write!(f, "{}是主机证书，不能用来登录", name(certificate))
            }
            CertProblem::Unreadable {
                certificate,
                reason,
            } => write!(f, "{}不能使用：{reason}", name(certificate)),
        }
    }
}

/// 本机能看出的、证书会被拒绝的原因：过期、还没生效、用户名不在允许的列表中。看不出时为 `None`
/// （服务器仍可能不接受，见 [`CertProblem::Rejected`]）。
pub fn diagnose(certificate: &Certificate, username: &str, now: i64) -> Option<CertProblem> {
    let info = CertInfo::of(certificate);
    match info.validity_at(now) {
        Validity::NotYetValid { from } => return Some(CertProblem::NotYetValid { from }),
        Validity::Expired { at } => return Some(CertProblem::Expired { at }),
        Validity::Valid { .. } => {}
    }
    if !info.allows(username) {
        return Some(CertProblem::Principal {
            username: username.to_string(),
            principals: info.principals,
        });
    }
    None
}

/// 证书被服务器拒绝时的说明：本机能看出原因的用它，否则是“服务器不接受（多半不信任 CA）”。
pub fn rejection(certificate: &Certificate, username: &str, now: i64) -> CertProblem {
    diagnose(certificate, username, now).unwrap_or_else(|| CertProblem::Rejected {
        ca_fingerprint: certificate
            .signature_key()
            .fingerprint(HashAlg::Sha256)
            .to_string(),
    })
}

/// 日志中的一行概要（不含秘密）：种类、密钥指纹、CA 指纹、序号、标识、有效期。
pub fn summary(certificate: &Certificate) -> String {
    let info = CertInfo::of(certificate);
    format!(
        "{:?} {} key={} ca={} serial={} id={:?} valid={}..{}",
        info.kind,
        info.algorithm,
        info.key_fingerprint,
        info.ca_fingerprint,
        info.serial,
        info.key_id,
        info.valid_after.map_or("always".into(), |t| t.to_string()),
        info.valid_before
            .map_or("forever".into(), |t| t.to_string()),
    )
}

/// 测试用：用 `ca` 给 `key` 签发证书（序号 42、标识 `test`、扩展项 `permit-pty`）；`principals` 为空时不限名字。
#[cfg(test)]
pub(crate) fn issue(
    ca: &ssh_key::PrivateKey,
    key: &KeyData,
    kind: CertKind,
    principals: &[&str],
    valid_after: u64,
    valid_before: u64,
) -> Certificate {
    let mut builder =
        ssh_key::certificate::Builder::new([7u8; 16], key.clone(), valid_after, valid_before)
            .unwrap();
    builder
        .cert_type(match kind {
            CertKind::User => CertType::User,
            CertKind::Host => CertType::Host,
        })
        .unwrap()
        .serial(42)
        .unwrap()
        .key_id("test")
        .unwrap()
        .extension("permit-pty", "")
        .unwrap();
    if principals.is_empty() {
        builder.all_principals_valid().unwrap();
    }
    for principal in principals {
        builder.valid_principal(*principal).unwrap();
    }
    builder.sign(ca).unwrap()
}

#[cfg(test)]
mod tests;
