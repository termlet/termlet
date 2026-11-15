//! 动态转发的 SOCKS 服务端：SOCKS5（RFC 1928，只支持无认证与 CONNECT）、SOCKS4 与 SOCKS4a。
//!
//! 地址类型支持 IPv4、IPv6 与域名。域名不在本机解析，原样交给服务器（与 `ssh -D` 相同，内网域名也能访问）。
//! 监听地址只允许回环地址或 0.0.0.0，后者会让局域网内的其他设备无需认证即可经服务器访问网络，界面会提示风险。
//!
//! 与 OpenSSH 的 `ssh -D` 一样同时接受 SOCKS4 / 4a。

use std::net::{Ipv4Addr, Ipv6Addr};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::SocksError;

const V5: u8 = 5;
const V4: u8 = 4;
const NO_AUTH: u8 = 0x00;
const NO_ACCEPTABLE: u8 = 0xff;
const CMD_CONNECT: u8 = 0x01;
const ADDR_IPV4: u8 = 0x01;
const ADDR_DOMAIN: u8 = 0x03;
const ADDR_IPV6: u8 = 0x04;
/// SOCKS4 中用户 ID、SOCKS4a 中域名的最大长度
const MAX_FIELD: usize = 255;

/// 客户端用的协议版本（回复时按它的格式）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Version {
    V4,
    V5,
}

/// 客户端请求连接的目标。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub version: Version,
    /// IP 地址或域名（IPv6 不带方括号）
    pub host: String,
    pub port: u16,
}

/// 对 CONNECT 的回复。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reply {
    Succeeded,
    /// 一般性失败
    Failure,
    /// 服务器不允许转发到这个目标
    NotAllowed,
    /// 服务器连不上目标
    HostUnreachable,
    /// 超时（SOCKS5 的 TTL expired）
    TimedOut,
    CommandNotSupported,
    AddressNotSupported,
}

impl Reply {
    fn v5_code(self) -> u8 {
        match self {
            Reply::Succeeded => 0x00,
            Reply::Failure => 0x01,
            Reply::NotAllowed => 0x02,
            Reply::HostUnreachable => 0x04,
            Reply::TimedOut => 0x06,
            Reply::CommandNotSupported => 0x07,
            Reply::AddressNotSupported => 0x08,
        }
    }
}

/// 完成协商并读取 CONNECT 请求。失败时已向客户端回复（能回复的情况下）。
pub(crate) async fn handshake<S>(stream: &mut S) -> Result<Request, SocksError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut head = [0u8; 2];
    read(stream, &mut head).await?;
    match head[0] {
        V5 => handshake_v5(stream, head[1]).await,
        V4 => handshake_v4(stream, head[1]).await,
        _ => Err(SocksError::NotSocks),
    }
}

async fn handshake_v5<S>(stream: &mut S, methods: u8) -> Result<Request, SocksError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut offered = vec![0u8; methods as usize];
    read(stream, &mut offered).await?;
    if !offered.contains(&NO_AUTH) {
        let _ = stream.write_all(&[V5, NO_ACCEPTABLE]).await;
        return Err(SocksError::AuthRequired);
    }
    stream
        .write_all(&[V5, NO_AUTH])
        .await
        .map_err(|_| SocksError::Read)?;

    let mut request = [0u8; 4]; // VER CMD RSV ATYP
    read(stream, &mut request).await?;
    if request[0] != V5 {
        return Err(SocksError::Malformed);
    }
    if request[1] != CMD_CONNECT {
        let _ = reply(stream, Version::V5, Reply::CommandNotSupported).await;
        return Err(SocksError::UnsupportedCommand(request[1]));
    }
    let host = match request[3] {
        ADDR_IPV4 => {
            let mut ip = [0u8; 4];
            read(stream, &mut ip).await?;
            Ipv4Addr::from(ip).to_string()
        }
        ADDR_IPV6 => {
            let mut ip = [0u8; 16];
            read(stream, &mut ip).await?;
            Ipv6Addr::from(ip).to_string()
        }
        ADDR_DOMAIN => {
            let mut len = [0u8; 1];
            read(stream, &mut len).await?;
            let mut name = vec![0u8; len[0] as usize];
            read(stream, &mut name).await?;
            match domain(name) {
                Some(name) => name,
                None => {
                    let _ = reply(stream, Version::V5, Reply::AddressNotSupported).await;
                    return Err(SocksError::BadDomain);
                }
            }
        }
        other => {
            let _ = reply(stream, Version::V5, Reply::AddressNotSupported).await;
            return Err(SocksError::UnsupportedAddress(other));
        }
    };
    let mut port = [0u8; 2];
    read(stream, &mut port).await?;
    Ok(Request {
        version: Version::V5,
        host,
        port: u16::from_be_bytes(port),
    })
}

