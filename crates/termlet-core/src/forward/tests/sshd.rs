//! 对真实 sshd 的集成测试（转发到容器内 sshd 自身的 127.0.0.1:22）：
//! 设置 `TERMLET_TEST_SSH=127.0.0.1:2222`（testing/sshd 的测试容器，账号 termlet / termlet-e2e）后运行，否则跳过。
//!
//! 测试容器的 sshd：`AllowTcpForwarding yes`、`GatewayPorts no`（远程转发只监听容器内的回环地址）。

use std::time::Duration;

use tokio::io::AsyncBufReadExt;

use super::*;
use crate::ssh::{Connection, exec, plan_for_host};
use crate::testkit::{SSH_USER, server_store};

/// 连到测试容器的管理器。
fn docker_env() -> Option<Env> {
    let (store, host) = server_store("docker", SSH_USER)?;
    let manager = ForwardManager::new(store.clone(), fast_options());
    Some(Env {
        store,
        manager,
        host,
    })
}

/// 另开一条到测试容器的连接，用于在容器内执行检查命令。
async fn probe(env: &Env) -> Arc<Connection> {
    let plan = plan_for_host(&env.store, &env.host).unwrap();
    connect(
        env.store.clone(),
        &plan,
        accept_host_key(),
        &crate::ssh::ConnectOptions::default(),
    )
    .await
    .unwrap()
}

/// 在容器内执行命令，返回退出码与标准输出。
async fn run(connection: &Connection, command: &str) -> (Option<u32>, String) {
    let result = exec(connection, command, 64 * 1024, Duration::from_secs(20))
        .await
        .unwrap();
    (
        result.exit_code,
        String::from_utf8_lossy(&result.stdout).trim().to_string(),
    )
}

/// 经本地端口连接，读取 SSH 服务器的版本行。
async fn read_banner(addr: &str) -> std::io::Result<String> {
    let stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr))
        .await
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))??;
    read_line(stream).await
}

async fn read_line(stream: TcpStream) -> std::io::Result<String> {
    let mut reader = tokio::io::BufReader::new(stream);
    let mut line = String::new();
    match tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line)).await {
        Ok(Ok(0)) => Err(std::io::ErrorKind::UnexpectedEof.into()),
        Ok(Ok(_)) => Ok(line.trim().to_string()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(std::io::ErrorKind::TimedOut.into()),
    }
}

/// 容器内 sshd 用不到的随机端口。
fn remote_port() -> u16 {
    20000 + (crate::time::now_ms() % 10000) as u16
}

#[test]
fn docker_forward_banner_and_stop() {
    let Some(env) = docker_env() else {
        return;
    };
    block_on(async {
        let id = env.local(free_port(), ("127.0.0.1", 22));
        let addr = env.bind(&id);
        env.start(&id, "w1").await.unwrap();
        let banner = read_banner(&addr).await.unwrap();
        assert!(
            banner.starts_with("SSH-2.0"),
            "应经转发读到 SSH 横幅：{banner}"
        );
        wait_until_async("统计字节数", || {
            let s = env.status(&id);
            s.bytes_in >= banner.len() as u64 && s.accepted == 1 && s.connections == 0
        })
        .await;

        env.manager.stop(&id).await.unwrap();
        assert!(read_banner(&addr).await.is_err(), "停止后连接应被拒绝");
        assert!(can_listen(&addr), "停止后端口应被释放");
    });
}

#[test]
fn docker_reconnect() {
    let Some(env) = docker_env() else {
        return;
    };
    block_on(async {
        let id = env.local(free_port(), ("127.0.0.1", 22));
        let addr = env.bind(&id);
        env.start(&id, "w1").await.unwrap();
        read_banner(&addr).await.unwrap();

        // 从本端断开 SSH 连接，模拟网络中断：转发应自动重连（不询问，指纹已记录、密码已保存）
        let first = env.connection(&id);
        first.disconnect().await;
        // 断开后立即接入的连接排队等待，恢复后照常转发
        let banner = read_banner(&addr).await.unwrap();
        assert!(
            banner.starts_with("SSH-2.0"),
            "重连后应读到 SSH 横幅：{banner}"
        );
        wait_until_async("恢复运行", || {
            env.status(&id).state == ForwardState::Running
        })
        .await;
        assert!(
            !Arc::ptr_eq(&first, &env.connection(&id)),
            "应重新建立 SSH 连接"
        );
        let banner = read_banner(&addr).await.unwrap();
        assert!(banner.starts_with("SSH-2.0"), "恢复后应正常转发：{banner}");
    });
}

