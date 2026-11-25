//! 以 root 身份打开 SFTP 的 UI 集成测试：主机菜单与 SFTP 面板菜单中的入口、Tab 标题与工具栏上的 root 标记、
//! sudo 密码的对话框；另有两栏的键盘操作与本机栏的筛选。菜单项按本线程语言的文案核对；主机的地址连不上（打开很快
//! 失败，不影响这些界面）。

use std::cell::RefCell;
use std::rc::Rc;

use futures::channel::oneshot;
use gpui_kit::component::dock::Panel as _;
use gpui_kit::component::v_flex;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Context, ElementId, Entity, IntoElement,
    ParentElement as _, Render, Styled as _, Subscription, TestAppContext, Window, WindowBounds,
    WindowOptions, div, px, size,
};
use termlet_core::id::HostId;
use termlet_core::model::{Host, SshOptions};
use termlet_core::secret::SecretString;
use termlet_core::sftp::SudoPrompt;
use termlet_core::store::Position;

use super::panel::{SftpPanel, SftpPanelEvent, SftpTarget};
use super::sudo_dialog::{self, SudoRequest};
use crate::app_state::AppState;
use crate::t;
use crate::testkit::{test_server, wait_until};

fn setup(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    cx.update(gpui_kit::init);
    cx.update(crate::init);
    cx.update(AppState::install_in_memory);
    // 对话框的入场动画按真实时间推进，动画中元素的位置在变，点击会偶尔落空
    cx.update(|cx| cx.set_reduce_motion(true));
}

fn options() -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
            Default::default(),
            size(px(1024.), px(640.)),
        ))),
        ..Default::default()
    }
}

/// 一台连不上的 SSH 主机。
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

/// 弹出的菜单中第 `index` 项的文案。
fn menu_label(cx: &mut TestAppContext, handle: AnyWindowHandle, index: usize) -> Option<String> {
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window
            .within("popup-menu")
            .try_find(index)
            .and_then(|item| item.label().map(str::to_string))
    })
    .unwrap()
}

