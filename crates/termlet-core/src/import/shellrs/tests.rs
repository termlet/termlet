//! 测试样例都在测试中现场生成：数据库按 ShellRS 提交 98d471c 的 `crates/shellrs-hosts/src/database.rs` 中成文的
//! 列手写建表（只建用到的表与列），同步文件夹按 `crates/shellrs-sync/FORMAT.md` 加密（见 [`super::sync::writer`]）。
//! `testdata/shellrs/` 中的两个文件是 ShellRS 自己写出的同步数据（MIT，见那里的 LICENSE），核对与真实 ShellRS 互通。

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde_json::{Value, json};

use super::sync::writer;
use super::*;
use crate::import::files::tests::env as base_env;
use crate::import::{ImportError, ImportPlan, PlannedHost};
use crate::model::{Auth, ForwardKind, Protocol, ProxyKind};

const PASSWORD: &str = "sync password 12";

fn env() -> LocalEnv {
    base_env()
}

fn host<'a>(plan: &'a ImportPlan, name: &str) -> &'a PlannedHost {
    plan.hosts
        .iter()
        .find(|h| h.host.name == name)
        .unwrap_or_else(|| panic!("没有主机 {name}：{:?}", names(plan)))
}

fn names(plan: &ImportPlan) -> Vec<&str> {
    plan.hosts.iter().map(|h| h.host.name.as_str()).collect()
}

fn group(plan: &ImportPlan, h: &PlannedHost) -> Vec<String> {
    h.group
        .map(|g| plan.group_path(g).into_iter().map(String::from).collect())
        .unwrap_or_default()
}

fn exposed(secret: Option<&SecretString>) -> Option<&str> {
    secret.map(SecretString::expose)
}

// ---------------------------------------------------------------- 数据库

