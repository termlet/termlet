//! 文件列表的显示与选择规则（远程栏与本地栏共用）。
//!
//! - 排序固定为目录在前，再按名称自然排序（不区分大小写，数字按数值：a、z、B2、b10）。
//! - 选择：单击只选这一项并作为锚点；Ctrl 单击增减这一项；Shift 单击从锚点到这里连续选择。
//! - 键盘：当前行（可以停在“..”上，它不能选中）随方向键移动，规则见 [`Selection::move_to`]；按名字跳转见
//!   [`type_ahead`]。
//! - 名称中看不见的控制字符与双向控制字符写成 `<U+XXXX>`（防止 RLO 伪装扩展名），只用于显示。

use std::cmp::Ordering;
use std::collections::BTreeSet;

/// 比较两个名称：不区分大小写，连续的数字按数值比较。
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut a = a.chars().peekable();
    let mut b = b.chars().peekable();
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars<'_>>| {
                    let mut digits = String::new();
                    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
                        digits.push(c);
                        it.next();
                    }
                    digits
                };
                let (da, db) = (take(&mut a), take(&mut b));
                let (ta, tb) = (da.trim_start_matches('0'), db.trim_start_matches('0'));
                let order = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if order != Ordering::Equal {
                    return order;
                }
            }
            (Some(x), Some(y)) => {
                let order = x.to_lowercase().cmp(y.to_lowercase());
                if order != Ordering::Equal {
                    return order;
                }
                a.next();
                b.next();
            }
        }
    }
}

/// 目录在前，再按名称自然排序。
pub fn sort_entries<T>(entries: &mut [T], key: impl Fn(&T) -> (bool, &str)) {
    entries.sort_by(|x, y| {
        let (dx, nx) = key(x);
        let (dy, ny) = key(y);
        dy.cmp(&dx).then_with(|| natural_cmp(nx, ny))
    });
}

/// 名称中看不见的控制字符与双向控制字符写成 `<U+XXXX>`；远程文件名中解码不了的字节（按主机设置的文件名编码，
/// 见 [`termlet_core::sftp::raw_byte`]）写成 `<0xNN>`。
pub fn visible_name(name: &str) -> String {
    let hidden = |c: char| {
        matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}' | '\u{61c}' | '\u{200e}' | '\u{200f}'
            | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    };
    let raw = termlet_core::sftp::raw_byte;
    if !name.chars().any(|c| hidden(c) || raw(c).is_some()) {
        return name.to_string();
    }
    name.chars()
        .map(|c| match raw(c) {
            Some(b) => format!("<0x{b:02X}>"),
            None if hidden(c) => format!("<U+{:04X}>", c as u32),
            None => c.to_string(),
        })
        .collect()
}

/// 文件大小：1024 进制，不足 1 KB 写整数 B，其余保留一位小数。
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024. && unit < UNITS.len() - 1 {
        value /= 1024.;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// 列表中的选择：选中的名称、Shift 连续选择的起点与键盘操作的当前行。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    pub selected: BTreeSet<String>,
    pub anchor: Option<String>,
    /// 当前行（单击、键盘移动到的项；可以是“..”，它不在选择中）
    pub cursor: Option<String>,
}

/// 键盘移动当前行时怎样改选择。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyMove {
    /// 只选中到达的项（方向键、Home / End、PageUp / PageDown）
    Select,
    /// 从锚点连续选择到到达的项（加 Shift）
    Extend,
    /// 只移动当前行，选择不变（加 Ctrl）
    Cursor,
}

/// 单击时的修饰键。
#[derive(Clone, Copy, Debug, Default)]
pub struct ClickMods {
    pub shift: bool,
    /// Ctrl（macOS 上为 Cmd）
    pub toggle: bool,
}

impl Selection {
    /// 单击 `name` 后的选择；`names` 为列表中按显示顺序排列的全部名称。
    pub fn click(&mut self, names: &[&str], name: &str, mods: ClickMods) {
        if mods.shift
            && let Some(anchor) = &self.anchor
            && let (Some(i), Some(j)) = (
                names.iter().position(|n| n == anchor),
                names.iter().position(|n| *n == name),
            )
        {
            let (from, to) = (i.min(j), i.max(j));
            self.selected = names[from..=to].iter().map(|n| n.to_string()).collect();
            self.cursor = Some(name.to_string());
            return;
        }
        if mods.toggle {
            if !self.selected.remove(name) {
                self.selected.insert(name.to_string());
            }
        } else {
            self.selected = [name.to_string()].into();
        }
        self.anchor = Some(name.to_string());
        self.cursor = Some(name.to_string());
    }

    /// 只选中这一项（右键、拖动没有选中的项时）。
    pub fn only(&mut self, name: &str) {
        self.selected = [name.to_string()].into();
        self.anchor = Some(name.to_string());
        self.cursor = Some(name.to_string());
    }

