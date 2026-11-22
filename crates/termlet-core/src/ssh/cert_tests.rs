//! OpenSSH 证书登录与主机证书的测试。
//!
//! - 进程内的测试服务器（russh 的服务端）：只开公钥认证，信任一个用户 CA（相当于 `TrustedUserCAKeys`），
//!   另可在 `authorized_keys` 中放几把公钥、出示主机证书。总是运行。
//! - 假的 ssh-agent（Windows 的命名管道、其他系统的 Unix 套接字）：列出一张证书并用对应的私钥签名，
//!   不碰本机真正的 agent。
//! - 真实的 OpenSSH：设置 `TERMLET_TEST_CERT=127.0.0.1:2227`（testing/sshcert 的临时容器，用法见其 Dockerfile 注释）。
//!   证书用容器里的 `ssh-keygen -s` 现场签发（`docker exec`，容器名默认 `termlet-cert-sshd`，
//!   可用 `TERMLET_TEST_CERT_CONTAINER` 指定）；没有设置时跳过。

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::keys::{Certificate, HashAlg, PrivateKey, PublicKey};
use russh::server::{Auth, ChannelOpenHandle, Handler, Msg, Session, run_stream};
use russh::{Channel, ChannelOpenFailure, MethodKind, MethodSet};
use tokio::net::TcpListener;

use super::*;
use crate::model::{Auth as HostAuth, Host, KeyEntry, KeySource, SecretField};
use crate::secret::SecretString;
use crate::sshcert::{self, CertKind, CertProblem};
use crate::sshkey::{self, KeyKind};
use crate::store::{Owner, Position, Store};
use crate::testkit::memory_store;

pub(crate) fn new_key() -> PrivateKey {
    sshkey::generate(KeyKind::Ed25519, "")
        .unwrap()
        .into_ssh_private_key()
}

fn rsa_key() -> PrivateKey {
    sshkey::parse(
        include_bytes!("../sshkey/testdata/kayrus/rsa2048-v3.ppk"),
        None,
    )
    .unwrap()
    .into_ssh_private_key()
}

fn openssh(key: &PrivateKey) -> String {
    key.to_openssh(russh::keys::ssh_key::LineEnding::LF)
        .unwrap()
        .to_string()
}

const USER: &str = "deploy";
const HOUR: u64 = 3600;

fn now() -> u64 {
    sshcert::now_secs() as u64
}

/// 用 `ca` 给 `key` 签发用户证书。
fn user_cert(
    ca: &PrivateKey,
    key: &PrivateKey,
    principals: &[&str],
    after: u64,
    before: u64,
) -> Certificate {
    sshcert::issue(
        ca,
        key.public_key().key_data(),
        CertKind::User,
        principals,
        after,
        before,
    )
}

// ---------------------------------------------------------------- 进程内的测试服务器

struct ServerPolicy {
    user_ca: PublicKey,
    /// authorized_keys 中的公钥
    authorized: Vec<PublicKey>,
    /// 登录成功用的是什么：`cert` / `key`
    logins: Mutex<Vec<&'static str>>,
}

struct CertServer {
    port: u16,
    policy: Arc<ServerPolicy>,
    host_key: PrivateKey,
    accept: tokio::task::JoinHandle<()>,
}

impl Drop for CertServer {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl CertServer {
    /// 在 127.0.0.1 的随机端口上启动；`host_cert` 给出时出示主机证书（由它签发主机密钥）。
    async fn start(
        user_ca: &PrivateKey,
        authorized: Vec<PublicKey>,
        host_cert: Option<(&PrivateKey, &[&str])>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let host_key = new_key();
        let certificates = host_cert
            .map(|(ca, names)| {
                sshcert::issue(
                    ca,
                    host_key.public_key().key_data(),
                    CertKind::Host,
                    names,
                    0,
                    u64::MAX,
                )
            })
            .into_iter()
            .collect();
        let config = Arc::new(russh::server::Config {
            keys: vec![host_key.clone()],
            certificates,
            methods: MethodSet::from(&[MethodKind::PublicKey][..]),
            auth_rejection_time: Duration::from_millis(10),
            auth_rejection_time_initial: Some(Duration::ZERO),
            inactivity_timeout: None,
            ..Default::default()
        });
        let policy = Arc::new(ServerPolicy {
            user_ca: user_ca.public_key().clone(),
            authorized,
            logins: Mutex::new(Vec::new()),
        });
        let shared = policy.clone();
        let accept = crate::runtime::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let handler = PolicyHandler(shared.clone());
                let config = config.clone();
                crate::runtime::spawn(async move {
                    if let Ok(session) = run_stream(config, stream, handler).await {
                        let _ = session.await;
                    }
                });
            }
        });
        Self {
            port,
            policy,
            host_key,
            accept,
        }
    }

    fn logins(&self) -> Vec<&'static str> {
        self.policy.logins.lock().unwrap().clone()
    }
}

