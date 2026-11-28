//! “把本机代理借给服务器”的对话框（终端的 Tab 菜单、状态条）：把本机的 HTTP 代理借给服务器（SSH 远程转发，服务器上只监听
//! `127.0.0.1`），服务器连不上外网时装包、拉镜像用。
//!
//! - 开启前：本机代理（设置中填了的，没填时用系统代理）与服务器上的端口（留空由服务器选），开启时记进本机设置。
//! - 开启后：服务器上使用这个代理的命令片段与撤销命令（环境变量、apt、dnf、yum、Docker 守护进程、docker build），
//!   可复制，环境变量可一键发到终端；可以停止。随这个终端的会话结束而停止（见 [`TerminalPanel::start_proxy_lend`]）。

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_kit::component::form::{field, v_form};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, ClickEvent, ClipboardItem, Context, Entity, Focusable as _,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Subscription, Task, TestSupportExt as _, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::forward::{
    ProxyAddress, ProxySource, ShareError, SnippetKind, local_proxy, proxy_snippet,
};

use crate::app_state::AppState;
use crate::notify::{self, Notice, NoticeLine};
use crate::prose::prose;
use crate::snippets::CommandInput;
use crate::t;
use crate::terminal_panel::TerminalPanel;
use crate::ui_scale::DialogTitleExt as _;

/// 打开“把本机代理借给服务器”：还没开启时是开启的表单，开启着时是命令片段与“停止”。
pub fn open(terminal: Entity<TerminalPanel>, window: &mut Window, cx: &mut App) {
    let view = cx.new(|cx| ProxyShareView::new(terminal.clone(), window, cx));
    let notice = view.read(cx).notice.clone();
    let first_focus = Rc::new(Cell::new(true));
    let width = rems(38.).to_pixels(window.rem_size());
    window.open_dialog(cx, move |dialog, window, cx| {
        let (running, starting) = {
            let view = view.read(cx);
            (view.running(cx), view.starting)
        };
        if first_focus.replace(false) && !running {
            view.read(cx).address.focus_handle(cx).focus(window, cx);
        }
        let footer = if running {
            let stop = view.clone();
            DialogFooter::new()
                .child(
                    div().size_full().child(
                        Button::new("proxy-stop")
                            .danger()
                            .outline()
                            .w_full()
                            .label(t!("terminal.proxy.stop"))
                            .on_click(move |_, _, cx| stop.update(cx, |view, cx| view.stop(cx))),
                    ),
                )
                .child(
                    DialogClose::new().child(
                        Button::new("proxy-close")
                            .primary()
                            .label(t!("common.close")),
                    ),
                )
        } else {
            DialogFooter::new()
                .child(
                    DialogClose::new()
                        .child(Button::new("proxy-cancel").label(t!("common.cancel"))),
                )
                .child(
                    DialogAction::new().child(
                        Button::new("proxy-start")
                            .primary()
                            .loading(starting)
                            .label(t!("terminal.proxy.start")),
                    ),
                )
        };
        let confirm = view.clone();
        // 关闭后焦点回到终端
        let terminal = terminal.clone();
        dialog
            .titled(t!("terminal.proxy.title"))
            .w(width)
            .overlay_closable(false)
            .child(view.clone())
            .on_ok(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                confirm.update(cx, |view, cx| view.confirm(window, cx))
            })
            .footer(footer)
            .on_close(move |_, window, cx| terminal.focus_handle(cx).focus(window, cx))
    });
    notify::attach(&notice, window, cx);
}

struct ProxyShareView {
    terminal: Entity<TerminalPanel>,
    address: Entity<InputState>,
    port: Entity<InputState>,
    /// 打开时用的本机代理与出处（设置中填的，或找到的系统代理）
    detected: Option<(ProxyAddress, ProxySource)>,
    error: Option<SharedString>,
    starting: bool,
    kind: SnippetKind,
    /// 复制、发到终端的结果（弹出的通知会被对话框盖住，见 [`crate::notify::show`]）
    notice: Entity<NoticeLine>,
    _start: Option<Task<()>>,
    _observe: Subscription,
}

