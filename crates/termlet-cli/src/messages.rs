//! 人类可读文本的中英文对照。JSON 输出不使用这里的文本（错误的 `message` 除外：服务端没有补充说明时用错误码的说明）。

use crate::i18n::Msg;

// 与 MCP 服务共用的文本（工具结果中的说明与命令行的输出相同）
pub use termlet_mcp::text::{CODE_USAGE, DOWNLOADED, NO_HOSTS, PROTOCOL, UPLOADED, VIA_SCP};

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

// 运行结果
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

/// 命令行工具自己产生的错误码（不经过 Termlet）。
pub const CODE_SKILL_INSTALL_FAILED: &str = "skill_install_failed";

/// 错误码对应的说明，文本模式下显示在服务端补充的说明前面：Termlet 返回的错误码与 MCP 服务共用一张表，
/// 这里补上命令行工具自己的。
pub fn human(code: &str) -> Option<Msg> {
    if code == CODE_SKILL_INSTALL_FAILED {
        return Some(Msg {
            zh: "安装技能文件失败",
            en: "failed to install the skill file",
        });
    }
    termlet_mcp::text::human(code)
}

#[cfg(test)]
mod tests {
    use termlet_proto::ErrorCode;

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
