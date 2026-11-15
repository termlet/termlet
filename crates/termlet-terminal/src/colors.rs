//! 终端配色：16 色、前景、背景、光标、选区，以及把单元格的颜色解析成实际的 RGB。

use gpui_kit::{Hsla, Rgba};

/// RGB 颜色。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// 单元格的颜色。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Color {
    /// 默认前景色（SGR 39）
    Foreground,
    /// 默认背景色（SGR 49）
    Background,
    /// 0–255 号颜色：0–15 是配色中的 16 色（SGR 30–37、90–97 等），之后是 6×6×6 色立方与灰阶（SGR 38;5）
    Indexed(u8),
    /// 程序直接给出的颜色（SGR 38;2）
    Rgb(Rgb),
}

/// 程序可以查询、修改的颜色（OSC 4 / 10 / 11 / 12）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorSlot {
    Indexed(u8),
    Foreground,
    Background,
    Cursor,
}

/// 程序用 OSC 4 / 10 / 11 / 12 改过的颜色；没改过的为 `None`，用配色中的。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColorOverrides {
    pub indexed: [Option<Rgb>; 256],
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    pub cursor: Option<Rgb>,
}

impl Default for ColorOverrides {
    fn default() -> Self {
        Self {
            indexed: [None; 256],
            foreground: None,
            background: None,
            cursor: None,
        }
    }
}

impl ColorOverrides {
    pub fn get(&self, slot: ColorSlot) -> Option<Rgb> {
        match slot {
            ColorSlot::Indexed(index) => self.indexed[index as usize],
            ColorSlot::Foreground => self.foreground,
            ColorSlot::Background => self.background,
            ColorSlot::Cursor => self.cursor,
        }
    }

    pub fn set(&mut self, slot: ColorSlot, color: Option<Rgb>) {
        match slot {
            ColorSlot::Indexed(index) => self.indexed[index as usize] = color,
            ColorSlot::Foreground => self.foreground = color,
            ColorSlot::Background => self.background = color,
            ColorSlot::Cursor => self.cursor = color,
        }
    }
}

/// 一套终端配色。
#[derive(Clone, Debug, PartialEq)]
pub struct Palette {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    /// 块状光标下的文字颜色；`None` 时用背景色
    pub cursor_text: Option<Rgb>,
    pub selection_background: Rgb,
    /// `None` 时选中的文字保持原来的颜色
    pub selection_foreground: Option<Rgb>,
    /// black、red、green、yellow、blue、magenta、cyan、white，之后是同顺序的 bright 一组
    pub ansi: [Rgb; 16],
}

const fn rgb(hex: u32) -> Rgb {
    Rgb {
        r: (hex >> 16) as u8,
        g: (hex >> 8) as u8,
        b: hex as u8,
    }
}

impl Palette {
    /// 深色界面的默认配色（与内置的 Termlet Dark 接近，正式数据由配色模块提供）。
    pub fn default_dark() -> Self {
        Self {
            foreground: rgb(0xd4d4d8),
            background: rgb(0x1e1e1f),
            cursor: rgb(0xe0e0e4),
            cursor_text: None,
            selection_background: rgb(0x3b4a63),
            selection_foreground: None,
            ansi: [
                rgb(0x2e2e30),
                rgb(0xf05d5d),
                rgb(0x6cc375),
                rgb(0xe5c07b),
                rgb(0x61a8f2),
                rgb(0xc678dd),
                rgb(0x56b6c2),
                rgb(0xc8c8cc),
                rgb(0x6b6b70),
                rgb(0xff7b7b),
                rgb(0x8ad890),
                rgb(0xf0d090),
                rgb(0x82bcff),
                rgb(0xd898ea),
                rgb(0x72cbd6),
                rgb(0xffffff),
            ],
        }
    }

    /// 浅色的配色（测试中换一套与默认不同的配色；正式数据由配色模块提供）。
    #[cfg(test)]
    pub(crate) fn default_light() -> Self {
        Self {
            foreground: rgb(0x2b2b2e),
            background: rgb(0xffffff),
            cursor: rgb(0x2b2b2e),
            cursor_text: None,
            selection_background: rgb(0xb4d5fe),
            selection_foreground: None,
            ansi: [
                rgb(0x000000),
                rgb(0xc7243a),
                rgb(0x2a8a3e),
                rgb(0x9a6a00),
                rgb(0x1f5fbf),
                rgb(0x9a3fae),
                rgb(0x187f8c),
                rgb(0xb8b8bc),
                rgb(0x6b6b70),
                rgb(0xe0384f),
                rgb(0x36a64f),
                rgb(0xb88200),
                rgb(0x2f78e0),
                rgb(0xb555c8),
                rgb(0x1d98a8),
                rgb(0x8e8e93),
            ],
        }
    }

