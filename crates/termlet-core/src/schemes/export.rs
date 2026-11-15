//! 导出终端配色：Windows Terminal 的 `schemes` 数组（JSON）与 iTerm2 的 `.itermcolors`（XML plist）。
//!
//! 两种格式都能被 [`parse_schemes`](super::parse_schemes) 读回（单元测试做往返检查）。
//! Windows Terminal 没有光标文字色、选区文字色，导出时这两项会丢失。

use serde::Serialize;

use super::color::Rgb;
use crate::model::ColorScheme;

/// Windows Terminal 的一套配色（字段按 Windows Terminal 自己保存的顺序）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WtScheme {
    name: String,
    foreground: String,
    background: String,
    cursor_color: String,
    selection_background: String,
    black: String,
    red: String,
    green: String,
    yellow: String,
    blue: String,
    purple: String,
    cyan: String,
    white: String,
    bright_black: String,
    bright_red: String,
    bright_green: String,
    bright_yellow: String,
    bright_blue: String,
    bright_purple: String,
    bright_cyan: String,
    bright_white: String,
}

/// 大写的 `#RRGGBB`，与 Windows Terminal 自己保存的写法一致；缺少的颜色写黑色。
fn up(c: Option<&String>) -> String {
    c.map_or("#000000", String::as_str).to_uppercase()
}

/// 导出为 Windows Terminal 的配色数组：把其中的对象粘贴进 `settings.json` 的 `"schemes": [ ... ]` 即可。
pub fn to_windows_terminal_json<'a>(schemes: impl IntoIterator<Item = &'a ColorScheme>) -> String {
    let list: Vec<WtScheme> = schemes
        .into_iter()
        .map(|s| {
            let a = |i: usize| up(s.ansi.get(i));
            WtScheme {
                name: s.name.clone(),
                foreground: up(Some(&s.foreground)),
                background: up(Some(&s.background)),
                cursor_color: up(Some(&s.cursor)),
                selection_background: up(Some(&s.selection_background)),
                black: a(0),
                red: a(1),
                green: a(2),
                yellow: a(3),
                blue: a(4),
                purple: a(5),
                cyan: a(6),
                white: a(7),
                bright_black: a(8),
                bright_red: a(9),
                bright_green: a(10),
                bright_yellow: a(11),
                bright_blue: a(12),
                bright_purple: a(13),
                bright_cyan: a(14),
                bright_white: a(15),
            }
        })
        .collect();
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"    ");
    let mut ser = serde_json::Serializer::with_formatter(&mut out, formatter);
    // 只有字符串字段，序列化不会失败
    if list.serialize(&mut ser).is_err() {
        return "[]\n".into();
    }
    let mut text = String::from_utf8(out).unwrap_or_default();
    text.push('\n');
    text
}

/// 0–255 换成 iTerm2 的 0–1 浮点分量，位数足够读回时得到同一个整数。
fn component(v: u8) -> String {
    let x = (f64::from(v) / 255.0 * 1e10).round() / 1e10;
    x.to_string()
}

fn color_dict(key: &str, hex: &str) -> String {
    let c = Rgb::parse(hex).unwrap_or(Rgb::BLACK);
    [
        format!("\t<key>{key}</key>"),
        "\t<dict>".into(),
        "\t\t<key>Alpha Component</key>".into(),
        "\t\t<real>1</real>".into(),
        "\t\t<key>Blue Component</key>".into(),
        format!("\t\t<real>{}</real>", component(c.b)),
        "\t\t<key>Color Space</key>".into(),
        "\t\t<string>sRGB</string>".into(),
        "\t\t<key>Green Component</key>".into(),
        format!("\t\t<real>{}</real>", component(c.g)),
        "\t\t<key>Red Component</key>".into(),
        format!("\t\t<real>{}</real>", component(c.r)),
        "\t</dict>".into(),
    ]
    .join("\n")
}

