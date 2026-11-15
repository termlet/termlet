//! 主机概览的小窗连真实的 sshd（`TERMLET_TEST_SSH=127.0.0.1:2222`）：命令面板的动作弹出，终端不在前台时照常采样，
//! 连接断开时显示“已断开”、点小窗中的“重新连接”重连后继续，小窗关掉后恢复原来的规则（不在前台不采样），终端的
//! Tab 关闭时小窗随之关闭。
//! 采样的间隔用测试的时钟推进（[`tick_until`]）。

use std::time::{Duration, Instant};

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};

use super::PopOutHostOverview;
use super::tools_follow_tests::{click_terminal, connect, open, open_local};
use crate::app_state::AppState;
use crate::host_tools::overview_windows;
use crate::terminal_panel::{TerminalPanel, TerminalPanelEvent};
use crate::testkit::{test_server, wait_until};
use crate::workspace::Workspace;

/// 推进测试的时钟（每次 1 秒）并让出时间给核心运行时上的读取，直到 `ready`；最多 60 秒。
fn tick_until(
    cx: &mut TestAppContext,
    what: &str,
    mut ready: impl FnMut(&mut TestAppContext) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        if ready(cx) {
            return;
        }
        assert!(Instant::now() < deadline, "等待超时：{what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// 推进测试的时钟这么多秒（每秒让出一点时间给核心运行时上的读取）。
fn tick(cx: &mut TestAppContext, seconds: u64) {
    for _ in 0..seconds {
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(150));
    }
    cx.run_until_parked();
}

/// 最近一次采样的时间。
fn last_sample(cx: &mut TestAppContext, terminal: &Entity<TerminalPanel>) -> Option<i64> {
    cx.read(|cx| terminal.read(cx).monitor_history().last_at())
}

/// 等到采到新的一次（比 `after` 晚）。
fn wait_sample(
    cx: &mut TestAppContext,
    terminal: &Entity<TerminalPanel>,
    after: Option<i64>,
    what: &str,
) {
    tick_until(cx, what, |cx| last_sample(cx, terminal) > after);
}

/// 切到 `terminal`，焦点在它上面时经命令面板的动作弹出它的主机的概览，等小窗打开；返回小窗。
fn pop_out(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    terminal: &Entity<TerminalPanel>,
) -> AnyWindowHandle {
    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |w, cx| w.show_terminal(terminal, window, cx))
    })
    .unwrap();
    click_terminal(cx, handle, terminal);
    cx.update_window(handle, |_, window, cx| {
        window.dispatch_action(Box::new(PopOutHostOverview), cx)
    })
    .unwrap();
    tick_until(cx, "小窗打开", |cx| {
        cx.read(overview_windows).len() == 1
    });
    cx.read(overview_windows)[0]
}

#[gpui_kit::test]
fn pop_out_samples_a_real_host_while_the_terminal_is_hidden(cx: &mut TestAppContext) {
    let Some(server) = test_server() else {
        eprintln!("没有设置 TERMLET_TEST_SSH，跳过");
        return;
    };
    let (handle, workspace) = open(cx, "overview");
    // 断开后停在“已断开”，由测试重连
    cx.update(|cx| {
        AppState::store(cx)
            .update_settings(|s| s.connection.auto_reconnect = false)
            .unwrap()
    });
    let host = connect(cx, handle, &workspace, "popout", server);
    let shown = last_sample(cx, &host);
    wait_sample(cx, &host, shown, "终端在前台时采样");
    let popout = pop_out(cx, handle, &workspace, &host);
    wait_until(cx, popout, "小窗中显示资源", |window, _| {
        window.try_find("tools-overview-cpu-chart").is_some()
    });

    // 新开的本地终端成为当前 Tab：主机的终端不在前台，小窗开着时照常采样
    let _local = open_local(cx, handle, &workspace);
    assert!(
        !cx.read(|cx| host.read(cx).is_shown()),
        "主机的终端不在前台"
    );
    assert!(cx.read(|cx| host.read(cx).samples_now()));
    let hidden = last_sample(cx, &host);
    wait_sample(cx, &host, hidden, "不在前台时第一次采样");
    let next = last_sample(cx, &host);
    wait_sample(cx, &host, next, "不在前台时第二次采样");

    // 连接断开：小窗显示已断开，终端停止采样
    let connection = cx.read(|cx| host.read(cx).connection()).expect("连着");
    termlet_core::runtime::spawn(async move { connection.disconnect().await });
    tick_until(cx, "小窗显示已断开", |cx| {
        cx.update_window(popout, |_, window, cx| {
            window.render_frame(cx);
            window.try_find("tools-overview-ended").is_some()
                || window.try_find("tools-overview-disconnected").is_some()
        })
        .unwrap()
    });
    tick(cx, 2);
    let lost = last_sample(cx, &host);
    tick(cx, 8);
    assert_eq!(last_sample(cx, &host), lost, "断开后不再采样");

    // 小窗中的“重新连接”：与终端的重新连接相同，重连后继续
    cx.update_window(popout, |_, window, cx| {
        window.render_frame(cx);
        window.click("overview-window-reconnect", cx);
    })
    .unwrap();
    tick_until(cx, "重连后小窗显示资源", |cx| {
        cx.update_window(popout, |_, window, cx| {
            window.render_frame(cx);
            window.try_find("tools-overview-cpu-chart").is_some()
        })
        .unwrap()
    });
    wait_sample(cx, &host, lost, "重连后照常采样");

    // 关掉小窗：恢复原来的规则，不在前台不采样（关之前发出的那次可能还在路上，先等它回来）
    cx.update_window(popout, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    assert!(!cx.read(|cx| host.read(cx).samples_now()));
    tick(cx, 4);
    let closed = last_sample(cx, &host);
    tick(cx, 8);
    assert_eq!(last_sample(cx, &host), closed, "小窗关掉后不在前台不采样");

    // 再弹出，关掉终端的 Tab：小窗随之关闭
    let popout = pop_out(cx, handle, &workspace, &host);
    cx.update(|cx| host.update(cx, |_, cx| cx.emit(TerminalPanelEvent::Close)));
    drop(host);
    tick_until(cx, "终端的 Tab 关闭后小窗关闭", |cx| {
        cx.read(overview_windows).is_empty() && !cx.read(|cx| cx.windows().contains(&popout))
    });
}
