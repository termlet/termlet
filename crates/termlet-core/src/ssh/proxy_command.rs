//! 代理命令（与 OpenSSH 的 ProxyCommand 相同）：在本机启动一条命令，用它的标准输入输出作为到第一跳的连接，
//! 例如 `cloudflared access ssh --hostname %h`、`aws ssm start-session --target %h …`、`ssh -W %h:%p jump`。
//!
//! # 占位符
//!
//! `%h` 主机地址、`%p` 端口、`%r` 用户名、`%n` 主机的名称（OpenSSH 中是命令行上写的别名；从 ssh_config 导入的主机，
//! 名称就是别名）、`%%` 百分号；其他的 `%x` 与 OpenSSH 一样不能用。代入的地址、用户名、名称含有空白、引号或 shell 的
//! 特殊字符（`` ` `` `$` `\` `;` `&` `<` `>` `|` 括号、花括号）时拒绝连接（规则与 OpenSSH 9.6 起检查命令行上的
//! 主机名、用户名相同，用户名中允许 `$`、中间的空格），代入的内容因此不会被 shell 或 cmd 解释成别的命令。
//!
//! # 怎么启动
//!
//! - Unix（Linux、macOS）：与 OpenSSH 相同，交给 shell：`$SHELL -c "exec <命令>"`（没有 `SHELL` 时用 `/bin/sh`），
//!   引号、转义、`~`、环境变量、管道都按 shell 的规则。
//! - Windows：与 Win32-OpenSSH 相同，不经过 cmd：命令行原样交给 CreateProcess。第一个词是程序（含空格的路径用双引号
//!   括起），没有路径时在 PATH 中查找、可以省略 `.exe`；其余部分原样作为参数行，由程序自己按它的规则拆分（多数程序按
//!   MSVC 的规则：双引号括起含空格的参数，`\"` 是引号本身；单引号没有特殊含义）。需要管道、`%VAR%` 等 cmd 的功能时
//!   写成 `cmd /c …`。不弹出控制台窗口。
//!
//! # 进程
//!
//! 标准错误留下最后几行：连接失败而命令已经退出时，与退出码一起显示（[`SshError::ProxyCommandExited`]）。
//! 连接结束时先关闭它的标准输入（多数代理随之退出），稍等仍在运行就结束它；Windows 上进程放在作业对象中，
//! 它启动的子进程（如 `cmd /c` 里的程序）一并结束。
//!
//! # 确认
//!
//! 代理命令会执行本机程序，而主机设置会随云同步到别的电脑、可以从文件导入：每台电脑第一次执行某条命令（替换占位符之前的
//! 原文）前请用户确认（[`Prompt::ProxyCommand`]，显示完整的命令），确认过的记在本机设置中
//! （[`crate::model::LocalConnectionSettings::allowed_proxy_commands`]，只存 SHA-256），不同步，恢复备份时也不覆盖。
//! 命令改了要重新确认；同一条命令用在多台主机上只确认一次（代入的内容见上，不会改变命令的含义）。
//! 命令行工具不能询问：没有在应用中确认过的返回 [`SshError::ProxyCommandUnconfirmed`]。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::sync::{oneshot, watch};

use crate::ssh::SshError;
use crate::ssh::plan::ProxyPlan;
use crate::ssh::prompt::{Answer, Prompt, Prompter, ProxyCommandPrompt};
use crate::ssh::transport::BoxStream;
use crate::store::Store;

/// 连接结束、关闭标准输入后，等代理命令自己退出的时间；之后结束它。
const CLOSE_GRACE: Duration = Duration::from_secs(1);
/// 连接失败时等代理命令退出（以便说明原因）的最长时间。
const EXPLAIN_WAIT: Duration = Duration::from_secs(2);
/// 进程退出后等标准错误读完的最长时间。
const STDERR_DRAIN: Duration = Duration::from_millis(500);
/// 标准错误最多留下这么多字节（最后的部分）。
const STDERR_KEEP: usize = 8 * 1024;
/// 显示标准错误的最后几行。
const STDERR_LINES: usize = 6;
/// 每行最多显示的字符数。
const STDERR_LINE_CHARS: usize = 300;
/// 本机最多记住多少条确认过的命令（超过时忘掉最早的）。
const MAX_ALLOWED: usize = 500;

