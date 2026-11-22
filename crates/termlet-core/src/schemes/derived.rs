//! 从配色派生的颜色与检查：看不清的颜色、搜索高亮、关键词高亮，以及配色卡片与编辑器中的预览示例。

use std::ops::Range;

use super::color::Rgb;
use super::{ansi_color, contrast_ratio, is_dark_scheme, mix};
use crate::model::ColorScheme;

/// 16 色的前 8 种（之后是同顺序的 bright 一组）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AnsiColor {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
}

impl AnsiColor {
    pub const ALL: [AnsiColor; 8] = [
        AnsiColor::Black,
        AnsiColor::Red,
        AnsiColor::Green,
        AnsiColor::Yellow,
        AnsiColor::Blue,
        AnsiColor::Magenta,
        AnsiColor::Cyan,
        AnsiColor::White,
    ];

    /// 在 16 色中的位置（普通的一组，0–7；bright 的一组再加 8）。
    pub fn index(self) -> usize {
        self as usize
    }

    /// 英文名（black、red……），与配置文件中的写法一致。
    pub fn name(self) -> &'static str {
        match self {
            AnsiColor::Black => "black",
            AnsiColor::Red => "red",
            AnsiColor::Green => "green",
            AnsiColor::Yellow => "yellow",
            AnsiColor::Blue => "blue",
            AnsiColor::Magenta => "magenta",
            AnsiColor::Cyan => "cyan",
            AnsiColor::White => "white",
        }
    }
}

/// 可读性检查的颜色：前景与 ANSI 色。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ColorKey {
    Foreground,
    Ansi(AnsiColor),
}

/// 对比度低于这个值（WCAG 的对比度，1–21）就标为看不清。
pub const MIN_READABLE: f64 = 2.0;

/// 在背景上看不清的颜色：前景与 6 种彩色（red 到 cyan）对背景的对比度低于 [`MIN_READABLE`]。
/// black、white 不算：深色配色的 black、浅色配色的 white 本来就接近背景色（程序多用作背景），
/// Solarized Light 等浅色配色的 black 也是浅色。
pub fn unreadable_colors(scheme: &ColorScheme) -> Vec<ColorKey> {
    let mut out = Vec::new();
    if contrast_ratio(&scheme.foreground, &scheme.background) < MIN_READABLE {
        out.push(ColorKey::Foreground);
    }
    for color in AnsiColor::ALL {
        if matches!(color, AnsiColor::Black | AnsiColor::White) {
            continue;
        }
        if contrast_ratio(ansi_color(scheme, color.index()), &scheme.background) < MIN_READABLE {
            out.push(ColorKey::Ansi(color));
        }
    }
    out
}

/// 搜索结果的高亮颜色（小写的 `#rrggbb`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchColors {
    pub match_background: String,
    pub active_match_background: String,
    /// 滚动条上标出其余结果的颜色
    pub match_overview_ruler: String,
    /// 滚动条上标出当前结果的颜色
    pub active_match_overview_ruler: String,
}

/// 背景 `color` 上的前景 `fg` 至少有 3:1 的对比度：不够时逐步向 `toward` 混合（最多 8 次）。
fn readable_on(color: &str, fg: &str, toward: &str) -> String {
    let mut c = color.to_string();
    for _ in 0..8 {
        if contrast_ratio(&c, fg) >= 3.0 {
            break;
        }
        c = mix(&c, toward, 0.25);
    }
    c
}

/// 搜索高亮从配色派生：当前结果用配色的黄色（与背景混合，保证前景文字可读），其余结果深色配色用淡一点的前景色、
/// 浅色配色用淡一点的黄色。
pub fn search_colors(scheme: &ColorScheme) -> SearchColors {
    let (fg, bg) = (scheme.foreground.as_str(), scheme.background.as_str());
    let yellow = ansi_color(scheme, 3);
    let active = readable_on(&mix(bg, yellow, 0.55), fg, bg);
    if is_dark_scheme(scheme) {
        return SearchColors {
            match_background: mix(bg, fg, 0.25),
            active_match_background: active,
            match_overview_ruler: mix(bg, fg, 0.6),
            active_match_overview_ruler: ansi_color(scheme, 11).to_string(),
        };
    }
    SearchColors {
        match_background: readable_on(&mix(bg, yellow, 0.3), fg, bg),
        active_match_background: active,
        match_overview_ruler: mix(bg, fg, 0.4),
        active_match_overview_ruler: yellow.to_string(),
    }
}

