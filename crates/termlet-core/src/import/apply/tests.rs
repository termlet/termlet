//! 导入执行的测试。

use super::*;
use crate::backup::testkit::Machine;
use crate::import::plan::PlanBuilder;
use crate::import::{JumpRef, KeyAlgorithm, PlannedKey, Source};
use crate::keyvault::KeyKind;
use crate::model::{Encoding, KeyEntry, Proxy, ProxyKind, SshOptions, TerminalOptions};
use crate::store::StoreEvent;
use crate::testkit::{SharedKeys, block_on};

fn host(name: &str, address: &str, port: u16, username: &str) -> Host {
    Host {
        name: name.into(),
        address: address.into(),
        port,
        username: username.into(),
        ..Default::default()
    }
}

fn planned(key: &str, host: Host) -> PlannedHost {
    PlannedHost::new(key, host)
}

fn jump(name: &str, address: &str, port: u16, username: &str) -> Option<JumpRef> {
    Some(JumpRef::new(name, Endpoint::new(address, port, username)))
}

/// 导入全部主机。
fn run(m: &Machine, plan: ImportPlan, on_duplicate: OnDuplicate) -> ImportReport {
    let keys: Vec<String> = plan.hosts.iter().map(|h| h.key.clone()).collect();
    block_on(apply(m.backups.clone(), plan, keys, on_duplicate)).unwrap()
}

fn plan_of(hosts: Vec<PlannedHost>) -> ImportPlan {
    let mut b = PlanBuilder::new(Source::SshConfig);
    for h in hosts {
        b.push(h);
    }
    b.finish()
}

fn counts(report: &ImportReport) -> (usize, usize, usize, usize) {
    (
        report.created,
        report.merged,
        report.skipped,
        report.failures.len(),
    )
}

fn jump_of(m: &Machine, name: &str) -> Option<HostId> {
    match m.host_named(name).doc.jump {
        Jump::Via { host } => Some(host),
        _ => None,
    }
}

// 跳过与合并。没有用户名的主机照样导入（连接时与 OpenSSH 相同，沿用分组的默认用户名
// 或本机的用户名；预览中有"缺少用户名"的提示）
#[test]
fn skip_and_merge() {
    let m = Machine::new();
    let existing = m
        .store
        .create(
            Host {
                tags: vec!["keep".into()],
                favorite: true,
                ..host("老名字", "10.0.0.1", 0, "root")
            },
            None,
            Position::Last,
        )
        .unwrap();
    m.set_password(&existing, "old-pw");

    let mut b = PlanBuilder::new(Source::SshConfig);
    let group = b.group(&["导入", "生产"]);
    let mut a = planned("a", host("新名字", "10.0.0.1", 22, "root"));
    a.group = group;
    b.push(a);
    b.push(planned(
        "b",
        Host {
            auth: Auth::Key {
                source: KeySource::file("/k/db"),
            },
            ..host("db", "10.0.0.2", 2222, "dba")
        },
    ));
    let mut c = planned("c", host("无用户名", "10.0.0.3", 22, ""));
    c.hints.push(Hint::MissingUsername);
    b.push(c);
    let plan = b.finish();

    let items = preview(&m.store, &plan).items;
    assert_eq!(items[0].duplicate_of.as_ref(), Some(&existing));
    assert_eq!(items[1].duplicate_of, None);
    assert_eq!(items[0].group, ["导入", "生产"]);
    assert_eq!(items[2].hints, [Hint::MissingUsername]);

    let report = run(&m, plan.clone(), OnDuplicate::Skip);
    assert_eq!(counts(&report), (2, 0, 1, 0));
    assert_eq!(
        report.hosts[0].outcome,
        HostOutcome::Skipped(existing.clone())
    );
    assert_eq!(
        m.store.get::<Host>(&existing).unwrap().doc.name,
        "老名字",
        "跳过时不应修改已有主机"
    );

    let mut first = plan.clone();
    first.hosts.truncate(1);
    let report = run(&m, first, OnDuplicate::Merge);
    assert_eq!(counts(&report), (0, 1, 0, 0));
    let merged = m.store.get::<Host>(&existing).unwrap();
    assert_eq!(merged.doc.name, "新名字");
    assert!(merged.parent.is_some(), "合并时应移进导入的分组");
    assert!(merged.doc.favorite && merged.doc.tags == ["keep"]);
    assert_eq!(m.password(&existing).as_deref(), Some("old-pw"));

    let db = m.host_named("db");
    assert_eq!(db.doc.port, 2222);
    assert_eq!(
        db.doc.auth,
        Auth::Key {
            source: KeySource::file("/k/db")
        }
    );
    assert_eq!(m.host_named("无用户名").doc.username, "");
}

