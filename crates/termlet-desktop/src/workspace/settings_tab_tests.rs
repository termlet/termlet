//! 设置 Tab（[`super::settings_tabs`]）的测试：一个窗口只开一个、各入口切到它；有未保存的修改时 Tab 名前有“●”，关闭前
//! 问保存、放弃还是取消，关窗口时并进汇总的确认；别处改了设置时没有修改就跟着重新载入，有修改时保存按字段合并。

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, ElementId, Entity, TestAppContext};
use termlet_core::model::ThemeSetting;

use super::Workspace;
use crate::app_state::AppState;
use crate::i18n::{Language, set_language};
use crate::settings::Page;
use crate::settings::panel::SettingsPanel;
use crate::t;
use crate::testkit::open_workspace;

/// 打开主窗口，点标题栏的齿轮打开设置。
fn open_settings(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Workspace>) {
    let handle = open_workspace(cx);
    set_language(Language::ZhCn);
    let workspace = cx
        .update_window(handle, |_, window, cx| {
            Workspace::in_window(window, cx).unwrap()
        })
        .unwrap();
    click_gear(cx, handle);
    (handle, workspace)
}

fn click_gear(cx: &mut TestAppContext, handle: AnyWindowHandle) {
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("open-settings", cx);
        window.render_frame(cx);
    })
    .unwrap();
}

fn panel(cx: &mut TestAppContext, workspace: &Entity<Workspace>) -> Option<Entity<SettingsPanel>> {
    cx.update(|cx| workspace.read(cx).live_settings())
}

fn close_button(panel: &Entity<SettingsPanel>) -> ElementId {
    ("settings-tab-close", panel.entity_id().as_u64() as usize).into()
}

fn click(cx: &mut TestAppContext, handle: AnyWindowHandle, id: impl Into<ElementId>) {
    let id = id.into();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click(id, cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
}

// 设置是停靠区里的一个 Tab；从别处再打开（“关于”、菜单的“设置…”）切到同一个 Tab 并换到那一页，不开第二个
#[gpui_kit::test]
fn settings_open_once_per_window(cx: &mut TestAppContext) {
    let (handle, workspace) = open_settings(cx);
    let first = panel(cx, &workspace).expect("设置 Tab");
    cx.update_window(handle, |_, window, cx| {
        assert!(
            window.try_find("settings-show-host-address").is_some(),
            "通用页"
        );
        crate::settings::open(Page::About, window, cx);
        window.render_frame(cx);
        assert!(
            window.try_find("about-copy-version").is_some(),
            "换到“关于”页"
        );
        // 菜单的“设置…”、⌘,（动作在这次更新之后分发）
        window.dispatch_action(Box::new(super::OpenSettings), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("settings-show-host-address").is_some(),
            "回到通用页"
        );
    })
    .unwrap();
    assert_eq!(
        panel(cx, &workspace).unwrap().entity_id(),
        first.entity_id()
    );
    assert_eq!(cx.update(|cx| workspace.read(cx).tab_count(cx)), 1);
}

// 有未保存的修改：Tab 名前有“●”；关闭时问，取消照旧开着，放弃修改后关闭、什么也不写，保存后关闭；没有修改时直接关
#[gpui_kit::test]
fn unsaved_changes_mark_the_tab_and_ask_before_closing(cx: &mut TestAppContext) {
    let (handle, workspace) = open_settings(cx);
    let before = cx.update(|cx| AppState::store(cx).settings());
    let settings = panel(cx, &workspace).unwrap();
    assert_eq!(
        cx.update(|cx| settings.read(cx).label(cx)),
        t!("settings.title")
    );
    click(cx, handle, "settings-show-host-address");
    assert_eq!(
        cx.update(|cx| settings.read(cx).label(cx)).to_string(),
        format!("● {}", t!("settings.title"))
    );

    click(cx, handle, close_button(&settings));
    click(cx, handle, "settings-close-cancel");
    assert!(panel(cx, &workspace).is_some(), "取消后照旧开着");

    click(cx, handle, close_button(&settings));
    click(cx, handle, "settings-close-discard");
    assert!(panel(cx, &workspace).is_none(), "放弃修改后关闭");
    assert_eq!(cx.update(|cx| AppState::store(cx).settings()), before);

    click_gear(cx, handle);
    let settings = panel(cx, &workspace).unwrap();
    click(cx, handle, "settings-show-host-address");
    click(cx, handle, close_button(&settings));
    click(cx, handle, "settings-close-save");
    assert!(panel(cx, &workspace).is_none(), "保存后关闭");
    assert_eq!(
        cx.update(|cx| AppState::store(cx).settings())
            .general
            .show_host_address,
        !before.general.show_host_address
    );

    click_gear(cx, handle);
    let settings = panel(cx, &workspace).unwrap();
    click(cx, handle, close_button(&settings));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("settings-close-save").is_none(),
            "没有修改时不问"
        );
    })
    .unwrap();
    assert!(panel(cx, &workspace).is_none());
}