/// 结构版本：20（0.1.16–0.1.17）、24（0.1.19）、25（之后）。
fn create_database(dir: &Path, version: u32) -> Connection {
    std::fs::create_dir_all(dir).unwrap();
    let conn = Connection::open(dir.join(db::DATABASE_FILE)).unwrap();
    let mut hosts = String::from(
        "id INTEGER PRIMARY KEY, public_id TEXT, group_id INTEGER, sort_order INTEGER NOT NULL DEFAULT 0,
         name TEXT NOT NULL, address TEXT NOT NULL, port INTEGER NOT NULL, auth TEXT NOT NULL, username TEXT,
         credential_id INTEGER, route TEXT NOT NULL DEFAULT 'direct', proxy_kind TEXT, proxy_host TEXT,
         proxy_port INTEGER, proxy_username TEXT, notes TEXT NOT NULL DEFAULT '', os TEXT,
         last_connected_at INTEGER, protocol TEXT NOT NULL DEFAULT 'ssh', serial_baud INTEGER,
         serial_data_bits INTEGER, serial_parity TEXT, serial_stop_bits INTEGER, serial_flow TEXT,
         telnet_user_prompt TEXT, telnet_pass_prompt TEXT, sidebar_tool TEXT,
         edited_ms INTEGER NOT NULL DEFAULT 0",
    );
    let mut categories = String::from(
        "id INTEGER PRIMARY KEY, name TEXT NOT NULL, sync_id TEXT NOT NULL DEFAULT '',
         edited_ms INTEGER NOT NULL DEFAULT 0",
    );
    let mut snippets = String::from(
        "id INTEGER PRIMARY KEY, category_id INTEGER, name TEXT NOT NULL, command TEXT NOT NULL,
         scope_group_id INTEGER, scope_host_id INTEGER, run_on_click INTEGER NOT NULL DEFAULT 0,
         sync_id TEXT NOT NULL DEFAULT '', edited_ms INTEGER NOT NULL DEFAULT 0",
    );
    if version >= 21 {
        categories.push_str(", parent_id INTEGER, sort_order INTEGER NOT NULL DEFAULT 0");
        snippets.push_str(", sort_order INTEGER NOT NULL DEFAULT 0");
    }
    if version >= 22 {
        hosts.push_str(", directory_follow INTEGER");
    }
    if version >= 23 {
        hosts.push_str(", credential_username TEXT");
    }
    if version >= 24 {
        hosts.push_str(", one_time_code INTEGER NOT NULL DEFAULT 0, one_time_code_prompt TEXT");
    }
    if version >= 25 {
        hosts.push_str(", login_commands TEXT NOT NULL DEFAULT ''");
    }
    conn.execute_batch(&format!(
        "PRAGMA user_version = {version};
         CREATE TABLE groups (id INTEGER PRIMARY KEY, parent_id INTEGER, name TEXT NOT NULL,
             sort_order INTEGER NOT NULL DEFAULT 0, expanded INTEGER NOT NULL DEFAULT 1,
             sync_id TEXT NOT NULL DEFAULT '', edited_ms INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE credentials (id INTEGER PRIMARY KEY, keychain_id TEXT NOT NULL UNIQUE, name TEXT NOT NULL,
             kind TEXT NOT NULL, username TEXT NOT NULL, key_path TEXT, sort_order INTEGER NOT NULL DEFAULT 0,
             edited_ms INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE hosts ({hosts});
         CREATE TABLE host_jumps (host_id INTEGER NOT NULL, position INTEGER NOT NULL, jump_id INTEGER,
             PRIMARY KEY (host_id, position));
         CREATE TABLE bookmarks (host_id INTEGER NOT NULL, side TEXT NOT NULL, path TEXT NOT NULL,
             sort_order INTEGER NOT NULL, edited_ms INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE forwards (id INTEGER PRIMARY KEY, host_id INTEGER NOT NULL, name TEXT NOT NULL DEFAULT '',
             kind TEXT NOT NULL, bind_host TEXT NOT NULL, bind_port INTEGER NOT NULL, target_host TEXT,
             target_port INTEGER, auto_start INTEGER NOT NULL DEFAULT 0, sort_order INTEGER NOT NULL DEFAULT 0,
             sync_id TEXT NOT NULL DEFAULT '', edited_ms INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE snippet_categories ({categories});
         CREATE TABLE snippets ({snippets});"
    ))
    .unwrap();
    conn
}

/// 一组覆盖各种情况的数据（结构版本 24 起的列）。
fn fill(conn: &Connection, dir: &Path) {
    let key_dir = dir.join("keys");
    std::fs::create_dir_all(&key_dir).unwrap();
    let plain_key = crate::sshkey::generate(crate::sshkey::KeyKind::Ed25519, "deploy@example")
        .unwrap()
        .to_openssh()
        .unwrap();
    std::fs::write(key_dir.join("aZ3plain"), plain_key.expose()).unwrap();
    // PuTTY 的公开测试密钥（加密，见 sshkey/testdata/README.md）
    let encrypted = include_bytes!("../../sshkey/testdata/putty/ed25519-v2-aes.ppk");
    std::fs::write(key_dir.join("aZ3locked"), encrypted).unwrap();
    let path = |name: &str| key_dir.join(name).display().to_string();
    conn.execute_batch(&format!(
        "INSERT INTO groups (id, parent_id, name, sort_order) VALUES
            (1, NULL, '生产', 1), (2, 1, '数据库', 0), (3, NULL, '测试', 0), (4, 4, '自己', 0);
         INSERT INTO credentials (id, keychain_id, name, kind, username, key_path) VALUES
            (1, 'c-pass', '运维', 'password', 'ops', NULL),
            (2, 'c-key', 'deploy', 'key', 'deploy', '{}'),
            (3, 'c-locked', 'locked', 'key', 'admin', '{}'),
            (4, 'c-agent', 'agent', 'agent', 'agent-user', NULL),
            (5, 'c-gone', 'gone', 'key', 'nobody', '{}');",
        path("aZ3plain"),
        path("aZ3locked"),
        path("missing"),
    ))
    .unwrap();
    conn.execute_batch(
        "INSERT INTO hosts (id, public_id, group_id, sort_order, name, address, port, auth, username,
                            credential_id, credential_username, route, proxy_kind, proxy_host, proxy_port,
                            proxy_username, notes, protocol, one_time_code, one_time_code_prompt) VALUES
            (1, 'pub-bastion', 1, 0, 'bastion', 'bastion.example', 22, 'password', 'jump', NULL, NULL,
                'direct', NULL, NULL, NULL, NULL, '跳板机', 'ssh', 1, NULL),
            (2, 'pub-db', 2, 1, 'db-1', '10.0.0.5', 2222, 'credential', NULL, 2, NULL,
                'jump', NULL, NULL, NULL, NULL, '', 'ssh', 0, NULL),
            (3, 'pub-web', 2, 0, 'web-1', '10.0.0.6', 22, 'credential', NULL, 1, 'web',
                'jump', NULL, NULL, NULL, NULL, '', 'ssh', 1, 'Verification code'),
            (4, 'pub-inner', 3, 0, 'inner', '10.1.0.1', 22, 'credential', NULL, 3, NULL,
                'jump', NULL, NULL, NULL, NULL, '', 'ssh', 0, NULL),
            (5, 'pub-agent', 3, 1, 'agent-host', 'agent.example', 22, 'no-password', 'me', NULL, NULL,
                'proxy', 'socks5', '127.0.0.1', 1080, 'proxy-user', '', 'ssh', 0, NULL),
            (6, 'pub-http', NULL, 0, 'via-http', 'http.example', 22, 'credential', NULL, 4, NULL,
                'proxy', 'http', 'proxy.example', 3128, NULL, '', 'ssh', 0, NULL),
            (7, 'pub-broken', NULL, 1, 'broken-key', 'broken.example', 22, 'credential', NULL, 5, NULL,
                'jump', NULL, NULL, NULL, NULL, '', 'ssh', 0, NULL),
            (8, NULL, 4, 0, 'loop-group', 'loop.example', 22, 'password', 'root', NULL, NULL,
                'direct', NULL, NULL, NULL, NULL, '', 'ssh', 0, NULL),
            (9, 'pub-rdp', NULL, 9, 'rdp', 'rdp.example', 3389, 'password', 'x', NULL, NULL,
                'direct', NULL, NULL, NULL, NULL, '', 'rdp', 0, NULL);
         INSERT INTO hosts (id, public_id, group_id, sort_order, name, address, port, auth, username,
                            credential_id, route, protocol, telnet_user_prompt) VALUES
            (10, 'pub-switch', 3, 2, 'switch', '10.2.0.1', 23, 'credential', NULL, 1, 'jump', 'telnet',
                'Login:'),
            (11, 'pub-console', 3, 3, 'console', '10.2.0.2', 23, 'no-password', 'admin', NULL, 'direct',
                'telnet', NULL);
         INSERT INTO hosts (id, public_id, name, address, port, auth, username, protocol, serial_baud,
                            serial_data_bits, serial_parity, serial_stop_bits, serial_flow) VALUES
            (12, 'pub-com', 'COM3', 'COM3', 1, 'no-password', 'root', 'serial', 9600, 7, 'even', 2, 'rts-cts');
         INSERT INTO host_jumps (host_id, position, jump_id) VALUES
            (2, 0, 1),
            (3, 0, 1), (3, 1, 2),
            (4, 0, 1), (4, 1, 3),
            (7, 0, NULL),
            (10, 0, 1);
         INSERT INTO forwards (host_id, name, kind, bind_host, bind_port, target_host, target_port, auto_start,
                               sort_order) VALUES
            (2, 'mysql', 'local', '127.0.0.1', 3306, 'db.internal', 3306, 1, 0),
            (2, '', 'dynamic', '192.168.1.10', 1080, NULL, NULL, 0, 1),
            (2, 'web', 'remote', '0.0.0.0', 8080, 'localhost', 3000, 0, 2),
            (10, 'telnet', 'local', '127.0.0.1', 9000, 'x', 1, 0, 0);
         INSERT INTO snippet_categories (id, name, parent_id, sort_order) VALUES
            (1, '运维', NULL, 0), (2, 'K8s', 1, 0);
         INSERT INTO snippets (category_id, name, command, run_on_click, sort_order) VALUES
            (2, 'pods', 'kubectl get pods -n {{ns}}', 1, 0),
            (1, 'disk', 'df -h', 0, 0),
            (NULL, 'uptime', 'uptime', 1, 0);",
    )
    .unwrap();
}

fn database_plan(version: u32) -> (tempfile::TempDir, ImportPlan) {
    let dir = tempfile::tempdir().unwrap();
    let conn = create_database(dir.path(), version);
    fill(&conn, dir.path());
    drop(conn);
    let plan = read(dir.path(), &env(), None).unwrap();
    (dir, plan)
}

#[test]
fn reads_the_data_directory() {
    let (_dir, plan) = database_plan(24);
    assert_eq!(plan.source, Source::ShellRs);
    assert_eq!(plan.skipped, 1, "RDP 跳过");
    assert!(plan.notes.contains(&PlanNote::SecretsInKeychain));

    // 分组在前，同一层按 ShellRS 的顺序；成环的上级停在那里
    let bastion = host(&plan, "bastion");
    assert_eq!(group(&plan, bastion), ["生产"]);
    assert_eq!(group(&plan, host(&plan, "db-1")), ["生产", "数据库"]);
    assert_eq!(group(&plan, host(&plan, "loop-group")), ["自己"]);
    let order = names(&plan);
    let pos = |n| order.iter().position(|x| *x == n).unwrap();
    assert!(
        pos("inner") < pos("bastion"),
        "“测试”（顺序 0）在“生产”（1）前：{order:?}"
    );
    assert!(pos("web-1") < pos("db-1"), "同一分组中按顺序");
    assert!(pos("bastion") > pos("web-1"), "同一层中分组在主机前");

    // 密码登录：钥匙串中的密码不导入；验证码的密钥也没有
    assert_eq!(bastion.host.username, "jump");
    assert_eq!(bastion.host.auth, Auth::Password);
    assert!(!bastion.has_password());
    assert_eq!(bastion.host.note, "跳板机");
    assert!(bastion.hints.contains(&Hint::TotpSecretMissing));
    // 密码都在钥匙串里；只有私钥文件读得到（存进密钥库）
    assert!(
        plan.hosts
            .iter()
            .all(|h| !h.has_password() && h.totp().is_none())
    );
    assert!(plan.profiles.iter().all(|p| !p.has_password()));

    // 凭据导入成档案；主机自己的用户名优先
    let web = host(&plan, "web-1");
    let profile = &plan.profiles[web.profile().unwrap()];
    assert_eq!(
        (profile.name.as_str(), profile.username.as_str()),
        ("运维", "ops")
    );
    assert_eq!(profile.auth, PlannedProfileAuth::Password);
    assert_eq!(web.host.username, "web");
    assert_eq!(web.endpoint().username(), "web");
    assert!(web.hints.contains(&Hint::TotpPromptIgnored));
    let db = host(&plan, "db-1");
    assert_eq!(db.host.username, "");
    assert_eq!(db.endpoint().username(), "deploy", "判断重复按档案的用户名");
    assert_eq!(db.host.port, 2222);
    // 不加密的私钥文件存进密钥库
    let deploy = &plan.profiles[db.profile().unwrap()];
    let key = deploy.key().expect("私钥进密钥库");
    assert_eq!(plan.keys[key].algorithm, KeyAlgorithm::Ed25519);
    assert_eq!(plan.keys[key].name, "deploy");
    // 加密的私钥文件按路径引用
    let inner = host(&plan, "inner");
    let locked = &plan.profiles[inner.profile().unwrap()];
    assert!(
        matches!(&locked.auth, PlannedProfileAuth::KeyFile { path } if path.ends_with("aZ3locked"))
    );
    assert!(inner.hints.contains(&Hint::EncryptedKeyFile));
    // 私钥文件不存在
    let broken = host(&plan, "broken-key");
    assert_eq!(
        plan.profiles[broken.profile().unwrap()].auth,
        PlannedProfileAuth::Password
    );
    assert!(broken.hints.contains(&Hint::LibraryKeyMissing));
    assert!(
        broken.hints.contains(&Hint::JumpNotFound),
        "跳板是已删除的主机"
    );
    assert!(broken.jump.is_none());
    // SSH Agent 凭据；HTTP 代理
    let http = host(&plan, "via-http");
    assert_eq!(
        plan.profiles[http.profile().unwrap()].auth,
        PlannedProfileAuth::Agent
    );
    let proxy = http.host.proxy.as_ref().unwrap();
    assert_eq!(
        (proxy.kind, proxy.address.as_str(), proxy.port),
        (ProxyKind::Http, "proxy.example", 3128)
    );
    assert!(
        !http.hints.contains(&Hint::ProxyPasswordRequired),
        "没有用户名"
    );
    // 同一个凭据只导入一个档案（Telnet 主机用的不算）
    assert_eq!(plan.profiles.len(), 5);
    let switch = host(&plan, "switch");
    assert!(switch.profile().is_none(), "Telnet 不用档案");

    // 无密码：ssh-agent 并提醒；SOCKS5 代理要密码
    let agent = host(&plan, "agent-host");
    assert_eq!(agent.host.auth, Auth::Agent);
    assert_eq!(agent.host.username, "me");
    assert!(agent.hints.contains(&Hint::NoPasswordLogin));
    assert_eq!(agent.host.proxy.as_ref().unwrap().kind, ProxyKind::Socks5);
    assert!(agent.hints.contains(&Hint::ProxyPasswordRequired));

    // 跳板：一级；多级且与跳板机自己的设置一致（db-1 自己经 bastion）；多级而不一致
    let multi_hop = |h: &PlannedHost| {
        h.hints
            .iter()
            .any(|h| matches!(h, Hint::MultiHopJump { .. }))
    };
    assert_eq!(db.jump.as_ref().unwrap().name, "bastion");
    assert!(!multi_hop(db));
    assert_eq!(web.jump.as_ref().unwrap().name, "db-1");
    assert!(!multi_hop(web), "{:?}", web.hints);
    assert_eq!(inner.jump.as_ref().unwrap().name, "web-1");
    assert!(
        inner.hints.contains(&Hint::MultiHopJump {
            chain: vec!["bastion".into(), "web-1".into()]
        }),
        "web-1 自己经 bastion → db-1，不是只经 bastion：{:?}",
        inner.hints
    );
    // 跳板的终点按跳板机实际的用户名
    let ep = &db.jump.as_ref().unwrap().endpoint;
    assert_eq!(
        (ep.address(), ep.port(), ep.username()),
        ("bastion.example", 22, "jump")
    );
    assert_eq!(
        plan.hosts[web.jump.as_ref().unwrap().host.unwrap()]
            .host
            .name,
        "db-1"
    );

    // Telnet：用密码凭据时展开用户名；不自动登录时不带用户名；自定义的提示没有导入
    assert_eq!(switch.host.protocol, Protocol::Telnet);
    assert_eq!(switch.host.port, 23);
    assert_eq!(switch.host.username, "ops");
    assert!(switch.hints.contains(&Hint::TelnetPromptsIgnored));
    assert_eq!(switch.jump.as_ref().unwrap().name, "bastion");
    let console = host(&plan, "console");
    assert_eq!(console.host.username, "");
    assert!(console.hints.is_empty());

    // 串口
    let com = host(&plan, "COM3");
    assert_eq!(com.host.protocol, Protocol::Serial);
    assert_eq!(com.host.port, 0);
    let s = &com.host.serial;
    assert_eq!(
        (s.baud, s.data_bits, s.parity, s.stop_bits, s.flow_control),
        (9600, 7, Parity::Even, StopBits::Two, FlowControl::RtsCts)
    );

    // 转发：只加在 SSH 主机上；网卡地址的监听改为回环地址
    assert_eq!(db.forwards.len(), 3);
    let mysql = &db.forwards[0];
    assert_eq!(
        (
            mysql.kind,
            mysql.name.as_str(),
            mysql.bind_port,
            mysql.target_host.as_str(),
            mysql.auto_start
        ),
        (ForwardKind::Local, "mysql", 3306, "db.internal", true)
    );
    assert_eq!(
        (db.forwards[1].kind, db.forwards[1].bind_host.as_str()),
        (ForwardKind::Dynamic, "127.0.0.1")
    );
    assert!(db.hints.contains(&Hint::ForwardBindLoopback {
        from: "192.168.1.10".into()
    }));
    assert_eq!(
        (db.forwards[2].kind, db.forwards[2].bind_host.as_str()),
        (ForwardKind::Remote, "0.0.0.0")
    );
    assert!(switch.forwards.is_empty());

    // 片段：分类在前，嵌套的合成“上级 / 下级”；点击时执行的末尾补换行
    let snippets: Vec<(&str, &str, &str)> = plan
        .snippets
        .iter()
        .map(|s| (s.group.as_str(), s.name.as_str(), s.content.as_str()))
        .collect();
    assert_eq!(
        snippets,
        [
            ("运维 / K8s", "pods", "kubectl get pods -n {{ns}}\n"),
            ("运维", "disk", "df -h"),
            ("", "uptime", "uptime\n"),
        ]
    );
    assert_eq!(plan.snippet_notes.literal_params, 1);
    assert_eq!(plan.snippet_notes.nested_groups, 1);
}

#[test]
fn older_and_newer_schemas() {
    // 0.1.16：还没有凭据用户名、验证码、片段的嵌套与顺序
    let dir = tempfile::tempdir().unwrap();
    let conn = create_database(dir.path(), 20);
    conn.execute_batch(
        "INSERT INTO credentials (id, keychain_id, name, kind, username) VALUES (1, 'c', 'ops', 'agent', 'ops');
         INSERT INTO hosts (id, public_id, name, address, port, auth, username, credential_id) VALUES
            (1, 'p1', 'a', 'a.example', 22, 'credential', NULL, 1);
         INSERT INTO snippet_categories (id, name) VALUES (1, 'x');
         INSERT INTO snippets (category_id, name, command) VALUES (1, 's', 'ls');",
    )
    .unwrap();
    drop(conn);
    let plan = read(dir.path(), &env(), None).unwrap();
    let a = host(&plan, "a");
    assert_eq!(a.host.username, "");
    assert_eq!(plan.profiles[a.profile().unwrap()].username, "ops");
    assert_eq!(plan.snippets[0].group, "x");

    // 主分支：登录后执行的命令
    let dir = tempfile::tempdir().unwrap();
    let conn = create_database(dir.path(), 25);
    conn.execute_batch(
        "INSERT INTO hosts (id, public_id, name, address, port, auth, username, login_commands) VALUES
            (1, 'p1', 'a', 'a.example', 22, 'password', 'root', 'sudo -i\n\ncd /srv\n');",
    )
    .unwrap();
    drop(conn);
    let plan = read(dir.path(), &env(), None).unwrap();
    assert_eq!(host(&plan, "a").host.terminal.startup, "sudo -i\ncd /srv");
}

#[test]
fn reads_while_shellrs_has_the_database_open() {
    let dir = tempfile::tempdir().unwrap();
    let conn = create_database(dir.path(), 24);
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0;")
        .unwrap();
    conn.execute(
        "INSERT INTO hosts (id, public_id, name, address, port, auth, username) VALUES
            (1, 'p1', 'fresh', 'fresh.example', 22, 'password', 'root')",
        [],
    )
    .unwrap();
    // 连接还开着，修改只在 WAL 中
    let plan = read(&dir.path().join(db::DATABASE_FILE), &env(), None).unwrap();
    assert_eq!(names(&plan), ["fresh"]);
    drop(conn);
}

#[test]
fn settings_point_to_the_sync_folder() {
    let dir = tempfile::tempdir().unwrap();
    drop(create_database(&dir.path().join("data"), 24));
    let sync = dir.path().join("sync");
    writer::vault(&sync, PASSWORD);
    let settings = |location: Value| {
        std::fs::write(
            dir.path().join("data").join("settings.json"),
            json!({"sync": {"location": location}}).to_string(),
        )
        .unwrap();
        read(&dir.path().join("data"), &env(), None).unwrap().notes
    };
    let notes = settings(json!({"kind": "folder", "path": sync}));
    assert!(notes.contains(&PlanNote::SyncFolderAvailable { path: sync.clone() }));
    assert!(notes.contains(&PlanNote::SecretsInKeychain));
    let notes =
        settings(json!({"kind": "webdav", "url": "https://dav.example/shellrs", "username": "me"}));
    assert!(notes.contains(&PlanNote::SyncFolderRemote));
    // 文件夹中没有同步数据时不提示
    let notes = settings(json!({"kind": "folder", "path": dir.path().join("nothing")}));
    assert_eq!(notes, [PlanNote::SecretsInKeychain]);
}

#[test]
fn not_shellrs() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(
        read(dir.path(), &env(), None),
        Err(ImportError::NotShellRs(_))
    ));
    let other = dir.path().join("other.db");
    Connection::open(&other)
        .unwrap()
        .execute_batch("CREATE TABLE t (x)")
        .unwrap();
    assert!(matches!(
        read(&other, &env(), None),
        Err(ImportError::NotShellRs(_))
    ));
}

#[test]
fn default_path_is_the_data_directory() {
    let dir = tempfile::tempdir().unwrap();
    let mut env = env();
    env.data_dir = Some(dir.path().to_path_buf());
    assert_eq!(default_path(&env), None);
    drop(create_database(&dir.path().join("shellrs"), 24));
    assert_eq!(default_path(&env), Some(dir.path().join("shellrs")));
    let portable = tempfile::tempdir().unwrap();
    drop(create_database(portable.path(), 24));
    env.shellrs_dir = Some(portable.path().to_path_buf());
    assert_eq!(default_path(&env), Some(portable.path().to_path_buf()));
}

// ---------------------------------------------------------------- 同步文件夹

/// 一条记录。
fn record(kind: &str, id: &str, ms: u64, data: Value) -> Value {
    json!({"kind": kind, "id": id, "v": [ms, 0, "aaaaaaaaaaaaaaaa"], "data": data})
}

fn snapshot(device: &str, records: Vec<Value>) -> Value {
    json!({
        "format": 1, "min_reader": 1,
        "device": {"id": device, "name": "PC", "platform": "windows", "app": "0.1.19"},
        "clock": [1, 0], "purged_before": 0, "records": records,
    })
}

const DEVICE: &str = "aaaaaaaaaaaaaaaa";
const OTHER: &str = "bbbbbbbbbbbbbbbb";

fn sync_folder(records: Vec<Value>) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let vault = writer::vault(dir.path(), PASSWORD);
    writer::device(dir.path(), &vault, DEVICE, &snapshot(DEVICE, records));
    dir
}

