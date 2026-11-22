//! 命令行工具与 MCP 服务共用的、给人（与 AI）看的文本：中英文对照、错误码的说明、证书快过期的提醒、字节数与用时的
//! 写法、上传前对本机文件的检查。
//!
//! 只有简体中文与英文（[`Lang`]）：命令行工具按自己的语言设置选，应用内的 HTTP 服务按界面语言映射（简体、繁体中文用
//! 中文，其余用英文）。

use std::io;

use termlet_proto::{ErrorCode, ExpiringCertificate, Lang};

/// 一条中英文对照的文本。
#[derive(Clone, Copy, Debug)]
pub struct Msg {
    pub zh: &'static str,
    pub en: &'static str,
}

impl Msg {
    pub fn get(self, lang: Lang) -> &'static str {
        lang.pick(self.zh, self.en)
    }

    /// 依次把文本中的 `{}` 换成参数。
    pub fn fill(self, lang: Lang, args: &[&dyn std::fmt::Display]) -> String {
        let mut out = String::new();
        let mut rest = self.get(lang);
        let mut args = args.iter();
        while let Some(i) = rest.find("{}") {
            out.push_str(&rest[..i]);
            if let Some(arg) = args.next() {
                out.push_str(&arg.to_string());
            }
            rest = &rest[i + 2..];
        }
        out.push_str(rest);
        out
    }
}

macro_rules! msg {
    ($name:ident, $zh:expr, $en:expr) => {
        pub const $name: Msg = Msg { zh: $zh, en: $en };
    };
}

/// 命令行工具自己产生的用法错误的错误码（不经过 Termlet）。
pub const CODE_USAGE: &str = "usage";

msg!(
    UPLOAD_IS_DIR,
    "{} 是目录，目前只支持上传单个文件；可先打包（如 tar czf dir.tar.gz dir）再上传",
    "{} is a directory; only single files can be uploaded for now. Archive it first (e.g. tar czf dir.tar.gz dir)"
);
msg!(
    UPLOAD_NOT_REGULAR,
    "{} 不是普通文件",
    "{} is not a regular file"
);

// 运行结果
msg!(
    NO_HOSTS,
    "没有允许 CLI 访问的主机。请在 Termlet 中编辑主机，勾选“允许 AI 工具与脚本使用这台主机”。",
    "No hosts allow CLI access. Edit a host in Termlet and turn on “Allow AI tools and scripts to use this host”."
);
msg!(UPLOADED, "已上传 {} → {}（{}）", "Uploaded {} → {} ({})");
msg!(
    DOWNLOADED,
    "已下载 {} → {}（{}）",
    "Downloaded {} → {} ({})"
);
msg!(
    VIA_SCP,
    "服务器没有 SFTP，已改用 SCP 传输（不能断点续传，失败后要从头再传）",
    "The server has no SFTP, so the file went over SCP instead (no resuming; a failed transfer starts over)"
);
msg!(
    CERT_EXPIRING,
    "登录用的证书将于 {} 过期（还剩约 {} 分钟），过期后无法再连接这台主机，请及时重新签发。",
    "The certificate used to log in expires at {} (about {} min left). After that you won't be able to connect to this host, so get a new one issued in time."
);
msg!(
    JUMP_CERT_EXPIRING,
    "跳板机 {} 登录用的证书将于 {} 过期（还剩约 {} 分钟），过期后无法再经它连接，请及时重新签发。",
    "The certificate used to log in to the jump host {} expires at {} (about {} min left). After that you won't be able to connect through it, so get a new one issued in time."
);
msg!(SECONDS, "{} 秒", "{}s");
msg!(
    PROTOCOL,
    "Termlet 的回答不符合协议: {}",
    "unexpected answer from Termlet: {}"
);

