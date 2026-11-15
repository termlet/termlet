//! 进程内的 SSH 测试服务器（russh 的服务端）：密码认证（用户名任意，密码 `pw`），
//! 支持 direct-tcpip 与远程转发（tcpip-forward 时在本机监听：测试中服务器与客户端在同一台机器上）。
//! 可以断开全部连接（模拟网络中断）、拒绝新连接、拒绝认证、拒绝某些端口的远程转发。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::server::{Auth, ChannelOpenHandle, Handler, Msg, Session, run_stream};
use russh::{Channel, ChannelOpenFailure, MethodKind, MethodSet};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::sshkey::{KeyKind, generate};

/// 测试服务器接受的密码。
pub(super) const PASSWORD: &str = "pw";

#[derive(Default)]
struct ServerState {
    /// 每条连接的套接字副本：关闭它即断开连接
    sockets: Mutex<Vec<std::net::TcpStream>>,
    deny_remote: Mutex<HashSet<u32>>,
    reject_auth: AtomicBool,
    refuse: AtomicBool,
    /// 认证成功的次数（建立了几条连接）
    logins: AtomicUsize,
    /// 还在进行的会话数
    sessions: AtomicUsize,
}

pub(super) struct TestServer {
    pub port: u16,
    state: Arc<ServerState>,
    accept: JoinHandle<()>,
}

impl TestServer {
    /// 在 127.0.0.1 的随机端口上启动。必须在核心的运行时上调用。
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let key = generate(KeyKind::Ed25519, "")
            .unwrap()
            .into_ssh_private_key();
        let config = Arc::new(russh::server::Config {
            keys: vec![key],
            methods: MethodSet::from(&[MethodKind::Password][..]),
            auth_rejection_time: Duration::from_millis(10),
            auth_rejection_time_initial: Some(Duration::ZERO),
            inactivity_timeout: None,
            ..Default::default()
        });
        let state = Arc::new(ServerState::default());
        let accept = crate::runtime::spawn(accept_loop(listener, config, state.clone()));
        Self {
            port,
            state,
            accept,
        }
    }

    /// 断开全部连接，模拟网络中断。
    pub fn drop_all(&self) {
        for socket in self.state.sockets.lock().unwrap().drain(..) {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
    }

    /// 之后的连接一律在握手前断开（重连失败，可以重试）。
    pub fn set_refuse(&self, refuse: bool) {
        self.state.refuse.store(refuse, Ordering::SeqCst);
    }

    /// 之后的认证一律失败（例如密码被修改）。
    pub fn set_reject_auth(&self, reject: bool) {
        self.state.reject_auth.store(reject, Ordering::SeqCst);
    }

    /// 拒绝这个端口的远程转发（模拟端口已被占用、sshd 禁止转发）。
    pub fn deny_remote(&self, port: u16) {
        self.state
            .deny_remote
            .lock()
            .unwrap()
            .insert(u32::from(port));
    }

    pub fn logins(&self) -> usize {
        self.state.logins.load(Ordering::SeqCst)
    }

    pub fn sessions(&self) -> usize {
        self.state.sessions.load(Ordering::SeqCst)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.accept.abort();
        self.drop_all();
    }
}

async fn accept_loop(
    listener: TcpListener,
    config: Arc<russh::server::Config>,
    state: Arc<ServerState>,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        if state.refuse.load(Ordering::SeqCst) {
            drop(stream);
            continue;
        }
        let std = stream.into_std().unwrap();
        state.sockets.lock().unwrap().push(std.try_clone().unwrap());
        let stream = TcpStream::from_std(std).unwrap();
        let handler = ServerHandler {
            state: state.clone(),
            forwards: Arc::default(),
        };
        let forwards = handler.forwards.clone();
        let config = config.clone();
        let state = state.clone();
        crate::runtime::spawn(async move {
            state.sessions.fetch_add(1, Ordering::SeqCst);
            if let Ok(session) = run_stream(config, stream, handler).await {
                let _ = session.await;
            }
            // 连接断开：关闭它在服务器上的监听
            for (_, task) in forwards.lock().unwrap().drain() {
                task.abort();
            }
            state.sessions.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

struct ServerHandler {
    state: Arc<ServerState>,
    /// 远程转发的监听（按地址与端口）
    forwards: Arc<Mutex<HashMap<(String, u32), JoinHandle<()>>>>,
}

impl Handler for ServerHandler {
    type Error = russh::Error;

    async fn auth_password(&mut self, _user: &str, password: &str) -> Result<Auth, Self::Error> {
        if !self.state.reject_auth.load(Ordering::SeqCst) && password == PASSWORD {
            self.state.logins.fetch_add(1, Ordering::SeqCst);
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

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
        match TcpStream::connect((host_to_connect, port)).await {
            Ok(target) => {
                reply.accept().await;
                crate::runtime::spawn(relay(channel.into_stream(), target));
            }
            Err(_) => reply.reject(ChannelOpenFailure::ConnectFailed).await,
        }
        Ok(())
    }

    async fn tcpip_forward(
        &mut self,
        address: &str,
        port: &mut u32,
        session: &mut Session,
    ) -> Result<bool, Self::Error> {
        if self.state.deny_remote.lock().unwrap().contains(port) {
            return Ok(false);
        }
        let Ok(listener) = TcpListener::bind((address, *port as u16)).await else {
            return Ok(false);
        };
        let handle = session.handle();
        let (bound, bound_port) = (address.to_string(), *port);
        let task = crate::runtime::spawn(async move {
            loop {
                let Ok((stream, origin)) = listener.accept().await else {
                    return;
                };
                let handle = handle.clone();
                let bound = bound.clone();
                crate::runtime::spawn(async move {
                    if let Ok(channel) = handle
                        .channel_open_forwarded_tcpip(
                            bound,
                            bound_port,
                            origin.ip().to_string(),
                            u32::from(origin.port()),
                        )
                        .await
                    {
                        relay(channel.into_stream(), stream).await;
                    }
                });
            }
        });
        self.forwards
            .lock()
            .unwrap()
            .insert((address.to_string(), *port), task);
        Ok(true)
    }

    async fn cancel_tcpip_forward(
        &mut self,
        address: &str,
        port: u32,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        if let Some(task) = self
            .forwards
            .lock()
            .unwrap()
            .remove(&(address.to_string(), port))
        {
            task.abort();
        }
        Ok(true)
    }
}

/// 双向复制，读到 EOF 时半关闭另一端。
async fn relay<A, B>(mut a: A, mut b: B)
where
    A: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    B: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
}
