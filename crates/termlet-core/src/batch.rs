//! 批量执行：不打开终端，在一批主机上新建连接执行同一条命令（或传同一个文件），并发有上限，逐台给出结果。
//!
//! 命令行服务（`termlet exec -g …`、MCP 的 `run_command_on_hosts`）与界面上的批量执行共用这里：
//! - [`resolve`]：按 [`Selector`] 展开主机（主机的 ID 或名称、分组含子分组、标签、全部，再减去排除的），
//!   不能执行的（未允许 AI 工具与脚本使用、不是 SSH 主机）列为跳过；
//! - [`run`]：逐台连接、执行或传输，并发最多 [`Options::parallel`] 台，达到 [`Options::max_fail`] 台失败后不再开始新的，
//!   每台的命令超过 [`Options::timeout`] 时发送 SIGTERM；开始与结束经 [`Event`] 送出，最后返回按主机顺序的结果；
//! - 各台的状态（[`BatchStatus`]）在这里判定：连接或登录失败为 `unreachable`，连上之后出错与退出码不是 0 为 `failed`，
//!   超时为 `timeout`，执行中被取消为 `canceled`，没有开始为 `skipped`。
//!
//! 危险命令的检查与批准、审计日志由调用方做（命令行服务在 `cli::handler` 中）：这里只负责执行。
//! 用谁的凭据（[`Access`]）：命令行与 MCP 用主机的 CLI 专用凭据、只能用允许了的主机；界面上用户自己发起的，与打开终端时
//! 相同（不要求“允许 AI 工具与脚本使用”）。两者都不能询问（指纹、密码、验证码）：要询问的主机为 `unreachable`。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use termlet_proto::batch::MAX_BATCH_HOSTS;
use termlet_proto::{BatchHostResult, BatchStatus, ErrorBody, ErrorCode, Lang, Selector};
use tokio::sync::{Semaphore, mpsc, watch};

use crate::cli::connect::{self, Credentials};
use crate::cli::error::{CliError, from_sftp, tr};
use crate::cli::service::Cancel;
use crate::model::{Group, Host};
use crate::sftp;
use crate::store::{Record, Store};

/// 谁在执行，决定能用哪些主机、用谁的凭据。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// 命令行与 MCP：只能用勾选了“允许 AI 工具与脚本使用这台主机”的 SSH 主机，用它的 CLI 专用凭据（有的话）
    Cli,
    /// 用户在界面上发起：SSH 主机都可以，凭据与打开终端时相同
    User,
}

impl Access {
    fn credentials(self) -> Credentials {
        match self {
            Access::Cli => Credentials::Cli,
            Access::User => Credentials::User,
        }
    }
}

/// [`resolve`] 的结果：按名称排序。
#[derive(Clone, Debug, Default)]
pub struct Resolved {
    pub run: Vec<Record<Host>>,
    /// 跳过的主机与原因（`host_not_allowed`）
    pub skipped: Vec<(Record<Host>, ErrorBody)>,
}

/// 选不出主机的原因（整批不执行）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectError {
    pub code: ErrorCode,
    pub message: String,
}

impl SelectError {
    fn new(code: ErrorCode, message: String) -> Self {
        Self { code, message }
    }
}

