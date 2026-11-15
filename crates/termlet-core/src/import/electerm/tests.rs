use super::*;
use crate::import::files::tests::{env, write};
use crate::model::Encoding;
use crate::sshkey::KeyKind;

fn testdata() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/import/testdata/electerm/bookmarks-2026-11-09-10-00-00.json")
}

fn by_name<'a>(plan: &'a ImportPlan, name: &str) -> &'a PlannedHost {
    plan.hosts
        .iter()
        .find(|h| h.host.name == name)
        .unwrap_or_else(|| panic!("没有 {name}"))
}

fn proxy_of(h: &PlannedHost) -> Option<(ProxyKind, &str, u16, &str)> {
    h.host
        .proxy
        .as_ref()
        .map(|p| (p.kind, p.address.as_str(), p.port, p.username.as_str()))
}

#[test]
fn unshift_reverses_the_old_password_encoding() {
    assert_eq!(unshift("tgfvjz"), "secret");
    assert_eq!(unshift("QBvw%}7zm+"), "P@ss w0rd!");
    assert_eq!(unshift("寇砃"), "密码");
    assert_eq!(unshift(""), "");
}

#[test]
fn exported_bookmarks() {
    let plan = read(&testdata(), &env()).unwrap();
    assert_eq!(plan.source, Source::Electerm);
    // rdp、web 与没有地址的书签跳过；为跳板机新建了一台
    assert_eq!((plan.hosts.len(), plan.skipped), (11, 3));
    assert!(plan.keys.is_empty());

    let web = by_name(&plan, "web1");
    assert_eq!(web.group, None, "默认分组是根");
    assert_eq!(
        (
            web.host.address.as_str(),
            web.host.port,
            web.host.username.as_str()
        ),
        ("10.0.0.5", 22, "root")
    );
    assert_eq!(web.password().unwrap().expose(), "secret");
    assert_eq!(web.host.note, "主站");
    assert_eq!(web.host.terminal.encoding, Encoding::Gb18030);
    assert!(web.host.ssh.x11_forward);
    assert_eq!(
        web.host.terminal.startup,
        "cd '/var/www/it'\\''s'\nuptime\ndf -h"
    );
    assert_eq!(
        proxy_of(web),
        Some((ProxyKind::Socks5, "127.0.0.1", 1080, "u"))
    );
    assert_eq!(web.proxy_password().unwrap().expose(), "pw");
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
            )
        })
        .collect();
    assert_eq!(
        forwards,
        [
            (ForwardKind::Local, "127.0.0.1", 13306, "db.internal", 3306),
            (ForwardKind::Remote, "127.0.0.1", 9090, "127.0.0.1", 8080),
            (ForwardKind::Dynamic, "127.0.0.1", 1080, "", 0),
        ]
    );
    assert_eq!(web.forwards[0].name, "mysql");
    assert_eq!(
        web.hints,
        [
            Hint::AgentForwardDisabled,
            Hint::ForwardsNotImported { count: 1 }
        ]
    );

    let switch = by_name(&plan, "switch");
    assert_eq!(
        (
            switch.host.protocol,
            switch.host.port,
            switch.host.username.as_str()
        ),
        (Protocol::Telnet, 23, "admin")
    );
    assert_eq!(switch.password().unwrap().expose(), "sw");
    assert!(switch.hints.is_empty());

    let bastion = by_name(&plan, "bastion");
    assert_eq!(
        (bastion.host.protocol, bastion.host.port),
        (Protocol::Ssh, 2222),
        "没有 type 时是 SSH，端口可以是字符串"
    );

    // 跳板机：第一跳是同一文件中的书签，第二跳新建；新建的跳板机经第一跳
    let app = by_name(&plan, "app");
    assert_eq!(plan.group_path(app.group.unwrap()), ["生产"]);
    let jump = app.jump.as_ref().unwrap();
    assert_eq!(jump.name, "hop@jump.internal");
    let hop = &plan.hosts[jump.host.unwrap()];
    assert_eq!(
        (
            hop.host.name.as_str(),
            hop.host.username.as_str(),
            hop.group
        ),
        ("jump.internal", "hop", app.group)
    );
    assert_eq!(
        hop.hints,
        [Hint::JumpHostFor {
            session: "app".into()
        }]
    );
    assert_eq!(hop.password().unwrap().expose(), "hop-pw");
    let hop_jump = hop.jump.as_ref().unwrap();
    assert_eq!(hop_jump.name, "ops@bastion.example.com:2222");
    assert_eq!(plan.hosts[hop_jump.host.unwrap()].host.name, "bastion");
    assert!(app.hints.is_empty(), "{:?}", app.hints);

    // 已新建的跳板机不重复建；有代理时不用代理命令
    let db = by_name(&plan, "db");
    assert_eq!(
        db.jump.as_ref().unwrap().host,
        app.jump.as_ref().unwrap().host
    );
    assert_eq!(proxy_of(db), Some((ProxyKind::Http, "proxy.lan", 8080, "")));
    assert_eq!(db.hints, [Hint::ProxyPortDefaulted { port: 8080 }]);

    // 旧书签的跳板顺序不同，不导入；分组的环到重复处为止
    let legacy = by_name(&plan, "legacy");
    assert_eq!(plan.group_path(legacy.group.unwrap()), ["生产", "子组"]);
    assert_eq!(legacy.jump, None);
    assert_eq!(legacy.hints, [Hint::JumpNotImported]);

    let keyfile = by_name(&plan, "keyfile");
    assert_eq!(
        keyfile.host.auth,
        Auth::Key {
            source: KeySource::file(env().expand_home("~/.ssh/id_rsa"))
        }
    );
    assert_eq!(keyfile.passphrase().unwrap().expose(), "kp");

    // 书签导出中没有凭据档案
    let profiled = by_name(&plan, "profiled");
    assert_eq!(
        profiled.hints,
        [Hint::CredentialProfileMissing, Hint::MissingUsername]
    );

    // 3.0 以前移位保存的密码，旧版的代理对象
    let old = by_name(&plan, "old");
    assert_eq!(old.password().unwrap().expose(), "secret");
    assert_eq!(
        proxy_of(old),
        Some((ProxyKind::Http, "10.9.9.9", 3128, "pu"))
    );
    assert_eq!(old.proxy_password().unwrap().expose(), "pp");

    let serial = by_name(&plan, "console");
    assert_eq!(
        (serial.host.protocol, serial.host.address.as_str()),
        (Protocol::Serial, "COM3")
    );
    let s = &serial.host.serial;
    assert_eq!(
        (s.baud, s.data_bits, s.stop_bits, s.parity, s.flow_control),
        (9600, 7, StopBits::Two, Parity::Even, FlowControl::RtsCts)
    );

    // 凭据不出现在 Debug 中；不同意导入秘密时全部去掉
    assert!(plan.has_secrets());
    let debug = format!("{plan:?}");
    for secret in ["hop-pw", "\"secret\"", "\"pw\""] {
        assert!(!debug.contains(secret), "{secret}");
    }
    let bare = plan.without_secrets();
    assert!(!bare.has_secrets());
    assert_eq!(bare.hosts.len(), plan.hosts.len());
}

