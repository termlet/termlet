//! 本机 WSL 发行版的终端与文件管理（只支持 Windows）。
//!
//! - 终端：[`open_terminal`] 在伪终端中运行 `wsl.exe -d <发行版> [-u <用户>] --cd ~`（启动描述见
//!   [`crate::localshell::wsl::launch`]，会话标记经 `WSLENV` 传进 Linux）。会话结束时报告 shell 的退出码。
//! - 一次性命令：[`exec`]（以后的资源监控、检测端口也用它）；命令经标准输入交给发行版中的 `sh`，不经过 Windows
//!   的命令行解析，引号、换行原样到达。
//! - 跟随终端目录：[`WslCwd`]，与 SSH 终端相同的脚本（[`crate::sftp::cwd_command`]）在发行版中执行。
//! - 文件管理：[`open_sftp`] 在发行版中找到 OpenSSH 的 `sftp-server`（[`find_sftp_server`]），经
//!   `wsl.exe -d <发行版> [-u <用户>] --cd ~ -e <sftp-server>` 启动，把它的标准输入输出交给
//!   [`SftpManager::open_stream`]，界面原样复用 SFTP 的面板。没有安装时返回 [`WslError::SftpServerMissing`]，
//!   带上这个发行版的安装命令。
//!
//! 全部 I/O 在核心的运行时上（`exec`、`open_sftp` 是异步函数），[`open_terminal`] 要创建伪终端，在后台线程调用。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use termlet_io::{GridSize, TerminalIo};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::process::{Child, ChildStdin, ChildStdout};

use crate::localpty::{self, PtyLaunch};
use crate::localshell::ShellError;
use crate::localshell::wsl as launch;
use crate::sftp::{SftpError, SftpManager, SftpSession, cwd_command, parse_cwd};
use crate::ssh::ExecResult;

/// 找 sftp-server 的最长时间（第一次启动发行版可能要几秒）。
const FIND_TIMEOUT: Duration = Duration::from_secs(60);
/// 读取当前目录的最长时间。
const CWD_TIMEOUT: Duration = Duration::from_secs(10);
/// 读取当前目录时输出的上限。
const CWD_LIMIT: usize = 8 << 10;
/// sftp-server 启动失败时保留的错误输出。
const STDERR_LIMIT: usize = 4 << 10;
/// 关闭文件管理后等 sftp-server 自己退出的时间，过了就结束 wsl.exe。
const EXIT_GRACE: Duration = Duration::from_secs(3);

/// WSL 的错误。消息是写给日志看的，界面按变体选用自己的文案。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WslError {
    /// 不是 Windows、没有安装 WSL、发行版名称或用户名不合法
    #[error("{0}")]
    Shell(#[from] ShellError),
    /// 没有这个发行版（已卸载、改了名）
    #[error("没有名为 {distro} 的 WSL 发行版")]
    DistroNotFound { distro: String },
    /// wsl.exe 无法启动
    #[error("无法运行 wsl.exe：{0}")]
    Spawn(String),
    /// wsl.exe 报告了错误（原样带上它的说明）
    #[error("WSL 出错：{0}")]
    Failed(String),
    #[error("WSL 中的命令在 {0} 秒内没有结束")]
    Timeout(u64),
    /// 发行版中没有 OpenSSH 的 sftp-server；`install` 是这个发行版的安装命令（认不出发行版时为空）
    #[error("发行版 {distro} 中没有安装 sftp-server")]
    SftpServerMissing {
        distro: String,
        install: Option<String>,
    },
    /// sftp-server 启动后没能建立 SFTP 会话
    #[error("{0}")]
    Sftp(#[from] SftpError),
}

/// 打开发行版的终端：以 `user`（为空时是发行版的默认用户）登录，从 Linux 中的用户主目录开始。
/// `session_key` 是终端的会话标识（`TerminalSession::key`，读取当前目录时用；为空时不设置）。
/// 要读取用户的环境变量、创建伪终端，在后台线程调用。
pub fn open_terminal(
    distro: &str,
    user: &str,
    session_key: &str,
    size: GridSize,
) -> Result<TerminalIo, WslError> {
    tracing::debug!(
        distro,
        user,
        cols = size.columns,
        rows = size.rows,
        "打开 WSL 终端"
    );
    let launch = launch::launch(distro, user, session_key)
        .inspect_err(|e| tracing::warn!(distro, user, "准备 WSL 终端失败：{e}"))?;
    let pty = PtyLaunch {
        program: launch.program,
        args: launch.args,
        cwd: Some(launch.cwd),
        env: Some(launch.env),
    };
    localpty::spawn(pty, size).map_err(|e| WslError::Spawn(e.to_string()))
}

/// `wsl.exe` 的命令（不弹出控制台窗口，自己的说明用 UTF-8 输出）。
fn command(args: &[String]) -> Result<tokio::process::Command, WslError> {
    let exe = launch::exe()?;
    let mut command = tokio::process::Command::new(exe);
    command.args(args).env("WSL_UTF8", "1").kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    Ok(command)
}

/// 只保留前 `limit` 字节，其余读完丢掉（让程序正常结束）。
async fn read_limited<R: AsyncRead + Unpin>(mut reader: R, limit: usize) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = limit.saturating_sub(kept.len());
                if n > room {
                    truncated = true;
                }
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
    (kept, truncated)
}

