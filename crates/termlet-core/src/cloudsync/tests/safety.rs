//! 读不到同步文件、写入丢失、删改冲突、引用完整性、设置写入失败、同步期间本机被修改等情况下不丢数据。

use serde_json::json;

use super::*;
use crate::cloudsync::{Answer, Pending, RecordKind, Side};
use crate::model::{Auth, Group, Jump, KeyEntry, KeySource, Kind, Profile, SecretField};
use crate::store::{Owner, Position};

fn has_secret(d: &Dev, owner: &Owner, field: SecretField) -> bool {
    d.store().secret(owner, field).unwrap().is_some()
}

// 同步文件夹不存在（U 盘没插、网盘目录被移走）时同步失败，本机数据不动
#[test]
fn missing_folder_is_unavailable() {
    let (a, _b, dir) = pair();
    let away = dir.path().with_extension("away");
    std::fs::rename(dir.path(), &away).unwrap();
    assert!(matches!(
        a.try_run().unwrap_err(),
        SyncError::FolderMissing { .. }
    ));
    assert_eq!(a.names(), ["h1"], "文件夹不存在时不应删除本机数据");
    assert!(!dir.path().exists(), "不应建立同步文件夹");
    assert!(matches!(
        a.sync.status().last_error,
        Some(SyncError::FolderMissing { .. })
    ));
    std::fs::rename(&away, dir.path()).unwrap();
    let res = a.sync();
    assert!(!res.reuploaded && res.pulled == 0, "{res:?}");
    assert_eq!(a.names(), ["h1"]);
    assert_eq!(a.sync.status().last_error, None, "成功后清除错误");
}

// 同步过而云端没有同步文件时以本机为准重新上传；其他设备随后照常合并
#[test]
fn missing_file_reuploads() {
    let (a, b, dir) = pair();
    let file = dir.path().join(FILE_NAME);
    let h2 = b.add_host("h2", "10.0.0.2", "p2");
    b.sync();
    a.sync();
    std::fs::remove_file(&file).unwrap();
    let res = a.sync();
    assert!(
        res.reuploaded && res.pulled == 0 && res.pushed > 0,
        "{res:?}"
    );
    assert_eq!(a.names(), ["h1", "h2"], "不应删除本机数据");
    assert_eq!(a.password(&h2).as_deref(), Some("p2"));
    assert!(!read_doc(dir.path()).records.is_empty(), "应重新上传");
    // b 在此期间新增的主机照常合并，原有的不被删除
    b.add_host("h3", "10.0.0.3", "p3");
    let res = b.sync();
    assert!(!res.lost_write, "{res:?}");
    a.sync();
    assert_eq!(a.names(), ["h1", "h2", "h3"]);
    assert_eq!(b.names(), ["h1", "h2", "h3"]);
    assert_eq!(b.password(&h2).as_deref(), Some("p2"));
    // 再同步时不再提示
    assert!(!a.sync().reuploaded);

    // 重新上传时保留设备列表：其他设备之后不会把自己写过的旧内容当作云端丢失的写入、覆盖重新上传中较新的修改
    b.sync();
    a.rename_host(&h2, "h2-a");
    std::fs::remove_file(&file).unwrap();
    assert!(a.sync().reuploaded);
    assert_eq!(
        read_doc(dir.path()).devices.len(),
        2,
        "重新上传应保留设备列表"
    );
    let res = b.sync();
    assert!(!res.lost_write, "{res:?}");
    assert_eq!(
        b.names(),
        ["h1", "h2-a", "h3"],
        "其他设备应收到重新上传中的修改"
    );
}