fn sync_plan(dir: &Path) -> ImportPlan {
    read(dir, &env(), Some(&SecretString::new(PASSWORD))).unwrap()
}

fn ssh(name: &str, address: &str, extra: Value) -> Value {
    let mut host = json!({
        "name": name, "address": address, "port": 22, "group": null, "sort_order": 0, "protocol": "ssh",
        "auth": "password", "username": "root", "credential": null, "route": "direct", "notes": "",
    });
    for (k, v) in extra.as_object().unwrap() {
        host[k] = v.clone();
    }
    host
}

#[test]
fn files_written_by_shellrs() {
    // ShellRS 仓库的 golden 文件：同步密码见 testdata/shellrs/LICENSE
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("shellrs-sync").join("v1");
    std::fs::create_dir_all(store.join("devices")).unwrap();
    let testdata = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/import/testdata/shellrs");
    std::fs::copy(testdata.join("vault.json"), store.join("vault.json")).unwrap();
    std::fs::copy(
        testdata.join("k3jq8x0m2pa7d4vn.srs"),
        store.join("devices").join("k3jq8x0m2pa7d4vn.srs"),
    )
    .unwrap();
    let password = SecretString::new("correct horse battery staple");
    let plan = read(dir.path(), &env(), Some(&password)).unwrap();
    assert_eq!(
        names(&plan),
        ["console", "switch", "web"],
        "删除了的 gone 不在"
    );
    assert!(plan.notes.contains(&PlanNote::SyncSnapshot));

    let web = host(&plan, "web");
    assert_eq!(exposed(web.password()), Some("test password"));
    let switch = host(&plan, "switch");
    assert_eq!(switch.host.protocol, Protocol::Telnet);
    assert_eq!(switch.host.username, "deploy", "Telnet 展开凭据的用户名");
    assert!(!switch.has_password(), "密钥凭据没有密码");
    assert!(switch.hints.contains(&Hint::TelnetPromptsIgnored));
    assert!(
        switch.hints.contains(&Hint::JumpNotFound),
        "第二跳是已删除的主机"
    );
    let console = host(&plan, "console");
    let proxy = console.host.proxy.as_ref().unwrap();
    assert_eq!(
        (
            proxy.kind,
            proxy.address.as_str(),
            proxy.port,
            proxy.username.as_str()
        ),
        (ProxyKind::Socks5, "proxy", 1080, "me")
    );
    assert_eq!(
        exposed(console.proxy_password()),
        Some("test proxy password")
    );

    // 转发 f1、f2 经由 web
    assert_eq!(web.forwards.len(), 2);
    assert_eq!(web.forwards[0].name, "db");
    assert_eq!(web.forwards[1].kind, ForwardKind::Dynamic);
    // 片段：s0 分类中的 disk 点击时执行
    let snippets: Vec<(&str, &str, &str)> = plan
        .snippets
        .iter()
        .map(|s| (s.group.as_str(), s.name.as_str(), s.content.as_str()))
        .collect();
    assert_eq!(
        snippets,
        [("Ops", "disk", "df -h\n"), ("", "uptime", "uptime")]
    );
}

