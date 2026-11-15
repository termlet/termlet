//! 云同步。
//!
//! 在多台设备之间同步主机、分组、凭据档案、密钥库、快捷命令、端口转发、工作区、终端配色、同步的设置组，
//! 以及它们的秘密（密码、私钥、TOTP 密钥等）。数据放在用户自己的位置：本机文件夹（可以是 OneDrive、坚果云等网盘的
//! 同步目录）、WebDAV、S3 兼容存储（见 `s3`）或 GitHub Gist（见 `gist`）。云端只有一个同步文件（Gist 中按段拆开存放），
//! 用各设备共用的同步密码加密（见 `document`），看不到任何内容。各种位置都实现 `backend::Backend`：读取时给出版本标记，
//! 写入时只在仍是那个版本时写入。
//! 秘密从本机保险库解密后放进同步文件，整个文件再用同步密码加密；写入本机时重新用本机保险库的主密钥加密。
//!
//! 合并按条目进行，与本机上次同步时的基准比较（见 `merge`）；写回云端时带上读取时的版本标记，其他设备先写入时
//! 重新拉取再合并（不会整份覆盖）。保护：
//!
//! - 首次同步而本机与云端都有数据时，由用户选择合并、以云端为准或以本机为准（[`Pending::FirstSync`]）；
//! - 一次要删除较多数据（至少 5 条且占三成）时先请用户确认（[`Pending::ConfirmDeletes`]）；
//! - 同步文件夹不存在时不同步；同步过而云端没有同步文件时以本机为准重新上传，不当作云端删除了全部数据；
//! - 本机上次写到云端的内容不在云端时（网盘以其他设备基于旧文件的写入为准、云端回退），不采纳云端的删除与旧版本，
//!   本机删除了的重新删除，重新上传；
//! - 本机不认识、写不进的记录（来自其他版本）不写入本机，也不把本机的值推上云端，升级后重新尝试写入；
//! - 写入其他设备的修改之前保存本机快照（[`crate::backup::SnapshotReason::Sync`]），可以回退；读取之后用户修改了
//!   本机数据时重新合并，不覆盖；写入本机在一个事务中完成，失败时什么都没写；
//! - 同步密码错误、云端格式来自更新版本时不写入云端；
//! - 本机修改后约 20 秒、以及每隔一段时间（默认 10 分钟）自动同步（见 [`CloudSync::spawn_auto`]）。
//!
//! # 用法（界面）
//!
//! 启动时（保险库解锁前后都可以）用存储与 [`Backups`] 建一个 [`CloudSync`]（克隆很便宜，各处共用），调用一次
//! [`CloudSync::spawn_auto`]。其余都是异步函数，必须在核心的运行时上执行（`crate::runtime::spawn`，返回的 future 是
//! `Send`）；同一时间只进行一项（同步、设置、停止），其余等待。
//!
//! - 状态：[`CloudSync::subscribe`] 得到 `watch` 接收端（与执行器无关，GPUI 的任务里可以直接等待），
//!   [`SyncStatus`] 中有设置、是否正在同步、上次同步的时间与结果、错误、等待决定的事、冲突与设备列表。
//! - 设置：先 [`CloudSync::test_connection`]（检查位置与账号；位置中已有同步文件时校验同步密码，并返回其中的数据量与
//!   设备，界面据此决定是否要两次输入密码），再 [`CloudSync::setup`] 保存（不立即同步），之后调 [`CloudSync::sync_now`]。
//! - 立即同步：[`CloudSync::sync_now`]；返回 [`SyncOutcome::Pending`] 时把选择交给用户，用 [`CloudSync::answer`] 回答
//!   后继续（等待期间自动同步暂停）。
//! - 冲突：自动解决（较新的修改胜出；一边删除、一边修改时保留修改），列在 [`SyncStatus::conflicts`] 中；
//!   用户可以 [`CloudSync::use_other_version`] 改用落败的一边（作为本机的新修改，下次同步时上传），或
//!   [`CloudSync::dismiss_conflicts`]。
//! - 自动同步的开关与间隔：[`CloudSync::set_options`]；停止同步：[`CloudSync::disable`]（云端的文件不动）。
//! - 写入本机后存储照常发出变更事件，各视图重新读取；被删除的端口转发规则由转发管理器自己停止。

mod auto;
mod backend;
mod document;
mod error;
mod gist;
mod http;
mod merge;
mod records;
mod s3;
mod sigv4;
mod state;
mod webdav;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, watch};

pub use document::FILE_NAME;
pub use error::SyncError;
pub use gist::MAX_SIZE as GIST_MAX_SIZE;
pub use s3::DEFAULT_REGION as S3_DEFAULT_REGION;

use backend::{Backend, Fetch, Folder, Stored};
use document::{Document, MAX_DOCUMENT_SIZE};
use merge::{Bases, ConflictEntry, Mode, PlanOpts, applied_bases, make_plan, next_bases, too_many};
use records::{Locals, Remote, counts, version_time};
use state::{Deletion, Persisted, Secrets, Wrote};

use crate::backup::{Backups, SnapshotReason, envelope};
use crate::id::{ForwardId, GroupId, HostId, KeyId, ProfileId, SchemeId, SnippetId, WorkspaceId};
use crate::model::{
    ColorScheme, Forward, Group, Host, KeyEntry, Kind, Profile, Snippet, Workspace,
};
use crate::secret::SecretString;
use crate::store::Store;
use crate::store::sync::ApplyOutcome;

/// 默认的自动同步间隔（分钟）。
pub const DEFAULT_INTERVAL_MINUTES: u32 = 10;
/// 自动同步间隔的范围（分钟）。
pub const INTERVAL_RANGE: std::ops::RangeInclusive<u32> = 1..=1440;
/// 设备名称的最大长度（字符数）。
pub const MAX_DEVICE_NAME: usize = 64;
/// 同步密码的最小长度（与备份密码相同）。
pub const MIN_PASSWORD_LEN: usize = envelope::MIN_PASSWORD_LEN;

/// 一次同步中最多重试几轮（其他设备先写入、本机在同步期间被修改）。
const MAX_ATTEMPTS: usize = 3;
/// 本机最后一次写入超过这么久时，即使没有变化也写入一次（更新设备列表中的时间）。
const REFRESH_DEVICE_MS: i64 = 24 * 3600 * 1000;

