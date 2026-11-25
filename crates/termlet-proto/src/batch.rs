//! 批量执行（协议版本 2 起）：一次请求在一批主机上执行同一条命令（或传同一个文件），各台的结果按完成的先后流式返回。
//!
//! - 选主机：[`Selector`]（主机的 ID 或名称、分组（含子分组，可写路径）、标签、全部，再减去排除的），由 Termlet 展开；
//!   只列出不执行用 [`crate::Request::ResolveHosts`]，回答 [`crate::Response::Resolved`]。
//! - 流式（[`BatchRequest::stream`] 为真，命令行工具用）：先回答 [`crate::Response::BatchStart`]（要执行与跳过的主机），
//!   每台开始时 [`crate::Response::BatchStarted`]、结束时 [`crate::Response::BatchHost`]，最后 [`crate::Response::BatchDone`]
//!   （汇总，`hosts` 为空）。不流式（MCP 用）时只回答一个 [`crate::Response::BatchDone`]，`hosts` 是全部结果。
//! - 整批的错误（锁定、没有选中可执行的主机、危险命令被拦截或拒绝）照常是一个 [`crate::Response::Error`]。
//! - 各台的状态（[`BatchStatus`]）由 Termlet 判定，命令行工具、MCP 与界面共用同一套。
//!
//! JSON 字段名与状态的写法发布后不能改（脚本与 AI 依赖它们），以后只加带默认值的字段。

use serde::{Deserialize, Serialize};

use crate::{ErrorBody, ExpiringCertificate, HostSummary};

/// 默认同时执行的主机数：sshd 默认的 `MaxStartups 10:30:100` 下，经同一台跳板机超过 10 个未认证的连接会被随机丢弃。
pub const DEFAULT_PARALLEL: u32 = 10;
/// 同时执行的主机数的上限。
pub const MAX_PARALLEL: u32 = 64;
/// 一批最多的主机数。
pub const MAX_BATCH_HOSTS: usize = 1000;

/// 选哪些主机：`hosts` 列出的 ∪（各 `groups` 之并 ∩ 各 `tags` 之并），再减去 `exclude`；`all` 代替前两项。
/// 只给 `groups` 或只给 `tags` 时就是它们的并集。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Selector {
    /// 主机的 ID 或名称
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
    /// 分组的名称或路径（`生产/web`），含子分组
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    /// 标签（不区分大小写）
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// 排除的主机（ID 或名称）
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// 全部允许的主机
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub all: bool,
}

impl Selector {
    /// 什么也没选。
    pub fn is_empty(&self) -> bool {
        !self.all && self.hosts.is_empty() && self.groups.is_empty() && self.tags.is_empty()
    }
}

/// 在每台主机上做什么。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum BatchAction {
    /// 执行命令；`stdin` 是远程命令的标准输入（`--script` / `--input`），`stdin_script` 为真时它是脚本，
    /// 同样按危险命令规则检查
    Exec {
        command: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        stdin: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        stdin_script: bool,
    },
    /// 上传同一个本机文件（绝对路径），`remote` 与单台的 `upload` 相同
    Upload { local: String, remote: String },
    /// 下载：每台主机的文件放进 `local`（本机的绝对路径，目录）下以主机名命名的子目录
    Download { remote: String, local: String },
}

/// 批量执行的请求。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchRequest {
    pub selector: Selector,
    pub action: BatchAction,
    /// 同时执行的主机数；0 为默认（[`DEFAULT_PARALLEL`]），超过 [`MAX_PARALLEL`] 按上限
    #[serde(default)]
    pub parallel: u32,
    /// 每台命令的时限（毫秒，不含连接）；超时向命令发送 SIGTERM，状态为 `timeout`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// 失败（`failed`、`unreachable`、`timeout`）达到这么多台后不再开始新的主机，已开始的做完
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_fail: Option<u32>,
    /// 流式返回各台的结果（见模块说明）
    #[serde(default)]
    pub stream: bool,
    /// 调用方（同 [`crate::ExecRequest::caller`]）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caller: Vec<String>,
}

/// 一台主机的结果状态。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchStatus {
    /// 成功：退出码为 0（传输：完成）
    #[default]
    Ok,
    /// 执行了但没有成功：退出码不是 0，或者连上之后出错（服务器不允许执行命令、传输出错）
    Failed,
    /// 没能连上或登录（网络、认证、指纹未确认、凭据锁定）
    Unreachable,
    /// 超过时限，已向命令发送 SIGTERM
    Timeout,
    /// 执行中被取消
    Canceled,
    /// 没有执行：未允许、不是 SSH 主机、达到失败的上限后不再开始、开始前已取消
    Skipped,
    /// 本版本不认识的状态（更新版本的 Termlet 加的）
    #[serde(other)]
    Other,
}

