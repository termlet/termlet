//! Termlet 桌面端：GPUI 窗口外壳与各功能的界面。
//!
//! 生产代码与 UI 集成测试都经由这个库，`main.rs` 只调用 [`run`]。

rust_i18n::i18n!("locales", fallback = "zh-CN");

mod app_menu;
mod app_state;
mod appearance;
mod args;
mod assets;
mod backup_ui;
mod batch_results;
mod boot;
mod brand;
mod brand_icons;
mod choice;
mod cli_bridge;
mod cli_ui;
mod close_confirm;
mod command_palette;
mod compose;
mod dialog_token;
mod dock_skin;
mod editor;
mod forward_dialog;
mod forwards;
mod group_dialog;
mod host_dialog;
mod host_info;
mod host_tools;
pub mod i18n;
mod install_kind;
mod key_dialogs;
mod key_fields;
mod keymap;
mod launch;
mod local_shell;
mod lock_ui;
mod macos;
mod menu_tooltip;
mod monitor_bar;
mod monitor_model;
mod name_dialog;
mod notify;
mod output_tap;
mod page_nav;
mod paste_guard;
mod profile_dialog;
mod prompt_dialog;
mod prose;
mod proxy_share;
mod radio;
mod raw_errors;
mod replay;
mod schemes;
mod settings;
mod sftp;
mod sidebar;
mod snippets;
mod ssh_errors;
mod startup_error;
mod startup_open;
mod store_errors;
mod sync_ui;
mod tab_fit;
mod taskbar;
mod terminal_colors;
mod terminal_panel;
mod terminal_search;
mod terminal_transfer;
#[cfg(test)]
mod testkit;
mod toolbar;
mod topmost;
mod tray;
mod ui_scale;
mod ui_state;
mod usage_ui;
mod vault_gate;
mod website;
mod window_state;
mod workspace;
mod workspaces;
mod wsl_errors;

use gpui_kit::component::TitleBar;
use gpui_kit::{WindowOptions, px, size};

pub use app_state::{AppState, StoreModel};
pub use workspace::Workspace;