    /// 键盘把当前行移到 `to`；`names` 为可以选中的名称（按显示顺序，不含“..”）。
    ///
    /// `to` 不能选中时（“..”）：只选中的移动清空选择，连续选择到第一项为止。连续选择以锚点为起点，没有锚点时
    /// 以原来的当前行为起点（资源管理器的做法）。
    pub fn move_to(&mut self, names: &[&str], to: &str, how: KeyMove) {
        let selectable = names.contains(&to);
        match how {
            KeyMove::Select if selectable => {
                self.selected = [to.to_string()].into();
                self.anchor = Some(to.to_string());
            }
            KeyMove::Select => {
                self.selected.clear();
                self.anchor = None;
            }
            KeyMove::Extend => {
                let end = if selectable {
                    Some(to)
                } else {
                    names.first().copied()
                };
                if let Some(end) = end {
                    let known =
                        |n: &Option<String>| n.clone().filter(|n| names.contains(&n.as_str()));
                    let anchor = known(&self.anchor)
                        .or_else(|| known(&self.cursor))
                        .unwrap_or_else(|| end.to_string());
                    let i = names.iter().position(|n| *n == anchor).unwrap_or(0);
                    let j = names.iter().position(|n| *n == end).unwrap_or(0);
                    let (from, to) = (i.min(j), i.max(j));
                    self.selected = names[from..=to].iter().map(|n| n.to_string()).collect();
                    self.anchor = Some(anchor);
                }
            }
            KeyMove::Cursor => {}
        }
        self.cursor = Some(to.to_string());
    }

    /// 增减当前行（Ctrl+空格）；当前行是“..”或没有当前行时不变。
    pub fn toggle_cursor(&mut self, names: &[&str]) {
        let Some(cursor) = self.cursor.clone() else {
            return;
        };
        if names.contains(&cursor.as_str()) {
            if !self.selected.remove(&cursor) {
                self.selected.insert(cursor.clone());
            }
            self.anchor = Some(cursor);
        }
    }

