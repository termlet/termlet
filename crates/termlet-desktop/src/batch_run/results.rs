//! 批量执行的结果：逐台的进度实时更新；结束时有退出码的按“退出码 + 输出”分组，人数最多的一组为基准，其余组默认只
//! 显示与基准不同的行（按行的最长公共子序列）。没有退出码的（连不上、超时、已取消、未执行，以及连上之后出错的）不参与
//! 分组，在“没有成功”中列出原因。
//!
//! 输出只存在组里（同样的输出只存一份），逐台只记进度、退出码与原因。

use std::collections::{HashMap, HashSet};
use std::hash::{Hash as _, Hasher as _};
use std::time::Duration;

use gpui_kit::{EntityId, SharedString};
use termlet_core::id::HostId;
use termlet_core::proto::{BatchStatus, BatchSummary, ErrorCode};

use crate::t;

/// 两边行数的乘积超过这么多时不算逐行差异。
const MAX_CELLS: usize = 4_000_000;

/// 一台：主机（新建连接执行）或终端（在已打开的终端里执行）。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TargetKey {
    Host(HostId),
    Terminal(EntityId),
}

impl TargetKey {
    /// 元素 ID 中的写法。
    pub(crate) fn slug(&self) -> String {
        match self {
            TargetKey::Host(id) => format!("h-{id}"),
            TargetKey::Terminal(id) => format!("t-{}", id.as_u64()),
        }
    }
}

/// 一台的进度。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Progress {
    /// 等着开始（同时执行的台数已满）
    Waiting,
    Running,
    Done(BatchStatus),
}

/// 一台的结果。
#[derive(Clone, Debug)]
pub(crate) struct Row {
    pub key: TargetKey,
    pub name: SharedString,
    pub progress: Progress,
    pub exit_code: Option<i32>,
    /// 没有成功的原因（界面语言）
    pub reason: Option<SharedString>,
    /// 出错时的错误码
    pub error: Option<ErrorCode>,
    /// 输出超过上限，只取回了开头
    pub truncated: bool,
    pub duration: Duration,
}

impl Row {
    pub(crate) fn status(&self) -> Option<BatchStatus> {
        match self.progress {
            Progress::Done(status) => Some(status),
            _ => None,
        }
    }

    /// 结束了、进了某一组（有退出码）。
    fn grouped(&self) -> bool {
        matches!(
            self.progress,
            Progress::Done(BatchStatus::Ok | BatchStatus::Failed)
        ) && self.exit_code.is_some()
    }

    /// 结束了、没有成功、不在组里：在“没有成功”中列出。
    pub(crate) fn is_problem(&self) -> bool {
        matches!(self.progress, Progress::Done(status) if status != BatchStatus::Ok)
            && !self.grouped()
    }

    /// 可以重试：结束了、没有成功，且不是因为不能执行而跳过（Telnet、串口）。
    pub(crate) fn retryable(&self) -> bool {
        matches!(self.progress, Progress::Done(status) if status != BatchStatus::Ok)
            && self.error != Some(ErrorCode::HostNotAllowed)
    }
}

/// 一台结束时交给 [`Results::finish`] 的内容。
#[derive(Clone, Debug, Default)]
pub(crate) struct Finished {
    pub status: BatchStatus,
    pub exit_code: Option<i32>,
    /// 输出（主机：标准输出后接标准错误；终端：两者本来就并在一起）
    pub output: String,
    pub truncated: bool,
    pub reason: Option<SharedString>,
    pub error: Option<ErrorCode>,
    pub duration: Duration,
}

/// 输出相同的一组。
#[derive(Clone, Debug)]
pub(crate) struct Group {
    /// 组的标识：最先结束的成员（组的顺序会变，它不会）
    id: usize,
    /// 成员在 [`Results::rows`] 中的位置，按选择时的顺序
    pub members: Vec<usize>,
    pub exit_code: i32,
    /// 去掉末尾空白的输出
    pub text: SharedString,
    hash: u64,
}

impl Group {
    pub(crate) fn id(&self) -> usize {
        self.id
    }
}

/// 差异中的一行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DiffLine {
    Same(String),
    /// 这一组有、基准没有
    Add(String),
    /// 基准有、这一组没有
    Del(String),
}

