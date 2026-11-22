//! 内置配色、查找与选取、派生的颜色、自定义配色与许可说明的测试。

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use super::*;
use crate::id::SchemeId;
use crate::model::TerminalSettings;

/// 测试用的自定义配色。
fn custom() -> ColorScheme {
    ColorScheme {
        name: "我的".into(),
        foreground: "#eeeeee".into(),
        background: "#101010".into(),
        cursor: "#ffffff".into(),
        selection_background: "#333333".into(),
        ansi: [
            "#000000", "#cc0000", "#00cc00", "#cccc00", "#0000cc", "#cc00cc", "#00cccc", "#cccccc",
            "#555555", "#ff5555", "#55ff55", "#ffff55", "#5555ff", "#ff55ff", "#55ffff", "#ffffff",
        ]
        .map(String::from)
        .to_vec(),
        ..ColorScheme::default()
    }
}

fn ansi(fill: &str) -> Vec<String> {
    vec![fill.to_string(); 16]
}

fn builtin(id: &str) -> &'static ColorScheme {
    builtin_scheme(id).unwrap().scheme()
}

fn is_lower_hex(c: &str) -> bool {
    c.len() == 7
        && c.starts_with('#')
        && c[1..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

// ---- 内置配色 ----

#[test]
fn builtin_ids_are_the_released_ones() {
    // ID 发布后不能改：设置、主机、分组中按 ID 引用
    let ids: Vec<&str> = builtin_schemes().iter().map(BuiltinScheme::id).collect();
    assert_eq!(
        ids,
        [
            "termlet-light",
            "termlet-dark",
            "one-half-dark",
            "one-dark-pro",
            "solarized-light",
            "solarized-dark",
            "github-light",
            "github-dark",
            "catppuccin-latte",
            "catppuccin-frappe",
            "catppuccin-macchiato",
            "catppuccin-mocha",
            "tokyo-night-day",
            "tokyo-night",
            "gruvbox-light",
            "gruvbox-dark",
            "rose-pine-dawn",
            "rose-pine-moon",
            "rose-pine",
            "tomorrow-night",
            "everforest-light",
            "everforest-dark",
            "flexoki-light",
            "flexoki-dark",
            "ayu-light",
            "ayu-dark",
            "vscode-light-2026",
            "vscode-dark-2026",
            "dayfox",
            "nightfox",
            "kanagawa-lotus",
            "kanagawa-wave",
            "campbell",
            "xterm-classic",
            "dracula",
            "nord",
            "high-contrast-light",
            "high-contrast-dark",
        ]
    );
}

#[test]
fn builtin_ids_and_names_unique() {
    let list = builtin_schemes();
    assert!((30..=40).contains(&list.len()));
    let ids: HashSet<&str> = list.iter().map(BuiltinScheme::id).collect();
    assert_eq!(ids.len(), list.len());
    for id in &ids {
        // 小写字母、数字与短横线
        assert!(
            id.split('-').all(|p| !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())),
            "{id}"
        );
        // 自定义配色的 ID 前缀留给存储
        assert!(!id.starts_with(SchemeId::PREFIX), "{id}");
    }
    let names: HashSet<&str> = list.iter().map(BuiltinScheme::name).collect();
    assert_eq!(names.len(), list.len());
}

#[test]
fn builtin_colors_are_valid() {
    for b in builtin_schemes() {
        let s = b.scheme();
        assert_eq!(s.ansi.len(), 16, "{}", b.id());
        let mut colors = vec![
            &s.foreground,
            &s.background,
            &s.cursor,
            &s.selection_background,
        ];
        colors.extend(s.ansi.iter());
        for optional in [&s.cursor_accent, &s.selection_foreground] {
            if !optional.is_empty() {
                colors.push(optional);
            }
        }
        for c in colors {
            assert!(is_lower_hex(c), "{} {c}", b.id());
        }
        // 存储能接受（复制为自定义配色时不会被拒）
        assert!(is_valid_scheme(s), "{}", b.id());
    }
}

#[test]
fn builtin_sources_are_complete() {
    for b in builtin_schemes() {
        let Some(src) = b.source() else {
            continue;
        };
        assert!(!src.project.trim().is_empty(), "{}", b.id());
        let rest = src.url.strip_prefix("https://github.com/").unwrap();
        let parts: Vec<&str> = rest.splitn(5, '/').collect();
        assert_eq!(parts.len(), 5, "{}", src.url);
        assert_eq!(parts[2], "blob", "{}", src.url);
        assert!(parts.iter().all(|p| !p.is_empty()), "{}", src.url);
        assert!(
            [
                "MIT",
                "Apache-2.0",
                "BSD-2-Clause",
                "BSD-3-Clause",
                "ISC",
                "X11"
            ]
            .contains(&src.license.spdx())
        );
        assert!(!src.license.text().is_empty());
        assert!(src.copyright.starts_with("Copyright "), "{}", b.id());
    }
    // 只有 Termlet 自调的高对比度没有出处
    let own: Vec<&str> = builtin_schemes()
        .iter()
        .filter(|b| b.source().is_none())
        .map(BuiltinScheme::id)
        .collect();
    assert_eq!(own, ["high-contrast-light", "high-contrast-dark"]);
}

#[test]
fn termlet_light_and_dark() {
    let light = builtin("termlet-light");
    assert_eq!(light.background, "#ffffff");
    assert_eq!(light.foreground, "#383a42");
    assert_eq!(light.cursor, "#526fff");
    assert_eq!(light.cursor_accent, "#ffffff");
    assert_eq!(light.selection_background, "#bfceff");
    assert_eq!(
        light.ansi,
        [
            "#383a42", "#e45649", "#50a14f", "#c18401", "#0184bc", "#a626a4", "#0997b3", "#a0a1a7",
            "#696c77", "#ca1243", "#3f8f3e", "#986801", "#4078f2", "#9a1fa0", "#0a7f96", "#4f525d",
        ]
    );
    let dark = builtin("termlet-dark");
    assert_eq!(dark.background, "#232323");
    // 前景与常用色在背景上清楚可读；除 black 外的 15 色不低于 4:1（brightBlack 用于命令补全提示等次要文字）
    assert!(contrast_ratio(&dark.foreground, &dark.background) > 7.0);
    for c in &dark.ansi[1..] {
        assert!(contrast_ratio(c, &dark.background) > 4.0, "{c}");
    }
}

#[test]
fn high_contrast_schemes() {
    for id in ["high-contrast-light", "high-contrast-dark"] {
        let s = builtin(id);
        assert!(contrast_ratio(&s.foreground, &s.background) >= 7.0);
        for c in &s.ansi[1..] {
            assert!(contrast_ratio(c, &s.background) > 7.0, "{id} {c}");
        }
        let sel_fg = if s.selection_foreground.is_empty() {
            &s.foreground
        } else {
            &s.selection_foreground
        };
        assert!(contrast_ratio(sel_fg, &s.selection_background) > 4.5);
    }
}

#[test]
fn license_text_matches_the_shipped_file() {
    let text = license_text();
    for b in builtin_schemes() {
        assert!(text.contains(&format!("({})", b.id())), "{}", b.id());
        if let Some(src) = b.source() {
            assert!(text.contains(src.url), "{}", src.url);
        }
    }
    // 随安装包分发的文件（scripts/release.mjs 打包时复制）：改了内置配色后用
    // `TERMLET_WRITE_LICENSES=1 cargo test -p termlet-core license_text` 重新生成
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../packaging/licenses/terminal-color-schemes.txt"
    );
    if std::env::var_os("TERMLET_WRITE_LICENSES").is_some() {
        std::fs::write(path, &text).expect("写出许可说明");
    }
    let shipped = std::fs::read_to_string(path).expect("读取随包分发的许可说明");
    assert!(shipped.contains(licenses::GENERATED_BY));
    assert_eq!(
        text, shipped,
        "内置配色变了：重新生成 packaging/licenses/terminal-color-schemes.txt"
    );
}

