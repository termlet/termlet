//! 排查问题时的调试输出：设了环境变量 `TERMLET_LOG`（任意非空值）时写到标准错误，没设时什么都不做。
//!
//! `termlet` 命令不写 Termlet 的日志文件（那是应用的；应用这边的记录在它的日志与审计中），也不引入日志库
//! （保持小、启动快）。只写连接、拉起应用、请求的种类与结果，不写命令内容与输出。

use std::sync::OnceLock;

/// 是否输出调试信息（读一次环境变量）。
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("TERMLET_LOG").is_some_and(|v| !v.is_empty()))
}

/// 写一行调试信息到标准错误：`[termlet] …`。
macro_rules! debug {
    ($($arg:tt)*) => {
        if $crate::debug::enabled() {
            eprintln!("[termlet] {}", format_args!($($arg)*));
        }
    };
}

pub(crate) use debug;
