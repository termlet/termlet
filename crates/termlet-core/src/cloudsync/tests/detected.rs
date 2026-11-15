//! 主机识别出的系统（`detected/host/…` 记录）：随主机同步到其他设备，但不算对主机的修改。

use serde_json::json;

use super::*;
use crate::model::RemoteOs;

fn detected_key(id: &HostId) -> String {
    format!("detected/host/{id}")
}

fn set_os(dev: &Dev, id: &HostId, os: RemoteOs) {
    assert!(dev.store().set_host_os(id, Some(os)).unwrap());
}

fn os_of(dev: &Dev, id: &HostId) -> Option<String> {
    dev.host(id).doc.os
}

/// 写入识别结果不改主机的修改时间与主机记录；同步后另一台设备也有，同样不改修改时间。
#[test]
fn detected_os_syncs_without_touching_the_host() {
    let (a, b, dir) = pair();
    let id = a.first_host();
    let before = a.host(&id);
    let host_v = read_doc(dir.path()).records[&host_key(&id)].v.clone();

    set_os(&a, &id, RemoteOs::Alpine);
    assert!(
        !a.store().set_host_os(&id, Some(RemoteOs::Alpine)).unwrap(),
        "相同时不写"
    );
    let after = a.host(&id);
    assert_eq!(after.updated_at, before.updated_at);
    assert_eq!(after.doc.remote_os(), Some(RemoteOs::Alpine));

    let res = a.sync();
    assert_eq!(res.pushed, 1, "只上传识别结果：{res:?}");
    let doc = read_doc(dir.path());
    assert_eq!(doc.records[&host_key(&id)].v, host_v, "主机记录不变");
    assert!(
        record_value(&doc, &host_key(&id))["doc"]
            .get("os")
            .is_none()
    );
    assert_eq!(
        record_value(&doc, &detected_key(&id)),
        json!({"os": "alpine"})
    );

    let b_before = b.host(&id).updated_at;
    let res = b.sync();
    assert!(res.conflicts.is_empty(), "{res:?}");
    assert_eq!(os_of(&b, &id).as_deref(), Some("alpine"));
    assert_eq!(b.host(&id).updated_at, b_before);
    // 之后两边都没有要同步的
    assert_eq!(b.sync().pushed, 0);
    assert_eq!(a.sync().pulled, 0);
}

/// 一台设备识别出系统、另一台同时修改了这台主机：不冲突，修改与识别结果两边都有（谁先同步都一样）。
#[test]
fn detected_os_never_conflicts_with_an_edit() {
    for detecting_first in [true, false] {
        let (a, b, _dir) = pair();
        let id = a.first_host();
        set_os(&a, &id, RemoteOs::Ubuntu);
        b.rename_host(&id, "b 改的名字");
        let (first, second) = if detecting_first { (&a, &b) } else { (&b, &a) };
        let res = first.sync();
        assert!(res.conflicts.is_empty(), "{res:?}");
        let res = second.sync();
        assert!(res.conflicts.is_empty(), "{res:?}");
        first.sync();
        for dev in [&a, &b] {
            let host = dev.host(&id);
            assert_eq!(host.doc.name, "b 改的名字");
            assert_eq!(host.doc.os.as_deref(), Some("ubuntu"));
            assert!(dev.sync.status().conflicts.is_empty());
        }
    }
}

/// 编辑对话框里是打开时的旧内容（还没有识别结果）：保存时保留识别结果。同步写入另一台设备的主机修改时也保留。
#[test]
fn edits_keep_the_detected_os() {
    let (a, b, _dir) = pair();
    let id = a.first_host();
    let stale = a.host(&id).doc;
    set_os(&a, &id, RemoteOs::Debian);
    let mut edited = stale;
    edited.name = "新名字".into();
    a.store().update(&id, edited).unwrap();
    assert_eq!(os_of(&a, &id).as_deref(), Some("debian"));

    // b 也识别出了（相同），之后从 a 拉取改名：条目记录中没有识别结果，b 的保留
    set_os(&b, &id, RemoteOs::Debian);
    a.sync();
    let res = b.sync();
    assert!(res.conflicts.is_empty(), "{res:?}");
    assert_eq!(b.host(&id).doc.name, "新名字");
    assert_eq!(os_of(&b, &id).as_deref(), Some("debian"));
}

