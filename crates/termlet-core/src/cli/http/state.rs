//! HTTP 服务的状态：旧协议的会话（ID 中自带版本与客户端名，不存在内存中）、进行中的工具调用（取消用）、被拒绝的
//! 请求的日志（限频）。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use data_encoding::BASE32_NOPAD;
use termlet_mcp::Session;
use termlet_mcp::protocol::legacy_version;
use tokio::sync::watch;

/// 同一原因的拒绝隔多久才再记一条日志。
const REJECT_LOG_EVERY: Duration = Duration::from_secs(60);

/// 一次握手的会话 ID：128 位随机数（`random_token(16)`，区分各次握手，按会话取消时用），后面是 base32 写的
/// 版本与客户端名（`<随机>.<base32(版本 + "\n" + 客户端名)>`，都是可见的 ASCII）。会话只用来记这两样、不用于鉴权，
/// 写进 ID 中就不用在内存中保存：Termlet 重启后客户端拿旧 ID 来，照样认得出版本与客户端名，不用回 404。
pub(super) fn session_id(session: &Session) -> String {
    let info = format!("{}\n{}", session.version, session.client);
    format!(
        "{}.{}",
        crate::id::random_token(16),
        BASE32_NOPAD.encode(info.as_bytes()).to_ascii_lowercase()
    )
}

/// 会话 ID 中的版本与客户端名；不是 [`session_id`] 写的（版本不认识、解不开）时为 `None`。客户端名是客户端自报的，
/// ID 也可以由客户端随意写：照样清洗，只用于显示与审计。
pub(super) fn session(id: &str) -> Option<Session> {
    let (_, info) = id.rsplit_once('.')?;
    let bytes = BASE32_NOPAD
        .decode(info.to_ascii_uppercase().as_bytes())
        .ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let (version, client) = text.split_once('\n')?;
    Some(Session {
        version: legacy_version(version)?,
        client: termlet_mcp::client_name(Some(&serde_json::json!({ "name": client }))),
    })
}

#[derive(Default)]
pub(super) struct State {
    calls: Mutex<Calls>,
    rejections: Mutex<HashMap<Rejected, Throttle>>,
}

impl State {
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

    /// 被拒绝的请求：同一原因每分钟最多记一条日志（带上期间省略的次数），不记内容与令牌。局域网（或别处）来的连接
    /// 记下对方的地址（`peer`），回环来的不记。
    pub fn reject(&self, reason: Rejected, peer: Option<IpAddr>) {
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
        match peer {
            Some(peer) => tracing::warn!(
                reason = reason.as_str(),
                suppressed,
                %peer,
                "拒绝了一个 MCP 的 HTTP 请求：{}",
                reason.describe()
            ),
            None => tracing::warn!(
                reason = reason.as_str(),
                suppressed,
                "拒绝了一个 MCP 的 HTTP 请求：{}",
                reason.describe()
            ),
        }
    }
}

/// 拒绝的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Rejected {
    /// 连接不是来自回环地址或私有网段（在读请求之前关闭）
    Peer,
    Host,
    Origin,
    Token,
    TooLarge,
    TooMany,
}

impl Rejected {
    fn as_str(self) -> &'static str {
        match self {
            Rejected::Peer => "peer",
            Rejected::Host => "host",
            Rejected::Origin => "origin",
            Rejected::Token => "token",
            Rejected::TooLarge => "too_large",
            Rejected::TooMany => "too_many",
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Rejected::Peer => "连接不是来自本机或局域网的私有网段，已关闭",
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

    // 会话 ID：每次握手不同，自带版本与客户端名（Termlet 重启后照样解得出）；不是这样写的、版本不认识的不认
    #[test]
    fn session_ids_carry_version_and_client() {
        let first = session_id(&session("Visual Studio Code"));
        let second = session_id(&session("Visual Studio Code"));
        assert_ne!(first, second, "每次握手不同");
        assert!(
            first.bytes().all(|b| (0x21..=0x7e).contains(&b)),
            "可见 ASCII：{first}"
        );
        assert_eq!(super::session(&first), Some(session("Visual Studio Code")));
        let unnamed = session_id(&session(""));
        assert_eq!(super::session(&unnamed).unwrap().client, "");
        let chinese = session_id(&session("客户端"));
        assert_eq!(super::session(&chinese).unwrap().client, "客户端");
        for bad in ["", "unknown-session", "abc.!!!", "abc.mzxw6"] {
            assert_eq!(super::session(bad), None, "{bad}");
        }
        // 客户端自己写的 ID：版本不认识的不认，客户端名照样清洗
        let forged = |info: &str| {
            format!(
                "x.{}",
                BASE32_NOPAD.encode(info.as_bytes()).to_ascii_lowercase()
            )
        };
        assert_eq!(super::session(&forged("2099-01-01\nx")), None);
        assert_eq!(
            super::session(&forged("2025-06-18\na < b")).unwrap().client,
            "a  b"
        );
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
