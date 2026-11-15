//! 代理命令与 X11 转发的连接测试。
//!
//! - 代理命令连接真实的 sshd：设置 `TERMLET_TEST_PROXY_COMMAND`（如 `docker exec -i termlet-sshd nc 127.0.0.1 22`，
//!   经测试容器中 BusyBox 的 nc 连到容器里的 sshd），账号同 testing/sshd；没有设置时跳过。
//! - X11 转发：设置 `TERMLET_TEST_X11=127.0.0.1:2224`（testing/x11 的测试容器：sshd 开启 X11Forwarding，带 xauth 与
//!   xdpyinfo，账号同上）；测试里起一个假的本机 X 服务器，检查收到的是换过的 cookie。没有设置时跳过。
//! - 代理命令自己退出时的说明不需要服务器，总是运行。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::id::HostId;
use crate::model::{Host, Proxy, ProxyKind, SecretField};
use crate::secret::SecretString;
use crate::ssh::plan::{AuthPlan, ProxyPlan};
use crate::ssh::shell::open_shell_reporting;
use crate::store::{Owner, Position, Store};
use crate::testkit::{SSH_PASSWORD, SSH_USER, memory_store};

fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// 接受指纹，按 `allow` 回答代理命令的确认，其余取消；记下问了什么。
fn answering(allow: bool) -> (Prompter, Arc<Mutex<Vec<String>>>) {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let log = asked.clone();
    let prompter: Prompter = Arc::new(move |prompt| {
        let log = log.clone();
        Box::pin(async move {
            match prompt {
                Prompt::HostKey(_) => Answer::AcceptHostKey,
                Prompt::ProxyCommand(p) => {
                    log.lock().unwrap().push(p.command);
                    if allow {
                        Answer::AllowProxyCommand
                    } else {
                        Answer::Cancel
                    }
                }
                _ => Answer::Cancel,
            }
        })
    });
    (prompter, asked)
}

