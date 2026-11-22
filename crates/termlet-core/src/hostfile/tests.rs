//! 主机文件的测试。

use super::export::alias;
use super::*;
use crate::backup::testkit::Machine;
use crate::id::{GroupId, HostId};
use crate::import::{HostOutcome, ImportReport, OnDuplicate};
use crate::keyvault::{GenerateRequest, ImportRequest, ImportSource, KeyKind};
use crate::model::{
    ForwardKind, Group, KeyEntry, Protocol, Proxy, ProxyKind, SshOptions, TerminalOptions,
};
use crate::store::{Owner, Position, Record};
use crate::testkit::block_on;

fn pw(s: &str) -> SecretString {
    SecretString::new(s)
}

fn secret(store: &Store, owner: &Owner, field: SecretField) -> Option<String> {
    store
        .secret(owner, field)
        .unwrap()
        .map(|s| s.expose().to_string())
}

/// 一组互相引用的数据：分组 生产/web（生产有默认的凭据档案、跳板机、终端配色）、跳板机、用密钥库密钥的主机、
/// 带 TOTP 的主机、带代理命令与端口转发的主机、Telnet 主机。
struct Sample {
    bastion: HostId,
    web: HostId,
    db: HostId,
    sw: HostId,
    key: crate::id::KeyId,
    fingerprint: String,
}

