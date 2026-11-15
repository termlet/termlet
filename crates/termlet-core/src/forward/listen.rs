//! 本地、动态转发在本机的监听。
//!
//! - 端口被占用、被系统保留或没有权限、本机没有这个地址分别报告，界面据此写文案。
//! - Windows 上独占监听（`SO_EXCLUSIVEADDRUSE`）：否则别的程序可以在同一端口上绑定更具体的地址，
//!   悄悄截走发往转发端口的连接；Unix 上与 Go、标准库相同地设 `SO_REUSEADDR`（停止后能立即重新监听）。

use std::io;
use std::net::{IpAddr, SocketAddr};

use tokio::net::{TcpListener, TcpSocket};

use super::ForwardError;
use crate::model::join_host_port;

/// 等待接受的连接数。
const BACKLOG: u32 = 1024;

/// 在本机 `host:port` 上监听（`host` 是规范化过的 IP 地址）。必须在核心的运行时上调用。
pub(crate) fn bind(host: &str, port: u16) -> Result<TcpListener, ForwardError> {
    let address = join_host_port(host, port);
    let ip: IpAddr = host.parse().map_err(|_| ForwardError::AddressUnavailable {
        address: address.clone(),
    })?;
    open(SocketAddr::new(ip, port)).map_err(|error| classify(&address, &error))
}

fn open(addr: SocketAddr) -> io::Result<TcpListener> {
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    #[cfg(windows)]
    exclusive(&socket)?;
    #[cfg(not(windows))]
    socket.set_reuseaddr(true)?;
    socket.bind(addr)?;
    socket.listen(BACKLOG)
}

/// 监听失败的分类。
fn classify(address: &str, error: &io::Error) -> ForwardError {
    let address = address.to_string();
    // Winsock 的错误码：标准库已把它们映射到 ErrorKind，这里再按原始值兜底
    const WSAEACCES: i32 = 10013;
    const WSAEADDRINUSE: i32 = 10048;
    const WSAEADDRNOTAVAIL: i32 = 10049;
    let raw = error.raw_os_error();
    match error.kind() {
        io::ErrorKind::AddrInUse => ForwardError::PortInUse { address },
        io::ErrorKind::PermissionDenied => ForwardError::ListenDenied { address },
        io::ErrorKind::AddrNotAvailable => ForwardError::AddressUnavailable { address },
        _ if cfg!(windows) && raw == Some(WSAEADDRINUSE) => ForwardError::PortInUse { address },
        _ if cfg!(windows) && raw == Some(WSAEACCES) => ForwardError::ListenDenied { address },
        _ if cfg!(windows) && raw == Some(WSAEADDRNOTAVAIL) => {
            ForwardError::AddressUnavailable { address }
        }
        _ => ForwardError::Listen {
            address,
            reason: error.to_string(),
        },
    }
}

/// 独占这个地址与端口：别的套接字不能再绑定同一端口上重叠的地址。
#[cfg(windows)]
fn exclusive(socket: &TcpSocket) -> io::Result<()> {
    use std::os::windows::io::AsRawSocket;

    use windows_sys::Win32::Networking::WinSock::{
        SO_EXCLUSIVEADDRUSE, SOCKET, SOCKET_ERROR, SOL_SOCKET, WSAGetLastError, setsockopt,
    };

    let value: i32 = 1;
    // SAFETY: 套接字句柄在 `socket` 存活期间有效；选项值指向一个 i32，长度与之相符
    let result = unsafe {
        setsockopt(
            socket.as_raw_socket() as SOCKET,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            (&value as *const i32).cast(),
            std::mem::size_of::<i32>() as i32,
        )
    };
    if result == SOCKET_ERROR {
        // SAFETY: 没有前置条件，只读取本线程最近一次 Winsock 错误
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::block_on;

    #[test]
    fn port_in_use_is_reported() {
        block_on(async {
            let first = bind("127.0.0.1", 0).unwrap();
            let port = first.local_addr().unwrap().port();
            let error = bind("127.0.0.1", port).unwrap_err();
            assert_eq!(
                error,
                ForwardError::PortInUse {
                    address: format!("127.0.0.1:{port}")
                }
            );
            // 关闭后可以立即重新监听
            drop(first);
            bind("127.0.0.1", port).unwrap();
        });
    }

    /// 独占监听：转发在 0.0.0.0 上监听后，别的程序不能再在同一端口的 127.0.0.1 上监听、截走本机的连接。
    #[cfg(windows)]
    #[test]
    fn exclusive_listen_prevents_hijack() {
        block_on(async {
            let ours = bind("0.0.0.0", 0).unwrap();
            let port = ours.local_addr().unwrap().port();
            assert!(
                std::net::TcpListener::bind(("127.0.0.1", port)).is_err(),
                "别的程序不应能绑定同一端口的更具体地址"
            );
        });
    }

    #[test]
    fn missing_address_is_reported() {
        block_on(async {
            // 192.0.2.0/24 是文档用的地址，本机不会有
            let error = bind("192.0.2.1", 1).unwrap_err();
            assert!(
                matches!(error, ForwardError::AddressUnavailable { .. }),
                "{error:?}"
            );
            assert!(matches!(
                bind("not-an-ip", 1).unwrap_err(),
                ForwardError::AddressUnavailable { .. }
            ));
        });
    }
}
