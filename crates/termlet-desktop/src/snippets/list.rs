//! 侧边栏的“快捷命令”页：按分组折叠显示，搜索、新建、编辑、删除；单击发送到当前终端。
//!
//! 只有未分组的快捷命令时不显示分组标题。分组的折叠状态记在本机的界面状态中（键为 `snippet-group:<分组名>`）。

use std::collections::BTreeMap;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, ClickEvent, Context, ElementId, Entity, EventEmitter, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, TestSupportExt as _, Window, div, prelude::FluentBuilder as _,
};
use termlet_core::id::SnippetId;
use termlet_core::keymap::{self, KeyAction};
use termlet_core::model::{Kind, Snippet};
use termlet_core::store::{Record, StoreEvent};

use super::{confirm_delete, editor};
use crate::app_state::AppState;
use crate::i18n::LocalizedPlaceholder as _;
use crate::prose::prose;
use crate::t;

/// 快捷命令页发给工作区的请求。
pub enum SnippetListEvent {
    /// 发送到当前终端；`run` 为 `None` 时按快捷命令保存时的设置执行或只写入
    Send { id: SnippetId, run: Option<bool> },
}

pub struct SnippetList {
    search: Entity<InputState>,
    snippets: Vec<Record<Snippet>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SnippetListEvent> for SnippetList {}

const GROUP_KEY: &str = "snippet-group:";

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
            _subscriptions: subscriptions,
        };
        this.reload(cx);
        this
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.snippets = AppState::store(cx).list::<Snippet>();
        cx.notify();
    }

    fn is_collapsed(&self, group: &str, cx: &gpui_kit::App) -> bool {
        AppState::ui(cx)
            .read(cx)
            .state()
            .collapsed_groups
            .contains(&format!("{GROUP_KEY}{group}"))
    }

    fn toggle(&mut self, group: &str, cx: &mut Context<Self>) {
        let key = format!("{GROUP_KEY}{group}");
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

    /// 按分组归类：未分组在前，分组按名称；分组内按保存的顺序。
    fn grouped(&self, query: &str) -> Vec<(String, Vec<&Record<Snippet>>)> {
        let query = query.trim().to_lowercase();
        let mut groups: BTreeMap<String, Vec<&Record<Snippet>>> = BTreeMap::new();
        for snippet in &self.snippets {
            let doc = &snippet.doc;
            let matched = query.is_empty()
                || doc.name.to_lowercase().contains(&query)
                || doc.content.to_lowercase().contains(&query)
                || doc.group.to_lowercase().contains(&query);
            if matched {
                groups.entry(doc.group.clone()).or_default().push(snippet);
            }
        }
        groups.into_iter().collect()
    }

    /// `menu_open`：窗口中开着右键菜单，这一行中的按钮不挂悬停提示（行自己的提示经 [`crate::menu_tooltip::quiet`]）。
    fn render_row(
        &self,
        snippet: &Record<Snippet>,
        indent: bool,
        menu_open: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let id = snippet.id.clone();
        let element =
            |name: &str| ElementId::from(SharedString::from(format!("{name}-{}", snippet.id)));
        let content: SharedString = snippet.doc.content.trim_end().to_string().into();
        let menu_id = id.clone();
        h_flex()
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
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .child(snippet.doc.name.clone()),
            )
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
            .tooltip(crate::menu_tooltip::quiet(move |window, cx| {
                Tooltip::new(content.clone()).build(window, cx)
            }))
            .on_click(cx.listener({
                let id = id.clone();
                move |_, _: &ClickEvent, _, cx| {
                    cx.emit(SnippetListEvent::Send {
                        id: id.clone(),
                        run: None,
                    })
                }
            }))
            .context_menu({
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
                        .item(
                            PopupMenuItem::new(t!("snippets.menu.run")).on_click(send(Some(true))),
                        )
                        .item(
                            PopupMenuItem::new(t!("snippets.menu.insert"))
                                .on_click(send(Some(false))),
                        )
                        .separator()
                        .item(PopupMenuItem::new(t!("snippets.edit")).on_click(edit))
                        .item(PopupMenuItem::new(t!("snippets.delete")).on_click(delete))
                }
            })
    }
}

impl Render for SnippetList {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let menu_open = crate::menu_tooltip::menu_open(window, cx);
        let query = self.search.read(cx).value().to_string();
        let groups = self.grouped(&query);
        let show_headers = groups.iter().any(|(g, _)| !g.is_empty());
        let muted = cx.theme().muted_foreground;
        let hover = cx.theme().list_hover;
        let radius = cx.theme().radius;
        let mut list = v_flex().gap_0p5();
        if groups.is_empty() {
            // 与主机、密钥、转发页的空状态相同的边距
            list = list.child(div().p_4().text_sm().text_color(muted).child(
                if query.trim().is_empty() {
                    t!("snippets.empty")
                } else {
                    t!("snippets.no_match")
                },
            ));
        }
        for (group, snippets) in &groups {
            // 搜索时不折叠
            let collapsed = query.trim().is_empty() && self.is_collapsed(group, cx);
            if show_headers {
                let toggle_group = group.clone();
                let new_group = group.clone();
                let header_id =
                    ElementId::from(SharedString::from(format!("snippet-group-{group}")));
                list = list.child(
                    h_flex()
                        .id(header_id)
                        .test_support()
                        .group("snippet-group")
                        .gap_1()
                        .px_1p5()
                        .py_0p5()
                        .rounded(radius)
                        .hover(|s| s.bg(hover))
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
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_sm()
                                .font_weight(gpui_kit::FontWeight::MEDIUM)
                                .child(prose(if group.is_empty() {
                                    t!("snippets.ungrouped")
                                } else {
                                    group.clone().into()
                                })),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(snippets.len().to_string()),
                        )
                        .child(
                            div()
                                .invisible()
                                .group_hover("snippet-group", |s| s.visible())
                                .child(
                                    Button::new(SharedString::from(format!(
                                        "snippet-new-in-{group}"
                                    )))
                                    .ghost()
                                    .small()
                                    .icon(IconName::Plus)
                                    .tooltip(t!("snippets.new_in_group"))
                                    .on_click(
                                        move |_, window, cx| {
                                            cx.stop_propagation();
                                            editor::open_new(
                                                new_group.clone(),
                                                String::new(),
                                                window,
                                                cx,
                                            )
                                        },
                                    ),
                                ),
                        )
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.toggle(&toggle_group, cx)),
                        ),
                );
            }
            if !collapsed {
                list = list.children(
                    snippets
                        .iter()
                        .map(|s| self.render_row(s, show_headers, menu_open, cx)),
                );
            }
        }
        let settings = AppState::store(cx).settings();
        let os = crate::keymap::current_os(cx);
        let picker_key = keymap::effective(&settings.keybindings, os)
            .combos(KeyAction::CommandPicker)
            .first()
            .map(|c| c.label(os));
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
                    .id("snippet-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_1()
                    .child(list),
            )
            .child(
                div()
                    .px_3()
                    .py_1p5()
                    .text_xs()
                    .text_color(muted)
                    .child(prose(match picker_key {
                        Some(key) => t!("snippets.picker_hint", key = key),
                        None => t!("snippets.picker_hint_no_key"),
                    })),
            )
    }
}
