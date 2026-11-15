//! SFTP 会话：一台主机上的 SFTP 子系统（或经别的方式连上的 SFTP 服务端）。目录操作、修改权限、在线编辑的读写
//! 都在这里；上传下载由管理器排队（见 [`super::transfer`]）。
//!
//! 断线（连接断开，或只是服务器上的 SFTP 子系统退出）后，经 SSH 打开的会话自动重连，会话 ID 不变，
//! 状态经 [`SftpSession::watch_state`] 报告给界面；重连后继续因断线失败的传输。

use std::fmt;
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;

use futures::StreamExt;
use futures::future::BoxFuture;
use tokio::sync::{Semaphore, watch};

use super::backend::{Backend, Protocol};
use super::cancel::Cancel;
use super::client::{SftpClient, is_dir, is_regular, is_symlink, perm_attrs};
use super::entry::{Entry, EntryKind, Listing};
use super::error::{self, Rejection, SftpError};
use super::manager::Inner;
use super::path;
use super::walk::{RemoteWalker, WalkError};
use crate::ssh::Connection;

/// 会话的 ID（`sftp_` 加随机串），重连后不变。
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(String);

impl SessionId {
    pub(crate) fn new() -> Self {
        Self(format!("sftp_{}", crate::id::random_token(8)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 会话的连接状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionState {
    Connected,
    /// 连接断开，正在自动重连（第 `attempt` 次）
    Reconnecting {
        attempt: u32,
    },
    /// 已关闭：用户关闭（`error` 为空），或断开后没能重连（重连也不会成功的错误，或超过了 10 分钟）
    Closed {
        error: Option<SftpError>,
    },
}

/// 会话的摘要。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: SessionId,
    pub host: String,
    pub owner: String,
    /// 登录后的初始目录
    pub home: String,
    /// 所用 SSH 连接的组（与终端共用连接时是终端的组）；不经 SSH 打开的会话没有
    pub group: Option<String>,
    pub state: SessionState,
    /// SFTP，或服务器没有 SFTP 时的 SCP 模式
    pub protocol: Protocol,
}

/// 修改权限作用于哪些项（递归时）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChmodScope {
    /// 文件与目录（默认）：目录在有读权限的位置加上执行权限（与 chmod 的 `X` 相同，否则目录无法进入）
    #[default]
    All,
    /// 只改目录（权限原样使用）
    DirsOnly,
    /// 只改文件
    FilesOnly,
}

/// 修改权限。`mode` 为八进制权限位（0–07777）；`recursive` 时目录中的文件与子目录一并修改，符号链接跳过。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChmodRequest {
    pub paths: Vec<String>,
    pub mode: u32,
    pub recursive: bool,
    pub scope: ChmodScope,
}

/// 一次连接的结果：SFTP 客户端（或 SCP 模式）与它所在的 SSH 连接（不经 SSH 时没有）。
pub(crate) struct Link {
    pub backend: Backend,
    pub connection: Option<Arc<Connection>>,
    pub group: Option<String>,
}

/// 断线后重新建立连接。
pub(crate) type Redial = Arc<dyn Fn() -> BoxFuture<'static, Result<Link, SftpError>> + Send + Sync>;

/// 一个 SFTP 会话。由 [`super::SftpManager`] 打开，界面持有 `Arc<SftpSession>` 调用目录操作。
pub struct SftpSession {
    pub(crate) id: SessionId,
    pub(crate) host: String,
    /// 所属窗口；Tab 移到别的窗口时随之改变（[`super::SftpManager::reassign_owner`]）
    owner: Mutex<String>,
    home: String,
    group: Mutex<Option<String>>,
    backend: RwLock<Backend>,
    connection: Mutex<Option<Arc<Connection>>>,
    pub(crate) redial: Option<Redial>,
    /// 会话关闭时取消（其上的传输随之取消）
    pub(crate) closing: Cancel,
    state: watch::Sender<SessionState>,
    /// 每次断线（标记为正在重连）加一：因断线失败的传输等它变化后再记下失败
    lost_epoch: watch::Sender<u64>,
    /// 同时进行的传输数
    pub(crate) slots: Arc<Semaphore>,
    pub(crate) per_session: usize,
    pub(crate) manager: Weak<Inner>,
}

impl fmt::Debug for SftpSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SftpSession")
            .field("id", &self.id)
            .field("host", &self.host)
            .field("owner", &*self.owner.lock().unwrap())
            .field("state", &*self.state.borrow())
            .finish()
    }
}

