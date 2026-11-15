//! 主机工具“在终端中执行”的命令结束时通知发起的一方（主机工具据此重读那一页，见 `workspace.rs` 的
//! `refresh_tools_after_run`）。
//!
//! - 有 shell 集成时按命令结束的标记（OSC 133 的 D）：命令发出之后的第一个，通知一次。
//! - 没有 shell 集成时按输出推断：发出后至少 [`SETTLE`]、输出停了 [`QUIET`]、不在等密码（sudo、su 的密码提示）时
//!   通知；之后又有输出（不出声的命令结束时 shell 打出提示符）、再停下时再通知，最多等 [`MAX_WAIT`]。
//! - Tab 关闭时还在等的也通知一次。

use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::{App, Context, Task};

use super::TerminalPanel;

/// 多久看一次输出停了没有。
const POLL: Duration = Duration::from_millis(500);
/// 没有 shell 集成时：命令发出后至少这么久、输出停了这么久，才当它结束了。
const SETTLE: Duration = Duration::from_secs(2);
const QUIET: Duration = Duration::from_secs(3);
/// 没有 shell 集成时最多等这么久（之后只在 Tab 关闭时通知）。
const MAX_WAIT: Duration = Duration::from_secs(600);

/// 等命令结束的状态。
#[derive(Default)]
pub(super) struct RunDone {
    /// 结束时做的事；不再等时为 `None`
    callback: Option<Rc<dyn Fn(&mut App)>>,
    /// 命令发出的时间
    sent: Option<Instant>,
    /// 按输出推断时，上一次通知时最后一段输出的时间（之后又有输出、再停下时再通知）
    notified_at: Option<Instant>,
    _watch: Option<Task<()>>,
}

impl TerminalPanel {
    /// 这个 Tab 中执行的命令（[`Self::duplicate_running`]）结束时调用 `done`（见模块说明）。
    pub(crate) fn on_run_finished(
        &mut self,
        done: impl Fn(&mut App) + 'static,
        cx: &mut Context<Self>,
    ) {
        self.run_done.callback = Some(Rc::new(done));
        cx.on_release(|this, cx| {
            if let Some(done) = this.run_done.callback.take() {
                done(cx);
            }
        })
        .detach();
    }

    /// 命令发出了：开始等它结束。
    pub(super) fn run_sent(&mut self, cx: &mut Context<Self>) {
        if self.run_done.callback.is_none() {
            return;
        }
        let sent = Instant::now();
        self.run_done.sent = Some(sent);
        self.run_done._watch = Some(cx.spawn(async move |this, cx| {
            while sent.elapsed() < MAX_WAIT {
                cx.background_executor().timer(POLL).await;
                if this.update(cx, |this, cx| this.check_quiet(cx)).is_err() {
                    return;
                }
            }
        }));
    }

    /// shell 集成报告一条命令结束了：通知，不再等。
    pub(super) fn run_command_finished(&mut self, cx: &mut App) {
        if self.run_done.sent.is_none() {
            return;
        }
        self.run_done._watch = None;
        if let Some(done) = self.run_done.callback.take() {
            tracing::debug!(session = %self.name, "在终端中执行的命令结束（shell 集成）");
            done(cx);
        }
    }

    /// 没有 shell 集成时按输出推断（见模块说明）。
    fn check_quiet(&mut self, cx: &mut App) {
        let Some(sent) = self.run_done.sent else {
            return;
        };
        let integration = self
            .view
            .as_ref()
            .is_some_and(|view| view.read(cx).engine().has_shell_integration());
        let settled = !integration
            && sent.elapsed() >= SETTLE
            && !self.password_prompt
            && self.last_output.is_none_or(|at| at.elapsed() >= QUIET)
            && self.last_output != self.run_done.notified_at;
        if !settled {
            return;
        }
        self.run_done.notified_at = self.last_output;
        if let Some(done) = self.run_done.callback.clone() {
            tracing::debug!(session = %self.name, "在终端中执行的命令的输出停下了");
            done(cx);
        }
    }
}
