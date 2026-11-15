//! 建立 SSH 连接：逐跳连接、核对指纹、认证；后面的每一跳经前一跳的 `direct-tcpip` 通道连接。
//!
//! 连接建好后按保活间隔探测服务器：超过 [`STALL_AFTER`] 没有回应时报告"没有响应"（会话不断开，
//! 网络恢复后照常），连续约保活间隔 × 3 没有回应才断开。

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

use russh::client::{self, Handle};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{Notify, watch};

use crate::ssh::auth::Authenticator;
use crate::ssh::handler::{AgentForwarding, ClientHandler, RemoteForwards};
use crate::ssh::known_hosts::KnownHosts;
use crate::ssh::plan::{ConnectPlan, Hop};
use crate::ssh::prompt::Prompter;
use crate::ssh::proxy_command;
use crate::ssh::transport::{self, BoxStream};
use crate::ssh::x11::{X11Forwarding, X11Notice};
use crate::ssh::{SshError, algorithms, host_ca};
use crate::store::Store;

/// 保活超过这么久没有回应时提示"连接没有响应"。
pub const STALL_AFTER: Duration = Duration::from_secs(10);
/// 保活连续这么多个间隔没有回应才判定断开。
const KEEPALIVE_MAX: u32 = 3;

/// 连接的选项（来自设置）。
#[derive(Clone, Debug)]
pub struct ConnectOptions {
    /// 保活间隔；`None` 关闭（也就不检测"没有响应"）
    pub keepalive: Option<Duration>,
    /// ssh-agent 的地址（留空自动查找）
    pub agent_path: Option<String>,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            keepalive: Some(Duration::from_secs(30)),
            agent_path: None,
        }
    }
}

/// 连接是否有响应。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    Responsive,
    /// 保活超过 [`STALL_AFTER`] 没有回应，还没有判定断开
    Stalled,
}

/// 一条已认证的 SSH 连接。可以在上面开多个通道（终端、SFTP、端口转发、执行命令）。
pub struct Connection {
    handle: Handle<ClientHandler>,
    /// 显示用的名称（目标主机）
    label: String,
    /// 服务器的版本串（`SSH-2.0-OpenSSH_9.6`）
    server_version: String,
    agent: Arc<AgentForwarding>,
    health: watch::Receiver<Health>,
    health_tx: watch::Sender<Health>,
    /// 让保活立即探测一次（系统从睡眠中唤醒时）
    probe: Arc<Notify>,
    /// 远程转发的登记（与回调共用）
    remote_forwards: Arc<RemoteForwards>,
    /// X11 转发（与回调共用）
    x11: Arc<X11Forwarding>,
    /// 连接断开时变为真（回调的 `disconnected` 设置）
    closed: watch::Receiver<bool>,
    /// 用证书登录时证书的失效时间（Unix 秒；永不过期的为 `None`）
    certificate_expiry: Option<i64>,
    /// 经过的跳板机连接（最后一跳的前一跳，它再带着它之前的）：随这条连接一起保持
    jumps: Option<Arc<Connection>>,
}

impl Connection {
    pub(crate) fn handle(&self) -> &Handle<ClientHandler> {
        &self.handle
    }

    pub(crate) fn remote_forwards(&self) -> &Arc<RemoteForwards> {
        &self.remote_forwards
    }

    pub(crate) fn x11(&self) -> &Arc<X11Forwarding> {
        &self.x11
    }

    /// X11 转发进行中的说明（连不上本机的 X 服务器等）；订阅之后发生的才收到，每条带一个递增的序号。
    pub fn x11_notices(&self) -> watch::Receiver<Option<(u64, X11Notice)>> {
        self.x11.notices()
    }