/// 在发行版中以 `user`（为空时是默认用户）经 `sh` 执行命令，标准输出、标准错误各最多保留 `limit` 字节，
/// 最多等 `timeout`。命令以非零退出码结束不算出错（见 [`ExecResult::exit_code`]）。
pub async fn exec(
    distro: &str,
    user: &str,
    script: &str,
    limit: usize,
    timeout: Duration,
) -> Result<ExecResult, WslError> {
    // 资源监控等会反复执行：只记 trace，不记脚本内容
    tracing::trace!(distro, user, "在 WSL 中执行命令");
    let args = launch::args(distro, user, &["-e", "sh", "-s"])?;
    let mut child = command(&args)?
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| WslError::Spawn(e.to_string()))?;
    let mut stdin = child.stdin.take().expect("标准输入是管道");
    let stdout = child.stdout.take().expect("标准输出是管道");
    let stderr = child.stderr.take().expect("标准错误是管道");
    let input = format!("{script}\n");
    let run = async {
        let feed = async {
            let _ = stdin.write_all(input.as_bytes()).await;
            drop(stdin);
        };
        let (_, (out, out_cut), (err, err_cut), status) = tokio::join!(
            feed,
            read_limited(stdout, limit),
            read_limited(stderr, limit),
            child.wait()
        );
        (out, err, out_cut || err_cut, status)
    };
    let (stdout, stderr, truncated, status) = tokio::time::timeout(timeout, run)
        .await
        .map_err(|_| WslError::Timeout(timeout.as_secs()))?;
    let status = status.map_err(|e| WslError::Spawn(e.to_string()))?;
    Ok(ExecResult {
        exit_code: status.code().map(|code| code as u32),
        stdout,
        stderr,
        truncated,
    })
}

/// wsl.exe 自己出错（发行版不存在、WSL 没有启用）时的错误；发行版不在列表中时报 [`WslError::DistroNotFound`]。
async fn wsl_failure(distro: &str, result: &ExecResult) -> WslError {
    let distro_name = distro.to_string();
    let listed = tokio::task::spawn_blocking(move || {
        launch::distributions()
            .iter()
            .any(|d| d.eq_ignore_ascii_case(&distro_name))
    })
    .await
    .unwrap_or(true);
    if !listed {
        return WslError::DistroNotFound {
            distro: distro.to_string(),
        };
    }
    let mut message = launch::decode_output(&result.stderr).trim().to_string();
    if message.is_empty() {
        message = launch::decode_output(&result.stdout).trim().to_string();
    }
    if message.is_empty() {
        message = format!("退出码 {:?}", result.exit_code);
    }
    WslError::Failed(message)
}