/// 代入占位符的值。
#[derive(Clone, Copy, Debug)]
pub struct Tokens<'a> {
    /// `%h`
    pub host: &'a str,
    /// `%p`
    pub port: u16,
    /// `%r`
    pub user: &'a str,
    /// `%n`
    pub alias: &'a str,
}

/// 代理命令不能使用的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandIssue {
    /// 命令为空
    Empty,
    /// 不认识的占位符 `%x`（也包括命令以单独的 `%` 结尾，这时为 `'\0'`）
    UnknownToken(char),
    /// 占位符代入的内容含有空白、引号或 shell 的特殊字符
    UnsafeValue(char),
}

impl std::fmt::Display for CommandIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandIssue::Empty => f.write_str("代理命令为空"),
            CommandIssue::UnknownToken('\0') => {
                f.write_str("命令以单独的 % 结尾（百分号本身写作 %%）")
            }
            CommandIssue::UnknownToken(c) => write!(f, "不认识占位符 %{c}（百分号本身写作 %%）"),
            CommandIssue::UnsafeValue(c) => write!(
                f,
                "%{c} 代入的内容含有空白、引号或 shell 的特殊字符，不能用在代理命令中"
            ),
        }
    }
}

/// 替换占位符（OpenSSH 的 `percent_expand`）：只检查用到了的值。
pub fn expand(template: &str, tokens: &Tokens<'_>) -> Result<String, CommandIssue> {
    if template.trim().is_empty() {
        return Err(CommandIssue::Empty);
    }
    let mut out = String::with_capacity(template.len() + tokens.host.len());
    let mut chars = template.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some('h') => out.push_str(checked('h', tokens.host, valid_hostname)?),
            Some('n') => out.push_str(checked('n', tokens.alias, valid_hostname)?),
            Some('r') => out.push_str(checked('r', tokens.user, valid_user)?),
            Some('p') => out.push_str(&tokens.port.to_string()),
            Some(other) => return Err(CommandIssue::UnknownToken(other)),
            None => return Err(CommandIssue::UnknownToken('\0')),
        }
    }
    Ok(out)
}

fn checked(token: char, value: &str, valid: fn(&str) -> bool) -> Result<&str, CommandIssue> {
    if valid(value) {
        Ok(value)
    } else {
        Err(CommandIssue::UnsafeValue(token))
    }
}

/// 与 OpenSSH 的 `valid_hostname` 相同：不以 `-` 开头，没有空白、控制字符与 shell 的特殊字符。
fn valid_hostname(s: &str) -> bool {
    !s.starts_with('-')
        && !s
            .chars()
            .any(|c| "'`\"$\\;&<>|(){}".contains(c) || c.is_whitespace() || c.is_control())
}

/// 与 OpenSSH 的 `valid_ruser` 相同：不以 `-` 开头，没有控制字符与引号、`;&<>|(){}`，空白后不跟 `-`，
/// 不以 `\` 结尾（允许 `$` 与中间的空格：Windows 的计算机账号、域用户名中会有）。
fn valid_user(s: &str) -> bool {
    if s.starts_with('-') || s.ends_with('\\') {
        return false;
    }
    let chars: Vec<char> = s.chars().collect();
    chars.iter().enumerate().all(|(i, &c)| {
        !c.is_control()
            && !"'`\";&<>|(){}".contains(c)
            && !(c.is_whitespace() && chars.get(i + 1) == Some(&'-'))
    })
}

/// Windows 命令行的第一个词（程序）与其余部分（原样作为参数行）：开头是双引号时到下一个双引号为止，
/// 否则到第一个空白为止（与 CommandLineToArgvW 拆分第一个参数的规则相同）。
pub fn split_program(line: &str) -> (&str, &str) {
    let line = line.trim_start();
    if let Some(rest) = line.strip_prefix('"') {
        return match rest.find('"') {
            Some(end) => (&rest[..end], rest[end + 1..].trim_start()),
            None => (rest, ""),
        };
    }
    match line.find(char::is_whitespace) {
        Some(end) => (&line[..end], line[end..].trim_start()),
        None => (line, ""),
    }
}

