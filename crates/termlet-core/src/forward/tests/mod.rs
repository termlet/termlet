//! 端口转发的测试：进程内的 SSH 服务器（见 [`server`]）与真实 sshd（见 [`sshd`]，设置
//! `TERMLET_TEST_SSH=127.0.0.1:2222` 后运行，否则跳过）。
//! 规则校验与冲突判断的测试在 `model::forward`，SOCKS 握手的在 `socks`，解析监听端口的在 `detect`。

mod server;
mod sshd;

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::*;
use crate::id::HostId;
use crate::model::{Forward, ForwardKind, Host, Protocol, SecretField};
use crate::secret::SecretString;
use crate::ssh::{Answer, ConnectionPool, Prompt, Prompter, SshError, TerminalSpec, connect};
use crate::store::{Owner, Position, Store};
use crate::testkit::{accept_host_key, block_on, memory_store, wait_until_async};
use crate::{CoreError, Invalid};
use server::{PASSWORD, TestServer};

/// 测试用的参数：等待时间缩短。
fn fast_options() -> ForwardOptions {
    ForwardOptions {
        backoff_base: Duration::from_millis(20),
        backoff_max: Duration::from_millis(100),
        reconnect_timeout: Duration::from_secs(5),
        channel_timeout: Duration::from_secs(5),
        queue_timeout: Duration::from_secs(5),
        stats_interval: Duration::from_millis(50),
    }
}

fn add_host(store: &Store, name: &str, port: u16, password: &str) -> HostId {
    let id = store
        .create(
            Host {
                name: name.into(),
                address: "127.0.0.1".into(),
                port,
                username: "u".into(),
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
            Some(&SecretString::new(password)),
        )
        .unwrap();
    id
}

struct Env {
    store: Arc<Store>,
    manager: ForwardManager,
    host: HostId,
}

impl Env {
    /// 连到进程内 SSH 服务器的管理器；`server` 为 `None` 时主机指向没有服务的端口（建连总是失败）。
    fn new(server: Option<&TestServer>) -> Self {
        let store = memory_store();
        let port = server.map_or_else(free_port, |s| s.port);
        let host = add_host(&store, "h1", port, PASSWORD);
        let manager = ForwardManager::new(store.clone(), fast_options());
        Self {
            store,
            manager,
            host,
        }
    }

    fn rule(&self, doc: Forward) -> ForwardId {
        self.store
            .create(
                Forward {
                    host: Some(self.host.clone()),
                    ..doc
                },
                None,
                Position::Last,
            )
            .unwrap()
    }

    /// 本地转发 `127.0.0.1:bind_port → target`。
    fn local(&self, bind_port: u16, target: (&str, u16)) -> ForwardId {
        self.rule(Forward {
            bind_port,
            target_host: target.0.into(),
            target_port: target.1,
            ..Default::default()
        })
    }

    fn bind(&self, id: &ForwardId) -> String {
        self.store.get::<Forward>(id).unwrap().doc.bind_address()
    }

    async fn start(&self, id: &ForwardId, owner: &str) -> Result<(), ForwardError> {
        self.start_shared(id, owner, None).await
    }

    async fn start_shared(
        &self,
        id: &ForwardId,
        owner: &str,
        share_group: Option<String>,
    ) -> Result<(), ForwardError> {
        self.manager
            .start(StartRequest {
                rule: id.clone(),
                owner: owner.into(),
                share_group,
                prompter: accept_host_key(),
                until_disconnect: false,
            })
            .await
    }

    fn status(&self, id: &ForwardId) -> ForwardStatus {
        self.manager.status(id)
    }

    /// 运行中的转发当前用的 SSH 连接。
    fn connection(&self, id: &ForwardId) -> Arc<crate::ssh::Connection> {
        let book = self.manager.0.book();
        let runner = book.running.get(id).expect("转发没有在运行");
        let link = runner.link.borrow();
        link.as_ref().expect("没有可用的连接").connection.clone()
    }
}

impl Drop for Env {
    /// 测试结束时停止全部转发（在运行时内丢弃，不能等待）。
    fn drop(&mut self) {
        let manager = self.manager.clone();
        crate::runtime::spawn(async move { manager.close_all().await });
    }
}

/// 转发目标：原样返回收到的数据。
async fn echo_server() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    crate::runtime::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            crate::runtime::spawn(async move {
                let (mut read, mut write) = stream.split();
                let _ = tokio::io::copy(&mut read, &mut write).await;
                let _ = write.shutdown().await;
            });
        }
    });
    port
}

