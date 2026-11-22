//! 按 TCP 对端查出发起连接的进程：HTTP 没有 stdio 那样的父子关系，审计中的进程链（`Cursor.exe`、`node` 等）
//! 据此得到，比客户端自报的名字可信，批准框也照样能按它配上 AI 工具的标志。
//!
//! - Windows：`GetExtendedTcpTable`（`TCP_TABLE_OWNER_PID_CONNECTIONS`）中本地地址等于对端、远程地址等于本机监听地址
//!   的那一行的进程号；
//! - Linux：`/proc/net/tcp`（及 `tcp6` 中的 IPv4 映射地址）找到这条连接的 inode，再在 `/proc/*/fd` 中找持有它的进程；
//! - 其他平台不记录（与 stdio 相同）。
//!
//! 查不到时为空，不影响请求。

use std::net::SocketAddr;

/// 发起连接 `peer → local` 的进程与它的祖先的进程名（由近到远）。
pub(super) fn process_chain(peer: SocketAddr, local: SocketAddr) -> Vec<String> {
    match owner_pid(peer, local) {
        Some(pid) => termlet_proto::caller::process_chain(pid),
        None => Vec::new(),
    }
}

/// 本机上 `peer` 这一端的进程号。
pub(super) fn owner_pid(peer: SocketAddr, local: SocketAddr) -> Option<u32> {
    if !peer.ip().is_loopback() {
        return None;
    }
    imp::owner_pid(peer, local)
}

/// `/proc/net/tcp` 中的地址（`0100007F:BB80`：IPv4 按本机字节序的十六进制，端口是大端的十六进制）。
/// `tcp6` 的地址是 32 个十六进制字符（4 个按本机字节序的 32 位字）；只认 IPv4 映射地址（`::ffff:a.b.c.d`）。
#[cfg(any(target_os = "linux", test))]
fn parse_proc_address(text: &str, little_endian: bool) -> Option<SocketAddr> {
    use std::net::{IpAddr, Ipv4Addr};
    let (ip, port) = text.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let word = |hex: &str| -> Option<[u8; 4]> {
        let value = u32::from_str_radix(hex, 16).ok()?;
        Some(if little_endian {
            value.to_le_bytes()
        } else {
            value.to_be_bytes()
        })
    };
    let ip = match ip.len() {
        8 => Ipv4Addr::from(word(ip)?),
        32 => {
            let words = [
                word(&ip[0..8])?,
                word(&ip[8..16])?,
                word(&ip[16..24])?,
                word(&ip[24..32])?,
            ];
            let mapped = words[0] == [0; 4] && words[1] == [0; 4] && words[2] == [0, 0, 0xff, 0xff];
            if !mapped {
                return None;
            }
            Ipv4Addr::from(words[3])
        }
        _ => return None,
    };
    Some(SocketAddr::new(IpAddr::V4(ip), port))
}

/// `/proc/net/tcp(6)` 中本地地址为 `peer`、远程地址为 `local` 的连接的 inode。
#[cfg(any(target_os = "linux", test))]
fn find_inode(
    table: &str,
    peer: SocketAddr,
    local: SocketAddr,
    little_endian: bool,
) -> Option<u64> {
    table.lines().skip(1).find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (from, to, inode) = (fields.get(1)?, fields.get(2)?, fields.get(9)?);
        (parse_proc_address(from, little_endian)? == peer
            && parse_proc_address(to, little_endian)? == local)
            .then(|| inode.parse().ok())
            .flatten()
            .filter(|inode| *inode != 0)
    })
}

#[cfg(windows)]
mod imp {
    use std::net::{IpAddr, SocketAddr};

