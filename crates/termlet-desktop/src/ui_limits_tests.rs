//! 防臃肿的上限（见 CLAUDE.md 的“防臃肿”）：说明的长度、右键菜单的项数、设置页的控件数、默认显示的工具栏按钮。
//!
//! 已经超过目标的，上限先定在现状（下面写明目标值），只许变少不许变多；降下来以后把上限跟着改小。
//! 不要为了让这些测试通过而放宽上限：超了先想能不能合并、折叠或删掉，确实要加时在合并请求中写明理由。

mod long_descriptions;

use std::collections::{BTreeMap, BTreeSet};

use gpui_kit::component::dock::PanelId;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, ElementId, Role, SharedString, TestAppContext, Window, px,
    size,
};
use termlet_core::model::Host;
use termlet_core::store::Position;

use crate::app_state::AppState;
use crate::i18n::{Language, set_language};
use crate::settings::Page;
use crate::testkit::{expand_host_tree, open_workspace_sized};
use crate::toolbar::ToolbarItem;
use crate::workspace::Workspace;
use long_descriptions::LONG_DESCRIPTIONS;

// ------------------------------------------------------------ 说明的长度

/// 说明的目标长度：zh-CN 去掉空白与插值后不超过 40 字，大约设置页里的一行。
const DESCRIPTION_TARGET: usize = 40;

/// 键名中的某一段是这些词的算说明。
const DESCRIPTION_WORDS: [&str; 14] = [
    "hint",
    "hints",
    "description",
    "desc",
    "note",
    "notes",
    "tip",
    "tips",
    "warning",
    "warn",
    "intro",
    "explain",
    "caution",
    "help",
];

fn is_description(key: &str) -> bool {
    key.split(['.', '_'])
        .any(|part| DESCRIPTION_WORDS.contains(&part))
}

/// 去掉空白与 `%{…}` 插值后的字数。
fn visible_len(text: &str) -> usize {
    let re = regex::Regex::new(r"%\{[A-Za-z0-9_]+\}").unwrap();
    re.replace_all(text, "")
        .chars()
        .filter(|c| !c.is_whitespace())
        .count()
}

