//! 诊断信息与脱敏的测试。

use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde_json::{Map, Value, json};

use super::*;
use crate::db::Database;
use crate::model::{HighlightRule, Host, LocalShell};
use crate::store::{Position, Store};
use crate::vault::{MemoryKeyStore, Vault};

// ---------------------------------------------------------------- 脱敏

#[test]
fn scrubber_text() {
    let s = Scrubber::new(r"C:\Users\Alice", "Alice");
    let cases = [
        (
            r"C:\Users\Alice\AppData\Roaming\termlet",
            r"~\AppData\Roaming\termlet",
        ),
        ("c:/users/alice/.ssh/config", "~/.ssh/config"),
        (
            r#""dir":"C:\\Users\\Alice\\AppData\\termlet""#,
            r#""dir":"~\\AppData\\termlet""#,
        ),
        (r"C:\Users\Alicebob\x", r"C:\Users\Alicebob\x"),
        (
            "dial tcp 10.1.2.3:22: i/o timeout",
            "dial tcp <ip>:22: i/o timeout",
        ),
        ("listen tcp 127.0.0.1:8080", "listen tcp 127.0.0.1:8080"),
        (
            "HTTP_PROXY=http://bob:secret@proxy.corp:3128",
            "HTTP_PROXY=<url>",
        ),
        (
            r#""err":"Get \"https://x.example.com/a\": EOF""#,
            r#""err":"Get \"<url>\": EOF""#,
        ),
        ("ssh root@db01 failed", "ssh <user>@<host> failed"),
        (
            "golang.org/x/crypto@v0.30.0/ssh/client.go:12",
            "golang.org/x/crypto@v0.30.0/ssh/client.go:12",
        ),
        (
            "lookup db.prod.example.com: no such host",
            "lookup <host>: no such host",
        ),
        (
            "connect fe80:0:0:0:1:2:3:4 and [::1]:22",
            "connect <ip> and <ip>:22",
        ),
        (r"DESKTOP-1\alice logged in", r"DESKTOP-1\<user> logged in"),
        (
            r#""time":"2026-10-14T21:05:00.123+08:00","msg":"x""#,
            r#""time":"2026-10-14T21:05:00.123+08:00","msg":"x""#,
        ),
    ];
    for (input, want) in cases {
        assert_eq!(s.text(input), want, "输入：{input}");
    }

    let unix = Scrubber::new("/home/bob/", "bob");
    assert_eq!(
        unix.text("/home/bob/.config/x /home/bobby/y"),
        "~/.config/x /home/bobby/y"
    );
    // 太短的用户名不替换，避免误伤
    assert_eq!(Scrubber::new("", "li").text("list li"), "list li");
}

#[test]
fn scrubber_ipv6() {
    let s = Scrubber::new("", "");
    let cases = [
        ("dial tcp [2001:db8::1]:22", "dial tcp <ip>:22"),
        ("from 2001:db8::1 port 22", "from <ip> port 22"),
        ("via fe80::1%eth0 ok", "via <ip> ok"),
        ("route 2001:db8::/32", "route <ip>/32"),
        ("peer ::ffff:a00:1 closed", "peer <ip> closed"),
        ("link fe80::2:3:4%enp0s3,", "link <ip>,"),
        ("[fe80::1%25eth0]:22", "<ip>:22"),
        ("2001:db8:0:0:1:0:0:1", "<ip>"),
        ("::1", "<ip>"),
        (
            "at 21:05:00 Foo::Add std::string",
            "at 21:05:00 Foo::Add std::string",
        ),
    ];
    for (input, want) in cases {
        assert_eq!(s.text(input), want, "输入：{input}");
    }
}

