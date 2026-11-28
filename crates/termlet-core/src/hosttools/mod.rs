//! 主机工具面板（进程、服务、Docker、网络连接）在远端的读取与操作。
//!
//! 与资源监控（[`crate::monitor`]）一样，所有读取与操作都在终端自己的连接上另开 exec 通道执行（[`Target`]）：
//! 不另外登录，不往服务器写任何文件，也不打进终端。脚本一律经 `sh -c` 执行，参数经 `$1…` 传入；参数先校验
//! （PID 只许数字、服务名用白名单、容器等只用 docker 给出的 ID），远端返回的名称不进命令。输出用 `@@段名` 分段解析，
//! 远端可控的内容逐行加前缀，伪造不了段标记（见 `scripts.rs`）；每条命令有超时与输出上限。
//!
//! 需要 root 时：先直接执行；权限不足且免密码 sudo 可用时改用 `sudo -n`；都不行时返回 [`ToolsError::NeedRoot`]
//! （带 sudo 的情况），由界面说明原因，或把同一条命令放进新的终端执行（[`terminal_command`]，sudo 的密码提示出现在
//! 终端里，可以自己输入或用 sudo 密码填入）。从不在后台通道里传密码。界面要事先知道有没有免密码 sudo 时用
//! [`HostTools::privilege`]（每条连接探测一次 `sudo -n true`）。
//!
//! # 用法（界面）
//!
//! - 每个终端 Tab 一个 [`HostTools`]（[`HostTools::new`] 传入终端的会话标识 `TerminalSession::key`，据此标出这个终端
//!   自己的 shell 及其子进程；它还记着上一次的进程读数，用来算 CPU 使用率）。克隆的是同一份。
//! - 每次调用时传入终端当前的连接（[`Target::ssh`]，带上主机的"只用一个通道"；WSL 终端为 [`Target::wsl`]）。
//!   本地终端、Telnet、串口没有主机工具。
//! - 方法都是异步的，内部在核心的运行时上执行，返回的 future 是 `Send`，可以在 GPUI 的任务里直接 `await`；
//!   调用方不再等待时命令照常执行完（受时限约束），写操作不会因界面关闭而中途放弃。
//! - 刷新：进程页显示时每 [`PROCESS_INTERVAL`] 读一次（上一次返回后再计时；鼠标停在列表上、菜单或确认框打开时暂停），
//!   其余页打开时读一次、有刷新按钮、操作后重读。Docker 页另读资源占用（[`HostTools::container_stats`]）与选中的
//!   容器的详情（[`HostTools::container_detail`]），都与列表分开读。
//! - 日志：[`HostTools::logs`]（[`LogQuery`]：行数、时间范围、服务的级别；跟随时按上次的 [`LogResult::cursor`]
//!   增量读取），时间换成 Unix 毫秒，错误、警告行按关键词标出（见 `logs.rs`）。第一次读进程没有 CPU 使用率，第二次起才有；上一次读数比调用方的
//!   刷新间隔的两倍还早时也不算（[`HostTools::processes`]），调用方稍后补读一次。
//! - 进程的归属：[`Process::service`]、[`Process::container`] 由同一次读取中的 `/proc/<pid>/cgroup` 认出（cgroup v1、v2，
//!   Docker、Podman、containerd、CRI-O，见 `cgroup.rs`），界面据此标出、筛选，并在进程、服务、容器之间跳转。
//! - 定时器：与服务列表一并读出（[`ServiceList::timers`]，`systemctl list-timers --all` 与 `systemctl show`），启停、
//!   开机启用与服务一样用 [`Action::Service`]（单元名以 `.timer` 结尾）。服务器的时区也一并读出（[`ServiceList::zone`]，
//!   日历表达式按它解释），各个触发时刻服务器的偏移见 [`Timer::next_offset`]：界面按服务器的时间显示，与本机不同时另写
//!   本机的时间。
//! - Docker 清理：[`HostTools::cleanup_plan`] 只读地列出可以清理的内容与可释放的空间（[`CleanupPlan::preview`]），确认后
//!   逐类执行 [`Action::Prune`]，由输出得到实际删除的个数与释放的空间（[`parse_prune`]）。
//! - 写操作：[`HostTools::act`]。操作前按 [`Action::command_line`] 取得命令（审计记录用，出错时也有）；
//!   成功时 [`ActionResult::command`] 是实际执行的命令（用了 sudo 时带 `sudo -n`）。命令以非 0 退出码结束不是错误，
//!   见 [`ActionResult::ok`] 与 [`ActionResult::output`]。审计的写法见 [`audit_detail`]。
//! - 需要 root：[`ToolsError::NeedRoot`] 的 `sudo` 说明 sudo 的情况；"在终端中执行"用
//!   [`TerminalRequest::Action`] 拼出命令，在共用这条连接的新 Tab 中执行。跟随日志、进入容器同样用
//!   [`terminal_command`]。
//! - 错误：不支持（[`ToolsError::Unsupported`]、[`ToolsError::NoSystemd`]、[`ToolsError::NoDocker`]、
//!   [`ToolsError::DockerDown`]、[`ToolsError::DockerDesktopDown`]）时整页显示原因、不再自动刷新；只用一个通道（[`ExecError::SingleChannel`]）时整个
//!   面板说明原因；连接断开（[`ExecError::Disconnected`]）时保留上次的数据，等终端重连好再读。

