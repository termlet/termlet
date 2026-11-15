//! `upload` / `download`：解析参数（本机路径按当前目录解析为绝对路径）、上传前检查本机文件、输出结果。

use std::io;

use termlet_proto::{ErrorCode, ExitCode, TransferRequest};

use crate::i18n::Lang;
use crate::messages;
use crate::output::Output;

/// 解析 `upload <主机> <本地文件> <远程路径>`。
pub fn parse_upload(args: &[String], lang: Lang) -> Result<TransferRequest, String> {
    match args {
        [host, local, remote] => request(host, local, remote, lang),
        _ => Err(messages::USAGE_UPLOAD.get(lang).into()),
    }
}

/// 解析 `download <主机> <远程文件> <本地路径>`；本地路径可以是 `.` 或已存在的目录。
pub fn parse_download(args: &[String], lang: Lang) -> Result<TransferRequest, String> {
    match args {
        [host, remote, local] => request(host, local, remote, lang),
        _ => Err(messages::USAGE_DOWNLOAD.get(lang).into()),
    }
}

fn request(host: &str, local: &str, remote: &str, lang: Lang) -> Result<TransferRequest, String> {
    if host.is_empty() || local.is_empty() || remote.is_empty() {
        return Err(messages::EMPTY_PATH.get(lang).into());
    }
    let absolute = std::path::absolute(local)
        .map_err(|e| messages::BAD_LOCAL_PATH.fill(lang, &[&local, &e]))?;
    Ok(TransferRequest {
        host: host.into(),
        local: absolute.display().to_string(),
        remote: remote.into(),
        caller: Vec::new(),
    })
}

/// 发请求前检查本机文件存在且是普通文件（目前不支持上传目录）。通过时为 `None`，否则输出错误并返回退出码。
pub fn check_upload_source(out: &mut Output<'_>, local: &str) -> Option<i32> {
    let (code, message) = upload_source_problem(local, out.lang)?;
    Some(out.fail(ExitCode::Usage, code, &message))
}

/// 本机要上传的文件的问题：（错误码，说明）；是普通文件时为 `None`。命令行工具与 MCP 服务共用。
pub fn upload_source_problem(local: &str, lang: Lang) -> Option<(&'static str, String)> {
    match std::fs::metadata(local) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Some((ErrorCode::PathNotFound.as_str(), local.to_string()))
        }
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            Some((ErrorCode::PermissionDenied.as_str(), local.to_string()))
        }
        Err(e) => Some((messages::CODE_USAGE, e.to_string())),
        Ok(meta) if meta.is_dir() => Some((
            messages::CODE_USAGE,
            messages::UPLOAD_IS_DIR.fill(lang, &[&local]),
        )),
        Ok(meta) if !meta.is_file() => Some((
            messages::CODE_USAGE,
            messages::UPLOAD_NOT_REGULAR.fill(lang, &[&local]),
        )),
        Ok(_) => None,
    }
}

/// 以 1024 为进位格式化字节数，如 `512 B`、`12.3 MB`。
pub fn format_bytes(n: u64) -> String {
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64;
    let mut unit = "";
    for u in ["KB", "MB", "GB", "TB"] {
        value /= 1024.0;
        unit = u;
        if value < 1024.0 {
            break;
        }
    }
    format!("{value:.1} {unit}")
}

pub fn format_duration(ms: u64, lang: Lang) -> String {
    messages::SECONDS.fill(lang, &[&format!("{:.1}", ms as f64 / 1000.0)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::tests::Captured;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_transfer_arguments() {
        let wd = std::env::current_dir().unwrap();
        let up = parse_upload(&args(&["web-1", "a.log", "/tmp/"]), Lang::Zh).unwrap();
        assert_eq!(up.host, "web-1");
        assert_eq!(up.local, wd.join("a.log").display().to_string());
        assert_eq!(up.remote, "/tmp/");

        let down = parse_download(&args(&["web-1", "/var/log/syslog", "."]), Lang::Zh).unwrap();
        assert_eq!(down.remote, "/var/log/syslog");
        assert_eq!(
            std::path::absolute(&down.local).unwrap(),
            std::path::absolute(&wd).unwrap()
        );

        let tmp = tempfile::tempdir().unwrap();
        let abs = tmp.path().join("x.bin").display().to_string();
        let down = parse_download(&args(&["web-1", "/x.bin", &abs]), Lang::Zh).unwrap();
        assert_eq!(down.local, abs, "绝对路径应原样保留");

        for bad in [
            &["web-1", "a"][..],
            &["web-1", "a", "b", "c"],
            &["web-1", "", "/tmp"],
            &["", "a", "b"],
        ] {
            assert!(
                parse_upload(&args(bad), Lang::Zh).is_err(),
                "upload {bad:?}"
            );
            assert!(
                parse_download(&args(bad), Lang::Zh).is_err(),
                "download {bad:?}"
            );
        }
    }

    #[test]
    fn checks_upload_source() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.txt");
        std::fs::write(&file, "hi").unwrap();

        let mut c = Captured::new();
        assert_eq!(
            check_upload_source(&mut c.output(true, Lang::Zh), &file.display().to_string()),
            None
        );

        let mut c = Captured::new();
        let missing = tmp.path().join("missing").display().to_string();
        assert_eq!(
            check_upload_source(&mut c.output(true, Lang::Zh), &missing),
            Some(1)
        );
        assert!(
            c.stdout().contains(r#""error":"path_not_found""#),
            "{}",
            c.stdout()
        );

        let mut c = Captured::new();
        let dir = tmp.path().display().to_string();
        assert_eq!(
            check_upload_source(&mut c.output(false, Lang::Zh), &dir),
            Some(1)
        );
        assert!(c.stderr().contains("只支持上传单个文件"), "{}", c.stderr());
    }

    #[test]
    fn formats_bytes() {
        for (n, want) in [
            (0, "0 B"),
            (512, "512 B"),
            (1024, "1.0 KB"),
            (1536, "1.5 KB"),
            (12_900_000, "12.3 MB"),
            (5 * 1024 * 1024 * 1024, "5.0 GB"),
        ] {
            assert_eq!(format_bytes(n), want, "{n}");
        }
    }

    #[test]
    fn formats_duration() {
        assert_eq!(format_duration(1234, Lang::Zh), "1.2 秒");
        assert_eq!(format_duration(1234, Lang::En), "1.2s");
    }
}
