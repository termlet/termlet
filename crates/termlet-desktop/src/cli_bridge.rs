//! 命令行工具（`termlet`）与界面之间：启动核心的命令行服务，处理它交来的事件（批准危险命令、在终端中执行、
//! 第二个 Termlet 进程交来的启动参数），以及无窗口（headless）模式与退出。
//!
//! - Termlet 没有运行时，`termlet` 以 `--headless` 拉起它（不开窗口），10 分钟没有命令行请求后退出；设置“AI 与自动化”
//!   中可以关掉自动启动（[`sync_launch_config`] 写给命令行读），关掉后 `termlet` 提示先打开 Termlet（`app_not_running`）。
//! - 关闭全部窗口后退出；还有命令行请求在执行（长时间的 `exec`、上传下载）时等它们做完再退出。macOS 上照系统的习惯
//!   关闭最后一个窗口后留在程序坞（不空闲退出），点程序坞图标重新打开主窗口（[`reopen`]），要退出时用“退出 Termlet”。
//! - 系统请求退出（macOS 从程序坞退出、关机、注销）时，有连接、传输、未保存的文件先确认（[`terminate_requested`]）。
//! - 已有实例在运行时，新启动的进程把启动参数交给它（[`forward_to_running`]）后退出；已有实例打开或激活主窗口，
//!   参数中有外部程序调起的连接时照常处理。

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::{ActiveTheme as _, v_flex};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, AsyncApp, Entity, Global, InteractiveElement as _,
    OperatingSystem, ParentElement as _, QuitMode, StatefulInteractiveElement as _, Styled as _,
    TestSupportExt as _, WeakEntity, div,
};
use termlet_core::audit::AuditLog;
use termlet_core::cli::{ApprovalRequest, CliConfig, CliEvent, CliService, TerminalRequest};
use termlet_core::paths::DataDir;
use termlet_core::termrun::TerminalError;

use crate::app_state::AppState;
use crate::close_confirm::CloseSummary;
use crate::i18n;
use crate::prose::prose;
use crate::t;
use crate::terminal_panel::TerminalPanel;
use crate::workspace::Workspace;

/// 无窗口模式（被 `termlet` 拉起）没有命令行请求这么久后退出。
const HEADLESS_IDLE: Duration = Duration::from_secs(10 * 60);
/// 隔这么久看一次是否该退出（窗口全部关闭后等进行中的命令行请求做完、无窗口模式空闲）。
const IDLE_CHECK: Duration = Duration::from_secs(1);
/// 退出时等收尾任务（见 [`spawn_cleanup`]）最多这么久。
const CLEANUP_WAIT: Duration = Duration::from_secs(3);
/// 退出时等会话日志写完最后的输出，合计最多这么久（免得退出卡住）。
const SESSION_LOG_WAIT: Duration = Duration::from_millis(1500);

thread_local! {
    /// Termlet 自己决定了退出（[`quit_now`]）：系统随后的退出请求直接同意，不再确认
    static QUIT_APPROVED: Cell<bool> = const { Cell::new(false) };
    /// 打开过的主窗口：系统请求退出时在 AppKit 的回调中经其中一个进入应用的状态（[`terminate_requested_async`]）
    static WINDOW_HANDLES: RefCell<Vec<AnyWindowHandle>> = const { RefCell::new(Vec::new()) };
}

/// 关闭 Tab、窗口时在后台做的收尾：结束 tmux 会话、关闭 SFTP 会话、停止端口转发。
static CLEANUP: Mutex<Vec<tokio::task::JoinHandle<()>>> = Mutex::new(Vec::new());

/// 在核心的运行时上做一件收尾的事；应用退出时等它做完（[`wait_cleanup`]）。
pub fn spawn_cleanup(future: impl Future<Output = ()> + Send + 'static) {
    let task = termlet_core::runtime::spawn(future);
    let mut tasks = CLEANUP.lock().unwrap_or_else(|e| e.into_inner());
    tasks.retain(|t| !t.is_finished());
    tasks.push(task);
}

/// 应用退出、窗口都已关闭后调用：等收尾任务做完，最多 [`CLEANUP_WAIT`]（服务器上不留下 `termlet-*` 的 tmux 会话）。
pub fn wait_cleanup() {
    let tasks = std::mem::take(&mut *CLEANUP.lock().unwrap_or_else(|e| e.into_inner()));
    if tasks.iter().all(|t| t.is_finished()) {
        return;
    }
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    termlet_core::runtime::spawn(async move {
        for task in tasks {
            let _ = task.await;
        }
        let _ = done_tx.send(());
    });
    if done_rx.recv_timeout(CLEANUP_WAIT).is_err() {
        tracing::info!("退出前的收尾没有在 {CLEANUP_WAIT:?} 内做完");
    }
}

/// 命令行服务与打开着的主窗口。
struct Bridge {
    service: Option<CliService>,
    audit: Option<Arc<AuditLog>>,
    windows: Vec<(AnyWindowHandle, WeakEntity<Workspace>)>,
    /// 窗口已全部关闭，等进行中的命令行请求做完就退出
    closing: bool,
    /// 要退出了（“退出 Termlet”、⌘Q）：窗口关完就退出，macOS 上也不留在程序坞
    quitting: bool,
    /// macOS：关闭了最后一个窗口，留在程序坞（不按无窗口模式空闲退出）
    in_dock: bool,
    /// 最近一次命令行请求的时间：无窗口模式据此空闲退出
    last_activity: Instant,
    /// 新版本的 Termlet 请本实例退出（退出时不再安装本实例下载的更新）
    replaced: bool,
}

impl Global for Bridge {}

