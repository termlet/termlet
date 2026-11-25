//! 主窗口的标题栏：左边是侧边栏的开关、应用名称与本地终端；中间余下的空间里靠右排可自定义的工具栏
//! （[`crate::toolbar`]）；最右边固定是更新提示、同步状态与设置。
//!
//! - 标题栏整条是拖动窗口的区域：Windows 上真实的鼠标按下会先问它（WM_NCHITTEST），指针下面有拖动区域就按拖动窗口
//!   处理，按钮收不到点击。按钮放进遮挡下层的容器（`occlude`），容器以外的地方（名称、空白）照常拖动。
//! - 工具栏按钮上右键：“隐藏这个按钮”“自定义工具栏…”（设置的“工具栏”页）；设置按钮、“更多”按钮上右键也有后者。
//! - 窗口窄、界面放大时放不下的按钮（排在后面的）收进“更多”菜单（[`crate::toolbar::Fit`]）；工具栏绝对定位在
//!   余下的空间里、超出的部分裁掉，不会把标题栏撑出窗口、挤掉窗口按钮。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Selectable as _, Sizable as _, h_flex};
use gpui_kit::{
    AnyElement, App, Context, Div, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, Styled as _, TestSupportExt as _, WeakEntity, Window, div,
    prelude::FluentBuilder as _,
};

use super::{
    KEY_CONTEXT, NewLocalTerminal, OpenCommandPicker, ToggleCompose, ToggleHostTools,
    ToggleMultiExec, ToggleSidebar, Workspace, no_drag,
};
use crate::app_state::AppState;
use crate::terminal_panel::TerminalPanel;
use crate::toolbar::{self, ToolbarItem};
use crate::{local_shell, settings, t};

/// 这一帧工具栏的状况：各按钮出现与否、选中与否。
struct BarState {
    has_terminals: bool,
    tools_open: bool,
    syncing: bool,
    compose_open: bool,
    forwards: usize,
    /// 当前是 SSH 终端时：为它的主机打开 SFTP
    sftp_for: Option<WeakEntity<TerminalPanel>>,
    lock_enabled: bool,
    dark: bool,
    /// 窗口中开着弹出菜单（右键按钮打开的“隐藏按钮”“自定义工具栏…”等）：按钮不挂悬停提示，免得盖住菜单
    /// （按钮的提示只在按下左键时收起，见 [`crate::menu_tooltip`]）
    menu_open: bool,
}

impl BarState {
    /// 按钮这时出现不出现（设置中隐藏的另算）。
    fn applies(&self, item: ToolbarItem) -> bool {
        match item {
            ToolbarItem::HostTools => self.has_terminals || self.tools_open,
            ToolbarItem::MultiExec | ToolbarItem::Snippets => self.has_terminals,
            ToolbarItem::Compose => self.has_terminals || self.compose_open,
            ToolbarItem::Forwards => self.forwards > 0,
            ToolbarItem::Sftp => self.sftp_for.is_some(),
            ToolbarItem::Highlight | ToolbarItem::Theme => true,
            ToolbarItem::Lock => self.lock_enabled,
        }
    }
}

impl Workspace {
    fn bar_state(&self, window: &Window, cx: &App) -> BarState {
        BarState {
            has_terminals: !self.live_terminals().is_empty(),
            tools_open: AppState::global(cx).ui.read(cx).state().host_tools_open,
            syncing: self.is_syncing(),
            compose_open: self.compose.is_some(),
            forwards: crate::forwards::active_count(cx),
            sftp_for: self.sftp_terminal(window, cx).map(|p| p.downgrade()),
            lock_enabled: crate::lock_ui::is_enabled(cx),
            dark: cx.theme().is_dark(),
            menu_open: crate::menu_tooltip::menu_open(window, cx),
        }
    }

    /// 标题栏的 SFTP 按钮对着的终端：命令对着的终端（[`Self::command_target`]）是 SSH 终端时。
    fn sftp_terminal(&self, window: &Window, cx: &App) -> Option<gpui_kit::Entity<TerminalPanel>> {
        self.command_target(window, cx)
            .filter(|p| p.read(cx).sftp_target().is_some())
    }