// 主机的右键菜单：SSH 主机在“打开 SFTP”之后有“以 root 身份打开 SFTP”，打开的 Tab 标题与工具栏上有 root 标记；
// “只用一个通道”的主机没有这一项
#[gpui_kit::test]
fn host_menu_opens_sftp_as_root(cx: &mut TestAppContext) {
    setup(cx);
    let web = add_host(cx, "web", false);
    let bastion = add_host(cx, "bastion", true);
    let (handle, _) = cx
        .update(|cx| {
            gpui_kit::open_window(options(), cx, |window, cx| {
                cx.new(|cx| crate::Workspace::new(window, cx))
            })
        })
        .unwrap();
    cx.run_until_parked();
    crate::testkit::expand_host_tree(cx);

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.right_click(format!("row-host-{bastion}"), cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(menu_label(cx, handle, 1), Some(t!("sftp.open").to_string()));
    assert_eq!(
        menu_label(cx, handle, 2),
        Some(t!("history.open").to_string()),
        "只用一个通道的主机不能以 root 身份打开"
    );
    cx.update_window(handle, |_, window, cx| window.press("escape", cx))
        .unwrap();
    cx.run_until_parked();

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("sftp-tab-root-badge").is_none());
        window.right_click(format!("row-host-{web}"), cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(menu_label(cx, handle, 1), Some(t!("sftp.open").to_string()));
    assert_eq!(
        menu_label(cx, handle, 2),
        Some(t!("sftp.open_root").to_string())
    );
    cx.update_window(handle, |_, window, cx| {
        window.within("popup-menu").click(2usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.find("sftp-tab-root-badge").visible(),
            "Tab 标题上有 root 标记"
        );
        assert!(
            window.find("sftp-root-badge").visible(),
            "工具栏上有 root 标记"
        );
    })
    .unwrap();
}

/// 放一个 SFTP 面板与它的 Tab 标题（平时由停靠区画），记下面板请求“以 root 身份打开”的次数。
struct Harness {
    panel: Entity<SftpPanel>,
    _events: Subscription,
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

fn open_panel(
    cx: &mut TestAppContext,
    target: SftpTarget,
) -> (AnyWindowHandle, Entity<SftpPanel>, Rc<RefCell<usize>>) {
    let asked = Rc::new(RefCell::new(0));
    let counter = asked.clone();
    let (handle, harness) = cx
        .update(|cx| {
            gpui_kit::open_window(options(), cx, |window, cx| {
                cx.new(|cx| {
                    let panel = SftpPanel::open(
                        "web".into(),
                        target,
                        None,
                        termlet_core::ssh::no_prompt(),
                        window,
                        cx,
                    );
                    let events = cx.subscribe(&panel, move |_, _, event: &SftpPanelEvent, _| {
                        if matches!(event, SftpPanelEvent::OpenAsRoot) {
                            *counter.borrow_mut() += 1;
                        }
                    });
                    Harness {
                        panel,
                        _events: events,
                    }
                })
            })
        })
        .unwrap();
    let panel = cx.update(|cx| harness.read(cx).panel.clone());
    cx.run_until_parked();
    (handle, panel, asked)
}

fn tab_title(panel: &Entity<SftpPanel>) -> ElementId {
    ("sftp-tab-title", panel.entity_id().as_u64() as usize).into()
}

// SFTP 面板（Tab 的右键菜单）：普通的有“以 root 身份打开 SFTP”，点了请求工作区另开；以 root 打开的有标记、没有这一项
#[gpui_kit::test]
fn panel_menu_offers_root_and_marks_root_tabs(cx: &mut TestAppContext) {
    setup(cx);
    let web = add_host(cx, "web", false);
    let (handle, panel, asked) = open_panel(cx, SftpTarget::Host(web.clone()));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("sftp-tab-root-badge").is_none());
        assert!(window.try_find("sftp-root-badge").is_none());
        window.right_click(tab_title(&panel), cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        menu_label(cx, handle, 2),
        Some(t!("sftp.open_root").to_string())
    );
    cx.update_window(handle, |_, window, cx| {
        window.within("popup-menu").click(2usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(*asked.borrow(), 1);

    let root = cx.update(|cx| SftpTarget::Host(web).as_root(&AppState::store(cx)));
    assert!(
        matches!(root, Some(SftpTarget::Sudo(_))),
        "SSH 主机应能以 root 身份打开：{root:?}"
    );
    let (handle, panel, _) = open_panel(cx, root.unwrap());
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("sftp-tab-root-badge").visible());
        assert!(window.find("sftp-root-badge").visible());
        window.right_click(tab_title(&panel), cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        menu_label(cx, handle, 2),
        Some(t!("hosts.menu.copy_ip").to_string()),
        "已经是 root 的没有这一项（第 2 项是复制 IP）"
    );
}

// 键盘：Tab 在本机栏与远程栏的列表之间切换；Ctrl+L 编辑本机栏的路径，路径框中 Esc 回到列表
#[gpui_kit::test]
fn keyboard_switches_panes_and_edits_path(cx: &mut TestAppContext) {
    setup(cx);
    let web = add_host(cx, "web", false);
    let (handle, panel, _) = open_panel(cx, SftpTarget::Host(web));
    let (remote, local) = cx.read(|cx| panel.read(cx).list_focus(cx));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("local-list").is_some(), "够宽时有本机栏");
        remote.focus(window, cx);
        window.press("tab", cx);
        assert!(local.is_focused(window), "Tab 到本机栏");
        window.press("shift-tab", cx);
        assert!(remote.is_focused(window), "再切回远程栏");
        local.focus(window, cx);
        window.press("ctrl-l", cx);
    })
    .unwrap();
    // 列表发出的事件在这次更新结束后才送到本机栏
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        assert!(!local.is_focused(window), "焦点到了路径框");
        window.press("escape", cx);
        assert!(local.is_focused(window), "Esc 回到列表");
    })
    .unwrap();
}

