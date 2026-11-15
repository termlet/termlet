//! 关闭窗口时最小化到托盘（只有 Windows）：设置“通用”中的开关，本机设置 `window.close_to_tray`，默认关。
//!
//! - 开关开着、有主窗口时通知区域中有 Termlet 的图标（无窗口模式没有：那时 Termlet 只是为命令行工具在后台运行）。
//!   单击图标或菜单的“显示 Termlet”恢复窗口（锁定时窗口中是锁屏，先解锁）；菜单的“退出 Termlet”与退出相同
//!   （[`crate::app_menu::request_quit`]：未断开的会话、未完成的传输与未保存的文件汇总确认一次），锁定时改为显示
//!   窗口与锁屏上的退出确认（锁定时不显示会话的情况）。
//! - 点窗口的关闭按钮：最后一个显示着的主窗口隐藏到托盘而不关闭，连接、传输、端口转发照常运行；还有别的显示着的
//!   主窗口时照常关闭（与开关关掉时相同，Ctrl+Shift+N 开的窗口也一样）。隐藏前记下窗口的位置，退出时照常写回；
//!   主机概览的小窗一起关闭。
//! - 别处要把窗口拿到用户面前时（另一个 Termlet 进程交来启动参数、命令行工具的危险命令要批准、点系统通知、退出前
//!   确认）用 [`show_window`]：隐藏着的先显示出来。
//! - 开关关掉时显示隐藏着的窗口、去掉图标；退出时去掉图标。
//!
//! GPUI 没有托盘的接口：Windows 上用 `Shell_NotifyIconW` 与一个不显示的窗口接收图标的消息（见 `win32`），隐藏、
//! 显示窗口用 `ShowWindowAsync`。图标与窗口的操作经 [`TrayHost`]，测试中换成记录调用的假实现。

#[cfg(windows)]
mod win32;

#[cfg(windows)]
pub(crate) use win32::window_hwnd;

use gpui_kit::{AnyWindowHandle, App, Global, OperatingSystem, Window};
use termlet_core::store::StoreEvent;

use crate::app_state::AppState;
use crate::workspace::Workspace;

/// 系统是否支持关闭窗口时最小化到托盘（设置中只在这些系统上显示这一项）。macOS 关闭最后一个窗口后本来就留在
/// 程序坞；Linux 的托盘各桌面环境不一，不做。
pub(crate) fn supported(os: OperatingSystem) -> bool {
    os == OperatingSystem::Windows
}

/// 托盘图标交来的命令（只有 Windows 的图标发出，其他系统上的构建中没有人用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) enum TrayCommand {
    /// 单击图标、菜单的“显示 Termlet”
    Show,
    /// 菜单的“退出 Termlet”
    Quit,
}

/// 通知区域的图标与窗口的隐藏、显示（Windows 上为 `win32::Win32Tray`）。
pub(crate) trait TrayHost {
    /// 在通知区域加上图标，返回是否成功。
    fn show_icon(&mut self) -> bool;
    fn remove_icon(&mut self);
    fn hide_window(&mut self, window: &Window);
    fn show_window(&mut self, window: &Window);
}

/// 托盘的状态（全局量，[`install`] 建立）。
struct Tray {
    host: Box<dyn TrayHost>,
    /// 图标在通知区域中
    icon: bool,
    /// 打开过的主窗口（已关闭的在用到时去掉）
    windows: Vec<AnyWindowHandle>,
    /// 隐藏到托盘的主窗口
    hidden: Vec<AnyWindowHandle>,
}

impl Global for Tray {}

/// 装上托盘（应用状态、命令行服务之后，打开主窗口之前调用）；不支持的系统上什么也不做。
pub(crate) fn install(cx: &mut App) {
    if !supported(cx.platform_info().os) {
        return;
    }
    let (commands, received) = async_channel::unbounded();
    match native_host(commands) {
        Some(host) => install_with(host, received, cx),
        None => tracing::warn!("建立托盘图标的窗口失败，关闭窗口时照常退出"),
    }
}

