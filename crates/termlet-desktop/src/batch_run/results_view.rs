//! 批量执行 Tab 的下半部分：汇总一行（重试没有成功的、复制结果），还在执行的，各组的输出（第一组为基准，其余组默认
//! 只显示与它不同的行），没有成功的与原因（要在终端中交互一次的给出“在终端中打开”）。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, Context, Div, ElementId, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    TestSupportExt as _, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::proto::BatchStatus;

use super::BatchPanel;
use super::results::{DiffLine, Results, Row, TargetKey, status_label, summary_text};
use super::run::needs_terminal;
use crate::prose::prose;
use crate::t;

/// 还在执行的最多列出这么多个名称。
const PENDING_NAMES: usize = 8;

impl BatchPanel {
    pub(super) fn render_results(&mut self, _: &mut Window, cx: &mut Context<Self>) -> Div {
        let running = self.running.is_some();
        let Some(results) = self.results.as_mut() else {
            return v_flex().p_3().child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(prose(t!("batch.empty"))),
            );
        };
        results.ensure_diffs();
        let results = &*results;
        let groups: Vec<AnyElement> = (0..results.groups.len())
            .map(|ix| render_group(results, ix, cx))
            .collect();
        let problems: Vec<&Row> = results.rows.iter().filter(|r| r.is_problem()).collect();
        let problems = (!problems.is_empty()).then(|| render_problems(&problems, cx));
        let pending: Vec<SharedString> = results.pending().map(|r| r.name.clone()).collect();
        let header = render_header(results, running, cx);
        let theme = cx.theme();
        v_flex()
            .gap_3()
            .p_3()
            .child(header)
            .when(!pending.is_empty() && running, |this| {
                let mut names = pending
                    .iter()
                    .take(PENDING_NAMES)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(&t!("batch.name_separator"));
                if pending.len() > PENDING_NAMES {
                    names.push('…');
                }
                this.child(
                    h_flex()
                        .gap_2()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(Icon::new(IconName::LoaderCircle).small())
                        .child(prose(t!(
                            "batch.running",
                            count = pending.len(),
                            names = names
                        ))),
                )
            })
            .children(groups)
            .children(problems)
    }
}

/// 汇总一行：台数与各状态的台数、用时；重试没有成功的、复制结果。
fn render_header(results: &Results, running: bool, cx: &mut Context<BatchPanel>) -> AnyElement {
    let summary = results.summary();
    let mut text = summary_text(&summary, results.total_label()).to_string();
    if results.groups.len() > 1 {
        text.push_str(&t!("batch.separator"));
        text.push_str(&t!("batch.group_count", count = results.groups.len()));
    }
    if let Some(elapsed) = results.elapsed {
        text.push_str(&t!("batch.separator"));
        text.push_str(&t!(
            "batch.elapsed",
            seconds = format!("{:.1}", elapsed.as_secs_f64())
        ));
    }
    let retryable = !running && !results.retry_keys().is_empty();
    h_flex()
        .gap_2()
        .flex_wrap()
        .items_center()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .font_medium()
                .child(prose(text)),
        )
        .child(
            Button::new("batch-retry")
                .ghost()
                .small()
                .icon(IconName::RotateCw)
                .label(t!("batch.retry"))
                .disabled(!retryable)
                .on_click(cx.listener(|this, _, window, cx| this.retry(window, cx))),
        )
        .child(
            Button::new("batch-copy")
                .ghost()
                .small()
                .icon(IconName::Copy)
                .label(t!("batch.copy"))
                .on_click(cx.listener(|this, _, window, cx| this.copy_results(window, cx))),
        )
        .into_any_element()
}