// 本机写到云端的内容被网盘用其他设备的旧文件覆盖、或云端回退到较早的版本时，
// 本机写入过的记录不采纳云端的删除与旧版本，本机删除了的记录（云端是本机删除的那个版本）重新删除；其他设备之后的
// 修改照常合并
#[test]
fn lost_write_keeps_local_changes() {
    let (a, b, dir) = pair();
    let file = dir.path().join(FILE_NAME);
    let z = a.add_host("z", "10.0.0.5", "pz");
    let w = a.add_host("w", "10.0.0.6", "pw");
    a.sync();
    b.sync();
    let old = std::fs::read(&file).unwrap();
    let h1 = a.first_host();
    let x = a.add_host("x", "10.0.0.9", "px");
    a.rename_host(&h1, "h1-a");
    a.delete_host(&z);
    a.delete_host(&w);
    a.sync();

    // b 在网盘把 a 的文件传过来之前写入（基于旧文件），网盘以后写入的为准；b 修改了 a 删除的 w
    std::fs::write(&file, &old).unwrap();
    let y = b.add_host("y", "10.0.0.8", "py");
    b.rename_host(&w, "w-b");
    b.sync();

    let res = a.sync();
    assert!(
        res.lost_write && res.pulled > 0,
        "应发现本机上次的写入丢失：{res:?}"
    );
    // z 重新删除；w 是 b 之后修改了的（不是 a 删除的那个版本），保留修改
    let want = ["h1-a", "w-b", "x", "y"];
    assert_eq!(a.names(), want);
    assert_eq!(a.password(&x).as_deref(), Some("px"));
    assert_eq!(a.password(&y).as_deref(), Some("py"));
    assert_eq!(a.password(&w).as_deref(), Some("pw"));
    assert!(
        !read_doc(dir.path()).records.contains_key(&host_key(&z)),
        "本机删除的主机应从云端重新删除"
    );
    b.sync();
    assert_eq!(b.names(), want);
    assert_eq!(b.password(&x).as_deref(), Some("px"));

    // 云端回退到较早的版本（有 h1、z、w 的旧版本）：a 写过的重新上传、删除的重新删除；b 的修改同样丢失了，由 b 重新上传
    std::fs::write(&file, &old).unwrap();
    let res = a.sync();
    assert!(res.lost_write, "{res:?}");
    assert_eq!(a.names(), ["h1-a", "w", "x", "y"]);
    assert!(
        !read_doc(dir.path()).records.contains_key(&host_key(&z)),
        "云端回退后本机删除的主机应重新删除"
    );
    let res = b.sync();
    assert!(res.lost_write, "{res:?}");
    assert_eq!(b.names(), want);
    a.sync();
    // 之后正常同步，不再当作写入丢失
    let res = a.sync();
    assert!(!res.lost_write && res.pulled == 0, "{res:?}");
    assert_eq!(a.names(), want);
    let res = b.sync();
    assert!(!res.lost_write, "{res:?}");
    assert_eq!(b.names(), want);
}

// 首次同步时云端还没有文件、以本机为准上传，写入前另一台设备先上传了：
// 重新拉取后两边都有数据，请用户选择，不覆盖
#[test]
fn first_sync_race_asks_choice() {
    let dir = sync_dir();
    let (a, b) = (Dev::new(), Dev::new());
    a.add_host("a1", "10.0.0.1", "p");
    b.add_host("b1", "10.0.0.2", "p");
    a.setup(dir.path());
    b.setup(dir.path());
    let bs = b.sync.clone();
    race_before_put(
        &a.sync,
        Box::pin(async move {
            bs.sync_now().await.unwrap();
        }),
    );
    assert!(matches!(
        a.run(),
        SyncOutcome::Pending(Pending::FirstSync { .. })
    ));
    let hosts = crate::cloudsync::records::counts(read_doc(dir.path()).records.keys()).hosts;
    assert_eq!(hosts, 1, "不应覆盖其他设备上传的数据");
}

// 一边删除主机、另一边修改了它，保留修改时它的密码、转发规则一并保留（两个方向）
#[test]
fn delete_modify_keeps_dependencies() {
    let (a, b, _dir) = pair();
    let h1 = a.first_host();
    add_forward(a.store(), &h1, "web", 18081);
    a.sync();
    b.sync();

    // b 删除、a 修改：a 保留修改
    b.delete_host(&h1);
    a.rename_host(&h1, "h1-改");
    b.sync();
    let res = a.sync();
    assert_eq!(res.conflicts.len(), 1, "{res:?}");
    assert!(res.conflicts[0].deleted && res.conflicts[0].winner == Side::Local);
    assert_eq!(a.password(&h1).as_deref(), Some("p1"));
    assert_eq!(a.forwards(), ["web"], "保留的主机应带着转发规则");
    b.sync();
    assert_eq!(b.names(), ["h1-改"]);
    assert_eq!(b.password(&h1).as_deref(), Some("p1"));
    assert_eq!(b.forwards(), ["web"], "另一台设备应收到完整的主机");

    // a 删除、b 修改：a 恢复 b 修改后的主机，连同密码与转发规则
    a.delete_host(&h1);
    b.rename_host(&h1, "h1-b");
    b.sync();
    let res = a.sync();
    assert_eq!(res.conflicts.len(), 1);
    assert_eq!(res.conflicts[0].winner, Side::Remote);
    assert_eq!(a.names(), ["h1-b"]);
    assert_eq!(a.password(&h1).as_deref(), Some("p1"));
    assert_eq!(a.forwards(), ["web"]);
    b.sync();
    assert_eq!(
        b.password(&h1).as_deref(),
        Some("p1"),
        "另一台设备上的密码与转发规则不应被删除"
    );
    assert_eq!(b.forwards(), ["web"]);
}