#[test]
fn asks_for_the_password() {
    let dir = sync_folder(vec![record(
        "host",
        "h1",
        1,
        ssh("a", "a.example", json!({})),
    )]);
    assert!(matches!(
        read(dir.path(), &env(), None),
        Err(ImportError::PasswordRequired)
    ));
    assert!(matches!(
        read(dir.path(), &env(), Some(&SecretString::new("wrong"))),
        Err(ImportError::WrongPassword)
    ));
    assert_eq!(names(&sync_plan(dir.path())), ["a"]);
}

#[test]
fn the_password_is_normalized() {
    // 建库时是合成的“é”，输入的是分解的写法
    let dir = tempfile::tempdir().unwrap();
    let vault = writer::vault(dir.path(), "caf\u{e9} password");
    writer::device(
        dir.path(),
        &vault,
        DEVICE,
        &snapshot(
            DEVICE,
            vec![record("host", "h1", 1, ssh("a", "a.example", json!({})))],
        ),
    );
    let plan = read(
        dir.path(),
        &env(),
        Some(&SecretString::new("cafe\u{301} password")),
    );
    assert_eq!(names(&plan.unwrap()), ["a"]);
}

#[test]
fn finds_the_store_from_any_level() {
    let dir = sync_folder(vec![record(
        "host",
        "h1",
        1,
        ssh("a", "a.example", json!({})),
    )]);
    let store = dir.path().join("shellrs-sync");
    for path in [
        dir.path().to_path_buf(),
        store.clone(),
        store.join("v1"),
        store.join("v1").join("vault.json"),
    ] {
        assert_eq!(names(&sync_plan(&path)), ["a"], "{}", path.display());
    }
}

