//! 远程配置：发布方不用发新版就能更换更新源、暂停更新、设置崩溃报告的接收地址、暂停或改换使用统计的接收地址。
//!
//! `https://dl.termlet.dev/update.json`（下载站；原文在仓库的 `packaging/remote-config/update.json`，由手动运行的
//! `.github/workflows/remote-config.yml` 签名并用 `node scripts/release.mjs publish-config` 上传）：
//!
//! ```json
//! { "feeds": ["https://…"], "disabled": false, "message": "…", "crashReportUrl": "https://…",
//!   "usageUrl": "https://api.termlet.dev/v1/stats" }
//! ```
//!
//! - 每次使用时读取（5 秒超时），不可用时为 `None`，调用方使用内置值。
//! - 与更新文件一样要有签名（`update.json.minisig`，见 [`crate::update::signature`]）：没有或不对时整份忽略。
//! - 签名带签发与失效时间：过期的、比已采用过的更早签发的同样整份忽略（重放旧的、签名有效的配置，
//!   例如已经撤销的 `disabled: true`，不会生效）；采用过的最新签发时间记在数据目录的 `remote-config.json`。
//! - 各字段按类型校验，写错的项当作没有；更新源只接受内置源同一来源下的地址（[`trusted_feeds`]）。
//! - `usageUrl`（使用统计，见 [`crate::usage`]）：没有这一项时用内置地址；为 `"off"` 或空字符串时所有客户端停发
//!   （发布方的总开关）；为别的地址时只接受 [`USAGE_TRUSTED_PREFIXES`] 下的 https 地址（不带账号、查询参数与 `#`），
//!   写错的当作没有。
//! - 发布方用 `node scripts/release.mjs sign-config` 签名（默认 90 天有效，最长 366 天）。

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;
use url::Url;

use crate::paths::DataDir;
use crate::update::http::{self, Fetched};
use crate::update::keys::UPDATE_PUBLIC_KEYS;
use crate::update::signature::{iso_seconds, verify_remote_config};

/// 远程配置的地址（签名在同一目录的 `update.json.minisig`）。
pub const REMOTE_CONFIG_URL: &str = "https://dl.termlet.dev/update.json";
const TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BYTES: usize = 64 * 1024;
/// 远程配置给出的使用统计接收地址只接受这些前缀下的：远程配置有签名，这里再限制去向，统计不会被改发到别处。
pub const USAGE_TRUSTED_PREFIXES: &[&str] = &["https://api.termlet.dev/"];

/// 远程配置。各字段都可以没有。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteConfig {
    /// 更新源（只采用 [`trusted_feeds`] 接受的），排在内置地址之前
    pub feeds: Option<Vec<String>>,
    /// 为 true 时暂停更新，界面显示 `message`
    pub disabled: Option<bool>,
    pub message: Option<String>,
    /// 崩溃报告的接收地址（`crashReportUrl`）
    pub crash_report_url: Option<String>,
    /// 使用统计的接收地址（`usageUrl`），见 [`RemoteConfig::usage_endpoint`]
    pub usage_url: Option<String>,
}

/// 远程配置对使用统计的接收地址的要求。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UsageEndpoint {
    /// 没有这一项（或写错了）：用内置地址
    Builtin,
    /// `"off"` 或空字符串：发布方暂停发送
    Paused,
    /// 改用这个地址
    Url(Url),
}

impl RemoteConfig {
    /// 使用统计的接收地址：没有 `usageUrl` 时用内置地址；为 `"off"` 或空字符串时暂停；为别的地址时只接受
    /// [`USAGE_TRUSTED_PREFIXES`] 下的 https 地址（规范化之后比较；不带账号、查询参数与 `#`），不接受的当作没有。
    pub fn usage_endpoint(&self) -> UsageEndpoint {
        let Some(value) = self.usage_url.as_deref().map(str::trim) else {
            return UsageEndpoint::Builtin;
        };
        if value.is_empty() || value == "off" {
            return UsageEndpoint::Paused;
        }
        let url = Url::parse(value).ok().filter(|u| {
            u.scheme() == "https"
                && u.username().is_empty()
                && u.password().is_none()
                && u.query().is_none()
                && u.fragment().is_none()
                && USAGE_TRUSTED_PREFIXES
                    .iter()
                    .any(|prefix| u.as_str().starts_with(prefix))
        });
        match url {
            Some(url) => UsageEndpoint::Url(url),
            None => {
                tracing::warn!(
                    url = value,
                    "远程配置中的使用统计地址不在白名单中，改用内置地址"
                );
                UsageEndpoint::Builtin
            }
        }
    }

