//! 平台适配：只允许当前用户访问的目录与文件、公共位置的判断。
//!
//! Windows 上设受保护的 DACL（只有当前用户与 SYSTEM，内容继承），Unix 上目录 0700、文件 0600。
//! 文件系统不支持持久 ACL（FAT32、exFAT 等）时什么也不做，视为已私有。

use std::io;
use std::path::{Path, PathBuf};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as imp;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as imp;

/// 创建目录（含上级目录）并限制为只有当前用户能访问。
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    restrict_dir(dir)
}

/// 把目录限制为只有当前用户能访问，权限继承到其中的文件与子目录。
pub fn restrict_dir(dir: &Path) -> io::Result<()> {
    imp::restrict_dir(dir)
}

/// 把文件限制为只有当前用户能读写。
pub fn restrict_file(path: &Path) -> io::Result<()> {
    imp::restrict_file(path)
}

/// 目录是否已经只有当前用户能访问（[`restrict_dir`] 的结果）。
pub fn is_private_dir(dir: &Path) -> bool {
    imp::is_private_dir(dir)
}

/// 写入只有当前用户能读写的文件：新建临时文件时就只有当前用户能访问，写好后改名替换，不会留下写了一半的文件，
/// 内容也不会在收紧权限之前被其他用户读到。
pub fn write_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.{}.tmp", crate::id::random_token(5)));
    let result = (|| {
        let mut file = imp::create_private_file(&tmp)?;
        std::io::Write::write_all(&mut file, contents)?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// 目录是否位于公共位置：盘符根目录、用户主目录、桌面 / 文档 / 下载等已知文件夹、
/// `%APPDATA%`、临时目录，以及它们的上级目录。这类目录不能整体收紧权限（会影响别的程序）。
pub fn is_shared_location(dir: &Path) -> bool {
    let Ok(abs) = std::path::absolute(dir) else {
        return true;
    };
    let paths = with_real_path(vec![abs]);
    let shared = with_real_path(shared_dirs());
    paths.iter().any(|p| {
        imp::is_top_level(p) || shared.iter().any(|s| same_path(p, s) || is_parent_of(p, s))
    })
}

fn shared_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![std::env::temp_dir()];
    dirs.extend(
        [dirs::home_dir(), dirs::config_dir(), dirs::cache_dir()]
            .into_iter()
            .flatten(),
    );
    if let Some(home) = dirs::home_dir() {
        dirs.extend(imp::HOME_FOLDERS.iter().map(|name| home.join(name)));
    }
    dirs.extend(imp::system_dirs());
    dirs
}

fn with_real_path(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(paths.len() * 2);
    for p in paths {
        if p.as_os_str().is_empty() {
            continue;
        }
        if let Ok(resolved) = dunce_canonicalize(&p)
            && resolved != p
        {
            out.push(resolved);
        }
        out.push(p);
    }
    out
}

/// `canonicalize`，但在 Windows 上去掉 `\\?\` 前缀，便于和环境变量中的路径比较。
fn dunce_canonicalize(p: &Path) -> io::Result<PathBuf> {
    let resolved = std::fs::canonicalize(p)?;
    #[cfg(windows)]
    {
        let s = resolved.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\")
            && !rest.starts_with("UNC\\")
        {
            return Ok(PathBuf::from(rest));
        }
    }
    Ok(resolved)
}

fn fold(p: &Path) -> String {
    let mut s = p.to_string_lossy().into_owned();
    while s.len() > 1 && (s.ends_with('/') || s.ends_with('\\')) && !s.ends_with(":\\") {
        s.pop();
    }
    if cfg!(any(windows, target_os = "macos")) {
        s = s.to_lowercase();
    }
    s
}

fn same_path(a: &Path, b: &Path) -> bool {
    fold(a) == fold(b)
}

fn is_parent_of(parent: &Path, child: &Path) -> bool {
    let mut parent = fold(parent);
    let child = fold(child);
    let sep = std::path::MAIN_SEPARATOR;
    if !parent.ends_with(sep) {
        parent.push(sep);
    }
    child.len() > parent.len() && child.starts_with(&parent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_and_its_parents_are_shared() {
        let home = dirs::home_dir().unwrap();
        assert!(is_shared_location(&home));
        assert!(is_shared_location(home.parent().unwrap()));
        assert!(is_shared_location(&home.join("Documents")));
        assert!(is_shared_location(&std::env::temp_dir()));
    }

    #[test]
    fn app_folder_inside_config_dir_is_not_shared() {
        let config = dirs::config_dir().unwrap();
        assert!(is_shared_location(&config));
        assert!(!is_shared_location(&config.join("termlet")));
    }

    #[test]
    fn private_dir_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("private");
        ensure_private_dir(&dir).unwrap();
        assert!(is_private_dir(&dir));
        let file = dir.join("secret.txt");
        write_private_file(&file, b"hello").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"hello");
        // 没有留下临时文件
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(names.len(), 1);
    }

    // 写到不私有的目录（例如导出私钥到 D:\keys）时，文件本身仍只有当前用户能访问；已有的文件被替换
    #[test]
    fn private_file_in_shared_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("id_ed25519");
        std::fs::write(&file, "old").unwrap();
        write_private_file(&file, b"secret").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"secret");
        #[cfg(windows)]
        assert!(imp::is_private_file(&file));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
