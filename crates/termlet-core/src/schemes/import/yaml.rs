//! YAML 的一个子集：缩进表示的映射与序列、行内的 `[..]` / `{..}`（只解析一层）、引号字符串、注释、锚点与别名。
//!
//! 不支持多行字符串（`|` 与 `>` 的内容跳过）、合并键、多文档（`---` 之后的内容接着前面的）等。
//! 按行处理、用显式的栈，不递归。标量一律保留原文（不转成数字），没加引号的 `#rrggbb` 也当作值
//! （严格的 YAML 中它是注释，配色文件里却常这样写）。没用完整的 YAML 解析器正是为了这两点：
//! 严格解析时 `2e3440`（Nord 的背景色）会变成浮点数、`#505050` 会被当作注释。

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use super::text::{MAX_LINE, split_top_level};
use super::tree::{MapNode, Node, NodeId, Tree};

/// YAML 的键值行：键可以加引号；值在冒号加空白之后。键（第 1 组）可能带着冒号前的空白，值（第 2 组）可能带着开头的空白，
/// 用时去掉。
static YAML_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^("(?:[^"\\]|\\.)*"[ \t]*|'(?:[^']|'')*'[ \t]*|[^\s"'#\[\]{}:][^:]*):(?:[ \t]([\s\S]*))?$"#)
        .expect("YAML 键值行的正则")
});

/// 键值行的键与值（值可能为空）；不是键值行时返回 `None`。
fn key_value(t: &str) -> Option<(&str, &str)> {
    let caps = YAML_KEY.captures(t)?;
    let key = caps.get(1)?.as_str();
    let value = caps.get(2).map_or("", |m| m.as_str());
    Some((key, value))
}

/// `#` 开始的是不是没加引号的颜色：`#` 后是 3–8 位十六进制数、之后是行尾或分隔符，且 `#` 前（跳过空白）是 `:`、`-`、`,`、`[`、`{`。
fn is_hex_color_at(b: &[u8], i: usize) -> bool {
    let digits = b[i + 1..]
        .iter()
        .take(9)
        .take_while(|c| c.is_ascii_hexdigit())
        .count();
    if !(3..=8).contains(&digits) {
        return false;
    }
    match b.get(i + 1 + digits) {
        None => {}
        Some(c) if c.is_ascii_whitespace() || matches!(c, b',' | b']' | b'}') => {}
        Some(_) => return false,
    }
    b[..i]
        .iter()
        .rev()
        .find(|&&c| c != b' ' && c != b'\t')
        .is_some_and(|c| b":-,[{".contains(c))
}

/// 去掉 YAML 行尾注释；宽容没加引号的 `#rrggbb`。
fn strip_comment(s: &str) -> &str {
    let b = s.as_bytes();
    let mut quote = 0u8;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if quote != 0 {
            if c == quote {
                quote = 0;
            } else if c == b'\\' && quote == b'"' {
                i += 1;
            }
            i += 1;
            continue;
        }
        if (c == b'"' || c == b'\'') && (i == 0 || b" \t:,[{-".contains(&b[i - 1])) {
            quote = c;
            i += 1;
            continue;
        }
        if c == b'#' && (i == 0 || b[i - 1] == b' ' || b[i - 1] == b'\t') && !is_hex_color_at(b, i)
        {
            return s[..i].trim_end();
        }
        i += 1;
    }
    s
}

/// 标量：去掉引号（双引号中处理 `\"`、`\\`、`\/`、`\n`、`\t`），`~` 与 `null` 为空值（`None`）。
fn scalar(v: &str) -> Option<String> {
    let s = v.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        let inner = &s[1..s.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\'
                && let Some(&next) = chars.peek()
                && matches!(next, '"' | '\\' | '/' | 'n' | 't')
            {
                chars.next();
                out.push(match next {
                    'n' => '\n',
                    't' => '\t',
                    other => other,
                });
                continue;
            }
            out.push(c);
        }
        return Some(out);
    }
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        return Some(s[1..s.len() - 1].replace("''", "'"));
    }
    if s == "~" || s == "null" {
        return None;
    }
    Some(s.to_string())
}

fn scalar_node(tree: &mut Tree, v: &str) -> NodeId {
    match scalar(v) {
        Some(s) => tree.push(Node::Scalar(s)),
        None => tree.null(),
    }
}

/// 行内的 `[a, b]` 与 `{k: v}`：只解析一层，更深的嵌套原样当字符串。
fn flow(tree: &mut Tree, v: &str) -> NodeId {
    let s = v.trim();
    if let Some(body) = s.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        let items = split_top_level(body)
            .into_iter()
            .map(|p| scalar_node(tree, p))
            .collect();
        return tree.push(Node::Seq(items));
    }
    if let Some(body) = s.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
        let mut map = MapNode::default();
        for part in split_top_level(body) {
            if let Some((k, v)) = key_value(part) {
                let key = scalar(k).unwrap_or_else(|| "null".into());
                let value = scalar_node(tree, v);
                map.insert(key, value);
            }
        }
        return tree.push(Node::Map(map));
    }
    scalar_node(tree, s)
}

