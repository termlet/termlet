//! 提示中的大小；连真实的 sshd（TERMLET_TEST_SSH）：服务器上的程序经 OSC 52 写本机剪贴板，弹出提示、可以查看内容；设置关掉后不写。
//! 测试中的剪贴板是测试平台自己的，不碰系统的剪贴板。

use gpui_kit::component::WindowExt as _;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, ClipboardItem, Entity, Focusable as _, TestAppContext,
};
use termlet_core::model::{Host, SecretField};
use termlet_core::secret::SecretString;
use termlet_core::store::{Owner, Position};

use super::megabytes;
use crate::app_state::AppState;
use crate::terminal_panel::TerminalPanel;
use crate::testkit::{open_workspace, poll_until, test_server, wait_until};
use crate::workspace::Workspace;

fn clipboard_text(cx: &mut TestAppContext) -> Option<String> {
    cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()))
}

fn type_text(cx: &mut TestAppContext, terminal: &Entity<TerminalPanel>, text: &str) {
    cx.update(|cx| terminal.update(cx, |t, cx| t.send_text(text, cx)));
}

/// 在服务器上用 printf 写剪贴板（与 pi、tmux 发出的相同），等这一行执行完。
fn printf_osc52(cx: &mut TestAppContext, terminal: &Entity<TerminalPanel>, word: &str) {
    type_text(
        cx,
        terminal,
        &format!("printf '\\e]52;c;%s\\a' $(echo -n {word} | base64); echo done-{word}\r"),
    );
    poll_until(cx, &format!("执行完 {word}"), |cx| {
        cx.update(|cx| terminal.read(cx).screen_text(cx))
            .lines()
            .any(|line| line.trim() == format!("done-{word}"))
    });
}

fn open_host(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    address: String,
    port: u16,
) -> Entity<TerminalPanel> {
    let workspace = cx
        .update_window(handle, |_, window, cx| {
            Workspace::in_window(window, cx).unwrap()
        })
        .unwrap();
    let store = cx.update(|cx| AppState::store(cx));
    let host = store
        .create(
            Host {
                name: "web-osc52".into(),
                address,
                port,
                username: "termlet".into(),
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    store
        .set_secret(
            &Owner::host(&host),
            SecretField::Password,
            Some(&SecretString::new("termlet-e2e")),
        )
        .unwrap();
    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |w, cx| w.open_host(&host, window, cx))
    })
    .unwrap();
    wait_until(cx, handle, "终端连上", |window, cx| {
        if window.try_find("prompt-ok").is_some() {
            window.click("prompt-ok", cx);
        }
        workspace
            .read(cx)
            .terminals_of(&host, cx)
            .first()
            .is_some_and(|t| t.read(cx).is_live(cx) && t.read(cx).screen_text(cx).contains('$'))
    });
    cx.update(|cx| workspace.read(cx).terminals_of(&host, cx)[0].clone())
}

#[gpui_kit::test]
fn programs_on_the_server_write_the_clipboard(cx: &mut TestAppContext) {
    let Some((address, port)) = test_server() else {
        return;
    };
    let handle = open_workspace(cx);
    let terminal = open_host(cx, handle, address, port);
    cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string("之前的内容".into())));
    // 只在窗口在前台、焦点在终端上时写
    cx.update_window(handle, |_, window, cx| {
        window.activate_window();
        let view = terminal.read(cx).view.clone().expect("终端视图");
        view.focus_handle(cx).focus(window, cx);
    })
    .unwrap();

    // 默认允许：写入，弹出“web-osc52 写入了剪贴板（2 字）”，点“查看”看到内容
    printf_osc52(cx, &terminal, "hi");
    wait_until(cx, handle, "写入剪贴板并提示", |window, cx| {
        cx.read_from_clipboard()
            .and_then(|item| item.text())
            .as_deref()
            == Some("hi")
            && window.try_find("clipboard-written-view").is_some()
    });
    cx.update_window(handle, |_, window, cx| {
        assert_eq!(window.notifications(cx).len(), 1);
        window.click("clipboard-written-view", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("clipboard-written-content").is_some());
    })
    .unwrap();

    // 关上对话框，焦点放回终端
    cx.update_window(handle, |_, window, cx| {
        window.press("escape", cx);
        window.clear_notifications(cx);
    })
    .unwrap();
    cx.run_until_parked();
    let focus_terminal = |cx: &mut TestAppContext| {
        cx.update_window(handle, |_, window, cx| {
            let view = terminal.read(cx).view.clone().expect("终端视图");
            view.focus_handle(cx).focus(window, cx);
            window.render_frame(cx);
        })
        .unwrap();
    };
    focus_terminal(cx);

    // 设置关掉：有焦点也不写（不写就没有提示：视图只在写了之后才发出事件）
    let store = cx.update(|cx| AppState::store(cx));
    store
        .update_settings(|s| s.terminal.clipboard_write = false)
        .unwrap();
    cx.run_until_parked();
    printf_osc52(cx, &terminal, "bye");
    cx.run_until_parked();
    assert_eq!(clipboard_text(cx).as_deref(), Some("hi"));

    // 再打开：照常写入（确认上面没写是因为设置，不是因为焦点）
    store
        .update_settings(|s| s.terminal.clipboard_write = true)
        .unwrap();
    cx.run_until_parked();
    focus_terminal(cx);
    printf_osc52(cx, &terminal, "again");
    wait_until(cx, handle, "再次写入", |_, cx| {
        cx.read_from_clipboard()
            .and_then(|item| item.text())
            .as_deref()
            == Some("again")
    });

    // 有选中内容时 Ctrl+C 复制，不发给服务器
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.press("ctrl-shift-a", cx);
        window.press("ctrl-c", cx);
    })
    .unwrap();
    let copied = clipboard_text(cx).unwrap_or_default();
    assert!(copied.contains("done-again"), "复制到：{copied:?}");
    cx.run_until_parked();
    assert!(
        !cx.update(|cx| terminal.read(cx).screen_text(cx))
            .contains("^C"),
        "Ctrl+C 没有发给服务器"
    );
}

// 太大时提示中的大小：最多两位小数，刚过上限的也看得出比上限大
#[test]
fn sizes_show_up_to_two_decimals() {
    assert_eq!(megabytes(1 << 20), "1 MB");
    assert_eq!(megabytes(1_100_000), "1.05 MB");
    assert_eq!(megabytes(3 << 19), "1.5 MB");
}