impl ProxyShareView {
    fn new(terminal: Entity<TerminalPanel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let local = AppState::store(cx).local_settings().connection;
        let detected = local_proxy(&local.local_proxy);
        let address_text = if local.local_proxy.trim().is_empty() {
            detected
                .as_ref()
                .map(|(a, _)| a.display())
                .unwrap_or_default()
        } else {
            local.local_proxy.clone()
        };
        let address = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("terminal.proxy.local_placeholder"))
                .default_value(address_text)
        });
        let port_text = match local.local_proxy_port {
            0 => String::new(),
            port => port.to_string(),
        };
        let port = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("terminal.proxy.port_placeholder"))
                .default_value(port_text)
        });
        // 终端那边开启、停止（含连接断开）时跟着变
        let observe = cx.observe(&terminal, |_, _, cx| cx.notify());
        Self {
            terminal,
            address,
            port,
            detected,
            error: None,
            starting: false,
            kind: SnippetKind::Shell,
            _start: None,
            notice: NoticeLine::new(cx),
            _observe: observe,
        }
    }

    fn running(&self, cx: &App) -> bool {
        self.terminal.read(cx).proxy_lend().is_some()
    }

    /// Enter 与“开启”：开启着时关闭对话框，否则开始开启（对话框留着，完成后显示命令片段）。
    fn confirm(&mut self, _: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.running(cx) {
            return true;
        }
        self.start(cx);
        false
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        if self.starting {
            return;
        }
        let text = self.address.read(cx).value().trim().to_string();
        let Some(local) = ProxyAddress::parse(&text) else {
            self.error = Some(if text.is_empty() {
                t!("terminal.proxy.error.address_empty")
            } else {
                t!("terminal.proxy.error.address", address = text)
            });
            cx.notify();
            return;
        };
        let port_text = self.port.read(cx).value().trim().to_string();
        let port = if port_text.is_empty() {
            0
        } else {
            match port_text.parse::<u16>() {
                Ok(port) => port,
                Err(_) => {
                    self.error = Some(t!("terminal.proxy.error.port", port = port_text));
                    cx.notify();
                    return;
                }
            }
        };
        // 记进本机设置：预填的是找到的系统代理且没改过时不记（之后仍跟随系统代理）
        let follows_system = self
            .detected
            .as_ref()
            .is_some_and(|(a, source)| *source != ProxySource::Settings && a.display() == text);
        let store = AppState::store(cx);
        if let Err(e) = store.update_local_settings(|s| {
            if !follows_system {
                s.connection.local_proxy = text.clone();
            }
            s.connection.local_proxy_port = port;
        }) {
            tracing::warn!("保存借给服务器的本机代理设置失败：{e}");
        }
        self.error = None;
        self.starting = true;
        let task = self.terminal.update(cx, |terminal, cx| {
            terminal.start_proxy_lend(local, port, cx)
        });
        self._start = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.starting = false;
                if let Err(error) = result {
                    tracing::info!("把本机代理借给服务器失败：{error}");
                    this.error = Some(describe(&error));
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        self.terminal
            .update(cx, |terminal, cx| terminal.stop_proxy_lend(cx));
        cx.notify();
    }

    /// 把环境变量的命令发到终端执行。
    fn send_to_terminal(
        terminal: &Entity<TerminalPanel>,
        command: &str,
        window: &mut Window,
        cx: &mut App,
    ) {
        if terminal.read(cx).is_alt_screen(cx) {
            notify::show(
                Notice::warning(t!("terminal.proxy.send_full_screen")),
                window,
                cx,
            );
            return;
        }
        let input = CommandInput::Type(format!("{command}\r"));
        let notice = match terminal.update(cx, |terminal, cx| terminal.write_command(input, cx)) {
            Ok(()) => Notice::success(t!("terminal.proxy.sent")),
            Err(error) => Notice::error(error),
        };
        notify::show(notice, window, cx);
    }

    fn render_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let source_hint = match &self.detected {
            Some((_, ProxySource::System)) => t!("terminal.proxy.local_from_system"),
            Some((_, ProxySource::Environment)) => t!("terminal.proxy.local_from_env"),
            Some((_, ProxySource::Settings)) => t!("terminal.proxy.local_from_settings"),
            None => t!("terminal.proxy.local_not_found"),
        };
        let danger = cx.theme().danger;
        v_flex()
            .gap_3()
            .child(div().text_sm().child(prose(t!("terminal.proxy.intro"))))
            .child(
                v_form()
                    .child(
                        field()
                            .label(t!("terminal.proxy.local"))
                            .description(prose(source_hint))
                            .child(Input::new(&self.address).id("proxy-local")),
                    )
                    .child(
                        field()
                            .label(t!("terminal.proxy.port"))
                            .description(prose(t!("terminal.proxy.port_hint")))
                            .child(Input::new(&self.port).id("proxy-port")),
                    ),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .id("proxy-error")
                        .test_support()
                        .text_sm()
                        .text_color(danger)
                        .child(prose(error)),
                )
            })
    }

    fn render_running(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let lend = self.terminal.read(cx).proxy_lend();
        let Some(lend) = lend else {
            return div().into_any_element();
        };
        let port = lend.share.port();
        let local = lend.share.local().display();
        let busy = lend.share.busy_port();
        let status = lend.status.clone();
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let snippet = proxy_snippet(self.kind, port);
        let kinds = SnippetKind::ALL;
        let selected = kinds.iter().position(|k| *k == self.kind).unwrap_or(0);
        let shell = self.kind == SnippetKind::Shell;
        v_flex()
            .gap_3()
            .child(
                div()
                    .id("proxy-running")
                    .test_support()
                    .text_sm()
                    .child(prose(t!(
                        "terminal.proxy.running",
                        port = port,
                        local = local
                    ))),
            )
            .when_some(busy, |this, wanted| {
                this.child(div().text_xs().text_color(muted).child(prose(t!(
                    "terminal.proxy.port_busy",
                    wanted = wanted,
                    port = port
                ))))
            })
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(t!("terminal.proxy.connections", count = status.connections)),
            )
            .when_some(status.last_error, |this, error| {
                this.child(
                    div()
                        .id("proxy-last-error")
                        .test_support()
                        .text_xs()
                        .text_color(danger)
                        .child(prose(t!(
                            "terminal.proxy.local_failed",
                            error = error.to_string()
                        ))),
                )
            })
            .child(
                h_flex().child(
                    TabBar::new("proxy-kinds")
                        .segmented()
                        .small()
                        .selected_index(selected)
                        .on_click(cx.listener(move |this, ix: &usize, _, cx| {
                            if let Some(kind) = kinds.get(*ix) {
                                this.kind = *kind;
                                cx.notify();
                            }
                        }))
                        .children(kinds.map(|kind| Tab::new().label(kind_label(kind)))),
                ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(prose(kind_hint(self.kind))),
            )
            .child(self.code_row(
                "proxy-apply",
                t!("terminal.proxy.apply"),
                snippet.apply,
                shell,
                cx,
            ))
            .when_some(snippet.undo, |this, undo| {
                this.child(self.code_row("proxy-undo", t!("terminal.proxy.undo"), undo, shell, cx))
            })
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(prose(t!("terminal.proxy.others"))),
            )
            .into_any_element()
    }

    /// 一段命令：标题、命令（等宽、可换行）、复制，`send` 时还有“发到终端”。
    fn code_row(
        &self,
        id: &'static str,
        label: SharedString,
        command: String,
        send: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, code_bg, radius) = (theme.muted_foreground, theme.muted, theme.radius);
        let mono = theme.mono_font_family.clone();
        let copy = command.clone();
        let sent = command.clone();
        let terminal = self.terminal.clone();
        v_flex()
            .gap_1()
            .child(div().text_xs().text_color(muted).child(label))
            .child(
                h_flex()
                    .items_start()
                    .gap_2()
                    .child(
                        div()
                            .id(id)
                            .test_support()
                            .flex_1()
                            .min_w_0()
                            .px_2()
                            .py_1()
                            .rounded(radius)
                            .bg(code_bg)
                            .text_xs()
                            .font_family(mono)
                            .child(command),
                    )
                    .child(
                        Button::new((id, 0usize))
                            .outline()
                            .small()
                            .icon(IconName::Copy)
                            .tooltip(t!("terminal.proxy.copy"))
                            .on_click(move |_, window, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                                notify::show(
                                    Notice::success(t!("terminal.proxy.copied")),
                                    window,
                                    cx,
                                );
                            }),
                    )
                    .when(send, |row| {
                        row.child(
                            Button::new((id, 1usize))
                                .outline()
                                .small()
                                .icon(IconName::Send)
                                .tooltip(t!("terminal.proxy.send"))
                                .on_click(move |_, window, cx| {
                                    Self::send_to_terminal(&terminal, &sent, window, cx)
                                }),
                        )
                    }),
            )
    }
}

