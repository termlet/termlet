//! 日志查看器：中间区域的一个 Tab（“日志 · demo-app”），服务与容器的日志共用，与编辑器、SFTP 的 Tab 一样可以切换、
//! 分屏、关闭。
//!
//! - 读取在打开它的终端的连接上进行（每次取终端当前的连接，重连后照常读；终端关了时说明，不再读）。读的行数
//!   （200 / 1000 / 全部）、时间范围、服务的级别在服务器上筛选，改了就重新读；关键词在本机筛选，命中处标出。
//! - 正文是只读的编辑器：可以选中、复制（Ctrl+C、右键菜单）、Ctrl+F 查找；错误、警告行着色，时间列（本机时区的短格式）
//!   可以不显示；长行默认自动换行，关掉时横向滚动。
//! - 工具条按宽度收起次要项：窄的时候“跟随”只留图标，复制全部、在新 Tab 中跟随收进 ⋯，不折成两行。
//! - 跟随：Tab 显示着时每 [`FOLLOW_INTERVAL`] 从上次读到的位置增量读取（上一次返回后再计时），新行到达时滚到底部；
//!   用户往上翻时立即提示并不再自动滚动，滚回底部（或点“回到底部”）时恢复。Tab 不显示、关闭时停止。
//! - “在新 Tab 中跟随日志”（在终端里跑 `journalctl -f` / `docker logs -f`）保留，是否带 sudo 与这里读的一致。

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dock::{BasePanel, Panel, PanelEvent};
use gpui_kit::component::input::{
    self, Editor, EditorState, Input, InputEvent, InputState, RopeExt as _, TextDecoration,
    TextDecorationCollection,
};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, Theme, WindowExt as _,
    h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, ClickEvent, ClipboardItem, Context, Entity, EntityId, EventEmitter,
    FocusHandle, Focusable, HighlightStyle, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, Point, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, TestSupportExt as _, WeakEntity, Window, canvas, div, point,
    prelude::FluentBuilder as _, px, rems,
};
use termlet_core::hosttools::{
    HostTools, LogCursor, LogLevel, LogLine, LogPriority, LogQuery, LogResult, LogSource, Target,
    TerminalRequest, ToolsError, terminal_command,
};

use super::errors;
use super::log_model::{Filter, LinesChoice, LogDisplay, RangeChoice, append_lines, build_display};
use crate::i18n::LocalizedPlaceholder as _;
use crate::prose::prose;
use crate::t;
use crate::terminal_panel::TerminalPanel;

/// 面板在持久化布局中的名称，发布后不能改。
const PANEL_NAME: &str = "LogPanel";

/// 跟随时增量读取的间隔（上一次返回后再计时）。
pub const FOLLOW_INTERVAL: Duration = Duration::from_secs(2);
/// 工具条至少这么宽（rem）时全部展开；窄的时候“跟随”只留图标，复制全部、在新 Tab 中跟随收进 ⋯；更窄的时候显示时间、
/// 自动换行两个开关也收进 ⋯（作为可勾选的菜单项）。
const TOOLBAR_FULL: f32 = 46.;
const TOOLBAR_NARROW: f32 = 36.;

/// 打开日志查看器的请求（主机工具面板发给工作区）。
#[derive(Clone)]
pub struct LogTab {
    /// 打开它的终端：在它的连接上读取，在它旁边的新 Tab 中跟随
    pub terminal: WeakEntity<TerminalPanel>,
    pub terminal_id: EntityId,
    /// 终端连的主机（Tab 的悬停提示）
    pub host: SharedString,
    /// 服务名（去掉 `.service`）或容器名（Tab 的标题）
    pub name: String,
    pub source: LogSource,
}

/// 日志查看器请工作区做的事。
pub enum LogPanelEvent {
    /// 关闭这个 Tab
    Close,
    /// 在共用 `terminal` 的连接的新 Tab 中执行 `command`（在终端中跟随日志）
    RunInTerminal {
        terminal: WeakEntity<TerminalPanel>,
        command: String,
        title: SharedString,
    },
}

/// 读不到时的原因。
enum LoadError {
    Tools(ToolsError),
    /// 打开它的终端已关闭
    SourceGone,
}

/// 新的内容显示时视图怎样动。
#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    /// 滚到底部（重新读取、跟随中且在底部时）
    Bottom,
    /// 保持滚动位置与选中的文字（往上翻着时来了新行）
    Keep,
}

/// 在新的终端中跟随日志的命令：服务的日志经 sudo 读到时跟随也用 sudo；容器按 Docker 页是否经 sudo 访问。
pub(super) fn follow_request(source: &LogSource, read_with_sudo: bool) -> TerminalRequest {
    match source {
        LogSource::Service { unit } => TerminalRequest::ServiceLogs {
            unit: unit.clone(),
            sudo: read_with_sudo,
        },
        LogSource::Container { engine, id, sudo } => TerminalRequest::ContainerLogs {
            engine: *engine,
            id: id.clone(),
            sudo: *sudo,
        },
    }
}

