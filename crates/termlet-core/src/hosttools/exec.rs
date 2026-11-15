//! 在终端所在的连接上执行一次性命令（主机工具与资源监控共用）：SSH 终端另开 exec 通道，WSL 终端在同一发行版中
//! 以同一用户执行。
//!
//! - 脚本一律经 `sh -c` 执行（[`sh_script`]）：exec 请求交给用户的登录 shell 解释，登录 shell 是 fish、tcsh 时
//!   for、if、`$(…)` 等 POSIX 写法语法不通。远端数据与参数经 `$1…` 传入，不拼进脚本。
//! - 名额：每条 SSH 连接同时最多 [`MAX_SIDE_CHANNELS`] 个这样的通道，其余排队（sshd 每条连接默认最多 10 个会话，
//!   还要留给共用这条连接的终端与 SFTP）；可能较慢的命令（写操作、读日志、Docker 与服务列表）最多占
//!   [`MAX_LONG_SIDE_CHANNELS`] 个，给资源监控等短命令留出名额。排队的时间算在命令的时限内。超时的命令先结束、
//!   通道关闭后才归还名额（见 [`crate::ssh::exec`]）。
//! - 服务器拒绝另开通道：拒绝的原因是 administratively prohibited，或者通道打开了却拒绝执行命令，
//!   记在连接上，之后这条连接上直接返回 [`ExecError::SingleChannel`]，不再尝试（很多堡垒机只允许一个通道）；
//!   资源不足（resource shortage）当作会话数满，稍后再试。其他原因（OpenSSH 超过 MaxSessions 时回复的
//!   connect failed）：这条连接上另开通道成功过时当作会话数满；从没成功过时与只允许一个会话的服务器分不出，
//!   同样返回 [`ExecError::SingleChannel`]，[`SOFT_REFUSAL`] 后允许再试。连接重建后重新允许尝试。

use std::future::Future;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use russh::ChannelOpenFailure;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use crate::ssh::{Connection, ExecFailure, ExecResult, SshError, exec_until};
use crate::wsl::{self, WslError};

/// 每条连接同时打开的旁路通道数上限。
pub const MAX_SIDE_CHANNELS: usize = 2;
/// 每条连接同时占用旁路通道名额的长时间命令数上限。
pub const MAX_LONG_SIDE_CHANNELS: usize = 1;
/// 从没另开成功过的连接被拒后，多久之后再试（见模块说明）。
const SOFT_REFUSAL: Duration = Duration::from_secs(60);

/// 在哪里执行命令：终端所在的连接。终端重连后换成新的连接（每次调用时取终端当前的连接）。
#[derive(Clone)]
pub enum Target {
    /// SSH 终端的连接（`OpenedTerminal::connection`）；`single_channel` 是主机的"只用一个通道"（堡垒机），
    /// 这时不另开通道，返回 [`ExecError::SingleChannel`]
    Ssh {
        connection: Arc<Connection>,
        single_channel: bool,
    },
    /// WSL 终端：在同一发行版中以同一用户执行（`user` 为空时是发行版的默认用户）
    Wsl { distro: String, user: String },
}

impl Target {
    /// SSH 终端的连接。
    pub fn ssh(connection: Arc<Connection>, single_channel: bool) -> Self {
        Target::Ssh {
            connection,
            single_channel,
        }
    }

    /// WSL 终端。
    pub fn wsl(distro: &str, user: &str) -> Self {
        Target::Wsl {
            distro: distro.to_string(),
            user: user.to_string(),
        }
    }
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Ssh {
                connection,
                single_channel,
            } => f
                .debug_struct("Ssh")
                .field("host", &connection.label())
                .field("single_channel", single_channel)
                .finish(),
            Target::Wsl { distro, user } => f
                .debug_struct("Wsl")
                .field("distro", distro)
                .field("user", user)
                .finish(),
        }
    }
}

