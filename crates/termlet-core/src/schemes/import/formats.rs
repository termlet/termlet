//! 各种配色文件格式：先用文本特征判断是不是这种格式，是的话取出原始的颜色值（由 `finalize` 统一校验、补全）。

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;

use super::text::{MAX_LINE, parse_ini, parse_toml_lite, strip_jsonc, strip_quote_chars};
use super::tree::{NodeId, Obj, Tree, parse_json};
use super::yaml::parse_yaml_lite;
use super::{Ctx, Detected, ImportWarning, MAX_SCHEMES, Raw, SchemeFormat, parse_color};
use crate::schemes::color::{Rgb, round_channel};

const ANSI_NAMES: [&str; 8] = [
    "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
];

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("配色格式的正则")
}

fn detected(format: SchemeFormat, raws: Vec<Raw>) -> Option<Detected> {
    Some(Detected { format, raws })
}

/// 按行切出"键 分隔符 值"（值去掉两端的空白），跳过超长行。
fn key_value_lines<'a>(text: &'a str, sep: &'a Regex) -> impl Iterator<Item = (&'a str, &'a str)> {
    text.split('\n')
        .filter(|line| line.len() <= MAX_LINE)
        .filter_map(move |line| {
            let caps = sep.captures(line)?;
            Some((caps.get(1)?.as_str(), caps.get(2)?.as_str().trim()))
        })
}

// ---- iTerm2 ----

static ITERM_KEY: LazyLock<Regex> =
    LazyLock::new(|| re(r"<key>\s*(?:Ansi [0-9]+ Color|Background Color|Foreground Color)"));
static ITERM_COMPONENT: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"<key>([^<]*)</key>\s*(?:<real>([^<]*)</real>|<integer>([^<]*)</integer>|<string>([^<]*)</string>)",
    )
});

/// `.itermcolors` 中的一项颜色。
struct ItermColor {
    /// 分量不全或不是数字时为 `None`
    hex: Option<String>,
    space: Option<String>,
}

/// JavaScript 的 `parseFloat`：取开头能解析的数字部分。
fn parse_float_prefix(s: &str) -> Option<f64> {
    let s = s.trim_start();
    let b = s.as_bytes();
    let mut i = 0;
    if matches!(b.first(), Some(b'+' | b'-')) {
        i += 1;
    }
    let digits = |i: &mut usize| {
        let start = *i;
        while b.get(*i).is_some_and(u8::is_ascii_digit) {
            *i += 1;
        }
        *i - start
    };
    let mut n = digits(&mut i);
    if b.get(i) == Some(&b'.') {
        let save = i;
        i += 1;
        let frac = digits(&mut i);
        if n == 0 && frac == 0 {
            i = save;
        }
        n += frac;
    }
    if n == 0 {
        return None;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        let save = i;
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if digits(&mut i) == 0 {
            i = save;
        }
    }
    s[..i].parse().ok()
}

/// 由 0–1 的分量得到颜色；缺少分量或不是有限的数时返回 `None`。
fn component_color(get: impl Fn(&str) -> Option<f64>) -> Option<String> {
    let part = |name: &str| {
        get(&format!("{name} Component"))
            .filter(|v| v.is_finite())
            .map(|v| round_channel(v.clamp(0.0, 1.0) * 255.0))
    };
    Some(Rgb::new(part("Red")?, part("Green")?, part("Blue")?).to_hex())
}

/// iTerm2 `.itermcolors`（XML plist）。按 `find` 线性扫描，不用跨越整个文本的正则。
pub(super) fn iterm(text: &str, ctx: &mut Ctx) -> Option<Detected> {
    if text.starts_with("bplist") {
        ctx.warnings.push(ImportWarning::BinaryPlist);
        return detected(SchemeFormat::Iterm2, Vec::new());
    }
    if !text.contains("<dict") || !ITERM_KEY.is_match(text) {
        return None;
    }
    let mut colors: HashMap<String, ItermColor> = HashMap::new();
    let mut pos = 0;
    while colors.len() < 1000 {
        let Some(k) = text
            .get(pos..)
            .and_then(|t| t.find("<key>"))
            .map(|i| pos + i)
        else {
            break;
        };
        let Some(ke) = text[k..].find("</key>").map(|i| k + i) else {
            break;
        };
        let key = text.get(k + 5..ke).unwrap_or("").trim().to_string();
        pos = ke + 6;
        let after = text[pos..].trim_start();
        let j = text.len() - after.len();
        if !after.starts_with("<dict>") {
            continue;
        }
        let Some(end) = text[j..].find("</dict>").map(|i| j + i) else {
            break;
        };
        let body = text.get(j + 6..end).unwrap_or("");
        pos = end + 7;
        let mut comps: HashMap<String, String> = HashMap::new();
        for caps in ITERM_COMPONENT.captures_iter(body) {
            let name = caps.get(1).map_or("", |m| m.as_str()).trim();
            let value = (2..=4)
                .find_map(|g| caps.get(g))
                .map_or("", |m| m.as_str())
                .trim();
            comps.insert(name.to_string(), value.to_string());
        }
        let hex = component_color(|k| comps.get(k).and_then(|v| parse_float_prefix(v)));
        colors.insert(
            key,
            ItermColor {
                hex,
                space: comps.get("Color Space").cloned(),
            },
        );
    }
    detected(SchemeFormat::Iterm2, iterm_variants(&colors, ctx))
}

