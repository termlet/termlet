//! 撰写栏“执行并对比”的结果：各终端同时执行，结果按完成的先后到达；全部返回后按“退出码 + 输出”分组，
//! 人数最多的一组为基准，其余组默认只显示与基准不同的行（按行的最长公共子序列）。没有执行成功的单独列出。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, Context, EntityId, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, TestSupportExt as _,
    Window, div, prelude::FluentBuilder as _, rems,
};
use std::collections::{HashMap, HashSet};

use crate::prose::prose;
use crate::t;

/// 两边行数的乘积超过这么多时不算逐行差异。
const MAX_CELLS: usize = 4_000_000;

/// 一台主机的结果。
#[derive(Clone, Debug)]
pub struct HostResult {
    pub id: EntityId,
    pub title: SharedString,
    pub outcome: Result<(i32, String), SharedString>,
}

/// 输出相同的一组（索引指向结果列表）。
#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    pub members: Vec<usize>,
    pub exit_code: i32,
    pub output: String,
}

/// 按“退出码 + 去掉末尾空白的输出”分组；人数多的在前，人数相同时先完成的在前。失败的不参与。
pub fn group_outputs(results: &[HostResult]) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for (i, result) in results.iter().enumerate() {
        let Ok((code, output)) = &result.outcome else {
            continue;
        };
        let output = output.trim_end().to_string();
        match groups
            .iter_mut()
            .find(|g| g.exit_code == *code && g.output == output)
        {
            Some(group) => group.members.push(i),
            None => groups.push(Group {
                members: vec![i],
                exit_code: *code,
                output,
            }),
        }
    }
    // 稳定排序：人数相同保持先完成的在前
    groups.sort_by_key(|g| std::cmp::Reverse(g.members.len()));
    groups
}

/// 差异中的一行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffLine {
    Same(String),
    /// 这一组有、基准没有
    Add(String),
    /// 基准有、这一组没有
    Del(String),
}

/// 按行比较 `base` 与 `other`；两边太长时返回 `None`。回溯时删除优先。
pub fn line_diff(base: &str, other: &str) -> Option<Vec<DiffLine>> {
    let a: Vec<&str> = base.split('\n').collect();
    let b: Vec<&str> = other.split('\n').collect();
    if a.len().saturating_mul(b.len()) > MAX_CELLS {
        return None;
    }
    let (n, m) = (a.len(), b.len());
    // lcs[i][j]：a[i..] 与 b[j..] 的最长公共子序列长度
    let mut lcs = vec![0u32; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[at(i, j)] = if a[i] == b[j] {
                lcs[at(i + 1, j + 1)] + 1
            } else {
                lcs[at(i + 1, j)].max(lcs[at(i, j + 1)])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            out.push(DiffLine::Same(a[i].to_string()));
            i += 1;
            j += 1;
        } else if i < n && (j >= m || lcs[at(i + 1, j)] >= lcs[at(i, j + 1)]) {
            out.push(DiffLine::Del(a[i].to_string()));
            i += 1;
        } else {
            out.push(DiffLine::Add(b[j].to_string()));
            j += 1;
        }
    }
    Some(out)
}

/// 结果对话框的内容。
pub struct BatchResults {
    command: SharedString,
    /// 全部目标（标题）与已返回的结果
    targets: Vec<(EntityId, SharedString)>,
    results: Vec<HostResult>,
    /// 显示完整输出的组（按组的第一个成员）
    full: HashSet<EntityId>,
    /// 分组、各组的输出与和基准的差异：结果到达时重算，不在每次渲染时算（输出最多 256 KB，差异要算最长公共子序列）
    groups: Vec<Group>,
    texts: Vec<SharedString>,
    diffs: HashMap<EntityId, Option<Vec<DiffLine>>>,
}

impl BatchResults {
    pub fn new(command: SharedString, targets: Vec<(EntityId, SharedString)>) -> Self {
        Self {
            command,
            targets,
            results: Vec::new(),
            full: HashSet::new(),
            groups: Vec::new(),
            texts: Vec::new(),
            diffs: HashMap::new(),
        }
    }

    /// 一台主机返回了。
    pub fn push(&mut self, result: HostResult, cx: &mut Context<Self>) {
        self.results.push(result);
        self.groups = group_outputs(&self.results);
        self.texts = self
            .groups
            .iter()
            .map(|g| SharedString::from(g.output.clone()))
            .collect();
        self.diffs.clear();
        cx.notify();
    }

    /// 要显示差异的组还没算过的，现在算（每组只算一次）。
    fn ensure_diffs(&mut self) {
        let Some(base) = self.groups.first() else {
            return;
        };
        for group in &self.groups[1..] {
            let first = self.results[group.members[0]].id;
            if !self.full.contains(&first) && !self.diffs.contains_key(&first) {
                self.diffs
                    .insert(first, line_diff(&base.output, &group.output));
            }
        }
    }

    fn done(&self) -> bool {
        self.results.len() >= self.targets.len()
    }