// ---- 查找与选取 ----

#[test]
fn default_schemes_exist_with_right_darkness() {
    assert_eq!(default_scheme(false).id(), DEFAULT_SCHEME_LIGHT);
    assert_eq!(default_scheme(true).id(), DEFAULT_SCHEME_DARK);
    assert!(!default_scheme(false).is_dark());
    assert!(default_scheme(true).is_dark());
    assert_eq!(builtin_schemes()[0].id(), DEFAULT_SCHEME_LIGHT);
    assert!(builtin_scheme("nord").is_some());
    assert!(builtin_scheme("custom-abc").is_none());
}

#[test]
fn light_and_dark_classification() {
    let dark: Vec<&str> = builtin_schemes()
        .iter()
        .filter(|b| b.is_dark())
        .map(BuiltinScheme::id)
        .collect();
    let light: Vec<&str> = builtin_schemes()
        .iter()
        .filter(|b| !b.is_dark())
        .map(BuiltinScheme::id)
        .collect();
    for id in [
        "termlet-light",
        "solarized-light",
        "github-light",
        "high-contrast-light",
        "ayu-light",
        "vscode-light-2026",
        "dayfox",
        "kanagawa-lotus",
    ] {
        assert!(light.contains(&id), "{id}");
    }
    for id in [
        "termlet-dark",
        "solarized-dark",
        "campbell",
        "xterm-classic",
        "one-dark-pro",
        "ayu-dark",
        "vscode-dark-2026",
        "tomorrow-night",
        "catppuccin-frappe",
        "catppuccin-macchiato",
        "rose-pine-moon",
        "nightfox",
    ] {
        assert!(dark.contains(&id), "{id}");
    }
    assert_eq!(light.len() + dark.len(), builtin_schemes().len());
}

