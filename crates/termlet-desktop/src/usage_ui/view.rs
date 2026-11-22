//! “将发送的使用统计”对话框：今天要发送的 JSON 原文（可选中、可复制，与实际发出的由同一个函数生成）、各字段的含义、
//! 接收端另外推断的国家或地区、接收地址与状态（上次发送的日期、今天是否已发送，或已关闭）、日志中的记录。
//! 由设置中的“查看发送的内容…”打开。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _, WindowExt as _, h_flex,
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, ClipboardItem, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Task, TestSupportExt as _, Window, div,
    prelude::FluentBuilder as _, rems,
};
use termlet_core::usage::{Destination, Preview, UsageReporter};

use crate::app_state::AppState;
use crate::prose::prose;
use crate::t;

/// 打开对话框，返回其中的视图（测试用它核对显示的正文）。
pub(crate) fn open_view(window: &mut Window, cx: &mut App) -> Option<Entity<UsageView>> {
    let reporter = super::reporter(cx)?;
    let view = cx.new(|cx| UsageView::new(reporter, window, cx));
    let width = rems(40.).to_pixels(window.rem_size());
    window.open_dialog(cx, {
        let view = view.clone();
        move |dialog, _, _| {
            dialog
                .title(t!("usage.view.title"))
                .w(width)
                .child(view.clone())
        }
    });
    Some(view)
}

/// 读取的结果。
enum Loaded {
    Loading,
    /// 这个系统或架构不发
    Unsupported,
    Ready(Preview),
}

pub(crate) struct UsageView {
    loaded: Loaded,
    text: Entity<TextareaState>,
    destination: Destination,
    copied: bool,
    _load: Task<()>,
}

impl UsageView {
    fn new(reporter: UsageReporter, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let text = cx.new(|cx| TextareaState::new(window, cx).rows(3));
        let lang = crate::i18n::language().code().to_string();
        let now_ms = termlet_core::time::now_ms();
        let destination = reporter.destination();
        // 读 usage.json 与系统版本：放到后台
        let preview = cx.background_spawn(async move { reporter.preview(&lang, now_ms) });
        let load = cx.spawn_in(window, async move |this, cx| {
            let preview = preview.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.loaded = match preview {
                    Some(preview) => {
                        this.text.update(cx, |state, cx| {
                            state.set_value(preview.json.clone(), window, cx)
                        });
                        Loaded::Ready(preview)
                    }
                    None => Loaded::Unsupported,
                };
                cx.notify();
            });
        });
        Self {
            loaded: Loaded::Loading,
            text,
            destination,
            copied: false,
            _load: load,
        }
    }

    /// 显示的正文（还在读取、这个平台不发时为 `None`）。
    pub(crate) fn json(&self) -> Option<&str> {
        match &self.loaded {
            Loaded::Ready(preview) => Some(&preview.json),
            _ => None,
        }
    }

    /// 文本框中的内容（测试用：与 [`UsageView::json`] 相同）。
    #[cfg(test)]
    pub(crate) fn text(&self, cx: &App) -> SharedString {
        self.text.read(cx).value()
    }

    /// 状态的一行：已关闭、暂停时说明不会发送，否则说今天是否已发送。
    pub(super) fn state_line(&self, cx: &App) -> Option<SharedString> {
        let usage = AppState::store(cx).local_settings().usage;
        let preview = match &self.loaded {
            Loaded::Loading => return None,
            Loaded::Unsupported => return Some(t!("usage.view.unsupported")),
            Loaded::Ready(preview) => preview,
        };
        Some(match &self.destination {
            Destination::EnvOff(name) => t!("usage.view.env_off", name = name),
            _ if !usage.enabled => t!("usage.view.disabled"),
            Destination::DebugBuild => t!("usage.view.debug"),
            Destination::Paused => t!("usage.view.paused"),
            _ if preview.sent_today => t!("usage.view.sent_today"),
            _ => t!("usage.view.not_sent_today"),
        })
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        if let Some(json) = self.json() {
            cx.write_to_clipboard(ClipboardItem::new_string(json.to_string()));
            self.copied = true;
            cx.notify();
        }
    }
}

