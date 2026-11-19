//! 终端 Tab 的会话录制（格式与写文件见 [`termlet_core::recording`]）。
//!
//! - Tab 菜单与命令面板中开始、停止；录制中 Tab 上有红色的录制标记与时长（每秒更新）。
//! - 开始：先在后台建文件，建好后在界面线程上取屏幕的快照、同一时刻把录制接到输出旁路上
//!   （[`termlet_terminal::TerminalEngine::screen_dump_then`]），录制从快照接上，不重不漏。开始时提示录制里可能有
//!   敏感的输出；设置开启了“同时录制输入”时另外提示输入（含密码）也会记下。
//! - 停止、会话结束（含断线）、写入失败、达到大小上限时停止，等后台写完文件再提示“录制已保存”，提示中可以直接回放；
//!   关闭 Tab 时停止（不再提示）。
//! - 主机设置了“连接后自动录制”时每次连上（含重连）都开始一个新的录制。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, h_flex};
use gpui_kit::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div,
};
use termlet_core::model::Host;
use termlet_core::recording::cast::CastTheme;
use termlet_core::recording::{self, PendingRecording, Recording, RecordingOptions};
use termlet_terminal::{Palette, Rgb};

use super::{Session, TerminalPanel};
use crate::app_state::AppState;
use crate::t;
use crate::workspace::Workspace;

/// 停止后等后台线程写完文件，最多等这么久再提示。
const FINISH_WAIT: Duration = Duration::from_secs(10);

impl TerminalPanel {
    /// 是否正在录制（文件还在建时也算）。
    pub fn is_recording(&self) -> bool {
        self.tap.recording().is_some() || self._record_start.is_some()
    }

    /// 正在录制的文件（已经接上的；文件还在建时为 `None`）。
    pub fn recording_path(&self) -> Option<PathBuf> {
        self.tap.recording().map(|r| r.path().to_path_buf())
    }

    /// 能不能开始、停止录制：终端视图已经建好（还在连接时没有）。
    pub fn can_record(&self) -> bool {
        self.view.is_some()
    }

    /// 开始或停止录制（Tab 菜单、命令面板）。
    pub fn toggle_recording(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_recording() {
            self.stop_recording(None, window, cx);
        } else {
            self.start_recording(window, cx);
        }
    }

