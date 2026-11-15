//! 转发管理器：运行中的转发与它们的状态。
//!
//! 规则本身是存储中的条目（增删改经 [`Store`]）；管理器订阅存储的变更，规则的路线被修改或规则被删除（含删除主机时的
//! 级联删除）时停止对应的转发，由界面按需重新启动（修改运行中的规则会先停止它）。只改名称、“自动启动”
//! 不影响运行中的转发，已建立的连接不断开。
//!
//! 转发属于启动它的窗口（`owner`，界面自己的标识）：用户停止、窗口关闭（[`ForwardManager::close_owner`]）或退出时
//! 关闭监听与全部连接。与终端共用连接的转发跟着那条连接走：终端移到别的窗口、原来的窗口关闭而别的窗口还有这条连接的
//! 终端时，界面把它改属那个窗口（[`ForwardManager::reassign_group`]）。状态的变化经 [`ForwardManager::watch`] 推送快照（合并到每 100 毫秒最多一次；有运行中的转发时
//! 每隔 [`ForwardOptions::stats_interval`] 刷新一次连接数与流量）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use tokio::sync::{Notify, broadcast, watch};

use super::runner::Runner;
use super::{ForwardError, TargetError};
use crate::id::ForwardId;
use crate::model::{Forward, Kind};
use crate::ssh::Prompter;
use crate::store::{Store, StoreEvent};

/// 重连与排队的参数。
#[derive(Clone, Debug)]
pub struct ForwardOptions {
    /// 第一次重连前的等待，之后每次翻倍，最长 `backoff_max`（与终端的重连相同：1 秒起，最长 30 秒）
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    /// 每次重连（建立 SSH 连接）的超时
    pub reconnect_timeout: Duration,
    /// 为一个本地连接打开 direct-tcpip 通道（远程转发：连接本机的目标）的超时
    pub channel_timeout: Duration,
    /// 重连期间新接入的连接最多等待的时间：期间恢复则照常转发，否则关闭该连接
    pub queue_timeout: Duration,
    /// 有运行中的转发时，多久推送一次连接数与流量
    pub stats_interval: Duration,
}

impl Default for ForwardOptions {
    fn default() -> Self {
        Self {
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
            reconnect_timeout: Duration::from_secs(60),
            channel_timeout: Duration::from_secs(15),
            queue_timeout: Duration::from_secs(15),
            stats_interval: Duration::from_secs(1),
        }
    }
}

impl ForwardOptions {
    /// 第 `attempt` 次（从 1 起）重连前等多久。
    pub(crate) fn backoff(&self, attempt: u32) -> Duration {
        self.backoff_base
            .checked_mul(
                1u32.checked_shl(attempt.saturating_sub(1))
                    .unwrap_or(u32::MAX),
            )
            .unwrap_or(self.backoff_max)
            .min(self.backoff_max)
    }
}

/// 转发的运行状态。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ForwardState {
    #[default]
    Stopped,
    /// 正在监听、连接主机（可能在等用户确认指纹、输入密码）
    Starting,
    Running,
    /// SSH 连接断开，正在重连；期间本地端口保持监听，新接入的连接排队等待
    Reconnecting {
        /// 第几次尝试（从 1 起）
        attempt: u32,
        /// 上一次重连失败的原因（第一次尝试之前为空）
        last_error: Option<ForwardError>,
    },
    /// 启动失败，或运行中遇到重试也无法解决的错误（认证失败、需要输入验证码等）而停止
    Failed(ForwardError),
}

impl ForwardState {
    /// 启动中、运行中或重连中（界面显示"停止"按钮）。
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            ForwardState::Starting | ForwardState::Running | ForwardState::Reconnecting { .. }
        )
    }
}

/// 一条规则的状态与统计。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ForwardStatus {
    pub state: ForwardState,
    /// 启动它的窗口；停止的为空
    pub owner: Option<String>,
    /// 当前连接数（含重连期间排队等待的连接）
    pub connections: usize,
    /// 本次启动以来接入的连接总数
    pub accepted: u64,
    /// 从目标收到、发给客户端的字节数（本次启动以来累计）
    pub bytes_in: u64,
    /// 发往目标的字节数（本次启动以来累计）
    pub bytes_out: u64,
    /// 运行期间最近一次连接目标失败的原因（例如目标端口没有服务），不影响转发本身
    pub last_error: Option<TargetError>,
    /// 本次启动的时间（Unix 毫秒）
    pub started_at: Option<i64>,
}

