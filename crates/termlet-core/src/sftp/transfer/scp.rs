//! SCP 模式（服务器没有 SFTP）的上传：一次一个文件，先写同目录下的临时文件，完成后 `mv` 成正式文件名，
//! 覆盖已有文件时沿用它的权限，修改时间随文件头一起设置。没有断点续传：失败、取消时删除临时文件，继续时从头传。
//! 下载见 [`super::download`]（本机一侧与 SFTP 相同）。

use std::fs::Metadata;
use std::path::Path;

use tokio::io::BufReader;

use super::upload::local_mtime;
use super::{FileError, PartPath, Partial, Transfer, claim_remote_part};
use crate::sftp::cancel::Cancel;
use crate::sftp::client::{is_regular, is_symlink, mtime_of, perm_bits, size_of};
use crate::sftp::error::{self, SftpError};
use crate::sftp::manager::Inner;
use crate::sftp::scp::{ScpFailure, ScpRemote, scp_error};

/// 本机读文件的缓冲。
const READ_BUFFER: usize = 512 * 1024;
/// 新文件的权限（服务器上的 scp 再按 umask 去掉一些位）。
const NEW_FILE_MODE: u32 = 0o644;

/// 上传一个文件（见模块说明）。`resume` 时远程已有大小与修改时间相同的文件即跳过。
/// 本机文件没有读权限时返回 [`FileError::Unreadable`]。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn upload_file(
    inner: Option<&Inner>,
    remote: &ScpRemote,
    t: &Transfer,
    ctx: &Cancel,
    local: &Path,
    target: &str,
    meta: &Metadata,
    resume: bool,
) -> Result<(), FileError> {
    let size = meta.len();
    let mtime = local_mtime(meta);
    let mut target = target.to_string();
    let mut existing = remote.lstat(&target).await.ok();
    // 目标是指向普通文件的符号链接：写到它指向的文件，链接保持不变
    if existing.as_ref().is_some_and(is_symlink)
        && let Ok(resolved) = remote.resolve_link(&target).await
        && let Ok(st) = remote.stat(&resolved).await
        && is_regular(&st)
    {
        existing = Some(st);
        target = resolved;
    }
    let regular = existing.as_ref().filter(|e| is_regular(e));
    if resume && regular.is_some_and(|e| size_of(e) == size && mtime_of(e) == mtime) {
        t.done.fetch_add(size, std::sync::atomic::Ordering::Relaxed);
        return Ok(());
    }
    let file = match tokio::fs::File::open(local).await {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(FileError::Unreadable(error::local(local, &e)));
        }
        Err(e) => return Err(error::local(local, &e).into()),
    };
    let keep_mode = regular.map(perm_bits);
    let mut p = Partial {
        local: local.to_path_buf(),
        remote: target.clone(),
        part: PartPath::Remote(String::new()),
        size,
        mtime,
        safe: 0,
        file: None,
    };
    claim_remote_part(inner, t, &mut p);
    let part = p.remote_part().unwrap_or_default().to_string();
    let mut reader = BufReader::with_capacity(READ_BUFFER, file);
    let sent = remote
        .upload(
            &part,
            keep_mode.unwrap_or(NEW_FILE_MODE),
            Some(mtime),
            size,
            &mut reader,
            ctx,
            &t.done,
        )
        .await;
    if let Err(failure) = sent {
        let _ = remote.remove_file(&part).await;
        t.set_part(None);
        if ctx.is_canceled() {
            return Err(SftpError::Canceled.into());
        }
        return Err(match failure {
            ScpFailure::Local(e) => error::local(local, &e),
            other => scp_error(&target, other),
        }
        .into());
    }
    t.set_safe(size);
    if let Err(e) = remote.commit_upload(&part, &target, keep_mode).await {
        let _ = remote.remove_file(&part).await;
        t.set_part(None);
        return Err(error::remote(&target, &e).into());
    }
    t.set_part(None);
    Ok(())
}
