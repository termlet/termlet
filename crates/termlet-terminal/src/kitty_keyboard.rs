//! kitty 键盘协议（<https://sw.kovidgoyal.net/kitty/keyboard-protocol/>）中由程序设置的渐进增强标志。
//!
//! 程序用 `CSI > flags u` 压栈、`CSI < n u` 出栈、`CSI = flags ; mode u` 修改当前的标志、`CSI ? u` 查询
//! （回答 `CSI ? flags u`）。主屏幕与备用屏幕各有一个栈，当前的标志是所在屏幕栈顶的那一项；栈空时为 0，
//! 按键照旧编码，bash、vim 等不开启协议的程序不受影响。按键怎样编码见 [`crate::keys::encode_key`]。
//!
//! 标志栈属于内核的功能，由 alacritty 后端用这里的 [`KeyboardModes`] 自己维护，不用 alacritty_terminal 自带的实现：
//! 它回答查询时给的是栈顶而不是当前的标志（`CSI =` 修改后查询不对），栈满（4096 层）时还会去删标题栈而 panic。
//! 解析前由 [`crate::shell_events::OscScanner`] 找出这些序列，引擎在序列之后转给内核，内核按当时所在的屏幕更新状态、
//! 写出查询的回答；RIS（`ESC c`）清空两个栈，换上新的传输时也清空。换 libghostty 后标志栈由它提供，
//! [`KeyboardFlags`] 仍是引擎接口中的类型（[`crate::grid::TerminalModes::keyboard`]）。

use std::ops::BitOr;

/// 每个栈最多的项数：超出时丢掉最旧的一项（规范要求限制栈的大小，防止拒绝服务）。
pub const MAX_STACK_DEPTH: usize = 16;

/// 渐进增强标志。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct KeyboardFlags(u8);

impl KeyboardFlags {
    /// 不开启（旧的编码）
    pub const NONE: Self = Self(0);
    /// 0b1：消歧义转义码（Esc、带 Ctrl / Alt 的键等改用 `CSI u`）
    pub const DISAMBIGUATE: Self = Self(0b1);
    /// 0b10：报告按住重复与松开
    pub const REPORT_EVENTS: Self = Self(0b10);
    /// 0b100：同时报告按下 Shift 时的键
    pub const REPORT_ALTERNATES: Self = Self(0b100);
    /// 0b1000：所有的键（包括输入文字的键与回车、Tab、退格）都用转义码报告
    pub const REPORT_ALL_KEYS: Self = Self(0b1000);
    /// 0b10000：转义码中带上按键输入的文字（与 [`Self::REPORT_ALL_KEYS`] 一起用）
    pub const REPORT_TEXT: Self = Self(0b1_0000);
    const KNOWN: u16 = 0b1_1111;

    /// 程序给的数值，不认识的位忽略。
    pub fn from_bits(bits: u16) -> Self {
        Self((bits & Self::KNOWN) as u8)
    }

    pub fn bits(self) -> u8 {
        self.0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
}

impl BitOr for KeyboardFlags {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        self.with(other)
    }
}

/// `CSI = flags ; mode u` 的 mode。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetMode {
    /// 1（默认）：设为给出的标志
    Replace,
    /// 2：加上给出的标志
    Union,
    /// 3：去掉给出的标志
    Difference,
}

/// 程序的一个请求。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyboardRequest {
    /// `CSI > flags u`（flags 默认 0）
    Push(KeyboardFlags),
    /// `CSI < n u`（n 默认 1）
    Pop(u16),
    /// `CSI = flags ; mode u`
    Set(KeyboardFlags, SetMode),
    /// `CSI ? u`
    Query,
    /// RIS（`ESC c`）：两个栈都清空
    Reset,
}

