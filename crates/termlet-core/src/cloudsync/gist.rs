//! GitHub Gist：把同步文件放在一个私密（secret）Gist 中，用个人访问令牌访问 GitHub 的 REST API。
//!
//! # 文件布局
//!
//! Gist 只能放文本，API 读取时单个文件最多给出 1 MB 的内容（超过时 `truncated`，要按 `raw_url` 另取；超过 10 MB 的
//! 只能用 git 克隆），列出的文件最多 300 个。所以加密后的同步文件先编码成 Base64，按 900 KiB 一段拆成
//! `termlet-sync.000.b64`、`termlet-sync.001.b64`……，另有说明文件 `termlet-sync.json`：
//!
//! ```text
//! {"format":"termlet-gist","version":1,"size":<字节数>,"sha256":"<加密文件的摘要>","parts":["termlet-sync.000.b64", …]}
//! ```
//!
//! 读取时一次取回整个 Gist（各段都在 1 MB 以内，不用另取；万一被截断就按 `raw_url` 读取，不带令牌），拼接、解码后
//! 核对大小与摘要。一次写入（PATCH）在 Gist 中是一个提交，说明与各段同时更新；段数变少时删掉多出来的段。
//! 加密后的同步文件超过 [`MAX_SIZE`]（8 MiB，约 12 段）时不写入，报告 [`SyncError::GistTooLarge`]，请用户改用其他
//! 存储位置。
//!
//! # 并发写入
//!
//! Gist 的 API 没有条件写入。版本标记是说明中的摘要（每次加密的随机数不同，内容相同也不会重复）：写入前重新读取比较，
//! 其他设备先写入了就报告“已被其他设备修改”，重新拉取再合并。读取与写入之间仍有很短的竞争窗口：写入后从提交历史
//! 发现覆盖了其他设备的写入时记下日志；那台设备下次同步时发现自己的写入不在云端（同步的“写入丢失”保护），重新上传，
//! 不会丢失数据。
//!
//! # 令牌与 Gist
//!
//! - 经典令牌要有 `gist` 权限（响应头 `X-OAuth-Scopes` 中没有时报告缺少权限）；细粒度令牌要有 Gists 的读写权限。
//! - 没有填写 Gist ID 时在令牌所属账号的 Gist 中查找含 `termlet-sync.json` 的（其他电脑已经建好的）；没有就在设置时
//!   新建一个私密 Gist 并记下 ID；找到多个时请用户指定（[`SyncError::GistAmbiguous`]）。
//! - 令牌另存在保险库中，只放在发给 GitHub API 的请求头里，不写进日志。
//! - 私密 Gist 不会出现在公开列表与搜索中，但知道地址的人可以看到（内容是加密的）。

use std::collections::BTreeMap;

use futures::future::BoxFuture;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, Method, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

use super::SyncError;
use super::backend::{Backend, Fetch, Stored, digest};
use super::http::{client, network, read_limited, retry_after};
use crate::secret::SecretString;

/// GitHub REST API 的地址。
pub const API_BASE: &str = "https://api.github.com/";
/// 说明文件的文件名（也用来认出 Termlet 建的 Gist）。
pub const MANIFEST: &str = "termlet-sync.json";
/// 每一段 Base64 的长度（字符）：在 API 读取时的 1 MB 截断以内。是 4 的倍数，每段都能单独解码。
pub(crate) const PART_SIZE: usize = 900 * 1024;
/// 加密后的同步文件放进 Gist 的大小上限。
pub const MAX_SIZE: usize = 8 << 20;
const FORMAT: &str = "termlet-gist";
const FORMAT_VERSION: u32 = 1;
/// 新建的 Gist 的说明（显示在 GitHub 上）。
const DESCRIPTION: &str = "Termlet sync data (end-to-end encrypted)";
/// 读取整个 Gist 的响应的上限（各段的内容加上 JSON）。
const RESPONSE_LIMIT: usize = 32 << 20;
/// 按 `raw_url` 读取一段的上限。
const RAW_LIMIT: usize = 2 * PART_SIZE;
/// 查找已有的 Gist 时最多翻几页（每页 100 个）。
const MAX_LIST_PAGES: usize = 10;

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    format: String,
    version: u32,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    sha256: String,
    #[serde(default)]
    parts: Vec<String>,
}

/// 第 `index` 段的文件名。
pub(crate) fn part_name(index: usize) -> String {
    format!("termlet-sync.{index:03}.b64")
}