// ---------------------------------------------------------------- 公开的类型

/// 同步位置。都属于本机的设置（存在本机的数据库中，不同步）；密码、密钥、令牌另存在保险库中。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SyncTarget {
    /// 本机文件夹（绝对路径；可以是网盘的同步目录、U 盘）
    Folder { path: PathBuf },
    /// WebDAV 目录，例如 `https://dav.jianguoyun.com/dav/termlet/`。`allow_http`：允许 `http://` 的地址
    /// （局域网中的 NAS 等；默认要求 HTTPS）。旧的设置中没有这一项时当作已允许：升级前已经在用 HTTP 的不会突然同步失败
    #[serde(rename = "webdav", rename_all = "camelCase")]
    WebDav {
        url: String,
        username: String,
        #[serde(default = "legacy_allow_http")]
        allow_http: bool,
    },
    /// S3 兼容存储：终端节点（如 `https://s3.amazonaws.com`、`http://192.168.1.10:9000`）、区域（留空为
    /// [`S3_DEFAULT_REGION`]）、桶、桶中的路径前缀（可以为空）、访问密钥 ID；秘密访问密钥另存在保险库中。
    /// `path_style`：用路径风格的地址（`终端节点/桶/键`，MinIO 等需要）；`allow_http` 同 WebDAV
    #[serde(rename_all = "camelCase")]
    S3 {
        endpoint: String,
        #[serde(default)]
        region: String,
        bucket: String,
        #[serde(default)]
        prefix: String,
        access_key_id: String,
        #[serde(default)]
        path_style: bool,
        #[serde(default)]
        allow_http: bool,
    },
    /// GitHub Gist（私密）。设置时留空表示在令牌所属的账号中查找 Termlet 的 Gist，没有就新建一个；保存的设置中
    /// 总是有 ID。个人访问令牌另存在保险库中
    #[serde(rename_all = "camelCase")]
    Gist {
        #[serde(default)]
        gist_id: String,
    },
}

fn legacy_allow_http() -> bool {
    true
}

/// 同步位置的种类（界面按它选用错误的说法）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    Folder,
    WebDav,
    S3,
    Gist,
}

impl SyncTarget {
    pub fn service(&self) -> Service {
        match self {
            SyncTarget::Folder { .. } => Service::Folder,
            SyncTarget::WebDav { .. } => Service::WebDav,
            SyncTarget::S3 { .. } => Service::S3,
            SyncTarget::Gist { .. } => Service::Gist,
        }
    }

    /// 种类的名称（日志、诊断信息用；不含地址与账号）。
    pub fn kind_name(&self) -> &'static str {
        match self {
            SyncTarget::Folder { .. } => "folder",
            SyncTarget::WebDav { .. } => "webdav",
            SyncTarget::S3 { .. } => "s3",
            SyncTarget::Gist { .. } => "gist",
        }
    }

    /// 显示用的位置：文件夹的路径、WebDAV 的地址、`s3://桶/前缀`、Gist 的网址。
    pub fn location(&self) -> String {
        match self {
            SyncTarget::Folder { path } => path.display().to_string(),
            SyncTarget::WebDav { url, .. } => url.clone(),
            SyncTarget::S3 { bucket, prefix, .. } => {
                if prefix.is_empty() {
                    format!("s3://{bucket}")
                } else {
                    format!("s3://{bucket}/{prefix}")
                }
            }
            SyncTarget::Gist { gist_id } => format!("https://gist.github.com/{gist_id}"),
        }
    }
}

/// 同步的设置（不含密码）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncConfig {
    pub target: SyncTarget,
    /// 自动同步
    pub auto: bool,
    /// 自动同步的间隔（分钟，见 [`INTERVAL_RANGE`]）
    pub interval_minutes: u32,
    /// 本机在设备列表中的名称
    pub device_name: String,
}

/// 设置同步的输入。
#[derive(Clone, Debug)]
pub struct SetupInput {
    pub target: SyncTarget,
    /// 同步位置的凭据：WebDAV 的密码（坚果云等为应用密码）、S3 的秘密访问密钥、GitHub 的个人访问令牌；
    /// 本机文件夹不用，留空即可
    pub credential: SecretString,
    /// 同步密码：各设备相同，用于加密同步文件（至少 [`MIN_PASSWORD_LEN`] 个字符）。同步位置中已有同步文件时
    /// 必须能用它解开
    pub password: SecretString,
    /// 本机的名称；留空用电脑名
    pub device_name: String,
}

/// 同步位置中已有的数据。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteInfo {
    /// 已有同步文件
    pub exists: bool,
    pub devices: Vec<Device>,
    pub counts: SyncCounts,
}

/// 同步过的一台设备。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub id: String,
    pub name: String,
    /// 最近一次写入时的 Termlet 版本
    #[serde(default)]
    pub app: String,
    /// 最近一次写入同步文件的时间（Unix 毫秒）
    pub last_sync: i64,
}

/// 各类数据的条数（首次同步时给用户对比）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SyncCounts {
    pub hosts: usize,
    pub groups: usize,
    /// 凭据档案
    pub profiles: usize,
    /// 密钥库中的密钥
    pub keys: usize,
    pub snippets: usize,
    pub forwards: usize,
    pub workspaces: usize,
    /// 自定义的终端配色
    pub schemes: usize,
}

impl SyncCounts {
    pub fn total(&self) -> usize {
        self.hosts
            + self.groups
            + self.profiles
            + self.keys
            + self.snippets
            + self.forwards
            + self.workspaces
            + self.schemes
    }
}

/// 冲突中采用了哪一边。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    /// 本机的
    Local,
    /// 云端的（其他设备的）
    Remote,
}

/// 冲突的数据是什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordKind {
    Host,
    Group,
    Profile,
    Key,
    Snippet,
    Forward,
    Workspace,
    Scheme,
    /// 一组设置（名称为组名：general、terminal……）
    Settings,
}

impl RecordKind {
    /// 记录键对应的种类；秘密与不认识的记录为 `None`（秘密随所属的条目显示，不单独列出）。
    fn of_key(key: &str) -> Option<Self> {
        Some(match records::kind_of(key) {
            records::SETTING => RecordKind::Settings,
            kind => match records::item_kind(kind)? {
                Kind::Host => RecordKind::Host,
                Kind::Group => RecordKind::Group,
                Kind::Profile => RecordKind::Profile,
                Kind::Key => RecordKind::Key,
                Kind::Snippet => RecordKind::Snippet,
                Kind::Forward => RecordKind::Forward,
                Kind::Workspace => RecordKind::Workspace,
                Kind::Scheme => RecordKind::Scheme,
            },
        })
    }
}