/// 映射中的键或序列中的位置。
enum Slot {
    Key(String),
    Index(usize),
}

/// 值为空的键（或序列项）：下一行更深的缩进决定它是映射还是序列。
struct Pending {
    parent: NodeId,
    slot: Slot,
    indent: usize,
    anchor: Option<String>,
}

struct Parser {
    tree: Tree,
    stack: Vec<(usize, NodeId)>,
    pending: Option<Pending>,
    anchors: HashMap<String, NodeId>,
    /// 多行字符串（`|`、`>`）所在键的缩进：更深的行是字符串的内容，跳过
    block_indent: Option<usize>,
}

impl Parser {
    fn set_child(&mut self, parent: NodeId, slot: Slot, value: NodeId) {
        match (self.tree.node_mut(parent), slot) {
            (Some(Node::Map(map)), Slot::Key(key)) => map.insert(key, value),
            (Some(Node::Seq(items)), Slot::Index(i)) => {
                if let Some(item) = items.get_mut(i) {
                    *item = value;
                }
            }
            _ => {}
        }
    }

    fn insert(&mut self, map: NodeId, key: String, value: NodeId) {
        self.set_child(map, Slot::Key(key), value);
    }

    /// 映射中的一行 `键: 值`；`col` 是键所在的列。
    fn entry(&mut self, map: NodeId, t: &str, col: usize) {
        let Some((key, value)) = key_value(t) else {
            return;
        };
        let key = scalar(key).unwrap_or_else(|| "null".into());
        let mut v = value.trim();
        let mut anchor = None;
        if let Some(rest) = v.strip_prefix('&') {
            match rest.find(char::is_whitespace) {
                Some(sp) => {
                    anchor = Some(rest[..sp].to_string());
                    v = rest[sp..].trim();
                }
                None => {
                    anchor = Some(rest.to_string());
                    v = "";
                }
            }
        }
        if v.is_empty() {
            let null = self.tree.null();
            self.insert(map, key.clone(), null);
            self.pending = Some(Pending {
                parent: map,
                slot: Slot::Key(key),
                indent: col,
                anchor,
            });
        } else if let Some(name) = v.strip_prefix('*') {
            let target = self
                .anchors
                .get(name.trim())
                .copied()
                .unwrap_or(self.tree.null());
            self.insert(map, key, target);
        } else if is_block_indicator(v) {
            let empty = self.tree.push(Node::Scalar(String::new()));
            self.insert(map, key, empty);
            self.block_indent = Some(col);
        } else {
            let node = flow(&mut self.tree, v);
            self.insert(map, key, node);
            if let Some(anchor) = anchor {
                self.anchors.insert(anchor, node);
            }
        }
    }

    fn line(&mut self, line: &str) {
        if line.len() > MAX_LINE {
            return;
        }
        let Some(start) = line.find(|c: char| !c.is_whitespace()) else {
            return;
        };
        let indent = line[..start].chars().count();
        if let Some(block) = self.block_indent {
            if indent > block {
                return;
            }
            self.block_indent = None;
        }
        let t = strip_comment(&line[start..]).trim_end();
        if t.is_empty() || t == "---" || t == "..." || (indent == 0 && t.starts_with('%')) {
            return;
        }
        let is_seq = t == "-" || t.starts_with("- ");

        if self.stack.is_empty() {
            let root = self.tree.push(if is_seq {
                Node::Seq(Vec::new())
            } else {
                Node::Map(MapNode::default())
            });
            self.stack.push((indent, root));
        }
        if let Some(p) = self.pending.take() {
            let parent_is_seq = self.tree.is_seq(p.parent);
            if indent > p.indent || (is_seq && indent == p.indent && !parent_is_seq) {
                let node = self.tree.push(if is_seq {
                    Node::Seq(Vec::new())
                } else {
                    Node::Map(MapNode::default())
                });
                self.set_child(p.parent, p.slot, node);
                if let Some(anchor) = p.anchor {
                    self.anchors.insert(anchor, node);
                }
                self.stack.push((indent, node));
            }
        }
        while self.stack.len() > 1 && self.stack.last().is_some_and(|&(i, _)| i > indent) {
            self.stack.pop();
        }
        // 与父键同一缩进的序列结束了
        if self.stack.len() > 1
            && let Some(&(top_indent, top)) = self.stack.last()
            && self.tree.is_seq(top)
            && !is_seq
            && top_indent == indent
        {
            self.stack.pop();
        }
        let Some(&(top_indent, top)) = self.stack.last() else {
            return;
        };
        if top_indent != indent {
            return;
        }

        if is_seq {
            if !self.tree.is_seq(top) {
                return;
            }
            let rest = t[1..].trim_start();
            let col = indent + t[..t.len() - rest.len()].chars().count();
            if rest.is_empty() {
                let null = self.tree.null();
                let index = self.push_item(top, null);
                self.pending = Some(Pending {
                    parent: top,
                    slot: Slot::Index(index),
                    indent,
                    anchor: None,
                });
            } else if key_value(rest).is_some() && !rest.starts_with('[') && !rest.starts_with('{')
            {
                let obj = self.tree.push(Node::Map(MapNode::default()));
                self.push_item(top, obj);
                self.stack.push((col, obj));
                self.entry(obj, rest, col);
            } else {
                let node = flow(&mut self.tree, rest);
                self.push_item(top, node);
            }
        } else if !self.tree.is_seq(top) {
            self.entry(top, t, indent);
        }
    }

