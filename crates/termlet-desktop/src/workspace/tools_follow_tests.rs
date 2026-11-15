//! 主机工具、快捷命令对着哪个终端（[`Workspace::command_target`]）：跟随显示着并最近获得焦点的终端，焦点移进
//! 主机工具等处时不变；切换 Tab、新开的终端成为当前 Tab、分屏中点了另一格时跟过去。
//! 测试窗口不在前台（GPUI 不发焦点事件），正好覆盖用投递的输入操作、连接中切到别的程序时的情况。

use gpui_kit::component::Placement;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Entity, EntityId, Focusable as _, TestAppContext,
};

use crate::app_state::AppState;
use crate::terminal_panel::TerminalPanel;
use crate::testkit::{poll_until, test_server, wait_until};
use crate::workspace::Workspace;

/// 打开主窗口，主机工具展开在 `page` 页。
pub(super) fn open(cx: &mut TestAppContext, page: &str) -> (AnyWindowHandle, Entity<Workspace>) {
    cx.executor().allow_parking();
    cx.update(gpui_kit::init);
    cx.update(crate::init);
    cx.update(AppState::install_in_memory);
    cx.update(|cx| cx.set_reduce_motion(true));
    let page = page.to_string();
    cx.update(|cx| {
        AppState::ui(cx).update(cx, |ui, cx| {
            ui.update(
                |s| {
                    s.host_tools_open = true;
                    s.host_tools_page = Some(page);
                },
                cx,
            )
        })
    });
    let (handle, workspace) = cx.update(|cx| {
        gpui_kit::open_window(Default::default(), cx, |window, cx| {
            cx.new(|cx| Workspace::new(window, cx))
        })
        .unwrap()
    });
    (handle, workspace)
}

/// 新开一个本地终端，等它启动；返回它。
pub(super) fn open_local(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    workspace: &Entity<Workspace>,
) -> Entity<TerminalPanel> {
    let before = cx.update(|cx| workspace.read(cx).live_terminals().len());
    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |w, cx| w.open_local_terminal(window, cx))
    })
    .unwrap();
    poll_until(cx, "本地终端启动", |cx| {
        cx.update(|cx| {
            let terminals = workspace.read(cx).live_terminals();
            terminals.len() > before && terminals.last().unwrap().read(cx).is_live(cx)
        })
    });
    cx.update(|cx| workspace.read(cx).live_terminals().pop().unwrap())
}

/// 主机工具跟随的终端与快捷命令对着的终端（两者应当一致）。
fn targets(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    workspace: &Entity<Workspace>,
) -> (Option<EntityId>, Option<EntityId>) {
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let w = workspace.read(cx);
        (
            w.host_tools.read(cx).target_id(),
            w.command_target(window, cx).map(|p| p.entity_id()),
        )
    })
    .unwrap()
}

fn assert_target(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    expected: EntityId,
    what: &str,
) {
    cx.run_until_parked();
    assert_eq!(
        targets(cx, handle, workspace),
        (Some(expected), Some(expected)),
        "{what}"
    );
}

fn click(cx: &mut TestAppContext, handle: AnyWindowHandle, id: impl Into<gpui_kit::ElementId>) {
    let id = id.into();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click(id, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn tab(panel: &Entity<TerminalPanel>) -> (&'static str, usize) {
    ("tab-title", panel.entity_id().as_u64() as usize)
}

/// 点这个终端的终端区域（分屏时各格都有一个终端视图）。
pub(super) fn click_terminal(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    panel: &Entity<TerminalPanel>,
) {
    let scope = gpui_kit::ElementId::View(panel.entity_id());
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within(scope).click("terminal", cx);
    })
    .unwrap();
    cx.run_until_parked();
}

// 两个终端在同一组 Tab 中：点了显示着的终端后点主机工具里不能获得焦点的地方，面板仍跟随它（不跳到看不见的
// 另一个 Tab）；切 Tab 时跟过去
#[gpui_kit::test]
fn clicking_the_panel_keeps_the_target_and_switching_tabs_follows(cx: &mut TestAppContext) {
    let (handle, workspace) = open(cx, "processes");
    let first = open_local(cx, handle, &workspace);
    let second = open_local(cx, handle, &workspace);
    assert_target(cx, handle, &workspace, second.entity_id(), "新开的终端");

    // 切到第一个 Tab，点它的终端
    click(cx, handle, tab(&first));
    assert_target(cx, handle, &workspace, first.entity_id(), "切 Tab 后跟过去");
    click(cx, handle, "terminal");
    assert_target(cx, handle, &workspace, first.entity_id(), "点了终端");

    // 点主机工具的标题、说明（不能获得焦点，焦点落到工作区）：目标不变
    click(cx, handle, "host-tools-title");
    assert_target(cx, handle, &workspace, first.entity_id(), "点了面板的标题");
    click(cx, handle, "host-tools-notice");
    assert_target(cx, handle, &workspace, first.entity_id(), "点了面板的内容");

    // 切回第二个
    click(cx, handle, tab(&second));
    assert_target(cx, handle, &workspace, second.entity_id(), "切回第二个 Tab");
    click(cx, handle, "host-tools-title");
    assert_target(cx, handle, &workspace, second.entity_id(), "再点面板");
}

