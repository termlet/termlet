//! 颜色的基本运算：解析与格式化、相对亮度与对比度（WCAG）、混合、深浅判断，
//! 以及"保证文字可读"时调整前景色的算法。

use std::fmt;

/// 一个 sRGB 颜色（每个分量 0–255）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const BLACK: Rgb = Rgb::new(0, 0, 0);
    pub const WHITE: Rgb = Rgb::new(255, 255, 255);

    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// `#rrggbb`（大小写均可）→ 颜色；格式不对时返回 `None`。
    pub fn parse(hex: &str) -> Option<Rgb> {
        let digits = hex.strip_prefix('#')?.as_bytes();
        if digits.len() != 6 {
            return None;
        }
        let channel = |i: usize| Some(hex_value(digits[i])? * 16 + hex_value(digits[i + 1])?);
        Some(Rgb::new(channel(0)?, channel(2)?, channel(4)?))
    }

    /// 小写的 `#rrggbb`。
    pub fn to_hex(self) -> String {
        self.to_string()
    }

    /// 相对亮度（WCAG），0–1。
    pub fn luminance(self) -> f64 {
        let lin = |c: u8| {
            let v = f64::from(c) / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(self.r) + 0.7152 * lin(self.g) + 0.0722 * lin(self.b)
    }

    /// 与另一种颜色的对比度（WCAG），1–21。
    pub fn contrast(self, other: Rgb) -> f64 {
        luminance_ratio(self.luminance(), other.luminance())
    }

    /// 按比例 `t`（0–1）从这种颜色向 `other` 混合。
    pub fn mix(self, other: Rgb, t: f64) -> Rgb {
        let channel = |a: u8, b: u8| {
            let (a, b) = (f64::from(a), f64::from(b));
            round_channel(a + (b - a) * t)
        };
        Rgb::new(
            channel(self.r, other.r),
            channel(self.g, other.g),
            channel(self.b, other.b),
        )
    }

    /// 颜色的鲜艳程度（0–1）：RGB 最大与最小分量之差。灰色为 0。
    pub fn chroma(self) -> f64 {
        let max = self.r.max(self.g).max(self.b);
        let min = self.r.min(self.g).min(self.b);
        f64::from(max - min) / 255.0
    }
}

impl fmt::Display for Rgb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

fn hex_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// 浮点分量四舍五入并限制在 0–255。
pub(crate) fn round_channel(v: f64) -> u8 {
    // 先限制范围再取整：结果一定在 0–255 之内，NaN 按 0 处理
    let v = v.clamp(0.0, 255.0).round();
    if v.is_nan() { 0 } else { v as u8 }
}

