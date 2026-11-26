//! 批量执行 Tab 的测试：工具栏按钮打开、按分组选主机（分组右键菜单中没有这一项）、Telnet 主机列为未执行、连不上的主机与审计记录、本地终端不能在终端里执行、
//! 撰写栏的“执行并对比”沿用没在执行的 Tab、插入快捷命令、移到别的窗口；连真实 sshd 的（`TERMLET_TEST_SSH`）：
//! 输出相同的合并成一组、不同的另成一组、危险命令先确认、停止后重试没有成功的、还在执行时关闭先确认。

use std::time::Duration;

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, ElementId, Entity, TestAppContext};
use termlet_core::audit::{AuditLog, AuditQuery, AuditSource, AuditStatus, CALLER_BATCH};
use termlet_core::id::{GroupId, HostId};
use termlet_core::model::{Group, Host, Protocol, SecretField, Snippet};
use termlet_core::proto::BatchStatus;
use termlet_core::secret::SecretString;
use termlet_core::store::{Owner, Position};

use super::results::Progress;
use super::*;
use crate::i18n::{Language, set_language};
use crate::testkit::{expand_host_tree, open_workspace, poll_until, test_server};
use crate::workspace::Workspace;

fn setup(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Workspace>) {
    let handle = open_workspace(cx);
    set_language(Language::ZhCn);
    cx.update(|cx| {
        AppState::store(cx)
            .update_settings(|s| s.general.confirm_close = false)
            .unwrap()
    });
    let workspace = cx
        .update_window(handle, |_, window, cx| {
            Workspace::in_window(window, cx).unwrap()
        })
        .unwrap();
    (handle, workspace)
}

fn host(name: &str, address: &str, port: u16) -> Host {
    Host {
        name: name.into(),
        address: address.into(),
        port,
        username: "termlet".into(),
        ..Default::default()
    }
}

/// 建分组“生产”，把 `hosts` 放进去。
fn group_with(cx: &mut TestAppContext, hosts: Vec<Host>) -> (GroupId, Vec<HostId>) {
    cx.update(|cx| {
        let store = AppState::store(cx);
        let group = store
            .create(
                Group {
                    name: "生产".into(),
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
        let ids = hosts
            .into_iter()
            .map(|h| {
                store
                    .create(h, Some(group.as_str().into()), Position::Last)
                    .unwrap()
            })
            .collect();
        (group, ids)
    })
}

fn open_group(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    group: &GroupId,
) -> Entity<BatchPanel> {
    let hosts = cx.update(|cx| crate::sidebar::host_tree::hosts_under(&AppState::store(cx), group));
    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |w, cx| w.open_batch_for_hosts(hosts, window, cx))
    })
    .unwrap();
    cx.run_until_parked();
    cx.update(|cx| workspace.read(cx).live_batches().last().cloned().unwrap())
}

fn run(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    panel: &Entity<BatchPanel>,
    command: &str,
) {
    cx.update_window(handle, |_, window, cx| {
        panel.update(cx, |p, cx| {
            p.input
                .update(cx, |s, cx| s.set_value(command.to_string(), window, cx));
            p.execute(window, cx);
        })
    })
    .unwrap();
    cx.run_until_parked();
}

fn wait_done(cx: &mut TestAppContext, panel: &Entity<BatchPanel>) {
    poll_until(cx, "批量执行结束", |cx| {
        cx.update(|cx| {
            let p = panel.read(cx);
            !p.is_running() && p.results.as_ref().is_some_and(|r| r.done())
        })
    });
}

fn statuses(cx: &mut TestAppContext, panel: &Entity<BatchPanel>) -> Vec<(String, Progress)> {
    cx.update(|cx| {
        panel
            .read(cx)
            .results
            .as_ref()
            .map(|r| {
                r.rows
                    .iter()
                    .map(|row| (row.name.to_string(), row.progress))
                    .collect()
            })
            .unwrap_or_default()
    })
}

fn audit_entries(cx: &mut TestAppContext) -> Vec<termlet_core::audit::AuditEntry> {
    cx.update(|cx| {
        AuditLog::new(AppState::store(cx))
            .unwrap()
            .list(&AuditQuery::default())
            .unwrap()
    })
}