/// 关键词高亮的颜色。设置中（`HighlightRule::color`）的写法：
///
/// - `red`、`green`、`yellow`、`blue`、`magenta`、`cyan`：配色中的颜色，在普通色与亮色中自动取对背景更清楚的一种
///   （见 [`highlight_palette`]；深色配色上多是亮色）；
/// - `darkRed`、`darkGreen`……：固定取配色的普通色（1–6 号），0.3.3 起；
/// - `brightRed`、`brightGreen`……：固定取配色的亮色（9–14 号），0.3.3 起；
/// - `#rrggbb`：自定义的颜色，0.3.3 起。
///
/// 实际显示时都保证对背景的对比度（见 [`HighlightPalette::get`]），同一个值在浅色、深色配色上都看得清。
/// 0.3.2 及之前的版本不认识后三种写法：终端中按红色显示，规则照常保留、同步；在它们的设置中保存时这几条的颜色改成
/// 红色。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HighlightColor {
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    DarkRed,
    DarkGreen,
    DarkYellow,
    DarkBlue,
    DarkMagenta,
    DarkCyan,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    /// 自定义的颜色
    Custom(Rgb),
}

/// 配色中的颜色取哪一组。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HighlightTone {
    /// 普通色与亮色中对背景更清楚的一种
    Auto,
    /// 普通色（1–6 号）
    Dark,
    /// 亮色（9–14 号）
    Bright,
}

impl HighlightColor {
    /// 配色中的十八种颜色：自动、深色、亮色三组，每组按红、绿、黄、蓝、紫、青，也是设置中列出的顺序。
    pub const NAMED: [HighlightColor; 18] = [
        HighlightColor::Red,
        HighlightColor::Green,
        HighlightColor::Yellow,
        HighlightColor::Blue,
        HighlightColor::Magenta,
        HighlightColor::Cyan,
        HighlightColor::DarkRed,
        HighlightColor::DarkGreen,
        HighlightColor::DarkYellow,
        HighlightColor::DarkBlue,
        HighlightColor::DarkMagenta,
        HighlightColor::DarkCyan,
        HighlightColor::BrightRed,
        HighlightColor::BrightGreen,
        HighlightColor::BrightYellow,
        HighlightColor::BrightBlue,
        HighlightColor::BrightMagenta,
        HighlightColor::BrightCyan,
    ];

    /// 设置中保存的写法（见类型的说明）→ 颜色；不认识时返回 `None`。
    pub fn from_name(name: &str) -> Option<Self> {
        if name.starts_with('#') {
            return Rgb::parse(name).map(HighlightColor::Custom);
        }
        Self::NAMED.into_iter().find(|c| c.name() == name)
    }

    /// 设置中保存的写法：颜色名，自定义的颜色为小写的 `#rrggbb`。
    pub fn name(self) -> std::borrow::Cow<'static, str> {
        let name = match self {
            HighlightColor::Red => "red",
            HighlightColor::Green => "green",
            HighlightColor::Yellow => "yellow",
            HighlightColor::Blue => "blue",
            HighlightColor::Magenta => "magenta",
            HighlightColor::Cyan => "cyan",
            HighlightColor::DarkRed => "darkRed",
            HighlightColor::DarkGreen => "darkGreen",
            HighlightColor::DarkYellow => "darkYellow",
            HighlightColor::DarkBlue => "darkBlue",
            HighlightColor::DarkMagenta => "darkMagenta",
            HighlightColor::DarkCyan => "darkCyan",
            HighlightColor::BrightRed => "brightRed",
            HighlightColor::BrightGreen => "brightGreen",
            HighlightColor::BrightYellow => "brightYellow",
            HighlightColor::BrightBlue => "brightBlue",
            HighlightColor::BrightMagenta => "brightMagenta",
            HighlightColor::BrightCyan => "brightCyan",
            HighlightColor::Custom(rgb) => return rgb.to_hex().into(),
        };
        name.into()
    }

    /// 配色中的颜色对应的 ANSI 色与取哪一组；自定义的颜色为 `None`。
    pub fn ansi(self) -> Option<(AnsiColor, HighlightTone)> {
        let slot = Self::NAMED.iter().position(|c| *c == self)?;
        let tone = [
            HighlightTone::Auto,
            HighlightTone::Dark,
            HighlightTone::Bright,
        ][slot / 6];
        Some((AnsiColor::ALL[slot % 6 + 1], tone))
    }
}

