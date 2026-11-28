//! 新建、编辑快捷命令：名称、分组（可以输入新的，也可以从右边的下拉中选已有的）、内容（可多行）、发送后是否立即执行
//! （内容末尾的换行）。

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, Focusable as _, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::id::SnippetId;
use termlet_core::model::Snippet;
use termlet_core::snippet_order;
use termlet_core::store::Position;

use crate::app_state::AppState;
use crate::prose::{ProseLabel as _, prose};
use crate::ui_scale::DialogTitleExt as _;
use crate::{store_errors, t};

/// 新建快捷命令，预先填好分组与内容（例如把历史命令存为快捷命令）。
pub fn open_new(group: String, content: String, window: &mut Window, cx: &mut App) {
    open(
        None,
        Snippet {
            group,
            content,
            ..Default::default()
        },
        window,
        cx,
    );
}

/// 编辑快捷命令。
pub fn open_edit(id: SnippetId, window: &mut Window, cx: &mut App) {
    let Some(record) = AppState::store(cx).get::<Snippet>(&id) else {
        return;
    };
    open(Some(id), record.doc, window, cx);
}

fn open(id: Option<SnippetId>, initial: Snippet, window: &mut Window, cx: &mut App) {
    let editing = id.is_some();
    let form = cx.new(|cx| SnippetForm::new(id, initial, window, cx));
    let first = Rc::new(Cell::new(true));
    let width = rems(28.).to_pixels(window.rem_size());
    window.open_dialog(cx, move |dialog, window, cx| {
        if first.replace(false) {
            form.read(cx).name.focus_handle(cx).focus(window, cx);
        }
        // 在名称、分组中按 Enter 与点“保存”相同（不能保存时不关闭）
        let save = {
            let form = form.clone();
            move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| {
                form.update(cx, |form, cx| form.save(cx))
            }
        };
        dialog
            .titled(if editing {
                t!("snippets.edit_title")
            } else {
                t!("snippets.new_title")
            })
            .w(width)
            .overlay_closable(false)
            .child(form.clone())
            .on_ok(save)
    });
}

struct SnippetForm {
    id: Option<SnippetId>,
    initial: Snippet,
    name: Entity<InputState>,
    group: Entity<InputState>,
    /// 已有的分组（打开时的，按快捷命令页的顺序，不含未分组）
    groups: Vec<String>,
    content: Entity<TextareaState>,
    /// 发送后立即执行（保存时内容末尾加换行）
    run: bool,
    error: Option<SharedString>,
    _subscriptions: [Subscription; 2],
}

impl SnippetForm {
    fn new(
        id: Option<SnippetId>,
        initial: Snippet,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // 新建时默认立即执行
        let run = if id.is_some() {
            initial.content.ends_with('\n')
        } else {
            true
        };
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("snippets.name"))
                .default_value(initial.name.clone())
        });
        let group = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("snippets.group_placeholder"))
                .default_value(initial.group.clone())
        });
        let content = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(6)
                .placeholder(t!("snippets.content"))
                .default_value(initial.content.trim_end_matches(['\r', '\n']).to_string())
        });
        let subscriptions = [
            cx.observe(&name, |_, _, cx| cx.notify()),
            cx.observe(&content, |_, _, cx| cx.notify()),
        ];
        let store = AppState::store(cx);
        let groups = snippet_order::group_names(
            &store.list::<Snippet>(),
            &store.settings().snippets.group_order,
        )
        .into_iter()
        .filter(|g| !g.is_empty())
        .collect();
        Self {
            id,
            initial,
            name,
            group,
            groups,
            content,
            run,
            error: None,
            _subscriptions: subscriptions,
        }
    }

    fn can_save(&self, cx: &App) -> bool {
        !self.name.read(cx).value().trim().is_empty()
            && !self.content.read(cx).value().trim().is_empty()
    }

    fn save(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.can_save(cx) {
            return false;
        }
        let body = self
            .content
            .read(cx)
            .value()
            .trim_end_matches(['\r', '\n'])
            .to_string();
        let snippet = Snippet {
            name: self.name.read(cx).value().trim().to_string(),
            group: self.group.read(cx).value().trim().to_string(),
            content: if self.run { format!("{body}\n") } else { body },
            ..self.initial.clone()
        };
        let store = AppState::store(cx);
        let result = match &self.id {
            // 编辑期间已被同步或命令行删掉：另存为新的，不丢掉这次的修改
            Some(id) => match store.update(id, snippet.clone()) {
                Err(termlet_core::CoreError::NotFound(_)) => store
                    .create(snippet, None, Position::Last)
                    .map(|id| self.id = Some(id)),
                other => other,
            },
            None => store.create(snippet, None, Position::Last).map(|_| ()),
        };
        match result {
            Ok(()) => {
                // 只记名称，不记命令内容
                tracing::info!(snippet = ?self.id, name = %self.name.read(cx).value(), "已保存快捷命令");
                true
            }
            Err(e) => {
                tracing::warn!(snippet = ?self.id, "保存快捷命令失败：{e}");
                self.error = Some(store_errors::describe(&e));
                cx.notify();
                false
            }
        }
    }

    /// 分组输入框右边的下拉：选一个已有的分组（或未分组）填进去。还没有分组时没有它。
    fn render_group_choices(&self, cx: &Context<Self>) -> Option<impl IntoElement + use<>> {
        if self.groups.is_empty() {
            return None;
        }
        let names: Vec<String> = std::iter::once(String::new())
            .chain(self.groups.iter().cloned())
            .collect();
        let current = self.group.read(cx).value().trim().to_string();
        let input = self.group.clone();
        Some(
            Button::new("snippet-group-choices")
                .ghost()
                .small()
                .icon(IconName::ChevronDown)
                .tooltip(t!("snippets.group.choose"))
                .dropdown_menu(crate::menu_fit::fitted(move |menu, _, _| {
                    names.iter().fold(menu, |menu, name| {
                        let label: SharedString = if name.is_empty() {
                            t!("snippets.ungrouped")
                        } else {
                            name.clone().into()
                        };
                        let (input, name) = (input.clone(), name.clone());
                        menu.item(PopupMenuItem::new(label).checked(current == name).on_click(
                            move |_, window, cx| {
                                input.update(cx, |state, cx| {
                                    state.set_value(name.clone(), window, cx)
                                })
                            },
                        ))
                    })
                })),
        )
    }
}

impl Render for SnippetForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let can_save = self.can_save(cx);
        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.name).id("snippet-name")),
                    )
                    .child(
                        div().w(rems(9.)).child(
                            Input::new(&self.group)
                                .id("snippet-group")
                                .when_some(self.render_group_choices(cx), |input, choices| {
                                    input.suffix(choices)
                                }),
                        ),
                    ),
            )
            .child(Textarea::new(&self.content))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(prose(t!("snippets.params_hint"))),
            )
            .child(
                Checkbox::new("snippet-run")
                    .prose_label(t!("snippets.run"))
                    .checked(self.run)
                    .on_change(cx.listener(|this, checked: &bool, _, cx| {
                        this.run = *checked;
                        cx.notify();
                    })),
            )
            .child(
                h_flex()
                    .gap_2()
                    .pt_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(theme.danger)
                            .when_some(self.error.clone(), |this, e| this.child(e)),
                    )
                    .child(
                        Button::new("snippet-cancel")
                            .label(t!("common.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("snippet-save")
                            .primary()
                            .label(t!("common.save"))
                            .disabled(!can_save)
                            .on_click(cx.listener(|this, _, window, cx| {
                                if this.save(cx) {
                                    window.close_dialog(cx);
                                }
                            })),
                    ),
            )
    }
}
