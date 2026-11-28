//! 批量执行的“选择主机”：主机树（分组全部展开）逐台或按分组（含子分组）勾选，可以搜索、按标签添加。Telnet、串口
//! 主机不能批量执行，灰掉并写明；勾选分组、标签、“全部”时只选其中的 SSH 主机。

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, StyledExt as _,
    VirtualListScrollHandle, WindowExt as _, h_flex, v_flex, v_virtual_list,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, ElementId, Entity, Focusable as _,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Subscription, Window, div, prelude::FluentBuilder as _, px, rems, size,
};
use termlet_core::id::{GroupId, HostId};
use termlet_core::model::{Group, Host};
use termlet_core::store::Store;

use crate::app_state::AppState;
use crate::prose::prose;
use crate::sidebar::rows::{TreeRow, group_members, tree_rows};
use crate::t;
use crate::ui_scale::DialogTitleExt as _;

/// 主机树中一行的高度（rem）。
const ROW_HEIGHT: f32 = 1.75;

/// 选好后交给调用方的主机（按树中的顺序）。
pub(super) type OnDone = Rc<dyn Fn(Vec<HostId>, &mut Window, &mut App)>;

/// 打开“选择主机”，勾选着 `selected`。
pub(super) fn open(selected: Vec<HostId>, on_done: OnDone, window: &mut Window, cx: &mut App) {
    let picker = cx.new(|cx| HostPicker::new(selected, on_done, window, cx));
    let rem = window.rem_size();
    let width = rems(32.)
        .to_pixels(rem)
        .min(window.viewport_size().width - rems(4.).to_pixels(rem));
    let first = Rc::new(std::cell::Cell::new(true));
    window.open_dialog(cx, move |dialog, window, cx| {
        if first.replace(false) {
            let query = picker.read(cx).query.clone();
            query.focus_handle(cx).focus(window, cx);
        }
        let ok = {
            let picker = picker.clone();
            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                picker.update(cx, |picker, cx| picker.submit(window, cx));
                false
            }
        };
        dialog
            .titled(t!("batch.pick_title"))
            .w(width)
            .child(picker.clone())
            .on_ok(ok)
    });
}

pub(super) struct HostPicker {
    store: Arc<Store>,
    query: Entity<InputState>,
    rows: Vec<TreeRow>,
    /// 每个分组（含子分组）中的 SSH 主机
    members: HashMap<GroupId, Vec<HostId>>,
    /// 全部 SSH 主机（按不搜索时树中的顺序）
    order: Vec<HostId>,
    selected: HashSet<HostId>,
    /// 用到的标签（按名称）
    tags: Vec<String>,
    on_done: OnDone,
    scroll: VirtualListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl HostPicker {
    fn new(
        selected: Vec<HostId>,
        on_done: OnDone,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder(t!("batch.pick_search")));
        let store_model = AppState::global(cx).store.clone();
        let subscriptions = vec![
            cx.subscribe(&query, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.load_rows(cx);
                    cx.notify();
                }
            }),
            // 打开期间主机、分组有变化（别的窗口、云同步）：重新列出
            cx.observe(&store_model, |this, _, cx| {
                this.load_rows(cx);
                cx.notify();
            }),
        ];
        let mut this = Self {
            store: AppState::store(cx),
            query,
            rows: Vec::new(),
            members: HashMap::new(),
            order: Vec::new(),
            selected: selected.into_iter().collect(),
            tags: Vec::new(),
            on_done,
            scroll: VirtualListScrollHandle::new(),
            _subscriptions: subscriptions,
        };
        this.load_rows(cx);
        this
    }

    fn load_rows(&mut self, cx: &App) {
        let groups = self.store.list::<Group>();
        let hosts = self.store.list::<Host>();
        let query = self.query.read(cx).value().trim().to_string();
        self.order = tree_rows(&groups, &hosts, "")
            .into_iter()
            .filter_map(|row| match row {
                TreeRow::Host { id, ssh: true, .. } => Some(id),
                _ => None,
            })
            .collect();
        self.rows = tree_rows(&groups, &hosts, &query);
        let ssh_hosts: HashSet<&HostId> = self.order.iter().collect();
        self.members = group_members(&self.rows)
            .into_iter()
            .map(|(group, ids)| {
                let ids = ids
                    .into_iter()
                    .filter(|id| ssh_hosts.contains(id))
                    .collect();
                (group, ids)
            })
            .collect();
        let existing: HashSet<HostId> = self.order.iter().cloned().collect();
        self.selected.retain(|id| existing.contains(id));
        let mut tags: Vec<String> = hosts
            .iter()
            .filter(|h| h.doc.is_ssh())
            .flat_map(|h| h.doc.tags.iter().cloned())
            .collect();
        tags.sort_by_key(|t| t.to_lowercase());
        tags.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        self.tags = tags;
    }

    fn set_many(&mut self, ids: &[HostId], on: bool, cx: &mut Context<Self>) {
        for id in ids {
            if on {
                self.selected.insert(id.clone());
            } else {
                self.selected.remove(id);
            }
        }
        cx.notify();
    }

    /// 添加带这个标签的 SSH 主机。
    fn add_tag(&mut self, tag: &str, cx: &mut Context<Self>) {
        let tagged: Vec<HostId> = self
            .store
            .list::<Host>()
            .into_iter()
            .filter(|h| h.doc.is_ssh() && h.doc.tags.iter().any(|t| t.eq_ignore_ascii_case(tag)))
            .map(|h| h.id)
            .collect();
        self.set_many(&tagged, true, cx);
    }

    /// 选中的主机，按树中的顺序。
    fn chosen(&self) -> Vec<HostId> {
        self.order
            .iter()
            .filter(|id| self.selected.contains(*id))
            .cloned()
            .collect()
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = self.chosen();
        if chosen.is_empty() {
            return;
        }
        window.close_dialog(cx);
        (self.on_done)(chosen, window, cx);
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, hover) = (theme.muted_foreground, theme.list_hover);
        let indent = |depth: usize| rems(0.5 + depth as f32);
        match &self.rows[ix] {
            TreeRow::Group { id, name, depth } => {
                let ids = self.members.get(id).cloned().unwrap_or_default();
                let picked = ids.iter().filter(|h| self.selected.contains(*h)).count();
                let total = ids.len();
                Checkbox::new(element_id("batch-pick-group", id.as_str()))
                    .size_full()
                    .pl(indent(*depth))
                    .pr_2()
                    .py_1()
                    .checked(total > 0 && picked == total)
                    .disabled(total == 0)
                    .accessibility_label(name.clone())
                    .hover(move |style| style.bg(hover))
                    .on_change(cx.listener(move |this, checked: &bool, _, cx| {
                        this.set_many(&ids, *checked, cx)
                    }))
                    .child(
                        h_flex()
                            .gap_1p5()
                            .text_sm()
                            .child(
                                Icon::new(IconName::Folder)
                                    .xsmall()
                                    .text_color(muted)
                                    .flex_none(),
                            )
                            .child(div().min_w_0().truncate().font_medium().child(name.clone()))
                            .child(
                                div()
                                    .flex_none()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(total.to_string()),
                            ),
                    )
                    .into_any_element()
            }
            TreeRow::Host {
                id,
                name,
                address,
                ssh,
                depth,
            } => {
                let host = id.clone();
                Checkbox::new(element_id("batch-pick-host", id.as_str()))
                    .size_full()
                    .pl(indent(*depth))
                    .pr_2()
                    .py_1()
                    .checked(*ssh && self.selected.contains(id))
                    .disabled(!ssh)
                    .accessibility_label(name.clone())
                    .hover(move |style| style.bg(hover))
                    .on_change(cx.listener(move |this, checked: &bool, _, cx| {
                        this.set_many(std::slice::from_ref(&host), *checked, cx)
                    }))
                    .child(
                        h_flex()
                            .gap_2()
                            .text_sm()
                            .child(div().min_w_0().truncate().child(name.clone()))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(if *ssh {
                                        SharedString::from(address.clone())
                                    } else {
                                        t!("batch.pick_not_ssh")
                                    }),
                            ),
                    )
                    .into_any_element()
            }
        }
    }
}

