//! 行规程与会话的测试：假的传输层、本机的最小 Telnet 服务端，
//! 以及设置了 `TERMLET_TEST_TELNET=127.0.0.1:2323`（testing/sshd 测试容器中的 busybox telnetd）时连真实的 telnetd，
//! 再设置 `TERMLET_TEST_SSH=127.0.0.1:2222` 时经 SSH 跳板机连接。

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::model::{Group, GroupDefaults, Jump, SecretField};
use crate::ssh::{Answer, Prompt, no_prompt};
use crate::store::{Owner, Position};
use crate::telnet::TelnetPlan;
use crate::testkit::{memory_store, test_server, test_telnet};

/// 模拟对方：记下发给对方的数据、传输文件的开关与是否关闭。
#[derive(Default)]
struct FakeWire {
    sent: Mutex<Vec<u8>>,
    echo: AtomicBool,
    binary: Mutex<Vec<bool>>,
    closed: AtomicBool,
    size: Mutex<Option<GridSize>>,
}

impl FakeWire {
    fn sent(&self) -> String {
        String::from_utf8_lossy(&self.sent.lock().unwrap()).into_owned()
    }
}

impl Wire for FakeWire {
    fn send(&self, data: &[u8]) {
        self.sent.lock().unwrap().extend_from_slice(data);
    }

    fn resize(&self, size: GridSize) {
        *self.size.lock().unwrap() = Some(size);
    }

    fn remote_echo(&self) -> bool {
        self.echo.load(Ordering::SeqCst)
    }

    fn set_binary(&self, on: bool) {
        self.binary.lock().unwrap().push(on);
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

fn fake(config: RawConfig) -> (Arc<FakeWire>, mpsc::Sender<WireEvent>, RawTerminal) {
    let wire = Arc::new(FakeWire::default());
    let (tx, rx) = mpsc::channel(16);
    let session = start(wire.clone(), rx, &config, None);
    (wire, tx, session)
}

/// 读到输出中出现 `want`，返回读到的全部输出。
fn read_until(io: &TerminalIo, want: &str) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut got = String::new();
    while !got.contains(want) {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!left.is_zero(), "等不到 {want:?}，已收到 {got:?}");
        let next = async { tokio::time::timeout(left, io.output.recv()).await };
        match crate::runtime::runtime().block_on(next) {
            Ok(Ok(IoEvent::Output(bytes))) => got.push_str(&String::from_utf8_lossy(&bytes)),
            Ok(Ok(IoEvent::Closed(reason))) => panic!("会话已结束（{reason:?}），已收到 {got:?}"),
            _ => panic!("等不到 {want:?}，已收到 {got:?}"),
        }
    }
    got
}

/// 等会话结束，返回结束原因（之前的输出丢掉）。
fn closed(io: &TerminalIo) -> CloseReason {
    let rt = crate::runtime::runtime();
    loop {
        let next = async { tokio::time::timeout(Duration::from_secs(10), io.output.recv()).await };
        match rt.block_on(next) {
            Ok(Ok(IoEvent::Closed(reason))) => return reason,
            Ok(Ok(IoEvent::Output(_))) => {}
            other => panic!("会话没有结束：{other:?}"),
        }
    }
}

