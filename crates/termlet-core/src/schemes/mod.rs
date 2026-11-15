//! 终端配色：内置配色（带来源与许可）、按 ID 查找与选取、可读性辅助、导入导出，
//! 以及读取本机已装客户端（Windows Terminal、Xshell、MobaXterm、Tabby）中的配色。
//!
//! 配色本身一律是 [`ColorScheme`]，颜色为小写的 `#rrggbb`。深浅不单独记录，按背景判断（[`is_dark`]）。
//! 内置配色只读、ID 固定（例如 `termlet-light`，发布后不能改）；导入、复制得到的是自定义配色，ID 由存储分配。
//! 设置、主机、分组中的配色 ID 不认识时（新版本同步过来的内置配色、已删除的自定义配色）跳到下一级，
//! 最后退回默认配色，不报错。

mod builtin;
mod color;
mod derived;
mod export;
mod import;
mod installed;
#[cfg(test)]
mod licenses;

use std::collections::HashSet;
use std::sync::LazyLock;

use crate::model::{ColorScheme, DEFAULT_SCHEME_DARK, DEFAULT_SCHEME_LIGHT, TerminalSettings};

pub use color::{
    MIN_CONTRAST_ON, Rgb, contrast_ratio, ensure_contrast, is_dark, mix, normalize_hex,
};
pub use derived::{
    AnsiColor, ColorKey, HighlightColor, HighlightPalette, MIN_HIGHLIGHT_CONTRAST, MIN_READABLE,
    SampleSpan, SearchColors, highlight_palette, scheme_sample, search_colors, unreadable_colors,
};
pub use export::{to_iterm_colors, to_windows_terminal_json};
pub use import::{
    ColorField, ImportWarning, MAX_INPUT_BYTES, MAX_SCHEMES, ParseResult, SchemeFormat,
    decode_scheme_file, parse_color, parse_scheme_bytes, parse_schemes,
};
pub use installed::{
    InstalledSchemeSource, MOBAXTERM_REGISTRY_PATH, SchemeClient, custom_scheme_count,
    installed_color_schemes,
};

/// 自定义配色最多这么多套。
pub const MAX_CUSTOM_SCHEMES: usize = 100;
/// 配色名称最长这么多个字符（与 [`ColorScheme`] 的校验一致）。
pub const MAX_SCHEME_NAME: usize = 64;

// ---- 内置配色 ----

/// 内置配色的许可（只收宽松许可）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum License {
    Mit,
    Apache2,
    X11,
}

impl License {
    /// SPDX 许可标识，例如 `MIT`、`Apache-2.0`。
    pub fn spdx(self) -> &'static str {
        match self {
            License::Mit => "MIT",
            License::Apache2 => "Apache-2.0",
            License::X11 => "X11",
        }
    }
}

/// 内置配色的出处（随包附带的许可文本、设置中的"来源"按此生成）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchemeSource {
    /// 项目名，如 Catppuccin
    pub project: &'static str,
    /// 取色的原始文件地址
    pub url: &'static str,
    pub license: License,
    /// 版权行，如 Copyright (c) 2021 Catppuccin
    pub copyright: &'static str,
}

/// 一套内置配色：固定的 ID、配色本身与出处。只读，只能经由 [`builtin_schemes`] 等取得。
#[derive(Debug)]
pub struct BuiltinScheme {
    id: &'static str,
    scheme: ColorScheme,
    source: Option<SchemeSource>,
}

impl BuiltinScheme {
    /// 固定的 ID（如 `termlet-light`），设置、主机、分组按它引用。
    pub fn id(&self) -> &'static str {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.scheme.name
    }

    /// 配色本身（用于显示、预览；复制为自定义配色见 [`to_custom_scheme`]）。
    pub fn scheme(&self) -> &ColorScheme {
        &self.scheme
    }

    /// 出处；Termlet 自己调的配色（高对比度）没有。基于别的配色调整的是原配色的出处。
    pub fn source(&self) -> Option<&SchemeSource> {
        self.source.as_ref()
    }

    pub fn is_dark(&self) -> bool {
        is_dark_scheme(&self.scheme)
    }
}

static BUILTIN: LazyLock<Vec<BuiltinScheme>> = LazyLock::new(|| {
    builtin::SCHEMES
        .iter()
        .map(|raw| BuiltinScheme {
            id: raw.id,
            scheme: ColorScheme {
                name: raw.name.into(),
                foreground: raw.foreground.into(),
                background: raw.background.into(),
                cursor: raw.cursor.into(),
                cursor_accent: raw.cursor_accent.unwrap_or_default().into(),
                selection_background: raw.selection_background.into(),
                selection_foreground: raw.selection_foreground.unwrap_or_default().into(),
                ansi: raw.ansi.iter().map(|c| c.to_string()).collect(),
                extra: Default::default(),
            },
            source: raw.source,
        })
        .collect()
});

/// 全部内置配色，按设置中列出的顺序（Termlet 浅色、深色在最前）。
pub fn builtin_schemes() -> &'static [BuiltinScheme] {
    &BUILTIN
}

/// 按 ID 查内置配色。
pub fn builtin_scheme(id: &str) -> Option<&'static BuiltinScheme> {
    BUILTIN.iter().find(|s| s.id == id)
}

/// 默认配色：界面为深色时用 Termlet Dark，否则用 Termlet Light。
pub fn default_scheme(dark: bool) -> &'static BuiltinScheme {
    let id = if dark {
        DEFAULT_SCHEME_DARK
    } else {
        DEFAULT_SCHEME_LIGHT
    };
    // 内置数据中一定有这两套（单元测试检查），找不到时退回第一套
    builtin_scheme(id).unwrap_or(&BUILTIN[0])
}

