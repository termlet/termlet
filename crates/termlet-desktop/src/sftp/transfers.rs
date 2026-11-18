//! SFTP 面板底部的传输列表：这个窗口里这台主机的传输，最新的在上。
//!
//! - 进行中的显示进度与速度，可以取消；失败的可以继续（有临时文件时从中断处接着传）或重试、移除；
//!   因断线中断、正在自动重连的显示“等待重连后继续”。下载完成的可以打开所在文件夹。
//! - 头部：“完成后自动清除”（与设置联动）、“全部继续”、“清除已完成”。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, Context, EventEmitter, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, Task, Window, div,
    prelude::FluentBuilder as _, relative, rems,
};
use termlet_core::sftp::{
    ClearFilter, Direction, ResumeOptions, SessionId, SftpError, SftpManager, TransferId,
    TransferState, TransferView,
};

use super::{errors, list_model};
use crate::app_state::AppState;
use crate::prose::prose;
use crate::t;

/// 列表变化（面板据此刷新完成上传、下载的目录）。
pub enum TransferListEvent {
    /// 一个任务成功完成（每个任务只发一次）
    Finished(TransferView),
}

pub struct TransferList {
    manager: SftpManager,
    owner: String,
    host: String,
    /// 这台主机的任务，最新的在前
    items: Vec<TransferView>,
    /// 继续、重试时使用的会话（重连后是新的会话）
    session: Option<SessionId>,
    /// 会话正在重连（不能继续、移除）
    reconnecting: bool,
    /// 已报告过完成的任务
    finished: std::collections::HashSet<TransferId>,
    _watch: Task<()>,
}

impl EventEmitter<TransferListEvent> for TransferList {}

impl TransferList {
    pub fn new(owner: String, host: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let manager = AppState::sftp(cx);
        let mut changes = manager.watch_transfers();
        let watch = cx.spawn_in(window, async move |this, cx| {
            loop {
                if this.update(cx, |this, cx| this.reload(cx)).is_err() {
                    return;
                }
                if changes.changed().await.is_err() {
                    return;
                }
            }
        });
        Self {
            manager,
            owner,
            host,
            items: Vec::new(),
            session: None,
            reconnecting: false,
            finished: Default::default(),
            _watch: watch,
        }
    }

    /// 换一个所属窗口（文件管理的 Tab 移到别的窗口，会话与它的传输已交给那个窗口）：列出那个窗口的记录。
    pub fn set_owner(&mut self, owner: String, cx: &mut Context<Self>) {
        self.owner = owner;
        self.reload(cx);
    }

    pub fn set_session(
        &mut self,
        session: Option<SessionId>,
        reconnecting: bool,
        cx: &mut Context<Self>,
    ) {
        self.session = session;
        self.reconnecting = reconnecting;
        cx.notify();
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 这个会话上未完成（排队、传输中、等待重连）的任务。列表中同一台主机别的 SFTP Tab 的任务不算：
    /// 关闭这个 Tab 不影响它们。
    pub fn pending_on(&self, session: &SessionId) -> Vec<TransferId> {
        self.items
            .iter()
            .filter(|t| t.state.is_pending() && &t.session == session)
            .map(|t| t.id.clone())
            .collect()
    }

    /// 重新读取（经管理器读取，自动清除的计时从窗口读到结束状态时开始）。
    fn reload(&mut self, cx: &mut Context<Self>) {
        let mut items: Vec<TransferView> = self
            .manager
            .transfers(Some(&self.owner))
            .into_iter()
            .filter(|t| t.host == self.host)
            .collect();
        items.reverse();
        for item in &items {
            if item.state == TransferState::Done && self.finished.insert(item.id.clone()) {
                cx.emit(TransferListEvent::Finished(item.clone()));
            }
        }
        self.items = items;
        cx.notify();
    }

    /// 失败、可以继续的任务（等待重连的会自动继续，不算）。
    fn resumable(&self) -> Vec<&TransferView> {
        self.items
            .iter()
            .filter(|t| t.state == TransferState::Failed && !needs_overwrite(t))
            .collect()
    }

    fn resume(
        &mut self,
        id: &TransferId,
        overwrite: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let options = ResumeOptions {
            session: self.session.clone(),
            overwrite,
        };
        tracing::info!(transfer = %id, overwrite, "继续传输");
        if let Err(error) = self.manager.resume(id, options) {
            crate::notify::error(errors::describe(&error), window, cx);
        }
    }

    fn resume_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ids: Vec<TransferId> = self.resumable().iter().map(|t| t.id.clone()).collect();
        tracing::info!(count = ids.len(), "全部继续传输");
        let mut first_error: Option<SftpError> = None;
        for id in ids {
            let options = ResumeOptions {
                session: self.session.clone(),
                overwrite: false,
            };
            if let Err(error) = self.manager.resume(&id, options) {
                first_error.get_or_insert(error);
            }
        }
        if let Some(error) = first_error {
            crate::notify::error(errors::describe(&error), window, cx);
        }
    }

