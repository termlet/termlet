//! 下载：先写本机的临时文件（"名称.termlet-part"），完成后改名（不覆盖已有的文件，重名时改为"名称 (1).扩展名"），
//! 避免留下看似完整的半截文件。

use std::fs::OpenOptions;
use std::io::{self, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use russh_sftp::protocol::{FileAttributes, OpenFlags};
use tokio::io::{AsyncSeekExt, AsyncWriteExt, BufWriter};

use super::upload::local_mtime;
use super::{FileError, PartPath, Partial, Transfer, blocking};
use crate::sftp::backend::Backend;
use crate::sftp::cancel::Cancel;
use crate::sftp::client::{PipeError, SftpClient, is_dir, is_regular, mtime_of, size_of};
use crate::sftp::error::{self, Rejection, SftpError, Side, is_permission};
use crate::sftp::names::{local_part, local_temp_part, numbered, safe_name};
use crate::sftp::os::{self, FileId};
use crate::sftp::scp::{ScpFailure, ScpRemote, scp_error};
use crate::sftp::session::SftpSession;
use crate::sftp::walk::{RemoteWalker, WalkError};

/// 本机写文件的缓冲。
const WRITE_BUFFER: usize = 512 * 1024;

/// 下载文件或目录；`resume` 为真时是继续失败的传输（写到第一次选定的位置）。
pub(super) async fn run(
    session: &SftpSession,
    t: &Arc<Transfer>,
    ctx: &Cancel,
    resume: bool,
) -> Result<(), SftpError> {
    let client = session.backend();
    let remote = t.lock().remote.clone();
    if !t.is_dir {
        let attrs = client
            .stat(&remote)
            .await
            .map_err(|e| error::remote(&remote, &e))?;
        let target = reserve_target(t).await?;
        t.set_totals(size_of(&attrs), 1);
        let saved = download_from(
            Source::of(&client),
            t,
            ctx,
            &remote,
            &target,
            &attrs,
            resume,
        )
        .await
        .map_err(FileError::into_inner)?;
        // 最终名称在下载期间被占用时改了名
        let mut s = t.lock();
        s.target = Some(saved.clone());
        s.local = saved;
        s.files_done = 1;
        return Ok(());
    }

    let target = reserve_target(t).await?;
    // 没有读权限的子目录与文件跳过（记在结果中），不中止整个任务（例如下载 /var/log 时只有 root 能读的）
    let (total, files) = scan(&client, &remote, ctx).await?;
    t.set_totals(total, files);
    let mut walker = RemoteWalker::new(&client, &remote, true);
    while let Some(item) = walker.next().await {
        if ctx.is_canceled() {
            return Err(SftpError::Canceled);
        }
        let item = match item {
            Ok(item) => item,
            Err(WalkError::Unreadable(p)) => {
                t.skip(p, None);
                continue;
            }
            Err(WalkError::Fatal(e)) => return Err(e),
        };
        let rel = item
            .path
            .strip_prefix(remote.as_str())
            .unwrap_or("")
            .trim_start_matches('/');
        let mut local = target.clone();
        for part in rel.split('/').filter(|p| !p.is_empty()) {
            local.push(safe_name(part));
        }
        if is_dir(&item.attrs) {
            let dir = local.clone();
            blocking(move || std::fs::create_dir_all(&dir))
                .await
                .map_err(|e| error::local(&local, &e))?;
        } else if is_regular(&item.attrs) {
            let source = Source::of(&client);
            match download_from(source, t, ctx, &item.path, &local, &item.attrs, resume).await {
                Ok(_) => t.file_done(),
                Err(FileError::Unreadable(_)) => t.skip(item.path, Some(size_of(&item.attrs))),
                Err(FileError::Error(e)) => return Err(e),
            }
        } else {
            // 符号链接、设备文件、命名管道、套接字等不传输，记为跳过
            t.skip_special(item.path);
        }
    }
    Ok(())
}

/// 统计目录中普通文件的总字节数与个数（读不了的子目录与文件不计）。
async fn scan(client: &Backend, root: &str, ctx: &Cancel) -> Result<(u64, u32), SftpError> {
    let mut walker = RemoteWalker::new(client, root, true);
    let (mut total, mut files) = (0u64, 0u32);
    while let Some(item) = walker.next().await {
        if ctx.is_canceled() {
            return Err(SftpError::Canceled);
        }
        match item {
            Ok(item) if is_regular(&item.attrs) => {
                total += size_of(&item.attrs);
                files += 1;
            }
            Ok(_) | Err(WalkError::Unreadable(_)) => {}
            Err(WalkError::Fatal(e)) => return Err(e),
        }
    }
    Ok((total, files))
}

/// 下载的本机位置：第一次运行（或占用已释放）时在所选目录中选定并占用（见 [`reserve_local`]），之后（继续时）沿用。
async fn reserve_target(t: &Arc<Transfer>) -> Result<PathBuf, SftpError> {
    let (target, dir, remote) = {
        let s = t.lock();
        (s.target.clone(), s.local.clone(), s.remote.clone())
    };
    if let Some(target) = target {
        return Ok(target);
    }
    let wanted = dir.join(safe_name(&t.name));
    let is_dir = t.is_dir;
    let (target, part) = blocking(move || reserve_local(&wanted, is_dir)).await?;
    let mut s = t.lock();
    s.target = Some(target.clone());
    s.local = target.clone();
    if let Some(id) = part {
        // 占用时新建的空临时文件是本任务的（见 owns_part）
        s.part = Some(Partial {
            local: target.clone(),
            remote,
            part: PartPath::Local(local_part(&target)),
            size: 0,
            mtime: 0,
            safe: 0,
            file: Some(id),
        });
    }
    Ok(target)
}

/// 选定下载的本机位置并占用它，返回位置与（下载文件时）占用的临时文件的身份。本机已有同名的文件或目录，
/// 或者有同名的临时文件（另一个任务正在写，或失败后留着供继续）时改为"名称 (1).扩展名"。
/// 下载文件时新建空的临时文件、下载目录时新建目录，都是原子操作：同时开始的任务不会选到同一个名称。
pub(crate) fn reserve_local(
    wanted: &Path,
    dir: bool,
) -> Result<(PathBuf, Option<FileId>), SftpError> {
    for i in 0..10000 {
        let candidate = numbered(wanted, i);
        match std::fs::symlink_metadata(&candidate) {
            Ok(_) => continue,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(error::local(&candidate, &e)),
        }
        if dir {
            // 有同名的临时文件时也跳过：那个名称已被文件任务占用，在这里建目录会让它最后改名时冲突
            if std::fs::symlink_metadata(local_part(&candidate)).is_ok() {
                continue;
            }
            match create_dir(&candidate) {
                Ok(()) => return Ok((candidate, None)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(error::local(&candidate, &e)),
            }
        }
        let part = local_part(&candidate);
        match open_part(&part, PartMode::CreateNew) {
            Ok(file) => return Ok((candidate, os::file_id_of(&file))),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(error::local(&candidate, &e)),
        }
    }
    Err(Rejection::NoUniqueName.into())
}

fn create_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o750).create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir(path)
    }
}

