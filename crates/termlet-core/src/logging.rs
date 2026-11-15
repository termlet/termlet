//! 日志：写入 `{数据目录}/logs/termlet.<日期>.log`，按天滚动，保留最近 14 天；调试构建同时输出到标准错误。
//!
//! 详细程度有三档（[`LogLevel`]，设置的“关于 → 诊断”中可选，立即生效）：普通（info）、详细（Termlet 自己的
//! 代码为 debug）、最详细（Termlet 为 trace，SSH 等依赖为 debug）。也可以在启动时指定，指定后设置中的选项不起作用：
//! - 环境变量 `TERMLET_LOG`：`tracing_subscriber::EnvFilter` 的语法（例如 `debug`、`info,termlet_core::ssh=trace`）；
//! - 启动参数 `--log-level <级别或过滤规则>`（`normal` / `detailed` / `full`，或与 `TERMLET_LOG` 相同的写法）、
//!   `--verbose`（等于 `detailed`）。环境变量优先于启动参数。
//!
//! 日志中不出现密码、私钥、命令内容等敏感数据。

use std::path::Path;
use std::sync::{Mutex, OnceLock};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Registry, fmt, reload};

pub use crate::model::LogLevel;

/// Termlet 自己的 crate（详细、最详细两档对它们打开 debug / trace）。
const OWN_CRATES: &[&str] = &[
    "termlet_core",
    "termlet_desktop",
    "termlet_terminal",
    "termlet_io",
    "termlet_proto",
];
/// 最详细时另外打开 debug 的依赖：SSH、SFTP 与 HTTP（更新、WebDAV）。界面渲染的依赖每帧都写，不打开。
const FULL_DEPENDENCIES: &[&str] = &["russh", "russh_sftp", "reqwest", "rustls"];

impl LogLevel {
    /// 这一档对应的过滤规则（`EnvFilter` 的写法）。
    pub fn filter(self) -> String {
        let own = |level: &str| {
            OWN_CRATES
                .iter()
                .map(|c| format!("{c}={level}"))
                .collect::<Vec<_>>()
        };
        let mut parts = vec!["info".to_string()];
        match self {
            LogLevel::Normal => {}
            LogLevel::Detailed => parts.extend(own("debug")),
            LogLevel::Full => {
                parts.extend(own("trace"));
                parts.extend(FULL_DEPENDENCIES.iter().map(|c| format!("{c}=debug")));
            }
        }
        parts.join(",")
    }

    /// 启动参数中的写法：`normal` / `detailed` / `full`（也认 `info`、`debug`、`trace`）。
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "normal" | "info" => Some(LogLevel::Normal),
            "detailed" | "debug" | "verbose" => Some(LogLevel::Detailed),
            "full" | "trace" => Some(LogLevel::Full),
            _ => None,
        }
    }
}

/// 启动时指定的详细程度（环境变量或启动参数）：设置中的选项不再起作用。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogOverride {
    /// 环境变量 `TERMLET_LOG` 中的过滤规则
    Env(String),
    /// 启动参数 `--log-level` / `--verbose`
    Arg(String),
}

impl LogOverride {
    /// 过滤规则：档位名换成它的规则，其余原样（`EnvFilter` 的写法）。
    fn filter(&self) -> String {
        let text = match self {
            LogOverride::Env(text) | LogOverride::Arg(text) => text,
        };
        LogLevel::parse(text).map_or_else(|| text.trim().to_string(), LogLevel::filter)
    }

    /// 显示用：在哪里指定的、写的是什么。
    pub fn describe(&self) -> String {
        match self {
            LogOverride::Env(text) => format!("TERMLET_LOG={text}"),
            LogOverride::Arg(text) => format!("--log-level {text}"),
        }
    }
}

/// 持有期间日志在后台线程写入文件；丢弃时把缓冲的日志写完。
pub struct LogGuard {
    _guard: WorkerGuard,
}

/// 运行中切换详细程度用。
struct Control {
    handle: reload::Handle<EnvFilter, Registry>,
    overridden: Option<LogOverride>,
    current: Mutex<String>,
}

static CONTROL: OnceLock<Control> = OnceLock::new();

/// 启动时指定的详细程度：`TERMLET_LOG` 优先，其次是启动参数 `--log-level` / `--verbose`（`arg`）。
pub fn startup_override(arg: Option<&str>) -> Option<LogOverride> {
    if let Ok(env) = std::env::var("TERMLET_LOG")
        && !env.trim().is_empty()
    {
        return Some(LogOverride::Env(env));
    }
    arg.filter(|a| !a.trim().is_empty())
        .map(|a| LogOverride::Arg(a.to_string()))
}

