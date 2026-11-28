//! 设置“本地终端”中检测到的 shell：编辑后改动传到启动的进程、隐藏与取消隐藏、恢复默认，以及这次没检测到的改动
//! 照样保存。用这台电脑上真实检测到的 shell：Windows 上总有命令提示符，其他系统上取 sh 一类的。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, SharedString, TestAppContext, Window, px, size};
use termlet_core::localpty;
use termlet_core::localshell::Profile;
use termlet_core::model::{LocalShell, ShellOverride};
use termlet_terminal::{GridSize, IoEvent, TerminalIo};

use crate::app_state::AppState;
use crate::i18n::{Language, set_language};
use crate::local_shell::{self, shell_key};
use crate::settings::Page;
use crate::testkit::{open_workspace_sized, wait_until};

/// 格式化出来的元素 ID。
fn id(name: String) -> SharedString {
    name.into()
}

/// 用完删掉的临时目录。
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "termlet-local-shell-{}",
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

/// 打开主窗口（1280 × 800，编辑对话框整个放得下），等本地 shell 检测完。
fn open(cx: &mut TestAppContext) -> (AnyWindowHandle, Vec<Profile>) {
    let handle = open_workspace_sized(cx, size(px(1280.), px(800.)));
    set_language(Language::ZhCn);
    let detector = cx.update(|cx| local_shell::detector(cx));
    let detected = detector.list(false);
    (handle, detected)
}

/// 测试用的检测到的 shell 与让它执行一条命令的启动参数：输出 `edited-<TERMLET_EDIT 的值>` 与当前目录后结束。
fn runnable_shell(detected: &[Profile]) -> Option<(Profile, &'static str)> {
    if cfg!(windows) {
        let cmd = detected.iter().find(|p| p.id == "cmd")?;
        return Some((cmd.clone(), r#"/d /c "echo edited-%TERMLET_EDIT%& cd""#));
    }
    let sh = detected.iter().find(|p| {
        let name = p.path.file_name().unwrap_or_default().to_string_lossy();
        ["sh", "bash", "zsh", "ksh"].contains(&name.as_ref())
    })?;
    Some((sh.clone(), "-c 'echo edited-$TERMLET_EDIT; pwd'"))
}

/// 打开设置的“本地终端”页，等列表出来。
fn open_local_page(cx: &mut TestAppContext, handle: AnyWindowHandle, first_row: &str) {
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("open-settings", cx);
        window.render_frame(cx);
        window.click(Page::Local.nav_id(), cx);
    })
    .unwrap();
    let row = id(format!("settings-shell-show-{first_row}"));
    wait_until(cx, handle, "本地终端的列表", |window, _| {
        window.try_find(row.clone()).is_some()
    });
}

/// 点编辑对话框中的输入框，全选后输入（`text` 为空时清空）。
fn replace_text(window: &mut Window, id: &'static str, text: &str, cx: &mut gpui_kit::App) {
    window.click(id, cx);
    window.press("secondary-a", cx);
    if text.is_empty() {
        window.press("backspace", cx);
    } else {
        window.input(text, cx);
    }
}