/// 执行命令本身失败（与命令的输出无关）。消息写给日志看，界面按变体选用自己的文案。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ExecError {
    /// 主机设置了"只用一个通道"（堡垒机），或服务器拒绝在这条连接上另开通道：不能另开通道执行命令
    #[error("这台主机只用一个通道，不能另开通道执行命令")]
    SingleChannel,
    /// 连接已断开（终端正在重连）
    #[error("连接已断开")]
    Disconnected,
    /// 执行超时：服务器没有及时响应；写操作的命令可能已经发出，结果未知
    #[error("执行超时，服务器没有及时响应")]
    Timeout,
    /// 打开通道或发出命令失败（服务器的会话数已满、拒绝另开通道等），带上原因
    #[error("打开通道失败：{0}")]
    Channel(String),
    /// WSL 中执行失败（发行版不存在、wsl.exe 无法启动等）
    #[error("{0}")]
    Wsl(WslError),
}

/// 一次执行的参数。
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    /// 最长时间（含排队等名额的时间）
    pub timeout: Duration,
    /// 标准输出、标准错误各最多保留的字节数
    pub output: usize,
    /// 可能较慢的命令：占长命令的名额
    pub long: bool,
}

/// 在 `target` 上执行 `command`（已经是 [`sh_script`] 拼好的命令行）。
pub(crate) async fn run(
    target: &Target,
    command: &str,
    limits: Limits,
) -> Result<ExecResult, ExecError> {
    match target {
        Target::Ssh {
            single_channel: true,
            ..
        } => Err(ExecError::SingleChannel),
        Target::Ssh { connection, .. } => run_ssh(connection, command, limits).await,
        Target::Wsl { distro, user } => {
            wsl::exec(distro, user, command, limits.output, limits.timeout)
                .await
                .map_err(|error| match error {
                    WslError::Timeout(_) => ExecError::Timeout,
                    other => ExecError::Wsl(other),
                })
        }
    }
}

async fn run_ssh(
    connection: &Arc<Connection>,
    command: &str,
    limits: Limits,
) -> Result<ExecResult, ExecError> {
    if connection.is_closed() {
        return Err(ExecError::Disconnected);
    }
    let slots = slots_for(connection);
    if slots.refused() {
        return Err(ExecError::SingleChannel);
    }
    let deadline = Instant::now() + limits.timeout;
    let Ok(_permits) = tokio::time::timeout_at(deadline, slots.acquire(limits.long)).await else {
        return Err(ExecError::Timeout);
    };
    // 排队期间别的命令可能已被拒绝
    if slots.refused() {
        return Err(ExecError::SingleChannel);
    }
    // 名额拿到返回为止：超时的命令结束、通道关闭后才归还
    match exec_until(connection, command, None, limits.output, deadline).await {
        Ok(result) => {
            slots.opened();
            Ok(result)
        }
        Err(ExecFailure::Timeout) => Err(ExecError::Timeout),
        Err(_) if connection.is_closed() => Err(ExecError::Disconnected),
        Err(ExecFailure::OpenRefused(reason)) => Err(slots.on_refused(reason)),
        Err(ExecFailure::ExecRefused) => {
            slots.refuse(None);
            Err(ExecError::SingleChannel)
        }
        Err(ExecFailure::Other(SshError::Disconnected)) => Err(ExecError::Disconnected),
        Err(ExecFailure::Other(SshError::Channel(reason))) => Err(ExecError::Channel(reason)),
        Err(ExecFailure::Other(other)) => Err(ExecError::Channel(other.to_string())),
    }
}

/// 一条连接的名额，以及服务器是否拒绝过另开通道。
#[derive(Clone)]
struct Slots {
    all: Arc<Semaphore>,
    long: Arc<Semaphore>,
    refusal: Arc<Mutex<Refusal>>,
}

#[derive(Default)]
struct Refusal {
    /// 服务器拒绝过另开通道：`Some(None)` 是不再尝试，`Some(Some(t))` 是到 `t` 之前不再尝试
    until: Option<Option<Instant>>,
    /// 这条连接上另开通道成功过
    opened: bool,
}