struct PolicyHandler(Arc<ServerPolicy>);

impl Handler for PolicyHandler {
    type Error = russh::Error;

    async fn auth_publickey(
        &mut self,
        _user: &str,
        key: russh::server::VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        if self
            .0
            .authorized
            .iter()
            .any(|k| k.key_data() == key.public_key.key_data())
        {
            self.0.logins.lock().unwrap().push("key");
            return Ok(Auth::Accept);
        }
        Ok(Auth::reject())
    }

    // 与 sshd 的 TrustedUserCAKeys 相同：CA 签名、有效期、用户证书、用户名在允许的列表中
    async fn auth_openssh_certificate(
        &mut self,
        user: &str,
        certificate: &Certificate,
    ) -> Result<Auth, Self::Error> {
        let ca = self.0.user_ca.fingerprint(HashAlg::Sha256);
        let info = sshcert::CertInfo::of(certificate);
        if certificate.validate([&ca]).is_ok() && info.kind == CertKind::User && info.allows(user) {
            self.0.logins.lock().unwrap().push("cert");
            return Ok(Auth::Accept);
        }
        Ok(Auth::reject())
    }

    // 作为跳板机：把 direct-tcpip 通道接到本机的端口上
    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host_to_connect: &str,
        port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let port = u16::try_from(port_to_connect).unwrap_or(0);
        match tokio::net::TcpStream::connect((host_to_connect, port)).await {
            Ok(mut target) => {
                reply.accept().await;
                crate::runtime::spawn(async move {
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut stream, &mut target).await;
                });
            }
            Err(_) => reply.reject(ChannelOpenFailure::ConnectFailed).await,
        }
        Ok(())
    }
}

/// 一台用私钥文件登录测试服务器的主机；`certificate` 是指定的证书文件（空：自动找旁边的 -cert.pub）。
fn key_file_host(
    store: &Store,
    port: u16,
    key_path: &Path,
    certificate: &str,
) -> crate::id::HostId {
    store
        .create(
            Host {
                name: "cert-test".into(),
                address: "127.0.0.1".into(),
                port,
                username: USER.into(),
                auth: HostAuth::Key {
                    source: KeySource::File {
                        path: key_path.display().to_string(),
                        certificate: certificate.into(),
                    },
                },
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap()
}

/// 接受指纹，记下问了几次；其余取消。
fn counting_prompter() -> (Prompter, Arc<Mutex<Vec<String>>>) {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let log = asked.clone();
    let prompter: Prompter = Arc::new(move |prompt| {
        let log = log.clone();
        Box::pin(async move {
            match prompt {
                Prompt::HostKey(p) => {
                    log.lock().unwrap().push(p.fingerprint);
                    Answer::AcceptHostKey
                }
                _ => Answer::Cancel,
            }
        })
    });
    (prompter, asked)
}

async fn connect_host(
    store: &Arc<Store>,
    host: &crate::id::HostId,
) -> Result<Arc<Connection>, SshError> {
    let plan = plan_for_host(store, host).unwrap();
    connect(
        store.clone(),
        &plan,
        counting_prompter().0,
        &ConnectOptions::default(),
    )
    .await
}

/// 把私钥写进临时目录（`name`），旁边按需放 `name-cert.pub`。
pub(crate) fn write_key(
    dir: &Path,
    name: &str,
    key: &PrivateKey,
    cert: Option<&Certificate>,
) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, openssh(key)).unwrap();
    if let Some(cert) = cert {
        std::fs::write(
            dir.join(format!("{name}-cert.pub")),
            cert.to_openssh().unwrap(),
        )
        .unwrap();
    }
    path
}

// 私钥文件旁同名的 -cert.pub 自动出示；登录后知道证书的失效时间
#[test]
fn certificate_beside_key_file_logs_in() {
    crate::runtime::runtime().block_on(async {
        let (ca, key) = (new_key(), new_key());
        let server = CertServer::start(&ca, Vec::new(), None).await;
        let before = now() + HOUR / 2;
        let cert = user_cert(&ca, &key, &[USER], now() - 60, before);
        let dir = tempfile::tempdir().unwrap();
        let path = write_key(dir.path(), "id_ed25519", &key, Some(&cert));
        let store = memory_store();
        let host = key_file_host(&store, server.port, &path, "");
        let connection = connect_host(&store, &host).await.unwrap();
        assert_eq!(server.logins(), ["cert"]);
        assert_eq!(connection.certificate_expiry(), Some(before as i64));
        assert!(
            sshcert::expires_soon(before as i64, sshcert::now_secs()),
            "剩半小时：快过期"
        );
    });
}

