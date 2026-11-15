//! 按命令历史给出的补全提示（思路同 fish 的 autosuggestion、zsh-autosuggestions）：在 shell 提示符后输入命令时，
//! 在光标后用淡色画出历史中最近一条以已输入内容开头的命令的剩余部分；→ 或 End 接受整条，Ctrl+→（macOS ⌥→，见
//! [`accept_word_combo`]）接受一个词，接受的部分与用户键入的一样发给远端。
//!
//! - 只在 shell 集成认出的输入区中提示（`TerminalEngine` 读出的 `PromptInput`）：收到过 B（输入开始）、还没有
//!   C（开始执行）；用户在这个提示符后输入过、没有按回车；主屏幕（不在全屏程序中）；光标在 B 所在的逻辑行上，且
//!   之后没有文字（光标在行尾）；已输入的内容不全是空白。没有 shell 集成、不回显的输入（读到的是空）、命令执行中都
//!   不提示。远端的 shell 自己画了提示（fish、zsh-autosuggestions，光标之后有文字）时也不再提示。
//! - 来源由使用方给出（[`CommandSuggestions`]：新的在前的命令），终端不知道它从哪里来；桌面端用本机加密保存的
//!   这台主机的命令历史。
//! - 提示不写进网格：每帧按快照算出画在哪些空白的格子上（`ghost_cells`），不影响选区、复制、查找与会话录制；
//!   光标所在行被程序改动、光标移开、改变大小（输入开始处的标记失效）时下一帧就消失或重算。

use gpui_kit::OperatingSystem;
use unicode_width::UnicodeWidthChar as _;

use crate::colors::Rgb;
use crate::grid::{CellFlags, GridPoint, GridSnapshot};

/// 提示的颜色在背景与前景之间的位置：次要文字色（界面主题的次要文字色大约在背景到正文颜色的 55%–65% 处），
/// 按终端自己的配色算，深浅配色下都看得清、又与键入的文字分得开。
const GHOST_WEIGHT: u16 = 55;

/// 提示的颜色：终端的前景色与背景色混合（见 [`GHOST_WEIGHT`]）。
pub(crate) fn ghost_color(foreground: Rgb, background: Rgb) -> Rgb {
    let mix =
        |f: u8, b: u8| ((f as u16 * GHOST_WEIGHT + b as u16 * (100 - GHOST_WEIGHT)) / 100) as u8;
    Rgb {
        r: mix(foreground.r, background.r),
        g: mix(foreground.g, background.g),
        b: mix(foreground.b, background.b),
    }
}

/// 接受一个词的按键：Windows、Linux 上 Ctrl+→，macOS 上 ⌥→（与在文字中按词移动的习惯相同）。没有提示时按键照常
/// 发给远端（shell 中多半是按词右移，光标已在行尾时什么也不做），见 `view::key_bindings`。
pub fn accept_word_combo(os: OperatingSystem) -> &'static str {
    if os == OperatingSystem::Mac {
        "alt-right"
    } else {
        "ctrl-right"
    }
}

/// 提示的来源：命令，新的（最近使用的）在前。命令可能含密码等秘密，`Debug` 只打印条数。
#[derive(Clone, Default)]
pub struct CommandSuggestions {
    commands: Vec<String>,
}

impl std::fmt::Debug for CommandSuggestions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CommandSuggestions({} 条)", self.commands.len())
    }
}

