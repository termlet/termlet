//! 解析、生成、导出的测试。
//!
//! 仓库中不存放 OpenSSH / PEM 私钥（gitleaks 会拦截）：这类测试材料在运行时构造——
//! 密钥由 ssh-key 随机生成或取自 `testdata/` 中的 ppk 向量，PKCS#8 由 russh 编码，
//! 再按需要拆成 PKCS#1、拼成 SEC1、加上旧式 PEM 加密；另有用系统 ssh-keygen、openssl 现场生成的兼容性测试（找不到时跳过）。

use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use ssh_key::encoding::Encode;
use ssh_key::private::{EcdsaKeypair, Ed25519Keypair, KeypairData};
use ssh_key::{Cipher, EcdsaCurve, HashAlg, Kdf, PrivateKey, PublicKey};

use super::der::{self, Reader, oid};
use super::*;

// ---- 测试材料 ----

pub(super) fn testdata(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/sshkey/testdata")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("读取 {}: {e}", path.display()))
}

pub(super) fn pass(s: &str) -> SecretString {
    SecretString::new(s)
}

/// 期望公钥（authorized_keys 格式）的指纹与去掉换行的整行。
pub(super) fn read_pub(name: &str) -> (String, String) {
    let text = String::from_utf8(testdata(name)).unwrap();
    let public = PublicKey::from_openssh(text.trim()).unwrap();
    (
        public.fingerprint(HashAlg::Sha256).to_string(),
        text.trim().to_string(),
    )
}

fn fingerprint_of(key: &PrivateKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// 各类测试密钥：Ed25519 与 ECDSA 随机生成，RSA 取自 ppk 向量（生成 RSA 在调试构建中较慢）。
fn test_keys() -> &'static [(&'static str, PrivateKey)] {
    static KEYS: OnceLock<Vec<(&'static str, PrivateKey)>> = OnceLock::new();
    KEYS.get_or_init(|| {
        let ecdsa = |curve| PrivateKey::from(EcdsaKeypair::random(&mut rng(), curve).unwrap());
        vec![
            (
                "ed25519",
                PrivateKey::from(Ed25519Keypair::random(&mut rng())),
            ),
            ("rsa", rsa2048()),
            ("p256", ecdsa(EcdsaCurve::NistP256)),
            ("p384", ecdsa(EcdsaCurve::NistP384)),
            ("p521", ecdsa(EcdsaCurve::NistP521)),
        ]
    })
}

fn test_key(name: &str) -> &'static PrivateKey {
    &test_keys().iter().find(|(n, _)| *n == name).unwrap().1
}

/// kayrus 向量中的 RSA-2048。
fn rsa2048() -> PrivateKey {
    super::ppk::decode(&testdata("kayrus/rsa2048-v3.ppk"), None).unwrap()
}

fn with_comment(key: &PrivateKey, comment: &str) -> PrivateKey {
    let mut key = key.clone();
    key.set_comment(comment);
    key
}

/// 用较少的 bcrypt 轮数加密 OpenSSH 私钥（ssh-keygen 默认 16 轮，测试里用 2 轮节省时间）。
fn encrypt_openssh(key: &PrivateKey, cipher: Cipher, passphrase: &str) -> String {
    let kdf = Kdf::Bcrypt {
        salt: vec![7; 16],
        rounds: 2,
    };
    let encrypted = key
        .encrypt_with(cipher, kdf, 0x1234_5678, passphrase)
        .unwrap();
    encrypted.to_openssh(LineEnding::LF).unwrap().to_string()
}

/// 编码 PEM 文本（每行 64 个字符）。
pub(super) fn pem_text(label: &str, headers: &[(&str, String)], der: &[u8]) -> String {
    let mut text = format!("-----BEGIN {label}-----\n");
    for (key, value) in headers {
        text.push_str(&format!("{key}: {value}\n"));
    }
    if !headers.is_empty() {
        text.push('\n');
    }
    let body = data_encoding::BASE64.encode(der);
    for line in body.as_bytes().chunks(64) {
        text.push_str(std::str::from_utf8(line).unwrap());
        text.push('\n');
    }
    text.push_str(&format!("-----END {label}-----\n"));
    text
}

/// PKCS#8 DER（由 russh 编码）。
fn pkcs8(key: &PrivateKey) -> Vec<u8> {
    russh::keys::pkcs8::encode_pkcs8(key).unwrap()
}

/// PKCS#8 中的私钥部分：RSA 为 PKCS#1 RSAPrivateKey。
fn pkcs8_inner(der: &[u8]) -> Vec<u8> {
    let mut outer = Reader::new(der);
    let mut info = Reader::new(outer.read(der::SEQUENCE).unwrap());
    info.read(der::INTEGER).unwrap();
    info.read(der::SEQUENCE).unwrap();
    info.read(der::OCTET_STRING).unwrap().to_vec()
}

fn pkcs1(key: &PrivateKey) -> Vec<u8> {
    pkcs8_inner(&pkcs8(key))
}

