//! 本机路径参数的 WSL 写法：在 WSL 中运行 Windows 上的 `termlet.exe` 时，AI 工具与用户写的是 WSL 中的路径
//! （`/home/u/x`、`/mnt/c/x`），Windows 会把以 `/` 开头的路径接在当前盘的根上（`C:\home\u\x`），悄悄读写别处。
//!
//! 只在 Windows 上换算，只管以单个 `/` 开头的参数（以 `\`、盘符、`//` 开头的与相对路径照 Windows 的规则）：
//!
//! - `/mnt/<盘符>/…` 是 WSL 挂载的 Windows 盘，换成 `<盘符>:\…`；
//! - 当前目录在 WSL 中（`\\wsl.localhost\<发行版>\…`、`\\wsl$\<发行版>\…`）时，其余的接在这个发行版的根上；
//! - 当前目录在 Windows 的盘上时不知道是哪个发行版，报错并说明怎样写，不当成当前盘上的路径。
//!
//! 当前目录：命令行工具与 `termlet mcp` 是调用方的（AI 工具在 WSL 中的哪个目录启动它）；应用内 HTTP 的 MCP 是
//! Termlet 的程序目录（命令行工具拉起它时也这样），不在 WSL 中，所以经 HTTP 只换算 `/mnt/<盘符>/…`。
//!
//! 换算前按 Linux 的规则去掉 `.` 与 `..`（`/mnt/c/../d/x` 是 `/mnt/d/x`），不解析符号链接。

use std::path::Path;

use termlet_proto::Lang;

use crate::text::{MCP_WSL_PATH, WSL_PATH};

/// 命令行工具的本机路径参数换成本进程能打开的写法（见模块说明）；不需要换算时原样返回。换不了时为给人看的说明
/// （相对路径、先进入 WSL 中的目录，或 `"$(wslpath -w …)"`）。
pub fn from_wsl(path: &str, lang: Lang) -> Result<String, String> {
    resolve(path).ok_or_else(|| WSL_PATH.fill(lang, &[&path, &path]))
}

/// MCP 的 `local_path`：同上，换不了时的说明只给绝对路径的写法（MCP 只收绝对路径，经 HTTP 时也谈不上进入目录）。
pub fn from_wsl_for_mcp(path: &str, lang: Lang) -> Result<String, String> {
    resolve(path).ok_or_else(|| MCP_WSL_PATH.fill(lang, &[&path, &path]))
}