/// 全部规则的状态。没有列出的规则是停止的。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ForwardSnapshot {
    entries: HashMap<ForwardId, ForwardStatus>,
}

impl ForwardSnapshot {
    /// 一条规则的状态（停止的规则为默认值）。
    pub fn status(&self, id: &ForwardId) -> ForwardStatus {
        self.entries.get(id).cloned().unwrap_or_default()
    }

    pub fn get(&self, id: &ForwardId) -> Option<&ForwardStatus> {
        self.entries.get(id)
    }

    /// 启动中、运行中、重连中的转发数（标题栏的"运行中的转发"）。
    pub fn active(&self) -> usize {
        self.entries
            .values()
            .filter(|s| s.state.is_active())
            .count()
    }

    /// 不是停止状态的规则（运行中的与失败的）。
    pub fn iter(&self) -> impl Iterator<Item = (&ForwardId, &ForwardStatus)> {
        self.entries.iter()
    }
}

/// 启动转发的请求。
pub struct StartRequest {
    pub rule: ForwardId,
    /// 所属窗口（窗口关闭时转发随之停止）
    pub owner: String,
    /// 与终端共用连接：终端的连接组（`TerminalSession::group`），不再认证；为空时新建连接。
    /// "只用一个通道"的主机不能共用（返回 [`ForwardError::SingleChannel`]）
    pub share_group: Option<String>,
    /// 首次连接时确认指纹、输入密码与验证码。只在启动时询问；自动重连只用这次输入过的密码，
    /// 需要再问时停止并报告原因
    pub prompter: Prompter,
    /// 随连接结束而停：SSH 连接断开（与终端共用时即终端的连接结束）时停止，不重连、不记为失败（主机工具的“在浏览器中
    /// 打开”）。用户之后从列表中再启动的照常重连
    pub until_disconnect: bool,
}

/// 转发管理器。可以克隆，克隆的是同一个管理器。
#[derive(Clone)]
pub struct ForwardManager(pub(crate) Arc<Inner>);

pub(crate) struct Inner {
    pub(crate) store: Arc<Store>,
    pub(crate) options: ForwardOptions,
    book: Mutex<Book>,
    snapshot: watch::Sender<Arc<ForwardSnapshot>>,
    dirty: Arc<Notify>,
    closed: AtomicBool,
}

#[derive(Default)]
pub(crate) struct Book {
    /// 启动中、运行中、重连中的
    pub(crate) running: HashMap<ForwardId, Arc<Runner>>,
    /// 启动失败或因错误停止的规则的最后状态，供界面显示原因；再次启动、停止、规则被修改或删除时清除
    failed: HashMap<ForwardId, Failure>,
}

struct Failure {
    /// 失败时的规则内容（规则被修改后清除）
    rule: Forward,
    status: ForwardStatus,
}

impl ForwardManager {
    /// 创建管理器（应用启动时建一个，各窗口共用）。可以在任何线程调用，后台任务在核心的运行时上执行。
    pub fn new(store: Arc<Store>, options: ForwardOptions) -> Self {
        let events = store.subscribe();
        let inner = Arc::new(Inner {
            store,
            options,
            book: Mutex::default(),
            snapshot: watch::channel(Arc::new(ForwardSnapshot::default())).0,
            dirty: Arc::new(Notify::new()),
            closed: AtomicBool::new(false),
        });
        crate::runtime::spawn(follow_store(Arc::downgrade(&inner), events));
        crate::runtime::spawn(publish_loop(Arc::downgrade(&inner), inner.dirty.clone()));
        Self(inner)
    }

    /// 订阅全部规则的状态：有变化时收到新的快照。
    pub fn watch(&self) -> watch::Receiver<Arc<ForwardSnapshot>> {
        self.0.snapshot.subscribe()
    }

    /// 当前的状态（立即计算，不等推送）。
    pub fn snapshot(&self) -> ForwardSnapshot {
        self.0.compute()
    }