/// 读伪终端的输出，直到其中有 `needles` 中的全部或进程结束（最多 20 秒）。
fn read_until(io: &TerminalIo, needles: &[&str]) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut text = String::new();
    while Instant::now() < deadline && !needles.iter().all(|n| text.contains(n)) {
        match io.output.try_recv() {
            Ok(IoEvent::Output(bytes)) => {
                let chunk = String::from_utf8_lossy(&bytes);
                // ConPTY 启动时查询光标位置，收到回答才继续（终端引擎会自动回答，这里模拟）
                if chunk.contains("\x1b[6n") {
                    io.input.write(b"\x1b[1;1R");
                }
                text.push_str(&chunk);
            }
            Ok(IoEvent::Closed(_)) => break,
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    text
}

// 编辑检测到的 shell：名称、启动参数、起始目录、环境变量保存进本机设置，界面上用改过的名称，打开时按改过的参数
// 启动，环境变量与起始目录传到启动的进程
#[gpui_kit::test]
fn edited_detected_shell_reaches_the_started_process(cx: &mut TestAppContext) {
    let (handle, detected) = open(cx);
    let Some((shell, args)) = runnable_shell(&detected) else {
        println!("跳过：没有检测到可以执行命令的 shell");
        return;
    };
    let key = shell_key(&shell.id);
    let dir = TempDir::new();
    let start = dir.0.join("termlet-edited-dir");
    std::fs::create_dir(&start).unwrap();
    open_local_page(cx, handle, &key);

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click(id(format!("settings-shell-edit-{key}")), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("shell-args").value(),
            Some(shell.args_text().as_str()),
            "启动参数填着检测到的"
        );
        assert!(
            window.try_find("shell-command").is_none(),
            "检测到的不能换程序"
        );
        replace_text(window, "shell-name", "我的 shell", cx);
        replace_text(window, "shell-args", args, cx);
        replace_text(window, "shell-dir", &start.to_string_lossy(), cx);
        window.click("shell-env", cx);
        window.input("TERMLET_EDIT=ok-7", cx);
        window.click("shell-save", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("shell-save").is_none(), "保存后关闭对话框");
        assert!(
            window
                .try_find(id(format!("settings-shell-status-{key}")))
                .is_some(),
            "列表中标着已修改"
        );
        window.click("settings-save", cx);
    })
    .unwrap();
    cx.run_until_parked();

    let saved = cx.update(|cx| AppState::store(cx).local_settings());
    let changes = &saved.shells.overrides[&shell.id];
    assert_eq!(changes.name, "我的 shell");
    assert_eq!(changes.args.as_deref(), Some(args));
    assert_eq!(changes.dir, start.to_string_lossy());
    assert_eq!(changes.env["TERMLET_EDIT"], "ok-7");

    let profile = cx
        .update(|cx| local_shell::find_profile(&shell.id, &saved, cx))
        .expect("按 ID 找得到");
    assert_eq!(local_shell::display_name(&profile), "我的 shell");
    let launch = local_shell::launch(&profile).expect("启动参数");
    assert_eq!(launch.cwd.as_deref(), Some(start.as_path()));
    let io = localpty::spawn(launch, GridSize::new(200, 24)).expect("启动 shell");
    let out = read_until(&io, &["edited-ok-7", "termlet-edited-dir"]);
    assert!(
        out.contains("edited-ok-7"),
        "应带着改过的参数与环境变量启动：{out:?}"
    );
    assert!(
        out.contains("termlet-edited-dir"),
        "应从改过的起始目录开始：{out:?}"
    );
    io.input.close();
}

/// 标题栏“+”旁的菜单中各项的名称。
fn local_menu(cx: &mut TestAppContext, handle: AnyWindowHandle) -> Vec<String> {
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("local-terminal-menu", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let menu = window.within("popup-menu");
        let labels = (0..100usize)
            .map_while(|ix| menu.try_find(ix))
            .filter_map(|item| item.label().map(str::to_string))
            .collect();
        window.press("escape", cx);
        labels
    })
    .unwrap()
}

// 隐藏检测到的 shell：不出现在“+”旁的菜单、侧边栏“本机”与默认终端的选项中，按 ID 仍能打开；默认打开的一项不能隐藏；
// 取消隐藏后回来
#[gpui_kit::test]
fn hidden_shell_leaves_the_menus_until_shown_again(cx: &mut TestAppContext) {
    let (handle, detected) = open(cx);
    let Some(first) = detected.first().cloned() else {
        println!("跳过：没有检测到 shell");
        return;
    };
    let key = shell_key(&first.id);
    let name = local_shell::display_name(&first).to_string();
    // 侧边栏“本机”展开，这一项勾选了显示在侧边栏
    cx.update(|cx| {
        AppState::store(cx)
            .update_local_settings(|s| s.shells.sidebar.push(key.clone()))
            .unwrap();
        AppState::ui(cx).update(cx, |ui, cx| {
            ui.update(
                |state| {
                    state
                        .expanded_groups
                        .insert(crate::sidebar::rows::LOCAL_SECTION.to_string());
                },
                cx,
            )
        });
    });
    open_local_page(cx, handle, &key);
    let hide = id(format!("settings-shell-hide-{key}"));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find(hide.clone()).disabled(),
            Some(true),
            "没有设默认的时候第一项是默认打开的，不能隐藏"
        );
    })
    .unwrap();

    // 另设一个自定义的为默认（设置中没有未保存的修改，跟着重新载入），第一项就能隐藏了
    cx.update(|cx| {
        AppState::store(cx)
            .update_local_settings(|s| {
                s.shells.custom.push(LocalShell {
                    id: "custom-default".into(),
                    name: "默认的".into(),
                    command: "default-shell".into(),
                    ..Default::default()
                });
                s.shells.default = shell_key("custom-default");
            })
            .unwrap();
    });
    open_local_page(cx, handle, &key);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(!window.find(hide.clone()).disabled().unwrap_or(false));
        window.click(hide.clone(), cx);
        window.render_frame(cx);
        assert_eq!(
            window
                .find(id(format!("settings-shell-show-{key}")))
                .checked(),
            Some(false),
            "隐藏的不显示在侧边栏中"
        );
        window.click("settings-save", cx);
    })
    .unwrap();
    cx.run_until_parked();

    let saved = cx.update(|cx| AppState::store(cx).local_settings());
    assert!(saved.shells.hidden.contains(&key));
    assert!(saved.shells.sidebar.contains(&key), "侧边栏的勾选留着");
    let all = cx.update(|cx| local_shell::all_targets(&saved, cx));
    assert!(all.iter().all(|t| t.key() != key), "隐藏的不在列表中");
    assert!(
        cx.update(|cx| local_shell::find_profile(&first.id, &saved, cx))
            .is_some(),
        "按 ID（工作区等）照样能打开"
    );
    assert!(
        !local_menu(cx, handle).contains(&name),
        "“+”旁的菜单中没有隐藏的"
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find(id(format!("row-{key}"))).is_none(),
            "侧边栏“本机”中没有隐藏的"
        );
        assert!(
            window
                .try_find(id(format!("settings-shell-status-{key}")))
                .is_some(),
            "设置的列表中照样列出，标着已隐藏"
        );
        // 取消隐藏
        window.click(hide.clone(), cx);
        window.click("settings-save", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let saved = cx.update(|cx| AppState::store(cx).local_settings());
    assert!(!saved.shells.hidden.contains(&key));
    assert!(
        local_menu(cx, handle).contains(&name),
        "取消隐藏后回到菜单中"
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find(id(format!("row-{key}"))).is_some(),
            "取消隐藏后回到侧边栏"
        );
    })
    .unwrap();
}

