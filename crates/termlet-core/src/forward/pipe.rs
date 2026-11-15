//! 转发一个连接：双向复制并实时统计字节数。

use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// 每次读写的缓冲区大小。
const BUFFER: usize = 32 * 1024;

/// 一个方向是怎么结束的。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    /// 读到 EOF，已向另一端半关闭
    Eof,
    /// 读写出错：连接已不可用
    Broken,
}

/// 双向复制直到两个方向都结束：一个方向读到 EOF 时向另一端半关闭（与 `ssh -L` 一致），
/// 任一方向出错时两个方向一起结束。`a_to_b` 统计从 `a` 写到 `b` 的字节数，`b_to_a` 反之（实时累加，长连接也能看到进度）。
pub(crate) async fn pipe<A, B>(a: A, b: B, a_to_b: &AtomicU64, b_to_a: &AtomicU64)
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let (mut a_read, mut a_write) = tokio::io::split(a);
    let (mut b_read, mut b_write) = tokio::io::split(b);
    let forward = copy(&mut a_read, &mut b_write, a_to_b);
    let backward = copy(&mut b_read, &mut a_write, b_to_a);
    tokio::pin!(forward, backward);
    let (first, rest) = tokio::select! {
        end = &mut forward => (end, Rest::Backward),
        end = &mut backward => (end, Rest::Forward),
    };
    if first == End::Broken {
        return;
    }
    match rest {
        Rest::Forward => forward.await,
        Rest::Backward => backward.await,
    };
}

enum Rest {
    Forward,
    Backward,
}

async fn copy<R, W>(read: &mut R, write: &mut W, counter: &AtomicU64) -> End
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = vec![0u8; BUFFER];
    loop {
        let n = match read.read(&mut buffer).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => return End::Broken,
        };
        if write.write_all(&buffer[..n]).await.is_err() {
            return End::Broken;
        }
        counter.fetch_add(n as u64, Ordering::Relaxed);
    }
    if write.shutdown().await.is_err() {
        return End::Broken;
    }
    End::Eof
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_both_ways_and_half_closes() {
        crate::runtime::runtime().block_on(async {
            let (mut client, near) = tokio::io::duplex(64);
            let (far, mut server) = tokio::io::duplex(64);
            let sent = AtomicU64::new(0);
            let received = AtomicU64::new(0);
            let piping = pipe(near, far, &sent, &received);
            let talk = async {
                client.write_all(b"hello").await.unwrap();
                // 客户端写完：服务端读到 EOF，仍可回复
                client.shutdown().await.unwrap();
                let mut got = Vec::new();
                server.read_to_end(&mut got).await.unwrap();
                assert_eq!(got, b"hello");
                server.write_all(b"world!").await.unwrap();
                server.shutdown().await.unwrap();
                let mut back = Vec::new();
                client.read_to_end(&mut back).await.unwrap();
                assert_eq!(back, b"world!");
            };
            tokio::join!(piping, talk);
            assert_eq!(sent.load(Ordering::Relaxed), 5);
            assert_eq!(received.load(Ordering::Relaxed), 6);
        });
    }
}