/// 二进制 plist 格式的 `.itermcolors`（iTerm2 有时这样保存）。读不了时提示重新导出。
pub(super) fn iterm_binary(bytes: &[u8], ctx: &mut Ctx) -> Detected {
    let dict = plist::Value::from_reader(std::io::Cursor::new(bytes))
        .ok()
        .and_then(plist::Value::into_dictionary);
    let Some(dict) = dict else {
        ctx.warnings.push(ImportWarning::BinaryPlist);
        return Detected {
            format: SchemeFormat::Iterm2,
            raws: Vec::new(),
        };
    };
    let number = |v: &plist::Value| match v {
        plist::Value::Real(f) => Some(*f),
        plist::Value::Integer(i) => i.as_signed().map(|i| i as f64),
        plist::Value::String(s) => parse_float_prefix(s),
        _ => None,
    };
    let colors: HashMap<String, ItermColor> = dict
        .iter()
        .filter_map(|(key, value)| {
            let comps = value.as_dictionary()?;
            Some((
                key.trim().to_string(),
                ItermColor {
                    hex: component_color(|k| comps.get(k).and_then(number)),
                    space: comps
                        .get("Color Space")
                        .and_then(plist::Value::as_string)
                        .map(|s| s.trim().to_string()),
                },
            ))
        })
        .take(1000)
        .collect();
    Detected {
        format: SchemeFormat::Iterm2,
        raws: iterm_variants(&colors, ctx),
    }
}

/// iTerm2 3.5 起可以给浅色、深色模式分别设颜色（键名后缀 ` (Light)`、` (Dark)`）；没有普通键时分成两套。
fn iterm_variants(colors: &HashMap<String, ItermColor>, ctx: &Ctx) -> Vec<Raw> {
    let variants: Vec<&str> = ["", " (Dark)", " (Light)"]
        .into_iter()
        .filter(|v| {
            colors.contains_key(&format!("Ansi 0 Color{v}"))
                || colors.contains_key(&format!("Background Color{v}"))
        })
        .collect();
    let used = if variants.contains(&"") {
        vec![""]
    } else {
        variants
    };
    used.into_iter()
        .map(|suffix| {
            let mut raw = Raw {
                name: (!suffix.is_empty()).then(|| format!("{}{suffix}", ctx.fallback_name)),
                ..Raw::default()
            };
            let mut spaces: Vec<String> = Vec::new();
            let mut take = |key: &str| -> Option<String> {
                let c = colors.get(&format!("{key}{suffix}"))?;
                if let Some(space) = &c.space
                    && !space.eq_ignore_ascii_case("srgb")
                    && !spaces.contains(space)
                {
                    spaces.push(space.clone());
                }
                Some(c.hex.clone().unwrap_or_else(|| format!("({key})")))
            };
            for i in 0..16 {
                raw.ansi[i] = take(&format!("Ansi {i} Color"));
            }
            raw.background = take("Background Color");
            raw.foreground = take("Foreground Color");
            raw.cursor = take("Cursor Color");
            raw.cursor_accent = take("Cursor Text Color");
            raw.selection_background = take("Selection Color");
            raw.selection_foreground = take("Selected Text Color");
            if !spaces.is_empty() {
                raw.color_space = Some(spaces.join(", "));
            }
            raw
        })
        .collect()
}

// ---- JSON：Windows Terminal、Gogh、base16、Tabby、NyaTerm ----

/// 开头（跳过空白与 `#`、`//` 注释行）是 `[` 或 `{`。只看前 4096 字节。
fn starts_like_json(text: &str) -> bool {
    let mut end = text.len().min(4096);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut s = &text[..end];
    loop {
        s = s.trim_start();
        if s.starts_with('#') || s.starts_with("//") {
            match s.find('\n') {
                Some(i) => s = &s[i + 1..],
                None => return false,
            }
        } else {
            return s.starts_with('[') || s.starts_with('{');
        }
    }
}

