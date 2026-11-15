//! 存放同步文件的位置：本机文件夹或 WebDAV（见 [`super::webdav`]）。
//!
//! 读取时得到内容与版本标记；写入时带上读取时的版本标记，只在同步文件仍是那个版本时写入（没有文件时标记为空，
//! 表示同步文件应当还不存在），否则报告"已被其他设备修改"，由调用方重新拉取再合并，不会整份覆盖。

use std::path::{Path, PathBuf};

use futures::future::BoxFuture;
use sha2::{Digest, Sha256};

use super::SyncError;
use super::document::{FILE_NAME, MAX_FILE_SIZE};

/// 读取同步文件的结果。
pub(crate) enum Fetch {
    /// 同步位置可以访问，但还没有同步文件（或被删除了）
    Missing,
    Found {
        data: Vec<u8>,
        etag: String,
    },
}

/// 只显示大小与版本标记（内容是加密的同步文件，没必要打印）。
impl std::fmt::Debug for Fetch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fetch::Missing => f.write_str("Missing"),
            Fetch::Found { data, etag } => f
                .debug_struct("Found")
                .field("bytes", &data.len())
                .field("etag", etag)
                .finish(),
        }
    }
}

/// 写入同步文件的结果。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Stored {
    Written,
    /// 同步文件已不是读取时的版本（其他设备先写入了）
    Changed,
}

/// 存放同步文件的位置。
pub(crate) trait Backend: Send + Sync {
    fn get(&self) -> BoxFuture<'_, Result<Fetch, SyncError>>;
    /// 只在同步文件仍是 `etag` 对应的版本时写入（`None` 表示同步文件应当还不存在）。
    fn put<'a>(
        &'a self,
        data: &'a [u8],
        etag: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Stored, SyncError>>;
    /// 检查能否访问（文件夹存在、可写；WebDAV 地址与账号正确，目录不存在时建立）。
    fn check(&self) -> BoxFuture<'_, Result<(), SyncError>>;
}

pub(crate) fn digest(data: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(data))
}

/// 在阻塞线程上执行文件操作（同步文件夹可能在网络位置上，读写会卡住）。
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, SyncError> + Send + 'static,
) -> Result<T, SyncError> {
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => Err(SyncError::Local {
            message: error.to_string(),
        }),
    }
}

/// 本机的文件夹（可以是 OneDrive、坚果云等网盘的同步目录，由网盘在设备之间传送）。
/// 版本标记是文件内容的 SHA-256；写入时先确认文件没有被改过，再写临时文件后改名替换。
pub(crate) struct Folder {
    dir: PathBuf,
}

impl Folder {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn missing(dir: &Path) -> SyncError {
        SyncError::FolderMissing {
            path: dir.display().to_string(),
        }
    }

    /// 文件夹本身不存在（U 盘没有插上、网盘目录被移走）时返回不可用：不能当作还没有同步文件，
    /// 否则同步过的数据会被当作云端删除了。
    fn get_blocking(dir: &Path) -> Result<Fetch, SyncError> {
        if !dir.is_dir() {
            return Err(Self::missing(dir));
        }
        let path = dir.join(FILE_NAME);
        // 过大的文件不读入内存
        match std::fs::metadata(&path) {
            Ok(meta) if meta.len() > MAX_FILE_SIZE as u64 => return Err(SyncError::TooLarge),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Fetch::Missing),
            Err(e) => return Err(SyncError::io(&e)),
        }
        let data = match std::fs::read(&path) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Fetch::Missing),
            Err(e) => return Err(SyncError::io(&e)),
        };
        if data.len() > MAX_FILE_SIZE {
            return Err(SyncError::TooLarge);
        }
        let etag = digest(&data);
        Ok(Fetch::Found { data, etag })
    }

    fn put_blocking(dir: &Path, data: &[u8], etag: Option<&str>) -> Result<Stored, SyncError> {
        match (Self::get_blocking(dir)?, etag) {
            (Fetch::Missing, None) => {}
            (Fetch::Found { etag: current, .. }, Some(etag)) if current == etag => {}
            _ => return Ok(Stored::Changed),
        }
        let path = dir.join(FILE_NAME);
        let tmp = dir.join(format!(".{FILE_NAME}.{}.part", crate::id::random_token(5)));
        let written = write_new(&tmp, data).and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                return Err(SyncError::FolderNotWritable {
                    path: dir.display().to_string(),
                });
            }
            return Err(SyncError::io(&e));
        }
        Ok(Stored::Written)
    }

    fn check_blocking(dir: &Path) -> Result<(), SyncError> {
        if !dir.is_absolute() {
            return Err(SyncError::invalid("path", "not_absolute"));
        }
        if !dir.is_dir() {
            return Err(Self::missing(dir));
        }
        let probe = dir.join(format!(
            ".termlet-sync-check-{}",
            crate::id::random_token(5)
        ));
        let result = write_new(&probe, b"ok");
        let _ = std::fs::remove_file(&probe);
        match result {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::StorageFull => {
                Err(SyncError::InsufficientStorage)
            }
            Err(_) => Err(SyncError::FolderNotWritable {
                path: dir.display().to_string(),
            }),
        }
    }
}

/// 写一个新文件（Unix 上只有本人可读写；内容是加密的，Windows 上沿用文件夹的权限，网盘、网络位置上不另设 ACL）。
fn write_new(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(data)?;
    file.sync_all()
}

impl Backend for Folder {
    fn get(&self) -> BoxFuture<'_, Result<Fetch, SyncError>> {
        let dir = self.dir.clone();
        Box::pin(blocking(move || Self::get_blocking(&dir)))
    }

    fn put<'a>(
        &'a self,
        data: &'a [u8],
        etag: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Stored, SyncError>> {
        let dir = self.dir.clone();
        let data = data.to_vec();
        let etag = etag.map(str::to_string);
        Box::pin(blocking(move || {
            Self::put_blocking(&dir, &data, etag.as_deref())
        }))
    }

    fn check(&self) -> BoxFuture<'_, Result<(), SyncError>> {
        let dir = self.dir.clone();
        Box::pin(blocking(move || Self::check_blocking(&dir)))
    }
}
