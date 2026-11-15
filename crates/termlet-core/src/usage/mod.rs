//! 匿名使用统计（方案见 `docs/rust/使用统计方案.md`，已由用户定稿）。
//!
//! - **发什么**：每个 UTC 日最多一条 `POST https://api.termlet.dev/v1/stats`，正文是 [`UsageMessage`] 的 9 个字段
//!   （格式版本、Termlet 版本、系统、系统大版本、CPU 架构、界面语言、使用时长的档位、本周 / 本月第一次），没有任何 ID、
//!   时间与时区。国家或地区由接收端按 Cloudflare 给出的代码另记，客户端不发。正文只由 [`UsageMessage::to_json`] 生成，
//!   “查看内容”对话框显示的（[`UsageReporter::preview`]）与实际发出的是同一个函数的结果。
//! - **本机记什么**：`{数据目录}/usage.json` 只有两个 UTC 日期（[`UsageState`]：第一次与上次发送成功），不记时刻、
//!   历史与服务器的回应；每次发送成功在程序日志中写一行，含发出的 JSON 原文。关闭后删除这个文件（[`UsageReporter::forget`]）。
//! - **什么时候发**（[`UsageReporter::check`]，全部满足）：设置开着（默认开，不另外提示，安装包的许可说明、隐私说明与设置中写明）；
//!   不是调试构建；没有被环境变量（[`DO_NOT_TRACK_ENV`]、[`USAGE_URL_ENV`]）或远程配置（`usageUrl`）关掉；今天用过
//!   （[`mark_active`]：窗口获得焦点、处理命令行工具的请求）；今天还没有发送成功。“今天”按 UTC 算，与接收端计数的日期一致。
//! - **失败**：网络错误、10 秒超时、HTTP 5xx、408、429 留到下一次检查（界面每小时检查一次）；其他 4xx（还有 3xx：不跟随
//!   重定向）当天不再试。跨过 UTC 零点没发成功的不补发前一天的。同一天只在日志中记第一次失败。
//! - **去向**（[`Route`]）：环境变量 `TERMLET_USAGE_URL` 指定的地址（`off` 关闭；调试构建只接受本机回环地址，测试用）>
//!   远程配置的 `usageUrl`（`off` 为发布方暂停，见 [`crate::remote_config::RemoteConfig::usage_endpoint`]）> 内置地址
//!   [`USAGE_URL`]。远程配置不可用（离线、GitHub Pages 不通）时照样用内置地址发。
//!
//! 命令行工具 `termlet` 不发统计；由它拉起的后台 Termlet 处理请求时算作当天用过。

#[cfg(test)]
mod tests;

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use chrono::{Datelike as _, NaiveDate};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use url::Url;

use crate::paths::DataDir;
use crate::remote_config::{RemoteConfigSource, UsageEndpoint};
use crate::update::http::is_loopback;

/// 内置的接收地址。
pub const USAGE_URL: &str = "https://api.termlet.dev/v1/stats";
/// 改接收地址的环境变量；值为 `off` 时不发送（公司可以用组策略统一设置）。
pub const USAGE_URL_ENV: &str = "TERMLET_USAGE_URL";
/// [Console Do Not Track](https://consoledonottrack.com/) 的约定：设为 `1`（`0`、`false` 以外的值）时不发送。
pub const DO_NOT_TRACK_ENV: &str = "DO_NOT_TRACK";
/// 官网的隐私说明（中文；英文界面用 [`PRIVACY_URL_EN`]）。末尾带斜杠：官网的地址都以斜杠结尾，不带会多跳转一次。
pub const PRIVACY_URL: &str = "https://termlet.dev/privacy/";
/// 官网英文的隐私说明。
pub const PRIVACY_URL_EN: &str = "https://termlet.dev/en/privacy/";
/// 正文的格式版本（`v`）。
pub const FORMAT: u32 = 1;

