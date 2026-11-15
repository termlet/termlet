//! 传输：上传、下载文件或目录（目录递归）。每个会话同时进行 3 个，其余排队；进度与速度、取消、继续（断点续传）、
//! 断线重连后自动继续、清除与自动清除、一批结束时的汇总（见 [`batch`]）。
//!
//! - 临时文件：先写"名称.termlet-part"（上传在远程、下载在本机），完成后改名为正式文件名；失败时保留，继续时从中断处
//!   接着传（来源的大小与修改时间没变才接着传）；取消时删除。下载改名时不覆盖已有的文件（改为"名称 (1).扩展名"），
//!   上传覆盖已有文件要用户同意（`overwrite`），覆盖时沿用原文件的权限；目标是符号链接时写到它指向的文件。
//! - 目录传输中没有读权限的子目录与文件、文件类型不支持的项（符号链接、设备文件、命名管道、套接字）跳过并记在结果中。
//! - 记录属于窗口（`owner`），每个窗口保留最近 200 条已结束的；被移除的记录不能再继续，失败时留下的临时文件一并删除。

pub mod batch;
pub(crate) mod download;
pub(crate) mod scp;
pub(crate) mod upload;

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use super::cancel::Cancel;
use super::client::is_dir;
use super::error::{self, Rejection, SftpError};
use super::manager::{Inner, SftpManager};
use super::names::{local_part, remote_part, remote_temp_part};
use super::os::{self, FileId};
use super::path;
use super::session::{SessionId, SftpSession};
use batch::BatchId;

pub(crate) use upload::upload_file;

/// 传输记录中保留的跳过项路径数（只用于提示，总数见 [`TransferView::skipped`]）。
const MAX_SKIPPED_PATHS: usize = 20;

/// 传输任务的 ID（`t_` 加随机串）。
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TransferId(String);

impl TransferId {
    fn new() -> Self {
        Self(format!("t_{}", crate::id::random_token(8)))
    }

    #[cfg(test)]
    pub(crate) fn from_raw(raw: &str) -> Self {
        Self(raw.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TransferId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for TransferId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    Upload,
    Download,
}

/// 传输的状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TransferState {
    /// 排队等待空位
    Queued,
    Running,
    /// 因连接断开而中断，会话正在自动重连，重连后从中断处自动继续
    WaitingReconnect,
    Done,
    /// 失败（可以继续：`resumable` 时从中断处接着传，否则从头传）
    Failed,
    Canceled,
}

impl TransferState {
    /// 已结束（成功、失败、取消）。
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Canceled)
    }

    /// 还没结束：排队、传输中，或等待重连后自动继续。
    pub fn is_pending(self) -> bool {
        !self.is_finished()
    }
}

/// 传输任务的快照。
#[derive(Clone, Debug, PartialEq)]
pub struct TransferView {
    pub id: TransferId,
    /// 所用的会话（继续时可以换成同一台主机的新会话）
    pub session: SessionId,
    pub host: String,
    pub owner: String,
    /// 所属的批（见 `batch`）；继续时可能加入新的一批
    pub batch: BatchId,
    pub direction: Direction,
    /// 显示用的文件或目录名
    pub name: String,
    /// 本机一侧：上传时是来源；下载时开始前是所选的目录，开始后是选定的保存位置（重名时改了名）
    pub local: PathBuf,
    /// 远程一侧（解析后的绝对路径）：上传时是目标，下载时是来源
    pub remote: String,
    pub is_dir: bool,
    /// 总字节数（目录在扫描完成前为 0）
    pub total: u64,
    /// 已传输的字节数
    pub done: u64,
    pub files: u32,
    pub files_done: u32,
    /// 速度（字节/秒，最近约 2 秒的平均），不在传输时为 0
    pub speed: u64,
    pub state: TransferState,
    /// 失败的原因
    pub error: Option<SftpError>,
    /// 新建与结束的时间（Unix 毫秒）
    pub created: i64,
    pub finished: Option<i64>,
    /// 失败时留下了临时文件：继续时从中断处接着传
    pub resumable: bool,
    /// 目录传输中跳过的项数：没有读权限的，以及文件类型不支持的（其数目是 `skipped_special`）。
    /// 有跳过项的记录不自动清除
    pub skipped: u32,
    pub skipped_special: u32,
    /// 跳过的项中前 20 项的路径（来源一侧的）
    pub skipped_paths: Vec<String>,
    /// 继续过的次数
    pub resumed: u32,
}

/// 上传本机的文件或目录到远程目录。
#[derive(Clone, Debug, Default)]
pub struct UploadRequest {
    /// 本机的绝对路径
    pub local_paths: Vec<PathBuf>,
    /// 远程目录（相对路径与 `~` 按会话的初始目录解析）
    pub remote_dir: String,
    /// 用户同意覆盖远程的同名项；为假时远程已存在同名项则该项失败（[`SftpError::AlreadyExists`]），
    /// 用户确认后用 [`ResumeOptions::overwrite`] 继续
    pub overwrite: bool,
}

/// 下载远程的文件或目录到本机目录；本机已存在同名项时自动改名为"名称 (1)"。
#[derive(Clone, Debug, Default)]
pub struct DownloadRequest {
    pub remote_paths: Vec<String>,
    /// 本机的绝对路径，必须是已存在的目录
    pub local_dir: PathBuf,
}

