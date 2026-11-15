//! 终端 Tab 的命令补全提示（怎样认出提示符、画在哪里、怎样接受见 [`termlet_terminal::suggest`]）：把这台主机
//! （本地终端按 shell）在本机加密保存的命令历史交给终端视图，作为提示的来源。
//!
//! - 设置“终端”中开启（默认关），且命令历史开着、这个 Tab 记命令历史（被外部程序调起的临时连接不记）时才读取；
//!   关掉时从视图中拿走读出的历史。
//! - 历史在后台线程读取、解密。记下一条命令后、每个提示符开始时先比较历史的版本（[`history::revision`]，不用解密），
//!   变了才重新读：别的 Tab 记下的、在历史窗口与快捷命令选择器中删掉的、清空的随之更新。记录与读取排成一队按顺序
//!   执行，后读的不会被先读的盖掉，刚执行的命令下一个提示符就能提示。
//! - 只用本机的数据：命令历史不同步、不上传，命令行工具与 MCP 读不到，提示也不写进终端（不进录制、日志）。

use std::sync::Arc;

use gpui_kit::{Context, Task};
use termlet_core::history::{self, Revision};
use termlet_core::store::Store;
use termlet_terminal::CommandSuggestions;

use super::TerminalPanel;

/// 命令补全提示的状态，每个 Tab 一份。
#[derive(Default)]
pub(super) struct Suggest {
    /// 交给终端视图的历史是哪个版本；没有交给视图（关闭着、还没读完、换了新的视图）时为 `None`
    revision: Option<Revision>,
    /// 记录与读取的队尾：每一步等前一步做完
    lane: Option<Task<()>>,
}

impl TerminalPanel {
    /// 这个 Tab 现在要不要提示：设置开启、命令历史开着、记命令历史、终端视图已经建好。
    fn suggest_enabled(&self) -> bool {
        let terminal = self.store.settings().terminal;
        terminal.command_suggest
            && terminal.command_history
            && self.view.is_some()
            && !self.history_owner().is_empty()
    }

    /// 设置变了：按需开始提示，或从视图中拿走历史。
    pub(super) fn sync_suggestions(&mut self, cx: &mut Context<Self>) {
        if self.suggest_enabled() {
            if self.suggest.revision.is_none() {
                self.queue_history(None, cx);
            }
        } else if self.suggest.revision.take().is_some()
            && let Some(view) = &self.view
        {
            tracing::debug!(session = %self.name, "命令补全提示：关闭");
            view.update(cx, |view, cx| view.set_command_suggestions(None, cx));
        }
    }

    /// 终端视图新建好了：重新给它读历史。
    pub(super) fn reset_suggestions(&mut self, cx: &mut Context<Self>) {
        self.suggest.revision = None;
        self.sync_suggestions(cx);
    }

    /// 提示符开始：历史有变化（别的 Tab 记下的、删掉的）时重新读。
    pub(super) fn refresh_suggestions(&mut self, cx: &mut Context<Self>) {
        if self.suggest_enabled() {
            self.queue_history(None, cx);
        }
    }

    /// 记下一条执行的命令（加密后写进数据库，在后台线程）；提示开着时接着重新读历史。
    pub(super) fn record_history(&mut self, command: String, cx: &mut Context<Self>) {
        self.queue_history(Some(command), cx);
    }

    /// 先记下 `record`（有的话），再排进队尾：在提示开着时按版本重新读历史。
    fn queue_history(&mut self, record: Option<String>, cx: &mut Context<Self>) {
        let owner = self.history_owner();
        if owner.is_empty() {
            return;
        }
        let store = self.store.clone();
        // 记录不随 Tab 关闭而取消（例如执行 exit 后 Tab 随即关闭）：单独在后台写完，队里的读取等它写完
        let recorded = record.map(|command| {
            let (done, recorded) = async_channel::bounded::<()>(1);
            let (store, owner) = (store.clone(), owner.clone());
            cx.background_executor()
                .spawn(async move {
                    if let Err(e) = history::record(&store, &owner, &command) {
                        tracing::warn!("记录命令历史失败：{e}");
                    }
                    let _ = done.send(()).await;
                })
                .detach();
            recorded
        });
        let previous = self.suggest.lane.take();
        self.suggest.lane = Some(cx.spawn(async move |this, cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            if let Some(recorded) = recorded {
                let _ = recorded.recv().await;
            }
            // 读的时候才看现在交给视图的是哪个版本（前面排着的步骤可能刚换过）
            let Ok(Some(known)) = this.update(cx, |this, _| {
                this.suggest_enabled().then_some(this.suggest.revision)
            }) else {
                return;
            };
            let loaded = cx
                .background_executor()
                .spawn(async move { load(&store, &owner, known) })
                .await;
            if let Some((revision, suggestions)) = loaded {
                let _ = this.update(cx, |this, cx| {
                    this.apply_suggestions(revision, suggestions, cx)
                });
            }
        }));
    }

    fn apply_suggestions(
        &mut self,
        revision: Revision,
        suggestions: CommandSuggestions,
        cx: &mut Context<Self>,
    ) {
        // 读的过程中关掉了
        if !self.suggest_enabled() {
            return;
        }
        let Some(view) = &self.view else {
            return;
        };
        tracing::debug!(
            session = %self.name,
            commands = suggestions.len(),
            "命令补全提示：读取了命令历史"
        );
        self.suggest.revision = Some(revision);
        let suggestions = Arc::new(suggestions);
        view.update(cx, |view, cx| {
            view.set_command_suggestions(Some(suggestions), cx)
        });
    }
}

/// 读这台主机的历史（新的在前）；版本与 `known` 相同（没有变化）或读不出来时为 `None`。
fn load(
    store: &Store,
    owner: &str,
    known: Option<Revision>,
) -> Option<(Revision, CommandSuggestions)> {
    let revision = history::revision(store, owner)
        .inspect_err(|e| tracing::warn!("读取命令历史的版本失败：{e}"))
        .ok()?;
    if known == Some(revision) {
        return None;
    }
    let entries = history::list(store, owner)
        .inspect_err(|e| tracing::warn!("读取命令历史失败：{e}"))
        .ok()?;
    Some((
        revision,
        CommandSuggestions::new(entries.into_iter().map(|entry| entry.command)),
    ))
}

#[cfg(test)]
mod tests;
