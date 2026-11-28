//! 快捷命令的分组与顺序：拖动排序、分组的重命名与整组删除（显示顺序见 [`crate::snippet_order`]）。
//!
//! 分组只是快捷命令上的名称（[`Snippet::group`]），没有自己的条目：改名、删除改的是其中的每一条快捷命令，分组的先后
//! 另存在设置组 [`SnippetSettings`] 中。每个操作在一个事务中写入快捷命令与设置，内存只改一次、事件只发一次。
//! 快捷命令都在同一层（没有上级），排序键在全部快捷命令之间比较，同一分组中的先后就是排序键的先后；同步时与主机
//! 相同，按条目合并（只改了被拖动的那一条）。
//!
//! [`SnippetSettings`]: crate::model::SnippetSettings

use std::sync::RwLockWriteGuard;

use super::{Position, Record, State, Store, StoreEvent, collection, settings_store};
use crate::id::SnippetId;
use crate::model::{Kind, Snippet};
use crate::{CoreError, Invalid, Result, sort_key};

/// 分组改名的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupRename {
    /// 改了几条快捷命令
    pub moved: usize,
    /// 新名称是已有的分组，并了进去
    pub merged: bool,
}

impl Store {
    /// 全部快捷命令，按显示顺序排列。
    pub fn snippets_in_order(&self) -> Vec<Record<Snippet>> {
        let state = self.read();
        crate::snippet_order::ordered(
            &state.snippets.sorted(),
            &state.settings.synced.snippets.group_order,
        )
    }

    /// 把快捷命令移到分组 `group`（空串为未分组）中的 `position`：拖动排序，跨组即改分组。位置在同一分组中
    /// 没有变化时什么都不写。
    pub fn move_snippet(
        &self,
        id: &SnippetId,
        group: &str,
        position: Position<SnippetId>,
    ) -> Result<()> {
        let mut state = self.write();
        let mut record = state
            .snippets
            .get(id)
            .cloned()
            .ok_or(CoreError::NotFound(Kind::Snippet.name()))?;
        let mut doc = record.doc.clone();
        doc.group = group.to_string();
        doc.normalize()?;
        let key = state.snippets.key_for(None, &position, Some(id));
        if doc.group == record.doc.group && record.parent.is_none() {
            // 新旧位置之间没有同组的快捷命令：组内的先后不变
            let (low, high) = if key < record.sort_key {
                (key.as_str(), record.sort_key.as_str())
            } else {
                (record.sort_key.as_str(), key.as_str())
            };
            let passes = state.snippets.iter().any(|other| {
                &other.id != id
                    && other.doc.group == doc.group
                    && other.sort_key.as_str() >= low
                    && other.sort_key.as_str() <= high
            });
            if !passes {
                return Ok(());
            }
        }
        record.doc = doc;
        record.sort_key = key;
        record.parent = None;
        record.updated_at = crate::time::now_ms();
        self.db
            .write(|tx| collection::put(tx, &record))
            .inspect_err(|e| tracing::error!(id = %id, "移动快捷命令失败：{e}"))?;
        tracing::debug!(id = %id, "移动快捷命令");
        state.snippets.insert(record);
        drop(state);
        self.emit(StoreEvent::Changed(Kind::Snippet));
        Ok(())
    }

    /// 把分组 `from` 改名为 `to`（去掉首尾空白）。`to` 是已有的分组时并进去：移过去的排在它原有的快捷命令之后，
    /// 彼此的先后不变。未分组不能改名，也不能改成空的。
    pub fn rename_snippet_group(&self, from: &str, to: &str) -> Result<GroupRename> {
        let to = to.trim();
        if from.is_empty() {
            return Err(Invalid::new("group", "ungrouped").into());
        }
        if to.is_empty() {
            return Err(Invalid::new("group", "required").into());
        }
        let state = self.write();
        let members = members(&state, from);
        if members.is_empty() {
            return Err(CoreError::NotFound(Kind::Snippet.name()));
        }
        if to == from {
            return Ok(GroupRename {
                moved: 0,
                merged: false,
            });
        }
        let merged = state.snippets.iter().any(|s| s.doc.group == to);
        let records = regroup(&state, members, to, merged)?;
        let mut order = state.settings.synced.snippets.group_order.clone();
        if merged {
            order.retain(|g| g != from);
        } else {
            for name in &mut order {
                if name == from {
                    *name = to.to_string();
                }
            }
        }
        let moved = records.len();
        self.write_group_change(state, records, &[], order)?;
        tracing::info!(moved, merged, "重命名快捷命令分组");
        Ok(GroupRename { moved, merged })
    }

