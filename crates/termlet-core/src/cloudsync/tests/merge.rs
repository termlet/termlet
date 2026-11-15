//! 合并规则的单元测试（不经过存储与同步位置）。

use serde_json::{Value, json};

use crate::cloudsync::Side;
use crate::cloudsync::merge::{Base, Bases, Mode, PlanOpts, make_plan, next_bases, too_many};
use crate::cloudsync::records::{
    Data, Local, Locals, Remote, Remotes, canonical, hash_of, version, version_time,
};

fn local(value: Value, changed: i64) -> Local {
    let data = Data::of(&value);
    let hash = data.hash();
    Local {
        data,
        hash,
        changed,
    }
}

fn remote(value: Value, at: i64) -> Remote {
    let data = Data::of(&value);
    let v = version(at, "other0device", &data.hash());
    Remote::new(v, data)
}

fn base_of(r: &Remote) -> Base {
    Base::new(&r.v, &r.hash)
}

fn host(name: &str) -> Value {
    json!({"created": 1, "parent": null, "sort": "V", "doc": {"name": name, "address": "10.0.0.1"}})
}

fn opts(mode: Mode) -> PlanOpts {
    PlanOpts::new(mode, "me", 2_000, "1.0")
}

fn one<K: Into<String>, V>(key: K, value: V) -> std::collections::BTreeMap<String, V> {
    std::collections::BTreeMap::from([(key.into(), value)])
}

