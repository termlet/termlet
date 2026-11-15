//! 云同步的界面：设置中的“同步”页（[`SyncPage`]）、同步需要用户决定时的
//! 对话框（[`answer_pending`]）、标题栏的同步图标（[`SyncIndicator`]）。
//!
//! 核心的 [`CloudSync`] 在启动时由 [`install`] 建一份并开始自动同步，各处经 [`cloud`] 取得。核心的状态经 `watch`
//! 由 [`SyncModel`] 转成 GPUI 的通知：设置页、对话框与标题栏图标都观察它。这里的操作都立即执行，不随设置的“保存”。

// 主会话接入（设置页、标题栏、启动流程）之前，这些入口还没有调用方；接入后删除这一行

mod errors;
mod indicator;
mod labels;
mod page;
mod pending;

#[cfg(test)]
mod tests;

use std::future::Future;

use gpui_kit::{App, AppContext as _, Context, Entity, Global, Task};
use termlet_core::cloudsync::{CloudSync, Service, SyncError, SyncStatus};

use crate::app_state::AppState;

pub(crate) use errors::describe;
// 主会话接入之前还没有调用方（同上）
pub(crate) use indicator::{OpenSyncSettings, SyncIndicator};
pub(crate) use page::SyncPage;
pub(crate) use pending::answer_pending;

/// 应用共用的云同步服务（GPUI 的全局量）。
pub(crate) struct SyncHandle(pub CloudSync);

impl Global for SyncHandle {}

/// 同步状态在 GPUI 中的代表（全局量中的实体）。
struct ModelHandle(Entity<SyncModel>);

impl Global for ModelHandle {}

/// 建立云同步服务并开始自动同步（启动时、[`AppState::install`] 之后调用一次）。写入其他设备的修改之前保存的快照
/// 放在数据目录的快照目录中。
pub(crate) fn install(cx: &mut App) {
    let store = AppState::store(cx);
    let backups = AppState::global(cx).backups.clone();
    let cloud = CloudSync::new(store, Some(backups));
    // 自动同步的任务随进程存活
    drop(cloud.spawn_auto());
    install_with(cloud, cx);
}

/// 用给定的服务安装全局量（测试不开始自动同步、不保存快照）。
fn install_with(cloud: CloudSync, cx: &mut App) {
    let model = cx.new(|cx| SyncModel::new(cloud.clone(), cx));
    cx.set_global(SyncHandle(cloud));
    cx.set_global(ModelHandle(model));
}

/// 云同步服务（克隆很便宜）。
pub(crate) fn cloud(cx: &App) -> CloudSync {
    cx.global::<SyncHandle>().0.clone()
}

/// 同步的状态（观察它得到变化）。
pub(crate) fn model(cx: &App) -> Entity<SyncModel> {
    cx.global::<ModelHandle>().0.clone()
}

/// 核心同步状态的副本：核心每次发布新的状态时更新并通知观察者。
pub(crate) struct SyncModel {
    cloud: CloudSync,
    status: SyncStatus,
    _watch: Task<()>,
}

impl SyncModel {
    fn new(cloud: CloudSync, cx: &mut Context<Self>) -> Self {
        let mut receiver = cloud.subscribe();
        let status = receiver.borrow_and_update().clone();
        // watch 的接收端与执行器无关，在 GPUI 的任务里直接等待
        let watch = cx.spawn(async move |this, cx| {
            while receiver.changed().await.is_ok() {
                let status = receiver.borrow_and_update().clone();
                if this
                    .update(cx, |model, cx| model.set_status(status, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            cloud,
            status,
            _watch: watch,
        }
    }

    pub(crate) fn status(&self) -> &SyncStatus {
        &self.status
    }

    /// 已设置的同步位置的种类（说错误用；没有设置时当作本机文件夹）。
    pub(crate) fn service(&self) -> Service {
        service_of(&self.status)
    }

    /// 立即读取核心的状态（操作完成时用，不等 watch 的通知先到）。
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        let status = self.cloud.status();
        self.set_status(status, cx);
    }

    fn set_status(&mut self, status: SyncStatus, cx: &mut Context<Self>) {
        if self.status != status {
            self.status = status;
            cx.notify();
        }
    }
}

/// 状态中同步位置的种类（没有设置时当作本机文件夹）。
pub(crate) fn service_of(status: &SyncStatus) -> Service {
    status
        .config
        .as_ref()
        .map_or(Service::Folder, |c| c.target.service())
}

/// 立即在核心的运行时上开始一项同步操作，返回的 future 在 GPUI 的任务里等待结果（不等待时操作照样做完，结果由状态发布）。
/// 运行时的任务没有正常结束（进程退出）时当作本机出错。
fn on_core<T: Send + 'static>(
    job: impl Future<Output = Result<T, SyncError>> + Send + 'static,
) -> impl Future<Output = Result<T, SyncError>> {
    let handle = termlet_core::runtime::spawn(job);
    async move {
        match handle.await {
            Ok(result) => result,
            Err(error) => Err(SyncError::Local {
                message: error.to_string(),
            }),
        }
    }
}
