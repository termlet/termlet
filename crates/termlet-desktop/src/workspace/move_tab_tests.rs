//! Tab 移到别的窗口（[`super::move_tab`]）的测试：Tab 菜单的“移到新窗口”“移到窗口 ›”，拖到窗口外放下（空处新开窗口、
//! 另一个窗口上移过去），窗口里唯一的 Tab，连接中不能移；移过去的终端会话不断、回滚还在、输入照常、原来的窗口关掉后
//! 照样运行，回放接着播放，日志查看器照常显示，SFTP 移走后“在终端中查找”切到终端所在的窗口；连真实 sshd 的（`TERMLET_TEST_SSH`）：SSH 终端的持续输出不断，文件管理的
//! 传输在原来的窗口关掉后照样完成，编辑器的未保存修改保留、移过去后照样能编辑，共用终端连接的端口转发、编辑器的
//! 会话跟着 Tab 走（原来的窗口还在用时留下，关掉时交出去）。

use std::time::Duration;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::dock::PanelId;
use gpui_kit::test::{TestEventExt as _, TestWindowExt as _};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, ElementId, Entity, Focusable as _, MouseButton,
    Pixels, Point, TestAppContext, WindowBounds, WindowOptions, point, px, size,
};

use super::move_tab::{MoveTarget, open_workspaces};
use super::{Workspace, collect_tab_groups};
use crate::app_state::AppState;
use crate::i18n::{self, Language};
use crate::t;
use crate::terminal_panel::{SessionStatus, TerminalPanel};
use crate::testkit::{poll_until, test_server, wait_until};

fn setup(cx: &mut TestAppContext) {
    i18n::set_language(Language::ZhCn);
    cx.executor().allow_parking();
    cx.update(gpui_kit::init);
    cx.update(crate::init);
    cx.update(AppState::install_in_memory);
    cx.update(|cx| {
        AppState::store(cx)
            .update_settings(|s| s.general.confirm_close = false)
            .unwrap()
    });
    cx.update(|cx| cx.set_reduce_motion(true));
}

/// 在屏幕的 `origin` 处打开一个 `width` × `height` 的主窗口。
fn open_at(
    cx: &mut TestAppContext,
    origin: (f32, f32),
    (width, height): (f32, f32),
) -> (AnyWindowHandle, Entity<Workspace>) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
            point(px(origin.0), px(origin.1)),
            size(px(width), px(height)),
        ))),
        ..Default::default()
    };
    let opened = cx
        .update(|cx| {
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| Workspace::new(window, cx))
            })
        })
        .unwrap();
    cx.run_until_parked();
    opened
}

/// 新开一个本地终端，等它启动。
fn open_local(
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

/// 在终端中输入一行并按回车。
fn type_line(cx: &mut TestAppContext, panel: &Entity<TerminalPanel>, line: &str) {
    let text = format!("{line}\r");
    cx.update(|cx| panel.update(cx, |p, cx| p.send_text(&text, cx)));
}

/// 等到屏幕与回滚中 `text` 至少出现 `times` 次（命令的回显与输出各一次）。
fn wait_output(cx: &mut TestAppContext, panel: &Entity<TerminalPanel>, text: &str, times: usize) {
    poll_until(cx, &format!("终端输出 {text}"), |cx| {
        cx.update(|cx| panel.update(cx, |p, cx| p.count_in_scrollback(text, cx)) >= times)
    });
}

fn title(panel: &Entity<TerminalPanel>) -> ElementId {
    ("tab-title", panel.entity_id().as_u64() as usize).into()
}

fn terminal_ids(cx: &mut TestAppContext, workspace: &Entity<Workspace>) -> Vec<gpui_kit::EntityId> {
    cx.update(|cx| workspace.read(cx).terminal_ids())
}

/// 窗口中各窗格的 Tab。
fn groups(cx: &mut TestAppContext, workspace: &Entity<Workspace>) -> Vec<Vec<PanelId>> {
    cx.update(|cx| {
        let dock = workspace.read(cx).dock_area.read(cx);
        let mut groups = Vec::new();
        if let Some(tree) = dock.layout(gpui_kit::component::dock::DockPlacement::Center) {
            collect_tab_groups(tree.root(), &mut groups);
        }
        groups.into_iter().map(|(panels, _)| panels).collect()
    })
}

/// 终端的 Tab 菜单中“分屏与窗口 ›”里的全部文案（移到窗口的几项在这里）。
fn menu_labels(cx: &mut TestAppContext, handle: AnyWindowHandle, title: ElementId) -> Vec<String> {
    let layout = t!("panel.menu.layout");
    crate::testkit::menu_labels_in(cx, handle, title, Some(&layout))
}

/// 右键 Tab 标题，点“分屏与窗口 ›”中的 `label`；返回这一项是否可用（不可用的点了没有反应，再按 Esc 关掉菜单）。
fn click_menu(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    title: ElementId,
    label: &str,
) -> bool {
    let layout = t!("panel.menu.layout");
    crate::testkit::click_menu_in(cx, handle, title, Some(&layout), label)
}

/// “移到“窗口名””一项的开头（各窗口一项，后面是窗口名）。
fn move_to_named_prefix() -> String {
    let label = t!("tab_window.move_to_named", window = "\u{1}").to_string();
    label.split('\u{1}').next().unwrap_or_default().to_string()
}

/// 除 `except` 以外唯一的另一个主窗口。
fn other_window(
    cx: &mut TestAppContext,
    except: AnyWindowHandle,
) -> (AnyWindowHandle, Entity<Workspace>) {
    let others: Vec<_> = cx.update(|cx| {
        open_workspaces(cx)
            .into_iter()
            .filter(|(h, _)| *h != except)
            .collect()
    });
    assert_eq!(others.len(), 1, "应有另一个窗口");
    others.into_iter().next().unwrap()
}

/// 元素中心在窗口中的位置。
fn center_of(cx: &mut TestAppContext, handle: AnyWindowHandle, id: ElementId) -> Point<Pixels> {
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.find(id).bounds().center()
    })
    .unwrap()
}

