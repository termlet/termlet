//! 只有快捷命令的来源：Termlet 导出的快捷命令（格式见 [`crate::snippetfile`]）与 NyaTerm 导出的快捷命令（见
//! [`super::nyaterm`]）。两个来源都按文件的内容识别格式：`format` 为 `termlet-snippets` 的按 Termlet 的读，其余按
//! NyaTerm 的读；计划的来源是识别出的格式（界面据此改选来源），读不出时按用户选的来源说明。

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::files::{LocalEnv, decode_text, newest_export, read_limited};
use super::plan::PlanBuilder;
use super::{ImportError, ImportPlan, Source, nyaterm};
use crate::snippetfile;

/// 文件的大小上限。
const MAX_FILE: u64 = 16 << 20;

pub(super) fn read(source: Source, path: &Path, env: &LocalEnv) -> Result<ImportPlan, ImportError> {
    let path = env.input_path(path);
    let raw = read_limited(&path, MAX_FILE)?;
    parse(source, &decode_text(&raw, env.ansi))
}

pub(super) fn parse(source: Source, text: &str) -> Result<ImportPlan, ImportError> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| not_snippets(source, e.to_string()))?;
    if snippetfile::is_snippet_file(&value) {
        let (snippets, skipped) = snippetfile::parse(&value)?;
        let mut plan = PlanBuilder::new(Source::TermletSnippets);
        for snippet in snippets {
            plan.add_snippet(snippet);
        }
        plan.skip(skipped);
        return Ok(plan.finish());
    }
    nyaterm::parse_value(value).map_err(|e| match e {
        ImportError::NotNyaTerm(reason) => not_snippets(source, reason),
        e => e,
    })
}

/// 文件不是快捷命令时的错误：按用户选的来源说明。
fn not_snippets(source: Source, reason: String) -> ImportError {
    if source == Source::TermletSnippets {
        ImportError::NotTermletSnippets(reason)
    } else {
        ImportError::NotNyaTerm(reason)
    }
}

/// Termlet 导出的默认位置：“下载”“桌面”“文档”中最新的 `termlet-snippets*.json`（另存时可能带了序号）。
pub(super) fn default_path(env: &LocalEnv) -> Option<PathBuf> {
    newest_export(env, |name| {
        name.starts_with(snippetfile::FORMAT_NAME) && name.ends_with(".json")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::files::tests::env;
    use crate::model::Snippet;

    const TERMLET: &str = r#"{"format":"termlet-snippets","version":1,"exportedAt":1,"app":"9.9.9",
        "snippets":[{"name":"磁盘","group":"运维","content":"df -h","run":true}]}"#;
    const NYATERM: &str = r#"{"commands":[{"label":"磁盘","command":"df -h","category":"运维"}]}"#;

    fn brief(plan: &ImportPlan) -> Vec<(&str, &str, &str)> {
        plan.snippets
            .iter()
            .map(|s| (s.group.as_str(), s.name.as_str(), s.content.as_str()))
            .collect()
    }

    // 两个来源都认得出两种格式，计划的来源是识别出的格式
    #[test]
    fn either_source_detects_the_format() {
        for source in [Source::TermletSnippets, Source::NyaTerm] {
            let plan = parse(source, TERMLET).unwrap();
            assert_eq!(plan.source, Source::TermletSnippets);
            assert_eq!(brief(&plan), [("运维", "磁盘", "df -h\n")]);
            let plan = parse(source, NYATERM).unwrap();
            assert_eq!(plan.source, Source::NyaTerm);
            assert_eq!(brief(&plan), [("运维", "磁盘", "df -h\n")]);
        }
    }

    // 读不出时按选的来源说明；更新的版本、坏掉的 Termlet 文件不当作 NyaTerm 的
    #[test]
    fn errors_follow_the_chosen_source() {
        for text in ["not json", r#"{"hosts":[]}"#] {
            assert!(matches!(
                parse(Source::TermletSnippets, text),
                Err(ImportError::NotTermletSnippets(_))
            ));
            assert!(matches!(
                parse(Source::NyaTerm, text),
                Err(ImportError::NotNyaTerm(_))
            ));
        }
        let newer = r#"{"format":"termlet-snippets","version":3,"snippets":[]}"#;
        for source in [Source::TermletSnippets, Source::NyaTerm] {
            assert!(matches!(
                parse(source, newer),
                Err(ImportError::SnippetsTooNew { found: 3, .. })
            ));
            assert!(matches!(
                parse(source, r#"{"format":"termlet-snippets","version":1}"#),
                Err(ImportError::NotTermletSnippets(_))
            ));
        }
    }

    // 不认识的字段、空的条目：照常读，跳过的计数
    #[test]
    fn unknown_fields_and_skipped_entries() {
        let text = r#"{"format":"termlet-snippets","version":1,"note":"新版本的字段",
            "snippets":[{"name":"a","content":"ls","run":false,"icon":"x"},{"name":"b"}]}"#;
        let plan = parse(Source::TermletSnippets, text).unwrap();
        assert_eq!(brief(&plan), [("", "a", "ls")]);
        assert_eq!(plan.skipped, 1);
        assert!(plan.snippet_notes.is_empty());
        assert!(plan.hosts.is_empty() && plan.groups.is_empty());
    }

    // 读文件：UTF-8 带 BOM；导出的文件原样读回
    #[test]
    fn reads_an_exported_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("termlet-snippets-2026-11-26.json");
        let original = Snippet {
            name: "中文".into(),
            group: "组 / 子组".into(),
            content: "echo {{名称:默认}}\necho 你好\n".into(),
            ..Default::default()
        };
        let json = snippetfile::to_json(&[&original]);
        std::fs::write(&path, [&[0xEF, 0xBB, 0xBF][..], json.as_bytes()].concat()).unwrap();
        let plan = read(Source::TermletSnippets, &path, &env()).unwrap();
        assert_eq!(plan.snippets, [original]);
    }

    // 默认位置：“下载”等处最新的 termlet-snippets*.json
    #[test]
    fn default_path_is_the_newest_export() {
        let dir = tempfile::tempdir().unwrap();
        let downloads = dir.path().join("Downloads");
        std::fs::create_dir_all(&downloads).unwrap();
        let mut env = env();
        env.downloads = Some(downloads.clone());
        assert_eq!(default_path(&env), None);
        let file = downloads.join("termlet-snippets-2026-11-26 (1).json");
        std::fs::write(&file, "{}").unwrap();
        std::fs::write(downloads.join("termlet-hosts-2026.json"), "{}").unwrap();
        assert_eq!(default_path(&env), Some(file));
    }
}
