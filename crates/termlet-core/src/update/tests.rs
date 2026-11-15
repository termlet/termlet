//! 自动更新的测试：更新源是本机起的最小 HTTP 服务器，签名用测试密钥，不依赖外网。

use std::time::Duration;

use url::Url;

use super::signature::{manifest_comment, remote_config_comment};
use super::test_support::{TestKey, TestServer, closed_port_url};
use super::*;

const PLATFORM: &str = "win-x64";

struct Fixture {
    server: TestServer,
    key: TestKey,
    tmp: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        Self {
            server: TestServer::start().await,
            key: TestKey::random(),
            tmp: tempfile::tempdir().unwrap(),
        }
    }

    fn feed(&self) -> String {
        self.server.url("/feed")
    }

    fn download_dir(&self) -> PathBuf {
        self.tmp.path().join("updates")
    }

    fn options(&self, feeds: FeedSource, install: bool) -> UpdaterOptions {
        UpdaterOptions {
            current_version: "1.0.0".into(),
            download_dir: self.download_dir(),
            install_dir: install.then(|| self.tmp.path().join("app")),
            feeds,
            keys: vec![self.key.public_key()],
            test_public_key: None,
            platform: PLATFORM.into(),
            metadata_file: "latest.yml".into(),
        }
    }

    fn updater(&self, install: bool) -> Updater {
        Updater::new(self.options(FeedSource::Fixed(self.feed()), install))
    }

    /// 发布一个版本：安装包、版本描述文件与签名（`sha512` 为 `None` 时按安装包计算）。
    fn publish(&self, version: &str, installer: &[u8], sha512: Option<&str>) -> String {
        let name = format!("termlet-{version}-{PLATFORM}.exe");
        let sha = sha512
            .map(str::to_string)
            .unwrap_or_else(|| manifest::sha512_base64(installer));
        let manifest = format!(
            "version: {version}\nfiles:\n  - url: {name}\n    sha512: {sha}\n    size: {}\npath: {name}\nsha512: {sha}\nreleaseDate: '2026-10-20T00:00:00.000Z'\n",
            installer.len()
        );
        let sig = self.key.sign(
            manifest.as_bytes(),
            &manifest_comment("latest.yml", version),
        );
        self.server.put(&format!("/feed/{name}"), installer);
        self.server.put("/feed/latest.yml", manifest.clone());
        self.server.put("/feed/latest.yml.minisig", sig);
        manifest
    }

    fn requests_for(&self, path: &str) -> usize {
        self.server
            .requests()
            .iter()
            .filter(|r| r.split('?').next() == Some(path))
            .count()
    }
}

async fn wait_status(updater: &Updater, done: impl Fn(&UpdateStatus) -> bool) -> UpdateState {
    let mut rx = updater.subscribe();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let state = rx.borrow_and_update().clone();
            if done(&state.status) {
                return state;
            }
            rx.changed().await.unwrap();
        }
    })
    .await
    .expect("等待更新状态超时")
}

fn failed_kind(state: &UpdateState) -> Option<UpdateErrorKind> {
    match state.status {
        UpdateStatus::Failed { kind, .. } => Some(kind),
        _ => None,
    }
}

// 便携版不自动安装（安装程序会装成安装版），只提示新版本；安装版照常装到安装目录
#[test]
fn portable_only_notifies() {
    let installed = || Some(PathBuf::from(r"C:\Termlet"));
    assert_eq!(install_dir_for(true, false, installed), None, "便携版");
    assert_eq!(install_dir_for(false, true, installed), None, "只提示");
    assert_eq!(
        install_dir_for(false, false, installed),
        Some(PathBuf::from(r"C:\Termlet"))
    );
    let tmp = tempfile::tempdir().unwrap();
    let options = UpdaterOptions::from_env(&DataDir::at(tmp.path()).into_portable());
    assert_eq!(options.install_dir, None);
}

#[tokio::test]
async fn notify_only_reports_the_new_version() {
    let f = Fixture::new().await;
    f.publish("2.0.0", b"installer", None);
    let updater = f.updater(false);
    assert!(!updater.state().auto_install);
    let state = updater.check(CheckTrigger::Manual).await;
    assert_eq!(
        state.status,
        UpdateStatus::Available {
            version: "2.0.0".into()
        }
    );
    assert_eq!(state.current, "1.0.0");
    // 只取版本描述文件与签名（带随机参数避免缓存），不下载安装包
    let requests = f.server.requests();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(requests[0].starts_with("/feed/latest.yml?noCache="));
    assert!(requests[1].starts_with("/feed/latest.yml.minisig?noCache="));
}