/// 一个当前空闲的本地端口。
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// 经本地端口发送一段内容并读回（目标为回显服务器）。
async fn round_trip(addr: &str, message: &str) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        stream.write_all(message.as_bytes()).await?;
        let mut buf = vec![0u8; message.len()];
        stream.read_exact(&mut buf).await?;
        Ok::<_, std::io::Error>(buf)
    })
    .await;
    match result {
        Ok(Ok(buf)) => assert_eq!(String::from_utf8_lossy(&buf), message),
        other => panic!("经 {addr} 回显失败：{other:?}"),
    }
}

/// 读一个字节：连接被关闭（EOF 或出错）时返回真，5 秒内没有结果时为假。
async fn closed_by_peer(stream: &mut TcpStream) -> bool {
    let mut byte = [0u8; 1];
    matches!(
        tokio::time::timeout(Duration::from_secs(5), stream.read(&mut byte)).await,
        Ok(Ok(0) | Err(_))
    )
}

fn can_listen(addr: &str) -> bool {
    std::net::TcpListener::bind(addr).is_ok()
}

// ---------------------------------------------------------------- 规则存储

#[test]
fn store_checks_bind_conflicts() {
    let store = memory_store();
    let h1 = add_host(&store, "h1", 22, "x");
    let h2 = add_host(&store, "h2", 22, "x");
    let local = |host: &HostId, bind: &str, port: u16| Forward {
        host: Some(host.clone()),
        name: "web".into(),
        bind_host: bind.into(),
        bind_port: port,
        target_host: "localhost".into(),
        target_port: 80,
        auto_start: true,
        ..Default::default()
    };
    let conflict = CoreError::Invalid(Invalid::new("bindPort", "conflict"));
    let a = store
        .create(local(&h1, "", 18080), None, Position::Last)
        .unwrap();
    let saved = store.get::<Forward>(&a).unwrap().doc;
    assert_eq!(saved.bind_host, "127.0.0.1");
    assert_eq!(saved.kind, ForwardKind::Local);

    // 同一本地地址与端口只能有一条规则；0.0.0.0 与 IPv4 回环地址同端口也冲突（与经由哪台主机无关）
    for bind in ["127.0.0.1", "localhost", "0.0.0.0"] {
        let error = store
            .create(local(&h2, bind, 18080), None, Position::Last)
            .unwrap_err();
        assert_eq!(error.to_string(), conflict.to_string(), "{bind}");
        // 界面据此说明被哪条规则占用
        assert_eq!(
            find_conflict(&store, &local(&h2, bind, 18080), None).map(|r| r.id),
            Some(a.clone())
        );
    }
    let b = store
        .create(local(&h2, "127.0.0.2", 18080), None, Position::Last)
        .expect("不同回环地址的同一端口应允许");

    // 修改：可以保持自己的端口，不能改成别人的地址
    store
        .update(
            &a,
            Forward {
                name: "web2".into(),
                ..local(&h1, "127.0.0.1", 18080)
            },
        )
        .unwrap();
    assert!(find_conflict(&store, &local(&h1, "127.0.0.1", 18080), Some(&a)).is_none());
    assert!(matches!(
        store.update(&b, local(&h2, "127.0.0.1", 18080)),
        Err(CoreError::Invalid(Invalid {
            reason: "conflict",
            ..
        }))
    ));
    // 主机不存在
    let missing = Forward {
        host: Some(HostId::from_raw("h_none")),
        ..local(&h1, "", 18081)
    };
    assert!(matches!(
        store.create(missing, None, Position::Last),
        Err(CoreError::Invalid(Invalid {
            field: "host",
            reason: "not_found"
        }))
    ));

    // 远程转发与本地转发同端口可以；同一台主机上的远程转发不能重复监听，另一台主机上的可以
    let remote = |host: &HostId, target_port: u16| Forward {
        kind: ForwardKind::Remote,
        host: Some(host.clone()),
        bind_port: 18080,
        target_host: "127.0.0.1".into(),
        target_port,
        ..Default::default()
    };
    store
        .create(remote(&h1, 3000), None, Position::Last)
        .expect("远程转发与本地转发同端口应允许");
    assert!(
        store
            .create(remote(&h1, 3001), None, Position::Last)
            .is_err()
    );
    store
        .create(remote(&h2, 3001), None, Position::Last)
        .expect("不同主机上的远程转发互不冲突");

    // 删除主机时级联删除其规则
    store.delete::<Host>(&h2).unwrap();
    assert!(store.get::<Forward>(&b).is_none());
    assert_eq!(store.list::<Forward>().len(), 2);
}

