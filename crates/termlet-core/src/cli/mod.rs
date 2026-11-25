//! 命令行工具（`termlet`）的服务：监听本机通道，替命令行工具列出主机、执行命令、传输文件。
//! 协议见 `termlet-proto`。MCP 的 HTTP 接入（[`http`]）也挂在这里，与本机通道共用下面的检查与审计。
//!
//! # 安全模型
//!
//! - **默认关闭**：本机设置 `LocalSettings::cli.enabled` 为假时一律返回 `cli_disabled`（执行与传输仍记入审计日志，
//!   便于发现有程序在尝试调用）。
//! - **按主机授权**：只有开启了"允许命令行访问"的 SSH 主机可以使用（`Host::cli.allowed`）；主机可以设 CLI 专用凭据
//!   （服务器上的低权限账号），设了时命令行用它登录，并且不能使用 `--terminal`（终端以用户本人的身份运行，会绕过它）。
//!   命令行不使用目标主机的 TOTP（否则能在无人值守时登录需要验证码的主机）；指纹、密码、验证码都不能询问。
//! - **危险命令**：命中 `guard` 的规则时，设置为"询问"则请界面批准（[`CliEvent::Approval`]，最多等 [`APPROVAL_TIMEOUT`]），
//!   为"拦截"或没有窗口可以询问时拦截。
//! - **锁定**：应用锁定时（[`CliService::set_app_locked`]）一律返回 `app_locked`，等待批准期间被锁定时立即返回；
//!   保险库没有解锁时需要凭据的请求返回 `vault_locked` 等（退出码 6）。
//! - **审计**：每次执行、传输都写入审计日志（含被拦截、失败的调用），见 [`crate::audit`]；运行日志中不记命令内容。
//! - **通道**：Windows 命名管道只允许当前用户与 SYSTEM，属主为当前用户，拒绝远程客户端，只建第一个实例（名字被占用时
//!   启动失败而不是共用）；Unix 套接字在 0700 的 `run/` 中。由操作系统做用户隔离，不需要 token。
//! - **MCP 的 HTTP 接入**：默认关；与外部 CLI 的开关都开着才在 127.0.0.1 上监听（可以改为同时接受局域网的私有网段，
//!   那边只有列出主机与执行命令，用单独的令牌），校验 Host、拒绝带 Origin 的请求、要 Bearer 令牌（TCP 回环没有用户
//!   隔离）。见 [`http`]。
//!
//! # 界面怎么接
//!
//! 1. **启动**：拿到单实例锁、打开存储后，在任意线程调用 [`CliService::start`]（不阻塞，监听在核心的运行时上）。
//!    监听失败只影响命令行：返回错误（写进日志），并在 `run/cli.unavailable` 中写下原因，`termlet` 据此说明。
//!    服务一直运行，不随"外部命令行"开关启停（开关在每次请求时读取）；保险库锁定时也照常启动。
//! 2. **事件**：`start` 同时返回一个 `async_channel::Receiver<CliEvent>`，界面在 GPUI 的任务里循环接收：
//!    - [`CliEvent::Approval`]：弹出批准对话框（主机、命令、调用方、命中的规则、剩余时间），用户选择后调用
//!      [`ApprovalRequest::answer`]；同时等待 [`ApprovalRequest::done`]（超时、命令行退出、被锁定时完成），完成时关闭对话框。
//!      没有窗口可以询问（只在后台服务）时直接丢弃请求，按拦截处理。可以闪烁任务栏提醒用户。
//!    - [`CliEvent::RunInTerminal`]：找到这台主机最新打开、已连接的 SSH 终端，用 [`crate::termrun::TermRun`] 执行，
//!      把结果交给 [`TerminalRequest::respond`]（见 `termrun` 的说明）；[`TerminalRequest::canceled`] 交给 `execute`。
//!      没有打开的终端时回答 `TerminalError::NotOpen`（直接丢弃请求也按没有打开的终端处理）。
//!    - [`CliEvent::Activate`]：第二个 Termlet 进程交来的启动参数，打开或激活主窗口、处理其中的链接。
//!    - [`CliEvent::Audited`]：写入了一条审计记录，审计对话框打开时刷新；不处理也可以（队列满时丢弃）。
//! 3. **锁定**：应用锁定、解锁时调用 [`CliService::set_app_locked`]。
//! 4. **审计**：审计对话框直接用 [`crate::audit::AuditLog`]（与服务共用同一份），在后台执行器上调用。
//! 5. **设置**：外部命令行的开关在 `LocalSettings::cli.enabled`；危险命令规则与"询问 / 拦截"在 `Settings::guard`；
//!    规则测试用 `guard::Guard::from_settings` 加 `check`。主机的访问与专用凭据在主机对话框中（`Host::cli`）。
//! 6. **停止**：退出前调用 [`CliService::stop`]（或丢弃它）：不再接受连接，进行中的请求被取消。
//! 7. **MCP 的 HTTP 接入**：本机设置 `cli.http` 保存后服务自己对照设置启停、换端口；界面显示实际状态
//!    （[`CliService::http_status`]、[`CliService::subscribe_http`]），端口被占用后可以 [`CliService::refresh_http`] 重试；
//!    令牌（本机的、局域网的）用 [`CliService::http_token`] 显示、[`CliService::regenerate_http_token`] 重新生成，
//!    这台电脑在局域网中的地址用 [`http::lan::addresses`] 列出；界面语言变了时调用
//!    [`CliService::set_language`]。无窗口模式按 [`CliService::last_request`] 计算空闲。
//! 8. **第二个实例**：拿不到单实例锁时，用 `proto::client::Client` 连上已有实例、发送 `Request::Activate`（带上启动参数）
//!    后退出。`termlet` 连不上时以 `Termlet --headless` 拉起应用（不开窗口），等待它的命令行服务最多 10 秒；设置中关闭了
//!    自动启动（`LocalSettings::cli.auto_start`，应用写进 `proto::endpoint::LaunchConfig`）时返回 `app_not_running`（退出码 7）。
//! 9. **Agent Skill 与 PATH**：安装位置与状态见 `proto::skill`（`targets`、`install`、`remove`），
//!    命令行工具的目录与 PATH 见 [`crate::userpath`]（应用启动时调用 `userpath::follow`）。
//!
//! 所有 I/O 在核心的 tokio 运行时上，界面线程不阻塞；事件中的回答用 `tokio::sync` 的通道，在任何执行器上都能用。

pub(crate) mod connect;
pub(crate) mod error;
mod events;
mod handler;
pub mod http;
mod listen;
pub(crate) mod service;
#[cfg(test)]
mod tests;

pub use events::{ApprovalRequest, CliEvent, TerminalRequest};
pub use http::{HttpError, HttpStatus, TokenKind, TokenView};
pub use service::{APPROVAL_TIMEOUT, CliConfig, CliService};
