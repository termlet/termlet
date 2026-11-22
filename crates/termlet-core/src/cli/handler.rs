//! 处理请求：列出主机、执行命令（新建连接或在已打开的终端中）、上传下载、启动参数。
//!
//! 每次执行、传输都写入审计日志，含被拦截、失败的调用（缺少参数的请求除外：不知道要做什么）；运行日志中不记命令内容。
//! 检查的顺序：锁定 → 外部命令行开关 → 主机是否存在、是否允许 → `--terminal` 与专用凭据 → 保险库 → 危险命令规则
//! （需要时请用户批准）→ 执行。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use termlet_proto::{
    ErrorBody, ErrorCode, ExecRequest, ExecResponse, ExpiringCertificate, HostSummary, Lang,
    OUTPUT_LIMIT, Request, Response, TransferRequest, TransferResponse,
};
use tokio::sync::{oneshot, watch};

use super::connect::{self, ExecOutput};
use super::error::{CliError, Failure, from_sftp, from_terminal, tr, vault_error};
use super::events::{ApprovalRequest, CliEvent, TerminalRequest};
use super::service::{Cancel, Shared};
use crate::audit::{AuditAction, AuditEntry, AuditSource, AuditStatus};
use crate::guard::{Guard, Rule};
use crate::id::{GroupId, HostId};
use crate::model::{Approval, Auth, Host, Profile};
use crate::sftp;
use crate::store::{Record, Store};
use crate::termrun::{TerminalError, TerminalOutput};

pub(super) async fn handle(
    shared: &Shared,
    lang: Lang,
    request: Request,
    cancel: Cancel,
) -> Response {
    let ctx = Ctx {
        shared,
        lang,
        cancel,
    };
    // 只记请求的种类与主机：命令内容、路径之外的参数不记
    let (kind, host) = match &request {
        Request::ListHosts { .. } => ("ls", None),
        Request::Exec(r) => (
            if r.terminal {
                "exec --terminal"
            } else {
                "exec"
            },
            Some(r.host.clone()),
        ),
        Request::Upload(r) => ("upload", Some(r.host.clone())),
        Request::Download(r) => ("download", Some(r.host.clone())),
        Request::Activate { .. } => ("activate", None),
        Request::Quit => ("quit", None),
    };
    let started = Instant::now();
    tracing::debug!(kind, host = host.as_deref().unwrap_or(""), "命令行请求");
    let response = match request {
        Request::ListHosts { query } => ctx.list_hosts(&query),
        Request::Exec(request) => ctx.exec(request).await,
        Request::Upload(request) => ctx.transfer(request, true).await,
        Request::Download(request) => ctx.transfer(request, false).await,
        Request::Activate { args, files } => ctx.activate(args, files).await,
        Request::Quit => ctx.quit().await,
    };
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match &response {
        Response::Error(e) => tracing::info!(
            kind,
            host = host.as_deref().unwrap_or(""),
            code = e.code.as_str(),
            elapsed_ms,
            "命令行请求失败：{}",
            e.message
        ),
        _ => tracing::debug!(
            kind,
            host = host.as_deref().unwrap_or(""),
            elapsed_ms,
            "命令行请求完成"
        ),
    }
    response
}

fn error(code: ErrorCode, message: String) -> Response {
    Response::Error(ErrorBody::new(code, message))
}

/// 一次调用的审计记录。
struct Call {
    started: Instant,
    /// 等待用户批准危险命令的时长（询问过用户的才有）：不算进用时
    approval_wait: Option<Duration>,
    entry: AuditEntry,
}

impl Call {
    fn new(action: AuditAction, host_key: &str, detail: String, caller: &[String]) -> Self {
        Self {
            started: Instant::now(),
            approval_wait: None,
            entry: AuditEntry {
                host_name: host_key.to_string(),
                action,
                detail,
                caller: caller.join(" < "),
                ..Default::default()
            },
        }
    }