// 本机栏的筛选：Ctrl+F 只显示名称匹配的项，Enter 回到列表保留筛选；F7 新建的文件夹不匹配时清空筛选再选中它
#[gpui_kit::test]
fn local_pane_filters_and_selects_the_new_folder(cx: &mut TestAppContext) {
    setup(cx);
    let dir = std::env::temp_dir().join(format!(
        "termlet-sftp-filter-{}",
        termlet_core::id::random_token(8)
    ));
    std::fs::create_dir_all(dir.join("logs")).unwrap();
    for name in ["access.log", "app.py", "notes.txt"] {
        std::fs::write(dir.join(name), "x").unwrap();
    }
    let start = dir.clone();
    cx.update(|cx| {
        AppState::ui(cx).update(cx, |ui, cx| {
            ui.update(|s| s.sftp_local_dir = Some(start), cx)
        })
    });
    let web = add_host(cx, "web", false);
    let (handle, panel, _) = open_panel(cx, SftpTarget::Host(web));
    wait_until(cx, handle, "本机栏列出临时目录", |window, _| {
        window.try_find("local-list-row-access.log").is_some()
    });
    let (_, local) = cx.read(|cx| panel.read(cx).list_focus(cx));
    cx.update_window(handle, |_, window, cx| {
        local.focus(window, cx);
        window.press("ctrl-f", cx);
        assert!(!local.is_focused(window), "焦点到了筛选框");
        window.input("LOG", cx);
    })
    .unwrap();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("local-list-row-access.log").is_some());
        assert!(
            window.try_find("local-list-row-logs").is_some(),
            "不分大小写"
        );
        assert!(window.try_find("local-list-row-app.py").is_none());
        assert!(
            window.try_find("local-list-find").is_none(),
            "本机栏没有“在终端中查找”"
        );
        window.press("enter", cx);
        assert!(local.is_focused(window), "Enter 回到列表");
        assert!(window.try_find("local-list-filter").is_some(), "筛选保留");
        window.press("f7", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.input("build", cx);
        window.press("enter", cx);
    })
    .unwrap();
    wait_until(cx, handle, "新建的文件夹显示出来", |window, _| {
        window.try_find("local-list-row-build").is_some()
    });
    let list = cx.read(|cx| panel.read(cx).local_list(cx));
    let selected: Vec<String> = cx.read(|cx| {
        list.read(cx)
            .selected()
            .into_iter()
            .map(|row| row.name)
            .collect()
    });
    assert_eq!(selected, ["build"], "选中新建的文件夹");
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("local-list-filter").is_none(), "清空了筛选");
        assert!(window.try_find("local-list-row-app.py").is_some());
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

// 新加的文案都在 locales 中（缺了的键会原样显示键名）
#[test]
fn root_and_encoding_texts_exist() {
    for (key, text) in [
        ("sftp.open_root", t!("sftp.open_root")),
        ("sftp.root_badge", t!("sftp.root_badge")),
        ("sftp.root_hint", t!("sftp.root_hint")),
        ("sftp.sudo.title", t!("sftp.sudo.title")),
        ("sftp.sudo.retry", t!("sftp.sudo.retry")),
        ("sftp.sudo.use_saved", t!("sftp.sudo.use_saved")),
        ("sftp.sudo.open", t!("sftp.sudo.open")),
        (
            "sftp.sudo.error.not_allowed",
            t!("sftp.sudo.error.not_allowed"),
        ),
        (
            "sftp.error.unencodable_name",
            t!("sftp.error.unencodable_name"),
        ),
        ("host.sftp.encoding", t!("host.sftp.encoding")),
        ("host.sftp.encoding_hint", t!("host.sftp.encoding_hint")),
    ] {
        assert_ne!(text.as_ref(), key, "缺少文案 {key}");
    }
    assert!(t!("sftp.sudo.description", host = "web", user = "alice").contains("alice"));
}

