//! “在 VS Code / Zed 中打开”的 UI 集成测试：菜单中只在装了编辑器、设置允许、连接能打开时出现；配置中有别名时直接
//! 打开，没有时先问，“写入配置并打开”只追加、先备份。装了哪些编辑器、`~/.ssh/config` 的位置与打开链接都换成测试的
//! （不真的检测、不改用户的配置、不启动编辑器）。主机的地址连不上（打开很快失败，不影响菜单）。

use std::path::{Path, PathBuf};

use gpui_kit::component::dock::Panel as _;
use gpui_kit::component::v_flex;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Context, ElementId, Entity, IntoElement,
    ParentElement as _, Render, Styled as _, TestAppContext, Window, WindowBounds, WindowOptions,
    div, px, size,
};
use termlet_core::id::HostId;
use termlet_core::model::{Host, RemoteEditorSetting, SshOptions};
use termlet_core::remote_editor::Editor;
use termlet_core::store::Position;

use super::fake_for_test;
use crate::app_state::AppState;
use crate::sftp::panel::{SftpPanel, SftpTarget};
use crate::t;

fn setup(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    cx.update(gpui_kit::init);
    cx.update(crate::init);
    cx.update(AppState::install_in_memory);
    cx.update(|cx| cx.set_reduce_motion(true));
}

fn add_host(cx: &mut TestAppContext, name: &str, single_channel: bool) -> HostId {
    cx.update(|cx| {
        AppState::store(cx)
            .create(
                Host {
                    name: name.into(),
                    address: "127.0.0.1".into(),
                    port: 1,
                    username: "alice".into(),
                    ssh: SshOptions {
                        single_channel,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap()
    })
}

/// 放一个 SFTP 面板与它的 Tab 标题（平时由停靠区画）。
struct Harness {
    panel: Entity<SftpPanel>,
}

impl Render for Harness {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = self
            .panel
            .update(cx, |panel, cx| panel.title(window, cx).into_any_element());
        v_flex()
            .size_full()
            .child(title)
            .child(div().flex_1().child(self.panel.clone()))
    }
}

/// 打开 `target` 的 SFTP 面板（连不上），当前目录设为 `/srv/www`。
fn open_panel(cx: &mut TestAppContext, target: SftpTarget) -> (AnyWindowHandle, Entity<SftpPanel>) {
    let (handle, panel) = open_panel_with(cx, target, termlet_core::ssh::no_prompt());
    cx.update_window(handle, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.start_at("/srv/www".into(), window, cx)
        })
    })
    .unwrap();
    cx.run_until_parked();
    (handle, panel)
}

/// 打开 `target` 的 SFTP 面板，用 `prompter` 回答连接中的询问。
fn open_panel_with(
    cx: &mut TestAppContext,
    target: SftpTarget,
    prompter: termlet_core::ssh::Prompter,
) -> (AnyWindowHandle, Entity<SftpPanel>) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
            Default::default(),
            size(px(1024.), px(640.)),
        ))),
        ..Default::default()
    };
    let (handle, harness) = cx
        .update(|cx| {
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| Harness {
                    panel: SftpPanel::open("web".into(), target, None, prompter, window, cx),
                })
            })
        })
        .unwrap();
    let panel = cx.update(|cx| harness.read(cx).panel.clone());
    cx.run_until_parked();
    (handle, panel)
}

fn tab_title(panel: &Entity<SftpPanel>) -> ElementId {
    ("sftp-tab-title", panel.entity_id().as_u64() as usize).into()
}

/// 右键 Tab 标题弹出的菜单中的全部文案；有“打开方式 ›”时连同其中的项。
fn tab_menu(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    panel: &Entity<SftpPanel>,
) -> Vec<String> {
    let open_with = t!("sftp.menu.open_with").to_string();
    let mut labels = crate::testkit::menu_labels_in(cx, handle, tab_title(panel), None);
    if labels.contains(&open_with) {
        labels.extend(crate::testkit::menu_labels_in(
            cx,
            handle,
            tab_title(panel),
            Some(&open_with),
        ));
    }
    labels
}

