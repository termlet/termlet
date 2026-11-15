//! 错误码与退出码。
//!
//! 错误码是稳定的英文字符串（JSON 输出中的 `error`，AI 与脚本据此判断），发布后不能改；退出码 0–7 同样固定。
//! 线路上的错误码是字符串（[`crate::ErrorBody::code`]）：新版本的 Termlet 返回本版本不认识的错误码时，
//! 命令行工具原样输出，退出码按"连接或执行失败"。

/// 错误码。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// 请求不合法（参数缺失、本地路径不是绝对路径、请求过大等）
    BadRequest,
    /// 要找的记录不存在（审计记录等）
    NotFound,
    HostNotFound,
    /// 主机没有允许命令行访问
    HostNotAllowed,
    /// 连接或执行失败（网络、服务器拒绝执行命令等）
    ConnectFailed,
    /// 与服务器没有共同的加密算法
    AlgorithmMismatch,
    /// `exec --terminal`：这台主机没有在 Termlet 中打开的终端
    TerminalNotOpen,
    /// `exec --terminal`：终端不在等待输入（正在运行程序、全屏程序或文件传输）
    TerminalBusy,
    /// 主机没有保存凭据（密码等）
    CredentialMissing,
    /// 还没有确认这台主机的指纹（命令行不能询问）
    HostKeyUnknown,
    /// 主机指纹与记录的不符
    HostKeyMismatch,
    HostKeyRejected,
    AuthFailed,
    /// 需要输入验证码等交互认证，命令行无法完成
    AuthInteractionRequired,
    /// 主机经代理命令连接，这条命令还没有在这台电脑的 Termlet 中确认过（命令行不能询问）
    ProxyCommandUnconfirmed,
    PathNotFound,
    PathExists,
    PermissionDenied,
    /// 服务器上不能使用 SFTP
    SftpUnavailable,
    /// 私钥已加密但没有保存私钥密码
    KeyPassphraseRequired,
    KeyWrongPassphrase,
    KeyUnsupported,
    KeyInvalid,
    /// 命中危险命令规则被拦截（设置为直接拦截，或没有窗口可以询问）
    CommandBlocked,
    /// 命中危险命令规则，没有获得批准（用户拒绝或等待超时）
    CommandDenied,
    /// 外部命令行已关闭
    CliDisabled,
    /// Termlet 没有运行（命令行工具不会自动启动它）
    AppNotRunning,
    /// Termlet 已锁定（应用锁）
    AppLocked,
    /// 保险库已锁定（需要输入主密码）
    VaultLocked,
    /// 系统钥匙串不可用，需要先设置主密码
    VaultNeedsSetup,
    /// 钥匙串中的主密钥丢失或不符，保存的凭据无法解密
    VaultKeyLost,
    /// 命令行工具与应用的协议版本不兼容
    ProtocolMismatch,
    /// 命令行工具在请求完成前退出（只出现在审计日志中）
    Canceled,
    Internal,
}

macro_rules! codes {
    ($($variant:ident => $text:literal),* $(,)?) => {
        impl ErrorCode {
            /// 全部错误码。
            pub const ALL: &'static [ErrorCode] = &[$(ErrorCode::$variant),*];

            /// 线路上与 JSON 输出中的写法。
            pub fn as_str(self) -> &'static str {
                match self {
                    $(ErrorCode::$variant => $text),*
                }
            }

            /// 解析错误码；不认识的（更新版本的 Termlet 加的）为 `None`。
            pub fn parse(text: &str) -> Option<Self> {
                match text {
                    $($text => Some(ErrorCode::$variant),)*
                    _ => None,
                }
            }
        }
    };
}