/// 启动用的命令。
fn command(line: &str) -> tokio::process::Command {
    #[cfg(windows)]
    {
        let (program, args) = split_program(line);
        let mut cmd = tokio::process::Command::new(program);
        if !args.is_empty() {
            cmd.raw_arg(args);
        }
        // CREATE_NO_WINDOW
        cmd.creation_flags(0x0800_0000);
        cmd
    }
    #[cfg(not(windows))]
    {
        let shell = std::env::var_os("SHELL")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/sh".into());
        let mut cmd = tokio::process::Command::new(shell);
        cmd.arg("-c").arg(format!("exec {line}"));
        cmd
    }
}

/// 启动代理命令（`line` 是替换过占位符的命令行）。返回的流用作 SSH 的传输；`ProxyWatch` 在连接失败时说明原因。
pub(crate) fn spawn(line: &str) -> Result<(BoxStream, ProxyWatch), SshError> {
    let (program, _) = split_program(line);
    if program.is_empty() {
        return Err(SshError::ProxyCommandInvalid(CommandIssue::Empty));
    }
    let mut cmd = command(line);
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // 只记程序名：参数中可能带令牌
    tracing::debug!(program = %program, "启动代理命令");
    let mut child = cmd
        .spawn()
        .map_err(|e| SshError::ProxyCommand(format!("无法启动 {program}：{e}")))?;
    #[cfg(windows)]
    let job = child.raw_handle().and_then(job::Job::for_process);
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (Some(stdin), Some(stdout)) = (stdin, stdout) else {
        return Err(SshError::ProxyCommand("没有标准输入输出".into()));
    };

    let tail = Arc::new(Mutex::new(Vec::new()));
    let reader = stderr.map(|mut stderr| {
        let tail = tail.clone();
        crate::runtime::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut tail = tail.lock().unwrap_or_else(|e| e.into_inner());
                        tail.extend_from_slice(&buf[..n]);
                        if tail.len() > STDERR_KEEP {
                            let cut = tail.len() - STDERR_KEEP;
                            tail.drain(..cut);
                        }
                    }
                }
            }
        })
    });

    let (exit_tx, exit) = watch::channel(None);
    let (close_tx, mut close_rx) = oneshot::channel::<()>();
    let program_name = program.to_string();
    crate::runtime::spawn(async move {
        #[cfg(windows)]
        let _job = job;
        let (status, after_close, killed) = tokio::select! {
            // 已经退出的优先：连接因它退出而断开时，两边可能同时就绪
            biased;
            status = child.wait() => (status, false, false),
            _ = &mut close_rx => match tokio::time::timeout(CLOSE_GRACE, child.wait()).await {
                Ok(status) => (status, true, false),
                Err(_) => {
                    let _ = child.start_kill();
                    (child.wait().await, true, true)
                }
            },
        };
        if let Some(reader) = reader {
            let _ = tokio::time::timeout(STDERR_DRAIN, reader).await;
        }
        let code = status.ok().and_then(|s| s.code());
        tracing::debug!(program = %program_name, ?code, after_close, killed, "代理命令已结束");
        exit_tx.send_replace(Some(Exit {
            code,
            after_close,
            killed,
        }));
    });

    let stream = CommandStream {
        io: tokio::io::join(stdout, stdin),
        _close: close_tx,
    };
    Ok((Box::new(stream), ProxyWatch { tail, exit }))
}

/// 代理命令的结束。
#[derive(Clone, Copy, Debug)]
struct Exit {
    /// 退出码；被信号结束（Unix）时为 `None`
    code: Option<i32>,
    /// 连接已经结束（关闭了它的标准输入）之后才退出
    after_close: bool,
    /// 关闭标准输入后没有自己退出，被结束
    killed: bool,
}

/// 代理命令的连接：随连接一起关闭标准输入、结束进程。
struct CommandStream {
    io: tokio::io::Join<tokio::process::ChildStdout, tokio::process::ChildStdin>,
    /// 丢弃时通知等待进程的任务：连接结束了
    _close: oneshot::Sender<()>,
}

impl AsyncRead for CommandStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl AsyncWrite for CommandStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

/// 连接失败时查看代理命令是否已经退出、为什么退出。
pub(crate) struct ProxyWatch {
    tail: Arc<Mutex<Vec<u8>>>,
    exit: watch::Receiver<Option<Exit>>,
}