pub struct LogPanel {
    tab: LogTab,
    lines_choice: LinesChoice,
    range: RangeChoice,
    priority: LogPriority,
    show_time: bool,
    /// 长行自动换行（关掉时横向滚动）
    wrap: bool,
    follow: bool,
    filter: Entity<InputState>,
    /// 上一帧工具条的宽度（窄的时候收起次要项）
    toolbar_width: Rc<Cell<Pixels>>,
    editor: Entity<EditorState>,
    /// 错误、警告行与时间列的颜色
    level_marks: TextDecorationCollection,
    /// 关键词命中的位置
    match_marks: TextDecorationCollection,
    /// 读到的全部行（跟随时最多 [`super::log_model::MAX_KEPT`] 行）
    lines: Vec<LogLine>,
    /// 正在显示的内容（重新着色、复制全部用）
    display: LogDisplay,
    cursor: Option<LogCursor>,
    sudo: bool,
    limited: bool,
    truncated: bool,
    error: Option<LoadError>,
    /// 重新读取中（跟随的增量读取不算）
    loading: bool,
    /// 读到过一次
    loaded: bool,
    updated_at: Option<i64>,
    /// 停在底部（新行到达时滚到底）：随编辑器的滚动位置更新（往上翻时立即不是，滚回底部时立即又是）
    pinned: bool,
    /// 刚要求滚到底部、编辑器还没有排好时的滚动位置（换内容时临时回到开头）：滚动位置还是它时不算用户翻动。编辑器
    /// 排好时改滚动位置不一定通知，下一次通知时滚动位置已经不同，就照常跟踪
    settling: Option<Point<Pixels>>,
    /// Tab 显示着
    active: bool,
    /// 每次读取加一：作废进行中的读取
    seq: u64,
    _load: Option<Task<()>>,
    _follow: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PanelEvent> for LogPanel {}
impl EventEmitter<LogPanelEvent> for LogPanel {}

impl Focusable for LogPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl LogPanel {
    /// 打开日志查看器并读取一次（默认最近 200 行、跟随）。
    pub fn open(tab: LogTab, window: &mut Window, cx: &mut App) -> Entity<Self> {
        cx.new(|cx: &mut Context<Self>| {
            let filter = cx.new(|cx| {
                InputState::new(window, cx)
                    .localized_placeholder(|| t!("tools.logs.filter"), window, cx)
                    .clean_on_escape()
            });
            let editor = cx.new(|cx| {
                EditorState::new(window, cx)
                    .language("text")
                    .line_number(false)
                    .folding(false)
                    .indent_guides(false)
                    .soft_wrap(true)
                    .scroll_beyond_last_line(Some(0))
                    .placeholder(t!("tools.loading"))
            });
            let (level_marks, match_marks) = editor.update(cx, |editor, cx| {
                (
                    editor.create_decorations_collection(Vec::new(), cx),
                    editor.create_decorations_collection(Vec::new(), cx),
                )
            });
            let subscriptions = Self::subscribe(&filter, &editor, window, cx);
            let mut this = Self {
                tab,
                lines_choice: LinesChoice::default(),
                range: RangeChoice::default(),
                priority: LogPriority::All,
                show_time: true,
                wrap: true,
                follow: true,
                filter,
                toolbar_width: Rc::new(Cell::new(px(0.))),
                editor,
                level_marks,
                match_marks,
                lines: Vec::new(),
                display: LogDisplay::default(),
                cursor: None,
                sudo: false,
                limited: false,
                truncated: false,
                error: None,
                loading: false,
                loaded: false,
                updated_at: None,
                pinned: true,
                settling: None,
                active: false,
                seq: 0,
                _load: None,
                _follow: None,
                _subscriptions: subscriptions,
            };
            this.reload(window, cx);
            this
        })
    }

    /// 对筛选框、编辑区与主题的订阅（与所在的窗口绑定）。
    fn subscribe(
        filter: &Entity<InputState>,
        editor: &Entity<EditorState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Subscription> {
        vec![
            cx.subscribe_in(filter, window, |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.show(this.follow_view(), false, window, cx);
                }
            }),
            // 滚动位置变了（滚轮、拖滚动条、键盘）：立即更新是否停在底部
            cx.observe(editor, |this, _, cx| this.track_scroll(cx)),
            // 换了主题：按新主题的颜色重新着色
            cx.observe_global_in::<Theme>(window, |this, _, cx| this.apply_marks(cx)),
        ]
    }