/// 一次发送的时限。
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// `usage.json` 的格式版本。
const STATE_FORMAT: u32 = 1;

// ---------------------------------------------------------------- 日期

/// Unix 毫秒所在的 UTC 日期。
pub fn utc_day(now_ms: i64) -> NaiveDate {
    chrono::DateTime::from_timestamp_millis(now_ms)
        .map(|t| t.date_naive())
        .unwrap_or_default()
}

/// 使用时长的档位：从本机第一次发送成功的 UTC 日期算起（参照 Fedora 的 countme）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Age {
    /// 当天（还没有发送成功过的也算）
    New,
    /// 1–6 天
    W1,
    /// 7–29 天
    M1,
    /// 30–179 天
    M6,
    /// 180 天以上
    Old,
}

/// 第一次发送成功的日期为 `since` 时，`today` 的档位；`since` 比今天晚（时钟往回调过）时算作当天。
pub fn age(since: Option<NaiveDate>, today: NaiveDate) -> Age {
    match since.map_or(0, |since| (today - since).num_days()) {
        ..=0 => Age::New,
        1..=6 => Age::W1,
        7..=29 => Age::M1,
        30..=179 => Age::M6,
        _ => Age::Old,
    }
}

/// 是不是本周（ISO 周，周一开始，UTC）第一次发送：上次发送在别的周、没有发送过、或比今天晚（时钟往回调过）时为真。
pub fn first_of_week(last_sent: Option<NaiveDate>, today: NaiveDate) -> bool {
    match last_sent {
        Some(last) if last <= today => last.iso_week() != today.iso_week(),
        _ => true,
    }
}

/// 是不是本月（UTC）第一次发送，规则同 [`first_of_week`]。
pub fn first_of_month(last_sent: Option<NaiveDate>, today: NaiveDate) -> bool {
    match last_sent {
        Some(last) if last <= today => (last.year(), last.month()) != (today.year(), today.month()),
        _ => true,
    }
}

// ---------------------------------------------------------------- 正文

/// 一条使用统计（接收端 `termlet-api` 的 `src/stats/message.ts` 按同样的格式逐项校验）。字段的顺序就是正文中的顺序。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageMessage {
    /// 格式版本（[`FORMAT`]）
    pub v: u32,
    /// Termlet 的版本（含预发布后缀）
    pub app: String,
    /// `windows`、`macos`、`linux`
    pub os: String,
    /// 系统的大版本（[`crate::diag::coarse_version`]）：`11 25H2`、`15`、`ubuntu 24.04`、`other`
    pub os_version: String,
    /// `x86_64`、`aarch64`
    pub arch: String,
    /// 实际使用的界面语言：[`LANGS`] 之一（其余语言按 `en`）
    pub lang: String,
    pub age: Age,
    pub first_of_week: bool,
    pub first_of_month: bool,
}

impl UsageMessage {
    /// 发出去的正文（紧凑的 JSON）。发送与“查看内容”都用它，两边逐字节相同。
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// 发统计的平台（系统、大版本、架构）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Platform {
    pub os: String,
    pub os_version: String,
    pub arch: String,
}

impl Platform {
    /// 这台电脑；不是 Windows、macOS、Linux 或不是 x86_64、aarch64 时为 `None`（接收端不认识，不发）。
    pub fn current() -> Option<Self> {
        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;
        let known =
            matches!(os, "windows" | "macos" | "linux") && matches!(arch, "x86_64" | "aarch64");
        known.then(|| Self {
            os: os.into(),
            os_version: crate::diag::coarse_version().into(),
            arch: arch.into(),
        })
    }
}

/// 接收端认的界面语言（它的白名单）。加别的语言时先改接收端（termlet-api 的 `LANG`）并部署，再发布发送新值的客户端：
/// 顺序反了，新客户端的统计整条被拒（400），当天不再发。`ja`、`ko` 是后加的，0.3.0 及以前的客户端按 `en` 发。
pub const LANGS: [&str; 5] = ["zh-CN", "zh-TW", "en", "ja", "ko"];

