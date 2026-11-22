//! 「AI 与自动化」页的 MCP 服务：AI 工具按配置自己启动 `termlet mcp`（stdio，不开端口）。在下拉框中选 AI 工具，给出
//! 配置片段与复制按钮（有 `mcp add` 命令的工具另给配置文件的写法）；片段中的命令是这台电脑上命令行工具的完整路径
//! （AI 工具从图形界面启动时不一定读到新的 PATH），Linux 的 deb、rpm 是 `/usr/bin/termlet`。找不到命令行工具时（开发构建
//! 没有编译它、AppImage 要先链接为 termlet）写 `termlet`，并说明它要在 PATH 中。

use std::path::PathBuf;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectEvent};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    App, ClipboardItem, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Subscription, TestSupportExt as _, Window, div,
    prelude::FluentBuilder as _, rems,
};
use termlet_core::proto::mcp::{AiTool, CODEX_TOOL_TIMEOUT_SECS, Snippet, SnippetForm};
use termlet_core::userpath;

use super::CliEnv;
use crate::choice::{Choice, ChoiceState, choice_state, chosen};
use crate::install_kind::{LinuxInstall, SYSTEM_CLI, linux_install};
use crate::prose::prose;
use crate::t;

/// 配置中启动 MCP 服务的命令。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum McpCommand {
    /// 命令行工具的完整路径
    Path(PathBuf),
    /// 只写 `termlet`：要在 PATH 中
    InPath,
}

impl McpCommand {
    /// 这台电脑上的命令行工具。
    pub(super) fn find(env: &CliEnv) -> Self {
        let Some(exe) = env.exe.as_deref() else {
            return Self::InPath;
        };
        match linux_install(exe, env.system_link.as_deref(), env.appimage.as_deref()) {
            Some(LinuxInstall::System) => return Self::Path(PathBuf::from(SYSTEM_CLI)),
            // AppImage 链接为 termlet 才是命令行工具（按调用的名字区分）
            Some(LinuxInstall::AppImage(_)) => return Self::InPath,
            None => {}
        }
        let cli = userpath::cli_path(exe);
        if cli.is_file() {
            return Self::Path(cli);
        }
        // 开发构建：cargo 把 termlet 编译在 termlet-desktop 的旁边（安装版中同一目录的 termlet.exe 就是 Termlet.exe 本身）
        let dev = exe.with_file_name(format!("termlet{}", std::env::consts::EXE_SUFFIX));
        if exe
            .file_stem()
            .is_some_and(|stem| stem == "termlet-desktop")
            && dev.is_file()
        {
            return Self::Path(dev);
        }
        Self::InPath
    }

    fn text(&self) -> String {
        match self {
            McpCommand::Path(path) => path.display().to_string(),
            McpCommand::InPath => "termlet".into(),
        }
    }
}

type ToolEvent = SelectEvent<SearchableVec<Choice<AiTool>>>;

pub(super) struct McpSection {
    command: McpCommand,
    /// 选中的 AI 工具（下拉框）
    pub(super) tools: Entity<ChoiceState<AiTool>>,
    _subscription: Subscription,
}

/// AI 工具在下拉框与提示中的名称。
fn tool_label(tool: AiTool) -> SharedString {
    match tool.name() {
        Some(name) => name.into(),
        None => t!("cli_ui.mcp.other"),
    }
}

/// 片段放在哪里、怎么生效。
fn where_text(tool: AiTool, form: SnippetForm) -> SharedString {
    match (tool, form) {
        (AiTool::ClaudeCode, SnippetForm::Command) => t!("cli_ui.mcp.where_claude_code"),
        (AiTool::ClaudeCode, SnippetForm::File) => t!("cli_ui.mcp.where_claude_code_file"),
        (AiTool::Codex, SnippetForm::File) => {
            t!("cli_ui.mcp.where_codex", seconds = CODEX_TOOL_TIMEOUT_SECS)
        }
        (AiTool::Codex, SnippetForm::Command) => t!(
            "cli_ui.mcp.where_codex_command",
            seconds = CODEX_TOOL_TIMEOUT_SECS
        ),
        (AiTool::Cursor, _) => t!("cli_ui.mcp.where_cursor"),
        (AiTool::VsCode, _) => t!("cli_ui.mcp.where_vscode"),
        (AiTool::GeminiCli, SnippetForm::Command) => t!("cli_ui.mcp.where_gemini"),
        (AiTool::GeminiCli, SnippetForm::File) => t!("cli_ui.mcp.where_gemini_file"),
        (AiTool::ClaudeDesktop, _) => t!("cli_ui.mcp.where_claude_desktop"),
        (AiTool::Other, _) => t!("cli_ui.mcp.where_other"),
    }
}

impl McpSection {
    pub(super) fn new(env: &CliEnv, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let tools = choice_state(
            AiTool::ALL
                .iter()
                .map(|tool| Choice::new(tool_label(*tool), *tool))
                .collect(),
            &AiTool::ClaudeCode,
            false,
            window,
            cx,
        );
        let subscription = cx.subscribe(&tools, |_, _, _: &ToolEvent, cx| cx.notify());
        Self {
            command: McpCommand::find(env),
            tools,
            _subscription: subscription,
        }
    }

