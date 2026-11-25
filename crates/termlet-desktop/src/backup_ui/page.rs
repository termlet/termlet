//! 设置中的“备份”：完整备份（用密码加密的文件，可在另一台电脑上恢复）与恢复、定时备份（见 [`schedule`]）、
//! 本机快照（列表、立即保存、恢复）、自动快照的设置。
//!
//! - 备份、恢复、快照、定时备份的设置都立即执行，与设置的“保存”无关；自动快照的开关与份数是草稿，随“保存”写入
//!   （[`BackupPage::validate`]、[`BackupPage::apply`]）。
//! - 恢复（完整备份或快照）成功后停止正在运行的端口转发（规则可能已不存在），并发出 [`BackupPageEvent::Restored`]：
//!   存储已发出全部种类的变更事件，各视图自己重新读取；设置中其余分类的草稿已过时，由设置从存储重新载入。
//! - 完整备份区域里的输入框按 Enter 执行当前的一步（备份、读取备份），不会触发设置的“保存”。

use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, InteractiveElement as _,
    IntoElement, ParentElement as _, PathPromptOptions, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, TestSupportExt as _, Window,
    div, prelude::FluentBuilder as _, rems,
};
use termlet_core::backup::{
    BackupError, BackupOptions, BackupSummary, Backups, Counts, RestoreOutcome, SnapshotInfo,
    SnapshotReason,
};
use termlet_core::model::LocalSettings;
use termlet_core::runtime;
use termlet_core::secret::SecretString;

use super::{PasswordCheck, PasswordPair, counts_more_text, counts_text, errors};

mod schedule;
use crate::app_state::AppState;
use crate::prose::prose;
use crate::t;

/// 端口转发的管理者：恢复替换了全部数据后要停止正在运行的转发（规则可能已不存在）。
fn forward_manager(cx: &App) -> termlet_core::forward::ForwardManager {
    AppState::global(cx).forwards.read(cx).manager().clone()
}

/// 自动快照保留的份数的范围（与核心的设置说明相同）。
const KEEP_RANGE: (u32, u32) = (1, 30);

/// 页面发出的事件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BackupPageEvent {
    /// 已用完整备份或快照替换了全部数据（端口转发已停止）。`vault_reset`：主密钥丢失，恢复前重置了保险库；
    /// `message` 是页面上显示的结果说明（设置重新载入后改在提示行里显示）
    Restored {
        vault_reset: bool,
        message: SharedString,
    },
}

/// 完整备份区域的当前步骤。
enum Mode {
    Idle,
    /// 设置备份密码
    Create,
    /// 从备份恢复：选好的文件，读出的说明（读出前为 `None`）
    Restore {
        path: PathBuf,
        needs_password: bool,
        summary: Option<BackupSummary>,
    },
}

/// 正在进行的操作（同一时间只进行一项）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Create,
    Inspect,
    Restore,
    Snapshot,
    RestoreSnapshot,
    ScheduleSave,
    ScheduleRun,
    ScheduleDisable,
}

/// 操作的结果说明。
struct Message {
    text: SharedString,
    error: bool,
    /// 可以在文件夹中显示的文件（备份的位置）
    reveal: Option<PathBuf>,
}

impl Message {
    fn ok(text: SharedString) -> Self {
        Self {
            text,
            error: false,
            reveal: None,
        }
    }

    fn error(text: SharedString) -> Self {
        Self {
            text,
            error: true,
            reveal: None,
        }
    }
}

/// 自动快照保留的份数：范围内的整数。
fn parse_keep(text: &str) -> Option<u32> {
    text.trim()
        .parse::<u32>()
        .ok()
        .filter(|n| (KEEP_RANGE.0..=KEEP_RANGE.1).contains(n))
}

fn reason_text(reason: SnapshotReason) -> SharedString {
    match reason {
        SnapshotReason::Auto => t!("backup_ui.reason.auto"),
        SnapshotReason::Manual => t!("backup_ui.reason.manual"),
        SnapshotReason::Restore => t!("backup_ui.reason.restore"),
        SnapshotReason::Import => t!("backup_ui.reason.import"),
        SnapshotReason::Upgrade => t!("backup_ui.reason.upgrade"),
        SnapshotReason::Sync => t!("backup_ui.reason.sync"),
    }
}

