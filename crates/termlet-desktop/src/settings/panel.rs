//! 设置 Tab：停靠区中的面板，里面是 [`SettingsForm`]。一个窗口最多一个（见
//! [`crate::workspace::Workspace::open_settings`]），可以与终端并排、移到别的窗口。
//!
//! - Tab 名“设置”，有未保存的修改时前面加“● ”（与编辑器的 Tab 相同）；右键菜单有保存、放弃修改、关闭与关闭几个 Tab。
//! - Ctrl+S（macOS ⌘S）保存，Ctrl+Shift+W（⌘W）关闭，Ctrl+F（⌘F）到导航顶上的搜索框。输入框中按 Enter 不保存整页，Esc 也不关；Enter 照对话框里的
//!   做法转成 `Confirm` 动作，各页自己处理（备份、定时备份、同步的表单按 Enter 执行当前一步），没人处理时什么也不做。
//! - 有未保存的修改时关闭先问：保存、放弃修改、取消。一次关几个 Tab、关窗口、退出时并进汇总的确认。备份、恢复进行中
//!   不能关闭。
//! - 移到别的窗口：没有未保存的修改、没有进行中的备份时才能移，设置在新窗口中重新建（输入框绑在打开它的窗口上）。
//! - 界面语言改了：没有未保存的修改时重新建，下拉框等里的文字跟着换。

use gpui_kit::assets::IconName;
use gpui_kit::base::actions::Confirm;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dock::{BasePanel, Panel, PanelEvent};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, TestSupportExt as _, Window,
    actions, div, prelude::FluentBuilder as _, rems,
};

use super::{SettingsForm, Target};
use crate::prose::prose;
use crate::{notify, t};

/// 面板在持久化布局中的名称，发布后不能改。
const PANEL_NAME: &str = "SettingsPanel";
const KEY_CONTEXT: &str = "SettingsPanel";

actions!(
    settings_panel,
    [
        /// 保存设置
        Save,
        /// 关闭设置 Tab（有未保存的修改时先问）
        CloseTab,
        /// 把焦点移到设置的搜索框
        FocusSearch,
    ]
);

/// 注册设置 Tab 固定的快捷键（按当前系统的习惯）。
pub fn init(cx: &mut App) {
    cx.bind_keys(key_bindings(cx.platform_info().os));
}

/// 设置 Tab 固定的快捷键：保存与关闭，与编辑器的 Tab 相同（`KeyBinding::new` 不认 `secondary-`，分开写），查找到
/// 搜索框；单行输入框不处理的 Enter 转成 `Confirm`（与 gpui-kit 的对话框相同）。
pub(crate) fn key_bindings(os: gpui_kit::OperatingSystem) -> Vec<KeyBinding> {
    let context = Some(KEY_CONTEXT);
    let confirm = KeyBinding::new("enter", Confirm { secondary: false }, context);
    if os == gpui_kit::OperatingSystem::Mac {
        vec![
            KeyBinding::new("cmd-s", Save, context),
            KeyBinding::new("cmd-w", CloseTab, context),
            KeyBinding::new("cmd-f", FocusSearch, context),
            confirm,
        ]
    } else {
        vec![
            KeyBinding::new("ctrl-s", Save, context),
            KeyBinding::new("ctrl-shift-w", CloseTab, context),
            KeyBinding::new("ctrl-f", FocusSearch, context),
            confirm,
        ]
    }
}

/// 设置 Tab 发给工作区的请求。
pub(crate) enum SettingsPanelEvent {
    /// 关闭这个 Tab（已确认过或不用确认）
    Close,
}

pub(crate) struct SettingsPanel {
    form: Entity<SettingsForm>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PanelEvent> for SettingsPanel {}
impl EventEmitter<SettingsPanelEvent> for SettingsPanel {}

impl Focusable for SettingsPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.form.read(cx).focus.clone()
    }
}

