//! 自动更新（技术方案第 12 节）。
//!
//! 1. **更新源**：环境变量 `TERMLET_UPDATE_URL`（测试与内部渠道；`off` 关闭检查）> 远程配置给出的地址
//!    （只接受内置源同一来源下的，排在内置地址之前；远程配置还可以暂停更新并给出提示）> 内置地址。依次尝试，
//!    有一个成功即停止；全部失败时报告最后一个的原因（每个地址的原因都写进日志）。
//! 2. **下载前校验**：取本机的版本描述文件（Windows x64 是 `latest.yml`，其他平台与架构见
//!    [`manifest::metadata_file_for`]）与它的签名（`.minisig`），用内置公钥校验（[`signature`]），可信注释要与
//!    文件名、版本一致，解析出的版本号也要与签名中的一致；只接受比当前高的版本（防降级）。只提示的平台同样校验，
//!    免得提示一个不存在的新版本。测试的更新源在本机回环地址上时，另外信任 `TERMLET_UPDATE_TEST_PUBKEY` 给出的公钥。
//! 3. **下载**：Windows 安装版（x64、arm64）自动下载本机架构的安装包到 `{数据目录}/updates/`，边下边算 SHA-512，
//!    大小与哈希都与校验过签名的清单一致才算下载完成；已下载过的同一文件哈希相同时直接沿用。其他平台、开发构建、便携版与其他
//!    免安装的程序（程序旁边没有卸载程序）与设了 `TERMLET_UPDATE_NOTIFY_ONLY` 时只提示新版本，界面打开下载页
//!    （[`DOWNLOAD_PAGE`]，便携版锚到便携版的下载）。
//! 4. **安装**：安装前再核对一次文件的 SHA-512（下载后被替换也装不上），然后以静默方式启动安装程序（参数与
//!    electron-updater 相同：`/S --updated`，装完要启动时加 `--force-run`，`/D=` 指定现在的安装目录）。
//!    “重启并安装”用 [`Updater::install_now`]，调用方随后退出应用；不点时在退出应用时安装（[`Updater::install_on_quit`]），
//!    装完不启动。安装程序等 Termlet 退出后再替换文件，只剩后台实例时直接结束它（`packaging/windows/installer.nsi`）。
//!
//! 状态经 [`Updater::subscribe`] 推送给界面；完整的错误只写日志（一行摘要），界面按 [`UpdateErrorKind`] 选文案。

mod error;
pub(crate) mod http;
pub mod keys;
pub mod manifest;
pub mod signature;
#[cfg(test)]
pub(crate) mod test_support;
pub mod version;

use std::cmp::Ordering;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use data_encoding::BASE64;
use sha2::{Digest as _, Sha512};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::watch;
use url::Url;

pub use error::{UpdateError, UpdateErrorKind, error_summary};
pub use signature::SignatureError;

use crate::paths::DataDir;
use crate::remote_config::{RemoteConfigSource, trusted_feeds};
use http::Fetched;
use manifest::Manifest;

/// 内置更新源（按顺序尝试）：自己的下载站（Cloudflare R2，`scripts/release.mjs publish` 上传），GitHub Releases
/// 备用（下载站不通时，或海外）。新加的地址同时加进 [`TRUSTED_FEED_PREFIXES`]。
pub const BUILTIN_FEEDS: &[&str] = &[
    "https://dl.termlet.dev",
    "https://github.com/termlet/termlet/releases/latest/download",
];
/// 远程配置给出的更新源只接受这些前缀下的地址：更新文件有签名，这里再限制来源，远程配置出问题也不能把更新指向别处。
pub const TRUSTED_FEED_PREFIXES: &[&str] = &[
    "https://dl.termlet.dev/",
    "https://github.com/termlet/termlet/releases/",
];
/// 手动下载页（官网的下载页；英文界面用 [`DOWNLOAD_PAGE_EN`]）：只提示新版本的平台、签名校验没有通过时打开它。
pub const DOWNLOAD_PAGE: &str = "https://termlet.dev/download/";
/// 英文的下载页。
pub const DOWNLOAD_PAGE_EN: &str = "https://termlet.dev/en/download/";
/// 这些主机上的更新源取版本描述文件时不加防缓存的参数：下载站的防火墙只放行不带参数的请求（带参数的绕过 CDN
/// 缓存，每次都读 R2），版本描述文件在 CDN 上只缓存 60 秒，发布时也会清掉。
const NO_CACHE_BUSTING_HOSTS: &[&str] = &["dl.termlet.dev"];
/// 两次自动检查的最小间隔（同一次运行中；手动检查不受限制）。
pub const AUTO_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// 下载进度最多隔这么久更新一次（百分比变了时也更新）：不必每收到一块就推给界面。
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// 下载的百分比：核对完才到 100，所以最多 99；总大小未知时为 0。
fn download_percent(received: u64, total: Option<u64>) -> u8 {
    total
        .and_then(|total| received.saturating_mul(100).checked_div(total))
        .map_or(0, |percent| percent.min(99) as u8)
}

