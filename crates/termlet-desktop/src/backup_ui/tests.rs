//! 导入、导出、备份页的 UI 集成测试：在无头窗口中打开生产环境的对话框与页面，经真实的事件路径操作
//! （按元素 ID 查找，与界面语言无关），文件选择对话框用测试平台模拟。临时文件放在系统的临时目录中，测试结束时删除。

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Context, Entity, IntoElement, ParentElement as _,
    Render, Styled as _, Subscription, TestAppContext, Window, WindowBounds, WindowOptions, div,
    px, size,
};
use termlet_core::import::Source;
use termlet_core::model::{Host, LocalSettings, Snippet};
use termlet_core::store::Position;

use super::import::ImportFrom;
use super::page::{BackupPage, BackupPageEvent};
use crate::app_state::AppState;
use crate::testkit::wait_until;

/// 测试窗口的内容：空白，或放着备份页（与设置对话框中的位置无关）。
struct Harness {
    page: Option<Entity<BackupPage>>,
    events: Rc<RefCell<Vec<BackupPageEvent>>>,
    _events: Option<Subscription>,
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .p_4()
            .when_some(self.page.clone(), |this, page| this.child(page))
    }
}

fn open(cx: &mut TestAppContext, with_page: bool) -> (AnyWindowHandle, Entity<Harness>) {
    // 导入、备份在核心的运行时（真实的线程）上执行
    cx.executor().allow_parking();
    cx.update(gpui_kit::init);
    cx.update(crate::init);
    cx.update(AppState::install_in_memory);
    cx.update(|cx| cx.set_reduce_motion(true));
    cx.update(|cx| {
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    size(px(1024.), px(720.)),
                ))),
                ..Default::default()
            },
            cx,
            |window, cx| {
                cx.new(|cx| {
                    let events = Rc::new(RefCell::new(Vec::new()));
                    let (page, subscription) = if with_page {
                        let page =
                            cx.new(|cx| BackupPage::new(&LocalSettings::default(), window, cx));
                        let recorded = events.clone();
                        let subscription =
                            cx.subscribe(&page, move |_, _, event: &BackupPageEvent, _| {
                                recorded.borrow_mut().push(event.clone())
                            });
                        (Some(page), Some(subscription))
                    } else {
                        (None, None)
                    };
                    Harness {
                        page,
                        events,
                        _events: subscription,
                    }
                })
            },
        )
        .expect("打开测试窗口")
    })
}

