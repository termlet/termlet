//! 通过本机代理：用 SSH 远程转发把本机的 HTTP 代理借给服务器。服务器连不上外网时装包、拉镜像用。
//!
//! - 服务器上只监听回环地址 `127.0.0.1`（端口由服务器选，或用设置中指定的；指定的被占用时改由服务器选），
//!   服务器上每个接入的连接经 SSH 转回本机，再连接本机代理（见 [`ProxyAddress`]）。转的是原始字节，
//!   服务器上的程序照常按 HTTP 代理（`http_proxy` 等，见 [`super::proxy_snippets`]）使用它。
//! - 属于一个终端的连接：不存进存储、不重连，连接断开时随之停止（[`ShareStatus::stopped`]）；
//!   丢弃 [`ProxyShare`] 或调用 [`ProxyShare::stop`] 时取消服务器上的监听。
//! - 服务器禁止远程转发（sshd 的 `AllowTcpForwarding no`、`DisableForwarding`、`PermitListen none` 等）时
//!   启动返回 [`ShareError::Denied`]。
//!
//! 服务器上的其他用户也能连这个回环端口、借用本机代理；界面中说明这一点。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use super::TargetError;
use super::pipe::pipe;
use crate::model::join_host_port;
use crate::ssh::{Connection, ForwardedChannel, dial};

/// 服务器上监听的地址（只监听回环地址）。
pub const SHARE_BIND: &str = "127.0.0.1";
/// 连接本机代理的超时（写成 localhost 时 IPv6、IPv4 并发连接，见 crate::ssh::dial）。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 取消服务器上的监听最多等多久。
const CANCEL_TIMEOUT: Duration = Duration::from_secs(5);

/// 本机代理的地址。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyAddress {
    pub host: String,
    pub port: u16,
}

impl ProxyAddress {
    /// 解析 `127.0.0.1:7890`、`localhost:7890`、`[::1]:7890`、`http://127.0.0.1:7890/` 等写法。
    /// 只认 HTTP 代理（没有协议或 `http://`）；带用户名密码、别的协议（`socks5://`、`https://`）、没有端口的不认。
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let rest = match text.split_once("://") {
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => rest,
            Some(_) => return None,
            None => text,
        };
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        if rest.is_empty()
            || rest.contains(['@', '/', '?', '#'])
            || rest.contains(char::is_whitespace)
        {
            return None;
        }
        let (host, port) = if let Some(inner) = rest.strip_prefix('[') {
            let (host, port) = inner.split_once("]:")?;
            (host, port)
        } else {
            let (host, port) = rest.rsplit_once(':')?;
            if host.contains(':') {
                return None;
            }
            (host, port)
        };
        let port: u16 = port.parse().ok().filter(|p| *p != 0)?;
        if host.is_empty() {
            return None;
        }
        Some(Self {
            host: host.to_string(),
            port,
        })
    }

    /// `主机:端口`（IPv6 加方括号）。
    pub fn display(&self) -> String {
        join_host_port(&self.host, self.port)
    }
}

impl std::fmt::Display for ProxyAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display())
    }
}

/// 启动失败的原因。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ShareError {
    /// 服务器拒绝监听（sshd 禁止了远程转发，或者是不允许转发的堡垒机）
    #[error("服务器不允许远程转发（sshd 的 AllowTcpForwarding、PermitListen 等）")]
    Denied,
    /// 连接已断开
    #[error("SSH 连接已断开")]
    Disconnected,
    /// 其他错误
    #[error("{0}")]
    Other(String),
}

/// 为什么停止了。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// 调用了 [`ProxyShare::stop`]（或丢弃了它）
    Stopped,
    /// SSH 连接断开了
    ConnectionClosed,
}

/// 状态与统计（有变化时经 [`ProxyShare::watch`] 推送）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShareStatus {
    /// 当前经它的连接数
    pub connections: usize,
    /// 启动以来接入的连接总数
    pub accepted: u64,
    /// 最近一次连不上本机代理的原因（例如本机的代理软件没有运行）
    pub last_error: Option<TargetError>,
    /// 已停止时的原因
    pub stopped: Option<StopReason>,
}

/// 一次正在进行的“通过本机代理”。丢弃时停止。
pub struct ProxyShare {
    shared: Arc<Shared>,
}

struct Shared {
    connection: Arc<Connection>,
    local: ProxyAddress,
    port: u16,
    /// 设置中指定的端口被占用、改由服务器选了端口时，原来指定的端口
    busy_port: Option<u16>,
    route: u64,
    status: watch::Sender<ShareStatus>,
    stop: watch::Sender<bool>,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
}

