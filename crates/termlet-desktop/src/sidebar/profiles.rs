//! 侧边栏的凭据档案页：列出档案（名称、用户名、认证方式、被多少主机与分组使用），新建、编辑、删除。

use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    Context, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
};
use termlet_core::model::{Auth, Group, Host, Kind, Profile, ProfileAuth};
use termlet_core::store::{Store, StoreEvent};

use crate::app_state::AppState;
use crate::prose::prose;
use crate::scrolling::ScrollGutter as _;
use crate::{profile_dialog, t};

pub struct ProfileList {
    store: Arc<Store>,
    _subscription: Subscription,
}

impl ProfileList {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let store_model = AppState::global(cx).store.clone();
        let store = store_model.read(cx).store().clone();
        let subscription = cx.subscribe(&store_model, |_, _, event: &StoreEvent, cx| {
            if matches!(
                event,
                StoreEvent::Changed(Kind::Profile | Kind::Host | Kind::Group)
            ) {
                cx.notify();
            }
        });
        Self {
            store,
            _subscription: subscription,
        }
    }
}

impl Render for ProfileList {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut profiles = self.store.list::<Profile>();
        profiles.sort_by_key(|p| p.doc.name.to_lowercase());
        let hosts = self.store.list::<Host>();
        let groups = self.store.list::<Group>();
        let theme = cx.theme();
        let rows = profiles.into_iter().map(|p| {
            let host_count = hosts
                .iter()
                .filter(|h| matches!(&h.doc.auth, Auth::Profile { profile } if *profile == p.id))
                .count();
            let group_count = groups
                .iter()
                .filter(|g| g.doc.defaults.profile.as_ref() == Some(&p.id))
                .count();
            let auth = match p.doc.auth {
                ProfileAuth::Password => t!("host.auth.password"),
                ProfileAuth::Key { .. } => t!("host.auth.key"),
                ProfileAuth::Agent => t!("host.auth.agent"),
            };
            let detail = t!(
                "profiles.row.detail",
                username = p.doc.username.clone(),
                auth = auth,
                hosts = host_count,
                groups = group_count
            );
            let (edit_id, menu_id) = (p.id.clone(), p.id.clone());
            h_flex()
                .id(SharedString::from(format!("profile-{}", p.id)))
                .px_2()
                .py_1()
                .gap_2()
                .rounded(theme.radius)
                .hover(|s| s.bg(theme.list_hover))
                .child(
                    Icon::new(IconName::UserRound)
                        .small()
                        .text_color(theme.muted_foreground),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().truncate().text_sm().child(p.doc.name.clone()))
                        .child(
                            div()
                                .truncate()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(prose(detail)),
                        ),
                )
                .on_click(move |_, window, cx| {
                    profile_dialog::open_edit(edit_id.clone(), window, cx)
                })
                .context_menu(move |menu, _, _| {
                    let (edit, delete) = (menu_id.clone(), menu_id.clone());
                    menu.item(PopupMenuItem::new(t!("profiles.menu.edit")).on_click(
                        move |_, window, cx| profile_dialog::open_edit(edit.clone(), window, cx),
                    ))
                    .separator()
                    .item(
                        PopupMenuItem::new(t!("profiles.menu.delete")).on_click(
                            move |_, window, cx| {
                                profile_dialog::confirm_delete(delete.clone(), window, cx)
                            },
                        ),
                    )
                })
        });
        let rows: Vec<_> = rows.collect();
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
                        Button::new("new-profile")
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .tooltip(t!("profiles.new"))
                            .on_click(|_, window, cx| profile_dialog::open_new(window, cx)),
                    ),
            )
            .child(if rows.is_empty() {
                div()
                    .p_4()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(prose(t!("profiles.empty")))
                    .into_any_element()
            } else {
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .pl_1()
                    .scrollbar_gutter()
                    .overflow_y_scrollbar()
                    .id("profile-list")
                    .children(rows)
                    .into_any_element()
            })
    }
}
