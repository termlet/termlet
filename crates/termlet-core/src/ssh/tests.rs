//! 对真实 sshd 的集成测试：设置 `TERMLET_TEST_SSH=127.0.0.1:2222`（testing/sshd 的测试容器，账号 termlet / termlet-e2e）后运行，
//! 否则跳过。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use termlet_io::{GridSize, IoEvent};

use super::*;
use crate::id::HostId;
use crate::model::{Host, Jump, SecretField};
use crate::secret::SecretString;
use crate::store::{Owner, Position, Store};
use crate::testkit::{SSH_PASSWORD, SSH_USER, memory_store, test_server};

fn add_host(store: &Store, address: &str, port: u16, password: Option<&str>) -> HostId {
    let id = store
        .create(
            Host {
                name: format!("{address}:{port}"),
                address: address.into(),
                port,
                username: SSH_USER.into(),
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    if let Some(password) = password {
        store
            .set_secret(
                &Owner::host(&id),
                SecretField::Password,
                Some(&SecretString::new(password)),
            )
            .unwrap();
    }
    id
}

/// 记下收到的询问；接受指纹，按顺序回答密码。
fn recording_prompter(passwords: Vec<&'static str>) -> (Prompter, Arc<Mutex<Vec<String>>>) {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let passwords = Arc::new(Mutex::new(passwords));
    let log = asked.clone();
    let prompter: Prompter = Arc::new(move |prompt| {
        let log = log.clone();
        let passwords = passwords.clone();
        Box::pin(async move {
            match prompt {
                Prompt::HostKey(p) => {
                    log.lock().unwrap().push(format!("hostkey {}", p.algorithm));
                    Answer::AcceptHostKey
                }
                Prompt::Password { retry, .. } => {
                    log.lock().unwrap().push(format!("password retry={retry}"));
                    match passwords.lock().unwrap().pop() {
                        Some(p) => Answer::Secret(SecretString::new(p)),
                        None => Answer::Cancel,
                    }
                }
                Prompt::Passphrase { retry, .. } => {
                    log.lock()
                        .unwrap()
                        .push(format!("passphrase retry={retry}"));
                    match passwords.lock().unwrap().pop() {
                        Some(p) => Answer::Secret(SecretString::new(p)),
                        None => Answer::Cancel,
                    }
                }
                other => {
                    log.lock().unwrap().push(format!("{other:?}"));
                    Answer::Cancel
                }
            }
        })
    });
    (prompter, asked)
}

async fn run_command(connection: Arc<Connection>, command: &str, expect: &str) -> String {
    let io = open_shell(
        connection,
        ShellOptions {
            size: GridSize::new(120, 30),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    io.input.write(format!("{command}\n").as_bytes());
    let mut text = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !text.contains(expect) {
        match tokio::time::timeout_at(deadline, io.output.recv()).await {
            Ok(Ok(IoEvent::Output(bytes))) => text.push_str(&String::from_utf8_lossy(&bytes)),
            Ok(Ok(IoEvent::Closed(reason))) => panic!("会话提前结束：{reason:?}；输出：{text}"),
            _ => panic!("等待输出超时；输出：{text}"),
        }
    }
    io.input.write(b"exit\n");
    text
}

#[test]
fn password_login_runs_a_command() {
    let Some((host, port)) = test_server() else {
        return;
    };
    crate::runtime::runtime().block_on(async {
        let store = memory_store();
        let id = add_host(&store, &host, port, Some(SSH_PASSWORD));
        let plan = plan_for_host(&store, &id).unwrap();
        let (prompter, asked) = recording_prompter(vec![]);
        let connection = connect(store.clone(), &plan, prompter, &ConnectOptions::default())
            .await
            .unwrap();
        let output = run_command(connection, "echo termlet-$((40+2))", "termlet-42").await;
        assert!(output.contains("termlet-42"));
        let asked = asked.lock().unwrap().clone();
        assert_eq!(asked.len(), 1, "首次连接只问一次指纹：{asked:?}");
        assert!(asked[0].starts_with("hostkey"));

        // 指纹已记住：第二次连接不再询问
        let (prompter, asked) = recording_prompter(vec![]);
        connect(store.clone(), &plan, prompter, &ConnectOptions::default())
            .await
            .unwrap();
        assert!(asked.lock().unwrap().is_empty());
    });
}

#[test]
fn wrong_password_asks_again() {
    let Some((host, port)) = test_server() else {
        return;
    };
    crate::runtime::runtime().block_on(async {
        let store = memory_store();
        let id = add_host(&store, &host, port, Some("not-the-password"));
        let plan = plan_for_host(&store, &id).unwrap();
        let (prompter, asked) = recording_prompter(vec![SSH_PASSWORD]);
        connect(store.clone(), &plan, prompter, &ConnectOptions::default())
            .await
            .unwrap();
        let asked = asked.lock().unwrap().clone();
        // 保存的密码不对：询问时说明"上次的不对"
        assert!(
            asked.contains(&"password retry=true".to_string()),
            "{asked:?}"
        );

        // 用户取消询问时报"已取消"
        let (prompter, _) = recording_prompter(vec![]);
        let error = connect(store.clone(), &plan, prompter, &ConnectOptions::default())
            .await
            .err()
            .unwrap();
        assert_eq!(error, SshError::AuthCancelled);
    });
}

#[test]
fn changed_host_key_is_refused() {
    let Some((host, port)) = test_server() else {
        return;
    };
    crate::runtime::runtime().block_on(async {
        let store = memory_store();
        let id = add_host(&store, &host, port, Some(SSH_PASSWORD));
        let plan = plan_for_host(&store, &id).unwrap();
        let (prompter, _) = recording_prompter(vec![]);
        connect(store.clone(), &plan, prompter, &ConnectOptions::default())
            .await
            .unwrap();
        // 把记录换成另一把同类型的密钥
        let known = KnownHosts::new(store.database());
        let algorithms = known.algorithms(&host, port).unwrap();
        assert_eq!(algorithms.len(), 1);
        let fake = match algorithms[0].as_str() {
            "ssh-ed25519" => {
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl"
            }
            other => panic!("测试只准备了 ed25519 的假密钥，服务器用的是 {other}"),
        };
        known.forget_key(&host, port, &algorithms[0]).unwrap();
        known
            .remember(
                &host,
                port,
                &russh::keys::PublicKey::from_openssh(fake).unwrap(),
            )
            .unwrap();
        let (prompter, _) = recording_prompter(vec![]);
        let error = connect(store.clone(), &plan, prompter, &ConnectOptions::default())
            .await
            .err()
            .unwrap();
        assert!(
            matches!(error, SshError::HostKeyChanged { .. }),
            "{error:?}"
        );
    });
}

#[test]
fn connects_through_a_jump_host() {
    let Some((host, port)) = test_server() else {
        return;
    };
    crate::runtime::runtime().block_on(async {
        let store = memory_store();
        let bastion = add_host(&store, &host, port, Some(SSH_PASSWORD));
        // 目标是容器里的 sshd 自己（从跳板机看是 127.0.0.1:22）
        let target = add_host(&store, "127.0.0.1", 22, Some(SSH_PASSWORD));
        let mut doc = store.get::<Host>(&target).unwrap().doc;
        doc.jump = Jump::Via { host: bastion };
        store.update(&target, doc).unwrap();
        let plan = plan_for_host(&store, &target).unwrap();
        assert_eq!(plan.hops.len(), 2);
        let (prompter, asked) = recording_prompter(vec![]);
        let connection = connect(store.clone(), &plan, prompter, &ConnectOptions::default())
            .await
            .unwrap();
        let output = run_command(connection, "echo jump-$((1+1))", "jump-2").await;
        assert!(output.contains("jump-2"));
        // 两跳各确认一次指纹
        assert_eq!(asked.lock().unwrap().len(), 2);
    });
}

#[test]
fn quick_connect_falls_back_to_password() {
    let Some((host, port)) = test_server() else {
        return;
    };
    crate::runtime::runtime().block_on(async {
        let store = memory_store();
        let target = crate::quick::QuickTarget {
            username: SSH_USER.into(),
            host,
            port,
        };
        let plan = plan_for_quick(&target);
        // 临时连接兼容旧算法，仍能连上只用安全算法的服务器
        assert!(plan.target().legacy_algorithms);
        let (prompter, asked) = recording_prompter(vec![SSH_PASSWORD]);
        connect(store.clone(), &plan, prompter, &ConnectOptions::default())
            .await
            .unwrap();
        assert!(
            asked
                .lock()
                .unwrap()
                .iter()
                .any(|a| a.starts_with("password"))
        );
    });
}

/// 读终端输出直到出现 `expect`。
async fn read_until(io: &termlet_io::TerminalIo, expect: &str) -> String {
    let mut text = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !text.contains(expect) {
        match tokio::time::timeout_at(deadline, io.output.recv()).await {
            Ok(Ok(IoEvent::Output(bytes))) => text.push_str(&String::from_utf8_lossy(&bytes)),
            Ok(Ok(IoEvent::Closed(reason))) => panic!("会话提前结束：{reason:?}；输出：{text}"),
            _ => panic!("等待 {expect:?} 超时；输出：{text}"),
        }
    }
    text
}

#[test]
fn terminals_in_a_group_share_one_connection() {
    let Some((host, port)) = test_server() else {
        return;
    };
    crate::runtime::runtime().block_on(async {
        let store = memory_store();
        let id = add_host(&store, &host, port, Some(SSH_PASSWORD));
        let spec = TerminalSpec::for_host(&store, &id).unwrap();
        let session = TerminalSession::new();
        let size = GridSize::new(100, 30);
        let (prompter, asked) = recording_prompter(vec![]);
        let first = open_terminal(store.clone(), &spec, &session, size, prompter.clone())
            .await
            .unwrap();
        assert_eq!(first.tmux, TmuxStatus::Off);
        assert!(
            first.connection.server_version().starts_with("SSH-2.0-"),
            "{}",
            first.connection.server_version()
        );
        let asked_before = asked.lock().unwrap().len();
        // 复制会话：同一条连接，不再询问
        let copy = open_terminal(
            store.clone(),
            &spec,
            &session.duplicate(),
            size,
            prompter.clone(),
        )
        .await
        .unwrap();
        assert!(Arc::ptr_eq(&first.connection, &copy.connection));
        assert_eq!(asked.lock().unwrap().len(), asked_before);
        // 两个终端各自独立
        copy.io.input.write(b"echo copy-$((2*3))\n");
        read_until(&copy.io, "copy-6").await;
        // 强制重连换了连接组：新的连接
        let forced = open_terminal(
            store.clone(),
            &spec,
            &session.with_new_group(),
            size,
            prompter,
        )
        .await
        .unwrap();
        assert!(!Arc::ptr_eq(&first.connection, &forced.connection));
    });
}

#[test]
fn closing_the_terminal_ends_the_channel() {
    let Some((host, port)) = test_server() else {
        return;
    };
    crate::runtime::runtime().block_on(async {
        let store = memory_store();
        let id = add_host(&store, &host, port, Some(SSH_PASSWORD));
        let spec = TerminalSpec::for_host(&store, &id).unwrap();
        let (prompter, _) = recording_prompter(vec![]);
        let opened = open_terminal(
            store.clone(),
            &spec,
            &TerminalSession::new(),
            GridSize::new(80, 24),
            prompter,
        )
        .await
        .unwrap();
        opened.io.input.write(b"echo ready\n");
        read_until(&opened.io, "ready").await;
        opened.io.input.close();
        let closed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match opened.io.output.recv().await {
                    Ok(IoEvent::Closed(reason)) => return reason,
                    Ok(IoEvent::Output(_)) => {}
                    Err(_) => panic!("输出通道关闭前没有收到结束"),
                }
            }
        })
        .await
        .expect("关闭后应很快结束");
        tracing::info!(?closed, "关闭终端后的结束原因");
        // 连接本身还在（可以给别的终端用）
        assert!(!opened.connection.is_closed());
    });
}

#[test]
fn tmux_session_survives_reconnect_and_is_killed_on_close() {
    let Some((host, port)) = test_server() else {
        return;
    };
    crate::runtime::runtime().block_on(async {
        let store = memory_store();
        let id = add_host(&store, &host, port, Some(SSH_PASSWORD));
        let mut doc = store.get::<Host>(&id).unwrap().doc;
        doc.terminal.tmux = true;
        store.update(&id, doc).unwrap();
        let spec = TerminalSpec::for_host(&store, &id).unwrap();
        let session = TerminalSession::new();
        let size = GridSize::new(100, 30);
        let (prompter, _) = recording_prompter(vec![]);
        let opened = open_terminal(store.clone(), &spec, &session, size, prompter.clone())
            .await
            .unwrap();
        assert_eq!(opened.tmux, TmuxStatus::Attached);
        opened
            .io
            .input
            .write(b"export MARK=kept-$((5*5)); tmux display-message -p 'in-#S'\n");
        read_until(&opened.io, &format!("in-{}", session.key)).await;

        // 断线（这里直接断开连接）：tmux 会话保留，重连回到原来的会话，之前设置的变量还在
        opened.connection.disconnect().await;
        drop(opened);
        let session = session.with_new_group();
        let again = open_terminal(store.clone(), &spec, &session, size, prompter)
            .await
            .unwrap();
        assert_eq!(again.tmux, TmuxStatus::Attached);
        again.io.input.write(b"echo got-$MARK\n");
        read_until(&again.io, "got-kept-25").await;

        // 关闭 Tab：结束 tmux 会话
        let connection = again.connection.clone();
        again.io.input.close();
        kill_tmux(store.clone(), &spec, &session.key, Some(connection.clone()))
            .await
            .unwrap();
        let check = exec(
            &connection,
            &format!("tmux has-session -t {}", session.key),
            1024,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_ne!(check.exit_code, Some(0), "tmux 会话应已结束");
    });
}

/// 一次性命令超时：结束远端命令并关闭通道，不在服务器上留下进程与通道。
#[test]
fn timed_out_command_is_ended_on_the_server() {
    let Some((host, port)) = test_server() else {
        return;
    };
    crate::runtime::runtime().block_on(async {
        let store = memory_store();
        let id = add_host(&store, &host, port, Some(SSH_PASSWORD));
        let plan = plan_for_host(&store, &id).unwrap();
        let (prompter, _) = recording_prompter(vec![]);
        let connection = connect(store.clone(), &plan, prompter, &ConnectOptions::default())
            .await
            .unwrap();
        let error = exec(&connection, "sleep 4242", 1024, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(matches!(error, SshError::Channel(_)), "{error:?}");
        // 远端的 sleep 已结束（pgrep 的模式拆开写，不匹配到它自己）
        let mut left = String::new();
        for _ in 0..20 {
            let check = exec(
                &connection,
                "pgrep -f 'sleep 424[2]' || echo none",
                1024,
                Duration::from_secs(10),
            )
            .await
            .unwrap();
            left = String::from_utf8_lossy(&check.stdout).trim().to_string();
            if left == "none" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(left, "none", "超时的命令还在运行");
    });
}

#[test]
fn test_connection_reports_latency_and_version() {
    let Some((host, port)) = test_server() else {
        return;
    };
    crate::runtime::runtime().block_on(async {
        let store = memory_store();
        let id = add_host(&store, &host, port, Some(SSH_PASSWORD));
        let plan = plan_for_host(&store, &id).unwrap();
        let (prompter, _) = recording_prompter(vec![]);
        let report = test_connection(store.clone(), &plan, prompter, &ConnectOptions::default())
            .await
            .unwrap();
        assert!(report.server_version.starts_with("SSH-2.0-"), "{report:?}");
        assert!(report.elapsed > Duration::ZERO);
        // 认证失败如实报告
        let id = add_host(&store, &host, port, Some("wrong"));
        let plan = plan_for_host(&store, &id).unwrap();
        let error = test_connection(
            store.clone(),
            &plan,
            no_prompt(),
            &ConnectOptions::default(),
        )
        .await
        .err()
        .unwrap();
        assert!(
            matches!(error, SshError::AuthCancelled | SshError::AuthFailed { .. }),
            "{error:?}"
        );
    });
}

/// 假的 ssh-agent（Windows 命名管道）：只认"列出密钥"，回答没有密钥；其他请求一律失败。
#[cfg(windows)]
fn spawn_fake_agent() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::ServerOptions;

    let name = format!(
        r"\\.\pipe\termlet-test-agent-{}",
        crate::id::random_token(5)
    );
    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&name)
        .unwrap();
    let pipe = name.clone();
    crate::runtime::spawn(async move {
        loop {
            if server.connect().await.is_err() {
                return;
            }
            let mut client = server;
            server = ServerOptions::new().create(&pipe).unwrap();
            tokio::spawn(async move {
                loop {
                    let mut len = [0u8; 4];
                    if client.read_exact(&mut len).await.is_err() {
                        return;
                    }
                    let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
                    if client.read_exact(&mut body).await.is_err() {
                        return;
                    }
                    // 11 = SSH_AGENTC_REQUEST_IDENTITIES → 12 = SSH_AGENT_IDENTITIES_ANSWER（0 个）；5 = SSH_AGENT_FAILURE
                    let reply: &[u8] = if body.first() == Some(&11) {
                        &[0, 0, 0, 5, 12, 0, 0, 0, 0]
                    } else {
                        &[0, 0, 0, 1, 5]
                    };
                    if client.write_all(reply).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    name
}

#[cfg(windows)]
#[test]
fn agent_is_forwarded_only_when_enabled() {
    let Some((host, port)) = test_server() else {
        return;
    };
    crate::runtime::runtime().block_on(async {
        let store = memory_store();
        let agent = spawn_fake_agent();
        let id = add_host(&store, &host, port, Some(SSH_PASSWORD));
        let mut spec = TerminalSpec::for_host(&store, &id).unwrap();
        spec.connect.agent_path = Some(agent);
        let size = GridSize::new(120, 30);
        let check = b"ssh-add -l; echo agent-rc=$?\n";

        let (prompter, _) = recording_prompter(vec![]);
        let plain = open_terminal(
            store.clone(),
            &spec,
            &TerminalSession::new(),
            size,
            prompter.clone(),
        )
        .await
        .unwrap();
        plain.io.input.write(check);
        // 2：连不上 agent（没有转发）
        read_until(&plain.io, "agent-rc=2").await;

        spec.agent_forward = true;
        let forwarded = open_terminal(
            store.clone(),
            &spec,
            &TerminalSession::new(),
            size,
            prompter,
        )
        .await
        .unwrap();
        forwarded.io.input.write(check);
        // 1：连上了转发的 agent，里面没有密钥
        let text = read_until(&forwarded.io, "agent-rc=1").await;
        assert!(text.contains("no identities"), "{text}");
    });
}

#[test]
fn encrypted_key_login_asks_for_passphrase() {
    let Some((host, port)) = test_server() else {
        return;
    };
    crate::runtime::runtime().block_on(async {
        use crate::model::{Auth, KeySource};
        use crate::sshkey;

        let store = memory_store();
        // 用密码连接部署一把新生成的公钥
        let key = sshkey::generate(sshkey::KeyKind::Ed25519, "termlet-test").unwrap();
        let with_password = add_host(&store, &host, port, Some(SSH_PASSWORD));
        let plan = plan_for_host(&store, &with_password).unwrap();
        let (prompter, _) = recording_prompter(vec![]);
        let connection = connect(store.clone(), &plan, prompter, &ConnectOptions::default())
            .await
            .unwrap();
        let script = sshkey::deploy_script(&key.public_key()).unwrap();
        let deployed = exec(&connection, &script, 4096, Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(
            deployed.exit_code,
            Some(0),
            "{}",
            String::from_utf8_lossy(&deployed.stderr)
        );

        // 粘贴的私钥加了密码、没有保存私钥密码：连接时询问，先答错一次
        let id = add_host(&store, &host, port, None);
        let mut doc = store.get::<Host>(&id).unwrap().doc;
        doc.auth = Auth::Key {
            source: KeySource::pasted(),
        };
        store.update(&id, doc).unwrap();
        let encrypted = key
            .export_openssh(Some(&SecretString::new("pp-123")))
            .unwrap();
        store
            .set_secret(&Owner::host(&id), SecretField::PrivateKey, Some(&encrypted))
            .unwrap();
        let plan = plan_for_host(&store, &id).unwrap();
        let (prompter, asked) = recording_prompter(vec!["pp-123", "wrong"]);
        let connection = connect(store.clone(), &plan, prompter, &ConnectOptions::default())
            .await
            .unwrap();
        let output = run_command(connection, "echo key-$((3+4))", "key-7").await;
        assert!(output.contains("key-7"));
        let asked = asked.lock().unwrap().clone();
        assert_eq!(
            asked,
            ["passphrase retry=false", "passphrase retry=true"],
            "{asked:?}"
        );
    });
}

#[test]
fn encoding_switches_live_and_ping_measures_latency() {
    let Some((host, port)) = test_server() else {
        return;
    };
    crate::runtime::runtime().block_on(async {
        let store = memory_store();
        let id = add_host(&store, &host, port, Some(SSH_PASSWORD));
        let spec = TerminalSpec::for_host(&store, &id).unwrap();
        let (prompter, _) = recording_prompter(vec![]);
        let opened = open_terminal(
            store.clone(),
            &spec,
            &TerminalSession::new(),
            GridSize::new(100, 30),
            prompter,
        )
        .await
        .unwrap();
        assert!(opened.connection.ping().await.is_some(), "能测到往返时间");
        // 切到 GBK 后，远端输出的 GBK 字节（“你好”）解码成 UTF-8
        opened.io.input.set_encoding("gb18030");
        opened
            .io
            .input
            .write(b"printf '\\304\\343\\272\\303-done\\n'\n");
        read_until(&opened.io, "你好-done").await;
    });
}
