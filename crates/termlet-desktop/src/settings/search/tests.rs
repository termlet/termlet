//! 设置搜索的测试：五种语言的文字都能搜到登记的项；搜索时导航只列出有命中的分类，命中的项加底色，在收起的一节里、
//! 另一个页签上的展开、切过去，滚到第一个命中的项；每个登记的项都画在登记的地方。

use std::collections::HashSet;

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};

use super::*;
use crate::app_state::AppState;
use crate::i18n::set_language;
use crate::settings::{SettingsForm, Target};
use crate::testkit::{open_workspace, open_workspace_sized, open_workspace_with};
use crate::workspace::Workspace;

/// 搜到的项的锚点（不管这个系统上有没有这一项）。
fn anchors(query: &str) -> Vec<&'static str> {
    let terms = terms(query);
    ITEMS
        .iter()
        .zip(haystacks())
        .filter(|(_, text)| matches(text, &terms))
        .map(|(item, _)| item.anchor)
        .collect()
}

// 登记的文案键在五种语言中都有（没有时 rust-i18n 返回键本身，搜索就只能搜到键名）
#[test]
fn every_key_has_text_in_every_language() {
    let keys = ITEMS
        .iter()
        .flat_map(|item| item.keys.iter().copied())
        .chain(Page::ALL.map(|page| page.title_key()));
    for key in keys {
        for language in Language::ALL {
            let text = rust_i18n::t!(key, locale = language.code());
            assert_ne!(text.as_ref(), key, "{language:?} 中没有 {key}");
        }
    }
}

#[test]
fn anchors_are_unique() {
    let mut seen = HashSet::new();
    for item in ITEMS {
        assert!(seen.insert(item.anchor), "锚点 {} 登记了两次", item.anchor);
    }
}

// 每一项都能用任何一种界面语言的标题搜到（中文界面下输入英文、日文也行）
#[test]
fn every_item_is_found_by_its_title_in_every_language() {
    for item in ITEMS {
        for language in Language::ALL {
            let title = strip_placeholders(&rust_i18n::t!(item.keys[0], locale = language.code()));
            assert!(
                anchors(&title).contains(&item.anchor),
                "{} 用 {language:?} 的标题“{title}”搜不到",
                item.anchor
            );
        }
    }
}

// 不分大小写；几个词用空格隔开时每个都要有；只有空白时不算搜索
#[test]
fn every_word_must_match_and_case_is_ignored() {
    assert!(anchors("SSH-AGENT").contains(&"settings-agent-path"));
    assert!(anchors("ssh-agent x11").contains(&"settings-fold-connection-advanced"));
    assert!(anchors("字体").contains(&"settings-font-family"));
    assert!(anchors("字体 没有这个词").is_empty());
    assert!(terms(" \t ").is_empty());
    // 内置危险命令规则按核心中的说明与 ID 搜
    assert!(anchors("递归删除").contains(&"cli-builtin-rules"));
    assert!(anchors("Recursively delete").contains(&"cli-builtin-rules"));
    assert!(anchors("rm-root").contains(&"cli-builtin-rules"));
}

#[test]
fn placeholders_are_left_out() {
    assert_eq!(strip_placeholders("按 %{key} 打开"), "按  打开");
    assert_eq!(strip_placeholders("%{a}%{b}"), "");
    assert_eq!(strip_placeholders("没有右括号 %{x"), "没有右括号 %{x");
}

// ------------------------------------------------------------ 界面

/// 打开主窗口与设置，返回窗口与设置的表单。
fn open_settings(cx: &mut TestAppContext, handle: AnyWindowHandle) -> Entity<SettingsForm> {
    set_language(Language::ZhCn);
    let form = cx
        .update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("open-settings", cx);
            window.render_frame(cx);
            let workspace = Workspace::in_window(window, cx).unwrap();
            let panel = workspace.read(cx).live_settings().expect("设置 Tab");
            panel.read(cx).form()
        })
        .unwrap();
    // 本机字体等在后台读：读完再搜，页面的高度不再变
    cx.run_until_parked();
    form
}

/// 在设置的搜索框中输入 `text`（原来的文字先清掉），画好、滚到命中的项。
fn search(cx: &mut TestAppContext, handle: AnyWindowHandle, text: &str) {
    cx.update_window(handle, |_, window, cx| {
        window.click("settings-search", cx);
        window.press("escape", cx);
        window.input(text, cx);
    })
    .unwrap();
    cx.run_until_parked();
    // 画一帧记下命中的项画在哪里，画完按位置滚动，再画一帧
    for _ in 0..2 {
        cx.update_window(handle, |_, window, cx| window.render_frame(cx))
            .unwrap();
    }
}