// ---------------------------------------------------------------- 运行管理（进程内 SSH 服务器）

#[test]
fn round_trip_and_stop() {
    block_on(async {
        let server = TestServer::start().await;
        let env = Env::new(Some(&server));
        let target = echo_server().await;
        let id = env.local(free_port(), ("127.0.0.1", target));
        let addr = env.bind(&id);
        let mut snapshots = env.manager.watch();

        env.start(&id, "w1").await.unwrap();
        let status = env.status(&id);
        assert_eq!(status.state, ForwardState::Running);
        assert_eq!(status.owner.as_deref(), Some("w1"));
        assert!(status.started_at.is_some());
        assert_eq!(env.manager.active(), 1);
        // 订阅者收到运行中的快照
        tokio::time::timeout(
            Duration::from_secs(5),
            snapshots.wait_for(|s| s.status(&id).state == ForwardState::Running),
        )
        .await
        .expect("应推送运行中的状态")
        .unwrap();
        // 重复启动直接成功，不重新建连
        env.start(&id, "w2").await.unwrap();
        assert_eq!(server.logins(), 1, "重复启动不应重新建连");

        round_trip(&addr, "hello").await;
        round_trip(&addr, "world!").await;
        wait_until_async("统计连接与字节数", || {
            let s = env.status(&id);
            s.accepted == 2 && s.connections == 0 && s.bytes_in == 11 && s.bytes_out == 11
        })
        .await;

        // 保持一个连接，停止后它应被关闭，端口应被释放
        let mut held = TcpStream::connect(&addr).await.unwrap();
        wait_until_async("连接数为 1", || env.status(&id).connections == 1).await;
        env.manager.stop(&id).await.unwrap();
        assert!(closed_by_peer(&mut held).await, "停止后已有连接应被关闭");
        assert_eq!(env.status(&id).state, ForwardState::Stopped);
        assert!(can_listen(&addr), "停止后端口应被释放");
        assert_eq!(env.manager.active(), 0);
        // 停止未运行的规则：成功；不存在的规则：RuleNotFound
        env.manager.stop(&id).await.unwrap();
        assert_eq!(
            env.manager.stop(&ForwardId::from_raw("f_none")).await,
            Err(ForwardError::RuleNotFound)
        );
    });
}

#[test]
fn port_in_use() {
    block_on(async {
        let env = Env::new(None);
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = occupied.local_addr().unwrap().port();
        let id = env.local(port, ("127.0.0.1", 22));
        let address = format!("127.0.0.1:{port}");

        let error = env.start(&id, "w1").await.unwrap_err();
        assert_eq!(error, ForwardError::PortInUse { address });
        // 端口被占用时不连接主机（主机指向没有服务的端口，连了会是连接失败）
        let status = env.status(&id);
        assert_eq!(status.state, ForwardState::Failed(error));
        assert_eq!(env.manager.active(), 0, "失败后不应有运行中的转发");
        // 停止会清除错误状态
        env.manager.stop(&id).await.unwrap();
        assert_eq!(env.status(&id).state, ForwardState::Stopped);
    });
}

#[test]
fn dial_failure_releases_port() {
    block_on(async {
        let env = Env::new(None);
        let id = env.local(free_port(), ("127.0.0.1", 22));
        let error = env.start(&id, "w1").await.unwrap_err();
        assert!(
            matches!(error, ForwardError::Connect(SshError::Refused { .. })),
            "{error:?}"
        );
        assert_eq!(env.status(&id).state, ForwardState::Failed(error));
        assert!(can_listen(&env.bind(&id)), "建连失败后端口应被释放");
    });
}

