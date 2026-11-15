//! 主机工具面板：窗口右侧的停靠栏，跟随显示着并最近获得焦点的终端，显示它所连主机的资源概览，并显示、操作进程、
//! 服务、Docker、网络连接，各页之间可以互相跳转（进程所属的服务与容器、进程与端口的连接、容器内的进程），都能用键盘
//! 操作（[`keys`]）。服务与容器的日志在中间区域的 Tab 中查看（[`LogPanel`]）。概览可以弹出成置顶的小窗
//! （[`popout`]），固定一台主机。

mod audit;
mod chart;
mod dialogs;
mod docker;
mod docker_cleanup;
mod errors;
pub(crate) mod keys;
mod log_model;
pub mod log_panel;
mod model;
mod network;
mod overview;
pub mod panel;
mod popout;
#[cfg(test)]
mod popout_tests;
mod processes;
mod services;
mod shared;
pub mod target;
#[cfg(test)]
mod tests;

pub use log_panel::{LogPanel, LogPanelEvent, LogTab};
pub use model::Page as HostToolsPage;
pub use panel::{HostToolsEvent, HostToolsPanel};
pub(crate) use popout::close_all as close_overview_windows;
pub use target::ToolsTarget;

/// 开着的主机概览小窗（测试看）。
#[cfg(test)]
pub(crate) fn overview_windows(cx: &gpui_kit::App) -> Vec<gpui_kit::AnyWindowHandle> {
    popout::open_windows(cx)
        .into_iter()
        .map(|(_, handle, _)| handle)
        .collect()
}

/// 测试用：把 `terminal` 当作连着主机 `host` 的终端（重连中，不读取），不经主机工具面板弹出它的概览小窗（本地终端
/// 也可以），弹出它的窗口是 `window`。
#[cfg(test)]
pub(crate) fn pop_out_for_test(
    terminal: &gpui_kit::Entity<crate::terminal_panel::TerminalPanel>,
    host: &str,
    window: &gpui_kit::Window,
    cx: &mut gpui_kit::App,
) {
    let target = ToolsTarget {
        terminal: terminal.downgrade(),
        id: terminal.entity_id(),
        name: "test".into(),
        login: None,
        state: target::ToolsState::Reconnecting,
        tools: terminal.read(cx).host_tools(),
        ssh_host: Some(termlet_core::id::HostId::from_raw(host)),
    };
    popout::pop_out(&target, shared::new_cache(), window, cx);
}

/// 注册面板的快捷键（`crate::init` 中调用）。
pub fn init(cx: &mut gpui_kit::App) {
    keys::init(cx);
}
