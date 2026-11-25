//! 关闭窗口时最小化到托盘的 UI 集成测试：生产环境的工作区，经真实的关闭按钮回调（`simulate_close`）与托盘命令
//! 的通道操作；图标与窗口的隐藏、显示换成记录调用的 [`FakeHost`]（测试窗口没有 HWND，也不该在通知区域留下图标）。

use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, OperatingSystem, PlatformInfo, PlatformTarget,
    TestAppContext, VisualTestContext, Window, WindowBounds, WindowOptions, px, size,
};

use super::{TrayCommand, TrayHost, install_with, is_hidden};
use crate::app_state::AppState;
use crate::i18n::{self, Language};
use crate::t;
use crate::testkit::{poll_until, wait_until};
use crate::workspace::Workspace;

/// 记录调用：`icon+`、`icon-`、`hide`、`show`。
struct FakeHost {
    log: Rc<RefCell<Vec<&'static str>>>,
}

impl TrayHost for FakeHost {
    fn show_icon(&mut self) -> bool {
        self.log.borrow_mut().push("icon+");
        true
    }

    fn remove_icon(&mut self) {
        self.log.borrow_mut().push("icon-");
    }

    fn hide_window(&mut self, _: &Window) {
        self.log.borrow_mut().push("hide");
    }

    fn show_window(&mut self, _: &Window) {
        self.log.borrow_mut().push("show");
    }
}

struct Fixture {
    commands: async_channel::Sender<TrayCommand>,
    log: Rc<RefCell<Vec<&'static str>>>,
}

impl Fixture {
    /// 取出记录的调用。
    fn take(&self) -> Vec<&'static str> {
        std::mem::take(&mut *self.log.borrow_mut())
    }

    /// 经通道发一个托盘命令（与图标的消息处理相同的路径）。
    fn send(&self, cx: &mut TestAppContext, command: TrayCommand) {
        self.commands.try_send(command).unwrap();
        cx.run_until_parked();
    }
}

/// 装好应用状态（“关闭窗口时最小化到托盘”按 `close_to_tray`）与假的托盘，按 Windows 运行（只有 Windows 上有这个
/// 功能；在别的系统上跑测试时也一样）。
fn setup(cx: &mut TestAppContext, close_to_tray: bool) -> Fixture {
    cx.set_platform_info(PlatformInfo {
        os: OperatingSystem::Windows,
        target: PlatformTarget::Native,
    });
    i18n::set_language(Language::ZhCn);
    cx.executor().allow_parking();
    cx.update(gpui_kit::init);
    cx.update(crate::init);
    cx.update(AppState::install_in_memory);
    // 对话框的入场动画按真实时间推进，动画中点击会偶尔落空
    cx.update(|cx| cx.set_reduce_motion(true));
    set_close_to_tray(cx, close_to_tray);
    let (commands, received) = async_channel::unbounded();
    let log = Rc::new(RefCell::new(Vec::new()));
    let host = FakeHost { log: log.clone() };
    cx.update(|cx| install_with(Box::new(host), received, cx));
    cx.run_until_parked();
    Fixture { commands, log }
}

fn set_close_to_tray(cx: &mut TestAppContext, on: bool) {
    cx.update(|cx| {
        AppState::store(cx)
            .update_local_settings(|s| s.window.close_to_tray = on)
            .unwrap()
    });
    cx.run_until_parked();
}

/// 打开一个主窗口。
fn open_window(cx: &mut TestAppContext) -> AnyWindowHandle {
    let handle = cx
        .update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        Default::default(),
                        size(px(1024.), px(640.)),
                    ))),
                    ..Default::default()
                },
                cx,
                |window, cx| cx.new(|cx| Workspace::new(window, cx)),
            )
        })
        .expect("打开测试窗口")
        .0;
    cx.run_until_parked();
    handle
}

/// 点窗口的关闭按钮（经关闭前的回调；回调同意时像系统那样关掉窗口），返回窗口是否关掉了。
fn click_close(cx: &mut TestAppContext, handle: AnyWindowHandle) -> bool {
    let closed = VisualTestContext::from_window(handle, cx).simulate_close();
    if closed {
        cx.update_window(handle, |_, window, _| window.remove_window())
            .unwrap();
    }
    cx.run_until_parked();
    closed
}

fn hidden(cx: &mut TestAppContext, handle: AnyWindowHandle) -> bool {
    cx.update(|cx| is_hidden(handle, cx))
}

