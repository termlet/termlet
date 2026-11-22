//! 同步地把请求交给 Termlet（`termlet mcp` 用：每个工具调用一个线程、一条到 Termlet 的连接）。应用内的 HTTP 服务
//! 在异步运行时上直接处理请求，不经过这里，只用 [`crate::tools::request`] 与 [`crate::tools::outcome`]。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

use termlet_proto::client::Abort;
use termlet_proto::{ExitCode, Request, Response};

/// 连不上 Termlet、通信失败的原因（说明已按语言写好；退出码给命令行工具用）。
#[derive(Debug)]
pub struct Failure {
    pub exit: ExitCode,
    pub code: String,
    pub message: String,
}

/// 一个进行中的工具调用的取消：连上 Termlet 前取消的不再发请求，之后取消的关闭连接。
#[derive(Default)]
pub struct AbortSlot {
    aborted: AtomicBool,
    handle: Mutex<Option<Abort>>,
}

impl AbortSlot {
    /// 连上 Termlet 后交给它中止连接的句柄；已经取消时返回假（不要再发请求）。
    pub fn arm(&self, handle: Option<Abort>) -> bool {
        let mut slot = lock(&self.handle);
        if self.aborted.load(Ordering::SeqCst) {
            return false;
        }
        *slot = handle;
        true
    }

    /// 取消：关闭到 Termlet 的连接（有的话）。
    pub fn abort(&self) {
        let handle = {
            let mut slot = lock(&self.handle);
            self.aborted.store(true, Ordering::SeqCst);
            slot.take()
        };
        if let Some(handle) = handle {
            handle.abort();
        }
    }

    pub fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::SeqCst)
    }
}

/// 怎么把请求交给 Termlet（测试中换成假的）。
pub trait Backend: Send + Sync {
    /// 连上 Termlet 发一个请求，等它回答；`slot` 被取消时中止。连不上、通信失败时为 `Err`（说明已按语言写好）。
    fn call(&self, request: Request, slot: &AbortSlot) -> Result<Response, Failure>;
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}
