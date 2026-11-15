//! 解析出的对象树（JSON 与 YAML 子集共用）：节点存在数组中，按下标引用。
//!
//! YAML 的别名让同一个节点出现在多处（甚至引用自己），所以子节点只记下标，遍历时限制深度与经过的节点数。
//! 映射保留键的先后顺序；同一个键出现多次时取最后的值、位置不变（与 JavaScript 的对象相同）。

use std::collections::{HashMap, HashSet};
use std::fmt;

use serde::de::{DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};

pub(super) type NodeId = usize;

/// 在对象树中查找配色时的最大深度。
const MAX_DEPTH: usize = 8;
/// 遍历对象树时最多经过的节点数。YAML 的别名让同一个节点出现在多处：几百字节的文件（每层十几个键都是上一层的别名，
/// 叠几层）展开后有上亿个节点，不能按展开后的树计数到叶子才停。
const MAX_NODES: usize = 100_000;
/// 拍平后最多的路径数。
const MAX_PATHS: usize = 10_000;
/// 一个 JSON 文本最多解析出的节点数（防止几 MB 的 `[[],[],…]` 占用大量内存）。
const MAX_JSON_NODES: usize = 1_000_000;

pub(super) enum Node {
    /// null、布尔值，以及 YAML 中没有值的键
    Null,
    /// 字符串与数字（数字转为字符串）
    Scalar(String),
    Seq(Vec<NodeId>),
    Map(MapNode),
}

#[derive(Default)]
pub(super) struct MapNode {
    entries: Vec<(String, NodeId)>,
    index: HashMap<String, usize>,
}

impl MapNode {
    /// 加入一个键；已有这个键时替换它的值，位置不变。
    pub fn insert(&mut self, key: String, value: NodeId) {
        match self.index.get(&key) {
            Some(&i) => self.entries[i].1 = value,
            None => {
                self.index.insert(key.clone(), self.entries.len());
                self.entries.push((key, value));
            }
        }
    }

    pub fn get(&self, key: &str) -> Option<NodeId> {
        self.index.get(key).map(|&i| self.entries[i].1)
    }

    /// 不区分大小写取键：先按原样找，再按先后顺序找第一个小写相同的。
    pub fn get_ci(&self, key: &str) -> Option<NodeId> {
        self.get(key).or_else(|| {
            let lower = key.to_lowercase();
            self.entries
                .iter()
                .find(|(k, _)| k.to_lowercase() == lower)
                .map(|&(_, v)| v)
        })
    }

    pub fn entries(&self) -> impl Iterator<Item = (&str, NodeId)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), *v))
    }
}

pub(super) struct Tree {
    nodes: Vec<Node>,
}

/// 共用的 null 节点（不会被修改，可以出现在多处）。
const NULL: NodeId = 0;

impl Tree {
    pub fn new() -> Self {
        Self {
            nodes: vec![Node::Null],
        }
    }

    pub fn null(&self) -> NodeId {
        NULL
    }

    pub fn push(&mut self, node: Node) -> NodeId {
        if matches!(node, Node::Null) {
            return NULL;
        }
        self.nodes.push(node);
        self.nodes.len() - 1
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn node(&self, id: NodeId) -> &Node {
        self.nodes.get(id).unwrap_or(&Node::Null)
    }

    pub fn node_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        // 共用的 null 节点不能修改
        if id == NULL {
            return None;
        }
        self.nodes.get_mut(id)
    }

    pub fn is_seq(&self, id: NodeId) -> bool {
        matches!(self.node(id), Node::Seq(_))
    }

