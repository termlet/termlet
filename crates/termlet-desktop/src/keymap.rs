//! 可自定义的快捷键：按设置把核心 [`termlet_core::keymap`] 的动作绑定到界面的动作上。
//!
//! 启动时与设置变化后调用 [`apply`]。GPUI 只能追加快捷键：改掉的组合追加一条同组合的 `Unbind(原动作)` 只解除
//! 原来的那个动作，新的组合追加绑定；只追加与上一次不同的部分。不用 `NoAction`：它会把层级更浅的同组合绑定
//! 一起盖住（终端层让出的组合改给工作区层的动作时，焦点在终端上按不出来）。
//!
//! 组合经键盘映射注册（`KeyBinding::load`）：Windows 上 GPUI 把 Shift 加数字、符号键报告为 Shift 之后的字符
//! （Alt+Shift+= 报告为 `alt-+`），按键盘布局映射后 `alt-shift-=` 才能按出来；`load` 也按当前系统解析
//! `secondary-`（`KeyBinding::new` 不认它）。
//!
//! 默认值与固定的快捷键都按系统分开（macOS 上用 ⌘，见 [`termlet_core::keymap`]）：固定的快捷键由各模块的
//! `key_bindings(os)` 给出，`init` 按当前系统注册，测试在 Windows 上也能逐张检查三个系统的表。

use std::collections::HashMap;
use std::rc::Rc;

use gpui_kit::{
    Action, App, Global, KeyBinding, KeyBindingContextPredicate, OperatingSystem, Unbind,
};
use termlet_core::keymap::{self, KeyAction, KeyOs};
use termlet_core::model::KeybindingSettings;

use crate::terminal_panel::{self, DuplicateSession, Reconnect};
use crate::terminal_search::{self, OpenSearch};
use crate::ui_scale;
use crate::workspace::{
    self, ActivateTab1, ActivateTab2, ActivateTab3, ActivateTab4, ActivateTab5, ActivateTab6,
    ActivateTab7, ActivateTab8, ActivateTab9, FocusTabList, MoveTabToNewWindow, NewLocalTerminal,
    NewWindow, NextTab, OpenCommandPalette, OpenCommandPicker, OpenHostToolsOverview, PrevTab,
    SplitDown, SplitRight, ToggleCompose, ToggleFullscreen, ToggleHostTools, ToggleMultiExec,
    ToggleSidebar,
};
use termlet_terminal::view::{Copy, Paste, ZoomIn, ZoomOut, ZoomReset};

/// GPUI 报告的系统对应的快捷键习惯：macOS 用 ⌘，其余按 Windows、Linux。
pub fn key_os(os: OperatingSystem) -> KeyOs {
    match os {
        OperatingSystem::Mac => KeyOs::Mac,
        OperatingSystem::Windows => KeyOs::Windows,
        _ => KeyOs::Linux,
    }
}

/// 当前系统的快捷键习惯。
pub fn current_os(cx: &App) -> KeyOs {
    key_os(cx.platform_info().os)
}

/// 按下的 `keystroke` 正是 `combo`（设置中的写法，修饰键完全相同）；写法不对时为 false。在按键监听器里判断几个固定的
/// 组合时用（同一个组合也用 [`keymap::label_of`] 显示在提示中）。
pub fn pressed(keystroke: &gpui_kit::Keystroke, combo: &str, os: OperatingSystem) -> bool {
    gpui_kit::Keystroke::parse_with_os(combo, os)
        .is_ok_and(|k| k.key == keystroke.key && k.modifiers == keystroke.modifiers)
}

