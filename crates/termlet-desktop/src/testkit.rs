//! 单元测试共用的辅助：打开主窗口、展开主机树、轮询等待后台的操作、真实 sshd 的测试容器、检查导航的分页名放不放得下。
//!
//! 集成测试（`tests/ui.rs`）用不了只在 `cfg(test)` 中编译的库代码，那里另有自己的一份 `wait_until`、`test_server`。

use std::time::{Duration, Instant};

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, SharedString, TestAppContext, Window,
    WindowBounds, WindowOptions, px, rems, size,
};

use crate::app_state::AppState;

/// 打开主窗口（1024 × 640，内存中的数据；关掉动画，免得点击时元素还在移动）。
pub(crate) fn open_workspace(cx: &mut TestAppContext) -> AnyWindowHandle {
    open_workspace_with(cx, AppState::install_in_memory)
}

/// 像便携版那样打开主窗口：数据目录标为便携版，保险库还没有选保管方式（界面上是保险库关卡）。
pub(crate) fn open_portable_workspace(cx: &mut TestAppContext) -> AnyWindowHandle {
    open_workspace_with(cx, AppState::install_in_memory_portable)
}

fn open_workspace_with(cx: &mut TestAppContext, install: fn(&mut App)) -> AnyWindowHandle {
    open_workspace_sized_with(cx, install, size(px(1024.), px(640.)))
}

/// 按指定的大小打开主窗口（内存中的数据）。
pub(crate) fn open_workspace_sized(
    cx: &mut TestAppContext,
    window_size: gpui_kit::Size<gpui_kit::Pixels>,
) -> AnyWindowHandle {
    open_workspace_sized_with(cx, AppState::install_in_memory, window_size)
}

fn open_workspace_sized_with(
    cx: &mut TestAppContext,
    install: fn(&mut App),
    window_size: gpui_kit::Size<gpui_kit::Pixels>,
) -> AnyWindowHandle {
    cx.executor().allow_parking();
    cx.update(gpui_kit::init);
    cx.update(crate::init);
    cx.update(install);
    cx.update(|cx| cx.set_reduce_motion(true));
    let (handle, _) = cx
        .update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        Default::default(),
                        window_size,
                    ))),
                    ..Default::default()
                },
                cx,
                |window, cx| cx.new(|cx| crate::Workspace::new(window, cx)),
            )
        })
        .expect("打开测试窗口");
    cx.run_until_parked();
    handle
}

/// 主机树的分组与节默认全部折叠：展开“主机”一节与现有的全部分组（测试要点到其中的主机时用）。
pub(crate) fn expand_host_tree(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let groups: Vec<String> = AppState::store(cx)
            .list::<termlet_core::model::Group>()
            .into_iter()
            .map(|g| g.id.as_str().to_string())
            .collect();
        AppState::ui(cx).update(cx, |ui, cx| {
            ui.update(
                |state| {
                    state
                        .expanded_groups
                        .insert(crate::sidebar::rows::HOSTS_SECTION.to_string());
                    state.expanded_groups.extend(groups);
                },
                cx,
            )
        });
    });
    cx.run_until_parked();
}

/// 对话框左侧导航（设置、主机对话框）的每一项都放得下分页名：按这个窗口的文字系统量分页名（`items` 为导航项的
/// 元素 ID 与分页名），加上侧边菜单项（gpui-kit 的 `SidebarMenuItem`）左右的内边距（各 0.5rem）、图标（与文字
/// 同大，0.875rem）与间距（0.5rem）。
pub(crate) fn assert_nav_labels_fit(
    window: &Window,
    items: impl IntoIterator<Item = (SharedString, SharedString)>,
    what: &str,
) {
    let rem = window.rem_size();
    let font_size = rems(0.875).to_pixels(rem);
    let chrome = rems(2.375).to_pixels(rem);
    for (id, label) in items {
        let width = window.find(id).bounds().size.width;
        let run = window.text_style().to_run(label.len());
        let text = window
            .text_system()
            .shape_line(label.clone(), font_size, &[run], None)
            .width;
        assert!(
            text + chrome <= width + px(0.5),
            "{what}：“{label}”要 {:?}，导航项只有 {width:?}",
            text + chrome
        );
    }
}

/// 轮询到 `ready` 为真：核心运行时上的操作（SFTP、同步、读写文件……）`run_until_parked` 不等它，
/// 反复让出时间；超过 60 秒失败。
pub(crate) fn poll_until(
    cx: &mut TestAppContext,
    what: &str,
    mut ready: impl FnMut(&mut TestAppContext) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        cx.run_until_parked();
        if ready(cx) {
            return;
        }
        assert!(Instant::now() < deadline, "等待超时：{what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 同 [`poll_until`]，每次先渲染窗口再检查（界面上的元素渲染后才更新）。
pub(crate) fn wait_until(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    what: &str,
    mut ready: impl FnMut(&mut Window, &mut App) -> bool,
) {
    poll_until(cx, what, |cx| {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            ready(window, cx)
        })
        .unwrap()
    });
}

/// 测试容器的 sshd（`TERMLET_TEST_SSH=127.0.0.1:2222`，账号 termlet / termlet-e2e）；没有设置时为 `None`，测试跳过。
pub(crate) fn test_server() -> Option<(String, u16)> {
    let value = std::env::var("TERMLET_TEST_SSH").ok()?;
    let (host, port) = value.rsplit_once(':')?;
    Some((host.to_string(), port.parse().ok()?))
}
