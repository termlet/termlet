//! 竖排的 Tab 列表（[`super::tab_list`]）的测试：切换位置后的布局（横排的 Tab 栏换成列表与窗格顶上的细标题，切回顶部后
//! 照旧），命令面板与设置切换位置；单个窗格不分组、几个窗格按窗格分组并用 ◀ 标出各窗格显示着的 Tab，焦点所在的高亮；
//! 按主机分组的折叠（按主机记在界面状态中，新窗口照旧）；点击切换并获得焦点、中键关闭、右键菜单、悬停时的关闭按钮；
//! 拖动排序、拖到另一窗格；窄栏（大一号的图标与图标角上的状态）与悬停浮出（在窄栏右边、不改终端的宽度，右键菜单开着时
//! 不收起）；键盘操作（逐行移动、切换、关闭、折叠、键盘打开的菜单、回到终端，窄栏上浮出）；宽度与收起状态记在界面
//! 状态中；窄窗口自动用窄栏；150% 界面大小下不溢出。

use gpui_kit::component::Placement;
use gpui_kit::component::dock::{DockPlacement, InsertTarget, PanelId};
use gpui_kit::test::{ClickOptions, TestEventExt as _, TestWindowExt as _};
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, ElementId, Entity, MouseButton, Pixels, SharedString,
    TestAppContext, WindowBounds, WindowOptions, point, px, size,
};
use termlet_core::model::{Host, TabPosition};
use termlet_core::store::Position;

use super::{FocusTabList, Workspace, collect_tab_groups};
use crate::app_state::AppState;
use crate::i18n::{self, Language};
use crate::t;
use crate::testkit::{open_workspace_sized, poll_until};

fn open(cx: &mut TestAppContext, width: f32, height: f32) -> (AnyWindowHandle, Entity<Workspace>) {
    let handle = open_workspace_sized(cx, size(px(width), px(height)));
    i18n::set_language(Language::ZhCn);
    cx.update(|cx| {
        AppState::store(cx)
            .update_settings(|s| s.general.confirm_close = false)
            .unwrap()
    });
    let workspace = cx
        .update_window(handle, |_, window, cx| {
            Workspace::in_window(window, cx).unwrap()
        })
        .unwrap();
    (handle, workspace)
}

fn set_position(cx: &mut TestAppContext, position: TabPosition) {
    cx.update(|cx| {
        AppState::store(cx)
            .update_local_settings(|s| s.window.tab_position = position)
            .unwrap()
    });
    cx.run_until_parked();
}

fn position(cx: &mut TestAppContext) -> TabPosition {
    cx.update(|cx| AppState::store(cx).local_settings().window.tab_position)
}

/// 再打开 `n` 个本地终端（都在第一个窗格中），等它们都运行起来；返回窗口中全部终端，按打开的先后。
fn open_terminals(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    n: usize,
) -> Vec<PanelId> {
    let before = cx.update(|cx| workspace.read(cx).live_terminals().len());
    for _ in 0..n {
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| w.open_local_terminal(window, cx))
        })
        .unwrap();
    }
    poll_until(cx, "终端都启动", |cx| {
        cx.update(|cx| {
            let terminals = workspace.read(cx).live_terminals();
            terminals.len() == before + n && terminals.iter().all(|p| p.read(cx).is_live(cx))
        })
    });
    cx.update(|cx| {
        workspace
            .read(cx)
            .live_terminals()
            .iter()
            .map(|p| PanelId::from(p.entity_id()))
            .collect()
    })
}

/// 各窗格中的 Tab 与显示着的是第几个。
fn panes(cx: &mut TestAppContext, workspace: &Entity<Workspace>) -> Vec<(Vec<PanelId>, usize)> {
    cx.update(|cx| {
        let dock = workspace.read(cx).dock_area.read(cx);
        let mut groups = Vec::new();
        if let Some(tree) = dock.layout(DockPlacement::Center) {
            collect_tab_groups(tree.root(), &mut groups);
        }
        groups
    })
}

fn groups(cx: &mut TestAppContext, workspace: &Entity<Workspace>) -> Vec<Vec<PanelId>> {
    panes(cx, workspace)
        .into_iter()
        .map(|(panels, _)| panels)
        .collect()
}

/// `panel` 所在窗格的节点。
fn node_of(cx: &mut TestAppContext, workspace: &Entity<Workspace>, panel: PanelId) -> u64 {
    cx.update(|cx| {
        workspace
            .read(cx)
            .dock_area
            .read(cx)
            .layout(DockPlacement::Center)
            .and_then(|tree| tree.find_panel_node(panel))
            .unwrap()
            .as_u64()
    })
}

/// 把 `moving` 移到 `anchor` 所在窗格的右边（分屏）。
fn split_right(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    anchor: PanelId,
    moving: PanelId,
) {
    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |w, cx| {
            let node = w
                .dock_area
                .read(cx)
                .layout(DockPlacement::Center)
                .and_then(|tree| tree.find_panel_node(anchor))
                .unwrap();
            let target = InsertTarget::Split {
                node,
                placement: Placement::Right,
                size: None,
            };
            w.dock_area
                .update(cx, |dock, cx| dock.move_panel(moving, target, window, cx));
        })
    })
    .unwrap();
    cx.run_until_parked();
}

fn row(panel: PanelId) -> ElementId {
    ("vtab", panel.as_u64() as usize).into()
}