    /// Tab 移到别的窗口（见 `workspace/move_tab.rs`）：读到的内容、筛选与选项都不变；订阅在新窗口中重新登记，编辑区
    /// 获得焦点时开始闪烁光标（编辑区自己的处理登记在打开它的窗口上）。从原来的停靠区取下时停掉的读取（见
    /// `on_removed`）在这里重新开始，跟随在新窗口中显示时接着进行。
    pub(crate) fn rebind_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut subscriptions = Self::subscribe(&self.filter, &self.editor, window, cx);
        let editor = self.editor.clone();
        let focus = editor.focus_handle(cx);
        subscriptions.push(cx.on_focus(&focus, window, move |_, window, cx| {
            editor.update(cx, |editor, cx| editor.focus(window, cx));
        }));
        self._subscriptions = subscriptions;
        if self.loading {
            self.reload(window, cx);
        }
        cx.notify();
    }

    /// 是不是这个终端、这个来源的日志（同一个已打开时工作区切过去，不再新开）。
    pub fn is_for(&self, terminal: EntityId, source: &LogSource) -> bool {
        self.tab.terminal_id == terminal && self.tab.source == *source
    }

    /// 不再确认、直接关闭（一次关闭几个 Tab 时）。
    pub(crate) fn close_tab(&mut self, cx: &mut Context<Self>) {
        cx.emit(LogPanelEvent::Close);
    }

    pub fn title_text(&self) -> SharedString {
        t!("tools.logs.title", name = self.tab.name.clone())
    }

    // ------------------------------------------------------------ 读取

    /// 打开它的终端现在的连接与主机工具。
    fn connection(&self, cx: &App) -> Result<(HostTools, Target), LoadError> {
        let terminal = self.tab.terminal.upgrade().ok_or(LoadError::SourceGone)?;
        let terminal = terminal.read(cx);
        let target = terminal
            .tools_state()
            .target()
            .cloned()
            .ok_or(LoadError::Tools(ToolsError::Exec(
                termlet_core::hosttools::ExecError::Disconnected,
            )))?;
        Ok((terminal.host_tools(), target))
    }

    fn query(&self, after: Option<LogCursor>) -> LogQuery {
        LogQuery {
            source: self.tab.source.clone(),
            lines: self.lines_choice.lines(),
            since_secs: self.range.since_secs(termlet_core::time::now_ms()),
            priority: self.priority,
            after,
        }
    }

    /// 按现在的选项重新读取（之前的内容留着，读到后换掉）。
    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.seq += 1;
        let seq = self.seq;
        let (tools, target) = match self.connection(cx) {
            Ok(connection) => connection,
            Err(error) => {
                self.error = Some(error);
                self.loading = false;
                cx.notify();
                return;
            }
        };
        let query = self.query(None);
        self.loading = true;
        self._load = Some(cx.spawn_in(window, async move |this, cx| {
            let result = tools.logs(&target, query).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.seq == seq {
                    this.loading = false;
                    this.finish_reload(result, window, cx);
                }
            });
        }));
        cx.notify();
    }

    fn finish_reload(
        &mut self,
        result: Result<LogResult, ToolsError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(logs) => {
                self.lines.clear();
                append_lines(&mut self.lines, logs.lines);
                self.cursor = logs.cursor;
                self.sudo = logs.sudo;
                self.limited = logs.limited;
                self.truncated = logs.truncated;
                self.error = None;
                self.loaded = true;
                self.updated_at = Some(termlet_core::time::now_ms());
                self.show(View::Bottom, false, window, cx);
            }
            Err(error) => {
                tracing::debug!(source = ?self.tab.source, "日志查看器：读取失败：{error}");
                self.error = Some(LoadError::Tools(error));
                if !self.loaded {
                    self.show(View::Bottom, false, window, cx);
                }
            }
        }
        self.sync_follow(window, cx);
        cx.notify();
    }

    /// 开始一次增量读取：重新读取中、还没有读到位置、没有连接时为 `None`（下一次再试）。
    fn begin_more(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<(
        u64,
        impl std::future::Future<Output = Result<LogResult, ToolsError>> + use<>,
    )> {
        if self.loading || !self.follow || !self.active {
            return None;
        }
        let cursor = self.cursor.clone()?;
        let (tools, target) = match self.connection(cx) {
            Ok(connection) => connection,
            Err(LoadError::SourceGone) => {
                self.error = Some(LoadError::SourceGone);
                cx.notify();
                return None;
            }
            // 重连中：等连上
            Err(_) => return None,
        };
        self.seq += 1;
        let query = self.query(Some(cursor));
        Some((self.seq, async move { tools.logs(&target, query).await }))
    }

    fn finish_more(
        &mut self,
        seq: u64,
        result: Result<LogResult, ToolsError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.seq != seq {
            return;
        }
        match result {
            Ok(logs) => {
                let cleared = self.error.take().is_some();
                self.cursor = logs.cursor;
                self.sudo = logs.sudo;
                self.limited = logs.limited;
                self.updated_at = Some(termlet_core::time::now_ms());
                if !logs.lines.is_empty() {
                    let view = self.follow_view();
                    let dropped = append_lines(&mut self.lines, logs.lines);
                    self.show(view, dropped > 0, window, cx);
                } else if cleared {
                    cx.notify();
                }
            }
            Err(error) => {
                tracing::debug!(source = ?self.tab.source, "日志查看器：跟随读取失败：{error}");
                self.error = Some(LoadError::Tools(error));
                cx.notify();
            }
        }
    }

    /// 跟随着且 Tab 显示着时定时增量读取，否则停止。
    fn sync_follow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !(self.follow && self.active) {
            self._follow = None;
            return;
        }
        if self._follow.is_some() {
            return;
        }
        self._follow = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(FOLLOW_INTERVAL).await;
                let Ok(next) = this.update(cx, |this, cx| this.begin_more(cx)) else {
                    return;
                };
                let Some((seq, job)) = next else {
                    continue;
                };
                let result = job.await;
                if this
                    .update_in(cx, |this, window, cx| {
                        this.finish_more(seq, result, window, cx)
                    })
                    .is_err()
                {
                    return;
                }
            }
        }));
    }

    // ------------------------------------------------------------ 显示

    /// 滚到了底部（差不到两行；还没有排版过时也算）。按滚动位置算：滚轮一动就变，不等编辑器下一次排版。自动换行时按
    /// 换行后的行数。
    fn at_bottom(&self, cx: &App) -> bool {
        let editor = self.editor.read(cx);
        let Some(line) = editor.line_height() else {
            return true;
        };
        let rows = editor
            .display_map()
            .display_row_count()
            .max(editor.text().lines_len());
        let content = line * rows as f32;
        let bottom = -editor.scroll_offset().y + editor.input_bounds().size.height;
        bottom + line * 2. >= content
    }

    /// 内容变了时视图怎样动：停在底部时滚到底，否则保持。
    fn follow_view(&self) -> View {
        if self.pinned {
            View::Bottom
        } else {
            View::Keep
        }
    }

    /// 编辑器有了变化：按滚动位置更新是否停在底部，变了才重绘（光标闪烁等不重绘）。刚要求滚到底部、滚动位置还没
    /// 变时不算（见 `settling`）。
    fn track_scroll(&mut self, cx: &mut Context<Self>) {
        let at_bottom = self.at_bottom(cx);
        if let Some(transient) = self.settling {
            if !at_bottom && self.editor.read(cx).scroll_offset() == transient {
                return;
            }
            self.settling = None;
        }
        if self.pinned != at_bottom {
            self.pinned = at_bottom;
            cx.notify();
        }
    }

    /// 按关键词与时间列重新排出正文。`trimmed` 为前面丢掉了行（选中的位置不再对应，不保留）。
    fn show(&mut self, view: View, trimmed: bool, window: &mut Window, cx: &mut Context<Self>) {
        let filter = Filter::new(self.filter.read(cx).value().as_ref());
        self.display = build_display(
            &self.lines,
            &filter,
            self.show_time,
            termlet_core::time::format_log_time,
        );
        let placeholder = if !self.loaded {
            t!("tools.loading")
        } else if self.lines.is_empty() {
            t!("tools.logs.empty")
        } else {
            t!("tools.logs.no_match")
        };
        let text = self.display.text.clone();
        self.editor.update(cx, |editor, cx| {
            let offset = editor.scroll_offset();
            let selection = editor.selected_range();
            editor.set_placeholder(placeholder, window, cx);
            editor.set_value(text, window, cx);
            match view {
                View::Bottom => editor.set_scroll_offset(point(px(0.), -Pixels::MAX), cx),
                View::Keep => {
                    editor.set_scroll_offset(offset, cx);
                    if !trimmed && !selection.is_empty() {
                        editor.set_selected_range(selection, cx);
                    }
                }
            }
        });
        self.pinned = view == View::Bottom;
        self.settling = self.pinned.then(|| self.editor.read(cx).scroll_offset());
        self.apply_marks(cx);
        cx.notify();
    }

    /// 按当前主题的颜色给正文着色：错误、警告行，时间列，关键词命中处。
    fn apply_marks(&mut self, cx: &mut Context<Self>) {
        let theme = cx.theme();
        let (danger, warning, muted) = (theme.danger, theme.warning, theme.muted_foreground);
        let color = |range: Range<usize>, color| {
            TextDecoration::new(
                range,
                HighlightStyle {
                    color: Some(color),
                    ..Default::default()
                },
            )
        };
        let mut levels: Vec<TextDecoration> = self
            .display
            .times
            .iter()
            .map(|r| color(r.clone(), muted))
            .collect();
        levels.extend(self.display.levels.iter().map(|(r, level)| {
            color(
                r.clone(),
                if *level == LogLevel::Error {
                    danger
                } else {
                    warning
                },
            )
        }));
        let matches = self
            .display
            .matches
            .iter()
            .map(|r| {
                TextDecoration::new(
                    r.clone(),
                    HighlightStyle {
                        background_color: Some(warning.opacity(0.35)),
                        ..Default::default()
                    },
                )
            })
            .collect();
        self.level_marks.set(levels, cx);
        self.match_marks.set(matches, cx);
    }

    fn scroll_to_bottom(&mut self, cx: &mut Context<Self>) {
        self.pinned = true;
        self.settling = Some(self.editor.read(cx).scroll_offset());
        self.editor.update(cx, |editor, cx| {
            editor.set_scroll_offset(point(px(0.), -Pixels::MAX), cx)
        });
        cx.notify();
    }

    // ------------------------------------------------------------ 操作

    fn set_lines(&mut self, choice: LinesChoice, window: &mut Window, cx: &mut Context<Self>) {
        if self.lines_choice != choice {
            self.lines_choice = choice;
            self.reload(window, cx);
        }
    }

    fn set_range(&mut self, range: RangeChoice, window: &mut Window, cx: &mut Context<Self>) {
        if self.range != range {
            self.range = range;
            self.reload(window, cx);
        }
    }

    fn set_priority(&mut self, priority: LogPriority, window: &mut Window, cx: &mut Context<Self>) {
        if self.priority != priority {
            self.priority = priority;
            self.reload(window, cx);
        }
    }

    fn toggle_time(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_time = !self.show_time;
        self.show(self.follow_view(), true, window, cx);
    }

    fn toggle_wrap(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.wrap = !self.wrap;
        let wrap = self.wrap;
        self.editor
            .update(cx, |editor, cx| editor.set_soft_wrap(wrap, window, cx));
        // 换行与否改变了行数：停在底部的照旧停在底部
        if self.pinned {
            self.scroll_to_bottom(cx);
        }
        cx.notify();
    }

    fn toggle_follow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.follow = !self.follow;
        tracing::debug!(follow = self.follow, "日志查看器：跟随");
        if self.follow {
            self.scroll_to_bottom(cx);
        }
        self.sync_follow(window, cx);
        cx.notify();
    }

    fn copy_all(&self, window: &mut Window, cx: &mut App) {
        cx.write_to_clipboard(ClipboardItem::new_string(self.display.text.clone()));
        window.push_notification(Notification::success(t!("tools.logs.copied")), cx);
    }

    /// 在打开它的终端旁的新 Tab 中跟随（终端里的 `journalctl -f` / `docker logs -f`）。
    fn follow_in_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tab.terminal.upgrade().is_none() {
            crate::notify::warning(t!("tools.source_gone"), window, cx);
            return;
        }
        match terminal_command(&follow_request(&self.tab.source, self.sudo)) {
            Ok(command) => cx.emit(LogPanelEvent::RunInTerminal {
                terminal: self.tab.terminal.clone(),
                command,
                title: t!("tools.tab_logs", name = self.tab.name.clone()),
            }),
            Err(error) => crate::notify::error(errors::describe(&error), window, cx),
        }
    }

    // ------------------------------------------------------------ 渲染

    fn render_toolbar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        let rem = window.rem_size();
        let full = self.toolbar_width.get() >= rems(TOOLBAR_FULL).to_pixels(rem);
        let narrow = self.toolbar_width.get() < rems(TOOLBAR_NARROW).to_pixels(rem);
        let lines_choice = self.lines_choice;
        let lines = Button::new("logs-lines")
            .ghost()
            .xsmall()
            .dropdown_caret(true)
            .label(lines_label(lines_choice))
            .dropdown_menu({
                let this = this.clone();
                move |menu, _, _| {
                    LinesChoice::ALL.into_iter().fold(menu, |menu, choice| {
                        let this = this.clone();
                        menu.item(
                            PopupMenuItem::new(lines_label(choice))
                                .checked(choice == lines_choice)
                                .on_click(move |_, window, cx| {
                                    let _ = this
                                        .update(cx, |this, cx| this.set_lines(choice, window, cx));
                                }),
                        )
                    })
                }
            });
        let range_choice = self.range;
        let range = Button::new("logs-range")
            .ghost()
            .xsmall()
            .dropdown_caret(true)
            .label(range_label(range_choice))
            .dropdown_menu({
                let this = this.clone();
                move |menu, _, _| {
                    RangeChoice::ALL.into_iter().fold(menu, |menu, choice| {
                        let this = this.clone();
                        menu.item(
                            PopupMenuItem::new(range_label(choice))
                                .checked(choice == range_choice)
                                .on_click(move |_, window, cx| {
                                    let _ = this
                                        .update(cx, |this, cx| this.set_range(choice, window, cx));
                                }),
                        )
                    })
                }
            });
        let service = matches!(self.tab.source, LogSource::Service { .. });
        let priority_choice = self.priority;
        let priority = service.then(|| {
            Button::new("logs-priority")
                .ghost()
                .xsmall()
                .dropdown_caret(true)
                .label(priority_label(priority_choice))
                .dropdown_menu({
                    let this = this.clone();
                    move |menu, _, _| {
                        [LogPriority::All, LogPriority::Warning, LogPriority::Error]
                            .into_iter()
                            .fold(menu, |menu, choice| {
                                let this = this.clone();
                                menu.item(
                                    PopupMenuItem::new(priority_label(choice))
                                        .checked(choice == priority_choice)
                                        .on_click(move |_, window, cx| {
                                            let _ = this.update(cx, |this, cx| {
                                                this.set_priority(choice, window, cx)
                                            });
                                        }),
                                )
                            })
                    }
                })
        });
        // 窄的时候收进 ⋯ 的：复制全部、在新 Tab 中跟随；更窄时另有显示时间、自动换行
        let empty = self.display.text.is_empty();
        let (show_time, wrap) = (self.show_time, self.wrap);
        let more = (!full).then(|| {
            let this = this.clone();
            Button::new("logs-more")
                .ghost()
                .xsmall()
                .icon(IconName::Ellipsis)
                .tooltip(t!("tools.detail.more"))
                .dropdown_menu(move |menu, _, _| {
                    let (copy, follow) = (this.clone(), this.clone());
                    let (time, wrapping) = (this.clone(), this.clone());
                    let menu = if narrow {
                        menu.item(
                            PopupMenuItem::new(t!("tools.logs.show_time"))
                                .checked(show_time)
                                .on_click(move |_, window, cx| {
                                    let _ =
                                        time.update(cx, |this, cx| this.toggle_time(window, cx));
                                }),
                        )
                        .item(
                            PopupMenuItem::new(t!("tools.logs.wrap_on"))
                                .checked(wrap)
                                .on_click(move |_, window, cx| {
                                    let _ = wrapping
                                        .update(cx, |this, cx| this.toggle_wrap(window, cx));
                                }),
                        )
                        .separator()
                    } else {
                        menu
                    };
                    menu.item(
                        PopupMenuItem::new(t!("tools.logs.copy_all"))
                            .icon(IconName::Copy)
                            .disabled(empty)
                            .on_click(move |_, window, cx| {
                                let _ = copy.update(cx, |this, cx| this.copy_all(window, cx));
                            }),
                    )
                    .item(
                        PopupMenuItem::new(t!("tools.follow_logs"))
                            .icon(IconName::SquareTerminal)
                            .on_click(move |_, window, cx| {
                                let _ = follow
                                    .update(cx, |this, cx| this.follow_in_terminal(window, cx));
                            }),
                    )
                })
        });
        let measured = self.toolbar_width.clone();
        let entity = cx.entity().downgrade();
        let thresholds = [TOOLBAR_FULL, TOOLBAR_NARROW].map(|from| rems(from).to_pixels(rem));
        h_flex()
            .relative()
            .flex_none()
            .flex_wrap()
            .px_2()
            .py_1()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                // 宽度跨过收起次要项的界限时重新渲染
                canvas(
                    move |bounds, _, cx| {
                        let old = measured.replace(bounds.size.width);
                        if thresholds
                            .iter()
                            .any(|t| (old >= *t) != (bounds.size.width >= *t))
                        {
                            let _ = entity.update(cx, |_, cx| cx.notify());
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(lines)
            .child(range)
            .children(priority)
            .child(
                div().flex_1().min_w(rems(6.)).child(
                    Input::new(&self.filter)
                        .id("logs-filter")
                        .xsmall()
                        .prefix(Icon::new(IconName::ListFilter).xsmall())
                        .cleanable(true),
                ),
            )
            .when(!narrow, |this| {
                this.child(
                    Button::new("logs-time")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Clock)
                        .selected(self.show_time)
                        .toggled(self.show_time)
                        .tooltip(if self.show_time {
                            t!("tools.logs.hide_time")
                        } else {
                            t!("tools.logs.show_time")
                        })
                        .on_click(cx.listener(|this, _, window, cx| this.toggle_time(window, cx))),
                )
                .child(
                    Button::new("logs-wrap")
                        .ghost()
                        .xsmall()
                        .icon(IconName::TextWrap)
                        .selected(self.wrap)
                        .toggled(self.wrap)
                        .tooltip(if self.wrap {
                            t!("tools.logs.wrap_off")
                        } else {
                            t!("tools.logs.wrap_on")
                        })
                        .on_click(cx.listener(|this, _, window, cx| this.toggle_wrap(window, cx))),
                )
            })
            .child(
                Button::new("logs-follow")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ArrowDownToLine)
                    .when(full, |this| this.label(t!("tools.logs.follow")))
                    .selected(self.follow)
                    .toggled(self.follow)
                    .tooltip(if self.follow {
                        t!("tools.logs.follow_stop")
                    } else {
                        t!("tools.logs.follow_start")
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_follow(window, cx))),
            )
            .child(
                Button::new("logs-refresh")
                    .ghost()
                    .xsmall()
                    .icon(IconName::RefreshCw)
                    .loading(self.loading)
                    .disabled(self.loading)
                    .tooltip(t!("tools.refresh"))
                    .on_click(cx.listener(|this, _, window, cx| this.reload(window, cx))),
            )
            .when(full, |this| {
                this.child(
                    Button::new("logs-copy")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Copy)
                        .tooltip(t!("tools.logs.copy_all"))
                        .disabled(empty)
                        .on_click(cx.listener(|this, _, window, cx| this.copy_all(window, cx))),
                )
                .child(
                    Button::new("logs-follow-terminal")
                        .ghost()
                        .xsmall()
                        .icon(IconName::SquareTerminal)
                        .tooltip(t!("tools.follow_logs"))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.follow_in_terminal(window, cx)),
                        ),
                )
            })
            .children(more)
    }

    /// 正文上方的说明：怎样读的（经 sudo、以普通用户）、截断、读不到的原因。
    fn render_notes(&self, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        let note = |id: &'static str, text: SharedString, color| {
            div()
                .id(id)
                .test_support()
                .px_2()
                .text_xs()
                .text_color(color)
                .child(prose(text))
        };
        let error = self.error.as_ref().map(|error| match error {
            LoadError::SourceGone => t!("tools.logs.source_gone"),
            LoadError::Tools(error) => errors::describe(error),
        });
        v_flex()
            .flex_none()
            .py_0p5()
            .gap_0p5()
            .when(self.sudo, |this| {
                this.child(note(
                    "logs-note-sudo",
                    t!("tools.logs.read_sudo"),
                    theme.muted_foreground,
                ))
            })
            .when(self.limited, |this| {
                this.child(note(
                    "logs-note-limited",
                    if self.lines.is_empty() {
                        t!("tools.logs.limited_none")
                    } else {
                        t!("tools.logs.limited_some")
                    },
                    theme.warning,
                ))
            })
            .when(self.truncated, |this| {
                this.child(note(
                    "logs-note-truncated",
                    t!("tools.logs.truncated"),
                    theme.warning,
                ))
            })
            .children(error.map(|text| note("logs-note-error", text, theme.danger)))
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let total = self.lines.len();
        let count = match (self.display.shown == total, total) {
            (true, 1) => t!("tools.logs.count_one"),
            (true, _) => t!("tools.logs.count", total = total),
            (false, 1) => t!("tools.logs.shown_of_one", shown = self.display.shown),
            (false, _) => t!(
                "tools.logs.shown",
                shown = self.display.shown,
                total = total
            ),
        };
        // 往上翻着（新行到达时不自动滚动）：说明，可以回到底部
        let paused = self.follow && self.loaded && !self.pinned;
        h_flex()
            .id("logs-status")
            .test_support()
            .flex_none()
            .h(gpui_kit::rems(1.75))
            .px_2()
            .gap_2()
            .text_xs()
            .text_color(muted)
            .border_t_1()
            .border_color(theme.border)
            .child(div().flex_1().min_w_0().truncate().child(format!(
                "{} · {}",
                self.tab.host,
                source_label(&self.tab.source, &self.tab.name)
            )))
            .when(paused, |this| {
                this.child(div().flex_none().child(t!("tools.logs.paused")))
                    .child(
                        Button::new("logs-to-bottom")
                            .ghost()
                            .xsmall()
                            .icon(IconName::ArrowDown)
                            .label(t!("tools.logs.to_bottom"))
                            .on_click(cx.listener(|this, _, _, cx| this.scroll_to_bottom(cx))),
                    )
            })
            .child(div().flex_none().child(count))
            .when_some(self.updated_at, |this, at| {
                this.child(
                    div()
                        .flex_none()
                        .child(t!("tools.updated_at", time = super::model::clock_time(at))),
                )
            })
    }
}