impl CommandSuggestions {
    /// `commands` 按新的在前排列。含控制字符的（多行命令等）不用：接受时会被远端当成按键（例如回车）执行。
    pub fn new(commands: impl IntoIterator<Item = String>) -> Self {
        Self {
            commands: commands
                .into_iter()
                .filter(|c| !c.is_empty() && !c.chars().any(char::is_control))
                .collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.commands.len()
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// 已输入 `typed` 时提示的剩余部分：最近一条以它开头（区分大小写）、比它长的命令去掉 `typed` 之后的部分。
    /// `typed` 为空或只有空白时没有提示。
    pub fn complete(&self, typed: &str) -> Option<&str> {
        if typed.trim().is_empty() {
            return None;
        }
        self.commands
            .iter()
            .find(|c| c.len() > typed.len() && c.starts_with(typed))
            .map(|c| &c[typed.len()..])
    }
}

/// shell 提示符后正在输入的命令（引擎线程读出，随快照交给界面，见模块说明的条件）。内容可能含秘密，`Debug` 只打印
/// 字符数。
#[derive(Clone)]
pub(crate) struct PromptInput {
    /// 从输入开始处（B）到光标的文字，保留末尾的空格（“git ” 与 “git” 的提示不同）
    pub(crate) text: String,
    /// 光标所在的格子：提示从这里画起
    pub(crate) cursor: GridPoint,
}

impl std::fmt::Debug for PromptInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PromptInput({} 个字符，光标 {:?})",
            self.text.chars().count(),
            self.cursor
        )
    }
}

/// 接受一个词：开头的空白，加上之后的一个词——到下一个空白为止，路径按 `/` 分段（`/` 归前一段）。
/// `" status --short"` → `" status"`，`"ar/log/nginx"` → `"ar/"`，`"/var/log"` → `"/var/"`。
pub(crate) fn next_word(rest: &str) -> &str {
    let body = rest.trim_start();
    let lead = rest.len() - body.len();
    // 开头的 `/` 连同之后的一段一起接受
    let slashes = body.len() - body.trim_start_matches('/').len();
    let end = body[slashes..]
        .char_indices()
        .find_map(|(i, c)| match c {
            '/' => Some(slashes + i + 1),
            c if c.is_whitespace() => Some(slashes + i),
            _ => None,
        })
        .unwrap_or(body.len());
    &rest[..lead + end]
}

/// 提示中的一个字符画在哪里：可见区的行、列，占几格，以及跟在它后面的组合字符。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GhostCell {
    pub(crate) row: usize,
    pub(crate) column: usize,
    pub(crate) wide: bool,
    pub(crate) text: String,
}

/// 把提示的剩余部分 `rest` 从光标处起排到可见区的格子上（可见区 `rows` 行、`columns` 列）。与终端自己排字的规则
/// 相同：写到行尾接着下一行，宽字符在行尾放不下时换到下一行；组合字符跟着前一个字符。只画在空白的格子上：遇到
/// 有文字的格子、出了可见区、遇到控制字符时停下，后面的不画。
pub(crate) fn ghost_cells(
    snapshot: &GridSnapshot,
    cursor: GridPoint,
    rest: &str,
    rows: usize,
    columns: usize,
) -> Vec<GhostCell> {
    let mut cells: Vec<GhostCell> = Vec::new();
    let offset = snapshot.display_offset() as i32;
    let mut line = cursor.line;
    let mut column = cursor.column;
    for c in rest.chars() {
        let Some(width) = c.width() else {
            break;
        };
        if width == 0 {
            if let Some(last) = cells.last_mut() {
                last.text.push(c);
            }
            continue;
        }
        if column + width > columns {
            line += 1;
            column = 0;
            if width > columns {
                break;
            }
        }
        let Ok(row) = usize::try_from(line + offset) else {
            break;
        };
        let free = (column..column + width).all(|col| {
            snapshot
                .cell(GridPoint::new(line, col))
                .is_some_and(|cell| cell.c == ' ' && !cell.flags.intersects(BUSY))
        });
        if row >= rows || !free {
            break;
        }
        cells.push(GhostCell {
            row,
            column,
            wide: width > 1,
            text: c.to_string(),
        });
        column += width;
    }
    cells
}

