//! 读取本机已装客户端的配色：用临时目录模拟各客户端的位置，
//! 以及配色计数。

use std::cell::Cell;
use std::fs;

use encoding_rs::GBK;
use tempfile::TempDir;

use super::*;

fn write_file(path: &Path, data: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
}

fn env<'a>() -> SchemeEnv<'a> {
    SchemeEnv {
        local_app_data: None,
        config_dir: None,
        xshell_data_dirs: Vec::new(),
        mobaxterm_inis: Vec::new(),
        mobaxterm_registry: None,
        ansi: encoding_for_code_page(936),
    }
}

fn by_client(list: &[InstalledSchemeSource], client: SchemeClient) -> Vec<&InstalledSchemeSource> {
    list.iter().filter(|s| s.client == client).collect()
}

const XCS_SAMPLE: &str = "[Names]\r\nname0=暗色\r\ncount=1\r\n[暗色]\r\ntext=dddddd\r\nbackground=202020\r\nred=c03030\r\nred(bold)=ff5050\r\n";

fn utf16le(s: &str, bom: bool) -> Vec<u8> {
    let mut out = if bom { vec![0xff, 0xfe] } else { Vec::new() };
    out.extend(s.encode_utf16().flat_map(u16::to_le_bytes));
    out
}

#[test]
fn nothing_found_is_an_empty_list() {
    assert!(collect(&env()).is_empty());
}

#[test]
fn windows_terminal_keeps_only_scheme_colors() {
    let local = TempDir::new().unwrap();
    let packaged = |package: &str| {
        local
            .path()
            .join("Packages")
            .join(package)
            .join("LocalState")
            .join("settings.json")
    };
    let store = packaged("Microsoft.WindowsTerminal_8wekyb3d8bbwe");
    let preview = packaged("Microsoft.WindowsTerminalPreview_8wekyb3d8bbwe");
    let unpackaged = local
        .path()
        .join("Microsoft")
        .join("Windows Terminal")
        .join("settings.json");
    let settings = r##"// 注释
{
  "profiles": {
    "defaults": { "environment": { "API_TOKEN": "env-secret" } },
    "list": [ { "name": "prod", "commandline": "plink -pw should-not-leak root@10.0.0.9", }, ],
  },
  /* 配色 */
  "schemes": [
    { "name": "Mine // 不是注释", "background": "#000000", "purple": "#aa00aa", "font": { "face": "x" }, },
    "不是对象",
  ],
}"##;
    write_file(
        &store,
        &[&[0xef, 0xbb, 0xbf][..], settings.as_bytes()].concat(),
    );
    // 没有 schemes 的跳过
    write_file(&preview, br#"{ "profiles": {} }"#);
    // 解析不了的跳过，不返回原文
    write_file(&unpackaged, br#"{ "schemes": [ { "name": "Broken" ] }"#);

    let e = SchemeEnv {
        local_app_data: Some(local.path().to_path_buf()),
        ..env()
    };
    let all = collect(&e);
    let got = by_client(&all, SchemeClient::WindowsTerminal);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].path, display(&store));
    let c = &got[0].content;
    for s in [
        "profiles",
        "plink",
        "should-not-leak",
        "env-secret",
        "commandline",
        "font",
    ] {
        assert!(!c.contains(s), "内容中不应有配色以外的 {s}：{c}");
    }
    let doc: serde_json::Value = serde_json::from_str(c).unwrap();
    let schemes = doc["schemes"].as_array().unwrap();
    assert_eq!(schemes.len(), 1);
    assert_eq!(schemes[0]["name"], "Mine // 不是注释");
    assert_eq!(schemes[0]["background"], "#000000");
    assert_eq!(schemes[0]["purple"], "#aa00aa");
    // 取出的内容能直接导入
    assert_eq!(custom_scheme_count(got[0]), 1);
}

