//! macOS 的应用菜单栏：照 macOS 与 Terminal.app、iTerm2 的习惯分为 Termlet、Shell、编辑、显示、窗口、帮助。
//!
//! 菜单项显示的快捷键由 GPUI 在设置菜单时从键位表中取（这个动作最早的一条在工作区上下文中生效的绑定，没有就取
//! 最早的一条），所以菜单项只用已有的动作，改了快捷键、切换界面语言后重新设置（[`refresh`]）。菜单项可不可用由
//! GPUI 按焦点所在的位置判断（这个动作有没有处理函数）：工作区、终端的动作在对应的位置才可用；退出、隐藏、最小化
//! 等应用级的动作是全局的处理函数，随时可用。
//!
//! 编辑菜单的剪切、复制、粘贴、全选用系统的 `OsAction`（系统的文件对话框里的输入框也就能用这些快捷键），动作是
//! gpui-kit 输入框的编辑动作；终端、SFTP 文件列表把它们当作自己的复制、粘贴、全选（粘贴照样经过终端的确认）。
//! “退出 Termlet”与关闭全部窗口相同：有连接、传输、未保存的文件时先一并确认，窗口关完后按原来的流程退出（等进行
//! 中的命令行请求做完、写完会话日志、安装已下载的更新）。系统请求退出（从程序坞退出、关机、注销）时要确认的也走这里
//! （[`crate::cli_bridge::terminate_requested`]）。
//!
//! 关闭最后一个窗口后 Termlet 留在程序坞，菜单栏照样可用：新窗口、新建本地终端、设置、关于先打开一个主窗口。窗口菜单
//! 在简体、繁體中文界面下由 [`crate::macos`] 设为系统的窗口菜单（GPUI 只认名为 “Window” 的菜单）。
//!
//! 只在 macOS 上设置菜单，其他系统不变；菜单的构建（[`app_menus`]）与快捷键（[`key_bindings`]）在 Windows 上也能测。

use gpui_kit::component::input as text_input;
use gpui_kit::{
    AnyWindowHandle, App, Entity, KeyBinding, Menu, MenuItem, OperatingSystem, OsAction,
    SystemMenuType, Window, actions,
};
use termlet_terminal::view::{Clear, ZoomIn, ZoomOut, ZoomReset};

use crate::close_confirm::{self, CloseSummary};
use crate::settings::Page;
use crate::t;
use crate::terminal_panel::{CloseTab, DuplicateSession, Reconnect};
use crate::terminal_search::OpenSearch;
use crate::ui_scale::{ResetUiZoom, ZoomInUi, ZoomOutUi};
use crate::workspace::{
    NewLocalTerminal, NewWindow, NextTab, OpenAbout, OpenCommandPalette, OpenCommandPicker,
    OpenHostToolsOverview, OpenSettings, PrevTab, SplitDown, SplitRight, ToggleCompose,
    ToggleFullscreen, ToggleHostTools, ToggleMultiExec, ToggleSidebar, Workspace,
};

actions!(
    app_menu,
    [
        /// 退出 Termlet：与关闭全部窗口相同（有连接、传输、未保存的文件时先确认）
        Quit,
        /// 隐藏 Termlet
        Hide,
        /// 隐藏其他应用
        HideOthers,
        /// 显示全部应用
        ShowAll,
        /// 最小化当前窗口
        Minimize,
        /// 缩放当前窗口
        Zoom,
        /// 关闭当前窗口（与点窗口的关闭按钮相同）
        CloseWindow,
        /// 把全部窗口移到最前
        BringAllToFront,
        /// 打开官网
        OpenWebsite,
        /// 打开隐私说明
        OpenPrivacy,
        /// 发送反馈（邮件）
        SendFeedback,
    ]
);

