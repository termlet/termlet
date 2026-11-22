//! 样例在测试中现场生成：表、键前缀、字段按 NyaTerm 提交 290212c 的 `crates/nyaterm-store/src/storage/`
//! 与 `crates/nyaterm-core/src/models/` 手写，秘密、主密钥、`.nya` 的外层按 [`super::crypto`] 中说明的方式加密。

use std::path::Path;

use redb::{Database, TableDefinition};
use serde_json::{Value, json};

use super::crypto::seal;
use super::memory::Memory;
use super::*;
use crate::import::files::tests::env;
use crate::model::Encoding;

const MASTER: [u8; 32] = [0x11; 32];

/// 本机没设主密码时的材料：测试环境的主目录路径。
fn home_material() -> Vec<u8> {
    env().home.unwrap().to_string_lossy().as_bytes().to_vec()
}

fn secret(plain: &str) -> String {
    seal::secret(&MASTER, plain.as_bytes())
}

fn ed25519() -> String {
    crate::sshkey::generate(crate::sshkey::KeyKind::Ed25519, "nya")
        .unwrap()
        .to_openssh()
        .unwrap()
        .expose()
        .to_string()
}

/// 写一个内存中的 redb 数据库：`blobs` 是值为 JSON 字节的表，`texts` 是值为文本的表。
fn database(
    blobs: &[(&'static str, Vec<(String, Value)>)],
    texts: &[(&'static str, Vec<(String, String)>)],
) -> Vec<u8> {
    let memory = Memory::default();
    {
        let db = Database::builder()
            .create_with_backend(memory.clone())
            .unwrap();
        let txn = db.begin_write().unwrap();
        for (name, rows) in blobs {
            let mut table = txn
                .open_table(TableDefinition::<&str, &[u8]>::new(name))
                .unwrap();
            for (key, value) in rows {
                let bytes = serde_json::to_vec(value).unwrap();
                table.insert(key.as_str(), bytes.as_slice()).unwrap();
            }
        }
        for (name, rows) in texts {
            let mut table = txn
                .open_table(TableDefinition::<&str, &str>::new(name))
                .unwrap();
            for (key, value) in rows {
                table.insert(key.as_str(), value.as_str()).unwrap();
            }
        }
        txn.commit().unwrap();
    }
    memory.bytes()
}

fn rows(prefix: &str, items: &[Value]) -> Vec<(String, Value)> {
    items
        .iter()
        .map(|v| (format!("{prefix}{}", v["id"].as_str().unwrap()), v.clone()))
        .collect()
}

/// 样例中的实体（本机数据库与备份共用；`backup` 时连接与代理的密码是明文，与 NyaTerm 导出时相同）。
struct Sample {
    groups: Vec<Value>,
    connections: Vec<Value>,
    keys: Vec<Value>,
    accounts: Vec<Value>,
    proxies: Vec<Value>,
    tunnels: Vec<Value>,
    otp: Vec<Value>,
    quick_commands: Value,
}

fn sample(backup: bool) -> Sample {
    let inline = |plain: &str| {
        if backup {
            json!({"mode": "password", "password": plain, "has_password": false})
        } else {
            json!({"mode": "password", "password": secret(plain), "has_password": true})
        }
    };
    let proxy_password = |plain: &str| {
        if backup {
            plain.to_string()
        } else {
            format!("{PROXY_SECRET_PREFIX}{}", secret(plain))
        }
    };
    Sample {
        groups: vec![
            json!({"id": "g1", "name": "生产", "sort_order": 0}),
            json!({"id": "g2", "name": "数据库", "parent_id": "g1", "sort_order": 1}),
            json!({"id": "g3", "name": "环 A", "parent_id": "g4", "sort_order": 2}),
            json!({"id": "g4", "name": "环 B", "parent_id": "g3", "sort_order": 3}),
        ],
        connections: vec![
            json!({"id": "c1", "name": "web-1", "type": "ssh", "host": "192.0.2.11", "port": 22,
                   "username": "root", "group_id": "g1", "auth": inline("pw-1"), "description": "前端",
                   "tags": ["prod", "web"], "encoding": "GBK", "x11_forwarding": true,
                   "post_login": {"enabled": true, "command": "uptime", "delay_ms": 500},
                   "recording": {"auto_start": true}, "network": {"proxy_id": "p1"}, "sort_order": 0,
                   "backspace_mode": "del", "某个新字段": 1}),
            json!({"id": "c2", "name": "db-1", "type": "ssh", "host": "192.0.2.21", "port": 2222,
                   "username": "dba", "group_id": "g2",
                   "auth": {"mode": "key", "key_id": "k1", "otp_id": "o1"},
                   "network": {"proxy_jump_id": "c1"}, "agent_forwarding_config": {"enabled": true}}),
            json!({"id": "c3", "name": "via-account", "type": "ssh", "host": "192.0.2.31", "port": 22,
                   "username": "ignored",
                   "auth": {"mode": "password", "account_id": "a1", "password_source": "account"}}),
            json!({"id": "c4", "name": "switch", "type": "telnet", "host": "192.0.2.41", "port": 23,
                   "username": "admin", "auth": inline("telnet-pw"), "encoding": "Shift_JIS"}),
            json!({"id": "c5", "name": "console", "type": "serial", "port_name": "COM3",
                   "baud_rate": 9600, "data_bits": 7, "parity": "even", "stop_bits": "2",
                   "flow_control": "hardware"}),
            json!({"id": "c6", "name": "desktop", "type": "rdp", "host": "192.0.2.61", "port": 3389}),
            json!({"id": "c8", "name": "bad-key", "type": "ssh", "host": "192.0.2.81", "port": 22,
                   "username": "u", "auth": {"mode": "key", "key_id": "k2"}}),
            json!({"id": "c9", "name": "via-command", "type": "ssh", "host": "192.0.2.91", "port": 22,
                   "username": "u", "auth": {"mode": "agent"}, "network": {"proxy_id": "p2"}}),
            json!({"id": "c10", "name": "via-http", "type": "ssh", "host": "192.0.2.101", "port": 22,
                   "username": "u", "auth": {"mode": "none"}, "network": {"proxy_id": "p3"},
                   "group_id": "g3"}),
            json!({"id": "c11", "name": "lost-jump", "type": "ssh", "host": "192.0.2.111",
                   "port": 22, "username": "u", "network": {"proxy_jump_id": "gone"}}),
            json!({"id": "c12", "name": "quiet-telnet", "type": "telnet", "host": "192.0.2.121",
                   "username": "admin", "auto_login": {"enabled": false}}),
        ],
        keys: vec![
            json!({"id": "k1", "name": "work", "key": secret(&ed25519()),
                   "cert": secret("ssh-ed25519-cert-v01@openssh.com AAAA")}),
            json!({"id": "k2", "name": "broken", "key": secret("not a private key")}),
        ],
        accounts: vec![json!({"id": "a1", "name": "运维账号", "username": "ops",
                              "password": secret("account-pw")})],
        proxies: vec![
            json!({"id": "p1", "name": "socks", "protocol": "socks5", "host": "127.0.0.1",
                   "port": 1080, "username": "pu", "password": proxy_password("proxy-pw")}),
            json!({"id": "p2", "name": "cmd", "protocol": "proxycommand",
                   "command": "ssh -W %h:%p gw"}),
            json!({"id": "p3", "name": "http", "protocol": "http", "host": "proxy.example",
                   "port": 3128, "username": "hu"}),
        ],
        tunnels: vec![
            json!({"id": "t1", "name": "web", "tunnel_type": "local", "connection_id": "c1",
                   "listen_port": 8080, "target_host": "127.0.0.1", "target_port": 80,
                   "auto_open": true, "bind_localhost": true}),
            json!({"id": "t2", "name": "socks", "tunnel_type": "dynamic", "connection_id": "c1",
                   "listen_port": 1081, "bind_localhost": false}),
            json!({"id": "t3", "name": "back", "tunnel_type": "remote", "connection_id": "c2",
                   "listen_port": 9000, "target_host": "localhost", "target_port": 22}),
            json!({"id": "t4", "tunnel_type": "local", "connection_id": "missing",
                   "listen_port": 1, "target_port": 1}),
        ],
        otp: vec![
            json!({"id": "o1", "otp_type": "totp", "issuer": "corp", "username": "dba",
                         "secret": secret("JBSWY3DPEHPK3PXP"), "algorithm": "SHA256",
                         "digits": 8, "period": 60}),
        ],
        quick_commands: json!({"categories": [], "commands": [
            {"id": "q1", "label": "看磁盘", "command": "df -h", "execution_mode": "execute",
             "pinned": false, "use_count": 3}
        ]}),
    }
}

/// 本机数据库：`password` 为 `Some` 时设了主密码（主密钥用它包装，主密码本身用 `material` 加密存在设置中）。
fn local_database(sample: &Sample, material: &[u8], password: Option<&str>) -> Vec<u8> {
    let wrap_material = password.map_or(material, str::as_bytes);
    let mut settings = vec![(
        "settings/doc/quick-command".into(),
        sample.quick_commands.clone(),
    )];
    if let Some(password) = password {
        let token = seal::secret(
            &{
                use sha2::{Digest, Sha256};
                let mut h = Sha256::new();
                h.update(b"nyaterm-key-wrap-v1:");
                h.update(material);
                <[u8; 32]>::from(h.finalize())
            },
            password.as_bytes(),
        );
        settings.push((
            "settings/default".into(),
            json!({"security": {"master_password": token}}),
        ));
    }
    let mut credentials = rows("credentials/key/", &sample.keys);
    credentials.extend(rows("credentials/password/", &sample.accounts));
    credentials.push((
        "credentials/connection-password/c7".into(),
        json!({"id": "x", "connection_id": "c7", "password": "plain-legacy"}),
    ));
    let mut connections = rows("connections/", &sample.connections);
    connections.push((
        "connections/c7".into(),
        json!({"id": "c7", "name": "legacy", "type": "ssh", "host": "192.0.2.71", "port": 22,
               "username": "old", "auth": {"mode": "password"}}),
    ));
    connections.push((
        "connection_custom_icons/i1".into(),
        json!({"id": "i1", "name": "图标"}),
    ));
    database(
        &[
            ("groups", rows("groups/", &sample.groups)),
            ("connections", connections),
            ("credentials", credentials),
            ("proxies", rows("proxies/", &sample.proxies)),
            ("tunnels", rows("tunnels/", &sample.tunnels)),
            ("otp_accounts", rows("otp_accounts/", &sample.otp)),
            ("settings", settings),
        ],
        &[(
            "meta",
            vec![(
                "security/master_key".into(),
                seal::wrap_master_key(&MASTER, wrap_material, false),
            )],
        )],
    )
}

/// `.nya` 中的快照（redb），主密钥用文件密码包装。
fn snapshot(sample: &Sample, password: &str, version: u64) -> Vec<u8> {
    let entities = [
        (
            "sessions",
            json!({"groups": sample.groups, "connections": sample.connections}),
        ),
        ("keys", json!({"keys": sample.keys})),
        ("passwords", json!({"passwords": sample.accounts})),
        ("otp", json!({"entries": sample.otp})),
        ("proxies", json!(sample.proxies)),
        ("tunnels", json!(sample.tunnels)),
        ("quick_commands", sample.quick_commands.clone()),
        (
            "master_key_token",
            json!(seal::wrap_master_key(
                &MASTER,
                password.trim().as_bytes(),
                false
            )),
        ),
        ("settings", json!({"security": {"master_password": null}})),
    ];
    database(
        &[],
        &[
            (
                "snapshot_meta",
                vec![(
                    "meta".into(),
                    json!({"schema_version": version, "snapshot_kind": "backup",
                           "revision_id": "r", "device_id": "native-local", "created_at_ms": 1,
                           "payload_hash": "h", "app_version": "2.0.7"})
                    .to_string(),
                )],
            ),
            (
                "entity_docs",
                entities
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
        ],
    )
}

fn backup(sample: &Sample, password: &str) -> Vec<u8> {
    let zip = zip::write(&[
        (
            "manifest.json",
            br#"{"format":"nyaterm-portable-snapshot-zip","version":1,"payload":"snapshot.redb"}"#,
        ),
        ("snapshot.redb", &snapshot(sample, password, 3)),
    ]);
    seal::snapshot(password, &zip)
}

fn host<'a>(plan: &'a ImportPlan, name: &str) -> &'a PlannedHost {
    plan.hosts
        .iter()
        .find(|h| h.host.name == name)
        .unwrap_or_else(|| panic!("没有 {name}"))
}

fn group(plan: &ImportPlan, h: &PlannedHost) -> Vec<String> {
    h.group
        .map(|g| plan.group_path(g).into_iter().map(str::to_string).collect())
        .unwrap_or_default()
}

fn password(h: &PlannedHost) -> Option<String> {
    h.password().map(|p| p.expose().to_string())
}

/// 账号导入成凭据档案：主机跟随账号的用户名，自己的留空；判断重复时用档案的用户名。
fn check_account(plan: &ImportPlan) {
    let account = host(plan, "via-account");
    assert_eq!(account.host.username, "", "用户名跟随账号");
    assert_eq!(plan.username_of(account), "ops");
    assert_eq!(account.endpoint(), Endpoint::new("192.0.2.31", 22, "ops"));
    assert_eq!(password(account), None, "密码在档案中");
    assert_eq!(plan.profiles.len(), 1);
    let profile = &plan.profiles[account.profile().unwrap()];
    assert_eq!(
        (profile.name.as_str(), profile.username.as_str()),
        ("运维账号", "ops")
    );
    assert_eq!(profile.auth, PlannedProfileAuth::Password);
    assert_eq!(
        profile
            .password()
            .map(|p| p.expose().to_string())
            .as_deref(),
        Some("account-pw")
    );
    assert!(plan.notes.is_empty());
}

fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// 两种来源都应得到的结果。
fn check_common(plan: &ImportPlan) {
    assert_eq!(plan.source, Source::NyaTermData);
    assert_eq!(plan.skipped, 1, "RDP 跳过并计数");

    let web = host(plan, "web-1");
    assert_eq!(group(plan, web), ["生产"]);
    assert_eq!(
        (
            web.host.address.as_str(),
            web.host.port,
            web.host.username.as_str()
        ),
        ("192.0.2.11", 22, "root")
    );
    assert_eq!(password(web).as_deref(), Some("pw-1"));
    assert_eq!(web.host.note, "前端");
    assert_eq!(web.host.tags, ["prod", "web"]);
    assert_eq!(web.host.terminal.encoding, Encoding::Gb18030);
    assert!(web.host.ssh.x11_forward);
    assert_eq!(web.host.terminal.startup, "uptime");
    assert!(web.host.terminal.record);
    let proxy = web.host.proxy.as_ref().unwrap();
    assert_eq!(
        (
            proxy.kind,
            proxy.address.as_str(),
            proxy.port,
            proxy.username.as_str()
        ),
        (ProxyKind::Socks5, "127.0.0.1", 1080, "pu")
    );
    assert_eq!(
        web.proxy_password()
            .map(|p| p.expose().to_string())
            .as_deref(),
        Some("proxy-pw")
    );
    assert!(web.hints.is_empty(), "{:?}", web.hints);
    let forwards: Vec<_> = web
        .forwards
        .iter()
        .map(|f| {
            (
                f.kind,
                f.bind_host.as_str(),
                f.bind_port,
                f.target_host.as_str(),
                f.target_port,
                f.auto_start,
            )
        })
        .collect();
    assert_eq!(
        forwards,
        [
            (
                ForwardKind::Local,
                "127.0.0.1",
                8080,
                "127.0.0.1",
                80,
                false
            ),
            (ForwardKind::Dynamic, "0.0.0.0", 1081, "", 0, false),
        ]
    );

    let db = host(plan, "db-1");
    assert_eq!(group(plan, db), ["生产", "数据库"]);
    assert_eq!(db.host.port, 2222);
    let key = db.key().expect("私钥存进密钥库");
    assert_eq!(plan.keys[key].name, "work");
    assert_eq!(plan.keys[key].algorithm, KeyAlgorithm::Ed25519);
    let jump = db.jump.as_ref().unwrap();
    assert_eq!(jump.name, "web-1");
    assert_eq!(
        jump.host,
        Some(
            plan.hosts
                .iter()
                .position(|h| h.host.name == "web-1")
                .unwrap()
        )
    );
    assert_eq!(
        db.totp().map(|t| t.expose().to_string()).as_deref(),
        Some("otpauth://totp/NyaTerm?secret=JBSWY3DPEHPK3PXP&algorithm=SHA256&digits=8&period=60")
    );
    assert_eq!(
        db.hints,
        [
            Hint::CertificateNotImported {
                name: "work".into()
            },
            Hint::AgentForwardDisabled,
        ]
    );
    assert_eq!(db.forwards.len(), 1);
    assert_eq!(db.forwards[0].kind, ForwardKind::Remote);

    check_account(plan);

    let telnet = host(plan, "switch");
    assert_eq!(telnet.host.protocol, Protocol::Telnet);
    assert_eq!(telnet.host.username, "admin");
    assert_eq!(password(telnet).as_deref(), Some("telnet-pw"));
    assert_eq!(
        telnet.hints,
        [Hint::UnsupportedEncoding {
            name: "Shift_JIS".into()
        }]
    );
    let quiet = host(plan, "quiet-telnet");
    assert_eq!(quiet.host.username, "", "关了自动登录");

    let console = host(plan, "console");
    assert_eq!(console.host.protocol, Protocol::Serial);
    assert_eq!(console.host.address, "COM3");
    let serial = &console.host.serial;
    assert_eq!(
        (
            serial.baud,
            serial.data_bits,
            serial.parity,
            serial.stop_bits,
            serial.flow_control
        ),
        (9600, 7, Parity::Even, StopBits::Two, FlowControl::RtsCts)
    );

    let bad = host(plan, "bad-key");
    assert_eq!(bad.key(), None);
    assert_eq!(
        bad.hints,
        [Hint::LibraryKeyUnusable {
            name: "broken".into()
        }]
    );
    let command = host(plan, "via-command");
    assert_eq!(command.host.auth, Auth::Agent);
    let proxy = command.host.proxy.as_ref().unwrap();
    assert_eq!(
        (proxy.kind, proxy.command.as_str()),
        (ProxyKind::Command, "ssh -W %h:%p gw")
    );
    assert_eq!(command.hints, [Hint::ProxyCommandImported]);
    let http = host(plan, "via-http");
    assert_eq!(group(plan, http), ["环 B", "环 A"], "成环的上级停在那里");
    assert_eq!(http.host.proxy.as_ref().unwrap().kind, ProxyKind::Http);
    assert_eq!(http.hints, [Hint::ProxyPasswordRequired]);
    assert_eq!(host(plan, "lost-jump").hints, [Hint::JumpNotFound]);

    assert_eq!(plan.snippets.len(), 1);
    assert_eq!(
        (
            plan.snippets[0].name.as_str(),
            plan.snippets[0].content.as_str()
        ),
        ("看磁盘", "df -h\n")
    );
}

#[test]
fn reads_local_database() {
    let dir = tempfile::tempdir().unwrap();
    let sample = sample(false);
    write_file(
        dir.path(),
        DATABASE_FILE,
        &local_database(&sample, &home_material(), None),
    );
    // 选目录或其中的数据库都可以
    for path in [dir.path().to_path_buf(), dir.path().join(DATABASE_FILE)] {
        let plan = read(&path, &env(), None).unwrap();
        check_common(&plan);
        let legacy = host(&plan, "legacy");
        assert_eq!(
            password(legacy).as_deref(),
            Some("plain-legacy"),
            "更早的版本单独存放的明文密码"
        );
    }
}

#[test]
fn master_password_and_portable_key() {
    // 设了主密码：主密码本身用主目录路径加密存在设置中
    let dir = tempfile::tempdir().unwrap();
    let sample = sample(false);
    write_file(
        dir.path(),
        DATABASE_FILE,
        &local_database(&sample, &home_material(), Some("主密码")),
    );
    check_common(&read(dir.path(), &env(), None).unwrap());

    // 便携版：材料是数据库旁 portable.key 的内容（去掉首尾空白）
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), PORTABLE_KEY, b"  portable-material\r\n");
    write_file(
        dir.path(),
        DATABASE_FILE,
        &local_database(&sample, b"portable-material", None),
    );
    check_common(&read(dir.path(), &env(), None).unwrap());
}

#[test]
fn other_computer_keeps_hosts_without_secrets() {
    let dir = tempfile::tempdir().unwrap();
    write_file(
        dir.path(),
        DATABASE_FILE,
        &local_database(&sample(false), b"/home/someone-else", None),
    );
    let plan = read(dir.path(), &env(), None).unwrap();
    assert_eq!(plan.hosts.len(), 11);
    let web = host(&plan, "web-1");
    assert_eq!(password(web), None);
    assert_eq!(
        web.hints,
        [Hint::ProxyPasswordRequired],
        "密码解不开在计划中说明一次，不逐台提醒"
    );
    assert_eq!(plan.notes, [PlanNote::SecretsUnreadable]);
    let account = &plan.profiles[host(&plan, "via-account").profile().unwrap()];
    assert!(!account.has_password(), "账号的密码也解不开");
    assert_eq!(host(&plan, "db-1").key(), None);
    assert_eq!(
        password(host(&plan, "legacy")).as_deref(),
        Some("plain-legacy"),
        "明文不需要主密钥"
    );
}

#[test]
fn encrypted_backup() {
    let dir = tempfile::tempdir().unwrap();
    let file = write_file(
        dir.path(),
        "nyaterm-encrypted.nya",
        &backup(&sample(true), "nya-pass"),
    );
    assert!(matches!(
        read(&file, &env(), None),
        Err(ImportError::PasswordRequired)
    ));
    assert!(matches!(
        read(&file, &env(), Some(&SecretString::new("wrong"))),
        Err(ImportError::WrongPassword)
    ));
    let plan = read(&file, &env(), Some(&SecretString::new(" nya-pass "))).unwrap();
    check_common(&plan);
    assert!(
        plan.hosts.iter().all(|h| h.host.name != "legacy"),
        "样例中这条明文记录只在本机数据库里"
    );
}

#[test]
fn legacy_envelope_and_raw_snapshot() {
    // 没有文件头的旧外层，里面直接是 redb（不经 zip）
    let dir = tempfile::tempdir().unwrap();
    let raw = snapshot(&sample(true), "old", 3);
    let file = write_file(dir.path(), "old.nya", &seal::legacy_snapshot("old", &raw));
    check_common(&read(&file, &env(), Some(&SecretString::new("old"))).unwrap());
}

#[test]
fn not_nyaterm_data() {
    let dir = tempfile::tempdir().unwrap();
    let not = |path: &Path, password: Option<&str>| {
        let password = password.map(SecretString::new);
        matches!(
            read(path, &env(), password.as_ref()),
            Err(ImportError::NotNyaTermData(_))
        )
    };
    // 没有 connections 表的 redb
    let other = write_file(
        dir.path(),
        "other.redb",
        &database(&[("things", vec![("a".into(), json!(1))])], &[]),
    );
    assert!(not(&other, None));
    // 旧版本（schema_version 2）的备份
    let zip = zip::write(&[("snapshot.redb", &snapshot(&sample(true), "p", 2))]);
    let v2 = write_file(dir.path(), "v2.nya", &seal::snapshot("p", &zip));
    assert!(not(&v2, Some("p")));
    // 外层解开后不是 zip 也不是 redb
    let junk = write_file(dir.path(), "junk.nya", &seal::snapshot("p", b"junk"));
    assert!(not(&junk, Some("p")));
    assert!(matches!(
        read(dir.path().join("empty-dir").as_path(), &env(), None),
        Err(ImportError::Io { .. })
    ));
    let empty = dir.path().join("no-db");
    std::fs::create_dir(&empty).unwrap();
    assert!(matches!(
        read(&empty, &env(), None),
        Err(ImportError::NyaTermNotFound)
    ));
}

#[test]
fn location_json_and_default_path() {
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let config = home.path().join(".nyaterm");
    std::fs::create_dir(&config).unwrap();
    write_file(
        &config,
        LOCATION_FILE,
        json!({"version": 1, "data_dir": data.path()})
            .to_string()
            .as_bytes(),
    );
    let db = write_file(
        data.path(),
        DATABASE_FILE,
        &local_database(&sample(false), &home_material(), None),
    );
    assert_eq!(database_in(&config), Some(db.clone()));
    let mut env = env();
    let material_home = env.home.clone();
    env.home = Some(home.path().to_path_buf());
    if std::env::var_os(DATA_DIR_ENV).is_none() {
        assert_eq!(default_path(&env), Some(db));
    }
    // 读取时用测试环境的主目录作材料
    env.home = material_home;
    assert_eq!(read(&config, &env, None).unwrap().hosts.len(), 11);
}

/// NyaTerm 自己写出的文件：在临时目录中用 NyaTerm 提交 290212c 的 `nyaterm-store`（`ConnectionStore`）写入一组
/// 假数据（便携版的 `portable.key` 为 `termlet-fixture-portable-key`、主密码为“主密码”），再导出加密的 `.nya`
/// （密码 `nya-pass`）。`nyaterm.redb.gz` 是那份本机数据库（redb 4.3 写的，1 MiB，gzip 压缩），
/// `nyaterm-fixture.nya` 是导出的备份。私钥是为此生成的一次性测试密钥（第二把的密码是 `keypass`）。
fn check_fixture(plan: &ImportPlan) {
    assert_eq!(plan.skipped, 1, "RDP");
    let names: HashSet<&str> = plan.hosts.iter().map(|h| h.host.name.as_str()).collect();
    assert_eq!(
        names,
        HashSet::from([
            "web-1",
            "db-1",
            "via-account",
            "switch",
            "console",
            "with-passphrase"
        ])
    );
    let web = host(plan, "web-1");
    assert_eq!(group(plan, web), ["生产"]);
    assert_eq!(password(web).as_deref(), Some("pw-1"));
    assert_eq!(web.host.note, "前端");
    assert_eq!(web.host.tags, ["prod", "web"]);
    assert_eq!(web.host.terminal.encoding, Encoding::Gb18030);
    assert!(web.host.ssh.x11_forward);
    assert_eq!(web.host.terminal.startup, "uptime");
    assert_eq!(
        web.proxy_password()
            .map(|p| p.expose().to_string())
            .as_deref(),
        Some("proxy-pw")
    );
    assert_eq!(web.forwards.len(), 1);
    let db = host(plan, "db-1");
    assert_eq!(group(plan, db), ["生产", "数据库"]);
    assert!(db.key().is_some());
    assert_eq!(db.jump.as_ref().unwrap().name, "web-1");
    assert_eq!(
        db.totp().map(|t| t.expose().to_string()).as_deref(),
        Some("JBSWY3DPEHPK3PXP")
    );
    assert_eq!(db.forwards[0].kind, ForwardKind::Remote);
    check_account(plan);
    assert_eq!(password(host(plan, "switch")).as_deref(), Some("telnet-pw"));
    assert_eq!(host(plan, "console").host.serial.baud, 9600);
    let passphrase = host(plan, "with-passphrase");
    assert!(passphrase.key().is_some(), "用私钥的密码解开");
    assert_ne!(passphrase.key(), db.key());
    assert_eq!(
        passphrase.host.proxy.as_ref().unwrap().command,
        "ssh -W %h:%p gw"
    );
    assert_eq!(plan.keys.len(), 2);
    assert_eq!(
        (
            plan.snippets[0].group.as_str(),
            plan.snippets[0].name.as_str()
        ),
        ("运维", "看磁盘")
    );
}

fn testdata(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/import/testdata/nyaterm")
        .join(name)
}

#[test]
fn files_written_by_nyaterm() {
    let plan = read(
        &testdata("nyaterm-fixture.nya"),
        &env(),
        Some(&SecretString::new("nya-pass")),
    )
    .unwrap();
    check_fixture(&plan);

    let dir = tempfile::tempdir().unwrap();
    let mut db = Vec::new();
    std::io::Read::read_to_end(
        &mut flate2::read::GzDecoder::new(
            std::fs::File::open(testdata("nyaterm.redb.gz")).unwrap(),
        ),
        &mut db,
    )
    .unwrap();
    write_file(dir.path(), DATABASE_FILE, &db);
    write_file(dir.path(), PORTABLE_KEY, b"termlet-fixture-portable-key\n");
    check_fixture(&read(dir.path(), &env(), None).unwrap());
}

/// NyaTerm 运行时用 redb 独占打开数据库，Windows 上别的进程读不了这个文件。
#[cfg(windows)]
#[test]
fn database_in_use() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(
        dir.path(),
        DATABASE_FILE,
        &local_database(&sample(false), &home_material(), None),
    );
    let _open = Database::create(&path).unwrap();
    assert!(matches!(
        read(&path, &env(), None),
        Err(ImportError::NyaTermInUse)
    ));
}
