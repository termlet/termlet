//! 命令行请求的错误：错误码（决定退出码，AI 据此判断）与按命令行工具的语言写的说明。
//!
//! 说明只补充错误码之外的信息（命中的规则、路径、服务器的说明）；错误码本身的说明由命令行工具按语言写。
//! 核心其他模块的错误说明（`SshError`、`SftpError` 的 `Display`）是中文，原样带上。

use termlet_proto::{ErrorBody, ErrorCode, Lang};

use crate::audit::AuditStatus;
use crate::sftp::{NoSftp, Rejection, SftpError};
use crate::ssh::SshError;
use crate::termrun::TerminalError;
use crate::vault::VaultStatus;

/// 一次请求失败的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CliError {
    pub code: ErrorCode,
    pub message: String,
}

impl CliError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn code(code: ErrorCode) -> Self {
        Self::new(code, String::new())
    }

    pub fn body(&self) -> ErrorBody {
        ErrorBody::new(self.code, self.message.clone())
    }

    /// 写进审计日志的结果：未能执行。
    pub fn failed(self) -> Failure {
        Failure {
            status: AuditStatus::Failed,
            error: self,
        }
    }
}

/// 失败与它在审计日志中的结果（未能执行、被拦截、被拒绝）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Failure {
    pub status: AuditStatus,
    pub error: CliError,
}

impl From<CliError> for Failure {
    fn from(error: CliError) -> Self {
        error.failed()
    }
}

/// 按语言取文字。
pub(super) fn tr(lang: Lang, zh: &str, en: &str) -> String {
    lang.pick(zh, en).to_string()
}

/// 保险库没有解锁时的错误。
pub(super) fn vault_error(status: VaultStatus) -> Option<CliError> {
    match status {
        VaultStatus::Unlocked => None,
        VaultStatus::NeedsPassword => Some(CliError::code(ErrorCode::VaultLocked)),
        VaultStatus::NeedsNewPassword { .. } | VaultStatus::ChooseKeyStore => {
            Some(CliError::code(ErrorCode::VaultNeedsSetup))
        }
        VaultStatus::KeyMissing | VaultStatus::KeyMismatch => {
            Some(CliError::code(ErrorCode::VaultKeyLost))
        }
        VaultStatus::KeyringError(e) => Some(CliError::new(ErrorCode::VaultLocked, e)),
    }
}

/// 连接时拒绝回答的询问：第一个被拒绝的问题决定错误码（命令行不能询问）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Refused {
    pub code: ErrorCode,
    /// 哪台主机、哪个用户或哪把私钥
    pub subject: String,
}

/// 连接失败的错误码。`refused` 是连接过程中被拒绝的询问。
pub(super) fn from_ssh(error: &SshError, refused: Option<&Refused>) -> CliError {
    let with_refused = |fallback: ErrorCode, message: String| match refused {
        Some(r) => CliError::new(r.code, r.subject.clone()),
        None => CliError::new(fallback, message),
    };
    match error {
        SshError::HostKeyRejected { host } => {
            with_refused(ErrorCode::HostKeyRejected, host.clone())
        }
        SshError::AuthCancelled => with_refused(ErrorCode::AuthFailed, String::new()),
        SshError::ProxyCommandUnconfirmed => {
            with_refused(ErrorCode::ProxyCommandUnconfirmed, String::new())
        }
        // 跳板机没能登录：被拒绝的询问更准确（例如跳板机没有保存密码）
        SshError::Jump { .. } if refused.is_some() => {
            with_refused(ErrorCode::ConnectFailed, String::new())
        }
        SshError::HostKeyChanged {
            host,
            port,
            fingerprint,
            known_fingerprint,
            ..
        } => CliError::new(
            ErrorCode::HostKeyMismatch,
            format!("{host}:{port} {fingerprint}（记录的是 {known_fingerprint}）"),
        ),
        SshError::AuthFailed { .. } | SshError::Certificate(_) => {
            CliError::new(ErrorCode::AuthFailed, error.to_string())
        }
        SshError::InteractionRequired => CliError::code(ErrorCode::AuthInteractionRequired),
        SshError::NoCommonAlgorithm(_) => {
            CliError::new(ErrorCode::AlgorithmMismatch, error.to_string())
        }
        SshError::Key(message) if message.contains("不支持") => {
            CliError::new(ErrorCode::KeyUnsupported, message.clone())
        }
        SshError::Key(message) => CliError::new(ErrorCode::KeyInvalid, message.clone()),
        SshError::Vault(message) => CliError::new(ErrorCode::VaultLocked, message.clone()),
        SshError::Config(message) => CliError::new(ErrorCode::BadRequest, message.clone()),
        other => CliError::new(ErrorCode::ConnectFailed, other.to_string()),
    }
}

