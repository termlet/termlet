//! 导入终端配色：自动识别 Windows Terminal、iTerm2、Xshell、MobaXterm、base16 / base24、Alacritty、Kitty、
//! Ghostty、Xresources、Tabby、Gogh 等格式并解析出配色。
//!
//! 格式以内容为准，文件扩展名只决定先试哪种格式。输入不可信：限制文本大小与配色数量；解析不递归（或限制深度），
//! 正则都是线性时间的；坏输入只会得到空结果和提示，不会出错。YAML、TOML 用只解析配色所需部分的宽容解析器
//! （见 `yaml.rs`、`text.rs`），以便接受配色文件中常见的不规范写法。

mod color;
mod formats;
mod text;
mod tree;
mod yaml;

use crate::model::ColorScheme;
use crate::schemes::{AnsiColor, MAX_SCHEME_NAME};

pub use color::parse_color;
pub(super) use formats::is_mobaxterm_builtin;
pub use text::decode_scheme_file;
pub(super) use text::{decode_with_fallback, strip_jsonc};

/// 超过这个大小（字节）的文本不解析。
pub const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;
/// 一次最多解析出的配色数。
pub const MAX_SCHEMES: usize = 200;
/// 名称中超过这个长度的部分截断（与 [`ColorScheme`] 的校验一致）。
const MAX_NAME: usize = MAX_SCHEME_NAME;
/// 没有名称、也没有文件名时的名称。
const FALLBACK_NAME: &str = "Imported";

/// 能识别的导入格式。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SchemeFormat {
    WindowsTerminal,
    Iterm2,
    Xshell,
    MobaXterm,
    Base16,
    Alacritty,
    Kitty,
    Ghostty,
    Xresources,
    Tabby,
    Gogh,
}

/// 配色中的一项颜色（导入提示中指出哪一项有问题）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ColorField {
    Foreground,
    Background,
    Cursor,
    CursorAccent,
    Selection,
    SelectionForeground,
    Ansi { color: AnsiColor, bright: bool },
}

impl ColorField {
    fn ansi(i: usize) -> Self {
        ColorField::Ansi {
            color: AnsiColor::ALL[i % 8],
            bright: i >= 8,
        }
    }
}

/// 解析时跳过、补全的内容，由界面翻译后显示。`scheme` 是这套配色的名称。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportWarning {
    /// 文本超过 `max_bytes`，没有解析
    TooLarge {
        max_bytes: usize,
    },
    /// 配色超过 `max` 套，只取了前面的
    TooMany {
        max: usize,
    },
    /// 认出了格式，但其中没有配色
    NoSchemes,
    /// 有 `count` 套颜色太少，不算配色
    Skipped {
        count: usize,
    },
    /// 二进制格式的 plist（只能读取 XML 格式的文本时）
    BinaryPlist,
    /// 颜色空间不是 sRGB（iTerm2 的 P3 等），按 sRGB 近似
    ColorSpace {
        scheme: String,
        space: String,
    },
    /// 颜色的写法认不出，按缺失处理；`value` 是原文（超过 40 个字符的截断并加省略号）
    InvalidColor {
        scheme: String,
        field: ColorField,
        value: String,
    },
    /// 缺少 `count` 种普通 ANSI 色，用对应的 bright 色或 xterm 的原生色补上
    MissingAnsi {
        scheme: String,
        count: usize,
    },
    /// 缺少 `count` 种 bright 色，用对应的普通色补上
    MissingBright {
        scheme: String,
        count: usize,
    },
    MissingForeground {
        scheme: String,
    },
    MissingBackground {
        scheme: String,
    },
    /// 缺少光标色，用前景色
    MissingCursor {
        scheme: String,
    },
    /// 缺少选区色，由前景与背景混合
    MissingSelection {
        scheme: String,
    },
}