impl SftpSession {
    pub(crate) fn new(
        manager: &Arc<Inner>,
        host: String,
        owner: String,
        home: String,
        link: Link,
        redial: Option<Redial>,
    ) -> Self {
        let per_session = manager.options.per_session.max(1);
        Self {
            id: SessionId::new(),
            host,
            owner: Mutex::new(owner),
            home,
            group: Mutex::new(link.group),
            backend: RwLock::new(link.backend),
            connection: Mutex::new(link.connection),
            redial,
            closing: Cancel::new(),
            state: watch::channel(SessionState::Connected).0,
            lost_epoch: watch::channel(0).0,
            slots: Arc::new(Semaphore::new(per_session)),
            per_session,
            manager: Arc::downgrade(manager),
        }
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }

    /// 主机的标识（打开会话时给的）。
    pub fn host(&self) -> &str {
        &self.host
    }

    /// 所属窗口。
    pub fn owner(&self) -> String {
        self.owner.lock().unwrap().clone()
    }

    /// 换一个所属窗口，返回原来的。
    pub(crate) fn set_owner(&self, owner: &str) -> String {
        std::mem::replace(&mut *self.owner.lock().unwrap(), owner.to_string())
    }

    /// 登录后的初始目录。
    pub fn home(&self) -> &str {
        &self.home
    }

    pub fn state(&self) -> SessionState {
        self.state.borrow().clone()
    }

    /// 订阅连接状态的变化。
    pub fn watch_state(&self) -> watch::Receiver<SessionState> {
        self.state.subscribe()
    }

    pub fn info(&self) -> SessionInfo {
        SessionInfo {
            id: self.id.clone(),
            host: self.host.clone(),
            owner: self.owner(),
            home: self.home.clone(),
            group: self.group.lock().unwrap().clone(),
            state: self.state(),
            protocol: self.protocol(),
        }
    }

    /// SFTP，或服务器没有 SFTP 时的 SCP 模式（重连后可能变化，界面随状态的变化重读）。
    pub fn protocol(&self) -> Protocol {
        self.backend().protocol()
    }

    /// 会话所在的 SSH 连接（不经 SSH 打开的会话没有；正在重连时是旧的）。
    pub fn connection(&self) -> Option<Arc<Connection>> {
        self.connection.lock().unwrap().clone()
    }

    pub(crate) fn backend(&self) -> Backend {
        self.backend.read().unwrap().clone()
    }

    /// 测试中直接操作 SFTP 客户端（SCP 模式的会话没有）。
    #[cfg(test)]
    pub(crate) fn client(&self) -> Arc<SftpClient> {
        match self.backend() {
            Backend::Sftp(client) => client,
            Backend::Scp(_) => panic!("SCP 模式的会话没有 SFTP 客户端"),
        }
    }

    pub(crate) fn is_reconnecting(&self) -> bool {
        matches!(*self.state.borrow(), SessionState::Reconnecting { .. })
    }

    /// 还能用的客户端；会话已关闭时报错。
    fn live(&self) -> Result<Backend, SftpError> {
        if self.closing.is_canceled() {
            return Err(SftpError::SessionClosed);
        }
        Ok(self.backend())
    }

    pub(crate) fn set_state(&self, state: SessionState) {
        self.state.send_replace(state.clone());
        if let Some(manager) = self.manager.upgrade() {
            manager.session_changed(&self.id, state);
        }
    }

    /// 标记为正在重连（断线的传输据此知道会自动继续）。
    pub(crate) fn mark_reconnecting(&self, attempt: u32) {
        if attempt == 1 {
            self.lost_epoch.send_modify(|n| *n += 1);
        }
        self.set_state(SessionState::Reconnecting { attempt });
    }

