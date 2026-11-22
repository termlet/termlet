//! SSH：连接（直连、代理、代理命令、多级跳板）、主机指纹、认证、终端会话、X11 转发、连接复用、断线重连。

mod algorithms;
mod auth;
mod connection;
pub(crate) mod dial;
mod error;
mod exec;
mod handler;
pub mod host_ca;
pub mod known_hosts;
pub mod plan;
mod pool;
pub mod prompt;
pub mod proxy_command;
mod shell;
mod terminal;
pub mod transport;
pub mod x11;

pub use connection::{ConnectOptions, Connection, Health, STALL_AFTER, connect};
pub use error::SshError;
pub(crate) use exec::{ExecFailure, exec_until};
pub use exec::{ExecResult, exec, exec_with_input};
pub(crate) use handler::ForwardedChannel;
pub use known_hosts::{HostKeyStatus, KnownHostEntry, KnownHosts};
pub use plan::{
    CertMaterial, ConnectPlan, Hop, HostDraft, SecretEdit, SecretEdits, plan_for_cli,
    plan_for_draft, plan_for_host, plan_for_quick,
};
pub use pool::ConnectionPool;
pub use prompt::{
    Answer, HostKeyPrompt, Prompt, Prompter, ProxyCommandPrompt, no_prompt, remembering,
};
pub use shell::{ShellOptions, open_shell};
pub use terminal::{
    OpenedTerminal, TerminalSession, TerminalSpec, TestReport, TmuxStatus, connect_options,
    kill_tmux, open_terminal, reconnect_delay, test_connection,
};
pub use x11::{X11Notice, X11Options, X11Problem, X11Status};

#[cfg(test)]
pub(crate) mod cert_tests;
#[cfg(test)]
mod proxy_x11_tests;
#[cfg(test)]
mod tests;