#[test]
fn jump_hosts_are_resolved_after_all_hosts() {
    let m = Machine::new();
    // 已存在的跳板机：预览时被标记为重复，导入的主机仍可引用它
    let bastion = m
        .store
        .create(
            host("已有堡垒机", "bastion.example.com", 2222, "jump"),
            None,
            Position::Last,
        )
        .unwrap();
    let with_jump = |key: &str, h: Host, j: Option<JumpRef>| {
        let mut p = planned(key, h);
        p.jump = j;
        p
    };
    let plan = plan_of(vec![
        // 引用排在后面的主机
        with_jump(
            "app",
            host("app", "10.0.0.1", 22, "app"),
            jump("gw", "gw", 22, "ops"),
        ),
        // 地址不区分大小写
        with_jump(
            "gw",
            host("gw", "gw", 22, "ops"),
            jump("bastion", "BASTION.example.com", 2222, "jump"),
        ),
        with_jump(
            "lost",
            host("lost", "10.0.0.2", 22, "u"),
            jump("missing", "missing", 22, "x"),
        ),
        with_jump(
            "self",
            host("self", "10.0.0.3", 22, "s"),
            jump("self", "10.0.0.3", 22, "s"),
        ),
        // 互相引用：先设置的生效，后一个会形成循环，保持原样
        with_jump(
            "a",
            host("loop-a", "10.0.1.1", 22, "a"),
            jump("b", "10.0.1.2", 22, "b"),
        ),
        with_jump(
            "b",
            host("loop-b", "10.0.1.2", 22, "b"),
            jump("a", "10.0.1.1", 22, "a"),
        ),
    ]);
    let report = run(&m, plan, OnDuplicate::Skip);
    assert_eq!(counts(&report), (6, 0, 0, 0));
    let gw = m.host_named("gw").id;
    assert_eq!(jump_of(&m, "app"), Some(gw.clone()));
    assert_eq!(jump_of(&m, "gw"), Some(bastion));
    assert_eq!(jump_of(&m, "lost"), None, "找不到跳板机时保持原样");
    assert_eq!(jump_of(&m, "self"), None, "引用自己时保持原样");
    assert!(jump_of(&m, "loop-a").is_some());
    assert_eq!(jump_of(&m, "loop-b"), None, "会形成循环的跳板机应被忽略");

    // 合并：导入项没有跳板机时保留已有主机的跳板机；有跳板机时更新
    let report = run(
        &m,
        plan_of(vec![
            planned("app", host("app-2", "10.0.0.1", 22, "app")),
            with_jump(
                "lost",
                host("lost-2", "10.0.0.2", 22, "u"),
                jump("gw", "gw", 22, "ops"),
            ),
        ]),
        OnDuplicate::Merge,
    );
    assert_eq!(report.merged, 2);
    assert_eq!(jump_of(&m, "app-2"), Some(gw.clone()));
    assert_eq!(jump_of(&m, "lost-2"), Some(gw));
}

#[test]
fn agent_and_tags() {
    let m = Machine::new();
    let old = m
        .store
        .create(
            Host {
                tags: vec!["keep".into()],
                ..host("old", "10.0.0.9", 22, "u")
            },
            None,
            Position::Last,
        )
        .unwrap();
    m.set_password(&old, "pw");
    let report = run(
        &m,
        plan_of(vec![
            planned(
                "a",
                Host {
                    auth: Auth::Agent,
                    tags: vec!["mysql".into(), "生产".into()],
                    ..host("agent-host", "10.0.0.8", 22, "ops")
                },
            ),
            planned(
                "b",
                Host {
                    auth: Auth::Agent,
                    tags: vec!["new".into()],
                    ..host("merged", "10.0.0.9", 22, "u")
                },
            ),
        ]),
        OnDuplicate::Merge,
    );
    assert_eq!(counts(&report), (1, 1, 0, 0));
    let agent = m.host_named("agent-host");
    assert_eq!(agent.doc.auth, Auth::Agent);
    assert_eq!(agent.doc.tags.len(), 2);
    let merged = m.store.get::<Host>(&old).unwrap();
    assert_eq!(merged.doc.name, "merged");
    assert_eq!(merged.doc.auth, Auth::Password, "合并时应保留已有的密码");
    assert_eq!(merged.doc.tags, ["keep"]);
    assert_eq!(m.password(&old).as_deref(), Some("pw"));
}

// 来源中的密码、代理密码随新主机保存；合并时已有的密码不被覆盖，还没有凭据的用来源中的补上
#[test]
fn secrets_are_saved_or_filled_in() {
    let m = Machine::new();
    let with_password = m
        .store
        .create(host("has-pw", "10.0.2.1", 22, "u"), None, Position::Last)
        .unwrap();
    m.set_password(&with_password, "old-pw");
    let empty = m
        .store
        .create(host("no-cred", "10.0.2.2", 22, "u"), None, Position::Last)
        .unwrap();
    let proxy = Proxy {
        kind: ProxyKind::Socks5,
        address: "127.0.0.1".into(),
        port: 1080,
        username: "pu".into(),
        command: String::new(),
    };
    let mut new = planned(
        "n",
        Host {
            proxy: Some(proxy),
            ..host("new", "10.0.2.3", 22, "u")
        },
    );
    new.set_password(SecretString::new("new-pw"));
    new.set_proxy_password(SecretString::new("proxy-pw"));
    let mut a = planned("a", host("has-pw", "10.0.2.1", 22, "u"));
    a.set_password(SecretString::new("other"));
    let mut b = planned("b", host("no-cred", "10.0.2.2", 22, "u"));
    b.set_password(SecretString::new("filled"));
    let plan = plan_of(vec![new, a, b]);
    assert!(preview(&m.store, &plan).items[0].has_password);
    let report = run(&m, plan, OnDuplicate::Merge);
    assert_eq!(counts(&report), (1, 2, 0, 0));

    let created = m.host_named("new").id;
    assert_eq!(m.password(&created).as_deref(), Some("new-pw"));
    let proxy_password = m
        .store
        .secret(&Owner::host(&created), SecretField::ProxyPassword)
        .unwrap();
    assert_eq!(proxy_password.unwrap().expose(), "proxy-pw");
    assert_eq!(m.password(&with_password).as_deref(), Some("old-pw"));
    assert_eq!(m.password(&empty).as_deref(), Some("filled"));
}

