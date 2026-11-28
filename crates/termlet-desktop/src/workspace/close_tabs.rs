//! Tab 菜单的“关闭其他 Tab”“关闭右侧 Tab”“关闭所有 Tab”：范围是右键的那个 Tab 所在的窗格（Tab 分组），别的窗格
//! 不动；要说明的汇总起来只确认一次（[`crate::close_confirm::confirm_tabs`]）。

use gpui_kit::component::dock::{DockPlacement, PaneRef, PanelId};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::{App, ClickEvent, Context, Entity, EntityId, Window};

use super::Workspace;
use crate::t;

/// Tab 菜单中关闭几个 Tab 的范围（都在右键的那个 Tab 所在的窗格中）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CloseTabs {
    /// 关闭其他 Tab：留下右键的那个
    Others,
    /// 关闭右侧 Tab：右键的那个右边的
    ToTheRight,
    /// 关闭所有 Tab
    All,
}

impl Workspace {
    /// `panel` 所在的 Tab 分组（同一个窗格中的 Tab，按 Tab 栏上的顺序）；不在 Dock 中时为空。
    pub(crate) fn tab_group_of(&self, panel: PanelId, cx: &App) -> Vec<PanelId> {
        self.tab_group_with_active(panel, cx)
            .map(|(panels, _)| panels)
            .unwrap_or_default()
    }

    /// `panel` 所在的 Tab 分组与其中当前显示的 Tab 的位置。
    fn tab_group_with_active(&self, panel: PanelId, cx: &App) -> Option<(Vec<PanelId>, usize)> {
        let tree = self.dock_area.read(cx).layout(DockPlacement::Center)?;
        let node = tree
            .find_panel_node(panel)
            .and_then(|node| tree.find_node(node))?;
        match node.kind() {
            PaneRef::Tabs { panels, active_ix } => Some((panels.to_vec(), active_ix)),
            _ => None,
        }
    }

    /// Tab 菜单的“关闭其他 Tab”“关闭右侧 Tab”“关闭所有 Tab”（见 [`CloseTabs`]）：只关 `panel` 所在窗格中的 Tab，
    /// 别的窗格不动（与 VS Code 按编辑器组一致）。关闭其他时切到右键的那个；关闭右侧时，当前显示的 Tab 被关掉了
    /// 才切到右键的那个。要说明的（连接着的终端、文件管理按设置，未完成的传输、未保存的文件总要说明）汇总起来只确认
    /// 一次，确认后一起关闭，取消时一个也不关；没有要说明的直接关闭。
    pub(crate) fn close_tabs(
        &mut self,
        panel: PanelId,
        which: CloseTabs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((group, active)) = self.tab_group_with_active(panel, cx) else {
            return;
        };
        let Some(ix) = group.iter().position(|id| *id == panel) else {
            return;
        };
        let (targets, keep): (Vec<PanelId>, Option<PanelId>) = match which {
            CloseTabs::Others => (
                group.iter().copied().filter(|id| *id != panel).collect(),
                Some(panel),
            ),
            CloseTabs::ToTheRight => (group[ix + 1..].to_vec(), (active > ix).then_some(panel)),
            CloseTabs::All => (group, None),
        };
        if targets.is_empty() {
            return;
        }
        let summary = self.close_summary_of(|id| targets.contains(&id), cx);
        if !summary.needs_confirm() {
            self.close_panels(&targets, keep, window, cx);
            return;
        }
        tracing::info!(count = targets.len(), ?summary, "关闭几个 Tab 前确认");
        let this = cx.entity().downgrade();
        crate::close_confirm::confirm_tabs(
            targets.len(),
            summary.description(crate::close_confirm::CloseScope::Tabs),
            move |window, cx| {
                let _ = this.update(cx, |this, cx| this.close_panels(&targets, keep, window, cx));
            },
            window,
            cx,
        );
    }

