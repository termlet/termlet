//! 更新与远程配置用的 HTTP 客户端。
//!
//! 走系统与环境变量中的代理（GitHub 在一些网络中要经代理才能访问）；
//! 本机回环地址（测试的更新源）不走代理。内容都有签名，传输层只是通道。

use std::sync::LazyLock;
use std::time::Duration;

use url::{Host, Url};

use super::UpdateError;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 两次读到数据之间的最长间隔（下载安装包没有总时限）。
const READ_TIMEOUT: Duration = Duration::from_secs(30);

fn build(direct: bool) -> Option<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .user_agent(format!("Termlet/{}", crate::VERSION));
    if direct {
        builder = builder.no_proxy();
    }
    match builder.build() {
        Ok(client) => Some(client),
        Err(e) => {
            tracing::warn!("无法创建 HTTP 客户端：{e}");
            None
        }
    }
}

static PROXIED: LazyLock<Option<reqwest::Client>> = LazyLock::new(|| build(false));
static DIRECT: LazyLock<Option<reqwest::Client>> = LazyLock::new(|| build(true));

/// 地址是否是本机回环地址（127.0.0.1、localhost、\[::1\]）。
pub(crate) fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip == std::net::Ipv4Addr::LOCALHOST,
        Some(Host::Ipv6(ip)) => ip == std::net::Ipv6Addr::LOCALHOST,
        None => false,
    }
}

fn client(url: &Url) -> Result<&'static reqwest::Client, UpdateError> {
    let client = if is_loopback(url) { &DIRECT } else { &PROXIED };
    client
        .as_ref()
        .ok_or_else(|| UpdateError::Network("无法创建 HTTP 客户端".into()))
}

/// GET 请求；`timeout` 为整个请求（含读完内容）的时限。
pub(crate) async fn get(
    url: &Url,
    timeout: Option<Duration>,
) -> Result<reqwest::Response, UpdateError> {
    let mut request = client(url)?.get(url.clone());
    if let Some(timeout) = timeout {
        request = request.timeout(timeout);
    }
    request.send().await.map_err(UpdateError::network)
}

/// 取一个小文件（版本描述文件、签名、远程配置）。
pub(crate) enum Fetched {
    Body(String),
    /// 服务器返回的不是成功的状态码
    Status(u16),
}

/// 取一个不超过 `max` 字节的文本文件。
pub(crate) async fn get_text(
    url: &Url,
    timeout: Duration,
    max: usize,
) -> Result<Fetched, UpdateError> {
    let mut response = get(url, Some(timeout)).await?;
    let status = response.status();
    if !status.is_success() {
        return Ok(Fetched::Status(status.as_u16()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(UpdateError::network)? {
        if body.len() + chunk.len() > max {
            return Err(UpdateError::Manifest(format!("文件超过 {max} 字节")));
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body)
        .map(Fetched::Body)
        .map_err(|_| UpdateError::Manifest("不是 UTF-8 文本".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts() {
        for url in [
            "http://127.0.0.1:8080/feed",
            "http://localhost:1/x",
            "http://[::1]:2/",
        ] {
            assert!(is_loopback(&Url::parse(url).unwrap()), "{url}");
        }
        for url in [
            "https://github.com/termlet/termlet/releases/latest/download",
            "http://127.0.0.1.evil.com/feed",
            "http://127.0.0.2/feed",
            "http://192.168.1.2/feed",
        ] {
            assert!(!is_loopback(&Url::parse(url).unwrap()), "{url}");
        }
    }
}
