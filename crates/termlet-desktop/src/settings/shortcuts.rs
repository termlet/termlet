//! 设置中的“快捷键”：可以修改的（终端、分屏、窗口）与固定的（鼠标等）。
//!
//! 点“+”后按下新的按键录入（期间拦下所有按键，快捷键不生效），Esc 取消；不允许的组合（只有 Shift 加字母等）
//! 或与其他动作冲突时说明原因。改过的可以恢复默认。这一版还没有的功能不列出。
//!
//! 按当前系统的习惯（[`KeyOs`]）：macOS 上录入、显示 ⌘（`⇧⌘C` 这样的符号写法），不提供照搬 Windows 客户端的键位方案。
//!
//! 顶上的搜索框按动作名、分组名与按键过滤（五种界面语言的名称都能搜，见 [`super::search`]），不能修改的快捷键中有
//! 命中的行时展开那一节。设置的搜索搜到快捷键时把文字带到这里。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Icon, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Context, ElementId, Entity, IntoElement,
    KeybindingKeystroke, KeystrokeEvent, ParentElement as _, Render, SharedString, Styled as _,
    Subscription, Window, div, prelude::FluentBuilder as _, rems,
};
use gpui_kit::{InteractiveElement as _, TestSupportExt as _};
use termlet_core::keymap::{
    self, BindingError, KeyAction, KeyCombo, KeyGroup, KeyInput, KeyOs, KeyPreset,
};
use termlet_core::model::{KeybindingSettings, RightClick};

use super::search;
use super::section;
use crate::prose::prose;
use crate::{keymap as app_keymap, t};