impl BatchStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            BatchStatus::Ok => "ok",
            BatchStatus::Failed => "failed",
            BatchStatus::Unreachable => "unreachable",
            BatchStatus::Timeout => "timeout",
            BatchStatus::Canceled => "canceled",
            BatchStatus::Skipped => "skipped",
            BatchStatus::Other => "other",
        }
    }

    /// 是否算作“失败”（`--max-fail` 计数、退出码 8）。
    pub fn is_failure(self) -> bool {
        matches!(
            self,
            BatchStatus::Failed | BatchStatus::Unreachable | BatchStatus::Timeout
        )
    }
}

/// 一台主机的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchHostResult {
    pub id: String,
    pub name: String,
    pub status: BatchStatus,
    /// 远程命令的退出码（执行了的）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    /// 输出超过上限被截断
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// 用时（含连接），没有开始的为 0
    #[serde(default)]
    pub duration_ms: u64,
    /// 没有成功的原因（退出码不是 0 的没有）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
    /// 传输：本机路径
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub local: String,
    /// 传输：远程路径
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub remote: String,
    /// 传输：字节数
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// 传输：`sftp` 或 `scp`
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub protocol: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expiring_certificates: Vec<ExpiringCertificate>,
}

/// 各状态的台数与总用时。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct BatchSummary {
    pub total: u32,
    pub ok: u32,
    pub failed: u32,
    pub unreachable: u32,
    pub timeout: u32,
    pub canceled: u32,
    pub skipped: u32,
    pub duration_ms: u64,
}

impl BatchSummary {
    /// 计入一台。
    pub fn add(&mut self, status: BatchStatus) {
        self.total += 1;
        match status {
            BatchStatus::Ok => self.ok += 1,
            BatchStatus::Failed | BatchStatus::Other => self.failed += 1,
            BatchStatus::Unreachable => self.unreachable += 1,
            BatchStatus::Timeout => self.timeout += 1,
            BatchStatus::Canceled => self.canceled += 1,
            BatchStatus::Skipped => self.skipped += 1,
        }
    }

    /// 由各台的结果算出（用时另填）。
    pub fn of(results: &[BatchHostResult]) -> Self {
        let mut summary = Self::default();
        for result in results {
            summary.add(result.status);
        }
        summary
    }

    /// 执行了的主机都成功：没有失败、连不上、超时、取消，也没有因为失败的上限或取消而没开始的。因为未允许、
    /// 不是 SSH 主机而跳过的不算（`skipped_unrun` 是其中因为失败的上限或取消而没开始的台数）。
    pub fn succeeded(&self, skipped_unrun: u32) -> bool {
        self.failed == 0
            && self.unreachable == 0
            && self.timeout == 0
            && self.canceled == 0
            && skipped_unrun == 0
    }
}

/// 批量执行结束时的汇总。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchReport {
    /// 这一批的 ID（审计日志中同一批的记录都带着它）
    pub batch: String,
    /// 各台的结果（含跳过的），按选中的顺序；流式返回时为空（已逐台发过）
    #[serde(default)]
    pub hosts: Vec<BatchHostResult>,
    pub summary: BatchSummary,
    /// 命中危险命令规则、等用户批准了多久；没有询问时没有
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_wait_ms: Option<u64>,
}

/// 选中的主机：要执行的与跳过的（状态为 `skipped`，`error` 是原因）。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resolution {
    /// 这一批的 ID（`--list` 时为空）
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub batch: String,
    pub hosts: Vec<HostSummary>,
    #[serde(default)]
    pub skipped: Vec<BatchHostResult>,
}

/// 输出相同的一组（[`group_outputs`] 的结果）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputGroup {
    /// 组内的主机（`results` 中的下标，按原来的顺序）
    pub members: Vec<usize>,
}