#[test]
fn devices_are_merged_by_version() {
    let dir = tempfile::tempdir().unwrap();
    let vault = writer::vault(dir.path(), PASSWORD);
    let mut newer = record("host", "h1", 20, ssh("renamed", "a.example", json!({})));
    newer["v"] = json!([20, 0, OTHER]);
    let mut deleted = record("host", "h2", 30, Value::Null);
    deleted["deleted"] = json!(true);
    deleted["v"] = json!([30, 0, OTHER]);
    let mut old_delete = record("host", "h3", 1, Value::Null);
    old_delete["deleted"] = json!(true);
    old_delete["v"] = json!([1, 0, OTHER]);
    writer::device(
        dir.path(),
        &vault,
        DEVICE,
        &snapshot(
            DEVICE,
            vec![
                record("host", "h1", 10, ssh("old-name", "a.example", json!({}))),
                record("host", "h2", 10, ssh("b", "b.example", json!({}))),
                record("host", "h3", 10, ssh("c", "c.example", json!({}))),
                // 不认识的种类与字段原样忽略
                record("tag", "t1", 10, json!({"name": "x"})),
                record(
                    "host",
                    "h4",
                    10,
                    ssh("d", "d.example", json!({"future": 1})),
                ),
            ],
        ),
    );
    writer::device(
        dir.path(),
        &vault,
        OTHER,
        &snapshot(OTHER, vec![newer, deleted, old_delete]),
    );
    let plan = sync_plan(dir.path());
    let mut found = names(&plan);
    found.sort();
    assert_eq!(found, ["c", "d", "renamed"]);
}

