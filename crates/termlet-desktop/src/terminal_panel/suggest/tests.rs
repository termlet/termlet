//! 终端 Tab 的命令补全提示：按版本读历史；连真实的 sshd 时从记下历史到提示、接受、关闭的整个过程。

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};
use termlet_core::model::{Host, SecretField};
use termlet_core::secret::SecretString;
use termlet_core::store::{Owner, Position};
use termlet_terminal::TerminalView;

use super::*;
use crate::app_state::AppState;
use crate::testkit::{open_workspace, poll_until, test_server, wait_until};
use crate::workspace::Workspace;

// 只在历史的版本变了时重新读；新的在前，含控制字符的命令不用
#[gpui_kit::test]
fn history_is_reloaded_only_when_it_changed(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.update(AppState::install_in_memory);
    let store = cx.update(|cx| AppState::store(cx));
    history::record(&store, "h_web", "ls -la").unwrap();
    history::record(&store, "h_web", "printf 'a\\nb'\nrm -rf /tmp/x").unwrap();
    history::record(&store, "h_web", "git status").unwrap();
    let (revision, suggestions) = load(&store, "h_web", None).expect("第一次读");
    assert_eq!(suggestions.len(), 2, "多行的那条不用");
    assert_eq!(suggestions.complete("git "), Some("status"));
    assert!(load(&store, "h_web", Some(revision)).is_none(), "没有变化");
    history::record(&store, "h_web", "git stash").unwrap();
    let (_, suggestions) = load(&store, "h_web", Some(revision)).expect("记下了新的");
    assert_eq!(suggestions.complete("git st"), Some("ash"), "新的在前");
    assert!(
        load(&store, "h_db", None).is_some_and(|(_, s)| s.is_empty()),
        "别的主机没有"
    );
}

fn workspace_of(cx: &mut TestAppContext, handle: AnyWindowHandle) -> Entity<Workspace> {
    cx.update_window(handle, |_, window, cx| {
        Workspace::in_window(window, cx).unwrap()
    })
    .unwrap()
}

fn view_of(cx: &mut TestAppContext, terminal: &Entity<TerminalPanel>) -> Entity<TerminalView> {
    cx.update(|cx| terminal.read(cx).view.clone().expect("终端视图"))
}

/// 屏幕上最后一行有字的（提示符所在的行；下面还有空行）。
fn last_line(cx: &mut TestAppContext, terminal: &Entity<TerminalPanel>) -> String {
    let screen = cx.update(|cx| terminal.read(cx).screen_text(cx));
    screen
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .to_string()
}

fn suggestion(cx: &mut TestAppContext, view: &Entity<TerminalView>) -> Option<String> {
    cx.update(|cx| view.read(cx).command_suggestion())
}

fn type_text(cx: &mut TestAppContext, terminal: &Entity<TerminalPanel>, text: &str) {
    cx.update(|cx| terminal.update(cx, |t, cx| t.send_text(text, cx)));
}

/// 输入 `typed`（还没有回车），等提示变成 `expected`。
fn expect_suggestion(
    cx: &mut TestAppContext,
    terminal: &Entity<TerminalPanel>,
    typed: &str,
    expected: Option<&str>,
) {
    let view = view_of(cx, terminal);
    type_text(cx, terminal, typed);
    poll_until(cx, &format!("输入 {typed} 后的提示"), |cx| {
        last_line(cx, terminal).ends_with(typed) && suggestion(cx, &view).as_deref() == expected
    });
}

/// 放弃这一行（Ctrl+C），等新的提示符。
fn cancel_line(cx: &mut TestAppContext, terminal: &Entity<TerminalPanel>) {
    let prompts = |cx: &mut TestAppContext| {
        cx.update(|cx| terminal.read(cx).screen_text(cx))
            .lines()
            .filter(|l| l.contains('$'))
            .count()
    };
    let before = prompts(cx);
    type_text(cx, terminal, "\x03");
    poll_until(cx, "新的提示符", |cx| prompts(cx) > before);
}

