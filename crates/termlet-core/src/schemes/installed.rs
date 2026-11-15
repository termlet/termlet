//! 读取本机已装客户端的终端配色：Windows Terminal、Xshell、MobaXterm、Tabby。
//!
//! 只读、只找固定位置，找不到就跳过。内容转为 UTF-8 文本，交给 [`parse_schemes`] 识别格式。
//! 为了不把无关数据带出来，Windows Terminal 只取 `schemes` 中的名称与颜色（`settings.json` 的 profiles 中有命令行、
//! 环境变量等），MobaXterm 只取 `[Colors]` 段（`MobaXterm.ini` 中还有书签、保存的密码等），
//! Tabby 只取配色相关的部分（`config.yaml` 中还有主机配置、密码库等），转为 JSON。
//! 文件读取与解析分开：解析部分是纯函数，直接用字符串测试。

use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use encoding_rs::Encoding;
use serde::Serialize;

use super::import::{decode_with_fallback, is_mobaxterm_builtin, strip_jsonc};
use super::parse_schemes;

/// 配色文件来自哪个客户端。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SchemeClient {
    WindowsTerminal,
    Xshell,
    MobaXterm,
    Tabby,
}

/// 本机已装客户端中的一个配色文件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledSchemeSource {
    pub client: SchemeClient,
    /// 文件的位置（显示用，主名也用作没有名称的配色的名称）；安装版 MobaXterm 保存在注册表中时为
    /// [`MOBAXTERM_REGISTRY_PATH`]
    pub path: String,
    /// 只含配色的内容（UTF-8）
    pub content: String,
}

/// 安装版 MobaXterm 把设置保存在注册表中时，[`InstalledSchemeSource::path`] 为这个位置。
pub const MOBAXTERM_REGISTRY_PATH: &str = r"HKEY_CURRENT_USER\Software\Mobatek\MobaXterm";

/// 单个配色文件（取出的内容）的大小上限，超过的跳过。
const MAX_SCHEME_FILE_SIZE: usize = 2 << 20;
/// 最多返回的文件数（Xshell 的配色目录中可能有很多 `.xcs`）。
const MAX_SCHEME_FILES: usize = 200;
/// 返回内容的总大小上限。
const MAX_SCHEME_TOTAL_SIZE: usize = 16 << 20;
/// 配置文件（`MobaXterm.ini`、Tabby 的 `config.yaml`）的大小上限。
const MAX_CONFIG_SIZE: usize = 16 << 20;

/// 查找配色文件用到的本机位置，测试中换成临时目录。
struct SchemeEnv<'a> {
    /// `%LOCALAPPDATA%`（Windows Terminal 的位置），非 Windows 为空
    local_app_data: Option<PathBuf>,
    /// 用户的配置目录：Windows 为 `%APPDATA%`（Tabby 的位置）
    config_dir: Option<PathBuf>,
    /// NetSarang 的用户数据目录（含版本号这一级，例如"文档\NetSarang Computer\8"）
    xshell_data_dirs: Vec<PathBuf>,
    /// `MobaXterm.ini` 可能的位置
    mobaxterm_inis: Vec<PathBuf>,
    /// 安装版 MobaXterm 保存在注册表中的 `[Colors]`（组织成 INI 文本）
    mobaxterm_registry: Option<Box<dyn Fn() -> Option<String> + 'a>>,
    /// 不是 UTF-8、UTF-16 的文件按这个编码解码（系统的 ANSI 代码页）
    ansi: &'static Encoding,
}

/// 读取本机已装客户端的配色文件；都找不到时返回空列表。会读取文件与注册表，在后台线程中调用。
pub fn installed_color_schemes() -> Vec<InstalledSchemeSource> {
    collect(&default_env())
}

fn default_env() -> SchemeEnv<'static> {
    let home = dirs::home_dir();
    SchemeEnv {
        local_app_data: if cfg!(windows) {
            dirs::data_local_dir()
        } else {
            None
        },
        config_dir: dirs::config_dir(),
        xshell_data_dirs: home.as_deref().map(xshell_data_dirs).unwrap_or_default(),
        mobaxterm_inis: home
            .as_deref()
            .map(mobaxterm_ini_candidates)
            .unwrap_or_default(),
        mobaxterm_registry: Some(Box::new(platform::read_mobaxterm_registry_colors)),
        ansi: platform::ansi_encoding(),
    }
}

/// 收集结果并执行数量、大小上限；同一文件只收一次。
#[derive(Default)]
struct Collector {
    out: Vec<InstalledSchemeSource>,
    total: usize,
    seen: HashSet<String>,
}