    /// 删除分组，返回其中有几条快捷命令：`keep` 为 true 时把它们移到未分组的最后（彼此的先后不变），否则一并删除。
    pub fn delete_snippet_group(&self, group: &str, keep: bool) -> Result<usize> {
        if group.is_empty() {
            return Err(Invalid::new("group", "ungrouped").into());
        }
        let state = self.write();
        let members = members(&state, group);
        if members.is_empty() {
            return Err(CoreError::NotFound(Kind::Snippet.name()));
        }
        let count = members.len();
        let (records, removed) = if keep {
            (regroup(&state, members, "", true)?, Vec::new())
        } else {
            (Vec::new(), members.into_iter().map(|r| r.id).collect())
        };
        let mut order = state.settings.synced.snippets.group_order.clone();
        order.retain(|g| g != group);
        self.write_group_change(state, records, &removed, order)?;
        tracing::info!(count, keep, "删除快捷命令分组");
        Ok(count)
    }

    /// 在一个事务中写入改过的快捷命令、删除 `removed`、保存分组顺序，再改内存、放开写锁、发出事件。
    fn write_group_change(
        &self,
        mut state: RwLockWriteGuard<'_, State>,
        records: Vec<Record<Snippet>>,
        removed: &[SnippetId],
        order: Vec<String>,
    ) -> Result<()> {
        let mut settings = state.settings.synced.clone();
        settings.snippets.group_order = order;
        let rows = settings_store::changed_rows(&state.settings.synced, &settings)?;
        let now = crate::time::now_ms();
        self.db
            .write(|tx| {
                for record in &records {
                    collection::put(tx, record)?;
                }
                for id in removed {
                    collection::put_tombstone(tx, Kind::Snippet, id.as_str(), now)?;
                }
                for row in &rows {
                    crate::db::put_setting(tx, "setting", row)?;
                }
                Ok(())
            })
            .inspect_err(|e| tracing::error!("修改快捷命令分组失败：{e}"))?;
        for record in records {
            state.snippets.insert(record);
        }
        for id in removed {
            state.snippets.remove(id.as_str(), now);
        }
        state.settings.synced = settings;
        drop(state);
        self.emit(StoreEvent::Changed(Kind::Snippet));
        if !rows.is_empty() {
            self.emit(StoreEvent::SettingsChanged);
        }
        Ok(())
    }
}

/// 分组中的快捷命令，按排序键。
fn members(state: &State, group: &str) -> Vec<Record<Snippet>> {
    state
        .snippets
        .sorted()
        .into_iter()
        .filter(|r| r.doc.group == group)
        .collect()
}