// 设置“快捷键”页中列出的 SFTP 键盘操作的文案都在 locales 中
#[test]
fn keyboard_texts_exist() {
    for (key, text) in [
        ("settings.keys.sftp_move", t!("settings.keys.sftp_move")),
        ("settings.keys.sftp_extend", t!("settings.keys.sftp_extend")),
        ("settings.keys.sftp_cursor", t!("settings.keys.sftp_cursor")),
        (
            "settings.keys.sftp_toggle_cursor",
            t!("settings.keys.sftp_toggle_cursor"),
        ),
        (
            "settings.keys.sftp_type_keys",
            t!("settings.keys.sftp_type_keys"),
        ),
        (
            "settings.keys.sftp_type_ahead",
            t!("settings.keys.sftp_type_ahead"),
        ),
        (
            "settings.keys.sftp_new_folder",
            t!("settings.keys.sftp_new_folder"),
        ),
        ("settings.keys.sftp_menu", t!("settings.keys.sftp_menu")),
        ("sftp.new_folder_menu", t!("sftp.new_folder_menu")),
        ("settings.keys.sftp_filter", t!("settings.keys.sftp_filter")),
        ("settings.keys.sftp_path", t!("settings.keys.sftp_path")),
        (
            "settings.keys.sftp_switch_pane",
            t!("settings.keys.sftp_switch_pane"),
        ),
    ] {
        assert_ne!(text.as_ref(), key, "缺少文案 {key}");
    }
}

#[gpui_kit::test]
fn root_target_is_a_separate_host(cx: &mut TestAppContext) {
    setup(cx);
    let web = add_host(cx, "web", false);
    let bastion = add_host(cx, "bastion", true);
    cx.update(|cx| {
        let store = AppState::store(cx);
        let root = SftpTarget::Host(web.clone()).as_root(&store).unwrap();
        // 传输、编辑器按主机标识配对：root 的另算一台
        assert_eq!(root.key(), format!("{web}#root"));
        assert_eq!(root.host(), Some(&web));
        assert!(root.is_root());
        assert!(root.as_root(&store).is_none());
        assert!(SftpTarget::Host(bastion.clone()).as_root(&store).is_none());
        let wsl = SftpTarget::Wsl {
            distro: "Ubuntu".into(),
            user: String::new(),
        };
        assert!(!wsl.is_root());
        let wsl_root = wsl.as_root(&store).unwrap();
        assert!(wsl_root.is_root());
        assert!(wsl_root.as_root(&store).is_none());
    });
}

/// 只用来开对话框的空视图。
struct Blank;

impl Render for Blank {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full()
    }
}

fn open_blank(cx: &mut TestAppContext) -> AnyWindowHandle {
    setup(cx);
    cx.update(|cx| gpui_kit::open_window(options(), cx, |_, cx| cx.new(|_| Blank)))
        .unwrap()
        .0
}

fn prompt(retry: bool) -> SudoPrompt {
    SudoPrompt {
        host: "web".into(),
        username: "alice".into(),
        retry,
    }
}

/// 弹出 sudo 的密码对话框，返回回答的接收端与“对话框已关闭”的接收端。
fn ask(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    retry: bool,
    saved: Option<&str>,
) -> (
    oneshot::Receiver<Option<SecretString>>,
    oneshot::Receiver<bool>,
) {
    let (reply, answer) = oneshot::channel();
    let request = SudoRequest {
        prompt: prompt(retry),
        reply,
    };
    let saved = saved.map(SecretString::new);
    let done = cx
        .update_window(handle, |_, window, cx| {
            let done = sudo_dialog::open(request, saved, window, cx);
            window.render_frame(cx);
            done
        })
        .unwrap();
    cx.run_until_parked();
    (answer, done)
}

