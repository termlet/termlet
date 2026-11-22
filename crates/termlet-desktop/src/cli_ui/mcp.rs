//! 「AI 与自动化」页的 MCP 服务。接入类型有两种：标准输入输出（AI 工具按配置自己启动 `termlet mcp`，不开端口，推荐）
//! 与 HTTP（Termlet 在本机监听，AI 工具按地址与令牌连接，见 [`super::http`]）。在下拉框中选 AI 工具，按接入类型给出
//! 配置片段与复制按钮（有 `mcp add` 命令的工具另给配置文件的写法）；两种接入类型共用选中的 AI 工具。
//!
//! 标准输入输出的片段中，命令是这台电脑上命令行工具的完整路径（AI 工具从图形界面启动时不一定读到新的 PATH），Linux 的
//! deb、rpm 是 `/usr/bin/termlet`；找不到命令行工具时（开发构建没有编译它、AppImage 要先链接为 termlet）写 `termlet`，
//! 并说明它要在 PATH 中。HTTP 的片段预览中令牌打码，复制出的是含原文的片段；还不能复制时预览中写占位。监听范围为
//! 局域网时，HTTP 的片段可以选给这台电脑还是局域网中的另一台电脑（这台电脑的局域网地址与局域网令牌）。

use std::path::PathBuf;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectEvent};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, ClipboardItem, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, TestSupportExt as _,
    Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::cli::TokenKind;
use termlet_core::model::{HttpScope, LocalSettings};
use termlet_core::proto::mcp::{AiTool, CODEX_TOOL_TIMEOUT_SECS, Snippet, SnippetForm, Transport};
use termlet_core::userpath;

use super::CliEnv;
use super::http::{HttpPanel, masked_token, row};
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
        match super::dev_cli(exe) {
            Some(dev) => Self::Path(dev),
            None => Self::InPath,
        }
    }

    fn text(&self) -> String {
        match self {
            McpCommand::Path(path) => path.display().to_string(),
            McpCommand::InPath => "termlet".into(),
        }
    }
}

type ToolEvent = SelectEvent<SearchableVec<Choice<AiTool>>>;

/// MCP 的接入类型（界面上的分段选择）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Access {
    Stdio,
    Http,
}

impl Access {
    const ALL: [Access; 2] = [Access::Stdio, Access::Http];
}

/// HTTP 的配置给谁用（监听范围为局域网时可选）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Target {
    /// 这台电脑上的 AI 工具
    Local,
    /// 局域网中的另一台电脑
    Lan,
}

impl Target {
    const ALL: [Target; 2] = [Target::Local, Target::Lan];
}

pub(super) struct McpSection {
    command: McpCommand,
    pub(super) access: Access,
    /// HTTP 的配置给谁用（选的；监听范围不是局域网时按这台电脑）
    pub(super) target: Target,
    /// 选中的 AI 工具（下拉框，两种接入类型共用）
    pub(super) tools: Entity<ChoiceState<AiTool>>,
    pub(super) http: Entity<HttpPanel>,
    _subscriptions: Vec<Subscription>,
}

/// AI 工具在下拉框与提示中的名称。
fn tool_label(tool: AiTool) -> SharedString {
    match tool.name() {
        Some(name) => name.into(),
        None => t!("cli_ui.mcp.other"),
    }
}