impl ProxyShare {
    /// 在 `connection` 上开始：请服务器在 `127.0.0.1:port` 监听（`port` 为 0 时由服务器选），转到本机代理 `local`。
    /// 指定的端口被占用（服务器拒绝，而让它自己选时可以）时改由服务器选，[`ProxyShare::busy_port`] 记下原来的端口。
    /// 要在核心的运行时上执行（`crate::runtime::spawn`）。
    pub async fn start(
        connection: Arc<Connection>,
        local: ProxyAddress,
        port: u16,
    ) -> Result<Self, ShareError> {
        let (route, receiver) = connection
            .remote_forwards()
            .register(SHARE_BIND, u32::from(port));
        let (assigned, busy_port) = match listen(&connection, port).await {
            Ok(assigned) => (assigned, None),
            Err(ShareError::Denied) if port != 0 => match listen(&connection, 0).await {
                Ok(assigned) => {
                    tracing::info!(port, assigned, "服务器上指定的端口不能监听，改由服务器选");
                    (assigned, Some(port))
                }
                Err(error) => {
                    connection.remote_forwards().unregister(route);
                    return Err(error);
                }
            },
            Err(error) => {
                connection.remote_forwards().unregister(route);
                return Err(error);
            }
        };
        connection
            .remote_forwards()
            .set_port(route, u32::from(assigned));
        let shared = Arc::new(Shared {
            connection,
            local,
            port: assigned,
            busy_port,
            route,
            status: watch::channel(ShareStatus::default()).0,
            stop: watch::channel(false).0,
            bytes_in: AtomicU64::new(0),
            bytes_out: AtomicU64::new(0),
        });
        tracing::info!(
            port = assigned,
            local = %shared.local,
            host = %shared.connection.label(),
            "通过本机代理已开始"
        );
        crate::runtime::spawn(shared.clone().accept(receiver));
        crate::runtime::spawn(shared.clone().watch_connection());
        Ok(Self { shared })
    }

    /// 服务器上监听的端口（在 [`SHARE_BIND`] 上）。
    pub fn port(&self) -> u16 {
        self.shared.port
    }

    /// 设置中指定、但服务器上不能监听而没有用的端口。
    pub fn busy_port(&self) -> Option<u16> {
        self.shared.busy_port
    }

    /// 本机代理的地址。
    pub fn local(&self) -> &ProxyAddress {
        &self.shared.local
    }

    /// 服务器上用的代理地址：`http://127.0.0.1:端口`。
    pub fn proxy_url(&self) -> String {
        super::snippets::proxy_url(self.shared.port)
    }

    /// 订阅状态与统计。
    pub fn watch(&self) -> watch::Receiver<ShareStatus> {
        self.shared.status.subscribe()
    }

    pub fn status(&self) -> ShareStatus {
        self.shared.status.borrow().clone()
    }

    /// 经本机代理收到（发给服务器）与发出的字节数。
    pub fn bytes(&self) -> (u64, u64) {
        (
            self.shared.bytes_in.load(Ordering::Relaxed),
            self.shared.bytes_out.load(Ordering::Relaxed),
        )
    }

    /// 停止：关闭经它的连接，取消服务器上的监听（连接还在时，最多等 5 秒）。可以重复调用。
    pub async fn stop(&self) {
        self.shared.finish(StopReason::Stopped).await;
    }
}

impl Drop for ProxyShare {
    fn drop(&mut self) {
        if self.shared.status.borrow().stopped.is_some() {
            return;
        }
        let shared = self.shared.clone();
        crate::runtime::spawn(async move { shared.finish(StopReason::Stopped).await });
    }
}

/// 请服务器在 [`SHARE_BIND`] 的 `port` 上监听，返回实际的端口。
async fn listen(connection: &Connection, port: u16) -> Result<u16, ShareError> {
    match connection
        .handle()
        .tcpip_forward(SHARE_BIND, u32::from(port))
        .await
    {
        // 请求了指定端口时服务器不回端口（得到 0）
        Ok(assigned) => match u16::try_from(assigned) {
            Ok(0) if port != 0 => Ok(port),
            Ok(0) | Err(_) => Err(ShareError::Other(format!(
                "服务器回复的端口不对：{assigned}"
            ))),
            Ok(assigned) => Ok(assigned),
        },
        Err(russh::Error::RequestDenied) => Err(ShareError::Denied),
        Err(_) if connection.is_closed() => Err(ShareError::Disconnected),
        Err(error) => Err(ShareError::Other(error.to_string())),
    }
}

