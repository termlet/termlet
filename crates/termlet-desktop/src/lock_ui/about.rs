//! 设置中的三节：更新（自动检查更新、状态、检查更新、重启并安装、前往下载）在“关于”页；隐私（发送匿名使用统计、
//! 查看发送的内容…、隐私说明、自动发送崩溃报告：“什么数据会离开这台电脑”放在一起）与诊断和崩溃报告（复制诊断信息、
//! 导出诊断信息…、打开日志目录、崩溃记录…、日志的详细程度）在“隐私与诊断”页（[`AboutPart`]）。
//!
//! “自动检查更新”“发送匿名使用统计”“自动发送崩溃报告”“日志的详细程度”是草稿，随设置的“保存”写入（日志的详细
//! 程度保存后立即生效，关掉使用统计后删除本机的 `usage.json`）；其余操作立即执行。“自动发送崩溃报告”只在有接收地址
//! （签名的远程配置给出，打开这一页时读取）或已经开启时显示。使用统计被环境变量关闭时开关显示为关、不能改，远程配置
//! 暂停发送时照常显示并说明“暂停发送”。

use std::path::PathBuf;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, ClipboardItem, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task,
    TestSupportExt as _, Window, div, prelude::FluentBuilder as _,
};
use termlet_core::diag::{Diagnostics, Scope};
use termlet_core::model::{LocalSettings, LogLevel, Settings};
use termlet_core::update::{CheckTrigger, UpdateErrorKind, UpdateStatus};

use super::update::status_text;
use crate::app_state::AppState;
use crate::choice::{Choice, ChoiceState, choice_state, chosen};
use crate::prose::prose;
use crate::settings::search::Hits;
use crate::t;

/// 设置“关于”页最上面的一块：logo、名称、版本（可复制版本信息）、简介、署名，官网等链接与版权。
pub(crate) fn app_info(cx: &App) -> impl IntoElement + use<> {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let link = |id: &'static str, label: SharedString, url: &'static str| {
        Button::new(id)
            .link()
            .small()
            .icon(IconName::ExternalLink)
            .label(label)
            .on_click(move |_, _, cx| cx.open_url(url))
    };
    let licenses = third_party_licenses().map(|dir| dir.display().to_string());
    let portable = AppState::is_portable(cx);
    v_flex()
        .gap_3()
        .child(
            h_flex()
                .gap_3()
                .items_start()
                .child(crate::brand::logo(gpui_kit::rems(2.5)))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(div().text_lg().font_semibold().child(t!("app.name")))
                        .child(
                            h_flex()
                                .gap_2()
                                .flex_wrap()
                                .child(
                                    div()
                                        .id("settings-version")
                                        .text_sm()
                                        .text_color(muted)
                                        .child(t!(
                                            "settings.version",
                                            version = termlet_core::VERSION
                                        )),
                                )
                                .when(portable, |this| {
                                    this.child(
                                        div()
                                            .id("about-portable")
                                            .test_support()
                                            .px_1p5()
                                            .rounded(theme.radius)
                                            .bg(theme.secondary)
                                            .text_xs()
                                            .text_color(muted)
                                            .child(t!("settings.about.portable")),
                                    )
                                })
                                .child(
                                    Button::new("about-copy-version")
                                        .ghost()
                                        .xsmall()
                                        .icon(IconName::Copy)
                                        .label(t!("settings.about.copy_version"))
                                        .on_click(move |_, window, cx| {
                                            let info = version_info(
                                                termlet_core::VERSION,
                                                portable,
                                                termlet_core::diag::system_info(),
                                            );
                                            cx.write_to_clipboard(ClipboardItem::new_string(info));
                                            crate::notify::show(
                                                crate::notify::Notice::success(t!(
                                                    "settings.about.version_copied"
                                                )),
                                                window,
                                                cx,
                                            );
                                        }),
                                ),
                        )
                        .child(div().text_sm().child(prose(t!("settings.slogan")))),
                ),
        )
        .child(
            h_flex()
                .id("about-links")
                .test_support()
                .flex_wrap()
                .gap_x_3()
                .gap_y_1()
                .child(link(
                    "about-website",
                    t!("settings.about.website"),
                    crate::website::home(),
                ))
                .child(link(
                    "about-changelog",
                    t!("settings.about.changelog"),
                    crate::website::changelog(),
                ))
                .child(link(
                    "about-feedback",
                    t!("settings.about.feedback"),
                    crate::website::FEEDBACK_MAIL,
                ))
                .child(link(
                    "about-license",
                    t!("settings.about.license"),
                    crate::website::license(),
                ))
                .child(link(
                    "about-source",
                    t!("settings.about.source"),
                    crate::website::SOURCE_CODE,
                ))
                // 随程序分发的第三方许可（安装目录的 licenses）；开发版没有，不显示
                .when_some(licenses, |this, dir| {
                    this.child(
                        Button::new("about-third-party")
                            .link()
                            .small()
                            .icon(IconName::FolderOpen)
                            .label(t!("settings.about.third_party"))
                            .on_click(move |_, _, cx| cx.open_url(&dir)),
                    )
                }),
        )
        .child(
            div()
                .id("about-copyright")
                .test_support()
                .text_xs()
                .text_color(muted)
                .child(t!("settings.about.copyright")),
        )
}