/// 片段放在哪里、怎么生效。带令牌的 HTTP 片段都指向用户级的配置。
fn where_text(tool: AiTool, form: SnippetForm, access: Access) -> SharedString {
    match (access, tool, form) {
        // HTTP 只有配置文件
        (_, AiTool::Codex, SnippetForm::File) | (Access::Http, AiTool::Codex, _) => {
            t!("cli_ui.mcp.where_codex", seconds = CODEX_TOOL_TIMEOUT_SECS)
        }
        (_, AiTool::GeminiCli, SnippetForm::File) => t!("cli_ui.mcp.where_gemini_file"),
        (Access::Stdio, AiTool::ClaudeCode, SnippetForm::Command) => {
            t!("cli_ui.mcp.where_claude_code")
        }
        (Access::Stdio, AiTool::ClaudeCode, SnippetForm::File) => {
            t!("cli_ui.mcp.where_claude_code_file")
        }
        (Access::Stdio, AiTool::Codex, SnippetForm::Command) => t!(
            "cli_ui.mcp.where_codex_command",
            seconds = CODEX_TOOL_TIMEOUT_SECS
        ),
        (Access::Stdio, AiTool::Cursor, _) => t!("cli_ui.mcp.where_cursor"),
        (Access::Stdio, AiTool::VsCode, _) => t!("cli_ui.mcp.where_vscode"),
        (Access::Stdio, AiTool::GeminiCli, SnippetForm::Command) => t!("cli_ui.mcp.where_gemini"),
        (Access::Stdio, AiTool::ClaudeDesktop, _) => t!("cli_ui.mcp.where_claude_desktop"),
        (Access::Stdio, AiTool::Other, _) => t!("cli_ui.mcp.where_other"),
        (Access::Http, AiTool::ClaudeCode, _) => t!("cli_ui.mcp.http_where_claude_code"),
        (Access::Http, AiTool::Cursor, _) => t!("cli_ui.mcp.http_where_cursor"),
        (Access::Http, AiTool::VsCode, _) => t!("cli_ui.mcp.http_where_vscode"),
        (Access::Http, AiTool::GeminiCli, SnippetForm::Command) => {
            t!("cli_ui.mcp.http_where_gemini")
        }
        // 没有 HTTP 的片段，界面上只写这一句
        (Access::Http, AiTool::ClaudeDesktop, _) => t!("cli_ui.mcp.http_claude_desktop"),
        (Access::Http, AiTool::Other, _) => t!("cli_ui.mcp.http_where_other"),
    }
}

impl McpSection {
    pub(super) fn new(
        env: &CliEnv,
        local: &LocalSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
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
        let http = cx.new(|cx| {
            HttpPanel::new(
                local.cli.enabled,
                &local.cli.http,
                env.lan_addresses.clone(),
                window,
                cx,
            )
        });
        let subscriptions = vec![
            cx.subscribe(&tools, |_, _, _: &ToolEvent, cx| cx.notify()),
            // 令牌、端口、状态变了：片段随之改变
            cx.observe(&http, |_, _, cx| cx.notify()),
        ];
        Self {
            command: McpCommand::find(env),
            // 打开过 HTTP 的直接显示它
            access: if local.cli.http.enabled {
                Access::Http
            } else {
                Access::Stdio
            },
            target: Target::Local,
            tools,
            http,
            _subscriptions: subscriptions,
        }
    }

    /// HTTP 的配置实际给谁用：草稿的监听范围不是局域网时只能给这台电脑。
    pub(super) fn target(&self, cx: &App) -> Target {
        match self.http.read(cx).scope {
            HttpScope::Lan => self.target,
            HttpScope::Local => Target::Local,
        }
    }

    /// 选中的 AI 工具。
    pub(super) fn selected(&self, cx: &App) -> AiTool {
        chosen(&self.tools, cx).unwrap_or(AiTool::ClaudeCode)
    }

