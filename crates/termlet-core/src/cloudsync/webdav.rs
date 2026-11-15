//! WebDAV：把同步文件放在 WebDAV 服务器的目录中（坚果云、Nextcloud、群晖等），HTTP Basic 认证。
//!
//! - 写入带 `If-Match`（服务器给出强 ETag 时）或 `If-None-Match: *`（新建时），其他设备先写入时服务器返回 412；
//!   服务器只给弱 ETag 或不给时退回为写入前重新读取、比较内容（有很短的竞争窗口）。
//! - 只有读取（GET）跟随重定向：PUT、PROPFIND、MKCOL 跟随 301、302 后会变成不带内容的 GET，得到的 200 会被误当作
//!   写入成功。其他请求遇到重定向时报告跳转后的地址，请用户改设置。
//! - 目录不存在时 `check` 用 MKCOL 建立；写入时返回 409 / 404（目录被删除）也先建立再重试一次。
//! - 地址中不能带账号密码（密码另存在保险库中，不会出现在日志里）；默认要求 `https://`，TLS、代理与 HTTP 的
//!   限制见 [`super::http`]。

use futures::future::BoxFuture;
use reqwest::header::{CONTENT_TYPE, ETAG, IF_MATCH, IF_NONE_MATCH, LOCATION};
use reqwest::{Client, Method, RequestBuilder, Response, StatusCode};
use url::Url;

use super::SyncError;
use super::backend::{Backend, Fetch, Stored, digest};
use super::document::{FILE_NAME, MAX_FILE_SIZE};
use super::http::{client, network, parse_url, read_limited};
use crate::secret::SecretString;

pub(crate) struct WebDav {
    dir: Url,
    file: Url,
    username: String,
    password: SecretString,
    /// 读取用：跟随重定向（最多 10 次）
    follow: Client,
    /// 其他请求用：不跟随重定向
    direct: Client,
}

/// 校验并规范 WebDAV 的目录地址：http / https（`http://` 要 `allow_http`）、有主机名、不带账号密码，末尾补 `/`。
pub(crate) fn parse_dir(url: &str, allow_http: bool) -> Result<Url, SyncError> {
    let mut dir = parse_url(url, allow_http, "url")?;
    dir.set_query(None);
    dir.set_fragment(None);
    if !dir.path().ends_with('/') {
        let path = format!("{}/", dir.path());
        dir.set_path(&path);
    }
    Ok(dir)
}

impl WebDav {
    pub fn new(
        url: &str,
        allow_http: bool,
        username: &str,
        password: SecretString,
    ) -> Result<Self, SyncError> {
        let dir = parse_dir(url, allow_http)?;
        let file = dir
            .join(FILE_NAME)
            .map_err(|_| SyncError::invalid("url", "invalid"))?;
        Ok(Self {
            dir,
            file,
            username: username.trim().to_string(),
            password,
            follow: client(reqwest::redirect::Policy::limited(10))?,
            direct: client(reqwest::redirect::Policy::none())?,
        })
    }

