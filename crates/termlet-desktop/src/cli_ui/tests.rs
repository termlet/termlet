//! 「AI 与自动化」页与审计对话框的 UI 集成测试：在无头窗口中渲染，经真实的事件路径操作并检查结果。
//! 应用程序与用户主目录指向临时目录：不碰真实的 PATH 与 AI 工具的目录。

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Context, Entity, IntoElement, ParentElement as _,
    Render, Styled as _, TestAppContext, Window, WindowBounds, WindowOptions, div, px, size,
};
use termlet_core::audit::{AuditAction, AuditEntry, AuditLog, AuditStatus};
use termlet_core::cli::http::lan::LanAddress;
use termlet_core::cli::{HttpError, HttpStatus, TokenKind, TokenView};
use termlet_core::model::{Approval, CliHttpSettings, HttpScope, LocalSettings, Settings};
use termlet_core::proto::skill::{self, Agent, Status};

use super::audit::{self, AuditView, PAGE, SEARCH_DELAY};
use super::http::TokenState;
use super::mcp::{Access, McpSection, Target};
use super::page::CliPage;
use super::tool::PathState;
use super::{CliEnv, refresh_open_audit};
use crate::app_state::AppState;
use crate::testkit::poll_until;

/// 只放设置页（或什么也不放、只用来开对话框）的视图。
struct Harness {
    page: Option<Entity<CliPage>>,
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().children(self.page.clone())
    }
}

/// 临时目录（测试结束时删除）。
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "termlet-cli-ui-{name}-{}",
            termlet_core::id::random_token(8)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn setup(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    cx.update(gpui_kit::init);
    cx.update(crate::init);
    cx.update(AppState::install_in_memory);
    // 对话框的入场动画按真实时间推进，动画中元素的位置在变，点击会偶尔落空
    cx.update(|cx| cx.set_reduce_motion(true));
}

/// 打开一个窗口；`page` 为真时放上设置页（窗口足够高，整页不用滚动）。
fn open(
    cx: &mut TestAppContext,
    page: Option<(Settings, LocalSettings, CliEnv)>,
) -> (AnyWindowHandle, Option<Entity<CliPage>>) {
    setup(cx);
    let (handle, harness) = cx.update(|cx| {
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    size(px(1024.), px(3200.)),
                ))),
                ..Default::default()
            },
            cx,
            |window, cx| {
                cx.new(|cx| Harness {
                    // 默认收起的两节（接入方式、内置规则）展开着：这里测的是它们里面的内容
                    page: page.map(|(settings, local, env)| {
                        cx.new(|cx| {
                            let mut page = CliPage::with_env(&settings, &local, env, window, cx);
                            page.access_open = true;
                            page.builtin_open = true;
                            page
                        })
                    }),
                })
            },
        )
        .expect("打开测试窗口")
    });
    cx.run_until_parked();
    let page = cx.read(|cx| harness.read(cx).page.clone());
    (handle, page)
}

fn frame(cx: &mut TestAppContext, handle: AnyWindowHandle) {
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| window.render_frame(cx))
        .unwrap();
}

// Linux 的安装方式：从 AppImage 运行时按 AppImage（显示 AppImage 文件本身）；/usr/bin/termlet 指向这个程序旁边的
// 命令行工具时是 deb、rpm；指向别处（例如以前手动链接的旧位置）、没有这个链接时照原来（tar.gz 等）
#[test]
fn linux_install_follows_the_package() {
    use std::path::Path;

    use crate::install_kind::{LinuxInstall, linux_install};
    let exe = Path::new("/opt/termlet/Termlet");
    let cli = termlet_core::userpath::cli_path(exe);
    assert_eq!(
        linux_install(exe, Some(&cli), None),
        Some(LinuxInstall::System)
    );
    let elsewhere = termlet_core::userpath::cli_path(Path::new("/home/u/termlet/Termlet"));
    assert_eq!(linux_install(exe, Some(&elsewhere), None), None);
    assert_eq!(linux_install(exe, None, None), None);
    let image = Path::new("/home/u/Apps/termlet-0.2.0-linux-x64.AppImage");
    let mounted = Path::new("/tmp/.mount_termleAbC123/usr/lib/termlet/Termlet");
    assert_eq!(
        linux_install(mounted, None, Some(image)),
        Some(LinuxInstall::AppImage(image.to_path_buf()))
    );
    assert_eq!(
        linux_install(exe, Some(&cli), Some(image)),
        Some(LinuxInstall::AppImage(image.to_path_buf())),
        "从 AppImage 运行时按 AppImage"
    );
    assert_eq!(
        linux_install(exe, None, Some(Path::new(""))),
        None,
        "空的 APPIMAGE 不算"
    );
}

// deb、rpm：说明命令行工具已在 /usr/bin/termlet，不给手动链接的命令
#[gpui_kit::test]
fn deb_and_rpm_say_the_command_is_installed(cx: &mut TestAppContext) {
    let tmp = TempDir::new("deb");
    let exe = PathBuf::from("/opt/termlet/Termlet");
    let cli = termlet_core::userpath::cli_path(&exe);
    let env = CliEnv::at(Some(exe), Some(tmp.0.join("home"))).with_linux(Some(cli), None);
    let (handle, page) = open(
        cx,
        Some((Settings::default(), LocalSettings::default(), env)),
    );
    let page = page.unwrap();
    frame(cx, handle);
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("cli-tool-system").is_some());
        assert!(window.try_find("cli-tool-path-toggle").is_none());
    })
    .unwrap();
    cx.read(|cx| {
        let tool = page.read(cx).tool.read(cx);
        assert_eq!(tool.state, PathState::SystemCommand);
        assert_eq!(
            tool.shown_path(),
            Some(std::path::Path::new("/usr/bin/termlet"))
        );
    });
}

// AppImage：显示 AppImage 文件本身（不是每次都变的挂载点），说明链接为 termlet 即可
#[gpui_kit::test]
fn appimage_shows_the_image_itself(cx: &mut TestAppContext) {
    let tmp = TempDir::new("appimage");
    let image = PathBuf::from("/home/u/Apps/termlet-0.2.0-linux-x64.AppImage");
    let env = CliEnv::at(
        Some(PathBuf::from(
            "/tmp/.mount_termleAbC123/usr/lib/termlet/Termlet",
        )),
        Some(tmp.0.join("home")),
    )
    .with_linux(None, Some(image.clone()));
    let (handle, page) = open(
        cx,
        Some((Settings::default(), LocalSettings::default(), env)),
    );
    let page = page.unwrap();
    frame(cx, handle);
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("cli-tool-appimage").is_some());
        assert!(window.try_find("cli-tool-path-toggle").is_none());
    })
    .unwrap();
    cx.read(|cx| {
        let tool = page.read(cx).tool.read(cx);
        assert_eq!(tool.state, PathState::AppImage);
        assert_eq!(tool.shown_path(), Some(image.as_path()));
    });
}

