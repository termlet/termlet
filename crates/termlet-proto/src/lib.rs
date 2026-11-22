//! `termlet` 命令与 Termlet 应用之间的协议：请求、响应、错误码与退出码，消息帧，本机通道的位置与客户端。
//!
//! 命令行工具只依赖这个 crate，不链接核心的实现，因此体积小、启动快。
//!
//! # 协议
//!
//! - **通道**：Windows 上是命名管道（名字含数据目录的哈希，只有当前用户与 SYSTEM 能访问），其他平台是数据目录
//!   `run/` 中的 Unix 套接字（`run/` 为 0700）。见 [`endpoint`]。
//! - **消息帧**：4 字节大端长度 + JSON（UTF-8）。见 [`frame`]。
//! - **握手**：连接后客户端先发 [`ClientHello`]（协议版本、语言），服务端回答 [`Response::Welcome`]（协商后的版本）
//!   或 [`Response::Error`]（[`ErrorCode::ProtocolMismatch`]）。之后每发一个 [`Request`] 收一个 [`Response`]，
//!   一条连接上可以依次发多个请求。
//! - **取消**：客户端在请求进行中断开（`termlet` 被结束）时，服务端中止这次请求（远程命令收到 SIGTERM）。
//! - **版本**：只加新的请求、响应变体或带默认值的字段时不升版本；双方都忽略不认识的字段。

pub mod caller;
pub mod endpoint;
mod error;
pub mod frame;
pub mod mcp;
mod message;
pub mod skill;

pub mod client;

pub use error::{ErrorCode, ExitCode};
pub use message::{
    ClientHello, ErrorBody, ExecRequest, ExecResponse, ExpiringCertificate, HostSummary, Lang,
    LaunchFile, Request, Response, TRANSFER_SCP, TRANSFER_SFTP, TransferRequest, TransferResponse,
    Welcome,
};

/// 协议版本：客户端与服务端都支持的最高版本。
pub const PROTOCOL_VERSION: u32 = 1;
/// 还支持的最低版本。
pub const MIN_PROTOCOL_VERSION: u32 = 1;

/// `exec` 时标准输出、标准错误各自最多返回的字节数，超出部分截断（`truncated`）。
pub const OUTPUT_LIMIT: usize = 1 << 20;
/// `exec --stdin` 读取的命令（脚本）的上限：超出时不执行（截断后的脚本可能只执行了一半）。
pub const MAX_STDIN_COMMAND: usize = 1 << 20;
/// 一条请求的上限（帧的长度）。命令中的控制字符在 JSON 中会变长，因此比 [`MAX_STDIN_COMMAND`] 大。
pub const MAX_REQUEST: usize = 2 << 20;
/// 一条响应的上限：两路输出各 1MB，JSON 转义后最多约 6 倍。
pub const MAX_RESPONSE: usize = 16 << 20;

/// 协商版本：对方支持的最高版本为 `peer` 时双方使用的版本；对方太旧时为 `None`。
pub fn negotiate(peer: u32) -> Option<u32> {
    (peer >= MIN_PROTOCOL_VERSION).then(|| peer.min(PROTOCOL_VERSION))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation_picks_the_lower_version() {
        assert_eq!(negotiate(PROTOCOL_VERSION), Some(PROTOCOL_VERSION));
        assert_eq!(negotiate(PROTOCOL_VERSION + 5), Some(PROTOCOL_VERSION));
        assert_eq!(negotiate(0), None);
    }
}