/// 远程转发：容器内的 sshd 监听端口，容器内的连接经 SSH 回到本机的回显服务器；停止后不再监听。
#[test]
fn docker_remote_forward() {
    let Some(env) = docker_env() else {
        return;
    };
    block_on(async {
        let target = echo_server().await;
        let port = remote_port();
        let id = env.rule(Forward {
            kind: ForwardKind::Remote,
            bind_port: port,
            target_host: "127.0.0.1".into(),
            target_port: target,
            ..Default::default()
        });
        env.start(&id, "w1").await.unwrap();
        let probe = probe(&env).await;
        let command = format!(
            "bash -c 'exec 3<>/dev/tcp/127.0.0.1/{port} && echo docker-remote >&3 && timeout 5 head -n1 <&3'"
        );
        let (code, out) = run(&probe, &command).await;
        assert_eq!(
            (code, out.as_str()),
            (Some(0), "docker-remote"),
            "容器内应经远程转发连到本机回显服务器"
        );
        wait_until_async("统计", || env.status(&id).accepted == 1).await;

        env.manager.stop(&id).await.unwrap();
        let check = format!("bash -c 'exec 3<>/dev/tcp/127.0.0.1/{port}' 2>/dev/null");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let (code, _) = run(&probe, &check).await;
            if code != Some(0) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "服务器上的端口应不再监听"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
}

/// 服务器上的端口已被占用（sshd 自己在 22 上监听）：sshd 拒绝。
/// （容器内允许普通用户监听 1024 以下的端口，测不了"低于 1024"的情形。）
#[test]
fn docker_remote_port_in_use_denied() {
    let Some(env) = docker_env() else {
        return;
    };
    block_on(async {
        let id = env.rule(Forward {
            kind: ForwardKind::Remote,
            bind_port: 22,
            target_host: "127.0.0.1".into(),
            target_port: 1,
            ..Default::default()
        });
        assert_eq!(
            env.start(&id, "w1").await,
            Err(ForwardError::RemoteListenDenied {
                address: "127.0.0.1:22".into()
            })
        );
    });
}

/// 动态转发：SOCKS5（IP）与 SOCKS4a（域名由服务器解析）访问容器内的 sshd。
#[test]
fn docker_dynamic_forward() {
    let Some(env) = docker_env() else {
        return;
    };
    block_on(async {
        let id = env.rule(Forward {
            kind: ForwardKind::Dynamic,
            bind_port: free_port(),
            ..Default::default()
        });
        env.start(&id, "w1").await.unwrap();
        let proxy = env.bind(&id);
        let (code, stream) = socks5_connect(&proxy, "127.0.0.1", 22).await;
        assert_eq!(code, 0);
        assert!(read_line(stream).await.unwrap().starts_with("SSH-2.0"));
        let (code, stream) = socks4a_connect(&proxy, "localhost", 22).await;
        assert_eq!(code, 90);
        assert!(read_line(stream).await.unwrap().starts_with("SSH-2.0"));
    });
}

/// 检测服务器端口：测试容器中只有 busybox 的 netstat（没有 ss），sshd 在 22、telnetd 在 23。
#[test]
fn docker_listening_ports() {
    let Some(env) = docker_env() else {
        return;
    };
    block_on(async {
        let connection = probe(&env).await;
        let ports = listening_ports(&connection).await.unwrap();
        for port in [22, 23] {
            assert!(
                ports.iter().any(|p| p.port == port),
                "应列出 {port}：{ports:?}"
            );
        }
    });
}