#[test]
fn canonical_json_sorts_keys_and_versions_carry_time() {
    let text = canonical(&json!({"b": 1, "a": {"d": [{"y": 1, "x": 2}], "c": null}}));
    assert_eq!(text, r#"{"a":{"c":null,"d":[{"x":2,"y":1}]},"b":1}"#);
    assert_eq!(hash_of(&text).len(), 32);
    let v = version(1234, "dev1", "abcdef0123456789");
    assert_eq!(v, "0000000001234-dev1-abcdef01");
    assert_eq!(version_time(&v), 1234);
    assert_eq!(version_time("坏的"), 0);
}

#[test]
fn too_many_needs_five_and_thirty_percent() {
    assert!(!too_many(4, 4));
    assert!(too_many(5, 10));
    assert!(too_many(3 * 2 + 1, 20));
    assert!(!too_many(5, 20));
}

// 只有一边改了的采用那一边；两边改成一样的只更新基准
#[test]
fn one_sided_changes_win() {
    let r = remote(host("h1"), 1_000);
    let bases: Bases = one("host/h_1", base_of(&r));
    // 本机改了
    let locals: Locals = one("host/h_1", local(host("本机改的"), 1_500));
    let plan = make_plan(
        &locals,
        &one("host/h_1", r.clone()),
        &bases,
        &opts(Mode::Normal),
    );
    assert!(plan.to_local.is_empty());
    assert_eq!(plan.to_remote.len(), 1);
    assert!(plan.conflicts.is_empty());
    // 云端改了
    let newer = remote(host("云端改的"), 1_800);
    let locals: Locals = one("host/h_1", local(host("h1"), 900));
    let plan = make_plan(
        &locals,
        &one("host/h_1", newer.clone()),
        &bases,
        &opts(Mode::Normal),
    );
    assert_eq!(plan.to_local.len(), 1);
    assert!(plan.to_remote.is_empty());
    // 两边改成了一样的
    let locals: Locals = one("host/h_1", local(host("云端改的"), 1_900));
    let plan = make_plan(
        &locals,
        &one("host/h_1", newer),
        &bases,
        &opts(Mode::Normal),
    );
    assert!(plan.to_local.is_empty() && plan.to_remote.is_empty() && plan.conflicts.is_empty());
}

// 两边都改了：较新的胜出；本机从没改过（修改时间为 0，默认值）的比任何修改都旧
#[test]
fn conflicts_newer_wins_and_defaults_lose() {
    let old = remote(host("h1"), 1_000);
    let bases: Bases = one("host/h_1", base_of(&old));
    let theirs = remote(host("对方的"), 1_500);
    let plan = make_plan(
        &one("host/h_1", local(host("本机的"), 1_600)),
        &one("host/h_1", theirs.clone()),
        &bases,
        &opts(Mode::Normal),
    );
    assert_eq!(plan.conflicts.len(), 1);
    assert_eq!(plan.conflicts[0].winner, Side::Local);
    assert_eq!(plan.conflicts[0].name, "本机的");
    assert!(plan.conflicts[0].loser.is_some());
    assert_eq!(plan.to_remote.len(), 1);
    let plan = make_plan(
        &one("host/h_1", local(host("本机的"), 1_400)),
        &one("host/h_1", theirs),
        &bases,
        &opts(Mode::Normal),
    );
    assert_eq!(plan.conflicts[0].winner, Side::Remote);
    assert_eq!(plan.to_local.len(), 1);

    // 首次合并：没有基准，设置组本机从没改过
    let plan = make_plan(
        &one("setting/terminal", local(json!({"fontSize": 14}), 0)),
        &one("setting/terminal", remote(json!({"fontSize": 18}), 5)),
        &Bases::new(),
        &opts(Mode::Merge),
    );
    assert_eq!(plan.conflicts[0].winner, Side::Remote);
    assert_eq!(plan.conflicts[0].name, "terminal");
}

// 一边删除、一边修改：保留修改，并列出冲突
#[test]
fn delete_versus_modify_keeps_the_modification() {
    let old = remote(host("h1"), 1_000);
    let bases: Bases = one("host/h_1", base_of(&old));
    // 本机删除、云端修改
    let plan = make_plan(
        &Locals::new(),
        &one("host/h_1", remote(host("改了"), 1_500)),
        &bases,
        &opts(Mode::Normal),
    );
    assert_eq!(plan.to_local.len(), 1);
    assert!(plan.to_local[0].1.is_some());
    assert!(plan.conflicts[0].deleted && plan.conflicts[0].winner == Side::Remote);
    assert!(plan.conflicts[0].loser.is_none());
    assert_eq!(plan.local_deletes, 0);
    // 云端删除、本机修改
    let plan = make_plan(
        &one("host/h_1", local(host("改了"), 1_500)),
        &Remotes::new(),
        &bases,
        &opts(Mode::Normal),
    );
    assert!(plan.to_remote["host/h_1"].is_some());
    assert!(plan.conflicts[0].deleted && plan.conflicts[0].winner == Side::Local);
    // 两边都删除了
    let plan = make_plan(&Locals::new(), &Remotes::new(), &bases, &opts(Mode::Normal));
    assert!(plan.to_local.is_empty() && plan.to_remote.is_empty());
}

// 保留的条目连同它的秘密；删改冲突中保留的主机，下属的端口转发一并保留；仍被引用的分组不删除
#[test]
fn kept_items_bring_their_dependencies() {
    let h = remote(host("h1"), 1_000);
    let secret = remote(json!({"password": "p1"}), 1_000);
    let forward = remote(
        json!({"created": 1, "parent": null, "sort": "V", "doc": {"name": "web", "host": "h_1", "bindPort": 8080}}),
        1_000,
    );
    let bases: Bases = [
        ("host/h_1".to_string(), base_of(&h)),
        ("secret/host/h_1".to_string(), base_of(&secret)),
        ("forward/f_1".to_string(), base_of(&forward)),
    ]
    .into();
    // 本机删除了主机（连同秘密与转发），云端修改了主机：三者都从云端恢复
    let remote_all: Remotes = [
        ("host/h_1".to_string(), remote(host("改了"), 1_500)),
        ("secret/host/h_1".to_string(), secret.clone()),
        ("forward/f_1".to_string(), forward.clone()),
    ]
    .into();
    let plan = make_plan(&Locals::new(), &remote_all, &bases, &opts(Mode::Normal));
    let restored: Vec<&str> = plan
        .to_local
        .iter()
        .filter(|(_, r)| r.is_some())
        .map(|(k, _)| k.as_str())
        .collect();
    assert_eq!(restored, ["forward/f_1", "host/h_1", "secret/host/h_1"]);
    assert_eq!(plan.local_deletes, 0);
    assert_eq!(plan.conflicts.len(), 1, "秘密不单独列出");

    // 本机删除了分组，另一台设备把主机移进了这个分组：分组写回本机，不从云端删除
    let g = remote(
        json!({"created": 1, "parent": null, "sort": "V", "doc": {"name": "G"}}),
        1_000,
    );
    let moved = json!({"created": 1, "parent": "g_1", "sort": "V", "doc": {"name": "h1", "address": "10.0.0.1"}});
    let bases: Bases = [
        ("group/g_1".to_string(), base_of(&g)),
        ("host/h_1".to_string(), base_of(&h)),
    ]
    .into();
    let locals: Locals = one("host/h_1", local(host("h1"), 900));
    let remote_side: Remotes = [
        ("group/g_1".to_string(), g.clone()),
        ("host/h_1".to_string(), remote(moved, 1_500)),
    ]
    .into();
    let plan = make_plan(&locals, &remote_side, &bases, &opts(Mode::Normal));
    assert!(
        plan.to_local
            .iter()
            .any(|(k, r)| k == "group/g_1" && r.is_some())
    );
    assert!(
        plan.to_local
            .iter()
            .any(|(k, r)| k == "host/h_1" && r.is_some())
    );
    assert!(!plan.to_remote.contains_key("group/g_1"));
    assert_eq!(plan.local_deletes + plan.remote_deletes, 0);
}

// 本机的条目没有秘密、云端有，而本机从没同步过它（重置保险库后重新设置同步）：任何方式下都采用云端的；
// 同步过的（有基准）照常：本机删除了就删除云端的
#[test]
fn missing_secrets_after_vault_reset_take_remote() {
    let secret = remote(json!({"password": "p1"}), 1_000);
    let locals: Locals = one("host/h_1", local(host("h1"), 900));
    let remote_side: Remotes = [
        ("host/h_1".to_string(), remote(host("h1"), 900)),
        ("secret/host/h_1".to_string(), secret.clone()),
    ]
    .into();
    for mode in [Mode::Normal, Mode::Merge, Mode::Local, Mode::Remote] {
        let plan = make_plan(&locals, &remote_side, &Bases::new(), &opts(mode));
        assert!(
            plan.to_local
                .iter()
                .any(|(k, r)| k == "secret/host/h_1" && r.is_some()),
            "{mode:?}"
        );
        assert!(!plan.to_remote.contains_key("secret/host/h_1"), "{mode:?}");
    }
    // 归属的条目本机没有（以本机为准时随条目一起从云端删除）
    let plan = make_plan(
        &Locals::new(),
        &remote_side,
        &Bases::new(),
        &opts(Mode::Local),
    );
    assert!(plan.to_remote["secret/host/h_1"].is_none());
    // 有基准：本机删除了这台主机的秘密（例如改用 agent 认证）
    let bases: Bases = [
        ("host/h_1".to_string(), base_of(&remote_side["host/h_1"])),
        ("secret/host/h_1".to_string(), base_of(&secret)),
    ]
    .into();
    let plan = make_plan(&locals, &remote_side, &bases, &opts(Mode::Normal));
    assert!(plan.to_remote["secret/host/h_1"].is_none());
}

// 以云端为准 / 以本机为准：不同的一律采用那一边
#[test]
fn remote_and_local_modes() {
    let locals: Locals = [
        ("host/h_1".to_string(), local(host("本机"), 900)),
        ("host/h_2".to_string(), local(host("只在本机"), 900)),
    ]
    .into();
    let remote_side: Remotes = [
        ("host/h_1".to_string(), remote(host("云端"), 800)),
        ("host/h_3".to_string(), remote(host("只在云端"), 800)),
    ]
    .into();
    let plan = make_plan(&locals, &remote_side, &Bases::new(), &opts(Mode::Remote));
    assert_eq!(plan.to_local.len(), 3);
    assert_eq!(plan.local_deletes, 1);
    let plan = make_plan(&locals, &remote_side, &Bases::new(), &opts(Mode::Local));
    assert_eq!(plan.to_remote.len(), 3);
    assert_eq!(plan.remote_deletes, 1);
    // 合并：只有一边有的都保留
    let plan = make_plan(&locals, &remote_side, &Bases::new(), &opts(Mode::Merge));
    assert_eq!(plan.local_deletes + plan.remote_deletes, 0);
    assert!(plan.to_remote.contains_key("host/h_2"));
    assert!(plan.to_local.iter().any(|(k, _)| k == "host/h_3"));
}

// 本机不认识的记录（来自更新版本）：留在云端，不写入本机，也不当作本机删除了它；基准记为没有写入
#[test]
fn unknown_records_stay_in_the_cloud() {
    let remote_side: Remotes = [
        ("setting/future".to_string(), remote(json!({"x": 1}), 1_000)),
        ("tunnel/t_1".to_string(), remote(json!({"x": 1}), 1_000)),
    ]
    .into();
    let plan = make_plan(
        &Locals::new(),
        &remote_side,
        &Bases::new(),
        &opts(Mode::Normal),
    );
    assert!(plan.to_local.is_empty() && plan.to_remote.is_empty());
    let bases = next_bases(
        &remote_side,
        &Locals::new(),
        &Bases::new(),
        &Default::default(),
        "1.0",
    );
    assert!(
        bases
            .values()
            .all(|b| b.unapplied_parts() == Some(("", "1.0")))
    );
    // 以本机为准时同样不删除
    let plan = make_plan(&Locals::new(), &remote_side, &bases, &opts(Mode::Local));
    assert!(plan.to_remote.is_empty());
}

// 没有写入本机的设置项的基准——两边都没有修改时不重试；用户之后改了按本机的修改上传；
// 升级之后重新尝试写入云端的值（例如旧版本中不合法、升级后合法了）
#[test]
fn unapplied_setting_base() {
    const K: &str = "setting/terminal";
    let old = local(json!({"fontSize": 14}), 100);
    let edited = local(json!({"fontSize": 16}), 200);
    let r = remote(json!({"fontSize": "big"}), 1_000);
    let cases = [
        (
            "两边都没有修改",
            old.clone(),
            Base::unapplied(&r.v, &old.hash, "1.0"),
            "1.0",
            0,
            0,
        ),
        (
            "用户修改了",
            edited,
            Base::unapplied(&r.v, &old.hash, "1.0"),
            "1.0",
            0,
            1,
        ),
        (
            "升级之后",
            old.clone(),
            Base::unapplied(&r.v, &old.hash, "1.0"),
            "1.1",
            1,
            0,
        ),
        (
            "旧版本不认识，升级之后",
            old.clone(),
            Base::unapplied(&r.v, "", "1.0"),
            "1.1",
            1,
            0,
        ),
        (
            "当时不认识，现在认识了（版本号没变）",
            old.clone(),
            Base::unapplied(&r.v, "", "1.0"),
            "1.0",
            1,
            0,
        ),
        (
            "两边相同",
            local(json!({"fontSize": "big"}), 300),
            Base::unapplied(&r.v, &old.hash, "1.0"),
            "1.1",
            0,
            0,
        ),
        (
            "云端又修改了",
            old.clone(),
            Base::unapplied("0000000000500-other0device-x", &old.hash, "1.0"),
            "1.0",
            1,
            0,
        ),
    ];
    for (name, l, b, app, to_local, to_remote) in cases {
        let o = PlanOpts::new(Mode::Normal, "me", 2_000, app);
        let plan = make_plan(&one(K, l), &one(K, r.clone()), &one(K, b), &o);
        assert_eq!(
            (plan.to_local.len(), plan.to_remote.len()),
            (to_local, to_remote),
            "{name}"
        );
    }
}

// 写入丢失：云端是本机删除前的版本时重新删除；本机后来写过的记录以本机的为准；云端没有了的记录不采纳删除
#[test]
fn lost_write_rules() {
    let h1 = remote(host("h1"), 1_000);
    let z = remote(host("z"), 1_000);
    let w = remote(host("w"), 1_000);
    let bases: Bases = [
        ("host/h_1".to_string(), base_of(&h1)),
        ("host/h_x".to_string(), base_of(&remote(host("x"), 1_200))),
    ]
    .into();
    let locals: Locals = [
        ("host/h_1".to_string(), local(host("h1-a"), 1_300)),
        ("host/h_x".to_string(), local(host("x"), 1_200)),
    ]
    .into();
    // 云端是旧文件：h1 的旧版本，本机已删除的 z、w（w 被其他设备改过），没有 x
    let w_modified = remote(host("w-b"), 1_400);
    let remote_side: Remotes = [
        ("host/h_1".to_string(), h1.clone()),
        ("host/h_z".to_string(), z.clone()),
        ("host/h_w".to_string(), w_modified),
    ]
    .into();
    let mut o = opts(Mode::Normal);
    o.lost = true;
    o.wrote_after = ["host/h_1".to_string()].into();
    o.deleted_after = [
        ("host/h_z".to_string(), z.v.clone()),
        ("host/h_w".to_string(), w.v.clone()),
    ]
    .into();
    let plan = make_plan(&locals, &remote_side, &bases, &o);
    assert!(
        plan.to_remote["host/h_1"].is_some(),
        "本机后来写过的以本机为准"
    );
    assert!(plan.to_remote["host/h_z"].is_none(), "本机删除的重新删除");
    assert!(
        plan.to_remote["host/h_x"].is_some(),
        "云端没有了的不采纳删除"
    );
    assert!(
        plan.to_local
            .iter()
            .any(|(k, r)| k == "host/h_w" && r.is_some()),
        "其他设备之后修改了的保留修改"
    );
    assert_eq!(plan.local_deletes, 0);
}
