//! 一批传输与结束时的汇总（由核心跟踪，界面只管显示）。
//!
//! 一批：同一个窗口、同一台主机上，从有任务开始（新建，或继续失败的任务）到这台主机的任务全部结束（成功、失败、取消，
//! 或已从列表中移除）。期间新建、继续的任务加入同一批。因断线失败、正在自动重连的任务还没结束。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{Direction, TransferId, TransferState, TransferView};
use crate::sftp::path;

/// 批次的编号（管理器内唯一）。
pub type BatchId = u64;

/// 只有一项、在这个时间内成功完成的不必提示：传输列表、文件列表上已经看得到，提示反而打扰。
pub const QUIET: Duration = Duration::from_secs(3);

/// 结束的一批。
#[derive(Clone, Debug)]
pub struct BatchReport {
    pub owner: String,
    pub host: String,
    pub batch: BatchId,
    /// 这一批的任务（最后的状态；已从列表中移除的取移除前的）
    pub transfers: Vec<TransferView>,
    /// 用时：从开始到最后一个任务结束
    pub elapsed: Duration,
    pub summary: BatchSummary,
}

impl BatchReport {
    /// 是否弹提示：全部是取消的不提示（用户自己取消的，或关闭了会话）；只有一项、很快成功完成的不提示；
    /// 有失败或跳过项的总是提示（自动清除打开时，传输列表中不一定还看得到）。
    pub fn should_announce(&self) -> bool {
        let s = &self.summary;
        let succeeded = s.uploaded + s.downloaded;
        if succeeded == 0 && s.failed == 0 {
            return false;
        }
        if s.failed > 0 || s.skipped + s.skipped_special > 0 {
            return true;
        }
        !(self.transfers.len() == 1 && self.elapsed < QUIET)
    }
}

/// 一批的汇总：界面据此写"已下载 3 项到 D:\Downloads（共 1.2 GB）；1 项失败……"。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BatchSummary {
    pub uploaded: u32,
    pub downloaded: u32,
    pub failed: u32,
    pub canceled: u32,
    /// 成功完成的目录传输中因为没有读权限而跳过的项数
    pub skipped: u32,
    /// 成功完成的目录传输中因为文件类型不支持（符号链接、设备文件等）而跳过的项数
    pub skipped_special: u32,
    /// 成功传输的字节数
    pub bytes: u64,
    /// 成功上传到的远程目录（都在同一个目录时）
    pub upload_dir: Option<String>,
    /// 成功下载到的本机目录（都在同一个目录时）
    pub download_dir: Option<PathBuf>,
    /// 最后一项成功下载的本机路径（"打开文件夹"时选中它）
    pub last_download: Option<PathBuf>,
}

impl BatchSummary {
    pub fn of(transfers: &[TransferView]) -> Self {
        let mut s = Self::default();
        let mut upload_dirs: Vec<String> = Vec::new();
        let mut download_dirs: Vec<PathBuf> = Vec::new();
        for t in transfers {
            match t.state {
                TransferState::Failed | TransferState::WaitingReconnect => s.failed += 1,
                TransferState::Canceled => s.canceled += 1,
                _ => {}
            }
            if t.state != TransferState::Done {
                continue;
            }
            s.skipped_special += t.skipped_special;
            s.skipped += t.skipped.saturating_sub(t.skipped_special);
            s.bytes += t.total;
            match t.direction {
                Direction::Upload => {
                    s.uploaded += 1;
                    upload_dirs.push(path::parent(&t.remote));
                }
                Direction::Download => {
                    s.downloaded += 1;
                    download_dirs.push(t.local.parent().unwrap_or(&t.local).to_path_buf());
                    s.last_download = Some(t.local.clone());
                }
            }
        }
        if let Some(first) = upload_dirs.first()
            && upload_dirs.iter().all(|d| d == first)
        {
            s.upload_dir = Some(first.clone());
        }
        if let Some(first) = download_dirs.first()
            && download_dirs.iter().all(|d| same_local_path(d, first))
        {
            s.download_dir = Some(first.clone());
        }
        s
    }
}

