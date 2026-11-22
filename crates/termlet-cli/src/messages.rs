//! 人类可读文本的中英文对照。JSON 输出不使用这里的文本（错误的 `message` 除外：服务端没有补充说明时用错误码的说明）。

use termlet_proto::ErrorCode;

use crate::i18n::Msg;

macro_rules! msg {
    ($name:ident, $zh:expr, $en:expr) => {
        pub const $name: Msg = Msg { zh: $zh, en: $en };
    };
}

pub const USAGE: Msg = Msg {
    zh: "termlet — 通过 Termlet 中保存的主机执行远程命令、传输文件

用法:
  termlet ls [-q <关键词>]                        列出允许 CLI 访问的主机（不含凭据）
  termlet exec <主机> \"<命令>\"                    执行一条远程命令（每次新建连接，无状态）
  termlet exec <主机> --stdin                     从标准输入读取命令（规避 shell 转义问题）
  termlet exec --terminal <主机> \"<命令>\"         在 Termlet 中已打开的这台主机的终端里执行（用户看得见；
                                                  堡垒机后面的服务器不能新建连接时使用；需要 bash / sh）
  termlet upload <主机> <本地文件> <远程路径>       上传单个文件（scp 语义）
  termlet download <主机> <远程文件> <本地路径>     下载单个文件（本地路径可为 . 或已存在的目录）
                                                  上传、下载用 SFTP，服务器没有 SFTP 时改用 SCP
  termlet skill                                   打印 Agent Skill 文件（SKILL.md）
  termlet skill install [--agent <位置>]          安装到 claude（Claude Code，默认）、agents（通用目录 ~/.agents/skills，
                                                  Codex、Cursor、Gemini CLI、Pi 等读取；codex、pi 等同 agents）或 workbuddy
  termlet skill install --dir <目录>              安装到指定的 skills 目录
  termlet mcp                                     在标准输入输出上提供 MCP 服务（由 Claude Code、Claude Desktop 等 AI 工具启动，
                                                  配置方法见 Termlet 设置中的「AI 与自动化」）
  termlet version                                 打印版本号
  termlet help                                    打印本帮助

<主机> 可以是 termlet ls 中的 ID 或名称。

输出: 标准输出是终端时为可读文本，被管道或程序捕获时为 JSON（字段名与错误码固定为英文）。

退出码:
  0  成功
  1  用法错误
  2  主机不存在
  3  主机未允许 CLI 访问
  4  命令命中危险命令规则被拦截
  5  连接或执行失败
  6  凭据已锁定
  7  外部 CLI 已关闭（在 Termlet 设置中的「AI 与自动化」中开启）
  其他  exec 连接成功后透传远程命令的退出码

语言: 跟随 LC_ALL / LC_MESSAGES / LANG 或系统界面语言，可用 TERMLET_LANG=en|zh 指定。
",
    en: "termlet — run remote commands and transfer files via hosts saved in Termlet

Usage:
  termlet ls [-q <keyword>]                       List hosts that allow CLI access (no credentials)
  termlet exec <host> \"<command>\"                 Run a remote command (new connection each time, stateless)
  termlet exec <host> --stdin                     Read the command from stdin (avoids shell escaping)
  termlet exec --terminal <host> \"<command>\"      Run it in the terminal of this host already open in Termlet (visible
                                                  to the user; for servers behind bastions that refuse new connections;
                                                  needs bash / sh)
  termlet upload <host> <local-file> <remote>     Upload a single file (scp semantics)
  termlet download <host> <remote-file> <local>   Download a single file (local may be . or an existing directory)
                                                  Transfers use SFTP, or SCP when the server has no SFTP
  termlet skill                                   Print the Agent Skill file (SKILL.md)
  termlet skill install [--agent <target>]        Install for claude (Claude Code, default), agents (shared ~/.agents/skills,
                                                  read by Codex, Cursor, Gemini CLI, Pi and others; codex and pi are aliases)
                                                  or workbuddy
  termlet skill install --dir <dir>               Install into the given skills directory
  termlet mcp                                     Serve MCP over stdin/stdout (started by AI tools such as Claude Code and Claude
                                                  Desktop; see Settings → AI & automation in Termlet for the setup)
  termlet version                                 Print the version
  termlet help                                    Print this help

