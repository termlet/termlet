//! 更新状态的文案与标题栏的更新提示（[`UpdateBadge`]：新版本可用、下载中、已下载）。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::{
    Anchor, AnyElement, App, Context, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Subscription, TestSupportExt as _, Window, div,
    prelude::FluentBuilder as _, rems,
};
use termlet_core::update::{UpdateErrorKind, UpdateState, UpdateStatus};

use super::progress;
use crate::app_state::AppState;
use crate::prose::prose;
use crate::t;

/// 更新状态的一行说明。
pub(super) fn status_text(state: &UpdateState) -> SharedString {
    let current = state.current.as_str();
    match &state.status {
        UpdateStatus::Idle => t!("lock_ui.update.current", version = current),
        UpdateStatus::Checking => t!("lock_ui.update.checking"),
        UpdateStatus::Latest => t!("lock_ui.update.latest", version = current),
        UpdateStatus::Available { version } => t!("lock_ui.update.available", version = version),
        UpdateStatus::Downloading {
            version, percent, ..
        } => t!(
            "lock_ui.update.downloading",
            version = version,
            percent = percent
        ),
        UpdateStatus::Downloaded { version } => {
            t!("lock_ui.update.downloaded", version = version)
        }
        UpdateStatus::Failed { kind, .. } => match kind {
            UpdateErrorKind::Unpublished => t!("lock_ui.update.unpublished"),
            UpdateErrorKind::Network => t!("lock_ui.update.offline"),
            UpdateErrorKind::Signature => t!("lock_ui.update.bad_signature"),
            UpdateErrorKind::Other => t!("lock_ui.update.failed"),
        },
        // 远程配置中的提示是发布方写的，原样显示
        UpdateStatus::Disabled => match &state.message {
            Some(message) if !message.trim().is_empty() => message.clone().into(),
            _ => t!("lock_ui.update.disabled"),
        },
    }
}

/// 标题栏上要显示的更新提示（纯函数 [`badge_for`] 的结果）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Badge {
    None,
    /// 只提示的平台有新版本：点击打开下载页
    Available {
        version: String,
    },
    /// 下载中：进度，点击看详情
    Downloading {
        version: String,
        percent: u8,
        downloaded: u64,
        total: Option<u64>,
    },
    /// 已下载：点击重启安装
    Downloaded {
        version: String,
    },
}

/// 标题栏显示什么：已下载的总是提示；有新版本、下载中只在开启了“自动检查更新”时主动提示（关掉时用户在“关于”
/// 中手动检查，结果就在那里）；检查中、最新、失败、关闭了更新时不显示（“关于”中照常）。
pub(crate) fn badge_for(status: &UpdateStatus, auto_check: bool) -> Badge {
    match status {
        UpdateStatus::Downloaded { version } => Badge::Downloaded {
            version: version.clone(),
        },
        UpdateStatus::Available { version } if auto_check => Badge::Available {
            version: version.clone(),
        },
        UpdateStatus::Downloading {
            version,
            percent,
            downloaded,
            total,
        } if auto_check => Badge::Downloading {
            version: version.clone(),
            percent: *percent,
            downloaded: *downloaded,
            total: *total,
        },
        _ => Badge::None,
    }
}

/// 打开下载页上这种包、这个架构所在的一节（只提示的平台的“前往下载”、标题栏的“新版本可用”）。
pub(crate) fn open_download_page(cx: &mut App) {
    let page = crate::install_kind::InstallEnv::system(cx).download_page();
    cx.open_url(&page);
}

/// 标题栏的更新提示（参照 VS Code、Tabby：一个小提示，点开看详情）：
/// - 有新版本（只提示的平台）：“新版本 X 可用”，点击打开下载页上这种包所在的一节；
/// - 下载中：下载图标与小进度条，悬停显示版本、百分比与大小，点击弹出详情（进度、大小、速度、剩余时间）；
/// - 已下载：“新版本 X 已下载，点击重启安装”，点击后“重启并安装”（先经主会话的确认）。
///
/// 其余时候什么也不显示（见 [`badge_for`]）。`compact` 时只显示图标，文字放在悬停提示中（侧边栏隐藏、标题栏
/// 拥挤时）。
pub(crate) struct UpdateBadge {
    compact: bool,
    _observe: Vec<Subscription>,
}

impl UpdateBadge {
    pub(crate) fn new(compact: bool, cx: &mut Context<Self>) -> Self {
        let mut observe: Vec<Subscription> = super::update_model(cx)
            .map(|model| cx.observe(&model, |_, _, cx| cx.notify()))
            .into_iter()
            .collect();
        // 设置中开关“自动检查更新”后跟着显示或隐藏
        if let Some(app) = cx.try_global::<AppState>() {
            let store = app.store.clone();
            observe.push(cx.observe(&store, |_, _, cx| cx.notify()));
        }
        Self {
            compact,
            _observe: observe,
        }
    }
}

