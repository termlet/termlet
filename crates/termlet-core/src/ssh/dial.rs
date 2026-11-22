//! TCP 连接的双栈并发（Happy Eyeballs，按 RFC 8305 的做法）：主机同时有 IPv6 与 IPv4 地址时，不再逐个等每个地址
//! 超时，而是按协议族交替排好、错开发起连接，第一个连上的胜出、其余的取消。本机的 IPv6 坏了时不用先等它超时。
//!
//! SSH、Telnet 的直连与连代理（[`super::transport::tcp`]）、端口转发连本机的目标、本机代理分享连本机的代理、X11 连
//! 本机的 X 服务器都经这里。

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use tokio::net::TcpStream;

/// 下一个地址错开的时间（RFC 8305 建议的 Connection Attempt Delay）：前面的这么久还没连上，同时连下一个；
/// 前面的失败了马上连下一个。
pub(crate) const ATTEMPT_DELAY: Duration = Duration::from_millis(250);

type Attempt<T> = Pin<Box<dyn Future<Output = io::Result<T>> + Send>>;

/// 连接这些地址（解析的结果，按系统给出的顺序）：按 IPv6、IPv4 交替排好（第一个地址的协议族在前），先连第一个，
/// [`ATTEMPT_DELAY`] 内没连上就同时连下一个，依次错开；第一个连上的胜出，其余的取消。`timeout` 是总的时长。
///
/// 全部失败时返回排在最后的那个地址的错误；到了总时长它还没有结果（或者还没轮到）时为超时
/// （[`io::ErrorKind::TimedOut`]）。与原来逐个连接时一样，报的是最后一个地址的结果。
pub(crate) async fn connect_any(
    addresses: &[SocketAddr],
    timeout: Duration,
) -> io::Result<(TcpStream, SocketAddr)> {
    race(&interleave(addresses), ATTEMPT_DELAY, timeout, |address| {
        Box::pin(TcpStream::connect(address))
    })
    .await
}

/// 解析 `host` 后按 [`connect_any`] 连接（转发的目标、本机代理、X 服务器用）：解析失败与连接失败都作为 `io::Error`
/// 返回，`timeout` 包括解析。
pub(crate) async fn connect_host(
    host: &str,
    port: u16,
    timeout: Duration,
) -> io::Result<TcpStream> {
    let deadline = tokio::time::Instant::now() + timeout;
    let addresses: Vec<SocketAddr> =
        tokio::time::timeout_at(deadline, tokio::net::lookup_host((host, port)))
            .await
            .map_err(|_| timed_out())??
            .collect();
    if addresses.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{host} 没有解析出地址"),
        ));
    }
    let left = deadline.saturating_duration_since(tokio::time::Instant::now());
    let (stream, address) = connect_any(&addresses, left).await?;
    tracing::debug!(host, %address, "TCP 已连接");
    Ok(stream)
}

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "连接超时")
}

/// 按协议族交替排列（RFC 8305 第 4 节）：第一个地址的协议族在前，同一协议族内保持原来的顺序。
fn interleave(addresses: &[SocketAddr]) -> Vec<SocketAddr> {
    let Some(first) = addresses.first() else {
        return Vec::new();
    };
    let (preferred, other): (Vec<_>, Vec<_>) = addresses
        .iter()
        .copied()
        .partition(|a| a.is_ipv6() == first.is_ipv6());
    let mut ordered = Vec::with_capacity(addresses.len());
    let (mut preferred, mut other) = (preferred.into_iter(), other.into_iter());
    loop {
        match (preferred.next(), other.next()) {
            (None, None) => return ordered,
            (a, b) => ordered.extend(a.into_iter().chain(b)),
        }
    }
}