/// 本次测试自己的临时目录（结束时删除）。
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "termlet-backup-ui-{name}-{}",
            termlet_core::id::random_token(8)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn add_host(cx: &mut TestAppContext, name: &str, address: &str, user: &str, port: u16) {
    cx.update(|cx| {
        AppState::store(cx)
            .create(
                Host {
                    name: name.into(),
                    address: address.into(),
                    username: user.into(),
                    port,
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
    });
}

fn host_names(cx: &mut TestAppContext) -> Vec<String> {
    let mut names: Vec<String> = cx.update(|cx| {
        AppState::store(cx)
            .list::<Host>()
            .into_iter()
            .map(|h| h.doc.name)
            .collect()
    });
    names.sort();
    names
}

/// 在输入框中填入文字（先清空）。
fn type_into(window: &mut Window, cx: &mut gpui_kit::App, id: &'static str, text: &str) {
    window.click(id, cx);
    window.press("ctrl-a", cx);
    window.input(text, cx);
}

fn exists(window: &Window, id: &str) -> bool {
    window
        .try_find(gpui_kit::SharedString::from(id.to_string()))
        .is_some()
}

#[gpui_kit::test]
fn ssh_config_is_previewed_and_imported(cx: &mut TestAppContext) {
    let (handle, _) = open(cx, false);
    let dir = TempDir::new("ssh");
    let config = dir.path("config");
    std::fs::write(
        &config,
        "Host web-1\n  HostName 10.0.0.1\n  User root\n\n\
         Host db\n  HostName 10.0.0.2\n  User postgres\n  Port 2222\n\n\
         Host *\n  ServerAliveInterval 30\n",
    )
    .unwrap();
    // 本机已有 db：预览中标为已存在，默认不勾选
    add_host(cx, "数据库", "10.0.0.2", "postgres", 2222);

    let dialog = cx
        .update_window(handle, |_, window, cx| super::import::open(window, cx))
        .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        type_into(window, cx, "import-path", &config.display().to_string());
        window.click("import-preview", cx);
    })
    .unwrap();
    wait_until(cx, handle, "预览列出主机", |window, _| {
        exists(window, "import-row-web-1") && exists(window, "import-row-db")
    });
    assert_eq!(
        cx.read(|cx| dialog.read(cx).selected_keys()),
        ["web-1"],
        "默认只勾选新主机"
    );

    // 单击已存在的那一行：勾选；重复的按“跳过”处理
    cx.update_window(handle, |_, window, cx| {
        window.click("import-row-db", cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert_eq!(
        cx.read(|cx| dialog.read(cx).selected_keys()),
        ["db", "web-1"]
    );
    cx.update_window(handle, |_, window, cx| window.click("import-apply", cx))
        .unwrap();
    wait_until(cx, handle, "导入完成", |window, _| {
        exists(window, "import-done")
    });
    let report = cx.read(|cx| dialog.read(cx).report_counts());
    assert_eq!(report, Some((1, 0, 1, 0)), "新建 web-1，跳过已有的 db");
    assert_eq!(host_names(cx), ["web-1", "数据库"]);
}

#[gpui_kit::test]
fn host_file_round_trips_with_a_password(cx: &mut TestAppContext) {
    let (handle, _) = open(cx, false);
    let dir = TempDir::new("hosts");
    let file = dir.path("hosts.termlet-hosts");
    add_host(cx, "web", "10.0.0.1", "root", 22);
    add_host(cx, "app", "10.0.0.3", "deploy", 22);

    // 导出全部主机，包含凭据：两次密码一致才能导出
    cx.update_window(handle, |_, window, cx| {
        super::export::open(Vec::new(), window, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("export-secrets", cx);
        window.render_frame(cx);
        type_into(window, cx, "export-password", "correct horse");
        type_into(window, cx, "export-password-repeat", "correct horsE");
        window.render_frame(cx);
        window.click("export-submit", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(!file.exists(), "密码不一致时不导出");
    cx.update_window(handle, |_, window, cx| {
        type_into(window, cx, "export-password-repeat", "correct horse");
        window.render_frame(cx);
        window.click("export-submit", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let target = file.clone();
    cx.simulate_new_path_selection(move |_| Some(target));
    wait_until(cx, handle, "导出完成", |window, _| {
        exists(window, "export-done")
    });
    let head = std::fs::read(&file).unwrap();
    assert!(
        head.starts_with(b"termlet-sealed/1"),
        "含凭据的文件整体加密"
    );
    cx.update_window(handle, |_, window, cx| window.click("export-close", cx))
        .unwrap();
    cx.run_until_parked();

    // 导入同一个文件：先要密码，两台都已存在
    let dialog = cx
        .update_window(handle, |_, window, cx| {
            let dialog = super::import::open(window, cx);
            dialog.update(cx, |d, cx| {
                d.switch_source(ImportFrom::HostFile, window, cx)
            });
            dialog
        })
        .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        type_into(window, cx, "import-path", &file.display().to_string());
        window.click("import-preview", cx);
    })
    .unwrap();
    wait_until(cx, handle, "要求输入密码", |window, _| {
        exists(window, "import-password")
    });
    cx.update_window(handle, |_, window, cx| {
        type_into(window, cx, "import-password", "correct horse");
        window.render_frame(cx);
        window.click("import-preview", cx);
    })
    .unwrap();
    wait_until(
        cx,
        handle,
        "预览列出主机文件中的主机",
        |window, cx| {
            dialog.read(cx).item_count() == Some(2) && exists(window, "import-check-visible")
        },
    );
    assert!(
        cx.read(|cx| dialog.read(cx).selected_keys()).is_empty(),
        "都已存在，默认不勾选"
    );
}

// WindTerm 设了主密码：先要主密码，不对时提示；对了才列出主机，快捷命令一并导入
#[gpui_kit::test]
fn windterm_asks_for_the_master_password(cx: &mut TestAppContext) {
    let (handle, _) = open(cx, false);
    let dir = TempDir::new("windterm");
    let profile = dir.path(".wind").join("profiles").join("default.v10");
    std::fs::create_dir_all(profile.join("terminal")).unwrap();
    // 密文由 Python 的 hashlib 与 cryptography 生成（主密码"主密码 pw"），
    // 内容为 {"PasswordEnabled":true,"Password":"p@ss 密码","session.user":"root"}
    std::fs::write(
        profile.join("user.config"),
        r#"{"application.masterPassword": true,
            "application.fingerprint": "rH1oBgV4xqPaj7vD/rVWYOpULez+HT+Ni2PTS8HMD7ecRpdktifOTfzhha6+NcBaOfW9eqV9+tyETaz/u3wH4g=="}"#,
    )
    .unwrap();
    std::fs::write(
        profile.join("terminal").join("user.sessions"),
        r#"[{"session.uuid": "web", "session.label": "web", "session.target": "10.0.0.1",
             "session.autoLogin": "CZkFrey3x1GLLGAtz4CMDRwO1W/i2s+u7p3FgLLk3CRnFrnVzR6zR6v3QtjW+wjTd9o8dLrlnTR4sgJIh+dEUesw1ujpCvPiIp+qnqlSDLo="}]"#,
    )
    .unwrap();
    std::fs::write(
        profile.join("terminal").join("quickbar.config"),
        r#"[{"quick.label": "重启", "quick.text": "systemctl restart app\n", "quick.type": "Send Text"}]"#,
    )
    .unwrap();

    let dialog = cx
        .update_window(handle, |_, window, cx| {
            let dialog = super::import::open(window, cx);
            dialog.update(cx, |d, cx| {
                d.switch_source(ImportFrom::Client(Source::WindTerm), window, cx)
            });
            dialog
        })
        .unwrap();
    cx.run_until_parked();
    let data = dir.path(".wind").display().to_string();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        type_into(window, cx, "import-path", &data);
        window.click("import-preview", cx);
    })
    .unwrap();
    wait_until(cx, handle, "要求输入主密码", |window, _| {
        exists(window, "import-password")
    });
    let preview = |cx: &mut TestAppContext, password: &str| {
        cx.update_window(handle, |_, window, cx| {
            type_into(window, cx, "import-password", password);
            window.render_frame(cx);
            window.click("import-preview", cx);
        })
        .unwrap();
    };
    preview(cx, "不对的密码");
    wait_until(cx, handle, "提示主密码不对", |_, cx| {
        dialog.read(cx).error_text().is_some()
    });
    assert_eq!(cx.read(|cx| dialog.read(cx).item_count()), None);
    preview(cx, "主密码 pw");
    wait_until(
        cx,
        handle,
        "预览列出 WindTerm 的会话",
        |window, cx| dialog.read(cx).item_count() == Some(1) && exists(window, "import-snippets"),
    );
    cx.update_window(handle, |_, window, cx| window.click("import-apply", cx))
        .unwrap();
    wait_until(cx, handle, "导入完成", |window, _| {
        exists(window, "import-done")
    });
    assert_eq!(
        cx.read(|cx| dialog.read(cx).report_counts()),
        Some((1, 0, 0, 0))
    );
    let (user, snippets) = cx.update(|cx| {
        let store = AppState::store(cx);
        let user = store.list::<Host>()[0].doc.username.clone();
        let snippets: Vec<String> = store
            .list::<Snippet>()
            .into_iter()
            .map(|s| s.doc.name)
            .collect();
        (user, snippets)
    });
    assert_eq!(
        (user.as_str(), snippets),
        ("root", vec!["重启".to_string()])
    );
}

/// 打开导入对话框，选好来源，预览 `path`，等到列出 `rows` 台主机。
fn preview_client(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    source: Source,
    path: &Path,
    rows: usize,
) -> Entity<super::import::ImportDialog> {
    let dialog = cx
        .update_window(handle, |_, window, cx| {
            let dialog = super::import::open(window, cx);
            dialog.update(cx, |d, cx| {
                d.switch_source(ImportFrom::Client(source), window, cx)
            });
            dialog
        })
        .unwrap();
    cx.run_until_parked();
    let path = path.display().to_string();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        type_into(window, cx, "import-path", &path);
        window.click("import-preview", cx);
    })
    .unwrap();
    wait_until(cx, handle, "预览列出主机", |_, cx| {
        dialog.read(cx).item_count() == Some(rows)
    });
    dialog
}

/// 主机保存的登录密码。
fn saved_password(cx: &mut TestAppContext, name: &str) -> Option<String> {
    cx.update(|cx| {
        let store = AppState::store(cx);
        let host = store
            .list::<Host>()
            .into_iter()
            .find(|h| h.doc.name == name)
            .unwrap();
        store
            .secret(
                &termlet_core::store::Owner::host(&host.id),
                termlet_core::model::SecretField::Password,
            )
            .unwrap()
            .map(|p| p.expose().to_string())
    })
}

// Netcatty 的 CSV 中密码是明文：默认不导入（不勾选“导入保存的密码”）；勾选后再导入一次，跳过的已有主机只补上
// 还没保存的密码，已保存的不覆盖
#[gpui_kit::test]
fn netcatty_passwords_need_consent(cx: &mut TestAppContext) {
    let (handle, _) = open(cx, false);
    let dir = TempDir::new("netcatty");
    let csv = dir.path("hosts_export_2026-11-09.csv");
    std::fs::write(
        &csv,
        "\u{feff}Groups,Label,Tags,Notes,Hostname/IP,Protocol,Port,Username,Password\r\n\
         Prod,web,,,10.0.0.1,ssh,22,root,pw-1\r\n\
         ,db,,,10.0.0.2,ssh,22,root,pw-2\r\n\
         ,app,,,10.0.0.3,ssh,22,root,pw-3\r\n",
    )
    .unwrap();
    add_host(cx, "数据库", "10.0.0.2", "root", 22);
    add_host(cx, "应用", "10.0.0.3", "root", 22);
    cx.update(|cx| {
        let store = AppState::store(cx);
        let app = store
            .list::<Host>()
            .into_iter()
            .find(|h| h.doc.name == "应用")
            .unwrap();
        store
            .set_secret(
                &termlet_core::store::Owner::host(&app.id),
                termlet_core::model::SecretField::Password,
                Some(&termlet_core::secret::SecretString::new("kept")),
            )
            .unwrap();
    });

    // 第一次：不勾选，只导入新主机
    let dialog = preview_client(cx, handle, Source::Netcatty, &csv, 3);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(exists(window, "import-secrets"), "带密码时给出选项");
        window.click("import-apply", cx);
    })
    .unwrap();
    wait_until(cx, handle, "导入完成", |window, _| {
        exists(window, "import-done")
    });
    assert_eq!(
        cx.read(|cx| dialog.read(cx).report_counts()),
        Some((1, 0, 0, 0))
    );
    assert_eq!(saved_password(cx, "web"), None, "没有同意时不保存密码");
    cx.update_window(handle, |_, window, cx| window.click("import-close", cx))
        .unwrap();
    cx.run_until_parked();

    // 第二次：全选、勾选后导入，三台都已存在而跳过，补上两台还没保存的密码
    let dialog = preview_client(cx, handle, Source::Netcatty, &csv, 3);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("import-select-all", cx);
        window.click("import-secrets", cx);
        window.render_frame(cx);
        window.click("import-apply", cx);
    })
    .unwrap();
    wait_until(cx, handle, "导入完成", |window, _| {
        exists(window, "import-done") && exists(window, "import-passwords-filled")
    });
    assert_eq!(
        cx.read(|cx| dialog.read(cx).report_counts()),
        Some((0, 0, 3, 0))
    );
    assert_eq!(saved_password(cx, "web").as_deref(), Some("pw-1"));
    assert_eq!(saved_password(cx, "数据库").as_deref(), Some("pw-2"));
    assert_eq!(saved_password(cx, "应用").as_deref(), Some("kept"));
}

