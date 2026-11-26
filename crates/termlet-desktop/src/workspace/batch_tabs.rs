//! 批量执行 Tab（[`BatchPanel`]）在工作区中的打开、切换与关闭：命令面板的“批量执行…”与工具栏的“批量执行”按钮（先选
//! 主机，可以按分组勾选）、撰写栏的“执行并对比”（目标是撰写栏选中的终端，立即执行；这个窗口里已有目标是终端、没在执行的
//! 批量执行 Tab 时在其中再执行一次，不再新开）。

use gpui_kit::component::dock::{DockPlacement, PanelId, panel_handle};
use gpui_kit::{Context, Entity, Focusable as _, Window};

use super::Workspace;
use crate::batch_run::{BatchPanel, BatchPanelEvent, TargetKind};
use crate::terminal_panel::TerminalPanel;

impl Workspace {
    pub(crate) fn live_batches(&self) -> Vec<Entity<BatchPanel>> {
        self.batches.iter().filter_map(|p| p.upgrade()).collect()
    }

    /// 命令面板的“批量执行…”、工具栏的“批量执行”：新开一个，先选主机。
    pub(crate) fn open_batch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.vault_gate.is_some() {
            return;
        }
        tracing::info!("打开批量执行");
        let panel = BatchPanel::for_hosts(Vec::new(), window, cx);
        self.add_batch(&panel, window, cx);
        panel.update(cx, |panel, cx| panel.pick_hosts(window, cx));
    }

    /// 测试用：新开一个，目标是这几台主机（不经过“选择主机”）。
    #[cfg(test)]
    pub(crate) fn open_batch_for_hosts(
        &mut self,
        hosts: Vec<termlet_core::id::HostId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel = BatchPanel::for_hosts(hosts, window, cx);
        self.add_batch(&panel, window, cx);
    }

    /// 撰写栏的“执行并对比”：在这几个终端里执行 `command`，结果在批量执行 Tab 中。
    pub(crate) fn run_batch_in_terminals(
        &mut self,
        terminals: Vec<Entity<TerminalPanel>>,
        command: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let terminals = terminals.iter().map(|t| t.downgrade()).collect();
        let idle = self.live_batches().into_iter().find(|p| {
            let p = p.read(cx);
            p.kind() == TargetKind::Terminals && !p.is_running()
        });
        let panel = match idle {
            Some(panel) => {
                self.show_batch(&panel, window, cx);
                panel
            }
            None => {
                let panel = BatchPanel::for_terminals(window, cx);
                self.add_batch(&panel, window, cx);
                panel
            }
        };
        panel.update(cx, |panel, cx| {
            panel.run_in_terminals(terminals, command, window, cx)
        });
    }

    fn add_batch(
        &mut self,
        panel: &Entity<BatchPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.watch_batch(panel, window, cx);
        self.dock_area.update(cx, |dock, cx| {
            dock.add_panel_view(
                panel_handle(panel.clone()),
                DockPlacement::Center,
                None,
                window,
                cx,
            )
        });
        self.show_batch(panel, window, cx);
        cx.notify();
    }

    /// 切到这个批量执行的 Tab 并让它获得焦点（命令面板中选了它时也用）。
    pub(crate) fn show_batch(
        &mut self,
        panel: &Entity<BatchPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel_id = PanelId::from(panel.entity_id());
        self.dock_area
            .update(cx, |dock, cx| dock.select_panel(panel_id, window, cx));
        self.set_sync_away(true, cx);
        panel.read(cx).focus_handle(cx).focus(window, cx);
    }

    pub(super) fn watch_batch(
        &mut self,
        panel: &Entity<BatchPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.batches.retain(|p| p.upgrade().is_some());
        self.batches.push(panel.downgrade());
        let id = panel.entity_id();
        let watch = [
            cx.subscribe_in(
                panel,
                window,
                |this, panel, event: &BatchPanelEvent, window, cx| match event {
                    BatchPanelEvent::Close => {
                        let panel = panel.clone();
                        this.dock_area
                            .update(cx, |dock, cx| dock.remove_panel(panel, window, cx));
                    }
                },
            ),
            cx.observe_release(panel, move |this, _, _| {
                this.batch_watch.remove(&id);
            }),
        ];
        self.batch_watch.insert(id, watch);
    }
}