fn answered(mut answer: oneshot::Receiver<Option<SecretString>>) -> Option<String> {
    answer
        .try_recv()
        .expect("回答前对话框不应丢弃回答的一方")
        .expect("对话框已回答")
        .map(|password| password.expose().to_string())
}

// sudo 的密码：输入后确定；保存了登录密码时一键使用它；取消时回答取消
#[gpui_kit::test]
fn sudo_dialog_answers(cx: &mut TestAppContext) {
    let handle = open_blank(cx);

    let (answer, mut done) = ask(cx, handle, false, None);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("sudo-use-saved").is_none(),
            "没有保存的密码"
        );
        window.click("sudo-password", cx);
        window.input("s3cret", cx);
        window.click("sudo-ok", cx);
        window.render_frame(cx);
        assert!(window.try_find("sudo-password").is_none(), "确定后关闭");
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(answered(answer), Some("s3cret".into()));
    assert_eq!(done.try_recv().unwrap(), Some(false));

    let (answer, mut done) = ask(cx, handle, false, Some("saved-pw"));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("sudo-use-saved", cx);
        window.render_frame(cx);
        assert!(
            window.try_find("sudo-password").is_none(),
            "用了保存的密码后关闭"
        );
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(answered(answer), Some("saved-pw".into()));
    assert_eq!(done.try_recv().unwrap(), Some(true), "回答的是保存的密码");

    let (answer, _) = ask(cx, handle, true, None);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("sudo-cancel", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(answered(answer), None, "取消");
}

// 真实的 sshd：主机菜单中以 root 身份打开，sudo 要密码时一键使用保存的登录密码，之后能列出 /root
// （测试容器中的 sudo-pw 用户，见 testing/sshd/Dockerfile）
#[gpui_kit::test]
fn root_sftp_uses_the_saved_password(cx: &mut TestAppContext) {
    use termlet_core::model::SecretField;
    use termlet_core::store::Owner;
    let Some((address, port)) = test_server() else {
        return;
    };
    setup(cx);
    let id = cx.update(|cx| {
        let store = AppState::store(cx);
        let id = store
            .create(
                Host {
                    name: "sudo".into(),
                    address,
                    port,
                    username: "sudo-pw".into(),
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
        store
            .set_secret(
                &Owner::host(&id),
                SecretField::Password,
                Some(&SecretString::new("termlet-e2e")),
            )
            .unwrap();
        id
    });
    let (handle, _) = cx
        .update(|cx| {
            gpui_kit::open_window(options(), cx, |window, cx| {
                cx.new(|cx| crate::Workspace::new(window, cx))
            })
        })
        .unwrap();
    cx.run_until_parked();
    crate::testkit::expand_host_tree(cx);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.right_click(format!("row-host-{id}"), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("popup-menu").click(2usize, cx);
    })
    .unwrap();
    // 内存中的存储没有记下主机指纹：接受它
    wait_until(cx, handle, "主机指纹的询问", |window, _| {
        window.try_find("prompt-ok").is_some()
    });
    cx.update_window(handle, |_, window, cx| window.click("prompt-ok", cx))
        .unwrap();
    wait_until(cx, handle, "sudo 的密码询问", |window, _| {
        window.try_find("sudo-use-saved").is_some()
    });
    cx.update_window(handle, |_, window, cx| window.click("sudo-use-saved", cx))
        .unwrap();
    wait_until(cx, handle, "以 root 身份连上", |window, _| {
        window.try_find("remote-list-empty").is_some()
            && window
                .try_find("sftp-path")
                .is_some_and(|input| input.value() == Some("/home/sudo-pw"))
    });
    cx.update_window(handle, |_, window, cx| {
        window.click("sftp-path", cx);
        window.press("ctrl-a", cx);
        window.input("/root", cx);
        window.press("enter", cx);
    })
    .unwrap();
    wait_until(cx, handle, "列出 /root", |window, _| {
        window
            .try_find("sftp-path")
            .is_some_and(|input| input.value() == Some("/root"))
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("sftp-error-close").is_none(), "没有出错");
        assert!(window.find("sftp-root-badge").visible());
    })
    .unwrap();
}