impl Render for ProxyShareView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = if self.running(cx) {
            self.render_running(cx).into_any_element()
        } else {
            self.render_form(cx).into_any_element()
        };
        v_flex()
            .gap_3()
            .child(content)
            .when(self.notice.read(cx).is_shown(), |this| {
                this.child(div().text_sm().child(self.notice.clone()))
            })
    }
}

/// 开启失败的原因。
fn describe(error: &ShareError) -> SharedString {
    match error {
        ShareError::Denied => t!("terminal.proxy.error.denied"),
        ShareError::Disconnected => t!("terminal.proxy.error.disconnected"),
        ShareError::Other(reason) => t!("terminal.proxy.error.other", reason = reason),
    }
}

fn kind_label(kind: SnippetKind) -> SharedString {
    match kind {
        SnippetKind::Shell => t!("terminal.proxy.kind.shell"),
        SnippetKind::Apt => "apt".into(),
        SnippetKind::Dnf => "dnf".into(),
        SnippetKind::Yum => "yum".into(),
        SnippetKind::DockerDaemon => "Docker".into(),
        SnippetKind::DockerBuild => "docker build".into(),
    }
}

fn kind_hint(kind: SnippetKind) -> SharedString {
    match kind {
        SnippetKind::Shell => t!("terminal.proxy.hint.shell"),
        SnippetKind::Apt => t!("terminal.proxy.hint.apt"),
        SnippetKind::Dnf => t!("terminal.proxy.hint.dnf"),
        SnippetKind::Yum => t!("terminal.proxy.hint.yum"),
        SnippetKind::DockerDaemon => t!("terminal.proxy.hint.docker_daemon"),
        SnippetKind::DockerBuild => t!("terminal.proxy.hint.docker_build"),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};

    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{Bounds, TestAppContext, WindowBounds, WindowOptions, px, size};
    use termlet_core::model::{Host, SecretField};
    use termlet_core::secret::SecretString;
    use termlet_core::store::{Owner, Position};

    use super::*;
    use crate::testkit::{test_server, wait_until};

    /// 假的本机 HTTP 代理：只要有一个连得上的端口。
    fn fake_proxy() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            }
        });
        port
    }

    // 连真实的 sshd（TERMLET_TEST_SSH）：在终端上开启“把本机代理借给服务器”，对话框给出命令片段、状态条上显示；
    // 停止后对话框回到开启前的表单，状态条上没有了
    #[gpui_kit::test]
    fn lends_and_stops_from_the_dialog(cx: &mut TestAppContext) {
        let Some((address, port)) = test_server() else {
            return;
        };
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        cx.update(crate::init);
        cx.update(AppState::install_in_memory);
        cx.update(|cx| cx.set_reduce_motion(true));
        let (handle, workspace) = cx
            .update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                            Default::default(),
                            size(px(1024.), px(640.)),
                        ))),
                        ..Default::default()
                    },
                    cx,
                    |window, cx| cx.new(|cx| crate::Workspace::new(window, cx)),
                )
            })
            .expect("打开测试窗口");
        let proxy = fake_proxy();
        let host = cx.update(|cx| {
            let store = AppState::store(cx);
            store
                .update_local_settings(|s| s.connection.local_proxy = format!("127.0.0.1:{proxy}"))
                .unwrap();
            let id = store
                .create(
                    Host {
                        name: "proxy".into(),
                        address,
                        port,
                        username: "termlet".into(),
                        ..Default::default()
                    },
                    None,
                    Position::Last,
                )
                .unwrap();
            store
                .set_secret(
                    &Owner::host(&id),
                    SecretField::Password,
                    Some(&SecretString::new("termlet-e2e")),
                )
                .unwrap();
            id
        });
        cx.update_window(handle, |_, window, cx| {
            workspace.update(cx, |w, cx| w.open_host(&host, window, cx))
        })
        .unwrap();
        wait_until(cx, handle, "终端连上", |window, cx| {
            if window.try_find("prompt-ok").is_some() {
                window.click("prompt-ok", cx);
            }
            workspace
                .read(cx)
                .terminals_of(&host, cx)
                .first()
                .is_some_and(|t| t.read(cx).can_lend_proxy())
        });
        let terminal = cx.update(|cx| workspace.read(cx).terminals_of(&host, cx)[0].clone());
        cx.update_window(handle, |_, window, cx| open(terminal.clone(), window, cx))
            .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("proxy-start", cx);
        })
        .unwrap();
        wait_until(cx, handle, "开启后显示命令片段", |window, _| {
            window.try_find("proxy-running").is_some()
        });
        let id = terminal.entity_id().as_u64() as usize;
        cx.update_window(handle, |_, window, cx| {
            let lend = terminal.read(cx).proxy_lend().expect("开启着");
            assert_eq!(lend.share.local().port, proxy);
            assert_ne!(lend.share.port(), 0);
            assert!(window.try_find("proxy-apply").is_some());
            assert!(window.try_find("proxy-undo").is_some());
            assert!(
                window.try_find(("proxy-lend", id)).is_some(),
                "状态条上显示"
            );
            window.click("proxy-stop", cx);
        })
        .unwrap();
        wait_until(
            cx,
            handle,
            "停止后回到开启前的表单",
            |window, cx| {
                terminal.read(cx).proxy_lend().is_none() && window.try_find("proxy-local").is_some()
            },
        );
        cx.update_window(handle, |_, window, _| {
            assert!(window.try_find(("proxy-lend", id)).is_none());
        })
        .unwrap();
    }
}