// 焦点在面板里时新开的终端：成为当前 Tab 后面板跟过去
#[gpui_kit::test]
fn a_new_terminal_becoming_the_current_tab_is_followed(cx: &mut TestAppContext) {
    let (handle, workspace) = open(cx, "processes");
    let first = open_local(cx, handle, &workspace);
    click(cx, handle, "terminal");
    click(cx, handle, "host-tools-title");
    assert_target(cx, handle, &workspace, first.entity_id(), "第一个终端");

    let second = open_local(cx, handle, &workspace);
    assert_target(
        cx,
        handle,
        &workspace,
        second.entity_id(),
        "新终端成为当前 Tab",
    );
    click(cx, handle, "host-tools-title");
    assert_target(cx, handle, &workspace, second.entity_id(), "之后点面板");
}

// 分屏：跟随获得焦点的那一格，点面板时不变
#[gpui_kit::test]
fn split_panes_follow_the_focused_pane(cx: &mut TestAppContext) {
    let (handle, workspace) = open(cx, "processes");
    let left = open_local(cx, handle, &workspace);
    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |w, cx| w.split(Placement::Right, window, cx))
    })
    .unwrap();
    poll_until(cx, "分屏的终端启动", |cx| {
        cx.update(|cx| {
            let terminals = workspace.read(cx).live_terminals();
            terminals.len() == 2 && terminals.iter().all(|p| p.read(cx).is_shown())
        })
    });
    let right = cx.update(|cx| workspace.read(cx).live_terminals().pop().unwrap());
    assert_target(cx, handle, &workspace, right.entity_id(), "新分出的一格");

    // 点左边一格的终端，再点面板
    click_terminal(cx, handle, &left);
    assert_target(cx, handle, &workspace, left.entity_id(), "左边一格");
    click(cx, handle, "host-tools-title");
    assert_target(cx, handle, &workspace, left.entity_id(), "点面板后仍是左边");

    click_terminal(cx, handle, &right);
    click(cx, handle, "host-tools-notice");
    assert_target(cx, handle, &workspace, right.entity_id(), "右边一格");

    // 点另一格的 Tab 标题：那一格的终端获得焦点，面板跟过去；点面板后不变
    click(cx, handle, tab(&left));
    assert_target(cx, handle, &workspace, left.entity_id(), "点左边一格的 Tab");
    cx.update_window(handle, |_, window, cx| {
        let focus = left.read(cx).focus_handle(cx);
        assert!(focus.contains_focused(window, cx), "焦点在左边的终端里");
    })
    .unwrap();
    click(cx, handle, "host-tools-title");
    assert_target(cx, handle, &workspace, left.entity_id(), "再点面板");
    click(cx, handle, tab(&right));
    assert_target(
        cx,
        handle,
        &workspace,
        right.entity_id(),
        "点右边一格的 Tab",
    );
}

/// 打开连测试容器的 SSH 主机，等终端出现提示符（第一次连接时确认指纹）；返回它的终端。
pub(super) fn connect(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    name: &str,
    (address, port): (String, u16),
) -> Entity<TerminalPanel> {
    use termlet_core::model::{Host, SecretField};
    use termlet_core::secret::SecretString;
    use termlet_core::store::{Owner, Position};
    let host = cx.update(|cx| {
        let store = AppState::store(cx);
        let id = store
            .create(
                Host {
                    name: name.into(),
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
                &Owner::host(&id),
                SecretField::Password,
                Some(&SecretString::new("termlet-e2e")),
            )
            .unwrap();
        id
    });
    let before = cx.update(|cx| workspace.read(cx).live_terminals().len());
    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |w, cx| w.open_host(&host, window, cx));
    })
    .unwrap();
    wait_until(cx, handle, "终端连上", |window, cx| {
        if window.try_find("prompt-ok").is_some() {
            window.click("prompt-ok", cx);
        }
        let terminals = workspace.read(cx).live_terminals();
        terminals.len() > before
            && terminals
                .last()
                .is_some_and(|p| p.read(cx).screen_text(cx).contains('$'))
    });
    cx.update(|cx| workspace.read(cx).live_terminals().pop().unwrap())
}