// MCP 的工具结果中给 AI 看的说明
msg!(MCP_ARG_MISSING, "缺少参数 {}", "missing argument: {}");
msg!(
    MCP_ARG_STRING,
    "参数 {} 必须是字符串",
    "argument {} must be a string"
);
msg!(
    MCP_ARG_BOOL,
    "参数 {} 必须是 true 或 false",
    "argument {} must be true or false"
);
msg!(
    MCP_ARG_EMPTY,
    "参数 {} 不能为空",
    "argument {} must not be empty"
);
msg!(
    MCP_ARG_UNKNOWN,
    "不认识的参数 {}（这个工具的参数：{}）",
    "unknown argument {} (this tool takes: {})"
);
msg!(
    MCP_NOT_ABSOLUTE,
    "{} 必须是这台电脑上的绝对路径：{}",
    "{} must be an absolute path on this computer: {}"
);
msg!(
    MCP_COMMAND_TOO_LARGE,
    "命令超过 1MB，未执行；较大的脚本请先用 upload_file 上传到服务器再执行",
    "the command exceeds 1MB; nothing was run. Upload larger scripts with upload_file first, then run them"
);
msg!(
    MCP_HOST_NOT_FOUND,
    "主机不存在，请先调用 list_hosts 查看可用的主机",
    "host not found; call list_hosts to see the available hosts"
);
msg!(MCP_EXIT, "退出码 {}，用时 {}", "exit code {}, took {}");
msg!(
    MCP_EXIT_APPROVED,
    "退出码 {}，用时 {}（另等待用户批准 {}）",
    "exit code {}, took {} (plus {} waiting for the user's approval)"
);
msg!(MCP_NO_OUTPUT, "（没有输出）", "(no output)");
msg!(
    MCP_CLIPPED,
    "[termlet] 输出较长，只返回了开头与结尾（合计约 {} KB）。需要完整的输出时，把它重定向到服务器上的文件再分段读取（head、tail、grep 等），或用 download_file 下载。",
    "[termlet] The output was long, so only its beginning and end are shown (about {} KB in total). For the full output, redirect it to a file on the server and read it in parts (head, tail, grep and so on), or fetch it with download_file."
);
msg!(
    MCP_APP_TRUNCATED,
    "[termlet] 标准输出或标准错误超过 {} 字节，超出的部分没有返回。",
    "[termlet] stdout or stderr exceeded {} bytes; the rest was not returned."
);

