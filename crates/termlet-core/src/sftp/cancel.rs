//! 取消：传输任务的取消随所属会话的关闭一起生效（子令牌）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::Notify;

#[derive(Clone, Default)]
pub(crate) struct Cancel(Arc<Inner>);

#[derive(Default)]
struct Inner {
    flag: AtomicBool,
    notify: Notify,
    children: Mutex<Vec<Weak<Inner>>>,
}

impl Inner {
    fn cancel(&self) {
        if self.flag.swap(true, Ordering::SeqCst) {
            return;
        }
        self.notify.notify_waiters();
        let children = std::mem::take(&mut *self.children.lock().unwrap());
        for child in children.iter().filter_map(Weak::upgrade) {
            child.cancel();
        }
    }
}

impl Cancel {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 子令牌：这个令牌取消时随之取消，单独取消它不影响这个令牌。
    pub(crate) fn child(&self) -> Self {
        let child = Self::new();
        let mut children = self.0.children.lock().unwrap();
        children.retain(|c| c.strong_count() > 0);
        children.push(Arc::downgrade(&child.0));
        drop(children);
        // 登记之前已经取消了
        if self.is_canceled() {
            child.cancel();
        }
        child
    }

    pub(crate) fn cancel(&self) {
        self.0.cancel();
    }

    pub(crate) fn is_canceled(&self) -> bool {
        self.0.flag.load(Ordering::SeqCst)
    }

    /// 等到取消。
    pub(crate) async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_canceled() {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn children_follow_parent() {
        let parent = Cancel::new();
        let child = parent.child();
        let other = parent.child();
        other.cancel();
        assert!(!parent.is_canceled() && !child.is_canceled());
        let waiter = tokio::spawn({
            let child = child.clone();
            async move { child.cancelled().await }
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        parent.cancel();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
        assert!(child.is_canceled());
        // 父令牌已取消时新的子令牌也是取消的
        assert!(parent.child().is_canceled());
    }
}