/// 注册菜单的动作与快捷键，设置菜单栏；界面语言变化后重新设置。
pub fn init(cx: &mut App) {
    cx.bind_keys(key_bindings(cx.platform_info().os));
    // 动作在窗口处理按键、菜单命令的过程中发出，这时那个窗口正借出去：要操作窗口的推迟到这之后
    cx.on_action(|_: &Quit, cx| cx.defer(request_quit));
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    cx.on_action(|_: &Minimize, cx| {
        cx.defer(|cx| with_active_window(cx, |window, _| window.minimize_window()))
    });
    cx.on_action(|_: &Zoom, cx| {
        cx.defer(|cx| with_active_window(cx, |window, _| window.zoom_window()))
    });
    cx.on_action(|_: &CloseWindow, cx| cx.defer(close_window));
    cx.on_action(|_: &BringAllToFront, cx| cx.defer(bring_all_to_front));
    cx.on_action(|_: &OpenWebsite, cx| cx.open_url(crate::website::home()));
    cx.on_action(|_: &OpenPrivacy, cx| cx.open_url(crate::website::privacy()));
    cx.on_action(|_: &SendFeedback, cx| cx.open_url(crate::website::FEEDBACK_MAIL));
    // 没有窗口时（macOS 关闭最后一个窗口后留在程序坞）菜单中的这几项先打开一个主窗口；有主窗口时由工作区处理，
    // 到不了这里
    cx.on_action(|_: &NewWindow, cx| cx.defer(|cx| open_window_then(cx, |_, _| {})));
    cx.on_action(|_: &NewLocalTerminal, cx| {
        cx.defer(|cx| {
            open_window_then(cx, |window, cx| {
                if let Some(workspace) = Workspace::in_window(window, cx) {
                    workspace.update(cx, |w, cx| w.open_local_terminal(window, cx));
                }
            })
        })
    });
    cx.on_action(|_: &OpenSettings, cx| {
        cx.defer(|cx| open_window_then(cx, |window, cx| open_settings(Page::General, window, cx)))
    });
    cx.on_action(|_: &OpenAbout, cx| {
        cx.defer(|cx| open_window_then(cx, |window, cx| open_settings(Page::About, window, cx)))
    });
    cx.observe_global::<crate::appearance::AppliedLanguage>(refresh)
        .detach();
    refresh(cx);
}

/// 设置 macOS 的菜单栏（其他系统上什么也不做）。菜单上显示的快捷键是设置时的键位表：改了快捷键（[`crate::keymap::apply`]）、
/// 切换界面语言后都要重新设置。
pub fn refresh(cx: &mut App) {
    if cx.platform_info().os == OperatingSystem::Mac {
        let menus = app_menus();
        let windows_menu = windows_menu_index(&menus);
        cx.set_menus(menus);
        if let Some(index) = windows_menu {
            crate::macos::set_windows_menu(index);
        }
    }
}

/// 窗口菜单在菜单栏中的位置（从应用菜单数起，与 GPUI 建的系统菜单栏一一对应）。
fn windows_menu_index(menus: &[Menu]) -> Option<usize> {
    let title = t!("app_menu.window");
    menus.iter().position(|menu| menu.name == title)
}

/// 应用级动作的快捷键（只有 macOS 上有：⌘Q、⌘H、⌥⌘H、⌘M、⇧⌘W），不带上下文，焦点在哪里都能用。
pub(crate) fn key_bindings(os: OperatingSystem) -> Vec<KeyBinding> {
    if os != OperatingSystem::Mac {
        return Vec::new();
    }
    vec![
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-h", Hide, None),
        KeyBinding::new("alt-cmd-h", HideOthers, None),
        KeyBinding::new("cmd-m", Minimize, None),
        KeyBinding::new("cmd-shift-w", CloseWindow, None),
    ]
}