/// 是 Termlet 写的段（写入时删掉多出来的段用；别的文件不动）。
fn is_part_name(name: &str) -> bool {
    name.strip_prefix("termlet-sync.")
        .and_then(|rest| rest.strip_suffix(".b64"))
        .is_some_and(|n| n.len() == 3 && n.bytes().all(|b| b.is_ascii_digit()))
}

/// 加密后的同步文件在 Gist 中的样子：说明与各段（文件名、内容）。
#[derive(Debug)]
pub(crate) struct Layout {
    pub manifest: String,
    pub parts: Vec<(String, String)>,
}

/// 把加密后的同步文件写成 Gist 中的文件。
pub(crate) fn encode(data: &[u8]) -> Result<Layout, SyncError> {
    if data.len() > MAX_SIZE {
        return Err(SyncError::GistTooLarge {
            size: data.len() as u64,
            limit: MAX_SIZE as u64,
        });
    }
    let text = data_encoding::BASE64.encode(data);
    let parts: Vec<(String, String)> = text
        .as_bytes()
        .chunks(PART_SIZE)
        .enumerate()
        .map(|(i, chunk)| {
            (
                part_name(i),
                String::from_utf8(chunk.to_vec()).expect("Base64 是 ASCII"),
            )
        })
        .collect();
    let manifest = serde_json::to_string(&Manifest {
        format: FORMAT.into(),
        version: FORMAT_VERSION,
        size: data.len() as u64,
        sha256: digest(data),
        parts: parts.iter().map(|(name, _)| name.clone()).collect(),
    })?;
    Ok(Layout { manifest, parts })
}

/// 还没有同步文件时的说明（新建 Gist 时写入）。
pub(crate) fn empty_manifest() -> String {
    serde_json::to_string(&Manifest {
        format: FORMAT.into(),
        version: FORMAT_VERSION,
        size: 0,
        sha256: String::new(),
        parts: Vec::new(),
    })
    .expect("说明可以序列化")
}

fn parse_manifest(text: &str) -> Result<Manifest, SyncError> {
    let manifest: Manifest = serde_json::from_str(text).map_err(|_| SyncError::Corrupt)?;
    if manifest.format != FORMAT {
        return Err(SyncError::Corrupt);
    }
    if manifest.version > FORMAT_VERSION {
        return Err(SyncError::TooNew);
    }
    if manifest.parts.len()
        > data_encoding::BASE64
            .encode_len(MAX_SIZE)
            .div_ceil(PART_SIZE)
        || manifest.size > MAX_SIZE as u64
        || manifest.parts.iter().any(|p| !is_part_name(p))
    {
        return Err(SyncError::Corrupt);
    }
    Ok(manifest)
}

/// 拼接各段并核对：返回加密后的同步文件与版本标记。
fn assemble(manifest: &Manifest, parts: &[String]) -> Result<(Vec<u8>, String), SyncError> {
    let mut text = String::with_capacity(parts.iter().map(String::len).sum());
    for part in parts {
        text.push_str(part.trim());
    }
    let data = data_encoding::BASE64
        .decode(text.as_bytes())
        .map_err(|_| SyncError::Corrupt)?;
    if data.len() as u64 != manifest.size || digest(&data) != manifest.sha256 {
        return Err(SyncError::Corrupt);
    }
    Ok((data, format!("gist:{}", manifest.sha256)))
}

/// 规范用户填写的 Gist ID：可以是 ID 本身，或 Gist 的网址（取最后一段）。留空表示自动查找或新建。
pub(crate) fn parse_gist_id(text: &str) -> Result<String, SyncError> {
    let text = text.trim();
    let text = text.split(['#', '?']).next().unwrap_or_default();
    let id = text
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .trim_end_matches(".git");
    if id.is_empty() {
        return Ok(String::new());
    }
    if id.len() > 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(SyncError::invalid("gist", "invalid"));
    }
    Ok(id.to_ascii_lowercase())
}

#[derive(Deserialize)]
struct GistWire {
    id: String,
    #[serde(default)]
    files: BTreeMap<String, Option<FileWire>>,
    #[serde(default)]
    history: Vec<HistoryWire>,
}

#[derive(Deserialize)]
struct FileWire {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    truncated: bool,
    #[serde(default)]
    raw_url: Option<String>,
}

#[derive(Deserialize)]
struct HistoryWire {
    version: String,
}

/// 列出 Gist 时的一项（只要 ID 与文件名）。
#[derive(Deserialize)]
struct ListedGist {
    id: String,
    #[serde(default)]
    files: BTreeMap<String, Value>,
}