<host> can be an ID or a name from termlet ls.

Output: human-readable text when stdout is a terminal, JSON when piped or captured
(field names and error codes are always English).

Exit codes:
  0  success
  1  usage error
  2  host not found
  3  host does not allow CLI access
  4  command blocked by a dangerous-command rule
  5  connection or execution failed
  6  credentials are locked
  7  external CLI is turned off (turn it on in Termlet under Settings → AI & automation)
  other  exec passes through the remote command's exit code once connected

Language: follows LC_ALL / LC_MESSAGES / LANG or the system UI language; override with TERMLET_LANG=en|zh.
",
};

// 用法与参数错误
msg!(
    UNKNOWN_COMMAND,
    "未知命令: {}（运行 termlet help 查看用法）",
    "unknown command: {} (run termlet help for usage)"
);
msg!(
    USAGE_LS,
    "用法: termlet ls [-q <关键词>]",
    "usage: termlet ls [-q <keyword>]"
);
msg!(
    USAGE_EXEC,
    "用法: termlet exec [--terminal] <主机> \"<命令>\" 或 termlet exec [--terminal] <主机> --stdin",
    "usage: termlet exec [--terminal] <host> \"<command>\" or termlet exec [--terminal] <host> --stdin"
);
msg!(
    USAGE_UPLOAD,
    "用法: termlet upload <主机> <本地文件> <远程路径>",
    "usage: termlet upload <host> <local-file> <remote-path>"
);
msg!(
    USAGE_DOWNLOAD,
    "用法: termlet download <主机> <远程文件> <本地路径>",
    "usage: termlet download <host> <remote-file> <local-path>"
);
msg!(
    USAGE_SKILL,
    "用法: termlet skill 或 termlet skill install [--agent claude|agents|workbuddy | --dir <目录>]",
    "usage: termlet skill or termlet skill install [--agent claude|agents|workbuddy | --dir <dir>]"
);
msg!(
    READ_STDIN,
    "读取标准输入失败: {}",
    "failed to read stdin: {}"
);
msg!(
    STDIN_NOT_UTF8,
    "标准输入不是 UTF-8 文本",
    "stdin is not UTF-8 text"
);
msg!(
    STDIN_TOO_LARGE,
    "标准输入超过 1MB，未执行；较大的脚本请先 termlet upload 到服务器再执行",
    "stdin exceeds 1MB; nothing was run. Upload larger scripts with termlet upload first, then run them"
);
msg!(EMPTY_COMMAND, "命令不能为空", "command must not be empty");
msg!(EMPTY_PATH, "路径不能为空", "path must not be empty");
msg!(
    BAD_LOCAL_PATH,
    "无法解析本地路径 {}: {}",
    "cannot resolve local path {}: {}"
);
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
msg!(
    HOSTS_HEADER,
    "ID\t名称\t地址\t用户\t协议",
    "ID\tNAME\tADDRESS\tUSER\tPROTOCOL"
);
msg!(
    TRUNCATED,
    "\n[termlet] 输出超过 {} 字节，已截断\n",
    "\n[termlet] output exceeded {} bytes and was truncated\n"
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
    SKILL_WRITTEN,
    "已安装 termlet 技能文件: {}",
    "Installed termlet skill file: {}"
);
msg!(
    SKILL_REPLACED,
    "已覆盖原有的 termlet 技能文件: {}",
    "Replaced existing termlet skill file: {}"
);
msg!(
    NO_HOME_DIR,
    "无法确定用户主目录",
    "cannot determine home directory"
);

