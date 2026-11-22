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
use termlet_core::model::{Approval, LocalSettings, Settings};
use termlet_core::proto::skill::{self, Agent, Status};

use super::audit::{self, AuditView, PAGE, SEARCH_DELAY};
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
                    page: page.map(|(settings, local, env)| {
                        cx.new(|cx| CliPage::with_env(&settings, &local, env, window, cx))
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
    use termlet_core::proto::mcp::{AiTool, SnippetForm};

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
    let clipboard = |cx: &mut TestAppContext| {
        cx.read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default()
    };
    let expected = |tool: AiTool, form: SnippetForm| {
        tool.snippets(&command, cfg!(windows))
            .into_iter()
            .find(|s| s.form == form)
            .unwrap()
            .text
    };
    let select = |cx: &mut TestAppContext, tool: AiTool| {
        cx.update_window(handle, |_, window, cx| {
            let tools = mcp.read(cx).tools.clone();
            tools.update(cx, |state, cx| state.set_selected_value(&tool, window, cx));
        })
        .unwrap();
        frame(cx, handle);
        assert_eq!(cx.read(|cx| mcp.read(cx).selected(cx)), tool);
    };
    let copy = |cx: &mut TestAppContext, id: &'static str| {
        cx.update_window(handle, |_, window, cx| window.click(id, cx))
            .unwrap();
        clipboard(cx)
    };

    // 默认是 Claude Code：命令在前，另有项目的 .mcp.json
    frame(cx, handle);
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("cli-mcp-tool").is_some());
        assert!(window.try_find("cli-mcp-snippet-command").is_some());
        assert!(window.try_find("cli-mcp-snippet-file").is_some());
        assert!(
            window.try_find("cli-mcp-in-path").is_none(),
            "找到了命令行工具，不用提示 PATH"
        );
    })
    .unwrap();
    assert_eq!(
        copy(cx, "cli-mcp-copy-command"),
        expected(AiTool::ClaudeCode, SnippetForm::Command)
    );
    assert_eq!(
        copy(cx, "cli-mcp-copy-file"),
        expected(AiTool::ClaudeCode, SnippetForm::File)
    );

    // 切到 Codex：config.toml 与 codex mcp add 两种
    select(cx, AiTool::Codex);
    let copied = copy(cx, "cli-mcp-copy-file");
    assert!(copied.starts_with("[mcp_servers.termlet]\n"), "{copied}");
    assert!(
        copied.contains(&format!("command = '{command}'")),
        "{copied}"
    );
    assert!(
        copy(cx, "cli-mcp-copy-command").starts_with("codex mcp add termlet -- "),
        "Codex 的命令"
    );

    // VS Code 只有配置文件；“其他”是通用的 JSON
    select(cx, AiTool::VsCode);
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("cli-mcp-snippet-command").is_none());
    })
    .unwrap();
    assert_eq!(
        copy(cx, "cli-mcp-copy-file"),
        expected(AiTool::VsCode, SnippetForm::File)
    );
    select(cx, AiTool::Other);
    assert_eq!(
        copy(cx, "cli-mcp-copy-file"),
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
        snippets[0].text,
        "claude mcp add --scope user termlet -- termlet mcp"
    );
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
    cx.read(|cx| page.read(cx).apply(&mut saved, &mut local));
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
    cx.read(|cx| page.read(cx).apply(&mut saved, &mut local));
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
