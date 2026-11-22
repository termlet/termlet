//! 侧边栏的“转发”页：端口转发规则的列表（状态、名称、路线与所属主机、连接数与流量、出错原因），
//! 新建、编辑、删除、启动 / 停止。监听所有网卡的规则带警示标记（本机的所有网卡与服务器的所有网卡各有说明）。

use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, TestSupportExt as _,
    Window, div, prelude::FluentBuilder as _,
};
use termlet_core::id::ForwardId;
use termlet_core::model::{Forward, ForwardKind, Kind, is_any_address};
use termlet_core::store::{Record, Store, StoreEvent};

use crate::app_state::AppState;
use crate::forwards::{self, RowState};
use crate::prose::prose;
use crate::sftp::list_model::format_size;
use crate::{forward_dialog, store_errors, t};

/// 监听所有网卡的规则：谁能经由它访问目标（侧边栏的警示标记）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Exposure {
    /// 本地、动态转发监听本机的所有网卡：局域网内的其他设备
    Lan,
    /// 远程转发监听服务器的所有网卡：能访问服务器的设备
    Server,
}

fn exposure(rule: &Forward) -> Option<Exposure> {
    if rule.listens_on_all_interfaces() {
        Some(Exposure::Lan)
    } else if rule.kind == ForwardKind::Remote && is_any_address(&rule.bind_host) {
        Some(Exposure::Server)
    } else {
        None
    }
}

pub struct ForwardList {
    store: Arc<Store>,
    _subscriptions: Vec<Subscription>,
}

impl ForwardList {
    pub fn new(_: &mut Window, cx: &mut Context<Self>) -> Self {
        let store_model = AppState::global(cx).store.clone();
        let store = store_model.read(cx).store().clone();
        let subscriptions = vec![
            cx.subscribe(&store_model, |_, _, event: &StoreEvent, cx| {
                if matches!(event, StoreEvent::Changed(Kind::Forward | Kind::Host)) {
                    cx.notify();
                }
            }),
            // 状态、连接数与流量变了
            forwards::observe(cx, |_, _, cx| cx.notify()),
        ];
        Self {
            store,
            _subscriptions: subscriptions,
        }
    }