    /// 继续或重试：上传因远程已存在而失败的，先确认覆盖。
    fn confirm_resume(&mut self, item: &TransferView, window: &mut Window, cx: &mut Context<Self>) {
        if !needs_overwrite(item) {
            self.resume(&item.id, false, window, cx);
            return;
        }
        let list = cx.entity().downgrade();
        let id = item.id.clone();
        let remote = item.remote.clone();
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let (list, id) = (list.clone(), id.clone());
            alert
                .confirm()
                .title(t!("sftp.transfer.overwrite_title"))
                .description(prose(t!(
                    "sftp.transfer.overwrite_detail",
                    name = remote.clone()
                )))
                .ok_text(t!("sftp.transfer.overwrite_confirm"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, window, cx| {
                    let _ = list.update(cx, |this, cx| this.resume(&id, true, window, cx));
                    true
                })
        });
    }

    fn remove(&mut self, id: TransferId, cx: &mut Context<Self>) {
        tracing::debug!(transfer = %id, "移除传输记录");
        let manager = self.manager.clone();
        let filter = ClearFilter {
            owner: Some(self.owner.clone()),
            id: Some(id),
            ..Default::default()
        };
        termlet_core::runtime::spawn(async move { manager.clear(filter).await });
        cx.notify();
    }

    /// 移除失败的记录：留有续传的临时文件时先确认（一并删除，之后只能从头传）。
    fn confirm_remove(&mut self, item: &TransferView, window: &mut Window, cx: &mut Context<Self>) {
        if !item.resumable {
            self.remove(item.id.clone(), cx);
            return;
        }
        let list = cx.entity().downgrade();
        let id = item.id.clone();
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let (list, id) = (list.clone(), id.clone());
            alert
                .confirm()
                .title(t!("sftp.transfer.remove_title"))
                .description(prose(t!("sftp.transfer.remove_resumable")))
                .ok_text(t!("sftp.transfer.remove"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, _, cx| {
                    let _ = list.update(cx, |this, cx| this.remove(id.clone(), cx));
                    true
                })
        });
    }

    fn clear_finished(&mut self, cx: &mut Context<Self>) {
        tracing::debug!(host = %self.host, "清除已完成的传输记录");
        let manager = self.manager.clone();
        let filter = ClearFilter {
            owner: Some(self.owner.clone()),
            host: Some(self.host.clone()),
            states: vec![TransferState::Done, TransferState::Canceled],
            id: None,
        };
        termlet_core::runtime::spawn(async move { manager.clear(filter).await });
        cx.notify();
    }

    fn set_auto_clear(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        tracing::info!(on, "传输完成后自动清除");
        let store = AppState::store(cx);
        if let Err(e) = store.update_settings(|s| s.terminal.transfer_auto_clear = on) {
            crate::notify::error(
                t!("settings.save_failed", error = e.to_string()),
                window,
                cx,
            );
        }
        // 打开后，已完成的记录也开始计时清除
        self.reload(cx);
    }

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let auto_clear = AppState::store(cx).settings().terminal.transfer_auto_clear;
        let can_resume_all =
            self.session.is_some() && !self.reconnecting && self.resumable().len() >= 2;
        let has_finished = self
            .items
            .iter()
            .any(|t| matches!(t.state, TransferState::Done | TransferState::Canceled));
        h_flex()
            .flex_none()
            .min_h(rems(1.5))
            .px_3()
            .gap_3()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(div().flex_1().child(t!("sftp.transfer.title")))
            .child(
                Checkbox::new("transfer-auto-clear")
                    .small()
                    .label(t!("sftp.transfer.auto_clear"))
                    .tooltip(t!("sftp.transfer.auto_clear_hint"))
                    .checked(auto_clear)
                    .on_click(cx.listener(|this, checked: &bool, window, cx| {
                        this.set_auto_clear(*checked, window, cx)
                    })),
            )
            .when(can_resume_all, |this| {
                this.child(
                    Button::new("transfer-resume-all")
                        .ghost()
                        .xsmall()
                        .icon(IconName::RotateCw)
                        .label(t!("sftp.transfer.resume_all"))
                        .on_click(cx.listener(|this, _, window, cx| this.resume_all(window, cx))),
                )
            })
            .when(has_finished, |this| {
                this.child(
                    Button::new("transfer-clear-finished")
                        .ghost()
                        .xsmall()
                        .label(t!("sftp.transfer.clear_finished"))
                        .tooltip(t!("sftp.transfer.clear_finished_hint"))
                        .on_click(cx.listener(|this, _, _, cx| this.clear_finished(cx))),
                )
            })
            .into_any_element()
    }

    fn render_item(&self, item: &TransferView, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let waiting = item.state == TransferState::WaitingReconnect;
        let pending = matches!(item.state, TransferState::Queued | TransferState::Running);
        let failed = item.state == TransferState::Failed;
        let can_act = failed && !self.reconnecting;
        let id = item.id.as_str().to_string();
        let full_path: SharedString = match item.direction {
            Direction::Upload => item.remote.clone().into(),
            Direction::Download => item.local.to_string_lossy().to_string().into(),
        };
        let mut name = list_model::visible_name(&item.name);
        if item.is_dir && item.files > 0 {
            name = t!(
                "sftp.transfer.files",
                name = name,
                done = item.files_done,
                total = item.files
            )
            .to_string();
        }
        let status: SharedString = match item.state {
            TransferState::Running if item.total > 0 || item.done > 0 => t!(
                "sftp.transfer.progress",
                done = list_model::format_size(item.done),
                total = list_model::format_size(item.total),
                speed = list_model::format_size(item.speed)
            ),
            TransferState::Queued => t!("sftp.transfer.queued"),
            TransferState::Running => t!("sftp.transfer.running"),
            TransferState::WaitingReconnect => t!("sftp.transfer.waiting_reconnect"),
            TransferState::Done => t!("sftp.transfer.done"),
            TransferState::Failed => t!("sftp.transfer.failed"),
            TransferState::Canceled => t!("sftp.transfer.canceled"),
        };
        let fraction = if item.total == 0 {
            if item.state == TransferState::Done {
                1.
            } else {
                0.
            }
        } else {
            (item.done as f32 / item.total as f32).clamp(0., 1.)
        };
        let skipped = skipped_text(item);
        let (direction_icon, direction_color) = match item.direction {
            Direction::Upload => (IconName::ArrowUp, theme.success),
            Direction::Download => (IconName::ArrowDown, theme.info),
        };
        let action_item = item.clone();
        let remove_item = item.clone();
        let local = item.local.clone();
        v_flex()
            .id(SharedString::from(format!("transfer-{id}")))
            .px_3()
            .py_1()
            .gap_0p5()
            .text_xs()
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        Icon::new(direction_icon)
                            .xsmall()
                            .flex_none()
                            .text_color(direction_color),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("transfer-name-{id}")))
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(name)
                            .tooltip(move |window, cx| {
                                Tooltip::new(full_path.clone()).build(window, cx)
                            }),
                    )
                    .child(div().flex_none().text_color(muted).child(status))
                    .when(pending || waiting, |this| {
                        let id = action_item.id.clone();
                        this.child(
                            Button::new(SharedString::from(format!(
                                "transfer-cancel-{}",
                                id.as_str()
                            )))
                            .ghost()
                            .small()
                            .icon(IconName::Close)
                            .tooltip(t!("common.cancel"))
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    if let Err(error) = this.manager.cancel(&id) {
                                        crate::notify::error(errors::describe(&error), window, cx);
                                    }
                                },
                            )),
                        )
                    })
                    .when(can_act && self.session.is_some(), |this| {
                        let item = action_item.clone();
                        this.child(
                            Button::new(SharedString::from(format!(
                                "transfer-resume-{}",
                                item.id.as_str()
                            )))
                            .ghost()
                            .xsmall()
                            .icon(IconName::RotateCw)
                            .label(if item.resumable {
                                t!("sftp.transfer.resume")
                            } else {
                                t!("sftp.transfer.retry")
                            })
                            .on_click(cx.listener(
                                move |this, _, window, cx| this.confirm_resume(&item, window, cx),
                            )),
                        )
                    })
                    .when(can_act, |this| {
                        let item = remove_item.clone();
                        this.child(
                            Button::new(SharedString::from(format!(
                                "transfer-remove-{}",
                                item.id.as_str()
                            )))
                            .ghost()
                            .xsmall()
                            .label(t!("sftp.transfer.remove"))
                            .on_click(cx.listener(
                                move |this, _, window, cx| this.confirm_remove(&item, window, cx),
                            )),
                        )
                    })
                    .when(
                        item.state == TransferState::Done && item.direction == Direction::Download,
                        |this| {
                            this.child(
                                Button::new(SharedString::from(format!("transfer-reveal-{id}")))
                                    .ghost()
                                    .small()
                                    .icon(IconName::FolderOpen)
                                    .tooltip(t!("sftp.show_in_folder"))
                                    .on_click(move |_, _, cx| cx.reveal_path(&local)),
                            )
                        },
                    ),
            )
            .when(pending, |this| {
                this.child(
                    div().h_1().w_full().rounded_full().bg(theme.muted).child(
                        div()
                            .h_full()
                            .rounded_full()
                            .bg(theme.primary)
                            .w(relative(fraction)),
                    ),
                )
            })
            .when_some(item.error.clone().filter(|_| failed), |this, error| {
                this.child(
                    div()
                        .text_color(theme.danger)
                        .child(prose(errors::describe(&error))),
                )
            })
            .when_some(skipped, |this, (text, paths)| {
                this.child(
                    div()
                        .id(SharedString::from(format!("transfer-skipped-{id}")))
                        .text_color(theme.warning)
                        .child(prose(text))
                        .tooltip(move |window, cx| Tooltip::new(paths.clone()).build(window, cx)),
                )
            })
            .into_any_element()
    }
}