/// 关掉菜单（[`tab_menu`] 已经关了，这里什么也不用做）。
fn close_menu(_: &mut TestAppContext, _: AnyWindowHandle) {}

/// 点菜单中文案为 `label` 的一项（不在顶层时在“打开方式 ›”中）。
fn click_menu(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    panel: &Entity<SftpPanel>,
    label: &str,
) {
    let open_with = t!("sftp.menu.open_with").to_string();
    let top = crate::testkit::menu_labels_in(cx, handle, tab_title(panel), None);
    let submenu = (!top.iter().any(|l| l == label)).then_some(open_with.as_str());
    crate::testkit::click_menu_in(cx, handle, tab_title(panel), submenu, label);
}

fn open_label(editor: Editor) -> String {
    t!("sftp.editor.open", editor = editor.name()).to_string()
}

/// 测试用的临时目录（结束时删除）。
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 临时目录中的 `.ssh/config`（还不存在）。
fn temp_config() -> (TempDir, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "termlet-editor-test-{}",
        termlet_core::id::random_token(8)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(".ssh").join("config");
    (TempDir(dir), path)
}

fn set_editor_setting(cx: &mut TestAppContext, setting: RemoteEditorSetting) {
    cx.update(|cx| {
        AppState::store(cx)
            .update_local_settings(|s| s.window.remote_editor = setting)
            .unwrap()
    });
}

// 没装编辑器时菜单中没有这两项；装了的才有；设置可以只留一个或都不显示；“只用一个通道”的主机（堡垒机）没有
#[gpui_kit::test]
fn menu_items_follow_installed_editors_and_setting(cx: &mut TestAppContext) {
    setup(cx);
    let (_dir, config) = temp_config();
    let web = add_host(cx, "web", false);
    let (handle, panel) = open_panel(cx, SftpTarget::Host(web));
    let code = open_label(Editor::VsCode);
    let zed = open_label(Editor::Zed);

    cx.update(|cx| fake_for_test(Vec::new(), config.clone(), cx));
    let labels = tab_menu(cx, handle, &panel);
    assert!(
        !labels.contains(&code) && !labels.contains(&zed),
        "没装编辑器时不显示：{labels:?}"
    );
    close_menu(cx, handle);

    cx.update(|cx| fake_for_test(vec![Editor::VsCode], config.clone(), cx));
    let labels = tab_menu(cx, handle, &panel);
    assert!(labels.contains(&code), "装了 VS Code：{labels:?}");
    assert!(!labels.contains(&zed), "没装 Zed");
    close_menu(cx, handle);

    cx.update(|cx| fake_for_test(vec![Editor::VsCode, Editor::Zed], config.clone(), cx));
    assert!(tab_menu(cx, handle, &panel).contains(&zed));
    close_menu(cx, handle);
    set_editor_setting(cx, RemoteEditorSetting::Zed);
    let labels = tab_menu(cx, handle, &panel);
    assert!(
        labels.contains(&zed) && !labels.contains(&code),
        "只用 Zed：{labels:?}"
    );
    close_menu(cx, handle);
    set_editor_setting(cx, RemoteEditorSetting::Off);
    let labels = tab_menu(cx, handle, &panel);
    assert!(
        !labels.contains(&zed) && !labels.contains(&code),
        "不显示：{labels:?}"
    );
    close_menu(cx, handle);

    set_editor_setting(cx, RemoteEditorSetting::Auto);
    let bastion = add_host(cx, "bastion", true);
    let (handle, panel) = open_panel(cx, SftpTarget::Host(bastion));
    let labels = tab_menu(cx, handle, &panel);
    assert!(!labels.contains(&code), "堡垒机不显示：{labels:?}");
}