/// SOCKS4：`VN CD DSTPORT DSTIP USERID NUL`；SOCKS4a 的 DSTIP 为 0.0.0.x（x 不为 0），用户 ID 之后是以 NUL 结尾的域名。
async fn handshake_v4<S>(stream: &mut S, command: u8) -> Result<Request, SocksError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut rest = [0u8; 6];
    read(stream, &mut rest).await?;
    let port = u16::from_be_bytes([rest[0], rest[1]]);
    let ip = Ipv4Addr::new(rest[2], rest[3], rest[4], rest[5]);
    // 用户 ID：不需要认证，读掉即可
    read_until_nul(stream).await?;
    if command != CMD_CONNECT {
        let _ = reply(stream, Version::V4, Reply::CommandNotSupported).await;
        return Err(SocksError::UnsupportedCommand(command));
    }
    let octets = ip.octets();
    let host = if octets[..3] == [0, 0, 0] && octets[3] != 0 {
        match domain(read_until_nul(stream).await?) {
            Some(name) => name,
            None => {
                let _ = reply(stream, Version::V4, Reply::AddressNotSupported).await;
                return Err(SocksError::BadDomain);
            }
        }
    } else {
        ip.to_string()
    };
    Ok(Request {
        version: Version::V4,
        host,
        port,
    })
}

/// 回复 CONNECT 请求的结果。绑定地址填 0.0.0.0:0（客户端不需要它）。
pub(crate) async fn reply<S>(stream: &mut S, version: Version, reply: Reply) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    match version {
        Version::V5 => {
            stream
                .write_all(&[V5, reply.v5_code(), 0, ADDR_IPV4, 0, 0, 0, 0, 0, 0])
                .await
        }
        Version::V4 => {
            // 90 允许，91 拒绝或失败
            let code = if reply == Reply::Succeeded { 90 } else { 91 };
            stream.write_all(&[0, code, 0, 0, 0, 0, 0, 0]).await
        }
    }
}

async fn read<S: AsyncRead + Unpin>(stream: &mut S, buf: &mut [u8]) -> Result<(), SocksError> {
    stream
        .read_exact(buf)
        .await
        .map(drop)
        .map_err(|_| SocksError::Read)
}

/// 读到 NUL 为止（不含 NUL），最多 [`MAX_FIELD`] 字节。
async fn read_until_nul<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Vec<u8>, SocksError> {
    let mut field = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        read(stream, &mut byte).await?;
        if byte[0] == 0 {
            return Ok(field);
        }
        if field.len() == MAX_FIELD {
            return Err(SocksError::Malformed);
        }
        field.push(byte[0]);
    }
}