#[gpui_kit::test]
fn full_backup_is_created_and_restored(cx: &mut TestAppContext) {
    let (handle, harness) = open(cx, true);
    let dir = TempDir::new("full");
    let file = dir.path("all.termlet-backup");
    add_host(cx, "kept", "10.0.0.1", "root", 22);
    let page = cx.read(|cx| harness.read(cx).page.clone().unwrap());
    wait_until(cx, handle, "读出快照列表", |_, cx| {
        page.read(cx).snapshot_count().is_some()
    });

    // 创建备份：两次密码、选择保存位置
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("backup-create", cx);
        window.render_frame(cx);
        type_into(window, cx, "backup-password", "backup password");
        type_into(window, cx, "backup-password-repeat", "backup password");
        window.render_frame(cx);
        window.click("backup-create-submit", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let target = file.clone();
    cx.simulate_new_path_selection(move |_| Some(target));
    wait_until(cx, handle, "备份完成", |window, _| {
        exists(window, "backup-message") && !exists(window, "backup-create-submit")
    });
    assert!(file.exists());

    // 备份之后新加的主机：恢复后不见了
    add_host(cx, "later", "10.0.0.9", "root", 22);
    assert_eq!(host_names(cx), ["kept", "later"]);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("backup-restore", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let picked = file.clone();
    cx.simulate_path_prompt_response(move |_| Some(vec![picked]));
    wait_until(cx, handle, "要求输入备份密码", |window, _| {
        exists(window, "backup-restore-password")
    });
    cx.update_window(handle, |_, window, cx| {
        type_into(window, cx, "backup-restore-password", "wrong password");
        window.render_frame(cx);
        window.click("backup-inspect", cx);
    })
    .unwrap();
    wait_until(cx, handle, "密码错误的提示", |window, _| {
        exists(window, "backup-message")
    });
    cx.update_window(handle, |_, window, cx| {
        type_into(window, cx, "backup-restore-password", "backup password");
        window.render_frame(cx);
        window.click("backup-inspect", cx);
    })
    .unwrap();
    wait_until(cx, handle, "读出备份的说明", |window, _| {
        exists(window, "backup-summary")
    });
    cx.update_window(handle, |_, window, cx| {
        window.click("backup-restore-submit", cx)
    })
    .unwrap();
    wait_until(cx, handle, "恢复完成", |_, cx| {
        !harness.read(cx).events.borrow().is_empty()
    });
    assert_eq!(host_names(cx), ["kept"]);
    let events = cx.read(|cx| harness.read(cx).events.borrow().clone());
    assert!(matches!(
        events.as_slice(),
        [BackupPageEvent::Restored {
            vault_reset: false,
            ..
        }]
    ));
    // 恢复前保存了一份快照，列表重新读取
    wait_until(cx, handle, "快照列表含恢复前的快照", |_, cx| {
        page.read(cx).snapshot_count().is_some_and(|n| n > 0)
    });
}

#[gpui_kit::test]
fn auto_snapshot_draft_is_validated_and_applied(cx: &mut TestAppContext) {
    let (handle, harness) = open(cx, true);
    let page = cx.read(|cx| harness.read(cx).page.clone().unwrap());
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        type_into(window, cx, "backup-keep", "0");
        window.render_frame(cx);
    })
    .unwrap();
    assert!(
        cx.read(|cx| page.read(cx).validate(cx)).is_err(),
        "份数 0 不合法"
    );
    cx.update_window(handle, |_, window, cx| {
        type_into(window, cx, "backup-keep", "12");
        window.click("backup-auto", cx);
        window.render_frame(cx);
    })
    .unwrap();
    let mut local = LocalSettings::default();
    cx.read(|cx| {
        let page = page.read(cx);
        assert!(page.validate(cx).is_ok());
        page.apply(&mut local, cx);
    });
    assert!(!local.backup.auto, "取消了每天自动保存");
    assert_eq!(local.backup.keep, 12);
}