    /// 开始录制：在后台建文件，建好后接上（见模块说明）。
    pub fn start_recording(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_recording() || self.view.is_none() {
            return;
        }
        let local = self.store.local_settings();
        let root = recording::recordings_root(&local.logs, &AppState::global(cx).data_dir);
        let title = self.name.to_string();
        let create = cx
            .background_executor()
            .spawn(async move { Recording::create(&root, &title) });
        self._record_start = Some(cx.spawn_in(window, async move |this, cx| {
            let result = create.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this._record_start = None;
                match result {
                    Ok(pending) => this.begin_recording(pending, window, cx),
                    Err(e) => {
                        cx.notify();
                        crate::notify::error(
                            t!(
                                "terminal.record.start_failed",
                                error = crate::store_errors::describe(&e)
                            ),
                            window,
                            cx,
                        );
                    }
                }
            });
        }));
        cx.notify();
    }

    /// 文件建好了：取屏幕的快照，同一时刻接上录制。
    fn begin_recording(
        &mut self,
        pending: PendingRecording,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 终端已经没有了（不会发生：视图建好后一直在）：丢弃时删掉空文件
        let Some(view) = self.view.clone() else {
            return;
        };
        let input = self.store.settings().terminal.record_input;
        let theme = cast_theme(&view.read(cx).style().palette);
        let title = self.name.to_string();
        let on_halt = self.tap.recording_halt_reporter();
        let tap = self.tap.clone();
        let recording = view.read(cx).engine().screen_dump_then(move |screen| {
            let recording = Arc::new(pending.begin(
                RecordingOptions {
                    title,
                    columns: screen.columns,
                    rows: screen.rows,
                    input,
                    screen: Some(screen.text),
                    theme: Some(theme),
                },
                on_halt,
            ));
            if let Some(previous) = tap.set_recording(Some(recording.clone())) {
                previous.stop();
            }
            recording
        });
        if recording.records_input() {
            let observer = self.tap.input_observer();
            view.update(cx, |view, _| view.set_input_observer(Some(observer)));
        }
        tracing::info!(
            session = %self.name,
            input = recording.records_input(),
            path = %recording.path().display(),
            "开始录制终端"
        );
        // Tab 上的时长每秒更新
        self._record_clock = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let recording = this.update(cx, |this, cx| {
                    let recording = this.tap.recording().is_some();
                    if recording {
                        cx.notify();
                    }
                    recording
                });
                if !matches!(recording, Ok(true)) {
                    break;
                }
            }
        }));
        let notice = if recording.records_input() {
            Notification::warning(t!("terminal.record.started_with_input"))
        } else {
            Notification::info(t!("terminal.record.started"))
        };
        window.push_notification(notice, cx);
        cx.notify();
    }

    /// 停止录制，等文件写完后提示已保存（`reason` 是录制为什么结束，用户停止的为 `None`）。没有在录制时什么也不做；
    /// 文件还在建时取消（删掉空文件）。
    pub fn stop_recording(
        &mut self,
        reason: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._record_start = None;
        self._record_clock = None;
        if let Some(view) = &self.view {
            view.update(cx, |view, _| view.set_input_observer(None));
        }
        let Some(recording) = self.tap.set_recording(None) else {
            cx.notify();
            return;
        };
        recording.stop();
        tracing::info!(
            session = %self.name,
            elapsed_secs = recording.elapsed().as_secs(),
            "停止录制终端"
        );
        let path = recording.path().to_path_buf();
        let wait = cx
            .background_executor()
            .spawn(async move { recording.wait_stopped(FINISH_WAIT) });
        cx.spawn_in(window, async move |_, cx| {
            wait.await;
            let _ = cx.update(|window, cx| announce_saved(path, reason, window, cx));
        })
        .detach();
        cx.notify();
    }

    /// 关闭 Tab：停止录制（后台线程写完文件），不再提示。
    pub(super) fn drop_recording(&mut self) {
        if let Some(recording) = self.tap.set_recording(None) {
            recording.stop();
        }
    }

    /// 录制自己停下了（写入失败、达到大小上限）：收起标记；文件已写完的照样提示已保存。
    pub(super) fn on_recording_halted(
        &mut self,
        halt: recording::Halt,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match halt {
            recording::Halt::Full => {
                let reason = t!("terminal.record.reason_full");
                self.stop_recording(Some(reason), window, cx);
            }
            recording::Halt::Failed(e) => {
                self._record_clock = None;
                if let Some(view) = &self.view {
                    view.update(cx, |view, _| view.set_input_observer(None));
                }
                self.tap.set_recording(None);
                crate::notify::error(
                    t!(
                        "terminal.record.failed",
                        error = crate::store_errors::describe(&e)
                    ),
                    window,
                    cx,
                );
                cx.notify();
            }
        }
    }

    /// 主机设置了“连接后自动录制”时开始录制（每次连上、重连后）。
    pub(super) fn auto_record(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let wanted = matches!(self.session, Session::Host { .. })
            && self
                .host()
                .and_then(|id| self.store.get::<Host>(id))
                .is_some_and(|host| host.doc.terminal.record);
        if wanted && !self.is_recording() {
            tracing::info!(session = %self.name, "主机设置了连接后自动录制");
            self.start_recording(window, cx);
        }
    }

    /// Tab 上的录制标记：红点与时长（文件还在建时只有红点）。悬停的说明在 Tab 的右键菜单开着时不显示
    /// （[`crate::menu_tooltip::quiet`]：标记也放在竖排 Tab 的行里，那里拿不到窗口）。
    pub(super) fn render_record_badge(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if !self.is_recording() {
            return None;
        }
        let elapsed = self.tap.recording().map(|r| r.elapsed());
        Some(
            h_flex()
                .id(("tab-record", cx.entity_id().as_u64() as usize))
                .gap_0p5()
                .text_xs()
                .text_color(cx.theme().danger)
                .tooltip(crate::menu_tooltip::quiet(|window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(t!("terminal.record.badge"))
                        .build(window, cx)
                }))
                .child(div().size_2().rounded_full().bg(cx.theme().danger))
                .children(elapsed.map(clock_text))
                .into_any_element(),
        )
    }
}

/// 录制的时长：`m:ss`，一小时以上 `h:mm:ss`。
pub(crate) fn clock_text(elapsed: Duration) -> SharedString {
    let secs = elapsed.as_secs();
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}").into()
    } else {
        format!("{m}:{s:02}").into()
    }
}

/// 录制时的配色写进头部（asciinema 回放时据此着色）。
fn cast_theme(palette: &Palette) -> CastTheme {
    let hex = |rgb: Rgb| format!("#{:02x}{:02x}{:02x}", rgb.r, rgb.g, rgb.b);
    CastTheme {
        foreground: hex(palette.foreground),
        background: hex(palette.background),
        palette: palette.ansi.iter().map(|&c| hex(c)).collect(),
    }
}

/// 提示录制已保存，可以直接回放。
fn announce_saved(
    path: PathBuf,
    reason: Option<SharedString>,
    window: &mut Window,
    cx: &mut gpui_kit::App,
) {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // 标题说已保存（与结束的原因），正文是文件名：文件名长，放进句子里换行时冒号会落到行首
    let title = match reason {
        Some(reason) => t!("terminal.record.saved_because", reason = reason),
        None => t!("terminal.record.saved"),
    };
    let notification = Notification::success(name)
        .title(title)
        .action(move |_, _, cx| {
            let path = path.clone();
            let notification = cx.entity().downgrade();
            Button::new("record-saved-replay")
                .link()
                .label(t!("terminal.record.replay"))
                .on_click(move |_, window, cx| {
                    if let Some(workspace) = Workspace::in_window(window, cx) {
                        workspace.update(cx, |workspace, cx| {
                            workspace.open_replay(path.clone(), window, cx)
                        });
                    }
                    let _ = notification
                        .update(cx, |notification, cx| notification.dismiss(window, cx));
                })
        });
    window.push_notification(notification, cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_text_switches_to_hours() {
        assert_eq!(clock_text(Duration::from_secs(5)), "0:05");
        assert_eq!(clock_text(Duration::from_secs(605)), "10:05");
        assert_eq!(clock_text(Duration::from_secs(3725)), "1:02:05");
    }
}