/// 域名：非空的 UTF-8，不含空白与控制字符。
fn domain(name: Vec<u8>) -> Option<String> {
    let name = String::from_utf8(name).ok()?;
    let bad = |c: char| c <= ' ' || c == '\u{7f}' || c.is_control();
    (!name.is_empty() && !name.contains(bad)).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把 `input` 交给握手，返回结果与写给客户端的内容。
    fn run(input: &[u8]) -> (Result<Request, SocksError>, Vec<u8>) {
        crate::runtime::runtime().block_on(async {
            let (mut client, mut server) = tokio::io::duplex(1024);
            client.write_all(input).await.unwrap();
            // 写完就关闭：读不够时握手报 Read
            client.shutdown().await.unwrap();
            let result = handshake(&mut server).await;
            drop(server);
            let mut out = Vec::new();
            client.read_to_end(&mut out).await.unwrap();
            (result, out)
        })
    }

    const GREET: [u8; 3] = [5, 1, 0];

    fn v5(rest: &[u8]) -> Vec<u8> {
        [&GREET[..], rest].concat()
    }

    /// SOCKS5 的握手与 SOCKS4 / 4a。
    #[test]
    fn socks5_targets() {
        // IPv4
        let (result, out) = run(&v5(&[5, 1, 0, 1, 127, 0, 0, 1, 0, 80]));
        assert_eq!(
            result.unwrap(),
            Request {
                version: Version::V5,
                host: "127.0.0.1".into(),
                port: 80
            }
        );
        assert_eq!(out, [5, 0], "只回复了协商结果");
        // IPv6
        let mut req = v5(&[5, 1, 0, 4]);
        req.extend_from_slice(&"::1".parse::<Ipv6Addr>().unwrap().octets());
        req.extend_from_slice(&[0x1f, 0x90]);
        let (result, _) = run(&req);
        let request = result.unwrap();
        assert_eq!((request.host.as_str(), request.port), ("::1", 8080));
        // 域名（不在本机解析）
        let mut req = v5(&[5, 1, 0, 3, 9]);
        req.extend_from_slice(b"db.intern");
        req.extend_from_slice(&[0x15, 0x38]);
        let (result, _) = run(&req);
        assert_eq!(result.unwrap().host, "db.intern");
    }

    #[test]
    fn socks5_rejections() {
        // 只支持 CONNECT：BIND 回复 0x07
        let (result, out) = run(&v5(&[5, 2, 0, 1, 127, 0, 0, 1, 0, 80]));
        assert_eq!(result.unwrap_err(), SocksError::UnsupportedCommand(2));
        assert_eq!(out[2 + 1], 0x07, "{out:?}");
        // 客户端只提供用户名密码认证：回复无可接受的方法
        let (result, out) = run(&[5, 1, 2]);
        assert_eq!(result.unwrap_err(), SocksError::AuthRequired);
        assert_eq!(out, [5, NO_ACCEPTABLE]);
        // 不认识的地址类型
        let (result, out) = run(&v5(&[5, 1, 0, 9]));
        assert_eq!(result.unwrap_err(), SocksError::UnsupportedAddress(9));
        assert_eq!(out[2 + 1], 0x08);
        // 域名为空、含空格
        let (result, _) = run(&v5(&[5, 1, 0, 3, 0, 0, 80]));
        assert_eq!(result.unwrap_err(), SocksError::BadDomain);
        let mut req = v5(&[5, 1, 0, 3, 3]);
        req.extend_from_slice(b"a b\x00\x50");
        assert_eq!(run(&req).0.unwrap_err(), SocksError::BadDomain);
        // 请求中途断开
        assert_eq!(run(&v5(&[5, 1])).0.unwrap_err(), SocksError::Read);
        // 不是 SOCKS（例如当成 HTTP 代理）
        let (result, out) = run(b"GET / HTTP/1.1\r\n\r\n");
        assert_eq!(result.unwrap_err(), SocksError::NotSocks);
        assert!(out.is_empty());
    }

    #[test]
    fn socks4_and_4a() {
        // SOCKS4：端口 80，127.0.0.1，用户 ID "u"
        let (result, out) = run(&[4, 1, 0, 80, 127, 0, 0, 1, b'u', 0]);
        assert_eq!(
            result.unwrap(),
            Request {
                version: Version::V4,
                host: "127.0.0.1".into(),
                port: 80
            }
        );
        assert!(out.is_empty(), "成功前不回复");
        // SOCKS4a：0.0.0.1 表示后面跟着域名
        let mut req = vec![4, 1, 0x1f, 0x90, 0, 0, 0, 1, 0];
        req.extend_from_slice(b"localhost\x00");
        let (result, _) = run(&req);
        let request = result.unwrap();
        assert_eq!((request.host.as_str(), request.port), ("localhost", 8080));
        // BIND 不支持：回复 91
        let (result, out) = run(&[4, 2, 0, 80, 127, 0, 0, 1, 0]);
        assert_eq!(result.unwrap_err(), SocksError::UnsupportedCommand(2));
        assert_eq!(out, [0, 91, 0, 0, 0, 0, 0, 0]);
        // 用户 ID 过长
        let mut req = vec![4, 1, 0, 80, 127, 0, 0, 1];
        req.extend(std::iter::repeat_n(b'x', 300));
        assert_eq!(run(&req).0.unwrap_err(), SocksError::Malformed);
    }

    #[test]
    fn replies() {
        crate::runtime::runtime().block_on(async {
            let mut out = Vec::new();
            reply(&mut out, Version::V5, Reply::HostUnreachable)
                .await
                .unwrap();
            assert_eq!(out, [5, 4, 0, 1, 0, 0, 0, 0, 0, 0]);
            let mut out = Vec::new();
            reply(&mut out, Version::V4, Reply::Succeeded)
                .await
                .unwrap();
            assert_eq!(out, [0, 90, 0, 0, 0, 0, 0, 0]);
        });
    }
}