/// 测试中不留下文件：临时目录在 Drop 中删除。
#[test]
fn temp_dir_is_removed() {
    let path: PathBuf;
    {
        let dir = TempDir::new("drop");
        std::fs::write(dir.path("x"), b"x").unwrap();
        path = dir.0.clone();
        assert!(Path::new(&path).exists());
    }
    assert!(!path.exists());
}

// 定时备份：设置（文件夹、保留份数、密码）后立即备份一次，按份数删掉旧的定时备份，手动保存的备份不动
#[gpui_kit::test]
fn schedule_is_set_up_and_runs_now(cx: &mut TestAppContext) {
    let (handle, harness) = open(cx, true);
    let dir = TempDir::new("schedule");
    add_host(cx, "web", "10.0.0.1", "root", 22);
    for day in 1..=2 {
        std::fs::write(
            dir.path(&format!("termlet-auto-2020010{day}-030000.termlet-backup")),
            b"old",
        )
        .unwrap();
    }
    std::fs::write(dir.path("termlet-backup-manual.termlet-backup"), b"manual").unwrap();
    let page = cx.read(|cx| harness.read(cx).page.clone().unwrap());
    let schedule = cx.read(|cx| AppState::global(cx).schedule.clone());

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("schedule-setup", cx);
        window.render_frame(cx);
        type_into(window, cx, "schedule-folder", &dir.0.display().to_string());
        type_into(window, cx, "schedule-keep", "2");
        type_into(window, cx, "schedule-password", "correct horse");
        type_into(window, cx, "schedule-password-repeat", "correct horsE");
        window.render_frame(cx);
        window.click("schedule-save", cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert!(schedule.status().config.is_none(), "两次密码不一致时不保存");
    cx.update_window(handle, |_, window, cx| {
        type_into(window, cx, "schedule-password-repeat", "correct horse");
        window.render_frame(cx);
        window.click("schedule-save", cx);
    })
    .unwrap();
    wait_until(cx, handle, "保存定时备份", |window, cx| {
        !page.read(cx).is_busy() && exists(window, "schedule-status")
    });
    let config = schedule.status().config.expect("已设置定时备份");
    assert_eq!((config.folder.as_path(), config.keep), (dir.0.as_path(), 2));
    assert!(schedule.status().next.is_some());

    cx.update_window(handle, |_, window, cx| window.click("schedule-run", cx))
        .unwrap();
    wait_until(cx, handle, "立即备份", |window, cx| {
        !page.read(cx).is_busy() && exists(window, "schedule-last")
    });
    let record = schedule.status().last.expect("有上次的结果");
    assert!(record.ok(), "{record:?}");
    assert_eq!(record.deleted, 1);
    let mut names: Vec<String> = std::fs::read_dir(&dir.0)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "termlet-auto-20200102-030000.termlet-backup".to_string(),
            record.file.clone().unwrap(),
            "termlet-backup-manual.termlet-backup".to_string(),
        ]
    );
    // 备份可以用设置的密码打开
    let file = dir.path(record.file.as_ref().unwrap());
    let backups = cx.read(|cx| AppState::global(cx).backups.clone());
    let summary = termlet_core::runtime::runtime()
        .block_on(backups.inspect_backup(
            file,
            Some(termlet_core::secret::SecretString::new("correct horse")),
        ))
        .unwrap();
    assert_eq!(summary.counts.hosts, 1);

    // 停止：回到“设置定时备份…”，备份文件不动
    cx.update_window(handle, |_, window, cx| window.click("schedule-disable", cx))
        .unwrap();
    wait_until(cx, handle, "停止定时备份", |window, cx| {
        !page.read(cx).is_busy() && exists(window, "schedule-setup")
    });
    assert!(schedule.status().config.is_none());
    assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 3);
}