impl SettingsPanel {
    /// 新的设置 Tab，去 `target`（一个分类或其中的某一处）。
    pub(crate) fn new(target: Target, window: &mut Window, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let form = cx.new(|cx| SettingsForm::new(target, window, cx));
            let mut this = Self {
                form,
                _subscriptions: Vec::new(),
            };
            this.watch(window, cx);
            this
        })
    }

    /// 观察设置（Tab 名前的“●”随它变）与界面语言，提示行登记在这个窗口上。
    fn watch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (notice, focus) = {
            let form = self.form.read(cx);
            (form.notice(), form.focus.clone())
        };
        notify::attach_tab(&notice, focus, window, cx);
        self._subscriptions = vec![
            cx.observe(&self.form, |_, _, cx| cx.notify()),
            cx.observe_global_in::<crate::appearance::AppliedLanguage>(
                window,
                |this, window, cx| this.on_language_changed(window, cx),
            ),
        ];
    }

    /// 去 `target`（一个分类或其中的某一处）。
    pub(crate) fn show(&mut self, target: Target, cx: &mut Context<Self>) {
        self.form.update(cx, |form, cx| form.show(target, cx));
    }

    /// 里面的设置（测试用）。
    #[cfg(test)]
    pub(super) fn form(&self) -> Entity<SettingsForm> {
        self.form.clone()
    }

    /// 现在有没有未保存的修改（关闭窗口前的汇总用）。
    pub(crate) fn has_changes(&self, cx: &App) -> bool {
        self.form.read(cx).has_changes(cx)
    }

    /// 重新建设置（显示同一分类，搜索的文字照留）：移到别的窗口、换了界面语言时。原来有焦点时新的也拿到焦点。
    fn rebuild(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focused = self.form.read(cx).focus.contains_focused(window, cx);
        let page = self.form.read(cx).page();
        let query = self.form.read(cx).query(cx);
        self.form = cx.new(|cx| {
            let mut form = SettingsForm::new(page, window, cx);
            if !query.is_empty() {
                form.set_query(&query, window, cx);
            }
            form
        });
        self.watch(window, cx);
        if focused {
            let focus = self.form.read(cx).focus.clone();
            focus.focus(window, cx);
        }
        cx.notify();
    }

    /// 界面语言改了：没有未保存的修改、没有进行中的备份时重新建（下拉框的选项等是建的时候翻译的）。
    fn on_language_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let form = self.form.read(cx);
        if !form.closing_blocked(cx) && !form.has_changes(cx) {
            self.rebuild(window, cx);
        }
    }

    /// 现在不能移到别的窗口的原因：有未保存的修改、备份或恢复进行中。
    pub(crate) fn move_blocker(&self, cx: &App) -> Option<SharedString> {
        let form = self.form.read(cx);
        if form.closing_blocked(cx) {
            Some(t!("settings.busy"))
        } else if form.has_changes(cx) {
            Some(t!("tab_window.settings_unsaved"))
        } else {
            None
        }
    }

    /// Tab 移到了别的窗口（见 `workspace/move_tab.rs`）：设置在新窗口中重新建，提示行登记在新窗口上。
    pub(crate) fn rebind_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rebuild(window, cx);
    }

    /// 不再确认、直接关闭（一次关闭几个 Tab 时，汇总的确认已经问过）。
    pub(crate) fn close_tab(&mut self, cx: &mut Context<Self>) {
        cx.emit(SettingsPanelEvent::Close);
    }

    /// 点了 ×、按了快捷键：有未保存的修改时先问保存、放弃还是取消；备份、恢复进行中不能关闭。
    pub(crate) fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (blocked, changed) = {
            let form = self.form.read(cx);
            (form.closing_blocked(cx), form.has_changes(cx))
        };
        if blocked {
            notify::warning(t!("settings.busy"), window, cx);
            return;
        }
        if !changed {
            self.close_tab(cx);
            return;
        }
        let panel = cx.entity().downgrade();
        let view = cx.new(|_| CloseConfirm { panel });
        let width = rems(26.).to_pixels(window.rem_size());
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title(t!("settings.close_title"))
                .w(width)
                .child(view.clone())
        });
    }

    /// 保存（Ctrl+S、Tab 菜单）。
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.form.update(cx, |form, cx| {
            form.save(window, cx);
        });
    }

    fn discard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.form.update(cx, |form, cx| form.discard(window, cx));
    }

    /// Tab 上的名称：有未保存的修改时前面加“● ”。
    pub(crate) fn label(&self, cx: &App) -> SharedString {
        if self.form.read(cx).is_dirty() {
            format!("● {}", t!("settings.title")).into()
        } else {
            t!("settings.title")
        }
    }

    /// 竖排的 Tab 列表中这个 Tab 的一行：与横排的 Tab 上相同的图标与名称，悬停显示现在的分类；不归在哪台主机下。
    pub(crate) fn tab_face(&self, cx: &App) -> crate::workspace::TabFace {
        crate::workspace::TabFace {
            icon: Icon::new(IconName::Settings).xsmall().into_any_element(),
            rail_icon: Icon::new(IconName::Settings)
                .with_size(crate::workspace::RAIL_ICON)
                .into_any_element(),
            name: self.label(cx),
            detail: Some(self.form.read(cx).page().title()),
            badges: Vec::new(),
            status: Vec::new(),
            host: None,
        }
    }

    /// 命令面板的“已打开的 Tab”中的说明：现在的分类。
    pub(crate) fn page_title(&self, cx: &App) -> SharedString {
        self.form.read(cx).page().title()
    }
}