/// 这个系统的托盘实现。
#[cfg(windows)]
fn native_host(commands: async_channel::Sender<TrayCommand>) -> Option<Box<dyn TrayHost>> {
    win32::Win32Tray::new(commands).map(|host| Box::new(host) as Box<dyn TrayHost>)
}

#[cfg(not(windows))]
fn native_host(_: async_channel::Sender<TrayCommand>) -> Option<Box<dyn TrayHost>> {
    None
}

/// 用给定的实现装上托盘：处理图标交来的命令，开关、主窗口变化时同步图标。
pub(crate) fn install_with(
    host: Box<dyn TrayHost>,
    commands: async_channel::Receiver<TrayCommand>,
    cx: &mut App,
) {
    // 已经开着的主窗口（测试中可能先开窗口）
    let windows = cx
        .windows()
        .into_iter()
        .filter(|handle| {
            handle
                .update(cx, |_, window, cx| {
                    Workspace::in_window(window, cx).is_some()
                })
                .unwrap_or(false)
        })
        .collect();
    cx.set_global(Tray {
        host,
        icon: false,
        windows,
        hidden: Vec::new(),
    });
    cx.spawn(async move |cx| {
        while let Ok(command) = commands.recv().await {
            cx.update(|cx| handle(command, cx));
        }
    })
    .detach();
    cx.observe_new::<Workspace>(|_, window, cx| {
        if let Some(window) = window {
            let handle = window.window_handle();
            cx.global_mut::<Tray>().windows.push(handle);
        }
        cx.defer(sync);
    })
    .detach();
    cx.on_window_closed(|cx, _| sync(cx)).detach();
    let store = AppState::global(cx).store.clone();
    cx.subscribe(&store, |_, event: &StoreEvent, cx| {
        if matches!(event, StoreEvent::LocalSettingsChanged) {
            sync(cx);
        }
    })
    .detach();
    cx.on_app_quit(|cx| {
        if cx.has_global::<Tray>() {
            let tray = cx.global_mut::<Tray>();
            if std::mem::take(&mut tray.icon) {
                tray.host.remove_icon();
            }
        }
        async {}
    })
    .detach();
    sync(cx);
}

/// 开关开着（并且系统支持）。
fn enabled(cx: &App) -> bool {
    supported(cx.platform_info().os)
        && cx
            .try_global::<AppState>()
            .is_some_and(|_| AppState::store(cx).local_settings().window.close_to_tray)
}

/// 去掉已关闭的窗口。
fn prune(cx: &mut App) {
    let open = cx.windows();
    let tray = cx.global_mut::<Tray>();
    tray.windows.retain(|handle| open.contains(handle));
    tray.hidden.retain(|handle| open.contains(handle));
}

/// 按开关与主窗口同步：开着且有主窗口时显示图标；关掉时先把隐藏着的窗口显示出来，再去掉图标。
fn sync(cx: &mut App) {
    if !cx.has_global::<Tray>() {
        return;
    }
    prune(cx);
    let enabled = enabled(cx);
    if !enabled {
        let hidden = std::mem::take(&mut cx.global_mut::<Tray>().hidden);
        for handle in hidden {
            tracing::info!("关掉了“关闭窗口时最小化到托盘”，显示隐藏着的窗口");
            let _ = handle.update(cx, |_, window, cx| {
                cx.global_mut::<Tray>().host.show_window(window);
            });
        }
    }
    let tray = cx.global_mut::<Tray>();
    let want = enabled && !tray.windows.is_empty();
    if want && !tray.icon {
        tray.icon = tray.host.show_icon();
        if !tray.icon {
            tracing::warn!("在通知区域加上图标失败");
        }
    } else if !want && tray.icon {
        tray.host.remove_icon();
        tray.icon = false;
    }
}