/// 启动应用并打开主窗口（启动参数见 `args`：`--headless`、`--log-level`、`--verbose`）。
pub fn run() {
    let start = args::parse(std::env::args().skip(1));
    let headless = start.headless;
    let log_override = termlet_core::logging::startup_override(start.log_level.as_deref());
    let args = start.rest;
    let mut booted = boot::start(log_override.clone());
    if let Err(boot::BootError::AlreadyRunning) = booted {
        // 被 termlet 拉起时已有实例：命令行工具会连上它
        if headless {
            return;
        }
        // 已有实例在运行：把启动参数交给它，由它打开或激活主窗口；它是没在用的旧版本时请它退出，本进程接着启动
        i18n::set_language(i18n::Language::system());
        match cli_bridge::forward_to_running(&args) {
            cli_bridge::Forwarded::Done => return,
            cli_bridge::Forwarded::Replaced => booted = boot::start(log_override.clone()),
            cli_bridge::Forwarded::Failed(reason) => {
                show_startup_message(t!("startup.error.forward_failed", reason = reason));
                return;
            }
        }
        // 又被别的进程抢先拿到了锁：交给它
        if let Err(boot::BootError::AlreadyRunning) = booted {
            if let cli_bridge::Forwarded::Failed(reason) = cli_bridge::forward_to_running(&args) {
                show_startup_message(t!("startup.error.forward_failed", reason = reason));
            }
            return;
        }
    }
    // 日志活到退出前的收尾做完之后：最后几行日志（空闲退出、启动了安装程序……）也写进文件
    let log = booted.as_mut().ok().and_then(|b| b.log.take());
    // macOS：系统请求退出（程序坞的“退出”、关机、注销）时先问 Termlet，要在 GPUI 建好应用委托之前装上
    macos::install_terminate_hook();
    let app = gpui_kit::application().with_assets(assets::AppAssets);
    // macOS：没有可见的窗口时点程序坞图标，重新打开主窗口（关闭最后一个窗口后留在程序坞）
    app.on_reopen(cli_bridge::reopen);
    app.run(move |cx| {
        gpui_kit::init(cx);
        init(cx);
        match booted {
            Ok(booted) => {
                lock_ui::install_crash_hook(&booted.data_dir.logs_dir());
                let settings = booted.store.settings();
                appearance::apply(&settings.appearance, None, cx);
                // 单实例锁随进程存活
                std::mem::forget(booted.instance);
                AppState::install(booted.data_dir, booted.store, cx);
                // 先打开应用锁：命令行服务从开始监听起就按锁定状态处理
                lock_ui::install(cx);
                usage_ui::install(cx);
                cli_bridge::start(cx);
                cli_bridge::install_hooks(cx);
                // 在打开主窗口之前：主窗口一打开就按设置显示托盘图标
                tray::install(cx);
                // 传输进度显示在任务栏按钮、程序坞图标上
                taskbar::install(cx);
                // 启动时打开的主机或本地终端：本次运行的第一个主窗口解锁后打开（被命令行工具在后台启动的，
                // 用户第一次打开窗口时）
                startup_open::arm(cx);
                if !headless {
                    launch::queue_from_args(&args, cx);
                    cli_bridge::open_main_window(cx);
                }
            }
            Err(boot::BootError::Fatal(error)) => {
                appearance::set_language(i18n::Language::system(), cx);
                // 说明窗口关掉后退出（macOS 上 GPUI 默认不随最后一个窗口退出）
                cx.set_quit_mode(gpui_kit::QuitMode::LastWindowClosed);
                startup_error::open(error, cx);
            }
            Err(boot::BootError::PortableNotWritable { dir, reason }) => {
                appearance::set_language(i18n::Language::system(), cx);
                cx.set_quit_mode(gpui_kit::QuitMode::LastWindowClosed);
                startup_error::open_message(
                    t!(
                        "startup.error.portable_not_writable",
                        dir = dir.display().to_string(),
                        reason = reason
                    ),
                    cx,
                );
            }
            Err(boot::BootError::AlreadyRunning) => unreachable!(),
        }
        cx.activate(true);
    });
    // 窗口都已关闭：等关闭 Tab、窗口时的收尾（结束 tmux 会话等）做完
    cli_bridge::wait_cleanup();
    tracing::info!("Termlet 退出");
    drop(log);
}

/// 没有拿到单实例锁时显示一个说明窗口（发布构建没有控制台，用户看不到 stderr）。
fn show_startup_message(message: gpui_kit::SharedString) {
    gpui_kit::application()
        .with_assets(assets::AppAssets)
        // 说明窗口关掉后退出（macOS 上 GPUI 默认不随最后一个窗口退出）
        .with_quit_mode(gpui_kit::QuitMode::LastWindowClosed)
        .run(move |cx| {
            gpui_kit::init(cx);
            init(cx);
            startup_error::open_message(message, cx);
            cx.activate(true);
        });
}

/// WSL 终端、文件管理的 Tab 名称：“发行版 (WSL)”，以 root 登录时为“发行版 (WSL, root)”。
pub(crate) fn wsl_name(distro: &str, user: &str) -> gpui_kit::SharedString {
    if user.is_empty() {
        t!("wsl.title", distro = distro)
    } else {
        t!("wsl.title_with_user", distro = distro, user = user)
    }
}

/// 地址是不是 IP（IPv6 可以带方括号与 `%网卡`）。
fn is_ip_address(address: &str) -> bool {
    let bare = address.trim_start_matches('[').trim_end_matches(']');
    let bare = bare.split('%').next().unwrap_or_default();
    bare.parse::<std::net::IpAddr>().is_ok()
}

