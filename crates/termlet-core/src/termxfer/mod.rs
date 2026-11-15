//! 终端中的文件传输：ZModem（服务器上的 `rz` / `sz`，lrzsz）与 trzsz（`trz` / `tsz`，`trz -d` 上传目录）。
//!
//! 远端程序在终端里输出起始序列（ZModem 的 ZRQINIT / ZRINIT 帧头，trzsz 的 `::TRZSZ:TRANSFER:` 触发行），
//! 包装终端字节接口的 [`Interceptor`] 在把输出交给终端之前检测到它：之后这个终端的输出交给协议，用户的输入
//! 除取消（Ctrl+C）外不再发给远端；界面请用户选择要上传的文件或保存的位置，核心直接读写本机文件。
//! 传输结束后，协议没读走的输出（远端程序退出后的提示符）与之后的输出照常显示。
//!
//! 适用于任何 [`TerminalIo`](termlet_io::TerminalIo)：SSH、本地终端、WSL、Telnet、串口。期间传输一侧收到
//! [`TerminalInput::set_binary`](termlet_io::TerminalInput::set_binary)，数据原样收发（不做编码转换）。
//!
//! ZModem 按 go-zmodem（MIT）实现，trzsz 按 trzsz-go（MIT）实现，许可文本见本目录的
//! `LICENSE-go-zmodem.txt`、`LICENSE-trzsz-go.txt`（随安装包附上）。
//!
//! # 用法（界面）
//!
//! - 每个终端 Tab 建一个 [`Interceptor`]（[`InterceptOptions::enabled`] 取设置
//!   [`TerminalSettings::file_transfer`](crate::model::TerminalSettings::file_transfer)，设置改变时
//!   [`Interceptor::set_enabled`]）。每次连接、重连得到的 `TerminalIo` 先经 [`Interceptor::wrap`] 再交给终端引擎
//!   （`TerminalEngine::new` / `attach`），并说明它是什么样的（[`LinkInfo`]：本地终端与 WSL 经过 ConPTY，
//!   SSH 接入了 tmux 时 `tmux`）。直接写远端的地方（sudo 密码填入等）也用包装后的输入。
//! - [`Interceptor::events`] 送出 [`TransferInfo`] 快照：[`TransferState::Waiting`] 时显示提示栏（下载：保存到上次的
//!   目录 / 选择目录 / 取消；上传：选择文件，`directory` 时可以选目录 / 取消），之后是进度（当前文件、已传 / 大小、
//!   第几个 / 共几个、速度）与结果（完成时的 `saved` / `uploaded` 与 `skipped`，失败的 [`XferError`]）。
//! - 回答用 [`Interceptor::answer`]（[`Answer::SaveTo`]、[`Answer::Send`]、[`Answer::Refuse`]）；把文件拖到终端上时，
//!   [`Interceptor::accepts_drop`] 有值就以 `Answer::Send` 作答。取消用 [`Interceptor::cancel`]，终端中按 Ctrl+C 同样取消。
//!   5 分钟没有回答视为取消。
//! - 传输在自己的线程上进行，转发在核心的运行时上；界面线程只接收事件，调用的方法都不阻塞。

mod detect;
mod files;
mod info;
mod interceptor;
mod pipe;
mod progress;
mod transfer;
mod trzsz;
mod zmodem;

pub use info::{
    Answer, LocalErrorKind, Protocol, TransferId, TransferInfo, TransferState, XferError,
};
pub use interceptor::{InterceptOptions, Interceptor, LinkInfo};

#[cfg(test)]
mod tests;
