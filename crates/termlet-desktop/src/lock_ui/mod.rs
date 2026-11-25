//! 应用锁、自动更新、诊断信息与崩溃记录的界面（R13）。核心在 `termlet_core` 的 `applock`、`update`、
//! `diag`、`crash` 与 `remote_config`。
//!
//! - **共享状态**：[`install`] 建好 [`Services`]（应用锁、更新、崩溃记录）放进全局量。锁定状态与更新状态各有一个
//!   可观察的实体（[`lock_model`]、[`update_model`]）：核心的 watch 有变化时由 GPUI 任务转过来，界面自己的操作
//!   （立即锁定、解锁）完成后当场同步。锁定状态变化时顺带告诉命令行服务（`cli_bridge::set_app_locked`）。
//! - **锁屏**：[`LockScreen`] 代替锁定窗口的全部内容（含标题栏），画在对话框、通知之上；见它的说明。
//! - **设置**：“安全”页 [`SecurityPage`]，“关于”页中的更新、隐私（使用统计与崩溃报告）与诊断 [`AboutExtras`]。
//! - **定时**：每小时检查更新（按设置），并检查一次使用统计（`usage_ui`）。
//! - **提示**：崩溃记录 [`check_crashes`]，标题栏的“新版本已下载” [`UpdateBadge`]。
//! - **退出**：“重启并安装”经 [`restart_and_install`]（先经主会话登记的 [`set_quit_guard`] 确认）；退出时
//!   [`install_on_quit`] 安装已下载的更新。

mod about;
mod crash;
mod lock_screen;
mod master_password;
mod progress;
mod security;
#[cfg(test)]
mod tests;
mod update;

use std::cell::Cell;
use std::num::NonZeroIsize;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::{App, AppContext as _, Context, Entity, Global, Task, Window, actions};
use termlet_core::applock::{AppLock, LockPlatform, LockStatus, Verification};
use termlet_core::crash::CrashLog;
use termlet_core::diag::Diagnostics;
use termlet_core::remote_config::RemoteConfigSource;
use termlet_core::store::StoreEvent;
use termlet_core::update::{
    CheckTrigger, FeedSource, UpdateState, UpdateStatus, Updater, UpdaterOptions,
};

use crate::app_state::AppState;
use crate::t;

pub(crate) use about::{AboutExtras, AboutPart, app_info, crash_report_shown};
pub(crate) use crash::{check_crashes, open_crash_log};
pub(crate) use lock_screen::LockScreen;
pub(crate) use security::SecurityPage;
pub(crate) use update::UpdateBadge;

actions!(
    lock_ui,
    [
        /// 立即锁定（已启用应用锁时；快捷键默认 Ctrl+Shift+L）
        LockNow
    ]
);

/// 定时自动检查更新的间隔（核心另外限制两次自动检查至少隔 6 小时）。
const UPDATE_POLL: Duration = Duration::from_secs(60 * 60);

/// 退出应用前的确认（主会话提供）：有未保存的文件、未完成的传输时询问，用户同意后调用传入的 `proceed`；
/// 没有需要确认的直接调用它。不调用即为取消。
pub(crate) type QuitGuard =
    Rc<dyn Fn(Box<dyn FnOnce(&mut Window, &mut App)>, &mut Window, &mut App)>;

/// 生成诊断信息时补充运行中的模块的信息（主会话提供）：终端、SFTP 会话与传输、运行中的转发的数量，
/// 云同步（`termlet_core::diag::sync_section`）等，用 [`Diagnostics::section`] 加上。
pub(crate) type DiagnosticsHook = Rc<dyn Fn(Diagnostics, &App) -> Diagnostics>;