    /// 执行工具栏中一个按钮的操作（标题栏上的按钮、“更多”菜单中的一项）。
    pub(super) fn run_toolbar_item(
        &mut self,
        item: ToolbarItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::debug!(button = item.id(), "工具栏");
        match item {
            ToolbarItem::HostTools => self.toggle_host_tools(window, cx),
            ToolbarItem::MultiExec => self.toggle_multi_exec(window, cx),
            ToolbarItem::Forwards => self.show_forwards(window, cx),
            ToolbarItem::Sftp => {
                if let Some(terminal) = self.sftp_terminal(window, cx) {
                    self.open_sftp_for_terminal(terminal, window, cx);
                }
            }
            ToolbarItem::Snippets => self.open_command_picker(window, cx),
            ToolbarItem::Compose => self.toggle_compose(window, cx),
            ToolbarItem::Highlight => self.open_settings(settings::Page::Highlight, window, cx),
            ToolbarItem::Theme => self.toggle_theme(window, cx),
            ToolbarItem::Lock => {
                crate::lock_ui::lock_now(cx);
            }
        }
    }

    pub(super) fn render_title_bar(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let state = self.bar_state(window, cx);
        let saved = AppState::store(cx).local_settings().window.toolbar;
        let present: Vec<ToolbarItem> = toolbar::shown(&saved)
            .into_iter()
            .filter(|item| state.applies(*item))
            .collect();
        let fit = self.toolbar_fit.clone();
        let count = fit.visible(&present);
        let (shown, overflow) = present.split_at(count);
        let (shown, overflow) = (shown.to_vec(), overflow.to_vec());
        let more = !overflow.is_empty();
        let buttons: Vec<AnyElement> = shown
            .iter()
            .map(|item| self.toolbar_button(*item, &state, cx))
            .collect();
        let toolbar = h_flex()
            .gap_1()
            // 量这一帧各按钮的宽度
            .on_children_prepainted({
                let fit = fit.clone();
                let shown = shown.clone();
                move |bounds, window, _| fit.record_items(&shown, more, &bounds, window.rem_size())
            })
            .id("title-toolbar")
            .test_support()
            .absolute()
            .top_0()
            .bottom_0()
            .right_0()
            .occlude()
            .children(buttons)
            .when(more, |el| el.child(self.toolbar_more(overflow, &state, cx)));
        crate::ui_scale::title_bar().child(
            h_flex()
                .w_full()
                .h_full()
                .pr_2()
                // 量工具栏可用的宽度（左右两组之间余下的），放得下的个数变了时再画一帧
                .on_children_prepainted(move |bounds, window, _| {
                    let Some(slot) = bounds.get(1) else {
                        return;
                    };
                    fit.record_available(slot.size.width, window.rem_size());
                    if fit.visible(&present) != count {
                        window.on_next_frame(|window, _| window.refresh());
                    }
                })
                .child(self.render_title_left(cx).flex_none().mr_2())
                .child(
                    div()
                        .relative()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .overflow_hidden()
                        .child(toolbar),
                )
                .child(
                    h_flex()
                        .flex_none()
                        .gap_1()
                        .pl_1()
                        .occlude()
                        .child(self.update_badge.clone())
                        .child(self.sync_indicator.clone())
                        .child(
                            div()
                                .id("toolbar-settings")
                                .flex()
                                .context_menu(|menu, _, _| menu.item(customize_item()))
                                .child(
                                    Button::new("open-settings")
                                        .ghost()
                                        .small()
                                        .icon(IconName::Settings)
                                        .when(!state.menu_open, |b| b.tooltip(t!("settings.title")))
                                        .on_click(|_, window, cx| {
                                            settings::open(settings::Page::General, window, cx)
                                        }),
                                ),
                        ),
                ),
        )
    }