    /// 选中的 AI 工具的配置片段（第一份是推荐的写法）：界面上显示的与复制的。HTTP 的预览中令牌打码（还不能复制时写
    /// 占位），复制的含原文；还不能复制时复制的一份为 `None`。
    pub(super) fn snippets(&self, cx: &App) -> Vec<(Snippet, Option<String>)> {
        let tool = self.selected(cx);
        let windows = cfg!(windows);
        match self.access {
            Access::Stdio => {
                let command = self.command.text();
                tool.snippets(Transport::Stdio { command: &command }, windows)
                    .into_iter()
                    .map(|snippet| {
                        let text = snippet.text.clone();
                        (snippet, Some(text))
                    })
                    .collect()
            }
            Access::Http => {
                let http = self.http.read(cx);
                let (url, kind) = match self.target(cx) {
                    Target::Local => (http.url(), TokenKind::Local),
                    Target::Lan => match http.lan_url(cx) {
                        Some(url) => (url, TokenKind::Lan),
                        // 这台电脑没有局域网地址：没有可给的配置
                        None => return Vec::new(),
                    },
                };
                let real = http.token_for_copy(kind);
                let shown = match real {
                    Some(_) => masked_token(kind),
                    None => t!("cli_ui.http.token_placeholder").to_string(),
                };
                let preview = tool.snippets(
                    Transport::Http {
                        url: &url,
                        token: &shown,
                    },
                    windows,
                );
                let copies: Vec<Option<String>> = match &real {
                    Some(token) => tool
                        .snippets(Transport::Http { url: &url, token }, windows)
                        .into_iter()
                        .map(|s| Some(s.text))
                        .collect(),
                    None => vec![None; preview.len()],
                };
                preview.into_iter().zip(copies).collect()
            }
        }
    }

    /// 一份片段：说明、代码与复制按钮。元素的 ID 按写法区分（同一工具中每种写法只有一份）。
    fn render_snippet(
        tool: AiTool,
        access: Access,
        target: Target,
        snippet: Snippet,
        copy: Option<String>,
        cx: &App,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, code_bg, radius) = (theme.muted_foreground, theme.muted, theme.radius);
        let mono = theme.mono_font_family.clone();
        let form = snippet.form.id();
        let lines: Vec<String> = snippet.text.lines().map(str::to_string).collect();
        let notice = match (tool.name(), access, target) {
            (Some(name), Access::Stdio, _) => t!("cli_ui.mcp.copied", tool = name),
            (None, Access::Stdio, _) => t!("cli_ui.mcp.copied_generic"),
            (Some(name), Access::Http, Target::Local) => {
                t!("cli_ui.mcp.copied_with_token", tool = name)
            }
            (None, Access::Http, Target::Local) => t!("cli_ui.mcp.copied_generic_with_token"),
            (Some(name), Access::Http, Target::Lan) => t!("cli_ui.mcp.copied_lan", tool = name),
            (None, Access::Http, Target::Lan) => t!("cli_ui.mcp.copied_generic_lan"),
        };
        v_flex()
            .gap_1()
            .child(div().text_xs().text_color(muted).child(prose(where_text(
                tool,
                snippet.form,
                access,
            ))))
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
                            .disabled(copy.is_none())
                            .on_click(move |_, window, cx| {
                                let Some(text) = copy.clone() else {
                                    return;
                                };
                                cx.write_to_clipboard(ClipboardItem::new_string(text));
                                crate::notify::show(
                                    crate::notify::Notice::success(notice.clone()),
                                    window,
                                    cx,
                                );
                            }),
                    ),
            )
    }

    /// 一行说明（灰色小字）。
    fn note(id: &'static str, text: SharedString, cx: &App) -> impl IntoElement {
        div()
            .id(id)
            .test_support()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(prose(text))
    }
}