codes! {
    BadRequest => "bad_request",
    NotFound => "not_found",
    HostNotFound => "host_not_found",
    HostNotAllowed => "host_not_allowed",
    ConnectFailed => "connect_failed",
    AlgorithmMismatch => "algorithm_mismatch",
    TerminalNotOpen => "terminal_not_open",
    TerminalBusy => "terminal_busy",
    CredentialMissing => "credential_missing",
    HostKeyUnknown => "hostkey_unknown",
    HostKeyMismatch => "hostkey_mismatch",
    HostKeyRejected => "hostkey_rejected",
    AuthFailed => "auth_failed",
    AuthInteractionRequired => "auth_interaction_required",
    ProxyCommandUnconfirmed => "proxy_command_unconfirmed",
    PathNotFound => "path_not_found",
    PathExists => "path_exists",
    PermissionDenied => "permission_denied",
    SftpUnavailable => "sftp_unavailable",
    KeyPassphraseRequired => "key_passphrase_required",
    KeyWrongPassphrase => "key_wrong_passphrase",
    KeyUnsupported => "key_unsupported",
    KeyInvalid => "key_invalid",
    CommandBlocked => "command_blocked",
    CommandDenied => "command_denied",
    CliDisabled => "cli_disabled",
    AppNotRunning => "app_not_running",
    AppLocked => "app_locked",
    VaultLocked => "vault_locked",
    VaultNeedsSetup => "vault_needs_setup",
    VaultKeyLost => "vault_key_lost",
    ProtocolMismatch => "protocol_mismatch",
    Canceled => "canceled",
    Internal => "internal",
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 命令行工具的退出码。`exec` 成功时透传远程命令自己的退出码。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ExitCode {
    Success = 0,
    /// 用法错误
    Usage = 1,
    /// 主机不存在
    HostNotFound = 2,
    /// 主机没有允许命令行访问
    HostNotAllowed = 3,
    /// 命中危险命令规则被拦截，或没有获得批准
    Blocked = 4,
    /// 连接或执行失败
    Failed = 5,
    /// 凭据已锁定或 Termlet 已锁定
    Locked = 6,
    /// 外部命令行用不了：已关闭，或 Termlet 没有运行
    CliDisabled = 7,
}

impl ExitCode {
    /// 错误码对应的退出码；不认识的错误码按连接或执行失败。
    pub fn for_error(code: &str) -> ExitCode {
        match ErrorCode::parse(code) {
            Some(ErrorCode::HostNotFound) => ExitCode::HostNotFound,
            Some(ErrorCode::HostNotAllowed) => ExitCode::HostNotAllowed,
            Some(ErrorCode::CommandBlocked | ErrorCode::CommandDenied) => ExitCode::Blocked,
            Some(ErrorCode::BadRequest) => ExitCode::Usage,
            Some(
                ErrorCode::VaultLocked
                | ErrorCode::VaultNeedsSetup
                | ErrorCode::VaultKeyLost
                | ErrorCode::AppLocked,
            ) => ExitCode::Locked,
            Some(ErrorCode::CliDisabled | ErrorCode::AppNotRunning) => ExitCode::CliDisabled,
            _ => ExitCode::Failed,
        }
    }
}

impl From<ExitCode> for i32 {
    fn from(code: ExitCode) -> Self {
        code as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_for_errors() {
        let cases = [
            ("host_not_found", ExitCode::HostNotFound),
            ("host_not_allowed", ExitCode::HostNotAllowed),
            ("command_blocked", ExitCode::Blocked),
            ("command_denied", ExitCode::Blocked),
            ("app_locked", ExitCode::Locked),
            ("bad_request", ExitCode::Usage),
            ("vault_locked", ExitCode::Locked),
            ("vault_needs_setup", ExitCode::Locked),
            ("vault_key_lost", ExitCode::Locked),
            ("cli_disabled", ExitCode::CliDisabled),
            ("app_not_running", ExitCode::CliDisabled),
            ("connect_failed", ExitCode::Failed),
            ("hostkey_unknown", ExitCode::Failed),
            ("proxy_command_unconfirmed", ExitCode::Failed),
            ("auth_failed", ExitCode::Failed),
            ("path_not_found", ExitCode::Failed),
            ("internal", ExitCode::Failed),
            ("something_new_and_odd", ExitCode::Failed),
        ];
        for (code, want) in cases {
            assert_eq!(ExitCode::for_error(code), want, "{code}");
        }
        assert_eq!(
            i32::from(ExitCode::Blocked),
            4,
            "命令被拦截的退出码必须是 4"
        );
        assert_eq!(i32::from(ExitCode::CliDisabled), 7);
    }

    #[test]
    fn codes_round_trip() {
        for &code in ErrorCode::ALL {
            assert_eq!(ErrorCode::parse(code.as_str()), Some(code));
        }
        assert_eq!(ErrorCode::parse("nope"), None);
        // 写法固定（AI 与脚本依赖它们）
        assert_eq!(ErrorCode::HostKeyUnknown.as_str(), "hostkey_unknown");
        assert_eq!(ErrorCode::SftpUnavailable.as_str(), "sftp_unavailable");
        assert_eq!(ErrorCode::CliDisabled.as_str(), "cli_disabled");
    }
}