    /// 标题栏左边：侧边栏的开关、应用名称、打开本地终端（“+”与全部本地终端的菜单）。
    fn render_title_left(&self, cx: &mut Context<Self>) -> Div {
        let hidden = self.sidebar_hidden(cx);
        h_flex()
            .gap_2()
            .child(
                no_drag().child(
                    Button::new("toggle-sidebar")
                        .ghost()
                        .small()
                        .icon(if hidden {
                            IconName::PanelLeftOpen
                        } else {
                            IconName::PanelLeftClose
                        })
                        .tooltip_with_action(
                            if hidden {
                                t!("title_bar.show_sidebar")
                            } else {
                                t!("title_bar.hide_sidebar")
                            },
                            &ToggleSidebar,
                            Some(KEY_CONTEXT),
                        )
                        .on_click(
                            cx.listener(|this, _, window, cx| this.toggle_sidebar(window, cx)),
                        ),
                ),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .child(crate::brand::logo(gpui_kit::rems(1.125)))
                    .child(div().text_sm().child(t!("app.name"))),
            )
            .child(
                no_drag()
                    .flex()
                    .items_center()
                    .child(
                        Button::new("new-local-terminal")
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .tooltip_with_action(
                                t!("title_bar.new_local_terminal"),
                                &NewLocalTerminal,
                                Some(KEY_CONTEXT),
                            )
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_local_terminal(window, cx)
                            })),
                    )
                    .child({
                        // 全部本地终端：打开时才读检测结果（只读缓存）
                        let this = cx.entity().downgrade();
                        Button::new("local-terminal-menu")
                            .ghost()
                            .small()
                            .icon(IconName::ChevronDown)
                            .tooltip(t!("title_bar.local_terminals"))
                            .dropdown_menu(move |menu, _, cx| {
                                let settings = AppState::store(cx).local_settings();
                                let default =
                                    local_shell::default_target(&settings, cx).map(|t| t.key());
                                let menu = local_shell::all_targets(&settings, cx)
                                    .into_iter()
                                    .fold(menu, |menu, target| {
                                        let key = target.key();
                                        let this = this.clone();
                                        menu.item(
                                            PopupMenuItem::new(target.title())
                                                .checked(default.as_ref() == Some(&key))
                                                .on_click(move |_, window, cx| {
                                                    let _ = this.update(cx, |w, cx| {
                                                        w.open_local_key(&key, window, cx)
                                                    });
                                                }),
                                        )
                                    });
                                menu.separator().item(
                                    PopupMenuItem::new(t!("hosts.local.manage")).on_click(
                                        |_, window, cx| {
                                            settings::open(settings::Page::Local, window, cx)
                                        },
                                    ),
                                )
                            })
                    }),
            )
    }

    /// 工具栏中的一个按钮（外面包一层接右键菜单）。
    fn toolbar_button(
        &self,
        item: ToolbarItem,
        state: &BarState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // 挂不挂悬停提示（见 BarState::menu_open）
        let tips = !state.menu_open;
        let button = match item {
            ToolbarItem::HostTools => Button::new("toggle-host-tools")
                .icon(if state.tools_open {
                    IconName::PanelRightClose
                } else {
                    IconName::PanelRightOpen
                })
                .selected(state.tools_open)
                .when(tips, |b| {
                    b.tooltip_with_action(
                        t!("workspace.host_tools"),
                        &ToggleHostTools,
                        Some(KEY_CONTEXT),
                    )
                }),
            ToolbarItem::MultiExec => Button::new("toggle-multi-exec")
                .icon(IconName::LayoutGrid)
                .selected(state.syncing)
                .when(state.syncing, |b| b.text_color(cx.theme().warning))
                .when(tips, |b| {
                    b.tooltip_with_action(
                        t!("multi_exec.title"),
                        &ToggleMultiExec,
                        Some(KEY_CONTEXT),
                    )
                }),
            ToolbarItem::Forwards => Button::new("title-forwards")
                .icon(IconName::ArrowRightLeft)
                .label(t!("forward.title_bar", count = state.forwards))
                .when(tips, |b| b.tooltip(t!("forward.title_bar_hint"))),
            ToolbarItem::Sftp => Button::new("open-sftp-for-tab")
                .icon(IconName::FolderOpen)
                .label("SFTP")
                .when(tips, |b| b.tooltip(t!("sftp.open_for_tab"))),
            ToolbarItem::Snippets => Button::new("title-command-picker")
                .icon(IconName::Zap)
                .when(tips, |b| {
                    b.tooltip_with_action(
                        t!("title_bar.command_picker"),
                        &OpenCommandPicker,
                        Some(KEY_CONTEXT),
                    )
                }),
            ToolbarItem::Compose => Button::new("title-compose")
                .icon(IconName::SquarePen)
                .selected(state.compose_open)
                .when(tips, |b| {
                    b.tooltip_with_action(
                        t!("toolbar.item.compose"),
                        &ToggleCompose,
                        Some(KEY_CONTEXT),
                    )
                }),
            ToolbarItem::Highlight => Button::new("title-highlight")
                .icon(IconName::Highlighter)
                .when(tips, |b| b.tooltip(t!("title_bar.highlight"))),
            ToolbarItem::Theme => Button::new("title-toggle-theme")
                .icon(if state.dark {
                    IconName::Sun
                } else {
                    IconName::Moon
                })
                .when(tips, |b| {
                    b.tooltip(if state.dark {
                        t!("title_bar.to_light")
                    } else {
                        t!("title_bar.to_dark")
                    })
                }),
            ToolbarItem::Lock => {
                Button::new("title-lock-now")
                    .icon(IconName::Lock)
                    .when(tips, |b| {
                        b.tooltip_with_action(
                            t!("lock_ui.lock_now_tooltip"),
                            &crate::lock_ui::LockNow,
                            Some(KEY_CONTEXT),
                        )
                    })
            }
        };
        div()
            .id(SharedString::from(format!("toolbar-{}", item.id())))
            .flex()
            .context_menu(move |menu, _, _| {
                menu.item(
                    PopupMenuItem::new(t!("toolbar.hide_button"))
                        .on_click(move |_, window, cx| toolbar::hide(item, window, cx)),
                )
                .item(customize_item())
            })
            .child(button.ghost().small().on_click(
                cx.listener(move |this, _, window, cx| this.run_toolbar_item(item, window, cx)),
            ))
            .into_any_element()
    }

    /// 放不下的按钮收进的“更多”菜单。
    fn toolbar_more(
        &self,
        overflow: Vec<ToolbarItem>,
        state: &BarState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let checked = |item: ToolbarItem| match item {
            ToolbarItem::HostTools => state.tools_open,
            ToolbarItem::MultiExec => state.syncing,
            ToolbarItem::Compose => state.compose_open,
            _ => false,
        };
        let items: Vec<(ToolbarItem, SharedString, bool)> = overflow
            .into_iter()
            .map(|item| {
                let title = match item {
                    // 菜单中写出个数（标题栏上的按钮也写着）
                    ToolbarItem::Forwards => t!("forward.title_bar", count = state.forwards),
                    _ => item.title(),
                };
                (item, title, checked(item))
            })
            .collect();
        div()
            .id("toolbar-more-menu")
            .flex()
            .context_menu(|menu, _, _| menu.item(customize_item()))
            .child(
                Button::new("title-toolbar-more")
                    .ghost()
                    .small()
                    .icon(IconName::Ellipsis)
                    .when(!state.menu_open, |b| b.tooltip(t!("toolbar.more")))
                    .dropdown_menu(move |menu, _, _| {
                        items
                            .iter()
                            .fold(menu, |menu, (item, title, checked)| {
                                let (item, this) = (*item, this.clone());
                                menu.item(
                                    PopupMenuItem::new(title.clone())
                                        .icon(item.icon())
                                        .checked(*checked)
                                        .on_click(move |_, window, cx| {
                                            let _ = this.update(cx, |w, cx| {
                                                w.run_toolbar_item(item, window, cx)
                                            });
                                        }),
                                )
                            })
                            .separator()
                            .item(customize_item())
                    }),
            )
            .into_any_element()
    }
}

