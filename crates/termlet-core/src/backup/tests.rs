//! 备份与快照的测试。

use std::path::Path;
use std::time::Duration;

use super::testkit::{Machine, NoKeyring};
use super::*;
use crate::model::{Host, Kind, Snippet};
use crate::store::{Position, StoreEvent};
use crate::testkit::{SharedKeys, block_on};

fn pw(s: &str) -> SecretString {
    SecretString::new(s)
}

fn create(
    m: &Machine,
    path: &Path,
    password: &str,
    audit: bool,
) -> Result<BackupSummary, BackupError> {
    block_on(
        m.backups
            .create_backup(path.to_path_buf(), pw(password), BackupOptions { audit }),
    )
}

fn restore(m: &Machine, path: &Path, password: &str) -> Result<RestoreOutcome, BackupError> {
    let result = block_on(m.backups.restore_backup(path.to_path_buf(), pw(password)));
    // 使用统计的功能名单记下恢复过完整备份
    if result.is_ok() {
        assert!(crate::usage::features::global().contains(crate::usage::Feature::Backup));
    }
    result
}

fn snapshots(m: &Machine) -> Vec<SnapshotInfo> {
    block_on(m.backups.snapshots()).unwrap()
}

fn add_known_host(m: &Machine, host: &str) {
    m.store
        .database()
        .write(|tx| {
            tx.execute(
                "INSERT INTO known_host (host, port, algorithm, key, added_at) VALUES (?1, 22, 'ssh-ed25519', 'ssh-ed25519 AAAA', 1)",
                [host],
            )?;
            Ok(())
        })
        .unwrap();
}

fn known_hosts(m: &Machine) -> Vec<String> {
    m.store
        .database()
        .read(|c| {
            let mut stmt = c.prepare("SELECT host FROM known_host ORDER BY host")?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            rows.collect()
        })
        .unwrap()
}

/// 模拟命令行工具的审计日志表（R10 之前还没有这张表）。
fn add_audit(m: &Machine, n: usize) {
    m.store
        .database()
        .write(|tx| {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS audit_log (id INTEGER PRIMARY KEY, detail TEXT NOT NULL) STRICT",
            )?;
            for i in 0..n {
                tx.execute("INSERT INTO audit_log (detail) VALUES (?1)", [format!("ls {i}")])?;
            }
            Ok(())
        })
        .unwrap();
}

fn audit_count(m: &Machine) -> i64 {
    m.store
        .database()
        .read(|c| c.query_row("SELECT COUNT(*) FROM audit_log", [], |r| r.get(0)))
        .unwrap()
}

/// 解开完整备份，用 `edit` 改动其中的数据库（或说明）后重新加密。
fn rewrite_backup(
    data: &[u8],
    password: &str,
    edit: impl FnOnce(&Path, &mut archive::Manifest),
) -> Vec<u8> {
    let (_, payload) = envelope::open(data, Some(&pw(password))).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("backup.db");
    let mut reader = &payload[..];
    let mut manifest = archive::read_manifest(&mut reader).unwrap();
    archive::extract(reader, &db, archive::MAX_DB_SIZE).unwrap();
    edit(&db, &mut manifest);
    let mut out = Vec::new();
    archive::write(&mut out, &manifest, &db).unwrap();
    envelope::seal(SealedKind::Backup, &pw(password), &out).unwrap()
}

fn edit_db(sql: &'static str) -> impl FnOnce(&Path, &mut archive::Manifest) {
    move |path, _| {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(sql).unwrap();
    }
}

