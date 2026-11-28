//! 侧边栏的“快捷命令”页：按分组折叠显示，搜索、新建、编辑、删除；单击发送到当前终端。“更多”中导入、导出。
//!
//! - 只有未分组的快捷命令时不显示分组标题。分组的折叠状态记在本机的界面状态中（键为 `snippet-group:<分组名>`）。
//! - 顺序见 [`termlet_core::snippet_order`]：未分组在前，分组按拖动排好的顺序（没排过的按名称），组内按拖动排好的顺序。
//! - 拖动（[`super::drag`]）：快捷命令在组内、跨组拖动，跨组即改分组；分组标题拖动调整分组的先后，拖动时各组画成
//!   折叠的、只剩标题。全都有分组时，拖动快捷命令期间最下面多出一行“放到这里移到未分组”。搜索时不能拖。
//! - 有名称的分组标题的右键菜单与悬停时的 ⋯：在这个分组中新建、重命名分组、删除分组（[`super::groups`]）。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Icon, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext as _, ClickEvent, Context, DragMoveEvent, ElementId, Entity,
    EventEmitter, InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, TestSupportExt as _, Window, div,
    prelude::FluentBuilder, px,
};
use termlet_core::id::SnippetId;
use termlet_core::keymap::{self, KeyAction};
use termlet_core::model::{Kind, Snippet};
use termlet_core::snippet_order;
use termlet_core::store::{Record, StoreEvent};

use super::drag::{self, Dragged, DropPlace, DropRow, DropTarget, Move};
use super::groups::{self, collapsed_key};
use super::{confirm_delete, editor};
use crate::app_state::AppState;
use crate::i18n::LocalizedPlaceholder as _;
use crate::prose::prose;
use crate::scrolling::ScrollGutter as _;
use crate::sidebar::host_tree::DragPreview;
use crate::truncated::{Clip, truncated};
use crate::{store_errors, t};

/// 快捷命令页发给工作区的请求。
pub enum SnippetListEvent {
    /// 发送到当前终端；`run` 为 `None` 时按快捷命令保存时的设置执行或只写入
    Send { id: SnippetId, run: Option<bool> },
}