#[test]
fn reconnect_after_disconnect() {
    block_on(async {
        let server = TestServer::start().await;
        let env = Env::new(Some(&server));
        let target = echo_server().await;
        let id = env.local(free_port(), ("127.0.0.1", target));
        let addr = env.bind(&id);
        env.start(&id, "w1").await.unwrap();
        round_trip(&addr, "before").await;

        // 保持一个连接；SSH 断开后它应被关闭，转发进入重连
        let mut held = TcpStream::connect(&addr).await.unwrap();
        held.write_all(b"x").await.unwrap();
        let mut echo = [0u8; 1];
        held.read_exact(&mut echo).await.unwrap();
        wait_until_async("连接建立", || env.status(&id).connections == 1).await;

        // 先让重连失败几次，观察重连中的状态
        server.set_refuse(true);
        server.drop_all();
        wait_until_async("进入重连", || {
            matches!(
                env.status(&id).state,
                ForwardState::Reconnecting { attempt, last_error: Some(_) } if attempt >= 2
            )
        })
        .await;
        assert!(
            closed_by_peer(&mut held).await,
            "SSH 断开后其上的本地连接应被关闭"
        );

        // 重连期间接入的连接排队等待，恢复后照常转发
        let queued = crate::runtime::spawn({
            let addr = addr.clone();
            async move { round_trip(&addr, "queued").await }
        });
        wait_until_async("排队的连接被计入", || {
            env.status(&id).connections == 1
        })
        .await;
        server.set_refuse(false);
        queued.await.expect("重连期间排队的连接应在恢复后转发");
        wait_until_async("恢复运行", || {
            env.status(&id).state == ForwardState::Running
        })
        .await;
        round_trip(&addr, "after").await;
        // before、held、queued、after 共 4 个连接，统计跨重连累计
        assert_eq!(env.status(&id).accepted, 4);
        assert_eq!(server.logins(), 2, "应重新建立 SSH 连接");
    });
}

#[test]
fn reconnect_gives_up_on_auth_failure() {
    block_on(async {
        let server = TestServer::start().await;
        let env = Env::new(Some(&server));
        let id = env.local(free_port(), ("127.0.0.1", 1));
        env.start(&id, "w1").await.unwrap();
        // 密码已修改：保存的密码不对，重连时不能询问
        server.set_reject_auth(true);
        server.drop_all();
        wait_until_async("因认证失败停止", || {
            matches!(
                env.status(&id).state,
                ForwardState::Failed(ForwardError::Connect(
                    SshError::AuthCancelled | SshError::AuthFailed { .. }
                ))
            )
        })
        .await;
        assert_eq!(env.manager.active(), 0, "认证失败后不应继续重连");
        assert!(can_listen(&env.bind(&id)), "停止后端口应被释放");
        // 失败的状态保留所属窗口，窗口关闭时一并清除
        assert_eq!(env.status(&id).owner.as_deref(), Some("w1"));
        env.manager.close_owner("w1").await;
        assert_eq!(env.status(&id).state, ForwardState::Stopped);
    });
}

#[test]
fn target_refused_keeps_running() {
    block_on(async {
        let server = TestServer::start().await;
        let env = Env::new(Some(&server));
        let id = env.local(free_port(), ("127.0.0.1", free_port())); // 目标端口没有服务
        env.start(&id, "w1").await.unwrap();
        let mut stream = TcpStream::connect(env.bind(&id)).await.unwrap();
        assert!(
            closed_by_peer(&mut stream).await,
            "目标拒绝连接时本地连接应被关闭"
        );
        wait_until_async("记录目标连接失败", || {
            env.status(&id).last_error.is_some()
        })
        .await;
        let status = env.status(&id);
        assert!(
            matches!(status.last_error, Some(TargetError::Unreachable { .. })),
            "{status:?}"
        );
        assert_eq!(
            status.state,
            ForwardState::Running,
            "目标连接失败不影响转发本身"
        );
    });
}

