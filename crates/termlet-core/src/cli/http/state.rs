//! HTTP 服务在内存中的状态：旧协议的会话、进行中的工具调用（取消用）、被拒绝的请求的日志（限频）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use termlet_mcp::Session;
use tokio::sync::watch;

use super::MAX_SESSIONS;

/// 同一原因的拒绝隔多久才再记一条日志。
const REJECT_LOG_EVERY: Duration = Duration::from_secs(60);

#[derive(Default)]
pub(super) struct State {
    sessions: Mutex<Sessions>,
    calls: Mutex<Calls>,
    rejections: Mutex<HashMap<Rejected, Throttle>>,
}

impl State {
    /// 记住一次握手，返回新的会话 ID（128 位随机数）。
    pub fn create_session(&self, session: Session) -> String {
        lock(&self.sessions).create(session)
    }

    /// 会话 ID 对应的会话（不认识的为 `None`）。
    pub fn session(&self, id: &str) -> Option<Session> {
        lock(&self.sessions).get(id)
    }

    /// 记下一个进行中的工具调用；返回的登记丢弃时去掉。
    pub fn track(
        self: &Arc<Self>,
        session: Option<String>,
        id: String,
        gone: Arc<watch::Sender<bool>>,
    ) -> Tracked {
        let mut calls = lock(&self.calls);
        calls.next += 1;
        let serial = calls.next;
        calls.list.push(Call {
            serial,
            session,
            id,
            gone,
        });
        Tracked {
            state: self.clone(),
            serial,
        }
    }

    /// 旧协议的 `notifications/cancelled`：有会话时取消这个会话中这个 ID 的调用；没有会话（或会话已不认识）时只在
    /// 这个 ID 唯一时取消。返回取消了几个。
    pub fn cancel(&self, session: Option<&str>, id: &str) -> usize {
        let calls = lock(&self.calls);
        let matched: Vec<&Call> = calls
            .list
            .iter()
            .filter(|c| c.id == id && session.is_none_or(|s| c.session.as_deref() == Some(s)))
            .collect();
        if session.is_none() && matched.len() != 1 {
            return 0;
        }
        for call in &matched {
            call.gone.send_replace(true);
        }
        matched.len()
    }

    /// 被拒绝的请求：同一原因每分钟最多记一条日志（带上期间省略的次数），不记内容与令牌。
    pub fn reject(&self, reason: Rejected) {
        let mut rejections = lock(&self.rejections);
        let throttle = rejections.entry(reason).or_default();
        let now = Instant::now();
        if throttle
            .logged
            .is_some_and(|at| now.duration_since(at) < REJECT_LOG_EVERY)
        {
            throttle.suppressed += 1;
            return;
        }
        let suppressed = std::mem::take(&mut throttle.suppressed);
        throttle.logged = Some(now);
        tracing::warn!(
            reason = reason.as_str(),
            suppressed,
            "拒绝了一个 MCP 的 HTTP 请求：{}",
            reason.describe()
        );
    }
}

/// 拒绝的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Rejected {
    Host,
    Origin,
    Token,
    TooLarge,
    TooMany,
}

impl Rejected {
    fn as_str(self) -> &'static str {
        match self {
            Rejected::Host => "host",
            Rejected::Origin => "origin",
            Rejected::Token => "token",
            Rejected::TooLarge => "too_large",
            Rejected::TooMany => "too_many",
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Rejected::Host => "Host 头不是本机地址（可能是 DNS 重绑定）",
            Rejected::Origin => "带有 Origin 头（来自浏览器中的网页）",
            Rejected::Token => "没有令牌或令牌不对",
            Rejected::TooLarge => "正文超过上限",
            Rejected::TooMany => "同时的连接太多",
        }
    }
}

#[derive(Default)]
struct Throttle {
    logged: Option<Instant>,
    suppressed: u64,
}

/// 旧协议的会话：最多 [`MAX_SESSIONS`] 个，超出时去掉最久没用的。
#[derive(Default)]
struct Sessions {
    map: HashMap<String, (Session, u64)>,
    tick: u64,
}

impl Sessions {
    fn create(&mut self, session: Session) -> String {
        if self.map.len() >= MAX_SESSIONS
            && let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(id, _)| id.clone())
        {
            self.map.remove(&oldest);
        }
        let id = crate::id::random_token(16);
        self.tick += 1;
        self.map.insert(id.clone(), (session, self.tick));
        id
    }

    fn get(&mut self, id: &str) -> Option<Session> {
        self.tick += 1;
        let tick = self.tick;
        let (session, used) = self.map.get_mut(id)?;
        *used = tick;
        Some(session.clone())
    }
}

#[derive(Default)]
struct Calls {
    next: u64,
    list: Vec<Call>,
}

struct Call {
    serial: u64,
    /// 旧协议的会话 ID（新协议、没有会话时为空）
    session: Option<String>,
    /// 请求 ID（JSON 的写法）
    id: String,
    gone: Arc<watch::Sender<bool>>,
}

/// 进行中的工具调用的登记：丢弃时去掉。
pub(super) struct Tracked {
    state: Arc<State>,
    serial: u64,
}

impl Drop for Tracked {
    fn drop(&mut self) {
        lock(&self.state.calls)
            .list
            .retain(|c| c.serial != self.serial);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(client: &str) -> Session {
        Session {
            version: "2025-11-25",
            client: client.into(),
        }
    }

    // 会话最多 MAX_SESSIONS 个：超出时去掉最久没用的，用过的留下
    #[test]
    fn sessions_are_bounded_lru() {
        let mut sessions = Sessions::default();
        let first = sessions.create(session("first"));
        let second = sessions.create(session("second"));
        assert_eq!(first.len(), 26, "128 位的 base32");
        assert_ne!(first, second);
        for i in 2..MAX_SESSIONS {
            sessions.create(session(&format!("s{i}")));
        }
        // 用一下第一个，再建一个：被去掉的是第二个
        assert_eq!(sessions.get(&first).unwrap().client, "first");
        sessions.create(session("new"));
        assert_eq!(sessions.map.len(), MAX_SESSIONS);
        assert!(sessions.get(&first).is_some());
        assert!(sessions.get(&second).is_none());
        assert!(sessions.get("unknown").is_none());
    }

    // 取消：有会话时按（会话，ID）；没有会话时只在 ID 唯一时
    #[test]
    fn cancel_by_session_and_unique_id() {
        let state = Arc::new(State::default());
        let gone = |_: ()| Arc::new(watch::channel(false).0);
        let a = gone(());
        let b = gone(());
        let c = gone(());
        let _ta = state.track(Some("s1".into()), "7".into(), a.clone());
        let _tb = state.track(Some("s2".into()), "7".into(), b.clone());
        let tc = state.track(None, "9".into(), c.clone());
        assert_eq!(state.cancel(None, "7"), 0, "ID 不唯一时不取消");
        assert!(!*a.borrow() && !*b.borrow());
        assert_eq!(state.cancel(Some("s2"), "7"), 1);
        assert!(*b.borrow() && !*a.borrow());
        assert_eq!(state.cancel(Some("s1"), "8"), 0);
        drop(tc);
        assert_eq!(state.cancel(None, "9"), 0, "做完的调用已去掉");
        assert!(!*c.borrow());
        let d = gone(());
        let _td = state.track(None, "10".into(), d.clone());
        assert_eq!(state.cancel(None, "10"), 1);
        assert!(*d.borrow());
    }
}