    use windows_sys::Win32::Foundation::NO_ERROR;
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID,
        TCP_TABLE_OWNER_PID_CONNECTIONS,
    };
    use windows_sys::Win32::Networking::WinSock::AF_INET;

    pub(super) fn owner_pid(peer: SocketAddr, local: SocketAddr) -> Option<u32> {
        let (IpAddr::V4(peer_ip), IpAddr::V4(local_ip)) = (peer.ip(), local.ip()) else {
            return None;
        };
        let table = tcp_table()?;
        // 表中的地址与端口是网络字节序：地址的四个字节按内存顺序，端口在低 16 位
        let addr = |row_addr: u32| row_addr.to_ne_bytes();
        let port = |row_port: u32| u16::from_be_bytes([row_port as u8, (row_port >> 8) as u8]);
        table
            .iter()
            .find(|row| {
                addr(row.dwLocalAddr) == peer_ip.octets()
                    && port(row.dwLocalPort) == peer.port()
                    && addr(row.dwRemoteAddr) == local_ip.octets()
                    && port(row.dwRemotePort) == local.port()
            })
            .map(|row| row.dwOwningPid)
    }

    /// IPv4 的 TCP 连接表（含进程号）。表在两次调用之间可能变大：按要的大小重试几次。
    fn tcp_table() -> Option<Vec<MIB_TCPROW_OWNER_PID>> {
        let mut size: u32 = 0;
        let mut buffer: Vec<u64> = Vec::new();
        for _ in 0..4 {
            // SAFETY: 缓冲区按 8 字节对齐，`size` 是它的字节数；第一次传空指针取需要的大小
            let result = unsafe {
                GetExtendedTcpTable(
                    if buffer.is_empty() {
                        std::ptr::null_mut()
                    } else {
                        buffer.as_mut_ptr().cast()
                    },
                    &mut size,
                    0,
                    u32::from(AF_INET),
                    TCP_TABLE_OWNER_PID_CONNECTIONS,
                    0,
                )
            };
            if result == NO_ERROR {
                if buffer.is_empty() {
                    return Some(Vec::new());
                }
                let base = buffer.as_ptr().cast::<u8>();
                let offset = std::mem::offset_of!(MIB_TCPTABLE_OWNER_PID, table);
                // SAFETY: 成功时缓冲区开头是 MIB_TCPTABLE_OWNER_PID：先是行数，`table` 处起是这么多行；读之前核对
                // 这些行都在缓冲区之内。指针都从整个缓冲区得来
                return unsafe {
                    let count = base.cast::<u32>().read() as usize;
                    let bytes = offset + count * std::mem::size_of::<MIB_TCPROW_OWNER_PID>();
                    (bytes <= buffer.len() * 8).then(|| {
                        let rows = base.add(offset).cast::<MIB_TCPROW_OWNER_PID>();
                        std::slice::from_raw_parts(rows, count).to_vec()
                    })
                };
            }
            // ERROR_INSUFFICIENT_BUFFER：按要的大小再来（多留一些，表可能又变大了）
            buffer = vec![0u64; (size as usize).div_ceil(8) + 64];
            size = (buffer.len() * 8) as u32;
        }
        None
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::net::SocketAddr;

    use super::find_inode;

    pub(super) fn owner_pid(peer: SocketAddr, local: SocketAddr) -> Option<u32> {
        let little_endian = cfg!(target_endian = "little");
        let inode = ["/proc/net/tcp", "/proc/net/tcp6"]
            .iter()
            .find_map(|path| {
                let table = std::fs::read_to_string(path).ok()?;
                find_inode(&table, peer, local, little_endian)
            })?;
        let target = format!("socket:[{inode}]");
        for entry in std::fs::read_dir("/proc").ok()?.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
                continue;
            };
            for fd in fds.flatten() {
                if std::fs::read_link(fd.path())
                    .is_ok_and(|link| link.as_os_str() == target.as_str())
                {
                    return Some(pid);
                }
            }
        }
        None
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    use std::net::SocketAddr;

    pub(super) fn owner_pid(_peer: SocketAddr, _local: SocketAddr) -> Option<u32> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    #[test]
    fn parses_proc_net_tcp() {
        // 小端机器上 127.0.0.1 写成 0100007F
        assert_eq!(
            parse_proc_address("0100007F:BB80", true),
            Some(addr("127.0.0.1:48000"))
        );
        assert_eq!(
            parse_proc_address("7F000001:0016", false),
            Some(addr("127.0.0.1:22"))
        );
        // tcp6 中的 IPv4 映射地址；真正的 IPv6 地址不认
        assert_eq!(
            parse_proc_address("0000000000000000FFFF00000100007F:BB80", true),
            Some(addr("127.0.0.1:48000"))
        );
        assert_eq!(
            parse_proc_address("00000000000000000000000001000000:BB80", true),
            None
        );
        assert_eq!(parse_proc_address("zz:1", true), None);

        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 0100007F:BAB9 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 11111 1 0000000000000000 100 0 0 10 0\n\
   1: 0100007F:C350 0100007F:BAB9 01 00000000:00000000 00:00000000 00000000  1000        0 22222 1 0000000000000000 20 4 30 10 -1\n\
   2: 0100007F:BAB9 0100007F:C350 01 00000000:00000000 00:00000000 00000000  1000        0 33333 1 0000000000000000 20 4 30 10 -1\n";
        // 对端 127.0.0.1:50000（C350）连到本机 127.0.0.1:47801（BAB9）：取对端那一行
        assert_eq!(
            find_inode(
                table,
                addr("127.0.0.1:50000"),
                addr("127.0.0.1:47801"),
                true
            ),
            Some(22222)
        );
        assert_eq!(
            find_inode(
                table,
                addr("127.0.0.1:50001"),
                addr("127.0.0.1:47801"),
                true
            ),
            None
        );
    }

    // 本进程连到本进程监听的端口：查到的是本进程（Windows、Linux）
    #[test]
    fn finds_the_connecting_process() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_server, peer) = listener.accept().unwrap();
        assert_eq!(peer, client.local_addr().unwrap());
        let local = listener.local_addr().unwrap();
        let found = owner_pid(peer, local);
        if cfg!(any(windows, target_os = "linux")) {
            assert_eq!(found, Some(std::process::id()));
            let chain = process_chain(peer, local);
            let me = std::env::current_exe().unwrap();
            let me = me.file_name().unwrap().to_string_lossy().to_lowercase();
            assert!(me.starts_with(&chain[0].to_lowercase()), "{chain:?}");
        } else {
            assert_eq!(found, None);
        }
        // 不是回环地址的对端不查
        assert_eq!(owner_pid(addr("192.0.2.1:5000"), local), None);
    }
}
