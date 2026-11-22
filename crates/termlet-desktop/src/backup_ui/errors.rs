//! 把备份、导入、导出的错误与导入时的提示说成用户能理解的话。

use std::io::ErrorKind;

use gpui_kit::SharedString;
use termlet_core::Invalid;
use termlet_core::backup::BackupError;
use termlet_core::import::{FailureReason, FailureSubject, Hint, ImportError, Source};

use crate::{store_errors, t};

/// 来源在提示中的名称。
pub(super) fn source_name(source: Source) -> &'static str {
    match source {
        Source::SshConfig => "ssh_config",
        Source::Xshell => "Xshell",
        Source::FinalShell => "FinalShell",
        Source::MobaXterm => "MobaXterm",
        Source::Tabby => "Tabby",
        Source::Termora => "Termora",
        Source::WindTerm => "WindTerm",
        Source::Electerm => "Electerm",
        Source::Netcatty => "Netcatty",
        Source::NyaTerm => "NyaTerm",
    }
}

pub(super) fn describe_backup(error: &BackupError) -> SharedString {
    match error {
        BackupError::PasswordTooShort { min } => t!("backup_ui.password.too_short", min = min),
        BackupError::PasswordRequired => t!("backup_ui.error.password_required"),
        BackupError::WrongPassword => t!("backup_ui.error.wrong_password"),
        BackupError::Invalid => t!("backup_ui.error.invalid"),
        BackupError::TooNew { found, supported } => t!(
            "backup_ui.error.too_new",
            found = found,
            supported = supported
        ),
        BackupError::IsHostsFile => t!("backup_ui.error.is_hosts_file"),
        BackupError::IsFullBackup => t!("backup_ui.error.is_full_backup"),
        BackupError::ForeignSnapshot => t!("backup_ui.error.foreign_snapshot"),
        BackupError::SnapshotNotFound => t!("backup_ui.error.snapshot_not_found"),
        BackupError::InvalidSnapshotName => t!("backup_ui.error.invalid_snapshot_name"),
        BackupError::NothingSelected => t!("backup_ui.error.nothing_selected"),
        BackupError::HostNotFound => t!("backup_ui.error.host_not_found"),
        BackupError::VaultLocked => t!("backup_ui.error.vault_locked"),
        BackupError::VaultUnavailable(reason) => {
            t!("backup_ui.error.vault_unavailable", reason = reason)
        }
        BackupError::FileNotFound { path } => {
            t!("backup_ui.error.file_not_found", path = path.display())
        }
        BackupError::PermissionDenied { path } => {
            t!("backup_ui.error.permission_denied", path = path.display())
        }
        BackupError::ReadFailed { path, reason } => t!(
            "backup_ui.error.read_failed",
            path = path.display(),
            reason = reason
        ),
        BackupError::WriteFailed { path, reason } => t!(
            "backup_ui.error.write_failed",
            path = path.display(),
            reason = reason
        ),
        BackupError::NotAbsolute => t!("backup_ui.error.not_absolute"),
        BackupError::IsDirectory { path } => {
            t!("backup_ui.error.is_directory", path = path.display())
        }
        BackupError::TooLarge => t!("backup_ui.error.too_large"),
        BackupError::FolderMissing { path } => {
            t!("backup_ui.error.folder_missing", path = path.display())
        }
        BackupError::InvalidSchedule => t!("backup_ui.error.invalid_schedule"),
        BackupError::NotScheduled => t!("backup_ui.error.not_scheduled"),
        BackupError::SchedulePasswordLost => t!("backup_ui.error.schedule_password_lost"),
        BackupError::Store(error) => t!("backup_ui.error.store", error = error),
    }
}

pub(super) fn describe_import(error: &ImportError) -> SharedString {
    match error {
        ImportError::Io { path, source } => match source.kind() {
            ErrorKind::NotFound => t!("backup_ui.import_error.not_found", path = path.display()),
            ErrorKind::PermissionDenied => {
                t!("backup_ui.import_error.permission", path = path.display())
            }
            _ => t!(
                "backup_ui.import_error.io",
                path = path.display(),
                error = source
            ),
        },
        ImportError::TooLarge { path, limit_mb } => t!(
            "backup_ui.import_error.too_large",
            path = path.display(),
            limit = limit_mb
        ),
        ImportError::Syntax(error) => t!("backup_ui.import_error.syntax", error = error),
        ImportError::TabbyEncrypted => t!("backup_ui.import_error.tabby_encrypted"),
        ImportError::Registry(error) => t!("backup_ui.import_error.registry", error = error),
        ImportError::TermoraV1 => t!("backup_ui.import_error.termora_v1"),
        ImportError::TermoraNotFound => t!("backup_ui.import_error.termora_not_found"),
        ImportError::NotTermora(error) => t!("backup_ui.import_error.not_termora", error = error),
        ImportError::TermoraKeyMissing => t!("backup_ui.import_error.termora_key_missing"),
        ImportError::TermoraDecrypt => t!("backup_ui.import_error.termora_decrypt"),
        ImportError::WindTermNotFound => t!("backup_ui.import_error.windterm_not_found"),
        ImportError::WindTermPasswordRequired => {
            t!("backup_ui.import_error.windterm_password_required")
        }
        ImportError::WindTermWrongPassword => t!("backup_ui.import_error.windterm_wrong_password"),
        ImportError::NotElecterm(error) => {
            t!("backup_ui.import_error.not_electerm", error = error)
        }
        ImportError::ElectermEncrypted => t!("backup_ui.import_error.electerm_encrypted"),
        ImportError::NotNetcatty => t!("backup_ui.import_error.not_netcatty"),
        ImportError::NotNyaTerm(error) => t!("backup_ui.import_error.not_nyaterm", error = error),
    }
}