    fn render_group(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let group = &self.groups[ix];
        let first = self.results[group.members[0]].id;
        let full = ix == 0 || self.full.contains(&first);
        let diff = if full {
            None
        } else {
            self.diffs.get(&first).and_then(|d| d.as_ref())
        };
        let too_long = !full && diff.is_none();
        let title = if ix == 0 {
            t!("batch.base_group", count = group.members.len())
        } else {
            t!("batch.other_group", count = group.members.len())
        };
        let body: AnyElement = if group.output.is_empty() {
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
            div().child(self.texts[ix].clone()).into_any_element()
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
                            .child(self.results[i].title.clone())
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
                            Button::new(("batch-toggle", first.as_u64() as usize))
                                .ghost()
                                .xsmall()
                                .label(if full {
                                    t!("batch.show_diff")
                                } else {
                                    t!("batch.show_full")
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if !this.full.remove(&first) {
                                        this.full.insert(first);
                                    }
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .child(
                div()
                    .id(("batch-output", first.as_u64() as usize))
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
            .into_any_element()
    }
}

impl Render for BatchResults {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_diffs();
        // 最高 36rem，窗口矮时缩到窗口内：对话框放不下时 gpui-kit 0.8 起滚轮只滚对话框本身，这里的结果就滚不到了
        let max_height = rems(36.)
            .to_pixels(window.rem_size())
            .min(window.viewport_size().height - rems(8.).to_pixels(window.rem_size()));
        let group_elements: Vec<AnyElement> = (0..self.groups.len())
            .map(|ix| self.render_group(ix, cx))
            .collect();
        let groups = &self.groups;
        let theme = cx.theme();
        let pending: Vec<SharedString> = self
            .targets
            .iter()
            .filter(|(id, _)| !self.results.iter().any(|r| r.id == *id))
            .map(|(_, title)| title.clone())
            .collect();
        let failed: Vec<&HostResult> = self.results.iter().filter(|r| r.outcome.is_err()).collect();
        let summary = self.done().then(|| {
            if groups.len() <= 1 && failed.is_empty() {
                t!("batch.all_same", count = self.results.len())
            } else {
                t!(
                    "batch.summary",
                    groups = groups.len(),
                    failed = failed.len()
                )
            }
        });
        v_flex()
            .id("batch-results")
            .test_support()
            .gap_3()
            .max_h(max_height)
            .overflow_y_scroll()
            .child(
                div()
                    .text_sm()
                    .truncate()
                    .font_family(termlet_terminal::fonts::DEFAULT_FAMILY)
                    .text_color(theme.muted_foreground)
                    .child(self.command.clone()),
            )
            .when(!pending.is_empty(), |el| {
                el.child(
                    h_flex()
                        .gap_2()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(Icon::new(IconName::LoaderCircle).small())
                        .child(prose(t!(
                            "batch.running",
                            count = pending.len(),
                            names = pending.join("、")
                        ))),
                )
            })
            .when_some(summary, |el, summary| {
                el.child(div().text_sm().font_medium().child(prose(summary)))
            })
            .when(!group_elements.is_empty(), |el| {
                el.child(v_flex().gap_2().children(group_elements))
            })
            .when(!failed.is_empty(), |el| {
                el.child(
                    v_flex()
                        .gap_1()
                        .p_2()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(theme.danger)
                        .text_sm()
                        .child(div().font_medium().child(t!("batch.failed")))
                        .children(failed.iter().map(|r| {
                            let reason = r.outcome.as_ref().err().cloned().unwrap_or_default();
                            h_flex()
                                .gap_2()
                                .child(div().flex_none().child(r.title.clone()))
                                .child(div().text_color(theme.danger).child(prose(reason)))
                        })),
                )
            })
    }
}

/// 打开结果对话框，返回它的内容（结果到达时 `push`）。
pub fn open(
    command: SharedString,
    targets: Vec<(EntityId, SharedString)>,
    window: &mut Window,
    cx: &mut App,
) -> gpui_kit::Entity<BatchResults> {
    use gpui_kit::AppContext as _;
    use gpui_kit::component::WindowExt as _;
    let content = cx.new(|_| BatchResults::new(command, targets));
    let width = rems(48.).to_pixels(window.rem_size());
    let view = content.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        dialog.title(t!("batch.title")).w(width).child(view.clone())
    });
    content
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(n: u64, outcome: Result<(i32, &str), &str>) -> HostResult {
        HostResult {
            id: EntityId::from(n),
            title: format!("h{n}").into(),
            outcome: outcome
                .map(|(code, out)| (code, out.to_string()))
                .map_err(|e| SharedString::from(e.to_string())),
        }
    }

    #[test]
    fn outputs_group_by_exit_code_and_trimmed_text() {
        let results = vec![
            result(1, Ok((0, "a\nb\n"))),
            result(2, Ok((0, "x"))),
            result(3, Ok((0, "a\nb"))),
            result(4, Err("忙")),
            result(5, Ok((1, "a\nb"))),
        ];
        let groups = group_outputs(&results);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].members, [0, 2], "人数多的在前");
        assert_eq!(groups[1].members, [1], "人数相同时先完成的在前");
        assert_eq!(groups[2].exit_code, 1, "退出码不同另成一组");
    }

    #[test]
    fn line_diff_marks_added_and_removed_lines() {
        let diff = line_diff("pid-1\nok", "pid-2\nok").unwrap();
        assert_eq!(
            diff,
            [
                DiffLine::Del("pid-1".into()),
                DiffLine::Add("pid-2".into()),
                DiffLine::Same("ok".into()),
            ]
        );
        assert_eq!(line_diff("a", "a").unwrap(), [DiffLine::Same("a".into())]);
        let long = "x\n".repeat(2100);
        assert!(line_diff(&long, &long).is_none(), "太长时不算");
    }
}