/// 状态栏中的来源：服务的完整单元名、容器名。
fn source_label(source: &LogSource, name: &str) -> String {
    match source {
        LogSource::Service { unit } => unit.clone(),
        LogSource::Container { .. } => name.to_string(),
    }
}

fn lines_label(choice: LinesChoice) -> SharedString {
    match choice {
        LinesChoice::Last200 => t!("tools.logs.lines_200"),
        LinesChoice::Last1000 => t!("tools.logs.lines_1000"),
        LinesChoice::All => t!("tools.logs.lines_all"),
    }
}

fn range_label(choice: RangeChoice) -> SharedString {
    match choice {
        RangeChoice::All => t!("tools.logs.range_all"),
        RangeChoice::Minutes15 => t!("tools.logs.range_15m"),
        RangeChoice::Hour1 => t!("tools.logs.range_1h"),
        RangeChoice::Today => t!("tools.logs.range_today"),
    }
}

fn priority_label(priority: LogPriority) -> SharedString {
    match priority {
        LogPriority::All => t!("tools.logs.level_all"),
        LogPriority::Warning => t!("tools.logs.level_warning"),
        LogPriority::Error => t!("tools.logs.level_error"),
    }
}

impl BasePanel for LogPanel {
    fn panel_name(&self) -> &'static str {
        PANEL_NAME
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.active = active;
        if active {
            self.editor
                .update(cx, |editor, cx| editor.focus(window, cx));
            // 不显示期间停了跟随：先补读一次
            if self.follow
                && let Some((seq, job)) = self.begin_more(cx)
            {
                self._load = Some(cx.spawn_in(window, async move |this, cx| {
                    let result = job.await;
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.finish_more(seq, result, window, cx)
                    });
                }));
            }
        }
        self.sync_follow(window, cx);
        cx.notify();
    }

    fn on_removed(&mut self, _: &mut Window, _: &mut Context<Self>) {
        // 关闭：停止跟随与进行中的读取
        self.active = false;
        self.seq += 1;
        self._follow = None;
        self._load = None;
    }
}