/// 固定的快捷键中会与自定义的快捷键冲突的（同一层或更深的一层，自定义的按不出来或把它盖住），录入时拒绝。
/// SFTP 文件列表、编辑器、主机工具面板中的 Ctrl+C、Ctrl+S、Ctrl+PageUp 等只在那里生效，与别处的自定义快捷键不冲突，
/// 不列在这里。
/// 与终端（`termlet_terminal::view::key_bindings`）、终端 Tab、工作区、macOS 应用菜单（`app_menu::key_bindings`）的
/// 固定快捷键一致（测试核对）。
pub(crate) fn reserved_table(os: KeyOs) -> &'static [(&'static str, Reserved)] {
    if os.is_mac() {
        &[
            ("cmd-a", Reserved::SelectAll),
            ("cmd-k", Reserved::Clear),
            ("cmd-up", Reserved::ScrollLine),
            ("cmd-down", Reserved::ScrollLine),
            ("alt-cmd-left", Reserved::FocusPane),
            ("alt-cmd-right", Reserved::FocusPane),
            ("alt-cmd-up", Reserved::FocusPane),
            ("alt-cmd-down", Reserved::FocusPane),
            ("cmd-w", Reserved::CloseTab),
            ("cmd-,", Reserved::Settings),
            ("cmd-q", Reserved::Quit),
            ("cmd-h", Reserved::Hide),
            ("alt-cmd-h", Reserved::HideOthers),
            ("cmd-m", Reserved::Minimize),
            ("cmd-shift-w", Reserved::CloseWindow),
        ]
    } else {
        &[
            ("ctrl-shift-a", Reserved::SelectAll),
            ("ctrl-shift-up", Reserved::ScrollLine),
            ("ctrl-shift-down", Reserved::ScrollLine),
            ("alt-left", Reserved::FocusPane),
            ("alt-right", Reserved::FocusPane),
            ("alt-up", Reserved::FocusPane),
            ("alt-down", Reserved::FocusPane),
            ("ctrl-shift-w", Reserved::CloseTab),
        ]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reserved {
    SelectAll,
    Clear,
    ScrollLine,
    FocusPane,
    CloseTab,
    Settings,
    Quit,
    Hide,
    HideOthers,
    Minimize,
    CloseWindow,
}

impl Reserved {
    fn name(self) -> SharedString {
        match self {
            Reserved::SelectAll => t!("settings.keys.reserved_select_all"),
            Reserved::Clear => t!("settings.keys.reserved_clear"),
            Reserved::ScrollLine => t!("settings.keys.reserved_scroll_line"),
            Reserved::FocusPane => t!("settings.keys.focus_pane"),
            Reserved::CloseTab => t!("settings.keys.reserved_close_tab"),
            Reserved::Settings => t!("settings.keys.reserved_settings"),
            // macOS 应用菜单中的命令：用菜单项的名称
            Reserved::Quit => t!("app_menu.quit"),
            Reserved::Hide => t!("app_menu.hide"),
            Reserved::HideOthers => t!("app_menu.hide_others"),
            Reserved::Minimize => t!("app_menu.minimize"),
            Reserved::CloseWindow => t!("app_menu.close_window"),
        }
    }
}

/// `combo` 是这个系统上固定的快捷键时返回它的用途。
fn reserved(combo: &KeyCombo, os: KeyOs) -> Option<Reserved> {
    reserved_table(os)
        .iter()
        .find(|(text, _)| KeyCombo::parse(text).as_ref() == Some(combo))
        .map(|(_, what)| *what)
}

pub(super) struct ShortcutsPage {
    settings: KeybindingSettings,
    /// 顶上的搜索框
    search: Entity<InputState>,
    /// 搜索的词（小写）；为空时不过滤
    terms: Vec<String>,
    /// 正在录入的动作
    recording: Option<KeyAction>,
    _intercept: Option<Subscription>,
    /// 录入期间窗口失去焦点就结束录入
    _activation: Option<Subscription>,
    /// 展开着不能修改的快捷键（参考表，默认收起）
    fixed_open: bool,
    /// 上一次录入没有成功的原因
    error: Option<(KeyAction, SharedString)>,
    /// 终端中右键的行为（固定快捷键中的说明随之变化，由设置的“终端”分类决定）
    pub(super) right_click: RightClick,
    _search: Subscription,
}

impl ShortcutsPage {
    pub(super) fn new(
        settings: &KeybindingSettings,
        right_click: RightClick,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("settings.keys.search"))
                .clean_on_escape()
        });
        let subscription = cx.subscribe(&search, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.on_query(cx);
            }
        });
        Self {
            settings: settings.clone(),
            search,
            terms: Vec::new(),
            recording: None,
            _intercept: None,
            _activation: None,
            fixed_open: false,
            error: None,
            right_click,
            _search: subscription,
        }
    }

    /// 换成存储中的快捷键（设置重新载入时；搜索的文字照留）。
    pub(super) fn reload(
        &mut self,
        settings: &KeybindingSettings,
        right_click: RightClick,
        cx: &mut Context<Self>,
    ) {
        self.stop_recording(cx);
        self.settings = settings.clone();
        self.right_click = right_click;
        self.error = None;
        cx.notify();
    }

    /// 换上搜索的文字（设置的搜索带过来的；空的为不过滤）。
    pub(super) fn set_query(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.read(cx).value() != query {
            self.search.update(cx, |state, cx| {
                state.set_value(query.to_string(), window, cx)
            });
        }
        self.on_query(cx);
    }

    /// 搜索的文字变了：不能修改的快捷键中有命中的行时展开那一节。
    fn on_query(&mut self, cx: &mut Context<Self>) {
        self.terms = search::terms(&self.search.read(cx).value());
        if !self.terms.is_empty() {
            let os = app_keymap::current_os(cx);
            if self.fixed_groups(os).iter().any(|(title, rows)| {
                rows.iter()
                    .any(|row| fixed_matches(title, row, &self.terms))
            }) {
                self.fixed_open = true;
            }
        }
        cx.notify();
    }

    /// 按 `terms` 命中的行数：可以修改的动作、不能修改的快捷键与键位方案（设置的搜索据此列出这一页）。
    pub(super) fn count_matches(&self, terms: &[String], cx: &App) -> usize {
        let os = app_keymap::current_os(cx);
        let keymap = keymap::effective(&self.settings, os);
        let actions: usize = KeyGroup::ALL
            .into_iter()
            .map(|group| {
                group
                    .actions()
                    .filter(|a| app_keymap::is_available(*a))
                    .filter(|a| action_matches(group, *a, &keymap, os, terms))
                    .count()
            })
            .sum();
        let fixed: usize = self
            .fixed_groups(os)
            .iter()
            .map(|(title, rows)| {
                rows.iter()
                    .filter(|row| fixed_matches(title, row, terms))
                    .count()
            })
            .sum();
        let presets = usize::from(KeyPreset::available(os) && presets_match(terms));
        actions + fixed + presets
    }

    pub(super) fn apply(&self, settings: &mut KeybindingSettings) {
        *settings = self.settings.clone();
    }

    fn start_recording(&mut self, action: KeyAction, window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        self.recording = Some(action);
        let this = cx.entity().downgrade();
        let own: AnyWindowHandle = window.window_handle();
        // 在快捷键分发之前拦下按键；拦截是应用级的，只管这个窗口的按键
        self._intercept = Some(cx.intercept_keystrokes(
            move |event: &KeystrokeEvent, window, cx| {
                if window.window_handle() != own {
                    return;
                }
                // 锁定了（锁屏上输入密码）：结束录入，按键照常
                if crate::lock_ui::is_locked(cx) {
                    let _ = this.update(cx, |this, cx| this.stop_recording(cx));
                    return;
                }
                cx.stop_propagation();
                let _ = this.update(cx, |this, cx| this.on_keystroke(event, cx));
            },
        ));
        self._activation = Some(cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() {
                this.stop_recording(cx);
            }
        }));
        cx.notify();
    }

    /// 结束录入（Esc、录入完成、离开快捷键页、点了别处、窗口失去焦点、锁定）。
    pub(super) fn stop_recording(&mut self, cx: &mut Context<Self>) {
        if self.recording.is_none() {
            return;
        }
        self.recording = None;
        self._intercept = None;
        self._activation = None;
        cx.notify();
    }

    fn on_keystroke(&mut self, event: &KeystrokeEvent, cx: &mut Context<Self>) {
        let Some(action) = self.recording else {
            return;
        };
        if event.keystroke.key == "escape" && !event.keystroke.modifiers.modified() {
            self.stop_recording(cx);
            return;
        }
        // Windows 上 Shift 加数字、符号键报告为 Shift 之后的字符：经键盘映射换回键面上的写法
        let mapped = KeybindingKeystroke::new_with_mapper(
            event.keystroke.clone(),
            false,
            cx.keyboard_mapper().as_ref(),
        );
        let modifiers = mapped.modifiers();
        let input = KeyInput {
            ctrl: modifiers.control,
            alt: modifiers.alt,
            shift: modifiers.shift,
            platform: modifiers.platform,
            key: mapped.key(),
        };
        // 只按了修饰键等：继续等
        let os = app_keymap::current_os(cx);
        let Some(combo) = KeyCombo::from_input(&input, os) else {
            return;
        };
        self.error = match reserved(&combo, os) {
            Some(what) => Some((
                action,
                t!(
                    "settings.keys.reserved",
                    key = combo.label(os),
                    what = what.name()
                ),
            )),
            None => match keymap::add_binding(&self.settings, action, combo, os) {
                Ok(next) => {
                    self.settings = next;
                    None
                }
                Err(e) => Some((action, describe(&e, os))),
            },
        };
        self.stop_recording(cx);
    }

    /// 恢复默认。默认的组合已被别的动作占用时拿不回来，说明被谁占用。
    fn reset(&mut self, action: KeyAction, cx: &mut Context<Self>) {
        let os = app_keymap::current_os(cx);
        self.settings = keymap::reset_binding(&self.settings, action, os);
        let keymap = keymap::effective(&self.settings, os);
        let taken = action
            .defaults(os)
            .iter()
            .filter_map(|text| KeyCombo::parse(text))
            .filter(|combo| !keymap.combos(action).contains(combo))
            .find_map(|combo| keymap.conflict(&combo, action).map(|owner| (combo, owner)));
        self.error = taken.map(|(combo, owner)| {
            (
                action,
                t!(
                    "settings.keys.reset_taken",
                    key = combo.label(os),
                    action = action_name(owner)
                ),
            )
        });
        cx.notify();
    }

    fn render_action(
        &self,
        action: KeyAction,
        keymap: &keymap::Keymap,
        os: KeyOs,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let combos = keymap.combos(action).to_vec();
        let recording = self.recording == Some(action);
        let error = self
            .error
            .as_ref()
            .filter(|(a, _)| *a == action)
            .map(|(_, e)| e.clone());
        let id = |name: &str, extra: &str| {
            ElementId::from(SharedString::from(format!("{name}-{}{extra}", action.id())))
        };
        let chip = |content: gpui_kit::AnyElement| {
            h_flex()
                .gap_0p5()
                .px_1p5()
                .py_0p5()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.border)
                .bg(theme.muted)
                .text_xs()
                .child(content)
        };
        h_flex()
            .gap_4()
            .items_start()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .pt_0p5()
                    .text_sm()
                    .child(action_name(action))
                    .when_some(error, |this, error| {
                        this.child(div().text_xs().text_color(theme.danger).child(prose(error)))
                    }),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .justify_end()
                    .gap_1()
                    .when(combos.is_empty() && !recording, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(t!("settings.keys.none")),
                        )
                    })
                    .children(combos.iter().map(|combo| {
                        let removed = combo.clone();
                        chip(
                            h_flex()
                                .gap_0p5()
                                .child(combo.label(os))
                                .child(
                                    Button::new(id("key-remove", &format!("-{combo}")))
                                        .ghost()
                                        .xsmall()
                                        .icon(IconName::Close)
                                        .tooltip(t!("settings.keys.remove", key = combo.label(os)))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.settings = keymap::remove_binding(
                                                &this.settings,
                                                action,
                                                &removed,
                                                os,
                                            );
                                            this.error = None;
                                            cx.notify();
                                        })),
                                )
                                .into_any_element(),
                        )
                    }))
                    .when(recording, |this| {
                        this.child(chip(
                            div()
                                .id(id("key-recording", ""))
                                .test_support()
                                .text_color(theme.muted_foreground)
                                // 点了别处：结束录入
                                .on_mouse_down_out(
                                    cx.listener(|this, _, _, cx| this.stop_recording(cx)),
                                )
                                .child(prose(t!("settings.keys.press")))
                                .into_any_element(),
                        ))
                    })
                    .when(!recording && combos.len() < keymap::MAX_COMBOS, |this| {
                        this.child(
                            Button::new(id("key-add", ""))
                                .ghost()
                                .small()
                                .icon(IconName::Plus)
                                .tooltip(t!("settings.keys.add"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.start_recording(action, window, cx)
                                })),
                        )
                    })
                    .when(keymap.is_customized(action), |this| {
                        this.child(
                            Button::new(id("key-reset", ""))
                                .ghost()
                                .small()
                                .icon(IconName::Undo2)
                                .tooltip(t!("settings.keys.reset"))
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.reset(action, cx)),
                                ),
                        )
                    }),
            )
    }

    /// 不能修改的快捷键：各组的标题（文案键）与各行。
    fn fixed_groups(&self, os: KeyOs) -> Vec<(&'static str, FixedKeys)> {
        let right_click = match self.right_click {
            RightClick::Auto => "settings.right_click.auto",
            RightClick::Paste => "settings.right_click.paste",
            RightClick::Menu => "settings.right_click.menu",
        };
        // 修饰键加鼠标、方向键：Windows、Linux 上 Alt、Ctrl，macOS 上 ⌥、⌘ 与 ⌥⌘（与终端视图、工作区的处理相同）
        let (drag, wheel, arrows) = if os.is_mac() {
            ("⌥", "⌘", "⌥⌘")
        } else {
            ("Alt", "Ctrl", "Alt")
        };
        let mouse = vec![
            (t!("settings.keys.right_click"), right_click),
            (
                t!("settings.keys.modifier_drag", key = drag),
                "settings.keys.column_select",
            ),
            (
                t!("settings.keys.modifier_wheel", key = wheel),
                "settings.keys.wheel_zoom",
            ),
            (
                t!("settings.keys.modifier_arrows", key = arrows),
                "settings.keys.focus_pane",
            ),
        ];
        // SFTP 文件列表、远程编辑器与主机工具面板的快捷键是固定的（与它们的 key_bindings(os) 相同）
        let (sftp, editor) = if os.is_mac() {
            fixed_mac()
        } else {
            fixed_windows()
        };
        vec![
            ("settings.keys.group_sftp", sftp),
            ("settings.keys.group_editor", editor),
            ("settings.keys.group_host_tools", fixed_host_tools(os)),
            ("settings.keys.group_tab_list", fixed_tab_list(os)),
            ("settings.keys.group_mouse", mouse),
        ]
    }

    /// 不能修改的快捷键（搜索时只有命中的组与行）。
    fn render_fixed(
        &self,
        groups: Vec<(&'static str, FixedKeys)>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        v_flex().gap_4().children(
            groups
                .into_iter()
                .map(|(title, rows)| self.render_fixed_group(t!(title), rows, cx)),
        )
    }

    fn render_fixed_group(
        &self,
        title: SharedString,
        items: FixedKeys,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .gap_1p5()
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(title),
            )
            .children(items.into_iter().map(|(keys, what)| {
                h_flex()
                    .gap_4()
                    .text_sm()
                    .child(div().flex_1().child(t!(what)))
                    .child(
                        div()
                            .px_1p5()
                            .py_0p5()
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.muted)
                            .text_xs()
                            .child(keys),
                    )
            }))
    }
}

