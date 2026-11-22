//! MCP 服务（`termlet mcp`）在 AI 工具中的配置：设置的「AI 与自动化」页按 AI 工具（Claude Code、Codex、Cursor、
//! VS Code、Gemini CLI、Claude Desktop，以及通用的 JSON）给出配置片段。协议的分发与工具在 `termlet-mcp` 中，
//! 两种传输共用：stdio 在命令行工具中（`termlet-cli` 的 `mcp` 模块），HTTP 由应用监听（`termlet-core` 的 `cli::http`）。

/// 配置中的服务名（AI 工具中显示为 `termlet`，Claude Code 的工具名因此是 `mcp__termlet__…`）。
pub const SERVER_NAME: &str = "termlet";

/// 启动 MCP 服务的子命令。
pub const SUBCOMMAND: &str = "mcp";

/// MCP 服务写在请求的调用方最前面的一项：`mcp:<客户端名>`（客户端自报的名字），客户端没有报名字时只有 `mcp`。
/// 审计日志据此把 MCP 的调用与命令行分开。
pub const CALLER_MARKER: &str = "mcp";

/// 经 HTTP 的 MCP 调用在调用方中紧跟 [`CALLER_MARKER`] 的一项（`["mcp:<客户端名>", "http", <进程链…>]`），
/// 审计据此写明传输方式；stdio 的调用没有这一项。
pub const CALLER_HTTP: &str = "http";

/// 局域网中的另一台电脑经 HTTP 的调用：紧跟 [`CALLER_HTTP`] 的一项是 `lan:<对方的 IP>`（查不到那边的进程，
/// 没有进程链）：`["mcp:<客户端名>", "http", "lan:<IP>"]`。
pub const CALLER_LAN: &str = "lan";

/// Codex 中工具调用的超时（秒）：一条命令（含等用户批准危险命令的最多 60 秒）最长能运行多久。Codex 默认 60 秒，
/// 等批准再执行时就会超时；这是总时长，进度通知不能延长它，所以放宽到 15 分钟。
pub const CODEX_TOOL_TIMEOUT_SECS: u32 = 900;

/// HTTP 接入的路径（只有这一个端点）。
pub const HTTP_PATH: &str = "/mcp";

/// HTTP 接入的地址。应用只在 IPv4 的回环地址上监听，所以写 `127.0.0.1` 而不写 `localhost`（有的运行时会先把
/// localhost 解析成 `::1`）。
pub fn http_url(port: u16) -> String {
    http_url_at("127.0.0.1", port)
}

/// 局域网中的另一台电脑用的地址：这台电脑在局域网中的 IPv4 地址（应用在局域网上只监听 IPv4）。
pub fn http_url_at(host: &str, port: u16) -> String {
    format!("http://{host}:{port}{HTTP_PATH}")
}

/// 配置片段连到 MCP 服务的方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport<'a> {
    /// 标准输入输出：AI 工具自己启动 `<command> mcp`（`command` 是命令行工具的路径，或在 PATH 中时的 `termlet`）
    Stdio { command: &'a str },
    /// HTTP：应用监听的地址与令牌（界面预览时传打码的令牌，复制时传原文）
    Http { url: &'a str, token: &'a str },
}

/// 给出配置的 AI 工具。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AiTool {
    ClaudeCode,
    Codex,
    Cursor,
    /// VS Code 的 GitHub Copilot（用户级的 `~/.copilot/mcp-config.json`）
    VsCode,
    GeminiCli,
    ClaudeDesktop,
    /// 其他 AI 工具：多数认的 `mcpServers` 写法
    Other,
}

/// 配置片段的写法。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SnippetForm {
    /// 在终端中运行的命令（AI 工具自己的 `mcp add`）
    Command,
    /// 加到配置文件中的内容（JSON，Codex 是 TOML）
    File,
}

impl SnippetForm {
    /// 稳定的标识（界面元素的 ID 用它）。
    pub fn id(self) -> &'static str {
        match self {
            SnippetForm::Command => "command",
            SnippetForm::File => "file",
        }
    }
}

/// 一份配置片段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snippet {
    pub form: SnippetForm,
    pub text: String,
}

impl AiTool {
    /// 界面上的顺序。
    pub const ALL: [AiTool; 7] = [
        AiTool::ClaudeCode,
        AiTool::Codex,
        AiTool::Cursor,
        AiTool::VsCode,
        AiTool::GeminiCli,
        AiTool::ClaudeDesktop,
        AiTool::Other,
    ];

