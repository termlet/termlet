//! Termlet 核心。界面（`termlet-desktop`）与命令行服务都经由这里访问数据和远程主机；
//! 本 crate 不依赖 GPUI，可以单独编译和测试。

pub mod applock;
pub mod audit;
pub mod backup;
pub mod batch;
pub mod charset;
pub mod cli;
pub mod cloudsync;
pub mod crash;
pub mod db;
pub mod diag;
mod error;
pub mod folder_menu;
pub mod forward;
pub mod guard;
pub mod history;
pub mod hostfile;
pub mod hosttools;
pub mod id;
pub mod import;
pub mod inherit;
pub mod instance;
pub mod keymap;
pub mod keyvault;
pub mod launch;
pub mod localpty;
pub mod localshell;
pub mod logging;
pub mod model;
pub mod monitor;
pub mod osprobe;
pub mod paste_image;
pub mod paths;
pub mod platform;
pub mod quick;
pub mod rawterm;
pub mod recording;
pub mod remote_config;
pub mod remote_editor;
pub mod runtime;
pub mod schemes;
pub mod secret;
pub mod serial;
pub mod session_log;
pub mod sftp;
pub mod snippet_params;
pub mod snippetfile;
pub mod sort_key;
pub mod ssh;
pub mod sshcert;
pub mod sshkey;
pub mod store;
pub mod sudo;
pub mod telnet;
pub mod termrun;
pub mod termxfer;
#[cfg(test)]
pub(crate) mod testkit;
pub mod time;
pub mod totp;
pub mod ui_scale;
pub mod ui_state;
pub mod update;
pub mod usage;
pub mod userpath;
pub mod vault;
pub mod wsl;
mod yaml;

pub use error::{CoreError, Invalid, Result};
/// 命令行工具与应用之间的协议（界面用其中的客户端把启动参数交给已有实例、用 Agent Skill 的安装）。
pub use termlet_proto as proto;

/// 终端的会话标记（环境变量）：值为终端的会话标识，SSH 终端经 `AcceptEnv LC_*`、WSL 经 `WSLENV` 传进去，
/// 读取当前目录（SFTP 跟随终端目录）时据此找到这个终端的 shell 进程。
pub const SESSION_ENV: &str = "LC_TERMLET_SESSION";

/// 应用版本，取自 workspace 的版本号。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