/// 不算空白的格子：宽字符及其占位、带组合字符的。
const BUSY: CellFlags = CellFlags::from_bits_retain(
    CellFlags::WIDE.bits()
        | CellFlags::WIDE_SPACER.bits()
        | CellFlags::LEADING_WIDE_SPACER.bits()
        | CellFlags::GRAPHEME.bits(),
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::Cell;

    fn suggestions(commands: &[&str]) -> CommandSuggestions {
        CommandSuggestions::new(commands.iter().map(|c| c.to_string()))
    }

    // 最近使用的优先；按前缀匹配、区分大小写；与已输入的相同、比它短的不算；空白不提示
    #[test]
    fn newest_matching_command_wins() {
        let history = suggestions(&["git status", "git stash pop", "Git log", "ls -la", "git"]);
        assert_eq!(history.complete("git s"), Some("tatus"));
        assert_eq!(history.complete("git st"), Some("atus"));
        assert_eq!(history.complete("git sta"), Some("tus"));
        assert_eq!(history.complete("git stas"), Some("h pop"));
        // 末尾的空格算在已输入的内容里
        assert_eq!(history.complete("git "), Some("status"));
        assert_eq!(history.complete("git"), Some(" status"));
        assert_eq!(history.complete("Git"), Some(" log"));
        assert_eq!(history.complete("git status"), None, "已经完整");
        assert_eq!(history.complete("git statusx"), None);
        assert_eq!(history.complete("vim"), None);
        assert_eq!(history.complete(""), None);
        assert_eq!(history.complete("   "), None);
        // 中文与宽字符：按字节前缀匹配，剩余部分从字符边界开始
        let history = suggestions(&["echo 中文测试", "echo 中"]);
        assert_eq!(history.complete("echo 中"), Some("文测试"));
    }

    // 含控制字符的命令（多行、制表符）不用：接受时会被当成回车等按键
    #[test]
    fn commands_with_control_characters_are_dropped() {
        let history = suggestions(&["make\nrm -rf /", "make\tall", "make build", ""]);
        assert_eq!(history.len(), 1);
        assert_eq!(history.complete("make"), Some(" build"));
        assert!(!format!("{history:?}").contains("make"), "Debug 不打印命令");
    }

    #[test]
    fn next_word_stops_at_spaces_and_path_separators() {
        assert_eq!(next_word("it status --short"), "it");
        assert_eq!(next_word(" status --short"), " status");
        assert_eq!(next_word("  --short"), "  --short");
        assert_eq!(next_word("ar/log/nginx"), "ar/");
        assert_eq!(next_word("/var/log"), "/var/");
        assert_eq!(next_word("log"), "log");
        assert_eq!(next_word("中文 测试"), "中文");
        assert_eq!(next_word("   "), "   ");
        assert_eq!(next_word(""), "");
    }

    // 提示的颜色在背景与前景之间，比正文淡、比背景亮（深浅配色都是）
    #[test]
    fn ghost_color_sits_between_background_and_foreground() {
        let rgb = |r, g, b| Rgb { r, g, b };
        let dark = ghost_color(rgb(212, 212, 216), rgb(30, 30, 31));
        assert!(dark.r > 100 && dark.r < 140, "{dark:?}");
        let light = ghost_color(rgb(30, 30, 30), rgb(255, 255, 255));
        assert!(light.r > 110 && light.r < 150, "{light:?}");
        assert_eq!(ghost_color(rgb(9, 9, 9), rgb(9, 9, 9)), rgb(9, 9, 9));
    }

    #[test]
    fn accept_word_key_follows_the_os() {
        assert_eq!(accept_word_combo(OperatingSystem::Windows), "ctrl-right");
        assert_eq!(accept_word_combo(OperatingSystem::Linux), "ctrl-right");
        assert_eq!(accept_word_combo(OperatingSystem::Mac), "alt-right");
    }

    /// `columns` 列的快照，从第 0 行起每行按 `lines` 中的一段填（不足的是空格）。
    fn snapshot(columns: usize, lines: &[&str]) -> GridSnapshot {
        let mut snapshot = GridSnapshot {
            columns,
            screen_lines: lines.len(),
            cells: vec![Cell::default(); columns * lines.len()],
            wraps: vec![false; lines.len()],
            ..GridSnapshot::default()
        };
        for (line, text) in lines.iter().enumerate() {
            for (column, c) in text.chars().enumerate() {
                snapshot
                    .cell_mut(GridPoint::new(line as i32, column))
                    .unwrap()
                    .c = c;
            }
        }
        snapshot
    }

    fn placed(cells: &[GhostCell]) -> Vec<(usize, usize, bool, &str)> {
        cells
            .iter()
            .map(|c| (c.row, c.column, c.wide, c.text.as_str()))
            .collect()
    }

    // 从光标处起排；写到行尾接着下一行，宽字符在行尾放不下时换行；出了可见区的不画
    #[test]
    fn ghost_wraps_like_the_terminal() {
        let screen = snapshot(8, &["$ git", "", ""]);
        let cursor = GridPoint::new(0, 5);
        assert_eq!(
            placed(&ghost_cells(&screen, cursor, " st", 3, 8)),
            vec![(0, 5, false, " "), (0, 6, false, "s"), (0, 7, false, "t")]
        );
        assert_eq!(
            placed(&ghost_cells(&screen, cursor, " status", 3, 8)),
            vec![
                (0, 5, false, " "),
                (0, 6, false, "s"),
                (0, 7, false, "t"),
                (1, 0, false, "a"),
                (1, 1, false, "t"),
                (1, 2, false, "u"),
                (1, 3, false, "s"),
            ]
        );
        // 宽字符：第 7 列只剩一格，“文”换到下一行
        assert_eq!(
            placed(&ghost_cells(&screen, cursor, "中文", 3, 8)),
            vec![(0, 5, true, "中"), (1, 0, true, "文")]
        );
        // 组合字符跟着前一个字符
        assert_eq!(
            placed(&ghost_cells(&screen, cursor, "e\u{301}x", 3, 8)),
            vec![(0, 5, false, "e\u{301}"), (0, 6, false, "x")]
        );
        // 只剩一行可见：换到下一行的部分不画
        assert_eq!(
            ghost_cells(&screen, cursor, " status", 1, 8).len(),
            3,
            "出了可见区"
        );
    }

    // 只画在空白的格子上：碰到有文字的格子就停
    #[test]
    fn ghost_stops_at_existing_text() {
        let screen = snapshot(10, &["$ ls    #", "x"]);
        let cursor = GridPoint::new(0, 4);
        assert_eq!(
            placed(&ghost_cells(&screen, cursor, " -la /tmp", 2, 10)),
            vec![
                (0, 4, false, " "),
                (0, 5, false, "-"),
                (0, 6, false, "l"),
                (0, 7, false, "a"),
            ]
        );
        let screen = snapshot(6, &["$ ls", "busy"]);
        assert_eq!(
            ghost_cells(&screen, GridPoint::new(0, 4), " -la", 2, 6).len(),
            2,
            "下一行开头有文字"
        );
        // 控制字符停下
        let screen = snapshot(10, &["$ ", ""]);
        assert_eq!(
            ghost_cells(&screen, GridPoint::new(0, 2), "ab\ncd", 2, 10).len(),
            2
        );
    }

    // 往回翻时按可见区的行算；光标不在可见区中时不画
    #[test]
    fn ghost_follows_the_display_offset() {
        // 往回翻了一行：可见区的第 0 行是回滚中的 -1 行，光标所在的第 0 行在可见区的第 1 行
        let mut screen = snapshot(10, &["old", "$ "]);
        screen.display_offset = 1;
        screen.top = -1;
        assert_eq!(
            placed(&ghost_cells(&screen, GridPoint::new(0, 2), "ls", 2, 10)),
            vec![(1, 2, false, "l"), (1, 3, false, "s")]
        );
        assert!(
            ghost_cells(&screen, GridPoint::new(0, 2), "ls", 1, 10).is_empty(),
            "只有一行可见"
        );
        let mut screen = snapshot(10, &["$ "]);
        screen.display_offset = 3;
        assert!(
            ghost_cells(&screen, GridPoint::new(0, 2), "ls", 1, 10).is_empty(),
            "光标在可见区之下"
        );
    }
}