    /// 等到连接断开（服务器断开、网络中断、保活判定断开或本机主动断开）。
    pub async fn closed(&self) {
        let mut closed = self.closed.clone();
        if self.is_closed() {
            return;
        }
        // 回调被丢弃（发送端关闭）也算断开
        let _ = closed.wait_for(|closed| *closed).await;
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn server_version(&self) -> &str {
        &self.server_version
    }

    /// 用证书登录（目标主机）时证书的失效时间（Unix 秒）；没用证书或证书永不过期时为 `None`。
    /// 快过期时界面在终端中提醒（见 [`crate::sshcert::expires_soon`]）。
    pub fn certificate_expiry(&self) -> Option<i64> {
        self.certificate_expiry
    }

    /// 经过的跳板机中用证书登录、证书会过期的：（跳板机显示用的名称，证书的失效时间），从第一跳起。跳板机的证书
    /// 快过期时界面同样提醒，写明是哪一跳。
    pub fn jump_certificate_expiries(&self) -> Vec<(String, i64)> {
        let mut expiries = Vec::new();
        let mut jump = self.jumps.as_deref();
        while let Some(connection) = jump {
            if let Some(expiry) = connection.certificate_expiry {
                expiries.push((connection.label.clone(), expiry));
            }
            jump = connection.jumps.as_deref();
        }
        expiries.reverse();
        expiries
    }

    pub fn is_closed(&self) -> bool {
        self.handle.is_closed()
    }

    /// 连接是否有响应；变化时接收端收到通知。
    pub fn health(&self) -> watch::Receiver<Health> {
        self.health.clone()
    }

    /// 立即探测一次连接（不等下一个保活间隔），用于系统从睡眠中唤醒后尽快发现断线。
    pub fn probe(&self) {
        self.probe.notify_one();
    }

    /// 测一次往返时间（SSH keepalive 请求到回应）；超过 [`STALL_AFTER`] 没有回应或连接已断开时为 `None`。
    pub async fn ping(&self) -> Option<Duration> {
        if self.is_closed() {
            return None;
        }
        let started = tokio::time::Instant::now();
        match tokio::time::timeout(STALL_AFTER, self.handle.send_ping()).await {
            Ok(Ok(())) => Some(started.elapsed()),
            _ => None,
        }
    }

    /// 允许服务器在这条连接上打开转发 ssh-agent 的通道（终端请求转发前调用）。
    pub(crate) fn enable_agent_forwarding(&self) {
        self.agent.enabled.store(true, Ordering::SeqCst);
    }

    /// 主动断开。
    pub async fn disconnect(&self) {
        tracing::debug!(host = %self.label, "主动断开 SSH 连接");
        let _ = self
            .handle
            .disconnect(russh::Disconnect::ByApplication, "", "")
            .await;
    }
}

/// 按计划连接到目标主机。
pub async fn connect(
    store: Arc<Store>,
    plan: &ConnectPlan,
    prompter: Prompter,
    options: &ConnectOptions,
) -> Result<Arc<Connection>, SshError> {
    let started = std::time::Instant::now();
    let target = plan
        .hops
        .last()
        .map(|h| h.label.clone())
        .unwrap_or_default();
    tracing::debug!(host = %target, hops = plan.hops.len(), "开始 SSH 连接");
    let result = connect_hops(store, plan, prompter, options).await;
    match &result {
        Ok(connection) => tracing::info!(
            host = %target,
            hops = plan.hops.len(),
            server = %connection.server_version,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "SSH 连接已建立"
        ),
        Err(error) => tracing::warn!(
            host = %target,
            hops = plan.hops.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "SSH 连接失败：{error}"
        ),
    }
    result
}

async fn connect_hops(
    store: Arc<Store>,
    plan: &ConnectPlan,
    prompter: Prompter,
    options: &ConnectOptions,
) -> Result<Arc<Connection>, SshError> {
    let mut previous: Option<Arc<Connection>> = None;
    let count = plan.hops.len();
    for (index, hop) in plan.hops.iter().enumerate() {
        let is_target = index + 1 == count;
        tracing::debug!(
            hop = index + 1,
            of = count,
            host = %hop.address,
            port = hop.port,
            user = %hop.username,
            via_jump = previous.is_some(),
            proxy = hop.proxy.is_some(),
            legacy = hop.legacy_algorithms,
            "连接一跳"
        );
        let (stream, proxy_watch): (BoxStream, _) = match &previous {
            None => {
                if let Some(proxy) = &hop.proxy {
                    let tokens = proxy_command::Tokens {
                        host: &hop.address,
                        port: hop.port,
                        user: &hop.username,
                        alias: &proxy.alias,
                    };
                    proxy_command::ensure_allowed(&store, proxy, &tokens, &hop.label, &prompter)
                        .await?;
                }
                transport::first_hop(&hop.address, hop.port, &hop.username, hop.proxy.as_ref())
                    .await?
            }
            Some(jump) => {
                let channel = jump
                    .handle
                    .channel_open_direct_tcpip(hop.address.clone(), hop.port as u32, "127.0.0.1", 0)
                    .await
                    .map_err(|e| SshError::JumpTarget {
                        host: hop.address.clone(),
                        port: hop.port,
                        reason: e.to_string(),
                    })?;
                (Box::new(channel.into_stream()), None)
            }
        };
        let connected = connect_hop(
            store.clone(),
            hop,
            stream,
            prompter.clone(),
            options,
            previous.take(),
        )
        .await;
        // 经代理命令时连接断开：命令自己退出了的，说明退出码与标准错误
        let connected = match (connected, proxy_watch) {
            (Err(error), Some(watch)) => Err(watch.explain(error).await),
            (connected, _) => connected,
        };
        let connection = connected.map_err(|error| match (is_target, error) {
            // 跳板机本身连不上时说明是哪一台
            (
                false,
                error @ (SshError::HostKeyRejected { .. }
                | SshError::HostKeyChanged { .. }
                | SshError::AuthCancelled),
            ) => error,
            (false, error) => SshError::Jump {
                jump: hop.label.clone(),
                reason: error.to_string(),
            },
            (true, error) => error,
        })?;
        previous = Some(Arc::new(connection));
    }
    let connection = previous.ok_or_else(|| SshError::Config("连接计划为空".into()))?;
    if let Some(interval) = options.keepalive {
        spawn_health_monitor(&connection, interval);
    }
    Ok(connection)
}

async fn connect_hop(
    store: Arc<Store>,
    hop: &Hop,
    stream: BoxStream,
    prompter: Prompter,
    options: &ConnectOptions,
    jump: Option<Arc<Connection>>,
) -> Result<Connection, SshError> {
    let known = KnownHosts::new(store.database())
        .algorithms(&hop.address, hop.port)
        .unwrap_or_default();
    let mut preferred = algorithms::preferred(hop.legacy_algorithms, &known);
    // 这台主机适用某个信任的主机 CA 时才请求主机证书（见 host_ca）
    let host_certificates = !host_ca::trusted_in(&store, &hop.address, hop.port).is_empty();
    if host_certificates {
        preferred = algorithms::with_host_certificates(preferred);
    }
    let config = client::Config {
        preferred,
        // russh 自己的保活是后备：探测（见 spawn_health_monitor）只在目标连接上进行，跳板机靠这个发现断线
        keepalive_interval: options.keepalive,
        keepalive_max: KEEPALIVE_MAX as usize,
        inactivity_timeout: None,
        nodelay: true,
        ..Default::default()
    };
    let failure = Arc::new(Mutex::new(None));
    let agent = Arc::new(AgentForwarding {
        enabled: AtomicBool::new(false),
        path: options.agent_path.clone(),
    });
    let remote_forwards = Arc::new(RemoteForwards::default());
    let x11 = Arc::new(X11Forwarding::new(&hop.label));
    let (closed_tx, closed) = watch::channel(false);
    let handler = ClientHandler {
        store,
        host: hop.address.clone(),
        port: hop.port,
        prompter: prompter.clone(),
        failure: failure.clone(),
        agent: agent.clone(),
        remote_forwards: remote_forwards.clone(),
        x11: x11.clone(),
        closed: closed_tx,
    };
    let version = Arc::new(Mutex::new(VersionLine::default()));
    let stream = VersionSniffer {
        inner: stream,
        line: version.clone(),
    };
    tracing::debug!(
        host = %hop.address,
        port = hop.port,
        known_algorithms = ?known,
        host_certificates,
        "SSH 握手"
    );
    let mut handle = match client::connect_stream(Arc::new(config), stream, handler).await {
        Ok(handle) => handle,
        Err(error) => {
            // 指纹回调拒绝时 russh 只报"未知密钥"，具体原因记在 failure 中
            if let Some(reason) = failure.lock().unwrap().take() {
                tracing::debug!(host = %hop.address, port = hop.port, "握手被拒绝：{reason}");
                return Err(reason);
            }
            tracing::debug!(host = %hop.address, port = hop.port, "握手失败：{error}");
            return Err(error.into());
        }
    };
    tracing::debug!(host = %hop.address, port = hop.port, "握手完成，开始认证");
    let certificate =
        Authenticator::new(&mut handle, hop, &prompter, options.agent_path.as_deref())
            .run()
            .await?;
    let certificate_expiry = certificate
        .as_ref()
        .and_then(|c| crate::sshcert::CertInfo::of(c).valid_before);
    let server_version = version.lock().unwrap().version.clone().unwrap_or_default();
    tracing::info!(
        host = %hop.address,
        port = hop.port,
        user = %hop.username,
        server = %server_version,
        certificate = certificate.is_some(),
        "SSH 已认证"
    );
    let (health_tx, health) = watch::channel(Health::Responsive);
    Ok(Connection {
        handle,
        label: hop.label.clone(),
        server_version,
        agent,
        health,
        health_tx,
        probe: Arc::new(Notify::new()),
        remote_forwards,
        x11,
        closed,
        certificate_expiry,
        jumps: jump,
    })
}

/// 按保活间隔探测连接，报告"没有响应 / 已恢复"；连续约保活间隔 × 3 没有回应时断开。
/// 只持有弱引用：连接不再被使用时随之结束。
fn spawn_health_monitor(connection: &Arc<Connection>, interval: Duration) {
    let weak: Weak<Connection> = Arc::downgrade(connection);
    let probe = connection.probe.clone();
    crate::runtime::spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = probe.notified() => {}
            }
            let Some(connection) = weak.upgrade() else {
                return;
            };
            if connection.is_closed() {
                return;
            }
            let ping = connection.handle.send_ping();
            tokio::pin!(ping);
            if tokio::time::timeout(STALL_AFTER, &mut ping).await.is_ok() {
                continue;
            }
            tracing::info!(host = %connection.label, "连接没有响应");
            connection.health_tx.send_replace(Health::Stalled);
            let give_up = (interval * KEEPALIVE_MAX).saturating_sub(STALL_AFTER);
            if tokio::time::timeout(give_up, &mut ping).await.is_err() {
                tracing::warn!(host = %connection.label, "连接长时间没有响应，断开");
                connection.disconnect().await;
                return;
            }
            tracing::info!(host = %connection.label, "连接已恢复");
            connection.health_tx.send_replace(Health::Responsive);
        }
    });
}