// 连接 Termlet
msg!(
    APP_TIMEOUT,
    "等待 Termlet 启动超时",
    "timed out waiting for Termlet to start"
);
msg!(
    APP_START,
    "启动 Termlet 失败: {}",
    "failed to start Termlet: {}"
);
msg!(
    APP_NOT_FOUND,
    "Termlet 没有在运行，也没有找到 Termlet 程序（可用 TERMLET_APP 指定）",
    "Termlet is not running and the Termlet program was not found (set TERMLET_APP to its path)"
);
msg!(
    APP_NOT_RUNNING,
    "请先打开 Termlet（设置的「AI 与自动化」中没有允许在 Termlet 未打开时自动在后台启动它）",
    "open Termlet first (Settings → AI & automation doesn't allow starting it in the background when it isn't open)"
);
msg!(
    CLI_UNAVAILABLE,
    "Termlet 正在运行，但命令行服务没有启动（{}）。请查看 Termlet 数据目录下的 logs 中的日志；常见原因是数据目录的路径过长，或位于网络共享上",
    "Termlet is running but its command-line service did not start ({}). See the logs in the Termlet data directory; this usually means the data directory path is too long or on a network share"
);
msg!(
    IMPOSTOR,
    "命令行通道不属于当前用户（可能被其他用户的程序占用），已拒绝连接",
    "the command-line channel is not owned by the current user (another user's program may be holding it); refused to connect"
);
msg!(
    CONNECT_IO,
    "连接 Termlet 失败: {}",
    "failed to connect to Termlet: {}"
);
msg!(
    PROTOCOL,
    "Termlet 的回答不符合协议: {}",
    "unexpected answer from Termlet: {}"
);
msg!(
    CLOSED,
    "Termlet 在回答前关闭了连接",
    "Termlet closed the connection before answering"
);
msg!(
    DATA_DIR,
    "找不到 Termlet 的数据目录: {}",
    "cannot find the Termlet data directory: {}"
);

// MCP 服务（`termlet mcp`）：用法、工具结果中给 AI 看的说明
msg!(
    USAGE_MCP,
    "用法: termlet mcp（由 AI 工具启动，在标准输入输出上提供 MCP 服务，没有其他参数）",
    "usage: termlet mcp (started by AI tools; serves MCP over stdin/stdout and takes no other arguments)"
);
msg!(
    MCP_INTERACTIVE,
    "termlet mcp 是给 AI 工具（Claude Code、Claude Desktop、Cursor、Codex 等）启动的 MCP 服务，从标准输入读取 JSON-RPC 消息；配置方法见 Termlet 设置中的「AI 与自动化」。按 Ctrl+C 退出。",
    "termlet mcp is an MCP server for AI tools (Claude Code, Claude Desktop, Cursor, Codex and others) to start; it reads JSON-RPC messages from stdin. See Settings → AI & automation in Termlet for the setup. Press Ctrl+C to quit."
);
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

/// 命令行工具自己产生的错误码（不经过 Termlet）。
pub const CODE_USAGE: &str = "usage";
pub const CODE_SKILL_INSTALL_FAILED: &str = "skill_install_failed";

/// 错误码对应的说明，文本模式下显示在服务端补充的说明前面。
pub fn human(code: &str) -> Option<Msg> {
    if code == CODE_SKILL_INSTALL_FAILED {
        return Some(Msg {
            zh: "安装技能文件失败",
            en: "failed to install the skill file",
        });
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_mentions_all_commands() {
        for text in [USAGE.zh, USAGE.en] {
            for want in [
                "termlet ls",
                "termlet exec",
                "termlet upload",
                "termlet download",
                "termlet skill install",
                "termlet mcp",
                "termlet help",
                "4 ",
                "7 ",
            ] {
                assert!(text.contains(want), "用法缺少 {want}");
            }
        }
    }

    // 每个错误码都有中英文说明
    #[test]
    fn every_code_has_both_languages() {
        for code in ErrorCode::ALL {
            let m = human(code.as_str()).unwrap_or_else(|| panic!("{code} 没有说明"));
            assert!(!m.zh.is_empty() && !m.en.is_empty(), "{code}");
        }
        assert!(human(CODE_SKILL_INSTALL_FAILED).is_some());
        assert!(human("something_new").is_none());
    }
}