/// 请求的对象：`gists/{id}` 不存在时报告 Gist 不存在，其余（列出、新建）的 404 说明令牌没有权限。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    Gist,
    Account,
}

/// 令牌所属的 GitHub 账号（查找、新建 Gist 用）。
pub(crate) struct Account {
    api: Url,
    token: SecretString,
    client: Client,
}

impl Account {
    pub fn new(api: &str, token: SecretString) -> Result<Self, SyncError> {
        if token.expose().trim().is_empty() {
            return Err(SyncError::invalid("token", "required"));
        }
        let api = Url::parse(api).map_err(|_| SyncError::invalid("gist", "invalid"))?;
        Ok(Self {
            api,
            token,
            client: client(reqwest::redirect::Policy::none())?,
        })
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        target: Target,
    ) -> Result<Response, SyncError> {
        let url = self
            .api
            .join(path)
            .map_err(|_| SyncError::invalid("gist", "invalid"))?;
        let mut request = self
            .client
            .request(method, url)
            .header(ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header(
                AUTHORIZATION,
                format!("Bearer {}", self.token.expose().trim()),
            );
        if let Some(body) = body {
            request = request
                .header(CONTENT_TYPE, "application/json")
                .body(serde_json::to_vec(body)?);
        }
        let response = request.send().await.map_err(network)?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let limited = response
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            == Some("0");
        let retry = retry_after(&response);
        let message = read_limited(response, 64 << 10).await.unwrap_or_default();
        let message = String::from_utf8_lossy(&message).to_lowercase();
        tracing::debug!(status = status.as_u16(), "GitHub API 返回错误");
        Err(match status {
            StatusCode::UNAUTHORIZED => SyncError::AuthFailed,
            StatusCode::TOO_MANY_REQUESTS => SyncError::RateLimited { retry_after: retry },
            StatusCode::FORBIDDEN if limited || message.contains("rate limit") => {
                SyncError::RateLimited { retry_after: retry }
            }
            StatusCode::FORBIDDEN => SyncError::invalid("token", "no_gist_scope"),
            StatusCode::NOT_FOUND if target == Target::Gist => {
                SyncError::invalid("gist", "not_found")
            }
            StatusCode::NOT_FOUND => SyncError::invalid("token", "no_gist_scope"),
            s if s.is_redirection() => SyncError::Redirected {
                status: s.as_u16(),
                location: String::new(),
            },
            s => SyncError::Server { status: s.as_u16() },
        })
    }

    /// 经典令牌没有 `gist` 权限时报错（细粒度令牌不给这个头，写入时才知道）。
    fn check_scopes(response: &Response) -> Result<(), SyncError> {
        let Some(scopes) = response
            .headers()
            .get("x-oauth-scopes")
            .and_then(|v| v.to_str().ok())
        else {
            return Ok(());
        };
        if scopes.split(',').any(|s| s.trim() == "gist") {
            Ok(())
        } else {
            Err(SyncError::invalid("token", "no_gist_scope"))
        }
    }

    /// 令牌所属账号中含 Termlet 说明文件的 Gist。
    pub async fn find(&self) -> Result<Vec<String>, SyncError> {
        let mut found = Vec::new();
        for page in 1..=MAX_LIST_PAGES {
            let response = self
                .send(
                    Method::GET,
                    &format!("gists?per_page=100&page={page}"),
                    None,
                    Target::Account,
                )
                .await?;
            Self::check_scopes(&response)?;
            let body = read_limited(response, RESPONSE_LIMIT).await?;
            let list: Vec<ListedGist> =
                serde_json::from_slice(&body).map_err(|_| SyncError::Server { status: 200 })?;
            let count = list.len();
            found.extend(
                list.into_iter()
                    .filter(|g| g.files.contains_key(MANIFEST))
                    .map(|g| g.id),
            );
            if count < 100 {
                break;
            }
        }
        Ok(found)
    }

    /// 新建一个私密 Gist（只有说明文件），返回它的 ID。
    pub async fn create(&self) -> Result<String, SyncError> {
        let body = json!({
            "description": DESCRIPTION,
            "public": false,
            "files": { MANIFEST: { "content": empty_manifest() } },
        });
        let response = self
            .send(Method::POST, "gists", Some(&body), Target::Account)
            .await?;
        let body = read_limited(response, RESPONSE_LIMIT).await?;
        let created: GistWire =
            serde_json::from_slice(&body).map_err(|_| SyncError::Server { status: 201 })?;
        parse_gist_id(&created.id)
            .ok()
            .filter(|id| !id.is_empty())
            .ok_or(SyncError::Server { status: 201 })
    }
}

/// 读到的 Gist：同步文件（没有时为 `None`）、最新提交的版本、其中的文件名。
struct Snapshot {
    file: Option<(Vec<u8>, String)>,
    version: Option<String>,
    names: Vec<String>,
}

pub(crate) struct Gist {
    account: Account,
    id: String,
    /// 按 `raw_url` 读取用：跟随重定向，不带令牌
    raw: Client,
}

impl Gist {
    pub fn new(api: &str, token: SecretString, id: &str) -> Result<Self, SyncError> {
        let id = parse_gist_id(id)?;
        if id.is_empty() {
            return Err(SyncError::invalid("gist", "invalid"));
        }
        Ok(Self {
            account: Account::new(api, token)?,
            id,
            raw: client(reqwest::redirect::Policy::limited(5))?,
        })
    }