#[test]
fn pick_scheme_follows_the_levels() {
    let mut store = HashMap::new();
    store.insert("custom-abc".to_string(), custom());
    let lookup = |id: &str| store.get(id);
    let settings = TerminalSettings {
        scheme_light: DEFAULT_SCHEME_LIGHT.into(),
        scheme_dark: "custom-abc".into(),
        ..TerminalSettings::default()
    };
    let pick = |choice: SchemeChoice<'_>, dark: bool| pick_scheme(&choice, &settings, dark, lookup);
    let light_default = default_scheme(false).scheme();
    let dark_default = default_scheme(true).scheme();
    let mine = &store["custom-abc"];

    let (s, origin) = pick(SchemeChoice::default(), false);
    assert_eq!((s, origin), (light_default, SchemeOrigin::Global));
    let (s, origin) = pick(SchemeChoice::default(), true);
    assert_eq!((s, origin), (mine, SchemeOrigin::Global));
    let group = SchemeChoice {
        group: Some(DEFAULT_SCHEME_DARK),
        ..SchemeChoice::default()
    };
    assert_eq!(pick(group, false).1, SchemeOrigin::Group);
    let host = SchemeChoice {
        host: Some("custom-abc"),
        group: Some(DEFAULT_SCHEME_DARK),
        ..SchemeChoice::default()
    };
    assert_eq!(pick(host, false), (mine, SchemeOrigin::Host));
    let tab = SchemeChoice {
        tab: Some(DEFAULT_SCHEME_LIGHT),
        host: Some("custom-abc"),
        ..SchemeChoice::default()
    };
    assert_eq!(pick(tab, true).1, SchemeOrigin::Tab);
    let preview = SchemeChoice {
        preview: Some("custom-abc"),
        tab: Some(DEFAULT_SCHEME_LIGHT),
        ..SchemeChoice::default()
    };
    assert_eq!(pick(preview, false).1, SchemeOrigin::Preview);
    // 主机的配色已删除：用分组的；空字符串表示没有指定
    let gone = SchemeChoice {
        host: Some("custom-gone"),
        group: Some("custom-abc"),
        tab: Some(""),
        ..SchemeChoice::default()
    };
    assert_eq!(pick(gone, false).1, SchemeOrigin::Group);

    // 全都不认识：按界面深浅用默认配色
    let unknown = TerminalSettings {
        scheme_light: "from-v99".into(),
        scheme_dark: "custom-gone".into(),
        ..TerminalSettings::default()
    };
    let none = |_: &str| None;
    let nope = SchemeChoice {
        host: Some("nope"),
        ..SchemeChoice::default()
    };
    assert_eq!(pick_scheme(&nope, &unknown, false, none).0, light_default);
    assert_eq!(
        pick_scheme(&SchemeChoice::default(), &unknown, true, none).0,
        dark_default
    );
    assert_eq!(slot_scheme(&unknown, true, none), dark_default);
    assert!(find_scheme("", lookup).is_none());
    assert_eq!(find_scheme("nord", none), Some(builtin("nord")));
}