#[test]
fn owner_update_delete_lifecycle() {
    block_on(async {
        let server = TestServer::start().await;
        let env = Env::new(Some(&server));
        let a = env.local(free_port(), ("127.0.0.1", 1));
        let b = env.local(free_port(), ("127.0.0.1", 1));
        env.start(&a, "w1").await.unwrap();
        env.start(&b, "w2").await.unwrap();

        // 窗口关闭只停止它启动的转发
        assert_eq!(env.manager.close_owner("w1").await, 1);
        assert_eq!(env.status(&a).state, ForwardState::Stopped);
        assert_eq!(env.status(&b).state, ForwardState::Running);

        // 只改名称、“自动启动”：照常运行，再次启动也不重新连接
        let logins = server.logins();
        let mut doc = env.store.get::<Forward>(&b).unwrap().doc;
        doc.name = "改名".into();
        doc.auto_start = true;
        env.store.update(&b, doc).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(env.status(&b).state, ForwardState::Running);
        env.start(&b, "w2").await.unwrap();
        assert_eq!(server.logins(), logins, "只改名称不应重新连接");

        // 修改运行中的规则的路线：先停止，由界面按需重新启动
        let mut doc = env.store.get::<Forward>(&b).unwrap().doc;
        doc.target_port = 2;
        env.store.update(&b, doc).unwrap();
        // 状态先变为已停止，端口随后释放（停止在后台完成）
        wait_until_async("修改后停止并释放端口", || {
            env.status(&b).state == ForwardState::Stopped && can_listen(&env.bind(&b))
        })
        .await;
        // 按新的内容重新启动
        env.start(&b, "w2").await.unwrap();
        assert_eq!(env.status(&b).state, ForwardState::Running);

        // 删除主机（级联删除规则）后停止
        let b_addr = env.bind(&b);
        env.store.delete::<Host>(&env.host).unwrap();
        wait_until_async("规则删除后转发停止并释放端口", || {
            env.manager.active() == 0 && can_listen(&b_addr)
        })
        .await;
    });
}

/// 经 SOCKS5 连接 `host:port`（域名类型），返回服务器的回复码与连接。
async fn socks5_connect(proxy: &str, host: &str, port: u16) -> (u8, TcpStream) {
    let mut stream = TcpStream::connect(proxy).await.unwrap();
    stream.write_all(&[5, 1, 0]).await.unwrap();
    let mut chosen = [0u8; 2];
    stream.read_exact(&mut chosen).await.unwrap();
    assert_eq!(chosen, [5, 0]);
    let mut request = vec![5, 1, 0];
    match host.parse::<std::net::Ipv4Addr>() {
        Ok(ip) => {
            request.push(1);
            request.extend_from_slice(&ip.octets());
        }
        Err(_) => {
            request.push(3);
            request.push(host.len() as u8);
            request.extend_from_slice(host.as_bytes());
        }
    }
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).await.unwrap();
    let mut reply = [0u8; 10];
    stream.read_exact(&mut reply).await.unwrap();
    (reply[1], stream)
}

/// 经 SOCKS4a 连接 `host:port`（域名由服务器解析），返回回复码（90 为成功）与连接。
async fn socks4a_connect(proxy: &str, host: &str, port: u16) -> (u8, TcpStream) {
    let mut stream = TcpStream::connect(proxy).await.unwrap();
    let mut request = vec![4, 1];
    request.extend_from_slice(&port.to_be_bytes());
    request.extend_from_slice(&[0, 0, 0, 1, b'u', 0]);
    request.extend_from_slice(host.as_bytes());
    request.push(0);
    stream.write_all(&request).await.unwrap();
    let mut reply = [0u8; 8];
    stream.read_exact(&mut reply).await.unwrap();
    (reply[1], stream)
}

async fn echo_through(stream: &mut TcpStream, message: &[u8]) {
    stream.write_all(message).await.unwrap();
    let mut buf = vec![0u8; message.len()];
    tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(buf, message);
}