fn close_window(cx: &mut TestAppContext, handle: AnyWindowHandle) {
    cx.update_window(handle, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
}

// 终端移到新窗口（Tab 菜单）：同一个面板实体移过去，会话不断、回滚还在；原来的窗口留着别的 Tab。原来的窗口关掉后
// 输入照常、输出照常显示，shell 退出时会话结束（订阅换到了新窗口）
#[gpui_kit::test]
fn terminal_moved_to_a_new_window_keeps_its_session(cx: &mut TestAppContext) {
    setup(cx);
    let (a, ws_a) = open_at(cx, (0., 0.), (1024., 640.));
    let first = open_local(cx, a, &ws_a);
    let second = open_local(cx, a, &ws_a);
    type_line(cx, &first, "echo tabwin-before");
    wait_output(cx, &first, "tabwin-before", 2);

    let labels = menu_labels(cx, a, title(&first));
    assert!(labels.contains(&t!("tab_window.move_to_new").to_string()));
    assert!(
        !labels
            .iter()
            .any(|l| l.starts_with(&move_to_named_prefix())),
        "没有别的窗口时没有移到别的窗口的项：{labels:?}"
    );
    assert!(click_menu(
        cx,
        a,
        title(&first),
        &t!("tab_window.move_to_new")
    ));
    let (b, ws_b) = other_window(cx, a);
    assert_eq!(terminal_ids(cx, &ws_a), [second.entity_id()]);
    assert_eq!(
        terminal_ids(cx, &ws_b),
        [first.entity_id()],
        "同一个终端移过去"
    );
    assert!(cx.update(|cx| first.read(cx).is_live(cx)), "会话没有断开");
    assert!(
        cx.update(|cx| first.update(cx, |p, cx| p.count_in_scrollback("tabwin-before", cx))) >= 2,
        "回滚还在"
    );
    cx.update_window(b, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find(title(&first)).is_some(), "在新窗口中画出");
    })
    .unwrap();
    // 面板自己拿到焦点时转交给终端：焦点的监听登记在新窗口中
    cx.update_window(b, |_, window, cx| {
        first.read(cx).root_focus().focus(window, cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(b, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            first.read(cx).focus_handle(cx).is_focused(window),
            "焦点转交给终端视图"
        );
    })
    .unwrap();
    // 新窗口与原来的窗口一样大，错开放
    let bounds = cx.update_window(b, |_, window, _| window.bounds()).unwrap();
    assert_eq!(bounds.size, size(px(1024.), px(640.)));
    assert_eq!(bounds.origin, point(px(32.), px(32.)));

    close_window(cx, a);
    type_line(cx, &first, "echo tabwin-after");
    wait_output(cx, &first, "tabwin-after", 2);
    type_line(cx, &first, "exit");
    poll_until(cx, "shell 退出后会话结束", |cx| {
        cx.update(|cx| first.read(cx).status() == SessionStatus::Disconnected)
    });
}

// 移到已经打开的窗口（“分屏与窗口 ›”列出它）：移走最后一个 Tab 的窗口留着、显示开始页；放进去的窗口切到它
#[gpui_kit::test]
fn moving_the_last_tab_to_another_window_leaves_the_start_page(cx: &mut TestAppContext) {
    setup(cx);
    let (a, ws_a) = open_at(cx, (0., 0.), (1024., 640.));
    let (b, ws_b) = open_at(cx, (1100., 0.), (800., 600.));
    let first = open_local(cx, a, &ws_a);
    let other = open_local(cx, b, &ws_b);
    let labels = menu_labels(cx, a, title(&first));
    let window_b = cx.update(|cx| ws_b.read(cx).window_label(cx));
    assert!(
        labels.contains(&t!("tab_window.move_to_named", window = window_b).to_string()),
        "有别的窗口时列出它：{labels:?}"
    );
    // 窗口里只有这一个 Tab：“移到新窗口”不可用
    assert!(!click_menu(
        cx,
        a,
        title(&first),
        &t!("tab_window.move_to_new")
    ));
    assert_eq!(cx.update(|cx| open_workspaces(cx).len()), 2);
    let label = cx.update(|cx| ws_b.read(cx).window_label(cx));
    assert_eq!(label, cx.update(|cx| other.read(cx).name().clone()));

    cx.update_window(a, |_, window, cx| {
        ws_a.update(cx, |w, cx| {
            w.move_tab(
                PanelId::from(first.entity_id()),
                MoveTarget::Window(b),
                window,
                cx,
            )
        })
    })
    .unwrap();
    cx.run_until_parked();
    assert!(groups(cx, &ws_a).is_empty(), "原来的窗口没有 Tab 了");
    assert!(cx.update(|cx| cx.windows().contains(&a)), "原来的窗口留着");
    cx.update_window(a, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("start-local-terminal").is_some(),
            "显示开始页"
        );
    })
    .unwrap();
    assert_eq!(
        terminal_ids(cx, &ws_b),
        [other.entity_id(), first.entity_id()]
    );
    cx.update_window(b, |_, window, cx| {
        let focused = ws_b.read(cx).focused_terminal(window, cx);
        assert_eq!(
            focused.map(|p| p.entity_id()),
            Some(first.entity_id()),
            "切到移过来的"
        );
    })
    .unwrap();
    let name = cx.update(|cx| first.read(cx).name().clone());
    assert_eq!(
        cx.update(|cx| ws_b.read(cx).window_label(cx)),
        t!("tab_window.window_many", name = name, count = 2)
    );
    type_line(cx, &first, "echo tabwin-moved");
    wait_output(cx, &first, "tabwin-moved", 2);
}