impl BasePanel for SettingsPanel {
    fn panel_name(&self) -> &'static str {
        PANEL_NAME
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        if active {
            let focus = self.form.read(cx).focus.clone();
            if !focus.contains_focused(window, cx) {
                focus.focus(window, cx);
            }
        } else {
            // 离开设置：结束快捷键的录入（否则在终端里按的键会被录成快捷键）
            self.form.update(cx, |form, cx| form.stop_recording(cx));
        }
        cx.notify();
    }
}

impl Panel for SettingsPanel {
    fn title(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let id = cx.entity_id().as_u64() as usize;
        // 右键菜单开着时关闭按钮不挂悬停提示（会浮在菜单上，见 crate::menu_tooltip）
        let menu_open = crate::menu_tooltip::menu_open(window, cx);
        let page = self.page_title(cx);
        h_flex()
            .gap_1()
            .child(
                h_flex()
                    .id(("settings-tab-title", id))
                    .test_support()
                    .gap_1()
                    .tooltip(crate::menu_tooltip::quiet(move |window, cx| {
                        Tooltip::new(page.clone()).build(window, cx)
                    }))
                    // 右键 Tab 打开这个 Tab 的菜单（Tab 栏上没有“…”，见 dock_skin）
                    .context_menu({
                        let this = cx.entity().downgrade();
                        move |menu, window, cx| match this.upgrade() {
                            Some(panel) => {
                                panel.update(cx, |panel, cx| panel.dropdown_menu(menu, window, cx))
                            }
                            None => menu,
                        }
                    })
                    .child(Icon::new(IconName::Settings).xsmall())
                    .child(self.label(cx)),
            )
            .child(
                // 自己的关闭按钮（Dock 自带的不显示）
                Button::new(("settings-tab-close", id))
                    .ghost()
                    .small()
                    .icon(IconName::Close)
                    .tab_stop(false)
                    .when(!menu_open, |this| this.tooltip(t!("panel.close")))
                    .on_click(cx.listener(|this, _, window, cx| {
                        cx.stop_propagation();
                        this.request_close(window, cx);
                    })),
            )
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }

    /// Tab 菜单（右键 Tab 打开）：保存、放弃修改、关闭与关闭几个 Tab。
    fn dropdown_menu(
        &mut self,
        menu: PopupMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PopupMenu {
        let this = cx.entity().downgrade();
        let act = |f: fn(&mut SettingsPanel, &mut Window, &mut Context<SettingsPanel>)| {
            let this = this.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let _ = this.update(cx, |this, cx| f(this, window, cx));
            }
        };
        let form = self.form.read(cx);
        let blocked = form.closing_blocked(cx);
        let dirty = form.is_dirty();
        let menu = menu
            .item(
                PopupMenuItem::new(t!("common.save"))
                    .disabled(blocked)
                    .on_click(act(|this, window, cx| this.save(window, cx))),
            )
            .item(
                PopupMenuItem::new(t!("settings.discard"))
                    .disabled(blocked || !dirty)
                    .on_click(act(|this, window, cx| this.discard(window, cx))),
            )
            .separator()
            .item(
                PopupMenuItem::new(t!("panel.close"))
                    .on_click(act(|this, window, cx| this.request_close(window, cx))),
            );
        crate::workspace::close_tab_items(menu, cx.entity_id(), window, cx).separator()
    }
}

impl Render for SettingsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("settings-panel")
            .key_context(KEY_CONTEXT)
            .size_full()
            .on_action(cx.listener(|this, _: &Save, window, cx| this.save(window, cx)))
            .on_action(cx.listener(|this, _: &CloseTab, window, cx| this.request_close(window, cx)))
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                this.form
                    .update(cx, |form, cx| form.focus_search(window, cx))
            }))
            .child(self.form.clone())
    }
}

/// 关闭有未保存的修改的设置 Tab 前的确认：保存、放弃修改、取消。
struct CloseConfirm {
    panel: gpui_kit::WeakEntity<SettingsPanel>,
}

impl CloseConfirm {
    /// 放弃修改并关闭。
    fn discard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.close_dialog(cx);
        let _ = self.panel.update(cx, |panel, cx| panel.close_tab(cx));
    }

    /// 保存后关闭；保存不了（有不合法的值、写不进存储）时不关，设置停在出错的分类上说明原因。
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.close_dialog(cx);
        let _ = self.panel.update(cx, |panel, cx| {
            let saved = panel.form.update(cx, |form, cx| form.save(window, cx));
            if saved {
                panel.close_tab(cx);
            }
        });
    }
}

impl Render for CloseConfirm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(prose(t!("settings.close_description"))),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("settings-close-cancel")
                            .label(t!("common.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("settings-close-discard")
                            .label(t!("settings.discard"))
                            .on_click(cx.listener(|this, _, window, cx| this.discard(window, cx))),
                    )
                    .child(
                        Button::new("settings-close-save")
                            .primary()
                            .label(t!("common.save"))
                            .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                    ),
            )
    }
}