    pub fn obj(&self, id: NodeId) -> Option<Obj<'_>> {
        match self.node(id) {
            Node::Map(map) => Some(Obj {
                tree: self,
                id,
                map,
            }),
            _ => None,
        }
    }

    pub fn seq(&self, id: NodeId) -> Option<&[NodeId]> {
        match self.node(id) {
            Node::Seq(items) => Some(items),
            _ => None,
        }
    }

    /// 取字符串值：数字转为字符串，去掉两端的空白后非空才算有值；对象、数组、null 为 `None`。
    pub fn str(&self, id: NodeId) -> Option<String> {
        match self.node(id) {
            Node::Scalar(s) => {
                let t = s.trim();
                (!t.is_empty()).then(|| t.to_string())
            }
            _ => None,
        }
    }

    /// 把映射拍平为小写的点号路径到字符串值（数组不展开）。限制深度与经过的节点数，防止别名造成的环与指数级展开。
    /// 同一个节点可以经由不同的路径出现（旧的 Alacritty 配置用 `colors: *dark` 引用 `schemes` 下的一套），不按引用去重。
    pub fn flatten(&self, root: NodeId) -> HashMap<String, String> {
        let mut out = HashMap::new();
        let mut walk = vec![(root, String::new(), 0usize)];
        let mut budget = MAX_NODES;
        while out.len() < MAX_PATHS {
            let Some((id, path, depth)) = walk.pop() else {
                break;
            };
            if depth > MAX_DEPTH {
                continue;
            }
            match self.node(id) {
                Node::Map(map) => {
                    for (key, child) in map.entries() {
                        if budget == 0 {
                            return out;
                        }
                        budget -= 1;
                        let key = key.to_lowercase();
                        let child_path = if path.is_empty() {
                            key
                        } else {
                            format!("{path}.{key}")
                        };
                        walk.push((child, child_path, depth + 1));
                    }
                }
                _ => {
                    if let Some(s) = self.str(id) {
                        out.entry(path).or_insert(s);
                    }
                }
            }
        }
        out
    }

    /// 在对象树中找出满足条件的映射（广度优先、限制深度与经过的节点数、同一个节点只访问一次）。
    /// 满足条件的映射不再往下找。
    pub fn find_objects(
        &self,
        root: NodeId,
        matches: impl Fn(Obj<'_>) -> bool,
        limit: usize,
    ) -> Vec<NodeId> {
        let mut found = Vec::new();
        let mut seen = HashSet::new();
        let mut queue = vec![(root, 0usize)];
        let mut qi = 0;
        while qi < queue.len() && found.len() < limit {
            let (id, depth) = queue[qi];
            qi += 1;
            if depth > MAX_DEPTH || !seen.insert(id) {
                continue;
            }
            let children: Vec<NodeId> = match self.node(id) {
                Node::Map(map) => {
                    if matches(Obj {
                        tree: self,
                        id,
                        map,
                    }) {
                        found.push(id);
                        continue;
                    }
                    map.entries().map(|(_, v)| v).collect()
                }
                Node::Seq(items) => items.clone(),
                _ => continue,
            };
            for child in children {
                // 只排入没有访问过的映射与数组
                let container = matches!(self.node(child), Node::Map(_) | Node::Seq(_));
                if !container || seen.contains(&child) {
                    continue;
                }
                if queue.len() >= MAX_NODES {
                    return found;
                }
                queue.push((child, depth + 1));
            }
        }
        found
    }
}

/// 映射节点的只读视图。
#[derive(Clone, Copy)]
pub(super) struct Obj<'t> {
    pub tree: &'t Tree,
    pub id: NodeId,
    map: &'t MapNode,
}

impl<'t> Obj<'t> {
    pub fn get(&self, key: &str) -> Option<NodeId> {
        self.map.get(key)
    }

    /// 有这个键（值可以是 null）。
    pub fn has(&self, key: &str) -> bool {
        self.map.get(key).is_some()
    }

    pub fn str(&self, key: &str) -> Option<String> {
        self.get(key).and_then(|id| self.tree.str(id))
    }

    pub fn obj(&self, key: &str) -> Option<Obj<'t>> {
        self.get(key).and_then(|id| self.tree.obj(id))
    }

    pub fn seq(&self, key: &str) -> Option<&'t [NodeId]> {
        self.get(key).and_then(|id| self.tree.seq(id))
    }

    /// 不区分大小写取字符串值。
    pub fn str_ci(&self, key: &str) -> Option<String> {
        self.map.get_ci(key).and_then(|id| self.tree.str(id))
    }

    pub fn has_ci(&self, key: &str) -> bool {
        self.map.get_ci(key).is_some()
    }
}