// Linux（x64、arm64）、macOS 读各自的清单（发布脚本 release.mjs manifest 写的，可信注释带这个文件名）：校验签名后
// 只提示新版本，不下载包；签给 Windows 的清单（可信注释中的文件名是 latest.yml）不认
#[tokio::test]
async fn linux_and_mac_read_their_own_manifests_and_only_notify() {
    for (file, platform, package) in [
        (
            "latest-linux.yml",
            "linux-x64",
            "termlet-2.0.0-linux-x64.tar.gz",
        ),
        (
            "latest-linux-arm64.yml",
            "linux-arm64",
            "termlet-2.0.0-linux-arm64.tar.gz",
        ),
        ("latest-mac.yml", "mac-arm64", "termlet-2.0.0-mac-arm64.zip"),
    ] {
        let f = Fixture::new().await;
        let sha = manifest::sha512_base64(b"package");
        let manifest = format!(
            "version: 2.0.0\nfiles:\n  - url: {package}\n    sha512: {sha}\n    size: 7\npath: {package}\nsha512: {sha}\nreleaseDate: '2026-10-30T00:00:00.000Z'\n"
        );
        f.server
            .put(&format!("/feed/{package}"), b"package".to_vec());
        f.server.put(&format!("/feed/{file}"), manifest.clone());
        let sign = |name: &str| {
            f.key
                .sign(manifest.as_bytes(), &manifest_comment(name, "2.0.0"))
        };
        f.server
            .put(&format!("/feed/{file}.minisig"), sign("latest.yml"));
        let mut options = f.options(FeedSource::Fixed(f.feed()), false);
        options.platform = platform.into();
        options.metadata_file = file.into();
        let updater = Updater::new(options);
        assert!(!updater.state().auto_install);
        let state = updater.check(CheckTrigger::Manual).await;
        assert_eq!(
            failed_kind(&state),
            Some(UpdateErrorKind::Signature),
            "{file}"
        );

        f.server.put(&format!("/feed/{file}.minisig"), sign(file));
        let state = updater.check(CheckTrigger::Manual).await;
        assert_eq!(
            state.status,
            UpdateStatus::Available {
                version: "2.0.0".into()
            },
            "{file}"
        );
        // 只取了清单与签名（两次检查），没有下载包
        assert_eq!(f.requests_for(&format!("/feed/{file}")), 2);
        assert_eq!(f.requests_for(&format!("/feed/{package}")), 0);
        assert!(!f.download_dir().exists());
    }
}

/// 在更新源上放一份 Windows 的清单（`file`，先列安装程序、再列便携版），按 `file` 签名；返回清单与签名。
fn publish_windows(f: &Fixture, file: &str, platform: &str, installer: &[u8]) -> (String, String) {
    let name = format!("termlet-2.0.0-{platform}.exe");
    let portable = format!("termlet-2.0.0-{platform}-portable.zip");
    let sha = manifest::sha512_base64(installer);
    let manifest = format!(
        "version: 2.0.0\nfiles:\n  - url: {name}\n    sha512: {sha}\n    size: {}\n  - url: {portable}\n    sha512: AAAA\n    size: 4\npath: {name}\nsha512: {sha}\nreleaseDate: '2026-11-09T00:00:00.000Z'\n",
        installer.len()
    );
    let sig = f
        .key
        .sign(manifest.as_bytes(), &manifest_comment(file, "2.0.0"));
    f.server.put(&format!("/feed/{name}"), installer.to_vec());
    f.server.put(&format!("/feed/{file}"), manifest.clone());
    f.server.put(&format!("/feed/{file}.minisig"), sig.clone());
    (manifest, sig)
}

// 能自动安装的只有 Windows 的 x64 与 arm64（各有安装向导与清单）；macOS、Linux 只提示
#[test]
fn auto_install_is_windows_x64_and_arm64() {
    assert!(auto_install_supported("windows", "x86_64"));
    assert!(auto_install_supported("windows", "aarch64"));
    for (os, arch) in [
        ("windows", "x86"),
        ("linux", "x86_64"),
        ("linux", "aarch64"),
        ("macos", "aarch64"),
    ] {
        assert!(!auto_install_supported(os, arch), "{os} {arch}");
    }
}