/// 继续传输的选项。
#[derive(Clone, Debug, Default)]
pub struct ResumeOptions {
    /// 要使用的会话：为空时用原来的会话；重新打开会话后是新的会话，须是同一台主机
    pub session: Option<SessionId>,
    /// 用户同意覆盖远程的同名项（上传因"远程已存在"失败后确认覆盖）。不同意时这样的任务继续前重新检查，
    /// 远程仍存在就仍以"已存在"失败。下载不使用
    pub overwrite: bool,
}

/// 选出要清除的已结束传输记录；为空的条件不限制。
#[derive(Clone, Debug, Default)]
pub struct ClearFilter {
    pub owner: Option<String>,
    pub host: Option<String>,
    /// 要清除的状态；为空时是全部已结束的（成功、失败、取消），不含等待重连的
    pub states: Vec<TransferState>,
    /// 只清除这一条（失败的行内的"移除"）
    pub id: Option<TransferId>,
}

impl ClearFilter {
    fn matches(&self, view: &TransferView) -> bool {
        self.owner.as_ref().is_none_or(|o| *o == view.owner)
            && self.host.as_ref().is_none_or(|h| *h == view.host)
            && self.id.as_ref().is_none_or(|id| *id == view.id)
            && if self.states.is_empty() {
                view.state.is_finished()
            } else {
                self.states.contains(&view.state)
            }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Queued,
    Running,
    Done,
    Failed,
    Canceled,
}

impl Phase {
    pub(crate) fn finished(self) -> bool {
        matches!(self, Phase::Done | Phase::Failed | Phase::Canceled)
    }
}

/// 临时文件的位置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PartPath {
    Remote(String),
    Local(PathBuf),
}

/// 中断时正在传输的文件：继续前核对来源的大小与修改时间，没变才接着传。
#[derive(Clone, Debug)]
pub(crate) struct Partial {
    pub local: PathBuf,
    pub remote: String,
    /// 它的临时文件（上传时在远程，由任务占用，继续时沿用；下载时在本机）
    pub part: PartPath,
    pub size: u64,
    pub mtime: i64,
    /// 临时文件中确认已写入的长度。上传时并发写入，出错后临时文件的长度不可信，继续时从它与临时文件的长度中较小的位置接着传
    pub safe: u64,
    /// 下载时本机的临时文件的身份（继续、删除前确认还是本任务的那个文件）
    pub file: Option<FileId>,
}

impl Partial {
    pub(crate) fn remote_part(&self) -> Option<&str> {
        match &self.part {
            PartPath::Remote(p) => Some(p),
            PartPath::Local(_) => None,
        }
    }
}

/// 单个文件的失败：来源没有读权限（目录传输中跳过这个文件，单个文件时照常失败），或其他错误。
pub(crate) enum FileError {
    Unreadable(SftpError),
    Error(SftpError),
}

impl FileError {
    pub(crate) fn into_inner(self) -> SftpError {
        match self {
            FileError::Unreadable(e) | FileError::Error(e) => e,
        }
    }
}

impl From<SftpError> for FileError {
    fn from(error: SftpError) -> Self {
        FileError::Error(error)
    }
}

/// 速度：最近约 2 秒的采样。
#[derive(Default)]
struct Rate {
    samples: VecDeque<(Instant, u64)>,
}

impl Rate {
    fn sample(&mut self, now: Instant, done: u64) -> u64 {
        if self
            .samples
            .back()
            .is_none_or(|(t, _)| now.duration_since(*t).as_millis() >= 200)
        {
            self.samples.push_back((now, done));
        }
        while self.samples.len() > 2
            && self
                .samples
                .front()
                .is_some_and(|(t, _)| now.duration_since(*t).as_secs_f64() > 2.0)
        {
            self.samples.pop_front();
        }
        self.speed()
    }

    fn speed(&self) -> u64 {
        match (self.samples.front(), self.samples.back()) {
            (Some((t0, d0)), Some((t1, d1))) if t1 > t0 => {
                let secs = t1.duration_since(*t0).as_secs_f64();
                if secs < 0.4 {
                    return 0;
                }
                (d1.saturating_sub(*d0) as f64 / secs) as u64
            }
            _ => 0,
        }
    }
}

pub(crate) struct TransferInner {
    pub session: SessionId,
    pub owner: String,
    pub local: PathBuf,
    pub remote: String,
    pub total: u64,
    pub files: u32,
    pub files_done: u32,
    pub phase: Phase,
    pub error: Option<SftpError>,
    pub finished: Option<i64>,
    pub skipped: u32,
    pub skipped_special: u32,
    pub skipped_paths: Vec<String>,
    pub resumed: u32,
    pub batch: BatchId,
    /// 当前这次运行的取消
    pub cancel: Option<Cancel>,
    /// 上传时用户是否同意覆盖远程的同名项（继续时沿用）；命令行工具的下载为真：按 scp 的语义直接覆盖本机的目标文件
    pub overwrite: bool,
    /// 下载时选定并占用的本机位置，继续时写到这里
    pub target: Option<PathBuf>,
    /// 中断时正在传输的文件（失败后保留了临时文件）
    pub part: Option<Partial>,
    /// 已安排自动清除
    clearing: bool,
    /// 上一次运行因远程已存在同名项（没有同意覆盖）而失败，还没写入任何内容：继续前重新检查
    pub conflict: bool,
    /// 已从列表中移除（清除、超出保留条数、自动清除），不能再继续
    removed: bool,
    /// 所属窗口已关闭时还没结束：结束后随即删除记录与临时文件
    orphan: bool,
    rate: Rate,
}