// 验证码密钥（NyaTerm）：新建时保存；合并时只补上还没有的
#[test]
fn totp_secrets() {
    let m = Machine::new();
    let totp = |id: &HostId| {
        m.store
            .secret(&Owner::host(id), SecretField::Totp)
            .unwrap()
            .map(|s| s.expose().to_string())
    };
    let kept = m
        .store
        .create(host("kept", "10.0.6.1", 22, "u"), None, Position::Last)
        .unwrap();
    m.store
        .set_secret(
            &Owner::host(&kept),
            SecretField::Totp,
            Some(&SecretString::new("KEEPKEEPKEEPKEEP")),
        )
        .unwrap();
    let filled = m
        .store
        .create(host("filled", "10.0.6.2", 22, "u"), None, Position::Last)
        .unwrap();
    let with_totp = |key: &str, h: Host, secret: &str| {
        let mut p = planned(key, h);
        p.set_totp(SecretString::new(secret));
        p
    };
    let plan = plan_of(vec![
        with_totp("n", host("new", "10.0.6.3", 22, "u"), "JBSWY3DPEHPK3PXP"),
        with_totp("k", host("kept", "10.0.6.1", 22, "u"), "OTHEROTHEROTHER2"),
        with_totp("f", host("filled", "10.0.6.2", 22, "u"), "FILLFILLFILLFILL"),
    ]);
    assert!(plan.has_secrets());
    assert!(!plan.without_secrets().has_secrets());
    let report = run(&m, plan, OnDuplicate::Merge);
    assert_eq!(counts(&report), (1, 2, 0, 0));
    assert_eq!(
        totp(&m.host_named("new").id).as_deref(),
        Some("JBSWY3DPEHPK3PXP")
    );
    assert_eq!(totp(&kept).as_deref(), Some("KEEPKEEPKEEPKEEP"));
    assert_eq!(totp(&filled).as_deref(), Some("FILLFILLFILLFILL"));
}

// 密码要用户同意才导入的来源（Electerm、Netcatty）：跳过重复的主机时补上它还没保存的密码，已保存的不覆盖；
// 合并时已保存的代理密码不覆盖；不同意时（without_secrets）什么秘密都不写
#[test]
fn consent_sources_only_fill_missing_secrets() {
    let m = Machine::new();
    let proxy = Proxy {
        kind: ProxyKind::Socks5,
        address: "127.0.0.1".into(),
        port: 1080,
        username: "pu".into(),
        command: String::new(),
    };
    let with_password = m
        .store
        .create(host("has-pw", "10.0.3.1", 22, "u"), None, Position::Last)
        .unwrap();
    m.set_password(&with_password, "old-pw");
    let empty = m
        .store
        .create(host("no-pw", "10.0.3.2", 22, "u"), None, Position::Last)
        .unwrap();
    let agent = m
        .store
        .create(
            Host {
                auth: Auth::Agent,
                ..host("agent", "10.0.3.3", 22, "u")
            },
            None,
            Position::Last,
        )
        .unwrap();
    let proxied = m
        .store
        .create(
            Host {
                proxy: Some(proxy.clone()),
                ..host("proxied", "10.0.3.4", 22, "u")
            },
            None,
            Position::Last,
        )
        .unwrap();
    m.store
        .set_secret(
            &Owner::host(&proxied),
            SecretField::ProxyPassword,
            Some(&SecretString::new("old-proxy")),
        )
        .unwrap();

    let plan = |source: Source| {
        let mut b = PlanBuilder::new(source);
        for (key, name, address) in [
            ("a", "has-pw", "10.0.3.1"),
            ("b", "no-pw", "10.0.3.2"),
            ("c", "agent", "10.0.3.3"),
        ] {
            let mut item = planned(key, host(name, address, 22, "u"));
            item.set_password(SecretString::new(format!("{name}-new")));
            b.push(item);
        }
        let mut d = planned(
            "d",
            Host {
                proxy: Some(proxy.clone()),
                ..host("proxied", "10.0.3.4", 22, "u")
            },
        );
        d.set_proxy_password(SecretString::new("new-proxy"));
        b.push(d);
        b.finish()
    };

    // 不同意：去掉秘密后跳过，什么都不写
    let report = run(
        &m,
        plan(Source::Netcatty).without_secrets(),
        OnDuplicate::Skip,
    );
    assert_eq!(
        (counts(&report), report.passwords_filled),
        ((0, 0, 4, 0), 0)
    );
    assert_eq!(m.password(&empty), None);

    // 同意、跳过：只补还没保存密码、用密码登录的
    let report = run(&m, plan(Source::Netcatty), OnDuplicate::Skip);
    assert_eq!(
        (counts(&report), report.passwords_filled),
        ((0, 0, 4, 0), 1)
    );
    assert_eq!(m.password(&with_password).as_deref(), Some("old-pw"));
    assert_eq!(m.password(&empty).as_deref(), Some("no-pw-new"));
    assert_eq!(m.password(&agent), None);
    assert_eq!(m.host_named("agent").doc.auth, Auth::Agent);

    // 合并：已保存的代理密码不覆盖（别的来源照旧更新）
    let proxy_password = |m: &Machine| {
        m.store
            .secret(&Owner::host(&proxied), SecretField::ProxyPassword)
            .unwrap()
            .unwrap()
            .expose()
            .to_string()
    };
    run(&m, plan(Source::Electerm), OnDuplicate::Merge);
    assert_eq!(proxy_password(&m), "old-proxy");
    assert_eq!(m.password(&with_password).as_deref(), Some("old-pw"));
    let report = run(&m, plan(Source::Tabby), OnDuplicate::Skip);
    assert_eq!(report.passwords_filled, 0, "其他来源跳过时不补");
    run(&m, plan(Source::Tabby), OnDuplicate::Merge);
    assert_eq!(proxy_password(&m), "new-proxy");
}

