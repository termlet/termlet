//! 工作区停靠面板的外观：沿用 gpui-kit 的 `DockSkin`，只换掉 Tab 栏——去掉右侧的“…”菜单与它所在的工具区
//! （Tab 的菜单改为右键 Tab 打开，见各面板的 `title`）。
//!
//! gpui-kit 没有隐藏“…”的选项（0.8.1 仍然没有），这里实现自己的 `TabGroupRenderer`：除 Tab 栏以外的部分
//! （外框、内容区、拖动时的落点提示）都交给 gpui-kit 原来的渲染；Tab 栏的写法参照 gpui-component 0.7.3 的
//! `TabGroupSkin::render_tabs`（Apache-2.0；0.8.1 中只改了侧边停靠栏的开合按钮），保留点选、拖动排序与拖到别的
//! 分组，去掉了工作区用不到的侧边停靠栏的开合按钮与折叠状态。
//!
//! Tab 上另加了鼠标中键单击关闭（与点它的 × 相同，经工作区找到这个 Tab 的面板，确认照旧）。Tab 放不下时在 Tab 栏上
//! 滚动鼠标滚轮左右滚动 Tab 是 gpui-kit 的 `TabBar` 自带的（滚轮的竖向滚动换成横向，触控板的横向滚动照常）。
//! 窗口里只有一个 Tab 时停靠区不让拖动它，这里换成只能拖到另一个窗口上的载体（见 `workspace/move_tab.rs`）。
//!
//! Tab 放在左侧（竖排，见 `workspace/tab_list.rs`）时窗格顶上不画 Tab 栏，只画一条细标题（[`TitleOnly`]）：当前显示的
//! Tab 的标题（图标、名称、状态、关闭按钮，右键是 Tab 的菜单），拖动、中键关闭与横排的 Tab 相同。

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::base::ResizeHandleContext;
use gpui_kit::component::dock::{
    AnyDrag, BasePanelView, DockArea, DockAreaRenderer, DockContext, DockSkin, DragPanel,
    DropIndicator, NodeId, PanelHandle, PanelState, TabGroupContext, TabGroupRenderer,
};
use gpui_kit::component::h_flex;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, Selectable as _};
use gpui_kit::{
    AnyElement, AnyView, App, AppContext as _, Axis, Context, Div, Empty, Entity,
    InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Point, Render, ScrollHandle,
    SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, TestSupportExt as _,
    Window, div, prelude::FluentBuilder as _, rems, size,
};

/// 拖动 Tab 时跟着指针的预览的大小（rem，告诉停靠面板，落点提示从这里飞过去）。
const DRAG_PREVIEW_SIZE: (f32, f32) = (6., 1.875);

/// 窗格顶上只画一条细标题、不画横排的 Tab 栏（Tab 放在左侧时）：工作区每帧按设置写入，各分组画 Tab 栏时读。
#[derive(Clone, Default)]
pub struct TitleOnly(Rc<Cell<bool>>);

impl TitleOnly {
    pub fn get(&self) -> bool {
        self.0.get()
    }

    pub fn set(&self, on: bool) {
        self.0.set(on);
    }
}

/// 建一个用这套外观的停靠区域，同时返回 gpui-kit 的外观（改设置用，例如 `set_panel_style`）与窗格顶上是否只画细标题
/// 的开关。
pub fn dock_area(
    id: impl Into<SharedString>,
    version: Option<usize>,
    window: &mut Window,
    cx: &mut App,
) -> (Entity<DockArea>, Rc<DockSkin>, TitleOnly) {
    let mut skin = None;
    let title_only = TitleOnly::default();
    let area = cx.new(|cx| {
        let inner = DockSkin::new(cx);
        skin = Some(inner.clone());
        DockArea::new(id, version, window, cx).with_renderer(Rc::new(TermletDock {
            inner,
            title_only: title_only.clone(),
        }))
    });
    (
        area,
        skin.expect("DockSkin::new 在构造中执行过"),
        title_only,
    )
}

/// 停靠区域的外观：全部交给 gpui-kit，只换 Tab 分组的渲染。
struct TermletDock {
    inner: Rc<DockSkin>,
    title_only: TitleOnly,
}