/// 什么时候把下载进度推给界面：百分比变了，或距上次超过 [`PROGRESS_INTERVAL`]（总大小未知、下载很慢时
/// 也看得到字节数在涨）。
struct ProgressThrottle {
    last: Instant,
    percent: u8,
}

impl ProgressThrottle {
    fn new(now: Instant) -> Self {
        Self {
            last: now,
            percent: 0,
        }
    }

    /// 现在该不该更新；该更新时记下这次。
    fn due(&mut self, now: Instant, percent: u8) -> bool {
        if percent == self.percent && now.duration_since(self.last) < PROGRESS_INTERVAL {
            return false;
        }
        self.last = now;
        self.percent = percent;
        true
    }
}

/// 改更新源；值为 `off` 时关闭更新检查。
pub const UPDATE_URL_ENV: &str = "TERMLET_UPDATE_URL";
/// 设置后 Windows 安装版也只提示新版本。
pub const NOTIFY_ONLY_ENV: &str = "TERMLET_UPDATE_NOTIFY_ONLY";
/// 测试用的公钥：只在更新源是本机回环地址时额外信任。
pub const TEST_PUBKEY_ENV: &str = "TERMLET_UPDATE_TEST_PUBKEY";
/// 安装向导放在安装目录中的卸载程序；程序旁边有它才是安装版（可以自动安装）。
pub const UNINSTALLER_EXE: &str = "Uninstall Termlet.exe";

/// 版本描述文件与签名的时限。
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const PART_SUFFIX: &str = ".part";

/// 谁发起的检查。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckTrigger {
    /// 启动后（设置“自动检查更新”开启时）或定时：距上次检查不足 [`AUTO_CHECK_INTERVAL`] 时跳过
    Auto,
    /// 设置中点“检查更新”
    Manual,
}

/// 更新的状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateStatus {
    Idle,
    Checking,
    /// 已是最新版本
    Latest,
    /// 有新版本，但这个平台只提示（界面给出“前往下载”）
    Available {
        version: String,
    },
    /// 下载中：百分比（核对完才到 100）、已下载与总字节数（总大小未知时为 `None`）。约每
    /// [`PROGRESS_INTERVAL`] 或每 1% 更新一次
    Downloading {
        version: String,
        percent: u8,
        downloaded: u64,
        total: Option<u64>,
    },
    /// 已下载并校验：界面提示“重启并安装”，不点时退出时安装
    Downloaded {
        version: String,
    },
    Failed {
        kind: UpdateErrorKind,
        /// 简短的说明（悬停提示用，完整内容在日志中）
        error: String,
    },
    /// 关闭了更新检查或远程配置暂停了更新：显示 [`UpdateState::message`]，没有时显示“更新暂不可用”
    Disabled,
}

/// 推送给界面的状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateState {
    pub status: UpdateStatus,
    /// 当前版本
    pub current: String,
    /// 远程配置中的提示
    pub message: Option<String>,
    /// false 表示这个平台只提示、需要手动下载
    pub auto_install: bool,
}

/// 更新源从哪里来。
#[derive(Clone, Debug)]
pub enum FeedSource {
    /// 关闭更新检查（`TERMLET_UPDATE_URL=off`）
    Off,
    /// 固定的更新源（`TERMLET_UPDATE_URL=<地址>`）
    Fixed(String),
    /// 远程配置给出的（只接受 `trusted_prefixes` 下的）排在内置地址之前；远程配置不可用时只用内置地址
    Remote {
        config: Box<RemoteConfigSource>,
        builtin: Vec<String>,
        trusted_prefixes: Vec<String>,
    },
}