/// 测试中代替实际传输的工作。
#[cfg(test)]
pub(crate) type TestWork =
    Box<dyn FnOnce(Cancel) -> futures::future::BoxFuture<'static, Result<(), SftpError>> + Send>;

/// 一个传输任务：上传或下载一个文件或目录。
pub(crate) struct Transfer {
    pub id: TransferId,
    pub direction: Direction,
    pub host: String,
    pub name: String,
    pub is_dir: bool,
    created: i64,
    /// 已传输的字节数
    pub done: AtomicU64,
    state: Mutex<TransferInner>,
    #[cfg(test)]
    pub test_work: Mutex<Option<TestWork>>,
}

impl Transfer {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: Option<TransferId>,
        direction: Direction,
        session: SessionId,
        host: String,
        owner: String,
        name: String,
        local: PathBuf,
        remote: String,
        is_dir: bool,
        overwrite: bool,
        batch: BatchId,
    ) -> Self {
        Self {
            id: id.unwrap_or_else(TransferId::new),
            direction,
            host,
            name,
            is_dir,
            created: crate::time::now_ms(),
            done: AtomicU64::new(0),
            state: Mutex::new(TransferInner {
                session,
                owner,
                local,
                remote,
                total: 0,
                files: 0,
                files_done: 0,
                phase: Phase::Queued,
                error: None,
                finished: None,
                skipped: 0,
                skipped_special: 0,
                skipped_paths: Vec::new(),
                resumed: 0,
                batch,
                cancel: None,
                overwrite,
                target: None,
                part: None,
                clearing: false,
                conflict: false,
                removed: false,
                orphan: false,
                rate: Rate::default(),
            }),
            #[cfg(test)]
            test_work: Mutex::new(None),
        }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, TransferInner> {
        self.state.lock().unwrap()
    }

    fn view_locked(&self, s: &TransferInner, reconnecting: bool, speed: u64) -> TransferView {
        let state = match s.phase {
            Phase::Queued => TransferState::Queued,
            Phase::Running => TransferState::Running,
            Phase::Done => TransferState::Done,
            Phase::Canceled => TransferState::Canceled,
            Phase::Failed
                if reconnecting && s.error.as_ref().is_some_and(SftpError::is_disconnected) =>
            {
                TransferState::WaitingReconnect
            }
            Phase::Failed => TransferState::Failed,
        };
        TransferView {
            id: self.id.clone(),
            session: s.session.clone(),
            host: self.host.clone(),
            owner: s.owner.clone(),
            batch: s.batch,
            direction: self.direction,
            name: self.name.clone(),
            local: s.local.clone(),
            remote: s.remote.clone(),
            is_dir: self.is_dir,
            total: s.total,
            done: self.done.load(Ordering::Relaxed),
            files: s.files,
            files_done: s.files_done,
            speed: if s.phase == Phase::Running { speed } else { 0 },
            state,
            error: s.error.clone(),
            created: self.created,
            finished: s.finished,
            resumable: s.phase == Phase::Failed && s.part.is_some(),
            skipped: s.skipped,
            skipped_special: s.skipped_special,
            skipped_paths: s.skipped_paths.clone(),
            resumed: s.resumed,
        }
    }

    pub(crate) fn set_totals(&self, total: u64, files: u32) {
        let mut s = self.lock();
        s.total = total;
        s.files = files;
    }

    pub(crate) fn file_done(&self) {
        self.lock().files_done += 1;
    }

    /// 记录目录传输中因为没有读权限而跳过的一项。`size` 是跳过的文件在扫描时计入总量的字节数，从总量中减去；
    /// 目录为 `None`（扫描时就没能进入，没有计入）。
    pub(crate) fn skip(&self, path: String, size: Option<u64>) {
        let mut s = self.lock();
        s.skipped += 1;
        if s.skipped_paths.len() < MAX_SKIPPED_PATHS {
            s.skipped_paths.push(path);
        }
        if let Some(size) = size {
            s.total = s.total.saturating_sub(size);
            s.files = s.files.saturating_sub(1);
        }
    }

    /// 记录目录传输中因为文件类型不支持而跳过的一项（扫描时没有计入总量）。
    pub(crate) fn skip_special(&self, path: String) {
        let mut s = self.lock();
        s.skipped += 1;
        s.skipped_special += 1;
        if s.skipped_paths.len() < MAX_SKIPPED_PATHS {
            s.skipped_paths.push(path);
        }
    }

    pub(crate) fn set_part(&self, part: Option<Partial>) {
        self.lock().part = part;
    }

    /// 记下临时文件中确认已写入的长度。
    pub(crate) fn set_safe(&self, safe: u64) {
        if let Some(part) = self.lock().part.as_mut() {
            part.safe = safe;
        }
    }

    /// 上次记下的 `local` 到 `remote` 这个文件的远程临时文件（不是这个文件时为空）。
    pub(crate) fn remote_part_of(&self, local: &Path, remote: &str) -> Option<String> {
        let s = self.lock();
        let p = s.part.as_ref()?;
        (p.local == local && p.remote == remote)
            .then(|| p.remote_part().map(str::to_string))
            .flatten()
    }

    /// 可以接着传的位置：记录的是同一个文件、来源没有变化、临时文件不比来源长时，为临时文件的长度与确认已写入的长度中较小的。
    pub(crate) fn resume_offset(
        &self,
        local: &Path,
        remote: &str,
        size: u64,
        mtime: i64,
        part_size: u64,
    ) -> u64 {
        let s = self.lock();
        match &s.part {
            Some(p)
                if p.local == local
                    && p.remote == remote
                    && p.size == size
                    && p.mtime == mtime
                    && part_size <= size =>
            {
                part_size.min(p.safe)
            }
            _ => 0,
        }
    }

    /// 下载时本机的临时文件是不是本任务的，只有本任务的才能截断或接着写：目录下载时在本任务新建的目录中；
    /// 单个文件时是本任务创建的那一个（失败后它可能被删除，同名的是别的任务新建的）。会读文件系统。
    pub(crate) fn owns_part(&self, local: &Path) -> bool {
        let (overwrite, target, part) = {
            let s = self.lock();
            (s.overwrite, s.target.clone(), s.part.clone())
        };
        if overwrite {
            return true;
        }
        let Some(target) = target else {
            return false;
        };
        if self.is_dir {
            return local != target && local.starts_with(&target);
        }
        match part {
            Some(p) if p.local == local => match p.file {
                Some(id) => os::same_file(&local_part(local), Some(id)),
                None => local_part(local).exists(),
            },
            _ => false,
        }
    }

    /// 删除本任务还没写入数据的临时文件（例如远程文件没有读权限，打开时就失败）：留着它没有用，反而显示为可继续、
    /// 让之后下载同名文件时都改名。下载单个文件时一并释放占用的名称，继续时重新选定。会读写文件系统。
    pub(crate) fn drop_empty_part(&self, local: &Path) {
        let part = self.lock().part.clone();
        let Some(p) = part else {
            return;
        };
        let PartPath::Local(part_path) = &p.part else {
            return;
        };
        if p.local != local || p.file.is_none() || !os::same_file(part_path, p.file) {
            return;
        }
        let empty = std::fs::metadata(part_path).is_ok_and(|m| m.len() == 0);
        if !empty || std::fs::remove_file(part_path).is_err() {
            return;
        }
        let mut s = self.lock();
        s.part = None;
        if !self.is_dir && s.target.as_deref() == Some(local) {
            s.target = None;
            s.local = local.parent().unwrap_or(local).to_path_buf();
        }
    }

    /// 已结束、且符合 `pred` 时标记为已移除，返回是否标记了。与 [`Self::requeue`] 在同一把锁里检查：
    /// "移除"与"继续"同时发生时只有一个生效。
    fn mark_removed(&self, pred: impl FnOnce(&TransferInner) -> bool) -> bool {
        let mut s = self.lock();
        if !s.phase.finished() || s.removed || !pred(&s) {
            return false;
        }
        s.removed = true;
        true
    }

    /// 把失败的任务重新置为排队，换到会话 `session` 上。不是失败状态（例如已被继续）或记录已被移除时报错。
    pub(crate) fn requeue(&self, session: &SftpSession, overwrite: bool) -> Result<(), SftpError> {
        let mut s = self.lock();
        if s.removed {
            return Err(SftpError::TransferNotFound);
        }
        if s.phase != Phase::Failed {
            return Err(Rejection::NotResumable.into());
        }
        if overwrite && self.direction == Direction::Upload {
            s.overwrite = true;
        }
        s.phase = Phase::Queued;
        s.error = None;
        s.finished = None;
        s.files_done = 0;
        s.skipped = 0;
        s.skipped_special = 0;
        s.skipped_paths.clear();
        s.session = session.id.clone();
        s.owner = session.owner();
        s.resumed += 1;
        // 上一次运行的取消已经没用了：新的运行开始前取消时直接置为已取消
        s.cancel = None;
        Ok(())
    }
}