fn element_id(prefix: &str, key: &str) -> ElementId {
    ElementId::from(SharedString::from(format!("{prefix}-{key}")))
}

impl Render for HostPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (border, radius, muted) = (theme.border, theme.radius, theme.muted_foreground);
        let total = self.order.len();
        let all_on = total > 0 && self.order.iter().all(|id| self.selected.contains(id));
        let count = self.chosen().len();
        let list = if self.rows.is_empty() {
            div()
                .p_3()
                .text_xs()
                .text_color(muted)
                .child(prose(t!("batch.pick_empty")))
                .into_any_element()
        } else {
            let row_height = rems(ROW_HEIGHT).to_pixels(window.rem_size());
            let sizes = Rc::new(vec![size(px(0.), row_height); self.rows.len()]);
            v_virtual_list(
                cx.entity(),
                "batch-pick-rows",
                sizes,
                |this, range, _, cx| range.map(|ix| this.render_row(ix, cx)).collect::<Vec<_>>(),
            )
            .track_scroll(&self.scroll)
            .size_full()
            .into_any_element()
        };
        // 列表的高度按窗口收：对话框放不下时滚轮只滚对话框本身，这里就滚不到了
        let rem = window.rem_size();
        let height = rems(20.)
            .to_pixels(rem)
            .min(window.viewport_size().height - rems(18.).to_pixels(rem))
            .max(rems(6.).to_pixels(rem));
        let all = self.order.clone();
        v_flex()
            .id("batch-host-picker")
            .gap_3()
            .child(Input::new(&self.query).id("batch-pick-search").small())
            .child(
                v_flex()
                    .h(height)
                    .border_1()
                    .border_color(border)
                    .rounded(radius)
                    .overflow_hidden()
                    .child(
                        div().px_2().py_1().border_b_1().border_color(border).child(
                            Checkbox::new("batch-pick-all")
                                .label(t!("batch.pick_all", count = total))
                                .checked(all_on)
                                .disabled(total == 0)
                                .on_change(cx.listener(move |this, checked: &bool, _, cx| {
                                    this.set_many(&all, *checked, cx)
                                })),
                        ),
                    )
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .child(list)
                            .vertical_scrollbar(&self.scroll),
                    ),
            )
            .when(!self.tags.is_empty(), |this| {
                this.child(
                    h_flex()
                        .flex_wrap()
                        .gap_1()
                        .items_center()
                        .text_xs()
                        .text_color(muted)
                        .child(t!("batch.pick_by_tag"))
                        .children(self.tags.iter().map(|tag| {
                            let name = tag.clone();
                            Button::new(element_id("batch-pick-tag", tag))
                                .ghost()
                                .xsmall()
                                .label(format!("#{tag}"))
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.add_tag(&name, cx)),
                                )
                        })),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(muted)
                            .child(prose(t!("batch.pick_count", count = count))),
                    )
                    .child(
                        Button::new("batch-pick-cancel")
                            .label(t!("common.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("batch-pick-ok")
                            .primary()
                            .label(t!("common.ok"))
                            .disabled(count == 0)
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}
