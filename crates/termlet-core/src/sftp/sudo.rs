//! 以 root 身份打开 SFTP：在 exec 通道上用 sudo 启动 sftp-server，在这个通道上跑 SFTP 协议，代替 `sftp` 子系统
//! （WSL 的"以 root 身份打开"是以 root 直接运行发行版中的 sftp-server，见 [`crate::wsl::open_sftp`]；这里同样在
//! 字节流上建立 SFTP 客户端）。
//!
//! 步骤：
//! 1. 一次性命令（脚本经标准输入交给 `sh`，不受登录 shell 种类影响）查看登录用户是不是 root、有没有 sudo、
//!    sftp-server 在哪里（先读 sshd_config 的 Subsystem 行，读不到或是内置的 internal-sftp 时试常见位置）。
//!    已经是 root 时直接打开 SFTP 子系统。
//! 2. `sudo -n <sftp-server>`：免密时直接开始 SFTP 协议，除了 SFTP 的数据什么也不写。sudoers 中只允许
//!    sftp-server 免密（WinSCP 等推荐的配置）时也能用。
//! 3. 需要密码时询问（[`SudoAsker`]），在新的通道上执行
//!    `sudo -S -p <提示标记> sh -c 'echo <就绪标记>; exec <sftp-server>'`：看到 sudo 的密码提示后才写入密码与换行；
//!    root 的 shell 输出就绪标记后才开始 SFTP 协议，密码只会被 sudo 读走，不会进 SFTP 的数据流。再次出现密码提示
//!    就是密码不对：关闭通道，不让 sudo 把之后的 SFTP 数据当作下一次的密码。sudoers 只允许以 root 执行
//!    sftp-server 本身（不允许 sh）时，改为直接 `sudo -S -p <提示标记> <sftp-server>`，写入密码后开始 SFTP 协议
//!    （这时密码已经验证过）。
//!
//! 提示用自己的标记而不是空串：sudo 再次要密码时（密码不对）能认出来，不依赖 sudo 的说明文字。命令前加
//! `env LC_ALL=C`：sudo 的说明是英文的（据此区分没有权限、要求终端等），PAM 的提示也是英文的 `Password:`，
//! sudo 因此换用 `-p` 的提示（PAM 用了别的提示时，标准错误停在以冒号结尾的一行上一会儿也当作密码提示）。
//!
//! 密码不进日志、不进 `Debug` 输出（[`SecretString`]，写入的内容用完清零）。

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use futures::future::BoxFuture;
use russh::client::Msg;
use russh::{ChannelMsg, ChannelReadHalf, ChannelWriteHalf};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use zeroize::Zeroizing;

use super::SftpError;
use super::client::SftpClient;
use super::manager::open_subsystem;
use crate::model::Encoding;
use crate::secret::SecretString;
use crate::ssh::{Connection, ExecFailure, TerminalSpec, exec_until};

/// 查看服务器情况的最长时间。
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
/// 打开通道的最长时间。
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);
/// 开始执行到可以开始 SFTP 的最长时间（PAM 在密码不对时会停几秒）。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// 标准错误停在以冒号结尾的一行上这么久没有新的输出时，当作密码提示（PAM 用了自己的提示时）。
const PROMPT_IDLE: Duration = Duration::from_millis(800);
/// SFTP 没能开始时，等命令结束（拿到退出码与完整的说明）的最长时间。
const EXIT_WAIT: Duration = Duration::from_secs(2);
/// 询问密码的次数上限（与 sudo 默认的 passwd_tries 相同）。
const MAX_ATTEMPTS: usize = 3;
/// 记下的标准错误的上限（只留最后这么多）。
const STDERR_LIMIT: usize = 4096;
/// sudo 的密码提示（`-p`）：认出 sudo 在要密码、密码不对时又要一次。不含 `%`（sudo 的转义）与引号。
const PROMPT: &str = "[termlet-sudo-password]";
/// root 的 shell 在 sudo 认证通过后输出这一行，之后才是 SFTP 协议。
const READY: &str = "termlet-sudo-ready";