fn shown_mark(panel: PanelId) -> ElementId {
    ("vtab-shown", panel.as_u64() as usize).into()
}

fn terminal_title(panel: PanelId) -> ElementId {
    ("tab-title", panel.as_u64() as usize).into()
}

/// 画一帧后在窗口中找 `id`。
fn bounds(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    id: ElementId,
) -> Option<Bounds<Pixels>> {
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.try_find(id).map(|e| e.bounds())
    })
    .unwrap()
}

fn shown(cx: &mut TestAppContext, handle: AnyWindowHandle, id: impl Into<ElementId>) -> bool {
    bounds(cx, handle, id.into()).is_some()
}

fn focused(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    workspace: &Entity<Workspace>,
) -> Option<PanelId> {
    cx.update_window(handle, |_, window, cx| {
        workspace
            .read(cx)
            .focused_terminal(window, cx)
            .map(|p| PanelId::from(p.entity_id()))
    })
    .unwrap()
}

fn ui_state(cx: &mut TestAppContext) -> termlet_core::ui_state::UiState {
    cx.update(|cx| AppState::global(cx).ui.read(cx).state().clone())
}

/// 等几帧：量出的宽度、悬停等在下一帧才生效。
fn settle(cx: &mut TestAppContext, handle: AnyWindowHandle) {
    for _ in 0..3 {
        cx.update_window(handle, |_, window, cx| window.render_frame(cx))
            .unwrap();
        cx.run_until_parked();
    }
}

// 放在左侧：Tab 竖排在侧边栏右边的一列，窗格顶上不再有横排的 Tab 栏、只有显示着的 Tab 的细标题；侧边栏收起后贴着
// 窗口左边；放回顶部后照旧是横排的 Tab 栏
#[gpui_kit::test]
fn left_position_lists_every_tab_beside_the_sidebar(cx: &mut TestAppContext) {
    let (handle, workspace) = open(cx, 1024., 640.);
    let ids = open_terminals(cx, handle, &workspace, 2);
    assert!(shown(cx, handle, terminal_title(ids[0])), "默认横排");
    assert!(shown(cx, handle, terminal_title(ids[1])));
    assert!(!shown(cx, handle, "vtab-column"));
    assert!(!shown(cx, handle, "pane-title"));

    set_position(cx, TabPosition::Left);
    let column = bounds(cx, handle, "vtab-column".into()).expect("有 Tab 列表");
    let sidebar = bounds(cx, handle, "sidebar-resize".into()).unwrap();
    assert!(
        (column.left() - sidebar.center().x).abs() < px(2.),
        "列表在侧边栏右边：{column:?}，侧边栏的边缘 {sidebar:?}"
    );
    let first = bounds(cx, handle, row(ids[0])).unwrap();
    let second = bounds(cx, handle, row(ids[1])).unwrap();
    assert!(first.top() < second.top(), "按 Tab 的顺序竖排");
    assert!(first.left() >= column.left() && first.right() <= column.right() + px(0.5));
    let title = bounds(cx, handle, "pane-title".into()).expect("窗格顶上有细标题");
    assert!(title.left() >= column.right() - px(0.5), "终端在列表右边");
    assert!(
        !shown(cx, handle, terminal_title(ids[0])),
        "没有横排的 Tab 栏：没显示的 Tab 的标题不在窗格上"
    );
    assert!(
        shown(cx, handle, terminal_title(ids[1])),
        "细标题是显示着的 Tab 的"
    );

    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |w, cx| w.toggle_sidebar(window, cx))
    })
    .unwrap();
    cx.run_until_parked();
    let column = bounds(cx, handle, "vtab-column".into()).unwrap();
    assert_eq!(column.left(), px(0.), "侧边栏收起后贴着窗口左边");

    set_position(cx, TabPosition::Top);
    assert!(!shown(cx, handle, "vtab-column"));
    assert!(!shown(cx, handle, "pane-title"));
    assert!(shown(cx, handle, terminal_title(ids[0])), "回到横排");
}

