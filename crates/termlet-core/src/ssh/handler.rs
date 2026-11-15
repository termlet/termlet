//! russh 的客户端回调：核对主机指纹（未知时询问用户、变化时拒绝；信任的 CA 签发的主机证书直接接受）；转发 ssh-agent；
//! 把远程转发中服务器转来的连接交给登记的规则；接受请求过 X11 转发的连接上的 x11 通道；连接断开时通知等待的一方。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use russh::client::{ChannelOpenHandle, DisconnectReason, Handler, Msg, Session};
use russh::keys::PublicKeyOrCertificate;
use russh::{Channel, ChannelOpenFailure};
use tokio::sync::{mpsc, watch};

use crate::ssh::known_hosts::{HostKeyStatus, KnownHosts, fingerprint};
use crate::ssh::prompt::{Answer, HostKeyPrompt, Prompt, Prompter};
use crate::ssh::x11::X11Forwarding;
use crate::ssh::{SshError, host_ca};
use crate::store::Store;

pub(crate) struct ClientHandler {
    pub store: Arc<Store>,
    /// 核对指纹用的地址（这一跳自己的地址，不是跳板机的）
    pub host: String,
    pub port: u16,
    pub prompter: Prompter,
    /// 回调拒绝连接时记下原因（russh 只报"未知密钥"）
    pub failure: Arc<Mutex<Option<SshError>>>,
    pub agent: Arc<AgentForwarding>,
    pub remote_forwards: Arc<RemoteForwards>,
    /// X11 转发：终端请求过才接受服务器打开的 x11 通道
    pub x11: Arc<X11Forwarding>,
    /// 连接断开时置为真
    pub closed: watch::Sender<bool>,
}

/// 转发 ssh-agent：只有这条连接上的终端请求过转发，才接受服务器打开的 agent 通道。
pub(crate) struct AgentForwarding {
    pub enabled: AtomicBool,
    /// 本机 agent 的地址（留空自动查找）
    pub path: Option<String>,
}

/// 远程转发（tcpip-forward）的登记：服务器上监听到的连接经 forwarded-tcpip 通道送来，
/// 按监听的地址与端口交给登记的一方（端口转发的规则）；没有登记的一律拒绝。
#[derive(Default)]
pub(crate) struct RemoteForwards {
    next: AtomicU64,
    routes: Mutex<Vec<Route>>,
}

struct Route {
    id: u64,
    address: String,
    port: u32,
    sender: mpsc::UnboundedSender<ForwardedChannel>,
}

/// 服务器转来的一个连接。
pub(crate) struct ForwardedChannel {
    pub channel: Channel<Msg>,
    /// 服务器上连进来的一方
    pub originator: String,
    pub originator_port: u32,
}

impl RemoteForwards {
    /// 登记 `address:port` 上的监听（在请求服务器监听之前），返回登记号与接收转来的连接的一端。
    pub(crate) fn register(
        &self,
        address: &str,
        port: u32,
    ) -> (u64, mpsc::UnboundedReceiver<ForwardedChannel>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::unbounded_channel();
        self.routes.lock().unwrap().push(Route {
            id,
            address: address.to_string(),
            port,
            sender,
        });
        (id, receiver)
    }

    pub(crate) fn unregister(&self, id: u64) {
        self.routes.lock().unwrap().retain(|r| r.id != id);
    }

    /// 请求时端口写的是 0（由服务器选）：服务器回复了实际的端口后改成它，转来的连接按它交付。
    pub(crate) fn set_port(&self, id: u64, port: u32) {
        if let Some(route) = self.routes.lock().unwrap().iter_mut().find(|r| r.id == id) {
            route.port = port;
        }
    }

    /// 转来的连接交给谁：先按地址与端口；服务器报告的地址与请求时的写法不同（例如 `localhost`、`::`）时，
    /// 这个端口上只有一处登记就交给它。
    fn route(&self, address: &str, port: u32) -> Option<mpsc::UnboundedSender<ForwardedChannel>> {
        let routes = self.routes.lock().unwrap();
        let live = || {
            routes
                .iter()
                .filter(|r| r.port == port && !r.sender.is_closed())
        };
        if let Some(route) = live().find(|r| r.address == address) {
            return Some(route.sender.clone());
        }
        let mut same_port = live();
        match (same_port.next(), same_port.next()) {
            (Some(route), None) => Some(route.sender.clone()),
            _ => None,
        }
    }
}

impl ClientHandler {
    fn fail(&self, error: SshError) {
        *self.failure.lock().unwrap() = Some(error);
    }
}