impl LogPanel {
    /// Tab 的悬停提示：主机与日志的来源。
    fn tab_tooltip(&self) -> SharedString {
        format!(
            "{} · {}",
            self.tab.host,
            source_label(&self.tab.source, &self.tab.name)
        )
        .into()
    }

    /// 竖排的 Tab 列表中这个 Tab 的一行：与横排的 Tab 上相同的图标、名称与跟随中的圆点；按主机分组时与打开它的终端
    /// 归在一起。
    pub(crate) fn tab_face(&self, cx: &App) -> crate::workspace::TabFace {
        let following = self.follow && self.active;
        crate::workspace::TabFace {
            icon: Icon::new(IconName::ScrollText)
                .xsmall()
                .text_color(cx.theme().info)
                .into_any_element(),
            rail_icon: Icon::new(IconName::ScrollText)
                .with_size(crate::workspace::RAIL_ICON)
                .text_color(cx.theme().info)
                .into_any_element(),
            name: self.title_text(),
            detail: Some(self.tab_tooltip()),
            target: None,
            badges: following
                .then(|| {
                    div()
                        .size_1p5()
                        .rounded_full()
                        .bg(cx.theme().success)
                        .into_any_element()
                })
                .into_iter()
                .collect(),
            status: following
                .then(|| {
                    crate::workspace::TabStatus::dot(
                        cx.theme().success,
                        t!("tab_list.status.following"),
                    )
                })
                .into_iter()
                .collect(),
            host: self
                .tab
                .terminal
                .upgrade()
                .and_then(|terminal| terminal.read(cx).tab_host(cx)),
        }
    }
}