/// 解析 JSON 为对象树（保留键的顺序）；不是 JSON、嵌套太深（超过 128 层）或节点太多时返回 `None`。
pub(super) fn parse_json(text: &str) -> Option<(Tree, NodeId)> {
    let mut tree = Tree::new();
    let mut de = serde_json::Deserializer::from_str(text);
    let root = NodeSeed { tree: &mut tree }.deserialize(&mut de).ok()?;
    de.end().ok()?;
    Some((tree, root))
}

struct NodeSeed<'t> {
    tree: &'t mut Tree,
}

impl<'de> DeserializeSeed<'de> for NodeSeed<'_> {
    type Value = NodeId;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<NodeId, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl NodeSeed<'_> {
    fn add<E: serde::de::Error>(self, node: Node) -> Result<NodeId, E> {
        if self.tree.len() >= MAX_JSON_NODES {
            return Err(E::custom("节点太多"));
        }
        Ok(self.tree.push(node))
    }
}

impl<'de> Visitor<'de> for NodeSeed<'_> {
    type Value = NodeId;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("JSON")
    }

    fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<NodeId, E> {
        Ok(NULL)
    }

    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<NodeId, E> {
        self.add(Node::Scalar(v.to_string()))
    }

    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<NodeId, E> {
        self.add(Node::Scalar(v.to_string()))
    }

    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<NodeId, E> {
        if v.is_finite() {
            self.add(Node::Scalar(v.to_string()))
        } else {
            Ok(NULL)
        }
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<NodeId, E> {
        self.add(Node::Scalar(v.to_owned()))
    }

    fn visit_string<E: serde::de::Error>(self, v: String) -> Result<NodeId, E> {
        self.add(Node::Scalar(v))
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<NodeId, E> {
        Ok(NULL)
    }

    fn visit_none<E: serde::de::Error>(self) -> Result<NodeId, E> {
        Ok(NULL)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<NodeId, A::Error> {
        let mut items = Vec::new();
        while let Some(id) = seq.next_element_seed(NodeSeed {
            tree: &mut *self.tree,
        })? {
            items.push(id);
        }
        self.add(Node::Seq(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<NodeId, A::Error> {
        let mut map = MapNode::default();
        while let Some(key) = access.next_key::<String>()? {
            let value = access.next_value_seed(NodeSeed {
                tree: &mut *self.tree,
            })?;
            map.insert(key, value);
        }
        self.add(Node::Map(map))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_keeps_key_order_and_last_duplicate() {
        let (tree, root) =
            parse_json(r#"{"b": 1, "a": "x", "b": 2.5, "c": [true, null]}"#).unwrap();
        let obj = tree.obj(root).unwrap();
        let keys: Vec<&str> = obj.map.entries().map(|(k, _)| k).collect();
        assert_eq!(keys, ["b", "a", "c"]);
        assert_eq!(obj.str("b").as_deref(), Some("2.5"));
        assert_eq!(obj.seq("c").map(<[_]>::len), Some(2));
        assert!(obj.has("c"));
        assert_eq!(obj.str("c"), None);
        // 深层嵌套、残缺、尾随内容都不是 JSON
        assert!(parse_json(&"[".repeat(1000)).is_none());
        assert!(parse_json(r#"{"a": "#).is_none());
        assert!(parse_json("{} x").is_none());
    }

    #[test]
    fn traversal_is_bounded_on_cycles() {
        // 映射引用自己
        let mut tree = Tree::new();
        let id = tree.push(Node::Map(MapNode::default()));
        if let Some(Node::Map(map)) = tree.node_mut(id) {
            map.insert("self".into(), id);
            map.insert("Name".into(), 0);
        }
        let flat = tree.flatten(id);
        assert!(flat.is_empty());
        assert!(tree.find_objects(id, |o| o.has("missing"), 10).is_empty());
        assert!(tree.obj(id).unwrap().has_ci("name"));
    }
}