/// 传输失败的错误码。
pub(super) fn from_sftp(error: &SftpError, refused: Option<&Refused>, lang: Lang) -> CliError {
    match error {
        SftpError::NotFound { path, .. } => CliError::new(ErrorCode::PathNotFound, path.clone()),
        SftpError::PermissionDenied { path, .. } => {
            CliError::new(ErrorCode::PermissionDenied, path.clone())
        }
        SftpError::AlreadyExists { path, .. } => CliError::new(ErrorCode::PathExists, path.clone()),
        SftpError::Unavailable(message) => {
            CliError::new(ErrorCode::SftpUnavailable, message.clone())
        }
        // 服务器没有 SFTP，也不能改用 SCP
        SftpError::NoSftp(reason) => {
            let (zh, en) = match reason {
                NoSftp::NoScp => (
                    "也不能改用 SCP：服务器上没有 scp 命令",
                    "SCP can't be used either: there is no scp command on the server",
                ),
                NoSftp::ExecRefused => (
                    "也不能改用 SCP：服务器不允许另开通道执行命令",
                    "SCP can't be used either: the server doesn't allow running commands on another channel",
                ),
                NoSftp::NoShell => (
                    "也不能改用 SCP：服务器上没有可用的 sh 或 ls",
                    "SCP can't be used either: there is no usable sh or ls on the server",
                ),
                NoSftp::SingleChannel => (
                    "也不能改用 SCP：这台主机设置了“只用一个通道”",
                    "SCP can't be used either: the host is set to use a single channel",
                ),
            };
            CliError::new(ErrorCode::SftpUnavailable, tr(lang, zh, en))
        }
        SftpError::Connect(e) => from_ssh(e, refused),
        SftpError::Rejected(Rejection::NotRegularFile) => CliError::new(
            ErrorCode::BadRequest,
            tr(
                lang,
                "不是普通文件：目前只支持传输单个文件，目录请先打包（如 tar czf dir.tar.gz dir）",
                "not a regular file: only single files can be transferred for now; archive directories first (e.g. tar czf dir.tar.gz dir)",
            ),
        ),
        SftpError::Rejected(rejection) => {
            CliError::new(ErrorCode::BadRequest, rejection.to_string())
        }
        SftpError::Canceled => CliError::code(ErrorCode::Canceled),
        other => CliError::new(ErrorCode::ConnectFailed, other.to_string()),
    }
}

/// 在终端中执行失败的错误码。
pub(super) fn from_terminal(error: &TerminalError, lang: Lang) -> CliError {
    match error {
        TerminalError::NotOpen => CliError::code(ErrorCode::TerminalNotOpen),
        TerminalError::Busy => CliError::code(ErrorCode::TerminalBusy),
        TerminalError::Closed => CliError::new(
            ErrorCode::ConnectFailed,
            tr(lang, "终端已关闭", "the terminal was closed"),
        ),
        TerminalError::TooLong => CliError::new(
            ErrorCode::BadRequest,
            tr(
                lang,
                "命令太长，不能在终端中执行（请改用脚本文件）",
                "the command is too long to run in the terminal (use a script file instead)",
            ),
        ),
        TerminalError::Canceled => CliError::code(ErrorCode::Canceled),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refused_prompts_decide_the_code() {
        let refused = Refused {
            code: ErrorCode::HostKeyUnknown,
            subject: "10.0.0.1:22".into(),
        };
        let error = from_ssh(
            &SshError::HostKeyRejected {
                host: "10.0.0.1".into(),
            },
            Some(&refused),
        );
        assert_eq!(error.code, ErrorCode::HostKeyUnknown);
        assert_eq!(
            from_ssh(&SshError::AuthCancelled, None).code,
            ErrorCode::AuthFailed
        );
        let jump = SshError::Jump {
            jump: "bastion".into(),
            reason: "已取消认证".into(),
        };
        assert_eq!(from_ssh(&jump, None).code, ErrorCode::ConnectFailed);
        let missing = Refused {
            code: ErrorCode::CredentialMissing,
            subject: "ops@bastion".into(),
        };
        assert_eq!(
            from_ssh(&jump, Some(&missing)).code,
            ErrorCode::CredentialMissing
        );
        assert_eq!(
            from_ssh(&SshError::NoCommonAlgorithm("x".into()), None).code,
            ErrorCode::AlgorithmMismatch
        );
        // 代理命令没有在应用中确认过
        let unconfirmed = Refused {
            code: ErrorCode::ProxyCommandUnconfirmed,
            subject: "web".into(),
        };
        let error = from_ssh(&SshError::ProxyCommandUnconfirmed, Some(&unconfirmed));
        assert_eq!(
            (error.code, error.message.as_str()),
            (ErrorCode::ProxyCommandUnconfirmed, "web")
        );
        assert_eq!(
            from_ssh(&SshError::ProxyCommandUnconfirmed, None).code,
            ErrorCode::ProxyCommandUnconfirmed
        );
        assert_eq!(
            vault_error(VaultStatus::NeedsPassword).unwrap().code,
            ErrorCode::VaultLocked
        );
        assert_eq!(
            vault_error(VaultStatus::KeyMismatch).unwrap().code,
            ErrorCode::VaultKeyLost
        );
        assert!(vault_error(VaultStatus::Unlocked).is_none());
    }

    // 服务器没有 SFTP、也不能改用 SCP：错误码与 SFTP 打不开时相同，说明写明为什么不能改用
    #[test]
    fn no_sftp_explains_why_scp_is_not_used() {
        let error = from_sftp(&SftpError::NoSftp(NoSftp::NoScp), None, Lang::Zh);
        assert_eq!(error.code, ErrorCode::SftpUnavailable);
        assert!(error.message.contains("scp"), "{}", error.message);
        let error = from_sftp(&SftpError::NoSftp(NoSftp::SingleChannel), None, Lang::En);
        assert_eq!(error.code, ErrorCode::SftpUnavailable);
        assert!(
            error.message.contains("single channel"),
            "{}",
            error.message
        );
        for reason in [NoSftp::ExecRefused, NoSftp::NoShell] {
            let error = from_sftp(&SftpError::NoSftp(reason), None, Lang::En);
            assert!(error.message.starts_with("SCP can't be used either"));
        }
    }
}
