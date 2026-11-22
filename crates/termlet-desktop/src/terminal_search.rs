//! 终端的查找栏：浮在终端右上角，Ctrl+Shift+F 打开，Enter / Shift+Enter 跳到下一处 / 上一处，Esc 关闭。
//! 输入停下一会儿再查找：回滚很大时每输入一个字都查一遍会卡（查找在界面线程上进行）。

use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Selectable as _, Sizable as _, h_flex};
use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Render, Styled as _,
    Subscription, Task, Window, actions, div, prelude::FluentBuilder as _,
};
use termlet_terminal::{SearchQuery, SearchSummary, TerminalView};

use crate::i18n::LocalizedPlaceholder as _;
use crate::prose::prose;
use crate::t;

actions!(terminal_search, [OpenSearch, CloseSearch]);

pub(crate) const KEY_CONTEXT: &str = "TerminalSearch";

/// 输入后等这么久再查找。
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(150);

pub fn init(cx: &mut gpui_kit::App) {
    cx.bind_keys(key_bindings(cx.platform_info().os));
}

/// 查找栏固定的快捷键（各系统相同）；打开查找栏的快捷键可以自定义，由 crate::keymap 注册。
pub(crate) fn key_bindings(_: gpui_kit::OperatingSystem) -> Vec<KeyBinding> {
    vec![KeyBinding::new("escape", CloseSearch, Some(KEY_CONTEXT))]
}

/// 查找栏发给终端面板的事件。
pub enum SearchBarEvent {
    /// 关闭查找栏，焦点回到终端
    Closed,
}

pub struct SearchBar {
    terminal: Entity<TerminalView>,
    input: Entity<InputState>,
    case_sensitive: bool,
    regex: bool,
    whole_word: bool,
    summary: SearchSummary,
    /// 正则不合法
    invalid: bool,
    /// 等输入停下再查找
    pending: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SearchBarEvent> for SearchBar {}

impl Focusable for SearchBar {
    fn focus_handle(&self, cx: &gpui_kit::App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl SearchBar {
    pub fn new(
        terminal: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx).localized_placeholder(
                || t!("terminal.search.placeholder"),
                window,
                cx,
            )
        });
        // 再次打开时沿用上次的查找条件
        let previous = terminal.read(cx).search_query().cloned();
        if let Some(previous) = &previous {
            input.update(cx, |input, cx| {
                input.set_value(previous.text.clone(), window, cx)
            });
        }
        let subscriptions =
            vec![
                cx.subscribe_in(&input, window, |this, _, event, _, cx| match event {
                    InputEvent::Change => this.schedule_search(cx),
                    InputEvent::PressEnter { shift, .. } => {
                        // 输入后马上按 Enter：先按最新的内容查找
                        if this.pending.is_some() {
                            this.update_search(cx);
                        }
                        this.step(!*shift, cx)
                    }
                    _ => {}
                }),
            ];
        let mut this = Self {
            terminal,
            input,
            case_sensitive: previous.as_ref().is_some_and(|q| q.case_sensitive),
            regex: previous.as_ref().is_some_and(|q| q.regex),
            whole_word: previous.as_ref().is_some_and(|q| q.whole_word),
            summary: SearchSummary::default(),
            invalid: false,
            pending: None,
            _subscriptions: subscriptions,
        };
        this.update_search(cx);
        this
    }

    fn query(&self, cx: &gpui_kit::App) -> SearchQuery {
        SearchQuery {
            text: self.input.read(cx).value().to_string(),
            case_sensitive: self.case_sensitive,
            regex: self.regex,
            whole_word: self.whole_word,
        }
    }

    fn schedule_search(&mut self, cx: &mut Context<Self>) {
        self.pending = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| this.update_search(cx));
        }));
    }

    fn update_search(&mut self, cx: &mut Context<Self>) {
        self.pending = None;
        let query = self.query(cx);
        let result = self
            .terminal
            .update(cx, |terminal, cx| terminal.set_search(Some(query), cx));
        match result {
            Ok(summary) => {
                self.summary = summary;
                self.invalid = false;
            }
            Err(_) => self.invalid = true,
        }
        cx.notify();
    }

    fn step(&mut self, forward: bool, cx: &mut Context<Self>) {
        self.summary = self
            .terminal
            .update(cx, |terminal, cx| terminal.search_step(forward, cx));
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.pending = None;
        self.terminal
            .update(cx, |terminal, cx| terminal.set_search(None, cx).ok());
        cx.emit(SearchBarEvent::Closed);
    }

    fn toggle(&mut self, option: fn(&mut Self) -> &mut bool, cx: &mut Context<Self>) {
        let value = option(self);
        *value = !*value;
        self.update_search(cx);
    }

    fn render_count(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let text = if self.invalid {
            t!("terminal.search.invalid")
        } else if self.input.read(cx).value().is_empty() {
            Default::default()
        } else if self.summary.total == 0 {
            t!("terminal.search.no_results")
        } else {
            t!(
                "terminal.search.count",
                current = self.summary.current,
                total = self.summary.total
            )
        };
        div()
            .text_xs()
            .min_w_16()
            .text_color(if self.invalid {
                cx.theme().danger
            } else {
                cx.theme().muted_foreground
            })
            .child(prose(text))
    }
}

impl Render for SearchBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 输入框、终端的右键菜单开着时按钮不挂悬停提示（按钮的提示只在按下左键时收起，会浮在菜单上，见
        // crate::menu_tooltip）
        let menu_open = crate::menu_tooltip::menu_open(window, cx);
        let toggle = |id: &'static str, icon: IconName, on: bool, tooltip| {
            Button::new(id)
                .ghost()
                .small()
                .icon(icon)
                .selected(on)
                .when(!menu_open, |this| this.tooltip(tooltip))
        };
        h_flex()
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(|this, _: &CloseSearch, _, cx| this.close(cx)))
            .gap_1()
            .p_1()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .shadow_md()
            .child(
                div()
                    .w_56()
                    .child(Input::new(&self.input).small().appearance(false)),
            )
            .child(self.render_count(cx))
            .child(
                toggle(
                    "search-case",
                    IconName::CaseSensitive,
                    self.case_sensitive,
                    t!("terminal.search.case_sensitive"),
                )
                .on_click(cx.listener(|this, _, _, cx| this.toggle(|s| &mut s.case_sensitive, cx))),
            )
            .child(
                toggle(
                    "search-word",
                    IconName::WholeWord,
                    self.whole_word,
                    t!("terminal.search.whole_word"),
                )
                .on_click(cx.listener(|this, _, _, cx| this.toggle(|s| &mut s.whole_word, cx))),
            )
            .child(
                toggle(
                    "search-regex",
                    IconName::Regex,
                    self.regex,
                    t!("terminal.search.regex"),
                )
                .on_click(cx.listener(|this, _, _, cx| this.toggle(|s| &mut s.regex, cx))),
            )
            .child(
                Button::new("search-previous")
                    .ghost()
                    .small()
                    .icon(IconName::ChevronUp)
                    .when(!menu_open, |this| {
                        this.tooltip(t!("terminal.search.previous"))
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.step(false, cx))),
            )
            .child(
                Button::new("search-next")
                    .ghost()
                    .small()
                    .icon(IconName::ChevronDown)
                    .when(!menu_open, |this| this.tooltip(t!("terminal.search.next")))
                    .on_click(cx.listener(|this, _, _, cx| this.step(true, cx))),
            )
            .child(
                Button::new("search-close")
                    .ghost()
                    .small()
                    .icon(IconName::Close)
                    .when(!menu_open, |this| this.tooltip(t!("terminal.search.close")))
                    .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
            )
    }
}