/// 导出为 iTerm2 的 `.itermcolors`。文件格式本身不存名称，名称写在开头的注释里，导入时取文件名。
pub fn to_iterm_colors(scheme: &ColorScheme) -> String {
    let ansi_keys: Vec<String> = (0..16).map(|i| format!("Ansi {i} Color")).collect();
    let mut entries: Vec<(&str, &str)> = ansi_keys
        .iter()
        .zip(&scheme.ansi)
        .map(|(k, c)| (k.as_str(), c.as_str()))
        .collect();
    entries.extend([
        ("Background Color", scheme.background.as_str()),
        ("Foreground Color", scheme.foreground.as_str()),
        ("Bold Color", scheme.foreground.as_str()),
        ("Cursor Color", scheme.cursor.as_str()),
        ("Cursor Text Color", scheme.cursor_accent.as_str()),
        ("Selection Color", scheme.selection_background.as_str()),
        ("Selected Text Color", scheme.selection_foreground.as_str()),
    ]);
    // XML 注释中不能出现 --，也去掉尖括号、& 与控制字符
    let mut name = String::with_capacity(scheme.name.len());
    for c in scheme.name.chars() {
        if (c == '-' && name.ends_with('-')) || matches!(c, '<' | '>' | '&') || c.is_control() {
            continue;
        }
        name.push(c);
    }
    let mut lines = vec![
        r#"<?xml version="1.0" encoding="UTF-8"?>"#.to_string(),
        format!("<!-- {name} (exported by Termlet) -->"),
        r#"<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">"#.to_string(),
        r#"<plist version="1.0">"#.to_string(),
        "<dict>".to_string(),
    ];
    lines.extend(
        entries
            .into_iter()
            .filter(|(_, hex)| !hex.is_empty())
            .map(|(key, hex)| color_dict(key, hex)),
    );
    lines.extend(["</dict>".to_string(), "</plist>".to_string(), String::new()]);
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::schemes::{SchemeFormat, builtin_scheme, builtin_schemes, parse_schemes};

    /// Windows Terminal 没有光标文字色与选区文字色。
    fn without_wt_extras(s: &ColorScheme) -> ColorScheme {
        ColorScheme {
            cursor_accent: String::new(),
            selection_foreground: String::new(),
            ..s.clone()
        }
    }

    #[test]
    fn windows_terminal_json_layout() {
        let campbell = builtin_scheme("campbell").unwrap().scheme();
        let text = to_windows_terminal_json([campbell]);
        // 与 JSON.stringify(list, null, 4) 相同的排版，字段顺序固定
        assert!(text.starts_with(
            "[\n    {\n        \"name\": \"Campbell\",\n        \"foreground\": \"#CCCCCC\",\n"
        ));
        assert!(text.ends_with("    }\n]\n"));
        let list: Value = serde_json::from_str(&text).unwrap();
        let expected = serde_json::json!([{
            "name": "Campbell",
            "foreground": "#CCCCCC",
            "background": "#0C0C0C",
            "cursorColor": "#FFFFFF",
            "selectionBackground": "#464646",
            "black": "#0C0C0C",
            "red": "#C50F1F",
            "green": "#13A10E",
            "yellow": "#C19C00",
            "blue": "#0037DA",
            "purple": "#881798",
            "cyan": "#3A96DD",
            "white": "#CCCCCC",
            "brightBlack": "#767676",
            "brightRed": "#E74856",
            "brightGreen": "#16C60C",
            "brightYellow": "#F9F1A5",
            "brightBlue": "#3B78FF",
            "brightPurple": "#B4009E",
            "brightCyan": "#61D6D6",
            "brightWhite": "#F2F2F2",
        }]);
        assert_eq!(list, expected);
        assert_eq!(to_windows_terminal_json([]), "[]\n");
    }

    #[test]
    fn windows_terminal_round_trip() {
        let all: Vec<&ColorScheme> = builtin_schemes().iter().map(|b| b.scheme()).collect();
        let r = parse_schemes(
            &to_windows_terminal_json(all.iter().copied()),
            Some("export.json"),
        );
        assert_eq!(r.format, Some(SchemeFormat::WindowsTerminal));
        assert_eq!(r.warnings, []);
        let expected: Vec<ColorScheme> = all.iter().map(|s| without_wt_extras(s)).collect();
        assert_eq!(r.schemes, expected);
    }

    #[test]
    fn windows_terminal_pastes_into_settings() {
        let two: Vec<&ColorScheme> = builtin_schemes()[..2].iter().map(|b| b.scheme()).collect();
        let exported = to_windows_terminal_json(two);
        let exported = exported.trim();
        let settings = format!(
            "{{\n    \"profiles\": {{ \"list\": [] }},\n    \"schemes\": [ {} ]\n}}",
            &exported[1..exported.len() - 1]
        );
        let r = parse_schemes(&settings, Some("settings.json"));
        let names: Vec<&str> = r.schemes.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Termlet Light", "Termlet Dark"]);
    }

    #[test]
    fn iterm_round_trip() {
        // 名称取文件名
        for b in builtin_schemes() {
            let s = b.scheme();
            let r = parse_schemes(
                &to_iterm_colors(s),
                Some(&format!("{}.itermcolors", s.name)),
            );
            assert_eq!(r.format, Some(SchemeFormat::Iterm2), "{}", b.id());
            assert_eq!(r.warnings, [], "{}", b.id());
            assert_eq!(r.schemes, std::slice::from_ref(s), "{}", b.id());
        }
    }

    #[test]
    fn iterm_keeps_cursor_text_and_selected_text() {
        let s = ColorScheme {
            cursor_accent: "#010203".into(),
            selection_foreground: "#fefdfc".into(),
            ..builtin_schemes()[0].scheme().clone()
        };
        let text = to_iterm_colors(&s);
        assert!(text.contains("<key>Cursor Text Color</key>"));
        assert!(text.contains("<key>Selected Text Color</key>"));
        let parsed = &parse_schemes(&text, None).schemes[0];
        assert_eq!(parsed.cursor_accent, "#010203");
        assert_eq!(parsed.selection_foreground, "#fefdfc");
        // 没有这两项时不写
        let plain = to_iterm_colors(builtin_scheme("dracula").unwrap().scheme());
        assert!(!plain.contains("Cursor Text Color"));
        assert!(!plain.contains("Selected Text Color"));
    }

    #[test]
    fn iterm_name_cannot_break_the_comment() {
        let s = ColorScheme {
            name: "a -- b <c> & d".into(),
            ..builtin_schemes()[1].scheme().clone()
        };
        let text = to_iterm_colors(&s);
        assert!(text.contains("<!-- a - b c  d (exported by Termlet) -->"));
        assert_eq!(parse_schemes(&text, None).schemes.len(), 1);
        // 去掉尖括号后相邻的短横线也不会连成 --
        let s = ColorScheme {
            name: "x-<->-y\u{7}".into(),
            ..s
        };
        assert!(to_iterm_colors(&s).contains("<!-- x-y (exported by Termlet) -->"));
    }
}