impl Render for UpdateBadge {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(model) = super::update_model(cx) else {
            return div().into_any_element();
        };
        let auto_check = cx
            .try_global::<AppState>()
            .is_none_or(|_| AppState::store(cx).settings().general.auto_check_update);
        let badge = badge_for(&model.read(cx).state().status, auto_check);
        let compact = self.compact;
        let theme = cx.theme();
        match badge {
            Badge::None => div().into_any_element(),
            Badge::Downloaded { version } => {
                let text = t!("lock_ui.update.banner", version = version);
                let button = Button::new("update-badge")
                    .ghost()
                    .small()
                    .icon(Icon::new(IconName::Download).text_color(theme.success))
                    .on_click(|_, window, cx| super::restart_and_install(window, cx));
                if compact {
                    button.tooltip(text).into_any_element()
                } else {
                    button.label(text).into_any_element()
                }
            }
            Badge::Available { version } => {
                let button = Button::new("update-badge-available")
                    .ghost()
                    .small()
                    .icon(Icon::new(IconName::CircleArrowUp).text_color(theme.info))
                    .tooltip(t!(
                        "lock_ui.update.badge_available_hint",
                        version = version.clone()
                    ))
                    .on_click(|_, _, cx| open_download_page(cx));
                if compact {
                    button.into_any_element()
                } else {
                    button
                        .label(t!("lock_ui.update.badge_available", version = version))
                        .into_any_element()
                }
            }
            Badge::Downloading {
                version,
                percent,
                downloaded,
                total,
            } => {
                let detail = t!(
                    "lock_ui.update.downloading_detail",
                    version = version.clone(),
                    percent = percent,
                    progress = progress::format_progress(downloaded, total)
                );
                let trigger = Button::new("update-badge-downloading")
                    .ghost()
                    .small()
                    .icon(Icon::new(IconName::Download).text_color(theme.info))
                    .tooltip(detail)
                    .child(
                        Progress::new("update-badge-progress")
                            .value(f32::from(percent))
                            .w(rems(2.5))
                            .h_1(),
                    )
                    .when(!compact, |this| this.label(format!("{percent}%")));
                Popover::new("update-badge-popover")
                    .anchor(Anchor::TopRight)
                    .trigger(trigger)
                    .content(|_, _, cx| download_panel(cx))
                    .into_any_element()
            }
        }
    }
}

/// 下载详情（标题栏提示点开后）：版本、进度条、已下载 / 总大小、速度与剩余时间。随更新模型的变化重画。
fn download_panel(cx: &App) -> AnyElement {
    let Some(model) = super::update_model(cx) else {
        return div().into_any_element();
    };
    let model = model.read(cx);
    let UpdateStatus::Downloading {
        version,
        percent,
        downloaded,
        total,
    } = &model.state().status
    else {
        return div().into_any_element();
    };
    let muted = cx.theme().muted_foreground;
    let speed = model.meter().speed();
    let eta = model.meter().eta(*total);
    v_flex()
        .id("update-panel")
        .test_support()
        .w(rems(18.))
        .gap_2()
        .text_sm()
        .child(
            div()
                .font_medium()
                .child(t!("lock_ui.update.panel_title", version = version.clone())),
        )
        .child(Progress::new("update-panel-progress").value(f32::from(*percent)))
        .child(
            h_flex()
                .justify_between()
                .gap_2()
                .text_xs()
                .child(
                    div()
                        .id("update-panel-size")
                        .test_support()
                        .child(progress::format_progress(*downloaded, *total)),
                )
                .child(div().text_color(muted).child(format!("{percent}%"))),
        )
        .child(
            h_flex()
                .justify_between()
                .gap_2()
                .text_xs()
                .text_color(muted)
                .child(
                    div()
                        .id("update-panel-speed")
                        .test_support()
                        .children(speed.map(|speed| {
                            t!(
                                "lock_ui.update.speed",
                                speed = progress::format_speed(speed)
                            )
                        })),
                )
                .child(
                    div()
                        .id("update-panel-eta")
                        .test_support()
                        .children(eta.map(progress::format_eta)),
                ),
        )
        .child(
            div()
                .text_xs()
                .text_color(muted)
                .child(prose(t!("lock_ui.update.panel_hint"))),
        )
        .into_any_element()
}