/// 菜单栏（按当前的界面语言）。第一个菜单的标题 macOS 总是显示为应用的名称。窗口菜单设为系统的窗口菜单（系统在其中
/// 列出打开的窗口）：英文界面的“Window”由 GPUI 设置，其他语言的由 [`refresh`] 设置。
pub fn app_menus() -> Vec<Menu> {
    vec![
        Menu::new(t!("app.name")).items([
            MenuItem::action(t!("app_menu.about"), OpenAbout),
            MenuItem::separator(),
            MenuItem::action(t!("app_menu.settings"), OpenSettings),
            MenuItem::separator(),
            MenuItem::os_submenu(t!("app_menu.services"), SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action(t!("app_menu.hide"), Hide),
            MenuItem::action(t!("app_menu.hide_others"), HideOthers),
            MenuItem::action(t!("app_menu.show_all"), ShowAll),
            MenuItem::separator(),
            MenuItem::action(t!("app_menu.quit"), Quit),
        ]),
        Menu::new(t!("app_menu.shell")).items([
            MenuItem::action(t!("app_menu.new_window"), NewWindow),
            MenuItem::action(t!("app_menu.new_local"), NewLocalTerminal),
            MenuItem::action(t!("app_menu.duplicate"), DuplicateSession),
            MenuItem::separator(),
            MenuItem::action(t!("app_menu.split_right"), SplitRight),
            MenuItem::action(t!("app_menu.split_down"), SplitDown),
            MenuItem::separator(),
            MenuItem::action(t!("app_menu.reconnect"), Reconnect),
            MenuItem::separator(),
            MenuItem::action(t!("app_menu.close_tab"), CloseTab),
            MenuItem::action(t!("app_menu.close_window"), CloseWindow),
        ]),
        Menu::new(t!("app_menu.edit")).items([
            MenuItem::os_action(t!("app_menu.undo"), text_input::Undo, OsAction::Undo),
            MenuItem::os_action(t!("app_menu.redo"), text_input::Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action(t!("app_menu.cut"), text_input::Cut, OsAction::Cut),
            MenuItem::os_action(t!("app_menu.copy"), text_input::Copy, OsAction::Copy),
            MenuItem::os_action(t!("app_menu.paste"), text_input::Paste, OsAction::Paste),
            MenuItem::os_action(
                t!("app_menu.select_all"),
                text_input::SelectAll,
                OsAction::SelectAll,
            ),
            MenuItem::separator(),
            MenuItem::action(t!("app_menu.find"), OpenSearch),
            MenuItem::action(t!("app_menu.clear"), Clear),
        ]),
        Menu::new(t!("app_menu.view")).items([
            MenuItem::action(t!("app_menu.toggle_sidebar"), ToggleSidebar),
            MenuItem::action(t!("app_menu.host_tools"), ToggleHostTools),
            MenuItem::action(t!("app_menu.host_tools_overview"), OpenHostToolsOverview),
            MenuItem::separator(),
            MenuItem::action(t!("app_menu.command_palette"), OpenCommandPalette),
            MenuItem::action(t!("app_menu.snippets"), OpenCommandPicker),
            MenuItem::action(t!("app_menu.compose"), ToggleCompose),
            MenuItem::action(t!("app_menu.multi_exec"), ToggleMultiExec),
            MenuItem::separator(),
            MenuItem::action(t!("app_menu.zoom_in"), ZoomIn),
            MenuItem::action(t!("app_menu.zoom_out"), ZoomOut),
            MenuItem::action(t!("app_menu.actual_size"), ZoomReset),
            MenuItem::separator(),
            MenuItem::action(t!("app_menu.ui_zoom_in"), ZoomInUi),
            MenuItem::action(t!("app_menu.ui_zoom_out"), ZoomOutUi),
            MenuItem::action(t!("app_menu.ui_zoom_reset"), ResetUiZoom),
            MenuItem::separator(),
            MenuItem::action(t!("app_menu.full_screen"), ToggleFullscreen),
        ]),
        Menu::new(t!("app_menu.window")).items([
            MenuItem::action(t!("app_menu.minimize"), Minimize),
            MenuItem::action(t!("app_menu.zoom"), Zoom),
            MenuItem::separator(),
            MenuItem::action(t!("app_menu.prev_tab"), PrevTab),
            MenuItem::action(t!("app_menu.next_tab"), NextTab),
            MenuItem::separator(),
            MenuItem::action(t!("app_menu.bring_all_to_front"), BringAllToFront),
        ]),
        Menu::new(t!("app_menu.help")).items([
            MenuItem::action(t!("app_menu.website"), OpenWebsite),
            MenuItem::action(t!("app_menu.privacy"), OpenPrivacy),
            MenuItem::separator(),
            MenuItem::action(t!("app_menu.feedback"), SendFeedback),
        ]),
    ]
}

fn with_active_window(cx: &mut App, f: impl FnOnce(&mut Window, &mut App)) {
    if let Some(handle) = cx.active_window() {
        let _ = handle.update(cx, |_, window, cx| f(window, cx));
    }
}

/// 打开着的主窗口。
fn main_windows(cx: &mut App) -> Vec<(AnyWindowHandle, Entity<Workspace>)> {
    cx.windows()
        .into_iter()
        .filter_map(|handle| {
            let workspace = handle
                .update(cx, |_, window, cx| Workspace::in_window(window, cx))
                .ok()
                .flatten()?;
            Some((handle, workspace))
        })
        .collect()
}

/// 退出：汇总所有窗口要说明的（与关闭窗口前的确认相同），有就在当前窗口中确认一次，确认后（或没有要说明的）关闭全部
/// 窗口；窗口关完后由 [`crate::cli_bridge`] 按原来的流程退出。
pub(crate) fn request_quit(cx: &mut App) {
    let windows = main_windows(cx);
    let summary = windows
        .iter()
        .fold(CloseSummary::default(), |summary, (_, workspace)| {
            summary.merge(workspace.read(cx).close_summary(cx))
        });
    let active = cx.active_window();
    let target = windows
        .iter()
        .find(|(handle, _)| Some(*handle) == active)
        .or(windows.last())
        .map(|(handle, _)| *handle);
    let Some(target) = target.filter(|_| summary.needs_confirm()) else {
        close_all(cx);
        return;
    };
    tracing::info!(?summary, "退出前确认");
    let _ = target.update(cx, |_, window, cx| {
        // 从托盘菜单退出时窗口可能隐藏着：先显示出来
        crate::tray::show_window(window, cx);
        close_confirm::confirm_quit(
            summary.description(close_confirm::CloseScope::Quit),
            |_, cx| cx.defer(close_all),
            window,
            cx,
        );
    });
}

/// 关闭全部窗口（不再逐个确认）；本来就没有窗口（无窗口模式）时直接按没有窗口的流程退出。
fn close_all(cx: &mut App) {
    let windows = cx.windows();
    if windows.is_empty() {
        crate::cli_bridge::quit_when_idle(cx);
        return;
    }
    tracing::info!(count = windows.len(), "退出：关闭全部窗口");
    // 窗口关完后退出（macOS 上也不留在程序坞）
    crate::cli_bridge::begin_quit(cx);
    // 记下最前面那个窗口的位置（下次打开主窗口时用）
    if let Some(handle) = cx.active_window().or_else(|| windows.last().copied()) {
        let _ = handle.update(cx, |_, window, cx| {
            if Workspace::in_window(window, cx).is_some() {
                crate::window_state::remember(window, cx);
            }
        });
    }
    for handle in windows {
        let _ = handle.update(cx, |_, window, _| window.remove_window());
    }
}

/// 关闭当前窗口：主窗口先按关闭按钮的规则确认。
fn close_window(cx: &mut App) {
    with_active_window(cx, |window, cx| {
        let close = match Workspace::in_window(window, cx) {
            Some(workspace) => {
                workspace.update(cx, |workspace, cx| workspace.should_close(window, cx))
            }
            None => true,
        };
        if close {
            window.remove_window();
        }
    });
}

/// 没有窗口时从菜单栏打开主窗口，打开后在其中做 `then`。
fn open_window_then(cx: &mut App, then: impl FnOnce(&mut Window, &mut App)) {
    if let Some(handle) = crate::cli_bridge::open_main_window_for_menu(cx) {
        let _ = handle.update(cx, |_, window, cx| then(window, cx));
    }
}

/// 打开设置的某一页（应用锁定时不打开，与工作区中的快捷键相同）。
fn open_settings(page: Page, window: &mut Window, cx: &mut App) {
    if !crate::lock_ui::is_locked(cx) {
        crate::settings::open(page, window, cx);
    }
}

/// 把全部窗口移到最前：当前窗口最后激活，仍在最前面。
fn bring_all_to_front(cx: &mut App) {
    let active = cx.active_window();
    let mut windows = cx.windows();
    windows.sort_by_key(|handle| Some(*handle) == active);
    for handle in windows {
        let _ = handle.update(cx, |_, window, _| window.activate_window());
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{Action, AppContext as _, Keystroke, TestAppContext};

    use super::*;
    use crate::i18n::{self, Language};
    use crate::keymap::tests::table;

    /// 菜单中的动作项（按菜单的顺序）。
    fn actions_of(menus: &[Menu]) -> Vec<Box<dyn Action>> {
        menus
            .iter()
            .flat_map(|menu| &menu.items)
            .filter_map(|item| match item {
                MenuItem::Action { action, .. } => Some(action.boxed_clone()),
                _ => None,
            })
            .collect()
    }

    /// 每个菜单项在 macOS 上显示的快捷键与 macOS 的键位表一致：GPUI 取这个动作最早的一条在工作区上下文中生效
    /// （不带上下文或上下文是工作区）的绑定，没有就取最早的一条。编辑菜单的剪切、复制等是 gpui-kit 输入框的动作，
    /// 快捷键是 gpui-kit 自己在 macOS 上注册的 ⌘X、⌘C 等，这里不查。
    #[test]
    fn menu_shortcuts_match_the_mac_keymap() {
        let expected: Vec<(&'static str, Option<&'static str>)> = vec![
            (OpenAbout.name(), None),
            (OpenSettings.name(), Some("cmd-,")),
            (Hide.name(), Some("cmd-h")),
            (HideOthers.name(), Some("alt-cmd-h")),
            (ShowAll.name(), None),
            (Quit.name(), Some("cmd-q")),
            (NewWindow.name(), Some("cmd-n")),
            (NewLocalTerminal.name(), Some("cmd-t")),
            (DuplicateSession.name(), Some("cmd-shift-t")),
            (SplitRight.name(), Some("cmd-d")),
            (SplitDown.name(), Some("cmd-shift-d")),
            (Reconnect.name(), Some("cmd-shift-r")),
            (CloseTab.name(), Some("cmd-w")),
            (CloseWindow.name(), Some("cmd-shift-w")),
            (OpenSearch.name(), Some("cmd-f")),
            (Clear.name(), Some("cmd-k")),
            (ToggleSidebar.name(), Some("ctrl-cmd-s")),
            (ToggleHostTools.name(), Some("cmd-shift-e")),
            (OpenHostToolsOverview.name(), Some("cmd-shift-o")),
            (OpenCommandPalette.name(), Some("cmd-shift-p")),
            (OpenCommandPicker.name(), Some("cmd-shift-space")),
            (ToggleCompose.name(), Some("cmd-shift-m")),
            (ToggleMultiExec.name(), Some("alt-cmd-i")),
            (ZoomIn.name(), Some("cmd-=")),
            (ZoomOut.name(), Some("cmd--")),
            (ZoomReset.name(), Some("cmd-0")),
            (ZoomInUi.name(), None),
            (ZoomOutUi.name(), None),
            (ResetUiZoom.name(), None),
            (ToggleFullscreen.name(), Some("ctrl-cmd-f")),
            (Minimize.name(), Some("cmd-m")),
            (Zoom.name(), None),
            (PrevTab.name(), Some("cmd-{")),
            (NextTab.name(), Some("cmd-}")),
            (BringAllToFront.name(), None),
            (OpenWebsite.name(), None),
            (OpenPrivacy.name(), None),
            (SendFeedback.name(), None),
        ];
        let os = OperatingSystem::Mac;
        let entries = table(os);
        let mut covered = Vec::new();
        for action in actions_of(&app_menus()) {
            let name = action.name();
            if name.starts_with("input::") {
                continue;
            }
            let (_, combo) = expected
                .iter()
                .find(|(n, _)| *n == name)
                .unwrap_or_else(|| panic!("{name} 不在预期的列表中"));
            covered.push(name);
            let bound: Vec<_> = entries.iter().filter(|e| e.action == name).collect();
            let shown = bound
                .iter()
                .find(|e| e.context.as_deref().is_none_or(|c| c == "Workspace"))
                .or(bound.first());
            match (combo, shown) {
                (None, None) => {}
                (Some(combo), Some(shown)) => {
                    let stroke = Keystroke::parse_with_os(combo, os).unwrap();
                    assert_eq!(
                        (stroke.modifiers, stroke.key.as_str()),
                        (shown.modifiers, shown.key.as_str()),
                        "{name} 在菜单上应显示 {combo}"
                    );
                }
                (combo, shown) => panic!(
                    "{name}：预期 {combo:?}，键位表中 {:?}",
                    shown.map(|e| (&e.modifiers, &e.key))
                ),
            }
        }
        assert_eq!(covered.len(), expected.len(), "每个预期的菜单项都在菜单中");
    }

    /// 每种语言下菜单的标题、各项的名称都有文案（不是键名），同一个菜单中不重名；只在英文界面下窗口菜单的标题是
    /// “Window”（GPUI 据此把它设为系统的窗口菜单），其他语言的由 [`refresh`] 按位置设置。
    #[test]
    fn menus_are_translated() {
        let original = i18n::language();
        for language in Language::ALL {
            i18n::set_language(language);
            let menus = app_menus();
            assert_eq!(menus.len(), 6);
            for menu in &menus {
                assert!(!menu.name.is_empty() && !menu.name.contains("app_menu."));
                let mut names = Vec::new();
                for item in &menu.items {
                    let name = match item {
                        MenuItem::Action { name, .. } => name.clone(),
                        MenuItem::SystemMenu(menu) => menu.name.clone(),
                        MenuItem::Submenu(menu) => menu.name.clone(),
                        MenuItem::Separator => continue,
                    };
                    assert!(
                        !name.is_empty() && !name.contains("app_menu."),
                        "{language:?}：{name}"
                    );
                    assert!(!names.contains(&name), "{language:?}：{name} 重名");
                    names.push(name);
                }
            }
            assert_eq!(
                menus[4].name.as_ref() == "Window",
                language == Language::En,
                "{language:?}"
            );
            // 每种语言都把窗口菜单设为系统的窗口菜单（英文的 GPUI 自己也设）
            assert_eq!(windows_menu_index(&menus), Some(4), "{language:?}");
        }
        i18n::set_language(original);
    }

    /// 只有 macOS 上有应用级动作的快捷键（Windows、Linux 的快捷键不变）。
    #[test]
    fn shortcuts_only_on_mac() {
        assert_eq!(key_bindings(OperatingSystem::Mac).len(), 5);
        assert!(key_bindings(OperatingSystem::Windows).is_empty());
        assert!(key_bindings(OperatingSystem::Linux).is_empty());
    }

    /// 菜单中的动作都有处理函数：应用级的随时可用；工作区的在主窗口中可用；终端的（含编辑菜单的复制、粘贴、全选）
    /// 在焦点位于终端时可用。
    #[gpui_kit::test]
    fn menu_actions_have_handlers(cx: &mut TestAppContext) {
        let handle = crate::testkit::open_workspace(cx);
        let global: Vec<Box<dyn Action>> = vec![
            Box::new(Quit),
            Box::new(Hide),
            Box::new(HideOthers),
            Box::new(ShowAll),
            Box::new(Minimize),
            Box::new(Zoom),
            Box::new(CloseWindow),
            Box::new(BringAllToFront),
            Box::new(OpenWebsite),
            Box::new(OpenPrivacy),
            Box::new(SendFeedback),
        ];
        let workspace: Vec<Box<dyn Action>> = vec![
            Box::new(OpenAbout),
            Box::new(OpenSettings),
            Box::new(NewWindow),
            Box::new(NewLocalTerminal),
            Box::new(DuplicateSession),
            Box::new(SplitRight),
            Box::new(SplitDown),
            Box::new(ToggleSidebar),
            Box::new(ToggleHostTools),
            Box::new(OpenHostToolsOverview),
            Box::new(OpenCommandPalette),
            Box::new(OpenCommandPicker),
            Box::new(ToggleCompose),
            Box::new(ToggleMultiExec),
            Box::new(ZoomInUi),
            Box::new(ZoomOutUi),
            Box::new(ResetUiZoom),
            Box::new(ToggleFullscreen),
            Box::new(PrevTab),
            Box::new(NextTab),
        ];
        let terminal: Vec<Box<dyn Action>> = vec![
            Box::new(Reconnect),
            Box::new(CloseTab),
            Box::new(OpenSearch),
            Box::new(Clear),
            Box::new(ZoomIn),
            Box::new(ZoomOut),
            Box::new(ZoomReset),
            Box::new(text_input::Copy),
            Box::new(text_input::Paste),
            Box::new(text_input::SelectAll),
        ];
        // 每个菜单动作都在三组之一（编辑菜单的撤销、重做、剪切只有输入框处理）
        let listed: Vec<&str> = global
            .iter()
            .chain(&workspace)
            .chain(&terminal)
            .map(|a| a.name())
            .collect();
        for action in actions_of(&app_menus()) {
            let name = action.name();
            assert!(
                listed.contains(&name)
                    || ["input::Undo", "input::Redo", "input::Cut"].contains(&name),
                "{name} 没有检查"
            );
        }
        cx.update(|cx| {
            for action in &global {
                assert!(cx.is_action_available(action.as_ref()), "{}", action.name());
            }
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            for action in &workspace {
                assert!(
                    window.is_action_available(action.as_ref(), cx),
                    "{}",
                    action.name()
                );
            }
            assert!(
                !window.is_action_available(&CloseTab, cx),
                "没有终端时不能关闭 Tab"
            );
            window.click("start-local-terminal", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("terminal", cx);
            window.render_frame(cx);
            for action in workspace.iter().chain(&terminal) {
                assert!(
                    window.is_action_available(action.as_ref(), cx),
                    "{}",
                    action.name()
                );
            }
        })
        .unwrap();
    }

    /// 退出：有本地终端在运行时先确认，取消后窗口还在；确认后关闭全部窗口。
    #[gpui_kit::test]
    fn quit_confirms_like_closing_the_window(cx: &mut TestAppContext) {
        i18n::set_language(Language::ZhCn);
        let handle = crate::testkit::open_workspace(cx);
        let quit = |cx: &mut TestAppContext| {
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.dispatch_action(Box::new(Quit), cx);
            })
            .unwrap();
            cx.run_until_parked();
        };
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("start-local-terminal", cx);
        })
        .unwrap();
        cx.run_until_parked();
        quit(cx);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let confirm = window.try_find("close-confirm");
            assert!(confirm.is_some(), "本地终端在运行：先确认");
            assert_eq!(
                confirm.as_ref().and_then(|c| c.label()),
                Some(t!("close.quit_confirm").as_ref()),
                "按钮是“退出”"
            );
            window.click("close-cancel", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| cx.windows().len()), 1, "取消后窗口还在");
        quit(cx);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("close-confirm", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert!(
            cx.update(|cx| cx.windows().is_empty()),
            "确认后关闭全部窗口"
        );
    }

    /// 没有要说明的（没有打开的终端）：退出、关闭窗口都直接关。
    #[gpui_kit::test]
    fn quit_without_sessions_closes_at_once(cx: &mut TestAppContext) {
        let handle = crate::testkit::open_workspace(cx);
        let second = cx
            .update(|cx| {
                gpui_kit::open_window(Default::default(), cx, |window, cx| {
                    cx.new(|cx| Workspace::new(window, cx))
                })
            })
            .unwrap()
            .0;
        cx.run_until_parked();
        cx.update_window(second, |_, window, cx| {
            window.activate_window();
            window.dispatch_action(Box::new(CloseWindow), cx)
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| cx.windows()), vec![handle], "关闭当前窗口");
        cx.update_window(handle, |_, window, cx| {
            window.activate_window();
            window.dispatch_action(Box::new(Quit), cx)
        })
        .unwrap();
        cx.run_until_parked();
        assert!(cx.update(|cx| cx.windows().is_empty()), "退出");
    }
}