/// 菜单中复制主机地址的一项（服务器列表、终端与 SFTP 的 Tab）：地址是 IP 时为“复制 IP”，是域名时为“复制主机名”；
/// 复制后提示。
pub(crate) fn copy_address_item(address: String) -> gpui_kit::component::menu::PopupMenuItem {
    use gpui_kit::component::WindowExt as _;
    let label = if is_ip_address(&address) {
        t!("hosts.menu.copy_ip")
    } else {
        t!("hosts.menu.copy_hostname")
    };
    gpui_kit::component::menu::PopupMenuItem::new(label).on_click(move |_, window, cx| {
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(address.clone()));
        window.push_notification(
            gpui_kit::component::notification::Notification::success(t!(
                "hosts.menu.address_copied",
                address = address.clone()
            )),
            cx,
        );
    })
}

/// 注册 Termlet 自己的字体、快捷键与 macOS 的菜单栏，调整主题，给 gpui-kit 补上日文、韩文文案（`gpui_kit::init`
/// 之后调用；UI 测试也要调用）。
pub fn init(cx: &mut gpui_kit::App) {
    i18n::extend_gpui_kit();
    menu_tooltip::init(cx);
    appearance::adjust_light_theme(cx);
    termlet_terminal::init(cx);
    local_shell::init(cx);
    terminal_search::init(cx);
    terminal_panel::init(cx);
    sidebar::host_tree::init(cx);
    sftp::init(cx);
    host_tools::init(cx);
    editor::init(cx);
    replay::init(cx);
    workspace::init(cx);
    // 在各模块的快捷键之后：macOS 菜单上显示的快捷键取自键位表
    app_menu::init(cx);
}

/// 应用的 ID，与 macOS 的 Bundle ID、Linux 的菜单项文件名（`dev.termlet.Termlet.desktop`）相同。设为窗口的
/// app_id：Linux 的桌面环境据此把窗口与菜单项对应起来（Wayland 的 xdg-toplevel app_id、X11 的 WM_CLASS），
/// 任务栏、切换窗口时显示菜单项中的名称与图标。Windows、macOS 上 GPUI 不用它。
pub(crate) const APP_ID: &str = "dev.termlet.Termlet";

/// 主窗口的选项：位置与大小按上次记下的恢复（没有记录时按默认大小放在主屏中间，见 [`window_state`]）。
pub(crate) fn main_window_options(cx: &gpui_kit::App) -> WindowOptions {
    // 窗口的大小与最小尺寸是物理边界，不随界面缩放
    let (window_bounds, display_id) = window_state::initial_bounds(cx);
    let (min_width, min_height) = window_state::MIN_SIZE;
    WindowOptions {
        window_bounds: Some(window_bounds),
        display_id,
        window_min_size: Some(size(px(min_width), px(min_height))),
        app_id: Some(APP_ID.into()),
        ..TitleBar::window_options()
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::TestAppContext;

    use super::{APP_ID, is_ip_address, main_window_options};

    // 应用的 ID 与打包用的文件一致：macOS 的 Bundle ID，Linux 菜单项的文件名、图标名与窗口类名
    #[test]
    fn app_id_matches_the_packaging() {
        let plist = include_str!("../../../packaging/macos/Info.plist");
        assert!(plist.contains(&format!(
            "<key>CFBundleIdentifier</key>\n\t<string>{APP_ID}</string>"
        )));
        let desktop = include_str!("../../../packaging/linux/dev.termlet.Termlet.desktop");
        assert!(desktop.contains(&format!("\nIcon={APP_ID}\n")));
        assert!(desktop.contains(&format!("\nStartupWMClass={APP_ID}\n")));
    }

    #[gpui_kit::test]
    fn main_window_sets_the_app_id(cx: &mut TestAppContext) {
        cx.update(|cx| {
            assert_eq!(main_window_options(cx).app_id.as_deref(), Some(APP_ID));
        });
    }

    #[test]
    fn ip_or_host_name() {
        for ip in ["10.0.0.1", "::1", "[fe80::1]", "fe80::1%eth0"] {
            assert!(is_ip_address(ip), "{ip}");
        }
        for name in ["web.example.com", "localhost", "", "10.0.0"] {
            assert!(!is_ip_address(name), "{name}");
        }
    }
}