/// 固定的快捷键（Windows、Linux）：SFTP 文件列表与远程编辑器。
fn fixed_windows() -> (FixedKeys, FixedKeys) {
    let key = |k: &'static str| SharedString::from(k);
    let sftp = vec![
        (
            key("↑ ↓  Page Up  Page Down  Home  End"),
            "settings.keys.sftp_move",
        ),
        (key("Shift+↑ ↓ …"), "settings.keys.sftp_extend"),
        (key("Ctrl+↑ ↓"), "settings.keys.sftp_cursor"),
        (key("Ctrl+Space"), "settings.keys.sftp_toggle_cursor"),
        (
            t!("settings.keys.sftp_type_keys"),
            "settings.keys.sftp_type_ahead",
        ),
        (key("Enter"), "settings.keys.sftp_open"),
        (key("Backspace"), "settings.keys.sftp_up"),
        (key("F2"), "settings.keys.sftp_rename"),
        (key("F5"), "settings.keys.sftp_refresh"),
        (key("F7"), "settings.keys.sftp_new_folder"),
        (key("Delete"), "settings.keys.sftp_delete"),
        (key("Ctrl+A"), "settings.keys.sftp_select_all"),
        (key("Ctrl+C"), "settings.keys.sftp_copy_paths"),
        (key("Ctrl+V"), "settings.keys.sftp_paste_upload"),
        (key("Ctrl+Alt+H"), "settings.keys.sftp_toggle_hidden"),
        (key("Shift+F10"), "settings.keys.sftp_menu"),
        (key("Ctrl+L  Alt+D"), "settings.keys.sftp_path"),
        (key("Tab"), "settings.keys.sftp_switch_pane"),
    ];
    let editor = vec![
        (key("Ctrl+S"), "settings.keys.editor_save"),
        (key("Ctrl+F"), "settings.keys.editor_find"),
    ];
    (sftp, editor)
}