impl Render for UsageView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, border, mono) = (
            theme.muted_foreground,
            theme.border,
            theme.mono_font_family.clone(),
        );
        let field = |name: &'static str, meaning: SharedString| {
            h_flex()
                .gap_2()
                .items_start()
                .text_xs()
                .child(
                    div()
                        .w(rems(7.))
                        .flex_none()
                        .font_family(mono.clone())
                        .child(name),
                )
                .child(div().flex_1().min_w_0().text_color(muted).child(meaning))
        };
        let last_sent = match &self.loaded {
            Loaded::Ready(preview) => Some(match preview.last_sent {
                Some(date) => t!(
                    "usage.view.last_sent",
                    date = date.format("%Y-%m-%d").to_string()
                ),
                None => t!("usage.view.never_sent"),
            }),
            _ => None,
        };
        let endpoint = match &self.destination {
            Destination::Url(url) => Some(t!("usage.view.endpoint", url = url)),
            _ => None,
        };
        let logs_dir = crate::lock_ui::logs_dir(cx);
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(prose(t!("usage.view.intro"))),
            )
            .child(
                div().id("usage-view-json").test_support().child(
                    Textarea::new(&self.text)
                        .readonly(true)
                        .font_family(mono.clone()),
                ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(field("v", t!("usage.view.field.v")))
                    .child(field("app", t!("usage.view.field.app")))
                    .child(field("os", t!("usage.view.field.os")))
                    .child(field("osVersion", t!("usage.view.field.os_version")))
                    .child(field("arch", t!("usage.view.field.arch")))
                    .child(field("lang", t!("usage.view.field.lang")))
                    .child(field("age", t!("usage.view.field.age")))
                    .child(field("firstOfWeek", t!("usage.view.field.first_of_week")))
                    .child(field("firstOfMonth", t!("usage.view.field.first_of_month")))
                    .child(field("features", t!("usage.view.field.features"))),
            )
            .child(
                v_flex()
                    .gap_1()
                    .text_xs()
                    .text_color(muted)
                    .child(prose(t!("usage.view.country")))
                    .child(prose(t!("usage.view.rate_limit"))),
            )
            .child(
                v_flex()
                    .gap_1()
                    .text_sm()
                    .when_some(self.state_line(cx), |this, line| {
                        this.child(
                            div()
                                .id("usage-view-state")
                                .test_support()
                                .font_medium()
                                .child(line),
                        )
                    })
                    .when_some(endpoint, |this, line| {
                        this.child(div().text_xs().text_color(muted).child(line))
                    })
                    .when_some(last_sent, |this, line| {
                        this.child(div().text_xs().text_color(muted).child(line))
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(muted)
                            .child(prose(t!("usage.view.log_hint"))),
                    )
                    .when_some(logs_dir, |this, dir| {
                        this.child(
                            Button::new("usage-view-open-logs")
                                .outline()
                                .small()
                                .icon(IconName::FolderOpen)
                                .label(t!("lock_ui.diag.open_logs"))
                                .on_click(move |_, _, cx| {
                                    let _ = std::fs::create_dir_all(&dir);
                                    cx.open_url(&dir.display().to_string());
                                }),
                        )
                    }),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .pt_3()
                    .border_t_1()
                    .border_color(border)
                    .child(
                        Button::new("usage-view-copy")
                            .outline()
                            .icon(IconName::Copy)
                            .label(if self.copied {
                                t!("usage.view.copied")
                            } else {
                                t!("usage.view.copy")
                            })
                            .disabled(self.json().is_none())
                            .on_click(cx.listener(|this, _, _, cx| this.copy(cx))),
                    )
                    .child(
                        Button::new("usage-view-close")
                            .primary()
                            .label(t!("common.close"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
            )
    }
}
