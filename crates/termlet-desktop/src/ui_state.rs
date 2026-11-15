//! 只与这台电脑有关的界面状态（分组的展开与折叠、侧边栏的宽度与显示、主窗口的位置）：读进内存，修改后稍等片刻在后台
//! 写回文件，退出时把还没写的立即写回（[`UiStateModel::flush`]）。

use std::time::Duration;

use gpui_kit::{Context, Task};
use termlet_core::ui_state::{UiState, UiStateFile};

/// 修改后等这么久再写文件（拖动侧边栏宽度时会连续修改）。
const SAVE_DELAY: Duration = Duration::from_millis(500);

pub struct UiStateModel {
    /// `None`：不落盘（UI 测试）
    file: Option<UiStateFile>,
    state: UiState,
    _save: Option<Task<()>>,
}

impl UiStateModel {
    pub fn load(file: Option<UiStateFile>) -> Self {
        let state = file.as_ref().map(UiStateFile::load).unwrap_or_default();
        Self {
            file,
            state,
            _save: None,
        }
    }

    pub fn state(&self) -> &UiState {
        &self.state
    }

    /// 立即写回还没写的修改（退出时用：等不到稍后的写回）。
    pub fn flush(&mut self) {
        if self._save.take().is_none() {
            return;
        }
        if let Some(file) = &self.file
            && let Err(e) = file.save(&self.state)
        {
            tracing::warn!("保存界面状态失败：{e}");
        }
    }

    /// 修改界面状态；有变化时通知观察者并安排保存。
    pub fn update(&mut self, edit: impl FnOnce(&mut UiState), cx: &mut Context<Self>) {
        let before = self.state.clone();
        edit(&mut self.state);
        if self.state == before {
            return;
        }
        cx.notify();
        let Some(file) = self.file.clone() else {
            return;
        };
        self._save = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            let Ok(state) = this.read_with(cx, |this, _| this.state.clone()) else {
                return;
            };
            let saved = cx
                .background_executor()
                .spawn(async move { file.save(&state) })
                .await;
            if let Err(e) = saved {
                tracing::warn!("保存界面状态失败：{e}");
            }
        }));
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{AppContext as _, TestAppContext};

    use super::*;

    /// 展开的分组一改就安排写回文件，重新读取（重启）后照旧。
    #[gpui_kit::test]
    fn expanded_groups_survive_a_restart(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!(
            "termlet-ui-state-{}",
            termlet_core::id::random_token(8)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = UiStateFile::new(dir.join("ui-state.json"));
        let model = cx.new(|_| UiStateModel::load(Some(file.clone())));
        model.update(cx, |model, cx| {
            model.update(
                |state| {
                    state.expanded_groups.insert("g_prod".into());
                },
                cx,
            )
        });
        cx.executor().advance_clock(SAVE_DELAY * 2);
        cx.run_until_parked();
        let restarted = UiStateModel::load(Some(file));
        assert!(restarted.state().expanded_groups.contains("g_prod"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 退出时等不到稍后的写回：`flush` 立即写，之后稍后的写回不再重复。
    #[gpui_kit::test]
    fn flush_writes_at_once(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!(
            "termlet-ui-state-{}",
            termlet_core::id::random_token(8)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = UiStateFile::new(dir.join("ui-state.json"));
        let model = cx.new(|_| UiStateModel::load(Some(file.clone())));
        model.update(cx, |model, cx| {
            model.update(
                |state| {
                    state.main_window = Some(termlet_core::ui_state::WindowPlacement {
                        width: 1000.,
                        height: 700.,
                        ..Default::default()
                    })
                },
                cx,
            );
            model.flush();
        });
        let written = UiStateModel::load(Some(file.clone()));
        assert_eq!(
            written.state().main_window.as_ref().map(|p| p.width),
            Some(1000.)
        );
        model.update(cx, |model, _| model.flush());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
