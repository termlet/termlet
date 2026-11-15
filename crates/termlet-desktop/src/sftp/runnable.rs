//! 用默认程序打开时会直接运行的文件（打开前先确认）：从服务器下载的程序双击就会运行，文件名还可能用 RLO
//! 等字符伪装扩展名。

/// 打开即运行（或交给解释器、安装程序执行）的文件类型。按 Windows 附件管理器与浏览器下载时视为危险的类型整理，
/// 多列一些只是多问一次。Windows 上另加 `PATHEXT` 中的类型。
#[rustfmt::skip]
const RUNNABLE: &[&str] = &[
    "exe", "com", "bat", "cmd", "btm", "pif", "scr", "cpl", "msc", "lnk", "url",
    "msi", "msp", "mst", "msix", "msixbundle", "appx", "appxbundle", "appinstaller",
    "application", "appref-ms", "xbap", "gadget", "hta", "chm", "hlp", "jar",
    "ps1", "psm1", "psd1", "ps1xml", "psc1", "vb", "vbs", "vbe", "js", "jse",
    "ws", "wsf", "wsc", "wsh", "sct", "shb", "shs", "reg", "inf", "ins", "scf",
    "settingcontent-ms", "diagcab", "library-ms", "search-ms", "searchconnector-ms",
    "py", "pyw", "pyz", "pyzw", "pyc", "pyo", "pl", "rb", "xll", "ade", "adp",
    // AutoHotkey、AutoIt 脚本，Java Web Start，Windows 沙盒配置，远程桌面连接，主题包（可指向远程资源）
    "ahk", "au3", "jnlp", "wsb", "rdp", "theme", "themepack", "deskthemepack",
    // Windows 附件管理器视为高风险的其余类型：Access 数据库与快捷方式、更新包、旧版 PowerShell 等
    "mde", "mda", "mdb", "mdt", "mdw", "mdz", "maf", "mag", "mam", "maq", "mar",
    "mas", "mat", "mav", "maw", "msu", "msh", "msh1", "msh2", "mshxml", "msh1xml",
    "msh2xml", "ps2", "ps2xml", "psc2", "isp", "its", "mcf", "website", "xnk",
    "vsmacros", "vsw", "prf", "printerexport", "webpnp", "bas", "csh", "ksh", "grp",
    // macOS、Linux
    "app", "command", "tool", "terminal", "pkg", "mpkg", "sh", "run", "appimage", "desktop",
];

/// 名称的扩展名（小写，不含点）。Windows 打开文件时忽略名称末尾的点与空格（`a.exe.` 打开的就是 `a.exe`）。
fn extension(name: &str) -> Option<String> {
    let name = if cfg!(windows) {
        name.trim_end_matches(['.', ' '])
    } else {
        name
    };
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty() && !ext.is_empty()).then(|| ext.to_lowercase())
}

/// 用默认程序打开这个文件会不会直接运行它。`pathext` 为 Windows 的 `PATHEXT`。
pub fn is_runnable(name: &str, pathext: Option<&str>) -> bool {
    let Some(ext) = extension(name) else {
        return false;
    };
    if RUNNABLE.contains(&ext.as_str()) {
        return true;
    }
    pathext.is_some_and(|list| {
        list.split(';')
            .map(|x| x.trim().trim_start_matches('.').to_lowercase())
            .any(|x| x == ext)
    })
}

/// 当前系统的 `PATHEXT`（只有 Windows 有）。
pub fn system_pathext() -> Option<String> {
    if cfg!(windows) {
        std::env::var("PATHEXT").ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn programs_and_scripts_are_runnable() {
        for name in [
            "a.exe", "A.EXE", "x.bat", "x.cmd", "x.lnk", "x.url", "x.hta", "x.ps1",
        ] {
            assert!(is_runnable(name, None), "{name}");
        }
        for name in ["a.txt", "readme", ".bashrc", "photo.png", "archive.tar.gz"] {
            assert!(!is_runnable(name, None), "{name}");
        }
    }

    #[test]
    fn pathext_adds_types() {
        assert!(!is_runnable("tool.xyz", None));
        assert!(is_runnable("tool.xyz", Some(".COM;.EXE;.XYZ")));
    }

    #[cfg(windows)]
    #[test]
    fn trailing_dots_and_spaces_are_ignored_on_windows() {
        assert!(is_runnable("a.exe.", None));
        assert!(is_runnable("a.exe . ", None));
    }
}