/// 解析的结果。认不出格式时 `format` 为 `None`、`schemes` 为空。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParseResult {
    pub format: Option<SchemeFormat>,
    /// 解析出的配色（颜色已规范为小写的 `#rrggbb`、补全为 16 色）；保存为自定义配色前用
    /// [`to_custom_scheme`](crate::schemes::to_custom_scheme) 处理重名
    pub schemes: Vec<ColorScheme>,
    pub warnings: Vec<ImportWarning>,
}

/// 各格式解析出的原始值（未校验的字符串），由 [`finalize`] 统一校验、补全。
#[derive(Default)]
pub(super) struct Raw {
    pub name: Option<String>,
    pub foreground: Option<String>,
    pub background: Option<String>,
    pub cursor: Option<String>,
    pub cursor_accent: Option<String>,
    pub selection_background: Option<String>,
    pub selection_foreground: Option<String>,
    /// 16 个位置，缺失为 `None`
    pub ansi: [Option<String>; 16],
    /// 颜色空间不是 sRGB 时记下（iTerm2）
    pub color_space: Option<String>,
}

pub(super) struct Detected {
    pub format: SchemeFormat,
    pub raws: Vec<Raw>,
}

pub(super) struct Ctx {
    pub fallback_name: String,
    pub warnings: Vec<ImportWarning>,
}

/// 缺少普通 ANSI 色（且没有对应的 bright 色）时的补色：xterm 原生配色。
const FALLBACK_ANSI: [&str; 8] = [
    "#000000", "#cd0000", "#00cd00", "#cdcd00", "#0000ee", "#cd00cd", "#00cdcd", "#e5e5e5",
];

/// 名称：去掉控制字符（含 U+0085 等 C1 控制字符）与两端的空白，过长的截断；空的为 `None`。
pub(super) fn clean_name(name: Option<&str>) -> Option<String> {
    let s: String = name?
        .chars()
        .filter(|c| !matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}'))
        .collect();
    let s = s.trim();
    (!s.is_empty()).then(|| s.chars().take(MAX_NAME).collect())
}

/// 提示中引用的原文：超过 40 个字符的截断并加省略号。
fn short(s: &str) -> String {
    if s.chars().count() > 40 {
        format!("{}…", s.chars().take(40).collect::<String>())
    } else {
        s.to_string()
    }
}

/// 检查一项颜色：写法认不出时记下提示并按缺失处理。
fn check(
    warnings: &mut Vec<ImportWarning>,
    scheme: &str,
    field: ColorField,
    value: Option<&str>,
) -> Option<String> {
    let value = value?;
    let color = parse_color(value);
    if color.is_none() {
        warnings.push(ImportWarning::InvalidColor {
            scheme: scheme.to_string(),
            field,
            value: short(value),
        });
    }
    color
}