// 经用证书登录的跳板机：跳板机的证书快过期时知道是哪一跳、何时过期（目标主机用私钥登录，没有证书的失效时间）；
// 两跳都用证书时从第一跳起列出跳板机的
#[test]
fn jump_host_certificate_expiry() {
    crate::runtime::runtime().block_on(async {
        let (ca, jump_key, target_key) = (new_key(), new_key(), new_key());
        let jump = CertServer::start(&ca, Vec::new(), None).await;
        let target = CertServer::start(&ca, vec![target_key.public_key().clone()], None).await;
        let before = now() + HOUR / 2;
        let dir = tempfile::tempdir().unwrap();
        let jump_cert = user_cert(&ca, &jump_key, &[USER], now() - 60, before);
        let jump_path = write_key(dir.path(), "jump", &jump_key, Some(&jump_cert));
        let target_path = write_key(dir.path(), "target", &target_key, None);
        let store = memory_store();
        let bastion = key_file_host(&store, jump.port, &jump_path, "");
        let host = key_file_host(&store, target.port, &target_path, "");
        let mut doc = store.get::<Host>(&host).unwrap().doc;
        doc.name = "app".into();
        doc.jump = crate::model::Jump::Via {
            host: bastion.clone(),
        };
        store.update(&host, doc).unwrap();
        let mut doc = store.get::<Host>(&bastion).unwrap().doc;
        doc.name = "bastion".into();
        store.update(&bastion, doc).unwrap();

        let connection = connect_host(&store, &host).await.unwrap();
        assert_eq!(
            (jump.logins(), target.logins()),
            (vec!["cert"], vec!["key"])
        );
        assert_eq!(connection.certificate_expiry(), None);
        assert_eq!(
            connection.jump_certificate_expiries(),
            [("bastion".to_string(), before as i64)]
        );

        // 目标主机也用证书登录：它的证书另算，跳板机的照样列出
        let target_cert = user_cert(&ca, &target_key, &[USER], 0, before + HOUR);
        let target_path = write_key(dir.path(), "target2", &target_key, Some(&target_cert));
        let mut doc = store.get::<Host>(&host).unwrap().doc;
        doc.auth = HostAuth::Key {
            source: KeySource::file(target_path.display().to_string()),
        };
        store.update(&host, doc).unwrap();
        let connection = connect_host(&store, &host).await.unwrap();
        assert_eq!(
            connection.certificate_expiry(),
            Some((before + HOUR) as i64)
        );
        assert_eq!(
            connection.jump_certificate_expiries(),
            [("bastion".to_string(), before as i64)]
        );
    });
}

