//! 以 root 身份打开 SFTP（sudo）对真实 sshd 的测试：设置 `TERMLET_TEST_SSH=127.0.0.1:2222` 后运行，否则跳过。
//! 测试容器中的 sudo 用户见 testing/sshd/Dockerfile：sudo-nopw 只能免密执行 sftp-server，sudo-pw 要密码，
//! sudo-srv 要密码且只能执行 sftp-server，sudo-deny 不在 sudoers 中；termlet 看不到 sudo。
//! 容器是旧的、还没有这些用户时跳过（登录失败）。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;
use crate::id::HostId;
use crate::secret::SecretString;
use crate::sftp::{
    ManagerOptions, SessionState, SftpError, SshTarget, SudoAsker, SudoError, SudoPrompt,
};
use crate::ssh::{ConnectionPool, SshError, TerminalSpec, connect};
use crate::store::Store;
use crate::testkit::{SSH_PASSWORD, accept_host_key, server_store};

/// 依次给出 `answers` 的询问者（`None` 为取消，用完后取消），记下每次的询问。
fn asker(answers: &[Option<&str>]) -> (SudoAsker, Arc<Mutex<Vec<SudoPrompt>>>) {
    let answers: Arc<Mutex<Vec<Option<SecretString>>>> = Arc::new(Mutex::new(
        answers
            .iter()
            .rev()
            .map(|a| a.map(SecretString::new))
            .collect(),
    ));
    let asked = Arc::new(Mutex::new(Vec::new()));
    let log = asked.clone();
    let asker: SudoAsker = Arc::new(move |prompt| {
        log.lock().unwrap().push(prompt);
        let answer = answers.lock().unwrap().pop().flatten();
        Box::pin(async move { answer })
    });
    (asker, asked)
}

struct Server {
    store: Arc<Store>,
    host: HostId,
}

/// 以 `user` 登录的主机（密码为测试容器的统一密码）。
fn setup(user: &str) -> Option<Server> {
    let (store, host) = server_store("sudo", user)?;
    Some(Server { store, host })
}

impl Server {
    fn target(&self, sudo: Option<SudoAsker>) -> SshTarget {
        SshTarget {
            store: self.store.clone(),
            host: format!("{}#root", self.host),
            spec: TerminalSpec::for_host(&self.store, &self.host).unwrap(),
            share_group: None,
            prompter: accept_host_key(),
            owner: "w1".into(),
            sudo,
            name_encoding: crate::model::Encoding::Utf8,
        }
    }
}

/// 打开；容器中还没有这个用户（旧的容器）时返回 `None`，测试跳过。
async fn open(
    m: &SftpManager,
    server: &Server,
    sudo: SudoAsker,
) -> Option<Result<Arc<SftpSession>, SftpError>> {
    match m.open_ssh(server.target(Some(sudo))).await {
        Err(SftpError::Connect(SshError::AuthFailed { .. } | SshError::AuthCancelled)) => {
            eprintln!("测试容器中没有 sudo 的测试用户（重新构建 testing/sshd 的镜像），跳过");
            None
        }
        other => Some(other),
    }
}

/// 以 root 身份能做普通用户做不了的事：列出 /root，在 /root 中建的目录属于 root。
async fn assert_root(s: &SftpSession) {
    s.list("/root").await.expect("root 才能列出 /root");
    let dir = format!("/root/termlet-sudo-{}", crate::id::random_token(6));
    s.mkdir(&dir).await.unwrap();
    let attrs = s.client().stat(&dir).await.unwrap();
    assert_eq!(attrs.uid, Some(0), "建的目录属于 root");
    s.remove(&dir).await.unwrap();
}

// 免密：sudoers 只允许免密执行 sftp-server；不询问密码
#[test]
fn sudo_without_password() {
    let Some(server) = setup("sudo-nopw") else {
        return;
    };
    block_on(async {
        let m = manager();
        let (sudo, asked) = asker(&[]);
        let Some(s) = open(&m, &server, sudo).await else {
            return;
        };
        let s = s.unwrap();
        assert_root(&s).await;
        assert!(asked.lock().unwrap().is_empty(), "免密时不询问");
        m.close_all().await;
    });
}