/// 点了窗口的关闭按钮（`Workspace::should_close`）：开关开着、这是最后一个显示着的主窗口时隐藏到托盘，返回 true
/// （不关闭）。还有别的显示着的主窗口、图标加不上（窗口藏起来就找不回来了）时返回 false，照常关闭。
pub(crate) fn close_to_tray(window: &mut Window, cx: &mut App) -> bool {
    if !cx.has_global::<Tray>() || !enabled(cx) {
        return false;
    }
    prune(cx);
    let this = window.window_handle();
    let tray = cx.global_mut::<Tray>();
    let others = tray
        .windows
        .iter()
        .any(|handle| *handle != this && !tray.hidden.contains(handle));
    if others {
        return false;
    }
    if !tray.icon {
        tray.icon = tray.host.show_icon();
        if !tray.icon {
            tracing::warn!("在通知区域加上图标失败，照常关闭窗口");
            return false;
        }
    }
    crate::window_state::remember(window, cx);
    let tray = cx.global_mut::<Tray>();
    tray.host.hide_window(window);
    if !tray.hidden.contains(&this) {
        tray.hidden.push(this);
    }
    // 主机概览的小窗与主窗口一起关闭（等这次关闭按钮的处理结束）
    cx.defer(crate::host_tools::close_overview_windows);
    tracing::info!("关闭窗口：隐藏到托盘，会话照常运行");
    true
}

/// 把窗口拿到用户面前：隐藏在托盘中的先显示出来，再激活。要窗口出现在前面的地方都用它代替 `activate_window`。
pub(crate) fn show_window(window: &mut Window, cx: &mut App) {
    let this = window.window_handle();
    if cx.has_global::<Tray>() {
        let tray = cx.global_mut::<Tray>();
        if let Some(ix) = tray.hidden.iter().position(|handle| *handle == this) {
            tray.hidden.remove(ix);
            tray.host.show_window(window);
            tracing::info!("从托盘恢复窗口");
        }
    }
    window.activate_window();
}

/// 窗口是否隐藏在托盘中。
#[cfg(test)]
fn is_hidden(handle: AnyWindowHandle, cx: &App) -> bool {
    cx.try_global::<Tray>()
        .is_some_and(|tray| tray.hidden.contains(&handle))
}

fn handle(command: TrayCommand, cx: &mut App) {
    tracing::info!(?command, "托盘图标的命令");
    match command {
        TrayCommand::Show => restore(cx),
        TrayCommand::Quit => quit(cx),
    }
}

/// 单击图标、菜单的“显示 Termlet”：显示隐藏着的窗口，激活最近的主窗口；一个主窗口也没有时（图标还没来得及去掉）
/// 打开一个。
fn restore(cx: &mut App) {
    prune(cx);
    let tray = cx.global::<Tray>();
    // 隐藏着的先显示，最近打开的最后激活（在最前面）
    let mut targets: Vec<AnyWindowHandle> = tray.hidden.clone();
    if let Some(last) = tray.windows.last()
        && !targets.contains(last)
    {
        targets.push(*last);
    }
    if targets.is_empty() {
        crate::cli_bridge::open_main_window(cx);
        return;
    }
    for handle in targets {
        let _ = handle.update(cx, |_, window, cx| show_window(window, cx));
    }
}

/// 菜单的“退出 Termlet”：与退出相同，要确认的在窗口中确认（隐藏着的先显示出来）。锁定时显示窗口与锁屏上的退出确认：
/// 锁屏挡在对话框之上，也不该显示会话的情况。
fn quit(cx: &mut App) {
    prune(cx);
    if crate::lock_ui::is_locked(cx) {
        let target = cx.global::<Tray>().windows.last().copied();
        if let Some(handle) = target {
            let _ = handle.update(cx, |_, window, cx| {
                show_window(window, cx);
                if let Some(workspace) = Workspace::in_window(window, cx) {
                    workspace.update(cx, |workspace, cx| workspace.ask_quit_on_lock_screen(cx));
                }
            });
            return;
        }
    }
    crate::app_menu::request_quit(cx);
}

#[cfg(test)]
mod tests;
