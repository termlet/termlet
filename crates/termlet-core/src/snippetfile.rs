//! Termlet 自己的快捷命令文件：快捷命令页“更多”→“导出快捷命令…”按分组导出，导入快捷命令时选“Termlet 快捷命令”
//! （[`crate::import::Source::TermletSnippets`]，NyaTerm 的来源中选了它也认得出）。
//!
//! 文件是可以直接阅读、编辑的 JSON，内容原样写出（快捷命令中写着的密码等也是明文），只有当前用户能读写：
//!
//! ```json
//! {
//!   "format": "termlet-snippets",
//!   "version": 1,
//!   "exportedAt": 1795680000000,
//!   "app": "0.3.5",
//!   "snippets": [
//!     { "name": "磁盘", "content": "df -h", "run": true },
//!     { "name": "日志", "group": "运维 / Kubernetes", "content": "kubectl logs -f {{pod}} -c {{容器:app}}", "run": false }
//!   ]
//! }
//! ```
//!
//! - `name` 名称；`group` 分组名，未分组的省略（Termlet 的分组只有一层，嵌套的写成“上级 / 下级”，原样写出）；
//! - `content` 内容，不含末尾的换行；`run` 发送后立即执行（Termlet 中保存为内容末尾的换行）；参数 `{{名称}}`、
//!   `{{名称:默认值}}` 与 `{{.Names}}` 这类都按原文；
//! - 不含 ID、时间等本机的内部字段。按快捷命令页的顺序写出：未分组的在前，分组按名称，同一分组中按保存的顺序。
//!
//! 读取（[`parse`]）：`format` 不是 [`FORMAT_NAME`] 的不是这种文件；`version` 比 [`VERSION`] 新的报“来自更新版本”；
//! 未知的字段忽略。一条中 `content` 不是字符串、为空或超长的跳过并计数；`name`、`group` 过长时截断，类型不对的
//! 当作没有；没有 `run` 时内容按原文（末尾有换行即执行）。

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::backup::BackupError;
use crate::import::ImportError;
use crate::model::Snippet;
use crate::store::{Record, Store};

/// 文件中 `format` 字段的值，也是默认文件名的开头。
pub const FORMAT_NAME: &str = "termlet-snippets";
/// 文件格式的版本。
pub const VERSION: i64 = 1;

/// 导出的默认文件名：`termlet-snippets-2026-11-26.json`（本地日期）。
pub fn default_file_name() -> String {
    format!(
        "{FORMAT_NAME}-{}.json",
        chrono::Local::now().format("%Y-%m-%d")
    )
}

/// 快捷命令的分组与各组的条数，按快捷命令页的顺序（未分组的空串在前，分组按名称）。
pub fn groups(snippets: &[Record<Snippet>]) -> Vec<(String, usize)> {
    let mut groups: BTreeMap<&str, usize> = BTreeMap::new();
    for snippet in snippets {
        *groups.entry(snippet.doc.group.as_str()).or_default() += 1;
    }
    groups
        .into_iter()
        .map(|(group, count)| (group.to_string(), count))
        .collect()
}

/// 导出 `groups`（分组名，空串为未分组）中的快捷命令到 `path`（本机的绝对路径，只有当前用户能读写；已存在时替换），
/// 返回导出的条数。这些分组中没有快捷命令时返回 [`BackupError::NothingSelected`]。必须在核心的运行时上执行。
pub async fn export(
    store: Arc<Store>,
    path: PathBuf,
    groups: Vec<String>,
) -> Result<usize, BackupError> {
    let job = tokio::task::spawn_blocking(move || {
        let groups: HashSet<String> = groups.into_iter().collect();
        let snippets = store.list::<Snippet>();
        let chosen = collect(&snippets, &groups);
        if chosen.is_empty() {
            return Err(BackupError::NothingSelected);
        }
        crate::backup::write_file(&path, to_json(&chosen).as_bytes())?;
        // 只记条数，不记内容
        tracing::info!(snippets = chosen.len(), "已导出快捷命令");
        Ok(chosen.len())
    });
    match job.await {
        Ok(result) => result,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => Err(BackupError::Store(e.to_string())),
    }
}