/// 改到分组 `to` 之后的记录；`append` 时排到全部快捷命令的最后（也就是 `to` 原有的之后），保持彼此的先后。
fn regroup(
    state: &State,
    members: Vec<Record<Snippet>>,
    to: &str,
    append: bool,
) -> Result<Vec<Record<Snippet>>> {
    let now = crate::time::now_ms();
    let mut last = state.snippets.sorted().last().map(|r| r.sort_key.clone());
    members
        .into_iter()
        .map(|mut record| {
            record.doc.group = to.to_string();
            record.doc.normalize()?;
            if append {
                let key = sort_key::between(last.as_deref(), None);
                last = Some(key.clone());
                record.sort_key = key;
                record.parent = None;
            }
            record.updated_at = now;
            Ok(record)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::memory_store;

    fn add(store: &Store, group: &str, name: &str) -> SnippetId {
        store
            .create(
                Snippet {
                    name: name.into(),
                    group: group.into(),
                    content: format!("echo {name}\n"),
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap()
    }

    /// 按显示顺序：每个分组一行，`分组: 名称 名称`（未分组写作 `-`）。
    fn layout(store: &Store) -> Vec<String> {
        let list = store.list::<Snippet>();
        let order = store.settings().snippets.group_order;
        crate::snippet_order::grouped(&list, &order)
            .into_iter()
            .map(|(group, members)| {
                let names: Vec<&str> = members.iter().map(|s| s.doc.name.as_str()).collect();
                let group = if group.is_empty() { "-" } else { &group };
                format!("{group}: {}", names.join(" "))
            })
            .collect()
    }

    // 升级前的数据（创建顺序交错在几个分组中，没有保存过分组顺序）：与以前一样，未分组在前、分组按名称、组内按创建顺序
    #[test]
    fn data_from_before_keeps_its_order() {
        let store = memory_store();
        add(&store, "乙", "b1");
        add(&store, "", "n1");
        add(&store, "甲", "a1");
        add(&store, "乙", "b2");
        add(&store, "", "n2");
        assert_eq!(layout(&store), ["-: n1 n2", "乙: b1 b2", "甲: a1"]);
        let names: Vec<String> = store
            .snippets_in_order()
            .into_iter()
            .map(|s| s.doc.name)
            .collect();
        assert_eq!(names, ["n1", "n2", "b1", "b2", "a1"]);
    }

    // 组内拖到前面、后面；放到原处（前后没有同组的）不写入
    #[test]
    fn move_within_a_group() {
        let store = memory_store();
        let a1 = add(&store, "a", "a1");
        let b1 = add(&store, "b", "b1");
        let a2 = add(&store, "a", "a2");
        let a3 = add(&store, "a", "a3");
        store
            .move_snippet(&a3, "a", Position::Before(a1.clone()))
            .unwrap();
        assert_eq!(layout(&store), ["a: a3 a1 a2", "b: b1"]);
        store
            .move_snippet(&a3, "a", Position::After(a2.clone()))
            .unwrap();
        assert_eq!(layout(&store), ["a: a1 a2 a3", "b: b1"]);
        // a2 放到 b1 后面：中间没有同组的，组内顺序不变，不写
        let before = store.get::<Snippet>(&a2).unwrap();
        store
            .move_snippet(&a2, "a", Position::After(b1.clone()))
            .unwrap();
        assert_eq!(store.get::<Snippet>(&a2).unwrap(), before);
    }

    // 跨组拖动即改分组，放在目标的前后；放到分组标题上排到最后；拖到未分组
    #[test]
    fn move_across_groups() {
        let store = memory_store();
        let a1 = add(&store, "a", "a1");
        let a2 = add(&store, "a", "a2");
        let b1 = add(&store, "b", "b1");
        let b2 = add(&store, "b", "b2");
        store
            .move_snippet(&a2, "b", Position::Before(b2.clone()))
            .unwrap();
        assert_eq!(layout(&store), ["a: a1", "b: b1 a2 b2"]);
        assert_eq!(store.get::<Snippet>(&a2).unwrap().doc.group, "b");
        store.move_snippet(&b1, "a", Position::Last).unwrap();
        assert_eq!(layout(&store), ["a: a1 b1", "b: a2 b2"]);
        store.move_snippet(&a1, "", Position::Last).unwrap();
        assert_eq!(layout(&store), ["-: a1", "a: b1", "b: a2 b2"]);
        assert!(matches!(
            store.move_snippet(&SnippetId::from_raw("s_none"), "a", Position::Last),
            Err(CoreError::NotFound(_))
        ));
    }

    // 改名：不重名时原地改名，分组顺序中的名称跟着改；与已有分组重名时并进去，接在原有的后面
    #[test]
    fn rename_and_merge() {
        let store = memory_store();
        add(&store, "a", "a1");
        add(&store, "b", "b1");
        add(&store, "a", "a2");
        add(&store, "c", "c1");
        add(&store, "b", "b2");
        store
            .update_settings(|s| s.snippets.group_order = vec!["c".into(), "a".into(), "b".into()])
            .unwrap();
        let renamed = store.rename_snippet_group("a", "  新 ").unwrap();
        assert_eq!(
            renamed,
            GroupRename {
                moved: 2,
                merged: false
            }
        );
        assert_eq!(layout(&store), ["c: c1", "新: a1 a2", "b: b1 b2"]);
        assert_eq!(store.settings().snippets.group_order, ["c", "新", "b"]);

        let merged = store.rename_snippet_group("新", "b").unwrap();
        assert_eq!(
            merged,
            GroupRename {
                moved: 2,
                merged: true
            }
        );
        assert_eq!(layout(&store), ["c: c1", "b: b1 b2 a1 a2"]);
        assert_eq!(store.settings().snippets.group_order, ["c", "b"]);

        assert!(
            store.rename_snippet_group("", "x").is_err(),
            "未分组不能改名"
        );
        assert!(
            store.rename_snippet_group("b", " ").is_err(),
            "不能改成空的"
        );
        assert!(store.rename_snippet_group("b", &"长".repeat(65)).is_err());
        assert!(matches!(
            store.rename_snippet_group("没有", "x"),
            Err(CoreError::NotFound(_))
        ));
        assert_eq!(layout(&store), ["c: c1", "b: b1 b2 a1 a2"], "失败时不变");
    }

    // 整组删除：移到未分组（排在未分组的最后）或一并删除；分组顺序中去掉它
    #[test]
    fn delete_group_both_ways() {
        let store = memory_store();
        add(&store, "a", "a1");
        add(&store, "", "n1");
        add(&store, "b", "b1");
        add(&store, "a", "a2");
        add(&store, "b", "b2");
        store
            .update_settings(|s| s.snippets.group_order = vec!["b".into(), "a".into()])
            .unwrap();
        assert_eq!(store.delete_snippet_group("a", true).unwrap(), 2);
        assert_eq!(layout(&store), ["-: n1 a1 a2", "b: b1 b2"]);
        assert_eq!(store.settings().snippets.group_order, ["b"]);

        let ids: Vec<SnippetId> = store
            .list::<Snippet>()
            .into_iter()
            .filter(|s| s.doc.group == "b")
            .map(|s| s.id)
            .collect();
        assert_eq!(store.delete_snippet_group("b", false).unwrap(), 2);
        assert_eq!(layout(&store), ["-: n1 a1 a2"]);
        assert!(store.settings().snippets.group_order.is_empty());
        // 留下墓碑（同步要知道删了哪些）
        for id in &ids {
            assert!(store.read().snippets.deleted_at(id.as_str()).is_some());
        }
        assert!(
            store.delete_snippet_group("", false).is_err(),
            "未分组不能删除"
        );
        assert!(matches!(
            store.delete_snippet_group("b", true),
            Err(CoreError::NotFound(_))
        ));
    }

    // 一次改名、删除只发一次快捷命令的变更事件（界面只重建一次）
    #[test]
    fn group_changes_emit_once() {
        let store = memory_store();
        add(&store, "a", "a1");
        add(&store, "a", "a2");
        add(&store, "a", "a3");
        let mut events = store.subscribe();
        store.rename_snippet_group("a", "b").unwrap();
        let mut seen = Vec::new();
        while let Ok(event) = events.try_recv() {
            seen.push(event);
        }
        assert_eq!(seen, [StoreEvent::Changed(Kind::Snippet)]);
    }

    // 写入的内容重新打开数据库后照样在（分组顺序存在设置中）
    #[test]
    fn group_order_is_saved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.db");
        let keys = crate::testkit::SharedKeys::default();
        let open = || {
            let db = crate::db::Database::open(&path, &dir.path().join("backups")).unwrap();
            let vault = crate::vault::Vault::open(&db, Box::new(keys.clone())).unwrap();
            Store::open(db, vault).unwrap()
        };
        {
            let store = open();
            add(&store, "a", "a1");
            add(&store, "b", "b1");
            store
                .update_settings(|s| s.snippets.group_order = vec!["b".into(), "a".into()])
                .unwrap();
            store.rename_snippet_group("a", "c").unwrap();
        }
        let store = open();
        assert_eq!(store.settings().snippets.group_order, ["b", "c"]);
        assert_eq!(layout(&store), ["b: b1", "c: a1"]);
    }
}
