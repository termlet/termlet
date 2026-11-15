//! 终端中的文件传输（ZModem 的 rz / sz、trzsz 的 trz / tsz）的提示栏：浮在终端顶部，不改变终端的大小。
//!
//! - 服务器要发送文件：保存到上次的目录 / 选择位置 / 取消；服务器在等待接收文件：选择文件（`trz -d` 为选择目录）/
//!   取消，也可以把文件拖到终端上。
//! - 传输中显示当前文件、已传 / 大小、第几个、速度与总进度，可以取消（终端中按 Ctrl+C 同样取消）。
//! - 结束时显示结果（下载的可以打开文件夹）；取消的 4 秒后关闭，完成且没有跳过项、开启了“传输完成后自动清除”的
//!   5 秒后关闭，失败与有跳过项的一直保留。

use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    Context, EventEmitter, InteractiveElement as _, IntoElement, ParentElement as _,
    PathPromptOptions, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Task,
    Window, div, prelude::FluentBuilder as _, relative,
};
use termlet_core::termxfer::{
    Answer, InterceptOptions, Interceptor, LocalErrorKind, Protocol, TransferInfo, TransferState,
    XferError,
};

use crate::app_state::AppState;
use crate::prose::prose;
use crate::sftp::list_model::format_size;
use crate::t;

/// 取消的提示多久后关闭。
const CANCELLED_CLOSE: Duration = Duration::from_secs(4);
/// 完成（没有跳过项、开启了自动清除）的提示多久后关闭。
const DONE_CLOSE: Duration = Duration::from_secs(5);

/// 提示栏关闭了（焦点回到终端）。
pub struct TransferBarClosed;

pub struct TransferBar {
    xfer: Interceptor,
    /// 正在显示的传输；关闭后为 `None`
    info: Option<TransferInfo>,
    /// 下载的默认目录（在后台检查过存在）；还没检查完时为 `None`
    default_dir: Option<PathBuf>,
    _default_dir: Option<Task<()>>,
    _events: Task<()>,
    _close: Option<Task<()>>,
}

impl EventEmitter<TransferBarClosed> for TransferBar {}