    /// 可用的崩溃报告接收地址：只接受 https、不带账号的地址。没有时设置中不显示“自动发送崩溃报告”。
    pub fn crash_report_endpoint(&self) -> Option<&str> {
        let url = self.crash_report_url.as_deref()?;
        let parsed = Url::parse(url).ok()?;
        (parsed.scheme() == "https"
            && parsed.host_str().is_some_and(|h| !h.is_empty())
            && parsed.username().is_empty()
            && parsed.password().is_none())
        .then_some(url)
    }
}

/// 从哪里读取远程配置，以及签发时间记在哪里。
#[derive(Clone, Debug)]
pub struct RemoteConfigSource {
    url: Url,
    signature_url: Url,
    state_file: PathBuf,
    keys: Vec<String>,
}

impl RemoteConfigSource {
    /// 正式的远程配置，签发时间记在数据目录中。
    pub fn new(data_dir: &DataDir) -> Self {
        Self::with(
            Url::parse(REMOTE_CONFIG_URL).expect("内置的远程配置地址"),
            data_dir.remote_config_state(),
            UPDATE_PUBLIC_KEYS.iter().map(|k| k.to_string()).collect(),
        )
    }

    /// 指定地址、记录文件与公钥（测试用）。
    pub fn with(url: Url, state_file: PathBuf, keys: Vec<String>) -> Self {
        let mut signature_url = url.clone();
        signature_url.set_path(&format!("{}.minisig", url.path()));
        Self {
            url,
            signature_url,
            state_file,
            keys,
        }
    }

    /// 读取远程配置。不可用、没有签名、签名不对、过期、重放旧配置时为 `None`（原因写进日志）。
    pub async fn fetch(&self) -> Option<RemoteConfig> {
        let (body, signature) = tokio::join!(
            http::get_text(&self.url, TIMEOUT, MAX_BYTES),
            http::get_text(&self.signature_url, TIMEOUT, MAX_BYTES),
        );
        let body = match body {
            Ok(Fetched::Body(text)) => text,
            Ok(Fetched::Status(status)) => {
                tracing::info!(status, "远程配置不可用");
                return None;
            }
            Err(e) => {
                // 离线或被拦截：使用内置值
                tracing::info!("读取远程配置失败：{e}");
                return None;
            }
        };
        let signature = match signature {
            Ok(Fetched::Body(text)) => text,
            Ok(Fetched::Status(status)) => {
                tracing::warn!(status, "远程配置没有签名，已忽略");
                return None;
            }
            Err(e) => {
                tracing::info!("读取远程配置的签名失败：{e}");
                return None;
            }
        };
        let seen = self.last_issued();
        let (config, issued) =
            read_remote_config(&body, &signature, &self.keys, crate::time::now_ms(), seen)?;
        if issued > seen {
            self.save_issued(issued);
        }
        Some(config)
    }

    fn last_issued(&self) -> i64 {
        std::fs::read_to_string(&self.state_file)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|v| v.get("issued").and_then(Value::as_i64))
            .unwrap_or(0)
    }

    fn save_issued(&self, issued: i64) {
        let text = serde_json::json!({ "issued": issued }).to_string();
        if let Err(e) = crate::platform::write_private_file(&self.state_file, text.as_bytes()) {
            tracing::warn!("记录远程配置的签发时间失败：{e}");
        }
    }
}

