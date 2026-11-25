//! 文件列表的显示与选择规则（远程栏与本地栏共用）。
//!
//! - 排序固定为目录在前，再按名称自然排序（不区分大小写，数字按数值：a、z、B2、b10）。
//! - 选择：单击只选这一项并作为锚点；Ctrl 单击增减这一项；Shift 单击从锚点到这里连续选择。
//! - 键盘：当前行（可以停在“..”上，它不能选中）随方向键移动，规则见 [`Selection::move_to`]；按名字跳转见
//!   [`type_ahead`]。
//! - 筛选当前目录见 [`NameFilter`]；筛不到时“在终端中查找”的命令见 [`find_command`]。
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

/// 当前目录的筛选：名称包含输入的文字即算匹配，不分大小写；文字中有 `*` 或 `?` 时按通配符匹配整个名称
/// （`*` 任意多个字符，`?` 一个字符，如 `*.log`、`access?.log`）。输入为空时都算匹配。
pub struct NameFilter {
    /// 小写的输入
    query: String,
    /// 按通配符匹配时的模式（小写，按字符拆开）
    glob: Option<Vec<char>>,
}

impl NameFilter {
    pub fn new(query: &str) -> Self {
        let query = query.to_lowercase();
        let glob = has_wildcard(&query).then(|| query.chars().collect());
        Self { query, glob }
    }

    pub fn matches(&self, name: &str) -> bool {
        if self.query.is_empty() {
            return true;
        }
        let name = name.to_lowercase();
        match &self.glob {
            Some(pattern) => wildcard(&name.chars().collect::<Vec<_>>(), pattern),
            None => name.contains(&self.query),
        }
    }
}

fn has_wildcard(query: &str) -> bool {
    query.contains(['*', '?'])
}

/// 通配符匹配整个名称：遇到不匹配时退回上一个 `*` 多吞一个字符，最坏为名称长度乘模式长度。
fn wildcard(name: &[char], pattern: &[char]) -> bool {
    let (mut n, mut p) = (0, 0);
    // 上一个 `*` 之后的模式位置与它已经吞到的名称位置
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p + 1, n));
                p += 1;
            }
            Some(&c) if c == '?' || c == name[n] => {
                n += 1;
                p += 1;
            }
            _ => match star {
                Some((after, from)) => {
                    star = Some((after, from + 1));
                    p = after;
                    n = from + 1;
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

/// “在终端中查找”写进终端的命令：在 `dir` 及其下各级目录中按名称查找（不分大小写），`query` 的含义与
/// [`NameFilter`] 相同（包含这段文字时前后加 `*`，其中的 `*?[\` 转义；有通配符时照用，`[`、`\` 转义）。
/// 两个参数按 POSIX shell 的单引号写（`dir` 只有常见字符时不加引号）。`query` 为空，或有控制字符、远程文件名中解码
/// 不了的字节（写进终端会变成按键或别的字符）时为 `None`。
pub fn find_command(dir: &str, query: &str) -> Option<String> {
    let unfit = |c: char| c.is_control() || termlet_core::sftp::raw_byte(c).is_some();
    if dir.is_empty() || query.is_empty() || dir.chars().chain(query.chars()).any(unfit) {
        return None;
    }
    let escape = |text: &str, special: &[char]| {
        let mut out = String::with_capacity(text.len());
        for c in text.chars() {
            if special.contains(&c) {
                out.push('\\');
            }
            out.push(c);
        }
        out
    };
    let pattern = if has_wildcard(query) {
        escape(query, &['[', '\\'])
    } else {
        format!("*{}*", escape(query, &['*', '?', '[', '\\']))
    };
    let plain = |text: &str| {
        !text.starts_with('-')
            && text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "/._-+,:@".contains(c))
    };
    let dir = if plain(dir) {
        dir.to_string()
    } else {
        shell_quote(dir)
    };
    Some(format!("find {dir} -iname {}", shell_quote(&pattern)))
}

/// POSIX shell 的单引号：其中的 `'` 写成 `'\''`。
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
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
    fn filter_contains_or_wildcard() {
        let matches = |query: &str, name: &str| NameFilter::new(query).matches(name);
        // 包含，不分大小写；空的都匹配
        assert!(matches("log", "Access.LOG"));
        assert!(matches("", "anything"));
        assert!(!matches("log", "app.txt"));
        assert!(matches("日志", "运行日志.txt"));
        // 有 * 或 ? 时匹配整个名称
        assert!(matches("*.log", "error.log"));
        assert!(!matches("*.log", "error.log.1"), "整个名称");
        assert!(!matches("*.log", "catalog"));
        assert!(matches("access?.log", "access1.LOG"));
        assert!(!matches("access?.log", "access.log"));
        assert!(matches("a*b*c", "aXXbYYbc"), "退回重试");
        assert!(matches("*", ""));
        assert!(matches("**a**", "bab"));
        assert!(!matches("a*", "ba"));
        // [ 等不是通配符
        assert!(matches("[1]", "log[1].txt"));
        assert!(!matches("[1]", "log1.txt"));
    }

    #[test]
    fn find_command_quotes_for_the_shell() {
        assert_eq!(
            find_command(".", "log").as_deref(),
            Some("find . -iname '*log*'")
        );
        assert_eq!(
            find_command("/var/log", "*.gz").as_deref(),
            Some("find /var/log -iname '*.gz'")
        );
        // 包含的文字中的方括号、反斜杠当作普通字符
        assert_eq!(
            find_command(".", r"x[1]\y").as_deref(),
            Some(r"find . -iname '*x\[1]\\y*'")
        );
        // 有通配符时 * ? 照用，[ 与 \ 转义
        assert_eq!(
            find_command(".", "[x]*.log").as_deref(),
            Some(r"find . -iname '\[x]*.log'")
        );
        // 单引号、空格、中文与 $ 等：目录也加引号，其中的 ' 写成 '\''
        assert_eq!(
            find_command("/home/me/it's mine", "王's $x").as_deref(),
            Some(r"find '/home/me/it'\''s mine' -iname '*王'\''s $x*'")
        );
        assert_eq!(
            find_command("/srv/$(rm -rf ~)", "a").as_deref(),
            Some("find '/srv/$(rm -rf ~)' -iname '*a*'")
        );
        // 写进终端会变成按键的字符、解码不了的字节、空的：不给命令
        assert_eq!(find_command(".", "a\rb"), None);
        assert_eq!(find_command("/x\u{1b}[2J", "a"), None);
        assert_eq!(find_command("/\u{10FED6}", "a"), None);
        assert_eq!(find_command(".", ""), None);
        assert_eq!(find_command("", "a"), None);
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