/// 生成今天（`today`，UTC）要发送的统计（纯函数）。`lang` 不在 [`LANGS`] 中时按 `en`。
pub fn build_message(
    platform: &Platform,
    lang: &str,
    state: Option<&UsageState>,
    today: NaiveDate,
) -> UsageMessage {
    let last_sent = state.map(|s| s.last_sent);
    let lang = if LANGS.contains(&lang) { lang } else { "en" };
    UsageMessage {
        v: FORMAT,
        app: crate::VERSION.into(),
        os: platform.os.clone(),
        os_version: platform.os_version.clone(),
        arch: platform.arch.clone(),
        lang: lang.into(),
        age: age(state.map(|s| s.since), today),
        first_of_week: first_of_week(last_sent, today),
        first_of_month: first_of_month(last_sent, today),
    }
}

// ---------------------------------------------------------------- 本机的两个日期

/// `usage.json` 的内容：第一次与上次发送成功的 UTC 日期。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsageState {
    pub since: NaiveDate,
    pub last_sent: NaiveDate,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredState {
    v: u32,
    since: String,
    last_sent: String,
}

const DATE_FORMAT: &str = "%Y-%m-%d";

impl UsageState {
    /// 解析 `usage.json`；格式不对时为 `None`（当作从头开始）。
    pub fn parse(text: &str) -> Option<Self> {
        let stored: StoredState = serde_json::from_str(text).ok()?;
        let date = |s: &str| NaiveDate::parse_from_str(s, DATE_FORMAT).ok();
        Some(Self {
            since: date(&stored.since)?,
            last_sent: date(&stored.last_sent)?,
        })
    }

    /// 写进 `usage.json` 的内容：`{"v":1,"since":"2026-10-29","lastSent":"2026-11-02"}`。
    pub fn to_json(&self) -> String {
        serde_json::to_string(&StoredState {
            v: STATE_FORMAT,
            since: self.since.format(DATE_FORMAT).to_string(),
            last_sent: self.last_sent.format(DATE_FORMAT).to_string(),
        })
        .unwrap_or_default()
    }

    /// 今天发送成功之后的记录：第一次的日期不变（没有、或比今天晚时为今天），上次发送为今天。
    pub fn after_sent(state: Option<&Self>, today: NaiveDate) -> Self {
        Self {
            since: state
                .map(|s| s.since)
                .filter(|since| *since <= today)
                .unwrap_or(today),
            last_sent: today,
        }
    }
}

// ---------------------------------------------------------------- 今天是否用过

/// 记下哪一天（UTC）用过 Termlet；变成新的一天时通知订阅者（界面据此马上检查一次）。
pub struct ActivityTracker {
    day: watch::Sender<Option<NaiveDate>>,
}

impl Default for ActivityTracker {
    fn default() -> Self {
        Self {
            day: watch::channel(None).0,
        }
    }
}

impl ActivityTracker {
    /// 记下 `now_ms` 那一天用过；是这一天第一次记下时返回 true。
    pub fn mark(&self, now_ms: i64) -> bool {
        let today = utc_day(now_ms);
        self.day.send_if_modified(|day| {
            if *day == Some(today) {
                false
            } else {
                *day = Some(today);
                true
            }
        })
    }

    /// `now_ms` 那一天用过没有。
    pub fn is_active(&self, now_ms: i64) -> bool {
        *self.day.borrow() == Some(utc_day(now_ms))
    }

    pub fn subscribe(&self) -> watch::Receiver<Option<NaiveDate>> {
        self.day.subscribe()
    }
}

static ACTIVITY: LazyLock<ActivityTracker> = LazyLock::new(ActivityTracker::default);