/// 没能导入的一项：是什么、为什么。
pub(super) fn describe_failure(subject: &FailureSubject, reason: &FailureReason) -> SharedString {
    let reason = describe_reason(reason);
    match subject {
        FailureSubject::Host { name, .. } => {
            t!("backup_ui.failure.host", name = name, reason = reason)
        }
        FailureSubject::Secrets { host } => {
            t!("backup_ui.failure.secrets", host = host, reason = reason)
        }
        FailureSubject::Key { name } => t!("backup_ui.failure.key", name = name, reason = reason),
        FailureSubject::Profile { name } => {
            t!("backup_ui.failure.profile", name = name, reason = reason)
        }
        FailureSubject::Group { name } => {
            t!("backup_ui.failure.group", name = name, reason = reason)
        }
        FailureSubject::Forward { host, label } => t!(
            "backup_ui.failure.forward",
            host = host,
            label = label,
            reason = reason
        ),
        FailureSubject::Jump { host } => t!("backup_ui.failure.jump", host = host, reason = reason),
        FailureSubject::GroupJump { group } => {
            t!(
                "backup_ui.failure.group_jump",
                group = group,
                reason = reason
            )
        }
        FailureSubject::Snippet { name } => {
            t!("backup_ui.failure.snippet", name = name, reason = reason)
        }
    }
}

fn describe_reason(reason: &FailureReason) -> SharedString {
    match reason {
        FailureReason::Invalid(invalid) => describe_invalid(invalid),
        FailureReason::Key(error) => crate::key_dialogs::errors::describe(error),
        FailureReason::VaultLocked => t!("backup_ui.error.vault_locked"),
        // 写给日志看的说明（数据库写入失败等），没有对应的文案
        FailureReason::Other(message) => message.clone().into(),
    }
}

/// 字段不合法：端口转发的几项在这里说，其余与保存主机、分组时相同。
fn describe_invalid(invalid: &Invalid) -> SharedString {
    match (invalid.field, invalid.reason) {
        ("bindPort", "conflict") => t!("backup_ui.invalid.bind_conflict"),
        ("bindPort", _) => t!("backup_ui.invalid.bind_port"),
        ("targetPort", _) => t!("backup_ui.invalid.target_port"),
        ("bindHost", _) => t!("backup_ui.invalid.bind_host"),
        ("targetHost", _) => t!("backup_ui.invalid.target_host"),
        _ => store_errors::describe_invalid(invalid),
    }
}