/// 随安装包分发的内置配色许可说明（`terminal-color-schemes.txt` 的全文）。只在测试中生成：
/// `license_text_matches_the_shipped_file` 核对随包的文件，按需重新生成。
#[cfg(test)]
pub(crate) fn license_text() -> String {
    licenses::build_license_text(&BUILTIN)
}

/// 深色配色：白字在背景上比黑字更清楚。
pub fn is_dark_scheme(scheme: &ColorScheme) -> bool {
    is_dark(&scheme.background)
}

/// 配色的第 `i` 种 ANSI 色（0–15）；数据不完整时用前景色。
pub fn ansi_color(scheme: &ColorScheme, i: usize) -> &str {
    scheme.ansi.get(i).unwrap_or(&scheme.foreground)
}

// ---- 查找与选取 ----

/// 配色从哪里来：命令面板的预览、Tab 临时换的、主机的、分组默认的、全局设置（按界面深浅的两个槽）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SchemeOrigin {
    Preview,
    Tab,
    Host,
    Group,
    Global,
}

/// 各级指定的配色 ID；`None` 或空字符串表示这一级没有指定。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SchemeChoice<'a> {
    pub preview: Option<&'a str>,
    pub tab: Option<&'a str>,
    pub host: Option<&'a str>,
    pub group: Option<&'a str>,
}

/// 按 ID 查找配色：先找内置配色，再用 `custom` 找自定义配色（存储中按 ID 取）。ID 为空或不认识时返回 `None`。
pub fn find_scheme<'a>(
    id: &str,
    custom: impl Fn(&str) -> Option<&'a ColorScheme>,
) -> Option<&'a ColorScheme> {
    if id.is_empty() {
        return None;
    }
    builtin_scheme(id).map(|b| &b.scheme).or_else(|| custom(id))
}

/// 全局设置中按界面深浅的配色；槽中的 ID 不认识时用默认配色。
pub fn slot_scheme<'a>(
    settings: &TerminalSettings,
    dark: bool,
    custom: impl Fn(&str) -> Option<&'a ColorScheme>,
) -> &'a ColorScheme {
    let id = if dark {
        &settings.scheme_dark
    } else {
        &settings.scheme_light
    };
    find_scheme(id, custom).unwrap_or(&default_scheme(dark).scheme)
}

/// 终端实际使用的配色：预览 > Tab 临时 > 主机 > 分组 > 全局槽（按界面当前的深浅）。
/// 某一级的 ID 不认识时跳到下一级。
pub fn pick_scheme<'a>(
    choice: &SchemeChoice<'_>,
    settings: &TerminalSettings,
    dark: bool,
    custom: impl Fn(&str) -> Option<&'a ColorScheme>,
) -> (&'a ColorScheme, SchemeOrigin) {
    let levels = [
        (choice.preview, SchemeOrigin::Preview),
        (choice.tab, SchemeOrigin::Tab),
        (choice.host, SchemeOrigin::Host),
        (choice.group, SchemeOrigin::Group),
    ];
    for (id, origin) in levels {
        if let Some(scheme) = id.and_then(|id| find_scheme(id, &custom)) {
            return (scheme, origin);
        }
    }
    (slot_scheme(settings, dark, &custom), SchemeOrigin::Global)
}

// ---- 自定义配色 ----

/// 名称与 `taken` 中的重复（不区分大小写）时改为"名称 (1)""名称 (2)"……；过长的截断，空的改为 `Scheme`。
pub fn unique_name<'t>(name: &str, taken: impl IntoIterator<Item = &'t str>) -> String {
    let used: HashSet<String> = taken.into_iter().map(str::to_lowercase).collect();
    let trimmed = name.trim();
    let base: String = if trimmed.is_empty() {
        "Scheme"
    } else {
        trimmed
    }
    .chars()
    .take(MAX_SCHEME_NAME)
    .collect();
    if !used.contains(&base.to_lowercase()) {
        return base;
    }
    (1usize..)
        .map(|i| {
            let suffix = format!(" ({i})");
            let head: String = base
                .chars()
                .take(MAX_SCHEME_NAME.saturating_sub(suffix.len()))
                .collect();
            format!("{head}{suffix}")
        })
        .find(|candidate| !used.contains(&candidate.to_lowercase()))
        .unwrap_or(base)
}

/// 把一套配色（导入的、内置的）变成新的自定义配色：名称与已有的重复时加编号，颜色规范为小写的 `#rrggbb`，
/// 去掉不认识的字段。必需的颜色不合法、不是 16 色时返回 `None`；可选的颜色不合法时去掉。
pub fn to_custom_scheme<'t>(
    scheme: &ColorScheme,
    taken: impl IntoIterator<Item = &'t str>,
) -> Option<ColorScheme> {
    if scheme.ansi.len() != 16 {
        return None;
    }
    let optional = |c: &str| normalize_hex(c).unwrap_or_default();
    Some(ColorScheme {
        name: unique_name(&scheme.name, taken),
        foreground: normalize_hex(&scheme.foreground)?,
        background: normalize_hex(&scheme.background)?,
        cursor: normalize_hex(&scheme.cursor)?,
        cursor_accent: optional(&scheme.cursor_accent),
        selection_background: normalize_hex(&scheme.selection_background)?,
        selection_foreground: optional(&scheme.selection_foreground),
        ansi: scheme
            .ansi
            .iter()
            .map(|c| normalize_hex(c))
            .collect::<Option<Vec<_>>>()?,
        extra: Default::default(),
    })
}

/// 配色是否完整、颜色都合法、名称不为空且不过长，即存储能否接受（编辑器保存前检查）。
pub fn is_valid_scheme(scheme: &ColorScheme) -> bool {
    scheme.clone().normalize().is_ok()
}

#[cfg(test)]
mod tests;