impl DockAreaRenderer for TermletDock {
    fn frame(&self, window: &mut Window, cx: &mut App) -> Stateful<Div> {
        self.inner.frame(window, cx)
    }

    fn split_frame(
        &self,
        node: NodeId,
        axis: Axis,
        window: &mut Window,
        cx: &mut App,
    ) -> Stateful<Div> {
        self.inner.split_frame(node, axis, window, cx)
    }

    fn center_frame(&self, window: &mut Window, cx: &mut App) -> Stateful<Div> {
        self.inner.center_frame(window, cx)
    }

    fn render_split_handle(
        &self,
        handle: &ResizeHandleContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        self.inner.render_split_handle(handle, window, cx)
    }

    fn render_dock(
        &self,
        dock: &DockContext,
        content: AnyElement,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        self.inner.render_dock(dock, content, window, cx)
    }

    fn build_placeholder(
        &self,
        state: &PanelState,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Arc<dyn BasePanelView>> {
        self.inner.build_placeholder(state, window, cx)
    }

    fn tab_group_renderer(&self) -> Rc<dyn TabGroupRenderer> {
        Rc::new(TermletTabGroup {
            inner: self.inner.tab_group_renderer(),
            scroll: ScrollHandle::default(),
            last_active_ix: Rc::new(Cell::new(None)),
            title_only: self.title_only.clone(),
        })
    }
}

/// 一个 Tab 分组的外观：Tab 栏自己画（没有“…”），其余交给 gpui-kit。
struct TermletTabGroup {
    inner: Rc<dyn TabGroupRenderer>,
    scroll: ScrollHandle,
    /// 上次画出时选中的 Tab：变了才把它滚进视野（点选的不滚，免得跳动）
    last_active_ix: Rc<Cell<Option<usize>>>,
    title_only: TitleOnly,
}

impl TermletTabGroup {
    /// Tab 放在左侧时窗格顶上的细标题（见模块说明）。别的窗格的 Tab 可以拖到这里，放进这个窗格。
    fn render_pane_title(
        &self,
        group: &TabGroupContext,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let Some(panel) = group.active_panel().cloned() else {
            return Empty.into_any_element();
        };
        let panel_id = panel.panel_id(cx);
        let visible = group.panels().iter().filter(|p| p.visible(cx)).count();
        let drag = group
            .panels()
            .iter()
            .position(|p| p.panel_id(cx) == panel_id)
            .filter(|_| group.is_draggable())
            .and_then(|ix| group.drag_panel(ix, cx));
        let sole = !group.is_draggable() && !group.is_locked() && visible == 1;
        let droppable = group.is_droppable();
        let theme = cx.theme();
        h_flex()
            .id("pane-title")
            .test_support()
            .flex_none()
            .w_full()
            .min_w_0()
            .overflow_hidden()
            .px_2()
            .py_0p5()
            .text_sm()
            .text_color(theme.tab_active_foreground)
            .bg(theme.tab_bar)
            .border_b_1()
            .border_color(theme.border)
            .child(panel_title(&panel, window, cx))
            .on_aux_click(move |event, window, cx| {
                if event.is_middle_click() {
                    cx.stop_propagation();
                    close_tab(panel_id, window, cx);
                }
            })
            .when_some(drag, |this, drag| {
                this.on_drag(drag, start_tab_drag(panel.clone()))
            })
            .when(sole, |this| {
                this.on_drag(
                    crate::workspace::SoleTabDrag::new(panel_id),
                    start_sole_tab_drag(panel.clone()),
                )
            })
            .when(droppable, |this| {
                this.drag_over::<DragPanel>(|this, _, _, cx| this.bg(cx.theme().tokens.drop_target))
                    .on_drop({
                        let group = group.clone();
                        move |drag: &DragPanel, window, cx| {
                            group.drop_panel(drag.clone(), None, true, window, cx);
                        }
                    })
            })
            .into_any_element()
    }
}

/// 鼠标中键单击 Tab：关闭它，与点它的 × 相同（经工作区找到这个 Tab 的面板，确认照旧）。
fn close_tab(panel: gpui_kit::component::dock::PanelId, window: &mut Window, cx: &mut App) {
    if let Some(workspace) = crate::Workspace::in_window(window, cx) {
        workspace.update(cx, |workspace, cx| {
            workspace.request_close_tab(panel, window, cx)
        });
    }
}

/// 开始拖动 Tab（`on_drag` 的构造）：记下指针在 Tab 中的位置与预览的大小（停靠区的落点提示从预览处飞过去），
/// 跟着指针的是 Tab 的标题。横排的 Tab、细标题与竖排的 Tab 列表共用。
pub(crate) fn start_tab_drag(
    panel: Arc<dyn BasePanelView>,
) -> impl Fn(&DragPanel, Point<Pixels>, &mut Window, &mut App) -> Entity<TabDragPreview> + 'static {
    move |drag, offset, window, cx| {
        cx.stop_propagation();
        drag.set_drag_offset(offset);
        let rem = window.rem_size();
        let (w, h) = DRAG_PREVIEW_SIZE;
        drag.set_preview_size(size(rems(w).to_pixels(rem), rems(h).to_pixels(rem)));
        cx.new(|_| TabDragPreview {
            panel: panel.clone(),
        })
    }
}