// 合并保留已有主机的连接与终端参数与直连设置；选用凭据档案的主机保持使用档案，
// 来源中的密码、私钥文件都不覆盖它
#[test]
fn merge_keeps_settings_and_profile() {
    let m = Machine::new();
    let profile = m
        .store
        .create(
            Profile {
                name: "ops".into(),
                username: "u".into(),
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    let terminal = TerminalOptions {
        encoding: Encoding::Gb18030,
        startup: "cd /srv".into(),
        ..Default::default()
    };
    let ssh = SshOptions {
        legacy_algorithms: true,
        agent_forward: true,
        ..Default::default()
    };
    let old = m
        .store
        .create(
            Host {
                jump: Jump::Direct,
                auth: Auth::Profile {
                    profile: profile.clone(),
                },
                terminal: terminal.clone(),
                ssh: ssh.clone(),
                ..host("old", "10.0.3.1", 22, "u")
            },
            None,
            Position::Last,
        )
        .unwrap();
    let mut with_password = planned("a", host("merged", "10.0.3.1", 22, "u"));
    with_password.set_password(SecretString::new("src-pw"));
    let key_file = planned(
        "b",
        Host {
            auth: Auth::Key {
                source: KeySource::file("/k/src"),
            },
            ..host("merged-2", "10.0.3.1", 22, "u")
        },
    );
    for item in [with_password, key_file] {
        let name = item.host.name.clone();
        let report = run(&m, plan_of(vec![item]), OnDuplicate::Merge);
        assert_eq!(report.merged, 1);
        let h = m.store.get::<Host>(&old).unwrap().doc;
        assert_eq!(h.name, name);
        assert_eq!(
            (h.terminal.clone(), h.ssh.clone(), h.jump.clone()),
            (terminal.clone(), ssh.clone(), Jump::Direct)
        );
        assert_eq!(
            h.auth,
            Auth::Profile {
                profile: profile.clone()
            },
            "应保留凭据档案"
        );
        assert_eq!(m.password(&old), None);
    }
}

// 代理直接导入；合并到已有主机时保留其 tmux 等设置
#[test]
fn proxy_is_imported_and_merge_keeps_tmux() {
    let m = Machine::new();
    let old = m
        .store
        .create(
            Host {
                terminal: TerminalOptions {
                    tmux: true,
                    ..Default::default()
                },
                ..host("old", "10.0.0.1", 22, "root")
            },
            None,
            Position::Last,
        )
        .unwrap();
    m.set_password(&old, "pw");
    let proxy = Proxy {
        kind: ProxyKind::Socks5,
        address: "proxy.local".into(),
        port: 1080,
        ..Default::default()
    };
    let with_proxy = |key: &str, h: Host| {
        planned(
            key,
            Host {
                proxy: Some(proxy.clone()),
                ..h
            },
        )
    };
    let report = run(
        &m,
        plan_of(vec![
            with_proxy("a", host("merged", "10.0.0.1", 22, "root")),
            with_proxy("b", host("fresh", "10.0.0.2", 22, "ops")),
        ]),
        OnDuplicate::Merge,
    );
    assert_eq!(counts(&report), (1, 1, 0, 0));
    let merged = m.store.get::<Host>(&old).unwrap().doc;
    assert!(merged.terminal.tmux);
    assert_eq!(merged.proxy.unwrap().address, "proxy.local");
    let fresh = m.host_named("fresh").doc.proxy.unwrap();
    assert_eq!((fresh.kind, fresh.port), (ProxyKind::Socks5, 1080));
}

// 分组路径的查找：已有层级复用，缺失层级自动创建
#[test]
fn group_paths_reuse_existing_groups() {
    let m = Machine::new();
    let prod = m
        .store
        .create(
            Group {
                name: "生产".into(),
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    m.store
        .create(
            Group {
                name: "Web".into(),
                ..Default::default()
            },
            Some(prod.to_string()),
            Position::Last,
        )
        .unwrap();
    let mut b = PlanBuilder::new(Source::SshConfig);
    let group = b.group(&["生产", "Web", "api"]);
    let mut h = planned("h", host("h", "10.0.0.1", 22, "u"));
    h.group = group;
    b.push(h);
    run(&m, b.finish(), OnDuplicate::Skip);
    assert_eq!(
        m.store.list::<Group>().len(),
        3,
        "应复用已有的两级、新建一级"
    );
}

// ssh_config 的预览标出重复的主机；导入前保存快照；变更事件合并成一次
#[test]
fn ssh_config_preview_and_apply() {
    let m = Machine::new();
    let existing = m
        .store
        .create(host("web", "10.0.0.1", 22, "root"), None, Position::Last)
        .unwrap();
    let config = m.path("config");
    std::fs::write(
        &config,
        "Host dup\n  HostName 10.0.0.1\n  User root\nHost fresh\n  HostName 10.0.0.2\n  User ops\n",
    )
    .unwrap();
    let plan = block_on(crate::import::read(Source::SshConfig, config)).unwrap();
    let preview = preview(&m.store, &plan);
    assert_eq!(preview.items.len(), 2);
    assert_eq!(preview.items[0].duplicate_of.as_ref(), Some(&existing));
    assert_eq!(
        crate::import::select::default_selection(&preview.items),
        ["fresh".to_string()].into_iter().collect()
    );

    let mut events = m.store.subscribe();
    let keys = preview.items.iter().map(|i| i.key.clone()).collect();
    let report = block_on(apply(m.backups.clone(), plan, keys, OnDuplicate::Skip)).unwrap();
    assert_eq!(counts(&report), (1, 0, 1, 0));
    assert!(
        crate::usage::features::global().contains(crate::usage::Feature::ImportSshConfig),
        "使用统计的功能名单按来源记下导入"
    );
    let mut host_events = 0;
    while let Ok(event) = events.try_recv() {
        if event == StoreEvent::Changed(crate::model::Kind::Host) {
            host_events += 1;
        }
    }
    assert_eq!(host_events, 1, "整个导入的变更事件应合并成一次");
    let snapshots = block_on(m.backups.snapshots()).unwrap();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].reason, SnapshotReason::Import);
}

// 凭据档案：本机已有名称与用户名都相同的直接用（不改动，也不写秘密），否则新建，连同密码；用计划中私钥的档案
// 设为密钥库中的密钥，私钥没能存进去时改为密码；没有勾选的主机用到的档案不导入。主机自己的用户名优先，判断重复用档案的
#[test]
fn credential_profiles() {
    use crate::id::ProfileId;
    use crate::import::{PlannedProfile, PlannedProfileAuth};
    use crate::model::ProfileAuth;

    let m = Machine::new();
    let kept = m
        .store
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
    let key = crate::sshkey::generate(KeyKind::Ed25519, "").unwrap();
    let mut b = PlanBuilder::new(Source::NyaTermData);
    let good = b.add_key(PlannedKey::new(
        "deploy".into(),
        KeyAlgorithm::Ed25519,
        key.to_openssh().unwrap(),
    ));
    let bad = b.add_key(PlannedKey::new(
        "broken".into(),
        KeyAlgorithm::Rsa,
        SecretString::new("not a key"),
    ));
    let mut existing = PlannedProfile::new("运维", "ops", PlannedProfileAuth::Password);
    existing.set_password(SecretString::new("not written"));
    let existing = b.add_profile(existing);
    let mut fresh = PlannedProfile::new("数据库", "dba", PlannedProfileAuth::Password);
    fresh.set_password(SecretString::new("db-pw"));
    let fresh = b.add_profile(fresh);
    let keyed = b.add_profile(PlannedProfile::new(
        "",
        "deploy",
        PlannedProfileAuth::Key { key: good },
    ));
    let broken = b.add_profile(PlannedProfile::new(
        "坏钥匙",
        "x",
        PlannedProfileAuth::Key { key: bad },
    ));
    let unused = b.add_profile(PlannedProfile::new(
        "没用到",
        "u",
        PlannedProfileAuth::Agent,
    ));
    let with = |key: &str, h: Host, profile: usize, username: &str| {
        let mut p = planned(key, h);
        p.set_profile(profile, username);
        p
    };
    b.push(with("a", host("a", "10.0.7.1", 22, ""), existing, "ops"));
    b.push(with("b", host("b", "10.0.7.2", 22, ""), fresh, "dba"));
    b.push(with("c", host("c", "10.0.7.3", 22, "own"), keyed, "deploy"));
    b.push(with("d", host("d", "10.0.7.4", 22, ""), broken, "x"));
    b.push(with("e", host("e", "10.0.7.5", 22, ""), unused, "u"));
    let plan = b.finish();
    assert_eq!(
        plan.hosts[0].endpoint(),
        Endpoint::new("10.0.7.1", 22, "ops")
    );
    assert_eq!(
        plan.hosts[2].endpoint(),
        Endpoint::new("10.0.7.3", 22, "own")
    );
    let items = preview(&m.store, &plan).items;
    assert_eq!(items[0].profile.as_deref(), Some("运维"));
    assert_eq!(items[0].username, "ops");
    assert!(items[1].has_password);
    assert!(items[2].has_key);
    assert_eq!(
        items[2].profile.as_deref(),
        Some("deploy"),
        "名称为空时用用户名"
    );

    let keys = ["a", "b", "c", "d"].map(String::from).to_vec();
    let report = block_on(apply(m.backups.clone(), plan, keys, OnDuplicate::Skip)).unwrap();
    assert_eq!(report.profiles_created, 3);
    assert_eq!(report.failures.len(), 1, "坏的私钥");
    let profile_of = |name: &str| -> ProfileId {
        match m.host_named(name).doc.auth {
            Auth::Profile { profile } => profile,
            other => panic!("{name} 的认证是 {other:?}"),
        }
    };
    assert_eq!(profile_of("a"), kept, "名称、用户名都相同的已有档案直接用");
    assert_eq!(
        m.store
            .secret(&Owner::profile(&kept), SecretField::Password)
            .unwrap(),
        None,
        "已有的档案不改动"
    );
    let db = profile_of("b");
    assert_eq!(
        m.store
            .secret(&Owner::profile(&db), SecretField::Password)
            .unwrap()
            .unwrap()
            .expose(),
        "db-pw"
    );
    let c = m.host_named("c");
    assert_eq!(c.doc.username, "own", "主机自己的用户名优先");
    let deploy = m.store.get::<Profile>(&profile_of("c")).unwrap().doc;
    assert!(matches!(
        deploy.auth,
        ProfileAuth::Key {
            source: KeySource::Library { .. }
        }
    ));
    let broken = m.store.get::<Profile>(&profile_of("d")).unwrap().doc;
    assert_eq!(broken.auth, ProfileAuth::Password);
    assert!(
        m.store
            .list::<Profile>()
            .iter()
            .all(|p| p.doc.name != "没用到"),
        "没有勾选的主机用到的档案不导入"
    );
}

// 来源中的私钥存进密钥库（同一把只存一次），主机改用它；存不进去的记下，用它的主机改用密码认证
#[test]
fn private_keys_go_to_the_key_library() {
    let m = Machine::new();
    let key = crate::sshkey::generate(KeyKind::Ed25519, "").unwrap();
    let text = key.to_openssh().unwrap();
    let mut b = PlanBuilder::new(Source::Termora);
    let good = b.add_key(PlannedKey::new(
        "deploy-key".into(),
        KeyAlgorithm::Ed25519,
        text,
    ));
    let bad = b.add_key(PlannedKey::new(
        "broken".into(),
        KeyAlgorithm::Rsa,
        SecretString::new("not a key"),
    ));
    for (k, address, index) in [
        ("h1", "10.0.0.1", good),
        ("h2", "10.0.0.2", good),
        ("h3", "10.0.0.3", bad),
    ] {
        let i = b.push(planned(k, host(k, address, 22, "root")));
        b.host_mut(i).set_key(index);
    }
    let plan = b.finish();
    assert!(preview(&m.store, &plan).items[0].has_key);
    let report = run(&m, plan, OnDuplicate::Skip);
    assert_eq!(report.created, 3);
    assert_eq!(report.failures.len(), 1);
    assert!(matches!(
        &report.failures[0],
        ImportFailure {
            subject: FailureSubject::Key { name },
            reason: FailureReason::Key(_),
        } if name == "broken"
    ));
    let keys = m.store.list::<KeyEntry>();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].doc.fingerprint, key.fingerprint());
    let library = Auth::Key {
        source: KeySource::Library {
            key: keys[0].id.clone(),
        },
    };
    assert_eq!(m.host_named("h1").doc.auth, library);
    assert_eq!(m.host_named("h2").doc.auth, library);
    assert_eq!(m.host_named("h3").doc.auth, Auth::Password);
}