// 开发构建：命令行工具编译在 termlet-desktop 旁边，显示它的路径、说明用完整路径运行，不修改 PATH
#[gpui_kit::test]
fn dev_build_shows_the_tool_next_to_the_program(cx: &mut TestAppContext) {
    let tmp = TempDir::new("dev-build");
    let suffix = std::env::consts::EXE_SUFFIX;
    let exe = tmp.0.join(format!("termlet-desktop{suffix}"));
    let cli = tmp.0.join(format!("termlet{suffix}"));
    std::fs::write(&cli, b"").unwrap();
    let env = CliEnv::at(Some(exe), Some(tmp.0.join("home")));
    let (handle, page) = open(
        cx,
        Some((Settings::default(), LocalSettings::default(), env)),
    );
    let page = page.unwrap();
    frame(cx, handle);
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("cli-tool-dev-found").is_some());
        assert!(window.try_find("cli-tool-dev-build").is_none());
        assert!(window.try_find("cli-tool-path-toggle").is_none());
    })
    .unwrap();
    cx.read(|cx| {
        let tool = page.read(cx).tool.read(cx);
        assert_eq!(tool.state, PathState::DevBuild);
        assert_eq!(tool.shown_path(), Some(cli.as_path()));
    });
}

// 便携版：命令行工具在程序旁边的 bin 中，但不修改 PATH——只说明用完整路径运行，没有加入 PATH 的按钮
#[gpui_kit::test]
fn portable_does_not_offer_to_change_path(cx: &mut TestAppContext) {
    let tmp = TempDir::new("portable");
    let exe = tmp.0.join("Termlet.exe");
    let cli = termlet_core::userpath::cli_path(&exe);
    std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
    std::fs::write(&cli, b"").unwrap();
    let env = CliEnv::at(Some(exe), Some(tmp.0.join("home"))).into_portable();
    let (handle, page) = open(
        cx,
        Some((Settings::default(), LocalSettings::default(), env)),
    );
    let page = page.unwrap();
    frame(cx, handle);
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("cli-tool-path").is_some(), "列出完整路径");
        assert!(window.try_find("cli-tool-portable").is_some());
        assert!(window.try_find("cli-tool-path-toggle").is_none());
    })
    .unwrap();
    assert_eq!(
        cx.read(|cx| page.read(cx).tool.read(cx).state.clone()),
        PathState::Portable
    );
}

// MCP 服务：在下拉框中选 AI 工具，片段与复制的内容随之改变；复制的片段中是这台电脑上命令行工具的完整路径。
// 有 mcp add 命令的工具另给配置文件的写法，各有复制按钮
#[gpui_kit::test]
fn mcp_snippets_use_the_installed_tool(cx: &mut TestAppContext) {
    use termlet_core::proto::mcp::{AiTool, SnippetForm, Transport};

    let tmp = TempDir::new("mcp");
    let exe = tmp.0.join("Termlet.exe");
    let cli = termlet_core::userpath::cli_path(&exe);
    std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
    std::fs::write(&cli, b"").unwrap();
    let command = cli.display().to_string();
    let env = CliEnv::at(Some(exe), Some(tmp.0.join("home")));
    let (handle, page) = open(
        cx,
        Some((Settings::default(), LocalSettings::default(), env)),
    );
    let page = page.unwrap();
    let mcp = cx.read(|cx| page.read(cx).mcp.clone());
    let expected = |tool: AiTool, form: SnippetForm| {
        tool.snippets(Transport::Stdio { command: &command }, cfg!(windows))
            .into_iter()
            .find(|s| s.form == form)
            .unwrap()
            .text
    };

    // 默认是标准输入输出与 Claude Code：命令在前，另有项目的 .mcp.json
    frame(cx, handle);
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("cli-http-enabled").is_none(), "没有选 HTTP");
        assert!(window.try_find("cli-mcp-tool").is_some());
        assert!(window.try_find("cli-mcp-snippet-command").is_some());
        assert!(window.try_find("cli-mcp-snippet-file").is_some());
        assert!(
            window.try_find("cli-mcp-in-path").is_none(),
            "找到了命令行工具，不用提示 PATH"
        );
    })
    .unwrap();
    assert_eq!(cx.read(|cx| mcp.read(cx).access), Access::Stdio);
    assert_eq!(
        copy(cx, handle, "cli-mcp-copy-command"),
        expected(AiTool::ClaudeCode, SnippetForm::Command)
    );
    assert_eq!(
        copy(cx, handle, "cli-mcp-copy-file"),
        expected(AiTool::ClaudeCode, SnippetForm::File)
    );

    // 切到 Codex：config.toml 与 codex mcp add 两种
    select_tool(cx, handle, &mcp, AiTool::Codex);
    let copied = copy(cx, handle, "cli-mcp-copy-file");
    assert!(copied.starts_with("[mcp_servers.termlet]\n"), "{copied}");
    assert!(
        copied.contains(&format!("command = '{command}'")),
        "{copied}"
    );
    assert!(
        copy(cx, handle, "cli-mcp-copy-command").starts_with("codex mcp add termlet -- "),
        "Codex 的命令"
    );
    // Codex 另有说明：怎样省掉它自己的逐次确认；标准输入输出不用绕开代理
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("cli-mcp-codex-approval").is_some());
        assert!(window.try_find("cli-mcp-codex-proxy").is_none());
    })
    .unwrap();

    // VS Code 只有配置文件；“其他”是通用的 JSON
    select_tool(cx, handle, &mcp, AiTool::VsCode);
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("cli-mcp-snippet-command").is_none());
        assert!(window.try_find("cli-mcp-codex-approval").is_none());
    })
    .unwrap();
    assert_eq!(
        copy(cx, handle, "cli-mcp-copy-file"),
        expected(AiTool::VsCode, SnippetForm::File)
    );
    select_tool(cx, handle, &mcp, AiTool::Other);
    assert_eq!(
        copy(cx, handle, "cli-mcp-copy-file"),
        expected(AiTool::Other, SnippetForm::File)
    );
}

