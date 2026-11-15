//! 按键编码：把 GPUI 的按键转成发给远端的字节（xterm 的规则）。
//!
//! 不带 Ctrl / Alt 的可打印字符不在这里处理（返回 `None`），交给输入法路径（`replace_text_in_range`），
//! 这样中文输入法、死键等都能正常工作。带 ⌘（macOS）、Win 键的组合是应用与系统的快捷键，也不编码，不写进终端。
//! Alt 加字符键在 Windows、Linux 上作为 Meta（ESC 前缀）；macOS 上 Option 照系统的习惯输入组合字符（[`AltKey`]）。
//!
//! 程序开启了 kitty 键盘协议（见 [`crate::kitty_keyboard`]）时按它的规范编码（[`encode_key`]）：
//!
//! - 消歧义（0b1）：Esc、带修饰键的回车 / Tab / 退格（Shift+Enter 为 `CSI 13;2u`）、带 Ctrl / Alt 的字符键
//!   改用 `CSI 编号;修饰键 u`；功能键用 `CSI 1;修饰键 X`、`CSI 编号;修饰键 ~` 的形式（F3 为 `CSI 13~`，
//!   不按应用光标键模式改用 SS3）；不带修饰键的回车、Tab、退格照旧发控制字符。
//! - 报告事件类型（0b10）：用转义码报告的键另报按住重复（`:2`）与松开（`:3`），回车、Tab、退格不报松开
//!   （除非同时开了 0b1000）。
//! - 报告替代键（0b100）：带 Shift 的字符键附上按 Shift 后的字符。
//! - 所有的键用转义码（0b1000）、附带文字（0b10000）：输入文字的键也用转义码报告，按需带上文字的码位。
//!
//! 做不到的部分：GPUI 不区分左右修饰键、小键盘与主键盘，也不单独报告修饰键本身的按下与松开，因此不报告
//! 修饰键本身、小键盘的专用编号；不报告 Caps Lock、Num Lock 状态；替代键中不带"标准布局的键"；GPUI 把
//! Shift+数字、符号键直接报告为按 Shift 后的字符（Shift 不再算修饰键），这些键的编号是该字符。AltGr 与输入法
//! 组合出的文字照旧作为文字发送。

use std::borrow::Cow;

use gpui_kit::{Keystroke, OperatingSystem};

use crate::grid::TerminalModes;
use crate::kitty_keyboard::KeyboardFlags;

/// Alt（macOS 上的 Option）加字符键怎么处理。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AltKey {
    /// 作为 Meta：ESC 前缀加上这个键的字符（Windows、Linux，与 xterm 相同）
    Meta,
    /// macOS 上 Option 作为 Meta（设置“Option 键作为 Meta 键”，与 Terminal.app、iTerm2 的同名选项相同）：ESC 前缀
    /// 加上不按 Option 时这个键的字符（⌥B 为 `ESC b`，不是 `ESC ∫`）。左右两个 Option 键一样（iTerm2 可以分开设，
    /// GPUI 不区分左右修饰键）
    OptionMeta,
    /// 照系统的习惯输入组合字符（macOS：⌥A 得到 å，⌥E 再按 E 得到 é），交给文字输入
    Compose,
}

impl AltKey {
    /// 这个系统上的习惯。
    pub fn for_os(os: OperatingSystem) -> Self {
        Self::for_settings(os, false)
    }

    /// 按设置：macOS 上开启了“Option 键作为 Meta 键”时为 [`AltKey::OptionMeta`]，没开启时照系统的习惯输入组合字符；
    /// 其他系统上 Alt 总是 Meta（设置只在 macOS 上显示）。
    pub fn for_settings(os: OperatingSystem, option_as_meta: bool) -> Self {
        match os {
            OperatingSystem::Mac if option_as_meta => AltKey::OptionMeta,
            OperatingSystem::Mac => AltKey::Compose,
            _ => AltKey::Meta,
        }
    }

    /// [`AltKey::OptionMeta`] 按 Meta 编码：macOS 给出的输入文字是按着 Option 组合出的（⌥A 为 å），去掉它，之后按
    /// 键名与 Shift 取字符。其余的原样。
    fn resolve(self, keystroke: &Keystroke) -> (Cow<'_, Keystroke>, AltKey) {
        match self {
            AltKey::OptionMeta if keystroke.modifiers.alt => (
                Cow::Owned(Keystroke {
                    key_char: None,
                    ..keystroke.clone()
                }),
                AltKey::Meta,
            ),
            AltKey::OptionMeta => (Cow::Borrowed(keystroke), AltKey::Meta),
            _ => (Cow::Borrowed(keystroke), self),
        }
    }
}

/// xterm 的修饰键参数：1 + Shift(1) + Alt(2) + Ctrl(4)。没有修饰键时为 `None`。
fn modifier_param(keystroke: &Keystroke) -> Option<u8> {
    let m = &keystroke.modifiers;
    let value = 1 + (m.shift as u8) + ((m.alt as u8) << 1) + ((m.control as u8) << 2);
    (value > 1).then_some(value)
}