pub struct SnippetList {
    search: Entity<InputState>,
    snippets: Vec<Record<Snippet>>,
    /// 保存的分组顺序（设置中的）
    group_order: Vec<String>,
    /// 正在拖动的东西（拖动开始到放下或取消）
    dragging: Option<Dragged>,
    drop_target: Option<DropTarget>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SnippetListEvent> for SnippetList {}

impl SnippetList {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .localized_placeholder(|| t!("snippets.search"), window, cx)
                .clean_on_escape()
        });
        let store = AppState::global(cx).store.clone();
        let ui = AppState::ui(cx);
        let subscriptions = vec![
            cx.subscribe(&search, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe(&store, |this, _, event: &StoreEvent, cx| {
                if matches!(
                    event,
                    StoreEvent::Changed(Kind::Snippet) | StoreEvent::SettingsChanged
                ) {
                    this.reload(cx);
                }
            }),
            cx.observe(&ui, |_, _, cx| cx.notify()),
        ];
        let mut this = Self {
            search,
            snippets: Vec::new(),
            group_order: Vec::new(),
            dragging: None,
            drop_target: None,
            _subscriptions: subscriptions,
        };
        this.reload(cx);
        this
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let store = AppState::store(cx);
        self.snippets = store.list::<Snippet>();
        self.group_order = store.settings().snippets.group_order;
        cx.notify();
    }

    fn is_collapsed(&self, group: &str, cx: &gpui_kit::App) -> bool {
        AppState::ui(cx)
            .read(cx)
            .state()
            .collapsed_groups
            .contains(&collapsed_key(group))
    }

    fn toggle(&mut self, group: &str, cx: &mut Context<Self>) {
        let key = collapsed_key(group);
        AppState::ui(cx).update(cx, |ui, cx| {
            ui.update(
                |state| {
                    if !state.collapsed_groups.remove(&key) {
                        state.collapsed_groups.insert(key);
                    }
                },
                cx,
            )
        });
    }

    /// 按分组归类（顺序见模块说明），只留匹配搜索的。
    fn grouped(&self, query: &str) -> Vec<(String, Vec<&Record<Snippet>>)> {
        let query = query.trim().to_lowercase();
        let matched = self.snippets.iter().filter(|snippet| {
            let doc = &snippet.doc;
            query.is_empty()
                || doc.name.to_lowercase().contains(&query)
                || doc.content.to_lowercase().contains(&query)
                || doc.group.to_lowercase().contains(&query)
        });
        snippet_order::grouped(matched, &self.group_order)
    }

    // ------------------------------------------------------------ 拖放

    fn start_drag(&mut self, dragged: Dragged, cx: &mut Context<Self>) {
        self.dragging = Some(dragged);
        self.drop_target = None;
        cx.notify();
    }

    /// 拖动结束而没有放在列表上（放到了别处、按了 Esc）。
    fn finish_drag_if_cancelled(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dragging.is_some() && !cx.has_active_drag() {
            cx.defer_in(window, |this, _, cx| {
                if this.dragging.is_some() && !cx.has_active_drag() {
                    this.dragging = None;
                    this.drop_target = None;
                    cx.notify();
                }
            });
        }
    }

    fn on_row_drag_move(
        &mut self,
        row: &DropRow,
        event: &DragMoveEvent<Dragged>,
        cx: &mut Context<Self>,
    ) {
        if !event.bounds.contains(&event.event.position) {
            return;
        }
        let height = f32::from(event.bounds.size.height).max(1.);
        let y = f32::from(event.event.position.y - event.bounds.origin.y) / height;
        let target = drag::drop_target(event.drag(cx), row, y);
        if target != self.drop_target {
            self.drop_target = target;
            cx.notify();
        }
    }

    fn on_drop(&mut self, dragged: &Dragged, window: &mut Window, cx: &mut Context<Self>) {
        let target = self.drop_target.take();
        self.dragging = None;
        let groups = snippet_order::group_names(&self.snippets, &self.group_order);
        if let Some(target) = target
            && let Some(plan) = drag::plan_move(dragged, &target, &groups)
        {
            let store = AppState::store(cx);
            let result = match plan {
                Move::Snippet {
                    id,
                    group,
                    position,
                } => {
                    tracing::info!(snippet = %id, "拖动快捷命令");
                    // 放进折叠的分组后展开它，看得到放到了哪里
                    if self.is_collapsed(&group, cx) {
                        self.toggle(&group, cx);
                    }
                    store.move_snippet(&id, &group, position)
                }
                Move::Groups(order) => {
                    tracing::info!(groups = order.len(), "拖动快捷命令分组");
                    store.update_settings(|s| s.snippets.group_order = order)
                }
            };
            if let Err(e) = result {
                crate::notify::error(store_errors::describe(&e), window, cx);
            }
        }
        cx.notify();
    }

    /// 拖动时放在这一行的哪里。
    fn target_place(&self, row: &DropRow, cx: &gpui_kit::App) -> Option<DropPlace> {
        self.drop_target
            .as_ref()
            .filter(|t| &t.row == row && cx.has_active_drag())
            .map(|t| t.place)
    }

    /// 让一行能接住拖动：放进去时整行高亮，放在前后时画插入线（2 像素的指示线是物理边界）。
    fn droppable<E>(&self, el: E, row: DropRow, cx: &Context<Self>) -> E
    where
        E: InteractiveElement + ParentElement + Styled + FluentBuilder,
    {
        let theme = cx.theme();
        let place = self.target_place(&row, cx);
        let mut el =
            el.relative()
                .when(place == Some(DropPlace::Into), |el| {
                    el.bg(theme.drop_target)
                        .border_1()
                        .border_color(theme.drag_border)
                })
                .on_drag_move::<Dragged>(cx.listener(
                    move |this, event: &DragMoveEvent<Dragged>, _, cx| {
                        this.on_row_drag_move(&row, event, cx)
                    },
                ))
                .on_drop(cx.listener(|this, dragged: &Dragged, window, cx| {
                    this.on_drop(dragged, window, cx)
                }));
        if let Some(place @ (DropPlace::Before | DropPlace::After)) = place {
            let line = div()
                .absolute()
                .left_0()
                .right_0()
                .h(px(2.))
                .bg(theme.drag_border);
            el = el.child(if place == DropPlace::Before {
                line.top_0()
            } else {
                line.bottom_0()
            });
        }
        el
    }

    /// 开始拖动 `dragged`，跟着指针的是 `label`。
    fn drag_source<E: StatefulInteractiveElement>(
        &self,
        el: E,
        dragged: Dragged,
        label: SharedString,
        cx: &Context<Self>,
    ) -> E {
        let this = cx.entity().downgrade();
        el.on_drag(dragged, move |dragged, _, _, cx| {
            let _ = this.update(cx, |this, cx| this.start_drag(dragged.clone(), cx));
            cx.new(|_| DragPreview(label.clone()))
        })
    }

    // ------------------------------------------------------------ 绘制

    /// `menu_open`：窗口中开着右键菜单，这一行中的按钮不挂悬停提示（行自己的提示经 [`crate::menu_tooltip::quiet`]）。
    fn render_row(
        &self,
        snippet: &Record<Snippet>,
        indent: bool,
        draggable: bool,
        menu_open: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let id = snippet.id.clone();
        let element =
            |name: &str| ElementId::from(SharedString::from(format!("{name}-{}", snippet.id)));
        let content: SharedString = snippet.doc.content.trim_end().to_string().into();
        // 名称放不下时截断，行的悬停提示（内容）前面补上全名
        let name: SharedString = snippet.doc.name.clone().into();
        let clip = Clip::default();
        let menu_id = id.clone();
        let row = h_flex()
            .id(element("snippet-row"))
            .test_support()
            .group("snippet-row")
            .gap_1()
            .pl(if indent {
                gpui_kit::rems(1.25)
            } else {
                gpui_kit::rems(0.375)
            })
            .pr_1()
            .py_0p5()
            .rounded(theme.radius)
            .hover(|s| s.bg(theme.list_hover))
            .child(
                Icon::new(IconName::Play)
                    .xsmall()
                    .text_color(theme.muted_foreground),
            )
            .child(clip.text(name.clone()).flex_1().text_sm())
            .child(
                h_flex()
                    .invisible()
                    .group_hover("snippet-row", |s| s.visible())
                    .child(
                        Button::new(element("snippet-edit"))
                            .ghost()
                            .small()
                            .icon(IconName::Pencil)
                            .when(!menu_open, |this| this.tooltip(t!("snippets.edit")))
                            .on_click({
                                let id = id.clone();
                                move |_, window, cx| {
                                    cx.stop_propagation();
                                    editor::open_edit(id.clone(), window, cx)
                                }
                            }),
                    )
                    .child(
                        Button::new(element("snippet-delete"))
                            .ghost()
                            .small()
                            .icon(IconName::Trash)
                            .when(!menu_open, |this| this.tooltip(t!("snippets.delete")))
                            .on_click({
                                let id = id.clone();
                                move |_, window, cx| {
                                    cx.stop_propagation();
                                    confirm_delete(id.clone(), window, cx)
                                }
                            }),
                    ),
            )
            .tooltip(crate::menu_tooltip::quiet({
                let name = name.clone();
                move |window, cx| {
                    let text = crate::workspace::tooltip_lines([clip.full(&name), Some(&content)]);
                    crate::truncated::tooltip(text, window, cx)
                }
            }))
            .on_click(cx.listener({
                let id = id.clone();
                move |_, _: &ClickEvent, _, cx| {
                    cx.emit(SnippetListEvent::Send {
                        id: id.clone(),
                        run: None,
                    })
                }
            }));
        let row = self.droppable(
            row,
            DropRow::Snippet {
                id: id.clone(),
                group: snippet.doc.group.clone(),
            },
            cx,
        );
        let row = if draggable {
            self.drag_source(row, Dragged::Snippet(id), name, cx)
        } else {
            row
        };
        row.context_menu({
            let this = cx.entity().downgrade();
            move |menu, _, _| {
                let send = |run: Option<bool>| {
                    let (this, id) = (this.clone(), menu_id.clone());
                    move |_: &ClickEvent, _: &mut Window, cx: &mut gpui_kit::App| {
                        let _ = this.update(cx, |_, cx| {
                            cx.emit(SnippetListEvent::Send {
                                id: id.clone(),
                                run,
                            })
                        });
                    }
                };
                let edit = {
                    let id = menu_id.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut gpui_kit::App| {
                        editor::open_edit(id.clone(), window, cx)
                    }
                };
                let delete = {
                    let id = menu_id.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut gpui_kit::App| {
                        confirm_delete(id.clone(), window, cx)
                    }
                };
                menu.item(PopupMenuItem::new(t!("snippets.menu.send")).on_click(send(None)))
                    .item(PopupMenuItem::new(t!("snippets.menu.run")).on_click(send(Some(true))))
                    .item(
                        PopupMenuItem::new(t!("snippets.menu.insert")).on_click(send(Some(false))),
                    )
                    .separator()
                    .item(PopupMenuItem::new(t!("snippets.edit")).on_click(edit))
                    .item(PopupMenuItem::new(t!("snippets.delete")).on_click(delete))
            }
        })
    }

    fn render_header(
        &self,
        group: &str,
        count: usize,
        collapsed: bool,
        draggable: bool,
        menu_open: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let label: SharedString = if group.is_empty() {
            t!("snippets.ungrouped")
        } else {
            group.to_string().into()
        };
        let named = !group.is_empty();
        let new_group = group.to_string();
        let toggle_group = group.to_string();
        let header = h_flex()
            .id(ElementId::from(SharedString::from(format!(
                "snippet-group-{group}"
            ))))
            .test_support()
            .group("snippet-group")
            .gap_1()
            .px_1p5()
            .py_0p5()
            .rounded(theme.radius)
            .hover(|s| s.bg(theme.list_hover))
            .child(
                Icon::new(if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .small()
                .text_color(muted),
            )
            .child(
                truncated(
                    SharedString::from(format!("snippet-group-name-{group}")),
                    label.clone(),
                )
                .flex_1()
                .text_sm()
                .font_weight(gpui_kit::FontWeight::MEDIUM),
            )
            .child(div().text_xs().text_color(muted).child(count.to_string()))
            .child(
                h_flex()
                    .invisible()
                    .group_hover("snippet-group", |s| s.visible())
                    .child(
                        Button::new(SharedString::from(format!("snippet-new-in-{group}")))
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .when(!menu_open, |b| b.tooltip(t!("snippets.new_in_group")))
                            .on_click(move |_, window, cx| {
                                cx.stop_propagation();
                                editor::open_new(new_group.clone(), String::new(), window, cx)
                            }),
                    )
                    .when(named, |el| {
                        el.child(
                            Button::new(SharedString::from(format!("snippet-group-more-{group}")))
                                .ghost()
                                .small()
                                .icon(IconName::Ellipsis)
                                .when(!menu_open, |b| b.tooltip(t!("hosts.toolbar.more")))
                                .dropdown_menu(crate::menu_fit::fitted(groups::group_menu(
                                    group.to_string(),
                                ))),
                        )
                    })
                    // 未分组没有 ⋯：留出同样的宽度，各组的条数上下对齐
                    .when(!named, |el| el.child(div().flex_none().size_6())),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.toggle(&toggle_group, cx)));
        let header = self.droppable(header, DropRow::Group(group.to_string()), cx);
        if !named {
            return header.into_any_element();
        }
        let header = if draggable {
            self.drag_source(header, Dragged::Group(group.to_string()), label, cx)
        } else {
            header
        };
        header
            .context_menu(crate::menu_fit::fitted(groups::group_menu(
                group.to_string(),
            )))
            .into_any_element()
    }

    /// 拖动快捷命令、又没有未分组的标题时，最下面多出的放置位置。
    fn render_ungrouped_drop(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let row = div()
            .id("snippet-drop-ungrouped")
            .test_support()
            .px_1p5()
            .py_0p5()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(t!("snippets.drop_ungrouped"));
        self.droppable(row, DropRow::Ungrouped, cx)
    }
}

