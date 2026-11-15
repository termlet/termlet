//! WebDAV、S3 兼容存储与 GitHub Gist 共用的 HTTP 部分：客户端、代理、连接失败的原因、地址的协议检查。
//!
//! - TLS 用 rustls 与系统的根证书。
//! - 代理只认环境变量（`HTTPS_PROXY`、`HTTP_PROXY`、`ALL_PROXY`、`NO_PROXY`），不用系统设置中的代理。
//! - 地址默认要求 `https://`；局域网中的服务要用 `http://` 时须在设置中明确勾选“允许不安全的 HTTP”
//!   （这一项属于本机的同步设置，不同步）。同步文件本身是端到端加密的，HTTP 暴露的是账号与密码（WebDAV 的 Basic
//!   认证）、请求可被篡改或重放，以及访问的时间与数据量。

use std::time::Duration;

use reqwest::{Client, Response};
use url::Url;

use super::SyncError;

/// 单个请求的总时限。
pub(crate) const TIMEOUT: Duration = Duration::from_secs(60);
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

pub(crate) fn client(redirect: reqwest::redirect::Policy) -> Result<Client, SyncError> {
    let mut builder = Client::builder()
        .timeout(TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .user_agent(format!("Termlet/{}", crate::VERSION))
        .redirect(redirect)
        // 不用系统设置中的代理：这个版本的 reqwest 读 Windows 的代理设置时不认"不使用代理的地址"，
        // 局域网中的 NAS 也会走代理。只认环境变量
        .no_proxy();
    for proxy in env_proxies() {
        builder = builder.proxy(proxy);
    }
    builder.build().map_err(network)
}

/// 环境变量中的代理（`HTTPS_PROXY`、`HTTP_PROXY`、`ALL_PROXY`，大小写均可），`NO_PROXY` 中的地址不走代理。
fn env_proxies() -> Vec<reqwest::Proxy> {
    let var = |name: &str| {
        std::env::var(name)
            .or_else(|_| std::env::var(name.to_lowercase()))
            .ok()
            .filter(|v| !v.trim().is_empty())
    };
    let mut out = Vec::new();
    let candidates: [(&str, fn(String) -> reqwest::Result<reqwest::Proxy>); 3] = [
        ("HTTPS_PROXY", |v| reqwest::Proxy::https(v)),
        ("HTTP_PROXY", |v| reqwest::Proxy::http(v)),
        ("ALL_PROXY", |v| reqwest::Proxy::all(v)),
    ];
    for (name, make) in candidates {
        let Some(value) = var(name) else {
            continue;
        };
        match make(value) {
            Ok(proxy) => out.push(proxy.no_proxy(reqwest::NoProxy::from_env())),
            Err(_) => tracing::warn!("环境变量 {name} 中的代理地址无法识别，忽略"),
        }
    }
    out
}

/// 连接失败的原因（去掉其中的地址，附上底层原因，例如证书不受信任）。
pub(crate) fn network(error: reqwest::Error) -> SyncError {
    let timeout = error.is_timeout();
    let error = error.without_url();
    let mut message = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        message.push('：');
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    if timeout {
        message.insert_str(0, "超时：");
    }
    SyncError::Network { message }
}

/// 解析服务的地址：http / https、有主机名、不带账号密码；`http://` 只在 `allow_http` 时接受。
/// 不合法时报告字段 `field`（`invalid`、`has_credentials`、`insecure`）。
pub(crate) fn parse_url(text: &str, allow_http: bool, field: &str) -> Result<Url, SyncError> {
    let url = Url::parse(text.trim()).map_err(|_| SyncError::invalid(field, "invalid"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none_or(str::is_empty) {
        return Err(SyncError::invalid(field, "invalid"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(SyncError::invalid(field, "has_credentials"));
    }
    if url.scheme() == "http" && !allow_http {
        return Err(SyncError::invalid(field, "insecure"));
    }
    Ok(url)
}

/// 读出响应的内容，超过 `limit` 字节时返回 [`SyncError::TooLarge`]（不把过大的内容读进内存）。
pub(crate) async fn read_limited(
    mut response: Response,
    limit: usize,
) -> Result<Vec<u8>, SyncError> {
    if response
        .content_length()
        .is_some_and(|len| len > limit as u64)
    {
        return Err(SyncError::TooLarge);
    }
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(network)? {
        if data.len() + chunk.len() > limit {
            return Err(SyncError::TooLarge);
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

/// 服务器要求过多久再试（`Retry-After` 的秒数；没有或不是秒数时为 `None`）。
pub(crate) fn retry_after(response: &Response) -> Option<u64> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_needs_explicit_permission() {
        assert_eq!(
            parse_url("http://nas.local:5005/dav", false, "url").unwrap_err(),
            SyncError::invalid("url", "insecure")
        );
        assert_eq!(
            parse_url(" http://nas.local:5005/dav ", true, "url")
                .unwrap()
                .as_str(),
            "http://nas.local:5005/dav"
        );
        assert!(parse_url("https://dav.example.com/", false, "url").is_ok());
        assert_eq!(
            parse_url("ftp://x", true, "endpoint").unwrap_err(),
            SyncError::invalid("endpoint", "invalid")
        );
        assert_eq!(
            parse_url("https://a:b@x.example.com", true, "endpoint").unwrap_err(),
            SyncError::invalid("endpoint", "has_credentials")
        );
    }
}