/// 编码一次按键。返回 `None` 表示不是终端要处理的键（交给文字输入或快捷键）。
/// 同步输入中把来源终端编码的方向键与 Home / End 转成目标终端的光标键模式：应用模式为 `ESC O x`，
/// 普通模式为 `ESC [ x`。带修饰键的（`ESC [1;5A`）与其余内容不变。
pub fn adapt_cursor_keys(data: &[u8], app_cursor: bool) -> Cow<'_, [u8]> {
    let is_cursor = |b: u8| matches!(b, b'A' | b'B' | b'C' | b'D' | b'H' | b'F');
    let mut out: Option<Vec<u8>> = None;
    let mut i = 0;
    while i < data.len() {
        if data[i] == 0x1b
            && i + 2 < data.len()
            && matches!(data[i + 1], b'[' | b'O')
            && is_cursor(data[i + 2])
        {
            let wanted = if app_cursor { b'O' } else { b'[' };
            if data[i + 1] != wanted {
                out.get_or_insert_with(|| data.to_vec())[i + 1] = wanted;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    match out {
        Some(out) => Cow::Owned(out),
        None => Cow::Borrowed(data),
    }
}

pub fn encode(
    keystroke: &Keystroke,
    mode: TerminalModes,
    alt: AltKey,
) -> Option<Cow<'static, [u8]>> {
    let (keystroke, alt) = alt.resolve(keystroke);
    let keystroke = keystroke.as_ref();
    let m = &keystroke.modifiers;
    // ⌘、Win 键的组合是应用与系统的快捷键，不作为输入
    if m.platform {
        return None;
    }
    let key = keystroke.key.as_str();
    let app_cursor = mode.app_cursor;
    let param = modifier_param(keystroke);

    // 光标键、Home、End：无修饰键时按应用模式选 SS3 或 CSI；有修饰键时为 CSI 1;m X
    let cursor_key = |code: u8| -> Cow<'static, [u8]> {
        match param {
            None if app_cursor => Cow::Owned(vec![0x1b, b'O', code]),
            None => Cow::Owned(vec![0x1b, b'[', code]),
            Some(p) => Cow::Owned(format!("\x1b[1;{p}{}", code as char).into_bytes()),
        }
    };
    // Insert、Delete、PageUp、PageDown、F5 以上：CSI n ~ 或 CSI n;m ~
    let tilde_key = |n: u8| -> Cow<'static, [u8]> {
        match param {
            None => Cow::Owned(format!("\x1b[{n}~").into_bytes()),
            Some(p) => Cow::Owned(format!("\x1b[{n};{p}~").into_bytes()),
        }
    };
    // F1–F4：SS3 P–S，有修饰键时 CSI 1;m P–S
    let pf_key = |code: u8| -> Cow<'static, [u8]> {
        match param {
            None => Cow::Owned(vec![0x1b, b'O', code]),
            Some(p) => Cow::Owned(format!("\x1b[1;{p}{}", code as char).into_bytes()),
        }
    };

    let bytes: Cow<'static, [u8]> = match key {
        "enter" => {
            let enter: &[u8] = if mode.line_feed_new_line {
                b"\r\n"
            } else {
                b"\r"
            };
            if m.alt {
                Cow::Owned([b"\x1b".as_slice(), enter].concat())
            } else {
                Cow::Borrowed(enter)
            }
        }
        "backspace" => match (m.control, m.alt) {
            (true, _) => Cow::Borrowed(b"\x08"),
            (false, true) => Cow::Borrowed(b"\x1b\x7f"),
            (false, false) => Cow::Borrowed(b"\x7f"),
        },
        "tab" if m.shift => Cow::Borrowed(b"\x1b[Z"),
        "tab" => Cow::Borrowed(b"\t"),
        "escape" if m.alt => Cow::Borrowed(b"\x1b\x1b"),
        "escape" => Cow::Borrowed(b"\x1b"),
        "up" => cursor_key(b'A'),
        "down" => cursor_key(b'B'),
        "right" => cursor_key(b'C'),
        "left" => cursor_key(b'D'),
        "home" => cursor_key(b'H'),
        "end" => cursor_key(b'F'),
        "insert" => tilde_key(2),
        "delete" => tilde_key(3),
        "pageup" => tilde_key(5),
        "pagedown" => tilde_key(6),
        "f1" => pf_key(b'P'),
        "f2" => pf_key(b'Q'),
        "f3" => pf_key(b'R'),
        "f4" => pf_key(b'S'),
        "f5" => tilde_key(15),
        "f6" => tilde_key(17),
        "f7" => tilde_key(18),
        "f8" => tilde_key(19),
        "f9" => tilde_key(20),
        "f10" => tilde_key(21),
        "f11" => tilde_key(23),
        "f12" => tilde_key(24),
        "space" if m.control => Cow::Borrowed(b"\x00"),
        "space" if m.alt && alt == AltKey::Meta => Cow::Borrowed(b"\x1b "),
        _ => return encode_character(keystroke, alt),
    };
    Some(bytes)
}