// 开发构建中没有命令行工具：配置中写 termlet，并说明它要在 PATH 中
#[gpui_kit::test]
fn mcp_without_the_tool_uses_path(cx: &mut TestAppContext) {
    let tmp = TempDir::new("mcp-dev");
    let env = CliEnv::at(Some(tmp.0.join("Termlet.exe")), Some(tmp.0.join("home")));
    let (handle, page) = open(
        cx,
        Some((Settings::default(), LocalSettings::default(), env)),
    );
    let page = page.unwrap();
    frame(cx, handle);
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("cli-mcp-in-path").is_some());
    })
    .unwrap();
    let snippets = cx.read(|cx| page.read(cx).mcp.read(cx).snippets(cx));
    assert_eq!(
        snippets[0].0.text,
        "claude mcp add --scope user termlet -- termlet mcp"
    );
}

/// 点一个复制按钮，返回剪贴板中的文字。
fn copy(cx: &mut TestAppContext, handle: AnyWindowHandle, id: &'static str) -> String {
    cx.update_window(handle, |_, window, cx| window.click(id, cx))
        .unwrap();
    cx.read_from_clipboard()
        .and_then(|item| item.text())
        .unwrap_or_default()
}

/// 在下拉框中选 AI 工具。
fn select_tool(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    mcp: &Entity<McpSection>,
    tool: termlet_core::proto::mcp::AiTool,
) {
    cx.update_window(handle, |_, window, cx| {
        let tools = mcp.read(cx).tools.clone();
        tools.update(cx, |state, cx| state.set_selected_value(&tool, window, cx));
    })
    .unwrap();
    frame(cx, handle);
    assert_eq!(cx.read(|cx| mcp.read(cx).selected(cx)), tool);
}

/// 只有本机时在监听。
fn listening(port: u16) -> HttpStatus {
    HttpStatus::Listening {
        port,
        scope: HttpScope::Local,
    }
}

/// 本机设置中外部 CLI 与 HTTP 的开关。
fn http_local(cli: bool, http: bool) -> LocalSettings {
    let mut local = LocalSettings::default();
    local.cli.enabled = cli;
    local.cli.http.enabled = http;
    local
}

// HTTP：分段选择切到 HTTP；开关与端口是草稿，与实际状态不同时写“保存后生效”；总开关关着时说明要先打开它；
// 端口不在 1–65535 时标出、不能保存；失败时有“重试”；保存时写进本机设置
#[gpui_kit::test]
fn mcp_http_drafts_and_status(cx: &mut TestAppContext) {
    let tmp = TempDir::new("mcp-http");
    let env = CliEnv::at(Some(tmp.0.join("Termlet.exe")), Some(tmp.0.join("home")));
    let (handle, page) = open(
        cx,
        Some((Settings::default(), http_local(false, false), env)),
    );
    let page = page.unwrap();
    let mcp = cx.read(|cx| page.read(cx).mcp.clone());
    let http = cx.read(|cx| mcp.read(cx).http.clone());
    let port = CliHttpSettings::DEFAULT_PORT;

    frame(cx, handle);
    cx.update_window(handle, |_, window, cx| {
        window.within("cli-mcp-access").click(1usize, cx);
    })
    .unwrap();
    frame(cx, handle);
    assert_eq!(cx.read(|cx| mcp.read(cx).access), Access::Http);
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("cli-http-enabled").is_some());
        assert!(window.try_find("cli-http-status").is_some());
        assert!(window.try_find("cli-http-pending").is_none());
        assert!(window.try_find("cli-http-retry").is_none());
    })
    .unwrap();
    cx.read(|cx| {
        let http = http.read(cx);
        assert_eq!(http.status, HttpStatus::Off);
        assert!(!http.pending());
        assert_eq!(http.url(), format!("http://127.0.0.1:{port}/mcp"));
    });
    let status =
        |cx: &mut TestAppContext| cx.update(|cx| http.read(cx).status_text(cx).0.to_string());
    assert_eq!(
        status(cx),
        "未启用：需要先允许 AI 工具与脚本访问（这一页最上面的开关）"
    );

    // 只打开 HTTP：总开关还关着，保存后也不会监听，不算“保存后生效”
    cx.update_window(handle, |_, window, cx| window.click("cli-http-enabled", cx))
        .unwrap();
    frame(cx, handle);
    assert!(cx.read(|cx| http.read(cx).enabled && !http.read(cx).pending()));
    // 再打开总开关：保存后才监听
    cx.update_window(handle, |_, window, cx| window.click("cli-enabled", cx))
        .unwrap();
    frame(cx, handle);
    assert_eq!(status(cx), "未启用");
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("cli-http-pending").is_some());
    })
    .unwrap();

    // 服务送来实际的状态：在草稿的端口上监听，与草稿一致
    cx.update(|cx| http.update(cx, |h, cx| h.set_status(listening(port), cx)));
    frame(cx, handle);
    assert_eq!(status(cx), format!("运行中：正在监听 127.0.0.1:{port}"));
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("cli-http-pending").is_none());
    })
    .unwrap();

    // 端口被占用：写明原因，有“重试”
    cx.update(|cx| {
        http.update(cx, |h, cx| {
            h.set_status(
                HttpStatus::Failed {
                    port,
                    scope: HttpScope::Local,
                    error: HttpError::PortInUse,
                },
                cx,
            )
        })
    });
    frame(cx, handle);
    assert!(status(cx).starts_with(&format!("端口 {port} 已被其他程序占用")));
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("cli-http-retry").is_some());
        assert!(window.try_find("cli-http-pending").is_none());
    })
    .unwrap();

    // 改端口：不合法时标出、不能保存；改成别的端口后“保存后生效”，不再给“重试”
    let retype = |cx: &mut TestAppContext, text: &str| {
        cx.update_window(handle, |_, window, cx| window.click("cli-http-port", cx))
            .unwrap();
        frame(cx, handle);
        cx.update_window(handle, |_, window, cx| {
            window.press("end", cx);
            for _ in 0..8 {
                window.press("backspace", cx);
            }
            window.input(text, cx);
        })
        .unwrap();
        frame(cx, handle);
    };
    retype(cx, "70000");
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("cli-http-port-error").is_some());
    })
    .unwrap();
    assert!(cx.read(|cx| page.read(cx).validate(cx).is_err()));
    retype(cx, "47900");
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("cli-http-port-error").is_none());
        assert!(window.try_find("cli-http-pending").is_some());
        assert!(window.try_find("cli-http-retry").is_none());
    })
    .unwrap();
    assert!(cx.read(|cx| page.read(cx).validate(cx).is_ok()));
    // 没在监听：地址按草稿的端口写
    assert_eq!(
        cx.read(|cx| http.read(cx).url()),
        "http://127.0.0.1:47900/mcp"
    );

    let mut saved = Settings::default();
    let mut local = http_local(false, false);
    cx.read(|cx| page.read(cx).apply(&mut saved, &mut local, cx));
    assert!(local.cli.enabled);
    assert!(local.cli.http.enabled);
    assert_eq!(local.cli.http.port, 47900);
    assert_eq!(local.cli.http.scope, HttpScope::Local, "默认只有本机");
}

