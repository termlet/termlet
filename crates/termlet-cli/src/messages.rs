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
  termlet exec <主机> --script <文件> [-- 参数…]   本机脚本经标准输入交给远程的 sh（按首行 #! 选 bash 等），可传参数
  termlet exec <主机> --input <文件> \"<命令>\"     本机文件作为远程命令的标准输入
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

批量执行（exec、upload、download 把 <主机> 换成下面的选项；每台新建连接，同时最多 10 台）:
  -g <分组>          分组（含子分组，可写路径 生产/web），可写多次
  -t <标签>          标签，可写多次；与 -g 同用时取交集
  -H <主机,…>        主机的 ID 或名称，逗号分隔；-H @文件 从文件读（每行一个）
  --all              全部允许的主机
  -x <主机,…>        排除
  --list             只列出会选中、会跳过的主机，不执行
  -j <数量>          同时执行的主机数（默认 10，最多 64）
  --timeout <秒>     每台命令的时限（不含连接），超时发送 SIGTERM
  --max-fail <数量>  失败达到这么多台后不再开始新的主机
  --no-group         按主机逐台输出（默认把输出相同的主机合并成一组）
  --output-dir <目录>  每台写 <主机名>.out / .err，另写 summary.json 与 retry.txt（-H @retry.txt 只重跑没成功的）
  --json-lines       每台结束时输出一行 JSON，最后一行是汇总
  批量下载时 <本地路径> 是目录，每台放进以主机名命名的子目录。执行中按 Ctrl+C 列出还没有结束的主机，再按一次取消全部。

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
  7  AI 工具与脚本的访问已关闭（在 Termlet 设置中的「AI 与自动化」中开启）
  8  批量执行：有主机没有成功（失败、连不上、超时、取消），详情看输出
  其他  exec 连接成功后透传远程命令的退出码

语言: 跟随 LC_ALL / LC_MESSAGES / LANG 或系统界面语言，可用 TERMLET_LANG=en|zh 指定。
",
    en: "termlet — run remote commands and transfer files via hosts saved in Termlet

Usage:
  termlet ls [-q <keyword>]                       List hosts that allow CLI access (no credentials)
  termlet exec <host> \"<command>\"                 Run a remote command (new connection each time, stateless)
  termlet exec <host> --stdin                     Read the command from stdin (avoids shell escaping)
  termlet exec <host> --script <file> [-- args]   Feed a local script to the remote sh over stdin (the #! line picks
                                                  bash and the like), with arguments
  termlet exec <host> --input <file> \"<command>\"  Use a local file as the remote command's stdin
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

