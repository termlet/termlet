//! NyaTerm 在快捷命令面板右上角“导出快捷命令”得到的 JSON（`nyaterm-quick-commands.json`）。文件中只有快捷命令与
//! 它们的分类，没有主机，也没有秘密（NyaTerm 的主机只在加密的完整备份 `.nya` 与本机的数据库中，这里不读）。
//!
//! 格式按 NyaTerm（Apache-2.0）提交 daedb58（2.0.7）的源码：导出是 `crates/nyaterm-core/src/models/quick_commands.rs`
//! 的 `QuickCommandsExportConfig`，命令文本的含义在 `quick_command_script.rs`（转义与暂停）与桌面端的
//! `quick_command_runtime/variables.rs`（变量）中。
//!
//! ```text
//! { "categories": [{ "id", "name", "parent_id"?, "sort_order" }],
//!   "commands": [{ "id", "label", "command", "category_id"?, "description"?, "color_tag"?, "icon_tag"?, "pinned",
//!                  "execution_mode": "execute" | "append", "segmented_send"?: { "mode": "line" | "character", … },
//!                  "raw_send"?: true, "source"?, "risk_level"?, "sort_order"?, …新版本的字段 }] }
//! ```
//!
//! 也接受 NyaTerm 文档中手写的写法：顶层直接是命令数组，用 `category`（分类名）代替 `category_id`，省略 `id`。
//!
//! 转换：
//! - 名称取 `label`；分组按 `category_id` 找分类，嵌套的分类写成“上级 / 下级”（Termlet 的分组只是一层名称），找上级时
//!   遇到不存在或成环的就停；指向不存在的分类时用 id 本身（NyaTerm 导入时也以 id 为名新建分类）。
//! - 内容见 [`script`]：`raw_send` 为真时原样；否则按 NyaTerm 的脚本规则换成 Termlet 的写法（转义、暂停、变量）。
//! - 执行：`execute` 或逐行发送时内容末尾补一个换行（Termlet 以末尾的换行表示发送后执行），`append`、逐字发送不补。
//! - 说明、颜色、图标、置顶、风险等级、来源不导入；置顶与 `sort_order` 只决定同一分组中的先后（置顶的在前）。
//!
//! 丢掉或改动的内容计入 [`SnippetNotes`]，预览中说明；空的、超长的、字段类型不对的条目跳过并计数。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::model::Snippet;
use crate::snippet_params::{is_param_name, snippet_params};

use super::files::{LocalEnv, decode_text, newest_export, read_limited};
use super::plan::{PlanBuilder, SnippetNotes, clip};
use super::{ImportError, ImportPlan, Source};

/// 文件的大小上限（与 NyaTerm 自己导入时相同）。
const MAX_FILE: u64 = 4 << 20;
/// 导出的默认文件名（小写），在“下载”等处找时按开头匹配（另存时可能带了序号）。
const FILE_NAME: &str = "nyaterm-quick-commands";
/// NyaTerm 的配置目录（正式版、预览版）：导出时保存对话框默认打开这里。
const CONFIG_DIRS: [&str; 2] = [".nyaterm", ".nyaterm-preview"];
/// 分类的层数上限。
const MAX_GROUP_DEPTH: usize = 32;
/// 嵌套的分类合成一层时各级之间的分隔。
const GROUP_SEPARATOR: &str = " / ";
/// 快捷命令内容的上限（字符，与保存时的校验相同）。
const MAX_CONTENT: usize = 64 * 1024;

pub(super) fn read(path: &Path, env: &LocalEnv) -> Result<ImportPlan, ImportError> {
    let path = env.input_path(path);
    let raw = read_limited(&path, MAX_FILE)?;
    parse(&decode_text(&raw, env.ansi))
}

/// 默认位置：NyaTerm 配置目录中的 `nyaterm-quick-commands.json`，没有时为“下载”“桌面”“文档”中最新的
/// `nyaterm-quick-commands*.json`。
pub(super) fn default_path(env: &LocalEnv) -> Option<PathBuf> {
    let exported = env.home.as_ref().and_then(|home| {
        CONFIG_DIRS
            .iter()
            .map(|dir| home.join(dir).join(format!("{FILE_NAME}.json")))
            .find(|path| path.is_file())
    });
    exported.or_else(|| {
        newest_export(env, |name| {
            name.starts_with(FILE_NAME) && name.ends_with(".json")
        })
    })
}