/// 校验并补全一套配色；连前景、背景和大部分 ANSI 色都没有时不算配色，返回 `None`。
fn finalize(raw: Raw, fallback_name: &str, w: &mut Vec<ImportWarning>) -> Option<ColorScheme> {
    let name = clean_name(raw.name.as_deref()).unwrap_or_else(|| fallback_name.to_string());
    let mut ansi: Vec<Option<String>> = raw
        .ansi
        .iter()
        .enumerate()
        .map(|(i, v)| check(w, &name, ColorField::ansi(i), v.as_deref()))
        .collect();
    let fg = check(w, &name, ColorField::Foreground, raw.foreground.as_deref());
    let bg = check(w, &name, ColorField::Background, raw.background.as_deref());
    let found = ansi.iter().filter(|c| c.is_some()).count();
    if fg.is_none() && bg.is_none() && found < 4 {
        return None;
    }

    // 普通色缺失：先用对应的 bright 色，再用 xterm 原生色
    let mut missing_normal = 0;
    for i in 0..8 {
        if ansi[i].is_none() {
            let bright = ansi[i + 8].clone();
            ansi[i] = Some(bright.unwrap_or_else(|| FALLBACK_ANSI[i].to_string()));
            missing_normal += 1;
        }
    }
    let mut missing_bright = 0;
    for i in 8..16 {
        if ansi[i].is_none() {
            ansi[i] = ansi[i - 8].clone();
            missing_bright += 1;
        }
    }
    let ansi: Vec<String> = ansi.into_iter().map(Option::unwrap_or_default).collect();
    if missing_normal > 0 {
        w.push(ImportWarning::MissingAnsi {
            scheme: name.clone(),
            count: missing_normal,
        });
    }
    if missing_bright > 0 {
        w.push(ImportWarning::MissingBright {
            scheme: name.clone(),
            count: missing_bright,
        });
    }

    let background = match (&bg, &fg) {
        (Some(bg), _) => bg.clone(),
        (None, Some(fg)) => color::pick_readable(&[&ansi[0], &ansi[15], &ansi[7]], fg),
        (None, None) => ansi[0].clone(),
    };
    if bg.is_none() {
        w.push(ImportWarning::MissingBackground {
            scheme: name.clone(),
        });
    }
    let foreground = fg.unwrap_or_else(|| {
        w.push(ImportWarning::MissingForeground {
            scheme: name.clone(),
        });
        color::pick_readable(&[&ansi[7], &ansi[15], &ansi[0]], &background)
    });
    let cursor = check(w, &name, ColorField::Cursor, raw.cursor.as_deref()).unwrap_or_else(|| {
        w.push(ImportWarning::MissingCursor {
            scheme: name.clone(),
        });
        foreground.clone()
    });
    let selection = check(
        w,
        &name,
        ColorField::Selection,
        raw.selection_background.as_deref(),
    )
    .unwrap_or_else(|| {
        w.push(ImportWarning::MissingSelection {
            scheme: name.clone(),
        });
        color::blend(&foreground, &background, 0.3)
    });
    if let Some(space) = raw.color_space {
        w.push(ImportWarning::ColorSpace {
            scheme: name.clone(),
            space,
        });
    }
    let cursor_accent = check(
        w,
        &name,
        ColorField::CursorAccent,
        raw.cursor_accent.as_deref(),
    );
    let selection_foreground = check(
        w,
        &name,
        ColorField::SelectionForeground,
        raw.selection_foreground.as_deref(),
    );
    Some(ColorScheme {
        name,
        foreground,
        background,
        cursor,
        cursor_accent: cursor_accent.unwrap_or_default(),
        selection_background: selection,
        selection_foreground: selection_foreground.unwrap_or_default(),
        ansi,
        extra: Default::default(),
    })
}

/// 按内容识别的顺序：特征明确的在前。
struct Detector {
    exts: &'static [&'static str],
    run: fn(&str, &mut Ctx) -> Option<Detected>,
}

const DETECTORS: &[Detector] = &[
    Detector {
        exts: &["itermcolors", "plist"],
        run: formats::iterm,
    },
    Detector {
        exts: &["json"],
        run: formats::json,
    },
    Detector {
        exts: &["mxtcolors", "ini"],
        run: formats::mobaxterm,
    },
    Detector {
        exts: &["xcs"],
        run: formats::xshell,
    },
    Detector {
        exts: &["toml"],
        run: formats::alacritty_toml,
    },
    Detector {
        exts: &[],
        run: formats::ghostty,
    },
    Detector {
        exts: &["xresources", "xrdb", "xdefaults"],
        run: formats::xresources,
    },
    Detector {
        exts: &["conf"],
        run: formats::kitty,
    },
    Detector {
        exts: &["yaml", "yml"],
        run: formats::yaml,
    },
];

/// 文件名去掉目录后的主名与小写的扩展名；`.Xresources` 这类以点开头的文件名整个当扩展名。
fn base_name(file_name: &str) -> (&str, String) {
    let base = file_name.rsplit(['/', '\\']).next().unwrap_or("");
    match base.rfind('.') {
        Some(0) => ("", base[1..].to_lowercase()),
        Some(dot) => (&base[..dot], base[dot + 1..].to_lowercase()),
        None => (base, String::new()),
    }
}