impl ProxyWatch {
    /// 连接（握手）失败：代理命令自己退出了（退出码不为 0，或在连接结束前就退出）时换成说明退出码与标准错误的错误；
    /// 还在运行、是连接结束后才退出或被结束的，原样返回原来的错误。只处理传输层的错误（连接断开、协议错误），
    /// 指纹、认证等错误与代理命令无关，不等待。
    pub(crate) async fn explain(mut self, error: SshError) -> SshError {
        if !matches!(error, SshError::Disconnected | SshError::Protocol(_)) {
            return error;
        }
        let exit =
            match tokio::time::timeout(EXPLAIN_WAIT, self.exit.wait_for(Option::is_some)).await {
                Ok(Ok(exit)) => *exit,
                _ => None,
            };
        let Some(exit) = exit else {
            return error;
        };
        if exit.killed || (exit.code == Some(0) && exit.after_close) {
            return error;
        }
        tracing::info!(code = ?exit.code, "代理命令已退出，连接失败");
        SshError::ProxyCommandExited {
            code: exit.code,
            stderr: stderr_tail(&self.tail.lock().unwrap_or_else(|e| e.into_inner())),
        }
    }
}

/// 标准错误的最后几行（去掉空行，每行截短）。Windows 上不是 UTF-8 时按系统的代码页解码（如 cmd 的中文提示）。
fn stderr_tail(raw: &[u8]) -> String {
    let text = match std::str::from_utf8(raw) {
        Ok(text) => text.to_string(),
        Err(_) => match crate::import::ansi_encoding() {
            Some(encoding) => encoding.decode_without_bom_handling(raw).0.into_owned(),
            None => String::from_utf8_lossy(raw).into_owned(),
        },
    };
    let lines: Vec<String> = text
        .lines()
        .map(|line| line.trim_end())
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let line: String = line.chars().filter(|c| !c.is_control()).collect();
            if line.chars().count() > STDERR_LINE_CHARS {
                let mut short: String = line.chars().take(STDERR_LINE_CHARS).collect();
                short.push('…');
                short
            } else {
                line
            }
        })
        .collect();
    let skip = lines.len().saturating_sub(STDERR_LINES);
    lines[skip..].join("\n")
}

// ---------------------------------------------------------------- 确认

/// 命令原文在本机设置中的记录（SHA-256）：不存命令本身（可能带令牌）。
fn fingerprint(template: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"termlet:proxy-command:v1\0");
    hasher.update(template.trim().as_bytes());
    data_encoding::HEXLOWER.encode(&hasher.finalize())
}

/// 这台电脑上是否确认过这条命令（替换占位符之前的原文）。
pub fn is_allowed(store: &Store, template: &str) -> bool {
    let fingerprint = fingerprint(template);
    store
        .local_settings()
        .connection
        .allowed_proxy_commands
        .contains(&fingerprint)
}

/// 记下这台电脑确认过这条命令。
pub fn allow(store: &Store, template: &str) -> crate::Result<()> {
    let fingerprint = fingerprint(template);
    store.update_local_settings(|s| {
        let allowed = &mut s.connection.allowed_proxy_commands;
        if !allowed.contains(&fingerprint) {
            allowed.push(fingerprint);
            let excess = allowed.len().saturating_sub(MAX_ALLOWED);
            allowed.drain(..excess);
        }
    })
}

/// 这台电脑上确认过多少条命令。
pub fn allowed_count(store: &Store) -> usize {
    store
        .local_settings()
        .connection
        .allowed_proxy_commands
        .len()
}

/// 忘掉这台电脑上全部的确认：之后每条命令第一次执行前重新询问。
pub fn forget_all(store: &Store) -> crate::Result<()> {
    store.update_local_settings(|s| s.connection.allowed_proxy_commands.clear())
}