    /// 在序列末尾加一项，返回它的位置。
    fn push_item(&mut self, seq: NodeId, value: NodeId) -> usize {
        match self.tree.node_mut(seq) {
            Some(Node::Seq(items)) => {
                items.push(value);
                items.len() - 1
            }
            _ => 0,
        }
    }
}

/// `|`、`>` 加可选的 `-`、`+`、数字：多行字符串的开头。
fn is_block_indicator(v: &str) -> bool {
    let mut chars = v.chars();
    matches!(chars.next(), Some('|' | '>'))
        && chars.all(|c| c == '-' || c == '+' || c.is_ascii_digit())
}

/// 解析 YAML 子集为对象树；没有内容时返回 `None`。
pub(super) fn parse_yaml_lite(text: &str) -> Option<(Tree, NodeId)> {
    let mut parser = Parser {
        tree: Tree::new(),
        stack: Vec::new(),
        pending: None,
        anchors: HashMap::new(),
        block_indent: None,
    };
    for line in text.split('\n') {
        parser.line(line);
    }
    let root = parser.stack.first()?.1;
    Some((parser.tree, root))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> (Tree, NodeId) {
        parse_yaml_lite(text).unwrap()
    }

    #[test]
    fn mappings_sequences_and_flow() {
        let (tree, root) = parse(
            "a: 1\nb:\n  c: \"x\\ty\" # 注释\n  d: 'it''s'\nlist:\n- one\n- [p, 'q, r']\n- k: v\n  k2: v2\nflow: {x: 1, y: ~}\ncolor: #ABCDEF\n",
        );
        let o = tree.obj(root).unwrap();
        assert_eq!(o.str("a").as_deref(), Some("1"));
        let b = o.obj("b").unwrap();
        assert_eq!(b.str("c").as_deref(), Some("x\ty"));
        assert_eq!(b.str("d").as_deref(), Some("it's"));
        let list = o.seq("list").unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(tree.str(list[0]).as_deref(), Some("one"));
        assert_eq!(tree.seq(list[1]).map(<[_]>::len), Some(2));
        let item = tree.obj(list[2]).unwrap();
        assert_eq!(item.str("k2").as_deref(), Some("v2"));
        let flow = o.obj("flow").unwrap();
        assert_eq!(flow.str("x").as_deref(), Some("1"));
        assert!(flow.has("y"));
        assert_eq!(o.str("color").as_deref(), Some("#ABCDEF"));
    }

    #[test]
    fn anchors_aliases_and_block_strings() {
        let (tree, root) = parse(
            "base: &b\n  x: '1'\ncopy: *b\nlit: |\n  colors:\n    - not parsed\nafter: yes\nself: &s\n  me: *s\n",
        );
        let o = tree.obj(root).unwrap();
        assert_eq!(o.get("copy"), o.get("base"));
        assert_eq!(o.str("lit"), None);
        assert_eq!(o.str("after").as_deref(), Some("yes"));
        let me = o.obj("self").unwrap();
        assert_eq!(me.get("me"), o.get("self"));
        assert!(parse_yaml_lite("\n# 只有注释\n---\n").is_none());
    }

    #[test]
    fn comments_and_colors() {
        assert_eq!(strip_comment("a: #fff # c"), "a: #fff");
        assert_eq!(strip_comment("a: x #fff"), "a: x");
        assert_eq!(strip_comment("a: '#x' # c"), "a: '#x'");
        assert_eq!(strip_comment("- #123456"), "- #123456");
        assert_eq!(strip_comment("a: #1234567890"), "a:");
        assert_eq!(strip_comment("# 整行"), "");
    }
}