/// 记下今天（UTC）用过 Termlet：主窗口获得焦点、处理命令行工具的请求时调用。
pub fn mark_active() {
    ACTIVITY.mark(crate::time::now_ms());
}

/// 今天（`now_ms` 那一天）用过没有。
pub fn is_active(now_ms: i64) -> bool {
    ACTIVITY.is_active(now_ms)
}

/// 订阅“新的一天第一次用到”：值为最近用过的 UTC 日期。
pub fn subscribe_active() -> watch::Receiver<Option<NaiveDate>> {
    ACTIVITY.subscribe()
}

// ---------------------------------------------------------------- 去向

/// 由环境变量与构建方式定下的去向（启动时定一次）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// 读远程配置的 `usageUrl`，没有时用内置地址
    Default,
    /// 环境变量 [`USAGE_URL_ENV`] 指定的地址（不读远程配置）
    Fixed(Url),
    /// 被环境变量关闭（变量名）
    EnvOff(&'static str),
    /// 调试构建不发（环境变量指向本机回环地址时除外）
    DebugBuild,
}

impl Route {
    /// 固定发到 `url`（https，或本机回环地址上的 http，测试用）；地址不可用时为 `None`。
    pub fn fixed(url: &str) -> Option<Self> {
        acceptable_url(url).map(Route::Fixed)
    }

    /// 按这个进程的环境变量与构建方式。
    pub fn from_env() -> Self {
        Self::resolve(
            std::env::var(DO_NOT_TRACK_ENV).ok().as_deref(),
            std::env::var(USAGE_URL_ENV).ok().as_deref(),
            cfg!(debug_assertions),
        )
    }

    /// `dnt`、`url` 为两个环境变量的值，`debug` 为是否调试构建（纯函数）。`DO_NOT_TRACK` 优先；`TERMLET_USAGE_URL`
    /// 为 `off` 或不是可用的地址（https，或本机回环地址上的 http；不带账号）时关闭。
    pub fn resolve(dnt: Option<&str>, url: Option<&str>, debug: bool) -> Self {
        let dnt = dnt.map(str::trim).unwrap_or_default();
        if !dnt.is_empty() && dnt != "0" && !dnt.eq_ignore_ascii_case("false") {
            return Route::EnvOff(DO_NOT_TRACK_ENV);
        }
        match url.map(str::trim).filter(|v| !v.is_empty()) {
            Some(value) if value.eq_ignore_ascii_case("off") => Route::EnvOff(USAGE_URL_ENV),
            Some(value) => match acceptable_url(value) {
                Some(url) if !debug || is_loopback(&url) => Route::Fixed(url),
                Some(_) => Route::DebugBuild,
                None => {
                    tracing::warn!(value, "{USAGE_URL_ENV} 不是可用的地址，不发送使用统计");
                    Route::EnvOff(USAGE_URL_ENV)
                }
            },
            None if debug => Route::DebugBuild,
            None => Route::Default,
        }
    }
}

/// https 地址，或本机回环地址上的 http 地址（测试用）；要有主机名、不带账号。
fn acceptable_url(value: &str) -> Option<Url> {
    let url = Url::parse(value).ok()?;
    let no_account = url.username().is_empty() && url.password().is_none();
    let has_host = url.host_str().is_some_and(|h| !h.is_empty());
    let scheme_ok = url.scheme() == "https" || (url.scheme() == "http" && is_loopback(&url));
    (no_account && has_host && scheme_ok).then_some(url)
}

/// 统计会发到哪里（界面显示用）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Destination {
    /// 发到这个地址（还没读过远程配置时是内置地址或环境变量给的地址）
    Url(String),
    /// 远程配置暂停了发送（发布方的总开关）
    Paused,
    /// 被环境变量关闭（变量名）
    EnvOff(&'static str),
    /// 调试构建不发
    DebugBuild,
}

// ---------------------------------------------------------------- 发送