// 没有勾选、保险库锁定时不写任何东西；某台主机不合法时记下，其余照常导入
#[test]
fn errors_and_failures() {
    let m = Machine::new();
    let plan = plan_of(vec![planned("a", host("a", "10.0.0.1", 22, "u"))]);
    assert_eq!(
        block_on(apply(
            m.backups.clone(),
            plan.clone(),
            vec!["other".into()],
            OnDuplicate::Skip
        ))
        .unwrap_err(),
        BackupError::NothingSelected
    );

    let mut bad = plan_of(vec![
        planned("bad", host("bad", "a b", 22, "u")),
        planned("ok", host("ok", "10.0.0.2", 22, "u")),
    ]);
    let report = run(&m, bad.clone(), OnDuplicate::Skip);
    assert_eq!(counts(&report), (1, 0, 0, 1));
    assert_eq!(report.failed(), 1);
    assert!(matches!(
        &report.failures[0],
        ImportFailure {
            subject: FailureSubject::Host { key, .. },
            reason: FailureReason::Invalid(Invalid { field: "address", .. }),
        } if key == "bad"
    ));

    // 主密钥丢失（保险库不可用）时，带密码的导入整体拒绝；不带秘密的照常
    let m = m.reopen(SharedKeys::default());
    bad.hosts[1].set_password(SecretString::new("pw"));
    assert_eq!(
        block_on(apply(
            m.backups.clone(),
            bad,
            vec!["ok".into()],
            OnDuplicate::Skip
        ))
        .unwrap_err(),
        BackupError::VaultLocked
    );
    let report = run(
        &m,
        plan_of(vec![planned("c", host("c", "10.0.0.3", 22, "u"))]),
        OnDuplicate::Skip,
    );
    assert_eq!(report.created, 1);
}