impl Panel for LogPanel {
    fn title(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let id = cx.entity_id().as_u64() as usize;
        // 右键菜单开着时关闭按钮不挂悬停提示（会浮在菜单上，见 crate::menu_tooltip）
        let menu_open = crate::menu_tooltip::menu_open(window, cx);
        let tip = self.tab_tooltip();
        h_flex()
            .gap_1()
            .child(
                h_flex()
                    .id(("log-tab-title", id))
                    .test_support()
                    .gap_1()
                    .tooltip(crate::menu_tooltip::quiet(move |window, cx| {
                        Tooltip::new(tip.clone()).build(window, cx)
                    }))
                    // 右键 Tab 打开这个 Tab 的菜单（Tab 栏上没有“…”，见 dock_skin）
                    .context_menu({
                        let this = cx.entity().downgrade();
                        move |menu, window, cx| match this.upgrade() {
                            Some(panel) => {
                                panel.update(cx, |panel, cx| panel.dropdown_menu(menu, window, cx))
                            }
                            None => menu,
                        }
                    })
                    .child(
                        Icon::new(IconName::ScrollText)
                            .xsmall()
                            .text_color(cx.theme().info),
                    )
                    .child(self.title_text()),
            )
            .when(self.follow && self.active, |this| {
                this.child(div().size_1p5().rounded_full().bg(cx.theme().success))
            })
            .child(
                // 自己的关闭按钮（Dock 自带的不显示）
                Button::new(("log-tab-close", id))
                    .ghost()
                    .small()
                    .icon(IconName::Close)
                    .tab_stop(false)
                    .when(!menu_open, |this| this.tooltip(t!("panel.close")))
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.close_tab(cx);
                    })),
            )
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }

    /// Tab 菜单（右键 Tab 打开）：刷新、复制全部、关闭与关闭几个 Tab。
    fn dropdown_menu(
        &mut self,
        menu: PopupMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PopupMenu {
        let this = cx.entity().downgrade();
        let act = |f: fn(&mut LogPanel, &mut Window, &mut Context<LogPanel>)| {
            let this = this.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let _ = this.update(cx, |this, cx| f(this, window, cx));
            }
        };
        let menu = menu
            .item(
                PopupMenuItem::new(t!("tools.refresh"))
                    .icon(IconName::RefreshCw)
                    .disabled(self.loading)
                    .on_click(act(|this, window, cx| this.reload(window, cx))),
            )
            .item(
                PopupMenuItem::new(t!("tools.logs.copy_all"))
                    .icon(IconName::Copy)
                    .disabled(self.display.text.is_empty())
                    .on_click(act(|this, window, cx| this.copy_all(window, cx))),
            )
            .item(
                PopupMenuItem::new(t!("tools.follow_logs"))
                    .icon(IconName::SquareTerminal)
                    .on_click(act(|this, window, cx| this.follow_in_terminal(window, cx))),
            )
            .separator()
            .item(
                PopupMenuItem::new(t!("panel.close"))
                    .on_click(act(|this, _, cx| this.close_tab(cx))),
            );
        crate::workspace::close_tab_items(menu, cx.entity_id(), window, cx).separator()
    }
}

