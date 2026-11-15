//! 打开 SSH 终端：连接（或共用连接组里的连接）、按主机设置接入 tmux、转发 ssh-agent 与 X11；
//! 断线重连的退避间隔；关闭 Tab 时结束 tmux 会话；测试连接。

use std::sync::Arc;
use std::time::{Duration, Instant};

use termlet_io::{GridSize, TerminalIo};

use crate::SESSION_ENV;
use crate::id::HostId;
use crate::model::{Encoding, Host, Settings};
use crate::quick::QuickTarget;
use crate::ssh::SshError;
use crate::ssh::connection::{ConnectOptions, Connection, connect};
use crate::ssh::exec::exec;
use crate::ssh::plan::{ConnectPlan, plan_for_host, plan_for_quick};
use crate::ssh::pool::ConnectionPool;
use crate::ssh::prompt::{Prompter, no_prompt};
use crate::ssh::shell::{ShellOptions, open_shell_reporting};
use crate::ssh::x11::{X11Options, X11Status};
use crate::store::Store;

/// 检测 tmux 的最长时间。
const TMUX_CHECK_TIMEOUT: Duration = Duration::from_secs(10);
/// 结束 tmux 会话的最长时间。
const TMUX_KILL_TIMEOUT: Duration = Duration::from_secs(10);

/// 打开终端所需的全部信息（来自主机设置与全局设置）。
#[derive(Clone, Debug)]
pub struct TerminalSpec {
    pub plan: ConnectPlan,
    pub connect: ConnectOptions,
    pub encoding: Encoding,
    /// 连到以会话标识命名的 tmux 会话，重连后回到原来的现场
    pub tmux: bool,
    pub agent_forward: bool,
    /// 只用一个通道（堡垒机）：不共用连接、不另开通道（因此也不检测 tmux）
    pub single_channel: bool,
    /// 转发 X11（主机开启了时）
    pub x11: Option<X11Options>,
}

impl TerminalSpec {
    /// 保存的主机。
    pub fn for_host(store: &Store, host: &HostId) -> Result<Self, SshError> {
        let record = store
            .get::<Host>(host)
            .ok_or_else(|| SshError::Config("主机不存在".into()))?;
        let doc = &record.doc;
        Ok(Self {
            plan: plan_for_host(store, host)?,
            connect: connect_options(&store.settings()),
            encoding: doc.terminal.encoding,
            tmux: doc.terminal.tmux,
            agent_forward: doc.ssh.agent_forward,
            single_channel: doc.ssh.single_channel,
            x11: doc.ssh.x11_forward.then(|| X11Options {
                display: X11Options::local_display(&store.local_settings().connection.x11_display),
                trusted: doc.ssh.x11_trusted,
            }),
        })
    }

    /// 快速连接（临时主机）。
    pub fn for_quick(store: &Store, target: &QuickTarget) -> Self {
        Self {
            plan: plan_for_quick(target),
            connect: connect_options(&store.settings()),
            encoding: Encoding::Utf8,
            tmux: false,
            agent_forward: false,
            single_channel: false,
            x11: None,
        }
    }
}

/// 设置中的连接选项。
pub fn connect_options(settings: &Settings) -> ConnectOptions {
    let connection = &settings.connection;
    ConnectOptions {
        keepalive: (connection.keepalive_seconds > 0)
            .then(|| Duration::from_secs(connection.keepalive_seconds as u64)),
        agent_path: Some(connection.agent_path.trim().to_string()).filter(|p| !p.is_empty()),
    }
}

/// 一个终端 Tab 的身份：重连时不变。
#[derive(Clone, Debug)]
pub struct TerminalSession {
    /// 会话标识：tmux 会话名、会话标记；只含小写字母、数字、短横线
    pub key: String,
    /// 连接组（复制会话、分屏的终端共用连接）
    pub group: String,
}

impl TerminalSession {
    pub fn new() -> Self {
        Self {
            key: format!("termlet-{}", crate::id::random_token(6)),
            group: ConnectionPool::new_group(),
        }
    }