/// 更新的参数。正式运行用 [`UpdaterOptions::from_env`]，测试可以逐项指定。
#[derive(Clone, Debug)]
pub struct UpdaterOptions {
    pub current_version: String,
    /// 下载安装包的目录
    pub download_dir: PathBuf,
    /// 安装版的安装目录；`None` 表示只提示新版本
    pub install_dir: Option<PathBuf>,
    pub feeds: FeedSource,
    /// 受信任的公钥（minisign 格式）
    pub keys: Vec<String>,
    /// 测试用的公钥，只在本机回环地址的更新源上生效
    pub test_public_key: Option<String>,
    /// 安装包文件名中的平台（`win-x64`、`win-arm64`）
    pub platform: String,
    /// 版本描述文件名（`latest.yml`、`latest-arm64.yml`）
    pub metadata_file: String,
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

impl UpdaterOptions {
    /// 按环境变量与程序所在位置确定：更新源、能否自动安装。
    pub fn from_env(data_dir: &DataDir) -> Self {
        let feeds = match env_value(UPDATE_URL_ENV) {
            Some(v) if v.trim() == "off" => FeedSource::Off,
            Some(v) => FeedSource::Fixed(v.trim().to_string()),
            None => FeedSource::Remote {
                config: Box::new(RemoteConfigSource::new(data_dir)),
                builtin: BUILTIN_FEEDS.iter().map(|s| s.to_string()).collect(),
                trusted_prefixes: TRUSTED_FEED_PREFIXES
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            },
        };
        let install_dir = install_dir_for(
            data_dir.is_portable(),
            env_value(NOTIFY_ONLY_ENV).is_some(),
            installed_dir,
        );
        Self {
            current_version: crate::VERSION.to_string(),
            download_dir: data_dir.updates_dir(),
            install_dir,
            feeds,
            keys: keys::UPDATE_PUBLIC_KEYS
                .iter()
                .map(|k| k.to_string())
                .collect(),
            test_public_key: env_value(TEST_PUBKEY_ENV),
            platform: manifest::platform_tag(),
            metadata_file: manifest::metadata_file(),
        }
    }
}

/// 自动安装到哪里：便携版（安装程序会装成安装版）与设了 [`NOTIFY_ONLY_ENV`] 时不自动安装（`None`，只提示新版本），
/// 否则为安装版的安装目录（`installed` 给出）。
fn install_dir_for(
    portable: bool,
    notify_only: bool,
    installed: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    if portable || notify_only {
        None
    } else {
        installed()
    }
}

/// 这个系统与架构（`std::env::consts` 的写法）有没有安装向导、能不能自动安装：Windows 的 x64 与 arm64
/// （各有自己的安装包与清单）。macOS、Linux 只提示新版本。
pub fn auto_install_supported(os: &str, arch: &str) -> bool {
    os == "windows" && matches!(arch, "x86_64" | "aarch64")
}

/// 安装版的安装目录：Windows（x64、arm64）上程序旁边有安装向导放的卸载程序时，为程序所在的目录。
/// 开发构建、免安装复制出来的程序为 `None`（只提示新版本）。
pub fn installed_dir() -> Option<PathBuf> {
    if !auto_install_supported(std::env::consts::OS, std::env::consts::ARCH) {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    dir.join(UNINSTALLER_EXE)
        .is_file()
        .then(|| dir.to_path_buf())
}

/// 受信任的公钥：内置的；更新源是本机回环地址时另加测试公钥。
pub fn trusted_keys(feed: &str, builtin: &[String], test_key: Option<&str>) -> Vec<String> {
    let mut keys = builtin.to_vec();
    if let Some(test) = test_key
        && Url::parse(feed).is_ok_and(|u| http::is_loopback(&u))
    {
        keys.push(test.to_string());
    }
    keys
}

/// 静默安装的参数（与 electron-updater 启动 NSIS 安装程序的参数相同；`/D=` 另加在最后）。
pub fn installer_args(run_after: bool) -> Vec<&'static str> {
    let mut args = vec!["/S", "--updated"];
    if run_after {
        args.push("--force-run");
    }
    args
}

/// 已校验过签名的、本机要下载与安装的版本。
#[derive(Clone, Debug)]
struct Release {
    version: String,
    url: Url,
    file: PathBuf,
    sha512: String,
    size: Option<u64>,
}

/// 自动更新。克隆后共用同一份状态。
#[derive(Clone)]
pub struct Updater {
    inner: Arc<Inner>,
}

struct Inner {
    options: UpdaterOptions,
    state: watch::Sender<UpdateState>,
    last_check: Mutex<Option<Instant>>,
    /// 最近一次校验过签名的版本；安装前用它核对下载的文件
    release: Mutex<Option<Release>>,
    /// 已经启动过安装程序（“重启并安装”之后退出时不再启动一次）
    launched: AtomicBool,
}

impl Updater {
    pub fn new(options: UpdaterOptions) -> Self {
        let state = UpdateState {
            status: UpdateStatus::Idle,
            current: options.current_version.clone(),
            message: None,
            auto_install: options.install_dir.is_some(),
        };
        Self {
            inner: Arc::new(Inner {
                options,
                state: watch::channel(state).0,
                last_check: Mutex::new(None),
                release: Mutex::new(None),
                launched: AtomicBool::new(false),
            }),
        }
    }

