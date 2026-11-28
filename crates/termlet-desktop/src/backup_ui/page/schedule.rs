//! “备份”页中的定时备份：按计划（每天或每周的某个时间）自动创建完整备份，存到指定的文件夹，保留最近 N 份。
//!
//! 没有设置时只有“设置定时备份…”；设置表单：文件夹、每天 / 每周与星期几、时间、保留份数、备份密码（两次输入；
//! 更改设置时留空沿用原来的）。设置之后显示计划、下一次的时间、上次的结果（时间、成功或失败的原因），可以立即备份、
//! 更改设置、打开文件夹、停止。这些操作都立即执行，不随设置的“保存”。

use std::path::PathBuf;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::radio::RadioGroup;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, PathPromptOptions, SharedString, Styled as _, Subscription, Task,
    TestSupportExt as _, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::backup::schedule::{DEFAULT_KEEP, KEEP_RANGE};
use termlet_core::backup::{
    BackupSchedule, Frequency, RunRecord, ScheduleConfig, ScheduleInput, ScheduleStatus,
};
use termlet_core::runtime;
use termlet_core::time::format_local;

use super::{BackupPage, Message, Op};
use crate::app_state::AppState;
use crate::backup_ui::{PasswordCheck, PasswordPair, errors};
use crate::choice::{Choice, ChoiceState, choice_state, chosen};
use crate::prose::prose;
use crate::t;

/// 新设置时的默认时间：凌晨 3 点。
const DEFAULT_MINUTE: u16 = 3 * 60;