/// Windows Terminal：单个配色、配色数组、`settings.json` 的 `schemes`；purple 对应 magenta。
fn wt_scheme(o: Obj<'_>) -> Raw {
    let mut raw = Raw {
        name: o.str("name"),
        foreground: o.str("foreground"),
        background: o.str("background"),
        cursor: o.str("cursorColor"),
        selection_background: o.str("selectionBackground"),
        ..Raw::default()
    };
    for (i, n) in ANSI_NAMES.into_iter().enumerate() {
        let magenta = n == "magenta";
        let wt = if magenta { "purple" } else { n };
        let cap = format!("{}{}", wt[..1].to_uppercase(), &wt[1..]);
        raw.ansi[i] = o
            .str(wt)
            .or_else(|| magenta.then(|| o.str("magenta")).flatten());
        raw.ansi[i + 8] = o
            .str(&format!("bright{cap}"))
            .or_else(|| magenta.then(|| o.str("brightMagenta")).flatten());
    }
    raw
}

const WT_KEYS: [&str; 5] = [
    "brightBlack",
    "purple",
    "brightPurple",
    "cursorColor",
    "selectionBackground",
];

fn looks_wt(o: &Obj<'_>) -> bool {
    (o.has("background") || o.has("foreground") || o.has("black"))
        && WT_KEYS.iter().any(|k| o.has(k))
}

/// Gogh：`color_01`..`color_16`、background、foreground、cursor。
fn gogh_scheme(o: Obj<'_>) -> Raw {
    let mut raw = Raw {
        name: o.str("name"),
        foreground: o.str("foreground"),
        background: o.str("background"),
        cursor: o.str("cursor"),
        ..Raw::default()
    };
    for i in 0..16 {
        raw.ansi[i] = o.str(&format!("color_{:02}", i + 1));
    }
    raw
}

/// base16 / base24 的调色板：新格式在 `palette` 下，旧格式直接在顶层。
fn base16_palette<'t>(o: &Obj<'t>) -> Obj<'t> {
    o.obj("palette").unwrap_or(*o)
}

fn has_base16(o: &Obj<'_>) -> bool {
    let pal = base16_palette(o);
    pal.has_ci("base00") && pal.has_ci("base05")
}

/// base16 / base24：按 base16-shell 的映射生成 16 色（base24 的 bright 色取 base12..base17）。值可以不带 `#`。
fn base16_scheme(o: Obj<'_>) -> Raw {
    let pal = base16_palette(&o);
    let b = |k: &str| pal.str_ci(&format!("base{k}"));
    let normal = ["00", "08", "0B", "0A", "0D", "0E", "0C", "05"];
    let bright = if b("12").is_some() {
        ["03", "12", "14", "13", "16", "17", "15", "07"]
    } else {
        ["03", "08", "0B", "0A", "0D", "0E", "0C", "07"]
    };
    let mut raw = Raw {
        name: o.str("name").or_else(|| o.str("scheme")),
        background: b("00"),
        foreground: b("05"),
        cursor: b("05"),
        selection_background: b("02"),
        ..Raw::default()
    };
    for (i, k) in normal.iter().chain(bright.iter()).enumerate() {
        raw.ansi[i] = b(k);
    }
    raw
}

/// NyaTerm 主题设计器导出的 `nyaterm-theme.json`（格式按 NyaTerm 提交 daedb58 的 `crates/nyaterm-core/src/themes/mod.rs`
/// 中的 `ThemeConfig`）：界面颜色在 `colors` 下，终端的颜色在 `colors.terminal` 下。也接受多个主题的数组
/// （NyaTerm 自带主题的写法）。
fn nyaterm_terminal<'t>(o: &Obj<'t>) -> Option<Obj<'t>> {
    let terminal = o.obj("colors")?.obj("terminal")?;
    (terminal.has("background") || terminal.has("black")).then_some(terminal)
}

/// 名称取 `name`（如 GitHub Dark），没有时取简称 `label`；终端的 `black`…`brightWhite`、`background`、`foreground`、
/// `cursor`、`selectionBackground`（`foregroundIntense`、`lineHighlight`、`findMatch*` 没有对应项，不用）。
fn nyaterm_scheme(o: Obj<'_>) -> Raw {
    let Some(t) = nyaterm_terminal(&o) else {
        return Raw::default();
    };
    let mut raw = Raw {
        name: o.str("name").or_else(|| o.str("label")),
        foreground: t.str("foreground"),
        background: t.str("background"),
        cursor: t.str("cursor"),
        selection_background: t.str("selectionBackground"),
        ..Raw::default()
    };
    for (i, n) in ANSI_NAMES.into_iter().enumerate() {
        let cap = format!("{}{}", n[..1].to_uppercase(), &n[1..]);
        raw.ansi[i] = t.str(n);
        raw.ansi[i + 8] = t.str(&format!("bright{cap}"));
    }
    raw
}