#[test]
fn newer_formats_and_strange_files() {
    let dir = tempfile::tempdir().unwrap();
    let vault = writer::vault(dir.path(), PASSWORD);
    let mut future = snapshot(
        OTHER,
        vec![record("host", "h9", 1, ssh("z", "z.example", json!({})))],
    );
    future["min_reader"] = json!(2);
    writer::device(dir.path(), &vault, OTHER, &future);
    // 只有更新的版本写的快照：读不了
    assert!(matches!(
        read(dir.path(), &env(), Some(&SecretString::new(PASSWORD))),
        Err(ImportError::ShellRsTooNew)
    ));

    let mut gated = record("host", "h2", 1, ssh("gated", "g.example", json!({})));
    gated["min_reader"] = json!(2);
    writer::device(
        dir.path(),
        &vault,
        DEVICE,
        &snapshot(
            DEVICE,
            vec![
                record("host", "h1", 1, ssh("a", "a.example", json!({}))),
                gated,
            ],
        ),
    );
    let devices = dir.path().join("shellrs-sync/v1/devices");
    // 网盘的冲突副本、改了名的文件（附加数据不符，解不开）
    std::fs::copy(
        devices.join(format!("{DEVICE}.srs")),
        devices.join(format!("{DEVICE} (1).srs")),
    )
    .unwrap();
    std::fs::copy(
        devices.join(format!("{DEVICE}.srs")),
        devices.join("cccccccccccccccc.srs"),
    )
    .unwrap();
    let plan = sync_plan(dir.path());
    assert_eq!(names(&plan), ["a"]);
    assert_eq!(plan.skipped, 2, "改了名的与更新版本的设备文件");

    let vault_file = dir.path().join("shellrs-sync/v1/vault.json");
    let mut value: Value =
        serde_json::from_str(&std::fs::read_to_string(&vault_file).unwrap()).unwrap();
    value["format"] = json!(2);
    std::fs::write(&vault_file, value.to_string()).unwrap();
    assert!(matches!(
        read(dir.path(), &env(), Some(&SecretString::new(PASSWORD))),
        Err(ImportError::ShellRsTooNew)
    ));
    value["format"] = json!(1);
    value["kdf"]["memory_kib"] = json!(1024 * 1024);
    std::fs::write(&vault_file, value.to_string()).unwrap();
    assert!(matches!(
        read(dir.path(), &env(), Some(&SecretString::new(PASSWORD))),
        Err(ImportError::NotShellRs(_))
    ));
}