    fn confirm_delete(&self, rule: &Record<Forward>, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let id = rule.id.clone();
        let name = forwards::display_name(&rule.doc);
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let (store, id) = (store.clone(), id.clone());
            alert
                .confirm()
                .title(t!("forward.confirm_delete", name = name.clone()))
                .ok_text(t!("common.delete"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, window, cx| {
                    // 运行中的先停止
                    forwards::stop(&id, window, cx);
                    tracing::info!(id = %id, "删除端口转发规则");
                    match store.delete::<Forward>(&id) {
                        // 确认期间已被同步或命令行删掉：目的已经达到
                        Ok(_) | Err(termlet_core::CoreError::NotFound(_)) => {}
                        Err(e) => crate::notify::error(store_errors::describe(&e), window, cx),
                    }
                    true
                })
        });
    }

    fn render_row(&self, rule: Record<Forward>, cx: &mut Context<Self>) -> AnyElement {
        let status = forwards::status(&rule.id, cx);
        let theme = cx.theme();
        let (muted, warning, danger, success) = (
            theme.muted_foreground,
            theme.warning,
            theme.danger,
            theme.success,
        );
        let dot = match &status.state {
            RowState::Running => success,
            RowState::Starting | RowState::Reconnecting(_) => warning,
            RowState::Error(_) => danger,
            RowState::Stopped => muted.opacity(0.5),
        };
        let state_text = match &status.state {
            RowState::Stopped => t!("forward.state_stopped"),
            RowState::Starting => t!("forward.state_starting"),
            RowState::Running => t!("forward.state_running"),
            RowState::Reconnecting(_) => t!("forward.state_reconnecting"),
            RowState::Error(_) => t!("forward.state_error"),
        };
        let id: ForwardId = rule.id.clone();
        let group = SharedString::from(format!("forward-row-{id}"));
        let name = forwards::display_name(&rule.doc);
        let host = forwards::host_name(&self.store, rule.doc.host.as_ref());
        let detail = if host.is_empty() {
            forwards::route(&rule.doc).to_string()
        } else {
            format!("{} · {host}", forwards::route(&rule.doc))
        };
        let exposed = exposure(&rule.doc);
        let bind_host = rule.doc.bind_host.clone();
        let active = status.state.is_active();
        let (toggle_id, edit_id) = (id.clone(), id.clone());
        let delete_rule = rule.clone();
        let this = cx.entity().downgrade();
        v_flex()
            .id(SharedString::from(format!("forward-{id}")))
            .test_support()
            .group(group.clone())
            .relative()
            .px_2()
            .py_1()
            .gap_0p5()
            .rounded(theme.radius)
            .hover(|s| s.bg(theme.list_hover))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .id(SharedString::from(format!("forward-state-{id}")))
                            .flex_none()
                            .size_2()
                            .rounded_full()
                            .bg(dot)
                            .tooltip(move |window, cx| {
                                Tooltip::new(state_text.clone()).build(window, cx)
                            }),
                    )
                    .child(div().flex_1().min_w_0().truncate().text_sm().child(name))
                    .when_some(exposed, |this, exposed| {
                        this.child(
                            div()
                                .id(SharedString::from(format!("forward-exposed-{id}")))
                                .test_support()
                                .flex_none()
                                .text_color(warning)
                                .child(Icon::new(IconName::TriangleAlert).xsmall())
                                .tooltip(move |window, cx| {
                                    let text = match exposed {
                                        Exposure::Lan => {
                                            t!("forward.any_badge", address = bind_host.as_str())
                                        }
                                        Exposure::Server => t!("forward.remote_any_badge"),
                                    };
                                    Tooltip::new(text).build(window, cx)
                                }),
                        )
                    })
                    .child(
                        // 悬停时浮在启停按钮的左侧（不占文字的宽度）
                        h_flex()
                            .absolute()
                            .right(gpui_kit::rems(1.75))
                            .top_0p5()
                            .bg(theme.list_hover)
                            .invisible()
                            .group_hover(group, |s| s.visible())
                            .child(
                                Button::new(SharedString::from(format!("forward-edit-{id}")))
                                    .ghost()
                                    .small()
                                    .icon(IconName::Pencil)
                                    .tab_stop(false)
                                    .tooltip(t!("forward.edit"))
                                    .on_click(move |_, window, cx| {
                                        forward_dialog::open_edit(edit_id.clone(), window, cx)
                                    }),
                            )
                            .child(
                                Button::new(SharedString::from(format!("forward-delete-{id}")))
                                    .ghost()
                                    .small()
                                    .icon(IconName::Trash)
                                    .tab_stop(false)
                                    .tooltip(t!("common.delete"))
                                    .on_click(move |_, window, cx| {
                                        let _ = this.update(cx, |this, cx| {
                                            this.confirm_delete(&delete_rule, window, cx)
                                        });
                                    }),
                            ),
                    )
                    .child(
                        Button::new(SharedString::from(format!("forward-toggle-{id}")))
                            .ghost()
                            .small()
                            .icon(if active {
                                IconName::Square
                            } else {
                                IconName::Play
                            })
                            .when(active, |b| b.text_color(success))
                            .tooltip(if active {
                                t!("forward.stop")
                            } else {
                                t!("forward.start")
                            })
                            .on_click(move |_, window, cx| {
                                if active {
                                    forwards::stop(&toggle_id, window, cx)
                                } else {
                                    forwards::start(&toggle_id, window, cx)
                                }
                            }),
                    ),
            )
            .child(
                div()
                    .pl_4()
                    .truncate()
                    .text_xs()
                    .text_color(muted)
                    .child(prose(detail)),
            )
            .when(active, |this| {
                this.child(div().pl_4().text_xs().text_color(muted).child(prose(t!(
                    "forward.stats",
                    conns = status.connections,
                    bytes_in = format_size(status.bytes_in),
                    bytes_out = format_size(status.bytes_out)
                ))))
            })
            .when_some(
                match &status.state {
                    RowState::Reconnecting(attempt) => Some(*attempt),
                    _ => None,
                },
                |this, attempt| {
                    this.child(
                        div()
                            .pl_4()
                            .text_xs()
                            .text_color(warning)
                            .child(prose(t!("forward.reconnecting", attempt = attempt))),
                    )
                },
            )
            .when_some(
                status
                    .last_error
                    .clone()
                    .filter(|_| status.state == RowState::Running),
                |this, message| {
                    this.child(
                        div()
                            .pl_4()
                            .truncate()
                            .text_xs()
                            .text_color(warning)
                            .child(prose(t!("forward.target_failed", message = message))),
                    )
                },
            )
            .when_some(
                match &status.state {
                    RowState::Error(message) => Some(message.clone()),
                    _ => None,
                },
                |this, message| {
                    this.child(
                        div()
                            .pl_4()
                            .text_xs()
                            .text_color(danger)
                            .child(prose(message)),
                    )
                },
            )
            .into_any_element()
    }
}