/// 复制的版本信息（反馈问题时贴出来）：版本（便携版另外注明），系统的名称、版本与架构。
pub(crate) fn version_info(
    version: &str,
    portable: bool,
    system: &termlet_core::diag::SystemInfo,
) -> String {
    let os = [system.name.as_str(), system.version.as_str()]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    let os = if os.is_empty() { system.os.clone() } else { os };
    let edition = if portable { " (portable)" } else { "" };
    format!(
        "Termlet {version}{edition}\n{os} ({}, {})",
        system.os, system.arch
    )
}

/// 随程序分发的第三方许可所在的目录：Windows、Linux 在程序旁边的 `licenses`，macOS 在应用包的
/// `Contents/Resources/licenses`；不存在（开发版）时为 `None`。
pub(crate) fn licenses_dir(exe: &std::path::Path) -> Option<PathBuf> {
    let dir = exe.parent()?;
    [
        dir.join("licenses"),
        dir.join("..").join("Resources").join("licenses"),
    ]
    .into_iter()
    .find(|candidate| candidate.is_dir())
}

/// 这个程序的第三方许可目录（第一次调用时找）。
fn third_party_licenses() -> Option<PathBuf> {
    static DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|exe| licenses_dir(&exe))
    })
    .clone()
}

/// [`AboutExtras`] 显示哪几节。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AboutPart {
    /// “关于”页：更新
    Updates,
    /// “隐私与诊断”页：隐私、诊断与崩溃报告
    Privacy,
}