/// 固定的快捷键（macOS）：照 Finder 的习惯，写法与菜单相同（`⇧⌘N`）。
fn fixed_mac() -> (FixedKeys, FixedKeys) {
    let keys = |combos: &[&str]| {
        SharedString::from(
            combos
                .iter()
                .map(|c| keymap::label_of(c, KeyOs::Mac))
                .collect::<Vec<_>>()
                .join("  "),
        )
    };
    let key = |k: &'static str| SharedString::from(k);
    let sftp = vec![
        (
            key("↑ ↓  Page Up  Page Down  Home  End"),
            "settings.keys.sftp_move",
        ),
        (key("⇧ ↑ ↓ …"), "settings.keys.sftp_extend"),
        (keys(&["alt-up", "alt-down"]), "settings.keys.sftp_cursor"),
        (keys(&["alt-space"]), "settings.keys.sftp_toggle_cursor"),
        (
            t!("settings.keys.sftp_type_keys"),
            "settings.keys.sftp_type_ahead",
        ),
        (keys(&["enter", "cmd-down"]), "settings.keys.sftp_open"),
        (keys(&["backspace", "cmd-up"]), "settings.keys.sftp_up"),
        (keys(&["f2"]), "settings.keys.sftp_rename"),
        (keys(&["f5", "cmd-r"]), "settings.keys.sftp_refresh"),
        (
            keys(&["f7", "cmd-shift-n"]),
            "settings.keys.sftp_new_folder",
        ),
        (
            keys(&["cmd-backspace", "delete"]),
            "settings.keys.sftp_delete",
        ),
        (keys(&["cmd-a"]), "settings.keys.sftp_select_all"),
        (keys(&["cmd-c"]), "settings.keys.sftp_copy_paths"),
        (keys(&["cmd-v"]), "settings.keys.sftp_paste_upload"),
        (keys(&["cmd->"]), "settings.keys.sftp_toggle_hidden"),
        (keys(&["shift-f10"]), "settings.keys.sftp_menu"),
        (keys(&["cmd-l"]), "settings.keys.sftp_path"),
        (keys(&["tab"]), "settings.keys.sftp_switch_pane"),
    ];
    let editor = vec![
        (keys(&["cmd-s"]), "settings.keys.editor_save"),
        (keys(&["cmd-f"]), "settings.keys.editor_find"),
    ];
    (sftp, editor)
}