    /// 按环境变量与数据目录创建（正式运行用）。
    pub fn from_env(data_dir: &DataDir) -> Self {
        Self::new(UpdaterOptions::from_env(data_dir))
    }

    pub fn state(&self) -> UpdateState {
        self.inner.state.borrow().clone()
    }

    /// 订阅状态变化（界面据此刷新设置中的更新一节与“新版本已下载”的提示）。
    pub fn subscribe(&self) -> watch::Receiver<UpdateState> {
        self.inner.state.subscribe()
    }

    fn set_status(&self, status: UpdateStatus) {
        self.inner.state.send_modify(|s| s.status = status);
    }

    /// 检查更新，返回检查完的状态（有新版本且可以自动安装时，下载在后台继续，状态为“正在下载”）。
    /// 正在检查、下载中或已下载时直接返回当前状态。要在 tokio 运行时中执行（[`crate::runtime::spawn`]）。
    pub async fn check(&self, trigger: CheckTrigger) -> UpdateState {
        {
            let mut last = self.inner.last_check.lock().unwrap();
            if trigger == CheckTrigger::Auto
                && last.is_some_and(|t| t.elapsed() < AUTO_CHECK_INTERVAL)
            {
                return self.state();
            }
            let started = self.inner.state.send_if_modified(|s| {
                if matches!(
                    s.status,
                    UpdateStatus::Checking
                        | UpdateStatus::Downloading { .. }
                        | UpdateStatus::Downloaded { .. }
                ) {
                    return false;
                }
                s.status = UpdateStatus::Checking;
                true
            });
            if !started {
                return self.state();
            }
            *last = Some(Instant::now());
        }
        self.clean_downloads();

        let (feeds, disabled, message) = self.feeds().await;
        self.inner.state.send_modify(|s| s.message = message);
        if disabled {
            self.set_status(UpdateStatus::Disabled);
            return self.state();
        }
        let mut last_error = None;
        tracing::debug!(feeds = feeds.len(), "检查更新");
        for feed in &feeds {
            match self.check_feed(feed).await {
                // 开始下载时 check_feed 已经设好了状态：下载任务可能已经结束并改了状态，不能再覆盖
                Ok(UpdateStatus::Downloading { .. }) => return self.state(),
                Ok(status) => {
                    self.set_status(status);
                    return self.state();
                }
                Err(e) => {
                    tracing::warn!(feed = %feed, "检查更新失败：{}", error_summary(&e.to_string(), 300));
                    last_error = Some(e);
                }
            }
        }
        let error = last_error.unwrap_or_else(|| UpdateError::Manifest("没有可用的更新源".into()));
        self.set_status(UpdateStatus::Failed {
            kind: error.kind(),
            error: error.short(),
        });
        self.state()
    }

    /// 更新源、是否暂停、远程配置中的提示。
    async fn feeds(&self) -> (Vec<String>, bool, Option<String>) {
        match &self.inner.options.feeds {
            FeedSource::Off => (Vec::new(), true, None),
            FeedSource::Fixed(url) => (vec![url.trim_end_matches('/').to_string()], false, None),
            FeedSource::Remote {
                config,
                builtin,
                trusted_prefixes,
            } => match config.fetch().await {
                None => (builtin.clone(), false, None),
                Some(cfg) => {
                    let mut feeds = trusted_feeds(cfg.feeds.as_deref(), trusted_prefixes);
                    for b in builtin {
                        if !feeds.contains(b) {
                            feeds.push(b.clone());
                        }
                    }
                    (feeds, cfg.disabled == Some(true), cfg.message)
                }
            },
        }
    }