/// 生成一把私钥，`passphrase` 非空时加密。
fn key_text(passphrase: &str) -> (String, String) {
    let key = crate::sshkey::generate(KeyKind::Ed25519, "t").unwrap();
    let pass = SecretString::new(passphrase);
    let text = key.export_openssh(Some(&pass)).unwrap();
    (text.expose().to_string(), key.fingerprint())
}

// 私钥内容（带密码的先解开）存进密钥库，同一把只存一次；解不开时提醒；凭据档案在“导出全部数据”中
#[test]
fn private_keys_and_profiles() {
    let (plain, plain_fp) = key_text("");
    let (locked, _) = key_text("kp");
    let doc = serde_json::json!({
        "bookmarks": [
            {"id": "a", "title": "a", "host": "10.0.0.1", "username": "u", "authType": "privateKey", "privateKey": plain},
            {"id": "b", "title": "b", "host": "10.0.0.2", "username": "u", "privateKey": plain},
            {"id": "c", "title": "c", "host": "10.0.0.3", "username": "u", "authType": "privateKey", "privateKey": locked, "passphrase": "kp"},
            {"id": "d", "title": "d", "host": "10.0.0.4", "username": "u", "authType": "privateKey", "privateKey": locked, "passphrase": "wrong"},
            {"id": "e", "title": "e", "host": "10.0.0.5", "authType": "profiles", "profile": "p1"},
            {"id": "f", "type": "telnet", "title": "f", "host": "10.0.0.6", "profile": "p1"},
            {"id": "g", "title": "g", "host": "10.0.0.7", "username": "u", "authType": "privateKey", "privateKey": "garbage", "privateKeyPath": "/k"}
        ],
        "profiles": [
            {"id": "p1", "name": "ops", "username": "pu", "password": "ppw", "telnet": {"username": "tu", "password": "tpw"}}
        ]
    });
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("2026-11-09-electerm-all-data.json");
    write(&file, doc.to_string().as_bytes());
    let plan = read(&file, &env()).unwrap();
    assert_eq!(plan.keys.len(), 2, "同一把私钥只存一次");
    let a = by_name(&plan, "a");
    let b = by_name(&plan, "b");
    let c = by_name(&plan, "c");
    assert_eq!((a.key(), b.key(), c.key()), (Some(0), Some(0), Some(1)));
    assert_eq!(a.host.auth, Auth::Password, "写入时换成密钥库中的私钥");
    let stored =
        crate::sshkey::parse(plan.keys[0].private_key().expose().as_bytes(), None).unwrap();
    assert_eq!(stored.fingerprint(), plain_fp);
    assert_eq!(plan.keys[0].algorithm, KeyAlgorithm::Ed25519);
    assert!(
        crate::sshkey::parse(plan.keys[1].private_key().expose().as_bytes(), None).is_ok(),
        "带密码的私钥解开后存"
    );
    let d = by_name(&plan, "d");
    assert_eq!(d.key(), None);
    assert_eq!(d.hints, [Hint::LibraryKeyUnusable { name: "d".into() }]);
    // 解不开的私钥内容有文件路径时用文件
    let g = by_name(&plan, "g");
    assert_eq!(
        g.host.auth,
        Auth::Key {
            source: KeySource::file("/k")
        }
    );

    let e = by_name(&plan, "e");
    assert_eq!(e.host.username, "pu");
    assert_eq!(e.password().unwrap().expose(), "ppw");
    assert!(e.hints.is_empty());
    let f = by_name(&plan, "f");
    assert_eq!(f.host.username, "tu");
    assert_eq!(f.password().unwrap().expose(), "tpw");

    let bare = plan.without_secrets();
    assert!(bare.keys.is_empty() && bare.hosts.iter().all(|h| h.key().is_none()));
}