/// SEC1 ECPrivateKey（与 OpenSSL 相同，带曲线名与公钥）。
fn sec1(key: &PrivateKey) -> Vec<u8> {
    let KeypairData::Ecdsa(pair) = key.key_data() else {
        panic!("不是 ECDSA 密钥");
    };
    let curve = match pair.curve() {
        EcdsaCurve::NistP256 => oid::P256,
        EcdsaCurve::NistP384 => oid::P384,
        EcdsaCurve::NistP521 => oid::P521,
    };
    let public = [&[0u8][..], pair.public_key_bytes()].concat();
    der::sequence(&[
        &der::write(der::INTEGER, &[1]),
        &der::write(der::OCTET_STRING, pair.private_key_bytes()),
        &der::write(der::CONTEXT_0, &der::write(der::OID, curve)),
        &der::write(der::CONTEXT_1, &der::write(der::BIT_STRING, &public)),
    ])
    .to_vec()
}

/// PKCS#8 外壳，算法与参数任意（用于构造不支持的算法）。
fn pkcs8_with_algorithm(algorithm: &[u8], params: Option<&[u8]>, private: &[u8]) -> Vec<u8> {
    let mut alg = vec![der::write(der::OID, algorithm)];
    if let Some(params) = params {
        alg.push(der::write(der::OID, params));
    }
    let alg_refs: Vec<&[u8]> = alg.iter().map(|v| v.as_slice()).collect();
    der::sequence(&[
        &der::write(der::INTEGER, &[0]),
        &der::sequence(&alg_refs),
        &der::write(der::OCTET_STRING, private),
    ])
    .to_vec()
}

/// 旧式加密 PEM（`Proc-Type: 4,ENCRYPTED`），按 OpenSSL 的做法加密。
fn legacy_pem(label: &str, der: &[u8], cipher: &str, passphrase: &str) -> String {
    let (key_len, block) = match cipher {
        "AES-128-CBC" => (16, 16),
        "AES-256-CBC" => (32, 16),
        "DES-EDE3-CBC" => (24, 8),
        _ => unreachable!(),
    };
    let mut iv = vec![0u8; block];
    getrandom::fill(&mut iv).unwrap();
    let key = legacy::evp_bytes_to_key(passphrase.as_bytes(), &iv[..8], key_len);
    let pad = block - der.len() % block;
    let mut buf = der.to_vec();
    buf.extend(std::iter::repeat_n(pad as u8, pad));
    match cipher {
        "AES-128-CBC" => {
            Cipher::Aes128Cbc.encrypt(&key, &iv, &mut buf).unwrap();
        }
        "AES-256-CBC" => {
            Cipher::Aes256Cbc.encrypt(&key, &iv, &mut buf).unwrap();
        }
        _ => legacy::des_ede3_cbc_encrypt(&key, &iv, &mut buf),
    }
    let headers = [
        ("Proc-Type", "4,ENCRYPTED".to_string()),
        (
            "DEK-Info",
            format!("{cipher},{}", data_encoding::HEXUPPER.encode(&iv)),
        ),
    ];
    pem_text(label, &headers, &buf)
}

/// 断言私钥可用：签名能被期望公钥验证。
fn assert_usable(key: &Key, expected: &PublicKey) {
    let sig = key
        .ssh_private_key()
        .sign("termlet-test", HashAlg::Sha256, b"termlet sshkey test")
        .unwrap();
    expected
        .verify("termlet-test", b"termlet sshkey test", &sig)
        .unwrap();
}

// ---- OpenSSH ----

#[test]
fn openssh_round_trip() {
    for (name, key) in test_keys() {
        let want = fingerprint_of(key);
        let text = with_comment(key, "user@host")
            .to_openssh(LineEnding::LF)
            .unwrap();
        let parsed = parse(text.as_bytes(), None).unwrap();
        assert_eq!(parsed.format(), KeyFormat::OpenSsh, "{name}");
        assert_eq!(parsed.comment(), "user@host", "{name}");
        assert_eq!(parsed.fingerprint(), want, "{name}");
        let line = parsed.public_key();
        assert!(
            line.ends_with(" user@host") && !line.contains('\n'),
            "{name}: {line}"
        );
        assert_usable(&parsed, key.public_key());

        // 转成 OpenSSH 再解析：指纹与注释不变
        let again = parse(parsed.to_openssh().unwrap().expose().as_bytes(), None).unwrap();
        assert_eq!(
            (again.fingerprint(), again.comment()),
            (want.clone(), "user@host"),
            "{name}"
        );

        // 加密的 OpenSSH 私钥：缺少密码、密码错误、正确密码；解密后注释也能取到
        let encrypted =
            encrypt_openssh(&with_comment(key, "user@host"), Cipher::Aes256Ctr, "secret");
        assert_eq!(
            parse(encrypted.as_bytes(), None).unwrap_err(),
            KeyError::PassphraseRequired
        );
        assert_eq!(
            parse(encrypted.as_bytes(), Some(&pass(""))).unwrap_err(),
            KeyError::PassphraseRequired
        );
        assert_eq!(
            parse(encrypted.as_bytes(), Some(&pass("wrong"))).unwrap_err(),
            KeyError::WrongPassphrase
        );
        let decrypted = parse(encrypted.as_bytes(), Some(&pass("secret"))).unwrap();
        assert_eq!(decrypted.fingerprint(), want, "{name}");
        assert_eq!(decrypted.format(), KeyFormat::OpenSsh);
        assert_eq!(decrypted.comment(), "user@host");
    }
}