    /// 检查一个更新源，返回检查后的状态；需要下载时先把状态设为下载中，再在后台开始下载。
    async fn check_feed(&self, feed: &str) -> Result<UpdateStatus, UpdateError> {
        let options = &self.inner.options;
        let file = options.metadata_file.as_str();
        // 地址末尾补上 /，否则相对地址会替换掉最后一段
        let mut base = Url::parse(feed).map_err(|e| UpdateError::Manifest(e.to_string()))?;
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        let fetch = |name: String| {
            let base = base.clone();
            async move {
                let url = manifest_url(&base, &name)?;
                http::get_text(&url, MANIFEST_TIMEOUT, MAX_MANIFEST_BYTES).await
            }
        };
        let text = match fetch(file.to_string()).await? {
            Fetched::Body(text) => text,
            Fetched::Status(404) => return Err(UpdateError::Unpublished(file.into())),
            Fetched::Status(status) => {
                return Err(UpdateError::Http {
                    file: file.into(),
                    status,
                });
            }
        };
        let sig = match fetch(format!("{file}.minisig")).await? {
            Fetched::Body(text) => text,
            Fetched::Status(status) => {
                return Err(SignatureError::Missing {
                    file: file.into(),
                    status,
                }
                .into());
            }
        };
        let keys = trusted_keys(feed, &options.keys, options.test_public_key.as_deref());
        let version = signature::verify_manifest(&text, &sig, file, &keys)?;
        let manifest = Manifest::parse(&text)?;
        // 签名覆盖的是原文：解析出的版本号也要与签名中的一致
        if manifest.version != version {
            return Err(SignatureError::VersionMismatch(manifest.version).into());
        }
        if version::compare_versions(&version, &options.current_version) != Ordering::Greater {
            tracing::info!(latest = %version, current = %options.current_version, "已是最新版本");
            *self.inner.release.lock().unwrap() = None;
            return Ok(UpdateStatus::Latest);
        }
        tracing::info!(version = %version, feed = %feed, "发现新版本");
        if options.install_dir.is_none() {
            return Ok(UpdateStatus::Available { version });
        }
        let entry = manifest
            .installer(&options.platform)
            .ok_or(UpdateError::NoInstaller)?;
        let url = base
            .join(&entry.url)
            .map_err(|e| UpdateError::Manifest(e.to_string()))?;
        let name = manifest::file_name(&url)
            .ok_or_else(|| UpdateError::Manifest("安装包的文件名不合法".into()))?;
        let release = Release {
            version: version.clone(),
            url,
            file: options.download_dir.join(name),
            sha512: entry.sha512.clone(),
            size: entry.size,
        };
        *self.inner.release.lock().unwrap() = Some(release.clone());
        let downloading = UpdateStatus::Downloading {
            version,
            percent: 0,
            downloaded: 0,
            total: release.size,
        };
        // 先设状态再开始下载：下载很快结束（已下载过的文件只核对一次）时，它写下的结果不会被这里覆盖
        self.set_status(downloading.clone());
        let this = self.clone();
        tokio::spawn(async move { this.download(release).await });
        Ok(downloading)
    }

    async fn download(&self, release: Release) {
        match self.download_to_file(&release).await {
            Ok(()) => {
                tracing::info!(version = %release.version, "更新已下载并核对");
                self.set_status(UpdateStatus::Downloaded {
                    version: release.version,
                });
            }
            Err(e) => {
                tracing::warn!("下载更新失败：{}", error_summary(&e.to_string(), 300));
                *self.inner.release.lock().unwrap() = None;
                self.set_status(UpdateStatus::Failed {
                    kind: e.kind(),
                    error: e.short(),
                });
            }
        }
    }

