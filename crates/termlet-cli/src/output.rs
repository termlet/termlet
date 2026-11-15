//! 输出：标准输出是终端时为可读文本，被管道或程序捕获时为 JSON（字段名与错误码固定英文）。
//! 文本模式的错误写到标准错误，JSON 模式的错误写到标准输出（程序读取的是标准输出）。

use std::io::Write;

use serde::Serialize;
use termlet_proto::{ErrorBody, ExitCode};
use unicode_width::UnicodeWidthStr;

use crate::i18n::Lang;
use crate::messages;

pub struct Output<'a> {
    pub json: bool,
    pub lang: Lang,
    pub stdout: &'a mut dyn Write,
    pub stderr: &'a mut dyn Write,
}

/// JSON 模式下的错误。
#[derive(Serialize)]
struct ErrorOut<'a> {
    error: &'a str,
    message: &'a str,
}

impl Output<'_> {
    /// 输出 JSON（一行），返回成功的退出码。`<` `>` `&` 不转义。
    pub fn print_json(&mut self, value: &impl Serialize) -> i32 {
        let _ = serde_json::to_writer(&mut *self.stdout, value);
        let _ = writeln!(self.stdout);
        ExitCode::Success.into()
    }

    /// 输出错误并返回退出码。JSON 模式写到标准输出：`message` 是服务端的补充说明，没有时是错误码的说明。
    pub fn fail(&mut self, exit: ExitCode, code: &str, message: &str) -> i32 {
        if self.json {
            let fallback;
            let message = if message.is_empty() {
                fallback = messages::human(code)
                    .map(|m| m.get(self.lang))
                    .unwrap_or_default();
                fallback
            } else {
                message
            };
            let _ = serde_json::to_writer(
                &mut *self.stdout,
                &ErrorOut {
                    error: code,
                    message,
                },
            );
            let _ = writeln!(self.stdout);
        } else {
            let _ = writeln!(
                self.stderr,
                "termlet: {}",
                human_text(self.lang, code, message)
            );
        }
        exit.into()
    }

    /// Termlet 返回的错误：退出码按错误码。
    pub fn api_fail(&mut self, body: &ErrorBody) -> i32 {
        self.fail(ExitCode::for_error(&body.code), &body.code, &body.message)
    }

    /// 用法错误（退出码 1）。
    pub fn usage_error(&mut self, message: &str) -> i32 {
        self.fail(ExitCode::Usage, messages::CODE_USAGE, message)
    }
}

/// 错误的可读说明：错误码的本地化说明 + 服务端的补充说明（如命中的规则）。
pub fn human_text(lang: Lang, code: &str, message: &str) -> String {
    let text = messages::human(code)
        .map(|m| m.get(lang).to_string())
        .unwrap_or_else(|| code.to_string());
    if message.is_empty() {
        text
    } else if code == messages::CODE_USAGE {
        message.to_string()
    } else {
        format!("{text}: {message}")
    }
}

/// 按显示宽度对齐的表格（与 tabwriter 相同：每列补齐到最宽的一格再加两个空格，最后一列不补）。
pub fn table(rows: &[Vec<String>]) -> String {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0; columns];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.width());
        }
    }
    let mut out = String::new();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            out.push_str(cell);
            if i + 1 < row.len() {
                out.push_str(&" ".repeat(widths[i] - cell.width() + 2));
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// 测试用的输出：返回（输出，标准输出，标准错误）。
    pub struct Captured {
        pub stdout: Vec<u8>,
        pub stderr: Vec<u8>,
    }

    impl Captured {
        pub fn new() -> Self {
            Self {
                stdout: Vec::new(),
                stderr: Vec::new(),
            }
        }

        pub fn output(&mut self, json: bool, lang: Lang) -> Output<'_> {
            Output {
                json,
                lang,
                stdout: &mut self.stdout,
                stderr: &mut self.stderr,
            }
        }

        pub fn stdout(&self) -> String {
            String::from_utf8_lossy(&self.stdout).into_owned()
        }

        pub fn stderr(&self) -> String {
            String::from_utf8_lossy(&self.stderr).into_owned()
        }
    }

    #[test]
    fn blocked_command_text() {
        let mut c = Captured::new();
        let code = c
            .output(false, Lang::Zh)
            .fail(ExitCode::Blocked, "command_blocked", "rm -rf /");
        assert_eq!(code, 4);
        let text = c.stderr();
        for want in ["危险命令规则", "「命令行工具」", "rm -rf /"] {
            assert!(text.contains(want), "拦截提示缺少 {want}: {text}");
        }
        let mut c = Captured::new();
        c.output(false, Lang::En)
            .fail(ExitCode::Blocked, "command_blocked", "rm -rf /");
        assert!(c.stderr().contains("dangerous-command rule") && c.stderr().contains("rm -rf /"));
    }

    // JSON 的字段名与错误码不受语言影响，不写标准错误
    #[test]
    fn json_errors() {
        for lang in [Lang::Zh, Lang::En] {
            let mut c = Captured::new();
            c.output(true, lang)
                .fail(ExitCode::Blocked, "command_blocked", "rule");
            let body: serde_json::Value = serde_json::from_slice(&c.stdout).unwrap();
            assert_eq!(
                body,
                serde_json::json!({"error": "command_blocked", "message": "rule"})
            );
            assert!(c.stderr.is_empty());
        }
        // 没有补充说明时 message 是错误码的说明（告诉 AI 去哪里开启）
        let mut c = Captured::new();
        c.output(true, Lang::Zh)
            .fail(ExitCode::CliDisabled, "cli_disabled", "");
        let body: serde_json::Value = serde_json::from_slice(&c.stdout).unwrap();
        assert!(body["message"].as_str().unwrap().contains("命令行工具"));
    }

    #[test]
    fn usage_error_text() {
        let mut c = Captured::new();
        c.output(false, Lang::En)
            .usage_error(messages::USAGE_LS.get(Lang::En));
        assert_eq!(c.stderr(), "termlet: usage: termlet ls [-q <keyword>]\n");
    }

    #[test]
    fn unknown_codes_are_shown_as_is() {
        assert_eq!(human_text(Lang::Zh, "brand_new", ""), "brand_new");
        assert_eq!(human_text(Lang::Zh, "brand_new", "x"), "brand_new: x");
        assert_eq!(
            human_text(Lang::Zh, "cli_disabled", ""),
            messages::human("cli_disabled").unwrap().zh
        );
    }

    #[test]
    fn table_aligns_by_display_width() {
        let rows = vec![
            vec!["ID".into(), "名称".into(), "地址".into()],
            vec!["h_1".into(), "web".into(), "10.0.0.5:22".into()],
        ];
        assert_eq!(table(&rows), "ID   名称  地址\nh_1  web   10.0.0.5:22\n");
    }
}