/// 上传因远程已存在同名项而失败：继续前要用户同意覆盖。
fn needs_overwrite(item: &TransferView) -> bool {
    matches!(item.error, Some(SftpError::AlreadyExists { .. }))
}

/// 跳过的项：说明与（悬停时显示的）路径。
fn skipped_text(item: &TransferView) -> Option<(SharedString, SharedString)> {
    if item.skipped == 0 {
        return None;
    }
    let unreadable = item.skipped.saturating_sub(item.skipped_special);
    let mut parts: Vec<SharedString> = Vec::new();
    if unreadable > 0 {
        parts.push(t!("sftp.transfer.skipped_unreadable", count = unreadable));
    }
    if item.skipped_special > 0 {
        parts.push(t!(
            "sftp.transfer.skipped_special",
            count = item.skipped_special
        ));
    }
    let text = parts.join(&t!("sftp.list_separator"));
    let mut paths = item.skipped_paths.join("\n");
    let more = item.skipped as usize - item.skipped_paths.len().min(item.skipped as usize);
    if more > 0 {
        paths.push('\n');
        paths.push_str(&t!("sftp.transfer.skipped_more", count = more));
    }
    Some((text.into(), paths.into()))
}

impl Render for TransferList {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let items: Vec<TransferView> = self.items.clone();
        let mut rows = Vec::with_capacity(items.len());
        for item in &items {
            rows.push(self.render_item(item, cx));
        }
        let border = cx.theme().border;
        v_flex()
            .id("transfer-list")
            .flex_none()
            .border_t_1()
            .border_color(border)
            .child(self.render_header(cx))
            .child(
                div()
                    .id("transfer-rows")
                    .max_h(rems(12.))
                    .overflow_y_scrollbar()
                    .child(v_flex().children(rows)),
            )
    }
}