// ---- 派生的颜色 ----

#[test]
fn highlight_palette_prefers_bright_on_dark_and_normal_on_light() {
    let p = highlight_palette(&custom());
    let got: Vec<&str> = HighlightColor::ALL.iter().map(|&c| p.get(c)).collect();
    assert_eq!(
        got,
        [
            "#ff5555", "#55ff55", "#ffff55", "#5555ff", "#ff55ff", "#55ffff"
        ]
    );
    // 浅色：两组都清楚时用普通的一组
    let mut normal: Vec<String> = custom().ansi[..8].to_vec();
    normal[6] = "#007070".into();
    let light = ColorScheme {
        background: "#ffffff".into(),
        foreground: "#222222".into(),
        ansi: normal.into_iter().chain(ansi("#bbbbbb")).take(16).collect(),
        ..custom()
    };
    assert_eq!(
        highlight_palette(&light).get(HighlightColor::Cyan),
        "#007070"
    );
    // Solarized Dark 的 bright 一组多是灰色：绿、黄、蓝、青用普通的一组
    let solarized = builtin("solarized-dark");
    let p = highlight_palette(solarized);
    for (color, i) in [
        (HighlightColor::Green, 2),
        (HighlightColor::Yellow, 3),
        (HighlightColor::Blue, 4),
        (HighlightColor::Cyan, 6),
    ] {
        assert_eq!(p.get(color), solarized.ansi[i]);
    }
}

#[test]
fn highlight_palette_is_readable_on_every_builtin() {
    for b in builtin_schemes() {
        let s = b.scheme();
        let p = highlight_palette(s);
        for color in HighlightColor::ALL {
            let c = p.get(color);
            assert!(is_lower_hex(c), "{} {color:?}", b.id());
            assert!(
                contrast_ratio(c, &s.background) >= MIN_HIGHLIGHT_CONTRAST,
                "{} {color:?}",
                b.id()
            );
        }
    }
    // 两组的黄色都只有约 1.5:1：向前景色混合，但不是前景色本身
    let pale = ColorScheme {
        background: "#fafafa".into(),
        foreground: "#202020".into(),
        ansi: ansi("#e0c040"),
        ..custom()
    };
    let yellow = highlight_palette(&pale)
        .get(HighlightColor::Yellow)
        .to_string();
    assert!(contrast_ratio("#e0c040", &pale.background) < 2.0);
    assert!(contrast_ratio(&yellow, &pale.background) >= MIN_HIGHLIGHT_CONTRAST);
    assert_ne!(yellow, pale.foreground);
}

#[test]
fn highlight_color_names() {
    assert_eq!(HighlightColor::from_name("red"), Some(HighlightColor::Red));
    assert_eq!(
        HighlightColor::from_name("cyan"),
        Some(HighlightColor::Cyan)
    );
    assert_eq!(HighlightColor::from_name("black"), None);
    assert_eq!(HighlightColor::from_name("Red"), None);
}

#[test]
fn search_colors_keep_text_readable() {
    let white = ColorScheme {
        background: "#ffffff".into(),
        foreground: "#333333".into(),
        ..custom()
    };
    let mut all: Vec<&ColorScheme> = builtin_schemes()
        .iter()
        .map(BuiltinScheme::scheme)
        .collect();
    let mine = custom();
    all.push(&mine);
    all.push(&white);
    for s in all {
        let c = search_colors(s);
        for v in [
            &c.match_background,
            &c.active_match_background,
            &c.match_overview_ruler,
            &c.active_match_overview_ruler,
        ] {
            assert!(is_lower_hex(v), "{} {v}", s.name);
        }
        assert!(
            contrast_ratio(&c.active_match_background, &s.foreground) >= 3.0,
            "{}",
            s.name
        );
        assert_ne!(c.active_match_background, s.background);
    }
}