/// 测试容器的两台主机，指纹已确认（批量执行不能询问）。
fn server_group(cx: &mut TestAppContext) -> Option<(GroupId, Vec<HostId>)> {
    let (address, port) = test_server()?;
    let (group, ids) = group_with(
        cx,
        vec![host("s1", &address, port), host("s2", &address, port)],
    );
    let store = cx.update(|cx| AppState::store(cx));
    for id in &ids {
        store
            .set_secret(
                &Owner::host(id),
                SecretField::Password,
                Some(&SecretString::new("termlet-e2e")),
            )
            .unwrap();
    }
    let plan = termlet_core::ssh::plan_for_host(&store, &ids[0]).unwrap();
    let accept: termlet_core::ssh::Prompter = std::sync::Arc::new(|prompt| {
        Box::pin(async move {
            match prompt {
                termlet_core::ssh::Prompt::HostKey(_) => termlet_core::ssh::Answer::AcceptHostKey,
                _ => termlet_core::ssh::Answer::Cancel,
            }
        })
    });
    let connected = termlet_core::runtime::runtime()
        .block_on(termlet_core::ssh::connect(
            store.clone(),
            &plan,
            accept,
            &termlet_core::ssh::ConnectOptions::default(),
        ))
        .unwrap();
    termlet_core::runtime::runtime().block_on(connected.disconnect());
    Some((group, ids))
}

// 工具栏的“批量执行”（默认隐藏，在设置中打开）：新开批量执行 Tab 并先选主机，勾选分组即选中其中的主机（含子分组）；
// 分组的右键菜单中不再有“批量执行…”
#[gpui_kit::test]
fn toolbar_button_opens_a_tab_and_picks_a_group(cx: &mut TestAppContext) {
    let (handle, workspace) = setup(cx);
    let (group, ids) = group_with(
        cx,
        vec![host("web-1", "127.0.0.1", 1), host("web-2", "127.0.0.1", 1)],
    );
    cx.update(|cx| {
        let store = AppState::store(cx);
        let sub = store
            .create(
                Group {
                    name: "db".into(),
                    ..Default::default()
                },
                Some(group.as_str().into()),
                Position::Last,
            )
            .unwrap();
        store
            .create(
                host("db-1", "127.0.0.1", 1),
                Some(sub.as_str().into()),
                Position::Last,
            )
            .unwrap();
    });
    expand_host_tree(cx);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("title-batch-run").is_none(),
            "工具栏的按钮默认隐藏"
        );
        window.right_click(format!("row-group-{}", group.as_str()), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let menu = window.within("popup-menu");
        let labels: Vec<String> = (0..20usize)
            .filter_map(|i| {
                menu.try_find(i)
                    .and_then(|item| item.label().map(String::from))
            })
            .collect();
        assert!(!labels.is_empty(), "弹出了分组的菜单");
        assert!(
            !labels.iter().any(|l| l == "批量执行…"),
            "分组的菜单中没有“批量执行…”：{labels:?}"
        );
        window.press("escape", cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update(|cx| {
        let mut entries = crate::toolbar::defaults();
        for entry in &mut entries {
            if entry.0 == crate::toolbar::ToolbarItem::BatchRun {
                entry.1 = true;
            }
        }
        AppState::store(cx)
            .update_local_settings(|s| s.window.toolbar = crate::toolbar::to_saved(&entries, &[]))
            .unwrap()
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("title-batch-run", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let panel = cx.update(|cx| workspace.read(cx).live_batches()[0].clone());
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("batch-pick-ok").is_some(), "先选主机");
        window.click(format!("batch-pick-group-{}", group.as_str()), cx);
        window.render_frame(cx);
        window.click("batch-pick-ok", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update(|cx| {
        let p = panel.read(cx);
        assert_eq!(p.kind(), TargetKind::Hosts);
        assert_eq!(p.hosts.len(), 3, "含子分组中的主机");
        assert!(p.hosts.contains(&ids[0]));
        assert_eq!(p.title_text(cx), "批量执行 · 3 台主机");
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("batch-pick-ok").is_none(), "选好后关掉");
        assert!(window.try_find("batch-panel").is_some());
        assert_eq!(window.find("batch-target-hosts").label(), Some("3 台主机"));
    })
    .unwrap();
}

// Telnet、串口主机不能批量执行：列为未执行并写明原因，不能重试，也不写审计
#[gpui_kit::test]
fn non_ssh_hosts_are_not_run(cx: &mut TestAppContext) {
    let (handle, workspace) = setup(cx);
    let (group, _) = group_with(
        cx,
        vec![Host {
            protocol: Protocol::Telnet,
            ..host("sw-1", "127.0.0.1", 23)
        }],
    );
    let panel = open_group(cx, handle, &workspace, &group);
    run(cx, handle, &panel, "show version");
    cx.update(|cx| {
        let p = panel.read(cx);
        assert!(!p.is_running());
        let results = p.results.as_ref().unwrap();
        assert_eq!(
            results.rows[0].progress,
            Progress::Done(BatchStatus::Skipped)
        );
        assert!(
            results.rows[0]
                .reason
                .as_ref()
                .unwrap()
                .contains("不是 SSH 主机")
        );
        assert!(results.retry_keys().is_empty());
    });
    assert!(audit_entries(cx).is_empty());
}

// 连不上的主机：结果中列为连不上，审计日志中一条，来源“批量执行”、带批 ID，结果“出错”
#[gpui_kit::test]
fn unreachable_hosts_are_listed_and_audited(cx: &mut TestAppContext) {
    let (handle, workspace) = setup(cx);
    let (group, ids) = group_with(cx, vec![host("gone", "127.0.0.1", 1)]);
    let panel = open_group(cx, handle, &workspace, &group);
    run(cx, handle, &panel, "uptime");
    wait_done(cx, &panel);
    assert_eq!(
        statuses(cx, &panel),
        [("gone".to_string(), Progress::Done(BatchStatus::Unreachable))]
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("batch-problems").is_some());
    })
    .unwrap();
    let entries = audit_entries(cx);
    assert_eq!(entries.len(), 1, "{entries:?}");
    let entry = &entries[0];
    assert_eq!(entry.host_id, ids[0].as_str());
    assert_eq!(entry.caller, CALLER_BATCH);
    assert_eq!(entry.source, AuditSource::Batch);
    assert_eq!(entry.status, AuditStatus::Failed);
    assert_eq!(entry.detail, "uptime");
    assert!(!entry.batch.is_empty());
    // 重试：同一条命令再执行一次，新的一批
    cx.update_window(handle, |_, window, cx| {
        panel.update(cx, |p, cx| p.retry(window, cx))
    })
    .unwrap();
    wait_done(cx, &panel);
    let entries = audit_entries(cx);
    assert_eq!(entries.len(), 2);
    assert_ne!(entries[0].batch, entries[1].batch);
}

// “已打开的终端”：本地终端不能在终端里执行（要 bash / sh），列为未执行；撰写栏再次“执行并对比”时沿用没在执行的 Tab
#[gpui_kit::test]
fn local_terminals_are_not_run_and_the_tab_is_reused(cx: &mut TestAppContext) {
    let (handle, workspace) = setup(cx);
    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |w, cx| w.open_local_terminal(window, cx))
    })
    .unwrap();
    poll_until(cx, "本地终端启动", |cx| {
        cx.update(|cx| {
            workspace
                .read(cx)
                .live_terminals()
                .iter()
                .any(|t| t.read(cx).is_live(cx))
        })
    });
    let terminals = cx.update(|cx| workspace.read(cx).live_terminals());
    for _ in 0..2 {
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| {
                w.run_batch_in_terminals(terminals.clone(), "echo hi", window, cx)
            })
        })
        .unwrap();
        cx.run_until_parked();
    }
    let batches = cx.update(|cx| workspace.read(cx).live_batches());
    assert_eq!(batches.len(), 1, "沿用没在执行的 Tab");
    let panel = batches[0].clone();
    wait_done(cx, &panel);
    cx.update(|cx| {
        let p = panel.read(cx);
        assert_eq!(p.kind(), TargetKind::Terminals);
        assert_eq!(p.history, ["echo hi"]);
        let row = &p.results.as_ref().unwrap().rows[0];
        assert_eq!(row.progress, Progress::Done(BatchStatus::Skipped));
        assert!(!row.retryable(), "本地终端重试也不行");
    });
    assert!(audit_entries(cx).is_empty(), "没有执行的不写审计");
}

