//! 样例按模块说明中列出的几份开源 FinalShell 导入（NyaTerm 290212c、Hatoba、DengShell）归纳的格式手写。

use std::path::PathBuf;

use super::*;
use crate::import::files::tests::{env, write};
use crate::model::Encoding;

/// FinalShell 保存的 “DengShell-QA-123!”（取自 DengShell（MIT）的测试，见 `password.rs`）
const SAVED: &str = "AQIDBAUGBwhSo9sQPy5eFkuGZiawt4poDYA6D2O3v3Y=";

fn files(items: &[(&str, &str)]) -> Vec<(Vec<String>, String)> {
    items
        .iter()
        .map(|(rel, text)| {
            (
                rel.split('/').map(str::to_string).collect(),
                text.to_string(),
            )
        })
        .collect()
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

#[test]
fn data_directory() {
    let dir = tempfile::tempdir().unwrap();
    let conn = dir.path().join("conn");
    let put = |rel: &str, text: &str| write(&conn.join(rel), text.as_bytes());
    put(
        "1001_connect_config.json",
        &format!(
            r#"{{"forwarding_auto_reconnect":false,"delete_time":0,"secret_key_id":"","user_name":"root",
               "remote_port_forwarding":{{}},"conection_type":100,"description":"前端\n4 号机柜","proxy_id":"",
               "authentication_type":1,"password":"{SAVED}","host":"192.0.2.10","id":"1001","port_forwarding_list":[],
               "parent_id":"root","port":22,"terminal_encoding":"UTF-8","exec_channel_enable":true,"name":"web-1"}}"#
        ),
    );
    put(
        "f1/folder.json",
        r#"{"id":"f1","name":"生产","parent_id":"root","delete_time":0}"#,
    );
    put(
        "f1/f2/folder.json",
        r#"{"id":"f2","name":"数据库","parent_id":"f1"}"#,
    );
    put(
        "f1/f2/1003_connect_config.json",
        r#"{"id":"1003","name":"db-1","host":"192.0.2.30","port":"2222","user_name":"dba","conection_type":100,
            "authentication_type":2,"secret_key_id":"k1","password":"","parent_id":"f2","terminal_encoding":"GBK"}"#,
    );
    put(
        "f1/1002_connect_config.json",
        r#"{"id":"1002","name":"跳板","host":"192.0.2.20","conection_type":100,"authentication_type":1,
            "parent_id":"f1","proxy_id":"p1","port_forwarding_list":[{"a":1},{"a":2}],
            "remote_port_forwarding":{"x":1},"terminal_encoding":"ISO-8859-1"}"#,
    );
    // 已删除的文件夹中没删的连接：放在最外层
    put(
        "f3/folder.json",
        r#"{"id":"f3","name":"旧环境","parent_id":"root","delete_time":1700000000000}"#,
    );
    put(
        "f3/1004_connect_config.json",
        r#"{"id":"1004","name":"old-1","host":"192.0.2.40","user_name":"u","conection_type":100,"parent_id":"f3"}"#,
    );
    // 跳过并计数：RDP、格式不对、没有地址；回收站中的连接不计数
    put(
        "1005_connect_config.json",
        r#"{"id":"1005","name":"desktop","host":"192.0.2.50","port":3389,"conection_type":101}"#,
    );
    put("1006_connect_config.json", "{ not json");
    put(
        "1007_connect_config.json",
        r#"{"id":"1007","name":"no-host","host":" ","conection_type":100}"#,
    );
    put(
        "1008_connect_config.json",
        r#"{"id":"1008","name":"deleted","host":"192.0.2.80","conection_type":100,"delete_time":1700000000000}"#,
    );
    // 不是连接的文件
    put(
        "config.json",
        r#"{"secret_key_list":[{"id":"k1","name":"id_ed25519"}]}"#,
    );
    put("f1/other.json", r#"{"host":"192.0.2.99","user_name":"x"}"#);

    // 选数据目录（conn 的上一级）
    let plan = read(dir.path(), &env()).unwrap();
    let mut names: Vec<_> = plan.hosts.iter().map(|h| h.host.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["db-1", "old-1", "web-1", "跳板"]);
    assert_eq!(plan.skipped, 3, "RDP、格式不对、没有地址");

    let web = host(&plan, "web-1");
    assert_eq!(
        (
            web.host.address.as_str(),
            web.host.port,
            web.host.username.as_str()
        ),
        ("192.0.2.10", 22, "root")
    );
    assert_eq!(web.host.note, "前端\n4 号机柜");
    assert!(group(&plan, web).is_empty());
    assert!(web.hints.is_empty(), "{:?}", web.hints);
    assert_eq!(
        web.password().map(|p| p.expose()),
        Some("DengShell-QA-123!")
    );

    let db = host(&plan, "db-1");
    assert_eq!(
        group(&plan, db),
        ["生产", "数据库"],
        "分组按 parent_id，不按目录名"
    );
    assert_eq!(db.host.port, 2222, "字符串端口");
    assert_eq!(db.host.terminal.encoding, Encoding::Gb18030);
    assert_eq!(db.hints, [Hint::PrivateKeyNotImported { name: None }]);
    assert!(!db.has_password());

    let jump = host(&plan, "跳板");
    assert_eq!(group(&plan, jump), ["生产"]);
    assert_eq!(jump.host.username, "");
    assert_eq!(
        jump.hints,
        [
            Hint::MissingUsername,
            Hint::UnsupportedEncoding {
                name: "ISO-8859-1".into()
            },
            Hint::ProxyNotImported { name: None },
            Hint::ForwardsNotImported { count: 3 },
        ]
    );

    let old = host(&plan, "old-1");
    assert!(group(&plan, old).is_empty(), "文件夹已删除");
}

