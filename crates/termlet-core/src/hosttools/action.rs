//! 写操作（结束进程、启停服务、容器与镜像等）、在新的终端中执行的命令，以及审计记录的写法。读日志见 `logs.rs`。
//!
//! 参数的白名单：远端返回的名称（进程名、容器名等）不进命令。进程用 PID，服务用单元名，容器、镜像、网络用 docker
//! 给出的 ID，卷只有名称（docker 对卷名的限制与这里相同）。

use std::sync::LazyLock;

use regex::Regex;

use super::docker::{Engine, ID_RE, VOLUME_RE};
use super::exec::{Limits, Target, on_runtime, one_line, run, sh_quote, sh_script};
use super::prune::PruneKind;
use super::sections::{PrivResult, PrivSudo, unreadable};
use super::{ACTION_LIMIT, ACTION_TIMEOUT, HostTools, SudoState, ToolsError, scripts};

/// 单元名：字母、数字与 `@ . _ : - \`（实例名中的转义），不以 `-` 开头，以 `.service` 或 `.timer`（定时器）结尾。
pub(crate) static UNIT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z0-9@_:\\][A-Za-z0-9@._:\\-]{0,240}\.(service|timer)$").expect("正则表达式")
});

/// 一次操作最多的对象数（compose 项目整体启停）。
pub const MAX_IDS: usize = 200;

/// 在新的终端中执行的命令的长度上限（例如 200 个容器的项目整体启停时可能超过）。
pub const MAX_TERMINAL_COMMAND: usize = 4 << 10;

/// 结束进程用的信号。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Signal {
    /// 结束（SIGTERM）
    Term,
    /// 强制结束（SIGKILL）
    Kill,
}

/// 服务的操作。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ServiceOp {
    Start,
    Stop,
    Restart,
    /// 重新加载配置
    Reload,
    /// 开机启用
    Enable,
    /// 开机禁用
    Disable,
}

impl ServiceOp {
    fn word(self) -> &'static str {
        match self {
            ServiceOp::Start => "start",
            ServiceOp::Stop => "stop",
            ServiceOp::Restart => "restart",
            ServiceOp::Reload => "reload",
            ServiceOp::Enable => "enable",
            ServiceOp::Disable => "disable",
        }
    }
}

/// 容器的操作。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ContainerOp {
    Start,
    Stop,
    Restart,
    /// 删除（`docker rm`）
    Remove,
}

impl ContainerOp {
    fn word(self) -> &'static str {
        match self {
            ContainerOp::Start => "start",
            ContainerOp::Stop => "stop",
            ContainerOp::Restart => "restart",
            ContainerOp::Remove => "rm",
        }
    }
}

/// 面板的一个写操作。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// 结束进程。`start_ticks` 是列表中读到的这个进程的启动时刻（[`super::Process::start_ticks`]）：给出时执行前核对，
    /// 不一致（进程已结束、PID 已被重新使用）时不执行，返回 [`ToolsError::ProcessReplaced`]。在终端中执行时不核对
    Kill {
        pid: u32,
        signal: Signal,
        start_ticks: Option<u64>,
    },
    /// 服务或定时器（`unit` 例如 `nginx.service`、`backup.timer`）
    Service { op: ServiceOp, unit: String },
    /// 容器（compose 项目整体启停时是项目内的全部容器）
    Container {
        engine: Engine,
        op: ContainerOp,
        ids: Vec<String>,
    },
    /// 删除镜像（ID）
    RemoveImages { engine: Engine, ids: Vec<String> },
    /// 删除卷（名称）
    RemoveVolumes { engine: Engine, names: Vec<String> },
    /// 删除网络（ID）
    RemoveNetworks { engine: Engine, ids: Vec<String> },
    /// 清理一类未使用的对象（`docker container prune -f` 等，见 [`PruneKind`]）；Podman 没有构建缓存的清理
    Prune { engine: Engine, kind: PruneKind },
}