// 粘贴的私钥附带的证书：先出示证书，登录后知道失效时间
#[test]
fn pasted_key_with_certificate() {
    crate::runtime::runtime().block_on(async {
        let (ca, key) = (new_key(), new_key());
        let server = CertServer::start(&ca, Vec::new(), None).await;
        let before = now() + 2 * HOUR;
        let store = memory_store();
        let host = store
            .create(
                Host {
                    name: "pasted".into(),
                    address: "127.0.0.1".into(),
                    port: server.port,
                    username: USER.into(),
                    auth: HostAuth::Key {
                        source: KeySource::Pasted {
                            certificate: user_cert(&ca, &key, &[USER], 0, before)
                                .to_openssh()
                                .unwrap(),
                        },
                    },
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
        store
            .set_secret(
                &Owner::host(&host),
                SecretField::PrivateKey,
                Some(&SecretString::new(openssh(&key))),
            )
            .unwrap();
        let connection = connect_host(&store, &host).await.unwrap();
        assert_eq!(server.logins(), ["cert"]);
        assert_eq!(connection.certificate_expiry(), Some(before as i64));
    });
}

// 证书被拒绝时说明原因：过期、用户名不在允许的列表中、CA 不被信任；私钥本身也不能登录
#[test]
fn rejected_certificates_are_explained() {
    crate::runtime::runtime().block_on(async {
        let (ca, other_ca, key) = (new_key(), new_key(), new_key());
        let server = CertServer::start(&ca, Vec::new(), None).await;
        let dir = tempfile::tempdir().unwrap();
        let store = memory_store();
        let attempt = |cert: Certificate, name: &str| {
            let path = write_key(dir.path(), name, &key, Some(&cert));
            let host = key_file_host(&store, server.port, &path, "");
            let store = store.clone();
            async move { connect_host(&store, &host).await.err() }
        };

        let expired_at = now() - 60;
        let error = attempt(user_cert(&ca, &key, &[USER], 0, expired_at), "expired").await;
        assert_eq!(
            error,
            Some(SshError::Certificate(CertProblem::Expired {
                at: expired_at as i64
            }))
        );
        assert!(error.unwrap().to_string().contains("过期，请重新签发"));

        let error = attempt(user_cert(&ca, &key, &["root"], 0, u64::MAX), "principal").await;
        assert_eq!(
            error,
            Some(SshError::Certificate(CertProblem::Principal {
                username: USER.into(),
                principals: vec!["root".into()],
            }))
        );

        let error = attempt(
            user_cert(&other_ca, &key, &[USER], 0, u64::MAX),
            "untrusted",
        )
        .await;
        assert_eq!(
            error,
            Some(SshError::Certificate(CertProblem::Rejected {
                ca_fingerprint: other_ca
                    .public_key()
                    .fingerprint(HashAlg::Sha256)
                    .to_string()
            }))
        );

        let error = attempt(
            user_cert(&ca, &key, &[USER], now() + HOUR, now() + 2 * HOUR),
            "future",
        )
        .await;
        assert!(
            matches!(
                error,
                Some(SshError::Certificate(CertProblem::NotYetValid { .. }))
            ),
            "{error:?}"
        );
        assert!(server.logins().is_empty());
    });
}

// 证书不被接受而 authorized_keys 中有这把公钥：照常用私钥登录（没有证书的失效时间）
#[test]
fn falls_back_to_the_key_itself() {
    crate::runtime::runtime().block_on(async {
        let (ca, other_ca, key) = (new_key(), new_key(), new_key());
        let server = CertServer::start(&ca, vec![key.public_key().clone()], None).await;
        let dir = tempfile::tempdir().unwrap();
        let cert = user_cert(&other_ca, &key, &[USER], 0, u64::MAX);
        let path = write_key(dir.path(), "id", &key, Some(&cert));
        let store = memory_store();
        let host = key_file_host(&store, server.port, &path, "");
        let connection = connect_host(&store, &host).await.unwrap();
        assert_eq!(server.logins(), ["key"]);
        assert_eq!(connection.certificate_expiry(), None);
    });
}

// 指定的证书文件：优先于旁边的；与私钥对不上、文件不存在时说明（私钥本身照常尝试）
#[test]
fn explicit_certificate_file() {
    crate::runtime::runtime().block_on(async {
        let (ca, key, other) = (new_key(), new_key(), new_key());
        let server = CertServer::start(&ca, Vec::new(), None).await;
        let dir = tempfile::tempdir().unwrap();
        // 旁边的证书已过期，指定的是新签发的
        let stale = user_cert(&ca, &key, &[USER], 0, now() - 1);
        let path = write_key(dir.path(), "id", &key, Some(&stale));
        let fresh = dir.path().join("fresh-cert.pub");
        std::fs::write(
            &fresh,
            user_cert(&ca, &key, &[USER], 0, u64::MAX)
                .to_openssh()
                .unwrap(),
        )
        .unwrap();
        let store = memory_store();
        let host = key_file_host(&store, server.port, &path, &fresh.display().to_string());
        let connection = connect_host(&store, &host).await.unwrap();
        assert_eq!(connection.certificate_expiry(), None, "永不过期");

        let wrong = dir.path().join("wrong-cert.pub");
        std::fs::write(
            &wrong,
            user_cert(&ca, &other, &[USER], 0, u64::MAX)
                .to_openssh()
                .unwrap(),
        )
        .unwrap();
        let host = key_file_host(&store, server.port, &path, &wrong.display().to_string());
        assert_eq!(
            connect_host(&store, &host).await.err(),
            Some(SshError::Certificate(CertProblem::KeyMismatch {
                certificate: "wrong-cert.pub".into()
            }))
        );

        let host = key_file_host(
            &store,
            server.port,
            &path,
            &dir.path().join("missing-cert.pub").display().to_string(),
        );
        assert_eq!(
            connect_host(&store, &host).await.err(),
            Some(SshError::Certificate(CertProblem::Unreadable {
                certificate: "missing-cert.pub".into(),
                reason: sshcert::CertReadIssue::NotFound,
            }))
        );
        assert_eq!(server.logins(), ["cert"]);
    });
}

// 密钥库中的私钥附带证书；RSA 证书按 rsa-sha2 签名
#[test]
fn library_key_with_rsa_certificate() {
    crate::runtime::runtime().block_on(async {
        let (ca, key) = (new_key(), rsa_key());
        let server = CertServer::start(&ca, Vec::new(), None).await;
        let store = memory_store();
        let parsed = sshkey::parse(openssh(&key).as_bytes(), None).unwrap();
        let mut entry = parsed.to_key_entry("rsa");
        entry.certificate = user_cert(&ca, &key, &[USER], 0, u64::MAX)
            .to_openssh()
            .unwrap();
        let id = store
            .create::<KeyEntry>(entry, None, Position::Last)
            .unwrap();
        store
            .set_secret(
                &Owner::key(&id),
                SecretField::PrivateKey,
                Some(&SecretString::new(openssh(&key))),
            )
            .unwrap();
        let host = store
            .create(
                Host {
                    name: "rsa".into(),
                    address: "127.0.0.1".into(),
                    port: server.port,
                    username: USER.into(),
                    auth: HostAuth::Key {
                        source: KeySource::Library { key: id },
                    },
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
        connect_host(&store, &host).await.unwrap();
        assert_eq!(server.logins(), ["cert"]);
    });
}

// 主机证书：信任的 CA 签发的免去指纹确认（也不记录）；没有信任的 CA 时照常询问指纹（证书里的主机密钥）
#[test]
fn host_certificate_from_trusted_ca() {
    crate::runtime::runtime().block_on(async {
        let (user_ca, host_ca_key, key) = (new_key(), new_key(), new_key());
        let server = CertServer::start(
            &user_ca,
            vec![key.public_key().clone()],
            Some((&host_ca_key, &["127.0.0.1"])),
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let path = write_key(dir.path(), "id", &key, None);
        let store = memory_store();
        let host = key_file_host(&store, server.port, &path, "");
        let plan = plan_for_host(&store, &host).unwrap();
        let options = ConnectOptions::default();

        // 没有信任的 CA：不请求证书，询问主机密钥的指纹
        let (prompter, asked) = counting_prompter();
        connect(store.clone(), &plan, prompter, &options)
            .await
            .unwrap();
        let expected = known_hosts::fingerprint(server.host_key.public_key());
        assert_eq!(*asked.lock().unwrap(), [expected]);
        KnownHosts::new(store.database())
            .forget("127.0.0.1", server.port)
            .unwrap();

        // 信任了别的主机名的 CA：不适用，照常询问
        let line = format!(
            "@cert-authority *.example.com {}",
            host_ca_key.public_key().to_openssh().unwrap()
        );
        host_ca::add(&store, vec![host_ca::parse_line(&line).unwrap()]).unwrap();
        let (prompter, asked) = counting_prompter();
        connect(store.clone(), &plan, prompter, &options)
            .await
            .unwrap();
        assert_eq!(asked.lock().unwrap().len(), 1);
        KnownHosts::new(store.database())
            .forget("127.0.0.1", server.port)
            .unwrap();

        // 适用于这台主机：不询问，也不记录指纹
        let line = format!(
            "@cert-authority [127.0.0.1]:{} {}",
            server.port,
            host_ca_key.public_key().to_openssh().unwrap()
        );
        host_ca::add(&store, vec![host_ca::parse_line(&line).unwrap()]).unwrap();
        let (prompter, asked) = counting_prompter();
        connect(store.clone(), &plan, prompter, &options)
            .await
            .unwrap();
        assert!(
            asked.lock().unwrap().is_empty(),
            "{:?}",
            asked.lock().unwrap()
        );
        assert!(KnownHosts::new(store.database()).list().unwrap().is_empty());
    });
}

// 主机证书的主机名不对（证书只给 db 签发）：退回按证书里的主机密钥核对，询问指纹
#[test]
fn host_certificate_for_another_name_falls_back_to_fingerprint() {
    crate::runtime::runtime().block_on(async {
        let (user_ca, host_ca_key, key) = (new_key(), new_key(), new_key());
        let server = CertServer::start(
            &user_ca,
            vec![key.public_key().clone()],
            Some((&host_ca_key, &["db.example.com"])),
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let path = write_key(dir.path(), "id", &key, None);
        let store = memory_store();
        let host = key_file_host(&store, server.port, &path, "");
        let line = format!(
            "@cert-authority * {}",
            host_ca_key.public_key().to_openssh().unwrap()
        );
        host_ca::add(&store, vec![host_ca::parse_line(&line).unwrap()]).unwrap();
        let plan = plan_for_host(&store, &host).unwrap();
        let (prompter, asked) = counting_prompter();
        connect(store.clone(), &plan, prompter, &ConnectOptions::default())
            .await
            .unwrap();
        assert_eq!(
            *asked.lock().unwrap(),
            [known_hosts::fingerprint(server.host_key.public_key())]
        );
        // 记下的是证书里的主机密钥：下次直接认得
        let (prompter, asked) = counting_prompter();
        connect(store.clone(), &plan, prompter, &ConnectOptions::default())
            .await
            .unwrap();
        assert!(asked.lock().unwrap().is_empty());
    });
}

// ---------------------------------------------------------------- 假的 ssh-agent

/// 假的 agent：只列出给定的身份（公钥或证书的编码），用对应的私钥签名。返回连接地址与服务任务（丢弃前先 abort）。
async fn fake_agent(
    identities: Vec<(Vec<u8>, Arc<PrivateKey>)>,
) -> (String, tokio::task::JoinHandle<()>) {
    let identities = Arc::new(identities);
    #[cfg(windows)]
    let served = {
        use std::sync::atomic::{AtomicU64, Ordering};
        use tokio::net::windows::named_pipe::ServerOptions;
        // 同一进程中并行的测试各用各的管道
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            r"\\.\pipe\termlet-test-agent-{}-{}-{}",
            std::process::id(),
            crate::time::now_ms(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)
            .unwrap();
        let pipe = name.clone();
        let task = crate::runtime::spawn(async move {
            loop {
                if server.connect().await.is_err() {
                    return;
                }
                let Ok(next) = ServerOptions::new().create(&pipe) else {
                    return;
                };
                let connected = std::mem::replace(&mut server, next);
                crate::runtime::spawn(serve_agent(connected, identities.clone()));
            }
        });
        (name, task)
    };
    #[cfg(unix)]
    let served = {
        let dir = tempfile::tempdir().unwrap().keep();
        let path = dir.join("agent.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let task = crate::runtime::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                crate::runtime::spawn(serve_agent(stream, identities.clone()));
            }
        });
        (path.display().to_string(), task)
    };
    served
}

/// 公钥在 agent 协议中的编码。
fn key_blob(key: &PrivateKey) -> Vec<u8> {
    use russh::keys::ssh_encoding::Encode as _;
    key.public_key().key_data().encode_vec().unwrap()
}

fn agent_host(store: &Store, port: u16) -> crate::id::HostId {
    store
        .create(
            Host {
                name: "agent".into(),
                address: "127.0.0.1".into(),
                port,
                username: USER.into(),
                auth: HostAuth::Agent,
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap()
}

// agent 中的身份按列出的顺序尝试：先列出的普通密钥不被接受，再用带证书的身份登录
#[test]
fn agent_certificate_identity() {
    crate::runtime::runtime().block_on(async {
        let (ca, key, other) = (new_key(), Arc::new(new_key()), Arc::new(new_key()));
        let server = CertServer::start(&ca, Vec::new(), None).await;
        let before = now() + 2 * HOUR;
        let cert = user_cert(&ca, &key, &[USER], 0, before);
        let (address, agent) = fake_agent(vec![
            (key_blob(&other), other.clone()),
            (cert.to_bytes().unwrap(), key.clone()),
        ])
        .await;
        let store = memory_store();
        let host = agent_host(&store, server.port);
        let plan = plan_for_host(&store, &host).unwrap();
        let options = ConnectOptions {
            agent_path: Some(address),
            ..Default::default()
        };
        let connection = connect(store.clone(), &plan, counting_prompter().0, &options)
            .await
            .unwrap();
        assert_eq!(server.logins(), ["cert"]);
        assert_eq!(connection.certificate_expiry(), Some(before as i64));
        agent.abort();
    });
}

// agent 中只有过期的证书：说明证书过期，而不只是“认证失败”
#[test]
fn agent_expired_certificate_is_explained() {
    crate::runtime::runtime().block_on(async {
        let (ca, key) = (new_key(), Arc::new(new_key()));
        let server = CertServer::start(&ca, Vec::new(), None).await;
        let expired_at = now() - 10;
        let cert = user_cert(&ca, &key, &[USER], 0, expired_at);
        let (address, agent) = fake_agent(vec![(cert.to_bytes().unwrap(), key.clone())]).await;
        let store = memory_store();
        let host = agent_host(&store, server.port);
        let plan = plan_for_host(&store, &host).unwrap();
        let options = ConnectOptions {
            agent_path: Some(address),
            ..Default::default()
        };
        let error = connect(store.clone(), &plan, counting_prompter().0, &options)
            .await
            .err();
        assert_eq!(
            error,
            Some(SshError::Certificate(CertProblem::Expired {
                at: expired_at as i64
            }))
        );
        agent.abort();
    });
}

async fn serve_agent<S>(mut stream: S, identities: Arc<Vec<(Vec<u8>, Arc<PrivateKey>)>>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use russh::keys::ssh_encoding::{Decode as _, Encode as _};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    loop {
        let mut len = [0u8; 4];
        if stream.read_exact(&mut len).await.is_err() {
            return;
        }
        let mut frame = vec![0u8; u32::from_be_bytes(len) as usize];
        if stream.read_exact(&mut frame).await.is_err() {
            return;
        }
        let mut reply = Vec::new();
        match frame.split_first() {
            // REQUEST_IDENTITIES → IDENTITIES_ANSWER
            Some((&11, _)) => {
                reply.push(12);
                (identities.len() as u32).encode(&mut reply).unwrap();
                for (blob, _) in identities.iter() {
                    blob.encode(&mut reply).unwrap();
                    "test".encode(&mut reply).unwrap();
                }
            }
            // SIGN_REQUEST → SIGN_RESPONSE
            Some((&13, mut rest)) => {
                let blob = Vec::<u8>::decode(&mut rest).unwrap();
                let data = Vec::<u8>::decode(&mut rest).unwrap();
                let flags = u32::decode(&mut rest).unwrap();
                match identities.iter().find(|(b, _)| *b == blob) {
                    Some((_, key)) => {
                        let hash = match flags {
                            2 => Some(HashAlg::Sha256),
                            4 => Some(HashAlg::Sha512),
                            _ => None,
                        };
                        reply.push(14);
                        sign(key, hash, &data).encode(&mut reply).unwrap();
                    }
                    None => reply.push(5),
                }
            }
            _ => reply.push(5),
        }
        let mut out = (reply.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(&reply);
        if stream.write_all(&out).await.is_err() {
            return;
        }
    }
}

/// 签名并编码（算法名 + 签名），与 OpenSSH 的 agent 相同。
fn sign(key: &PrivateKey, hash: Option<HashAlg>, data: &[u8]) -> Vec<u8> {
    use russh::keys::signature::Signer as _;
    use russh::keys::ssh_encoding::Encode as _;
    use russh::keys::ssh_key::private::KeypairData;
    let signature: russh::keys::ssh_key::Signature = match key.key_data() {
        KeypairData::Rsa(pair) => (pair, hash).try_sign(data).unwrap(),
        other => other.try_sign(data).unwrap(),
    };
    signature.encode_vec().unwrap()
}

// ---------------------------------------------------------------- 真实的 OpenSSH（testing/sshcert）

/// 证书测试容器的 sshd（`TERMLET_TEST_CERT=127.0.0.1:2227`）；没有设置时为 `None`，测试跳过。
pub(crate) fn cert_server() -> Option<(String, u16)> {
    let value = std::env::var("TERMLET_TEST_CERT").ok()?;
    let (host, port) = value.rsplit_once(':')?;
    Some((host.to_string(), port.parse().ok()?))
}

fn container() -> String {
    std::env::var("TERMLET_TEST_CERT_CONTAINER")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "termlet-cert-sshd".into())
}

/// 在容器里执行命令（`input` 写进标准输入），返回标准输出。
fn docker_exec(args: &[&str], input: &str) -> String {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    let container = container();
    let mut child = Command::new("docker")
        .args(["exec", "-i", container.as_str()])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("需要 docker 命令");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "docker exec {args:?} 失败：{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// 用容器里的 ssh-keygen 现场签发用户证书（`validity` 是 `ssh-keygen -V` 的写法）。
pub(crate) fn sign_in_container(key: &PrivateKey, principals: &str, validity: &str) -> Certificate {
    let public = key.public_key().to_openssh().unwrap();
    let text = docker_exec(&["sign-user", principals, validity], &format!("{public}\n"));
    sshcert::parse(&text).unwrap()
}

// 真实的 OpenSSH：证书登录、过期与用户名不对时的说明、agent 中的证书、不被信任的 CA、信任主机 CA 后免去指纹确认
#[test]
fn real_openssh_certificates() {
    let Some((address, port)) = cert_server() else {
        return;
    };
    crate::runtime::runtime().block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let store = memory_store();
        let host_for = |name: &str, cert: Option<&Certificate>, key: &PrivateKey| {
            let path = write_key(dir.path(), name, key, cert);
            store
                .create(
                    Host {
                        name: name.into(),
                        address: address.clone(),
                        port,
                        username: "termlet".into(),
                        auth: HostAuth::Key {
                            source: KeySource::file(path.display().to_string()),
                        },
                        ..Default::default()
                    },
                    None,
                    Position::Last,
                )
                .unwrap()
        };

        // 1. 私钥旁的 -cert.pub（ssh-keygen 签发，30 分钟后过期）：登录、执行命令，知道快过期
        let key = new_key();
        let cert = sign_in_container(&key, "termlet", "-5m:+30m");
        let host = host_for("good", Some(&cert), &key);
        let connection = connect_host(&store, &host).await.unwrap();
        let whoami = exec(&connection, "whoami", 1024, Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&whoami.stdout).trim(), "termlet");
        let expiry = connection.certificate_expiry().unwrap();
        assert!(sshcert::expires_soon(expiry, sshcert::now_secs()));
        connection.disconnect().await;

        // 2. 过期的证书
        let key = new_key();
        let cert = sign_in_container(&key, "termlet", "-2h:-1h");
        let host = host_for("expired", Some(&cert), &key);
        let error = connect_host(&store, &host).await.err();
        assert!(
            matches!(
                error,
                Some(SshError::Certificate(CertProblem::Expired { .. }))
            ),
            "{error:?}"
        );

        // 3. 用户名不在证书允许的列表中
        let key = new_key();
        let cert = sign_in_container(&key, "root,admin", "-5m:+1h");
        let host = host_for("principal", Some(&cert), &key);
        assert_eq!(
            connect_host(&store, &host).await.err(),
            Some(SshError::Certificate(CertProblem::Principal {
                username: "termlet".into(),
                principals: vec!["root".into(), "admin".into()],
            }))
        );

        // 4. 服务器不信任的 CA（本机签发）
        let (own_ca, key) = (new_key(), new_key());
        let cert = user_cert(&own_ca, &key, &["termlet"], 0, u64::MAX);
        let host = host_for("untrusted", Some(&cert), &key);
        assert!(matches!(
            connect_host(&store, &host).await.err(),
            Some(SshError::Certificate(CertProblem::Rejected { .. }))
        ));

        // 5. agent 中的证书
        let key = Arc::new(new_key());
        let cert = sign_in_container(&key, "termlet", "-5m:+1h");
        let (agent_address, agent) =
            fake_agent(vec![(cert.to_bytes().unwrap(), key.clone())]).await;
        let agent_host = store
            .create(
                Host {
                    name: "agent".into(),
                    address: address.clone(),
                    port,
                    username: "termlet".into(),
                    auth: HostAuth::Agent,
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
        let plan = plan_for_host(&store, &agent_host).unwrap();
        let options = ConnectOptions {
            agent_path: Some(agent_address),
            ..Default::default()
        };
        let connection = connect(store.clone(), &plan, counting_prompter().0, &options)
            .await
            .unwrap();
        assert!(connection.certificate_expiry().is_some());
        connection.disconnect().await;
        agent.abort();

        // 6. 主机证书：信任容器的主机 CA 之后不再询问指纹（容器的主机证书签给 127.0.0.1 与 localhost）
        KnownHosts::new(store.database())
            .forget(&address, port)
            .unwrap();
        let key = new_key();
        let cert = sign_in_container(&key, "termlet", "-5m:+1h");
        let host = host_for("hostcert", Some(&cert), &key);
        let plan = plan_for_host(&store, &host).unwrap();
        let (prompter, asked) = counting_prompter();
        connect(store.clone(), &plan, prompter, &ConnectOptions::default())
            .await
            .unwrap();
        assert_eq!(asked.lock().unwrap().len(), 1, "还没信任 CA：询问指纹");
        KnownHosts::new(store.database())
            .forget(&address, port)
            .unwrap();
        let host_ca_public = docker_exec(&["cat", "/etc/ssh/host_ca.pub"], "");
        let line = format!(
            "@cert-authority [{address}]:{port} {}",
            host_ca_public.trim()
        );
        host_ca::add(&store, vec![host_ca::parse_line(&line).unwrap()]).unwrap();
        let (prompter, asked) = counting_prompter();
        connect(store.clone(), &plan, prompter, &ConnectOptions::default())
            .await
            .unwrap();
        assert!(asked.lock().unwrap().is_empty(), "信任 CA 之后不询问");
    });
}