// HTTP 的令牌与片段：令牌默认打码，“显示”后露出原文；预览的片段中令牌打码、复制的含原文；重新生成要先确认，
// 取消时不变，确认后立即换成新的
#[gpui_kit::test]
fn mcp_http_token_and_snippets(cx: &mut TestAppContext) {
    use termlet_core::cli::http::token;
    use termlet_core::proto::mcp::{AiTool, Transport};

    let tmp = TempDir::new("mcp-token");
    let env = CliEnv::at(Some(tmp.0.join("Termlet.exe")), Some(tmp.0.join("home")));
    let (handle, page) = open(cx, Some((Settings::default(), http_local(true, true), env)));
    let page = page.unwrap();
    let mcp = cx.read(|cx| page.read(cx).mcp.clone());
    let http = cx.read(|cx| mcp.read(cx).http.clone());
    let port = CliHttpSettings::DEFAULT_PORT;
    let url = format!("http://127.0.0.1:{port}/mcp");

    // 本机设置中打开过 HTTP：直接显示 HTTP；还没有令牌时预览写占位，不能复制
    poll_until(cx, "读令牌", |cx| {
        cx.read(|cx| !matches!(http.read(cx).token, TokenState::Loading))
    });
    frame(cx, handle);
    assert_eq!(cx.read(|cx| mcp.read(cx).access), Access::Http);
    cx.update_window(handle, |_, window, _| {
        assert!(
            window.try_find("cli-http-token-note").is_some(),
            "还没有令牌"
        );
        assert!(window.try_find("cli-mcp-copy-later").is_some());
    })
    .unwrap();
    let snippets = cx.read(|cx| mcp.read(cx).snippets(cx));
    assert!(
        snippets[0].0.text.contains("<令牌>"),
        "{}",
        snippets[0].0.text
    );
    assert!(snippets.iter().all(|(_, copy)| copy.is_none()));

    // 开始监听时（核心在这时生成令牌）重新读令牌
    let store = cx.update(|cx| AppState::store(cx));
    let first = token::regenerate(&store, TokenKind::Local)
        .unwrap()
        .expose()
        .to_string();
    cx.update(|cx| http.update(cx, |h, cx| h.set_status(listening(port), cx)));
    poll_until(cx, "读出令牌", |cx| {
        cx.read(|cx| http.read(cx).token_for_copy(TokenKind::Local))
            .as_ref()
            == Some(&first)
    });
    frame(cx, handle);
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("cli-http-token").is_some());
        assert!(window.try_find("cli-mcp-copy-later").is_none());
    })
    .unwrap();
    assert!(!cx.read(|cx| http.read(cx).revealed), "默认打码");

    // 预览打码、复制原文：Claude Code 只给用户级的命令
    let snippets = cx.read(|cx| mcp.read(cx).snippets(cx));
    assert_eq!(snippets.len(), 1);
    let (preview, real) = &snippets[0];
    assert!(
        preview
            .text
            .contains(&super::http::masked_token(TokenKind::Local))
    );
    assert!(!preview.text.contains(&first));
    let copied = copy(cx, handle, "cli-mcp-copy-command");
    assert_eq!(Some(&copied), real.as_ref());
    assert!(
        copied.contains("--scope user") && copied.contains(&first),
        "{copied}"
    );
    select_tool(cx, handle, &mcp, AiTool::Cursor);
    assert_eq!(
        copy(cx, handle, "cli-mcp-copy-file"),
        AiTool::Cursor.snippets(
            Transport::Http {
                url: &url,
                token: &first
            },
            cfg!(windows)
        )[0]
        .text
    );

    // 显示 / 隐藏；复制令牌
    cx.update_window(handle, |_, window, cx| {
        window.click("cli-http-token-reveal", cx)
    })
    .unwrap();
    frame(cx, handle);
    assert!(cx.read(|cx| http.read(cx).revealed));
    assert_eq!(copy(cx, handle, "cli-http-token-copy"), first);

    // 重新生成：取消时不变
    cx.update_window(handle, |_, window, cx| {
        window.click("cli-http-token-regenerate", cx)
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| window.click("cancel", cx))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(
        cx.read(|cx| http.read(cx).token_for_copy(TokenKind::Local)),
        Some(first.clone())
    );
    // 确认后换成新的（立即生效：存储中已是新的），重新打码
    cx.update_window(handle, |_, window, cx| {
        window.click("cli-http-token-regenerate", cx)
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| window.click("ok", cx))
        .unwrap();
    poll_until(cx, "重新生成令牌", |cx| {
        cx.read(|cx| http.read(cx).token_for_copy(TokenKind::Local))
            .is_some_and(|t| t != first)
    });
    let second = cx
        .read(|cx| http.read(cx).token_for_copy(TokenKind::Local))
        .unwrap();
    assert!(!cx.read(|cx| http.read(cx).revealed));
    match token::view(&store, TokenKind::Local).unwrap() {
        TokenView::Ready(stored) => assert_eq!(stored.expose(), second),
        other => panic!("{other:?}"),
    }
    frame(cx, handle);
    assert!(
        copy(cx, handle, "cli-mcp-copy-file").contains(&second),
        "片段跟着换成新的令牌"
    );

    // Codex 的 HTTP：另外提醒开着系统代理时设 NO_PROXY
    select_tool(cx, handle, &mcp, AiTool::Codex);
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("cli-mcp-codex-approval").is_some());
        assert!(window.try_find("cli-mcp-codex-proxy").is_some());
    })
    .unwrap();

    // Claude Desktop 选 HTTP：不给片段，说明它连不了本机的 HTTP
    select_tool(cx, handle, &mcp, AiTool::ClaudeDesktop);
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("cli-mcp-no-http").is_some());
        assert!(window.try_find("cli-mcp-snippet-file").is_none());
        assert!(window.try_find("cli-mcp-snippet-command").is_none());
    })
    .unwrap();
    // 切回标准输入输出：Claude Desktop 照常有片段，同一个 AI 工具
    cx.update_window(handle, |_, window, cx| {
        window.within("cli-mcp-access").click(0usize, cx);
    })
    .unwrap();
    frame(cx, handle);
    assert_eq!(
        cx.read(|cx| mcp.read(cx).selected(cx)),
        AiTool::ClaudeDesktop
    );
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("cli-mcp-no-http").is_none());
        assert!(window.try_find("cli-mcp-snippet-file").is_some());
        assert!(window.try_find("cli-http-enabled").is_none());
    })
    .unwrap();
}