/// 两边都修改了的一条数据（已按规则解决）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Conflict {
    /// 记录键（[`CloudSync::use_other_version`] 用），界面不需要解读
    pub key: String,
    pub kind: RecordKind,
    /// 条目的名称（设置为组名）
    pub name: String,
    /// 采用了哪一边
    pub winner: Side,
    /// 其中一边删除了它（保留了修改的一边）
    pub deleted: bool,
    /// 还能改用另一边（落败一边的内容只保存在内存中，重启后不能再改用）
    pub can_use_other: bool,
}

/// 一次完成的同步。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SyncReport {
    /// Unix 毫秒
    pub at: i64,
    /// 写入本机、写到云端的记录数
    pub pulled: usize,
    pub pushed: usize,
    pub conflicts: Vec<Conflict>,
    /// 同步过而云端没有同步文件（被删除、网盘还没有下载下来）：没有当作其他设备删除了全部数据，以本机为准重新上传了
    pub reuploaded: bool,
    /// 本机上次写到云端的内容不在云端（被其他设备的旧文件覆盖、云端回退）：没有采纳云端的删除与旧版本，重新上传了
    pub lost_write: bool,
}

/// 需要用户决定的事（这时什么都没改）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Pending {
    /// 首次同步，本机与云端都有数据：请用户选择 [`Answer::Merge`]、[`Answer::UseRemote`] 或 [`Answer::UseLocal`]。
    /// `duplicates` 是两边各自添加的同一台主机（协议、地址、端口、用户名相同）的数量，合并后会有两份
    #[serde(rename_all = "camelCase")]
    FirstSync {
        local: SyncCounts,
        remote: SyncCounts,
        duplicates: usize,
    },
    /// 要删除较多数据：在本机、云端删除的条数。用户确认后回答 [`Answer::ConfirmDeletes`]
    #[serde(rename_all = "camelCase")]
    ConfirmDeletes { local: usize, remote: usize },
}

/// 对 [`Pending`] 的回答。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    /// 首次同步：合并两边（同一条数据两边都改了时采用较新的）
    Merge,
    /// 首次同步：以云端为准（本机改为与云端相同）
    UseRemote,
    /// 首次同步：以本机为准（云端改为与本机相同）
    UseLocal,
    /// 确认删除较多数据
    ConfirmDeletes,
}

/// 一次同步的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncOutcome {
    Done(SyncReport),
    /// 需要用户决定，什么都没改
    Pending(Pending),
}

/// 同步的状态（[`CloudSync::subscribe`]）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncStatus {
    /// `None` 表示还没有设置同步
    pub config: Option<SyncConfig>,
    /// 本机的设备 ID（设备列表中标出"本机"）
    pub device_id: Option<String>,
    pub syncing: bool,
    /// 上次成功同步的时间（Unix 毫秒）
    pub last_sync: Option<i64>,
    /// 上次同步失败的原因（之后成功了就清除）
    pub last_error: Option<SyncError>,
    pub last_report: Option<SyncReport>,
    /// 等待用户决定的事；有时自动同步暂停
    pub pending: Option<Pending>,
    /// 最近一次有冲突的同步中的冲突
    pub conflicts: Vec<Conflict>,
    /// 同步过的设备，最近同步的在前
    pub devices: Vec<Device>,
}

// ---------------------------------------------------------------- 服务

/// 云同步服务。克隆很便宜，各处共用同一个。
#[derive(Clone)]
pub struct CloudSync {
    inner: Arc<Inner>,
}

struct Inner {
    store: Arc<Store>,
    backups: Option<Backups>,
    /// 同一时间只进行一项（同步、设置、停止）
    lock: tokio::sync::Mutex<()>,
    status: watch::Sender<SyncStatus>,
    persisted: Mutex<Persisted>,
    /// 最近一次有冲突的同步中的冲突（含落败一边的内容）
    conflicts: Mutex<Vec<ConflictEntry>>,
    /// 单调递增的时钟（Unix 毫秒）：同一毫秒内的两次写入也能分出先后
    clock: Mutex<i64>,
    /// 上次同步成功后本机数据的指纹（自动同步据此判断本机修改后是否真有变化）
    fingerprint: Mutex<Option<String>>,
    /// 设置变了：自动同步重新计算下一次的时间
    wake: Notify,
    #[cfg(test)]
    hooks: Mutex<tests::Hooks>,
}

/// 在阻塞线程上执行（数据库、派生密钥、压缩）。
pub(crate) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, SyncError> + Send + 'static,
) -> Result<T, SyncError> {
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => Err(SyncError::Local {
            message: error.to_string(),
        }),
    }
}

/// 电脑名（设备名称的默认值）。
fn default_device_name() -> String {
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|var| std::env::var(var).ok().filter(|v| !v.trim().is_empty()))
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "Termlet".into())
}

impl CloudSync {
    /// 读出本机的同步设置与上次的状态（只读几个 `meta` 键）。`backups` 用于写入其他设备的修改之前保存快照，
    /// 为 `None` 时不保存（测试用）。保险库已解锁而同步密码解不开（保险库重置过）时停止同步。
    pub fn new(store: Arc<Store>, backups: Option<Backups>) -> Self {
        let db = store.database();
        if store.vault().is_unlocked() {
            // 只为发现保险库重置过：解不开时 load 会删除全部同步状态
            let _ = state::load(&store);
        }
        let config = state::load_config(db).ok().flatten();
        let persisted = state::load_persisted(db);
        let device_id = state::existing_device_id(db);
        let status = SyncStatus {
            config,
            device_id,
            ..Default::default()
        };
        let this = Self {
            inner: Arc::new(Inner {
                store,
                backups,
                lock: tokio::sync::Mutex::new(()),
                status: watch::Sender::new(status),
                persisted: Mutex::new(persisted),
                conflicts: Mutex::new(Vec::new()),
                clock: Mutex::new(0),
                fingerprint: Mutex::new(None),
                wake: Notify::new(),
                #[cfg(test)]
                hooks: Mutex::new(tests::Hooks::default()),
            }),
        };
        this.publish(|_| {});
        this
    }

