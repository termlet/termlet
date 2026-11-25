//! 设置 Tab（[`SettingsPanel`]）在工作区中的打开、切换与关闭。一个窗口最多一个：已经开着时切过去并换到要的分类。
//! 标题栏的齿轮、命令面板、工具栏、菜单栏、托盘等打开设置的入口都走这里（经 [`crate::settings::open`]）。

use gpui_kit::component::dock::{DockPlacement, PanelId, panel_handle};
use gpui_kit::{Context, Entity, Focusable as _, Window};

use super::Workspace;
use crate::settings::Page;
use crate::settings::panel::{SettingsPanel, SettingsPanelEvent};

impl Workspace {
    /// 这个窗口的设置 Tab。
    pub(crate) fn live_settings(&self) -> Option<Entity<SettingsPanel>> {
        self.settings_tab.as_ref().and_then(|p| p.upgrade())
    }

    /// 打开设置，显示 `page` 分类：已经开着时切过去。保险库没有解锁、应用锁定时不打开。
    pub(crate) fn open_settings(
        &mut self,
        page: Page,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.vault_gate.is_some() || crate::lock_ui::is_locked(cx) {
            return;
        }
        tracing::debug!(page = ?page, "打开设置");
        if let Some(panel) = self.live_settings() {
            panel.update(cx, |panel, cx| panel.show_page(page, cx));
            self.show_settings(&panel, window, cx);
            return;
        }
        let panel = SettingsPanel::new(page, window, cx);
        self.watch_settings(&panel, window, cx);
        self.dock_area.update(cx, |dock, cx| {
            dock.add_panel_view(
                panel_handle(panel.clone()),
                DockPlacement::Center,
                None,
                window,
                cx,
            )
        });
        self.show_settings(&panel, window, cx);
        cx.notify();
    }

    /// 切到设置 Tab 并让它获得焦点（命令面板中选了它时也用）。
    pub(crate) fn show_settings(
        &mut self,
        panel: &Entity<SettingsPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel_id = PanelId::from(panel.entity_id());
        self.dock_area
            .update(cx, |dock, cx| dock.select_panel(panel_id, window, cx));
        self.set_sync_away(true, cx);
        panel.read(cx).focus_handle(cx).focus(window, cx);
    }

    pub(super) fn watch_settings(
        &mut self,
        panel: &Entity<SettingsPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings_tab = Some(panel.downgrade());
        let id = panel.entity_id();
        self.settings_watch = Some([
            cx.subscribe_in(
                panel,
                window,
                |this, panel, event: &SettingsPanelEvent, window, cx| match event {
                    SettingsPanelEvent::Close => {
                        // 关掉后别处可能还拿着面板（还没释放），不再算这个窗口的设置 Tab
                        this.settings_tab = None;
                        let panel = panel.clone();
                        this.dock_area
                            .update(cx, |dock, cx| dock.remove_panel(panel, window, cx));
                    }
                },
            ),
            cx.observe_release(panel, move |this, _, _| {
                if this
                    .settings_tab
                    .as_ref()
                    .is_some_and(|p| p.entity_id() == id)
                {
                    this.settings_tab = None;
                    this.settings_watch = None;
                }
            }),
        ]);
    }
}
