//! 导入预览的筛选与勾选：按关键词与状态筛选；
//! 作用于当前筛选结果的全选、全不选、只选新主机、反选；Shift 单击连续勾选（把上次单击的行到这一行都设为与这一行
//! 相同的状态，与 Gmail 相同）。勾选的集合由界面保存，执行导入时传入。

use std::collections::HashSet;

use super::PreviewItem;

/// 按状态筛选。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StatusFilter {
    #[default]
    All,
    /// 本机还没有的主机
    New,
    /// 本机已有相同的主机
    Duplicate,
}

/// 批量勾选，只作用于当前筛选结果，其他行的勾选不变。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BulkAction {
    All,
    None,
    /// 只选新主机：已存在的取消勾选
    New,
    Invert,
}

/// 预览后默认的勾选：新主机全选，已存在的不选。
pub fn default_selection(items: &[PreviewItem]) -> HashSet<String> {
    items
        .iter()
        .filter(|i| i.duplicate_of.is_none())
        .map(|i| i.key.clone())
        .collect()
}

/// 按关键词（名称、地址、用户名、分组、标签，不区分大小写，空白分开的多个词都要出现）与状态筛选，保持原来的顺序。
pub fn filter<'a>(
    items: &'a [PreviewItem],
    query: &str,
    status: StatusFilter,
) -> Vec<&'a PreviewItem> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    items
        .iter()
        .filter(|it| match status {
            StatusFilter::All => true,
            StatusFilter::New => it.duplicate_of.is_none(),
            StatusFilter::Duplicate => it.duplicate_of.is_some(),
        })
        .filter(|it| {
            if words.is_empty() {
                return true;
            }
            let text = [
                it.name.as_str(),
                it.address.as_str(),
                it.username.as_str(),
                &it.group.join("/"),
            ]
            .into_iter()
            .chain(it.tags.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
            words.iter().all(|w| text.contains(w.as_str()))
        })
        .collect()
}

/// 批量勾选当前筛选结果 `visible`。
pub fn bulk_select(selected: &mut HashSet<String>, visible: &[&PreviewItem], action: BulkAction) {
    let before = selected.clone();
    for it in visible {
        let on = match action {
            BulkAction::All => true,
            BulkAction::None => false,
            BulkAction::New => it.duplicate_of.is_none(),
            BulkAction::Invert => !before.contains(&it.key),
        };
        if on {
            selected.insert(it.key.clone());
        } else {
            selected.remove(&it.key);
        }
    }
}