/// 固定的快捷键：主机工具面板（与 `host_tools::keys::key_bindings` 相同；macOS 上照 Finder 用 ⌘⌫ 删除，⇧⌘[ ] 切换页）。
fn fixed_host_tools(os: KeyOs) -> FixedKeys {
    let key = |k: &'static str| SharedString::from(k);
    let keys = |combos: &[&str]| {
        SharedString::from(
            combos
                .iter()
                .map(|c| keymap::label_of(c, os))
                .collect::<Vec<_>>()
                .join("  "),
        )
    };
    let (delete, search, pages) = if os.is_mac() {
        (
            keys(&["cmd-backspace", "delete"]),
            key("/  ⌘F"),
            key("⇧⌘[  ⇧⌘]"),
        )
    } else {
        (
            key("Delete"),
            key("/  Ctrl+F"),
            key("Ctrl+Page Up  Ctrl+Page Down"),
        )
    };
    vec![
        (
            key("↑ ↓  Page Up  Page Down  Home  End"),
            "settings.keys.tools_move",
        ),
        (keys(&["enter"]), "settings.keys.tools_open"),
        (key("← →"), "settings.keys.tools_fold"),
        (key("L"), "settings.keys.tools_logs"),
        (delete, "settings.keys.tools_delete"),
        (keys(&["shift-f10"]), "settings.keys.tools_menu"),
        (search, "settings.keys.tools_search"),
        (key("Esc"), "settings.keys.tools_escape"),
        (pages, "settings.keys.tools_pages"),
    ]
}

/// 固定的快捷键：竖排的 Tab 列表（与 `workspace::tab_list_key_bindings` 相同；macOS 上另有 ⌘⌫ 关闭）。
fn fixed_tab_list(os: KeyOs) -> FixedKeys {
    let key = |k: &'static str| SharedString::from(k);
    let keys = |combos: &[&str]| {
        SharedString::from(
            combos
                .iter()
                .map(|c| keymap::label_of(c, os))
                .collect::<Vec<_>>()
                .join("  "),
        )
    };
    let close = if os.is_mac() {
        keys(&["cmd-backspace", "delete"])
    } else {
        key("Delete")
    };
    vec![
        (key("↑ ↓  Home  End"), "settings.keys.tab_list_move"),
        (keys(&["enter"]), "settings.keys.tab_list_open"),
        (close, "settings.keys.tab_list_close"),
        (key("← →"), "settings.keys.tab_list_fold"),
        (keys(&["shift-f10"]), "settings.keys.tab_list_menu"),
        (key("Esc"), "settings.keys.tab_list_escape"),
    ]
}

