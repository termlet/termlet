//! 两台以上设备经本机文件夹同步。

use std::time::Duration;

use super::*;
use crate::backup::{BackupOptions, SnapshotReason};
use crate::cloudsync::{Answer, Pending, RecordKind, Side, SyncCounts, SyncStatus};
use crate::model::{
    Auth, CliAccess, Group, GroupDefaults, Jump, KeySource, Kind, SecretField, Snippet,
};
use crate::store::{Owner, Position, StoreEvent};

fn secret(store: &Store, owner: &Owner, field: SecretField) -> Option<String> {
    store
        .secret(owner, field)
        .unwrap()
        .map(|s| s.expose().to_string())
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

#[test]
fn first_device_then_new_device() {
    let dir = sync_dir();
    let (a, b) = (Dev::new(), Dev::new());
    let s = a.store();
    let key = add_key(s, "部署");
    let profile = add_profile(s, "运维", "ops-pass");
    let g = s
        .create(
            Group {
                name: "生产".into(),
                defaults: GroupDefaults {
                    profile: Some(profile.clone()),
                    ..Default::default()
                },
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    let sub = add_group(s, "web", Some(&g));
    let jump = a.add_host("跳板机", "10.0.0.1", "jump-pass");
    let web = s
        .create(
            Host {
                name: "web".into(),
                address: "10.0.0.2".into(),
                auth: Auth::Inherit,
                jump: Jump::Via { host: jump.clone() },
                tags: vec!["prod".into(), "web".into()],
                cli: CliAccess {
                    allowed: true,
                    ..Default::default()
                },
                ..Default::default()
            },
            Some(sub.to_string()),
            Position::Last,
        )
        .unwrap();
    let db = s
        .create(
            Host {
                name: "db".into(),
                address: "10.0.0.3".into(),
                username: "pg".into(),
                auth: Auth::Key {
                    source: KeySource::Library { key: key.clone() },
                },
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    s.set_secret(
        &Owner::host(&db),
        SecretField::Totp,
        Some(&"JBSWY3DPEHPK3PXP".into()),
    )
    .unwrap();
    add_forward(s, &db, "pg", 15432);
    s.create(
        Snippet {
            name: "磁盘".into(),
            content: "df -h".into(),
            ..Default::default()
        },
        None,
        Position::Last,
    )
    .unwrap();
    s.update_settings(|st| {
        st.terminal.font_size = 18;
        st.connection.agent_path = r"\\.\pipe\my-agent".into();
    })
    .unwrap();
    s.update_local_settings(|l| l.backup.keep = 3).unwrap();

    assert!(!a.setup(dir.path()).exists, "空的文件夹中不应有同步文件");
    let res = a.sync();
    assert!(res.pushed > 0 && res.pulled == 0, "{res:?}");
    let data = std::fs::read(dir.path().join(FILE_NAME)).unwrap();
    for plain in [
        "10.0.0.2",
        "jump-pass",
        "web",
        "df -h",
        "ops-pass",
        "JBSWY3DPEHPK3PXP",
    ] {
        assert!(!contains(&data, plain), "同步文件中出现了明文 {plain}");
    }

    // 新设备（没有数据）：同步密码错误被拒绝；之后自动以云端为准
    assert_eq!(
        block_on(b.sync.setup(Dev::input(dir.path(), "wrong-pass"))).unwrap_err(),
        SyncError::WrongPassword
    );
    let info = b.setup(dir.path());
    assert!(
        info.exists && info.counts.hosts == 3 && info.devices.len() == 1,
        "{info:?}"
    );
    let mut events = b.store().subscribe();
    let res = b.sync();
    assert!(res.pulled > 0, "{res:?}");
    let mut seen = Vec::new();
    while let Ok(event) = events.try_recv() {
        seen.push(event);
    }
    assert!(
        seen.contains(&StoreEvent::Changed(Kind::Host))
            && seen.contains(&StoreEvent::SettingsChanged)
    );
    assert_eq!(b.names(), ["db", "web", "跳板机"]);

    // 关系、标签、CLI 权限、秘密（主机的、凭据档案的、跳板机的、密钥库的、TOTP）在新设备上都在
    let bs = b.store();
    let bweb = b.host(&web);
    assert_eq!(bweb.parent.as_deref(), Some(sub.as_str()));
    assert_eq!(bweb.doc.jump, Jump::Via { host: jump.clone() });
    assert_eq!(bweb.doc.tags, ["prod", "web"]);
    assert!(bweb.doc.cli.allowed);
    assert_eq!(
        bs.get::<Group>(&sub).unwrap().parent.as_deref(),
        Some(g.as_str())
    );
    assert_eq!(
        bs.get::<Group>(&g).unwrap().doc.defaults.profile,
        Some(profile.clone())
    );
    assert_eq!(
        secret(bs, &Owner::profile(&profile), SecretField::Password).as_deref(),
        Some("ops-pass")
    );
    assert_eq!(b.password(&jump).as_deref(), Some("jump-pass"));
    assert_eq!(
        secret(bs, &Owner::host(&db), SecretField::Totp).as_deref(),
        Some("JBSWY3DPEHPK3PXP")
    );
    assert_eq!(
        secret(bs, &Owner::key(&key), SecretField::PrivateKey).as_deref(),
        Some(PRIVATE_KEY)
    );
    assert_eq!(bs.list::<Forward>().len(), 1);
    assert_eq!(bs.list::<Snippet>().len(), 1);
    // 同步的设置照写；只属于本机的项（ssh-agent 地址）与本机设置不变
    assert_eq!(bs.settings().terminal.font_size, 18);
    assert_eq!(bs.settings().connection.agent_path, "");
    assert_eq!(bs.local_settings().backup.keep, 7);
    // 写入前保存了快照
    let snaps = block_on(b.m.backups.snapshots()).unwrap();
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].reason, SnapshotReason::Sync);

    // 再同步一次：没有变化，不写入
    let before = std::fs::read(dir.path().join(FILE_NAME)).unwrap();
    let res = b.sync();
    assert_eq!((res.pulled, res.pushed), (0, 0));
    assert_eq!(
        std::fs::read(dir.path().join(FILE_NAME)).unwrap(),
        before,
        "没有变化时不应写入同步文件"
    );
    let st = b.sync.status();
    assert!(st.config.is_some() && st.last_sync.is_some() && st.last_error.is_none());
    assert_eq!(st.devices.len(), 2);
    assert_eq!(st.device_id.as_deref(), Some(st.devices[0].id.as_str()));
}

#[test]
fn incremental_changes() {
    let (a, b, _dir) = pair();
    let h1 = a.first_host();
    let sn = b
        .store()
        .create(
            Snippet {
                name: "s".into(),
                content: "ls".into(),
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    b.sync();
    a.sync();

    // a 改名并改密码，b 新增主机、删除快捷命令
    a.rename_host(&h1, "h1-改");
    a.m.set_password(&h1, "p1-new");
    let h2 = b.add_host("h2", "10.0.0.2", "p2");
    b.store().delete::<Snippet>(&sn).unwrap();
    let res = a.sync();
    assert!(res.pushed > 0 && res.conflicts.is_empty(), "{res:?}");
    let res = b.sync();
    assert!(
        res.pulled > 0 && res.pushed > 0 && res.conflicts.is_empty(),
        "{res:?}"
    );
    a.sync();
    for d in [&a, &b] {
        assert_eq!(d.names(), ["h1-改", "h2"]);
        assert_eq!(d.password(&h1).as_deref(), Some("p1-new"));
        assert_eq!(d.password(&h2).as_deref(), Some("p2"));
        assert!(
            d.store().list::<Snippet>().is_empty(),
            "删除的快捷命令应在两边都删除"
        );
    }
    // 删除主机
    b.delete_host(&h2);
    b.sync();
    a.sync();
    assert_eq!(a.names(), ["h1-改"]);
    assert_eq!(a.password(&h2), None);
}

#[test]
fn conflicts() {
    let (a, b, _dir) = pair();
    let h1 = a.first_host();
    let key = host_key(&h1);

    // 两边都改了同一台主机：b 的修改较新，b 胜出
    a.rename_host(&h1, "a 的名字");
    a.set_updated(&key, 1_000_000);
    b.rename_host(&h1, "b 的名字");
    b.set_updated(&key, 2_000_000);
    a.sync();
    let res = b.sync();
    assert_eq!(res.conflicts.len(), 1, "{res:?}");
    assert_eq!(res.conflicts[0].name, "b 的名字");
    assert_eq!(res.conflicts[0].winner, Side::Local);
    assert_eq!(res.conflicts[0].kind, RecordKind::Host);
    a.sync();
    assert_eq!(a.names(), ["b 的名字"]);
    assert_eq!(b.names(), ["b 的名字"]);

    // 一边删除、一边修改：保留修改
    a.rename_host(&h1, "仍要用");
    b.delete_host(&h1);
    b.sync();
    let res = a.sync();
    assert_eq!(res.conflicts.len(), 1);
    assert!(res.conflicts[0].deleted && res.conflicts[0].winner == Side::Local);
    b.sync();
    assert_eq!(b.names(), ["仍要用"]);
    assert_eq!(
        b.password(&h1).as_deref(),
        Some("p1"),
        "保留的主机应带着密码"
    );
}

#[test]
fn first_sync_with_data_on_both_sides() {
    let dir = sync_dir();
    let (a, b, c) = (Dev::new(), Dev::new(), Dev::new());
    a.add_host("a1", "10.0.0.1", "p");
    a.setup(dir.path());
    a.sync();

    b.add_host("b1", "10.0.0.2", "p");
    b.add_host("a1 的另一份", "10.0.0.1", "p");
    b.setup(dir.path());
    match b.run() {
        SyncOutcome::Pending(Pending::FirstSync {
            local,
            remote,
            duplicates,
        }) => {
            assert_eq!((local.hosts, remote.hosts, duplicates), (2, 1, 1));
        }
        other => panic!("两边都有数据时应请用户选择：{other:?}"),
    }
    assert_eq!(b.names(), ["a1 的另一份", "b1"], "等待选择时不应改动数据");
    assert!(b.sync.status().pending.is_some());
    b.answer_done(Answer::Merge);
    assert_eq!(b.names(), ["a1", "a1 的另一份", "b1"]);
    assert!(b.sync.status().pending.is_none());

    // 以云端为准：本机独有的删除
    c.add_host("c1", "10.0.0.3", "p");
    c.setup(dir.path());
    c.answer_done(Answer::UseRemote);
    assert_eq!(c.names(), ["a1", "a1 的另一份", "b1"]);
    // 以本机为准：云端改为与本机相同
    c.add_host("c2", "10.0.0.4", "p");
    c.setup(dir.path());
    c.delete_host(&c.first_host());
    c.answer_done(Answer::UseLocal);
    a.sync();
    assert_eq!(a.names(), c.names(), "以本机为准后其他设备应与它相同");
}

#[test]
fn many_deletions_need_confirm() {
    let (a, b, dir) = pair();
    for i in 0..9 {
        a.add_host(&format!("x{i}"), &format!("10.1.0.{i}"), "p");
    }
    a.sync();
    b.sync();
    let before = std::fs::read(dir.path().join(FILE_NAME)).unwrap();
    for host in a.store().list::<Host>().into_iter().take(6) {
        a.delete_host(&host.id);
    }
    match a.run() {
        SyncOutcome::Pending(Pending::ConfirmDeletes { local, remote }) => {
            assert_eq!((local, remote), (0, 6))
        }
        other => panic!("删除较多时应请用户确认：{other:?}"),
    }
    assert_eq!(
        std::fs::read(dir.path().join(FILE_NAME)).unwrap(),
        before,
        "等待确认时不应写入云端"
    );
    // 等待确认期间自动同步暂停
    assert!(matches!(
        a.sync.status().pending,
        Some(Pending::ConfirmDeletes { .. })
    ));
    // 上传的记录含主机与它们的秘密
    let res = a.answer_done(Answer::ConfirmDeletes);
    assert_eq!(res.pushed, 12, "{res:?}");
    // 另一台设备要删除这些主机：同样先确认
    match b.run() {
        SyncOutcome::Pending(Pending::ConfirmDeletes { local, .. }) => assert_eq!(local, 6),
        other => panic!("{other:?}"),
    }
    b.answer_done(Answer::ConfirmDeletes);
    assert_eq!(b.names().len(), 4);
}

// 其他设备在读写之间先写入了，重新拉取合并，不覆盖
#[test]
fn concurrent_write_retries() {
    let (a, b, _dir) = pair();
    a.add_host("a2", "10.0.0.2", "p");
    b.add_host("b2", "10.0.0.3", "p");
    let bs = b.sync.clone();
    race_before_put(
        &a.sync,
        Box::pin(async move {
            bs.sync_now().await.unwrap();
        }),
    );
    a.sync();
    b.sync();
    let want = ["a2", "b2", "h1"];
    assert_eq!(a.names(), want);
    assert_eq!(b.names(), want);
}

// 两台设备各自新建了监听同一端口的规则，先同步上去的那条留下
#[test]
fn forward_bind_conflict() {
    let (a, b, _dir) = pair();
    let h = a.first_host();
    let ra = add_forward(a.store(), &h, "a", 18080);
    add_forward(b.store(), &h, "b", 18080);
    a.sync();
    b.sync();
    a.sync();
    for d in [&a, &b] {
        let list = d.store().list::<Forward>();
        assert_eq!(list.len(), 1, "监听同一端口的规则应只剩一条");
        assert_eq!(list[0].id, ra);
    }
}

// 本机解析不了的条目（更新版本写的）：引用它的条目、它的秘密与转发规则也不写入本机，更不当作本机删除、修改了它们
#[test]
fn records_depending_on_unreadable_items_are_kept() {
    let (a, b, dir) = pair();
    let bastion = a.add_host("bastion", "10.0.0.9", "jump-pass");
    let forward = add_forward(a.store(), &bastion, "web", 18081);
    let target = a.add_host("db", "10.0.0.10", "db-pass");
    a.edit_host(&target, |h| {
        h.jump = Jump::Via {
            host: bastion.clone(),
        }
    });
    a.sync();
    // 更新版本把跳板机改成了本机不认识的协议
    let mut doc = read_doc(dir.path());
    let key = host_key(&bastion);
    let mut value = record_value(&doc, &key);
    value["doc"]["protocol"] = serde_json::json!("future");
    put_record(&mut doc, &key, &value);
    write_doc(dir.path(), &doc);
    let before = read_doc(dir.path());

    b.sync();
    assert!(b.store().list::<Host>().iter().all(|h| h.id != bastion));
    assert!(
        b.store().list::<Host>().iter().all(|h| h.id != target),
        "经它连接的主机也不写入"
    );
    assert!(b.forwards().is_empty());
    let after = read_doc(dir.path());
    for key in [
        host_key(&bastion),
        host_key(&target),
        format!("forward/{forward}"),
        format!("secret/host/{bastion}"),
        format!("secret/host/{target}"),
    ] {
        assert_eq!(
            after.records.get(&key).map(|r| &r.v),
            before.records.get(&key).map(|r| &r.v),
            "{key} 应原样留在云端"
        );
    }

    // 另一台设备照常
    a.sync();
    assert_eq!(a.forwards().len(), 1);
    assert_eq!(a.password(&bastion).as_deref(), Some("jump-pass"));
    assert_eq!(
        a.host(&target).doc.jump,
        Jump::Via {
            host: bastion.clone()
        }
    );
}

// 云端来自更新版本时不读也不写，状态中记下错误
#[test]
fn refuse_newer_document() {
    let (a, _b, dir) = pair();
    let path = dir.path().join(FILE_NAME);
    let password = SecretString::new(PASSWORD);
    let (_, payload) =
        crate::backup::envelope::open(&std::fs::read(&path).unwrap(), Some(&password)).unwrap();
    let mut json = String::new();
    std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&payload[..]), &mut json)
        .unwrap();
    let mut value: Value = serde_json::from_str(&json).unwrap();
    value["schema"] = Value::from(crate::db::SCHEMA_VERSION + 1);
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut gz, value.to_string().as_bytes()).unwrap();
    let sealed = crate::backup::envelope::seal(
        crate::backup::envelope::SealedKind::Sync,
        &password,
        &gz.finish().unwrap(),
    )
    .unwrap();
    std::fs::write(&path, &sealed).unwrap();

    a.add_host("new", "10.0.0.9", "p");
    assert_eq!(a.try_run().unwrap_err(), SyncError::TooNew);
    assert_eq!(std::fs::read(&path).unwrap(), sealed, "不应写入云端");
    assert_eq!(a.sync.status().last_error, Some(SyncError::TooNew));
}

#[test]
fn disable_and_options() {
    let (a, _b, dir) = pair();
    block_on(a.sync.set_options(Some(false), Some(30))).unwrap();
    assert_eq!(
        block_on(a.sync.set_options(None, Some(0))).unwrap_err(),
        SyncError::invalid("interval", "out_of_range")
    );
    let config = a.sync.status().config.unwrap();
    assert!(!config.auto);
    assert_eq!(config.interval_minutes, 30);
    // 重新设置时沿用自动同步的开关与间隔
    a.setup(dir.path());
    assert_eq!(a.sync.status().config.unwrap().interval_minutes, 30);

    block_on(a.sync.disable()).unwrap();
    assert_eq!(a.sync.status().config, None, "停止后应为未设置");
    assert_eq!(a.try_run().unwrap_err(), SyncError::NotConfigured);
    assert_eq!(
        block_on(a.sync.set_options(Some(true), None)).unwrap_err(),
        SyncError::NotConfigured
    );
    assert!(
        crate::cloudsync::state::load_bases(a.store().database())
            .unwrap()
            .is_empty(),
        "停止后应清除基准"
    );
    assert!(dir.path().join(FILE_NAME).exists(), "云端的文件不动");
}

// 恢复备份不清除同步基准，恢复后的数据按本机的修改上传
#[test]
fn backup_keeps_sync_base() {
    let (a, b, _dir) = pair();
    let backup = a.m.path("full.termlet-backup");
    block_on(a.m.backups.create_backup(
        backup.clone(),
        "backup-pass".into(),
        BackupOptions::default(),
    ))
    .unwrap();
    a.add_host("after", "10.0.0.5", "p");
    a.sync();
    block_on(a.m.backups.restore_backup(backup, "backup-pass".into())).unwrap();
    assert!(
        !crate::cloudsync::state::load_bases(a.store().database())
            .unwrap()
            .is_empty(),
        "恢复不应清除同步基准"
    );
    let res = a.sync();
    assert!(res.pushed > 0, "恢复删除了 after，这是本机的修改：{res:?}");
    b.sync();
    assert_eq!(b.names(), ["h1"]);
}

#[test]
fn folder_backend() {
    use crate::cloudsync::backend::{Backend, Fetch, Folder, Stored};
    let dir = sync_dir();
    let f = Folder::new(dir.path());
    assert!(matches!(block_on(f.get()).unwrap(), Fetch::Missing));
    assert_eq!(
        block_on(f.put(b"v1", Some("some"))).unwrap(),
        Stored::Changed,
        "文件不存在而带了版本标记"
    );
    assert_eq!(block_on(f.put(b"v1", None)).unwrap(), Stored::Written);
    let Fetch::Found { data, etag } = block_on(f.get()).unwrap() else {
        panic!("应有同步文件")
    };
    assert_eq!(data, b"v1");
    assert_eq!(
        block_on(f.put(b"v2", None)).unwrap(),
        Stored::Changed,
        "文件已存在而当作新建"
    );
    assert_eq!(
        block_on(f.put(b"v2", Some(&etag))).unwrap(),
        Stored::Written
    );
    assert_eq!(
        block_on(f.put(b"v3", Some(&etag))).unwrap(),
        Stored::Changed,
        "旧的版本标记"
    );
    block_on(f.check()).unwrap();
    // 没有留下临时文件
    let names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, [FILE_NAME]);
    let missing = dir.path().join("missing");
    assert!(matches!(
        block_on(Folder::new(&missing).check()).unwrap_err(),
        SyncError::FolderMissing { .. }
    ));
    assert!(matches!(
        block_on(Folder::new(&missing).get()),
        Err(SyncError::FolderMissing { .. })
    ));
    assert_eq!(
        block_on(Folder::new("relative/dir").check()).unwrap_err(),
        SyncError::invalid("path", "not_absolute")
    );
}

// 设置的校验：密码太短、设备名太长、相对路径
#[test]
fn setup_validates_input() {
    let dir = sync_dir();
    let a = Dev::new();
    let short = Dev::input(dir.path(), "short");
    assert_eq!(
        block_on(a.sync.setup(short)).unwrap_err(),
        SyncError::invalid("password", "too_short")
    );
    let mut long = Dev::input(dir.path(), PASSWORD);
    long.device_name = "名".repeat(65);
    assert_eq!(
        block_on(a.sync.setup(long)).unwrap_err(),
        SyncError::invalid("device_name", "too_long")
    );
    let relative = Dev::input(Path::new("relative"), PASSWORD);
    assert_eq!(
        block_on(a.sync.test_connection(&relative)).unwrap_err(),
        SyncError::invalid("path", "not_absolute")
    );
    // 设备名留空用电脑名
    let mut unnamed = Dev::input(dir.path(), PASSWORD);
    unnamed.device_name = "  ".into();
    block_on(a.sync.setup(unnamed)).unwrap();
    assert!(
        !a.sync
            .status()
            .config
            .unwrap()
            .device_name
            .trim()
            .is_empty()
    );
    assert_eq!(a.sync.status().config.unwrap().interval_minutes, 10);
}

// 只属于本机的设置项不同步，写入时保留本机的值；同步的项照常合并
#[test]
fn local_only_settings_stay_local() {
    let (a, b, _dir) = pair();
    a.store()
        .update_settings(|s| {
            s.connection.agent_path = "a-agent".into();
            s.connection.keepalive_seconds = 60;
            s.terminal.session_log = true;
            s.general.auto_check_update = false;
        })
        .unwrap();
    b.store()
        .update_settings(|s| s.connection.agent_path = "b-agent".into())
        .unwrap();
    a.sync();
    let res = b.sync();
    assert!(res.conflicts.is_empty(), "只改本机的项不算修改：{res:?}");
    let s = b.store().settings();
    assert_eq!(s.connection.keepalive_seconds, 60);
    assert_eq!(s.connection.agent_path, "b-agent");
    assert!(!s.terminal.session_log);
    assert!(s.general.auto_check_update);
}

// 保险库未解锁时不能同步
#[test]
fn locked_vault_cannot_sync() {
    let (a, _b, _dir) = pair();
    let a = {
        let Dev { m, sync } = a;
        drop(sync);
        Dev::of(m.reopen(crate::backup::testkit::NoKeyring))
    };
    assert_eq!(a.store().vault_status(), VaultStatus::KeyMissing);
    assert_eq!(a.try_run().unwrap_err(), SyncError::VaultLocked);
    // 同步状态没有被删除（换回原来的钥匙串就能继续）
    assert!(a.sync.status().config.is_some());
}

// 同步状态的订阅：同步中、完成后的时间与结果
#[test]
fn status_is_published() {
    let (a, _b, _dir) = pair();
    let mut rx = a.sync.subscribe();
    rx.borrow_and_update();
    a.add_host("h2", "10.0.0.2", "p");
    let report = a.sync();
    assert!(rx.has_changed().unwrap());
    let status: SyncStatus = rx.borrow_and_update().clone();
    assert!(!status.syncing);
    assert_eq!(status.last_sync, Some(report.at));
    assert_eq!(
        status.last_report.as_ref().map(|r| r.pushed),
        Some(report.pushed)
    );
    // 重启后从本机读回上次的状态
    let again = CloudSync::new(a.store().clone(), None);
    let restored = again.status();
    assert_eq!(restored.last_sync, Some(report.at));
    assert_eq!(restored.devices, status.devices);
    assert_eq!(restored.config, status.config);
}

// 冲突中改用另一边：作为本机的新修改写入，下次同步时上传
#[test]
fn use_other_version_of_a_conflict() {
    let (a, b, _dir) = pair();
    let h1 = a.first_host();
    let key = host_key(&h1);
    a.rename_host(&h1, "a 的");
    a.set_updated(&key, 1_000_000);
    b.rename_host(&h1, "b 的");
    b.set_updated(&key, 2_000_000);
    b.sync();
    let res = a.sync();
    assert_eq!(res.conflicts[0].winner, Side::Remote);
    assert!(res.conflicts[0].can_use_other);
    assert_eq!(a.names(), ["b 的"]);
    let conflicts = a.sync.status().conflicts;
    assert_eq!(conflicts.len(), 1);
    block_on(a.sync.use_other_version(&conflicts[0].key)).unwrap();
    assert_eq!(a.names(), ["a 的"]);
    assert!(a.sync.status().conflicts.is_empty());
    assert!(block_on(a.sync.use_other_version(&key)).is_err());
    a.sync();
    b.sync();
    assert_eq!(b.names(), ["a 的"]);
    assert_eq!(b.password(&h1).as_deref(), Some("p1"));

    // 删改冲突：改用删除的一边就是删除
    a.rename_host(&h1, "仍要用");
    b.delete_host(&h1);
    b.sync();
    let res = a.sync();
    assert!(res.conflicts[0].deleted);
    block_on(a.sync.use_other_version(&res.conflicts[0].key)).unwrap();
    assert!(a.names().is_empty());
    a.sync();
    b.sync();
    assert!(b.names().is_empty());

    // 看过了：清空
    a.add_host("x", "10.0.0.8", "p");
    a.sync();
    a.sync.dismiss_conflicts();
    assert!(a.sync.status().conflicts.is_empty());
}

// 冲突之后本机又改过：不能再改用另一边（会用冲突时的旧内容覆盖之后的修改），冲突从列表中去掉
#[test]
fn stale_conflict_cannot_use_other_version() {
    let (a, b, _dir) = pair();
    let h1 = a.first_host();
    let key = host_key(&h1);
    a.rename_host(&h1, "a 的");
    a.set_updated(&key, 1_000_000);
    b.rename_host(&h1, "b 的");
    b.set_updated(&key, 2_000_000);
    b.sync();
    assert_eq!(a.sync().conflicts.len(), 1);
    a.rename_host(&h1, "之后改的");
    let error = block_on(a.sync.use_other_version(&key)).unwrap_err();
    assert!(
        matches!(&error, SyncError::Invalid { reason, .. } if reason == "stale"),
        "{error:?}"
    );
    assert_eq!(a.names(), ["之后改的"]);
    assert!(a.sync.status().conflicts.is_empty());
}

// 本机修改后自动同步；同步自己写入本机引起的变更事件不会再触发一次
#[test]
fn auto_sync_after_local_change() {
    let (a, b, dir) = pair();
    // 云端有 a 还没有的主机：a 自动同步时会写入本机，写入引起的变更事件不应再触发一次同步
    b.add_host("h3", "10.0.0.3", "p3");
    b.sync();
    let initial = a.sync.status().last_sync;
    let task = crate::runtime::spawn(
        a.sync
            .clone()
            .auto_loop(Duration::from_secs(3600), Duration::from_millis(100)),
    );
    let h2 = a.add_host("h2", "10.0.0.2", "p2");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let status = a.sync.status();
        if !status.syncing
            && status.last_sync != initial
            && read_doc(dir.path()).records.contains_key(&host_key(&h2))
        {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "本机修改后应自动同步");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(a.names(), ["h1", "h2", "h3"]);
    let first = a.sync.status().last_sync;
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        a.sync.status().last_sync,
        first,
        "本机的数据与上次同步后相同时不应再自动同步"
    );
    task.abort();
}