// ~/.ssh/config 中有这台主机的别名：直接打开，用别名
#[gpui_kit::test]
fn alias_in_config_opens_directly(cx: &mut TestAppContext) {
    setup(cx);
    let (_dir, config) = temp_config();
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(
        &config,
        "Host web-box\n    HostName 127.0.0.1\n    Port 1\n    User alice\n",
    )
    .unwrap();
    let opened =
        cx.update(|cx| fake_for_test(vec![Editor::VsCode, Editor::Zed], config.clone(), cx));
    let web = add_host(cx, "web", false);
    let (handle, panel) = open_panel(cx, SftpTarget::Host(web));
    click_menu(cx, handle, &panel, &open_label(Editor::VsCode));
    click_menu(cx, handle, &panel, &open_label(Editor::Zed));
    assert_eq!(
        opened.borrow().as_slice(),
        &[
            "vscode://vscode-remote/ssh-remote+web-box/srv/www".to_string(),
            "zed://ssh/web-box/srv/www".to_string(),
        ]
    );
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("editor-confirm").is_none(), "有别名时不问");
    })
    .unwrap();
}

// 没有别名：先问，显示要追加的条目；“写入配置并打开”只追加到末尾、先备份，再用新的别名打开
#[gpui_kit::test]
fn missing_alias_asks_then_appends(cx: &mut TestAppContext) {
    setup(cx);
    let (_dir, config) = temp_config();
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    let original = "Host other\n    HostName 10.0.0.9\n";
    std::fs::write(&config, original).unwrap();
    let opened = cx.update(|cx| fake_for_test(vec![Editor::VsCode], config.clone(), cx));
    let web = add_host(cx, "web", false);
    let (handle, panel) = open_panel(cx, SftpTarget::Host(web));
    click_menu(cx, handle, &panel, &open_label(Editor::VsCode));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("editor-confirm").visible(), "没有别名时先问");
        assert!(window.find("editor-entry").visible(), "显示要追加的条目");
    })
    .unwrap();
    assert!(opened.borrow().is_empty(), "确认前不打开");
    // 端口不是 22：推荐写入配置（主按钮）
    cx.update_window(handle, |_, window, cx| {
        window.click("editor-write-config", cx)
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        opened.borrow().as_slice(),
        &["vscode://vscode-remote/ssh-remote+termlet-web/srv/www".to_string()]
    );
    let written = std::fs::read_to_string(&config).unwrap();
    assert!(written.starts_with(original), "原来的内容不变：{written}");
    assert!(
        written.contains("Host termlet-web\n    HostName 127.0.0.1\n    User alice\n    Port 1\n"),
        "追加的条目：{written}"
    );
    let backups = backups_of(&config);
    assert_eq!(backups.len(), 1, "写之前备份");
    assert_eq!(std::fs::read_to_string(&backups[0]).unwrap(), original);

    // 再打开一次：配置中已有别名，直接打开
    click_menu(cx, handle, &panel, &open_label(Editor::VsCode));
    assert_eq!(opened.borrow().len(), 2);
    assert_eq!(backups_of(&config).len(), 1, "没有再写");
}

// 没有别名时也可以直接打开（不写配置）
#[gpui_kit::test]
fn missing_alias_can_open_directly(cx: &mut TestAppContext) {
    setup(cx);
    let (_dir, config) = temp_config();
    let opened = cx.update(|cx| fake_for_test(vec![Editor::Zed], config.clone(), cx));
    let web = add_host(cx, "web", false);
    let (handle, panel) = open_panel(cx, SftpTarget::Host(web));
    click_menu(cx, handle, &panel, &open_label(Editor::Zed));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("editor-open-direct", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        opened.borrow().as_slice(),
        &["zed://ssh/alice@127.0.0.1:1/srv/www".to_string()]
    );
    assert!(!config.exists(), "没有写配置");
}

