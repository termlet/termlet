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

/// 关键词高亮可选的颜色。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HighlightColor {
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
}

impl HighlightColor {
    pub const ALL: [HighlightColor; 6] = [
        HighlightColor::Red,
        HighlightColor::Green,
        HighlightColor::Yellow,
        HighlightColor::Blue,
        HighlightColor::Magenta,
        HighlightColor::Cyan,
    ];

    /// 设置中保存的颜色名（red、green……，见 `HighlightRule::color`）；不认识时返回 `None`。
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.ansi().name() == name)
    }

    /// 对应的 ANSI 色。
    pub fn ansi(self) -> AnsiColor {
        match self {
            HighlightColor::Red => AnsiColor::Red,
            HighlightColor::Green => AnsiColor::Green,
            HighlightColor::Yellow => AnsiColor::Yellow,
            HighlightColor::Blue => AnsiColor::Blue,
            HighlightColor::Magenta => AnsiColor::Magenta,
            HighlightColor::Cyan => AnsiColor::Cyan,
        }
    }
}

/// 关键词高亮在一套配色下的实际颜色（小写的 `#rrggbb`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HighlightPalette {
    colors: [String; 6],
}

impl HighlightPalette {
    pub fn get(&self, color: HighlightColor) -> &str {
        &self.colors[color as usize]
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

/// 关键词高亮的颜色：每种颜色在普通的与 bright 的一组之间选对背景更清楚、更鲜艳的（深色配色先看 bright 一组，
/// 浅色先看普通的一组，另一组明显更好时才换：Solarized Dark 等的 bright 一组多是灰色）；
/// 对背景仍不到 3:1 时向前景色混合（浅色配色的黄色常常只有 2:1 左右）。
pub fn highlight_palette(scheme: &ColorScheme) -> HighlightPalette {
    let dark = is_dark_scheme(scheme);
    let bg = scheme.background.as_str();
    let colors = HighlightColor::ALL.map(|color| {
        let i = color.ansi().index();
        let normal = ansi_color(scheme, i);
        let bright = ansi_color(scheme, i + 8);
        let (first, second) = if dark {
            (bright, normal)
        } else {
            (normal, bright)
        };
        let pick = if highlight_score(second, bg) > highlight_score(first, bg) + 0.05 {
            second
        } else {
            first
        };
        ensure_highlight_contrast(pick, bg, &scheme.foreground, MIN_HIGHLIGHT_CONTRAST)
    });
    HighlightPalette { colors }
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
            out.push(SampleSpan::fg(hit, palette.get(color)));
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