/// 上一帧画出的命中的项。
fn drawn(cx: &mut TestAppContext, handle: AnyWindowHandle) -> Vec<&'static str> {
    cx.update_window(handle, |_, window, cx| hits(window, cx).drawn_anchors())
        .unwrap()
}

fn page(cx: &mut TestAppContext, form: &Entity<SettingsForm>) -> Page {
    cx.update(|cx| form.read(cx).page())
}

// 搜索时导航只列出有命中的分类，现在的分类没有命中时换到第一个有命中的，命中的项加底色；Esc 清空后回到原样
#[gpui_kit::test]
fn search_lists_matching_categories_and_marks_hits(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    let form = open_settings(cx, handle);
    search(cx, handle, "字体");
    assert_eq!(
        page(cx, &form),
        Page::Terminal,
        "“通用”没有命中，换到“终端”"
    );
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find(Page::Terminal.nav_id()).is_some());
        assert!(
            window.try_find(Page::Backup.nav_id()).is_none(),
            "没有命中的分类不显示"
        );
    })
    .unwrap();
    assert!(drawn(cx, handle).contains(&"settings-font-family"));

    cx.update_window(handle, |_, window, cx| window.press("escape", cx))
        .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        for page in Page::ALL {
            assert!(window.try_find(page.nav_id()).is_some(), "{page:?}");
        }
    })
    .unwrap();
    assert!(drawn(cx, handle).is_empty(), "清空后不再标出");
    assert_eq!(page(cx, &form), Page::Terminal, "停在原来的分类");
}

// 中文界面下输入英文、日文也能搜到；什么也没搜到时说明一句
#[gpui_kit::test]
fn other_languages_find_settings_too(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    let form = open_settings(cx, handle);
    search(cx, handle, "Copy on select");
    assert!(drawn(cx, handle).contains(&"settings-copy-on-select"));
    search(cx, handle, "フォント");
    assert!(drawn(cx, handle).contains(&"settings-font-family"));
    search(cx, handle, "keepalive");
    assert_eq!(page(cx, &form), Page::Connection);
    assert!(drawn(cx, handle).contains(&"settings-keepalive"));

    search(cx, handle, "没有这样的设置");
    cx.update_window(handle, |_, window, _| {
        assert!(window.try_find("settings-search-none").is_some());
        assert!(
            Page::ALL
                .iter()
                .all(|p| window.try_find(p.nav_id()).is_none())
        );
    })
    .unwrap();
}

// 命中的项在收起的一节里：展开，并滚到看得到的地方
#[gpui_kit::test]
fn hits_in_folded_sections_open_and_scroll_into_view(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    let form = open_settings(cx, handle);
    search(cx, handle, "补全提示");
    assert_eq!(page(cx, &form), Page::Terminal);
    assert!(drawn(cx, handle).contains(&"settings-command-suggest"));
    cx.update_window(handle, |_, window, _| {
        let page = window.find("settings-page").bounds();
        let item = window.find("settings-command-suggest").bounds();
        assert!(
            item.top() >= page.top() && item.bottom() <= page.bottom(),
            "滚到了命中的项：{item:?} / {page:?}"
        );
    })
    .unwrap();

    search(cx, handle, "录制文件");
    cx.update_window(handle, |_, window, _| {
        assert!(
            window.try_find("settings-record-dir").is_some(),
            "展开了“会话日志与录制”"
        );
    })
    .unwrap();
    assert!(drawn(cx, handle).contains(&"settings-record-dir"));
}

// 命中的项在另一个页签上、在 AI 与自动化收起的部分里：切过去、展开
#[gpui_kit::test]
fn hits_switch_tabs_and_open_parts(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    let form = open_settings(cx, handle);
    search(cx, handle, "正则");
    assert_eq!(page(cx, &form), Page::Colors);
    assert!(
        cx.update(|cx| form.read(cx).colors_tab == ColorsTab::Highlight),
        "切到关键词高亮"
    );
    assert!(drawn(cx, handle).contains(&"highlight-rules"));

    search(cx, handle, "MCP");
    assert_eq!(page(cx, &form), Page::Cli);
    assert!(drawn(cx, handle).contains(&"cli-mcp"), "展开了接入方式");
}