// Windows arm64 的安装版读 latest-arm64.yml、下载其中的 -win-arm64.exe；同一更新源上 x64 的 latest.yml 照旧只给
// x64 的客户端（内容不变，不列 arm64 的包）。签给 latest.yml 的签名不能拿来冒充 latest-arm64.yml，
// 没有 arm64 的清单时报“未发布”，不会退回去取 x64 的安装包。
#[tokio::test]
async fn windows_arm64_reads_its_own_manifest_and_installer() {
    let f = Fixture::new().await;
    let x64_installer = b"x64 installer".to_vec();
    let arm64_installer = b"arm64 installer".to_vec();
    let (x64_manifest, x64_sig) = publish_windows(&f, "latest.yml", "win-x64", &x64_installer);

    let mut options = f.options(FeedSource::Fixed(f.feed()), true);
    options.platform = "win-arm64".into();
    options.metadata_file = "latest-arm64.yml".into();
    let arm64 = Updater::new(options);
    let state = arm64.check(CheckTrigger::Manual).await;
    assert_eq!(failed_kind(&state), Some(UpdateErrorKind::Unpublished));

    // x64 的清单原样放到 arm64 的位置：可信注释中的文件名对不上
    f.server.put("/feed/latest-arm64.yml", x64_manifest);
    f.server.put("/feed/latest-arm64.yml.minisig", x64_sig);
    let state = arm64.check(CheckTrigger::Manual).await;
    assert_eq!(failed_kind(&state), Some(UpdateErrorKind::Signature));

    publish_windows(&f, "latest-arm64.yml", "win-arm64", &arm64_installer);
    let state = arm64.check(CheckTrigger::Manual).await;
    assert!(
        matches!(state.status, UpdateStatus::Downloading { ref version, .. } if version == "2.0.0"),
        "{state:?}"
    );
    let state = wait_status(&arm64, |s| !matches!(s, UpdateStatus::Downloading { .. })).await;
    assert_eq!(
        state.status,
        UpdateStatus::Downloaded {
            version: "2.0.0".into()
        }
    );
    let file = f.download_dir().join("termlet-2.0.0-win-arm64.exe");
    assert_eq!(std::fs::read(&file).unwrap(), arm64_installer);
    assert!(arm64.verified_installer().is_ok());
    assert_eq!(f.requests_for("/feed/termlet-2.0.0-win-x64.exe"), 0);
    assert_eq!(f.requests_for("/feed/latest.yml"), 0);

    // x64 的客户端（另一个下载目录）：读 latest.yml，取 x64 的安装包，不碰 arm64 的清单与安装包
    let x64_dir = tempfile::tempdir().unwrap();
    let mut options = f.options(FeedSource::Fixed(f.feed()), true);
    options.download_dir = x64_dir.path().to_path_buf();
    let updater = Updater::new(options);
    updater.check(CheckTrigger::Manual).await;
    let state = wait_status(&updater, |s| !matches!(s, UpdateStatus::Downloading { .. })).await;
    assert_eq!(
        state.status,
        UpdateStatus::Downloaded {
            version: "2.0.0".into()
        }
    );
    let file = x64_dir.path().join("termlet-2.0.0-win-x64.exe");
    assert_eq!(std::fs::read(&file).unwrap(), x64_installer);
    assert_eq!(f.requests_for("/feed/latest-arm64.yml"), 3);
    assert_eq!(f.requests_for("/feed/termlet-2.0.0-win-arm64.exe"), 1);
}

#[tokio::test]
async fn same_or_older_versions_are_latest() {
    let f = Fixture::new().await;
    let updater = f.updater(true);
    for version in ["1.0.0", "0.9.0", "1.0.0-beta.3"] {
        f.publish(version, b"installer", None);
        let state = updater.check(CheckTrigger::Manual).await;
        assert_eq!(state.status, UpdateStatus::Latest, "{version}");
    }
    assert_eq!(f.requests_for("/feed/termlet-0.9.0-win-x64.exe"), 0);
}