mod action;
mod cgroup;
mod container;
mod docker;
mod exec;
mod logs;
mod netstat;
mod privilege;
mod process;
mod prune;
mod scripts;
mod sections;
mod service;
#[cfg(test)]
pub(crate) mod tests;

use std::time::Duration;

pub use action::{
    Action, ActionResult, ContainerOp, MAX_IDS, MAX_TERMINAL_COMMAND, ServiceOp, Signal,
    TerminalRequest, audit_detail, display, terminal_command,
};
pub use container::{
    ContainerDetail, ContainerStats, Health, HealthCheck, HealthState, MountInfo, NetworkInfo,
    PortBinding, RunState,
};
pub use docker::{Container, ContainerNames, DockerInfo, Engine, Image, Network, Volume};
pub use exec::{ExecError, MAX_LONG_SIDE_CHANNELS, MAX_SIDE_CHANNELS, Target, sh_quote, sh_script};
pub use logs::{
    LogCursor, LogLevel, LogLine, LogLines, LogPriority, LogQuery, LogResult, LogSource,
    MAX_LOG_LINES, level_of,
};
pub use netstat::{Conn, ConnCount, ConnList, ConnProcess, ConnSource};
pub use privilege::Privilege;
pub use process::{Process, ProcessDetail, ProcessList};
pub use prune::{
    BuildCacheUsage, CleanupPlan, PlanContainer, PlanImage, PlanNetwork, PlanVolume, PruneItem,
    PruneKind, PruneOutcome, PrunePreview, parse_prune,
};
pub use service::{MainExit, ServerZone, Service, ServiceList, Timer};

pub(crate) use exec::{Limits, on_runtime, run};

/// 进程页显示时的刷新间隔。
pub const PROCESS_INTERVAL: Duration = Duration::from_secs(5);

/// 读取列表的最长时间。
const LIST_TIMEOUT: Duration = Duration::from_secs(20);
/// 写操作的最长时间。
const ACTION_TIMEOUT: Duration = Duration::from_secs(120);
/// 读取列表时输出的上限。
const LIST_LIMIT: usize = 4 << 20;
/// 读取日志时输出的上限（只留最后这么多字节，见 `scripts::READ_TAIL`）。
pub const LOG_LIMIT: usize = 1 << 20;
/// 写操作的输出上限。
const ACTION_LIMIT: usize = 256 << 10;

