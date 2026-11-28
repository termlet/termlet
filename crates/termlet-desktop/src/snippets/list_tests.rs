//! 快捷命令页的界面测试：拖动排序（组内、跨组、分组标题）、分组的重命名与整组删除、编辑时选已有分组。
//! 文案按本线程的语言（默认简体中文）查找。

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, SharedString, TestAppContext, point};
use termlet_core::id::SnippetId;
use termlet_core::model::Snippet;
use termlet_core::snippet_order;
use termlet_core::store::Position;

use super::groups::collapsed_key;
use crate::app_state::AppState;
use crate::testkit::open_workspace;

fn add(cx: &mut TestAppContext, group: &str, name: &str) -> SnippetId {
    cx.update(|cx| {
        AppState::store(cx)
            .create(
                Snippet {
                    name: name.into(),
                    group: group.into(),
                    content: format!("echo {name}\n"),
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap()
    })
}

/// 按显示顺序：每个分组一行，`分组: 名称 名称`（未分组写作 `-`）。
fn layout(cx: &mut TestAppContext) -> Vec<String> {
    cx.update(|cx| {
        let store = AppState::store(cx);
        let list = store.list::<Snippet>();
        let order = store.settings().snippets.group_order;
        snippet_order::grouped(&list, &order)
            .into_iter()
            .map(|(group, members)| {
                let names: Vec<&str> = members.iter().map(|s| s.doc.name.as_str()).collect();
                let group = if group.is_empty() { "-" } else { &group };
                format!("{group}: {}", names.join(" "))
            })
            .collect()
    })
}

fn row(id: &SnippetId) -> SharedString {
    format!("snippet-row-{id}").into()
}

fn header(group: &str) -> SharedString {
    format!("snippet-group-{group}").into()
}

/// 打开主窗口，切到侧边栏的“快捷命令”页。
fn open_snippets(cx: &mut TestAppContext) -> AnyWindowHandle {
    let handle = open_workspace(cx);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("sidebar-pages").click(1usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    handle
}

/// 从 `from` 的中间拖到 `to` 上相对高度 `y` 处放下（0 在上沿）。
fn drag_onto(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    from: SharedString,
    to: SharedString,
    y: f32,
) {
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let start = window.find(from).bounds().center();
        let target = window.find(to).bounds();
        let end = point(target.center().x, target.top() + target.size.height * y);
        window.drag(start, end, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

/// 点开着的弹出菜单中文字为 `label` 的一项。
fn click_menu_item(cx: &mut TestAppContext, handle: AnyWindowHandle, label: &str) {
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let index = {
            let menu = window.within("popup-menu");
            (0..30usize).find(|i| {
                menu.try_find(*i)
                    .is_some_and(|item| item.label() == Some(label))
            })
        };
        let index = index.unwrap_or_else(|| panic!("菜单中应有“{label}”"));
        window.within("popup-menu").click(index, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

/// 右键分组标题，点菜单中的 `label`。
fn group_menu(cx: &mut TestAppContext, handle: AnyWindowHandle, group: &str, label: &str) {
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.right_click(header(group), cx);
    })
    .unwrap();
    cx.run_until_parked();
    click_menu_item(cx, handle, label);
}

// 组内拖到前面；拖到别组的快捷命令后面即改分组；拖到分组标题上排到那一组的最后
#[gpui_kit::test]
fn drag_sorts_within_and_across_groups(cx: &mut TestAppContext) {
    let handle = open_snippets(cx);
    let a1 = add(cx, "a", "a1");
    let a2 = add(cx, "a", "a2");
    let a3 = add(cx, "a", "a3");
    let b1 = add(cx, "b", "b1");
    cx.run_until_parked();
    assert_eq!(layout(cx), ["a: a1 a2 a3", "b: b1"]);

    drag_onto(cx, handle, row(&a3), row(&a1), 0.25);
    assert_eq!(layout(cx), ["a: a3 a1 a2", "b: b1"], "放在 a1 前面");

    drag_onto(cx, handle, row(&a1), row(&b1), 0.75);
    assert_eq!(
        layout(cx),
        ["a: a3 a2", "b: b1 a1"],
        "放在 b1 后面，改到分组 b"
    );
    let moved = cx.update(|cx| AppState::store(cx).get::<Snippet>(&a1).unwrap());
    assert_eq!(moved.doc.group, "b");

    drag_onto(cx, handle, row(&a2), header("b"), 0.5);
    assert_eq!(layout(cx), ["a: a3", "b: b1 a1 a2"], "放到标题上排到最后");
}

// 拖动分组标题调整分组的先后，存进设置中的分组顺序；未分组固定在最前
#[gpui_kit::test]
fn dragging_group_headers_reorders_groups(cx: &mut TestAppContext) {
    let handle = open_snippets(cx);
    add(cx, "", "n1");
    for group in ["a", "b", "c"] {
        add(cx, group, &format!("{group}1"));
    }
    // 先都折叠起来：拖分组时各组画成折叠的，拖动开始前后行的位置不变
    cx.update(|cx| {
        AppState::ui(cx).update(cx, |ui, cx| {
            ui.update(
                |state| {
                    for group in ["", "a", "b", "c"] {
                        state.collapsed_groups.insert(collapsed_key(group));
                    }
                },
                cx,
            )
        })
    });
    cx.run_until_parked();

    drag_onto(cx, handle, header("c"), header("a"), 0.2);
    assert_eq!(layout(cx), ["-: n1", "c: c1", "a: a1", "b: b1"]);
    let order = cx.update(|cx| AppState::store(cx).settings().snippets.group_order);
    assert_eq!(order, ["c", "a", "b"]);

    drag_onto(cx, handle, header("a"), header("b"), 0.8);
    assert_eq!(layout(cx), ["-: n1", "c: c1", "b: b1", "a: a1"]);

    // 拖到未分组的标题上不接受
    drag_onto(cx, handle, header("a"), header(""), 0.2);
    assert_eq!(layout(cx), ["-: n1", "c: c1", "b: b1", "a: a1"]);
}

// 右键分组“重命名分组…”：不重名时改名（本机的折叠状态、快捷命令栏选中的分组跟着改），重名时并进已有分组
#[gpui_kit::test]
fn rename_group_from_its_menu(cx: &mut TestAppContext) {
    let handle = open_snippets(cx);
    add(cx, "a", "a1");
    add(cx, "b", "b1");
    add(cx, "b", "b2");
    cx.update(|cx| {
        AppState::ui(cx).update(cx, |ui, cx| {
            ui.update(
                |state| {
                    state.collapsed_groups.insert(collapsed_key("a"));
                    state.snippet_bar_group = Some("a".into());
                },
                cx,
            )
        })
    });
    cx.run_until_parked();

    group_menu(cx, handle, "a", "重命名分组…");
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("snippet-group-rename").value(),
            Some("a"),
            "填好原来的名称"
        );
        // 原来的名称已全选：直接输入即替换
        window.input("新", cx);
        window.press("enter", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(layout(cx), ["b: b1 b2", "新: a1"]);
    cx.update(|cx| {
        let ui = AppState::ui(cx);
        let state = ui.read(cx).state();
        assert!(state.collapsed_groups.contains(&collapsed_key("新")));
        assert!(!state.collapsed_groups.contains(&collapsed_key("a")));
        assert_eq!(state.snippet_bar_group.as_deref(), Some("新"));
    });

    group_menu(cx, handle, "新", "重命名分组…");
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.input("b", cx);
        window.press("enter", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(layout(cx), ["b: b1 b2 a1"], "并进 b，接在原有的后面");
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("snippet-group-rename").is_none(),
            "对话框已关闭"
        );
    })
    .unwrap();
}

// 右键分组“删除分组…”：确认框写明条数；默认移到未分组，也可以选一并删除
#[gpui_kit::test]
fn delete_group_both_ways(cx: &mut TestAppContext) {
    let handle = open_snippets(cx);
    add(cx, "a", "a1");
    add(cx, "", "n1");
    add(cx, "b", "b1");
    add(cx, "a", "a2");
    cx.run_until_parked();

    group_menu(cx, handle, "a", "删除分组…");
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let choices = window.within("snippet-group-delete");
        assert_eq!(
            choices.find(0usize).label(),
            Some("把其中的 2 条快捷命令移到“未分组”")
        );
        assert_eq!(choices.find(0usize).checked(), Some(true), "默认移到未分组");
        assert_eq!(
            choices.find(1usize).label(),
            Some("连同其中的 2 条快捷命令一起删除")
        );
        window.click("ok", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(layout(cx), ["-: n1 a1 a2", "b: b1"]);

    group_menu(cx, handle, "b", "删除分组…");
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("snippet-group-delete").click(1usize, cx);
        window.render_frame(cx);
        window.click("ok", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(layout(cx), ["-: n1 a1 a2"]);
    assert_eq!(
        cx.update(|cx| AppState::store(cx).list::<Snippet>().len()),
        3,
        "b 中的快捷命令一并删除"
    );
}

// 编辑快捷命令时，分组可以从已有的分组中选
#[gpui_kit::test]
fn editor_picks_an_existing_group(cx: &mut TestAppContext) {
    let handle = open_snippets(cx);
    let s1 = add(cx, "", "s1");
    add(cx, "运维", "s2");
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        super::editor::open_edit(s1.clone(), window, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("snippet-group-choices", cx);
    })
    .unwrap();
    cx.run_until_parked();
    click_menu_item(cx, handle, "运维");
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("snippet-group").value(), Some("运维"));
        window.click("snippet-save", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let saved = cx.update(|cx| AppState::store(cx).get::<Snippet>(&s1).unwrap());
    assert_eq!(saved.doc.group, "运维");
}
