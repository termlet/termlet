//! 「AI 与自动化」页 MCP 服务的 HTTP 接入：在本机监听的开关与端口（草稿，随设置对话框的“保存”写进本机设置，命令行
//! 服务随即启停、换端口，这里不用另外调用）、实际的状态（订阅命令行服务）、地址与令牌。
//!
//! 令牌不在设置草稿中（在本机的 `meta` 表）：读取、重新生成都在后台执行器上直接经存储做，重新生成立即生效。界面上默认
//! 打码，“显示”后才露出原文；复制出的配置含原文（[`HttpPanel::token_for_copy`]）。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, ClipboardItem, Context, Entity, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Subscription, Task,
    TestSupportExt as _, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::CoreError;
use termlet_core::cli::http::token::{self, TOKEN_PREFIX};
use termlet_core::cli::{HttpError, HttpStatus, TokenView};
use termlet_core::model::CliHttpSettings;
use termlet_core::proto::mcp::http_url;

use crate::app_state::AppState;
use crate::notify::{self, Notice};
use crate::prose::{ProseLabel as _, prose};
use crate::t;

/// 这一部分各行左边标签的宽度。
pub(super) const LABEL_WIDTH: f32 = 6.;

/// 一行：左边是标签，右边是内容。
pub(super) fn row(label: SharedString, muted: Hsla, content: impl IntoElement) -> gpui_kit::Div {
    h_flex()
        .gap_2()
        .items_center()
        .child(
            div()
                .w(rems(LABEL_WIDTH))
                .flex_none()
                .text_sm()
                .text_color(muted)
                .child(label),
        )
        .child(div().flex_1().min_w_0().child(content))
}

/// 打码的令牌：只露出前缀。
pub(super) fn masked_token() -> String {
    format!("{TOKEN_PREFIX}••••••••")
}

/// 读到的令牌。
#[derive(Clone, Debug)]
pub(super) enum TokenState {
    Loading,
    View(TokenView),
    /// 读取失败（数据库出错）
    Failed(String),
}

pub(crate) struct HttpPanel {
    /// “允许 AI 工具与脚本访问”的草稿（这一页最上面的开关，由页面同步过来）：关着时 HTTP 不监听
    cli_enabled: bool,
    /// “在本机监听 HTTP”的草稿
    pub(super) enabled: bool,
    port: Entity<InputState>,
    /// 端口输入框中的原文（随输入更新）
    port_text: String,
    /// 打开对话框时本机设置中的端口（草稿的端口不能用时，地址按它写）
    saved_port: u16,
    /// 实际的状态（命令行服务送来；服务没有启动时一直是 `Off`）
    pub(super) status: HttpStatus,
    pub(super) token: TokenState,
    /// 令牌显示原文（默认打码）
    pub(super) revealed: bool,
    /// 正在重新生成令牌
    busy: bool,
    _watch: Option<Task<()>>,
    _token: Option<Task<()>>,
    _subscription: Subscription,
}