    /// 0–255 号颜色：16 色、6×6×6 色立方、24 级灰阶。
    pub fn indexed(&self, index: u8) -> Rgb {
        match index {
            0..=15 => self.ansi[index as usize],
            16..=231 => {
                let i = index - 16;
                let level = |v: u8| if v == 0 { 0 } else { v * 40 + 55 };
                Rgb {
                    r: level(i / 36),
                    g: level((i / 6) % 6),
                    b: level(i % 6),
                }
            }
            232..=255 => {
                let v = (index - 232) * 10 + 8;
                Rgb { r: v, g: v, b: v }
            }
        }
    }

    /// 解析单元格的一个颜色。`overrides` 是程序用 OSC 4 / 10 / 11 改过的颜色。
    pub fn resolve(&self, color: Color, overrides: &ColorOverrides) -> Rgb {
        let slot = match color {
            Color::Rgb(rgb) => return rgb,
            Color::Indexed(index) => ColorSlot::Indexed(index),
            Color::Foreground => ColorSlot::Foreground,
            Color::Background => ColorSlot::Background,
        };
        self.query(slot, overrides)
    }

    /// 一个可查询的颜色的实际取值（回答 OSC 4 / 10 / 11 / 12 的查询、画光标）：程序改过的（`overrides`）优先。
    pub fn query(&self, slot: ColorSlot, overrides: &ColorOverrides) -> Rgb {
        overrides.get(slot).unwrap_or(match slot {
            ColorSlot::Indexed(index) => self.indexed(index),
            ColorSlot::Foreground => self.foreground,
            ColorSlot::Background => self.background,
            ColorSlot::Cursor => self.cursor,
        })
    }
}

/// 暗淡（SGR 2）：向背景色靠近三分之一。
pub fn dim(color: Rgb, background: Rgb) -> Rgb {
    let mix = |c: u8, b: u8| ((c as u16 * 2 + b as u16) / 3) as u8;
    Rgb {
        r: mix(color.r, background.r),
        g: mix(color.g, background.g),
        b: mix(color.b, background.b),
    }
}

/// 粗体用亮色（与 xterm.js 的 drawBoldTextInBrightColors 相同）：0–7 号颜色换成 8–15 号。
pub fn bold_bright(color: Color) -> Color {
    match color {
        Color::Indexed(index) if index < 8 => Color::Indexed(index + 8),
        other => other,
    }
}

pub fn to_hsla(color: Rgb) -> Hsla {
    Rgba {
        r: color.r as f32 / 255.0,
        g: color.g as f32 / 255.0,
        b: color.b as f32 / 255.0,
        a: 1.0,
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_colors_follow_xterm() {
        let palette = Palette::default_dark();
        assert_eq!(palette.indexed(1), palette.ansi[1]);
        assert_eq!(palette.indexed(16), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(
            palette.indexed(231),
            Rgb {
                r: 255,
                g: 255,
                b: 255
            }
        );
        assert_eq!(palette.indexed(196), Rgb { r: 255, g: 0, b: 0 });
        assert_eq!(palette.indexed(232), Rgb { r: 8, g: 8, b: 8 });
        assert_eq!(
            palette.indexed(255),
            Rgb {
                r: 238,
                g: 238,
                b: 238
            }
        );
    }

    #[test]
    fn overrides_take_precedence() {
        let palette = Palette::default_dark();
        let mut overrides = ColorOverrides::default();
        let custom = Rgb { r: 1, g: 2, b: 3 };
        overrides.set(ColorSlot::Foreground, Some(custom));
        assert_eq!(palette.resolve(Color::Foreground, &overrides), custom);
        assert_eq!(
            palette.resolve(Color::Background, &overrides),
            palette.background
        );
    }

    #[test]
    fn color_queries_prefer_overrides() {
        let palette = Palette::default_dark();
        let mut overrides = ColorOverrides::default();
        let custom = Rgb { r: 1, g: 2, b: 3 };
        overrides.set(ColorSlot::Background, Some(custom));
        overrides.set(ColorSlot::Indexed(4), Some(custom));
        assert_eq!(palette.query(ColorSlot::Background, &overrides), custom);
        assert_eq!(palette.query(ColorSlot::Indexed(4), &overrides), custom);
        assert_eq!(
            palette.query(ColorSlot::Indexed(1), &overrides),
            palette.ansi[1]
        );
        assert_eq!(
            palette.query(ColorSlot::Foreground, &overrides),
            palette.foreground
        );
        assert_eq!(palette.query(ColorSlot::Cursor, &overrides), palette.cursor);
        // 单元格的 4 号色同样用改过的
        assert_eq!(palette.resolve(Color::Indexed(4), &overrides), custom);
    }

    #[test]
    fn bold_uses_bright_variant() {
        // 红色（SGR 31）换成亮红（SGR 91）
        assert_eq!(bold_bright(Color::Indexed(1)), Color::Indexed(9));
        assert_eq!(bold_bright(Color::Indexed(3)), Color::Indexed(11));
        assert_eq!(bold_bright(Color::Indexed(100)), Color::Indexed(100));
        assert_eq!(bold_bright(Color::Foreground), Color::Foreground);
    }
}