// 命令面板的“切换 Tab 的位置”在顶部、左侧之间切换；设置“通用”页选“左侧”保存后立即生效，只存进本机设置
#[gpui_kit::test]
fn palette_and_settings_switch_the_position(cx: &mut TestAppContext) {
    let (handle, _) = open(cx, 1024., 640.);
    let synced = cx.update(|cx| AppState::store(cx).settings());
    let palette = |cx: &mut TestAppContext| {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.press("ctrl-shift-p", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.input("> 切换 Tab 的位置", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
    };
    palette(cx);
    assert_eq!(position(cx), TabPosition::Left);
    assert!(shown(cx, handle, "vtab-column"));
    palette(cx);
    assert_eq!(position(cx), TabPosition::Top);

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("open-settings", cx);
        window.render_frame(cx);
        // 在窗口的行为之后，测试窗口中要往下滚才看得到
        window.scroll(
            "settings-page",
            gpui_kit::ScrollDelta::Pixels(point(px(0.), px(-400.))),
            cx,
        );
        window.render_frame(cx);
        window.within("settings-tab-position").click(1usize, cx);
        window.render_frame(cx);
        window.click("settings-save", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(position(cx), TabPosition::Left);
    assert_eq!(
        cx.update(|cx| AppState::store(cx).settings()),
        synced,
        "会同步的设置没有变"
    );
    assert!(shown(cx, handle, "vtab-column"), "保存后立即生效");
}

// 一个窗格时不分组、没有 ◀；分屏后按窗格分组（左侧窗格、右侧窗格），◀ 标出各窗格显示着的 Tab；焦点所在的窗格显示着
// 的 Tab 高亮，点另一行切过去并获得焦点
#[gpui_kit::test]
fn split_panes_are_grouped_and_the_shown_tabs_marked(cx: &mut TestAppContext) {
    let (handle, workspace) = open(cx, 1024., 640.);
    set_position(cx, TabPosition::Left);
    let ids = open_terminals(cx, handle, &workspace, 3);
    let node = node_of(cx, &workspace, ids[0]);
    assert!(
        !shown(cx, handle, ("vtab-pane", node as usize)),
        "一个窗格不分组"
    );
    for id in &ids {
        assert!(!shown(cx, handle, shown_mark(*id)), "一个窗格没有 ◀");
    }

    split_right(cx, handle, &workspace, ids[0], ids[2]);
    let layout = panes(cx, &workspace);
    assert_eq!(layout.len(), 2);
    let (left, right) = (&layout[0], &layout[1]);
    assert_eq!(right.0, vec![ids[2]]);
    let left_node = node_of(cx, &workspace, ids[0]);
    let right_node = node_of(cx, &workspace, ids[2]);
    let header_label = |cx: &mut TestAppContext, node: u64| {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let header = window.find(("vtab-pane", node as usize));
            (header.label().map(str::to_string), header.bounds())
        })
        .unwrap()
    };
    let (left_label, left_header) = header_label(cx, left_node);
    let (right_label, right_header) = header_label(cx, right_node);
    assert_eq!(
        left_label.as_deref(),
        Some(t!("tab_list.pane.left").as_ref())
    );
    assert_eq!(
        right_label.as_deref(),
        Some(t!("tab_list.pane.right").as_ref())
    );
    let first = bounds(cx, handle, row(ids[0])).unwrap();
    let moved = bounds(cx, handle, row(ids[2])).unwrap();
    assert!(left_header.top() < first.top() && first.top() < right_header.top());
    assert!(
        right_header.top() < moved.top(),
        "右侧窗格的 Tab 在它的标题下"
    );

    let left_shown = left.0[left.1];
    let left_other = *left.0.iter().find(|p| **p != left_shown).unwrap();
    assert!(
        shown(cx, handle, shown_mark(left_shown)),
        "◀ 标出左侧窗格显示着的"
    );
    assert!(!shown(cx, handle, shown_mark(left_other)));
    assert!(
        shown(cx, handle, shown_mark(ids[2])),
        "◀ 标出右侧窗格显示着的"
    );

    let selected = |cx: &mut TestAppContext, panel: PanelId| {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.find(row(panel)).selected()
        })
        .unwrap()
    };
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click(row(ids[2]), cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(focused(cx, handle, &workspace), Some(ids[2]));
    assert_eq!(
        selected(cx, ids[2]),
        Some(true),
        "焦点所在的窗格显示着的高亮"
    );
    assert_eq!(selected(cx, left_shown), Some(false));

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click(row(left_other), cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        focused(cx, handle, &workspace),
        Some(left_other),
        "点击切换并获得焦点"
    );
    assert_eq!(selected(cx, left_other), Some(true));
    assert_eq!(selected(cx, ids[2]), Some(false));
    assert!(
        shown(cx, handle, shown_mark(left_other)),
        "◀ 跟到左侧窗格新显示的"
    );
    assert!(!shown(cx, handle, shown_mark(left_shown)));
}

