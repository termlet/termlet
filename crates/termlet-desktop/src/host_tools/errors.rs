//! 把主机工具的错误说成用户能理解的话。执行超时单独说明（写操作可能已经执行），核心给出的原因保留。

use gpui_kit::SharedString;
use termlet_core::hosttools::{ExecError, SudoState, ToolsError};

use crate::t;

pub fn describe(error: &ToolsError) -> SharedString {
    match error {
        ToolsError::Unsupported => t!("tools.error.unsupported"),
        ToolsError::Unreadable { exit_code, stderr } => {
            let detail = stderr.trim();
            match exit_code {
                Some(code) => t!("tools.error.unreadable_code", code = code, detail = detail),
                None => t!("tools.error.unreadable", detail = detail),
            }
        }
        ToolsError::NoSystemd => t!("tools.unavailable.no_systemd"),
        ToolsError::NoDocker => t!("tools.unavailable.no_docker"),
        ToolsError::DockerDown | ToolsError::DockerDesktopDown => {
            t!("tools.unavailable.docker_down")
        }
        ToolsError::DockerFailed(detail) => t!("tools.error.docker_failed", detail = detail),
        ToolsError::LogsFailed(detail) => t!("tools.error.logs_failed", detail = detail),
        ToolsError::NeedRoot { sudo, .. } => sudo_text(*sudo),
        ToolsError::NoProcess => t!("tools.error.no_process"),
        ToolsError::ProcessReplaced => t!("tools.error.process_replaced"),
        ToolsError::Invalid(detail) => t!("tools.error.invalid", detail = detail),
        ToolsError::Exec(error) => describe_exec(error),
    }
}

fn describe_exec(error: &ExecError) -> SharedString {
    match error {
        ExecError::SingleChannel => t!("tools.unavailable_hint.single_channel"),
        ExecError::Disconnected => t!("tools.error.disconnected"),
        ExecError::Timeout => t!("tools.error.timeout"),
        ExecError::Channel(detail) => t!("tools.error.channel", detail = detail),
        ExecError::Wsl(error) => crate::wsl_errors::describe(error),
    }
}

/// 需要 root 时 sudo 的情况。
pub fn sudo_text(sudo: SudoState) -> SharedString {
    match sudo {
        SudoState::Password => t!("tools.sudo.password"),
        SudoState::Denied => t!("tools.sudo.denied"),
        SudoState::NotInstalled => t!("tools.sudo.none"),
    }
}