impl Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        offered: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        // 主机证书（只在这台主机适用某个信任的 CA 时才请求，见 host_ca）：由信任的 CA 签发且有效时直接接受；
        // 否则与 OpenSSH 相同，按证书里的主机密钥核对指纹
        let inner;
        let key = match offered {
            PublicKeyOrCertificate::PublicKey { key, .. } => key,
            PublicKeyOrCertificate::Certificate(certificate) => {
                let trusted = host_ca::trusted_in(&self.store, &self.host, self.port);
                match host_ca::check(
                    certificate,
                    &self.host,
                    &trusted,
                    crate::sshcert::now_secs(),
                ) {
                    Ok(ca) => {
                        tracing::info!(
                            host = %self.host,
                            port = self.port,
                            ca = %ca,
                            certificate = %crate::sshcert::summary(certificate),
                            "主机证书由信任的 CA 签发，接受"
                        );
                        return Ok(true);
                    }
                    Err(issue) => {
                        tracing::info!(
                            host = %self.host,
                            port = self.port,
                            certificate = %crate::sshcert::summary(certificate),
                            "主机证书没有被接受（{issue}），改为核对其中的主机密钥"
                        );
                        inner = russh::keys::PublicKey::new(certificate.public_key().clone(), "");
                        &inner
                    }
                }
            }
        };
        let known = KnownHosts::new(self.store.database());
        let status = match known.check(&self.host, self.port, key) {
            Ok(status) => status,
            Err(e) => {
                self.fail(SshError::Vault(e.to_string()));
                return Ok(false);
            }
        };
        tracing::debug!(
            host = %self.host,
            port = self.port,
            algorithm = %key.algorithm().as_str(),
            fingerprint = %fingerprint(key),
            status = ?status,
            "核对主机指纹"
        );
        match status {
            HostKeyStatus::Known => Ok(true),
            HostKeyStatus::Changed { known_fingerprint } => {
                tracing::warn!(
                    host = %self.host,
                    port = self.port,
                    fingerprint = %fingerprint(key),
                    known = %known_fingerprint,
                    "主机指纹与记录的不同，拒绝连接"
                );
                self.fail(SshError::HostKeyChanged {
                    host: self.host.clone(),
                    port: self.port,
                    algorithm: key.algorithm().as_str().to_string(),
                    fingerprint: fingerprint(key),
                    known_fingerprint,
                });
                Ok(false)
            }
            HostKeyStatus::Unknown => {
                // 连接时已把记录过的算法排在最前面请求，服务器仍然给了别的类型：它不再有记录的那把密钥
                let known_algorithms = known.algorithms(&self.host, self.port).unwrap_or_default();
                let answer = (self.prompter)(Prompt::HostKey(HostKeyPrompt {
                    host: self.host.clone(),
                    port: self.port,
                    algorithm: key.algorithm().as_str().to_string(),
                    fingerprint: fingerprint(key),
                    known_algorithms,
                }))
                .await;
                match answer {
                    Answer::AcceptHostKey => {
                        if let Err(e) = known.remember(&self.host, self.port, key) {
                            tracing::warn!("记录主机指纹失败：{e}");
                        }
                        Ok(true)
                    }
                    _ => {
                        tracing::info!(host = %self.host, port = self.port, "用户没有接受主机指纹");
                        self.fail(SshError::HostKeyRejected {
                            host: self.host.clone(),
                        });
                        Ok(false)
                    }
                }
            }
        }
    }

    async fn server_channel_open_agent_forward(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if !self.agent.enabled.load(Ordering::SeqCst) {
            tracing::warn!(host = %self.host, "服务器打开了 agent 通道，但没有请求转发，拒绝");
            reply
                .reject(ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            return Ok(());
        }
        reply.accept().await;
        let path = self.agent.path.clone();
        crate::runtime::spawn(async move {
            let agent = match crate::ssh::auth::connect_agent(path.as_deref()).await {
                Ok(agent) => agent,
                Err(e) => {
                    tracing::warn!("转发 ssh-agent 失败：{e}");
                    return;
                }
            };
            let mut local = agent.into_inner();
            let mut remote = channel.into_stream();
            if let Err(e) = tokio::io::copy_bidirectional(&mut local, &mut remote).await {
                tracing::debug!("agent 转发通道结束：{e}");
            }
        });
        Ok(())
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<Msg>,
        connected_address: &str,
        connected_port: u32,
        originator_address: &str,
        originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Some(sender) = self
            .remote_forwards
            .route(connected_address, connected_port)
        else {
            tracing::warn!(
                host = %self.host,
                address = connected_address,
                port = connected_port,
                "服务器转来了没有登记的远程转发连接，拒绝"
            );
            reply
                .reject(ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            return Ok(());
        };
        reply.accept().await;
        let forwarded = ForwardedChannel {
            channel,
            originator: originator_address.to_string(),
            originator_port,
        };
        // 规则刚好停止：关闭通道，服务器上连进来的一方随之断开
        if let Err(mpsc::error::SendError(forwarded)) = sender.send(forwarded) {
            let _ = forwarded.channel.close().await;
        }
        Ok(())
    }

    async fn server_channel_open_x11(
        &mut self,
        channel: Channel<Msg>,
        originator_address: &str,
        originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if !self.x11.is_enabled() {
            tracing::warn!(host = %self.host, "服务器打开了 x11 通道，但没有请求转发，拒绝");
            reply
                .reject(ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            return Ok(());
        }
        reply.accept().await;
        tracing::debug!(
            host = %self.host,
            originator = originator_address,
            originator_port,
            "服务器打开了 x11 通道"
        );
        self.x11
            .serve(channel, format!("{originator_address}:{originator_port}"));
        Ok(())
    }

    async fn disconnected(
        &mut self,
        reason: DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        self.closed.send_replace(true);
        match &reason {
            DisconnectReason::ReceivedDisconnect(info) => tracing::info!(
                host = %self.host,
                port = self.port,
                reason = ?info.reason_code,
                message = %info.message,
                "服务器断开了 SSH 连接"
            ),
            DisconnectReason::Error(e) => {
                tracing::info!(host = %self.host, port = self.port, "SSH 连接断开：{e}")
            }
        }
        // 与 russh 的默认处理相同
        match reason {
            DisconnectReason::ReceivedDisconnect(_) => Ok(()),
            DisconnectReason::Error(e) => Err(e),
        }
    }
}
