//! S3 兼容存储：把同步文件作为一个对象放在桶中（AWS S3、Cloudflare R2、MinIO、阿里云 OSS、腾讯云 COS 等），
//! 请求用 AWS Signature Version 4 签名（见 [`super::sigv4`]）。
//!
//! - 对象键为 `{路径前缀}/termlet-sync.tlsync`（没有前缀时在桶的根上）。
//! - 地址：默认虚拟主机风格（`https://{桶}.{终端节点}/{键}`）；勾选路径风格（MinIO 等需要）或终端节点是 IP 地址、
//!   `localhost` 时为 `https://{终端节点}/{桶}/{键}`。终端节点可以带路径（反向代理之后的服务）。
//! - 防止覆盖其他设备的写入：写入前先 HEAD 比较 ETag（新建时确认对象不存在），再带 `If-Match` / `If-None-Match: *`
//!   写入（AWS S3 自 2024 年起、MinIO、R2 支持，其他设备先写入时返回 412 / 409）。不支持条件写入的服务返回 501 时
//!   不带条件重写一次（只剩写入前比较的很短的竞争窗口；万一被覆盖，那台设备下次同步时发现自己的写入不在云端，重新上传）。
//!   服务只给弱 ETag 或不给时退回为比较内容的 SHA-256。
//! - 不跟随重定向：S3 的 301 / 307 说明区域或终端节点不对，报告服务给出的终端节点。
//! - 检查（测试连接）：读取同步对象的第一个字节（桶不存在、密钥不对、区域不对都在这一步发现），再写入、删除一个探测对象
//!   `{路径前缀}/.termlet-sync-check`（确认有写入权限；没有删除权限时留下这个 2 字节的对象，不影响同步）。
//! - 秘密访问密钥另存在保险库中，只用于派生签名密钥，不在请求中出现，也不写进日志。

use futures::future::BoxFuture;
use reqwest::header::{CONTENT_TYPE, ETAG, HeaderName, IF_MATCH, IF_NONE_MATCH, LOCATION, RANGE};
use reqwest::{Client, Method, Response, StatusCode};
use url::Url;

use super::SyncError;
use super::backend::{Backend, Fetch, Stored, digest};
use super::document::{FILE_NAME, MAX_FILE_SIZE};
use super::http::{client, network, parse_url, read_limited, retry_after};
use super::sigv4::{self, EMPTY_SHA256, Signer};
use crate::secret::SecretString;

/// 没有填写区域时用的区域（AWS 的默认区域，MinIO 的默认值也是它）。
pub const DEFAULT_REGION: &str = "us-east-1";
/// 探测写入权限用的对象名。
const PROBE_NAME: &str = ".termlet-sync-check";
/// 错误响应读取的上限。
const ERROR_BODY_LIMIT: usize = 64 << 10;

/// 规范后的 S3 设置（[`normalize`] 的结果）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Location {
    pub endpoint: Url,
    pub region: String,
    pub bucket: String,
    /// 不带首尾的 `/`；可以为空
    pub prefix: String,
    pub access_key_id: String,
    pub path_style: bool,
}

/// 校验并规范 S3 的设置。终端节点没写协议时补 `https://`；区域留空为 [`DEFAULT_REGION`]。
pub(crate) fn normalize(
    endpoint: &str,
    region: &str,
    bucket: &str,
    prefix: &str,
    access_key_id: &str,
    path_style: bool,
    allow_http: bool,
) -> Result<Location, SyncError> {
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return Err(SyncError::invalid("endpoint", "required"));
    }
    let endpoint = if endpoint.contains("://") {
        endpoint.to_string()
    } else {
        format!("https://{endpoint}")
    };
    let mut endpoint = parse_url(&endpoint, allow_http, "endpoint")?;
    endpoint.set_query(None);
    endpoint.set_fragment(None);
    let trimmed = endpoint.path().trim_end_matches('/').to_string();
    endpoint.set_path(&trimmed);

    let region = match region.trim() {
        "" => DEFAULT_REGION.to_string(),
        r if r.len() <= 64
            && r.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') =>
        {
            r.to_string()
        }
        _ => return Err(SyncError::invalid("region", "invalid")),
    };

    let bucket = bucket.trim();
    let bucket_ok = !bucket.is_empty()
        && bucket.len() <= 255
        && bucket
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !bucket.starts_with('.');
    if !bucket_ok {
        return Err(SyncError::invalid(
            "bucket",
            if bucket.is_empty() {
                "required"
            } else {
                "invalid"
            },
        ));
    }

    let prefix = normalize_prefix(prefix)?;

    let access_key_id = access_key_id.trim();
    if access_key_id.is_empty() {
        return Err(SyncError::invalid("access_key", "required"));
    }
    if access_key_id.len() > 256 || access_key_id.chars().any(|c| c.is_whitespace() || c == '/') {
        return Err(SyncError::invalid("access_key", "invalid"));
    }

    Ok(Location {
        endpoint,
        region,
        bucket: bucket.to_string(),
        prefix,
        access_key_id: access_key_id.to_string(),
        path_style,
    })
}