// 凭据档案、密钥的删改冲突，保留修改时它们的秘密一并保留
#[test]
fn delete_modify_keeps_credentials() {
    let (a, b, _dir) = pair();
    let profile = add_profile(a.store(), "运维", "ops-pass");
    let key = add_key(a.store(), "部署");
    a.sync();
    b.sync();

    b.store().delete::<Profile>(&profile).unwrap();
    b.store().delete::<KeyEntry>(&key).unwrap();
    let mut doc = a.store().get::<Profile>(&profile).unwrap().doc;
    doc.name = "运维-改".into();
    a.store().update(&profile, doc).unwrap();
    let mut doc = a.store().get::<KeyEntry>(&key).unwrap().doc;
    doc.name = "部署-改".into();
    a.store().update(&key, doc).unwrap();
    b.sync();
    let res = a.sync();
    assert_eq!(res.conflicts.len(), 2, "{res:?}");
    assert!(
        res.conflicts
            .iter()
            .all(|c| c.deleted && c.winner == Side::Local)
    );
    b.sync();
    for d in [&a, &b] {
        let profiles = d.store().list::<Profile>();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].doc.name, "运维-改");
        assert_eq!(d.store().get::<KeyEntry>(&key).unwrap().doc.name, "部署-改");
        assert!(
            has_secret(d, &Owner::profile(&profile), SecretField::Password),
            "凭据档案应带着秘密保留"
        );
        assert!(
            has_secret(d, &Owner::key(&key), SecretField::PrivateKey),
            "密钥应带着私钥保留"
        );
    }
}

// 一边删除分组、另一边把主机移入这个分组并先同步：分组被保留，每次同步都不出错
#[test]
fn referenced_group_is_kept() {
    let (a, b, _dir) = pair();
    let g = add_group(a.store(), "G", None);
    a.sync();
    b.sync();
    let h1 = a.first_host();

    a.store().delete::<Group>(&g).unwrap();
    b.store()
        .move_to::<Host>(&h1, Some(g.to_string()), Position::Last)
        .unwrap();
    b.sync();
    for _ in 0..2 {
        a.try_run().expect("同步不应因引用失败");
    }
    b.sync();
    for d in [&a, &b] {
        assert_eq!(
            d.host(&h1).parent.as_deref(),
            Some(g.as_str()),
            "主机应仍在分组中"
        );
        assert_eq!(d.store().list::<Group>().len(), 1);
    }
}

// 一边删除密钥、另一边让主机改用这把密钥并先同步：密钥连同私钥被保留
#[test]
fn referenced_key_is_kept() {
    let (a, b, _dir) = pair();
    let key = add_key(a.store(), "部署");
    a.sync();
    b.sync();
    let h1 = a.first_host();

    a.store().delete::<KeyEntry>(&key).unwrap();
    b.edit_host(&h1, |h| {
        h.auth = Auth::Key {
            source: KeySource::Library { key: key.clone() },
        }
    });
    b.sync();
    a.try_run().expect("同步不应因引用失败");
    b.sync();
    for d in [&a, &b] {
        assert_eq!(
            d.store().list::<KeyEntry>().len(),
            1,
            "主机使用的密钥应保留"
        );
        assert!(has_secret(d, &Owner::key(&key), SecretField::PrivateKey));
        assert!(matches!(d.host(&h1).doc.auth, Auth::Key { .. }));
    }
}