// 连接中不能移：提示原因，Tab 留在原来的窗口；命令面板的“把 Tab 移到新窗口”对着当前 Tab，只有一个 Tab 时提示
#[gpui_kit::test]
fn a_connecting_terminal_stays_and_explains(cx: &mut TestAppContext) {
    setup(cx);
    // 接受连接、从不回应的“服务器”：SSH 一直停在连接中
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = silent.local_addr().unwrap().port();
    let (a, ws_a) = open_at(cx, (0., 0.), (1024., 640.));
    let local = open_local(cx, a, &ws_a);
    let target = termlet_core::quick::parse(&format!("alice@127.0.0.1:{port}")).unwrap();
    cx.update_window(a, |_, window, cx| {
        ws_a.update(cx, |w, cx| w.open_quick(target, window, cx))
    })
    .unwrap();
    cx.run_until_parked();
    let connecting = cx.update(|cx| ws_a.read(cx).live_terminals().pop().unwrap());
    assert_eq!(
        cx.update(|cx| connecting.read(cx).status()),
        SessionStatus::Connecting
    );
    assert!(click_menu(
        cx,
        a,
        title(&connecting),
        &t!("tab_window.move_to_new")
    ));
    assert_eq!(cx.update(|cx| open_workspaces(cx).len()), 1, "没有新开窗口");
    assert_eq!(
        terminal_ids(cx, &ws_a),
        [local.entity_id(), connecting.entity_id()]
    );
    cx.update_window(a, |_, window, cx| {
        assert!(!window.notifications(cx).is_empty(), "提示不能移的原因");
    })
    .unwrap();

    // 快捷键 / 命令面板：当前 Tab（焦点所在的本地终端）移到新窗口
    cx.update_window(a, |_, window, cx| {
        ws_a.update(cx, |w, cx| w.show_terminal(&local, window, cx));
    })
    .unwrap();
    cx.update_window(a, |_, window, cx| {
        window.render_frame(cx);
        window.dispatch_action(Box::new(super::MoveTabToNewWindow), cx);
    })
    .unwrap();
    cx.run_until_parked();
    let (_, ws_b) = other_window(cx, a);
    assert_eq!(terminal_ids(cx, &ws_b), [local.entity_id()]);
    // 新窗口里只有这一个 Tab：再按不移
    let b = cx.update(|cx| {
        open_workspaces(cx)
            .into_iter()
            .find(|(_, w)| w == &ws_b)
            .unwrap()
            .0
    });
    cx.update_window(b, |_, window, cx| {
        ws_b.update(cx, |w, cx| w.show_terminal(&local, window, cx));
    })
    .unwrap();
    cx.update_window(b, |_, window, cx| {
        window.render_frame(cx);
        window.dispatch_action(Box::new(super::MoveTabToNewWindow), cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| open_workspaces(cx).len()), 2);
    drop(silent);
}

/// 从 `from`（窗口中的位置）按下鼠标，经几步移到 `to`（可以在窗口外），不松开。
fn press_and_move(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    from: Point<Pixels>,
    to: Point<Pixels>,
) {
    cx.update_window(handle, |_, window, cx| {
        window.pointer_move(from, None, cx);
        window.pointer_down(from, MouseButton::Left, cx);
    })
    .unwrap();
    press_and_move_on(cx, handle, from, to);
}

/// 按住左键从 `from` 移到 `to`。
fn press_and_move_on(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    from: Point<Pixels>,
    to: Point<Pixels>,
) {
    cx.update_window(handle, |_, window, cx| {
        for step in 1..=8 {
            let f = step as f32 / 8.;
            let at = point(from.x + (to.x - from.x) * f, from.y + (to.y - from.y) * f);
            window.pointer_move(at, Some(MouseButton::Left), cx);
        }
    })
    .unwrap();
    cx.run_until_parked();
}

fn release(cx: &mut TestAppContext, handle: AnyWindowHandle, at: Point<Pixels>) {
    cx.update_window(handle, |_, window, cx| {
        window.pointer_up(at, MouseButton::Left, cx)
    })
    .unwrap();
    cx.run_until_parked();
}

fn hint_shown(cx: &mut TestAppContext, handle: AnyWindowHandle) -> bool {
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.try_find("tab-drag-out-hint").is_some()
    })
    .unwrap()
}

// 把 Tab 拖到窗口外的空处：拖出窗口时提示，松开后在放下的位置新开一个窗口；拖回窗口里时提示消失、不移
#[gpui_kit::test]
fn dragging_a_tab_out_opens_a_new_window_where_it_is_dropped(cx: &mut TestAppContext) {
    setup(cx);
    let (a, ws_a) = open_at(cx, (300., 200.), (1024., 640.));
    let first = open_local(cx, a, &ws_a);
    let second = open_local(cx, a, &ws_a);
    let from = center_of(cx, a, title(&first));

    // 拖出去又拖回来：不移
    press_and_move(cx, a, from, point(px(-80.), px(150.)));
    assert!(hint_shown(cx, a), "拖到窗口外时提示");
    press_and_move_on(cx, a, point(px(-80.), px(150.)), point(px(500.), px(300.)));
    assert!(!hint_shown(cx, a), "回到窗口里时不提示");
    release(cx, a, point(px(500.), px(300.)));
    assert_eq!(cx.update(|cx| open_workspaces(cx).len()), 1);

    // 拖出去松开：在放下的位置新开一个窗口
    let drop_at = point(px(-80.), px(150.));
    press_and_move(cx, a, from, drop_at);
    release(cx, a, drop_at);
    assert!(!hint_shown(cx, a));
    let (b, ws_b) = other_window(cx, a);
    assert_eq!(terminal_ids(cx, &ws_b), [first.entity_id()]);
    assert_eq!(terminal_ids(cx, &ws_a), [second.entity_id()]);
    let bounds = cx.update_window(b, |_, window, _| window.bounds()).unwrap();
    let screen = point(px(300. - 80.), px(200. + 150.));
    assert!(
        bounds.contains(&screen),
        "新窗口在放下的位置：{bounds:?} 不含 {screen:?}"
    );
    assert!(cx.update(|cx| first.read(cx).is_live(cx)));
}