#[tokio::test]
async fn unpublished_and_unsigned_manifests() {
    let f = Fixture::new().await;
    let updater = f.updater(true);
    // 没有发布版本信息（404）
    let state = updater.check(CheckTrigger::Manual).await;
    assert_eq!(failed_kind(&state), Some(UpdateErrorKind::Unpublished));

    let manifest = f.publish("2.0.0", b"installer", None);
    let good_sig = f.key.sign(
        manifest.as_bytes(),
        &manifest_comment("latest.yml", "2.0.0"),
    );
    let other = TestKey::with_id(f.key.id());
    let cases = [
        // 没有签名
        None,
        // 内容被改过
        Some((manifest.replace("2.0.0", "2.0.1"), good_sig.clone())),
        // 同一个密钥 ID 的别的密钥
        Some((
            manifest.clone(),
            other.sign(
                manifest.as_bytes(),
                &manifest_comment("latest.yml", "2.0.0"),
            ),
        )),
        // 旧版本的可信注释
        Some((
            manifest.clone(),
            f.key.sign(
                manifest.as_bytes(),
                &manifest_comment("latest.yml", "1.0.0"),
            ),
        )),
        // 别的平台的清单
        Some((
            manifest.clone(),
            f.key.sign(
                manifest.as_bytes(),
                &manifest_comment("latest-mac.yml", "2.0.0"),
            ),
        )),
    ];
    for case in cases {
        match &case {
            None => f.server.remove("/feed/latest.yml.minisig"),
            Some((text, sig)) => {
                f.server.put("/feed/latest.yml", text.clone());
                f.server.put("/feed/latest.yml.minisig", sig.clone());
            }
        }
        let state = updater.check(CheckTrigger::Manual).await;
        assert_eq!(
            failed_kind(&state),
            Some(UpdateErrorKind::Signature),
            "{case:?}"
        );
    }
    // 签名不对时一律不下载
    assert_eq!(f.requests_for("/feed/termlet-2.0.0-win-x64.exe"), 0);
    assert!(!f.download_dir().join("termlet-2.0.0-win-x64.exe").exists());
}

#[tokio::test]
async fn no_builtin_keys_rejects_everything() {
    let f = Fixture::new().await;
    f.publish("2.0.0", b"installer", None);
    let mut options = f.options(FeedSource::Fixed(f.feed()), true);
    options.keys.clear();
    let updater = Updater::new(options);
    let state = updater.check(CheckTrigger::Manual).await;
    assert_eq!(failed_kind(&state), Some(UpdateErrorKind::Signature));
    let UpdateStatus::Failed { error, .. } = state.status else {
        unreachable!()
    };
    assert_eq!(error, "应用中没有内置更新公钥");
}

#[tokio::test]
async fn downloads_verifies_and_checks_again_before_installing() {
    let f = Fixture::new().await;
    let installer: Vec<u8> = (0..200_000u32).map(|i| (i % 253) as u8).collect();
    f.publish("2.0.0", &installer, None);
    let updater = f.updater(true);
    assert!(updater.state().auto_install);
    let state = updater.check(CheckTrigger::Manual).await;
    assert!(
        matches!(state.status, UpdateStatus::Downloading { ref version, .. } if version == "2.0.0"),
        "{state:?}"
    );
    // 开始时就知道总大小（版本信息中的），已下载为 0
    assert!(
        matches!(
            state.status,
            UpdateStatus::Downloading {
                downloaded: 0,
                total: Some(200_000),
                ..
            }
        ),
        "{state:?}"
    );
    let state = wait_status(&updater, |s| !matches!(s, UpdateStatus::Downloading { .. })).await;
    assert_eq!(
        state.status,
        UpdateStatus::Downloaded {
            version: "2.0.0".into()
        }
    );
    let file = f.download_dir().join("termlet-2.0.0-win-x64.exe");
    assert_eq!(std::fs::read(&file).unwrap(), installer);
    assert!(!part_path(&file).exists());

    // 已下载时再检查不会重复下载
    let again = updater.check(CheckTrigger::Manual).await;
    assert_eq!(again.status, state.status);
    assert_eq!(f.requests_for("/feed/termlet-2.0.0-win-x64.exe"), 1);

    // 安装前的核对通过；测试的“安装包”不是可执行文件，启动失败，之后还可以再试
    assert!(updater.verified_installer().is_ok());
    if cfg!(windows) {
        assert!(matches!(updater.install_now(), Err(UpdateError::Launch(_))));
        assert!(!updater.inner.launched.load(AtomicOrdering::SeqCst));
    }

    // 下载后被替换：不安装，状态显示签名问题
    std::fs::write(&file, b"tampered").unwrap();
    let err = updater.install_now().unwrap_err();
    assert!(matches!(
        err,
        UpdateError::Signature(SignatureError::InstallerMismatch)
    ));
    assert_eq!(
        failed_kind(&updater.state()),
        Some(UpdateErrorKind::Signature)
    );
    assert!(!file.exists());
    assert!(!updater.install_on_quit());
}