/// 固定快捷键的一组：按键的写法、用途（文案键）。
type FixedKeys = Vec<(SharedString, &'static str)>;

/// 可以修改的动作命中了 `terms`（没有词时都算）：按分组名、动作名（五种语言）与现在的按键。
fn action_matches(
    group: KeyGroup,
    action: KeyAction,
    keymap: &keymap::Keymap,
    os: KeyOs,
    terms: &[String],
) -> bool {
    if terms.is_empty() {
        return true;
    }
    let mut text = search::texts(group_key(group));
    text.push('\n');
    text.push_str(&search::texts(action_key(action)));
    if let Some(ix) = action.tab_index() {
        text.push_str(&format!(" {}", ix + 1));
    }
    for combo in keymap.combos(action) {
        text.push('\n');
        text.push_str(&combo.label(os).to_lowercase());
    }
    search::matches(&text, terms)
}

/// 不能修改的快捷键中的一行命中了 `terms`：按这一节、这一组的标题与用途（五种语言）和按键。
fn fixed_matches(
    title: &str,
    (keys, what): &(SharedString, &'static str),
    terms: &[String],
) -> bool {
    if terms.is_empty() {
        return true;
    }
    let text = [
        search::texts("settings.keys.fixed"),
        search::texts(title),
        search::texts(what),
        keys.to_lowercase(),
    ]
    .join("\n");
    search::matches(&text, terms)
}

/// 键位方案一行命中了 `terms`。
fn presets_match(terms: &[String]) -> bool {
    if terms.is_empty() {
        return true;
    }
    let text = [
        "settings.keys.preset",
        "settings.keys.preset.xshell",
        "settings.keys.preset.mobaxterm",
        "settings.keys.preset_hint",
    ]
    .map(search::texts)
    .join("\n");
    search::matches(&text, terms)
}

impl Render for ShortcutsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let os = app_keymap::current_os(cx);
        let keymap = keymap::effective(&self.settings, os);
        let muted = cx.theme().muted_foreground;
        let customized = KeyAction::ALL.iter().any(|a| keymap.is_customized(*a));
        // 搜索时只列出命中的动作、命中的不能修改的快捷键与键位方案
        let terms = &self.terms;
        let filtering = !terms.is_empty();
        let groups = KeyGroup::ALL
            .into_iter()
            .filter_map(|group| {
                let actions: Vec<KeyAction> = group
                    .actions()
                    .filter(|a| app_keymap::is_available(*a))
                    .filter(|a| action_matches(group, *a, &keymap, os, terms))
                    .collect();
                (!actions.is_empty()).then(|| {
                    v_flex()
                        .gap_1p5()
                        .child(div().text_xs().text_color(muted).child(group_name(group)))
                        .children(
                            actions
                                .into_iter()
                                .map(|a| self.render_action(a, &keymap, os, cx)),
                        )
                })
            })
            .collect::<Vec<_>>();
        let fixed: Vec<(&'static str, FixedKeys)> = self
            .fixed_groups(os)
            .into_iter()
            .map(|(title, rows)| {
                let rows: FixedKeys = rows
                    .into_iter()
                    .filter(|row| fixed_matches(title, row, terms))
                    .collect();
                (title, rows)
            })
            .filter(|(_, rows)| !rows.is_empty())
            .collect();
        // 现在的快捷键正好是哪一套键位方案（macOS 上不提供键位方案）
        let presets_available = KeyPreset::available(os) && presets_match(terms);
        let none = filtering && groups.is_empty() && fixed.is_empty() && !presets_available;
        let active = keymap::active_preset(&self.settings);
        let presets = h_flex()
            .gap_2()
            .items_center()
            .flex_wrap()
            .child(div().text_sm().child(prose(t!("settings.keys.preset"))))
            .children(KeyPreset::ALL.into_iter().map(|preset| {
                Button::new(SharedString::from(format!("key-preset-{}", preset.id())))
                    .outline()
                    .small()
                    .label(preset_name(preset))
                    .selected(active == Some(preset))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        tracing::debug!(preset = preset.id(), "快捷键换成键位方案");
                        this.settings = keymap::apply_preset(&this.settings, preset);
                        this.error = None;
                        cx.notify();
                    }))
            }));
        let hint = if os.is_mac() {
            t!("settings.keys.hint_mac")
        } else {
            t!("settings.keys.hint")
        };
        v_flex()
            .gap_4()
            .child(
                div().w(rems(16.)).child(
                    Input::new(&self.search)
                        .id("key-search")
                        .small()
                        .cleanable(true)
                        .prefix(Icon::new(IconName::Search).xsmall().text_color(muted)),
                ),
            )
            .child(div().text_xs().text_color(muted).child(prose(hint)))
            .when(none, |this| {
                this.child(
                    div()
                        .id("key-search-none")
                        .test_support()
                        .text_sm()
                        .text_color(muted)
                        .child(prose(t!("settings.keys.no_match"))),
                )
            })
            .when(presets_available, |this| {
                this.child(
                    v_flex().gap_1().child(presets).child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(prose(t!("settings.keys.preset_hint"))),
                    ),
                )
            })
            .children(groups)
            // 不能修改的（SFTP、编辑器、主机工具、竖排 Tab、鼠标）只是参考，默认收起
            .when(!fixed.is_empty(), |this| {
                this.child(section::fold(
                    "key-fixed",
                    t!("settings.keys.fixed"),
                    self.fixed_open,
                    true,
                    cx.listener(|this, _, _, cx| {
                        this.fixed_open = !this.fixed_open;
                        cx.notify();
                    }),
                    cx,
                ))
            })
            .when(self.fixed_open && !fixed.is_empty(), |this| {
                this.child(self.render_fixed(fixed, cx))
            })
            .when(customized, |this| {
                this.child(
                    h_flex().child(
                        Button::new("key-reset-all")
                            .outline()
                            .small()
                            .label(t!("settings.keys.reset_all"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.settings = keymap::reset_all(&this.settings, os);
                                this.error = None;
                                cx.notify();
                            })),
                    ),
                )
            })
    }
}