/// Tabby：配色对象为 `{ name, foreground, background, cursor, colors[16], selection, ... }`。
fn looks_tabby(o: &Obj<'_>) -> bool {
    o.seq("colors").is_some_and(|c| c.len() >= 8) && (o.has("foreground") || o.has("background"))
}

fn tabby_scheme(o: &Obj<'_>) -> Raw {
    let mut raw = Raw {
        name: o.str("name"),
        foreground: o.str("foreground"),
        background: o.str("background"),
        cursor: o.str("cursor"),
        cursor_accent: o.str("cursorAccent"),
        selection_background: o.str("selection"),
        selection_foreground: o.str("selectionForeground"),
        ..Raw::default()
    };
    let colors = o.seq("colors").unwrap_or_default();
    for (slot, &id) in raw.ansi.iter_mut().zip(colors) {
        *slot = o.tree.str(id);
    }
    raw
}

/// `customColorSchemes`、`colorScheme`、`lightColorScheme`、各 profile 的 `terminalColorScheme`；同名同色的只取一次。
/// 去重只比较标量：名称等字段可能是指向一大棵树的 YAML 别名。
fn tabby_schemes(tree: &Tree, root: NodeId) -> Vec<Raw> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for id in tree.find_objects(root, |o| looks_tabby(&o), MAX_SCHEMES + 1) {
        let Some(o) = tree.obj(id) else {
            continue;
        };
        let colors: Vec<Option<String>> = o
            .seq("colors")
            .unwrap_or_default()
            .iter()
            .take(16)
            .map(|&c| tree.str(c))
            .collect();
        let key = (
            o.str("name"),
            o.str("foreground"),
            o.str("background"),
            colors,
        );
        if seen.insert(key) {
            out.push(tabby_scheme(&o));
        }
    }
    out
}

/// Alacritty：TOML 与旧的 YAML 拍平后的路径相同。
fn alacritty_scheme(flat: &HashMap<String, String>) -> Option<Raw> {
    let get = |p: &str| {
        let v = flat.get(&format!("colors.{p}"))?;
        // CellForeground / CellBackground 表示跟随单元格颜色，不是具体颜色
        let cell =
            v.eq_ignore_ascii_case("cellforeground") || v.eq_ignore_ascii_case("cellbackground");
        (!cell).then(|| v.clone())
    };
    let mut raw = Raw {
        background: get("primary.background"),
        foreground: get("primary.foreground"),
        cursor: get("cursor.cursor"),
        cursor_accent: get("cursor.text"),
        selection_background: get("selection.background"),
        selection_foreground: get("selection.text"),
        ..Raw::default()
    };
    for (i, n) in ANSI_NAMES.into_iter().enumerate() {
        raw.ansi[i] = get(&format!("normal.{n}"));
        raw.ansi[i + 8] = get(&format!("bright.{n}"));
    }
    let any = raw.background.is_some()
        || raw.foreground.is_some()
        || raw.ansi.iter().any(Option::is_some);
    any.then_some(raw)
}

pub(super) fn json(text: &str, _: &mut Ctx) -> Option<Detected> {
    if !starts_like_json(text) {
        return None;
    }
    let (tree, root) = parse_json(&strip_jsonc(text))?;
    // settings.json 或 { "schemes": [...] }
    if let Some(list) = tree.obj(root).and_then(|o| o.seq("schemes")) {
        let raws = list
            .iter()
            .filter_map(|&id| tree.obj(id))
            .map(wt_scheme)
            .collect();
        return detected(SchemeFormat::WindowsTerminal, raws);
    }
    let items: Vec<NodeId> = match (tree.seq(root), tree.obj(root)) {
        (Some(items), _) => items.to_vec(),
        (None, Some(o)) => o.seq("themes").map_or_else(|| vec![root], <[_]>::to_vec),
        _ => Vec::new(),
    };
    let list: Vec<Obj<'_>> = items.into_iter().filter_map(|id| tree.obj(id)).collect();
    let head = list.first()?;
    if nyaterm_terminal(head).is_some() {
        let raws = list
            .into_iter()
            .filter(|o| nyaterm_terminal(o).is_some())
            .map(nyaterm_scheme)
            .collect();
        return detected(SchemeFormat::NyaTerm, raws);
    }
    if head.has("color_01") {
        return detected(
            SchemeFormat::Gogh,
            list.into_iter().map(gogh_scheme).collect(),
        );
    }
    if has_base16(head) {
        let raws = list
            .into_iter()
            .filter(has_base16)
            .map(base16_scheme)
            .collect();
        return detected(SchemeFormat::Base16, raws);
    }
    if looks_wt(head) {
        let raws = list.into_iter().filter(looks_wt).map(wt_scheme).collect();
        return detected(SchemeFormat::WindowsTerminal, raws);
    }
    let tabby = tabby_schemes(&tree, root);
    if !tabby.is_empty() {
        return detected(SchemeFormat::Tabby, tabby);
    }
    None
}