/// 带 Ctrl 或 Alt 的字符键。
fn encode_character(keystroke: &Keystroke, alt: AltKey) -> Option<Cow<'static, [u8]>> {
    let m = &keystroke.modifiers;
    if m.platform || m.function {
        return None;
    }
    // AltGr（Windows 上表现为 Ctrl + Alt）组合出的字符会再经 WM_CHAR 送到文字输入，这里不处理，免得输入两次
    if m.control
        && m.alt
        && keystroke
            .key_char
            .as_ref()
            .is_some_and(|c| !c.chars().any(char::is_control))
    {
        return None;
    }
    let key = keystroke.key.as_str();
    let mut chars = key.chars();
    let (Some(ch), None) = (chars.next(), chars.next()) else {
        return None;
    };
    if m.control {
        let code = control_code(ch, m.shift)?;
        let mut bytes = Vec::with_capacity(2);
        if m.alt && alt == AltKey::Meta {
            bytes.push(0x1b);
        }
        bytes.push(code);
        return Some(Cow::Owned(bytes));
    }
    if m.alt && alt == AltKey::Meta {
        // Alt 作为 Meta：ESC 前缀加上这个键本来会输入的字符
        let text = keystroke.key_char.clone().unwrap_or_else(|| {
            if m.shift {
                ch.to_uppercase().collect()
            } else {
                ch.to_string()
            }
        });
        return Some(Cow::Owned([b"\x1b".as_slice(), text.as_bytes()].concat()));
    }
    None
}

/// Ctrl 加一个键的控制字符（xterm 的规则）。
fn control_code(ch: char, shift: bool) -> Option<u8> {
    let ch = ch.to_ascii_lowercase();
    Some(match ch {
        'a'..='z' => ch as u8 - b'a' + 1,
        '@' | '2' | ' ' => 0x00,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '7' | '/' => 0x1f,
        '-' if shift => 0x1f,
        '8' | '?' => 0x7f,
        _ => return None,
    })
}

/// 按键事件的类型（kitty 键盘协议的"报告事件类型"）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyEventKind {
    Press,
    /// 按住不放时的重复
    Repeat,
    Release,
}

/// 编码后的一次按键。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedKey {
    pub bytes: Cow<'static, [u8]>,
    /// 松开时也要报告：调用方记下这个键（[`Keystroke::key`]），松开时用 [`encode_key`] 编码
    /// [`KeyEventKind::Release`]
    pub report_release: bool,
}

/// kitty 键盘协议的修饰键位（另加 1 后写进转义码）。
const SHIFT: u8 = 0b1;
const ALT: u8 = 0b10;
const CTRL: u8 = 0b100;

/// kitty 键盘协议中的一个键。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KittyKey {
    Escape,
    /// 回车（13）、Tab（9）、退格（127）：不带修饰键时照旧发控制字符
    Control(u32),
    /// 不输入文字的功能键：`CSI 编号;修饰键 结尾`，结尾为 `u`、`~` 或字母（编号为 1 且没有其余参数时省略）
    Functional(u32, u8),
    /// 输入文字的键：编号是不按 Shift 时的字符
    Text(char),
}

impl KittyKey {
    fn of(key: &str) -> Option<Self> {
        let functional = |number, end| Some(Self::Functional(number, end));
        match key {
            "escape" => Some(Self::Escape),
            "enter" => Some(Self::Control(13)),
            "tab" => Some(Self::Control(9)),
            "backspace" => Some(Self::Control(127)),
            "space" => Some(Self::Text(' ')),
            "insert" => functional(2, b'~'),
            "delete" => functional(3, b'~'),
            "pageup" => functional(5, b'~'),
            "pagedown" => functional(6, b'~'),
            "up" => functional(1, b'A'),
            "down" => functional(1, b'B'),
            "right" => functional(1, b'C'),
            "left" => functional(1, b'D'),
            "home" => functional(1, b'H'),
            "end" => functional(1, b'F'),
            "f1" => functional(1, b'P'),
            "f2" => functional(1, b'Q'),
            "f3" => functional(13, b'~'),
            "f4" => functional(1, b'S'),
            "f5" => functional(15, b'~'),
            "f6" => functional(17, b'~'),
            "f7" => functional(18, b'~'),
            "f8" => functional(19, b'~'),
            "f9" => functional(20, b'~'),
            "f10" => functional(21, b'~'),
            "f11" => functional(23, b'~'),
            "f12" => functional(24, b'~'),
            "menu" => functional(57363, b'u'),
            _ => {
                // F13–F24 在私用区：57376 起
                if let Some(n) = key.strip_prefix('f').and_then(|n| n.parse::<u32>().ok())
                    && (13..=24).contains(&n)
                {
                    return functional(57376 + n - 13, b'u');
                }
                let mut chars = key.chars();
                let (Some(ch), None) = (chars.next(), chars.next()) else {
                    return None;
                };
                let mut lower = ch.to_lowercase();
                match (lower.next(), lower.next()) {
                    (Some(lower), None) => Some(Self::Text(lower)),
                    _ => Some(Self::Text(ch)),
                }
            }
        }
    }
}