fn luminance_ratio(x: f64, y: f64) -> f64 {
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

/// 解析 `#rrggbb`；格式不对时按黑色处理（数据不完整的配色也能计算，不出错）。
fn rgb_or_black(hex: &str) -> Rgb {
    Rgb::parse(hex).unwrap_or(Rgb::BLACK)
}

/// 用户输入的颜色（`#rgb`、`#rrggbb`，可以不带 `#`）规范为小写的 `#rrggbb`；不是颜色时返回 `None`。
pub fn normalize_hex(input: &str) -> Option<String> {
    let s = input.trim();
    let s = s.strip_prefix('#').unwrap_or(s);
    if !s.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    match s.len() {
        6 => Some(format!("#{}", s.to_ascii_lowercase())),
        3 => Some(
            std::iter::once('#')
                .chain(s.chars().flat_map(|c| [c, c]))
                .collect::<String>()
                .to_ascii_lowercase(),
        ),
        _ => None,
    }
}

/// 两种颜色（`#rrggbb`）的对比度（WCAG，1–21）。
pub fn contrast_ratio(a: &str, b: &str) -> f64 {
    rgb_or_black(a).contrast(rgb_or_black(b))
}

/// 按比例 `t`（0–1）从 `a` 向 `b` 混合，结果为小写的 `#rrggbb`。
pub fn mix(a: &str, b: &str, t: f64) -> String {
    rgb_or_black(a).mix(rgb_or_black(b), t).to_hex()
}

/// 深色背景：白字在背景上比黑字更清楚。配色的深浅不单独记录，按背景判断。
pub fn is_dark(background: &str) -> bool {
    let bg = rgb_or_black(background);
    bg.contrast(Rgb::WHITE) > bg.contrast(Rgb::BLACK)
}

/// "保证文字可读"开启时的最低对比度（WCAG AA 正文的要求）。
pub const MIN_CONTRAST_ON: f64 = 4.5;

/// "保证文字可读"：前景 `fg` 在背景 `bg` 上的对比度低于 `ratio` 时，调亮或调暗前景直到够了，返回调整后的颜色；
/// 本来就够时返回 `None`。
///
/// 与 xterm.js 的 `minimumContrastRatio`（MIT）相同：前景比背景暗时先每次调暗 10%，到纯黑仍不够时
/// 改为调亮，取两个方向中对比度更高的；前景比背景亮时反过来。终端中暗淡（SGR 2）的文字用一半的 `ratio`，
/// 制表符、方块等当作背景色的字符不调整（由终端渲染处理）。
pub fn ensure_contrast(fg: Rgb, bg: Rgb, ratio: f64) -> Option<Rgb> {
    let bg_l = bg.luminance();
    let fg_l = fg.luminance();
    if luminance_ratio(bg_l, fg_l) >= ratio {
        return None;
    }
    let (first, second): (fn(Rgb, Rgb, f64) -> Rgb, fn(Rgb, Rgb, f64) -> Rgb) = if fg_l < bg_l {
        (reduce_luminance, increase_luminance)
    } else {
        (increase_luminance, reduce_luminance)
    };
    let a = first(fg, bg, ratio);
    let a_ratio = luminance_ratio(bg_l, a.luminance());
    if a_ratio >= ratio {
        return Some(a);
    }
    let b = second(fg, bg, ratio);
    let b_ratio = luminance_ratio(bg_l, b.luminance());
    Some(if a_ratio > b_ratio { a } else { b })
}

/// 每次把各分量减少 10%（向上取整），直到对比度够了或成为纯黑。
/// 按浮点计算 `ceil(c * 0.1)`，与 xterm.js 逐步得到的颜色相同（例如 30 × 0.1 略大于 3，减去 4）。
fn reduce_luminance(fg: Rgb, bg: Rgb, ratio: f64) -> Rgb {
    let step = |c: u8| round_channel(f64::from(c) - (f64::from(c) * 0.1).ceil());
    let mut c = fg;
    while c.contrast(bg) < ratio && c != Rgb::BLACK {
        c = Rgb::new(step(c.r), step(c.g), step(c.b));
    }
    c
}

/// 每次把各分量与 255 的差距缩小 10%（向上取整），直到对比度够了或成为纯白。
fn increase_luminance(fg: Rgb, bg: Rgb, ratio: f64) -> Rgb {
    let step = |c: u8| round_channel(f64::from(c) + (f64::from(255 - c) * 0.1).ceil());
    let mut c = fg;
    while c.contrast(bg) < ratio && c != Rgb::WHITE {
        c = Rgb::new(step(c.r), step(c.g), step(c.b));
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn hex_contrast_mix_and_darkness() {
        assert_eq!(normalize_hex("#ABC").as_deref(), Some("#aabbcc"));
        assert_eq!(normalize_hex(" 1A2b3C ").as_deref(), Some("#1a2b3c"));
        assert_eq!(normalize_hex("#12345"), None);
        assert_eq!(normalize_hex("red"), None);
        assert!(close(contrast_ratio("#000000", "#ffffff"), 21.0));
        assert!(close(contrast_ratio("#777777", "#777777"), 1.0));
        assert_eq!(mix("#000000", "#ffffff", 0.5), "#808080");
        assert!(is_dark("#232323"));
        assert!(!is_dark("#fdf6e3"));
        assert!(!is_dark("#ffffff"));
        assert!(is_dark("#002b36"));
    }

    #[test]
    fn parse_and_format() {
        assert_eq!(Rgb::parse("#1A2b3c"), Some(Rgb::new(0x1a, 0x2b, 0x3c)));
        assert_eq!(Rgb::parse("1a2b3c"), None);
        assert_eq!(Rgb::parse("#abc"), None);
        assert_eq!(Rgb::parse("#ééé"), None);
        assert_eq!(Rgb::new(1, 2, 255).to_hex(), "#0102ff");
        // 不是颜色时按黑色计算，不出错
        assert!(close(contrast_ratio("oops", "#ffffff"), 21.0));
        assert_eq!(round_channel(f64::NAN), 0);
        assert_eq!(round_channel(300.0), 255);
        assert_eq!(round_channel(-3.0), 0);
    }

    #[test]
    fn ensure_contrast_like_xterm() {
        let white = Rgb::WHITE;
        // 已经够了：不调整
        assert_eq!(ensure_contrast(Rgb::BLACK, white, MIN_CONTRAST_ON), None);
        // 白底上的浅黄：调暗到够
        let pale = Rgb::new(0xe0, 0xc0, 0x40);
        let fixed = ensure_contrast(pale, white, MIN_CONTRAST_ON).unwrap();
        assert!(fixed.contrast(white) >= MIN_CONTRAST_ON);
        assert!(fixed.luminance() < pale.luminance());
        // 黑底上的深蓝：调亮到够
        let navy = Rgb::new(0, 0, 0x80);
        let fixed = ensure_contrast(navy, Rgb::BLACK, MIN_CONTRAST_ON).unwrap();
        assert!(fixed.contrast(Rgb::BLACK) >= MIN_CONTRAST_ON);
        assert!(fixed.luminance() > navy.luminance());
        // 中灰背景上要求 21:1 达不到：取两个方向中更清楚的
        let gray = Rgb::new(0x80, 0x80, 0x80);
        let best = ensure_contrast(gray, gray, 21.0).unwrap();
        assert!(best == Rgb::BLACK || best == Rgb::WHITE);
        assert!(best.contrast(gray) >= Rgb::BLACK.contrast(gray).min(Rgb::WHITE.contrast(gray)));
        // 前景比背景亮但调亮不够：改为调暗
        let light_bg = Rgb::new(0xf0, 0xf0, 0xf0);
        let fixed = ensure_contrast(Rgb::new(0xf8, 0xf8, 0xf8), light_bg, 4.5).unwrap();
        assert!(fixed.contrast(light_bg) >= 4.5);
    }
}