/// 关键词高亮在一套配色下的实际颜色（小写的 `#rrggbb`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HighlightPalette {
    /// 按 [`HighlightColor::NAMED`] 的顺序
    named: [String; 18],
    background: String,
    foreground: String,
}

impl HighlightPalette {
    /// 颜色在这套配色上实际显示的样子：配色中的颜色取算好的；自定义的颜色对背景不到 3:1 时与配色中的颜色一样向前景色
    /// 混合（浅色背景上的浅黄变深、深色背景上的深蓝变浅），够了就原样。
    pub fn get(&self, color: HighlightColor) -> String {
        match color {
            HighlightColor::Custom(rgb) => ensure_highlight_contrast(
                &rgb.to_hex(),
                &self.background,
                &self.foreground,
                MIN_HIGHLIGHT_CONTRAST,
            ),
            named => {
                let slot = HighlightColor::NAMED
                    .iter()
                    .position(|c| *c == named)
                    .unwrap_or(0);
                self.named[slot].clone()
            }
        }
    }
}

/// 关键词高亮的颜色对背景的最低对比度（WCAG 对大号文字、图形的要求）。
pub const MIN_HIGHLIGHT_CONTRAST: f64 = 3.0;

/// 作为高亮色的得分：对背景的对比度（到 4.5 为止）与鲜艳程度（到 0.5 为止，够鲜艳的不再细分）。
/// 两组都清楚、都鲜艳时得分相同，用先看的一组。
fn highlight_score(color: &str, background: &str) -> f64 {
    let chroma = Rgb::parse(color).unwrap_or(Rgb::BLACK).chroma();
    contrast_ratio(color, background).min(4.5) / 4.5 + chroma.min(0.5)
}

/// 对背景 `bg` 的对比度不足 `min` 时，逐步向 `toward`（前景色）混合，直到够了。
fn ensure_highlight_contrast(color: &str, bg: &str, toward: &str, min: f64) -> String {
    if contrast_ratio(color, bg) >= min {
        return color.to_string();
    }
    // 每次多混合 0.1，逐次累加（浮点误差使最后一步约为 0.9999999999999999，也算一步）
    let mut t = 0.1;
    while t < 1.0 {
        let c = mix(color, toward, t);
        if contrast_ratio(&c, bg) >= min {
            return c;
        }
        t += 0.1;
    }
    toward.to_string()
}

/// 关键词高亮的颜色：自动的六种各在普通的与 bright 的一组之间选对背景更清楚、更鲜艳的（深色配色先看 bright 一组，
/// 浅色先看普通的一组，另一组明显更好时才换：Solarized Dark 等的 bright 一组多是灰色）；深色、亮色两组固定取普通的、
/// bright 的一组。对背景仍不到 3:1 时向前景色混合（浅色配色的黄色常常只有 2:1 左右）。
pub fn highlight_palette(scheme: &ColorScheme) -> HighlightPalette {
    let dark = is_dark_scheme(scheme);
    let bg = scheme.background.as_str();
    let fg = scheme.foreground.as_str();
    let named = HighlightColor::NAMED.map(|color| {
        let Some((ansi, tone)) = color.ansi() else {
            return fg.to_string();
        };
        let i = ansi.index();
        let normal = ansi_color(scheme, i);
        let bright = ansi_color(scheme, i + 8);
        let pick = match tone {
            HighlightTone::Dark => normal,
            HighlightTone::Bright => bright,
            HighlightTone::Auto => {
                let (first, second) = if dark {
                    (bright, normal)
                } else {
                    (normal, bright)
                };
                if highlight_score(second, bg) > highlight_score(first, bg) + 0.05 {
                    second
                } else {
                    first
                }
            }
        };
        ensure_highlight_contrast(pick, bg, fg, MIN_HIGHLIGHT_CONTRAST)
    });
    HighlightPalette {
        named,
        background: bg.to_string(),
        foreground: fg.to_string(),
    }
}