/// 动态转发：经 SOCKS5（IP 与域名，域名由服务器解析）、SOCKS4a 访问回显服务器；目标拒绝连接时回复失败、
/// 转发保持运行；收到非 SOCKS 请求时记录原因。
#[test]
fn dynamic_forward() {
    block_on(async {
        let server = TestServer::start().await;
        let env = Env::new(Some(&server));
        let target = echo_server().await;
        let id = env.rule(Forward {
            kind: ForwardKind::Dynamic,
            bind_port: free_port(),
            target_host: "ignored".into(),
            target_port: 1,
            ..Default::default()
        });
        let rule = env.store.get::<Forward>(&id).unwrap().doc;
        assert!(
            rule.target_host.is_empty()
                && rule.target_port == 0
                && rule.target_address().is_empty(),
            "动态转发不应保存目标地址：{rule:?}"
        );
        env.start(&id, "w1").await.unwrap();
        let proxy = env.bind(&id);

        for host in ["127.0.0.1", "localhost"] {
            let (code, mut stream) = socks5_connect(&proxy, host, target).await;
            assert_eq!(code, 0, "经 SOCKS5 连接 {host} 失败");
            echo_through(&mut stream, b"socks").await;
        }
        let (code, mut stream) = socks4a_connect(&proxy, "localhost", target).await;
        assert_eq!(code, 90, "经 SOCKS4a 连接失败");
        echo_through(&mut stream, b"socks4a").await;

        let (code, _) = socks5_connect(&proxy, "127.0.0.1", free_port()).await;
        assert_eq!(code, 4, "目标端口没有服务时应回复主机不可达");
        let status = env.status(&id);
        assert_eq!(status.state, ForwardState::Running);
        assert!(matches!(
            status.last_error,
            Some(TargetError::Unreachable { .. })
        ));

        let mut http = TcpStream::connect(&proxy).await.unwrap();
        http.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
        assert!(closed_by_peer(&mut http).await, "非 SOCKS 请求应被关闭");
        wait_until_async("记录非 SOCKS 请求的原因", || {
            env.status(&id).last_error == Some(TargetError::Socks(SocksError::NotSocks))
        })
        .await;
    });
}

/// 远程转发：服务器上监听（测试中服务器在本机），接入的连接经 SSH 转回本机的回显服务器；
/// 断线重连后重新请服务器监听；停止后服务器上的监听随之关闭。
#[test]
fn remote_forward() {
    block_on(async {
        let server = TestServer::start().await;
        let env = Env::new(Some(&server));
        let target = echo_server().await;
        let id = env.rule(Forward {
            kind: ForwardKind::Remote,
            bind_host: String::new(),
            bind_port: free_port(),
            target_host: "127.0.0.1".into(),
            target_port: target,
            ..Default::default()
        });
        let addr = env.bind(&id);
        assert!(
            addr.starts_with("127.0.0.1:"),
            "服务器上的监听地址默认为 127.0.0.1：{addr}"
        );
        env.start(&id, "w1").await.unwrap();
        round_trip(&addr, "remote-hello").await;
        wait_until_async("统计", || {
            let s = env.status(&id);
            s.state == ForwardState::Running
                && s.accepted == 1
                && s.bytes_out == 12
                && s.bytes_in == 12
        })
        .await;

        // 断线：重连后服务器重新监听
        server.drop_all();
        wait_until_async("重连后恢复运行", || {
            env.status(&id).state == ForwardState::Running && server.logins() == 2
        })
        .await;
        let reachable = |addr: &str| std::net::TcpStream::connect(addr).is_ok();
        wait_until_async("重新监听", || reachable(&addr)).await;
        round_trip(&addr, "again").await;

        env.manager.stop(&id).await.unwrap();
        wait_until_async("服务器上的监听关闭", || !reachable(&addr)).await;
    });
}

/// 服务器拒绝监听：返回 RemoteListenDenied 并进入失败状态，连接被释放。
#[test]
fn remote_forward_denied() {
    block_on(async {
        let server = TestServer::start().await;
        let env = Env::new(Some(&server));
        let port = free_port();
        server.deny_remote(port);
        let id = env.rule(Forward {
            kind: ForwardKind::Remote,
            bind_port: port,
            target_host: "127.0.0.1".into(),
            target_port: 1,
            ..Default::default()
        });
        let error = env.start(&id, "w1").await.unwrap_err();
        assert_eq!(
            error,
            ForwardError::RemoteListenDenied {
                address: format!("127.0.0.1:{port}")
            }
        );
        assert_eq!(env.status(&id).state, ForwardState::Failed(error));
        wait_until_async("失败后应释放连接", || server.sessions() == 0).await;
    });
}