#[test]
fn secrets_from_the_sync_folder() {
    let key = crate::sshkey::generate(crate::sshkey::KeyKind::Ed25519, "work")
        .unwrap()
        .to_openssh()
        .unwrap();
    let dir = sync_folder(vec![
        record(
            "group",
            "g1",
            1,
            json!({"name": "生产", "parent": "missing", "sort_order": 0}),
        ),
        record(
            "credential",
            "c1",
            1,
            json!({"name": "ops", "kind": "password", "username": "ops", "sort_order": 0}),
        ),
        record(
            "credential",
            "c2",
            1,
            json!({"name": "work", "kind": "key", "username": "deploy", "sort_order": 0}),
        ),
        record(
            "credential",
            "c3",
            1,
            json!({"name": "ops", "kind": "agent", "username": "ops", "sort_order": 1}),
        ),
        record(
            "host",
            "h1",
            1,
            ssh(
                "pw",
                "Pw.Example",
                json!({"group": "g1",
            "totp": {"prompt": null}}),
            ),
        ),
        record(
            "host",
            "h2",
            1,
            ssh(
                "prof",
                "p.example",
                json!({"auth": "credential", "username": null,
            "credential": "c1", "credential_username": "alice"}),
            ),
        ),
        record(
            "host",
            "h3",
            1,
            ssh(
                "key",
                "k.example",
                json!({"auth": "credential", "username": null,
            "credential": "c2"}),
            ),
        ),
        record(
            "host",
            "h4",
            1,
            ssh(
                "key2",
                "k2.example",
                json!({"auth": "credential", "username": null,
            "credential": "c2"}),
            ),
        ),
        record(
            "host",
            "h5",
            1,
            ssh(
                "agent",
                "ag.example",
                json!({"auth": "credential", "username": null,
            "credential": "c3"}),
            ),
        ),
        record(
            "host",
            "h6",
            1,
            ssh(
                "gone-cred",
                "gc.example",
                json!({"auth": "credential",
            "username": null, "credential": "c9"}),
            ),
        ),
        record(
            "host",
            "h7",
            1,
            json!({"name": "tel", "address": "t.example", "port": 23, "group": null,
            "sort_order": 0, "protocol": "telnet", "telnet": {"user_prompt": null, "pass_prompt": null},
            "auth": "credential", "username": null, "credential": "c1", "route": "direct", "notes": ""}),
        ),
        record(
            "forward",
            "f1",
            1,
            json!({"host": "gone", "name": "x", "kind": "local", "bind_host": "127.0.0.1",
            "bind_port": 1, "target_host": "y", "target_port": 2, "sort_order": 0}),
        ),
        record(
            "snippet",
            "s1",
            1,
            json!({"category": "missing", "name": "ls", "command": "ls",
            "scope": "all", "scope_id": null, "run_on_click": false}),
        ),
        record(
            "secret.password",
            "x1",
            1,
            json!({"user": "root", "host": "pw.example", "port": 22, "value": "pw-secret"}),
        ),
        record("secret.credential", "c1", 1, json!({"value": "ops-secret"})),
        record(
            "secret.private_key",
            "c2",
            1,
            json!({"value": key.expose()}),
        ),
        record("secret.totp", "h1", 1, json!({"value": "JBSWY3DPEHPK3PXP"})),
    ]);
    let plan = sync_plan(dir.path());
    assert!(plan.has_secrets());

    let pw = host(&plan, "pw");
    assert_eq!(
        exposed(pw.password()),
        Some("pw-secret"),
        "地址不区分大小写"
    );
    assert_eq!(exposed(pw.totp()), Some("JBSWY3DPEHPK3PXP"));
    assert_eq!(group(&plan, pw), ["生产"], "上级不存在的到最外层");

    // 名称、用户名相同的两条凭据：先用到的是“ops”（主机按名称，agent 在 prof 前），后一条加序号
    let agent = &plan.profiles[host(&plan, "agent").profile().unwrap()];
    assert_eq!(
        (agent.name.as_str(), agent.auth.clone()),
        ("ops", PlannedProfileAuth::Agent)
    );
    let prof = host(&plan, "prof");
    assert_eq!(prof.host.username, "alice");
    let ops = &plan.profiles[prof.profile().unwrap()];
    assert_eq!(ops.name, "ops (2)");
    assert_eq!(exposed(ops.password()), Some("ops-secret"));
    assert!(ops.has_password());

    // 同一把私钥只存一次，两台主机共用一个档案
    let key1 = host(&plan, "key");
    let key2 = host(&plan, "key2");
    assert_eq!(key1.profile(), key2.profile());
    assert_eq!(plan.keys.len(), 1);
    assert_eq!(plan.profiles[key1.profile().unwrap()].key(), Some(0));

    // 凭据不存在：密码登录、没有用户名
    let gone = host(&plan, "gone-cred");
    assert!(gone.profile().is_none());
    assert_eq!(
        (gone.host.auth.clone(), gone.host.username.as_str()),
        (Auth::Password, "")
    );

    // Telnet 展开密码凭据
    let tel = host(&plan, "tel");
    assert_eq!(tel.host.username, "ops");
    assert_eq!(exposed(tel.password()), Some("ops-secret"));
    assert!(tel.hints.is_empty());

    // 片段的分类不存在：最外层；转发的主机不存在：不导入
    assert_eq!(plan.snippets[0].group, "");
    assert!(plan.hosts.iter().all(|h| h.forwards.is_empty()));

    // 秘密不出现在 Debug 中；不同意导入秘密时全部去掉
    let debug = format!("{plan:?}");
    for secret in ["pw-secret", "ops-secret", "JBSWY3DPEHPK3PXP", "OPENSSH"] {
        assert!(!debug.contains(secret), "{secret}");
    }
    assert!(!plan.without_secrets().has_secrets());
}

#[test]
fn imports_into_the_store() {
    use crate::backup::testkit::Machine;
    use crate::import::{OnDuplicate, apply};
    use crate::model::{Forward, KeySource, Profile, ProfileAuth, SecretField, Snippet};
    use crate::store::Owner;
    use crate::testkit::block_on;

    let key = crate::sshkey::generate(crate::sshkey::KeyKind::Ed25519, "work")
        .unwrap()
        .to_openssh()
        .unwrap();
    let dir = sync_folder(vec![
        record(
            "credential",
            "c1",
            1,
            json!({"name": "ops", "kind": "password", "username": "ops", "sort_order": 0}),
        ),
        record(
            "credential",
            "c2",
            1,
            json!({"name": "work", "kind": "key", "username": "deploy", "sort_order": 0}),
        ),
        record(
            "host",
            "h1",
            1,
            ssh(
                "web",
                "web.example",
                json!({"auth": "credential", "username": null,
            "credential": "c1", "totp": {"prompt": null}}),
            ),
        ),
        record(
            "host",
            "h2",
            1,
            ssh(
                "db",
                "db.example",
                json!({"auth": "credential", "username": null,
            "credential": "c2", "route": "jump", "jumps": ["h1"]}),
            ),
        ),
        record(
            "host",
            "h3",
            1,
            json!({"name": "tel", "address": "t.example", "port": 23, "group": null,
            "sort_order": 0, "protocol": "telnet", "auth": "credential", "username": null, "credential": "c1",
            "route": "direct", "notes": ""}),
        ),
        record(
            "forward",
            "f1",
            1,
            json!({"host": "h2", "name": "pg", "kind": "local", "bind_host": "127.0.0.1",
            "bind_port": 15432, "target_host": "localhost", "target_port": 5432, "sort_order": 0}),
        ),
        record(
            "snippet",
            "s1",
            1,
            json!({"category": null, "name": "up", "command": "uptime",
            "scope": "all", "scope_id": null, "run_on_click": true}),
        ),
        record("secret.credential", "c1", 1, json!({"value": "ops-secret"})),
        record(
            "secret.private_key",
            "c2",
            1,
            json!({"value": key.expose()}),
        ),
        record("secret.totp", "h1", 1, json!({"value": "JBSWY3DPEHPK3PXP"})),
    ]);
    let plan = sync_plan(dir.path());
    let m = Machine::new();
    let keys: Vec<String> = plan.hosts.iter().map(|h| h.key.clone()).collect();
    let report = block_on(apply(m.backups.clone(), plan, keys, OnDuplicate::Skip)).unwrap();
    assert_eq!(
        (report.created, report.profiles_created, report.snippets),
        (3, 2, 1)
    );
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    let secret = |owner: &Owner, field| {
        m.store
            .secret(owner, field)
            .unwrap()
            .map(|s| s.expose().to_string())
    };
    let web = m.host_named("web");
    let Auth::Profile { profile: ops } = web.doc.auth.clone() else {
        panic!("{:?}", web.doc.auth)
    };
    assert_eq!(
        secret(&Owner::profile(&ops), SecretField::Password).as_deref(),
        Some("ops-secret")
    );
    assert_eq!(
        secret(&Owner::host(&web.id), SecretField::Totp).as_deref(),
        Some("JBSWY3DPEHPK3PXP")
    );
    let db = m.host_named("db");
    let Auth::Profile { profile: work } = db.doc.auth.clone() else {
        panic!("{:?}", db.doc.auth)
    };
    let work = m.store.get::<Profile>(&work).unwrap().doc;
    assert_eq!(work.username, "deploy");
    assert!(matches!(
        work.auth,
        ProfileAuth::Key {
            source: KeySource::Library { .. }
        }
    ));
    assert_eq!(
        db.doc.jump,
        crate::model::Jump::Via {
            host: web.id.clone()
        }
    );
    let forwards = m.store.list::<Forward>();
    assert_eq!(forwards.len(), 1);
    assert_eq!(forwards[0].doc.host.as_ref(), Some(&db.id));
    assert!(!forwards[0].doc.auto_start);
    let tel = m.host_named("tel");
    assert_eq!(
        (tel.doc.protocol, tel.doc.username.as_str()),
        (Protocol::Telnet, "ops")
    );
    assert_eq!(
        secret(&Owner::host(&tel.id), SecretField::Password).as_deref(),
        Some("ops-secret")
    );
    assert_eq!(m.store.list::<Snippet>()[0].doc.content, "uptime\n");
}