    /// 文件的内容：没有被截断时直接用，否则按 `raw_url` 读取。
    async fn content(&self, file: &FileWire) -> Result<String, SyncError> {
        if !file.truncated
            && let Some(content) = &file.content
        {
            return Ok(content.clone());
        }
        let url = file.raw_url.as_deref().ok_or(SyncError::Corrupt)?;
        let url = Url::parse(url).map_err(|_| SyncError::Corrupt)?;
        tracing::debug!("Gist 中的文件被截断，按 raw_url 读取");
        let response = self.raw.get(url).send().await.map_err(network)?;
        if !response.status().is_success() {
            return Err(SyncError::Server {
                status: response.status().as_u16(),
            });
        }
        let data = read_limited(response, RAW_LIMIT).await?;
        String::from_utf8(data).map_err(|_| SyncError::Corrupt)
    }

    async fn snapshot(&self) -> Result<Snapshot, SyncError> {
        let response = self
            .account
            .send(
                Method::GET,
                &format!("gists/{}", self.id),
                None,
                Target::Gist,
            )
            .await?;
        let body = read_limited(response, RESPONSE_LIMIT).await?;
        let wire: GistWire =
            serde_json::from_slice(&body).map_err(|_| SyncError::Server { status: 200 })?;
        let version = wire.history.first().map(|h| h.version.clone());
        let names: Vec<String> = wire.files.keys().cloned().collect();
        let file = match wire.files.get(MANIFEST) {
            Some(Some(manifest)) => {
                let manifest = parse_manifest(&self.content(manifest).await?)?;
                if manifest.parts.is_empty() {
                    None
                } else {
                    let mut parts = Vec::with_capacity(manifest.parts.len());
                    for name in &manifest.parts {
                        let Some(Some(file)) = wire.files.get(name) else {
                            return Err(SyncError::Corrupt);
                        };
                        parts.push(self.content(file).await?);
                    }
                    Some(assemble(&manifest, &parts)?)
                }
            }
            // 新建后还没有写入、或说明文件被删掉了：当作还没有同步文件
            _ => None,
        };
        Ok(Snapshot {
            file,
            version,
            names,
        })
    }

    async fn get_file(&self) -> Result<Fetch, SyncError> {
        Ok(match self.snapshot().await?.file {
            Some((data, etag)) => Fetch::Found { data, etag },
            None => Fetch::Missing,
        })
    }

    async fn put_file(&self, data: &[u8], etag: Option<&str>) -> Result<Stored, SyncError> {
        // 先检查大小：放不下时不发任何请求
        let layout = encode(data)?;
        let before = self.snapshot().await?;
        if before.file.as_ref().map(|(_, tag)| tag.as_str()) != etag {
            return Ok(Stored::Changed);
        }
        let mut files = serde_json::Map::new();
        files.insert(MANIFEST.into(), json!({ "content": layout.manifest }));
        for (name, content) in &layout.parts {
            files.insert(name.clone(), json!({ "content": content }));
        }
        for name in before.names.iter().filter(|n| is_part_name(n)) {
            if !files.contains_key(name) {
                files.insert(name.clone(), Value::Null);
            }
        }
        let body = json!({ "files": files });
        let response = self
            .account
            .send(
                Method::PATCH,
                &format!("gists/{}", self.id),
                Some(&body),
                Target::Gist,
            )
            .await?;
        // 这次提交之前的那个提交应是读取时的最新提交，否则这期间其他设备写入过（被这次覆盖了）
        if let Some(read) = before.version
            && let Ok(body) = read_limited(response, RESPONSE_LIMIT).await
            && let Ok(after) = serde_json::from_slice::<GistWire>(&body)
            && after.history.get(1).is_some_and(|h| h.version != read)
        {
            tracing::warn!(
                "写入 Gist 时其他设备在读取之后也写入过：那台设备下次同步时会发现并重新上传"
            );
        }
        Ok(Stored::Written)
    }