/// 开始拖动窗口里唯一的 Tab（只能拖到另一个窗口上，见 `workspace/move_tab.rs`）。
pub(crate) fn start_sole_tab_drag(
    panel: Arc<dyn BasePanelView>,
) -> impl Fn(
    &crate::workspace::SoleTabDrag,
    Point<Pixels>,
    &mut Window,
    &mut App,
) -> Entity<TabDragPreview>
+ 'static {
    move |_, _, _, cx| {
        cx.stop_propagation();
        cx.new(|_| TabDragPreview {
            panel: panel.clone(),
        })
    }
}

impl TabGroupRenderer for TermletTabGroup {
    fn frame(&self, group: &TabGroupContext, window: &mut Window, cx: &mut App) -> Stateful<Div> {
        self.inner.frame(group, window, cx)
    }

    fn content_frame(
        &self,
        group: &TabGroupContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Stateful<Div> {
        self.inner.content_frame(group, window, cx)
    }

    fn render_active_panel(
        &self,
        panel: AnyView,
        group: &TabGroupContext,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        self.inner.render_active_panel(panel, group, window, cx)
    }

    fn render_drop_indicator(
        &self,
        indicator: DropIndicator,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        self.inner.render_drop_indicator(indicator, window, cx)
    }

    fn render_empty(
        &self,
        group: &TabGroupContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        self.inner.render_empty(group, window, cx)
    }

    fn render_tab_bar(
        &self,
        group: &TabGroupContext,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let visible: Vec<usize> = group
            .panels()
            .iter()
            .enumerate()
            .filter(|(_, panel)| panel.visible(cx))
            .map(|(ix, _)| ix)
            .collect();
        if visible.is_empty() {
            return Empty.into_any_element();
        }
        if self.title_only.get() {
            // 横排时再滚进视野
            self.last_active_ix.set(None);
            return self.render_pane_title(group, window, cx);
        }
        let droppable = group.is_droppable();
        // 窗口里唯一的 Tab：停靠区不让拖动它，换成只能拖到另一个窗口上的载体（见 workspace 的 move_tab）
        let sole = !group.is_draggable() && !group.is_locked() && visible.len() == 1;
        let tabs_count = group.panels().len();
        let active_ix = group.active_ix();
        let displayed = group.active_panel().map(|panel| panel.panel_id(cx));
        let displayed_ix = displayed.and_then(|displayed| {
            group
                .panels()
                .iter()
                .position(|panel| panel.panel_id(cx) == displayed)
        });
        // 新显示的 Tab 滚进视野
        if self.last_active_ix.replace(Some(active_ix)) != Some(active_ix)
            && let Some(position) = visible.iter().position(|ix| *ix == active_ix)
        {
            self.scroll.scroll_to_item(position);
        }

        let tabs = visible.into_iter().map(|ix| {
            let panel = &group.panels()[ix];
            let drag = group
                .is_draggable()
                .then(|| group.drag_panel(ix, cx))
                .flatten();
            let panel_id = panel.panel_id(cx);
            Tab::new()
                .child(panel_title(panel, window, cx))
                .selected(Some(ix) == displayed_ix)
                // 鼠标中键单击：关闭这个 Tab，与点它的 × 相同（右键由标题的菜单处理）
                .on_aux_click(move |event, window, cx| {
                    if !event.is_middle_click() {
                        return;
                    }
                    cx.stop_propagation();
                    close_tab(panel_id, window, cx);
                })
                .on_click({
                    let group = group.clone();
                    let last_active_ix = self.last_active_ix.clone();
                    move |_, window, cx| {
                        last_active_ix.set(Some(ix));
                        group.select_tab(ix, window, cx);
                    }
                })
                .when_some(drag, |this, drag| {
                    this.on_drag(drag, start_tab_drag(panel.clone()))
                })
                .when(sole, |this| {
                    this.on_drag(
                        crate::workspace::SoleTabDrag::new(panel_id),
                        start_sole_tab_drag(panel.clone()),
                    )
                })
                .when(droppable, |this| {
                    this.drag_over::<DragPanel>(|this, _, _, cx| {
                        this.rounded_l_none()
                            .border_l_2()
                            .border_r_0()
                            .border_color(cx.theme().drag_border)
                    })
                    .on_drop({
                        let group = group.clone();
                        move |drag: &DragPanel, window, cx| {
                            group.drop_panel(drag.clone(), Some(ix), true, window, cx);
                        }
                    })
                    .drag_over::<AnyDrag>(|this, _, _, cx| {
                        this.rounded_l_none()
                            .border_l_2()
                            .border_r_0()
                            .border_color(cx.theme().drag_border)
                    })
                    .on_drop({
                        let group = group.clone();
                        move |item: &AnyDrag, window, cx| {
                            group.drop_item(item.clone(), None, window, cx);
                        }
                    })
                })
        });

        TabBar::new("tab-bar")
            .track_scroll(&self.scroll)
            .children(tabs)
            // 最后一个 Tab 后面的空白：可以把 Tab 拖到最后（TabBar 只在有后缀时才画它，后缀是空的）
            .last_empty_space(
                div()
                    .id("tab-bar-empty-space")
                    .h_full()
                    .flex_grow_1()
                    .min_w_16()
                    .when(droppable, |this| {
                        this.drag_over::<DragPanel>(|this, _, _, cx| {
                            this.bg(cx.theme().tokens.drop_target)
                        })
                        .on_drop({
                            let group = group.clone();
                            let node = group.node();
                            move |drag: &DragPanel, window, cx| {
                                // 拖到自己分组的最后：放进最后一格；别的分组拖来的追加在后面
                                let ix = (drag.source() == node).then(|| tabs_count - 1);
                                group.drop_panel(drag.clone(), ix, false, window, cx);
                            }
                        })
                        .drag_over::<AnyDrag>(|this, _, _, cx| {
                            this.bg(cx.theme().tokens.drop_target)
                        })
                        .on_drop({
                            let group = group.clone();
                            move |item: &AnyDrag, window, cx| {
                                group.drop_item(item.clone(), None, window, cx);
                            }
                        })
                    }),
            )
            .suffix(div())
            .into_any_element()
    }
}

/// 面板的标题；不是经 gpui-kit 包装的面板时用它的名字。
fn panel_title(panel: &Arc<dyn BasePanelView>, window: &mut Window, cx: &mut App) -> AnyElement {
    match PanelHandle::of(panel) {
        Some(handle) => handle.title(window, cx),
        None => SharedString::from(panel.panel_name(cx)).into_any_element(),
    }
}

/// 拖动 Tab 时跟着指针的预览。
pub(crate) struct TabDragPreview {
    panel: Arc<dyn BasePanelView>,
}

impl Render for TabDragPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("drag-panel")
            .cursor_grab()
            .py_1()
            .px_3()
            .w_24()
            .overflow_hidden()
            .whitespace_nowrap()
            .border_1()
            .border_color(cx.theme().border)
            .rounded(cx.theme().radius)
            .text_color(cx.theme().tab_foreground)
            .bg(cx.theme().tokens.tab_active)
            .opacity(0.75)
            .child(panel_title(&self.panel, window, cx))
    }
}