/// 在后台线程上执行同步的文件操作。
pub(crate) async fn blocking<T, F>(f: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(value) => value,
        Err(e) => std::panic::resume_unwind(e.into_panic()),
    }
}

impl SftpManager {
    /// 为每个本机路径新建一个上传任务（排队后开始），返回它们的快照。
    pub async fn upload(
        &self,
        session: &SftpSession,
        request: UploadRequest,
    ) -> Result<Vec<TransferView>, SftpError> {
        let inner = &self.0;
        let session = inner.live_session(&session.id)?;
        if request.local_paths.is_empty() {
            return Err(Rejection::NoPaths.into());
        }
        let remote_dir = session.resolve(&request.remote_dir);
        let mut items = Vec::new();
        for local in &request.local_paths {
            if !local.is_absolute() {
                return Err(Rejection::NotAbsolute.into());
            }
            let meta = tokio::fs::metadata(local)
                .await
                .map_err(|e| error::local(local, &e))?;
            let name = local
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "_".into());
            items.push((local.clone(), name, meta.is_dir()));
        }
        let mut views = Vec::new();
        for (local, name, is_dir) in items {
            let remote = path::join(&remote_dir, &name);
            let t = inner
                .add_transfer(
                    &session,
                    Direction::Upload,
                    name,
                    local,
                    remote,
                    is_dir,
                    request.overwrite,
                )
                .await;
            inner.spawn_run(&session, &t, false);
            views.push(inner.view_of(&t));
        }
        Ok(views)
    }

    /// 为每个远程路径新建一个下载任务（排队后开始），返回它们的快照。
    pub async fn download(
        &self,
        session: &SftpSession,
        request: DownloadRequest,
    ) -> Result<Vec<TransferView>, SftpError> {
        let inner = &self.0;
        let session = inner.live_session(&session.id)?;
        if request.remote_paths.is_empty() {
            return Err(Rejection::NoPaths.into());
        }
        if !request.local_dir.is_absolute() {
            return Err(Rejection::NotAbsolute.into());
        }
        match tokio::fs::metadata(&request.local_dir).await {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(Rejection::NotDirectory.into()),
            Err(e) => return Err(error::local(&request.local_dir, &e)),
        }
        let client = session.backend();
        let mut items = Vec::new();
        for remote in &request.remote_paths {
            let remote = session.resolve(remote);
            let attrs = client
                .stat(&remote)
                .await
                .map_err(|e| error::remote(&remote, &e))?;
            items.push((path::base_name(&remote), remote, is_dir(&attrs)));
        }
        let mut views = Vec::new();
        for (name, remote, is_dir) in items {
            let t = inner
                .add_transfer(
                    &session,
                    Direction::Download,
                    name,
                    request.local_dir.clone(),
                    remote,
                    is_dir,
                    false,
                )
                .await;
            inner.spawn_run(&session, &t, false);
            views.push(inner.view_of(&t));
        }
        Ok(views)
    }

    /// 继续失败的传输：中断的文件从临时文件接着传，目录中已经传完（大小与修改时间相同）的文件跳过。
    /// 上传因远程已存在而失败的，没有同意覆盖时重新检查。
    pub fn resume(
        &self,
        id: &TransferId,
        options: ResumeOptions,
    ) -> Result<TransferView, SftpError> {
        let inner = &self.0;
        let t = inner.find(id).ok_or(SftpError::TransferNotFound)?;
        inner.resume_transfer(&t, options.session.as_ref(), options.overwrite)
    }

    /// 取消排队中、进行中或等待重连的传输（删除它的临时文件）；已结束的不受影响。
    pub fn cancel(&self, id: &TransferId) -> Result<(), SftpError> {
        let inner = &self.0;
        let t = inner.find(id).ok_or(SftpError::TransferNotFound)?;
        let session = t.lock().session.clone();
        let reconnecting = inner.reconnecting(&session);
        let mut drop_part = false;
        let cancel = {
            let mut s = t.lock();
            match s.phase {
                Phase::Queued if s.cancel.is_none() => {
                    s.phase = Phase::Canceled;
                    s.finished = Some(crate::time::now_ms());
                    None
                }
                Phase::Queued | Phase::Running => s.cancel.clone(),
                Phase::Failed
                    if reconnecting && s.error.as_ref().is_some_and(SftpError::is_disconnected) =>
                {
                    s.phase = Phase::Canceled;
                    s.error = None;
                    s.finished = Some(crate::time::now_ms());
                    drop_part = true;
                    None
                }
                _ => None,
            }
        };
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
        if drop_part {
            let inner = inner.clone();
            crate::runtime::spawn(async move { inner.remove_part_via(&t, None).await });
        }
        self.0.mark_dirty();
        Ok(())
    }

    /// 传输任务的快照（按创建顺序）；`owner` 非空时只返回该窗口的。
    pub fn transfers(&self, owner: Option<&str>) -> Vec<TransferView> {
        let inner = &self.0;
        if owner.is_some() && inner.auto_clear() {
            let list = inner.transfers.lock().unwrap().clone();
            for t in &list {
                let eligible = {
                    let s = t.lock();
                    owner.is_some_and(|o| s.owner == o)
                        && matches!(s.phase, Phase::Done | Phase::Canceled)
                        && s.skipped == 0
                };
                if eligible {
                    inner.schedule_clear(t);
                }
            }
        }
        inner.views(owner, false)
    }

    /// 删除符合条件的已结束记录，返回删除的条数。被删除的不能再继续：失败时留下的临时文件一并删除
    /// （先删本机的，再删远程的；远程的只在同一台主机还有会话时能删）。
    pub async fn clear(&self, filter: ClearFilter) -> usize {
        let inner = &self.0;
        inner.clear_matching(|view| filter.matches(view)).await
    }
}