/// 一次执行的结果。
pub(crate) struct Results {
    pub command: SharedString,
    pub rows: Vec<Row>,
    /// 人数多的在前；人数相同时先出现的在前
    pub groups: Vec<Group>,
    /// 显示完整输出（而不是与基准的差异）的组（[`Group::id`]）
    pub full: HashSet<usize>,
    /// 与基准的差异，按组（[`Group::id`]）；基准换了时清空（每组只算一次）
    diffs: HashMap<usize, Option<Vec<DiffLine>>>,
    base: Option<usize>,
    /// 全部结束时的用时
    pub elapsed: Option<Duration>,
}

impl Results {
    pub(crate) fn new(command: SharedString, targets: Vec<(TargetKey, SharedString)>) -> Self {
        Self {
            command,
            rows: targets
                .into_iter()
                .map(|(key, name)| Row {
                    key,
                    name,
                    progress: Progress::Waiting,
                    exit_code: None,
                    reason: None,
                    error: None,
                    truncated: false,
                    duration: Duration::ZERO,
                })
                .collect(),
            groups: Vec::new(),
            full: HashSet::new(),
            diffs: HashMap::new(),
            base: None,
            elapsed: None,
        }
    }

    pub(crate) fn start(&mut self, index: usize) {
        if let Some(row) = self.rows.get_mut(index)
            && row.progress == Progress::Waiting
        {
            row.progress = Progress::Running;
        }
    }

    /// 一台结束了：记下结果；有退出码的放进输出相同的一组。
    pub(crate) fn finish(&mut self, index: usize, done: Finished) {
        let Some(row) = self.rows.get_mut(index) else {
            return;
        };
        if matches!(row.progress, Progress::Done(_)) {
            return;
        }
        row.progress = Progress::Done(done.status);
        row.exit_code = done.exit_code;
        row.reason = done.reason;
        row.error = done.error;
        row.truncated = done.truncated;
        row.duration = done.duration;
        if !row.grouped() {
            return;
        }
        let exit_code = done.exit_code.unwrap_or_default();
        let text = done.output.trim_end();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        let hash = hasher.finish();
        match self
            .groups
            .iter_mut()
            .find(|g| g.exit_code == exit_code && g.hash == hash && g.text.as_ref() == text)
        {
            Some(group) => {
                let at = group.members.partition_point(|&m| m < index);
                group.members.insert(at, index);
            }
            None => self.groups.push(Group {
                id: index,
                members: vec![index],
                exit_code,
                text: SharedString::from(text.to_string()),
                hash,
            }),
        }
        // 稳定排序：人数相同保持先出现的在前
        self.groups
            .sort_by_key(|g| std::cmp::Reverse(g.members.len()));
        let base = self.groups.first().map(Group::id);
        if base != self.base {
            self.base = base;
            self.diffs.clear();
        }
    }

    /// 全部结束了（含停止后标为已取消的）。
    pub(crate) fn done(&self) -> bool {
        self.rows
            .iter()
            .all(|r| matches!(r.progress, Progress::Done(_)))
    }

    /// 还没有结束的：等着开始的与执行中的。
    pub(crate) fn pending(&self) -> impl Iterator<Item = &Row> {
        self.rows
            .iter()
            .filter(|r| !matches!(r.progress, Progress::Done(_)))
    }

    /// 停止：还没有结束的标为已取消（执行中的稍后报来的结果不再记）。
    pub(crate) fn cancel_pending(&mut self) {
        for row in &mut self.rows {
            if !matches!(row.progress, Progress::Done(_)) {
                row.progress = Progress::Done(BatchStatus::Canceled);
            }
        }
    }

    /// 一共多少台：“4 台主机”“3 个终端”。
    pub(crate) fn total_label(&self) -> SharedString {
        let count = self.rows.len();
        match self.rows.first().map(|r| &r.key) {
            Some(TargetKey::Terminal(_)) => t!("batch.terminals", count = count),
            _ => t!("batch.hosts", count = count),
        }
    }

    pub(crate) fn summary(&self) -> BatchSummary {
        let mut summary = BatchSummary::default();
        for status in self.rows.iter().filter_map(Row::status) {
            summary.add(status);
        }
        summary
    }

    /// 可以重试的（见 [`Row::retryable`]）。
    pub(crate) fn retry_keys(&self) -> Vec<TargetKey> {
        self.rows
            .iter()
            .filter(|r| r.retryable())
            .map(|r| r.key.clone())
            .collect()
    }