/// 按排好的顺序错开 `delay` 发起连接（`connect` 发起一次，测试中可以换成不响应的）；见 [`connect_any`]。
async fn race<T>(
    addresses: &[SocketAddr],
    delay: Duration,
    timeout: Duration,
    connect: impl Fn(SocketAddr) -> Attempt<T>,
) -> io::Result<(T, SocketAddr)> {
    let Some(&last) = addresses.last() else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "没有地址"));
    };
    let deadline = tokio::time::Instant::now() + timeout;
    let mut attempts = FuturesUnordered::new();
    let mut last_error = None;
    let mut next = 0;
    // 第一个马上发起
    let stagger = tokio::time::sleep(Duration::ZERO);
    tokio::pin!(stagger);
    loop {
        tokio::select! {
            Some((address, result)) = attempts.next() => match result {
                // 返回时丢掉 `attempts`：其余正在连接的随之取消
                Ok(stream) => return Ok((stream, address)),
                Err(error) => {
                    tracing::debug!(%address, "TCP 连接失败：{error}");
                    if address == last {
                        last_error = Some(error);
                    }
                    if next == addresses.len() {
                        if attempts.is_empty() {
                            break;
                        }
                    } else {
                        stagger.as_mut().reset(tokio::time::Instant::now());
                    }
                }
            },
            () = &mut stagger, if next < addresses.len() => {
                let address = addresses[next];
                next += 1;
                let attempt = connect(address);
                attempts.push(async move { (address, attempt.await) });
                stagger.as_mut().reset(tokio::time::Instant::now() + delay);
            }
            () = tokio::time::sleep_until(deadline) => {
                tracing::debug!(timeout_ms = timeout.as_millis() as u64, "TCP 连接超时");
                break;
            }
        }
    }
    Err(last_error.unwrap_or_else(timed_out))
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    use super::*;
    use crate::testkit::block_on;

    fn v4(last: u8) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, last)), 22)
    }

    fn v6(last: u16) -> SocketAddr {
        SocketAddr::new(
            IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, last)),
            22,
        )
    }

    #[test]
    fn interleaves_families_starting_with_the_first() {
        assert_eq!(
            interleave(&[v6(1), v6(2), v6(3), v4(1), v4(2)]),
            [v6(1), v4(1), v6(2), v4(2), v6(3)]
        );
        assert_eq!(
            interleave(&[v4(1), v4(2), v6(1)]),
            [v4(1), v6(1), v4(2)],
            "系统把 IPv4 排在前面时 IPv4 先"
        );
        assert_eq!(interleave(&[v4(1), v4(2)]), [v4(1), v4(2)]);
        assert!(interleave(&[]).is_empty());
    }

    /// 测试中的一次连接：成功、马上失败（拒绝），或者一直不响应（丢掉时记下）。
    #[derive(Clone)]
    enum Fake {
        Accept,
        Refuse,
        Silent(Arc<AtomicBool>),
    }

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn fake(plan: Vec<(SocketAddr, Fake)>) -> impl Fn(SocketAddr) -> Attempt<SocketAddr> {
        move |address| {
            let behavior = plan
                .iter()
                .find(|(a, _)| *a == address)
                .map(|(_, b)| b.clone())
                .unwrap();
            Box::pin(async move {
                match behavior {
                    Fake::Accept => Ok(address),
                    Fake::Refuse => Err(io::ErrorKind::ConnectionRefused.into()),
                    Fake::Silent(dropped) => {
                        let _flag = DropFlag(dropped);
                        std::future::pending().await
                    }
                }
            })
        }
    }

    // IPv6 不响应：250 毫秒后同时连 IPv4，IPv4 连上就用它，不等 IPv6 超时；还在连的 IPv6 被取消
    #[test]
    fn falls_back_to_ipv4_after_the_attempt_delay() {
        block_on(async {
            let dropped = Arc::new(AtomicBool::new(false));
            let connect = fake(vec![
                (v6(1), Fake::Silent(dropped.clone())),
                (v4(1), Fake::Accept),
            ]);
            let started = Instant::now();
            let (_, address) = race(
                &[v6(1), v4(1)],
                ATTEMPT_DELAY,
                Duration::from_secs(15),
                connect,
            )
            .await
            .unwrap();
            let elapsed = started.elapsed();
            assert_eq!(address, v4(1));
            assert!(elapsed >= ATTEMPT_DELAY, "{elapsed:?}");
            assert!(
                elapsed < Duration::from_secs(2),
                "不应等 IPv6 超时：{elapsed:?}"
            );
            assert!(dropped.load(Ordering::SeqCst), "没连上的应取消");
        });
    }

    // 前一个失败了马上连下一个，不等 250 毫秒
    #[test]
    fn failure_starts_the_next_attempt_at_once() {
        block_on(async {
            let connect = fake(vec![(v6(1), Fake::Refuse), (v4(1), Fake::Accept)]);
            let started = Instant::now();
            let (_, address) = race(
                &[v6(1), v4(1)],
                Duration::from_secs(5),
                Duration::from_secs(15),
                connect,
            )
            .await
            .unwrap();
            assert_eq!(address, v4(1));
            assert!(started.elapsed() < Duration::from_secs(2));
        });
    }

    // 总时长到了：最后一个地址还没有结果时报超时；最后一个已经失败时报它的错误（与原来逐个连接时相同）
    #[test]
    fn total_timeout_reports_the_last_address() {
        block_on(async {
            let silent = || Fake::Silent(Arc::new(AtomicBool::new(false)));
            let timeout = Duration::from_millis(400);
            let connect = fake(vec![(v6(1), Fake::Refuse), (v4(1), silent())]);
            let started = Instant::now();
            let error = race(&[v6(1), v4(1)], ATTEMPT_DELAY, timeout, connect)
                .await
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            let elapsed = started.elapsed();
            assert!(
                elapsed >= timeout && elapsed < Duration::from_secs(3),
                "总时长：{elapsed:?}"
            );

            let connect = fake(vec![(v6(1), silent()), (v4(1), Fake::Refuse)]);
            let error = race(&[v6(1), v4(1)], ATTEMPT_DELAY, timeout, connect)
                .await
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);

            // 全部马上失败：不等总时长
            let connect = fake(vec![(v6(1), Fake::Refuse), (v4(1), Fake::Refuse)]);
            let started = Instant::now();
            let error = race(
                &[v6(1), v4(1)],
                ATTEMPT_DELAY,
                Duration::from_secs(15),
                connect,
            )
            .await
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
            assert!(started.elapsed() < Duration::from_secs(2));
        });
    }

    // 地址多于两个时依次错开：第一个、第二个都不响应，第三个在约 500 毫秒时发起
    #[test]
    fn attempts_are_staggered_one_by_one() {
        block_on(async {
            let silent = || Fake::Silent(Arc::new(AtomicBool::new(false)));
            let connect = fake(vec![
                (v6(1), silent()),
                (v4(1), silent()),
                (v6(2), Fake::Accept),
            ]);
            let started = Instant::now();
            let (_, address) = race(
                &interleave(&[v6(1), v6(2), v4(1)]),
                ATTEMPT_DELAY,
                Duration::from_secs(15),
                connect,
            )
            .await
            .unwrap();
            let elapsed = started.elapsed();
            assert_eq!(address, v6(2));
            assert!(elapsed >= ATTEMPT_DELAY * 2, "{elapsed:?}");
            assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
        });
    }

    // 真实的套接字：IPv6 回环上没有服务（Windows 上连本机关着的端口要重试约 2 秒才报拒绝）、IPv4 回环上有，
    // 并发连接很快用上 IPv4；只有 IPv6 回环上有服务时用 IPv6（本机没有 IPv6 回环时跳过这一半）
    #[test]
    fn real_sockets_use_whichever_family_listens() {
        block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let addresses = [
                SocketAddr::new(Ipv6Addr::LOCALHOST.into(), port),
                SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port),
            ];
            let started = Instant::now();
            let (_, address) = connect_any(&addresses, Duration::from_secs(15))
                .await
                .unwrap();
            assert_eq!(address, addresses[1]);
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "{:?}",
                started.elapsed()
            );

            let Ok(listener6) = tokio::net::TcpListener::bind("[::1]:0").await else {
                return;
            };
            let port = listener6.local_addr().unwrap().port();
            let addresses = [
                SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port),
                SocketAddr::new(Ipv6Addr::LOCALHOST.into(), port),
            ];
            let (_, address) = connect_any(&addresses, Duration::from_secs(15))
                .await
                .unwrap();
            assert_eq!(address, addresses[1]);
            drop(listener);
        });
    }

    #[test]
    fn connect_host_resolves_and_reports_failures() {
        block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            // localhost 在多数系统上解析出 ::1 与 127.0.0.1
            let started = Instant::now();
            connect_host("localhost", port, Duration::from_secs(15))
                .await
                .unwrap();
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "{:?}",
                started.elapsed()
            );
            drop(listener);
            let error = connect_host("127.0.0.1", port, Duration::from_secs(15))
                .await
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
        });
    }
}