impl Inner {
    pub(crate) fn live_session(&self, id: &SessionId) -> Result<Arc<SftpSession>, SftpError> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .filter(|s| !s.closing.is_canceled())
            .cloned()
            .ok_or(SftpError::SessionClosed)
    }

    pub(crate) fn find(&self, id: &TransferId) -> Option<Arc<Transfer>> {
        self.transfers
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.id == *id)
            .cloned()
    }

    fn reconnecting_ids(&self) -> HashSet<SessionId> {
        self.sessions
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.is_reconnecting())
            .map(|s| s.id.clone())
            .collect()
    }

    pub(crate) fn view_of(&self, t: &Transfer) -> TransferView {
        let session = t.lock().session.clone();
        let reconnecting = self.reconnecting(&session);
        let s = t.lock();
        let speed = s.rate.speed();
        t.view_locked(&s, reconnecting, speed)
    }

    /// 全部（或某个窗口的）任务的快照；`sample` 时为进行中的任务采样速度（发布任务每 250 毫秒一次）。
    pub(crate) fn views(&self, owner: Option<&str>, sample: bool) -> Vec<TransferView> {
        let reconnecting = self.reconnecting_ids();
        let list = self.transfers.lock().unwrap().clone();
        let now = Instant::now();
        list.iter()
            .filter_map(|t| {
                let mut s = t.lock();
                if owner.is_some_and(|o| s.owner != o) {
                    return None;
                }
                let speed = if sample && s.phase == Phase::Running {
                    let done = t.done.load(Ordering::Relaxed);
                    s.rate.sample(now, done)
                } else {
                    if sample {
                        s.rate = Rate::default();
                    }
                    s.rate.speed()
                };
                Some(t.view_locked(&s, reconnecting.contains(&s.session), speed))
            })
            .collect()
    }

    /// 登记任务；每个窗口只保留最近的若干条已结束记录，被挤出的删除临时文件。
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn add_transfer(
        &self,
        session: &SftpSession,
        direction: Direction,
        name: String,
        local: PathBuf,
        remote: String,
        is_dir: bool,
        overwrite: bool,
    ) -> Arc<Transfer> {
        let id = TransferId::new();
        let owner = session.owner();
        let batch =
            self.batches
                .lock()
                .unwrap()
                .join(&owner, &session.host, &id, crate::time::now_ms());
        let t = Arc::new(Transfer::new(
            Some(id),
            direction,
            session.id.clone(),
            session.host.clone(),
            owner.clone(),
            name,
            local,
            remote,
            is_dir,
            overwrite,
            batch,
        ));
        let dropped = {
            let mut list = self.transfers.lock().unwrap();
            list.push(t.clone());
            prune_locked(&mut list, &owner, self.options.keep_finished)
        };
        self.forget(&dropped);
        self.remove_parts(&dropped).await;
        self.mark_dirty();
        t
    }

    pub(crate) fn spawn_run(
        self: &Arc<Self>,
        session: &Arc<SftpSession>,
        t: &Arc<Transfer>,
        resume: bool,
    ) {
        tracing::info!(
            id = %t.id,
            session = %session.id,
            host = %t.host,
            direction = ?t.direction,
            name = %t.name,
            is_dir = t.is_dir,
            resume,
            "传输开始"
        );
        crate::runtime::spawn(run(self.clone(), session.clone(), t.clone(), resume));
    }

    fn resume_transfer(
        self: &Arc<Self>,
        t: &Arc<Transfer>,
        session: Option<&SessionId>,
        overwrite: bool,
    ) -> Result<TransferView, SftpError> {
        let (phase, current) = {
            let s = t.lock();
            (s.phase, s.session.clone())
        };
        if phase != Phase::Failed {
            return Err(Rejection::NotResumable.into());
        }
        let session = self.live_session(session.unwrap_or(&current))?;
        if session.host != t.host {
            return Err(Rejection::OtherHost.into());
        }
        // 检查与置为排队在同一把锁里完成：断线重连后的自动继续与用户点"继续"同时发生时只启动一次
        t.requeue(&session, overwrite)?;
        let batch = self.batches.lock().unwrap().join(
            &session.owner(),
            &session.host,
            &t.id,
            crate::time::now_ms(),
        );
        t.lock().batch = batch;
        t.done.store(0, Ordering::Relaxed);
        self.spawn_run(&session, t, true);
        self.mark_dirty();
        Ok(self.view_of(t))
    }

    /// 重连后继续这个会话上因断线失败的传输（从中断处接着传）。
    pub(crate) fn resume_after_reconnect(self: &Arc<Self>, session: &SftpSession) {
        let list = self.transfers.lock().unwrap().clone();
        for t in &list {
            let lost = {
                let s = t.lock();
                s.session == session.id
                    && s.phase == Phase::Failed
                    && s.error.as_ref().is_some_and(SftpError::is_disconnected)
            };
            if lost && let Err(e) = self.resume_transfer(t, Some(&session.id), false) {
                tracing::warn!(id = %t.id, "重连后继续传输失败：{e}");
            }
        }
    }

    /// 会话上是否还有排队中或进行中的传输。
    pub(crate) fn pending_on(&self, id: &SessionId) -> bool {
        self.transfers.lock().unwrap().iter().any(|t| {
            let s = t.lock();
            s.session == *id && matches!(s.phase, Phase::Queued | Phase::Running)
        })
    }

    fn finish(self: &Arc<Self>, t: &Arc<Transfer>, result: Result<(), SftpError>) {
        let (phase, skipped) = {
            let mut s = t.lock();
            s.finished = Some(crate::time::now_ms());
            let bytes = t.done.load(Ordering::Relaxed);
            match result {
                Ok(()) => {
                    tracing::info!(
                        id = %t.id,
                        direction = ?t.direction,
                        name = %t.name,
                        bytes,
                        files = s.files_done,
                        skipped = s.skipped,
                        "传输完成"
                    );
                    s.phase = Phase::Done
                }
                Err(SftpError::Canceled) => {
                    tracing::info!(id = %t.id, name = %t.name, bytes, "传输已取消");
                    s.phase = Phase::Canceled
                }
                Err(e) => {
                    tracing::warn!(id = %t.id, direction = ?t.direction, name = %t.name, "传输失败：{e}");
                    s.phase = Phase::Failed;
                    s.error = Some(e);
                }
            }
            (s.phase, s.skipped)
        };
        // 有跳过项的不自动清除：留在列表中，用户能看到哪些没有传
        if matches!(phase, Phase::Done | Phase::Canceled) && skipped == 0 && self.auto_clear() {
            self.schedule_clear(t);
        }
        self.mark_dirty();
    }

    /// 安排在一段时间后移除成功或已取消的记录（已安排过的不重复安排）。到时再读一次开关：期间关闭了自动清除时保留。
    fn schedule_clear(self: &Arc<Self>, t: &Arc<Transfer>) {
        {
            let mut s = t.lock();
            if s.clearing {
                return;
            }
            s.clearing = true;
        }
        let inner = self.clone();
        let t = t.clone();
        crate::runtime::spawn(async move {
            tokio::time::sleep(inner.options.auto_clear_delay).await;
            if !inner.auto_clear() {
                t.lock().clearing = false;
                return;
            }
            let removed = {
                let mut list = inner.transfers.lock().unwrap();
                match list.iter().position(|x| Arc::ptr_eq(x, &t)) {
                    Some(i) if t.mark_removed(|_| true) => {
                        list.remove(i);
                        true
                    }
                    _ => false,
                }
            };
            if removed {
                // 成功的与已取消的通常没有临时文件；失败后继续、开始前又被取消的任务还留着上次的，一并删除
                inner.forget(std::slice::from_ref(&t));
                inner.remove_parts(&[t]).await;
                inner.mark_dirty();
            }
        });
    }

    /// 记下要移除的任务的最后状态（所在的批结束时汇总要用）。
    fn forget(&self, list: &[Arc<Transfer>]) {
        if list.is_empty() {
            return;
        }
        let views: Vec<TransferView> = list.iter().map(|t| self.view_of(t)).collect();
        let mut book = self.batches.lock().unwrap();
        for view in &views {
            book.remember(view);
        }
    }

    /// 删除符合条件的已结束记录与它们的临时文件，返回删除的条数。
    pub(crate) async fn clear_matching(&self, pred: impl Fn(&TransferView) -> bool) -> usize {
        let reconnecting = self.reconnecting_ids();
        let dropped = {
            let mut list = self.transfers.lock().unwrap();
            let mut dropped = Vec::new();
            list.retain(|t| {
                let hit = t.mark_removed(|s| {
                    let view = t.view_locked(s, reconnecting.contains(&s.session), 0);
                    pred(&view)
                });
                if hit {
                    dropped.push(t.clone());
                }
                !hit
            });
            dropped
        };
        self.forget(&dropped);
        self.remove_parts(&dropped).await;
        self.mark_dirty();
        dropped.len()
    }

    /// 删除窗口（为空时是全部窗口）全部已结束的记录（含等待重连的）；连接没有响应时最多等 `cleanup_wait`，删除留在后台继续。
    pub(crate) async fn clear_within(self: &Arc<Self>, owner: Option<&str>) {
        let inner = self.clone();
        let owner = owner.map(str::to_string);
        let task = crate::runtime::spawn(async move {
            inner
                .clear_matching(|v| {
                    owner.as_ref().is_none_or(|o| *o == v.owner)
                        && v.state != TransferState::Queued
                        && v.state != TransferState::Running
                })
                .await
        });
        if tokio::time::timeout(self.options.cleanup_wait, task)
            .await
            .is_err()
        {
            tracing::warn!("删除传输的临时文件超时，不再等待");
        }
    }

    /// 把窗口（为空时是全部窗口）还没结束的任务标记为孤儿：结束后随即删除记录与临时文件。
    pub(crate) fn mark_orphans(&self, owner: Option<&str>) {
        for t in self.transfers.lock().unwrap().iter() {
            let mut s = t.lock();
            if !s.phase.finished() && owner.is_none_or(|o| s.owner == o) {
                s.orphan = true;
            }
        }
    }

    /// 会话从窗口 `from` 换到 `to`（[`super::SftpManager::reassign_owner`]）：它的传输记录随之换过去；还没结束的
    /// （含等待重连的）从原来窗口的那一批移进新窗口的一批，已经结束的留在原来那一批的汇总里。
    pub(crate) fn reassign_transfers(&self, session: &SessionId, from: &str, to: &str) {
        let list = self.transfers.lock().unwrap().clone();
        let mut moved = Vec::new();
        for t in &list {
            let mut s = t.lock();
            if s.session == *session && s.owner == from {
                s.owner = to.to_string();
                moved.push(t.clone());
            }
        }
        let pending: Vec<Arc<Transfer>> = moved
            .into_iter()
            .filter(|t| self.view_of(t).state.is_pending())
            .collect();
        if let Some(first) = pending.first() {
            let ids: Vec<TransferId> = pending.iter().map(|t| t.id.clone()).collect();
            let batches = self.batches.lock().unwrap().reassign(
                &ids,
                from,
                to,
                &first.host,
                crate::time::now_ms(),
            );
            for (t, batch) in pending.iter().zip(batches) {
                t.lock().batch = batch;
            }
        }
        self.mark_dirty();
    }

    async fn drop_orphan(&self, t: &Transfer) {
        if t.lock().orphan {
            let id = t.id.clone();
            self.clear_matching(|v| v.id == id).await;
        }
    }

    /// 删除这些任务失败时留下的临时文件：先删本机的（下载），再删远程的（上传）。删除远程的在连接卡住时会一直等，
    /// 不应耽误本机的清理。
    async fn remove_parts(&self, list: &[Arc<Transfer>]) {
        for download in [true, false] {
            for t in list {
                if (t.direction == Direction::Download) == download {
                    self.remove_part_via(t, None).await;
                }
            }
        }
    }

    /// 删除任务留下的临时文件；远程的经会话 `via` 删除（为空时用任务所在的会话，它已关闭时用同一台主机的其他会话）。
    pub(crate) async fn remove_part_via(&self, t: &Transfer, via: Option<&Arc<SftpSession>>) {
        let (part, session_id) = {
            let mut s = t.lock();
            (s.part.take(), s.session.clone())
        };
        let Some(part) = part else {
            return;
        };
        match part.part {
            PartPath::Local(p) => {
                // 只删除本任务的临时文件：失败后它可能被删除，同名的是别的任务新建的
                let id = part.file;
                blocking(move || {
                    if os::same_file(&p, id) {
                        let _ = std::fs::remove_file(&p);
                    }
                })
                .await;
            }
            PartPath::Remote(p) => {
                let session = via.cloned().or_else(|| {
                    let sessions = self.sessions.lock().unwrap();
                    sessions
                        .get(&session_id)
                        .cloned()
                        .or_else(|| sessions.values().find(|s| s.host == t.host).cloned())
                });
                // 本任务放开它之后、删除之前，新的上传可能已选中这个名称：这时不删
                let busy = part_held(&self.transfers.lock().unwrap(), &t.host, &p, t);
                if let Some(session) = session
                    && !busy
                {
                    let _ = session.backend().remove(&p).await;
                }
            }
        }
    }
}

