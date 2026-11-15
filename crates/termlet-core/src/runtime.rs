//! 核心的异步运行时：网络（SSH、SFTP、端口转发、WebDAV）与文件 I/O 都在这里执行。
//!
//! 全进程一份 tokio 多线程运行时。界面线程不进入它，只等待它的任务（`JoinHandle` 与 `async-channel`
//! 与执行器无关，在 GPUI 的任务里可以直接 `await`）。

use std::future::Future;
use std::sync::LazyLock;

use tokio::runtime::{Builder, Runtime};
use tokio::task::JoinHandle;

static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
    Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("termlet-io")
        .enable_all()
        .build()
        .expect("无法创建异步运行时")
});

/// 全局的运行时。
pub fn runtime() -> &'static Runtime {
    &RUNTIME
}

/// 在核心的运行时上执行一个任务。
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    RUNTIME.spawn(future)
}