#[test]
fn other_export_shapes_and_errors() {
    let plan = parse_export(r#"[{"host":"10.0.0.1","username":"u"}]"#).unwrap();
    assert_eq!(plan.bookmarks.len(), 1);
    let plan = parse_export(r#"{"host":"10.0.0.1"}"#).unwrap();
    assert_eq!(plan.bookmarks.len(), 1);
    assert!(matches!(
        parse_export(r#"{"foo":1}"#),
        Err(ImportError::NotElecterm(_))
    ));
    assert!(matches!(
        parse_export("not json"),
        Err(ImportError::NotElecterm(_))
    ));
    let empty = tempfile::tempdir().unwrap();
    assert!(matches!(
        read(empty.path(), &env()),
        Err(ImportError::NotElecterm(_))
    ));
}

const PLAIN_BOOKMARK: &str = r#"{"title":"plain","host":"10.0.0.1","username":"u","password":"tgfvjz","passwordEncrypted":true}"#;

// NeDB：同一 `_id` 以最后一行为准，删除的不要；`_encdata` 是系统加密的跳过并计数，`enc:` 加明文 JSON 的照读
#[test]
fn nedb_files() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("users").join(DEFAULT_USER);
    let bookmarks = [
        r#"{"_id":"a","title":"old","host":"10.0.0.9","username":"u"}"#.to_string(),
        format!(r#"{{"_id":"b",{}"#, &PLAIN_BOOKMARK[1..]),
        r#"{"_id":"a","title":"new","host":"10.0.0.2","username":"u"}"#.into(),
        r#"{"_id":"c","title":"gone","host":"10.0.0.3","username":"u"}"#.into(),
        r#"{"$$deleted":true,"_id":"c"}"#.into(),
        r#"{"_id":"d","_encdata":"enc:v2:safe:djEwAAAA","title":"stale"}"#.into(),
        r#"{"_id":"e","_encdata":"enc:{\"title\":\"plain-enc\",\"host\":\"10.0.0.5\",\"username\":\"u\"}"}"#.into(),
        r#"{"$$indexCreated":{"fieldName":"_id","unique":true}}"#.into(),
        "broken".into(),
    ];
    write(
        &user.join("electerm.bookmarks.nedb"),
        bookmarks.join("\n").as_bytes(),
    );
    write(
        &user.join("electerm.bookmarkGroups.nedb"),
        br#"{"_id":"g1","title":"G","bookmarkIds":["e"],"bookmarkGroupIds":[]}"#,
    );
    // 数据目录、用户目录、NeDB 文件都可以选
    for path in [
        dir.path().to_path_buf(),
        user.clone(),
        user.join("electerm.bookmarks.nedb"),
    ] {
        let plan = read(&path, &env()).unwrap();
        let names: Vec<&str> = plan.hosts.iter().map(|h| h.host.name.as_str()).collect();
        assert_eq!(names, ["new", "plain", "plain-enc"], "{}", path.display());
        assert_eq!(plan.skipped, 1, "系统加密的书签");
        assert_eq!(
            by_name(&plan, "plain").password().unwrap().expose(),
            "secret"
        );
        let enc = by_name(&plan, "plain-enc");
        assert_eq!(plan.group_path(enc.group.unwrap()), ["G"]);
    }

    // 迁移到 SQLite 后留下的备份
    let bak = tempfile::tempdir().unwrap();
    write(
        &bak.path().join("electerm.bookmarks.nedb.bak"),
        r#"{"_id":"a","host":"10.0.0.1","username":"u"}"#.as_bytes(),
    );
    assert_eq!(read(bak.path(), &env()).unwrap().hosts.len(), 1);
}

fn write_sqlite(file: &Path, bookmarks: &[(&str, &str)], groups: &[(&str, &str)]) {
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let conn = Connection::open(file).unwrap();
    for (table, rows) in [("bookmarks", bookmarks), ("bookmarkGroups", groups)] {
        conn.execute(
            &format!("CREATE TABLE `{table}` (_id TEXT PRIMARY KEY, data TEXT)"),
            [],
        )
        .unwrap();
        for (id, data) in rows {
            conn.execute(
                &format!("INSERT INTO `{table}` (_id, data) VALUES (?1, ?2)"),
                [id, data],
            )
            .unwrap();
        }
    }
}

#[test]
fn sqlite_database() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir
        .path()
        .join("electerm")
        .join("users")
        .join(DEFAULT_USER)
        .join(SQLITE_FILE);
    let enc_plain = format!("enc:{PLAIN_BOOKMARK}");
    write_sqlite(
        &file,
        &[
            (
                "a",
                r#"{"title":"a","host":"10.0.0.1","username":"u","type":"ssh"}"#,
            ),
            ("b", enc_plain.as_str()),
            ("c", "enc:v2:safe:djEwAAAA"),
        ],
        &[
            (
                "default",
                r#"{"title":"default","bookmarkIds":["a"],"bookmarkGroupIds":["g"]}"#,
            ),
            ("g", r#"{"title":"G","bookmarkIds":["b"]}"#),
        ],
    );
    for path in [dir.path().to_path_buf(), file.clone()] {
        let plan = read(&path, &env()).unwrap();
        let names: Vec<&str> = plan.hosts.iter().map(|h| h.host.name.as_str()).collect();
        assert_eq!(names, ["a", "plain"]);
        assert_eq!(plan.skipped, 1);
        let plain = by_name(&plan, "plain");
        assert_eq!(plan.group_path(plain.group.unwrap()), ["G"]);
        assert_eq!(plain.password().unwrap().expose(), "secret");
        assert_eq!(by_name(&plan, "a").group, None);
    }

    // 3.0 起全部书签都用系统加密：请用户先导出
    let locked = tempfile::tempdir().unwrap();
    write_sqlite(
        &locked.path().join(SQLITE_FILE),
        &[("a", "enc:v2:safe:AAAA"), ("b", "enc:v2:safe:BBBB")],
        &[],
    );
    assert!(matches!(
        read(locked.path(), &env()),
        Err(ImportError::ElectermEncrypted)
    ));
    // 不是数据库
    let bogus = tempfile::tempdir().unwrap();
    write(&bogus.path().join(SQLITE_FILE), b"not a database");
    assert!(matches!(
        read(bogus.path(), &env()),
        Err(ImportError::NotElecterm(_))
    ));
}

#[test]
fn default_path_prefers_exports_then_data() {
    let dir = tempfile::tempdir().unwrap();
    let mut env = env();
    assert_eq!(default_path(&env), None);
    let config = dir.path().join("config");
    env.config_dir = Some(config.clone());
    let user = config.join("electerm").join("users").join(DEFAULT_USER);
    write(&user.join("electerm.bookmarks.nedb"), b"");
    assert_eq!(default_path(&env), Some(user));
    let downloads = dir.path().join("downloads");
    write(&downloads.join("notes.json"), b"{}");
    write(&downloads.join("bookmarks-2026-11-09-10-00-00.json"), b"{}");
    env.downloads = Some(downloads.clone());
    assert_eq!(
        default_path(&env),
        Some(downloads.join("bookmarks-2026-11-09-10-00-00.json"))
    );
    assert!(is_export_name("2026-11-09-10-00-00-electerm-all-data.json"));
}