#[test]
fn unreadable_colors_ignore_black_and_white() {
    assert_eq!(
        unreadable_colors(&custom()),
        [ColorKey::Ansi(AnsiColor::Blue)]
    );
    let colorful = [
        AnsiColor::Red,
        AnsiColor::Green,
        AnsiColor::Yellow,
        AnsiColor::Blue,
        AnsiColor::Magenta,
        AnsiColor::Cyan,
    ]
    .map(ColorKey::Ansi);
    let bad = ColorScheme {
        foreground: "#1a1a1a".into(),
        ansi: ansi("#121212"),
        ..custom()
    };
    let mut expected = vec![ColorKey::Foreground];
    expected.extend(colorful);
    assert_eq!(unreadable_colors(&bad), expected);
    let light = ColorScheme {
        background: "#ffffff".into(),
        foreground: "#222222".into(),
        ansi: ansi("#f8f8f8"),
        ..custom()
    };
    assert_eq!(unreadable_colors(&light), colorful);
}

#[test]
fn builtin_schemes_have_no_unreadable_colors() {
    // Solarized Light 等浅色配色的 black 本来就是浅色，不算
    for b in builtin_schemes() {
        assert!(unreadable_colors(b.scheme()).is_empty(), "{}", b.id());
    }
}

#[test]
fn derived_colors_tolerate_incomplete_schemes() {
    // 默认值（颜色为空、没有 16 色）也不出错
    let empty = ColorScheme::default();
    let _ = search_colors(&empty);
    let _ = highlight_palette(&empty);
    let _ = unreadable_colors(&empty);
    let _ = scheme_sample(&empty, |_| Vec::new());
    assert_eq!(ansi_color(&empty, 3), "");
}

#[test]
fn sample_follows_the_scheme_and_highlight_rules() {
    let c = custom();
    // 不区分大小写地匹配 error、warn（代替关键词高亮规则）
    let rules = |text: &str| {
        let lower = text.to_lowercase();
        let mut out = Vec::new();
        for (word, color) in [
            ("error", HighlightColor::Red),
            ("warn", HighlightColor::Yellow),
        ] {
            if let Some(at) = lower.find(word) {
                out.push((at..at + word.len(), color));
            }
        }
        out.sort_by_key(|(r, _)| r.start);
        out
    };
    let lines = scheme_sample(&c, rules);
    let palette = highlight_palette(&c);
    // 提示符：粗体的绿色显示为 bright 绿；光标用光标色
    assert_eq!(lines[0][0].text, "ruan@web-01");
    assert_eq!(lines[0][0].fg.as_deref(), Some(c.ansi[10].as_str()));
    assert!(lines[0][0].bold);
    let cursor = lines[0].last().unwrap();
    assert_eq!(cursor.bg.as_deref(), Some(c.cursor.as_str()));
    assert_eq!(cursor.fg.as_deref(), Some(c.background.as_str()));
    let selected = lines[1].last().unwrap();
    assert_eq!(selected.text, "README.md");
    assert_eq!(
        selected.bg.as_deref(),
        Some(c.selection_background.as_str())
    );
    assert_eq!(selected.fg, None);
    let all: Vec<&SampleSpan> = lines.iter().flatten().collect();
    let error = all.iter().find(|s| s.text == "ERROR").unwrap();
    assert_eq!(error.fg.as_deref(), Some(palette.get(HighlightColor::Red)));
    let warn = all.iter().find(|s| s.text == "WARN").unwrap();
    assert_eq!(
        warn.fg.as_deref(),
        Some(palette.get(HighlightColor::Yellow))
    );
    // 没有规则（关闭了高亮）时日志不着色
    let plain = scheme_sample(&c, |_| Vec::new());
    let log = plain
        .iter()
        .flatten()
        .find(|s| s.text.contains("ERROR"))
        .unwrap();
    assert_eq!(log.fg, None);
    // 位置不合法的命中忽略
    let broken = scheme_sample(&c, |t| {
        vec![
            (Range { start: 5, end: 2 }, HighlightColor::Red),
            (0..1000, HighlightColor::Red),
            (t.len() - 1..t.len() + 1, HighlightColor::Red),
        ]
    });
    assert_eq!(broken, plain);
    // 有光标文字色、选区文字色时用它们
    let with_extras = ColorScheme {
        cursor_accent: "#010203".into(),
        selection_foreground: "#fefdfc".into(),
        ..custom()
    };
    let lines = scheme_sample(&with_extras, |_| Vec::new());
    assert_eq!(lines[0].last().unwrap().fg.as_deref(), Some("#010203"));
    assert_eq!(lines[1].last().unwrap().fg.as_deref(), Some("#fefdfc"));
}