    /// 复制会话：共用这个终端的连接，tmux 会话另起一个。
    pub fn duplicate(&self) -> Self {
        Self {
            key: Self::new().key,
            group: self.group.clone(),
        }
    }

    /// 强制重连：换一个连接组（不再与别的终端共用旧连接），tmux 会话不变。
    pub fn with_new_group(&self) -> Self {
        Self {
            key: self.key.clone(),
            group: ConnectionPool::new_group(),
        }
    }
}

impl Default for TerminalSession {
    fn default() -> Self {
        Self::new()
    }
}

/// tmux 的情况。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TmuxStatus {
    /// 主机没有设置 tmux
    Off,
    /// 在 tmux 会话中
    Attached,
    /// 服务器没有安装 tmux，用了普通 shell
    Unavailable,
    /// 没能检查（超时、通道数已满），这次用了普通 shell
    Unchecked,
}

/// 打开的终端。
pub struct OpenedTerminal {
    pub connection: Arc<Connection>,
    pub io: TerminalIo,
    pub tmux: TmuxStatus,
    pub x11: X11Status,
}

/// 打开终端：取连接组里还能用的连接，没有时新建；按主机设置接入 tmux。
pub async fn open_terminal(
    store: Arc<Store>,
    spec: &TerminalSpec,
    session: &TerminalSession,
    size: GridSize,
    prompter: Prompter,
) -> Result<OpenedTerminal, SshError> {
    let target = spec
        .plan
        .hops
        .last()
        .map(|h| h.label.as_str())
        .unwrap_or_default();
    tracing::debug!(
        host = %target,
        session = %session.key,
        group = %session.group,
        single_channel = spec.single_channel,
        tmux = spec.tmux,
        cols = size.columns,
        rows = size.rows,
        "打开 SSH 终端"
    );
    let connection = if spec.single_channel {
        connect(store, &spec.plan, prompter, &spec.connect).await?
    } else {
        ConnectionPool::global()
            .acquire(&session.group, || {
                connect(store, &spec.plan, prompter, &spec.connect)
            })
            .await?
    };
    let tmux = if !spec.tmux || spec.single_channel {
        TmuxStatus::Off
    } else {
        match detect_tmux(&connection).await {
            Ok(true) => TmuxStatus::Attached,
            Ok(false) => TmuxStatus::Unavailable,
            Err(e) => {
                tracing::warn!("没能检查服务器上是否安装了 tmux，使用普通 shell：{e}");
                TmuxStatus::Unchecked
            }
        }
    };
    let options = ShellOptions {
        size,
        encoding: spec.encoding,
        env: if spec.single_channel {
            Vec::new()
        } else {
            vec![(SESSION_ENV.to_string(), session.key.clone())]
        },
        command: (tmux == TmuxStatus::Attached).then(|| tmux_attach_command(&session.key)),
        agent_forward: spec.agent_forward,
        x11: spec.x11.clone(),
        ..Default::default()
    };
    let (io, x11) = open_shell_reporting(connection.clone(), options)
        .await
        .inspect_err(
            |e| tracing::warn!(host = %target, session = %session.key, "打开远端 shell 失败：{e}"),
        )?;
    tracing::info!(host = %target, session = %session.key, tmux = ?tmux, x11 = ?x11, "SSH 终端已打开");
    Ok(OpenedTerminal {
        connection,
        io,
        tmux,
        x11,
    })
}

/// 服务器上有没有 tmux：出错时稍等再试一次（通道数暂时满了时，名额可能很快空出来）。
async fn detect_tmux(connection: &Connection) -> Result<bool, SshError> {
    let check = || {
        exec(
            connection,
            "command -v tmux >/dev/null 2>&1",
            1024,
            TMUX_CHECK_TIMEOUT,
        )
    };
    let result = match check().await {
        Ok(result) => result,
        Err(_) => {
            tokio::time::sleep(Duration::from_millis(500)).await;
            check().await?
        }
    };
    Ok(result.exit_code == Some(0))
}

