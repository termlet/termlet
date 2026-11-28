//! 快捷命令的顺序经同步：拖动只改被拖的那一条（与主机相同，按条目合并），分组顺序是单独的一组设置。

use std::time::Duration;

use super::*;
use crate::cloudsync::RecordKind;
use crate::id::SnippetId;
use crate::model::Snippet;

fn add(dev: &Dev, group: &str, name: &str) -> SnippetId {
    dev.store()
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
}

/// 按显示顺序：每个分组一行，`分组: 名称 名称`（未分组写作 `-`）。
fn layout(dev: &Dev) -> Vec<String> {
    let store = dev.store();
    let list = store.list::<Snippet>();
    let order = store.settings().snippets.group_order;
    crate::snippet_order::grouped(&list, &order)
        .into_iter()
        .map(|(group, members)| {
            let names: Vec<&str> = members.iter().map(|s| s.doc.name.as_str()).collect();
            let group = if group.is_empty() { "-" } else { &group };
            format!("{group}: {}", names.join(" "))
        })
        .collect()
}

fn set_group_order(dev: &Dev, order: &[&str]) {
    dev.store()
        .update_settings(|s| s.snippets.group_order = order.iter().map(|g| g.to_string()).collect())
        .unwrap();
}

// 组内拖、跨组拖、调分组顺序、改名后，另一台的顺序与分组相同
#[test]
fn dragged_order_reaches_the_other_device() {
    let (a, b, _dir) = pair();
    let a1 = add(&a, "a", "a1");
    let a2 = add(&a, "a", "a2");
    let b1 = add(&a, "b", "b1");
    add(&a, "c", "c1");
    a.sync();
    b.sync();
    assert_eq!(layout(&b), ["a: a1 a2", "b: b1", "c: c1"]);

    let store = a.store();
    store
        .move_snippet(&a2, "a", Position::Before(a1.clone()))
        .unwrap();
    store
        .move_snippet(&a1, "b", Position::Before(b1.clone()))
        .unwrap();
    set_group_order(&a, &["c", "b", "a"]);
    store.rename_snippet_group("c", "丙").unwrap();
    let expected = ["丙: c1", "b: a1 b1", "a: a2"];
    assert_eq!(layout(&a), expected);
    let res = a.sync();
    assert!(res.conflicts.is_empty(), "{res:?}");
    let res = b.sync();
    assert!(res.conflicts.is_empty() && res.pulled > 0, "{res:?}");
    assert_eq!(layout(&b), expected);
    let names = |dev: &Dev| -> Vec<String> {
        dev.store()
            .snippets_in_order()
            .into_iter()
            .map(|s| s.doc.name)
            .collect()
    };
    assert_eq!(names(&b), names(&a));
}

// 两台各拖了不同的快捷命令：按条目合并，两边的拖动都保留，没有冲突
#[test]
fn drags_on_both_devices_merge() {
    let (a, b, _dir) = pair();
    let ids: Vec<SnippetId> = ["s1", "s2", "s3", "s4"]
        .iter()
        .map(|n| add(&a, "g", n))
        .collect();
    a.sync();
    b.sync();
    // a：s4 拖到最前；b：s2 拖到 s3 后面
    a.store()
        .move_snippet(&ids[3], "g", Position::Before(ids[0].clone()))
        .unwrap();
    b.store()
        .move_snippet(&ids[1], "g", Position::After(ids[2].clone()))
        .unwrap();
    a.sync();
    let res = b.sync();
    assert!(res.conflicts.is_empty(), "{res:?}");
    a.sync();
    assert_eq!(layout(&a), ["g: s4 s1 s3 s2"]);
    assert_eq!(layout(&b), layout(&a));
}

// a 拖动分组的顺序，b 同时新建了一个分组：两边都按 a 排好的顺序，新分组按名称排在后面
#[test]
fn group_order_and_a_new_group_merge() {
    let (a, b, _dir) = pair();
    add(&a, "甲", "a1");
    add(&a, "乙", "b1");
    add(&a, "丙", "c1");
    a.sync();
    b.sync();
    set_group_order(&a, &["丙", "甲", "乙"]);
    add(&b, "丁", "d1");
    add(&b, "", "n1");
    a.sync();
    let res = b.sync();
    assert!(res.conflicts.is_empty(), "{res:?}");
    a.sync();
    let expected = ["-: n1", "丙: c1", "甲: a1", "乙: b1", "丁: d1"];
    assert_eq!(layout(&a), expected);
    assert_eq!(layout(&b), expected);
}

// 两台都拖了分组的顺序：分组顺序整组按修改时间合并，较新的胜出，列为冲突
#[test]
fn group_order_on_both_devices_newer_wins() {
    let (a, b, _dir) = pair();
    add(&a, "x", "x1");
    add(&a, "y", "y1");
    add(&a, "z", "z1");
    a.sync();
    b.sync();
    set_group_order(&a, &["z", "y", "x"]);
    std::thread::sleep(Duration::from_millis(5));
    set_group_order(&b, &["y", "x", "z"]);
    a.sync();
    let res = b.sync();
    assert_eq!(res.conflicts.len(), 1, "{res:?}");
    assert_eq!(res.conflicts[0].kind, RecordKind::Settings);
    assert_eq!(res.conflicts[0].name, "snippets");
    a.sync();
    let expected = ["y: y1", "x: x1", "z: z1"];
    assert_eq!(layout(&a), expected);
    assert_eq!(layout(&b), expected);
}