/// 同一时间只问一次：同时打开的终端、SFTP 用同一条命令时，第一个确认后其余的不再询问。
static ASKING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 连接前检查代理命令：占位符能否替换，这台电脑上是否确认过；没有确认过的请用户确认（显示替换后的完整命令），
/// 确认后记住。不是代理命令时什么也不做。`label` 是显示用的主机名称。
pub(crate) async fn ensure_allowed(
    store: &Store,
    proxy: &ProxyPlan,
    tokens: &Tokens<'_>,
    label: &str,
    prompter: &Prompter,
) -> Result<(), SshError> {
    if proxy.kind != crate::model::ProxyKind::Command {
        return Ok(());
    }
    let command = expand(&proxy.command, tokens).map_err(SshError::ProxyCommandInvalid)?;
    if is_allowed(store, &proxy.command) {
        return Ok(());
    }
    let _asking = ASKING.lock().await;
    if is_allowed(store, &proxy.command) {
        return Ok(());
    }
    let program = split_program(&command).0.to_string();
    tracing::info!(host = %label, program = %program, "代理命令还没有在这台电脑上确认，询问用户");
    let answer = prompter(Prompt::ProxyCommand(ProxyCommandPrompt {
        host: label.to_string(),
        command,
        template: proxy.command.clone(),
        from_group: proxy.from_group.clone(),
    }))
    .await;
    match answer {
        Answer::AllowProxyCommand => {
            tracing::info!(host = %label, program = %program, "用户允许执行代理命令");
            if let Err(e) = allow(store, &proxy.command) {
                // 记不下来也照常连接：下次再问
                tracing::warn!("记录确认过的代理命令失败：{e}");
            }
            Ok(())
        }
        _ => {
            tracing::info!(host = %label, "没有允许执行代理命令");
            Err(SshError::ProxyCommandUnconfirmed)
        }
    }
}

/// Windows 的作业对象：关闭时结束其中的全部进程（代理命令与它启动的子进程）。
#[cfg(windows)]
mod job {
    use std::os::windows::io::RawHandle;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    pub(super) struct Job(HANDLE);

    // 句柄只在关闭时使用
    unsafe impl Send for Job {}