/// OpenSSH 支持的各种加密算法：密码错误都能识别为 WrongPassphrase。
#[test]
fn openssh_ciphers() {
    let key = test_key("ed25519");
    for cipher in [
        Cipher::Aes256Ctr,
        Cipher::Aes256Cbc,
        Cipher::Aes256Gcm,
        Cipher::ChaCha20Poly1305,
    ] {
        let encrypted = encrypt_openssh(key, cipher, "pw");
        assert_eq!(
            parse(encrypted.as_bytes(), Some(&pass("nope"))).unwrap_err(),
            KeyError::WrongPassphrase,
            "{cipher:?}"
        );
        assert_eq!(
            parse(encrypted.as_bytes(), Some(&pass("pw")))
                .unwrap()
                .fingerprint(),
            fingerprint_of(key)
        );
    }
}

/// 构造 OpenSSH 私钥的二进制外层（私钥部分是无效内容，只用于在解密前就被拒绝的情况）。
fn openssh_header(cipher: &str, kdf: &str, kdf_options: &[u8], algorithm: &str) -> Vec<u8> {
    let mut out = b"openssh-key-v1\0".to_vec();
    cipher.encode(&mut out).unwrap();
    kdf.encode(&mut out).unwrap();
    kdf_options.encode(&mut out).unwrap();
    1u32.encode(&mut out).unwrap();
    let mut public = Vec::new();
    algorithm.encode(&mut public).unwrap();
    [0u8; 32].as_slice().encode(&mut public).unwrap();
    public.as_slice().encode(&mut out).unwrap();
    [0u8; 16].as_slice().encode(&mut out).unwrap();
    out
}

fn openssh_pem(bin: &[u8]) -> String {
    pem_text("OPENSSH PRIVATE KEY", &[], bin)
}

#[test]
fn openssh_rejects_before_passphrase() {
    let bcrypt = |rounds: u32| {
        let mut options = Vec::new();
        [1u8; 16].as_slice().encode(&mut options).unwrap();
        rounds.encode(&mut options).unwrap();
        options
    };
    let cases = [
        (
            openssh_header("none", "none", &[], "ssh-dss"),
            Unsupported::Dsa,
        ),
        (
            openssh_header("none", "none", &[], "ssh-ed448"),
            Unsupported::Ed448,
        ),
        (
            openssh_header(
                "aes256-ctr",
                "bcrypt",
                &bcrypt(16),
                "sk-ssh-ed25519@openssh.com",
            ),
            Unsupported::SecurityKey,
        ),
        (
            openssh_header("none", "none", &[], "ecdsa-sha2-1.3.132.0.10"),
            Unsupported::Curve,
        ),
        (
            openssh_header("none", "none", &[], "ssh-foo"),
            Unsupported::Algorithm,
        ),
        (
            openssh_header("3des-cbc", "bcrypt", &bcrypt(16), "ssh-ed25519"),
            Unsupported::OpenSshCipher,
        ),
        (
            openssh_header("twofish-cbc", "bcrypt", &bcrypt(16), "ssh-ed25519"),
            Unsupported::OpenSshCipher,
        ),
        (
            openssh_header("aes256-ctr", "scrypt", &[], "ssh-ed25519"),
            Unsupported::OpenSshCipher,
        ),
        (
            openssh_header("aes256-ctr", "bcrypt", &bcrypt(1 << 20), "ssh-ed25519"),
            Unsupported::KdfTooCostly,
        ),
    ];
    for (bin, want) in cases {
        // 不提供私钥密码也直接返回不支持
        assert_eq!(
            parse(openssh_pem(&bin).as_bytes(), None).unwrap_err(),
            want.into(),
            "{want:?}"
        );
    }
    // 未加密却声明了 KDF
    let bin = openssh_header("none", "bcrypt", &bcrypt(16), "ssh-ed25519");
    assert_eq!(
        parse(openssh_pem(&bin).as_bytes(), None).unwrap_err(),
        Malformed::Corrupted.into()
    );
}

#[test]
fn comment_sanitized() {
    let text = with_comment(test_key("ed25519"), "a\r\nb\nc")
        .to_openssh(LineEnding::LF)
        .unwrap();
    let key = parse(text.as_bytes(), None).unwrap();
    assert_eq!(key.comment(), "a b c");
    assert!(!key.public_key().contains(['\r', '\n']));
}

// ---- PEM ----

#[test]
fn pem_formats() {
    let rsa = test_key("rsa");
    let ec = test_key("p384");
    let ed = test_key("ed25519");
    let cases = [
        ("pkcs1", pem_text("RSA PRIVATE KEY", &[], &pkcs1(rsa)), rsa),
        ("pkcs8-rsa", pem_text("PRIVATE KEY", &[], &pkcs8(rsa)), rsa),
        (
            "pkcs8-ed25519",
            pem_text("PRIVATE KEY", &[], &pkcs8(ed)),
            ed,
        ),
        ("pkcs8-ecdsa", pem_text("PRIVATE KEY", &[], &pkcs8(ec)), ec),
        ("sec1", pem_text("EC PRIVATE KEY", &[], &sec1(ec)), ec),
        (
            "sec1-p256",
            pem_text("EC PRIVATE KEY", &[], &sec1(test_key("p256"))),
            test_key("p256"),
        ),
        (
            "pkcs8-p521",
            pem_text("PRIVATE KEY", &[], &pkcs8(test_key("p521"))),
            test_key("p521"),
        ),
    ];
    for (name, text, key) in cases {
        // 前后带空白与 CRLF 也能解析；未加密时忽略私钥密码
        let data = format!("\r\n{}", text.replace('\n', "\r\n"));
        let parsed = parse(data.as_bytes(), Some(&pass("ignored")))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(parsed.format(), KeyFormat::Pem, "{name}");
        assert_eq!(parsed.comment(), "", "{name}");
        assert_eq!(parsed.fingerprint(), fingerprint_of(key), "{name}");
        assert_eq!(parsed.algorithm(), key.algorithm().as_str(), "{name}");
        assert_usable(&parsed, key.public_key());
    }
}