#[test]
fn backup_restores_on_another_machine() {
    let a = Machine::new();
    let b = Machine::new();
    let web = a.add_host("web", "10.0.0.1", "s3cret");
    crate::history::record(&a.store, web.as_str(), "export TOKEN=abc").unwrap();
    a.store
        .create(
            Snippet {
                name: "磁盘".into(),
                content: "df -h".into(),
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    a.store
        .update_settings(|s| s.terminal.font_size = 18)
        .unwrap();
    add_known_host(&a, "10.0.0.1");
    assert_eq!(a.backups.current_counts().unwrap().hosts, 1);

    let file = a.path("my.termlet-backup");
    let summary = create(&a, &file, "backup-pass", false).unwrap();
    let c = summary.counts;
    assert_eq!(
        (c.hosts, c.snippets, c.history, c.known_hosts, c.audit),
        (1, 1, 1, 1, 0)
    );
    assert!(!summary.audit);
    assert_eq!(
        (summary.app.as_str(), summary.schema),
        (crate::VERSION, SCHEMA_VERSION)
    );
    // 文件中不含明文的密码、命令与主机地址
    let data = std::fs::read(&file).unwrap();
    for plain in ["s3cret", "TOKEN=abc", "10.0.0.1", "df -h"] {
        assert!(
            !data.windows(plain.len()).any(|w| w == plain.as_bytes()),
            "备份文件中出现了明文 {plain}"
        );
    }

    // b 上已有自己的数据
    b.add_host("old", "10.9.9.9", "x");
    let mut events = b.store.subscribe();
    let inspected = block_on(
        b.backups
            .inspect_backup(file.clone(), Some(pw("backup-pass"))),
    )
    .unwrap();
    assert_eq!(inspected, summary);
    let outcome = restore(&b, &file, "backup-pass").unwrap();
    assert!(!outcome.vault_reset);
    assert_eq!(b.host_names(), ["web"]);
    // 秘密与命令历史已转为 b 的主密钥加密
    assert_eq!(b.password(&web).as_deref(), Some("s3cret"));
    let history = crate::history::list(&b.store, web.as_str()).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].command, "export TOKEN=abc");
    assert_eq!(b.store.settings().terminal.font_size, 18);
    assert_eq!(known_hosts(&b), ["10.0.0.1"]);
    // 恢复后发出了全部种类的变更事件
    let mut got = Vec::new();
    while let Ok(event) = events.try_recv() {
        got.push(event);
    }
    for kind in Kind::ALL {
        assert!(got.contains(&StoreEvent::Changed(kind)), "缺少 {kind:?}");
    }
    assert!(
        got.contains(&StoreEvent::SettingsChanged) && got.contains(&StoreEvent::SecretsChanged)
    );
    // b 的保险库照常可用，新保存的秘密可以解密
    let new = b.add_host("new", "10.0.0.2", "pw2");
    assert_eq!(b.password(&new).as_deref(), Some("pw2"));
    // 恢复前保存了一份当前数据的快照
    let list = snapshots(&b);
    assert_eq!(list.len(), 1);
    assert_eq!(
        (list[0].reason, list[0].counts.hosts),
        (SnapshotReason::Restore, 1)
    );
    assert_eq!(outcome.snapshot, list[0]);
}

// 含审计日志的备份恢复后替换本机的审计日志；不含时保留本机的
#[test]
fn audit_log_is_optional() {
    let a = Machine::new();
    let b = Machine::new();
    a.add_host("web", "10.0.0.1", "pw");
    add_audit(&a, 3);
    add_audit(&b, 1);
    let without = a.path("without.termlet-backup");
    let summary = create(&a, &without, "backup-pass", false).unwrap();
    assert_eq!((summary.audit, summary.counts.audit), (false, 0));
    restore(&b, &without, "backup-pass").unwrap();
    assert_eq!(audit_count(&b), 1, "应保留本机的审计日志");

    let with = a.path("with.termlet-backup");
    let summary = create(&a, &with, "backup-pass", true).unwrap();
    assert_eq!((summary.audit, summary.counts.audit), (true, 3));
    restore(&b, &with, "backup-pass").unwrap();
    assert_eq!(audit_count(&b), 3);
}

