//! UI 集成测试：在无头窗口中渲染生产环境的视图，经真实的事件路径操作并检查结果。
//!
//! 这里的界面语言是进程全局的：按文案查找、输入的测试先设成简体中文，任何测试都不切换到别的语言（会让并行的
//! 其他测试按中文找不到要点的项）。要切换语言的测试写成桌面端的单元测试（如 `src/sidebar/mod.rs` 中的），
//! 那里 Termlet 的文案按本线程的语言。

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Entity, ScrollDelta, SharedString, TestAppContext,
    WindowBounds, WindowOptions, point, px, size,
};
use termlet_core::model::{Group, Host, Snippet};
use termlet_core::store::Position;
use termlet_core::vault::MemoryKeyStore;
use termlet_desktop::i18n::{self, Language};
use termlet_desktop::{AppState, Workspace};

fn open_workspace(cx: &mut TestAppContext) -> AnyWindowHandle {
    open_workspace_view(cx).0
}

fn open_workspace_view(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Workspace>) {
    open_workspace_sized(cx, size(px(1024.), px(640.)))
}

/// 按指定的窗口大小打开工作区（主窗口的最小尺寸是 720 × 480）。
fn open_workspace_sized(
    cx: &mut TestAppContext,
    window_size: gpui_kit::Size<gpui_kit::Pixels>,
) -> (AnyWindowHandle, Entity<Workspace>) {
    // 终端的伪终端与解析线程是真实的系统线程，不受测试调度器管理
    cx.executor().allow_parking();
    cx.update(gpui_kit::init);
    cx.update(termlet_desktop::init);
    cx.update(AppState::install_in_memory);
    // 对话框的入场动画按真实时间推进，动画中元素的位置在变，点击会偶尔落空
    cx.update(|cx| cx.set_reduce_motion(true));
    cx.update(|cx| {
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    window_size,
                ))),
                ..Default::default()
            },
            cx,
            |window, cx| cx.new(|cx| Workspace::new(window, cx)),
        )
        .expect("打开测试窗口")
    })
}

/// 主机树的分组与节默认全部折叠：点开“主机”一节（上面有本机的终端时才有这个标题）。
fn expand_hosts(cx: &mut TestAppContext, handle: AnyWindowHandle) {
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        if window.try_find("row-hosts").is_some() {
            window.click("row-hosts", cx);
        }
    })
    .unwrap();
    cx.run_until_parked();
}

/// 点开始页的“打开本地终端”，等后台启动好本地 shell。
fn start_local_terminal(cx: &mut TestAppContext, handle: AnyWindowHandle) {
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("start-local-terminal", cx);
    })
    .unwrap();
    cx.run_until_parked();
}

// 设置左侧的分页导航可以用键盘切换：↓ 下一页，End 最后一页，Home 第一页
#[gpui_kit::test]
fn settings_nav_works_with_keyboard(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("open-settings", cx);
        window.render_frame(cx);
        window.click("settings-nav-general", cx);
        window.press("down", cx);
        window.render_frame(cx);
        assert!(
            window.try_find("settings-toolbar").is_some(),
            "↓ 到工具栏页"
        );
        window.press("down", cx);
        window.render_frame(cx);
        assert!(
            window.try_find("settings-font-size").is_some(),
            "再 ↓ 到终端页"
        );
        window.press("end", cx);
        window.render_frame(cx);
        assert!(window.try_find("settings-font-size").is_none());
        window.press("home", cx);
        window.render_frame(cx);
        assert!(
            window.try_find("settings-show-host-address").is_some(),
            "Home 回到通用页"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn settings_dialog_validates_and_saves(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    let before = cx.update(|cx| AppState::store(cx).settings());

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("open-settings", cx);
        window.render_frame(cx);
        // 通用：关掉“在主机列表中显示地址”
        window.click("settings-show-host-address", cx);
        // 终端：字号超出范围时不能保存，停在终端页并提示
        window.click("settings-nav-terminal", cx);
        window.render_frame(cx);
        window.click("settings-font-size", cx);
        window.press("ctrl-a", cx);
        window.input("99", cx);
        window.click("settings-save", cx);
        window.render_frame(cx);
        assert!(
            window.try_find("settings-font-size").is_some(),
            "出错时对话框不关闭"
        );
    })
    .unwrap();
    assert_eq!(
        cx.update(|cx| AppState::store(cx).settings()),
        before,
        "有错误时什么也不保存"
    );

    cx.update_window(handle, |_, window, cx| {
        window.click("settings-font-size", cx);
        window.press("ctrl-a", cx);
        window.input("16", cx);
        window.click("settings-save", cx);
        window.render_frame(cx);
        assert!(window.try_find("settings-save").is_none(), "保存后关闭");
    })
    .unwrap();
    let after = cx.update(|cx| AppState::store(cx).settings());
    assert_eq!(after.terminal.font_size, 16);
    assert!(!after.general.show_host_address);
    assert_eq!(after.connection, before.connection, "没改的组不变");

    // 取消不保存
    cx.update_window(handle, |_, window, cx| {
        window.click("open-settings", cx);
        window.render_frame(cx);
        window.click("settings-confirm-close", cx);
        window.click("settings-cancel", cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert!(
        cx.update(|cx| AppState::store(cx).settings())
            .general
            .confirm_close
    );
}

#[gpui_kit::test]
fn custom_shortcut_replaces_the_default(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("open-settings", cx);
        window.render_frame(cx);
        window.click("settings-nav-shortcuts", cx);
        window.render_frame(cx);
        // 命令面板在“窗口”组：往下滚到看得见为止
        for _ in 0..40 {
            if window
                .try_find("key-remove-commandPalette-ctrl-shift-p")
                .is_some_and(|row| row.visible())
            {
                break;
            }
            window.scroll(
                "settings-page",
                ScrollDelta::Pixels(point(px(0.), px(-100.))),
                cx,
            );
            window.render_frame(cx);
        }
        // 命令面板：去掉默认的 Ctrl+Shift+P，录入 Ctrl+Alt+K
        window.click("key-remove-commandPalette-ctrl-shift-p", cx);
        window.render_frame(cx);
        window.click("key-add-commandPalette", cx);
        window.render_frame(cx);
        assert!(
            window.try_find("key-recording-commandPalette").is_some(),
            "进入录入状态"
        );
        window.press("ctrl-alt-k", cx);
        window.render_frame(cx);
        assert!(
            window.try_find("key-recording-commandPalette").is_none(),
            "录到一个按键后结束"
        );
        window.click("settings-save", cx);
        window.render_frame(cx);
    })
    .unwrap();
    // 存储的变更事件经异步任务转发，之后才重新注册快捷键
    cx.run_until_parked();
    let saved = cx.update(|cx| AppState::store(cx).settings().keybindings.bindings);
    assert_eq!(
        saved.get("commandPalette").map(Vec::as_slice),
        Some(&["ctrl-alt-k".to_string()][..])
    );

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.press("ctrl-shift-p", cx);
        window.render_frame(cx);
        assert!(
            window.try_find("command-palette").is_none(),
            "原来的快捷键不再生效"
        );
        window.press("ctrl-alt-k", cx);
        window.render_frame(cx);
        assert!(
            window.try_find("command-palette").is_some(),
            "新的快捷键打开命令面板"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn xshell_key_preset_is_saved_and_works(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    start_local_terminal(cx, handle);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("open-settings", cx);
        window.render_frame(cx);
        window.click("settings-nav-shortcuts", cx);
        window.render_frame(cx);
        window.click("key-preset-xshell", cx);
        window.render_frame(cx);
        window.click("settings-save", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    let saved = cx.update(|cx| AppState::store(cx).settings().keybindings.bindings);
    let combos = |id: &str| saved.get(id).cloned().unwrap_or_default();
    assert_eq!(combos("tab1"), ["alt-1", "ctrl-alt-1"]);
    assert_eq!(combos("compose"), ["alt-i", "ctrl-shift-m"]);
    assert_eq!(combos("sftpBeside"), ["ctrl-alt-f"]);
    // Xshell 的 Alt+I 打开撰写栏
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("terminal", cx);
        window.press("alt-i", cx);
        window.render_frame(cx);
        assert!(
            window.try_find("compose-send").is_some(),
            "Alt+I 打开撰写栏"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn vault_gate_sets_master_password_when_keyring_is_unavailable(cx: &mut TestAppContext) {
    // 应用锁、云同步、自动快照在核心的运行时上工作，不受测试调度器管理
    cx.executor().allow_parking();
    cx.update(gpui_kit::init);
    cx.update(termlet_desktop::init);
    cx.update(|cx| AppState::install_in_memory_with(Box::new(MemoryKeyStore::unavailable()), cx));
    let (handle, _) = cx.update(|cx| {
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    size(px(1024.), px(640.)),
                ))),
                ..Default::default()
            },
            cx,
            |window, cx| cx.new(|cx| Workspace::new(window, cx)),
        )
        .expect("打开测试窗口")
    });

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("vault-password", cx);
        window.input("correct horse", cx);
        window.click("vault-confirm", cx);
        window.input("different", cx);
        window.click("vault-submit", cx);
        assert!(
            !AppState::store(cx).vault().is_unlocked(),
            "两次输入不一致时不设置"
        );

        window.click("vault-confirm", cx);
        window.press("secondary-a", cx);
        window.input("correct horse", cx);
        window.click("vault-submit", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(AppState::store(cx).vault().is_unlocked());
        assert!(window.try_find("vault-submit").is_none(), "解锁后关卡消失");
    })
    .unwrap();
}

#[gpui_kit::test]
fn local_terminal_opens_and_search_bar_toggles(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    start_local_terminal(cx, handle);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("terminal").is_some(), "应打开终端 Tab");

        window.click("terminal", cx);
        window.press("ctrl-shift-f", cx);
        window.render_frame(cx);
        assert!(window.try_find("search-close").is_some(), "应打开查找栏");

        window.press("escape", cx);
    })
    .unwrap();
    // 关闭经事件通知面板，在这次更新结束后处理
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("search-close").is_none(), "Esc 关闭查找栏");
        assert_eq!(
            window.find("terminal").focused(),
            Some(true),
            "焦点回到终端"
        );
    })
    .unwrap();
}

/// 在终端上右键，返回终端的右键菜单有没有弹出来（弹出来的按 Esc 关掉）。
fn right_click_terminal_opens_menu(cx: &mut TestAppContext, handle: AnyWindowHandle) -> bool {
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.right_click("terminal", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let opened = window.try_find("popup-menu").is_some();
        if opened {
            window.press("escape", cx);
        }
        opened
    })
    .unwrap()
}

