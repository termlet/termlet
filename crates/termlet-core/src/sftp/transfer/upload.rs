//! 上传：先写远程的临时文件（见 [`super::claim_remote_part`]），完成后改名（posix-rename，原子地覆盖），
//! 覆盖已有文件时沿用它的权限。

use std::fs::Metadata;
use std::io::SeekFrom;
use std::path::Path;
use std::sync::Arc;

use russh_sftp::client::error::Error as ClientError;
use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};
use tokio::io::{AsyncSeekExt, BufReader};

use super::{FileError, PartPath, Partial, Transfer, claim_remote_part};
use crate::sftp::backend::Backend;
use crate::sftp::cancel::Cancel;
use crate::sftp::client::{
    PipeError, SftpClient, is_dir, is_regular, mtime_of, perm_attrs, perm_bits, size_attrs,
    size_of, time_attrs,
};
use crate::sftp::error::{self, SftpError, Side, status_of};
use crate::sftp::manager::Inner;
use crate::sftp::names::{random_hex, remote_name_fits, remote_temp_part, tagged_name};
use crate::sftp::path;
use crate::sftp::session::{SftpSession, link_target, mkdir_all};
use crate::sftp::walk::{LocalKind, LocalWalker, WalkError};

/// 本机读文件的缓冲（每次读这么多再分块发出）。
const READ_BUFFER: usize = 512 * 1024;

/// 上传文件或目录；`resume` 为真时是继续失败的传输。
/// 没有同意覆盖时远程已存在同名项即失败：第一次运行时检查；继续时只有上次就是因此失败的才重新检查
/// （其他原因失败的，远程的同名项是上次运行建的，例如目录上传中断前已建好的目录）。
pub(super) async fn run(
    inner: &Arc<Inner>,
    session: &SftpSession,
    t: &Arc<Transfer>,
    ctx: &Cancel,
    resume: bool,
) -> Result<(), SftpError> {
    let client = session.backend();
    let (local, remote, check) = {
        let s = t.lock();
        (
            s.local.clone(),
            s.remote.clone(),
            !s.overwrite && (!resume || s.conflict),
        )
    };
    if check && client.lstat(&remote).await.is_ok() {
        t.lock().conflict = true;
        return Err(SftpError::AlreadyExists {
            side: Side::Remote,
            path: remote,
        });
    }
    t.lock().conflict = false;
    if !t.is_dir {
        let meta = tokio::fs::metadata(&local)
            .await
            .map_err(|e| error::local(&local, &e))?;
        t.set_totals(meta.len(), 1);
        upload_one(inner, &client, t, ctx, &local, &remote, &meta, resume)
            .await
            .map_err(FileError::into_inner)?;
        t.file_done();
        return Ok(());
    }

    // 目录：先统计总量，再逐个创建目录、上传文件。没有读权限的子目录与文件跳过（记在结果中），不中止整个任务；
    // 所选的目录本身读不了时照常失败
    let (total, files) = scan(&local, ctx).await?;
    t.set_totals(total, files);
    let mut walker = LocalWalker::new(&local);
    while let Some(item) = walker.next().await {
        if ctx.is_canceled() {
            return Err(SftpError::Canceled);
        }
        let item = match item {
            Ok(item) => item,
            Err(WalkError::Unreadable(p)) => {
                // 读不了的子目录（之前已在远程建好它，里面是空的）
                t.skip(p, None);
                continue;
            }
            Err(WalkError::Fatal(e)) => return Err(e),
        };
        let target = remote_of(&local, &remote, &item.path);
        match item.kind {
            LocalKind::Dir => match &client {
                Backend::Sftp(sftp) => mkdir_all(sftp, &target).await?,
                Backend::Scp(scp) => scp
                    .mkdir_all(&target)
                    .await
                    .map_err(|e| error::remote(&target, &e))?,
            },
            LocalKind::File(meta) => {
                let meta = meta.map_err(|e| error::local(&item.path, &e))?;
                match upload_one(inner, &client, t, ctx, &item.path, &target, &meta, resume).await {
                    Ok(()) => t.file_done(),
                    Err(FileError::Unreadable(_)) => {
                        t.skip(item.path.display().to_string(), Some(meta.len()))
                    }
                    Err(FileError::Error(e)) => return Err(e),
                }
            }
            // 符号链接（含目录联接）、设备文件、命名管道、套接字等不传输，记为跳过
            LocalKind::Other => t.skip_special(item.path.display().to_string()),
        }
    }
    Ok(())
}

/// 上传一个文件：SFTP 或 SCP 模式。
#[allow(clippy::too_many_arguments)]
async fn upload_one(
    inner: &Inner,
    backend: &Backend,
    t: &Transfer,
    ctx: &Cancel,
    local: &Path,
    remote: &str,
    meta: &Metadata,
    resume: bool,
) -> Result<(), FileError> {
    match backend {
        Backend::Sftp(client) => {
            upload_file(Some(inner), client, t, ctx, local, remote, meta, resume).await
        }
        Backend::Scp(scp) => {
            super::scp::upload_file(Some(inner), scp, t, ctx, local, remote, meta, resume).await
        }
    }
}