impl Action {
    /// 校验参数并返回要执行的命令（不含 sudo）。
    pub fn argv(&self) -> Result<Vec<String>, ToolsError> {
        let invalid = |reason: &str| Err(ToolsError::Invalid(reason.to_string()));
        let items = |engine: Engine, head: &[&str], ids: &[String], volume: bool| {
            if ids.is_empty() || ids.len() > MAX_IDS {
                return Err(ToolsError::Invalid(format!("对象应有 1–{MAX_IDS} 个")));
            }
            let check: &Regex = if volume { &VOLUME_RE } else { &ID_RE };
            if let Some(bad) = ids.iter().find(|id| !check.is_match(id)) {
                return Err(ToolsError::Invalid(format!("ID 不正确：{bad}")));
            }
            let mut argv = vec![engine.command().to_string()];
            argv.extend(head.iter().map(|s| s.to_string()));
            argv.extend(ids.iter().cloned());
            Ok(argv)
        };
        match self {
            Action::Kill { pid, signal, .. } => {
                if *pid == 0 {
                    return invalid("PID 不正确");
                }
                let signal = match signal {
                    Signal::Term => "-TERM",
                    Signal::Kill => "-KILL",
                };
                Ok(vec!["kill".into(), signal.into(), pid.to_string()])
            }
            Action::Service { op, unit } => {
                if !UNIT_RE.is_match(unit) {
                    return invalid("服务名不正确");
                }
                Ok(vec![
                    "systemctl".into(),
                    op.word().into(),
                    "--".into(),
                    unit.clone(),
                ])
            }
            Action::Container { engine, op, ids } => items(*engine, &[op.word()], ids, false),
            Action::RemoveImages { engine, ids } => items(*engine, &["rmi"], ids, false),
            Action::RemoveVolumes { engine, names } => {
                items(*engine, &["volume", "rm"], names, true)
            }
            Action::RemoveNetworks { engine, ids } => {
                items(*engine, &["network", "rm"], ids, false)
            }
            Action::Prune { engine, kind } => {
                if *engine == Engine::Podman && *kind == PruneKind::BuildCache {
                    return invalid("Podman 没有构建缓存的清理");
                }
                Ok(vec![
                    engine.command().into(),
                    kind.word().into(),
                    "prune".into(),
                    "-f".into(),
                ])
            }
        }
    }

    /// 要执行的命令写成一行（不含 sudo）：执行前写审计记录、执行出错时说明用。
    pub fn command_line(&self) -> Result<String, ToolsError> {
        self.argv().map(|argv| display(&argv))
    }

    /// 经免密码 sudo 执行时的命令（`sudo` 为假时与 [`Self::command_line`] 相同）：与 [`ActionResult::command`]、
    /// 审计记录的写法一致，确认框据此写出将执行的命令。
    pub fn command_line_with(&self, sudo: bool) -> Result<String, ToolsError> {
        let line = self.command_line()?;
        Ok(if sudo {
            format!("sudo -n {line}")
        } else {
            line
        })
    }

    /// 执行的脚本与参数：结束进程且给出了启动时刻时先核对。
    fn script(&self, argv: Vec<String>) -> (String, Vec<String>) {
        match self {
            Action::Kill {
                pid,
                start_ticks: Some(ticks),
                ..
            } => {
                let mut args = vec![pid.to_string(), ticks.to_string()];
                args.extend(argv);
                (scripts::kill_script(), args)
            }
            _ => (scripts::action_script(), argv),
        }
    }
}

/// 操作的结果。命令以非 0 退出码结束时 `ok` 为假，`output` 是它的输出（原因）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionResult {
    pub ok: bool,
    pub exit_code: u32,
    /// 命令的标准输出与标准错误（最多 256 KB）
    pub output: String,
    /// 经免密码 sudo 执行
    pub sudo: bool,
    /// 执行的命令（用了 sudo 时带 `sudo -n`），写进审计记录
    pub command: String,
}