/// 一次检查的输入（界面按当前的设置与状态给出）。
#[derive(Clone, Debug)]
pub struct CheckInput {
    /// 设置“发送匿名使用统计”
    pub enabled: bool,
    /// 今天用过（[`is_active`]）
    pub active: bool,
    /// 实际使用的界面语言
    pub lang: String,
    /// 现在（Unix 毫秒）
    pub now_ms: i64,
}

/// 一次检查的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// 发送成功，带发出的正文
    Sent(String),
    /// 设置中关闭了
    Disabled,
    /// 被环境变量关闭（变量名）
    EnvOff(&'static str),
    DebugBuild,
    /// 这个系统或架构不发
    Unsupported,
    /// 今天还没有用过
    Inactive,
    /// 今天已经发送过
    AlreadySent,
    /// 今天被接收端拒绝过，不再试
    GaveUpToday,
    /// 另一次检查正在发送
    Busy,
    /// 远程配置暂停了发送
    Paused,
    /// 发送失败，留到下一次检查
    Failed(SendError),
    /// 接收端拒绝（HTTP 状态码），今天不再试
    Rejected(u16),
}

/// 发送失败的原因。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    #[error("网络错误：{0}")]
    Network(String),
    #[error("接收地址返回 HTTP {0}")]
    Status(u16),
}

impl SendError {
    /// 留到下一次检查再试：网络错误（含超时）、HTTP 5xx、408、429；其他状态码当天不再试。
    pub fn retryable(&self) -> bool {
        match self {
            SendError::Network(_) => true,
            SendError::Status(status) => matches!(status, 408 | 429 | 500..),
        }
    }
}

/// [`UsageReporter`] 的参数。正式运行用 [`UsageReporter::new`]，测试逐项指定。
#[derive(Clone, Debug)]
pub struct ReporterOptions {
    /// `usage.json` 的位置
    pub state_file: PathBuf,
    pub route: Route,
    /// [`Route::Default`] 时读的远程配置；`None` 时直接用 `builtin`
    pub remote: Option<RemoteConfigSource>,
    /// 内置的接收地址
    pub builtin: Url,
}

/// “查看内容”对话框显示的：今天要发送的正文与本机记的日期。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    /// 与实际发出的相同（[`UsageMessage::to_json`]）
    pub json: String,
    pub last_sent: Option<NaiveDate>,
    pub sent_today: bool,
}

/// 发送使用统计。克隆后共用同一份状态。
#[derive(Clone)]
pub struct UsageReporter {
    inner: Arc<Inner>,
}

struct Inner {
    options: ReporterOptions,
    /// 正在发送（同一时间只发一条）
    busy: AtomicBool,
    /// [`UsageReporter::forget`] 的次数：发送期间关闭了的，发送成功后不再写 `usage.json`
    generation: AtomicU64,
    day: Mutex<DayStatus>,
}

/// 只在内存中的当天状态。
#[derive(Default)]
struct DayStatus {
    /// 这一天被接收端拒绝过
    gave_up: Option<NaiveDate>,
    /// 这一天已在日志中记过失败
    failure_logged: Option<NaiveDate>,
    /// 最近一次读远程配置得到的去向
    remote: Option<UsageEndpoint>,
}

impl UsageReporter {
    /// 正式运行：按环境变量定去向，`usage.json` 在数据目录中；去向是 [`Route::Default`] 时读远程配置。
    pub fn new(data_dir: &DataDir) -> Self {
        let route = Route::from_env();
        let remote = (route == Route::Default).then(|| RemoteConfigSource::new(data_dir));
        Self::with(ReporterOptions {
            state_file: data_dir.usage_state(),
            route,
            remote,
            builtin: Url::parse(USAGE_URL).expect("内置的使用统计地址"),
        })
    }

    pub fn with(options: ReporterOptions) -> Self {
        Self {
            inner: Arc::new(Inner {
                options,
                busy: AtomicBool::new(false),
                generation: AtomicU64::new(0),
                day: Mutex::new(DayStatus::default()),
            }),
        }
    }