// ---- MobaXterm ----

static MOBA_GATE: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?im)^[ \t]*(?:ForegroundColour|BackgroundColour|BoldBlack)[ \t]*="));

const MOBA_KEYS: [&str; 4] = [
    "foregroundcolour",
    "backgroundcolour",
    "boldblack",
    "cursorcolour",
];

/// `[Colors]` 段；没有时为第一个含 MobaXterm 颜色键的段。
fn moba_colors_section<'s, 'a>(
    sections: &'s [super::text::IniSection<'a>],
) -> Option<&'s super::text::IniSection<'a>> {
    sections
        .iter()
        .find(|s| s.name.eq_ignore_ascii_case("colors"))
        .or_else(|| {
            sections
                .iter()
                .find(|s| MOBA_KEYS.iter().any(|k| s.entries.contains_key(*k)))
        })
}

/// MobaXterm：`.mxtcolors` 与 `MobaXterm.ini` 的 `[Colors]` 段，颜色是十进制 `r,g,b`。
pub(super) fn mobaxterm(text: &str, _: &mut Ctx) -> Option<Detected> {
    if !MOBA_GATE.is_match(text) {
        return None;
    }
    let sections = parse_ini(text);
    let sec = moba_colors_section(&sections)?;
    let e = &sec.entries;
    let get = |k: &str| e.get(k).map(|v| v.to_string());
    let whole_ini = sec.name.eq_ignore_ascii_case("colors") && sections.len() > 2;
    let mut raw = Raw {
        name: whole_ini.then(|| "MobaXterm".to_string()),
        foreground: get("foregroundcolour"),
        background: get("backgroundcolour"),
        cursor: get("cursorcolour"),
        ..Raw::default()
    };
    for (i, n) in ANSI_NAMES.into_iter().enumerate() {
        raw.ansi[i] = get(n);
        raw.ansi[i + 8] = get(&format!("bold{n}"));
    }
    detected(SchemeFormat::MobaXterm, vec![raw])
}

/// MobaXterm 自带的 9 套配色（`MobaXterm.ini` 的 `[ColorThemes]`，24.3 版）：每套依次是 Black、BoldBlack、Red、
/// BoldRed……White、BoldWhite，然后是前景、背景、光标。
const MOBAXTERM_THEMES: [&str; 9] = [
    // 深色背景/浅色文字（默认）
    "1E1E1E,808080,FF6060,FF8080,0EB16C,18ED93,BEBE12,DEDC12,1296BE,1BBAE9,FF4DFF,FF7DFF,54CCEF,8EDDF4,CCCCCC,CCCCCC,ECECEC,1E1E1E,B4B4C0",
    // 浅色背景/深色文字
    "000000,444444,800000,AA0000,008000,00AA00,808000,AAAA00,000080,0000AA,800080,AA00AA,008080,00AAAA,808080,AAAAAA,444444,FFFFFF,AAAAAA",
    // 日晒深色调、日晒浅色调
    "073642,808080,DC322F,CB4B16,859900,586E75,B58900,657B83,268BD2,839496,D33682,6C71C4,2AA198,93A1A1,EEE8D5,FDF6E3,EEE8D5,002B36,859900",
    "073642,002B36,DC322F,CB4B16,859900,586E75,B58900,657B83,268BD2,839496,D33682,6C71C4,2AA198,93A1A1,EEE8D5,FDF6E3,586E75,FDF6E3,EEE8D5",
    // 柔和粉彩色、明亮色、Monokai、黑色余晖、黑白灰
    "0C0C0C,808080,D78787,DF8787,AFD787,AFDF87,F7F7AF,FFFFAF,87AFD7,87AFDF,D7AFD7,DFAFDF,AFD7D7,AFDFDF,E6E6E6,EEEEEE,D0D0D0,1C1C1C,FFAF00",
    "000000,808080,FF6060,FF8080,60FF60,80FF80,FFFF36,FFFF80,3B78FF,72A1FF,FF36FF,FF82FF,36FFFF,82FFFF,ECECEC,FFFFFF,EDECEC,010000,B6B4C0",
    "000000,808080,F92672,DD4278,A6E22E,9DC54B,E6DB74,D1CA89,66D9EF,7ACCDA,AE81FF,B392EF,00D9D9,C8F0F0,F5DEB3,FFFFFF,F8F8F2,272822,FF8000",
    "000000,808080,AA4244,CC7B7D,7E8D53,A5B17C,E4B46D,ECC995,6E9ABA,96B6CD,9E5085,C083AC,80D5CF,A9E2DE,CCCCCC,CCCCCC,ECECEC,242424,B4B4C0",
    "000000,808080,FFFFFF,FFFFFF,FFFFFF,FFFFFF,FFFFFF,FFFFFF,FFFFFF,FFFFFF,FFFFFF,FFFFFF,FFFFFF,FFFFFF,FFFFFF,FFFFFF,FFFFFF,000000,B4B4C0",
];