/// 预览中的一段文字：前景、背景（`None` 时用配色的前景、背景）与粗体。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SampleSpan {
    pub text: &'static str,
    pub fg: Option<String>,
    pub bg: Option<String>,
    pub bold: bool,
}

impl SampleSpan {
    fn plain(text: &'static str) -> Self {
        Self {
            text,
            fg: None,
            bg: None,
            bold: false,
        }
    }

    fn fg(text: &'static str, fg: &str) -> Self {
        Self {
            fg: Some(fg.to_string()),
            ..Self::plain(text)
        }
    }

    fn bold(text: &'static str, fg: &str) -> Self {
        Self {
            bold: true,
            ..Self::fg(text, fg)
        }
    }
}

/// 配色卡片、编辑器预览中的固定示例：提示符与光标、`ls` 的目录 / 链接 / 可执行文件与选中的文字、`git diff` 的增删行、
/// 按用户的关键词高亮规则着色的日志。与终端一致，粗体的普通色显示为 bright 一组（ls、bash 提示符的默认配色是粗体）。
///
/// `matches` 给出一行文字中命中高亮规则的位置（字节范围，按顺序、互不重叠）与颜色；关闭高亮时返回空。
/// 位置不合法（越界、不在字符边界上、与前一段重叠）的命中忽略。
pub fn scheme_sample(
    scheme: &ColorScheme,
    matches: impl Fn(&str) -> Vec<(Range<usize>, HighlightColor)>,
) -> Vec<Vec<SampleSpan>> {
    let c = |i: usize| ansi_color(scheme, i);
    let palette = highlight_palette(scheme);
    let highlighted = |text: &'static str| -> Vec<SampleSpan> {
        let mut out = Vec::new();
        let mut at = 0;
        for (range, color) in matches(text) {
            if range.start < at || range.start >= range.end {
                continue;
            }
            let (Some(before), Some(hit)) = (text.get(at..range.start), text.get(range.clone()))
            else {
                continue;
            };
            if !before.is_empty() {
                out.push(SampleSpan::plain(before));
            }
            out.push(SampleSpan::fg(hit, &palette.get(color)));
            at = range.end;
        }
        if let Some(rest) = text.get(at..).filter(|r| !r.is_empty()) {
            out.push(SampleSpan::plain(rest));
        }
        out
    };
    let cursor_fg = if scheme.cursor_accent.is_empty() {
        &scheme.background
    } else {
        &scheme.cursor_accent
    };
    let selection_fg = Some(scheme.selection_foreground.clone()).filter(|c| !c.is_empty());
    vec![
        vec![
            SampleSpan::bold("ruan@web-01", c(10)),
            SampleSpan::plain(":"),
            SampleSpan::bold("~/app", c(12)),
            SampleSpan::plain("$ ls -F "),
            SampleSpan {
                bg: Some(scheme.cursor.clone()),
                ..SampleSpan::fg(" ", cursor_fg)
            },
        ],
        vec![
            SampleSpan::bold("src/", c(12)),
            SampleSpan::plain("  "),
            SampleSpan::bold("current@", c(14)),
            SampleSpan::plain("  "),
            SampleSpan::bold("deploy.sh*", c(10)),
            SampleSpan::plain("  "),
            SampleSpan {
                fg: selection_fg,
                bg: Some(scheme.selection_background.clone()),
                ..SampleSpan::plain("README.md")
            },
        ],
        vec![SampleSpan::fg("$ git diff", c(8))],
        vec![SampleSpan::fg("-  timeout: 30", c(1))],
        vec![SampleSpan::fg("+  timeout: 60", c(2))],
        highlighted("ERROR db 10.0.0.12 refused"),
        highlighted("WARN retry 1/3 · OK"),
    ]
}
