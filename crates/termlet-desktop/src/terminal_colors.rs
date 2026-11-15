//! 终端的样式：字体、字号、选中即复制、右键的行为、光标的默认样式、关键词高亮，以及按"预览 > 当前 Tab > 主机 >
//! 分组 > 全局"选出的配色。

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
        style.highlighter = Some(highlighter);
        // 高亮色取这套配色的（对背景对比度不够时核心会调亮或调暗）
        let palette = schemes::highlight_palette(scheme);
        let colors = HighlightColor::ALL.map(|color| {
            let core = schemes::HighlightColor::from_name(color.name())
                .unwrap_or(schemes::HighlightColor::Red);
            schemes::Rgb::parse(palette.get(core))
                .map(|c| Rgb {
                    r: c.r,
                    g: c.g,
                    b: c.b,
                })
                .unwrap_or(style.palette.ansi[color.ansi_index() as usize])
        });
        style.highlight_colors = Some(colors);
    }
    style
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

/// 设置中的一条规则转成终端的规则（颜色名不认识时用红色）。
pub fn rule_spec(rule: &termlet_core::model::HighlightRule) -> HighlightRuleSpec {
    HighlightRuleSpec {
        pattern: rule.pattern.clone(),
        regex: rule.regex,
        case_sensitive: rule.case_sensitive,
        color: HighlightColor::from_name(&rule.color).unwrap_or_default(),
        enabled: rule.enabled,
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
