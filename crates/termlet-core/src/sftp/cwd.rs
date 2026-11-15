//! 跟随终端目录：读出终端里 shell 的当前目录（SFTP 跟随终端目录）。
//!
//! 终端界面优先用 shell 主动报告的目录（OSC 7 等，见 `termlet-terminal` 的 `ShellEvent::WorkingDirectory`），
//! 这里是没有报告时的后备，不往服务器注入脚本：
//! - tmux 会话：`tmux display-message` 报告当前窗格的目录；
//! - 普通 shell：打开时请服务器设置了会话标记（[`crate::SESSION_ENV`]，值为 `TerminalSession::key`）。服务器接受时
//!   （`AcceptEnv LC_*`），在 `/proc` 中找到环境变量里带这个标记、而父进程没有的进程（即这个终端的 shell），取终端前台
//!   进程组（例如嵌套的 bash）的当前目录，读不到时用 shell 自己的。找到的进程号缓存起来，之后每次只读两个文件。
//!
//! 只支持 Linux（`/proc`）。服务器不接受 `LC_*` 环境变量、切换到了其他用户（`sudo -i`）时可能读不到或不准。

use std::sync::Mutex;
use std::time::Duration;

use super::SftpError;
use crate::ssh::{Connection, ExecResult, TerminalSession, TmuxStatus, exec};

/// 读取的最长时间。
const TIMEOUT: Duration = Duration::from_secs(10);
/// 输出的上限。
const LIMIT: usize = 8 << 10;

/// 按会话标记找到 shell 进程（`$2` 是上次找到的进程号，仍属于这个终端时直接使用），输出两行：shell 的进程号、当前目录。
const CWD_SCRIPT: &str = r#"k=$1; p=$2
if [ -z "$p" ] || ! grep -q "$k" /proc/$p/environ 2>/dev/null; then
  c=" $(grep -l "$k" /proc/[0-9]*/environ 2>/dev/null | cut -d/ -f3 | tr '\n' ' ') "
  p=
  for q in $c; do
    pp=$(awk '/^PPid:/{print $2}' /proc/$q/status 2>/dev/null)
    case "$c" in *" $pp "*) ;; *) p=$q; break;; esac
  done
fi
[ -n "$p" ] || exit 3
t=$(sed 's/.*) //' /proc/$p/stat 2>/dev/null | awk '{print $6}')
d=$(readlink /proc/$t/cwd 2>/dev/null) || d=$(readlink /proc/$p/cwd)
printf '%s\n%s\n' "$p" "$d""#;

/// 会话标记只含字母、数字、短横线，拼进命令时无需转义。
fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// 按会话标记读取终端当前目录的命令；`pid` 为上次找到的 shell 进程号。会话标记不合法时为 `None`。
/// WSL 终端同样使用（在发行版中执行）。
pub fn cwd_command(key: &str, pid: Option<u32>) -> Option<String> {
    if !valid_key(key) {
        return None;
    }
    let script = CWD_SCRIPT.replace('\'', r#"'\''"#);
    let pid = pid.map(|p| p.to_string()).unwrap_or_default();
    Some(format!(
        "sh -c '{script}' sh {}={key} {pid}",
        crate::SESSION_ENV
    ))
}

/// 解析 [`cwd_command`] 的执行结果：shell 的进程号与当前目录；读不到时为 `None`。
pub fn parse_cwd(result: &ExecResult) -> Option<(u32, String)> {
    if result.exit_code != Some(0) {
        return None;
    }
    let text = String::from_utf8_lossy(&result.stdout);
    let lines: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
    if lines.len() != 2 || !lines[1].starts_with('/') {
        return None;
    }
    Some((lines[0].trim().parse().ok()?, lines[1].to_string()))
}

/// 一个终端的当前目录读取器（缓存找到的 shell 进程号）。每个终端 Tab 一个，重连后照常使用。
pub struct TerminalCwd {
    key: String,
    tmux: bool,
    single_channel: bool,
    pid: Mutex<Option<u32>>,
}

impl TerminalCwd {
    /// `tmux` 是终端打开时的结果（[`TmuxStatus::Attached`] 时读 tmux 窗格的目录）；`single_channel` 是主机的
    /// "只用一个通道"（不能另开通道执行命令）。
    pub fn new(session: &TerminalSession, tmux: TmuxStatus, single_channel: bool) -> Self {
        Self {
            key: session.key.clone(),
            tmux: tmux == TmuxStatus::Attached,
            single_channel,
            pid: Mutex::new(None),
        }
    }

    /// 读出终端的当前目录（经这个终端的连接执行命令）；无法得知时为 `Ok(None)`
    /// （服务器不接受会话标记、不是 Linux 等），界面据此提示。只用一个通道的主机返回 [`SftpError::SingleChannel`]。
    pub async fn read(&self, connection: &Connection) -> Result<Option<String>, SftpError> {
        if self.single_channel {
            return Err(SftpError::SingleChannel);
        }
        if connection.is_closed() {
            return Err(SftpError::Disconnected);
        }
        if self.tmux {
            let command = format!(
                "tmux display-message -p -t {} '#{{pane_current_path}}'",
                self.key
            );
            if !valid_key(&self.key) {
                return Ok(None);
            }
            let result = exec(connection, &command, LIMIT, TIMEOUT).await?;
            let dir = String::from_utf8_lossy(&result.stdout).trim().to_string();
            return Ok((result.exit_code == Some(0) && dir.starts_with('/')).then_some(dir));
        }
        let cached = *self.pid.lock().unwrap();
        let Some(command) = cwd_command(&self.key, cached) else {
            return Ok(None);
        };
        let result = exec(connection, &command, LIMIT, TIMEOUT).await?;
        let parsed = parse_cwd(&result);
        *self.pid.lock().unwrap() = parsed.as_ref().map(|(pid, _)| *pid);
        Ok(parsed.map(|(_, dir)| dir))
    }

    /// 缓存的 shell 进程号（测试用）。
    #[cfg(test)]
    pub(crate) fn cached_pid(&self) -> Option<u32> {
        *self.pid.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_and_parse() {
        assert_eq!(cwd_command("a;rm -rf /", None), None);
        let command = cwd_command("termlet-abc", Some(42)).unwrap();
        assert!(command.starts_with("sh -c '"), "{command}");
        assert!(
            command.ends_with(" sh LC_TERMLET_SESSION=termlet-abc 42"),
            "{command}"
        );
        // 脚本中的单引号写成 '\''
        assert!(command.contains(r#"tr '\''\n'\'' '\'' '\''"#), "{command}");
        let ok = ExecResult {
            exit_code: Some(0),
            stdout: b"1234\n/home/u/my dir\n".to_vec(),
            ..Default::default()
        };
        assert_eq!(parse_cwd(&ok), Some((1234, "/home/u/my dir".into())));
        let missing = ExecResult {
            exit_code: Some(3),
            ..Default::default()
        };
        assert_eq!(parse_cwd(&missing), None);
        let relative = ExecResult {
            exit_code: Some(0),
            stdout: b"1\nrelative\n".to_vec(),
            ..Default::default()
        };
        assert_eq!(parse_cwd(&relative), None);
    }
}