/// 服务器在协议开始时发来的版本行（之前可能还有几行说明文字）。
#[derive(Default)]
struct VersionLine {
    buffer: Vec<u8>,
    version: Option<String>,
    done: bool,
}

impl VersionLine {
    /// 最多看这么多字节（RFC 4253 允许版本行之前有其他行）
    const LIMIT: usize = 8 * 1024;

    fn feed(&mut self, data: &[u8]) {
        for &byte in data {
            if self.done {
                return;
            }
            if byte == b'\n' {
                let line = String::from_utf8_lossy(&self.buffer)
                    .trim_end_matches('\r')
                    .to_string();
                self.buffer.clear();
                if line.starts_with("SSH-") {
                    self.version = Some(line);
                    self.done = true;
                }
            } else {
                self.buffer.push(byte);
            }
            if self.buffer.len() > Self::LIMIT {
                self.done = true;
            }
        }
    }
}

/// 读取方向上记下服务器的版本行，其余原样转发。
struct VersionSniffer {
    inner: BoxStream,
    line: Arc<Mutex<VersionLine>>,
}

impl AsyncRead for VersionSniffer {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = result {
            let mut line = self.line.lock().unwrap();
            if !line.done {
                line.feed(&buf.filled()[before..]);
            }
        }
        result
    }
}

impl AsyncWrite for VersionSniffer {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

#[cfg(test)]
mod tests {
    use super::VersionLine;

    #[test]
    fn version_line_skips_banner_lines() {
        let mut line = VersionLine::default();
        line.feed(b"Welcome\r\nSSH-2.0-Open");
        assert_eq!(line.version, None);
        line.feed(b"SSH_9.6\r\n\x00\x00binary");
        assert_eq!(line.version.as_deref(), Some("SSH-2.0-OpenSSH_9.6"));
        assert!(line.done);
    }
}
