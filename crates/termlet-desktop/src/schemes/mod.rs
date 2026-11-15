//! 终端配色的界面：设置中的配色库（`settings::schemes`）、配色编辑器、导入与导出对话框、命令面板的“切换终端配色…”，
//! 以及它们共用的配色列表、示例的绘制、颜色名称与可读性提示。
//!
//! 配色本身、选取规则与可读性检查都在核心（`termlet_core::schemes`），这里只负责显示。

pub mod editor;
pub mod export;
pub mod import;
pub mod picker;

use std::ops::Range;
use std::rc::Rc;

use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::{
    App, ElementId, FontFeatures, FontWeight, HighlightStyle, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, RenderOnce, Rgba, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledText, Window, div,
    prelude::FluentBuilder as _, relative,
};
use termlet_core::model::{ColorScheme, HighlightSettings, Settings, TerminalSettings};
use termlet_core::schemes::{
    self, AnsiColor, ColorKey, HighlightColor, SchemeOrigin, SchemeSource, find_scheme,
    is_dark_scheme, scheme_sample, unreadable_colors,
};
use termlet_core::store::Store;

use crate::t;
use crate::terminal_colors::SchemeLevels;

/// 列表中的一套配色：内置的，或自定义的（存储中的，或设置中还没有保存的）。
#[derive(Clone, Debug)]
pub struct SchemeEntry {
    /// 内置配色的固定 ID、存储中的 ID，或设置中新增、还没有保存的临时 ID
    pub id: String,
    pub scheme: ColorScheme,
    pub kind: SchemeKind,
}

#[derive(Clone, Copy, Debug)]
pub enum SchemeKind {
    /// 内置配色与它的出处（Termlet 自己调的没有出处）
    Builtin(Option<&'static SchemeSource>),
    Custom,
}

impl SchemeEntry {
    pub fn is_builtin(&self) -> bool {
        matches!(self.kind, SchemeKind::Builtin(_))
    }

    pub fn is_dark(&self) -> bool {
        is_dark_scheme(&self.scheme)
    }

    /// 列表中的说明：“深色 · 内置”。
    pub fn detail(&self) -> SharedString {
        let kind = if self.is_builtin() {
            t!("schemes.kind.builtin")
        } else {
            t!("schemes.kind.custom")
        };
        t!(
            "schemes.detail",
            mode = mode_label(self.is_dark()),
            kind = kind
        )
    }

    /// 出处的项目名（搜索时也按它找）；自定义配色、Termlet 自己的配色为空。
    pub fn project(&self) -> &'static str {
        match self.kind {
            SchemeKind::Builtin(Some(source)) => source.project,
            _ => "",
        }
    }
}

/// 全部内置配色，按设置中列出的顺序。
pub fn builtin_entries() -> Vec<SchemeEntry> {
    schemes::builtin_schemes()
        .iter()
        .map(|b| SchemeEntry {
            id: b.id().to_string(),
            scheme: b.scheme().clone(),
            kind: SchemeKind::Builtin(b.source()),
        })
        .collect()
}

/// 存储中的自定义配色，按存储中的顺序（先建的在前）。
pub fn custom_entries(store: &Store) -> Vec<SchemeEntry> {
    store
        .list::<ColorScheme>()
        .into_iter()
        .map(|r| SchemeEntry {
            id: r.id.as_str().to_string(),
            scheme: r.doc,
            kind: SchemeKind::Custom,
        })
        .collect()
}

/// 终端实际使用的配色的 ID 与来源（不算命令面板中的预览）：Tab 临时 > 主机 > 分组 > 全局槽，某一级的 ID
/// 不认识时跳到下一级，与 [`schemes::pick_scheme`] 的规则相同。`custom` 是自定义配色。
pub fn effective_scheme(
    levels: &SchemeLevels,
    terminal: &TerminalSettings,
    dark: bool,
    custom: &[SchemeEntry],
) -> (String, SchemeOrigin) {
    let known = |id: &str| {
        find_scheme(id, |id| {
            custom.iter().find(|e| e.id == id).map(|e| &e.scheme)
        })
        .is_some()
    };
    let levels = [
        (levels.tab.as_str(), SchemeOrigin::Tab),
        (levels.host.as_str(), SchemeOrigin::Host),
        (levels.group.as_str(), SchemeOrigin::Group),
    ];
    if let Some((id, origin)) = levels.into_iter().find(|(id, _)| known(id)) {
        return (id.to_string(), origin);
    }
    let slot = if dark {
        &terminal.scheme_dark
    } else {
        &terminal.scheme_light
    };
    let id = if known(slot) {
        slot.clone()
    } else {
        schemes::default_scheme(dark).id().to_string()
    };
    (id, SchemeOrigin::Global)
}