fn wait_sent(wire: &FakeWire, want: &[u8]) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while *wire.sent.lock().unwrap() != want {
        assert!(
            std::time::Instant::now() < deadline,
            "发送 {:?}，期望 {want:?}",
            wire.sent()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn login_config(username: &str, password: Option<&str>) -> RawConfig {
    RawConfig {
        enter: Enter::Crlf,
        local_echo: LocalEcho::Off,
        username: username.into(),
        password: password.map(SecretString::new),
        ..Default::default()
    }
}

// 登录、密码提示各填写一次，密码错误后再次出现的提示交给用户
#[test]
fn auto_login() {
    let (wire, tx, session) = fake(login_config("admin", Some("pw")));
    tx.blocking_send(WireEvent::Data(
        b"\r\nLogin authentication\r\n\r\nUsername:".to_vec(),
    ))
    .unwrap();
    read_until(&session.io, "Username:");
    tx.blocking_send(WireEvent::Data(
        b"admin\r\n\x1b[1mPassword:\x1b[0m ".to_vec(),
    ))
    .unwrap();
    read_until(&session.io, "Password:");
    tx.blocking_send(WireEvent::Data(
        b"\r\n% Login failed!\r\nUsername:".to_vec(),
    ))
    .unwrap();
    read_until(&session.io, "failed!\r\nUsername:");
    wait_sent(&wire, b"admin\r\npw\r\n");
}

// 用户同时在输入（本地回显）、切换了编码，提示照样被认出
#[test]
fn auto_login_with_local_echo_and_encoding_switch() {
    let (wire, tx, session) = fake(RawConfig {
        local_echo: LocalEcho::On,
        ..login_config("admin", None)
    });
    session.io.input.write(b"x");
    tx.blocking_send(WireEvent::Data(b"host login: ".to_vec()))
        .unwrap();
    // 回显与对方的输出谁先到达终端不一定（几乎同时发生）
    let mut got = read_until(&session.io, "login: ");
    if !got.contains('x') {
        got += &read_until(&session.io, "x");
    }
    assert!(got.contains('x'), "本地回显 {got:?}");
    wait_sent(&wire, b"xadmin\r\n");

    let (wire, tx, session) = fake(login_config("admin", None));
    session.io.input.set_encoding("gb18030");
    tx.blocking_send(WireEvent::Data(b"\xd3\xc3\xbb\xa7\xc3\xfb: ".to_vec()))
        .unwrap();
    read_until(&session.io, "用户名: ");
    wait_sent(&wire, b"admin\r\n");
}

// 对方不回显时在本地显示输入，对方回显时（"自动"）不显示
#[test]
fn local_echo_follows_the_remote() {
    let (wire, tx, session) = fake(RawConfig {
        enter: Enter::Cr,
        local_echo: LocalEcho::Auto,
        ..Default::default()
    });
    session.io.input.write(b"ab\x7f\x1b[Ac\r");
    assert_eq!(read_until(&session.io, "\r\n"), "ab\x08 \x08c\r\n");
    wire.echo.store(true, Ordering::SeqCst);
    session.io.input.write(b"x");
    tx.blocking_send(WireEvent::Data(b"y".to_vec())).unwrap();
    assert_eq!(read_until(&session.io, "y"), "y");
    wait_sent(&wire, b"ab\x7f\x1b[Ac\rx");
}

#[test]
fn encoding_and_live_switch() {
    let (wire, tx, session) = fake(RawConfig {
        encoding: Encoding::Gb18030,
        local_echo: LocalEcho::Off,
        ..Default::default()
    });
    let data = b"\xbd\xbb\xbb\xbb\xbb\xfa>";
    tx.blocking_send(WireEvent::Data(data[..3].to_vec()))
        .unwrap();
    tx.blocking_send(WireEvent::Data(data[3..].to_vec()))
        .unwrap();
    read_until(&session.io, "交换机>");
    session.io.input.write("显示\r".as_bytes());
    wait_sent(&wire, b"\xcf\xd4\xca\xbe\r\n");
    // 不认识的编码：保持原来的
    session.io.input.set_encoding("latin1");
    tx.blocking_send(WireEvent::Data(b"\xc4\xe3\xba\xc3".to_vec()))
        .unwrap();
    read_until(&session.io, "你好");
    session.io.input.set_encoding("utf-8");
    tx.blocking_send(WireEvent::Data("好的".as_bytes().to_vec()))
        .unwrap();
    read_until(&session.io, "好的");
    session.io.input.resize(GridSize::new(100, 30));
    assert_eq!(*wire.size.lock().unwrap(), Some(GridSize::new(100, 30)));
}

// 传输文件期间原样收发，并通知传输层
#[test]
fn binary_mode_passes_bytes_through() {
    let (wire, tx, session) = fake(RawConfig {
        encoding: Encoding::Gb18030,
        local_echo: LocalEcho::On,
        ..Default::default()
    });
    session.io.input.set_binary(true);
    let raw = [0x18, b'B', 0xd6, 0xd0, b'\r', 0xff];
    session.io.input.write(&raw);
    assert_eq!(*wire.sent.lock().unwrap(), raw);
    tx.blocking_send(WireEvent::Data(raw.to_vec())).unwrap();
    let rt = crate::runtime::runtime();
    match rt.block_on(session.io.output.recv()).unwrap() {
        IoEvent::Output(bytes) => assert_eq!(bytes, raw, "不回显、不解码"),
        other => panic!("{other:?}"),
    }
    session.io.input.set_binary(false);
    assert_eq!(*wire.binary.lock().unwrap(), [true, false]);
    // 结束后照常：本地回显、回车映射
    wire.sent.lock().unwrap().clear();
    session.io.input.write(b"\r");
    assert_eq!(read_until(&session.io, "\r\n"), "\r\n");
    wait_sent(&wire, b"\r\n");
}

#[test]
fn close_reasons() {
    // Telnet：服务器关闭连接是正常结束
    let (_, tx, session) = fake(RawConfig {
        eof_exit: true,
        ..Default::default()
    });
    tx.blocking_send(WireEvent::Data(b"bye".to_vec())).unwrap();
    tx.blocking_send(WireEvent::Eof).unwrap();
    read_until(&session.io, "bye");
    assert_eq!(closed(&session.io), CloseReason::Exited(Some(0)));

    // 串口：设备断开是断线（界面自动重连）
    let (_, tx, session) = fake(RawConfig::default());
    tx.blocking_send(WireEvent::Eof).unwrap();
    assert!(matches!(closed(&session.io), CloseReason::Lost(_)));
    let (_, tx, session) = fake(RawConfig::default());
    tx.blocking_send(WireEvent::Lost("串口 COM3 已断开".into()))
        .unwrap();
    assert_eq!(
        closed(&session.io),
        CloseReason::Lost("串口 COM3 已断开".into())
    );

    // 主动关闭：通知传输层，之后的结束不算断线
    let (wire, tx, session) = fake(RawConfig {
        eof_exit: true,
        ..Default::default()
    });
    session.io.input.close();
    assert!(wire.closed.load(Ordering::SeqCst));
    tx.blocking_send(WireEvent::Lost("connection reset".into()))
        .unwrap();
    assert_eq!(closed(&session.io), CloseReason::Exited(None));
}

fn set_password(store: &Store, id: &HostId, password: &str) {
    store
        .set_secret(
            &Owner::host(id),
            SecretField::Password,
            Some(&SecretString::new(password)),
        )
        .unwrap();
}

// Telnet 主机的连接配置：经跳板机、自动登录的用户名与密码；Telnet 主机不能作为跳板机
#[test]
fn spec_for_saved_hosts() {
    let store = memory_store();
    let bastion = store
        .create(
            Host {
                name: "bastion".into(),
                address: "10.0.0.9".into(),
                username: "ops".into(),
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    let group = store
        .create(
            Group {
                name: "switches".into(),
                defaults: GroupDefaults {
                    username: "admin".into(),
                    jump: Some(bastion.clone()),
                    ..Default::default()
                },
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    let telnet = store
        .create(
            Host {
                protocol: Protocol::Telnet,
                address: "10.0.0.2".into(),
                terminal: crate::model::TerminalOptions {
                    encoding: Encoding::Gb18030,
                    ..Default::default()
                },
                ..Default::default()
            },
            Some(group.as_str().to_string()),
            Position::Last,
        )
        .unwrap();
    set_password(&store, &telnet, "sw-pw");
    let spec = RawSpec::for_host(&store, &telnet).unwrap();
    let RawTarget::Telnet(plan) = &spec.target else {
        panic!("{spec:?}")
    };
    assert_eq!((plan.address.as_str(), plan.port), ("10.0.0.2", 23));
    assert_eq!(plan.jumps.len(), 1, "沿用分组的跳板机");
    assert_eq!(plan.jumps[0].host_id.as_ref(), Some(&bastion));
    assert!(plan.proxy.is_none());
    assert_eq!(plan.term_type, "xterm-256color");
    assert_eq!(spec.config.username, "admin", "沿用分组的用户名");
    assert_eq!(
        spec.config.password.as_ref().map(|p| p.expose()),
        Some("sw-pw")
    );
    assert_eq!(spec.config.encoding, Encoding::Gb18030);
    assert_eq!(spec.config.enter, Enter::Crlf);
    assert!(spec.config.eof_exit);
    assert!(!format!("{spec:?}").contains("sw-pw"), "Debug 中不出现密码");

    // Telnet 主机不能作为跳板机（主机与分组默认值都不行）
    let mut ssh = Host {
        address: "10.0.0.3".into(),
        username: "u".into(),
        jump: Jump::Via {
            host: telnet.clone(),
        },
        ..Default::default()
    };
    let error = store.create(ssh.clone(), None, Position::Last).unwrap_err();
    assert!(error.to_string().contains("not_ssh"), "{error}");
    ssh.jump = Jump::Direct;
    store.create(ssh, None, Position::Last).unwrap();
    let mut doc = store.get::<Group>(&group).unwrap().doc;
    doc.defaults.jump = Some(telnet.clone());
    assert!(store.update(&group, doc).is_err());

    // 直连、没有用户名：不自动填写用户名（不用本机的用户名）
    let direct = store
        .create(
            Host {
                protocol: Protocol::Telnet,
                address: "10.0.0.4".into(),
                port: 2323,
                jump: Jump::Direct,
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    let spec = RawSpec::for_host(&store, &direct).unwrap();
    let RawTarget::Telnet(plan) = &spec.target else {
        panic!()
    };
    assert!(plan.jumps.is_empty() && plan.port == 2323);
    assert!(spec.config.username.is_empty() && spec.config.password.is_none());

    // 串口；SSH 主机不在这里打开
    let serial = store
        .create(
            Host {
                protocol: Protocol::Serial,
                address: "COM3".into(),
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    let spec = RawSpec::for_host(&store, &serial).unwrap();
    assert!(matches!(&spec.target, RawTarget::Serial(c) if c.port == "COM3" && c.baud == 115_200));
    assert_eq!(spec.config.enter, Enter::Cr);
    assert_eq!(spec.config.local_echo, LocalEcho::Off);
    assert!(!spec.config.eof_exit);
    assert_eq!(
        RawSpec::for_host(&store, &bastion).unwrap_err(),
        RawError::NotRaw
    );
    assert!(RawSpec::for_host(&store, &HostId::from_raw("h_missing")).is_err());
}

/// 本机的最小 Telnet 服务端：协商（同意 NAWS，自己回显），询问用户名、密码，登录后报告窗口大小，`exit` 后关闭连接。
async fn mini_telnetd(listener: tokio::net::TcpListener) -> Vec<u8> {
    const IAC: u8 = 255;
    let (mut socket, _) = listener.accept().await.unwrap();
    socket
        .write_all(&[IAC, 253, 31, IAC, 251, 1, IAC, 251, 3])
        .await
        .unwrap();
    socket.write_all(b"\r\nswitch login: ").await.unwrap();
    // 收到的全部字节（含协商）与去掉命令后的数据
    let mut raw = Vec::new();
    let mut data = Vec::new();
    let mut naws = None;
    let mut stage = 0;
    let mut buf = [0u8; 1024];
    loop {
        let n = socket.read(&mut buf).await.unwrap();
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
        // 简单地解析：IAC 命令三个字节，NAWS 子协商九个字节
        data.clear();
        let mut i = 0;
        while i < raw.len() {
            if raw[i] == IAC && raw.get(i + 1) == Some(&250) && raw.get(i + 2) == Some(&31) {
                if let Some(frame) = raw.get(i + 3..i + 7) {
                    naws = Some((
                        u16::from_be_bytes([frame[0], frame[1]]),
                        u16::from_be_bytes([frame[2], frame[3]]),
                    ));
                }
                i += 9;
            } else if raw[i] == IAC {
                i += 3;
            } else {
                data.push(raw[i]);
                i += 1;
            }
        }
        let text = String::from_utf8_lossy(&data).into_owned();
        if stage == 0 && text.ends_with("admin\r\n") {
            stage = 1;
            socket.write_all(b"admin\r\nPassword: ").await.unwrap();
        } else if stage == 1 && text.ends_with("secret\r\n") {
            stage = 2;
            let (cols, rows) = naws.unwrap_or_default();
            let banner = format!("\r\nsize {cols}x{rows}\r\n$ ");
            socket.write_all(banner.as_bytes()).await.unwrap();
        } else if stage == 2 && text.ends_with("exit\r\n") {
            socket.write_all(b"exit\r\nbye\r\n").await.unwrap();
            break;
        }
    }
    raw
}

fn telnet_spec(address: &str, port: u16, username: &str, password: &str) -> RawSpec {
    RawSpec {
        target: RawTarget::Telnet(TelnetPlan {
            label: address.into(),
            address: address.into(),
            port,
            username: username.into(),
            password: Some(SecretString::new(password)),
            proxy: None,
            jumps: Vec::new(),
            term_type: "xterm-256color".into(),
        }),
        config: RawConfig {
            eof_exit: true,
            username: username.into(),
            password: Some(SecretString::new(password)),
            ..Default::default()
        },
    }
}

// 完整的 Telnet 会话：协商窗口大小、自动登录、服务器回显时不在本地回显、logout 后正常结束
#[test]
fn telnet_session_against_a_local_server() {
    let rt = crate::runtime::runtime();
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = crate::runtime::spawn(mini_telnetd(listener));
    let session = rt
        .block_on(open(
            memory_store(),
            telnet_spec("127.0.0.1", port, "admin", "secret"),
            GridSize::new(100, 30),
            no_prompt(),
            &ConnectOptions::default(),
        ))
        .unwrap();
    let got = read_until(&session.io, "$ ");
    assert!(got.contains("size 100x30"), "{got:?}");
    // 服务器同意了 ECHO：本地回显为"自动"时不在本地回显
    session.io.input.write(b"exit\r");
    let got = read_until(&session.io, "bye");
    assert_eq!(
        got.matches("exit").count(),
        1,
        "不应在本地再回显一次：{got:?}"
    );
    assert_eq!(closed(&session.io), CloseReason::Exited(Some(0)));
    let raw = rt.block_on(server).unwrap();
    // 客户端先主动协商，之后收到的 DO NAWS 只回窗口大小
    assert!(raw.starts_with(&[255, 251, 31, 255, 251, 24, 255, 253, 3, 255, 253, 1]));
    assert!(
        raw.windows(9)
            .any(|w| w == [255, 250, 31, 0, 100, 0, 30, 255, 240])
    );
}

#[test]
fn telnet_connection_refused_is_retryable() {
    let rt = crate::runtime::runtime();
    // 拿一个空闲端口再关掉：没有服务
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let error = rt
        .block_on(open(
            memory_store(),
            telnet_spec("127.0.0.1", port, "", ""),
            GridSize::new(80, 24),
            no_prompt(),
            &ConnectOptions::default(),
        ))
        .err()
        .unwrap();
    assert!(
        matches!(error, RawError::Connect(SshError::Refused { .. })),
        "{error:?}"
    );
    assert!(error.is_retryable());
    let plan = match telnet_spec("127.0.0.1", port, "", "").target {
        RawTarget::Telnet(plan) => plan,
        RawTarget::Serial(_) => unreachable!(),
    };
    assert!(
        rt.block_on(crate::telnet::test_connection(
            memory_store(),
            &plan,
            no_prompt(),
            &ConnectOptions::default()
        ))
        .is_err()
    );
}

fn run_telnetd_session(store: Arc<Store>, spec: RawSpec, prompter: Prompter) {
    let rt = crate::runtime::runtime();
    let session = rt
        .block_on(open(
            store,
            spec,
            GridSize::new(100, 30),
            prompter,
            &ConnectOptions::default(),
        ))
        .unwrap();
    // 自动登录后执行命令：stty size 是 NAWS 通知的大小（busybox telnetd 不询问终端类型）
    read_until(&session.io, "$ ");
    session.io.input.write(b"stty size; echo SUM-$((40+2))\r");
    let got = read_until(&session.io, "SUM-42");
    assert!(got.contains("30 100"), "窗口大小不对：{got:?}");
    // telnetd 回显输入：不应在本地再回显一次
    assert_eq!(got.matches("stty size").count(), 1, "{got:?}");
    session.io.input.resize(GridSize::new(120, 40));
    session.io.input.write(b"stty size\r");
    read_until(&session.io, "40 120");
    session.io.input.write(b"exit\r");
    assert_eq!(closed(&session.io), CloseReason::Exited(Some(0)));
}

// 连接真实的 telnetd，协商、自动登录、执行命令、logout 后正常结束
#[test]
fn real_telnetd() {
    let Some((host, port)) = test_telnet() else {
        println!("未设置 TERMLET_TEST_TELNET，跳过");
        return;
    };
    run_telnetd_session(
        memory_store(),
        telnet_spec(&host, port, "termlet", "termlet-e2e"),
        no_prompt(),
    );
}

// 经 SSH 跳板机连 telnetd（容器自己的 23 端口），跳板机的指纹同样要确认
#[test]
fn real_telnetd_through_a_jump_host() {
    let (Some(_), Some((ssh_host, ssh_port))) = (test_telnet(), test_server()) else {
        println!("未设置 TERMLET_TEST_TELNET、TERMLET_TEST_SSH，跳过");
        return;
    };
    let store = memory_store();
    let bastion = store
        .create(
            Host {
                name: "bastion".into(),
                address: ssh_host,
                port: ssh_port,
                username: "termlet".into(),
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    set_password(&store, &bastion, "termlet-e2e");
    let telnet = store
        .create(
            Host {
                protocol: Protocol::Telnet,
                address: "127.0.0.1".into(),
                username: "termlet".into(),
                jump: Jump::Via { host: bastion },
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    set_password(&store, &telnet, "termlet-e2e");
    let asked = Arc::new(Mutex::new(Vec::new()));
    let log = asked.clone();
    let prompter: Prompter = Arc::new(move |prompt| {
        let log = log.clone();
        Box::pin(async move {
            match prompt {
                Prompt::HostKey(_) => {
                    log.lock().unwrap().push("hostkey");
                    Answer::AcceptHostKey
                }
                _ => Answer::Cancel,
            }
        })
    });
    let spec = RawSpec::for_host(&store, &telnet).unwrap();
    let plan = match &spec.target {
        RawTarget::Telnet(plan) => plan.clone(),
        RawTarget::Serial(_) => unreachable!(),
    };
    run_telnetd_session(store.clone(), spec, prompter.clone());
    assert_eq!(
        *asked.lock().unwrap(),
        ["hostkey"],
        "首次连接确认跳板机的指纹"
    );
    let rt = crate::runtime::runtime();
    let elapsed = rt
        .block_on(crate::telnet::test_connection(
            store,
            &plan,
            prompter,
            &ConnectOptions::default(),
        ))
        .unwrap();
    assert!(elapsed > Duration::ZERO);
}