    /// 选中的 AI 工具。
    pub(super) fn selected(&self, cx: &App) -> AiTool {
        chosen(&self.tools, cx).unwrap_or(AiTool::ClaudeCode)
    }

    /// 选中的 AI 工具的配置片段（第一份是推荐的写法）。
    pub(super) fn snippets(&self, cx: &App) -> Vec<Snippet> {
        self.selected(cx)
            .snippets(&self.command.text(), cfg!(windows))
    }

    /// 一份片段：说明、代码与复制按钮。元素的 ID 按写法区分（同一工具中每种写法只有一份）。
    fn render_snippet(tool: AiTool, snippet: Snippet, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, code_bg, radius) = (theme.muted_foreground, theme.muted, theme.radius);
        let mono = theme.mono_font_family.clone();
        let form = snippet.form.id();
        let lines: Vec<String> = snippet.text.lines().map(str::to_string).collect();
        let text = snippet.text;
        v_flex()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(prose(where_text(tool, snippet.form))),
            )
            .child(
                h_flex()
                    .items_start()
                    .gap_2()
                    .child(
                        div()
                            .id(SharedString::from(format!("cli-mcp-snippet-{form}")))
                            .test_support()
                            .flex_1()
                            .min_w_0()
                            .px_2()
                            .py_1()
                            .rounded(radius)
                            .bg(code_bg)
                            .text_xs()
                            .font_family(mono)
                            .children(lines.into_iter().map(|line| div().child(line))),
                    )
                    .child(
                        Button::new(SharedString::from(format!("cli-mcp-copy-{form}")))
                            .outline()
                            .small()
                            .icon(IconName::Copy)
                            .label(t!("cli_ui.mcp.copy"))
                            .on_click(move |_, window, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                                let notice = match tool.name() {
                                    Some(name) => t!("cli_ui.mcp.copied", tool = name),
                                    None => t!("cli_ui.mcp.copied_generic"),
                                };
                                crate::notify::show(
                                    crate::notify::Notice::success(notice),
                                    window,
                                    cx,
                                );
                            }),
                    ),
            )
    }
}

impl Render for McpSection {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let tool = self.selected(cx);
        let snippets = self.snippets(cx);
        // 以后加上 HTTP 时，接入类型（标准输入输出 / HTTP）放在 AI 工具的上面，两种共用这里选中的 AI 工具
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().text_sm().child(t!("cli_ui.mcp.tool")))
                    .child(
                        div()
                            .w(rems(16.))
                            .child(Select::new(&self.tools).small().id("cli-mcp-tool")),
                    ),
            )
            .children(
                snippets
                    .into_iter()
                    .map(|snippet| Self::render_snippet(tool, snippet, cx)),
            )
            .when(self.command == McpCommand::InPath, |this| {
                this.child(
                    div()
                        .id("cli-mcp-in-path")
                        .test_support()
                        .text_xs()
                        .text_color(muted)
                        .child(prose(t!("cli_ui.mcp.in_path"))),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "termlet-cli-ui-mcp-{name}-{}",
            termlet_core::id::random_token(8)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // 命令：安装版、便携版用 bin 中的命令行工具；deb、rpm 用 /usr/bin/termlet；AppImage 与没有命令行工具时写 termlet；
    // 开发构建用 termlet-desktop 旁边的 termlet
    #[test]
    fn command_follows_the_installation() {
        let dir = temp_dir("find");
        let suffix = std::env::consts::EXE_SUFFIX;
        let app = dir.join(format!("Termlet{suffix}"));
        let none = CliEnv::at(Some(app.clone()), None);
        assert_eq!(McpCommand::find(&none), McpCommand::InPath);
        assert_eq!(
            McpCommand::find(&CliEnv::at(None, None)),
            McpCommand::InPath
        );
        let cli = userpath::cli_path(&app);
        std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
        std::fs::write(&cli, b"").unwrap();
        assert_eq!(McpCommand::find(&none), McpCommand::Path(cli.clone()));

        let opt = PathBuf::from("/opt/termlet/Termlet");
        let deb =
            CliEnv::at(Some(opt.clone()), None).with_linux(Some(userpath::cli_path(&opt)), None);
        assert_eq!(
            McpCommand::find(&deb),
            McpCommand::Path(PathBuf::from("/usr/bin/termlet"))
        );
        let image = CliEnv::at(Some(opt), None)
            .with_linux(None, Some(PathBuf::from("/home/u/termlet.AppImage")));
        assert_eq!(McpCommand::find(&image), McpCommand::InPath);

        let target = dir.join("debug");
        std::fs::create_dir_all(&target).unwrap();
        let desktop = target.join(format!("termlet-desktop{suffix}"));
        let dev = CliEnv::at(Some(desktop), None);
        assert_eq!(McpCommand::find(&dev), McpCommand::InPath);
        let built = target.join(format!("termlet{suffix}"));
        std::fs::write(&built, b"").unwrap();
        assert_eq!(McpCommand::find(&dev), McpCommand::Path(built));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
