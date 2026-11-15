//! MCP 服务（`termlet mcp`）在 AI 工具中的配置：设置的「命令行工具」页给出 Claude Code、Claude Desktop、Cursor、
//! Codex 的配置片段。服务本身在命令行工具中（`termlet-cli` 的 `mcp` 模块），只有 stdio 传输。

/// 配置中的服务名（AI 工具中显示为 `termlet`，Claude Code 的工具名因此是 `mcp__termlet__…`）。
pub const SERVER_NAME: &str = "termlet";

/// 启动 MCP 服务的子命令。
pub const SUBCOMMAND: &str = "mcp";

/// Codex 中工具调用的超时（秒）：默认 60 秒，等用户批准危险命令（最多 60 秒）再执行时会超时。
pub const CODEX_TOOL_TIMEOUT_SECS: u32 = 300;

/// 给出配置的 AI 工具。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AiTool {
    ClaudeCode,
    ClaudeDesktop,
    Cursor,
    Codex,
}

impl AiTool {
    /// 界面上的顺序。
    pub const ALL: [AiTool; 4] = [
        AiTool::ClaudeCode,
        AiTool::ClaudeDesktop,
        AiTool::Cursor,
        AiTool::Codex,
    ];

    /// 稳定的标识（界面元素的 ID 用它）。
    pub fn id(self) -> &'static str {
        match self {
            AiTool::ClaudeCode => "claude-code",
            AiTool::ClaudeDesktop => "claude-desktop",
            AiTool::Cursor => "cursor",
            AiTool::Codex => "codex",
        }
    }

    /// 产品名（不翻译）。
    pub fn name(self) -> &'static str {
        match self {
            AiTool::ClaudeCode => "Claude Code",
            AiTool::ClaudeDesktop => "Claude Desktop",
            AiTool::Cursor => "Cursor",
            AiTool::Codex => "Codex",
        }
    }

    /// 配置片段。`command` 是命令行工具的路径（或在 PATH 中时的 `termlet`）；`windows` 决定命令行中路径的写法。
    /// Claude Code 是一条命令，Claude Desktop 与 Cursor 是 JSON（`mcpServers`），Codex 是 TOML（`mcp_servers`）。
    pub fn snippet(self, command: &str, windows: bool) -> String {
        match self {
            AiTool::ClaudeCode => format!(
                "claude mcp add --scope user {SERVER_NAME} -- {} {SUBCOMMAND}",
                shell_word(command, windows)
            ),
            // 手写缩进（`args` 写在一行上，比 to_string_pretty 短）；路径按 JSON 转义
            AiTool::ClaudeDesktop | AiTool::Cursor => format!(
                "{{\n  \"mcpServers\": {{\n    \"{SERVER_NAME}\": {{\n      \"command\": {},\n      \"args\": [\"{SUBCOMMAND}\"]\n    }}\n  }}\n}}",
                serde_json::Value::from(command)
            ),
            AiTool::Codex => format!(
                "[mcp_servers.{SERVER_NAME}]\ncommand = {}\nargs = [\"{SUBCOMMAND}\"]\ntool_timeout_sec = {CODEX_TOOL_TIMEOUT_SECS}",
                toml_string(command)
            ),
        }
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

    #[test]
    fn claude_code_command() {
        assert_eq!(
            AiTool::ClaudeCode.snippet(WIN, true),
            format!("claude mcp add --scope user termlet -- {WIN} mcp")
        );
        assert_eq!(
            AiTool::ClaudeCode.snippet(r"D:\My Apps\Termlet\bin\termlet.exe", true),
            r#"claude mcp add --scope user termlet -- "D:\My Apps\Termlet\bin\termlet.exe" mcp"#
        );
        assert_eq!(
            AiTool::ClaudeCode.snippet("/usr/bin/termlet", false),
            "claude mcp add --scope user termlet -- /usr/bin/termlet mcp"
        );
        assert_eq!(
            AiTool::ClaudeCode.snippet(
                "/Users/o'neil/Termlet.app/Contents/MacOS/bin/termlet",
                false
            ),
            r"claude mcp add --scope user termlet -- '/Users/o'\''neil/Termlet.app/Contents/MacOS/bin/termlet' mcp"
        );
        assert_eq!(
            AiTool::ClaudeCode.snippet("termlet", true),
            "claude mcp add --scope user termlet -- termlet mcp"
        );
    }

    // JSON 片段是合法的 JSON，路径中的反斜杠转义
    #[test]
    fn json_snippets() {
        for tool in [AiTool::ClaudeDesktop, AiTool::Cursor] {
            let text = tool.snippet(WIN, true);
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(value["mcpServers"]["termlet"]["command"], WIN);
            assert_eq!(
                value["mcpServers"]["termlet"]["args"],
                serde_json::json!(["mcp"])
            );
            assert!(text.contains(r"C:\\Users"), "{text}");
            assert!(text.contains("\n      \"args\": [\"mcp\"]\n"), "{text}");
        }
        // 引号与控制字符同样转义
        let odd = AiTool::Cursor.snippet("/opt/a \"b\"\tc/termlet", false);
        let value: serde_json::Value = serde_json::from_str(&odd).unwrap();
        assert_eq!(
            value["mcpServers"]["termlet"]["command"],
            "/opt/a \"b\"\tc/termlet"
        );
    }

    #[test]
    fn codex_toml() {
        assert_eq!(
            AiTool::Codex.snippet(WIN, true),
            format!(
                "[mcp_servers.termlet]\ncommand = '{WIN}'\nargs = [\"mcp\"]\ntool_timeout_sec = 300"
            )
        );
        assert_eq!(toml_string("/opt/it's/termlet"), r#""/opt/it's/termlet""#);
        assert_eq!(toml_string("a\"b\\c'"), r#""a\"b\\c'""#);
    }

    #[test]
    fn ids_are_distinct() {
        let ids: std::collections::HashSet<_> = AiTool::ALL.iter().map(|t| t.id()).collect();
        assert_eq!(ids.len(), AiTool::ALL.len());
    }
}