/// 一组：成员、退出码，基准的完整输出或其余组与基准的差异（可以切换）。
fn render_group(results: &Results, ix: usize, cx: &mut Context<BatchPanel>) -> AnyElement {
    let theme = cx.theme();
    let group = &results.groups[ix];
    let id = group.id();
    let slug = results.rows[id].key.slug();
    let full = ix == 0 || results.full.contains(&id);
    let diff = if full { None } else { results.diff_of(id) };
    let too_long = !full && diff.is_none();
    let title = match (ix, results.groups.len()) {
        (0, 1) => t!("batch.same_group", count = group.members.len()),
        (0, _) if results.has_majority() => t!("batch.base_group", count = group.members.len()),
        (0, _) => t!("batch.first_group", count = group.members.len()),
        _ => t!("batch.other_group", count = group.members.len()),
    };
    let truncated = group.members.iter().any(|&i| results.rows[i].truncated);
    let body: AnyElement = if group.text.is_empty() {
        div()
            .text_color(theme.muted_foreground)
            .child(prose(t!("batch.no_output")))
            .into_any_element()
    } else if let Some(diff) = diff {
        v_flex()
            .children(diff.iter().map(|line| {
                let (prefix, text, bg) = match line {
                    DiffLine::Same(text) => ("  ", text, None),
                    DiffLine::Add(text) => ("+ ", text, Some(theme.success.opacity(0.15))),
                    DiffLine::Del(text) => ("- ", text, Some(theme.danger.opacity(0.15))),
                };
                div()
                    .when_some(bg, |el, bg| el.bg(bg))
                    .child(format!("{prefix}{text}"))
            }))
            .into_any_element()
    } else {
        div().child(group.text.clone()).into_any_element()
    };
    v_flex()
        .gap_1()
        .p_2()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .child(
            h_flex()
                .gap_2()
                .flex_wrap()
                .text_sm()
                .child(div().font_medium().child(title))
                .children(group.members.iter().map(|&i| {
                    div()
                        .px_1()
                        .rounded(theme.radius)
                        .bg(theme.muted)
                        .text_xs()
                        .child(results.rows[i].name.clone())
                }))
                .child(
                    div()
                        .text_xs()
                        .text_color(if group.exit_code == 0 {
                            theme.muted_foreground
                        } else {
                            theme.danger
                        })
                        .child(t!("batch.exit_code", code = group.exit_code)),
                )
                .child(div().flex_1())
                .when(ix > 0, |el| {
                    el.child(
                        Button::new(element_id("batch-toggle", &slug))
                            .ghost()
                            .xsmall()
                            .label(if full {
                                t!("batch.show_diff")
                            } else {
                                t!("batch.show_full")
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(results) = this.results.as_mut() {
                                    results.toggle_full(id);
                                }
                                cx.notify();
                            })),
                    )
                }),
        )
        .child(
            div()
                .id(element_id("batch-output", &slug))
                .max_h(rems(16.))
                .overflow_y_scroll()
                .text_xs()
                .font_family(termlet_terminal::fonts::DEFAULT_FAMILY)
                .child(body),
        )
        .when(too_long, |el| {
            el.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(prose(t!("batch.too_long_diff"))),
            )
        })
        .when(truncated, |el| {
            el.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(prose(t!("batch.truncated"))),
            )
        })
        .into_any_element()
}

/// 没有成功的：名称、状态与原因；要在终端中交互一次的给出“在终端中打开”。
fn render_problems(rows: &[&Row], cx: &mut Context<BatchPanel>) -> AnyElement {
    let theme = cx.theme();
    v_flex()
        .id("batch-problems")
        .test_support()
        .gap_1()
        .p_2()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.danger)
        .text_sm()
        .child(
            div()
                .font_medium()
                .child(t!("batch.problems", count = rows.len())),
        )
        .children(rows.iter().map(|row| {
            let status = row.status().unwrap_or(BatchStatus::Skipped);
            let color = status_color(status, cx);
            let open = match (&row.key, needs_terminal(row.error)) {
                (TargetKey::Host(id), true) => Some(id.clone()),
                _ => None,
            };
            h_flex()
                .gap_2()
                .items_start()
                .child(div().flex_none().child(row.name.clone()))
                .child(
                    div()
                        .flex_none()
                        .text_color(color)
                        .child(status_label(status)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(theme.muted_foreground)
                        .when_some(row.reason.clone(), |el, reason| el.child(prose(reason))),
                )
                .when_some(open, |el, id| {
                    el.child(
                        Button::new(element_id("batch-open-terminal", id.as_str()))
                            .ghost()
                            .xsmall()
                            .icon(IconName::SquareTerminal)
                            .label(t!("batch.open_terminal"))
                            .on_click(move |_, window, cx| {
                                if let Some(workspace) =
                                    crate::workspace::Workspace::in_window(window, cx)
                                {
                                    workspace.update(cx, |w, cx| w.open_host(&id, window, cx));
                                }
                            }),
                    )
                })
        }))
        .into_any_element()
}

fn status_color(status: BatchStatus, cx: &Context<BatchPanel>) -> Hsla {
    let theme = cx.theme();
    match status {
        BatchStatus::Ok => theme.success,
        BatchStatus::Failed | BatchStatus::Unreachable | BatchStatus::Other => theme.danger,
        BatchStatus::Timeout => theme.warning,
        BatchStatus::Canceled | BatchStatus::Skipped => theme.muted_foreground,
    }
}

fn element_id(prefix: &str, key: &str) -> ElementId {
    ElementId::from(SharedString::from(format!("{prefix}-{key}")))
}
