//! 崩溃记录：窗口打开后有新的记录时提示“Termlet 上次运行时出错了”（[`check_crashes`]，每次运行只提示一次，提示后
//! 标为已提示）；“查看…”打开崩溃记录对话框（[`open_crash_log`]），可以复制、删除。

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, StyledExt as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, ClipboardItem, Context, ElementId, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Task, TestSupportExt as _, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::crash::{CrashLog, CrashRecord, MAX_RECORDS, Upload};

use crate::prose::prose;
use crate::t;

/// 崩溃提示的通知（按类型去重）。
struct CrashNotice;

/// 窗口打开后调用：有还没提示过的崩溃记录时弹出提示，提示后标为已提示。多个窗口只提示一次。
pub(crate) fn check_crashes(window: &mut Window, cx: &mut App) {
    let Some(services) = super::services(cx) else {
        return;
    };
    if services.crashes_checked.replace(true) {
        return;
    }
    let crash = services.crash.clone();
    window
        .spawn(cx, async move |cx| {
            let unseen = crash.unseen().await;
            if unseen.is_empty() {
                return;
            }
            let shown = cx.update(|window, cx| {
                let notification =
                    Notification::warning(t!("lock_ui.crash.notice_message", count = unseen.len()))
                        .id::<CrashNotice>()
                        .title(t!("lock_ui.crash.notice_title"))
                        .action(|_, _, _| {
                            Button::new("crash-notice-view")
                                .outline()
                                .xsmall()
                                .label(t!("lock_ui.crash.view"))
                                .on_click(|_, window, cx| open_crash_log(window, cx))
                        });
                window.push_notification(notification, cx);
            });
            if shown.is_ok()
                && let Err(e) = crash.mark_seen().await
            {
                tracing::warn!("记下崩溃记录已提示失败：{e}");
            }
        })
        .detach();
}

/// 打开崩溃记录对话框（最新的在前）。
pub(crate) fn open_crash_log(window: &mut Window, cx: &mut App) {
    let Some(services) = super::services(cx) else {
        return;
    };
    let crash = services.crash.clone();
    let view = cx.new(|cx| CrashLogView::new(crash, window, cx));
    let notice = view.read(cx).notice.clone();
    let width = rems(44.).to_pixels(window.rem_size());
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title(t!("lock_ui.crash.title"))
            .w(width)
            .child(view.clone())
    });
    crate::notify::attach(&notice, window, cx);
}

/// 崩溃记录的列表。
struct CrashLogView {
    crash: CrashLog,
    /// `None` 为还在读取
    records: Option<Vec<CrashRecord>>,
    error: Option<SharedString>,
    /// 复制的结果（写在底部，见 [`crate::notify::show`]）
    notice: Entity<crate::notify::NoticeLine>,
    _load: Task<()>,
    _delete: Option<Task<()>>,
}

impl CrashLogView {
    fn new(crash: CrashLog, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let load = Self::load(&crash, window, cx);
        Self {
            crash,
            records: None,
            error: None,
            notice: crate::notify::NoticeLine::new(cx),
            _load: load,
            _delete: None,
        }
    }