impl Slots {
    fn new() -> Self {
        Slots {
            all: Arc::new(Semaphore::new(MAX_SIDE_CHANNELS)),
            long: Arc::new(Semaphore::new(MAX_LONG_SIDE_CHANNELS)),
            refusal: Arc::default(),
        }
    }

    fn refusal(&self) -> std::sync::MutexGuard<'_, Refusal> {
        self.refusal.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 现在不能另开通道（服务器拒绝过，且没有到再试的时间）。
    fn refused(&self) -> bool {
        let mut refusal = self.refusal();
        match refusal.until {
            Some(Some(until)) if Instant::now() >= until => {
                refusal.until = None;
                false
            }
            Some(_) => true,
            None => false,
        }
    }

    fn opened(&self) {
        self.refusal().opened = true;
    }

    /// 记下服务器拒绝另开通道（`retry_at` 为 `None` 时这条连接上不再尝试）。
    fn refuse(&self, retry_at: Option<Instant>) {
        let mut refusal = self.refusal();
        if refusal.until != Some(None) {
            refusal.until = Some(retry_at);
        }
    }

    /// 打开会话通道被拒：按模块说明区分会话数满与不允许另开通道。
    fn on_refused(&self, reason: ChannelOpenFailure) -> ExecError {
        let text = russh::Error::ChannelOpenFailure(reason.clone()).to_string();
        match reason {
            ChannelOpenFailure::AdministrativelyProhibited => {
                self.refuse(None);
                ExecError::SingleChannel
            }
            ChannelOpenFailure::ResourceShortage => ExecError::Channel(text),
            _ if self.refusal().opened => ExecError::Channel(text),
            _ => {
                self.refuse(Some(Instant::now() + SOFT_REFUSAL));
                ExecError::SingleChannel
            }
        }
    }

    /// 取得名额（长命令先取长命令的名额，再取总名额），返回时持有，丢弃即归还。
    async fn acquire(&self, long: bool) -> (Option<OwnedSemaphorePermit>, OwnedSemaphorePermit) {
        let long = match long {
            true => Some(
                self.long
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("信号量不会关闭"),
            ),
            false => None,
        };
        let all = self
            .all
            .clone()
            .acquire_owned()
            .await
            .expect("信号量不会关闭");
        (long, all)
    }
}

/// 各连接的名额：只记弱引用，连接不再使用后在下一次登记时清掉。
static SLOTS: Mutex<Vec<(Weak<Connection>, Slots)>> = Mutex::new(Vec::new());

fn slots_for(connection: &Arc<Connection>) -> Slots {
    let mut book = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
    book.retain(|(weak, _)| weak.strong_count() > 0);
    let weak = Arc::downgrade(connection);
    if let Some((_, slots)) = book.iter().find(|(w, _)| w.ptr_eq(&weak)) {
        return slots.clone();
    }
    let slots = Slots::new();
    book.push((weak, slots.clone()));
    slots
}

/// 在核心的运行时上执行 `future` 并等待结果：返回的 future 是 `Send`，界面可以在自己的执行器里直接 `await`。
/// 调用方不再等待时任务照常执行完（写操作不会因界面关闭而中途放弃），受命令自己的时限约束。
pub(crate) async fn on_runtime<T, E>(
    future: impl Future<Output = Result<T, E>> + Send + 'static,
) -> Result<T, E>
where
    T: Send + 'static,
    E: From<ExecError> + Send + 'static,
{
    match crate::runtime::spawn(future).await {
        Ok(result) => result,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        // 运行时正在关闭（退出时）
        Err(_) => Err(ExecError::Disconnected.into()),
    }
}

