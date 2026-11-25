//! 「AI 与自动化」页 MCP 服务的 HTTP 接入：监听的开关、范围与端口（草稿，随设置的“保存”写进本机设置，命令行
//! 服务随即启停、换端口与范围，这里不用另外调用）、实际的状态（订阅命令行服务）、地址与令牌。监听范围为局域网时另有
//! 这台电脑在局域网中的地址（多个时可选）、局域网令牌与风险的说明。
//!
//! 令牌不在设置草稿中（在本机的 `meta` 表）：读取、重新生成都在后台执行器上直接经存储做，重新生成立即生效。本机的与
//! 局域网的令牌各自显示、复制、重新生成。界面上默认打码，“显示”后才露出原文；复制出的配置含原文
//! （[`HttpPanel::token_for_copy`]）。

use std::net::Ipv4Addr;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectEvent};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, ClipboardItem, Context, ElementId, Entity, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Subscription, Task,
    TestSupportExt as _, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::CoreError;
use termlet_core::cli::http::lan::LanAddress;
use termlet_core::cli::http::token;
use termlet_core::cli::{APPROVAL_TIMEOUT, HttpError, HttpStatus, TokenKind, TokenView};
use termlet_core::model::{CliHttpSettings, HttpScope};
use termlet_core::proto::mcp::{http_url, http_url_at};

use crate::app_state::AppState;
use crate::choice::{Choice, ChoiceState, choice_state, chosen};
use crate::notify::{self, Notice};
use crate::prose::{ProseLabel as _, prose};
use crate::t;

/// 这一部分各行左边标签的宽度。
pub(super) const LABEL_WIDTH: f32 = 6.;

/// 监听范围，按界面上的顺序。
const SCOPES: [HttpScope; 2] = [HttpScope::Local, HttpScope::Lan];

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
pub(super) fn masked_token(kind: TokenKind) -> String {
    format!("{}••••••••", kind.prefix())
}

/// 界面元素 ID 中区分两个令牌的部分。
fn token_id(kind: TokenKind, part: &str) -> ElementId {
    let base = match kind {
        TokenKind::Local => "cli-http-token",
        TokenKind::Lan => "cli-http-lan-token",
    };
    if part.is_empty() {
        ElementId::Name(base.into())
    } else {
        ElementId::Name(format!("{base}-{part}").into())
    }
}

/// 读到的令牌。
#[derive(Clone, Debug)]
pub(super) enum TokenState {
    Loading,
    View(TokenView),
    /// 读取失败（数据库出错）
    Failed(String),
}

type AddressEvent = SelectEvent<SearchableVec<Choice<Ipv4Addr>>>;

