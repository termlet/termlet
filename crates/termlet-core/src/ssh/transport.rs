//! 建立到第一跳的字节流：直连 TCP、SOCKS5 / SOCKS4 / 4a、HTTP / HTTPS 代理、代理命令（见 [`proxy_command`]）。
//! 后面的各跳由前一跳的 `direct-tcpip` 通道承载（见 connection.rs）。

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use zeroize::Zeroizing;

use crate::model::ProxyKind;
use crate::ssh::plan::ProxyPlan;
use crate::ssh::proxy_command::{self, ProxyWatch};
use crate::ssh::{SshError, dial};

/// 任意的双向字节流。
pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

pub type BoxStream = Box<dyn Stream>;

/// TCP 连接的超时（解析出几个地址时是总的时长，见 [`dial::connect_any`]）。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// TCP keepalive 的空闲时间与探测间隔（与 Go `net.Dialer` 的默认值相同）：空闲时断网、对方没有发 RST，
/// 也能在几分钟内判定断线（Telnet 没有自己的保活，SSH 的保活之外多一层）。
const TCP_KEEPALIVE: Duration = Duration::from_secs(15);

/// 直连 TCP（先单独解析域名，区分"解析失败"与"连不上"）。解析出 IPv6、IPv4 几个地址时并发连接
/// （Happy Eyeballs，见 [`dial`]）。连上后关闭 Nagle、开启 TCP keepalive。
pub async fn tcp(host: &str, port: u16) -> Result<TcpStream, SshError> {
    let started = std::time::Instant::now();
    let addresses: Vec<_> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| {
            tracing::debug!(host, port, "域名解析失败：{e}");
            SshError::Dns {
                host: host.into(),
                reason: e.to_string(),
            }
        })?
        .collect();
    tracing::debug!(
        host,
        port,
        addresses = ?addresses,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "域名解析"
    );
    if addresses.is_empty() {
        return Err(SshError::Dns {
            host: host.into(),
            reason: "没有地址".into(),
        });
    }
    let (stream, address) = dial::connect_any(&addresses, CONNECT_TIMEOUT)
        .await
        .map_err(|e| {
            tracing::debug!(host, port, "TCP 连接失败：{e}");
            SshError::from_io(host, port, &e)
        })?;
    let _ = stream.set_nodelay(true);
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(TCP_KEEPALIVE)
        .with_interval(TCP_KEEPALIVE);
    if let Err(e) = socket2::SockRef::from(&stream).set_tcp_keepalive(&keepalive) {
        tracing::debug!("开启 TCP keepalive 失败：{e}");
    }
    tracing::debug!(
        host,
        %address,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "TCP 已连接"
    );
    Ok(stream)
}

/// 到第一跳的流：有代理时经代理，否则直连。经代理命令时另外返回查看命令进程的一端（连接失败时说明原因）；
/// 代理命令要先经 [`proxy_command::ensure_allowed`] 确认过。
pub(crate) async fn first_hop(
    host: &str,
    port: u16,
    username: &str,
    proxy: Option<&ProxyPlan>,
) -> Result<(BoxStream, Option<ProxyWatch>), SshError> {
    let Some(proxy) = proxy else {
        return Ok((Box::new(tcp(host, port).await?), None));
    };
    tracing::debug!(
        host,
        port,
        proxy = ?proxy.kind,
        proxy_address = %proxy.address,
        proxy_port = proxy.port,
        "经代理连接"
    );
    let stream: BoxStream = match proxy.kind {
        ProxyKind::Command => {
            let tokens = proxy_command::Tokens {
                host,
                port,
                user: username,
                alias: &proxy.alias,
            };
            let line = proxy_command::expand(&proxy.command, &tokens)
                .map_err(SshError::ProxyCommandInvalid)?;
            let (stream, watch) = proxy_command::spawn(&line)?;
            return Ok((stream, Some(watch)));
        }
        ProxyKind::Socks5 => {
            let mut stream = proxy_tcp(proxy).await?;
            socks5(&mut stream, proxy, host, port).await?;
            Box::new(stream)
        }
        ProxyKind::Socks4 => {
            let mut stream = proxy_tcp(proxy).await?;
            socks4(&mut stream, proxy, host, port).await?;
            Box::new(stream)
        }
        ProxyKind::Http => {
            let mut stream = proxy_tcp(proxy).await?;
            http_connect(&mut stream, proxy, host, port).await?;
            Box::new(stream)
        }
        ProxyKind::Https => {
            let tcp = proxy_tcp(proxy).await?;
            let mut stream = tls(tcp, &proxy.address).await?;
            http_connect(&mut stream, proxy, host, port).await?;
            Box::new(stream)
        }
    };
    Ok((stream, None))
}

