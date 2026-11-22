//! `upload` / `download`：解析参数（本机路径按当前目录解析为绝对路径）、上传前检查本机文件、输出结果。

use termlet_proto::{ExitCode, TransferRequest};

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

// 上传前对本机文件的检查、字节数与用时的写法与 MCP 服务共用
pub use termlet_mcp::text::{format_bytes, format_duration, upload_source_problem};

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
}