// ---------------------------------------------------------------- 名称与提示

pub fn mode_label(dark: bool) -> SharedString {
    if dark {
        t!("schemes.mode.dark")
    } else {
        t!("schemes.mode.light")
    }
}

/// 16 色中第 `i` 种的名称（黑、红……亮白）。
pub fn ansi_name(i: usize) -> SharedString {
    match i {
        0 => t!("schemes.color.black"),
        1 => t!("schemes.color.red"),
        2 => t!("schemes.color.green"),
        3 => t!("schemes.color.yellow"),
        4 => t!("schemes.color.blue"),
        5 => t!("schemes.color.magenta"),
        6 => t!("schemes.color.cyan"),
        7 => t!("schemes.color.white"),
        8 => t!("schemes.color.bright_black"),
        9 => t!("schemes.color.bright_red"),
        10 => t!("schemes.color.bright_green"),
        11 => t!("schemes.color.bright_yellow"),
        12 => t!("schemes.color.bright_blue"),
        13 => t!("schemes.color.bright_magenta"),
        14 => t!("schemes.color.bright_cyan"),
        _ => t!("schemes.color.bright_white"),
    }
}

fn color_key_name(key: ColorKey) -> SharedString {
    match key {
        ColorKey::Foreground => t!("schemes.color.foreground"),
        ColorKey::Ansi(color) => ansi_name(color.index()),
    }
}

/// 在背景上看不清的颜色的名称（“黄、蓝”），放进各处的说明中；都看得清时为 `None`。
pub fn unreadable_names(scheme: &ColorScheme) -> Option<String> {
    let keys = unreadable_colors(scheme);
    if keys.is_empty() {
        return None;
    }
    let names = keys
        .into_iter()
        .map(|k| color_key_name(k).to_string())
        .collect::<Vec<_>>()
        .join(&t!("schemes.list_separator"));
    Some(names)
}

// ---------------------------------------------------------------- 颜色

/// `#rrggbb` 转成界面的颜色；写错时按黑色（存储与导入保证不会）。配色中的颜色是数据，不是界面的主题色。
pub fn hsla(hex: &str) -> Hsla {
    let c = schemes::Rgb::parse(hex).unwrap_or(schemes::Rgb::BLACK);
    Rgba {
        r: f32::from(c.r) / 255.,
        g: f32::from(c.g) / 255.,
        b: f32::from(c.b) / 255.,
        a: 1.,
    }
    .into()
}

/// 界面的颜色转成小写的 `#rrggbb`（四舍五入，不带透明度）。
pub fn hex_of(color: Hsla) -> String {
    let rgba = color.to_rgb();
    let channel = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
    schemes::Rgb::new(channel(rgba.r), channel(rgba.g), channel(rgba.b)).to_hex()
}

/// 配色示例用的字体：设置中的终端字体，没有设置时用内置的等宽字体。
pub fn sample_font(settings: &Settings) -> SharedString {
    let family = settings.terminal.font_family.trim();
    if family.is_empty() {
        termlet_terminal::fonts::DEFAULT_FAMILY.into()
    } else {
        family.to_string().into()
    }
}

// ---------------------------------------------------------------- 示例

/// 编译好的关键词高亮规则：配色示例中的日志按它着色（与终端中的高亮一致）。关闭高亮时没有规则。
#[derive(Default)]
pub struct SampleHighlights {
    rules: Vec<(regex::Regex, HighlightColor)>,
}

impl SampleHighlights {
    pub fn new(settings: &HighlightSettings) -> Rc<Self> {
        if !settings.enabled {
            return Rc::default();
        }
        let rules = settings
            .rules
            .iter()
            .filter(|r| r.enabled && !r.pattern.trim().is_empty())
            .filter_map(|rule| {
                let source = if rule.regex {
                    rule.pattern.clone()
                } else {
                    regex::escape(&rule.pattern)
                };
                let re = regex::RegexBuilder::new(&source)
                    .case_insensitive(!rule.case_sensitive)
                    .build()
                    .ok()?;
                let color = HighlightColor::from_name(&rule.color).unwrap_or(HighlightColor::Red);
                Some((re, color))
            })
            .collect();
        Rc::new(Self { rules })
    }