// 把 Tab 拖到另一个 Termlet 窗口上：提示移到那个窗口，松开后移过去；窗口里唯一的 Tab 也能这样移，拖到空处则不移
#[gpui_kit::test]
fn dragging_a_tab_onto_another_window_moves_it_there(cx: &mut TestAppContext) {
    setup(cx);
    let (a, ws_a) = open_at(cx, (0., 0.), (1024., 640.));
    let (b, ws_b) = open_at(cx, (1100., 0.), (800., 600.));
    let first = open_local(cx, a, &ws_a);
    let second = open_local(cx, a, &ws_a);
    let on_b = point(px(1300.), px(200.));
    let from = center_of(cx, a, title(&first));
    press_and_move(cx, a, from, on_b);
    let hint = cx
        .update_window(a, |_, window, cx| {
            window.render_frame(cx);
            window.find("tab-drag-out-hint").bounds()
        })
        .unwrap();
    assert!(hint.origin.x < px(1024.), "提示在窗口里：{hint:?}");
    release(cx, a, on_b);
    assert_eq!(cx.update(|cx| open_workspaces(cx).len()), 2, "没有新开窗口");
    assert_eq!(terminal_ids(cx, &ws_b), [first.entity_id()]);
    assert_eq!(terminal_ids(cx, &ws_a), [second.entity_id()]);

    // 窗口里唯一的 Tab：拖到空处不移，拖到另一个窗口上移过去
    let from = center_of(cx, a, title(&second));
    let empty = point(px(400.), px(900.));
    press_and_move(cx, a, from, empty);
    assert!(hint_shown(cx, a), "提示只能拖到另一个窗口上");
    release(cx, a, empty);
    assert_eq!(cx.update(|cx| open_workspaces(cx).len()), 2);
    assert_eq!(terminal_ids(cx, &ws_a), [second.entity_id()]);
    press_and_move(cx, a, from, on_b);
    release(cx, a, on_b);
    assert!(terminal_ids(cx, &ws_a).is_empty());
    assert_eq!(
        terminal_ids(cx, &ws_b),
        [first.entity_id(), second.entity_id()]
    );
    cx.update_window(b, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find(title(&second)).is_some());
    })
    .unwrap();
}