/// 单击一行：切换这一行；按住 Shift 且有上次单击的行（在当前筛选结果中）时，把两行之间（含）都设为这一行切换后的状态。
/// `keys` 是当前筛选结果按显示顺序的键，`anchor` 是上次单击的行。
pub fn click(
    selected: &mut HashSet<String>,
    keys: &[&str],
    anchor: Option<&str>,
    key: &str,
    shift: bool,
) {
    let on = !selected.contains(key);
    let from = anchor.and_then(|a| keys.iter().position(|k| *k == a));
    let to = keys.iter().position(|k| *k == key);
    let range: Vec<&str> = match (shift, from, to) {
        (true, Some(i), Some(j)) => keys[i.min(j)..=i.max(j)].to_vec(),
        _ => vec![key],
    };
    for k in range {
        if on {
            selected.insert(k.to_string());
        } else {
            selected.remove(k);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::HostId;
    use crate::model::Protocol;

    fn item(key: &str, group: &str, tags: &[&str], username: &str, duplicate: bool) -> PreviewItem {
        PreviewItem {
            key: key.into(),
            name: key.into(),
            protocol: Protocol::Ssh,
            address: format!("{key}.example"),
            port: 22,
            username: username.into(),
            group: group.split('/').map(String::from).collect(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            jump: None,
            has_password: false,
            has_key: false,
            duplicate_of: duplicate.then(|| HostId::from_raw("h_1")),
            hints: Vec::new(),
            proxy_command: None,
            startup: None,
            forwards: Vec::new(),
        }
    }

    fn items() -> Vec<PreviewItem> {
        vec![
            item("web-1", "生产/web", &["nginx"], "root", false),
            item("web-2", "生产/web", &[], "root", true),
            item("db-1", "生产/db", &[], "postgres", false),
            item("test", "测试", &[], "root", true),
        ]
    }

    fn keys(list: &[&PreviewItem]) -> Vec<String> {
        list.iter().map(|i| i.key.clone()).collect()
    }

    fn sorted(set: &HashSet<String>) -> Vec<String> {
        let mut v: Vec<_> = set.iter().cloned().collect();
        v.sort();
        v
    }

    fn set(keys: &[&str]) -> HashSet<String> {
        keys.iter().map(|k| k.to_string()).collect()
    }

    #[test]
    fn filter_by_words_and_status() {
        let items = items();
        assert_eq!(
            keys(&filter(&items, "生产", StatusFilter::All)),
            ["web-1", "web-2", "db-1"]
        );
        assert_eq!(
            keys(&filter(&items, "WEB 生产", StatusFilter::All)),
            ["web-1", "web-2"]
        );
        assert_eq!(
            keys(&filter(&items, "postgres", StatusFilter::All)),
            ["db-1"]
        );
        assert_eq!(keys(&filter(&items, "nginx", StatusFilter::All)), ["web-1"]);
        assert_eq!(filter(&items, "  ", StatusFilter::All).len(), 4);
        assert_eq!(
            keys(&filter(&items, "", StatusFilter::New)),
            ["web-1", "db-1"]
        );
        assert_eq!(
            keys(&filter(&items, "", StatusFilter::Duplicate)),
            ["web-2", "test"]
        );
        assert_eq!(
            keys(&filter(&items, "生产", StatusFilter::Duplicate)),
            ["web-2"]
        );
        assert_eq!(sorted(&default_selection(&items)), ["db-1", "web-1"]);
    }

    #[test]
    fn bulk_actions_apply_to_visible_rows_only() {
        let items = items();
        let visible = filter(&items, "生产", StatusFilter::All);
        let mut s = set(&["test"]);
        bulk_select(&mut s, &visible, BulkAction::All);
        assert_eq!(sorted(&s), ["db-1", "test", "web-1", "web-2"]);
        let mut s = set(&["test", "web-1"]);
        bulk_select(&mut s, &visible, BulkAction::None);
        assert_eq!(sorted(&s), ["test"]);
        let all: Vec<&PreviewItem> = items.iter().collect();
        let mut s = set(&["web-2"]);
        bulk_select(&mut s, &all, BulkAction::New);
        assert_eq!(sorted(&s), ["db-1", "web-1"]);
        let mut s = set(&["web-1", "test"]);
        bulk_select(&mut s, &visible, BulkAction::Invert);
        assert_eq!(sorted(&s), ["db-1", "test", "web-2"]);
    }

    #[test]
    fn click_toggles_and_shift_click_selects_ranges() {
        let order = ["web-1", "web-2", "db-1", "test"];
        let mut s = HashSet::new();
        click(&mut s, &order, None, "db-1", false);
        assert_eq!(sorted(&s), ["db-1"]);
        click(&mut s, &order, Some("web-1"), "db-1", false);
        assert!(s.is_empty());

        let mut s = HashSet::new();
        click(&mut s, &order, Some("web-1"), "db-1", true);
        assert_eq!(sorted(&s), ["db-1", "web-1", "web-2"]);
        // 反方向，且取消勾选
        let mut s = set(&order);
        click(&mut s, &order, Some("test"), "web-2", true);
        assert_eq!(sorted(&s), ["web-1"]);
        // 没有起点或起点不在当前筛选结果中时只切换这一行
        let mut s = HashSet::new();
        click(&mut s, &order, None, "db-1", true);
        assert_eq!(sorted(&s), ["db-1"]);
        let mut s = HashSet::new();
        click(&mut s, &["db-1", "test"], Some("web-1"), "test", true);
        assert_eq!(sorted(&s), ["test"]);
    }
}