    /// 一行文字中的命中：按位置排序、互不重叠，重叠时排在前面的规则优先。
    fn matches(&self, text: &str) -> Vec<(Range<usize>, HighlightColor)> {
        let mut found: Vec<(Range<usize>, HighlightColor)> = Vec::new();
        for (re, color) in &self.rules {
            for m in re.find_iter(text).filter(|m| !m.is_empty()) {
                if !found
                    .iter()
                    .any(|(r, _)| m.start() < r.end && m.end() > r.start)
                {
                    found.push((m.range(), *color));
                }
            }
        }
        found.sort_by_key(|(r, _)| r.start);
        found
    }
}

/// 配色的示例：提示符、ls、git diff、按关键词高亮着色的日志（见 [`scheme_sample`]），下面是 16 色色块，
/// 在背景上看不清的色块标出来。颜色都取配色本身。
#[derive(IntoElement)]
pub struct SchemeSample {
    scheme: ColorScheme,
    highlights: Rc<SampleHighlights>,
    font_family: SharedString,
}

impl SchemeSample {
    pub fn new(
        scheme: &ColorScheme,
        highlights: &Rc<SampleHighlights>,
        font_family: &SharedString,
    ) -> Self {
        Self {
            scheme: scheme.clone(),
            highlights: highlights.clone(),
            font_family: font_family.clone(),
        }
    }
}

impl RenderOnce for SchemeSample {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let highlights = self.highlights.clone();
        let lines = scheme_sample(&self.scheme, move |text| highlights.matches(text));
        let unreadable = unreadable_colors(&self.scheme);
        let danger = cx.theme().danger;
        let swatch = |i: usize| {
            let color = schemes::ansi_color(&self.scheme, i).to_string();
            let bad = i < 8 && unreadable.contains(&ColorKey::Ansi(AnsiColor::ALL[i]));
            let tip = if bad {
                t!(
                    "schemes.swatch_unreadable",
                    name = ansi_name(i),
                    color = color.clone()
                )
            } else {
                t!("schemes.swatch", name = ansi_name(i), color = color.clone())
            };
            // 16 色按位置排列，位置就是颜色的身份
            div()
                .id(ElementId::from(("swatch", i)))
                .flex_1()
                .h_2p5()
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .bg(hsla(&color))
                .when(bad, |this| {
                    this.border_1()
                        .border_color(danger)
                        .child(div().size_1().rounded_full().bg(danger))
                })
                .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
        };
        v_flex()
            .w_full()
            .px_2p5()
            .py_2()
            .gap_0()
            .bg(hsla(&self.scheme.background))
            .text_color(hsla(&self.scheme.foreground))
            .font_family(self.font_family)
            // 与终端一致：不显示连字
            .font_features(FontFeatures::disable_ligatures())
            .text_xs()
            .line_height(relative(1.45))
            .children(lines.into_iter().map(|line| {
                let mut text = String::new();
                let mut styles = Vec::new();
                for span in line {
                    let start = text.len();
                    text.push_str(span.text);
                    let style = HighlightStyle {
                        color: span.fg.as_deref().map(hsla),
                        background_color: span.bg.as_deref().map(hsla),
                        font_weight: span.bold.then_some(FontWeight::BOLD),
                        ..Default::default()
                    };
                    if style != HighlightStyle::default() {
                        styles.push((start..text.len(), style));
                    }
                }
                div()
                    .w_full()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(StyledText::new(text).with_highlights(styles))
            }))
            .child(
                v_flex()
                    .pt_1p5()
                    .gap_0p5()
                    .child(h_flex().gap_0p5().children((0..8).map(swatch)))
                    .child(h_flex().gap_0p5().children((8..16).map(swatch))),
            )
    }
}

/// 列表中的小色块：配色的背景上三个点（红、绿、蓝）。
#[derive(IntoElement)]
pub struct SchemeSwatch {
    background: Hsla,
    dots: [Hsla; 3],
}

impl SchemeSwatch {
    pub fn new(scheme: &ColorScheme) -> Self {
        Self {
            background: hsla(&scheme.background),
            dots: [1, 2, 4].map(|i| hsla(schemes::ansi_color(scheme, i))),
        }
    }
}

impl RenderOnce for SchemeSwatch {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        h_flex()
            .flex_none()
            .h_4()
            .w_7()
            .justify_center()
            .gap_px()
            .rounded_sm()
            .border_1()
            .border_color(cx.theme().border)
            .bg(self.background)
            .children(
                self.dots
                    .into_iter()
                    .map(|c| div().size_1p5().rounded_full().bg(c)),
            )
    }
}
