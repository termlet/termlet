//! 本机文件的平台相关部分：不覆盖的改名、文件的身份（判断两个路径是不是同一个文件）、回收站、盘符。

use std::io;
use std::path::{Path, PathBuf};

/// 文件的身份：Windows 上是卷序列号与文件 ID，其他系统是设备号与 inode。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileId {
    volume: u64,
    index: u64,
}

/// 把 `from` 改名为 `to`；`to` 已存在（文件或目录）时不覆盖，返回 [`io::ErrorKind::AlreadyExists`]。
/// 标准库的 `rename` 会静默覆盖已有的文件。
pub(crate) fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    imp::rename_no_replace(from, to)
}

/// 打开的文件的身份；文件系统不提供（为 0）时为 `None`。
pub(crate) fn file_id_of(file: &std::fs::File) -> Option<FileId> {
    imp::file_id_of(file)
}

/// 路径的身份（不跟随符号链接）；读不到时为 `None`。
pub(crate) fn entry_id(path: &Path) -> Option<FileId> {
    imp::entry_id(path)
}

/// `path` 现在是否还是 `id` 记录的那个文件。
pub(crate) fn same_file(path: &Path, id: Option<FileId>) -> bool {
    id.is_some() && entry_id(path) == id
}

/// 移到回收站（Windows）。全部成功才返回 `Ok`。
pub(crate) fn trash(paths: &[PathBuf]) -> Result<(), super::SftpError> {
    imp::trash(paths)
}

/// 系统报告的盘符（`C:\`、`D:\` …）；其他平台为 `/`。
pub(crate) fn logical_drives() -> Vec<PathBuf> {
    imp::logical_drives()
}