/// 终端视图自己注册的、可以自定义的默认快捷键（终端 crate 可以单独使用，带着默认值，见
/// `termlet_terminal::view::key_bindings`）：第一次应用时当作已绑定的，用户改掉时同样用 `Unbind` 解除。
fn terminal_builtin(os: KeyOs) -> &'static [(&'static str, KeyAction)] {
    if os.is_mac() {
        &[
            ("cmd-c", KeyAction::Copy),
            ("cmd-v", KeyAction::Paste),
            ("cmd-=", KeyAction::ZoomIn),
            ("cmd-+", KeyAction::ZoomIn),
            ("cmd--", KeyAction::ZoomOut),
            ("cmd-0", KeyAction::ZoomReset),
        ]
    } else {
        &[
            ("ctrl-shift-c", KeyAction::Copy),
            ("ctrl-insert", KeyAction::Copy),
            ("ctrl-shift-v", KeyAction::Paste),
            ("shift-insert", KeyAction::Paste),
            ("ctrl-=", KeyAction::ZoomIn),
            ("ctrl-+", KeyAction::ZoomIn),
            ("ctrl--", KeyAction::ZoomOut),
            ("ctrl-0", KeyAction::ZoomReset),
        ]
    }
}

/// gpui-kit 全局（不带上下文、优先级最高）绑给界面检查器的组合，会盖住同步输入（Windows、Linux 上 Ctrl+Shift+I，
/// macOS 上 ⌥⌘I）：第一次应用时解除。
fn inspector_combo(os: KeyOs) -> &'static str {
    if os.is_mac() {
        "alt-cmd-i"
    } else {
        "ctrl-shift-i"
    }
}

/// 动作在界面上对应的 GPUI 动作与生效的上下文；这一版还没有的功能返回空（设置页不列出）。
fn targets(action: KeyAction) -> Vec<(Box<dyn Action>, &'static str)> {
    let terminal = termlet_terminal::view::KEY_CONTEXT;
    match action {
        KeyAction::Copy => vec![(Box::new(Copy), terminal)],
        KeyAction::Paste => vec![(Box::new(Paste), terminal)],
        KeyAction::Search => vec![
            (Box::new(OpenSearch), terminal),
            (Box::new(OpenSearch), terminal_search::KEY_CONTEXT),
        ],
        KeyAction::ZoomIn => vec![(Box::new(ZoomIn), terminal)],
        KeyAction::ZoomOut => vec![(Box::new(ZoomOut), terminal)],
        KeyAction::ZoomReset => vec![(Box::new(ZoomReset), terminal)],
        KeyAction::Duplicate => vec![(Box::new(DuplicateSession), terminal_panel::KEY_CONTEXT)],
        KeyAction::Reconnect => vec![(Box::new(Reconnect), terminal_panel::KEY_CONTEXT)],
        KeyAction::SplitRight => vec![(Box::new(SplitRight), workspace::KEY_CONTEXT)],
        KeyAction::SplitDown => vec![(Box::new(SplitDown), workspace::KEY_CONTEXT)],
        KeyAction::CommandPalette => vec![(Box::new(OpenCommandPalette), workspace::KEY_CONTEXT)],
        KeyAction::NewLocal => vec![(Box::new(NewLocalTerminal), workspace::KEY_CONTEXT)],
        KeyAction::ToggleSidebar => vec![(Box::new(ToggleSidebar), workspace::KEY_CONTEXT)],
        KeyAction::FullScreen => vec![(Box::new(ToggleFullscreen), workspace::KEY_CONTEXT)],
        KeyAction::UiZoomIn => vec![(Box::new(ui_scale::ZoomInUi), workspace::KEY_CONTEXT)],
        KeyAction::UiZoomOut => vec![(Box::new(ui_scale::ZoomOutUi), workspace::KEY_CONTEXT)],
        KeyAction::UiZoomReset => vec![(Box::new(ui_scale::ResetUiZoom), workspace::KEY_CONTEXT)],
        KeyAction::CommandPicker => vec![(Box::new(OpenCommandPicker), workspace::KEY_CONTEXT)],
        KeyAction::MultiExec => vec![(Box::new(ToggleMultiExec), workspace::KEY_CONTEXT)],
        KeyAction::Compose => vec![(Box::new(ToggleCompose), workspace::KEY_CONTEXT)],
        KeyAction::HostTools => vec![(Box::new(ToggleHostTools), workspace::KEY_CONTEXT)],
        KeyAction::HostToolsOverview => {
            vec![(Box::new(OpenHostToolsOverview), workspace::KEY_CONTEXT)]
        }
        KeyAction::Lock => vec![(Box::new(crate::lock_ui::LockNow), workspace::KEY_CONTEXT)],
        KeyAction::NewWindow => vec![(Box::new(NewWindow), workspace::KEY_CONTEXT)],
        KeyAction::NextTab => vec![(Box::new(NextTab), workspace::KEY_CONTEXT)],
        KeyAction::PrevTab => vec![(Box::new(PrevTab), workspace::KEY_CONTEXT)],
        KeyAction::Tab1 => vec![(Box::new(ActivateTab1), workspace::KEY_CONTEXT)],
        KeyAction::Tab2 => vec![(Box::new(ActivateTab2), workspace::KEY_CONTEXT)],
        KeyAction::Tab3 => vec![(Box::new(ActivateTab3), workspace::KEY_CONTEXT)],
        KeyAction::Tab4 => vec![(Box::new(ActivateTab4), workspace::KEY_CONTEXT)],
        KeyAction::Tab5 => vec![(Box::new(ActivateTab5), workspace::KEY_CONTEXT)],
        KeyAction::Tab6 => vec![(Box::new(ActivateTab6), workspace::KEY_CONTEXT)],
        KeyAction::Tab7 => vec![(Box::new(ActivateTab7), workspace::KEY_CONTEXT)],
        KeyAction::Tab8 => vec![(Box::new(ActivateTab8), workspace::KEY_CONTEXT)],
        KeyAction::Tab9 => vec![(Box::new(ActivateTab9), workspace::KEY_CONTEXT)],
        KeyAction::MoveToNewWindow => {
            vec![(Box::new(MoveTabToNewWindow), workspace::KEY_CONTEXT)]
        }
        KeyAction::FocusTabList => vec![(Box::new(FocusTabList), workspace::KEY_CONTEXT)],
    }
}