// 云端的记录引用了两边都没有的记录：写入时修复（引用改为空），修复后的记录上传，
// 之后照常同步
#[test]
fn dangling_reference_is_repaired() {
    let (a, b, dir) = pair();
    let h1 = a.first_host();
    let key = host_key(&h1);
    let mut doc = read_doc(dir.path());
    let mut value = record_value(&doc, &key);
    value["parent"] = json!("g_missing");
    value["doc"]["auth"] = json!({"type": "profile", "profile": "p_missing"});
    value["doc"]["jump"] = json!({"mode": "via", "host": "h_missing"});
    put_record(&mut doc, &key, &value);
    write_doc(dir.path(), &doc);

    a.try_run().expect("悬空的引用应被修复");
    let host = a.host(&h1);
    assert_eq!(host.parent, None);
    assert_eq!(host.doc.auth, Auth::Password);
    assert_eq!(host.doc.jump, Jump::Inherit);
    assert_eq!(a.password(&h1).as_deref(), Some("p1"));
    let uploaded = record_value(&read_doc(dir.path()), &key);
    assert!(
        uploaded["parent"].is_null(),
        "修复后的记录应上传：{uploaded}"
    );
    b.try_run().unwrap();
    let res = a.sync();
    assert_eq!((res.pulled, res.pushed), (0, 0), "修复之后不应再有变化");
    assert_eq!(b.host(&h1).doc.auth, Auth::Password);
}

// 云端的转发规则属于两边都没有的主机（引用不能为空）：写入时删除这条规则，删除同样上传
#[test]
fn orphan_forward_is_removed() {
    let (a, b, dir) = pair();
    let h1 = a.first_host();
    let rule = add_forward(a.store(), &h1, "web", 18082);
    a.sync();
    b.sync();

    let key = format!("forward/{rule}");
    let mut doc = read_doc(dir.path());
    let mut value = record_value(&doc, &key);
    value["doc"]["host"] = json!("h_missing");
    value["doc"]["name"] = json!("web-2");
    put_record(&mut doc, &key, &value);
    write_doc(dir.path(), &doc);

    a.try_run().expect("悬空的引用应被修复");
    assert!(a.forwards().is_empty(), "没有主机的转发规则应删除");
    assert!(
        !read_doc(dir.path()).records.contains_key(&key),
        "删除应上传"
    );
    b.sync();
    assert!(b.forwards().is_empty());
    assert_eq!(b.names(), ["h1"]);
}

// 合并后分组的上级、跳板机成环（两台设备各自把一个移进了另一个）：断开一处，不让分组从树中消失
#[test]
fn merged_cycles_are_broken() {
    let (a, b, _dir) = pair();
    let g1 = add_group(a.store(), "G1", None);
    let g2 = add_group(a.store(), "G2", None);
    let h1 = a.first_host();
    let h2 = a.add_host("h2", "10.0.0.2", "p");
    a.sync();
    b.sync();
    a.store()
        .move_to::<Group>(&g1, Some(g2.to_string()), Position::Last)
        .unwrap();
    a.edit_host(&h1, |h| h.jump = Jump::Via { host: h2.clone() });
    b.store()
        .move_to::<Group>(&g2, Some(g1.to_string()), Position::Last)
        .unwrap();
    b.edit_host(&h2, |h| h.jump = Jump::Via { host: h1.clone() });
    a.sync();
    b.sync();
    a.sync();
    for d in [&a, &b] {
        let parents: Vec<Option<String>> = d
            .store()
            .list::<Group>()
            .into_iter()
            .map(|g| g.parent)
            .collect();
        assert!(
            parents.iter().any(Option::is_none),
            "环应被断开：{parents:?}"
        );
        let jumps = [d.host(&h1).doc.jump, d.host(&h2).doc.jump];
        assert!(
            jumps.contains(&Jump::Inherit),
            "跳板机的环应被断开：{jumps:?}"
        );
    }
}

