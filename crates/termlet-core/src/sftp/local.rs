//! 本机文件（双栏的本地栏）：列出目录、盘符与常用位置、新建文件夹、重命名、移到回收站。
//!
//! 只接受绝对路径的目录与其中的名称：名称不能含路径分隔符、不能是 `.` 或 `..`，操作不会越出所在目录。设备路径
//! （`\\?\`、`\\.\`）一律拒绝。网络位置（`\\server\share`）访问前要用户确认（访问时 Windows 会自动把当前账户的登录信息
//! 发给对方），这一步由界面做：见 [`network_server`]。全部是异步函数，文件操作在后台线程上执行。

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::entry::EntryKind;
use super::error::{self, Rejection, SftpError, Side};
use super::os;
use super::transfer::blocking;

/// 盘符检查的超时：断开的网络驱动器可能很久才返回。
const DRIVE_TIMEOUT: Duration = Duration::from_millis(800);

/// 本机目录中的一项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalEntry {
    pub name: String,
    /// 完整路径：上传、拖动到远程栏时使用
    pub path: PathBuf,
    pub kind: EntryKind,
    /// 符号链接（含 Windows 的目录联接）指向的目标的种类；失效的链接为 `None`
    pub target: Option<EntryKind>,
    /// 字节数（目录为 0）
    pub size: u64,
    /// 修改时间（Unix 秒）；读不到信息时（系统文件、失效的链接）为 0
    pub modified: i64,
    /// 隐藏文件（Windows 的隐藏属性；其他系统是以 `.` 开头的名称）
    pub hidden: bool,
}

impl LocalEntry {
    /// 目录，或指向目录的链接。
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir || self.target == Some(EntryKind::Dir)
    }

    pub fn is_link(&self) -> bool {
        self.kind == EntryKind::Symlink
    }
}

/// 本机目录列表。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalListing {
    pub path: PathBuf,
    /// 上级目录；已是磁盘根目录（`C:\`、`/`）时为 `None`
    pub parent: Option<PathBuf>,
    pub entries: Vec<LocalEntry>,
}

/// 常用位置。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalPlaces {
    pub home: Option<PathBuf>,
    pub desktop: Option<PathBuf>,
    pub downloads: Option<PathBuf>,
    pub documents: Option<PathBuf>,
    /// Windows 上为存在的盘符（`C:\`、`D:\` …），其他平台为 `/`
    pub roots: Vec<PathBuf>,
}

/// 设备路径（`\\?\`、`\\.\`，也写作 `//?/`）：能绕过路径的规范化、访问设备与命名管道，不是本地栏要用的路径。
fn is_device_path(path: &Path) -> bool {
    let text = path.as_os_str().to_string_lossy();
    let b = text.as_bytes();
    b.len() >= 4
        && matches!(b[0], b'\\' | b'/')
        && matches!(b[1], b'\\' | b'/')
        && matches!(b[2], b'?' | b'.')
        && matches!(b[3], b'\\' | b'/')
}

/// 访问前需要用户确认的网络位置：UNC 路径的服务器名（小写）。不是网络位置、是 WSL 的位置
/// （`\\wsl.localhost\…`、`\\wsl$\…`，由本机的 WSL 提供）时为 `None`。界面记下本次运行中用户允许过的服务器。
pub fn network_server(path: &Path) -> Option<String> {
    if is_device_path(path) {
        return None;
    }
    let text = path.as_os_str().to_string_lossy().replace('/', "\\");
    let rest = text.strip_prefix(r"\\")?;
    let server = rest.split('\\').next()?.to_lowercase();
    if server.is_empty() || server == "wsl.localhost" || server == "wsl$" {
        return None;
    }
    Some(server)
}

/// 目录必须是绝对路径、不能是设备路径；返回规范化后的路径（处理 `.` 与 `..`）。
fn absolute_dir(dir: &Path) -> Result<PathBuf, SftpError> {
    if !dir.is_absolute() {
        return Err(Rejection::NotAbsolute.into());
    }
    if is_device_path(dir) {
        return Err(Rejection::DevicePath.into());
    }
    let path = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
    if is_device_path(&path) {
        return Err(Rejection::DevicePath.into());
    }
    Ok(path)
}

/// 文件名：不能为空、不能含路径分隔符与 Windows 不允许的字符、不能是 `.` 或 `..`，最长 255 个字符。
pub fn valid_local_name(name: &str) -> Result<&str, SftpError> {
    let bad = name.is_empty()
        || name.chars().count() > 255
        || name == "."
        || name == ".."
        || name.chars().any(|c| {
            matches!(
                c,
                '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0'
            )
        });
    if bad {
        return Err(Rejection::InvalidName.into());
    }
    Ok(name)
}

fn entry_path(dir: &Path, name: &str) -> Result<PathBuf, SftpError> {
    Ok(absolute_dir(dir)?.join(valid_local_name(name)?))
}