// “终端中右键”设为弹出菜单时，右键终端区域弹出菜单；默认（有选中时复制，否则粘贴）不弹
#[gpui_kit::test]
fn right_click_in_the_terminal_opens_the_menu_only_when_set(cx: &mut TestAppContext) {
    use termlet_core::model::RightClick;
    let handle = open_workspace(cx);
    start_local_terminal(cx, handle);
    assert!(!right_click_terminal_opens_menu(cx, handle), "默认不弹菜单");

    cx.update(|cx| {
        AppState::store(cx).update_settings(|s| s.terminal.right_click = RightClick::Menu)
    })
    .unwrap();
    cx.run_until_parked();
    assert!(right_click_terminal_opens_menu(cx, handle), "弹出菜单");
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("terminal").focused(),
            Some(true),
            "菜单关掉后焦点回到终端"
        );
    })
    .unwrap();
}

// 焦点在终端上时 Tab / Shift+Tab 交给终端（shell 补全），不被 gpui-kit 的 Root 拿去切换焦点；对话框中照旧切换焦点
#[gpui_kit::test]
fn tab_goes_to_the_terminal_and_still_moves_focus_in_dialogs(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    start_local_terminal(cx, handle);
    // 被快捷键用掉的按键与动作（交给终端的按键不经过快捷键，不会出现在这里）
    let actions = std::rc::Rc::new(std::cell::RefCell::new(Vec::<&'static str>::new()));
    let _observer = cx.update(|cx| {
        let actions = actions.clone();
        cx.observe_keystrokes(move |event, _, _| {
            if let Some(action) = &event.action {
                actions.borrow_mut().push(action.name());
            }
        })
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("terminal", cx);
        window.press("tab", cx);
        window.press("shift-tab", cx);
        assert_eq!(
            window.find("terminal").focused(),
            Some(true),
            "焦点留在终端上"
        );
    })
    .unwrap();
    assert!(
        actions.borrow().is_empty(),
        "终端中的 Tab 被快捷键用掉了：{:?}",
        actions.borrow()
    );

    cx.update_window(handle, |_, window, cx| {
        window.click("open-settings", cx);
        window.render_frame(cx);
        window.click("settings-nav-general", cx);
        let nav = window.focused(cx);
        window.press("tab", cx);
        assert_ne!(window.focused(cx), nav, "对话框中 Tab 切换焦点");
        window.press("shift-tab", cx);
        assert_eq!(window.focused(cx), nav, "Shift+Tab 切回去");
    })
    .unwrap();
    assert_eq!(*actions.borrow(), ["root::Tab", "root::TabPrev"]);
}

#[gpui_kit::test]
fn closing_a_local_terminal_asks_first(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    start_local_terminal(cx, handle);
    let close_tab = |cx: &mut TestAppContext| {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("terminal", cx);
            window.press("ctrl-shift-w", cx);
        })
        .unwrap();
        cx.run_until_parked();
    };
    // 本地 shell 还在运行：先确认；取消后 Tab 还在
    close_tab(cx);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("close-confirm").is_some(),
            "本地终端关闭前也确认"
        );
        window.click("close-cancel", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("terminal").is_some(), "取消后 Tab 还在");
    })
    .unwrap();
    // 确认后关闭
    close_tab(cx);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("close-confirm", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("terminal").is_none(), "确认后关闭");
    })
    .unwrap();
}

#[gpui_kit::test]
fn duplicate_session_and_reopen_confirmation(cx: &mut TestAppContext) {
    let (handle, workspace) = open_workspace_view(cx);
    start_local_terminal(cx, handle);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("terminal", cx);
        window.press("ctrl-shift-d", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            workspace.read(cx).terminal_count(),
            2,
            "复制会话新开一个 Tab"
        );

        // 会话还在时 Ctrl+Shift+R 先确认
        window.click("terminal", cx);
        window.press("ctrl-shift-r", cx);
        window.render_frame(cx);
        assert!(window.try_find("ok").is_some(), "应弹出确认框");
        window.click("ok", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("ok").is_none(), "确认后关闭");
        assert!(window.try_find("terminal").is_some(), "在原 Tab 重新打开");
        assert_eq!(workspace.read(cx).terminal_count(), 2);
    })
    .unwrap();
}

/// 直接在存储里建一台主机（`parent` 为分组 ID）。
fn add_host(
    cx: &mut gpui_kit::App,
    name: &str,
    parent: Option<String>,
) -> termlet_core::id::HostId {
    AppState::store(cx)
        .create(
            Host {
                name: name.into(),
                address: "127.0.0.1".into(),
                port: 1,
                username: "ops".into(),
                ..Default::default()
            },
            parent,
            Position::Last,
        )
        .unwrap()
}

#[gpui_kit::test]
fn new_host_dialog_saves_and_tree_lists_it(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("new-host", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        // 名称框一开始就有焦点
        window.input("web-01", cx);
        window.click("host-address", cx);
        window.input("10.0.0.5", cx);
        // 没有可沿用的用户名：保存时提示
        window.click("host-save", cx);
        window.render_frame(cx);
        assert!(
            AppState::store(cx).list::<Host>().is_empty(),
            "缺用户名时不保存"
        );
        window.click("host-username", cx);
        window.input("deploy", cx);
        window.click("host-save", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let hosts = AppState::store(cx).list::<Host>();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].doc.name, "web-01");
        assert_eq!(hosts[0].doc.username, "deploy");
        assert!(window.try_find("host-save").is_none(), "保存后关闭对话框");
        assert!(
            window
                .try_find(format!("row-host-{}", hosts[0].id))
                .is_some(),
            "树中列出新主机"
        );
    })
    .unwrap();
}