// 真实的服务器：两个 SSH 终端在同一组 Tab 中，点了显示着的终端后点进程页里的行，面板仍跟随它
#[gpui_kit::test]
fn clicking_a_process_row_keeps_the_target_on_a_real_host(cx: &mut TestAppContext) {
    let Some(server) = test_server() else {
        eprintln!("没有设置 TERMLET_TEST_SSH，跳过");
        return;
    };
    let (handle, workspace) = open(cx, "processes");
    let first = connect(cx, handle, &workspace, "first", server.clone());
    let second = connect(cx, handle, &workspace, "second", server);
    assert_target(cx, handle, &workspace, second.entity_id(), "新连上的终端");

    click(cx, handle, tab(&first));
    click(cx, handle, "terminal");
    assert_target(cx, handle, &workspace, first.entity_id(), "切到第一个");
    wait_until(cx, handle, "进程列表", |window, _| {
        window.try_find(("tools-process-row", 1usize)).is_some()
    });
    click(cx, handle, ("tools-process-row", 1usize));
    assert_target(cx, handle, &workspace, first.entity_id(), "点了进程行");
    // 面板仍在显示第一个终端的进程（选中的行还在）
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("tools-process-detail").is_some());
    })
    .unwrap();
}

/// 主机工具的“查看日志”在中间区域打开日志查看器的 Tab：同一个终端的同一个来源再打开时切过去，不另开；命令面板的
/// 已打开的 Tab 中有它，选了切过去；Tab 上的关闭按钮关掉它。
#[gpui_kit::test]
fn logs_open_in_a_center_tab_once(cx: &mut TestAppContext) {
    use termlet_core::hosttools::LogSource;

    use crate::host_tools::{HostToolsEvent, LogTab};

    let (handle, workspace) = open(cx, "services");
    let terminal = open_local(cx, handle, &workspace);
    let tab = LogTab {
        terminal: terminal.downgrade(),
        terminal_id: terminal.entity_id(),
        host: "local".into(),
        name: "demo-app".into(),
        source: LogSource::Service {
            unit: "demo-app.service".into(),
        },
    };
    for _ in 0..2 {
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| {
                w.on_host_tools_event(&HostToolsEvent::OpenLogs(tab.clone()), window, cx)
            });
            window.render_frame(cx);
        })
        .unwrap();
    }
    cx.run_until_parked();
    let logs = cx.update(|cx| workspace.read(cx).live_logs());
    assert_eq!(logs.len(), 1, "同一个来源只开一个 Tab");
    let id = logs[0].entity_id().as_u64() as usize;

    // 切回终端，再从命令面板（只搜已打开的 Tab）选它
    crate::i18n::set_language(crate::i18n::Language::ZhCn);
    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |w, cx| w.show_terminal(&terminal, window, cx));
    })
    .unwrap();
    cx.run_until_parked();
    assert!(!cx.read(|cx| logs[0].read(cx).is_active()));
    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |w, cx| w.open_command_palette(window, cx));
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.input("@demo", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.press("enter", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(
        cx.read(|cx| logs[0].read(cx).is_active()),
        "选了以后切到日志 Tab"
    );
    // 日志查看器中按 Esc 不关 Tab（编辑器、搜索框自己用不到 Esc 时也没有别的动作接住它）
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        logs[0].read(cx).focus_handle(cx).focus(window, cx);
        window.press("escape", cx);
        window.press("escape", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| workspace.read(cx).live_logs().len()), 1);
    assert!(cx.read(|cx| logs[0].read(cx).is_active()), "Esc 不关 Tab");
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find(("log-tab-title", id)).is_some());
        window.click(("log-tab-close", id), cx);
    })
    .unwrap();
    cx.run_until_parked();
    let in_dock = cx.update(|cx| {
        let w = workspace.read(cx);
        let panel = gpui_kit::component::dock::PanelId::from(logs[0].entity_id());
        w.dock_area
            .read(cx)
            .layout(gpui_kit::component::dock::DockPlacement::Center)
            .and_then(|tree| tree.find_panel_node(panel))
            .is_some()
    });
    assert!(!in_dock, "关闭后不在中间区域");
    drop(logs);
    wait_until(cx, handle, "日志 Tab 关闭", |_, cx| {
        workspace.read(cx).live_logs().is_empty()
    });
}