    /// 稳定的标识（界面元素的 ID 用它）。
    pub fn id(self) -> &'static str {
        match self {
            AiTool::ClaudeCode => "claude-code",
            AiTool::Codex => "codex",
            AiTool::Cursor => "cursor",
            AiTool::VsCode => "vscode",
            AiTool::GeminiCli => "gemini-cli",
            AiTool::ClaudeDesktop => "claude-desktop",
            AiTool::Other => "other",
        }
    }

    /// 产品名（不翻译）；“其他”没有，由界面按语言写。
    pub fn name(self) -> Option<&'static str> {
        Some(match self {
            AiTool::ClaudeCode => "Claude Code",
            AiTool::Codex => "Codex",
            AiTool::Cursor => "Cursor",
            AiTool::VsCode => "VS Code (GitHub Copilot)",
            AiTool::GeminiCli => "Gemini CLI",
            AiTool::ClaudeDesktop => "Claude Desktop",
            AiTool::Other => return None,
        })
    }

    /// 配置片段，第一份是推荐的写法；`windows` 决定命令中参数的引号。JSON 都是 `mcpServers` 下的一项 `termlet`，
    /// Codex 是 TOML（`mcp_servers`），带放宽的工具超时。
    pub fn snippets(self, transport: Transport<'_>, windows: bool) -> Vec<Snippet> {
        match transport {
            Transport::Stdio { command } => self.stdio(command, windows),
            Transport::Http { url, token } => self.http(url, token, windows),
        }
    }

    /// 标准输入输出。有 `mcp add` 命令的（Claude Code、Codex、Gemini CLI）另给配置文件的写法：Codex 推荐配置文件
    /// （命令不能放宽超时），另两个推荐命令。Claude Code 的 `.mcp.json` 与 VS Code 写明 `"type": "stdio"`，VS Code
    /// 另写 `"tools": ["*"]`（文档的写法）。
    fn stdio(self, command: &str, windows: bool) -> Vec<Snippet> {
        let word = shell_word(command, windows);
        let args = || serde_json::json!([SUBCOMMAND]);
        let plain = || {
            json_entry(&[
                ("command", serde_json::Value::from(command)),
                ("args", args()),
            ])
        };
        match self {
            AiTool::ClaudeCode => vec![
                run(format!(
                    "claude mcp add --scope user {SERVER_NAME} -- {word} {SUBCOMMAND}"
                )),
                file(json_entry(&[
                    ("type", "stdio".into()),
                    ("command", command.into()),
                    ("args", args()),
                ])),
            ],
            AiTool::Codex => vec![
                file(format!(
                    "[mcp_servers.{SERVER_NAME}]\ncommand = {}\nargs = [\"{SUBCOMMAND}\"]\ntool_timeout_sec = {CODEX_TOOL_TIMEOUT_SECS}",
                    toml_string(command)
                )),
                run(format!(
                    "codex mcp add {SERVER_NAME} -- {word} {SUBCOMMAND}"
                )),
            ],
            AiTool::VsCode => vec![file(json_entry(&[
                ("type", "stdio".into()),
                ("command", command.into()),
                ("args", args()),
                ("tools", serde_json::json!(["*"])),
            ]))],
            // 不加 -s user 时写进当前项目
            AiTool::GeminiCli => vec![
                run(format!(
                    "gemini mcp add -s user {SERVER_NAME} {word} {SUBCOMMAND}"
                )),
                file(plain()),
            ],
            AiTool::Cursor | AiTool::ClaudeDesktop | AiTool::Other => vec![file(plain())],
        }
    }

    /// HTTP：令牌放在 `Authorization: Bearer` 请求头中。带令牌的配置只写到用户级的文件（不进项目、不会被提交）：
    /// Claude Code 只给 `--scope user` 的命令，Gemini CLI 的命令带 `-s user`。各家的写法：
    ///
    /// - Claude Code：`claude mcp add --transport http … --header`；
    /// - Codex：`url` 与 `http_headers`（`codex mcp add` 只能从环境变量读令牌，不给命令）；
    /// - Cursor：`url` 与 `headers`，不写类型；
    /// - VS Code（Copilot）：`"type": "http"`，另写 `"tools": ["*"]`；
    /// - Gemini CLI：`gemini mcp add --transport http -H`，配置文件写 `"type": "http"` 与 `url`（与命令写入的相同；
    ///   旧写法 `httpUrl` 已标为弃用）；
    /// - 通用的 JSON：`"type": "http"`、`url`、`headers`；
    /// - Claude Desktop 没有：它的自定义连接器从 Anthropic 的服务器发起连接，连不到这台电脑上的地址。
    fn http(self, url: &str, token: &str, windows: bool) -> Vec<Snippet> {
        let bearer = format!("Bearer {token}");
        let header = shell_word(&format!("Authorization: {bearer}"), windows);
        let address = shell_word(url, windows);
        let headers = || serde_json::json!({ "Authorization": bearer });
        let typed = || {
            json_entry(&[
                ("type", "http".into()),
                ("url", url.into()),
                ("headers", headers()),
            ])
        };
        match self {
            AiTool::ClaudeCode => vec![run(format!(
                "claude mcp add --transport http --scope user {SERVER_NAME} {address} --header {header}"
            ))],
            AiTool::Codex => vec![file(format!(
                "[mcp_servers.{SERVER_NAME}]\nurl = {}\nhttp_headers = {{ Authorization = {} }}\ntool_timeout_sec = {CODEX_TOOL_TIMEOUT_SECS}",
                toml_string(url),
                toml_string(&bearer)
            ))],
            AiTool::Cursor => vec![file(json_entry(&[
                ("url", url.into()),
                ("headers", headers()),
            ]))],
            AiTool::VsCode => vec![file(json_entry(&[
                ("type", "http".into()),
                ("url", url.into()),
                ("headers", headers()),
                ("tools", serde_json::json!(["*"])),
            ]))],
            AiTool::GeminiCli => vec![
                run(format!(
                    "gemini mcp add -s user --transport http -H {header} {SERVER_NAME} {address}"
                )),
                file(typed()),
            ],
            AiTool::ClaudeDesktop => Vec::new(),
            AiTool::Other => vec![file(typed())],
        }
    }
}