/// 在窗口中开一个本地终端，等它运行起来。
fn start_local_terminal(cx: &mut TestAppContext, handle: AnyWindowHandle) {
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("start-local-terminal", cx);
    })
    .unwrap();
    poll_until(cx, "本地终端运行起来", |cx| {
        terminal_live(cx, handle)
    });
}

fn terminal_live(cx: &mut TestAppContext, handle: AnyWindowHandle) -> bool {
    cx.update_window(handle, |_, window, cx| {
        Workspace::in_window(window, cx).is_some_and(|workspace| {
            let terminals = workspace.read(cx).live_terminals();
            terminals.len() == 1 && terminals[0].read(cx).is_live(cx)
        })
    })
    .unwrap()
}

// 开关关着（默认）：没有托盘图标，点关闭按钮照旧关闭窗口
#[gpui_kit::test]
fn closing_without_the_setting_closes_as_before(cx: &mut TestAppContext) {
    let fixture = setup(cx, false);
    let handle = open_window(cx);
    assert!(fixture.take().is_empty(), "没有图标");
    assert!(click_close(cx, handle), "照旧关闭");
    assert!(cx.update(|cx| cx.windows().is_empty()));
    assert!(fixture.take().is_empty());
}

// 开关开着：有主窗口时显示图标；关闭最后一个窗口时隐藏到托盘（不确认，本地终端照常运行），单击图标恢复
#[gpui_kit::test]
fn closing_the_last_window_hides_it_in_the_tray(cx: &mut TestAppContext) {
    let fixture = setup(cx, true);
    assert!(
        fixture.take().is_empty(),
        "还没有主窗口（无窗口模式）时没有图标"
    );
    let handle = open_window(cx);
    assert_eq!(fixture.take(), ["icon+"]);
    start_local_terminal(cx, handle);

    assert!(!click_close(cx, handle), "不关闭");
    assert_eq!(fixture.take(), ["hide"]);
    assert!(hidden(cx, handle));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("close-confirm").is_none(), "隐藏不用确认");
    })
    .unwrap();
    assert!(terminal_live(cx, handle), "会话照常运行");

    fixture.send(cx, TrayCommand::Show);
    assert_eq!(fixture.take(), ["show"]);
    assert!(!hidden(cx, handle));
    // 显示着时单击图标只是激活
    fixture.send(cx, TrayCommand::Show);
    assert!(fixture.take().is_empty());
}

// 多个窗口：还有别的显示着的主窗口时照常关闭，只有最后一个进托盘
#[gpui_kit::test]
fn only_the_last_visible_window_goes_to_the_tray(cx: &mut TestAppContext) {
    let fixture = setup(cx, true);
    let first = open_window(cx);
    let second = open_window(cx);
    assert_eq!(fixture.take(), ["icon+"], "一个图标");
    assert!(click_close(cx, first), "还有别的窗口：照常关闭");
    assert_eq!(cx.update(|cx| cx.windows()), [second]);
    assert!(!click_close(cx, second), "最后一个：进托盘");
    assert!(hidden(cx, second));
    assert_eq!(fixture.take(), ["hide"]);
}

