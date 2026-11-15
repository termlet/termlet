//! 把 SSH 连接的错误说成用户能理解的话，并给出下一步。

use gpui_kit::SharedString;
use termlet_core::ssh::SshError;
use termlet_core::ssh::proxy_command::CommandIssue;
use termlet_core::sshcert::{CertProblem, CertReadIssue};

use crate::t;

pub fn describe(error: &SshError) -> SharedString {
    match error {
        SshError::Dns { host, .. } => t!("ssh.error.dns", host = host),
        SshError::Refused { host, port } => t!("ssh.error.refused", host = host, port = port),
        SshError::Timeout { host, port } => t!("ssh.error.timeout", host = host, port = port),
        SshError::Network { host, port, reason } => {
            t!(
                "ssh.error.network",
                host = host,
                port = port,
                reason = reason
            )
        }
        SshError::Proxy(reason) => t!("ssh.error.proxy", reason = reason),
        SshError::ProxyCommand(reason) => t!("ssh.error.proxy_command", reason = reason),
        SshError::ProxyCommandInvalid(issue) => match issue {
            CommandIssue::Empty => t!("ssh.error.proxy_command_empty"),
            CommandIssue::UnknownToken('\0') => t!("ssh.error.proxy_command_trailing_percent"),
            CommandIssue::UnknownToken(token) => {
                t!(
                    "ssh.error.proxy_command_unknown_token",
                    token = format!("%{token}")
                )
            }
            CommandIssue::UnsafeValue(token) => {
                t!(
                    "ssh.error.proxy_command_unsafe_value",
                    token = format!("%{token}")
                )
            }
        },
        SshError::ProxyCommandExited { code, stderr } => {
            let exit = match code {
                Some(code) => t!("ssh.error.proxy_command_exit_code", code = code),
                None => t!("ssh.error.proxy_command_signaled"),
            };
            if stderr.is_empty() {
                t!("ssh.error.proxy_command_exited", exit = exit)
            } else {
                t!(
                    "ssh.error.proxy_command_exited_with_output",
                    exit = exit,
                    output = stderr
                )
            }
        }
        SshError::ProxyCommandUnconfirmed => t!("ssh.error.proxy_command_unconfirmed"),
        SshError::HostKeyRejected { .. } => t!("ssh.error.host_key_rejected"),
        SshError::HostKeyChanged {
            host,
            algorithm,
            fingerprint,
            known_fingerprint,
            ..
        } => t!(
            "ssh.error.host_key_changed",
            host = host,
            algorithm = algorithm,
            fingerprint = fingerprint,
            known = known_fingerprint
        ),
        SshError::NoCommonAlgorithm(detail) => t!("ssh.error.no_common_algorithm", detail = detail),
        SshError::AuthFailed { methods } => t!("ssh.error.auth_failed", methods = methods),
        SshError::Certificate(problem) => describe_certificate(problem),
        SshError::AuthCancelled => t!("ssh.error.auth_cancelled"),
        SshError::InteractionRequired => t!("ssh.error.interaction_required"),
        SshError::Key(reason) => t!("ssh.error.key", reason = reason),
        SshError::Jump { jump, reason } => t!("ssh.error.jump", jump = jump, reason = reason),
        SshError::JumpTarget { host, port, reason } => {
            t!(
                "ssh.error.jump_target",
                host = host,
                port = port,
                reason = reason
            )
        }
        SshError::Channel(reason) => t!("ssh.error.channel", reason = reason),
        SshError::Disconnected => t!("ssh.error.disconnected"),
        SshError::Vault(reason) => t!("ssh.error.vault", reason = reason),
        SshError::Config(reason) => t!("ssh.error.config", reason = reason),
        SshError::Protocol(reason) => t!("ssh.error.protocol", reason = reason),
    }
}

/// 证书没能用来登录的说明。`certificate` 为空的是密钥库中附带的证书。
fn describe_certificate(problem: &CertProblem) -> SharedString {
    let library = |certificate: &str| certificate.is_empty();
    match problem {
        CertProblem::Expired { at } => t!("ssh.error.cert_expired", time = cert_time(*at)),
        CertProblem::NotYetValid { from } => {
            t!("ssh.error.cert_not_yet_valid", time = cert_time(*from))
        }
        CertProblem::Principal {
            username,
            principals,
        } => t!(
            "ssh.error.cert_principal",
            user = username,
            principals = principals.join(", ")
        ),
        CertProblem::Rejected { ca_fingerprint } => {
            t!("ssh.error.cert_rejected", ca = ca_fingerprint)
        }
        CertProblem::KeyMismatch { certificate } if library(certificate) => {
            t!("ssh.error.cert_key_mismatch_library")
        }
        CertProblem::KeyMismatch { certificate } => {
            t!("ssh.error.cert_key_mismatch", file = certificate)
        }
        CertProblem::HostCertificate { certificate } if library(certificate) => {
            t!("ssh.error.cert_host_library")
        }
        CertProblem::HostCertificate { certificate } => {
            t!("ssh.error.cert_host", file = certificate)
        }
        CertProblem::Unreadable {
            certificate,
            reason: CertReadIssue::NotFound,
        } => t!("ssh.error.cert_missing", file = certificate),
        CertProblem::Unreadable {
            certificate,
            reason,
        } if library(certificate) => t!(
            "ssh.error.cert_unreadable_library",
            reason = read_issue(reason)
        ),
        CertProblem::Unreadable {
            certificate,
            reason,
        } => t!(
            "ssh.error.cert_unreadable",
            file = certificate,
            reason = read_issue(reason)
        ),
    }
}

fn read_issue(issue: &CertReadIssue) -> SharedString {
    match issue {
        CertReadIssue::NotFound => t!("keys.error.file_not_found_short"),
        CertReadIssue::Io(reason) => reason.clone().into(),
        CertReadIssue::Invalid(error) => crate::key_dialogs::errors::describe_cert(error),
    }
}

/// 证书的生效、失效时间（Unix 秒）：今年的写月日与时分，往年的写年月日。
pub fn cert_time(secs: i64) -> String {
    termlet_core::time::format_compact(
        secs.saturating_mul(1000),
        termlet_core::time::now_ms(),
        crate::sftp::file_list::date_style(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // 证书的问题说清楚：过期写出时间，用户名写出允许的列表，密钥库中的证书不写文件名
    #[test]
    fn certificate_problems() {
        let at = termlet_core::sshcert::now_secs() - 60;
        let text = describe(&SshError::Certificate(CertProblem::Expired { at }));
        assert!(
            text.contains(&cert_time(at)) && text.contains("过期"),
            "{text}"
        );
        let text = describe(&SshError::Certificate(CertProblem::Principal {
            username: "root".into(),
            principals: vec!["deploy".into(), "ops".into()],
        }));
        assert!(
            text.contains("root") && text.contains("deploy, ops"),
            "{text}"
        );
        let text = describe(&SshError::Certificate(CertProblem::Rejected {
            ca_fingerprint: "SHA256:ca".into(),
        }));
        assert!(text.contains("SHA256:ca") && text.contains("CA"), "{text}");
        let text = describe(&SshError::Certificate(CertProblem::KeyMismatch {
            certificate: String::new(),
        }));
        assert!(text.contains("密钥库"), "{text}");
        let text = describe(&SshError::Certificate(CertProblem::Unreadable {
            certificate: "id-cert.pub".into(),
            reason: CertReadIssue::Invalid(termlet_core::sshcert::CertError::NotCertificate),
        }));
        assert!(
            text.contains("id-cert.pub") && text.contains("不是证书"),
            "{text}"
        );
    }
}