impl TransferBar {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let enabled = AppState::store(cx).settings().terminal.file_transfer;
        let xfer = Interceptor::new(InterceptOptions {
            enabled,
            ..Default::default()
        });
        let events = xfer.events();
        let task = cx.spawn(async move |this, cx| {
            while let Ok(info) = events.recv().await {
                if this.update(cx, |this, cx| this.on_info(info, cx)).is_err() {
                    return;
                }
            }
        });
        Self {
            xfer,
            info: None,
            default_dir: None,
            _default_dir: None,
            _events: task,
            _close: None,
        }
    }

    pub fn interceptor(&self) -> &Interceptor {
        &self.xfer
    }

    pub fn is_visible(&self) -> bool {
        self.info.is_some()
    }

    /// 传输在进行（等待回答或传输中）；结束后结果栏还在显示时不算。
    pub fn is_transferring(&self) -> bool {
        self.info.as_ref().is_some_and(|i| !i.state.is_finished())
    }

    /// 文件拖到终端上：服务器在等待接收文件时作答（返回是否接收了）。
    pub fn drop_files(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.xfer.accepts_drop() else {
            return false;
        };
        let count = paths.len();
        let accepted = self.xfer.answer(id, Answer::Send(paths)).is_ok();
        tracing::info!(count, accepted, "终端文件传输：拖入要上传的文件");
        cx.notify();
        accepted
    }

    fn on_info(&mut self, info: TransferInfo, cx: &mut Context<Self>) {
        self._close = None;
        // 服务器要发送文件：在后台检查默认目录（上次的目录可能是已断开的网络共享，检查会卡住）
        let asks_dir = info.state == TransferState::Waiting && !info.upload;
        let was_asking = self
            .info
            .as_ref()
            .is_some_and(|i| i.id == info.id && i.state == TransferState::Waiting && !i.upload);
        if asks_dir && !was_asking {
            self.default_dir = None;
            let saved = AppState::global(cx)
                .ui
                .read(cx)
                .state()
                .terminal_transfer_dir
                .clone();
            let check = cx
                .background_executor()
                .spawn(async move { Self::default_dir(saved) });
            self._default_dir = Some(cx.spawn(async move |this, cx| {
                let dir = check.await;
                let _ = this.update(cx, |this, cx| {
                    this.default_dir = dir;
                    cx.notify();
                });
            }));
        }
        let delay = match &info.state {
            TransferState::Cancelled => Some(CANCELLED_CLOSE),
            TransferState::Done
                if info.skipped.is_empty()
                    && AppState::store(cx).settings().terminal.transfer_auto_clear =>
            {
                Some(DONE_CLOSE)
            }
            _ => None,
        };
        if let Some(delay) = delay {
            let id = info.id;
            self._close = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(delay).await;
                let _ = this.update(cx, |this, cx| {
                    if this.info.as_ref().is_some_and(|i| i.id == id) {
                        this.close(cx);
                    }
                });
            }));
        }
        self.info = Some(info);
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.info = None;
        self._close = None;
        cx.emit(TransferBarClosed);
        cx.notify();
    }

    /// 下载的默认目录：上次选的（`saved`），没有时是“下载”文件夹。要访问文件系统，在后台调用。
    fn default_dir(saved: Option<PathBuf>) -> Option<PathBuf> {
        saved.filter(|d| d.is_dir()).or_else(|| {
            std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(|home| PathBuf::from(home).join("Downloads"))
                .filter(|d| d.is_dir())
        })
    }

    fn save_to(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        let Some(info) = &self.info else {
            return;
        };
        tracing::info!(dir = %dir.display(), "终端文件传输：保存到");
        if self
            .xfer
            .answer(info.id, Answer::SaveTo(dir.clone()))
            .is_ok()
        {
            AppState::ui(cx).update(cx, |ui, cx| {
                ui.update(|s| s.terminal_transfer_dir = Some(dir), cx)
            });
        }
    }

    fn pick_dir(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(dirs))) = picked.await else {
                return;
            };
            if let Some(dir) = dirs.into_iter().next() {
                let _ = this.update(cx, |this, cx| this.save_to(dir, cx));
            }
        })
        .detach();
    }

    fn pick_files(&mut self, directory: bool, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: !directory,
            directories: directory,
            multiple: true,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                if let Some(info) = &this.info {
                    tracing::info!(
                        count = paths.len(),
                        directory,
                        "终端文件传输：选择要上传的文件"
                    );
                    let _ = this.xfer.answer(info.id, Answer::Send(paths));
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn refuse(&mut self, cx: &mut Context<Self>) {
        if let Some(info) = &self.info {
            tracing::info!("终端文件传输：拒绝");
            let _ = self.xfer.answer(info.id, Answer::Refuse);
        }
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(info) = &self.info {
            tracing::info!("终端文件传输：取消");
            self.xfer.cancel(info.id);
        }
        cx.notify();
    }
}

pub fn describe_error(error: &XferError) -> SharedString {
    match error {
        XferError::ZmodemOverConpty => t!("transfer.error.zmodem_conpty"),
        XferError::ZmodemInTmux => t!("transfer.error.zmodem_tmux"),
        XferError::ZmodemDirectory(path) => {
            t!("transfer.error.zmodem_directory", path = path.display())
        }
        XferError::TrzNeedsDirectory(path) => {
            t!("transfer.error.trz_needs_directory", path = path.display())
        }
        XferError::BadSaveDir(path) => t!("transfer.error.bad_save_dir", path = path.display()),
        XferError::NotRegularFile(path) => t!("transfer.error.not_regular", path = path.display()),
        XferError::Local {
            path,
            kind,
            message,
        } => {
            let path = path.display();
            match kind {
                LocalErrorKind::NotFound => t!("transfer.error.local_not_found", path = path),
                LocalErrorKind::PermissionDenied => {
                    t!("transfer.error.local_permission", path = path)
                }
                LocalErrorKind::NoSpace => t!("transfer.error.local_no_space", path = path),
                LocalErrorKind::Busy => t!("transfer.error.local_busy", path = path),
                LocalErrorKind::Other => {
                    t!("transfer.error.local_other", path = path, message = message)
                }
            }
        }
        XferError::NoUniqueName(name) => t!("transfer.error.no_unique_name", name = name),
        XferError::DuplicateName(name) => t!("transfer.error.duplicate_name", name = name),
        XferError::Remote(message) => t!("transfer.error.remote", message = message),
        XferError::RemoteInterrupted => t!("transfer.error.remote_interrupted"),
        XferError::RemoteAborted => t!("transfer.error.remote_aborted"),
        XferError::Timeout => t!("transfer.error.timeout"),
        XferError::Disconnected => t!("transfer.error.disconnected"),
        XferError::Checksum => t!("transfer.error.checksum"),
        XferError::LineTooLong => t!("transfer.error.line_too_long"),
        XferError::BlockTooLarge => t!("transfer.error.block_too_large"),
        XferError::UnsupportedTrzsz(what) => t!("transfer.error.unsupported_trzsz", what = what),
        XferError::Protocol { protocol, detail } => t!(
            "transfer.error.protocol",
            protocol = protocol.label(),
            detail = detail
        ),
        XferError::NotWaiting => t!("transfer.error.not_waiting"),
        XferError::WrongAnswer => t!("transfer.error.wrong_answer"),
    }
}

impl Render for TransferBar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(info) = self.info.clone() else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let (muted, foreground) = (theme.muted_foreground, theme.foreground);
        let tint = match &info.state {
            TransferState::Done => theme.success,
            TransferState::Failed(_) => theme.danger,
            _ => theme.info,
        };
        let protocol = info.protocol.label();
        let icon = if info.upload {
            IconName::ArrowUp
        } else {
            IconName::ArrowDown
        };
        let mut row = h_flex()
            .id("transfer-bar")
            .w_full()
            .px_3()
            .py_1()
            .gap_2()
            .text_xs()
            .bg(theme.popover)
            .border_b_1()
            .border_color(tint.opacity(0.6))
            .text_color(foreground)
            .child(Icon::new(icon).small().text_color(tint));
        let mut progress: Option<f32> = None;
        match &info.state {
            TransferState::Waiting if info.upload => {
                let directory = info.directory;
                row = row
                    .child(div().child(prose(t!("transfer.ask_upload", protocol = protocol))))
                    .child(div().text_color(muted).child(t!("transfer.drop_hint")))
                    .child(div().flex_1())
                    .child(
                        Button::new("transfer-pick-files")
                            .primary()
                            .xsmall()
                            .label(if directory {
                                t!("transfer.pick_dirs")
                            } else {
                                t!("transfer.pick_files")
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.pick_files(directory, window, cx)
                            })),
                    )
                    .child(
                        Button::new("transfer-refuse")
                            .ghost()
                            .xsmall()
                            .label(t!("common.cancel"))
                            .on_click(cx.listener(|this, _, _, cx| this.refuse(cx))),
                    );
            }
            TransferState::Waiting => {
                let default = self.default_dir.clone();
                row = row
                    .child(div().child(prose(t!("transfer.ask_download", protocol = protocol))))
                    .child(div().flex_1())
                    .when_some(default, |this, dir| {
                        let name = dir
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| dir.display().to_string());
                        let full: SharedString = dir.display().to_string().into();
                        this.child(
                            Button::new("transfer-save-default")
                                .primary()
                                .xsmall()
                                .label(t!("transfer.save_to", dir = name))
                                .tooltip(full)
                                .on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        this.save_to(dir.clone(), cx)
                                    }),
                                ),
                        )
                    })
                    .child(
                        Button::new("transfer-pick-dir")
                            .outline()
                            .xsmall()
                            .label(t!("transfer.pick_dir"))
                            .on_click(cx.listener(|this, _, window, cx| this.pick_dir(window, cx))),
                    )
                    .child(
                        Button::new("transfer-refuse")
                            .ghost()
                            .xsmall()
                            .label(t!("common.cancel"))
                            .on_click(cx.listener(|this, _, _, cx| this.refuse(cx))),
                    );
            }
            TransferState::Running => {
                let mut text = if info.upload {
                    t!("transfer.uploading", file = info.file.clone())
                } else {
                    t!("transfer.downloading", file = info.file.clone())
                }
                .to_string();
                text.push_str(&format!(
                    "  {} / {}",
                    format_size(info.file_bytes),
                    format_size(info.file_size)
                ));
                if info.total_files > 1 {
                    text.push_str(" · ");
                    text.push_str(&t!(
                        "transfer.file_count",
                        index = (info.files + 1).min(info.total_files),
                        total = info.total_files
                    ));
                }
                if info.speed > 0 {
                    text.push_str(&format!(" · {}/s", format_size(info.speed)));
                }
                if info.total_bytes > 0 {
                    let percent = info.bytes as f64 * 100. / info.total_bytes as f64;
                    text.push_str(&format!(" · {percent:.0}%"));
                }
                if info.file_size > 0 {
                    progress = Some((info.file_bytes as f32 / info.file_size as f32).clamp(0., 1.));
                }
                row = row
                    .child(div().flex_1().min_w_0().truncate().child(prose(text)))
                    .child(
                        Button::new("transfer-cancel")
                            .ghost()
                            .xsmall()
                            .label(t!("common.cancel"))
                            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                    );
            }
            TransferState::Done => {
                let mut text = if info.upload {
                    t!("transfer.uploaded", count = info.uploaded.len()).to_string()
                } else {
                    let dir = info
                        .dir
                        .as_ref()
                        .map(|d| d.display().to_string())
                        .unwrap_or_default();
                    t!("transfer.saved", count = info.saved.len(), dir = dir).to_string()
                };
                if !info.skipped.is_empty() {
                    let count = info.skipped.len();
                    text.push_str(&if info.upload && info.protocol == Protocol::Zmodem {
                        t!("transfer.skipped_rz", count = count)
                    } else {
                        t!("transfer.skipped", count = count)
                    });
                }
                let skipped: SharedString = info.skipped.join("\n").into();
                let first = info.saved.first().cloned();
                row = row
                    .child(
                        div()
                            .id("transfer-result")
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(prose(text))
                            .when(!skipped.is_empty(), |this| {
                                this.tooltip(move |window, cx| {
                                    Tooltip::new(skipped.clone()).build(window, cx)
                                })
                            }),
                    )
                    .when_some(first.filter(|_| !info.upload), |this, first| {
                        this.child(
                            Button::new("transfer-show-folder")
                                .outline()
                                .xsmall()
                                .icon(IconName::FolderOpen)
                                .label(t!("sftp.open_folder"))
                                .on_click(move |_, _, cx| cx.reveal_path(&first)),
                        )
                    });
            }
            TransferState::Failed(error) => {
                row = row.child(div().flex_1().min_w_0().truncate().child(prose(t!(
                    "transfer.failed",
                    message = describe_error(error)
                ))));
            }
            TransferState::Cancelled => {
                row = row.child(div().flex_1().child(t!("transfer.cancelled")));
            }
        }
        if info.state.is_finished() {
            row = row.child(
                Button::new("transfer-close")
                    .ghost()
                    .small()
                    .icon(IconName::Close)
                    .tooltip(t!("common.close"))
                    .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
            );
        }
        v_flex()
            .w_full()
            .child(row)
            .when_some(progress, |this, fraction| {
                this.child(
                    div()
                        .h_0p5()
                        .w_full()
                        .bg(theme.muted)
                        .child(div().h_full().bg(theme.primary).w(relative(fraction))),
                )
            })
            .into_any_element()
    }
}