    /// 要显示差异的组还没算过的，现在算。
    pub(crate) fn ensure_diffs(&mut self) {
        let Some(base) = self.groups.first() else {
            return;
        };
        for group in &self.groups[1..] {
            let id = group.id;
            if !self.full.contains(&id) && !self.diffs.contains_key(&id) {
                self.diffs.insert(id, line_diff(&base.text, &group.text));
            }
        }
    }

    /// 这一组与基准的差异；没算或太长时为 `None`。
    pub(crate) fn diff_of(&self, group: usize) -> Option<&Vec<DiffLine>> {
        self.diffs.get(&group).and_then(|d| d.as_ref())
    }

    /// 切换一组显示完整输出还是差异。
    pub(crate) fn toggle_full(&mut self, group: usize) {
        if !self.full.remove(&group) {
            self.full.insert(group);
        }
    }

    /// 第一组比别的组人多：称“多数”，否则只是第一组。
    pub(crate) fn has_majority(&self) -> bool {
        match (self.groups.first(), self.groups.get(1)) {
            (Some(a), Some(b)) => a.members.len() > b.members.len(),
            _ => false,
        }
    }

    /// “复制结果”的文字：命令、各组（主机、退出码与输出）、没有成功的与原因、汇总。
    pub(crate) fn copy_text(&self) -> String {
        let mut out = format!("$ {}\n", self.command);
        for group in &self.groups {
            let names: Vec<&str> = group
                .members
                .iter()
                .map(|&i| self.rows[i].name.as_ref())
                .collect();
            let heading = t!(
                "batch.copy_group",
                names = names.join(", "),
                code = group.exit_code
            );
            out.push_str(&format!("\n── {heading}\n"));
            if !group.text.is_empty() {
                out.push_str(&group.text);
                out.push('\n');
            }
        }
        let problems: Vec<&Row> = self.rows.iter().filter(|r| r.is_problem()).collect();
        if !problems.is_empty() {
            out.push_str(&format!(
                "\n── {}\n",
                t!("batch.problems", count = problems.len())
            ));
            for row in problems {
                let status = row.status().map(status_label).unwrap_or_default();
                let line = match &row.reason {
                    Some(reason) => t!(
                        "batch.copy_problem",
                        name = row.name.clone(),
                        status = status,
                        reason = reason.clone()
                    ),
                    None => t!(
                        "batch.copy_problem_short",
                        name = row.name.clone(),
                        status = status
                    ),
                };
                out.push_str(&line);
                out.push('\n');
            }
        }
        out.push_str(&format!(
            "\n{}\n",
            summary_text(&self.summary(), self.total_label())
        ));
        out
    }
}

/// 状态的说法（与命令行相同的几个词）。
pub(crate) fn status_label(status: BatchStatus) -> SharedString {
    match status {
        BatchStatus::Ok => t!("batch.status.ok"),
        BatchStatus::Failed | BatchStatus::Other => t!("batch.status.failed"),
        BatchStatus::Unreachable => t!("batch.status.unreachable"),
        BatchStatus::Timeout => t!("batch.status.timeout"),
        BatchStatus::Canceled => t!("batch.status.canceled"),
        BatchStatus::Skipped => t!("batch.status.skipped"),
    }
}

/// 汇总的一行：“4 台主机，成功 2，失败 1，连不上 1”（是 0 的不写）。
pub(crate) fn summary_text(summary: &BatchSummary, total: SharedString) -> SharedString {
    let mut parts = vec![total];
    for (count, status) in [
        (summary.ok, BatchStatus::Ok),
        (summary.failed, BatchStatus::Failed),
        (summary.unreachable, BatchStatus::Unreachable),
        (summary.timeout, BatchStatus::Timeout),
        (summary.canceled, BatchStatus::Canceled),
        (summary.skipped, BatchStatus::Skipped),
    ] {
        if count > 0 {
            parts.push(t!(
                "batch.count",
                status = status_label(status),
                count = count
            ));
        }
    }
    parts.join(&t!("batch.separator")).into()
}