    async fn download_to_file(&self, release: &Release) -> Result<(), UpdateError> {
        let dir = &self.inner.options.download_dir;
        tokio::fs::create_dir_all(dir).await?;
        self.remove_downloads_except(Some(&release.file));
        // 已下载过的同一文件：哈希与这次校验过的清单相同时沿用
        if release.file.is_file() {
            let path = release.file.clone();
            let sha = tokio::task::spawn_blocking(move || manifest::sha512_file(&path))
                .await
                .map_err(|e| UpdateError::Io(std::io::Error::other(e)))??;
            if sha == release.sha512 {
                return Ok(());
            }
            let _ = tokio::fs::remove_file(&release.file).await;
        }
        let part = part_path(&release.file);
        let result = self.receive(release, &part).await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&part).await;
        }
        result?;
        tokio::fs::rename(&part, &release.file).await?;
        Ok(())
    }

    /// 下载到 `part`，边下边算 SHA-512，大小与哈希都要与清单一致。
    async fn receive(&self, release: &Release, part: &Path) -> Result<(), UpdateError> {
        let name = release
            .file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut response = http::get(&release.url, None).await?;
        let status = response.status();
        if !status.is_success() {
            return Err(UpdateError::Http {
                file: name,
                status: status.as_u16(),
            });
        }
        let total = release
            .size
            .or(response.content_length())
            .filter(|&total| total > 0);
        let mut out = tokio::fs::File::create(part).await?;
        let mut hasher = Sha512::new();
        let mut received: u64 = 0;
        let mut progress = ProgressThrottle::new(Instant::now());
        while let Some(chunk) = response.chunk().await.map_err(UpdateError::network)? {
            received += chunk.len() as u64;
            if release.size.is_some_and(|size| received > size) {
                return Err(UpdateError::Download("比版本信息中的大小更大".into()));
            }
            hasher.update(&chunk);
            out.write_all(&chunk).await?;
            let percent = download_percent(received, total);
            if progress.due(Instant::now(), percent) {
                self.inner.state.send_if_modified(|s| match &mut s.status {
                    UpdateStatus::Downloading {
                        percent: p,
                        downloaded,
                        total: t,
                        ..
                    } => {
                        *p = percent;
                        *downloaded = received;
                        *t = total;
                        true
                    }
                    _ => false,
                });
            }
        }
        out.flush().await?;
        out.sync_all().await?;
        drop(out);
        if release.size.is_some_and(|size| received != size) {
            return Err(UpdateError::Download(format!(
                "大小为 {received} 字节，版本信息中为 {} 字节",
                release.size.unwrap_or_default()
            )));
        }
        if BASE64.encode(&hasher.finalize()) != release.sha512 {
            return Err(UpdateError::Download("SHA-512 不同".into()));
        }
        Ok(())
    }

    /// 安装前核对：已下载的安装包必须是最近一次校验过签名的版本信息中列出的（SHA-512 相同）。
    fn verified_installer(&self) -> Result<Release, UpdateError> {
        if !matches!(self.state().status, UpdateStatus::Downloaded { .. }) {
            return Err(UpdateError::NotDownloaded);
        }
        let release = self
            .inner
            .release
            .lock()
            .unwrap()
            .clone()
            .ok_or(SignatureError::Unverified)?;
        let sha = manifest::sha512_file(&release.file)?;
        if sha != release.sha512 {
            tracing::warn!("下载的安装包与签名的版本信息不一致，不安装");
            let _ = std::fs::remove_file(&release.file);
            *self.inner.release.lock().unwrap() = None;
            let error = UpdateError::from(SignatureError::InstallerMismatch);
            self.set_status(UpdateStatus::Failed {
                kind: error.kind(),
                error: error.short(),
            });
            return Err(error);
        }
        tracing::info!(version = %release.version, "安装包与签名的版本信息一致");
        Ok(release)
    }

    /// “重启并安装”：核对后启动安装程序（静默安装，装完启动 Termlet），调用方随后退出应用。
    /// 调用前先请有未保存的文件、未完成的传输的窗口确认（安装程序启动后再取消退出就来不及了）。
    pub fn install_now(&self) -> Result<(), UpdateError> {
        self.launch(true)
    }

    /// 退出应用时调用：已下载更新时静默安装（装完不启动），返回是否启动了安装程序。
    /// 没有下载、已经由 [`Self::install_now`] 启动过、核对没有通过时什么也不做。
    pub fn install_on_quit(&self) -> bool {
        if self.inner.launched.load(AtomicOrdering::SeqCst)
            || !matches!(self.state().status, UpdateStatus::Downloaded { .. })
        {
            return false;
        }
        match self.launch(false) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!("退出时安装更新失败：{e}");
                false
            }
        }
    }

    fn launch(&self, run_after: bool) -> Result<(), UpdateError> {
        let Some(install_dir) = self.inner.options.install_dir.clone() else {
            return Err(UpdateError::NotDownloaded);
        };
        let release = self.verified_installer()?;
        if self.inner.launched.swap(true, AtomicOrdering::SeqCst) {
            return Ok(());
        }
        if let Err(e) = spawn_installer(&release.file, &install_dir, run_after) {
            self.inner.launched.store(false, AtomicOrdering::SeqCst);
            return Err(UpdateError::Launch(e));
        }
        tracing::info!(version = %release.version, run_after, "已启动安装程序");
        Ok(())
    }

    /// 清理下载目录：没下完的、不比当前版本新的安装包（装好之后留下的）。启动后在后台调用；检查更新时也会调用。
    pub fn clean_downloads(&self) {
        let options = &self.inner.options;
        let Ok(entries) = std::fs::read_dir(&options.download_dir) else {
            return;
        };
        let pending = self
            .inner
            .release
            .lock()
            .unwrap()
            .as_ref()
            .map(|r| r.file.clone());
        for entry in entries.flatten() {
            let path = entry.path();
            if pending.as_ref() == Some(&path) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let newer = installer_version(&name, &options.platform).is_some_and(|v| {
                version::compare_versions(v, &options.current_version) == Ordering::Greater
            });
            if !newer {
                remove_quietly(&path);
            }
        }
    }

    /// 只留下 `keep`（这次要下载的），其余都删除。
    fn remove_downloads_except(&self, keep: Option<&Path>) {
        let Ok(entries) = std::fs::read_dir(&self.inner.options.download_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if Some(path.as_path()) != keep {
                remove_quietly(&path);
            }
        }
    }
}