/// 这一版有没有这个功能（设置页只列出有的）。
pub fn is_available(action: KeyAction) -> bool {
    !targets(action).is_empty()
}

/// 已经绑定的：（组合, 上下文）→ 动作。
#[derive(Default)]
struct Applied(HashMap<(String, &'static str), KeyAction>);

impl Global for Applied {}

/// 按设置注册快捷键（按当前系统的习惯）。
pub fn apply(settings: &KeybindingSettings, cx: &mut App) {
    let os = current_os(cx);
    let keymap = keymap::effective(settings, os);
    let mut next: HashMap<(String, &'static str), KeyAction> = HashMap::new();
    for (action, combos) in keymap.iter() {
        for (_, context) in targets(action) {
            for combo in combos {
                next.insert((combo.to_string(), context), action);
            }
        }
    }
    let previous = match cx.try_global::<Applied>() {
        Some(applied) => applied.0.clone(),
        None => terminal_builtin(os)
            .iter()
            .map(|(combo, action)| {
                (
                    (combo.to_string(), termlet_terminal::view::KEY_CONTEXT),
                    *action,
                )
            })
            .collect(),
    };
    let mut bindings = Vec::new();
    // gpui-kit 把检查器的组合全局绑给了界面检查器（不带上下文的绑定优先级最高），会盖住同步输入：解除它
    if cx.try_global::<Applied>().is_none() {
        bindings.push(KeyBinding::new(
            inspector_combo(os),
            Unbind("inspector::ToggleInspector".into()),
            None,
        ));
    }
    let app: &App = cx;
    let mut load = |combo: &str, action: Box<dyn Action>, context: &str| {
        let predicate = KeyBindingContextPredicate::parse(context).ok().map(Rc::new);
        match KeyBinding::load(combo, action, predicate, false, None, app) {
            Ok(binding) => bindings.push(binding),
            Err(e) => tracing::warn!("快捷键 {combo} 无效：{e}"),
        }
    };
    for (key, action) in previous.iter().filter(|(key, _)| !next.contains_key(*key)) {
        if let Some((target, _)) = targets(*action).into_iter().find(|(_, c)| *c == key.1) {
            load(&key.0, Box::new(Unbind(target.name().into())), key.1);
        }
    }
    for ((combo, context), action) in &next {
        if previous.get(&(combo.clone(), *context)) == Some(action) {
            continue;
        }
        if let Some((target, _)) = targets(*action).into_iter().find(|(_, c)| c == context) {
            load(combo, target, context);
        }
    }
    cx.bind_keys(bindings);
    cx.set_global(Applied(next));
    // macOS 菜单上显示的快捷键是设置菜单时的键位表
    crate::app_menu::refresh(cx);
}

/// 全部固定的快捷键（各模块的 `key_bindings(os)`，含终端视图自己的），测试逐个系统检查。
#[cfg(test)]
pub(crate) fn fixed_bindings(os: OperatingSystem) -> Vec<KeyBinding> {
    let mut bindings = Vec::new();
    bindings.extend(workspace::key_bindings(os));
    bindings.extend(workspace::tab_list_key_bindings(os));
    bindings.extend(terminal_panel::key_bindings(os));
    bindings.extend(terminal_search::key_bindings(os));
    bindings.extend(crate::editor::key_bindings(os));
    bindings.extend(crate::sftp::file_list::key_bindings(os));
    bindings.extend(crate::host_tools::keys::key_bindings(os));
    bindings.extend(crate::sidebar::host_tree::key_bindings(os));
    bindings.extend(termlet_terminal::view::key_bindings(os));
    bindings.extend(crate::app_menu::key_bindings(os));
    bindings.extend(crate::replay::key_bindings(os));
    bindings.extend(crate::settings::panel::key_bindings(os));
    bindings
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use gpui_kit::{AsKeystroke as _, KeyContext, Keystroke, Modifiers, TestAppContext};

    /// 在 `contexts`（从外到内）中按下 `combo` 会触发的动作。
    fn action_for(cx: &App, combo: &str, contexts: &[&str]) -> Option<&'static str> {
        let stack: Vec<KeyContext> = contexts
            .iter()
            .map(|c| KeyContext::parse(c).unwrap())
            .collect();
        let keystroke = Keystroke::parse_with_os(combo, cx.platform_info().os).unwrap();
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        let (bindings, _) = keymap.bindings_for_input(&[keystroke], &stack);
        bindings.first().map(|b| b.action().name())
    }

    /// gpui-kit 0.8 起 `KeyBinding::new` 不认 `secondary-`（注册时 panic）、`Keystroke::parse` 只收静态字符串：
    /// 注册 Termlet 全部固定的快捷键与各套键位方案的自定义快捷键，每一条都能解析，典型的组合按下去是对应的动作。
    #[gpui_kit::test]
    fn every_shortcut_registers_and_resolves(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            // 固定的快捷键（`KeyBinding::new` 的写法不对时在这里 panic）
            crate::init(cx);
            apply(&KeybindingSettings::default(), cx);

            // 默认与各套键位方案中的每个组合都能经 `KeyBinding::load` 注册（`apply` 遇到失败只记日志）
            let presets = keymap::KeyPreset::ALL
                .iter()
                .map(|preset| keymap::apply_preset(&KeybindingSettings::default(), *preset));
            let os = current_os(cx);
            for settings in std::iter::once(KeybindingSettings::default()).chain(presets) {
                for (action, combos) in keymap::effective(&settings, os).iter() {
                    for (target, context) in targets(action) {
                        let predicate =
                            KeyBindingContextPredicate::parse(context).ok().map(Rc::new);
                        for combo in combos {
                            let combo = combo.to_string();
                            let loaded = KeyBinding::load(
                                &combo,
                                target.boxed_clone(),
                                predicate.clone(),
                                false,
                                None,
                                cx,
                            );
                            assert!(loaded.is_ok(), "{} 的 {combo} 注册失败", action.id());
                        }
                    }
                }
            }

            // 注册的每个绑定都是合法的按键：按当前系统写回文字再解析，得到同样的按键
            let os = cx.platform_info().os;
            let keymap = cx.key_bindings();
            let keymap = keymap.borrow();
            assert!(keymap.bindings().len() > 0);
            for binding in keymap.bindings() {
                for keystroke in binding.keystrokes() {
                    let text = keystroke.unparse(os);
                    let parsed = Keystroke::parse_with_os(&text, os).unwrap_or_else(|e| {
                        panic!("{} 的 {text} 解析失败：{e}", binding.action().name())
                    });
                    assert_eq!(&parsed.key, keystroke.key(), "{text}");
                    assert_eq!(&parsed.modifiers, keystroke.modifiers(), "{text}");
                }
            }
            drop(keymap);

            // 典型的组合（`secondary-` 按当前系统换成 Ctrl 或 Cmd）
            let sftp = ["Workspace", "SftpPanel", "SftpFileList"];
            assert_eq!(
                action_for(cx, "secondary-a", &sftp),
                Some("sftp_list::SelectAll")
            );
            assert_eq!(
                action_for(cx, "secondary-c", &sftp),
                Some("sftp_list::CopyPaths")
            );
            assert_eq!(
                action_for(cx, "secondary-v", &sftp),
                Some("sftp_list::PasteFiles")
            );
            assert_eq!(
                action_for(cx, "f2", &sftp),
                Some("sftp_list::RenameSelected")
            );
            let editor = ["Workspace", "EditorPanel"];
            assert_eq!(
                action_for(cx, "secondary-s", &editor),
                Some("editor_panel::Save")
            );
            assert_eq!(
                action_for(cx, "secondary-f", &editor),
                Some("editor_panel::Find")
            );
            let terminal = [
                workspace::KEY_CONTEXT,
                terminal_panel::KEY_CONTEXT,
                termlet_terminal::view::KEY_CONTEXT,
            ];
            assert_eq!(action_for(cx, "ctrl-shift-c", &terminal), Some(Copy.name()));
            assert_eq!(
                action_for(cx, "shift-insert", &terminal),
                Some(Paste.name())
            );
            assert_eq!(
                action_for(cx, "ctrl-shift-p", &terminal),
                Some(OpenCommandPalette.name())
            );
            assert_eq!(
                action_for(cx, "alt-left", &[workspace::KEY_CONTEXT]),
                Some(workspace::FocusPaneLeft.name())
            );
        });
    }

    #[gpui_kit::test]
    fn moved_shortcut_works_while_the_terminal_has_focus(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut settings = KeybindingSettings::default();
            apply(&settings, cx);
            let stack = [
                workspace::KEY_CONTEXT,
                terminal_panel::KEY_CONTEXT,
                termlet_terminal::view::KEY_CONTEXT,
            ];
            assert_eq!(
                action_for(cx, "ctrl-shift-d", &stack),
                Some(DuplicateSession.name())
            );
            // 复制会话改为 Ctrl+Shift+K，Ctrl+Shift+D 给工作区层的“向右分屏”
            settings
                .bindings
                .insert("duplicate".into(), vec!["ctrl-shift-k".into()]);
            settings
                .bindings
                .insert("splitRight".into(), vec!["ctrl-shift-d".into()]);
            apply(&settings, cx);
            assert_eq!(
                action_for(cx, "ctrl-shift-d", &stack),
                Some(SplitRight.name())
            );
            assert_eq!(
                action_for(cx, "ctrl-shift-k", &stack),
                Some(DuplicateSession.name())
            );
            // 改回去：原来的绑定重新生效
            apply(&KeybindingSettings::default(), cx);
            assert_eq!(
                action_for(cx, "ctrl-shift-d", &stack),
                Some(DuplicateSession.name())
            );
            assert_eq!(action_for(cx, "ctrl-shift-k", &stack), None);
        });
    }

    /// 一个快捷键：按键、生效的上下文、动作。
    pub(crate) struct Entry {
        pub modifiers: Modifiers,
        pub key: String,
        pub context: Option<String>,
        pub action: &'static str,
    }

    /// `os` 上的全部快捷键：固定的与可以自定义的默认值（解除别的动作的 `Unbind` 不算）。
    pub(crate) fn table(os: OperatingSystem) -> Vec<Entry> {
        let context_of = |binding: &KeyBinding| {
            binding.predicate().map(|p| {
                // 终端视图的 `Terminal && !closed` 按 Terminal 算
                let text = p.to_string();
                text.split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_string()
            })
        };
        let unbind = Unbind("".into()).name();
        let mut entries: Vec<Entry> = fixed_bindings(os)
            .iter()
            .filter(|b| b.action().name() != unbind)
            .map(|b| {
                let stroke = &b.keystrokes()[0];
                Entry {
                    modifiers: *stroke.modifiers(),
                    key: stroke.key().to_string(),
                    context: context_of(b),
                    action: b.action().name(),
                }
            })
            .collect();
        for (action, combos) in keymap::Keymap::defaults(key_os(os)).iter() {
            for combo in combos {
                let text = combo.to_string();
                let stroke = Keystroke::parse_with_os(&text, os)
                    .unwrap_or_else(|e| panic!("{os:?} 的 {text} 解析失败：{e}"));
                for (target, context) in targets(action) {
                    entries.push(Entry {
                        modifiers: stroke.modifiers,
                        key: stroke.key.clone(),
                        context: Some(context.to_string()),
                        action: target.name(),
                    });
                }
            }
        }
        entries
    }

    /// 上下文的外层（窗口中的嵌套关系）。
    fn parent(context: &str) -> Option<&'static str> {
        match context {
            "TerminalPanel" | "SftpPanel" | "EditorPanel" | "HostTree" | "HostTools"
            | "TabList" => Some(workspace::KEY_CONTEXT),
            "Terminal" | "TerminalSearch" => Some(terminal_panel::KEY_CONTEXT),
            "SftpFileList" | "SftpPath" | "SftpFilter" => Some("SftpPanel"),
            "HostToolsList" | "HostToolsSearch" => Some("HostTools"),
            _ => None,
        }
    }

    /// 有意盖住外层同一按键的固定快捷键：macOS 上主机工具面板中 ⇧⌘[ ]（`cmd-{`、`cmd-}`）切换面板的页（照 Safari、
    /// Terminal 在当前区域内切换页签的习惯），面板外照常切换 Tab；面板中切换 Tab 另有 ⌃⇥、⌃⇧⇥。
    fn intended_override(os: OperatingSystem, inner: &Entry, outer: &Entry) -> bool {
        use crate::host_tools::keys::{NextPage, PrevPage};
        os == OperatingSystem::Mac
            && inner.context.as_deref() == Some("HostTools")
            && ((inner.action == NextPage.name() && outer.action == NextTab.name())
                || (inner.action == PrevPage.name() && outer.action == PrevTab.name()))
    }

    /// 两个上下文在同一条嵌套链上（相同或一个在另一个里面）：同一按键绑给不同的动作时，内层把外层盖住。
    fn nested(a: &str, b: &str) -> bool {
        let within = |inner: &str, outer: &str| {
            let mut current = Some(inner);
            while let Some(context) = current {
                if context == outer {
                    return true;
                }
                current = parent(context);
            }
            false
        };
        within(a, b) || within(b, a)
    }

    /// Windows、Linux、macOS 三张表（固定的与默认的可自定义快捷键）在 Windows 上也能逐张检查：每个组合按那个系统的写法
    /// 能解析、写回；各表没有冲突（同一按键在同一条上下文嵌套链上绑给不同的动作）；macOS 的应用快捷键都在 ⌘ 上，
    /// 不再占 Ctrl（Ctrl 组合交给终端），只有切换 Tab 的 ⌃⇥、⌃⇧⇥ 例外，Shift 加数字、符号键写 Shift 之后的字符
    /// （`cmd-}`，不写 `cmd-shift-]`）；Windows、Linux 上不用 Win / Super 键。
    #[test]
    fn tables_for_every_os() {
        for os in [
            OperatingSystem::Windows,
            OperatingSystem::Linux,
            OperatingSystem::Mac,
        ] {
            for binding in fixed_bindings(os) {
                for keystroke in binding.keystrokes() {
                    let text = keystroke.as_keystroke().unparse(os);
                    let parsed = Keystroke::parse_with_os(&text, os).unwrap_or_else(|e| {
                        panic!(
                            "{os:?}：{} 的 {text} 解析失败：{e}",
                            binding.action().name()
                        )
                    });
                    assert_eq!(&parsed.key, keystroke.key(), "{os:?} {text}");
                    assert_eq!(&parsed.modifiers, keystroke.modifiers(), "{os:?} {text}");
                }
            }
            let entries = table(os);
            assert!(entries.len() > 50, "{os:?}");
            for entry in &entries {
                let m = entry.modifiers;
                let label = format!("{os:?}：{}（{:?} {}）", entry.action, m, entry.key);
                if os == OperatingSystem::Mac {
                    let tab_switch = m.control
                        && !m.alt
                        && !m.platform
                        && entry.key == "tab"
                        && (entry.action == NextTab.name() || entry.action == PrevTab.name());
                    assert!(
                        !m.control || m.platform || tab_switch,
                        "macOS 上不占 Ctrl：{label}"
                    );
                    // GPUI 把 Shift 加数字、符号键报告为 Shift 之后的字符并去掉 Shift（⇧⌘] 为 cmd-}），这样写的按不出来
                    let symbol = entry.key.chars().count() == 1
                        && !entry.key.chars().all(|c| c.is_ascii_lowercase());
                    assert!(!(m.shift && symbol), "macOS 上写 Shift 之后的字符：{label}");
                } else {
                    assert!(!m.platform, "不用 Win / Super 键：{label}");
                }
            }
            for (i, a) in entries.iter().enumerate() {
                for b in &entries[i + 1..] {
                    if a.modifiers != b.modifiers || a.key != b.key || a.action == b.action {
                        continue;
                    }
                    let (Some(ca), Some(cb)) = (&a.context, &b.context) else {
                        panic!("{os:?}：{} 与 {} 用了同一个按键", a.action, b.action);
                    };
                    if intended_override(os, a, b) || intended_override(os, b, a) {
                        continue;
                    }
                    assert!(
                        !nested(ca, cb),
                        "{os:?}：{:?} {} 同时绑给 {}（{ca}）与 {}（{cb}）",
                        a.modifiers,
                        a.key,
                        a.action,
                        b.action
                    );
                }
            }
        }
    }

    /// 应用记下的“终端自己注册的可自定义快捷键”与终端视图的表一致（改掉时用 Unbind 解除的就是它们）。
    #[test]
    fn terminal_builtin_matches_the_terminal_table() {
        let customizable = [
            Copy.name(),
            Paste.name(),
            ZoomIn.name(),
            ZoomOut.name(),
            ZoomReset.name(),
        ];
        for os in [
            OperatingSystem::Windows,
            OperatingSystem::Linux,
            OperatingSystem::Mac,
        ] {
            let mut from_view: Vec<(String, &'static str)> =
                termlet_terminal::view::key_bindings(os)
                    .iter()
                    .filter(|b| customizable.contains(&b.action().name()))
                    .map(|b| {
                        (
                            b.keystrokes()[0].as_keystroke().unparse(os),
                            b.action().name(),
                        )
                    })
                    .collect();
            let mut recorded: Vec<(String, &'static str)> = terminal_builtin(key_os(os))
                .iter()
                .map(|(combo, action)| {
                    let stroke = Keystroke::parse_with_os(combo, os).unwrap();
                    let (target, _) = targets(*action).remove(0);
                    (stroke.unparse(os), target.name())
                })
                .collect();
            from_view.sort();
            recorded.sort();
            assert_eq!(recorded, from_view, "{os:?}");
        }
    }

    #[test]
    fn pressed_matches_exact_modifiers() {
        let os = OperatingSystem::Windows;
        let key = |text: &str| Keystroke::parse_with_os(text, os).unwrap();
        assert!(pressed(&key("ctrl-s"), "ctrl-s", os));
        assert!(!pressed(&key("ctrl-shift-s"), "ctrl-s", os));
        assert!(!pressed(&key("s"), "ctrl-s", os));
        assert!(pressed(
            &key("cmd-enter"),
            "cmd-enter",
            OperatingSystem::Mac
        ));
        assert!(!pressed(&key("ctrl-s"), "not a combo", os));
        assert_eq!(key_os(OperatingSystem::Mac), KeyOs::Mac);
        assert_eq!(key_os(OperatingSystem::Windows), KeyOs::Windows);
        assert_eq!(key_os(OperatingSystem::Linux), KeyOs::Linux);
    }
}