/// NyaTerm 导出的快捷命令（按 NyaTerm 提交 daedb58 的 `QuickCommandsExportConfig` 手写，只留测试要用的几条）：
/// 一条与已有的相同，一条含暂停，一条在嵌套的分类中、变量有下拉选项，一条为空。
const NYATERM_EXPORT: &str = r#"{
  "categories": [
    { "id": "ops", "name": "运维", "sort_order": 0 },
    { "id": "k8s", "name": "Kubernetes", "parent_id": "ops", "sort_order": 1 }
  ],
  "commands": [
    { "id": "a", "label": "磁盘", "command": "df -h", "category_id": "ops", "description": "看磁盘",
      "pinned": false, "execution_mode": "execute" },
    { "id": "b", "label": "登录", "command": "admin\\r\\psecret", "pinned": false, "execution_mode": "execute" },
    { "id": "c", "label": "日志", "command": "kubectl logs {{pod}} -c {{容器|app,sidecar}}", "category_id": "k8s",
      "pinned": false, "execution_mode": "append" },
    { "id": "d", "label": "空", "command": " ", "pinned": false, "execution_mode": "execute" }
  ]
}"#;

fn snippets_in_store(cx: &mut TestAppContext) -> Vec<(String, String, String)> {
    cx.update(|cx| {
        AppState::store(cx)
            .list::<Snippet>()
            .into_iter()
            .map(|s| (s.doc.group, s.doc.name, s.doc.content))
            .collect()
    })
}