/// 按行比较 `base` 与 `other`；两边太长时返回 `None`。回溯时删除优先。
pub(crate) fn line_diff(base: &str, other: &str) -> Option<Vec<DiffLine>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn results(n: u64) -> Results {
        Results::new(
            "uptime".into(),
            (1..=n)
                .map(|i| {
                    (
                        TargetKey::Terminal(EntityId::from(i)),
                        format!("h{i}").into(),
                    )
                })
                .collect(),
        )
    }

    fn ran(code: i32, output: &str) -> Finished {
        Finished {
            status: if code == 0 {
                BatchStatus::Ok
            } else {
                BatchStatus::Failed
            },
            exit_code: Some(code),
            output: output.into(),
            ..Default::default()
        }
    }

    #[test]
    fn outputs_group_by_exit_code_and_trimmed_text() {
        let mut r = results(5);
        r.finish(0, ran(0, "a\nb\n"));
        r.finish(1, ran(0, "x"));
        r.finish(2, ran(0, "a\nb"));
        r.finish(
            3,
            Finished {
                status: BatchStatus::Unreachable,
                reason: Some("忙".into()),
                ..Default::default()
            },
        );
        r.finish(4, ran(1, "a\nb"));
        assert_eq!(r.groups.len(), 3);
        assert_eq!(r.groups[0].members, [0, 2], "人数多的在前");
        assert_eq!(r.groups[1].members, [1], "人数相同时先出现的在前");
        assert_eq!(r.groups[2].exit_code, 1, "退出码不同另成一组");
        assert!(r.rows[3].is_problem() && !r.rows[4].is_problem());
        assert!(r.done());
        let s = r.summary();
        assert_eq!((s.ok, s.failed, s.unreachable), (3, 1, 1));
    }

    #[test]
    fn retry_skips_successes_and_unsupported_hosts() {
        let mut r = results(4);
        r.finish(0, ran(0, ""));
        r.finish(1, ran(2, ""));
        r.finish(
            2,
            Finished {
                status: BatchStatus::Skipped,
                error: Some(ErrorCode::HostNotAllowed),
                ..Default::default()
            },
        );
        r.start(3);
        r.cancel_pending();
        assert_eq!(
            r.retry_keys(),
            [
                TargetKey::Terminal(EntityId::from(2)),
                TargetKey::Terminal(EntityId::from(4))
            ]
        );
        assert_eq!(r.rows[3].status(), Some(BatchStatus::Canceled));
        // 停止之后才报来的结果不再记
        r.finish(3, ran(0, "late"));
        assert_eq!(r.rows[3].status(), Some(BatchStatus::Canceled));
    }

    #[test]
    fn diffs_follow_the_base_group() {
        let mut r = results(3);
        r.finish(0, ran(0, "pid-1\nok"));
        r.finish(1, ran(0, "pid-2\nok"));
        r.ensure_diffs();
        assert_eq!(
            r.diff_of(1).unwrap(),
            &[
                DiffLine::Del("pid-1".into()),
                DiffLine::Add("pid-2".into()),
                DiffLine::Same("ok".into()),
            ]
        );
        // 第三台与第二台相同：基准换成它们这一组，差异重算
        r.finish(2, ran(0, "pid-2\nok"));
        assert_eq!(r.groups[0].id(), 1);
        assert_eq!(r.groups[0].members, [1, 2]);
        assert!(r.has_majority());
        assert!(r.diff_of(0).is_none());
        r.ensure_diffs();
        assert!(r.diff_of(0).is_some());
        r.toggle_full(0);
        assert!(r.full.contains(&0));
    }

    #[test]
    fn line_diff_gives_up_on_long_outputs() {
        assert_eq!(line_diff("a", "a").unwrap(), [DiffLine::Same("a".into())]);
        let long = "x\n".repeat(2100);
        assert!(line_diff(&long, &long).is_none(), "太长时不算");
    }

    #[test]
    fn copy_text_lists_groups_problems_and_summary() {
        crate::i18n::set_language(crate::i18n::Language::ZhCn);
        let mut r = results(3);
        r.finish(0, ran(0, "active"));
        r.finish(1, ran(0, "active"));
        r.finish(
            2,
            Finished {
                status: BatchStatus::Unreachable,
                reason: Some("认证失败".into()),
                ..Default::default()
            },
        );
        let text = r.copy_text();
        assert!(text.starts_with("$ uptime\n"), "{text}");
        assert!(text.contains("── h1, h2（退出码 0）\nactive\n"), "{text}");
        assert!(text.contains("h3：连不上，认证失败"), "{text}");
        assert!(text.contains("3 个终端，成功 2，连不上 1"), "{text}");
    }
}