fn describe(error: &BindingError, os: KeyOs) -> SharedString {
    match error {
        BindingError::NeedsModifier(combo) if os.is_mac() => {
            t!("settings.keys.needs_modifier_mac", key = combo.label(os))
        }
        BindingError::NeedsModifier(combo) => {
            t!("settings.keys.needs_modifier", key = combo.label(os))
        }
        BindingError::Conflict { combo, action } => {
            t!(
                "settings.keys.conflict",
                key = combo.label(os),
                action = action_name(*action)
            )
        }
        BindingError::TooMany => t!("settings.keys.too_many", max = keymap::MAX_COMBOS),
    }
}

fn preset_name(preset: KeyPreset) -> SharedString {
    match preset {
        KeyPreset::Xshell => t!("settings.keys.preset.xshell"),
        KeyPreset::MobaXterm => t!("settings.keys.preset.mobaxterm"),
    }
}

fn group_name(group: KeyGroup) -> SharedString {
    t!(group_key(group))
}

/// 分组名的文案键。
fn group_key(group: KeyGroup) -> &'static str {
    match group {
        KeyGroup::Terminal => "settings.keys.group_terminal",
        KeyGroup::Tabs => "settings.keys.group_tabs",
        KeyGroup::Panes => "settings.keys.group_panes",
        KeyGroup::Window => "settings.keys.group_window",
    }
}

/// 动作的名称（设置页、冲突提示中用）。
pub fn action_name(action: KeyAction) -> SharedString {
    match action.tab_index() {
        Some(ix) => t!("settings.keys.action.tab_n", n = ix + 1),
        None => t!(action_key(action)),
    }
}