impl Collector {
    fn full(&self) -> bool {
        self.out.len() >= MAX_SCHEME_FILES || self.total >= MAX_SCHEME_TOTAL_SIZE
    }

    fn add(&mut self, client: SchemeClient, path: String, content: String) {
        let key = same_path_key(&path);
        if self.full()
            || self.seen.contains(&key)
            || content.len() > MAX_SCHEME_FILE_SIZE
            || self.total + content.len() > MAX_SCHEME_TOTAL_SIZE
        {
            return;
        }
        self.seen.insert(key);
        self.total += content.len();
        self.out.push(InstalledSchemeSource {
            client,
            path,
            content,
        });
    }
}

/// 判断两个路径是否是同一个文件用的键：按字面规范化（去掉 `.`、处理 `..`），Windows 上不区分大小写。
fn same_path_key(path: &str) -> String {
    let mut clean = PathBuf::new();
    for c in Path::new(path).components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !clean.pop() {
                    clean.push("..");
                }
            }
            other => clean.push(other),
        }
    }
    let key = clean.to_string_lossy().into_owned();
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key
    }
}

fn collect(env: &SchemeEnv<'_>) -> Vec<InstalledSchemeSource> {
    let mut c = Collector::default();
    collect_windows_terminal(&mut c, env);
    collect_xshell(&mut c, env);
    collect_mobaxterm(&mut c, env);
    collect_tabby(&mut c, env);
    c.out
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

// ---- 读取文件 ----

/// 读取普通文件（不是目录、设备等），超过 `limit` 时返回 `None`。
fn read_limited(path: &Path, limit: usize) -> Option<Vec<u8>> {
    let file = File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > limit as u64 {
        return None;
    }
    let mut raw = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut raw).ok()?;
    (raw.len() <= limit).then_some(raw)
}

/// 读取配置文件；`path` 是目录时读取其中名为 `name` 的文件（例如便携版的程序目录）。返回实际读取的位置。
fn read_config_file(path: &Path, name: &str) -> Option<(Vec<u8>, PathBuf)> {
    let path = if path.is_dir() {
        path.join(name)
    } else {
        path.to_path_buf()
    };
    let raw = read_limited(&path, MAX_CONFIG_SIZE)?;
    Some((raw, path))
}

// ---- Windows Terminal ----

/// 商店版、预览版、非商店版的 `settings.json`（配色在其中的 `schemes` 数组）。
fn collect_windows_terminal(c: &mut Collector, env: &SchemeEnv<'_>) {
    let Some(local) = &env.local_app_data else {
        return;
    };
    let packaged = |package: &str| {
        local
            .join("Packages")
            .join(package)
            .join("LocalState")
            .join("settings.json")
    };
    for path in [
        packaged("Microsoft.WindowsTerminal_8wekyb3d8bbwe"),
        packaged("Microsoft.WindowsTerminalPreview_8wekyb3d8bbwe"),
        local
            .join("Microsoft")
            .join("Windows Terminal")
            .join("settings.json"),
    ] {
        let Some(raw) = read_limited(&path, MAX_SCHEME_FILE_SIZE) else {
            continue;
        };
        // 只取 schemes：settings.json 的 profiles 中有命令行（可能带着 plink -pw 的密码）、环境变量等。
        // 解析不了、没有配色的跳过，不返回原文
        if let Some(content) = wt_schemes_json(&decode_with_fallback(&raw, env.ansi)) {
            c.add(SchemeClient::WindowsTerminal, display(&path), content);
        }
    }
}

/// Windows Terminal 配色中取出的键（名称与颜色）；magenta、brightMagenta 是一些移植文件的写法。
const WT_SCHEME_KEYS: [&str; 23] = [
    "name",
    "foreground",
    "background",
    "cursorColor",
    "selectionBackground",
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "purple",
    "magenta",
    "cyan",
    "white",
    "brightBlack",
    "brightRed",
    "brightGreen",
    "brightYellow",
    "brightBlue",
    "brightPurple",
    "brightMagenta",
    "brightCyan",
    "brightWhite",
];

/// 按给定的顺序序列化的键值对。
struct Ordered<'a>(Vec<(&'a str, &'a str)>);

impl Serialize for Ordered<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_map(self.0.iter().copied())
    }
}