// Ctrl+F 到搜索框
#[gpui_kit::test]
fn ctrl_f_focuses_the_search_box(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    let form = open_settings(cx, handle);
    cx.update_window(handle, |_, window, cx| {
        window.click(Page::About.nav_id(), cx);
        window.press("ctrl-f", cx);
        window.input("字体", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(page(cx, &form), Page::Terminal);
}

// 每个登记的项都画在登记的地方（分类、收起的一节、页签都对得上）：搜它的标题、到它的分类，画出来的命中项中有它
#[gpui_kit::test]
fn every_item_is_drawn_where_it_says(cx: &mut TestAppContext) {
    let handle = open_workspace_sized(cx, gpui_kit::size(gpui_kit::px(1280.), gpui_kit::px(900.)));
    let form = open_settings(cx, handle);
    let mut missing = Vec::new();
    for item in ITEMS {
        if item.shown.is_some_and(|shown| !cx.update(|cx| shown(cx))) {
            continue;
        }
        let title = rust_i18n::t!(item.keys[0], locale = "zh-CN").into_owned();
        cx.update_window(handle, |_, window, cx| {
            form.update(cx, |form, cx| {
                form.set_query(&strip_placeholders(&title), window, cx);
                form.show(Target::Page(item.page), cx);
            });
            window.render_frame(cx);
        })
        .unwrap();
        // 本地终端的检测等在后台：等一下再画
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| window.render_frame(cx))
            .unwrap();
        if !drawn(cx, handle).contains(&item.anchor) {
            missing.push(format!("{}（{:?}）", item.anchor, item.page));
        }
    }
    assert!(missing.is_empty(), "没有画出：{}", missing.join("、"));
}

// 只在某些情况下才有的“主密码”一节与其中记住的主密码（便携版用主密码、在这台电脑上记住时）也画在登记的地方
// （上一个测试中保险库在钥匙串中，没有它们）
#[gpui_kit::test]
fn remembered_master_password_is_drawn_where_it_says(cx: &mut TestAppContext) {
    let handle = open_workspace_with(cx, |cx| {
        let remembered = Box::new(termlet_core::vault::MemoryKeyStore::default());
        AppState::install_in_memory_locked("master pw 1", Some(remembered), cx);
        let store = AppState::store(cx);
        store.unlock_vault(&"master pw 1".into()).unwrap();
        store.remember_vault_key().unwrap();
    });
    open_settings(cx, handle);
    for anchor in ["master-password", "master-remembered"] {
        let item = ITEMS.iter().find(|item| item.anchor == anchor).unwrap();
        assert!(item.shown.is_some_and(|shown| cx.update(|cx| shown(cx))));
        let title = rust_i18n::t!(item.keys[0], locale = "zh-CN").into_owned();
        search(cx, handle, &strip_placeholders(&title));
        assert!(drawn(cx, handle).contains(&anchor), "没有画出 {anchor}");
    }
}

fn is_open(cx: &mut TestAppContext, form: &Entity<SettingsForm>, fold: Fold) -> bool {
    cx.update(|cx| form.read(cx).folds.contains(&fold))
}

// 边输入边搜：上一次搜索展开、这次不再命中的收起（不会越开越多）；自己点开的不动；清空后照样开着
#[gpui_kit::test]
fn folds_opened_by_search_close_when_no_longer_hit(cx: &mut TestAppContext) {
    let handle = open_workspace(cx);
    let form = open_settings(cx, handle);
    search(cx, handle, "录制文件");
    assert!(is_open(cx, &form, Fold::Logs));
    search(cx, handle, "代理命令");
    assert!(!is_open(cx, &form, Fold::Logs), "不再命中，收起");
    assert!(is_open(cx, &form, Fold::ConnectionAdvanced));

    cx.update(|cx| form.update(cx, |form, cx| form.toggle_fold(Fold::History, cx)));
    search(cx, handle, "保活");
    assert!(is_open(cx, &form, Fold::History), "自己点开的不动");
    assert!(!is_open(cx, &form, Fold::ConnectionAdvanced));

    search(cx, handle, "代理命令");
    cx.update_window(handle, |_, window, cx| window.press("escape", cx))
        .unwrap();
    cx.run_until_parked();
    assert!(
        is_open(cx, &form, Fold::ConnectionAdvanced),
        "清空后照样开着"
    );
}