fn run(text: String) -> Snippet {
    Snippet {
        form: SnippetForm::Command,
        text,
    }
}

fn file(text: String) -> Snippet {
    Snippet {
        form: SnippetForm::File,
        text,
    }
}

/// `{"mcpServers": {"termlet": {…}}}`：手写缩进，数组与对象写在一行上（比 to_string_pretty 短）；字符串按 JSON 转义。
fn json_entry(fields: &[(&str, serde_json::Value)]) -> String {
    let body = fields
        .iter()
        .map(|(key, value)| format!("      \"{key}\": {}", inline(value)))
        .collect::<Vec<_>>()
        .join(",\n");
    format!("{{\n  \"mcpServers\": {{\n    \"{SERVER_NAME}\": {{\n{body}\n    }}\n  }}\n}}")
}

/// 一行内的 JSON：对象写成 `{ "键": 值 }`（比紧凑的写法好读），其余照 serde_json 的紧凑写法。
fn inline(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) if !map.is_empty() => {
            let items: Vec<String> = map
                .iter()
                .map(|(key, value)| {
                    format!(
                        "{}: {}",
                        serde_json::Value::from(key.as_str()),
                        inline(value)
                    )
                })
                .collect();
            format!("{{ {} }}", items.join(", "))
        }
        other => other.to_string(),
    }
}