    /// 用时：从收到请求到现在，不含等待用户批准的时间。
    fn elapsed_ms(&self) -> u64 {
        let elapsed = self.started.elapsed();
        elapsed
            .saturating_sub(self.approval_wait.unwrap_or_default())
            .as_millis() as u64
    }

    fn approval_wait_ms(&self) -> Option<u64> {
        self.approval_wait.map(|wait| wait.as_millis() as u64)
    }

    /// 写入审计日志；写入失败只记运行日志，不影响结果。
    async fn record(mut self, shared: &Shared, status: AuditStatus, error: Option<&CliError>) {
        self.entry.status = status;
        self.entry.at = crate::time::now_ms();
        self.entry.duration_ms = self.elapsed_ms() as i64;
        self.entry.approval_wait_ms = self.approval_wait_ms().map(|ms| ms as i64);
        self.entry.source = AuditSource::of(self.entry.action, &self.entry.caller);
        if self.entry.source != AuditSource::Panel {
            crate::usage::mark(crate::audit::usage_feature(&self.entry.caller));
        }
        if let Some(error) = error {
            self.entry.error_code = error.code.as_str().to_string();
        }
        let audit = shared.audit.clone();
        let entry = self.entry;
        // 交给界面的是摘要（与列表中的一样），不复制可能很长的命令
        let summary = entry.summary();
        let saved = tokio::task::spawn_blocking(move || {
            audit.add(entry).map(|id| AuditEntry { id, ..summary })
        })
        .await;
        match saved {
            Ok(Ok(entry)) => {
                // 界面不处理也可以：队列满时丢弃
                let _ = shared.events.try_send(CliEvent::Audited(entry));
            }
            Ok(Err(e)) => tracing::error!("写入审计日志失败：{e}"),
            Err(e) => tracing::error!("写入审计日志失败：{e}"),
        }
    }
}

/// 请求批准的结果。
enum Asked {
    Approved,
    Denied,
    Timeout,
    /// 没有窗口可以询问（界面没有接收，或丢弃了请求）
    NoDesktop,
    Locked,
    Canceled,
}

struct Ctx<'a> {
    shared: &'a Shared,
    lang: Lang,
    cancel: Cancel,
}