/// 查看服务器情况的脚本：登录用户的 uid、有没有 sudo、sftp-server 的路径。
const PROBE_SCRIPT: &str = r#"echo "uid $(id -u 2>/dev/null)"
if command -v sudo >/dev/null 2>&1; then echo "sudo yes"; else echo "sudo no"; fi
server=
for f in /etc/ssh/sshd_config /etc/ssh/sshd_config.d/*.conf; do
  [ -r "$f" ] || continue
  p=$(awk 'tolower($1) == "subsystem" && $2 == "sftp" { print $3; exit }' "$f" 2>/dev/null)
  case "$p" in
    /*) if [ -x "$p" ]; then server=$p; break; fi ;;
  esac
done
if [ -z "$server" ]; then
  for p in /usr/lib/openssh/sftp-server /usr/libexec/openssh/sftp-server /usr/lib/ssh/sftp-server /usr/libexec/ssh/sftp-server /usr/libexec/sftp-server /usr/lib/sftp-server /usr/lib/misc/sftp-server /usr/local/libexec/sftp-server /usr/sbin/sftp-server; do
    if [ -x "$p" ]; then server=$p; break; fi
  done
fi
if [ -z "$server" ]; then
  p=$(command -v sftp-server 2>/dev/null) && [ -x "$p" ] && server=$p
fi
echo "server $server"
"#;

/// sudo 要密码时的询问。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SudoPrompt {
    /// 显示用的主机名称
    pub host: String,
    /// sudo 问的是这个用户（登录用户）的密码
    pub username: String,
    /// 上一次给的密码不对
    pub retry: bool,
}

/// 回答 sudo 的密码询问：返回密码，`None` 为取消。
pub type SudoAsker =
    Arc<dyn Fn(SudoPrompt) -> BoxFuture<'static, Option<SecretString>> + Send + Sync>;

/// 以 root 身份打开 SFTP 失败的原因。消息写给日志看，界面按变体选用自己的文案。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SudoError {
    #[error("服务器上没有 sudo")]
    NotInstalled,
    /// 只有内置的 internal-sftp，或装在不常见的位置
    #[error("服务器上找不到 sftp-server")]
    ServerMissing,
    /// 需要密码而没能询问（自动重连时）
    #[error("sudo 需要密码")]
    PasswordRequired,
    #[error("sudo 的密码不对")]
    WrongPassword,
    /// 不在 sudoers 中，或不允许以 root 执行 sftp-server
    #[error("这个用户没有 sudo 权限")]
    NotAllowed,
    /// sudoers 设置了 requiretty：不在终端中不能用 sudo
    #[error("服务器的 sudo 要求在终端中运行（requiretty）")]
    RequiresTty,
    /// 服务器拒绝执行命令（只允许 SFTP、ForceCommand、受限的 shell 等）
    #[error("服务器不允许执行命令")]
    ExecRefused,
    /// "只用一个通道"的主机（堡垒机）
    #[error("这台主机只用一个通道，不能以 root 身份打开 SFTP")]
    SingleChannel,
    /// 其余失败：sudo 的说明（最后一行）或退出码
    #[error("sudo 失败：{0}")]
    Failed(String),
}

impl From<SudoError> for SftpError {
    fn from(error: SudoError) -> Self {
        SftpError::Sudo(error)
    }
}

/// 一个会话的 sudo 认证：询问密码，记住用过的密码（自动重连时不再询问）。
pub(crate) struct SudoAuth {
    asker: SudoAsker,
    /// 为假时不再询问（打开之后、自动重连时）
    interactive: Arc<AtomicBool>,
    host: String,
    username: String,
    remembered: Mutex<Option<SecretString>>,
}

impl SudoAuth {
    pub(crate) fn new(asker: SudoAsker, spec: &TerminalSpec, interactive: Arc<AtomicBool>) -> Self {
        let target = spec.plan.target();
        Self {
            asker,
            interactive,
            host: target.label.clone(),
            username: target.username.clone(),
            remembered: Mutex::new(None),
        }
    }

    /// 要用的密码：先用记住的（`retry` 时它刚被证明不对），否则询问。
    async fn password(&self, retry: bool) -> Result<SecretString, SftpError> {
        if !retry && let Some(password) = self.remembered.lock().unwrap().clone() {
            return Ok(password);
        }
        if !self.interactive.load(Ordering::SeqCst) {
            return Err(if retry {
                SudoError::WrongPassword
            } else {
                SudoError::PasswordRequired
            }
            .into());
        }
        let prompt = SudoPrompt {
            host: self.host.clone(),
            username: self.username.clone(),
            retry,
        };
        tracing::info!(host = %self.host, user = %self.username, retry, "询问 sudo 的密码");
        (self.asker)(prompt).await.ok_or(SftpError::Canceled)
    }

    fn remember(&self, password: SecretString) {
        *self.remembered.lock().unwrap() = Some(password);
    }

    fn forget(&self) {
        self.remembered.lock().unwrap().take();
    }
}

/// 以 root 身份打开 SFTP（见模块说明）；服务器上的文件名是 `encoding` 编码的。
pub(crate) async fn open(
    connection: &Connection,
    auth: &SudoAuth,
    encoding: Encoding,
) -> Result<SftpClient, SftpError> {
    let host = connection.label();
    let probe = probe(connection).await?;
    tracing::debug!(
        host,
        root = probe.root,
        sudo = probe.sudo,
        server = ?probe.server,
        "以 root 身份打开 SFTP：服务器的情况"
    );
    if probe.root {
        tracing::info!(host, "登录用户已经是 root，直接打开 SFTP 子系统");
        return open_subsystem(connection, encoding).await;
    }
    let server = probe.server.ok_or(SudoError::ServerMissing)?;
    if !probe.sudo {
        return Err(SudoError::NotInstalled.into());
    }
    match open_direct(connection, &direct_command(&server), encoding).await {
        Ok(client) => {
            tracing::info!(host, server = %server, "已经免密 sudo 以 root 身份打开 SFTP");
            return Ok(client);
        }
        Err(SftpError::Sudo(SudoError::PasswordRequired)) => {}
        Err(error) => return Err(error),
    }
    let mut retry = false;
    for _ in 0..MAX_ATTEMPTS {
        let password = auth.password(retry).await?;
        match open_with_password(connection, &server, &password, true, encoding).await {
            Ok(client) => {
                tracing::info!(host, server = %server, "已经用密码 sudo 以 root 身份打开 SFTP");
                auth.remember(password);
                return Ok(client);
            }
            Err(SftpError::Sudo(SudoError::WrongPassword)) => {
                tracing::info!(host, "sudo 的密码不对");
                auth.forget();
                retry = true;
            }
            // 认证通过了但不允许执行 sh：sudoers 可能只允许 sftp-server 本身，直接执行它
            Err(SftpError::Sudo(SudoError::NotAllowed)) => {
                tracing::info!(host, "sudo 不允许执行 sh，改为直接执行 sftp-server");
                let client =
                    open_with_password(connection, &server, &password, false, encoding).await?;
                tracing::info!(host, server = %server, "已经用密码 sudo 以 root 身份打开 SFTP");
                auth.remember(password);
                return Ok(client);
            }
            Err(error) => return Err(error),
        }
    }
    Err(SudoError::WrongPassword.into())
}

// ------------------------------------------------------------ 服务器的情况

/// [`PROBE_SCRIPT`] 的结果。
#[derive(Debug, PartialEq, Eq)]
struct Probe {
    /// 登录用户已经是 root
    root: bool,
    sudo: bool,
    /// sftp-server 的路径（只接受普通的绝对路径）
    server: Option<String>,
}

async fn probe(connection: &Connection) -> Result<Probe, SftpError> {
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let result = exec_until(
        connection,
        "sh -s",
        Some(PROBE_SCRIPT.as_bytes()),
        4096,
        deadline,
    )
    .await
    .map_err(|failure| match failure {
        ExecFailure::OpenRefused(reason) => SftpError::Unavailable(format!(
            "打开通道失败：{}",
            russh::Error::ChannelOpenFailure(reason)
        )),
        ExecFailure::ExecRefused => SudoError::ExecRefused.into(),
        ExecFailure::Timeout => SudoError::Failed("服务器没有回应".into()).into(),
        ExecFailure::Other(error) => SftpError::Connect(error),
    })?;
    let output = String::from_utf8_lossy(&result.stdout);
    parse_probe(&output).ok_or_else(|| {
        tracing::info!(
            host = connection.label(),
            exit_code = ?result.exit_code,
            stderr = %String::from_utf8_lossy(&result.stderr).trim(),
            "服务器上没能执行查看情况的脚本"
        );
        SudoError::ExecRefused.into()
    })
}

/// 解析 [`PROBE_SCRIPT`] 的输出。前面可能有登录 shell 的启动脚本的输出：只认这几个开头，取最后出现的。
fn parse_probe(output: &str) -> Option<Probe> {
    let (mut uid, mut sudo, mut server) = (None, false, None);
    for line in output.lines() {
        let line = line.trim();
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        let value = value.trim();
        match key {
            "uid" => uid = Some(value.to_string()),
            "sudo" => sudo = value == "yes",
            "server" => server = Some(value.to_string()),
            _ => {}
        }
    }
    Some(Probe {
        root: uid? == "0",
        sudo,
        server: server.filter(|path| plain_path(path)),
    })
}

/// 普通的绝对路径：路径放进命令行，只接受这些字符。
fn plain_path(path: &str) -> bool {
    path.starts_with('/')
        && path.len() <= 255
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b'+'))
}

/// 免密执行 sftp-server。
fn direct_command(server: &str) -> String {
    format!("env LC_ALL=C sudo -n {server}")
}

/// 从标准输入读密码执行：`marker` 时经 root 的 shell 输出就绪标记，否则直接执行 sftp-server。
fn password_command(server: &str, marker: bool) -> String {
    if marker {
        format!("env LC_ALL=C sudo -S -p '{PROMPT}' sh -c 'echo {READY}; exec {server}'")
    } else {
        format!("env LC_ALL=C sudo -S -p '{PROMPT}' {server}")
    }
}

// ------------------------------------------------------------ 与 sudo 的握手

/// 需要密码时与 sudo 的握手：决定什么时候写入密码、什么时候可以开始 SFTP、什么时候放弃。不做读写，便于测试。
pub(crate) struct Handshake {
    password: SecretString,
    /// 等 root 的 shell 输出就绪标记；为假时直接执行 sftp-server，写入密码后就开始 SFTP
    marker: bool,
    /// 写入密码时标准错误的长度
    sent_at: Option<usize>,
    /// 上一次认出提示时标准错误的长度（同一个提示不算两次）
    prompt_at: Option<usize>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit: Option<u32>,
}

/// 握手的下一步。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Wait,
    /// 写入密码与换行（[`Handshake::input`]）；`start` 时写完就开始 SFTP
    Send {
        start: bool,
    },
    /// 可以开始 SFTP：就绪标记之后已经收到的输出（属于 SFTP）
    Ready(Vec<u8>),
    Fail(SudoError),
}

impl Handshake {
    pub(crate) fn new(password: SecretString, marker: bool) -> Self {
        Self {
            password,
            marker,
            sent_at: None,
            prompt_at: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit: None,
        }
    }

    /// 要写入的内容：密码加换行。
    pub(crate) fn input(&self) -> Zeroizing<Vec<u8>> {
        let secret = self.password.expose().as_bytes();
        let mut input = Zeroizing::new(Vec::with_capacity(secret.len() + 1));
        input.extend_from_slice(secret);
        input.push(b'\n');
        input
    }

    /// 标准输出。就绪标记之前的（登录 shell 的启动脚本的输出等）丢掉；直接执行 sftp-server 时在写入密码、
    /// 开始 SFTP 之前它不会输出，也丢掉。
    pub(crate) fn stdout(&mut self, data: &[u8]) -> Step {
        if !self.marker {
            return Step::Wait;
        }
        self.stdout.extend_from_slice(data);
        let line = format!("{READY}\n");
        if let Some(at) = find(&self.stdout, line.as_bytes()) {
            return Step::Ready(self.stdout.split_off(at + line.len()));
        }
        // 只留最后一点：标记可能被拆在两段输出中
        let keep = line.len() - 1;
        if self.stdout.len() > keep {
            self.stdout.drain(..self.stdout.len() - keep);
        }
        Step::Wait
    }

    /// 标准错误：密码提示、密码不对的说明。
    pub(crate) fn stderr(&mut self, data: &[u8]) -> Step {
        self.stderr.extend_from_slice(data);
        if self.stderr.len() > STDERR_LIMIT {
            let cut = self.stderr.len() - STDERR_LIMIT;
            self.stderr.drain(..cut);
            self.sent_at = self.sent_at.map(|at| at.saturating_sub(cut));
            self.prompt_at = self.prompt_at.map(|at| at.saturating_sub(cut));
        }
        if let Some(at) = self.sent_at
            && says_wrong_password(&String::from_utf8_lossy(&self.stderr[at..]).to_lowercase())
        {
            return Step::Fail(SudoError::WrongPassword);
        }
        if self.last_line().ends_with(PROMPT) {
            return self.prompted();
        }
        Step::Wait
    }

    /// 一会儿没有新的输出：标准错误停在以冒号结尾、还没换行的一行上时当作密码提示（PAM 用了自己的提示）。
    pub(crate) fn idle(&mut self) -> Step {
        let line = self.last_line();
        if line.ends_with(':') || line.ends_with('：') {
            return self.prompted();
        }
        Step::Wait
    }

    /// 标准错误中还没换行的最后一行（去掉末尾的空白）。
    fn last_line(&self) -> String {
        let line = match self.stderr.iter().rposition(|&b| b == b'\n') {
            Some(at) => &self.stderr[at + 1..],
            None => &self.stderr[..],
        };
        String::from_utf8_lossy(line).trim_end().to_string()
    }

    /// 命令的退出码（之后通道关闭）。
    pub(crate) fn exited(&mut self, code: u32) {
        self.exit = Some(code);
    }

    /// 通道关闭了，还没能开始 SFTP：按 sudo 的说明给出原因。
    pub(crate) fn ended(&self) -> SudoError {
        classify(&self.stderr, self.exit)
    }

    fn prompted(&mut self) -> Step {
        let at = self.stderr.len();
        if self.prompt_at == Some(at) {
            return Step::Wait;
        }
        self.prompt_at = Some(at);
        if self.sent_at.is_some() {
            return Step::Fail(SudoError::WrongPassword);
        }
        self.sent_at = Some(at);
        Step::Send {
            start: !self.marker,
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 小写后的 sudo 说明是不是密码不对（sudo：Sorry, try again.／N incorrect password attempts；
/// sudo-rs：Authentication failed, try again.）。
fn says_wrong_password(lower: &str) -> bool {
    [
        "incorrect password",
        "try again",
        "authentication failed",
        "authentication failure",
    ]
    .iter()
    .any(|w| lower.contains(w))
}

/// 按 sudo 的说明（`LC_ALL=C`，英文）与退出码判断失败的原因。
pub(crate) fn classify(stderr: &[u8], exit: Option<u32>) -> SudoError {
    let text = String::from_utf8_lossy(stderr).replace(PROMPT, "\n");
    let lower = text.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| lower.contains(w));
    if has(&["must have a tty", "requiretty"]) {
        SudoError::RequiresTty
    } else if has(&[
        "password is required",
        "terminal is required",
        "no tty present",
    ]) {
        SudoError::PasswordRequired
    } else if says_wrong_password(&lower) {
        SudoError::WrongPassword
    } else if has(&[
        "not in the sudoers",
        "may not run sudo",
        "not allowed to",
        "is not permitted",
        "can't do that",
    ]) {
        SudoError::NotAllowed
    } else if exit == Some(127) || has(&["sudo: not found", "command not found"]) {
        SudoError::NotInstalled
    } else {
        let line = text
            .lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty())
            .map(|l| l.chars().take(200).collect::<String>());
        SudoError::Failed(match (line, exit) {
            (Some(line), _) => line,
            (None, Some(code)) => format!("退出码 {code}"),
            (None, None) => "没有输出".into(),
        })
    }
}

// ------------------------------------------------------------ exec 通道

/// exec 通道上收到的消息。
enum Event {
    Out(Vec<u8>),
    Err(Vec<u8>),
    Exit(u32),
    /// 服务器拒绝执行命令
    Refused,
}

/// 命令的标准错误与退出码：SFTP 没能开始时据此说明原因。由收消息的任务记下，与谁在读标准输出无关。
#[derive(Default)]
struct Diag {
    stderr: Vec<u8>,
    exit: Option<u32>,
    refused: bool,
    /// 通道已经关闭
    ended: bool,
}

/// 执行中的命令。
struct Exec {
    write: ChannelWriteHalf<Msg>,
    events: mpsc::Receiver<Event>,
    diag: Arc<watch::Sender<Diag>>,
}

impl Exec {
    /// 在新的通道上执行命令（命令中没有秘密，但可能有路径：只记 debug）。
    async fn open(connection: &Connection, command: &str) -> Result<Self, SftpError> {
        let opened =
            tokio::time::timeout(OPEN_TIMEOUT, connection.handle().channel_open_session()).await;
        let channel = match opened {
            Err(_) => return Err(SftpError::Unavailable("打开通道超时".into())),
            Ok(Err(e)) => return Err(SftpError::Unavailable(format!("打开通道失败：{e}"))),
            Ok(Ok(channel)) => channel,
        };
        let (read, write) = channel.split();
        let diag = Arc::new(watch::channel(Diag::default()).0);
        let events = pump(read, diag.clone());
        tracing::debug!(host = connection.label(), command, "执行 sudo");
        if let Err(e) = write.exec(true, command).await {
            let _ = write.close().await;
            return Err(SftpError::Unavailable(format!("执行命令失败：{e}")));
        }
        Ok(Self {
            write,
            events,
            diag,
        })
    }

    async fn close(self) {
        let _ = self.write.close().await;
    }

    /// 握手完成：剩下的交给 SFTP（`rest` 是已经收到的属于 SFTP 的输出）。
    fn into_stream(self, rest: Vec<u8>) -> (ExecStream, Arc<watch::Sender<Diag>>) {
        let writer = Box::pin(self.write.make_writer());
        let stream = ExecStream {
            events: self.events,
            chunk: rest,
            pos: 0,
            writer,
            write: Some(self.write),
        };
        (stream, self.diag)
    }
}

/// 收通道的消息：转给读的一方，同时记下标准错误与退出码，直到通道关闭。
fn pump(mut read: ChannelReadHalf, diag: Arc<watch::Sender<Diag>>) -> mpsc::Receiver<Event> {
    let (tx, rx) = mpsc::channel(32);
    crate::runtime::spawn(async move {
        while let Some(message) = read.wait().await {
            let event = match message {
                ChannelMsg::Data { data } => Event::Out(data.to_vec()),
                ChannelMsg::ExtendedData { data, ext: 1 } => {
                    diag.send_modify(|d| {
                        d.stderr.extend_from_slice(&data);
                        if d.stderr.len() > STDERR_LIMIT {
                            let cut = d.stderr.len() - STDERR_LIMIT;
                            d.stderr.drain(..cut);
                        }
                    });
                    Event::Err(data.to_vec())
                }
                ChannelMsg::ExitStatus { exit_status } => {
                    diag.send_modify(|d| d.exit = Some(exit_status));
                    Event::Exit(exit_status)
                }
                ChannelMsg::Failure => {
                    diag.send_modify(|d| d.refused = true);
                    Event::Refused
                }
                ChannelMsg::Close => break,
                _ => continue,
            };
            // 读的一方不在了（握手失败、SFTP 已关闭）也继续记下，直到通道关闭
            let _ = tx.send(event).await;
        }
        diag.send_modify(|d| d.ended = true);
    });
    rx
}

/// 执行免密 sudo：直接开始 SFTP，没能开始时按 sudo 的说明给出原因。
async fn open_direct(
    connection: &Connection,
    command: &str,
    encoding: Encoding,
) -> Result<SftpClient, SftpError> {
    let exec = Exec::open(connection, command).await?;
    let (stream, diag) = exec.into_stream(Vec::new());
    match SftpClient::connect(stream, encoding).await {
        Ok(client) => Ok(client),
        Err(error) => Err(explain(&diag, error).await),
    }
}

/// 执行从标准输入读密码的 sudo（见 [`Handshake`]），然后开始 SFTP。
async fn open_with_password(
    connection: &Connection,
    server: &str,
    password: &SecretString,
    marker: bool,
    encoding: Encoding,
) -> Result<SftpClient, SftpError> {
    let mut exec = Exec::open(connection, &password_command(server, marker)).await?;
    let mut handshake = Handshake::new(password.clone(), marker);
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let rest = loop {
        let step = tokio::select! {
            event = exec.events.recv() => match event {
                Some(Event::Out(data)) => handshake.stdout(&data),
                Some(Event::Err(data)) => handshake.stderr(&data),
                Some(Event::Exit(code)) => {
                    handshake.exited(code);
                    Step::Wait
                }
                Some(Event::Refused) => Step::Fail(SudoError::ExecRefused),
                None => Step::Fail(handshake.ended()),
            },
            _ = tokio::time::sleep(PROMPT_IDLE) => handshake.idle(),
            _ = tokio::time::sleep_until(deadline) => {
                Step::Fail(SudoError::Failed("sudo 没有回应".into()))
            }
        };
        match step {
            Step::Wait => {}
            Step::Send { start } => {
                let input = handshake.input();
                if let Err(e) = exec.write.data(&input[..]).await {
                    exec.close().await;
                    return Err(SftpError::Unavailable(format!("写入密码失败：{e}")));
                }
                if start {
                    break Vec::new();
                }
            }
            Step::Ready(rest) => break rest,
            Step::Fail(error) => {
                tracing::debug!(host = connection.label(), "sudo 失败：{error}");
                exec.close().await;
                return Err(error.into());
            }
        }
    };
    let (stream, diag) = exec.into_stream(rest);
    match SftpClient::connect(stream, encoding).await {
        Ok(client) => Ok(client),
        Err(error) => Err(explain(&diag, error).await),
    }
}

/// SFTP 没能开始：等命令结束（最多 [`EXIT_WAIT`]），按 sudo 的说明给出原因；命令还在运行时用原来的错误。
async fn explain(diag: &watch::Sender<Diag>, error: SftpError) -> SftpError {
    let mut rx = diag.subscribe();
    let _ = tokio::time::timeout(EXIT_WAIT, rx.wait_for(|d| d.ended)).await;
    let diag = diag.borrow();
    if diag.refused {
        return SudoError::ExecRefused.into();
    }
    if diag.ended && (diag.exit.is_some_and(|code| code != 0) || !diag.stderr.is_empty()) {
        return classify(&diag.stderr, diag.exit).into();
    }
    error
}

/// 命令的标准输入输出：标准输出交给 SFTP（标准错误、退出码由 [`pump`] 记下）。丢弃时关闭通道。
struct ExecStream {
    events: mpsc::Receiver<Event>,
    /// 还没读完的一段标准输出
    chunk: Vec<u8>,
    pos: usize,
    writer: Pin<Box<dyn AsyncWrite + Send>>,
    write: Option<ChannelWriteHalf<Msg>>,
}

impl AsyncRead for ExecStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        loop {
            if this.pos < this.chunk.len() {
                let n = buf.remaining().min(this.chunk.len() - this.pos);
                buf.put_slice(&this.chunk[this.pos..this.pos + n]);
                this.pos += n;
                return Poll::Ready(Ok(()));
            }
            match ready!(this.events.poll_recv(cx)) {
                Some(Event::Out(data)) => {
                    this.chunk = data;
                    this.pos = 0;
                }
                Some(_) => {}
                None => return Poll::Ready(Ok(())),
            }
        }
    }
}

impl AsyncWrite for ExecStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.writer.as_mut().poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.writer.as_mut().poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.writer.as_mut().poll_shutdown(cx)
    }
}

impl Drop for ExecStream {
    fn drop(&mut self) {
        if let Some(write) = self.write.take() {
            crate::runtime::spawn(async move {
                let _ = write.eof().await;
                let _ = write.close().await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handshake(marker: bool) -> Handshake {
        Handshake::new(SecretString::new("pa ss\\word"), marker)
    }

    #[test]
    fn parses_the_probe_output() {
        let probe = parse_probe("uid 1000\nsudo yes\nserver /usr/lib/openssh/sftp-server\n");
        assert_eq!(
            probe,
            Some(Probe {
                root: false,
                sudo: true,
                server: Some("/usr/lib/openssh/sftp-server".into()),
            })
        );
        // 登录 shell 的启动脚本先输出了别的；root；没有 sudo、找不到 sftp-server
        let probe = parse_probe("Welcome!\nuid is fun\nuid 0\nsudo no\nserver \n");
        assert_eq!(
            probe,
            Some(Probe {
                root: true,
                sudo: false,
                server: None,
            })
        );
        // 不是普通的路径：不放进命令行
        let probe = parse_probe("uid 1000\nsudo yes\nserver /opt/my dir/sftp-server\n").unwrap();
        assert_eq!(probe.server, None);
        let probe = parse_probe("uid 1000\nsudo yes\nserver /x;reboot\n").unwrap();
        assert_eq!(probe.server, None);
        // 脚本没能执行
        assert_eq!(
            parse_probe("This service allows sftp connections only.\n"),
            None
        );
    }

    #[test]
    fn commands_read_the_password_only_when_asked() {
        let server = "/usr/lib/openssh/sftp-server";
        assert_eq!(
            direct_command(server),
            "env LC_ALL=C sudo -n /usr/lib/openssh/sftp-server"
        );
        assert_eq!(
            password_command(server, true),
            "env LC_ALL=C sudo -S -p '[termlet-sudo-password]' sh -c \
             'echo termlet-sudo-ready; exec /usr/lib/openssh/sftp-server'"
        );
        assert_eq!(
            password_command(server, false),
            "env LC_ALL=C sudo -S -p '[termlet-sudo-password]' /usr/lib/openssh/sftp-server"
        );
    }

    // 不需要密码（凭据还在等）：直接出现就绪标记，一个字节也不写
    #[test]
    fn no_password_written_when_sudo_does_not_ask() {
        let mut hs = handshake(true);
        assert_eq!(
            hs.stderr(b"sudo: unable to resolve host web: Name or service not known\n"),
            Step::Wait
        );
        assert_eq!(hs.stdout(b"motd from .bashrc\ntermlet-sudo-"), Step::Wait);
        // 标记被拆在两段输出中；之后紧跟的是 SFTP 的数据
        assert_eq!(
            hs.stdout(b"ready\n\x00\x00\x00\x05\x02"),
            Step::Ready(b"\x00\x00\x00\x05\x02".to_vec())
        );
        assert_eq!(hs.idle(), Step::Wait);
    }

    // 需要密码：看到提示才写入（密码加换行，只写一次），就绪标记之后才开始 SFTP
    #[test]
    fn password_written_once_after_the_prompt() {
        let mut hs = handshake(true);
        assert_eq!(hs.idle(), Step::Wait, "还没有提示时不写");
        // 第一次使用 sudo 时的告诫在提示之前
        assert_eq!(
            hs.stderr(b"\nWe trust you have received the usual lecture from the local System\nAdministrator.\n\n"),
            Step::Wait
        );
        assert_eq!(hs.stderr(b"[termlet-sudo-"), Step::Wait);
        assert_eq!(hs.stderr(b"password]"), Step::Send { start: false });
        assert_eq!(&hs.input()[..], b"pa ss\\word\n");
        // 同一个提示不再写
        assert_eq!(hs.idle(), Step::Wait);
        assert_eq!(hs.stdout(b"termlet-sudo-ready\n"), Step::Ready(Vec::new()));
    }

    #[test]
    fn wrong_password_is_recognized() {
        // sudo：再次提示
        let mut hs = handshake(true);
        assert_eq!(hs.stderr(PROMPT.as_bytes()), Step::Send { start: false });
        assert_eq!(
            hs.stderr(format!("\n{PROMPT}").as_bytes()),
            Step::Fail(SudoError::WrongPassword)
        );
        // sudo 的说明
        let mut hs = handshake(true);
        assert_eq!(hs.stderr(PROMPT.as_bytes()), Step::Send { start: false });
        assert_eq!(
            hs.stderr(b"\nSorry, try again.\n"),
            Step::Fail(SudoError::WrongPassword)
        );
        // sudo-rs 的说明
        let mut hs = handshake(true);
        assert_eq!(hs.stderr(PROMPT.as_bytes()), Step::Send { start: false });
        assert_eq!(
            hs.stderr(b"sudo: Authentication failed, try again.\n"),
            Step::Fail(SudoError::WrongPassword)
        );
        // 写入之前的说明不算（例如上一次的残留）
        let mut hs = handshake(true);
        assert_eq!(hs.stderr(b"Sorry, try again.\n"), Step::Wait);
    }

    // PAM 用了自己的提示（不是 Password:）：停在以冒号结尾的一行上一会儿后才写
    #[test]
    fn pam_prompt_recognized_after_idle() {
        let mut hs = handshake(true);
        assert_eq!(hs.stderr(b"Password for alice@EXAMPLE.COM: "), Step::Wait);
        assert_eq!(hs.idle(), Step::Send { start: false });
        assert_eq!(hs.idle(), Step::Wait);
        assert_eq!(hs.stderr(b"\nPassword for alice@EXAMPLE.COM: "), Step::Wait);
        assert_eq!(hs.idle(), Step::Fail(SudoError::WrongPassword));
        // 以冒号结尾、已经换行的说明不是提示
        let mut hs = handshake(true);
        assert_eq!(
            hs.stderr(b"sudo: unable to resolve host web:\n"),
            Step::Wait
        );
        assert_eq!(hs.idle(), Step::Wait);
    }

    // 直接执行 sftp-server：写入密码后就开始 SFTP；之前的输出不交给 SFTP
    #[test]
    fn direct_server_starts_after_the_password() {
        let mut hs = handshake(false);
        assert_eq!(hs.stdout(b"junk"), Step::Wait);
        assert_eq!(hs.stderr(PROMPT.as_bytes()), Step::Send { start: true });
    }

    #[test]
    fn exit_without_ready_is_explained() {
        let mut hs = handshake(true);
        assert_eq!(hs.stderr(PROMPT.as_bytes()), Step::Send { start: false });
        hs.stderr(b"\nbob is not in the sudoers file.\n");
        hs.exited(1);
        assert_eq!(hs.ended(), SudoError::NotAllowed);
    }

    #[test]
    fn classifies_sudo_messages() {
        for (stderr, exit, want) in [
            (
                "sudo: a password is required\n",
                Some(1),
                SudoError::PasswordRequired,
            ),
            (
                "sudo: a terminal is required to read the password; either use the -S option to read from standard input or configure an askpass helper\n",
                Some(1),
                SudoError::PasswordRequired,
            ),
            (
                "sudo: sorry, you must have a tty to run sudo\n",
                Some(1),
                SudoError::RequiresTty,
            ),
            (
                "Sorry, try again.\nsudo: 3 incorrect password attempts\n",
                Some(1),
                SudoError::WrongPassword,
            ),
            (
                "sudo: no password was provided\nsudo: 1 incorrect password attempt\n",
                Some(1),
                SudoError::WrongPassword,
            ),
            (
                "alice is not in the sudoers file.\n",
                Some(1),
                SudoError::NotAllowed,
            ),
            (
                "Sorry, user alice is not allowed to execute '/bin/sh -c echo' as root on web.\n",
                Some(1),
                SudoError::NotAllowed,
            ),
            (
                "Sorry, user alice may not run sudo on web.\n",
                Some(1),
                SudoError::NotAllowed,
            ),
            (
                "sudo: I'm sorry alice. I'm afraid I can't do that\n",
                Some(1),
                SudoError::NotAllowed,
            ),
            (
                "bash: sudo: command not found\n",
                Some(127),
                SudoError::NotInstalled,
            ),
            (
                "sudo: unable to resolve host web: Name or service not known\nsudo: account validation failure, is your account locked?\n",
                Some(1),
                SudoError::Failed(
                    "sudo: account validation failure, is your account locked?".into(),
                ),
            ),
            ("", Some(1), SudoError::Failed("退出码 1".into())),
            ("", None, SudoError::Failed("没有输出".into())),
        ] {
            assert_eq!(classify(stderr.as_bytes(), exit), want, "{stderr:?}");
        }
    }

    #[test]
    fn errors_do_not_show_the_password() {
        let hs = handshake(true);
        assert_eq!(
            format!("{:?}", Step::Send { start: false }),
            "Send { start: false }"
        );
        assert!(!format!("{:?}", hs.password).contains("word"));
    }
}
