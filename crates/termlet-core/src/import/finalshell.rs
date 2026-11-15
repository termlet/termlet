//! FinalShell 的连接配置目录（conn）。
//!
//! FinalShell 把每个连接保存为一个 JSON 文件。这里按字段名宽容解析：有 `host` 与 `user_name`（或 `username`）
//! 字段的对象视为一个连接，`name`、`port` 可选；连接文件所在的子目录作为分组。
//! 不同版本的格式可能有差异，未识别的文件（例如文件夹的配置）跳过、不计数。

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::model::Host;

use super::files::{
    LocalEnv, MAX_CONFIG_SIZE, decode_text, dir_segments, read_limited, walk_files,
};
use super::plan::{Hint, PlanBuilder, PlannedHost};
use super::{ImportError, ImportPlan, Source};

pub(super) fn read(dir: &Path, env: &LocalEnv) -> Result<ImportPlan, ImportError> {
    let dir = env.input_path(dir);
    let mut plan = PlanBuilder::new(Source::FinalShell);
    for (rel, file) in walk_files(&dir, "json")? {
        let Ok(raw) = read_limited(&file, MAX_CONFIG_SIZE) else {
            continue;
        };
        add_connection(&mut plan, &rel, &decode_text(&raw, env.ansi));
    }
    Ok(plan.finish())
}

/// 加入一个连接：`rel` 是相对于连接目录的各级名称（最后一个是文件名），`text` 是文件内容。
pub(super) fn add_connection(plan: &mut PlanBuilder, rel: &[String], text: &str) {
    let Ok(Value::Object(conn)) = serde_json::from_str::<Value>(text) else {
        return;
    };
    let string = |key: &str| {
        conn.get(key)
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let address = string("host");
    let mut user = string("user_name");
    if user.is_empty() {
        user = string("username");
    }
    if address.is_empty() || user.is_empty() {
        return; // 不是连接配置（例如文件夹或其他设置）
    }
    let mut name = string("name");
    if name.is_empty() {
        name = address.clone();
    }
    let port = conn
        .get("port")
        .and_then(Value::as_f64)
        .filter(|p| (1.0..65536.0).contains(p))
        .map_or(22, |p| p as u16);
    let host = Host {
        name,
        address,
        port,
        username: user,
        ..Default::default()
    };
    let mut item = PlannedHost::new(rel.join("/"), host);
    item.group = plan.group(dir_segments(rel));
    if conn.get("secret_key_id").is_some_and(has_value) {
        item.hints.push(Hint::PrivateKeyNotImported { name: None });
    }
    plan.push(item);
}

/// 字段是否有实际值（非空字符串或非零数字）。
fn has_value(v: &Value) -> bool {
    match v {
        Value::String(s) => !s.trim().is_empty(),
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        _ => false,
    }
}

/// FinalShell 的连接配置位于其数据目录下的 conn 目录，安装位置因人而异。
pub(super) fn default_candidates(env: &LocalEnv) -> Vec<PathBuf> {
    let Some(home) = &env.home else {
        return Vec::new();
    };
    [
        home.join("AppData").join("Local").join("finalshell"),
        home.join("finalshell"),
        home.join("Library").join("FinalShell"),
    ]
    .into_iter()
    .map(|base| base.join("conn"))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::files::tests::{env, write};

    #[test]
    fn connections_directory() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("a_connect_config.json"),
            r#"{"name":"跳板机","host":"1.2.3.4","port":2200,"user_name":"ops","secret_key_id":"k1"}"#
                .as_bytes(),
        );
        write(
            &dir.path().join("测试环境").join("b_connect_config.json"),
            br#"{"host":"5.6.7.8","username":"dev"}"#,
        );
        write(
            &dir.path().join("folder_config.json"),
            r#"{"name":"测试环境","id":"f1"}"#.as_bytes(),
        );
        write(&dir.path().join("broken.json"), b"{not json");

        let plan = read(dir.path(), &env()).unwrap();
        assert_eq!(plan.hosts.len(), 2, "应识别 2 个连接");
        let by_address = |a: &str| plan.hosts.iter().find(|h| h.host.address == a).unwrap();
        let a = by_address("1.2.3.4");
        assert_eq!(
            (a.host.name.as_str(), a.host.port, a.host.username.as_str()),
            ("跳板机", 2200, "ops")
        );
        assert_eq!(a.hints, [Hint::PrivateKeyNotImported { name: None }]);
        let b = by_address("5.6.7.8");
        assert_eq!(
            (b.host.name.as_str(), b.host.port),
            ("5.6.7.8", 22),
            "名称缺省取地址"
        );
        assert_eq!(plan.group_path(b.group.unwrap()), ["测试环境"]);
        assert!(b.hints.is_empty());
    }

    #[test]
    fn lenient_fields() {
        let mut plan = PlanBuilder::new(Source::FinalShell);
        let rel = ["x.json".to_string()];
        add_connection(
            &mut plan,
            &rel,
            r#"{"host":" h ","user_name":"u","port":"22","secret_key_id":0}"#,
        );
        add_connection(&mut plan, &rel, r#"[1,2]"#);
        add_connection(
            &mut plan,
            &rel,
            r#"{"host":"h","port":70000,"username":"u"}"#,
        );
        let plan = plan.finish();
        assert_eq!(plan.hosts.len(), 2);
        assert_eq!(plan.hosts[0].host.address, "h");
        assert_eq!(plan.hosts[0].host.port, 22, "字符串端口不认");
        assert!(
            plan.hosts[0].hints.is_empty(),
            "secret_key_id 为 0 表示没有私钥"
        );
        assert_eq!(plan.hosts[1].host.port, 22, "超出范围的端口按 22");
        assert_eq!(plan.hosts[1].key, "x.json#2");
    }
}