// 窗口最小时主机对话框也放得下，滚轮滚的是右侧的分页（gpui-kit 0.8 起对话框放不下时滚轮只滚对话框本身，
// 分页里靠下的内容就滚不到了）
#[gpui_kit::test]
fn host_dialog_page_scrolls_with_the_wheel_in_a_small_window(cx: &mut TestAppContext) {
    let (handle, _) = open_workspace_sized(cx, size(px(720.), px(480.)));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("new-host", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let viewport = window.viewport_size();
        let page = window.find("host-page").bounds();
        let name = window.find("host-name").bounds();
        let save = window.find("host-save").bounds();
        assert!(
            page.top() >= px(0.) && save.bottom() <= viewport.height,
            "对话框在窗口内：分页 {page:?}，保存按钮 {save:?}"
        );
        window.scroll(
            "host-page",
            ScrollDelta::Pixels(point(px(0.), px(-200.))),
            cx,
        );
        window.render_frame(cx);
        assert_eq!(window.find("host-page").bounds(), page, "对话框本身不动");
        assert_eq!(window.find("host-save").bounds(), save);
        assert!(
            window.find("host-name").bounds().top() < name.top(),
            "分页的内容往上滚"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn tree_keyboard_drag_and_status(cx: &mut TestAppContext) {
    let (handle, workspace) = open_workspace_view(cx);
    let (group, host_a, host_b) = cx.update(|cx| {
        let group = AppState::store(cx)
            .create(
                Group {
                    name: "生产".into(),
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
        let a = add_host(cx, "alpha", None);
        let b = add_host(cx, "beta", None);
        (group, a, b)
    });
    cx.run_until_parked();
    expand_hosts(cx, handle);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        // 拖进分组（折叠着的分组放进去后展开）
        window.drag_to(
            format!("row-host-{host_a}"),
            format!("row-group-{group}"),
            cx,
        );
    })
    .unwrap();
    cx.run_until_parked();
    cx.update(|cx| {
        let parent = AppState::store(cx).get::<Host>(&host_a).unwrap().parent;
        assert_eq!(
            parent.as_deref(),
            Some(group.as_str()),
            "拖到分组上放进分组"
        );
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        // 单击选中 beta，Enter 打开（连不上也会开一个 Tab）
        window.click(format!("row-host-{host_b}"), cx);
        window.press("enter", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            workspace.read(cx).terminal_count(),
            1,
            "Enter 打开选中的主机"
        );
        assert!(
            window.try_find(format!("status-{host_b}")).is_some(),
            "打开的主机行首有状态圆点"
        );
        // 单击分组折叠：其中的主机不再显示
        window.click(format!("row-group-{group}"), cx);
        window.render_frame(cx);
        assert!(
            window.try_find(format!("row-host-{host_a}")).is_none(),
            "折叠后不显示"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn command_palette_filters_and_runs(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    // 界面语言是进程全局的：下面按中文的操作名搜索
    i18n::set_language(Language::ZhCn);
    cx.update(|cx| {
        add_host(cx, "db-primary", None);
    });
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.press("ctrl-shift-p", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        // 只搜操作：新建主机
        window.input("> 新建主机", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.press("enter", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("host-save").is_some(),
            "执行后打开新建主机对话框"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn split_right_opens_a_pane_beside(cx: &mut TestAppContext) {
    let (handle, workspace) = open_workspace_view(cx);
    start_local_terminal(cx, handle);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("terminal", cx);
        window.press("alt-shift-=", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            workspace.read(cx).terminal_count(),
            2,
            "向右分屏新开一个窗格"
        );
        // 两个窗格都在显示（不是同一组里的两个 Tab）
        for pane in 0..2usize {
            assert!(
                window
                    .within(("resizable-panel", pane))
                    .try_find("terminal")
                    .is_some(),
                "第 {pane} 个窗格里有终端"
            );
        }
    })
    .unwrap();
}

#[gpui_kit::test]
fn workspace_saves_layout_and_reopens_it(cx: &mut TestAppContext) {
    let (handle, workspace) = open_workspace_view(cx);
    i18n::set_language(Language::ZhCn);
    start_local_terminal(cx, handle);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("terminal", cx);
        window.press("alt-shift-=", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        workspace.update(cx, |w, cx| w.save_as_workspace(window, cx));
        window.render_frame(cx);
        window.click("workspace-name", cx);
        window.press("secondary-a", cx);
        window.input("双屏", cx);
        window.press("enter", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("workspace-name").is_none(),
            "保存后关闭对话框"
        );
    })
    .unwrap();
    let saved = cx.update(|cx| AppState::store(cx).list::<termlet_core::model::Workspace>());
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].doc.name, "双屏");
    assert_eq!(saved[0].doc.hosts().len(), 2, "两个窗格都记下");

    // 在侧边栏中双击打开：已有布局保留，工作区的两个窗格加在右侧
    let row = gpui_kit::SharedString::from(format!("row-workspace-{}", saved[0].id));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.double_click(row.clone(), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(workspace.read(cx).terminal_count(), 4);
    })
    .unwrap();
}

#[gpui_kit::test]
fn command_picker_fills_params_and_sends(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    cx.update(|cx| {
        let store = AppState::store(cx);
        for (name, content) in [("问候", "echo {{名字:世界}}\n"), ("磁盘", "df -h\n")] {
            store
                .create(
                    termlet_core::model::Snippet {
                        name: name.into(),
                        content: content.into(),
                        ..Default::default()
                    },
                    None,
                    Position::Last,
                )
                .unwrap();
        }
    });
    start_local_terminal(cx, handle);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("terminal", cx);
        window.press("ctrl-shift-space", cx);
        window.render_frame(cx);
        assert!(
            window.try_find("command-picker").is_some(),
            "快捷键打开选择器"
        );
        window.input("问候", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        // 带参数：先填写参数，预览随之变化
        window.press("enter", cx);
        window.render_frame(cx);
        assert!(window.try_find("picker-preview").is_some(), "进入填写参数");
        window.press("enter", cx);
        window.render_frame(cx);
        assert!(window.try_find("command-picker").is_none(), "发送后关闭");
    })
    .unwrap();
}

#[gpui_kit::test]
fn scheme_card_click_sets_the_slot_on_save(cx: &mut TestAppContext) {
    use gpui_kit::component::ActiveTheme as _;
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    let dark = cx.update(|cx| cx.theme().is_dark());
    let before = cx.update(|cx| AppState::store(cx).settings().terminal);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("open-settings", cx);
        window.render_frame(cx);
        window.click("settings-nav-schemes", cx);
        window.render_frame(cx);
        // 搜索 Dracula：它的卡片排到最前
        window.click("scheme-search", cx);
        window.input("dracula", cx);
        window.render_frame(cx);
        assert_eq!(window.find("scheme-card-dracula").selected(), Some(false));
        window.click("scheme-use-dracula", cx);
        window.render_frame(cx);
        assert_eq!(
            window.find("scheme-card-dracula").selected(),
            Some(true),
            "单击后成为界面当前深浅所用的配色"
        );
    })
    .unwrap();
    assert_eq!(
        cx.update(|cx| AppState::store(cx).settings().terminal),
        before,
        "保存前只改草稿"
    );
    cx.update_window(handle, |_, window, cx| {
        window.click("settings-save", cx);
        window.render_frame(cx);
        assert!(window.try_find("settings-save").is_none(), "保存后关闭");
    })
    .unwrap();
    let after = cx.update(|cx| AppState::store(cx).settings().terminal);
    let (changed, kept, kept_before) = if dark {
        (
            &after.scheme_dark,
            &after.scheme_light,
            &before.scheme_light,
        )
    } else {
        (&after.scheme_light, &after.scheme_dark, &before.scheme_dark)
    };
    assert_eq!(changed, "dracula");
    assert_eq!(kept, kept_before, "另一个槽不变");
}

#[gpui_kit::test]
fn scheme_picker_previews_reverts_and_applies(cx: &mut TestAppContext) {
    use gpui_kit::component::ActiveTheme as _;
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    let dark = cx.update(|cx| cx.theme().is_dark());
    let builtin = termlet_core::schemes::builtin_schemes();
    // 界面当前深浅所用的配色槽
    let slot = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            let terminal = AppState::store(cx).settings().terminal;
            if dark {
                terminal.scheme_dark
            } else {
                terminal.scheme_light
            }
        })
    };
    // 在终端上打开命令面板，执行“切换终端配色…”
    let open_picker = |cx: &mut TestAppContext| {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("terminal", cx);
            window.press("ctrl-shift-p", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.input("> 切换终端配色", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
    };
    // 选择器中选中（正在终端上预览）的配色；选择器关闭时为 None
    let shown = |cx: &mut TestAppContext| {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window
                .try_find("scheme-picker")
                .map(|e| e.value().unwrap_or_default().to_string())
        })
        .unwrap()
    };
    let press = |cx: &mut TestAppContext, key: &str| {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.press(key, cx);
        })
        .unwrap();
        cx.run_until_parked();
    };

    start_local_terminal(cx, handle);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    let current = slot(cx);
    let at = builtin.iter().position(|s| s.id() == current).unwrap();

    open_picker(cx);
    assert_eq!(
        shown(cx).as_deref(),
        Some(builtin[at].name()),
        "打开时选中当前的配色"
    );
    press(cx, "down");
    assert_eq!(
        shown(cx).as_deref(),
        Some(builtin[at + 1].name()),
        "向下移动时预览下一套"
    );
    press(cx, "escape");
    assert_eq!(shown(cx), None, "Esc 关闭选择器");
    assert_eq!(slot(cx), current, "Esc 还原，不改设置");

    // 搜索之后按 Esc 也是直接还原并关闭（不只是清空搜索框）
    open_picker(cx);
    assert_eq!(shown(cx).as_deref(), Some(builtin[at].name()));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.input("nord", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(shown(cx).as_deref(), Some("Nord"), "搜索后预览第一个匹配的");
    press(cx, "escape");
    assert_eq!(shown(cx), None, "Esc 关闭选择器");
    assert_eq!(slot(cx), current);

    // 再打开时仍是原来的配色；向下选择后 Enter 确定，写进设置
    open_picker(cx);
    assert_eq!(shown(cx).as_deref(), Some(builtin[at].name()));
    press(cx, "down");
    press(cx, "enter");
    assert_eq!(shown(cx), None, "Enter 确定后关闭");
    assert_eq!(
        slot(cx),
        builtin[at + 1].id(),
        "确定后改界面当前深浅的配色槽"
    );

    // 只用于这个 Tab：不改设置，再打开时选中的是这个 Tab 用的配色
    open_picker(cx);
    assert_eq!(shown(cx).as_deref(), Some(builtin[at + 1].name()));
    cx.update_window(handle, |_, window, cx| {
        window.click("scheme-picker-tab-only", cx);
    })
    .unwrap();
    press(cx, "down");
    press(cx, "enter");
    assert_eq!(slot(cx), builtin[at + 1].id(), "只用于这个 Tab 时不改设置");
    open_picker(cx);
    assert_eq!(
        shown(cx).as_deref(),
        Some(builtin[at + 2].name()),
        "这个 Tab 用的是刚选的配色"
    );
}