/// 错误码对应的说明（Termlet 返回的错误码；命令行工具自己的错误码由它另外补上）。
pub fn human(code: &str) -> Option<Msg> {
    use ErrorCode::*;
    let (zh, en) = match ErrorCode::parse(code)? {
        HostNotFound => (
            "主机不存在，请先运行 termlet ls 查看可用主机",
            "host not found; run termlet ls to see available hosts",
        ),
        HostNotAllowed => (
            "该主机未允许 CLI 访问，请在 Termlet 中编辑这台主机，勾选“允许 AI 工具与脚本使用这台主机”",
            "this host does not allow CLI access; edit it in Termlet and turn on “Allow AI tools and scripts to use this host”",
        ),
        CommandBlocked => (
            "命令命中危险命令规则被拦截，可在 Termlet 设置中的「AI 与自动化」中查看规则",
            "command blocked by a dangerous-command rule; see the rules under Settings → AI & automation in Termlet",
        ),
        CommandDenied => (
            "命令命中危险命令规则，未获得批准（在 Termlet 中被拒绝或等待超时），不要尝试绕过，请让用户在终端中手动执行",
            "command matches a dangerous-command rule and was not approved in Termlet (denied or timed out); do not try to bypass it, ask the user to run it manually",
        ),
        AppLocked => (
            "Termlet 已锁定，请先在 Termlet 中解锁",
            "Termlet is locked; unlock it in Termlet first",
        ),
        CliDisabled => (
            "外部 CLI 已关闭，请在 Termlet 设置中的「AI 与自动化」中开启",
            "External CLI is turned off; turn it on in Termlet under Settings → AI & automation",
        ),
        AppNotRunning => ("Termlet 没有运行", "Termlet is not running"),
        ConnectFailed => ("连接或执行失败", "connection or execution failed"),
        BadRequest => ("请求无效", "invalid request"),
        VaultLocked => (
            "Termlet 的凭据已锁定，请先在 Termlet 中输入主密码",
            "Termlet credentials are locked; enter the master password in Termlet first",
        ),
        VaultNeedsSetup => (
            "系统钥匙串不可用，请先在 Termlet 中设置主密码",
            "the system keychain is unavailable; set a master password in Termlet first",
        ),
        VaultKeyLost => (
            "系统钥匙串中的主密钥丢失，已保存的凭据无法解密，请在 Termlet 中重新录入",
            "the master key in the system keychain is missing and saved credentials cannot be decrypted; re-enter them in Termlet",
        ),
        CredentialMissing => (
            "主机还没有凭据，请先在 Termlet 中编辑主机",
            "the host has no credentials; edit the host in Termlet first",
        ),
        HostKeyUnknown => (
            "尚未确认该主机的指纹，请先在 Termlet 中连接一次并确认",
            "the host key has not been confirmed; connect once in Termlet and accept it first",
        ),
        HostKeyMismatch => (
            "主机指纹与记录不一致，可能存在中间人攻击，已拒绝连接。如确认服务器已重装，请在 Termlet 中连接该主机，按提示删除旧记录",
            "the host key does not match the recorded one (possible man-in-the-middle attack); connection refused. If the server was reinstalled, connect to it in Termlet and remove the old record when prompted",
        ),
        HostKeyRejected => ("主机指纹被拒绝", "the host key was rejected"),
        AuthFailed => (
            "认证失败，请检查用户名与凭据",
            "authentication failed; check the username and credentials",
        ),
        AuthInteractionRequired => (
            "该主机需要输入验证码等交互认证，CLI 无法完成，请在 Termlet 中使用",
            "this host requires interactive authentication (e.g. a verification code) that the CLI cannot complete; use Termlet instead",
        ),
        ProxyCommandUnconfirmed => (
            "该主机经代理命令连接，这条命令还没有在这台电脑上确认过，请先在 Termlet 中连接一次并允许执行",
            "this host connects through a proxy command that hasn't been confirmed on this computer; connect once in Termlet and allow it first",
        ),
        KeyPassphraseRequired => (
            "私钥已加密但没有保存私钥密码，请在 Termlet 中编辑主机或改用密钥库中的密钥",
            "the private key is encrypted but no passphrase is saved; edit the host in Termlet or use a key from the key library",
        ),
        KeyWrongPassphrase => (
            "保存的私钥密码不正确，请在 Termlet 中编辑主机",
            "the saved key passphrase is incorrect; edit the host in Termlet",
        ),
        KeyInvalid => (
            "私钥内容无效或已损坏",
            "the private key is invalid or corrupted",
        ),
        KeyUnsupported => (
            "不支持的私钥格式或算法",
            "unsupported private key format or algorithm",
        ),
        AlgorithmMismatch => (
            "与服务器没有共同的加密算法，请在 Termlet 中为主机勾选「兼容旧设备」",
            "no common encryption algorithm with the server; enable \"Legacy devices\" for the host in Termlet",
        ),
        TerminalNotOpen => (
            "这台主机没有在 Termlet 中打开的终端，请先在 Termlet 中打开",
            "this host has no terminal open in Termlet; open one in Termlet first",
        ),
        TerminalBusy => (
            "终端正忙（正在运行程序或全屏程序），请回到 shell 提示符后重试",
            "the terminal is busy (a program or full-screen app is running); return to the shell prompt and try again",
        ),
        PathNotFound => ("路径不存在", "path not found"),
        PathExists => ("路径已存在", "path already exists"),
        PermissionDenied => ("没有权限", "permission denied"),
        SftpUnavailable => (
            "服务器上不能使用 SFTP，也不能改用 SCP，无法传输文件",
            "neither SFTP nor SCP can be used on the server; cannot transfer files",
        ),
        ProtocolMismatch => (
            "termlet 与 Termlet 的版本不一致，请重新安装 Termlet",
            "termlet and Termlet versions do not match; reinstall Termlet",
        ),
        Internal => (
            "Termlet 内部错误，请查看 Termlet 日志",
            "internal Termlet error; check the Termlet logs",
        ),
        NotFound => ("要找的记录不存在", "not found"),
        Canceled => ("已取消", "canceled"),
    };
    Some(Msg { zh, en })
}

/// 错误的可读说明：错误码的说明 + 服务端的补充说明（如命中的规则）。
pub fn human_text(lang: Lang, code: &str, message: &str) -> String {
    let text = human(code)
        .map(|m| m.get(lang).to_string())
        .unwrap_or_else(|| code.to_string());
    if message.is_empty() {
        text
    } else if code == CODE_USAGE {
        message.to_string()
    } else {
        format!("{text}: {message}")
    }
}

/// 登录用的证书快过期的提醒，每张证书一句（写法与 Termlet 终端里的提醒相同）。命令行的可读文本模式写在标准错误，
/// MCP 写进结果。
pub fn expiring_notes(certificates: &[ExpiringCertificate], lang: Lang) -> Vec<String> {
    certificates
        .iter()
        .map(|c| {
            let time = to_minute(&c.expires_at);
            match &c.jump {
                Some(jump) => JUMP_CERT_EXPIRING.fill(lang, &[jump, &time, &c.minutes_left]),
                None => CERT_EXPIRING.fill(lang, &[&time, &c.minutes_left]),
            }
        })
        .collect()
}