#[cfg(windows)]
mod imp {
    use std::ffi::OsStr;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        GetFileInformationByHandle, GetLogicalDrives, MoveFileExW,
    };
    use windows_sys::Win32::UI::Shell::{
        FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT,
        FOF_WANTNUKEWARNING, SHFILEOPSTRUCTW, SHFileOperationW,
    };

    use super::FileId;
    use crate::sftp::error::{SftpError, Side};

    fn wide(path: &OsStr) -> Vec<u16> {
        path.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// 给超过 MAX_PATH 的绝对路径加上 `\\?\` 前缀（与标准库对长路径的处理相同），否则 MoveFileExW 可能处理不了。
    pub(super) fn long_path(path: &Path) -> PathBuf {
        let text = path.as_os_str().to_string_lossy();
        if text.len() < 248
            || !path.is_absolute()
            || text.starts_with(r"\\?\")
            || text.starts_with(r"\\.\")
        {
            return path.to_path_buf();
        }
        let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let text = absolute.to_string_lossy();
        match text.strip_prefix(r"\\") {
            Some(unc) => PathBuf::from(format!(r"\\?\UNC\{unc}")),
            None => PathBuf::from(format!(r"\\?\{text}")),
        }
    }

    pub(super) fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
        let source = wide(long_path(from).as_os_str());
        let target = wide(long_path(to).as_os_str());
        // 不带 MOVEFILE_REPLACE_EXISTING：目标已存在时由系统原子地拒绝
        let ok = unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), 0) };
        if ok != 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists && std::fs::symlink_metadata(to).is_ok() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, error));
        }
        Err(error)
    }

    pub(super) fn file_id_of(file: &std::fs::File) -> Option<FileId> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) };
        if ok == 0 {
            return None;
        }
        let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        (index != 0).then_some(FileId {
            volume: u64::from(info.dwVolumeSerialNumber),
            index,
        })
    }

    pub(super) fn entry_id(path: &Path) -> Option<FileId> {
        // 只读属性，不需要读权限；目录要 BACKUP_SEMANTICS；不跟随符号链接与目录联接
        let file = std::fs::OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .ok()?;
        file_id_of(&file)
    }

    pub(super) fn trash(paths: &[PathBuf]) -> Result<(), SftpError> {
        if paths.is_empty() {
            return Ok(());
        }
        // pFrom 是以 \0 分隔、以两个 \0 结尾的路径列表
        let mut from: Vec<u16> = Vec::new();
        for path in paths {
            from.extend(path.as_os_str().encode_wide());
            from.push(0);
        }
        from.push(0);
        let mut op = SHFILEOPSTRUCTW {
            wFunc: FO_DELETE,
            pFrom: from.as_ptr(),
            // 不能进回收站（网络位置等）时系统会提示将永久删除，由用户决定
            fFlags: (FOF_ALLOWUNDO
                | FOF_NOCONFIRMATION
                | FOF_WANTNUKEWARNING
                | FOF_SILENT
                | FOF_NOERRORUI) as u16,
            ..Default::default()
        };
        let code = unsafe { SHFileOperationW(&mut op) };
        let first = paths[0].display().to_string();
        if code == 0 && op.fAnyOperationsAborted == 0 {
            return Ok(());
        }
        if code == 0 {
            return Err(SftpError::Canceled);
        }
        Err(match code {
            2 | 3 | 0x7C => SftpError::NotFound {
                side: Side::Local,
                path: first,
            },
            5 | 0x78 => SftpError::PermissionDenied {
                side: Side::Local,
                path: first,
            },
            32 | 33 => SftpError::Busy {
                side: Side::Local,
                path: first,
            },
            other => SftpError::failed(
                Side::Local,
                &first,
                format!("移到回收站失败（代码 0x{other:X}）"),
            ),
        })
    }

    pub(super) fn logical_drives() -> Vec<PathBuf> {
        let mask = unsafe { GetLogicalDrives() };
        (0..26u8)
            .filter(|i| mask & (1 << i) != 0)
            .map(|i| PathBuf::from(format!("{}:\\", (b'A' + i) as char)))
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn long_paths_get_verbatim_prefix() {
            let long = "a".repeat(250);
            for (input, want) in [
                (r"C:\x\y".to_string(), r"C:\x\y".to_string()),
                (format!(r"C:\{long}"), format!(r"\\?\C:\{long}")),
                (format!(r"C:\x\..\{long}"), format!(r"\\?\C:\{long}")),
                (
                    format!(r"\\server\share\{long}"),
                    format!(r"\\?\UNC\server\share\{long}"),
                ),
                (format!(r"\\?\C:\{long}"), format!(r"\\?\C:\{long}")),
                (format!(r"x\{long}"), format!(r"x\{long}")),
            ] {
                assert_eq!(
                    long_path(Path::new(&input)),
                    PathBuf::from(&want),
                    "{input}"
                );
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::io;
    use std::path::{Path, PathBuf};

    use super::FileId;
    use crate::sftp::error::{Rejection, SftpError};

    /// 先建硬链接（目标已存在时原子地失败）再删除 `from`；文件系统不支持硬链接（FAT、部分网络文件系统）时退回先检查再改名。
    pub(super) fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
        match std::fs::hard_link(from, to) {
            Ok(()) => {
                let _ = std::fs::remove_file(from);
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e),
            Err(_) => {
                if std::fs::symlink_metadata(to).is_ok() {
                    return Err(io::ErrorKind::AlreadyExists.into());
                }
                std::fs::rename(from, to)
            }
        }
    }

    fn id_of(meta: &std::fs::Metadata) -> Option<FileId> {
        use std::os::unix::fs::MetadataExt;
        (meta.ino() != 0).then_some(FileId {
            volume: meta.dev(),
            index: meta.ino(),
        })
    }

    pub(super) fn file_id_of(file: &std::fs::File) -> Option<FileId> {
        id_of(&file.metadata().ok()?)
    }

    pub(super) fn entry_id(path: &Path) -> Option<FileId> {
        id_of(&std::fs::symlink_metadata(path).ok()?)
    }

    pub(super) fn trash(_paths: &[PathBuf]) -> Result<(), SftpError> {
        Err(Rejection::TrashUnsupported.into())
    }

    pub(super) fn logical_drives() -> Vec<PathBuf> {
        vec![PathBuf::from("/")]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 改名不覆盖已有的文件或目录
    #[test]
    fn rename_never_replaces() {
        let tmp = tempfile::tempdir().unwrap();
        let part = tmp.path().join("x.termlet-part");
        let local = tmp.path().join("x");
        std::fs::write(&part, "new").unwrap();
        rename_no_replace(&part, &local).unwrap();
        std::fs::write(&part, "newer").unwrap();
        let err = rename_no_replace(&part, &local).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let err = rename_no_replace(&part, &sub).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
        assert_eq!(std::fs::read_to_string(&local).unwrap(), "new");
    }

    // 超过 MAX_PATH 的路径也能改名
    #[test]
    fn rename_long_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp
            .path()
            .join("d".repeat(100))
            .join("e".repeat(100))
            .join("f".repeat(100));
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("x.termlet-part");
        std::fs::write(&part, "new").unwrap();
        rename_no_replace(&part, &dir.join("x")).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("x")).unwrap(), "new");
    }

    #[test]
    fn identities() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        std::fs::write(&a, "x").unwrap();
        let file = std::fs::File::open(&a).unwrap();
        let id = file_id_of(&file);
        drop(file);
        assert!(id.is_some());
        assert!(same_file(&a, id));
        std::fs::remove_file(&a).unwrap();
        std::fs::write(&a, "y").unwrap();
        let b = tmp.path().join("b");
        std::fs::write(&b, "y").unwrap();
        assert_ne!(entry_id(&b), id);
        assert!(!same_file(&tmp.path().join("none"), id));
    }
}
