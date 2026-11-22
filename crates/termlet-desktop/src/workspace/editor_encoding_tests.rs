//! 在线编辑 UTF-16 文件（连真实 sshd，`TERMLET_TEST_SSH`）：带 BOM 的 UTF-16LE 自动识别，状态栏写明编码与 BOM，
//! BOM 不进编辑区（Ctrl+Home 后输入的文字在 BOM 之后），保存后仍是带 BOM 的 UTF-16LE、换行仍是 CRLF；自动识别
//! 不了的（没有 BOM、以汉字为主）在提示中选 UTF-16LE 打开。

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Entity, Focusable as _, TestAppContext,
    WindowBounds, WindowOptions, point, px, size,
};
use termlet_core::sftp::{TextEncoding, WriteTextRequest};

use super::Workspace;
use crate::app_state::AppState;
use crate::i18n::{self, Language};
use crate::testkit::{test_server, wait_until};

fn setup(cx: &mut TestAppContext) {
    i18n::set_language(Language::ZhCn);
    cx.executor().allow_parking();
    cx.update(gpui_kit::init);
    cx.update(crate::init);
    cx.update(AppState::install_in_memory);
    cx.update(|cx| cx.set_reduce_motion(true));
}

fn open_workspace(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Workspace>) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
            point(px(0.), px(0.)),
            size(px(1024.), px(640.)),
        ))),
        ..Default::default()
    };
    let opened = cx
        .update(|cx| {
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| Workspace::new(window, cx))
            })
        })
        .unwrap();
    cx.run_until_parked();
    opened
}

/// 测试容器上的主机（保存了密码）。
fn add_test_host(
    cx: &mut TestAppContext,
    (address, port): (String, u16),
) -> termlet_core::id::HostId {
    use termlet_core::model::{Host, SecretField};
    use termlet_core::secret::SecretString;
    use termlet_core::store::{Owner, Position};
    cx.update(|cx| {
        let store = AppState::store(cx);
        let id = store
            .create(
                Host {
                    name: "utf16".into(),
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
    })
}

/// 状态栏上编码按钮的文字。
fn encoding_label(cx: &mut TestAppContext, handle: AnyWindowHandle) -> Option<String> {
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.find("editor-encoding").label().map(str::to_string)
    })
    .unwrap()
}