/// 应用给的失效时间（本地时间的 RFC 3339，`2026-11-19T20:30:00+08:00`）写到分钟：`2026-11-19 20:30`；认不出的原样。
fn to_minute(rfc3339: &str) -> String {
    match (rfc3339.get(..10), rfc3339.get(10..11), rfc3339.get(11..16)) {
        (Some(date), Some("T"), Some(time)) => format!("{date} {time}"),
        _ => rfc3339.to_string(),
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
    SECONDS.fill(lang, &[&format!("{:.1}", ms as f64 / 1000.0)])
}

/// 本机要上传的文件的问题：（错误码，说明）；是普通文件时为 `None`。命令行工具与 MCP 服务共用（应用内的 HTTP 服务
/// 与命令行工具以同一用户运行，检查的是同一个文件）。
pub fn upload_source_problem(local: &str, lang: Lang) -> Option<(&'static str, String)> {
    match std::fs::metadata(local) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Some((ErrorCode::PathNotFound.as_str(), local.to_string()))
        }
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            Some((ErrorCode::PermissionDenied.as_str(), local.to_string()))
        }
        Err(e) => Some((CODE_USAGE, e.to_string())),
        Ok(meta) if meta.is_dir() => Some((CODE_USAGE, UPLOAD_IS_DIR.fill(lang, &[&local]))),
        Ok(meta) if !meta.is_file() => Some((CODE_USAGE, UPLOAD_NOT_REGULAR.fill(lang, &[&local]))),
        Ok(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 每个错误码都有中英文说明
    #[test]
    fn every_code_has_both_languages() {
        for code in ErrorCode::ALL {
            let m = human(code.as_str()).unwrap_or_else(|| panic!("{code} 没有说明"));
            assert!(!m.zh.is_empty() && !m.en.is_empty(), "{code}");
        }
        assert!(human("something_new").is_none());
    }

    #[test]
    fn unknown_codes_are_shown_as_is() {
        assert_eq!(human_text(Lang::Zh, "brand_new", ""), "brand_new");
        assert_eq!(human_text(Lang::Zh, "brand_new", "x"), "brand_new: x");
        assert_eq!(
            human_text(Lang::Zh, "cli_disabled", ""),
            human("cli_disabled").unwrap().zh
        );
        assert_eq!(human_text(Lang::En, CODE_USAGE, "bad"), "bad");
    }

    // 快过期的证书：写到分钟的失效时间、还剩几分钟，跳板机的写明哪一台
    #[test]
    fn expiring_certificate_notes() {
        let certificates = [
            ExpiringCertificate {
                jump: Some("bastion".into()),
                expires_at: "2026-11-19T20:30:00+08:00".into(),
                minutes_left: 25,
            },
            ExpiringCertificate {
                jump: None,
                expires_at: "2026-11-19T20:40:59-05:00".into(),
                minutes_left: 1,
            },
        ];
        assert_eq!(
            expiring_notes(&certificates, Lang::Zh),
            [
                "跳板机 bastion 登录用的证书将于 2026-11-19 20:30 过期（还剩约 25 分钟），过期后无法再经它连接，请及时重新签发。",
                "登录用的证书将于 2026-11-19 20:40 过期（还剩约 1 分钟），过期后无法再连接这台主机，请及时重新签发。",
            ]
        );
        let en = expiring_notes(&certificates, Lang::En);
        assert!(
            en[0].starts_with(
                "The certificate used to log in to the jump host bastion expires at 2026-11-19 20:30 (about 25 min left)."
            ),
            "{en:?}"
        );
        assert_eq!(to_minute("soon"), "soon", "认不出的原样");
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

    #[test]
    fn fills_placeholders() {
        assert_eq!(
            UPLOADED.fill(Lang::En, &[&"1 B", &"/tmp", &"0.1s"]),
            "Uploaded 1 B → /tmp (0.1s)"
        );
        // 参数不够时空着
        assert_eq!(MCP_EXIT.fill(Lang::Zh, &[&3]), "退出码 3，用时 ");
    }

    #[test]
    fn upload_source_problems() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.txt");
        std::fs::write(&file, "hi").unwrap();
        assert_eq!(
            upload_source_problem(&file.display().to_string(), Lang::Zh),
            None
        );
        let missing = tmp.path().join("missing").display().to_string();
        assert_eq!(
            upload_source_problem(&missing, Lang::Zh),
            Some(("path_not_found", missing.clone()))
        );
        let dir = tmp.path().display().to_string();
        let (code, message) = upload_source_problem(&dir, Lang::Zh).unwrap();
        assert_eq!(code, CODE_USAGE);
        assert!(message.contains("只支持上传单个文件"), "{message}");
    }
}