/// 每个窗口只保留最近的 `keep` 条已结束记录，返回删除的记录。
fn prune_locked(list: &mut Vec<Arc<Transfer>>, owner: &str, keep: usize) -> Vec<Arc<Transfer>> {
    let mut dropped = Vec::new();
    let mut finished = 0;
    let mut i = list.len();
    while i > 0 {
        i -= 1;
        let t = &list[i];
        let mine = {
            let s = t.lock();
            s.owner == owner && s.phase.finished()
        };
        if !mine {
            continue;
        }
        finished += 1;
        if finished > keep && t.mark_removed(|_| true) {
            dropped.push(list.remove(i));
        }
    }
    dropped
}

/// 主机上的远程临时文件 `part` 是否被 `except` 以外的上传任务占用（排队、进行中，或失败后留着它供继续）。
fn part_held(list: &[Arc<Transfer>], host: &str, part: &str, except: &Transfer) -> bool {
    list.iter().any(|t| {
        if std::ptr::eq(Arc::as_ptr(t), except)
            || t.direction != Direction::Upload
            || t.host != host
        {
            return false;
        }
        t.lock()
            .part
            .as_ref()
            .and_then(Partial::remote_part)
            .is_some_and(|p| p == part)
    })
}

/// 为任务选定上传时用的远程临时文件，记在 `p.part` 并占用，与检查在同一把锁里完成。
///
/// 通常是"名称.termlet-part"；同一台主机上别的任务占用了它时改用"名称.1.termlet-part"等：两个窗口同时上传同名文件到
/// 同一个目录时不共用一个临时文件。已存在但没有任务占用的同名临时文件是以前留下的（例如程序异常退出），删除后重新创建（见 `upload::create_part`）。
/// 没有管理器时（命令行工具的单文件上传）用带随机部分的一次性名称：不查占用表，固定的名称会截断桌面端正在写的临时文件。
pub(crate) fn claim_remote_part(inner: Option<&Inner>, t: &Transfer, p: &mut Partial) {
    let Some(inner) = inner else {
        p.part = PartPath::Remote(remote_temp_part(&p.remote));
        t.set_part(Some(p.clone()));
        return;
    };
    let list = inner.transfers.lock().unwrap();
    let mut n = 0;
    let part = loop {
        let candidate = remote_part(&p.remote, n);
        if !part_held(&list, &t.host, &candidate, t) {
            break candidate;
        }
        n += 1;
    };
    p.part = PartPath::Remote(part);
    t.set_part(Some(p.clone()));
}

