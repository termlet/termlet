use super::*;
use crate::model::{ForwardKind, SecretField};
use crate::secret::SecretString;
use crate::testkit::SharedKeys;

struct Fixture {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
    keys: SharedKeys,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.db");
        Self {
            _dir: dir,
            path,
            keys: SharedKeys::default(),
        }
    }

    fn open(&self) -> Store {
        let db = Database::open(&self.path, &self.path.with_file_name("backups")).unwrap();
        let vault = Vault::open(&db, Box::new(self.keys.clone())).unwrap();
        Store::open(db, vault).unwrap()
    }
}

fn host(address: &str) -> Host {
    Host {
        address: address.into(),
        ..Default::default()
    }
}

#[test]
fn hosts_persist_across_reopen() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let group = store
        .create(
            Group {
                name: "生产".into(),
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    let web = store
        .create(host("10.0.0.1"), Some(group.to_string()), Position::Last)
        .unwrap();
    let db = store
        .create(host("10.0.0.2"), Some(group.to_string()), Position::First)
        .unwrap();
    drop(store);

    let store = fixture.open();
    let hosts = store.list::<Host>();
    assert_eq!(
        hosts.iter().map(|h| &h.id).collect::<Vec<_>>(),
        vec![&db, &web],
        "Position::First 排在前面"
    );
    assert_eq!(hosts[0].doc.name, "10.0.0.2");
    assert_eq!(hosts[0].parent.as_deref(), Some(group.as_str()));
}

#[test]
fn update_changes_doc_and_emits_event() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let id = store.create(host("a"), None, Position::Last).unwrap();
    let mut events = store.subscribe();
    let mut doc = store.get::<Host>(&id).unwrap().doc;
    doc.name = "web".into();
    store.update(&id, doc).unwrap();
    assert_eq!(store.get::<Host>(&id).unwrap().doc.name, "web");
    assert_eq!(events.try_recv().unwrap(), StoreEvent::Changed(Kind::Host));
}

#[test]
fn invalid_doc_is_rejected_without_changes() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let err = store
        .create(host("bad host"), None, Position::Last)
        .unwrap_err();
    assert!(matches!(
        err,
        CoreError::Invalid(Invalid {
            field: "address",
            ..
        })
    ));
    assert!(store.list::<Host>().is_empty());
}

