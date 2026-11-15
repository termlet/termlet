//! 把同步的错误说成用户能理解的话（[`SyncError`] 的每个变体都有自己的文案）。连接、认证、跳转、服务器的错误按同步位置的
//! 种类（[`Service`]）说：WebDAV 服务器、S3 服务、GitHub。

use gpui_kit::SharedString;
use termlet_core::cloudsync::{
    INTERVAL_RANGE, MAX_DEVICE_NAME, MIN_PASSWORD_LEN, S3_DEFAULT_REGION, Service, SyncError,
};

use crate::t;

/// Gist 的 ID 在错误中最多列出几个。
const MAX_LISTED_GISTS: usize = 5;

/// 字节数写成 MB（保留一位小数）。
fn megabytes(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1024.0 * 1024.0))
}

pub(crate) fn describe(error: &SyncError, service: Service) -> SharedString {
    match (error, service) {
        (SyncError::NotConfigured, _) => t!("sync_ui.error.not_configured"),
        (SyncError::VaultLocked, _) => t!("sync_ui.error.vault_locked"),
        (SyncError::Invalid { field, reason }, _) => describe_invalid(field, reason),
        (SyncError::FolderMissing { path }, _) => t!("sync_ui.error.folder_missing", path = path),
        (SyncError::FolderNotWritable { path }, _) => {
            t!("sync_ui.error.folder_not_writable", path = path)
        }
        (SyncError::Network { message }, Service::S3) => {
            t!("sync_ui.error.s3_network", message = message)
        }
        (SyncError::Network { message }, Service::Gist) => {
            t!("sync_ui.error.gist_network", message = message)
        }
        (SyncError::Network { message }, _) => t!("sync_ui.error.network", message = message),
        (SyncError::AuthFailed, Service::S3) => t!("sync_ui.error.s3_auth_failed"),
        (SyncError::AuthFailed, Service::Gist) => t!("sync_ui.error.gist_auth_failed"),
        (SyncError::AuthFailed, _) => t!("sync_ui.error.auth_failed"),
        (SyncError::Redirected { status, location }, Service::S3) if location.is_empty() => {
            t!("sync_ui.error.s3_redirected_unknown", status = status)
        }
        (SyncError::Redirected { status, location }, Service::S3) => t!(
            "sync_ui.error.s3_redirected",
            status = status,
            location = location
        ),
        (SyncError::Redirected { status, .. }, Service::Gist) => {
            t!("sync_ui.error.gist_redirected", status = status)
        }
        (SyncError::Redirected { status, location }, _) if location.is_empty() => {
            t!("sync_ui.error.redirected_unknown", status = status)
        }
        (SyncError::Redirected { status, location }, _) => t!(
            "sync_ui.error.redirected",
            status = status,
            location = location
        ),
        (SyncError::Server { status }, Service::S3) => {
            t!("sync_ui.error.s3_server", status = status)
        }
        (SyncError::Server { status }, Service::Gist) => {
            t!("sync_ui.error.gist_server", status = status)
        }
        (SyncError::Server { status }, _) => t!("sync_ui.error.server", status = status),
        (SyncError::S3 { status, error_code }, _) => {
            t!("sync_ui.error.s3_code", status = status, code = error_code)
        }
        (SyncError::ClockSkew, _) => t!("sync_ui.error.clock_skew"),
        (
            SyncError::RateLimited {
                retry_after: Some(seconds),
            },
            _,
        ) => t!(
            "sync_ui.error.rate_limited_retry",
            minutes = seconds.div_ceil(60).max(1)
        ),
        (SyncError::RateLimited { retry_after: None }, _) => t!("sync_ui.error.rate_limited"),
        (SyncError::GistAmbiguous { ids }, _) => {
            let mut listed: Vec<&str> = ids
                .iter()
                .take(MAX_LISTED_GISTS)
                .map(String::as_str)
                .collect();
            if ids.len() > MAX_LISTED_GISTS {
                listed.push("…");
            }
            t!("sync_ui.error.gist_ambiguous", ids = listed.join(", "))
        }
        (SyncError::GistTooLarge { size, limit }, _) => t!(
            "sync_ui.error.gist_too_large",
            size = megabytes(*size),
            limit = megabytes(*limit)
        ),
        (SyncError::InsufficientStorage, _) => t!("sync_ui.error.insufficient_storage"),
        (SyncError::WrongPassword, _) => t!("sync_ui.error.wrong_password"),
        (SyncError::Corrupt, _) => t!("sync_ui.error.corrupt"),
        (SyncError::TooLarge, _) => t!("sync_ui.error.too_large"),
        (SyncError::TooNew, _) => t!("sync_ui.error.too_new"),
        (SyncError::Contended, _) => t!("sync_ui.error.contended"),
        (SyncError::LocalBusy, _) => t!("sync_ui.error.local_busy"),
        (SyncError::Snapshot { message }, _) => t!("sync_ui.error.snapshot", message = message),
        (SyncError::Io { message }, _) => t!("sync_ui.error.io", message = message),
        (SyncError::Local { message }, _) => t!("sync_ui.error.local", message = message),
    }
}