fn sample(m: &Machine) -> Sample {
    let store = &m.store;
    let key = block_on(crate::keyvault::generate(
        store.clone(),
        GenerateRequest {
            name: "部署密钥".into(),
            kind: KeyKind::Ed25519,
            comment: String::new(),
        },
    ))
    .unwrap();
    let fingerprint = store.get::<KeyEntry>(&key).unwrap().doc.fingerprint;
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
    store
        .set_secret(
            &Owner::profile(&profile),
            SecretField::Password,
            Some(&pw("ops-pass")),
        )
        .unwrap();
    let group = |name: &str, parent: Option<&GroupId>| {
        store
            .create(
                Group {
                    name: name.into(),
                    ..Default::default()
                },
                parent.map(|p| p.to_string()),
                Position::Last,
            )
            .unwrap()
    };
    let prod = group("生产", None);
    let web_group = group("web", Some(&prod));
    let bastion = store
        .create(
            Host {
                name: "跳板机".into(),
                address: "bastion.example".into(),
                username: "jump".into(),
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    m.set_password(&bastion, "jump-pass");
    let mut prod_doc = store.get::<Group>(&prod).unwrap().doc;
    prod_doc.defaults.profile = Some(profile);
    prod_doc.defaults.jump = Some(bastion.clone());
    prod_doc.defaults.color_scheme = "nord".into();
    store.update(&prod, prod_doc).unwrap();
    // 用户名、认证、跳板机都沿用分组的默认值
    let web = store
        .create(
            Host {
                name: "web 01".into(),
                address: "10.0.0.1".into(),
                tags: vec!["prod".into()],
                note: "主站".into(),
                auth: Auth::Inherit,
                ..Default::default()
            },
            Some(web_group.to_string()),
            Position::Last,
        )
        .unwrap();
    store
        .set_secret(
            &Owner::host(&web),
            SecretField::Totp,
            Some(&pw("JBSWY3DPEHPK3PXP")),
        )
        .unwrap();
    let db = store
        .create(
            Host {
                name: "db".into(),
                address: "10.0.0.2".into(),
                port: 2222,
                username: "postgres".into(),
                auth: Auth::Key {
                    source: KeySource::Library { key: key.clone() },
                },
                proxy: Some(Proxy {
                    kind: ProxyKind::Command,
                    command: "nc -X 5 -x 127.0.0.1:1080 %h %p".into(),
                    ..Default::default()
                }),
                terminal: TerminalOptions {
                    tmux: true,
                    startup: "cd /srv\nls".into(),
                    color_scheme: "dracula".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
            Some(prod.to_string()),
            Position::Last,
        )
        .unwrap();
    for forward in [
        Forward {
            host: Some(db.clone()),
            name: "pg".into(),
            bind_port: 15432,
            target_host: "127.0.0.1".into(),
            target_port: 5432,
            ..Default::default()
        },
        Forward {
            host: Some(db.clone()),
            kind: ForwardKind::Dynamic,
            bind_port: 11080,
            ..Default::default()
        },
    ] {
        store.create(forward, None, Position::Last).unwrap();
    }
    let sw = store
        .create(
            Host {
                name: "交换机".into(),
                protocol: Protocol::Telnet,
                address: "192.168.1.1".into(),
                username: "admin".into(),
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    m.set_password(&sw, "sw-pass");
    Sample {
        bastion,
        web,
        db,
        sw,
        key,
        fingerprint,
    }
}

fn request(hosts: &[&HostId], password: Option<&str>) -> ExportRequest {
    ExportRequest {
        hosts: hosts.iter().map(|h| (*h).clone()).collect(),
        format: ExportFormat::Termlet,
        forwards: true,
        password: password.map(pw),
    }
}

fn export_to(
    m: &Machine,
    name: &str,
    request: ExportRequest,
) -> Result<(PathBuf, ExportSummary), BackupError> {
    let path = m.path(name);
    let summary = block_on(export(m.store.clone(), path.clone(), request))?;
    Ok((path, summary))
}

fn read_file(path: &std::path::Path, password: Option<&str>) -> Result<HostFile, BackupError> {
    block_on(read(path.to_path_buf(), password.map(pw)))
}

fn import_all(m: &Machine, file: &HostFile, on_duplicate: OnDuplicate) -> ImportReport {
    let keys = file.doc.hosts.iter().map(|h| h.id.clone()).collect();
    let report = block_on(import(m.backups.clone(), file.clone(), keys, on_duplicate)).unwrap();
    // 使用统计的功能名单记下导入过主机文件
    assert!(crate::usage::features::global().contains(crate::usage::Feature::ImportTermletFile));
    report
}

fn contains(data: &[u8], text: &str) -> bool {
    data.windows(text.len()).any(|w| w == text.as_bytes())
}

#[test]
fn export_without_secrets() {
    let a = Machine::new();
    let s = sample(&a);
    let req = request(&[&s.web, &s.db, &s.sw], None);
    let expected = summarize(&a.store, &req).unwrap();
    let (path, summary) = export_to(&a, "hosts.termlet-hosts", req.clone()).unwrap();
    // 跳板机（分组默认的）没有选中，一并导出
    assert_eq!(summary, expected);
    assert_eq!(
        (
            summary.hosts,
            summary.added,
            summary.groups,
            summary.profiles,
            summary.keys,
            summary.forwards
        ),
        (4, 1, 2, 1, 1, 2)
    );
    let data = std::fs::read(&path).unwrap();
    assert!(!envelope::is_sealed(&data), "不含秘密时应是明文 JSON");
    for secret in [
        "ops-pass",
        "jump-pass",
        "sw-pass",
        "JBSWY3DPEHPK3PXP",
        "PRIVATE KEY",
    ] {
        assert!(
            !contains(&data, secret),
            "不含秘密的导出文件中出现了 {secret}"
        );
    }
    let file = read_file(&path, None).unwrap();
    let doc = &file.doc;
    assert!(!doc.secrets);
    assert_eq!(
        (doc.hosts.len(), doc.keys.len(), doc.profiles.len()),
        (4, 1, 1)
    );
    assert!(doc.keys[0].private_key.is_none());
    let web = doc.host(s.web.as_str()).unwrap();
    assert_eq!(doc.group_path(web.group.as_deref()), ["生产", "web"]);
    assert_eq!(
        (web.host.username.as_str(), doc.effective_user(web).as_str()),
        ("", "ops")
    );
    assert!(web.secrets.is_empty());
    let prod = doc.groups.iter().find(|g| g.name == "生产").unwrap();
    assert_eq!(prod.defaults.jump.as_ref(), Some(&s.bastion));
    assert!(prod.defaults.profile.is_some());
    assert_eq!(prod.defaults.color_scheme, "nord");
    let db = doc.host(s.db.as_str()).unwrap();
    assert_eq!(
        db.host.auth,
        Auth::Key {
            source: KeySource::Library { key: s.key.clone() }
        }
    );
    assert_eq!(db.forwards.len(), 2);
    assert!(db.forwards.iter().all(|f| f.host.is_none()));
    assert_eq!(db.host.terminal.startup, "cd /srv\nls");
    assert_eq!(db.host.terminal.color_scheme, "dracula");

    // 不带端口转发
    let without = ExportRequest {
        forwards: false,
        ..req
    };
    assert_eq!(summarize(&a.store, &without).unwrap().forwards, 0);
    let (path, _) = export_to(&a, "no-forwards.termlet-hosts", without).unwrap();
    assert!(
        read_file(&path, None)
            .unwrap()
            .doc
            .hosts
            .iter()
            .all(|h| h.forwards.is_empty())
    );
}

#[test]
fn export_and_import_with_secrets() {
    let a = Machine::new();
    let b = Machine::new();
    let s = sample(&a);
    assert_eq!(
        export_to(&a, "x.termlet-hosts", request(&[&s.web], Some("short"))).unwrap_err(),
        BackupError::PasswordTooShort { min: 8 }
    );
    let (path, _) = export_to(
        &a,
        "hosts.termlet-hosts",
        request(&[&s.web, &s.db, &s.sw], Some("export-pass")),
    )
    .unwrap();
    let data = std::fs::read(&path).unwrap();
    assert!(
        envelope::is_sealed(&data) && !contains(&data, "ops-pass") && !contains(&data, "10.0.0.1")
    );
    assert_eq!(
        read_file(&path, None).unwrap_err(),
        BackupError::PasswordRequired
    );
    assert_eq!(
        read_file(&path, Some("wrong-pass")).unwrap_err(),
        BackupError::WrongPassword
    );
    let file = read_file(&path, Some("export-pass")).unwrap();
    assert!(file.doc.secrets);
    assert!(
        !format!("{file:?}").contains("ops-pass"),
        "Debug 中不应出现秘密"
    );

    let items = preview(&b.store, &file).items;
    assert_eq!(items.len(), 4);
    assert!(items.iter().all(|i| i.duplicate_of.is_none()));
    let item = |name: &str| items.iter().find(|i| i.name == name).unwrap();
    let db = item("db");
    assert!(db.proxy_command.as_deref().unwrap().contains("nc -X 5"));
    assert_eq!(db.startup.as_deref(), Some("cd /srv; ls"));
    assert!(db.has_key && db.forwards.len() == 2);
    let web = item("web 01");
    assert_eq!(
        (web.username.as_str(), web.jump.as_deref()),
        ("ops", Some("跳板机"))
    );
    assert!(web.has_password);
    assert_eq!(item("交换机").protocol, Protocol::Telnet);

    let report = import_all(&b, &file, OnDuplicate::Skip);
    assert_eq!(
        (report.created, report.skipped, report.failures.len()),
        (4, 0, 0),
        "{report:?}"
    );

    // web：用户名、凭据档案、跳板机都沿用分组的默认值，秘密可用
    let web = b.host_named("web 01");
    assert_eq!(web.doc.auth, Auth::Inherit);
    assert!(!web.doc.cli.allowed, "导入的主机不应允许命令行访问");
    assert_eq!(
        crate::import::effective_username(&b.store, &web.doc, web.parent.as_deref()),
        "ops"
    );
    assert_eq!(
        secret(&b.store, &Owner::host(&web.id), SecretField::Totp).as_deref(),
        Some("JBSWY3DPEHPK3PXP")
    );
    let chain = b
        .store
        .group_chain(web.parent.clone().map(GroupId::from_raw).as_ref());
    let prod = chain.iter().find(|g| g.doc.name == "生产").unwrap();
    let profile = prod.doc.defaults.profile.clone().unwrap();
    assert_eq!(
        secret(&b.store, &Owner::profile(&profile), SecretField::Password).as_deref(),
        Some("ops-pass")
    );
    assert_eq!(
        prod.doc.defaults.color_scheme, "nord",
        "分组的终端配色应随主机导入"
    );
    let bastion = b.host_named("跳板机");
    assert_eq!(prod.doc.defaults.jump.as_ref(), Some(&bastion.id));
    assert_eq!(b.password(&bastion.id).as_deref(), Some("jump-pass"));
    // db：密钥存入了密钥库，代理命令、tmux、登录后执行的命令、端口转发都在
    let db = b.host_named("db");
    let keys = b.store.list::<KeyEntry>();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].doc.fingerprint, s.fingerprint);
    assert_eq!(
        db.doc.auth,
        Auth::Key {
            source: KeySource::Library {
                key: keys[0].id.clone()
            }
        }
    );
    let private = secret(&b.store, &Owner::key(&keys[0].id), SecretField::PrivateKey).unwrap();
    assert!(private.contains("OPENSSH PRIVATE KEY"));
    assert_eq!(db.doc.proxy.as_ref().unwrap().kind, ProxyKind::Command);
    assert!(
        db.doc.terminal.tmux && db.doc.terminal.startup == "cd /srv\nls" && db.doc.port == 2222
    );
    assert_eq!(db.doc.terminal.color_scheme, "dracula");
    let rules: Vec<Record<Forward>> = b.store.list::<Forward>();
    assert_eq!(rules.len(), 2);
    assert!(
        rules
            .iter()
            .all(|r| r.doc.host.as_ref() == Some(&db.id) && !r.doc.auto_start)
    );
    assert_eq!(b.host_named("交换机").doc.protocol, Protocol::Telnet);

    // 再导入一次：跳过时全部跳过；合并时保留已有的秘密，不重复添加转发规则
    let report = import_all(&b, &file, OnDuplicate::Skip);
    assert_eq!((report.created, report.skipped), (0, 4));
    let sw = b.host_named("交换机").id;
    b.set_password(&sw, "changed");
    let report = import_all(&b, &file, OnDuplicate::Merge);
    assert_eq!((report.merged, report.failures.len()), (4, 0), "{report:?}");
    assert_eq!(
        b.store.list::<Forward>().len(),
        2,
        "合并时不应重复添加转发规则"
    );
    assert_eq!(
        b.password(&sw).as_deref(),
        Some("changed"),
        "合并时应保留已有的密码"
    );
    assert_eq!(
        b.store.list::<Profile>().len(),
        1,
        "同名的凭据档案不应重复建立"
    );
}

// 本机密钥库中已有同一把密钥（指纹相同）时导入的主机使用它；没有密码的主机连接时询问
#[test]
fn import_without_secrets_uses_local_key() {
    let a = Machine::new();
    let b = Machine::new();
    let s = sample(&a);
    let (path, _) = export_to(
        &a,
        "hosts.termlet-hosts",
        request(&[&s.db, &s.bastion], None),
    )
    .unwrap();
    let private = a
        .store
        .secret(&Owner::key(&s.key), SecretField::PrivateKey)
        .unwrap()
        .unwrap();
    let local = block_on(crate::keyvault::import(
        b.store.clone(),
        ImportRequest {
            name: String::new(),
            source: ImportSource::Text(private),
            passphrase: None,
        },
    ))
    .unwrap();
    let file = read_file(&path, None).unwrap();
    let report = import_all(&b, &file, OnDuplicate::Skip);
    assert_eq!(report.created, 2, "{report:?}");
    assert_eq!(
        b.host_named("db").doc.auth,
        Auth::Key {
            source: KeySource::Library {
                key: local.id().clone()
            }
        }
    );
    assert_eq!(b.password(&b.host_named("跳板机").id), None);
}

#[test]
fn export_ssh_config() {
    let a = Machine::new();
    let s = sample(&a);
    let file_host = a
        .store
        .create(
            Host {
                name: "key file".into(),
                address: "10.0.0.9".into(),
                username: "me".into(),
                auth: Auth::Key {
                    source: KeySource::file(r"C:\Users\a b\.ssh\id_ed25519"),
                },
                ssh: SshOptions {
                    agent_forward: true,
                    ..Default::default()
                },
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    let req = ExportRequest {
        format: ExportFormat::SshConfig,
        ..request(&[&s.web, &s.db, &s.sw, &file_host], None)
    };
    let (path, summary) = export_to(&a, "config", req).unwrap();
    assert_eq!(
        (
            summary.hosts,
            summary.skipped,
            summary.added,
            summary.forwards
        ),
        (4, 1, 1, 2)
    );
    let text = std::fs::read_to_string(&path).unwrap();
    for want in [
        "Host 跳板机\n    HostName bastion.example\n    User jump\n",
        "# 分组：生产/web\nHost web-01\n    HostName 10.0.0.1\n    User ops\n    ProxyJump 跳板机\n",
        "Host db\n    HostName 10.0.0.2\n    Port 2222\n    User postgres\n    # 使用 Termlet 密钥库中的密钥：部署密钥",
        "    ProxyJump 跳板机\n    LocalForward 127.0.0.1:15432 127.0.0.1:5432\n    DynamicForward 127.0.0.1:11080\n",
        "Host key-file\n    HostName 10.0.0.9\n    User me\n    IdentityFile \"C:\\Users\\a b\\.ssh\\id_ed25519\"\n    ForwardAgent yes\n",
    ] {
        assert!(
            text.contains(want),
            "ssh_config 中缺少：\n{want}\n实际：\n{text}"
        );
    }
    // 经跳板机连接时主机自己的代理不生效，不写 ProxyCommand
    assert!(
        !text.contains("交换机") && !text.contains("pass") && !text.contains("ProxyCommand"),
        "ssh_config 中不应有 Telnet 主机、密码与经跳板机的主机的代理命令：\n{text}"
    );
    // 直连的主机写出代理命令
    let direct = a
        .store
        .create(
            Host {
                name: "via-proxy".into(),
                address: "10.0.0.8".into(),
                username: "u".into(),
                jump: Jump::Direct,
                proxy: Some(Proxy {
                    kind: ProxyKind::Command,
                    command: "nc %h %p".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    let req = ExportRequest {
        format: ExportFormat::SshConfig,
        ..request(&[&direct], None)
    };
    let (path, _) = export_to(&a, "config-2", req).unwrap();
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("    ProxyCommand nc %h %p\n")
    );
    // 沿用分组的代理命令也写出；X11 转发写明是否受信任
    let group = a
        .store
        .create(
            Group {
                name: "ssm".into(),
                defaults: crate::model::GroupDefaults {
                    proxy_command: "aws ssm start-session --target %n".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    let mut x11 = Host {
        name: "i-0abc".into(),
        address: "i-0abc".into(),
        username: "ec2-user".into(),
        jump: Jump::Direct,
        ..Default::default()
    };
    x11.ssh.x11_forward = true;
    let inherits = a
        .store
        .create(x11, Some(group.as_str().to_string()), Position::Last)
        .unwrap();
    let req = ExportRequest {
        format: ExportFormat::SshConfig,
        ..request(&[&inherits], None)
    };
    let (path, _) = export_to(&a, "config-3", req).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("    ProxyCommand aws ssm start-session --target %n\n")
            && text.contains("    ForwardX11 yes\n")
            && text.contains("    ForwardX11Trusted no\n"),
        "{text}"
    );
    assert_eq!(
        summarize(&a.store, &request(&[&HostId::from_raw("h_missing")], None)).unwrap_err(),
        BackupError::HostNotFound
    );
    assert_eq!(
        summarize(&a.store, &request(&[], None)).unwrap_err(),
        BackupError::NothingSelected
    );
}

#[test]
fn parse_rejects_other_files() {
    let sealed = envelope::seal(SealedKind::Backup, &pw("password-1"), b"x").unwrap();
    assert_eq!(
        parse(&sealed, Some(&pw("password-1"))).unwrap_err(),
        BackupError::IsFullBackup
    );
    for data in [
        "hello",
        r#"{"format":"other"}"#,
        r#"{"format":"termlet-hosts","version":0}"#,
    ] {
        assert_eq!(
            parse(data.as_bytes(), None).unwrap_err(),
            BackupError::Invalid,
            "{data}"
        );
    }
    assert_eq!(
        parse(
            r#"{"format":"termlet-hosts","version":2,"hosts":"新格式"}"#.as_bytes(),
            None
        )
        .unwrap_err(),
        BackupError::TooNew {
            found: 2,
            supported: 1
        }
    );
}

#[test]
fn ssh_config_aliases() {
    for (name, want) in [
        ("web 01", "web-01"),
        (" *db?  ", "db"),
        ("", "10.0.0.1"),
        ("a#b", "a-b"),
    ] {
        assert_eq!(alias(name, "10.0.0.1"), want, "{name}");
    }
}

// 随主机导入的端口转发在预览中列出（监听回环以外的地址、文件中设为
// 自动启动的标出）；导入后（包括合并到本机已有的主机上）一律不自动启动
#[test]
fn forwards_preview_and_no_auto_start() {
    let b = Machine::new();
    let existing = b
        .store
        .create(
            Host {
                name: "web".into(),
                address: "10.0.0.1".into(),
                port: 22,
                username: "root".into(),
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    let json = r#"{"format":"termlet-hosts","version":1,"hosts":[{"ref":"h1",
        "host":{"name":"web","address":"10.0.0.1","port":22,"username":"root"},
        "forwards":[
          {"kind":"dynamic","bindHost":"0.0.0.0","bindPort":11080,"autoStart":true},
          {"kind":"local","name":"pg","bindHost":"localhost","bindPort":15432,"targetHost":"127.0.0.1","targetPort":5432,"autoStart":true},
          {"kind":"remote","bindHost":"*","bindPort":9000,"targetHost":"127.0.0.1","targetPort":3000}
        ]}]}"#;
    let file = parse(json.as_bytes(), None).unwrap();
    let items = preview(&b.store, &file).items;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].duplicate_of.as_ref(), Some(&existing));
    let fw = &items[0].forwards;
    assert_eq!(fw.len(), 3);
    assert!(
        fw[0].kind == ForwardKind::Dynamic && fw[0].exposed && fw[0].auto_start,
        "{:?}",
        fw[0]
    );
    assert!(
        !fw[1].exposed && fw[1].bind_host == "127.0.0.1" && fw[1].auto_start && fw[1].name == "pg"
    );
    assert!(fw[2].exposed && fw[2].bind_host == "0.0.0.0" && !fw[2].auto_start);

    let report = import_all(&b, &file, OnDuplicate::Merge);
    assert_eq!((report.merged, report.failures.len()), (1, 0), "{report:?}");
    assert_eq!(
        report.hosts[0].outcome,
        HostOutcome::Merged(existing.clone())
    );
    let rules = b.store.list::<Forward>();
    assert_eq!(rules.len(), 3);
    assert!(
        rules
            .iter()
            .all(|r| r.doc.host.as_ref() == Some(&existing) && !r.doc.auto_start)
    );
    // 监听地址被本机其他规则占用时跳过并记下
    let json = json.replace("10.0.0.1", "10.0.0.3");
    let file = parse(json.as_bytes(), None).unwrap();
    let report = import_all(&b, &file, OnDuplicate::Skip);
    assert_eq!(report.created, 1);
    assert_eq!(
        report.failures.len(),
        2,
        "本地、动态转发的端口已被占用：{report:?}"
    );
}

// 别人给的文件中开启的 agent 转发：预览中提示，导入后关闭
#[test]
fn agent_forwarding_is_turned_off_on_import() {
    let b = Machine::new();
    let json = r#"{"format":"termlet-hosts","version":1,"hosts":[{"ref":"h1",
        "host":{"name":"web","address":"10.0.0.8","port":22,"username":"root","ssh":{"agentForward":true}}}]}"#;
    let file = parse(json.as_bytes(), None).unwrap();
    assert_eq!(
        preview(&b.store, &file).items[0].hints,
        [crate::import::Hint::AgentForwardDisabled]
    );
    let report = import_all(&b, &file, OnDuplicate::Skip);
    assert_eq!(report.created, 1, "{report:?}");
    let host = b.store.list::<Host>().remove(0);
    assert!(!host.doc.ssh.agent_forward);
}