/// 从 Windows Terminal 的 `settings.json`（JSONC：可以有注释、尾随逗号）中取出 `schemes`，
/// 每套只保留名称与颜色（字符串），转为 `{"schemes": [...]}`。解析不了或没有配色时返回 `None`。
fn wt_schemes_json(text: &str) -> Option<String> {
    let doc: serde_json::Value = serde_json::from_str(&strip_jsonc(text)).ok()?;
    let list = doc.as_object()?.get("schemes")?.as_array()?;
    let schemes: Vec<Ordered<'_>> = list
        .iter()
        .filter_map(serde_json::Value::as_object)
        .map(|s| {
            Ordered(
                WT_SCHEME_KEYS
                    .iter()
                    .filter_map(|&k| Some((k, s.get(k)?.as_str()?)))
                    .collect(),
            )
        })
        .filter(|o| !o.0.is_empty())
        .collect();
    if schemes.is_empty() {
        return None;
    }
    #[derive(Serialize)]
    struct Doc<'a> {
        schemes: Vec<Ordered<'a>>,
    }
    serde_json::to_string_pretty(&Doc { schemes }).ok()
}

// ---- Xshell ----

/// 用户数据目录下 `Xshell\ColorScheme Files` 中的 `.xcs`（常见为 UTF-16，中文版也可能是 GBK）。
fn collect_xshell(c: &mut Collector, env: &SchemeEnv<'_>) {
    for data in &env.xshell_data_dirs {
        let dir = data.join("Xshell").join("ColorScheme Files");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<(String, PathBuf)> = entries
            .filter_map(Result::ok)
            // 只读目录中的普通文件：不跟随符号链接，防止被引到目录外
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                name.to_lowercase()
                    .ends_with(".xcs")
                    .then(|| (name, e.path()))
            })
            .collect();
        files.sort();
        for (_, path) in files {
            if c.full() {
                break;
            }
            let Some(raw) = read_limited(&path, MAX_SCHEME_FILE_SIZE) else {
                continue;
            };
            c.add(
                SchemeClient::Xshell,
                display(&path),
                decode_with_fallback(&raw, env.ansi),
            );
        }
    }
}

/// NetSarang 用户数据目录的候选（含版本号这一级）：Xshell 选项中自定义的位置（注册表
/// `HKCU\Software\NetSarang\Common\<版本>\UserData` 的 `UserDataPath`），以及"文档"下的默认位置。
/// "文档"可能被重定向到其他磁盘或 OneDrive。
fn xshell_data_dirs(home: &Path) -> Vec<PathBuf> {
    let documents = documents_dir(home);
    let mut dirs: Vec<PathBuf> = Vec::new();
    for version in ["8", "7", "6", "5"] {
        let candidates = [
            platform::xshell_registry_data_path(version),
            Some(documents.join("NetSarang Computer").join(version)),
            Some(
                home.join("Documents")
                    .join("NetSarang Computer")
                    .join(version),
            ),
        ];
        for d in candidates.into_iter().flatten() {
            let key = same_path_key(&display(&d));
            if !dirs.iter().any(|x| same_path_key(&display(x)) == key) {
                dirs.push(d);
            }
        }
    }
    dirs
}

/// "文档"文件夹，它可能被重定向到其他磁盘或 OneDrive。
fn documents_dir(home: &Path) -> PathBuf {
    dirs::document_dir().unwrap_or_else(|| home.join("Documents"))
}

// ---- MobaXterm ----

/// 安装版 `MobaXterm.ini` 可能的位置（"文档\MobaXterm"、`%APPDATA%\MobaXterm`）。
fn mobaxterm_ini_candidates(home: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![
        documents_dir(home).join("MobaXterm").join("MobaXterm.ini"),
        home.join("Documents")
            .join("MobaXterm")
            .join("MobaXterm.ini"),
    ];
    if let Some(dir) = dirs::config_dir() {
        candidates.push(dir.join("MobaXterm").join("MobaXterm.ini"));
    }
    candidates
}

/// `MobaXterm.ini` 的 `[Colors]` 段；没有时读注册表（安装版可能把设置保存在注册表中）。
fn collect_mobaxterm(c: &mut Collector, env: &SchemeEnv<'_>) {
    let mut found = false;
    for path in &env.mobaxterm_inis {
        let Some((raw, file)) = read_config_file(path, "MobaXterm.ini") else {
            continue;
        };
        if let Some(colors) = ini_section(&decode_with_fallback(&raw, env.ansi), "Colors") {
            c.add(SchemeClient::MobaXterm, display(&file), colors);
            found = true;
        }
    }
    if found {
        return;
    }
    if let Some(colors) = env.mobaxterm_registry.as_ref().and_then(|read| read()) {
        c.add(
            SchemeClient::MobaXterm,
            MOBAXTERM_REGISTRY_PATH.to_string(),
            colors,
        );
    }
}