// 没有 SFTP 的服务器（testing/nosftp 的临时容器，TERMLET_TEST_NOSFTP=127.0.0.1:2225）：打开文件管理时改用 SCP，
// 工具栏上有 SCP 标记，列表上方说明受限之处（可以关掉）
#[gpui_kit::test]
fn sftp_falls_back_to_scp_and_says_so(cx: &mut TestAppContext) {
    use termlet_core::model::SecretField;
    use termlet_core::store::Owner;
    let Some((address, port)) = std::env::var("TERMLET_TEST_NOSFTP").ok().and_then(|v| {
        let (host, port) = v.rsplit_once(':')?;
        Some((host.to_string(), port.parse::<u16>().ok()?))
    }) else {
        return;
    };
    setup(cx);
    let id = cx.update(|cx| {
        let store = AppState::store(cx);
        let id = store
            .create(
                Host {
                    name: "nosftp".into(),
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
                &Owner::host(&id),
                SecretField::Password,
                Some(&SecretString::new("termlet-e2e")),
            )
            .unwrap();
        id
    });
    let (handle, _) = cx
        .update(|cx| {
            gpui_kit::open_window(options(), cx, |window, cx| {
                cx.new(|cx| crate::Workspace::new(window, cx))
            })
        })
        .unwrap();
    cx.run_until_parked();
    crate::testkit::expand_host_tree(cx);
    // 主机的右键菜单：打开终端、打开 SFTP……
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.right_click(format!("row-host-{id}"), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("popup-menu").click(1usize, cx);
    })
    .unwrap();
    wait_until(cx, handle, "主机指纹的询问", |window, _| {
        window.try_find("prompt-ok").is_some()
    });
    cx.update_window(handle, |_, window, cx| window.click("prompt-ok", cx))
        .unwrap();
    wait_until(cx, handle, "改用 SCP 连上", |window, _| {
        window.try_find("sftp-scp-badge").is_some()
            && window
                .try_find("sftp-path")
                .is_some_and(|input| input.value() == Some("/home/termlet"))
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("sftp-scp-notice").visible());
        window.click("sftp-scp-notice-close", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("sftp-scp-notice").is_none(),
            "关掉后不再显示"
        );
        assert!(window.find("sftp-scp-badge").visible());
    })
    .unwrap();
}

// 主机对话框（终端页）：SSH 主机可以选 SFTP 文件名编码，保存到主机设置
#[gpui_kit::test]
fn host_dialog_saves_sftp_encoding(cx: &mut TestAppContext) {
    use termlet_core::model::Encoding;
    let handle = open_blank(cx);
    let web = add_host(cx, "web", false);
    cx.update_window(handle, |_, window, cx| {
        crate::host_dialog::open_edit(web.clone(), window, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("host-pages-3", cx);
        window.render_frame(cx);
        assert_eq!(window.find("host-sftp-encoding").value(), Some("UTF-8"));
        // 选项：UTF-8、GBK / GB18030、Big5
        window.within("host-sftp-encoding").click("input", cx);
        window.press("down", cx);
        window.press("enter", cx);
    })
    .unwrap();
    // 选择在按键处理完之后才提交（defer）
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("host-sftp-encoding").value(),
            Some("GBK / GB18030")
        );
        window.click("host-save", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let saved = cx.update(|cx| {
        AppState::store(cx)
            .get::<Host>(&web)
            .unwrap()
            .doc
            .ssh
            .sftp_encoding
    });
    assert_eq!(saved, Encoding::Gb18030);
}