#[test]
fn wt_schemes_json_from_strings() {
    let out =
        wt_schemes_json(r##"{"schemes": [{"name": "A", "red": "#ff0000", "x": 1}, {"x": 2}]}"##)
            .unwrap();
    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(doc["schemes"].as_array().map(Vec::len), Some(1));
    assert_eq!(doc["schemes"][0]["red"], "#ff0000");
    assert!(doc["schemes"][0].get("x").is_none());
    assert_eq!(wt_schemes_json(r#"{"schemes": {}}"#), None);
    assert_eq!(wt_schemes_json(r#"[{"schemes": []}]"#), None);
    assert_eq!(wt_schemes_json("not json"), None);
}

#[test]
fn xshell_files_in_every_encoding() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("NetSarang Computer").join("8");
    let dir = data.join("Xshell").join("ColorScheme Files");
    write_file(&dir.join("a.xcs"), &utf16le(XCS_SAMPLE, true));
    write_file(&dir.join("b.XCS"), &GBK.encode(XCS_SAMPLE).0);
    write_file(&dir.join("c.xcs"), &utf16le(XCS_SAMPLE, false));
    write_file(&dir.join("d.xcs"), XCS_SAMPLE.as_bytes());
    write_file(&dir.join("notes.txt"), b"x");
    write_file(&dir.join("huge.xcs"), &vec![0u8; MAX_SCHEME_FILE_SIZE + 1]);
    fs::create_dir_all(dir.join("sub.xcs")).unwrap();

    // 同一目录出现两次（注册表与"文档"指向同一处）只读一次
    let e = SchemeEnv {
        xshell_data_dirs: vec![data.clone(), data.clone(), tmp.path().join("none")],
        ..env()
    };
    let got = collect(&e);
    assert_eq!(got.len(), 4, "{got:?}");
    for (s, name) in got.iter().zip(["a.xcs", "b.XCS", "c.xcs", "d.xcs"]) {
        assert_eq!(s.client, SchemeClient::Xshell);
        assert_eq!(s.path, display(&dir.join(name)));
        // UTF-16（带或不带 BOM）、GBK、UTF-8 都转为 UTF-8
        assert_eq!(s.content, XCS_SAMPLE, "{name}");
        assert_eq!(custom_scheme_count(s), 1, "{name}");
    }
}

#[test]
fn xshell_file_count_is_limited() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("Xshell").join("ColorScheme Files");
    for i in 0..MAX_SCHEME_FILES + 20 {
        write_file(&dir.join(format!("s{i:03}.xcs")), XCS_SAMPLE.as_bytes());
    }
    let e = SchemeEnv {
        xshell_data_dirs: vec![tmp.path().to_path_buf()],
        ..env()
    };
    assert_eq!(collect(&e).len(), MAX_SCHEME_FILES);
}

#[test]
fn mobaxterm_ini_keeps_only_colors() {
    let tmp = TempDir::new().unwrap();
    let ini = tmp.path().join("MobaXterm").join("MobaXterm.ini");
    write_file(
        &ini,
        b"[Bookmarks]\r\nSubRep=\r\nweb=#109#0%10.0.0.1%22%root%%-1%-1%\r\n\r\n[Colors]\r\nForegroundColour=236,236,236\r\nBackgroundColour=30,30,30\r\nBlack=0,0,0\r\n\r\n[Passwords]\r\nroot@10.0.0.1=secret\r\n",
    );
    let called = Cell::new(false);
    let e = SchemeEnv {
        // 同一文件出现两次（"文档"与重定向后的"文档"相同）
        mobaxterm_inis: vec![
            ini.clone(),
            ini.parent().unwrap().to_path_buf(),
            tmp.path().join("none.ini"),
        ],
        mobaxterm_registry: Some(Box::new(|| {
            called.set(true);
            Some("[Colors]\nBlack=1,1,1\n".into())
        })),
        ..env()
    };
    let got = collect(&e);
    assert_eq!(
        got,
        [InstalledSchemeSource {
            client: SchemeClient::MobaXterm,
            path: display(&ini),
            content:
                "[Colors]\nForegroundColour=236,236,236\nBackgroundColour=30,30,30\nBlack=0,0,0\n"
                    .into(),
        }]
    );
    // 已从 MobaXterm.ini 读到配色时不再读注册表
    assert!(!called.get());
}

#[test]
fn mobaxterm_registry_when_ini_has_no_colors() {
    let tmp = TempDir::new().unwrap();
    let ini = tmp.path().join("MobaXterm.ini");
    write_file(&ini, b"[Misc]\nPasswordsInRegistry=1\n");
    let e = SchemeEnv {
        mobaxterm_inis: vec![ini],
        mobaxterm_registry: Some(Box::new(|| Some("[Colors]\nBlack=1,1,1\n".into()))),
        ..env()
    };
    let got = collect(&e);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].path, MOBAXTERM_REGISTRY_PATH);
    assert_eq!(got[0].content, "[Colors]\nBlack=1,1,1\n");
}

const TABBY_CONFIG: &str = "version: 3
profiles:
  - type: ssh
    name: prod
    options:
      host: 10.0.0.9
      user: root
      password: should-not-leak
    terminalColorScheme:
      name: Red Alert
      foreground: '#ffffff'
      background: '#330000'
      cursor: '#ffffff'
      colors: ['#000000', '#ff0000', '#00ff00', '#ffff00', '#0000ff', '#ff00ff', '#00ffff', '#ffffff']
  - type: local
    name: shell
terminal:
  fontSize: 14
  colorScheme:
    name: Mine
    foreground: '#dddddd'
    background: '#202020'
    cursor: '#ffcc00'
    colors:
      - '#101010'
      - '#c03030'
  customColorSchemes:
    - name: Other
      foreground: '#c5c8c6'
      background: '#1d1f21'
      colors: []
vault:
  contents: encrypted-blob
";

#[test]
fn tabby_keeps_only_scheme_parts() {
    let cfg = TempDir::new().unwrap();
    let path = cfg.path().join("tabby").join("config.yaml");
    write_file(&path, TABBY_CONFIG.as_bytes());
    // 没有配色
    write_file(
        &cfg.path().join("terminus").join("config.yaml"),
        b"terminal:\n  fontSize: 12\n",
    );
    let e = SchemeEnv {
        config_dir: Some(cfg.path().to_path_buf()),
        ..env()
    };
    let got = collect(&e);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].client, SchemeClient::Tabby);
    assert_eq!(got[0].path, display(&path));
    let c = &got[0].content;
    for s in ["Red Alert", "Mine", "Other", "#c03030"] {
        assert!(c.contains(s), "内容中应有 {s}：{c}");
    }
    for s in ["10.0.0.9", "should-not-leak", "encrypted-blob", "fontSize"] {
        assert!(!c.contains(s), "内容中不应有配色以外的 {s}：{c}");
    }
    assert!(serde_json::from_str::<serde_json::Value>(c).is_ok());
    // 解析：Red Alert 只有 8 色、Mine 颜色太少（不算），Other 没有颜色（不是 Tabby 配色的样子）
    let r = parse_schemes(c, Some(&got[0].path));
    assert_eq!(r.format, Some(crate::schemes::SchemeFormat::Tabby));
    assert_eq!(
        r.schemes
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["Red Alert"]
    );
}