#[test]
fn pem_legacy_encrypted() {
    let rsa = test_key("rsa");
    for cipher in ["AES-128-CBC", "AES-256-CBC", "DES-EDE3-CBC"] {
        let text = legacy_pem("RSA PRIVATE KEY", &pkcs1(rsa), cipher, "secret");
        assert_eq!(
            parse(text.as_bytes(), None).unwrap_err(),
            KeyError::PassphraseRequired,
            "{cipher}"
        );
        // 旧式 PEM 无完整性校验，多试几次覆盖不同的随机解密结果
        for i in 0..20 {
            let wrong = pass(&format!("wrong{i}"));
            assert_eq!(
                parse(text.as_bytes(), Some(&wrong)).unwrap_err(),
                KeyError::WrongPassphrase,
                "{cipher}"
            );
        }
        let key = parse(text.as_bytes(), Some(&pass("secret"))).unwrap();
        assert_eq!(
            (key.fingerprint(), key.format()),
            (fingerprint_of(rsa), KeyFormat::Pem),
            "{cipher}"
        );
    }
    // SEC1 与 PKCS#8 也可以带旧式加密
    let ec = test_key("p256");
    let text = legacy_pem("EC PRIVATE KEY", &sec1(ec), "AES-128-CBC", "pw");
    assert_eq!(
        parse(text.as_bytes(), Some(&pass("pw")))
            .unwrap()
            .fingerprint(),
        fingerprint_of(ec)
    );
    let text = legacy_pem("PRIVATE KEY", &pkcs8(ec), "AES-256-CBC", "pw");
    assert_eq!(
        parse(text.as_bytes(), Some(&pass("pw")))
            .unwrap()
            .fingerprint(),
        fingerprint_of(ec)
    );

    // 不认识的加密算法
    let text = legacy_pem("RSA PRIVATE KEY", &pkcs1(rsa), "AES-128-CBC", "pw")
        .replace("AES-128-CBC", "CAMELLIA-128-CBC");
    assert_eq!(
        parse(text.as_bytes(), Some(&pass("pw"))).unwrap_err(),
        Unsupported::PemCipher.into()
    );
}

/// 证书与私钥放在同一个文件里：取私钥块。
#[test]
fn pem_bundle_picks_private_key() {
    let ec = test_key("p256");
    let text = format!(
        "subject=CN=test\n{}{}",
        pem_text("CERTIFICATE", &[], &[0x30, 0x00]),
        pem_text("EC PRIVATE KEY", &[], &sec1(ec))
    );
    assert_eq!(
        parse(text.as_bytes(), None).unwrap().fingerprint(),
        fingerprint_of(ec)
    );
}