pub(crate) struct AboutExtras {
    /// 显示哪几节（设置换页时给出）
    part: AboutPart,
    auto_check_update: bool,
    log_level: Entity<ChoiceState<LogLevel>>,
    /// 发送匿名使用统计
    usage: bool,
    crash_report: bool,
    /// 打开时已经开启：没有接收地址也显示开关，便于关闭
    initial_crash_report: bool,
    /// 正在生成诊断信息（复制、导出的结果与其他操作的一样写在设置底部，见 [`crate::notify::show`]）
    diag_busy: bool,
    _diag: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl AboutExtras {
    pub(crate) fn new(
        settings: &Settings,
        local: &LocalSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let log_level = choice_state(
            vec![
                Choice::new(t!("lock_ui.diag.log_level.normal"), LogLevel::Normal),
                Choice::new(t!("lock_ui.diag.log_level.detailed"), LogLevel::Detailed),
                Choice::new(t!("lock_ui.diag.log_level.full"), LogLevel::Full),
            ],
            &local.logs.level,
            false,
            window,
            cx,
        );
        let subscriptions = super::update_model(cx)
            .map(|model| cx.observe(&model, |_, _, cx| cx.notify()))
            .into_iter()
            .collect();
        // 是否显示“自动发送崩溃报告”要看有没有接收地址
        super::ensure_crash_endpoint(cx);
        Self {
            part: AboutPart::Updates,
            auto_check_update: settings.general.auto_check_update,
            log_level,
            usage: local.usage.enabled,
            crash_report: local.logs.crash_report,
            initial_crash_report: local.logs.crash_report,
            diag_busy: false,
            _diag: None,
            _subscriptions: subscriptions,
        }
    }

    /// 正在复制、导出诊断信息（这时设置不重新载入这一节）。
    pub(crate) fn is_busy(&self) -> bool {
        self.diag_busy
    }

    /// 换成显示 `part` 的几节。
    pub(crate) fn set_part(&mut self, part: AboutPart, cx: &mut Context<Self>) {
        if self.part != part {
            self.part = part;
            cx.notify();
        }
    }

    /// 写进设置（随设置的“保存”）。
    pub(crate) fn apply(&self, settings: &mut Settings, local: &mut LocalSettings, cx: &App) {
        settings.general.auto_check_update = self.auto_check_update;
        if let Some(level) = chosen(&self.log_level, cx) {
            local.logs.level = level;
        }
        // 被环境变量关闭时开关改不了，草稿就是原来的值
        local.usage.enabled = self.usage;
        local.logs.crash_report = self.crash_report;
    }

    fn check_now(&mut self, cx: &mut Context<Self>) {
        if let Some(services) = super::services(cx) {
            super::spawn_check(&services.updater, CheckTrigger::Manual);
        }
    }

    /// 诊断信息：界面与运行中的模块的信息加在核心收集的内容之后。
    fn diagnostics(cx: &App) -> Option<Diagnostics> {
        let services = super::services(cx)?;
        let app = AppState::global(cx);
        let mut diag = Diagnostics::new(AppState::store(cx), app.data_dir.clone())
            .client("language", crate::i18n::language().code())
            .client(
                "theme",
                if cx.theme().is_dark() {
                    "dark"
                } else {
                    "light"
                },
            )
            .client("windows", cx.windows().len())
            .section("appLockEnabled", super::is_enabled(cx))
            .section(
                "update",
                status_name(&services.update_model.read(cx).state().status),
            );
        if let Some(hook) = services.diagnostics_hook.clone() {
            diag = hook(diag, cx);
        }
        Some(diag)
    }

    fn copy_diagnostics(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(diag) = Self::diagnostics(cx) else {
            return;
        };
        self.diag_busy = true;
        cx.notify();
        self._diag = Some(cx.spawn_in(window, async move |this, cx| {
            let text = diag.text(Scope::Copy).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.diag_busy = false;
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                crate::notify::show(
                    crate::notify::Notice::success(t!("lock_ui.diag.copied")),
                    window,
                    cx,
                );
                cx.notify();
            });
        }));
    }

    fn export_diagnostics(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(diag) = Self::diagnostics(cx) else {
            return;
        };
        let dir = std::env::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let name = termlet_core::diag::default_file_name(termlet_core::time::now_ms());
        let picked = cx.prompt_for_new_path(&dir, Some(&name));
        self.diag_busy = true;
        cx.notify();
        self._diag = Some(cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = picked.await else {
                let _ = this.update(cx, |this, cx| {
                    this.diag_busy = false;
                    cx.notify();
                });
                return;
            };
            let result = diag.export(path.clone()).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.diag_busy = false;
                let notice = match result {
                    Ok(()) => crate::notify::Notice::success(t!(
                        "lock_ui.diag.saved",
                        path = path.display().to_string()
                    )),
                    Err(e) => {
                        tracing::warn!("导出诊断信息失败：{e}");
                        crate::notify::Notice::error(t!(
                            "lock_ui.diag.export_failed",
                            error = e.to_string()
                        ))
                    }
                };
                crate::notify::show(notice, window, cx);
                cx.notify();
            });
        }));
    }

    fn render_updates(&self, hits: &Hits, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, warning) = (theme.muted_foreground, theme.warning);
        let state = super::update_model(cx).map(|model| model.read(cx).state().clone());
        let (text, error, status) = match &state {
            Some(state) => (
                status_text(state),
                match &state.status {
                    UpdateStatus::Failed { error, .. } => Some(SharedString::from(error.clone())),
                    _ => None,
                },
                Some(state.status.clone()),
            ),
            None => (
                t!("lock_ui.update.current", version = termlet_core::VERSION),
                None,
                None,
            ),
        };
        let busy = matches!(
            status,
            Some(UpdateStatus::Checking | UpdateStatus::Downloading { .. })
        );
        let restart = matches!(status, Some(UpdateStatus::Downloaded { .. }));
        let download = matches!(
            status,
            Some(
                UpdateStatus::Available { .. }
                    | UpdateStatus::Failed {
                        kind: UpdateErrorKind::Signature,
                        ..
                    }
            )
        );
        // 只提示新版本的平台：“前往下载”代替“检查更新”
        let available = matches!(status, Some(UpdateStatus::Available { .. }));
        // 下载中：进度条下面写已下载 / 总大小与速度（与标题栏的提示同一套格式）
        let progress = match &status {
            Some(UpdateStatus::Downloading {
                percent,
                downloaded,
                total,
                ..
            }) => {
                let speed = super::update_model(cx)
                    .and_then(|model| model.read(cx).meter().speed())
                    .map(super::progress::format_speed);
                let text = [
                    Some(super::progress::format_progress(*downloaded, *total)),
                    speed,
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
                Some((*percent, text))
            }
            _ => None,
        };
        let failed = error.is_some();
        v_flex()
            .gap_2()
            .child(
                hits.mark(
                    div()
                        .text_sm()
                        .font_medium()
                        .child(t!("lock_ui.update.section")),
                    "about-updates",
                    cx,
                ),
            )
            .child(
                Checkbox::new("about-auto-update")
                    .label(t!("lock_ui.update.auto_check"))
                    .checked(self.auto_check_update)
                    .on_change(cx.listener(|this, checked: &bool, _, cx| {
                        this.auto_check_update = *checked;
                        cx.notify();
                    })),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        div()
                            .id("update-status")
                            .test_support()
                            .min_w_0()
                            .text_xs()
                            .text_color(if failed { warning } else { muted })
                            .child(prose(text))
                            // 完整的说明放在悬停提示中，详细内容见日志
                            .when_some(error, |this, error| {
                                this.tooltip(move |window, cx| {
                                    Tooltip::new(error.clone()).build(window, cx)
                                })
                            }),
                    )
                    .when(restart, |this| {
                        this.child(
                            Button::new("update-restart")
                                .primary()
                                .small()
                                .label(t!("lock_ui.update.restart"))
                                .on_click(|_, window, cx| super::restart_and_install(window, cx)),
                        )
                    })
                    .when(download, |this| {
                        this.child(
                            Button::new("update-download")
                                .primary()
                                .small()
                                .icon(IconName::ExternalLink)
                                .label(t!("lock_ui.update.download"))
                                .on_click(|_, _, cx| super::update::open_download_page(cx)),
                        )
                    })
                    .when(!restart && !available, |this| {
                        this.child(
                            Button::new("update-check")
                                .outline()
                                .small()
                                .label(t!("lock_ui.update.check"))
                                .disabled(busy || state.is_none())
                                .on_click(cx.listener(|this, _, _, cx| this.check_now(cx))),
                        )
                    }),
            )
            .when_some(progress, |this, (percent, text)| {
                this.child(
                    h_flex()
                        .gap_2()
                        .child(
                            Progress::new("update-progress")
                                .flex_1()
                                .value(f32::from(percent)),
                        )
                        .child(
                            div()
                                .id("update-progress-text")
                                .test_support()
                                .flex_none()
                                .text_xs()
                                .text_color(muted)
                                .child(text),
                        ),
                )
            })
    }

    /// 隐私：发送匿名使用统计（说明、查看发送的内容…、隐私说明）与自动发送崩溃报告。
    fn render_privacy(&self, hits: &Hits, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, warning) = (theme.muted_foreground, theme.warning);
        let env_off = crate::usage_ui::env_off(cx);
        let paused = env_off.is_none()
            && crate::usage_ui::destination(cx) == Some(termlet_core::usage::Destination::Paused);
        let show_crash_report = self.initial_crash_report
            || super::update_model(cx).is_some_and(|model| model.read(cx).has_crash_endpoint());
        let note = |id: &'static str, text: SharedString, color| {
            div()
                .id(id)
                .test_support()
                .pl_6()
                .text_xs()
                .text_color(color)
                .child(prose(text))
        };
        v_flex()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .font_medium()
                    .child(t!("lock_ui.privacy.section")),
            )
            .child(
                hits.mark(
                    v_flex()
                        .gap_1()
                        .child(
                            Checkbox::new("about-usage")
                                .label(t!("lock_ui.privacy.usage"))
                                .checked(self.usage && env_off.is_none())
                                .disabled(env_off.is_some())
                                .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                    this.usage = *checked;
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .pl_6()
                                .text_xs()
                                .text_color(muted)
                                .child(prose(t!("lock_ui.privacy.usage_hint"))),
                        )
                        .when_some(env_off, |this, name| {
                            this.child(note(
                                "about-usage-env-off",
                                t!("lock_ui.privacy.usage_env_off", name = name),
                                warning,
                            ))
                        })
                        .when(paused, |this| {
                            this.child(note(
                                "about-usage-paused",
                                t!("lock_ui.privacy.usage_paused"),
                                muted,
                            ))
                        })
                        .child(
                            h_flex()
                                .pl_6()
                                .pt_1()
                                .flex_wrap()
                                .gap_2()
                                .child(
                                    Button::new("about-usage-view")
                                        .outline()
                                        .small()
                                        .label(t!("lock_ui.privacy.usage_view"))
                                        .on_click(|_, window, cx| {
                                            crate::usage_ui::open_view(window, cx);
                                        }),
                                )
                                .child(
                                    Button::new("about-privacy-policy")
                                        .ghost()
                                        .small()
                                        .icon(IconName::ExternalLink)
                                        .label(t!("lock_ui.privacy.policy"))
                                        .on_click(|_, _, cx| {
                                            cx.open_url(crate::website::privacy())
                                        }),
                                ),
                        ),
                    "about-usage",
                    cx,
                ),
            )
            .when(show_crash_report, |this| {
                this.child(
                    hits.mark(
                        v_flex()
                            .gap_1()
                            .child(
                                Checkbox::new("about-crash-report")
                                    .label(t!("lock_ui.diag.crash_report"))
                                    .checked(self.crash_report)
                                    .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                        this.crash_report = *checked;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .pl_6()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(prose(t!("lock_ui.diag.crash_report_hint"))),
                            ),
                        "about-crash-report",
                        cx,
                    ),
                )
            })
    }

    fn render_diagnostics(&self, hits: &Hits, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let available = super::services(cx).is_some();
        let logs_dir = super::services(cx).map(|s| s.logs_dir.clone());
        v_flex()
            .gap_2()
            .child(
                hits.mark(
                    div()
                        .text_sm()
                        .font_medium()
                        .child(t!("lock_ui.diag.section")),
                    "about-diagnostics",
                    cx,
                ),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        Button::new("diag-copy")
                            .outline()
                            .small()
                            .icon(IconName::Copy)
                            .label(t!("lock_ui.diag.copy"))
                            .loading(self.diag_busy)
                            .disabled(!available)
                            .on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.copy_diagnostics(window, cx)
                                }),
                            ),
                    )
                    .child(
                        Button::new("diag-export")
                            .outline()
                            .small()
                            .icon(IconName::FileDown)
                            .label(t!("lock_ui.diag.export"))
                            .disabled(!available || self.diag_busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.export_diagnostics(window, cx)
                            })),
                    )
                    .when_some(logs_dir, |this, dir| {
                        this.child(
                            Button::new("diag-open-logs")
                                .outline()
                                .small()
                                .icon(IconName::FolderOpen)
                                .label(t!("lock_ui.diag.open_logs"))
                                .on_click(move |_, _, cx| {
                                    let _ = std::fs::create_dir_all(&dir);
                                    cx.open_url(&dir.display().to_string());
                                }),
                        )
                        .child(
                            Button::new("diag-crash-records")
                                .outline()
                                .small()
                                .label(t!("lock_ui.diag.crash_records"))
                                .on_click(|_, window, cx| super::open_crash_log(window, cx)),
                        )
                    }),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(prose(t!("lock_ui.diag.hint"))),
            )
            .child(self.render_log_level(hits, cx))
    }
}