/// 共用连接：指定连接组启动的转发与组内其他使用者共用一条连接；停止时只释放自己的那一份。
#[test]
fn shares_terminal_connection() {
    block_on(async {
        let server = TestServer::start().await;
        let env = Env::new(Some(&server));
        let target = echo_server().await;
        // 模拟终端已经持有一条连接
        let spec = TerminalSpec::for_host(&env.store, &env.host).unwrap();
        let group = ConnectionPool::new_group();
        let terminal = ConnectionPool::global()
            .acquire(&group, || {
                connect(
                    env.store.clone(),
                    &spec.plan,
                    accept_host_key(),
                    &spec.connect,
                )
            })
            .await
            .unwrap();
        let id = env.local(free_port(), ("127.0.0.1", target));
        env.start_shared(&id, "w1", Some(group)).await.unwrap();
        assert_eq!(server.logins(), 1, "应共用终端的连接，不再建连");
        assert!(Arc::ptr_eq(&env.connection(&id), &terminal));
        round_trip(&env.bind(&id), "shared").await;
        env.manager.stop(&id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!terminal.is_closed(), "停止转发后终端的连接应保留");
        assert_eq!(server.sessions(), 1);
    });
}

/// 共用终端连接的转发跟着连接换窗口：只改这个连接组的、属于原来窗口的；之后原来的窗口关闭不影响它。
#[test]
fn shared_forward_follows_terminal_window() {
    block_on(async {
        let server = TestServer::start().await;
        let env = Env::new(Some(&server));
        let target = echo_server().await;
        let spec = TerminalSpec::for_host(&env.store, &env.host).unwrap();
        let group = ConnectionPool::new_group();
        let _terminal = ConnectionPool::global()
            .acquire(&group, || {
                connect(
                    env.store.clone(),
                    &spec.plan,
                    accept_host_key(),
                    &spec.connect,
                )
            })
            .await
            .unwrap();
        let shared = env.local(free_port(), ("127.0.0.1", target));
        env.start_shared(&shared, "w1", Some(group.clone()))
            .await
            .unwrap();
        let own = env.local(free_port(), ("127.0.0.1", target));
        env.start(&own, "w1").await.unwrap();

        assert_eq!(env.manager.reassign_group("w1", "other-group", "w2"), 0);
        assert_eq!(env.manager.reassign_group("w9", &group, "w2"), 0);
        assert_eq!(env.manager.reassign_group("w1", &group, "w2"), 1);
        assert_eq!(env.status(&shared).owner.as_deref(), Some("w2"));
        assert_eq!(env.status(&own).owner.as_deref(), Some("w1"));

        assert_eq!(env.manager.close_owner("w1").await, 1);
        assert_eq!(env.status(&own).state, ForwardState::Stopped);
        assert_eq!(env.status(&shared).state, ForwardState::Running);
        round_trip(&env.bind(&shared), "still here").await;
        assert_eq!(env.manager.close_owner("w2").await, 1);
        assert_eq!(env.status(&shared).state, ForwardState::Stopped);
    });
}

/// 共用终端连接的转发：终端关闭后连接断开，重连需要密码（是在终端里输入的）时等一阵终端，等不到就停止，
/// 不无限重试。
#[test]
fn shared_forward_stops_when_terminal_is_gone() {
    block_on(async {
        let server = TestServer::start().await;
        let env = Env::new(Some(&server));
        // 密码没有保存：终端是询问后登录的
        env.store
            .set_secret(&Owner::host(&env.host), SecretField::Password, None)
            .unwrap();
        let ask_password: Prompter = Arc::new(|prompt| {
            Box::pin(async move {
                match prompt {
                    Prompt::HostKey(_) => Answer::AcceptHostKey,
                    Prompt::Password { .. } => Answer::Secret(SecretString::new(PASSWORD)),
                    _ => Answer::Cancel,
                }
            })
        });
        let spec = TerminalSpec::for_host(&env.store, &env.host).unwrap();
        let group = ConnectionPool::new_group();
        let terminal = ConnectionPool::global()
            .acquire(&group, || {
                connect(env.store.clone(), &spec.plan, ask_password, &spec.connect)
            })
            .await
            .unwrap();
        let id = env.local(free_port(), ("127.0.0.1", 1));
        env.start_shared(&id, "w1", Some(group)).await.unwrap();
        // 终端关闭，随后连接断开
        drop(terminal);
        server.drop_all();
        wait_until_async("等不到终端后停止", || {
            matches!(
                env.status(&id).state,
                ForwardState::Failed(ForwardError::Connect(SshError::InteractionRequired))
            )
        })
        .await;
        assert_eq!(env.manager.active(), 0);
        assert_eq!(server.logins(), 1, "重连时不应再登录");
    });
}