Batch runs (exec, upload and download take these options instead of <host>; a new connection per host, 10 at a time):
  -g <group>         Host group (subgroups included; paths such as prod/web work); repeatable
  -t <tag>           Tag; repeatable; combined with -g, a host must match both
  -H <host,...>      Host IDs or names, comma-separated; -H @file reads them from a file (one per line)
  --all              Every allowed host
  -x <host,...>      Exclude hosts
  --list             Only list the hosts that would run or be skipped
  -j <count>         Hosts at a time (default 10, at most 64)
  --timeout <sec>    Time limit per host (connection not included); SIGTERM when exceeded
  --max-fail <count> Stop starting new hosts after this many failures
  --no-group         Print each host separately (by default hosts with the same output are grouped)
  --output-dir <dir> Write <host>.out / .err per host, plus summary.json and retry.txt (-H @retry.txt reruns the rest)
  --json-lines       Print one JSON line per host as it finishes, then a summary line
  For a batch download <local> is a directory with one subdirectory per host. While running, Ctrl+C lists the hosts
  still running; press it again to cancel them all.

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
  7  access from AI tools and scripts is turned off (turn it on in Termlet under Settings → AI & automation)
  8  batch run: some hosts didn't succeed (failed, unreachable, timed out or canceled); see the output
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
    "用法: termlet exec [--terminal] <主机> \"<命令>\" 或 termlet exec [--terminal] <主机> --stdin；批量：termlet exec -g <分组>|-t <标签>|-H <主机,…>|--all \"<命令>\"（termlet help 查看全部选项）",
    "usage: termlet exec [--terminal] <host> \"<command>\" or termlet exec [--terminal] <host> --stdin; batch: termlet exec -g <group>|-t <tag>|-H <host,...>|--all \"<command>\" (termlet help lists all options)"
);
msg!(
    USAGE_UPLOAD,
    "用法: termlet upload <主机> <本地文件> <远程路径>；批量：termlet upload -g <分组>|-t <标签>|-H <主机,…>|--all <本地文件> <远程路径>",
    "usage: termlet upload <host> <local-file> <remote-path>; batch: termlet upload -g <group>|-t <tag>|-H <host,...>|--all <local-file> <remote-path>"
);
msg!(
    USAGE_DOWNLOAD,
    "用法: termlet download <主机> <远程文件> <本地路径>；批量：termlet download -g <分组>|-t <标签>|-H <主机,…>|--all <远程文件> <本地目录>",
    "usage: termlet download <host> <remote-file> <local-path>; batch: termlet download -g <group>|-t <tag>|-H <host,...>|--all <remote-file> <local-dir>"
);
msg!(FLAG_NEEDS_VALUE, "{} 后面要跟一个值", "{} needs a value");
msg!(
    FLAG_NUMBER,
    "{} 后面要跟正整数，而不是 {}",
    "{} needs a positive integer, not {}"
);
msg!(
    BATCH_ONLY_FLAG,
    "{} 只用于批量执行（与 -g、-t、-H 或 --all 一起用）",
    "{} is only for batch runs (use it with -g, -t, -H or --all)"
);
msg!(EXEC_ONLY_FLAG, "{} 只用于 exec", "{} is only for exec");
msg!(
    TERMINAL_WITH_INPUT,
    "--terminal 不能与 --script、--input 一起用",
    "--terminal can't be used with --script or --input"
);
msg!(
    TERMINAL_BATCH,
    "--terminal 不能批量执行（批量执行每台新建连接）",
    "--terminal can't be used for batch runs (they open a new connection per host)"
);
msg!(
    INPUT_CONFLICT,
    "--stdin、--script、--input 只能用其中一个",
    "use only one of --stdin, --script and --input"
);
msg!(
    REST_WITHOUT_SCRIPT,
    "-- 之后的参数只能给 --script 的脚本",
    "arguments after -- are only for the --script script"
);
msg!(
    HOSTS_FILE,
    "读不了主机列表文件 {}: {}",
    "cannot read the host list file {}: {}"
);
msg!(READ_FILE, "读不了文件 {}: {}", "cannot read {}: {}");
msg!(
    FILE_TOO_LARGE,
    "{} 超过 1MB，未执行；较大的文件请先 termlet upload 到服务器",
    "{} exceeds 1MB; nothing was run. Upload larger files with termlet upload first"
);
msg!(FILE_NOT_UTF8, "{} 不是 UTF-8 文本", "{} is not UTF-8 text");
msg!(
    SCRIPT_INTERPRETER,
    "--script 只支持 sh 一类的 shell（bash、sh、zsh、dash、ksh、ash），这个脚本的解释器是 {}；其他解释器请用 --input 与自己的命令，如 --input x.py \"python3 -\"",
    "--script only supports sh-like shells (bash, sh, zsh, dash, ksh, ash), but this script's interpreter is {}; for other interpreters use --input with your own command, such as --input x.py \"python3 -\""
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

// 批量执行
msg!(
    BATCH_OLD_APP,
    "这个版本的 Termlet（{}）还不支持批量执行，请更新 Termlet",
    "this version of Termlet ({}) doesn't support batch runs yet; please update Termlet"
);
msg!(BATCH_START, "开始：{}", "Starting: {}");
msg!(
    BATCH_START_SKIPPED,
    "开始：{}（跳过 {} 台）",
    "Starting: {} ({} skipped)"
);
msg!(BATCH_SKIPPED_TITLE, "跳过（{} 台）", "Skipped ({})");
msg!(
    BATCH_UNFINISHED,
    "还没有结束（{} 台）：{}",
    "Still running ({}): {}"
);
msg!(
    BATCH_INTERRUPT_HINT,
    "再按一次 Ctrl+C 取消全部",
    "Press Ctrl+C again to cancel them all"
);
msg!(BATCH_CANCELING, "正在取消……", "Canceling...");
msg!(BATCH_SUMMARY, "汇总：{}（{}）", "Summary: {} ({})");
msg!(BATCH_WROTE, "结果已写入 {}", "Results were written to {}");
msg!(
    BATCH_DIR,
    "不能写入结果目录 {}: {}",
    "cannot write to the result directory {}: {}"
);
msg!(BATCH_HOST_NOTE, "{}：{}", "{}: {}");

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
                "8 ",
                "--max-fail",
                "--output-dir",
                "--script",
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