    /// 订阅状态的变化。
    pub fn subscribe(&self) -> watch::Receiver<SyncStatus> {
        self.inner.status.subscribe()
    }

    /// 当前的状态。
    pub fn status(&self) -> SyncStatus {
        self.inner.status.borrow().clone()
    }

    fn store(&self) -> &Arc<Store> {
        &self.inner.store
    }

    fn app(&self) -> String {
        #[cfg(test)]
        if let Some(app) = self.hooks().app.clone() {
            return app;
        }
        crate::VERSION.to_string()
    }

    /// 单调递增的现在（Unix 毫秒）。
    fn now(&self) -> i64 {
        let mut last = self.inner.clock.lock().unwrap_or_else(|e| e.into_inner());
        let now = crate::time::now_ms().max(*last + 1);
        *last = now;
        now
    }

    /// 把内存中的状态（上次的结果、冲突）合进 [`SyncStatus`] 并通知订阅者；`edit` 修改其余的字段。
    fn publish(&self, edit: impl FnOnce(&mut SyncStatus)) {
        let persisted = self.persisted().clone();
        let live = self.live_conflicts();
        self.inner.status.send_modify(|status| {
            edit(status);
            status.last_sync = persisted.last_sync;
            status.last_error = persisted.last_error.clone();
            status.last_report = persisted.last_report.clone();
            status.pending = persisted.pending.clone();
            status.devices = persisted.devices.clone();
            status.conflicts = if live.is_empty() {
                persisted.conflicts.clone()
            } else {
                live
            };
        });
    }