/// 校验签名后解析，返回配置与签发时间。签名不对、已过期、比 `last_issued` 更早签发、不是 JSON 时整份忽略。
/// 同一份配置重复读取（签发时间相同）照常采用。
pub fn read_remote_config(
    text: &str,
    signature: &str,
    keys: &[impl AsRef<str>],
    now_ms: i64,
    last_issued: i64,
) -> Option<(RemoteConfig, i64)> {
    let validity = match verify_remote_config(text.as_bytes(), signature, keys) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("远程配置的签名校验没有通过，已忽略：{e}");
            return None;
        }
    };
    if now_ms >= validity.expires {
        tracing::warn!(expires = %iso_seconds(validity.expires), "远程配置已过期，已忽略");
        return None;
    }
    if validity.issued < last_issued {
        tracing::warn!(
            issued = %iso_seconds(validity.issued),
            "远程配置比已采用过的更早签发（可能是重放的旧配置），已忽略"
        );
        return None;
    }
    let Ok(raw) = serde_json::from_str::<Value>(text) else {
        tracing::warn!("远程配置不是有效的 JSON，已忽略");
        return None;
    };
    Some((parse_remote_config(&raw), validity.issued))
}

/// 只保留类型正确的字段：写错的项当作没有，不能让调用方出错。
pub fn parse_remote_config(raw: &Value) -> RemoteConfig {
    let Some(r) = raw.as_object() else {
        tracing::warn!("远程配置的格式不对，已忽略");
        return RemoteConfig::default();
    };
    let mut cfg = RemoteConfig::default();
    let mut wrong = Vec::new();
    match r.get("feeds") {
        Some(Value::Array(items)) => {
            cfg.feeds = Some(
                items
                    .iter()
                    .filter_map(|f| f.as_str().map(str::to_string))
                    .collect(),
            );
        }
        Some(_) => wrong.push("feeds"),
        None => {}
    }
    match r.get("disabled") {
        Some(Value::Bool(b)) => cfg.disabled = Some(*b),
        Some(_) => wrong.push("disabled"),
        None => {}
    }
    match r.get("message") {
        Some(Value::String(s)) => cfg.message = Some(s.clone()),
        Some(_) => wrong.push("message"),
        None => {}
    }
    match r.get("crashReportUrl") {
        Some(Value::String(s)) => cfg.crash_report_url = Some(s.clone()),
        Some(_) => wrong.push("crashReportUrl"),
        None => {}
    }
    match r.get("usageUrl") {
        Some(Value::String(s)) => cfg.usage_url = Some(s.clone()),
        Some(_) => wrong.push("usageUrl"),
        None => {}
    }
    if !wrong.is_empty() {
        tracing::warn!(fields = %wrong.join(", "), "远程配置中这些字段的类型不对，已忽略");
    }
    cfg
}