/// 取出 INI 中的一段（段名不区分大小写），连同段头返回；没有这一段或段中没有键值时返回 `None`。
fn ini_section(text: &str, name: &str) -> Option<String> {
    let mut out = String::new();
    let (mut inside, mut keys) = (false, false);
    let lower = name.to_lowercase();
    for line in text.split('\n') {
        let t = line.trim();
        if t.len() >= 2 && t.starts_with('[') && t.ends_with(']') {
            if inside {
                break;
            }
            inside = t[1..t.len() - 1].trim().to_lowercase() == lower;
            if inside {
                out.push_str(&format!("[{name}]\n"));
            }
            continue;
        }
        if inside && !t.is_empty() {
            out.push_str(t);
            out.push('\n');
            keys = keys || t.contains('=');
        }
    }
    keys.then_some(out)
}

// ---- Tabby ----

/// `config.yaml` 中的 customColorSchemes、当前的深色 / 浅色配色与各 profile 的配色（旧版名为 Terminus）。
fn collect_tabby(c: &mut Collector, env: &SchemeEnv<'_>) {
    let Some(config) = &env.config_dir else {
        return;
    };
    for name in ["tabby", "terminus"] {
        let path = config.join(name).join("config.yaml");
        let Some(raw) = read_limited(&path, MAX_CONFIG_SIZE) else {
            continue;
        };
        if let Some(content) = tabby_schemes_json(&decode_with_fallback(&raw, env.ansi)) {
            c.add(SchemeClient::Tabby, display(&path), content);
        }
    }
}

/// 从 Tabby 的 `config.yaml` 中取出配色相关的部分，转为 JSON。YAML 写错时返回 `None`；个别字段类型不符时
/// 只跳过这些字段。
fn tabby_schemes_json(text: &str) -> Option<String> {
    // 不是字符串的键转为字符串；合并键、锚点与别名已展开；带标签的取其中的值
    let doc = crate::yaml::to_json(text).ok()?;
    let present = |v: Option<&serde_json::Value>| v.filter(|v| !v.is_null()).cloned();
    let terminal = doc.get("terminal").filter(|t| t.is_object());
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Terminal {
        #[serde(skip_serializing_if = "Option::is_none")]
        color_scheme: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        light_color_scheme: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        custom_color_schemes: Option<serde_json::Value>,
    }
    let out_terminal = Terminal {
        color_scheme: present(terminal.and_then(|t| t.get("colorScheme"))),
        light_color_scheme: present(terminal.and_then(|t| t.get("lightColorScheme"))),
        custom_color_schemes: terminal
            .and_then(|t| t.get("customColorSchemes"))
            .and_then(serde_json::Value::as_array)
            .filter(|s| !s.is_empty())
            .map(|s| s.clone().into()),
    };
    let profiles: Vec<serde_json::Value> = doc
        .get("profiles")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|p| p.is_object())
        .filter_map(|p| present(p.get("terminalColorScheme")))
        .map(|cs| serde_json::json!({ "terminalColorScheme": cs }))
        .collect();
    let empty_terminal = out_terminal.color_scheme.is_none()
        && out_terminal.light_color_scheme.is_none()
        && out_terminal.custom_color_schemes.is_none();
    if empty_terminal && profiles.is_empty() {
        return None;
    }
    #[derive(Serialize)]
    struct Doc {
        terminal: Terminal,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        profiles: Vec<serde_json::Value>,
    }
    serde_json::to_string_pretty(&Doc {
        terminal: out_terminal,
        profiles,
    })
    .ok()
}

// ---- 计数 ----

/// 一个配色文件中有几套用户自己的配色（"导入主机"时提示可以一起导入）：MobaXterm 的 `[Colors]` 是自带的配色时
/// 不算（`MobaXterm.ini` 总有这一段）；Tabby 只算 customColorSchemes，当前所用的配色、各 profile 的配色多是
/// Tabby 自带的。其余客户端按解析出的配色计数。
pub fn custom_scheme_count(source: &InstalledSchemeSource) -> usize {
    match source.client {
        SchemeClient::MobaXterm if is_mobaxterm_builtin(&source.content) => 0,
        SchemeClient::Tabby => {
            // 内容是 tabby_schemes_json 取出的 JSON：{ terminal: { colorScheme, lightColorScheme, customColorSchemes }, profiles }
            let Ok(doc) = serde_json::from_str::<serde_json::Value>(&source.content) else {
                return 0;
            };
            let custom = doc
                .get("terminal")
                .and_then(|t| t.get("customColorSchemes"))
                .and_then(serde_json::Value::as_array)
                .filter(|c| !c.is_empty());
            let Some(custom) = custom else {
                return 0;
            };
            let only = serde_json::json!({ "terminal": { "customColorSchemes": custom } });
            parse_schemes(&only.to_string(), Some(&source.path))
                .schemes
                .len()
        }
        _ => parse_schemes(&source.content, Some(&source.path))
            .schemes
            .len(),
    }
}