/// 另一台设备新加的主机连同识别结果一起到达（识别结果的记录键排在主机前面，要等主机写好再写）。
#[test]
fn new_host_arrives_with_its_os() {
    let (a, b, _dir) = pair();
    let id = a.add_host("h2", "10.0.0.2", "p2");
    set_os(&a, &id, RemoteOs::Rocky);
    a.sync();
    let res = b.sync();
    assert!(res.conflicts.is_empty(), "{res:?}");
    assert_eq!(os_of(&b, &id).as_deref(), Some("rocky"));
}

/// 更新的版本识别出、这个版本不认识的系统：原样保存、当作没有识别，不当作本机的修改改掉云端的。
#[test]
fn unknown_os_from_newer_version_is_kept() {
    let (a, b, dir) = pair();
    let id = a.first_host();
    let mut doc = read_doc(dir.path());
    put_record(&mut doc, &detected_key(&id), &json!({"os": "nixos"}));
    write_doc(dir.path(), &doc);
    b.sync();
    let host = b.host(&id);
    assert_eq!(host.doc.os.as_deref(), Some("nixos"));
    assert_eq!(host.doc.remote_os(), None);
    assert_eq!(b.sync().pushed, 0);
    assert_eq!(
        record_value(&read_doc(dir.path()), &detected_key(&id)),
        json!({"os": "nixos"})
    );
    // 格式不对的记录不写入，也不删除云端的
    let mut doc = read_doc(dir.path());
    put_record(&mut doc, &detected_key(&id), &json!({"os": "Bad Key"}));
    write_doc(dir.path(), &doc);
    b.sync();
    assert_eq!(b.host(&id).doc.os.as_deref(), Some("nixos"));
    assert_eq!(
        record_value(&read_doc(dir.path()), &detected_key(&id)),
        json!({"os": "Bad Key"})
    );
}

/// 一边删除主机、另一边重新识别出不同的系统：主机照常删除（识别结果不让它留下），也不列冲突、不在云端留下残余。
#[test]
fn detected_os_does_not_keep_a_deleted_host() {
    for deleting_first in [true, false] {
        let (a, b, dir) = pair();
        let id = a.first_host();
        set_os(&a, &id, RemoteOs::Alpine);
        a.sync();
        b.sync();
        a.delete_host(&id);
        set_os(&b, &id, RemoteOs::Debian);
        let (first, second) = if deleting_first { (&a, &b) } else { (&b, &a) };
        for dev in [first, second, first] {
            let res = dev.sync();
            assert!(res.conflicts.is_empty(), "{res:?}");
        }
        for dev in [&a, &b] {
            assert!(dev.store().get::<Host>(&id).is_none(), "主机应已删除");
        }
        let doc = read_doc(dir.path());
        assert!(!doc.records.contains_key(&host_key(&id)));
        assert!(
            !doc.records.contains_key(&detected_key(&id)),
            "云端不留识别结果"
        );
    }
}

/// 一边删除主机、另一边修改了它：保留修改的同时，识别结果随主机一起回来。
#[test]
fn kept_host_comes_back_with_its_os() {
    let (a, b, _dir) = pair();
    let id = a.first_host();
    set_os(&a, &id, RemoteOs::Fedora);
    a.sync();
    b.sync();
    a.delete_host(&id);
    b.rename_host(&id, "还要用");
    b.sync();
    let res = a.sync();
    assert_eq!(
        res.conflicts.len(),
        1,
        "主机本身的删改冲突照常列出：{res:?}"
    );
    let host = a.host(&id);
    assert_eq!(host.doc.name, "还要用");
    assert_eq!(host.doc.os.as_deref(), Some("fedora"));
    b.sync();
    assert_eq!(os_of(&b, &id).as_deref(), Some("fedora"));
}
