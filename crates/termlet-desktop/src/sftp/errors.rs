//! 把 SFTP 的错误说成用户能理解的话。

use gpui_kit::SharedString;
use termlet_core::sftp::{NoSftp, Rejection, SftpError, Side, SudoError};

use crate::{ssh_errors, t};

pub fn describe(error: &SftpError) -> SharedString {
    let local = |side: &Side| *side == Side::Local;
    match error {
        SftpError::NotFound { side, path } if local(side) => {
            t!("sftp.error.local_not_found", path = path)
        }
        SftpError::NotFound { path, .. } => t!("sftp.error.not_found", path = path),
        SftpError::PermissionDenied { side, path } if local(side) => {
            t!("sftp.error.local_permission", path = path)
        }
        SftpError::PermissionDenied { path, .. } => t!("sftp.error.permission", path = path),
        SftpError::AlreadyExists { side, path } if local(side) => {
            t!("sftp.error.local_exists", path = path)
        }
        SftpError::AlreadyExists { path, .. } => t!("sftp.error.exists", path = path),
        SftpError::NoSpace { side, path } if local(side) => {
            t!("sftp.error.local_no_space", path = path)
        }
        SftpError::NoSpace { path, .. } => t!("sftp.error.no_space", path = path),
        SftpError::Busy { path, .. } => t!("sftp.error.busy", path = path),
        SftpError::Disconnected => t!("sftp.error.disconnected"),
        SftpError::Unavailable(reason) => t!("sftp.error.unavailable", reason = reason),
        SftpError::NoSftp(reason) => match reason {
            NoSftp::NoScp => t!("sftp.error.no_sftp_no_scp"),
            NoSftp::ExecRefused => t!("sftp.error.no_sftp_exec_refused"),
            NoSftp::NoShell => t!("sftp.error.no_sftp_no_shell"),
            NoSftp::SingleChannel => t!("sftp.error.no_sftp_single_channel"),
        },
        SftpError::SingleChannel => t!("sftp.error.single_channel"),
        SftpError::Connect(error) => ssh_errors::describe(error),
        SftpError::Sudo(error) => describe_sudo(error),
        SftpError::TooLarge { .. } => t!("sftp.error.too_large"),
        SftpError::NotText { .. } => t!("sftp.error.not_text"),
        SftpError::Changed { deleted: true, .. } => t!("sftp.error.deleted"),
        SftpError::Changed { .. } => t!("sftp.error.changed"),
        SftpError::ReplaceFailed {
            path, backup, kept, ..
        } => match kept {
            Some(kept) => t!(
                "sftp.error.replace_failed_kept",
                path = path,
                backup = backup,
                kept = kept
            ),
            None => t!("sftp.error.replace_failed", path = path, backup = backup),
        },
        SftpError::Rejected(rejection) => describe_rejection(*rejection),
        SftpError::Failed {
            side,
            path,
            message,
        } if local(side) => {
            t!("sftp.error.local_failed", path = path, message = message)
        }
        SftpError::Failed { path, message, .. } => {
            t!("sftp.error.failed", path = path, message = message)
        }
        SftpError::Canceled => t!("sftp.error.canceled"),
        SftpError::SessionClosed => t!("sftp.error.session_closed"),
        SftpError::TransferNotFound => t!("sftp.error.transfer_not_found"),
    }
}

/// 以 root 身份打开（sudo）失败的原因。
fn describe_sudo(error: &SudoError) -> SharedString {
    match error {
        SudoError::NotInstalled => t!("sftp.sudo.error.not_installed"),
        SudoError::ServerMissing => t!("sftp.sudo.error.server_missing"),
        SudoError::PasswordRequired => t!("sftp.sudo.error.password_required"),
        SudoError::WrongPassword => t!("sftp.sudo.error.wrong_password"),
        SudoError::NotAllowed => t!("sftp.sudo.error.not_allowed"),
        SudoError::RequiresTty => t!("sftp.sudo.error.requires_tty"),
        SudoError::ExecRefused => t!("sftp.sudo.error.exec_refused"),
        SudoError::SingleChannel => t!("sftp.sudo.error.single_channel"),
        SudoError::Failed(message) => t!("sftp.sudo.error.failed", message = message),
    }
}

fn describe_rejection(rejection: Rejection) -> SharedString {
    match rejection {
        Rejection::RemoveRootOrHome => t!("sftp.error.remove_root"),
        Rejection::InvalidMode => t!("sftp.error.invalid_mode"),
        Rejection::NoPaths => t!("sftp.error.no_paths"),
        Rejection::NotAbsolute | Rejection::DevicePath => t!("sftp.error.invalid_path"),
        Rejection::InvalidName => t!("sftp.error.invalid_name"),
        Rejection::NotRegularFile => t!("sftp.error.not_regular_file"),
        Rejection::NotDirectory => t!("sftp.error.not_directory"),
        Rejection::NotResumable => t!("sftp.error.not_resumable"),
        Rejection::OtherHost => t!("sftp.error.other_host"),
        Rejection::NoUniqueName => t!("sftp.error.no_unique_name"),
        Rejection::Unencodable => t!("sftp.error.unencodable"),
        Rejection::UnencodableName => t!("sftp.error.unencodable_name"),
        Rejection::TrashUnsupported => t!("sftp.error.trash_unsupported"),
        Rejection::NotRecyclable => t!("sftp.error.not_recyclable"),
        Rejection::ScpUnsupported => t!("sftp.error.scp_unsupported"),
    }
}