/// 与 [`MOBAXTERM_THEMES`] 中各项对应的 `[Colors]` 键（小写）。
const MOBAXTERM_THEME_KEYS: [&str; 19] = [
    "black",
    "boldblack",
    "red",
    "boldred",
    "green",
    "boldgreen",
    "yellow",
    "boldyellow",
    "blue",
    "boldblue",
    "magenta",
    "boldmagenta",
    "cyan",
    "boldcyan",
    "white",
    "boldwhite",
    "foregroundcolour",
    "backgroundcolour",
    "cursorcolour",
];

/// MobaXterm 的 `[Colors]` 是不是自带的某一套：段中出现的颜色都与这一套相同（`MobaXterm.ini` 总有这一段，
/// 是自带的配色时不算用户自己的）。
pub(in crate::schemes) fn is_mobaxterm_builtin(text: &str) -> bool {
    let sections = parse_ini(text);
    let Some(sec) = moba_colors_section(&sections) else {
        return false;
    };
    MOBAXTERM_THEMES.iter().any(|theme| {
        let colors: Vec<String> = theme
            .split(',')
            .map(|h| format!("#{}", h.to_ascii_lowercase()))
            .collect();
        let mut any = false;
        for (key, color) in MOBAXTERM_THEME_KEYS.iter().zip(&colors) {
            let Some(v) = sec.entries.get(*key) else {
                continue;
            };
            if parse_color(v).as_ref() != Some(color) {
                return false;
            }
            any = true;
        }
        any
    })
}

// ---- Xshell ----

static XSHELL_BOLD: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?im)^[ \t]*[a-z0-9_]+\(bold\)[ \t]*="));
static XSHELL_TEXT: LazyLock<Regex> = LazyLock::new(|| re(r"(?im)^[ \t]*text[ \t]*="));

/// Xshell `.xcs`：每段一套配色，`(bold)` 对应 bright，颜色是不带 `#` 的十六进制。
pub(super) fn xshell(text: &str, _: &mut Ctx) -> Option<Detected> {
    if !XSHELL_BOLD.is_match(text) && !XSHELL_TEXT.is_match(text) {
        return None;
    }
    let mut raws = Vec::new();
    for sec in parse_ini(text) {
        if sec.name.is_empty() || sec.name.eq_ignore_ascii_case("names") {
            continue;
        }
        let e = &sec.entries;
        if !e.contains_key("text") && !e.contains_key("background") && !e.contains_key("black") {
            continue;
        }
        let get = |k: &str| e.get(k).map(|v| v.to_string());
        let mut raw = Raw {
            name: Some(sec.name.to_string()),
            foreground: get("text"),
            background: get("background"),
            ..Raw::default()
        };
        for (i, n) in ANSI_NAMES.into_iter().enumerate() {
            raw.ansi[i] = get(n);
            raw.ansi[i + 8] = get(&format!("{n}(bold)"));
        }
        raws.push(raw);
        if raws.len() > MAX_SCHEMES {
            break;
        }
    }
    (!raws.is_empty()).then_some(Detected {
        format: SchemeFormat::Xshell,
        raws,
    })
}

// ---- Alacritty TOML ----

static TOML_TABLE: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?m)^[ \t]*\[colors(?:\.[a-z_]+)?\][ \t]*$"));
static TOML_DOTTED: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)^[ \t]*colors\.[a-z_]+"));

pub(super) fn alacritty_toml(text: &str, _: &mut Ctx) -> Option<Detected> {
    if !TOML_TABLE.is_match(text) && !TOML_DOTTED.is_match(text) {
        return None;
    }
    let raw = alacritty_scheme(&parse_toml_lite(text))?;
    detected(SchemeFormat::Alacritty, vec![raw])
}

// ---- Ghostty ----

static GHOSTTY_PALETTE: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?m)^[ \t]*palette[ \t]*=[ \t]*[0-9]+[ \t]*="));
static GHOSTTY_KEYS: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?m)^[ \t]*(?:cursor-color|selection-background|selection-foreground|cursor-text)[ \t]*=")
});
static GHOSTTY_LINE: LazyLock<Regex> =
    LazyLock::new(|| re(r"^[ \t]*([a-z][a-z0-9-]*)[ \t]*=(.*)$"));