/// 初始化全局日志：先按 `overridden`（没有时为普通），打开数据之后再由 [`apply_setting`] 换成设置中的档位。
/// 同一进程只能调用一次（再次调用返回 `None`，日志照旧写到第一次的位置）。
pub fn init(logs_dir: &Path, overridden: Option<LogOverride>) -> Option<LogGuard> {
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("termlet")
        .filename_suffix("log")
        .max_log_files(14)
        .build(logs_dir)
        .ok()?;
    let (file, guard) = tracing_appender::non_blocking(appender);
    let wanted = overridden
        .as_ref()
        .map_or_else(|| LogLevel::Normal.filter(), LogOverride::filter);
    let (filter, rejected) = match EnvFilter::try_new(&wanted) {
        Ok(filter) => (filter, None),
        Err(e) => (EnvFilter::new(LogLevel::Normal.filter()), Some(e)),
    };
    let (filter, handle) = reload::Layer::new(filter);
    let file_layer = fmt::layer()
        .with_ansi(false)
        .with_target(true)
        .with_thread_names(true)
        .with_writer(file);
    let registry = tracing_subscriber::registry().with(filter).with(file_layer);
    let result = if cfg!(debug_assertions) {
        registry
            .with(fmt::layer().with_target(true).with_writer(std::io::stderr))
            .try_init()
    } else {
        registry.try_init()
    };
    result.ok()?;
    let current = if rejected.is_some() {
        LogLevel::Normal.filter()
    } else {
        wanted
    };
    if let Some(e) = rejected {
        tracing::warn!(
            source = overridden
                .as_ref()
                .map(LogOverride::describe)
                .unwrap_or_default(),
            "日志的过滤规则写得不对，改用普通：{e}"
        );
    }
    let _ = CONTROL.set(Control {
        handle,
        overridden,
        current: Mutex::new(current.clone()),
    });
    tracing::info!(filter = %current, "日志已开始");
    Some(LogGuard { _guard: guard })
}

/// 按设置中的档位切换（启动时打开数据后、设置改了时调用）；启动时已指定了详细程度时不变。
pub fn apply_setting(level: LogLevel) {
    let Some(control) = CONTROL.get() else {
        return;
    };
    if control.overridden.is_some() {
        return;
    }
    let filter = level.filter();
    let mut current = control.current.lock().unwrap_or_else(|e| e.into_inner());
    if *current == filter {
        return;
    }
    match control.handle.reload(EnvFilter::new(&filter)) {
        Ok(()) => {
            tracing::info!(level = ?level, filter = %filter, "日志的详细程度已切换");
            *current = filter;
        }
        Err(e) => tracing::warn!("切换日志的详细程度失败：{e}"),
    }
}

/// 启动时指定的详细程度（设置页据此说明选项为什么不起作用）。
pub fn overridden() -> Option<LogOverride> {
    CONTROL.get().and_then(|c| c.overridden.clone())
}

/// 正在使用的过滤规则（诊断信息中列出）。
pub fn current_filter() -> Option<String> {
    CONTROL
        .get()
        .map(|c| c.current.lock().unwrap_or_else(|e| e.into_inner()).clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_map_to_filters_that_parse() {
        assert_eq!(LogLevel::Normal.filter(), "info");
        let detailed = LogLevel::Detailed.filter();
        assert!(detailed.contains("termlet_core=debug"));
        assert!(detailed.contains("termlet_desktop=debug"));
        assert!(!detailed.contains("russh"));
        let full = LogLevel::Full.filter();
        assert!(full.contains("termlet_core=trace"));
        assert!(full.contains("russh=debug"));
        for level in [LogLevel::Normal, LogLevel::Detailed, LogLevel::Full] {
            EnvFilter::try_new(level.filter()).expect("档位的规则能解析");
        }
    }

    #[test]
    fn startup_values_accept_level_names_and_raw_filters() {
        assert_eq!(LogLevel::parse("Detailed"), Some(LogLevel::Detailed));
        assert_eq!(LogLevel::parse("trace"), Some(LogLevel::Full));
        assert_eq!(LogLevel::parse("info,russh=trace"), None);
        let named = LogOverride::Arg("debug".into());
        assert_eq!(named.filter(), LogLevel::Detailed.filter());
        let raw = LogOverride::Env("info,termlet_core::ssh=trace".into());
        assert_eq!(raw.filter(), "info,termlet_core::ssh=trace");
        assert_eq!(raw.describe(), "TERMLET_LOG=info,termlet_core::ssh=trace");
    }
}
