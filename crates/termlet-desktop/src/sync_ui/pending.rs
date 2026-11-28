//! 同步需要用户决定的事（这时什么都没改，自动同步暂停）：首次同步而两边都有数据时选择合并、以同步位置为准或以本机为准；
//! 一次要删除较多数据时确认。回答后继续同步，又遇到需要决定的事时在同一个对话框中接着问。
//!
//! 在设置页中触发的同步遇到时自动打开；自动同步遇到时由标题栏的同步图标提示，在设置页中打开。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::radio::Radio;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Task, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::cloudsync::{Answer, Pending, SyncOutcome};

use super::labels::{count_label, count_rows};
use super::{cloud, describe, model, on_core};
use crate::dialog_token::{DialogToken, close_own};
use crate::prose::prose;
use crate::t;
use crate::ui_scale::DialogTitleExt as _;

/// 首次同步的三种方式（单选的顺序）。
const FIRST_SYNC_CHOICES: [Answer; 3] = [Answer::Merge, Answer::UseRemote, Answer::UseLocal];

/// 打开对话框回答等待决定的事；没有时什么也不做。
pub(crate) fn answer_pending(window: &mut Window, cx: &mut App) {
    if let Some(pending) = cloud(cx).status().pending {
        open(pending, window, cx);
    }
}

fn open(pending: Pending, window: &mut Window, cx: &mut App) {
    let dialog = cx.new(|_| PendingDialog::new(pending));
    let width = rems(32.).to_pixels(window.rem_size());
    let owner = dialog.clone();
    window.open_dialog(cx, move |d, _, cx| {
        let state = dialog.read(cx);
        // 正在同步时不能关闭：结果回来时关闭的是这个对话框
        let busy = state.busy;
        // Enter 与点“继续”相同；要删除条目时不响应 Enter（必须点按钮）。完成后由 submit 关闭，这里总是返回 false
        let on_ok = {
            let dialog = dialog.clone();
            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                dialog.update(cx, |dialog, cx| {
                    if !matches!(dialog.pending, Pending::ConfirmDeletes { .. }) {
                        dialog.submit(window, cx);
                    }
                });
                false
            }
        };
        d.titled(state.title())
            .w(width)
            .overlay_closable(false)
            .close_button(!busy)
            .keyboard(!busy)
            .child(dialog.clone())
            .on_ok(on_ok)
    });
    let token = DialogToken::take(window, cx);
    owner.update(cx, |dialog, _| dialog.dialog = token);
}

struct PendingDialog {
    pending: Pending,
    /// 首次同步选择的方式
    choice: Answer,
    busy: bool,
    error: Option<SharedString>,
    /// 所在的对话框（完成后只关它）
    dialog: Option<DialogToken>,
    _task: Option<Task<()>>,
}

impl PendingDialog {
    fn new(pending: Pending) -> Self {
        Self {
            pending,
            choice: Answer::Merge,
            busy: false,
            error: None,
            dialog: None,
            _task: None,
        }
    }

    fn title(&self) -> SharedString {
        match self.pending {
            Pending::FirstSync { .. } => t!("sync_ui.pending.first_title"),
            Pending::ConfirmDeletes { .. } => t!("sync_ui.pending.deletes_title"),
        }
    }