/// 实机截图用的样例（默认不跑）：在环境变量 `TERMLET_SHELLRS_SAMPLES` 指定的目录中写 `data`（数据目录，其中的
/// `settings.json` 指向 `sync`）与 `sync`（同步文件夹，同步密码是 [`PASSWORD`]）。
#[test]
#[ignore]
fn write_samples() {
    let root = PathBuf::from(std::env::var_os("TERMLET_SHELLRS_SAMPLES").expect("样例目录"));
    let data = root.join("data");
    let sync = root.join("sync");
    let conn = create_database(&data, 25);
    fill(&conn, &data);
    drop(conn);
    std::fs::write(
        data.join("settings.json"),
        json!({"sync": {"location": {"kind": "folder", "path": sync}}}).to_string(),
    )
    .unwrap();
    let key = crate::sshkey::generate(crate::sshkey::KeyKind::Ed25519, "deploy")
        .unwrap()
        .to_openssh()
        .unwrap();
    let vault = writer::vault(&sync, PASSWORD);
    writer::device(
        &sync,
        &vault,
        DEVICE,
        &snapshot(
            DEVICE,
            vec![
                record(
                    "group",
                    "g1",
                    1,
                    json!({"name": "生产", "parent": null, "sort_order": 0}),
                ),
                record(
                    "group",
                    "g2",
                    1,
                    json!({"name": "数据库", "parent": "g1", "sort_order": 0}),
                ),
                record(
                    "credential",
                    "c1",
                    1,
                    json!({"name": "运维", "kind": "password",
                    "username": "ops", "sort_order": 0}),
                ),
                record(
                    "credential",
                    "c2",
                    1,
                    json!({"name": "deploy", "kind": "key",
                    "username": "deploy", "sort_order": 1}),
                ),
                record(
                    "host",
                    "h1",
                    1,
                    ssh(
                        "bastion",
                        "bastion.example.com",
                        json!({"group": "g1",
                    "username": "jump", "totp": {"prompt": null}}),
                    ),
                ),
                record(
                    "host",
                    "h2",
                    1,
                    ssh(
                        "db-1",
                        "10.0.1.5",
                        json!({"group": "g2", "auth": "credential",
                    "username": null, "credential": "c2", "route": "jump", "jumps": ["h1"]}),
                    ),
                ),
                record(
                    "host",
                    "h3",
                    1,
                    ssh(
                        "web-1",
                        "10.0.1.6",
                        json!({"group": "g1",
                    "auth": "credential", "username": null, "credential": "c1", "credential_username": "www",
                    "login_commands": "sudo -i\ncd /srv/www"}),
                    ),
                ),
                record(
                    "host",
                    "h4",
                    1,
                    ssh(
                        "outside",
                        "203.0.113.7",
                        json!({"route": "proxy",
                    "proxy": {"kind": "socks5", "host": "127.0.0.1", "port": 1080, "username": "me"}}),
                    ),
                ),
                record(
                    "host",
                    "h5",
                    1,
                    json!({"name": "switch", "address": "10.0.2.1", "port": 23,
                    "group": "g1", "sort_order": 9, "protocol": "telnet",
                    "telnet": {"user_prompt": "Username:", "pass_prompt": null}, "auth": "credential",
                    "username": null, "credential": "c1", "route": "direct", "notes": "机房交换机"}),
                ),
                record(
                    "forward",
                    "f1",
                    1,
                    json!({"host": "h2", "name": "mysql", "kind": "local",
                    "bind_host": "127.0.0.1", "bind_port": 13306, "target_host": "127.0.0.1",
                    "target_port": 3306, "sort_order": 0}),
                ),
                record(
                    "snippet_category",
                    "s0",
                    1,
                    json!({"name": "运维", "parent": null, "sort_order": 0}),
                ),
                record(
                    "snippet",
                    "s1",
                    1,
                    json!({"category": "s0", "name": "磁盘", "command": "df -h",
                    "scope": "all", "scope_id": null, "run_on_click": true, "sort_order": 0}),
                ),
                record(
                    "secret.password",
                    "x1",
                    1,
                    json!({"user": "jump", "host": "bastion.example.com",
                    "port": 22, "value": "bastion-pw"}),
                ),
                record(
                    "secret.proxy",
                    "x2",
                    1,
                    json!({"user": "me", "host": "127.0.0.1", "port": 1080,
                    "value": "proxy-pw"}),
                ),
                record("secret.credential", "c1", 1, json!({"value": "ops-pw"})),
                record(
                    "secret.private_key",
                    "c2",
                    1,
                    json!({"value": key.expose()}),
                ),
                record("secret.totp", "h1", 1, json!({"value": "JBSWY3DPEHPK3PXP"})),
            ],
        ),
    );
}