fn mtime(meta: &std::fs::Metadata) -> i64 {
    super::transfer::upload::local_mtime(meta)
}

#[cfg(windows)]
fn hidden(_name: &str, meta: Option<&std::fs::Metadata>) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    meta.is_some_and(|m| m.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0)
}

#[cfg(not(windows))]
fn hidden(name: &str, _meta: Option<&std::fs::Metadata>) -> bool {
    name.starts_with('.')
}

fn kind_of(file_type: std::fs::FileType) -> EntryKind {
    if file_type.is_symlink() {
        EntryKind::Symlink
    } else if file_type.is_dir() {
        EntryKind::Dir
    } else if file_type.is_file() {
        EntryKind::File
    } else {
        EntryKind::Special
    }
}

/// 列出本机目录。
pub async fn list_local(dir: &Path) -> Result<LocalListing, SftpError> {
    let path = absolute_dir(dir)?;
    blocking(move || {
        let reader = std::fs::read_dir(&path).map_err(|e| error::local(&path, &e))?;
        let mut entries = Vec::new();
        for entry in reader {
            let entry = entry.map_err(|e| error::local(&path, &e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let full = entry.path();
            let own = entry.metadata().ok();
            let kind = entry.file_type().map(kind_of).unwrap_or(EntryKind::File);
            let mut item = LocalEntry {
                hidden: hidden(&name, own.as_ref()),
                name,
                path: full.clone(),
                kind,
                target: None,
                size: 0,
                modified: 0,
            };
            // 链接（含 Windows 的目录联接）按目标判断是不是目录
            let meta = if kind == EntryKind::Symlink {
                std::fs::metadata(&full).ok()
            } else {
                own
            };
            // 系统文件（如 pagefile.sys）、失效的链接读不到信息，只显示名称
            if let Some(meta) = meta {
                if kind == EntryKind::Symlink {
                    item.target = Some(kind_of(meta.file_type()));
                }
                item.size = if meta.is_dir() { 0 } else { meta.len() };
                item.modified = mtime(&meta);
            }
            entries.push(item);
        }
        let parent = path.parent().map(Path::to_path_buf);
        Ok(LocalListing {
            path,
            parent,
            entries,
        })
    })
    .await
}

/// 本机的根：Windows 上为存在的盘符（并行检查，断开的网络驱动器不列出），其他平台为 `/`。
pub async fn local_roots() -> Vec<PathBuf> {
    let candidates = os::logical_drives();
    let checks = candidates.into_iter().map(|drive| async move {
        let probe = drive.clone();
        let check = blocking(move || std::fs::metadata(&probe).is_ok());
        matches!(tokio::time::timeout(DRIVE_TIMEOUT, check).await, Ok(true)).then_some(drive)
    });
    futures::future::join_all(checks)
        .await
        .into_iter()
        .flatten()
        .collect()
}

/// 常用位置：主目录、桌面、下载、文档与根。
pub async fn local_places() -> LocalPlaces {
    LocalPlaces {
        home: dirs::home_dir(),
        desktop: dirs::desktop_dir(),
        downloads: dirs::download_dir(),
        documents: dirs::document_dir(),
        roots: local_roots().await,
    }
}

/// 在 `dir` 中新建文件夹 `name`，返回它的路径。
pub async fn make_local_dir(dir: &Path, name: &str) -> Result<PathBuf, SftpError> {
    let target = entry_path(dir, name)?;
    blocking(move || {
        std::fs::create_dir(&target).map_err(|e| error::local(&target, &e))?;
        Ok(target)
    })
    .await
}

/// 重命名 `dir` 中的 `from` 为 `to`；目标已存在时不覆盖。只改大小写时，不区分大小写的目录里目标就是源文件本身，可以改；
/// 区分大小写的目录（WSL 创建的目录、`\\wsl.localhost\…`、Linux）里可能是另一个文件，不能覆盖。
pub async fn rename_local(dir: &Path, from: &str, to: &str) -> Result<(), SftpError> {
    let source = entry_path(dir, from)?;
    let target = entry_path(dir, to)?;
    if source == target {
        return Ok(());
    }
    blocking(move || {
        if std::fs::symlink_metadata(&target).is_ok() {
            let case_only =
                source.to_string_lossy().to_lowercase() == target.to_string_lossy().to_lowercase();
            if !case_only || !same_entry(&source, &target) {
                return Err(SftpError::AlreadyExists {
                    side: Side::Local,
                    path: target.display().to_string(),
                });
            }
        }
        std::fs::rename(&source, &target).map_err(|e| error::local(&source, &e))
    })
    .await
}

/// 两个路径是不是同一个目录项：比较文件的身份（不跟随链接）。文件系统不提供身份时（部分 SMB/NAS、虚拟盘）看所在目录的
/// 列表：两个名称都列出来才是两个目录项，不区分大小写的目录只列出实际的那一个名称。
fn same_entry(a: &Path, b: &Path) -> bool {
    match (os::entry_id(a), os::entry_id(b)) {
        (Some(x), Some(y)) => x == y,
        _ => {
            if a.parent() != b.parent() {
                return false;
            }
            let Some(dir) = a.parent() else {
                return false;
            };
            let Ok(names) = std::fs::read_dir(dir) else {
                return false;
            };
            let names: Vec<_> = names.flatten().map(|e| e.file_name()).collect();
            let has = |p: &Path| p.file_name().is_some_and(|n| names.iter().any(|x| x == n));
            !(has(a) && has(b))
        }
    }
}

/// 把 `dir` 中的这些项移到回收站（误删可以找回）。不能进回收站的位置（网络位置等）系统会提示将永久删除。
/// 只在 Windows 上支持，其他平台返回 [`Rejection::TrashUnsupported`]。
pub async fn trash_local(dir: &Path, names: &[String]) -> Result<(), SftpError> {
    let paths = names
        .iter()
        .map(|name| entry_path(dir, name))
        .collect::<Result<Vec<_>, _>>()?;
    if paths.is_empty() {
        return Err(Rejection::NoPaths.into());
    }
    blocking(move || os::trash(&paths)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_paths() {
        for bad in ["", ".", "..", "a/b", r"a\b", "a:b", "a?", "a\0b"] {
            assert!(valid_local_name(bad).is_err(), "{bad:?}");
        }
        assert!(valid_local_name("新建文件夹 (2)").is_ok());
        assert!(is_device_path(Path::new(r"\\?\C:\x")));
        assert!(is_device_path(Path::new(r"\\.\pipe\x")));
        assert!(is_device_path(Path::new("//?/C:/x")));
        assert!(!is_device_path(Path::new(r"\\server\share")));
        assert_eq!(
            network_server(Path::new(r"\\NAS\share\a")).as_deref(),
            Some("nas")
        );
        assert_eq!(
            network_server(Path::new(r"\\wsl.localhost\Ubuntu\home")),
            None
        );
        assert_eq!(network_server(Path::new(r"\\wsl$\Ubuntu")), None);
        assert_eq!(network_server(Path::new(r"C:\x")), None);
        assert!(matches!(
            absolute_dir(Path::new("relative")),
            Err(SftpError::Rejected(Rejection::NotAbsolute))
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn local_operations() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        let made = make_local_dir(dir, "sub").await.unwrap();
        assert!(made.is_dir());
        assert!(matches!(
            make_local_dir(dir, "sub").await,
            Err(SftpError::AlreadyExists {
                side: Side::Local,
                ..
            })
        ));
        let listing = list_local(dir).await.unwrap();
        assert_eq!(listing.parent.as_deref(), dir.parent());
        let mut names: Vec<_> = listing.entries.iter().map(|e| e.name.clone()).collect();
        names.sort();
        assert_eq!(names, ["a.txt", "sub"]);
        let file = listing.entries.iter().find(|e| e.name == "a.txt").unwrap();
        assert_eq!((file.kind, file.size), (EntryKind::File, 5));
        assert!(file.modified > 0);
        assert!(
            listing
                .entries
                .iter()
                .any(|e| e.name == "sub" && e.is_dir())
        );

        // 重命名：目标已存在时不覆盖；只改大小写可以
        std::fs::write(dir.join("b.txt"), "b").unwrap();
        assert!(matches!(
            rename_local(dir, "a.txt", "b.txt").await,
            Err(SftpError::AlreadyExists { .. })
        ));
        rename_local(dir, "a.txt", "A.txt").await.unwrap();
        let names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"A.txt".to_string()), "{names:?}");
        assert!(matches!(
            rename_local(dir, "A.txt", "../x").await,
            Err(SftpError::Rejected(Rejection::InvalidName))
        ));
        assert!(matches!(
            list_local(&dir.join("none")).await,
            Err(SftpError::NotFound {
                side: Side::Local,
                ..
            })
        ));
        let roots = local_roots().await;
        assert!(!roots.is_empty());
        if cfg!(windows) {
            assert!(roots.iter().any(|r| r.to_string_lossy().ends_with(":\\")));
        }
    }

    // 移到回收站（只在 Windows 上）：删除后原位置不再有。会在回收站里留下测试文件，设置 TERMLET_TEST_TRASH=1 时才运行
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread")]
    async fn trash_moves_out() {
        if std::env::var_os("TERMLET_TEST_TRASH").is_none() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("trash-me.txt"), "x").unwrap();
        std::fs::create_dir(dir.join("trash-dir")).unwrap();
        trash_local(dir, &["trash-me.txt".into(), "trash-dir".into()])
            .await
            .unwrap();
        assert!(!dir.join("trash-me.txt").exists());
        assert!(!dir.join("trash-dir").exists());
        assert!(matches!(
            trash_local(dir, &["../x".into()]).await,
            Err(SftpError::Rejected(Rejection::InvalidName))
        ));
    }
}