pub(crate) struct HttpPanel {
    /// “允许 AI 工具与脚本访问”的草稿（这一页最上面的开关，由页面同步过来）：关着时 HTTP 不监听
    cli_enabled: bool,
    /// “监听 HTTP”的草稿
    pub(super) enabled: bool,
    /// 监听范围的草稿
    pub(super) scope: HttpScope,
    port: Entity<InputState>,
    /// 端口输入框中的原文（随输入更新）
    port_text: String,
    /// 打开对话框时本机设置中的端口（草稿的端口不能用时，地址按它写）
    saved_port: u16,
    /// 实际的状态（命令行服务送来；服务没有启动时一直是 `Off`）
    pub(super) status: HttpStatus,
    /// 本机的令牌
    pub(super) token: TokenState,
    /// 局域网的令牌
    pub(super) lan_token: TokenState,
    /// 本机的令牌显示原文（默认打码）
    pub(super) revealed: bool,
    /// 局域网的令牌显示原文（默认打码）
    pub(super) lan_revealed: bool,
    /// 这台电脑在局域网中的地址；还在读时为 `None`
    pub(super) lan_addresses: Option<Vec<LanAddress>>,
    /// 有几个地址时选用哪一个
    pub(super) lan_select: Option<Entity<ChoiceState<Ipv4Addr>>>,
    /// 正在重新生成令牌
    busy: bool,
    _watch: Option<Task<()>>,
    _token: Option<Task<()>>,
    _lan: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl HttpPanel {
    /// `lan_addresses` 为 `None` 时在后台读这台电脑的网卡（测试中给定）。
    pub(super) fn new(
        cli_enabled: bool,
        settings: &CliHttpSettings,
        lan_addresses: Option<Vec<LanAddress>>,
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
            scope: settings.scope,
            port,
            port_text,
            saved_port: settings.port,
            status,
            token: TokenState::Loading,
            lan_token: TokenState::Loading,
            revealed: false,
            lan_revealed: false,
            lan_addresses: None,
            lan_select: None,
            busy: false,
            _watch: watch,
            _token: None,
            _lan: None,
            _subscriptions: vec![subscription],
        };
        this.load_tokens(cx);
        match lan_addresses {
            Some(addresses) => this.set_lan_addresses(addresses, window, cx),
            None => {
                this._lan = Some(cx.spawn_in(window, async move |this, cx| {
                    let addresses = cx
                        .background_spawn(async { termlet_core::cli::http::lan::addresses() })
                        .await;
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.set_lan_addresses(addresses, window, cx);
                    });
                }));
            }
        }
        this
    }

    /// 读出了这台电脑在局域网中的地址：有几个时建下拉框（默认第一个：有默认网关的在前）。
    fn set_lan_addresses(
        &mut self,
        addresses: Vec<LanAddress>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if addresses.len() > 1 {
            let choices = addresses
                .iter()
                .map(|a| Choice::new(address_label(a), a.ip))
                .collect();
            let select = choice_state(choices, &addresses[0].ip, false, window, cx);
            self._subscriptions
                .push(cx.subscribe(&select, |_, _, _: &AddressEvent, cx| cx.notify()));
            self.lan_select = Some(select);
        }
        self.lan_addresses = Some(addresses);
        cx.notify();
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
            self.load_tokens(cx);
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

    /// 正在重新生成令牌。
    pub(super) fn is_busy(&self) -> bool {
        self.busy
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
        settings.scope = self.scope;
        if let Some(port) = self.draft_port() {
            settings.port = port;
        }
    }

    /// 按草稿保存后要监听的端口与范围；不监听时为 `None`。
    fn wanted(&self) -> Option<(u16, HttpScope)> {
        (self.cli_enabled && self.enabled).then(|| (self.draft_port().unwrap_or(0), self.scope))
    }

    /// 草稿与实际的状态不同：保存后才生效。
    pub(super) fn pending(&self) -> bool {
        match (self.wanted(), &self.status) {
            (None, HttpStatus::Off) => false,
            (
                Some(want),
                HttpStatus::Listening { port, scope } | HttpStatus::Failed { port, scope, .. },
            ) => want != (*port, *scope),
            _ => true,
        }
    }

    /// 地址中的端口：在监听时按实际的端口，否则按草稿的端口。
    fn port(&self) -> u16 {
        match self.status {
            HttpStatus::Listening { port, .. } => port,
            _ => self.draft_port().unwrap_or(self.saved_port),
        }
    }

    /// 这台电脑上的 AI 工具用的地址。
    pub(super) fn url(&self) -> String {
        http_url(self.port())
    }

    /// 选用的局域网地址；还在读、或没有时为 `None`。
    pub(super) fn lan_ip(&self, cx: &App) -> Option<Ipv4Addr> {
        match &self.lan_select {
            Some(select) => chosen(select, cx),
            None => self.lan_addresses.as_ref()?.first().map(|a| a.ip),
        }
    }

    /// 局域网中的另一台电脑用的地址（按选用的局域网地址）。
    pub(super) fn lan_url(&self, cx: &App) -> Option<String> {
        self.lan_ip(cx)
            .map(|ip| http_url_at(&ip.to_string(), self.port()))
    }

    fn token_state(&self, kind: TokenKind) -> &TokenState {
        match kind {
            TokenKind::Local => &self.token,
            TokenKind::Lan => &self.lan_token,
        }
    }

    fn revealed(&self, kind: TokenKind) -> bool {
        match kind {
            TokenKind::Local => self.revealed,
            TokenKind::Lan => self.lan_revealed,
        }
    }

    fn set_revealed(&mut self, kind: TokenKind, revealed: bool) {
        match kind {
            TokenKind::Local => self.revealed = revealed,
            TokenKind::Lan => self.lan_revealed = revealed,
        }
    }

    /// 草稿中这个令牌用得上：两个开关都开着（局域网的另要监听范围为局域网）。
    fn token_in_use(&self, kind: TokenKind) -> bool {
        self.cli_enabled
            && self.enabled
            && (kind == TokenKind::Local || self.scope == HttpScope::Lan)
    }

    /// 复制配置用的令牌原文：草稿中用得上、令牌读得出来时才有（否则预览中写占位）。
    pub(super) fn token_for_copy(&self, kind: TokenKind) -> Option<String> {
        match self.token_state(kind) {
            TokenState::View(TokenView::Ready(token)) if self.token_in_use(kind) => {
                Some(token.expose().to_string())
            }
            _ => None,
        }
    }

    /// 还不能复制配置时的原因。
    pub(super) fn copy_blocked(&self, kind: TokenKind) -> Option<SharedString> {
        if self.token_for_copy(kind).is_some() {
            return None;
        }
        let off = match kind {
            TokenKind::Local => t!("cli_ui.http.copy_later_off"),
            TokenKind::Lan => t!("cli_ui.http.lan_copy_later_off"),
        };
        Some(match self.token_state(kind) {
            _ if !self.token_in_use(kind) => off,
            TokenState::View(TokenView::Locked) => t!("cli_ui.http.copy_later_locked"),
            TokenState::View(TokenView::Lost) => t!("cli_ui.http.copy_later_lost"),
            _ => off,
        })
    }

    /// 在后台读两个令牌。
    fn load_tokens(&mut self, cx: &mut Context<Self>) {
        let store = AppState::store(cx);
        self._token = Some(cx.spawn(async move |this, cx| {
            let (local, lan) = cx
                .background_spawn(async move {
                    (
                        token::view(&store, TokenKind::Local),
                        token::view(&store, TokenKind::Lan),
                    )
                })
                .await;
            let state = |result: termlet_core::Result<TokenView>| match result {
                Ok(view) => TokenState::View(view),
                Err(e) => {
                    tracing::warn!("读取 MCP 的 HTTP 令牌失败：{e}");
                    TokenState::Failed(e.to_string())
                }
            };
            let _ = this.update(cx, |this, cx| {
                this.token = state(local);
                this.lan_token = state(lan);
                cx.notify();
            });
        }));
    }

    fn confirm_regenerate(&mut self, kind: TokenKind, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.entity().downgrade();
        let (title, body) = match kind {
            TokenKind::Local => (
                t!("cli_ui.http.regenerate_title"),
                t!("cli_ui.http.regenerate_body"),
            ),
            TokenKind::Lan => (
                t!("cli_ui.http.lan_regenerate_title"),
                t!("cli_ui.http.lan_regenerate_body"),
            ),
        };
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let view = view.clone();
            alert
                .confirm()
                .title(title.clone())
                .description(prose(body.clone()))
                .ok_text(t!("cli_ui.http.regenerate_ok"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, window, cx| {
                    let _ = view.update(cx, |view, cx| view.regenerate(kind, window, cx));
                    true
                })
        });
    }

    /// 重新生成令牌（立即生效，用旧令牌的 AI 工具随即被拒绝；另一个令牌不变）。
    pub(super) fn regenerate(
        &mut self,
        kind: TokenKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy {
            return;
        }
        self.busy = true;
        let store = AppState::store(cx);
        self._token = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { token::regenerate(&store, kind) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                let notice = match result {
                    Ok(token) => {
                        let view = TokenState::View(TokenView::Ready(token));
                        match kind {
                            TokenKind::Local => this.token = view,
                            TokenKind::Lan => this.lan_token = view,
                        }
                        this.set_revealed(kind, false);
                        Notice::success(match kind {
                            TokenKind::Local => t!("cli_ui.http.regenerated"),
                            TokenKind::Lan => t!("cli_ui.http.lan_regenerated"),
                        })
                    }
                    Err(CoreError::VaultLocked) => {
                        Notice::error(t!("cli_ui.http.regenerate_locked"))
                    }
                    Err(e) => {
                        tracing::warn!(?kind, "重新生成 MCP 的 HTTP 令牌失败：{e}");
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
            HttpStatus::Listening {
                port,
                scope: HttpScope::Local,
            } => (
                t!("cli_ui.http.status_listening", port = port),
                theme.success,
            ),
            HttpStatus::Listening {
                port,
                scope: HttpScope::Lan,
            } => (
                t!("cli_ui.http.status_listening_lan", port = port),
                theme.success,
            ),
            HttpStatus::Off if !self.cli_enabled => {
                (t!("cli_ui.http.status_needs_cli"), theme.muted_foreground)
            }
            HttpStatus::Off => (t!("cli_ui.http.status_off"), theme.muted_foreground),
            HttpStatus::Failed { port, error, .. } => {
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

    fn render_scope(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex().child(
            TabBar::new("cli-http-scope")
                .segmented()
                .small()
                .selected_index(
                    SCOPES
                        .iter()
                        .position(|s| *s == self.scope)
                        .unwrap_or_default(),
                )
                .on_click(cx.listener(|this, ix: &usize, _, cx| {
                    if let Some(scope) = SCOPES.get(*ix) {
                        this.scope = *scope;
                        cx.notify();
                    }
                }))
                .child(Tab::new().label(t!("cli_ui.http.scope_local")))
                .child(Tab::new().label(t!("cli_ui.http.scope_lan"))),
        )
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

    /// 地址与复制按钮。
    fn url_line(id: &'static str, url: String, mono: SharedString) -> impl IntoElement {
        h_flex()
            .gap_1()
            .child(
                div()
                    .id(id)
                    .test_support()
                    .text_sm()
                    .font_family(mono)
                    .child(url.clone()),
            )
            .child(
                Button::new(SharedString::from(format!("{id}-copy")))
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

    /// 局域网地址：有几个时可选，下面是另一台电脑用的地址。
    fn render_lan_address(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, mono) = (theme.muted_foreground, theme.mono_font_family.clone());
        let note = |text: SharedString| {
            div()
                .id("cli-http-lan-none")
                .test_support()
                .text_sm()
                .text_color(muted)
                .child(prose(text))
        };
        let Some(addresses) = &self.lan_addresses else {
            return note(t!("cli_ui.http.lan_loading")).into_any_element();
        };
        let Some(first) = addresses.first() else {
            return note(t!("cli_ui.http.lan_none")).into_any_element();
        };
        let picker = match &self.lan_select {
            Some(select) => div()
                .w(rems(22.))
                .child(Select::new(select).small().id("cli-http-lan-ip"))
                .into_any_element(),
            None => div()
                .id("cli-http-lan-ip")
                .test_support()
                .text_sm()
                .child(address_label(first))
                .into_any_element(),
        };
        v_flex()
            .gap_1()
            .child(picker)
            .when_some(self.lan_url(cx), |this, url| {
                this.child(Self::url_line("cli-http-lan-url", url, mono))
            })
            .into_any_element()
    }

    fn render_token(&self, kind: TokenKind, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let mono = theme.mono_font_family.clone();
        let note = |text: SharedString, color: Hsla| {
            div()
                .id(token_id(kind, "note"))
                .test_support()
                .text_sm()
                .text_color(color)
                .child(prose(text))
        };
        let regenerate = Button::new(token_id(kind, "regenerate"))
            .outline()
            .small()
            .icon(IconName::RefreshCw)
            .label(t!("cli_ui.http.regenerate"))
            .disabled(self.busy)
            .on_click(
                cx.listener(move |this, _, window, cx| this.confirm_regenerate(kind, window, cx)),
            );
        let revealed = self.revealed(kind);
        match self.token_state(kind) {
            TokenState::View(TokenView::Ready(token)) => {
                let shown = if revealed {
                    token.expose().to_string()
                } else {
                    masked_token(kind)
                };
                let secret = token.expose().to_string();
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    // 显示原文时独占一行、放不下时折行（令牌中间没有可以断开的地方），按钮排到下一行
                    .child(
                        div()
                            .id(token_id(kind, ""))
                            .test_support()
                            .min_w_0()
                            .when(revealed, |this| this.w_full())
                            .text_sm()
                            .font_family(mono)
                            .child(shown),
                    )
                    .child(
                        Button::new(token_id(kind, "reveal"))
                            .ghost()
                            .small()
                            .icon(if revealed {
                                IconName::EyeOff
                            } else {
                                IconName::Eye
                            })
                            .label(if revealed {
                                t!("cli_ui.http.hide")
                            } else {
                                t!("cli_ui.http.show")
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.set_revealed(kind, !this.revealed(kind));
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new(token_id(kind, "copy"))
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
                let text = match kind {
                    TokenKind::Local => t!("cli_ui.http.token_missing"),
                    TokenKind::Lan => t!("cli_ui.http.lan_token_missing"),
                };
                note(text, muted).into_any_element()
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

    /// 局域网的风险与限制：明文、防火墙、批准框在这台电脑上、没有传文件的工具。
    fn render_lan_notes(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (warning, muted) = (theme.warning, theme.muted_foreground);
        let firewall = if cfg!(windows) {
            t!("cli_ui.http.lan_firewall_windows")
        } else {
            t!(
                "cli_ui.http.lan_firewall_other",
                port = self.draft_port().unwrap_or(self.saved_port)
            )
        };
        let line = |id: &'static str, text: SharedString, color: Hsla| {
            div()
                .id(id)
                .test_support()
                .text_xs()
                .text_color(color)
                .child(prose(text))
        };
        v_flex()
            .gap_1()
            .child(line(
                "cli-http-lan-plaintext",
                t!("cli_ui.http.lan_plaintext"),
                warning,
            ))
            .child(line("cli-http-lan-firewall", firewall, muted))
            .child(line(
                "cli-http-lan-approval",
                t!(
                    "cli_ui.http.lan_approval",
                    seconds = APPROVAL_TIMEOUT.as_secs()
                ),
                muted,
            ))
            .child(line(
                "cli-http-lan-tools",
                t!("cli_ui.http.lan_tools"),
                muted,
            ))
    }
}

/// 下拉框与说明中的一个局域网地址：IP 与网卡的名字。
fn address_label(address: &LanAddress) -> SharedString {
    if address.interface.is_empty() {
        address.ip.to_string().into()
    } else {
        t!(
            "cli_ui.http.lan_address_item",
            ip = address.ip.to_string(),
            interface = address.interface.clone()
        )
    }
}

impl Render for HttpPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let mono = theme.mono_font_family.clone();
        let port_ok = self.draft_port().is_some();
        let lan = self.scope == HttpScope::Lan;
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
            .child(row(t!("cli_ui.http.scope"), muted, self.render_scope(cx)))
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
            .child(row(
                t!("cli_ui.http.address"),
                muted,
                Self::url_line("cli-http-url", self.url(), mono),
            ))
            .child(row(
                t!("cli_ui.http.token"),
                muted,
                self.render_token(TokenKind::Local, cx),
            ))
            .when(lan, |this| {
                this.child(row(
                    t!("cli_ui.http.lan_address"),
                    muted,
                    self.render_lan_address(cx),
                ))
                .child(row(
                    t!("cli_ui.http.lan_token"),
                    muted,
                    self.render_token(TokenKind::Lan, cx),
                ))
                .child(self.render_lan_notes(cx))
            })
            .child(div().text_xs().text_color(muted).child(prose(note)))
    }
}