    fn answer(&self) -> Answer {
        match self.pending {
            Pending::FirstSync { .. } => self.choice,
            Pending::ConfirmDeletes { .. } => Answer::ConfirmDeletes,
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let cloud = cloud(cx);
        let answer = self.answer();
        tracing::info!(answer = ?answer, "云同步：回答等待决定的事");
        let job = on_core(async move { cloud.answer(answer).await });
        self.busy = true;
        self.error = None;
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                model(cx).update(cx, |model, cx| model.refresh(cx));
                match result {
                    Ok(SyncOutcome::Done(_)) => {
                        this.busy = true;
                        close_own(this.dialog.as_ref(), window, cx);
                    }
                    Ok(SyncOutcome::Pending(next)) => {
                        this.pending = next;
                        this.choice = Answer::Merge;
                    }
                    Err(error) => {
                        let service = model(cx).read(cx).service();
                        this.error = Some(describe(&error, service));
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn render_first_sync(
        &self,
        local: &termlet_core::cloudsync::SyncCounts,
        remote: &termlet_core::cloudsync::SyncCounts,
        duplicates: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let rows = count_rows(local, remote);
        let number = |n: usize| {
            div()
                .w(rems(6.))
                .flex_none()
                .text_right()
                .child(n.to_string())
        };
        let table = v_flex()
            .id("sync-first-counts")
            .gap_1()
            .text_sm()
            .child(
                h_flex()
                    .text_xs()
                    .text_color(muted)
                    .child(div().flex_1())
                    .child(
                        div()
                            .w(rems(6.))
                            .flex_none()
                            .text_right()
                            .child(t!("sync_ui.pending.column_local")),
                    )
                    .child(
                        div()
                            .w(rems(6.))
                            .flex_none()
                            .text_right()
                            .child(t!("sync_ui.pending.column_remote")),
                    ),
            )
            .children(rows.into_iter().map(|row| {
                h_flex()
                    .child(div().flex_1().child(count_label(row.kind)))
                    .child(number(row.local))
                    .child(number(row.remote))
            }));
        let selected = FIRST_SYNC_CHOICES.iter().position(|c| *c == self.choice);
        let choice = |label: SharedString, hint: SharedString| {
            Radio::new(label.clone())
                .label(label)
                .child(div().text_xs().text_color(muted).child(prose(hint)))
        };
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .child(prose(t!("sync_ui.pending.first_body"))),
            )
            .child(
                div()
                    .px_3()
                    .py_2()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .child(table),
            )
            .child(
                crate::radio::vertical("sync-first-choice")
                    .children([
                        choice(
                            t!("sync_ui.pending.merge"),
                            t!("sync_ui.pending.merge_hint"),
                        ),
                        choice(
                            t!("sync_ui.pending.use_remote"),
                            t!("sync_ui.pending.use_remote_hint"),
                        ),
                        choice(
                            t!("sync_ui.pending.use_local"),
                            t!("sync_ui.pending.use_local_hint"),
                        ),
                    ])
                    .selected_index(selected)
                    .disabled(self.busy)
                    .on_change(cx.listener(|this, ix: &usize, _, cx| {
                        if let Some(choice) = FIRST_SYNC_CHOICES.get(*ix) {
                            this.choice = *choice;
                            cx.notify();
                        }
                    })),
            )
            .when(duplicates > 0, |this| {
                this.child(
                    h_flex()
                        .id("sync-first-duplicates")
                        .items_start()
                        .gap_2()
                        .text_sm()
                        .text_color(theme.warning)
                        .child(Icon::new(IconName::TriangleAlert).small().flex_none())
                        .child(prose(t!("sync_ui.pending.duplicates", count = duplicates))),
                )
            })
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(prose(t!("sync_ui.pending.snapshot_hint"))),
            )
    }

    fn render_deletes(
        &self,
        local: usize,
        remote: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        v_flex()
            .gap_3()
            .child(div().text_sm().child(prose(t!(
                "sync_ui.pending.deletes_body",
                local = local,
                remote = remote
            ))))
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(prose(t!("sync_ui.pending.deletes_hint"))),
            )
    }
}

impl Render for PendingDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.pending.clone() {
            Pending::FirstSync {
                local,
                remote,
                duplicates,
            } => self
                .render_first_sync(&local, &remote, duplicates, cx)
                .into_any_element(),
            Pending::ConfirmDeletes { local, remote } => {
                self.render_deletes(local, remote, cx).into_any_element()
            }
        };
        let deletes = matches!(self.pending, Pending::ConfirmDeletes { .. });
        let theme = cx.theme();
        v_flex()
            .id("sync-pending")
            .gap_3()
            .child(body)
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .id("sync-pending-error")
                        .text_sm()
                        .text_color(theme.danger)
                        .child(prose(error)),
                )
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .pt_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("sync-pending-cancel")
                            .ghost()
                            .label(t!("sync_ui.pending.later"))
                            .disabled(self.busy)
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("sync-pending-submit")
                            .map(|b| if deletes { b.danger() } else { b.primary() })
                            .label(t!("sync_ui.pending.continue"))
                            .loading(self.busy)
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}
