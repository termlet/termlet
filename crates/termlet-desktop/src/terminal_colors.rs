//! 终端的样式：字体、字号、选中即复制、右键的行为、光标的默认样式、关键词高亮，以及按"预览 > 当前 Tab > 主机 >
//! 分组 > 全局"选出的配色。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use termlet_core::id::{GroupId, HostId};
use termlet_core::model::{
    ColorScheme, CursorShape as CursorShapeSetting, HighlightSettings, Host,
    RightClick as RightClickSetting, Settings,
};
use termlet_core::schemes::{self, SchemeChoice};
use termlet_core::store::Store;
use termlet_terminal::{
    ContrastAdjust, CursorShape, HighlightColor, HighlightRuleSpec, Highlighter, Palette, Rgb,
    RightClick, TerminalStyle,
};

/// 一个终端在选配色时要看的几级（ID 为空表示这一级没有指定）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SchemeLevels {
    /// 命令面板"切换终端配色…"上下选择时的实时预览
    pub preview: String,
    /// 只用于当前 Tab 的配色
    pub tab: String,
    pub host: String,
    /// 分组链上最近设置了配色的分组的
    pub group: String,
}

impl SchemeLevels {
    /// 保存的主机：主机自己的、分组链上最近的。
    pub fn for_host(store: &Store, host: &HostId) -> Self {
        let Some(record) = store.get::<Host>(host) else {
            return Self::default();
        };
        let group = record.parent.clone().map(GroupId::from_raw);
        let group_scheme = store
            .group_chain(group.as_ref())
            .into_iter()
            .map(|g| g.doc.defaults.color_scheme)
            .find(|id| !id.trim().is_empty())
            .unwrap_or_default();
        Self {
            host: record.doc.terminal.color_scheme,
            group: group_scheme,
            ..Default::default()
        }
    }
}

/// 按设置与界面深浅生成终端样式。
pub fn terminal_style(
    store: &Store,
    settings: &Settings,
    dark: bool,
    levels: &SchemeLevels,
) -> TerminalStyle {
    let terminal = &settings.terminal;
    let custom = store.list::<ColorScheme>();
    let find_custom = |id: &str| custom.iter().find(|s| s.id.as_str() == id).map(|s| &s.doc);
    let choice = SchemeChoice {
        preview: Some(levels.preview.as_str()),
        tab: Some(levels.tab.as_str()),
        host: Some(levels.host.as_str()),
        group: Some(levels.group.as_str()),
    };
    let (scheme, _) = schemes::pick_scheme(&choice, terminal, dark, find_custom);
    let mut style = TerminalStyle {
        font_size: gpui_kit::px(terminal.font_size.clamp(8, 40) as f32),
        palette: palette_from(scheme),
        copy_on_select: terminal.copy_on_select,
        right_click: match terminal.right_click {
            RightClickSetting::Auto => RightClick::Auto,
            RightClickSetting::Paste => RightClick::Paste,
            RightClickSetting::Menu => RightClick::Menu,
        },
        cursor_shape: match terminal.cursor_shape {
            CursorShapeSetting::Block => CursorShape::Block,
            CursorShapeSetting::Bar => CursorShape::Bar,
            CursorShapeSetting::Underline => CursorShape::Underline,
        },
        cursor_blink: terminal.cursor_blink,
        option_as_meta: terminal.option_as_meta,
        ..TerminalStyle::default()
    };
    if !terminal.font_family.trim().is_empty() {
        style.font_family = terminal.font_family.trim().to_string().into();
    }
    // 汉字的字形跟随界面语言：日文、韩文界面优先用日文、韩文字体
    style.font_fallbacks = Some(termlet_terminal::fonts::fallbacks(
        match crate::i18n::language() {
            crate::i18n::Language::Ja => termlet_terminal::fonts::CjkPreference::Japanese,
            crate::i18n::Language::Ko => termlet_terminal::fonts::CjkPreference::Korean,
            _ => termlet_terminal::fonts::CjkPreference::Chinese,
        },
    ));
    // 保证文字可读：文字与背景的对比度低于设置的最低值时调整文字色（与 xterm.js 的 minimumContrastRatio 相同）
    let ratio = f64::from(terminal.min_contrast);
    if ratio > 1.0 {
        style.contrast = Some(ContrastAdjust(std::sync::Arc::new(
            move |fg: Rgb, bg: Rgb| {
                let to = |c: Rgb| schemes::Rgb {
                    r: c.r,
                    g: c.g,
                    b: c.b,
                };
                schemes::ensure_contrast(to(fg), to(bg), ratio).map(|c| Rgb {
                    r: c.r,
                    g: c.g,
                    b: c.b,
                })
            },
        )));
    }
    if let Some(highlighter) = highlighter(&settings.highlight) {
        style.highlight_colors = Some(Arc::new(highlight_colors(&highlighter, scheme)));
        style.highlighter = Some(highlighter);
    }
    style
}