#[test]
fn exported_directory() {
    // 右键导出：目录以文件夹名称命名，没有 folder.json，parent_id 指向没有导出的文件夹
    let plan = parse(&files(&[
        (
            "0001_connect_config.json",
            r#"{"id":"1","name":"top","host":"h1","user_name":"u","parent_id":"root"}"#,
        ),
        (
            "Staging/3001_connect_config.json",
            r#"{"id":"3001","name":"stage","host":"h2","user_name":"ci","conection_type":100,"parent_id":"gone"}"#,
        ),
        (
            "Staging/Web/3002_connect_config.json",
            r#"{"id":"3002","name":"web","host":"h3","user_name":"ci","conection_type":100,"parent_id":"gone2"}"#,
        ),
    ]));
    assert_eq!(plan.hosts.len(), 3);
    assert!(group(&plan, host(&plan, "top")).is_empty());
    assert_eq!(group(&plan, host(&plan, "stage")), ["Staging"]);
    assert_eq!(group(&plan, host(&plan, "web")), ["Staging", "Web"]);
}

#[test]
fn folder_chains() {
    let plan = parse(&files(&[
        // 成环的上级
        ("a/folder.json", r#"{"id":"a","name":"甲","parent_id":"b"}"#),
        ("b/folder.json", r#"{"id":"b","name":"乙","parent_id":"a"}"#),
        // 上级不存在
        (
            "c/folder.json",
            r#"{"id":"c","name":"丙","parent_id":"missing"}"#,
        ),
        // ID 是数字
        ("7/folder.json", r#"{"id":7,"name":"丁","parent_id":0}"#),
        (
            "a/1_connect_config.json",
            r#"{"name":"loop","host":"h1","user_name":"u","parent_id":"a"}"#,
        ),
        (
            "c/2_connect_config.json",
            r#"{"name":"orphan","host":"h2","user_name":"u","parent_id":"c"}"#,
        ),
        (
            "7/3_connect_config.json",
            r#"{"name":"numeric","host":"h3","user_name":"u","parent_id":7}"#,
        ),
    ]));
    assert_eq!(group(&plan, host(&plan, "loop")), ["乙", "甲"]);
    assert_eq!(group(&plan, host(&plan, "orphan")), ["丙"]);
    assert_eq!(group(&plan, host(&plan, "numeric")), ["丁"]);
}

#[test]
fn saved_passwords() {
    let conn = |name: &str, extra: &str| {
        format!(r#"{{"name":"{name}","host":"{name}.example","user_name":"u"{extra}}}"#)
    };
    let plan = parse(&files(&[
        (
            "1_connect_config.json",
            &conn(
                "pw",
                &format!(r#","authentication_type":1,"password":"{SAVED}""#),
            ),
        ),
        // 旧文件没有 authentication_type：有密码就按密码
        (
            "2_connect_config.json",
            &conn("legacy", &format!(r#","password":"{SAVED}""#)),
        ),
        // 公钥认证时留着的旧密码不导入
        (
            "3_connect_config.json",
            &conn(
                "key",
                &format!(r#","authentication_type":2,"secret_key_id":"k","password":"{SAVED}""#),
            ),
        ),
        (
            "4_connect_config.json",
            &conn("broken", r#","authentication_type":1,"password":"AAAA""#),
        ),
        (
            "5_connect_config.json",
            &conn("empty", r#","authentication_type":1,"password":"""#),
        ),
    ]));
    let password = |name: &str| host(&plan, name).password().map(|p| p.expose().to_string());
    assert_eq!(password("pw").as_deref(), Some("DengShell-QA-123!"));
    assert_eq!(password("legacy").as_deref(), Some("DengShell-QA-123!"));
    assert_eq!(password("key"), None);
    assert_eq!(password("broken"), None);
    assert_eq!(host(&plan, "broken").hints, [Hint::SavedPasswordInvalid]);
    assert_eq!(password("empty"), None);
    assert!(host(&plan, "empty").hints.is_empty());
    assert!(plan.has_secrets());
    assert!(
        !plan.without_secrets().has_secrets(),
        "不勾选“导入保存的密码”时不带密码"
    );
    assert!(Source::FinalShell.secrets_need_consent());
}

#[test]
fn lenient_fields() {
    let plan = parse(&files(&[
        (
            "a_connect_config.json",
            r#"{"host":" h ","user_name":"u","port":" 2200 ","secret_key_id":0}"#,
        ),
        ("b_connect_config.json", "[1,2]"),
        (
            "c_connect_config.json",
            r#"{"host":"h","port":70000,"username":"u","proxy_id":"-1","port_forwarding_list":[]}"#,
        ),
        (
            "d_connect_config.json",
            r#"{"host":"h4","user_name":"u","secret_key_id":"k2"}"#,
        ),
    ]));
    assert_eq!(plan.hosts.len(), 3);
    assert_eq!(plan.skipped, 1, "不是对象的连接文件");
    let a = &plan.hosts[0];
    assert_eq!((a.host.address.as_str(), a.host.name.as_str()), ("h", "h"));
    assert_eq!(a.host.port, 2200, "字符串端口");
    assert!(a.hints.is_empty(), "secret_key_id 为 0 表示没有私钥");
    let c = &plan.hosts[1];
    assert_eq!(c.host.port, 22, "超出范围的端口按 22");
    assert_eq!(c.host.username, "u", "旧的 username 字段");
    assert!(
        c.hints.is_empty(),
        "代理 ID 为 -1、空的端口转发列表都表示没有"
    );
    assert_eq!(
        plan.hosts[2].hints,
        [Hint::PrivateKeyNotImported { name: None }],
        "没有 authentication_type、有私钥 ID 的按公钥"
    );
    assert_eq!(plan.hosts[0].key, "a_connect_config.json");
}

#[test]
fn default_candidates_order() {
    let mut env = env();
    let home = env.home.clone().unwrap();
    env.config_dir = Some(home.join("AppData").join("Roaming"));
    let installed = PathBuf::from("D:/Tools/FinalShell");
    let got = default_candidates(&env, || Some(installed.clone()));
    assert_eq!(got[0], installed.join("conn"), "注册表中的安装目录在前");
    assert_eq!(
        got[1],
        home.join("AppData")
            .join("Local")
            .join("finalshell")
            .join("conn")
    );
    assert_eq!(
        got[2],
        home.join("AppData")
            .join("Roaming")
            .join("finalshell")
            .join("conn")
    );
    assert!(got.contains(&home.join(".finalshell").join("conn")));
    assert!(got.contains(&home.join("Library").join("FinalShell").join("conn")));
    assert!(default_candidates(&env, || None).len() < got.len());
}