impl Render for SnippetList {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.finish_drag_if_cancelled(window, cx);
        let menu_open = crate::menu_tooltip::menu_open(window, cx);
        let query = self.search.read(cx).value().to_string();
        // 搜索结果看不出完整的顺序：不能拖
        let searching = !query.trim().is_empty();
        let groups = self.grouped(&query);
        let show_headers = groups.iter().any(|(g, _)| !g.is_empty());
        let dragging_group = matches!(self.dragging, Some(Dragged::Group(_)));
        let muted = cx.theme().muted_foreground;
        let settings = AppState::store(cx).settings();
        let os = crate::keymap::current_os(cx);
        let picker_key = keymap::effective(&settings.keybindings, os)
            .combos(KeyAction::CommandPicker)
            .first()
            .map(|c| c.label(os));
        let mut list = v_flex().gap_0p5();
        if groups.is_empty() {
            // 与主机、密钥、转发页的空状态相同的边距；还没有快捷命令时顺带说怎么在终端中调出、命令历史在哪管理
            let body = if query.trim().is_empty() {
                v_flex()
                    .gap_2()
                    .child(prose(t!("snippets.empty")))
                    .child(prose(match picker_key {
                        Some(key) => t!("snippets.picker_hint", key = key),
                        None => t!("snippets.picker_hint_no_key"),
                    }))
            } else {
                v_flex().child(prose(t!("snippets.no_match")))
            };
            list = list.child(div().p_4().text_sm().text_color(muted).child(body));
        }
        for (group, snippets) in &groups {
            // 搜索时不折叠；拖动分组时只留标题
            let collapsed = dragging_group || (!searching && self.is_collapsed(group, cx));
            if show_headers {
                list = list.child(self.render_header(
                    group,
                    snippets.len(),
                    collapsed,
                    !searching,
                    menu_open,
                    cx,
                ));
            }
            if !collapsed {
                list = list.children(
                    snippets
                        .iter()
                        .map(|s| self.render_row(s, show_headers, !searching, menu_open, cx)),
                );
            }
        }
        // 放在最下面：拖动开始时不挤动指针下的行
        if show_headers
            && matches!(self.dragging, Some(Dragged::Snippet(_)))
            && groups.first().is_some_and(|(g, _)| !g.is_empty())
        {
            list = list.child(self.render_ungrouped_drop(cx));
        }
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .gap_1()
                    .px_2()
                    .pt_2()
                    .pb_1()
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&self.search)
                                .id("snippet-search")
                                .small()
                                .cleanable(true),
                        ),
                    )
                    .child(
                        Button::new("snippet-new")
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .tooltip(t!("snippets.new_title"))
                            .on_click(|_, window, cx| {
                                editor::open_new(String::new(), String::new(), window, cx)
                            }),
                    )
                    .child(
                        Button::new("snippets-more")
                            .ghost()
                            .small()
                            .icon(IconName::Ellipsis)
                            .tooltip(t!("hosts.toolbar.more"))
                            .dropdown_menu(|menu, _, _| {
                                menu.item(PopupMenuItem::new(t!("snippets.import")).on_click(
                                    |_, window, cx| {
                                        crate::backup_ui::open_import_snippets(window, cx)
                                    },
                                ))
                                .item(
                                    PopupMenuItem::new(t!("snippets.export")).on_click(
                                        |_, window, cx| {
                                            crate::backup_ui::open_export_snippets(window, cx)
                                        },
                                    ),
                                )
                            }),
                    )
                    .child({
                        let open = super::bar::is_open(cx);
                        Button::new("snippet-bar-toggle")
                            .ghost()
                            .small()
                            .icon(IconName::PanelBottom)
                            .selected(open)
                            .tooltip(if open {
                                t!("snippets.bar.hide")
                            } else {
                                t!("snippets.bar.show")
                            })
                            .on_click(|_, _, cx| super::bar::toggle(cx))
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .pl_1()
                    .scrollbar_gutter()
                    .overflow_y_scrollbar()
                    .id("snippet-list")
                    // 放在两行之间的空隙里：照拖动时指着的位置放
                    .on_drop(cx.listener(|this, dragged: &Dragged, window, cx| {
                        this.on_drop(dragged, window, cx)
                    }))
                    .child(list),
            )
    }
}