#[tokio::test]
async fn download_that_does_not_match_the_manifest_is_discarded() {
    let f = Fixture::new().await;
    let wrong = manifest::sha512_base64(b"something else");
    f.publish("2.0.0", b"installer", Some(&wrong));
    let updater = f.updater(true);
    updater.check(CheckTrigger::Manual).await;
    let state = wait_status(&updater, |s| !matches!(s, UpdateStatus::Downloading { .. })).await;
    assert_eq!(failed_kind(&state), Some(UpdateErrorKind::Other));
    // 没有留下文件
    let left: Vec<_> = std::fs::read_dir(f.download_dir())
        .unwrap()
        .flatten()
        .collect();
    assert!(left.is_empty(), "{left:?}");
    assert!(!updater.install_on_quit());

    // 清单中的大小与下载的不同
    let manifest = f.publish("2.0.0", b"installer", None);
    let bigger = manifest.replace("size: 9", "size: 8");
    f.server.put("/feed/latest.yml", bigger.clone());
    f.server.put(
        "/feed/latest.yml.minisig",
        f.key
            .sign(bigger.as_bytes(), &manifest_comment("latest.yml", "2.0.0")),
    );
    updater.check(CheckTrigger::Manual).await;
    let state = wait_status(&updater, |s| !matches!(s, UpdateStatus::Downloading { .. })).await;
    assert_eq!(failed_kind(&state), Some(UpdateErrorKind::Other));
}

#[tokio::test]
async fn verified_earlier_download_is_reused() {
    let f = Fixture::new().await;
    f.publish("2.0.0", b"installer", None);
    let first = f.updater(true);
    first.check(CheckTrigger::Manual).await;
    wait_status(&first, |s| matches!(s, UpdateStatus::Downloaded { .. })).await;
    // 应用重启后（没有安装）再检查：文件与这次校验过的清单一致，直接沿用
    let second = f.updater(true);
    second.check(CheckTrigger::Manual).await;
    wait_status(&second, |s| matches!(s, UpdateStatus::Downloaded { .. })).await;
    assert_eq!(f.requests_for("/feed/termlet-2.0.0-win-x64.exe"), 1);
}

/// 沿用已下载的文件时下载任务很快结束（多线程运行时上可能在检查返回前就结束）：结果不被“正在下载”覆盖。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quick_download_result_is_not_overwritten() {
    let f = Fixture::new().await;
    f.publish("2.0.0", b"installer", None);
    let first = f.updater(true);
    first.check(CheckTrigger::Manual).await;
    wait_status(&first, |s| matches!(s, UpdateStatus::Downloaded { .. })).await;
    for _ in 0..10 {
        let again = f.updater(true);
        again.check(CheckTrigger::Manual).await;
        wait_status(&again, |s| matches!(s, UpdateStatus::Downloaded { .. })).await;
    }
}

#[tokio::test]
async fn falls_back_to_the_next_feed() {
    let f = Fixture::new().await;
    f.publish("2.0.0", b"installer", None);
    let remote = RemoteConfigSource::with(
        Url::parse(&f.server.url("/update.json")).unwrap(),
        f.tmp.path().join("remote-config.json"),
        vec![f.key.public_key()],
    );
    let dead = format!("{}/feed", closed_port_url().await);
    let updater = Updater::new(
        f.options(
            FeedSource::Remote {
                config: Box::new(remote.clone()),
                builtin: vec![dead.clone(), f.feed()],
                trusted_prefixes: TRUSTED_FEED_PREFIXES
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            },
            false,
        ),
    );
    let state = updater.check(CheckTrigger::Manual).await;
    assert_eq!(
        state.status,
        UpdateStatus::Available {
            version: "2.0.0".into()
        }
    );
    // 只有连不上的更新源时：无法连接
    let updater = Updater::new(f.options(
        FeedSource::Remote {
            config: Box::new(remote),
            builtin: vec![dead],
            trusted_prefixes: Vec::new(),
        },
        false,
    ));
    let state = updater.check(CheckTrigger::Manual).await;
    assert_eq!(failed_kind(&state), Some(UpdateErrorKind::Network));
}