fn add_snippet(cx: &mut TestAppContext, group: &str, name: &str, content: &str) {
    cx.update(|cx| {
        AppState::store(cx)
            .create(
                Snippet {
                    name: name.into(),
                    content: content.into(),
                    group: group.into(),
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
    });
}

// 快捷命令页的“导入快捷命令…”：打开导入对话框并选好 NyaTerm；预览列出快捷命令与说明，不用勾选就能导入，
// 与已有的相同的跳过
#[gpui_kit::test]
fn nyaterm_snippets_from_the_snippets_page(cx: &mut TestAppContext) {
    open(cx, false);
    let dir = TempDir::new("nyaterm");
    let file = dir.path("nyaterm-quick-commands.json");
    std::fs::write(&file, NYATERM_EXPORT).unwrap();
    add_snippet(cx, "运维", "磁盘", "df -h\n");
    let handle = cx
        .update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        Default::default(),
                        size(px(1024.), px(720.)),
                    ))),
                    ..Default::default()
                },
                cx,
                |window, cx| cx.new(|cx| crate::snippets::list::SnippetList::new(window, cx)),
            )
        })
        .unwrap()
        .0;
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("snippet-import", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let path = file.display().to_string();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(exists(window, "import-dialog"));
        type_into(window, cx, "import-path", &path);
        window.click("import-preview", cx);
    })
    .unwrap();
    wait_until(cx, handle, "预览列出快捷命令", |window, _| {
        exists(window, "import-snippets-list") && exists(window, "import-snippet-notes")
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(!exists(window, "import-search"), "没有主机的搜索与勾选");
        window.click("import-apply", cx);
    })
    .unwrap();
    wait_until(cx, handle, "导入完成", |window, _| {
        exists(window, "import-done") && exists(window, "import-snippets-skipped")
    });
    assert_eq!(
        snippets_in_store(cx),
        [
            ("运维".into(), "磁盘".into(), "df -h\n".into()),
            ("".into(), "登录".into(), "admin\nsecret\n".into()),
            (
                "运维 / Kubernetes".into(),
                "日志".into(),
                "kubectl logs {{pod}} -c {{容器:app}}".into()
            ),
        ]
    );
}