// 按主机分组：同一窗格中连同一台主机的两个 Tab 归到主机标题下（带数量），点标题折叠、再点展开；不连主机的照常一行；
// 关掉开关后不分组
#[gpui_kit::test]
fn group_by_host_folds_tabs_of_the_same_host(cx: &mut TestAppContext) {
    let (handle, workspace) = open(cx, 1024., 640.);
    set_position(cx, TabPosition::Left);
    // 连不上的主机：打开很快失败，Tab 照样在
    let host = cx.update(|cx| {
        AppState::store(cx)
            .create(
                Host {
                    name: "web".into(),
                    address: "127.0.0.1".into(),
                    port: 1,
                    username: "alice".into(),
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap()
    });
    let local = open_terminals(cx, handle, &workspace, 1)[0];
    for _ in 0..2 {
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| w.open_host(&host, window, cx))
        })
        .unwrap();
    }
    cx.run_until_parked();
    let terminals: Vec<PanelId> = cx.update(|cx| {
        workspace
            .read(cx)
            .live_terminals()
            .iter()
            .map(|p| PanelId::from(p.entity_id()))
            .collect()
    });
    let web: Vec<PanelId> = terminals.into_iter().filter(|p| *p != local).collect();
    assert_eq!(web.len(), 2);
    let node = node_of(cx, &workspace, local);
    let header: ElementId =
        SharedString::from(format!("vtab-host-{node}-{}", host.as_str())).into();
    assert!(!shown(cx, handle, header.clone()), "默认不按主机分组");

    cx.update(|cx| {
        AppState::store(cx)
            .update_local_settings(|s| s.window.group_tabs_by_host = true)
            .unwrap()
    });
    cx.run_until_parked();
    let snapshot = |cx: &mut TestAppContext| {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let header = window.find(header.clone());
            (header.label().map(str::to_string), header.expanded())
        })
        .unwrap()
    };
    assert_eq!(snapshot(cx), (Some("web".to_string()), Some(true)));
    let before = focused(cx, handle, &workspace);
    assert!(before.is_some());
    let title = bounds(cx, handle, header.clone()).unwrap();
    for id in &web {
        let r = bounds(cx, handle, row(*id)).expect("主机标题下的 Tab");
        assert!(r.top() > title.top(), "在主机标题下");
    }
    assert!(shown(cx, handle, row(local)), "本地终端照常一行");

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click(header.clone(), cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(snapshot(cx).1, Some(false), "折叠");
    for id in &web {
        assert!(!shown(cx, handle, row(*id)));
    }
    assert!(shown(cx, handle, row(local)));
    assert_eq!(
        ui_state(cx).collapsed_tab_hosts,
        [host.as_str()],
        "按主机记在界面状态中"
    );
    assert_eq!(
        focused(cx, handle, &workspace),
        before,
        "点主机标题不拿走终端的焦点"
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click(header.clone(), cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(web.iter().all(|id| shown(cx, handle, row(*id))), "再点展开");
    assert!(ui_state(cx).collapsed_tab_hosts.is_empty());

    cx.update(|cx| {
        AppState::store(cx)
            .update_local_settings(|s| s.window.group_tabs_by_host = false)
            .unwrap()
    });
    cx.run_until_parked();
    assert!(!shown(cx, handle, header));
    assert_eq!(groups(cx, &workspace).concat().len(), 3, "只影响显示");
}

// 右键一行是与横排相同的 Tab 菜单，先切到这个 Tab（菜单中的操作派发给显示着的终端），菜单关掉后焦点在它上面；中键
// 关闭；悬停时行尾出现关闭按钮，点它关闭
#[gpui_kit::test]
fn rows_open_the_tab_menu_and_close_tabs(cx: &mut TestAppContext) {
    let (handle, workspace) = open(cx, 1024., 640.);
    set_position(cx, TabPosition::Left);
    let ids = open_terminals(cx, handle, &workspace, 3);
    assert_eq!(
        panes(cx, &workspace),
        [(ids.clone(), 2)],
        "最后打开的显示着"
    );

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.right_click(row(ids[0]), cx);
    })
    .unwrap();
    cx.run_until_parked();
    let close_others = t!("panel.close_others").to_string();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let menu = window.within("popup-menu");
        let found = (0..60usize).any(|i| {
            menu.try_find(i)
                .is_some_and(|item| item.label() == Some(close_others.as_str()))
        });
        assert!(found, "右键是 Tab 的菜单");
        window.press("escape", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        panes(cx, &workspace),
        [(ids.clone(), 0)],
        "右键不关闭，切到右键的那个"
    );
    assert_eq!(focused(cx, handle, &workspace), Some(ids[0]));

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click_with_options(
            row(ids[0]),
            ClickOptions::new().with_button(MouseButton::Middle),
            cx,
        );
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(groups(cx, &workspace), [ids[1..].to_vec()], "中键关闭");

    // 指针停在关掉的那一行原来的地方（现在是下一行）：最后一行不在指针下
    let close: ElementId = ("vtab-close", ids[2].as_u64() as usize).into();
    let visible = |cx: &mut TestAppContext| {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.try_find(close.clone()).is_some_and(|e| e.visible())
        })
        .unwrap()
    };
    assert!(!visible(cx), "不悬停时不显示关闭按钮");
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.hover(row(ids[2]), cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(visible(cx), "悬停时显示");
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click(close.clone(), cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(groups(cx, &workspace), [vec![ids[1]]]);
}

/// 从 `from` 拖到 `to` 所在行的上半（`upper`）或下半。
fn drag_row(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    from: ElementId,
    to: ElementId,
    upper: bool,
) {
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let start = window.find(from).bounds().center();
        let target = window.find(to).bounds();
        let y = target.top() + target.size.height * if upper { 0.25 } else { 0.75 };
        window.drag(start, point(target.center().x, y), cx);
    })
    .unwrap();
    cx.run_until_parked();
}

// 拖动排序：放在一行的下半插到它后面、上半插到它前面；拖到另一窗格的行上移过去；拖到另一窗格顶上的细标题、列表中的
// 窗格标题上放到那个窗格最后
#[gpui_kit::test]
fn dragging_rows_reorders_and_moves_tabs_between_panes(cx: &mut TestAppContext) {
    let (handle, workspace) = open(cx, 1024., 640.);
    set_position(cx, TabPosition::Left);
    let ids = open_terminals(cx, handle, &workspace, 4);

    drag_row(cx, handle, row(ids[0]), row(ids[2]), false);
    assert_eq!(
        groups(cx, &workspace),
        [vec![ids[1], ids[2], ids[0], ids[3]]]
    );
    drag_row(cx, handle, row(ids[0]), row(ids[1]), true);
    assert_eq!(groups(cx, &workspace), std::slice::from_ref(&ids));

    split_right(cx, handle, &workspace, ids[0], ids[3]);
    assert_eq!(groups(cx, &workspace), [ids[..3].to_vec(), vec![ids[3]]]);
    drag_row(cx, handle, row(ids[0]), row(ids[3]), false);
    assert_eq!(
        groups(cx, &workspace),
        [vec![ids[1], ids[2]], vec![ids[3], ids[0]]],
        "拖到另一窗格的行后面"
    );
    assert_eq!(
        focused(cx, handle, &workspace),
        Some(ids[0]),
        "拖过去的显示并获得焦点"
    );

    // 右边窗格顶上的细标题
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let start = window.find(row(ids[1])).bounds().center();
        let title = window
            .find_all("pane-title")
            .into_iter()
            .map(|t| t.bounds())
            .max_by(|a, b| f32::from(a.left()).total_cmp(&f32::from(b.left())))
            .unwrap();
        window.drag(start, title.center(), cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        groups(cx, &workspace),
        [vec![ids[2]], vec![ids[3], ids[0], ids[1]]],
        "拖到细标题上放到那个窗格最后"
    );

    let right = node_of(cx, &workspace, ids[3]);
    drag_row(
        cx,
        handle,
        row(ids[2]),
        ("vtab-pane", right as usize).into(),
        true,
    );
    assert_eq!(
        groups(cx, &workspace),
        [vec![ids[3], ids[0], ids[1], ids[2]]],
        "拖到窗格的标题上放到最后（原来的窗格空了，合成一个）"
    );
}

