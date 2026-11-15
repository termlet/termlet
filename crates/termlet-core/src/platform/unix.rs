use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// 用户主目录中的已知文件夹与存放各程序配置、数据的目录。
pub(super) const HOME_FOLDERS: &[&str] = &[
    "Desktop",
    "Documents",
    "Downloads",
    "Music",
    "Pictures",
    "Videos",
    "Movies",
    "Public",
    "Templates",
    "Library",
    ".local",
    ".local/share",
    ".config",
    ".cache",
];

pub(super) fn system_dirs() -> Vec<PathBuf> {
    Vec::new()
}

/// 根目录或 `/tmp`、`/home`、`/var` 这样的顶层目录。
pub(super) fn is_top_level(p: &Path) -> bool {
    p == Path::new("/") || p.parent() == Some(Path::new("/"))
}

pub(super) fn restrict_dir(dir: &Path) -> io::Result<()> {
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

pub(super) fn restrict_file(path: &Path) -> io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// 新建只有当前用户能读写的文件（已存在时失败），创建时就是 0600。
pub(super) fn create_private_file(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

pub(super) fn is_private_dir(dir: &Path) -> bool {
    std::fs::metadata(dir)
        .map(|m| m.permissions().mode() & 0o077 == 0)
        .unwrap_or(false)
}