// 私钥文件的密码、随主机导入的端口转发（不自动启动、已有的不重复、跳过的主机不导入）、快捷命令（不重复）、
// Telnet 与串口主机
#[test]
fn passphrase_forwards_snippets_and_other_protocols() {
    let m = Machine::new();
    let existing = m
        .store
        .create(host("已有", "10.0.3.9", 22, "u"), None, Position::Last)
        .unwrap();
    m.store
        .create(
            Snippet {
                name: "看日志".into(),
                content: "tail -f log".into(),
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    let rule = |bind_port, target_port| Forward {
        kind: ForwardKind::Local,
        name: format!("r{bind_port}"),
        bind_host: "localhost".into(),
        bind_port,
        target_host: "db.internal".into(),
        target_port,
        auto_start: true,
        ..Default::default()
    };
    let mut keyed = planned(
        "k",
        Host {
            auth: Auth::Key {
                source: KeySource::file(r"C:\keys\id"),
            },
            ..host("keyed", "10.0.3.1", 22, "deploy")
        },
    );
    keyed.set_passphrase(SecretString::new("kp"));
    keyed.set_password(SecretString::new("unused"));
    keyed.forwards = vec![rule(15432, 5432), rule(15432, 5433)];
    let mut skipped = planned("s", host("已有", "10.0.3.9", 22, "u"));
    skipped.forwards = vec![rule(16379, 6379)];
    let mut telnet = planned(
        "t",
        Host {
            protocol: Protocol::Telnet,
            ..host("交换机", "10.0.3.2", 0, "admin")
        },
    );
    telnet.set_password(SecretString::new("telnet-pw"));
    let serial = planned(
        "c",
        Host {
            protocol: Protocol::Serial,
            ..host("控制台", "COM3", 0, "")
        },
    );
    let mut b = PlanBuilder::new(Source::WindTerm);
    for h in [keyed, skipped, telnet, serial] {
        b.push(h);
    }
    for (name, content) in [
        ("看日志", "tail -f log"),
        ("重启", "systemctl restart app\n"),
        ("重启", "systemctl restart app\n"),
    ] {
        b.add_snippet(Snippet {
            name: name.into(),
            content: content.into(),
            ..Default::default()
        });
    }
    let plan = b.finish();
    let items = preview(&m.store, &plan).items;
    assert_eq!(items[0].forwards.len(), 2);
    assert_eq!(items[0].forwards[0].bind_host, "127.0.0.1");
    assert!(!items[0].forwards[0].exposed && items[0].forwards[0].auto_start);
    assert_eq!(items[1].duplicate_of.as_ref(), Some(&existing));

    let report = run(&m, plan, OnDuplicate::Skip);
    assert_eq!(counts(&report), (3, 0, 1, 1));
    assert_eq!(report.snippets, 1);
    assert_eq!(report.snippets_skipped, 2, "已有的与计划中重复的各一条");
    // 第二条规则的监听地址与第一条相同：没能导入
    assert!(matches!(
        &report.failures[0].subject,
        FailureSubject::Forward { host, .. } if host == "keyed"
    ));

    let keyed = m.host_named("keyed").id;
    let passphrase = m
        .store
        .secret(&Owner::host(&keyed), SecretField::Passphrase)
        .unwrap();
    assert_eq!(passphrase.unwrap().expose(), "kp");
    assert_eq!(m.password(&keyed), None, "用私钥时不保存登录密码");
    let forwards: Vec<Forward> = m
        .store
        .list::<Forward>()
        .into_iter()
        .map(|f| f.doc)
        .collect();
    assert_eq!(forwards.len(), 1, "跳过的主机的规则不导入");
    assert_eq!(
        (
            forwards[0].host.as_ref(),
            forwards[0].bind_port,
            forwards[0].auto_start
        ),
        (Some(&keyed), 15432, false)
    );
    let telnet = m.host_named("交换机");
    assert_eq!(telnet.doc.protocol, Protocol::Telnet);
    assert_eq!(m.password(&telnet.id).as_deref(), Some("telnet-pw"));
    assert_eq!(m.host_named("控制台").doc.protocol, Protocol::Serial);
    let snippets: Vec<String> = m
        .store
        .list::<Snippet>()
        .into_iter()
        .map(|s| s.doc.name)
        .collect();
    assert_eq!(snippets, ["看日志", "重启"]);

    // 再导入一次：规则已有不重复添加（合并时）
    let mut again = planned(
        "k",
        Host {
            auth: Auth::Key {
                source: KeySource::file(r"C:\keys\id"),
            },
            ..host("keyed", "10.0.3.1", 22, "deploy")
        },
    );
    again.forwards = vec![rule(15432, 5432)];
    let report = run(&m, plan_of(vec![again]), OnDuplicate::Merge);
    assert_eq!(counts(&report), (0, 1, 0, 0));
    assert_eq!(m.store.list::<Forward>().len(), 1);
}

// 只有快捷命令的计划（NyaTerm）：不用勾选主机；再导入一次时全部相同，不重复添加；什么也没有时不能导入
#[test]
fn snippets_only() {
    let m = Machine::new();
    let mut b = PlanBuilder::new(Source::NyaTerm);
    for (group, name) in [("运维", "磁盘"), ("", "进程")] {
        b.add_snippet(Snippet {
            name: name.into(),
            content: format!("{name}\n"),
            group: group.into(),
            ..Default::default()
        });
    }
    let plan = b.finish();
    let report = block_on(apply(
        m.backups.clone(),
        plan.clone(),
        Vec::new(),
        OnDuplicate::Skip,
    ))
    .unwrap();
    assert_eq!((report.snippets, report.snippets_skipped), (2, 0));
    assert_eq!(counts(&report), (0, 0, 0, 0));
    let groups: Vec<String> = m
        .store
        .list::<Snippet>()
        .into_iter()
        .map(|s| s.doc.group)
        .collect();
    assert_eq!(groups, ["运维", ""]);
    let again = block_on(apply(
        m.backups.clone(),
        plan,
        Vec::new(),
        OnDuplicate::Skip,
    ))
    .unwrap();
    assert_eq!((again.snippets, again.snippets_skipped), (0, 2));
    assert!(matches!(
        block_on(apply(
            m.backups.clone(),
            PlanBuilder::new(Source::NyaTerm).finish(),
            Vec::new(),
            OnDuplicate::Skip,
        )),
        Err(BackupError::NothingSelected)
    ));
}

/// 存储中全部快捷命令的（分组，名称，内容），按快捷命令页的顺序。
fn snippet_rows(m: &Machine) -> Vec<(String, String, String)> {
    let mut rows: Vec<(String, String, String)> = m
        .store
        .list::<Snippet>()
        .into_iter()
        .map(|s| (s.doc.group, s.doc.name, s.doc.content))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

// Termlet 导出的快捷命令：导到另一台上原样还原（分组层级、中文、多行、参数），预览中没有重复；
// 导回原来的那台时全部相同、默认跳过（末尾的换行写法不同也算相同）
#[test]
fn termlet_snippet_file_round_trips() {
    let a = Machine::new();
    for (group, name, content) in [
        ("", "磁盘", "df -h\n"),
        ("运维", "进程", "ps aux | grep {{名称}}\n"),
        (
            "运维 / Kubernetes",
            "日志",
            "kubectl logs -f {{pod}} -c {{容器:app}}",
        ),
        (
            "运维 / Kubernetes",
            "容器名",
            "docker ps --format '{{.Names}}'\n",
        ),
        ("部署", "多行", "cd /srv/应用\r\ngit pull\r\n"),
    ] {
        a.store
            .create(
                Snippet {
                    name: name.into(),
                    group: group.into(),
                    content: content.into(),
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
    }
    let file = a.dir.path().join(crate::snippetfile::default_file_name());
    let groups = crate::snippetfile::groups(&a.store.list::<Snippet>())
        .into_iter()
        .map(|(group, _)| group)
        .collect();
    let exported = block_on(crate::snippetfile::export(
        a.store.clone(),
        file.clone(),
        groups,
    ))
    .unwrap();
    assert_eq!(exported, 5);

    let b = Machine::new();
    let plan = crate::import::parse(Source::TermletSnippets, &file).unwrap();
    assert_eq!(plan.source, Source::TermletSnippets);
    assert_eq!(preview(&b.store, &plan).snippet_duplicates, [false; 5]);
    let report = block_on(apply(
        b.backups.clone(),
        plan.clone(),
        Vec::new(),
        OnDuplicate::Skip,
    ))
    .unwrap();
    assert_eq!((report.snippets, report.snippets_skipped), (5, 0));
    let mut want = snippet_rows(&a);
    // 内容末尾的 \r\n 导出为“执行”，导入后写成 \n；中间的照旧
    want[4].2 = "cd /srv/应用\r\ngit pull\n".into();
    assert_eq!(snippet_rows(&b), want);

    // 导回原来的那台：全部已有
    assert_eq!(preview(&a.store, &plan).snippet_duplicates, [true; 5]);
    let again = block_on(apply(
        a.backups.clone(),
        plan,
        Vec::new(),
        OnDuplicate::Skip,
    ))
    .unwrap();
    assert_eq!((again.snippets, again.snippets_skipped), (0, 5));
    assert_eq!(a.store.list::<Snippet>().len(), 5);
}

// 重复：名称、内容都相同（名称的首尾空白、内容末尾的换行写法不算）；文件中重复的也只导入第一条；
// 名称相同、内容不同或执行与否不同的照常导入
#[test]
fn snippet_duplicates_in_preview_and_apply() {
    let m = Machine::new();
    m.store
        .create(
            Snippet {
                name: "磁盘".into(),
                content: "df -h\n".into(),
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    let mut b = PlanBuilder::new(Source::TermletSnippets);
    for (name, content) in [
        (" 磁盘 ", "df -h\r\n"),
        ("磁盘", "df -h"),
        ("磁盘", "df -hT\n"),
        ("新的", "uptime\n"),
        ("新的", "uptime\n"),
    ] {
        b.add_snippet(Snippet {
            name: name.into(),
            content: content.into(),
            ..Default::default()
        });
    }
    let plan = b.finish();
    assert_eq!(
        preview(&m.store, &plan).snippet_duplicates,
        [true, false, false, false, true]
    );
    let report = block_on(apply(
        m.backups.clone(),
        plan,
        Vec::new(),
        OnDuplicate::Skip,
    ))
    .unwrap();
    assert_eq!((report.snippets, report.snippets_skipped), (3, 2));
}
