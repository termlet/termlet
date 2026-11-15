//! 连接复用：共用一条连接的终端（复制会话、分屏）属于同一个"连接组"。
//!
//! 连接断开后组里的终端各自重连时，第一个建好新连接，其余的等它、然后共用，只认证一次。
//! 池里只放弱引用：组里的终端都关闭后，连接随之断开。

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use crate::ssh::SshError;
use crate::ssh::connection::Connection;

#[derive(Default)]
struct Group {
    connection: Weak<Connection>,
    /// 同一组同时只建一条连接
    connecting: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Default)]
pub struct ConnectionPool {
    groups: Mutex<HashMap<String, Group>>,
}

impl ConnectionPool {
    /// 进程中的连接池。
    pub fn global() -> &'static ConnectionPool {
        static POOL: OnceLock<ConnectionPool> = OnceLock::new();
        POOL.get_or_init(ConnectionPool::default)
    }

    /// 新的连接组 ID。
    pub fn new_group() -> String {
        crate::id::random_token(10)
    }

    /// 组里还能用的连接。
    pub fn get(&self, group: &str) -> Option<Arc<Connection>> {
        let groups = self.groups.lock().unwrap();
        groups
            .get(group)
            .and_then(|g| g.connection.upgrade())
            .filter(|c| !c.is_closed())
    }

    /// 取组里还能用的连接；没有时用 `connect` 建一条放进组里（同一组同时只建一条，其余的等待后共用）。
    pub async fn acquire<F, Fut>(
        &self,
        group: &str,
        connect: F,
    ) -> Result<Arc<Connection>, SshError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Arc<Connection>, SshError>>,
    {
        if let Some(connection) = self.get(group) {
            tracing::debug!(group, host = %connection.label(), "共用连接组中已有的连接");
            return Ok(connection);
        }
        let lock = {
            let mut groups = self.groups.lock().unwrap();
            groups
                .entry(group.to_string())
                .or_default()
                .connecting
                .clone()
        };
        let _guard = lock.lock().await;
        // 等锁期间别的终端可能已经建好
        if let Some(connection) = self.get(group) {
            tracing::debug!(group, host = %connection.label(), "共用连接组中刚建好的连接");
            return Ok(connection);
        }
        tracing::debug!(group, "连接组中没有可用的连接，新建");
        let connection = connect().await?;
        self.put(group, &connection);
        Ok(connection)
    }

    /// 把连接放进组里（替换断开的旧连接）。
    pub fn put(&self, group: &str, connection: &Arc<Connection>) {
        let mut groups = self.groups.lock().unwrap();
        groups.entry(group.to_string()).or_default().connection = Arc::downgrade(connection);
        // 顺便清掉没人用的组
        groups.retain(|key, g| {
            key == group || g.connection.strong_count() > 0 || Arc::strong_count(&g.connecting) > 1
        });
    }
}