/// 按程序开启的 kitty 键盘协议标志编码一次按键事件；`flags` 不含 0b1、0b10、0b1000 时与 [`encode`] 相同
/// （松开不报告）。返回 `None` 表示不是终端要处理的键（交给文字输入或快捷键），松开时表示不报告。
/// 带 ⌘、Win 键的组合不编码；`alt` 为 [`AltKey::Compose`] 时带 Option 的字符键也不编码（交给文字输入）。
pub fn encode_key(
    keystroke: &Keystroke,
    mode: TerminalModes,
    flags: KeyboardFlags,
    kind: KeyEventKind,
    alt: AltKey,
) -> Option<EncodedKey> {
    let (keystroke, alt) = alt.resolve(keystroke);
    let keystroke = keystroke.as_ref();
    let m = &keystroke.modifiers;
    if m.platform {
        return None;
    }
    if alt == AltKey::Compose
        && m.alt
        && !m.control
        && matches!(KittyKey::of(&keystroke.key), Some(KittyKey::Text(_)))
    {
        return None;
    }
    let legacy = || {
        if kind == KeyEventKind::Release {
            return None;
        }
        encode(keystroke, mode, alt).map(|bytes| EncodedKey {
            bytes,
            report_release: false,
        })
    };
    let all = flags.contains(KeyboardFlags::REPORT_ALL_KEYS);
    let disambiguate = all || flags.contains(KeyboardFlags::DISAMBIGUATE);
    let events = flags.contains(KeyboardFlags::REPORT_EVENTS);
    // AltGr（Windows 上表现为 Ctrl + Alt）组合出的字符照旧作为文字输入（与 encode_character 相同）
    let altgr = m.control
        && m.alt
        && keystroke
            .key_char
            .as_ref()
            .is_some_and(|c| !c.chars().any(char::is_control));
    let key = match KittyKey::of(&keystroke.key) {
        Some(key) if (disambiguate || events) && !altgr => key,
        _ => return legacy(),
    };
    let bits = [(m.shift, SHIFT), (m.alt, ALT), (m.control, CTRL)]
        .into_iter()
        .filter(|&(on, _)| on)
        .fold(0, |bits, (_, bit)| bits | bit);
    let escaped = all
        || match key {
            KittyKey::Escape => disambiguate,
            KittyKey::Control(_) => disambiguate && bits != 0,
            KittyKey::Functional(..) => true,
            KittyKey::Text(_) => disambiguate && bits & !SHIFT != 0,
        };
    if !escaped {
        return legacy();
    }
    // 回车、Tab、退格不报告松开（程序没有关闭协议就退出时，用户仍能输入 reset 回车），除非所有的键都用转义码
    let report_release = events && (all || !matches!(key, KittyKey::Control(_)));
    if kind == KeyEventKind::Release && !report_release {
        return None;
    }

    let (number, end) = match key {
        KittyKey::Escape => (27, b'u'),
        KittyKey::Control(number) => (number, b'u'),
        KittyKey::Functional(number, end) => (number, end),
        KittyKey::Text(ch) => (u32::from(ch), b'u'),
    };
    let mut params = number.to_string();
    if let KittyKey::Text(ch) = key
        && flags.contains(KeyboardFlags::REPORT_ALTERNATES)
        && bits & SHIFT != 0
        && let Some(shifted) = shifted_char(keystroke, ch)
    {
        params.push_str(&format!(":{}", u32::from(shifted)));
    }
    let event = match kind {
        KeyEventKind::Press => None,
        KeyEventKind::Repeat => events.then_some(2),
        KeyEventKind::Release => Some(3),
    };
    // 附带的文字：按下（重复）时这个键输入的文字；带 Ctrl / Alt 时没有
    let text = (all
        && flags.contains(KeyboardFlags::REPORT_TEXT)
        && kind != KeyEventKind::Release
        && bits & !SHIFT == 0)
        .then(|| key_text(keystroke, key))
        .flatten();
    if bits != 0 || event.is_some() || text.is_some() {
        params.push_str(&format!(";{}", 1 + bits));
        if let Some(event) = event {
            params.push_str(&format!(":{event}"));
        }
    }
    if let Some(text) = text {
        let codepoints: Vec<String> = text.chars().map(|c| u32::from(c).to_string()).collect();
        params.push(';');
        params.push_str(&codepoints.join(":"));
    }
    if end.is_ascii_alphabetic() && end != b'u' && params == "1" {
        params.clear();
    }
    Some(EncodedKey {
        bytes: Cow::Owned(format!("\x1b[{params}{}", end as char).into_bytes()),
        report_release,
    })
}

/// 按 Shift 后的字符（与不按时不同才有）：GPUI 给出的输入文字，没有时（例如同时按着 Ctrl）取大写。
fn shifted_char(keystroke: &Keystroke, ch: char) -> Option<char> {
    let from_text = keystroke.key_char.as_deref().and_then(|text| {
        let mut chars = text.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => Some(c),
            _ => None,
        }
    });
    let shifted = from_text.or_else(|| {
        let mut upper = ch.to_uppercase();
        match (upper.next(), upper.next()) {
            (Some(c), None) => Some(c),
            _ => None,
        }
    })?;
    (shifted != ch).then_some(shifted)
}

/// 这个键输入的文字（不含控制字符）。
fn key_text(keystroke: &Keystroke, key: KittyKey) -> Option<String> {
    let KittyKey::Text(ch) = key else {
        return None;
    };
    let text = keystroke.key_char.clone().unwrap_or_else(|| {
        if keystroke.modifiers.shift {
            ch.to_uppercase().collect()
        } else {
            ch.to_string()
        }
    });
    (!text.is_empty() && !text.chars().any(char::is_control)).then_some(text)
}