/// 路径前缀：去掉首尾的 `/`，合并重复的 `/`；只允许字母、数字与 `- _ . /`，不能有 `.`、`..` 这样的段。
fn normalize_prefix(prefix: &str) -> Result<String, SyncError> {
    let segments: Vec<&str> = prefix.trim().split('/').filter(|s| !s.is_empty()).collect();
    let valid = segments.iter().all(|s| {
        *s != "."
            && *s != ".."
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    });
    let joined = segments.join("/");
    if !valid || joined.len() > 512 {
        return Err(SyncError::invalid("prefix", "invalid"));
    }
    Ok(joined)
}

impl Location {
    /// 终端节点的主机是 IP 地址或 `localhost`：只能用路径风格。
    fn needs_path_style(&self) -> bool {
        match self.endpoint.host() {
            Some(url::Host::Ipv4(_) | url::Host::Ipv6(_)) => true,
            Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
            None => true,
        }
    }

    /// 对象 `name`（在路径前缀之下）的地址。
    pub fn object_url(&self, name: &str) -> Result<Url, SyncError> {
        let key = if self.prefix.is_empty() {
            name.to_string()
        } else {
            format!("{}/{name}", self.prefix)
        };
        let mut url = self.endpoint.clone();
        let base = url.path().trim_end_matches('/').to_string();
        if self.path_style || self.needs_path_style() {
            url.set_path(&format!("{base}/{}/{key}", self.bucket));
        } else {
            let host = format!("{}.{}", self.bucket, url.host_str().unwrap_or_default());
            url.set_host(Some(&host))
                .map_err(|_| SyncError::invalid("bucket", "invalid"))?;
            url.set_path(&format!("{base}/{key}"));
        }
        Ok(url)
    }
}

pub(crate) struct S3 {
    object: Url,
    probe: Url,
    signer: Signer,
    client: Client,
    /// 测试用：跳过写入前的比较，只靠条件写入（验证服务是否真的按 If-Match / If-None-Match 拒绝）
    #[cfg(test)]
    pub skip_precheck: bool,
}

/// S3 错误响应中的 `<Code>` 与 `<Endpoint>`（只取文字，不解析整个 XML）。
fn xml_value(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = body.find(&open)? + open.len();
    let end = body[start..].find(&close)? + start;
    Some(body[start..end].trim().to_string()).filter(|v| !v.is_empty())
}

/// 把错误的响应转成同步的错误（不是 2xx、304、404 中的 NoSuchKey）。
fn classify(
    status: StatusCode,
    code: &str,
    endpoint: Option<String>,
    location: Option<String>,
) -> SyncError {
    match (status.as_u16(), code) {
        (_, "RequestTimeTooSkewed") => SyncError::ClockSkew,
        (_, "NoSuchBucket") => SyncError::invalid("bucket", "not_found"),
        (_, "InvalidBucketName") => SyncError::invalid("bucket", "invalid"),
        (_, "AuthorizationHeaderMalformed" | "IllegalLocationConstraintException") => {
            SyncError::invalid("region", "mismatch")
        }
        (_, "InvalidAccessKeyId" | "SignatureDoesNotMatch" | "AccessDenied" | "InvalidToken") => {
            SyncError::AuthFailed
        }
        (_, "EntityTooLarge") => SyncError::TooLarge,
        (_, "SlowDown") | (429, _) => SyncError::RateLimited { retry_after: None },
        (301 | 302 | 307 | 308, _) => SyncError::Redirected {
            status: status.as_u16(),
            location: endpoint.or(location).unwrap_or_default(),
        },
        (401 | 403, _) => SyncError::AuthFailed,
        (507, _) => SyncError::InsufficientStorage,
        (413, _) => SyncError::TooLarge,
        (s, "") => SyncError::Server { status: s },
        (s, code) => SyncError::S3 {
            status: s,
            error_code: code.to_string(),
        },
    }
}