// 主密钥丢失时也能恢复完整备份（先重置保险库）。重置只在备份能解开、完成迁移、
// 每张表都能读出、全部密文都能解开之后进行；在此之前失败的不重置，本机数据不变
#[test]
fn restore_resets_lost_vault_only_when_backup_is_usable() {
    let a = Machine::new();
    let web = a.add_host("web", "10.0.0.1", "s3cret");
    let file = a.path("a.termlet-backup");
    create(&a, &file, "backup-pass", false).unwrap();
    let data = std::fs::read(&file).unwrap();

    let b = Machine::new();
    b.add_host("old", "10.9.9.9", "x");
    let b = b.reopen(SharedKeys::default());
    assert_eq!(b.store.vault_status(), VaultStatus::KeyMissing);

    let garbage = rewrite_backup(&data, "backup-pass", |path, _| {
        std::fs::write(path, "不是数据库").unwrap();
    });
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("备份密码错误", data.clone(), "wrong-pass"),
        ("数据库无法读取", garbage, "backup-pass"),
        (
            "缺少条目表",
            rewrite_backup(&data, "backup-pass", edit_db("DROP TABLE host")),
            "backup-pass",
        ),
        (
            "密文损坏",
            rewrite_backup(
                &data,
                "backup-pass",
                edit_db("UPDATE secret SET blob = randomblob(64)"),
            ),
            "backup-pass",
        ),
    ];
    for (name, content, password) in cases {
        let path = b.path("case.termlet-backup");
        std::fs::write(&path, content).unwrap();
        assert!(restore(&b, &path, password).is_err(), "{name}：恢复应失败");
        assert_eq!(
            b.store.vault_status(),
            VaultStatus::KeyMissing,
            "{name}：不应重置保险库"
        );
        assert_eq!(b.host_names(), ["old"], "{name}：失败的恢复不应改动数据");
    }

    // 重置之后替换数据时失败（模拟写入失败）：保险库已重置、数据没有恢复，再次恢复即可
    b.store
        .database()
        .write(|tx| {
            tx.execute_batch(
                "CREATE TRIGGER fail_restore BEFORE INSERT ON host BEGIN SELECT RAISE(ABORT, '模拟写入失败'); END",
            )?;
            Ok(())
        })
        .unwrap();
    let err = restore(&b, &file, "backup-pass").unwrap_err();
    assert!(err.to_string().contains("模拟写入失败"), "{err}");
    assert_eq!(b.store.vault_status(), VaultStatus::Unlocked);
    assert_eq!(b.host_names(), ["old"]);
    b.store
        .database()
        .write(|tx| {
            tx.execute_batch("DROP TRIGGER fail_restore")?;
            Ok(())
        })
        .unwrap();

    let outcome = restore(&b, &file, "backup-pass").unwrap();
    assert!(!outcome.vault_reset, "保险库已在上一次重置");
    assert_eq!(b.host_names(), ["web"]);
    assert_eq!(b.password(&web).as_deref(), Some("s3cret"));
}

// 第一次恢复就重置保险库时，结果中标出
#[test]
fn restore_reports_vault_reset() {
    let a = Machine::new();
    let web = a.add_host("web", "10.0.0.1", "s3cret");
    let file = a.path("a.termlet-backup");
    create(&a, &file, "backup-pass", false).unwrap();
    let b = Machine::new().reopen(SharedKeys::default());
    // 新的数据目录第一次打开时生成了主密钥；换一个钥匙串后主密钥丢失
    assert_eq!(b.store.vault_status(), VaultStatus::KeyMissing);
    let outcome = restore(&b, &file, "backup-pass").unwrap();
    assert!(outcome.vault_reset);
    assert_eq!(b.password(&web).as_deref(), Some("s3cret"));
}

// 主密钥丢失、钥匙串又写不进新的主密钥时不重置，本机数据不变
#[test]
fn restore_without_keyring_leaves_data_untouched() {
    let a = Machine::new();
    a.add_host("web", "10.0.0.1", "s3cret");
    let file = a.path("a.termlet-backup");
    create(&a, &file, "backup-pass", false).unwrap();
    let b = Machine::new();
    b.add_host("old", "10.9.9.9", "x");
    let b = b.reopen(NoKeyring);
    assert_eq!(b.store.vault_status(), VaultStatus::KeyMissing);
    assert!(matches!(
        restore(&b, &file, "backup-pass"),
        Err(BackupError::VaultUnavailable(_))
    ));
    assert_eq!(b.store.vault_status(), VaultStatus::KeyMissing);
    assert_eq!(b.host_names(), ["old"]);
}