impl Render for ForwardList {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut rules = self.store.list::<Forward>();
        rules.sort_by_key(|r| forwards::display_name(&r.doc).to_lowercase());
        let mut rows = Vec::with_capacity(rules.len());
        for rule in rules {
            rows.push(self.render_row(rule, cx));
        }
        let muted = cx.theme().muted_foreground;
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .px_2()
                    .pt_2()
                    .pb_1()
                    .gap_1()
                    // 页头只放按钮（说明在空状态里）
                    .child(div().flex_1())
                    .child(
                        Button::new("forward-new")
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .tooltip(t!("forward.new"))
                            .on_click(|_, window, cx| {
                                forward_dialog::open_new(None, None, None, window, cx)
                            }),
                    ),
            )
            .when(rows.is_empty(), |this| {
                this.child(
                    div()
                        .p_4()
                        .text_sm()
                        .text_color(muted)
                        .child(prose(t!("forward.empty"))),
                )
            })
            .when(!rows.is_empty(), |this| {
                this.child(
                    v_flex()
                        .id("forward-list")
                        .flex_1()
                        .min_h_0()
                        .px_1()
                        .overflow_y_scroll()
                        .children(rows),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use termlet_core::model::{ANY_IPV4, ANY_IPV6};

    use super::*;

    fn rule(kind: ForwardKind, bind_host: &str) -> Forward {
        Forward {
            kind,
            bind_host: bind_host.into(),
            ..Default::default()
        }
    }

    // 本地、动态转发监听本机的所有网卡是局域网的风险；远程转发监听的是服务器的网卡，说明不同
    #[test]
    fn exposure_depends_on_where_the_rule_listens() {
        assert_eq!(
            exposure(&rule(ForwardKind::Local, ANY_IPV4)),
            Some(Exposure::Lan)
        );
        assert_eq!(
            exposure(&rule(ForwardKind::Dynamic, ANY_IPV4)),
            Some(Exposure::Lan)
        );
        assert_eq!(
            exposure(&rule(ForwardKind::Remote, ANY_IPV4)),
            Some(Exposure::Server)
        );
        // IPv6 的任意地址同样
        assert_eq!(
            exposure(&rule(ForwardKind::Local, ANY_IPV6)),
            Some(Exposure::Lan)
        );
        assert_eq!(
            exposure(&rule(ForwardKind::Remote, ANY_IPV6)),
            Some(Exposure::Server)
        );
        for kind in [
            ForwardKind::Local,
            ForwardKind::Dynamic,
            ForwardKind::Remote,
        ] {
            assert_eq!(exposure(&rule(kind, "127.0.0.1")), None, "{kind:?}");
            assert_eq!(exposure(&rule(kind, "::1")), None, "{kind:?}");
        }
    }
}