/// 高亮器用到的颜色在这套配色上实际显示的样子（对背景对比度不够时核心会调亮或调暗，自定义的颜色也是）。
pub fn highlight_colors(
    highlighter: &Highlighter,
    scheme: &ColorScheme,
) -> HashMap<HighlightColor, Rgb> {
    let palette = schemes::highlight_palette(scheme);
    highlighter
        .colors()
        .iter()
        .filter_map(|&color| {
            let c = schemes::Rgb::parse(&palette.get(core_color(color)))?;
            Some((
                color,
                Rgb {
                    r: c.r,
                    g: c.g,
                    b: c.b,
                },
            ))
        })
        .collect()
}

/// 编译好的关键词高亮：所有终端共用同一个（终端按它的指针缓存匹配结果），设置变了才重新编译。
static HIGHLIGHTER: Mutex<Option<(HighlightSettings, Option<Arc<Highlighter>>)>> = Mutex::new(None);

/// 设置中的高亮规则编译成的高亮器；关闭了高亮、没有有效的规则时为 `None`。
pub fn highlighter(settings: &HighlightSettings) -> Option<Arc<Highlighter>> {
    let mut cached = HIGHLIGHTER.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((key, highlighter)) = cached.as_ref()
        && key == settings
    {
        return highlighter.clone();
    }
    let highlighter = settings.enabled.then(|| {
        let specs: Vec<HighlightRuleSpec> = settings.rules.iter().map(rule_spec).collect();
        let (highlighter, errors) = Highlighter::new(&specs);
        for error in errors {
            tracing::warn!(
                index = error.index,
                "关键词高亮的规则无效：{}",
                error.message
            );
        }
        Arc::new(highlighter)
    });
    *cached = Some((settings.clone(), highlighter.clone()));
    highlighter
}

/// 设置中的一条规则转成终端的规则（颜色不认识时用红色，与之前的版本相同）。
pub fn rule_spec(rule: &termlet_core::model::HighlightRule) -> HighlightRuleSpec {
    HighlightRuleSpec {
        pattern: rule.pattern.clone(),
        regex: rule.regex,
        case_sensitive: rule.case_sensitive,
        color: terminal_color(
            schemes::HighlightColor::from_name(&rule.color).unwrap_or(schemes::HighlightColor::Red),
        ),
        enabled: rule.enabled,
    }
}

/// 核心的高亮颜色（设置中的写法）转成终端的。
pub fn terminal_color(color: schemes::HighlightColor) -> HighlightColor {
    use schemes::HighlightColor as C;
    match color {
        C::Red => HighlightColor::Red,
        C::Green => HighlightColor::Green,
        C::Yellow => HighlightColor::Yellow,
        C::Blue => HighlightColor::Blue,
        C::Magenta => HighlightColor::Magenta,
        C::Cyan => HighlightColor::Cyan,
        C::DarkRed => HighlightColor::DarkRed,
        C::DarkGreen => HighlightColor::DarkGreen,
        C::DarkYellow => HighlightColor::DarkYellow,
        C::DarkBlue => HighlightColor::DarkBlue,
        C::DarkMagenta => HighlightColor::DarkMagenta,
        C::DarkCyan => HighlightColor::DarkCyan,
        C::BrightRed => HighlightColor::BrightRed,
        C::BrightGreen => HighlightColor::BrightGreen,
        C::BrightYellow => HighlightColor::BrightYellow,
        C::BrightBlue => HighlightColor::BrightBlue,
        C::BrightMagenta => HighlightColor::BrightMagenta,
        C::BrightCyan => HighlightColor::BrightCyan,
        C::Custom(c) => HighlightColor::Custom(Rgb {
            r: c.r,
            g: c.g,
            b: c.b,
        }),
    }
}

