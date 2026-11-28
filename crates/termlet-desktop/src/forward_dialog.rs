//! 新建 / 编辑端口转发：类型（本地、远程、动态）、经由的主机、名称、监听地址与端口、目标、
//! “Termlet 启动后自动启动”；从终端打开时可以“保存后立即启动，共用已打开的终端连接”，本地转发还可以
//! 检测服务器上正在监听的端口一键填入。

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_kit::component::form::{field, v_form};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Task, WeakEntity, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::id::{ForwardId, HostId};
use termlet_core::model::{
    ANY_IPV4, ANY_IPV6, Forward, ForwardKind, Host, Protocol, is_any_address,
};
use termlet_core::store::{Position, Store};

use crate::app_state::AppState;
use crate::choice::{Choice, ChoiceState, choice_state, chosen};
use crate::prose::{ProseLabel as _, prose};
use crate::terminal_panel::TerminalPanel;
use crate::{forwards, store_errors, t};

const KINDS: [ForwardKind; 3] = [
    ForwardKind::Local,
    ForwardKind::Remote,
    ForwardKind::Dynamic,
];

/// 保存之后（从终端打开时切到侧边栏的“转发”页）。
pub type OnSaved = Rc<dyn Fn(&mut Window, &mut App)>;

/// 新建转发：`host` 为默认的主机，`terminal` 为从哪个终端打开（可以共用它的连接、检测它的服务器端口）。
pub fn open_new(
    host: Option<HostId>,
    terminal: Option<WeakEntity<TerminalPanel>>,
    on_saved: Option<OnSaved>,
    window: &mut Window,
    cx: &mut App,
) {
    open(None, host, terminal, None, on_saved, window, cx);
}

/// 新建转发并预填内容（主机工具网络页的“转发到本机…”）。
pub fn open_preset(
    host: HostId,
    terminal: Option<WeakEntity<TerminalPanel>>,
    preset: Forward,
    on_saved: Option<OnSaved>,
    window: &mut Window,
    cx: &mut App,
) {
    open(
        None,
        Some(host),
        terminal,
        Some(preset),
        on_saved,
        window,
        cx,
    );
}

pub fn open_edit(id: ForwardId, window: &mut Window, cx: &mut App) {
    open(Some(id), None, None, None, None, window, cx);
}

fn open(
    id: Option<ForwardId>,
    host: Option<HostId>,
    terminal: Option<WeakEntity<TerminalPanel>>,
    preset: Option<Forward>,
    on_saved: Option<OnSaved>,
    window: &mut Window,
    cx: &mut App,
) {
    let editing = id.is_some();
    let form = cx.new(|cx| ForwardForm::new(id, host, terminal, preset, on_saved, window, cx));
    let first_focus = Rc::new(Cell::new(true));
    let width = rems(30.).to_pixels(window.rem_size());
    window.open_dialog(cx, move |dialog, window, cx| {
        if first_focus.replace(false) {
            form.read(cx).bind_port.focus_handle(cx).focus(window, cx);
        }
        let save = {
            let form = form.clone();
            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                form.update(cx, |form, cx| form.save(window, cx))
            }
        };
        dialog
            .title(if editing {
                t!("forward.edit")
            } else {
                t!("forward.new")
            })
            .w(width)
            .overlay_closable(false)
            .child(form.clone())
            .on_ok(save)
            .footer(
                DialogFooter::new()
                    .child(
                        DialogClose::new()
                            .child(Button::new("forward-cancel").label(t!("common.cancel"))),
                    )
                    .child(
                        DialogAction::new().child(
                            Button::new("forward-save")
                                .primary()
                                .label(t!("common.save")),
                        ),
                    ),
            )
    });
}

/// 监听地址的选项（随类型变化）；已保存的值不在其中时补在最前面。
fn bind_choices(kind: ForwardKind, current: &str) -> Vec<Choice<String>> {
    let mut choices = match kind {
        ForwardKind::Remote => vec![
            Choice::new(t!("forward.remote_bind_loopback"), "127.0.0.1".to_string()),
            Choice::new(t!("forward.remote_bind_any"), "0.0.0.0".to_string()),
        ],
        _ => vec![
            Choice::new(t!("forward.bind_loopback"), "127.0.0.1".to_string()),
            Choice::new(t!("forward.bind_loopback6"), "::1".to_string()),
            Choice::new(t!("forward.bind_any"), ANY_IPV4.to_string()),
            Choice::new(t!("forward.bind_any6"), ANY_IPV6.to_string()),
        ],
    };
    if !current.is_empty() && !choices.iter().any(|c| c.value == current) {
        choices.insert(0, Choice::new(current.to_string(), current.to_string()));
    }
    choices
}