/// 这台电脑在局域网中的两个地址（测试中给定，不读真实的网卡）。
fn two_lan_addresses() -> Vec<LanAddress> {
    vec![
        LanAddress {
            ip: [192, 168, 1, 5].into(),
            interface: "WLAN".into(),
            gateway: true,
        },
        LanAddress {
            ip: [10, 0, 0, 7].into(),
            interface: "vEthernet (WSL)".into(),
            gateway: false,
        },
    ]
}

// 监听范围：默认只有本机；选“本机与局域网”后另有局域网地址（多个时可选）、局域网令牌与风险的说明，片段可以给局域网
// 中的另一台电脑（局域网地址与局域网令牌）；保存时写进本机设置
#[gpui_kit::test]
fn mcp_http_lan_scope(cx: &mut TestAppContext) {
    use termlet_core::cli::http::token;
    use termlet_core::proto::mcp::{AiTool, Transport};

    let tmp = TempDir::new("mcp-lan");
    let env = CliEnv::at(Some(tmp.0.join("Termlet.exe")), Some(tmp.0.join("home")))
        .with_lan(two_lan_addresses());
    let (handle, page) = open(cx, Some((Settings::default(), http_local(true, true), env)));
    let page = page.unwrap();
    let mcp = cx.read(|cx| page.read(cx).mcp.clone());
    let http = cx.read(|cx| mcp.read(cx).http.clone());
    let port = CliHttpSettings::DEFAULT_PORT;
    let store = cx.update(|cx| AppState::store(cx));
    let local_token = token::regenerate(&store, TokenKind::Local)
        .unwrap()
        .expose()
        .to_string();
    cx.update(|cx| http.update(cx, |h, cx| h.set_status(listening(port), cx)));
    poll_until(cx, "读令牌", |cx| {
        cx.read(|cx| http.read(cx).token_for_copy(TokenKind::Local))
            .is_some()
    });
    frame(cx, handle);
    let lan_ids = [
        "cli-http-lan-ip",
        "cli-http-lan-url",
        "cli-http-lan-plaintext",
        "cli-http-lan-firewall",
        "cli-http-lan-approval",
        "cli-http-lan-tools",
    ];
    let has_target = |window: &mut Window| window.try_find("cli-mcp-target-row").is_some();
    cx.update_window(handle, |_, window, _| {
        assert!(window.within("cli-http-scope").try_find(1usize).is_some());
        for id in lan_ids {
            assert!(window.try_find(id).is_none(), "只有本机时没有 {id}");
        }
        assert!(!has_target(window), "只有本机时片段只给这台电脑");
    })
    .unwrap();
    assert!(!cx.read(|cx| http.read(cx).pending()));

    // 选“本机与局域网”：保存后才生效；局域网的部分出现，局域网令牌还没有
    cx.update_window(handle, |_, window, cx| {
        window.within("cli-http-scope").click(1usize, cx);
    })
    .unwrap();
    frame(cx, handle);
    cx.read(|cx| {
        let http = http.read(cx);
        assert_eq!(http.scope, HttpScope::Lan);
        assert!(http.pending(), "监听范围变了：保存后生效");
        assert_eq!(
            http.lan_url(cx).as_deref(),
            Some(format!("http://192.168.1.5:{port}/mcp").as_str()),
            "默认第一个地址（有默认网关的）"
        );
        assert!(http.token_for_copy(TokenKind::Lan).is_none());
    });
    cx.update_window(handle, |_, window, _| {
        for id in lan_ids {
            assert!(window.try_find(id).is_some(), "局域网时有 {id}");
        }
        assert!(has_target(window));
        assert!(
            window.try_find("cli-http-lan-token-note").is_some(),
            "还没有局域网令牌"
        );
    })
    .unwrap();

    // 片段给局域网中的另一台电脑：局域网地址；还没有局域网令牌时预览写占位、不能复制
    cx.update_window(handle, |_, window, cx| {
        window.within("cli-mcp-target").click(1usize, cx);
    })
    .unwrap();
    frame(cx, handle);
    assert_eq!(cx.read(|cx| mcp.read(cx).target(cx)), Target::Lan);
    let snippets = cx.read(|cx| mcp.read(cx).snippets(cx));
    assert!(
        snippets[0].0.text.contains("http://192.168.1.5:") && snippets[0].0.text.contains("<令牌>"),
        "{}",
        snippets[0].0.text
    );
    assert!(snippets.iter().all(|(_, copy)| copy.is_none()));
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("cli-mcp-lan-target").is_some());
        assert!(window.try_find("cli-mcp-copy-later").is_some());
    })
    .unwrap();

    // 保存后核心在监听局域网时生成局域网令牌：重新读出，片段中是局域网令牌（不是本机的）
    let lan_token = token::regenerate(&store, TokenKind::Lan)
        .unwrap()
        .expose()
        .to_string();
    cx.update(|cx| {
        http.update(cx, |h, cx| {
            h.set_status(
                HttpStatus::Listening {
                    port,
                    scope: HttpScope::Lan,
                },
                cx,
            )
        })
    });
    poll_until(cx, "读出局域网令牌", |cx| {
        cx.read(|cx| http.read(cx).token_for_copy(TokenKind::Lan))
            .as_ref()
            == Some(&lan_token)
    });
    frame(cx, handle);
    assert!(!cx.read(|cx| http.read(cx).pending()));
    assert_eq!(
        cx.update(|cx| http.read(cx).status_text(cx).0.to_string()),
        format!("运行中：正在所有 IPv4 网卡上监听端口 {port}（本机与局域网）")
    );
    select_tool(cx, handle, &mcp, AiTool::Cursor);
    let lan_url = format!("http://192.168.1.5:{port}/mcp");
    let copied = copy(cx, handle, "cli-mcp-copy-file");
    assert_eq!(
        copied,
        AiTool::Cursor.snippets(
            Transport::Http {
                url: &lan_url,
                token: &lan_token
            },
            cfg!(windows)
        )[0]
        .text
    );
    assert!(!copied.contains(&local_token));
    let preview = cx.read(|cx| mcp.read(cx).snippets(cx))[0].0.text.clone();
    assert!(
        preview.contains(&super::http::masked_token(TokenKind::Lan)),
        "{preview}"
    );

    // 局域网令牌：显示、复制
    cx.update_window(handle, |_, window, cx| {
        window.click("cli-http-lan-token-reveal", cx)
    })
    .unwrap();
    frame(cx, handle);
    assert!(cx.read(|cx| http.read(cx).lan_revealed && !http.read(cx).revealed));
    assert_eq!(copy(cx, handle, "cli-http-lan-token-copy"), lan_token);

    // 换一个局域网地址：片段随之改变；Codex 另提醒 NO_PROXY 写这个地址
    cx.update_window(handle, |_, window, cx| {
        let select = http.read(cx).lan_select.clone().unwrap();
        let ip: std::net::Ipv4Addr = [10, 0, 0, 7].into();
        select.update(cx, |state, cx| state.set_selected_value(&ip, window, cx));
    })
    .unwrap();
    frame(cx, handle);
    assert_eq!(
        cx.read(|cx| http.read(cx).lan_url(cx)),
        Some(format!("http://10.0.0.7:{port}/mcp"))
    );
    select_tool(cx, handle, &mcp, AiTool::Codex);
    assert!(copy(cx, handle, "cli-mcp-copy-file").contains("http://10.0.0.7:"));
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("cli-mcp-codex-proxy").is_some());
    })
    .unwrap();

    // 改回只有本机：片段回到这台电脑（选的“另一台电脑”不再起作用）
    cx.update_window(handle, |_, window, cx| {
        window.within("cli-http-scope").click(0usize, cx);
    })
    .unwrap();
    frame(cx, handle);
    assert_eq!(cx.read(|cx| mcp.read(cx).target(cx)), Target::Local);
    cx.update_window(handle, |_, window, _| assert!(!has_target(window)))
        .unwrap();
    assert!(copy(cx, handle, "cli-mcp-copy-file").contains(&local_token));

    // 保存：监听范围写进本机设置
    cx.update_window(handle, |_, window, cx| {
        window.within("cli-http-scope").click(1usize, cx);
    })
    .unwrap();
    frame(cx, handle);
    let mut saved = Settings::default();
    let mut local = http_local(true, true);
    cx.read(|cx| page.read(cx).apply(&mut saved, &mut local, cx));
    assert_eq!(local.cli.http.scope, HttpScope::Lan);
}