// ---- 自定义配色 ----

#[test]
fn unique_name_adds_numbers_and_truncates() {
    assert_eq!(unique_name("Dracula", ["nord"]), "Dracula");
    assert_eq!(unique_name("Dracula", ["dracula"]), "Dracula (1)");
    assert_eq!(
        unique_name("Dracula", ["Dracula", "Dracula (1)"]),
        "Dracula (2)"
    );
    assert_eq!(unique_name("  ", []), "Scheme");
    let long = "长".repeat(MAX_SCHEME_NAME + 5);
    assert_eq!(unique_name(&long, []).chars().count(), MAX_SCHEME_NAME);
    let taken = "长".repeat(MAX_SCHEME_NAME);
    let renamed = unique_name(&long, [taken.as_str()]);
    assert_eq!(renamed.chars().count(), MAX_SCHEME_NAME);
    assert!(renamed.ends_with(" (1)"));
}

#[test]
fn to_custom_scheme_normalizes_and_drops_extras() {
    let builtin = default_scheme(true).scheme();
    let copy = to_custom_scheme(builtin, [builtin.name.as_str()]).unwrap();
    assert_eq!(copy.name, format!("{} (1)", builtin.name));
    assert_eq!(copy.ansi, builtin.ansi);
    let mut odd = custom();
    odd.name = "X".into();
    odd.foreground = "#ABC".into();
    odd.cursor_accent = "nope".into();
    odd.selection_foreground = "FEFDFC".into();
    odd.extra.insert("source".into(), "x".into());
    let parsed = to_custom_scheme(&odd, []).unwrap();
    assert_eq!(parsed.foreground, "#aabbcc");
    assert_eq!(parsed.cursor_accent, "");
    assert_eq!(parsed.selection_foreground, "#fefdfc");
    assert!(parsed.extra.is_empty());
    let mut short = custom();
    short.ansi.pop();
    assert_eq!(to_custom_scheme(&short, []), None);
    let mut bad = custom();
    bad.background = "blue".into();
    assert_eq!(to_custom_scheme(&bad, []), None);
}

#[test]
fn is_valid_scheme_matches_the_store() {
    assert!(is_valid_scheme(&custom()));
    assert!(!is_valid_scheme(&ColorScheme {
        name: " ".into(),
        ..custom()
    }));
    assert!(!is_valid_scheme(&ColorScheme {
        cursor: "#12".into(),
        ..custom()
    }));
    assert!(!is_valid_scheme(&ColorScheme {
        name: "x".repeat(MAX_SCHEME_NAME + 1),
        ..custom()
    }));
}

#[test]
fn names_trim_like_go_trim_space() {
    // U+0085 也算空白（与 Go 的 strings.TrimSpace 一致）
    assert!(!is_valid_scheme(&ColorScheme {
        name: "\u{85}".into(),
        ..custom()
    }));
    assert_eq!(unique_name("\u{85}", []), "Scheme");
    assert_eq!(unique_name("\u{85}Dracula", ["dracula"]), "Dracula (1)");
    assert_eq!(unique_name("\u{85} Night\u{3000}\u{85}", []), "Night");
}
