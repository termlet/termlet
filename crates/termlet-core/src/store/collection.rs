//! 一种条目在内存中的集合，以及条目行的读写。

use std::collections::HashMap;

use rusqlite::Connection;

use crate::db::{self, ItemRow};
use crate::model::{Item, ItemId, Kind};
use crate::sort_key;

/// 一条条目：ID、上级、排序键与内容。
#[derive(Clone, Debug, PartialEq)]
pub struct Record<T: Item> {
    pub id: T::Id,
    /// 所属分组等上级的 ID
    pub parent: Option<String>,
    pub sort_key: String,
    pub doc: T,
    pub created_at: i64,
    pub updated_at: i64,
}

impl<T: Item> Record<T> {
    pub(crate) fn to_row(&self) -> serde_json::Result<ItemRow> {
        Ok(ItemRow {
            id: self.id.as_str().to_string(),
            parent_id: self.parent.clone(),
            sort_key: self.sort_key.clone(),
            doc: serde_json::to_string(&self.doc)?,
            created_at: self.created_at,
            updated_at: self.updated_at,
            deleted_at: None,
        })
    }
}

/// 在同级中的位置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Position<Id> {
    First,
    Last,
    Before(Id),
    After(Id),
}

pub struct Collection<T: Item> {
    live: HashMap<T::Id, Record<T>>,
    /// 删除墓碑：ID → 删除时间
    tombstones: HashMap<String, i64>,
}

impl<T: Item> Default for Collection<T> {
    fn default() -> Self {
        Self {
            live: HashMap::new(),
            tombstones: HashMap::new(),
        }
    }
}

impl<T: Item> Clone for Collection<T> {
    fn clone(&self) -> Self {
        Self {
            live: self.live.clone(),
            tombstones: self.tombstones.clone(),
        }
    }
}

impl<T: Item> Collection<T> {
    pub(crate) fn load(conn: &Connection) -> rusqlite::Result<Self> {
        let mut collection = Self::default();
        for row in db::load_items(conn, T::KIND.table())? {
            if let Some(deleted_at) = row.deleted_at {
                collection.tombstones.insert(row.id, deleted_at);
                continue;
            }
            match serde_json::from_str::<T>(&row.doc) {
                Ok(doc) => {
                    let id = T::Id::from_raw(row.id);
                    collection.live.insert(
                        id.clone(),
                        Record {
                            id,
                            parent: row.parent_id,
                            sort_key: row.sort_key,
                            doc,
                            created_at: row.created_at,
                            updated_at: row.updated_at,
                        },
                    );
                }
                // 内容损坏（被手工改过）时跳过这一条，不让整个程序起不来
                Err(e) => {
                    tracing::warn!(kind = T::KIND.name(), id = %row.id, "条目内容无法解析，已跳过：{e}")
                }
            }
        }
        Ok(collection)
    }

    pub fn get(&self, id: &T::Id) -> Option<&Record<T>> {
        self.live.get(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Record<T>> {
        self.live.values()
    }

    pub fn len(&self) -> usize {
        self.live.len()
    }

    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }

    /// 删除时间（墓碑）。
    pub fn deleted_at(&self, id: &str) -> Option<i64> {
        self.tombstones.get(id).copied()
    }

    /// 全部条目，按上级、排序键、ID 排列。
    pub fn sorted(&self) -> Vec<Record<T>> {
        let mut records: Vec<Record<T>> = self.live.values().cloned().collect();
        records.sort_by(|a, b| {
            (a.parent.as_deref(), a.sort_key.as_str(), a.id.as_str()).cmp(&(
                b.parent.as_deref(),
                b.sort_key.as_str(),
                b.id.as_str(),
            ))
        });
        records
    }

    /// 同一上级下的条目，按排序键排列。
    fn siblings(&self, parent: Option<&str>, except: Option<&T::Id>) -> Vec<&Record<T>> {
        let mut siblings: Vec<&Record<T>> = self
            .live
            .values()
            .filter(|r| r.parent.as_deref() == parent && Some(&r.id) != except)
            .collect();
        siblings.sort_by(|a, b| (&a.sort_key, a.id.as_str()).cmp(&(&b.sort_key, b.id.as_str())));
        siblings
    }

    /// 放到 `parent` 下 `position` 处的排序键。`moving` 是正在移动的条目（不算作邻居）。
    pub(crate) fn key_for(
        &self,
        parent: Option<&str>,
        position: &Position<T::Id>,
        moving: Option<&T::Id>,
    ) -> String {
        let siblings = self.siblings(parent, moving);
        let keys: Vec<&str> = siblings.iter().map(|r| r.sort_key.as_str()).collect();
        let index_of = |id: &T::Id| siblings.iter().position(|r| &r.id == id);
        let (before, after) = match position {
            Position::First => (None, keys.first().copied()),
            Position::Before(target) => match index_of(target) {
                Some(i) => (i.checked_sub(1).map(|j| keys[j]), Some(keys[i])),
                None => (keys.last().copied(), None),
            },
            Position::After(target) => match index_of(target) {
                Some(i) => (Some(keys[i]), keys.get(i + 1).copied()),
                None => (keys.last().copied(), None),
            },
            Position::Last => (keys.last().copied(), None),
        };
        sort_key::between(before, after)
    }

    pub(crate) fn new_record(
        &self,
        doc: T,
        parent: Option<String>,
        position: &Position<T::Id>,
    ) -> Record<T> {
        let now = crate::time::now_ms();
        Record {
            id: T::Id::generate(),
            sort_key: self.key_for(parent.as_deref(), position, None),
            parent,
            doc,
            created_at: now,
            updated_at: now,
        }
    }

    pub(crate) fn insert(&mut self, record: Record<T>) {
        self.tombstones.remove(record.id.as_str());
        self.live.insert(record.id.clone(), record);
    }

    pub(crate) fn remove(&mut self, id: &str, now: i64) {
        self.live.remove(&T::Id::from_raw(id.to_string()));
        self.tombstones.insert(id.to_string(), now);
    }

    /// 全部墓碑的 ID。
    pub(crate) fn tombstone_ids(&self) -> impl Iterator<Item = &String> {
        self.tombstones.keys()
    }

    /// 忘掉一个墓碑（同步完成后清理）。
    pub(crate) fn forget_tombstone(&mut self, id: &str) {
        self.tombstones.remove(id);
    }
}

/// 写入一条条目。
pub(crate) fn put<T: Item>(conn: &Connection, record: &Record<T>) -> crate::Result<()> {
    db::put_item(conn, T::KIND.table(), &record.to_row()?)?;
    Ok(())
}

/// 把条目改成墓碑（内容清空）。
pub(crate) fn put_tombstone(
    conn: &Connection,
    kind: Kind,
    id: &str,
    now: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        &format!(
            "UPDATE {} SET doc = '{{}}', deleted_at = ?2, updated_at = ?2 WHERE id = ?1",
            kind.table()
        ),
        rusqlite::params![id, now],
    )?;
    Ok(())
}