// 这台电脑没有局域网地址：说明找不到，给另一台电脑的片段不给
#[gpui_kit::test]
fn mcp_http_lan_without_an_address(cx: &mut TestAppContext) {
    let tmp = TempDir::new("mcp-lan-none");
    let env = CliEnv::at(Some(tmp.0.join("Termlet.exe")), Some(tmp.0.join("home")));
    let mut local = http_local(true, true);
    local.cli.http.scope = HttpScope::Lan;
    let (handle, page) = open(cx, Some((Settings::default(), local, env)));
    let mcp = cx.read(|cx| page.unwrap().read(cx).mcp.clone());
    frame(cx, handle);
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("cli-http-lan-none").is_some());
        assert!(window.try_find("cli-http-lan-url").is_none());
    })
    .unwrap();
    cx.update_window(handle, |_, window, cx| {
        window.within("cli-mcp-target").click(1usize, cx);
    })
    .unwrap();
    frame(cx, handle);
    assert!(cx.read(|cx| mcp.read(cx).snippets(cx)).is_empty());
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("cli-mcp-no-lan-address").is_some());
        assert!(window.try_find("cli-mcp-no-http").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn page_drafts_rules_and_validates(cx: &mut TestAppContext) {
    let tmp = TempDir::new("page");
    let mut settings = Settings::default();
    settings.guard.disabled_rules = vec!["from-a-newer-version".into()];
    let env = CliEnv::at(Some(tmp.0.join("Termlet.exe")), Some(tmp.0.join("home")));
    let (handle, page) = open(cx, Some((settings.clone(), LocalSettings::default(), env)));
    let page = page.unwrap();

    frame(cx, handle);
    cx.update_window(handle, |_, window, cx| {
        // 开发构建（应用程序旁边没有 bin\termlet.exe）：只作说明，没有修改 PATH 的按钮
        assert!(window.try_find("cli-tool-dev-build").is_some());
        assert!(window.try_find("cli-tool-path-toggle").is_none());
        window.click("cli-enabled", cx);
        // 默认允许在 Termlet 没有打开时自动启动：点一下关掉
        window.click("cli-auto-start", cx);
        window.click("cli-rule-power", cx);
        window.within("cli-approval").click(1usize, cx);
        window.click("cli-custom-rule-add", cx);
    })
    .unwrap();
    assert_eq!(
        cx.read(|cx| page.read(cx).tool.read(cx).state.clone()),
        PathState::Missing
    );
    // 新加的规则获得焦点：输入不合法的正则（输入引起的事件在这次更新结束后才处理，之后再画一帧检查）
    frame(cx, handle);
    cx.update_window(handle, |_, window, cx| window.input("(", cx))
        .unwrap();
    frame(cx, handle);
    cx.update_window(handle, |_, window, _| {
        assert!(
            window.try_find(("cli-custom-rule-error", 0u64)).is_some(),
            "不合法的正则在输入框下标出"
        );
    })
    .unwrap();
    cx.read(|cx| {
        assert_eq!(page.read(cx).custom.len(), 1);
        assert!(
            page.read(cx).validate(cx).is_err(),
            "有不能使用的规则时不能保存"
        );
    });

    cx.update_window(handle, |_, window, cx| {
        window.press("backspace", cx);
        window.input(r" \bpurge\b ", cx);
    })
    .unwrap();
    frame(cx, handle);
    cx.update_window(handle, |_, window, cx| {
        assert!(window.try_find(("cli-custom-rule-error", 0u64)).is_none());
        // 规则测试：按草稿中的规则（power 已关闭，新的自定义规则已生效）
        window.click("cli-rule-test", cx);
        window.input("sudo reboot", cx);
    })
    .unwrap();
    frame(cx, handle);
    cx.read(|cx| {
        let page = page.read(cx);
        assert!(page.validate(cx).is_ok());
        assert_eq!(page.test_result, Some(None), "关掉的内置规则不再命中");
    });
    cx.update_window(handle, |_, window, cx| {
        window.press("ctrl-a", cx);
        window.input("apt purge nginx", cx);
    })
    .unwrap();
    frame(cx, handle);
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("cli-rule-test-result").is_some());
    })
    .unwrap();
    cx.read(|cx| {
        let hit = page
            .read(cx)
            .test_result
            .clone()
            .flatten()
            .expect("应命中自定义规则");
        assert_eq!(hit.pattern(), r"\bpurge\b");
    });

    // 写进设置：只动这一页的字段，不认识的关闭 ID 保留
    let (mut saved, mut local) = (settings.clone(), LocalSettings::default());
    cx.read(|cx| page.read(cx).apply(&mut saved, &mut local, cx));
    assert!(local.cli.enabled);
    assert!(!local.cli.auto_start);
    assert_eq!(
        saved.guard.disabled_rules,
        ["from-a-newer-version", "power"]
    );
    assert_eq!(saved.guard.custom_rules, [r"\bpurge\b"]);
    assert_eq!(saved.guard.approval, Approval::Block);
    assert_eq!(saved.terminal, settings.terminal);

    // 删除规则后保存时为空
    cx.update_window(handle, |_, window, cx| {
        window.click(("cli-custom-rule-delete", 0u64), cx)
    })
    .unwrap();
    frame(cx, handle);
    cx.read(|cx| page.read(cx).apply(&mut saved, &mut local, cx));
    assert!(saved.guard.custom_rules.is_empty());
}