#[test]
fn tabby_malformed_config_is_skipped() {
    let cfg = TempDir::new().unwrap();
    write_file(
        &cfg.path().join("tabby").join("config.yaml"),
        b"terminal: [unclosed\n  : :\n",
    );
    let e = SchemeEnv {
        config_dir: Some(cfg.path().to_path_buf()),
        ..env()
    };
    assert!(collect(&e).is_empty());
}

#[test]
fn tabby_schemes_json_tolerates_wrong_types() {
    // customColorSchemes 不是数组、profiles 中有不是映射的项：只跳过这些
    let out = tabby_schemes_json(
        "terminal:\n  colorScheme: {name: A}\n  customColorSchemes: oops\nprofiles:\n  - just text\n  - terminalColorScheme: {name: B}\n  - terminalColorScheme: ~\n",
    )
    .unwrap();
    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(doc["terminal"]["colorScheme"]["name"], "A");
    assert!(doc["terminal"].get("customColorSchemes").is_none());
    assert_eq!(doc["profiles"].as_array().map(Vec::len), Some(1));
    assert_eq!(tabby_schemes_json("terminal:\n  fontSize: 12\n"), None);
    assert_eq!(tabby_schemes_json("- a\n- b\n"), None);
    // 不是字符串的键转为字符串，带标签的取其中的值
    let out = tabby_schemes_json("terminal:\n  colorScheme: !custom\n    1: one\n    true: yes\n")
        .unwrap();
    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(doc["terminal"]["colorScheme"]["1"], "one");
}

