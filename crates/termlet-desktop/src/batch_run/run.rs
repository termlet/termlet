//! 执行：主机经核心的批量执行器（`termlet_core::batch`，新建连接，凭据与打开终端时相同），终端经
//! [`TerminalPanel::run_command`]（在终端里执行）。逐台的结果交给 [`Results`]；开始执行了的每台写一条审计记录（来源
//! “批量执行”，同一批带同一个批 ID）。
//!
//! 每次执行有自己的代号（`generation`）：停止后马上再执行时，上一次还在收尾的主机报来的结果不会记到这一次里（审计照写）。

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::{App, Context, Entity, SharedString, Window};
use termlet_core::audit::{AuditAction, AuditEntry, AuditLog, AuditStatus, CALLER_BATCH};
use termlet_core::batch::{self, Access, Event, Job};
use termlet_core::id::HostId;
use termlet_core::model::Host;
use termlet_core::proto::{BatchHostResult, BatchStatus, ErrorBody, ErrorCode};
use termlet_core::store::Record;
use termlet_core::termrun::{TerminalError, TerminalOutput};
use tokio::sync::{mpsc, watch};

use super::results::{Finished, Results, TargetKey};
use super::{BatchPanel, Running};
use crate::app_state::AppState;
use crate::i18n::{self, Language};
use crate::t;
use crate::terminal_panel::{RUN_OUTPUT_LIMIT, RunError, TerminalPanel};

/// 要在哪些目标上执行。
pub(super) enum Targets {
    Hosts(Vec<HostId>),
    Terminals(Vec<Entity<TerminalPanel>>),
}

impl Targets {
    /// 能执行的台数（确认框中写的）：主机只算 SSH 主机。
    pub(super) fn runnable(&self, cx: &App) -> usize {
        match self {
            Targets::Hosts(ids) => {
                let store = AppState::store(cx);
                ids.iter()
                    .filter(|id| store.get::<Host>(id).is_some_and(|h| h.doc.is_ssh()))
                    .count()
            }
            Targets::Terminals(panels) => panels.len(),
        }
    }
}

/// 一次执行的审计记录共用的部分。
#[derive(Clone)]
struct Audit {
    log: Option<Arc<AuditLog>>,
    batch: String,
    command: String,
    /// 命中危险命令规则、用户确认后执行的
    approved: bool,
}

impl Audit {
    /// 写一条（在后台写库；没有审计日志或写入失败只记运行日志）。
    fn write(&self, entry: AuditEntry, cx: &App) {
        let Some(log) = self.log.clone() else {
            return;
        };
        let entry = AuditEntry {
            action: AuditAction::Exec,
            detail: self.command.clone(),
            caller: CALLER_BATCH.to_string(),
            batch: self.batch.clone(),
            ..entry
        };
        cx.background_executor()
            .spawn(async move {
                if let Err(e) = log.add(entry) {
                    tracing::warn!("写入审计日志失败：{e}");
                }
            })
            .detach();
    }

    fn status(&self, error: bool) -> AuditStatus {
        match (error, self.approved) {
            (true, _) => AuditStatus::Failed,
            (false, true) => AuditStatus::Approved,
            (false, false) => AuditStatus::Ok,
        }
    }
}

