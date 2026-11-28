//! 快捷命令栏：终端区域下方的一行按钮（WindTerm 的 Quick Bar、Tabby 下载最多的快捷命令插件），单击把快捷命令
//! 发到当前终端（与侧边栏的单击相同：带参数的先填写参数，按保存时的设置执行或只写入）。
//!
//! 默认不显示；命令面板的“显示快捷命令栏”、快捷命令页的按钮或栏上的 × 切换，开关与选中的分组记在本机的
//! 界面状态中。有分组时最前面是分组的选择（全部、未分组、各分组）。按钮太多时横向滚动，鼠标滚轮也能左右滚（与 Tab 栏
//! 相同，竖向的滚动换成横向）。

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableMask;
use gpui_kit::component::{ActiveTheme as _, InteractiveElementExt as _, Sizable as _, h_flex};
use gpui_kit::{
    App, Context, EventEmitter, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    ScrollHandle, SharedString, Styled as _, Subscription, TestSupportExt as _, Window, div,
    prelude::FluentBuilder as _, rems,
};
use termlet_core::id::SnippetId;
use termlet_core::model::{Kind, Snippet};
use termlet_core::store::{Record, StoreEvent};

use crate::app_state::AppState;
use crate::prose::prose;
use crate::scrolling::TrackedScroll as _;
use crate::t;

/// 快捷命令栏发给工作区的请求。
pub enum SnippetBarEvent {
    /// 发送到当前终端（按保存时的设置执行或只写入）
    Send(SnippetId),
    /// 打开侧边栏的“快捷命令”页（还没有快捷命令时的“添加”）
    OpenList,
}

/// 显示快捷命令栏。
pub fn is_open(cx: &App) -> bool {
    AppState::global(cx).ui.read(cx).state().snippet_bar_open
}

/// 显示 / 隐藏快捷命令栏。
pub fn toggle(cx: &mut App) {
    let open = !is_open(cx);
    tracing::debug!(open, "快捷命令栏");
    AppState::ui(cx).update(cx, |ui, cx| ui.update(|s| s.snippet_bar_open = open, cx));
}

pub struct SnippetBar {
    snippets: Vec<Record<Snippet>>,
    /// 按钮那一条的横向滚动
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SnippetBarEvent> for SnippetBar {}

impl SnippetBar {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let store = AppState::global(cx).store.clone();
        let ui = AppState::ui(cx);
        let subscriptions = vec![
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
            snippets: Vec::new(),
            scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        };
        this.reload(cx);
        this
    }

    /// 与侧边栏的顺序相同：未分组在前，分组按名称，分组内按保存的顺序（排序是稳定的）。
    fn reload(&mut self, cx: &mut Context<Self>) {
        let mut snippets = AppState::store(cx).list::<Snippet>();
        snippets.sort_by(|a, b| a.doc.group.cmp(&b.doc.group));
        self.snippets = snippets;
        cx.notify();
    }

    /// 选中的分组：界面状态中记下的分组已经不存在时为全部。
    fn group(&self, cx: &App) -> Option<String> {
        AppState::global(cx)
            .ui
            .read(cx)
            .state()
            .snippet_bar_group
            .clone()
            .filter(|g| self.snippets.iter().any(|s| &s.doc.group == g))
    }

    /// 分组名，按名称排序（未分组在前，与侧边栏相同）。
    fn groups(&self) -> Vec<String> {
        let mut groups: Vec<String> = self.snippets.iter().map(|s| s.doc.group.clone()).collect();
        groups.sort();
        groups.dedup();
        groups
    }

    fn render_group_menu(&self, groups: Vec<String>, current: Option<String>) -> impl IntoElement {
        let label: SharedString = match &current {
            None => t!("snippets.bar.all"),
            Some(g) if g.is_empty() => t!("snippets.ungrouped"),
            Some(g) => g.clone().into(),
        };
        Button::new("snippet-bar-group")
            .ghost()
            .xsmall()
            .label(label)
            .dropdown_caret(true)
            .tooltip(t!("snippets.bar.group"))
            .dropdown_menu(move |menu, _, _| {
                let pick = |group: Option<String>| {
                    move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| {
                        let group = group.clone();
                        AppState::ui(cx)
                            .update(cx, |ui, cx| ui.update(|s| s.snippet_bar_group = group, cx));
                    }
                };
                let menu = menu.item(
                    PopupMenuItem::new(t!("snippets.bar.all"))
                        .checked(current.is_none())
                        .on_click(pick(None)),
                );
                groups.iter().fold(menu, |menu, group| {
                    let name: SharedString = if group.is_empty() {
                        t!("snippets.ungrouped")
                    } else {
                        group.clone().into()
                    };
                    menu.item(
                        PopupMenuItem::new(name)
                            .checked(current.as_ref() == Some(group))
                            .on_click(pick(Some(group.clone()))),
                    )
                })
            })
    }
}