/// 菜单中的“自定义工具栏…”：打开设置的“工具栏”页。
fn customize_item() -> PopupMenuItem {
    PopupMenuItem::new(t!("toolbar.customize"))
        .on_click(|_, window, cx| settings::open(settings::Page::Toolbar, window, cx))
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Bounds, Pixels, ScrollDelta, TestAppContext, point, px,
        size,
    };
    use termlet_core::model::ThemeSetting;

    use super::Workspace;
    use crate::app_state::AppState;
    use crate::i18n::{Language, set_language};
    use crate::testkit::{open_workspace, open_workspace_sized};
    use crate::toolbar::{self, ToolbarItem};

    /// 设置中显示的按钮，按顺序。
    fn shown(cx: &mut TestAppContext) -> Vec<ToolbarItem> {
        cx.update(|cx| toolbar::shown(&AppState::store(cx).local_settings().window.toolbar))
    }

    fn bounds_of(cx: &mut TestAppContext, handle: AnyWindowHandle, id: &str) -> Bounds<Pixels> {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.find(id.to_string()).bounds()
        })
        .unwrap()
    }

    // 标题栏按钮上右键“隐藏这个按钮”：立即存进本机设置（会同步的设置不变），按钮不见了；它的功能仍可从命令面板使用
    #[gpui_kit::test]
    fn hidden_button_is_still_in_the_command_palette(cx: &mut TestAppContext) {
        let handle = open_workspace(cx);
        set_language(Language::ZhCn);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.right_click("title-toggle-theme", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            // 隐藏这个按钮、自定义工具栏…
            window.within("popup-menu").click(0usize, cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert!(!shown(cx).contains(&ToolbarItem::Theme));
        assert!(shown(cx).contains(&ToolbarItem::Highlight));
        assert_eq!(
            cx.update(|cx| AppState::store(cx).settings()),
            Default::default(),
            "会同步的设置没有变"
        );
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("title-toggle-theme").is_none());
            assert!(window.try_find("title-highlight").is_some());
            window.press("ctrl-shift-p", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.input("> 切换浅色", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_ne!(
            cx.update(|cx| AppState::store(cx).settings().appearance.theme),
            ThemeSetting::System,
            "从命令面板切换了浅色 / 深色"
        );
    }

    // 右键菜单的“自定义工具栏…”打开设置的“工具栏”页：调整顺序、显示撰写栏，保存后标题栏照着排；“恢复默认”后回到
    // 默认的顺序，设置中存空
    #[gpui_kit::test]
    fn toolbar_page_orders_shows_and_resets(cx: &mut TestAppContext) {
        let handle = open_workspace(cx);
        set_language(Language::ZhCn);
        let highlight_first = |cx: &mut TestAppContext| {
            bounds_of(cx, handle, "title-highlight").left()
                < bounds_of(cx, handle, "title-toggle-theme").left()
        };
        assert!(highlight_first(cx), "默认关键词高亮在前");
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.right_click("title-highlight", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.within("popup-menu").click(1usize, cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window.try_find("settings-toolbar").is_some(),
                "打开了工具栏页"
            );
            // 主题、撰写栏在列表的后半，测试窗口中往下滚才看得到
            window.scroll(
                "settings-page",
                ScrollDelta::Pixels(point(px(0.), px(-400.))),
                cx,
            );
            window.render_frame(cx);
            window.click("toolbar-up-theme", cx);
            window.click("toolbar-show-compose", cx);
            window.render_frame(cx);
            window.click("settings-save", cx);
        })
        .unwrap();
        cx.run_until_parked();
        let saved = shown(cx);
        let at = |item| saved.iter().position(|i| *i == item);
        assert!(at(ToolbarItem::Theme) < at(ToolbarItem::Highlight));
        assert!(at(ToolbarItem::Compose).is_some(), "撰写栏显示了");
        assert!(!highlight_first(cx), "标题栏上主题排到了前面");

        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("open-settings", cx);
            window.render_frame(cx);
            window.click("settings-nav-toolbar", cx);
            window.render_frame(cx);
            window.scroll(
                "settings-page",
                ScrollDelta::Pixels(point(px(0.), px(-600.))),
                cx,
            );
            window.render_frame(cx);
            window.click("toolbar-reset", cx);
            window.render_frame(cx);
            window.click("settings-save", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert!(
            cx.update(|cx| AppState::store(cx).local_settings().window.toolbar)
                .is_empty(),
            "恢复默认后存空"
        );
        assert!(highlight_first(cx));
    }

    /// 界面放大到 150%、全部按钮都显示、有终端（按钮最多）时打开 `width` 宽的窗口，画几帧让工具栏量好放得下几个；
    /// 返回窗口与这时应当出现的按钮的 ID（按顺序）。
    fn crowded_title_bar(
        cx: &mut TestAppContext,
        width: f32,
    ) -> (AnyWindowHandle, Vec<&'static str>) {
        let handle = open_workspace_sized(cx, size(px(width), px(480.)));
        set_language(Language::En);
        cx.update(|cx| {
            let mut entries = toolbar::defaults();
            entries.iter_mut().for_each(|e| e.1 = true);
            AppState::store(cx)
                .update_local_settings(|s| {
                    s.window.ui_scale = 150;
                    s.window.toolbar = toolbar::to_saved(&entries, &[]);
                })
                .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            let workspace = Workspace::in_window(window, cx).unwrap();
            workspace.update(cx, |w, cx| w.open_local_terminal(window, cx));
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            for _ in 0..3 {
                window.render_frame(cx);
            }
            assert_eq!(window.rem_size(), px(24.));
        })
        .unwrap();
        // 有终端、没有转发与应用锁、不是 SSH 终端时出现的
        let ids = vec![
            "toggle-host-tools",
            "toggle-multi-exec",
            "title-command-picker",
            "title-compose",
            "title-highlight",
            "title-toggle-theme",
        ];
        (handle, ids)
    }

    /// 排出来的按钮都在标题栏之内、在左边一组的右边、设置按钮的左边，设置按钮在标题栏之内；排出来的是设置中的前几个，
    /// 放不下时才有“更多”。返回排出来的个数。
    fn assert_toolbar_fits(
        cx: &mut TestAppContext,
        handle: AnyWindowHandle,
        ids: &[&str],
    ) -> usize {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let bar = window.find("title-bar").bounds();
            let left = window.find("local-terminal-menu").bounds();
            let settings = window.find("open-settings").bounds();
            assert!(
                settings.right() <= bar.right() + px(0.5),
                "设置按钮超出了标题栏：{settings:?}，标题栏 {bar:?}"
            );
            let shown: Vec<&str> = ids
                .iter()
                .copied()
                .take_while(|id| window.try_find(id.to_string()).is_some())
                .collect();
            assert!(
                ids[shown.len()..]
                    .iter()
                    .all(|id| window.try_find(id.to_string()).is_none()),
                "排出来的是前几个"
            );
            let more = window.try_find("title-toolbar-more").map(|m| m.bounds());
            for b in shown
                .iter()
                .map(|id| window.find(id.to_string()).bounds())
                .chain(more)
            {
                assert!(
                    b.left() >= left.right() && b.right() <= settings.left() + px(0.5),
                    "按钮挤出了工具栏：{b:?}，左边一组到 {:?}，设置在 {:?}",
                    left.right(),
                    settings.left()
                );
                assert!(b.top() >= bar.top() && b.bottom() <= bar.bottom() + px(0.5));
            }
            assert_eq!(
                more.is_some(),
                shown.len() < ids.len(),
                "放不下时才有“更多”"
            );
            shown.len()
        })
        .unwrap()
    }

    // 界面放大到 150%、窗口最窄（主窗口的最小宽度）时，工具栏的按钮都在标题栏之内，不压住左边的按钮
    #[gpui_kit::test]
    fn title_bar_fits_the_narrowest_window_at_150_percent(cx: &mut TestAppContext) {
        let (handle, ids) = crowded_title_bar(cx, crate::window_state::MIN_SIZE.0);
        assert_toolbar_fits(cx, handle, &ids);
        set_language(Language::ZhCn);
    }

    // 再窄时放不下的按钮收进“更多”菜单，菜单最后是“自定义工具栏…”
    #[gpui_kit::test]
    fn buttons_that_do_not_fit_go_into_the_more_menu(cx: &mut TestAppContext) {
        let (handle, ids) = crowded_title_bar(cx, 480.);
        let count = assert_toolbar_fits(cx, handle, &ids);
        assert!(count < ids.len(), "480 宽放不下全部");
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("title-toolbar-more", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            // 收起的按钮、分隔线、自定义工具栏…
            window.within("popup-menu").click(ids.len() - count + 1, cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("settings-toolbar").is_some());
        })
        .unwrap();
        set_language(Language::ZhCn);
    }
}