impl KeyboardRequest {
    /// 由 CSI 的参数部分（结尾的 `u` 之前，以 `>`、`<`、`=`、`?` 开头）解析；不是键盘协议的请求时为 `None`。
    /// 与 vte 相同：参数只能是数字、`;` 与 `:`（子参数忽略），为空或 0 时用默认值，过大的数取 65535。
    pub fn parse(params: &[u8]) -> Option<Self> {
        let (&marker, rest) = params.split_first()?;
        if !rest
            .iter()
            .all(|b| b.is_ascii_digit() || matches!(b, b';' | b':'))
        {
            return None;
        }
        let mut values = rest.split(|&b| b == b';').map(|param| {
            let digits = param.split(|&b| b == b':').next().unwrap_or_default();
            digits.iter().fold(0u16, |n, &d| {
                n.saturating_mul(10).saturating_add(u16::from(d - b'0'))
            })
        });
        let mut next_or = |default: u16| match values.next() {
            Some(0) | None => default,
            Some(value) => value,
        };
        Some(match marker {
            b'>' => Self::Push(KeyboardFlags::from_bits(next_or(0))),
            b'<' => Self::Pop(next_or(1)),
            b'=' => {
                let flags = KeyboardFlags::from_bits(next_or(0));
                let mode = match next_or(1) {
                    1 => SetMode::Replace,
                    2 => SetMode::Union,
                    3 => SetMode::Difference,
                    _ => return None,
                };
                Self::Set(flags, mode)
            }
            b'?' => Self::Query,
            _ => return None,
        })
    }
}

/// 主屏幕与备用屏幕的标志栈。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyboardModes {
    main: Vec<KeyboardFlags>,
    alt: Vec<KeyboardFlags>,
}

impl KeyboardModes {
    /// 所在屏幕当前的标志。
    pub fn flags(&self, alt_screen: bool) -> KeyboardFlags {
        self.stack(alt_screen).last().copied().unwrap_or_default()
    }

    /// 处理程序的请求（`alt_screen`：请求时是否在备用屏幕中）；查询时返回要写回远端的回答。
    pub fn apply(&mut self, alt_screen: bool, request: KeyboardRequest) -> Option<String> {
        let stack = if alt_screen {
            &mut self.alt
        } else {
            &mut self.main
        };
        match request {
            KeyboardRequest::Push(flags) => {
                if stack.len() >= MAX_STACK_DEPTH {
                    stack.remove(0);
                }
                stack.push(flags);
            }
            KeyboardRequest::Pop(count) => {
                stack.truncate(stack.len().saturating_sub(usize::from(count)));
            }
            KeyboardRequest::Set(flags, mode) => {
                let current = stack.last().copied().unwrap_or_default();
                let next = match mode {
                    SetMode::Replace => flags,
                    SetMode::Union => current.with(flags),
                    SetMode::Difference => current.without(flags),
                };
                // 栈空时没有可改的一项：放一项作为底（之后出栈就回到 0）
                match stack.last_mut() {
                    Some(top) => *top = next,
                    None => stack.push(next),
                }
            }
            KeyboardRequest::Query => {
                return Some(format!("\x1b[?{}u", self.flags(alt_screen).bits()));
            }
            KeyboardRequest::Reset => self.clear(),
        }
        None
    }

    /// 两个栈都清空（换上新的传输、RIS）。
    pub fn clear(&mut self) {
        self.main.clear();
        self.alt.clear();
    }