#[test]
fn parse_rejects() {
    let (_, public_line) = read_pub("putty/ed25519.pub");
    let small = super::ppk::decode_unchecked(&testdata("kayrus/rsa512b-v2.ppk"), None).unwrap();
    let broken = pem_text("OPENSSH PRIVATE KEY", &[], &[0, 0, 0]);

    let malformed: Vec<(&str, Vec<u8>, Malformed)> = vec![
        ("empty", Vec::new(), Malformed::Empty),
        ("blank", b" \r\n\t".to_vec(), Malformed::Empty),
        ("garbage", b"hello world".to_vec(), Malformed::Unrecognized),
        (
            "authorized",
            public_line.clone().into_bytes(),
            Malformed::NotPrivateKey,
        ),
        (
            "ssh2-public",
            b"---- BEGIN SSH2 PUBLIC KEY ----\nAAAA\n---- END SSH2 PUBLIC KEY ----".to_vec(),
            Malformed::NotPrivateKey,
        ),
        (
            "public-pem",
            pem_text("PUBLIC KEY", &[], &[0x30, 0x00]).into_bytes(),
            Malformed::NotPrivateKey,
        ),
        (
            "certificate",
            pem_text("CERTIFICATE", &[], &[0x30, 0x00]).into_bytes(),
            Malformed::NotPrivateKey,
        ),
        ("broken-openssh", broken.into_bytes(), Malformed::Corrupted),
        (
            "bad-der",
            pem_text("RSA PRIVATE KEY", &[], &[1, 2, 3]).into_bytes(),
            Malformed::Corrupted,
        ),
        (
            "no-end",
            format!("-----BEGIN {}-----\nAAAA\n", "RSA PRIVATE KEY").into_bytes(),
            Malformed::Corrupted,
        ),
        (
            "bad-base64",
            pem_text("PRIVATE KEY", &[], &[0x30, 0x00])
                .replace("MAA=", "M@A=")
                .into_bytes(),
            Malformed::Corrupted,
        ),
        (
            "trailing-der",
            pem_text(
                "PRIVATE KEY",
                &[],
                &[pkcs8(test_key("ed25519")), vec![0]].concat(),
            )
            .into_bytes(),
            Malformed::Corrupted,
        ),
        ("too-large", vec![0; MAX_KEY_SIZE + 1], Malformed::TooLarge),
    ];
    for (name, data, want) in malformed {
        assert_eq!(parse(&data, None).unwrap_err(), want.into(), "{name}");
    }

    let unsupported: Vec<(&str, Vec<u8>, Unsupported)> = vec![
        (
            "rsa512",
            pem_text("RSA PRIVATE KEY", &[], &pkcs1(&small)).into_bytes(),
            Unsupported::RsaTooSmall { bits: 512 },
        ),
        (
            "dsa",
            pem_text("DSA PRIVATE KEY", &[], &[0]).into_bytes(),
            Unsupported::Dsa,
        ),
        (
            "pkcs8-encrypted",
            pem_text(
                "ENCRYPTED PRIVATE KEY",
                &[],
                &russh::keys::pkcs8::encode_pkcs8_encrypted(b"pw", 1, test_key("ed25519")).unwrap(),
            )
            .into_bytes(),
            Unsupported::EncryptedPkcs8,
        ),
        (
            "ssh1",
            b"SSH PRIVATE KEY FILE FORMAT 1.1\n...".to_vec(),
            Unsupported::Ssh1,
        ),
        (
            "sshcom",
            b"---- BEGIN SSH2 ENCRYPTED PRIVATE KEY ----\n...".to_vec(),
            Unsupported::SshCom,
        ),
        (
            "unknown-type",
            pem_text("FOO PRIVATE KEY", &[], &[0]).into_bytes(),
            Unsupported::PemType,
        ),
        (
            "pkcs8-ed448",
            pem_text(
                "PRIVATE KEY",
                &[],
                &pkcs8_with_algorithm(oid::ED448, None, &[0x04, 0x00]),
            )
            .into_bytes(),
            Unsupported::Ed448,
        ),
        (
            "pkcs8-dsa",
            pem_text(
                "PRIVATE KEY",
                &[],
                &pkcs8_with_algorithm(oid::DSA, None, &[0x02, 0x01, 0x01]),
            )
            .into_bytes(),
            Unsupported::Dsa,
        ),
        (
            "pkcs8-x25519",
            pem_text(
                "PRIVATE KEY",
                &[],
                &pkcs8_with_algorithm(&[0x2b, 0x65, 0x6e], None, &[0x04, 0x00]),
            )
            .into_bytes(),
            Unsupported::Algorithm,
        ),
        (
            "pkcs8-secp256k1",
            pem_text(
                "PRIVATE KEY",
                &[],
                &pkcs8_with_algorithm(
                    oid::EC_PUBLIC_KEY,
                    Some(&[0x2b, 0x81, 0x04, 0x00, 0x0a]),
                    &[0x30, 0x00],
                ),
            )
            .into_bytes(),
            Unsupported::Curve,
        ),
        (
            "sec1-secp256k1",
            pem_text("EC PRIVATE KEY", &[], &{
                // 曲线换成 secp256k1
                let sec1 = sec1(test_key("p256"));
                let k1 = der::write(der::OID, &[0x2b, 0x81, 0x04, 0x00, 0x0a]);
                let mut inner = Reader::new(&sec1);
                let mut seq = Reader::new(inner.read(der::SEQUENCE).unwrap());
                let version = seq.read(der::INTEGER).unwrap();
                let private = seq.read(der::OCTET_STRING).unwrap();
                der::sequence(&[
                    &der::write(der::INTEGER, version),
                    &der::write(der::OCTET_STRING, private),
                    &der::write(der::CONTEXT_0, &k1),
                ])
                .to_vec()
            })
            .into_bytes(),
            Unsupported::Curve,
        ),
    ];
    for (name, data, want) in unsupported {
        assert_eq!(parse(&data, None).unwrap_err(), want.into(), "{name}");
    }
}

// ---- 用系统的 ssh-keygen / openssl 现场生成，验证兼容性 ----

/// 执行外部命令；命令不存在时返回 `None`（调用方跳过）。
fn run(program: &str, args: &[&str]) -> Option<std::process::Output> {
    Command::new(program).args(args).output().ok()
}