/// 随连接结束而停（主机工具的“在浏览器中打开”）：共用的终端连接结束时停止、释放端口，不重连、不记为失败。
#[test]
fn until_disconnect_stops_with_the_connection() {
    block_on(async {
        let server = TestServer::start().await;
        let env = Env::new(Some(&server));
        let target = echo_server().await;
        let spec = TerminalSpec::for_host(&env.store, &env.host).unwrap();
        let group = ConnectionPool::new_group();
        let terminal = ConnectionPool::global()
            .acquire(&group, || {
                connect(
                    env.store.clone(),
                    &spec.plan,
                    accept_host_key(),
                    &spec.connect,
                )
            })
            .await
            .unwrap();
        let id = env.local(free_port(), ("127.0.0.1", target));
        env.manager
            .start(StartRequest {
                rule: id.clone(),
                owner: "w1".into(),
                share_group: Some(group),
                prompter: accept_host_key(),
                until_disconnect: true,
            })
            .await
            .unwrap();
        round_trip(&env.bind(&id), "browser").await;
        drop(terminal);
        server.drop_all();
        wait_until_async("随连接结束而停", || {
            env.status(&id).state == ForwardState::Stopped
        })
        .await;
        assert_eq!(env.manager.active(), 0);
        assert!(can_listen(&env.bind(&id)), "端口已释放");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(env.status(&id).state, ForwardState::Stopped, "不重连");
        assert_eq!(server.logins(), 1, "不重连");
    });
}

/// "只用一个通道"的主机不能共用终端的连接；不共用时照常单独连接。
#[test]
fn single_channel_host_does_not_share() {
    block_on(async {
        let server = TestServer::start().await;
        let env = Env::new(Some(&server));
        let mut host = env.store.get::<Host>(&env.host).unwrap().doc;
        host.ssh.single_channel = true;
        env.store.update(&env.host, host).unwrap();
        let id = env.local(free_port(), ("127.0.0.1", 1));
        assert_eq!(
            env.start_shared(&id, "w1", Some(ConnectionPool::new_group()))
                .await,
            Err(ForwardError::SingleChannel)
        );
        env.start(&id, "w1").await.unwrap();
        assert_eq!(env.status(&id).state, ForwardState::Running);
    });
}

/// 经由的主机改成了 Telnet：不能启动。
#[test]
fn non_ssh_host_is_rejected() {
    block_on(async {
        let env = Env::new(None);
        let id = env.local(free_port(), ("127.0.0.1", 1));
        let mut host = env.store.get::<Host>(&env.host).unwrap().doc;
        host.protocol = Protocol::Telnet;
        // 存储不允许转发经由非 SSH 主机，但主机本身可以改协议
        env.store.update(&env.host, host).unwrap();
        assert_eq!(env.start(&id, "w1").await, Err(ForwardError::NotSsh));
        assert!(can_listen(&env.bind(&id)));
    });
}

/// 自动启动：勾选了的、当前停止的规则（失败的不自动重试）。
#[test]
fn auto_start_rules() {
    block_on(async {
        let env = Env::new(None);
        let auto = env.rule(Forward {
            bind_port: free_port(),
            target_host: "127.0.0.1".into(),
            target_port: 1,
            auto_start: true,
            ..Default::default()
        });
        let failed = env.rule(Forward {
            bind_port: free_port(),
            target_host: "127.0.0.1".into(),
            target_port: 1,
            auto_start: true,
            ..Default::default()
        });
        env.local(free_port(), ("127.0.0.1", 1));
        assert!(env.start(&failed, "w1").await.is_err());
        assert_eq!(env.manager.auto_start_rules(), [auto]);
    });
}

#[test]
fn backoff_doubles_up_to_max() {
    let options = ForwardOptions::default();
    let delays: Vec<u64> = (1..=7).map(|n| options.backoff(n).as_secs()).collect();
    assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30]);
    assert_eq!(options.backoff(200).as_secs(), 30);
}