    fn day(&self) -> std::sync::MutexGuard<'_, DayStatus> {
        self.inner.day.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 被环境变量关闭时为变量名（设置中的开关显示为关、不能改）。
    pub fn env_off(&self) -> Option<&'static str> {
        match self.inner.options.route {
            Route::EnvOff(name) => Some(name),
            _ => None,
        }
    }

    /// 统计会发到哪里。
    pub fn destination(&self) -> Destination {
        match &self.inner.options.route {
            Route::EnvOff(name) => Destination::EnvOff(name),
            Route::DebugBuild => Destination::DebugBuild,
            Route::Fixed(url) => Destination::Url(url.to_string()),
            Route::Default => match &self.day().remote {
                Some(UsageEndpoint::Paused) => Destination::Paused,
                Some(UsageEndpoint::Url(url)) => Destination::Url(url.to_string()),
                _ => Destination::Url(self.inner.options.builtin.to_string()),
            },
        }
    }

    /// 本机记的两个日期（没有或读不出时为 `None`）。
    pub fn state(&self) -> Option<UsageState> {
        fs::read_to_string(&self.inner.options.state_file)
            .ok()
            .and_then(|text| UsageState::parse(&text))
    }

    /// 今天（`now_ms` 那一天，UTC）要发送的正文；这个系统或架构不发时为 `None`。读一个小文件，界面在后台调用。
    pub fn preview(&self, lang: &str, now_ms: i64) -> Option<Preview> {
        let platform = Platform::current()?;
        let today = utc_day(now_ms);
        let state = self.state();
        Some(Preview {
            json: build_message(&platform, lang, state.as_ref(), today).to_json(),
            last_sent: state.map(|s| s.last_sent),
            sent_today: state.is_some_and(|s| s.last_sent == today),
        })
    }

    /// 关闭后：删除 `usage.json`（本机不再保留统计用的任何记录），正在进行的那一次发送成功后也不再写它。
    /// 再打开时从头算（`age` 为 `new`）。
    pub fn forget(&self) {
        self.inner.generation.fetch_add(1, Ordering::SeqCst);
        {
            let mut day = self.day();
            day.gave_up = None;
            day.failure_logged = None;
        }
        match fs::remove_file(&self.inner.options.state_file) {
            Ok(()) => tracing::info!("已关闭使用统计，删除了本机记的日期（usage.json）"),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("删除使用统计的本机记录（usage.json）失败：{e}"),
        }
    }

    /// 检查一次，满足条件时发送（见模块说明）。要在核心的运行时上执行（[`crate::runtime::spawn`]）。
    pub async fn check(&self, input: CheckInput) -> Outcome {
        if !input.enabled {
            return Outcome::Disabled;
        }
        let route = match &self.inner.options.route {
            Route::EnvOff(name) => return Outcome::EnvOff(name),
            Route::DebugBuild => return Outcome::DebugBuild,
            route => route.clone(),
        };
        let Some(platform) = Platform::current() else {
            return Outcome::Unsupported;
        };
        if !input.active {
            return Outcome::Inactive;
        }
        let today = utc_day(input.now_ms);
        if self.day().gave_up == Some(today) {
            return Outcome::GaveUpToday;
        }
        if self.inner.busy.swap(true, Ordering::AcqRel) {
            return Outcome::Busy;
        }
        struct Release<'a>(&'a AtomicBool);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _release = Release(&self.inner.busy);

        let generation = self.inner.generation.load(Ordering::SeqCst);
        let state = self.state();
        // 上次发送比今天晚（时钟往回调过）时按今天还没发处理
        if state.is_some_and(|s| s.last_sent == today) {
            return Outcome::AlreadySent;
        }
        let url = match route {
            Route::Fixed(url) => url,
            _ => match self.remote_url().await {
                Some(url) => url,
                None => return Outcome::Paused,
            },
        };
        let body = build_message(&platform, &input.lang, state.as_ref(), today).to_json();
        match send(&url, &body).await {
            Ok(()) => {
                if self.inner.generation.load(Ordering::SeqCst) == generation {
                    let next = UsageState::after_sent(state.as_ref(), today);
                    let path = &self.inner.options.state_file;
                    let written = path
                        .parent()
                        .map_or(Ok(()), fs::create_dir_all)
                        .and_then(|()| {
                            crate::platform::write_private_file(path, next.to_json().as_bytes())
                        });
                    if let Err(e) = written {
                        tracing::warn!("记下使用统计的发送日期失败：{e}");
                    }
                } else {
                    tracing::info!("发送期间关闭了使用统计，不记录发送日期");
                }
                tracing::info!("已发送使用统计：{body}");
                Outcome::Sent(body)
            }
            Err(error) if error.retryable() => {
                let mut day = self.day();
                if day.failure_logged != Some(today) {
                    day.failure_logged = Some(today);
                    tracing::info!("发送使用统计失败，下次检查时再试：{error}");
                }
                Outcome::Failed(error)
            }
            Err(error) => {
                self.day().gave_up = Some(today);
                tracing::warn!("使用统计被接收端拒绝，今天不再发送：{error}");
                match error {
                    SendError::Status(status) => Outcome::Rejected(status),
                    SendError::Network(_) => Outcome::Failed(error),
                }
            }
        }
    }

    /// 读远程配置的 `usageUrl`：暂停时为 `None`；没有这一项、写错了、远程配置不可用时为内置地址。
    async fn remote_url(&self) -> Option<Url> {
        let endpoint = match &self.inner.options.remote {
            Some(source) => source
                .fetch()
                .await
                .map_or(UsageEndpoint::Builtin, |config| config.usage_endpoint()),
            None => UsageEndpoint::Builtin,
        };
        let paused_before = {
            let mut day = self.day();
            let before = day.remote == Some(UsageEndpoint::Paused);
            day.remote = Some(endpoint.clone());
            before
        };
        match endpoint {
            UsageEndpoint::Builtin => Some(self.inner.options.builtin.clone()),
            UsageEndpoint::Url(url) => Some(url),
            UsageEndpoint::Paused => {
                if !paused_before {
                    tracing::info!("远程配置暂停了使用统计，不发送");
                }
                None
            }
        }
    }
}