impl Render for SnippetBar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (border, background, muted) = (theme.border, theme.secondary, theme.muted_foreground);
        let groups = self.groups();
        let current = self.group(cx);
        let items = self
            .snippets
            .iter()
            .filter(|s| current.as_ref().is_none_or(|g| &s.doc.group == g))
            .map(|snippet| {
                let id = snippet.id.clone();
                let preview: SharedString = snippet.doc.content.trim_end().to_string().into();
                Button::new(SharedString::from(format!("snippet-bar-{}", snippet.id)))
                    .outline()
                    .xsmall()
                    .flex_none()
                    .label(snippet.doc.name.clone())
                    .tooltip(preview)
                    .on_click(
                        cx.listener(move |_, _, _, cx| cx.emit(SnippetBarEvent::Send(id.clone()))),
                    )
            })
            .collect::<Vec<_>>();
        let empty = self.snippets.is_empty();
        h_flex()
            .id("snippet-bar")
            .test_support()
            .flex_none()
            .w_full()
            .h(rems(2.25))
            .px_2()
            .gap_1()
            .border_t_1()
            .border_color(border)
            .bg(background)
            // 有不止一个分组（或有分组的快捷命令）时才需要选择
            .when(groups.iter().any(|g| !g.is_empty()), |this| {
                this.child(self.render_group_menu(groups, current))
            })
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(
                        h_flex()
                            .id("snippet-bar-items")
                            .tracked_x(&self.scroll)
                            .lock_scroll_axis()
                            .test_support()
                            .size_full()
                            .gap_1()
                            .when(empty, |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(prose(t!("snippets.bar.empty"))),
                                )
                                .child(
                                    Button::new("snippet-bar-add")
                                        .ghost()
                                        .xsmall()
                                        .icon(IconName::Plus)
                                        .label(t!("snippets.bar.add"))
                                        .on_click(cx.listener(|_, _, _, cx| {
                                            cx.emit(SnippetBarEvent::OpenList)
                                        })),
                                )
                            })
                            .children(items),
                    )
                    // 鼠标滚轮的竖向滚动换成横向（gpui-kit 的 Tab 栏也是这样做的）
                    .child(ScrollableMask::vertical_wheel(&self.scroll)),
            )
            .child(
                Button::new("snippet-bar-close")
                    .ghost()
                    .small()
                    .icon(IconName::Close)
                    .tooltip(t!("snippets.bar.hide"))
                    .on_click(|_, _, cx| toggle(cx)),
            )
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AppContext as _, Bounds, ScrollDelta, TestAppContext, WindowBounds, WindowOptions, point,
        px, size,
    };
    use termlet_core::store::Position;

    use super::*;

    // 按钮放不下时，鼠标滚轮（只有竖向）把这一条向左滚：最后一个按钮滚进视野，完整露出
    #[gpui_kit::test]
    fn wheel_scrolls_the_bar_sideways(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        cx.update(crate::init);
        cx.update(AppState::install_in_memory);
        let last = cx.update(|cx| {
            let store = AppState::store(cx);
            (0..20)
                .map(|ix| {
                    store
                        .create(
                            Snippet {
                                name: format!("快捷命令 {ix:02}"),
                                content: format!("echo {ix}"),
                                ..Default::default()
                            },
                            None,
                            Position::Last,
                        )
                        .unwrap()
                })
                .collect::<Vec<_>>()
                .pop()
                .unwrap()
        });
        let (handle, _) = cx
            .update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                            Default::default(),
                            size(px(480.), px(120.)),
                        ))),
                        ..Default::default()
                    },
                    cx,
                    |_, cx| cx.new(SnippetBar::new),
                )
            })
            .unwrap();
        cx.run_until_parked();
        let last = SharedString::from(format!("snippet-bar-{last}"));
        let (strip, before) = cx
            .update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                (
                    window.find("snippet-bar-items").bounds(),
                    window.find(last.clone()).bounds(),
                )
            })
            .unwrap();
        assert!(before.right() > strip.right(), "放不下：最后一个在右边外面");
        cx.update_window(handle, |_, window, cx| {
            window.scroll(
                "snippet-bar-items",
                ScrollDelta::Lines(point(0., -100.)),
                cx,
            );
        })
        .unwrap();
        cx.run_until_parked();
        let after = cx
            .update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.find(last.clone()).bounds()
            })
            .unwrap();
        assert!(
            after.left() >= strip.left() && after.right() <= strip.right(),
            "滚到最后：最后一个完整露出（{after:?}，可见部分 {strip:?}）"
        );
    }
}