/// 终端的高亮颜色转回核心的（按配色算实际的颜色用）。
pub fn core_color(color: HighlightColor) -> schemes::HighlightColor {
    use schemes::HighlightColor as C;
    match color {
        HighlightColor::Red => C::Red,
        HighlightColor::Green => C::Green,
        HighlightColor::Yellow => C::Yellow,
        HighlightColor::Blue => C::Blue,
        HighlightColor::Magenta => C::Magenta,
        HighlightColor::Cyan => C::Cyan,
        HighlightColor::DarkRed => C::DarkRed,
        HighlightColor::DarkGreen => C::DarkGreen,
        HighlightColor::DarkYellow => C::DarkYellow,
        HighlightColor::DarkBlue => C::DarkBlue,
        HighlightColor::DarkMagenta => C::DarkMagenta,
        HighlightColor::DarkCyan => C::DarkCyan,
        HighlightColor::BrightRed => C::BrightRed,
        HighlightColor::BrightGreen => C::BrightGreen,
        HighlightColor::BrightYellow => C::BrightYellow,
        HighlightColor::BrightBlue => C::BrightBlue,
        HighlightColor::BrightMagenta => C::BrightMagenta,
        HighlightColor::BrightCyan => C::BrightCyan,
        HighlightColor::Custom(c) => C::Custom(schemes::Rgb::new(c.r, c.g, c.b)),
    }
}

/// 全局设置下（不看主机、分组）当前深浅所用的配色，设置页的预览用。
pub fn global_scheme(store: &Store, settings: &Settings, dark: bool) -> ColorScheme {
    let custom = store.list::<ColorScheme>();
    let find_custom = |id: &str| custom.iter().find(|s| s.id.as_str() == id).map(|s| &s.doc);
    let (scheme, _) = schemes::pick_scheme(
        &SchemeChoice::default(),
        &settings.terminal,
        dark,
        find_custom,
    );
    scheme.clone()
}