impl Ctx<'_> {
    fn tr(&self, zh: &str, en: &str) -> String {
        tr(self.lang, zh, en)
    }

    /// 锁定与外部命令行的开关。
    fn gate(&self) -> Result<(), CliError> {
        if *self.shared.locked.borrow() {
            return Err(CliError::code(ErrorCode::AppLocked));
        }
        if !self.shared.store.local_settings().cli.enabled {
            return Err(CliError::code(ErrorCode::CliDisabled));
        }
        Ok(())
    }

    /// 检查锁定与开关、找到主机并检查是否允许命令行访问。外部命令行关闭时同样记入审计日志（便于发现有程序在尝试调用）。
    fn access(&self, call: &mut Call) -> Result<Record<Host>, Failure> {
        self.gate()?;
        let record = find_host(&self.shared.store, &call.entry.host_name, self.lang)?;
        call.entry.host_id = record.id.to_string();
        call.entry.host_name = record.doc.name.clone();
        if !record.doc.cli.allowed || !record.doc.is_ssh() {
            return Err(CliError::code(ErrorCode::HostNotAllowed).into());
        }
        Ok(record)
    }

    fn check_vault(&self) -> Result<(), CliError> {
        match vault_error(self.shared.store.vault_status()) {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    // ---------------------------------------------------------------- 列出主机

    fn list_hosts(&self, query: &str) -> Response {
        if let Err(e) = self.gate() {
            return Response::Error(e.body());
        }
        let store = &self.shared.store;
        let query = query.trim().to_lowercase();
        let mut hosts: Vec<HostSummary> = store
            .list::<Host>()
            .into_iter()
            .filter(|r| r.doc.cli.allowed && r.doc.is_ssh())
            .filter(|r| {
                query.is_empty()
                    || r.doc.matches(&query)
                    || r.id.as_str().to_lowercase().contains(&query)
            })
            .map(|r| HostSummary {
                id: r.id.to_string(),
                name: r.doc.name.clone(),
                host: r.doc.address.clone(),
                port: r.doc.effective_port(),
                username: cli_username(store, &r),
                protocol: "SSH".into(),
            })
            .collect();
        hosts.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        });
        Response::Hosts(hosts)
    }

    // ---------------------------------------------------------------- 执行

    async fn exec(&self, request: ExecRequest) -> Response {
        if request.host.is_empty() || request.command.is_empty() {
            return error(
                ErrorCode::BadRequest,
                self.tr("需要 host 和 command", "host and command are required"),
            );
        }
        let action = if request.terminal {
            AuditAction::Terminal
        } else {
            AuditAction::Exec
        };
        let mut call = Call::new(
            action,
            &request.host,
            request.command.clone(),
            &request.caller,
        );
        match self.exec_inner(&mut call, &request).await {
            Ok((response, status)) => {
                call.entry.exit_code = Some(response.exit_code);
                tracing::info!(
                    host = %call.entry.host_id,
                    terminal = request.terminal,
                    exit_code = response.exit_code,
                    truncated = response.truncated,
                    "命令行执行完成"
                );
                call.record(self.shared, status, None).await;
                Response::Exec(response)
            }
            Err(failure) => {
                call.record(self.shared, failure.status, Some(&failure.error))
                    .await;
                Response::Error(failure.error.body())
            }
        }
    }

    async fn exec_inner(
        &self,
        call: &mut Call,
        request: &ExecRequest,
    ) -> Result<(ExecResponse, AuditStatus), Failure> {
        let record = self.access(call)?;
        // 终端以桌面端登录的用户（可能已经 sudo -i）运行，会绕过 CLI 专用账号这道服务器端的权限边界
        if request.terminal && record.doc.cli.credential.is_some() {
            return Err(CliError::new(
                ErrorCode::BadRequest,
                self.tr(
                    "这台主机设置了 CLI 专用凭据，不能使用 --terminal（终端以桌面端登录的用户身份运行，会绕过 CLI 专用账号的权限限制）；请去掉 --terminal，用普通 exec 执行",
                    "this host has a dedicated CLI credential, so --terminal is not allowed (the terminal runs as the user logged in to Termlet and would bypass the CLI account's restrictions); drop --terminal and use a plain exec",
                ),
            )
            .into());
        }
        if !request.terminal {
            self.check_vault()?;
        }
        let status = self.check_rules(call, &record, &request.command).await?;
        // 在终端中执行用的是终端的连接：快过期的证书在连接时已在终端里提醒过
        let (exit_code, stdout, stderr, truncated, expiring_certificates) = if request.terminal {
            let output = self.run_in_terminal(&record, &request.command).await?;
            (
                output.exit_code,
                output.output,
                String::new(),
                output.truncated,
                Vec::new(),
            )
        } else {
            let (
                ExecOutput {
                    exit_code,
                    stdout,
                    stderr,
                    truncated,
                },
                expiring,
            ) = self.run_exec(&record, &request.command).await?;
            (
                exit_code,
                String::from_utf8_lossy(&stdout).into_owned(),
                String::from_utf8_lossy(&stderr).into_owned(),
                truncated,
                expiring,
            )
        };
        Ok((
            ExecResponse {
                exit_code,
                stdout,
                stderr,
                truncated,
                duration_ms: call.elapsed_ms(),
                approval_wait_ms: call.approval_wait_ms(),
                expiring_certificates,
            },
            status,
        ))
    }

    /// 危险命令规则：没有命中时为 `Ok`，经批准执行时为 `Approved`；拦截、拒绝时为错误。检查的是完整的命令（多行脚本逐行）。
    /// 询问了用户时把等待的时长记在 `call` 中（不算进用时）。
    async fn check_rules(
        &self,
        call: &mut Call,
        record: &Record<Host>,
        command: &str,
    ) -> Result<AuditStatus, Failure> {
        let settings = self.shared.store.settings().guard;
        let (guard, invalid) = Guard::from_settings(&settings);
        if !invalid.is_empty() {
            tracing::warn!(
                count = invalid.len(),
                "设置中有不合法的自定义危险命令规则，已跳过"
            );
        }
        let Some(rule) = guard.check(command).cloned() else {
            return Ok(AuditStatus::Ok);
        };
        let desc = rule_text(self.lang, &rule);
        let blocked = || Failure {
            status: AuditStatus::Blocked,
            error: CliError::new(ErrorCode::CommandBlocked, desc.clone()),
        };
        let denied = |zh: &str, en: &str| Failure {
            status: AuditStatus::Denied,
            error: CliError::new(
                ErrorCode::CommandDenied,
                match self.lang {
                    Lang::Zh => format!("{zh}（{desc}）"),
                    Lang::En => format!("{en} ({desc})"),
                },
            ),
        };
        let rule_id = rule.id().to_string();
        if settings.approval == Approval::Block {
            tracing::info!(host = %record.id, rule = rule_id, "命令行的命令被拦截");
            return Err(blocked());
        }
        let (asked, waited) = self.ask(record, command, &call.entry.caller, rule).await;
        call.approval_wait = waited;
        match asked {
            Asked::Approved => {
                // 批准与锁定可能同时发生：执行前再检查一次
                if *self.shared.locked.borrow() {
                    return Err(CliError::code(ErrorCode::AppLocked).into());
                }
                tracing::info!(host = %record.id, rule = rule_id, "命令行的命令经用户批准执行");
                Ok(AuditStatus::Approved)
            }
            Asked::Denied => {
                tracing::info!(host = %record.id, rule = rule_id, "命令行的命令被用户拒绝");
                Err(denied("用户拒绝执行", "denied by the user"))
            }
            Asked::Timeout => Err(denied("等待批准超时", "timed out waiting for approval")),
            // 没有窗口可以询问：按拦截处理
            Asked::NoDesktop => {
                tracing::info!(host = %record.id, rule = rule_id, "命令行的命令被拦截（没有窗口可以询问）");
                Err(blocked())
            }
            // 等待期间被锁定：与其他锁定时的调用一样返回 app_locked
            Asked::Locked => Err(CliError::code(ErrorCode::AppLocked).into()),
            Asked::Canceled => Err(CliError::code(ErrorCode::Canceled).into()),
        }
    }

    /// 请界面批准：发出请求后等用户回答，最多等 `approval_timeout`；锁定、命令行工具退出时立即结束。另返回等待的
    /// 时长（没有发出请求、没有窗口可以询问时为 `None`）。
    async fn ask(
        &self,
        record: &Record<Host>,
        command: &str,
        caller: &str,
        rule: Rule,
    ) -> (Asked, Option<Duration>) {
        let mut locked = self.shared.locked.subscribe();
        if *locked.borrow() {
            return (Asked::Locked, None);
        }
        let asked_at = Instant::now();
        let timeout = self.shared.approval_timeout;
        let (reply_tx, reply_rx) = oneshot::channel();
        let (done_tx, done_rx) = watch::channel(false);
        crate::usage::mark(crate::usage::Feature::ApprovalPrompt);
        let request = ApprovalRequest {
            host_id: record.id.clone(),
            host_name: record.doc.name.clone(),
            command: command.to_string(),
            caller: caller.to_string(),
            rule,
            deadline: Instant::now() + timeout,
            reply: Some(reply_tx),
            done: done_rx,
        };
        let events = self.shared.events.clone();
        let wait = async move {
            if events.send(CliEvent::Approval(request)).await.is_err() {
                return Asked::NoDesktop;
            }
            match reply_rx.await {
                Ok(true) => Asked::Approved,
                Ok(false) => Asked::Denied,
                Err(_) => Asked::NoDesktop,
            }
        };
        let mut cancel = self.cancel.clone();
        let outcome = tokio::select! {
            outcome = wait => outcome,
            _ = tokio::time::sleep(timeout) => Asked::Timeout,
            _ = async { locked.wait_for(|l| *l).await.is_ok() } => Asked::Locked,
            _ = cancel.wait() => Asked::Canceled,
        };
        // 界面据此关闭对话框
        done_tx.send_replace(true);
        let waited = match outcome {
            Asked::NoDesktop => None,
            _ => Some(asked_at.elapsed()),
        };
        (outcome, waited)
    }

    /// 交给界面在已打开的终端中执行。
    async fn run_in_terminal(
        &self,
        record: &Record<Host>,
        command: &str,
    ) -> Result<TerminalOutput, CliError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let request = TerminalRequest {
            host_id: record.id.clone(),
            host_name: record.doc.name.clone(),
            command: command.to_string(),
            limit: OUTPUT_LIMIT,
            reply: reply_tx,
            canceled: cancel_rx,
        };
        let events = self.shared.events.clone();
        let wait = async move {
            if events.send(CliEvent::RunInTerminal(request)).await.is_err() {
                return Err(TerminalError::NotOpen);
            }
            reply_rx.await.unwrap_or(Err(TerminalError::NotOpen))
        };
        let mut cancel = self.cancel.clone();
        let result = tokio::select! {
            result = wait => result,
            _ = cancel.wait() => {
                // 界面据此向终端发送 Ctrl+C
                cancel_tx.send_replace(true);
                Err(TerminalError::Canceled)
            }
        };
        result.map_err(|e| from_terminal(&e, self.lang))
    }

    /// 新建连接执行，用完即断开。另返回登录用的证书中快过期的（结果中提醒）。
    async fn run_exec(
        &self,
        record: &Record<Host>,
        command: &str,
    ) -> Result<(ExecOutput, Vec<ExpiringCertificate>), CliError> {
        let mut cancel = self.cancel.clone();
        let (connection, _) =
            connect::connect(self.shared.store.clone(), &record.id, &mut cancel).await?;
        let expiring = expiring_certificates(&connection);
        let result = connect::exec(&connection, command, OUTPUT_LIMIT, &mut cancel).await;
        disconnect_later(connection);
        result.map(|output| (output, expiring))
    }

    // ---------------------------------------------------------------- 传输

    async fn transfer(&self, request: TransferRequest, upload: bool) -> Response {
        if request.host.is_empty() || request.local.is_empty() || request.remote.is_empty() {
            return error(
                ErrorCode::BadRequest,
                self.tr(
                    "需要 host、local 和 remote",
                    "host, local and remote are required",
                ),
            );
        }
        let (action, detail) = if upload {
            (
                AuditAction::Upload,
                format!("{} → {}", request.local, request.remote),
            )
        } else {
            (
                AuditAction::Download,
                format!("{} → {}", request.remote, request.local),
            )
        };
        let mut call = Call::new(action, &request.host, detail, &request.caller);
        match self.transfer_inner(&mut call, &request, upload).await {
            Ok(response) => {
                call.entry.bytes = Some(response.bytes as i64);
                call.entry.detail = if upload {
                    format!("{} → {}", response.local, response.remote)
                } else {
                    format!("{} → {}", response.remote, response.local)
                };
                call.record(self.shared, AuditStatus::Ok, None).await;
                Response::Transfer(response)
            }
            Err(failure) => {
                call.record(self.shared, failure.status, Some(&failure.error))
                    .await;
                Response::Error(failure.error.body())
            }
        }
    }

    async fn transfer_inner(
        &self,
        call: &mut Call,
        request: &TransferRequest,
        upload: bool,
    ) -> Result<TransferResponse, Failure> {
        let local = PathBuf::from(&request.local);
        if !local.is_absolute() {
            return Err(CliError::new(
                ErrorCode::BadRequest,
                self.tr("本地路径必须是绝对路径", "the local path must be absolute"),
            )
            .into());
        }
        let record = self.access(call)?;
        self.check_vault()?;
        let mut cancel = self.cancel.clone();
        let (connection, refused) =
            connect::connect(self.shared.store.clone(), &record.id, &mut cancel).await?;
        let expiring_certificates = expiring_certificates(&connection);
        // 取消时由传输自己删除临时文件再返回（直接丢弃会在远程或本机留下临时文件）
        let stopped = std::sync::atomic::AtomicBool::new(false);
        let stop = async {
            cancel.wait().await;
            stopped.store(true, std::sync::atomic::Ordering::Relaxed);
        };
        // 服务器没有 SFTP 时改用 SCP（“只用一个通道”的主机除外），结果中说明
        let single_channel = record.doc.ssh.single_channel;
        let result = if upload {
            sftp::put_file(&connection, &local, &request.remote, single_channel, stop)
                .await
                .map(|done| (request.local.clone(), done.path, done.bytes, done.protocol))
        } else {
            sftp::get_file(&connection, &request.remote, &local, single_channel, stop)
                .await
                .map(|done| {
                    let local = done.path.display().to_string();
                    (local, request.remote.clone(), done.bytes, done.protocol)
                })
        };
        let result = if stopped.load(std::sync::atomic::Ordering::Relaxed) {
            Err(CliError::code(ErrorCode::Canceled))
        } else {
            result.map_err(|e| {
                let refused = refused.lock().unwrap_or_else(|e| e.into_inner()).clone();
                from_sftp(&e, refused.as_ref(), self.lang)
            })
        };
        disconnect_later(connection);
        let (local, remote, bytes, protocol) = result?;
        Ok(TransferResponse {
            local,
            remote,
            bytes,
            duration_ms: call.elapsed_ms(),
            protocol: match protocol {
                sftp::Protocol::Sftp => termlet_proto::TRANSFER_SFTP,
                sftp::Protocol::Scp => termlet_proto::TRANSFER_SCP,
            }
            .to_string(),
            expiring_certificates,
        })
    }

    // ---------------------------------------------------------------- 启动参数

    async fn activate(&self, args: Vec<String>, files: Vec<termlet_proto::LaunchFile>) -> Response {
        let sent = tokio::time::timeout(
            Duration::from_secs(5),
            self.shared.events.send(CliEvent::Activate { args, files }),
        )
        .await;
        match sent {
            Ok(Ok(())) => Response::Ok,
            _ => error(
                ErrorCode::Internal,
                self.tr("Termlet 没有响应", "Termlet is not responding"),
            ),
        }
    }

    async fn quit(&self) -> Response {
        let (reply, answer) = oneshot::channel();
        let asked = tokio::time::timeout(Duration::from_secs(5), async {
            self.shared.events.send(CliEvent::Quit(reply)).await.ok()?;
            answer.await.ok()
        })
        .await;
        match asked {
            Ok(Some(true)) => Response::Ok,
            _ => error(
                ErrorCode::Internal,
                self.tr(
                    "Termlet 有打开的窗口或进行中的请求，没有退出",
                    "Termlet has open windows or requests in progress, so it didn't quit",
                ),
            ),
        }
    }
}