/// 动作名称的文案键（`Tab1`–`Tab9` 共用一个，序号是插值）。
fn action_key(action: KeyAction) -> &'static str {
    match action {
        KeyAction::Copy => "settings.keys.action.copy",
        KeyAction::Paste => "settings.keys.action.paste",
        KeyAction::Search => "settings.keys.action.search",
        KeyAction::ZoomIn => "settings.keys.action.zoom_in",
        KeyAction::ZoomOut => "settings.keys.action.zoom_out",
        KeyAction::ZoomReset => "settings.keys.action.zoom_reset",
        KeyAction::Duplicate => "settings.keys.action.duplicate",
        KeyAction::NextTab => "settings.keys.action.next_tab",
        KeyAction::PrevTab => "settings.keys.action.prev_tab",
        KeyAction::Tab1
        | KeyAction::Tab2
        | KeyAction::Tab3
        | KeyAction::Tab4
        | KeyAction::Tab5
        | KeyAction::Tab6
        | KeyAction::Tab7
        | KeyAction::Tab8
        | KeyAction::Tab9 => "settings.keys.action.tab_n",
        KeyAction::MoveToNewWindow => "settings.keys.action.move_to_new_window",
        KeyAction::FocusTabList => "settings.keys.action.focus_tab_list",
        KeyAction::Compose => "settings.keys.action.compose",
        KeyAction::MultiExec => "settings.keys.action.multi_exec",
        KeyAction::CommandPicker => "settings.keys.action.command_picker",
        KeyAction::Reconnect => "settings.keys.action.reconnect",
        KeyAction::SplitRight => "settings.keys.action.split_right",
        KeyAction::SplitDown => "settings.keys.action.split_down",
        KeyAction::CommandPalette => "settings.keys.action.command_palette",
        KeyAction::HostTools => "settings.keys.action.host_tools",
        KeyAction::HostToolsOverview => "settings.keys.action.host_tools_overview",
        KeyAction::NewWindow => "settings.keys.action.new_window",
        KeyAction::NewLocal => "settings.keys.action.new_local",
        KeyAction::ToggleSidebar => "settings.keys.action.toggle_sidebar",
        KeyAction::FullScreen => "settings.keys.action.full_screen",
        KeyAction::UiZoomIn => "settings.keys.action.ui_zoom_in",
        KeyAction::UiZoomOut => "settings.keys.action.ui_zoom_out",
        KeyAction::UiZoomReset => "settings.keys.action.ui_zoom_reset",
        KeyAction::Lock => "settings.keys.action.lock",
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{Keystroke, OperatingSystem};

    use super::*;
    use crate::keymap::{fixed_bindings, key_os};

    // 录入时拒绝的固定快捷键，就是终端、终端 Tab、工作区实际注册的固定快捷键（三个系统各一张）；
    // 它们在这个系统上本来是允许的组合（否则录入时先报“需要修饰键”，用不上这张表）
    #[test]
    fn reserved_keys_are_the_fixed_shortcuts() {
        for os in [
            OperatingSystem::Windows,
            OperatingSystem::Linux,
            OperatingSystem::Mac,
        ] {
            let fixed = fixed_bindings(os);
            for (combo, _) in reserved_table(key_os(os)) {
                let stroke = Keystroke::parse_with_os(combo, os).unwrap();
                assert!(
                    fixed.iter().any(|b| {
                        let k = &b.keystrokes()[0];
                        k.key() == stroke.key && k.modifiers() == &stroke.modifiers
                    }),
                    "{os:?}：{combo} 不是固定的快捷键"
                );
                assert!(
                    KeyCombo::parse(combo).unwrap().is_allowed(key_os(os)),
                    "{os:?}：{combo}"
                );
            }
        }
    }

    /// 打开设置的快捷键页，返回窗口与这一页。
    fn open_page(
        cx: &mut gpui_kit::TestAppContext,
    ) -> (gpui_kit::AnyWindowHandle, Entity<ShortcutsPage>) {
        use gpui_kit::test::TestWindowExt as _;
        let handle = crate::testkit::open_workspace(cx);
        crate::i18n::set_language(crate::i18n::Language::ZhCn);
        let page = cx
            .update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.click("open-settings", cx);
                window.render_frame(cx);
                window.click(super::super::Page::Shortcuts.nav_id(), cx);
                window.render_frame(cx);
                let workspace = crate::workspace::Workspace::in_window(window, cx).unwrap();
                let panel = workspace.read(cx).live_settings().unwrap();
                let form = panel.read(cx).form();
                form.read(cx).shortcuts.clone()
            })
            .unwrap();
        (handle, page)
    }

    /// 在快捷键页的搜索框中输入 `text`（原来的文字先清掉）。
    fn search(cx: &mut gpui_kit::TestAppContext, handle: gpui_kit::AnyWindowHandle, text: &str) {
        use gpui_kit::test::TestWindowExt as _;
        cx.update_window(handle, |_, window, cx| {
            window.click("key-search", cx);
            window.press("escape", cx);
            window.input(text, cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    // 快捷键页自己的搜索：按动作名（五种语言）、按键过滤；只命中不能修改的快捷键时展开那一节；都没有时说明
    #[gpui_kit::test]
    fn the_page_filters_by_name_and_key(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        let (handle, page) = open_page(cx);
        let shown = |cx: &mut gpui_kit::TestAppContext, id: &'static str| {
            cx.update_window(handle, |_, window, _| window.try_find(id).is_some())
                .unwrap()
        };
        assert!(!cx.update(|cx| page.read(cx).fixed_open));
        search(cx, handle, "F7");
        assert!(
            cx.update(|cx| page.read(cx).fixed_open),
            "命中 SFTP 的新建文件夹：展开"
        );
        assert!(!shown(cx, "key-add-copy"));
        assert!(shown(cx, "key-fixed"));

        search(cx, handle, "复制");
        assert!(shown(cx, "key-add-copy"));
        assert!(!shown(cx, "key-add-paste"), "不相干的动作不显示");
        search(cx, handle, "Paste");
        assert!(shown(cx, "key-add-paste"), "英文的动作名");
        assert!(!shown(cx, "key-add-copy"));
        search(cx, handle, "ctrl+shift+c");
        assert!(shown(cx, "key-add-copy"), "按键");

        search(cx, handle, "没有这样的快捷键");
        assert!(shown(cx, "key-search-none"));
        assert!(!shown(cx, "key-fixed"));
    }

    // 设置的搜索搜到快捷键时把文字带到这一页的搜索框，只显示命中的行；清空后都回来
    #[gpui_kit::test]
    fn settings_search_carries_over(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;
        let (handle, page) = open_page(cx);
        cx.update_window(handle, |_, window, cx| {
            window.click("settings-search", cx);
            window.input("分屏", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("key-add-splitRight").is_some());
            assert!(window.try_find("key-add-copy").is_none());
        })
        .unwrap();
        assert_eq!(
            cx.update(|cx| page.read(cx).search.read(cx).value()),
            "分屏"
        );

        cx.update_window(handle, |_, window, cx| window.press("escape", cx))
            .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("key-add-copy").is_some());
        })
        .unwrap();
        assert_eq!(cx.update(|cx| page.read(cx).search.read(cx).value()), "");
    }
}