/// 一个 WSL 终端的当前目录读取器（缓存找到的 shell 进程号）。每个终端 Tab 一个，重新打开后照常使用。
pub struct WslCwd {
    distro: String,
    user: String,
    key: String,
    pid: Mutex<Option<u32>>,
}

impl WslCwd {
    /// `session_key` 是打开终端时给 [`open_terminal`] 的会话标识。
    pub fn new(distro: &str, user: &str, session_key: &str) -> Self {
        Self {
            distro: distro.to_string(),
            user: user.to_string(),
            key: session_key.to_string(),
            pid: Mutex::new(None),
        }
    }

    /// 读出终端的当前目录（前台进程组的目录，嵌套的 shell 也能跟随）；无法得知时为 `Ok(None)`。
    pub async fn read(&self) -> Result<Option<String>, WslError> {
        let cached = *self.pid.lock().unwrap();
        let Some(script) = cwd_command(&self.key, cached) else {
            return Ok(None);
        };
        let result = exec(&self.distro, &self.user, &script, CWD_LIMIT, CWD_TIMEOUT).await?;
        let parsed = parse_cwd(&result);
        *self.pid.lock().unwrap() = parsed.as_ref().map(|(pid, _)| *pid);
        Ok(parsed.map(|(_, dir)| dir))
    }
}

/// 找 sftp-server：各发行版的 OpenSSH 包放的位置（Debian / Ubuntu、Fedora / RHEL、Arch / Alpine、openSUSE、
/// Gentoo、Void 等），再找 PATH；都没有时报告发行版（`/etc/os-release` 的 ID 与 ID_LIKE），给出安装命令。
const FIND_SCRIPT: &str = r#"for p in /usr/lib/openssh/sftp-server /usr/libexec/openssh/sftp-server /usr/lib/ssh/sftp-server /usr/libexec/ssh/sftp-server /usr/libexec/sftp-server /usr/lib/sftp-server /usr/lib/misc/sftp-server /usr/sbin/sftp-server; do
  if [ -x "$p" ]; then echo "ok $p"; exit 0; fi
done
p=$(command -v sftp-server 2>/dev/null) && [ -x "$p" ] && { echo "ok $p"; exit 0; }
ID=; ID_LIKE=
[ -r /etc/os-release ] && . /etc/os-release
echo "missing $ID $ID_LIKE""#;

/// [`FIND_SCRIPT`] 的结果。
#[derive(Debug, PartialEq, Eq)]
enum Found {
    /// sftp-server 的路径
    At(String),
    /// 没有；发行版的 ID 与 ID_LIKE
    Missing(Vec<String>),
}

fn parse_found(output: &str) -> Option<Found> {
    let line = output.lines().rev().find(|l| !l.trim().is_empty())?.trim();
    if let Some(path) = line.strip_prefix("ok ") {
        let path = path.trim();
        // 路径作为 wsl.exe 的一个参数传过去：只接受普通的绝对路径
        let plain = path.starts_with('/')
            && path.len() <= 255
            && path.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b'+')
            });
        return plain.then(|| Found::At(path.to_string()));
    }
    let rest = line.strip_prefix("missing")?;
    Some(Found::Missing(
        rest.split_whitespace()
            .map(|s| s.trim_matches('"').to_ascii_lowercase())
            .collect(),
    ))
}