    pub fn all(&mut self, names: &[&str]) {
        self.selected = names.iter().map(|n| n.to_string()).collect();
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn contains(&self, name: &str) -> bool {
        self.selected.contains(name)
    }
}

/// 按名字跳转：在 `names`（显示顺序）中找以 `typed` 开头的项（不区分大小写），返回它的位置；`from` 为当前行。
///
/// 连按同一个字符（含只按了一个）时在以它开头的项之间轮换，从当前行的下一项找起；输入的是一段文字时从当前行
/// 找起，继续输入时停在原处（资源管理器的做法）。到底了从头找。
pub fn type_ahead(names: &[&str], from: Option<usize>, typed: &str) -> Option<usize> {
    let typed = typed.to_lowercase();
    let mut chars = typed.chars();
    let first = chars.next()?;
    let (prefix, start) = if chars.all(|c| c == first) {
        (first.to_string(), from.map_or(0, |i| i + 1))
    } else {
        (typed.clone(), from.unwrap_or(0))
    };
    let len = names.len();
    (0..len)
        .map(|k| (start + k) % len)
        .find(|&i| names[i].to_lowercase().starts_with(&prefix))
}

/// 从目录 `from` 回到它的上级目录 `to` 时，`from` 在 `to` 中的那一项的名称（回到上一级后选中它）。
/// `separators` 为路径的分隔符（远程只有 `/`，本机还有 `\`）；`from` 不在 `to` 之下时为 `None`。
pub fn child_toward(to: &str, from: &str, separators: &[char]) -> Option<String> {
    let rest = from.strip_prefix(to)?;
    let rest = if to.ends_with(separators) {
        rest
    } else {
        rest.strip_prefix(separators)?
    };
    let name = rest.split(separators).next()?;
    (!name.is_empty()).then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order_ignores_case_and_compares_numbers() {
        let mut names = vec!["b10", "z", "B2", "a", "file01", "file1", "File2"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(names, ["a", "B2", "b10", "file01", "file1", "File2", "z"]);
    }

    #[test]
    fn directories_come_first() {
        let mut entries = vec![("b.txt", false), ("z", true), ("a.txt", false), ("A", true)];
        sort_entries(&mut entries, |(n, d)| (*d, *n));
        assert_eq!(
            entries.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            ["A", "z", "a.txt", "b.txt"]
        );
    }

    #[test]
    fn hidden_characters_are_made_visible() {
        assert_eq!(
            visible_name("report\u{202e}fdp.exe"),
            "report<U+202E>fdp.exe"
        );
        assert_eq!(visible_name("a\nb"), "a<U+000A>b");
        assert_eq!(visible_name("正常.txt"), "正常.txt");
        // 远程文件名中解码不了的字节（GBK 的“中”在 UTF-8 的主机上）
        assert_eq!(visible_name("\u{10FED6}\u{10FED0}.txt"), "<0xD6><0xD0>.txt");
    }

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn click_shift_and_toggle() {
        let names = ["a", "b", "c", "d"];
        let mut s = Selection::default();
        s.click(&names, "b", ClickMods::default());
        s.click(
            &names,
            "d",
            ClickMods {
                shift: true,
                toggle: false,
            },
        );
        assert_eq!(s.selected.len(), 3);
        assert_eq!(s.anchor.as_deref(), Some("b"), "Shift 不改变锚点");
        s.click(
            &names,
            "c",
            ClickMods {
                shift: false,
                toggle: true,
            },
        );
        assert!(!s.contains("c"));
        assert_eq!(s.anchor.as_deref(), Some("c"));
        s.click(&names, "a", ClickMods::default());
        assert_eq!(s.selected.len(), 1);
        assert_eq!(s.cursor.as_deref(), Some("a"), "单击的项是当前行");
    }

    #[test]
    fn keyboard_moves_select_extend_and_keep() {
        let names = ["a", "b", "c", "d"];
        let mut s = Selection::default();
        s.move_to(&names, "b", KeyMove::Select);
        assert_eq!(s.selected, BTreeSet::from(["b".into()]));
        // Shift：从锚点连续选择，锚点不变
        s.move_to(&names, "d", KeyMove::Extend);
        assert_eq!(s.selected.len(), 3);
        assert_eq!(s.anchor.as_deref(), Some("b"));
        s.move_to(&names, "a", KeyMove::Extend);
        assert_eq!(
            s.selected,
            BTreeSet::from(["a".into(), "b".into()]),
            "越过锚点反向选择"
        );
        // Ctrl：只移动当前行，再用 Ctrl+空格增减
        s.move_to(&names, "d", KeyMove::Cursor);
        assert_eq!(s.cursor.as_deref(), Some("d"));
        assert_eq!(s.selected.len(), 2);
        s.toggle_cursor(&names);
        assert!(s.contains("d"));
        s.toggle_cursor(&names);
        assert!(!s.contains("d"));
    }

    #[test]
    fn keyboard_on_parent_row_selects_nothing() {
        let names = ["a", "b", "c"];
        let mut s = Selection::default();
        s.move_to(&names, "b", KeyMove::Select);
        s.move_to(&names, "..", KeyMove::Select);
        assert!(s.selected.is_empty(), "停在“..”上不选中任何项");
        assert_eq!(s.cursor.as_deref(), Some(".."));
        s.toggle_cursor(&names);
        assert!(s.selected.is_empty(), "“..”不能选中");
        // 从“..”向下连续选择：没有锚点，从到达的项开始
        s.move_to(&names, "a", KeyMove::Extend);
        assert_eq!(s.selected, BTreeSet::from(["a".into()]));
        // 从下面连续选择到“..”：到第一项为止
        s.move_to(&names, "c", KeyMove::Select);
        s.move_to(&names, "..", KeyMove::Extend);
        assert_eq!(s.selected.len(), 3);
        assert_eq!(s.cursor.as_deref(), Some(".."));
    }

    #[test]
    fn type_ahead_jumps_and_cycles() {
        let names = ["bin", "boot", "dev", "Docs", "etc", "build.sh"];
        assert_eq!(type_ahead(&names, None, "d"), Some(2));
        // 连按同一个字母：在以它开头的项之间轮换，到底了从头找
        assert_eq!(type_ahead(&names, Some(2), "d"), Some(3));
        assert_eq!(type_ahead(&names, Some(3), "dd"), Some(2));
        // 一段文字：从当前行找起，不区分大小写
        assert_eq!(type_ahead(&names, Some(0), "bo"), Some(1));
        assert_eq!(
            type_ahead(&names, Some(1), "boo"),
            Some(1),
            "继续输入停在原处"
        );
        assert_eq!(type_ahead(&names, Some(1), "bu"), Some(5));
        assert_eq!(type_ahead(&names, Some(0), "DOC"), Some(3));
        assert_eq!(type_ahead(&names, Some(0), "x"), None);
        assert_eq!(type_ahead(&[], None, "a"), None);
    }

    #[test]
    fn child_toward_parent() {
        let remote = ['/'];
        assert_eq!(
            child_toward("/", "/home/x", &remote).as_deref(),
            Some("home")
        );
        assert_eq!(
            child_toward("/home", "/home/x/y", &remote).as_deref(),
            Some("x")
        );
        assert_eq!(
            child_toward("/home", "/homework", &remote),
            None,
            "不是它的下级"
        );
        assert_eq!(child_toward("/a", "/b/c", &remote), None);
        assert_eq!(child_toward("/a", "/a", &remote), None);
        // 远程的名称中可以有反斜杠
        assert_eq!(
            child_toward("/a", r"/a/x\y", &remote).as_deref(),
            Some(r"x\y")
        );
        let local = ['\\', '/'];
        assert_eq!(
            child_toward(r"C:\Users", r"C:\Users\me\Downloads", &local).as_deref(),
            Some("me")
        );
        assert_eq!(
            child_toward(r"C:\", r"C:\Users", &local).as_deref(),
            Some("Users")
        );
    }
}
