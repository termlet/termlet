//! 快捷命令选择器（在终端中按 Ctrl+Shift+Space）：搜索快捷命令与这台主机的命令历史，Enter 发送到终端、
//! Tab 只写入不执行。带参数的快捷命令（`{{名称}}`、`{{名称:默认值}}`）先填写参数再发送。
//! 历史：Shift+Delete 删除这一条，Ctrl+S 存为快捷命令。快捷命令的新建、编辑在侧边栏的“快捷命令”页。
//!
//! 这些按键在输入框中各有用途，所以在快捷键分发之前拦下（`intercept_keystrokes`）；只处理焦点在选择器内的按键
//! （别的窗口、选择器上面的对话框、锁屏中的按键都不管）。

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, ElementId, Entity, FocusHandle, Focusable as _,
    InteractiveElement as _, IntoElement, KeystrokeEvent, ParentElement as _, Render, ScrollHandle,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, TestSupportExt as _,
    WeakEntity, Window, div, prelude::FluentBuilder as _, rems,
};
use termlet_core::history::{self, HistoryEntry};
use termlet_core::id::SnippetId;
use termlet_core::model::Snippet;
use termlet_core::snippet_params::{SnippetParam, fill_snippet, snippet_params};
use termlet_core::store::Record;

use super::{CommandInput, command_input, editor};
use crate::app_state::AppState;
use crate::keymap::{key_os, pressed};
use crate::prose::prose;
use crate::t;
use crate::terminal_panel::TerminalPanel;

/// 历史最多列出这么多条。
const HISTORY_LIMIT: usize = 30;

/// 打开选择器，选好后写入 `target`。`preset` 是直接为它填写参数的快捷命令（在侧边栏点击带参数的快捷命令时）。
pub fn open(
    target: Option<WeakEntity<TerminalPanel>>,
    preset: Option<(SnippetId, Option<bool>)>,
    window: &mut Window,
    cx: &mut App,
) {
    let picker = cx.new(|cx| CommandPicker::new(target, preset, window, cx));
    let first = Rc::new(Cell::new(true));
    let width = rems(36.).to_pixels(window.rem_size());
    window.open_dialog(cx, move |dialog, window, cx| {
        if first.replace(false) {
            picker.update(cx, |picker, cx| picker.focus_first(window, cx));
        }
        dialog
            .p_0()
            .close_button(false)
            .w(width)
            .child(picker.clone())
    });
}

/// 选择器里另外的几个组合（按系统：macOS 上用 ⌘）：放到撰写栏、删除这条历史、存为快捷命令。
struct PickerKeys {
    os: gpui_kit::OperatingSystem,
    compose: &'static str,
    delete: &'static str,
    save: &'static str,
}

impl PickerKeys {
    fn of(os: gpui_kit::OperatingSystem) -> Self {
        if os == gpui_kit::OperatingSystem::Mac {
            // macOS 的键盘上没有向后删除键：照 Finder 用 ⌘⌫ 删除
            Self {
                os,
                compose: "cmd-enter",
                delete: "cmd-backspace",
                save: "cmd-s",
            }
        } else {
            Self {
                os,
                compose: "ctrl-enter",
                delete: "shift-delete",
                save: "ctrl-s",
            }
        }
    }

    /// 提示中的写法（`Ctrl+Enter`、`⌘↩`）。
    fn label(&self, combo: &str) -> String {
        termlet_core::keymap::label_of(combo, key_os(self.os))
    }
}

#[derive(Clone)]
enum Item {
    Snippet(Record<Snippet>),
    History(HistoryEntry),
}

/// 正在为一条快捷命令填写参数。
struct Filling {
    snippet: Record<Snippet>,
    /// 发送方式：`None` 按快捷命令保存时的设置，`Some(false)` 只写入
    run: Option<bool>,
    /// 填好后放进撰写栏（Ctrl+Enter），不发送
    compose: bool,
    params: Vec<(SnippetParam, Entity<InputState>)>,
    /// 参数输入框变化时重绘预览；回到列表时随之丢弃
    _subscriptions: Vec<Subscription>,
}