impl Render for LogPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selection = self.editor.clone();
        let editor = Editor::new(&self.editor)
            .bordered(false)
            .rounded_none()
            .size_full()
            .readonly(true)
            .font_family(cx.theme().mono_font_family.clone())
            .text_sm()
            // 只读：右键菜单只放复制、全选与查找
            .context_menu(move |menu, _, cx| {
                let selected = !selection.read(cx).selected_range().is_empty();
                menu.menu_with_disabled(t!("editor.menu.copy"), !selected, Box::new(input::Copy))
                    .menu(t!("editor.menu.select_all"), Box::new(input::SelectAll))
                    .separator()
                    .menu(t!("editor.find"), Box::new(input::Search))
            });
        v_flex()
            .id("log-panel")
            .test_support()
            .size_full()
            .child(self.render_toolbar(window, cx))
            .child(self.render_notes(cx))
            .child(
                div()
                    .id("logs-text")
                    .test_support()
                    .flex_1()
                    .min_h_0()
                    .child(editor),
            )
            .child(self.render_status(cx))
    }
}

#[cfg(test)]
impl LogPanel {
    /// Tab 显示着。
    pub(crate) fn is_active(&self) -> bool {
        self.active
    }

    /// 测试中直接放进读到的行（不连服务器）。
    pub(super) fn show_test_lines(
        &mut self,
        lines: Vec<LogLine>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.lines = lines;
        self.loaded = true;
        self.error = None;
        self.show(View::Bottom, false, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{Bounds, ScrollDelta, TestAppContext, WindowBounds, WindowOptions, size};

    use super::*;
    use crate::app_state::AppState;

    fn line(at: Option<i64>, text: &str, level: LogLevel) -> LogLine {
        LogLine {
            at,
            text: text.into(),
            level,
        }
    }

    /// 时间列可以不显示；关键词在本机筛选并标出命中处；跟随可以关掉；复制全部复制显示着的内容。打开它的终端已关闭时
    /// 说明原因。
    #[gpui_kit::test]
    fn times_filter_follow_and_copy(cx: &mut TestAppContext) {
        // 日志的读取在核心的运行时（termlet-io 线程）上执行：允许测试调度器看到别的线程的活动
        cx.executor().allow_parking();
        crate::i18n::set_language(crate::i18n::Language::ZhCn);
        cx.update(gpui_kit::init);
        cx.update(crate::init);
        cx.update(AppState::install_in_memory);
        let tab = LogTab {
            terminal: WeakEntity::new_invalid(),
            terminal_id: cx.update(|cx| cx.new(|_| ()).entity_id()),
            host: "web".into(),
            name: "demo-app".into(),
            source: LogSource::Service {
                unit: "demo-app.service".into(),
            },
        };
        let (handle, panel) = cx.update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        Default::default(),
                        size(px(900.), px(600.)),
                    ))),
                    ..Default::default()
                },
                cx,
                |window, cx| LogPanel::open(tab, window, cx),
            )
            .expect("打开测试窗口")
        });
        let at = 1_793_756_832_000;
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("logs-note-error").is_some(), "终端已关闭");
            panel.update(cx, |panel, cx| {
                panel.show_test_lines(
                    vec![
                        line(Some(at), "app[7]: started", LogLevel::Normal),
                        line(Some(at + 1000), "app[7]: ERROR boom", LogLevel::Error),
                        line(None, "        at Main.run", LogLevel::Error),
                    ],
                    window,
                    cx,
                )
            });
            window.render_frame(cx);
        })
        .unwrap();
        let text = |cx: &mut TestAppContext| {
            cx.read(|cx| panel.read(cx).editor.read(cx).value().to_string())
        };
        let time = termlet_core::time::format_log_time(at);
        assert!(text(cx).starts_with(&format!("{time} app[7]: started")));

        cx.update_window(handle, |_, window, cx| {
            window.click("logs-time", cx);
            window.render_frame(cx);
        })
        .unwrap();
        assert_eq!(
            text(cx),
            "app[7]: started\napp[7]: ERROR boom\n        at Main.run"
        );

        cx.update_window(handle, |_, window, cx| {
            window.click("logs-filter", cx);
            window.input("main", cx);
            window.render_frame(cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(text(cx), "app[7]: ERROR boom\n        at Main.run");
        assert_eq!(cx.read(|cx| panel.read(cx).display.matches.len()), 1);

        cx.update_window(handle, |_, window, cx| {
            window.click("logs-follow", cx);
            window.render_frame(cx);
        })
        .unwrap();
        assert!(!cx.read(|cx| panel.read(cx).follow));

        cx.update_window(handle, |_, window, cx| {
            window.click("logs-copy", cx);
        })
        .unwrap();
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("app[7]: ERROR boom\n        at Main.run".to_string())
        );
    }

    /// 长行默认自动换行，可以关掉（横向滚动）；工具条窄的时候“跟随”只留图标，复制全部、在新 Tab 中跟随收进 ⋯，
    /// 宽的时候都在工具条上。
    #[gpui_kit::test]
    fn wrap_toggle_and_compact_toolbar(cx: &mut TestAppContext) {
        // 日志的读取在核心的运行时（termlet-io 线程）上执行：允许测试调度器看到别的线程的活动
        cx.executor().allow_parking();
        crate::i18n::set_language(crate::i18n::Language::En);
        cx.update(gpui_kit::init);
        cx.update(crate::init);
        cx.update(AppState::install_in_memory);
        let tab = LogTab {
            terminal: WeakEntity::new_invalid(),
            terminal_id: cx.update(|cx| cx.new(|_| ()).entity_id()),
            host: "web".into(),
            name: "demo-app".into(),
            source: LogSource::Service {
                unit: "demo-app.service".into(),
            },
        };
        let (handle, panel) = cx.update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        Default::default(),
                        size(px(1200.), px(600.)),
                    ))),
                    ..Default::default()
                },
                cx,
                |window, cx| LogPanel::open(tab, window, cx),
            )
            .expect("打开测试窗口")
        });
        let frames = |cx: &mut TestAppContext| {
            for _ in 0..2 {
                cx.update_window(handle, |_, window, cx| window.render_frame(cx))
                    .unwrap();
                cx.run_until_parked();
            }
        };
        frames(cx);
        cx.update_window(handle, |_, window, cx| {
            assert!(
                window.try_find("logs-copy").is_some(),
                "宽的时候都在工具条上"
            );
            assert!(window.try_find("logs-more").is_none());
            window.click("logs-wrap", cx);
        })
        .unwrap();
        assert!(!cx.read(|cx| panel.read(cx).wrap), "关掉自动换行");
        cx.simulate_window_resize(handle, size(px(520.), px(600.)));
        frames(cx);
        cx.update_window(handle, |_, window, cx| {
            assert!(window.try_find("logs-copy").is_none(), "窄的时候收进 ⋯");
            window.click("logs-more", cx);
        })
        .unwrap();
        cx.run_until_parked();
        let toolbar = cx
            .update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                (
                    window.find("logs-filter").bounds(),
                    window.find("logs-refresh").bounds(),
                )
            })
            .unwrap();
        assert!(
            (toolbar.0.center().y - toolbar.1.center().y).abs() < px(2.),
            "英文下工具条也在一行"
        );
        crate::i18n::set_language(crate::i18n::Language::ZhCn);
    }

    /// 服务的日志可能因为权限看不全时说明原因与办法（看不到、只看到一部分两种说法）；能看全时不说明。
    #[gpui_kit::test]
    fn limited_service_logs_explain_permissions(cx: &mut TestAppContext) {
        // 日志的读取在核心的运行时（termlet-io 线程）上执行：允许测试调度器看到别的线程的活动
        cx.executor().allow_parking();
        crate::i18n::set_language(crate::i18n::Language::ZhCn);
        cx.update(gpui_kit::init);
        cx.update(crate::init);
        cx.update(AppState::install_in_memory);
        let tab = LogTab {
            terminal: WeakEntity::new_invalid(),
            terminal_id: cx.update(|cx| cx.new(|_| ()).entity_id()),
            host: "web".into(),
            name: "demo-app".into(),
            source: LogSource::Service {
                unit: "demo-app.service".into(),
            },
        };
        let (handle, panel) = cx.update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        Default::default(),
                        size(px(900.), px(600.)),
                    ))),
                    ..Default::default()
                },
                cx,
                |window, cx| LogPanel::open(tab, window, cx),
            )
            .expect("打开测试窗口")
        });
        let result = |lines: Vec<LogLine>, limited| LogResult {
            lines,
            truncated: false,
            sudo: false,
            limited,
            cursor: None,
        };
        let some = || vec![line(None, "app[7]: started", LogLevel::Normal)];
        for (lines, limited, shown) in [
            (Vec::new(), true, true),
            (some(), true, true),
            (some(), false, false),
        ] {
            cx.update_window(handle, |_, window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.finish_reload(Ok(result(lines, limited)), window, cx)
                });
                window.render_frame(cx);
                assert_eq!(window.try_find("logs-note-limited").is_some(), shown);
            })
            .unwrap();
        }
    }

    /// 往上翻时立即提示“已暂停自动滚动”与“回到底部”（不等新行），点“回到底部”或自己滚回底部时立即恢复。
    #[gpui_kit::test]
    fn scrolling_up_pauses_at_once_and_back_resumes(cx: &mut TestAppContext) {
        // 日志的读取在核心的运行时（termlet-io 线程）上执行：允许测试调度器看到别的线程的活动
        cx.executor().allow_parking();
        crate::i18n::set_language(crate::i18n::Language::ZhCn);
        cx.update(gpui_kit::init);
        cx.update(crate::init);
        cx.update(AppState::install_in_memory);
        let tab = LogTab {
            terminal: WeakEntity::new_invalid(),
            terminal_id: cx.update(|cx| cx.new(|_| ()).entity_id()),
            host: "web".into(),
            name: "demo-app".into(),
            source: LogSource::Service {
                unit: "demo-app.service".into(),
            },
        };
        let (handle, panel) = cx.update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        Default::default(),
                        size(px(900.), px(600.)),
                    ))),
                    ..Default::default()
                },
                cx,
                |window, cx| LogPanel::open(tab, window, cx),
            )
            .expect("打开测试窗口")
        });
        let lines: Vec<LogLine> = (0..300)
            .map(|i| {
                line(
                    Some(1_793_756_832_000 + i),
                    &format!("line {i}"),
                    LogLevel::Normal,
                )
            })
            .collect();
        let frame = |cx: &mut TestAppContext| {
            for _ in 0..2 {
                cx.update_window(handle, |_, window, cx| window.render_frame(cx))
                    .unwrap();
                cx.run_until_parked();
            }
        };
        cx.update_window(handle, |_, window, cx| {
            panel.update(cx, |panel, cx| panel.show_test_lines(lines, window, cx))
        })
        .unwrap();
        frame(cx);
        let paused = |cx: &mut TestAppContext| {
            cx.update_window(handle, |_, window, _| {
                window.try_find("logs-to-bottom").is_some()
            })
            .unwrap()
        };
        assert!(!paused(cx), "读到时停在底部");

        let wheel = |cx: &mut TestAppContext, lines: f32| {
            cx.update_window(handle, |_, window, cx| {
                window.scroll("logs-text", ScrollDelta::Lines(point(0., lines)), cx)
            })
            .unwrap();
            cx.run_until_parked();
        };
        wheel(cx, 10.);
        frame(cx);
        assert!(paused(cx), "往上翻：立即提示");
        assert!(!cx.read(|cx| panel.read(cx).pinned));

        cx.update_window(handle, |_, window, cx| window.click("logs-to-bottom", cx))
            .unwrap();
        frame(cx);
        assert!(!paused(cx), "回到底部：提示消失");
        assert!(cx.read(|cx| panel.read(cx).pinned));

        wheel(cx, 10.);
        frame(cx);
        assert!(paused(cx));
        wheel(cx, -1000.);
        frame(cx);
        assert!(!paused(cx), "自己滚回底部：立即恢复");
    }
}