#[gpui_kit::test]
fn custom_scheme_is_saved_with_settings_and_deleted_with_its_references(cx: &mut TestAppContext) {
    use gpui_kit::component::ActiveTheme as _;
    use termlet_core::model::ColorScheme;
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    let dark = cx.update(|cx| cx.theme().is_dark());
    let slot = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            let terminal = AppState::store(cx).settings().terminal;
            if dark {
                terminal.scheme_dark
            } else {
                terminal.scheme_light
            }
        })
    };
    let open_schemes = |cx: &mut TestAppContext, query: &str| {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("open-settings", cx);
            window.render_frame(cx);
            window.click("settings-nav-schemes", cx);
            window.render_frame(cx);
            window.click("scheme-search", cx);
            window.input(query, cx);
            window.render_frame(cx);
        })
        .unwrap();
    };

    // 复制 Dracula 为自定义配色，用于界面当前的深浅，保存设置
    open_schemes(cx, "dracula");
    cx.update_window(handle, |_, window, cx| {
        window.click("scheme-menu-dracula", cx);
        window.render_frame(cx);
        // 菜单：用于浅色、用于深色、分隔线、复制为自定义…
        window.within("popup-menu").click(3usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("scheme-editor-save", cx);
        window.render_frame(cx);
        assert!(
            window.try_find("scheme-editor-save").is_none(),
            "确定后关闭编辑器"
        );
        window.click("scheme-use-new:1", cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert!(
        cx.update(|cx| AppState::store(cx).list::<ColorScheme>())
            .is_empty(),
        "保存设置前不写入存储"
    );
    cx.update_window(handle, |_, window, cx| {
        window.click("settings-save", cx);
        window.render_frame(cx);
    })
    .unwrap();
    let saved = cx.update(|cx| AppState::store(cx).list::<ColorScheme>());
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].doc.name, "Dracula (1)");
    let id = saved[0].id.clone();
    assert_eq!(slot(cx), id.as_str(), "配色槽换成存储分配的 ID");

    // 一台主机用它；删除后保存，主机改回跟随全局，配色槽改回默认
    let host = cx.update(|cx| {
        AppState::store(cx)
            .create(
                Host {
                    name: "web".into(),
                    address: "127.0.0.1".into(),
                    port: 1,
                    username: "ops".into(),
                    terminal: termlet_core::model::TerminalOptions {
                        color_scheme: id.as_str().to_string(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap()
    });
    cx.run_until_parked();
    open_schemes(cx, "(1)");
    cx.update_window(handle, |_, window, cx| {
        window.click(format!("scheme-menu-{id}"), cx);
        window.render_frame(cx);
        // 自定义配色的菜单：用于浅色、用于深色、分隔线、复制、编辑、导出、分隔线、删除
        window.within("popup-menu").click(7usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("ok", cx);
        window.render_frame(cx);
        assert!(
            window.try_find(format!("scheme-card-{id}")).is_none(),
            "确认后从配色库中去掉"
        );
        window.click("settings-save", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.update(|cx| {
        let store = AppState::store(cx);
        assert!(store.list::<ColorScheme>().is_empty());
        assert_eq!(
            store.get::<Host>(&host).unwrap().doc.terminal.color_scheme,
            ""
        );
    });
    let default = if dark {
        termlet_core::model::DEFAULT_SCHEME_DARK
    } else {
        termlet_core::model::DEFAULT_SCHEME_LIGHT
    };
    assert_eq!(slot(cx), default);
}

#[gpui_kit::test]
fn enter_in_settings_saves_and_known_hosts_page_lists_nothing(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("open-settings", cx);
        window.render_frame(cx);
        window.click("settings-nav-known-hosts", cx);
        window.render_frame(cx);
        assert!(
            window.try_find("known-hosts-search").is_some(),
            "主机指纹分页"
        );
        window.click("settings-nav-terminal", cx);
        window.render_frame(cx);
        window.click("settings-scrollback", cx);
        window.press("secondary-a", cx);
        window.input("8000", cx);
        // 在输入框中按 Enter 与点“保存”相同
        window.press("enter", cx);
        window.render_frame(cx);
        assert!(window.try_find("settings-save").is_none(), "保存后关闭");
    })
    .unwrap();
    assert_eq!(
        cx.update(|cx| AppState::store(cx).settings())
            .terminal
            .scrollback,
        8000
    );
}

/// 真实 sshd 的地址（`TERMLET_TEST_SSH=127.0.0.1:2222`，testing/sshd 的测试容器，账号 termlet / termlet-e2e）；
/// 没有设置时跳过连真实服务器的测试。
fn test_server() -> Option<(String, u16)> {
    let value = std::env::var("TERMLET_TEST_SSH").ok()?;
    let (host, port) = value.rsplit_once(':')?;
    Some((host.to_string(), port.parse().ok()?))
}

/// 反复让出时间（网络操作在核心的运行时上进行），直到条件成立；超时则失败。
fn wait_until(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    what: &str,
    mut ready: impl FnMut(&mut gpui_kit::Window, &mut gpui_kit::App) -> bool,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        cx.run_until_parked();
        let done = cx
            .update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                ready(window, cx)
            })
            .unwrap();
        if done {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "等待超时：{what}");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[gpui_kit::test]
fn sftp_opens_from_the_host_menu_and_manages_folders(cx: &mut TestAppContext) {
    use termlet_core::model::SecretField;
    use termlet_core::secret::SecretString;
    use termlet_core::store::Owner;
    let Some((address, port)) = test_server() else {
        return;
    };
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    let id = cx.update(|cx| {
        let store = AppState::store(cx);
        let id = store
            .create(
                Host {
                    name: "sftp".into(),
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
    cx.run_until_parked();
    expand_hosts(cx, handle);
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
    // 内存中的存储没有记下主机指纹：接受它
    wait_until(cx, handle, "主机指纹的询问", |window, _| {
        window.try_find("prompt-ok").is_some()
    });
    cx.update_window(handle, |_, window, cx| window.click("prompt-ok", cx))
        .unwrap();
    wait_until(cx, handle, "列出初始目录", |window, _| {
        window.try_find("remote-list-row-.ssh").is_some()
    });

    // 新建文件夹：输入名称后按 Enter
    let name = format!("ui-test-{}", std::process::id());
    cx.update_window(handle, |_, window, cx| {
        window.click("sftp-new-folder-button", cx);
        window.render_frame(cx);
        window.click("sftp-new-folder", cx);
        window.input(&name, cx);
        window.press("enter", cx);
    })
    .unwrap();
    let row = format!("remote-list-row-{name}");
    wait_until(
        cx,
        handle,
        "新建的文件夹出现在列表中",
        |window, _| {
            window.try_find(row.clone()).is_some() && window.try_find("sftp-new-folder").is_none()
        },
    );

    // 选中后按 Delete，确认后删除
    cx.update_window(handle, |_, window, cx| {
        window.click(row.clone(), cx);
        window.press("delete", cx);
    })
    .unwrap();
    wait_until(cx, handle, "删除前确认", |window, _| {
        window.try_find("ok").is_some()
    });
    cx.update_window(handle, |_, window, cx| window.click("ok", cx))
        .unwrap();
    wait_until(cx, handle, "删除后从列表中去掉", |window, _| {
        window.try_find(row.clone()).is_none()
    });
}

#[gpui_kit::test]
fn key_library_lists_views_renames_and_deletes(cx: &mut TestAppContext) {
    use termlet_core::model::KeyEntry;
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    let id = cx.update(|cx| {
        AppState::store(cx)
            .create(
                KeyEntry {
                    name: "laptop".into(),
                    algorithm: "ssh-ed25519".into(),
                    fingerprint: "SHA256:abc".into(),
                    public_key: "ssh-ed25519 AAAAC3Nza laptop".into(),
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap()
    });
    cx.run_until_parked();
    // 侧边栏第三页“密钥”：单击整行查看公钥
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("sidebar-pages").click(2usize, cx);
        window.render_frame(cx);
        window.click(format!("key-{id}"), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("key-copy-public", cx);
        window.render_frame(cx);
        assert_eq!(
            cx.read_from_clipboard().and_then(|c| c.text()).as_deref(),
            Some("ssh-ed25519 AAAAC3Nza laptop"),
            "复制的是公钥这一行"
        );
        window.click("key-view-close", cx);
    })
    .unwrap();
    cx.run_until_parked();
    // 右键菜单：重命名
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.right_click(format!("key-{id}"), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        // 查看公钥、部署、导出公钥、导出私钥、重命名、分隔线、删除
        window.within("popup-menu").click(4usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.press("secondary-a", cx);
        window.input("工作笔记本", cx);
        window.press("enter", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let name = cx.update(|cx| AppState::store(cx).get::<KeyEntry>(&id).unwrap().doc.name);
    assert_eq!(name, "工作笔记本");
    // 删除
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.right_click(format!("key-{id}"), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("popup-menu").click(6usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("ok", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(cx.update(|cx| AppState::store(cx).list::<KeyEntry>().is_empty()));
}

#[gpui_kit::test]
fn forward_dialog_validates_and_saves_a_rule(cx: &mut TestAppContext) {
    use termlet_core::model::{Forward, ForwardKind};
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    let host = cx.update(|cx| {
        AppState::store(cx)
            .create(
                Host {
                    name: "db".into(),
                    address: "10.0.0.9".into(),
                    username: "ops".into(),
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap()
    });
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("sidebar-pages").click(3usize, cx);
        window.render_frame(cx);
        window.click("forward-new", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        // 监听端口框一开始就有焦点；端口不对时提示且不保存
        window.input("70000", cx);
        window.render_frame(cx);
        window.click("forward-save", cx);
        window.render_frame(cx);
        assert!(window.try_find("forward-save").is_some(), "不合法时不关闭");
        window.click("forward-bind-port", cx);
        window.press("secondary-a", cx);
        window.input("15432", cx);
        window.click("forward-target-port", cx);
        window.input("5432", cx);
        window.click("forward-save", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let rules = cx.update(|cx| AppState::store(cx).list::<Forward>());
    assert_eq!(rules.len(), 1);
    let rule = &rules[0].doc;
    assert_eq!(rule.kind, ForwardKind::Local);
    assert_eq!(rule.host.as_ref(), Some(&host));
    assert_eq!(
        (rule.bind_host.as_str(), rule.bind_port),
        ("127.0.0.1", 15432)
    );
    assert_eq!(
        (rule.target_host.as_str(), rule.target_port),
        ("127.0.0.1", 5432)
    );
    let id = rules[0].id.clone();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find(format!("forward-{id}")).is_some(),
            "列表中出现新规则"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn key_import_from_pasted_text_and_generate(cx: &mut TestAppContext) {
    use termlet_core::model::KeyEntry;
    use termlet_core::sshkey::{self, KeyKind};
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    let pasted = sshkey::generate(KeyKind::Ed25519, "ui@test")
        .unwrap()
        .to_openssh()
        .unwrap();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("sidebar-pages").click(2usize, cx);
        window.render_frame(cx);
        window.click("key-import", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        // 改为粘贴私钥内容
        window.within("key-import-from").click(1usize, cx);
        window.render_frame(cx);
        window.input(pasted.expose(), cx);
        window.render_frame(cx);
        window.click("key-import-submit", cx);
    })
    .unwrap();
    wait_until(cx, handle, "导入后关闭对话框", |window, _| {
        window.try_find("key-import-submit").is_none()
    });
    let keys = cx.update(|cx| AppState::store(cx).list::<KeyEntry>());
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].doc.name, "ui@test", "名称留空时用私钥的注释");

    // 生成：留空名称、注释，生成后打开新密钥的公钥
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("key-generate", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("key-generate-submit", cx);
    })
    .unwrap();
    wait_until(cx, handle, "生成后打开公钥", |window, _| {
        window.try_find("key-copy-public").is_some()
    });
    assert_eq!(
        cx.update(|cx| AppState::store(cx).list::<KeyEntry>().len()),
        2
    );
}

/// `ssh-keygen -s ca -I ui-test -z 7 -n deploy,ops -V always:forever` 签发的证书与对应的公钥（都是公开的内容）。
const CERT_PUBLIC_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICbe1igcT4sGwlBXnjShpoUBNNsmCXkTQ3M0Wigq9xzy ui@test";
const CERTIFICATE: &str = "ssh-ed25519-cert-v01@openssh.com AAAAIHNzaC1lZDI1NTE5LWNlcnQtdjAxQG9wZW5zc2guY29tAAAAIGQpp1S5wvgGEEY14QlbUPW8DW7Ni7RBI6xfoaCXTXkoAAAAICbe1igcT4sGwlBXnjShpoUBNNsmCXkTQ3M0Wigq9xzyAAAAAAAAAAcAAAABAAAAB3VpLXRlc3QAAAARAAAABmRlcGxveQAAAANvcHMAAAAAAAAAAP//////////AAAAAAAAAGUAAAAXcGVybWl0LWFnZW50LWZvcndhcmRpbmcAAAAAAAAAFnBlcm1pdC1wb3J0LWZvcndhcmRpbmcAAAAAAAAACnBlcm1pdC1wdHkAAAAAAAAADnBlcm1pdC11c2VyLXJjAAAAAAAAAAAAAAAzAAAAC3NzaC1lZDI1NTE5AAAAIONm4WSHmInVaSsRZw7Wwa5oEpzqhjBVuvy+flV8sr7oAAAAUwAAAAtzc2gtZWQyNTUxOQAAAEBqvf4vRXCDpYqTSAF+3xUf+FRfVzNkE9Bq87r/dRSCH7NvBXuQwhEm6Iqou0qdbyGkfpQGqyfUfRpEiBJn3J8N ui@test";

// 密钥附带证书：在“查看公钥”中粘贴附带，显示证书的信息，可以去掉；不是这把密钥的证书说明原因
#[gpui_kit::test]
fn key_certificate_attach_view_and_remove(cx: &mut TestAppContext) {
    use termlet_core::model::KeyEntry;
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    let (id, other) = cx.update(|cx| {
        let store = AppState::store(cx);
        let entry = |name: &str, public_key: &str| KeyEntry {
            name: name.into(),
            algorithm: "ssh-ed25519".into(),
            fingerprint: format!("SHA256:{name}"),
            public_key: public_key.into(),
            ..Default::default()
        };
        let id = store
            .create(entry("work", CERT_PUBLIC_KEY), None, Position::Last)
            .unwrap();
        let other = store
            .create(
                entry(
                    "other",
                    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl",
                ),
                None,
                Position::Last,
            )
            .unwrap();
        (id, other)
    });
    cx.run_until_parked();
    let attach = |cx: &mut TestAppContext, key: &termlet_core::id::KeyId| {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click(format!("key-{key}"), cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("key-cert-attach", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.within("key-cert-from").click(1usize, cx);
            window.render_frame(cx);
            window.input(CERTIFICATE, cx);
            window.render_frame(cx);
            window.click("key-cert-submit", cx);
        })
        .unwrap();
    };
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("sidebar-pages").click(2usize, cx);
    })
    .unwrap();
    cx.run_until_parked();

    attach(cx, &id);
    wait_until(
        cx,
        handle,
        "附带后关闭对话框并显示证书",
        |window, _| {
            window.try_find("key-cert-submit").is_none()
                && window.try_find("key-cert-replace").is_some()
        },
    );
    let saved = cx.update(|cx| {
        AppState::store(cx)
            .get::<KeyEntry>(&id)
            .unwrap()
            .doc
            .certificate
    });
    assert_eq!(saved, CERTIFICATE);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("key-cert-remove", cx);
    })
    .unwrap();
    wait_until(
        cx,
        handle,
        "去掉后回到“附带证书”",
        |window, _| window.try_find("key-cert-attach").is_some(),
    );
    assert!(
        cx.update(|cx| AppState::store(cx)
            .get::<KeyEntry>(&id)
            .unwrap()
            .doc
            .certificate)
            .is_empty()
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("key-view-close", cx);
    })
    .unwrap();
    cx.run_until_parked();

    // 另一把密钥：证书对不上，对话框不关闭，说明原因
    attach(cx, &other);
    wait_until(cx, handle, "说明证书对不上", |window, _| {
        window.try_find("key-cert-error").is_some()
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("key-cert-submit").is_some(), "出错时不关闭");
    })
    .unwrap();
    assert!(
        cx.update(|cx| AppState::store(cx)
            .get::<KeyEntry>(&other)
            .unwrap()
            .doc
            .certificate)
            .is_empty()
    );
}

#[gpui_kit::test]
fn telnet_and_serial_hosts_are_saved_from_the_dialog(cx: &mut TestAppContext) {
    use termlet_core::model::{LocalEcho, Parity, Protocol};
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    // Telnet：用户名可以不填，端口留空用 23
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("new-host", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.input("switch-01", cx);
        window.within("host-protocol").click(1usize, cx);
        window.render_frame(cx);
        window.click("host-address", cx);
        window.input("10.0.0.9", cx);
        window.click("host-save", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let hosts = cx.update(|cx| AppState::store(cx).list::<Host>());
    assert_eq!(hosts.len(), 1, "Telnet 主机不要求用户名");
    let telnet = &hosts[0].doc;
    assert_eq!(telnet.protocol, Protocol::Telnet);
    assert_eq!(telnet.effective_port(), 23);
    assert_eq!(telnet.line.enter, None, "没改过的行规程按协议默认");

    // 串口：没填串口时不保存；终端页改本地回显
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("new-host", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.input("console", cx);
        window.within("host-protocol").click(2usize, cx);
        window.render_frame(cx);
        window.click("host-save", cx);
        window.render_frame(cx);
        assert!(window.try_find("host-save").is_some(), "没填串口时不关闭");
        window.click("host-serial-port", cx);
        window.input("COM250", cx);
        window.click("host-serial-baud", cx);
        window.press("secondary-a", cx);
        window.input("9600", cx);
        window.render_frame(cx);
        // 串口的分页：常规、终端、标签与备注
        window.click("host-pages-1", cx);
        window.render_frame(cx);
        window.within("host-local-echo").click(1usize, cx);
        window.render_frame(cx);
        window.click("host-save", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let serial = cx
        .update(|cx| AppState::store(cx).list::<Host>())
        .into_iter()
        .find(|h| h.doc.protocol == Protocol::Serial)
        .expect("保存了串口主机")
        .doc;
    assert_eq!(serial.address, "COM250");
    assert_eq!(serial.name, "console");
    assert_eq!(serial.serial.baud, 9600);
    assert_eq!(serial.serial.parity, Parity::None);
    assert_eq!(serial.line.local_echo, Some(LocalEcho::On));
    assert!(serial.username.is_empty());
}

#[gpui_kit::test]
fn sync_input_tiles_terminals_and_types_into_all(cx: &mut TestAppContext) {
    let (handle, workspace) = open_workspace_view(cx);
    i18n::set_language(Language::ZhCn);
    start_local_terminal(cx, handle);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("terminal", cx);
        window.press("ctrl-shift-d", cx);
    })
    .unwrap();
    wait_until(cx, handle, "两个终端都启动", |_, cx| {
        workspace.read(cx).terminal_count() == 2
    });
    // 两个终端都连上后才能开启（不足两个时只提示）
    wait_until(cx, handle, "开启同步输入", |window, cx| {
        if workspace.read(cx).synced_terminals().is_none() {
            window.click("terminal", cx);
            window.press("ctrl-shift-i", cx);
        }
        workspace.read(cx).synced_terminals().is_some()
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(workspace.read(cx).synced_terminals(), Some(2));
        assert!(window.try_find("multi-exec-grid").is_some(), "平铺成网格");
        assert!(window.try_find("multi-exec-bar").is_some(), "显示横幅");
        // 焦点仍在开启前的终端上
        window.input("echo synced", cx);
    })
    .unwrap();
    wait_until(cx, handle, "两个终端都收到输入", |_, cx| {
        workspace
            .read(cx)
            .terminal_screens(cx)
            .iter()
            .all(|screen| screen.contains("echo synced"))
    });
    // 退出后照原样显示
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("multi-exec-exit", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(workspace.read(cx).synced_terminals(), None);
        assert!(window.try_find("multi-exec-grid").is_none());
        assert_eq!(workspace.read(cx).terminal_count(), 2);
    })
    .unwrap();
}

#[gpui_kit::test]
fn snippet_bar_sends_to_the_terminal_and_filters_by_group(cx: &mut TestAppContext) {
    let (handle, workspace) = open_workspace_view(cx);
    i18n::set_language(Language::ZhCn);
    start_local_terminal(cx, handle);
    let (plain, grouped) = cx.update(|cx| {
        let store = AppState::store(cx);
        let create = |name: &str, content: &str, group: &str| {
            store
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
                .unwrap()
        };
        (
            create("say", "echo snippet-bar-ok", ""),
            create("deploy", "echo grouped", "ops"),
        )
    });
    cx.run_until_parked();
    // 命令面板打开快捷命令栏（默认不显示）
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("snippet-bar").is_none(), "默认不显示");
        window.press("ctrl-shift-p", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.input("> 显示快捷命令栏", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.press("enter", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let item = |id: &termlet_core::id::SnippetId| SharedString::from(format!("snippet-bar-{id}"));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("snippet-bar").is_some(), "显示快捷命令栏");
        assert!(window.try_find(item(&grouped)).is_some(), "全部分组都列出");
        // 单击发到当前终端（没有末尾换行：只写入，不执行）
        window.click(item(&plain), cx);
    })
    .unwrap();
    wait_until(cx, handle, "终端收到快捷命令", |_, cx| {
        workspace
            .read(cx)
            .terminal_screens(cx)
            .iter()
            .any(|screen| screen.contains("echo snippet-bar-ok"))
    });
    // 只显示 ops 分组
    cx.update(|cx| {
        AppState::ui(cx).update(cx, |ui, cx| {
            ui.update(|s| s.snippet_bar_group = Some("ops".into()), cx)
        })
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find(item(&grouped)).is_some());
        assert!(window.try_find(item(&plain)).is_none(), "其他分组的不显示");
        window.click("snippet-bar-close", cx);
        window.render_frame(cx);
        assert!(window.try_find("snippet-bar").is_none(), "× 隐藏");
    })
    .unwrap();
}

#[gpui_kit::test]
fn compose_bar_sends_to_all_terminals(cx: &mut TestAppContext) {
    let (handle, workspace) = open_workspace_view(cx);
    i18n::set_language(Language::ZhCn);
    start_local_terminal(cx, handle);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("terminal", cx);
        window.press("ctrl-shift-d", cx);
    })
    .unwrap();
    wait_until(cx, handle, "两个终端都启动", |_, cx| {
        workspace.read(cx).terminal_count() == 2
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.press("ctrl-shift-m", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("compose-send").is_some(), "打开撰写栏");
    })
    .unwrap();
    // 发送到全部终端（默认的“当前 Tab”只有显示着的那一个）
    cx.update_window(handle, |_, window, cx| {
        window.click("compose-targets", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("popup-menu").click(2usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    // 终端都连上后再发送
    wait_until(cx, handle, "发送到两个终端", |window, cx| {
        let screens = workspace.read(cx).terminal_screens(cx);
        if screens.iter().all(|s| s.contains("composed-ok")) {
            return true;
        }
        // 菜单关闭后焦点回到输入框
        if screens.iter().all(|s| !s.contains("echo composed-ok")) {
            window.input("echo composed-ok", cx);
            window.press("enter", cx);
        }
        false
    });
}

#[gpui_kit::test]
fn compose_bar_runs_and_compares_on_ssh_terminals(cx: &mut TestAppContext) {
    let Some((address, port)) = test_server() else {
        eprintln!("没有设置 TERMLET_TEST_SSH，跳过");
        return;
    };
    let (handle, workspace) = open_workspace_view(cx);
    i18n::set_language(Language::ZhCn);
    let host = cx.update(|cx| {
        let store = AppState::store(cx);
        let id = store
            .create(
                Host {
                    name: "e2e".into(),
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
                &termlet_core::store::Owner::host(&id),
                termlet_core::model::SecretField::Password,
                Some(&termlet_core::secret::SecretString::new("termlet-e2e")),
            )
            .unwrap();
        id
    });
    // 测试容器的指纹：第一次连接时确认
    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |w, cx| {
            w.open_host(&host, window, cx);
            w.open_host(&host, window, cx);
        });
    })
    .unwrap();
    wait_until(cx, handle, "两个终端都连上", |window, cx| {
        if window.try_find("prompt-ok").is_some() {
            window.click("prompt-ok", cx);
        }
        workspace
            .read(cx)
            .terminal_screens(cx)
            .iter()
            .filter(|s| s.contains('$'))
            .count()
            == 2
    });
    cx.update_window(handle, |_, window, cx| {
        window.press("ctrl-shift-m", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("compose-targets", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("popup-menu").click(2usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.input("echo pid-$$", cx);
        window.render_frame(cx);
        window.click("compose-compare", cx);
    })
    .unwrap();
    wait_until(cx, handle, "两个终端都执行完", |window, cx| {
        window.try_find("batch-results").is_some()
            && workspace
                .read(cx)
                .terminal_screens(cx)
                .iter()
                .all(|s| s.lines().any(|l| l.trim_start().starts_with("pid-")))
    });
}

// 真实的服务器：终端底部的状态条显示资源；点状态条在主机工具中打开概览页（CPU、内存、网络、磁盘、进程），
// 右键状态条切到精简后网速收起
#[gpui_kit::test]
fn status_bar_opens_the_overview_on_a_real_host(cx: &mut TestAppContext) {
    let Some((address, port)) = test_server() else {
        eprintln!("没有设置 TERMLET_TEST_SSH，跳过");
        return;
    };
    let (handle, workspace) = open_workspace_sized(cx, size(px(1400.), px(1000.)));
    i18n::set_language(Language::ZhCn);
    let host = cx.update(|cx| {
        let store = AppState::store(cx);
        let id = store
            .create(
                Host {
                    name: "monitor".into(),
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
                &termlet_core::store::Owner::host(&id),
                termlet_core::model::SecretField::Password,
                Some(&termlet_core::secret::SecretString::new("termlet-e2e")),
            )
            .unwrap();
        id
    });
    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |w, cx| w.open_host(&host, window, cx));
    })
    .unwrap();
    wait_until(cx, handle, "终端连上", |window, cx| {
        if window.try_find("prompt-ok").is_some() {
            window.click("prompt-ok", cx);
        }
        workspace
            .read(cx)
            .terminal_screens(cx)
            .iter()
            .any(|s| s.contains('$'))
    });
    let id = cx.update(|cx| workspace.read(cx).terminal_ids()[0].as_u64() as usize);
    wait_until(cx, handle, "状态条上的资源", |window, _| {
        [
            "monitor-latency",
            "monitor-cpu",
            "monitor-memory",
            "monitor-disk",
            "monitor-net",
        ]
        .iter()
        .all(|item| window.try_find((*item, id)).is_some())
    });

    cx.update_window(handle, |_, window, cx| {
        window.click(("monitor-bar", id), cx)
    })
    .unwrap();
    wait_until(cx, handle, "主机工具的概览页", |window, _| {
        [
            "tools-overview-system",
            "tools-overview-cpu",
            "tools-overview-memory",
            "tools-overview-net",
            "tools-overview-disk-/",
            "tools-overview-iface-eth0",
            "tools-overview-top",
        ]
        .iter()
        .all(|element| window.try_find(*element).is_some())
    });
    assert!(cx.update(|cx| AppState::ui(cx).read(cx).state().host_tools_open));

    // 右键状态条：切到精简（第二项），网速收起
    cx.update_window(handle, |_, window, cx| {
        window.right_click(("monitor-bar", id), cx)
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("popup-menu").click(1usize, cx);
    })
    .unwrap();
    wait_until(cx, handle, "精简的状态条", |window, _| {
        window.try_find(("monitor-net", id)).is_none()
            && window.try_find(("monitor-cpu", id)).is_some()
    });
    assert_eq!(
        cx.update(|cx| AppState::store(cx).settings().terminal.monitor_bar),
        termlet_core::model::MonitorBar::Compact
    );
}

#[gpui_kit::test]
fn editing_a_forward_keeps_its_unavailable_host(cx: &mut TestAppContext) {
    use termlet_core::model::{Forward, Protocol};
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    let (db, id) = cx.update(|cx| {
        let store = AppState::store(cx);
        let host = |name: &str| Host {
            name: name.into(),
            address: format!("{name}.example"),
            username: "ops".into(),
            ..Default::default()
        };
        let db = store.create(host("db"), None, Position::Last).unwrap();
        store.create(host("web"), None, Position::Last).unwrap();
        let id = store
            .create(
                Forward {
                    host: Some(db.clone()),
                    bind_port: 15433,
                    target_host: "127.0.0.1".into(),
                    target_port: 5432,
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
        // 经由的主机改成了 Telnet：不再出现在可选的 SSH 主机中
        let mut doc = store.get::<Host>(&db).unwrap().doc;
        doc.protocol = Protocol::Telnet;
        store.update(&db, doc).unwrap();
        (db, id)
    });
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("sidebar-pages").click(3usize, cx);
        window.render_frame(cx);
        window.hover(format!("forward-{id}"), cx);
        window.render_frame(cx);
        window.click(format!("forward-edit-{id}"), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        // 没有换主机就保存：不能保存，也不会被悄悄换成列表中的另一台主机
        window.click("forward-save", cx);
        window.render_frame(cx);
        assert!(
            window.try_find("forward-save").is_some(),
            "原主机不可用时不能保存"
        );
    })
    .unwrap();
    let rule = cx.update(|cx| AppState::store(cx).get::<Forward>(&id).unwrap().doc);
    assert_eq!(rule.host.as_ref(), Some(&db));
}

// 监听所有网卡的规则在侧边栏带警示标记（本地转发是本机的网卡、远程转发是服务器的网卡）；只监听本机的没有
#[gpui_kit::test]
fn forwards_listening_on_all_interfaces_are_marked(cx: &mut TestAppContext) {
    use termlet_core::model::{Forward, ForwardKind};
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    let [local_any, local_any6, remote_any, local_only] = cx.update(|cx| {
        let store = AppState::store(cx);
        let host = store
            .create(
                Host {
                    name: "db".into(),
                    address: "db.example".into(),
                    username: "ops".into(),
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
        let add = |kind, bind_host: &str, bind_port| {
            store
                .create(
                    Forward {
                        host: Some(host.clone()),
                        kind,
                        bind_host: bind_host.into(),
                        bind_port,
                        target_host: "127.0.0.1".into(),
                        target_port: 5432,
                        ..Default::default()
                    },
                    None,
                    Position::Last,
                )
                .unwrap()
        };
        [
            add(ForwardKind::Local, "0.0.0.0", 15433),
            add(ForwardKind::Local, "::", 15436),
            add(ForwardKind::Remote, "0.0.0.0", 15434),
            add(ForwardKind::Local, "127.0.0.1", 15435),
        ]
    });
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("sidebar-pages").click(3usize, cx);
        window.render_frame(cx);
        for id in [&local_any, &local_any6, &remote_any] {
            assert!(
                window.try_find(format!("forward-exposed-{id}")).is_some(),
                "监听所有网卡的 {id} 应带警示标记"
            );
        }
        assert!(window.try_find(format!("forward-{local_only}")).is_some());
        assert!(
            window
                .try_find(format!("forward-exposed-{local_only}"))
                .is_none(),
            "只监听本机的不带标记"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn deleting_a_settings_row_keeps_focus_in_the_dialog(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("open-settings", cx);
        window.render_frame(cx);
        window.click("settings-nav-highlight", cx);
        window.render_frame(cx);
        // 在一条规则的输入框里输入后删掉这一行：获得焦点的输入框随这一行消失
        window.click(("highlight-pattern", 0usize), cx);
        window.input("x", cx);
        window.click(("highlight-delete", 0usize), cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        // 焦点没有被收到对话框下面的工作区：Esc 仍然关闭设置
        window.press("escape", cx);
        window.render_frame(cx);
        assert!(
            window.try_find("settings-save").is_none(),
            "删除一行后 Esc 仍能关闭设置"
        );
    })
    .unwrap();
}

/// 打开设置的“关键词高亮”页。
fn open_highlight_settings(cx: &mut TestAppContext, handle: AnyWindowHandle) {
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("open-settings", cx);
        window.render_frame(cx);
        window.click("settings-nav-highlight", cx);
        window.render_frame(cx);
    })
    .unwrap();
}

/// 设置中高亮规则的正则原文，按顺序。
fn highlight_patterns(cx: &mut TestAppContext) -> Vec<String> {
    cx.update(|cx| AppState::store(cx).settings().highlight.rules)
        .into_iter()
        .map(|rule| rule.pattern)
        .collect()
}

fn preset_pattern(id: &str) -> String {
    use termlet_core::model::HIGHLIGHT_PRESETS;
    HIGHLIGHT_PRESETS
        .iter()
        .find(|p| p.id == id)
        .unwrap()
        .pattern
        .to_string()
}

// “添加预设规则”：预设行在关键词前标出名称；菜单中已在列表里的打勾、不能再加；加入的预设按预设的先后插入
// （MAC 在 IPv4 之后，网址在最前），保存后记下默认规则的版本
#[gpui_kit::test]
fn highlight_presets_are_inserted_in_order_and_marked(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    open_highlight_settings(cx, handle);
    cx.update_window(handle, |_, window, cx| {
        // 默认 5 条：错误、警告、成功、IPv6、IPv4（行号 0–4）
        assert_eq!(
            window.find(("highlight-preset-name", 3usize)).label(),
            Some("IPv6 地址")
        );
        assert_eq!(
            window.find(("highlight-preset-name", 0usize)).label(),
            Some("错误关键词")
        );
        window.click("highlight-add-preset", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        // 菜单按预设的先后：网址、错误、警告、成功、IPv6、IPv4、MAC、UUID、日期与时间、数字。
        // 错误关键词已在列表中（打勾、禁用），点了不加
        window.within("popup-menu").click(1usize, cx);
        window.render_frame(cx);
        assert!(window.try_find(("highlight-pattern", 5usize)).is_none());
        window.within("popup-menu").click(6usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find(("highlight-preset-name", 5usize)).label(),
            Some("MAC 地址")
        );
        window.click("highlight-add-preset", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("popup-menu").click(0usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let y = |window: &gpui_kit::Window, key: usize| {
            window.find(("highlight-pattern", key)).bounds().origin.y
        };
        assert!(y(window, 6) < y(window, 0), "网址插在最前");
        assert!(y(window, 5) > y(window, 4), "MAC 插在 IPv4 之后");
        window.click("settings-save", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    let ids = [
        "url", "errors", "warnings", "success", "ipv6", "ipv4", "mac",
    ];
    assert_eq!(
        highlight_patterns(cx),
        ids.map(preset_pattern).to_vec(),
        "保存后的顺序与预设的先后相同"
    );
    assert_eq!(
        cx.update(|cx| AppState::store(cx).settings().highlight.rules_version),
        Some(2)
    );
    // 再打开（行号从 0 重新编）：MAC 已加过，点了不加；数字还没加，加在最后
    open_highlight_settings(cx, handle);
    cx.update_window(handle, |_, window, cx| {
        window.click("highlight-add-preset", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("popup-menu").click(6usize, cx);
        window.render_frame(cx);
        assert!(window.try_find(("highlight-pattern", 7usize)).is_none());
        window.within("popup-menu").click(9usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find(("highlight-preset-name", 7usize)).label(),
            Some("数字")
        );
    })
    .unwrap();
}

// 规则满 50 条时“添加规则”与“添加预设规则”都禁用，点了也不出菜单
#[gpui_kit::test]
fn highlight_presets_are_disabled_when_the_rules_are_full(cx: &mut TestAppContext) {
    use termlet_core::model::HighlightRule;
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    cx.update(|cx| {
        AppState::store(cx)
            .update_settings(|s| {
                s.highlight.rules = (0..50)
                    .map(|i| HighlightRule {
                        pattern: format!("keyword{i}"),
                        color: "blue".into(),
                        enabled: true,
                        ..Default::default()
                    })
                    .collect();
            })
            .unwrap();
    });
    open_highlight_settings(cx, handle);
    cx.update_window(handle, |_, window, cx| {
        // 按钮在 50 行规则下面：先滚到底
        window.scroll(
            ("highlight-pattern", 0usize),
            ScrollDelta::Pixels(point(px(0.), px(-5000.))),
            cx,
        );
        window.render_frame(cx);
        assert_eq!(window.find("highlight-add").disabled(), Some(true));
        assert_eq!(window.find("highlight-add-preset").disabled(), Some(true));
        window.click("highlight-add-preset", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("popup-menu").is_none(), "满了不出菜单");
    })
    .unwrap();
}

// 迁移：0.3.3 之前保存的、从没改过的 4 条默认规则读出来是新的默认（设置页中有 IPv6 一行，菜单中打勾）；
// 在设置页删掉 IPv6 并保存后，保存的内容带版本标记，再读出来不会把 IPv6 加回来
#[gpui_kit::test]
fn old_default_highlight_rules_are_upgraded_and_stay_edited(cx: &mut TestAppContext) {
    use termlet_core::model::HighlightSettings;
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    // 0.3.2 写进数据库的原样（没有 rulesVersion）
    let old = r#"{"enabled":true,"rules":[
        {"pattern":"\\b(error|errors|fail|failed|failure|fatal|panic|denied|refused|exception|critical)\\b","regex":true,"caseSensitive":false,"color":"red","enabled":true},
        {"pattern":"\\b(warn|warning|warnings|deprecated)\\b","regex":true,"caseSensitive":false,"color":"yellow","enabled":true},
        {"pattern":"\\b(ok|success|successful|succeeded|passed)\\b","regex":true,"caseSensitive":false,"color":"green","enabled":true},
        {"pattern":"\\b(?:\\d{1,3}\\.){3}\\d{1,3}(?:/\\d{1,2})?\\b","regex":true,"caseSensitive":false,"color":"cyan","enabled":true}]}"#;
    let upgraded: HighlightSettings = serde_json::from_str(old).unwrap();
    assert_eq!(upgraded, HighlightSettings::default());
    cx.update(|cx| {
        AppState::store(cx)
            .update_settings(|s| s.highlight = upgraded)
            .unwrap();
    });
    open_highlight_settings(cx, handle);
    cx.update_window(handle, |_, window, cx| {
        assert_eq!(
            window.find(("highlight-preset-name", 3usize)).label(),
            Some("IPv6 地址")
        );
        window.click(("highlight-delete", 3usize), cx);
        window.render_frame(cx);
        window.click("settings-save", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    let saved = cx.update(|cx| AppState::store(cx).settings().highlight);
    assert_eq!(saved.rules.len(), 4);
    assert_eq!(saved.rules_version, Some(2));
    let json = serde_json::to_string(&saved).unwrap();
    let reread: HighlightSettings = serde_json::from_str(&json).unwrap();
    assert_eq!(
        reread.rules, saved.rules,
        "带了版本标记，不再把 IPv6 加回来"
    );
}

// 配色库搜索不到时给出导入的两条路；“读取本机的配色”打开导入对话框并停在“本机已装的客户端”页签
#[gpui_kit::test]
fn scheme_library_offers_import_when_nothing_matches(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("open-settings", cx);
        window.render_frame(cx);
        window.click("settings-nav-schemes", cx);
        window.render_frame(cx);
        assert!(window.try_find("scheme-no-match-installed").is_none());
        window.click("scheme-search", cx);
        window.input("没有这套配色", cx);
        window.render_frame(cx);
        assert!(window.try_find("scheme-no-match-import").is_some());
        window.click("scheme-no-match-installed", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("scheme-import-installed").is_some(),
            "停在本机已装的客户端"
        );
        assert!(window.try_find("scheme-import-drop").is_none());
    })
    .unwrap();
}

// 同步输入时粘贴要确认的内容，在确认框里点“取消”：之后的键入照常同步到各个终端
#[gpui_kit::test]
fn sync_input_continues_after_cancelling_a_paste(cx: &mut TestAppContext) {
    let (handle, workspace) = open_workspace_view(cx);
    i18n::set_language(Language::ZhCn);
    start_local_terminal(cx, handle);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("terminal", cx);
        window.press("ctrl-shift-d", cx);
    })
    .unwrap();
    wait_until(cx, handle, "两个终端都启动", |_, cx| {
        workspace.read(cx).terminal_count() == 2
    });
    wait_until(cx, handle, "开启同步输入", |window, cx| {
        if workspace.read(cx).synced_terminals().is_none() {
            window.click("terminal", cx);
            window.press("ctrl-shift-i", cx);
        }
        workspace.read(cx).synced_terminals().is_some()
    });
    // 带一个零宽空格：不论终端开没开括号粘贴（Linux 上的 bash 默认开启，多行内容不会立即执行、不用确认）都要确认
    cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
        "echo one\u{200b}\necho two\n".into(),
    ));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.press("ctrl-shift-v", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("paste-cancel", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("paste-cancel").is_none(), "确认框已关闭");
        window.input("echo after-cancel", cx);
    })
    .unwrap();
    wait_until(
        cx,
        handle,
        "两个终端都收到之后的输入",
        |_, cx| {
            workspace
                .read(cx)
                .terminal_screens(cx)
                .iter()
                .all(|screen| screen.contains("echo after-cancel"))
        },
    );
}

#[gpui_kit::test]
fn command_picker_ignores_keys_outside_it(cx: &mut TestAppContext) {
    use gpui_kit::component::WindowExt as _;
    let (handle, workspace) = open_workspace_view(cx);
    i18n::set_language(Language::ZhCn);
    cx.update(|cx| {
        AppState::store(cx)
            .create(
                termlet_core::model::Snippet {
                    name: "标记".into(),
                    content: "echo picker-marker\n".into(),
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
    });
    start_local_terminal(cx, handle);
    // 等 shell 开始读输入
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("terminal", cx);
        window.input("echo shell-ready", cx);
    })
    .unwrap();
    wait_until(cx, handle, "shell 回显输入", |_, cx| {
        workspace
            .read(cx)
            .terminal_screens(cx)
            .iter()
            .any(|screen| screen.contains("echo shell-ready"))
    });
    cx.update_window(handle, |_, window, cx| {
        window.press("enter", cx);
        window.press("ctrl-shift-space", cx);
        window.render_frame(cx);
        assert!(window.try_find("command-picker").is_some());
        // 选择器上面又弹出一个对话框（命令行工具的批准框等）：焦点在它上面（宽度与这里无关，直接用 gpui-kit 的）
        #[allow(clippy::disallowed_methods)]
        window.open_alert_dialog(
            cx,
            |alert: gpui_kit::component::dialog::AlertDialog, _, _| alert.title("上面的对话框"),
        );
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.press("enter", cx);
    })
    .unwrap();
    // Enter 归上面的对话框：选择器没有把快捷命令发到终端
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
    while std::time::Instant::now() < deadline {
        cx.run_until_parked();
        let sent = cx.update(|cx| {
            workspace
                .read(cx)
                .terminal_screens(cx)
                .iter()
                .any(|screen| screen.contains("echo picker-marker"))
        });
        assert!(!sent, "选择器拦下了别的对话框的 Enter");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("command-picker").is_some(), "选择器还开着");
    })
    .unwrap();
}

#[gpui_kit::test]
fn host_tree_keys_act_on_the_right_clicked_row(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    let (a, b) = cx.update(|cx| (add_host(cx, "alpha", None), add_host(cx, "beta", None)));
    cx.run_until_parked();
    expand_hosts(cx, handle);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click(format!("row-host-{a}"), cx);
        window.right_click(format!("row-host-{b}"), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("popup-menu").is_some(), "弹出右键菜单");
        // 照菜单上的提示按 Delete：删除的是右键的那一行
        window.press("delete", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("ok", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update(|cx| {
        let store = AppState::store(cx);
        assert!(store.get::<Host>(&b).is_none(), "删除右键的 beta");
        assert!(store.get::<Host>(&a).is_some(), "左键选中的 alpha 还在");
    });
}

#[gpui_kit::test]
fn host_tree_search_results_cannot_be_dragged(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    let (a, b) = cx.update(|cx| {
        let group = AppState::store(cx)
            .create(
                Group {
                    name: "生产".into(),
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
        let a = add_host(cx, "web-a", None);
        let b = add_host(cx, "web-b", Some(group.as_str().to_string()));
        (a, b)
    });
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("host-search", cx);
        window.input("web", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find(format!("row-host-{b}")).is_some());
        // 平铺的结果里看不出分组：拖到 web-b 上不会把 web-a 移进它的分组
        window.drag_to(format!("row-host-{a}"), format!("row-host-{b}"), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update(|cx| {
        let parent = AppState::store(cx).get::<Host>(&a).unwrap().parent;
        assert_eq!(parent, None, "搜索结果不能拖动");
    });
}

#[gpui_kit::test]
fn enter_in_sync_and_security_pages_keeps_settings_open(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("open-settings", cx);
        window.render_frame(cx);
        // 同步：表单没填完时 Enter 什么也不做，不保存、不关闭设置（填好的表单不丢）
        window.click("settings-nav-sync", cx);
        window.render_frame(cx);
        window.click("sync-path", cx);
        window.input("D:\\Sync", cx);
        window.press("enter", cx);
        window.render_frame(cx);
        assert!(
            window.try_find("settings-save").is_some(),
            "同步页中的 Enter 不关闭设置"
        );
        // 安全：Enter 提交锁定密码（两次输入不一致时提示），不关闭设置
        window.click("settings-nav-security", cx);
        window.render_frame(cx);
        window.click("security-enable", cx);
        window.render_frame(cx);
        window.click("security-new", cx);
        window.input("secret-1", cx);
        window.click("security-confirm", cx);
        window.input("secret-2", cx);
        window.press("enter", cx);
        window.render_frame(cx);
        assert!(
            window.try_find("security-message").is_some(),
            "Enter 提交了密码并显示结果"
        );
        assert!(
            window.try_find("settings-save").is_some(),
            "安全页中的 Enter 不关闭设置"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn host_dialog_keeps_spaces_around_the_password(cx: &mut TestAppContext) {
    use termlet_core::model::SecretField;
    use termlet_core::store::Owner;
    let handle = open_workspace(cx);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("new-host", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.input("web-01", cx);
        window.click("host-address", cx);
        window.input("10.0.0.5", cx);
        window.click("host-username", cx);
        window.input("deploy", cx);
        window.click("host-password", cx);
        window.input(" p w ", cx);
        window.click("host-save", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let password = cx.update(|cx| {
        let store = AppState::store(cx);
        let hosts = store.list::<Host>();
        assert_eq!(hosts.len(), 1);
        store
            .secret(&Owner::host(&hosts[0].id), SecretField::Password)
            .unwrap()
            .map(|s| s.expose().to_string())
    });
    assert_eq!(
        password.as_deref(),
        Some(" p w "),
        "密码原样保存，不去掉首尾的空格"
    );
}

#[gpui_kit::test]
fn shortcut_recording_rejects_fixed_keys_and_stops_on_leaving_the_page(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("open-settings", cx);
        window.render_frame(cx);
        window.click("settings-nav-shortcuts", cx);
        window.render_frame(cx);
        for _ in 0..40 {
            if window
                .try_find("key-add-commandPalette")
                .is_some_and(|row| row.visible())
            {
                break;
            }
            window.scroll(
                "settings-page",
                ScrollDelta::Pixels(point(px(0.), px(-100.))),
                cx,
            );
            window.render_frame(cx);
        }
        // Alt+←（切换分屏）是固定的快捷键：不录入
        window.click("key-add-commandPalette", cx);
        window.render_frame(cx);
        window.press("alt-left", cx);
        window.render_frame(cx);
        assert!(
            window
                .try_find("key-remove-commandPalette-alt-left")
                .is_none()
        );
        // 没有占用的组合照常录入
        window.click("key-add-commandPalette", cx);
        window.render_frame(cx);
        window.press("ctrl-alt-j", cx);
        window.render_frame(cx);
        assert!(
            window
                .try_find("key-remove-commandPalette-ctrl-alt-j")
                .is_some(),
            "录入了新的组合"
        );
        // 录入中切到别的页：结束录入，在那里按的键不会被录成快捷键
        window.click("key-add-commandPalette", cx);
        window.render_frame(cx);
        assert!(window.try_find("key-recording-commandPalette").is_some());
        window.click("settings-nav-general", cx);
        window.render_frame(cx);
        window.click("settings-nav-shortcuts", cx);
        window.render_frame(cx);
        assert!(
            window.try_find("key-recording-commandPalette").is_none(),
            "离开快捷键页后不再录入"
        );
        window.click("settings-save", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    let saved = cx.update(|cx| AppState::store(cx).settings().keybindings.bindings);
    assert_eq!(
        saved.get("commandPalette").map(Vec::as_slice),
        Some(&["ctrl-shift-p".to_string(), "ctrl-alt-j".to_string()][..])
    );
}

#[gpui_kit::test]
fn unavailable_default_terminal_is_kept_on_save(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    cx.update(|cx| {
        AppState::store(cx)
            .update_local_settings(|s| s.shells.default = "wsl:NoSuchDistro".into())
            .unwrap()
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("open-settings", cx);
        window.render_frame(cx);
        window.click("settings-nav-local", cx);
        window.render_frame(cx);
    })
    .unwrap();
    // 等检测完（检测在后台线程上）
    wait_until(cx, handle, "检测本地终端", |window, _| {
        window.try_find("settings-default-shell").is_some()
            || window.try_find("settings-save").is_none()
    });
    cx.update_window(handle, |_, window, cx| {
        window.click("settings-save", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        cx.update(|cx| AppState::store(cx).local_settings().shells.default),
        "wsl:NoSuchDistro",
        "没检测到的默认终端不被换成列表中的第一项"
    );
}

#[gpui_kit::test]
fn ctrl_shift_n_opens_a_new_window(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.press("ctrl-shift-n", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| cx.windows().len()), 2, "新窗口已打开");
}

#[gpui_kit::test]
fn title_bar_toggles_theme_and_sidebar_takes_arrow_keys(cx: &mut TestAppContext) {
    use gpui_kit::component::ActiveTheme as _;
    let handle = open_workspace(cx);
    let dark = cx.update(|cx| cx.theme().is_dark());
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("title-toggle-theme", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let theme = cx.update(|cx| AppState::store(cx).settings().appearance.theme);
    assert_eq!(
        theme,
        if dark {
            termlet_core::model::ThemeSetting::Light
        } else {
            termlet_core::model::ThemeSetting::Dark
        },
        "标题栏按钮切换浅色 / 深色并保存"
    );
    // 点一下侧边栏的拖动条后，方向键微调宽度（每次 16；最小宽度随页签的文案放宽，所以不从固定的宽度算起）
    let width =
        |cx: &mut TestAppContext| cx.update(|cx| AppState::ui(cx).read(cx).state().sidebar_width);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("sidebar-resize", cx);
        window.press("right", cx);
    })
    .unwrap();
    let first = width(cx).expect("记下了宽度");
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.press("right", cx);
        window.render_frame(cx);
        window.press("left", cx);
        window.press("left", cx);
    })
    .unwrap();
    assert_eq!(width(cx), Some(first - 16.));
}

#[gpui_kit::test]
fn about_page_shows_the_copyright(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("open-settings", cx);
        window.render_frame(cx);
        window.click("settings-nav-about", cx);
        window.render_frame(cx);
        assert!(
            window.try_find("about-copyright").is_some(),
            "“关于”中有版权行（署名只在这一处）"
        );
    })
    .unwrap();
}

// “关于”的“隐私”一组：发送匿名使用统计默认勾选，取消后保存才写进本机设置；能打开查看发送的内容
#[gpui_kit::test]
fn about_page_turns_usage_statistics_off_on_save(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("open-settings", cx);
        window.render_frame(cx);
        window.click("settings-nav-about", cx);
        window.render_frame(cx);
        assert_eq!(window.find("about-usage").checked(), Some(true));
        assert!(window.try_find("about-privacy-policy").is_some());
        window.click("about-usage", cx);
        window.render_frame(cx);
        assert_eq!(window.find("about-usage").checked(), Some(false));
    })
    .unwrap();
    assert!(
        cx.update(|cx| AppState::store(cx).local_settings().usage.enabled),
        "保存之前不变"
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("settings-save", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(
        !cx.update(|cx| AppState::store(cx).local_settings().usage.enabled),
        "保存后关闭"
    );

    // 再打开设置，“查看发送的内容…”打开对话框
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("open-settings", cx);
        window.render_frame(cx);
        window.click("settings-nav-about", cx);
        window.render_frame(cx);
        assert_eq!(window.find("about-usage").checked(), Some(false));
        // 按钮在下面：往下滚到整个露出来为止（点的是按钮的中央，只露出一半时会点到页面外面）
        for _ in 0..40 {
            let page = window.find("settings-page").bounds();
            if window
                .try_find("about-usage-view")
                .is_some_and(|button| button.visible() && button.bounds().bottom() <= page.bottom())
            {
                break;
            }
            window.scroll(
                "settings-page",
                ScrollDelta::Pixels(point(px(0.), px(-100.))),
                cx,
            );
            window.render_frame(cx);
        }
        window.click("about-usage-view", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("usage-view-json").is_some());
        assert!(window.try_find("usage-view-state").is_some());
    })
    .unwrap();
}

// 设置“连接”页的“连接后识别远程系统”：默认开，可以关掉
#[gpui_kit::test]
fn detect_os_setting_can_be_turned_off(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    i18n::set_language(Language::ZhCn);
    assert!(
        cx.update(|cx| AppState::store(cx).settings())
            .connection
            .detect_os
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("open-settings", cx);
        window.render_frame(cx);
        window.click("settings-nav-connection", cx);
        window.render_frame(cx);
        window.click("settings-detect-os", cx);
        window.click("settings-save", cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert!(
        !cx.update(|cx| AppState::store(cx).settings())
            .connection
            .detect_os
    );
}

/// 打开连测试容器的 SSH 主机，等终端出现提示符（第一次连接时确认指纹）。
fn connect_e2e_host(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    name: &str,
    (address, port): (String, u16),
) -> termlet_core::id::HostId {
    let host = cx.update(|cx| {
        let store = AppState::store(cx);
        let id = store
            .create(
                Host {
                    name: name.into(),
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
                &termlet_core::store::Owner::host(&id),
                termlet_core::model::SecretField::Password,
                Some(&termlet_core::secret::SecretString::new("termlet-e2e")),
            )
            .unwrap();
        id
    });
    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |w, cx| w.open_host(&host, window, cx));
    })
    .unwrap();
    wait_until(cx, handle, "终端连上", |window, cx| {
        if window.try_find("prompt-ok").is_some() {
            window.click("prompt-ok", cx);
        }
        workspace
            .read(cx)
            .terminal_screens(cx)
            .iter()
            .any(|s| s.contains('$'))
    });
    host
}

/// SSH 终端连上后识别出远程系统（测试容器是 Alpine）：记进主机，不改主机的修改时间，侧边栏与 Tab 照常显示；
/// 设置中关掉后新的连接不再识别。
#[gpui_kit::test]
fn ssh_terminal_detects_the_remote_system(cx: &mut TestAppContext) {
    use termlet_core::model::RemoteOs;
    let Some(server) = test_server() else {
        eprintln!("没有设置 TERMLET_TEST_SSH，跳过");
        return;
    };
    let (handle, workspace) = open_workspace_view(cx);
    i18n::set_language(Language::ZhCn);
    let host = connect_e2e_host(cx, handle, &workspace, "e2e", server.clone());
    let created = cx.update(|cx| AppState::store(cx).get::<Host>(&host).unwrap().updated_at);
    wait_until(cx, handle, "识别出 Alpine", |_, cx| {
        AppState::store(cx)
            .get::<Host>(&host)
            .is_some_and(|h| h.doc.remote_os() == Some(RemoteOs::Alpine))
    });
    let record = cx.update(|cx| AppState::store(cx).get::<Host>(&host).unwrap());
    assert_eq!(record.updated_at, created, "识别结果不算修改主机");
    // 带着系统标志的侧边栏行与 Tab 照常画出
    expand_hosts(cx, handle);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let row = SharedString::from(format!("row-host-{host}"));
        assert!(window.try_find(row).is_some());
    })
    .unwrap();

    cx.update(|cx| AppState::store(cx).update_settings(|s| s.connection.detect_os = false))
        .unwrap();
    let off = connect_e2e_host(cx, handle, &workspace, "e2e-off", server);
    // 识别在连上后另开通道执行（最多几秒）：等一会儿仍然没有
    for _ in 0..20 {
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(
        cx.update(|cx| AppState::store(cx).get::<Host>(&off).unwrap().doc.os),
        None,
        "关掉后不识别"
    );
}
