//! 自动同步：启动 1 分钟后同步一次，之后按设置的间隔（默认 10 分钟），以及本机修改后约 20 秒
//! （期间的修改合为一次）。没有设置、关闭了自动同步、保险库未解锁、等待用户决定时跳过。
//!
//! 本机的修改从存储的变更事件得知，界面不用另外通知。同步自己写入本机也会发出变更事件：本机的数据与上次同步后
//! 相同（指纹不变）时这一次跳过，不会同步完又同步一次。

use std::time::Duration;

use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};

use super::{CloudSync, DEFAULT_INTERVAL_MINUTES, Persisted, blocking, records, state};
use crate::store::StoreEvent;

/// 启动后第一次自动同步的延迟。
pub const FIRST_DELAY: Duration = Duration::from_secs(60);
/// 本机修改后等待这么久再同步（期间的修改合为一次）。
pub const TOUCH_DELAY: Duration = Duration::from_secs(20);

impl CloudSync {
    /// 在核心的运行时上开始自动同步。返回的任务一直运行，退出时 `abort` 即可。
    pub fn spawn_auto(&self) -> JoinHandle<()> {
        crate::runtime::spawn(self.clone().auto_loop(FIRST_DELAY, TOUCH_DELAY))
    }

    /// 自动同步的循环。调用时就订阅存储的变更事件，不等返回的 future 第一次被轮询：任务开始运行之前的本机修改
    /// 也不会漏掉（漏掉的话要等下一个周期才同步）。
    pub(crate) fn auto_loop(
        self,
        first: Duration,
        touch: Duration,
    ) -> impl Future<Output = ()> + Send + 'static {
        let mut events = self.store().subscribe();
        async move {
            let mut next = Instant::now() + first;
            let mut debounce: Option<Instant> = None;
            loop {
                let deadline = debounce.map_or(next, |d| d.min(next));
                tokio::select! {
                    event = events.recv() => match event {
                        Ok(StoreEvent::Changed(_) | StoreEvent::SettingsChanged | StoreEvent::SecretsChanged)
                        | Err(RecvError::Lagged(_)) => {
                            debounce = Some(Instant::now() + touch);
                        }
                        Ok(StoreEvent::VaultChanged) => {
                            if self.on_vault_changed().await {
                                debounce = Some(Instant::now() + touch);
                            }
                        }
                        Ok(StoreEvent::LocalSettingsChanged) => {}
                        Err(RecvError::Closed) => return,
                    },
                    () = self.inner.wake.notified() => {
                        // 设置变了（重新设置、改了间隔）：按新的间隔重新计时
                        next = Instant::now() + self.interval();
                    }
                    () = sleep_until(deadline) => {
                        let now = Instant::now();
                        let periodic = now >= next;
                        let touched = debounce.is_some_and(|d| d <= now);
                        if touched || periodic {
                            debounce = None;
                        }
                        if self.auto_once(periodic).await {
                            next = Instant::now() + self.interval();
                        }
                    }
                }
            }
        }
    }

    /// 自动同步的间隔。
    fn interval(&self) -> Duration {
        let minutes = self
            .status()
            .config
            .map_or(DEFAULT_INTERVAL_MINUTES, |c| c.interval_minutes)
            .max(1);
        Duration::from_secs(u64::from(minutes) * 60)
    }

    /// 进行一次自动同步；`periodic` 为 false 表示由本机的修改引起。返回是否要重新计时（同步了，或到了定时的时间）。
    async fn auto_once(&self, periodic: bool) -> bool {
        let status = self.status();
        let Some(config) = status.config else {
            return periodic;
        };
        if !config.auto || !self.store().vault().is_unlocked() || status.pending.is_some() {
            return periodic;
        }
        if !periodic && !self.changed_since_sync().await {
            return false;
        }
        // 结果与错误由 run 记下并通知
        let _ = self.sync_now().await;
        true
    }

    /// 本机的数据与上次同步成功后是否不同（读不出来时当作不同）。
    async fn changed_since_sync(&self) -> bool {
        let Some(last) = self
            .inner
            .fingerprint
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        else {
            return true;
        };
        match self.extract().await {
            Ok(locals) => records::fingerprint(&locals) != last,
            Err(_) => true,
        }
    }

    /// 保险库的状态变了：解锁后检查同步密码（保险库重置过时停止同步）。返回是否应当随即同步一次。
    async fn on_vault_changed(&self) -> bool {
        if !self.store().vault().is_unlocked() || self.status().config.is_none() {
            return false;
        }
        let store = self.store().clone();
        match blocking(move || state::load(&store).map(|_| ())).await {
            Ok(()) => true,
            Err(super::SyncError::NotConfigured) => {
                *self.persisted() = Persisted::default();
                self.publish(|s| s.config = None);
                false
            }
            Err(_) => false,
        }
    }
}