// 重置保险库后本机的秘密是空的：重新设置同步后，无论选择哪种方式，都采用云端
// 完好的秘密，不把"没有秘密"推上云端；选择以本机为准时其余的照常以本机为准（云端独有的记录删除，本机较旧的修改也上传）
#[test]
fn emptied_secrets_take_remote() {
    let (a, b, dir) = pair();
    let key = add_key(a.store(), "部署");
    a.sync();
    b.sync();
    let h1 = a.first_host();

    let mut b = b;
    let mut merged = None;
    for answer in [Answer::Merge, Answer::UseLocal] {
        b = b.reset_vault();
        assert_eq!(
            b.try_run().unwrap_err(),
            SyncError::NotConfigured,
            "重置保险库后应重新设置同步"
        );
        assert_eq!(b.sync.status().config, None);
        assert!(!has_secret(&b, &Owner::host(&h1), SecretField::Password));
        // 其他设备在此期间新增主机、修改 h1（比 b 的新）
        let tag = format!("{answer:?}");
        let only = a.add_host(&format!("only-{tag}"), "10.0.0.7", "p7");
        if answer == Answer::Merge {
            merged = Some(only.clone());
        }
        a.rename_host(&h1, &format!("a-{tag}"));
        a.sync();

        b.setup(dir.path());
        assert!(matches!(
            b.run(),
            SyncOutcome::Pending(Pending::FirstSync { .. })
        ));
        b.answer_done(answer);
        let doc = read_doc(dir.path());
        let kept = doc.records.contains_key(&host_key(&only));
        let name = record_value(&doc, &host_key(&h1))["doc"]["name"].clone();
        if answer == Answer::UseLocal {
            assert!(!kept, "以本机为准：云端独有的主机应删除");
            assert_eq!(name, "a-Merge", "以本机为准：h1 应为本机的");
        } else {
            assert!(kept);
            assert_eq!(name, "a-Merge");
        }
        for d in [&b, &a] {
            d.sync();
            assert_eq!(
                d.password(&h1).as_deref(),
                Some("p1"),
                "{tag}：空的秘密不应覆盖云端的"
            );
            assert!(
                has_secret(d, &Owner::key(&key), SecretField::PrivateKey),
                "{tag}"
            );
        }
        assert_eq!(a.names(), b.names(), "{tag}");
    }
    let merged = merged.unwrap();
    assert_eq!(a.names(), ["a-Merge", "only-Merge"]);
    assert_eq!(a.password(&merged).as_deref(), Some("p7"));
    assert_eq!(b.password(&merged).as_deref(), Some("p7"));
}

// 其他设备的设置中有本机写不进、不认识的组（版本不同）：其余的照常写入；写不进的组
// 不当作本机的修改推回云端，不认识的组不从云端删除，也不会每次同步都重试；升级之后重新尝试写入一次
#[test]
fn settings_partially_applied() {
    let (a, _b, dir) = pair();
    let mut doc = read_doc(dir.path());
    let mut terminal = record_value(&doc, "setting/terminal");
    terminal["fontSize"] = json!("很大");
    put_record(&mut doc, "setting/terminal", &terminal);
    put_record(&mut doc, "setting/future", &json!({"option": true}));
    let mut general = record_value(&doc, "setting/general");
    general["confirmClose"] = json!(false);
    put_record(&mut doc, "setting/general", &general);
    write_doc(dir.path(), &doc);

    a.sync();
    let s = a.store().settings();
    assert!(!s.general.confirm_close, "合法的组应写入");
    assert_eq!(
        s.terminal.font_size,
        crate::model::TerminalSettings::default().font_size,
        "写不进的组保留本机的"
    );
    let snaps = block_on(a.m.backups.snapshots()).unwrap().len();
    for _ in 0..2 {
        let res = a.sync();
        assert_eq!(
            (res.pulled, res.pushed),
            (0, 0),
            "没有变化时不应再写入：{res:?}"
        );
    }
    // 升级之后重新尝试一次：仍然写不进，同样不推回云端，之后不再重试
    a.sync.hooks().app = Some("test-2".into());
    for _ in 0..2 {
        let res = a.sync();
        assert_eq!((res.pulled, res.pushed), (0, 0), "升级之后：{res:?}");
    }
    assert_eq!(
        block_on(a.m.backups.snapshots()).unwrap().len(),
        snaps + 1,
        "只在升级后重新尝试写入一次"
    );
    let doc = read_doc(dir.path());
    assert_eq!(
        record_value(&doc, "setting/terminal")["fontSize"],
        json!("很大"),
        "本机的旧值不应推回云端"
    );
    assert!(
        doc.records.contains_key("setting/future"),
        "本机不认识的设置不应从云端删除"
    );
    // 用户之后在本机修改了这一组：作为本机的修改上传
    a.store()
        .update_settings(|s| s.terminal.font_size = 16)
        .unwrap();
    a.sync();
    assert_eq!(
        record_value(&read_doc(dir.path()), "setting/terminal")["fontSize"],
        json!(16),
        "用户的修改应上传"
    );
}