    async fn check_gist(&self) -> Result<(), SyncError> {
        let response = self
            .account
            .send(
                Method::GET,
                &format!("gists/{}", self.id),
                None,
                Target::Gist,
            )
            .await?;
        Account::check_scopes(&response)
    }
}

impl Backend for Gist {
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
        Box::pin(self.check_gist())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files_of(layout: &Layout) -> Vec<String> {
        layout.parts.iter().map(|(_, c)| c.clone()).collect()
    }

    #[test]
    fn layout_round_trips_in_parts() {
        // 刚好一段、跨两段、空文件
        for len in [
            0usize,
            1,
            PART_SIZE / 4 * 3,
            PART_SIZE / 4 * 3 + 1,
            2_000_000,
        ] {
            let data: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
            let layout = encode(&data).unwrap();
            let expected_parts = data_encoding::BASE64.encode_len(len).div_ceil(PART_SIZE);
            assert_eq!(layout.parts.len(), expected_parts, "{len}");
            assert!(layout.parts.iter().all(|(_, c)| c.len() <= PART_SIZE));
            let manifest = parse_manifest(&layout.manifest).unwrap();
            assert_eq!(
                manifest.parts,
                (0..expected_parts).map(part_name).collect::<Vec<_>>()
            );
            if len == 0 {
                assert!(manifest.parts.is_empty());
                continue;
            }
            let (back, etag) = assemble(&manifest, &files_of(&layout)).unwrap();
            assert_eq!(back, data);
            assert_eq!(etag, format!("gist:{}", digest(&data)));
        }
    }

    #[test]
    fn layout_is_checked() {
        let data = vec![7u8; PART_SIZE * 2];
        let layout = encode(&data).unwrap();
        let manifest = parse_manifest(&layout.manifest).unwrap();
        let mut parts = files_of(&layout);
        // 少一段、段的内容不对：都不能当作同步文件
        assert_eq!(
            assemble(&manifest, &parts[..1]).unwrap_err(),
            SyncError::Corrupt
        );
        parts[0].replace_range(0..4, "AAAA");
        assert_eq!(assemble(&manifest, &parts).unwrap_err(), SyncError::Corrupt);
        // 格式不对、来自更新版本
        assert_eq!(
            parse_manifest(r#"{"format":"x","version":1}"#).unwrap_err(),
            SyncError::Corrupt
        );
        assert_eq!(
            parse_manifest(r#"{"format":"termlet-gist","version":9}"#).unwrap_err(),
            SyncError::TooNew
        );
        assert_eq!(
            parse_manifest(r#"{"format":"termlet-gist","version":1,"parts":["../x"]}"#)
                .unwrap_err(),
            SyncError::Corrupt
        );
        assert!(parse_manifest(&empty_manifest()).unwrap().parts.is_empty());
    }

    #[test]
    fn too_large_for_a_gist() {
        let data = vec![0u8; MAX_SIZE + 1];
        assert_eq!(
            encode(&data).unwrap_err(),
            SyncError::GistTooLarge {
                size: MAX_SIZE as u64 + 1,
                limit: MAX_SIZE as u64
            }
        );
        // 上限以内的段数远少于 300 个文件
        let layout = encode(&vec![0u8; MAX_SIZE]).unwrap();
        assert!(layout.parts.len() < 20, "{}", layout.parts.len());
    }

    #[test]
    fn gist_ids_and_part_names() {
        assert_eq!(parse_gist_id("  ").unwrap(), "");
        assert_eq!(
            parse_gist_id("https://gist.github.com/alice/AA0f3c9e2b5d4e1f8a7b6c5d4e3f2a1b/")
                .unwrap(),
            "aa0f3c9e2b5d4e1f8a7b6c5d4e3f2a1b"
        );
        assert_eq!(
            parse_gist_id("https://gist.github.com/aa0f3c9e2b5d4e1f8a7b?file=x#y").unwrap(),
            "aa0f3c9e2b5d4e1f8a7b"
        );
        assert_eq!(
            parse_gist_id("not a gist").unwrap_err(),
            SyncError::invalid("gist", "invalid")
        );
        assert!(is_part_name("termlet-sync.000.b64") && is_part_name(&part_name(11)));
        assert!(!is_part_name("termlet-sync.json") && !is_part_name("notes.md"));
        assert!(!is_part_name("termlet-sync.1000.b64"));
    }
}