// 远程栏的右键菜单（测试容器的 sshd）：选中一个目录时打开它，没有选中时打开当前目录
#[gpui_kit::test]
fn list_menu_opens_selected_folder(cx: &mut TestAppContext) {
    use termlet_core::model::SecretField;
    use termlet_core::secret::SecretString;
    use termlet_core::ssh::{Answer, Prompt, Prompter};
    use termlet_core::store::Owner;

    let Some((address, port)) = crate::testkit::test_server() else {
        eprintln!("没有设置 TERMLET_TEST_SSH，跳过");
        return;
    };
    setup(cx);
    let (_dir, config) = temp_config();
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(
        &config,
        format!("Host test-box\n    HostName {address}\n    Port {port}\n    User termlet\n"),
    )
    .unwrap();
    let opened = cx.update(|cx| fake_for_test(vec![Editor::VsCode], config.clone(), cx));
    let store = cx.update(|cx| AppState::store(cx));
    let host = store
        .create(
            Host {
                name: "box".into(),
                address,
                port,
                username: "termlet".into(),
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    store
        .set_secret(
            &Owner::host(&host),
            SecretField::Password,
            Some(&SecretString::new("termlet-e2e")),
        )
        .unwrap();
    let prompter: Prompter = std::sync::Arc::new(|prompt| {
        Box::pin(async move {
            match prompt {
                Prompt::HostKey(_) => Answer::AcceptHostKey,
                _ => Answer::Cancel,
            }
        })
    });
    let (handle, panel) = open_panel_with(cx, SftpTarget::Host(host), prompter);
    crate::testkit::wait_until(cx, handle, "连上", |window, _| {
        window
            .try_find("sftp-path")
            .is_some_and(|input| input.value() == Some("/home/termlet"))
    });
    // 一个只有子目录 sub 的临时目录
    let base = format!("/tmp/termlet-editor-{}", termlet_core::id::random_token(8));
    let session = cx
        .update(|cx| panel.read(cx).current_lease())
        .expect("连接着")
        .session()
        .clone();
    {
        let (session, base) = (session.clone(), base.clone());
        termlet_core::runtime::runtime()
            .block_on(async move {
                session.mkdir(&base).await?;
                session.mkdir(&format!("{base}/sub")).await
            })
            .expect("建临时目录");
    }
    cx.update_window(handle, |_, window, cx| {
        panel.update(cx, |panel, cx| panel.start_at(base.clone(), window, cx))
    })
    .unwrap();
    crate::testkit::wait_until(cx, handle, "列出临时目录", |window, _| {
        window.try_find("remote-list-row-sub").is_some()
    });
    let label = open_label(Editor::VsCode);
    cx.update_window(handle, |_, window, cx| {
        window.right_click("remote-list-row-sub", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let ix = cx
        .update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let menu = window.within("popup-menu");
            (0..40)
                .map_while(|ix| menu.try_find(ix))
                .position(|item| item.label() == Some(label.as_str()))
        })
        .unwrap()
        .expect("选中目录时菜单中有“在 VS Code 中打开”");
    cx.update_window(handle, |_, window, cx| {
        window.within("popup-menu").click(ix, cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        opened.borrow().as_slice(),
        &[format!(
            "vscode://vscode-remote/ssh-remote+test-box{base}/sub"
        )]
    );
    let _ = termlet_core::runtime::runtime().block_on(session.remove(&base));
}

/// `config` 的备份文件。
fn backups_of(config: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(config.parent().unwrap())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("config.termlet-backup-"))
        })
        .collect()
}

// 新加的文案都在 locales 中
#[test]
fn texts_exist() {
    for key in [
        "sftp.editor.unsupported",
        "sftp.editor.obstacles",
        "sftp.editor.obstacle.proxy",
        "sftp.editor.obstacle.proxy_command",
        "sftp.editor.obstacle.stored_key",
        "sftp.editor.entry_hint",
        "sftp.editor.open_direct",
        "sftp.editor.write_and_open",
        "settings.remote_editor",
        "settings.remote_editor.auto",
        "settings.remote_editor.vscode",
        "settings.remote_editor.zed",
        "settings.remote_editor.off",
        "settings.remote_editor_hint",
        "settings.remote_editor_none",
    ] {
        assert_ne!(t!(key).as_ref(), key, "缺少文案 {key}");
    }
    assert!(t!("sftp.editor.open", editor = "VS Code").contains("VS Code"));
}