/// HTTP 客户端：走系统与环境变量中的代理（与检查更新相同），本机回环地址不走代理；不跟随重定向。
fn client(url: &Url) -> Option<&'static reqwest::Client> {
    fn build(direct: bool) -> Option<reqwest::Client> {
        let mut builder = reqwest::Client::builder()
            .user_agent(format!("Termlet/{}", crate::VERSION))
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(SEND_TIMEOUT);
        if direct {
            builder = builder.no_proxy();
        }
        builder
            .build()
            .inspect_err(|e| tracing::warn!("无法创建 HTTP 客户端：{e}"))
            .ok()
    }
    static PROXIED: LazyLock<Option<reqwest::Client>> = LazyLock::new(|| build(false));
    static DIRECT: LazyLock<Option<reqwest::Client>> = LazyLock::new(|| build(true));
    if is_loopback(url) {
        DIRECT.as_ref()
    } else {
        PROXIED.as_ref()
    }
}

/// `POST` 正文（`application/json`），10 秒时限；2xx 算成功。
async fn send(url: &Url, body: &str) -> Result<(), SendError> {
    let client = client(url).ok_or_else(|| SendError::Network("无法创建 HTTP 客户端".into()))?;
    let response = client
        .post(url.clone())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .timeout(SEND_TIMEOUT)
        .send()
        .await
        .map_err(|e| SendError::Network(e.without_url().to_string()))?;
    let status = response.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(SendError::Status(status.as_u16()))
    }
}