/// 远程配置给出的更新源只接受 `trusted` 前缀下的 https 地址（按 URL 规范化之后比较，`../`、`%2e%2e`、
/// 反斜杠、非默认端口都绕不过去；不能带账号、查询参数或 `#`），其余忽略并记日志。返回去掉末尾 `/` 的地址。
pub fn trusted_feeds(feeds: Option<&[String]>, trusted: &[impl AsRef<str>]) -> Vec<String> {
    let mut out = Vec::new();
    for feed in feeds.unwrap_or_default() {
        let href = Url::parse(feed)
            .ok()
            .filter(|u| {
                u.scheme() == "https"
                    && u.username().is_empty()
                    && u.password().is_none()
                    && u.query().is_none()
                    && u.fragment().is_none()
            })
            .map(|u| u.as_str().trim_end_matches('/').to_string());
        match href {
            Some(href)
                if trusted
                    .iter()
                    .any(|prefix| format!("{href}/").starts_with(prefix.as_ref())) =>
            {
                out.push(href);
            }
            _ => tracing::warn!(feed, "远程配置中的更新源不在白名单中，已忽略"),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::signature::{manifest_comment, parse_iso_seconds, remote_config_comment};
    use crate::update::test_support::{TestKey, TestServer};

    const DAY: i64 = 24 * 60 * 60 * 1000;
    const TEXT: &str = "{\"disabled\":true,\"message\":\"暂停更新\"}\n";

    fn issued() -> i64 {
        parse_iso_seconds("2026-10-15T08:00:00Z").unwrap()
    }

    fn signed(key: &TestKey, body: &str, from: i64, days: i64) -> String {
        key.sign(
            body.as_bytes(),
            &remote_config_comment(from, from + days * DAY),
        )
    }

    fn paused() -> RemoteConfig {
        RemoteConfig {
            disabled: Some(true),
            message: Some("暂停更新".into()),
            ..Default::default()
        }
    }

    #[test]
    fn reads_valid_config_with_issue_time() {
        let key = TestKey::random();
        let keys = [key.public_key()];
        let sig = signed(&key, TEXT, issued(), 90);
        assert_eq!(
            read_remote_config(TEXT, &sig, &keys, issued() + DAY, 0),
            Some((paused(), issued()))
        );
    }

    #[test]
    fn ignores_bad_signatures_and_non_json() {
        let key = TestKey::random();
        let keys = [key.public_key()];
        let now = issued() + DAY;
        let sig = signed(&key, TEXT, issued(), 90);
        assert_eq!(
            read_remote_config(&TEXT.replace("true", "false"), &sig, &keys, now, 0),
            None
        );
        // 拿别的文件的签名、没有时间的旧格式
        let other = key.sign(TEXT.as_bytes(), &manifest_comment("latest.yml", "1.0.0"));
        assert_eq!(read_remote_config(TEXT, &other, &keys, now, 0), None);
        let untimed = key.sign(TEXT.as_bytes(), "termlet-remote-config");
        assert_eq!(read_remote_config(TEXT, &untimed, &keys, now, 0), None);
        // 没有内置公钥
        let none: [&str; 0] = [];
        assert_eq!(read_remote_config(TEXT, &sig, &none, now, 0), None);
        let not_json = "disabled: true\n";
        let sig = signed(&key, not_json, issued(), 90);
        assert_eq!(read_remote_config(not_json, &sig, &keys, now, 0), None);
    }

    #[test]
    fn ignores_expired_config() {
        let key = TestKey::random();
        let keys = [key.public_key()];
        let sig = signed(&key, TEXT, issued(), 30);
        assert!(read_remote_config(TEXT, &sig, &keys, issued() + 30 * DAY - 1, 0).is_some());
        assert_eq!(
            read_remote_config(TEXT, &sig, &keys, issued() + 30 * DAY, 0),
            None
        );
    }

    #[test]
    fn ignores_replayed_older_config() {
        let key = TestKey::random();
        let keys = [key.public_key()];
        let now = issued() + DAY;
        let later = issued() + 60_000;
        let old = signed(&key, TEXT, issued(), 90);
        assert_eq!(read_remote_config(TEXT, &old, &keys, now, later), None);
        let same = signed(&key, TEXT, later, 90);
        assert_eq!(
            read_remote_config(TEXT, &same, &keys, now, later),
            Some((paused(), later))
        );
    }

    #[test]
    fn keeps_only_fields_of_the_right_type() {
        let cfg = parse_remote_config(&serde_json::json!({
            "feeds": "https://github.com/termlet/termlet/releases/latest/download",
            "disabled": "yes",
            "message": 1,
            "crashReportUrl": "https://crash.example.com",
        }));
        assert_eq!(
            cfg,
            RemoteConfig {
                crash_report_url: Some("https://crash.example.com".into()),
                ..Default::default()
            }
        );
        let cfg = parse_remote_config(&serde_json::json!({
            "feeds": ["https://a", 1, null], "disabled": true, "message": "m",
        }));
        assert_eq!(
            cfg,
            RemoteConfig {
                feeds: Some(vec!["https://a".into()]),
                disabled: Some(true),
                message: Some("m".into()),
                ..Default::default()
            }
        );
        for bad in [
            Value::Null,
            serde_json::json!([]),
            serde_json::json!("text"),
            serde_json::json!(42),
        ] {
            assert_eq!(parse_remote_config(&bad), RemoteConfig::default());
        }
    }

    #[test]
    fn crash_report_endpoint_requires_https() {
        let with = |url: &str| RemoteConfig {
            crash_report_url: Some(url.into()),
            ..Default::default()
        };
        assert_eq!(
            with("https://crash.example.com/report").crash_report_endpoint(),
            Some("https://crash.example.com/report")
        );
        for bad in [
            "http://crash.example.com",
            "https://user:pass@crash.example.com",
            "not a url",
            "",
        ] {
            assert_eq!(with(bad).crash_report_endpoint(), None, "{bad}");
        }
        assert_eq!(RemoteConfig::default().crash_report_endpoint(), None);
    }

    #[test]
    fn usage_url_is_parsed_and_covered_by_the_signature() {
        let key = TestKey::random();
        let keys = [key.public_key()];
        let text = "{\"usageUrl\":\"off\"}\n";
        let sig = signed(&key, text, issued(), 90);
        let (cfg, _) = read_remote_config(text, &sig, &keys, issued() + DAY, 0).unwrap();
        assert_eq!(cfg.usage_url.as_deref(), Some("off"));
        assert_eq!(cfg.usage_endpoint(), UsageEndpoint::Paused);
        // 改了 usageUrl 签名就对不上（不能借别的签名把统计改发到别处或重新打开）
        let changed = "{\"usageUrl\":\"https://api.termlet.dev/v2/stats\"}\n";
        assert_eq!(
            read_remote_config(changed, &sig, &keys, issued() + DAY, 0),
            None
        );
        // 类型不对的当作没有
        let cfg = parse_remote_config(&serde_json::json!({ "usageUrl": false }));
        assert_eq!(cfg.usage_url, None);
        assert_eq!(cfg.usage_endpoint(), UsageEndpoint::Builtin);
    }

    #[test]
    fn usage_endpoint_rules() {
        let with = |url: &str| RemoteConfig {
            usage_url: Some(url.into()),
            ..Default::default()
        };
        assert_eq!(
            RemoteConfig::default().usage_endpoint(),
            UsageEndpoint::Builtin
        );
        for paused in ["off", "", "  ", " off "] {
            assert_eq!(
                with(paused).usage_endpoint(),
                UsageEndpoint::Paused,
                "{paused:?}"
            );
        }
        for good in [
            "https://api.termlet.dev/v1/stats",
            "https://API.termlet.dev:443/v2/stats",
        ] {
            assert!(
                matches!(with(good).usage_endpoint(), UsageEndpoint::Url(u) if u.host_str() == Some("api.termlet.dev")),
                "{good}"
            );
        }
        for bad in [
            "http://api.termlet.dev/v1/stats",
            "https://evil.example.com/v1/stats",
            "https://api.termlet.dev.evil.com/v1/stats",
            "https://api.termlet.dev@evil.com/v1/stats",
            "https://user:pass@api.termlet.dev/v1/stats",
            "https://api.termlet.dev:8443/v1/stats",
            "https://api.termlet.dev/v1/stats?id=1",
            "https://api.termlet.dev/v1/stats#x",
            "not a url",
            "OFF",
        ] {
            assert_eq!(with(bad).usage_endpoint(), UsageEndpoint::Builtin, "{bad}");
        }
    }

    /// 内置的白名单（下载站与 GitHub Releases）。
    const TRUSTED: &[&str] = crate::update::TRUSTED_FEED_PREFIXES;

    #[test]
    fn trusted_feeds_are_normalized() {
        let feeds: Vec<String> = [
            "https://dl.termlet.dev",
            "https://dl.termlet.dev/",
            "https://DL.termlet.dev:443/beta/",
            "https://dl.termlet.dev/a/../b",
            "https://github.com/termlet/termlet/releases/latest/download/",
            "https://github.com/termlet/termlet/releases/download/v1.2.0",
            "https://GITHUB.com:443/termlet/termlet/releases/download/v1.2.1",
        ]
        .map(String::from)
        .into();
        assert_eq!(
            trusted_feeds(Some(&feeds), TRUSTED),
            [
                "https://dl.termlet.dev",
                "https://dl.termlet.dev",
                "https://dl.termlet.dev/beta",
                "https://dl.termlet.dev/b",
                "https://github.com/termlet/termlet/releases/latest/download",
                "https://github.com/termlet/termlet/releases/download/v1.2.0",
                "https://github.com/termlet/termlet/releases/download/v1.2.1",
            ]
        );
        assert!(trusted_feeds(None, TRUSTED).is_empty());
        // 内置的更新源本身在白名单中
        let builtin: Vec<String> = crate::update::BUILTIN_FEEDS
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(trusted_feeds(Some(&builtin), TRUSTED), builtin);
    }

    #[test]
    fn untrusted_feeds_are_rejected() {
        let feeds: Vec<String> = [
            "https://evil.example.com/termlet/termlet/releases/latest/download",
            "https://github.com/attacker/termlet/releases/latest/download",
            "https://github.com.evil.com/termlet/termlet/releases/latest/download",
            "https://github.com@evil.com/termlet/termlet/releases/latest/download",
            "https://user:pass@github.com/termlet/termlet/releases/latest/download",
            "http://github.com/termlet/termlet/releases/latest/download",
            "https://github.com:8443/termlet/termlet/releases/latest/download",
            "https://github.com/termlet/termlet/releases/latest/download?x=1",
            "https://github.com/termlet/termlet/releases/latest/download#x",
            "https://github.com/termlet/termlet/releasesX/latest/download",
            "https://github.com/termlet/termlet/releases/../../../attacker/x/releases/latest/download",
            "https://github.com/termlet/termlet/releases/%2e%2e/%2e%2e/%2e%2e/attacker/x/releases/a",
            "https://github.com/termlet/termlet/releases/..\\..\\..\\attacker/x/releases/a",
            // 下载站：别的主机、相似的主机名、账号、http、别的端口、参数、跳到别的主机
            "https://dl.termlet.dev.evil.com/",
            "https://dl.termlet.devx/",
            "https://xdl.termlet.dev/",
            "https://termlet.dev/",
            "https://api.termlet.dev/",
            "https://evil.com/dl.termlet.dev/",
            "https://dl.termlet.dev@evil.com/",
            "https://dl.termlet.dev%40evil.com/",
            "https://user:pass@dl.termlet.dev/",
            "http://dl.termlet.dev/",
            "https://dl.termlet.dev:8443/",
            "https://dl.termlet.dev/?x=1",
            "https://dl.termlet.dev/#x",
            "//evil.com/dl.termlet.dev/",
            "https:/\\evil.com/",
            "not a url",
        ]
        .map(String::from)
        .into();
        assert!(trusted_feeds(Some(&feeds), TRUSTED).is_empty());
    }

    /// 下载站的防火墙只放行不带参数的 /update.json 与 /update.json.minisig。
    #[test]
    fn builtin_source_is_on_the_download_site() {
        let source = RemoteConfigSource::with(
            Url::parse(REMOTE_CONFIG_URL).unwrap(),
            PathBuf::new(),
            Vec::new(),
        );
        assert_eq!(source.url.as_str(), "https://dl.termlet.dev/update.json");
        assert_eq!(
            source.signature_url.as_str(),
            "https://dl.termlet.dev/update.json.minisig"
        );
    }

    #[tokio::test]
    async fn fetches_from_server_and_records_issue_time() {
        let server = TestServer::start().await;
        let key = TestKey::random();
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("remote-config.json");
        let source = RemoteConfigSource::with(
            Url::parse(&server.url("/update.json")).unwrap(),
            state.clone(),
            vec![key.public_key()],
        );
        // 没有配置、没有签名时为 None
        assert_eq!(source.fetch().await, None);
        server.put("/update.json", TEXT);
        assert_eq!(source.fetch().await, None);

        let now = crate::time::now_ms() / 1000 * 1000;
        server.put("/update.json.minisig", signed(&key, TEXT, now, 90));
        assert_eq!(source.fetch().await, Some(paused()));
        let recorded: Value =
            serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
        assert_eq!(recorded["issued"].as_i64(), Some(now));

        // 之后换回更早签发的配置（重放）：不采用
        let older = "{\"disabled\":false}\n";
        server.put("/update.json", older);
        server.put("/update.json.minisig", signed(&key, older, now - DAY, 90));
        assert_eq!(source.fetch().await, None);
        assert!(
            server
                .requests()
                .iter()
                .any(|r| r == "/update.json.minisig")
        );
    }
}