// 写入本机时数据库出错，同步失败，什么都没写（写入本机在一个事务中）；
// 基准不推进，下次同步重新写入
#[test]
fn local_write_error_writes_nothing() {
    let (a, b, _dir) = pair();
    b.store()
        .update_settings(|s| s.terminal.font_size = 16)
        .unwrap();
    b.add_host("h2", "10.0.0.2", "p2");
    b.sync();

    let exec = |sql: &str| {
        a.store()
            .database()
            .write(|tx| Ok(tx.execute_batch(sql)?))
            .unwrap()
    };
    exec(
        "CREATE TRIGGER fail_insert BEFORE INSERT ON setting BEGIN SELECT RAISE(ABORT, '磁盘已满'); END;",
    );
    exec(
        "CREATE TRIGGER fail_update BEFORE UPDATE ON setting BEGIN SELECT RAISE(ABORT, '磁盘已满'); END;",
    );
    assert!(a.try_run().is_err(), "写入设置失败时同步应失败");
    assert_eq!(a.names(), ["h1"], "失败时什么都不写");
    assert!(a.sync.status().last_error.is_some());
    exec("DROP TRIGGER fail_insert; DROP TRIGGER fail_update;");
    a.sync();
    assert_eq!(
        a.store().settings().terminal.font_size,
        16,
        "设置应在下次同步时写入"
    );
    assert_eq!(a.names(), ["h1", "h2"]);
}

// 写入本机之后、写到云端之前其他设备先写入了：下一轮以刚写入本机的内容为基准合并，
// 其他设备随后删除的记录照常删除，不当作本机新增的推回云端（新设备首次同步、普通同步、首次同步选择合并都是）
#[test]
fn applied_then_other_device_writes() {
    let (a, b, dir) = pair();
    let h1 = a.first_host();
    let h2 = a.add_host("h2", "10.0.0.2", "p2");
    a.sync();
    b.sync();

    // 新设备：自动以云端为准写入本机之后，a 删除 h2 并先写入
    let c = Dev::new();
    c.setup(dir.path());
    let (astore, async_) = (a.store().clone(), a.sync.clone());
    let deleted = h2.clone();
    race_before_put(
        &c.sync,
        Box::pin(async move {
            astore.delete::<Host>(&deleted).unwrap();
            async_.sync_now().await.unwrap();
        }),
    );
    // c 首次同步只写入本机、不上传（设备列表中还没有 c 时也要写一次），写入时 a 先写了
    c.sync();
    a.sync();
    assert_eq!(c.names(), ["h1"], "其他设备删除的主机不应被推回");
    assert_eq!(a.names(), ["h1"]);
    assert_eq!(
        crate::cloudsync::records::counts(read_doc(dir.path()).records.keys()).hosts,
        1
    );

    // 普通同步：写入 b 修改的 h1 之后，b 删除 h1 并先写入
    b.sync();
    b.rename_host(&h1, "h1-b");
    b.sync();
    a.add_host("h3", "10.0.0.3", "p3");
    let (bstore, bsync) = (b.store().clone(), b.sync.clone());
    let target = h1.clone();
    race_before_put(
        &a.sync,
        Box::pin(async move {
            bstore.delete::<Host>(&target).unwrap();
            bsync.sync_now().await.unwrap();
        }),
    );
    let res = a.sync();
    assert!(res.conflicts.is_empty(), "{res:?}");
    a.sync.hooks().wrap = None;
    b.sync();
    assert_eq!(a.names(), ["h3"], "其他设备删除的主机不应被推回");
    assert_eq!(b.names(), ["h3"]);

    // 首次同步选择合并：写入云端的 h3 之后，a 删除 h3 并先写入
    let d = Dev::new();
    d.add_host("d1", "10.0.0.4", "p4");
    d.setup(dir.path());
    let h3 = a.first_host();
    let (astore, async_) = (a.store().clone(), a.sync.clone());
    race_before_put(
        &d.sync,
        Box::pin(async move {
            astore.delete::<Host>(&h3).unwrap();
            async_.sync_now().await.unwrap();
        }),
    );
    d.answer_done(Answer::Merge);
    a.sync();
    assert_eq!(d.names(), ["d1"], "其他设备删除的主机不应被推回");
    assert_eq!(a.names(), ["d1"]);
}