    /// 一条规则的状态。
    pub fn status(&self, id: &ForwardId) -> ForwardStatus {
        self.0.status(id)
    }

    /// 启动中、运行中、重连中的转发数。
    pub fn active(&self) -> usize {
        self.0.book().running.len()
    }

    /// 启动转发：本地、动态转发先监听本地地址（端口被占用时立即返回，不连接主机），再连接主机；
    /// 远程转发连接主机后请服务器监听。已在运行或正在启动时直接返回成功（规则在运行后被修改过的，按新的内容重新启动）。
    ///
    /// 失败的原因同时记在状态中（[`ForwardState::Failed`]），界面在列表中显示。
    pub async fn start(&self, request: StartRequest) -> Result<(), ForwardError> {
        let inner = self.0.clone();
        join(crate::runtime::spawn(
            async move { inner.start(request).await },
        ))
        .await
        .unwrap_or(Err(ForwardError::Stopped))
    }

    /// 停止转发，清除失败的状态。规则不存在且没有在运行时返回 [`ForwardError::RuleNotFound`]。
    /// 返回时本地端口已经释放。
    pub async fn stop(&self, id: &ForwardId) -> Result<(), ForwardError> {
        let inner = self.0.clone();
        let id = id.clone();
        join(crate::runtime::spawn(async move {
            if inner.stop_id(&id).await {
                return Ok(());
            }
            inner
                .store
                .get::<Forward>(&id)
                .map(drop)
                .ok_or(ForwardError::RuleNotFound)
        }))
        .await
        .unwrap_or(Ok(()))
    }

    /// 停止某个窗口启动的全部转发（窗口关闭时调用），并清除它们的失败状态，返回停止的数量。
    pub async fn close_owner(&self, owner: &str) -> usize {
        let owner = owner.to_string();
        let matcher = owner.clone();
        self.stop_matching(move |r| r.owner() == matcher, Some(owner))
            .await
    }

    /// 窗口 `from` 的转发中与终端共用连接组 `group` 的改属窗口 `to`，返回改了几条：终端的 Tab 移到别的窗口、原来的
    /// 窗口里不再有这条连接的终端时，或者原来的窗口关闭而别的窗口还有这条连接的终端时（转发不停）。
    pub fn reassign_group(&self, from: &str, group: &str, to: &str) -> usize {
        let mut moved = 0;
        for runner in self.0.book().running.values() {
            if runner.owner() == from && runner.shared_group().as_deref() == Some(group) {
                runner.set_owner(to);
                moved += 1;
            }
        }
        if moved > 0 {
            tracing::info!(
                from,
                to,
                count = moved,
                "共用终端连接的端口转发改属别的窗口"
            );
            self.0.mark_dirty();
        }
        moved
    }

    /// 停止全部转发（退出时）。
    pub async fn close_all(&self) {
        self.0.book().failed.clear();
        self.stop_matching(|_| true, None).await;
    }

    /// 应用启动后要自动启动的规则：勾选了"Termlet 启动后自动启动"且当前已停止的（上次出错的不自动重试，
    /// 避免反复失败）。界面在第一个窗口打开、保险库解锁后逐条 [`ForwardManager::start`]。
    pub fn auto_start_rules(&self) -> Vec<ForwardId> {
        let snapshot = self.0.compute();
        self.0
            .store
            .list::<Forward>()
            .into_iter()
            .filter(|r| r.doc.auto_start && snapshot.get(&r.id).is_none())
            .map(|r| r.id)
            .collect()
    }

    async fn stop_matching(
        &self,
        matches: impl Fn(&Runner) -> bool + Send + 'static,
        clear_owner: Option<String>,
    ) -> usize {
        let inner = self.0.clone();
        join(crate::runtime::spawn(async move {
            let list: Vec<Arc<Runner>> = {
                let mut book = inner.book();
                if let Some(owner) = &clear_owner {
                    book.failed
                        .retain(|_, f| f.status.owner.as_deref() != Some(owner.as_str()));
                }
                let ids: Vec<ForwardId> = book
                    .running
                    .iter()
                    .filter(|(_, r)| matches(r))
                    .map(|(id, _)| id.clone())
                    .collect();
                ids.iter()
                    .filter_map(|id| book.running.remove(id))
                    .collect()
            };
            for runner in &list {
                runner.stop().await;
                tracing::info!(id = %runner.id, "端口转发已停止");
            }
            inner.mark_dirty();
            list.len()
        }))
        .await
        .unwrap_or(0)
    }
}