// ⚡ 插入快捷命令：选择器中选一条，插入到命令输入框的光标处，不发送
#[gpui_kit::test]
fn snippets_are_inserted_into_the_command(cx: &mut TestAppContext) {
    let (handle, workspace) = setup(cx);
    cx.update(|cx| {
        AppState::store(cx)
            .create(
                Snippet {
                    name: "磁盘".into(),
                    content: "df -h /\n".into(),
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
    });
    let (group, _) = group_with(cx, vec![host("web-1", "127.0.0.1", 1)]);
    let panel = open_group(cx, handle, &workspace, &group);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("batch-insert-snippet", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("command-picker").is_some());
        window.press("enter", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update(|cx| {
        let p = panel.read(cx);
        assert_eq!(p.input.read(cx).value().as_ref(), "df -h /");
        assert!(p.results.is_none(), "只插入，不执行");
    });
}

// Tab 菜单的“移到新窗口”：批量执行 Tab 连同命令移过去
#[gpui_kit::test]
fn the_tab_moves_to_a_new_window(cx: &mut TestAppContext) {
    let (handle, workspace) = setup(cx);
    let (group, _) = group_with(cx, vec![host("web-1", "127.0.0.1", 1)]);
    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |w, cx| w.open_local_terminal(window, cx))
    })
    .unwrap();
    let panel = open_group(cx, handle, &workspace, &group);
    cx.update_window(handle, |_, window, cx| {
        panel.update(cx, |p, cx| {
            p.input
                .update(cx, |s, cx| s.set_value("uptime", window, cx))
        })
    })
    .unwrap();
    let title: ElementId = ("batch-tab-title", panel.entity_id().as_u64() as usize).into();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.right_click(title, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let mut menu = window.within("popup-menu");
        let ix = (0..40usize)
            .find(|i| {
                menu.try_find(*i)
                    .is_some_and(|item| item.label() == Some("移到新窗口"))
            })
            .expect("Tab 菜单中有“移到新窗口”");
        menu.click(ix, cx);
    })
    .unwrap();
    poll_until(cx, "移到新窗口", |cx| {
        cx.update(|cx| workspace.read(cx).live_batches().is_empty()) && cx.windows().len() == 2
    });
    let moved = cx.windows().into_iter().find(|w| *w != handle).unwrap();
    let other = cx
        .update_window(moved, |_, window, cx| {
            Workspace::in_window(window, cx).unwrap()
        })
        .unwrap();
    cx.update(|cx| {
        let batches = other.read(cx).live_batches();
        assert_eq!(batches.len(), 1);
        assert_eq!(
            batches[0].read(cx).input.read(cx).value().as_ref(),
            "uptime"
        );
    });
}