#[gpui_kit::test]
fn page_loads_saved_custom_rules(cx: &mut TestAppContext) {
    let tmp = TempDir::new("saved");
    let mut settings = Settings::default();
    settings.guard.custom_rules = vec![r"\bpurge\b".into(), ".*".into()];
    let env = CliEnv::at(None, Some(tmp.0.clone()));
    let (handle, page) = open(cx, Some((settings, LocalSettings::default(), env)));
    let page = page.unwrap();
    frame(cx, handle);
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find(("cli-custom-rule", 0u64)).is_some());
        assert!(window.try_find(("cli-custom-rule-error", 0u64)).is_none());
        assert!(
            window.try_find(("cli-custom-rule-error", 1u64)).is_some(),
            "设置中已有的、会匹配任何命令的规则也标出"
        );
    })
    .unwrap();
    cx.read(|cx| {
        assert_eq!(page.read(cx).custom.len(), 2);
        assert!(page.read(cx).validate(cx).is_err());
    });
}

#[gpui_kit::test]
fn skill_installs_and_removes(cx: &mut TestAppContext) {
    let home = TempDir::new("home");
    std::fs::create_dir_all(home.0.join(".claude")).unwrap();
    let env = CliEnv::at(None, Some(home.0.clone()));
    let (handle, page) = open(
        cx,
        Some((Settings::default(), LocalSettings::default(), env)),
    );
    let page = page.unwrap();
    let skill_section = cx.read(|cx| page.read(cx).skill.clone());
    let status = |cx: &mut TestAppContext, agent: Agent| {
        cx.read(|cx| {
            skill_section
                .read(cx)
                .targets
                .clone()
                .flatten()
                .and_then(|list| list.into_iter().find(|t| t.agent == agent))
                .map(|t| t.status)
        })
    };
    poll_until(cx, "列出安装位置", |cx| {
        status(cx, Agent::ClaudeCode).is_some()
    });
    assert_eq!(status(cx, Agent::ClaudeCode), Some(Status::Missing));
    assert_eq!(status(cx, Agent::Codex), None, "旧位置没有文件时不列出");

    frame(cx, handle);
    cx.update_window(handle, |_, window, cx| {
        window.click("cli-skill-install-claude", cx)
    })
    .unwrap();
    poll_until(cx, "安装到 Claude Code", |cx| {
        status(cx, Agent::ClaudeCode) == Some(Status::Installed)
    });
    let file = skill::file(&home.0.join(".claude").join("skills"));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), skill::CONTENT);

    // 内容与内置版本不同：显示“有更新”，更新后与内置版本相同
    std::fs::write(&file, "old").unwrap();
    cx.update(|cx| skill_section.update(cx, |s, cx| s.reload(cx)));
    poll_until(cx, "发现有更新", |cx| {
        status(cx, Agent::ClaudeCode) == Some(Status::Outdated)
    });
    frame(cx, handle);
    cx.update_window(handle, |_, window, cx| {
        window.click("cli-skill-install-claude", cx)
    })
    .unwrap();
    poll_until(cx, "更新", |cx| {
        status(cx, Agent::ClaudeCode) == Some(Status::Installed)
    });
    assert_eq!(std::fs::read_to_string(&file).unwrap(), skill::CONTENT);

    frame(cx, handle);
    cx.update_window(handle, |_, window, cx| {
        window.click("cli-skill-remove-claude", cx)
    })
    .unwrap();
    poll_until(cx, "删除", |cx| {
        status(cx, Agent::ClaudeCode) == Some(Status::Missing)
    });
    assert!(!file.exists());
}

fn audit_entry(host: &str, action: AuditAction, detail: &str, status: AuditStatus) -> AuditEntry {
    AuditEntry {
        host_id: format!("h_{host}"),
        host_name: host.into(),
        action,
        detail: detail.into(),
        status,
        exit_code: (status == AuditStatus::Ok).then_some(0),
        ..Default::default()
    }
}

fn audit_view(cx: &mut TestAppContext) -> Entity<AuditView> {
    cx.read(audit::open_views)
        .into_iter()
        .next()
        .expect("审计对话框应已打开")
}