static GHOSTTY_ENTRY: LazyLock<Regex> = LazyLock::new(|| re(r"^([0-9]+)[ \t]*=([\s\S]+)$"));

/// Ghostty：`palette = N=颜色`、background、foreground、cursor-color 等。
pub(super) fn ghostty(text: &str, _: &mut Ctx) -> Option<Detected> {
    if !GHOSTTY_PALETTE.is_match(text) && !GHOSTTY_KEYS.is_match(text) {
        return None;
    }
    let mut raw = Raw::default();
    for (k, v) in key_value_lines(text, &GHOSTTY_LINE) {
        let value = strip_quote_chars(v);
        let owned = Some(value.to_string());
        match k {
            "palette" => {
                if let Some(caps) = GHOSTTY_ENTRY.captures(value)
                    && let Ok(i) = caps[1].parse::<usize>()
                    && i < 16
                {
                    raw.ansi[i] = Some(caps[2].trim().to_string());
                }
            }
            "background" => raw.background = owned,
            "foreground" => raw.foreground = owned,
            "cursor-color" => raw.cursor = owned,
            "cursor-text" => raw.cursor_accent = owned,
            "selection-background" => raw.selection_background = owned,
            "selection-foreground" => raw.selection_foreground = owned,
            _ => {}
        }
    }
    detected(SchemeFormat::Ghostty, vec![raw])
}

// ---- Xresources ----

static XRES_GATE: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?m)^[ \t]*[A-Za-z0-9_.*-]*[.*](?:color[0-9]{1,3}|foreground|background|cursorColor)[ \t]*:",
    )
});
static XRDB_GATE: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?m)^[ \t]*#define[ \t]+(?:Ansi_[0-9]+_Color|Background_Color|Foreground_Color)(?-u:\b)")
});
static XRES_DEFINE: LazyLock<Regex> = LazyLock::new(|| re(r"^#define[ \t]+(\S+)[ \t]+(\S+)"));
static XRES_LINE: LazyLock<Regex> = LazyLock::new(|| re(r"^([A-Za-z0-9_.*-]+)[ \t]*:([\s\S]*)$"));

/// Xresources：`*.color0: #xxxxxx`、`URxvt*foreground: ...` 等；支持 `#define` 宏。
/// iTerm2-Color-Schemes 的 xrdb 导出只有 `#define Ansi_0_Color` 这类宏，也直接识别。
/// Tabby 社区配色包的文件就是这种 Xresources 写法。
pub(super) fn xresources(text: &str, _: &mut Ctx) -> Option<Detected> {
    if !XRES_GATE.is_match(text) && !XRDB_GATE.is_match(text) {
        return None;
    }
    let mut defines: HashMap<&str, &str> = HashMap::new();
    let mut res: HashMap<String, String> = HashMap::new();
    for line in text.split('\n').filter(|l| l.len() <= MAX_LINE) {
        let t = line.trim();
        if t.is_empty() || t.starts_with('!') {
            continue;
        }
        if let Some(caps) = XRES_DEFINE.captures(t) {
            if let (Some(name), Some(value)) = (caps.get(1), caps.get(2))
                && defines.len() < 1000
            {
                defines.insert(name.as_str(), value.as_str());
            }
            continue;
        }
        let Some(caps) = XRES_LINE.captures(t) else {
            continue;
        };
        let (Some(key), Some(value)) = (caps.get(1), caps.get(2)) else {
            continue;
        };
        let name = key
            .as_str()
            .rsplit(['.', '*'])
            .next()
            .unwrap_or("")
            .to_lowercase();
        if name.is_empty() {
            continue;
        }
        let v = value.as_str().trim();
        res.insert(name, defines.get(v).copied().unwrap_or(v).to_string());
    }
    let mut raw = Raw::default();
    if !res.is_empty() {
        let get = |k: &str| res.get(k).cloned();
        for i in 0..16 {
            raw.ansi[i] = get(&format!("color{i}"));
        }
        raw.foreground = get("foreground");
        raw.background = get("background");
        raw.cursor = get("cursorcolor");
        raw.selection_background = get("highlightcolor");
        raw.selection_foreground = get("highlighttextcolor");
    } else {
        let get = |k: &str| defines.get(k).map(|v| v.to_string());
        for i in 0..16 {
            raw.ansi[i] = get(&format!("Ansi_{i}_Color"));
        }
        raw.foreground = get("Foreground_Color");
        raw.background = get("Background_Color");
        raw.cursor = get("Cursor_Color");
        raw.cursor_accent = get("Cursor_Text_Color");
        raw.selection_background = get("Selection_Color");
        raw.selection_foreground = get("Selected_Text_Color");
    }
    detected(SchemeFormat::Xresources, vec![raw])
}