/// 连接登录用的证书（经过的跳板机的与目标主机的）中快过期的，与终端里的提醒按同一个判断
/// （[`crate::ssh::Connection::expiring_certificates`]）。
fn expiring_certificates(connection: &crate::ssh::Connection) -> Vec<ExpiringCertificate> {
    let now = crate::sshcert::now_secs();
    let expiring: Vec<ExpiringCertificate> = connection
        .expiring_certificates(now)
        .into_iter()
        .map(|(jump, expiry)| ExpiringCertificate {
            jump,
            expires_at: crate::time::format_rfc3339_local(expiry.saturating_mul(1000)),
            minutes_left: crate::sshcert::minutes_left(expiry, now) as u64,
        })
        .collect();
    if !expiring.is_empty() {
        tracing::info!(
            count = expiring.len(),
            jump = expiring.iter().any(|c| c.jump.is_some()),
            "命令行连接登录用的证书快过期，结果中提醒"
        );
    }
    expiring
}

/// 在后台断开（不让命令行等它）。
fn disconnect_later(connection: Arc<crate::ssh::Connection>) {
    crate::runtime::spawn(async move { connection.disconnect().await });
}

/// 命中的规则的说明：`关机或重启（power）`、`Shutdown or reboot (power)`；自定义规则的说明是正则本身。
fn rule_text(lang: Lang, rule: &Rule) -> String {
    match lang {
        Lang::Zh => format!("{}（{}）", rule.description(), rule.id()),
        Lang::En => format!("{} ({})", rule.description_en(), rule.id()),
    }
}

