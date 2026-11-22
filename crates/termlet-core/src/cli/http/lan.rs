//! 局域网访问（监听范围为局域网时，见 [`crate::model::HttpScope`]）：按对端地址区分本机与局域网的连接、局域网的连接
//! 的 `Host` 可以写的本机名字、这台电脑在局域网中的 IPv4 地址（设置页给另一台电脑写配置用）。
//!
//! 只接受私有网段的连接：RFC 1918 的 10/8、172.16/12、192.168/16，链路本地的 169.254/16，与运营商级 NAT、Tailscale
//! 等用的 100.64/10。别的地址来的连接在读请求之前关闭。

use std::net::{IpAddr, Ipv4Addr};

use termlet_mcp::Reach;

/// 对端是哪一种：回环地址为本机，私有网段为局域网，其余为 `None`（拒绝）。IPv4 映射的 IPv6 地址按 IPv4 看。
pub(super) fn reach_of(peer: IpAddr) -> Option<Reach> {
    let peer = peer.to_canonical();
    if peer.is_loopback() {
        return Some(Reach::Local);
    }
    match peer {
        IpAddr::V4(v4) if is_private(v4) => Some(Reach::Lan),
        _ => None,
    }
}

/// 私有网段中的 IPv4 地址（见模块说明）。
pub fn is_private(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 10
        || (a == 172 && (16..32).contains(&b))
        || (a == 192 && b == 168)
        || (a == 169 && b == 254)
        || (a == 100 && (64..128).contains(&b))
}

/// 局域网的连接的 `Host` 中可以写的这台电脑的名字（小写）：主机名、它的第一段，与第一段加上 `.local`（mDNS）。
/// 这条连接的本机地址另外认（[`super::serve`] 中按连接比较）。
pub(super) fn host_names() -> Vec<String> {
    let mut names = Vec::new();
    for name in computer_names() {
        let name = name.trim().trim_end_matches('.').to_ascii_lowercase();
        if name.is_empty() {
            continue;
        }
        let first = name.split('.').next().unwrap_or_default().to_string();
        for candidate in [name, format!("{first}.local"), first] {
            if !candidate.is_empty() && !names.contains(&candidate) {
                names.push(candidate);
            }
        }
    }
    names
}

/// 这台电脑的名字：Windows 上是 DNS 主机名与 NetBIOS 名（`COMPUTERNAME`），其他系统是 `gethostname`。
fn computer_names() -> Vec<String> {
    #[cfg(windows)]
    {
        let mut names = Vec::new();
        if let Some(dns) = windows_dns_hostname() {
            names.push(dns);
        }
        if let Ok(netbios) = std::env::var("COMPUTERNAME") {
            names.push(netbios);
        }
        names
    }
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY：缓冲区有效，长度正确；结果按 NUL 截断
        let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        if ok && end > 0 {
            vec![String::from_utf8_lossy(&buf[..end]).into_owned()]
        } else {
            Vec::new()
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        Vec::new()
    }
}

#[cfg(windows)]
fn windows_dns_hostname() -> Option<String> {
    use windows_sys::Win32::System::SystemInformation::{
        ComputerNamePhysicalDnsHostname, GetComputerNameExW,
    };
    let mut buf = [0u16; 256];
    let mut len = buf.len() as u32;
    // SAFETY：缓冲区有效，`len` 是它的长度（字符数）；成功时 `len` 是写入的字符数（不含 NUL）
    let ok =
        unsafe { GetComputerNameExW(ComputerNamePhysicalDnsHostname, buf.as_mut_ptr(), &mut len) };
    (ok != 0 && len > 0).then(|| String::from_utf16_lossy(&buf[..len as usize]))
}

/// 这台电脑在局域网中的一个 IPv4 地址。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LanAddress {
    pub ip: Ipv4Addr,
    /// 网卡的名字（Windows 上是“WLAN”“以太网”“vEthernet (WSL)”这样的显示名）
    pub interface: String,
    /// 网卡上有默认网关（多半是连着路由器的那一块，排在前面）
    pub gateway: bool,
}