// 把一行拖到窗口外的空处：与拖横排的 Tab 相同，在放下的位置新开一个窗口，Tab 移过去
#[gpui_kit::test]
fn dragging_a_row_out_of_the_window_moves_the_tab(cx: &mut TestAppContext) {
    let (handle, workspace) = open(cx, 1024., 640.);
    set_position(cx, TabPosition::Left);
    let ids = open_terminals(cx, handle, &workspace, 2);
    let out = point(px(-80.), px(150.));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let start = window.find(row(ids[0])).bounds().center();
        window.pointer_move(start, None, cx);
        window.pointer_down(start, MouseButton::Left, cx);
        for step in 1..=8 {
            let f = step as f32 / 8.;
            let at = point(
                start.x + (out.x - start.x) * f,
                start.y + (out.y - start.y) * f,
            );
            window.pointer_move(at, Some(MouseButton::Left), cx);
        }
    })
    .unwrap();
    cx.run_until_parked();
    assert!(shown(cx, handle, "tab-drag-out-hint"), "拖到窗口外时提示");
    cx.update_window(handle, |_, window, cx| {
        window.pointer_up(out, MouseButton::Left, cx)
    })
    .unwrap();
    cx.run_until_parked();
    let windows = cx.update(|cx| super::move_tab::open_workspaces(cx));
    assert_eq!(windows.len(), 2, "新开了一个窗口");
    let (_, other) = windows.into_iter().find(|(h, _)| *h != handle).unwrap();
    assert_eq!(
        cx.update(|cx| other.read(cx).terminal_ids()),
        [gpui_kit::EntityId::from(ids[0].as_u64())]
    );
    assert_eq!(groups(cx, &workspace), [vec![ids[1]]]);
}

