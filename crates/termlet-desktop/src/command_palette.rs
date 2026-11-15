//! 命令面板（Ctrl+Shift+P）：搜索主机、已打开的 Tab 与各种操作；输入地址时第一项为快速连接。
//!
//! - 以 `>` 开头只搜操作，`@` 只搜已打开的 Tab，`#` 只搜主机（有前缀时不提供快速连接）。
//! - 按空白拆成多个词，每个词都要出现在名称或说明里（不区分大小写）。每个词计分：名称以它开头 3 分，
//!   名称中含有 2 分，只在说明中出现 1 分；按总分从高到低，同分保持原来的顺序；最多列出 50 项。

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::command::{Command, CommandItem, CommandState};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, WindowExt as _, h_flex};
use gpui_kit::{
    Action, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, TestSupportExt as _, WeakEntity, Window,
    div, rems,
};
use termlet_core::id::{HostId, WorkspaceId};
use termlet_core::model::{Host, Workspace as SavedWorkspace};
use termlet_core::quick::{self, QuickTarget};

use crate::app_state::AppState;
use crate::editor::EditorPanel;
use crate::host_tools::LogPanel;
use crate::host_tools::target::ToolsState;
use crate::prose::prose;
use crate::replay::ReplayPanel;
use crate::sftp::panel::SftpPanel;
use crate::terminal_panel::{DuplicateSession, Reconnect, TerminalPanel, ToggleRecording};
use crate::ui_scale::{self, ResetUiZoom, ZoomInUi, ZoomOutUi};
use crate::workspace::{
    FocusTabList, MoveTabToNewWindow, NewLocalTerminal, NewWindow, OpenCommandPicker, OpenCompose,
    OpenHostToolsOverview, OpenSftpBeside, PopOutHostOverview, RevealHostInSidebar,
    SaveAsWorkspace, ToggleFullscreen, ToggleHostTools, ToggleMultiExec, ToggleSidebar, Workspace,
};
use crate::{group_dialog, host_dialog, schemes, settings, t};

/// 最多列出这么多项。
const MAX_RESULTS: usize = 50;

/// 一个可以执行的条目。
#[derive(Clone)]
enum Target {
    Quick(QuickTarget),
    Host(HostId),
    Tab(TabRef),
    Workspace(WorkspaceId),
    Op(Op),
}