/// tmux 会话名只含字母、数字、短横线，拼进命令无需转义。
fn valid_tmux_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn tmux_attach_command(name: &str) -> String {
    debug_assert!(valid_tmux_name(name));
    format!("exec tmux new-session -A -s {name}")
}

/// 关闭 Tab 时结束它的 tmux 会话（连接断开时不调用：会话保留，重连后接着用）。
/// 原来的连接不能用时（正在重连）用一条新连接，不询问用户（需要确认指纹、输入验证码时放弃）。
pub async fn kill_tmux(
    store: Arc<Store>,
    spec: &TerminalSpec,
    name: &str,
    connection: Option<Arc<Connection>>,
) -> Result<(), SshError> {
    if !valid_tmux_name(name) {
        return Err(SshError::Config(format!("tmux 会话名不合法：{name}")));
    }
    let command = format!("tmux kill-session -t {name}");
    let reuse = connection.as_ref().is_some_and(|c| !c.is_closed());
    tracing::debug!(session = %name, reuse_connection = reuse, "结束 tmux 会话");
    let connection = match connection.filter(|c| !c.is_closed()) {
        Some(connection) => connection,
        None => connect(store, &spec.plan, no_prompt(), &spec.connect).await?,
    };
    exec(&connection, &command, 1024, TMUX_KILL_TIMEOUT)
        .await
        .inspect_err(|e| tracing::warn!(session = %name, "结束 tmux 会话失败：{e}"))?;
    Ok(())
}

/// 断线重连的第 `attempt` 次（从 1 起）之前等多久：1、2、4……最多 30 秒。
pub fn reconnect_delay(attempt: u32) -> Duration {
    let seconds = 1u64
        .checked_shl(attempt.saturating_sub(1))
        .unwrap_or(u64::MAX);
    Duration::from_secs(seconds.min(30))
}

/// 测试连接的结果。
#[derive(Clone, Debug)]
pub struct TestReport {
    /// 建立连接并完成认证的耗时（不含等待用户回答的时间）
    pub elapsed: Duration,
    pub server_version: String,
}

/// 测试连接：按计划连接并完成认证，随即断开。首次连接同样请用户确认指纹（确认后记住）。
pub async fn test_connection(
    store: Arc<Store>,
    plan: &ConnectPlan,
    prompter: Prompter,
    options: &ConnectOptions,
) -> Result<TestReport, SshError> {
    let waited = Arc::new(std::sync::Mutex::new(Duration::ZERO));
    let timed: Prompter = {
        let waited = waited.clone();
        Arc::new(move |prompt| {
            let prompter = prompter.clone();
            let waited = waited.clone();
            Box::pin(async move {
                let started = Instant::now();
                let answer = prompter(prompt).await;
                *waited.lock().unwrap() += started.elapsed();
                answer
            })
        })
    };
    let options = ConnectOptions {
        keepalive: None,
        ..options.clone()
    };
    let started = Instant::now();
    let connection = connect(store, plan, timed, &options).await?;
    let elapsed = started.elapsed().saturating_sub(*waited.lock().unwrap());
    let report = TestReport {
        elapsed: elapsed.max(Duration::from_millis(1)),
        server_version: connection.server_version().to_string(),
    };
    connection.disconnect().await;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_double_up_to_thirty_seconds() {
        let delays: Vec<u64> = (1..=7).map(|n| reconnect_delay(n).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30]);
        assert_eq!(reconnect_delay(200).as_secs(), 30);
        assert_eq!(reconnect_delay(0).as_secs(), 1);
    }

    #[test]
    fn session_keys_are_valid_tmux_names() {
        let session = TerminalSession::new();
        assert!(valid_tmux_name(&session.key), "{}", session.key);
        assert!(session.key.starts_with("termlet-"));
        let copy = session.duplicate();
        assert_eq!(copy.group, session.group);
        assert_ne!(copy.key, session.key);
        let forced = session.with_new_group();
        assert_eq!(forced.key, session.key);
        assert_ne!(forced.group, session.group);
        assert!(!valid_tmux_name("a;rm -rf /"));
        assert!(!valid_tmux_name(""));
    }
}