#[gpui_kit::test]
fn audit_dialog_lists_filters_pages_and_refreshes(cx: &mut TestAppContext) {
    let (handle, _) = open(cx, None);
    let log = Arc::new(cx.update(|cx| AuditLog::new(AppState::store(cx))).unwrap());
    for i in 0..(PAGE + 50) {
        let (host, status) = if i % 5 == 0 {
            ("db", AuditStatus::Blocked)
        } else {
            ("web", AuditStatus::Ok)
        };
        log.add(audit_entry(
            host,
            AuditAction::Exec,
            &format!("echo {i}"),
            status,
        ))
        .unwrap();
    }
    let long = format!("cat <<'EOF'\n{}\nEOF", "x".repeat(800));
    let long_id = log
        .add(audit_entry(
            "web",
            AuditAction::Exec,
            &long,
            AuditStatus::Ok,
        ))
        .unwrap();

    cx.update_window(handle, |_, window, cx| {
        audit::open_audit_with(log.clone(), window, cx)
    })
    .unwrap();
    frame(cx, handle);
    let view = audit_view(cx);
    cx.read(|cx| {
        let view = view.read(cx);
        assert_eq!(view.entries.len(), PAGE, "先读一页");
        assert!(view.more);
        assert_eq!(view.entries[0].id, long_id, "最新的在前");
        assert!(view.entries[0].partial, "列表中的内容只有开头");
    });

    // 加载更多：按 ID 向更早翻页
    cx.update_window(handle, |_, window, cx| window.click("audit-load-more", cx))
        .unwrap();
    frame(cx, handle);
    cx.read(|cx| {
        let view = view.read(cx);
        assert_eq!(view.entries.len(), PAGE + 51);
        assert!(!view.more);
        let mut ids: Vec<i64> = view.entries.iter().map(|e| e.id).collect();
        ids.dedup();
        assert_eq!(ids.len(), PAGE + 51, "翻页不重复");
    });

    // 选中：取完整内容；上下键换选中的记录
    cx.update_window(handle, |_, window, cx| {
        window.click(("audit-row", long_id as u64), cx)
    })
    .unwrap();
    frame(cx, handle);
    cx.read(|cx| {
        let selected = view.read(cx).selected.clone().expect("应选中");
        assert!(
            !selected.partial && selected.detail == long,
            "应取到完整内容"
        );
    });
    cx.update_window(handle, |_, window, cx| {
        assert!(window.try_find("audit-detail").is_some());
        window.press("down", cx);
    })
    .unwrap();
    frame(cx, handle);
    cx.read(|cx| {
        let view = view.read(cx);
        assert_eq!(
            view.selected.as_ref().map(|e| e.id),
            Some(view.entries[1].id)
        );
    });

    // 搜索（停止输入一会儿后查询）与按结果筛选
    cx.update_window(handle, |_, window, cx| {
        window.click("audit-search", cx);
        window.input("db", cx);
    })
    .unwrap();
    cx.executor().advance_clock(SEARCH_DELAY * 2);
    frame(cx, handle);
    cx.read(|cx| {
        let view = view.read(cx);
        assert_eq!(view.filter.q, "db");
        assert_eq!(view.entries.len(), (PAGE + 50) / 5);
        assert!(view.entries.iter().all(|e| e.host_name == "db"));
        assert!(view.selected.is_none(), "选中的记录被筛掉后关闭详情");
    });
    cx.update(|cx| view.update(cx, |v, cx| v.set_status(Some(AuditStatus::Ok), cx)));
    frame(cx, handle);
    cx.update_window(handle, |_, window, _| {
        assert!(
            window.try_find("audit-empty").is_some(),
            "没有符合条件的记录"
        );
    })
    .unwrap();
    cx.update(|cx| view.update(cx, |v, cx| v.set_status(Some(AuditStatus::Blocked), cx)));
    frame(cx, handle);
    assert_eq!(cx.read(|cx| view.read(cx).entries.len()), (PAGE + 50) / 5);

    // 打开着时写入了新记录：只补上符合条件的新记录
    log.add(audit_entry(
        "db",
        AuditAction::Exec,
        "rm -rf /",
        AuditStatus::Blocked,
    ))
    .unwrap();
    log.add(audit_entry(
        "web",
        AuditAction::Exec,
        "uptime",
        AuditStatus::Ok,
    ))
    .unwrap();
    cx.update(refresh_open_audit);
    frame(cx, handle);
    cx.read(|cx| {
        let view = view.read(cx);
        assert_eq!(view.entries.len(), (PAGE + 50) / 5 + 1);
        assert_eq!(view.entries[0].detail, "rm -rf /");
    });

    // 清空：先确认
    cx.update_window(handle, |_, window, cx| window.click("audit-clear", cx))
        .unwrap();
    frame(cx, handle);
    cx.update_window(handle, |_, window, cx| window.click("ok", cx))
        .unwrap();
    frame(cx, handle);
    assert!(
        log.list(&Default::default()).unwrap().is_empty(),
        "确认后清空"
    );
    assert!(cx.read(|cx| view.read(cx).entries.is_empty()));
}

// MCP 的调用：来源显示 MCP 与客户端名（旧记录同样认得出），可以单独筛选；详情中等待批准的时长与耗时分开写
#[gpui_kit::test]
fn audit_dialog_tells_mcp_apart_and_shows_the_approval_wait(cx: &mut TestAppContext) {
    use termlet_core::audit::AuditSource;
    let (handle, _) = open(cx, None);
    crate::i18n::set_language(crate::i18n::Language::ZhCn);
    let log = Arc::new(cx.update(|cx| AuditLog::new(AppState::store(cx))).unwrap());
    let mut cli = audit_entry("web", AuditAction::Exec, "uptime", AuditStatus::Ok);
    cli.caller = "bash < node".into();
    log.add(cli).unwrap();
    let mut mcp = audit_entry(
        "web",
        AuditAction::Exec,
        "rm -rf /tmp/x",
        AuditStatus::Approved,
    );
    mcp.caller = "mcp:claude-code < claude.exe".into();
    mcp.duration_ms = 40;
    mcp.approval_wait_ms = Some(31_353);
    let mcp_id = log.add(mcp).unwrap();

    cx.update_window(handle, |_, window, cx| {
        audit::open_audit_with(log.clone(), window, cx)
    })
    .unwrap();
    frame(cx, handle);
    let view = audit_view(cx);
    cx.read(|cx| {
        let entries = &view.read(cx).entries;
        assert_eq!(entries[0].source, AuditSource::Mcp);
        assert_eq!(audit::entry_source(&entries[0]), "MCP（claude-code）");
        assert_eq!(entries[1].source, AuditSource::Cli);
    });
    cx.update_window(handle, |_, window, cx| {
        window.click(("audit-row", mcp_id as u64), cx)
    })
    .unwrap();
    frame(cx, handle);
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("audit-approval-wait").is_some());
    })
    .unwrap();

    cx.update(|cx| view.update(cx, |v, cx| v.set_source(Some(AuditSource::Mcp), cx)));
    frame(cx, handle);
    cx.read(|cx| {
        let entries = &view.read(cx).entries;
        assert!(entries.len() == 1 && entries[0].id == mcp_id, "{entries:?}");
    });
    cx.update(|cx| view.update(cx, |v, cx| v.set_source(Some(AuditSource::Cli), cx)));
    frame(cx, handle);
    cx.read(|cx| {
        let entries = &view.read(cx).entries;
        assert!(entries.len() == 1 && entries[0].detail == "uptime");
    });
}

#[gpui_kit::test]
fn audit_export_writes_the_filtered_entries(cx: &mut TestAppContext) {
    setup(cx);
    let tmp = TempDir::new("export");
    let log = cx.update(|cx| AuditLog::new(AppState::store(cx))).unwrap();
    log.add(audit_entry(
        "web",
        AuditAction::Exec,
        "uptime",
        AuditStatus::Ok,
    ))
    .unwrap();
    log.add(audit_entry(
        "db",
        AuditAction::Upload,
        "a → b",
        AuditStatus::Failed,
    ))
    .unwrap();
    let filter = audit::Filter {
        q: "web".into(),
        ..Default::default()
    };
    let path = tmp.0.join("audit.json");
    let count = audit::export_to(
        &log,
        &path,
        termlet_core::audit::ExportFormat::Json,
        &filter.query(0),
    )
    .unwrap();
    assert_eq!(count, 1);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("uptime") && !text.contains("a → b"));
    // 写不了的位置：报错，不留半个文件
    let bad = tmp.0.join("missing").join("audit.csv");
    assert!(
        audit::export_to(
            &log,
            &bad,
            termlet_core::audit::ExportFormat::Csv,
            &filter.query(0)
        )
        .is_err()
    );
    assert!(!bad.exists());
}
