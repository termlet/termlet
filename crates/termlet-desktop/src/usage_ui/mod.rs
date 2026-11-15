//! 匿名使用统计的界面（核心在 `termlet_core::usage`，方案见 `docs/rust/使用统计方案.md`）。
//!
//! 默认开，不另外提示（用户 2026-10-29 定）：安装包的许可说明第四节、官网的隐私说明与设置“关于 → 隐私”中写明。
//!
//! - **查看内容**（[`view`]）：今天要发送的 JSON 原文（与实际发出的是同一个函数生成的）、各字段的含义、接收地址与状态。
//! - **设置**：“关于”页的“隐私”一组（`lock_ui::about`），保存后生效；关掉后删除本机的 `usage.json`。
//! - **发送的时机**（[`check`]）：主窗口显示（解锁之后）、每小时（与检查更新共用 `lock_ui` 的定时器）、新的一个 UTC 日
//!   第一次用到（窗口获得焦点、处理命令行工具的请求）、重新打开开关时检查一次；应用锁锁着时不检查。发送在核心的运行时上，
//!   不阻塞界面。

#[cfg(test)]
mod tests;
mod view;

use std::cell::Cell;

use gpui_kit::{App, AppContext as _, Global, Task};
use termlet_core::store::StoreEvent;
use termlet_core::usage::{CheckInput, Destination, ReporterOptions, Route, UsageReporter};

use crate::app_state::AppState;

pub(crate) use view::open_view;

/// 使用统计（全局量，[`install`] 建立）。
pub(crate) struct UsageUi {
    reporter: UsageReporter,
    /// 上一次看到的“发送匿名使用统计”（设置变化时据此判断改没改）
    enabled: Cell<bool>,
    _watch: Task<()>,
}

impl Global for UsageUi {}

/// 建立 [`UsageUi`] 用到的东西（正式运行与测试各一套）。
pub(crate) struct Setup {
    pub reporter: UsageReporter,
    /// 新的一天第一次用到时检查一次（正式运行）
    pub follow_activity: bool,
}

/// 启动时（[`AppState`] 与 `lock_ui` 装好之后）调用：按环境变量定去向，`usage.json` 在数据目录中。
pub(crate) fn install(cx: &mut App) {
    let reporter = UsageReporter::new(&AppState::global(cx).data_dir);
    install_with(
        Setup {
            reporter,
            follow_activity: true,
        },
        cx,
    );
}

/// UI 测试用：不发送（按调试构建处理）。在 `AppState::install_in_memory` 中调用。
pub(crate) fn install_in_memory(cx: &mut App) {
    let reporter = UsageReporter::with(ReporterOptions {
        state_file: AppState::global(cx).data_dir.usage_state(),
        route: Route::DebugBuild,
        remote: None,
        builtin: termlet_core::usage::USAGE_URL.parse().expect("内置地址"),
    });
    install_with(
        Setup {
            reporter,
            follow_activity: false,
        },
        cx,
    );
}

pub(crate) fn install_with(setup: Setup, cx: &mut App) {
    let Setup {
        reporter,
        follow_activity,
    } = setup;
    let enabled = AppState::store(cx).local_settings().usage.enabled;
    if !enabled {
        // 关着时本机不留统计用的记录（例如上次关闭时没删成）
        let reporter = reporter.clone();
        cx.background_spawn(async move { reporter.forget() })
            .detach();
    }
    let watch = if follow_activity {
        let mut changes = termlet_core::usage::subscribe_active();
        cx.spawn(async move |cx| {
            while changes.changed().await.is_ok() {
                cx.update(check);
            }
        })
    } else {
        Task::ready(())
    };
    cx.set_global(UsageUi {
        reporter,
        enabled: Cell::new(enabled),
        _watch: watch,
    });
    let store_model = AppState::global(cx).store.clone();
    cx.subscribe(&store_model, |_, event: &StoreEvent, cx| {
        if matches!(event, StoreEvent::LocalSettingsChanged) {
            on_local_settings_changed(cx);
        }
    })
    .detach();
}

fn ui(cx: &App) -> Option<&UsageUi> {
    cx.try_global::<UsageUi>()
}

/// 发送统计用的（“查看内容”、设置页读状态）。
pub(crate) fn reporter(cx: &App) -> Option<UsageReporter> {
    ui(cx).map(|ui| ui.reporter.clone())
}

/// 被环境变量关闭时为变量名：设置中的开关显示为关、不能改。
pub(crate) fn env_off(cx: &App) -> Option<&'static str> {
    ui(cx).and_then(|ui| ui.reporter.env_off())
}

/// 统计会发到哪里（远程配置暂停时设置中说明“暂停发送”）。
pub(crate) fn destination(cx: &App) -> Option<Destination> {
    ui(cx).map(|ui| ui.reporter.destination())
}

/// 记下今天用过 Termlet（窗口获得焦点时）。
pub(crate) fn mark_active() {
    termlet_core::usage::mark_active();
}

/// 按当前的设置检查一次，满足条件时在后台发送（见模块说明）。应用锁锁着时不检查（主窗口在解锁之后再调用一次）。
pub(crate) fn check(cx: &mut App) {
    let Some(ui) = ui(cx) else {
        return;
    };
    if crate::lock_ui::is_locked(cx) {
        return;
    }
    // 窗口一直在前台、跨过了 UTC 零点：也算今天用过
    if cx.active_window().is_some() {
        termlet_core::usage::mark_active();
    }
    let usage = AppState::store(cx).local_settings().usage;
    let now_ms = termlet_core::time::now_ms();
    let input = CheckInput {
        enabled: usage.enabled,
        active: termlet_core::usage::is_active(now_ms),
        lang: crate::i18n::language().code().to_string(),
        now_ms,
    };
    let reporter = ui.reporter.clone();
    drop(termlet_core::runtime::spawn(async move {
        let outcome = reporter.check(input).await;
        tracing::debug!(?outcome, "检查使用统计");
    }));
}

/// 本机设置变了：关掉“发送匿名使用统计”时删除 `usage.json`，重新打开时检查一次。
fn on_local_settings_changed(cx: &mut App) {
    let Some(ui) = ui(cx) else {
        return;
    };
    let enabled = AppState::store(cx).local_settings().usage.enabled;
    if ui.enabled.replace(enabled) == enabled {
        return;
    }
    tracing::info!(enabled, "使用统计的开关改变");
    if enabled {
        check(cx);
    } else {
        let reporter = ui.reporter.clone();
        cx.background_spawn(async move { reporter.forget() })
            .detach();
    }
}