/// 按 ID 或名称找主机。名称相同的有多台时：只有一台允许命令行访问就用它；都不允许时用第一台（随后报"未允许"）；
/// 有多台允许时报错，请改用 ID。
fn find_host(store: &Store, key: &str, lang: Lang) -> Result<Record<Host>, CliError> {
    if let Some(record) = store.get::<Host>(&HostId::from_raw(key)) {
        return Ok(record);
    }
    let mut same: Vec<Record<Host>> = store
        .list::<Host>()
        .into_iter()
        .filter(|r| r.doc.name == key)
        .collect();
    same.sort_by(|a, b| a.id.cmp(&b.id));
    let allowed: Vec<&Record<Host>> = same
        .iter()
        .filter(|r| r.doc.cli.allowed && r.doc.is_ssh())
        .collect();
    match (same.first(), allowed.as_slice()) {
        (None, _) => Err(CliError::code(ErrorCode::HostNotFound)),
        (Some(first), []) => Ok(first.clone()),
        (_, [only]) => Ok((*only).clone()),
        (_, many) => {
            let ids: Vec<&str> = many.iter().map(|r| r.id.as_str()).collect();
            Err(CliError::new(
                ErrorCode::BadRequest,
                match lang {
                    Lang::Zh => format!(
                        "有多台允许 CLI 访问的主机名为 {key}，请改用主机 ID：{}",
                        ids.join("、")
                    ),
                    Lang::En => format!(
                        "several hosts that allow CLI access are named {key}; use a host ID instead: {}",
                        ids.join(", ")
                    ),
                },
            ))
        }
    }
}

