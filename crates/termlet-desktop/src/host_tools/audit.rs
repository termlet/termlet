//! 面板写操作的审计记录。
//!
//! 写进与命令行工具共用的审计日志（来源记为主机工具面板）。记录的内容：执行的命令
//! （[`termlet_core::hosttools::audit_detail`] 拼上界面上的名称），没有任何秘密（命令只有 PID、服务名、容器 ID 等，
//! 从不含密码）。

use std::sync::Arc;
use std::time::Duration;

use termlet_core::audit::AuditLog;

use termlet_core::hosttools::{ActionResult, ExecError, ToolsError, audit_detail};

/// 审计记录的种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditAction {
    /// 面板直接执行的写操作
    Panel,
    /// 权限不足时交给终端执行（结果与是否输入了密码不在记录中）
    PanelTerminal,
}

/// 一条审计记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEntry {
    pub action: AuditAction,
    /// 主机：终端的名称（SSH 主机名、WSL 发行版）
    pub host: String,
    /// 命令与名称（`命令  # 名称`），执行超时时注明结果未知
    pub detail: String,
    pub ok: bool,
    /// 出错时的原因（日志用）
    pub error: Option<String>,
    pub exit_code: Option<u32>,
    pub duration: Duration,
}

/// 超时时附在审计记录后面的说明。
const UNKNOWN_RESULT: &str = "结果未知：执行超时，命令可能已在服务器上执行";

/// 写操作执行之后的审计记录：`command` 为执行前的命令（出错时用），`label` 为界面上的名称。
/// 执行超时记为已执行（命令可能已经发出），并注明结果未知。
pub fn action_entry(
    host: &str,
    command: &str,
    label: &str,
    result: &Result<ActionResult, ToolsError>,
    duration: Duration,
) -> AuditEntry {
    let (detail, ok, error, exit_code) = match result {
        Ok(result) => (
            audit_detail(&result.command, label),
            result.ok,
            None,
            Some(result.exit_code),
        ),
        Err(error @ ToolsError::Exec(ExecError::Timeout)) => (
            format!("{}  # {UNKNOWN_RESULT}", audit_detail(command, label)),
            true,
            Some(error.to_string()),
            None,
        ),
        Err(error) => (
            audit_detail(command, label),
            false,
            Some(error.to_string()),
            None,
        ),
    };
    AuditEntry {
        action: AuditAction::Panel,
        host: host.to_string(),
        detail,
        ok,
        error,
        exit_code,
        duration,
    }
}

/// 交给终端执行的命令的审计记录。
pub fn terminal_entry(host: &str, command: &str, label: &str) -> AuditEntry {
    AuditEntry {
        action: AuditAction::PanelTerminal,
        host: host.to_string(),
        detail: audit_detail(command, label),
        ok: true,
        error: None,
        exit_code: None,
        duration: Duration::ZERO,
    }
}

/// 写一条审计记录（在后台线程写库）；没有审计日志或写入失败只记运行日志，不影响操作的结果。使用统计的功能名单记
/// 一次“在主机工具中执行过写操作”。
pub fn record(log: Option<Arc<AuditLog>>, entry: AuditEntry) {
    termlet_core::usage::mark(termlet_core::usage::Feature::HostToolsAction);
    tracing::info!(
        action = ?entry.action,
        host = %entry.host,
        ok = entry.ok,
        exit_code = ?entry.exit_code,
        duration_ms = entry.duration.as_millis() as u64,
        error = ?entry.error,
        "主机工具：{}",
        entry.detail
    );
    let Some(log) = log else {
        return;
    };
    let row = termlet_core::audit::AuditEntry {
        host_name: entry.host.clone(),
        action: match entry.action {
            AuditAction::Panel => termlet_core::audit::AuditAction::Panel,
            AuditAction::PanelTerminal => termlet_core::audit::AuditAction::PanelTerminal,
        },
        detail: entry.detail.clone(),
        status: if entry.ok {
            termlet_core::audit::AuditStatus::Ok
        } else {
            termlet_core::audit::AuditStatus::Failed
        },
        exit_code: entry.exit_code.map(|c| c as i32),
        duration_ms: entry.duration.as_millis() as i64,
        ..Default::default()
    };
    std::thread::spawn(move || {
        if let Err(e) = log.add(row) {
            tracing::warn!("写入审计日志失败：{e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_use_the_executed_command_and_mark_timeouts() {
        let done = Ok(ActionResult {
            ok: true,
            exit_code: 0,
            output: String::new(),
            sudo: true,
            command: "sudo -n kill -TERM 42".into(),
        });
        let entry = action_entry(
            "web",
            "kill -TERM 42",
            "sleep (termlet)",
            &done,
            Duration::ZERO,
        );
        assert_eq!(entry.detail, "sudo -n kill -TERM 42  # sleep (termlet)");
        assert!(entry.ok);
        assert_eq!(entry.exit_code, Some(0));

        let timeout = Err(ToolsError::Exec(ExecError::Timeout));
        let entry = action_entry("web", "kill -TERM 42", "sleep", &timeout, Duration::ZERO);
        assert!(entry.ok, "超时记为已执行");
        assert!(
            entry
                .detail
                .starts_with("kill -TERM 42  # sleep  # 结果未知")
        );

        let failed = Err(ToolsError::ProcessReplaced);
        let entry = action_entry("web", "kill -TERM 42", "", &failed, Duration::ZERO);
        assert!(!entry.ok);
        assert_eq!(entry.detail, "kill -TERM 42");

        let entry = terminal_entry("web", "su -c 'kill -TERM 1'", "sshd (root)");
        assert_eq!(entry.action, AuditAction::PanelTerminal);
        assert_eq!(entry.detail, "su -c 'kill -TERM 1'  # sshd (root)");
    }
}