impl Shared {
    fn is_stopped(&self) -> bool {
        *self.stop.borrow()
    }

    async fn stopped(&self) {
        let mut stop = self.stop.subscribe();
        let _ = stop.wait_for(|stopped| *stopped).await;
    }

    /// 停止（只做一次）：记下原因，关闭经它的连接，连接还在时取消服务器上的监听。
    async fn finish(&self, reason: StopReason) {
        let first = self
            .stop
            .send_if_modified(|stopped| !std::mem::replace(stopped, true));
        if !first {
            return;
        }
        self.status.send_modify(|s| {
            s.stopped = Some(reason);
            s.connections = 0;
        });
        self.connection.remote_forwards().unregister(self.route);
        tracing::info!(port = self.port, ?reason, "通过本机代理已停止");
        if self.connection.is_closed() {
            return;
        }
        let cancel = self
            .connection
            .handle()
            .cancel_tcpip_forward(SHARE_BIND, u32::from(self.port));
        if let Ok(Err(error)) = tokio::time::timeout(CANCEL_TIMEOUT, cancel).await {
            tracing::debug!(port = self.port, "取消服务器上的监听失败：{error}");
        }
    }

    /// 连接断开时停止。
    async fn watch_connection(self: Arc<Self>) {
        tokio::select! {
            _ = self.connection.closed() => self.finish(StopReason::ConnectionClosed).await,
            _ = self.stopped() => {}
        }
    }

    /// 接收服务器转来的连接，直到停止。
    async fn accept(self: Arc<Self>, mut forwarded: mpsc::UnboundedReceiver<ForwardedChannel>) {
        loop {
            let next = tokio::select! {
                next = forwarded.recv() => next,
                _ = self.stopped() => None,
            };
            let Some(channel) = next else {
                return;
            };
            crate::runtime::spawn(self.clone().serve(channel));
        }
    }