/// 命令行登录用的用户名：CLI 专用凭据的用户名，否则与连接时相同（主机 → 凭据档案 → 分组默认值 → 本机用户名）。
fn cli_username(store: &Store, record: &Record<Host>) -> String {
    let host = &record.doc;
    if let Some(credential) = &host.cli.credential
        && !credential.username.trim().is_empty()
    {
        return credential.username.clone();
    }
    if !host.username.trim().is_empty() {
        return host.username.clone();
    }
    let group = record.parent.clone().map(GroupId::from_raw);
    let chain = store.group_chain(group.as_ref());
    let profile = match &host.auth {
        Auth::Profile { profile } => Some(profile.clone()),
        Auth::Inherit => chain.iter().find_map(|g| g.doc.defaults.profile.clone()),
        _ => None,
    };
    if let Some(profile) = profile.and_then(|id| store.get::<Profile>(&id))
        && !profile.doc.username.trim().is_empty()
    {
        return profile.doc.username;
    }
    chain
        .iter()
        .map(|g| g.doc.defaults.username.clone())
        .find(|u| !u.trim().is_empty())
        .unwrap_or_else(|| {
            std::env::var(if cfg!(windows) { "USERNAME" } else { "USER" })
                .unwrap_or_else(|_| "root".into())
        })
}