// ---- Kitty ----

static KITTY_COLOR: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?m)^[ \t]*color(?:1[0-5]|[0-9])[ \t]+[#0-9a-zA-Z]"));
static KITTY_KEYS: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?m)^[ \t]*(?:foreground|background|selection_background)[ \t]+#[0-9a-fA-F]{3,6}(?-u:\b)")
});
static KITTY_NAME: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)^##[ \t]*name:[ \t]*(.+)$"));
static KITTY_LINE: LazyLock<Regex> =
    LazyLock::new(|| re(r"^[ \t]*([a-z_0-9]+)[ \t]+([^=:\s][^\n]*)$"));

/// Kitty：`color0 #xxxxxx`、foreground、selection_background 等，键与值之间是空白。
pub(super) fn kitty(text: &str, _: &mut Ctx) -> Option<Detected> {
    if !KITTY_COLOR.is_match(text) && !KITTY_KEYS.is_match(text) {
        return None;
    }
    let mut raw = Raw {
        name: KITTY_NAME
            .captures(text)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string()),
        ..Raw::default()
    };
    for (k, v) in key_value_lines(text, &KITTY_LINE) {
        let value = v.split_whitespace().next().unwrap_or("");
        // none、background 等不是具体颜色
        if ["none", "background", "foreground"]
            .iter()
            .any(|w| value.eq_ignore_ascii_case(w))
        {
            continue;
        }
        let owned = Some(value.to_string());
        if let Some(n) = k
            .strip_prefix("color")
            .filter(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
        {
            if let Ok(n) = n.parse::<usize>()
                && n < 16
            {
                raw.ansi[n] = owned;
            }
            continue;
        }
        match k {
            "foreground" => raw.foreground = owned,
            "background" => raw.background = owned,
            "cursor" => raw.cursor = owned,
            "cursor_text_color" => raw.cursor_accent = owned,
            "selection_background" => raw.selection_background = owned,
            "selection_foreground" => raw.selection_foreground = owned,
            _ => {}
        }
    }
    detected(SchemeFormat::Kitty, vec![raw])
}

// ---- YAML：base16 / base24、Gogh、Tabby 的 config.yaml、旧版 Alacritty ----

static YAML_GATE: LazyLock<Regex> =
    LazyLock::new(|| re(r#"(?m)^[ \t]*(?:- )?["']?[A-Za-z0-9_-][^:\n]*:"#));

pub(super) fn yaml(text: &str, _: &mut Ctx) -> Option<Detected> {
    if !YAML_GATE.is_match(text) {
        return None;
    }
    let (tree, root) = parse_yaml_lite(text)?;
    let items = tree.seq(root).map_or_else(|| vec![root], <[_]>::to_vec);
    let list: Vec<Obj<'_>> = items.into_iter().filter_map(|id| tree.obj(id)).collect();
    let head = list.first().copied();
    if let Some(head) = head {
        if has_base16(&head) {
            let raws = list
                .into_iter()
                .filter(has_base16)
                .map(base16_scheme)
                .collect();
            return detected(SchemeFormat::Base16, raws);
        }
        if head.has("color_01") {
            return detected(
                SchemeFormat::Gogh,
                list.into_iter().map(gogh_scheme).collect(),
            );
        }
    }
    let tabby = tabby_schemes(&tree, root);
    if !tabby.is_empty() {
        return detected(SchemeFormat::Tabby, tabby);
    }
    if let Some(head) = head
        && head.obj("colors").is_some()
        && let Some(raw) = alacritty_scheme(&tree.flatten(head.id))
    {
        return detected(SchemeFormat::Alacritty, vec![raw]);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_prefix_like_parse_float() {
        assert_eq!(parse_float_prefix("0.5"), Some(0.5));
        assert_eq!(parse_float_prefix(" .25x"), Some(0.25));
        assert_eq!(parse_float_prefix("1e-1"), Some(0.1));
        assert_eq!(parse_float_prefix("2e"), Some(2.0));
        assert_eq!(parse_float_prefix("-1"), Some(-1.0));
        assert_eq!(parse_float_prefix("inf"), None);
        assert_eq!(parse_float_prefix("."), None);
        assert_eq!(parse_float_prefix(""), None);
    }

    #[test]
    fn json_gate() {
        assert!(starts_like_json("  \n# c\n// d\n ["));
        assert!(!starts_like_json("# 没有换行 {"));
        assert!(!starts_like_json("a: {"));
        let long = format!("{}{{", " ".repeat(5000));
        assert!(!starts_like_json(&long));
        assert!(!starts_like_json(&"配".repeat(2000)));
    }
}