/// 命令行中的一个参数：只有常见的路径字符时原样写，否则加引号。Windows（PowerShell、cmd）用双引号，
/// 其他平台用单引号（其中的单引号写成 `'\''`）。
fn shell_word(text: &str, windows: bool) -> String {
    let plain =
        |c: char| c.is_ascii_alphanumeric() || "/._-+:@".contains(c) || (windows && c == '\\');
    if !text.is_empty() && text.chars().all(plain) {
        text.to_string()
    } else if windows {
        format!("\"{text}\"")
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

/// TOML 字符串：没有单引号与控制字符时用字面字符串（Windows 路径中的反斜杠不用转义），否则用转义的基本字符串。
fn toml_string(text: &str) -> String {
    if !text.contains('\'') && !text.chars().any(char::is_control) {
        return format!("'{text}'");
    }
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIN: &str = r"C:\Users\Ruan\AppData\Local\Programs\Termlet\bin\termlet.exe";
    const SPACED: &str = r"D:\My Apps\Termlet\bin\termlet.exe";
    const MAC: &str = "/Users/o'neil/Termlet.app/Contents/MacOS/bin/termlet";
    /// 引号、制表符：JSON 与 TOML 都要转义
    const ODD: &str = "/opt/a \"b\"\tc/it's/termlet";

    const URL: &str = "http://127.0.0.1:47800/mcp";
    /// 测试用的假令牌（比真令牌短，gitleaks 的规则认不出它）
    const TOKEN: &str = "termlet_mcp_testtoken234567";
    /// 界面预览中打码的令牌
    const MASKED: &str = "termlet_mcp_••••••••";
    /// 引号、反斜杠、制表符：JSON 与 TOML 都要转义
    const ODD_TOKEN: &str = "a\"b\\c'd\te";

    fn stdio(command: &str) -> Transport<'_> {
        Transport::Stdio { command }
    }

    fn http(token: &str) -> Transport<'_> {
        Transport::Http { url: URL, token }
    }

    fn forms(tool: AiTool, transport: Transport<'_>) -> Vec<SnippetForm> {
        tool.snippets(transport, true)
            .iter()
            .map(|s| s.form)
            .collect()
    }

    fn snippet(tool: AiTool, form: SnippetForm, transport: Transport<'_>, windows: bool) -> String {
        tool.snippets(transport, windows)
            .into_iter()
            .find(|s| s.form == form)
            .unwrap()
            .text
    }

    fn text(tool: AiTool, form: SnippetForm, command: &str, windows: bool) -> String {
        snippet(tool, form, stdio(command), windows)
    }

    fn json(tool: AiTool, command: &str) -> serde_json::Value {
        let text = text(tool, SnippetForm::File, command, cfg!(windows));
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{tool:?}：{e}\n{text}"))
    }

    // 各工具给哪几种写法、哪种在前；同一工具中每种写法只有一份（界面按写法区分元素）
    #[test]
    fn forms_per_tool() {
        use SnippetForm::*;
        let local = stdio(WIN);
        assert_eq!(forms(AiTool::ClaudeCode, local), [Command, File]);
        assert_eq!(forms(AiTool::Codex, local), [File, Command]);
        assert_eq!(forms(AiTool::GeminiCli, local), [Command, File]);
        for tool in [
            AiTool::Cursor,
            AiTool::VsCode,
            AiTool::ClaudeDesktop,
            AiTool::Other,
        ] {
            assert_eq!(forms(tool, local), [File], "{tool:?}");
        }
        // HTTP：Claude Code 只给用户级的命令（不给项目的 .mcp.json），Codex 只给配置文件，Claude Desktop 没有
        let remote = http(TOKEN);
        assert_eq!(forms(AiTool::ClaudeCode, remote), [Command]);
        assert_eq!(forms(AiTool::Codex, remote), [File]);
        assert_eq!(forms(AiTool::GeminiCli, remote), [Command, File]);
        assert!(forms(AiTool::ClaudeDesktop, remote).is_empty());
        for tool in [AiTool::Cursor, AiTool::VsCode, AiTool::Other] {
            assert_eq!(forms(tool, remote), [File], "{tool:?}");
        }
    }

    #[test]
    fn http_address() {
        assert_eq!(http_url(47800), URL);
        assert_eq!(http_url(1), "http://127.0.0.1:1/mcp");
        assert_eq!(
            http_url_at("192.168.1.5", 47800),
            "http://192.168.1.5:47800/mcp"
        );
    }

    // HTTP 的命令：Claude Code 写进用户级（--scope user），Gemini CLI 带 -s user；请求头按系统加引号
    #[test]
    fn http_commands() {
        assert_eq!(
            snippet(AiTool::ClaudeCode, SnippetForm::Command, http(TOKEN), true),
            format!(
                "claude mcp add --transport http --scope user termlet {URL} --header \"Authorization: Bearer {TOKEN}\""
            )
        );
        assert_eq!(
            snippet(AiTool::ClaudeCode, SnippetForm::Command, http(TOKEN), false),
            format!(
                "claude mcp add --transport http --scope user termlet {URL} --header 'Authorization: Bearer {TOKEN}'"
            )
        );
        assert_eq!(
            snippet(AiTool::GeminiCli, SnippetForm::Command, http(TOKEN), true),
            format!(
                "gemini mcp add -s user --transport http -H \"Authorization: Bearer {TOKEN}\" termlet {URL}"
            )
        );
    }

    // HTTP 的 JSON：地址、Bearer 令牌（含要转义的字符）原样读回；各家的类型字段
    #[test]
    fn http_json() {
        for token in [TOKEN, MASKED, ODD_TOKEN] {
            for (tool, kind) in [
                (AiTool::Cursor, None),
                (AiTool::VsCode, Some("http")),
                (AiTool::GeminiCli, Some("http")),
                (AiTool::Other, Some("http")),
            ] {
                let text = snippet(tool, SnippetForm::File, http(token), cfg!(windows));
                let value: serde_json::Value =
                    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{tool:?}：{e}\n{text}"));
                assert_eq!(value.as_object().unwrap().len(), 1, "{text}");
                let entry = &value["mcpServers"]["termlet"];
                assert_eq!(entry["url"], URL);
                assert_eq!(
                    entry["headers"],
                    serde_json::json!({ "Authorization": format!("Bearer {token}") })
                );
                assert_eq!(entry.get("type").and_then(|t| t.as_str()), kind, "{tool:?}");
                assert!(entry.get("command").is_none() && entry.get("httpUrl").is_none());
                if tool == AiTool::VsCode {
                    assert_eq!(entry["tools"], serde_json::json!(["*"]));
                }
            }
        }
        // 对象写在一行上
        let text = snippet(AiTool::Cursor, SnippetForm::File, http(TOKEN), true);
        assert!(
            text.contains(&format!(
                "\n      \"headers\": {{ \"Authorization\": \"Bearer {TOKEN}\" }}\n"
            )),
            "{text}"
        );
    }

    // Codex 的 HTTP：url 与 http_headers，照样放宽工具超时
    #[test]
    fn http_codex_toml() {
        assert_eq!(
            snippet(AiTool::Codex, SnippetForm::File, http(TOKEN), true),
            format!(
                "[mcp_servers.termlet]\nurl = '{URL}'\nhttp_headers = {{ Authorization = 'Bearer {TOKEN}' }}\ntool_timeout_sec = 900"
            )
        );
        for token in [TOKEN, MASKED, ODD_TOKEN] {
            let text = snippet(AiTool::Codex, SnippetForm::File, http(token), true);
            let table: toml::Table = text.parse().unwrap_or_else(|e| panic!("{e}\n{text}"));
            let entry = table["mcp_servers"]["termlet"].as_table().unwrap();
            assert_eq!(entry["url"].as_str(), Some(URL));
            assert_eq!(
                entry["http_headers"]["Authorization"].as_str(),
                Some(format!("Bearer {token}").as_str())
            );
            assert_eq!(entry["tool_timeout_sec"].as_integer(), Some(900));
            assert_eq!(entry.len(), 3, "{text}");
        }
    }

    // 预览（打码）与复制（原文）的片段只在令牌处不同
    #[test]
    fn masked_snippets_differ_only_in_the_token() {
        for tool in AiTool::ALL {
            for windows in [true, false] {
                let masked = tool.snippets(http(MASKED), windows);
                let real = tool.snippets(http(TOKEN), windows);
                assert_eq!(masked.len(), real.len());
                for (masked, real) in masked.iter().zip(&real) {
                    assert_eq!(masked.form, real.form);
                    assert_eq!(real.text.matches(TOKEN).count(), 1, "{}", real.text);
                    assert!(!masked.text.contains(TOKEN));
                    assert_eq!(masked.text.replace(MASKED, TOKEN), real.text);
                }
            }
        }
    }

    #[test]
    fn claude_code_command() {
        let command =
            |path: &str, windows| text(AiTool::ClaudeCode, SnippetForm::Command, path, windows);
        assert_eq!(
            command(WIN, true),
            format!("claude mcp add --scope user termlet -- {WIN} mcp")
        );
        assert_eq!(
            command(SPACED, true),
            r#"claude mcp add --scope user termlet -- "D:\My Apps\Termlet\bin\termlet.exe" mcp"#
        );
        assert_eq!(
            command("/usr/bin/termlet", false),
            "claude mcp add --scope user termlet -- /usr/bin/termlet mcp"
        );
        assert_eq!(
            command(MAC, false),
            r"claude mcp add --scope user termlet -- '/Users/o'\''neil/Termlet.app/Contents/MacOS/bin/termlet' mcp"
        );
        assert_eq!(
            command("termlet", true),
            "claude mcp add --scope user termlet -- termlet mcp"
        );
    }

    // Claude Code 的 .mcp.json：写明 stdio
    #[test]
    fn claude_code_json() {
        for path in [WIN, SPACED, MAC, ODD] {
            let value = json(AiTool::ClaudeCode, path);
            let entry = &value["mcpServers"]["termlet"];
            assert_eq!(entry["type"], "stdio");
            assert_eq!(entry["command"], path);
            assert_eq!(entry["args"], serde_json::json!(["mcp"]));
            assert_eq!(entry.as_object().unwrap().len(), 3, "{entry}");
        }
    }

    // Codex：config.toml 中的 [mcp_servers.termlet]，放宽工具超时；命令是 codex mcp add
    #[test]
    fn codex_toml_and_command() {
        assert_eq!(
            text(AiTool::Codex, SnippetForm::File, WIN, true),
            format!(
                "[mcp_servers.termlet]\ncommand = '{WIN}'\nargs = [\"mcp\"]\ntool_timeout_sec = 900"
            )
        );
        for path in [WIN, SPACED, MAC, ODD] {
            let text = text(AiTool::Codex, SnippetForm::File, path, cfg!(windows));
            let table: toml::Table = text.parse().unwrap_or_else(|e| panic!("{e}\n{text}"));
            assert_eq!(table.len(), 1, "{text}");
            let entry = table["mcp_servers"]["termlet"].as_table().unwrap();
            assert_eq!(entry["command"].as_str(), Some(path));
            assert_eq!(
                entry["args"].as_array().unwrap(),
                &vec![toml::Value::from("mcp")]
            );
            assert_eq!(entry["tool_timeout_sec"].as_integer(), Some(900));
            assert_eq!(entry.len(), 3, "{text}");
        }
        assert_eq!(toml_string("/opt/it's/termlet"), r#""/opt/it's/termlet""#);
        assert_eq!(toml_string("a\"b\\c'"), r#""a\"b\\c'""#);
        assert_eq!(
            text(AiTool::Codex, SnippetForm::Command, SPACED, true),
            r#"codex mcp add termlet -- "D:\My Apps\Termlet\bin\termlet.exe" mcp"#
        );
    }

    // VS Code（GitHub Copilot）：~/.copilot/mcp-config.json 的 mcpServers（不是旧的 servers），写明 stdio 与 tools
    #[test]
    fn vscode_json() {
        for path in [WIN, SPACED, MAC, ODD] {
            let value = json(AiTool::VsCode, path);
            assert_eq!(value.as_object().unwrap().len(), 1, "{value}");
            let entry = &value["mcpServers"]["termlet"];
            assert_eq!(entry["type"], "stdio");
            assert_eq!(entry["command"], path);
            assert_eq!(entry["args"], serde_json::json!(["mcp"]));
            assert_eq!(entry["tools"], serde_json::json!(["*"]));
        }
    }

    // Gemini CLI：命令写进用户级（-s user），参数直接跟在命令后面；配置文件是 settings.json 的 mcpServers
    #[test]
    fn gemini_cli_command_and_json() {
        assert_eq!(
            text(AiTool::GeminiCli, SnippetForm::Command, WIN, true),
            format!("gemini mcp add -s user termlet {WIN} mcp")
        );
        assert_eq!(
            text(AiTool::GeminiCli, SnippetForm::Command, MAC, false),
            r"gemini mcp add -s user termlet '/Users/o'\''neil/Termlet.app/Contents/MacOS/bin/termlet' mcp"
        );
        for path in [WIN, SPACED, MAC, ODD] {
            let entry = &json(AiTool::GeminiCli, path)["mcpServers"]["termlet"];
            assert_eq!(entry["command"], path);
            assert_eq!(entry["args"], serde_json::json!(["mcp"]));
        }
    }

    // Cursor、Claude Desktop 与通用的 JSON：mcpServers 中只有 command 与 args；路径中的反斜杠、引号与控制字符转义
    #[test]
    fn plain_json() {
        for tool in [AiTool::Cursor, AiTool::ClaudeDesktop, AiTool::Other] {
            let text = text(tool, SnippetForm::File, WIN, true);
            assert!(text.contains(r"C:\\Users"), "{text}");
            assert!(text.contains("\n      \"args\": [\"mcp\"]\n"), "{text}");
            for path in [WIN, SPACED, MAC, ODD] {
                let value = json(tool, path);
                let entry = &value["mcpServers"]["termlet"];
                assert_eq!(entry["command"], path, "{tool:?}");
                assert_eq!(entry["args"], serde_json::json!(["mcp"]));
                assert_eq!(entry.as_object().unwrap().len(), 2, "{entry}");
            }
        }
    }

    #[test]
    fn ids_are_distinct() {
        let ids: std::collections::HashSet<_> = AiTool::ALL.iter().map(|t| t.id()).collect();
        assert_eq!(ids.len(), AiTool::ALL.len());
        assert_eq!(
            AiTool::ALL.iter().filter(|t| t.name().is_none()).count(),
            1,
            "只有“其他”没有产品名"
        );
    }
}