/// 已打开的 Tab。
#[derive(Clone)]
pub enum TabRef {
    Terminal(WeakEntity<TerminalPanel>),
    Sftp(WeakEntity<SftpPanel>),
    Editor(WeakEntity<EditorPanel>),
    /// 日志查看器
    Logs(WeakEntity<LogPanel>),
    /// 会话录制的回放
    Replay(WeakEntity<ReplayPanel>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Duplicate,
    Reconnect,
    ToggleRecording,
    OpenRecording,
    NewLocalTerminal,
    NewHost,
    NewGroup,
    ToggleSidebar,
    ToggleFullscreen,
    UiZoomIn,
    UiZoomOut,
    UiZoomReset,
    ToggleTheme,
    /// 切换 Tab 的位置（顶部 / 左侧）
    ToggleTabPosition,
    ChangeScheme,
    CommandPicker,
    SaveWorkspace,
    SftpBeside,
    RevealHost,
    MultiExec,
    Compose,
    HostTools,
    HostToolsOverview,
    /// 把当前终端所连主机的概览弹出成置顶的小窗
    HostToolsPopOut,
    SnippetBar,
    ImportHosts,
    ExportHosts,
    Audit,
    LockNow,
    NewWindow,
    /// 把当前 Tab 移到新窗口
    MoveToNewWindow,
    /// 焦点进竖排的 Tab 列表（Tab 放在左侧时）
    FocusTabList,
    Settings,
    Shortcuts,
    /// 切到侧边栏的“转发”页（标题栏上运行中的转发数的按钮）
    Forwards,
    /// 关键词高亮的设置（标题栏的按钮）
    Highlight,
    /// 自定义工具栏（设置的“工具栏”页）
    Toolbar,
}

impl Op {
    fn action(self) -> Option<Box<dyn Action>> {
        match self {
            Op::Duplicate => Some(Box::new(DuplicateSession)),
            Op::Reconnect => Some(Box::new(Reconnect)),
            Op::ToggleRecording => Some(Box::new(ToggleRecording)),
            Op::NewLocalTerminal => Some(Box::new(NewLocalTerminal)),
            Op::ToggleSidebar => Some(Box::new(ToggleSidebar)),
            Op::ToggleFullscreen => Some(Box::new(ToggleFullscreen)),
            Op::CommandPicker => Some(Box::new(OpenCommandPicker)),
            Op::SaveWorkspace => Some(Box::new(SaveAsWorkspace)),
            Op::SftpBeside => Some(Box::new(OpenSftpBeside)),
            Op::RevealHost => Some(Box::new(RevealHostInSidebar)),
            Op::MultiExec => Some(Box::new(ToggleMultiExec)),
            Op::Compose => Some(Box::new(OpenCompose)),
            Op::HostTools => Some(Box::new(ToggleHostTools)),
            Op::HostToolsOverview => Some(Box::new(OpenHostToolsOverview)),
            Op::HostToolsPopOut => Some(Box::new(PopOutHostOverview)),
            Op::NewWindow => Some(Box::new(NewWindow)),
            Op::MoveToNewWindow => Some(Box::new(MoveTabToNewWindow)),
            Op::FocusTabList => Some(Box::new(FocusTabList)),
            Op::UiZoomIn => Some(Box::new(ZoomInUi)),
            Op::UiZoomOut => Some(Box::new(ZoomOutUi)),
            Op::UiZoomReset => Some(Box::new(ResetUiZoom)),
            Op::NewHost
            | Op::NewGroup
            | Op::OpenRecording
            | Op::SnippetBar
            | Op::ToggleTheme
            | Op::ToggleTabPosition
            | Op::ChangeScheme
            | Op::Settings
            | Op::ImportHosts
            | Op::ExportHosts
            | Op::Audit
            | Op::LockNow
            | Op::Shortcuts
            | Op::Forwards
            | Op::Highlight
            | Op::Toolbar => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Category {
    Quick,
    Op,
    Host,
    Tab,
    Workspace,
}

#[derive(Clone)]
struct Entry {
    category: Category,
    name: SharedString,
    detail: SharedString,
    target: Target,
}

pub struct CommandPalette {
    state: Entity<CommandState>,
    workspace: WeakEntity<Workspace>,
    /// 打开面板时焦点所在的终端（复制会话、重新连接作用于它）
    current: Option<WeakEntity<TerminalPanel>>,
    /// 打开面板时已打开的 Tab
    tabs: Vec<TabRef>,
    /// 打开面板时同步输入开着
    syncing: bool,
    /// 当前查询下列出的条目
    shown: Vec<Entry>,
}

/// 打开命令面板。
pub fn open(
    workspace: WeakEntity<Workspace>,
    current: Option<WeakEntity<TerminalPanel>>,
    tabs: Vec<TabRef>,
    syncing: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let palette = cx.new(|cx| CommandPalette::new(workspace, current, tabs, syncing, window, cx));
    let first_focus = Rc::new(Cell::new(true));
    let width = rems(36.).to_pixels(window.rem_size());
    window.open_dialog(cx, move |dialog, window, cx| {
        if first_focus.replace(false) {
            let state = palette.read(cx).state.clone();
            state.update(cx, |state, cx| state.focus(window, cx));
        }
        dialog
            .p_0()
            .close_button(false)
            .w(width)
            .child(palette.clone())
    });
}

impl CommandPalette {
    fn new(
        workspace: WeakEntity<Workspace>,
        current: Option<WeakEntity<TerminalPanel>>,
        tabs: Vec<TabRef>,
        syncing: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let state = cx.new(|cx| CommandState::new(window, cx));
        let mut palette = Self {
            state,
            workspace,
            current,
            tabs,
            syncing,
            shown: Vec::new(),
        };
        palette.refresh(cx);
        palette
    }

    /// 按当前的查询重新列出条目。
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let query = self.state.read(cx).query(cx).to_string();
        let (scope, words) = parse_query(&query);
        let mut entries = Vec::new();
        if scope.is_none()
            && let Some(target) = quick::parse(&query)
        {
            entries.push(Entry {
                category: Category::Quick,
                name: t!("palette.quick_connect", target = target.display()),
                detail: SharedString::default(),
                target: Target::Quick(target),
            });
        }
        let candidates = self.candidates(scope, cx);
        let ranked = rank(&candidates, &words);
        entries.extend(
            ranked
                .into_iter()
                .take(MAX_RESULTS)
                .map(|ix| candidates[ix].clone()),
        );
        self.shown = entries;
        cx.notify();
    }

    /// 全部可选的条目（按类别排列：操作、主机、已打开的 Tab）。
    fn candidates(&self, scope: Option<Category>, cx: &App) -> Vec<Entry> {
        let wanted = |c: Category| scope.is_none_or(|s| s == c);
        let mut out = Vec::new();
        if wanted(Category::Op) {
            let current = self.current.as_ref().and_then(|p| p.upgrade());
            let has_terminal = current.is_some();
            let has_host = current
                .as_ref()
                .is_some_and(|p| p.read(cx).host().is_some());
            let has_ssh = current
                .as_ref()
                .is_some_and(|p| p.read(cx).sftp_target().is_some());
            // 有主机工具的终端（SSH、WSL，不是只用一个通道的）才能弹出概览
            let has_tools = current.as_ref().is_some_and(|p| {
                !matches!(
                    p.read(cx).tools_state(),
                    ToolsState::Unsupported | ToolsState::SingleChannel
                )
            });
            let (can_record, recording) = current.as_ref().map_or((false, false), |p| {
                let p = p.read(cx);
                (p.can_record(), p.is_recording())
            });
            let sidebar_hidden = AppState::global(cx).ui.read(cx).state().sidebar_hidden;
            let syncing = self.syncing;
            let tools_open = AppState::global(cx).ui.read(cx).state().host_tools_open;
            let bar_open = crate::snippets::bar::is_open(cx);
            let ops = [
                (Op::Duplicate, t!("palette.op.duplicate"), has_terminal),
                (Op::Reconnect, t!("palette.op.reconnect"), has_terminal),
                (
                    Op::ToggleRecording,
                    if recording {
                        t!("palette.op.stop_record")
                    } else {
                        t!("palette.op.start_record")
                    },
                    can_record,
                ),
                (Op::OpenRecording, t!("palette.op.open_recording"), true),
                (
                    Op::NewLocalTerminal,
                    t!("palette.op.new_local_terminal"),
                    true,
                ),
                (Op::NewHost, t!("palette.op.new_host"), true),
                (Op::NewGroup, t!("palette.op.new_group"), true),
                (
                    Op::ToggleSidebar,
                    if sidebar_hidden {
                        t!("palette.op.show_sidebar")
                    } else {
                        t!("palette.op.hide_sidebar")
                    },
                    true,
                ),
                (
                    Op::ToggleFullscreen,
                    t!("palette.op.toggle_fullscreen"),
                    true,
                ),
                (Op::NewWindow, t!("palette.op.new_window"), true),
                // 窗口里只有一个 Tab 时移到新窗口没有意义
                (
                    Op::MoveToNewWindow,
                    t!("palette.op.move_to_new_window"),
                    self.tabs.len() > 1,
                ),
                (Op::UiZoomIn, t!("palette.op.ui_zoom_in"), true),
                (Op::UiZoomOut, t!("palette.op.ui_zoom_out"), true),
                (Op::UiZoomReset, t!("palette.op.ui_zoom_reset"), true),
                (Op::ToggleTheme, t!("palette.op.toggle_theme"), true),
                (
                    Op::ToggleTabPosition,
                    t!("palette.op.toggle_tab_position"),
                    true,
                ),
                (
                    Op::FocusTabList,
                    t!("palette.op.focus_tab_list"),
                    Workspace::tabs_on_left(cx),
                ),
                (Op::ChangeScheme, t!("schemes.picker.command"), true),
                (Op::CommandPicker, t!("palette.op.command_picker"), true),
                (
                    Op::SaveWorkspace,
                    t!("palette.op.save_workspace"),
                    has_terminal,
                ),
                (Op::SftpBeside, t!("palette.op.sftp_beside"), has_ssh),
                (Op::RevealHost, t!("palette.op.reveal_host"), has_host),
                (
                    Op::MultiExec,
                    if syncing {
                        t!("palette.op.multi_exec_off")
                    } else {
                        t!("palette.op.multi_exec")
                    },
                    true,
                ),
                (Op::Compose, t!("palette.op.compose"), true),
                (
                    Op::HostTools,
                    if tools_open {
                        t!("palette.op.host_tools_hide")
                    } else {
                        t!("palette.op.host_tools")
                    },
                    true,
                ),
                (
                    Op::HostToolsOverview,
                    t!("palette.op.host_tools_overview"),
                    true,
                ),
                (
                    Op::HostToolsPopOut,
                    t!("palette.op.host_tools_popout"),
                    has_tools,
                ),
                (
                    Op::SnippetBar,
                    if bar_open {
                        t!("palette.op.snippet_bar_hide")
                    } else {
                        t!("palette.op.snippet_bar")
                    },
                    true,
                ),
                (Op::Forwards, t!("palette.op.forwards"), true),
                (Op::Highlight, t!("palette.op.highlight"), true),
                (Op::ImportHosts, t!("hosts.toolbar.import"), true),
                (Op::ExportHosts, t!("hosts.toolbar.export"), true),
                (Op::Audit, t!("palette.op.audit"), true),
                (
                    Op::LockNow,
                    t!("palette.op.lock_now"),
                    crate::lock_ui::is_enabled(cx),
                ),
                (Op::Settings, t!("settings.open"), true),
                (Op::Shortcuts, t!("palette.op.shortcuts"), true),
                (Op::Toolbar, t!("toolbar.customize"), true),
            ];
            // 界面大小的几项在说明中写上现在的大小，Tab 的位置写上现在在哪里
            let scale = t!(
                "palette.op.ui_scale_detail",
                percent = ui_scale::current_percent(cx)
            );
            let tab_position = if Workspace::tabs_on_left(cx) {
                t!("palette.op.tab_position_left")
            } else {
                t!("palette.op.tab_position_top")
            };
            out.extend(
                ops.into_iter()
                    .filter(|(_, _, ok)| *ok)
                    .map(|(op, name, _)| Entry {
                        category: Category::Op,
                        name,
                        detail: match op {
                            Op::UiZoomIn | Op::UiZoomOut | Op::UiZoomReset => scale.clone(),
                            Op::ToggleTabPosition => tab_position.clone(),
                            _ => SharedString::default(),
                        },
                        target: Target::Op(op),
                    }),
            );
        }
        if wanted(Category::Host) {
            out.extend(AppState::store(cx).list::<Host>().into_iter().map(|h| {
                let detail = if h.doc.username.is_empty() {
                    h.doc.address.clone()
                } else {
                    format!("{}@{}", h.doc.username, h.doc.address)
                };
                Entry {
                    category: Category::Host,
                    name: h.doc.name.into(),
                    detail: detail.into(),
                    target: Target::Host(h.id),
                }
            }));
        }
        if wanted(Category::Workspace) {
            out.extend(
                AppState::store(cx)
                    .list::<SavedWorkspace>()
                    .into_iter()
                    .map(|w| Entry {
                        category: Category::Workspace,
                        name: w.doc.name.clone().into(),
                        detail: t!("workspace.panes", count = w.doc.hosts().len()),
                        target: Target::Workspace(w.id),
                    }),
            );
        }
        if wanted(Category::Tab) {
            out.extend(self.tabs.iter().filter_map(|tab| {
                let (name, detail) = match tab {
                    TabRef::Terminal(p) => (
                        p.upgrade()?.read(cx).name().clone(),
                        t!("palette.tab_detail"),
                    ),
                    TabRef::Sftp(p) => (
                        p.upgrade()?.read(cx).name().clone(),
                        t!("palette.tab_detail_sftp"),
                    ),
                    TabRef::Editor(p) => {
                        let path = p.upgrade()?.read(cx).remote_path().to_string();
                        (
                            termlet_core::sftp::path::base_name(&path).into(),
                            t!("palette.tab_detail_editor"),
                        )
                    }
                    // 与 Tab 上的标题相同（“日志 · demo-app”）
                    TabRef::Logs(p) => (
                        p.upgrade()?.read(cx).title_text(),
                        t!("palette.tab_detail_logs"),
                    ),
                    TabRef::Replay(p) => (
                        p.upgrade()?.read(cx).title_text(),
                        t!("palette.tab_detail_replay"),
                    ),
                };
                Some(Entry {
                    category: Category::Tab,
                    name,
                    detail,
                    target: Target::Tab(tab.clone()),
                })
            }));
        }
        out
    }

    fn run(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.shown.get(ix).cloned() else {
            return;
        };
        window.close_dialog(cx);
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        tracing::debug!(entry = %entry.name, "命令面板：执行");
        match entry.target {
            Target::Quick(target) => workspace.update(cx, |w, cx| w.open_quick(target, window, cx)),
            Target::Host(id) => workspace.update(cx, |w, cx| w.open_host(&id, window, cx)),
            Target::Tab(TabRef::Terminal(panel)) => {
                if let Some(panel) = panel.upgrade() {
                    workspace.update(cx, |w, cx| w.show_terminal(&panel, window, cx));
                }
            }
            Target::Tab(TabRef::Sftp(panel)) => {
                if let Some(panel) = panel.upgrade() {
                    workspace.update(cx, |w, cx| w.show_sftp(&panel, window, cx));
                }
            }
            Target::Tab(TabRef::Editor(panel)) => {
                if let Some(panel) = panel.upgrade() {
                    workspace.update(cx, |w, cx| w.show_editor(&panel, window, cx));
                }
            }
            Target::Tab(TabRef::Logs(panel)) => {
                if let Some(panel) = panel.upgrade() {
                    workspace.update(cx, |w, cx| w.show_logs(&panel, window, cx));
                }
            }
            Target::Tab(TabRef::Replay(panel)) => {
                if let Some(panel) = panel.upgrade() {
                    workspace.update(cx, |w, cx| w.show_replay(&panel, window, cx));
                }
            }
            Target::Workspace(id) => {
                workspace.update(cx, |w, cx| w.open_saved_workspace(&id, window, cx))
            }
            Target::Op(op) => match op {
                Op::NewHost => host_dialog::open_new(None, window, cx),
                Op::NewGroup => group_dialog::open_new(None, window, cx),
                Op::OpenRecording => workspace.update(cx, |w, cx| w.pick_recording(window, cx)),
                Op::ToggleTheme => workspace.update(cx, |w, cx| w.toggle_theme(window, cx)),
                Op::ToggleTabPosition => {
                    workspace.update(cx, |w, cx| w.toggle_tab_position(window, cx))
                }
                Op::SnippetBar => crate::snippets::bar::toggle(cx),
                // 在打开面板时焦点所在的终端上预览；没有终端时只改设置
                Op::ChangeScheme => schemes::picker::open(self.current.clone(), window, cx),
                Op::Settings => settings::open(settings::Page::General, window, cx),
                Op::ImportHosts => crate::backup_ui::open_import(window, cx),
                Op::ExportHosts => crate::backup_ui::open_export(Vec::new(), window, cx),
                Op::Audit => crate::cli_ui::open_audit(window, cx),
                Op::LockNow => {
                    window.close_dialog(cx);
                    crate::lock_ui::lock_now(cx);
                }
                Op::Shortcuts => settings::open(settings::Page::Shortcuts, window, cx),
                Op::Highlight => settings::open(settings::Page::Highlight, window, cx),
                Op::Toolbar => settings::open(settings::Page::Toolbar, window, cx),
                Op::Forwards => workspace.update(cx, |w, cx| w.show_forwards(window, cx)),
                Op::UiZoomIn => ui_scale::zoom(ui_scale::Step::In, window, cx),
                Op::UiZoomOut => ui_scale::zoom(ui_scale::Step::Out, window, cx),
                Op::UiZoomReset => ui_scale::zoom(ui_scale::Step::Reset, window, cx),
                // 其余的是有快捷键的动作：交给当前终端（或工作区）去处理
                _ => {
                    if let Some(action) = op.action() {
                        if let Some(panel) = self.current.as_ref().and_then(|p| p.upgrade()) {
                            workspace.update(cx, |w, cx| w.show_terminal(&panel, window, cx));
                        }
                        window.dispatch_action(action, cx);
                    }
                }
            },
        }
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let items = self.shown.iter().map(|entry| {
            let entry = entry.clone();
            CommandItem::new()
                .label(entry.name.clone())
                .child(move |window, cx| {
                    let theme = cx.theme();
                    let (icon, color, category) = match entry.category {
                        Category::Quick => {
                            (IconName::Zap, theme.success, t!("palette.category.quick"))
                        }
                        Category::Op => (IconName::Zap, theme.warning, t!("palette.category.op")),
                        Category::Host => (
                            IconName::Server,
                            theme.muted_foreground,
                            t!("palette.category.host"),
                        ),
                        Category::Tab => (
                            IconName::SquareTerminal,
                            theme.muted_foreground,
                            t!("palette.category.tab"),
                        ),
                        Category::Workspace => (
                            IconName::LayoutDashboard,
                            theme.muted_foreground,
                            t!("palette.category.workspace"),
                        ),
                    };
                    let shortcut = match &entry.target {
                        Target::Op(op) => op.action().and_then(|action| {
                            Kbd::binding_for_action(action.as_ref(), None, window)
                        }),
                        _ => None,
                    };
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(Icon::new(icon).small().text_color(color))
                        .child(div().flex_none().text_sm().child(entry.name.clone()))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(entry.detail.clone()),
                        )
                        .children(shortcut)
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(category),
                        )
                })
        });
        let this = cx.entity().downgrade();
        let on_query = this.clone();
        // Esc（查询为空时）由承载面板的对话框关闭
        let command = Command::new(&self.state)
            .placeholder(t!("palette.placeholder"))
            .filterable(false)
            .bordered(false)
            .max_h(rems(24.))
            .items(items)
            .empty(|_, _, cx| {
                div()
                    .p_4()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(t!("palette.empty"))
            })
            .footer(|_, _, cx| {
                div()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(prose(t!("palette.hint")))
            })
            // 只在查询变化时重新列出（选择的移动不会打乱高亮）
            .on_query(move |_, _, cx| {
                let _ = on_query.update(cx, |this, cx| this.refresh(cx));
            })
            .on_confirm(move |ix, window, cx| {
                let _ = this.update(cx, |this, cx| this.run(ix.row, window, cx));
            });
        div().id("command-palette").test_support().child(command)
    }
}

/// 拆出前缀与搜索词。
fn parse_query(query: &str) -> (Option<Category>, Vec<String>) {
    let query = query.trim_start();
    let (scope, rest) = match query.chars().next() {
        Some('>') => (Some(Category::Op), &query[1..]),
        Some('@') => (Some(Category::Tab), &query[1..]),
        Some('#') => (Some(Category::Host), &query[1..]),
        _ => (None, query),
    };
    let words = rest.split_whitespace().map(str::to_lowercase).collect();
    (scope, words)
}

/// 按搜索词给条目排序，返回下标；没有搜索词时保持原来的顺序。
fn rank(entries: &[Entry], words: &[String]) -> Vec<usize> {
    if words.is_empty() {
        return (0..entries.len()).collect();
    }
    let mut scored: Vec<(usize, u32)> = entries
        .iter()
        .enumerate()
        .filter_map(|(ix, entry)| {
            let name = entry.name.to_lowercase();
            let detail = entry.detail.to_lowercase();
            let mut score = 0;
            for word in words {
                score += if name.starts_with(word.as_str()) {
                    3
                } else if name.contains(word.as_str()) {
                    2
                } else if detail.contains(word.as_str()) {
                    1
                } else {
                    return None;
                };
            }
            Some((ix, score))
        })
        .collect();
    // 稳定排序：同分保持原来的顺序
    scored.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
    scored.into_iter().map(|(ix, _)| ix).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, detail: &str) -> Entry {
        Entry {
            category: Category::Host,
            name: name.to_string().into(),
            detail: detail.to_string().into(),
            target: Target::Op(Op::NewHost),
        }
    }

    #[test]
    fn prefixes_select_a_category() {
        assert!(matches!(parse_query("> new").0, Some(Category::Op)));
        assert!(matches!(parse_query("@web").0, Some(Category::Tab)));
        assert!(matches!(parse_query("#db prod").0, Some(Category::Host)));
        let (scope, words) = parse_query("  Web  Prod ");
        assert!(scope.is_none());
        assert_eq!(words, ["web", "prod"]);
    }

    #[test]
    fn ranking_prefers_name_prefix_then_name_then_detail() {
        let entries = vec![
            entry("db-web", "root@10.0.0.2"),
            entry("web-1", "root@10.0.0.1"),
            entry("cache", "web@10.0.0.3"),
            entry("other", "root@10.0.0.4"),
        ];
        assert_eq!(rank(&entries, &["web".into()]), [1, 0, 2]);
        // 每个词都要出现
        assert_eq!(rank(&entries, &["web".into(), "10.0.0.1".into()]), [1]);
        assert_eq!(rank(&entries, &[]), [0, 1, 2, 3]);
    }
}