/// 启动命令行服务（拿到单实例锁、装好 [`AppState`] 之后）；监听失败只影响命令行，写进日志。
pub fn start(cx: &mut App) {
    let app = AppState::global(cx);
    let store = AppState::store(cx);
    let data_dir: DataDir = app.data_dir.clone();
    // 重装到了别处（旧目录已删除）：PATH 中原来的命令行工具目录换成现在的。便携版不改 PATH
    if !data_dir.is_portable()
        && let Ok(exe) = std::env::current_exe()
    {
        let record = data_dir.cli_path_record();
        let dir = termlet_core::userpath::cli_dir(&exe);
        std::thread::spawn(
            move || match termlet_core::userpath::follow(&record, &dir) {
                Ok(Some(old)) => {
                    tracing::info!(old = %old.display(), "安装位置已改变，已更新 PATH 中的命令行工具目录")
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("更新 PATH 中的命令行工具目录失败：{e}"),
            },
        );
    }
    let audit = match AuditLog::new(store.clone()) {
        Ok(audit) => Some(Arc::new(audit)),
        Err(e) => {
            tracing::error!("打开审计日志失败：{e}");
            None
        }
    };
    let mut service = None;
    if let Some(audit) = &audit {
        let mut config = CliConfig::new(data_dir, store, audit.clone());
        config.locked = crate::lock_ui::is_locked(cx) || crate::lock_ui::is_unavailable(cx);
        match CliService::start(config) {
            Ok((started, events)) => {
                // MCP 的 HTTP 接入按界面语言写工具结果中的说明（界面语言在这之前已经设好）
                started.set_language(mcp_language(i18n::language()));
                service = Some(started);
                cx.spawn(async move |cx| {
                    while let Ok(event) = events.recv().await {
                        cx.update(|cx| handle(event, cx));
                    }
                })
                .detach();
            }
            Err(e) => tracing::error!("命令行服务没有启动：{e}"),
        }
    }
    cx.set_global(Bridge {
        service,
        audit,
        windows: Vec::new(),
        closing: false,
        quitting: false,
        in_dock: false,
        last_activity: Instant::now(),
        replaced: false,
    });
    sync_launch_config(cx);
    // 窗口全部关闭后由这里决定何时退出（等进行中的命令行请求做完）
    cx.set_quit_mode(QuitMode::Explicit);
    cx.on_window_closed(|cx, _| on_window_closed(cx)).detach();
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(IDLE_CHECK).await;
            let quit = cx.update(|cx| {
                let bridge = cx.global::<Bridge>();
                // 长时间的执行、上传下载进行中：做完再退出
                let busy = bridge.service.as_ref().is_some_and(|s| s.in_flight() > 0);
                // 最近的活动：交给界面的事件，以及命令行服务收到的请求（含通过鉴权的 MCP 的 HTTP 请求）
                let last = bridge.service.as_ref().map_or(bridge.last_activity, |s| {
                    s.last_request().max(bridge.last_activity)
                });
                let idle = bridge.closing || last.elapsed() >= HEADLESS_IDLE;
                live_windows(cx).is_empty() && !busy && idle && !bridge.in_dock
            });
            if quit {
                tracing::info!("没有窗口，也没有进行中的命令行请求，退出");
                cx.update(quit_now);
                break;
            }
        }
    })
    .detach();
}

/// 界面语言对应的 MCP 工具说明的语言：简体、繁体中文用中文，其余用英文（核心中的说明只有这两种）。
fn mcp_language(language: i18n::Language) -> termlet_core::proto::Lang {
    match language {
        i18n::Language::ZhCn | i18n::Language::ZhTw => termlet_core::proto::Lang::Zh,
        i18n::Language::En | i18n::Language::Ja | i18n::Language::Ko => {
            termlet_core::proto::Lang::En
        }
    }
}

/// 界面语言变了：MCP 的 HTTP 接入的工具说明随之换（命名管道上的请求按命令行工具自己的语言）。
pub(crate) fn set_language(language: i18n::Language, cx: &App) {
    if let Some(service) = cx.try_global::<Bridge>().and_then(|b| b.service.as_ref()) {
        service.set_language(mcp_language(language));
    }
}

/// 登记应用锁、更新模块要的钩子：“重启并安装”前确认未保存的文件与未完成的传输，诊断信息中补上会话的数量；
/// 退出时先等会话日志写完，再安装已下载的更新。在 [`crate::lock_ui::install`] 之后调用。
pub fn install_hooks(cx: &mut App) {
    crate::lock_ui::set_quit_guard(confirm_quit, cx);
    // macOS：系统请求退出时问这里（见 crate::macos）
    let mut async_cx = cx.to_async();
    crate::macos::on_terminate_request(move || terminate_requested_async(&mut async_cx));
    crate::lock_ui::set_diagnostics_hook(
        |d, cx| {
            let windows = live_windows(cx);
            let (terminals, sftps, transfers) =
                windows
                    .iter()
                    .fold((0usize, 0usize, 0usize), |(t, s, p), (_, w)| {
                        let (terms, sftp, pending) = w.read(cx).session_counts(cx);
                        (t + terms, s + sftp, p + pending)
                    });
            let sync = crate::sync_ui::cloud(cx).status();
            d.section("terminals", terminals as u64)
                .section("sftpSessions", sftps as u64)
                .section("transfers", transfers as u64)
                .section("forwards", crate::forwards::active_count(cx) as u64)
                .section("cloudSync", termlet_core::diag::sync_section(&sync))
        },
        cx,
    );
    cx.on_app_quit(|cx| {
        // 主窗口的位置：还有窗口开着时记下最前面的那个；界面状态等不到稍后的写回，立即写
        crate::window_state::save_on_quit(cx);
        // 所有的退出（关闭全部窗口、无窗口模式空闲、重启并安装、被新版本替换……）都经过这里。
        // 先停止全部会话日志与录制、等它们写完，再启动安装程序（安装时会结束还在运行的 Termlet）
        termlet_core::session_log::stop_all(SESSION_LOG_WAIT);
        termlet_core::recording::stop_all(SESSION_LOG_WAIT);
        let replaced = cx.try_global::<Bridge>().is_some_and(|b| b.replaced);
        if !replaced {
            crate::lock_ui::install_on_quit(cx);
        }
        async {}
    })
    .detach();
}

/// “重启并安装”前：有未保存的文件或未完成的传输时先确认。
fn confirm_quit(
    proceed: Box<dyn FnOnce(&mut gpui_kit::Window, &mut App)>,
    window: &mut gpui_kit::Window,
    cx: &mut App,
) {
    let (unsaved, pending) = live_windows(cx)
        .iter()
        .fold((0usize, 0usize), |(u, p), (_, w)| {
            let (unsaved, pending) = w.read(cx).critical_work(cx);
            (u + unsaved, p + pending)
        });
    if unsaved == 0 && pending == 0 {
        proceed(window, cx);
        return;
    }
    let proceed = Rc::new(RefCell::new(Some(proceed)));
    crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
        let proceed = proceed.clone();
        alert
            .confirm()
            .title(t!("cli.quit.title"))
            .description(prose(t!(
                "cli.quit.description",
                unsaved = unsaved,
                transfers = pending
            )))
            .ok_text(t!("cli.quit.ok"))
            .ok_variant(ButtonVariant::Danger)
            .cancel_text(t!("common.cancel"))
            .on_ok(move |_, window, cx| {
                if let Some(proceed) = proceed.borrow_mut().take() {
                    proceed(window, cx);
                }
                true
            })
    });
}