// 真实的服务器：输出相同的合并成一组，不同的另成一组；每台一条审计，同一批的批 ID 相同
#[gpui_kit::test]
fn outputs_from_real_hosts_are_grouped(cx: &mut TestAppContext) {
    let (handle, workspace) = setup(cx);
    let Some((group, _)) = server_group(cx) else {
        eprintln!("没有设置 TERMLET_TEST_SSH，跳过");
        return;
    };
    let panel = open_group(cx, handle, &workspace, &group);
    run(cx, handle, &panel, "echo same");
    wait_done(cx, &panel);
    cx.update(|cx| {
        let results = panel.read(cx).results.as_ref().unwrap();
        assert_eq!(results.groups.len(), 1, "{:?}", results.rows);
        assert_eq!(results.groups[0].members.len(), 2);
        assert_eq!(results.groups[0].text.as_ref(), "same");
        assert_eq!(results.summary().ok, 2);
    });
    run(cx, handle, &panel, "echo pid-$$; exit 3");
    wait_done(cx, &panel);
    cx.update(|cx| {
        let results = panel.read(cx).results.as_ref().unwrap();
        assert_eq!(results.groups.len(), 2, "每台的 PID 不同");
        assert!(results.groups.iter().all(|g| g.exit_code == 3));
        assert_eq!(results.summary().failed, 2);
        assert_eq!(results.retry_keys().len(), 2);
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("batch-problems").is_none(),
            "有退出码的在组里"
        );
    })
    .unwrap();
    let entries = audit_entries(cx);
    assert_eq!(entries.len(), 4);
    assert!(entries.iter().all(|e| e.source == AuditSource::Batch));
    assert_eq!(entries[0].batch, entries[1].batch);
    assert_ne!(entries[1].batch, entries[2].batch);
    assert_eq!(entries[0].exit_code, Some(3));
}

// 真实的服务器：危险命令先确认（取消则不执行）；执行中停止，都标为已取消，可以重试；还在执行时关闭先确认
#[gpui_kit::test]
fn dangerous_commands_stop_and_close(cx: &mut TestAppContext) {
    let (handle, workspace) = setup(cx);
    let Some((group, _)) = server_group(cx) else {
        eprintln!("没有设置 TERMLET_TEST_SSH，跳过");
        return;
    };
    let panel = open_group(cx, handle, &workspace, &group);
    run(cx, handle, &panel, "sudo reboot --dry-run-x");
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("ok").is_some(), "应弹出确认框");
        window.click("cancel", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update(|cx| assert!(panel.read(cx).results.is_none(), "取消则不执行"));

    run(cx, handle, &panel, "sleep 30");
    poll_until(cx, "两台都在执行", |cx| {
        cx.update(|cx| {
            panel
                .read(cx)
                .results
                .as_ref()
                .is_some_and(|r| r.rows.iter().all(|row| row.progress == Progress::Running))
        })
    });
    // 还在执行时点 ×：先确认
    cx.update_window(handle, |_, window, cx| {
        panel.update(cx, |p, cx| p.request_close(window, cx))
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("ok").is_some(), "关闭前确认");
        window.click("cancel", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("batch-stop", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update(|cx| {
        let p = panel.read(cx);
        assert!(!p.is_running());
        let results = p.results.as_ref().unwrap();
        assert_eq!(results.summary().canceled, 2);
        assert_eq!(results.retry_keys().len(), 2);
    });
    // 服务器上的命令收到 SIGTERM，审计照写（已开始执行）
    poll_until(cx, "停止的两台写了审计", |cx| {
        audit_entries(cx).len() == 2
    });
    std::thread::sleep(Duration::from_millis(100));
    cx.update(|cx| assert!(workspace.read(cx).live_batches().len() == 1));
}