/// “打开主机工具的概览”（Ctrl+Shift+O）：面板收着时展开、切到概览页，焦点进面板；面板中 Esc 把焦点还给终端，
/// 面板照常展开、跟随的终端不变。
#[gpui_kit::test]
fn overview_shortcut_opens_the_panel_and_esc_returns_to_the_terminal(cx: &mut TestAppContext) {
    let (handle, workspace) = open(cx, "network");
    cx.update(|cx| {
        AppState::ui(cx).update(cx, |ui, cx| ui.update(|s| s.host_tools_open = false, cx))
    });
    let terminal = open_local(cx, handle, &workspace);
    click_terminal(cx, handle, &terminal);
    let terminal_focused = |cx: &mut TestAppContext| {
        cx.update_window(handle, |_, window, cx| {
            terminal
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx)
        })
        .unwrap()
    };
    assert!(terminal_focused(cx));
    cx.update_window(handle, |_, window, cx| window.press("ctrl-shift-o", cx))
        .unwrap();
    cx.run_until_parked();
    let (open, page) = cx.update(|cx| {
        let state = AppState::global(cx).ui.read(cx).state();
        (state.host_tools_open, state.host_tools_page.clone())
    });
    assert!(open, "面板展开");
    assert_eq!(page.as_deref(), Some("overview"));
    assert!(!terminal_focused(cx), "焦点进面板");
    cx.update_window(handle, |_, window, cx| window.press("escape", cx))
        .unwrap();
    cx.run_until_parked();
    assert!(terminal_focused(cx), "Esc 把焦点还给终端");
    assert!(cx.update(|cx| AppState::global(cx).ui.read(cx).state().host_tools_open));
    assert_target(
        cx,
        handle,
        &workspace,
        terminal.entity_id(),
        "跟随的终端不变",
    );
}

/// “在终端中执行”的新 Tab：命令结束时通知（这台服务器没有 shell 集成：输出停下、不在等密码时；不出声的命令结束、
/// shell 打出提示符后再停下时再通知），Tab 关掉时还在等的也通知；主机工具据此重读发起操作的页。
#[gpui_kit::test]
fn run_in_terminal_notifies_when_the_command_ends(cx: &mut TestAppContext) {
    use std::cell::Cell;
    use std::rc::Rc;

    let Some(server) = test_server() else {
        eprintln!("没有设置 TERMLET_TEST_SSH，跳过");
        return;
    };
    let (handle, workspace) = open(cx, "processes");
    let source = connect(cx, handle, &workspace, "source", server);
    let run = |cx: &mut TestAppContext, command: &str| {
        let count = Rc::new(Cell::new(0));
        let counter = count.clone();
        let command = command.to_string();
        let panel = cx
            .update_window(handle, |_, window, cx| {
                let panel = source.update(cx, |p, cx| {
                    p.duplicate_running(command, "run".into(), window, cx)
                });
                panel.update(cx, |p, cx| {
                    p.on_run_finished(move |_| counter.set(counter.get() + 1), cx)
                });
                workspace.update(cx, |w, cx| w.add_terminal(panel.clone(), window, cx));
                panel
            })
            .unwrap();
        (panel, count)
    };
    let screen = |cx: &mut TestAppContext, panel: &Entity<TerminalPanel>| {
        cx.update(|cx| panel.read(cx).screen_text(cx))
    };
    // 发命令、看输出停没停的计时器按测试的时钟走：等的时候一起推进
    let wait = |cx: &mut TestAppContext,
                what: &str,
                mut ready: Box<dyn FnMut(&mut TestAppContext) -> bool>| {
        poll_until(cx, what, |cx| {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(100));
            ready(cx)
        })
    };

    // 很快结束的命令：输出停下后通知一次
    let (quick, done) = run(cx, "echo run-done-$((6 * 7))");
    let counted = done.clone();
    wait(cx, "命令结束后通知", Box::new(move |_| counted.get() == 1));
    assert!(screen(cx, &quick).contains("run-done-42"));
    let started = std::time::Instant::now();
    wait(
        cx,
        "等一会儿",
        Box::new(move |_| started.elapsed() >= std::time::Duration::from_secs(4)),
    );
    assert_eq!(done.get(), 1, "之后没有输出，不再通知");

    // 不出声的命令：停下时先通知一次，结束后 shell 打出提示符、再停下时再通知
    let (slow, slept) = run(cx, "sleep 6; echo run-slept");
    let slow_screen = slow.clone();
    wait(
        cx,
        "结束后再通知",
        Box::new(move |cx| {
            slept.get() >= 2
                && cx.update(|cx| slow_screen.read(cx).screen_text(cx).contains("run-slept"))
        }),
    );

    // 关掉 Tab：还在等的也通知
    let (long, closed) = run(cx, "sleep 600");
    let quiet = closed.clone();
    wait(cx, "输出停下", Box::new(move |_| quiet.get() == 1));
    cx.update(|cx| {
        long.update(cx, |_, cx| {
            cx.emit(crate::terminal_panel::TerminalPanelEvent::Close)
        })
    });
    drop(long);
    cx.run_until_parked();
    poll_until(cx, "Tab 关掉时通知", |_| closed.get() == 2);
}