    pub(crate) fn lost_epoch(&self) -> u64 {
        *self.lost_epoch.borrow()
    }

    /// 传输因断线失败后、记下失败之前调用：能自动重连的会话等到它被标记为正在重连（`epoch` 之后又断开过），最多等 `wait`。
    /// 否则界面可能恰好在两者之间读到传输列表，把这个马上会自动继续的任务当作失败。
    pub(crate) async fn await_lost(&self, epoch: u64, wait: Duration) {
        if self.redial.is_none() || self.is_reconnecting() {
            return;
        }
        let mut rx = self.lost_epoch.subscribe();
        tokio::select! {
            _ = rx.wait_for(|n| *n > epoch) => {}
            _ = self.closing.cancelled() => {}
            _ = tokio::time::sleep(wait) => {}
        }
    }

    /// 重连成功：换上新的客户端与连接。
    pub(crate) fn replace_link(&self, link: Link) {
        let old = std::mem::replace(&mut *self.backend.write().unwrap(), link.backend);
        old.close();
        *self.connection.lock().unwrap() = link.connection;
        *self.group.lock().unwrap() = link.group;
    }

    /// 关闭客户端，放开连接。
    pub(crate) fn shutdown(&self) {
        self.backend().close();
        self.connection.lock().unwrap().take();
    }

    /// 把空路径解析为初始目录，把 `~` 开头的路径展开到初始目录，相对路径按初始目录解析，并规范化为绝对路径。
    pub fn resolve(&self, p: &str) -> String {
        if p.is_empty() || p == "~" {
            return self.home.clone();
        }
        if let Some(rest) = p.strip_prefix("~/") {
            return path::join(&self.home, rest);
        }
        if !p.starts_with('/') {
            return path::join(&self.home, p);
        }
        path::clean(p)
    }

    /// 服务器解析后的绝对路径（展开符号链接）。
    pub async fn realpath(&self, p: &str) -> Result<String, SftpError> {
        let client = self.live()?;
        let p = self.resolve(p);
        client.realpath(&p).await.map_err(|e| error::remote(&p, &e))
    }

    /// 列出目录。符号链接按目标判断是否为目录，权限也取目标的。
    pub async fn list(&self, p: &str) -> Result<Listing, SftpError> {
        let client = match self.live()? {
            Backend::Sftp(client) => client,
            Backend::Scp(remote) => {
                let dir = self.resolve(p);
                let listing = remote.list(&dir).await.map_err(|e| {
                    tracing::debug!(session = %self.id, dir = %dir, "列出远程目录失败（SCP）：{e}");
                    error::remote(&dir, &e)
                })?;
                tracing::debug!(
                    session = %self.id,
                    dir = %listing.path,
                    entries = listing.entries.len(),
                    "列出远程目录（SCP）"
                );
                return Ok(listing);
            }
        };
        let mut dir = self.resolve(p);
        if let Ok(resolved) = client.realpath(&dir).await {
            dir = resolved;
        }
        let items = client.read_dir(&dir).await.map_err(|e| {
            tracing::debug!(session = %self.id, dir = %dir, "列出远程目录失败：{e}");
            error::remote(&dir, &e)
        })?;
        tracing::debug!(session = %self.id, dir = %dir, entries = items.len(), "列出远程目录");
        let client = &client;
        let dir_ref = &dir;
        let entries: Vec<Entry> = futures::stream::iter(items)
            .map(|item| async move {
                let mut entry = Entry::from_item(&item);
                if entry.is_link() {
                    let target = client.stat(&path::join(dir_ref, &item.name)).await.ok();
                    entry.resolve_link(target.as_ref());
                }
                entry
            })
            .buffered(32)
            .collect()
            .await;
        Ok(Listing { path: dir, entries })
    }