/// 开发构建中预览标题栏的更新提示（截图核对用，不真的检查、下载）：环境变量 `TERMLET_UPDATE_PREVIEW` 为
/// `available`、`downloading`、`downloaded` 之一时，启动后把更新状态换成它（`downloading` 按约 0.8 MB/s 涨到 99%）。
/// 同时请设 `TERMLET_UPDATE_URL=off`，免得真的检查覆盖它。正式构建中没有。
#[cfg(debug_assertions)]
pub(super) fn start_preview(model: &gpui_kit::Entity<super::UpdateModel>, cx: &mut App) {
    let Ok(kind) = std::env::var("TERMLET_UPDATE_PREVIEW") else {
        return;
    };
    let model = model.downgrade();
    cx.spawn(async move |cx| {
        let version = "0.2.1".to_string();
        let total: u64 = 18 * 1024 * 1024 + 300 * 1024;
        let mut downloaded: u64 = 0;
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(250))
                .await;
            let status = match kind.as_str() {
                "available" => UpdateStatus::Available {
                    version: version.clone(),
                },
                "downloaded" => UpdateStatus::Downloaded {
                    version: version.clone(),
                },
                _ => {
                    downloaded = (downloaded + 200 * 1024).min(total * 99 / 100);
                    UpdateStatus::Downloading {
                        version: version.clone(),
                        percent: (downloaded * 100 / total) as u8,
                        downloaded,
                        total: Some(total),
                    }
                }
            };
            let alive = model.update(cx, |model, cx| {
                let state = UpdateState {
                    status,
                    current: termlet_core::VERSION.to_string(),
                    message: None,
                    auto_install: true,
                };
                model.set_state(state, std::time::Instant::now(), cx);
            });
            if alive.is_err() {
                break;
            }
        }
    })
    .detach();
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    use crate::i18n::{self, Language};

    fn state(status: UpdateStatus) -> UpdateState {
        UpdateState {
            status,
            current: "1.0.0".into(),
            message: None,
            auto_install: true,
        }
    }

    #[test]
    fn status_text_follows_the_old_version() {
        let original = i18n::language();
        i18n::set_language(Language::ZhCn);
        assert_eq!(status_text(&state(UpdateStatus::Idle)), "当前版本 1.0.0");
        assert_eq!(
            status_text(&state(UpdateStatus::Latest)),
            "当前版本 1.0.0 · 已是最新版本"
        );
        assert_eq!(
            status_text(&state(UpdateStatus::Downloading {
                version: "1.1.0".into(),
                percent: 42,
                downloaded: 420,
                total: Some(1000),
            })),
            "正在下载 1.1.0（42%）"
        );
        assert_eq!(
            status_text(&state(UpdateStatus::Failed {
                kind: UpdateErrorKind::Network,
                error: "连接被拒绝".into()
            })),
            "无法连接更新服务器，请检查网络后重试"
        );
        assert_eq!(status_text(&state(UpdateStatus::Disabled)), "更新暂不可用");
        let mut paused = state(UpdateStatus::Disabled);
        paused.message = Some("维护中，明天恢复".into());
        assert_eq!(status_text(&paused), "维护中，明天恢复");
        i18n::set_language(Language::En);
        assert_eq!(
            status_text(&state(UpdateStatus::Downloaded {
                version: "1.1.0".into()
            })),
            "Version 1.1.0 is ready"
        );
        assert_eq!(
            status_text(&state(UpdateStatus::Failed {
                kind: UpdateErrorKind::Signature,
                error: String::new()
            })),
            "The update failed signature verification and was stopped. Download it manually from the releases page"
        );
        i18n::set_language(original);
    }

    // 标题栏：已下载的总是提示；有新版本、下载中只在开启了自动检查时提示；检查中、最新、失败时不提示
    #[test]
    fn badge_follows_the_status_and_auto_check() {
        let available = UpdateStatus::Available {
            version: "2.0.0".into(),
        };
        let downloading = UpdateStatus::Downloading {
            version: "2.0.0".into(),
            percent: 45,
            downloaded: 450,
            total: Some(1000),
        };
        let downloaded = UpdateStatus::Downloaded {
            version: "2.0.0".into(),
        };
        assert_eq!(
            badge_for(&available, true),
            Badge::Available {
                version: "2.0.0".into()
            }
        );
        assert_eq!(
            badge_for(&downloading, true),
            Badge::Downloading {
                version: "2.0.0".into(),
                percent: 45,
                downloaded: 450,
                total: Some(1000)
            }
        );
        for auto_check in [true, false] {
            assert_eq!(
                badge_for(&downloaded, auto_check),
                Badge::Downloaded {
                    version: "2.0.0".into()
                }
            );
        }
        assert_eq!(badge_for(&available, false), Badge::None);
        assert_eq!(badge_for(&downloading, false), Badge::None);
        for status in [
            UpdateStatus::Idle,
            UpdateStatus::Checking,
            UpdateStatus::Latest,
            UpdateStatus::Disabled,
            UpdateStatus::Failed {
                kind: UpdateErrorKind::Network,
                error: String::new(),
            },
        ] {
            assert_eq!(badge_for(&status, true), Badge::None, "{status:?}");
        }
    }
}