    fn stack(&self, alt_screen: bool) -> &[KeyboardFlags] {
        if alt_screen { &self.alt } else { &self.main }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(bits: u16) -> KeyboardFlags {
        KeyboardFlags::from_bits(bits)
    }

    #[test]
    fn parses_requests_like_vte() {
        let parse = |s: &str| KeyboardRequest::parse(s.as_bytes());
        assert_eq!(parse(">1"), Some(KeyboardRequest::Push(flags(1))));
        assert_eq!(parse(">"), Some(KeyboardRequest::Push(KeyboardFlags::NONE)));
        // 不认识的位忽略；子参数忽略
        assert_eq!(parse(">255"), Some(KeyboardRequest::Push(flags(31))));
        assert_eq!(parse(">3:9;5"), Some(KeyboardRequest::Push(flags(3))));
        assert_eq!(parse("<"), Some(KeyboardRequest::Pop(1)));
        assert_eq!(parse("<0"), Some(KeyboardRequest::Pop(1)));
        assert_eq!(parse("<3"), Some(KeyboardRequest::Pop(3)));
        assert_eq!(parse("<99999999"), Some(KeyboardRequest::Pop(u16::MAX)));
        assert_eq!(
            parse("=5"),
            Some(KeyboardRequest::Set(flags(5), SetMode::Replace))
        );
        assert_eq!(
            parse("=2;2"),
            Some(KeyboardRequest::Set(flags(2), SetMode::Union))
        );
        assert_eq!(
            parse("=1;3"),
            Some(KeyboardRequest::Set(flags(1), SetMode::Difference))
        );
        assert_eq!(parse("?"), Some(KeyboardRequest::Query));
        for other in ["=1;4", "", "1", ">1 ", ">1$", ">a", "!1"] {
            assert_eq!(parse(other), None, "{other}");
        }
    }

    #[test]
    fn push_pop_and_query_per_screen() {
        let mut modes = KeyboardModes::default();
        let query = |modes: &mut KeyboardModes, alt| modes.apply(alt, KeyboardRequest::Query);
        assert_eq!(query(&mut modes, false).as_deref(), Some("\x1b[?0u"));
        modes.apply(false, KeyboardRequest::Push(flags(1)));
        modes.apply(false, KeyboardRequest::Push(flags(3)));
        assert_eq!(modes.flags(false), flags(3));
        assert_eq!(query(&mut modes, false).as_deref(), Some("\x1b[?3u"));
        // 备用屏幕有自己的栈
        assert_eq!(modes.flags(true), KeyboardFlags::NONE);
        modes.apply(true, KeyboardRequest::Push(flags(31)));
        assert_eq!(query(&mut modes, true).as_deref(), Some("\x1b[?31u"));
        modes.apply(true, KeyboardRequest::Pop(1));
        assert_eq!(modes.flags(true), KeyboardFlags::NONE);
        assert_eq!(modes.flags(false), flags(3));
        // 出栈回到上一项；出空了回到 0，多出的出栈不出错
        modes.apply(false, KeyboardRequest::Pop(1));
        assert_eq!(modes.flags(false), flags(1));
        modes.apply(false, KeyboardRequest::Pop(5));
        assert_eq!(modes.flags(false), KeyboardFlags::NONE);
        modes.apply(false, KeyboardRequest::Pop(u16::MAX));
        assert_eq!(modes, KeyboardModes::default());
    }

    #[test]
    fn set_changes_the_current_flags() {
        let mut modes = KeyboardModes::default();
        // 栈空时修改：放一项作为底
        modes.apply(false, KeyboardRequest::Set(flags(1), SetMode::Replace));
        assert_eq!(modes.flags(false), flags(1));
        modes.apply(false, KeyboardRequest::Set(flags(6), SetMode::Union));
        assert_eq!(modes.flags(false), flags(7));
        modes.apply(false, KeyboardRequest::Set(flags(2), SetMode::Difference));
        assert_eq!(modes.flags(false), flags(5));
        assert_eq!(
            modes.apply(false, KeyboardRequest::Query).as_deref(),
            Some("\x1b[?5u")
        );
        // 压栈后修改只改栈顶
        modes.apply(false, KeyboardRequest::Push(flags(8)));
        modes.apply(false, KeyboardRequest::Set(flags(16), SetMode::Union));
        assert_eq!(modes.flags(false), flags(24));
        modes.apply(false, KeyboardRequest::Pop(1));
        assert_eq!(modes.flags(false), flags(5));
        modes.apply(false, KeyboardRequest::Pop(1));
        assert_eq!(modes.flags(false), KeyboardFlags::NONE);
    }

    #[test]
    fn full_stack_drops_the_oldest_and_reset_clears_both() {
        let mut modes = KeyboardModes::default();
        for i in 0..(MAX_STACK_DEPTH as u16 + 100) {
            modes.apply(false, KeyboardRequest::Push(flags(i % 32)));
        }
        assert_eq!(modes.main.len(), MAX_STACK_DEPTH);
        assert_eq!(
            modes.flags(false),
            flags((MAX_STACK_DEPTH as u16 + 99) % 32)
        );
        // 丢掉的是最旧的：出栈到底只剩最近压入的那些
        modes.apply(false, KeyboardRequest::Pop(MAX_STACK_DEPTH as u16 - 1));
        assert_eq!(modes.flags(false), flags(100 % 32));
        modes.apply(true, KeyboardRequest::Push(flags(1)));
        modes.apply(false, KeyboardRequest::Reset);
        assert_eq!(modes, KeyboardModes::default());
    }
}