pub struct CommandPicker {
    focus: FocusHandle,
    target: Option<WeakEntity<TerminalPanel>>,
    query: Entity<InputState>,
    snippets: Vec<Record<Snippet>>,
    /// 这台主机的全部历史（新的在前）；命令历史关闭或没有终端时为空
    history: Vec<HistoryEntry>,
    history_owner: Option<(String, SharedString)>,
    items: Vec<Item>,
    selected: usize,
    filling: Option<Filling>,
    /// 直接为某条快捷命令填写参数时，Esc 关闭而不是回到列表
    preset: bool,
    error: Option<SharedString>,
    scroll: ScrollHandle,
    /// 每一项在列表容器中的子元素序号（中间夹着分类标题），滚动到选中项时用
    child_index: Vec<usize>,
    _intercept: Subscription,
    _subscriptions: Vec<Subscription>,
}

impl CommandPicker {
    fn new(
        target: Option<WeakEntity<TerminalPanel>>,
        preset: Option<(SnippetId, Option<bool>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = AppState::store(cx);
        let query = cx.new(|cx| InputState::new(window, cx).placeholder(t!("picker.placeholder")));
        let subscriptions = vec![cx.subscribe(&query, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.refilter(cx);
            }
        })];
        let panel = target.as_ref().and_then(|t| t.upgrade());
        let history_owner = panel
            .filter(|_| store.settings().terminal.command_history)
            .map(|p| {
                let p = p.read(cx);
                (p.history_owner(), p.name().clone())
            });
        let history = history_owner
            .as_ref()
            .and_then(|(owner, _)| history::list(&store, owner).ok())
            .unwrap_or_default();
        let this = cx.entity().downgrade();
        let intercept = cx.intercept_keystrokes(move |event: &KeystrokeEvent, window, cx| {
            let _ = this.update(cx, |this, cx| this.on_keystroke(event, window, cx));
        });
        let mut picker = Self {
            focus: cx.focus_handle(),
            target,
            query,
            snippets: store.list::<Snippet>(),
            history,
            history_owner,
            items: Vec::new(),
            selected: 0,
            filling: None,
            preset: preset.is_some(),
            error: None,
            scroll: ScrollHandle::new(),
            child_index: Vec::new(),
            _intercept: intercept,
            _subscriptions: subscriptions,
        };
        picker.refilter(cx);
        if let Some((id, run)) = preset
            && let Some(snippet) = picker.snippets.iter().find(|s| s.id == id).cloned()
        {
            picker.start_filling(snippet, run, window, cx);
        }
        picker
    }

    fn focus_first(&self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.filling {
            Some(filling) => {
                if let Some((_, input)) = filling.params.first() {
                    input.focus_handle(cx).focus(window, cx);
                }
            }
            None => self.query.focus_handle(cx).focus(window, cx),
        }
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        let query = self.query.read(cx).value().trim().to_lowercase();
        let matches = |text: &str| query.is_empty() || text.to_lowercase().contains(&query);
        self.items = self
            .snippets
            .iter()
            .filter(|s| matches(&s.doc.name) || matches(&s.doc.content) || matches(&s.doc.group))
            .cloned()
            .map(Item::Snippet)
            .chain(
                self.history
                    .iter()
                    .filter(|e| matches(&e.command))
                    .take(HISTORY_LIMIT)
                    .cloned()
                    .map(Item::History),
            )
            .collect();
        // 每一项在列表容器中的子元素序号：快捷命令、历史各有一个分类标题
        let first_history = self
            .items
            .iter()
            .position(|i| matches!(i, Item::History(_)));
        self.child_index = (0..self.items.len())
            .map(|ix| {
                let snippet_heading = usize::from(first_history != Some(0));
                let history_heading = usize::from(first_history.is_some_and(|f| ix >= f));
                ix + snippet_heading + history_heading
            })
            .collect();
        self.selected = 0;
        self.error = None;
        cx.notify();
    }

    fn can_send(&self, cx: &App) -> bool {
        self.target
            .as_ref()
            .and_then(|t| t.upgrade())
            .is_some_and(|p| p.read(cx).can_write(cx))
    }

    fn on_keystroke(
        &mut self,
        event: &KeystrokeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.focus.contains_focused(window, cx) || crate::lock_ui::is_locked(cx) {
            return;
        }
        let keystroke = &event.keystroke;
        let m = keystroke.modifiers;
        let plain = !m.control && !m.alt && !m.shift && !m.platform;
        let key = keystroke.key.as_str();
        if self.filling.is_some() {
            match key {
                "enter" if plain => self.submit_params(window, cx),
                "escape" if plain => {
                    if self.preset {
                        window.close_dialog(cx);
                    } else {
                        self.filling = None;
                        self.focus_first(window, cx);
                        cx.notify();
                    }
                }
                _ => return,
            }
            cx.stop_propagation();
            return;
        }
        let os = cx.platform_info().os;
        let extra = PickerKeys::of(os);
        match key {
            "up" if plain => self.move_selection(-1, cx),
            "down" if plain => self.move_selection(1, cx),
            "enter" if plain => self.choose(None, window, cx),
            "tab" if plain => self.choose(Some(false), window, cx),
            _ if pressed(keystroke, extra.compose, os) => self.choose_compose(window, cx),
            _ if pressed(keystroke, extra.delete, os) => {
                if !self.delete_history(cx) {
                    return;
                }
            }
            _ if pressed(keystroke, extra.save, os) => {
                let Some(Item::History(entry)) = self.items.get(self.selected).cloned() else {
                    return;
                };
                window.close_dialog(cx);
                editor::open_new(String::new(), format!("{}\n", entry.command), window, cx);
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    fn move_selection(&mut self, step: isize, cx: &mut Context<Self>) {
        if self.items.is_empty() {
            return;
        }
        let last = self.items.len() as isize - 1;
        self.selected = (self.selected as isize + step).clamp(0, last) as usize;
        if let Some(child) = self.child_index.get(self.selected) {
            self.scroll.scroll_to_item(*child);
        }
        cx.notify();
    }

    /// 删除选中的历史；选中的不是历史时返回 false（按键照常交给输入框）。
    fn delete_history(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(Item::History(entry)) = self.items.get(self.selected).cloned() else {
            return false;
        };
        let Some((owner, _)) = &self.history_owner else {
            return false;
        };
        let store = AppState::store(cx);
        if history::delete(&store, owner, &entry.command).is_ok() {
            self.history.retain(|e| e.command != entry.command);
            let selected = self.selected;
            self.refilter(cx);
            self.selected = selected.min(self.items.len().saturating_sub(1));
        }
        true
    }

    /// 选中了一项：`run` 为 `None` 时发送（快捷命令按保存时的设置，历史执行），`Some(false)` 只写入。
    fn choose(&mut self, run: Option<bool>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.items.get(self.selected).cloned() else {
            return;
        };
        match item {
            Item::History(entry) => {
                let run = run.unwrap_or(true);
                self.finish(command_input(&entry.command, Some(run)), window, cx);
            }
            Item::Snippet(snippet) => {
                if snippet_params(&snippet.doc.content).is_empty() {
                    self.finish(command_input(&snippet.doc.content, run), window, cx);
                } else {
                    self.start_filling(snippet, run, window, cx);
                }
            }
        }
    }

    fn start_filling(
        &mut self,
        snippet: Record<Snippet>,
        run: Option<bool>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut subscriptions = Vec::new();
        let params = snippet_params(&snippet.doc.content)
            .into_iter()
            .map(|param| {
                let input = cx.new(|cx| {
                    InputState::new(window, cx).default_value(param.default_value.clone())
                });
                subscriptions.push(cx.observe(&input, |_, _, cx| cx.notify()));
                (param, input)
            })
            .collect();
        self.filling = Some(Filling {
            snippet,
            run,
            compose: false,
            params,
            _subscriptions: subscriptions,
        });
        self.error = None;
        self.focus_first(window, cx);
        cx.notify();
    }

    fn filled_text(&self, cx: &App) -> Option<String> {
        let filling = self.filling.as_ref()?;
        let values: HashMap<String, String> = filling
            .params
            .iter()
            .map(|(p, input)| (p.name.clone(), input.read(cx).value().to_string()))
            .collect();
        Some(fill_snippet(&filling.snippet.doc.content, &values))
    }

    fn submit_params(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(text), Some(run)) = (self.filled_text(cx), self.filling.as_ref().map(|f| f.run))
        else {
            return;
        };
        if self.filling.as_ref().is_some_and(|f| f.compose) {
            self.compose(text, window, cx);
            return;
        }
        self.finish(command_input(&text, run), window, cx);
    }

    /// Ctrl+Enter：放进撰写栏（发送到多个终端），带参数的先填写参数。
    fn choose_compose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.items.get(self.selected).cloned() else {
            return;
        };
        match item {
            Item::History(entry) => self.compose(entry.command.clone(), window, cx),
            Item::Snippet(snippet) => {
                if snippet_params(&snippet.doc.content).is_empty() {
                    self.compose(snippet.doc.content.clone(), window, cx);
                } else {
                    self.start_filling(snippet, None, window, cx);
                    if let Some(filling) = self.filling.as_mut() {
                        filling.compose = true;
                    }
                }
            }
        }
    }

    /// 关闭选择器，把内容放进这个窗口的撰写栏。
    fn compose(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        window.close_dialog(cx);
        if let Some(workspace) = crate::cli_bridge::workspace_in(window, cx) {
            workspace.update(cx, |w, cx| w.compose_text(&text, window, cx));
        }
    }

    /// 写入终端；成功后关闭选择器，焦点回到终端。
    fn finish(&mut self, input: CommandInput, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.target.as_ref().and_then(|t| t.upgrade()) else {
            self.error = Some(t!("snippets.no_terminal"));
            cx.notify();
            return;
        };
        match panel.update(cx, |panel, cx| panel.write_command(input, cx)) {
            Ok(()) => {
                tracing::info!(session = %panel.read(cx).name(), "快捷命令选择器：写入终端");
                window.close_dialog(cx);
                panel.read(cx).focus_handle(cx).focus(window, cx);
            }
            Err(message) => {
                // 只写入多行被拒绝等：留在选择器中，说明原因
                self.error = Some(message.clone());
                crate::notify::warning(message, window, cx);
                cx.notify();
            }
        }
    }

    fn render_filling(&self, filling: &Filling, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let preview = self.filled_text(cx).unwrap_or_default();
        let preview = preview.trim_end_matches(['\r', '\n']).to_string();
        let submit_label = if filling.compose {
            t!("picker.submit_compose")
        } else if filling.run == Some(false) {
            t!("picker.submit_insert")
        } else {
            t!("picker.submit_send")
        };
        v_flex()
            .gap_3()
            .p_4()
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .text_sm()
                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                            .child(Icon::new(IconName::Zap).small().text_color(theme.warning))
                            .child(filling.snippet.doc.name.clone()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .font_family(termlet_terminal::fonts::DEFAULT_FAMILY)
                            .child(filling.snippet.doc.content.trim_end().to_string()),
                    ),
            )
            .children(filling.params.iter().map(|(param, input)| {
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(param.name.clone()),
                    )
                    .child(Input::new(input).small())
            }))
            .child(
                div()
                    .id("picker-preview")
                    .test_support()
                    .px_2()
                    .py_1p5()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.muted)
                    .truncate()
                    .text_xs()
                    .font_family(termlet_terminal::fonts::DEFAULT_FAMILY)
                    .child(preview),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(div().text_sm().text_color(theme.danger).child(prose(error)))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("picker-back")
                            .label(if self.preset {
                                t!("common.cancel")
                            } else {
                                t!("picker.back")
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                if this.preset {
                                    window.close_dialog(cx);
                                } else {
                                    this.filling = None;
                                    this.focus_first(window, cx);
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        Button::new("picker-param-submit")
                            .primary()
                            .label(submit_label)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.submit_params(window, cx)),
                            ),
                    ),
            )
    }

    fn render_item(&self, ix: usize, item: &Item, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let selected = ix == self.selected;
        let id = match item {
            Item::Snippet(s) => {
                ElementId::from(SharedString::from(format!("picker-snippet-{}", s.id)))
            }
            Item::History(e) => {
                ElementId::from(SharedString::from(format!("picker-history-{}", e.id)))
            }
        };
        let row = h_flex()
            .id(id)
            .test_support()
            .gap_2()
            .px_2()
            .py_1p5()
            .rounded(theme.radius)
            .when(selected, |el| el.bg(theme.list_active))
            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                if this.selected != ix {
                    this.selected = ix;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.selected = ix;
                this.choose(None, window, cx);
            }));
        match item {
            Item::Snippet(snippet) => row
                .child(Icon::new(IconName::Zap).small().text_color(theme.warning))
                .when(!snippet.doc.group.is_empty(), |el| {
                    el.child(
                        div()
                            .flex_none()
                            .px_1()
                            .rounded(theme.radius)
                            .bg(theme.muted)
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(snippet.doc.group.clone()),
                    )
                })
                .child(
                    div()
                        .flex_none()
                        .text_sm()
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                        .child(snippet.doc.name.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .font_family(termlet_terminal::fonts::DEFAULT_FAMILY)
                        .child(snippet.doc.content.trim_end().to_string()),
                ),
            Item::History(entry) => row
                .child(
                    Icon::new(IconName::Clock)
                        .small()
                        .text_color(theme.muted_foreground),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .font_family(termlet_terminal::fonts::DEFAULT_FAMILY)
                        .child(entry.command.clone()),
                ),
        }
    }
}