/// 需要 root 而当前用户不是 root、也没有可用的免密码 sudo 时，sudo 的情况。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SudoState {
    /// 有 sudo，但需要密码（也可能根本不在 sudoers 中，`sudo -n` 时区分不了，界面一并说明）：
    /// 可以在终端中用 sudo 或 su 执行
    Password,
    /// 这个用户不能用 sudo 执行它：在终端中执行时改用 `su -c`（需要 root 的密码）
    Denied,
    /// 没有 sudo 命令：在终端中执行时改用 `su -c`
    NotInstalled,
}

/// 主机工具的错误。消息写给日志看，界面按变体选用自己的文案。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ToolsError {
    /// 主机不是 Linux（或读不到 /proc）：进程、服务、网络页不可用
    #[error("主机工具只支持 Linux")]
    Unsupported,
    /// 脚本没能执行或没有输出预期的内容（登录 shell 不能执行 sh 的网络设备等）：带退出码与标准错误的开头
    #[error("无法读取（退出码 {exit_code:?}）：{stderr}")]
    Unreadable {
        exit_code: Option<u32>,
        stderr: String,
    },
    /// 主机没有使用 systemd
    #[error("这台主机没有使用 systemd")]
    NoSystemd,
    /// 主机上没有 docker 与 podman
    #[error("这台主机没有安装 Docker 或 Podman")]
    NoDocker,
    /// 装了 Docker，但守护进程没有运行（连不上 docker.sock）
    #[error("Docker 没有运行")]
    DockerDown,
    /// WSL 中的 docker 是 Docker Desktop 的集成，而它用不了：Docker Desktop 没有启动、启动过又退出了，或者没有为这个
    /// 发行版打开 WSL 集成（下一步在 Windows 一侧，与 [`ToolsError::DockerDown`] 不同）
    #[error("Docker Desktop 没有运行，或没有为这个发行版打开 WSL 集成")]
    DockerDesktopDown,
    /// 读取容器等失败（不是权限、不是守护进程没有运行）：带上错误输出的第一行
    #[error("读取容器失败：{0}")]
    DockerFailed(String),
    /// 读取容器日志失败（容器已不存在等）：带上错误输出的第一行
    #[error("读取日志失败：{0}")]
    LogsFailed(String),
    /// 需要 root 权限，而当前用户不是 root、也没有可用的免密码 sudo；`output` 是权限不足的原文
    #[error("需要 root 权限（sudo：{sudo:?}）：{}", .output.trim())]
    NeedRoot { sudo: SudoState, output: String },
    /// 进程已不存在（读取详情时）
    #[error("进程已不存在")]
    NoProcess,
    /// 要结束的进程已结束，或 PID 已被别的进程重新使用（启动时刻与列表中的不同），没有执行
    #[error("进程已结束或已被替换，没有执行")]
    ProcessReplaced,
    /// 参数不合法（PID、服务名、容器 ID 等），没有执行
    #[error("参数不合法：{0}")]
    Invalid(String),
    /// 执行本身失败：只用一个通道、连接断开、超时、打开通道失败、WSL 出错
    #[error("{0}")]
    Exec(#[from] ExecError),
}

/// 一个终端的主机工具：记着终端的会话标识与上一次的进程读数（算 CPU 使用率）。克隆的是同一份。
#[derive(Clone)]
pub struct HostTools {
    inner: std::sync::Arc<process::Inner>,
}

impl HostTools {
    /// `session_key` 是终端的会话标识（`TerminalSession::key`，SSH 终端经 `LC_TERMLET_SESSION`、WSL 经 `WSLENV`
    /// 传进 shell 的环境变量）；不合法时不标出这个终端自己的进程。
    pub fn new(session_key: &str) -> Self {
        Self {
            inner: std::sync::Arc::new(process::Inner::new(session_key)),
        }
    }
}

impl std::fmt::Debug for HostTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostTools")
            .field("session_key", &self.inner.key)
            .finish_non_exhaustive()
    }
}