/// 审计日志（审计对话框用）。
pub fn audit(cx: &App) -> Option<Arc<AuditLog>> {
    cx.try_global::<Bridge>().and_then(|b| b.audit.clone())
}

/// MCP 的 HTTP 接入实际的状态与之后的变化（设置页显示用）；命令行服务没有启动时为 `None`。
pub(crate) fn subscribe_http(
    cx: &App,
) -> Option<tokio::sync::watch::Receiver<termlet_core::cli::HttpStatus>> {
    cx.try_global::<Bridge>()
        .and_then(|b| b.service.as_ref())
        .map(CliService::subscribe_http)
}

/// 现在对照一次设置（端口被占用、对方让出后“重试”）；新的状态经 [`subscribe_http`] 送达。
pub(crate) fn refresh_http(cx: &App) {
    if let Some(service) = cx.try_global::<Bridge>().and_then(|b| b.service.as_ref()) {
        drop(service.refresh_http());
    }
}

/// 应用锁定、解锁：锁定期间命令行一律返回 `app_locked`（应用锁打不开时一直按锁定处理）。
pub fn set_app_locked(locked: bool, cx: &App) {
    if let Some(service) = cx.try_global::<Bridge>().and_then(|b| b.service.as_ref()) {
        service.set_app_locked(locked || crate::lock_ui::is_unavailable(cx));
    }
}

/// 打开一个主窗口并登记。
pub fn open_main_window(cx: &mut App) {
    open_with(crate::main_window_options(cx), cx);
}

/// 为移过来的 Tab 打开一个主窗口（见 `workspace/move_tab.rs`）：大小为 `bounds` 的大小，放在 `bounds` 的位置，
/// `display` 为所在的显示器；返回它与它的工作区。
pub(crate) fn open_window_at(
    bounds: gpui_kit::Bounds<gpui_kit::Pixels>,
    display: Option<gpui_kit::DisplayId>,
    cx: &mut App,
) -> Option<(AnyWindowHandle, Entity<Workspace>)> {
    let mut options = crate::main_window_options(cx);
    options.window_bounds = Some(gpui_kit::WindowBounds::Windowed(bounds));
    options.display_id = display;
    open_with(options, cx)
}

fn open_with(
    options: gpui_kit::WindowOptions,
    cx: &mut App,
) -> Option<(AnyWindowHandle, Entity<Workspace>)> {
    match gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| Workspace::new(window, cx))
    }) {
        Ok((handle, workspace)) => {
            if cx.has_global::<Bridge>() {
                let bridge = cx.global_mut::<Bridge>();
                bridge.windows.push((handle, workspace.downgrade()));
                // 又打开了窗口：不再按之前的退出、留在程序坞处理
                bridge.closing = false;
                bridge.quitting = false;
                bridge.in_dock = false;
            }
            WINDOW_HANDLES.with(|handles| handles.borrow_mut().push(handle));
            Some((handle, workspace))
        }
        Err(e) => {
            tracing::error!("无法打开主窗口：{e}");
            None
        }
    }
}

/// 没有窗口时从菜单栏（macOS 关闭最后一个窗口后留在程序坞）打开一个主窗口，返回它。启动出错时（没有装好命令行服务）
/// 不打开。
pub(crate) fn open_main_window_for_menu(cx: &mut App) -> Option<AnyWindowHandle> {
    if !cx.has_global::<Bridge>() {
        return None;
    }
    tracing::info!("从菜单栏打开主窗口");
    open_main_window(cx);
    live_windows(cx).last().map(|(handle, _)| *handle)
}

/// 新窗口（Ctrl+Shift+N）：与当前窗口一样大，错开一点放，免得正好盖住它。`display` 为当前窗口所在的显示器（位置
/// 按它的坐标换算，各显示器缩放不同时也放在它旁边）。
pub fn open_window_near(
    near: gpui_kit::Bounds<gpui_kit::Pixels>,
    display: Option<gpui_kit::DisplayId>,
    cx: &mut App,
) {
    let mut options = crate::main_window_options(cx);
    let offset = gpui_kit::px(32.);
    options.window_bounds = Some(gpui_kit::WindowBounds::Windowed(gpui_kit::Bounds {
        origin: gpui_kit::point(near.origin.x + offset, near.origin.y + offset),
        size: near.size,
    }));
    options.display_id = display;
    open_with(options, cx);
}

/// 这个窗口的工作区（快捷命令选择器把内容放进撰写栏时用）。
pub fn workspace_in(window: &gpui_kit::Window, cx: &App) -> Option<Entity<Workspace>> {
    let handle = window.window_handle();
    live_windows(cx)
        .into_iter()
        .find(|(h, _)| *h == handle)
        .map(|(_, w)| w)
}

/// 打开着的主窗口（最近打开的在后）。
fn live_windows(cx: &App) -> Vec<(AnyWindowHandle, Entity<Workspace>)> {
    let Some(bridge) = cx.try_global::<Bridge>() else {
        return Vec::new();
    };
    bridge
        .windows
        .iter()
        .filter_map(|(handle, workspace)| workspace.upgrade().map(|w| (*handle, w)))
        .collect()
}

/// 把“Termlet 没有打开时自动在后台启动”写给命令行（它在 Termlet 没有运行时读 `cli-launch.json`）。
/// 启动时与本机设置改变时调用。
pub fn sync_launch_config(cx: &App) {
    let auto_start = AppState::store(cx).local_settings().cli.auto_start;
    let endpoint =
        termlet_core::proto::endpoint::Endpoint::for_data_dir(AppState::global(cx).data_dir.root());
    let config = termlet_core::proto::endpoint::LaunchConfig { auto_start };
    if let Err(e) = endpoint.write_launch_config(&config) {
        tracing::warn!("写下命令行的启动选项失败：{e}");
    }
}

fn on_window_closed(cx: &mut App) {
    if cx.has_global::<Bridge>() {
        cx.global_mut::<Bridge>()
            .windows
            .retain(|(_, w)| w.upgrade().is_some());
    }
    // 主窗口都关了：主机概览的小窗也关掉（一般已随终端的 Tab 关闭；它们关掉后再回到这里）
    if cx.has_global::<Bridge>() && live_windows(cx).is_empty() {
        crate::host_tools::close_overview_windows(cx);
    }
    let open = cx.windows();
    WINDOW_HANDLES.with(|handles| handles.borrow_mut().retain(|h| open.contains(h)));
    if !open.is_empty() {
        return;
    }
    let quitting = cx.try_global::<Bridge>().is_some_and(|b| b.quitting);
    if quitting || quits_with_last_window(cx.platform_info().os) {
        quit_when_idle(cx);
    } else if cx.has_global::<Bridge>() {
        tracing::info!("关闭了最后一个窗口，留在程序坞");
        cx.global_mut::<Bridge>().in_dock = true;
    }
}