/// 两条规则的路线是否相同（类型、经由的主机、监听与目标）：只改名称、“自动启动”等不用重新启动。
fn same_route(a: &Forward, b: &Forward) -> bool {
    a.kind == b.kind
        && a.host == b.host
        && a.bind_host == b.bind_host
        && a.bind_port == b.bind_port
        && a.target_host == b.target_host
        && a.target_port == b.target_port
}

/// 等待核心运行时上的任务：任务中的 panic 原样抛出，被取消（运行时关闭）时为 `None`。
async fn join<T>(handle: tokio::task::JoinHandle<T>) -> Option<T> {
    match handle.await {
        Ok(value) => Some(value),
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(_) => None,
    }
}

impl Inner {
    pub(crate) fn book(&self) -> MutexGuard<'_, Book> {
        self.book.lock().unwrap_or_else(|e| e.into_inner())
    }

    async fn start(self: &Arc<Self>, request: StartRequest) -> Result<(), ForwardError> {
        let StartRequest {
            rule: id,
            owner,
            share_group,
            prompter,
            until_disconnect,
        } = request;
        let rule = self
            .store
            .get::<Forward>(&id)
            .ok_or(ForwardError::RuleNotFound)?
            .doc;
        let (previous, runner) = {
            let mut book = self.book();
            if book
                .running
                .get(&id)
                .is_some_and(|r| same_route(&r.rule, &rule))
            {
                return Ok(());
            }
            let runner = Runner::new(self, id.clone(), rule, owner);
            let previous = book.running.insert(id.clone(), runner.clone());
            book.failed.remove(&id);
            (previous, runner)
        };
        // 规则改过：先停止旧的（释放端口）再按新的内容启动
        if let Some(previous) = previous {
            previous.stop().await;
        }
        self.mark_dirty();
        match runner.begin(share_group, prompter, until_disconnect).await {
            Ok(()) => {
                tracing::info!(
                    id = %id,
                    kind = ?runner.rule.kind,
                    bind = %runner.rule.bind_address(),
                    "端口转发已启动"
                );
                Ok(())
            }
            Err(ForwardError::Stopped) => Err(ForwardError::Stopped),
            Err(error) => {
                self.fail(&runner, error.clone()).await;
                Err(error)
            }
        }
    }

    /// 停止转发并清除失败的状态，返回是否确有运行中的转发。
    async fn stop_id(&self, id: &ForwardId) -> bool {
        let runner = {
            let mut book = self.book();
            book.failed.remove(id);
            book.running.remove(id)
        };
        self.mark_dirty();
        let Some(runner) = runner else {
            return false;
        };
        runner.stop().await;
        tracing::info!(id = %id, "端口转发已停止");
        self.mark_dirty();
        true
    }

    /// 因错误停止转发，保留失败的状态供界面显示；转发已被用户停止（或换成了新的）时不记录。
    pub(crate) async fn fail(&self, runner: &Arc<Runner>, error: ForwardError) {
        let current = {
            let mut book = self.book();
            let current = book
                .running
                .get(&runner.id)
                .is_some_and(|r| Arc::ptr_eq(r, runner));
            if current {
                book.running.remove(&runner.id);
                let mut status = runner.status();
                status.state = ForwardState::Failed(error.clone());
                status.connections = 0;
                book.failed.insert(
                    runner.id.clone(),
                    Failure {
                        rule: runner.rule.clone(),
                        status,
                    },
                );
            }
            current
        };
        runner.stop().await;
        if current {
            tracing::warn!(id = %runner.id, "端口转发因错误停止：{error}");
        }
        self.mark_dirty();
    }

    /// 随连接结束而停的转发（[`StartRequest::until_disconnect`]）的连接结束了：停止它，不记为失败。转发已被用户停止
    /// （或换成了新的）时只确保它停下。
    pub(crate) async fn ended(&self, runner: &Arc<Runner>) {
        let current = {
            let mut book = self.book();
            let current = book
                .running
                .get(&runner.id)
                .is_some_and(|r| Arc::ptr_eq(r, runner));
            if current {
                book.running.remove(&runner.id);
            }
            current
        };
        runner.stop().await;
        if current {
            tracing::info!(id = %runner.id, "端口转发的 SSH 连接已结束，转发随之停止");
        }
        self.mark_dirty();
    }

    /// 规则的路线被修改或规则被删除：停止对应的转发，清除对应的失败状态。
    async fn reconcile(&self) {
        let stale: Vec<Arc<Runner>> = {
            let mut book = self.book();
            let mut stale = Vec::new();
            book.running.retain(|id, runner| {
                let keep = self
                    .store
                    .get::<Forward>(id)
                    .is_some_and(|r| same_route(&r.doc, &runner.rule));
                if !keep {
                    stale.push(runner.clone());
                }
                keep
            });
            book.failed.retain(|id, failure| {
                self.store
                    .get::<Forward>(id)
                    .is_some_and(|r| same_route(&r.doc, &failure.rule))
            });
            stale
        };
        for runner in &stale {
            runner.stop().await;
            tracing::info!(id = %runner.id, "转发规则已修改或删除，转发已停止");
        }
        self.mark_dirty();
    }

    pub(crate) fn status(&self, id: &ForwardId) -> ForwardStatus {
        let book = self.book();
        if let Some(runner) = book.running.get(id) {
            return runner.status();
        }
        book.failed
            .get(id)
            .map(|f| f.status.clone())
            .unwrap_or_default()
    }

    fn compute(&self) -> ForwardSnapshot {
        let book = self.book();
        let mut entries: HashMap<ForwardId, ForwardStatus> = book
            .failed
            .iter()
            .map(|(id, f)| (id.clone(), f.status.clone()))
            .collect();
        for (id, runner) in &book.running {
            entries.insert(id.clone(), runner.status());
        }
        ForwardSnapshot { entries }
    }

    /// 状态有变化：唤醒发布任务。
    pub(crate) fn mark_dirty(&self) {
        self.dirty.notify_one();
    }

    /// 发布一次快照（有变化时），返回是否有运行中的转发。
    fn publish(&self) -> bool {
        let snapshot = self.compute();
        let active = snapshot.active() > 0;
        self.snapshot.send_if_modified(|current| {
            if **current == snapshot {
                false
            } else {
                *current = Arc::new(snapshot);
                true
            }
        });
        active
    }
}