/// 导入时需要提醒的事项；`source` 是来源的名称（见 [`source_name`]）。
pub(super) fn describe_hint(hint: &Hint, source: &str) -> SharedString {
    match hint {
        Hint::MissingUsername => t!("backup_ui.hint.missing_username"),
        Hint::PrivateKeyNotImported { name: Some(name) } => t!(
            "backup_ui.hint.private_key_not_imported_named",
            source = source,
            name = name
        ),
        Hint::PrivateKeyNotImported { name: None } => {
            t!("backup_ui.hint.private_key_not_imported", source = source)
        }
        Hint::PrivateKeyFileMissing => {
            t!("backup_ui.hint.private_key_file_missing", source = source)
        }
        Hint::ExtraPrivateKeys { total } => t!(
            "backup_ui.hint.extra_private_keys",
            source = source,
            total = total
        ),
        Hint::LibraryKeyMissing => t!("backup_ui.hint.library_key_missing", source = source),
        Hint::LibraryKeyUnusable { name } => t!(
            "backup_ui.hint.library_key_unusable",
            source = source,
            name = name
        ),
        Hint::UnknownPathPlaceholder { placeholder } => t!(
            "backup_ui.hint.unknown_path_placeholder",
            placeholder = placeholder
        ),
        Hint::MultiHopJump { chain } => {
            t!("backup_ui.hint.multi_hop_jump", chain = chain.join(" → "))
        }
        Hint::JumpNotFound => t!("backup_ui.hint.jump_not_found", source = source),
        Hint::JumpMissingUsername { name } => {
            t!("backup_ui.hint.jump_missing_username", name = name)
        }
        Hint::ProxyCommandIgnored => t!("backup_ui.hint.proxy_command_ignored"),
        Hint::ProxyCommandImported => t!("backup_ui.hint.proxy_command_imported"),
        Hint::ProxyPortDefaulted { port } => t!("backup_ui.hint.proxy_port_defaulted", port = port),
        Hint::ProxyPasswordRequired => t!("backup_ui.hint.proxy_password_required"),
        Hint::UnsupportedProxy => t!("backup_ui.hint.unsupported_proxy", source = source),
        Hint::GatewayFor { session } => t!(
            "backup_ui.hint.gateway_for",
            source = source,
            session = session
        ),
        Hint::ForwardsNotImported { count } => t!(
            "backup_ui.hint.forwards_not_imported",
            source = source,
            count = count
        ),
        Hint::AgentForwardDisabled => t!("backup_ui.hint.agent_forward_disabled"),
        Hint::UnsupportedEncoding { name } => {
            t!("backup_ui.hint.unsupported_encoding", name = name)
        }
        Hint::ProxyNotImported { name: Some(name) } => t!(
            "backup_ui.hint.proxy_not_imported_named",
            source = source,
            name = name
        ),
        Hint::ProxyNotImported { name: None } => {
            t!("backup_ui.hint.proxy_not_imported", source = source)
        }
        Hint::JumpNotImported => t!("backup_ui.hint.jump_not_imported", source = source),
        Hint::LoginScriptNotImported => {
            t!("backup_ui.hint.login_script_not_imported", source = source)
        }
        Hint::SerialSettingsUnverified => {
            t!("backup_ui.hint.serial_settings_unverified", source = source)
        }
        Hint::SavedPasswordUnreadable => {
            t!("backup_ui.hint.saved_password_unreadable", source = source)
        }
        Hint::SavedPasswordEncrypted => {
            t!("backup_ui.hint.saved_password_encrypted", source = source)
        }
        Hint::SavedPasswordInvalid => {
            t!("backup_ui.hint.saved_password_invalid", source = source)
        }
        Hint::CredentialProfileMissing => {
            t!("backup_ui.hint.credential_profile_missing", source = source)
        }
        Hint::JumpHostFor { session } => t!(
            "backup_ui.hint.jump_host_for",
            source = source,
            session = session
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::i18n::{self, Language};

    fn backup_errors() -> Vec<BackupError> {
        let path = PathBuf::from(r"C:\x\backup.termlet-backup");
        vec![
            BackupError::PasswordTooShort { min: 8 },
            BackupError::PasswordRequired,
            BackupError::WrongPassword,
            BackupError::Invalid,
            BackupError::TooNew {
                found: 9,
                supported: 3,
            },
            BackupError::IsHostsFile,
            BackupError::IsFullBackup,
            BackupError::ForeignSnapshot,
            BackupError::SnapshotNotFound,
            BackupError::InvalidSnapshotName,
            BackupError::NothingSelected,
            BackupError::HostNotFound,
            BackupError::VaultLocked,
            BackupError::VaultUnavailable("no keyring".into()),
            BackupError::FileNotFound { path: path.clone() },
            BackupError::PermissionDenied { path: path.clone() },
            BackupError::ReadFailed {
                path: path.clone(),
                reason: "disk".into(),
            },
            BackupError::WriteFailed {
                path: path.clone(),
                reason: "disk".into(),
            },
            BackupError::NotAbsolute,
            BackupError::IsDirectory { path: path.clone() },
            BackupError::TooLarge,
            BackupError::FolderMissing { path },
            BackupError::InvalidSchedule,
            BackupError::NotScheduled,
            BackupError::SchedulePasswordLost,
            BackupError::Store("db".into()),
        ]
    }

    fn import_errors() -> Vec<ImportError> {
        let path = PathBuf::from("/home/u/.ssh/config");
        let io = |kind| ImportError::Io {
            path: path.clone(),
            source: std::io::Error::from(kind),
        };
        vec![
            io(ErrorKind::NotFound),
            io(ErrorKind::PermissionDenied),
            io(ErrorKind::InvalidData),
            ImportError::TooLarge {
                path: path.clone(),
                limit_mb: 16,
            },
            ImportError::Syntax("line 3".into()),
            ImportError::TabbyEncrypted,
            ImportError::Registry("denied".into()),
            ImportError::TermoraV1,
            ImportError::TermoraNotFound,
            ImportError::NotTermora("bad header".into()),
            ImportError::TermoraKeyMissing,
            ImportError::TermoraDecrypt,
            ImportError::WindTermNotFound,
            ImportError::WindTermPasswordRequired,
            ImportError::WindTermWrongPassword,
            ImportError::NotElecterm("bad json".into()),
            ImportError::ElectermEncrypted,
            ImportError::NotNetcatty,
        ]
    }

    fn hints() -> Vec<Hint> {
        vec![
            Hint::MissingUsername,
            Hint::PrivateKeyNotImported { name: None },
            Hint::PrivateKeyNotImported {
                name: Some("work".into()),
            },
            Hint::PrivateKeyFileMissing,
            Hint::ExtraPrivateKeys { total: 3 },
            Hint::LibraryKeyMissing,
            Hint::LibraryKeyUnusable {
                name: "broken".into(),
            },
            Hint::UnknownPathPlaceholder {
                placeholder: "%X%".into(),
            },
            Hint::MultiHopJump {
                chain: vec!["a".into(), "b".into()],
            },
            Hint::JumpNotFound,
            Hint::JumpMissingUsername { name: "gw".into() },
            Hint::ProxyCommandIgnored,
            Hint::ProxyCommandImported,
            Hint::ProxyPortDefaulted { port: 1080 },
            Hint::ProxyPasswordRequired,
            Hint::UnsupportedProxy,
            Hint::GatewayFor {
                session: "prod".into(),
            },
            Hint::ForwardsNotImported { count: 2 },
            Hint::AgentForwardDisabled,
            Hint::UnsupportedEncoding {
                name: "ISO-8859-1".into(),
            },
            Hint::ProxyNotImported { name: None },
            Hint::ProxyNotImported {
                name: Some("office".into()),
            },
            Hint::JumpNotImported,
            Hint::LoginScriptNotImported,
            Hint::SerialSettingsUnverified,
            Hint::SavedPasswordUnreadable,
            Hint::SavedPasswordEncrypted,
            Hint::SavedPasswordInvalid,
            Hint::CredentialProfileMissing,
            Hint::JumpHostFor {
                session: "app".into(),
            },
        ]
    }

    fn subjects() -> Vec<FailureSubject> {
        vec![
            FailureSubject::Host {
                key: "k".into(),
                name: "web".into(),
            },
            FailureSubject::Secrets { host: "web".into() },
            FailureSubject::Key { name: "id".into() },
            FailureSubject::Profile { name: "ops".into() },
            FailureSubject::Group {
                name: "prod".into(),
            },
            FailureSubject::Forward {
                host: "web".into(),
                label: "8080".into(),
            },
            FailureSubject::Jump { host: "web".into() },
            FailureSubject::GroupJump {
                group: "prod".into(),
            },
            FailureSubject::Snippet {
                name: "restart".into(),
            },
        ]
    }

    /// 每种错误、提示在每种语言下都有文案（缺键时 rust-i18n 返回键名本身），插值都已替换。
    #[test]
    fn every_variant_has_text_in_every_language() {
        let original = i18n::language();
        for language in Language::ALL {
            i18n::set_language(language);
            let mut texts: Vec<SharedString> = Vec::new();
            texts.extend(backup_errors().iter().map(describe_backup));
            texts.extend(import_errors().iter().map(describe_import));
            texts.extend(hints().iter().map(|h| describe_hint(h, "Tabby")));
            let reasons = [
                FailureReason::Invalid(Invalid::new("bindPort", "conflict")),
                FailureReason::Invalid(Invalid::new("name", "too_long")),
                FailureReason::VaultLocked,
                FailureReason::Other("写入失败".into()),
            ];
            for subject in subjects() {
                for reason in &reasons {
                    texts.push(describe_failure(&subject, reason));
                }
            }
            for text in texts {
                assert!(!text.starts_with("backup_ui."), "缺少文案：{text}");
                assert!(!text.contains("%{"), "插值没有替换：{text}");
            }
        }
        i18n::set_language(original);
    }

    #[test]
    fn hints_name_the_source_and_details() {
        let original = i18n::language();
        i18n::set_language(Language::ZhCn);
        let text = describe_hint(
            &Hint::PrivateKeyNotImported {
                name: Some("work".into()),
            },
            source_name(Source::Xshell),
        );
        assert!(text.contains("Xshell") && text.contains("work"), "{text}");
        let chain = describe_hint(
            &Hint::MultiHopJump {
                chain: vec!["a".into(), "b".into()],
            },
            "ssh_config",
        );
        assert!(chain.contains("a → b"), "{chain}");
        let conflict = describe_failure(
            &FailureSubject::Forward {
                host: "web".into(),
                label: "8080".into(),
            },
            &FailureReason::Invalid(Invalid::new("bindPort", "conflict")),
        );
        assert!(
            conflict.contains("web") && conflict.contains("占用"),
            "{conflict}"
        );
        i18n::set_language(original);
    }
}