impl BatchPanel {
    /// 开始执行（危险命令已确认过）。
    pub(super) fn start(
        &mut self,
        command: String,
        targets: Targets,
        approved: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.running.is_some() {
            return;
        }
        termlet_core::usage::mark(termlet_core::usage::Feature::BatchExec);
        self.remember(&command);
        self.generation += 1;
        // 命令行服务的审计日志；没有启动它时（单元测试）自己打开
        let log = crate::cli_bridge::audit(cx)
            .or_else(|| AuditLog::new(AppState::store(cx)).ok().map(Arc::new));
        let audit = Audit {
            log,
            batch: batch::new_batch_id(),
            command: command.clone(),
            approved,
        };
        let (stop, stopped) = watch::channel(false);
        let started = match targets {
            Targets::Hosts(ids) => self.start_hosts(&command, ids, audit, stopped, cx),
            Targets::Terminals(panels) => {
                self.start_terminals(&command, panels, audit, stopped, cx)
            }
        };
        self.scroll.set_offset(Default::default());
        if !started {
            // 没有能执行的（选中的都是 Telnet、串口主机）：结果中已列为未执行
            if let Some(results) = self.results.as_mut() {
                results.elapsed = Some(Duration::ZERO);
            }
        } else {
            self.running = Some(Running {
                stop,
                started: Instant::now(),
            });
        }
        self.input.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    /// 在主机上执行；返回有没有开始（选中的都不能执行时没有）。
    fn start_hosts(
        &mut self,
        command: &str,
        ids: Vec<HostId>,
        audit: Audit,
        stopped: watch::Receiver<bool>,
        cx: &mut Context<Self>,
    ) -> bool {
        let store = AppState::store(cx);
        let (run, skipped): (Vec<Record<Host>>, Vec<Record<Host>>) = ids
            .iter()
            .filter_map(|id| store.get::<Host>(id))
            .partition(|h| h.doc.is_ssh());
        let targets = run
            .iter()
            .chain(&skipped)
            .map(|h| {
                (
                    TargetKey::Host(h.id.clone()),
                    SharedString::from(h.doc.name.clone()),
                )
            })
            .collect();
        let mut results = Results::new(command.to_string().into(), targets);
        for ix in run.len()..run.len() + skipped.len() {
            results.finish(
                ix,
                Finished {
                    status: BatchStatus::Skipped,
                    reason: Some(t!("batch.reason.not_ssh")),
                    error: Some(ErrorCode::HostNotAllowed),
                    ..Default::default()
                },
            );
        }
        self.results = Some(results);
        if run.is_empty() {
            return false;
        }
        tracing::info!(
            batch = %audit.batch,
            hosts = run.len(),
            skipped = skipped.len(),
            approved = audit.approved,
            "批量执行：在主机上执行"
        );
        let (events, mut received) = mpsc::unbounded_channel();
        let options = batch::Options {
            parallel: termlet_core::proto::batch::DEFAULT_PARALLEL as usize,
            timeout: None,
            max_fail: None,
            output_limit: RUN_OUTPUT_LIMIT,
            // 输出随事件交过来，执行器不用留到最后
            keep_output: false,
        };
        let job = Job::Exec {
            command: command.to_string(),
            stdin: Vec::new(),
        };
        let lang = i18n::core_lang(i18n::language());
        let hosts = run.clone();
        let done = termlet_core::runtime::spawn(batch::run(
            store,
            Access::User,
            run,
            job,
            options,
            lang,
            stopped,
            events,
        ));
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let mut begun = HashSet::new();
            while let Some(event) = received.recv().await {
                let handled = this.update(cx, |this, cx| {
                    this.on_host_event(generation, event, &hosts, &mut begun, &audit, cx)
                });
                if handled.is_err() {
                    break;
                }
            }
            let _ = done.await;
            let _ = this.update(cx, |this, cx| this.finish_run(generation, cx));
        })
        .detach();
        true
    }

    fn on_host_event(
        &mut self,
        generation: u64,
        event: Event,
        hosts: &[Record<Host>],
        begun: &mut HashSet<usize>,
        audit: &Audit,
        cx: &mut Context<Self>,
    ) {
        let current = generation == self.generation;
        match event {
            Event::Started(ix) => {
                begun.insert(ix);
                if current && let Some(results) = self.results.as_mut() {
                    results.start(ix);
                }
            }
            Event::Finished(ix, result) => {
                if begun.contains(&ix)
                    && let Some(host) = hosts.get(ix)
                {
                    audit.write(host_entry(host, &result, audit), cx);
                }
                if current && let Some(results) = self.results.as_mut() {
                    results.finish(ix, finished_of_host(&result));
                    if results.done() {
                        self.finish_run(generation, cx);
                    }
                }
            }
        }
        cx.notify();
    }

    /// 在终端里执行；返回有没有开始。
    fn start_terminals(
        &mut self,
        command: &str,
        panels: Vec<Entity<TerminalPanel>>,
        audit: Audit,
        stopped: watch::Receiver<bool>,
        cx: &mut Context<Self>,
    ) -> bool {
        let targets = panels
            .iter()
            .map(|p| {
                (
                    TargetKey::Terminal(p.entity_id()),
                    p.read(cx).name().clone(),
                )
            })
            .collect();
        self.results = Some(Results::new(command.to_string().into(), targets));
        tracing::info!(
            batch = %audit.batch,
            terminals = panels.len(),
            approved = audit.approved,
            "批量执行：在终端里执行"
        );
        let generation = self.generation;
        let started = !panels.is_empty();
        for (ix, panel) in panels.into_iter().enumerate() {
            let mut stop = stopped.clone();
            let canceled = async move {
                let _ = stop.wait_for(|s| *s).await;
            };
            let job = panel.update(cx, |p, cx| p.run_command(command.to_string(), canceled, cx));
            if let Some(results) = self.results.as_mut() {
                results.start(ix);
            }
            let (name, host) = {
                let p = panel.read(cx);
                (p.name().to_string(), p.ssh_host().cloned())
            };
            let audit = audit.clone();
            let started = Instant::now();
            cx.spawn(async move |this, cx| {
                let outcome = job.await;
                let _ = this.update(cx, |this, cx| {
                    let (finished, ran) = finished_of_terminal(outcome, started.elapsed());
                    if ran {
                        audit.write(
                            AuditEntry {
                                host_id: host.map(|h| h.to_string()).unwrap_or_default(),
                                host_name: name,
                                status: audit.status(finished.error.is_some()),
                                exit_code: finished.exit_code,
                                duration_ms: finished.duration.as_millis() as i64,
                                error_code: finished
                                    .error
                                    .map(|e| e.as_str().to_string())
                                    .unwrap_or_default(),
                                ..Default::default()
                            },
                            cx,
                        );
                    }
                    if generation == this.generation
                        && let Some(results) = this.results.as_mut()
                    {
                        results.finish(ix, finished);
                        if results.done() {
                            this.finish_run(generation, cx);
                        }
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        started
    }

    /// 这一次全部结束了。
    fn finish_run(&mut self, generation: u64, cx: &mut Context<Self>) {
        if generation != self.generation {
            return;
        }
        let Some(running) = self.running.take() else {
            return;
        };
        if let Some(results) = self.results.as_mut() {
            results.elapsed = Some(running.started.elapsed());
            let summary = results.summary();
            tracing::info!(
                ok = summary.ok,
                failed = summary.failed,
                unreachable = summary.unreachable,
                canceled = summary.canceled,
                skipped = summary.skipped,
                "批量执行结束"
            );
        }
        cx.notify();
    }
}

/// 主机的结果：输出是标准输出后接标准错误。
fn finished_of_host(result: &BatchHostResult) -> Finished {
    let mut output = result.stdout.clone();
    if !result.stderr.is_empty() {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&result.stderr);
    }
    // 已取消的不另写原因（状态就是）
    let reason = result
        .error
        .as_ref()
        .filter(|e| e.error_code() != Some(ErrorCode::Canceled))
        .map(error_reason);
    Finished {
        status: result.status,
        exit_code: result.exit_code,
        output,
        truncated: result.truncated,
        reason,
        error: result.error.as_ref().and_then(ErrorBody::error_code),
        duration: Duration::from_millis(result.duration_ms),
    }
}

/// 主机的审计记录。
fn host_entry(host: &Record<Host>, result: &BatchHostResult, audit: &Audit) -> AuditEntry {
    AuditEntry {
        host_id: host.id.to_string(),
        host_name: host.doc.name.clone(),
        status: audit.status(result.error.is_some()),
        exit_code: result.exit_code,
        error_code: result
            .error
            .as_ref()
            .map(|e| e.code.clone())
            .unwrap_or_default(),
        duration_ms: result.duration_ms as i64,
        ..Default::default()
    }
}

/// 终端的结果与命令有没有真的开始执行（开始了的才写审计）。
fn finished_of_terminal(
    outcome: Result<TerminalOutput, RunError>,
    duration: Duration,
) -> (Finished, bool) {
    let refused = |status: BatchStatus, reason: SharedString, error: Option<ErrorCode>| Finished {
        status,
        reason: Some(reason),
        error,
        duration,
        ..Default::default()
    };
    match outcome {
        Ok(out) => (
            Finished {
                status: if out.exit_code == 0 {
                    BatchStatus::Ok
                } else {
                    BatchStatus::Failed
                },
                exit_code: Some(out.exit_code),
                output: out.output,
                truncated: out.truncated,
                duration,
                ..Default::default()
            },
            true,
        ),
        Err(RunError::Unsupported) => (
            refused(
                BatchStatus::Skipped,
                t!("batch.unsupported"),
                Some(ErrorCode::HostNotAllowed),
            ),
            false,
        ),
        Err(RunError::NotConnected | RunError::Terminal(TerminalError::NotOpen)) => (
            refused(
                BatchStatus::Unreachable,
                t!("batch.not_connected"),
                Some(ErrorCode::TerminalNotOpen),
            ),
            false,
        ),
        Err(RunError::Terminal(TerminalError::Busy)) => (
            refused(
                BatchStatus::Skipped,
                t!("batch.busy"),
                Some(ErrorCode::TerminalBusy),
            ),
            false,
        ),
        Err(RunError::Terminal(TerminalError::TooLong)) => (
            refused(
                BatchStatus::Skipped,
                t!("batch.too_long"),
                Some(ErrorCode::BadRequest),
            ),
            false,
        ),
        Err(RunError::Terminal(TerminalError::Closed)) => (
            refused(
                BatchStatus::Failed,
                t!("batch.closed"),
                Some(ErrorCode::ConnectFailed),
            ),
            true,
        ),
        Err(RunError::Terminal(TerminalError::Canceled)) => (
            Finished {
                status: BatchStatus::Canceled,
                error: Some(ErrorCode::Canceled),
                duration,
                ..Default::default()
            },
            true,
        ),
    }
}

/// 没有成功的原因（界面语言）：按错误码说明，核心给出的补充说明（服务器的说法、主机名等）能看懂时附在后面。
pub(super) fn error_reason(error: &ErrorBody) -> SharedString {
    let reason = match error.error_code() {
        Some(ErrorCode::ConnectFailed) => t!("batch.reason.connect_failed"),
        Some(ErrorCode::AlgorithmMismatch) => t!("batch.reason.algorithm"),
        Some(ErrorCode::CredentialMissing) => t!("batch.reason.credential_missing"),
        Some(ErrorCode::HostKeyUnknown) => t!("batch.reason.hostkey_unknown"),
        Some(ErrorCode::HostKeyMismatch) => t!("batch.reason.hostkey_mismatch"),
        Some(ErrorCode::HostKeyRejected) => t!("batch.reason.hostkey_rejected"),
        Some(ErrorCode::AuthFailed) => t!("batch.reason.auth_failed"),
        Some(ErrorCode::AuthInteractionRequired) => t!("batch.reason.interaction"),
        Some(ErrorCode::ProxyCommandUnconfirmed) => t!("batch.reason.proxy_command"),
        Some(ErrorCode::KeyPassphraseRequired) => t!("batch.reason.key_passphrase"),
        Some(ErrorCode::KeyWrongPassphrase | ErrorCode::KeyUnsupported | ErrorCode::KeyInvalid) => {
            t!("batch.reason.key_invalid")
        }
        Some(ErrorCode::VaultLocked | ErrorCode::VaultNeedsSetup | ErrorCode::VaultKeyLost) => {
            t!("batch.reason.vault")
        }
        Some(ErrorCode::HostNotFound) => t!("batch.reason.host_gone"),
        _ if !error.message.is_empty() => return error.message.clone().into(),
        _ => return error.code.clone().into(),
    };
    if error.message.is_empty() || !readable(&error.message, i18n::language()) {
        return reason;
    }
    t!(
        "batch.reason_detail",
        reason = reason,
        detail = error.message.clone()
    )
}

/// 核心的补充说明有时是中文（连接出错的原文）：界面不是中文时只附不含汉字的。
fn readable(message: &str, language: Language) -> bool {
    matches!(language, Language::ZhCn | Language::ZhTw)
        || !message.chars().any(|c| {
            ('\u{4e00}'..='\u{9fff}').contains(&c) || ('\u{3000}'..='\u{303f}').contains(&c)
        })
}

/// 要在终端中交互一次才能连上的（确认指纹、输入密码或验证码、确认代理命令）：结果中给出“在终端中打开”。
pub(super) fn needs_terminal(error: Option<ErrorCode>) -> bool {
    matches!(
        error,
        Some(
            ErrorCode::HostKeyUnknown
                | ErrorCode::AuthInteractionRequired
                | ErrorCode::CredentialMissing
                | ErrorCode::ProxyCommandUnconfirmed
                | ErrorCode::KeyPassphraseRequired
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_results_merge_stderr_and_explain_errors() {
        i18n::set_language(Language::ZhCn);
        let result = BatchHostResult {
            status: BatchStatus::Failed,
            exit_code: Some(1),
            stdout: "out".into(),
            stderr: "err\n".into(),
            ..Default::default()
        };
        let finished = finished_of_host(&result);
        assert_eq!(finished.output, "out\nerr\n");
        assert!(finished.reason.is_none());
        let unreachable = BatchHostResult {
            status: BatchStatus::Unreachable,
            error: Some(ErrorBody::new(ErrorCode::HostKeyUnknown, "")),
            ..Default::default()
        };
        let finished = finished_of_host(&unreachable);
        assert_eq!(finished.error, Some(ErrorCode::HostKeyUnknown));
        assert!(needs_terminal(finished.error));
        assert!(!needs_terminal(Some(ErrorCode::AuthFailed)));
    }

    #[test]
    fn details_in_chinese_only_show_in_chinese() {
        let error = ErrorBody::new(ErrorCode::ConnectFailed, "连接被拒绝");
        i18n::set_language(Language::ZhCn);
        assert!(error_reason(&error).contains("连接被拒绝"));
        i18n::set_language(Language::En);
        let reason = error_reason(&error);
        assert!(!reason.contains("连接被拒绝"), "{reason}");
        let error = ErrorBody::new(
            ErrorCode::ConnectFailed,
            "Connection refused (os error 111)",
        );
        assert!(error_reason(&error).contains("os error 111"));
        i18n::set_language(Language::ZhCn);
    }

    #[test]
    fn terminal_refusals_are_not_audited() {
        let (busy, ran) =
            finished_of_terminal(Err(RunError::Terminal(TerminalError::Busy)), Duration::ZERO);
        assert_eq!((busy.status, ran), (BatchStatus::Skipped, false));
        let (local, _) = finished_of_terminal(Err(RunError::Unsupported), Duration::ZERO);
        assert_eq!(local.error, Some(ErrorCode::HostNotAllowed), "不能重试");
        let (out, ran) = finished_of_terminal(
            Ok(TerminalOutput {
                exit_code: 3,
                output: "x".into(),
                truncated: false,
            }),
            Duration::ZERO,
        );
        assert_eq!(
            (out.status, out.exit_code, ran),
            (BatchStatus::Failed, Some(3), true)
        );
        let (stopped, ran) = finished_of_terminal(
            Err(RunError::Terminal(TerminalError::Canceled)),
            Duration::ZERO,
        );
        assert_eq!((stopped.status, ran), (BatchStatus::Canceled, true));
    }
}
