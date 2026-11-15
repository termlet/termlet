//! SFTP 文件列表的彩色文件图标：Catppuccin vscode-icons（MIT，许可见 `assets/file-icons/LICENSE`）。
//!
//! 只打包 Mocha（深色）风味的 SVG（`assets/file-icons/mocha.txt`，每行 `名称<TAB>SVG`），
//! Latte（浅色）在首次用到时按调色板换色得到。名称到图标的映射见 [`super::file_icons_table`]；
//! 两者都由 `scripts/file-icons.py` 生成，生成时已核对换色结果与 Latte 原文件一致。

use std::borrow::Cow;
use std::sync::LazyLock;

use gpui_kit::SharedString;

use super::file_icons_table::{
    DEFAULT_FILE, DEFAULT_FOLDER, FILE_EXTENSIONS, FILE_NAMES, FOLDER_NAMES, PALETTE_LATTE,
};

/// 浅色界面的图标路径前缀。
const LATTE_DIR: &str = "file-icons/latte/";
/// 深色界面的图标路径前缀。
const MOCHA_DIR: &str = "file-icons/mocha/";

/// 打包的 Mocha 图标：每行 `名称<TAB>压成一行的 SVG`，按名称排序。
const MOCHA_DATA: &str = include_str!("../../assets/file-icons/mocha.txt");

/// 备份留下的后缀：去掉后按原来的名称找图标（全数字的日志轮转编号另外判断）。
const BACKUP_SUFFIXES: &[&str] = &[
    "bak",
    "old",
    "orig",
    "rpmnew",
    "rpmsave",
    "dpkg-dist",
    "dpkg-new",
    "dpkg-old",
];

/// 列表中一行是什么（决定图标）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IconKind {
    /// 返回上一级（“..”）
    Parent,
    Folder,
    File,
}

/// 一个打包的图标。
struct Icon {
    name: &'static str,
    mocha: &'static str,
    latte_path: SharedString,
    mocha_path: SharedString,
}

/// 打包的图标，按名称排序；首次用到时解析。
static ICONS: LazyLock<Vec<Icon>> = LazyLock::new(|| {
    MOCHA_DATA
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(name, mocha)| Icon {
            name,
            mocha,
            latte_path: format!("{LATTE_DIR}{name}.svg").into(),
            mocha_path: format!("{MOCHA_DIR}{name}.svg").into(),
        })
        .collect()
});

/// 换色后的 Latte 图标，与 [`ICONS`] 一一对应；首次加载浅色图标时一起换好。
static LATTE: LazyLock<Vec<String>> =
    LazyLock::new(|| ICONS.iter().map(|icon| to_latte(icon.mocha)).collect());

/// 彩色图标的资源路径：浅色界面 `file-icons/latte/<图标>.svg`，深色 `file-icons/mocha/<图标>.svg`。
pub fn color_icon_path(name: &str, kind: IconKind, dark: bool) -> SharedString {
    let Some(i) = icon_index(icon_name(name, kind)) else {
        // 打包数据与表由同一个脚本生成，测试保证表中的图标都已打包
        return SharedString::default();
    };
    if dark {
        ICONS[i].mocha_path.clone()
    } else {
        ICONS[i].latte_path.clone()
    }
}

/// 资源：`file-icons/` 下的路径由这里提供（其他路径返回 None / 空）。
pub fn load_asset(path: &str) -> Option<Cow<'static, [u8]>> {
    if let Some(name) = icon_file(path, MOCHA_DIR) {
        let i = icon_index(name)?;
        return Some(Cow::Borrowed(ICONS[i].mocha.as_bytes()));
    }
    let i = icon_index(icon_file(path, LATTE_DIR)?)?;
    Some(Cow::Borrowed(LATTE[i].as_bytes()))
}

/// `file-icons/` 下以 `path` 开头的资源路径。
pub fn list_assets(path: &str) -> Vec<SharedString> {
    if !(path.starts_with("file-icons/") || "file-icons/".starts_with(path)) {
        return Vec::new();
    }
    ICONS
        .iter()
        .flat_map(|icon| [&icon.latte_path, &icon.mocha_path])
        .filter(|p| p.starts_with(path))
        .cloned()
        .collect()
}

/// 资源路径中的图标名：`<目录><图标>.svg`。
fn icon_file<'a>(path: &'a str, dir: &str) -> Option<&'a str> {
    path.strip_prefix(dir)?.strip_suffix(".svg")
}

fn icon_index(name: &str) -> Option<usize> {
    ICONS.binary_search_by(|icon| icon.name.cmp(name)).ok()
}

fn default_icon(kind: IconKind) -> &'static str {
    match kind {
        IconKind::Parent | IconKind::Folder => DEFAULT_FOLDER,
        IconKind::File => DEFAULT_FILE,
    }
}