/// 按发行版的 ID 与 ID_LIKE 给出安装 sftp-server 的命令。
fn install_command(ids: &[String]) -> Option<String> {
    let has = |names: &[&str]| ids.iter().any(|id| names.contains(&id.as_str()));
    let command = if has(&["debian", "ubuntu"]) {
        "sudo apt install openssh-sftp-server"
    } else if has(&["alpine"]) {
        "sudo apk add openssh-sftp-server"
    } else if has(&[
        "fedora",
        "rhel",
        "centos",
        "rocky",
        "almalinux",
        "ol",
        "amzn",
    ]) {
        "sudo dnf install openssh-server"
    } else if has(&["arch", "manjaro", "endeavouros"]) {
        "sudo pacman -S openssh"
    } else if has(&[
        "opensuse",
        "suse",
        "sles",
        "opensuse-leap",
        "opensuse-tumbleweed",
    ]) {
        "sudo zypper install openssh-server"
    } else if has(&["gentoo"]) {
        "sudo emerge net-misc/openssh"
    } else if has(&["void"]) {
        "sudo xbps-install openssh"
    } else {
        return None;
    };
    Some(command.to_string())
}

/// 在发行版中找 OpenSSH 的 sftp-server，返回它的路径。
pub async fn find_sftp_server(distro: &str, user: &str) -> Result<String, WslError> {
    let result = exec(distro, user, FIND_SCRIPT, 4096, FIND_TIMEOUT).await?;
    let output = String::from_utf8_lossy(&result.stdout);
    match parse_found(&output) {
        Some(Found::At(path)) => Ok(path),
        Some(Found::Missing(ids)) => Err(WslError::SftpServerMissing {
            distro: distro.to_string(),
            install: install_command(&ids),
        }),
        None => Err(wsl_failure(distro, &result).await),
    }
}

/// 文件管理会话在 [`SftpManager`] 中的主机标识（同一发行版、同一用户的会话可以互相继续传输）。
pub fn sftp_host_key(distro: &str, user: &str) -> String {
    if user.is_empty() {
        format!("wsl:{distro}")
    } else {
        format!("wsl:{distro}:{user}")
    }
}

/// 打开发行版中的文件管理：以 `user`（为空时是默认用户，`root` 可以读写整个系统）运行 sftp-server，
/// 从用户主目录开始。`server` 为空时自动查找（[`find_sftp_server`]）。会话属于窗口 `owner`；
/// 发行版被关闭（`wsl --shutdown`）时会话结束、不重连。
pub async fn open_sftp(
    manager: &SftpManager,
    distro: &str,
    user: &str,
    owner: &str,
    server: Option<&str>,
) -> Result<Arc<SftpSession>, WslError> {
    let server = match server {
        Some(server) => server.to_string(),
        None => find_sftp_server(distro, user).await?,
    };
    let args = launch::args(distro, user, &["--cd", "~", "-e", &server])?;
    let mut child = command(&args)?
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| WslError::Spawn(e.to_string()))?;
    let stdin = child.stdin.take().expect("标准输入是管道");
    let stdout = child.stdout.take().expect("标准输出是管道");
    let stderr = child.stderr.take().expect("标准错误是管道");
    let errors = crate::runtime::spawn(read_limited(stderr, STDERR_LIMIT));
    let stream = WslStream {
        stdout,
        stdin: Some(stdin),
        child: Some(child),
    };
    match manager
        .open_stream(sftp_host_key(distro, user), owner, stream)
        .await
    {
        Ok(session) => {
            tracing::info!(distro, user, server = %server, "WSL 文件管理已打开");
            Ok(session)
        }
        Err(error) => {
            // 程序已随流一起结束：带上它的错误输出（例如没有权限、发行版启动失败）
            let message = match tokio::time::timeout(EXIT_GRACE, errors).await {
                Ok(Ok((stderr, _))) => launch::decode_output(&stderr).trim().to_string(),
                _ => String::new(),
            };
            tracing::warn!(
                distro,
                user,
                "无法启动 WSL 中的 sftp-server：{error}；{message}"
            );
            Err(match (error, message.is_empty()) {
                (SftpError::Unavailable(_), false) => {
                    WslError::Sftp(SftpError::Unavailable(message))
                }
                (error, _) => WslError::Sftp(error),
            })
        }
    }
}

/// sftp-server 的标准输入输出。丢弃时先关闭标准输入（sftp-server 读到结尾后自己退出），
/// 过 [`EXIT_GRACE`] 还没退出就结束 wsl.exe。
struct WslStream {
    stdout: ChildStdout,
    stdin: Option<ChildStdin>,
    child: Option<Child>,
}