fn too_large() -> ParseResult {
    ParseResult {
        warnings: vec![ImportWarning::TooLarge {
            max_bytes: MAX_INPUT_BYTES,
        }],
        ..ParseResult::default()
    }
}

/// 解析配色文件（或粘贴的文本）。自动识别格式，以内容为准，`file_name`（可以带目录）的扩展名只决定先试哪种格式，
/// 主名用作没有名称的配色的名称。认不出格式时 `format` 为 `None`、`schemes` 为空。
pub fn parse_schemes(text: &str, file_name: Option<&str>) -> ParseResult {
    if text.len() > MAX_INPUT_BYTES {
        return too_large();
    }
    // 去掉 BOM，统一换行
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let clean = text.replace("\r\n", "\n").replace('\r', "\n");
    let (stem, ext) = base_name(file_name.unwrap_or(""));
    let fallback_name = clean_name(Some(stem)).unwrap_or_else(|| FALLBACK_NAME.into());
    let first = DETECTORS.iter().filter(|d| d.exts.contains(&ext.as_str()));
    let rest = DETECTORS.iter().filter(|d| !d.exts.contains(&ext.as_str()));
    for detector in first.chain(rest) {
        let mut ctx = Ctx {
            fallback_name: fallback_name.clone(),
            warnings: Vec::new(),
        };
        if let Some(found) = (detector.run)(&clean, &mut ctx) {
            return finish(found, ctx);
        }
    }
    ParseResult::default()
}

/// 解析读到的配色文件：二进制 plist（iTerm2 另存的 `.itermcolors`）直接读取，其余按 [`decode_scheme_file`]
/// 解码后交给 [`parse_schemes`]。
pub fn parse_scheme_bytes(bytes: &[u8], file_name: Option<&str>) -> ParseResult {
    if bytes.len() > MAX_INPUT_BYTES {
        return too_large();
    }
    if bytes.starts_with(b"bplist") {
        let (stem, _) = base_name(file_name.unwrap_or(""));
        let mut ctx = Ctx {
            fallback_name: clean_name(Some(stem)).unwrap_or_else(|| FALLBACK_NAME.into()),
            warnings: Vec::new(),
        };
        let found = formats::iterm_binary(bytes, &mut ctx);
        return finish(found, ctx);
    }
    parse_schemes(&decode_scheme_file(bytes), file_name)
}

/// 校验、补全识别出的每一套配色；一个文件多套、没有名称时按文件名加序号。
fn finish(found: Detected, ctx: Ctx) -> ParseResult {
    let Ctx {
        fallback_name,
        mut warnings,
    } = ctx;
    let total = found.raws.len();
    let mut raws = found.raws;
    if raws.len() > MAX_SCHEMES {
        warnings.push(ImportWarning::TooMany { max: MAX_SCHEMES });
        raws.truncate(MAX_SCHEMES);
    }
    let count = raws.len();
    let mut schemes = Vec::new();
    let mut skipped = 0;
    for (i, raw) in raws.into_iter().enumerate() {
        let name = if count > 1 {
            format!("{fallback_name} {}", i + 1)
        } else {
            fallback_name.clone()
        };
        match finalize(raw, &name, &mut warnings) {
            Some(s) => schemes.push(s),
            None => skipped += 1,
        }
    }
    if skipped > 0 {
        warnings.push(ImportWarning::Skipped { count: skipped });
    }
    if schemes.is_empty() && total == 0 && warnings.is_empty() {
        warnings.push(ImportWarning::NoSchemes);
    }
    ParseResult {
        format: Some(found.format),
        schemes,
        warnings,
    }
}

#[cfg(test)]
mod tests;