    /// 一项的属性（符号链接同时给出目标的种类与权限）。
    pub async fn stat(&self, p: &str) -> Result<Entry, SftpError> {
        let client = self.live()?;
        let p = self.resolve(p);
        let attrs = client.lstat(&p).await.map_err(|e| error::remote(&p, &e))?;
        let mut entry = Entry::from_attrs(path::base_name(&p), &attrs);
        if entry.is_link() {
            let target = client.stat(&p).await.ok();
            entry.resolve_link(target.as_ref());
        }
        Ok(entry)
    }

    /// 是否已存在（不跟随符号链接；悬空链接也算）。上传前检查同名项、重命名前检查目标时用。
    pub async fn exists(&self, p: &str) -> Result<bool, SftpError> {
        let client = self.live()?;
        let p = self.resolve(p);
        match client.lstat(&p).await {
            Ok(_) => Ok(true),
            Err(e) if error::is_not_found(&e) => Ok(false),
            Err(e) => Err(error::remote(&p, &e)),
        }
    }

    /// 新建目录（上级目录必须存在）。
    pub async fn mkdir(&self, p: &str) -> Result<(), SftpError> {
        let client = self.live()?;
        let p = self.resolve(p);
        if client.lstat(&p).await.is_ok() {
            return Err(SftpError::AlreadyExists {
                side: error::Side::Remote,
                path: p,
            });
        }
        tracing::debug!(session = %self.id, path = %p, "新建远程目录");
        client.mkdir(&p).await.map_err(|e| {
            tracing::debug!(session = %self.id, path = %p, "新建远程目录失败：{e}");
            error::remote(&p, &e)
        })
    }

    /// 重命名或移动；目标已存在时拒绝，避免误覆盖。
    pub async fn rename(&self, from: &str, to: &str) -> Result<(), SftpError> {
        let client = self.live()?;
        let (from, to) = (self.resolve(from), self.resolve(to));
        if client.lstat(&to).await.is_ok() {
            return Err(SftpError::AlreadyExists {
                side: error::Side::Remote,
                path: to,
            });
        }
        tracing::debug!(session = %self.id, from = %from, to = %to, "重命名远程文件");
        client.rename(&from, &to).await.map_err(|e| {
            tracing::debug!(session = %self.id, from = %from, to = %to, "重命名远程文件失败：{e}");
            error::remote(&from, &e)
        })
    }

    /// 删除文件或目录（目录递归删除）；符号链接只删除链接本身。不能删除根目录与初始目录。
    pub async fn remove(&self, p: &str) -> Result<(), SftpError> {
        let backend = self.live()?;
        let p = self.resolve(p);
        tracing::debug!(session = %self.id, path = %p, "删除远程文件或目录");
        let result = match backend {
            Backend::Sftp(client) => remove_tree(&client, p.clone(), &self.home).await,
            Backend::Scp(remote) => {
                if p == "/" || p == self.home {
                    return Err(Rejection::RemoveRootOrHome.into());
                }
                remote
                    .remove_tree(&p)
                    .await
                    .map_err(|e| error::remote(&p, &e))
            }
        };
        result.inspect_err(|e| tracing::debug!(session = %self.id, path = %p, "删除失败：{e}"))
    }