/// 粘贴：远端开启了括号粘贴时用 `ESC[200~ … ESC[201~` 包起来；换行统一成回车（与 xterm 相同）。
///
/// 调用前应已用粘贴防护去掉控制字符（含 `ESC[201~`，防止提前结束括号粘贴）。
pub fn encode_paste(text: &str, mode: TerminalModes) -> Vec<u8> {
    let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
    if mode.bracketed_paste {
        let mut bytes = Vec::with_capacity(normalized.len() + 12);
        bytes.extend_from_slice(b"\x1b[200~");
        bytes.extend_from_slice(normalized.as_bytes());
        bytes.extend_from_slice(b"\x1b[201~");
        bytes
    } else {
        normalized.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(spec: &'static str) -> Keystroke {
        Keystroke::parse(spec).unwrap()
    }

    fn enc(spec: &'static str) -> Option<Vec<u8>> {
        encode(&key(spec), TerminalModes::default(), AltKey::Meta).map(|b| b.into_owned())
    }

    #[test]
    fn cursor_keys_follow_the_target_mode() {
        assert_eq!(&*adapt_cursor_keys(b"\x1b[A", true), b"\x1bOA");
        assert_eq!(&*adapt_cursor_keys(b"\x1bOB", false), b"\x1b[B");
        assert_eq!(&*adapt_cursor_keys(b"\x1b[H\x1b[F", true), b"\x1bOH\x1bOF");
        // 已经是目标模式、带修饰键、其余序列不变
        assert!(matches!(
            adapt_cursor_keys(b"\x1b[A", false),
            Cow::Borrowed(_)
        ));
        assert_eq!(&*adapt_cursor_keys(b"\x1b[1;5A", true), b"\x1b[1;5A");
        assert_eq!(&*adapt_cursor_keys(b"\x1b[3~", true), b"\x1b[3~");
        assert_eq!(&*adapt_cursor_keys(b"ls\r", true), b"ls\r");
    }

    #[test]
    fn named_keys() {
        assert_eq!(enc("enter").unwrap(), b"\r");
        assert_eq!(enc("backspace").unwrap(), b"\x7f");
        assert_eq!(enc("tab").unwrap(), b"\t");
        assert_eq!(enc("shift-tab").unwrap(), b"\x1b[Z");
        assert_eq!(enc("escape").unwrap(), b"\x1b");
        assert_eq!(enc("delete").unwrap(), b"\x1b[3~");
        assert_eq!(enc("f1").unwrap(), b"\x1bOP");
        assert_eq!(enc("f5").unwrap(), b"\x1b[15~");
        assert_eq!(enc("f12").unwrap(), b"\x1b[24~");
    }

    #[test]
    fn cursor_keys_follow_application_mode_and_modifiers() {
        assert_eq!(enc("up").unwrap(), b"\x1b[A");
        let app = encode(
            &key("up"),
            TerminalModes {
                app_cursor: true,
                ..TerminalModes::default()
            },
            AltKey::Meta,
        )
        .unwrap();
        assert_eq!(&*app, b"\x1bOA");
        assert_eq!(enc("ctrl-left").unwrap(), b"\x1b[1;5D");
        assert_eq!(enc("shift-end").unwrap(), b"\x1b[1;2F");
        assert_eq!(enc("alt-pagedown").unwrap(), b"\x1b[6;3~");
    }

    #[test]
    fn control_and_alt_characters() {
        assert_eq!(enc("ctrl-c").unwrap(), b"\x03");
        assert_eq!(enc("ctrl-a").unwrap(), b"\x01");
        assert_eq!(enc("ctrl-[").unwrap(), b"\x1b");
        assert_eq!(enc("ctrl-space").unwrap(), b"\x00");
        assert_eq!(enc("alt-b").unwrap(), b"\x1bb");
        // 没有 AltGr 字符时 Ctrl+Alt+X 是 ESC 加 ^X（xterm）
        assert_eq!(enc("ctrl-alt-x").unwrap(), b"\x1b\x18");
        // AltGr 打出的字符（德语键盘 AltGr+Q = @）交给文字输入，不在这里编码
        let altgr = Keystroke {
            key_char: Some("@".into()),
            ..key("ctrl-alt-q")
        };
        assert_eq!(encode(&altgr, TerminalModes::default(), AltKey::Meta), None);
    }

    #[test]
    fn plain_text_goes_through_input_handler() {
        assert_eq!(enc("a"), None);
        assert_eq!(enc("shift-a"), None);
    }

    // ---- kitty 键盘协议 ----

    fn typed(spec: &'static str, text: &str) -> Keystroke {
        Keystroke {
            key_char: Some(text.into()),
            ..key(spec)
        }
    }

    fn kitty_event(
        keystroke: &Keystroke,
        mode: TerminalModes,
        bits: u16,
        kind: KeyEventKind,
    ) -> Option<Vec<u8>> {
        encode_key(
            keystroke,
            mode,
            KeyboardFlags::from_bits(bits),
            kind,
            AltKey::Meta,
        )
        .map(|k| k.bytes.into_owned())
    }

    fn kitty(spec: &'static str, bits: u16) -> Option<Vec<u8>> {
        kitty_event(
            &key(spec),
            TerminalModes::default(),
            bits,
            KeyEventKind::Press,
        )
    }

    #[test]
    fn without_the_protocol_keys_are_unchanged() {
        let app = TerminalModes {
            app_cursor: true,
            ..TerminalModes::default()
        };
        for spec in [
            "enter",
            "shift-enter",
            "alt-enter",
            "tab",
            "shift-tab",
            "backspace",
            "ctrl-backspace",
            "escape",
            "alt-escape",
            "up",
            "ctrl-left",
            "f1",
            "f3",
            "delete",
            "ctrl-c",
            "ctrl-shift-a",
            "alt-b",
            "ctrl-space",
            "a",
            "shift-a",
        ] {
            for mode in [TerminalModes::default(), app] {
                let flags = [KeyboardFlags::NONE, KeyboardFlags::REPORT_ALTERNATES];
                for flags in flags {
                    let got =
                        encode_key(&key(spec), mode, flags, KeyEventKind::Press, AltKey::Meta)
                            .map(|k| k.bytes.into_owned());
                    let want = encode(&key(spec), mode, AltKey::Meta).map(|b| b.into_owned());
                    assert_eq!(got, want, "{spec}");
                    let release =
                        encode_key(&key(spec), mode, flags, KeyEventKind::Release, AltKey::Meta);
                    assert_eq!(release, None, "{spec}");
                }
            }
        }
        // 没有开启协议时 Shift+Enter 与回车相同
        assert_eq!(kitty("shift-enter", 0).unwrap(), b"\r");
    }

    #[test]
    fn disambiguate_encodes_modified_control_keys_and_esc() {
        let d = |spec: &'static str| kitty(spec, 1);
        // 回车、Tab、退格：不带修饰键照旧，带修饰键（包括 Shift）用 CSI u
        assert_eq!(d("enter").unwrap(), b"\r");
        assert_eq!(d("shift-enter").unwrap(), b"\x1b[13;2u");
        assert_eq!(d("ctrl-enter").unwrap(), b"\x1b[13;5u");
        assert_eq!(d("alt-enter").unwrap(), b"\x1b[13;3u");
        assert_eq!(d("tab").unwrap(), b"\t");
        assert_eq!(d("shift-tab").unwrap(), b"\x1b[9;2u");
        assert_eq!(d("ctrl-shift-tab").unwrap(), b"\x1b[9;6u");
        assert_eq!(d("backspace").unwrap(), b"\x7f");
        assert_eq!(d("ctrl-backspace").unwrap(), b"\x1b[127;5u");
        assert_eq!(d("alt-backspace").unwrap(), b"\x1b[127;3u");
        // Esc 总是 CSI u
        assert_eq!(d("escape").unwrap(), b"\x1b[27u");
        assert_eq!(d("shift-escape").unwrap(), b"\x1b[27;2u");
        assert_eq!(d("alt-escape").unwrap(), b"\x1b[27;3u");
    }

    #[test]
    fn disambiguate_encodes_ctrl_and_alt_characters() {
        let d = |spec: &'static str| kitty(spec, 1);
        assert_eq!(d("ctrl-c").unwrap(), b"\x1b[99;5u");
        assert_eq!(d("ctrl-a").unwrap(), b"\x1b[97;5u");
        assert_eq!(d("ctrl-[").unwrap(), b"\x1b[91;5u");
        // 编号是不按 Shift 时的字符
        assert_eq!(d("ctrl-shift-a").unwrap(), b"\x1b[97;6u");
        assert_eq!(d("alt-b").unwrap(), b"\x1b[98;3u");
        assert_eq!(d("shift-alt-b").unwrap(), b"\x1b[98;4u");
        assert_eq!(d("ctrl-alt-x").unwrap(), b"\x1b[120;7u");
        assert_eq!(d("ctrl-space").unwrap(), b"\x1b[32;5u");
        // 不带修饰键、只带 Shift 的字符照旧交给文字输入；AltGr 打出的字符也是
        assert_eq!(d("a"), None);
        assert_eq!(d("shift-a"), None);
        assert_eq!(d("space"), None);
        let altgr = typed("ctrl-alt-q", "@");
        assert_eq!(
            kitty_event(&altgr, TerminalModes::default(), 1, KeyEventKind::Press),
            None
        );
    }

    #[test]
    fn disambiguate_uses_csi_forms_for_functional_keys() {
        let d = |spec: &'static str| kitty(spec, 1);
        assert_eq!(d("up").unwrap(), b"\x1b[A");
        // 应用光标键模式中也不用 SS3（转义码只有 CSI 的两种形式）
        let app = TerminalModes {
            app_cursor: true,
            ..TerminalModes::default()
        };
        assert_eq!(
            kitty_event(&key("up"), app, 1, KeyEventKind::Press).unwrap(),
            b"\x1b[A"
        );
        assert_eq!(d("ctrl-left").unwrap(), b"\x1b[1;5D");
        assert_eq!(d("shift-end").unwrap(), b"\x1b[1;2F");
        assert_eq!(d("f1").unwrap(), b"\x1b[P");
        assert_eq!(d("f2").unwrap(), b"\x1b[Q");
        assert_eq!(d("f3").unwrap(), b"\x1b[13~");
        assert_eq!(d("shift-f3").unwrap(), b"\x1b[13;2~");
        assert_eq!(d("ctrl-f4").unwrap(), b"\x1b[1;5S");
        assert_eq!(d("f5").unwrap(), b"\x1b[15~");
        assert_eq!(d("delete").unwrap(), b"\x1b[3~");
        assert_eq!(d("alt-pagedown").unwrap(), b"\x1b[6;3~");
        assert_eq!(d("f13").unwrap(), b"\x1b[57376u");
        assert_eq!(d("f24").unwrap(), b"\x1b[57387u");
        assert_eq!(d("menu").unwrap(), b"\x1b[57363u");
    }

    #[test]
    fn report_events_adds_repeat_and_release() {
        let mode = TerminalModes::default();
        let ev = |spec: &'static str, bits, kind| kitty_event(&key(spec), mode, bits, kind);
        // 0b1 | 0b10（Codex 等用 crossterm 的程序还会加上 0b100）
        assert_eq!(ev("up", 3, KeyEventKind::Press).unwrap(), b"\x1b[A");
        assert_eq!(ev("up", 3, KeyEventKind::Repeat).unwrap(), b"\x1b[1;1:2A");
        assert_eq!(ev("up", 3, KeyEventKind::Release).unwrap(), b"\x1b[1;1:3A");
        assert_eq!(
            ev("ctrl-c", 7, KeyEventKind::Release).unwrap(),
            b"\x1b[99;5:3u"
        );
        assert_eq!(
            ev("escape", 3, KeyEventKind::Release).unwrap(),
            b"\x1b[27;1:3u"
        );
        let press = encode_key(
            &key("ctrl-c"),
            mode,
            KeyboardFlags::from_bits(3),
            KeyEventKind::Press,
            AltKey::Meta,
        )
        .unwrap();
        assert!(press.report_release);
        // 回车、Tab、退格不报告松开；输入文字的键（按文字发送）也不报告
        let shift_enter = encode_key(
            &key("shift-enter"),
            mode,
            KeyboardFlags::from_bits(3),
            KeyEventKind::Press,
            AltKey::Meta,
        )
        .unwrap();
        assert_eq!(&*shift_enter.bytes, b"\x1b[13;2u");
        assert!(!shift_enter.report_release);
        assert_eq!(ev("shift-enter", 3, KeyEventKind::Release), None);
        assert_eq!(ev("a", 3, KeyEventKind::Release), None);
        assert_eq!(ev("a", 3, KeyEventKind::Press), None);
        // 只开 0b10：功能键用 CSI 的形式带上事件类型，其余照旧
        assert_eq!(ev("up", 2, KeyEventKind::Repeat).unwrap(), b"\x1b[1;1:2A");
        assert_eq!(ev("escape", 2, KeyEventKind::Press).unwrap(), b"\x1b");
        assert_eq!(ev("ctrl-c", 2, KeyEventKind::Press).unwrap(), b"\x03");
        assert_eq!(ev("ctrl-c", 2, KeyEventKind::Release), None);
        // 没开 0b10 时重复与按下相同，松开不报告
        assert_eq!(ev("up", 1, KeyEventKind::Repeat).unwrap(), b"\x1b[A");
        assert_eq!(ev("up", 1, KeyEventKind::Release), None);
    }

    #[test]
    fn report_alternates_adds_the_shifted_key() {
        assert_eq!(kitty("ctrl-shift-a", 5).unwrap(), b"\x1b[97:65;6u");
        assert_eq!(kitty("ctrl-a", 5).unwrap(), b"\x1b[97;5u");
        assert_eq!(kitty("shift-alt-b", 5).unwrap(), b"\x1b[98:66;4u");
        // 功能键没有替代键
        assert_eq!(kitty("shift-up", 5).unwrap(), b"\x1b[1;2A");
    }

    #[test]
    fn report_all_keys_and_associated_text() {
        let mode = TerminalModes::default();
        let ev = |k: &Keystroke, bits, kind| kitty_event(k, mode, bits, kind);
        let press = KeyEventKind::Press;
        // 0b1000：输入文字的键与回车、Tab、退格也用转义码
        assert_eq!(ev(&typed("a", "a"), 8, press).unwrap(), b"\x1b[97u");
        assert_eq!(ev(&typed("shift-a", "A"), 8, press).unwrap(), b"\x1b[97;2u");
        assert_eq!(kitty("enter", 8).unwrap(), b"\x1b[13u");
        assert_eq!(kitty("tab", 8).unwrap(), b"\x1b[9u");
        assert_eq!(kitty("backspace", 8).unwrap(), b"\x1b[127u");
        assert_eq!(kitty("space", 8).unwrap(), b"\x1b[32u");
        assert_eq!(kitty("escape", 8).unwrap(), b"\x1b[27u");
        // 加上 0b10：回车也报告松开
        assert_eq!(
            ev(&key("enter"), 10, KeyEventKind::Release).unwrap(),
            b"\x1b[13;1:3u"
        );
        assert_eq!(
            ev(&typed("a", "a"), 10, KeyEventKind::Release).unwrap(),
            b"\x1b[97;1:3u"
        );
        // 0b10000：附带文字的码位；带 Ctrl、松开时没有文字
        assert_eq!(ev(&typed("a", "a"), 24, press).unwrap(), b"\x1b[97;1;97u");
        assert_eq!(
            ev(&typed("shift-a", "A"), 24, press).unwrap(),
            b"\x1b[97;2;65u"
        );
        assert_eq!(ev(&key("ctrl-a"), 24, press).unwrap(), b"\x1b[97;5u");
        assert_eq!(
            ev(&typed("a", "a"), 26, KeyEventKind::Release).unwrap(),
            b"\x1b[97;1:3u"
        );
        assert_eq!(ev(&typed("é", "é"), 24, press).unwrap(), b"\x1b[233;1;233u");
    }

    #[test]
    fn bracketed_paste_wraps_and_normalizes_newlines() {
        let plain = encode_paste("ls\nwhoami\r\n", TerminalModes::default());
        assert_eq!(plain, b"ls\rwhoami\r");
        let bracketed = encode_paste(
            "a\nb",
            TerminalModes {
                bracketed_paste: true,
                ..TerminalModes::default()
            },
        );
        assert_eq!(bracketed, b"\x1b[200~a\rb\x1b[201~");
    }

    // ⌘（macOS）、Win 键的组合是应用与系统的快捷键：没有被快捷键用掉的也不写进终端
    #[test]
    fn platform_combos_are_never_input() {
        for spec in [
            "cmd-c",
            "cmd-k",
            "cmd-up",
            "cmd-enter",
            "cmd-backspace",
            "cmd-}",
            "cmd-f5",
        ] {
            for alt in [AltKey::Meta, AltKey::Compose] {
                assert_eq!(
                    encode(&key(spec), TerminalModes::default(), alt),
                    None,
                    "{spec}"
                );
                for bits in [0, 0b1, 0b11, 0b11111] {
                    let flags = KeyboardFlags::from_bits(bits);
                    for kind in [KeyEventKind::Press, KeyEventKind::Release] {
                        assert_eq!(
                            encode_key(&key(spec), TerminalModes::default(), flags, kind, alt),
                            None,
                            "{spec} {bits:#b}"
                        );
                    }
                }
            }
        }
        // Ctrl 组合照常给 shell（macOS 上也是：应用的快捷键都在 ⌘ 上）
        let ctrl_c = encode(&key("ctrl-c"), TerminalModes::default(), AltKey::Compose);
        assert_eq!(ctrl_c.as_deref(), Some(b"\x03".as_slice()));
    }

    // macOS 上 Option 输入组合字符：带 Option 的字符键交给文字输入；功能键照旧带 Alt 参数，Ctrl+Option 不加 ESC
    #[test]
    fn option_composes_characters_on_mac() {
        let compose = |keystroke: &Keystroke| {
            encode(keystroke, TerminalModes::default(), AltKey::Compose).map(|b| b.into_owned())
        };
        assert_eq!(compose(&typed("alt-a", "å")), None);
        assert_eq!(compose(&typed("alt-e", "")), None, "死键");
        assert_eq!(compose(&key("alt-space")), None);
        assert_eq!(compose(&key("alt-left")).unwrap(), b"\x1b[1;3D");
        assert_eq!(compose(&key("ctrl-alt-x")).unwrap(), b"\x18");
        let flags = KeyboardFlags::from_bits(0b1);
        assert_eq!(
            encode_key(
                &typed("alt-a", "å"),
                TerminalModes::default(),
                flags,
                KeyEventKind::Press,
                AltKey::Compose
            ),
            None
        );
        // Windows、Linux 上 Alt 作为 Meta
        assert_eq!(enc("alt-b").unwrap(), b"\x1bb");
        assert_eq!(AltKey::for_os(OperatingSystem::Mac), AltKey::Compose);
        assert_eq!(AltKey::for_os(OperatingSystem::Windows), AltKey::Meta);
        assert_eq!(AltKey::for_os(OperatingSystem::Linux), AltKey::Meta);
    }

    // macOS 上开启“Option 键作为 Meta 键”：ESC 前缀加上不按 Option 时的字符（不是组合出的 å、∫），Shift 照常；
    // 功能键、Ctrl+Option 与 Windows、Linux 上的 Alt 相同。设置只在 macOS 上起作用
    #[test]
    fn option_as_meta_sends_esc_prefix_on_mac() {
        let meta = |keystroke: &Keystroke| {
            encode(keystroke, TerminalModes::default(), AltKey::OptionMeta).map(|b| b.into_owned())
        };
        assert_eq!(meta(&typed("alt-a", "å")).unwrap(), b"\x1ba");
        assert_eq!(meta(&typed("alt-b", "∫")).unwrap(), b"\x1bb");
        assert_eq!(meta(&typed("alt-shift-a", "Å")).unwrap(), b"\x1bA");
        assert_eq!(
            meta(&typed("alt-e", "")).unwrap(),
            b"\x1be",
            "死键也不再组合"
        );
        assert_eq!(meta(&typed("alt-.", "≥")).unwrap(), b"\x1b.");
        assert_eq!(meta(&key("alt-space")).unwrap(), b"\x1b ");
        assert_eq!(meta(&key("alt-left")).unwrap(), b"\x1b[1;3D");
        assert_eq!(meta(&key("ctrl-alt-x")).unwrap(), b"\x1b\x18");
        // 不按 Option 的字符照常交给文字输入
        assert_eq!(meta(&typed("a", "a")), None);
        // kitty 键盘协议：与 Alt 一样报告这个键本身（97 是 a）
        let flags = KeyboardFlags::from_bits(0b1);
        let kitty = encode_key(
            &typed("alt-a", "å"),
            TerminalModes::default(),
            flags,
            KeyEventKind::Press,
            AltKey::OptionMeta,
        )
        .unwrap();
        assert_eq!(&*kitty.bytes, b"\x1b[97;3u");
        assert_eq!(
            AltKey::for_settings(OperatingSystem::Mac, true),
            AltKey::OptionMeta
        );
        assert_eq!(
            AltKey::for_settings(OperatingSystem::Mac, false),
            AltKey::Compose
        );
        for os in [OperatingSystem::Windows, OperatingSystem::Linux] {
            assert_eq!(AltKey::for_settings(os, true), AltKey::Meta);
            assert_eq!(AltKey::for_settings(os, false), AltKey::Meta);
        }
    }
}