#[test]
fn ssh_keygen_compat() {
    let dir = tempfile::tempdir().unwrap();
    struct Case {
        name: &'static str,
        kind: &'static str,
        bits: &'static str,
        format: &'static str,
        pass: &'static str,
        want_format: KeyFormat,
        want_comment: &'static str,
    }
    let cases = [
        Case {
            name: "ed25519",
            kind: "ed25519",
            bits: "",
            format: "",
            pass: "",
            want_format: KeyFormat::OpenSsh,
            want_comment: "c@d",
        },
        // 加密的 OpenSSH 私钥：解密后能取到注释
        Case {
            name: "ed25519-enc",
            kind: "ed25519",
            bits: "",
            format: "",
            pass: "secret",
            want_format: KeyFormat::OpenSsh,
            want_comment: "c@d",
        },
        Case {
            name: "rsa-enc",
            kind: "rsa",
            bits: "3072",
            format: "",
            pass: "secret",
            want_format: KeyFormat::OpenSsh,
            want_comment: "c@d",
        },
        Case {
            name: "ecdsa",
            kind: "ecdsa",
            bits: "384",
            format: "",
            pass: "",
            want_format: KeyFormat::OpenSsh,
            want_comment: "c@d",
        },
        Case {
            name: "rsa-pem-enc",
            kind: "rsa",
            bits: "2048",
            format: "PEM",
            pass: "secret",
            want_format: KeyFormat::Pem,
            want_comment: "",
        },
        Case {
            name: "ecdsa-pem",
            kind: "ecdsa",
            bits: "256",
            format: "PEM",
            pass: "",
            want_format: KeyFormat::Pem,
            want_comment: "",
        },
        Case {
            name: "ecdsa-pkcs8",
            kind: "ecdsa",
            bits: "521",
            format: "PKCS8",
            pass: "",
            want_format: KeyFormat::Pem,
            want_comment: "",
        },
        Case {
            name: "rsa-pkcs8",
            kind: "rsa",
            bits: "2048",
            format: "PKCS8",
            pass: "",
            want_format: KeyFormat::Pem,
            want_comment: "",
        },
    ];
    for c in cases {
        let path = dir.path().join(c.name);
        let path_str = path.to_str().unwrap();
        let mut args = vec![
            "-q", "-t", c.kind, "-N", c.pass, "-C", "c@d", "-f", path_str,
        ];
        if !c.bits.is_empty() {
            args.extend(["-b", c.bits]);
        }
        if !c.format.is_empty() {
            args.extend(["-m", c.format]);
        }
        let Some(out) = run("ssh-keygen", &args) else {
            eprintln!("未找到 ssh-keygen，跳过");
            return;
        };
        if !out.status.success() {
            eprintln!(
                "ssh-keygen 执行失败，跳过 {}: {}",
                c.name,
                String::from_utf8_lossy(&out.stderr)
            );
            continue;
        }
        let data = std::fs::read(&path).unwrap();
        let public = PublicKey::from_openssh(
            std::fs::read_to_string(path.with_extension("pub"))
                .unwrap()
                .trim(),
        )
        .unwrap();

        if !c.pass.is_empty() {
            assert_eq!(
                parse(&data, None).unwrap_err(),
                KeyError::PassphraseRequired,
                "{}",
                c.name
            );
            assert_eq!(
                parse(&data, Some(&pass("wrong"))).unwrap_err(),
                KeyError::WrongPassphrase,
                "{}",
                c.name
            );
        }
        let key = parse(&data, Some(&pass(c.pass))).unwrap_or_else(|e| panic!("{}: {e}", c.name));
        assert_eq!(key.format(), c.want_format, "{}", c.name);
        assert_eq!(key.comment(), c.want_comment, "{}", c.name);
        assert_eq!(
            key.fingerprint(),
            public.fingerprint(HashAlg::Sha256).to_string(),
            "{}",
            c.name
        );
        assert_usable(&key, &public);
    }
}

/// 用 openssl 生成传统格式（含旧式加密）的私钥：DES-EDE3、AES-256 与 Ed448、DSA 等不支持的算法。
#[test]
fn openssl_compat() {
    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| dir.path().join(name).to_str().unwrap().to_string();
    let Some(version) = run("openssl", &["version"]) else {
        eprintln!("未找到 openssl，跳过");
        return;
    };
    if !version.status.success() {
        eprintln!("openssl 不可用，跳过");
        return;
    }
    let gen_rsa = |name: &str, extra: &[&str]| {
        let out_path = path(name);
        let mut args = vec!["genrsa", "-out", &out_path];
        args.extend(extra);
        args.push("2048");
        let out = run("openssl", &args).unwrap();
        out.status
            .success()
            .then(|| std::fs::read(&out_path).unwrap())
    };
    // OpenSSL 3 的 genrsa 默认输出 PKCS#8；-traditional 输出 PKCS#1（加密时是旧式加密 PEM）
    for cipher in ["-des3", "-aes256", "-aes128"] {
        let Some(data) = gen_rsa(cipher, &["-traditional", cipher, "-passout", "pass:secret"])
        else {
            eprintln!("openssl genrsa {cipher} 失败，跳过");
            continue;
        };
        assert!(
            String::from_utf8_lossy(&data).contains("DEK-Info"),
            "{cipher}"
        );
        assert_eq!(
            parse(&data, None).unwrap_err(),
            KeyError::PassphraseRequired,
            "{cipher}"
        );
        assert_eq!(
            parse(&data, Some(&pass("wrong"))).unwrap_err(),
            KeyError::WrongPassphrase,
            "{cipher}"
        );
        let key = parse(&data, Some(&pass("secret"))).unwrap_or_else(|e| panic!("{cipher}: {e}"));
        assert_eq!(
            (key.algorithm(), key.bits(), key.format()),
            ("ssh-rsa", 2048, KeyFormat::Pem)
        );
    }
    if let Some(data) = gen_rsa("plain", &["-traditional"]) {
        assert_eq!(parse(&data, None).unwrap().bits(), 2048);
    }

    let genpkey = |name: &str, args: &[&str]| {
        let out_path = path(name);
        let mut all = vec!["genpkey", "-out", &out_path];
        all.extend(args);
        let out = run("openssl", &all).unwrap();
        out.status
            .success()
            .then(|| std::fs::read(&out_path).unwrap())
    };
    for (name, args, want) in [
        ("ed448", &["-algorithm", "ed448"][..], Unsupported::Ed448),
        (
            "x25519",
            &["-algorithm", "x25519"][..],
            Unsupported::Algorithm,
        ),
        (
            "secp256k1",
            &[
                "-algorithm",
                "EC",
                "-pkeyopt",
                "ec_paramgen_curve:secp256k1",
            ][..],
            Unsupported::Curve,
        ),
        (
            "rsa-pss",
            &["-algorithm", "RSA-PSS", "-pkeyopt", "rsa_keygen_bits:1024"][..],
            Unsupported::Algorithm,
        ),
    ] {
        match genpkey(name, args) {
            Some(data) => assert_eq!(parse(&data, None).unwrap_err(), want.into(), "{name}"),
            None => eprintln!("openssl genpkey {name} 失败，跳过"),
        }
    }
    if let Some(data) = genpkey("ed25519", &["-algorithm", "ed25519"]) {
        assert_eq!(parse(&data, None).unwrap().algorithm(), "ssh-ed25519");
    }
    // 加密的 PKCS#8
    if let Some(data) = genpkey(
        "enc",
        &["-algorithm", "ed25519", "-aes-256-cbc", "-pass", "pass:pw"],
    ) {
        assert_eq!(
            parse(&data, Some(&pass("pw"))).unwrap_err(),
            Unsupported::EncryptedPkcs8.into()
        );
    }
}