// 关闭窗口、退出时的汇总确认算上设置中未保存的修改
#[gpui_kit::test]
fn window_close_summary_counts_unsaved_settings(cx: &mut TestAppContext) {
    let (handle, workspace) = open_settings(cx);
    assert!(!cx.update(|cx| workspace.read(cx).close_summary(cx).needs_confirm()));
    click(cx, handle, "settings-show-host-address");
    let summary = cx.update(|cx| workspace.read(cx).close_summary(cx));
    assert!(summary.settings && summary.needs_confirm());
    let text = summary.description(crate::close_confirm::CloseScope::Window);
    assert!(
        text.contains(t!("close.settings_unsaved").as_ref()),
        "{text}"
    );
}

// 别处改了设置：没有未保存的修改时界面跟着存储；有修改时不动，保存时按字段合并（别处改的不被这里的旧值覆盖），
// 保存后界面换成合并的结果
#[gpui_kit::test]
fn settings_follow_external_changes_and_merge_on_save(cx: &mut TestAppContext) {
    let (handle, _workspace) = open_settings(cx);
    let store = cx.update(|cx| AppState::store(cx));
    let before = store.settings();
    store
        .update_settings(|s| s.general.confirm_close = false)
        .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("settings-confirm-close").checked(),
            Some(false),
            "没有修改时跟着存储"
        );
        assert!(window.try_find("settings-unsaved").is_none());
    })
    .unwrap();

    click(cx, handle, "settings-show-host-address");
    store
        .update_settings(|s| {
            s.general.confirm_close = true;
            s.appearance.theme = ThemeSetting::Dark;
        })
        .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("settings-confirm-close").checked(),
            Some(false),
            "有修改时不重新载入"
        );
        window.click("settings-save", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    let saved = store.settings();
    assert_eq!(
        saved.general.show_host_address,
        !before.general.show_host_address
    );
    assert!(
        saved.general.confirm_close,
        "别处改的同一组的另一项不被覆盖"
    );
    assert_eq!(
        saved.appearance.theme,
        ThemeSetting::Dark,
        "别处改的另一组不被覆盖"
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("settings-confirm-close").checked(),
            Some(true),
            "保存后界面换成合并的结果"
        );
        assert!(window.try_find("settings-unsaved").is_none());
    })
    .unwrap();
}

// 设置这一栏窄（最小窗口）时左侧分类只显示图标，内容区照常；窗口宽时照常显示分类名
#[gpui_kit::test]
fn narrow_settings_show_icons_only(cx: &mut TestAppContext) {
    let handle = crate::testkit::open_workspace_sized(
        cx,
        gpui_kit::size(gpui_kit::px(720.), gpui_kit::px(480.)),
    );
    set_language(Language::ZhCn);
    click_gear(cx, handle);
    cx.update_window(handle, |_, window, cx| {
        // 第一帧量出宽度，第二帧按它排
        window.render_frame(cx);
        let rem = window.rem_size();
        let nav = window.find(Page::Terminal.nav_id()).bounds();
        assert!(
            nav.size.width < gpui_kit::rems(4.).to_pixels(rem),
            "只有图标：{nav:?}"
        );
        let page = window.find("settings-page").bounds();
        assert!(
            page.size.width > gpui_kit::rems(20.).to_pixels(rem),
            "{page:?}"
        );
        window.click(Page::Terminal.nav_id(), cx);
        window.render_frame(cx);
        assert!(
            window.try_find("settings-font-size").is_some(),
            "点图标照常换页"
        );
    })
    .unwrap();
}