    impl Job {
        /// 建一个作业对象并把进程放进去；失败时为 `None`（只是子进程不会一并结束）。
        pub(super) fn for_process(process: RawHandle) -> Option<Job> {
            // SAFETY：参数都是有效的指针或句柄；失败时关闭建好的作业对象
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    tracing::debug!("建立作业对象失败：{}", std::io::Error::last_os_error());
                    return None;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) != 0
                    && AssignProcessToJobObject(job, process as HANDLE) != 0;
                if !ok {
                    tracing::debug!(
                        "把代理命令放进作业对象失败：{}",
                        std::io::Error::last_os_error()
                    );
                    CloseHandle(job);
                    return None;
                }
                Some(Job(job))
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY：句柄由 CreateJobObjectW 返回，只关闭一次
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens<'a>(host: &'a str, user: &'a str, alias: &'a str) -> Tokens<'a> {
        Tokens {
            host,
            port: 2222,
            user,
            alias,
        }
    }

    #[test]
    fn expands_openssh_tokens() {
        let t = tokens("10.0.0.5", "ops", "web-1");
        assert_eq!(
            expand("cloudflared access ssh --hostname %h", &t).unwrap(),
            "cloudflared access ssh --hostname 10.0.0.5"
        );
        assert_eq!(
            expand("ssh -W %h:%p -l %r jump # %n 100%%", &t).unwrap(),
            "ssh -W 10.0.0.5:2222 -l ops jump # web-1 100%"
        );
        assert_eq!(
            expand(
                "aws ssm start-session --target %n --parameters portNumber=%p",
                &t
            )
            .unwrap(),
            "aws ssm start-session --target web-1 --parameters portNumber=2222"
        );
        // IPv6 地址、带区域的地址可以代入
        assert_eq!(
            expand("nc %h %p", &tokens("fe80::1%eth0", "", "")).unwrap(),
            "nc fe80::1%eth0 2222"
        );
        // 与 OpenSSH 一样，不认识的占位符、单独的 % 报错
        assert_eq!(expand("nc %d %p", &t), Err(CommandIssue::UnknownToken('d')));
        assert_eq!(
            expand("nc %h 50%", &t),
            Err(CommandIssue::UnknownToken('\0'))
        );
        assert_eq!(expand("  ", &t), Err(CommandIssue::Empty));
    }

    #[test]
    fn rejects_values_that_a_shell_would_interpret() {
        for host in [
            "a;rm", "a b", "$(id)", "`id`", "a|b", "a&b", "-oProxy", "a\"b", "a'b",
        ] {
            assert_eq!(
                expand("nc %h %p", &tokens(host, "ops", "x")),
                Err(CommandIssue::UnsafeValue('h')),
                "{host}"
            );
        }
        // 名称：中文可以，空格不行（只在用到 %n 时检查）
        assert!(expand("x %n", &tokens("h", "u", "生产-web")).is_ok());
        assert_eq!(
            expand("x %n", &tokens("h", "u", "生产 web")),
            Err(CommandIssue::UnsafeValue('n'))
        );
        assert!(expand("x %h", &tokens("h", "u", "生产 web")).is_ok());
        // 用户名：允许 $ 与中间的空格（与 OpenSSH 相同），不允许引号、分号、空格后跟 -、以 \ 结尾
        assert!(expand("x %r", &tokens("h", "HOST$", "n")).is_ok());
        assert!(expand("x %r", &tokens("h", "John Smith", "n")).is_ok());
        for user in ["a;b", "a -oX", "-l", "a'b", "dom\\", "a\nb"] {
            assert_eq!(
                expand("x %r", &tokens("h", user, "n")),
                Err(CommandIssue::UnsafeValue('r')),
                "{user}"
            );
        }
    }

    #[test]
    fn splits_the_program_like_windows() {
        assert_eq!(
            split_program(
                r#"  "C:\Program Files\cloudflared\cloudflared.exe" access ssh --hostname h"#
            ),
            (
                r"C:\Program Files\cloudflared\cloudflared.exe",
                "access ssh --hostname h"
            )
        );
        assert_eq!(
            split_program(r#"cmd /c "nc h 22 2>nul""#),
            ("cmd", r#"/c "nc h 22 2>nul""#)
        );
        assert_eq!(split_program("nc"), ("nc", ""));
        assert_eq!(split_program(r#""C:\a b\nc.exe"#), (r"C:\a b\nc.exe", ""));
        assert_eq!(split_program("   "), ("", ""));
    }

    #[test]
    fn stderr_tail_keeps_the_last_lines() {
        let raw = (1..=10)
            .map(|n| format!("line {n}\r\n\r\n"))
            .collect::<String>();
        assert_eq!(
            stderr_tail(raw.as_bytes()),
            "line 5\nline 6\nline 7\nline 8\nline 9\nline 10"
        );
        let long = "x".repeat(400);
        assert_eq!(stderr_tail(long.as_bytes()).chars().count(), 301);
        assert_eq!(stderr_tail(b"\x1b[31mred\x1b[0m"), "[31mred[0m");
    }

    #[test]
    fn confirmations_are_local_and_keyed_by_the_command_text() {
        let store = crate::testkit::memory_store();
        assert!(!is_allowed(&store, "nc %h %p"));
        allow(&store, "nc %h %p").unwrap();
        allow(&store, "  nc %h %p ").unwrap();
        assert!(is_allowed(&store, "nc %h %p"));
        assert!(!is_allowed(&store, "nc %h %p -v"), "改过的命令要重新确认");
        assert_eq!(allowed_count(&store), 1);
        // 只存哈希，不存命令（命令中可能有令牌）
        let local = store.local_settings();
        let saved = serde_json::to_string(&local.connection).unwrap();
        assert!(!saved.contains("nc %h"), "{saved}");
        assert_eq!(local.connection.allowed_proxy_commands[0].len(), 64);
        forget_all(&store).unwrap();
        assert!(!is_allowed(&store, "nc %h %p"));
    }

    fn proxy(command: &str) -> ProxyPlan {
        ProxyPlan {
            kind: crate::model::ProxyKind::Command,
            address: String::new(),
            port: 0,
            username: String::new(),
            password: None,
            command: command.into(),
            alias: "web".into(),
            from_group: Some("生产".into()),
        }
    }

    // 没有确认过时询问一次（显示替换后的命令），允许后记住；拒绝时不记，返回"没有确认"
    #[test]
    fn asks_once_and_remembers_the_answer() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let store = crate::testkit::memory_store();
        let asked = Arc::new(AtomicUsize::new(0));
        let answering = |allow: bool| -> Prompter {
            let asked = asked.clone();
            Arc::new(move |prompt| {
                asked.fetch_add(1, Ordering::SeqCst);
                let Prompt::ProxyCommand(p) = prompt else {
                    panic!("{prompt:?}");
                };
                assert_eq!(p.command, "nc 10.0.0.5 2222");
                assert_eq!(p.template, "nc %h %p");
                assert_eq!(p.from_group.as_deref(), Some("生产"));
                Box::pin(async move {
                    if allow {
                        Answer::AllowProxyCommand
                    } else {
                        Answer::Cancel
                    }
                })
            })
        };
        let t = tokens("10.0.0.5", "ops", "web");
        let run = |prompter: Prompter| {
            crate::runtime::runtime().block_on(ensure_allowed(
                &store,
                &proxy("nc %h %p"),
                &t,
                "web",
                &prompter,
            ))
        };
        assert_eq!(
            run(answering(false)),
            Err(SshError::ProxyCommandUnconfirmed)
        );
        assert!(!is_allowed(&store, "nc %h %p"));
        assert_eq!(run(answering(true)), Ok(()));
        assert_eq!(run(answering(true)), Ok(()));
        assert_eq!(asked.load(Ordering::SeqCst), 2, "确认过的不再询问");
        // 替换不了的不询问，直接报错
        let bad = crate::runtime::runtime().block_on(ensure_allowed(
            &store,
            &proxy("nc %x"),
            &t,
            "web",
            &crate::ssh::no_prompt(),
        ));
        assert_eq!(
            bad,
            Err(SshError::ProxyCommandInvalid(CommandIssue::UnknownToken(
                'x'
            )))
        );
    }

    /// 测试用的命令行：在标准错误写两行后以退出码 3 退出。
    fn failing_command() -> &'static str {
        if cfg!(windows) {
            "cmd /c \"echo first 1>&2 & echo boom 1>&2 & exit 3\""
        } else {
            // Unix 上整条命令前加了 exec（与 OpenSSH 相同），复合命令要再包一层 sh -c
            "sh -c 'echo first >&2; echo boom >&2; exit 3'"
        }
    }

    // 代理命令自己退出：连接失败时说明退出码与标准错误的末几行
    #[test]
    fn explains_an_exited_command() {
        crate::runtime::runtime().block_on(async {
            let (mut stream, watch) = spawn(failing_command()).unwrap();
            let mut out = Vec::new();
            let _ = tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut out).await;
            let error = watch.explain(SshError::Disconnected).await;
            assert_eq!(
                error,
                SshError::ProxyCommandExited {
                    code: Some(3),
                    stderr: "first\nboom".into()
                }
            );
        });
    }

    // 指纹、认证等错误与代理命令无关：原样返回，不等待
    #[test]
    fn unrelated_errors_are_kept() {
        crate::runtime::runtime().block_on(async {
            let (_stream, watch) = spawn(failing_command()).unwrap();
            let error = watch.explain(SshError::AuthCancelled).await;
            assert_eq!(error, SshError::AuthCancelled);
        });
    }

    #[test]
    fn missing_program_is_reported() {
        // 启动子进程要在运行时中（Unix 上 tokio 的子进程要它的 SIGCHLD 驱动），与连接时相同
        let result = crate::runtime::runtime()
            .block_on(async { spawn("termlet-no-such-program-x1 %h").map(|_| ()) });
        assert!(
            matches!(&result, Err(SshError::ProxyCommand(m)) if m.contains("termlet-no-such-program-x1"))
                // Unix 上经 shell 启动：找不到程序时 shell 以 127 退出
                || cfg!(unix),
            "{:?}",
            result.as_ref().err()
        );
    }

    // 连接结束（丢弃流）时进程随之结束
    #[test]
    fn process_ends_with_the_stream() {
        crate::runtime::runtime().block_on(async {
            let line = if cfg!(windows) {
                // 一直等标准输入的程序
                "cmd /c \"more\""
            } else {
                "cat"
            };
            let (stream, mut watch) = spawn(line).unwrap();
            drop(stream);
            let ended = tokio::time::timeout(
                Duration::from_secs(10),
                watch.exit.wait_for(Option::is_some),
            )
            .await;
            assert!(matches!(ended, Ok(Ok(_))), "进程没有结束");
        });
    }
}
