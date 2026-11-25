//! 设置中的“快捷键”：可以修改的（终端、分屏、窗口）与固定的（鼠标等）。
//!
//! 点“+”后按下新的按键录入（期间拦下所有按键，快捷键不生效），Esc 取消；不允许的组合（只有 Shift 加字母等）
//! 或与其他动作冲突时说明原因。改过的可以恢复默认。这一版还没有的功能不列出。
//!
//! 按当前系统的习惯（[`KeyOs`]）：macOS 上录入、显示 ⌘（`⇧⌘C` 这样的符号写法），不提供照搬 Windows 客户端的键位方案。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyWindowHandle, Context, ElementId, IntoElement, KeybindingKeystroke, KeystrokeEvent,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _,
};
use gpui_kit::{InteractiveElement as _, TestSupportExt as _};
use termlet_core::keymap::{
    self, BindingError, KeyAction, KeyCombo, KeyGroup, KeyInput, KeyOs, KeyPreset,
};
use termlet_core::model::{KeybindingSettings, RightClick};

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
}

impl ShortcutsPage {
    pub(super) fn new(settings: &KeybindingSettings, right_click: RightClick) -> Self {
        Self {
            settings: settings.clone(),
            recording: None,
            _intercept: None,
            _activation: None,
            fixed_open: false,
            error: None,
            right_click,
        }
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

    fn render_fixed(&self, os: KeyOs, cx: &Context<Self>) -> impl IntoElement {
        let right_click = match self.right_click {
            RightClick::Auto => t!("settings.right_click.auto"),
            RightClick::Paste => t!("settings.right_click.paste"),
            RightClick::Menu => t!("settings.right_click.menu"),
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
                t!("settings.keys.column_select"),
            ),
            (
                t!("settings.keys.modifier_wheel", key = wheel),
                t!("settings.keys.wheel_zoom"),
            ),
            (
                t!("settings.keys.modifier_arrows", key = arrows),
                t!("settings.keys.focus_pane"),
            ),
        ];
        // SFTP 文件列表、远程编辑器与主机工具面板的快捷键是固定的（与它们的 key_bindings(os) 相同）
        let (sftp, editor) = if os.is_mac() {
            fixed_mac()
        } else {
            fixed_windows()
        };
        v_flex()
            .gap_4()
            .child(self.render_fixed_group(t!("settings.keys.group_sftp"), sftp, cx))
            .child(self.render_fixed_group(t!("settings.keys.group_editor"), editor, cx))
            .child(self.render_fixed_group(
                t!("settings.keys.group_host_tools"),
                fixed_host_tools(os),
                cx,
            ))
            .child(self.render_fixed_group(
                t!("settings.keys.group_tab_list"),
                fixed_tab_list(os),
                cx,
            ))
            .child(self.render_fixed_group(t!("settings.keys.group_mouse"), mouse, cx))
    }

