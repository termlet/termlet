//! 设置中的“主机指纹”：首次连接时确认并记下的服务器指纹，可以搜索、删除（立即生效，与“保存”无关）。
//!
//! 每条记录列出地址、算法、指纹、记录的时间，以及哪些主机用这个地址与端口。
//! 上面是信任的主机 CA（见 [`super::host_cas`]）。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, Context, ElementId, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Window, div,
};
use termlet_core::model::Host;
use termlet_core::ssh::{KnownHostEntry, KnownHosts};

use crate::app_state::AppState;
use crate::prose::prose;
use crate::{store_errors, t};

pub(super) struct KnownHostsPage {
    /// 记录与用这个地址、端口的主机名（读取时算好，不在每次渲染时遍历全部主机）
    entries: Vec<(KnownHostEntry, Vec<String>)>,
    search: Entity<InputState>,
    host_cas: Entity<super::host_cas::HostCasSection>,
    _subscription: Subscription,
}

impl KnownHostsPage {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(t!("known_hosts.search")));
        let subscription = cx.subscribe(&search, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let mut this = Self {
            entries: Vec::new(),
            search,
            host_cas: cx.new(super::host_cas::HostCasSection::new),
            _subscription: subscription,
        };
        this.reload(cx);
        this
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let store = AppState::store(cx);
        let hosts = store.list::<Host>();
        // 用这个地址与端口的主机
        let users = |entry: &KnownHostEntry| -> Vec<String> {
            hosts
                .iter()
                .filter(|h| {
                    h.doc.address.eq_ignore_ascii_case(&entry.host)
                        && h.doc.effective_port() == entry.port
                })
                .map(|h| h.doc.name.clone())
                .collect()
        };
        self.entries = KnownHosts::new(store.database())
            .list()
            .unwrap_or_else(|e| {
                tracing::error!("读取主机指纹失败：{e}");
                Vec::new()
            })
            .into_iter()
            .map(|entry| {
                let names = users(&entry);
                (entry, names)
            })
            .collect();
        cx.notify();
    }

    fn confirm_delete(
        &mut self,
        entry: KnownHostEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity().downgrade();
        let host = format!("{}:{}", entry.host, entry.port);
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let this = this.clone();
            let entry = entry.clone();
            alert
                .confirm()
                .title(t!("known_hosts.delete_title", host = host.clone()))
                .description(prose(t!("known_hosts.delete_description")))
                .ok_text(t!("common.delete"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, window, cx| {
                    let store = AppState::store(cx);
                    tracing::info!(
                        host = %entry.host,
                        port = entry.port,
                        algorithm = %entry.algorithm,
                        "删除一条主机指纹"
                    );
                    if let Err(e) = KnownHosts::new(store.database()).forget_key(
                        &entry.host,
                        entry.port,
                        &entry.algorithm,
                    ) {
                        crate::notify::error(store_errors::describe(&e), window, cx);
                    }
                    let _ = this.update(cx, |this, cx| this.reload(cx));
                    true
                })
        });
    }
}

impl Render for KnownHostsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let query = self.search.read(cx).value().trim().to_lowercase();
        let rows: Vec<(KnownHostEntry, Vec<String>)> = self
            .entries
            .iter()
            .filter(|(e, names)| {
                query.is_empty()
                    || e.host.to_lowercase().contains(&query)
                    || e.fingerprint.to_lowercase().contains(&query)
                    || names.iter().any(|n| n.to_lowercase().contains(&query))
            })
            .cloned()
            .collect();
        let empty = self.entries.is_empty();
        v_flex()
            .gap_3()
            // 搜索框中的 Enter 不交给设置对话框（会保存并关闭设置）
            .on_action(|_: &gpui_kit::base::actions::Confirm, _, _| {})
            .child(
                div()
                    .pb_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(self.host_cas.clone()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(prose(t!("known_hosts.description"))),
            )
            .child(Input::new(&self.search).id("known-hosts-search").small())
            .children(empty.then(|| {
                div()
                    .py_4()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(prose(t!("known_hosts.empty")))
            }))
            .children(rows.into_iter().map(|(entry, names)| {
                let key = format!("{}-{}-{}", entry.host, entry.port, entry.algorithm);
                let id = ElementId::from(SharedString::from(format!("known-host-{key}")));
                let delete = entry.clone();
                let used_by = if names.is_empty() {
                    t!("known_hosts.unused")
                } else {
                    t!("known_hosts.used_by", names = names.join("、"))
                };
                h_flex()
                    .id(id)
                    .gap_2()
                    .px_3()
                    .py_2()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .text_sm()
                                    .child(format!("{}:{}", entry.host, entry.port))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(entry.algorithm.clone()),
                                    ),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_xs()
                                    .font_family(termlet_terminal::fonts::DEFAULT_FAMILY)
                                    .child(entry.fingerprint.clone()),
                            )
                            .child(div().text_xs().text_color(theme.muted_foreground).child(
                                format!(
                                    "{} · {used_by}",
                                    termlet_core::time::format_local(entry.added_at)
                                ),
                            )),
                    )
                    .child(
                        Button::new(SharedString::from(format!("known-host-delete-{key}")))
                            .ghost()
                            .small()
                            .icon(IconName::Trash)
                            .tooltip(t!("known_hosts.delete"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.confirm_delete(delete.clone(), window, cx)
                            })),
                    )
            }))
    }
}
