//! 服务交给界面的事件：批准危险命令、在终端中执行、启动参数、审计记录。

use std::future::Future;
use std::time::Instant;

use termlet_proto::LaunchFile;
use tokio::sync::{oneshot, watch};

use crate::audit::AuditEntry;
use crate::guard::Rule;
use crate::id::HostId;
use crate::termrun::{TerminalError, TerminalOutput};

/// 服务交给界面的事件。
#[derive(Debug)]
pub enum CliEvent {
    /// 命令命中危险命令规则，请用户批准
    Approval(ApprovalRequest),
    /// `exec --terminal`：在这台主机已打开的终端中执行
    RunInTerminal(TerminalRequest),
    /// 已有实例在运行时，第二个 Termlet 进程交来的启动参数与其中会话文件的内容（较早的版本不带内容）
    Activate {
        args: Vec<String>,
        files: Vec<LaunchFile>,
    },
    /// 版本不同的新 Termlet 进程请本实例退出：没有打开的窗口、没有别的命令行请求时回答 `true` 并退出，
    /// 否则回答 `false`（丢弃不回答也按 `false`）
    Quit(oneshot::Sender<bool>),
    /// 写入了一条审计记录
    Audited(AuditEntry),
}

/// 等待界面批准的一条命令。回答一次后失效；丢弃而不回答按"没有窗口可以询问"处理（拦截）。
pub struct ApprovalRequest {
    pub host_id: HostId,
    pub host_name: String,
    /// 要执行的命令（可能是多行脚本）
    pub command: String,
    /// 调用方祖先进程名，由近到远，以 ` < ` 连接（`bash < node < Code.exe`）
    pub caller: String,
    /// 命中的规则（说明有中英文两种）
    pub rule: Rule,
    /// 超过这个时间按"等待批准超时"拒绝
    pub deadline: Instant,
    pub(super) reply: Option<oneshot::Sender<bool>>,
    pub(super) done: watch::Receiver<bool>,
}

impl ApprovalRequest {
    /// 用户的选择：批准执行或拒绝。服务已不再等待（超时等）时什么也不做。
    pub fn answer(mut self, approved: bool) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(approved);
        }
    }

    /// 服务不再等待这次批准时完成（已回答、超时、命令行工具退出、应用被锁定）：界面据此关闭对话框。
    pub fn done(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut done = self.done.clone();
        async move {
            let _ = done.wait_for(|d| *d).await;
        }
    }

    /// 服务已不再等待这次批准（测试中检查对话框该不该关闭；界面用 [`ApprovalRequest::done`]）。
    #[cfg(test)]
    pub(crate) fn is_done(&self) -> bool {
        *self.done.borrow() || self.done.has_changed().is_err()
    }
}

impl std::fmt::Debug for ApprovalRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovalRequest")
            .field("host_id", &self.host_id)
            .field("host_name", &self.host_name)
            .field("rule", &self.rule.id())
            .finish_non_exhaustive()
    }
}

/// 在已打开的终端中执行的一条命令（已通过主机授权与危险命令检查）。
pub struct TerminalRequest {
    pub host_id: HostId,
    pub host_name: String,
    pub command: String,
    /// 输出的上限（字节）
    pub limit: usize,
    pub(super) reply: oneshot::Sender<Result<TerminalOutput, TerminalError>>,
    pub(super) canceled: watch::Receiver<bool>,
}

impl TerminalRequest {
    /// 交回结果。
    pub fn respond(self, result: Result<TerminalOutput, TerminalError>) {
        let _ = self.reply.send(result);
    }

    /// 命令行工具不再等待时完成（交给 `TermRun::execute` 的 `canceled`：向终端发送 Ctrl+C）。
    pub fn canceled(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut canceled = self.canceled.clone();
        async move {
            let _ = canceled.wait_for(|c| *c).await;
        }
    }
}

impl std::fmt::Debug for TerminalRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalRequest")
            .field("host_id", &self.host_id)
            .field("limit", &self.limit)
            .finish_non_exhaustive()
    }
}