// 读取本机数据之后、写入之前用户修改了云端也修改过的记录：不直接覆盖，重新比较
// （较新的用户修改胜出）
#[test]
fn local_edit_during_sync() {
    let (a, b, _dir) = pair();
    let h1 = a.first_host();
    b.rename_host(&h1, "b 改的");
    b.set_updated(&host_key(&h1), 1_000_000);
    b.sync();

    let (store, id) = (a.store().clone(), h1.clone());
    a.sync.hooks().before_apply = Some(Box::new(move || {
        let mut doc = store.get::<Host>(&id).unwrap().doc;
        doc.name = "用户刚改的".into();
        store.update(&id, doc).unwrap();
    }));
    let res = a.sync();
    assert_eq!(a.names(), ["用户刚改的"], "同步不应覆盖用户刚做的修改");
    assert_eq!(res.conflicts.len(), 1, "{res:?}");
    assert_eq!(res.conflicts[0].winner, Side::Local);
    b.sync();
    assert_eq!(b.names(), ["用户刚改的"]);
}

// 同上，修改的是设置：用户刚改的设置不被云端的覆盖，并上传
#[test]
fn local_setting_edit_during_sync() {
    let (a, b, _dir) = pair();
    b.store()
        .update_settings(|s| s.terminal.font_size = 16)
        .unwrap();
    b.sync();
    let store = a.store().clone();
    a.sync.hooks().before_apply = Some(Box::new(move || {
        store
            .update_settings(|s| s.terminal.font_size = 20)
            .unwrap();
    }));
    a.sync();
    assert_eq!(
        a.store().settings().terminal.font_size,
        20,
        "同步不应覆盖用户刚改的设置"
    );
    b.sync();
    assert_eq!(
        b.store().settings().terminal.font_size,
        20,
        "用户的修改应上传"
    );
}

// 两边都改了分组与设置时按修改的先后，而不是谁后同步谁胜出；
// 写到云端的版本带修改的时间（分组与设置组都有自己的修改时间）
#[test]
fn group_and_setting_conflict_by_edit_time() {
    let (a, b, dir) = pair();
    let g = add_group(a.store(), "原名", None);
    a.sync();
    b.sync();
    let t0 = crate::time::now_ms();
    let minute = |m: i64| t0 + m * 60_000;
    let gkey = format!("group/{g}");
    let edit = |d: &Dev, m: i64, name: &str, size: u16| {
        let mut group = d.store().get::<Group>(&g).unwrap().doc;
        group.name = name.into();
        d.store().update(&g, group).unwrap();
        d.set_updated(&gkey, minute(m));
        d.edit_setting_at("terminal", minute(m), |t| {
            t.insert("fontSize".into(), json!(size));
        });
    };
    let group_name = |d: &Dev| d.store().get::<Group>(&g).unwrap().doc.name;
    let font = |d: &Dev| d.store().settings().terminal.font_size;

    // a 先改（第 1 分钟）但后同步，b 后改（第 2 分钟）先同步：b 的修改较新，a 同步时采用 b 的
    edit(&a, 1, "a 改的", 15);
    edit(&b, 2, "b 改的", 16);
    b.sync();
    let res = a.sync();
    assert_eq!(
        (group_name(&a).as_str(), font(&a)),
        ("b 改的", 16),
        "较新的修改（b 的）应胜出"
    );
    assert_eq!(res.conflicts.len(), 2, "分组与设置各一处冲突：{res:?}");
    assert!(res.conflicts.iter().all(|c| c.winner == Side::Remote));
    assert!(
        res.conflicts
            .iter()
            .any(|c| c.kind == RecordKind::Settings && c.name == "terminal")
    );

    // 反过来：b 先改先同步，a 后改后同步，a 的较新；云端的版本带 a 修改的时间（第 5 分钟），不是同步的时间
    edit(&b, 4, "b 又改", 17);
    edit(&a, 5, "a 又改", 18);
    b.sync();
    let res = a.sync();
    assert_eq!(res.conflicts.len(), 2);
    assert!(res.conflicts.iter().all(|c| c.winner == Side::Local));
    let doc = read_doc(dir.path());
    assert_eq!(
        crate::cloudsync::records::version_time(&doc.records[&gkey].v),
        minute(5)
    );
    b.sync();
    assert_eq!((group_name(&b).as_str(), font(&b)), ("a 又改", 18));
}