// 收成窄栏：只显示图标，终端变宽；悬停时浮出展开的列表盖在终端上、终端宽度不变，可以点里面的行；右键菜单开着时
// 指针离开也不收起，菜单关掉后收起；“⇥”展开
#[gpui_kit::test]
fn rail_expands_on_hover_without_resizing_the_terminal(cx: &mut TestAppContext) {
    let (handle, workspace) = open(cx, 1024., 640.);
    set_position(cx, TabPosition::Left);
    let ids = open_terminals(cx, handle, &workspace, 2);
    let expanded_title = bounds(cx, handle, "pane-title".into()).unwrap();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("vtab-collapse", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(ui_state(cx).tab_list_collapsed, "记下收起");
    assert!(!shown(cx, handle, "vtab-column"));
    let rail = bounds(cx, handle, "vtab-rail".into()).expect("窄栏");
    assert!(shown(cx, handle, ("vtab-rail", ids[0].as_u64() as usize)));
    assert!(!shown(cx, handle, row(ids[0])), "窄栏只有图标");
    let title = bounds(cx, handle, "pane-title".into()).unwrap();
    assert!(title.left() < expanded_title.left(), "终端变宽");

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.hover("vtab-rail", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let overlay = bounds(cx, handle, "vtab-overlay".into()).expect("悬停时浮出");
    assert_eq!(overlay.left(), rail.right(), "在窄栏的右边，不盖住窄栏");
    assert!(overlay.right() > rail.right() + px(40.), "盖在终端上");
    assert_eq!(
        bounds(cx, handle, "pane-title".into()).unwrap(),
        title,
        "不改终端的宽度"
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click(row(ids[0]), cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(focused(cx, handle, &workspace), Some(ids[0]));
    assert!(shown(cx, handle, "vtab-overlay"), "指针还在列表上");

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.right_click(row(ids[1]), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("popup-menu").is_some());
        window.hover("title-bar", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(shown(cx, handle, "vtab-overlay"), "菜单开着时不收起");
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.press("escape", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(
        !shown(cx, handle, "vtab-overlay"),
        "菜单关掉、指针不在列表上：收起"
    );

    // 浮出的列表不盖住窄栏：窄栏顶上的“⇥”与浮出的列表标题行的“⇥”都能点，都是展开
    for expand in ["vtab-rail-expand", "vtab-expand"] {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.hover("vtab-rail", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("vtab-overlay").is_some());
            let rail_expand = window.find("vtab-rail-expand").bounds();
            let overlay = window.find("vtab-overlay").bounds();
            assert!(
                rail_expand.right() <= overlay.left(),
                "窄栏的“⇥”不在浮出的列表下面"
            );
            window.click(expand, cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert!(!ui_state(cx).tab_list_collapsed, "{expand}");
        assert!(shown(cx, handle, "vtab-column"));
        assert!(!shown(cx, handle, "vtab-overlay"));
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("vtab-collapse", cx);
        })
        .unwrap();
        cx.run_until_parked();
    }
}

// 窄栏：图标的方块随界面大小（2rem）；连不上的主机的 Tab 图标右上角有状态点（悬停提示的写法见 `tab_list` 的单元
// 测试）；没有状态的不画
#[gpui_kit::test]
fn rail_icons_show_status_dots(cx: &mut TestAppContext) {
    let (handle, workspace) = open(cx, 1024., 640.);
    set_position(cx, TabPosition::Left);
    // 本地 shell 的提示符可能在切到下面那个 Tab 之后才到，那时会标上“新输出”；关掉这项标记，本地终端才确实没有状态
    cx.update(|cx| {
        AppState::store(cx)
            .update_settings(|s| s.terminal.tab_activity = false)
            .unwrap()
    });
    let local = open_terminals(cx, handle, &workspace, 1)[0];
    let host = cx.update(|cx| {
        AppState::store(cx)
            .create(
                Host {
                    name: "down".into(),
                    address: "127.0.0.1".into(),
                    port: 1,
                    username: "alice".into(),
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap()
    });
    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |w, cx| w.open_host(&host, window, cx))
    })
    .unwrap();
    let failed = cx.update(|cx| {
        workspace
            .read(cx)
            .live_terminals()
            .iter()
            .map(|p| PanelId::from(p.entity_id()))
            .find(|p| *p != local)
            .unwrap()
    });
    let rail_status: ElementId = ("vtab-rail-status", failed.as_u64() as usize).into();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("vtab-collapse", cx);
    })
    .unwrap();
    cx.run_until_parked();
    poll_until(cx, "连接失败", |cx| {
        shown(cx, handle, rail_status.clone())
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let cell = window
            .find(("vtab-rail", failed.as_u64() as usize))
            .bounds();
        assert_eq!(
            cell.size.height,
            window.rem_size() * 2.,
            "图标的方块是 2rem"
        );
        let dot = window.find(rail_status.clone()).bounds();
        assert!(
            dot.right() <= cell.right() + px(0.5)
                && dot.top() >= cell.top() - px(0.5)
                && dot.center().x > cell.center().x
                && dot.center().y < cell.center().y,
            "状态点在图标的右上角：{dot:?}，图标 {cell:?}"
        );
        assert!(
            window
                .try_find(("vtab-rail-status", local.as_u64() as usize))
                .is_none(),
            "没有状态的不画"
        );
    })
    .unwrap();
}

/// 在窗口中派发动作（键盘焦点所在的位置）。
fn dispatch(cx: &mut TestAppContext, handle: AnyWindowHandle, action: impl gpui_kit::Action) {
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.dispatch_action(Box::new(action), cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn press(cx: &mut TestAppContext, handle: AnyWindowHandle, key: &str) {
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.press(key, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn list_focus(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    workspace: &Entity<Workspace>,
) -> (bool, Option<Result<PanelId, SharedString>>) {
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let w = workspace.read(cx);
        (w.tab_list_has_focus(window), w.tab_list_cursor())
    })
    .unwrap()
}

// 键盘：“焦点进 Tab 列表”后当前行是高亮的 Tab；↑ ↓ Home End 移动；Enter 切过去并获得焦点；Delete 关闭、焦点仍在列表
// 上、下一行补上来；Shift+F10 在当前行下打开 Tab 菜单，Esc 关掉后回到列表；Esc 回到终端；点列表的空白处焦点进列表
#[gpui_kit::test]
fn keyboard_moves_switches_and_closes(cx: &mut TestAppContext) {
    let (handle, workspace) = open(cx, 1024., 640.);
    set_position(cx, TabPosition::Left);
    let ids = open_terminals(cx, handle, &workspace, 4);
    dispatch(cx, handle, FocusTabList);
    assert_eq!(
        list_focus(cx, handle, &workspace),
        (true, Some(Ok(ids[3]))),
        "当前行从高亮的 Tab 开始"
    );
    press(cx, handle, "up");
    assert_eq!(list_focus(cx, handle, &workspace).1, Some(Ok(ids[2])));
    press(cx, handle, "home");
    assert_eq!(list_focus(cx, handle, &workspace).1, Some(Ok(ids[0])));
    press(cx, handle, "up");
    assert_eq!(
        list_focus(cx, handle, &workspace).1,
        Some(Ok(ids[0])),
        "到头不动"
    );
    press(cx, handle, "end");
    press(cx, handle, "up");
    assert_eq!(list_focus(cx, handle, &workspace).1, Some(Ok(ids[2])));
    assert_eq!(panes(cx, &workspace), [(ids.clone(), 3)], "移动不切换");

    press(cx, handle, "enter");
    assert_eq!(
        focused(cx, handle, &workspace),
        Some(ids[2]),
        "Enter 切过去"
    );
    assert!(!list_focus(cx, handle, &workspace).0);

    dispatch(cx, handle, FocusTabList);
    press(cx, handle, "up");
    press(cx, handle, "delete");
    assert_eq!(
        groups(cx, &workspace),
        [vec![ids[0], ids[2], ids[3]]],
        "Delete 关闭"
    );
    assert_eq!(
        list_focus(cx, handle, &workspace),
        (true, Some(Ok(ids[1]))),
        "焦点仍在列表上"
    );
    press(cx, handle, "down");
    assert_eq!(
        list_focus(cx, handle, &workspace).1,
        Some(Ok(ids[3])),
        "关掉的那一行由下一行补上，再往下"
    );

    press(cx, handle, "shift-f10");
    let close_others = t!("panel.close_others").to_string();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let menu = window.within("popup-menu");
        let found = (0..60usize).any(|i| {
            menu.try_find(i)
                .is_some_and(|item| item.label() == Some(close_others.as_str()))
        });
        assert!(found, "Tab 的菜单");
        let menu = window.find("popup-menu").bounds();
        let row = window.find(row(ids[3])).bounds();
        assert!(menu.top() >= row.bottom() - px(1.), "在当前行的下面");
    })
    .unwrap();
    assert_eq!(
        panes(cx, &workspace),
        [(vec![ids[0], ids[2], ids[3]], 2)],
        "先切过去"
    );
    press(cx, handle, "escape");
    assert!(!shown(cx, handle, "popup-menu"));
    assert!(list_focus(cx, handle, &workspace).0, "菜单关掉后回到列表");

    press(cx, handle, "escape");
    assert!(!list_focus(cx, handle, &workspace).0);
    assert_eq!(
        focused(cx, handle, &workspace),
        Some(ids[3]),
        "Esc 回到终端"
    );

    // 列表的空白处（行都在上面）
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let list = window.find("vtab-list").bounds();
        let offset = point(list.size.width / 2., list.size.height - px(8.));
        window.click_at("vtab-list", offset, cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(list_focus(cx, handle, &workspace).0, "点空白处焦点进列表");
}

// 键盘与主机分组：← 折叠主机标题（记下）、→ 展开，再 → 到其中第一个 Tab，在其中按 ← 回到主机标题；Enter 在主机标题上
// 折叠 / 展开；收成窄栏时焦点进列表就浮出，Esc 后收起
#[gpui_kit::test]
fn keyboard_folds_host_groups_and_floats_over_the_rail(cx: &mut TestAppContext) {
    let (handle, workspace) = open(cx, 1024., 640.);
    set_position(cx, TabPosition::Left);
    cx.update(|cx| {
        AppState::store(cx)
            .update_local_settings(|s| s.window.group_tabs_by_host = true)
            .unwrap()
    });
    let host = cx.update(|cx| {
        AppState::store(cx)
            .create(
                Host {
                    name: "web".into(),
                    address: "127.0.0.1".into(),
                    port: 1,
                    username: "alice".into(),
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap()
    });
    let local = open_terminals(cx, handle, &workspace, 1)[0];
    for _ in 0..2 {
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| w.open_host(&host, window, cx))
        })
        .unwrap();
    }
    cx.run_until_parked();
    let web: Vec<PanelId> = cx.update(|cx| {
        workspace
            .read(cx)
            .live_terminals()
            .iter()
            .map(|p| PanelId::from(p.entity_id()))
            .filter(|p| *p != local)
            .collect()
    });
    let key = SharedString::from(host.as_str().to_string());
    // 焦点在本地终端上：当前行是它，↓ 到主机标题
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click(row(local), cx);
    })
    .unwrap();
    cx.run_until_parked();
    dispatch(cx, handle, FocusTabList);
    assert_eq!(list_focus(cx, handle, &workspace).1, Some(Ok(local)));
    press(cx, handle, "down");
    assert_eq!(list_focus(cx, handle, &workspace).1, Some(Err(key.clone())));
    press(cx, handle, "left");
    assert_eq!(ui_state(cx).collapsed_tab_hosts, [host.as_str()], "折叠");
    assert!(web.iter().all(|id| !shown(cx, handle, row(*id))));
    press(cx, handle, "right");
    assert!(ui_state(cx).collapsed_tab_hosts.is_empty(), "展开");
    press(cx, handle, "right");
    assert_eq!(
        list_focus(cx, handle, &workspace).1,
        Some(Ok(web[0])),
        "已展开时到其中第一个 Tab"
    );
    press(cx, handle, "left");
    assert_eq!(
        list_focus(cx, handle, &workspace).1,
        Some(Err(key.clone())),
        "回到主机标题"
    );
    press(cx, handle, "enter");
    assert!(!ui_state(cx).collapsed_tab_hosts.is_empty(), "Enter 折叠");
    assert!(list_focus(cx, handle, &workspace).0);

    // 高亮的 Tab 折在主机标题里：焦点进列表时当前行是主机标题，→ 展开后再 → 到其中第一个 Tab（推算出的当前行记下来，
    // 不跳回展开后露出来的高亮的 Tab）
    let header: ElementId = SharedString::from(format!(
        "vtab-host-{}-{key}",
        node_of(cx, &workspace, web[0])
    ))
    .into();
    press(cx, handle, "escape");
    for target in [header.clone(), row(web[1]), header.clone()] {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click(target, cx);
        })
        .unwrap();
        cx.run_until_parked();
    }
    assert_eq!(focused(cx, handle, &workspace), Some(web[1]));
    assert!(!ui_state(cx).collapsed_tab_hosts.is_empty());
    dispatch(cx, handle, FocusTabList);
    press(cx, handle, "right");
    assert!(ui_state(cx).collapsed_tab_hosts.is_empty(), "展开");
    press(cx, handle, "right");
    assert_eq!(list_focus(cx, handle, &workspace).1, Some(Ok(web[0])));
    press(cx, handle, "left");
    press(cx, handle, "enter");
    assert!(!ui_state(cx).collapsed_tab_hosts.is_empty(), "折回去");

    // 折叠的主机分组在新窗口中照旧（按主机记在界面状态中，重启后读回同一份）
    let _second = cx
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
        .unwrap();
    cx.run_until_parked();
    assert!(cx.update(|cx| {
        AppState::global(cx)
            .ui
            .read(cx)
            .state()
            .tab_host_collapsed(&key)
    }));

    // 收成窄栏：焦点进列表时浮出（指针不在上面），Esc 回到终端后收起
    press(cx, handle, "escape");
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("vtab-collapse", cx);
        window.render_frame(cx);
        window.hover("title-bar", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(!shown(cx, handle, "vtab-overlay"));
    dispatch(cx, handle, FocusTabList);
    assert!(shown(cx, handle, "vtab-overlay"), "焦点进列表时浮出");
    press(cx, handle, "escape");
    assert!(!shown(cx, handle, "vtab-overlay"), "回到终端后收起");
}