// 自动同步的任务还没开始运行时的本机修改也会同步：订阅在调用 auto_loop 时就完成（以前在任务第一次运行时才订阅，
// 运行器慢时启动后紧接着的修改被漏掉，要等下一个周期）
#[test]
fn auto_sync_sees_changes_before_the_task_runs() {
    let (a, _b, dir) = pair();
    let auto = a
        .sync
        .clone()
        .auto_loop(Duration::from_secs(3600), Duration::from_millis(100));
    let h2 = a.add_host("h2", "10.0.0.2", "p2");
    let task = crate::runtime::spawn(auto);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !read_doc(dir.path()).records.contains_key(&host_key(&h2)) {
        assert!(
            std::time::Instant::now() < deadline,
            "任务运行之前的修改应自动同步"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    task.abort();
}

// 同步完成后清理已经同步出去的墓碑
#[test]
fn synced_tombstones_are_purged() {
    let (a, _b, _dir) = pair();
    let h2 = a.add_host("h2", "10.0.0.2", "p");
    a.sync();
    a.delete_host(&h2);
    let tombstones = |d: &Dev| -> i64 {
        d.store()
            .database()
            .read(|c| {
                c.query_row(
                    "SELECT COUNT(*) FROM host WHERE deleted_at IS NOT NULL",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap()
    };
    assert_eq!(tombstones(&a), 1);
    a.sync();
    assert_eq!(tombstones(&a), 0);
    assert_eq!(a.names(), ["h1"]);
}

// 首次同步时云端为空：以本机为准上传，不需要选择
#[test]
fn first_sync_with_empty_cloud_uploads() {
    let dir = sync_dir();
    let a = Dev::new();
    a.add_host("a1", "10.0.0.1", "p");
    a.setup(dir.path());
    let res = a.sync();
    assert!(res.pushed > 0);
    assert_eq!(
        crate::cloudsync::records::counts(read_doc(dir.path()).records.keys()),
        SyncCounts {
            hosts: 1,
            ..Default::default()
        }
    );
}