// 新的说明不超过 40 字；已经超过的记在 long_descriptions.rs 中，只许变短：删短到 40 字以内时从名单中删掉，
// 删短一些时把名单中的字数改小
#[test]
fn descriptions_stay_short() {
    let zh: BTreeMap<String, String> = crate::_rust_i18n_backend()
        .messages_for_locale("zh-CN")
        .unwrap_or_default()
        .into_iter()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .filter(|(k, _)| !k.starts_with("gpui_component."))
        .collect();
    let listed: BTreeMap<&str, usize> = LONG_DESCRIPTIONS.iter().copied().collect();
    assert_eq!(listed.len(), LONG_DESCRIPTIONS.len(), "名单中有重复的键");
    let mut failures = Vec::new();
    for (key, text) in zh.iter().filter(|(k, _)| is_description(k)) {
        let len = visible_len(text);
        match listed.get(key.as_str()) {
            None if len > DESCRIPTION_TARGET => failures.push(format!(
                "{key}：{len} 字，超过 {DESCRIPTION_TARGET} 字（一行）。删短，实现细节写进用户指南"
            )),
            None => {}
            Some(&cap) if len > cap => {
                failures.push(format!("{key}：{len} 字，比名单中记的 {cap} 字还长"))
            }
            Some(_) if len <= DESCRIPTION_TARGET => failures.push(format!(
                "{key}：已经只有 {len} 字，从 long_descriptions.rs 中删掉"
            )),
            Some(&cap) if len < cap => failures.push(format!(
                "{key}：删短到了 {len} 字，把 long_descriptions.rs 中的 {cap} 改成 {len}"
            )),
            Some(_) => {}
        }
    }
    for key in listed.keys().filter(|k| !zh.contains_key(**k)) {
        failures.push(format!(
            "{key}：文案已经没有了，从 long_descriptions.rs 中删掉"
        ));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// ------------------------------------------------------------ 右键菜单的项数

/// 右键菜单的目标：不超过 12 项、5 组（分隔线隔开），低频项收进子菜单（只一层）。硬上限 16 项。
const MENU_ITEMS_TARGET: usize = 12;
const MENU_GROUPS_TARGET: usize = 5;

/// 弹出着的菜单有几项、几组（子菜单的入口算一项，里面的不算）。
fn menu_shape(window: &mut Window) -> (usize, usize) {
    let menu = window.within("popup-menu");
    let (mut items, mut groups) = (0, 1);
    for ix in 0..100usize {
        match menu.try_find(ix) {
            Some(item) if item.label().is_some() => items += 1,
            // 分隔线没有名称
            Some(_) => groups += 1,
            None => break,
        }
    }
    (items, groups)
}

/// 右键 `id`，量弹出的菜单，再按 Esc 关掉。
fn right_click_menu(
    cx: &mut TestAppContext,
    handle: AnyWindowHandle,
    id: ElementId,
) -> (usize, usize) {
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.right_click(id, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let shape = menu_shape(window);
        assert!(shape.0 > 0, "没有弹出菜单");
        window.press("escape", cx);
        shape
    })
    .unwrap()
}

// 保存的 SSH 主机的终端 Tab 菜单（主机连不上也一样：这些项不看连接状态），侧边栏中这台主机与它的分组的右键菜单
#[gpui_kit::test]
fn context_menus_stay_short(cx: &mut TestAppContext) {
    let handle = open_workspace_sized(cx, size(px(1280.), px(800.)));
    set_language(Language::ZhCn);
    let (group, host) = cx.update(|cx| {
        let store = AppState::store(cx);
        let group = store
            .create(
                termlet_core::model::Group {
                    name: "生产".into(),
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
        let host = store
            .create(
                Host {
                    name: "web".into(),
                    address: "127.0.0.1".into(),
                    port: 1,
                    username: "alice".into(),
                    ..Default::default()
                },
                Some(group.as_str().into()),
                Position::Last,
            )
            .unwrap();
        (group, host)
    });
    let workspace = cx
        .update_window(handle, |_, window, cx| {
            Workspace::in_window(window, cx).unwrap()
        })
        .unwrap();
    cx.update_window(handle, |_, window, cx| {
        workspace.update(cx, |w, cx| w.open_host(&host, window, cx))
    })
    .unwrap();
    cx.run_until_parked();
    let panel = cx.update(|cx| PanelId::from(workspace.read(cx).live_terminals()[0].entity_id()));

    // 终端 Tab 菜单：现在 21 项（目标 12，硬上限 16）、5 组
    let tab: ElementId = ("tab-title", panel.as_u64() as usize).into();
    let (items, groups) = right_click_menu(cx, handle, tab);
    const TAB_MENU_ITEMS: usize = 21;
    assert!(
        items <= TAB_MENU_ITEMS,
        "终端 Tab 菜单 {items} 项，上限 {TAB_MENU_ITEMS}（目标 {MENU_ITEMS_TARGET}）"
    );
    assert!(
        groups <= MENU_GROUPS_TARGET,
        "终端 Tab 菜单 {groups} 组，上限 {MENU_GROUPS_TARGET}"
    );

    // 侧边栏的主机：现在 10 项、3 组
    expand_host_tree(cx);
    let row: ElementId = SharedString::from(format!("row-host-{}", host.as_str())).into();
    let (items, groups) = right_click_menu(cx, handle, row);
    assert!(
        items <= MENU_ITEMS_TARGET,
        "侧边栏主机的菜单 {items} 项，上限 {MENU_ITEMS_TARGET}"
    );
    assert!(
        groups <= MENU_GROUPS_TARGET,
        "侧边栏主机的菜单 {groups} 组，上限 {MENU_GROUPS_TARGET}"
    );

    // 侧边栏的分组：现在 6 项、3 组
    let row: ElementId = SharedString::from(format!("row-group-{}", group.as_str())).into();
    let (items, groups) = right_click_menu(cx, handle, row);
    assert!(
        items <= MENU_ITEMS_TARGET,
        "侧边栏分组的菜单 {items} 项，上限 {MENU_ITEMS_TARGET}"
    );
    assert!(
        groups <= MENU_GROUPS_TARGET,
        "侧边栏分组的菜单 {groups} 组，上限 {MENU_GROUPS_TARGET}"
    );
}

// ------------------------------------------------------------ 设置页的控件数

/// 每个设置页默认可见的控件的目标：不超过 12 个，超过就分小节、低频项折叠进“高级”。
const PAGE_CONTROLS_TARGET: usize = 12;

/// 已经超过目标的页的上限（现状）。列表页（工具栏、关键词高亮、内置规则）的行也算在内。
const PAGE_CONTROLS_OVER: [(Page, usize); 3] = [
    (Page::Terminal, 22),
    (Page::Highlight, 22),
    // 15 条内置的危险命令规则各是一个复选框
    (Page::Cli, 21),
];

/// 设置页中的控件数：复选框、开关、下拉框、输入框（含密码、多行），单选组与分段选择（一组算一个）。
/// 按钮、链接不算。
fn control_count(window: &mut Window) -> usize {
    let page = window.within("settings-page");
    let mut count = 0;
    let mut groups = BTreeSet::new();
    for element in page.elements() {
        match element.role() {
            Some(
                Role::CheckBox
                | Role::Switch
                | Role::ComboBox
                | Role::TextInput
                | Role::PasswordInput
                | Role::MultilineTextInput,
            ) => count += 1,
            Some(Role::RadioButton | Role::Tab) => {
                let path = element.path();
                groups.insert(format!("{:?}", &path[..path.len() - 1]));
            }
            _ => {}
        }
    }
    count + groups.len()
}

// 按 Windows 的界面量（各系统的控件略有不同，Windows 上最多：多了托盘一项）
#[gpui_kit::test]
fn settings_pages_stay_small(cx: &mut TestAppContext) {
    cx.set_platform_info(gpui_kit::PlatformInfo {
        os: gpui_kit::OperatingSystem::Windows,
        target: gpui_kit::PlatformTarget::Native,
    });
    let handle = open_workspace_sized(cx, size(px(1280.), px(800.)));
    set_language(Language::ZhCn);
    let failures = cx
        .update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("open-settings", cx);
            window.render_frame(cx);
            let mut failures = Vec::new();
            let mut total = 0;
            for page in Page::ALL {
                window.click(page.nav_id(), cx);
                window.render_frame(cx);
                let count = control_count(window);
                total += count;
                let cap = PAGE_CONTROLS_OVER
                    .iter()
                    .find(|(p, _)| *p == page)
                    .map_or(PAGE_CONTROLS_TARGET, |(_, cap)| *cap);
                if count > cap {
                    failures.push(format!(
                        "设置“{}”有 {count} 个控件，上限 {cap}（目标 {PAGE_CONTROLS_TARGET}）",
                        page.title()
                    ));
                }
            }
            assert!(
                total > 0,
                "没有量到任何控件：看看设置页的元素 ID 与控件的无障碍角色有没有变"
            );
            failures
        })
        .unwrap();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// ------------------------------------------------------------ 工具栏

// 默认显示的工具栏按钮：现在 8 个（其中运行中的转发、立即锁定按需出现），目标 5 个；新按钮默认隐藏
#[test]
fn toolbar_defaults_stay_few() {
    const SHOWN_BY_DEFAULT: usize = 8;
    let shown = ToolbarItem::ALL
        .iter()
        .filter(|item| item.shown_by_default())
        .count();
    assert!(
        shown <= SHOWN_BY_DEFAULT,
        "默认显示 {shown} 个工具栏按钮，上限 {SHOWN_BY_DEFAULT}（目标 5）：新按钮默认隐藏"
    );
}