// 主窗口隐藏到托盘：主机概览的小窗一起关闭（终端照常运行）
#[gpui_kit::test]
fn hiding_in_the_tray_closes_the_overview_windows(cx: &mut TestAppContext) {
    let _fixture = setup(cx, true);
    let handle = open_window(cx);
    start_local_terminal(cx, handle);
    cx.update_window(handle, |_, window, cx| {
        let terminal = Workspace::in_window(window, cx)
            .unwrap()
            .read(cx)
            .live_terminals()[0]
            .clone();
        crate::host_tools::pop_out_for_test(&terminal, "h_web", window, cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(cx.read(crate::host_tools::overview_windows).len(), 1);
    assert!(!click_close(cx, handle), "进托盘");
    assert!(
        cx.read(crate::host_tools::overview_windows).is_empty(),
        "小窗一起关闭"
    );
    assert_eq!(cx.update(|cx| cx.windows()), [handle]);
    assert!(terminal_live(cx, handle), "终端照常运行");
}

// 托盘菜单的“退出”：与退出相同，本地终端在运行时先在（显示出来的）窗口中确认，确认后关闭全部窗口
#[gpui_kit::test]
fn quitting_from_the_tray_confirms_in_the_shown_window(cx: &mut TestAppContext) {
    let fixture = setup(cx, true);
    let handle = open_window(cx);
    start_local_terminal(cx, handle);
    assert!(!click_close(cx, handle));
    fixture.take();

    fixture.send(cx, TrayCommand::Quit);
    assert_eq!(fixture.take(), ["show"], "先把窗口显示出来");
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let confirm = window.try_find("close-confirm");
        assert_eq!(
            confirm.as_ref().and_then(|c| c.label()),
            Some(t!("close.quit_confirm").as_ref()),
            "按钮是“退出”"
        );
        window.click("close-confirm", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(
        cx.update(|cx| cx.windows().is_empty()),
        "确认后关闭全部窗口"
    );
    assert_eq!(fixture.take(), ["icon-"], "没有主窗口了，去掉图标");
}

// 关掉开关：隐藏着的窗口显示出来，图标去掉；之后关闭按钮照旧关闭
#[gpui_kit::test]
fn turning_the_setting_off_shows_hidden_windows(cx: &mut TestAppContext) {
    let fixture = setup(cx, true);
    let handle = open_window(cx);
    assert!(!click_close(cx, handle));
    fixture.take();
    set_close_to_tray(cx, false);
    assert_eq!(fixture.take(), ["show", "icon-"]);
    assert!(!hidden(cx, handle));
    assert!(click_close(cx, handle));
}

// 锁定时从托盘退出：显示窗口与锁屏上的退出确认（锁屏挡住对话框，也不显示会话的情况）
#[gpui_kit::test]
fn quitting_while_locked_asks_on_the_lock_screen(cx: &mut TestAppContext) {
    let fixture = setup(cx, true);
    let handle = open_window(cx);
    let lock = cx
        .update(|cx| crate::lock_ui::app_lock(cx))
        .expect("应用锁");
    termlet_core::runtime::runtime()
        .block_on(lock.set_password(
            None,
            &termlet_core::secret::SecretString::new("secret-1".to_string()),
        ))
        .expect("设置锁定密码");
    cx.update(crate::lock_ui::sync_lock_model);
    assert!(cx.update(crate::lock_ui::lock_now));
    cx.run_until_parked();
    assert!(!click_close(cx, handle), "锁定时同样进托盘");
    fixture.take();

    fixture.send(cx, TrayCommand::Quit);
    assert_eq!(fixture.take(), ["show"]);
    wait_until(cx, handle, "锁屏上的退出确认", |window, _| {
        window.try_find("lock-screen").is_some() && window.try_find("lock-screen-quit-ok").is_some()
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("close-confirm").is_none(), "不弹会话的确认");
    })
    .unwrap();
    assert_eq!(cx.update(|cx| cx.windows().len()), 1, "确认前不退出");
}

/// 在模拟的 `os` 上打开设置的“通用”页，返回有没有“关闭窗口时最小化到托盘”；有的话勾上并保存。
fn tray_setting_on(cx: &mut TestAppContext, handle: AnyWindowHandle, os: OperatingSystem) -> bool {
    cx.set_platform_info(PlatformInfo {
        os,
        target: PlatformTarget::Native,
    });
    let shown = cx
        .update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("open-settings", cx);
            window.render_frame(cx);
            window.click("settings-nav-general", cx);
            window.render_frame(cx);
            let shown = window.try_find("settings-close-to-tray").is_some();
            if shown {
                window.click("settings-close-to-tray", cx);
                window.render_frame(cx);
                window.click("settings-save", cx);
            }
            shown
        })
        .unwrap();
    cx.run_until_parked();
    shown
}

fn close_to_tray(cx: &mut App) -> bool {
    AppState::store(cx).local_settings().window.close_to_tray
}

// 设置项只在 Windows 的“通用”页上有，勾上后保存进本机设置；macOS、Linux 上没有
#[gpui_kit::test]
fn the_setting_is_only_on_windows(cx: &mut TestAppContext) {
    let _fixture = setup(cx, false);
    let handle = open_window(cx);
    assert!(!tray_setting_on(cx, handle, OperatingSystem::Mac));
    assert!(!tray_setting_on(cx, handle, OperatingSystem::Linux));
    assert!(!cx.update(close_to_tray));
    assert!(tray_setting_on(cx, handle, OperatingSystem::Windows));
    assert!(cx.update(close_to_tray), "保存进本机设置");
}