    fn persisted(&self) -> std::sync::MutexGuard<'_, Persisted> {
        self.inner
            .persisted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn live_conflicts(&self) -> Vec<Conflict> {
        self.inner
            .conflicts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|c| conflict_info(c, true))
            .collect()
    }

    /// 修改并保存上次的状态。
    async fn update_persisted(&self, edit: impl FnOnce(&mut Persisted)) {
        let snapshot = {
            let mut persisted = self.persisted();
            edit(&mut persisted);
            persisted.clone()
        };
        let store = self.store().clone();
        if let Err(e) = blocking(move || state::save_persisted(store.database(), &snapshot)).await {
            tracing::warn!("保存同步状态失败：{e}");
        }
    }

    // ---------------------------------------------------------------- 设置

    /// 检查同步位置与账号；位置中已有同步文件时用同步密码解开它，返回其中的数据量与设备。不保存。
    /// Gist 没有填写 ID 时只查找已有的 Termlet Gist，不新建（没有时返回 `exists` 为 false）。
    pub async fn test_connection(&self, input: &SetupInput) -> Result<RemoteInfo, SyncError> {
        self.probe(input, false).await.map(|(_, info)| info)
    }

    /// 检查（同 [`CloudSync::test_connection`]）后保存设置与密码，不立即同步。重新设置时清除本机的同步基准：
    /// 下次同步按首次同步处理。自动同步的开关与间隔沿用原来的。Gist 没有填写 ID、账号中也没有 Termlet 的 Gist 时
    /// 新建一个私密 Gist，保存它的 ID。
    pub async fn setup(&self, input: SetupInput) -> Result<RemoteInfo, SyncError> {
        let _guard = self.inner.lock.lock().await;
        let (config, info) = self.probe(&input, true).await?;
        let secrets = Secrets {
            password: input.password.clone(),
            credential: input.credential.clone(),
        };
        let persisted = Persisted {
            devices: info.devices.clone(),
            ..Default::default()
        };
        let store = self.store().clone();
        let saved = (config.clone(), persisted.clone());
        let device =
            blocking(move || state::save_setup(&store, &saved.0, &secrets, &saved.1)).await?;
        *self.persisted() = persisted;
        self.inner
            .conflicts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        *self
            .inner
            .fingerprint
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        self.publish(|s| {
            s.config = Some(config);
            s.device_id = Some(device);
        });
        self.inner.wake.notify_one();
        tracing::info!(exists = info.exists, "已设置云同步");
        Ok(info)
    }

    async fn probe(
        &self,
        input: &SetupInput,
        create: bool,
    ) -> Result<(SyncConfig, RemoteInfo), SyncError> {
        envelope::check_password(&input.password)?;
        let mut device_name = input.device_name.trim().to_string();
        if device_name.is_empty() {
            device_name = default_device_name();
        }
        if device_name.chars().count() > MAX_DEVICE_NAME {
            return Err(SyncError::invalid("device_name", "too_long"));
        }
        let target = match &input.target {
            SyncTarget::Folder { path } => {
                if !path.is_absolute() {
                    return Err(SyncError::invalid("path", "not_absolute"));
                }
                SyncTarget::Folder { path: path.clone() }
            }
            SyncTarget::WebDav {
                url,
                username,
                allow_http,
            } => SyncTarget::WebDav {
                url: webdav::parse_dir(url, *allow_http)?.to_string(),
                username: username.trim().to_string(),
                allow_http: *allow_http,
            },
            SyncTarget::S3 {
                endpoint,
                region,
                bucket,
                prefix,
                access_key_id,
                path_style,
                allow_http,
            } => {
                let l = s3::normalize(
                    endpoint,
                    region,
                    bucket,
                    prefix,
                    access_key_id,
                    *path_style,
                    *allow_http,
                )?;
                if input.credential.expose().trim().is_empty() {
                    return Err(SyncError::invalid("secret_key", "required"));
                }
                SyncTarget::S3 {
                    endpoint: l.endpoint.to_string(),
                    region: l.region,
                    bucket: l.bucket,
                    prefix: l.prefix,
                    access_key_id: l.access_key_id,
                    path_style: *path_style,
                    allow_http: *allow_http,
                }
            }
            SyncTarget::Gist { gist_id } => {
                let mut gist_id = gist::parse_gist_id(gist_id)?;
                if gist_id.is_empty() {
                    let account = gist::Account::new(&self.gist_api(), input.credential.clone())?;
                    let found = account.find().await?;
                    match found.as_slice() {
                        [one] => gist_id = one.clone(),
                        [] if create => {
                            gist_id = account.create().await?;
                            tracing::info!("已新建私密 Gist 存放同步文件");
                        }
                        // 测试连接：账号中还没有 Termlet 的 Gist（开始同步时新建），令牌可用
                        [] => {
                            return Ok((
                                SyncConfig {
                                    target: SyncTarget::Gist {
                                        gist_id: String::new(),
                                    },
                                    auto: true,
                                    interval_minutes: DEFAULT_INTERVAL_MINUTES,
                                    device_name,
                                },
                                RemoteInfo::default(),
                            ));
                        }
                        _ => return Err(SyncError::GistAmbiguous { ids: found }),
                    }
                }
                SyncTarget::Gist { gist_id }
            }
        };
        let store = self.store().clone();
        let old = blocking(move || state::load_config(store.database())).await?;
        let config = SyncConfig {
            target,
            auto: old.as_ref().is_none_or(|c| c.auto),
            interval_minutes: old
                .as_ref()
                .map_or(DEFAULT_INTERVAL_MINUTES, |c| c.interval_minutes),
            device_name,
        };
        let backend = self.backend(&config, &input.credential)?;
        backend.check().await?;
        let mut info = RemoteInfo::default();
        if let Fetch::Found { data, .. } = backend.get().await? {
            let password = input.password.clone();
            let doc = blocking(move || document::open(&data, &password, MAX_DOCUMENT_SIZE)).await?;
            info.exists = true;
            info.devices = device_list(&doc.devices);
            info.counts = counts(doc.records.keys());
        }
        Ok((config, info))
    }

    /// 停止同步：删除本机的同步设置、密码、基准与状态。云端的同步文件不动。
    pub async fn disable(&self) -> Result<(), SyncError> {
        let _guard = self.inner.lock.lock().await;
        let store = self.store().clone();
        blocking(move || state::clear(store.database())).await?;
        *self.persisted() = Persisted::default();
        self.inner
            .conflicts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.publish(|s| s.config = None);
        self.inner.wake.notify_one();
        tracing::info!("已停止云同步");
        Ok(())
    }

    /// 修改自动同步的开关与间隔（分钟，见 [`INTERVAL_RANGE`]）。
    pub async fn set_options(
        &self,
        auto: Option<bool>,
        interval_minutes: Option<u32>,
    ) -> Result<(), SyncError> {
        if let Some(minutes) = interval_minutes
            && !INTERVAL_RANGE.contains(&minutes)
        {
            return Err(SyncError::invalid("interval", "out_of_range"));
        }
        let _guard = self.inner.lock.lock().await;
        let store = self.store().clone();
        let config = blocking(move || {
            let db = store.database();
            let mut config = state::load_config(db)?.ok_or(SyncError::NotConfigured)?;
            if let Some(auto) = auto {
                config.auto = auto;
            }
            if let Some(minutes) = interval_minutes {
                config.interval_minutes = minutes;
            }
            state::save_config(db, &config)?;
            Ok(config)
        })
        .await?;
        self.publish(|s| s.config = Some(config));
        self.inner.wake.notify_one();
        Ok(())
    }

    // ---------------------------------------------------------------- 同步

    /// 立即同步一次。需要用户决定时返回 [`SyncOutcome::Pending`]，什么都不改。
    pub async fn sync_now(&self) -> Result<SyncOutcome, SyncError> {
        self.run(Mode::Normal, false).await
    }

    /// 回答等待决定的事并继续同步。
    pub async fn answer(&self, answer: Answer) -> Result<SyncOutcome, SyncError> {
        match answer {
            Answer::Merge => self.run(Mode::Merge, false).await,
            Answer::UseRemote => self.run(Mode::Remote, false).await,
            Answer::UseLocal => self.run(Mode::Local, false).await,
            Answer::ConfirmDeletes => self.run(Mode::Normal, true).await,
        }
    }

    async fn run(&self, mode: Mode, confirm: bool) -> Result<SyncOutcome, SyncError> {
        let _guard = self.inner.lock.lock().await;
        tracing::debug!(mode = ?mode, confirm, "开始同步");
        self.publish(|s| s.syncing = true);
        let result = self.run_inner(mode, confirm).await;
        match &result {
            Err(SyncError::NotConfigured) => {
                // 保险库重置过时同步状态已被删除
                let store = self.store().clone();
                let config = blocking(move || state::load_config(store.database()))
                    .await
                    .ok()
                    .flatten();
                if config.is_none() {
                    *self.persisted() = Persisted::default();
                }
                self.publish(|s| {
                    s.syncing = false;
                    s.config = config;
                });
                return result.map(|(outcome, _)| outcome);
            }
            Err(error) => {
                tracing::warn!(code = error.code(), "同步失败：{error}");
                self.update_persisted(|p| p.last_error = Some(error.clone()))
                    .await;
            }
            Ok((SyncOutcome::Done(report), devices)) => {
                tracing::info!(
                    pulled = report.pulled,
                    pushed = report.pushed,
                    conflicts = report.conflicts.len(),
                    "已同步"
                );
                let devices = devices.clone();
                self.update_persisted(|p| {
                    p.last_error = None;
                    p.pending = None;
                    p.last_sync = Some(report.at);
                    p.last_report = Some(report.clone());
                    if !report.conflicts.is_empty() {
                        p.conflicts = report
                            .conflicts
                            .iter()
                            .map(|c| Conflict {
                                can_use_other: false,
                                ..c.clone()
                            })
                            .collect();
                    }
                    if let Some(devices) = devices {
                        p.devices = devices;
                    }
                })
                .await;
            }
            Ok((SyncOutcome::Pending(pending), _)) => {
                tracing::info!(?pending, "同步等待用户决定");
                self.update_persisted(|p| {
                    p.last_error = None;
                    p.pending = Some(pending.clone());
                })
                .await;
            }
        }
        self.publish(|s| s.syncing = false);
        result.map(|(outcome, _)| outcome)
    }

    /// GitHub API 的地址（测试中换成假的服务）。
    fn gist_api(&self) -> String {
        #[cfg(test)]
        if let Some(api) = self.hooks().gist_api.clone() {
            return api;
        }
        gist::API_BASE.to_string()
    }

    fn backend(
        &self,
        config: &SyncConfig,
        credential: &SecretString,
    ) -> Result<Arc<dyn Backend>, SyncError> {
        let backend: Arc<dyn Backend> = match &config.target {
            SyncTarget::Folder { path } => Arc::new(Folder::new(path.clone())),
            SyncTarget::WebDav {
                url,
                username,
                allow_http,
            } => Arc::new(webdav::WebDav::new(
                url,
                *allow_http,
                username,
                credential.clone(),
            )?),
            SyncTarget::S3 {
                endpoint,
                region,
                bucket,
                prefix,
                access_key_id,
                path_style,
                allow_http,
            } => {
                let location = s3::normalize(
                    endpoint,
                    region,
                    bucket,
                    prefix,
                    access_key_id,
                    *path_style,
                    *allow_http,
                )?;
                Arc::new(s3::S3::new(&location, credential.clone())?)
            }
            SyncTarget::Gist { gist_id } => Arc::new(gist::Gist::new(
                &self.gist_api(),
                credential.clone(),
                gist_id,
            )?),
        };
        #[cfg(test)]
        if let Some(wrap) = self.hooks().wrap.clone() {
            return Ok(wrap(backend));
        }
        Ok(backend)
    }

    /// 读出本机的全部记录。
    async fn extract(&self) -> Result<Locals, SyncError> {
        let store = self.store().clone();
        blocking(move || Ok(records::locals_of(&store.sync_snapshot()?))).await
    }

    /// 写入其他设备的修改之前保存本机快照。
    async fn snapshot_before_apply(&self) -> Result<(), SyncError> {
        if let Some(backups) = &self.inner.backups {
            backups
                .snapshot(SnapshotReason::Sync)
                .await
                .map_err(|e| SyncError::Snapshot {
                    message: e.to_string(),
                })?;
        }
        Ok(())
    }

    /// 把云端的变更写入本机：先确认本机的数据仍与读取时的 `before` 相同（被修改了返回 `None`，不覆盖用户刚做的修改），
    /// 再在一个事务中写入。返回没有写入的记录（本机不认识、内容解析不了或不合法）。
    async fn apply_local(
        &self,
        before: &Locals,
        changes: &[(String, Option<Remote>)],
    ) -> Result<Option<HashSet<String>>, SyncError> {
        let mut raw = Vec::new();
        let mut keys = Vec::new();
        let mut failed = HashSet::new();
        for (key, remote) in changes {
            let updated_at = remote.as_ref().map_or(0, |r| version_time(&r.v));
            match records::change_of(key, remote.as_ref().map(|r| &r.data), updated_at) {
                Some(change) => {
                    raw.push(change);
                    keys.push(key.clone());
                }
                // 云端的内容不是本机能理解的格式；云端没有某组设置时保留本机的（不算失败）
                None if remote.is_some() => match records::unreadable_of(key) {
                    Some(change) => {
                        raw.push(change);
                        keys.push(key.clone());
                    }
                    None => {
                        failed.insert(key.clone());
                    }
                },
                None => {}
            }
        }
        let expect = records::hashes(before);
        let store = self.store().clone();
        let outcome = blocking(move || {
            Ok(store.sync_apply(
                |current| records::hashes(&records::locals_of(current)) == expect,
                &raw,
            )?)
        })
        .await?;
        match outcome {
            ApplyOutcome::LocalChanged => Ok(None),
            ApplyOutcome::Applied { failed: indices } => {
                for index in indices {
                    if let Some(key) = keys.get(index) {
                        tracing::warn!(key = %key, "同步的数据没有写入本机，保留本机的");
                        failed.insert(key.clone());
                    }
                }
                Ok(Some(failed))
            }
        }
    }

    async fn run_inner(
        &self,
        mode: Mode,
        confirm: bool,
    ) -> Result<(SyncOutcome, Option<Vec<Device>>), SyncError> {
        let store = self.store().clone();
        let (config, secrets) = blocking(move || state::load(&store)).await?;
        let store = self.store().clone();
        let device = blocking(move || state::device_id(store.database())).await?;
        let backend = self.backend(&config, &secrets.credential)?;
        let app = self.app();
        let mut report = SyncReport::default();
        let mut conflicts: Vec<ConflictEntry> = Vec::new();
        let mut applied = false;
        // 上一轮写入本机之后的基准（见 applied_bases）：那一轮写到云端时其他设备先写入了，这一轮以它为准合并，
        // 不从数据库读取写入之前的基准，否则刚写入本机、其他设备随后删除的记录会被当作本机新增的推回云端
        let mut carried: Option<Bases> = None;
        // 上一轮因为本机的数据在同步期间被修改而重来
        let mut busy = false;
        let mut attempt = 0;
        loop {
            if attempt >= MAX_ATTEMPTS {
                return Err(if busy {
                    SyncError::LocalBusy
                } else {
                    SyncError::Contended
                });
            }
            attempt += 1;
            busy = false;
            // 每一轮重新决定方式：上一轮以本机为准上传时其他设备先写入了，这一轮要重新比较，不能覆盖。
            // 合并的结果已经写入本机时，之后以写入的内容为基准合并
            let mut m = if applied && mode == Mode::Merge {
                Mode::Normal
            } else {
                mode
            };
            let (exists, etag, mut doc) = match backend.get().await? {
                Fetch::Missing => {
                    tracing::debug!(attempt, "同步位置中还没有同步文件");
                    (false, None, Document::default())
                }
                Fetch::Found { data, etag } => {
                    tracing::debug!(attempt, bytes = data.len(), "读到同步文件");
                    let password = secrets.password.clone();
                    let doc = blocking(move || document::open(&data, &password, MAX_DOCUMENT_SIZE))
                        .await?;
                    (true, Some(etag), doc)
                }
            };
            let mut locals = self.extract().await?;
            let mut bases = match &carried {
                Some(bases) => bases.clone(),
                None => {
                    let store = self.store().clone();
                    blocking(move || state::load_bases(store.database())).await?
                }
            };
            let store = self.store().clone();
            let wrote = blocking(move || state::load_wrote(store.database())).await?;
            let now = self.now();

            // 同步过而云端没有同步文件（被删除、网盘还没有下载下来、WebDAV 返回 404）：不能当作其他设备删除了全部数据，
            // 以本机为准重新上传（与首次同步时云端为空相同）。用户选择以云端为准时云端已经没有数据，同样处理
            report.reuploaded = false;
            if !exists && ((!bases.is_empty() && m == Mode::Normal) || m == Mode::Remote) {
                m = Mode::Local;
                report.reuploaded = true;
                // 带上本机上次同步时看到的设备列表：其他设备据此判断自己的写入是否还在，
                // 不会把自己写过的旧内容当作丢失的写入、覆盖这次上传中较新的修改
                for d in self.persisted().devices.clone() {
                    doc.devices.entry(d.id.clone()).or_insert(d);
                }
            }

            // 首次同步（本机没有基准）
            if bases.is_empty() && m == Mode::Normal && !applied {
                let local = counts(locals.keys());
                let remote = counts(doc.records.keys());
                if !exists || remote.total() == 0 {
                    m = Mode::Local;
                } else if local.total() == 0 {
                    m = Mode::Remote;
                } else {
                    let duplicates = records::duplicate_hosts(&locals, &doc.records);
                    return Ok((
                        SyncOutcome::Pending(Pending::FirstSync {
                            local,
                            remote,
                            duplicates,
                        }),
                        None,
                    ));
                }
            }

            let mut o = PlanOpts::new(m, &device, now, &app);
            // seen 是云端中本机最后一次写入的时间：本机在这之后写到云端的内容都不在云端
            let me = doc.devices.get(&device).cloned();
            let seen = me.as_ref().map_or(0, |d| d.last_sync);
            if exists
                && !bases.is_empty()
                && wrote.at != 0
                && m == Mode::Normal
                && (me.is_none() || seen < wrote.at)
            {
                o.lost = true;
                o.wrote_after = wrote
                    .keys
                    .iter()
                    .filter(|(_, at)| **at > seen)
                    .map(|(k, _)| k.clone())
                    .collect();
                o.deleted_after = wrote
                    .deleted
                    .iter()
                    .filter(|(_, d)| d.at > seen)
                    .map(|(k, d)| (k.clone(), d.v.clone()))
                    .collect();
            }
            let mut plan = make_plan(&locals, &doc.records, &bases, &o);
            report.lost_write = o.lost;
            if !applied {
                conflicts = plan.conflicts.clone();
                // 用户明确选择了方式（合并、以云端 / 本机为准）时不再确认
                if !confirm
                    && m == Mode::Normal
                    && (too_many(plan.local_deletes, counts(locals.keys()).total())
                        || too_many(plan.remote_deletes, counts(doc.records.keys()).total()))
                {
                    return Ok((
                        SyncOutcome::Pending(Pending::ConfirmDeletes {
                            local: plan.local_deletes,
                            remote: plan.remote_deletes,
                        }),
                        None,
                    ));
                }
            }

            #[cfg(test)]
            self.before_apply_hook();
            if !plan.to_local.is_empty() {
                self.snapshot_before_apply().await?;
                let Some(failed) = self.apply_local(&locals, &plan.to_local).await? else {
                    // 读取之后用户修改了本机的数据：重新读取再合并，不覆盖
                    busy = true;
                    continue;
                };
                report.pulled += plan.to_local.len() - failed.len();
                applied = true;
                // 写入后重新比较：写入了的记录以云端的为基准。写入时被替换的本机数据（例如监听同一端口的转发规则）
                // 不再上传，写入时修复了引用的记录作为本机的修改上传
                let before = std::mem::replace(&mut locals, self.extract().await?);
                bases = applied_bases(
                    &bases,
                    &doc.records,
                    &plan.to_local,
                    &failed,
                    &before,
                    &locals,
                    &app,
                );
                carried = Some(bases.clone());
                // 以本机为准时写入本机的只有云端完好的秘密（见 make_plan），其余仍以本机为准
                if o.mode != Mode::Local {
                    o.mode = Mode::Normal;
                }
                plan.to_remote = make_plan(&locals, &doc.records, &bases, &o).to_remote;
            }

            // deleted 是这次从云端删除的记录删除前的版本（见 Wrote::deleted）
            let mut deleted: BTreeMap<String, String> = BTreeMap::new();
            for (key, record) in &plan.to_remote {
                match record {
                    Some(record) => {
                        doc.records.insert(key.clone(), record.clone());
                    }
                    None => {
                        if let Some(old) = doc.records.remove(key) {
                            deleted.insert(key.clone(), old.v);
                        }
                    }
                }
            }
            // 写入丢失时即使没有要上传的记录也写入：记下本机这次的写入，之后不再当作丢失
            let need_write = !plan.to_remote.is_empty()
                || !exists
                || o.lost
                || me.as_ref().is_none_or(|d| {
                    d.name != config.device_name || now - d.last_sync > REFRESH_DEVICE_MS
                });
            let mut written = None;
            if need_write {
                doc.devices.insert(
                    device.clone(),
                    Device {
                        id: device.clone(),
                        name: config.device_name.clone(),
                        app: app.clone(),
                        last_sync: now,
                    },
                );
                let password = secrets.password.clone();
                let seal_app = app.clone();
                let (back, sealed) = blocking(move || {
                    let sealed =
                        document::seal(&doc, &password, &seal_app, now, MAX_DOCUMENT_SIZE)?;
                    Ok((doc, sealed))
                })
                .await?;
                doc = back;
                tracing::debug!(
                    bytes = sealed.len(),
                    pushed = plan.to_remote.len(),
                    "写入同步文件"
                );
                if backend.put(&sealed, etag.as_deref()).await? == Stored::Changed {
                    tracing::info!("写入同步文件时其他设备先写入了，重新拉取再合并");
                    // 其他设备先写入了：重新拉取再合并
                    continue;
                }
                report.pushed = plan.to_remote.len();
                let mut log = Wrote {
                    at: now,
                    ..Default::default()
                };
                for (key, at) in &wrote.keys {
                    if doc.records.contains_key(key) {
                        log.keys.insert(key.clone(), *at);
                    }
                }
                // 还没有确认在云端的删除（云端中本机最后一次写入的时间早于它）保留到确认为止；云端又有了这条记录时不再保留
                for (key, d) in &wrote.deleted {
                    if !doc.records.contains_key(key) && d.at > seen {
                        log.deleted.insert(key.clone(), d.clone());
                    }
                }
                for (key, record) in &plan.to_remote {
                    if record.is_some() {
                        log.keys.insert(key.clone(), now);
                    } else if let Some(v) = deleted.get(key) {
                        log.deleted.insert(
                            key.clone(),
                            Deletion {
                                v: v.clone(),
                                at: now,
                            },
                        );
                    }
                }
                written = Some(log);
            }

            let next = next_bases(&doc.records, &locals, &bases, &plan.to_remote, &app);
            let store = self.store().clone();
            blocking(move || {
                state::save_bases(store.database(), &next, written.as_ref())?;
                // 删除已经同步出去的墓碑：云端已经没有这条记录，其他设备按各自的基准发现删除，不再需要它
                let purged = store.sync_purge_tombstones(|kind, id| {
                    next.contains_key(&records::item_key(kind, id))
                })?;
                if purged > 0 {
                    tracing::debug!(purged, "已清理同步过的墓碑");
                }
                Ok(())
            })
            .await?;
            *self
                .inner
                .fingerprint
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(records::fingerprint(&locals));

            report.at = now;
            report.conflicts = conflicts.iter().map(|c| conflict_info(c, true)).collect();
            let mut live = self
                .inner
                .conflicts
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !conflicts.is_empty() {
                *live = conflicts;
            } else {
                // 之前的冲突中本机已不是胜出一边的（之后改过）：不能再改用另一边
                live.retain(|c| locals.get(&c.key).map(|l| &l.hash) == c.winner_hash.as_ref());
            }
            drop(live);
            return Ok((SyncOutcome::Done(report), Some(device_list(&doc.devices))));
        }
    }

    // ---------------------------------------------------------------- 冲突

    /// 冲突中改用落败的一边：作为本机的新修改写入（删除的一边落败时删除它），下次同步时上传。
    /// 冲突已不在列表中（重启过、已改用过）时返回 [`SyncError::Invalid`]（`conflict`，`not_found`）；冲突之后这条记录
    /// 又改过（本机修改，或之后的同步写入了更新的版本）时为 `stale`，并从列表中去掉；落败的一边
    /// 本机写不进（来自其他版本）时为 `not_applicable`；要删除的凭据档案、密钥仍被使用时为（`profile` / `key`，`in_use`）。
    pub async fn use_other_version(&self, key: &str) -> Result<(), SyncError> {
        let _guard = self.inner.lock.lock().await;
        let entry = {
            let conflicts = self
                .inner
                .conflicts
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            conflicts.iter().find(|c| c.key == key).cloned()
        }
        .ok_or_else(|| SyncError::invalid("conflict", "not_found"))?;
        let now = self.now();
        let store = self.store().clone();
        let key_owned = entry.key.clone();
        let current = {
            let store = store.clone();
            let key = entry.key.clone();
            blocking(move || {
                let locals = records::locals_of(&store.sync_snapshot()?);
                Ok(locals.get(&key).map(|l| l.hash.clone()))
            })
            .await?
        };
        if current != entry.winner_hash {
            self.forget_conflict(key).await;
            return Err(SyncError::invalid("conflict", "stale"));
        }
        blocking(move || {
            match &entry.loser {
                Some(data) => {
                    let change = records::change_of(&key_owned, Some(data), now)
                        .ok_or_else(|| SyncError::invalid("conflict", "not_found"))?;
                    match store.sync_apply(|_| true, std::slice::from_ref(&change))? {
                        ApplyOutcome::Applied { failed } if failed.is_empty() => Ok(()),
                        // 落败的一边来自其他版本，本机写不进
                        _ => Err(SyncError::invalid("conflict", "not_applicable")),
                    }
                }
                None => delete_item(&store, &key_owned),
            }
        })
        .await?;
        self.forget_conflict(key).await;
        Ok(())
    }

    /// 从冲突列表中去掉一条（已改用另一边，或已失效）。
    async fn forget_conflict(&self, key: &str) {
        self.inner
            .conflicts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|c| c.key != key);
        self.update_persisted(|p| p.conflicts.retain(|c| c.key != key))
            .await;
        self.publish(|_| {});
    }

    /// 清空冲突列表（用户看过了）。
    pub fn dismiss_conflicts(&self) {
        self.inner
            .conflicts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        let snapshot = {
            let mut persisted = self.persisted();
            persisted.conflicts.clear();
            persisted.clone()
        };
        if let Err(e) = state::save_persisted(self.store().database(), &snapshot) {
            tracing::warn!("保存同步状态失败：{e}");
        }
        self.publish(|_| {});
    }
}