// ---- 生成与导出 ----

/// 本次测试运行中唯一一次生成 RSA（3072 位）。
fn generated_rsa() -> &'static Key {
    static KEY: OnceLock<Key> = OnceLock::new();
    KEY.get_or_init(|| generate(KeyKind::Rsa3072, "ruan@测试 机器").unwrap())
}

/// 生成的密钥可以被 parse 读回（公钥、指纹、注释一致），RSA 位数正确。
#[test]
fn generate_round_trip() {
    let ed = generate(KeyKind::Ed25519, "ruan@测试 机器").unwrap();
    for (key, algorithm, bits) in [
        (&ed, "ssh-ed25519", 256),
        (generated_rsa(), "ssh-rsa", 3072),
    ] {
        assert_eq!(
            (key.algorithm(), key.bits(), key.format()),
            (algorithm, bits, KeyFormat::OpenSsh)
        );
        let back = parse(key.to_openssh().unwrap().expose().as_bytes(), None).unwrap();
        assert_eq!(back.fingerprint(), key.fingerprint());
        assert_eq!(back.comment(), "ruan@测试 机器");
        assert_eq!(back.public_key(), key.public_key());
        assert!(
            key.public_key().ends_with(" ruan@测试 机器"),
            "{}",
            key.public_key()
        );
        // 保存的私钥 russh 也能直接读取，之后连接不再需要私钥密码
        let russh_key =
            russh::keys::decode_secret_key(key.to_openssh().unwrap().expose(), None).unwrap();
        assert_eq!(fingerprint_of(&russh_key), key.fingerprint());
    }
    // 默认（Ed25519），注释两端的空白去掉
    let key = generate(KeyKind::default(), "  c  ").unwrap();
    assert_eq!((key.algorithm(), key.comment()), ("ssh-ed25519", "c"));
}

#[test]
#[ignore = "调试构建中生成 RSA 4096 需要数秒到数十秒"]
fn generate_rsa4096() {
    let key = generate(KeyKind::Rsa4096, "").unwrap();
    assert_eq!((key.algorithm(), key.bits()), ("ssh-rsa", 4096));
}

#[test]
fn generate_rejects_comment() {
    for (comment, reason) in [
        ("a\nb", "control_characters"),
        ("a\0b", "control_characters"),
        ("a\tb", "control_characters"),
        ("a\u{7f}b", "control_characters"),
        (&"x".repeat(MAX_COMMENT_LEN + 1), "too_long"),
    ] {
        assert_eq!(
            generate(KeyKind::Ed25519, comment).unwrap_err(),
            KeyError::Invalid(Invalid::new("comment", reason)),
            "{comment:?}"
        );
    }
    // 256 个字符（含中文）可以
    assert!(generate(KeyKind::Ed25519, &"测".repeat(MAX_COMMENT_LEN)).is_ok());
}

#[test]
fn key_kind_serde() {
    for (kind, name) in [
        (KeyKind::Ed25519, "\"ed25519\""),
        (KeyKind::Rsa3072, "\"rsa3072\""),
        (KeyKind::Rsa4096, "\"rsa4096\""),
    ] {
        assert_eq!(serde_json::to_string(&kind).unwrap(), name);
        assert_eq!(serde_json::from_str::<KeyKind>(name).unwrap(), kind);
    }
    assert!(serde_json::from_str::<KeyKind>("\"dsa\"").is_err());
}