    /// 修改权限（见 [`ChmodRequest`]）。遇错即停：之前的项已经改好。
    pub async fn chmod(&self, request: &ChmodRequest) -> Result<(), SftpError> {
        if request.paths.is_empty() || request.mode > 0o7777 {
            return Err(Rejection::InvalidMode.into());
        }
        let Backend::Sftp(client) = self.live()? else {
            return Err(Rejection::ScpUnsupported.into());
        };
        let mode = request.mode;
        tracing::debug!(
            session = %self.id,
            paths = request.paths.len(),
            mode = format!("{mode:o}"),
            recursive = request.recursive,
            scope = ?request.scope,
            "修改远程权限"
        );
        let mut dir_mode = mode;
        for r in [0o400, 0o040, 0o004] {
            if mode & r != 0 {
                dir_mode |= r >> 2;
            }
        }
        let wanted = |kind: EntryKind| match request.scope {
            ChmodScope::All => true,
            ChmodScope::DirsOnly => kind == EntryKind::Dir,
            ChmodScope::FilesOnly => kind != EntryKind::Dir,
        };
        for p in &request.paths {
            let p = self.resolve(p);
            let attrs = client.stat(&p).await.map_err(|e| error::remote(&p, &e))?;
            let kind = EntryKind::of(&attrs);
            if !request.recursive || !is_dir(&attrs) {
                if wanted(kind) {
                    client
                        .setstat(&p, perm_attrs(mode))
                        .await
                        .map_err(|e| error::remote(&p, &e))?;
                }
                continue;
            }
            let backend = Backend::Sftp(client.clone());
            let mut walker = RemoteWalker::new(&backend, &p, false);
            while let Some(item) = walker.next().await {
                let item = match item {
                    Ok(item) => item,
                    Err(WalkError::Fatal(e)) => return Err(e),
                    Err(WalkError::Unreadable(path)) => {
                        return Err(SftpError::PermissionDenied {
                            side: error::Side::Remote,
                            path,
                        });
                    }
                };
                // 根路径的属性跟随了链接；子项中的链接跳过
                let kind = if item.path == p {
                    kind
                } else {
                    EntryKind::of(&item.attrs)
                };
                if kind == EntryKind::Symlink || !wanted(kind) {
                    continue;
                }
                let bits = if kind == EntryKind::Dir && request.scope == ChmodScope::All {
                    dir_mode
                } else {
                    mode
                };
                client
                    .setstat(&item.path, perm_attrs(bits))
                    .await
                    .map_err(|e| error::remote(&item.path, &e))?;
            }
        }
        Ok(())
    }
}

/// 递归删除。
fn remove_tree<'a>(
    client: &'a SftpClient,
    p: String,
    home: &'a str,
) -> BoxFuture<'a, Result<(), SftpError>> {
    Box::pin(async move {
        if p == "/" || p == home {
            return Err(Rejection::RemoveRootOrHome.into());
        }
        let attrs = client.lstat(&p).await.map_err(|e| error::remote(&p, &e))?;
        if !is_dir(&attrs) {
            return client.remove(&p).await.map_err(|e| error::remote(&p, &e));
        }
        let items = client
            .read_dir(&p)
            .await
            .map_err(|e| error::remote(&p, &e))?;
        for item in items {
            remove_tree(client, path::join(&p, &item.name), home).await?;
        }
        client.rmdir(&p).await.map_err(|e| error::remote(&p, &e))
    })
}

/// 逐级创建远程目录（已存在的跳过，跟随符号链接）。
pub(crate) async fn mkdir_all(client: &SftpClient, dir: &str) -> Result<(), SftpError> {
    let mut missing = Vec::new();
    let mut current = dir.to_string();
    loop {
        match client.stat(&current).await {
            Ok(attrs) if is_dir(&attrs) => break,
            Ok(_) => {
                return Err(SftpError::AlreadyExists {
                    side: error::Side::Remote,
                    path: current,
                });
            }
            Err(e) if error::is_not_found(&e) => {
                let parent = path::parent(&current);
                missing.push(current);
                if parent == "/" || parent == "." {
                    break;
                }
                current = parent;
            }
            Err(e) => return Err(error::remote(&current, &e)),
        }
    }
    for p in missing.into_iter().rev() {
        if let Err(e) = client.mkdir(&p).await {
            // 同时有别人建好了
            if !matches!(client.stat(&p).await, Ok(attrs) if is_dir(&attrs)) {
                return Err(error::remote(&p, &e));
            }
        }
    }
    Ok(())
}

/// 指向普通文件的符号链接所指的文件：写到它旁边再改名，链接保持不变。
pub(crate) async fn link_target(client: &SftpClient, p: &str) -> Option<String> {
    let attrs = client.lstat(p).await.ok()?;
    if !is_symlink(&attrs) {
        return None;
    }
    let target = client.realpath(p).await.ok()?;
    let st = client.stat(&target).await.ok()?;
    is_regular(&st).then_some(target)
}