// 冲突判给云端、写入本机之后，本机之后改回同样的内容时按改回的时间比较，
// 不按很早的时间算而输给其他设备较早的修改
#[test]
fn edit_after_remote_win_uses_new_time() {
    let (a, b, _dir) = pair();
    let g = add_group(a.store(), "原名", None);
    a.sync();
    b.sync();
    let gkey = format!("group/{g}");
    let t0 = crate::time::now_ms() - 3_600_000;
    let rename = |d: &Dev, m: i64, name: &str| {
        let mut group = d.store().get::<Group>(&g).unwrap().doc;
        group.name = name.into();
        d.store().update(&g, group).unwrap();
        d.set_updated(&gkey, t0 + m * 60_000);
    };
    rename(&a, 1, "a 改的");
    rename(&b, 2, "b 改的");
    b.sync();
    a.sync();
    assert_eq!(a.store().get::<Group>(&g).unwrap().doc.name, "b 改的");
    // 写入本机后的修改时间是云端版本中的时间
    assert_eq!(
        a.store().get::<Group>(&g).unwrap().updated_at,
        t0 + 2 * 60_000
    );

    // b 在第 10 分钟又改并同步；a 现在改回第 1 分钟时的内容：a 的较新，应胜出
    rename(&b, 10, "b 又改");
    b.sync();
    let mut group = a.store().get::<Group>(&g).unwrap().doc;
    group.name = "a 改的".into();
    a.store().update(&g, group).unwrap();
    let res = a.sync();
    assert_eq!(a.store().get::<Group>(&g).unwrap().doc.name, "a 改的");
    assert!(
        res.conflicts.iter().all(|c| c.winner == Side::Local),
        "{res:?}"
    );
}

// 同步文件解压后的大小有上限（解开同步密码之后才解压）；写入时同样检查
#[test]
fn document_size_limit() {
    let mut doc = Document::default();
    put_record(
        &mut doc,
        "snippet/s1",
        &json!({"doc": {"content": "x".repeat(4096)}}),
    );
    let password = SecretString::new(PASSWORD);
    let sealed = document::seal(&doc, &password, "test", 1, MAX_DOCUMENT_SIZE).unwrap();
    assert_eq!(
        document::open(&sealed, &password, 1024).unwrap_err(),
        SyncError::TooLarge
    );
    assert_eq!(
        document::seal(&doc, &password, "test", 1, 1024).unwrap_err(),
        SyncError::TooLarge
    );
    // 不是同步文件、密码错误
    assert_eq!(
        document::open(b"hello", &password, MAX_DOCUMENT_SIZE).unwrap_err(),
        SyncError::Corrupt
    );
    let backup =
        crate::backup::envelope::seal(crate::backup::envelope::SealedKind::Backup, &password, b"x")
            .unwrap();
    assert_eq!(
        document::open(&backup, &password, MAX_DOCUMENT_SIZE).unwrap_err(),
        SyncError::Corrupt
    );
    assert_eq!(
        document::open(
            &sealed,
            &SecretString::new("other-password"),
            MAX_DOCUMENT_SIZE
        )
        .unwrap_err(),
        SyncError::WrongPassword
    );
    let mut garbage = sealed.clone();
    *garbage.last_mut().unwrap() ^= 1;
    assert_eq!(
        document::open(&garbage, &password, MAX_DOCUMENT_SIZE).unwrap_err(),
        SyncError::WrongPassword
    );
}

// 秘密不进 Debug 输出
#[test]
fn secrets_do_not_appear_in_debug() {
    let (a, _b, dir) = pair();
    let doc = read_doc(dir.path());
    let text = format!("{doc:?}");
    assert!(!text.contains("p1"), "{text}");
    let snapshot = a.store().sync_snapshot().unwrap();
    assert!(!format!("{snapshot:?}").contains("p1"));
    let input = Dev::input(dir.path(), "secret-sync-password");
    assert!(!format!("{input:?}").contains("secret-sync-password"));
    let _ = Kind::Host;
}
