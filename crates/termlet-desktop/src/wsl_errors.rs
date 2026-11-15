//! 把 WSL 的错误说成用户能理解的话。

use gpui_kit::SharedString;
use termlet_core::wsl::WslError;

use crate::{local_shell, t};

pub fn describe(error: &WslError) -> SharedString {
    match error {
        WslError::Shell(error) => local_shell::describe_error(error),
        WslError::DistroNotFound { distro } => t!("wsl.error.distro_not_found", distro = distro),
        WslError::Spawn(reason) => t!("wsl.error.spawn", reason = reason),
        WslError::Failed(reason) => t!("wsl.error.failed", reason = reason),
        WslError::Timeout(seconds) => t!("wsl.error.timeout", seconds = seconds),
        WslError::SftpServerMissing { distro, install } => match install {
            Some(command) => t!(
                "wsl.error.sftp_missing_install",
                distro = distro,
                command = command
            ),
            None => t!("wsl.error.sftp_missing", distro = distro),
        },
        WslError::Sftp(error) => crate::sftp::errors::describe(error),
    }
}
