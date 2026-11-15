//! 端口转发：本地转发（`ssh -L`）、动态转发（`ssh -D`，本机 SOCKS4 / 4a / 5 代理）、远程转发（`ssh -R`）。
//!
//! - 本地转发：在本机监听 `bind_host:bind_port`，每个接入的连接经 SSH 打开 direct-tcpip 通道到
//!   `target_host:target_port`（目标地址由服务器解析和连接）。
//! - 动态转发：在本机监听处开 SOCKS 服务（无需认证），目标由客户端在每个连接中指定，同样经 direct-tcpip 通道转出；
//!   域名由服务器解析。
//! - 远程转发：请服务器监听 `bind_host:bind_port`（tcpip-forward），服务器上每个接入的连接经 SSH 转回本机，
//!   再连接 `target_host:target_port`（目标由本机解析和连接）。服务器只在 sshd 的 GatewayPorts 允许时
//!   才会监听回环地址以外的地址。
//!
//! # 用法（界面）
//!
//! - 规则是存储中的 [`Forward`] 条目：增删改经 `Store`（保存时校验地址、检查与已有规则的监听冲突，
//!   冲突的规则用 [`find_conflict`] 找到）。修改或删除运行中的规则时管理器自动停止它，界面按需重新启动。
//! - 管理器：应用启动时建一个 [`ForwardManager`]（应用级，与 `SftpManager` 一样放在 `AppState`），各窗口共用；
//!   转发属于启动它的窗口（`owner`），窗口关闭时调用 [`ForwardManager::close_owner`]，退出时
//!   [`ForwardManager::close_all`]。
//! - 启动、停止：[`ForwardManager::start`]（从终端 Tab 启动时把终端的连接组放进 [`StartRequest::share_group`]，
//!   共用连接、不再认证；[`StartRequest::until_disconnect`] 的随连接结束而停，不重连）、[`ForwardManager::stop`]。启动时
//!   可能要确认指纹、输入密码与验证码（经 `prompter`）。
//!   勾选了自动启动的规则：第一个窗口打开、保险库解锁后，对 [`ForwardManager::auto_start_rules`] 逐条启动。
//! - 状态：[`ForwardManager::watch`] 推送全部规则的状态快照（[`ForwardSnapshot`]，没有列出的规则是停止的），
//!   [`ForwardManager::snapshot`] 随时取。失败的原因在 [`ForwardState::Failed`]，运行中连接目标失败的原因在
//!   [`ForwardStatus::last_error`]。
//! - 检测服务器端口：[`listening_ports`]（用终端的连接执行），选中后用 [`ListeningPort::target_host`] 填目标。
//!
//! 管理器的异步方法内部在核心的运行时上执行，返回的 future 是 `Send`，可以在 GPUI 的任务里直接 `await`；
//! `watch` 的接收端与执行器无关。[`listening_ports`] 要在核心的运行时上执行（`crate::runtime::spawn`）。
//!
//! # 通过本机代理
//!
//! 与规则无关、属于一个终端的连接的远程转发（[`ProxyShare`]）：把本机的 HTTP 代理借给服务器，服务器上只监听
//! `127.0.0.1`，随连接断开而停止。本机代理的地址见 [`local_proxy`]（设置中填写的，没有时用系统代理），
//! 服务器上怎样使用它的命令片段见 [`proxy_snippets`]。

mod detect;
mod error;
mod listen;
mod manager;
mod pipe;
mod runner;
mod share;
mod snippets;
mod socks;
mod system_proxy;
#[cfg(test)]
mod tests;

pub use detect::{ListeningPort, PORTS_SCRIPT, listening_ports, parse_ports, ports_command};
pub use error::{ForwardError, SocksError, TargetError};
pub use manager::{
    ForwardManager, ForwardOptions, ForwardSnapshot, ForwardState, ForwardStatus, StartRequest,
};
pub use share::{ProxyAddress, ProxyShare, SHARE_BIND, ShareError, ShareStatus, StopReason};
pub use snippets::{ProxySnippet, SnippetKind, proxy_snippet, proxy_snippets, proxy_url};
pub use system_proxy::{ProxySource, local_proxy, system_proxy};

use crate::id::ForwardId;
use crate::model::Forward;
use crate::store::{Record, Store};

/// 与 `rule` 的监听地址冲突的已有规则（`except` 是正在编辑的规则自己）；`rule` 本身不合法时为 `None`。
/// 存储保存时只报告 `bindPort` / `conflict`，界面用它说明被哪条规则占用。
pub fn find_conflict(
    store: &Store,
    rule: &Forward,
    except: Option<&ForwardId>,
) -> Option<Record<Forward>> {
    let mut rule = rule.clone();
    rule.normalize().ok()?;
    store
        .list::<Forward>()
        .into_iter()
        .find(|other| Some(&other.id) != except && rule.conflicts_with(&other.doc))
}