impl Render for CommandPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(filling) = &self.filling {
            return v_flex()
                .id("command-picker")
                .test_support()
                .track_focus(&self.focus)
                .child(self.render_filling(filling, cx))
                .into_any_element();
        }
        let theme = cx.theme();
        let first_history = self
            .items
            .iter()
            .position(|i| matches!(i, Item::History(_)));
        let mut list = div()
            .id("picker-list")
            .max_h(rems(20.))
            .p_1()
            .overflow_y_scroll()
            .track_scroll(&self.scroll);
        if self.items.is_empty() {
            list = list.child(
                div()
                    .px_3()
                    .py_6()
                    .text_center()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(prose(
                        if self.snippets.is_empty() && self.history.is_empty() {
                            t!("picker.empty_all")
                        } else {
                            t!("picker.empty")
                        },
                    )),
            );
        }
        for (ix, item) in self.items.iter().enumerate() {
            let heading = match item {
                Item::Snippet(_) if ix == 0 => Some(t!("picker.snippets")),
                Item::History(_) if Some(ix) == first_history => Some(t!(
                    "picker.history",
                    host = self
                        .history_owner
                        .as_ref()
                        .map(|(_, n)| n.to_string())
                        .unwrap_or_default()
                )),
                _ => None,
            };
            if let Some(heading) = heading {
                list = list.child(
                    div()
                        .px_2()
                        .pt_2()
                        .pb_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(heading),
                );
            }
            list = list.child(self.render_item(ix, item, cx));
        }
        let can_send = self.can_send(cx);
        let on_history = matches!(self.items.get(self.selected), Some(Item::History(_)));
        let extra = PickerKeys::of(cx.platform_info().os);
        v_flex()
            .id("command-picker")
            .test_support()
            .track_focus(&self.focus)
            .child(
                div()
                    .px_2()
                    .py_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(Input::new(&self.query).id("picker-input").appearance(false)),
            )
            .child(list)
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .px_3()
                        .pb_1()
                        .text_xs()
                        .text_color(theme.danger)
                        .child(prose(error)),
                )
            })
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_x_4()
                    .px_3()
                    .py_1p5()
                    .border_t_1()
                    .border_color(theme.border)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .when(can_send, |this| {
                        this.child(t!("picker.hint_send"))
                            .child(t!("picker.hint_insert"))
                    })
                    .when(!can_send, |this| {
                        this.child(
                            div()
                                .text_color(theme.warning)
                                .child(prose(t!("snippets.no_terminal"))),
                        )
                    })
                    .child(prose(t!(
                        "picker.hint_compose",
                        key = extra.label(extra.compose)
                    )))
                    .when(on_history, |this| {
                        this.child(prose(t!(
                            "picker.hint_history",
                            delete = extra.label(extra.delete),
                            save = extra.label(extra.save)
                        )))
                    }),
            )
            .into_any_element()
    }
}