/// 按选择条件展开主机。名称相同的主机都选上（批量的语义是“叫这个名字的”）。找不到的主机名、分组，或者展开后一台
/// 也没有时为错误；选中的都不能执行时同样为错误（`host_not_allowed`，命令行据此返回退出码 3）。
pub fn resolve(
    store: &Store,
    selector: &Selector,
    access: Access,
    lang: Lang,
) -> Result<Resolved, SelectError> {
    if selector.is_empty() {
        return Err(SelectError::new(
            ErrorCode::BadRequest,
            tr(
                lang,
                "没有选择主机（用主机、分组、标签或“全部”选择）",
                "no hosts selected (choose hosts, groups, tags or all)",
            ),
        ));
    }
    let hosts = store.list::<Host>();
    let groups = store.list::<Group>();
    let by_key = |key: &str| -> Vec<&Record<Host>> {
        hosts
            .iter()
            .filter(|h| h.id.as_str() == key || h.doc.name == key)
            .collect()
    };
    let mut missing = Vec::new();
    let mut picked: Vec<&Record<Host>> = Vec::new();
    fn add<'a>(picked: &mut Vec<&'a Record<Host>>, host: &'a Record<Host>) {
        if !picked.iter().any(|p| p.id == host.id) {
            picked.push(host);
        }
    }
    for key in &selector.hosts {
        let found = by_key(key);
        if found.is_empty() {
            missing.push(key.clone());
        }
        for host in found {
            add(&mut picked, host);
        }
    }
    if selector.all {
        for host in &hosts {
            add(&mut picked, host);
        }
    } else if !selector.groups.is_empty() || !selector.tags.is_empty() {
        let mut inside: Option<std::collections::HashSet<String>> = None;
        if !selector.groups.is_empty() {
            let mut set = std::collections::HashSet::new();
            for path in &selector.groups {
                let found = groups_by_path(&groups, path);
                if found.is_empty() {
                    missing.push(match lang {
                        Lang::Zh => format!("分组 {path}"),
                        Lang::En => format!("group {path}"),
                    });
                }
                for id in found {
                    set.extend(with_subgroups(&groups, &id));
                }
            }
            inside = Some(set);
        }
        let tags: Vec<String> = selector.tags.iter().map(|t| t.to_lowercase()).collect();
        for host in &hosts {
            let in_group = inside
                .as_ref()
                .is_none_or(|set| host.parent.as_ref().is_some_and(|p| set.contains(p)));
            let tagged = tags.is_empty()
                || host
                    .doc
                    .tags
                    .iter()
                    .any(|t| tags.contains(&t.to_lowercase()));
            if in_group && tagged {
                add(&mut picked, host);
            }
        }
    }
    for key in &selector.exclude {
        let found = by_key(key);
        if found.is_empty() {
            missing.push(key.clone());
        }
        picked.retain(|h| !found.iter().any(|f| f.id == h.id));
    }
    if !missing.is_empty() {
        return Err(SelectError::new(
            ErrorCode::HostNotFound,
            missing.join(", "),
        ));
    }
    if picked.is_empty() {
        return Err(SelectError::new(
            ErrorCode::HostNotFound,
            tr(lang, "没有符合条件的主机", "no hosts match the selection"),
        ));
    }
    if picked.len() > MAX_BATCH_HOSTS {
        return Err(SelectError::new(
            ErrorCode::BadRequest,
            match lang {
                Lang::Zh => format!(
                    "选中了 {} 台主机，一批最多 {MAX_BATCH_HOSTS} 台",
                    picked.len()
                ),
                Lang::En => format!(
                    "{} hosts selected; a batch can have at most {MAX_BATCH_HOSTS}",
                    picked.len()
                ),
            },
        ));
    }
    picked.sort_by(|a, b| {
        a.doc
            .name
            .to_lowercase()
            .cmp(&b.doc.name.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut resolved = Resolved::default();
    for host in picked {
        match skip_reason(&host.doc, access, lang) {
            Some(reason) => resolved.skipped.push((host.clone(), reason)),
            None => resolved.run.push(host.clone()),
        }
    }
    if resolved.run.is_empty() {
        return Err(SelectError::new(
            ErrorCode::HostNotAllowed,
            resolved
                .skipped
                .iter()
                .map(|(h, _)| h.doc.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        ));
    }
    Ok(resolved)
}

/// 不能执行的原因：不是 SSH 主机；命令行与 MCP 另要“允许 AI 工具与脚本使用”。
fn skip_reason(host: &Host, access: Access, lang: Lang) -> Option<ErrorBody> {
    if !host.is_ssh() {
        return Some(ErrorBody::new(
            ErrorCode::HostNotAllowed,
            tr(
                lang,
                "不是 SSH 主机（Telnet、串口不能批量执行）",
                "not an SSH host (Telnet and serial hosts can't run batches)",
            ),
        ));
    }
    if access == Access::Cli && !host.cli.allowed {
        return Some(ErrorBody::new(ErrorCode::HostNotAllowed, String::new()));
    }
    None
}

/// 按名称或路径（`生产/web`，从最外层写起，也可以只写最后几级）找分组；同名的都算。
fn groups_by_path(groups: &[Record<Group>], path: &str) -> Vec<String> {
    let parts: Vec<&str> = path
        .split('/')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    if parts.is_empty() {
        return Vec::new();
    }
    let by_id = |id: &str| groups.iter().find(|g| g.id.as_str() == id);
    groups
        .iter()
        .filter(|g| {
            if g.id.as_str() == path {
                return true;
            }
            // 从这个分组逐级向上对照路径（最后一级在最后）
            let mut current = Some(*g);
            for part in parts.iter().rev() {
                match current {
                    Some(group) if group.doc.name == *part => {
                        current = group.parent.as_deref().and_then(by_id);
                    }
                    _ => return false,
                }
            }
            true
        })
        .map(|g| g.id.as_str().to_string())
        .collect()
}

/// 分组与它的各级子分组的 ID（存储保证没有循环；限制层数防御异常数据）。
fn with_subgroups(groups: &[Record<Group>], id: &str) -> Vec<String> {
    let mut inside: Vec<String> = vec![id.to_string()];
    for _ in 0..64 {
        let before = inside.len();
        for g in groups {
            if g.parent.as_ref().is_some_and(|p| inside.contains(p))
                && !inside.iter().any(|i| i == g.id.as_str())
            {
                inside.push(g.id.as_str().to_string());
            }
        }
        if inside.len() == before {
            break;
        }
    }
    inside
}

/// 每台做什么。
#[derive(Clone, Debug)]
pub enum Job {
    /// 执行命令；`stdin` 不为空时作为远程命令的标准输入
    Exec { command: String, stdin: Vec<u8> },
    /// 上传本机文件（绝对路径），`remote` 与单台的上传相同
    Upload { local: PathBuf, remote: String },
    /// 下载：放进 `dir`（绝对路径）下以主机名命名的子目录（没有时创建）
    Download { remote: String, dir: PathBuf },
}

/// 执行的选项。
#[derive(Clone, Debug)]
pub struct Options {
    /// 同时执行的主机数（至少 1）
    pub parallel: usize,
    /// 每台命令的时限（不含连接）
    pub timeout: Option<Duration>,
    /// 失败（`failed`、`unreachable`、`timeout`）达到这么多台后不再开始新的主机
    pub max_fail: Option<usize>,
    /// 每台标准输出、标准错误各自保留的字节数
    pub output_limit: usize,
    /// [`run`] 返回的结果中保留输出；为假时输出只随 [`Event::Finished`] 发出（流式时几百台的输出不在内存中攒到最后）
    pub keep_output: bool,
}

/// 执行中的事件（下标是主机在 [`run`] 的 `hosts` 中的位置）。
#[derive(Clone, Debug)]
pub enum Event {
    Started(usize),
    Finished(usize, Box<BatchHostResult>),
}

/// 逐台执行。`stop` 变为真时取消：执行中的发送 SIGTERM（状态 `canceled`），还没开始的不再开始（`skipped`）。
/// 返回按 `hosts` 顺序的结果（`keep_output` 为假时不含输出）；`events` 的接收方不在了也照常执行完。
pub async fn run(
    store: Arc<Store>,
    access: Access,
    hosts: Vec<Record<Host>>,
    job: Job,
    options: Options,
    lang: Lang,
    stop: watch::Receiver<bool>,
    events: mpsc::UnboundedSender<Event>,
) -> Vec<BatchHostResult> {
    let total = hosts.len();
    let mut results: Vec<Option<BatchHostResult>> = vec![None; total];
    let semaphore = Arc::new(Semaphore::new(options.parallel.max(1)));
    let failures = Arc::new(AtomicUsize::new(0));
    let folders = download_folders(&hosts);
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<(usize, BatchHostResult)>();
    let mut started = 0usize;
    let mut stop_watch = stop.clone();
    for (index, host) in hosts.iter().enumerate() {
        let permit = tokio::select! {
            permit = semaphore.clone().acquire_owned() => permit.ok(),
            _ = async { stop_watch.wait_for(|s| *s).await.is_ok() } => None,
        };
        let over_limit = options
            .max_fail
            .is_some_and(|max| failures.load(Ordering::SeqCst) >= max);
        let Some(permit) = permit.filter(|_| !*stop.borrow() && !over_limit) else {
            let message = if over_limit {
                tr(
                    lang,
                    "失败的主机达到上限（--max-fail），没有开始",
                    "not started: the failure limit (--max-fail) was reached",
                )
            } else {
                tr(lang, "开始前已取消", "canceled before it started")
            };
            let skipped = BatchHostResult {
                id: host.id.to_string(),
                name: host.doc.name.clone(),
                status: BatchStatus::Skipped,
                error: Some(ErrorBody::new(ErrorCode::Canceled, message)),
                ..Default::default()
            };
            let _ = events.send(Event::Finished(index, Box::new(skipped.clone())));
            results[index] = Some(skipped);
            continue;
        };
        started += 1;
        let _ = events.send(Event::Started(index));
        let task = HostTask {
            store: store.clone(),
            access,
            host: host.clone(),
            job: job.clone(),
            options: options.clone(),
            lang,
            stop: stop.clone(),
            folder: folders[index].clone(),
        };
        let (events, done_tx, failures) = (events.clone(), done_tx.clone(), failures.clone());
        let keep_output = options.keep_output;
        crate::runtime::spawn(async move {
            let mut result = task.run().await;
            if result.status.is_failure() {
                failures.fetch_add(1, Ordering::SeqCst);
            }
            drop(permit);
            let full = if keep_output {
                result.clone()
            } else {
                BatchHostResult {
                    stdout: std::mem::take(&mut result.stdout),
                    stderr: std::mem::take(&mut result.stderr),
                    ..result.clone()
                }
            };
            let _ = events.send(Event::Finished(index, Box::new(full)));
            let _ = done_tx.send((index, result));
        });
    }
    drop(done_tx);
    for _ in 0..started {
        match done_rx.recv().await {
            Some((index, result)) => results[index] = Some(result),
            None => break,
        }
    }
    results
        .into_iter()
        .zip(hosts.iter())
        .map(|(result, host)| {
            result.unwrap_or_else(|| BatchHostResult {
                id: host.id.to_string(),
                name: host.doc.name.clone(),
                status: BatchStatus::Canceled,
                error: Some(ErrorBody::new(ErrorCode::Canceled, String::new())),
                ..Default::default()
            })
        })
        .collect()
}

/// 下载时各台的子目录名：主机名换成能做文件名的写法，重名的加上主机 ID。
fn download_folders(hosts: &[Record<Host>]) -> Vec<String> {
    let names: Vec<String> = hosts.iter().map(|h| sftp::safe_name(&h.doc.name)).collect();
    names
        .iter()
        .zip(hosts)
        .map(|(name, host)| {
            let same = names
                .iter()
                .filter(|n| n.eq_ignore_ascii_case(name))
                .count();
            if same > 1 || name.is_empty() {
                sftp::safe_name(&format!("{name}-{}", host.id))
            } else {
                name.clone()
            }
        })
        .collect()
}

/// 一台主机。
struct HostTask {
    store: Arc<Store>,
    access: Access,
    host: Record<Host>,
    job: Job,
    options: Options,
    lang: Lang,
    stop: watch::Receiver<bool>,
    folder: String,
}

impl HostTask {
    async fn run(self) -> BatchHostResult {
        let started = Instant::now();
        let mut result = BatchHostResult {
            id: self.host.id.to_string(),
            name: self.host.doc.name.clone(),
            ..Default::default()
        };
        // 取消：整批的 stop，或者这台的命令超时（连接不算在时限内，连上之后才开始计时）
        let (gone_tx, gone_rx) = watch::channel(false);
        let (_never_tx, never) = watch::channel(false);
        let gone_tx = Arc::new(gone_tx);
        let timed_out = Arc::new(AtomicBool::new(false));
        let forward = {
            let (mut stop, gone_tx) = (self.stop.clone(), gone_tx.clone());
            AbortOnDrop(crate::runtime::spawn(async move {
                let _ = stop.wait_for(|s| *s).await;
                gone_tx.send_replace(true);
            }))
        };
        let mut cancel = Cancel::new(gone_rx, never);
        let connected = connect::connect(
            self.store.clone(),
            &self.host.id,
            self.access.credentials(),
            &mut cancel,
        )
        .await;
        let (connection, refused) = match connected {
            Ok(connected) => connected,
            Err(error) => {
                result.status = if error.code == ErrorCode::Canceled {
                    BatchStatus::Canceled
                } else {
                    BatchStatus::Unreachable
                };
                result.error = Some(error.body());
                result.duration_ms = started.elapsed().as_millis() as u64;
                drop(forward);
                return result;
            }
        };
        result.expiring_certificates = connect::expiring_certificates(&connection);
        let _deadline = self.options.timeout.map(|timeout| {
            let (gone_tx, timed_out) = (gone_tx.clone(), timed_out.clone());
            AbortOnDrop(crate::runtime::spawn(async move {
                tokio::time::sleep(timeout).await;
                timed_out.store(true, Ordering::SeqCst);
                gone_tx.send_replace(true);
            }))
        });
        let outcome: Result<(), CliError> = match &self.job {
            Job::Exec { command, stdin } => connect::exec(
                &connection,
                command,
                stdin,
                self.options.output_limit,
                &mut cancel,
            )
            .await
            .map(|output| {
                result.exit_code = Some(output.exit_code);
                result.stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                result.stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                result.truncated = output.truncated;
            }),
            Job::Upload { local, remote } => {
                let stopped = AtomicBool::new(false);
                let stop = async {
                    cancel.wait().await;
                    stopped.store(true, Ordering::Relaxed);
                };
                let done = sftp::put_file(
                    &connection,
                    local,
                    remote,
                    self.host.doc.ssh.single_channel,
                    stop,
                )
                .await;
                self.transferred(&mut result, done, &stopped, &refused, |done| {
                    (local.display().to_string(), done.path.clone())
                })
            }
            Job::Download { remote, dir } => {
                let folder = dir.join(&self.folder);
                match std::fs::create_dir_all(&folder) {
                    Err(e) => Err(CliError::new(
                        ErrorCode::PermissionDenied,
                        format!("{}: {e}", folder.display()),
                    )),
                    Ok(()) => {
                        let stopped = AtomicBool::new(false);
                        let stop = async {
                            cancel.wait().await;
                            stopped.store(true, Ordering::Relaxed);
                        };
                        let done = sftp::get_file(
                            &connection,
                            remote,
                            &folder,
                            self.host.doc.ssh.single_channel,
                            stop,
                        )
                        .await;
                        self.transferred(&mut result, done, &stopped, &refused, |done| {
                            (done.path.display().to_string(), remote.clone())
                        })
                    }
                }
            }
        };
        connect::disconnect_later(connection);
        drop(forward);
        result.duration_ms = started.elapsed().as_millis() as u64;
        result.status = match &outcome {
            Ok(()) if result.exit_code.is_none_or(|code| code == 0) => BatchStatus::Ok,
            Ok(()) => BatchStatus::Failed,
            Err(e) if e.code == ErrorCode::Canceled && timed_out.load(Ordering::SeqCst) => {
                BatchStatus::Timeout
            }
            Err(e) if e.code == ErrorCode::Canceled => BatchStatus::Canceled,
            Err(_) => BatchStatus::Failed,
        };
        if let Err(error) = outcome {
            let error = if result.status == BatchStatus::Timeout {
                CliError::new(
                    ErrorCode::Canceled,
                    tr(
                        self.lang,
                        "超过时限，已向命令发送 SIGTERM",
                        "timed out; SIGTERM was sent to the command",
                    ),
                )
            } else {
                error
            };
            result.error = Some(error.body());
        }
        result
    }

    /// 传输的结果写进 `result`；`paths` 给出（本机路径，远程路径）。
    fn transferred<T>(
        &self,
        result: &mut BatchHostResult,
        done: Result<sftp::Transferred<T>, sftp::SftpError>,
        stopped: &AtomicBool,
        refused: &std::sync::Mutex<Option<crate::cli::error::Refused>>,
        paths: impl FnOnce(&sftp::Transferred<T>) -> (String, String),
    ) -> Result<(), CliError> {
        if stopped.load(Ordering::Relaxed) {
            return Err(CliError::code(ErrorCode::Canceled));
        }
        match done {
            Ok(done) => {
                let (local, remote) = paths(&done);
                result.local = local;
                result.remote = remote;
                result.bytes = Some(done.bytes);
                result.protocol = match done.protocol {
                    sftp::Protocol::Sftp => termlet_proto::TRANSFER_SFTP,
                    sftp::Protocol::Scp => termlet_proto::TRANSFER_SCP,
                }
                .to_string();
                Ok(())
            }
            Err(e) => {
                let refused = refused.lock().unwrap_or_else(|e| e.into_inner()).clone();
                Err(from_sftp(&e, refused.as_ref(), self.lang))
            }
        }
    }
}

/// 丢弃时停下的后台任务。
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// 一批的 ID（审计日志中同一批的记录都带着它）。
pub fn new_batch_id() -> String {
    format!("b_{}", crate::id::random_token(10))
}

#[cfg(test)]
mod tests;