// 回放移到新窗口：位置不变、接着播放（原来的窗口关掉后照样推进）；日志查看器移过去照常显示
#[gpui_kit::test]
fn replays_and_logs_move_too(cx: &mut TestAppContext) {
    use termlet_core::hosttools::LogSource;

    use crate::host_tools::{HostToolsEvent, LogTab};

    setup(cx);
    let (a, ws_a) = open_at(cx, (0., 0.), (1024., 640.));
    let terminal = open_local(cx, a, &ws_a);
    let dir = cx.update(|cx| AppState::global(cx).data_dir.root().join("tabwin-test"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tabwin.cast");
    std::fs::write(
        &path,
        concat!(
            "{\"version\": 2, \"width\": 40, \"height\": 5}\n",
            "[0.0, \"o\", \"start\\r\\n\"]\n",
            "[60.0, \"o\", \"end\\r\\n\"]\n",
        ),
    )
    .unwrap();
    cx.update_window(a, |_, window, cx| {
        ws_a.update(cx, |w, cx| w.open_replay(path.clone(), window, cx))
    })
    .unwrap();
    poll_until(cx, "回放打开", |cx| {
        cx.update(|cx| !ws_a.read(cx).live_replays().is_empty())
    });
    let replay = cx.update(|cx| ws_a.read(cx).live_replays().pop().unwrap());
    assert!(cx.update(|cx| replay.read(cx).is_playing()));
    cx.update_window(a, |_, window, cx| {
        ws_a.update(cx, |w, cx| {
            w.move_tab(
                PanelId::from(replay.entity_id()),
                MoveTarget::NewWindow { at: None },
                window,
                cx,
            )
        })
    })
    .unwrap();
    cx.run_until_parked();
    let (b, ws_b) = other_window(cx, a);
    assert_eq!(
        cx.update(|cx| ws_b.read(cx).live_replays())
            .iter()
            .map(|p| p.entity_id())
            .collect::<Vec<_>>(),
        [replay.entity_id()]
    );
    close_window(cx, a);
    let before = cx.update(|cx| replay.read(cx).position());
    std::thread::sleep(Duration::from_millis(300));
    cx.executor().advance_clock(Duration::from_millis(100));
    cx.run_until_parked();
    let after = cx.update(|cx| replay.read(cx).position());
    assert!(
        after > before,
        "原来的窗口关掉后接着播放：{before} → {after}"
    );
    assert!(cx.update(|cx| replay.read(cx).is_playing()));

    // 日志查看器（本地终端没有日志：显示原因）：移到新窗口照常显示
    let tab = LogTab {
        terminal: terminal.downgrade(),
        terminal_id: terminal.entity_id(),
        host: "local".into(),
        name: "demo".into(),
        source: LogSource::Service {
            unit: "demo.service".into(),
        },
    };
    cx.update_window(b, |_, window, cx| {
        ws_b.update(cx, |w, cx| {
            w.on_host_tools_event(&HostToolsEvent::OpenLogs(tab), window, cx)
        })
    })
    .unwrap();
    cx.run_until_parked();
    let logs = cx.update(|cx| ws_b.read(cx).live_logs().pop().unwrap());
    cx.update_window(b, |_, window, cx| {
        ws_b.update(cx, |w, cx| {
            w.move_tab(
                PanelId::from(logs.entity_id()),
                MoveTarget::NewWindow { at: None },
                window,
                cx,
            )
        })
    })
    .unwrap();
    cx.run_until_parked();
    let (c, ws_c) = cx.update(|cx| {
        open_workspaces(cx)
            .into_iter()
            .find(|(h, _)| *h != b)
            .unwrap()
    });
    assert_eq!(
        cx.update(|cx| ws_c.read(cx).live_logs())
            .iter()
            .map(|p| p.entity_id())
            .collect::<Vec<_>>(),
        [logs.entity_id()]
    );
    let logs_title: ElementId = ("log-tab-title", logs.entity_id().as_u64() as usize).into();
    wait_until(
        cx,
        c,
        "日志查看器在新窗口中显示",
        |window, _| window.try_find(logs_title.clone()).is_some(),
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- 连真实 sshd

/// 测试容器上的主机（保存了密码）。
fn add_test_host(
    cx: &mut TestAppContext,
    (address, port): (String, u16),
) -> termlet_core::id::HostId {
    use termlet_core::model::{Host, SecretField};
    use termlet_core::secret::SecretString;
    use termlet_core::store::{Owner, Position};
    cx.update(|cx| {
        let store = AppState::store(cx);
        let id = store
            .create(
                Host {
                    name: "tabwin".into(),
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
    })
}

/// 等 `ready`，期间出现的询问（主机指纹）都确认。
fn wait_accepting(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    what: &str,
    mut ready: impl FnMut(&mut App) -> bool,
) {
    wait_until(cx, handle, what, |window, cx| {
        if window.try_find("prompt-ok").is_some() {
            window.click("prompt-ok", cx);
        }
        ready(cx)
    });
}

// SSH 终端在持续输出时移到新窗口：连接不断、回滚还在；原来的窗口关掉后输出接着来，Ctrl+C 与新的命令照常
#[gpui_kit::test]
fn ssh_terminal_keeps_streaming_after_moving(cx: &mut TestAppContext) {
    let Some(server) = test_server() else {
        eprintln!("没有设置 TERMLET_TEST_SSH，跳过");
        return;
    };
    setup(cx);
    let host = add_test_host(cx, server);
    let (a, ws_a) = open_at(cx, (0., 0.), (1024., 640.));
    let _local = open_local(cx, a, &ws_a);
    cx.update_window(a, |_, window, cx| {
        ws_a.update(cx, |w, cx| w.open_host(&host, window, cx))
    })
    .unwrap();
    let ssh = cx.update(|cx| ws_a.read(cx).live_terminals().pop().unwrap());
    wait_accepting(cx, a, "SSH 终端连上", |cx| ssh.read(cx).is_live(cx));
    type_line(
        cx,
        &ssh,
        "for i in $(seq 1 300); do echo tabwin-line-$i; done",
    );
    wait_output(cx, &ssh, "tabwin-line-300", 1);
    type_line(
        cx,
        &ssh,
        "while true; do echo tabwin-tick-$(date +%s%N); sleep 0.1; done",
    );
    wait_output(cx, &ssh, "tabwin-tick-1", 3);
    cx.update_window(a, |_, window, cx| {
        ws_a.update(cx, |w, cx| {
            w.move_tab(
                PanelId::from(ssh.entity_id()),
                MoveTarget::NewWindow { at: None },
                window,
                cx,
            )
        })
    })
    .unwrap();
    cx.run_until_parked();
    let (_, ws_b) = other_window(cx, a);
    assert_eq!(terminal_ids(cx, &ws_b), [ssh.entity_id()]);
    close_window(cx, a);
    assert!(
        cx.update(|cx| ssh.update(cx, |p, cx| p.count_in_scrollback("tabwin-line-1", cx))) >= 1,
        "回滚还在"
    );
    let ticks = cx.update(|cx| ssh.update(cx, |p, cx| p.count_in_scrollback("tabwin-tick-", cx)));
    wait_output(cx, &ssh, "tabwin-tick-", ticks + 10);
    type_line(cx, &ssh, "\u{3}");
    type_line(cx, &ssh, "echo tabwin-$((40 + 2))");
    wait_output(cx, &ssh, "tabwin-42", 1);
    assert!(cx.update(|cx| ssh.read(cx).is_live(cx)));
}

// 文件管理在上传中移到新窗口：会话与传输改属新窗口，原来的窗口关掉后传输照样完成
#[gpui_kit::test]
fn sftp_transfer_survives_moving_and_closing_the_old_window(cx: &mut TestAppContext) {
    use termlet_core::sftp::{TransferState, UploadRequest};

    let Some(server) = test_server() else {
        eprintln!("没有设置 TERMLET_TEST_SSH，跳过");
        return;
    };
    setup(cx);
    let host = add_test_host(cx, server);
    let (a, ws_a) = open_at(cx, (0., 0.), (1024., 640.));
    let _local = open_local(cx, a, &ws_a);
    cx.update_window(a, |_, window, cx| {
        ws_a.update(cx, |w, cx| w.open_host_sftp(&host, window, cx))
    })
    .unwrap();
    let sftp = cx.update(|cx| ws_a.read(cx).live_sftps().pop().unwrap());
    wait_accepting(cx, a, "文件管理连上", |cx| {
        sftp.read(cx).current_lease().is_some()
    });
    let session = cx.update(|cx| sftp.read(cx).current_lease().unwrap().session().clone());
    let manager = cx.update(|cx| AppState::sftp(cx));
    let local = std::env::temp_dir().join(format!(
        "termlet-tabwin-{}.bin",
        termlet_core::id::random_token(8)
    ));
    std::fs::write(&local, vec![7u8; 128 << 20]).unwrap();
    let request = UploadRequest {
        local_paths: vec![local.clone()],
        remote_dir: "/tmp".into(),
        overwrite: true,
    };
    let started = {
        let (manager, session) = (manager.clone(), session.clone());
        termlet_core::runtime::runtime()
            .block_on(async move { manager.upload(&session, request).await })
            .expect("开始上传")
    };
    let id = started[0].id.clone();
    cx.update_window(a, |_, window, cx| {
        ws_a.update(cx, |w, cx| {
            w.move_tab(
                PanelId::from(sftp.entity_id()),
                MoveTarget::NewWindow { at: None },
                window,
                cx,
            )
        })
    })
    .unwrap();
    cx.run_until_parked();
    let state = |cx: &mut TestAppContext| {
        cx.update(|_| {
            manager
                .transfers(None)
                .into_iter()
                .find(|t| t.id == id)
                .map(|t| (t.state, t.owner))
        })
    };
    let (moved_state, _) = state(cx).unwrap();
    assert!(
        moved_state.is_pending(),
        "移动时传输还在进行：{moved_state:?}"
    );
    let (b, ws_b) = other_window(cx, a);
    let owner_b = cx
        .update_window(b, |_, window, _| crate::sftp::panel::window_owner(window))
        .unwrap();
    assert_eq!(
        cx.update(|cx| ws_b.read(cx).live_sftps()).len(),
        1,
        "文件管理在新窗口中"
    );
    assert_eq!(session.owner(), owner_b, "会话改属新窗口");
    // 路径框在新窗口中重建：输入后不提交就离开，恢复为当前目录（靠它的失去焦点事件）
    cx.update_window(b, |_, window, cx| window.render_frame(cx))
        .unwrap();
    let path_input = cx.update(|cx| sftp.read(cx).path_input());
    let dir = cx.update(|cx| path_input.read(cx).value());
    cx.update_window(b, |_, window, cx| {
        path_input.read(cx).focus_handle(cx).focus(window, cx);
        window.render_frame(cx);
        window.input("/tabwin-typed", cx);
    })
    .unwrap();
    assert_ne!(cx.update(|cx| path_input.read(cx).value()), dir);
    cx.update_window(b, |_, window, cx| {
        sftp.read(cx).focus_handle(cx).focus(window, cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        cx.update(|cx| path_input.read(cx).value()),
        dir,
        "离开路径框时恢复"
    );
    close_window(cx, a);
    poll_until(cx, "上传完成", |cx| {
        state(cx).is_some_and(|(s, _)| s.is_finished())
    });
    let (final_state, owner) = state(cx).unwrap();
    assert_eq!(
        final_state,
        TransferState::Done,
        "原来的窗口关掉没有打断传输"
    );
    assert_eq!(owner, owner_b);
    assert!(manager.session(session.id()).is_some(), "会话还开着");
    let name = local.file_name().unwrap().to_string_lossy().to_string();
    let remote = termlet_core::sftp::path::join("/tmp", &name);
    let _ = termlet_core::runtime::runtime().block_on(session.remove(&remote));
    let _ = std::fs::remove_file(&local);
}

// 编辑器移到新窗口：未保存的修改与撤销记录都在，移过去后照样能编辑；原来的窗口关掉后内容还在
#[gpui_kit::test]
fn editor_keeps_unsaved_changes_when_moved(cx: &mut TestAppContext) {
    use termlet_core::sftp::WriteTextRequest;

    let Some(server) = test_server() else {
        eprintln!("没有设置 TERMLET_TEST_SSH，跳过");
        return;
    };
    setup(cx);
    let host = add_test_host(cx, server);
    let (a, ws_a) = open_at(cx, (0., 0.), (1024., 640.));
    cx.update_window(a, |_, window, cx| {
        ws_a.update(cx, |w, cx| w.open_host_sftp(&host, window, cx))
    })
    .unwrap();
    let sftp = cx.update(|cx| ws_a.read(cx).live_sftps().pop().unwrap());
    wait_accepting(cx, a, "文件管理连上", |cx| {
        sftp.read(cx).current_lease().is_some()
    });
    let lease = cx.update(|cx| sftp.read(cx).current_lease().unwrap());
    let path = format!(
        "/tmp/termlet-tabwin-{}.txt",
        termlet_core::id::random_token(8)
    );
    let file = {
        let session = lease.session().clone();
        let path = path.clone();
        termlet_core::runtime::runtime()
            .block_on(async move {
                session
                    .write_text(&WriteTextRequest {
                        path: path.clone(),
                        content: "hello\n".into(),
                        force: true,
                        ..Default::default()
                    })
                    .await?;
                session.read_text(&path, None).await
            })
            .expect("写入、读回测试文件")
    };
    cx.update_window(a, |_, window, cx| {
        ws_a.update(cx, |w, cx| {
            w.open_editor(&sftp, lease.clone(), file, window, cx)
        })
    })
    .unwrap();
    cx.run_until_parked();
    let editor = cx.update(|cx| ws_a.read(cx).live_editors().pop().unwrap());
    cx.update_window(a, |_, window, cx| {
        window.render_frame(cx);
        editor.read(cx).focus_handle(cx).focus(window, cx);
        window.input("edited-", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let text = cx.update(|cx| editor.read(cx).text(cx));
    assert!(text.contains("edited-"), "{text:?}");
    assert!(cx.update(|cx| editor.read(cx).is_dirty(cx)));

    cx.update_window(a, |_, window, cx| {
        ws_a.update(cx, |w, cx| {
            w.move_tab(
                PanelId::from(editor.entity_id()),
                MoveTarget::NewWindow { at: None },
                window,
                cx,
            )
        })
    })
    .unwrap();
    cx.run_until_parked();
    let (b, ws_b) = other_window(cx, a);
    assert_eq!(
        cx.update(|cx| ws_b.read(cx).live_editors())
            .iter()
            .map(|e| e.entity_id())
            .collect::<Vec<_>>(),
        [editor.entity_id()]
    );
    let owner_of = |cx: &mut TestAppContext, handle: AnyWindowHandle| {
        cx.update_window(handle, |_, window, _| {
            crate::sftp::panel::window_owner(window)
        })
        .unwrap()
    };
    let (owner_a, owner_b) = (owner_of(cx, a), owner_of(cx, b));
    assert_eq!(
        lease.session().owner(),
        owner_a,
        "原来的窗口里文件管理还用着会话：仍属原来的窗口"
    );
    assert!(
        cx.update(|cx| editor.read(cx).is_dirty(cx)),
        "未保存的修改还在"
    );
    assert_eq!(cx.update(|cx| editor.read(cx).text(cx)), text);
    // 在新窗口中接着编辑、撤销
    cx.update_window(b, |_, window, cx| {
        window.render_frame(cx);
        editor.read(cx).focus_handle(cx).focus(window, cx);
        window.input("more-", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let edited = cx.update(|cx| editor.read(cx).text(cx));
    assert!(
        edited.contains("more-") && edited.contains("edited-"),
        "{edited:?}"
    );
    // 窗口关闭时工作区随之释放（测试别拿着它）
    drop(ws_a);
    close_window(cx, a);
    assert!(cx.update(|cx| editor.read(cx).is_dirty(cx)));
    assert_eq!(cx.update(|cx| editor.read(cx).text(cx)), edited);
    // 原来的窗口关掉：会话交给编辑器所在的窗口，不断开（原来的窗口的清理在后台做完后会话还在）
    assert_eq!(lease.session().owner(), owner_b, "会话交给编辑器所在的窗口");
    std::thread::sleep(Duration::from_millis(500));
    let manager = cx.update(|cx| AppState::sftp(cx));
    assert!(
        manager.session(lease.session().id()).is_some(),
        "原来的窗口关掉后会话还开着"
    );
    let session = lease.session().clone();
    drop(lease);
    let _ = termlet_core::runtime::runtime().block_on(async move { session.remove(&path).await });
}

// 共用终端连接的端口转发跟着终端走：终端移到新窗口、原来的窗口里不再有这条连接的终端时改属新窗口；原来的窗口关掉
// 后照样转发。复制会话（同一条连接）还在原来的窗口时转发留下，原来的窗口关掉时交给新窗口
#[gpui_kit::test]
fn shared_forward_follows_moved_terminal(cx: &mut TestAppContext) {
    use termlet_core::model::{Forward, ForwardKind};
    use termlet_core::store::Position;

    let Some(server) = test_server() else {
        eprintln!("没有设置 TERMLET_TEST_SSH，跳过");
        return;
    };
    setup(cx);
    let host = add_test_host(cx, server);
    let (a, ws_a) = open_at(cx, (0., 0.), (1024., 640.));
    let _local = open_local(cx, a, &ws_a);
    cx.update_window(a, |_, window, cx| {
        ws_a.update(cx, |w, cx| w.open_host(&host, window, cx))
    })
    .unwrap();
    let ssh = cx.update(|cx| ws_a.read(cx).live_terminals().pop().unwrap());
    wait_accepting(cx, a, "SSH 终端连上", |cx| ssh.read(cx).is_live(cx));
    let manager = cx.update(|cx| AppState::global(cx).forwards.read(cx).manager().clone());
    let start = |cx: &mut TestAppContext, terminal: &Entity<TerminalPanel>| {
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let id = cx.update(|cx| {
            AppState::store(cx)
                .create(
                    Forward {
                        kind: ForwardKind::Local,
                        host: Some(host.clone()),
                        bind_host: "127.0.0.1".into(),
                        bind_port: port,
                        target_host: "127.0.0.1".into(),
                        target_port: 22,
                        ..Default::default()
                    },
                    None,
                    Position::Last,
                )
                .unwrap()
        });
        cx.update_window(a, |_, window, cx| {
            crate::forwards::start_from_terminal(&id, terminal, window, cx)
        })
        .unwrap();
        poll_until(cx, "转发启动", |_| {
            manager.status(&id).state == termlet_core::forward::ForwardState::Running
        });
        id
    };
    let owner = |id: &termlet_core::id::ForwardId| manager.status(id).owner;
    let move_to_new_window = |cx: &mut TestAppContext, terminal: &Entity<TerminalPanel>| {
        cx.update_window(a, |_, window, cx| {
            ws_a.update(cx, |w, cx| {
                w.move_tab(
                    PanelId::from(terminal.entity_id()),
                    MoveTarget::NewWindow { at: None },
                    window,
                    cx,
                )
            })
        })
        .unwrap();
        cx.run_until_parked();
    };
    let window_owner = |cx: &mut TestAppContext, handle: AnyWindowHandle| {
        cx.update_window(handle, |_, window, _| {
            crate::sftp::panel::window_owner(window)
        })
        .unwrap()
    };
    let owner_a = window_owner(cx, a);

    // 移到新窗口、原来的窗口里不再有这条连接的终端：转发改属新窗口
    let forward = start(cx, &ssh);
    assert_eq!(owner(&forward).as_deref(), Some(owner_a.as_str()));
    move_to_new_window(cx, &ssh);
    let (b, ws_b) = other_window(cx, a);
    let owner_b = window_owner(cx, b);
    assert_eq!(owner(&forward).as_deref(), Some(owner_b.as_str()));

    // 新窗口里复制会话（同一条连接）后把终端移回去：复制的还在新窗口，转发留下；新窗口关掉时交回原来的窗口，不停
    let duplicate = cx
        .update_window(b, |_, window, cx| {
            let copy = ssh.update(cx, |p, cx| p.duplicate(window, cx));
            ws_b.update(cx, |w, cx| w.add_terminal(copy.clone(), window, cx));
            copy
        })
        .unwrap();
    wait_accepting(cx, b, "复制的会话连上", |cx| {
        duplicate.read(cx).is_live(cx)
    });
    cx.update_window(b, |_, window, cx| {
        ws_b.update(cx, |w, cx| {
            w.move_tab(
                PanelId::from(ssh.entity_id()),
                MoveTarget::Window(a),
                window,
                cx,
            )
        })
    })
    .unwrap();
    cx.run_until_parked();
    assert!(terminal_ids(cx, &ws_a).contains(&ssh.entity_id()));
    assert_eq!(
        owner(&forward).as_deref(),
        Some(owner_b.as_str()),
        "新窗口里还有这条连接的终端"
    );
    // 窗口关闭时工作区随之释放（测试别拿着它）
    drop(ws_b);
    close_window(cx, b);
    assert_eq!(owner(&forward).as_deref(), Some(owner_a.as_str()));
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        manager.status(&forward).state,
        termlet_core::forward::ForwardState::Running,
        "关掉的窗口没有停止交出去的转发"
    );
    let _ = termlet_core::runtime::runtime().block_on(manager.stop(&forward));
}

// 设置 Tab 可以移到别的窗口（在那里重新建）；有未保存的修改时不移，那个窗口已经开着设置时也不移
#[gpui_kit::test]
fn settings_tab_moves_only_without_unsaved_changes(cx: &mut TestAppContext) {
    use crate::settings::Page;
    setup(cx);
    let (a, ws_a) = open_at(cx, (0., 0.), (1024., 640.));
    let (b, ws_b) = open_at(cx, (1100., 0.), (1024., 640.));
    let open_settings = |cx: &mut TestAppContext| {
        cx.update_window(a, |_, window, cx| {
            ws_a.update(cx, |w, cx| w.open_settings(Page::General, window, cx));
            window.render_frame(cx);
        })
        .unwrap();
        cx.run_until_parked();
        let settings = cx.update(|cx| ws_a.read(cx).live_settings().unwrap());
        PanelId::from(settings.entity_id())
    };
    let move_to_b = |cx: &mut TestAppContext, id: PanelId| {
        cx.update_window(a, |_, window, cx| {
            ws_a.update(cx, |w, cx| {
                w.move_tab(id, MoveTarget::Window(b), window, cx)
            })
        })
        .unwrap();
        cx.run_until_parked();
    };
    let has = |cx: &mut TestAppContext, ws: &Entity<Workspace>| {
        cx.update(|cx| ws.read(cx).live_settings().is_some())
    };

    let id = open_settings(cx);
    cx.update_window(a, |_, window, cx| {
        window.click("settings-show-host-address", cx);
        window.render_frame(cx);
    })
    .unwrap();
    move_to_b(cx, id);
    assert!(has(cx, &ws_a) && !has(cx, &ws_b), "有未保存的修改时不移");

    cx.update_window(a, |_, window, cx| {
        window.click("settings-discard", cx);
        window.render_frame(cx);
    })
    .unwrap();
    move_to_b(cx, id);
    assert!(!has(cx, &ws_a) && has(cx, &ws_b), "放弃修改后移过去");
    cx.update_window(b, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("settings-show-host-address").is_some(),
            "在新窗口中重新建，照常显示"
        );
        window.click("settings-show-host-address", cx);
        window.render_frame(cx);
        window.click("settings-save", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(
        !cx.update(|cx| AppState::store(cx).settings())
            .general
            .show_host_address,
        "移过去后照常保存"
    );

    let id = open_settings(cx);
    move_to_b(cx, id);
    assert!(has(cx, &ws_a), "那个窗口已经开着设置：不移");
}

// SFTP 的 Tab 移到了别的窗口：“在终端中查找”写进命令后切到终端所在的窗口与 Tab；终端所在的窗口关了时提示
#[gpui_kit::test]
fn find_in_terminal_switches_to_the_terminal_window(cx: &mut TestAppContext) {
    use crate::sftp::panel::{SftpPanelEvent, SftpTarget};
    setup(cx);
    let (first, ws_a) = open_at(cx, (0., 0.), (900., 600.));
    let (second, ws_b) = open_at(cx, (950., 0.), (900., 600.));
    let terminal = open_local(cx, first, &ws_a);
    // 后开的终端在前面，跟随的那个不是当前的 Tab
    let _front = open_local(cx, first, &ws_a);
    let host = cx.update(|cx| {
        AppState::store(cx)
            .create(
                termlet_core::model::Host {
                    name: "web".into(),
                    address: "127.0.0.1".into(),
                    port: 1,
                    username: "alice".into(),
                    ..Default::default()
                },
                None,
                termlet_core::store::Position::Last,
            )
            .unwrap()
    });
    let sftp = cx
        .update_window(second, |_, window, cx| {
            ws_b.update(cx, |w, cx| {
                w.open_sftp("web".into(), SftpTarget::Host(host), None, window, cx)
            })
        })
        .unwrap()
        .unwrap();
    cx.run_until_parked();
    let shown = |cx: &mut TestAppContext| {
        cx.update_window(first, |_, window, cx| {
            terminal
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx)
        })
        .unwrap()
    };
    assert!(!shown(cx));
    let reveal = SftpPanelEvent::ShowTerminal(terminal.clone());
    cx.update(|cx| sftp.update(cx, |_, cx| cx.emit(reveal)));
    cx.run_until_parked();
    assert!(shown(cx), "切到了另一个窗口中的这个终端");

    // 终端所在的窗口关了：在 SFTP 所在的窗口提示
    cx.update_window(first, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    let reveal = SftpPanelEvent::ShowTerminal(terminal.clone());
    cx.update(|cx| sftp.update(cx, |_, cx| cx.emit(reveal)));
    cx.run_until_parked();
    let notices = cx
        .update_window(second, |_, window, cx| window.notifications(cx).len())
        .unwrap();
    assert_eq!(notices, 1);
}