#[derive(Clone, Copy)]
enum PartMode {
    /// 接着写（继续传输）
    Append,
    /// 本任务的临时文件：截断
    Truncate,
    /// 新建，已存在时失败
    CreateNew,
}

fn open_part(part: &Path, mode: PartMode) -> io::Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.write(true);
    match mode {
        PartMode::Append => {}
        PartMode::Truncate => {
            options.create(true).truncate(true);
        }
        PartMode::CreateNew => {
            options.create_new(true);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(part)
}

/// 下载一个文件到 `local`（经临时文件），返回最终的路径。失败时保留写了数据的临时文件供继续，取消时删除。
/// `resume` 时本机已有大小与修改时间相同的文件即跳过。只截断、接着写本任务的临时文件，同名的临时文件是别的任务的
/// （正在写或失败后留着供继续）时报错，不碰它。远程文件没有读权限时返回 [`FileError::Unreadable`]。
/// 来源是 SCP 模式时不从中断处接着传（临时文件截断后从头写）。
pub(crate) async fn download_from(
    source: Source<'_>,
    t: &Arc<Transfer>,
    ctx: &Cancel,
    remote: &str,
    local: &Path,
    attrs: &FileAttributes,
    resume: bool,
) -> Result<PathBuf, FileError> {
    let resumable = matches!(source, Source::Sftp(_));
    let size = size_of(attrs);
    let mtime = mtime_of(attrs);
    // 准备：已经下载完的跳过；临时文件是不是本任务的、从哪里接着写
    let prep = {
        let t = t.clone();
        let local = local.to_path_buf();
        let remote = remote.to_string();
        blocking(move || {
            if resume
                && let Ok(st) = std::fs::metadata(&local)
                && st.is_file()
                && st.len() == size
                && local_mtime(&st) == mtime
            {
                return None;
            }
            let own = t.owns_part(&local);
            let mut offset = 0;
            if resume
                && resumable
                && own
                && let Ok(st) = std::fs::metadata(local_part(&local))
            {
                offset = t.resume_offset(&local, &remote, size, mtime, st.len());
            }
            Some((own, offset))
        })
        .await
    };
    let Some((own, offset)) = prep else {
        t.done.fetch_add(size, Ordering::Relaxed);
        return Ok(local.to_path_buf());
    };
    let result = fetch(source, t, ctx, remote, local, size, mtime, own, offset).await;
    if result.is_err() {
        let t = t.clone();
        let local = local.to_path_buf();
        blocking(move || t.drop_empty_part(&local)).await;
    }
    result
}

/// 下载的来源：SFTP，或 SCP 模式（没有断点续传）。
#[derive(Clone, Copy)]
pub(crate) enum Source<'a> {
    Sftp(&'a SftpClient),
    Scp(&'a ScpRemote),
}

impl<'a> Source<'a> {
    pub(crate) fn of(backend: &'a Backend) -> Self {
        match backend {
            Backend::Sftp(client) => Source::Sftp(client),
            Backend::Scp(remote) => Source::Scp(remote),
        }
    }
}

/// 读远程文件失败：SFTP 带确认写入的位置，SCP 没有（不接着传）。
enum ReadFailure {
    Pipe(u64, PipeError),
    Scp(ScpFailure),
}

#[allow(clippy::too_many_arguments)]
async fn fetch(
    source: Source<'_>,
    t: &Arc<Transfer>,
    ctx: &Cancel,
    remote: &str,
    local: &Path,
    size: u64,
    mtime: i64,
    own: bool,
    offset: u64,
) -> Result<PathBuf, FileError> {
    let handle = match source {
        Source::Sftp(client) => match client
            .open(remote, OpenFlags::READ, FileAttributes::empty())
            .await
        {
            Ok(handle) => Some(handle),
            Err(e) if is_permission(&e) => {
                return Err(FileError::Unreadable(error::remote(remote, &e)));
            }
            Err(e) => return Err(error::remote(remote, &e).into()),
        },
        Source::Scp(_) => None,
    };
    let close = async |handle: &Option<String>| {
        if let (Source::Sftp(client), Some(handle)) = (source, handle) {
            let _ = client.close_handle(handle).await;
        }
    };
    let overwrite = t.lock().overwrite;
    // 命令行工具（直接覆盖、不继续传输）用一次性的临时文件：固定的名称可能正被桌面端下载同一个文件时写着
    let (part, mode) = match (offset > 0, own) {
        _ if overwrite && offset == 0 => (local_temp_part(local), PartMode::CreateNew),
        (true, _) => (local_part(local), PartMode::Append),
        (false, true) => (local_part(local), PartMode::Truncate),
        (false, false) => (local_part(local), PartMode::CreateNew),
    };
    let opened = {
        let part = part.clone();
        blocking(move || {
            let file = open_part(&part, mode)?;
            let id = os::file_id_of(&file);
            if offset > 0 {
                // 确认写入的位置之后的内容不可信
                file.set_len(offset)?;
            }
            Ok::<_, io::Error>((file, id))
        })
        .await
    };
    let (file, id) = match opened {
        Ok(opened) => opened,
        Err(e) => {
            close(&handle).await;
            if e.kind() == io::ErrorKind::AlreadyExists {
                // 临时文件已存在（可能是另一个传输正在写入）
                return Err(SftpError::AlreadyExists {
                    side: Side::Local,
                    path: part.display().to_string(),
                }
                .into());
            }
            return Err(error::local(&part, &e).into());
        }
    };
    t.set_part(Some(Partial {
        local: local.to_path_buf(),
        remote: remote.to_string(),
        part: PartPath::Local(part.clone()),
        size,
        mtime,
        safe: offset,
        file: id,
    }));
    let mut file = tokio::fs::File::from_std(file);
    if offset > 0
        && let Err(e) = file.seek(SeekFrom::Start(offset)).await
    {
        close(&handle).await;
        return Err(error::local(&part, &e).into());
    }
    t.done.fetch_add(offset, Ordering::Relaxed);
    let mut writer = BufWriter::with_capacity(WRITE_BUFFER, file);
    let read = match (source, &handle) {
        (Source::Sftp(client), Some(handle)) => client
            .read_into(handle, offset, &mut writer, ctx, &t.done, None)
            .await
            .map_err(|f| ReadFailure::Pipe(f.safe, f.error)),
        (Source::Scp(scp), _) => scp
            .download(remote, &mut writer, ctx, &t.done, None)
            .await
            .map(|(_, size)| size)
            .map_err(ReadFailure::Scp),
        (Source::Sftp(_), None) => unreachable!("SFTP 已打开文件"),
    };
    close(&handle).await;
    let flushed = writer.flush().await;
    let mut file = writer.into_inner();
    let _ = file.flush().await;
    drop(file);
    let failure = match (read, flushed) {
        (Ok(_), Ok(())) => None,
        (Ok(end), Err(e)) => Some(ReadFailure::Pipe(end, PipeError::Local(e))),
        (Err(f), _) => Some(f),
    };
    if let Some(failure) = failure {
        let canceled = ctx.is_canceled()
            || matches!(
                failure,
                ReadFailure::Pipe(_, PipeError::Canceled) | ReadFailure::Scp(ScpFailure::Canceled)
            );
        // 取消时、SCP（不能接着传）出错时删除临时文件
        if canceled || matches!(failure, ReadFailure::Scp(_)) {
            let _ = tokio::fs::remove_file(&part).await;
            t.set_part(None);
        }
        if canceled {
            return Err(SftpError::Canceled.into());
        }
        let (safe, error) = match failure {
            ReadFailure::Scp(ScpFailure::Local(e)) => return Err(error::local(&part, &e).into()),
            ReadFailure::Scp(other) => {
                let error = scp_error(remote, other);
                return Err(match error {
                    SftpError::PermissionDenied { .. } => FileError::Unreadable(error),
                    other => other.into(),
                });
            }
            ReadFailure::Pipe(safe, error) => (safe, error),
        };
        // 本机按顺序写入：写进文件的部分都可信
        let len = tokio::fs::metadata(&part)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        t.set_safe(safe.min(len));
        return Err(match error {
            PipeError::Remote(e) => error::remote(remote, &e),
            PipeError::Local(e) => error::local(&part, &e),
            PipeError::Canceled => SftpError::Canceled,
            PipeError::TooLarge => SftpError::failed(Side::Remote, remote, "超过上限"),
        }
        .into());
    }
    let committed = {
        let part = part.clone();
        let local = local.to_path_buf();
        blocking(move || commit_part(overwrite, &part, &local)).await
    };
    let saved = match committed {
        Ok(saved) => saved,
        Err(e) => {
            let _ = tokio::fs::remove_file(&part).await;
            t.set_part(None);
            return Err(e.into());
        }
    };
    t.set_part(None);
    let stamp = saved.clone();
    blocking(move || {
        let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime.max(0) as u64);
        if let Ok(file) = OpenOptions::new().write(true).open(&stamp) {
            let _ = file.set_modified(time);
        }
    })
    .await;
    Ok(saved)
}

/// 把下载完成的临时文件改名为 `local`，返回最终的路径。不覆盖已有的文件或目录：下载期间别的程序（或用户、rz、trzsz）
/// 在 `local` 建了文件时改为"名称 (1).扩展名"，跳过别的任务占用的名称（有它的临时文件）。`overwrite`（命令行工具）时直接覆盖。
pub(crate) fn commit_part(
    overwrite: bool,
    part: &Path,
    local: &Path,
) -> Result<PathBuf, SftpError> {
    if overwrite {
        std::fs::rename(part, local).map_err(|e| error::local(local, &e))?;
        return Ok(local.to_path_buf());
    }
    for i in 0..10000 {
        let candidate = numbered(local, i);
        if i > 0 && std::fs::symlink_metadata(local_part(&candidate)).is_ok() {
            continue;
        }
        match os::rename_no_replace(part, &candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(error::local(local, &e)),
        }
    }
    Err(Rejection::NoUniqueName.into())
}