pub(super) fn parse(text: &str) -> Result<ImportPlan, ImportError> {
    let not_nyaterm = |reason: &str| ImportError::NotNyaTerm(reason.to_string());
    let value: Value =
        serde_json::from_str(text).map_err(|e| ImportError::NotNyaTerm(e.to_string()))?;
    let (categories, commands) = match value {
        Value::Array(commands) => (Vec::new(), commands),
        Value::Object(mut root) => {
            let Some(Value::Array(commands)) = root.remove("commands") else {
                return Err(not_nyaterm("没有 commands 数组"));
            };
            let categories = match root.remove("categories") {
                Some(Value::Array(categories)) => categories,
                _ => Vec::new(),
            };
            (categories, commands)
        }
        _ => return Err(not_nyaterm("不是 JSON 对象或数组")),
    };
    let categories = Categories::new(&categories);
    let mut plan = PlanBuilder::new(Source::NyaTerm);
    // 分组按第一次出现的顺序，同一分组中置顶的在前，再按 sort_order、文件中的顺序
    let mut group_order: HashMap<String, usize> = HashMap::new();
    let mut items: Vec<((usize, bool, i64, usize), Snippet)> = Vec::new();
    let mut notes = SnippetNotes::default();
    for (index, value) in commands.iter().enumerate() {
        let Some(object) = value.as_object() else {
            plan.skip(1);
            continue;
        };
        let Some(command) = command(object, &categories, &mut notes) else {
            plan.skip(1);
            continue;
        };
        let next = group_order.len();
        let group = *group_order
            .entry(command.snippet.group.clone())
            .or_insert(next);
        let key = (
            group,
            !command.pinned,
            command.sort_order.unwrap_or(i64::MAX),
            index,
        );
        items.push((key, command.snippet));
    }
    items.sort_by_key(|(key, _)| *key);
    for (_, snippet) in items {
        plan.add_snippet(snippet);
    }
    *plan.snippet_notes_mut() = notes;
    Ok(plan.finish())
}

/// 文件中的分类：id → （名称，上级 id）。
struct Categories(HashMap<String, (String, Option<String>)>);

impl Categories {
    fn new(values: &[Value]) -> Self {
        let mut map = HashMap::new();
        for value in values {
            let Some(o) = value.as_object() else {
                continue;
            };
            let (Some(id), Some(name)) = (text(o, "id"), text(o, "name")) else {
                continue;
            };
            map.entry(id.to_string())
                .or_insert((name.to_string(), text(o, "parent_id").map(String::from)));
        }
        Self(map)
    }

    /// 分类从根开始的各级名称；不存在时为 `None`。上级不存在、成环或超过层数时停在那里。
    fn path(&self, id: &str) -> Option<Vec<&str>> {
        let mut names = Vec::new();
        let mut seen = HashSet::new();
        let mut current = Some(id);
        while let Some(id) = current {
            if !seen.insert(id) || names.len() >= MAX_GROUP_DEPTH {
                break;
            }
            let Some((name, parent)) = self.0.get(id) else {
                break;
            };
            names.push(name.as_str());
            current = parent.as_deref();
        }
        if names.is_empty() {
            return None;
        }
        names.reverse();
        Some(names)
    }
}

/// 去掉首尾空白后不为空的字符串字段。
fn text<'a>(o: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    o.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// 一条转换好的快捷命令与排序用的字段。
struct Command {
    snippet: Snippet,
    pinned: bool,
    sort_order: Option<i64>,
}