#[test]
fn backup_errors() {
    let a = Machine::new();
    a.add_host("web", "10.0.0.1", "pw");
    let file = a.path("a.termlet-backup");
    assert_eq!(
        create(&a, &file, "short", false).unwrap_err(),
        BackupError::PasswordTooShort { min: 8 }
    );
    assert_eq!(
        create(
            &a,
            Path::new("relative.termlet-backup"),
            "backup-pass",
            false
        )
        .unwrap_err(),
        BackupError::NotAbsolute
    );
    create(&a, &file, "backup-pass", false).unwrap();
    let data = std::fs::read(&file).unwrap();
    let mut tampered = data.clone();
    *tampered.last_mut().unwrap() ^= 1;
    let cases: Vec<(&str, Vec<u8>, Option<&str>, BackupError)> = vec![
        (
            "没有密码",
            data.clone(),
            None,
            BackupError::PasswordRequired,
        ),
        (
            "密码错误",
            data.clone(),
            Some("wrong-pass"),
            BackupError::WrongPassword,
        ),
        (
            "不是备份文件",
            b"hello".to_vec(),
            Some("backup-pass"),
            BackupError::Invalid,
        ),
        (
            "被改动的文件",
            tampered,
            Some("backup-pass"),
            BackupError::WrongPassword,
        ),
    ];
    for (name, content, password, want) in cases {
        let path = a.path("case.termlet-backup");
        std::fs::write(&path, content).unwrap();
        assert_eq!(
            block_on(a.backups.inspect_backup(path.clone(), password.map(pw))).unwrap_err(),
            want,
            "{name}"
        );
        if let Some(password) = password {
            assert_eq!(restore(&a, &path, password).unwrap_err(), want, "{name}");
        }
    }
    // 导出的主机文件（加密或明文）不能当作完整备份
    let sealed_hosts = envelope::seal(SealedKind::Hosts, &pw("backup-pass"), b"{}").unwrap();
    for content in [
        sealed_hosts,
        br#"{"format": "termlet-hosts", "version": 1}"#.to_vec(),
    ] {
        let path = a.path("hosts.termlet-hosts");
        std::fs::write(&path, content).unwrap();
        for password in [None, Some(pw("backup-pass"))] {
            assert_eq!(
                block_on(a.backups.inspect_backup(path.clone(), password)).unwrap_err(),
                BackupError::IsHostsFile
            );
        }
    }
    assert_eq!(
        block_on(a.backups.inspect_backup(a.path("missing"), None)).unwrap_err(),
        BackupError::FileNotFound {
            path: a.path("missing")
        }
    );
    assert_eq!(a.host_names(), ["web"], "失败的恢复不应改动数据");
}

#[test]
fn backup_from_newer_version_is_rejected() {
    let a = Machine::new();
    let file = a.path("a.termlet-backup");
    create(&a, &file, "backup-pass", false).unwrap();
    let newer = rewrite_backup(&std::fs::read(&file).unwrap(), "backup-pass", |_, m| {
        m.schema = SCHEMA_VERSION + 1;
    });
    std::fs::write(&file, newer).unwrap();
    assert_eq!(
        restore(&a, &file, "backup-pass").unwrap_err(),
        BackupError::TooNew {
            found: SCHEMA_VERSION + 1,
            supported: SCHEMA_VERSION
        }
    );
}

#[test]
fn snapshot_restore_and_foreign_snapshots() {
    let a = Machine::new();
    let web = a.add_host("web", "10.0.0.1", "pw");
    let snap = block_on(a.backups.snapshot(SnapshotReason::Manual)).unwrap();
    assert_eq!(
        (snap.counts.hosts, snap.reason),
        (1, SnapshotReason::Manual)
    );
    a.store.delete::<Host>(&web).unwrap();
    a.add_host("other", "10.0.0.2", "pw");
    let outcome = block_on(a.backups.restore_snapshot(snap.name.clone())).unwrap();
    assert_eq!(outcome.summary.counts.hosts, 1);
    assert_eq!(a.host_names(), ["web"]);
    assert_eq!(a.password(&web).as_deref(), Some("pw"));
    for name in [
        "../x.termlet-snapshot",
        "snapshot-x.txt",
        r"snapshot-..\x.termlet-snapshot",
    ] {
        assert_eq!(
            block_on(a.backups.restore_snapshot(name.into())).unwrap_err(),
            BackupError::InvalidSnapshotName,
            "{name}"
        );
    }
    assert_eq!(
        block_on(
            a.backups
                .restore_snapshot("snapshot-20000101-000000-manual-9.termlet-snapshot".into())
        )
        .unwrap_err(),
        BackupError::SnapshotNotFound
    );

    // 另一台电脑（另一把主密钥）的快照：秘密解不开，拒绝恢复
    let b = Machine::new();
    b.add_host("b-host", "10.0.0.3", "pw");
    let foreign = block_on(b.backups.snapshot(SnapshotReason::Manual)).unwrap();
    let name = "snapshot-20000101-000000-manual.termlet-snapshot";
    std::fs::copy(
        b.backups.dir().join(&foreign.name),
        a.backups.dir().join(name),
    )
    .unwrap();
    assert_eq!(
        block_on(a.backups.restore_snapshot(name.into())).unwrap_err(),
        BackupError::ForeignSnapshot
    );
    assert_eq!(a.host_names(), ["web"], "失败的恢复不应改动数据");
}

