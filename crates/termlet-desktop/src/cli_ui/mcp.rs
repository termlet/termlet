//! 「命令行工具」页的 MCP 服务：AI 工具按配置自己启动 `termlet mcp`（stdio，不开端口）。按 AI 工具（Claude Code、
//! Claude Desktop、Cursor、Codex）给出配置片段与复制按钮；片段中的命令是这台电脑上命令行工具的完整路径（AI 工具从图形界面
//! 启动时不一定读到新的 PATH），Linux 的 deb、rpm 是 `/usr/bin/termlet`。找不到命令行工具时（开发构建没有编译它、AppImage
//! 要先链接为 termlet）写 `termlet`，并说明它要在 PATH 中。

use std::path::PathBuf;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    ClipboardItem, Context, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, TestSupportExt as _, Window, div, prelude::FluentBuilder as _,
};
use termlet_core::proto::mcp::{AiTool, CODEX_TOOL_TIMEOUT_SECS};
use termlet_core::userpath;

use super::CliEnv;
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

pub(super) struct McpSection {
    command: McpCommand,
    pub(super) selected: AiTool,
}

/// Claude Desktop 的配置文件（Linux 上没有官方版本，按社区版的位置写）。
fn claude_desktop_config() -> &'static str {
    if cfg!(windows) {
        r"%APPDATA%\Claude\claude_desktop_config.json"
    } else if cfg!(target_os = "macos") {
        "~/Library/Application Support/Claude/claude_desktop_config.json"
    } else {
        "~/.config/Claude/claude_desktop_config.json"
    }
}

/// 片段放在哪里、怎么生效。
fn where_text(tool: AiTool) -> SharedString {
    match tool {
        AiTool::ClaudeCode => t!("cli_ui.mcp.where_claude_code"),
        AiTool::ClaudeDesktop => {
            t!(
                "cli_ui.mcp.where_claude_desktop",
                path = claude_desktop_config()
            )
        }
        AiTool::Cursor => t!("cli_ui.mcp.where_cursor"),
        AiTool::Codex => t!("cli_ui.mcp.where_codex", seconds = CODEX_TOOL_TIMEOUT_SECS),
    }
}

impl McpSection {
    pub(super) fn new(env: &CliEnv) -> Self {
        Self {
            command: McpCommand::find(env),
            selected: AiTool::ClaudeCode,
        }
    }

    /// 选中的 AI 工具的配置片段。
    pub(super) fn snippet(&self) -> String {
        self.selected.snippet(&self.command.text(), cfg!(windows))
    }
}

impl Render for McpSection {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, code_bg, radius) = (theme.muted_foreground, theme.muted, theme.radius);
        let mono = theme.mono_font_family.clone();
        let selected = self.selected;
        let snippet = self.snippet();
        let lines: Vec<String> = snippet.lines().map(str::to_string).collect();
        v_flex()
            .gap_2()
            .child(
                h_flex().child(
                    TabBar::new("cli-mcp-tools")
                        .segmented()
                        .small()
                        .selected_index(
                            AiTool::ALL
                                .iter()
                                .position(|tool| *tool == selected)
                                .unwrap_or_default(),
                        )
                        .on_click(cx.listener(|this, ix: &usize, _, cx| {
                            if let Some(tool) = AiTool::ALL.get(*ix) {
                                this.selected = *tool;
                                cx.notify();
                            }
                        }))
                        .children(AiTool::ALL.map(|tool| Tab::new().label(tool.name()))),
                ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(prose(where_text(selected))),
            )
            .child(
                h_flex()
                    .items_start()
                    .gap_2()
                    .child(
                        div()
                            .id("cli-mcp-snippet")
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
                        Button::new("cli-mcp-copy")
                            .outline()
                            .small()
                            .icon(IconName::Copy)
                            .label(t!("cli_ui.mcp.copy"))
                            .on_click(move |_, window, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(snippet.clone()));
                                crate::notify::show(
                                    crate::notify::Notice::success(t!(
                                        "cli_ui.mcp.copied",
                                        tool = selected.name()
                                    )),
                                    window,
                                    cx,
                                );
                            }),
                    ),
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