/// 本机 `item` 在远程的位置（`root` 对应 `remote`）。
fn remote_of(root: &Path, remote: &str, item: &Path) -> String {
    let rel = item.strip_prefix(root).unwrap_or(Path::new(""));
    let mut target = remote.to_string();
    for part in rel.components() {
        target = path::join(&target, &part.as_os_str().to_string_lossy());
    }
    target
}

/// 统计目录中普通文件的总字节数与个数（读不了的子目录与文件不计）。
async fn scan(root: &Path, ctx: &Cancel) -> Result<(u64, u32), SftpError> {
    let mut walker = LocalWalker::new(root);
    let (mut total, mut files) = (0u64, 0u32);
    while let Some(item) = walker.next().await {
        if ctx.is_canceled() {
            return Err(SftpError::Canceled);
        }
        match item {
            Ok(item) => {
                if let LocalKind::File(Ok(meta)) = item.kind {
                    total += meta.len();
                    files += 1;
                }
            }
            Err(WalkError::Unreadable(_)) => {}
            Err(WalkError::Fatal(e)) => return Err(e),
        }
    }
    Ok((total, files))
}

/// 本机文件的修改时间（Unix 秒）。
pub(crate) fn local_mtime(meta: &Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 上传一个文件：先写远程的临时文件，完成后改名，覆盖已有文件时沿用它的权限。
/// 失败时保留临时文件供继续传输，取消时删除。`resume` 时远程已有大小与修改时间相同的文件即跳过。
/// 本机文件没有读权限时返回 [`FileError::Unreadable`]。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn upload_file(
    inner: Option<&Inner>,
    client: &SftpClient,
    t: &Transfer,
    ctx: &Cancel,
    local: &Path,
    remote: &str,
    meta: &Metadata,
    resume: bool,
) -> Result<(), FileError> {
    let size = meta.len();
    let mtime = local_mtime(meta);
    let mut remote = remote.to_string();
    let mut existing = client.lstat(&remote).await.ok();
    // 目标是符号链接：写到它指向的文件旁边再改名，链接保持不变
    if let Some(target) = link_target(client, &remote).await {
        existing = client.stat(&target).await.ok();
        remote = target;
    }
    if resume
        && let Some(e) = &existing
        && is_regular(e)
        && size_of(e) == size
        && mtime_of(e) == mtime
    {
        // 已经传完（例如期间另一个任务上传了同一个文件）：本任务上次留下的临时文件用不着了
        if let Some(prev) = t.remote_part_of(local, &remote) {
            let _ = client.remove(&prev).await;
            t.set_part(None);
        }
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

    let mut p = Partial {
        local: local.to_path_buf(),
        remote: remote.clone(),
        part: PartPath::Remote(String::new()),
        size,
        mtime,
        safe: 0,
        file: None,
    };
    let mut offset = 0;
    match t.remote_part_of(local, &remote).filter(|_| resume) {
        Some(prev) => {
            // 继续：沿用上次（仍由本任务占用）的临时文件，从中断处接着传
            if let Ok(st) = client.stat(&prev).await {
                offset = t.resume_offset(local, &remote, size, mtime, size_of(&st));
            }
            p.part = PartPath::Remote(prev);
            p.safe = offset;
            t.set_part(Some(p.clone()));
        }
        None => claim_remote_part(inner, t, &mut p),
    }
    let handle = if offset > 0 {
        let part = p.remote_part().unwrap_or_default().to_string();
        client
            .open(&part, OpenFlags::WRITE, FileAttributes::empty())
            .await
    } else {
        let mode = existing.as_ref().filter(|e| is_regular(e)).map(perm_bits);
        create_part(client, t, &mut p, mode).await
    }
    .map_err(|e| error::remote(&remote, &e))?;
    let part = p.remote_part().unwrap_or_default().to_string();
    let mut file = file;
    if offset > 0
        && let Err(e) = file.seek(SeekFrom::Start(offset)).await
    {
        let _ = client.close_handle(&handle).await;
        return Err(error::local(local, &e).into());
    }
    t.done
        .fetch_add(offset, std::sync::atomic::Ordering::Relaxed);
    let mut reader = BufReader::with_capacity(READ_BUFFER, file);
    let written = client
        .write_from(&handle, offset, &mut reader, ctx, &t.done)
        .await;
    let closed = client.close_handle(&handle).await;
    let failure = match (written, closed) {
        (Ok(_), Ok(())) => None,
        (Ok(end), Err(e)) => Some((end, PipeError::Remote(e))),
        (Err(f), _) => Some((f.safe, f.error)),
    };
    if let Some((safe, error)) = failure {
        if ctx.is_canceled() || matches!(error, PipeError::Canceled) {
            // 取消：删除临时文件
            let _ = client.remove(&part).await;
            t.set_part(None);
            return Err(SftpError::Canceled.into());
        }
        t.set_safe(safe);
        // 连接还在时把临时文件截到确认写入的长度（断开时截不了，继续时取两者中较小的位置）
        let _ = client.setstat(&part, size_attrs(safe)).await;
        return Err(match error {
            PipeError::Remote(e) => write_error(client, &remote, &part, &e).await,
            PipeError::Local(e) => error::local(local, &e),
            PipeError::Canceled => SftpError::Canceled,
            PipeError::TooLarge => SftpError::failed(Side::Remote, &remote, "超过上限"),
        }
        .into());
    }

    // 内容已全部写入：之后改名失败时，继续传输只需重新改名
    t.set_safe(size);
    if let Some(e) = existing.as_ref().filter(|e| is_regular(e)) {
        let _ = client.setstat(&part, perm_attrs(perm_bits(e))).await;
    }
    if let Err(e) = client.posix_rename(&part, &remote).await {
        if !client.rename_unsupported(&e) {
            // 改名失败（没有权限、连接断开等）：原文件与临时文件都保留，可以继续（重新改名）
            return Err(error::remote(&remote, &e).into());
        }
        // 服务器不支持 posix-rename：经备份替换原文件（目录不替换，改名照常失败）。失败时原文件恢复、临时文件保留，
        // 可以继续（重新改名）
        let replaces = existing.as_ref().is_some_and(|e| !is_dir(e));
        rename_replacing(client, &part, &remote, replaces).await?;
    }
    t.set_part(None);
    let _ = client.setstat(&remote, time_attrs(mtime)).await;
    Ok(())
}

/// 新建上传用的远程临时文件 `p.part`：先删掉以前留下的同名文件，再带 EXCLUDE 创建（不会写进别人预先放好的
/// 同名符号链接），覆盖已有文件时创建时就给它的权限 `mode`（覆盖 0600 的文件时，临时文件中的新内容不会被其他用户读到）。
/// 同名文件删不掉、仍然存在时改用一次性的随机名称（记在 `p` 与任务上）。
async fn create_part(
    client: &SftpClient,
    t: &Transfer,
    p: &mut Partial,
    mode: Option<u32>,
) -> Result<String, ClientError> {
    let attrs = || mode.map(perm_attrs).unwrap_or_else(FileAttributes::empty);
    let flags = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE;
    let part = p.remote_part().unwrap_or_default().to_string();
    let _ = client.remove(&part).await;
    match client.open(&part, flags, attrs()).await {
        Ok(handle) => Ok(handle),
        Err(_) if client.lstat(&part).await.is_ok() => {
            let other = remote_temp_part(&p.remote);
            p.part = PartPath::Remote(other.clone());
            t.set_part(Some(p.clone()));
            client.open(&other, flags, attrs()).await
        }
        Err(e) => Err(e),
    }
}

/// 写入失败的原因：服务器只说"失败"时查一下可用空间，空间已满就报告空间不足。
pub(crate) async fn write_error(
    client: &SftpClient,
    remote: &str,
    part: &str,
    e: &ClientError,
) -> SftpError {
    if status_of(e) == Some(StatusCode::Failure)
        && let Some(free) = client.free_space(&path::parent(part)).await
        && free < 1024 * 1024
    {
        return SftpError::NoSpace {
            side: Side::Remote,
            path: remote.to_string(),
        };
    }
    error::remote(remote, e)
}

/// 服务器不支持 posix-rename 时用普通的改名（不能覆盖已有的文件）把 `src` 改名为 `dst`：`exists`（`dst` 是已有的文件）时
/// 先把 `dst` 改名为备份，再把 `src` 改名为 `dst`，最后删除备份。不"先删除原文件再改名"：之后改名失败时原文件就丢了。
/// 第二步失败时把备份改回 `dst`，`src` 留给调用方处理；改不回时报告 [`SftpError::ReplaceFailed`]（带备份的路径）。
pub(crate) async fn rename_replacing(
    client: &SftpClient,
    src: &str,
    dst: &str,
    exists: bool,
) -> Result<(), SftpError> {
    if !exists {
        return client
            .rename(src, dst)
            .await
            .map_err(|e| error::remote(dst, &e));
    }
    let (dir, name) = path::split(dst);
    let backup = format!(
        "{dir}{}",
        tagged_name(
            &format!(".{name}"),
            &format!(".termlet-{}", random_hex(4)),
            ".bak",
            remote_name_fits
        )
    );
    client
        .rename(dst, &backup)
        .await
        .map_err(|e| error::remote(dst, &e))?;
    if let Err(e) = client.rename(src, dst).await {
        if let Err(back) = client.rename(&backup, dst).await {
            return Err(SftpError::ReplaceFailed {
                path: dst.to_string(),
                backup,
                kept: None,
                reason: format!("{e}；改回时：{back}"),
            });
        }
        return Err(error::remote(dst, &e));
    }
    let _ = client.remove(&backup).await;
    Ok(())
}