/// 两个本机路径是否相同（Windows 不区分大小写、`\` 与 `/` 相同，忽略末尾的分隔符）。
fn same_local_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        let text = p.to_string_lossy();
        let text = text.trim_end_matches(['\\', '/']);
        if cfg!(windows) {
            text.replace('/', "\\").to_lowercase()
        } else {
            text.to_string()
        }
    };
    norm(a) == norm(b)
}

struct OpenBatch {
    id: BatchId,
    members: Vec<TransferId>,
    /// 各任务最后见到的状态：较早结束的任务在这一批结束前就可能已从列表中移除，汇总时取这里的
    seen: HashMap<TransferId, TransferView>,
    started: i64,
}

/// 进行中的各批（按窗口与主机）。
#[derive(Default)]
pub(crate) struct BatchBook {
    next: BatchId,
    open: HashMap<(String, String), OpenBatch>,
}

impl BatchBook {
    /// 任务加入它所在主机的批（没有时开始新的一批），返回批次。
    pub(crate) fn join(
        &mut self,
        owner: &str,
        host: &str,
        id: &TransferId,
        now_ms: i64,
    ) -> BatchId {
        let key = (owner.to_string(), host.to_string());
        if !self.open.contains_key(&key) {
            self.next += 1;
            self.open.insert(
                key.clone(),
                OpenBatch {
                    id: self.next,
                    members: Vec::new(),
                    seen: HashMap::new(),
                    started: now_ms,
                },
            );
        }
        let batch = self.open.get_mut(&key).expect("刚插入");
        if !batch.members.contains(id) {
            batch.members.push(id.clone());
        }
        batch.id
    }

    /// 这些还没结束的任务换了窗口（Tab 移到别的窗口）：从 `from` 窗口这台主机的那一批中取出（取出后空了的一批作废，
    /// 不汇总），加入 `to` 窗口这台主机的一批，按 `ids` 的顺序返回各自新的批次。
    pub(crate) fn reassign(
        &mut self,
        ids: &[TransferId],
        from: &str,
        to: &str,
        host: &str,
        now_ms: i64,
    ) -> Vec<BatchId> {
        let key = (from.to_string(), host.to_string());
        if let Some(batch) = self.open.get_mut(&key) {
            batch.members.retain(|id| !ids.contains(id));
            batch.seen.retain(|id, _| !ids.contains(id));
            if batch.members.is_empty() {
                self.open.remove(&key);
            }
        }
        ids.iter()
            .map(|id| self.join(to, host, id, now_ms))
            .collect()
    }

    /// 记下任务的最后状态（从列表中移除前）。
    pub(crate) fn remember(&mut self, view: &TransferView) {
        for batch in self.open.values_mut() {
            if batch.members.contains(&view.id) {
                batch.seen.insert(view.id.clone(), view.clone());
            }
        }
    }

    /// 按最新的传输列表推进，返回结束的批。
    pub(crate) fn advance(&mut self, views: &[TransferView], now_ms: i64) -> Vec<BatchReport> {
        let current: HashMap<&TransferId, &TransferView> =
            views.iter().map(|v| (&v.id, v)).collect();
        let mut ended = Vec::new();
        self.open.retain(|(owner, host), batch| {
            for id in &batch.members {
                if let Some(view) = current.get(id) {
                    batch.seen.insert(id.clone(), (*view).clone());
                }
            }
            let pending = batch
                .members
                .iter()
                .any(|id| current.get(id).is_some_and(|v| v.state.is_pending()));
            if pending {
                return true;
            }
            let transfers: Vec<TransferView> = batch
                .members
                .iter()
                .filter_map(|id| batch.seen.get(id).cloned())
                .collect();
            // 结束的时间取最后结束的任务的时间
            let finished = transfers.iter().filter_map(|t| t.finished).max();
            let end = finished
                .map(|f| f.clamp(batch.started, now_ms.max(batch.started)))
                .unwrap_or(now_ms);
            let summary = BatchSummary::of(&transfers);
            ended.push(BatchReport {
                owner: owner.clone(),
                host: host.clone(),
                batch: batch.id,
                elapsed: Duration::from_millis((end - batch.started).max(0) as u64),
                transfers,
                summary,
            });
            false
        });
        ended
    }
}
