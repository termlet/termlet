//! 导入时颜色的各种写法，以及补全缺失颜色用到的计算。

use std::sync::LazyLock;

use regex::Regex;

use crate::schemes::color::{Rgb, round_channel};

/// 常见的 X11 颜色名（Xresources、Ghostty 中可能出现）。
const NAMED_COLORS: &[(&str, &str)] = &[
    ("black", "#000000"),
    ("white", "#ffffff"),
    ("red", "#ff0000"),
    ("green", "#00ff00"),
    ("blue", "#0000ff"),
    ("yellow", "#ffff00"),
    ("magenta", "#ff00ff"),
    ("cyan", "#00ffff"),
    ("gray", "#bebebe"),
    ("grey", "#bebebe"),
    ("red3", "#cd0000"),
    ("green3", "#00cd00"),
    ("yellow3", "#cdcd00"),
    ("blue2", "#0000ee"),
    ("blue3", "#0000cd"),
    ("magenta3", "#cd00cd"),
    ("cyan3", "#00cdcd"),
    ("gray90", "#e5e5e5"),
    ("grey90", "#e5e5e5"),
    ("gray50", "#7f7f7f"),
    ("grey50", "#7f7f7f"),
];

/// `rgb(r, g, b)`、`rgba(r, g, b, a)` 与十进制的 `r,g,b`（MobaXterm）。
static DECIMAL_RGB: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:rgba?\(\s*([0-9]{1,3})\s*,\s*([0-9]{1,3})\s*,\s*([0-9]{1,3})\s*(?:,\s*[0-9.]+%?\s*)?\)|([0-9]{1,3})\s*,\s*([0-9]{1,3})\s*,\s*([0-9]{1,3}))$",
    )
    .expect("十进制颜色的正则")
});

fn hex_digits(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|c| c.is_ascii_hexdigit())
}

/// 把各种写法的颜色统一为小写 `#rrggbb`；认不出时返回 `None`。
///
/// 支持 `#rgb`、`#rrggbb`、`#rrggbbaa`、`0xrrggbb`、不带 `#` 的 `rrggbb`、X11 的 `rgb:r/g/b` 与 `#rrrrggggbbbb`、
/// `rgb()` / `rgba()`、十进制 `r,g,b`（MobaXterm），以及少量 X11 颜色名。
pub fn parse_color(input: &str) -> Option<String> {
    let s = input.trim().to_ascii_lowercase();
    if s.is_empty() || s.len() > 64 {
        return None;
    }
    let bare = s
        .strip_prefix('#')
        .or_else(|| s.strip_prefix("0x"))
        .unwrap_or(&s);
    if hex_digits(bare, 6) || hex_digits(bare, 8) {
        return Some(format!("#{}", &bare[..6]));
    }
    if let Some(h) = s.strip_prefix('#') {
        if hex_digits(h, 3) {
            return Some(h.chars().fold(String::from("#"), |mut out, c| {
                out.push(c);
                out.push(c);
                out
            }));
        }
        if hex_digits(h, 12) {
            return Some(format!("#{}{}{}", &h[0..2], &h[4..6], &h[8..10]));
        }
    }
    if let Some(parts) = s.strip_prefix("rgb:") {
        let parts: Vec<&str> = parts.split('/').collect();
        let valid =
            |p: &&str| (1..=4).contains(&p.len()) && p.bytes().all(|c| c.is_ascii_hexdigit());
        if parts.len() == 3 && parts.iter().all(valid) {
            let scaled = parts
                .iter()
                .map(|p| {
                    let v = u32::from_str_radix(p, 16).ok()?;
                    let max = 16u32.pow(p.len() as u32) - 1;
                    Some(round_channel(f64::from(v) / f64::from(max) * 255.0))
                })
                .collect::<Option<Vec<u8>>>()?;
            return Some(Rgb::new(scaled[0], scaled[1], scaled[2]).to_hex());
        }
    }
    if let Some(caps) = DECIMAL_RGB.captures(&s) {
        let groups = if caps.get(1).is_some() { 1..4 } else { 4..7 };
        let mut channels = [0u8; 3];
        for (slot, g) in channels.iter_mut().zip(groups) {
            let v: u16 = caps.get(g)?.as_str().parse().ok()?;
            *slot = u8::try_from(v).ok()?;
        }
        return Some(Rgb::new(channels[0], channels[1], channels[2]).to_hex());
    }
    NAMED_COLORS
        .iter()
        .find(|(name, _)| *name == s)
        .map(|(_, hex)| (*hex).to_string())
}

/// 按权重 `t` 混合两个颜色（`t` 为 `a` 的占比）。
pub(super) fn blend(a: &str, b: &str, t: f64) -> String {
    let a = Rgb::parse(a).unwrap_or(Rgb::BLACK);
    let b = Rgb::parse(b).unwrap_or(Rgb::BLACK);
    let channel = |x: u8, y: u8| round_channel(f64::from(x) * t + f64::from(y) * (1.0 - t));
    Rgb::new(channel(a.r, b.r), channel(a.g, b.g), channel(a.b, b.b)).to_hex()
}

/// 从候选中选第一个与 `against` 对比度够（4.5:1）的颜色；都不够时用黑或白。
pub(super) fn pick_readable(candidates: &[&str], against: &str) -> String {
    let bg = Rgb::parse(against).unwrap_or(Rgb::BLACK);
    candidates
        .iter()
        .find(|c| Rgb::parse(c).unwrap_or(Rgb::BLACK).contrast(bg) >= 4.5)
        .map(|c| (*c).to_string())
        .unwrap_or_else(|| {
            if bg.luminance() > 0.18 {
                "#000000".into()
            } else {
                "#ffffff".into()
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_spellings() {
        for (input, out) in [
            ("#ABCDEF", "#abcdef"),
            ("#abc", "#aabbcc"),
            ("#11223344", "#112233"),
            ("0x1A2B3C", "#1a2b3c"),
            ("1a2b3c", "#1a2b3c"),
            ("rgb:ff/80/00", "#ff8000"),
            ("rgb:f/8/0", "#ff8800"),
            ("rgb:ffff/0000/8080", "#ff0080"),
            ("#ffff00008080", "#ff0080"),
            ("rgb(255, 128, 0)", "#ff8000"),
            ("rgba(1,2,3,0.5)", "#010203"),
            ("69,71,90", "#45475a"),
            ("red3", "#cd0000"),
            ("  #FFFFFF  ", "#ffffff"),
        ] {
            assert_eq!(parse_color(input).as_deref(), Some(out), "{input}");
        }
    }

    #[test]
    fn unknown_spellings() {
        let long = "x".repeat(100);
        for input in [
            "",
            "blue-ish",
            "#12",
            "#12345",
            "256,0,0",
            "rgb(300,0,0)",
            long.as_str(),
            "constructor",
            "__proto__",
            "rgb:/1/2",
            "rgb:12345/0/0",
            "١٢,٣,٤",
        ] {
            assert_eq!(parse_color(input), None, "{input}");
        }
    }

    #[test]
    fn blend_and_readable() {
        // 前景占 30%
        assert_eq!(blend("#dddddd", "#202020", 0.3), "#595959");
        assert_eq!(blend("#cccccc", "#0c0c0c", 0.3), "#464646");
        assert_eq!(pick_readable(&["#111111", "#eeeeee"], "#000000"), "#eeeeee");
        assert_eq!(pick_readable(&["#777777"], "#ffffff"), "#000000");
        assert_eq!(pick_readable(&[], "#202020"), "#ffffff");
    }
}
