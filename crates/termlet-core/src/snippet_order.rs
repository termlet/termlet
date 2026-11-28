//! 快捷命令的显示顺序：侧边栏的“快捷命令”页、快捷命令栏、选择器与导出都按它排。
//!
//! - 未分组的在最前，不参与分组的排序；
//! - 接着是保存的分组顺序（[`SnippetSettings::group_order`]，拖动分组时写入）中还有快捷命令的分组；
//! - 其余的分组（之后新建、导入或其他设备新加的）按名称排在后面。没有拖动过分组时全部按名称，与以前相同；
//! - 同一分组中按条目的排序键（拖动快捷命令时改的就是它），也就是存储列出的顺序。
//!
//! [`SnippetSettings::group_order`]: crate::model::SnippetSettings::group_order

use std::collections::{BTreeMap, HashSet};

use crate::model::Snippet;
use crate::store::Record;

/// 分组名按显示顺序排列（有未分组的快捷命令时空串在最前）。`saved` 是保存的分组顺序。
pub fn group_names<'a>(
    snippets: impl IntoIterator<Item = &'a Record<Snippet>>,
    saved: &[String],
) -> Vec<String> {
    grouped(snippets, saved)
        .into_iter()
        .map(|(group, _)| group)
        .collect()
}

/// 按分组归类，分组按显示顺序，组内保持传入的先后（传入存储列出的顺序即按排序键）。
pub fn grouped<'a>(
    snippets: impl IntoIterator<Item = &'a Record<Snippet>>,
    saved: &[String],
) -> Vec<(String, Vec<&'a Record<Snippet>>)> {
    let mut by_name: BTreeMap<&'a str, Vec<&'a Record<Snippet>>> = BTreeMap::new();
    for snippet in snippets {
        by_name
            .entry(snippet.doc.group.as_str())
            .or_default()
            .push(snippet);
    }
    let mut out = Vec::with_capacity(by_name.len());
    if let Some(ungrouped) = by_name.remove("") {
        out.push((String::new(), ungrouped));
    }
    let mut seen = HashSet::new();
    for name in saved {
        if seen.insert(name.as_str())
            && let Some(members) = by_name.remove(name.as_str())
        {
            out.push((name.clone(), members));
        }
    }
    out.extend(
        by_name
            .into_iter()
            .map(|(name, members)| (name.to_string(), members)),
    );
    out
}

/// 全部快捷命令按显示顺序排成一列（快捷命令栏的“全部”、选择器）。
pub fn ordered(snippets: &[Record<Snippet>], saved: &[String]) -> Vec<Record<Snippet>> {
    grouped(snippets, saved)
        .into_iter()
        .flat_map(|(_, members)| members)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::SnippetId;

    fn record(id: &str, group: &str, sort: &str) -> Record<Snippet> {
        Record {
            id: SnippetId::from_raw(id.to_string()),
            parent: None,
            sort_key: sort.into(),
            doc: Snippet {
                name: id.into(),
                group: group.into(),
                content: "ls\n".into(),
                ..Default::default()
            },
            created_at: 0,
            updated_at: 0,
        }
    }

    fn ids(list: &[Record<Snippet>]) -> Vec<&str> {
        list.iter().map(|s| s.id.as_str()).collect()
    }

    // 没有保存过分组顺序（升级前的数据）：未分组在前，分组按名称，组内按传入的先后，与以前的快捷命令页相同
    #[test]
    fn default_order_is_by_name() {
        let list = vec![
            record("b1", "乙", "a"),
            record("n1", "", "b"),
            record("a1", "甲", "c"),
            record("b2", "乙", "d"),
            record("o1", "ops", "e"),
        ];
        assert_eq!(group_names(&list, &[]), ["", "ops", "乙", "甲"]);
        assert_eq!(ids(&ordered(&list, &[])), ["n1", "o1", "b1", "b2", "a1"]);
    }

    // 保存的顺序优先；已经没有快捷命令的名称、重复的名称、空串忽略；不在其中的分组按名称排在后面
    #[test]
    fn saved_order_first_then_the_rest_by_name() {
        let list = vec![
            record("a1", "甲", "a"),
            record("b1", "乙", "b"),
            record("c1", "丙", "c"),
            record("n1", "", "d"),
            record("z1", "新", "e"),
            record("d1", "丁", "f"),
        ];
        let saved: Vec<String> = ["乙", "不在了", "", "甲", "乙"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            group_names(&list, &saved),
            ["", "乙", "甲", "丁", "丙", "新"]
        );
        let grouped = grouped(&list, &saved);
        assert_eq!(grouped[1].1.len(), 1);
        assert_eq!(
            ids(&ordered(&list, &saved)),
            ["n1", "b1", "a1", "d1", "c1", "z1"]
        );
    }
}