/// 关闭最后一个窗口后是否随之退出：Windows、Linux 退出；macOS 的习惯是留在程序坞，点程序坞图标再打开窗口，
/// 用“退出 Termlet”（⌘Q）退出。
pub(crate) fn quits_with_last_window(os: OperatingSystem) -> bool {
    os != OperatingSystem::Mac
}

/// 要退出了（“退出 Termlet”、⌘Q，关闭全部窗口之前）：窗口关完后退出，macOS 上也不留在程序坞。
pub(crate) fn begin_quit(cx: &mut App) {
    if cx.has_global::<Bridge>() {
        cx.global_mut::<Bridge>().quitting = true;
    }
}

/// 退出应用（已经确认过或不需要确认）：系统随后的退出请求（macOS 上 GPUI 经 `terminate:` 退出）不再确认。
pub(crate) fn quit_now(cx: &mut App) {
    QUIT_APPROVED.set(true);
    cx.quit();
}

/// 点程序坞图标（macOS；GPUI 在没有可见的窗口时才调用）：还有主窗口（都最小化了）时激活最近打开的那个，没有时打开
/// 一个。启动出错时（没有装好命令行服务）什么也不做。
pub(crate) fn reopen(cx: &mut App) {
    if !cx.has_global::<Bridge>() {
        return;
    }
    let windows = live_windows(cx);
    match windows.last() {
        Some((handle, _)) => {
            let _ = handle.update(cx, |_, window, _| window.activate_window());
        }
        None => {
            tracing::info!("点程序坞图标，重新打开主窗口");
            open_main_window(cx);
        }
    }
    cx.activate(true);
}

/// 系统请求退出（macOS 从程序坞退出、关机、注销，见 [`crate::macos`]）：返回现在能不能退出。Termlet 自己发起的
/// 退出（[`quit_now`]）、应用已锁定、没有要说明的现在退出；有连接、传输、未保存的文件时先不退出，改走“退出 Termlet”
/// 的确认（确认后关闭全部窗口再退出；关机、注销因此被取消，确认后要再做一次）。
pub(crate) fn terminate_requested(cx: &mut App) -> bool {
    if QUIT_APPROVED.get() || crate::lock_ui::is_locked(cx) {
        return true;
    }
    let summary = live_windows(cx)
        .iter()
        .fold(CloseSummary::default(), |summary, (_, workspace)| {
            summary.merge(workspace.read(cx).close_summary(cx))
        });
    if !summary.needs_confirm() {
        return true;
    }
    tracing::info!(?summary, "系统请求退出，先确认");
    // 回答了系统之后再弹出确认框；从程序坞退出时 Termlet 可能不在前台
    cx.spawn(async move |cx| {
        cx.update(|cx| {
            cx.activate(true);
            crate::app_menu::request_quit(cx);
        })
    })
    .detach();
    false
}

/// 在 AppKit 的回调中问 [`terminate_requested`]：经还开着的某个主窗口进入应用的状态（借不到时出错而不是 panic）。
/// 没有窗口、借不到状态时现在退出，与没有这个回调时相同。
pub(crate) fn terminate_requested_async(cx: &mut AsyncApp) -> bool {
    if QUIT_APPROVED.get() {
        return true;
    }
    let handles = WINDOW_HANDLES.with(|handles| handles.borrow().clone());
    for handle in handles {
        if let Ok(now) = cx.update_window(handle, |_, _, cx| terminate_requested(cx)) {
            return now;
        }
    }
    true
}

/// 没有窗口了：没有进行中的命令行请求时退出，有的话等它们做完再退出。关闭最后一个窗口（macOS 上要退出时）、没有
/// 窗口时退出（macOS 菜单的“退出 Termlet”）都走这里。
pub(crate) fn quit_when_idle(cx: &mut App) {
    let busy = cx
        .try_global::<Bridge>()
        .and_then(|b| b.service.as_ref())
        .map_or(0, |s| s.in_flight());
    if busy == 0 {
        tracing::info!("窗口已全部关闭，退出");
        quit_now(cx);
    } else {
        // 等进行中的命令行请求做完（见 `start` 中的检查），期间再打开 Termlet 会回到这个实例
        tracing::info!(
            in_flight = busy,
            "窗口已全部关闭，等进行中的命令行请求做完再退出"
        );
        cx.global_mut::<Bridge>().closing = true;
    }
}

fn handle(event: CliEvent, cx: &mut App) {
    if cx.has_global::<Bridge>() {
        cx.global_mut::<Bridge>().last_activity = Instant::now();
    }
    match event {
        CliEvent::Approval(request) => ask_approval(request, cx),
        CliEvent::RunInTerminal(request) => run_in_terminal(request, cx),
        CliEvent::Activate { args, files } => activate(args, files, cx),
        CliEvent::Quit(reply) => quit_for_newer(reply, cx),
        CliEvent::Audited(_) => crate::cli_ui::refresh_open_audit(cx),
    }
}

/// 第二个 Termlet 进程交来的启动参数：处理其中外部程序调起的连接，打开或激活主窗口。会话文件用它交来的内容
/// （它退出后堡垒机就删除文件）；较早的版本不带内容，按路径读。
fn activate(args: Vec<String>, files: Vec<termlet_core::proto::LaunchFile>, cx: &mut App) {
    let target = if files.is_empty() {
        let cwd = std::env::current_dir().unwrap_or_default();
        termlet_core::launch::parse(&args, &cwd)
    } else {
        termlet_core::launch::parse_with(&args, |path| {
            files
                .iter()
                .find(|f| f.path == path)
                .map(|f| f.data.clone())
        })
    };
    tracing::info!(
        args = args.len(),
        files = files.len(),
        launch = target.is_some(),
        "另一个 Termlet 进程交来启动参数"
    );
    if let Some(target) = target {
        crate::launch::queue(target, cx);
    }
    let windows = live_windows(cx);
    match windows.last() {
        Some((handle, workspace)) => {
            let workspace = workspace.downgrade();
            let _ = handle.update(cx, |_, window, cx| {
                // 隐藏在托盘中的先显示出来
                crate::tray::show_window(window, cx);
                let _ = workspace.update(cx, |w, cx| w.take_launches(window, cx));
            });
        }
        // 无窗口模式：现在打开（工作区建好、保险库解锁后取走排队的连接）
        None => open_main_window(cx),
    }
}