/// 这台电脑在私有网段中的 IPv4 地址（开着的网卡，不含回环）：有默认网关的在前，其余按系统给出的顺序。只读网卡的
/// 信息，不监听、不连网。
pub fn addresses() -> Vec<LanAddress> {
    let mut found: Vec<LanAddress> = imp::addresses()
        .into_iter()
        .filter(|a| is_private(a.ip))
        .collect();
    let mut seen = Vec::new();
    found.retain(|a| {
        let fresh = !seen.contains(&a.ip);
        seen.push(a.ip);
        fresh
    });
    found.sort_by_key(|a| !a.gateway);
    found
}

#[cfg(windows)]
mod imp {
    use std::net::Ipv4Addr;

    use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
        GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses, IF_TYPE_SOFTWARE_LOOPBACK,
        IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

    use super::LanAddress;

    pub(super) fn addresses() -> Vec<LanAddress> {
        // 一般 15KB 就够；网卡多时按要的大小重试
        let mut size: u32 = 16 * 1024;
        for _ in 0..4 {
            let mut buffer: Vec<u64> = vec![0; (size as usize).div_ceil(8)];
            // SAFETY：缓冲区按 8 字节对齐，`size` 不大于它的字节数
            let result = unsafe {
                GetAdaptersAddresses(
                    u32::from(AF_INET),
                    GAA_FLAG_SKIP_ANYCAST
                        | GAA_FLAG_SKIP_MULTICAST
                        | GAA_FLAG_SKIP_DNS_SERVER
                        | GAA_FLAG_INCLUDE_GATEWAYS,
                    std::ptr::null(),
                    buffer.as_mut_ptr().cast(),
                    &mut size,
                )
            };
            match result {
                NO_ERROR => return walk(buffer.as_ptr().cast()),
                ERROR_BUFFER_OVERFLOW => continue,
                _ => return Vec::new(),
            }
        }
        Vec::new()
    }

    /// 走一遍网卡的链表（都在同一个缓冲区中，调用方在这期间持有它）。
    fn walk(mut adapter: *const IP_ADAPTER_ADDRESSES_LH) -> Vec<LanAddress> {
        let mut found = Vec::new();
        while !adapter.is_null() {
            // SAFETY：链表由 GetAdaptersAddresses 写在缓冲区中，节点、地址与名字的指针都指向缓冲区之内或为空
            let a = unsafe { &*adapter };
            if a.OperStatus == IfOperStatusUp && a.IfType != IF_TYPE_SOFTWARE_LOOPBACK {
                let interface = unsafe { wide(a.FriendlyName) };
                let gateway = !a.FirstGatewayAddress.is_null();
                let mut unicast = a.FirstUnicastAddress;
                while !unicast.is_null() {
                    let u = unsafe { &*unicast };
                    let sockaddr = u.Address.lpSockaddr;
                    if !sockaddr.is_null()
                        && unsafe { (*sockaddr).sa_family } == AF_INET
                        && u.Address.iSockaddrLength as usize >= size_of::<SOCKADDR_IN>()
                    {
                        let v4 = unsafe { &*sockaddr.cast::<SOCKADDR_IN>() };
                        // 地址按网络字节序存放：内存中的四个字节就是点分的四段
                        let octets = unsafe { v4.sin_addr.S_un.S_addr }.to_ne_bytes();
                        found.push(LanAddress {
                            ip: Ipv4Addr::from(octets),
                            interface: interface.clone(),
                            gateway,
                        });
                    }
                    unicast = u.Next;
                }
            }
            adapter = a.Next;
        }
        found
    }

    /// 以 NUL 结尾的 UTF-16 字符串。
    ///
    /// # Safety
    /// `text` 为空或指向以 NUL 结尾的 UTF-16 字符串。
    unsafe fn wide(text: *const u16) -> String {
        if text.is_null() {
            return String::new();
        }
        let mut len = 0;
        // SAFETY：调用方保证以 NUL 结尾
        while unsafe { *text.add(len) } != 0 {
            len += 1;
        }
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) })
    }
}

#[cfg(unix)]
mod imp {
    use std::ffi::CStr;
    use std::net::Ipv4Addr;

    use super::LanAddress;