/// 时间：`3:05`、`03:05`（中文输入法的全角冒号也认）。返回当天的第几分钟。
pub(super) fn parse_time(text: &str) -> Option<u16> {
    let text = text.trim().replace('：', ":");
    let (h, m) = text.split_once(':')?;
    if h.is_empty() || h.len() > 2 || m.len() != 2 {
        return None;
    }
    let (h, m): (u16, u16) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

pub(super) fn format_time(minute_of_day: u16) -> String {
    format!("{:02}:{:02}", minute_of_day / 60, minute_of_day % 60)
}

/// 保留份数：范围内的整数。
pub(super) fn parse_keep(text: &str) -> Option<u32> {
    text.trim()
        .parse::<u32>()
        .ok()
        .filter(|n| KEEP_RANGE.contains(n))
}

fn weekday_name(weekday: u8) -> SharedString {
    match weekday {
        1 => t!("backup_ui.schedule.monday"),
        2 => t!("backup_ui.schedule.tuesday"),
        3 => t!("backup_ui.schedule.wednesday"),
        4 => t!("backup_ui.schedule.thursday"),
        5 => t!("backup_ui.schedule.friday"),
        6 => t!("backup_ui.schedule.saturday"),
        _ => t!("backup_ui.schedule.sunday"),
    }
}

fn weekday_choices() -> Vec<Choice<u8>> {
    (1..=7).map(|d| Choice::new(weekday_name(d), d)).collect()
}

/// 计划的说法：“每天 03:00”“每周星期一 03:00”。
fn plan_text(config: &ScheduleConfig) -> SharedString {
    let time = format_time(config.minute_of_day);
    match config.frequency {
        Frequency::Daily => t!("backup_ui.schedule.plan_daily", time = time),
        Frequency::Weekly { weekday } => t!(
            "backup_ui.schedule.plan_weekly",
            weekday = weekday_name(weekday),
            time = time
        ),
    }
}

/// 上次的结果。
fn last_text(record: &RunRecord) -> (SharedString, bool) {
    let time = format_local(record.at);
    match (&record.error, &record.file) {
        (Some(error), _) => (
            t!(
                "backup_ui.schedule.last_failed",
                time = time,
                error = errors::describe_backup(error)
            ),
            true,
        ),
        (None, Some(file)) if record.deleted > 0 => (
            t!(
                "backup_ui.schedule.last_ok_pruned",
                time = time,
                file = file,
                count = record.deleted
            ),
            false,
        ),
        (None, file) => (
            t!(
                "backup_ui.schedule.last_ok",
                time = time,
                file = file.clone().unwrap_or_default()
            ),
            false,
        ),
    }
}

/// 定时备份的表单与状态（[`BackupPage`] 的一部分）。
pub(super) struct ScheduleView {
    service: BackupSchedule,
    status: ScheduleStatus,
    /// 正在设置（新设置或更改）
    editing: bool,
    folder: Entity<InputState>,
    time: Entity<InputState>,
    keep: Entity<InputState>,
    weekly: bool,
    weekday: Entity<ChoiceState<u8>>,
    password: PasswordPair,
    message: Option<Message>,
    _watch: Task<()>,
    _pick: Option<Task<()>>,
}

impl ScheduleView {
    /// 清空输入到一半的定时备份密码（锁定时）。
    pub(super) fn clear_password(&self, window: &mut Window, cx: &mut App) {
        self.password.clear(window, cx);
    }

    /// 建好表单并订阅核心的状态；返回的订阅由页面保存。
    pub(super) fn new(
        window: &mut Window,
        cx: &mut Context<BackupPage>,
    ) -> (Self, Vec<Subscription>) {
        let service = AppState::global(cx).schedule.clone();
        let mut receiver = service.subscribe();
        let status = receiver.borrow_and_update().clone();
        // watch 的接收端与执行器无关，在 GPUI 的任务里直接等待
        let watch = cx.spawn(async move |this, cx| {
            while receiver.changed().await.is_ok() {
                let status = receiver.borrow_and_update().clone();
                if this
                    .update(cx, |page, cx| {
                        page.schedule.status = status;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let input = |value: String,
                     placeholder: SharedString,
                     window: &mut Window,
                     cx: &mut Context<BackupPage>| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        let folder = input(
            String::new(),
            t!("backup_ui.schedule.folder_placeholder"),
            window,
            cx,
        );
        let time = input(format_time(DEFAULT_MINUTE), "03:00".into(), window, cx);
        let keep = input(
            DEFAULT_KEEP.to_string(),
            SharedString::default(),
            window,
            cx,
        );
        let weekday = choice_state(weekday_choices(), &1u8, false, window, cx);
        let (password, mut subscriptions) = PasswordPair::new(window, cx);
        for state in [&folder, &time, &keep] {
            subscriptions.push(cx.subscribe(state, |page, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    page.schedule.message = None;
                    cx.notify();
                }
            }));
        }
        (
            Self {
                service,
                status,
                editing: false,
                folder,
                time,
                keep,
                weekly: false,
                weekday,
                password,
                message: None,
                _watch: watch,
                _pick: None,
            },
            subscriptions,
        )
    }

    /// 表单中的问题（提交前就能发现的；文件夹是否存在等由核心检查）。
    fn input(&self, cx: &App) -> Result<ScheduleInput, SharedString> {
        let folder = self.folder.read(cx).value().trim().to_string();
        if folder.is_empty() {
            return Err(t!("backup_ui.schedule.folder_required"));
        }
        let minute_of_day = parse_time(&self.time.read(cx).value())
            .ok_or_else(|| t!("backup_ui.schedule.time_invalid"))?;
        let keep = parse_keep(&self.keep.read(cx).value()).ok_or_else(|| {
            t!(
                "backup_ui.schedule.keep_invalid",
                min = KEEP_RANGE.start(),
                max = KEEP_RANGE.end()
            )
        })?;
        let frequency = if self.weekly {
            Frequency::Weekly {
                weekday: chosen(&self.weekday, cx).unwrap_or(1),
            }
        } else {
            Frequency::Daily
        };
        // 更改设置时两个密码框都空着：沿用原来的密码
        let configured = self.status.config.is_some() && !self.status.password_lost;
        let password = match self.password.check(cx) {
            PasswordCheck::Empty if configured => None,
            PasswordCheck::Ready => self.password.secret(cx),
            PasswordCheck::Empty | PasswordCheck::TooShort => {
                return Err(t!(
                    "backup_ui.password.too_short",
                    min = termlet_core::backup::MIN_PASSWORD_LEN
                ));
            }
            PasswordCheck::Incomplete => return Err(t!("backup_ui.schedule.password_repeat")),
            PasswordCheck::Mismatch => return Err(t!("backup_ui.password.mismatch")),
        };
        Ok(ScheduleInput {
            folder: PathBuf::from(folder),
            frequency,
            minute_of_day,
            keep,
            password,
        })
    }
}

impl BackupPage {
    /// 开始设置（或更改）定时备份：表单预填当前的设置，密码留空。
    fn schedule_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let config = self.schedule.status.config.clone();
        let view = &mut self.schedule;
        let set = |state: &Entity<InputState>, value: String, window: &mut Window, cx: &mut App| {
            state.update(cx, |s, cx| s.set_value(value, window, cx));
        };
        match &config {
            Some(config) => {
                set(
                    &view.folder,
                    config.folder.display().to_string(),
                    window,
                    cx,
                );
                set(&view.time, format_time(config.minute_of_day), window, cx);
                set(&view.keep, config.keep.to_string(), window, cx);
                view.weekly = matches!(config.frequency, Frequency::Weekly { .. });
                if let Frequency::Weekly { weekday } = config.frequency {
                    view.weekday
                        .update(cx, |s, cx| s.set_selected_value(&weekday, window, cx));
                }
            }
            None => {
                set(&view.time, format_time(DEFAULT_MINUTE), window, cx);
                set(&view.keep, DEFAULT_KEEP.to_string(), window, cx);
            }
        }
        if view.folder.read(cx).value().is_empty() {
            set(
                &view.folder,
                crate::backup_ui::default_dir().display().to_string(),
                window,
                cx,
            );
        }
        view.password.clear(window, cx);
        view.editing = true;
        view.message = None;
        cx.notify();
    }

    fn schedule_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.schedule.editing = false;
        self.schedule.message = None;
        self.schedule.password.clear(window, cx);
        cx.notify();
    }

    fn schedule_browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        self.schedule._pick = Some(cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(dir) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.schedule.folder.update(cx, |state, cx| {
                    state.set_value(dir.display().to_string(), window, cx)
                });
                cx.notify();
            });
        }));
    }

    /// 保存设置（检查文件夹、计划与密码）。
    fn schedule_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() || !self.schedule.editing {
            return;
        }
        let input = match self.schedule.input(cx) {
            Ok(input) => input,
            Err(text) => {
                self.schedule.message = Some(Message::error(text));
                cx.notify();
                return;
            }
        };
        let service = self.schedule.service.clone();
        tracing::info!(keep = input.keep, "设置定时备份");
        let job = runtime::spawn(async move { service.configure(input).await });
        self.busy = Some(Op::ScheduleSave);
        self.schedule.message = None;
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = None;
                match result {
                    Ok(Ok(config)) => {
                        this.schedule.editing = false;
                        this.schedule.password.clear(window, cx);
                        this.schedule.status.config = Some(config);
                        this.schedule.status.password_lost = false;
                        this.schedule.message = Some(Message::ok(t!("backup_ui.schedule.saved")));
                    }
                    Ok(Err(e)) => {
                        this.schedule.message = Some(Message::error(errors::describe_backup(&e)))
                    }
                    Err(e) => this.schedule.message = Some(Message::error(e.to_string().into())),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn schedule_run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() || self.schedule.status.running {
            return;
        }
        let service = self.schedule.service.clone();
        tracing::info!("立即进行定时备份");
        let job = runtime::spawn(async move { service.run_now().await });
        self.busy = Some(Op::ScheduleRun);
        self.schedule.message = None;
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                this.busy = None;
                // 成功、失败都记在上次的结果中；这里只报告没能开始的（保险库锁定、密码解不开）
                match result {
                    Ok(Ok(record)) => this.schedule.status.last = Some(record),
                    Ok(Err(e)) => {
                        this.schedule.message = Some(Message::error(errors::describe_backup(&e)))
                    }
                    Err(e) => this.schedule.message = Some(Message::error(e.to_string().into())),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn schedule_disable(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let service = self.schedule.service.clone();
        tracing::info!("停止定时备份");
        let job = runtime::spawn(async move { service.disable().await });
        self.busy = Some(Op::ScheduleDisable);
        self.schedule.message = None;
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                this.busy = None;
                match result {
                    Ok(Ok(())) => this.schedule.status = ScheduleStatus::default(),
                    Ok(Err(e)) => {
                        this.schedule.message = Some(Message::error(errors::describe_backup(&e)))
                    }
                    Err(e) => this.schedule.message = Some(Message::error(e.to_string().into())),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn schedule_open_folder(&self, cx: &mut App) {
        if let Some(config) = &self.schedule.status.config {
            cx.open_url(&config.folder.display().to_string());
        }
    }

    /// Enter（在定时备份的输入框中）：保存设置。
    fn schedule_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.schedule.editing {
            self.schedule_save(window, cx);
        }
    }

    // ------------------------------------------------------------ 渲染

    fn render_schedule_form(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let busy = self.busy.is_some();
        let view = &self.schedule;
        let configured = view.status.config.is_some() && !view.status.password_lost;
        let label = |text: SharedString| div().w(rems(6.)).flex_none().text_sm().child(text);
        v_flex()
            .gap_2()
            .max_w(rems(36.))
            .child(
                h_flex()
                    .gap_2()
                    .child(label(t!("backup_ui.schedule.folder")))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&view.folder).small().id("schedule-folder")),
                    )
                    .child(
                        Button::new("schedule-browse")
                            .outline()
                            .small()
                            .label(t!("backup_ui.schedule.browse"))
                            .disabled(busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.schedule_browse(window, cx)),
                            ),
                    ),
            )
            .child(
                div()
                    .pl(rems(6.5))
                    .text_xs()
                    .text_color(muted)
                    .child(prose(t!("backup_ui.schedule.folder_hint"))),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(label(t!("backup_ui.schedule.frequency")))
                    // 单选组默认占满整行：收成内容的宽度，时间紧跟在后面
                    .child(
                        div().flex_none().child(
                            RadioGroup::horizontal("schedule-frequency")
                                .children([
                                    t!("backup_ui.schedule.daily"),
                                    t!("backup_ui.schedule.weekly"),
                                ])
                                .selected_index(Some(usize::from(view.weekly)))
                                .disabled(busy)
                                .on_change(cx.listener(|this, ix: &usize, _, cx| {
                                    this.schedule.weekly = *ix == 1;
                                    cx.notify();
                                })),
                        ),
                    )
                    .when(view.weekly, |row| {
                        row.child(
                            div().w(rems(8.)).child(
                                crate::choice::select(&view.weekday)
                                    .id("schedule-weekday")
                                    .small()
                                    .disabled(busy),
                            ),
                        )
                    })
                    .child(div().text_sm().child(t!("backup_ui.schedule.at")))
                    .child(
                        div()
                            .w(rems(5.))
                            .child(Input::new(&view.time).small().id("schedule-time")),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(label(t!("backup_ui.schedule.keep")))
                    .child(
                        div()
                            .w(rems(5.))
                            .child(Input::new(&view.keep).small().id("schedule-keep")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(muted)
                            .child(prose(t!(
                                "backup_ui.schedule.keep_hint",
                                min = KEEP_RANGE.start(),
                                max = KEEP_RANGE.end()
                            ))),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_sm().child(t!("backup_ui.schedule.password")))
                    .child(view.password.render("schedule", cx))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(prose(if configured {
                                t!("backup_ui.schedule.password_keep_hint")
                            } else {
                                t!("backup_ui.schedule.password_hint")
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("schedule-cancel")
                            .ghost()
                            .small()
                            .label(t!("common.cancel"))
                            .disabled(busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.schedule_cancel(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("schedule-save")
                            .primary()
                            .small()
                            .label(t!("backup_ui.schedule.save"))
                            .loading(self.busy == Some(Op::ScheduleSave))
                            .disabled(busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.schedule_save(window, cx)),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_schedule_status(
        &self,
        config: &ScheduleConfig,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (muted, danger, warning, border, radius) = (
            theme.muted_foreground,
            theme.danger,
            theme.warning,
            theme.border,
            theme.radius,
        );
        let busy = self.busy.is_some();
        let status = &self.schedule.status;
        let running = status.running || self.busy == Some(Op::ScheduleRun);
        let next = status.next.map(|at| {
            if at <= termlet_core::time::now_ms() {
                t!("backup_ui.schedule.next_soon")
            } else {
                t!("backup_ui.schedule.next", time = format_local(at))
            }
        });
        let last: AnyElement = if running {
            h_flex()
                .gap_2()
                .child(Spinner::new().small())
                .child(prose(t!("backup_ui.schedule.running")))
                .into_any_element()
        } else {
            match &status.last {
                Some(record) => {
                    let (text, failed) = last_text(record);
                    div()
                        .id("schedule-last")
                        .test_support()
                        .text_color(if failed { danger } else { muted })
                        .child(prose(text))
                        .into_any_element()
                }
                None => div()
                    .text_color(muted)
                    .child(prose(t!("backup_ui.schedule.never")))
                    .into_any_element(),
            }
        };
        let card = v_flex()
            .id("schedule-status")
            .test_support()
            .gap_1()
            .p_3()
            .rounded(radius)
            .border_1()
            .border_color(border)
            .text_sm()
            .child(prose(t!(
                "backup_ui.schedule.summary",
                plan = plan_text(config),
                folder = config.folder.display(),
                keep = config.keep
            )))
            .when_some(next, |card, next| {
                card.child(div().text_color(muted).child(prose(next)))
            })
            .child(last)
            .when(status.password_lost, |card| {
                card.child(
                    h_flex()
                        .gap_2()
                        .items_start()
                        .text_color(warning)
                        .child(Icon::new(IconName::TriangleAlert).small().flex_none())
                        .child(prose(t!("backup_ui.schedule.password_lost"))),
                )
            });
        let actions = h_flex()
            .gap_2()
            .child(
                Button::new("schedule-run")
                    .outline()
                    .small()
                    .label(t!("backup_ui.schedule.run_now"))
                    .loading(running)
                    .disabled(busy || running || status.password_lost)
                    .on_click(cx.listener(|this, _, window, cx| this.schedule_run(window, cx))),
            )
            .child(
                Button::new("schedule-edit")
                    .outline()
                    .small()
                    .label(t!("backup_ui.schedule.edit"))
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, window, cx| this.schedule_edit(window, cx))),
            )
            .child(
                Button::new("schedule-open-folder")
                    .ghost()
                    .small()
                    .label(t!("backup_ui.page.open_folder"))
                    .on_click(cx.listener(|this, _, _, cx| this.schedule_open_folder(cx))),
            )
            .child(div().flex_1())
            .child(
                Button::new("schedule-disable")
                    .ghost()
                    .small()
                    .label(t!("backup_ui.schedule.disable"))
                    .loading(self.busy == Some(Op::ScheduleDisable))
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, window, cx| this.schedule_disable(window, cx))),
            );
        v_flex()
            .gap_2()
            .child(card)
            .child(actions)
            .into_any_element()
    }

    pub(super) fn render_schedule(
        &self,
        hits: &crate::settings::search::Hits,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let busy = self.busy.is_some();
        let body = match (&self.schedule.status.config, self.schedule.editing) {
            (_, true) => self.render_schedule_form(cx),
            (Some(config), false) => {
                let config = config.clone();
                self.render_schedule_status(&config, cx)
            }
            (None, false) => h_flex()
                .child(
                    Button::new("schedule-setup")
                        .outline()
                        .small()
                        .label(t!("backup_ui.schedule.setup"))
                        .disabled(busy)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.schedule_edit(window, cx)),
                        ),
                )
                .into_any_element(),
        };
        let message = self
            .schedule
            .message
            .as_ref()
            .map(|m| self.render_message(m, "schedule-message", cx));
        v_flex()
            .id("backup-schedule")
            .gap_3()
            .pt_4()
            .border_t_1()
            .border_color(border)
            // 输入框中的 Enter（设置把它转成 Confirm）保存定时备份的设置，到这里为止
            .on_action(
                cx.listener(|this, _: &gpui_kit::base::actions::Confirm, window, cx| {
                    this.schedule_confirm(window, cx)
                }),
            )
            .child(
                hits.mark(
                    div()
                        .text_sm()
                        .font_medium()
                        .child(t!("backup_ui.schedule.title")),
                    "backup-schedule",
                    cx,
                ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(prose(t!("backup_ui.schedule.description"))),
            )
            .child(body)
            .children(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_are_parsed() {
        assert_eq!(parse_time("03:00"), Some(180));
        assert_eq!(parse_time(" 3:05 "), Some(185));
        assert_eq!(parse_time("23：59"), Some(23 * 60 + 59));
        assert_eq!(parse_time("0:00"), Some(0));
        for bad in ["24:00", "3:60", "3", "3:5", "abc", "", ":30", "123:00"] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
        assert_eq!(format_time(185), "03:05");
    }

    #[test]
    fn keep_is_in_range() {
        assert_eq!(parse_keep(" 7 "), Some(7));
        assert_eq!(parse_keep("100"), Some(100));
        assert_eq!(parse_keep("0"), None);
        assert_eq!(parse_keep("101"), None);
    }

    #[test]
    fn every_weekday_has_a_name() {
        for d in 1..=7 {
            assert!(!weekday_name(d).starts_with("backup_ui."));
        }
    }
}