#[test]
fn auto_snapshots_are_deduplicated_and_pruned() {
    let a = Machine::new();
    a.add_host("web", "10.0.0.1", "pw");
    a.store
        .update_local_settings(|s| s.backup.keep = 2)
        .unwrap();
    let autos = || -> Vec<SnapshotInfo> {
        snapshots(&a)
            .into_iter()
            .filter(|s| s.reason == SnapshotReason::Auto)
            .collect()
    };
    assert!(block_on(a.backups.auto_snapshot()).unwrap(), "第一次应保存");
    assert!(
        !block_on(a.backups.auto_snapshot()).unwrap(),
        "间隔内不应再保存"
    );

    a.backups.set_auto_interval(Duration::ZERO);
    // 内容没有变化：用新的替换旧的，仍是一份
    assert!(block_on(a.backups.auto_snapshot()).unwrap());
    assert_eq!(autos().len(), 1, "内容相同的自动快照应只保留一份");
    for (i, name) in ["a", "b", "c"].into_iter().enumerate() {
        a.add_host(name, &format!("10.1.0.{i}"), "pw");
        block_on(a.backups.auto_snapshot()).unwrap();
        assert_eq!(autos().len(), (i + 2).min(2), "第 {i} 次");
    }
    assert_eq!(autos()[0].counts.hosts, 4, "最新的自动快照应有 4 台主机");
    // 手动快照不受自动快照清理的影响
    block_on(a.backups.snapshot(SnapshotReason::Manual)).unwrap();
    a.store
        .update_local_settings(|s| s.backup.keep = 1)
        .unwrap();
    a.add_host("d", "10.1.0.9", "pw");
    block_on(a.backups.auto_snapshot()).unwrap();
    let list = snapshots(&a);
    assert_eq!(list.len(), 2, "应剩 1 份自动快照与 1 份手动快照：{list:?}");
    // 关闭后不再保存
    a.store
        .update_local_settings(|s| s.backup.auto = false)
        .unwrap();
    assert!(!block_on(a.backups.auto_snapshot()).unwrap());
    // 没有残留的临时文件
    for entry in std::fs::read_dir(a.backups.dir()).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        assert!(
            !name.starts_with(".tmp-") && !name.ends_with(".part"),
            "残留临时文件 {name}"
        );
    }
}

// 恢复时外部命令行、应用锁、自动快照、日志、使用统计、本机连接设置这几组本机设置保留本机当前的值，本地终端随备份恢复
#[test]
fn restore_keeps_security_related_local_settings() {
    let a = Machine::new();
    a.add_host("web", "10.0.0.1", "pw");
    a.store
        .update_local_settings(|s| {
            s.cli.enabled = true;
            s.app_lock.on_startup = true;
            s.backup.keep = 7;
            s.logs.crash_report = false;
            s.usage.enabled = true;
            s.shells.default = "local:pwsh".into();
            s.connection.allowed_proxy_commands = vec!["from-backup".into()];
        })
        .unwrap();
    let snap = block_on(a.backups.snapshot(SnapshotReason::Manual)).unwrap();
    a.store
        .update_local_settings(|s| {
            s.cli.enabled = false;
            s.app_lock.on_startup = false;
            s.backup.keep = 3;
            s.logs.crash_report = true;
            s.usage.enabled = false;
            s.shells.default = "local:cmd".into();
            s.connection.allowed_proxy_commands = vec!["this-pc".into()];
        })
        .unwrap();
    block_on(a.backups.restore_snapshot(snap.name)).unwrap();
    let local = a.store.local_settings();
    assert!(!local.cli.enabled, "关掉的命令行访问不应被重新打开");
    assert!(!local.app_lock.on_startup);
    assert_eq!(local.backup.keep, 3);
    assert!(local.logs.crash_report);
    assert!(!local.usage.enabled, "关掉的使用统计不应被重新打开");
    assert_eq!(
        local.connection.allowed_proxy_commands,
        ["this-pc"],
        "备份中确认过的代理命令不带到这台电脑"
    );
    assert_eq!(local.shells.default, "local:pwsh", "本地终端随备份恢复");
}