/// 配色转成终端的调色板。颜色写错时（存储保证不会）用前景色兜底。
pub fn palette_from(scheme: &ColorScheme) -> Palette {
    let parse = |hex: &str| {
        schemes::Rgb::parse(hex).map(|c| Rgb {
            r: c.r,
            g: c.g,
            b: c.b,
        })
    };
    let foreground = parse(&scheme.foreground).unwrap_or(Rgb {
        r: 0x80,
        g: 0x80,
        b: 0x80,
    });
    let background = parse(&scheme.background).unwrap_or(Rgb { r: 0, g: 0, b: 0 });
    let mut ansi = [foreground; 16];
    for (i, slot) in ansi.iter_mut().enumerate() {
        *slot = parse(schemes::ansi_color(scheme, i)).unwrap_or(foreground);
    }
    Palette {
        foreground,
        background,
        cursor: parse(&scheme.cursor).unwrap_or(foreground),
        cursor_text: parse(&scheme.cursor_accent),
        selection_background: parse(&scheme.selection_background).unwrap_or(foreground),
        selection_foreground: parse(&scheme.selection_foreground),
        ansi,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_scheme_converts_fully() {
        let scheme = schemes::default_scheme(true).scheme();
        let palette = palette_from(scheme);
        let bg = schemes::Rgb::parse(&scheme.background).unwrap();
        assert_eq!(
            (
                palette.background.r,
                palette.background.g,
                palette.background.b
            ),
            (bg.r, bg.g, bg.b)
        );
        let red = schemes::Rgb::parse(&scheme.ansi[1]).unwrap();
        assert_eq!(
            (palette.ansi[1].r, palette.ansi[1].g, palette.ansi[1].b),
            (red.r, red.g, red.b)
        );
        // 没有光标文字色时为 None（终端用背景色）
        assert_eq!(
            palette.cursor_text.is_some(),
            !scheme.cursor_accent.is_empty()
        );
    }

    // 设置中的颜色转成终端的：深色、亮色、自定义颜色照原样，不认识的按红色（与之前的版本相同）；颜色表是高亮器用到的
    // 颜色在这套配色上实际显示的样子（白色在浅色配色上调深了）
    #[test]
    fn highlight_rules_use_bright_and_custom_colors() {
        use termlet_core::model::HighlightRule;
        let rule = |pattern: &str, color: &str| HighlightRule {
            pattern: pattern.into(),
            color: color.into(),
            enabled: true,
            ..Default::default()
        };
        let white = Rgb {
            r: 0xff,
            g: 0xff,
            b: 0xff,
        };
        assert_eq!(
            rule_spec(&rule("a", "brightCyan")).color,
            HighlightColor::BrightCyan
        );
        assert_eq!(
            rule_spec(&rule("a", "darkBlue")).color,
            HighlightColor::DarkBlue
        );
        assert_eq!(
            rule_spec(&rule("a", "#FFFFFF")).color,
            HighlightColor::Custom(white)
        );
        assert_eq!(rule_spec(&rule("a", "dim")).color, HighlightColor::Red);
        for color in schemes::HighlightColor::NAMED {
            assert_eq!(core_color(terminal_color(color)), color);
        }
        let specs: Vec<_> = [
            rule("a", "#ffffff"),
            rule("b", "brightRed"),
            rule("c", "#ffffff"),
        ]
        .iter()
        .map(rule_spec)
        .collect();
        let (highlighter, _) = Highlighter::new(&specs);
        let light = schemes::default_scheme(false).scheme();
        let table = highlight_colors(&highlighter, light);
        assert_eq!(table.len(), 2);
        let shown = table[&HighlightColor::Custom(white)];
        assert_ne!(shown, white, "白底上的白色调深");
        let hex = format!("#{:02x}{:02x}{:02x}", shown.r, shown.g, shown.b);
        assert!(
            schemes::contrast_ratio(&hex, &light.background) >= schemes::MIN_HIGHLIGHT_CONTRAST
        );
        // 深色配色上白色原样
        let dark = schemes::default_scheme(true).scheme();
        assert_eq!(
            highlight_colors(&highlighter, dark)[&HighlightColor::Custom(white)],
            white
        );
    }

    // 光标的默认样式按设置：方块（默认，不闪烁）、竖线、下划线，闪烁与否
    #[test]
    fn cursor_follows_the_settings() {
        use termlet_core::db::Database;
        use termlet_core::vault::{MemoryKeyStore, Vault};
        let db = Database::open_in_memory().unwrap();
        let vault = Vault::open(&db, Box::new(MemoryKeyStore::default())).unwrap();
        let store = Store::open(db, vault).unwrap();
        let levels = SchemeLevels::default();
        let mut settings = Settings::default();
        let style = terminal_style(&store, &settings, true, &levels);
        assert_eq!(
            (style.cursor_shape, style.cursor_blink),
            (CursorShape::Block, false)
        );
        for (setting, shape) in [
            (CursorShapeSetting::Bar, CursorShape::Bar),
            (CursorShapeSetting::Underline, CursorShape::Underline),
            (CursorShapeSetting::Block, CursorShape::Block),
        ] {
            settings.terminal.cursor_shape = setting;
            settings.terminal.cursor_blink = true;
            let style = terminal_style(&store, &settings, false, &levels);
            assert_eq!((style.cursor_shape, style.cursor_blink), (shape, true));
        }
    }
}