impl Render for McpSection {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let tool = self.selected(cx);
        let access = self.access;
        let lan_scope = self.http.read(cx).scope == HttpScope::Lan;
        let target = self.target(cx);
        let snippets = self.snippets(cx);
        let kind = match target {
            Target::Local => TokenKind::Local,
            Target::Lan => TokenKind::Lan,
        };
        let copy_blocked = match access {
            Access::Http if !snippets.is_empty() => self.http.read(cx).copy_blocked(kind),
            _ => None,
        };
        // 给另一台电脑而这台电脑没有局域网地址（Claude Desktop 另有说明）
        let no_lan_address = access == Access::Http
            && target == Target::Lan
            && tool != AiTool::ClaudeDesktop
            && snippets.is_empty();
        let lan_ip = self.http.read(cx).lan_ip(cx);
        v_flex()
            .gap_2()
            .child(row(
                t!("cli_ui.mcp.access"),
                muted,
                h_flex().child(
                    TabBar::new("cli-mcp-access")
                        .segmented()
                        .small()
                        .selected_index(
                            Access::ALL
                                .iter()
                                .position(|a| *a == access)
                                .unwrap_or_default(),
                        )
                        .on_click(cx.listener(|this, ix: &usize, _, cx| {
                            if let Some(access) = Access::ALL.get(*ix) {
                                this.access = *access;
                                cx.notify();
                            }
                        }))
                        .child(Tab::new().label(t!("cli_ui.mcp.access_stdio")))
                        .child(Tab::new().label(t!("cli_ui.mcp.access_http"))),
                ),
            ))
            .when(access == Access::Http, |this| {
                this.child(
                    div()
                        .pl_3()
                        .py_1()
                        .border_l_2()
                        .border_color(cx.theme().border)
                        .child(self.http.clone()),
                )
            })
            .child(row(
                t!("cli_ui.mcp.tool"),
                muted,
                div()
                    .w(rems(16.))
                    .child(Select::new(&self.tools).small().id("cli-mcp-tool")),
            ))
            .when(access == Access::Http && lan_scope, |this| {
                this.child(row(
                    t!("cli_ui.mcp.target"),
                    muted,
                    h_flex().id("cli-mcp-target-row").test_support().child(
                        TabBar::new("cli-mcp-target")
                            .segmented()
                            .small()
                            .selected_index(
                                Target::ALL
                                    .iter()
                                    .position(|t| *t == target)
                                    .unwrap_or_default(),
                            )
                            .on_click(cx.listener(|this, ix: &usize, _, cx| {
                                if let Some(target) = Target::ALL.get(*ix) {
                                    this.target = *target;
                                    cx.notify();
                                }
                            }))
                            .child(Tab::new().label(t!("cli_ui.mcp.target_local")))
                            .child(Tab::new().label(t!("cli_ui.mcp.target_lan"))),
                    ),
                ))
            })
            .when(
                access == Access::Http && target == Target::Lan && !snippets.is_empty(),
                |this| {
                    this.child(Self::note(
                        "cli-mcp-lan-target",
                        t!("cli_ui.mcp.lan_target_note"),
                        cx,
                    ))
                },
            )
            .when(no_lan_address, |this| {
                this.child(Self::note(
                    "cli-mcp-no-lan-address",
                    t!("cli_ui.http.lan_none"),
                    cx,
                ))
            })
            .when(snippets.is_empty() && !no_lan_address, |this| {
                this.child(Self::note(
                    "cli-mcp-no-http",
                    t!("cli_ui.mcp.http_claude_desktop"),
                    cx,
                ))
            })
            .children(snippets.into_iter().map(|(snippet, copy)| {
                Self::render_snippet(tool, access, target, snippet, copy, cx)
            }))
            .when_some(copy_blocked, |this, text| {
                this.child(Self::note("cli-mcp-copy-later", text, cx))
            })
            // Codex：它自己还会逐次确认（怎样省掉、意味着什么）；HTTP 还要绕开系统代理
            .when(tool == AiTool::Codex, |this| {
                this.child(Self::note(
                    "cli-mcp-codex-approval",
                    t!("cli_ui.mcp.codex_approval"),
                    cx,
                ))
            })
            .when(
                tool == AiTool::Codex && access == Access::Http && target == Target::Local,
                |this| {
                    this.child(Self::note(
                        "cli-mcp-codex-proxy",
                        t!("cli_ui.mcp.http_codex_proxy"),
                        cx,
                    ))
                },
            )
            .when_some(
                lan_ip.filter(|_| {
                    tool == AiTool::Codex && access == Access::Http && target == Target::Lan
                }),
                |this, ip| {
                    this.child(Self::note(
                        "cli-mcp-codex-proxy",
                        t!("cli_ui.mcp.lan_codex_proxy", address = ip.to_string()),
                        cx,
                    ))
                },
            )
            .when(
                access == Access::Stdio && self.command == McpCommand::InPath,
                |this| this.child(Self::note("cli-mcp-in-path", t!("cli_ui.mcp.in_path"), cx)),
            )
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
