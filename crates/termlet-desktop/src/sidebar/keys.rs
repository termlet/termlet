//! 侧边栏“密钥”页的密钥库：列出密钥（名称、类型、被多少主机与凭据档案使用、是否附带证书），生成、导入；
//! 每行悬停时可以查看公钥、部署到服务器、导出私钥、重命名、删除，单击整行查看公钥。

use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, TestSupportExt as _,
    Window, div, prelude::FluentBuilder as _,
};
use termlet_core::model::{KeyEntry, Kind};
use termlet_core::store::{Record, Store, StoreEvent};

use crate::app_state::AppState;
use crate::prose::prose;
use crate::{key_dialogs, t};

pub struct KeyList {
    store: Arc<Store>,
    /// 按名称排好的密钥（密钥、主机、档案变化时重算，不在每帧统计、解析证书）
    keys: Vec<KeyRow>,
    _subscription: Subscription,
}

struct KeyRow {
    key: Record<KeyEntry>,
    /// 被多少主机、凭据档案使用
    hosts: usize,
    profiles: usize,
    /// 附带的证书：`Some(失效时间)`（Unix 秒，永不过期为 `None`）；没有证书或解析不了时为 `None`
    certificate: Option<Option<i64>>,
}

/// 类型的简写：去掉 `ssh-` 前缀（ed25519、rsa、ecdsa-sha2-nistp256）。
pub fn short_algorithm(algorithm: &str) -> &str {
    algorithm.strip_prefix("ssh-").unwrap_or(algorithm)
}

