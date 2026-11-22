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

/// Codex 中工具调用的超时（秒）：默认 60 秒，等用户批准危险命令（最多 60 秒）再执行时会超时。
pub const CODEX_TOOL_TIMEOUT_SECS: u32 = 300;

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

    /// 标准输入输出的配置片段，第一份是推荐的写法。有 `mcp add` 命令的（Claude Code、Codex、Gemini CLI）另给配置文件
    /// 的写法：Codex 推荐配置文件（命令不能放宽超时），另两个推荐命令。`command` 是命令行工具的路径（或在 PATH 中时的
    /// `termlet`）；`windows` 决定命令中路径的写法。
    ///
    /// JSON 都是 `mcpServers` 下的一项 `termlet`：Claude Code 的 `.mcp.json` 与 VS Code 写明 `"type": "stdio"`，
    /// VS Code 另写 `"tools": ["*"]`（文档的写法）；Codex 是 TOML（`mcp_servers`），带放宽的工具超时。
    pub fn snippets(self, command: &str, windows: bool) -> Vec<Snippet> {
        let word = shell_word(command, windows);
        let run = |text: String| Snippet {
            form: SnippetForm::Command,
            text,
        };
        let file = |text: String| Snippet {
            form: SnippetForm::File,
            text,
        };
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
}

/// `{"mcpServers": {"termlet": {…}}}`：手写缩进，数组写在一行上（比 to_string_pretty 短）；字符串按 JSON 转义。
fn json_entry(fields: &[(&str, serde_json::Value)]) -> String {
    let body = fields
        .iter()
        .map(|(key, value)| format!("      \"{key}\": {value}"))
        .collect::<Vec<_>>()
        .join(",\n");
    format!("{{\n  \"mcpServers\": {{\n    \"{SERVER_NAME}\": {{\n{body}\n    }}\n  }}\n}}")
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

    fn forms(tool: AiTool) -> Vec<SnippetForm> {
        tool.snippets(WIN, true).iter().map(|s| s.form).collect()
    }

    fn text(tool: AiTool, form: SnippetForm, command: &str, windows: bool) -> String {
        tool.snippets(command, windows)
            .into_iter()
            .find(|s| s.form == form)
            .unwrap()
            .text
    }

    fn json(tool: AiTool, command: &str) -> serde_json::Value {
        let text = text(tool, SnippetForm::File, command, cfg!(windows));
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{tool:?}：{e}\n{text}"))
    }

    // 各工具给哪几种写法、哪种在前；同一工具中每种写法只有一份（界面按写法区分元素）
    #[test]
    fn forms_per_tool() {
        use SnippetForm::*;
        assert_eq!(forms(AiTool::ClaudeCode), [Command, File]);
        assert_eq!(forms(AiTool::Codex), [File, Command]);
        assert_eq!(forms(AiTool::GeminiCli), [Command, File]);
        for tool in [
            AiTool::Cursor,
            AiTool::VsCode,
            AiTool::ClaudeDesktop,
            AiTool::Other,
        ] {
            assert_eq!(forms(tool), [File], "{tool:?}");
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
                "[mcp_servers.termlet]\ncommand = '{WIN}'\nargs = [\"mcp\"]\ntool_timeout_sec = 300"
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
            assert_eq!(entry["tool_timeout_sec"].as_integer(), Some(300));
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