    fn load(crash: &CrashLog, window: &mut Window, cx: &mut Context<Self>) -> Task<()> {
        let crash = crash.clone();
        cx.spawn_in(window, async move |this, cx| {
            let records = crash.list().await;
            let _ = this.update(cx, |this, cx| {
                this.records = Some(records);
                cx.notify();
            });
        })
    }

    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._load = Self::load(&self.crash, window, cx);
    }

    fn copy(&self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(record) = self.records.iter().flatten().find(|r| r.id == id) else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(record.to_text()));
        crate::notify::show(
            crate::notify::Notice::success(t!("lock_ui.crash.copied")),
            window,
            cx,
        );
    }

    /// 删除一份（`id` 为 `None` 时全部删除），然后重新读取。
    fn delete(&mut self, id: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let crash = self.crash.clone();
        self._delete = Some(cx.spawn_in(window, async move |this, cx| {
            let result = match &id {
                Some(id) => crash.delete(id).await,
                None => crash.clear().await.map(drop),
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.error = result.err().map(|e| {
                    tracing::warn!("删除崩溃记录失败：{e}");
                    t!("lock_ui.crash.delete_failed", error = e.to_string())
                });
                this.reload(window, cx);
            });
        }));
    }

    fn render_record(&self, record: &CrashRecord, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, border, radius, mono, code_bg) = (
            theme.muted_foreground,
            theme.border,
            theme.radius,
            theme.mono_font_family.clone(),
            theme.muted,
        );
        let report = &record.report;
        let id = record.id.clone();
        let element_id = |name: &str| ElementId::Name(format!("{name}-{id}").into());
        let line = |label: SharedString, value: &Option<String>| {
            value.as_ref().map(|value| {
                h_flex()
                    .gap_2()
                    .items_start()
                    .child(div().w(rems(5.)).flex_none().text_color(muted).child(label))
                    .child(div().flex_1().min_w_0().child(value.clone()))
            })
        };
        v_flex()
            .id(element_id("crash-record"))
            .test_support()
            .gap_2()
            .p_3()
            .border_1()
            .border_color(border)
            .rounded(radius)
            .text_xs()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .font_medium()
                                    .child(termlet_core::time::format_local(record.created_at)),
                            )
                            .child(div().text_color(muted).child(prose(t!(
                                "lock_ui.crash.version",
                                version = report.version,
                                platform = report.platform,
                                os = report.os_release,
                                arch = report.arch
                            )))),
                    )
                    .when(record.upload == Upload::Sent, |this| {
                        this.child(div().text_color(muted).child(t!("lock_ui.crash.sent")))
                    })
                    .when(record.upload == Upload::Rejected, |this| {
                        this.child(div().text_color(muted).child(t!("lock_ui.crash.rejected")))
                    })
                    .child(
                        Button::new(element_id("crash-copy"))
                            .ghost()
                            .xsmall()
                            .label(t!("lock_ui.crash.copy"))
                            .on_click(cx.listener({
                                let id = id.clone();
                                move |this, _, window, cx| this.copy(&id, window, cx)
                            })),
                    )
                    .child(
                        Button::new(element_id("crash-delete"))
                            .ghost()
                            .xsmall()
                            .label(t!("common.delete"))
                            .on_click(cx.listener({
                                let id = id.clone();
                                move |this, _, window, cx| this.delete(Some(id.clone()), window, cx)
                            })),
                    ),
            )
            .children(line(t!("lock_ui.crash.thread"), &report.thread))
            .children(line(t!("lock_ui.crash.location"), &report.location))
            .children(line(t!("lock_ui.crash.message"), &report.message))
            // 原生崩溃：内存转储只在本机，可以在文件夹中找到它
            .when_some(self.crash.dump_path(report), |this, path| {
                this.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .w(rems(5.))
                                .flex_none()
                                .text_color(muted)
                                .child(t!("lock_ui.crash.dump")),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(report.dump.clone().unwrap_or_default()),
                        )
                        .child(
                            Button::new(element_id("crash-reveal-dump"))
                                .ghost()
                                .xsmall()
                                .label(t!("lock_ui.crash.reveal_dump"))
                                .on_click(move |_, _, cx| cx.reveal_path(&path)),
                        ),
                )
                .child(
                    div()
                        .text_color(muted)
                        .child(prose(t!("lock_ui.crash.dump_hint"))),
                )
            })
            .when_some(report.stack.clone(), |this, stack| {
                this.child(div().text_color(muted).child(t!("lock_ui.crash.stack")))
                    .child(
                        div()
                            .id(element_id("crash-stack"))
                            .max_h(rems(12.))
                            .overflow_y_scroll()
                            .px_2()
                            .py_1()
                            .rounded(radius)
                            .bg(code_bg)
                            .font_family(mono)
                            .children(stack.lines().map(|l| div().child(l.to_string()))),
                    )
            })
    }
}

impl Render for CrashLogView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger, border) = (theme.muted_foreground, theme.danger, theme.border);
        let has_records = self.records.as_ref().is_some_and(|r| !r.is_empty());
        let body = match &self.records {
            None => div()
                .text_sm()
                .text_color(muted)
                .child(prose(t!("lock_ui.crash.loading")))
                .into_any_element(),
            Some(records) if records.is_empty() => div()
                .id("crash-empty")
                .test_support()
                .text_sm()
                .text_color(muted)
                .child(t!("lock_ui.crash.empty"))
                .into_any_element(),
            Some(records) => {
                let rows: Vec<_> = records
                    .iter()
                    .map(|record| self.render_record(record, cx).into_any_element())
                    .collect();
                v_flex().gap_2().children(rows).into_any_element()
            }
        };
        // 列表高度随窗口，留出标题与按钮
        let max_height = rems(32.)
            .to_pixels(window.rem_size())
            .min(window.viewport_size().height - rems(12.).to_pixels(window.rem_size()));
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(prose(t!("lock_ui.crash.hint", max = MAX_RECORDS))),
            )
            .child(
                div()
                    .id("crash-list")
                    .max_h(max_height)
                    .overflow_y_scroll()
                    .child(body),
            )
            .child(
                h_flex()
                    .gap_2()
                    .pt_3()
                    .border_t_1()
                    .border_color(border)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .text_xs()
                            .when_some(self.error.clone(), |this, error| {
                                this.child(div().text_color(danger).child(prose(error)))
                            })
                            .child(self.notice.clone()),
                    )
                    .when(has_records, |this| {
                        this.child(
                            Button::new("crash-delete-all")
                                .outline()
                                .small()
                                .label(t!("lock_ui.crash.delete_all"))
                                .on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.delete(None, window, cx)
                                    }),
                                ),
                        )
                    })
                    .child(
                        Button::new("crash-close")
                            .small()
                            .label(t!("common.close"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
            )
    }
}