/// 执行操作的命令行（测试用）。
pub(crate) fn action_command(action: &Action) -> Result<String, ToolsError> {
    let argv = action.argv()?;
    let (script, args) = action.script(argv);
    Ok(sh_script(&one_line(&script), &args))
}

/// 解析操作的执行结果。
pub(crate) fn parse_action(
    action: &Action,
    result: &crate::ssh::ExecResult,
) -> Result<ActionResult, ToolsError> {
    let out = String::from_utf8_lossy(&result.stdout);
    if out.trim() == "@@gone" {
        return Err(ToolsError::ProcessReplaced);
    }
    let parsed = PrivResult::parse(&out).ok_or_else(|| unreadable(result))?;
    if let Some(error) = parsed.root_failure() {
        return Err(error);
    }
    let sudo = parsed.sudo == PrivSudo::Used;
    let command = action.command_line_with(sudo)?;
    Ok(ActionResult {
        ok: parsed.code == 0,
        exit_code: parsed.code,
        output: parsed.output,
        sudo,
        command,
    })
}

impl HostTools {
    /// 执行操作：先直接执行，权限不足且免密码 sudo 可用时改用 `sudo -n`；都不行时返回 [`ToolsError::NeedRoot`]。
    /// 结束进程且给出了启动时刻时先核对，进程已结束或已被替换时返回 [`ToolsError::ProcessReplaced`]（没有执行）。
    /// 执行超时（[`super::ExecError::Timeout`]）时命令可能已经在服务器上执行，结果未知。
    pub async fn act(&self, target: &Target, action: Action) -> Result<ActionResult, ToolsError> {
        let command = action_command(&action)?;
        let target = target.clone();
        tracing::info!(action = ?action, "主机工具执行操作");
        on_runtime(async move {
            let limits = Limits {
                timeout: ACTION_TIMEOUT,
                output: ACTION_LIMIT,
                long: true,
            };
            let result = run(&target, &command, limits).await;
            let parsed = result
                .map_err(ToolsError::from)
                .and_then(|r| parse_action(&action, &r));
            match &parsed {
                // 只记结果的概要：命令的输出可能有服务器上的敏感内容，不写进日志
                Ok(result) => tracing::info!(
                    action = ?action,
                    ok = result.ok,
                    exit_code = result.exit_code,
                    sudo = result.sudo,
                    output_len = result.output.len(),
                    "主机工具操作完成"
                ),
                Err(e) => tracing::warn!(action = ?action, "主机工具操作失败：{e}"),
            }
            parsed
        })
        .await
    }
}

/// 要在新的终端中执行的命令（在共用这条连接的新 Tab 中执行）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalRequest {
    /// 跟随服务日志；`sudo` 为真时命令前加 sudo（日志经 sudo 读取时）
    ServiceLogs { unit: String, sudo: bool },
    /// 跟随容器日志；`sudo` 为真时加 sudo（Docker 页经 sudo 读取时，见 [`super::DockerInfo::sudo`]）
    ContainerLogs {
        engine: Engine,
        id: String,
        sudo: bool,
    },
    /// 进入容器（有 bash 用 bash，否则用 sh）
    ContainerShell {
        engine: Engine,
        id: String,
        sudo: bool,
    },
    /// 在终端中执行操作（权限不足时：sudo 的密码提示出现在终端里）。`root` 是 [`ToolsError::NeedRoot`] 给出的 sudo
    /// 情况：没有 sudo、不允许用 sudo 时改用 `su -c`（需要 root 的密码），否则用 sudo；`su` 为真时一定用 `su -c`
    /// （sudo 要密码时也可能根本没有 sudo 权限，由用户选择改用 su）
    Action {
        action: Action,
        root: Option<SudoState>,
        su: bool,
    },
}