/// 只在 Windows 上换算；`None` 是当前目录不在 WSL 中时的 WSL 绝对路径（不知道是哪个发行版）。
fn resolve(path: &str) -> Option<String> {
    if !cfg!(windows) {
        return Some(path.to_string());
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    match convert(path, &cwd) {
        Converted::Unchanged => Some(path.to_string()),
        Converted::Windows(path) => Some(path),
        Converted::UnknownDistro => None,
    }
}

/// [`convert`] 的结果。
#[derive(Debug, PartialEq, Eq)]
pub enum Converted {
    /// 不是 WSL 中的写法
    Unchanged,
    /// 换成的 Windows 路径
    Windows(String),
    /// WSL 中的绝对路径，但当前目录不在 WSL 中，不知道是哪个发行版
    UnknownDistro,
}

/// 换算 `path`（当前目录是 `cwd`）。规则与系统无关，各系统都能测；只在 Windows 上使用（[`from_wsl`]）。
pub fn convert(path: &str, cwd: &Path) -> Converted {
    let Some(rest) = path.strip_prefix('/') else {
        return Converted::Unchanged;
    };
    if rest.starts_with('/') {
        return Converted::Unchanged;
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in rest.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    // 结尾的 / 保留（下载时表示目录）
    let slash = if rest.ends_with('/') && !parts.is_empty() {
        "\\"
    } else {
        ""
    };
    if let ["mnt", drive, tail @ ..] = parts.as_slice()
        && let [letter] = drive.as_bytes()
        && letter.is_ascii_alphabetic()
    {
        let letter = char::from(letter.to_ascii_uppercase());
        let tail = tail.join("\\");
        let slash = if tail.is_empty() { "" } else { slash };
        return Converted::Windows(format!("{letter}:\\{tail}{slash}"));
    }
    match distro_root(&cwd.to_string_lossy()) {
        Some(root) => Converted::Windows(format!("{root}\\{}{slash}", parts.join("\\"))),
        None => Converted::UnknownDistro,
    }
}

/// 当前目录在 WSL 中时，它所在发行版的根（`\\wsl.localhost\Ubuntu`，沿用当前目录的写法）。
fn distro_root(cwd: &str) -> Option<String> {
    let unc = cwd
        .strip_prefix(r"\\?\UNC\")
        .or_else(|| cwd.strip_prefix(r"\\"))?;
    let mut parts = unc.split('\\');
    let server = parts.next()?;
    let distro = parts.next().filter(|d| !d.is_empty())?;
    (server.eq_ignore_ascii_case("wsl.localhost") || server.eq_ignore_ascii_case("wsl$"))
        .then(|| format!(r"\\{server}\{distro}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn windows(path: &str) -> Converted {
        Converted::Windows(path.to_string())
    }

    #[test]
    fn mounted_drives() {
        let cwd = Path::new(r"C:\work");
        assert_eq!(convert("/mnt/c/x", cwd), windows(r"C:\x"));
        assert_eq!(convert("/mnt/D/x/y.txt", cwd), windows(r"D:\x\y.txt"));
        assert_eq!(convert("/mnt/c", cwd), windows(r"C:\"));
        assert_eq!(convert("/mnt/c/", cwd), windows(r"C:\"));
        assert_eq!(convert("/mnt/c/dl/", cwd), windows(r"C:\dl\"));
        assert_eq!(
            convert("/mnt/c/中文 目录/a b.txt", cwd),
            windows(r"C:\中文 目录\a b.txt")
        );
        // 当前目录在 WSL 中也一样
        let wsl = Path::new(r"\\wsl.localhost\Ubuntu\home\u");
        assert_eq!(convert("/mnt/c/x", wsl), windows(r"C:\x"));
    }

    #[test]
    fn paths_inside_the_current_distro() {
        for (cwd, root) in [
            (r"\\wsl.localhost\Ubuntu\home\u", r"\\wsl.localhost\Ubuntu"),
            (r"\\wsl$\Ubuntu\home\u", r"\\wsl$\Ubuntu"),
            (
                r"\\WSL.LOCALHOST\Ubuntu-24.04",
                r"\\WSL.LOCALHOST\Ubuntu-24.04",
            ),
            (
                r"\\?\UNC\wsl.localhost\Debian\tmp",
                r"\\wsl.localhost\Debian",
            ),
        ] {
            let cwd = Path::new(cwd);
            assert_eq!(
                convert("/home/u/x", cwd),
                windows(&format!(r"{root}\home\u\x"))
            );
            assert_eq!(convert("/", cwd), windows(&format!(r"{root}\")));
            assert_eq!(
                convert("/tmp/dl/", cwd),
                windows(&format!(r"{root}\tmp\dl\"))
            );
            // /mnt 下不是单个盘符的（/mnt/wsl 等）在发行版中
            assert_eq!(
                convert("/mnt/wsl/x", cwd),
                windows(&format!(r"{root}\mnt\wsl\x"))
            );
            assert_eq!(convert("/mnt", cwd), windows(&format!(r"{root}\mnt")));
        }
    }

    #[test]
    fn dots_follow_linux() {
        let wsl = Path::new(r"\\wsl.localhost\Ubuntu\home\u");
        assert_eq!(
            convert("/home/u/../v/./x", wsl),
            windows(r"\\wsl.localhost\Ubuntu\home\v\x")
        );
        assert_eq!(
            convert("/../../etc/hosts", wsl),
            windows(r"\\wsl.localhost\Ubuntu\etc\hosts")
        );
        // /mnt/c/.. 是 /mnt，不是 C: 的上一级
        assert_eq!(convert("/mnt/c/../d/x", wsl), windows(r"D:\x"));
        assert_eq!(convert("/mnt/c/../x/y", wsl), windows(r"X:\y"));
        assert_eq!(convert("/mnt/c/x/../y", wsl), windows(r"C:\y"));
        assert_eq!(
            convert("/mnt/c/..", wsl),
            windows(r"\\wsl.localhost\Ubuntu\mnt")
        );
    }

    #[test]
    fn other_distro_paths_need_wsl_as_current_directory() {
        for cwd in [r"C:\work", r"\\server\share\dir", r"\\wsl.localhost", ""] {
            let cwd = Path::new(cwd);
            assert_eq!(
                convert("/home/u/x", cwd),
                Converted::UnknownDistro,
                "{cwd:?}"
            );
            assert_eq!(convert("/", cwd), Converted::UnknownDistro, "{cwd:?}");
        }
    }

    #[test]
    fn windows_paths_are_unchanged() {
        for cwd in [r"C:\work", r"\\wsl.localhost\Ubuntu\home\u"] {
            let cwd = Path::new(cwd);
            for path in [
                "a.txt",
                "./a.txt",
                "../a.txt",
                "中文 目录/测试 文件.txt",
                r"C:\x",
                "C:/x",
                r"\x",
                r"\\wsl.localhost\Ubuntu\home\u\x",
                "//server/share/x",
                "//",
                "",
            ] {
                assert_eq!(convert(path, cwd), Converted::Unchanged, "{path:?} {cwd:?}");
            }
        }
    }

    #[test]
    fn explains_how_to_write_it() {
        let text = WSL_PATH.fill(Lang::Zh, &[&"/home/u/x", &"/home/u/x"]);
        assert!(
            text.contains("相对路径") && text.contains("\"$(wslpath -w /home/u/x)\""),
            "{text}"
        );
        // MCP 只收绝对路径：说明中不提相对路径与进入目录
        let text = MCP_WSL_PATH.fill(Lang::Zh, &[&"/home/u/x", &"/home/u/x"]);
        assert!(
            !text.contains("相对路径") && text.contains("wslpath -w /home/u/x"),
            "{text}"
        );
        if cfg!(windows) {
            // 测试进程的当前目录不在 WSL 中
            let error = from_wsl("/home/u/x", Lang::En).unwrap_err();
            assert!(error.contains("wslpath -w /home/u/x"), "{error}");
            let error = from_wsl_for_mcp("/home/u/x", Lang::En).unwrap_err();
            assert!(error.contains(r"\\wsl.localhost\Ubuntu\"), "{error}");
            assert_eq!(from_wsl("/mnt/c/x", Lang::En).unwrap(), r"C:\x");
            assert_eq!(from_wsl_for_mcp("/mnt/c/x", Lang::En).unwrap(), r"C:\x");
        } else {
            assert_eq!(from_wsl("/home/u/x", Lang::En).unwrap(), "/home/u/x");
        }
        assert_eq!(from_wsl("a.txt", Lang::En).unwrap(), "a.txt");
    }
}