#[gpui_kit::test]
fn editor_opens_and_saves_utf16_files(cx: &mut TestAppContext) {
    let Some(server) = test_server() else {
        eprintln!("没有设置 TERMLET_TEST_SSH，跳过");
        return;
    };
    setup(cx);
    let host = add_test_host(cx, server);
    let (handle, ws) = open_workspace(cx);
    cx.update_window(handle, |_, window, cx| {
        ws.update(cx, |w, cx| w.open_host_sftp(&host, window, cx))
    })
    .unwrap();
    let sftp = cx.update(|cx| ws.read(cx).live_sftps().pop().unwrap());
    wait_until(cx, handle, "文件管理连上", |window, cx| {
        if window.try_find("prompt-ok").is_some() {
            window.click("prompt-ok", cx);
        }
        sftp.read(cx).current_lease().is_some()
    });
    let session = cx.update(|cx| sftp.read(cx).current_lease().unwrap().session().clone());
    let dir = format!("/tmp/termlet-utf16-{}", termlet_core::id::random_token(8));
    let ps = format!("{dir}/ps.txt");
    {
        let (session, dir, ps) = (session.clone(), dir.clone(), ps.clone());
        termlet_core::runtime::runtime()
            .block_on(async move {
                session.mkdir(&dir).await?;
                // Windows PowerShell 5.1 重定向生成的样子：带 BOM 的 UTF-16LE、CRLF
                session
                    .write_text(&WriteTextRequest {
                        path: ps,
                        content: "\u{feff}Name  Id\r\npwsh 1234\r\n".into(),
                        encoding: Some(TextEncoding::Utf16Le),
                        force: true,
                        ..Default::default()
                    })
                    .await?;
                // 没有 BOM、以汉字为主：自动识别不了
                session
                    .write_text(&WriteTextRequest {
                        path: format!("{dir}/cn.txt"),
                        content: "全部是汉字\r\n".into(),
                        encoding: Some(TextEncoding::Utf16Le),
                        force: true,
                        ..Default::default()
                    })
                    .await
            })
            .expect("准备测试文件");
    }
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("sftp-path", cx);
        window.press("ctrl-a", cx);
        window.input(&dir, cx);
        window.press("enter", cx);
    })
    .unwrap();
    wait_until(cx, handle, "列出测试目录", |window, _| {
        window.try_find("remote-list-row-ps.txt").is_some()
    });

    // 带 BOM 的：双击直接打开
    cx.update_window(handle, |_, window, cx| {
        window.double_click("remote-list-row-ps.txt", cx)
    })
    .unwrap();
    wait_until(cx, handle, "编辑器打开 ps.txt", |_, cx| {
        !ws.read(cx).live_editors().is_empty()
    });
    let editor = cx.update(|cx| ws.read(cx).live_editors().pop().unwrap());
    assert_eq!(encoding_label(cx, handle).as_deref(), Some("UTF-16LE BOM"));
    assert_eq!(
        cx.update(|cx| editor.read(cx).text(cx)),
        "Name  Id\npwsh 1234\n",
        "BOM 不进编辑区，CRLF 按 LF 编辑"
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        editor.read(cx).focus_handle(cx).focus(window, cx);
        window.press("ctrl-home", cx);
        window.input("# ", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        cx.update(|cx| editor.read(cx).text(cx)),
        "# Name  Id\npwsh 1234\n"
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("editor-save", cx);
    })
    .unwrap();
    wait_until(cx, handle, "保存完", |window, cx: &mut App| {
        assert!(
            window.try_find("editor-overwrite").is_none(),
            "不应提示冲突"
        );
        !editor.read(cx).is_dirty(cx)
    });
    let saved = {
        let (session, ps) = (session.clone(), ps.clone());
        termlet_core::runtime::runtime()
            .block_on(async move { session.read_text(&ps, None).await })
            .unwrap()
    };
    assert_eq!(saved.encoding, TextEncoding::Utf16Le);
    assert_eq!(
        saved.content, "\u{feff}# Name  Id\r\npwsh 1234\r\n",
        "仍是带 BOM 的 UTF-16LE，输入的文字在 BOM 之后，换行仍是 CRLF"
    );

    // 没有 BOM、以汉字为主的：提示中选 UTF-16LE 打开（GBK、Big5、UTF-16LE、UTF-16BE）。先切回文件管理的 Tab
    let sftp_tab: gpui_kit::ElementId =
        ("sftp-tab-title", sftp.entity_id().as_u64() as usize).into();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click(sftp_tab, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.double_click("remote-list-row-cn.txt", cx)
    })
    .unwrap();
    wait_until(cx, handle, "问要不要选编码打开", |window, _| {
        window.try_find("sftp-open-encoding").is_some()
    });
    cx.update_window(handle, |_, window, cx| {
        window.click("sftp-open-encoding", cx)
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.within("popup-menu").click(2usize, cx);
    })
    .unwrap();
    wait_until(cx, handle, "编辑器打开 cn.txt", |_, cx| {
        ws.read(cx).live_editors().len() == 2
    });
    let chinese = cx.update(|cx| {
        ws.read(cx)
            .live_editors()
            .into_iter()
            .find(|e| e.entity_id() != editor.entity_id())
            .unwrap()
    });
    assert_eq!(cx.update(|cx| chinese.read(cx).text(cx)), "全部是汉字\n");
    assert_eq!(encoding_label(cx, handle).as_deref(), Some("UTF-16LE"));

    let _ = termlet_core::runtime::runtime().block_on(async move { session.remove(&dir).await });
}