/// 转换一条命令；`label`、`command` 不是字符串，内容为空或超长时为 `None`。丢掉或改动的内容记入 `notes`
/// （只记导入的）。
fn command(
    o: &Map<String, Value>,
    categories: &Categories,
    notes: &mut SnippetNotes,
) -> Option<Command> {
    let label = o.get("label")?.as_str()?;
    let source = o.get("command")?.as_str()?;
    let raw_send = o.get("raw_send").and_then(Value::as_bool).unwrap_or(false);
    // 逐行发送最后补行尾（执行）；逐字不补；写法认不出的 NyaTerm 也不执行，按逐字处理
    let paced_line = match o.get("segmented_send") {
        None | Some(Value::Null) => None,
        Some(send) => Some(send.get("mode").and_then(Value::as_str) == Some("line")),
    };
    let execute = paced_line
        .unwrap_or_else(|| o.get("execution_mode").and_then(Value::as_str) != Some("append"));

    let mut changes = Changes::default();
    let mut content = if raw_send {
        changes.literal_params = !snippet_params(source).is_empty();
        source.to_string()
    } else {
        script(source, &mut changes)
    };
    if content.trim().is_empty() {
        return None;
    }
    if execute && !content.ends_with(['\r', '\n']) {
        content.push('\n');
    }
    if content.chars().count() > MAX_CONTENT {
        return None;
    }

    let path = match (text(o, "category_id"), text(o, "category")) {
        (Some(id), _) => categories.path(id).unwrap_or_else(|| vec![id]),
        (None, Some(name)) => vec![name],
        (None, None) => Vec::new(),
    };
    let decorated = ["color_tag", "icon_tag", "risk_level"]
        .iter()
        .any(|key| text(o, key).is_some())
        || o.get("pinned").and_then(Value::as_bool) == Some(true);

    let count = |flag: bool, n: &mut usize| *n += usize::from(flag);
    count(changes.pauses, &mut notes.pauses);
    count(changes.options, &mut notes.options);
    count(changes.unknown_params, &mut notes.unknown_params);
    count(changes.literal_params, &mut notes.literal_params);
    count(paced_line.is_some(), &mut notes.paced);
    count(text(o, "description").is_some(), &mut notes.descriptions);
    count(decorated, &mut notes.decorations);
    count(path.len() > 1, &mut notes.nested_groups);

    Some(Command {
        snippet: Snippet {
            name: clip(label.trim(), 128),
            content,
            group: clip(&path.join(GROUP_SEPARATOR), 64).trim().to_string(),
            ..Default::default()
        },
        pinned: o.get("pinned").and_then(Value::as_bool).unwrap_or(false),
        sort_order: o.get("sort_order").and_then(Value::as_i64),
    })
}

/// 一条命令转换时改动的内容。
#[derive(Debug, Default, PartialEq, Eq)]
struct Changes {
    pauses: bool,
    options: bool,
    unknown_params: bool,
    literal_params: bool,
}

/// NyaTerm 的命令脚本换成 Termlet 的写法：
///
/// | NyaTerm | Termlet |
/// |---|---|
/// | `\r`、`\n` | 换行（执行时都是回车） |
/// | `\t` | 制表符 |
/// | `\\` | `\` |
/// | `\p`（暂停 1 秒） | 去掉 |
/// | `\r\p` 之后紧跟的换行 | 去掉（NyaTerm 当作排版，不发送） |
/// | 其他 `\x` | 原样 |
/// | `{{名}}`、`{{名=默认值}}`、`{{名\|甲,乙}}` | `{{名}}`、`{{名:默认值}}`、`{{名:甲}}` |
///
/// 变量中的文字不按转义处理（NyaTerm 填入的值按字面发送）。名称 Termlet 认不出、默认值中有 `{`、`}` 的变量按原文。
fn script(text: &str, changes: &mut Changes) -> String {
    let mut out = String::with_capacity(text.len());
    // 上一个写出的是回车（`\r` 或真实的回车）
    let mut returned = false;
    // 回车之后有暂停：接下来的真实换行是排版
    let mut layout = false;
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if let Some((variable, after)) = take_variable(rest) {
            out.push_str(&convert_variable(variable, changes));
            (returned, layout) = (false, false);
            rest = after;
            continue;
        }
        rest = &rest[c.len_utf8()..];
        if c != '\\' {
            if layout && (c == '\r' || c == '\n') {
                continue;
            }
            out.push(c);
            (returned, layout) = (c == '\r', false);
            continue;
        }
        let Some(next) = rest.chars().next() else {
            // 末尾单独的反斜杠原样
            out.push('\\');
            break;
        };
        rest = &rest[next.len_utf8()..];
        match next {
            'p' => {
                changes.pauses = true;
                layout |= returned;
            }
            'r' => {
                out.push('\n');
                (returned, layout) = (true, false);
            }
            'n' | 't' | '\\' => {
                out.push(match next {
                    'n' => '\n',
                    't' => '\t',
                    _ => '\\',
                });
                (returned, layout) = (false, false);
            }
            other => {
                out.push('\\');
                out.push(other);
                (returned, layout) = (false, false);
            }
        }
    }
    out
}