/// 把 `text` 写成 shell 的单引号字符串，内容原样传给命令。单引号与反斜杠放在引号外、用反斜杠转义：
/// fish 在单引号内也把反斜杠当转义，放在引号外时 bash、zsh、dash、fish 的解释一致。
/// 登录 shell 是 tcsh、csh 时单引号内不能有换行，脚本要写成一行（见 `one_line`）。
pub fn sh_quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('\'');
    for c in text.chars() {
        match c {
            '\'' => out.push_str(r"'\''"),
            '\\' => out.push_str(r"'\\'"),
            other => out.push(other),
        }
    }
    out.push('\'');
    out
}

/// 经 `sh -c` 执行 `script` 的命令行；`args` 依次成为脚本中的 `$1`、`$2`……（各自加引号，不经 shell 展开）。
/// 脚本中用到的远端数据（服务名、容器 ID 等）应经 `args` 传入，不要拼进脚本。
pub fn sh_script<S: AsRef<str>>(script: &str, args: &[S]) -> String {
    let mut out = String::from("sh -c ");
    out.push_str(&sh_quote(script));
    out.push_str(" sh");
    for arg in args {
        out.push(' ');
        out.push_str(&sh_quote(arg.as_ref()));
    }
    out
}

/// 把多行脚本合成一行（换行与下一行的缩进换成一个空格）：tcsh 的单引号内不能有换行。
/// 因此脚本的每行都以分号、`do`、`then`、`else`、`in`、`|`、`&&`、`||` 等结尾，不写注释。
pub(crate) fn one_line(script: &str) -> String {
    let mut out = String::with_capacity(script.len());
    let mut lines = script.split('\n');
    if let Some(first) = lines.next() {
        out.push_str(first);
    }
    for line in lines {
        out.push(' ');
        out.push_str(line.trim_start_matches([' ', '\t']));
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 引号与反斜杠的写法；用本机的 sh 执行时参数原样到达脚本，不经展开。
    #[test]
    fn sh_script_quotes_arguments() {
        let got = sh_script(
            r#"printf '%s|' "$1" "$2" "$3""#,
            &["it's", "$(id)", r"a\b\'c"],
        );
        let want =
            r#"sh -c 'printf '\''%s|'\'' "$1" "$2" "$3"' sh 'it'\''s' '$(id)' 'a'\\'b'\\''\''c'"#;
        assert_eq!(got, want);
        let Some(out) = super::super::tests::run_sh_stdin(&got) else {
            return;
        };
        assert_eq!(out, r"it's|$(id)|a\b\'c|");
    }

    /// 服务器拒绝另开通道：按原因记下或不记。
    #[test]
    fn refusals_are_remembered_by_reason() {
        let slots = Slots::new();
        assert_eq!(
            slots.on_refused(ChannelOpenFailure::AdministrativelyProhibited),
            ExecError::SingleChannel
        );
        assert!(slots.refused());
        // 明确拒绝后不会被"稍后再试"覆盖
        slots.refuse(Some(Instant::now()));
        assert!(slots.refused());

        // 资源不足：会话数满，不记
        let slots = Slots::new();
        assert!(matches!(
            slots.on_refused(ChannelOpenFailure::ResourceShortage),
            ExecError::Channel(_)
        ));
        assert!(!slots.refused());

        // connect failed：另开成功过时是会话数满
        let slots = Slots::new();
        slots.opened();
        assert!(matches!(
            slots.on_refused(ChannelOpenFailure::ConnectFailed),
            ExecError::Channel(_)
        ));
        assert!(!slots.refused());

        // 从没成功过：暂时不再尝试，到时间后允许再试
        let slots = Slots::new();
        assert_eq!(
            slots.on_refused(ChannelOpenFailure::ConnectFailed),
            ExecError::SingleChannel
        );
        assert!(slots.refused());
        slots.refusal().until = Some(Some(Instant::now()));
        assert!(!slots.refused());
        assert!(!slots.refused());
    }

    #[test]
    fn one_line_joins_indented_lines() {
        assert_eq!(one_line("a;\n  b;\n\tc\n"), "a; b; c");
        assert_eq!(one_line("  x  "), "x");
    }
}