/// 版本描述文件或签名的地址（`base` 以 `/` 结尾）：带随机参数，避免取到缓存的旧文件；下载站上不带
/// （[`NO_CACHE_BUSTING_HOSTS`]）。
fn manifest_url(base: &Url, name: &str) -> Result<Url, UpdateError> {
    let mut url = base
        .join(name)
        .map_err(|e| UpdateError::Manifest(e.to_string()))?;
    let busting = !url.host_str().is_some_and(|h| {
        NO_CACHE_BUSTING_HOSTS
            .iter()
            .any(|n| h.eq_ignore_ascii_case(n))
    });
    if busting {
        url.query_pairs_mut()
            .append_pair("noCache", &crate::id::random_token(8));
    }
    Ok(url)
}

fn remove_quietly(path: &Path) {
    let result = if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    // 安装程序还在运行时删不掉，下次再删
    if let Err(e) = result {
        tracing::debug!(path = %path.display(), "删除更新文件失败：{e}");
    }
}

fn part_path(file: &Path) -> PathBuf {
    let mut name = file.file_name().map(OsString::from).unwrap_or_default();
    name.push(PART_SUFFIX);
    file.with_file_name(name)
}

/// 安装包文件名（`termlet-<版本>-<平台>.exe`）中的版本。
fn installer_version<'a>(name: &'a str, platform: &str) -> Option<&'a str> {
    name.strip_prefix("termlet-")?
        .strip_suffix(".exe")?
        .strip_suffix(platform)?
        .strip_suffix('-')
        .filter(|v| !v.is_empty())
}

#[cfg(windows)]
fn spawn_installer(file: &Path, install_dir: &Path, run_after: bool) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt as _;
    use std::process::{Command, Stdio};
    use windows_sys::Win32::System::Threading::{
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS,
    };

    let build = |flags: u32| {
        let mut cmd = Command::new(file);
        cmd.args(installer_args(run_after));
        // NSIS 的 /D= 必须放在最后，并且不能加引号（路径中有空格也一样）
        let mut dir = OsString::from("/D=");
        dir.push(install_dir.as_os_str());
        cmd.raw_arg(dir)
            .creation_flags(flags)
            .current_dir(file.parent().unwrap_or(Path::new(".")))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd
    };
    let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    // 应用退出后安装程序要继续运行：先尝试脱离应用所在的作业对象（作业不允许时退回不脱离）
    match build(flags | CREATE_BREAKAWAY_FROM_JOB).spawn() {
        Ok(_) => Ok(()),
        Err(_) => build(flags).spawn().map(drop),
    }
}

#[cfg(not(windows))]
fn spawn_installer(_file: &Path, _install_dir: &Path, _run_after: bool) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "这个平台不支持自动安装",
    ))
}

#[cfg(test)]
mod tests;