/// 标题栏的更新提示：在主窗口中模拟各种更新状态，检查显示与点击。
#[cfg(test)]
mod badge_tests {
    use std::time::{Duration, Instant};

    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AnyWindowHandle, AppContext as _, TestAppContext};
    use termlet_core::update::{UpdateState, UpdateStatus};

    use crate::app_state::AppState;
    use crate::i18n::{Language, set_language};
    use crate::testkit::open_workspace;

    fn set(cx: &mut TestAppContext, status: UpdateStatus, at: Instant) {
        cx.update(|cx| {
            let model = super::super::update_model(cx).expect("更新模型");
            model.update(cx, |model, cx| {
                let state = UpdateState {
                    status,
                    current: "0.2.0".into(),
                    message: None,
                    auto_install: false,
                };
                model.set_state(state, at, cx)
            });
        });
        cx.run_until_parked();
    }

    fn shown(cx: &mut TestAppContext, handle: AnyWindowHandle, id: &'static str) -> bool {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.try_find(id).is_some()
        })
        .unwrap()
    }

    const BADGES: [&str; 3] = [
        "update-badge-available",
        "update-badge-downloading",
        "update-badge",
    ];

    // 有新版本（只提示的平台）：“新版本 X 可用”，点击打开下载页上这种包所在的一节；关掉自动检查更新时不主动提示
    #[gpui_kit::test]
    fn available_opens_the_download_page(cx: &mut TestAppContext) {
        let handle = open_workspace(cx);
        set_language(Language::ZhCn);
        assert!(
            BADGES.iter().all(|id| !shown(cx, handle, id)),
            "最新时不显示"
        );
        let available = UpdateStatus::Available {
            version: "0.2.1".into(),
        };
        set(cx, available.clone(), Instant::now());
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("update-badge-available", cx);
        })
        .unwrap();
        let expected = cx.update(|cx| crate::install_kind::InstallEnv::system(cx).download_page());
        assert_eq!(cx.opened_url(), Some(expected));

        cx.update(|cx| {
            AppState::store(cx)
                .update_settings(|s| s.general.auto_check_update = false)
                .unwrap()
        });
        cx.run_until_parked();
        assert!(
            !shown(cx, handle, "update-badge-available"),
            "关掉了自动检查"
        );
    }

    // 下载中：图标、小进度条与百分比；点开看版本、进度、大小、速度与剩余时间（开始几秒后才有剩余时间）
    #[gpui_kit::test]
    fn downloading_shows_progress_and_details(cx: &mut TestAppContext) {
        let handle = open_workspace(cx);
        set_language(Language::ZhCn);
        let mb = 1024 * 1024;
        let total = Some(18 * mb);
        let start = Instant::now();
        for (seconds, downloaded) in [(0, 0), (2, 4 * mb), (4, 8 * mb)] {
            set(
                cx,
                UpdateStatus::Downloading {
                    version: "0.2.1".into(),
                    percent: (downloaded * 100 / (18 * mb)) as u8,
                    downloaded,
                    total,
                },
                start + Duration::from_secs(seconds),
            );
        }
        assert!(shown(cx, handle, "update-badge-downloading"));
        assert!(!shown(cx, handle, "update-panel"), "点开才有详情");
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("update-badge-downloading", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert!(shown(cx, handle, "update-panel"));
        assert!(shown(cx, handle, "update-panel-speed"));
        let (speed, eta) = cx.update(|cx| {
            let model = super::super::update_model(cx).unwrap();
            let meter = model.read(cx).meter();
            (meter.speed(), meter.eta(total))
        });
        assert_eq!(speed, Some(2. * mb as f64));
        assert_eq!(eta, Some(Duration::from_secs(5)));
    }

    // 已下载：“新版本 X 已下载，点击重启安装”；回到最新时提示消失
    #[gpui_kit::test]
    fn downloaded_offers_to_restart(cx: &mut TestAppContext) {
        let handle = open_workspace(cx);
        set(
            cx,
            UpdateStatus::Downloaded {
                version: "0.2.1".into(),
            },
            Instant::now(),
        );
        assert!(shown(cx, handle, "update-badge"));
        assert!(!shown(cx, handle, "update-badge-available"));
        set(cx, UpdateStatus::Latest, Instant::now());
        assert!(BADGES.iter().all(|id| !shown(cx, handle, id)));
    }
}