    pub(super) fn addresses() -> Vec<LanAddress> {
        let gateway = default_route_interface();
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        // SAFETY：成功时 `list` 是系统分配的链表，用完由 freeifaddrs 释放
        if unsafe { libc::getifaddrs(&mut list) } != 0 {
            return Vec::new();
        }
        let mut found = Vec::new();
        let mut node = list;
        while !node.is_null() {
            // SAFETY：链表节点由 getifaddrs 分配，在 freeifaddrs 之前有效
            let n = unsafe { &*node };
            let up = n.ifa_flags & (libc::IFF_UP as libc::c_uint) != 0;
            let loopback = n.ifa_flags & (libc::IFF_LOOPBACK as libc::c_uint) != 0;
            if up
                && !loopback
                && !n.ifa_addr.is_null()
                && i32::from(unsafe { (*n.ifa_addr).sa_family }) == libc::AF_INET
            {
                let v4 = unsafe { &*n.ifa_addr.cast::<libc::sockaddr_in>() };
                let interface = if n.ifa_name.is_null() {
                    String::new()
                } else {
                    unsafe { CStr::from_ptr(n.ifa_name) }
                        .to_string_lossy()
                        .into_owned()
                };
                found.push(LanAddress {
                    ip: Ipv4Addr::from(v4.sin_addr.s_addr.to_ne_bytes()),
                    gateway: gateway.as_deref() == Some(interface.as_str()),
                    interface,
                });
            }
            node = n.ifa_next;
        }
        // SAFETY：释放 getifaddrs 分配的链表，之后不再使用其中的指针
        unsafe { libc::freeifaddrs(list) };
        found
    }

    /// Linux：默认路由所在的网卡（`/proc/net/route` 中目标为 0 的一行）；其他系统不查。
    fn default_route_interface() -> Option<String> {
        let table = std::fs::read_to_string("/proc/net/route").ok()?;
        table.lines().skip(1).find_map(|line| {
            let mut fields = line.split_whitespace();
            let interface = fields.next()?;
            (fields.next()? == "00000000").then(|| interface.to_string())
        })
    }
}

#[cfg(not(any(windows, unix)))]
mod imp {
    pub(super) fn addresses() -> Vec<super::LanAddress> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 回环为本机；私有网段为局域网（边界两侧都看）；公网、组播、IPv6 的非回环地址拒绝
    #[test]
    fn reach_follows_the_peer_address() {
        let reach = |ip: &str| reach_of(ip.parse().unwrap());
        for local in ["127.0.0.1", "127.8.9.10", "::1", "::ffff:127.0.0.1"] {
            assert_eq!(reach(local), Some(Reach::Local), "{local}");
        }
        for lan in [
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.1.23",
            "169.254.10.20",
            "100.64.0.1",
            "100.127.255.255",
            "::ffff:192.168.1.23",
        ] {
            assert_eq!(reach(lan), Some(Reach::Lan), "{lan}");
        }
        for outside in [
            "8.8.8.8",
            "172.15.255.255",
            "172.32.0.1",
            "192.169.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "11.0.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "fe80::1",
            "fd00::1",
            "2001:db8::1",
        ] {
            assert_eq!(reach(outside), None, "{outside}");
        }
    }

    // 本机名字：小写，带上第一段与 .local；读网卡地址只读不监听，得到的都是私有网段、不重复
    #[test]
    fn names_and_addresses_of_this_computer() {
        let names = host_names();
        for name in &names {
            assert_eq!(name, &name.to_ascii_lowercase());
            assert!(!name.is_empty() && !name.ends_with('.'));
        }
        if let Some(first) = names.first() {
            let short = first.split('.').next().unwrap();
            assert!(names.contains(&format!("{short}.local")), "{names:?}");
        }
        let found = addresses();
        assert!(found.iter().all(|a| is_private(a.ip)), "{found:?}");
        let ips: std::collections::HashSet<Ipv4Addr> = found.iter().map(|a| a.ip).collect();
        assert_eq!(ips.len(), found.len(), "不重复：{found:?}");
        let gateways = found.iter().take_while(|a| a.gateway).count();
        assert!(
            found[gateways..].iter().all(|a| !a.gateway),
            "有网关的在前：{found:?}"
        );
    }
}
