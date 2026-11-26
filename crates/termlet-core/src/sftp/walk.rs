//! 遍历目录（先序，同一目录中按名称排序）：根路径是指向目录的符号链接（Windows 上也可以是目录联接）时进入它指向的目录，
//! 子项中的链接不跟随（避免循环）。目录传输、递归修改权限用。

use std::fs::Metadata;
use std::io;
use std::path::{Path, PathBuf};

use russh_sftp::protocol::FileAttributes;

use super::SftpError;
use super::backend::Backend;
use super::client::is_dir;
use super::error::{self, is_permission};
use super::local::is_wsl_special;
use super::path;

/// 遍历中遇到的问题。
pub(crate) enum WalkError {
    /// 子目录没有读权限（调用方允许跳过时）：它本身已经交给调用方，里面的内容跳过
    Unreadable(String),
    /// 中止遍历
    Fatal(SftpError),
}

/// 远程的一项。根路径的属性跟随符号链接，子项的不跟随。
pub(crate) struct RemoteItem {
    pub path: String,
    pub attrs: FileAttributes,
}

pub(crate) struct RemoteWalker<'a> {
    client: &'a Backend,
    root: String,
    started: bool,
    stack: Vec<std::vec::IntoIter<RemoteItem>>,
    /// 刚交给调用方的目录：下一步读出它的内容（是不是根目录）
    descend: Option<(String, bool)>,
    skip_unreadable: bool,
}

impl<'a> RemoteWalker<'a> {
    /// `skip_unreadable` 为真时读不了的子目录报告为 [`WalkError::Unreadable`] 后继续，否则中止；根目录读不了总是中止。
    pub(crate) fn new(client: &'a Backend, root: &str, skip_unreadable: bool) -> Self {
        Self {
            client,
            root: root.to_string(),
            started: false,
            stack: Vec::new(),
            descend: None,
            skip_unreadable,
        }
    }

    pub(crate) async fn next(&mut self) -> Option<Result<RemoteItem, WalkError>> {
        if !self.started {
            self.started = true;
            return Some(match self.client.stat(&self.root).await {
                Ok(attrs) => {
                    if is_dir(&attrs) {
                        self.descend = Some((self.root.clone(), true));
                    }
                    Ok(RemoteItem {
                        path: self.root.clone(),
                        attrs,
                    })
                }
                Err(e) => Err(WalkError::Fatal(error::remote(&self.root, &e))),
            });
        }
        if let Some((dir, is_root)) = self.descend.take() {
            match self.client.read_dir(&dir).await {
                Ok(mut items) => {
                    items.sort_by(|a, b| a.name.cmp(&b.name));
                    let items: Vec<RemoteItem> = items
                        .into_iter()
                        .map(|item| RemoteItem {
                            path: path::join(&dir, &item.name),
                            attrs: item.attrs,
                        })
                        .collect();
                    self.stack.push(items.into_iter());
                }
                Err(e) if !is_root && self.skip_unreadable && is_permission(&e) => {
                    return Some(Err(WalkError::Unreadable(dir)));
                }
                Err(e) => {
                    self.stack.clear();
                    return Some(Err(WalkError::Fatal(error::remote(&dir, &e))));
                }
            }
        }
        loop {
            let top = self.stack.last_mut()?;
            match top.next() {
                Some(item) => {
                    if is_dir(&item.attrs) {
                        self.descend = Some((item.path.clone(), false));
                    }
                    return Some(Ok(item));
                }
                None => {
                    self.stack.pop();
                }
            }
        }
    }
}

/// 本机一项的种类。
pub(crate) enum LocalKind {
    Dir,
    File(io::Result<Metadata>),
    /// 符号链接（含 Windows 的目录联接）、设备文件、命名管道、套接字等
    Other,
}

pub(crate) struct LocalItem {
    pub path: PathBuf,
    pub kind: LocalKind,
}

pub(crate) struct LocalWalker {
    root: PathBuf,
    started: bool,
    stack: Vec<std::vec::IntoIter<LocalItem>>,
    descend: Option<(PathBuf, bool)>,
}

impl LocalWalker {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            started: false,
            stack: Vec::new(),
            descend: None,
        }
    }

    /// 读不了的子目录报告为 [`WalkError::Unreadable`] 后继续；根目录读不了时中止。
    pub(crate) async fn next(&mut self) -> Option<Result<LocalItem, WalkError>> {
        if !self.started {
            self.started = true;
            return Some(match tokio::fs::metadata(&self.root).await {
                Ok(meta) if meta.is_dir() => {
                    self.descend = Some((self.root.clone(), true));
                    Ok(LocalItem {
                        path: self.root.clone(),
                        kind: LocalKind::Dir,
                    })
                }
                Ok(meta) if meta.is_file() && !is_wsl_special(&self.root, &meta) => Ok(LocalItem {
                    path: self.root.clone(),
                    kind: LocalKind::File(Ok(meta)),
                }),
                Ok(_) => Ok(LocalItem {
                    path: self.root.clone(),
                    kind: LocalKind::Other,
                }),
                Err(e) => Err(WalkError::Fatal(error::local(&self.root, &e))),
            });
        }
        if let Some((dir, is_root)) = self.descend.take() {
            match read_local_dir(&dir).await {
                Ok(items) => self.stack.push(items.into_iter()),
                Err(e) if !is_root && e.kind() == io::ErrorKind::PermissionDenied => {
                    return Some(Err(WalkError::Unreadable(dir.display().to_string())));
                }
                Err(e) => {
                    self.stack.clear();
                    return Some(Err(WalkError::Fatal(error::local(&dir, &e))));
                }
            }
        }
        loop {
            let top = self.stack.last_mut()?;
            match top.next() {
                Some(item) => {
                    if matches!(item.kind, LocalKind::Dir) {
                        self.descend = Some((item.path.clone(), false));
                    }
                    return Some(Ok(item));
                }
                None => {
                    self.stack.pop();
                }
            }
        }
    }
}

/// 读出目录的内容（不跟随链接），按名称排序。
async fn read_local_dir(dir: &Path) -> io::Result<Vec<LocalItem>> {
    let mut reader = tokio::fs::read_dir(dir).await?;
    let mut items = Vec::new();
    while let Some(entry) = reader.next_entry().await? {
        let path = entry.path();
        let kind = match entry.file_type().await {
            Ok(t) if t.is_symlink() => LocalKind::Other,
            Ok(t) if t.is_dir() => LocalKind::Dir,
            Ok(t) if t.is_file() => {
                let meta = entry.metadata().await;
                // WSL 的位置中的命名管道等在 Windows 一侧也是“文件”，读它会一直等到超时
                if meta.as_ref().is_ok_and(|m| is_wsl_special(&path, m)) {
                    LocalKind::Other
                } else {
                    LocalKind::File(meta)
                }
            }
            Ok(_) => LocalKind::Other,
            Err(e) => LocalKind::File(Err(e)),
        };
        items.push(LocalItem { path, kind });
    }
    items.sort_by(|a, b| a.path.file_name().cmp(&b.path.file_name()));
    Ok(items)
}