/// 应用锁、更新与崩溃记录（全局量，[`install`] 建立）。
pub(crate) struct Services {
    /// 打开失败时为 `None`，按没有启用处理
    lock: Option<AppLock>,
    lock_model: Entity<LockModel>,
    updater: Updater,
    update_model: Entity<UpdateModel>,
    crash: CrashLog,
    logs_dir: PathBuf,
    /// 读取崩溃报告接收地址的远程配置；环境变量指定了接收地址、测试中为 `None`
    remote: Option<RemoteConfigSource>,
    quit_guard: Option<QuitGuard>,
    diagnostics_hook: Option<DiagnosticsHook>,
    /// 这次运行已经检查过崩溃记录（多个窗口只提示一次）
    crashes_checked: Cell<bool>,
    /// 上一次看到的“自动发送崩溃报告”“自动检查更新”（设置变化时据此判断改没改）
    crash_report: Cell<bool>,
    auto_check: Cell<bool>,
    _poll: Task<()>,
}

impl Global for Services {}

/// 锁定状态（可观察）：主会话据此在各窗口中盖上或去掉锁屏。
pub(crate) struct LockModel {
    status: LockStatus,
    _watch: Task<()>,
}

impl LockModel {
    fn new(lock: Option<&AppLock>, cx: &mut Context<Self>) -> Self {
        let Some(lock) = lock else {
            return Self {
                status: LockStatus::default(),
                _watch: Task::ready(()),
            };
        };
        let mut changes = lock.subscribe();
        let watch = cx.spawn(async move |this, cx| {
            while changes.changed().await.is_ok() {
                let status = *changes.borrow_and_update();
                if this.update(cx, |model, cx| model.set(status, cx)).is_err() {
                    break;
                }
            }
        });
        Self {
            status: lock.status(),
            _watch: watch,
        }
    }

    pub(crate) fn is_locked(&self) -> bool {
        self.status.locked
    }

    fn set(&mut self, status: LockStatus, cx: &mut Context<Self>) {
        if self.status == status {
            return;
        }
        if self.status.locked != status.locked {
            if status.locked {
                tracing::info!("应用已锁定");
            } else {
                tracing::info!("应用已解锁");
            }
            crate::cli_bridge::set_app_locked(status.locked, cx);
        }
        if self.status.enabled != status.enabled {
            tracing::info!(enabled = status.enabled, "应用锁启用状态变化");
        }
        self.status = status;
        cx.notify();
    }
}

/// 崩溃报告的接收地址（来自签名的远程配置）读到了没有。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Endpoint {
    Unknown,
    Loading,
    /// 远程配置给出的地址；没有、读不到时为 `None`
    Loaded(Option<String>),
}

/// 更新状态与崩溃报告的接收地址（都来自发布方，可观察）。
pub(crate) struct UpdateModel {
    state: UpdateState,
    /// 下载中的速度与剩余时间（按收到进度的时间记）
    meter: progress::DownloadMeter,
    crash_endpoint: Endpoint,
    _watch: Task<()>,
}