#[tokio::test]
async fn remote_config_can_pause_updates_and_cannot_redirect_them() {
    let f = Fixture::new().await;
    f.publish("2.0.0", b"installer", None);
    let remote = RemoteConfigSource::with(
        Url::parse(&f.server.url("/update.json")).unwrap(),
        f.tmp.path().join("remote-config.json"),
        vec![f.key.public_key()],
    );
    let now = crate::time::now_ms() / 1000 * 1000;
    let sign_config = |text: &str, issued: i64| {
        f.server.put("/update.json", text.to_string());
        f.server.put(
            "/update.json.minisig",
            f.key.sign(
                text.as_bytes(),
                &remote_config_comment(issued, issued + 86_400_000),
            ),
        );
    };
    let feeds = FeedSource::Remote {
        config: Box::new(remote),
        builtin: vec![f.feed()],
        trusted_prefixes: TRUSTED_FEED_PREFIXES
            .iter()
            .map(|s| s.to_string())
            .collect(),
    };
    sign_config(r#"{"disabled":true,"message":"维护中"}"#, now);
    let updater = Updater::new(f.options(feeds.clone(), false));
    let state = updater.check(CheckTrigger::Manual).await;
    assert_eq!(state.status, UpdateStatus::Disabled);
    assert_eq!(state.message.as_deref(), Some("维护中"));

    // 不在白名单中的更新源忽略，照常用内置地址
    let evil = f.server.url("/evil");
    sign_config(
        &format!(r#"{{"feeds":["{evil}"],"message":"换源"}}"#),
        now + 1000,
    );
    let updater = Updater::new(f.options(feeds, false));
    let state = updater.check(CheckTrigger::Manual).await;
    assert!(matches!(state.status, UpdateStatus::Available { .. }));
    assert_eq!(state.message.as_deref(), Some("换源"));
    assert!(!f.server.requests().iter().any(|r| r.starts_with("/evil")));
}

#[tokio::test]
async fn off_disables_checking() {
    let f = Fixture::new().await;
    let updater = Updater::new(f.options(FeedSource::Off, true));
    let state = updater.check(CheckTrigger::Manual).await;
    assert_eq!(state.status, UpdateStatus::Disabled);
    assert_eq!(state.message, None);
    assert!(f.server.requests().is_empty());
}

#[tokio::test]
async fn automatic_checks_respect_the_interval() {
    let f = Fixture::new().await;
    f.publish("1.0.0", b"installer", None);
    let updater = f.updater(false);
    assert_eq!(
        updater.check(CheckTrigger::Auto).await.status,
        UpdateStatus::Latest
    );
    let count = f.server.requests().len();
    updater.check(CheckTrigger::Auto).await;
    assert_eq!(f.server.requests().len(), count);
    // 手动检查不受限制
    updater.check(CheckTrigger::Manual).await;
    assert_eq!(f.server.requests().len(), count + 2);
}

#[tokio::test]
async fn test_key_only_counts_on_loopback_feeds() {
    let f = Fixture::new().await;
    f.publish("2.0.0", b"installer", None);
    // 内置公钥是别的密钥，测试公钥是签名用的这把
    let mut options = f.options(FeedSource::Fixed(f.feed()), false);
    options.keys = vec![TestKey::random().public_key()];
    options.test_public_key = Some(f.key.public_key());
    let state = Updater::new(options).check(CheckTrigger::Manual).await;
    assert!(matches!(state.status, UpdateStatus::Available { .. }));

    let builtin = vec!["BUILTIN".to_string()];
    for feed in [
        "http://127.0.0.1:8080/feed",
        "http://localhost:1/x",
        "http://[::1]:2/",
    ] {
        assert!(trusted_keys(feed, &builtin, Some("TEST")).contains(&"TEST".to_string()));
    }
    for feed in [
        "https://dl.termlet.dev",
        "https://github.com/termlet/termlet/releases/latest/download",
        "http://127.0.0.1.evil.com/feed",
        "http://192.168.1.2/feed",
        "not a url",
    ] {
        assert_eq!(trusted_keys(feed, &builtin, Some("TEST")), builtin);
    }
    assert_eq!(
        trusted_keys("http://127.0.0.1/feed", &builtin, None),
        builtin
    );
}

/// 下载站（防火墙只放行不带参数的请求）上取版本描述文件不带防缓存的参数，别的更新源带。
#[test]
fn manifest_urls_skip_cache_busting_on_the_download_site() {
    let url = |base: &str, name: &str| manifest_url(&Url::parse(base).unwrap(), name).unwrap();
    assert_eq!(
        url("https://dl.termlet.dev/", "latest.yml").as_str(),
        "https://dl.termlet.dev/latest.yml"
    );
    assert_eq!(
        url("https://DL.termlet.dev/", "latest.yml.minisig").as_str(),
        "https://dl.termlet.dev/latest.yml.minisig"
    );
    for base in [
        "https://github.com/termlet/termlet/releases/latest/download/",
        "https://dl.termlet.dev.evil.com/",
        "http://127.0.0.1:8080/feed/",
    ] {
        let url = url(base, "latest.yml");
        assert!(url.path().ends_with("/latest.yml"), "{url}");
        assert!(
            url.query().is_some_and(|q| q.starts_with("noCache=")),
            "{url}"
        );
    }
    // 第一个内置更新源就是下载站
    assert_eq!(BUILTIN_FEEDS[0], "https://dl.termlet.dev");
}

#[test]
fn installer_arguments_match_the_old_version() {
    assert_eq!(installer_args(true), ["/S", "--updated", "--force-run"]);
    assert_eq!(installer_args(false), ["/S", "--updated"]);
}

#[tokio::test]
async fn nothing_to_install_on_quit_without_a_download() {
    let f = Fixture::new().await;
    let updater = f.updater(true);
    assert!(!updater.install_on_quit());
    assert!(matches!(
        updater.install_now(),
        Err(UpdateError::NotDownloaded)
    ));
}

#[test]
fn cleaning_keeps_only_newer_installers() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("updates");
    std::fs::create_dir_all(&dir).unwrap();
    for name in [
        "termlet-0.9.0-win-x64.exe",
        "termlet-1.0.0-win-x64.exe",
        "termlet-2.0.0-win-x64.exe",
        "termlet-2.0.0-win-x64.exe.part",
        "termlet-3.0.0-win-arm64.exe",
        "other.txt",
    ] {
        std::fs::write(dir.join(name), b"x").unwrap();
    }
    let updater = Updater::new(UpdaterOptions {
        current_version: "1.0.0".into(),
        download_dir: dir.clone(),
        install_dir: None,
        feeds: FeedSource::Off,
        keys: Vec::new(),
        test_public_key: None,
        platform: PLATFORM.into(),
        metadata_file: "latest.yml".into(),
    });
    updater.clean_downloads();
    let mut left: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["termlet-2.0.0-win-x64.exe"]);
    // 目录不存在时什么也不做
    std::fs::remove_dir_all(&dir).unwrap();
    updater.clean_downloads();

    assert_eq!(
        installer_version("termlet-1.0.0-beta.1-win-x64.exe", PLATFORM),
        Some("1.0.0-beta.1")
    );
    assert_eq!(installer_version("termlet--win-x64.exe", PLATFORM), None);
    assert_eq!(
        installer_version("termlet-1.0.0-win-x64.zip", PLATFORM),
        None
    );
}

// 下载进度：百分比核对完才到 100（最多 99），总大小未知时为 0；百分比变了或隔了 PROGRESS_INTERVAL 才推给界面
#[test]
fn progress_is_reported_by_percent_or_interval() {
    assert_eq!(download_percent(0, Some(1000)), 0);
    assert_eq!(download_percent(450, Some(1000)), 45);
    assert_eq!(download_percent(1000, Some(1000)), 99);
    assert_eq!(download_percent(5000, None), 0);

    let start = Instant::now();
    let mut throttle = ProgressThrottle::new(start);
    assert!(!throttle.due(start, 0), "刚开始、百分比没变");
    assert!(throttle.due(start, 1), "百分比变了");
    assert!(!throttle.due(start + Duration::from_millis(100), 1));
    assert!(throttle.due(start + PROGRESS_INTERVAL, 1), "隔够了时间");
    assert!(!throttle.due(start + PROGRESS_INTERVAL + Duration::from_millis(10), 1));
}