impl HttpPanel {
    pub(super) fn new(
        cli_enabled: bool,
        settings: &CliHttpSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let port_text = settings.port.to_string();
        let port = cx.new(|cx| InputState::new(window, cx).default_value(port_text.clone()));
        let subscription = cx.subscribe(&port, |this, input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.port_text = input.read(cx).value().to_string();
                cx.notify();
            }
        });
        let mut status = HttpStatus::Off;
        let watch = crate::cli_bridge::subscribe_http(cx).map(|mut rx| {
            status = rx.borrow_and_update().clone();
            cx.spawn(async move |this, cx| {
                while rx.changed().await.is_ok() {
                    let status = rx.borrow_and_update().clone();
                    if this
                        .update(cx, |this, cx| this.set_status(status, cx))
                        .is_err()
                    {
                        break;
                    }
                }
            })
        });
        let mut this = Self {
            cli_enabled,
            enabled: settings.enabled,
            port,
            port_text,
            saved_port: settings.port,
            status,
            token: TokenState::Loading,
            revealed: false,
            busy: false,
            _watch: watch,
            _token: None,
            _subscription: subscription,
        };
        this.load_token(cx);
        this
    }

    /// 页面最上面的开关改了。
    pub(super) fn set_cli_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.cli_enabled = enabled;
        cx.notify();
    }

    /// 实际的状态变了（保存设置后、重试后）：开始监听时可能刚生成了令牌，重新读一次。
    pub(super) fn set_status(&mut self, status: HttpStatus, cx: &mut Context<Self>) {
        if status == self.status {
            return;
        }
        let listening = matches!(status, HttpStatus::Listening { .. });
        self.status = status;
        if listening {
            self.load_token(cx);
        }
        cx.notify();
    }

    /// 草稿中的端口：1–65535，不能用时为 `None`。
    pub(super) fn draft_port(&self) -> Option<u16> {
        self.port_text
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|p| *p != 0)
    }

    /// 端口不能用时不能保存。
    pub(super) fn validate(&self) -> Result<(), SharedString> {
        match self.draft_port() {
            Some(_) => Ok(()),
            None => Err(t!("cli_ui.http.port_invalid")),
        }
    }

    /// 写进本机设置的 `cli.http`。
    pub(super) fn apply(&self, settings: &mut CliHttpSettings) {
        settings.enabled = self.enabled;
        if let Some(port) = self.draft_port() {
            settings.port = port;
        }
    }

    /// 按草稿保存后要监听的端口；不监听时为 `None`。
    fn wanted(&self) -> Option<u16> {
        (self.cli_enabled && self.enabled).then(|| self.draft_port().unwrap_or(0))
    }

    /// 草稿与实际的状态不同：保存后才生效。
    pub(super) fn pending(&self) -> bool {
        match (self.wanted(), &self.status) {
            (None, HttpStatus::Off) => false,
            (Some(want), HttpStatus::Listening { port } | HttpStatus::Failed { port, .. }) => {
                want != *port
            }
            _ => true,
        }
    }

    /// 片段与地址行中的地址：在监听时按实际的端口，否则按草稿的端口。
    pub(super) fn url(&self) -> String {
        let port = match self.status {
            HttpStatus::Listening { port } => port,
            _ => self.draft_port().unwrap_or(self.saved_port),
        };
        http_url(port)
    }

    /// 复制配置用的令牌原文：草稿中两个开关都开着、令牌读得出来时才有（否则预览中写占位）。
    pub(super) fn token_for_copy(&self) -> Option<String> {
        match &self.token {
            TokenState::View(TokenView::Ready(token)) if self.cli_enabled && self.enabled => {
                Some(token.expose().to_string())
            }
            _ => None,
        }
    }

    /// 还不能复制配置时的原因。
    pub(super) fn copy_blocked(&self) -> Option<SharedString> {
        if self.token_for_copy().is_some() {
            return None;
        }
        Some(match &self.token {
            _ if !(self.cli_enabled && self.enabled) => t!("cli_ui.http.copy_later_off"),
            TokenState::View(TokenView::Locked) => t!("cli_ui.http.copy_later_locked"),
            TokenState::View(TokenView::Lost) => t!("cli_ui.http.copy_later_lost"),
            _ => t!("cli_ui.http.copy_later_off"),
        })
    }

    /// 在后台读令牌。
    fn load_token(&mut self, cx: &mut Context<Self>) {
        let store = AppState::store(cx);
        self._token = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { token::view(&store) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.token = match result {
                    Ok(view) => TokenState::View(view),
                    Err(e) => {
                        tracing::warn!("读取 MCP 的 HTTP 令牌失败：{e}");
                        TokenState::Failed(e.to_string())
                    }
                };
                cx.notify();
            });
        }));
    }

    fn confirm_regenerate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.entity().downgrade();
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let view = view.clone();
            alert
                .confirm()
                .title(t!("cli_ui.http.regenerate_title"))
                .description(prose(t!("cli_ui.http.regenerate_body")))
                .ok_text(t!("cli_ui.http.regenerate_ok"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, window, cx| {
                    let _ = view.update(cx, |view, cx| view.regenerate(window, cx));
                    true
                })
        });
    }

    /// 重新生成令牌（立即生效，用旧令牌的 AI 工具随即被拒绝）。
    pub(super) fn regenerate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        let store = AppState::store(cx);
        self._token = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { token::regenerate(&store) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                let notice = match result {
                    Ok(token) => {
                        this.token = TokenState::View(TokenView::Ready(token));
                        this.revealed = false;
                        Notice::success(t!("cli_ui.http.regenerated"))
                    }
                    Err(CoreError::VaultLocked) => {
                        Notice::error(t!("cli_ui.http.regenerate_locked"))
                    }
                    Err(e) => {
                        tracing::warn!("重新生成 MCP 的 HTTP 令牌失败：{e}");
                        Notice::error(t!("cli_ui.http.regenerate_failed", error = e.to_string()))
                    }
                };
                notify::show(notice, window, cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    // ------------------------------------------------------------ 渲染

    /// 状态行的文字与颜色。
    pub(super) fn status_text(&self, cx: &App) -> (SharedString, Hsla) {
        let theme = cx.theme();
        match &self.status {
            HttpStatus::Listening { port } => (
                t!("cli_ui.http.status_listening", port = port),
                theme.success,
            ),
            HttpStatus::Off if !self.cli_enabled => {
                (t!("cli_ui.http.status_needs_cli"), theme.muted_foreground)
            }
            HttpStatus::Off => (t!("cli_ui.http.status_off"), theme.muted_foreground),
            HttpStatus::Failed { port, error } => {
                let text = match error {
                    HttpError::PortInUse => t!("cli_ui.http.status_in_use", port = port),
                    HttpError::Denied => t!("cli_ui.http.status_denied", port = port),
                    HttpError::InvalidPort => t!("cli_ui.http.status_invalid", port = port),
                    HttpError::Other(error) => t!(
                        "cli_ui.http.status_other",
                        port = port,
                        error = error.clone()
                    ),
                };
                (text, theme.danger)
            }
        }
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (text, color) = self.status_text(cx);
        let pending = self.pending();
        let failed = matches!(self.status, HttpStatus::Failed { .. });
        let theme = cx.theme();
        v_flex()
            .gap_1()
            .child(
                div()
                    .id("cli-http-status")
                    .test_support()
                    .text_sm()
                    .text_color(color)
                    .child(prose(text)),
            )
            .when(pending || failed, |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .when(pending, |this| {
                            this.child(
                                div()
                                    .id("cli-http-pending")
                                    .test_support()
                                    .text_xs()
                                    .text_color(theme.warning)
                                    .child(t!("cli_ui.http.pending")),
                            )
                        })
                        .when(failed && !pending, |this| {
                            this.child(
                                Button::new("cli-http-retry")
                                    .outline()
                                    .small()
                                    .icon(IconName::RefreshCw)
                                    .label(t!("cli_ui.http.retry"))
                                    .on_click(|_, _, cx| crate::cli_bridge::refresh_http(cx)),
                            )
                        }),
                )
            })
    }

    fn render_url(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        let url = self.url();
        h_flex()
            .gap_1()
            .child(
                div()
                    .id("cli-http-url")
                    .test_support()
                    .text_sm()
                    .font_family(mono)
                    .child(url.clone()),
            )
            .child(
                Button::new("cli-http-url-copy")
                    .ghost()
                    .small()
                    .icon(IconName::Copy)
                    .tooltip(t!("cli_ui.http.copy_address"))
                    .on_click(move |_, window, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                        notify::show(
                            Notice::success(t!("cli_ui.http.copied_address")),
                            window,
                            cx,
                        );
                    }),
            )
    }

    fn render_token(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let mono = theme.mono_font_family.clone();
        let note = |text: SharedString, color: Hsla| {
            div()
                .id("cli-http-token-note")
                .test_support()
                .text_sm()
                .text_color(color)
                .child(prose(text))
        };
        let regenerate = Button::new("cli-http-token-regenerate")
            .outline()
            .small()
            .icon(IconName::RefreshCw)
            .label(t!("cli_ui.http.regenerate"))
            .disabled(self.busy)
            .on_click(cx.listener(|this, _, window, cx| this.confirm_regenerate(window, cx)));
        match &self.token {
            TokenState::View(TokenView::Ready(token)) => {
                let shown = if self.revealed {
                    token.expose().to_string()
                } else {
                    masked_token()
                };
                let secret = token.expose().to_string();
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    // 显示原文时独占一行、放不下时折行（令牌中间没有可以断开的地方），按钮排到下一行
                    .child(
                        div()
                            .id("cli-http-token")
                            .test_support()
                            .min_w_0()
                            .when(self.revealed, |this| this.w_full())
                            .text_sm()
                            .font_family(mono)
                            .child(shown),
                    )
                    .child(
                        Button::new("cli-http-token-reveal")
                            .ghost()
                            .small()
                            .icon(if self.revealed {
                                IconName::EyeOff
                            } else {
                                IconName::Eye
                            })
                            .label(if self.revealed {
                                t!("cli_ui.http.hide")
                            } else {
                                t!("cli_ui.http.show")
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.revealed = !this.revealed;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("cli-http-token-copy")
                            .ghost()
                            .small()
                            .icon(IconName::Copy)
                            .label(t!("cli_ui.http.copy_token"))
                            .on_click(move |_, window, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(secret.clone()));
                                notify::show(
                                    Notice::success(t!("cli_ui.http.copied_token")),
                                    window,
                                    cx,
                                );
                            }),
                    )
                    .child(regenerate)
                    .into_any_element()
            }
            TokenState::View(TokenView::Lost) => h_flex()
                .gap_2()
                .flex_wrap()
                .child(note(t!("cli_ui.http.token_lost"), muted))
                .child(regenerate)
                .into_any_element(),
            TokenState::View(TokenView::Missing) => {
                note(t!("cli_ui.http.token_missing"), muted).into_any_element()
            }
            TokenState::View(TokenView::Locked) => {
                note(t!("cli_ui.http.token_locked"), muted).into_any_element()
            }
            TokenState::Loading => note(t!("cli_ui.http.token_loading"), muted).into_any_element(),
            TokenState::Failed(error) => note(
                t!("cli_ui.http.token_failed", error = error.clone()),
                danger,
            )
            .into_any_element(),
        }
    }
}