// 要密码：先给错的，再次询问（retry）时给对的；连接断开（SFTP 退出）后自动重连用记住的密码，不再询问
#[test]
fn sudo_with_password_and_reconnect() {
    let Some(server) = setup("sudo-pw") else {
        return;
    };
    block_on(async {
        let m = SftpManager::new(ManagerOptions {
            reconnect_base: Duration::from_millis(50),
            ..options()
        });
        let (sudo, asked) = asker(&[Some("wrong"), Some(SSH_PASSWORD)]);
        let Some(s) = open(&m, &server, sudo).await else {
            return;
        };
        let s = s.unwrap();
        assert_root(&s).await;
        let retries: Vec<bool> = asked.lock().unwrap().iter().map(|p| p.retry).collect();
        assert_eq!(retries, [false, true]);
        let prompt = asked.lock().unwrap()[0].clone();
        assert_eq!(prompt.username, "sudo-pw");

        s.client().close();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if s.state() == SessionState::Connected && s.list("/root").await.is_ok() {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "应自动重连");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(asked.lock().unwrap().len(), 2, "重连时用记住的密码");
        m.close_all().await;
    });
}

// 密码一直不对：问三次后报密码不对；取消时报已取消
#[test]
fn sudo_wrong_password_and_cancel() {
    let Some(server) = setup("sudo-pw") else {
        return;
    };
    block_on(async {
        let m = manager();
        let (sudo, asked) = asker(&[Some("a"), Some("b"), Some("c")]);
        let Some(result) = open(&m, &server, sudo).await else {
            return;
        };
        assert_eq!(
            result.err(),
            Some(SftpError::Sudo(SudoError::WrongPassword))
        );
        assert_eq!(asked.lock().unwrap().len(), 3);

        let (sudo, _) = asker(&[None]);
        let result = open(&m, &server, sudo).await.unwrap();
        assert_eq!(result.err(), Some(SftpError::Canceled));
        assert!(m.sessions(None).is_empty());
    });
}

// sudoers 只允许以 root 执行 sftp-server（要密码）：不能经 sh，改为直接执行它
#[test]
fn sudo_only_sftp_server() {
    let Some(server) = setup("sudo-srv") else {
        return;
    };
    block_on(async {
        let m = manager();
        let (sudo, asked) = asker(&[Some(SSH_PASSWORD)]);
        let Some(s) = open(&m, &server, sudo).await else {
            return;
        };
        let s = s.unwrap();
        assert_root(&s).await;
        assert_eq!(asked.lock().unwrap().len(), 1, "密码只问一次");
        m.close_all().await;
    });
}

// 不在 sudoers 中：密码对了也不行；看不到 sudo 的用户：没有 sudo
#[test]
fn sudo_not_allowed_or_missing() {
    let Some(server) = setup("sudo-deny") else {
        return;
    };
    block_on(async {
        let m = manager();
        let (sudo, _) = asker(&[Some(SSH_PASSWORD)]);
        let Some(result) = open(&m, &server, sudo).await else {
            return;
        };
        assert_eq!(result.err(), Some(SftpError::Sudo(SudoError::NotAllowed)));
    });
    let server = setup("termlet").unwrap();
    block_on(async {
        let m = manager();
        let (sudo, asked) = asker(&[Some(SSH_PASSWORD)]);
        let result = open(&m, &server, sudo).await.unwrap();
        assert_eq!(result.err(), Some(SftpError::Sudo(SudoError::NotInstalled)));
        assert!(asked.lock().unwrap().is_empty());
    });
}

// 堡垒机（只用一个通道）不支持
#[test]
fn sudo_single_channel_rejected() {
    let Some(server) = setup("sudo-nopw") else {
        return;
    };
    block_on(async {
        let m = manager();
        let (sudo, _) = asker(&[]);
        let mut target = server.target(Some(sudo));
        target.spec.single_channel = true;
        assert_eq!(
            m.open_ssh(target).await.err(),
            Some(SftpError::Sudo(SudoError::SingleChannel))
        );
    });
}

// 从终端打开：共用终端的连接（在它上面另开 exec 通道）
#[test]
fn sudo_shares_terminal_connection() {
    let Some(server) = setup("sudo-nopw") else {
        return;
    };
    block_on(async {
        let m = manager();
        let spec = TerminalSpec::for_host(&server.store, &server.host).unwrap();
        let group = ConnectionPool::new_group();
        let terminal = ConnectionPool::global()
            .acquire(&group, || {
                connect(
                    server.store.clone(),
                    &spec.plan,
                    accept_host_key(),
                    &spec.connect,
                )
            })
            .await;
        let Ok(terminal) = terminal else {
            eprintln!("测试容器中没有 sudo 的测试用户（重新构建 testing/sshd 的镜像），跳过");
            return;
        };
        let (sudo, _) = asker(&[]);
        let mut target = server.target(Some(sudo));
        target.share_group = Some(group.clone());
        let s = m.open_ssh(target).await.unwrap();
        assert!(
            Arc::ptr_eq(&terminal, &s.connection().unwrap()),
            "应共用终端的连接"
        );
        assert_root(&s).await;
        m.close_all().await;
    });
}