    /// 关闭这些 Tab，不再逐个确认（文件管理照样先取消未完成的传输）；`keep` 是留下的 Tab，先切到它并让它获得焦点。
    fn close_panels(
        &mut self,
        panels: &[PanelId],
        keep: Option<PanelId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::info!(count = panels.len(), "一次关闭几个 Tab");
        if let Some(keep) = keep {
            self.select_tab_panel(keep, window, cx);
        }
        let has = |id: EntityId| panels.contains(&PanelId::from(id));
        for panel in self.live_terminals() {
            if has(panel.entity_id()) {
                panel.update(cx, |panel, cx| panel.close_tab(cx));
            }
        }
        for panel in self.live_sftps() {
            if has(panel.entity_id()) {
                panel.update(cx, |panel, cx| panel.close_tab(cx));
            }
        }
        for editor in self.live_editors() {
            if has(editor.entity_id()) {
                editor.update(cx, |editor, cx| editor.close_tab(cx));
            }
        }
        for logs in self.live_logs() {
            if has(logs.entity_id()) {
                logs.update(cx, |logs, cx| logs.close_tab(cx));
            }
        }
        for replay in self.live_replays() {
            if has(replay.entity_id()) {
                replay.update(cx, |replay, cx| replay.close_tab(cx));
            }
        }
        for batch in self.live_batches() {
            if has(batch.entity_id()) {
                batch.update(cx, |batch, cx| batch.close_tab(cx));
            }
        }
        if let Some(settings) = self.live_settings().filter(|p| has(p.entity_id())) {
            settings.update(cx, |settings, cx| settings.close_tab(cx));
        }
    }

    /// 关闭一个 Tab，与点它的 × 相同（各自的确认照旧）：鼠标中键单击 Tab（见 `dock_skin`）。不是终端、文件管理、
    /// 编辑器、日志查看器、回放、批量执行、设置的 Tab 时什么也不做。
    pub(crate) fn request_close_tab(
        &mut self,
        panel: PanelId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is = |id: EntityId| PanelId::from(id) == panel;
        if let Some(terminal) = self
            .live_terminals()
            .into_iter()
            .find(|p| is(p.entity_id()))
        {
            terminal.update(cx, |terminal, cx| terminal.request_close(window, cx));
        } else if let Some(sftp) = self.live_sftps().into_iter().find(|p| is(p.entity_id())) {
            sftp.update(cx, |sftp, cx| sftp.request_close(window, cx));
        } else if let Some(editor) = self.live_editors().into_iter().find(|e| is(e.entity_id())) {
            editor.update(cx, |editor, cx| editor.request_close(window, cx));
        } else if let Some(logs) = self.live_logs().into_iter().find(|p| is(p.entity_id())) {
            logs.update(cx, |logs, cx| logs.close_tab(cx));
        } else if let Some(replay) = self.live_replays().into_iter().find(|p| is(p.entity_id())) {
            replay.update(cx, |replay, cx| replay.close_tab(cx));
        } else if let Some(batch) = self.live_batches().into_iter().find(|p| is(p.entity_id())) {
            batch.update(cx, |batch, cx| batch.request_close(window, cx));
        } else if let Some(settings) = self.live_settings().filter(|p| is(p.entity_id())) {
            settings.update(cx, |settings, cx| settings.request_close(window, cx));
        }
    }
}

/// Tab 菜单中“关闭”后面的“关闭其他 Tab”“关闭右侧 Tab”（Tab 竖排时为“关闭下方 Tab”）“关闭所有 Tab”（[`Workspace::close_tabs`]），以及“移到新窗口”
/// “移到窗口 ›”（[`super::move_tab::move_tab_items`]），文件管理、编辑器、日志查看器、回放等共用；窗格中只有这一个
/// Tab 时“关闭其他 Tab”不可用，它是最右边一个时“关闭右侧 Tab”不可用。终端的 Tab 菜单把移到窗口收在“分屏与窗口 ›”中，
/// 用 [`close_tab_items_without_move`]。
pub(crate) fn close_tab_items(
    menu: PopupMenu,
    panel: EntityId,
    window: &mut Window,
    cx: &mut App,
) -> PopupMenu {
    let Some(workspace) = Workspace::in_window(window, cx) else {
        return menu;
    };
    let panel = PanelId::from(panel);
    let menu = close_some_items(menu, &workspace, panel, cx);
    super::move_tab::move_tab_items(menu, &workspace, panel, window, cx)
}

/// 同 [`close_tab_items`]，只有关闭的三项，不带移到窗口。
pub(crate) fn close_tab_items_without_move(
    menu: PopupMenu,
    panel: EntityId,
    window: &mut Window,
    cx: &mut App,
) -> PopupMenu {
    match Workspace::in_window(window, cx) {
        Some(workspace) => close_some_items(menu, &workspace, PanelId::from(panel), cx),
        None => menu,
    }
}

