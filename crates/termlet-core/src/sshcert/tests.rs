use super::*;
use crate::sshkey::{self, KeyKind};

fn new_key() -> ssh_key::PrivateKey {
    sshkey::generate(KeyKind::Ed25519, "")
        .unwrap()
        .into_ssh_private_key()
}

const HOUR: u64 = 3600;
const NOW: i64 = 1_800_000_000;

fn user_cert(principals: &[&str], after: u64, before: u64) -> (ssh_key::PrivateKey, Certificate) {
    let (ca, key) = (new_key(), new_key());
    let cert = issue(
        &ca,
        key.public_key().key_data(),
        CertKind::User,
        principals,
        after,
        before,
    );
    (key, cert)
}

// 解析 OpenSSH 的一行证书（BOM、注释、首尾空白不影响），信息与签发时一致
#[test]
fn parse_and_info() {
    let ca = new_key();
    let key = new_key();
    let mut cert = issue(
        &ca,
        key.public_key().key_data(),
        CertKind::User,
        &["deploy", "ops"],
        NOW as u64 - HOUR,
        NOW as u64 + HOUR,
    );
    cert.set_comment("ruan@laptop");
    let line = cert.to_openssh().unwrap();
    assert!(line.starts_with("ssh-ed25519-cert-v01@openssh.com "));
    let text = format!("\u{feff}# 证书\n\n  {line}  \n");
    let parsed = parse(&text).unwrap();
    assert_eq!(parsed, cert);

    let info = CertInfo::of(&parsed);
    assert_eq!(info.kind, CertKind::User);
    assert_eq!(info.algorithm, "ssh-ed25519");
    assert_eq!(
        info.key_fingerprint,
        key.public_key().fingerprint(HashAlg::Sha256).to_string()
    );
    assert_eq!(
        info.ca_fingerprint,
        ca.public_key().fingerprint(HashAlg::Sha256).to_string()
    );
    assert_eq!(info.ca_algorithm, "ssh-ed25519");
    assert_eq!(info.serial, 42);
    assert_eq!(info.key_id, "test");
    assert_eq!(info.principals, ["deploy", "ops"]);
    assert_eq!(info.valid_after, Some(NOW - HOUR as i64));
    assert_eq!(info.valid_before, Some(NOW + HOUR as i64));
    assert_eq!(info.extensions, [("permit-pty".to_string(), String::new())]);
    assert!(info.critical_options.is_empty());
    assert_eq!(info.comment, "ruan@laptop");
}

#[test]
fn parse_rejects_non_certificates() {
    let key = new_key();
    assert_eq!(parse(""), Err(CertError::Empty));
    assert_eq!(parse("\n# 只有注释\n"), Err(CertError::Empty));
    let public = key.public_key().to_openssh().unwrap();
    assert_eq!(parse(&public), Err(CertError::NotCertificate));
    let private = sshkey::generate(KeyKind::Ed25519, "")
        .unwrap()
        .to_openssh()
        .unwrap();
    assert_eq!(parse(private.expose()), Err(CertError::NotCertificate));
    assert_eq!(
        parse("ssh-ed25519-cert-v01@openssh.com AAAAbroken"),
        Err(CertError::Malformed)
    );
    assert_eq!(parse("hello world"), Err(CertError::Malformed));
    assert_eq!(
        parse(&"a".repeat(MAX_TEXT_LEN + 1)),
        Err(CertError::TooLarge)
    );
    // CA 的签名对不上（证书被改过）
    let (_, cert) = user_cert(&["ops"], 0, u64::MAX);
    let mut bytes = cert.to_bytes().unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    let tampered = format!(
        "ssh-ed25519-cert-v01@openssh.com {}",
        data_encoding::BASE64.encode(&bytes)
    );
    assert_eq!(parse(&tampered), Err(CertError::Malformed));
}