    fn render_fixed_group(
        &self,
        title: SharedString,
        items: Vec<(SharedString, SharedString)>,
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
                    .child(div().flex_1().child(what))
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
            t!("settings.keys.sftp_move"),
        ),
        (key("Shift+↑ ↓ …"), t!("settings.keys.sftp_extend")),
        (key("Ctrl+↑ ↓"), t!("settings.keys.sftp_cursor")),
        (key("Ctrl+Space"), t!("settings.keys.sftp_toggle_cursor")),
        (
            t!("settings.keys.sftp_type_keys"),
            t!("settings.keys.sftp_type_ahead"),
        ),
        (key("Enter"), t!("settings.keys.sftp_open")),
        (key("Backspace"), t!("settings.keys.sftp_up")),
        (key("F2"), t!("settings.keys.sftp_rename")),
        (key("F5"), t!("settings.keys.sftp_refresh")),
        (key("F7"), t!("settings.keys.sftp_new_folder")),
        (key("Delete"), t!("settings.keys.sftp_delete")),
        (key("Ctrl+A"), t!("settings.keys.sftp_select_all")),
        (key("Ctrl+C"), t!("settings.keys.sftp_copy_paths")),
        (key("Ctrl+V"), t!("settings.keys.sftp_paste_upload")),
        (key("Ctrl+Alt+H"), t!("settings.keys.sftp_toggle_hidden")),
        (key("Shift+F10"), t!("settings.keys.sftp_menu")),
        (key("Ctrl+L  Alt+D"), t!("settings.keys.sftp_path")),
        (key("Tab"), t!("settings.keys.sftp_switch_pane")),
    ];
    let editor = vec![
        (key("Ctrl+S"), t!("settings.keys.editor_save")),
        (key("Ctrl+F"), t!("settings.keys.editor_find")),
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
            t!("settings.keys.sftp_move"),
        ),
        (key("⇧ ↑ ↓ …"), t!("settings.keys.sftp_extend")),
        (
            keys(&["alt-up", "alt-down"]),
            t!("settings.keys.sftp_cursor"),
        ),
        (keys(&["alt-space"]), t!("settings.keys.sftp_toggle_cursor")),
        (
            t!("settings.keys.sftp_type_keys"),
            t!("settings.keys.sftp_type_ahead"),
        ),
        (keys(&["enter", "cmd-down"]), t!("settings.keys.sftp_open")),
        (keys(&["backspace", "cmd-up"]), t!("settings.keys.sftp_up")),
        (keys(&["f2"]), t!("settings.keys.sftp_rename")),
        (keys(&["f5", "cmd-r"]), t!("settings.keys.sftp_refresh")),
        (
            keys(&["f7", "cmd-shift-n"]),
            t!("settings.keys.sftp_new_folder"),
        ),
        (
            keys(&["cmd-backspace", "delete"]),
            t!("settings.keys.sftp_delete"),
        ),
        (keys(&["cmd-a"]), t!("settings.keys.sftp_select_all")),
        (keys(&["cmd-c"]), t!("settings.keys.sftp_copy_paths")),
        (keys(&["cmd-v"]), t!("settings.keys.sftp_paste_upload")),
        (keys(&["cmd->"]), t!("settings.keys.sftp_toggle_hidden")),
        (keys(&["shift-f10"]), t!("settings.keys.sftp_menu")),
        (keys(&["cmd-l"]), t!("settings.keys.sftp_path")),
        (keys(&["tab"]), t!("settings.keys.sftp_switch_pane")),
    ];
    let editor = vec![
        (keys(&["cmd-s"]), t!("settings.keys.editor_save")),
        (keys(&["cmd-f"]), t!("settings.keys.editor_find")),
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
            t!("settings.keys.tools_move"),
        ),
        (keys(&["enter"]), t!("settings.keys.tools_open")),
        (key("← →"), t!("settings.keys.tools_fold")),
        (key("L"), t!("settings.keys.tools_logs")),
        (delete, t!("settings.keys.tools_delete")),
        (keys(&["shift-f10"]), t!("settings.keys.tools_menu")),
        (search, t!("settings.keys.tools_search")),
        (key("Esc"), t!("settings.keys.tools_escape")),
        (pages, t!("settings.keys.tools_pages")),
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
        (key("↑ ↓  Home  End"), t!("settings.keys.tab_list_move")),
        (keys(&["enter"]), t!("settings.keys.tab_list_open")),
        (close, t!("settings.keys.tab_list_close")),
        (key("← →"), t!("settings.keys.tab_list_fold")),
        (keys(&["shift-f10"]), t!("settings.keys.tab_list_menu")),
        (key("Esc"), t!("settings.keys.tab_list_escape")),
    ]
}

/// 固定快捷键的一组：按键的写法、用途。
type FixedKeys = Vec<(SharedString, SharedString)>;