async fn proxy_tcp(proxy: &ProxyPlan) -> Result<TcpStream, SshError> {
    tcp(&proxy.address, proxy.port).await.map_err(|e| {
        SshError::Proxy(format!(
            "连不上代理 {}:{}（{e}）",
            proxy.address, proxy.port
        ))
    })
}

fn proxy_io(e: std::io::Error) -> SshError {
    SshError::Proxy(e.to_string())
}

/// SOCKS5（RFC 1928），支持用户名密码认证（RFC 1929）；域名交给代理解析。
async fn socks5<S: Stream>(
    stream: &mut S,
    proxy: &ProxyPlan,
    host: &str,
    port: u16,
) -> Result<(), SshError> {
    let with_auth = !proxy.username.is_empty();
    let greeting: &[u8] = if with_auth { &[5, 2, 0, 2] } else { &[5, 1, 0] };
    stream.write_all(greeting).await.map_err(proxy_io)?;
    let mut reply = [0u8; 2];
    stream.read_exact(&mut reply).await.map_err(proxy_io)?;
    if reply[0] != 5 {
        return Err(SshError::Proxy("不是 SOCKS5 代理".into()));
    }
    match reply[1] {
        0 => {}
        2 => {
            let user = proxy.username.as_bytes();
            let password = proxy.password.as_ref().map(|p| p.expose()).unwrap_or("");
            if user.len() > 255 || password.len() > 255 {
                return Err(SshError::Proxy("代理的用户名或密码过长".into()));
            }
            let mut auth = Zeroizing::new(vec![1, user.len() as u8]);
            auth.extend_from_slice(user);
            auth.push(password.len() as u8);
            auth.extend_from_slice(password.as_bytes());
            stream.write_all(&auth).await.map_err(proxy_io)?;
            let mut status = [0u8; 2];
            stream.read_exact(&mut status).await.map_err(proxy_io)?;
            if status[1] != 0 {
                return Err(SshError::Proxy("代理的用户名或密码不正确".into()));
            }
        }
        0xff => return Err(SshError::Proxy("代理不接受这种认证方式".into())),
        other => return Err(SshError::Proxy(format!("代理要求不支持的认证方式 {other}"))),
    }
    if host.len() > 255 {
        return Err(SshError::Proxy("地址过长".into()));
    }
    let mut request = vec![5, 1, 0];
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => {
            request.push(1);
            request.extend_from_slice(&ip.octets());
        }
        Ok(std::net::IpAddr::V6(ip)) => {
            request.push(4);
            request.extend_from_slice(&ip.octets());
        }
        Err(_) => {
            request.push(3);
            request.push(host.len() as u8);
            request.extend_from_slice(host.as_bytes());
        }
    }
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).await.map_err(proxy_io)?;
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await.map_err(proxy_io)?;
    if head[1] != 0 {
        let reason = match head[1] {
            1 => "代理服务器故障",
            2 => "代理的规则不允许",
            3 => "网络不可达",
            4 => "主机不可达",
            5 => "目标拒绝连接",
            6 => "超时",
            _ => "代理拒绝了连接",
        };
        return Err(SshError::Proxy(reason.into()));
    }
    // 跳过代理绑定的地址
    let skip = match head[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await.map_err(proxy_io)?;
            len[0] as usize
        }
        _ => return Err(SshError::Proxy("代理的回复无法识别".into())),
    } + 2;
    let mut rest = vec![0u8; skip];
    stream.read_exact(&mut rest).await.map_err(proxy_io)?;
    Ok(())
}

/// SOCKS4（目标是 IPv4 时）或 SOCKS4a（域名交给代理解析）。
async fn socks4<S: Stream>(
    stream: &mut S,
    proxy: &ProxyPlan,
    host: &str,
    port: u16,
) -> Result<(), SshError> {
    let mut request = vec![4, 1];
    request.extend_from_slice(&port.to_be_bytes());
    let ipv4 = host.parse::<std::net::Ipv4Addr>().ok();
    match ipv4 {
        Some(ip) => request.extend_from_slice(&ip.octets()),
        None => request.extend_from_slice(&[0, 0, 0, 1]),
    }
    request.extend_from_slice(proxy.username.as_bytes());
    request.push(0);
    if ipv4.is_none() {
        request.extend_from_slice(host.as_bytes());
        request.push(0);
    }
    stream.write_all(&request).await.map_err(proxy_io)?;
    let mut reply = [0u8; 8];
    stream.read_exact(&mut reply).await.map_err(proxy_io)?;
    if reply[1] != 0x5a {
        return Err(SshError::Proxy("SOCKS4 代理拒绝了连接".into()));
    }
    Ok(())
}