// 有效期：生效时间 ≤ 现在 < 失效时间；always / forever 不限
#[test]
fn validity() {
    let (_, cert) = user_cert(&["ops"], NOW as u64, NOW as u64 + HOUR);
    let info = CertInfo::of(&cert);
    assert_eq!(
        info.validity_at(NOW - 1),
        Validity::NotYetValid { from: NOW }
    );
    assert_eq!(
        info.validity_at(NOW),
        Validity::Valid {
            until: Some(NOW + HOUR as i64)
        }
    );
    assert_eq!(
        info.validity_at(NOW + HOUR as i64),
        Validity::Expired {
            at: NOW + HOUR as i64
        }
    );
    let (_, forever) = user_cert(&[], 0, u64::MAX);
    let info = CertInfo::of(&forever);
    assert_eq!((info.valid_after, info.valid_before), (None, None));
    assert_eq!(info.validity_at(NOW), Validity::Valid { until: None });
    assert!(info.allows("anyone"), "没有列出用户名时不限");

    assert!(expires_soon(NOW + 600, NOW));
    assert!(!expires_soon(NOW + EXPIRING_SOON_SECS, NOW));
    assert!(!expires_soon(NOW - 1, NOW), "已过期的不算快过期");
}

// 被拒绝时的说明：过期、未生效、用户名不在允许的列表中；看不出原因时说多半不信任 CA
#[test]
fn diagnosis() {
    let (_, cert) = user_cert(&["deploy"], NOW as u64 - 2 * HOUR, NOW as u64 - HOUR);
    let expired = NOW - HOUR as i64;
    assert_eq!(
        diagnose(&cert, "deploy", NOW),
        Some(CertProblem::Expired { at: expired })
    );
    let text = CertProblem::Expired { at: expired }.to_string();
    assert!(
        text.starts_with("证书已于 ") && text.ends_with("过期，请重新签发"),
        "{text}"
    );
    assert!(text.contains(&crate::time::format_local(expired * 1000)));

    let (_, cert) = user_cert(&["deploy"], NOW as u64 + HOUR, NOW as u64 + 2 * HOUR);
    assert_eq!(
        diagnose(&cert, "deploy", NOW),
        Some(CertProblem::NotYetValid {
            from: NOW + HOUR as i64
        })
    );

    let (_, cert) = user_cert(&["deploy", "ops"], 0, u64::MAX);
    let problem = diagnose(&cert, "root", NOW).unwrap();
    assert_eq!(
        problem,
        CertProblem::Principal {
            username: "root".into(),
            principals: vec!["deploy".into(), "ops".into()],
        }
    );
    assert_eq!(
        problem.to_string(),
        "证书不允许以 root 登录（允许的用户名：deploy, ops）"
    );
    assert_eq!(diagnose(&cert, "ops", NOW), None);
    match rejection(&cert, "ops", NOW) {
        CertProblem::Rejected { ca_fingerprint } => {
            assert_eq!(ca_fingerprint, CertInfo::of(&cert).ca_fingerprint)
        }
        other => panic!("{other:?}"),
    }
}

// 证书与私钥的配对：证书里的公钥就是私钥的公钥
#[test]
fn pairing_with_private_key() {
    let (key, cert) = user_cert(&["ops"], 0, u64::MAX);
    assert!(matches_key(&cert, key.public_key().key_data()));
    let other = new_key();
    assert!(!matches_key(&cert, other.public_key().key_data()));
    let line = key.public_key().to_openssh().unwrap();
    assert!(matches_public_key(&cert, &format!("{line} comment")));
    assert!(!matches_public_key(
        &cert,
        &other.public_key().to_openssh().unwrap()
    ));
    assert!(!matches_public_key(&cert, "not a key"));
}

// 私钥文件旁的证书：先 `<私钥>-cert`，再 `<私钥>-cert.pub`（与 OpenSSH 相同）；只认文件
#[test]
fn certificate_beside_key_file() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("id_ed25519");
    let [plain, with_pub] = beside(&key);
    assert_eq!(plain, dir.path().join("id_ed25519-cert"));
    assert_eq!(with_pub, dir.path().join("id_ed25519-cert.pub"));
    assert_eq!(find_beside(&key), None);
    std::fs::write(&with_pub, "x").unwrap();
    assert_eq!(find_beside(&key), Some(with_pub.clone()));
    std::fs::create_dir(&plain).unwrap();
    assert_eq!(find_beside(&key), Some(with_pub), "同名的目录不算");
    std::fs::remove_dir(&plain).unwrap();
    std::fs::write(&plain, "x").unwrap();
    assert_eq!(find_beside(&key), Some(plain));

    // 带扩展名的私钥（PuTTY 等）同样在完整文件名后面加
    assert_eq!(
        beside(Path::new("/k/work.ppk"))[1],
        Path::new("/k/work.ppk-cert.pub")
    );

    // 读文件：过大的不读
    let big = dir.path().join("big-cert.pub");
    std::fs::write(&big, vec![b'a'; MAX_TEXT_LEN + 1]).unwrap();
    assert!(read_file(&big).is_err());
    assert!(read_file(dir.path()).is_err(), "目录");
}