impl Render for HttpPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let port_ok = self.draft_port().is_some();
        let note = if cfg!(windows) {
            t!("cli_ui.http.note_windows")
        } else {
            t!("cli_ui.http.note_other")
        };
        v_flex()
            .gap_2()
            .child(
                Checkbox::new("cli-http-enabled")
                    .prose_label(t!("cli_ui.http.enabled"))
                    .checked(self.enabled)
                    .on_change(cx.listener(|this, checked: &bool, _, cx| {
                        this.enabled = *checked;
                        cx.notify();
                    })),
            )
            .child(row(
                t!("cli_ui.http.port"),
                muted,
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .w(rems(7.))
                            .child(Input::new(&self.port).small().id("cli-http-port")),
                    )
                    .when(!port_ok, |this| {
                        this.child(
                            div()
                                .id("cli-http-port-error")
                                .test_support()
                                .text_xs()
                                .text_color(danger)
                                .child(t!("cli_ui.http.port_range")),
                        )
                    }),
            ))
            .child(row(t!("cli_ui.http.status"), muted, self.render_status(cx)))
            .child(row(t!("cli_ui.http.address"), muted, self.render_url(cx)))
            .child(row(t!("cli_ui.http.token"), muted, self.render_token(cx)))
            .child(div().text_xs().text_color(muted).child(prose(note)))
    }
}