/// 带私钥密码导出：没有密码时提示需要密码，密码错误、正确密码分别能区分；不带密码导出可直接读取。
#[test]
fn export_with_passphrase() {
    let key = generate(KeyKind::Ed25519, "export").unwrap();
    let encrypted = key.export_openssh(Some(&pass("s3cret"))).unwrap();
    let text = encrypted.expose();
    assert!(text.contains("BEGIN OPENSSH PRIVATE KEY"), "{text}");
    assert_eq!(
        parse(text.as_bytes(), None).unwrap_err(),
        KeyError::PassphraseRequired
    );
    assert_eq!(
        parse(text.as_bytes(), Some(&pass("wrong"))).unwrap_err(),
        KeyError::WrongPassphrase
    );
    let back = parse(text.as_bytes(), Some(&pass("s3cret"))).unwrap();
    assert_eq!(
        (back.fingerprint(), back.comment()),
        (key.fingerprint(), "export")
    );
    // 与 ssh-keygen 相同：aes256-ctr + bcrypt 16 轮
    let raw = PrivateKey::from_openssh(text).unwrap();
    assert_eq!(raw.cipher(), Cipher::Aes256Ctr);
    assert!(matches!(raw.kdf(), Kdf::Bcrypt { rounds: 16, .. }));

    for passphrase in [None, Some(pass(""))] {
        let plain = key.export_openssh(passphrase.as_ref()).unwrap();
        let back = parse(plain.expose().as_bytes(), None).unwrap();
        assert_eq!(
            (back.fingerprint(), back.comment()),
            (key.fingerprint(), "export")
        );
    }
}

/// 用系统的 ssh-keygen 读取生成并加密导出的私钥，确认与 OpenSSH 兼容。
#[test]
fn exported_key_with_ssh_keygen() {
    let dir = tempfile::tempdir().unwrap();
    let ed = generate(KeyKind::Ed25519, "keygen-compat").unwrap();
    for (name, key) in [("ed25519", &ed), ("rsa", generated_rsa())] {
        let data = key.export_openssh(Some(&pass("pw-compat"))).unwrap();
        let path = dir.path().join(format!("id_{name}"));
        std::fs::write(&path, data.expose()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let Some(out) = run(
            "ssh-keygen",
            &["-y", "-P", "pw-compat", "-f", path.to_str().unwrap()],
        ) else {
            eprintln!("未找到 ssh-keygen，跳过");
            return;
        };
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() {
            // Windows 上 ssh-keygen 会检查文件 ACL，临时目录的 ACL 不符合要求时跳过
            if stderr.contains("UNPROTECTED") || stderr.contains("bad permissions") {
                eprintln!("ssh-keygen 拒绝了临时文件的权限，跳过: {stderr}");
                return;
            }
            panic!("ssh-keygen 读取失败: {stderr}");
        }
        let public = PublicKey::from_openssh(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
        assert_eq!(
            public.fingerprint(HashAlg::Sha256).to_string(),
            key.fingerprint(),
            "{name}"
        );
    }
}

// ---- 密钥库条目 ----

#[test]
fn key_entry_and_default_name() {
    let key = generate(KeyKind::Ed25519, "ruan@laptop").unwrap();
    let entry = key.to_key_entry("  ");
    assert_eq!(entry.name, "ruan@laptop");
    assert_eq!(entry.algorithm, "ssh-ed25519");
    assert_eq!(entry.bits, 256);
    assert_eq!(entry.fingerprint, key.fingerprint());
    assert!(entry.fingerprint.starts_with("SHA256:"));
    assert!(
        entry.public_key.starts_with("ssh-ed25519 AAAA")
            && entry.public_key.ends_with(" ruan@laptop")
    );
    assert_eq!(key.to_key_entry(" 部署 ").name, "部署");

    // 没有注释：用"类型 + 指纹前几位"；公钥行只有两段
    let bare = generate(KeyKind::Ed25519, "").unwrap();
    let hash = bare
        .fingerprint()
        .strip_prefix("SHA256:")
        .unwrap()
        .chars()
        .take(8)
        .collect::<String>();
    assert_eq!(bare.default_name(), format!("ed25519 {hash}"));
    assert_eq!(bare.public_key().split(' ').count(), 2);
    let ecdsa = parse(
        pem_text("EC PRIVATE KEY", &[], &sec1(test_key("p256"))).as_bytes(),
        None,
    )
    .unwrap();
    assert!(
        ecdsa.default_name().starts_with("ecdsa-sha2-nistp256 "),
        "{}",
        ecdsa.default_name()
    );
    assert_eq!(ecdsa.bits(), 256);

    // 注释过长时截断到 64 个字符
    let long = generate(KeyKind::Ed25519, &"长".repeat(70)).unwrap();
    assert_eq!(long.default_name().chars().count(), 64);
}

#[test]
fn debug_hides_private_key() {
    let key = generate(KeyKind::Ed25519, "dbg").unwrap();
    let text = format!("{key:?}");
    assert!(
        text.contains("ssh-ed25519") && text.contains(&key.fingerprint()),
        "{text}"
    );
    let pem = key.to_openssh().unwrap();
    let body = pem.expose().lines().nth(1).unwrap();
    assert!(!text.contains(body), "{text}");
    assert_eq!(format!("{pem:?}"), "SecretString(***)");
}
