//! 检查、下载与安装更新的错误，以及给界面选文案用的分类。
//!
//! 完整的原因写进日志（只记一行摘要，[`error_summary`]）；界面按 [`UpdateErrorKind`] 显示一行简短的说明，
//! 悬停提示中给出前 160 个字符（[`UpdateError::short`]）。

use std::io;

use super::signature::SignatureError;

/// 界面按它选文案。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UpdateErrorKind {
    /// 更新源还没有发布版本信息（404）：“暂未获取到更新信息（尚未发布新版本）”。
    Unpublished,
    /// 无法连接：“无法连接更新服务器，请检查网络后重试”。
    Network,
    /// 签名校验没有通过，不下载、不安装：“更新文件的签名校验没有通过……请到发布页手动下载”，旁边给出“前往下载”。
    Signature,
    /// 其他：“检查更新失败，详情见日志”。
    Other,
}

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("更新源没有发布版本信息（{0}：HTTP 404）")]
    Unpublished(String),
    #[error("请求 {file} 失败：HTTP {status}")]
    Http { file: String, status: u16 },
    #[error("无法连接更新服务器：{0}")]
    Network(String),
    #[error("{0}")]
    Signature(#[from] SignatureError),
    #[error("版本描述文件的格式不对：{0}")]
    Manifest(String),
    #[error("版本描述文件中没有适合本机的安装包")]
    NoInstaller,
    #[error("下载的安装包与版本信息不一致（{0}）")]
    Download(String),
    #[error("读写更新文件失败：{0}")]
    Io(#[from] io::Error),
    #[error("启动安装程序失败：{0}")]
    Launch(io::Error),
    #[error("没有已下载的更新")]
    NotDownloaded,
}

impl UpdateError {
    pub fn kind(&self) -> UpdateErrorKind {
        match self {
            UpdateError::Unpublished(_) => UpdateErrorKind::Unpublished,
            UpdateError::Network(_) => UpdateErrorKind::Network,
            UpdateError::Signature(_) => UpdateErrorKind::Signature,
            _ => UpdateErrorKind::Other,
        }
    }

    /// 界面悬停提示中的简短说明：第一行，最多 160 个字符。
    pub fn short(&self) -> String {
        error_summary(&self.to_string(), 160)
    }

    /// 把 reqwest 的错误转成“无法连接”（去掉地址，附上底层原因，例如证书不受信任）。
    pub(crate) fn network(error: reqwest::Error) -> Self {
        let timeout = error.is_timeout();
        let error = error.without_url();
        let mut message = error.to_string();
        let mut source = std::error::Error::source(&error);
        while let Some(cause) = source {
            message.push('：');
            message.push_str(&cause.to_string());
            source = cause.source();
        }
        if timeout {
            message.insert_str(0, "超时：");
        }
        UpdateError::Network(message)
    }
}

/// 日志与界面用的错误摘要：只取第一行并限制长度（按字符计），超出时以“…”结尾。
pub fn error_summary(message: &str, max: usize) -> String {
    let first = message.lines().next().unwrap_or("").trim();
    if first.chars().count() > max {
        let mut out: String = first.chars().take(max).collect();
        out.push('…');
        out
    } else {
        first.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_for_the_interface() {
        assert_eq!(
            UpdateError::Unpublished("latest.yml".into()).kind(),
            UpdateErrorKind::Unpublished
        );
        assert_eq!(
            UpdateError::Network("连接被拒绝".into()).kind(),
            UpdateErrorKind::Network
        );
        // “没有签名（HTTP 404）”属于签名问题，不是“尚未发布”
        let missing = UpdateError::from(SignatureError::Missing {
            file: "latest.yml".into(),
            status: 404,
        });
        assert_eq!(missing.kind(), UpdateErrorKind::Signature);
        assert_eq!(missing.to_string(), "latest.yml 没有签名（HTTP 404）");
        // 安装前核对失败
        assert_eq!(
            UpdateError::from(SignatureError::InstallerMismatch).kind(),
            UpdateErrorKind::Signature
        );
        // 下载的文件与清单不符（多半是下载出错），重新检查即可
        for other in [
            UpdateError::Download("sha512 不同".into()),
            UpdateError::Http {
                file: "latest.yml".into(),
                status: 500,
            },
            UpdateError::NoInstaller,
            UpdateError::Manifest("x".into()),
        ] {
            assert_eq!(other.kind(), UpdateErrorKind::Other);
        }
    }

    #[test]
    fn summary_keeps_first_line_and_truncates() {
        assert_eq!(error_summary("line1\nset-cookie: secret", 300), "line1");
        assert_eq!(
            error_summary(&"x".repeat(400), 10),
            format!("{}…", "x".repeat(10))
        );
        assert_eq!(error_summary(&"中".repeat(5), 3), "中中中…");
        let long = UpdateError::Network("y".repeat(400));
        assert!(long.short().chars().count() <= 161);
    }
}