/// 版本不同的新进程请本实例退出：没有窗口、没有别的命令行请求时同意并退出（稍后退出，先把回答写出去）。
fn quit_for_newer(reply: tokio::sync::oneshot::Sender<bool>, cx: &mut App) {
    // 这次退出请求自己也算一个进行中的请求
    let idle = live_windows(cx).is_empty()
        && cx
            .try_global::<Bridge>()
            .and_then(|b| b.service.as_ref())
            .is_none_or(|s| s.in_flight() <= 1);
    let _ = reply.send(idle);
    if !idle {
        tracing::info!("新版本的 Termlet 请本实例退出，但还有窗口或进行中的请求，不退出");
        return;
    }
    tracing::info!("新版本的 Termlet 请本实例退出");
    cx.global_mut::<Bridge>().replaced = true;
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(Duration::from_millis(500))
            .await;
        cx.update(quit_now);
    })
    .detach();
}

/// 批准危险命令：在最近使用的主窗口中询问；没有窗口时按拦截处理（丢弃请求）。
fn ask_approval(request: ApprovalRequest, cx: &mut App) {
    let windows = live_windows(cx);
    let target = cx
        .active_window()
        .and_then(|active| windows.iter().find(|(h, _)| *h == active).cloned())
        .or_else(|| windows.last().cloned());
    let Some((handle, _)) = target else {
        tracing::info!(host = %request.host_name, "没有窗口可以询问，拦截命令行的危险命令");
        return;
    };
    // 命令内容不写进日志（审计日志中有）
    tracing::info!(
        host = %request.host_name,
        caller = %request.caller,
        rule = %request.rule.pattern(),
        "命令行工具的危险命令：询问用户"
    );
    let rule = i18n::rule_description(&request.rule);
    let rule = if rule.is_empty() {
        request.rule.pattern().to_string()
    } else {
        rule.to_string()
    };
    let deadline = request.deadline;
    let title = approval_title(&request.host_name, &request.caller);
    let description = t!(
        "cli.approval.description",
        command = request.command.clone(),
        caller = if request.caller.is_empty() {
            t!("cli.approval.unknown_caller").to_string()
        } else {
            request.caller.clone()
        },
        rule = rule
    );
    // 发起请求的是认识的 AI 编程工具（Claude Code、Gemini CLI）时，标题前配上它的标志
    let caller_brand = crate::brand_icons::for_caller(&request.caller);
    let done = request.done();
    let request = Rc::new(RefCell::new(Some(request)));
    let expired = Rc::new(std::cell::Cell::new(false));
    let _ = handle.update(cx, |_, window, cx| {
        crate::tray::show_window(window, cx);
        let (ok, cancel, shown_expired) = (request.clone(), request.clone(), expired.clone());
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, cx| {
            let (ok, cancel, closed) = (ok.clone(), cancel.clone(), cancel.clone());
            // 已不再等待（超时、命令行退出、被锁定）而它不在最上面（不能关）：说明已拦截，只留“关闭”
            if shown_expired.get() {
                return alert
                    .title(title.clone())
                    .description(prose(t!("cli.approval.expired")))
                    .ok_text(t!("common.close"));
            }
            let alert = match caller_brand {
                Some(brand) => {
                    let theme = cx.theme();
                    let color = crate::brand_icons::color(
                        brand,
                        AppState::store(cx).settings().appearance.icons,
                        theme.muted_foreground,
                        theme.background,
                        theme.foreground,
                    );
                    alert.icon(crate::brand_icons::icon(brand, color).size_5())
                }
                None => alert,
            };
            // 倒计时每次绘制时按截止时间重新算（下面的任务每秒刷新窗口）
            let countdown = t!(
                "cli.approval.countdown",
                seconds = seconds_left(deadline, Instant::now())
            );
            alert
                .confirm()
                .title(title.clone())
                .description(
                    v_flex()
                        .gap_2()
                        .child(prose(description.clone()))
                        .child(
                            div()
                                .id("approval-countdown")
                                .test_support()
                                .aria_label(countdown.clone())
                                .child(prose(countdown)),
                        ),
                )
                .ok_text(t!("cli.approval.allow"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("cli.approval.deny"))
                .on_ok(move |_, _, _| {
                    if let Some(request) = ok.borrow_mut().take() {
                        tracing::info!(host = %request.host_name, "命令行工具的危险命令：用户允许");
                        request.answer(true);
                    }
                    true
                })
                .on_cancel(move |_, _, _| {
                    if let Some(request) = cancel.borrow_mut().take() {
                        tracing::info!(host = %request.host_name, "命令行工具的危险命令：用户拒绝");
                        request.answer(false);
                    }
                    true
                })
                // Esc、右上角关闭：拒绝
                .on_close(move |_, _, _| {
                    if let Some(request) = closed.borrow_mut().take() {
                        tracing::info!(host = %request.host_name, "命令行工具的危险命令：关闭询问，按拒绝处理");
                        request.answer(false);
                    }
                })
        });
        let token = crate::dialog_token::DialogToken::take(window, cx);
        // 倒计时：剩余的秒数每变一次刷新一次窗口，回答了、不再等待（请求被取走）后停止
        let ticking = request.clone();
        window
            .spawn(cx, async move |cx| {
                while let Some(wait) = until_next_second(deadline, Instant::now()) {
                    cx.background_executor().timer(wait).await;
                    if ticking.borrow().is_none() || cx.update(|window, _| window.refresh()).is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        // 超时、命令行退出、被锁定：它在最上面时关闭；上面叠了别的对话框时不能关（会关错），改为说明已拦截
        let pending = request.clone();
        window
            .spawn(cx, async move |cx| {
                done.await;
                let taken = pending.borrow_mut().take();
                if let Some(request) = taken {
                    tracing::info!(host = %request.host_name, "命令行工具的危险命令：不再等待批准（超时、命令行退出或已锁定）");
                    let _ = cx.update(|window, cx| {
                        let closed = token.as_ref().is_some_and(|t| t.close(window, cx));
                        if !closed {
                            expired.set(true);
                            window.refresh();
                        }
                    });
                }
            })
            .detach();
    });
}

/// 批准框的标题，按来源写：MCP 的写客户端自报的名字（没有报名字时写“AI 工具”），其余是命令行工具。
fn approval_title(host: &str, caller: &str) -> gpui_kit::SharedString {
    match termlet_core::audit::mcp_client(caller) {
        None => t!("cli.approval.title", host = host),
        Some("") => t!("cli.approval.title_mcp_unnamed", host = host),
        Some(client) => t!("cli.approval.title_mcp", client = client, host = host),
    }
}

/// 批准框中距离超时拦截还剩几秒：向上取整（刚打开时是完整的秒数，最后不到一秒时是 1）。
fn seconds_left(deadline: Instant, now: Instant) -> u64 {
    let left = deadline.saturating_duration_since(now);
    left.as_secs() + u64::from(left.subsec_nanos() > 0)
}

/// 剩余的秒数（[`seconds_left`]）下一次变化前要等多久；已到期时为 `None`。
fn until_next_second(deadline: Instant, now: Instant) -> Option<Duration> {
    let left = deadline.saturating_duration_since(now);
    if left.is_zero() {
        return None;
    }
    Some(match left.subsec_nanos() {
        0 => Duration::from_secs(1),
        nanos => Duration::from_nanos(nanos.into()),
    })
}

/// `termlet exec --terminal`：在这台主机最近打开、连接着的 SSH 终端中执行。
fn run_in_terminal(request: TerminalRequest, cx: &mut App) {
    let panel = newest_terminal(&request, cx);
    let Some(panel) = panel else {
        tracing::info!(host = %request.host_name, "命令行工具要在终端中执行，但这台主机没有打开的终端");
        request.respond(Err(TerminalError::NotOpen));
        return;
    };
    tracing::info!(host = %request.host_name, session = %panel.read(cx).name(), "命令行工具在终端中执行");
    let canceled = request.canceled();
    let task = panel.update(cx, |p, cx| {
        p.run_in_terminal(request.command.clone(), request.limit, canceled, cx)
    });
    cx.spawn(async move |_| {
        let result = task.await;
        request.respond(result);
    })
    .detach();
}

fn newest_terminal(request: &TerminalRequest, cx: &App) -> Option<Entity<TerminalPanel>> {
    live_windows(cx)
        .iter()
        .flat_map(|(_, workspace)| workspace.read(cx).terminals_of(&request.host_id, cx))
        .last()
}

/// 已有实例还在启动（迁移数据库、打开钥匙串）、命令行服务还没开始监听时，转交启动参数最多等这么久。
const FORWARD_WAIT: Duration = Duration::from_secs(5);
/// 请旧版本的实例退出后，等它释放单实例锁最多这么久（它退出前要做完收尾）。
const REPLACE_WAIT: Duration = Duration::from_secs(15);

/// 把启动参数交给已有实例的结果。
pub enum Forwarded {
    /// 交给了它（由它打开或激活主窗口）
    Done,
    /// 它是别的版本、没有在用（没有窗口），已请它退出并等到它释放了单实例锁：本进程照常启动
    Replaced,
    /// 交不出去：原因（给用户看）
    Failed(String),
}

/// 已有实例在运行时把启动参数交给它：相对路径的会话文件先换成绝对路径，内容先读好一起交（堡垒机在调起的进程
/// 退出后就删除它）。已有实例是别的版本（更新后旧版本还在后台运行）时先请它退出。
pub fn forward_to_running(args: &[String]) -> Forwarded {
    use termlet_core::proto::client::{CallError, Client, ConnectError};
    use termlet_core::proto::endpoint::Endpoint;
    use termlet_core::proto::{ClientHello, PROTOCOL_VERSION, Request, Response};
    let data_dir = match DataDir::resolve() {
        Ok(dir) => dir,
        Err(e) => return Forwarded::Failed(e.to_string()),
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    let args: Vec<String> = args
        .iter()
        .map(|arg| {
            let path = std::path::Path::new(arg);
            if arg.to_ascii_lowercase().ends_with(".xsh") && path.is_relative() {
                cwd.join(path).display().to_string()
            } else {
                arg.clone()
            }
        })
        .collect();
    let files = read_launch_files(&args);
    let endpoint = Endpoint::for_data_dir(data_dir.root());
    let hello = ClientHello {
        protocol: PROTOCOL_VERSION,
        client: format!("Termlet {}", termlet_core::VERSION),
        lang: Default::default(),
    };
    let deadline = Instant::now() + FORWARD_WAIT;
    let mut client = loop {
        match Client::connect(&endpoint, &hello) {
            Ok(client) => break client,
            Err(ConnectError::NotRunning | ConnectError::Io(_)) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Forwarded::Failed(e.to_string()),
        }
    };
    let running = client.welcome().app_version.clone();
    if !running.is_empty() && running != termlet_core::VERSION {
        // 回答写出前对方可能已经退出（连接被关闭）：同样等它释放锁
        let quitting = matches!(
            client.call(&Request::Quit),
            Ok(Response::Ok) | Err(CallError::Closed)
        );
        if quitting {
            return if wait_released(&data_dir) {
                Forwarded::Replaced
            } else {
                Forwarded::Failed(t!("startup.error.old_instance", version = running).to_string())
            };
        }
        // 它有窗口、有进行中的请求，或是不认得退出请求的更早版本：照常交给它
    }
    match client.call(&Request::Activate { args, files }) {
        Ok(Response::Ok) => Forwarded::Done,
        Ok(Response::Error(body)) => Forwarded::Failed(body.message),
        Ok(_) => Forwarded::Failed(t!("startup.error.unexpected_reply").to_string()),
        Err(e) => Forwarded::Failed(e.to_string()),
    }
}

/// 启动参数中的会话文件（.xsh，最多 [`termlet_core::launch::MAX_XSH_SIZE`]）的内容；读不到的不带，由已有实例再试。
fn read_launch_files(args: &[String]) -> Vec<termlet_core::proto::LaunchFile> {
    use std::io::Read as _;
    args.iter()
        .filter(|arg| arg.to_ascii_lowercase().ends_with(".xsh"))
        .filter_map(|path| {
            let file = std::fs::File::open(path).ok()?;
            let limit = termlet_core::launch::MAX_XSH_SIZE;
            let mut data = Vec::new();
            file.take(limit + 1).read_to_end(&mut data).ok()?;
            let file = termlet_core::proto::LaunchFile {
                path: path.clone(),
                data,
            };
            (file.data.len() as u64 <= limit).then_some(file)
        })
        .collect()
}

/// 等已有实例释放单实例锁（它退出了）。
fn wait_released(data_dir: &DataDir) -> bool {
    let deadline = Instant::now() + REPLACE_WAIT;
    while Instant::now() < deadline {
        match termlet_core::instance::InstanceLock::try_acquire(data_dir) {
            Ok(Some(lock)) => {
                drop(lock);
                return true;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(_) => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use termlet_core::proto::endpoint::Endpoint;

    use super::*;

    // 设置“Termlet 没有打开时也能使用”改了：写给命令行读的文件跟着变
    #[gpui_kit::test]
    fn launch_config_follows_the_setting(cx: &mut gpui_kit::TestAppContext) {
        // 应用状态在核心的运行时线程上有后台任务（存储事件、使用统计的检查等）
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        cx.update(crate::init);
        cx.update(AppState::install_in_memory);
        let endpoint = cx.update(|cx| Endpoint::for_data_dir(AppState::global(cx).data_dir.root()));
        // 测试用的数据目录还没有建（正式启动时 `DataDir::prepare` 会建好）
        std::fs::create_dir_all(endpoint.data_dir()).unwrap();
        cx.update(|cx| sync_launch_config(cx));
        assert!(endpoint.launch_config().auto_start, "默认允许");
        cx.update(|cx| {
            AppState::store(cx)
                .update_local_settings(|s| s.cli.auto_start = false)
                .unwrap()
        });
        cx.run_until_parked();
        assert!(!endpoint.launch_config().auto_start);
    }

    /// 装好界面与内存中的数据，按 `os` 模拟系统；不启动命令行服务，只登记窗口与关闭窗口后的处理（与 [`start`] 相同）。
    fn setup(cx: &mut gpui_kit::TestAppContext, os: OperatingSystem) {
        cx.executor().allow_parking();
        cx.set_platform_info(gpui_kit::PlatformInfo {
            os,
            target: gpui_kit::PlatformTarget::Native,
        });
        cx.update(gpui_kit::init);
        cx.update(crate::init);
        cx.update(AppState::install_in_memory);
        cx.update(|cx| cx.set_reduce_motion(true));
        QUIT_APPROVED.set(false);
        WINDOW_HANDLES.with(|handles| handles.borrow_mut().clear());
        cx.update(|cx| {
            cx.set_global(Bridge {
                service: None,
                audit: None,
                windows: Vec::new(),
                closing: false,
                quitting: false,
                in_dock: false,
                last_activity: Instant::now(),
                replaced: false,
            });
            cx.set_quit_mode(QuitMode::Explicit);
            cx.on_window_closed(|cx, _| on_window_closed(cx)).detach();
        });
    }

    fn in_dock(cx: &mut gpui_kit::TestAppContext) -> bool {
        cx.update(|cx| cx.global::<Bridge>().in_dock)
    }

    fn close_all_windows(cx: &mut gpui_kit::TestAppContext) {
        for handle in cx.update(|cx| cx.windows()) {
            cx.update_window(handle, |_, window, _| window.remove_window())
                .unwrap();
        }
        cx.run_until_parked();
    }

    // macOS：关闭最后一个窗口后不退出、留在程序坞（不空闲退出）；点程序坞图标重新打开主窗口；“退出 Termlet”照样退出
    #[gpui_kit::test]
    fn macos_stays_in_the_dock_after_the_last_window(cx: &mut gpui_kit::TestAppContext) {
        setup(cx, OperatingSystem::Mac);
        cx.update(open_main_window);
        close_all_windows(cx);
        assert!(in_dock(cx), "留在程序坞");
        assert!(!QUIT_APPROVED.get(), "没有退出");

        cx.update(reopen);
        assert_eq!(cx.update(|cx| live_windows(cx).len()), 1, "重新打开主窗口");
        assert!(!in_dock(cx));
        cx.update(reopen);
        assert_eq!(
            cx.update(|cx| live_windows(cx).len()),
            1,
            "已有窗口时不再开一个"
        );

        cx.update(crate::app_menu::request_quit);
        cx.run_until_parked();
        assert!(cx.update(|cx| cx.windows().is_empty()));
        assert!(QUIT_APPROVED.get(), "退出 Termlet 照样退出");
    }

    // Windows、Linux：关闭最后一个窗口后退出
    #[gpui_kit::test]
    fn other_systems_quit_with_the_last_window(cx: &mut gpui_kit::TestAppContext) {
        assert!(quits_with_last_window(OperatingSystem::Windows));
        assert!(quits_with_last_window(OperatingSystem::Linux));
        assert!(!quits_with_last_window(OperatingSystem::Mac));
        setup(cx, OperatingSystem::Windows);
        cx.update(open_main_window);
        close_all_windows(cx);
        assert!(!in_dock(cx));
        assert!(QUIT_APPROVED.get(), "随之退出");
    }

    // 没有窗口时菜单栏照样可用：新窗口、设置先打开一个主窗口（设置打开在其中）
    #[gpui_kit::test]
    fn menu_bar_works_without_windows(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        setup(cx, OperatingSystem::Mac);
        cx.update(|cx| cx.dispatch_action(&crate::workspace::NewWindow));
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| live_windows(cx).len()), 1, "新窗口");
        // 有主窗口时由工作区处理，只开一个
        let first = cx.update(|cx| live_windows(cx)[0].0);
        cx.update_window(first, |_, window, cx| {
            window.render_frame(cx);
            window.dispatch_action(Box::new(crate::workspace::NewWindow), cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| live_windows(cx).len()), 2);
        close_all_windows(cx);
        assert!(in_dock(cx));
        cx.update(|cx| cx.dispatch_action(&crate::workspace::OpenSettings));
        cx.run_until_parked();
        let windows = cx.update(|cx| live_windows(cx));
        assert_eq!(windows.len(), 1);
        cx.update_window(windows[0].0, |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window.try_find("settings-cancel").is_some(),
                "设置打开在新窗口中"
            );
        })
        .unwrap();
    }

    // 批准框的标题按来源写：MCP 写客户端名（没有名字时写 AI 工具），命令行照旧
    #[test]
    fn approval_title_follows_the_source() {
        let title = |caller: &str| approval_title("web", caller).to_string();
        crate::i18n::set_language(crate::i18n::Language::ZhCn);
        assert_eq!(
            title("claude.exe < pwsh.exe"),
            "命令行工具要在“web”上执行危险命令"
        );
        assert_eq!(title(""), "命令行工具要在“web”上执行危险命令");
        assert_eq!(
            title("mcp:claude-code < claude.exe"),
            "claude-code（MCP）要在“web”上执行危险命令"
        );
        assert_eq!(
            title("mcp < Cursor.exe"),
            "AI 工具（MCP）要在“web”上执行危险命令"
        );
        crate::i18n::set_language(crate::i18n::Language::En);
        assert_eq!(
            title("mcp:cursor"),
            "cursor (MCP) wants to run a dangerous command on “web”"
        );
        crate::i18n::set_language(crate::i18n::Language::ZhCn);
    }

    // 剩余秒数向上取整：刚打开时是完整的秒数、最后不到一秒时是 1；正好在整秒上时等一整秒再变
    #[test]
    fn countdown_rounds_up_and_ticks_on_whole_seconds() {
        let now = Instant::now();
        let at = |ms: u64| now + Duration::from_millis(ms);
        assert_eq!(seconds_left(at(60_000), now), 60);
        assert_eq!(seconds_left(at(59_400), now), 60);
        assert_eq!(seconds_left(at(59_000), now), 59);
        assert_eq!(seconds_left(at(300), now), 1);
        assert_eq!(seconds_left(now, now), 0);
        assert_eq!(seconds_left(now, at(500)), 0, "已过期");
        assert_eq!(
            until_next_second(at(59_400), now),
            Some(Duration::from_millis(400))
        );
        assert_eq!(
            until_next_second(at(59_000), now),
            Some(Duration::from_secs(1))
        );
        assert_eq!(until_next_second(now, now), None);
        assert_eq!(until_next_second(now, at(1)), None);
    }

    // 批准危险命令的对话框倒计时：剩余的秒数随时间减少（不是打开时算好的静态文字）；拒绝后命令行得到 command_denied
    #[gpui_kit::test]
    fn approval_dialog_counts_down(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        use termlet_core::model::{CliAccess, Host};
        use termlet_core::proto::client::Client;
        use termlet_core::proto::{ClientHello, ExecRequest, Lang, Request};
        use termlet_core::store::Position;

        setup(cx, OperatingSystem::Windows);
        crate::i18n::set_language(crate::i18n::Language::ZhCn);
        cx.update(open_main_window);
        let handle = cx.update(|cx| live_windows(cx)[0].0);
        let store = cx.update(|cx| AppState::store(cx));
        store
            .update_local_settings(|s| s.cli.enabled = true)
            .unwrap();
        store
            .create(
                Host {
                    name: "web".into(),
                    address: "127.0.0.1".into(),
                    username: "u".into(),
                    cli: CliAccess {
                        allowed: true,
                        credential: None,
                    },
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
        let dir = std::env::temp_dir().join(format!(
            "termlet-approval-{}",
            termlet_core::id::random_token(8)
        ));
        let data_dir = DataDir::at(dir.join("data"));
        data_dir.prepare().unwrap();
        let audit = Arc::new(AuditLog::new(store.clone()).unwrap());
        let mut config = CliConfig::new(data_dir, store, audit);
        config.approval_timeout = Duration::from_secs(30);
        let (service, events) = CliService::start(config).unwrap();
        let endpoint = service.endpoint().clone();
        let client = std::thread::spawn(move || {
            Client::connect(
                &endpoint,
                &ClientHello {
                    protocol: termlet_core::proto::PROTOCOL_VERSION,
                    client: "test".into(),
                    lang: Lang::Zh,
                },
            )
            .unwrap()
            .call(&Request::Exec(ExecRequest {
                host: "web".into(),
                command: "reboot".into(),
                terminal: true,
                caller: Vec::new(),
            }))
            .unwrap()
        });
        let Ok(CliEvent::Approval(request)) = events.recv_blocking() else {
            panic!("应收到批准请求");
        };
        cx.update(|cx| ask_approval(request, cx));
        cx.run_until_parked();
        let seconds = |cx: &mut gpui_kit::TestAppContext| -> u64 {
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                let text = window
                    .find("approval-countdown")
                    .label()
                    .unwrap()
                    .to_string();
                let number: String = text.chars().take_while(char::is_ascii_digit).collect();
                assert!(text.ends_with("秒后仍没有回应时拦截。"), "{text}");
                number.parse().unwrap()
            })
            .unwrap()
        };
        let first = seconds(cx);
        assert!((25..=30).contains(&first), "{first}");
        // 真实的时间过去一秒多，测试的时钟也拨过去（刷新窗口的定时器按它走）
        std::thread::sleep(Duration::from_millis(1100));
        cx.executor().advance_clock(Duration::from_millis(1100));
        cx.run_until_parked();
        let later = seconds(cx);
        assert!(later < first, "倒计时：{first} → {later}");

        cx.update_window(handle, |_, window, cx| window.click("cancel", cx))
            .unwrap();
        cx.run_until_parked();
        match client.join().unwrap() {
            termlet_core::proto::Response::Error(body) => assert_eq!(body.code, "command_denied"),
            other => panic!("应被拒绝：{other:?}"),
        }
        drop(service);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 系统请求退出（程序坞的“退出”、关机、注销）：有运行中的本地终端时先不退出、弹出“退出 Termlet”的确认；
    // Termlet 自己发起的退出、没有要说明的直接同意
    #[gpui_kit::test]
    fn system_quit_asks_while_sessions_are_open(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        setup(cx, OperatingSystem::Mac);
        let mut async_cx = cx.update(|cx| cx.to_async());
        assert!(terminate_requested_async(&mut async_cx), "没有窗口");
        cx.update(open_main_window);
        let (handle, workspace) = cx.update(|cx| live_windows(cx)[0].clone());
        assert!(terminate_requested_async(&mut async_cx), "没有会话");
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| w.open_local_terminal(window, cx))
        })
        .unwrap();
        crate::testkit::poll_until(cx, "本地终端启动", |cx| {
            cx.update(|cx| {
                let terminals = workspace.read(cx).live_terminals();
                !terminals.is_empty() && terminals.iter().all(|t| t.read(cx).is_live(cx))
            })
        });
        assert!(!terminate_requested_async(&mut async_cx), "先确认");
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                window.find("close-confirm").label(),
                Some(crate::t!("close.quit_confirm").as_ref()),
                "退出 Termlet 的确认"
            );
            window.click("close-cancel", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| live_windows(cx).len()), 1, "取消后窗口还在");
        QUIT_APPROVED.set(true);
        assert!(terminate_requested_async(&mut async_cx), "已经决定退出");
    }
}