impl AboutExtras {
    /// 日志的详细程度：排查问题时调高；启动时用参数或环境变量指定了的，说明这里的选项暂不起作用。
    fn render_log_level(&self, hits: &Hits, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let overridden = termlet_core::logging::overridden();
        let level = v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().text_sm().child(t!("lock_ui.diag.log_level.label")))
                    .child(
                        div().w(gpui_kit::rems(16.)).child(
                            crate::choice::select(&self.log_level)
                                .id("diag-log-level")
                                .small(),
                        ),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(prose(t!("lock_ui.diag.log_level.hint"))),
            )
            .when_some(overridden, |this, source| {
                this.child(
                    div()
                        .id("diag-log-level-overridden")
                        .test_support()
                        .text_xs()
                        .text_color(cx.theme().warning)
                        .child(prose(t!(
                            "lock_ui.diag.log_level.overridden",
                            source = source.describe()
                        ))),
                )
            });
        hits.mark(level, "diag-log-level", cx)
    }
}

impl Render for AboutExtras {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().border;
        let hits = crate::settings::search::hits(window, cx);
        match self.part {
            AboutPart::Updates => v_flex().gap_4().child(
                div()
                    .pt_3()
                    .border_t_1()
                    .border_color(border)
                    .child(self.render_updates(&hits, cx)),
            ),
            AboutPart::Privacy => v_flex()
                .gap_4()
                .child(self.render_privacy(&hits, cx))
                .child(
                    div()
                        .pt_3()
                        .border_t_1()
                        .border_color(border)
                        .child(self.render_diagnostics(&hits, cx)),
                ),
        }
    }
}

/// “隐私与诊断”中有没有“自动发送崩溃报告”（设置搜索据此数命中的项；[`AboutExtras`] 另按打开时的值，开过的关掉后
/// 照样显示）。
pub(crate) fn crash_report_shown(cx: &App) -> bool {
    AppState::store(cx).local_settings().logs.crash_report
        || super::update_model(cx).is_some_and(|model| model.read(cx).has_crash_endpoint())
}

/// 诊断信息中的更新状态（不含版本以外的内容）。
fn status_name(status: &UpdateStatus) -> String {
    match status {
        UpdateStatus::Idle => "idle".into(),
        UpdateStatus::Checking => "checking".into(),
        UpdateStatus::Latest => "latest".into(),
        UpdateStatus::Available { version } => format!("available {version}"),
        UpdateStatus::Downloading {
            version, percent, ..
        } => {
            format!("downloading {version} {percent}%")
        }
        UpdateStatus::Downloaded { version } => format!("downloaded {version}"),
        UpdateStatus::Failed { kind, .. } => format!("failed {kind:?}"),
        UpdateStatus::Disabled => "disabled".into(),
    }
}