/// 请求的结果：成功（含 404 NoSuchKey）时的响应，或其他设备先写入（412 / 409），或不支持条件写入（501）。
enum Outcome {
    Ok(Response),
    Missing,
    PreconditionFailed,
    NotImplemented,
}

impl S3 {
    pub fn new(location: &Location, secret: SecretString) -> Result<Self, SyncError> {
        if secret.is_empty() {
            return Err(SyncError::invalid("secret_key", "required"));
        }
        Ok(Self {
            object: location.object_url(FILE_NAME)?,
            probe: location.object_url(PROBE_NAME)?,
            signer: Signer {
                access_key: location.access_key_id.clone(),
                secret,
                region: location.region.clone(),
                service: "s3".into(),
            },
            client: client(reqwest::redirect::Policy::none())?,
            #[cfg(test)]
            skip_precheck: false,
        })
    }

    /// 签名并发送一个请求。`headers` 中的头也参与签名。
    async fn send(
        &self,
        method: Method,
        url: &Url,
        headers: &[(HeaderName, String)],
        body: Option<&[u8]>,
    ) -> Result<Outcome, SyncError> {
        let payload = body.map_or_else(|| EMPTY_SHA256.to_string(), sigv4::sha256_hex);
        let date = sigv4::amz_date_now();
        let mut signed: Vec<(&str, &str)> = vec![
            ("x-amz-content-sha256", payload.as_str()),
            ("x-amz-date", date.as_str()),
        ];
        for (name, value) in headers {
            signed.push((name.as_str(), value.as_str()));
        }
        let auth = self.signer.authorization(
            &sigv4::Request {
                method: method.as_str(),
                url,
                headers: &signed,
                payload_sha256: &payload,
            },
            &date,
        );
        let mut request = self
            .client
            .request(method.clone(), url.clone())
            .header("x-amz-content-sha256", &payload)
            .header("x-amz-date", &date)
            .header(reqwest::header::AUTHORIZATION, auth);
        for (name, value) in headers {
            request = request.header(name.clone(), value);
        }
        if let Some(body) = body {
            request = request
                .header(CONTENT_TYPE, "application/octet-stream")
                .body(body.to_vec());
        }
        let response = request.send().await.map_err(network)?;
        let status = response.status();
        if status.is_success() || status == StatusCode::NOT_MODIFIED {
            return Ok(Outcome::Ok(response));
        }
        match status {
            StatusCode::PRECONDITION_FAILED => return Ok(Outcome::PreconditionFailed),
            StatusCode::NOT_IMPLEMENTED if headers.iter().any(|(n, _)| is_condition(n)) => {
                return Ok(Outcome::NotImplemented);
            }
            // HEAD 没有内容，分不出是对象还是桶不存在：当作对象不存在（桶不存在在读取、检查时已经发现）
            StatusCode::NOT_FOUND if method == Method::HEAD => return Ok(Outcome::Missing),
            _ => {}
        }
        let retry = retry_after(&response);
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = read_limited(response, ERROR_BODY_LIMIT)
            .await
            .unwrap_or_default();
        let body = String::from_utf8_lossy(&body);
        let code = xml_value(&body, "Code").unwrap_or_default();
        if status == StatusCode::NOT_FOUND && (code == "NoSuchKey" || code.is_empty()) {
            return Ok(Outcome::Missing);
        }
        // 409 ConditionalRequestConflict：同时有另一个条件写入
        if status == StatusCode::CONFLICT && code == "ConditionalRequestConflict" {
            return Ok(Outcome::PreconditionFailed);
        }
        tracing::debug!(status = status.as_u16(), code = %code, "S3 返回错误");
        let mut error = classify(status, &code, xml_value(&body, "Endpoint"), location);
        if let SyncError::RateLimited { retry_after } = &mut error {
            *retry_after = retry;
        }
        Err(error)
    }

    fn etag_of(response: &Response) -> Option<String> {
        response
            .headers()
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .filter(|tag| !tag.is_empty() && !tag.starts_with("W/"))
    }