// 预览中的转换说明与跳过数；标题随来源；不是 NyaTerm 的文件给出说明；结果中新增与相同的数量
#[gpui_kit::test]
fn nyaterm_preview_lists_what_is_dropped_or_changed(cx: &mut TestAppContext) {
    let (handle, _) = open(cx, false);
    let dir = TempDir::new("nyaterm-notes");
    let file = dir.path("nyaterm-quick-commands.json");
    std::fs::write(&file, NYATERM_EXPORT).unwrap();
    let wrong = dir.path("hosts.json");
    std::fs::write(&wrong, r#"{"hosts": []}"#).unwrap();
    add_snippet(cx, "", "磁盘", "df -h\n");
    let dialog = cx
        .update_window(handle, |_, window, cx| {
            super::import::open_with(ImportFrom::Client(Source::NyaTerm), window, cx)
        })
        .unwrap();
    cx.run_until_parked();
    assert!(cx.read(|cx| dialog.read(cx).source().snippets_only()));
    let preview = |cx: &mut TestAppContext, path: &Path| {
        let path = path.display().to_string();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            type_into(window, cx, "import-path", &path);
            window.click("import-preview", cx);
        })
        .unwrap();
    };
    preview(cx, &wrong);
    wait_until(cx, handle, "说明不是 NyaTerm 的文件", |_, cx| {
        dialog.read(cx).error_text().is_some()
    });
    preview(cx, &file);
    wait_until(cx, handle, "预览列出快捷命令", |_, cx| {
        dialog.read(cx).snippet_preview().is_some()
    });
    let (names, notes, skipped) = cx.read(|cx| dialog.read(cx).snippet_preview()).unwrap();
    // 分组按第一次出现的顺序
    assert_eq!(names, ["磁盘", "登录", "日志"]);
    assert_eq!(skipped, 1);
    assert_eq!(
        (
            notes.pauses,
            notes.options,
            notes.nested_groups,
            notes.descriptions
        ),
        (1, 1, 1, 1)
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("import-apply", cx);
    })
    .unwrap();
    wait_until(cx, handle, "导入完成", |window, _| {
        exists(window, "import-done")
    });
    assert_eq!(
        cx.read(|cx| dialog.read(cx).report_snippets()),
        Some((2, 1))
    );
}