impl UpdateModel {
    fn new(updater: &Updater, cx: &mut Context<Self>) -> Self {
        let mut changes = updater.subscribe();
        let watch = cx.spawn(async move |this, cx| {
            while changes.changed().await.is_ok() {
                let state = changes.borrow_and_update().clone();
                let alive = this.update(cx, |model, cx| {
                    model.apply(state, std::time::Instant::now(), cx)
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        Self {
            state: updater.state(),
            meter: progress::DownloadMeter::default(),
            crash_endpoint: Endpoint::Unknown,
            _watch: watch,
        }
    }

    pub(crate) fn state(&self) -> &UpdateState {
        &self.state
    }

    /// 下载中的速度与剩余时间。
    pub(crate) fn meter(&self) -> &progress::DownloadMeter {
        &self.meter
    }

    /// 换上核心推来的状态：下载中时记下进度的读数（换了版本、不在下载时从头算）。
    fn apply(&mut self, state: UpdateState, now: std::time::Instant, cx: &mut Context<Self>) {
        if self.state == state {
            return;
        }
        match (&self.state.status, &state.status) {
            (
                UpdateStatus::Downloading { version: old, .. },
                UpdateStatus::Downloading {
                    version,
                    downloaded,
                    ..
                },
            ) if old == version => self.meter.record(now, *downloaded),
            (_, UpdateStatus::Downloading { downloaded, .. }) => {
                self.meter.reset();
                self.meter.record(now, *downloaded);
            }
            _ => self.meter.reset(),
        }
        self.state = state;
        cx.notify();
    }

    /// 直接换上一个状态（UI 测试、开发构建中预览标题栏的提示时用），`now` 为这次读数的时间。
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn set_state(
        &mut self,
        state: UpdateState,
        now: std::time::Instant,
        cx: &mut Context<Self>,
    ) {
        self.apply(state, now, cx);
    }

    #[cfg(test)]
    pub(crate) fn crash_endpoint(&self) -> &Endpoint {
        &self.crash_endpoint
    }

    fn set_endpoint(&mut self, endpoint: Endpoint, cx: &mut Context<Self>) {
        if self.crash_endpoint != endpoint {
            self.crash_endpoint = endpoint;
            cx.notify();
        }
    }

    /// 有可用的崩溃报告接收地址（环境变量优先，见 `termlet_core::crash::report_url`）。
    pub(crate) fn has_crash_endpoint(&self) -> bool {
        match &self.crash_endpoint {
            Endpoint::Loaded(endpoint) => {
                termlet_core::crash::report_url(endpoint.as_deref()).is_some()
            }
            _ => false,
        }
    }
}

/// 建立 [`Services`] 用到的东西（正式运行与测试各一套）。
struct Setup {
    lock: Option<AppLock>,
    updater: Updater,
    logs_dir: PathBuf,
    remote: Option<RemoteConfigSource>,
}

/// panic 时写崩溃记录（`logs_dir` 为 `DataDir::logs_dir()`）。主会话在初始化日志之后、打开窗口之前尽早调用。
pub(crate) fn install_crash_hook(logs_dir: &Path) {
    termlet_core::crash::install(logs_dir);
}

/// 启动时（[`AppState`] 装好、`cli_bridge::start` 之前：命令行服务从开始监听起就要知道是否锁定）调用一次：
/// 打开应用锁并开始自动锁定，建立更新
/// （按设置后台检查一次、之后每小时检查），按设置处理待发送的崩溃报告。
pub(crate) fn install(cx: &mut App) {
    let data_dir = AppState::global(cx).data_dir.clone();
    let lock = match AppLock::open(AppState::store(cx)) {
        Ok(lock) => {
            // 丢弃 JoinHandle 不会结束任务；所有 AppLock 都丢弃后它自行结束
            drop(lock.spawn_auto_lock());
            Some(lock)
        }
        Err(e) => {
            tracing::error!("打开应用锁失败，按没有启用处理：{e}");
            None
        }
    };
    // 环境变量指定了接收地址（或 off）时不读远程配置
    let remote = std::env::var_os(termlet_core::crash::CRASH_URL_ENV)
        .is_none()
        .then(|| RemoteConfigSource::new(&data_dir));
    install_with(
        Setup {
            lock,
            updater: Updater::from_env(&data_dir),
            logs_dir: data_dir.logs_dir(),
            remote,
        },
        cx,
    );
}

/// UI 测试用：系统功能（空闲、锁屏、Windows Hello）都不可用，不检查更新、不读远程配置，崩溃记录在临时目录中。
/// 在 `AppState::install_in_memory` 之后调用。
pub(crate) fn install_in_memory(cx: &mut App) {
    let lock = AppLock::open_with(AppState::store(cx), Arc::new(NoSystem)).ok();
    let dir = std::env::temp_dir().join(format!(
        "termlet-lock-ui-{}",
        termlet_core::id::random_token(8)
    ));
    install_with(
        Setup {
            lock,
            updater: offline_updater(dir.join("updates")),
            logs_dir: dir.join("logs"),
            remote: None,
        },
        cx,
    );
}

/// 不访问网络的更新（测试用）。
fn offline_updater(download_dir: PathBuf) -> Updater {
    Updater::new(UpdaterOptions {
        current_version: termlet_core::VERSION.to_string(),
        download_dir,
        install_dir: None,
        feeds: FeedSource::Off,
        keys: Vec::new(),
        test_public_key: None,
        platform: "win-x64".into(),
        metadata_file: "latest.yml".into(),
    })
}

/// 没有任何系统功能的平台（测试用）。
struct NoSystem;

impl LockPlatform for NoSystem {
    fn idle_time(&self) -> Option<Duration> {
        None
    }

    fn session_locked(&self) -> Option<bool> {
        None
    }

    fn system_auth_available(&self) -> bool {
        false
    }

    fn system_verify(&self, _: Option<NonZeroIsize>, _: &str) -> Verification {
        Verification::Unavailable
    }
}

fn install_with(setup: Setup, cx: &mut App) {
    let Setup {
        lock,
        updater,
        logs_dir,
        remote,
    } = setup;
    let store = AppState::store(cx);
    let settings = store.settings();
    let local = store.local_settings();

    let lock_model = cx.new(|cx| LockModel::new(lock.as_ref(), cx));
    crate::cli_bridge::set_app_locked(lock_model.read(cx).is_locked(), cx);

    let update_model = cx.new(|cx| UpdateModel::new(&updater, cx));
    // 开发构建：按环境变量预览标题栏的更新提示（见 update::start_preview）
    #[cfg(debug_assertions)]
    update::start_preview(&update_model, cx);
    // 启动后检查一次（检查时先清理下载目录）；不检查时只清理
    if settings.general.auto_check_update {
        spawn_check(&updater, CheckTrigger::Auto);
    } else {
        let updater = updater.clone();
        cx.background_spawn(async move { updater.clean_downloads() })
            .detach();
    }
    let poll = cx.spawn({
        let updater = updater.clone();
        async move |cx| {
            loop {
                cx.background_executor().timer(UPDATE_POLL).await;
                let auto = cx.update(|cx| AppState::store(cx).settings().general.auto_check_update);
                if auto {
                    spawn_check(&updater, CheckTrigger::Auto);
                }
                // 使用统计也每小时检查一次（今天还没发成功时再试）
                cx.update(crate::usage_ui::check);
            }
        }
    });

    termlet_core::crash::set_reporting(local.logs.crash_report);
    cx.set_global(Services {
        lock,
        lock_model,
        updater,
        update_model,
        crash: CrashLog::new(logs_dir.clone()),
        logs_dir,
        remote,
        quit_guard: None,
        diagnostics_hook: None,
        crashes_checked: Cell::new(false),
        crash_report: Cell::new(local.logs.crash_report),
        auto_check: Cell::new(settings.general.auto_check_update),
        _poll: poll,
    });
    let store_model = AppState::global(cx).store.clone();
    cx.subscribe(&store_model, |_, event: &StoreEvent, cx| {
        on_store_event(event, cx)
    })
    .detach();
    // 开启了自动上报：读到接收地址后发送待发送的；关闭时把待发送的标为不发送
    flush_crashes(cx);
}

fn spawn_check(updater: &Updater, trigger: CheckTrigger) {
    let updater = updater.clone();
    drop(termlet_core::runtime::spawn(async move {
        updater.check(trigger).await;
    }));
}

/// 设置变化：“自动发送崩溃报告”改了时重新设置并处理待发送的；“自动检查更新”打开时检查一次。
fn on_store_event(event: &StoreEvent, cx: &mut App) {
    let Some(services) = cx.try_global::<Services>() else {
        return;
    };
    match event {
        StoreEvent::LocalSettingsChanged => {
            let enabled = AppState::store(cx).local_settings().logs.crash_report;
            if services.crash_report.replace(enabled) != enabled {
                termlet_core::crash::set_reporting(enabled);
                flush_crashes(cx);
            }
        }
        StoreEvent::SettingsChanged => {
            let auto = AppState::store(cx).settings().general.auto_check_update;
            if !services.auto_check.replace(auto) && auto {
                spawn_check(&services.updater, CheckTrigger::Auto);
            }
        }
        _ => {}
    }
}

/// 按设置处理待发送的崩溃报告：关闭时标为不发送；开启时发送（还不知道接收地址时先读，读到后再发）。
fn flush_crashes(cx: &mut App) {
    let Some(services) = cx.try_global::<Services>() else {
        return;
    };
    let enabled = services.crash_report.get();
    let crash = services.crash.clone();
    let url = if enabled {
        match services.update_model.read(cx).crash_endpoint.clone() {
            Endpoint::Loaded(endpoint) => termlet_core::crash::report_url(endpoint.as_deref()),
            Endpoint::Loading => return,
            Endpoint::Unknown => {
                ensure_crash_endpoint(cx);
                return;
            }
        }
    } else {
        None
    };
    // 开启了但没有接收地址：留到以后
    if enabled && url.is_none() {
        return;
    }
    drop(termlet_core::runtime::spawn(async move {
        let flushed = crash.flush(url, enabled).await;
        if let Some(error) = flushed.error {
            tracing::info!("崩溃报告留到下次发送：{error}");
        }
    }));
}

/// 读取崩溃报告的接收地址（每次运行最多一次），读到后按设置处理待发送的。
pub(crate) fn ensure_crash_endpoint(cx: &mut App) {
    let Some(services) = cx.try_global::<Services>() else {
        return;
    };
    let model = services.update_model.clone();
    if model.read(cx).crash_endpoint != Endpoint::Unknown {
        return;
    }
    let Some(source) = services.remote.clone() else {
        model.update(cx, |m, cx| m.set_endpoint(Endpoint::Loaded(None), cx));
        flush_crashes(cx);
        return;
    };
    model.update(cx, |m, cx| m.set_endpoint(Endpoint::Loading, cx));
    cx.spawn(async move |cx| {
        let config = termlet_core::runtime::spawn(async move { source.fetch().await })
            .await
            .ok()
            .flatten();
        let endpoint = config
            .as_ref()
            .and_then(|c| c.crash_report_endpoint())
            .map(str::to_string);
        cx.update(|cx| {
            model.update(cx, |m, cx| m.set_endpoint(Endpoint::Loaded(endpoint), cx));
            flush_crashes(cx);
        });
    })
    .detach();
}

// ---------------------------------------------------------------- 对外接口

fn services(cx: &App) -> Option<&Services> {
    cx.try_global::<Services>()
}

/// 应用锁（打开失败、没有安装时为 `None`）。
pub(crate) fn app_lock(cx: &App) -> Option<AppLock> {
    services(cx).and_then(|s| s.lock.clone())
}

/// 锁定状态的实体：`cx.observe` 它，在 [`LockModel::is_locked`] 变化时盖上或去掉锁屏。
pub(crate) fn lock_model(cx: &App) -> Option<Entity<LockModel>> {
    services(cx).map(|s| s.lock_model.clone())
}

/// 更新状态的实体。
pub(crate) fn update_model(cx: &App) -> Option<Entity<UpdateModel>> {
    services(cx).map(|s| s.update_model.clone())
}

/// 程序日志的目录（“打开日志目录”）。
pub(crate) fn logs_dir(cx: &App) -> Option<PathBuf> {
    services(cx).map(|s| s.logs_dir.clone())
}

/// 现在是否锁定（直接读应用锁，比 [`LockModel`] 的通知早）。锁定期间要拒绝的操作据此判断。
pub(crate) fn is_locked(cx: &App) -> bool {
    services(cx)
        .and_then(|s| s.lock.as_ref())
        .is_some_and(AppLock::is_locked)
}

/// 应用锁打不开（读不到设置）：不知道是否启用了，命令行一侧按已锁定处理。
pub(crate) fn is_unavailable(cx: &App) -> bool {
    services(cx).is_some_and(|s| s.lock.is_none())
}

/// 是否已启用应用锁（标题栏的锁按钮、命令面板的“立即锁定”据此显示）。
pub(crate) fn is_enabled(cx: &App) -> bool {
    services(cx)
        .and_then(|s| s.lock.as_ref())
        .is_some_and(|lock| lock.status().enabled)
}

/// 立即锁定（快捷键、标题栏的锁按钮、命令面板、设置页）。没有启用应用锁时什么也不做，返回 false。
pub(crate) fn lock_now(cx: &mut App) -> bool {
    let Some(lock) = app_lock(cx) else {
        return false;
    };
    tracing::info!("立即锁定");
    let locked = lock.lock_now().is_ok();
    sync_lock_model(cx);
    locked
}

/// 把应用锁现在的状态同步到 [`LockModel`]（界面自己的操作完成后调用，不等 watch 的通知）。
pub(crate) fn sync_lock_model(cx: &mut App) {
    let Some(services) = services(cx) else {
        return;
    };
    let Some(status) = services.lock.as_ref().map(AppLock::status) else {
        return;
    };
    let model = services.lock_model.clone();
    model.update(cx, |model, cx| model.set(status, cx));
}

/// 登记退出前的确认（“重启并安装”用），见 [`QuitGuard`]。
pub(crate) fn set_quit_guard(
    guard: impl Fn(Box<dyn FnOnce(&mut Window, &mut App)>, &mut Window, &mut App) + 'static,
    cx: &mut App,
) {
    if cx.has_global::<Services>() {
        cx.global_mut::<Services>().quit_guard = Some(Rc::new(guard));
    }
}

/// 登记生成诊断信息时补充信息的函数，见 [`DiagnosticsHook`]。
pub(crate) fn set_diagnostics_hook(
    hook: impl Fn(Diagnostics, &App) -> Diagnostics + 'static,
    cx: &mut App,
) {
    if cx.has_global::<Services>() {
        cx.global_mut::<Services>().diagnostics_hook = Some(Rc::new(hook));
    }
}

/// “重启并安装”：先经 [`set_quit_guard`] 登记的确认，再核对并启动安装程序（装完启动 Termlet），成功后退出应用
/// （真正退出，不转入无窗口模式）；启动失败时提示。
pub(crate) fn restart_and_install(window: &mut Window, cx: &mut App) {
    let Some(services) = services(cx) else {
        return;
    };
    let updater = services.updater.clone();
    let guard = services.quit_guard.clone();
    let proceed: Box<dyn FnOnce(&mut Window, &mut App)> = Box::new(move |window, cx| {
        // 安装前要再算一次安装包的 SHA-512，放到后台
        let task = cx.background_spawn(async move { updater.install_now() });
        window
            .spawn(cx, async move |cx| {
                let result = task.await;
                let _ = cx.update(|window, cx| match result {
                    Ok(()) => crate::cli_bridge::quit_now(cx),
                    Err(e) => {
                        tracing::warn!("启动安装程序失败：{e}");
                        crate::notify::show(
                            crate::notify::Notice::error(t!("lock_ui.update.install_failed")),
                            window,
                            cx,
                        );
                    }
                });
            })
            .detach();
    });
    match guard {
        Some(guard) => guard(proceed, window, cx),
        None => proceed(window, cx),
    }
}

/// 退出应用时调用（`on_app_quit`、无窗口模式空闲退出）：已下载更新时静默安装（装完不启动），返回是否启动了安装程序。
pub(crate) fn install_on_quit(cx: &App) -> bool {
    services(cx).is_some_and(|s| s.updater.install_on_quit())
}

/// 窗口的 HWND（Windows Hello 的验证框属于它、出现在它前面）。取不到时（如测试中的窗口）为 `None`，
/// 核心改用不属于窗口的验证框（可能出现在 Termlet 后面）。
#[cfg(windows)]
fn window_hwnd(window: &Window) -> Option<NonZeroIsize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    // `Window` 自己也有 `window_handle`（返回 GPUI 的窗口句柄），这里要的是 raw-window-handle 的
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd),
        _ => None,
    }
}

/// 其他系统没有 HWND，系统验证也用不到它。
#[cfg(not(windows))]
fn window_hwnd(_window: &Window) -> Option<NonZeroIsize> {
    None
}