/// `ssh-keygen -s ca -I ui-test -z 7 -n deploy,ops -V always:forever -O no-x11-forwarding user.pub` 签发的证书
/// 与对应的公钥（证书与公钥都是公开的内容，私钥没有保存）。
const SSH_KEYGEN_PUBLIC_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICbe1igcT4sGwlBXnjShpoUBNNsmCXkTQ3M0Wigq9xzy ui@test";
const SSH_KEYGEN_CERTIFICATE: &str = "ssh-ed25519-cert-v01@openssh.com AAAAIHNzaC1lZDI1NTE5LWNlcnQtdjAxQG9wZW5zc2guY29tAAAAIGQpp1S5wvgGEEY14QlbUPW8DW7Ni7RBI6xfoaCXTXkoAAAAICbe1igcT4sGwlBXnjShpoUBNNsmCXkTQ3M0Wigq9xzyAAAAAAAAAAcAAAABAAAAB3VpLXRlc3QAAAARAAAABmRlcGxveQAAAANvcHMAAAAAAAAAAP//////////AAAAAAAAAGUAAAAXcGVybWl0LWFnZW50LWZvcndhcmRpbmcAAAAAAAAAFnBlcm1pdC1wb3J0LWZvcndhcmRpbmcAAAAAAAAACnBlcm1pdC1wdHkAAAAAAAAADnBlcm1pdC11c2VyLXJjAAAAAAAAAAAAAAAzAAAAC3NzaC1lZDI1NTE5AAAAIONm4WSHmInVaSsRZw7Wwa5oEpzqhjBVuvy+flV8sr7oAAAAUwAAAAtzc2gtZWQyNTUxOQAAAEBqvf4vRXCDpYqTSAF+3xUf+FRfVzNkE9Bq87r/dRSCH7NvBXuQwhEm6Iqou0qdbyGkfpQGqyfUfRpEiBJn3J8N ui@test";

// OpenSSH 的 ssh-keygen 签发的证书：各项信息与 `ssh-keygen -L` 一致，与公钥配对
#[test]
fn certificate_from_ssh_keygen() {
    let cert = parse(SSH_KEYGEN_CERTIFICATE).unwrap();
    let info = CertInfo::of(&cert);
    assert_eq!(info.kind, CertKind::User);
    assert_eq!(info.algorithm, "ssh-ed25519");
    assert_eq!(info.serial, 7);
    assert_eq!(info.key_id, "ui-test");
    assert_eq!(info.principals, ["deploy", "ops"]);
    assert_eq!((info.valid_after, info.valid_before), (None, None));
    assert_eq!(
        info.extensions
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>(),
        [
            "permit-agent-forwarding",
            "permit-port-forwarding",
            "permit-pty",
            "permit-user-rc"
        ],
        "-O no-x11-forwarding 去掉了 permit-X11-forwarding"
    );
    assert!(info.critical_options.is_empty());
    assert_eq!(info.comment, "ui@test");
    assert!(matches_public_key(&cert, SSH_KEYGEN_PUBLIC_KEY));
    assert_eq!(diagnose(&cert, "ops", NOW), None);
}

#[test]
fn host_certificate_kind() {
    let (ca, key) = (new_key(), new_key());
    let cert = issue(
        &ca,
        key.public_key().key_data(),
        CertKind::Host,
        &["web.example.com"],
        0,
        u64::MAX,
    );
    let info = CertInfo::of(&parse(&cert.to_openssh().unwrap()).unwrap());
    assert_eq!(info.kind, CertKind::Host);
    assert!(info.allows("web.example.com") && !info.allows("db.example.com"));
    assert!(summary(&cert).starts_with("Host ssh-ed25519 key=SHA256:"));
}