/// YAML 的边界情况。“旧：”标出与原来的 serde_yaml_ng 不同的结果。
#[test]
fn tabby_schemes_json_yaml_edge_cases() {
    // 比较 JSON 值而不是文本：编译整个 workspace 时 serde_json 开了 preserve_order，键的顺序不同
    let scheme = |text: &str| {
        tabby_schemes_json(text).map(|out| {
            let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
            doc["terminal"]["colorScheme"].clone()
        })
    };
    let json =
        |text: Option<&str>| text.map(|t| serde_json::from_str::<serde_json::Value>(t).unwrap());
    let cases = [
        // 数字、布尔值与空值
        (
            "{name: 1.0, b: 0x10, c: yes, d: ~, g: 1e3, h: 12345678901234567890, i: '#fff', j: True}",
            Some(
                r##"{"b":16,"c":"yes","d":null,"g":1000.0,"h":12345678901234567890,"i":"#fff","j":true,"name":1.0}"##,
            ),
        ),
        // 旧：NaN、无穷大为 null
        ("{a: .nan, f: .inf}", Some(r#"{"a":".nan","f":".inf"}"#)),
        // 旧：多余的前导零保留为文本 "007"
        ("{a: 007}", Some(r#"{"a":7.0}"#)),
        // 不是字符串的键转为字符串；旧：null 与集合作键时跳过这些键，现在整个文件不认
        (
            "{1: a, true: b, 1.5: c, -1: d}",
            Some(r#"{"-1":"d","1":"a","1.5":"c","true":"b"}"#),
        ),
        ("{~: a, name: A}", None),
        ("{[x]: a, name: A}", None),
        // 重复的键：整个文件不认
        ("{name: A, name: B}", None),
        // 标签：取其中的值
        (
            "!scheme {name: !!str 12, colors: !!seq [a]}",
            Some(r#"{"colors":["a"],"name":"12"}"#),
        ),
    ];
    for (value, expected) in cases {
        let text = format!("terminal:\n  colorScheme: {value}\n");
        assert_eq!(scheme(&text), json(expected), "{value}");
    }
    // 锚点与别名展开；旧：合并键（<<）不展开，保留为名为 "<<" 的键
    let text = "s: &s {name: A, foreground: '#fff'}\nterminal:\n  colorScheme:\n    <<: *s\n    name: B\n  customColorSchemes: [*s]\n";
    assert_eq!(
        scheme(text),
        json(Some(r##"{"foreground":"#fff","name":"B"}"##))
    );
    let doc: serde_json::Value = serde_json::from_str(&tabby_schemes_json(text).unwrap()).unwrap();
    assert_eq!(doc["terminal"]["customColorSchemes"][0]["name"], "A");
    // 多个文档：整个文件不认
    assert_eq!(
        tabby_schemes_json("terminal:\n  colorScheme: {name: A}\n---\nterminal: {}\n"),
        None
    );
}

#[test]
fn ini_section_extracts_one_section() {
    let text = "[a]\nx=1\n[ colors ]\n; 注释\nBlack=0,0,0\n\n[b]\ny=2\n";
    assert_eq!(
        ini_section(text, "Colors").as_deref(),
        Some("[Colors]\n; 注释\nBlack=0,0,0\n")
    );
    // 空段
    assert_eq!(ini_section("[Colors]\n\n[b]\n", "Colors"), None);
    assert_eq!(
        ini_section("[Colors]\r\nBlack=0,0,0\r\n", "colors").as_deref(),
        Some("[colors]\nBlack=0,0,0\n")
    );
}

#[test]
fn xshell_data_dirs_without_duplicates() {
    let home = TempDir::new().unwrap();
    let dirs = xshell_data_dirs(home.path());
    let mut seen = HashSet::new();
    for d in &dirs {
        assert!(
            seen.insert(same_path_key(&display(d))),
            "重复的目录：{dirs:?}"
        );
    }
    let default = home
        .path()
        .join("Documents")
        .join("NetSarang Computer")
        .join("8");
    assert!(
        seen.contains(&same_path_key(&display(&default))),
        "{dirs:?}"
    );
}

#[test]
fn same_path_keys() {
    assert_eq!(same_path_key("a/./b/../c"), same_path_key("a/c"));
    if cfg!(windows) {
        assert_eq!(
            same_path_key(r"C:\Users\Me\X.ini"),
            same_path_key("c:/users/me/x.ini")
        );
    }
}

#[test]
fn code_pages_and_env_expansion() {
    assert_eq!(encoding_for_code_page(936), encoding_rs::GB18030);
    assert_eq!(encoding_for_code_page(950), encoding_rs::BIG5);
    assert_eq!(encoding_for_code_page(1252), encoding_rs::WINDOWS_1252);
    assert_eq!(encoding_for_code_page(12345), encoding_rs::GB18030);
    let path = std::env::var("PATH").unwrap_or_default();
    assert_eq!(expand_env("%PATH%"), path);
    assert_eq!(
        expand_env("a%NO_SUCH_VAR_TERMLET%b"),
        "a%NO_SUCH_VAR_TERMLET%b"
    );
    assert_eq!(expand_env("50%"), "50%");
    assert_eq!(expand_env("%%"), "%%");
}

// ---- 计数 ----

/// MobaXterm 自带的默认配色（深色背景/浅色文字），按 [Colors] 的写法
const MOBA_DEFAULT: [&str; 19] = [
    "Black=30,30,30",
    "BoldBlack=128,128,128",
    "Red=255,96,96",
    "BoldRed=255,128,128",
    "Green=14,177,108",
    "BoldGreen=24,237,147",
    "Yellow=190,190,18",
    "BoldYellow=222,220,18",
    "Blue=18,150,190",
    "BoldBlue=27,186,233",
    "Magenta=255,77,255",
    "BoldMagenta=255,125,255",
    "Cyan=84,204,239",
    "BoldCyan=142,221,244",
    "White=204,204,204",
    "BoldWhite=204,204,204",
    "ForegroundColour=236,236,236",
    "BackgroundColour=30,30,30",
    "CursorColour=180,180,192",
];

fn moba(lines: &[&str]) -> InstalledSchemeSource {
    InstalledSchemeSource {
        client: SchemeClient::MobaXterm,
        path: "MobaXterm.ini".into(),
        content: format!("[Colors]\nDefaultColorScheme=0\n{}\n", lines.join("\n")),
    }
}

#[test]
fn mobaxterm_builtin_colors_do_not_count() {
    assert_eq!(custom_scheme_count(&moba(&MOBA_DEFAULT)), 0);
    assert_eq!(custom_scheme_count(&moba(&MOBA_DEFAULT[16..])), 0);
    // 自带的 Monokai
    assert_eq!(
        custom_scheme_count(&moba(&[
            "ForegroundColour=248,248,242",
            "BackgroundColour=39,40,34"
        ])),
        0
    );
    let mut changed: Vec<&str> = MOBA_DEFAULT[..18].to_vec();
    changed.push("CursorColour=255,0,0");
    assert_eq!(custom_scheme_count(&moba(&changed)), 1);
}

#[test]
fn tabby_counts_only_custom_schemes() {
    let p: Vec<String> = [
        "#101010", "#c03030", "#30a030", "#c0a030", "#3060c0", "#a040a0", "#30a0a0", "#c0c0c0",
        "#505050", "#ff5050", "#50ff50", "#ffff50", "#5080ff", "#ff50ff", "#50ffff", "#ffffff",
    ]
    .map(String::from)
    .to_vec();
    let scheme = |name: &str| {
        serde_json::json!({
            "name": name, "foreground": "#dddddd", "background": "#202020", "cursor": "#ffcc00", "colors": p,
        })
    };
    let content = |custom: Option<Vec<serde_json::Value>>| {
        let mut terminal = serde_json::json!({
            "colorScheme": scheme("Tabby Default"),
            "lightColorScheme": scheme("Tabby Default Light"),
        });
        if let Some(custom) = custom {
            terminal["customColorSchemes"] = custom.into();
        }
        serde_json::json!({
            "terminal": terminal,
            "profiles": [{ "terminalColorScheme": scheme("Profile") }],
        })
        .to_string()
    };
    let tabby = |c: String| InstalledSchemeSource {
        client: SchemeClient::Tabby,
        path: "config.yaml".into(),
        content: c,
    };
    assert_eq!(custom_scheme_count(&tabby(content(None))), 0);
    assert_eq!(
        custom_scheme_count(&tabby(content(Some(vec![scheme("Mine"), scheme("Other")])))),
        2
    );
    assert_eq!(custom_scheme_count(&tabby("not json".into())), 0);
}

#[test]
fn other_clients_count_parsed_schemes() {
    let wt = |name: &str| {
        serde_json::json!({
            "name": name, "foreground": "#dddddd", "background": "#202020", "brightBlack": "#505050",
            "black": "#101010", "red": "#c03030", "green": "#30a030", "yellow": "#c0a030",
        })
    };
    let source = InstalledSchemeSource {
        client: SchemeClient::WindowsTerminal,
        path: "settings.json".into(),
        content: serde_json::json!({ "schemes": [wt("A"), wt("B")] }).to_string(),
    };
    assert_eq!(custom_scheme_count(&source), 2);
}