// 旧快照删不掉（被别的程序打开、不允许删除）时新快照照常保存，只是多留一份
#[cfg(windows)]
#[test]
fn snapshot_succeeds_when_old_one_cannot_be_pruned() {
    use std::os::windows::fs::OpenOptionsExt;
    let a = Machine::new();
    a.add_host("web", "10.0.0.1", "pw");
    a.store
        .update_local_settings(|s| s.backup.keep = 1)
        .unwrap();
    block_on(a.backups.snapshot(SnapshotReason::Manual)).unwrap();
    let old = snapshots(&a).remove(0);
    let held = std::fs::OpenOptions::new()
        .read(true)
        // 只允许别人读（列出快照时要读说明），不允许删除
        .share_mode(1)
        .open(a.backups.dir().join(&old.name))
        .unwrap();
    a.add_host("db", "10.0.0.2", "pw");
    block_on(a.backups.snapshot(SnapshotReason::Manual)).unwrap();
    drop(held);
    let names: Vec<String> = snapshots(&a).into_iter().map(|s| s.name).collect();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(names.contains(&old.name));
}

// 升级数据库结构前用正在打开的连接保存快照（旧结构缺少的表按 0 计）
#[test]
fn upgrade_snapshot_from_connection() {
    let a = Machine::new();
    a.add_host("web", "10.0.0.1", "pw");
    let conn = Connection::open(a.path("data.db")).unwrap();
    let info = save_upgrade_snapshot(&conn, a.backups.dir()).unwrap();
    assert_eq!(
        (info.reason, info.counts.hosts),
        (SnapshotReason::Upgrade, 1)
    );

    let dir = tempfile::tempdir().unwrap();
    let old = Connection::open(dir.path().join("old.db")).unwrap();
    old.execute_batch("CREATE TABLE host (id TEXT PRIMARY KEY, deleted_at INTEGER); INSERT INTO host VALUES ('h_1', NULL); PRAGMA user_version = 0;")
        .unwrap();
    let info = save_upgrade_snapshot(&old, a.backups.dir()).unwrap();
    assert_eq!((info.counts.hosts, info.counts.keys), (1, 0));
    let list = snapshots(&a);
    assert_eq!(
        list.iter()
            .filter(|s| s.reason == SnapshotReason::Upgrade)
            .count(),
        2
    );
}

// 恢复前自动保存了一份快照，用它恢复后回到恢复前的状态
#[test]
fn restore_can_be_undone_with_its_snapshot() {
    let a = Machine::new();
    a.add_host("web", "10.0.0.1", "pw");
    let file = a.path("a.termlet-backup");
    create(&a, &file, "backup-pass", false).unwrap();
    let b = Machine::new();
    let outcome = restore(&b, &file, "backup-pass").unwrap();
    assert_eq!(b.host_names(), ["web"]);
    assert_eq!(outcome.snapshot.counts.hosts, 0);
    let manual = block_on(b.backups.snapshot(SnapshotReason::Manual)).unwrap();
    assert_eq!(manual.reason, SnapshotReason::Manual);
    block_on(b.backups.restore_snapshot(outcome.snapshot.name)).unwrap();
    assert!(
        b.host_names().is_empty(),
        "用快照恢复后应回到没有主机的状态"
    );
    b.backups.clean_temp();
}