    async fn get_file(&self) -> Result<Fetch, SyncError> {
        let response = match self.send(Method::GET, &self.object, &[], None).await? {
            Outcome::Ok(response) => response,
            Outcome::Missing => return Ok(Fetch::Missing),
            Outcome::PreconditionFailed | Outcome::NotImplemented => {
                return Err(SyncError::Server { status: 412 });
            }
        };
        let etag = Self::etag_of(&response);
        let data = read_limited(response, MAX_FILE_SIZE).await?;
        let etag = etag.unwrap_or_else(|| format!("sha256:{}", digest(&data)));
        Ok(Fetch::Found { data, etag })
    }

    /// 同步对象现在的版本标记（`None` 为不存在）。
    async fn current_etag(&self, fallback: bool) -> Result<Option<String>, SyncError> {
        if fallback {
            return Ok(match self.get_file().await? {
                Fetch::Found { etag, .. } => Some(etag),
                Fetch::Missing => None,
            });
        }
        match self.send(Method::HEAD, &self.object, &[], None).await? {
            Outcome::Ok(response) => Ok(Some(Self::etag_of(&response).unwrap_or_default())),
            Outcome::Missing => Ok(None),
            Outcome::PreconditionFailed | Outcome::NotImplemented => {
                Err(SyncError::Server { status: 412 })
            }
        }
    }

    async fn put_file(&self, data: &[u8], etag: Option<&str>) -> Result<Stored, SyncError> {
        let fallback = etag.is_some_and(|t| t.starts_with("sha256:"));
        #[cfg(test)]
        let precheck = !self.skip_precheck;
        #[cfg(not(test))]
        let precheck = true;
        // 写入前比较：服务不认条件写入（静默忽略这两个头）时也不会覆盖其他设备已完成的写入
        if precheck && self.current_etag(fallback).await?.as_deref() != etag {
            return Ok(Stored::Changed);
        }
        let condition: Vec<(HeaderName, String)> = match etag {
            None => vec![(IF_NONE_MATCH, "*".into())],
            Some(_) if fallback => Vec::new(),
            Some(tag) => vec![(IF_MATCH, tag.to_string())],
        };
        match self
            .send(Method::PUT, &self.object, &condition, Some(data))
            .await?
        {
            Outcome::Ok(_) => Ok(Stored::Written),
            Outcome::PreconditionFailed => Ok(Stored::Changed),
            Outcome::NotImplemented => {
                tracing::info!("S3 服务不支持条件写入，改为写入前比较后直接写入");
                match self
                    .send(Method::PUT, &self.object, &[], Some(data))
                    .await?
                {
                    Outcome::Ok(_) => Ok(Stored::Written),
                    Outcome::PreconditionFailed => Ok(Stored::Changed),
                    _ => Err(SyncError::Server { status: 501 }),
                }
            }
            Outcome::Missing => Err(SyncError::invalid("bucket", "not_found")),
        }
    }

    async fn check_access(&self) -> Result<(), SyncError> {
        // 读：只要第一个字节（对象为空时 416，也算可以访问）
        let range = [(RANGE, "bytes=0-0".to_string())];
        match self.send(Method::GET, &self.object, &range, None).await {
            Ok(_) => {}
            Err(SyncError::S3 { status: 416, .. } | SyncError::Server { status: 416 }) => {}
            Err(e) => return Err(e),
        }
        // 写：写入、删除探测对象
        match self
            .send(Method::PUT, &self.probe, &[], Some(b"ok"))
            .await?
        {
            Outcome::Ok(_) => {}
            Outcome::Missing => return Err(SyncError::invalid("bucket", "not_found")),
            _ => return Err(SyncError::Server { status: 412 }),
        }
        if let Err(e) = self.send(Method::DELETE, &self.probe, &[], None).await {
            tracing::info!(
                code = e.code(),
                "删除 S3 中的探测对象失败（没有删除权限？），不影响同步"
            );
        }
        Ok(())
    }
}

fn is_condition(name: &HeaderName) -> bool {
    name == IF_MATCH || name == IF_NONE_MATCH
}