/// 返回在新的终端中执行的命令（交给用户的登录 shell 解释，参数按需加引号）。参数经过与 [`HostTools::act`]、
/// [`HostTools::logs`](super::HostTools::logs) 相同的校验；返回的命令不含换行等控制字符，最长 [`MAX_TERMINAL_COMMAND`] 字节
/// （超过时返回 [`ToolsError::Invalid`]，请减少一次操作的对象）。
pub fn terminal_command(request: &TerminalRequest) -> Result<String, ToolsError> {
    let with_sudo = |sudo: bool, argv: &[&str]| {
        let line = display(argv);
        if sudo { format!("sudo {line}") } else { line }
    };
    let container = |id: &str| {
        if ID_RE.is_match(id) {
            Ok(())
        } else {
            Err(ToolsError::Invalid("容器 ID 不正确".into()))
        }
    };
    let command = match request {
        TerminalRequest::ServiceLogs { unit, sudo } => {
            if !UNIT_RE.is_match(unit) {
                return Err(ToolsError::Invalid("服务名不正确".into()));
            }
            with_sudo(
                *sudo,
                &["journalctl", "-u", unit.as_str(), "-f", "-n", "200"],
            )
        }
        TerminalRequest::ContainerLogs { engine, id, sudo } => {
            container(id)?;
            with_sudo(
                *sudo,
                &[engine.command(), "logs", "-f", "--tail", "200", id.as_str()],
            )
        }
        TerminalRequest::ContainerShell { engine, id, sudo } => {
            container(id)?;
            with_sudo(
                *sudo,
                &[
                    engine.command(),
                    "exec",
                    "-it",
                    id.as_str(),
                    "sh",
                    "-c",
                    scripts::CONTAINER_SHELL,
                ],
            )
        }
        TerminalRequest::Action { action, root, su } => {
            let line = action.command_line()?;
            let use_su = *su || matches!(root, Some(SudoState::Denied | SudoState::NotInstalled));
            if use_su {
                format!("su -c {}", sh_quote(&line))
            } else {
                format!("sudo {line}")
            }
        }
    };
    if command.len() > MAX_TERMINAL_COMMAND {
        return Err(ToolsError::Invalid(format!(
            "命令太长（超过 {} KB），不能在终端中执行；请减少一次操作的对象",
            MAX_TERMINAL_COMMAND >> 10
        )));
    }
    if command.chars().any(|c| c != '\t' && c.is_control()) {
        return Err(ToolsError::Invalid("命令中有控制字符".into()));
    }
    Ok(command)
}

/// 不用加引号的参数。
static PLAIN_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9@%+=:,./_-]+$").expect("正则表达式"));

/// 把命令写成一行：只含常见字符的参数原样，其余加单引号（[`sh_quote`]，bash、zsh、fish 都能正确解释）。
pub fn display<S: AsRef<str>>(argv: &[S]) -> String {
    argv.iter()
        .map(|arg| {
            let arg = arg.as_ref();
            if PLAIN_WORD.is_match(arg) {
                arg.to_string()
            } else {
                sh_quote(arg)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// 审计记录中名称的长度上限（字符）。
const MAX_LABEL: usize = 120;

/// 写进审计记录的内容：执行的命令，界面上的名称（进程名、容器名等，来自远端，去掉控制字符与双向文字控制符，
/// 最多 120 个字符）作为注释附在后面。
pub fn audit_detail(command: &str, label: &str) -> String {
    let label: String = label
        .trim()
        .chars()
        .filter(|&c| !c.is_control() && !is_bidi_control(c))
        .collect();
    if label.is_empty() {
        return command.to_string();
    }
    let mut chars = label.chars();
    let mut shown: String = chars.by_ref().take(MAX_LABEL).collect();
    if chars.next().is_some() {
        shown.push('…');
    }
    format!("{command}  # {shown}")
}

/// Unicode 的双向文字控制符（Bidi_Control）。
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}
