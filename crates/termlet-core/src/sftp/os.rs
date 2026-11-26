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

/// 移到回收站（Windows）。全部成功才返回 `Ok`。系统不能把某一项移到回收站时：`permanent` 为假则整个操作中止、
/// 返回 [`Rejection::NotRecyclable`](super::Rejection::NotRecyclable)，不会悄悄永久删除（中止前已移到回收站的留在
/// 回收站中）；为真（用户已确认永久删除）则永久删除它。
pub(crate) fn trash(paths: &[PathBuf], permanent: bool) -> Result<(), super::SftpError> {
    imp::trash(paths, permanent)
}

/// 在 `dir` 中删除是不是永久删除：Windows 上只有本地固定盘有回收站，UNC 路径（网络共享、WSL）、映射的网络盘、
/// 可移动盘、光盘、内存盘都没有。只用于界面上的说明；实际删除时系统做不到移到回收站的由 [`trash`] 把关。
/// 其他平台不支持回收站（删除被拒绝，不会永久删除），为假。
pub(crate) fn deletes_permanently(dir: &Path) -> bool {
    imp::deletes_permanently(dir)
}

/// Windows 上不是链接的重解析点：WSL 位置（`\\wsl.localhost\…`）中的命名管道、套接字、设备文件就是这样，
/// 读它会一直等到超时。其他平台为假。
pub(crate) fn non_link_reparse_point(meta: &std::fs::Metadata) -> bool {
    imp::non_link_reparse_point(meta)
}

/// 系统报告的盘符（`C:\`、`D:\` …）；其他平台为 `/`。
pub(crate) fn logical_drives() -> Vec<PathBuf> {
    imp::logical_drives()
}