/// 执行一条命令，等它的输出。
fn run(cx: &mut TestAppContext, terminal: &Entity<TerminalPanel>, word: &str) {
    type_text(cx, terminal, &format!("echo {word}\r"));
    poll_until(cx, word, |cx| {
        let screen = cx.update(|cx| terminal.read(cx).screen_text(cx));
        screen.matches(word).count() >= 2 && last_line(cx, terminal).contains('$')
    });
}

// 连真实的 sshd（TERMLET_TEST_SSH）：在 bash 中装上 shell 集成、执行几条命令，输入前缀时提示这台主机最近的一条；
// 别处记下、删掉的在下一个提示符更新；End 接受后整条执行；关掉开关或命令历史后没有提示
#[gpui_kit::test]
fn suggests_from_this_hosts_history_over_ssh(cx: &mut TestAppContext) {
    let Some((address, port)) = test_server() else {
        return;
    };
    let handle = open_workspace(cx);
    let workspace = workspace_of(cx, handle);
    let store = cx.update(|cx| AppState::store(cx));
    store
        .update_settings(|s| s.terminal.command_suggest = true)
        .unwrap();
    let host = store
        .create(
            Host {
                name: "suggest".into(),
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
    let owner = host.as_str().to_string();
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
    let terminal = cx.update(|cx| workspace.read(cx).terminals_of(&host, cx)[0].clone());
    let view = view_of(cx, &terminal);

    // 装上 shell 集成（设置中给出的 bash 片段）
    type_text(
        cx,
        &terminal,
        concat!(
            r#"PS1='\[\e]133;A\a\]'"$PS1"'\[\e]133;B\a\]'; PS0=$'\e]133;C\a'"#,
            "\r"
        ),
    );
    poll_until(cx, "shell 集成", |cx| {
        cx.update(|cx| view.read(cx).has_shell_integration())
    });
    run(cx, &terminal, "suggest-alpha-1");
    run(cx, &terminal, "suggest-beta-2");

    // 最近的优先；接着输入就换成对得上的那条
    expect_suggestion(cx, &terminal, "echo sugg", Some("est-beta-2"));
    expect_suggestion(cx, &terminal, "est-a", Some("lpha-1"));
    cancel_line(cx, &terminal);

    // 别处记下的（另一个 Tab、命令历史窗口）：下一个提示符时读到
    history::record(&store, &owner, "echo suggest-gamma-3").unwrap();
    cancel_line(cx, &terminal);
    expect_suggestion(cx, &terminal, "echo suggest-", Some("gamma-3"));
    cancel_line(cx, &terminal);
    // 删掉的不再提示
    history::delete(&store, &owner, "echo suggest-gamma-3").unwrap();
    cancel_line(cx, &terminal);
    expect_suggestion(cx, &terminal, "echo suggest-", Some("beta-2"));

    // End 接受整条，回车执行
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.press("end", cx);
    })
    .unwrap();
    poll_until(cx, "接受的命令回显", |cx| {
        last_line(cx, &terminal).ends_with("echo suggest-beta-2")
    });
    assert_eq!(suggestion(cx, &view), None, "已经完整");
    type_text(cx, &terminal, "\r");
    poll_until(cx, "执行接受的命令", |cx| {
        cx.update(|cx| terminal.read(cx).screen_text(cx))
            .matches("suggest-beta-2")
            .count()
            >= 4
    });

    // 关掉开关：没有提示；再打开又有
    store
        .update_settings(|s| s.terminal.command_suggest = false)
        .unwrap();
    expect_suggestion(cx, &terminal, "echo sugg", None);
    store
        .update_settings(|s| s.terminal.command_suggest = true)
        .unwrap();
    poll_until(cx, "重新打开后的提示", |cx| {
        suggestion(cx, &view).as_deref() == Some("est-beta-2")
    });
    // 关掉命令历史：也没有
    store
        .update_settings(|s| s.terminal.command_history = false)
        .unwrap();
    poll_until(cx, "关掉命令历史后没有提示", |cx| {
        suggestion(cx, &view).is_none()
    });
    cancel_line(cx, &terminal);
}