    /// 一个连接：连接本机代理并双向复制。
    async fn serve(self: Arc<Self>, forwarded: ForwardedChannel) {
        // 先转成流：连不上本机代理时丢弃即关闭通道
        let remote = forwarded.channel.into_stream();
        if self.is_stopped() {
            return;
        }
        self.status.send_modify(|s| {
            s.accepted += 1;
            s.connections += 1;
        });
        let local = tokio::select! {
            result = dial::connect_host(&self.local.host, self.local.port, CONNECT_TIMEOUT) => result,
            _ = self.stopped() => return,
        };
        let target = self.local.display();
        let failure = match local {
            Ok(local) => {
                let _ = local.set_nodelay(true);
                tokio::select! {
                    _ = pipe(remote, local, &self.bytes_out, &self.bytes_in) => {}
                    _ = self.stopped() => {}
                }
                None
            }
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                Some(TargetError::LocalRefused { target })
            }
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                Some(TargetError::Local {
                    target,
                    reason: "超时".into(),
                })
            }
            Err(error) => Some(TargetError::Local {
                target,
                reason: error.to_string(),
            }),
        };
        if let Some(error) = &failure {
            tracing::debug!(port = self.port, "通过本机代理连不上本机代理：{error}");
        }
        self.status.send_modify(|s| {
            s.connections = s.connections.saturating_sub(1);
            if failure.is_some() {
                s.last_error = failure;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_addresses() {
        let parse = |s: &str| ProxyAddress::parse(s).map(|a| (a.host, a.port));
        assert_eq!(parse("127.0.0.1:7890"), Some(("127.0.0.1".into(), 7890)));
        assert_eq!(
            parse(" http://127.0.0.1:7890/ "),
            Some(("127.0.0.1".into(), 7890))
        );
        assert_eq!(
            parse("HTTP://proxy.lan:8080"),
            Some(("proxy.lan".into(), 8080))
        );
        assert_eq!(parse("[::1]:1080"), Some(("::1".into(), 1080)));
        assert_eq!(parse("localhost:3128"), Some(("localhost".into(), 3128)));
        for bad in [
            "",
            "127.0.0.1",
            "127.0.0.1:0",
            "127.0.0.1:70000",
            ":7890",
            "socks5://127.0.0.1:7890",
            "https://127.0.0.1:7890",
            "http://user:pass@127.0.0.1:7890",
            "::1:7890",
            "127.0.0.1:7890/path",
            "a b:1",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
        assert_eq!(
            ProxyAddress::parse("[::1]:1080").unwrap().display(),
            "[::1]:1080"
        );
    }

    /// 假的本机 HTTP 代理：每个连接读到请求头的结尾后，把请求行送出来，回一个 200。
    async fn fake_proxy() -> (u16, mpsc::UnboundedReceiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::unbounded_channel();
        crate::runtime::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let tx = tx.clone();
                crate::runtime::spawn(async move {
                    let mut head = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&head);
                    let _ = tx.send(text.lines().next().unwrap_or_default().to_string());
                    let body = "hello-from-proxy";
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(reply.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (port, rx)
    }

    /// 在服务器上按片段设好环境变量后用 wget 经代理取一个网址。
    async fn fetch_via(connection: &Connection, port: u16) -> crate::ssh::ExecResult {
        let shell = super::super::proxy_snippet(super::super::SnippetKind::Shell, port);
        let command = format!(
            "{}; wget -q -T 10 -O - http://termlet.invalid/hello",
            shell.apply
        );
        crate::ssh::exec(connection, &command, 64 * 1024, Duration::from_secs(30))
            .await
            .unwrap()
    }

    // 连真实的 sshd（TERMLET_TEST_SSH）：服务器上的 wget 经借来的代理取到本机假代理的回复；指定的端口被占用时改由服务器选；
    // 停止后服务器上的端口不再监听；连接断开时随之停止
    #[test]
    fn docker_share_lends_the_local_proxy() {
        let Some((host, port)) = crate::testkit::test_server() else {
            return;
        };
        crate::runtime::runtime().block_on(async {
            let (proxy_port, mut requests) = fake_proxy().await;
            let local = ProxyAddress {
                host: "127.0.0.1".into(),
                port: proxy_port,
            };
            let connection = crate::testkit::connect_test(&host, port).await;
            let share = ProxyShare::start(connection.clone(), local.clone(), 0)
                .await
                .unwrap();
            assert_ne!(share.port(), 0);
            assert_eq!(share.busy_port(), None);
            let result = fetch_via(&connection, share.port()).await;
            assert_eq!(
                String::from_utf8_lossy(&result.stdout),
                "hello-from-proxy",
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert_eq!(
                requests.recv().await.as_deref(),
                Some("GET http://termlet.invalid/hello HTTP/1.1")
            );
            let status = share.status();
            assert_eq!(status.accepted, 1);
            assert_eq!(status.stopped, None);
            assert!(share.bytes().0 > 0);

            // 同一个端口再借一次：被占用，改由服务器选
            let second = ProxyShare::start(connection.clone(), local.clone(), share.port())
                .await
                .unwrap();
            assert_eq!(second.busy_port(), Some(share.port()));
            assert_ne!(second.port(), share.port());
            drop(second);

            // 停止后服务器上的端口不再接受连接
            let used = share.port();
            share.stop().await;
            assert_eq!(share.status().stopped, Some(StopReason::Stopped));
            let after = fetch_via(&connection, used).await;
            assert_ne!(after.exit_code, Some(0));

            // 连接断开时随之停止
            let other = ProxyShare::start(connection.clone(), local, 0)
                .await
                .unwrap();
            let mut watch = other.watch();
            connection.disconnect().await;
            tokio::time::timeout(
                Duration::from_secs(10),
                watch.wait_for(|s| s.stopped == Some(StopReason::ConnectionClosed)),
            )
            .await
            .expect("连接断开后应停止")
            .unwrap();
        });
    }

    // 服务器禁止了远程转发（testing/nosftp 的容器：AllowTcpForwarding no）：说明原因，不留下登记
    #[test]
    fn nosftp_share_denied_by_server() {
        let Some((store, host)) = crate::testkit::nosftp_store("nosftp", crate::testkit::SSH_USER)
        else {
            return;
        };
        crate::runtime::runtime().block_on(async {
            let plan = crate::ssh::plan_for_host(&store, &host).unwrap();
            let connection = crate::ssh::connect(
                store,
                &plan,
                crate::testkit::accept_host_key(),
                &crate::ssh::ConnectOptions::default(),
            )
            .await
            .unwrap();
            let local = ProxyAddress {
                host: "127.0.0.1".into(),
                port: 7890,
            };
            for port in [0, 17890] {
                assert_eq!(
                    ProxyShare::start(connection.clone(), local.clone(), port)
                        .await
                        .err(),
                    Some(ShareError::Denied)
                );
            }
        });
    }
}
