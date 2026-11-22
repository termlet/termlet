//! Termlet 的 MCP 服务中与传输无关的部分：两代协议的分发（[`protocol`]）、工具的定义、参数检查与结果的写法
//! （[`tools`]）、给人与 AI 看的中英文文本（[`text`]）。
//!
//! 两种传输共用这里：
//!
//! - **stdio**：`termlet mcp`（`termlet-cli` 的 `mcp` 模块），由 AI 工具启动，逐行读写；每个工具调用经 [`Backend`]
//!   连到 Termlet、发与命令行工具相同的请求。
//! - **HTTP**：Termlet 应用自己监听（`termlet-core` 的 `cli::http`），在核心的运行时上直接处理请求。
//!
//! 外部 CLI 的开关、按主机授权、CLI 专用凭据、危险命令规则与批准、应用锁与保险库、审计日志都在应用端检查，两种传输
//! 走同一套；这里不绕过任何一道，也不另做一套。业务上的失败（外部 CLI 已关闭、命令被拦截等）是工具执行错误
//! （`isError: true`，文本中有错误码与说明，AI 据此处理）；未知的工具、参数不是对象等是 JSON-RPC 错误。

mod backend;
pub mod protocol;
pub mod text;
pub mod tools;

pub use backend::{AbortSlot, Backend, Failure};
pub use protocol::{
    Era, HEADER_MISMATCH, INVALID_PARAMS, INVALID_REQUEST, MAX_MESSAGE, METHOD_NOT_FOUND, Message,
    PARSE_ERROR, Session, Step, ToolCall, UNSUPPORTED_PROTOCOL_VERSION, client_name,
};