// 恢复默认：编辑对话框中各项换回检测到的，保存后不再记着改动
#[gpui_kit::test]
fn restore_defaults_drops_the_changes(cx: &mut TestAppContext) {
    let (handle, detected) = open(cx);
    let Some(first) = detected.first().cloned() else {
        println!("跳过：没有检测到 shell");
        return;
    };
    let key = shell_key(&first.id);
    cx.update(|cx| {
        AppState::store(cx)
            .update_local_settings(|s| {
                s.shells.overrides.insert(
                    first.id.clone(),
                    ShellOverride {
                        name: "改过的".into(),
                        args: Some("--changed".into()),
                        dir: "~".into(),
                        env: [("A".to_string(), "1".to_string())].into(),
                    },
                );
            })
            .unwrap();
    });
    open_local_page(cx, handle, &key);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click(id(format!("settings-shell-edit-{key}")), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("shell-name").value(), Some("改过的"));
        assert_eq!(window.find("shell-args").value(), Some("--changed"));
        window.click("shell-reset", cx);
        window.render_frame(cx);
        assert_eq!(
            window.find("shell-name").value(),
            Some(local_shell::detected_name(&first).as_ref())
        );
        assert_eq!(
            window.find("shell-args").value(),
            Some(first.args_text().as_str())
        );
        assert_eq!(window.find("shell-dir").value(), Some(""));
        window.click("shell-save", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window
                .try_find(id(format!("settings-shell-status-{key}")))
                .is_none(),
            "恢复默认后不再标着已修改"
        );
        window.click("settings-save", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let saved = cx.update(|cx| AppState::store(cx).local_settings());
    assert!(
        saved.shells.overrides.is_empty(),
        "{:?}",
        saved.shells.overrides
    );
    let profile = cx
        .update(|cx| local_shell::find_profile(&first.id, &saved, cx))
        .unwrap();
    assert_eq!(profile, first, "打开的是检测到的原样");
}

// 这次没检测到的 shell（卸载了）：改动与隐藏照样保存、不列出；检测到的照常
#[gpui_kit::test]
fn changes_for_missing_shells_are_kept(cx: &mut TestAppContext) {
    let (handle, detected) = open(cx);
    let Some(first) = detected.first().cloned() else {
        println!("跳过：没有检测到 shell");
        return;
    };
    let missing = "uninstalled-shell";
    let changes = ShellOverride {
        dir: "~/work".into(),
        ..Default::default()
    };
    cx.update(|cx| {
        AppState::store(cx)
            .update_local_settings(|s| {
                s.shells.overrides.insert(missing.into(), changes.clone());
                s.shells.hidden.push(shell_key(missing));
            })
            .unwrap();
    });
    let key = shell_key(&first.id);
    open_local_page(cx, handle, &key);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window
                .try_find(id(format!("settings-shell-show-{}", shell_key(missing))))
                .is_none(),
            "没检测到的不列出"
        );
        // 改别的设置后保存
        window.click(id(format!("settings-shell-show-{key}")), cx);
        window.click("settings-save", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let saved = cx.update(|cx| AppState::store(cx).local_settings());
    assert_eq!(saved.shells.overrides.get(missing), Some(&changes));
    assert!(saved.shells.hidden.contains(&shell_key(missing)));
}