// 拖动右边缘调整宽度，记在界面状态中（按 100% 的界面大小），双击恢复默认；收起的状态也记着，新开的窗口照样是窄栏
#[gpui_kit::test]
fn width_and_collapsed_state_are_remembered(cx: &mut TestAppContext) {
    let (handle, workspace) = open(cx, 1024., 640.);
    set_position(cx, TabPosition::Left);
    open_terminals(cx, handle, &workspace, 1);
    let before = bounds(cx, handle, "vtab-column".into()).unwrap();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let edge = window.find("vtab-resize").bounds().center();
        window.drag(edge, point(edge.x + px(80.), edge.y), cx);
    })
    .unwrap();
    cx.run_until_parked();
    let saved = ui_state(cx).tab_list_width.expect("记下宽度");
    assert!(
        (saved - (f32::from(before.size.width) + 80.)).abs() < 3.,
        "宽了 80：{saved}"
    );
    let after = bounds(cx, handle, "vtab-column".into()).unwrap();
    assert!((after.size.width - before.size.width - px(80.)).abs() < px(3.));

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.double_click("vtab-resize", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(ui_state(cx).tab_list_width, None, "双击恢复默认");

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("vtab-collapse", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let (second, _) = cx
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
        .unwrap();
    cx.run_until_parked();
    assert!(shown(cx, second, "vtab-rail"), "新开的窗口照样是窄栏");
}

// 窗口窄得放不下展开的列表时自动用窄栏（不改记下的状态，没有“⇥”），侧边栏收起、放得下时展开
#[gpui_kit::test]
fn a_narrow_window_uses_the_rail_by_itself(cx: &mut TestAppContext) {
    let (min_w, min_h) = crate::window_state::MIN_SIZE;
    let (handle, workspace) = open(cx, min_w, min_h);
    set_position(cx, TabPosition::Left);
    open_terminals(cx, handle, &workspace, 1);
    assert!(shown(cx, handle, "vtab-rail"));
    assert!(!ui_state(cx).tab_list_collapsed, "不改记下的状态");
    assert!(
        !shown(cx, handle, "vtab-rail-expand"),
        "放不下时没有展开的按钮"
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.hover("vtab-rail", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(shown(cx, handle, "vtab-overlay"));
    assert!(!shown(cx, handle, "vtab-expand"));

    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |w, cx| w.toggle_sidebar(window, cx))
    })
    .unwrap();
    cx.run_until_parked();
    assert!(shown(cx, handle, "vtab-column"), "侧边栏收起后放得下");
}