/// 名称对应的图标名（大小写不敏感），没有对应的用默认的文件夹 / 文件图标。
fn icon_name(name: &str, kind: IconKind) -> &'static str {
    let found = match kind {
        IconKind::Parent => None,
        IconKind::Folder => lookup(FOLDER_NAMES, &name.to_lowercase()),
        IconKind::File => file_icon(&name.to_lowercase()),
    };
    found.unwrap_or_else(|| default_icon(kind))
}

/// 文件的图标：先按完整文件名，再按扩展名从长到短（`backup.tar.gz` 先试 `tar.gz` 再试 `gz`，
/// `.bashrc` 整体是文件名、扩展名是 `bashrc`）；都没有时去掉末尾的日志轮转编号或备份后缀再找
/// （`app.log.1` 按 `app.log`，`nginx.conf.bak` 按 `nginx.conf`）。`name` 已是小写。
fn file_icon(name: &str) -> Option<&'static str> {
    let mut name = name;
    loop {
        let found = lookup(FILE_NAMES, name).or_else(|| {
            name.match_indices('.')
                .find_map(|(i, _)| lookup(FILE_EXTENSIONS, &name[i + 1..]))
        });
        if found.is_some() {
            return found;
        }
        let (stem, suffix) = name.rsplit_once('.')?;
        if stem.is_empty() || !is_backup_suffix(suffix) {
            return None;
        }
        name = stem;
    }
}

fn is_backup_suffix(suffix: &str) -> bool {
    (!suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()))
        || BACKUP_SUFFIXES.contains(&suffix)
}