/// `groups` 中的快捷命令，按快捷命令页的顺序。
fn collect<'a>(snippets: &'a [Record<Snippet>], groups: &HashSet<String>) -> Vec<&'a Snippet> {
    let mut by_group: BTreeMap<&str, Vec<&Snippet>> = BTreeMap::new();
    for snippet in snippets.iter().filter(|s| groups.contains(&s.doc.group)) {
        by_group
            .entry(snippet.doc.group.as_str())
            .or_default()
            .push(&snippet.doc);
    }
    by_group.into_values().flatten().collect()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FileDoc<'a> {
    format: &'static str,
    version: i64,
    /// Unix 毫秒
    exported_at: i64,
    app: &'static str,
    snippets: Vec<FileSnippet<'a>>,
}

#[derive(Serialize)]
struct FileSnippet<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    group: &'a str,
    content: &'a str,
    run: bool,
}

/// 生成文件的内容（末尾带换行）。
pub(crate) fn to_json(snippets: &[&Snippet]) -> String {
    let doc = FileDoc {
        format: FORMAT_NAME,
        version: VERSION,
        exported_at: crate::time::now_ms(),
        app: crate::VERSION,
        snippets: snippets
            .iter()
            .map(|s| {
                let body = s.content.trim_end_matches(['\r', '\n']);
                FileSnippet {
                    name: &s.name,
                    group: &s.group,
                    content: body,
                    run: body.len() != s.content.len(),
                }
            })
            .collect(),
    };
    let mut json = serde_json::to_string_pretty(&doc).unwrap_or_default();
    json.push('\n');
    json
}

/// `value` 是 Termlet 的快捷命令文件（`format` 为 [`FORMAT_NAME`]），版本与内容另由 [`parse`] 检查。
pub(crate) fn is_snippet_file(value: &Value) -> bool {
    value.get("format").and_then(Value::as_str) == Some(FORMAT_NAME)
}

/// 读出文件中的快捷命令与跳过的条数。版本比 [`VERSION`] 新时返回 [`ImportError::SnippetsTooNew`]，
/// 不是这种文件或结构不对时返回 [`ImportError::NotTermletSnippets`]。
pub(crate) fn parse(value: &Value) -> Result<(Vec<Snippet>, usize), ImportError> {
    let invalid = |reason: &str| ImportError::NotTermletSnippets(reason.to_string());
    if !is_snippet_file(value) {
        return Err(invalid("format 不是 termlet-snippets"));
    }
    let version = value
        .get("version")
        .and_then(Value::as_i64)
        .ok_or_else(|| invalid("没有 version"))?;
    if version > VERSION {
        return Err(ImportError::SnippetsTooNew {
            found: version,
            supported: VERSION,
        });
    }
    if version < 1 {
        return Err(invalid("version 不对"));
    }
    let Some(Value::Array(items)) = value.get("snippets") else {
        return Err(invalid("没有 snippets 数组"));
    };
    let mut snippets = Vec::new();
    let mut skipped = 0;
    for item in items {
        match item.as_object().and_then(snippet) {
            Some(s) => snippets.push(s),
            None => skipped += 1,
        }
    }
    Ok((snippets, skipped))
}