/// JSON 只处理字符串：用户名是纯数字（工号）、true、null 时 JSON 仍然合法，数字与布尔值不变；
/// 转义后的内容（`\nalice`）按解码后的文字处理。
#[test]
fn scrubber_json() {
    let cases = [
        (
            "10086",
            r#"{"count": 10086, "owner": "10086", "path": "/home/10086/x"}"#,
            r#"{"count": 10086, "owner": "<user>", "path": "/home/<user>/x"}"#,
        ),
        (
            "true",
            r#"{"ok": true, "name": "true"}"#,
            r#"{"ok": true, "name": "<user>"}"#,
        ),
        ("null", r#"[null, "null"]"#, r#"[null, "<user>"]"#),
        (
            "alice",
            r#"{"msg":"line1\nalice: \"x\" <"}"#,
            r#"{"msg":"line1\n<user>: \"x\" <"}"#,
        ),
        (
            "alice",
            r#"{"alice":1,"ip":"fe80::1%eth0","n":"plain é"}"#,
            r#"{"<user>":1,"ip":"<ip>","n":"plain é"}"#,
        ),
        // 秘密字段的值（键与值是分开的两个字符串）
        (
            "",
            r#"{"user":"a","password":"hunter2","api_key" : 123456,"token":null,"ok":true}"#,
            r#"{"user":"a","password":"<redacted>","api_key" : "<redacted>","token":null,"ok":true}"#,
        ),
        (
            "",
            r#"[{"Secret":"x\"y"},{"name":"password"},"password"]"#,
            r#"[{"Secret":"<redacted>"},{"name":"password"},"password"]"#,
        ),
        // 不是 JSON 时按文本处理
        ("alice", "alice@db01 failed\n", "<user>@<host> failed\n"),
    ];
    for (user, input, want) in cases {
        let got = Scrubber::new("", user).json(input);
        assert_eq!(got, want, "用户名 {user}，输入 {input}");
        if serde_json::from_str::<Value>(input).is_ok() {
            assert!(
                serde_json::from_str::<Value>(&got).is_ok(),
                "脱敏后应仍是合法的 JSON：{got}"
            );
        }
    }
}

/// 按 ASCII 判断词的边界：中文紧挨着地址时也要去掉；Rust 的路径不当作 IPv6。
#[test]
fn ascii_word_boundaries() {
    let s = Scrubber::new("", "");
    assert_eq!(s.text("主机10.0.0.5连接失败"), "主机<ip>连接失败");
    assert_eq!(s.text("用户root@db01登录"), "用户<user>@<host>登录");
    assert_eq!(s.text("经由fe80::1转发"), "经由<ip>转发");
    for keep in [
        "termlet_core::db::Database::open",
        "Add::new",
        "at crates/termlet-core/src/db/mod.rs:12:5",
        "termlet.2026-10-20.log",
        "data.db-wal",
    ] {
        assert_eq!(s.text(keep), keep);
    }
}

/// 日志里万一出现秘密也要去掉。
#[test]
fn scrubber_removes_secrets() {
    let s = Scrubber::new("", "");
    let key = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----";
    assert_eq!(
        s.text(&format!("key: {key} done")),
        "key: <private key> done"
    );
    assert_eq!(
        s.text("password=hunter2 user=x"),
        "password=<redacted> user=x"
    );
    assert_eq!(
        s.text(r#"{"token": "abc123", "n": 1}"#),
        r#"{"token": <redacted>, "n": 1}"#
    );
    assert_eq!(
        s.text("access_token=ghp_xxx&page=2"),
        "access_token=<redacted>&page=2"
    );
    assert_eq!(
        s.text("Authorization: Bearer eyJhbGciOi.payload.sig"),
        "Authorization: <redacted> <redacted>"
    );
    // 设置项的名字不是秘密
    assert_eq!(s.text("passwordFill=true"), "passwordFill=true");
    // JSON 文本中键名为秘密的值在字符串内容之外，按字符串处理时不受影响；JSON 值用 value 处理
    let mut value =
        json!({"password": "hunter2", "nested": {"apiKey": "k", "count": 3}, "ok": "fine"});
    s.value(&mut value);
    assert_eq!(
        value,
        json!({"password": "<redacted>", "nested": {"apiKey": "<redacted>", "count": 3}, "ok": "fine"})
    );
}

// ---------------------------------------------------------------- 崩溃说明与调用栈

fn alice() -> Scrubber {
    Scrubber::new(r"C:\Users\alice", "alice")
}

#[test]
fn message_removes_addresses_users_urls_quotes_and_domains() {
    let msg = alice().message(
        r#"dial tcp 10.1.2.3:22 failed for root@db.example.com via "prod-db.internal" ssh://jump.example.com/x [fe80::1] 2001:db8:0:0:0:0:2:1 api.prod.example.com:8443"#,
    );
    for leak in [
        "10.1.2.3",
        "root",
        "db.example",
        "prod-db",
        "jump",
        "fe80",
        "2001:db8",
        "api.prod",
    ] {
        assert!(!msg.contains(leak), "不应包含 {leak}：{msg}");
    }
    for want in [
        "<ip>:22",
        "<user>@<host>",
        "\"…\"",
        "<url>",
        "<host>:<port>",
    ] {
        assert!(msg.contains(want), "应包含 {want}：{msg}");
    }
}

#[test]
fn message_ipv6_compressed_and_zone() {
    let s = alice();
    assert_eq!(
        s.message("connect to 2001:db8::1 port 22"),
        "connect to <ip> port 22"
    );
    assert_eq!(
        s.message("via fe80::1%eth0 and ::ffff:a00:1"),
        "via <ip> and <ip>"
    );
    assert_eq!(
        s.message("in Foo::Add and std::string"),
        "in Foo::Add and std::string"
    );
}

#[test]
fn message_keeps_identifiers_and_replaces_home_and_user() {
    let msg = alice().message(
        r"Cannot read properties of undefined (reading 'foo') at C:\Users\alice\x.txt by ALICE",
    );
    assert_eq!(
        msg,
        r"Cannot read properties of undefined (reading 'foo') at ~\x.txt by <user>"
    );
}

#[test]
fn message_short_and_non_ascii_usernames() {
    assert_eq!(Scrubber::new("", "li").message("alias li"), "alias li");
    assert_eq!(
        Scrubber::new("", "张三").message("打开 张三 的文件"),
        "打开 <user> 的文件"
    );
}

#[test]
fn message_and_stack_are_truncated() {
    let s = alice();
    assert_eq!(
        s.message(&"x".repeat(2000)).chars().count(),
        MAX_MESSAGE_CHARS + 1
    );
    let stack = s.stack(&"y".repeat(20_000));
    assert!(stack.ends_with("\n…"));
    assert_eq!(stack.chars().count(), MAX_STACK_CHARS + 2);
}

#[test]
fn stack_keeps_functions_locations_and_versions() {
    let stack = [
        "Error: boom",
        "    at load (file:///C:/Users/alice/AppData/Local/Programs/termlet/resources/app.asar/out/main/index.js:12:3)",
        "github.com/termlet/termlet/core/pkg/server.(*Server).handle(0xc000123456)",
        "\tgolang.org/x/crypto@v0.57.0/ssh/client.go:123 +0x1d",
        "   3: termlet_core::db::Database::open",
        r"             at C:\Users\alice\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\tokio-1.47.1\src\runtime.rs:12:5",
    ]
    .join("\n");
    let out = alice().stack(&stack);
    assert!(
        out.contains("file:///~/AppData/Local/Programs/termlet/"),
        "{out}"
    );
    assert!(out.contains("server.(*Server).handle"), "{out}");
    assert!(out.contains("crypto@v0.57.0/ssh/client.go:123"), "{out}");
    assert!(out.contains("termlet_core::db::Database::open"), "{out}");
    assert!(out.contains(r"at ~\.cargo\registry"), "{out}");
    assert!(!out.contains("alice"), "{out}");
}

// ---------------------------------------------------------------- 诊断信息的文本

fn info_with(sections: &[(&str, Value)]) -> Map<String, Value> {
    sections
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

/// 各节与日志都经过脱敏，各节仍是合法的 JSON（diag 的 TestWriteScrubs）。
#[test]
fn render_scrubs_sections_and_logs() {
    let info = info_with(&[
        ("app", json!({"version": "1.0.0"})),
        (
            "runtime",
            json!({"dataDir": r"C:\Users\Alice\AppData\Roaming\termlet", "proxy": "http://u:p@10.0.0.9:8080"}),
        ),
        (
            "client",
            json!({"update": {"file": r"C:\Users\Alice\Downloads\x.exe"}}),
        ),
    ]);
    let log_line = r#"{"time":"2026-10-14T21:05:00+08:00","msg":"重连失败","err":"dial tcp 10.0.0.5:22: timeout","dir":"C:\\Users\\Alice\\AppData"}"#;
    let logs = [LogTail {
        name: "termlet.2026-10-14.log".into(),
        size: 100,
        modified_ms: 0,
        text: format!(
            "{log_line}\n2026-10-14T13:05:00Z  WARN termlet_core::ssh: 连接 10.0.0.6:22 失败 user=Alice\nno newline at end"
        ),
        truncated: false,
    }];
    let text = render(&info, &logs, &Scrubber::new(r"C:\Users\Alice", "alice"));
    assert!(!text.contains("Alice"), "{text}");
    assert!(!text.contains("10.0.0."), "{text}");
    assert!(
        text.contains(r#""dataDir": "~\\AppData\\Roaming\\termlet""#),
        "{text}"
    );
    assert!(text.contains(r#""proxy": "<url>""#), "{text}");
    assert!(text.contains(r#""file": "~\\Downloads\\x.exe""#), "{text}");
    let want_log = r#"{"time":"2026-10-14T21:05:00+08:00","msg":"重连失败","err":"dial tcp <ip>:22: timeout","dir":"~\\AppData"}"#;
    assert!(text.contains(want_log), "JSON 行只处理字符串：{text}");
    assert!(text.contains("连接 <ip>:22 失败 user=<user>"), "{text}");
    assert!(text.ends_with("no newline at end\n"), "{text}");
    // 每一节都是合法的 JSON
    for chunk in text
        .split("\n## ")
        .skip(1)
        .filter(|c| !c.starts_with("日志"))
    {
        let body = chunk.split_once('\n').unwrap().1;
        serde_json::from_str::<Value>(body).unwrap_or_else(|e| panic!("{e}：{body}"));
    }
}

/// 固定的几节在前，其余按名称，界面的信息最后。
#[test]
fn render_orders_sections() {
    let info = info_with(&[
        ("client", json!({})),
        ("zeta", json!(1)),
        ("app", json!({})),
        ("alpha", json!(2)),
        ("system", json!({})),
    ]);
    let text = render(&info, &[], &Scrubber::default());
    let heads: Vec<&str> = text.lines().filter_map(|l| l.strip_prefix("## ")).collect();
    assert_eq!(heads, ["app", "system", "alpha", "zeta", "client"]);
}

/// 用户名是纯数字时数量中同样的数字不被替换（diag 的 TestWriteNumericUser）。
#[test]
fn render_numeric_username() {
    let info = info_with(&[
        ("counts", json!({"hosts": 10086})),
        ("runtime", json!({"dataDir": "/home/10086/.termlet"})),
    ]);
    let text = render(&info, &[], &Scrubber::new("", "10086"));
    assert!(text.contains(r#""hosts": 10086"#), "{text}");
    assert!(
        text.contains(r#""dataDir": "/home/<user>/.termlet""#),
        "{text}"
    );
}

/// 跨行的私钥块整体去掉。
#[test]
fn render_removes_private_key_blocks_in_logs() {
    let logs = [LogTail {
        name: "termlet.log".into(),
        size: 10,
        modified_ms: 0,
        text: "before\n-----BEGIN RSA PRIVATE KEY-----\nMIIEow\nIBAAK\n-----END RSA PRIVATE KEY-----\nafter\n".into(),
        truncated: false,
    }];
    let text = render(&Map::new(), &logs, &Scrubber::default());
    assert!(text.contains("before\n<private key>\nafter\n"), "{text}");
    assert!(!text.contains("MIIEow"));
}

/// 导出时日志的脱敏速度：`cargo test -p termlet-core --release diag::tests::scrub_throughput -- --ignored --nocapture`
#[test]
#[ignore = "吞吐测试，手动运行"]
fn scrub_throughput() {
    let line = "2026-10-19T05:15:01.534671Z  INFO termlet_core::localpty: 本地终端已启动 program=C:\\Users\\Alice\\AppData\\Local\\pwsh.exe pid=Some(27928) peer=10.1.2.3:22\n";
    let text = line.repeat(EXPORT_LOG_BYTES / line.len());
    let logs = [LogTail {
        name: "termlet.log".into(),
        size: text.len() as u64,
        modified_ms: 0,
        text,
        truncated: false,
    }];
    let began = std::time::Instant::now();
    let out = render(
        &Map::new(),
        &logs,
        &Scrubber::new(r"C:\Users\Alice", "Alice"),
    );
    println!("{} MB 用时 {:?}", EXPORT_LOG_BYTES >> 20, began.elapsed());
    assert!(!out.contains("Alice") && !out.contains("10.1.2.3"));
}

// ---------------------------------------------------------------- 日志的末尾（diag 的 TestWrite）

#[test]
fn log_tails_newest_first_within_budget() {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, content: &str, age_secs: u64| {
        let path = dir.path().join(name);
        fs::write(&path, content).unwrap();
        let time = SystemTime::now() - Duration::from_secs(age_secs);
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    };
    write("termlet.2026-10-18.log", &"old line\n".repeat(20), 7200);
    write(
        "termlet.2026-10-19.log",
        "yesterday 1\nyesterday 2\nyesterday 3\n",
        3600,
    );
    write("termlet.2026-10-20.log", "today\n", 0);
    write("notes.txt", "ignored", 0);
    fs::create_dir(dir.path().join("sub.log")).unwrap();

    // 预算：今天的全部（6 字节）+ 昨天的最后 20 字节（从下一行的开头算起）
    let tails = read_log_tails(dir.path(), 26);
    let names: Vec<&str> = tails.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["termlet.2026-10-19.log", "termlet.2026-10-20.log"]);
    assert_eq!(tails[0].text, "yesterday 3\n");
    assert!(tails[0].truncated);
    assert_eq!(tails[1].text, "today\n");
    assert!(!tails[1].truncated);

    // 预算足够时全部取出，按时间从旧到新
    let all = read_log_tails(dir.path(), 1 << 20);
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].name, "termlet.2026-10-18.log");
    assert!(all.iter().all(|t| !t.truncated));
    // 没有日志目录时为空
    assert!(read_log_tails(&dir.path().join("missing"), 100).is_empty());
}

// ---------------------------------------------------------------- 设置摘要

/// 设置摘要不含自定义规则的内容、本地终端的命令行参数与环境变量的值（令牌、带密码的代理地址）、高亮规则的关键字。
#[test]
fn settings_summary_omits_sensitive_content() {
    let mut settings = Settings::default();
    settings.guard.custom_rules = vec!["prod-db-01".into()];
    settings.highlight.rules = vec![
        HighlightRule {
            pattern: "billing-cluster".into(),
            enabled: true,
            ..Default::default()
        },
        HighlightRule {
            pattern: r"ERR\d+".into(),
            regex: true,
            ..Default::default()
        },
    ];
    settings.connection.agent_path = r"\\.\pipe\openssh-ssh-agent-alice".into();
    settings
        .general
        .extra
        .insert("unknownFromNewerVersion".into(), json!("secret-ish"));
    let mut local = LocalSettings::default();
    local.shells.custom = vec![
        LocalShell {
            id: "custom-a".into(),
            name: "dev".into(),
            command: "bash --login".into(),
            dir: "/srv".into(),
            env: BTreeMap::from([
                ("GITHUB_TOKEN".into(), "ghp_secret".into()),
                ("HTTP_PROXY".into(), "http://u:pass@proxy:3128".into()),
            ]),
        },
        LocalShell {
            id: "custom-b".into(),
            name: "pwsh".into(),
            command:
                r#""C:\Program Files\PowerShell\7\pwsh.exe" -NoLogo -c "$env:T='tok_secret'; cd x""#
                    .into(),
            ..Default::default()
        },
    ];
    local.logs.session_log_dir = r"D:\logs\alice".into();

    let summary = settings_summary(&settings, &local);
    let text = serde_json::to_string(&summary).unwrap();
    for leak in [
        "ghp_secret",
        "pass@proxy",
        "prod-db-01",
        "tok_secret",
        "--login",
        "billing-cluster",
        "openssh-ssh-agent",
        r"D:\\logs",
        "unknownFromNewerVersion",
        "/srv",
    ] {
        assert!(!text.contains(leak), "诊断信息不应包含 {leak}：{text}");
    }
    assert_eq!(
        summary["local"]["shells"]["custom"][0]["envNames"],
        json!(["GITHUB_TOKEN", "HTTP_PROXY"])
    );
    assert_eq!(summary["local"]["shells"]["custom"][0]["program"], "bash");
    assert_eq!(summary["local"]["shells"]["custom"][0]["args"], 1);
    assert_eq!(
        summary["local"]["shells"]["custom"][1]["program"],
        "pwsh.exe"
    );
    assert_eq!(summary["local"]["shells"]["custom"][1]["args"], 3);
    assert_eq!(summary["guard"]["customRules"], 1);
    assert_eq!(
        summary["highlight"],
        json!({"enabled": true, "rules": 2, "enabledRules": 1, "regexRules": 1})
    );
    assert_eq!(summary["connection"]["agentPathSet"], true);
    assert_eq!(summary["local"]["logs"]["sessionLogDirSet"], true);
    assert_eq!(summary["local"]["appLock"]["idleMinutes"], 15);
    assert_eq!(summary["terminal"]["fontSize"], settings.terminal.font_size);
    // 不修改传入的设置
    assert_eq!(local.shells.custom[0].env.len(), 2);
    assert_eq!(settings.guard.custom_rules.len(), 1);
}

#[test]
fn command_shape_counts_arguments() {
    assert_eq!(command_shape("bash --login"), ("bash".into(), 1));
    assert_eq!(
        command_shape(r#""C:\Program Files\x\a b.exe""#),
        ("a b.exe".into(), 0)
    );
    assert_eq!(command_shape("  "), (String::new(), 0));
    assert_eq!(command_shape("/usr/bin/zsh -l -i"), ("zsh".into(), 2));
}

#[test]
fn sync_section_has_no_locations_or_accounts() {
    use crate::cloudsync::{SyncConfig, SyncTarget};
    let status = SyncStatus {
        config: Some(SyncConfig {
            target: SyncTarget::WebDav {
                url: "https://dav.example.com/dav/termlet/".into(),
                username: "alice@example.com".into(),
                allow_http: false,
            },
            auto: true,
            interval_minutes: 10,
            device_name: "ALICE-PC".into(),
        }),
        device_id: Some("d_abc".into()),
        last_sync: Some(1_700_000_000_000),
        ..Default::default()
    };
    let section = sync_section(&status);
    let text = section.to_string();
    for leak in ["dav.example.com", "alice", "ALICE-PC", "d_abc"] {
        assert!(!text.contains(leak), "{text}");
    }
    assert_eq!(section["config"]["type"], "webdav");
    assert_eq!(section["config"]["intervalMinutes"], 10);
    assert_eq!(section["configured"], true);
    assert_eq!(sync_section(&SyncStatus::default())["configured"], false);
}

#[test]
fn default_file_name_has_timestamp() {
    let name = default_file_name(crate::time::now_ms());
    assert!(name.starts_with("termlet-diagnostics-"), "{name}");
    assert!(name.ends_with(".txt"), "{name}");
    assert_eq!(name.len(), "termlet-diagnostics-20261020-051000.txt".len());
}

#[test]
fn system_info_is_filled() {
    let info = system_info();
    assert_eq!(info.os, std::env::consts::OS);
    assert!(!info.name.is_empty());
    assert!(info.cpus > 0);
    if cfg!(windows) {
        assert!(info.version.starts_with("10.0."), "{}", info.version);
    }
}

// ---------------------------------------------------------------- 整体

fn data_dir_with_store() -> (tempfile::TempDir, DataDir, Arc<Store>) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = DataDir::at(tmp.path().join("termlet"));
    dir.prepare().unwrap();
    let db = Database::open(&dir.database(), &dir.backups_dir()).unwrap();
    let vault = Vault::open(&db, Box::new(MemoryKeyStore::default())).unwrap();
    (tmp, dir, Arc::new(Store::open(db, vault).unwrap()))
}

/// 生成整段诊断信息：有版本、数量、界面给的信息与日志；主机地址、日志中的 IP 都不出现。
#[test]
fn diagnostics_text_and_export() {
    let (tmp, dir, store) = data_dir_with_store();
    store
        .create(
            Host {
                name: "生产库".into(),
                address: "10.9.8.7".into(),
                username: "deploy".into(),
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    fs::write(
        dir.logs_dir().join("termlet.2026-10-20.log"),
        "2026-10-20T00:00:00Z  INFO termlet_core::ssh: 已连接 10.9.8.7:22\n",
    )
    .unwrap();
    let diagnostics = Diagnostics::new(store, dir.clone())
        .client("windows", 2)
        .section("terminals", json!({"open": 3}));
    let text = crate::runtime::runtime().block_on(diagnostics.clone().text(Scope::Copy));
    assert!(text.starts_with("Termlet 诊断信息"), "{text}");
    assert!(
        text.contains(&format!(r#""version": "{}""#, crate::VERSION)),
        "{text}"
    );
    assert!(text.contains(r#""hosts": 1"#), "{text}");
    assert!(text.contains(r#""schemaVersion": 1"#), "{text}");
    assert!(text.contains(r#""vault": "unlocked""#), "{text}");
    assert!(text.contains("## terminals"), "{text}");
    assert!(text.contains(r#""windows": 2"#), "{text}");
    assert!(text.contains("## 日志：termlet.2026-10-20.log"), "{text}");
    assert!(text.contains("已连接 <ip>:22"), "{text}");
    for leak in ["10.9.8.7", "生产库", "deploy"] {
        assert!(!text.contains(leak), "不应包含 {leak}：{text}");
    }

    let path = tmp.path().join(default_file_name(0));
    crate::runtime::runtime()
        .block_on(diagnostics.export(path.clone()))
        .unwrap();
    let saved = fs::read_to_string(&path).unwrap();
    assert!(saved.contains("## 日志：termlet.2026-10-20.log"));
}