impl Render for ShortcutsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let os = app_keymap::current_os(cx);
        let keymap = keymap::effective(&self.settings, os);
        let muted = cx.theme().muted_foreground;
        let customized = KeyAction::ALL.iter().any(|a| keymap.is_customized(*a));
        let groups = KeyGroup::ALL
            .into_iter()
            .filter_map(|group| {
                let actions: Vec<KeyAction> = group
                    .actions()
                    .filter(|a| app_keymap::is_available(*a))
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
        // 现在的快捷键正好是哪一套键位方案（macOS 上不提供键位方案）
        let presets_available = KeyPreset::available(os);
        let active = KeyPreset::ALL
            .into_iter()
            .find(|p| keymap::apply_preset(&self.settings, *p).bindings == self.settings.bindings);
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
            .child(div().text_xs().text_color(muted).child(prose(hint)))
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
            .child(section::fold(
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
            .when(self.fixed_open, |this| {
                this.child(self.render_fixed(os, cx))
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
    match group {
        KeyGroup::Terminal => t!("settings.keys.group_terminal"),
        KeyGroup::Tabs => t!("settings.keys.group_tabs"),
        KeyGroup::Panes => t!("settings.keys.group_panes"),
        KeyGroup::Window => t!("settings.keys.group_window"),
    }
}

/// 动作的名称（设置页、冲突提示中用）。
pub fn action_name(action: KeyAction) -> SharedString {
    match action {
        KeyAction::Copy => t!("settings.keys.action.copy"),
        KeyAction::Paste => t!("settings.keys.action.paste"),
        KeyAction::Search => t!("settings.keys.action.search"),
        KeyAction::ZoomIn => t!("settings.keys.action.zoom_in"),
        KeyAction::ZoomOut => t!("settings.keys.action.zoom_out"),
        KeyAction::ZoomReset => t!("settings.keys.action.zoom_reset"),
        KeyAction::Duplicate => t!("settings.keys.action.duplicate"),
        KeyAction::SftpBeside => t!("settings.keys.action.sftp_beside"),
        KeyAction::NextTab => t!("settings.keys.action.next_tab"),
        KeyAction::PrevTab => t!("settings.keys.action.prev_tab"),
        KeyAction::Tab1
        | KeyAction::Tab2
        | KeyAction::Tab3
        | KeyAction::Tab4
        | KeyAction::Tab5
        | KeyAction::Tab6
        | KeyAction::Tab7
        | KeyAction::Tab8
        | KeyAction::Tab9 => t!(
            "settings.keys.action.tab_n",
            n = action.tab_index().unwrap_or_default() + 1
        ),
        KeyAction::MoveToNewWindow => t!("settings.keys.action.move_to_new_window"),
        KeyAction::FocusTabList => t!("settings.keys.action.focus_tab_list"),
        KeyAction::Compose => t!("settings.keys.action.compose"),
        KeyAction::MultiExec => t!("settings.keys.action.multi_exec"),
        KeyAction::CommandPicker => t!("settings.keys.action.command_picker"),
        KeyAction::Reconnect => t!("settings.keys.action.reconnect"),
        KeyAction::SplitRight => t!("settings.keys.action.split_right"),
        KeyAction::SplitDown => t!("settings.keys.action.split_down"),
        KeyAction::CommandPalette => t!("settings.keys.action.command_palette"),
        KeyAction::HostTools => t!("settings.keys.action.host_tools"),
        KeyAction::HostToolsOverview => t!("settings.keys.action.host_tools_overview"),
        KeyAction::NewWindow => t!("settings.keys.action.new_window"),
        KeyAction::NewLocal => t!("settings.keys.action.new_local"),
        KeyAction::ToggleSidebar => t!("settings.keys.action.toggle_sidebar"),
        KeyAction::FullScreen => t!("settings.keys.action.full_screen"),
        KeyAction::UiZoomIn => t!("settings.keys.action.ui_zoom_in"),
        KeyAction::UiZoomOut => t!("settings.keys.action.ui_zoom_out"),
        KeyAction::UiZoomReset => t!("settings.keys.action.ui_zoom_reset"),
        KeyAction::Lock => t!("settings.keys.action.lock"),
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
}