impl Backend for S3 {
    fn get(&self) -> BoxFuture<'_, Result<Fetch, SyncError>> {
        Box::pin(self.get_file())
    }

    fn put<'a>(
        &'a self,
        data: &'a [u8],
        etag: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Stored, SyncError>> {
        Box::pin(self.put_file(data, etag))
    }

    fn check(&self) -> BoxFuture<'_, Result<(), SyncError>> {
        Box::pin(self.check_access())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn location(endpoint: &str, path_style: bool, prefix: &str) -> Location {
        normalize(endpoint, "", "my-bucket", prefix, "AKID", path_style, true).unwrap()
    }

    #[test]
    fn settings_are_normalized() {
        let l = location(" s3.amazonaws.com ", false, "/a//b/ ");
        assert_eq!(l.endpoint.as_str(), "https://s3.amazonaws.com/");
        assert_eq!(l.region, DEFAULT_REGION);
        assert_eq!(l.prefix, "a/b");
        assert_eq!(
            l.object_url(FILE_NAME).unwrap().as_str(),
            "https://my-bucket.s3.amazonaws.com/a/b/termlet-sync.tlsync"
        );
        // 路径风格；终端节点带路径
        let l = location("https://minio.example.com/s3/", true, "");
        assert_eq!(
            l.object_url(FILE_NAME).unwrap().as_str(),
            "https://minio.example.com/s3/my-bucket/termlet-sync.tlsync"
        );
        // IP 地址只能用路径风格
        let l = location("http://192.168.1.10:9000", false, "termlet");
        assert_eq!(
            l.object_url(PROBE_NAME).unwrap().as_str(),
            "http://192.168.1.10:9000/my-bucket/termlet/.termlet-sync-check"
        );
    }

    #[test]
    fn invalid_settings_are_reported() {
        let err =
            |endpoint: &str, region: &str, bucket: &str, prefix: &str, key: &str, http: bool| {
                normalize(endpoint, region, bucket, prefix, key, false, http).unwrap_err()
            };
        assert_eq!(
            err("http://nas:9000", "", "b", "", "k", false),
            SyncError::invalid("endpoint", "insecure")
        );
        assert_eq!(
            err("", "", "b", "", "k", true),
            SyncError::invalid("endpoint", "required")
        );
        assert_eq!(
            err("https://x.example.com", "us east", "b", "", "k", true),
            SyncError::invalid("region", "invalid")
        );
        assert_eq!(
            err("https://x.example.com", "", " ", "", "k", true),
            SyncError::invalid("bucket", "required")
        );
        assert_eq!(
            err("https://x.example.com", "", "a/b", "", "k", true),
            SyncError::invalid("bucket", "invalid")
        );
        assert_eq!(
            err("https://x.example.com", "", "b", "a/../c", "k", true),
            SyncError::invalid("prefix", "invalid")
        );
        assert_eq!(
            err("https://x.example.com", "", "b", "备份", "k", true),
            SyncError::invalid("prefix", "invalid")
        );
        assert_eq!(
            err("https://x.example.com", "", "b", "", " ", true),
            SyncError::invalid("access_key", "required")
        );
        assert!(matches!(
            S3::new(
                &location("https://x.example.com", false, ""),
                SecretString::default()
            ),
            Err(SyncError::Invalid { .. })
        ));
    }

    #[test]
    fn error_codes_are_classified() {
        let c = |status: u16, code: &str| {
            classify(StatusCode::from_u16(status).unwrap(), code, None, None)
        };
        assert_eq!(c(403, "SignatureDoesNotMatch"), SyncError::AuthFailed);
        assert_eq!(c(403, "InvalidAccessKeyId"), SyncError::AuthFailed);
        assert_eq!(c(403, "RequestTimeTooSkewed"), SyncError::ClockSkew);
        assert_eq!(
            c(404, "NoSuchBucket"),
            SyncError::invalid("bucket", "not_found")
        );
        assert_eq!(
            c(400, "AuthorizationHeaderMalformed"),
            SyncError::invalid("region", "mismatch")
        );
        assert_eq!(
            c(503, "SlowDown"),
            SyncError::RateLimited { retry_after: None }
        );
        assert_eq!(c(500, ""), SyncError::Server { status: 500 });
        assert_eq!(
            c(400, "InvalidArgument"),
            SyncError::S3 {
                status: 400,
                error_code: "InvalidArgument".into()
            }
        );
        assert_eq!(
            classify(
                StatusCode::MOVED_PERMANENTLY,
                "PermanentRedirect",
                Some("my-bucket.s3.eu-west-1.amazonaws.com".into()),
                None
            ),
            SyncError::Redirected {
                status: 301,
                location: "my-bucket.s3.eu-west-1.amazonaws.com".into()
            }
        );
        assert_eq!(
            xml_value(
                "<Error><Code>NoSuchKey</Code><Message>x</Message></Error>",
                "Code"
            )
            .as_deref(),
            Some("NoSuchKey")
        );
        assert_eq!(xml_value("<Error></Error>", "Code"), None);
    }
}