/// 用存储的删除删除一条条目（连同它的转发规则与秘密；仍被引用的凭据档案、密钥不能删除）。
fn delete_item(store: &Store, key: &str) -> Result<(), SyncError> {
    let (kind, id) = key
        .split_once('/')
        .and_then(|(k, id)| Some((records::item_kind(k)?, id.to_string())))
        .ok_or_else(|| SyncError::invalid("conflict", "not_found"))?;
    let result = match kind {
        Kind::Host => store.delete::<Host>(&HostId::from_raw(id)),
        Kind::Group => store.delete::<Group>(&GroupId::from_raw(id)),
        Kind::Profile => store.delete::<Profile>(&ProfileId::from_raw(id)),
        Kind::Key => store.delete::<KeyEntry>(&KeyId::from_raw(id)),
        Kind::Snippet => store.delete::<Snippet>(&SnippetId::from_raw(id)),
        Kind::Forward => store.delete::<Forward>(&ForwardId::from_raw(id)),
        Kind::Workspace => store.delete::<Workspace>(&WorkspaceId::from_raw(id)),
        Kind::Scheme => store.delete::<ColorScheme>(&SchemeId::from_raw(id)),
    };
    match result {
        Ok(()) | Err(crate::CoreError::NotFound(_)) => Ok(()),
        Err(crate::CoreError::Invalid(invalid)) => {
            Err(SyncError::invalid(invalid.field, invalid.reason))
        }
        Err(e) => Err(e.into()),
    }
}

fn conflict_info(entry: &ConflictEntry, can_use_other: bool) -> Conflict {
    Conflict {
        key: entry.key.clone(),
        kind: entry.kind,
        name: entry.name.clone(),
        winner: entry.winner,
        deleted: entry.deleted,
        // 设置组不会被删除；落败的一边是删除时改用它就是删除
        can_use_other: can_use_other
            && (entry.loser.is_some() || entry.kind != RecordKind::Settings),
    }
}

/// 设备列表：最近同步的在前。
fn device_list(devices: &BTreeMap<String, Device>) -> Vec<Device> {
    let mut list: Vec<Device> = devices.values().cloned().collect();
    list.sort_by(|a, b| b.last_sync.cmp(&a.last_sync).then_with(|| a.id.cmp(&b.id)));
    list
}