/// 设置不合法、冲突不能改用另一边（字段与原因见 [`SyncError::Invalid`] 与 `CloudSync::use_other_version`）。
fn describe_invalid(field: &str, reason: &str) -> SharedString {
    match (field, reason) {
        ("path", _) => t!("sync_ui.error.path_not_absolute"),
        ("url", "has_credentials") => t!("sync_ui.error.url_has_credentials"),
        ("url", "insecure") => t!("sync_ui.error.url_insecure"),
        ("url", _) => t!("sync_ui.error.url_invalid"),
        ("endpoint", "required") => t!("sync_ui.error.endpoint_required"),
        ("endpoint", "has_credentials") => t!("sync_ui.error.endpoint_has_credentials"),
        ("endpoint", "insecure") => t!("sync_ui.error.endpoint_insecure"),
        ("endpoint", _) => t!("sync_ui.error.endpoint_invalid"),
        ("region", "mismatch") => t!("sync_ui.error.region_mismatch"),
        ("region", _) => t!("sync_ui.error.region_invalid", default = S3_DEFAULT_REGION),
        ("bucket", "required") => t!("sync_ui.error.bucket_required"),
        ("bucket", "not_found") => t!("sync_ui.error.bucket_not_found"),
        ("bucket", _) => t!("sync_ui.error.bucket_invalid"),
        ("prefix", _) => t!("sync_ui.error.prefix_invalid"),
        ("access_key", "required") => t!("sync_ui.error.access_key_required"),
        ("access_key", _) => t!("sync_ui.error.access_key_invalid"),
        ("secret_key", _) => t!("sync_ui.error.secret_key_required"),
        ("token", "no_gist_scope") => t!("sync_ui.error.token_no_gist_scope"),
        ("token", _) => t!("sync_ui.error.token_required"),
        ("gist", "not_found") => t!("sync_ui.error.gist_not_found"),
        ("gist", _) => t!("sync_ui.error.gist_invalid"),
        ("password", _) => t!("sync_ui.error.password_too_short", min = MIN_PASSWORD_LEN),
        ("device_name", _) => t!("sync_ui.error.device_name_too_long", max = MAX_DEVICE_NAME),
        ("interval", _) => t!(
            "sync_ui.error.interval_out_of_range",
            min = INTERVAL_RANGE.start(),
            max = INTERVAL_RANGE.end()
        ),
        ("conflict", "not_applicable") => t!("sync_ui.error.conflict_not_applicable"),
        ("conflict", "stale") => t!("sync_ui.error.conflict_stale"),
        ("conflict", _) => t!("sync_ui.error.conflict_not_found"),
        ("profile", "in_use") => t!("sync_ui.error.profile_in_use"),
        ("key", "in_use") => t!("sync_ui.error.key_in_use"),
        (field, reason) => t!(
            "sync_ui.error.invalid_other",
            field = field,
            reason = reason
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{self, Language};

    const SERVICES: [Service; 4] = [Service::Folder, Service::WebDav, Service::S3, Service::Gist];

    /// 每个变体（含 `Invalid` 的各种字段）。
    fn all_errors() -> Vec<SyncError> {
        let invalid = |field: &str, reason: &str| SyncError::Invalid {
            field: field.into(),
            reason: reason.into(),
        };
        vec![
            SyncError::NotConfigured,
            SyncError::VaultLocked,
            invalid("path", "not_absolute"),
            invalid("url", "invalid"),
            invalid("url", "has_credentials"),
            invalid("url", "insecure"),
            invalid("endpoint", "required"),
            invalid("endpoint", "invalid"),
            invalid("endpoint", "has_credentials"),
            invalid("endpoint", "insecure"),
            invalid("region", "invalid"),
            invalid("region", "mismatch"),
            invalid("bucket", "required"),
            invalid("bucket", "invalid"),
            invalid("bucket", "not_found"),
            invalid("prefix", "invalid"),
            invalid("access_key", "required"),
            invalid("access_key", "invalid"),
            invalid("secret_key", "required"),
            invalid("token", "required"),
            invalid("token", "no_gist_scope"),
            invalid("gist", "invalid"),
            invalid("gist", "not_found"),
            invalid("password", "too_short"),
            invalid("device_name", "too_long"),
            invalid("interval", "out_of_range"),
            invalid("conflict", "not_found"),
            invalid("conflict", "not_applicable"),
            invalid("conflict", "stale"),
            invalid("profile", "in_use"),
            invalid("key", "in_use"),
            invalid("other", "why"),
            SyncError::FolderMissing {
                path: "D:/Sync".into(),
            },
            SyncError::FolderNotWritable {
                path: "D:/Sync".into(),
            },
            SyncError::Network {
                message: "timeout".into(),
            },
            SyncError::AuthFailed,
            SyncError::Redirected {
                status: 301,
                location: "https://dav.example.com/termlet/".into(),
            },
            SyncError::Redirected {
                status: 302,
                location: String::new(),
            },
            SyncError::Server { status: 500 },
            SyncError::S3 {
                status: 400,
                error_code: "InvalidArgument".into(),
            },
            SyncError::ClockSkew,
            SyncError::RateLimited { retry_after: None },
            SyncError::RateLimited {
                retry_after: Some(90),
            },
            SyncError::GistAmbiguous {
                ids: vec!["aaa".into(), "bbb".into()],
            },
            SyncError::GistTooLarge {
                size: 9 << 20,
                limit: 8 << 20,
            },
            SyncError::InsufficientStorage,
            SyncError::WrongPassword,
            SyncError::Corrupt,
            SyncError::TooLarge,
            SyncError::TooNew,
            SyncError::Contended,
            SyncError::LocalBusy,
            SyncError::Snapshot {
                message: "disk".into(),
            },
            SyncError::Io {
                message: "denied".into(),
            },
            SyncError::Local {
                message: "db".into(),
            },
        ]
    }

    #[test]
    fn every_error_has_its_own_text_in_every_language() {
        let original = i18n::language();
        let errors = all_errors();
        for language in Language::ALL {
            i18n::set_language(language);
            for service in SERVICES {
                let texts: Vec<SharedString> =
                    errors.iter().map(|e| describe(e, service)).collect();
                for (error, text) in errors.iter().zip(&texts) {
                    assert!(
                        !text.contains("sync_ui.") && !text.contains("%{"),
                        "{language:?} 缺少 {} 的文案：{text}",
                        error.code()
                    );
                    if language == Language::En {
                        assert!(
                            !text.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                            "英文文案中有中文：{text}"
                        );
                    }
                }
                let mut unique = texts.clone();
                unique.sort();
                unique.dedup();
                assert_eq!(
                    unique.len(),
                    texts.len(),
                    "{language:?} {service:?} 的错误文案有重复"
                );
            }
        }
        i18n::set_language(original);
    }

    #[test]
    fn details_are_kept_in_the_text() {
        let original = i18n::language();
        i18n::set_language(Language::ZhCn);
        let text = describe(
            &SyncError::FolderMissing {
                path: "E:/U 盘/termlet".into(),
            },
            Service::Folder,
        );
        assert!(text.contains("E:/U 盘/termlet"));
        let text = describe(
            &SyncError::Redirected {
                status: 301,
                location: "https://dav.example.com/x/".into(),
            },
            Service::WebDav,
        );
        assert!(text.contains("https://dav.example.com/x/"));
        assert!(describe(&SyncError::Server { status: 507 }, Service::WebDav).contains("507"));
        // 服务不同，说法不同
        assert!(describe(&SyncError::AuthFailed, Service::S3).contains("访问密钥"));
        assert!(describe(&SyncError::AuthFailed, Service::Gist).contains("令牌"));
        let text = describe(
            &SyncError::GistAmbiguous {
                ids: (0..7).map(|i| format!("id{i}")).collect(),
            },
            Service::Gist,
        );
        assert!(text.contains("id0") && text.contains("id4") && !text.contains("id5"));
        let text = describe(
            &SyncError::GistTooLarge {
                size: 9 << 20,
                limit: 8 << 20,
            },
            Service::Gist,
        );
        assert!(text.contains("9.0") && text.contains("8.0"), "{text}");
        assert!(
            describe(
                &SyncError::RateLimited {
                    retry_after: Some(90)
                },
                Service::Gist
            )
            .contains('2')
        );
        i18n::set_language(original);
    }
}