/// 一条快捷命令；内容不是字符串、为空或超长时为 `None`。
fn snippet(o: &Map<String, Value>) -> Option<Snippet> {
    let content = o.get("content")?.as_str()?;
    let text = |key: &str| {
        o.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
    };
    let content = match o.get("run").and_then(Value::as_bool) {
        Some(run) => {
            let body = content.trim_end_matches(['\r', '\n']);
            if run {
                format!("{body}\n")
            } else {
                body.to_string()
            }
        }
        None => content.to_string(),
    };
    let mut snippet = Snippet {
        name: text("name").chars().take(128).collect(),
        group: text("group").chars().take(64).collect(),
        content,
        ..Default::default()
    };
    // 与保存时相同的校验：内容不能为空、不超过 64K 字符；没有名称时取内容的第一行
    snippet.normalize().ok()?;
    Some(snippet)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Position;
    use crate::testkit::{block_on, memory_store};

    fn add(store: &Store, group: &str, name: &str, content: &str) {
        store
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

    fn snippet(group: &str, name: &str, content: &str) -> Snippet {
        Snippet {
            name: name.into(),
            group: group.into(),
            content: content.into(),
            ..Default::default()
        }
    }

    fn parse_text(text: &str) -> Result<(Vec<Snippet>, usize), ImportError> {
        parse(&serde_json::from_str(text).unwrap())
    }

    #[test]
    fn default_file_name_has_the_date() {
        let name = default_file_name();
        assert!(name.starts_with("termlet-snippets-20"), "{name}");
        assert!(name.ends_with(".json"));
        assert_eq!(name.len(), "termlet-snippets-2026-11-26.json".len());
    }

    // 写出的字段：没有分组的省略 group，内容不含末尾的换行、另写 run；不含 ID 等内部字段
    #[test]
    fn file_has_only_user_visible_fields() {
        let a = snippet("", "磁盘", "df -h\n");
        let b = snippet("运维 / Kubernetes", "日志", "kubectl logs {{pod}}\r\n");
        let c = snippet("运维", "草稿", "echo 一\necho 二");
        let json: Value = serde_json::from_str(&to_json(&[&a, &b, &c])).unwrap();
        assert_eq!(json["format"], "termlet-snippets");
        assert_eq!(json["version"], 1);
        assert_eq!(json["app"], crate::VERSION);
        assert!(json["exportedAt"].as_i64().unwrap() > 0);
        assert_eq!(
            json["snippets"],
            serde_json::json!([
                {"name": "磁盘", "content": "df -h", "run": true},
                {"name": "日志", "group": "运维 / Kubernetes", "content": "kubectl logs {{pod}}", "run": true},
                {"name": "草稿", "group": "运维", "content": "echo 一\necho 二", "run": false},
            ])
        );
    }

    // 只导出选中的分组，按快捷命令页的顺序（未分组在前，分组按名称，组内按保存的顺序）
    #[test]
    fn groups_and_order_follow_the_snippets_page() {
        let store = memory_store();
        add(&store, "乙", "b1", "b1\n");
        add(&store, "", "无", "none\n");
        add(&store, "甲", "a1", "a1\n");
        add(&store, "乙", "b2", "b2\n");
        let list = store.list::<Snippet>();
        assert_eq!(
            groups(&list),
            [
                (String::new(), 1),
                ("乙".to_string(), 2),
                ("甲".to_string(), 1)
            ]
        );
        let names = |chosen: &[&str]| -> Vec<String> {
            let set = chosen.iter().map(|s| s.to_string()).collect();
            collect(&list, &set)
                .iter()
                .map(|s| s.name.clone())
                .collect()
        };
        assert_eq!(names(&["乙", "", "甲"]), ["无", "b1", "b2", "a1"]);
        assert_eq!(names(&["乙"]), ["b1", "b2"]);
        assert!(names(&["不存在"]).is_empty());
    }

    #[test]
    fn export_writes_the_chosen_groups() {
        let store = memory_store();
        add(&store, "", "pwd", "pwd\n");
        add(&store, "ops", "uptime", "uptime\n");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(default_file_name());
        let count = block_on(export(store.clone(), path.clone(), vec!["ops".into()])).unwrap();
        assert_eq!(count, 1);
        let (read, skipped) = parse_text(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read, [snippet("ops", "uptime", "uptime\n")]);
        assert_eq!(skipped, 0);
        assert!(matches!(
            block_on(export(store, path, vec!["没有".into()])),
            Err(BackupError::NothingSelected)
        ));
    }

    // 往返：中文、多行、参数与 {{.Names}} 原样，执行与否不变
    #[test]
    fn round_trip_keeps_everything_visible() {
        let originals = [
            snippet("", "磁盘", "df -h\n"),
            snippet(
                "运维 / Kubernetes",
                "看 Pod",
                "kubectl get pods -n {{命名空间:default}}\n",
            ),
            snippet("运维", "多行", "cd /srv/应用\ngit pull\n\tmake\n"),
            snippet("Docker", "名称", "docker ps --format '{{.Names}}'"),
            snippet(
                "",
                "参数",
                "ssh {{用户}}@{{主机:10.0.0.1}} 'echo \"你好\" \\n'",
            ),
        ];
        let refs: Vec<&Snippet> = originals.iter().collect();
        let (read, skipped) = parse_text(&to_json(&refs)).unwrap();
        assert_eq!(read, originals);
        assert_eq!(skipped, 0);
    }

    // 更新的版本报错；format、version、snippets 不对的不是这种文件
    #[test]
    fn versions_and_structure() {
        assert!(matches!(
            parse_text(r#"{"format":"termlet-snippets","version":2,"snippets":[]}"#),
            Err(ImportError::SnippetsTooNew {
                found: 2,
                supported: 1
            })
        ));
        for text in [
            r#"{"format":"termlet-hosts","version":1,"snippets":[]}"#,
            r#"{"format":"termlet-snippets","snippets":[]}"#,
            r#"{"format":"termlet-snippets","version":0,"snippets":[]}"#,
            r#"{"format":"termlet-snippets","version":"1","snippets":[]}"#,
            r#"{"format":"termlet-snippets","version":1}"#,
            r#"{"format":"termlet-snippets","version":1,"snippets":{}}"#,
        ] {
            assert!(
                matches!(parse_text(text), Err(ImportError::NotTermletSnippets(_))),
                "{text}"
            );
        }
        assert!(is_snippet_file(
            &serde_json::json!({"format": "termlet-snippets"})
        ));
        assert!(!is_snippet_file(&serde_json::json!([])));
    }

    // 未知字段忽略；类型不对的可选字段当作没有；内容不对的跳过；没有 run 时按原文；名称、分组过长时截断
    #[test]
    fn lenient_entries() {
        let long_name = "名".repeat(200);
        let text = serde_json::json!({
            "format": "termlet-snippets",
            "version": 1,
            "future": {"anything": true},
            "snippets": [
                {"name": "a", "content": "ls", "run": true, "color": "red", "pinned": true},
                {"name": 42, "group": ["x"], "content": "uptime\n"},
                {"name": "b", "content": "echo x\n", "run": false},
                {"name": "c", "content": "echo y\n", "run": "yes"},
                {"name": long_name, "group": "组".repeat(100), "content": "w"},
                {"name": "空", "content": "  \n", "run": true},
                {"name": "超长", "content": "x".repeat(64 * 1024 + 1)},
                {"name": "数字", "content": 42},
                "不是对象",
            ]
        })
        .to_string();
        let (read, skipped) = parse_text(&text).unwrap();
        assert_eq!(skipped, 4);
        let brief: Vec<(&str, &str, &str)> = read
            .iter()
            .map(|s| (s.group.as_str(), s.name.as_str(), s.content.as_str()))
            .collect();
        assert_eq!(brief[0], ("", "a", "ls\n"));
        // 没有名称时与保存时相同，取内容的第一行
        assert_eq!(brief[1], ("", "uptime", "uptime\n"));
        assert_eq!(brief[2], ("", "b", "echo x"));
        assert_eq!(brief[3], ("", "c", "echo y\n"));
        assert_eq!(read[4].name.chars().count(), 128);
        assert_eq!(read[4].group.chars().count(), 64);
    }
}