/// 在按键排序的表中二分查找。
fn lookup(table: &'static [(&'static str, &'static str)], key: &str) -> Option<&'static str> {
    table
        .binary_search_by(|(k, _)| (*k).cmp(key))
        .ok()
        .map(|i| table[i].1)
}

/// Mocha 的 SVG 换成 Latte：`#rrggbb` 形式的颜色按调色板替换，其他内容不变。
fn to_latte(svg: &str) -> String {
    let mut out = String::with_capacity(svg.len());
    let mut rest = svg;
    while let Some(i) = rest.find('#') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let latte = tail
            .get(..7)
            .filter(|_| !tail[7..].starts_with(|c: char| c.is_ascii_alphanumeric()))
            .and_then(|color| lookup(PALETTE_LATTE, color));
        match latte {
            Some(latte) => {
                out.push_str(latte);
                rest = &tail[7..];
            }
            None => {
                out.push('#');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui_kit::SvgRenderer;

    use super::*;

    fn file(name: &str) -> &'static str {
        icon_name(name, IconKind::File)
    }

    fn folder(name: &str) -> &'static str {
        icon_name(name, IconKind::Folder)
    }

    #[test]
    fn matches_file_names_and_extensions() {
        assert_eq!(file("Dockerfile"), "docker");
        assert_eq!(file("docker-compose.yml"), "docker-compose");
        assert_eq!(file("deploy.sh"), "bash");
        assert_eq!(file("config.yaml"), "yaml");
        assert_eq!(file("nginx.conf"), "nginx");
        assert_eq!(file("README.md"), "readme");
        assert_eq!(file("notes.MD"), "markdown");
        assert_eq!(file("resolv.conf"), "properties");
    }

    #[test]
    fn tries_longer_extensions_first() {
        // tar.gz 没有单独的图标，按 gz
        assert_eq!(file("backup.tar.gz"), "zip");
        // tf.json 比 json 更长，先匹配
        assert_eq!(file("main.tf.json"), "terraform");
    }

    #[test]
    fn strips_rotation_numbers_and_backup_suffixes() {
        assert_eq!(file("app.log.1"), "log");
        assert_eq!(file("app.log.12"), "log");
        assert_eq!(file("nginx.conf.bak"), "nginx");
        assert_eq!(file("known_hosts.old"), "key");
        assert_eq!(file("sshd_config.rpmnew"), "security");
        assert_eq!(file("libssl.so.3"), "lib");
        // 只有后缀、去掉后什么都不剩的不算
        assert_eq!(file(".bak"), DEFAULT_FILE);
        assert_eq!(file("data.1"), DEFAULT_FILE);
    }

    #[test]
    fn covers_common_server_files() {
        assert_eq!(file("id_ed25519"), "key");
        assert_eq!(file("id_ed25519.pub"), "key");
        assert_eq!(file("authorized_keys"), "key");
        assert_eq!(file(".bashrc"), "bash");
        assert_eq!(file(".zshrc"), "bash");
        assert_eq!(file("sshd_config"), "security");
        assert_eq!(file("fstab"), "properties");
        assert_eq!(file("nginx.service"), "properties");
        assert_eq!(file("package.deb"), "zip");
    }

    #[test]
    fn folders_and_parent() {
        assert_eq!(folder(".ssh"), "folder_security");
        assert_eq!(folder("node_modules"), "folder_node");
        assert_eq!(folder("SRC"), "folder_src");
        assert_eq!(folder("no-such-folder"), DEFAULT_FOLDER);
        assert_eq!(icon_name("..", IconKind::Parent), DEFAULT_FOLDER);
        // 文件夹名不按文件的规则找
        assert_eq!(folder("deploy.sh"), DEFAULT_FOLDER);
    }

    #[test]
    fn unknown_files_use_default_icon() {
        assert_eq!(file("unknown-file"), DEFAULT_FILE);
        assert_eq!(file("data.unknownext"), DEFAULT_FILE);
        assert_eq!(file(""), DEFAULT_FILE);
        assert_eq!(file("trailing."), DEFAULT_FILE);
    }

    #[test]
    fn path_follows_light_or_dark() {
        assert_eq!(
            color_icon_path("deploy.sh", IconKind::File, true).as_ref(),
            "file-icons/mocha/bash.svg"
        );
        assert_eq!(
            color_icon_path("deploy.sh", IconKind::File, false).as_ref(),
            "file-icons/latte/bash.svg"
        );
        assert_eq!(
            color_icon_path("..", IconKind::Parent, false).as_ref(),
            "file-icons/latte/_folder.svg"
        );
    }

    #[test]
    fn tables_sorted_and_icons_bundled() {
        let tables = [FILE_NAMES, FILE_EXTENSIONS, FOLDER_NAMES, PALETTE_LATTE];
        for table in tables {
            assert!(table.windows(2).all(|w| w[0].0 < w[1].0), "表未按键排序");
        }
        assert!(
            ICONS.windows(2).all(|w| w[0].name < w[1].name),
            "图标未按名称排序"
        );
        assert_eq!(ICONS.len(), MOCHA_DATA.lines().count());
        for (key, icon) in FILE_NAMES.iter().chain(FILE_EXTENSIONS).chain(FOLDER_NAMES) {
            assert_eq!(*key, key.to_lowercase(), "键应为小写：{key}");
            assert!(
                icon_index(icon).is_some(),
                "{key} 引用的图标 {icon} 没有打包"
            );
        }
        assert!(icon_index(DEFAULT_FILE).is_some());
        assert!(icon_index(DEFAULT_FOLDER).is_some());
    }

    #[test]
    fn latte_differs_from_mocha() {
        for name in ["bash", "_file", "_folder", "folder_security"] {
            let mocha = load_asset(&format!("file-icons/mocha/{name}.svg")).unwrap();
            let latte = load_asset(&format!("file-icons/latte/{name}.svg")).unwrap();
            assert!(mocha.starts_with(b"<svg"), "{name}");
            assert!(latte.starts_with(b"<svg"), "{name}");
            assert_ne!(mocha, latte, "{name}");
        }
        let latte = load_asset("file-icons/latte/bash.svg").unwrap();
        let latte = std::str::from_utf8(&latte).unwrap();
        assert!(latte.contains("#40a02b") && !latte.contains("#a6e3a1"));
    }

    #[test]
    fn recolor_only_touches_palette_colors() {
        assert_eq!(
            to_latte(r##"<path stroke="#cdd6f4" fill="#fff" clip-path="url(#a)"/>"##),
            r##"<path stroke="#4c4f69" fill="#fff" clip-path="url(#a)"/>"##
        );
        // 8 位颜色不是调色板中的颜色，不换
        assert_eq!(to_latte("#cdd6f480"), "#cdd6f480");
        assert_eq!(to_latte("#"), "#");
    }

    #[test]
    fn listed_assets_all_load() {
        let all = list_assets("file-icons/");
        assert_eq!(all.len(), ICONS.len() * 2);
        for path in &all {
            let data = load_asset(path).unwrap_or_else(|| panic!("加载不了 {path}"));
            assert!(
                data.starts_with(b"<svg") && data.ends_with(b"</svg>"),
                "{path}"
            );
        }
        assert_eq!(list_assets("file-icons/mocha/").len(), ICONS.len());
        assert_eq!(list_assets("").len(), ICONS.len() * 2);
        assert!(list_assets("icons/").is_empty());
        assert!(load_asset("icons/folder.svg").is_none());
        assert!(load_asset("file-icons/mocha/no-such-icon.svg").is_none());
        assert!(load_asset("file-icons/mocha/bash").is_none());
    }

    #[test]
    fn bundled_svgs_render() {
        // 用 GPUI 的 SVG 渲染器（img 元素用的同一个）逐个渲染：压成一行、换色后仍能解析，且画出了内容
        let renderer = SvgRenderer::new(Arc::new(crate::assets::AppAssets));
        for path in list_assets("file-icons/") {
            let data = load_asset(&path).unwrap();
            let image = renderer
                .render_single_frame(&data, 1.0)
                .unwrap_or_else(|e| panic!("{path} 渲染失败：{e}"));
            let pixels = image.as_bytes(0).unwrap();
            assert!(
                pixels.as_chunks::<4>().0.iter().any(|p| p[3] != 0),
                "{path} 渲染出来是空的"
            );
        }
    }
}