/// 路径的盘符（`C:\x`、`\\?\C:\x` 为 `C`）；UNC 路径、相对路径为 `None`。
#[cfg_attr(not(windows), allow(dead_code))]
fn drive_letter(path: &Path) -> Option<char> {
    use std::path::{Component, Prefix};
    match path.components().next()? {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => Some(char::from(letter)),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::OsStr;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows::Win32::Foundation::E_ABORT;
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
        CoInitializeEx, CoUninitialize,
    };
    use windows::Win32::UI::Shell::{
        COPYENGINE_E_ACCESS_DENIED_SRC, COPYENGINE_E_SHARING_VIOLATION_SRC,
        COPYENGINE_E_USER_CANCELLED, FOF_ALLOWUNDO, FOF_NO_UI, FOFX_RECYCLEONDELETE, FileOperation,
        IFileOperation, IFileOperationProgressSink, IFileOperationProgressSink_Impl, IShellItem,
        SHCreateItemFromParsingName, TSF_DELETE_RECYCLE_IF_POSSIBLE,
    };
    use windows::core::{HRESULT, PCWSTR, Ref, implement};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, GetDriveTypeW, GetFileInformationByHandle, GetLogicalDrives,
        MoveFileExW,
    };

    use super::FileId;
    use crate::sftp::error::{Rejection, SftpError, Side};

    /// `GetDriveTypeW` 的本地固定盘（`DRIVE_FIXED`，在 windows-sys 没有开的 WindowsProgramming 中）。
    const DRIVE_FIXED: u32 = 3;

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

    /// 删除前逐项检查：系统不能把这一项移到回收站（没有 `TSF_DELETE_RECYCLE_IF_POSSIBLE`）、用户又没有确认永久删除时
    /// 中止整个操作（与 Chromium 移到回收站的做法相同）。`SHFileOperationW` 加 `FOF_NOCONFIRMATION` 时，网络位置、
    /// WSL（`\\wsl.localhost\…`）等没有回收站的位置会被直接永久删除，`FOF_WANTNUKEWARNING` 也不提示（实测）。
    #[implement(IFileOperationProgressSink)]
    struct RecycleOnly {
        permanent: bool,
        refused: Arc<AtomicBool>,
    }

    impl IFileOperationProgressSink_Impl for RecycleOnly_Impl {
        fn PreDeleteItem(&self, flags: u32, _: Ref<IShellItem>) -> windows::core::Result<()> {
            if !self.permanent && flags & TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32 == 0 {
                self.refused.store(true, Ordering::SeqCst);
                return Err(E_ABORT.into());
            }
            Ok(())
        }
        fn StartOperations(&self) -> windows::core::Result<()> {
            Ok(())
        }
        fn FinishOperations(&self, _: HRESULT) -> windows::core::Result<()> {
            Ok(())
        }
        fn PreRenameItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: &PCWSTR,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostRenameItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: &PCWSTR,
            _: HRESULT,
            _: Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PreMoveItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: Ref<IShellItem>,
            _: &PCWSTR,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostMoveItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: Ref<IShellItem>,
            _: &PCWSTR,
            _: HRESULT,
            _: Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PreCopyItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: Ref<IShellItem>,
            _: &PCWSTR,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostCopyItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: Ref<IShellItem>,
            _: &PCWSTR,
            _: HRESULT,
            _: Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostDeleteItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: HRESULT,
            _: Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn PreNewItem(&self, _: u32, _: Ref<IShellItem>, _: &PCWSTR) -> windows::core::Result<()> {
            Ok(())
        }
        fn PostNewItem(
            &self,
            _: u32,
            _: Ref<IShellItem>,
            _: &PCWSTR,
            _: &PCWSTR,
            _: u32,
            _: HRESULT,
            _: Ref<IShellItem>,
        ) -> windows::core::Result<()> {
            Ok(())
        }
        fn UpdateProgress(&self, _: u32, _: u32) -> windows::core::Result<()> {
            Ok(())
        }
        fn ResetTimer(&self) -> windows::core::Result<()> {
            Ok(())
        }
        fn PauseTimer(&self) -> windows::core::Result<()> {
            Ok(())
        }
        fn ResumeTimer(&self) -> windows::core::Result<()> {
            Ok(())
        }
    }

    /// 本线程初始化的 COM，离开时释放。
    struct Com;

    impl Drop for Com {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }

    pub(super) fn trash(paths: &[PathBuf], permanent: bool) -> Result<(), SftpError> {
        if paths.is_empty() {
            return Ok(());
        }
        let first = paths[0].display().to_string();
        // 外壳的文件操作要在单线程套间中执行：另开一个线程，不改动调用方线程（运行时的线程池）的 COM 状态
        std::thread::scope(|scope| {
            scope
                .spawn(|| shell_delete(paths, permanent, &first))
                .join()
                .unwrap_or_else(|_| Err(SftpError::failed(Side::Local, &first, "删除时出错")))
        })
    }

    /// 用 `IFileOperation` 删除：能移到回收站的总是移到回收站，不能的按 `permanent` 永久删除或中止。
    fn shell_delete(paths: &[PathBuf], permanent: bool, first: &str) -> Result<(), SftpError> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }
            .ok()
            .map_err(|e| describe(e.code(), first))?;
        let _com = Com;
        let refused = Arc::new(AtomicBool::new(false));
        let run = || -> windows::core::Result<bool> {
            let op: IFileOperation = unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_ALL)? };
            unsafe { op.SetOperationFlags(FOF_ALLOWUNDO | FOF_NO_UI | FOFX_RECYCLEONDELETE)? };
            let sink: IFileOperationProgressSink = RecycleOnly {
                permanent,
                refused: refused.clone(),
            }
            .into();
            for path in paths {
                let name = wide(path.as_os_str());
                let item: IShellItem =
                    unsafe { SHCreateItemFromParsingName(PCWSTR(name.as_ptr()), None)? };
                unsafe { op.DeleteItem(&item, &sink)? };
            }
            unsafe { op.PerformOperations()? };
            Ok(unsafe { op.GetAnyOperationsAborted()? }.as_bool())
        };
        let result = run();
        if refused.load(Ordering::SeqCst) {
            return Err(Rejection::NotRecyclable.into());
        }
        match result {
            Ok(false) => Ok(()),
            Ok(true) => Err(SftpError::Canceled),
            Err(e) => Err(describe(e.code(), first)),
        }
    }

    /// 外壳文件操作的错误：Win32 错误码按种类归类，其余带上代码。
    fn describe(code: HRESULT, first: &str) -> SftpError {
        let path = first.to_string();
        if code == E_ABORT || code == COPYENGINE_E_USER_CANCELLED {
            return SftpError::Canceled;
        }
        // 外壳自己的错误码换成对应的 Win32 错误码；HRESULT_FROM_WIN32 是 0x8007xxxx
        let win32 = if code == COPYENGINE_E_ACCESS_DENIED_SRC {
            Some(5)
        } else if code == COPYENGINE_E_SHARING_VIOLATION_SRC {
            Some(32)
        } else {
            (code.0 as u32 >> 16 == 0x8007).then_some(code.0 as u32 & 0xFFFF)
        };
        match win32 {
            Some(2 | 3) => SftpError::NotFound {
                side: Side::Local,
                path,
            },
            Some(5) => SftpError::PermissionDenied {
                side: Side::Local,
                path,
            },
            Some(32 | 33) => SftpError::Busy {
                side: Side::Local,
                path,
            },
            _ => SftpError::failed(
                Side::Local,
                first,
                format!("删除失败（代码 0x{:08X}）", code.0 as u32),
            ),
        }
    }

    pub(super) fn deletes_permanently(dir: &Path) -> bool {
        let dir = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
        let Some(letter) = super::drive_letter(&dir) else {
            // UNC 路径（网络共享、WSL）、设备路径
            return true;
        };
        let root = wide(OsStr::new(&format!("{letter}:\\")));
        unsafe { GetDriveTypeW(root.as_ptr()) != DRIVE_FIXED }
    }

    pub(super) fn non_link_reparse_point(meta: &std::fs::Metadata) -> bool {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            && !meta.file_type().is_symlink()
            && !meta.is_dir()
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

        // 只有本地固定盘按回收站：UNC 路径（网络共享、WSL）都是永久删除；系统盘、临时目录所在的盘是固定盘
        #[test]
        fn permanent_deletion_by_location() {
            for (path, want) in [
                (r"C:\x", Some('C')),
                (r"d:\", Some('D')),
                (r"\\?\C:\x", Some('C')),
                (r"\\server\share\x", None),
                (r"\\wsl.localhost\Ubuntu\home", None),
                (r"\\?\UNC\server\share", None),
                (r"relative\x", None),
            ] {
                assert_eq!(super::super::drive_letter(Path::new(path)), want, "{path}");
            }
            for path in [
                r"\\server\share\x",
                r"\\wsl.localhost\Ubuntu\home",
                r"\\wsl$\Ubuntu",
                r"\\?\UNC\server\share",
            ] {
                assert!(deletes_permanently(Path::new(path)), "{path}");
            }
            let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
            assert!(!deletes_permanently(
                &Path::new(&format!(r"{system}\")).join("Windows")
            ));
            let tmp = tempfile::tempdir().unwrap();
            assert!(!deletes_permanently(tmp.path()), "{}", tmp.path().display());
            // 不存在的盘符：没有回收站（按系统的盘符表找，不访问断开的网络驱动器）
            let used = logical_drives();
            let free = (b'A'..=b'Z')
                .rev()
                .map(char::from)
                .find(|c| !used.contains(&PathBuf::from(format!(r"{c}:\"))));
            if let Some(free) = free {
                assert!(deletes_permanently(Path::new(&format!(r"{free}:\x"))));
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

    pub(super) fn trash(_paths: &[PathBuf], _permanent: bool) -> Result<(), SftpError> {
        Err(Rejection::TrashUnsupported.into())
    }

    pub(super) fn deletes_permanently(_dir: &Path) -> bool {
        false
    }

    pub(super) fn non_link_reparse_point(_meta: &std::fs::Metadata) -> bool {
        false
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