/// “关闭其他 Tab”“关闭右侧 Tab”“关闭所有 Tab”。
fn close_some_items(
    menu: PopupMenu,
    workspace: &Entity<Workspace>,
    panel: PanelId,
    cx: &mut App,
) -> PopupMenu {
    let group = workspace.read(cx).tab_group_of(panel, cx);
    let is_last = group.last() == Some(&panel);
    let close = |which: CloseTabs| {
        let workspace = workspace.downgrade();
        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            let _ = workspace.update(cx, |workspace, cx| {
                workspace.close_tabs(panel, which, window, cx)
            });
        }
    };
    menu.item(
        PopupMenuItem::new(t!("panel.close_others"))
            .disabled(group.len() < 2)
            .on_click(close(CloseTabs::Others)),
    )
    .item(
        // Tab 竖排在左侧时，排在后面的 Tab 在下方
        PopupMenuItem::new(if Workspace::tabs_on_left(cx) {
            t!("panel.close_below")
        } else {
            t!("panel.close_right")
        })
        .disabled(is_last)
        .on_click(close(CloseTabs::ToTheRight)),
    )
    .item(PopupMenuItem::new(t!("panel.close_all")).on_click(close(CloseTabs::All)))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gpui_kit::component::Placement;
    use gpui_kit::component::dock::{DockPlacement, InsertTarget, PanelId};
    use gpui_kit::test::{ClickOptions, TestWindowExt as _};
    use gpui_kit::{
        AnyWindowHandle, App, AppContext as _, ElementId, Entity, MouseButton, ScrollDelta,
        TestAppContext, point,
    };
    use termlet_core::model::Host;
    use termlet_core::store::Position;

    use crate::app_state::AppState;
    use crate::i18n::{self, Language};
    use crate::t;
    use crate::workspace::{Workspace, collect_tab_groups};

    fn open(cx: &mut TestAppContext, confirm_close: bool) -> (AnyWindowHandle, Entity<Workspace>) {
        i18n::set_language(Language::ZhCn);
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        cx.update(crate::init);
        cx.update(AppState::install_in_memory);
        cx.update(|cx| {
            AppState::store(cx)
                .update_settings(|s| s.general.confirm_close = confirm_close)
                .unwrap()
        });
        // 对话框的入场动画按真实时间推进，动画中点击会偶尔落空
        cx.update(|cx| cx.set_reduce_motion(true));
        cx.update(|cx| {
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| Workspace::new(window, cx))
            })
            .unwrap()
        })
    }

    fn wait(cx: &mut TestAppContext, what: &str, mut done: impl FnMut(&mut App) -> bool) {
        for _ in 0..400 {
            cx.run_until_parked();
            if cx.update(&mut done) {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("等待超时：{what}");
    }

    /// 打开 `n` 个本地终端（同一个窗格中），等它们都运行起来。
    fn open_terminals(
        cx: &mut TestAppContext,
        handle: AnyWindowHandle,
        workspace: &Entity<Workspace>,
        n: usize,
    ) -> Vec<PanelId> {
        for _ in 0..n {
            cx.update_window(handle, |_, window, cx| {
                workspace.update(cx, |w, cx| w.open_local_terminal(window, cx))
            })
            .unwrap();
        }
        wait(cx, "终端都启动", |cx| {
            let terminals = workspace.read(cx).live_terminals();
            terminals.len() == n && terminals.iter().all(|p| p.read(cx).is_live(cx))
        });
        cx.update(|cx| {
            workspace
                .read(cx)
                .live_terminals()
                .iter()
                .map(|p| PanelId::from(p.entity_id()))
                .collect()
        })
    }

    /// 各窗格中的 Tab。
    fn groups(cx: &mut TestAppContext, workspace: &Entity<Workspace>) -> Vec<Vec<PanelId>> {
        cx.update(|cx| {
            let dock = workspace.read(cx).dock_area.read(cx);
            let mut groups = Vec::new();
            if let Some(tree) = dock.layout(DockPlacement::Center) {
                collect_tab_groups(tree.root(), &mut groups);
            }
            groups.into_iter().map(|(panels, _)| panels).collect()
        })
    }

    fn terminal_title(id: PanelId) -> ElementId {
        ("tab-title", id.as_u64() as usize).into()
    }

    /// 右键 Tab 标题，点菜单中的 `label`；返回这一项是否可用（可用的点了以后菜单关闭，不可用的点了没有反应，
    /// 再按 Esc 关掉菜单）。
    fn tab_menu(
        cx: &mut TestAppContext,
        handle: AnyWindowHandle,
        title: ElementId,
        label: &str,
    ) -> bool {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.right_click(title, cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let mut menu = window.within("popup-menu");
            let ix = (0..60usize)
                .find(|i| {
                    menu.try_find(*i)
                        .is_some_and(|item| item.label() == Some(label))
                })
                .unwrap_or_else(|| panic!("Tab 菜单中没有“{label}”"));
            menu.click(ix, cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let open = window.try_find("popup-menu").is_some();
            if open {
                window.press("escape", cx);
            }
            !open
        })
        .unwrap()
    }

    // 关闭其他 Tab：只关同一个窗格中的，留下右键的那个并切到它；别的窗格不动
    #[gpui_kit::test]
    fn close_others_keeps_the_clicked_tab_and_other_panes(cx: &mut TestAppContext) {
        let (handle, workspace) = open(cx, false);
        let ids = open_terminals(cx, handle, &workspace, 4);
        // 第四个移到右边的窗格
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| {
                let node = w
                    .dock_area
                    .read(cx)
                    .layout(DockPlacement::Center)
                    .and_then(|tree| tree.find_panel_node(ids[0]))
                    .unwrap();
                let target = InsertTarget::Split {
                    node,
                    placement: Placement::Right,
                    size: None,
                };
                w.dock_area
                    .update(cx, |dock, cx| dock.move_panel(ids[3], target, window, cx));
            })
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(groups(cx, &workspace), [ids[..3].to_vec(), vec![ids[3]]]);

        let close_others = t!("panel.close_others").to_string();
        assert!(tab_menu(cx, handle, terminal_title(ids[1]), &close_others));
        assert_eq!(
            groups(cx, &workspace),
            [vec![ids[1]], vec![ids[3]]],
            "左边只留下右键的那个，右边的窗格不动"
        );
        cx.update_window(handle, |_, window, cx| {
            let focused = workspace.read(cx).focused_terminal(window, cx);
            assert_eq!(focused.map(|p| PanelId::from(p.entity_id())), Some(ids[1]));
        })
        .unwrap();
        assert!(
            !tab_menu(cx, handle, terminal_title(ids[1]), &close_others),
            "窗格中只剩一个 Tab 时不可用"
        );
    }

    // 关闭右侧 Tab：只关右键的那个右边的；当前显示的 Tab 被关掉时切到右键的那个，没被关掉时不切换；
    // 最右边一个时不可用
    #[gpui_kit::test]
    fn close_right_closes_only_the_tabs_after_the_clicked_one(cx: &mut TestAppContext) {
        let (handle, workspace) = open(cx, false);
        let ids = open_terminals(cx, handle, &workspace, 4);
        let close_right = t!("panel.close_right").to_string();
        assert!(
            !tab_menu(cx, handle, terminal_title(ids[3]), &close_right),
            "最右边一个时不可用"
        );
        // 当前显示的是最后打开的（ids[3]），在右键的 ids[1] 右边：关掉后切到 ids[1]
        assert!(tab_menu(cx, handle, terminal_title(ids[1]), &close_right));
        assert_eq!(groups(cx, &workspace), [ids[..2].to_vec()]);
        cx.update_window(handle, |_, window, cx| {
            let focused = workspace.read(cx).focused_terminal(window, cx);
            assert_eq!(focused.map(|p| PanelId::from(p.entity_id())), Some(ids[1]));
        })
        .unwrap();
        // 当前显示的在左边、没被关掉：不切换
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| w.select_tab_panel(ids[0], window, cx))
        })
        .unwrap();
        cx.run_until_parked();
        assert!(tab_menu(cx, handle, terminal_title(ids[0]), &close_right));
        assert_eq!(groups(cx, &workspace), [vec![ids[0]]]);
    }

    // 关闭所有 Tab：关掉这个窗格中的全部 Tab，别的窗格不动
    #[gpui_kit::test]
    fn close_all_closes_only_this_pane(cx: &mut TestAppContext) {
        let (handle, workspace) = open(cx, false);
        let ids = open_terminals(cx, handle, &workspace, 3);
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| {
                let node = w
                    .dock_area
                    .read(cx)
                    .layout(DockPlacement::Center)
                    .and_then(|tree| tree.find_panel_node(ids[0]))
                    .unwrap();
                let target = InsertTarget::Split {
                    node,
                    placement: Placement::Bottom,
                    size: None,
                };
                w.dock_area
                    .update(cx, |dock, cx| dock.move_panel(ids[2], target, window, cx));
            })
        })
        .unwrap();
        cx.run_until_parked();
        let close_all = t!("panel.close_all").to_string();
        assert!(tab_menu(cx, handle, terminal_title(ids[0]), &close_all));
        assert_eq!(groups(cx, &workspace), [vec![ids[2]]]);
        assert!(tab_menu(cx, handle, terminal_title(ids[2]), &close_all));
        assert!(groups(cx, &workspace).is_empty(), "最后一个窗格也关掉");
    }

    // 有运行中的本地终端时汇总确认一次：取消后一个也不关，确认后一起关闭，不再逐个询问
    #[gpui_kit::test]
    fn closing_several_tabs_confirms_once(cx: &mut TestAppContext) {
        let (handle, workspace) = open(cx, true);
        let ids = open_terminals(cx, handle, &workspace, 3);
        let close_all = t!("panel.close_all").to_string();
        assert!(tab_menu(cx, handle, terminal_title(ids[0]), &close_all));
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find_all("close-confirm").len(), 1, "只问一次");
            window.click("close-cancel", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(
            groups(cx, &workspace),
            vec![ids.clone()],
            "取消后一个也不关"
        );

        assert!(tab_menu(cx, handle, terminal_title(ids[0]), &close_all));
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("close-confirm", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert!(groups(cx, &workspace).is_empty(), "确认后一起关闭");
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("close-confirm").is_none(), "不再逐个询问");
        })
        .unwrap();
    }

    // 文件管理的 Tab 菜单也有这两项，与终端放在同一个窗格中时一起算
    #[gpui_kit::test]
    fn sftp_tabs_have_the_same_items(cx: &mut TestAppContext) {
        let (handle, workspace) = open(cx, false);
        let ids = open_terminals(cx, handle, &workspace, 1);
        // 连不上的主机：打开很快失败，不影响菜单
        let host = cx.update(|cx| {
            AppState::store(cx)
                .create(
                    Host {
                        name: "web".into(),
                        address: "127.0.0.1".into(),
                        port: 1,
                        username: "alice".into(),
                        ..Default::default()
                    },
                    None,
                    Position::Last,
                )
                .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| w.open_host_sftp(&host, window, cx))
        })
        .unwrap();
        cx.run_until_parked();
        let sftp = cx.update(|cx| PanelId::from(workspace.read(cx).live_sftps()[0].entity_id()));
        assert_eq!(groups(cx, &workspace), [vec![ids[0], sftp]]);
        let sftp_title: ElementId = ("sftp-tab-title", sftp.as_u64() as usize).into();
        let close_others = t!("panel.close_others").to_string();
        assert!(tab_menu(cx, handle, sftp_title.clone(), &close_others));
        assert_eq!(
            groups(cx, &workspace),
            [vec![sftp]],
            "终端关掉，文件管理留下"
        );
        let close_all = t!("panel.close_all").to_string();
        assert!(tab_menu(cx, handle, sftp_title, &close_all));
        assert!(groups(cx, &workspace).is_empty());
    }

    /// 鼠标中键单击 `title`（Tab 上的标题，在 Tab 之内）。
    fn middle_click(cx: &mut TestAppContext, handle: AnyWindowHandle, title: ElementId) {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click_with_options(
                title,
                ClickOptions::new().with_button(MouseButton::Middle),
                cx,
            );
        })
        .unwrap();
        cx.run_until_parked();
    }

    // 鼠标中键单击 Tab：与点它的 × 相同，运行中的本地终端按设置先确认（取消则不关）；文件管理的 Tab 也一样；
    // 右键照旧打开 Tab 菜单、不关闭
    #[gpui_kit::test]
    fn middle_click_closes_a_tab_like_its_close_button(cx: &mut TestAppContext) {
        let (handle, workspace) = open(cx, true);
        let ids = open_terminals(cx, handle, &workspace, 2);
        middle_click(cx, handle, terminal_title(ids[0]));
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("close-confirm").is_some(), "先确认");
            window.click("close-cancel", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(groups(cx, &workspace), vec![ids.clone()], "取消则不关");
        middle_click(cx, handle, terminal_title(ids[0]));
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("close-confirm", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(groups(cx, &workspace), [vec![ids[1]]]);

        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.right_click(terminal_title(ids[1]), cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("popup-menu").is_some(), "右键打开菜单");
            window.press("escape", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(groups(cx, &workspace), [vec![ids[1]]], "右键不关闭");

        // 文件管理的 Tab（连不上的主机）：关掉确认后中键直接关闭
        cx.update(|cx| {
            AppState::store(cx)
                .update_settings(|s| s.general.confirm_close = false)
                .unwrap()
        });
        let host = cx.update(|cx| {
            AppState::store(cx)
                .create(
                    Host {
                        name: "web".into(),
                        address: "127.0.0.1".into(),
                        port: 1,
                        username: "alice".into(),
                        ..Default::default()
                    },
                    None,
                    Position::Last,
                )
                .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| w.open_host_sftp(&host, window, cx))
        })
        .unwrap();
        cx.run_until_parked();
        let sftp = cx.update(|cx| PanelId::from(workspace.read(cx).live_sftps()[0].entity_id()));
        assert_eq!(groups(cx, &workspace), [vec![ids[1], sftp]]);
        middle_click(
            cx,
            handle,
            ("sftp-tab-title", sftp.as_u64() as usize).into(),
        );
        assert_eq!(groups(cx, &workspace), [vec![ids[1]]]);
    }

    // Tab 放不下时在 Tab 栏上滚动鼠标滚轮：左右滚动 Tab 栏（gpui-kit 的 TabBar 把滚轮的竖向滚动换成横向）
    #[gpui_kit::test]
    fn wheel_scrolls_the_tab_bar_sideways(cx: &mut TestAppContext) {
        let (handle, workspace) = open(cx, false);
        let ids = open_terminals(cx, handle, &workspace, 10);
        // 窗口缩到最小尺寸：Tab 名称较短的系统上（Linux 的本地 shell）10 个也一定放不下
        cx.simulate_window_resize(
            handle,
            gpui_kit::size(gpui_kit::px(720.), gpui_kit::px(480.)),
        );
        cx.run_until_parked();
        let last = *ids.last().unwrap();
        // 先切到第一个再切回最后一个：最后一个滚进视野，Tab 栏在最右
        for panel in [ids[0], last] {
            cx.update_window(handle, |_, window, cx| {
                workspace.update(cx, |w, cx| w.select_tab_panel(panel, window, cx));
                window.render_frame(cx);
            })
            .unwrap();
            cx.run_until_parked();
        }
        // 滚轮落在倒数第三个 Tab 上：滚动后它仍在 Tab 栏的可见部分（最后一个会被移出右边）
        let target = ids[ids.len() - 3];
        let x = |cx: &mut TestAppContext| {
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.find(terminal_title(last)).bounds().origin.x
            })
            .unwrap()
        };
        let wheel = |cx: &mut TestAppContext, lines: f32| {
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.scroll(
                    terminal_title(target),
                    ScrollDelta::Lines(point(0., lines)),
                    cx,
                );
            })
            .unwrap();
            cx.run_until_parked();
        };
        // 布局会晚一些才定下：标题栏的工具栏按量出的宽度收起按钮，本地 shell 在后台线程上设置窗口标题、改变 Tab 的宽度，
        // Tab 栏能滚动的范围随之变化。先等最后一个 Tab 的位置连续几次不变再量（最多约 2 秒）
        let mut stable = 0;
        let mut last_x = x(cx);
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            cx.run_until_parked();
            let now = x(cx);
            stable = if now == last_x { stable + 1 } else { 0 };
            last_x = now;
            if stable >= 5 {
                break;
            }
        }
        // 最后打开的 Tab 已滚进视野；滚轮向上往左滚，Tab 右移；向下往右滚，Tab 左移。只比方向、不要求滚回原处：
        // Linux 的 CI 上滚动之后 Tab 栏能滚的范围偶尔还会变（几十像素），滚回来停的位置跟着变
        let end = x(cx);
        wheel(cx, 3.);
        let moved = x(cx);
        assert!(moved > end, "向上滚：{end:?} → {moved:?}");
        wheel(cx, -3.);
        let back = x(cx);
        assert!(back < moved, "向下滚：{moved:?} → {back:?}");
    }
}