/// 执行传输（测试中可以换成别的工作）。
async fn perform(
    inner: &Arc<Inner>,
    session: &SftpSession,
    t: &Arc<Transfer>,
    ctx: &Cancel,
    resume: bool,
) -> Result<(), SftpError> {
    #[cfg(test)]
    {
        let work = t.test_work.lock().unwrap().take();
        if let Some(work) = work {
            return work(ctx.clone()).await;
        }
    }
    match t.direction {
        Direction::Upload => upload::run(inner, session, t, ctx, resume).await,
        Direction::Download => download::run(session, t, ctx, resume).await,
    }
}

/// 排队等待空位后执行传输，并记录结果。
async fn run(inner: Arc<Inner>, session: Arc<SftpSession>, t: Arc<Transfer>, resume: bool) {
    let ctx = session.closing.child();
    let canceled = {
        let mut s = t.lock();
        s.cancel = Some(ctx.clone());
        s.phase == Phase::Canceled
    };
    if canceled {
        // 开始前就被取消（继续后排队时）：上次失败留下的临时文件用不着了，与传输中取消一样删除
        inner.remove_part_via(&t, Some(&session)).await;
        inner.drop_orphan(&t).await;
        inner.mark_dirty();
        return;
    }
    let epoch = session.lost_epoch();
    let permit = tokio::select! {
        permit = session.slots.clone().acquire_owned() => permit.ok(),
        _ = ctx.cancelled() => None,
    };
    let Some(permit) = permit else {
        // 排队时被取消，或会话关闭：在置为已取消之前删除临时文件（关闭会话时等这里处理完再关闭连接）
        inner.remove_part_via(&t, Some(&session)).await;
        inner.finish(&t, Err(SftpError::Canceled));
        inner.drop_orphan(&t).await;
        return;
    };
    t.lock().phase = Phase::Running;
    inner.mark_dirty();
    let result = perform(&inner, &session, &t, &ctx, resume).await;
    let result = match result {
        Err(_) if ctx.is_canceled() => Err(SftpError::Canceled),
        other => other,
    };
    if result.as_ref().is_err_and(SftpError::is_disconnected) {
        // 因断线失败：等会话标记为正在重连再记下失败，读到的失败记录一定是"等待重连"
        session.await_lost(epoch, inner.options.lost_wait).await;
    }
    inner.finish(&t, result);
    drop(permit);
    inner.drop_orphan(&t).await;
}