/// `text` 以 NyaTerm 的变量开头时返回 `{{` 与 `}}` 之间的部分与其后的文字（与 NyaTerm 相同：找最近的 `}}`，
/// 中间为空的不算）。
fn take_variable(text: &str) -> Option<(&str, &str)> {
    let inner = text.strip_prefix("{{")?;
    let end = inner.find("}}")?;
    (end > 0).then(|| (&inner[..end], &inner[end + 2..]))
}

/// 一个变量（`{{` 与 `}}` 之间的部分）换成 Termlet 的写法。
fn convert_variable(inner: &str, changes: &mut Changes) -> String {
    let (name, default, more_options) = if let Some((name, options)) = inner.split_once('|') {
        // NyaTerm 只看第一个 `|` 之后到下一个 `|` 之间的选项
        let options: Vec<&str> = options
            .split('|')
            .next()
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .collect();
        (name.trim(), options[0], options.len() > 1)
    } else if let Some((name, default)) = inner.split_once('=') {
        // NyaTerm 只取第一个 `=` 与下一个 `=` 之间作默认值
        let default = default.split('=').next().unwrap_or_default();
        (name.trim(), default.trim(), false)
    } else {
        (inner.trim(), "", false)
    };
    if !is_param_name(name) || default.contains(['{', '}']) {
        changes.unknown_params = true;
        return format!("{{{{{inner}}}}}");
    }
    changes.options |= more_options;
    if default.is_empty() {
        format!("{{{{{name}}}}}")
    } else {
        format!("{{{{{name}:{default}}}}}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::files::tests::env;

    /// 测试样例按 NyaTerm 提交 daedb58（2.0.7）的导出格式（`QuickCommandsExportConfig`）与文档的写法手写。
    fn testdata(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/import/testdata/nyaterm")
            .join(name)
    }

    fn convert(text: &str) -> (String, Changes) {
        let mut changes = Changes::default();
        (script(text, &mut changes), changes)
    }

    fn snippets(plan: &ImportPlan) -> Vec<(&str, &str, &str)> {
        plan.snippets
            .iter()
            .map(|s| (s.group.as_str(), s.name.as_str(), s.content.as_str()))
            .collect()
    }

    // 导出的样例：分组路径、排序、内容的转换、跳过与说明
    #[test]
    fn export_sample() {
        let plan = read(&testdata("nyaterm-quick-commands.json"), &env()).unwrap();
        assert_eq!(plan.source, Source::NyaTerm);
        assert!(plan.hosts.is_empty() && plan.groups.is_empty() && plan.keys.is_empty());
        assert_eq!(
            snippets(&plan),
            [
                ("运维", "磁盘", "df -h\n"),
                // 同一分组中置顶的在前，再按 sort_order
                (
                    "运维 / Kubernetes",
                    "看 Pod",
                    "kubectl get pods -n {{命名空间:default}}\n"
                ),
                (
                    "运维 / Kubernetes",
                    "日志",
                    "kubectl logs -f {{pod}} -c {{容器:app}}"
                ),
                ("网络设备", "登录交换机", "admin\nsecret\ndisplay version\n"),
                ("网络设备", "逐行", "show run\nshow ip int brief\n"),
                (
                    "",
                    "转义",
                    "printf 'a\tb\\n' | grep -E '\\d+' > C:\\Users\\x.txt\n"
                ),
                ("", "原样", "echo {{name}} \\r\n"),
                ("", "逐字", "y"),
                ("", "Docker 名称", "docker ps --format '{{.Names}}'\n"),
                ("", "多行", "cd /srv/app\ngit pull\n"),
                ("", "新版本的字段", "echo 未来\n"),
                ("孤儿", "孤儿分类", "uptime\n"),
                ("环 B / 环 A", "成环的分类", "w\n"),
                ("deleted-group", "分类已删除", "id\n"),
            ]
        );
        // 只有暂停、空白、label 不是字符串的跳过
        assert_eq!(plan.skipped, 3);
        assert_eq!(
            plan.snippet_notes,
            SnippetNotes {
                pauses: 1,
                options: 1,
                unknown_params: 1,
                literal_params: 1,
                paced: 2,
                descriptions: 1,
                decorations: 3,
                nested_groups: 3,
            }
        );
    }

    // NyaTerm 文档中手写的写法：用分类名、省略 id；顶层直接是命令数组
    #[test]
    fn handwritten_files() {
        let plan = read(&testdata("handwritten.json"), &env()).unwrap();
        assert_eq!(
            snippets(&plan),
            [
                ("General", "List files", "ls -la\n"),
                ("Kubernetes", "Kubernetes pods", "kubectl get pods -A"),
            ]
        );
        let plan =
            parse(r#"[{"label":"pwd","command":"pwd"},{"label":"空","command":""}]"#).unwrap();
        assert_eq!(snippets(&plan), [("", "pwd", "pwd\n")]);
        assert_eq!(plan.skipped, 1);
        assert!(plan.snippet_notes.is_empty());
    }

    #[test]
    fn not_nyaterm() {
        for text in [
            "",
            "not json",
            r#"{"hosts":[]}"#,
            r#"{"commands":{}}"#,
            "42",
        ] {
            assert!(
                matches!(parse(text), Err(ImportError::NotNyaTerm(_))),
                "{text}"
            );
        }
        // 有 commands 但一条也读不出：照常返回，界面说明没有可以导入的
        let plan = parse(r#"{"commands":[1, {"label":"x"}]}"#).unwrap();
        assert!(plan.snippets.is_empty());
        assert_eq!(plan.skipped, 2);
    }

    // 转义按 NyaTerm 的规则；暂停去掉；`\r\p` 之后的换行（含 CRLF、连续几个）是排版
    #[test]
    fn script_escapes() {
        let cases = [
            (r"a\rb", "a\nb"),
            (r"a\nb\tc\\d", "a\nb\tc\\d"),
            (r"grep '\d+' C:\Users", r"grep '\d+' C:\Users"),
            (r"end\", r"end\"),
            ("admin\\r\\p\nsecret", "admin\nsecret"),
            ("admin\\r\\p\\p\r\n\nsecret", "admin\nsecret"),
            // 暂停前不是回车：换行照常发送
            ("a\\p\nb", "a\nb"),
            ("a\\n\\p\nb", "a\n\nb"),
            ("一\\p二", "一二"),
        ];
        for (text, want) in cases {
            assert_eq!(convert(text).0, want, "{text}");
        }
        assert!(convert(r"a\pb").1.pauses);
        assert_eq!(convert(r"a\rb").1, Changes::default());
    }

    // 变量：`=` 换成 `:`，下拉选项保留第一个；名称认不出的按原文；变量中的反斜杠不按转义处理
    #[test]
    fn variables() {
        let cases = [
            ("{{主机}}", "{{主机}}"),
            ("{{ 端口 = 22 }}", "{{端口:22}}"),
            ("{{a=b=c}}", "{{a:b}}"),
            ("{{路径=C:\\temp}}", "{{路径:C:\\temp}}"),
            ("{{env|prod}}", "{{env:prod}}"),
            ("{{env| prod , test }}", "{{env:prod}}"),
            ("{{x}}{{x=1}}", "{{x}}{{x:1}}"),
            ("{{}} {x}", "{{}} {x}"),
            ("{{.Names}}", "{{.Names}}"),
            ("{{a/b}}", "{{a/b}}"),
            ("{{a=x{y}}", "{{a=x{y}}"),
            ("{{a", "{{a"),
        ];
        for (text, want) in cases {
            assert_eq!(convert(text).0, want, "{text}");
        }
        assert!(convert("{{env|a,b}}").1.options);
        assert!(!convert("{{env|a}}").1.options);
        assert!(convert("{{.Names}}").1.unknown_params);
        assert!(!convert("{{主机}}").1.unknown_params);
    }

    // 执行方式：execute、逐行发送补换行；append、逐字发送不补；原样发送的不转义
    #[test]
    fn execution() {
        use serde_json::json;
        let content = |value: Value| {
            let o = value.as_object().unwrap().clone();
            let mut notes = SnippetNotes::default();
            command(&o, &Categories::new(&[]), &mut notes).map(|c| c.snippet.content)
        };
        assert_eq!(
            content(json!({"label":"a","command":"ls"})).unwrap(),
            "ls\n"
        );
        assert_eq!(
            content(json!({"label":"a","command":"ls\\r","execution_mode":"execute"})).unwrap(),
            "ls\n",
            "已以回车结尾的不再补"
        );
        assert_eq!(
            content(json!({"label":"a","command":"ls","execution_mode":"append"})).unwrap(),
            "ls"
        );
        assert_eq!(
            content(json!({"label":"a","command":"ls","execution_mode":"append",
                "segmented_send":{"mode":"line","interval_ms":1000}}))
            .unwrap(),
            "ls\n"
        );
        assert_eq!(
            content(json!({"label":"a","command":"ls","segmented_send":{"mode":"bytes"}})).unwrap(),
            "ls",
            "认不出的分段发送不执行"
        );
        assert_eq!(
            content(json!({"label":"a","command":"a\\tb {{x=1}}","raw_send":true})).unwrap(),
            "a\\tb {{x=1}}\n"
        );
        assert_eq!(
            content(json!({"label":"a","command":"x".repeat(MAX_CONTENT)})),
            None,
            "补上换行后超长"
        );
    }

    // 分组：名称截到 64 字符；分类没有名称的不算
    #[test]
    fn groups() {
        let categories = Categories::new(&[
            serde_json::json!({"id":"a","name":"甲".repeat(40)}),
            serde_json::json!({"id":"b","name":"乙".repeat(40),"parent_id":"a"}),
            serde_json::json!({"id":"c","name":"  "}),
            serde_json::json!({"name":"没有 id"}),
        ]);
        assert_eq!(categories.0.len(), 2);
        assert_eq!(categories.path("c"), None);
        let o = serde_json::json!({"label":"x","command":"ls","category_id":"b"});
        let snippet = command(
            o.as_object().unwrap(),
            &categories,
            &mut SnippetNotes::default(),
        )
        .unwrap()
        .snippet;
        assert_eq!(snippet.group.chars().count(), 64);
        assert!(snippet.group.starts_with("甲甲"));
    }

    // 编码：UTF-8（可带 BOM），不是 UTF-8 时按系统的代码页（测试环境为 GBK）
    #[test]
    fn encodings() {
        let dir = tempfile::tempdir().unwrap();
        let text = r#"{"commands":[{"label":"中文","command":"echo 你好"}]}"#;
        let path = dir.path().join("a.json");
        std::fs::write(&path, [&[0xEF, 0xBB, 0xBF][..], text.as_bytes()].concat()).unwrap();
        assert_eq!(read(&path, &env()).unwrap().snippets[0].name, "中文");
        let (gbk, _, _) = encoding_rs::GBK.encode(text);
        std::fs::write(&path, &gbk).unwrap();
        assert_eq!(
            read(&path, &env()).unwrap().snippets[0].content,
            "echo 你好\n"
        );
    }

    // 默认位置：先找 NyaTerm 配置目录中的导出，没有时找“下载”等处最新的
    #[test]
    fn default_path_prefers_the_config_dir() {
        let dir = tempfile::tempdir().unwrap();
        let downloads = dir.path().join("Downloads");
        std::fs::create_dir_all(&downloads).unwrap();
        let mut env = env();
        env.home = Some(dir.path().to_path_buf());
        env.downloads = Some(downloads.clone());
        assert_eq!(default_path(&env), None);
        let older = downloads.join("nyaterm-quick-commands.json");
        let newer = downloads.join("NyaTerm-Quick-Commands (1).json");
        std::fs::write(&older, "[]").unwrap();
        std::fs::write(&newer, "[]").unwrap();
        std::fs::write(downloads.join("other.json"), "[]").unwrap();
        let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&older)
            .unwrap()
            .set_modified(old_time)
            .unwrap();
        assert_eq!(default_path(&env), Some(newer));
        let config = dir.path().join(".nyaterm");
        std::fs::create_dir_all(&config).unwrap();
        let exported = config.join("nyaterm-quick-commands.json");
        std::fs::write(&exported, "[]").unwrap();
        assert_eq!(default_path(&env), Some(exported));
    }
}