// 界面放大到 150%、英文、分屏时，列表的各行、关闭按钮都在列表之内，窗格的细标题在窗口之内
#[gpui_kit::test]
fn rows_fit_at_150_percent(cx: &mut TestAppContext) {
    let (handle, workspace) = open(cx, 1280., 800.);
    i18n::set_language(Language::En);
    cx.update(|cx| {
        AppState::store(cx)
            .update_local_settings(|s| s.window.ui_scale = 150)
            .unwrap()
    });
    set_position(cx, TabPosition::Left);
    let ids = open_terminals(cx, handle, &workspace, 3);
    split_right(cx, handle, &workspace, ids[0], ids[2]);
    settle(cx, handle);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.rem_size(), px(24.));
        window.hover(row(ids[1]), cx);
        window.render_frame(cx);
        let column = window.find("vtab-column").bounds();
        let inside = |b: Bounds<Pixels>, what: &str| {
            assert!(
                b.left() >= column.left() - px(0.5) && b.right() <= column.right() + px(0.5),
                "{what} 超出了列表：{b:?}，列表 {column:?}"
            );
        };
        for id in &ids {
            inside(window.find(row(*id)).bounds(), "行");
        }
        inside(
            window
                .find(("vtab-close", ids[1].as_u64() as usize))
                .bounds(),
            "关闭按钮",
        );
        inside(window.find("vtab-new").bounds(), "新建");
        inside(window.find("vtab-collapse").bounds(), "收起");
        let width = window.viewport_size().width;
        for title in window.find_all("pane-title") {
            let b = title.bounds();
            assert!(b.left() >= column.right() - px(0.5) && b.right() <= width + px(0.5));
        }
    })
    .unwrap();
    i18n::set_language(Language::ZhCn);
}