/// 把执行了的主机按“退出码 + 标准输出 + 标准错误”分组（与 ClusterShell 的 `clush -b` 相同：退出码不同的不合并），
/// 人数多的组在前，同样多时按第一台的先后。没有退出码的（连不上、跳过等）不分组，另外返回。
pub fn group_outputs(results: &[BatchHostResult]) -> (Vec<OutputGroup>, Vec<usize>) {
    let mut groups: Vec<OutputGroup> = Vec::new();
    let mut others = Vec::new();
    for (i, result) in results.iter().enumerate() {
        let Some(code) = result.exit_code else {
            others.push(i);
            continue;
        };
        let same = groups.iter_mut().find(|g| {
            let first = &results[g.members[0]];
            first.exit_code == Some(code)
                && first.stdout == result.stdout
                && first.stderr == result.stderr
        });
        match same {
            Some(group) => group.members.push(i),
            None => groups.push(OutputGroup { members: vec![i] }),
        }
    }
    groups.sort_by(|a, b| {
        b.members
            .len()
            .cmp(&a.members.len())
            .then(a.members[0].cmp(&b.members[0]))
    });
    (groups, others)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ran(name: &str, code: i32, out: &str) -> BatchHostResult {
        BatchHostResult {
            id: format!("h_{name}"),
            name: name.into(),
            status: if code == 0 {
                BatchStatus::Ok
            } else {
                BatchStatus::Failed
            },
            exit_code: Some(code),
            stdout: out.into(),
            ..Default::default()
        }
    }

    #[test]
    fn statuses_have_fixed_names() {
        let all = [
            (BatchStatus::Ok, "ok"),
            (BatchStatus::Failed, "failed"),
            (BatchStatus::Unreachable, "unreachable"),
            (BatchStatus::Timeout, "timeout"),
            (BatchStatus::Canceled, "canceled"),
            (BatchStatus::Skipped, "skipped"),
        ];
        for (status, name) in all {
            assert_eq!(status.as_str(), name);
            assert_eq!(serde_json::to_value(status).unwrap(), name);
            assert_eq!(
                serde_json::from_value::<BatchStatus>(name.into()).unwrap(),
                status
            );
        }
        // 更新版本加的状态：不认识，按失败计
        let newer: BatchStatus = serde_json::from_value("paused".into()).unwrap();
        assert_eq!(newer, BatchStatus::Other);
        let mut summary = BatchSummary::default();
        summary.add(newer);
        assert_eq!(summary.failed, 1);
    }

    // 脚本读取的字段名不能改
    #[test]
    fn json_field_names() {
        let host = BatchHostResult {
            error: Some(ErrorBody {
                code: "connect_failed".into(),
                message: "refused".into(),
            }),
            ..ran("web-1", 3, "x\n")
        };
        assert_eq!(
            serde_json::to_value(&host).unwrap(),
            serde_json::json!({"id": "h_web-1", "name": "web-1", "status": "failed", "exitCode": 3,
                "stdout": "x\n", "stderr": "", "durationMs": 0,
                "error": {"code": "connect_failed", "message": "refused"}})
        );
        let summary = BatchSummary {
            total: 3,
            ok: 1,
            failed: 1,
            unreachable: 1,
            duration_ms: 5,
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&summary).unwrap(),
            serde_json::json!({"total": 3, "ok": 1, "failed": 1, "unreachable": 1, "timeout": 0,
                "canceled": 0, "skipped": 0, "durationMs": 5})
        );
        let request = BatchRequest {
            selector: Selector {
                groups: vec!["生产".into()],
                ..Default::default()
            },
            action: BatchAction::Exec {
                command: "sh -s --".into(),
                stdin: "echo hi\n".into(),
                stdin_script: true,
            },
            parallel: 0,
            timeout_ms: Some(1000),
            max_fail: None,
            stream: true,
            caller: Vec::new(),
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(
            json["action"],
            serde_json::json!({"kind": "exec", "command": "sh -s --", "stdin": "echo hi\n", "stdinScript": true})
        );
        assert_eq!(json["selector"], serde_json::json!({"groups": ["生产"]}));
        assert_eq!(
            serde_json::from_value::<BatchRequest>(json).unwrap(),
            request
        );
    }

    #[test]
    fn groups_by_exit_code_and_output() {
        let mut results = vec![
            ran("a", 0, "active\n"),
            ran("b", 3, "inactive\n"),
            ran("c", 0, "active\n"),
            BatchHostResult {
                status: BatchStatus::Unreachable,
                ..Default::default()
            },
            ran("e", 1, "active\n"),
            ran("f", 3, "inactive\n"),
            ran("g", 0, "active\n"),
        ];
        let (groups, others) = group_outputs(&results);
        let members: Vec<Vec<usize>> = groups.into_iter().map(|g| g.members).collect();
        // 同样的输出、退出码不同时不合并；人数多的在前
        assert_eq!(members, vec![vec![0, 2, 6], vec![1, 5], vec![4]]);
        assert_eq!(others, vec![3]);
        // 标准错误不同也不合并
        results[2].stderr = "warn".into();
        let (groups, _) = group_outputs(&results);
        assert_eq!(groups[0].members, vec![0, 6]);
    }

    #[test]
    fn summary_success() {
        let mut summary = BatchSummary::default();
        summary.add(BatchStatus::Ok);
        summary.add(BatchStatus::Skipped);
        assert!(summary.succeeded(0), "因为未允许而跳过的不算失败");
        assert!(!summary.succeeded(1), "因为失败的上限而没开始的算");
        summary.add(BatchStatus::Timeout);
        assert!(!summary.succeeded(0));
        assert!(BatchStatus::Unreachable.is_failure());
        assert!(!BatchStatus::Canceled.is_failure());
    }
}
