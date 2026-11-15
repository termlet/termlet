//! 回放 Tab（[`ReplayPanel`]）在工作区中的打开、切换与关闭。同一个文件已打开时切过去；文件在后台读入，读不了时
//! 提示原因。命令面板的“打开录制文件…”、录制完成的提示中的“回放”、拖进窗口的 `.cast` 文件都走这里。

use std::path::{Path, PathBuf};

use gpui_kit::component::dock::{DockPlacement, PanelId, panel_handle};
use gpui_kit::{Context, Entity, Focusable as _, PathPromptOptions, Window};
use termlet_core::recording;

use super::Workspace;
use crate::replay::{self, ReplayPanel, ReplayPanelEvent};
use crate::t;

impl Workspace {
    pub(crate) fn live_replays(&self) -> Vec<Entity<ReplayPanel>> {
        self.replays.iter().filter_map(|p| p.upgrade()).collect()
    }

    /// 选一个录制文件回放（命令面板）。
    pub(crate) fn pick_recording(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                for path in paths {
                    this.open_replay(path, window, cx);
                }
            });
        })
        .detach();
    }

    /// 拖进窗口的文件中的录制文件，各开一个回放 Tab；返回有没有录制文件。
    pub(crate) fn open_replay_files(
        &mut self,
        paths: &[PathBuf],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let casts: Vec<PathBuf> = paths
            .iter()
            .filter(|p| recording::is_cast_path(p))
            .cloned()
            .collect();
        for path in &casts {
            self.open_replay(path.clone(), window, cx);
        }
        !casts.is_empty()
    }

    /// 在中间区域打开录制文件的回放：同一个文件已打开时切过去，否则在后台读入后新开一个 Tab。
    pub(crate) fn open_replay(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.vault_gate.is_some() {
            return;
        }
        if let Some(panel) = self
            .live_replays()
            .into_iter()
            .find(|p| same_file(p.read(cx).path(), &path))
        {
            self.show_replay(&panel, window, cx);
            return;
        }
        tracing::info!("打开录制文件回放");
        let load = replay::load(path.clone(), cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = load.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(cast) => {
                    let panel = ReplayPanel::new(path, cast, window, cx);
                    this.watch_replay(&panel, window, cx);
                    this.dock_area.update(cx, |dock, cx| {
                        dock.add_panel_view(
                            panel_handle(panel.clone()),
                            DockPlacement::Center,
                            None,
                            window,
                            cx,
                        )
                    });
                    this.show_replay(&panel, window, cx);
                    cx.notify();
                }
                Err(e) => {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    crate::notify::error(
                        t!(
                            "replay.open_failed",
                            name = name,
                            error = replay::describe_error(&e)
                        ),
                        window,
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    /// 切到这个回放的 Tab 并让它获得焦点（命令面板中选了它时也用）。
    pub(crate) fn show_replay(
        &mut self,
        panel: &Entity<ReplayPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel_id = PanelId::from(panel.entity_id());
        self.dock_area
            .update(cx, |dock, cx| dock.select_panel(panel_id, window, cx));
        self.set_sync_away(true, cx);
        panel.read(cx).focus_handle(cx).focus(window, cx);
    }

    pub(super) fn watch_replay(
        &mut self,
        panel: &Entity<ReplayPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replays.retain(|p| p.upgrade().is_some());
        self.replays.push(panel.downgrade());
        let id = panel.entity_id();
        let watch = [
            cx.subscribe_in(
                panel,
                window,
                |this, panel, event: &ReplayPanelEvent, window, cx| match event {
                    ReplayPanelEvent::Close => {
                        let panel = panel.clone();
                        this.dock_area
                            .update(cx, |dock, cx| dock.remove_panel(panel, window, cx));
                    }
                },
            ),
            cx.observe_release(panel, move |this, _, _| {
                this.replay_watch.remove(&id);
            }),
        ];
        self.replay_watch.insert(id, watch);
    }
}

/// 两个路径是不是同一个文件（能解析时按解析后的路径比）。
fn same_file(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(a), Ok(b)) if a == b
        )
}