/// 端口：1–65535；空为 `None`，不是端口为 `Some(None)`。
fn parse_port(text: &str) -> Option<Option<u16>> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(text.parse::<u16>().ok().filter(|p| *p > 0))
}

struct ForwardForm {
    store: Arc<Store>,
    id: Option<ForwardId>,
    /// 编辑时原来的规则（保留不认识的字段）
    original: Forward,
    kind: ForwardKind,
    host: Option<Entity<ChoiceState<HostId>>>,
    /// 编辑时原来经由的主机已删除或不再是 SSH 主机：列在最前面标明不可用，保存前必须换一台
    unavailable_host: Option<HostId>,
    name: Entity<InputState>,
    bind: Entity<ChoiceState<String>>,
    bind_port: Entity<InputState>,
    target: Entity<InputState>,
    target_port: Entity<InputState>,
    auto_start: bool,
    /// 从这个终端打开（共用连接、检测端口）
    terminal: Option<WeakEntity<TerminalPanel>>,
    start_now: bool,
    on_saved: Option<OnSaved>,
    /// 检测到的服务器端口：`None` 为没有检测过
    detected: Option<Vec<forwards::Port>>,
    detecting: bool,
    error: Option<SharedString>,
    _detect: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ForwardForm {
    fn new(
        id: Option<ForwardId>,
        host: Option<HostId>,
        terminal: Option<WeakEntity<TerminalPanel>>,
        preset: Option<Forward>,
        on_saved: Option<OnSaved>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = AppState::store(cx);
        let preset_given = preset.is_some();
        let original = id
            .as_ref()
            .and_then(|id| store.get::<Forward>(id))
            .map(|r| r.doc)
            .or(preset)
            .unwrap_or_default();
        let mut hosts: Vec<Choice<HostId>> = store
            .list::<Host>()
            .into_iter()
            .filter(|h| h.doc.protocol == Protocol::Ssh)
            .map(|h| {
                let title = if h.doc.username.is_empty() {
                    format!("{}（{}）", h.doc.name, h.doc.address)
                } else {
                    format!("{}（{}@{}）", h.doc.name, h.doc.username, h.doc.address)
                };
                Choice::new(title, h.id)
            })
            .collect();
        hosts.sort_by_key(|c| c.title.to_lowercase());
        // 不静默换成列表中的第一台：原来的主机照样选中，标明不可用
        let unavailable_host = original
            .host
            .clone()
            .filter(|h| !hosts.iter().any(|c| &c.value == h));
        if let Some(id) = &unavailable_host {
            let name = forwards::host_name(&store, Some(id));
            hosts.insert(
                0,
                Choice::new(t!("forward.host_unavailable", name = name), id.clone()),
            );
        }
        let selected_host = original
            .host
            .clone()
            .or(host)
            .or_else(|| hosts.first().map(|c| c.value.clone()));
        let host_select = (!hosts.is_empty()).then(|| {
            choice_state(
                hosts.clone(),
                selected_host.as_ref().unwrap_or(&hosts[0].value),
                hosts.len() > 8,
                window,
                cx,
            )
        });
        let editing = id.is_some();
        let input = |value: String,
                     placeholder: SharedString,
                     window: &mut Window,
                     cx: &mut Context<Self>| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        let port_text = |port: u16| {
            if port == 0 {
                String::new()
            } else {
                port.to_string()
            }
        };
        let kind = original.kind;
        let name = input(
            original.name.clone(),
            t!("forward.name_placeholder"),
            window,
            cx,
        );
        let bind = choice_state(
            bind_choices(kind, &original.bind_host),
            &original.bind_host,
            false,
            window,
            cx,
        );
        let bind_port = input(
            port_text(original.bind_port),
            SharedString::default(),
            window,
            cx,
        );
        let target_host = if editing || preset_given {
            original.target_host.clone()
        } else {
            "127.0.0.1".to_string()
        };
        let target = input(target_host, t!("forward.target_placeholder"), window, cx);
        let target_port = input(
            port_text(original.target_port),
            t!("forward.target_port"),
            window,
            cx,
        );
        let mut subscriptions = Vec::new();
        for state in [&bind_port, &target_port, &target, &name] {
            subscriptions.push(cx.subscribe(
                state,
                |this: &mut Self, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.error = None;
                        cx.notify();
                    }
                },
            ));
        }
        if let Some(select) = &host_select {
            // 换了主机：检测到的端口作废
            subscriptions.push(cx.observe(select, |this: &mut Self, _, cx| {
                this.detected = None;
                this._detect = None;
                this.detecting = false;
                cx.notify();
            }));
        }
        let start_now = !editing && terminal.is_some();
        let mut form = Self {
            store,
            id,
            auto_start: original.auto_start,
            original,
            kind,
            host: host_select,
            unavailable_host,
            name,
            bind,
            bind_port,
            target,
            target_port,
            terminal,
            start_now,
            on_saved,
            detected: None,
            detecting: false,
            error: None,
            _detect: None,
            _subscriptions: subscriptions,
        };
        form.refresh_placeholders(window, cx);
        form
    }

    fn selected_host(&self, cx: &App) -> Option<HostId> {
        self.host.as_ref().and_then(|s| chosen(s, cx))
    }

    /// 选着的是已不可用的原主机。
    fn host_unavailable(&self, cx: &App) -> bool {
        self.unavailable_host.is_some() && self.selected_host(cx) == self.unavailable_host
    }

    /// 能共用的终端：从终端打开、所选主机就是它的主机、连接着（只用一个通道的主机不能共用）。
    fn shared_terminal(&self, cx: &App) -> Option<gpui_kit::Entity<TerminalPanel>> {
        let terminal = self.terminal.as_ref()?.upgrade()?;
        let host = self.selected_host(cx)?;
        let panel = terminal.read(cx);
        (panel.host() == Some(&host) && panel.share_group().is_some()).then_some(terminal)
    }

    fn set_kind(&mut self, kind: ForwardKind, window: &mut Window, cx: &mut Context<Self>) {
        if self.kind == kind {
            return;
        }
        // 本机与服务器的监听地址选项不同：换类型后回到默认的回环地址
        let current = chosen(&self.bind, cx).unwrap_or_default();
        let keep = match kind {
            ForwardKind::Remote => ["127.0.0.1", "0.0.0.0"].contains(&current.as_str()),
            _ => ["127.0.0.1", "::1", ANY_IPV4, ANY_IPV6].contains(&current.as_str()),
        };
        let value = if keep {
            current
        } else {
            "127.0.0.1".to_string()
        };
        self.bind = choice_state(bind_choices(kind, &value), &value, false, window, cx);
        self.kind = kind;
        self.error = None;
        self.refresh_placeholders(window, cx);
        cx.notify();
    }

    fn refresh_placeholders(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let placeholder = match self.kind {
            ForwardKind::Remote => t!("forward.remote_port"),
            _ => t!("forward.local_port"),
        };
        self.bind_port.update(cx, |input, cx| {
            input.set_placeholder(placeholder, window, cx)
        });
    }

    /// 端口有没有问题：填了但不在 1–65535 之间。
    fn port_error(&self, cx: &App) -> bool {
        let bind = parse_port(&self.bind_port.read(cx).value());
        let target = parse_port(&self.target_port.read(cx).value());
        matches!(bind, Some(None))
            || (self.kind != ForwardKind::Dynamic && matches!(target, Some(None)))
    }

    fn can_save(&self, cx: &App) -> bool {
        let bind = parse_port(&self.bind_port.read(cx).value()).flatten();
        let target = parse_port(&self.target_port.read(cx).value()).flatten();
        self.selected_host(cx).is_some()
            && bind.is_some()
            && (self.kind == ForwardKind::Dynamic
                || (target.is_some() && !self.target.read(cx).value().trim().is_empty()))
    }

    fn collect(&self, cx: &App) -> Forward {
        let mut doc = self.original.clone();
        doc.kind = self.kind;
        doc.host = self.selected_host(cx);
        doc.name = self.name.read(cx).value().trim().to_string();
        doc.bind_host = chosen(&self.bind, cx).unwrap_or_else(|| "127.0.0.1".into());
        doc.bind_port = parse_port(&self.bind_port.read(cx).value())
            .flatten()
            .unwrap_or(0);
        if self.kind == ForwardKind::Dynamic {
            doc.target_host = String::new();
            doc.target_port = 0;
        } else {
            doc.target_host = self.target.read(cx).value().trim().to_string();
            doc.target_port = parse_port(&self.target_port.read(cx).value())
                .flatten()
                .unwrap_or(0);
        }
        doc.auto_start = self.auto_start;
        doc
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.host_unavailable(cx) {
            self.error = Some(t!("forward.host_unavailable_hint"));
            cx.notify();
            return false;
        }
        if !self.can_save(cx) {
            self.error = Some(t!("forward.incomplete"));
            cx.notify();
            return false;
        }
        let doc = self.collect(cx);
        if let Some(conflict) = forwards::listen_conflict(&self.store, &doc, self.id.as_ref()) {
            self.error = Some(conflict);
            cx.notify();
            return false;
        }
        let shared = self.start_now.then(|| self.shared_terminal(cx)).flatten();
        let was_active = self
            .id
            .as_ref()
            .is_some_and(|id| forwards::status(id, cx).state.is_active());
        let result = match &self.id {
            None => self.store.create(doc, None, Position::Last),
            Some(id) => self.store.update(id, doc).map(|()| id.clone()),
        };
        match result {
            Ok(id) => {
                let editing = self.id.is_some();
                tracing::info!(
                    forward = %id,
                    editing,
                    was_active,
                    start_now = shared.is_some(),
                    "已保存端口转发规则"
                );
                if editing {
                    // 改了运行中的规则：按新的配置重新启动
                    forwards::restart_if_running(&id, was_active, window, cx);
                } else if let Some(terminal) = shared {
                    forwards::start_from_terminal(&id, &terminal, window, cx);
                }
                if let Some(on_saved) = self.on_saved.clone() {
                    on_saved(window, cx);
                }
                true
            }
            Err(e) => {
                tracing::warn!("保存端口转发规则失败：{e}");
                self.error = Some(store_errors::describe(&e));
                cx.notify();
                false
            }
        }
    }

    /// 检测服务器上正在监听的端口（经终端已有的连接执行一次只读命令）。
    fn detect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(terminal) = self.shared_terminal(cx) else {
            return;
        };
        let Some(job) = forwards::detect_ports(&terminal, cx) else {
            return;
        };
        self.detecting = true;
        self.error = None;
        self._detect = Some(cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                this.detecting = false;
                match result {
                    Ok(ports) => this.detected = Some(ports),
                    Err(message) => this.error = Some(message),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// 点了检测到的一个端口：填入目标，监听端口、名称为空时一并填上。
    fn pick(&mut self, port: &forwards::Port, window: &mut Window, cx: &mut Context<Self>) {
        let target = port.target_host().to_string();
        let number = port.port.to_string();
        self.target
            .update(cx, |input, cx| input.set_value(target, window, cx));
        self.target_port
            .update(cx, |input, cx| input.set_value(number.clone(), window, cx));
        if self.bind_port.read(cx).value().trim().is_empty() {
            self.bind_port
                .update(cx, |input, cx| input.set_value(number, window, cx));
        }
        if self.name.read(cx).value().trim().is_empty()
            && let Some(process) = &port.process
        {
            let process = process.clone();
            self.name
                .update(cx, |input, cx| input.set_value(process, window, cx));
        }
        self.detected = None;
        cx.notify();
    }
}

impl Render for ForwardForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, warning, danger) = (theme.muted_foreground, theme.warning, theme.danger);
        let mono = theme.mono_font_family.clone();
        let kind = self.kind;
        let hint = match kind {
            ForwardKind::Local => t!("forward.dialog_hint"),
            ForwardKind::Remote => t!("forward.dialog_hint_remote"),
            ForwardKind::Dynamic => t!("forward.dialog_hint_dynamic"),
        };
        let radio = crate::radio::vertical("forward-kind")
            .children([
                t!("forward.kind_local"),
                t!("forward.kind_remote"),
                t!("forward.kind_dynamic"),
            ])
            .selected_index(KINDS.iter().position(|k| *k == kind))
            .on_change(cx.listener(|this, ix: &usize, window, cx| {
                if let Some(kind) = KINDS.get(*ix) {
                    this.set_kind(*kind, window, cx);
                }
            }));
        // 选了任意地址（0.0.0.0、::）时提示风险，写明是哪一个
        let any = chosen(&self.bind, cx).filter(|b| is_any_address(b));
        let shared = self.shared_terminal(cx).is_some();
        let can_detect = kind == ForwardKind::Local && shared;
        let detected = self.detected.clone();
        let port_error = self.port_error(cx);
        let host_unavailable = self.host_unavailable(cx);
        let host_field = match &self.host {
            Some(select) => v_flex()
                .gap_1()
                .child(crate::choice::select(select).id("forward-host"))
                .when(host_unavailable, |this| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(warning)
                            .child(prose(t!("forward.host_unavailable_hint"))),
                    )
                })
                .into_any_element(),
            None => div()
                .text_sm()
                .text_color(warning)
                .child(prose(t!("forward.no_hosts")))
                .into_any_element(),
        };
        v_form()
            .child(field().child(div().text_sm().text_color(muted).child(prose(hint))))
            .child(field().child(radio))
            .child(field().label(t!("forward.host")).child(host_field))
            .child(
                field()
                    .label(t!("forward.name"))
                    .child(Input::new(&self.name).id("forward-name")),
            )
            .child(
                field()
                    .label(if kind == ForwardKind::Remote {
                        t!("forward.remote_bind")
                    } else {
                        t!("forward.local")
                    })
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(div().flex_1().child(
                                        crate::choice::select(&self.bind).id("forward-bind"),
                                    ))
                                    .child(div().w(rems(6.)).child(
                                        Input::new(&self.bind_port).id("forward-bind-port"),
                                    )),
                            )
                            .when_some(any, |this, address| {
                                this.child(div().text_xs().text_color(warning).child(prose(
                                    if kind == ForwardKind::Remote {
                                        t!("forward.remote_any_warning")
                                    } else {
                                        t!("forward.any_warning", address = address)
                                    },
                                )))
                            }),
                    ),
            )
            .when(kind != ForwardKind::Dynamic, |this| {
                this.child(
                    field()
                        .label(if kind == ForwardKind::Remote {
                            t!("forward.remote_target")
                        } else {
                            t!("forward.target")
                        })
                        .child(
                            v_flex()
                                .gap_1()
                                .when(can_detect, |this| {
                                    this.child(
                                        h_flex().justify_end().child(
                                            Button::new("forward-detect")
                                                .ghost()
                                                .xsmall()
                                                .icon(IconName::Radar)
                                                .label(t!("forward.detect_ports"))
                                                .loading(self.detecting)
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.detect(window, cx)
                                                })),
                                        ),
                                    )
                                })
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            div().flex_1().child(
                                                Input::new(&self.target).id("forward-target"),
                                            ),
                                        )
                                        .child(div().w(rems(6.)).child(
                                            Input::new(&self.target_port).id("forward-target-port"),
                                        )),
                                )
                                .when_some(detected.filter(|_| can_detect), |this, ports| {
                                    let list =
                                        if ports.is_empty() {
                                            div()
                                                .p_2()
                                                .text_xs()
                                                .text_color(muted)
                                                .child(t!("forward.no_ports"))
                                                .into_any_element()
                                        } else {
                                            // gpui-kit 的滚动区域：对话框放不下时滚轮也先滚这里（普通的
                                            // overflow_y_scroll 拿不到滚轮，只滚对话框本身）
                                            v_flex()
                                                .id("forward-ports")
                                                .max_h(rems(9.))
                                                .overflow_y_scrollbar()
                                                .children(ports.into_iter().map(|port| {
                                                    let label = termlet_core::model::join_host_port(
                                                        &port.address,
                                                        port.port,
                                                    );
                                                    let process = port.process.clone();
                                                    let id = SharedString::from(format!(
                                                        "forward-port-{}-{}",
                                                        port.address, port.port
                                                    ));
                                                    h_flex()
                                                        .id(id)
                                                        .px_2()
                                                        .py_0p5()
                                                        .gap_2()
                                                        .text_xs()
                                                        .cursor_pointer()
                                                        .hover(|s| s.bg(cx.theme().list_hover))
                                                        .child(
                                                            div()
                                                                .font_family(mono.clone())
                                                                .child(label),
                                                        )
                                                        .children(process.map(|p| {
                                                            div().text_color(muted).child(p)
                                                        }))
                                                        .on_click(cx.listener(
                                                            move |this, _, window, cx| {
                                                                this.pick(&port, window, cx)
                                                            },
                                                        ))
                                                }))
                                                .into_any_element()
                                        };
                                    this.child(
                                        div()
                                            .border_1()
                                            .border_color(cx.theme().border)
                                            .rounded(cx.theme().radius)
                                            .child(list),
                                    )
                                }),
                        ),
                )
            })
            .when(port_error, |this| {
                this.child(
                    field().child(
                        div()
                            .text_xs()
                            .text_color(danger)
                            .child(t!("forward.port_invalid")),
                    ),
                )
            })
            .child(
                field().child(
                    Checkbox::new("forward-auto-start")
                        .label(t!("forward.auto_start"))
                        .checked(self.auto_start)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.auto_start = *checked;
                            cx.notify();
                        })),
                ),
            )
            .when(self.id.is_none() && shared, |this| {
                this.child(
                    field().child(
                        Checkbox::new("forward-start-now")
                            .prose_label(t!("forward.start_shared"))
                            .checked(self.start_now)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.start_now = *checked;
                                cx.notify();
                            })),
                    ),
                )
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    field().child(
                        h_flex()
                            .gap_1()
                            .text_sm()
                            .text_color(danger)
                            .child(Icon::new(IconName::TriangleAlert).xsmall())
                            .child(prose(error)),
                    ),
                )
            })
    }
}