pub(crate) struct BackupPage {
    backups: Backups,
    /// 草稿：每天自动保存快照
    auto: bool,
    /// 草稿：自动快照保留的份数
    keep: Entity<InputState>,
    mode: Mode,
    password: PasswordPair,
    audit: bool,
    restore_password: Entity<InputState>,
    /// 当前数据的数量（创建备份时显示）
    counts: Option<Counts>,
    /// 本机快照，从新到旧；`None` 为还在读取
    snapshots: Option<Vec<SnapshotInfo>>,
    busy: Option<Op>,
    full_message: Option<Message>,
    snapshot_message: Option<Message>,
    /// 定时备份的表单与状态
    schedule: schedule::ScheduleView,
    _task: Option<Task<()>>,
    _list: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<BackupPageEvent> for BackupPage {}

impl BackupPage {
    pub(crate) fn new(local: &LocalSettings, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let keep =
            cx.new(|cx| InputState::new(window, cx).default_value(local.backup.keep.to_string()));
        let restore_password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t!("backup_ui.page.restore_password"))
        });
        let (password, mut subscriptions) = PasswordPair::new(window, cx);
        let (schedule, schedule_subscriptions) = schedule::ScheduleView::new(window, cx);
        subscriptions.extend(schedule_subscriptions);
        for state in [&keep, &restore_password] {
            subscriptions.push(cx.subscribe(state, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }));
        }
        let mut this = Self {
            backups: super::backups(cx),
            auto: local.backup.auto,
            keep,
            mode: Mode::Idle,
            password,
            audit: false,
            restore_password,
            counts: None,
            snapshots: None,
            busy: None,
            full_message: None,
            snapshot_message: None,
            schedule,
            _task: None,
            _list: None,
            _subscriptions: subscriptions,
        };
        this.load_snapshots(cx);
        this
    }

    /// 锁定时清空输入到一半的密码（备份、恢复与定时备份的）。
    pub(crate) fn clear_secrets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.password.clear(window, cx);
        self.restore_password
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.schedule.clear_password(window, cx);
    }

    /// 草稿是否可以保存。
    pub(crate) fn validate(&self, cx: &App) -> Result<(), SharedString> {
        if self.auto && parse_keep(&self.keep.read(cx).value()).is_none() {
            return Err(t!(
                "backup_ui.page.keep_invalid",
                min = KEEP_RANGE.0,
                max = KEEP_RANGE.1
            ));
        }
        Ok(())
    }

    /// 把草稿写进设置（份数不合法时保持原值；保存前先 [`BackupPage::validate`]）。
    pub(crate) fn apply(&self, local: &mut LocalSettings, cx: &App) {
        local.backup.auto = self.auto;
        if let Some(keep) = parse_keep(&self.keep.read(cx).value()) {
            local.backup.keep = keep;
        }
    }

    fn load_snapshots(&mut self, cx: &mut Context<Self>) {
        let backups = self.backups.clone();
        let job = runtime::spawn(async move { backups.snapshots().await });
        self._list = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(list)) => this.snapshots = Some(list),
                    Ok(Err(e)) => {
                        this.snapshots = Some(Vec::new());
                        this.snapshot_message = Some(Message::error(errors::describe_backup(&e)));
                    }
                    Err(e) => {
                        this.snapshots = Some(Vec::new());
                        this.snapshot_message = Some(Message::error(e.to_string().into()));
                    }
                }
                cx.notify();
            });
        }));
    }

    fn set_mode(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = mode;
        self.audit = false;
        self.full_message = None;
        self.password.clear(window, cx);
        self.restore_password
            .update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    fn start_create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.counts = self.backups.current_counts().ok();
        self.set_mode(Mode::Create, window, cx);
    }

    /// 选择保存位置后创建完整备份。
    fn submit_create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() || !matches!(self.mode, Mode::Create) {
            return;
        }
        let Some(password) = self.password.secret(cx) else {
            return;
        };
        let name = format!("termlet-backup-{}.termlet-backup", super::file_stamp());
        let picked = cx.prompt_for_new_path(&super::default_dir(), Some(&name));
        let backups = self.backups.clone();
        let options = BackupOptions { audit: self.audit };
        self.busy = Some(Op::Create);
        self.full_message = None;
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = picked.await else {
                let _ = this.update(cx, |this, cx| {
                    this.busy = None;
                    cx.notify();
                });
                return;
            };
            let target = path.clone();
            tracing::info!(path = %path.display(), "创建完整备份");
            let job =
                runtime::spawn(
                    async move { backups.create_backup(target, password, options).await },
                );
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = None;
                match result {
                    Ok(Ok(_)) => {
                        termlet_core::usage::mark(termlet_core::usage::Feature::Backup);
                        this.set_mode(Mode::Idle, window, cx);
                        this.full_message = Some(Message {
                            text: t!("backup_ui.page.created", path = path.display()),
                            error: false,
                            reveal: Some(path),
                        });
                    }
                    Ok(Err(e)) => {
                        this.full_message = Some(Message::error(errors::describe_backup(&e)))
                    }
                    Err(e) => this.full_message = Some(Message::error(e.to_string().into())),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// 选择备份文件，读出其中的说明（加密的先要密码）。
    fn pick_restore(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.set_mode(
                    Mode::Restore {
                        path,
                        needs_password: false,
                        summary: None,
                    },
                    window,
                    cx,
                );
                this.inspect(window, cx);
            });
        }));
    }

    fn inspect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Mode::Restore {
            path,
            needs_password,
            summary: None,
        } = &self.mode
        else {
            return;
        };
        if self.busy.is_some() {
            return;
        }
        let password = Some(self.restore_password.read(cx).value().to_string())
            .filter(|p| *needs_password && !p.is_empty())
            .map(SecretString::new);
        if *needs_password && password.is_none() {
            return;
        }
        let backups = self.backups.clone();
        let target = path.clone();
        tracing::info!(path = %path.display(), "读取备份的信息");
        let job = runtime::spawn(async move { backups.inspect_backup(target, password).await });
        self.busy = Some(Op::Inspect);
        self.full_message = None;
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                this.busy = None;
                let Mode::Restore {
                    needs_password,
                    summary,
                    ..
                } = &mut this.mode
                else {
                    return;
                };
                match result {
                    Ok(Ok(found)) => *summary = Some(found),
                    Ok(Err(BackupError::PasswordRequired)) => *needs_password = true,
                    Ok(Err(e)) => {
                        this.full_message = Some(Message::error(errors::describe_backup(&e)))
                    }
                    Err(e) => this.full_message = Some(Message::error(e.to_string().into())),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// 用读出的备份替换全部数据。
    fn restore(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Mode::Restore {
            path,
            summary: Some(summary),
            ..
        } = &self.mode
        else {
            return;
        };
        if self.busy.is_some() {
            return;
        }
        let password = SecretString::new(self.restore_password.read(cx).value().to_string());
        let backups = self.backups.clone();
        let target = path.clone();
        let time = termlet_core::time::format_local(summary.created_at);
        let forwards = forward_manager(cx);
        tracing::info!(path = %path.display(), created = %time, "从完整备份恢复");
        let job = runtime::spawn(async move {
            let result = backups.restore_backup(target, password).await;
            // 停止端口转发放在后台任务里：页面（设置）中途没了也照样执行
            if result.is_ok() {
                forwards.close_all().await;
            }
            result
        });
        self.busy = Some(Op::Restore);
        self.full_message = None;
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = None;
                match result {
                    Ok(Ok(outcome)) => {
                        let text = t!("backup_ui.page.restored", time = time);
                        this.set_mode(Mode::Idle, window, cx);
                        let message = this.after_restore(&outcome, text, window, cx);
                        this.full_message = Some(message);
                    }
                    Ok(Err(e)) => {
                        this.full_message = Some(Message::error(errors::describe_backup(&e)))
                    }
                    Err(e) => this.full_message = Some(Message::error(e.to_string().into())),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn snapshot_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let backups = self.backups.clone();
        tracing::info!("立即保存本机快照");
        let job = runtime::spawn(async move { backups.snapshot(SnapshotReason::Manual).await });
        self.busy = Some(Op::Snapshot);
        self.snapshot_message = None;
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                this.busy = None;
                this.snapshot_message = Some(match result {
                    Ok(Ok(_)) => Message::ok(t!("backup_ui.page.snapshot_saved")),
                    Ok(Err(e)) => Message::error(errors::describe_backup(&e)),
                    Err(e) => Message::error(e.to_string().into()),
                });
                this.load_snapshots(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// 恢复快照前确认（会替换全部数据）。
    fn confirm_restore_snapshot(
        &mut self,
        info: &SnapshotInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let page = cx.entity().downgrade();
        let name = info.name.clone();
        let time = termlet_core::time::format_local(info.created_at);
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let (page, name, time) = (page.clone(), name.clone(), time.clone());
            alert
                .confirm()
                .title(t!("backup_ui.page.confirm_snapshot", time = time.clone()))
                .description(prose(t!("backup_ui.page.restore_warning")))
                .ok_text(t!("backup_ui.page.confirm_snapshot_ok"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, window, cx| {
                    let _ = page.update(cx, |page, cx| {
                        page.restore_snapshot(name.clone(), time.clone(), window, cx)
                    });
                    true
                })
        });
    }

    fn restore_snapshot(
        &mut self,
        name: String,
        time: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        let backups = self.backups.clone();
        let forwards = forward_manager(cx);
        tracing::info!(snapshot = %name, "从本机快照恢复");
        let job = runtime::spawn(async move {
            let result = backups.restore_snapshot(name).await;
            if result.is_ok() {
                forwards.close_all().await;
            }
            result
        });
        self.busy = Some(Op::RestoreSnapshot);
        self.snapshot_message = None;
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = None;
                this.snapshot_message = Some(match result {
                    Ok(Ok(outcome)) => {
                        let text = t!("backup_ui.page.snapshot_restored", time = time);
                        this.after_restore(&outcome, text, window, cx)
                    }
                    Ok(Err(e)) => Message::error(errors::describe_backup(&e)),
                    Err(e) => Message::error(e.to_string().into()),
                });
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// 是否有操作正在进行：设置这时不能关闭、不能移到别的窗口（关掉后结果没处显示，恢复后的收尾也做不了）。
    pub(crate) fn is_busy(&self) -> bool {
        self.busy.is_some()
    }

    /// 恢复成功后（端口转发已在后台任务中停止）：重新读取快照列表与草稿、通知主会话。返回页面上显示的说明。
    fn after_restore(
        &mut self,
        outcome: &RestoreOutcome,
        text: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Message {
        let text: SharedString = if outcome.vault_reset {
            format!("{text}{}", t!("backup_ui.page.vault_reset")).into()
        } else {
            text
        };
        // 设置也被替换了：草稿跟着换成恢复后的值
        let local = AppState::store(cx).local_settings();
        self.auto = local.backup.auto;
        let keep = local.backup.keep.to_string();
        self.keep.update(cx, |s, cx| s.set_value(keep, window, cx));
        self.load_snapshots(cx);
        cx.emit(BackupPageEvent::Restored {
            vault_reset: outcome.vault_reset,
            message: text.clone(),
        });
        Message::ok(text)
    }

    fn open_folder(&self, cx: &mut App) {
        let dir = self.backups.dir().to_path_buf();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(dir = %dir.display(), "无法创建快照目录：{e}");
        }
        cx.open_url(&dir.display().to_string());
    }

    /// Enter（在完整备份区域的输入框中）：执行当前的一步。替换数据不经 Enter，须点按钮。
    fn on_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.mode {
            Mode::Create => self.submit_create(window, cx),
            Mode::Restore { summary: None, .. } => self.inspect(window, cx),
            _ => {}
        }
    }

    // ------------------------------------------------------------ 渲染

    fn render_message(&self, message: &Message, id: &'static str, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let reveal = message.reveal.clone();
        h_flex()
            .gap_2()
            .child(
                div()
                    .id(id)
                    .test_support()
                    .min_w_0()
                    .text_xs()
                    .text_color(if message.error {
                        theme.danger
                    } else {
                        theme.success
                    })
                    .child(message.text.clone()),
            )
            .when_some(reveal, |this, path| {
                this.child(
                    Button::new("backup-reveal")
                        .ghost()
                        .xsmall()
                        .flex_none()
                        .label(t!("sftp.show_in_folder"))
                        .on_click(move |_, _, cx| cx.reveal_path(&path)),
                )
            })
            .into_any_element()
    }

    fn render_full(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (muted, warning) = (theme.muted_foreground, theme.warning);
        let (border, radius) = (theme.border, theme.radius);
        let busy = self.busy.is_some();
        let body = match &self.mode {
            Mode::Idle => h_flex()
                .gap_2()
                .child(
                    Button::new("backup-create")
                        .outline()
                        .small()
                        .label(t!("backup_ui.page.create"))
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, window, cx| this.start_create(window, cx))),
                )
                .child(
                    Button::new("backup-restore")
                        .outline()
                        .small()
                        .label(t!("backup_ui.page.restore_from"))
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, window, cx| this.pick_restore(window, cx))),
                )
                .into_any_element(),
            Mode::Create => v_flex()
                .gap_2()
                .max_w(rems(32.))
                .when_some(self.counts, |this, counts| {
                    this.child(div().text_xs().text_color(muted).child(prose(t!(
                        "backup_ui.page.current",
                        counts = counts_text(&counts)
                    ))))
                })
                .child(self.password.render("backup", cx))
                .child(
                    v_flex()
                        .gap_1()
                        .child(
                            Checkbox::new("backup-audit")
                                .label(t!("backup_ui.page.include_audit"))
                                .checked(self.audit)
                                .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                    this.audit = *checked;
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .pl_6()
                                .text_xs()
                                .text_color(muted)
                                .child(prose(t!("backup_ui.page.include_audit_hint"))),
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(prose(t!("backup_ui.page.password_hint"))),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            Button::new("backup-create-cancel")
                                .ghost()
                                .small()
                                .label(t!("common.cancel"))
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.set_mode(Mode::Idle, window, cx)
                                })),
                        )
                        .child(
                            Button::new("backup-create-submit")
                                .primary()
                                .small()
                                .label(t!("backup_ui.page.create_submit"))
                                .loading(self.busy == Some(Op::Create))
                                .disabled(busy || self.password.check(cx) != PasswordCheck::Ready)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.submit_create(window, cx)
                                })),
                        ),
                )
                .into_any_element(),
            Mode::Restore {
                path,
                needs_password,
                summary,
            } => {
                let cancel = Button::new("backup-restore-cancel")
                    .ghost()
                    .small()
                    .label(t!("common.cancel"))
                    .disabled(busy)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.set_mode(Mode::Idle, window, cx)),
                    );
                let path_line = div()
                    .id("backup-restore-path")
                    .test_support()
                    .text_xs()
                    .font_family(termlet_terminal::fonts::DEFAULT_FAMILY)
                    .whitespace_normal()
                    .child(path.display().to_string());
                match summary {
                    Some(summary) => v_flex()
                        .gap_2()
                        .child(path_line)
                        .child(
                            v_flex()
                                .id("backup-summary")
                                .test_support()
                                .gap_1()
                                .p_3()
                                .border_1()
                                .border_color(border)
                                .rounded(radius)
                                .text_xs()
                                .child(div().text_sm().child(prose(t!(
                                    "backup_ui.page.summary_time",
                                    time = termlet_core::time::format_local(summary.created_at),
                                    app = summary.app.clone()
                                ))))
                                .child(div().text_color(muted).child(counts_text(&summary.counts)))
                                .child(
                                    div()
                                        .text_color(muted)
                                        .child(counts_more_text(&summary.counts)),
                                )
                                .child(div().text_color(muted).child(prose(if summary.audit {
                                    t!("backup_ui.page.summary_audit", count = summary.counts.audit)
                                } else {
                                    t!("backup_ui.page.summary_no_audit")
                                })))
                                .child(
                                    div()
                                        .pt_1()
                                        .text_color(warning)
                                        .child(prose(t!("backup_ui.page.restore_warning"))),
                                ),
                        )
                        .child(
                            h_flex().gap_2().justify_end().child(cancel).child(
                                Button::new("backup-restore-submit")
                                    .danger()
                                    .small()
                                    .label(t!("backup_ui.page.restore_submit"))
                                    .loading(self.busy == Some(Op::Restore))
                                    .disabled(busy)
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.restore(window, cx)),
                                    ),
                            ),
                        )
                        .into_any_element(),
                    None => {
                        let has_password = !self.restore_password.read(cx).value().is_empty();
                        v_flex()
                            .gap_2()
                            .child(path_line)
                            .child(
                                h_flex()
                                    .gap_2()
                                    .when(*needs_password, |this| {
                                        this.child(
                                            div().w(rems(18.)).child(
                                                Input::new(&self.restore_password)
                                                    .id("backup-restore-password")
                                                    .mask_toggle(),
                                            ),
                                        )
                                    })
                                    .child(div().flex_1())
                                    .child(cancel)
                                    .child(
                                        Button::new("backup-inspect")
                                            .primary()
                                            .small()
                                            .label(t!("backup_ui.page.inspect"))
                                            .loading(self.busy == Some(Op::Inspect))
                                            .disabled(busy || !*needs_password || !has_password)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.inspect(window, cx)
                                            })),
                                    ),
                            )
                            .into_any_element()
                    }
                }
            }
        };
        let message = self
            .full_message
            .as_ref()
            .map(|m| self.render_message(m, "backup-message", cx));
        v_flex()
            .id("backup-full")
            .gap_3()
            // 输入框中的 Enter（设置把它转成 Confirm）执行这里的一步，到这里为止
            .on_action(
                cx.listener(|this, _: &gpui_kit::base::actions::Confirm, window, cx| {
                    this.on_confirm(window, cx)
                }),
            )
            .child(
                div()
                    .text_sm()
                    .font_medium()
                    .child(t!("backup_ui.page.full_title")),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(prose(t!("backup_ui.page.full_description"))),
            )
            .child(body)
            .children(message)
    }

    fn render_snapshot_row(&self, info: &SnapshotInfo, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let tip: SharedString = format!(
            "{}\n{}",
            counts_text(&info.counts),
            counts_more_text(&info.counts)
        )
        .into();
        let target = info.clone();
        h_flex()
            .id(super::element_id("snapshot-row", &info.name))
            .test_support()
            .gap_2()
            .px_2()
            .py_1()
            .border_t_1()
            .border_color(theme.border)
            .text_xs()
            .child(
                div()
                    .w(rems(9.))
                    .flex_none()
                    .child(termlet_core::time::format_local(info.created_at)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(muted)
                    .child(reason_text(info.reason)),
            )
            .child(
                div()
                    .id(super::element_id("snapshot-hosts", &info.name))
                    .w(rems(3.5))
                    .flex_none()
                    .text_right()
                    .text_color(muted)
                    .child(info.counts.hosts.to_string())
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx)),
            )
            .child(
                div()
                    .w(rems(5.))
                    .flex_none()
                    .text_right()
                    .text_color(muted)
                    .child(crate::sftp::list_model::format_size(info.size)),
            )
            .child(
                h_flex().w(rems(5.)).flex_none().justify_end().child(
                    Button::new(super::element_id("snapshot-restore", &info.name))
                        .ghost()
                        .xsmall()
                        .label(t!("backup_ui.page.restore_snapshot"))
                        .disabled(self.busy.is_some())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.confirm_restore_snapshot(&target, window, cx)
                        })),
                ),
            )
            .into_any_element()
    }

    fn render_snapshots(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (muted, border, radius, danger) = (
            theme.muted_foreground,
            theme.border,
            theme.radius,
            theme.danger,
        );
        let busy = self.busy.is_some();
        let keep_invalid = parse_keep(&self.keep.read(cx).value()).is_none();
        let list: AnyElement = match &self.snapshots {
            None => div()
                .text_xs()
                .text_color(muted)
                .child(prose(t!("backup_ui.page.loading_snapshots")))
                .into_any_element(),
            Some(list) if list.is_empty() => div()
                .text_xs()
                .text_color(muted)
                .child(prose(t!("backup_ui.page.no_snapshots")))
                .into_any_element(),
            Some(list) => {
                let header = |text: SharedString| div().child(prose(text));
                v_flex()
                    .id("snapshot-list")
                    .test_support()
                    .border_1()
                    .border_color(border)
                    .rounded(radius)
                    .child(
                        h_flex()
                            .gap_2()
                            .px_2()
                            .py_1()
                            .text_xs()
                            .font_medium()
                            .text_color(muted)
                            .child(
                                header(t!("backup_ui.page.column.time"))
                                    .w(rems(9.))
                                    .flex_none(),
                            )
                            .child(header(t!("backup_ui.page.column.reason")).flex_1())
                            .child(
                                header(t!("backup_ui.page.column.hosts"))
                                    .w(rems(3.5))
                                    .flex_none()
                                    .text_right(),
                            )
                            .child(
                                header(t!("backup_ui.page.column.size"))
                                    .w(rems(5.))
                                    .flex_none()
                                    .text_right(),
                            )
                            .child(div().w(rems(5.)).flex_none()),
                    )
                    .children(list.iter().map(|info| self.render_snapshot_row(info, cx)))
                    .into_any_element()
            }
        };
        let message = self
            .snapshot_message
            .as_ref()
            .map(|m| self.render_message(m, "snapshot-message", cx));
        v_flex()
            .gap_3()
            .pt_4()
            .border_t_1()
            .border_color(border)
            .child(
                div()
                    .text_sm()
                    .font_medium()
                    .child(t!("backup_ui.page.snapshots_title")),
            )
            .child(
                Checkbox::new("backup-auto")
                    .label(t!("backup_ui.page.auto"))
                    .checked(self.auto)
                    .on_change(cx.listener(|this, checked: &bool, _, cx| {
                        this.auto = *checked;
                        cx.notify();
                    })),
            )
            .child(
                v_flex()
                    .gap_1()
                    .pl_6()
                    .child(
                        h_flex()
                            .gap_2()
                            .text_sm()
                            .child(t!("backup_ui.page.keep"))
                            .child(
                                div().w(rems(5.)).child(
                                    Input::new(&self.keep)
                                        .small()
                                        .id("backup-keep")
                                        .disabled(!self.auto),
                                ),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(if self.auto && keep_invalid {
                                danger
                            } else {
                                muted
                            })
                            .child(prose(t!(
                                "backup_ui.page.keep_hint",
                                min = KEEP_RANGE.0,
                                max = KEEP_RANGE.1
                            ))),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(prose(t!("backup_ui.page.snapshot_hint"))),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("snapshot-create")
                            .outline()
                            .small()
                            .label(t!("backup_ui.page.snapshot_now"))
                            .loading(self.busy == Some(Op::Snapshot))
                            .disabled(busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.snapshot_now(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("snapshot-open-folder")
                            .ghost()
                            .small()
                            .label(t!("backup_ui.page.open_folder"))
                            .on_click(cx.listener(|this, _, _, cx| this.open_folder(cx))),
                    ),
            )
            .children(message)
            .child(list)
    }
}

#[cfg(test)]
impl BackupPage {
    /// 列出的快照数；还在读取时为 `None`。
    pub(super) fn snapshot_count(&self) -> Option<usize> {
        self.snapshots.as_ref().map(Vec::len)
    }
}

impl Render for BackupPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("backup-page")
            .test_support()
            .gap_4()
            .child(self.render_full(cx))
            .child(self.render_schedule(cx))
            .child(self.render_snapshots(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_must_be_in_range() {
        assert_eq!(parse_keep(" 7 "), Some(7));
        assert_eq!(parse_keep("1"), Some(1));
        assert_eq!(parse_keep("30"), Some(30));
        assert_eq!(parse_keep("0"), None);
        assert_eq!(parse_keep("31"), None);
        assert_eq!(parse_keep("七"), None);
        assert_eq!(parse_keep(""), None);
    }

    #[test]
    fn every_snapshot_reason_has_text() {
        for reason in SnapshotReason::ALL {
            assert!(!reason_text(reason).starts_with("backup_ui."));
        }
    }
}