fn add_proxied_host(store: &Store, command: &str) -> HostId {
    let id = store
        .create(
            Host {
                name: "via-proxy-command".into(),
                // 命令里自己写了目标：地址只用来记录指纹
                address: "proxied.test".into(),
                port: 22,
                username: SSH_USER.into(),
                proxy: Some(Proxy {
                    kind: ProxyKind::Command,
                    command: command.into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    store
        .set_secret(
            &Owner::host(&id),
            SecretField::Password,
            Some(&SecretString::new(SSH_PASSWORD)),
        )
        .unwrap();
    id
}

// 经代理命令连接：第一次询问（显示完整的命令），允许后连上并记住；之后不再询问；不能询问时不执行
#[test]
fn proxy_command_connects_after_confirmation() {
    let Some(command) = env_value("TERMLET_TEST_PROXY_COMMAND") else {
        return;
    };
    crate::runtime::runtime().block_on(async {
        let store = memory_store();
        let id = add_proxied_host(&store, &command);
        let plan = plan_for_host(&store, &id).unwrap();
        let options = ConnectOptions::default();

        let (prompter, asked) = answering(false);
        let error = connect(store.clone(), &plan, prompter, &options)
            .await
            .err();
        assert_eq!(error, Some(SshError::ProxyCommandUnconfirmed));
        assert_eq!(*asked.lock().unwrap(), std::slice::from_ref(&command));

        let error = connect(store.clone(), &plan, no_prompt(), &options)
            .await
            .err();
        assert_eq!(
            error,
            Some(SshError::ProxyCommandUnconfirmed),
            "不能询问时不执行"
        );

        let (prompter, asked) = answering(true);
        let connection = connect(store.clone(), &plan, prompter, &options)
            .await
            .unwrap();
        assert_eq!(asked.lock().unwrap().len(), 1);
        let result = exec(
            &connection,
            "echo via-$((6*7))",
            1024,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "via-42");
        connection.disconnect().await;

        // 确认过的不再询问（no_prompt 也能连上）
        let connection = connect(store.clone(), &plan, no_prompt(), &options)
            .await
            .unwrap();
        connection.disconnect().await;
    });
}

/// 在标准错误写一行后以退出码 7 退出的命令。
fn exiting_command() -> &'static str {
    if cfg!(windows) {
        "cmd /c \"echo no route to %h 1>&2 & exit 7\""
    } else {
        // Unix 上整条命令前加了 exec（与 OpenSSH 相同），复合命令要再包一层 sh -c
        "sh -c 'echo no route to %h >&2; exit 7'"
    }
}

// 代理命令在握手前退出：错误说明退出码与标准错误（%h 已替换）
#[test]
fn exited_proxy_command_is_explained() {
    crate::runtime::runtime().block_on(async {
        let store = memory_store();
        let plan = ConnectPlan {
            hops: vec![Hop {
                label: "web".into(),
                host_id: None,
                address: "10.9.8.7".into(),
                port: 22,
                username: "ops".into(),
                auth: AuthPlan::Agent,
                totp: None,
                proxy: Some(ProxyPlan {
                    kind: ProxyKind::Command,
                    address: String::new(),
                    port: 0,
                    username: String::new(),
                    password: None,
                    command: exiting_command().into(),
                    alias: "web".into(),
                    from_group: None,
                }),
                legacy_algorithms: false,
            }],
        };
        let (prompter, _) = answering(true);
        let options = ConnectOptions {
            keepalive: None,
            agent_path: None,
        };
        let error = connect(store, &plan, prompter, &options).await.err();
        assert_eq!(
            error,
            Some(SshError::ProxyCommandExited {
                code: Some(7),
                stderr: "no route to 10.9.8.7".into()
            })
        );
    });
}

/// 假的本机 X 服务器：记下每个连接请求中的认证（协议名、数据），回答"拒绝"（X 程序随之退出）。
async fn fake_x_server(
    listener: tokio::net::TcpListener,
    seen: Arc<Mutex<Vec<(String, Vec<u8>)>>>,
) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let seen = seen.clone();
        tokio::spawn(async move {
            let mut head = [0u8; 12];
            if socket.read_exact(&mut head).await.is_err() {
                return;
            }
            let big = head[0] == b'B';
            let read_u16 = |at: usize| {
                let bytes = [head[at], head[at + 1]];
                if big {
                    u16::from_be_bytes(bytes)
                } else {
                    u16::from_le_bytes(bytes)
                }
            } as usize;
            let (name_len, data_len) = (read_u16(6), read_u16(8));
            let pad = |n: usize| (4 - n % 4) % 4;
            let mut rest = vec![0u8; name_len + pad(name_len) + data_len + pad(data_len)];
            if socket.read_exact(&mut rest).await.is_err() {
                return;
            }
            let name = String::from_utf8_lossy(&rest[..name_len]).into_owned();
            let data_at = name_len + pad(name_len);
            seen.lock()
                .unwrap()
                .push((name, rest[data_at..data_at + data_len].to_vec()));
            // 连接被拒绝（Failed）：原因、协议版本 11.0、附加数据的长度（4 字节为单位）
            let reason = b"termlet-fake-x";
            let units = (reason.len() + pad(reason.len())) / 4;
            let u16_bytes = |v: u16| {
                if big {
                    v.to_be_bytes()
                } else {
                    v.to_le_bytes()
                }
            };
            let mut reply = vec![0u8, reason.len() as u8];
            reply.extend_from_slice(&u16_bytes(11));
            reply.extend_from_slice(&u16_bytes(0));
            reply.extend_from_slice(&u16_bytes(units as u16));
            reply.extend_from_slice(reason);
            reply.resize(reply.len() + pad(reason.len()), 0);
            let _ = socket.write_all(&reply).await;
        });
    }
}

/// Xauthority 文件中的一条（任意地址，显示号 `number`）。
fn xauthority_entry(number: u32, cookie: &[u8]) -> Vec<u8> {
    let mut out = 65535u16.to_be_bytes().to_vec();
    let number = number.to_string();
    for field in [&b""[..], number.as_bytes(), b"MIT-MAGIC-COOKIE-1", cookie] {
        out.extend_from_slice(&(field.len() as u16).to_be_bytes());
        out.extend_from_slice(field);
    }
    out
}

// X11 转发：服务器上的程序带着伪造的 cookie 连来，换成本机 Xauthority 中的真实 cookie 后送到本机的 X 服务器；
// 不带 cookie（不是经 sshd 认证的）的连接不送过去
#[test]
fn x11_forwarding_swaps_in_the_real_cookie() {
    let Some((host, port)) = env_value("TERMLET_TEST_X11")
        .and_then(|v| {
            v.rsplit_once(':')
                .map(|(h, p)| (h.to_string(), p.to_string()))
        })
        .and_then(|(h, p)| Some((h, p.parse::<u16>().ok()?)))
    else {
        return;
    };
    crate::runtime::runtime().block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let number = u32::from(listener.local_addr().unwrap().port()) - 6000;
        let seen = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn(fake_x_server(listener, seen.clone()));
        let real_cookie = [0x5au8; 16];
        let dir = tempfile::tempdir().unwrap();
        let xauthority = dir.path().join("Xauthority");
        std::fs::write(&xauthority, xauthority_entry(number, &real_cookie)).unwrap();

        let store = memory_store();
        let id = store
            .create(
                Host {
                    name: "x11".into(),
                    address: host.clone(),
                    port,
                    username: SSH_USER.into(),
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
        store
            .set_secret(
                &Owner::host(&id),
                SecretField::Password,
                Some(&SecretString::new(SSH_PASSWORD)),
            )
            .unwrap();
        let plan = plan_for_host(&store, &id).unwrap();
        let (prompter, _) = answering(false);
        let connection = connect(store.clone(), &plan, prompter, &ConnectOptions::default())
            .await
            .unwrap();
        connection.x11().use_xauthority(xauthority.clone());
        let notices = connection.x11_notices();
        // 先用一个不带 cookie 的连接（XAUTHORITY 指向不存在的文件），再正常连接
        let command = "XAUTHORITY=/nonexistent xdpyinfo >/dev/null 2>&1; \
                       xdpyinfo 2>&1 | head -3; echo \"display=$DISPLAY\"; echo x11-done";
        let (io, status) = open_shell_reporting(
            connection.clone(),
            ShellOptions {
                command: Some(command.into()),
                x11: Some(X11Options {
                    display: format!("127.0.0.1:{number}"),
                    trusted: true,
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(status, X11Status::Forwarding { trusted: true, .. }),
            "{status:?}"
        );
        let mut output = String::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while !output.contains("x11-done") {
            match tokio::time::timeout_at(deadline, io.output.recv()).await {
                Ok(Ok(termlet_io::IoEvent::Output(bytes))) => {
                    output.push_str(&String::from_utf8_lossy(&bytes))
                }
                Ok(Ok(termlet_io::IoEvent::Closed(_))) | Ok(Err(_)) => break,
                Err(_) => panic!("等待输出超时：{output}"),
            }
        }
        assert!(output.contains("display=localhost:1"), "{output}");
        assert!(
            output.contains("termlet-fake-x"),
            "X 程序应收到假服务器的回答：{output}"
        );
        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            [("MIT-MAGIC-COOKIE-1".to_string(), real_cookie.to_vec())],
            "只有带着伪造 cookie 的连接被送到本机，且换成了真实的 cookie"
        );
        assert!(!notices.has_changed().unwrap(), "X 服务器在运行：没有说明");
        connection.disconnect().await;
    });
}

// 服务器没有开启 X11 转发（测试容器 termlet-sshd 的默认）：报告被拒绝，终端照常
#[test]
fn x11_refused_by_the_server_is_reported() {
    let Some((host, port)) = crate::testkit::test_server() else {
        return;
    };
    crate::runtime::runtime().block_on(async {
        let store = memory_store();
        let id = store
            .create(
                Host {
                    name: "no-x11".into(),
                    address: host,
                    port,
                    username: SSH_USER.into(),
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
        store
            .set_secret(
                &Owner::host(&id),
                SecretField::Password,
                Some(&SecretString::new(SSH_PASSWORD)),
            )
            .unwrap();
        let plan = plan_for_host(&store, &id).unwrap();
        let (prompter, _) = answering(false);
        let connection = connect(store.clone(), &plan, prompter, &ConnectOptions::default())
            .await
            .unwrap();
        let (io, status) = open_shell_reporting(
            connection.clone(),
            ShellOptions {
                command: Some("echo still-$((1+1))".into()),
                x11: Some(X11Options {
                    display: "localhost:0".into(),
                    trusted: true,
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(status, X11Status::Refused);
        let mut output = String::new();
        while !output.contains("still-2") {
            match tokio::time::timeout(Duration::from_secs(10), io.output.recv()).await {
                Ok(Ok(termlet_io::IoEvent::Output(bytes))) => {
                    output.push_str(&String::from_utf8_lossy(&bytes))
                }
                other => panic!("{output} {other:?}"),
            }
        }
        connection.disconnect().await;
    });
}