impl KeyList {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let store_model = AppState::global(cx).store.clone();
        let store = store_model.read(cx).store().clone();
        let subscription = cx.subscribe(&store_model, |this, _, event: &StoreEvent, cx| {
            if matches!(
                event,
                StoreEvent::Changed(Kind::Key | Kind::Host | Kind::Profile)
            ) {
                this.reload();
                cx.notify();
            }
        });
        let mut this = Self {
            store,
            keys: Vec::new(),
            _subscription: subscription,
        };
        this.reload();
        this
    }

    fn reload(&mut self) {
        let mut keys = self.store.list::<KeyEntry>();
        keys.sort_by_key(|k| k.doc.name.to_lowercase());
        self.keys = keys
            .into_iter()
            .map(|key| {
                // 被多少主机（含命令行工具的专用凭据）与凭据档案使用
                let used = termlet_core::keyvault::usage(&self.store, &key.id);
                let certificate = termlet_core::sshcert::parse(&key.doc.certificate)
                    .ok()
                    .map(|c| termlet_core::sshcert::CertInfo::of(&c).valid_before);
                KeyRow {
                    key,
                    hosts: used.hosts.len(),
                    profiles: used.profiles.len(),
                    certificate,
                }
            })
            .collect();
    }

    /// `menu_open`：窗口中开着右键菜单，行上的按钮不挂悬停提示（见 [`crate::menu_tooltip`]）。
    fn render_row(&self, row: &KeyRow, menu_open: bool, cx: &mut Context<Self>) -> AnyElement {
        let KeyRow {
            key,
            hosts: host_count,
            profiles: profile_count,
            certificate,
        } = row;
        let (host_count, profile_count, certificate) = (*host_count, *profile_count, *certificate);
        let theme = cx.theme();
        let mut detail = t!(
            "keys.row.detail",
            kind = short_algorithm(&key.doc.algorithm),
            hosts = host_count
        )
        .to_string();
        if profile_count > 0 {
            detail.push_str(&t!("keys.row.profiles", count = profile_count));
        }
        // 附带了证书：标出来，过期的说明已过期
        if let Some(expiry) = certificate {
            let expired = expiry.is_some_and(|at| at <= termlet_core::sshcert::now_secs());
            detail.push_str(&if expired {
                t!("keys.row.certificate_expired")
            } else {
                t!("keys.row.certificate")
            });
        }
        let id = key.id.clone();
        let group = SharedString::from(format!("key-row-{id}"));
        let action = |name: &str, icon: IconName, tip: SharedString| {
            Button::new(SharedString::from(format!("key-{name}-{id}")))
                .ghost()
                .small()
                .icon(icon)
                .tab_stop(false)
                .when(!menu_open, |this| this.tooltip(tip))
        };
        let (view, deploy, export, rename, delete, open) = (
            id.clone(),
            id.clone(),
            id.clone(),
            id.clone(),
            id.clone(),
            id.clone(),
        );
        let menu_id = id.clone();
        h_flex()
            .id(SharedString::from(format!("key-{id}")))
            .test_support()
            .group(group.clone())
            .relative()
            .px_2()
            .py_1()
            .gap_2()
            .rounded(theme.radius)
            .hover(|s| s.bg(theme.list_hover))
            .child(
                Icon::new(IconName::KeyRound)
                    .small()
                    .text_color(theme.warning),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().truncate().text_sm().child(key.doc.name.clone()))
                    .child(
                        div()
                            .truncate()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(prose(detail)),
                    ),
            )
            .child(
                // 悬停时浮在行的右侧（不占文字的宽度）
                h_flex()
                    .absolute()
                    .right_1()
                    .top_0()
                    .bottom_0()
                    .px_0p5()
                    .bg(theme.list_hover)
                    .invisible()
                    .group_hover(group, |s| s.visible())
                    .child(action("view", IconName::Eye, t!("keys.view")).on_click(
                        move |_, window, cx| {
                            cx.stop_propagation();
                            key_dialogs::view::open(view.clone(), window, cx)
                        },
                    ))
                    .child(
                        action("deploy", IconName::Send, t!("keys.deploy")).on_click(
                            move |_, window, cx| {
                                cx.stop_propagation();
                                key_dialogs::deploy::open(deploy.clone(), window, cx)
                            },
                        ),
                    )
                    .child(
                        action("export", IconName::FileDown, t!("keys.export")).on_click(
                            move |_, window, cx| {
                                cx.stop_propagation();
                                key_dialogs::export::open(export.clone(), window, cx)
                            },
                        ),
                    )
                    .child(
                        action("rename", IconName::Pencil, t!("keys.rename")).on_click(
                            move |_, window, cx| {
                                cx.stop_propagation();
                                key_dialogs::rename(rename.clone(), window, cx)
                            },
                        ),
                    )
                    .child(
                        action("delete", IconName::Trash, t!("common.delete")).on_click(
                            move |_, window, cx| {
                                cx.stop_propagation();
                                key_dialogs::confirm_delete(delete.clone(), window, cx)
                            },
                        ),
                    ),
            )
            .on_click(move |_, window, cx| key_dialogs::view::open(open.clone(), window, cx))
            .context_menu(move |menu, _, _| {
                let ids = [
                    menu_id.clone(),
                    menu_id.clone(),
                    menu_id.clone(),
                    menu_id.clone(),
                    menu_id.clone(),
                    menu_id.clone(),
                ];
                let [view, deploy, export_public, export, rename, delete] = ids;
                menu.item(
                    PopupMenuItem::new(t!("keys.view")).on_click(move |_, window, cx| {
                        key_dialogs::view::open(view.clone(), window, cx)
                    }),
                )
                .item(
                    PopupMenuItem::new(t!("keys.deploy")).on_click(move |_, window, cx| {
                        key_dialogs::deploy::open(deploy.clone(), window, cx)
                    }),
                )
                .item(PopupMenuItem::new(t!("keys.export_public")).on_click(
                    move |_, window, cx| {
                        key_dialogs::export::export_public(export_public.clone(), window, cx)
                    },
                ))
                .item(
                    PopupMenuItem::new(t!("keys.export")).on_click(move |_, window, cx| {
                        key_dialogs::export::open(export.clone(), window, cx)
                    }),
                )
                .item(
                    PopupMenuItem::new(t!("keys.rename")).on_click(move |_, window, cx| {
                        key_dialogs::rename(rename.clone(), window, cx)
                    }),
                )
                .separator()
                .item(
                    PopupMenuItem::new(t!("common.delete")).on_click(move |_, window, cx| {
                        key_dialogs::confirm_delete(delete.clone(), window, cx)
                    }),
                )
            })
            .into_any_element()
    }
}

impl Render for KeyList {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let menu_open = crate::menu_tooltip::menu_open(window, cx);
        let mut rows = Vec::with_capacity(self.keys.len());
        for row in &self.keys {
            rows.push(self.render_row(row, menu_open, cx));
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
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(muted)
                            .child(prose(t!("keys.hint"))),
                    )
                    .child(
                        Button::new("key-generate")
                            .ghost()
                            .small()
                            .icon(IconName::Sparkles)
                            .tooltip(t!("keys.generate"))
                            .on_click(|_, window, cx| key_dialogs::generate::open(window, cx)),
                    )
                    .child(
                        Button::new("key-import")
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .tooltip(t!("keys.import"))
                            .on_click(|_, window, cx| key_dialogs::import::open(window, cx)),
                    ),
            )
            .when(rows.is_empty(), |this| {
                this.child(
                    div()
                        .p_4()
                        .text_sm()
                        .text_color(muted)
                        .child(prose(t!("keys.empty"))),
                )
            })
            .when(!rows.is_empty(), |this| {
                this.child(
                    v_flex()
                        .id("key-list")
                        .flex_1()
                        .min_h_0()
                        .px_1()
                        .overflow_y_scroll()
                        .children(rows),
                )
            })
    }
}