    async fn send(
        &self,
        method: Method,
        url: &Url,
        build: impl FnOnce(RequestBuilder) -> RequestBuilder,
    ) -> Result<Response, SyncError> {
        let client = if method == Method::GET {
            &self.follow
        } else {
            &self.direct
        };
        let request = client
            .request(method, url.clone())
            .basic_auth(&self.username, Some(self.password.expose()));
        let response = build(request).send().await.map_err(network)?;
        let status = response.status();
        match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(SyncError::AuthFailed),
            StatusCode::INSUFFICIENT_STORAGE => Err(SyncError::InsufficientStorage),
            StatusCode::PAYLOAD_TOO_LARGE => Err(SyncError::TooLarge),
            s if s.is_redirection() => {
                let location = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| url.join(v).ok())
                    .map(|mut target| {
                        let _ = target.set_username("");
                        let _ = target.set_password(None);
                        target.to_string()
                    })
                    .unwrap_or_default();
                Err(SyncError::Redirected {
                    status: s.as_u16(),
                    location,
                })
            }
            _ => Ok(response),
        }
    }

    async fn get_file(&self) -> Result<Fetch, SyncError> {
        let response = self.send(Method::GET, &self.file, |r| r).await?;
        match response.status() {
            StatusCode::NOT_FOUND | StatusCode::GONE => return Ok(Fetch::Missing),
            StatusCode::OK => {}
            other => {
                return Err(SyncError::Server {
                    status: other.as_u16(),
                });
            }
        }
        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let data = read_limited(response, MAX_FILE_SIZE).await?;
        // 弱 ETag（W/"…"）不能用于 If-Match，退回为比较内容
        let etag = match etag {
            Some(tag) if !tag.is_empty() && !tag.starts_with("W/") => tag,
            _ => format!("sha256:{}", digest(&data)),
        };
        Ok(Fetch::Found { data, etag })
    }

    async fn put_file(&self, data: &[u8], etag: Option<&str>) -> Result<Stored, SyncError> {
        let condition: Option<(reqwest::header::HeaderName, String)> = match etag {
            None => Some((IF_NONE_MATCH, "*".into())),
            Some(tag) if tag.starts_with("sha256:") => {
                // 服务器不提供 ETag：写入前重新读取比较
                match self.get_file().await? {
                    Fetch::Found { etag: current, .. } if current == tag => None,
                    _ => return Ok(Stored::Changed),
                }
            }
            Some(tag) => Some((IF_MATCH, tag.to_string())),
        };
        let put = || {
            let condition = condition.clone();
            let body = data.to_vec();
            move |r: RequestBuilder| {
                let r = r
                    .header(CONTENT_TYPE, "application/octet-stream")
                    .body(body);
                match condition {
                    Some((name, value)) => r.header(name, value),
                    None => r,
                }
            }
        };
        let response = self.send(Method::PUT, &self.file, put()).await?;
        match response.status() {
            StatusCode::OK | StatusCode::CREATED | StatusCode::NO_CONTENT => Ok(Stored::Written),
            StatusCode::PRECONDITION_FAILED => Ok(Stored::Changed),
            StatusCode::CONFLICT | StatusCode::NOT_FOUND => {
                // 目录不存在：建立后重试一次
                self.mkcol().await?;
                let response = self.send(Method::PUT, &self.file, put()).await?;
                match response.status() {
                    StatusCode::PRECONDITION_FAILED => Ok(Stored::Changed),
                    s if s.is_success() => Ok(Stored::Written),
                    s => Err(SyncError::Server { status: s.as_u16() }),
                }
            }
            s => Err(SyncError::Server { status: s.as_u16() }),
        }
    }

    async fn mkcol(&self) -> Result<(), SyncError> {
        let method = Method::from_bytes(b"MKCOL").expect("方法名合法");
        let response = self.send(method, &self.dir, |r| r).await?;
        // 201 已建立；405 已存在
        match response.status() {
            StatusCode::CREATED | StatusCode::METHOD_NOT_ALLOWED => Ok(()),
            s if s.is_success() => Ok(()),
            s => Err(SyncError::Server { status: s.as_u16() }),
        }
    }

    async fn check_dir(&self) -> Result<(), SyncError> {
        let method = Method::from_bytes(b"PROPFIND").expect("方法名合法");
        let response = self
            .send(method, &self.dir, |r| r.header("Depth", "0"))
            .await?;
        match response.status() {
            // 目录还不存在：建立它
            StatusCode::NOT_FOUND => self.mkcol().await,
            s if s.as_u16() >= 300 => Err(SyncError::Server { status: s.as_u16() }),
            _ => Ok(()),
        }
    }
}

impl Backend for WebDav {
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
        Box::pin(self.check_dir())
    }
}