/// HTTP CONNECT（可带 Basic 认证）。
async fn http_connect<S: Stream>(
    stream: &mut S,
    proxy: &ProxyPlan,
    host: &str,
    port: u16,
) -> Result<(), SshError> {
    let authority = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    // 带着代理的密码：用完清零
    let mut request = Zeroizing::new(format!(
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n"
    ));
    if !proxy.username.is_empty() {
        let password = proxy.password.as_ref().map(|p| p.expose()).unwrap_or("");
        let credentials = Zeroizing::new(format!("{}:{password}", proxy.username));
        let token = Zeroizing::new(data_encoding::BASE64.encode(credentials.as_bytes()));
        request.push_str("Proxy-Authorization: Basic ");
        request.push_str(&token);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(proxy_io)?;

    // 逐字节读到空行，不多读：之后的字节属于 SSH
    let mut response = Vec::new();
    let mut byte = [0u8; 1];
    while !response.ends_with(b"\r\n\r\n") {
        if response.len() > 16 * 1024 {
            return Err(SshError::Proxy("代理的回复过长".into()));
        }
        stream.read_exact(&mut byte).await.map_err(proxy_io)?;
        response.push(byte[0]);
    }
    let status_line = String::from_utf8_lossy(&response);
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    match status {
        200..=299 => Ok(()),
        407 => Err(SshError::Proxy("代理要求认证，或用户名密码不正确".into())),
        other => Err(SshError::Proxy(format!("代理返回 {other}"))),
    }
}

/// 与 HTTPS 代理之间的 TLS（系统证书）。
async fn tls(
    stream: TcpStream,
    server: &str,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>, SshError> {
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    let config = tokio_rustls::rustls::ClientConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| SshError::Proxy(e.to_string()))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let name = tokio_rustls::rustls::pki_types::ServerName::try_from(server.to_string())
        .map_err(|e| SshError::Proxy(format!("代理地址不合法：{e}")))?;
    tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, stream)
        .await
        .map_err(|e| SshError::Proxy(format!("与代理建立 TLS 失败：{e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本机起一个简单的 SOCKS5 服务端，验证握手与认证。
    #[test]
    fn socks5_handshake_with_password() {
        crate::runtime::runtime().block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut greeting = [0u8; 4];
                s.read_exact(&mut greeting).await.unwrap();
                assert_eq!(greeting, [5, 2, 0, 2]);
                s.write_all(&[5, 2]).await.unwrap();
                let mut auth = [0u8; 2 + 3 + 1 + 2];
                s.read_exact(&mut auth).await.unwrap();
                assert_eq!(&auth[..], &[1, 3, b'o', b'p', b's', 2, b'p', b'w']);
                s.write_all(&[1, 0]).await.unwrap();
                let mut request = [0u8; 4 + 1 + 7 + 2];
                s.read_exact(&mut request).await.unwrap();
                assert_eq!(&request[..5], &[5, 1, 0, 3, 7]);
                assert_eq!(&request[5..12], b"example");
                assert_eq!(&request[12..], &22u16.to_be_bytes());
                s.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
                    .await
                    .unwrap();
                s.write_all(b"SSH-2.0-test\r\n").await.unwrap();
            });
            let proxy = ProxyPlan {
                kind: ProxyKind::Socks5,
                address: address.ip().to_string(),
                port: address.port(),
                username: "ops".into(),
                password: Some("pw".into()),
                command: String::new(),
                alias: String::new(),
                from_group: None,
            };
            let (mut stream, _) = first_hop("example", 22, "root", Some(&proxy))
                .await
                .unwrap();
            let mut banner = [0u8; 14];
            stream.read_exact(&mut banner).await.unwrap();
            assert_eq!(&banner, b"SSH-2.0-test\r\n");
            server.await.unwrap();
        });
    }

    #[test]
    fn http_connect_reports_status() {
        crate::runtime::runtime().block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 1024];
                let n = s.read(&mut buf).await.unwrap();
                assert!(String::from_utf8_lossy(&buf[..n]).starts_with("CONNECT db:5432 HTTP/1.1"));
                s.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n")
                    .await
                    .unwrap();
            });
            let proxy = ProxyPlan {
                kind: ProxyKind::Http,
                address: address.ip().to_string(),
                port: address.port(),
                username: String::new(),
                password: None,
                command: String::new(),
                alias: String::new(),
                from_group: None,
            };
            let result = first_hop("db", 5432, "root", Some(&proxy)).await;
            assert!(matches!(result, Err(SshError::Proxy(m)) if m.contains("403")));
        });
    }

    #[test]
    fn unresolvable_host_is_dns_error() {
        // 有的网络（代理的 fake-IP 模式）会把任何域名都解析成功，这时无法验证，跳过
        let resolves =
            std::net::ToSocketAddrs::to_socket_addrs(&("no-such-host.invalid", 22)).is_ok();
        if resolves {
            return;
        }
        let result = crate::runtime::runtime().block_on(tcp("no-such-host.invalid", 22));
        assert!(matches!(result, Err(SshError::Dns { .. })), "{result:?}");
    }
}
