//! 这台电脑上装了哪些编辑器：系统能把它们的链接（`vscode://`、`zed://`）交给它们打开才算。
//!
//! - Windows：注册表中有这个 URL 协议（`HKEY_CLASSES_ROOT\<协议>`，含“URL Protocol”值），并且
//!   `shell\open\command` 中的程序还在（卸载后残留的注册项不算）。
//! - macOS：`/Applications` 或 `~/Applications` 中有应用包（`Visual Studio Code.app`；`Zed.app`、`Zed Preview.app`）。
//! - Linux：XDG 数据目录的 `applications` 中有声明了 `x-scheme-handler/<协议>` 的桌面文件（`xdg-open` 据此打开链接），
//!   含 Flatpak、Snap 导出的。
//!
//! 读文件与注册表，在后台线程中调用。

use std::path::{Path, PathBuf};

use super::Editor;

/// 装了的编辑器（按 [`Editor::ALL`] 的顺序）。
pub fn installed() -> Vec<Editor> {
    Editor::ALL
        .into_iter()
        .filter(|editor| is_installed(*editor))
        .collect()
}

#[cfg(windows)]
fn is_installed(editor: Editor) -> bool {
    let scheme = editor.scheme();
    let root = windows_registry::CLASSES_ROOT;
    let Ok(key) = root.open(scheme) else {
        return false;
    };
    if key.get_value("URL Protocol").is_err() {
        return false;
    }
    root.open(format!(r"{scheme}\shell\open\command"))
        .and_then(|command| command.get_string(""))
        .ok()
        .and_then(|command| command_program(&command))
        .is_some_and(|program| program.is_file())
}

#[cfg(target_os = "macos")]
fn is_installed(editor: Editor) -> bool {
    let mut roots = vec![PathBuf::from("/Applications")];
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join("Applications"));
    }
    has_app_bundle(&roots, mac_bundles(editor))
}

#[cfg(not(any(windows, target_os = "macos")))]
fn is_installed(editor: Editor) -> bool {
    has_scheme_handler(&xdg_application_dirs(), editor.scheme())
}

/// 注册表中打开命令的程序：开头带引号的取引号中的，否则取到第一个空白为止。
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(super) fn command_program(command: &str) -> Option<PathBuf> {
    let command = command.trim();
    let program = match command.strip_prefix('"') {
        Some(rest) => rest.split('"').next()?,
        None => command.split_whitespace().next()?,
    };
    (!program.is_empty()).then(|| PathBuf::from(program))
}

/// macOS 上编辑器的应用包名。
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
pub(super) fn mac_bundles(editor: Editor) -> &'static [&'static str] {
    match editor {
        Editor::VsCode => &["Visual Studio Code.app"],
        Editor::Zed => &["Zed.app", "Zed Preview.app"],
    }
}

/// `roots` 中有没有 `bundles` 之一。
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
pub(super) fn has_app_bundle(roots: &[PathBuf], bundles: &[&str]) -> bool {
    roots
        .iter()
        .any(|root| bundles.iter().any(|bundle| root.join(bundle).is_dir()))
}

/// XDG 的 `applications` 目录：`$XDG_DATA_HOME`（默认 `~/.local/share`）、`$XDG_DATA_DIRS`（默认
/// `/usr/local/share:/usr/share`），加上 Flatpak、Snap 导出桌面文件的目录。
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn xdg_application_dirs() -> Vec<PathBuf> {
    let home = dirs::home_dir();
    let mut dirs: Vec<PathBuf> = Vec::new();
    match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(data_home) => dirs.push(PathBuf::from(data_home)),
        None => {
            if let Some(home) = &home {
                dirs.push(home.join(".local/share"));
            }
        }
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    dirs.extend(
        data_dirs
            .split(':')
            .filter(|d| !d.is_empty())
            .map(PathBuf::from),
    );
    if let Some(home) = &home {
        dirs.push(home.join(".local/share/flatpak/exports/share"));
    }
    dirs.push(PathBuf::from("/var/lib/flatpak/exports/share"));
    let mut out: Vec<PathBuf> = dirs.into_iter().map(|d| d.join("applications")).collect();
    out.push(PathBuf::from("/var/lib/snapd/desktop/applications"));
    out
}

/// `dirs` 中有没有声明了 `x-scheme-handler/<scheme>` 的桌面文件。
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
pub(super) fn has_scheme_handler(dirs: &[PathBuf], scheme: &str) -> bool {
    let wanted = format!("x-scheme-handler/{scheme}");
    dirs.iter().any(|dir| {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        entries.flatten().any(|entry| {
            let path = entry.path();
            path.extension().is_some_and(|e| e == "desktop") && declares(&path, &wanted)
        })
    })
}

/// 桌面文件的 `MimeType=` 中有 `wanted`。
fn declares(path: &Path, wanted: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    text.lines().any(|line| {
        line.trim()
            .strip_prefix("MimeType=")
            .is_some_and(|types| types.split(';').any(|t| t.trim() == wanted))
    })
}