impl AsyncRead for WslStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stdout).poll_read(cx, buf)
    }
}

impl WslStream {
    fn stdin(&mut self) -> std::io::Result<std::pin::Pin<&mut ChildStdin>> {
        self.stdin
            .as_mut()
            .map(std::pin::Pin::new)
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::BrokenPipe))
    }
}

impl AsyncWrite for WslStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.stdin() {
            Ok(stdin) => stdin.poll_write(cx, buf),
            Err(e) => std::task::Poll::Ready(Err(e)),
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.stdin() {
            Ok(stdin) => stdin.poll_flush(cx),
            Err(e) => std::task::Poll::Ready(Err(e)),
        }
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.stdin() {
            Ok(stdin) => stdin.poll_shutdown(cx),
            Err(_) => std::task::Poll::Ready(Ok(())),
        }
    }
}

impl Drop for WslStream {
    fn drop(&mut self) {
        drop(self.stdin.take());
        if let Some(mut child) = self.child.take() {
            crate::runtime::spawn(async move {
                if tokio::time::timeout(EXIT_GRACE, child.wait())
                    .await
                    .is_err()
                {
                    let _ = child.kill().await;
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_find_output() {
        assert_eq!(
            parse_found("ok /usr/lib/openssh/sftp-server\n"),
            Some(Found::At("/usr/lib/openssh/sftp-server".into()))
        );
        // 前面有发行版的欢迎信息等：看最后一行
        assert_eq!(
            parse_found("welcome\nok /usr/libexec/openssh/sftp-server\n\n"),
            Some(Found::At("/usr/libexec/openssh/sftp-server".into()))
        );
        assert_eq!(
            parse_found("missing ubuntu debian\n"),
            Some(Found::Missing(vec!["ubuntu".into(), "debian".into()]))
        );
        assert_eq!(
            parse_found("missing \"rhel centos fedora\"\n"),
            Some(Found::Missing(vec![
                "rhel".into(),
                "centos".into(),
                "fedora".into()
            ]))
        );
        assert_eq!(parse_found("missing  \n"), Some(Found::Missing(vec![])));
        // 不是普通的绝对路径、无法识别的输出
        assert_eq!(parse_found("ok relative/sftp-server"), None);
        assert_eq!(parse_found("ok /opt/my dir/sftp-server"), None);
        assert_eq!(parse_found("ok /x;rm -rf /"), None);
        assert_eq!(parse_found("sh: 1: syntax error"), None);
        assert_eq!(parse_found(""), None);
    }

    #[test]
    fn install_commands() {
        let ids = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        assert_eq!(
            install_command(&ids("ubuntu debian")).as_deref(),
            Some("sudo apt install openssh-sftp-server")
        );
        assert_eq!(
            install_command(&ids("pop ubuntu debian")).as_deref(),
            Some("sudo apt install openssh-sftp-server"),
            "按 ID_LIKE 认出衍生发行版"
        );
        assert_eq!(
            install_command(&ids("almalinux rhel centos fedora")).as_deref(),
            Some("sudo dnf install openssh-server")
        );
        assert_eq!(
            install_command(&ids("alpine")).as_deref(),
            Some("sudo apk add openssh-sftp-server")
        );
        assert_eq!(
            install_command(&ids("arch")).as_deref(),
            Some("sudo pacman -S openssh")
        );
        assert_eq!(
            install_command(&ids("opensuse-tumbleweed opensuse suse")).as_deref(),
            Some("sudo zypper install openssh-server")
        );
        assert_eq!(install_command(&ids("nixos")), None);
        assert_eq!(install_command(&[]), None);
    }

    #[test]
    fn host_keys_and_validation() {
        assert_eq!(sftp_host_key("Ubuntu", ""), "wsl:Ubuntu");
        assert_eq!(sftp_host_key("Ubuntu", "root"), "wsl:Ubuntu:root");
        let rt = crate::runtime::runtime();
        let bad = rt.block_on(exec("x;y", "", "true", 64, Duration::from_secs(5)));
        let want = if cfg!(windows) {
            ShellError::InvalidDistro
        } else {
            ShellError::WslUnsupported
        };
        // 不是 Windows 时先报不支持（找 wsl.exe 之前先校验名称）
        if cfg!(windows) {
            assert_eq!(bad.unwrap_err(), WslError::Shell(want));
        }
        let bad = rt.block_on(exec(
            "Ubuntu",
            "Bad User",
            "true",
            64,
            Duration::from_secs(5),
        ));
        assert!(bad.is_err());
    }

    /// 本机的第一个发行版（没有 WSL、没有发行版时跳过）；`TERMLET_TEST_WSL` 可以指定。
    #[cfg(windows)]
    fn test_distro() -> Option<String> {
        if let Ok(distro) = std::env::var("TERMLET_TEST_WSL") {
            return Some(distro);
        }
        let distro = launch::distributions().into_iter().next();
        if distro.is_none() {
            println!("本机没有 WSL 发行版，跳过");
        }
        distro
    }

    // 一次性命令：在发行版中执行，引号原样到达，退出码照实报告；名称不对时说明没有这个发行版
    #[cfg(windows)]
    #[test]
    fn exec_in_distro() {
        let Some(distro) = test_distro() else { return };
        let rt = crate::runtime::runtime();
        let result = rt
            .block_on(exec(
                &distro,
                "",
                "uname -s; echo 'quote \"ok\"'; exit 3",
                4096,
                Duration::from_secs(60),
            ))
            .unwrap();
        assert_eq!(result.exit_code, Some(3));
        assert_eq!(
            String::from_utf8_lossy(&result.stdout),
            "Linux\nquote \"ok\"\n"
        );
        let root = rt
            .block_on(exec(&distro, "root", "id -u", 64, Duration::from_secs(60)))
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&root.stdout).trim(), "0");
        // 输出超过上限时截断
        let long = rt
            .block_on(exec(
                &distro,
                "",
                "seq 1 10000",
                100,
                Duration::from_secs(60),
            ))
            .unwrap();
        assert!(long.truncated && long.stdout.len() == 100);
        let missing = rt.block_on(find_sftp_server("TermletNoSuchDistro", ""));
        assert_eq!(
            missing.unwrap_err(),
            WslError::DistroNotFound {
                distro: "TermletNoSuchDistro".into()
            }
        );
    }

    // 终端的大小、当前目录（含嵌套的 shell）与退出码
    #[cfg(windows)]
    #[test]
    fn terminal_and_cwd() {
        use termlet_io::{CloseReason, IoEvent};
        let Some(distro) = test_distro() else { return };
        let key = format!("termlet-wsltest-{}", crate::id::random_token(4));
        let io = open_terminal(&distro, "", &key, GridSize::new(100, 30)).unwrap();
        let output = Arc::new(Mutex::new(String::new()));
        let closed = Arc::new(Mutex::new(None));
        {
            let output = output.clone();
            let closed = closed.clone();
            let rx = io.output.clone();
            let input = io.input.clone();
            std::thread::spawn(move || {
                while let Ok(event) = rx.recv_blocking() {
                    match event {
                        IoEvent::Output(bytes) => {
                            let text = String::from_utf8_lossy(&bytes).into_owned();
                            // ConPTY 启动时查询光标位置（终端引擎会自动回答，这里模拟）
                            if text.contains("\x1b[6n") {
                                input.write(b"\x1b[1;1R");
                            }
                            output.lock().unwrap().push_str(&text);
                        }
                        IoEvent::Closed(reason) => {
                            *closed.lock().unwrap() = Some(reason);
                            break;
                        }
                    }
                }
            });
        }
        let wait_for = |text: &str| {
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            while !output.lock().unwrap().contains(text) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "等不到 {text:?}：{:?}",
                    output.lock().unwrap()
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        io.input.write(b"echo wsl-$((1+1)) $(stty size)\r");
        wait_for("wsl-2 30 100");
        io.input.resize(GridSize::new(120, 40));
        io.input.write(b"echo size-$(stty size)\r");
        wait_for("size-40 120");

        let cwd = WslCwd::new(&distro, "", &key);
        let rt = crate::runtime::runtime();
        let wait_cwd = |want: &str| {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            loop {
                let dir = rt.block_on(cwd.read());
                if matches!(&dir, Ok(Some(d)) if d == want) {
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "当前目录应为 {want}，实际 {dir:?}"
                );
                std::thread::sleep(Duration::from_millis(200));
            }
        };
        io.input.write(b"cd /tmp\r");
        wait_cwd("/tmp");
        io.input.write(b"bash\rcd /etc\r");
        wait_cwd("/etc");
        io.input.write(b"exit\r");
        wait_cwd("/tmp");

        io.input.write(b"exit 3\r");
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while closed.lock().unwrap().is_none() {
            assert!(std::time::Instant::now() < deadline, "shell 没有退出");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(
            closed.lock().unwrap().clone(),
            Some(CloseReason::Exited(Some(3)))
        );
    }

    // 在发行版中启动 sftp-server，经 SFTP 会话读写文件；没有安装时说明怎样安装。
    // TERMLET_TEST_WSL_SFTP_SERVER 可以指定发行版中 sftp-server 的路径（例如从安装包中解出、没有安装的）
    #[cfg(windows)]
    #[test]
    fn sftp_in_distro() {
        use crate::sftp::{EntryKind, ManagerOptions};
        let Some(distro) = test_distro() else { return };
        let rt = crate::runtime::runtime();
        let server = std::env::var("TERMLET_TEST_WSL_SFTP_SERVER").ok();
        if server.is_none() {
            match rt.block_on(find_sftp_server(&distro, "")) {
                Ok(path) => println!("{distro} 中的 sftp-server：{path}"),
                Err(WslError::SftpServerMissing { install, .. }) => {
                    println!("{distro} 中没有 sftp-server，安装命令：{install:?}，跳过");
                    let ids = rt
                        .block_on(exec(
                            &distro,
                            "",
                            ". /etc/os-release; echo $ID $ID_LIKE",
                            256,
                            FIND_TIMEOUT,
                        ))
                        .unwrap();
                    let ids = String::from_utf8_lossy(&ids.stdout).to_string();
                    if ids.contains("debian") || ids.contains("ubuntu") {
                        assert_eq!(
                            install.as_deref(),
                            Some("sudo apt install openssh-sftp-server")
                        );
                    }
                    return;
                }
                Err(e) => panic!("找 sftp-server 失败：{e}"),
            }
        }
        let manager = SftpManager::new(ManagerOptions::default());
        let session = rt
            .block_on(open_sftp(
                &manager,
                &distro,
                "",
                "test-window",
                server.as_deref(),
            ))
            .unwrap();
        let home = session.home().to_string();
        assert!(home.starts_with('/'), "应从用户主目录开始：{home}");
        let dir = format!("{home}/termlet-sftp-test-{}", crate::id::random_token(4));
        rt.block_on(async {
            session.mkdir(&dir).await.unwrap();
            let listing = session.list(&home).await.unwrap();
            let name = dir.rsplit('/').next().unwrap();
            assert!(
                listing
                    .entries
                    .iter()
                    .any(|e| e.name == name && e.kind == EntryKind::Dir),
                "{listing:?}"
            );
            session.remove(&dir).await.unwrap();
        });
        rt.block_on(manager.close_all());

        // 以 root 打开时从 /root 开始
        let root = rt
            .block_on(open_sftp(
                &manager,
                &distro,
                "root",
                "test-window",
                server.as_deref(),
            ))
            .unwrap();
        assert_eq!(root.home(), "/root");
        rt.block_on(manager.close_all());
    }
}