#[test]
fn move_reorders_within_and_across_groups() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let a = store.create(host("a"), None, Position::Last).unwrap();
    let b = store.create(host("b"), None, Position::Last).unwrap();
    let c = store.create(host("c"), None, Position::Last).unwrap();
    store
        .move_to::<Host>(&c, None, Position::Before(a.clone()))
        .unwrap();
    let order: Vec<_> = store.list::<Host>().into_iter().map(|h| h.id).collect();
    assert_eq!(order, vec![c.clone(), a.clone(), b.clone()]);

    let group = store
        .create(
            Group {
                name: "g".into(),
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    store
        .move_to::<Host>(&a, Some(group.to_string()), Position::Last)
        .unwrap();
    assert_eq!(
        store.get::<Host>(&a).unwrap().parent.as_deref(),
        Some(group.as_str())
    );
    let missing = store.move_to::<Host>(&b, Some("g_missing".into()), Position::Last);
    assert!(missing.is_err());
}

// 服务器列表的“复制”：内容与登录用的秘密照抄、放在原来那台后面；收藏、命令行工具的授权、端口转发不带过去
#[test]
fn duplicate_host_copies_content_and_login_secrets() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let group = store
        .create(
            Group {
                name: "g".into(),
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    let parent = Some(group.to_string());
    let mut original = host("10.0.0.1");
    original.name = "web".into();
    original.port = 2222;
    original.username = "deploy".into();
    original.favorite = true;
    original.note = "备注".into();
    original.cli.allowed = true;
    let a = store
        .create(original, parent.clone(), Position::Last)
        .unwrap();
    let b = store
        .create(host("10.0.0.2"), parent.clone(), Position::Last)
        .unwrap();
    store
        .create(
            Forward {
                kind: ForwardKind::Local,
                host: Some(a.clone()),
                bind_port: 8080,
                target_host: "127.0.0.1".into(),
                target_port: 80,
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    let owner = Owner::host(&a);
    for (field, value) in [
        (SecretField::Password, "pw"),
        (SecretField::Totp, "JBSWY3DPEHPK3PXP"),
        (SecretField::CliPassword, "cli"),
    ] {
        store
            .set_secret(&owner, field, Some(&SecretString::new(value)))
            .unwrap();
    }

    let copy = store.duplicate_host(&a, "web - 副本".into()).unwrap();
    let record = store.get::<Host>(&copy).unwrap();
    assert_eq!(record.parent, parent, "在同一分组");
    let doc = record.doc;
    assert_eq!(doc.name, "web - 副本");
    assert_eq!(
        (doc.address.as_str(), doc.port, doc.username.as_str()),
        ("10.0.0.1", 2222, "deploy")
    );
    assert_eq!(doc.note, "备注");
    assert!(!doc.favorite, "副本不在收藏中");
    assert!(!doc.cli.allowed, "命令行工具的授权不带过去");
    let order: Vec<_> = store
        .list::<Host>()
        .into_iter()
        .filter(|h| h.parent == parent)
        .map(|h| h.id)
        .collect();
    assert_eq!(order, vec![a.clone(), copy.clone(), b], "放在原来那台后面");

    let copied = Owner::host(&copy);
    let read = |field| {
        store
            .secret(&copied, field)
            .unwrap()
            .map(|s| s.expose().to_string())
    };
    assert_eq!(read(SecretField::Password).as_deref(), Some("pw"));
    assert_eq!(read(SecretField::Totp).as_deref(), Some("JBSWY3DPEHPK3PXP"));
    assert_eq!(read(SecretField::CliPassword), None);
    let forwards = store.list::<Forward>();
    assert_eq!(forwards.len(), 1, "端口转发不复制");
    assert_eq!(forwards[0].doc.host.as_ref(), Some(&a));
    // 原来那台不变
    assert!(store.get::<Host>(&a).unwrap().doc.favorite);
    assert!(store.has_secret(&owner, SecretField::CliPassword));
}

#[test]
fn group_cannot_become_its_own_ancestor() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let group = |name: &str| Group {
        name: name.into(),
        ..Default::default()
    };
    let outer = store.create(group("outer"), None, Position::Last).unwrap();
    let inner = store
        .create(group("inner"), Some(outer.to_string()), Position::Last)
        .unwrap();
    let err = store
        .move_to::<Group>(&outer, Some(inner.to_string()), Position::Last)
        .unwrap_err();
    assert!(matches!(
        err,
        CoreError::Invalid(Invalid {
            reason: "cycle",
            ..
        })
    ));
    assert_eq!(store.group_chain(Some(&inner)).len(), 2);
}

#[test]
fn jump_chain_is_checked() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let bastion = store.create(host("bastion"), None, Position::Last).unwrap();
    let mut target = host("target");
    target.jump = Jump::Via {
        host: bastion.clone(),
    };
    let target = store.create(target, None, Position::Last).unwrap();
    // bastion 经 target 跳转会形成循环
    let mut doc = store.get::<Host>(&bastion).unwrap().doc;
    doc.jump = Jump::Via {
        host: target.clone(),
    };
    let err = store.update(&bastion, doc).unwrap_err();
    assert!(matches!(
        err,
        CoreError::Invalid(Invalid {
            reason: "cycle",
            ..
        })
    ));
}

#[test]
fn deleting_host_removes_forwards_and_secrets() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let id = store.create(host("a"), None, Position::Last).unwrap();
    let forward = store
        .create(
            Forward {
                kind: ForwardKind::Local,
                host: Some(id.clone()),
                bind_port: 8080,
                target_host: "127.0.0.1".into(),
                target_port: 80,
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    let owner = Owner::host(&id);
    store
        .set_secret(
            &owner,
            SecretField::Password,
            Some(&SecretString::new("pw")),
        )
        .unwrap();
    assert!(store.has_secret(&owner, SecretField::Password));

    store.delete::<Host>(&id).unwrap();
    assert!(store.get::<Host>(&id).is_none());
    assert!(store.get::<Forward>(&forward).is_none());
    assert!(!store.has_secret(&owner, SecretField::Password));
    drop(store);

    // 墓碑写进了数据库，重新打开后仍然是删除状态
    let store = fixture.open();
    assert!(store.list::<Host>().is_empty());
    assert!(store.list::<Forward>().is_empty());
    let deleted = store.read().hosts.deleted_at(id.as_str());
    assert!(deleted.is_some());
    let secrets: i64 = store
        .database()
        .read(|c| c.query_row("SELECT COUNT(*) FROM secret", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(secrets, 0);
}

// 删除跳板机后，经它连接的主机、分组不留悬空的引用；
// 它的命令历史一并删除
#[test]
fn deleting_jump_host_clears_references_and_history() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let bastion = store.create(host("bastion"), None, Position::Last).unwrap();
    let group = store
        .create(
            Group {
                name: "prod".into(),
                defaults: crate::model::GroupDefaults {
                    jump: Some(bastion.clone()),
                    ..Default::default()
                },
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    let target = store
        .create(
            Host {
                jump: Jump::Via {
                    host: bastion.clone(),
                },
                ..host("db")
            },
            None,
            Position::Last,
        )
        .unwrap();
    crate::history::record(&store, bastion.as_str(), "uptime").unwrap();
    let mut events = store.subscribe();

    store.delete::<Host>(&bastion).unwrap();
    assert_eq!(store.get::<Host>(&target).unwrap().doc.jump, Jump::Inherit);
    assert_eq!(store.get::<Group>(&group).unwrap().doc.defaults.jump, None);
    let mut changed = Vec::new();
    while let Ok(event) = events.try_recv() {
        changed.push(event);
    }
    assert!(changed.contains(&StoreEvent::Changed(Kind::Host)));
    assert!(changed.contains(&StoreEvent::Changed(Kind::Group)));
    let history: i64 = store
        .database()
        .read(|c| c.query_row("SELECT COUNT(*) FROM command_history", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(history, 0);
    drop(store);

    let store = fixture.open();
    assert_eq!(store.get::<Host>(&target).unwrap().doc.jump, Jump::Inherit);
    assert_eq!(store.get::<Group>(&group).unwrap().doc.defaults.jump, None);
}

#[test]
fn deleting_group_ungroups_its_hosts_and_removes_subgroups() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let group = |name: &str| Group {
        name: name.into(),
        ..Default::default()
    };
    let outer = store.create(group("outer"), None, Position::Last).unwrap();
    let inner = store
        .create(group("inner"), Some(outer.to_string()), Position::Last)
        .unwrap();
    let h = store
        .create(host("a"), Some(inner.to_string()), Position::Last)
        .unwrap();
    store.delete::<Group>(&outer).unwrap();
    assert!(store.list::<Group>().is_empty());
    assert_eq!(store.get::<Host>(&h).unwrap().parent, None);
}

#[test]
fn profile_in_use_cannot_be_deleted() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let profile = store
        .create(
            Profile {
                name: "运维".into(),
                username: "ops".into(),
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    let mut h = host("a");
    h.auth = Auth::Profile {
        profile: profile.clone(),
    };
    let h = store.create(h, None, Position::Last).unwrap();
    let err = store.delete::<Profile>(&profile).unwrap_err();
    assert!(matches!(
        err,
        CoreError::Invalid(Invalid {
            reason: "in_use",
            ..
        })
    ));
    store.delete::<Host>(&h).unwrap();
    store.delete::<Profile>(&profile).unwrap();
}

#[test]
fn reference_to_missing_profile_is_rejected() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let mut h = host("a");
    h.auth = Auth::Profile {
        profile: ProfileId::from_raw("p_missing"),
    };
    assert!(store.create(h, None, Position::Last).is_err());
}

#[test]
fn secrets_round_trip_and_never_appear_in_docs() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let id = store.create(host("a"), None, Position::Last).unwrap();
    let owner = Owner::host(&id);
    store
        .set_secret(
            &owner,
            SecretField::Password,
            Some(&SecretString::new("s3cret")),
        )
        .unwrap();
    drop(store);

    let store = fixture.open();
    let value = store
        .secret(&owner, SecretField::Password)
        .unwrap()
        .unwrap();
    assert_eq!(value.expose(), "s3cret");
    let docs: Vec<String> = store
        .database()
        .read(|c| {
            let mut stmt = c.prepare("SELECT doc FROM host")?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            rows.collect()
        })
        .unwrap();
    assert!(docs.iter().all(|d| !d.contains("s3cret")));

    store
        .set_secret(&owner, SecretField::Password, None)
        .unwrap();
    assert_eq!(store.secret(&owner, SecretField::Password).unwrap(), None);
}

#[test]
fn reset_vault_clears_secrets_but_keeps_hosts() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let id = store.create(host("a"), None, Position::Last).unwrap();
    store
        .set_secret(&Owner::host(&id), SecretField::Password, Some(&"pw".into()))
        .unwrap();
    drop(store);

    // 换到另一台电脑：钥匙串里没有主密钥
    let other = Fixture {
        _dir: tempfile::tempdir().unwrap(),
        path: fixture.path.clone(),
        keys: SharedKeys::default(),
    };
    let store = other.open();
    assert_eq!(store.vault_status(), VaultStatus::KeyMissing);
    assert!(matches!(
        store.secret(&Owner::host(&id), SecretField::Password),
        Err(CoreError::VaultLocked)
    ));
    store.reset_vault(None).unwrap();
    assert!(store.vault().is_unlocked());
    assert_eq!(store.list::<Host>().len(), 1);
    assert!(!store.has_secret(&Owner::host(&id), SecretField::Password));
}

#[test]
fn settings_write_only_changed_sections_and_persist() {
    let fixture = Fixture::new();
    let store = fixture.open();
    store
        .update_settings(|s| s.terminal.font_size = 16)
        .unwrap();
    store
        .update_local_settings(|s| s.cli.enabled = true)
        .unwrap();
    let rows: Vec<String> = store
        .database()
        .read(|c| {
            let mut stmt = c.prepare("SELECT id FROM setting")?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            rows.collect()
        })
        .unwrap();
    assert_eq!(rows, vec!["terminal"]);
    drop(store);

    let store = fixture.open();
    assert_eq!(store.settings().terminal.font_size, 16);
    assert_eq!(store.settings().terminal.scrollback, 5000);
    assert!(store.local_settings().cli.enabled);
}

// 0.3.3 之前保存的、与当时的默认相同的高亮规则：读取时升级成新的默认（含 IPv6），数据库中的一行不动；
// 云同步写入同样的旧内容时也升级（写库的是升级后的内容）
#[test]
fn old_default_highlight_rules_upgrade_when_loaded_and_synced() {
    use crate::db::{self, SettingRow};
    use crate::model::HighlightSettings;
    use crate::store::sync::RawChange;
    let rule = |pattern: &str, color: &str| {
        serde_json::json!({
            "pattern": pattern, "regex": true, "caseSensitive": false, "color": color, "enabled": true
        })
    };
    let legacy = |enabled: bool| {
        serde_json::json!({"enabled": enabled, "rules": [
            rule(
                r"\b(error|errors|fail|failed|failure|fatal|panic|denied|refused|exception|critical)\b",
                "red",
            ),
            rule(r"\b(warn|warning|warnings|deprecated)\b", "yellow"),
            rule(r"\b(ok|success|successful|succeeded|passed)\b", "green"),
            rule(r"\b(?:\d{1,3}\.){3}\d{1,3}(?:/\d{1,2})?\b", "cyan"),
        ]})
    };
    let fixture = Fixture::new();
    let store = fixture.open();
    let row = SettingRow {
        id: "highlight".into(),
        doc: legacy(true).to_string(),
        updated_at: 1,
    };
    store
        .database()
        .write(|tx| Ok(db::put_setting(tx, "setting", &row)?))
        .unwrap();
    drop(store);

    let store = fixture.open();
    assert_eq!(store.settings().highlight, HighlightSettings::default());
    let stored = |store: &Store| -> String {
        store
            .database()
            .read(|c| {
                c.query_row("SELECT doc FROM setting WHERE id = 'highlight'", [], |r| {
                    r.get(0)
                })
            })
            .unwrap()
    };
    assert_eq!(stored(&store), row.doc, "读取时升级，不写库");

    store
        .sync_apply(
            |_| true,
            &[RawChange::PutSetting {
                id: "highlight".into(),
                doc: legacy(false),
                updated_at: 5,
                keep_local: &[],
            }],
        )
        .unwrap();
    let highlight = store.settings().highlight;
    assert!(!highlight.enabled);
    assert_eq!(highlight.rules, HighlightSettings::default().rules);
    let doc: serde_json::Value = serde_json::from_str(&stored(&store)).unwrap();
    assert_eq!(doc["rulesVersion"], 2);
    assert_eq!(doc["rules"].as_array().unwrap().len(), 5);
}

// 使用统计的开关是本机设置（`local_setting` 的 usage 一行），不进同步的 `setting` 表
#[test]
fn usage_settings_are_local_and_persist() {
    let fixture = Fixture::new();
    let store = fixture.open();
    assert!(store.local_settings().usage.enabled, "默认发送");
    store
        .update_local_settings(|s| s.usage.enabled = false)
        .unwrap();
    let ids = |table: &str| -> Vec<String> {
        store
            .database()
            .read(|c| {
                let mut stmt = c.prepare(&format!("SELECT id FROM {table}"))?;
                let rows = stmt.query_map([], |r| r.get(0))?;
                rows.collect()
            })
            .unwrap()
    };
    assert_eq!(ids("local_setting"), vec!["usage"]);
    assert!(ids("setting").is_empty());
    drop(store);

    let store = fixture.open();
    assert!(!store.local_settings().usage.enabled);
}

#[test]
fn unknown_fields_survive_reload_and_update() {
    let fixture = Fixture::new();
    let store = fixture.open();
    let id = store.create(host("a"), None, Position::Last).unwrap();
    // 模拟更新版本的 Termlet 写入了本版本不认识的字段
    store
        .database()
        .write(|tx| {
            tx.execute(
                "UPDATE host SET doc = json_set(doc, '$.futureOption', 42) WHERE id = ?1",
                [id.as_str()],
            )?;
            Ok(())
        })
        .unwrap();
    drop(store);

    let store = fixture.open();
    let mut doc = store.get::<Host>(&id).unwrap().doc;
    doc.name = "renamed".into();
    store.update(&id, doc).unwrap();
    let raw: String = store
        .database()
        .read(|c| {
            c.query_row("SELECT doc FROM host WHERE id = ?1", [id.as_str()], |r| {
                r.get(0)
            })
        })
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(value["futureOption"], 42);
    assert_eq!(value["name"], "renamed");
}
