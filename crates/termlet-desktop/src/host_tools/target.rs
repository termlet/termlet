//! 主机工具跟随的终端：工作区在跟随的终端变化（见 `Workspace::command_target`）、终端连接状态变化时更新它，面板据此
//! 读取与操作。

use gpui_kit::{EntityId, SharedString, WeakEntity};
use termlet_core::hosttools::{HostTools, Target};
use termlet_core::id::HostId;

use crate::terminal_panel::TerminalPanel;

/// 主机工具跟随的终端。
#[derive(Clone)]
pub struct ToolsTarget {
    pub terminal: WeakEntity<TerminalPanel>,
    /// 终端面板的实体 ID：换了终端时各页重建（搜索、排序、选中都重置）
    pub id: EntityId,
    /// 终端连的主机（保存的主机名、`用户@地址`、WSL 发行版；本地终端为 Tab 标题）：面板标题与审计记录用，
    /// 不是 Tab 标题（从面板开出的“日志 · nginx”等 Tab 也记成原来的主机）
    pub name: SharedString,
    /// 登录的用户与地址（`用户@地址`，[`TerminalPanel::login_label`]）：写操作的确认框写明主机
    pub login: Option<SharedString>,
    pub state: ToolsState,
    /// 这个终端的主机工具（标出本终端的 shell 与子进程、记着上次的进程读数）
    pub tools: HostTools,
    /// 保存的 SSH 主机（网络页“转发到本机…”只对它提供）
    pub ssh_host: Option<HostId>,
}

/// 写操作的确认框中写明的主机：名称（与面板标题相同）与登录的用户和地址。
#[derive(Clone, Debug, PartialEq)]
pub struct HostLabel {
    pub name: SharedString,
    pub login: Option<SharedString>,
}

/// 终端的情况。
#[derive(Clone)]
pub enum ToolsState {
    /// 本地终端、Telnet、串口：没有主机工具
    Unsupported,
    /// 只用一个通道的主机（堡垒机）、外部程序调起的临时连接：不另开通道
    SingleChannel,
    Connecting,
    /// 断线重连中：显示上次的数据，不读取
    Reconnecting,
    /// 会话已结束、连接失败
    Ended,
    /// 连接着：每次读取、操作时传入这条连接
    Connected(Target),
}

impl ToolsState {
    /// 当前可以读取、操作时的连接。
    pub fn target(&self) -> Option<&Target> {
        match self {
            ToolsState::Connected(target) => Some(target),
            _ => None,
        }
    }
}