// ---- 平台相关 ----

#[cfg(windows)]
mod platform {
    use std::path::PathBuf;

    use encoding_rs::Encoding;
    use windows_registry::{CURRENT_USER, LOCAL_MACHINE, Type};

    /// Xshell 选项中设置的用户数据目录（`HKCU\Software\NetSarang\Common\<版本>\UserData` 的 `UserDataPath`）。
    pub fn xshell_registry_data_path(version: &str) -> Option<PathBuf> {
        let key = CURRENT_USER
            .open(format!(r"Software\NetSarang\Common\{version}\UserData"))
            .ok()?;
        let value = key.get_value("UserDataPath").ok()?;
        let expand = value.ty() == Type::ExpandString;
        let s = String::try_from(value).ok()?;
        let s = if expand { super::expand_env(&s) } else { s };
        let s = s.trim();
        (!s.is_empty()).then(|| PathBuf::from(s))
    }

    /// 安装版 MobaXterm 保存在注册表中的 `Colors` 子键，组织成与 `MobaXterm.ini` 相同的 `[Colors]` 段。只读这一个子键。
    pub fn read_mobaxterm_registry_colors() -> Option<String> {
        let key = CURRENT_USER
            .open(r"Software\Mobatek\MobaXterm\Colors")
            .ok()?;
        let mut values: Vec<(String, Option<String>)> = key
            .values()
            .ok()?
            .map(|(name, value)| (name, String::try_from(value).ok()))
            .collect();
        if values.is_empty() {
            return None;
        }
        values.sort_by(|a, b| a.0.cmp(&b.0));
        let mut out = String::from("[Colors]\n");
        for (name, value) in values {
            if let Some(value) = value {
                out.push_str(&format!("{name}={value}\n"));
            }
        }
        Some(out)
    }

    /// 系统的 ANSI 代码页对应的编码（MobaXterm 等老程序按它保存配置文件，中文 Windows 上为 GBK）。
    pub fn ansi_encoding() -> &'static Encoding {
        let code_page = LOCAL_MACHINE
            .open(r"SYSTEM\CurrentControlSet\Control\Nls\CodePage")
            .and_then(|k| k.get_string("ACP"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(936);
        super::encoding_for_code_page(code_page)
    }
}

#[cfg(not(windows))]
mod platform {
    use std::path::PathBuf;

    use encoding_rs::{Encoding, GB18030};

    /// 非 Windows 系统没有注册表：Xshell、安装版 MobaXterm 的注册表设置不存在。
    pub fn xshell_registry_data_path(_version: &str) -> Option<PathBuf> {
        None
    }

    pub fn read_mobaxterm_registry_colors() -> Option<String> {
        None
    }

    /// 无从得知配置文件来自哪个代码页，按 GB18030（GBK 的超集）处理。
    pub fn ansi_encoding() -> &'static Encoding {
        GB18030
    }
}

/// Windows 代码页对应的编码；不认识的代码页按 GB18030（GBK 的超集）处理。
#[cfg_attr(not(windows), allow(dead_code))]
fn encoding_for_code_page(code_page: u32) -> &'static Encoding {
    use encoding_rs::*;
    match code_page {
        874 => WINDOWS_874,
        932 => SHIFT_JIS,
        949 => EUC_KR,
        950 => BIG5,
        1250 => WINDOWS_1250,
        1251 => WINDOWS_1251,
        1252 => WINDOWS_1252,
        1253 => WINDOWS_1253,
        1254 => WINDOWS_1254,
        1255 => WINDOWS_1255,
        1256 => WINDOWS_1256,
        1257 => WINDOWS_1257,
        1258 => WINDOWS_1258,
        _ => GB18030,
    }
}

/// 展开 `%NAME%` 形式的环境变量；没有的变量原样保留（与 Windows 的 ExpandEnvironmentStrings 相同）。
#[cfg_attr(not(windows), allow(dead_code))]
fn expand_env(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => {
                let name = &after[..end];
                match std::env::var(name).ok().filter(|_| !name.is_empty()) {
                    Some(value) => out.push_str(&value),
                    None => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests;