impl Drop for Inner {
    /// 管理器不再被使用（没有调用 `close_all` 就丢弃了）：通知全部转发停止，后台任务随之结束、端口释放。
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        for runner in self
            .book
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .running
            .values()
        {
            runner.signal_stop();
        }
    }
}

/// 跟随存储的变更：转发规则或主机有变化时核对运行中的转发。只持有弱引用，管理器丢弃后结束。
async fn follow_store(inner: Weak<Inner>, mut events: broadcast::Receiver<StoreEvent>) {
    loop {
        let relevant = match events.recv().await {
            Ok(StoreEvent::Changed(Kind::Forward | Kind::Host)) => true,
            Ok(_) => false,
            Err(broadcast::error::RecvError::Lagged(_)) => true,
            Err(broadcast::error::RecvError::Closed) => return,
        };
        let Some(inner) = inner.upgrade() else {
            return;
        };
        if relevant {
            inner.reconcile().await;
        }
    }
}

/// 发布任务：有变化时（最多每 100 毫秒一次）、有运行中的转发时每隔 `stats_interval` 发布一次快照。
async fn publish_loop(inner: Weak<Inner>, dirty: Arc<Notify>) {
    let mut active = false;
    loop {
        let wait = match inner.upgrade() {
            Some(inner) if active => inner.options.stats_interval,
            Some(_) => Duration::from_secs(5),
            None => return,
        };
        let _ = tokio::time::timeout(wait, dirty.notified()).await;
        let Some(manager) = inner.upgrade() else {
            return;
        };
        if manager.closed.load(Ordering::SeqCst) {
            return;
        }
        active = manager.publish();
        drop(manager);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
