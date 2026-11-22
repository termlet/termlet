//! 文件列表（远程栏与本机栏共用）：固定表头、虚拟滚动、多选、键盘、右键菜单、拖放。
//!
//! - 排序固定为目录在前、名称自然排序；每次换目录后清空选择。
//! - 次要的列（[`layout_columns`]）：名称列至少留 [`NAME_MIN`]，修改时间、大小、权限按这个顺序放得下才显示（变窄时
//!   先藏权限、再藏大小，最后才藏时间）；时间一律用紧凑的写法（今年的 `11-03 14:22`，往年的 `2025-11-03`），两栏
//!   一致，悬停提示完整到秒。表头的菜单（名称列右端的按钮、表头上右键）勾选显示哪些列，记在本机的界面状态中
//!   （远程栏、本机栏共用）；勾选了但放不下的列在菜单中说明。
//! - 不在根目录时最上面有“..”一行：双击回到上一级，文件可以拖到它上面放进上一级；它不能选中，不参与操作。
//! - 隐藏的文件（远程是以 `.` 开头的，本机是 Windows 的隐藏属性）默认显示、颜色淡一些；栏上的小眼睛或
//!   Ctrl+Alt+H 切换是否显示（远程栏、本机栏各一个开关，所有 SFTP 面板相同，记在界面状态中）。
//! - 图标按设置“通用”中的“图标”：按名称与类型区分的彩色图标（见 [`super::file_icons`]），或单色的文件夹 / 文件。
//! - 单击单选，Ctrl 单击增减，Shift 单击连续选择；右键或拖动没有选中的行时只选它。
//! - 键盘（焦点在列表上，按资源管理器的习惯；列表有焦点时当前行有一圈焦点框）：
//!   - ↑ ↓、PageUp / PageDown、Home / End 移动当前行并只选中它，加 Shift 从锚点连续选择；Ctrl+↑ ↓ 只移动
//!     当前行，Ctrl+空格增减它。当前行可以停在“..”上（不选中任何项），在它上面按 Enter 回到上一级。
//!   - 直接输入名称的开头跳到那一项（1 秒内连续输入算一次；连按同一个字母在以它开头的项之间轮换）。
//!   - Enter 打开、Backspace 上级目录、F2 重命名、F5 刷新、F7 新建文件夹、Delete 删除、Ctrl+A 全选、
//!     Ctrl+C 复制所选的路径、Ctrl+V 上传资源管理器中复制的文件（只有远程栏）、Ctrl+Alt+H 显示 / 隐藏隐藏的文件、
//!     Shift+F10 或菜单键在当前行旁边打开右键菜单、Ctrl+L 或 Alt+D 编辑路径（路径框中 Esc 回到列表）、
//!     Tab 在本机栏与远程栏之间切换（由 SFTP 面板处理）。
//!   - 回到上一级目录后选中刚才所在的目录；当前行被删掉（或不再显示）时留在原来的位置。
//! - 拖放：拖出所选的项；放在目录行上为放进这个目录，放在“..”上为放进上一级，放在别处为放进当前目录。

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input as text_input;
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _, VirtualListScrollHandle, h_flex, v_flex, v_virtual_list,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, ClickEvent, Context, DismissEvent, Entity, EntityId,
    EventEmitter, ExternalPaths, FocusHandle, Focusable, InteractiveElement as _, IntoElement,
    KeyBinding, KeyDownEvent, MouseButton, ParentElement as _, Pixels, Render, ScrollStrategy,
    SharedString, Size, StatefulInteractiveElement as _, Styled as _, Subscription,
    TestSupportExt as _, Window, actions, anchored, canvas, deferred, div, img,
    prelude::FluentBuilder as _, px, rems, size,
};
use termlet_core::model::IconStyle;
use termlet_core::time::DateStyle;
use termlet_core::ui_state::SftpColumns;

use super::file_icons::{self, IconKind};
use super::list_model::{self, ClickMods, KeyMove, Selection};
use crate::app_state::AppState;
use crate::prose::prose;
use crate::t;

actions!(
    sftp_list,
    [
        /// 进入目录，或打开文件（远程为在线编辑，本机为用默认程序打开）
        OpenSelected,
        GoUp,
        RenameSelected,
        Refresh,
        DeleteSelected,
        SelectAll,
        /// 复制所选项的完整路径（每行一个）
        CopyPaths,
        /// 上传资源管理器中复制的文件（远程栏）
        PasteFiles,
        /// 显示 / 隐藏隐藏的文件（这一栏的开关）
        ToggleHidden,
        /// 当前行移动并只选中到达的项
        SelectPrev,
        SelectNext,
        SelectPageUp,
        SelectPageDown,
        SelectFirst,
        SelectLast,
        /// 当前行移动并从锚点连续选择（加 Shift）
        ExtendPrev,
        ExtendNext,
        ExtendPageUp,
        ExtendPageDown,
        ExtendFirst,
        ExtendLast,
        /// 只移动当前行（加 Ctrl）
        CursorPrev,
        CursorNext,
        /// 增减当前行（Ctrl+空格）
        ToggleCursor,
        /// 在当前行旁边打开右键菜单（Shift+F10、菜单键）
        OpenMenu,
        NewFolder,
        /// 编辑这一栏的路径
        FocusPath,
        /// 在本机栏与远程栏之间切换（由 SFTP 面板处理；只有一栏时交给默认的焦点切换）
        SwitchPane,
        /// 从路径框回到列表，放弃没有提交的输入（Esc）
        FocusList,
    ]
);

const KEY_CONTEXT: &str = "SftpFileList";
/// 路径框外面的容器的上下文（其中按 Esc 回到列表）。
pub const PATH_CONTEXT: &str = "SftpPath";

/// “..”一行的名称；拖到它上面时 [`FileListEvent::Drop`] 的 `into` 就是它（放进上一级）。
pub const PARENT: &str = "..";

/// 行高与表头的高度（rem）。
const ROW_HEIGHT: f32 = 1.75;
/// 按名字跳转时，相隔这么久以内的输入算作一次。
const TYPE_AHEAD_TIMEOUT: Duration = Duration::from_secs(1);

/// 列表的快捷键是固定的（设置的“快捷键”页中列出，不能修改）。在列表的上下文中，比窗口一级的自定义快捷键优先。
/// 按当前系统的习惯注册（[`key_bindings`]）。
pub fn init(cx: &mut App) {
    cx.bind_keys(key_bindings(cx.platform_info().os));
}

/// 显示 / 隐藏隐藏文件的快捷键：Windows、Linux 上照 WinSCP 用 Ctrl+Alt+H，macOS 上照 Finder 用 ⇧⌘.（GPUI 报告为
/// `cmd->`，显示为 ⌘>）。
pub fn toggle_hidden_combo(os: gpui_kit::OperatingSystem) -> &'static str {
    if os == gpui_kit::OperatingSystem::Mac {
        "cmd->"
    } else {
        "ctrl-alt-h"
    }
}

/// 提示中显示 / 隐藏隐藏文件的快捷键（`Ctrl+Alt+H`、`⌘>`）。
fn toggle_hidden_label(cx: &App) -> String {
    let os = cx.platform_info().os;
    termlet_core::keymap::label_of(toggle_hidden_combo(os), crate::keymap::key_os(os))
}

/// 列表的快捷键：`os` 为按哪个系统的习惯（`KeyBinding::new` 不认 `secondary-`，会 panic，分开写）。
///
/// - Windows、Linux：照资源管理器、WinSCP（Ctrl+A / C / V、Ctrl+Alt+H、Ctrl+↑ ↓ 移动焦点、Ctrl+L / Alt+D 地址栏）。
/// - macOS：照 Finder（⌘A / C / V、⇧⌘. 隐藏文件、⌘↑ 上一级、⌘↓ 打开、⌘⌫ 删除、⇧⌘N 新建文件夹、⌘R 刷新、⌘L 地址栏），
///   Ctrl+↑ ↓ 是系统的调度中心，移动焦点改用 ⌥↑ ⌥↓、⌥Space。
pub fn key_bindings(os: gpui_kit::OperatingSystem) -> Vec<KeyBinding> {
    let ctx = Some(KEY_CONTEXT);
    let mut bindings = vec![
        KeyBinding::new("enter", OpenSelected, ctx),
        KeyBinding::new("backspace", GoUp, ctx),
        KeyBinding::new("f2", RenameSelected, ctx),
        KeyBinding::new("f5", Refresh, ctx),
        KeyBinding::new("delete", DeleteSelected, ctx),
        KeyBinding::new(toggle_hidden_combo(os), ToggleHidden, ctx),
        KeyBinding::new("up", SelectPrev, ctx),
        KeyBinding::new("down", SelectNext, ctx),
        KeyBinding::new("pageup", SelectPageUp, ctx),
        KeyBinding::new("pagedown", SelectPageDown, ctx),
        KeyBinding::new("home", SelectFirst, ctx),
        KeyBinding::new("end", SelectLast, ctx),
        KeyBinding::new("shift-up", ExtendPrev, ctx),
        KeyBinding::new("shift-down", ExtendNext, ctx),
        KeyBinding::new("shift-pageup", ExtendPageUp, ctx),
        KeyBinding::new("shift-pagedown", ExtendPageDown, ctx),
        KeyBinding::new("shift-home", ExtendFirst, ctx),
        KeyBinding::new("shift-end", ExtendLast, ctx),
        KeyBinding::new("shift-f10", OpenMenu, ctx),
        KeyBinding::new("menu", OpenMenu, ctx),
        // WinSCP、Total Commander 的新建文件夹（资源管理器的 Ctrl+Shift+N 是“新窗口”）
        KeyBinding::new("f7", NewFolder, ctx),
        // WinSCP、FileZilla 的切换左右两栏
        KeyBinding::new("tab", SwitchPane, ctx),
        KeyBinding::new("shift-tab", SwitchPane, ctx),
        // 输入框自己用不到 Esc 时才交给这里
        KeyBinding::new("escape", FocusList, Some(PATH_CONTEXT)),
    ];
    if os == gpui_kit::OperatingSystem::Mac {
        bindings.extend([
            KeyBinding::new("cmd-a", SelectAll, ctx),
            KeyBinding::new("cmd-c", CopyPaths, ctx),
            KeyBinding::new("cmd-v", PasteFiles, ctx),
            KeyBinding::new("cmd-up", GoUp, ctx),
            KeyBinding::new("cmd-down", OpenSelected, ctx),
            KeyBinding::new("cmd-backspace", DeleteSelected, ctx),
            KeyBinding::new("cmd-shift-n", NewFolder, ctx),
            KeyBinding::new("cmd-r", Refresh, ctx),
            KeyBinding::new("cmd-l", FocusPath, ctx),
            KeyBinding::new("alt-up", CursorPrev, ctx),
            KeyBinding::new("alt-down", CursorNext, ctx),
            KeyBinding::new("alt-space", ToggleCursor, ctx),
        ]);
    } else {
        bindings.extend([
            KeyBinding::new("ctrl-a", SelectAll, ctx),
            KeyBinding::new("ctrl-c", CopyPaths, ctx),
            KeyBinding::new("ctrl-v", PasteFiles, ctx),
            KeyBinding::new("ctrl-up", CursorPrev, ctx),
            KeyBinding::new("ctrl-down", CursorNext, ctx),
            KeyBinding::new("ctrl-space", ToggleCursor, ctx),
            // 资源管理器的地址栏
            KeyBinding::new("ctrl-l", FocusPath, ctx),
            KeyBinding::new("alt-d", FocusPath, ctx),
        ]);
    }
    bindings
}

/// 键盘移动当前行的幅度。
#[derive(Clone, Copy)]
enum Step {
    Prev,
    Next,
    PageUp,
    PageDown,
    First,
    Last,
}

/// 这一栏是否不显示隐藏的文件。
pub fn hides_hidden(source: &DragSource, cx: &App) -> bool {
    let state = AppState::global(cx).ui.read(cx).state();
    match source {
        DragSource::Local => state.sftp_local_hide_hidden,
        DragSource::Remote(_) => state.sftp_remote_hide_hidden,
    }
}

/// 栏上的小眼睛：图标是现在的样子（睁着眼为显示着隐藏的文件），提示是点了会怎样。
pub fn hidden_files_button(id: &'static str, source: DragSource, cx: &App) -> Button {
    let hidden = hides_hidden(&source, cx);
    Button::new(id)
        .ghost()
        .small()
        .icon(if hidden {
            IconName::EyeOff
        } else {
            IconName::Eye
        })
        .tooltip({
            let key = toggle_hidden_label(cx);
            if hidden {
                t!("sftp.show_hidden", key = key)
            } else {
                t!("sftp.hide_hidden", key = key)
            }
        })
        .on_click(move |_, _, cx| toggle_hidden(&source, cx))
}

/// 切换这一栏是否显示隐藏的文件（所有 SFTP 面板的这一栏一起变）。
pub fn toggle_hidden(source: &DragSource, cx: &mut App) {
    let local = matches!(source, DragSource::Local);
    tracing::debug!(local, "SFTP 切换显示隐藏的文件");
    AppState::ui(cx).update(cx, |ui, cx| {
        ui.update(
            |s| {
                if local {
                    s.sftp_local_hide_hidden = !s.sftp_local_hide_hidden;
                } else {
                    s.sftp_remote_hide_hidden = !s.sftp_remote_hide_hidden;
                }
            },
            cx,
        )
    });
}

/// 列表中的一项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRow {
    pub name: String,
    /// 目录，或指向目录的符号链接
    pub is_dir: bool,
    pub is_link: bool,
    /// 隐藏的文件（远程：以 `.` 开头；本机：Windows 的隐藏属性，其他系统以 `.` 开头）
    pub hidden: bool,
    pub size: u64,
    /// 修改时间（Unix 毫秒）；不知道时为 `None`
    pub modified: Option<i64>,
    /// 权限的写法（如 `drwxr-xr-x`），只有远程有
    pub mode: Option<String>,
}

/// 拖动的是哪里的文件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DragSource {
    Local,
    /// 远程栏（所在的 SFTP 面板）
    Remote(EntityId),
}

/// 拖动中的文件：来源、所在目录与名称。
#[derive(Clone, Debug)]
pub struct DraggedFiles {
    pub source: DragSource,
    pub dir: String,
    pub names: Vec<String>,
}

/// 列表请求所在的栏做的事。
#[derive(Clone, Debug)]
pub enum FileListEvent {
    /// 双击或按 Enter（恰好选中一项时）
    Open(FileRow),
    Up,
    /// 重命名这一项（F2）
    Rename(FileRow),
    Refresh,
    /// 删除所选的项（Delete）
    Delete,
    CopyPaths,
    PasteFiles,
    /// 列表中的文件拖进来；`into` 为放在哪个目录行上（`None` 为当前目录，[`PARENT`] 为上一级）
    Drop {
        files: DraggedFiles,
        into: Option<String>,
    },
    /// 从资源管理器拖进来
    DropExternal {
        paths: Vec<PathBuf>,
        into: Option<String>,
    },
    /// 新建文件夹（F7）
    NewFolder,
    /// 编辑这一栏的路径（Ctrl+L、Alt+D）
    FocusPath,
}

type MenuBuilder = dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu;

/// 列宽与间距（rem）：行左右各 0.75（`px_3`），列之间 0.5（`gap_2`）；名称列（含图标）至少留 [`NAME_MIN`]。
pub(crate) const NAME_MIN: f32 = 10.;
const ROW_PADDING: f32 = 1.5;
const COLUMN_GAP: f32 = 0.5;
const SIZE_WIDTH: f32 = 6.;
/// 修改时间：紧凑的写法（`11-03 14:22`、`2025-11-03`、`Nov 3, 2025`）
const TIME_WIDTH: f32 = 6.;
const MODE_WIDTH: f32 = 7.;

/// 显示哪些次要的列。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ColumnLayout {
    pub size: bool,
    pub modified: bool,
    pub mode: bool,
}

/// 列表宽 `width`（rem）时显示哪些列：名称列至少留 [`NAME_MIN`]，勾选了的列按修改时间、大小、权限的顺序放得下才
/// 显示（权限只在有权限信息时），余下的宽度都给名称列。
pub(crate) fn layout_columns(width: f32, chosen: SftpColumns, has_mode: bool) -> ColumnLayout {
    let mut room = width - ROW_PADDING - NAME_MIN;
    let mut fits = |wanted: bool, column: f32| {
        let fit = wanted && room >= COLUMN_GAP + column;
        if fit {
            room -= COLUMN_GAP + column;
        }
        fit
    };
    let modified = fits(chosen.modified, TIME_WIDTH);
    let size = fits(chosen.size, SIZE_WIDTH);
    let mode = fits(chosen.mode && has_mode, MODE_WIDTH);
    ColumnLayout {
        size,
        modified,
        mode,
    }
}

/// 次要的列（表头菜单中的一项）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Column {
    Size,
    Modified,
    Mode,
}

impl Column {
    const ALL: [Column; 3] = [Column::Size, Column::Modified, Column::Mode];

    fn name(self) -> SharedString {
        match self {
            Column::Size => t!("sftp.column.size"),
            Column::Modified => t!("sftp.column.modified"),
            Column::Mode => t!("sftp.column.mode"),
        }
    }

    fn chosen(self, columns: &SftpColumns) -> bool {
        match self {
            Column::Size => columns.size,
            Column::Modified => columns.modified,
            Column::Mode => columns.mode,
        }
    }

    fn shown(self, layout: &ColumnLayout) -> bool {
        match self {
            Column::Size => layout.size,
            Column::Modified => layout.modified,
            Column::Mode => layout.mode,
        }
    }
}

/// 勾选、取消勾选一列（记进界面状态，所有的文件列表一起变）。
pub(crate) fn toggle_column(column: Column, cx: &mut App) {
    AppState::ui(cx).update(cx, |ui, cx| {
        ui.update(
            |s| {
                let c = &mut s.sftp_columns;
                match column {
                    Column::Size => c.size = !c.size,
                    Column::Modified => c.modified = !c.modified,
                    Column::Mode => c.mode = !c.mode,
                }
            },
            cx,
        )
    });
}

/// 勾选了但这个宽度放不下的列（权限只在有权限信息时算）。
fn squeezed(chosen: &SftpColumns, layout: &ColumnLayout, has_mode: bool) -> Vec<Column> {
    Column::ALL
        .into_iter()
        .filter(|c| c.chosen(chosen) && !c.shown(layout) && (*c != Column::Mode || has_mode))
        .collect()
}

/// 表头的菜单：勾选显示的列；有勾选了但放不下的列时在最下面说明。
fn column_menu(menu: PopupMenu, chosen: SftpColumns, squeezed: &[Column]) -> PopupMenu {
    let menu = Column::ALL.into_iter().fold(menu, |menu, column| {
        menu.item(
            PopupMenuItem::new(column.name())
                .checked(column.chosen(&chosen))
                .on_click(move |_, _, cx| toggle_column(column, cx)),
        )
    });
    if squeezed.is_empty() {
        return menu;
    }
    menu.separator()
        .item(PopupMenuItem::label(squeezed_text(squeezed)))
}

fn squeezed_text(squeezed: &[Column]) -> SharedString {
    let names: Vec<SharedString> = squeezed.iter().map(|c| c.name()).collect();
    t!(
        "sftp.columns.squeezed",
        columns = names.join(&t!("sftp.columns.list_separator"))
    )
}

/// 日期的写法跟随界面语言（修改时间；主机工具中服务失败的时间等也用它）。
pub(crate) fn date_style() -> DateStyle {
    match crate::i18n::language() {
        crate::i18n::Language::ZhCn | crate::i18n::Language::Ko => DateStyle::Dash,
        crate::i18n::Language::ZhTw | crate::i18n::Language::Ja => DateStyle::Slash,
        crate::i18n::Language::En => DateStyle::English,
    }
}

pub struct FileList {
    /// 元素 ID 的前缀（远程栏与本机栏的行区分开）
    id: &'static str,
    /// 目录中的全部项（含隐藏的），已排序
    all_rows: Vec<FileRow>,
    /// 显示的项：`all_rows` 去掉不显示的隐藏文件（不含“..”）
    rows: Vec<FileRow>,
    /// 不显示隐藏的文件（跟随界面状态中这一栏的开关）
    hide_hidden: bool,
    /// 最上面有“..”一行（不在根目录时）
    parent: bool,
    /// 这一帧的图标风格（渲染开始时从设置读一次）
    icon_style: IconStyle,
    /// 有没有行带权限（是否显示权限列）；内容变化时计算，不在每次渲染时扫描
    has_mode: bool,
    /// 虚拟列表的行高表（行高、行数不变时沿用，不在每次渲染时按全部行数分配）
    sizes: Option<(Pixels, Rc<Vec<Size<Pixels>>>)>,
    selection: Selection,
    focus: FocusHandle,
    scroll: VirtualListScrollHandle,
    /// 上一帧的宽度（决定显示哪些列）
    width: Rc<Cell<Pixels>>,
    /// 上一帧的高度（PageUp / PageDown 一次移动多少行）
    height: Rc<Cell<Pixels>>,
    /// 按名字跳转时已经输入的文字与最后一次输入的时间
    type_ahead: Option<(String, Instant)>,
    /// 用键盘打开的右键菜单（画在当前行的下面）
    key_menu: Option<(Entity<PopupMenu>, Subscription)>,
    source: DragSource,
    dir: String,
    empty_text: SharedString,
    menu: Option<Rc<MenuBuilder>>,
    /// 不可用（远程断开、重连中）：没有右键菜单，不接收拖放
    disabled: bool,
    /// 接收从资源管理器拖进来的文件（远程栏）
    accept_external: bool,
    /// 只接收从这里拖来的文件（本机栏只接收同一个面板的远程栏）；`None` 为另一边的都接收
    peer: Option<DragSource>,
    /// 勾选了显示的列（界面状态中的，见 [`layout_columns`]）
    chosen: SftpColumns,
    _ui: Subscription,
}

impl EventEmitter<FileListEvent> for FileList {}

impl Focusable for FileList {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl FileList {
    pub fn new(
        id: &'static str,
        source: DragSource,
        empty_text: SharedString,
        cx: &mut Context<Self>,
    ) -> Self {
        // 小眼睛的开关、显示的列在界面状态中：别的面板（或别的窗口）切换时这里一起变
        let ui = cx.observe(&AppState::ui(cx), |this: &mut Self, ui, cx| {
            this.sync_hidden(cx);
            let chosen = ui.read(cx).state().sftp_columns;
            if chosen != this.chosen {
                this.chosen = chosen;
                cx.notify();
            }
        });
        let chosen = AppState::ui(cx).read(cx).state().sftp_columns;
        Self {
            id,
            all_rows: Vec::new(),
            rows: Vec::new(),
            hide_hidden: hides_hidden(&source, cx),
            parent: false,
            icon_style: IconStyle::default(),
            has_mode: false,
            sizes: None,
            selection: Selection::default(),
            focus: cx.focus_handle(),
            scroll: VirtualListScrollHandle::new(),
            width: Rc::new(Cell::new(px(0.))),
            height: Rc::new(Cell::new(px(0.))),
            type_ahead: None,
            key_menu: None,
            source: source.clone(),
            dir: String::new(),
            empty_text,
            menu: None,
            disabled: false,
            accept_external: matches!(source, DragSource::Remote(_)),
            peer: None,
            chosen,
            _ui: ui,
        }
    }

    /// 右键菜单（打开时读取所在栏当时的状态）。
    pub fn set_menu(
        &mut self,
        menu: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) {
        self.menu = Some(Rc::new(menu));
    }

    /// 换成另一个目录的内容：排序，清空选择，回到顶部。`parent` 为不在根目录（最上面显示“..”）。
    /// 回到上一级（或更上面）时选中刚才所在的那个目录，滚动到它。
    pub fn set_entries(
        &mut self,
        dir: String,
        mut rows: Vec<FileRow>,
        parent: bool,
        cx: &mut Context<Self>,
    ) {
        list_model::sort_entries(&mut rows, |r| (r.is_dir, r.name.as_str()));
        let changed = dir != self.dir;
        let came_from = list_model::child_toward(&dir, &self.dir, self.separators());
        self.all_rows = rows;
        self.parent = parent;
        self.apply_filter();
        self.dir = dir;
        self.selection.clear();
        self.type_ahead = None;
        match came_from.and_then(|name| self.display_index(&name).map(|ix| (name, ix))) {
            Some((name, ix)) => {
                self.selection.only(&name);
                self.scroll.scroll_to_item(ix, ScrollStrategy::Center);
            }
            None if changed => self.scroll.scroll_to_item(0, ScrollStrategy::Top),
            None => {}
        }
        cx.notify();
    }

    /// 路径的分隔符：远程只有 `/`（名称中可以有 `\`），本机两种都认。
    fn separators(&self) -> &'static [char] {
        match self.source {
            DragSource::Local => &['\\', '/'],
            DragSource::Remote(_) => &['/'],
        }
    }

    /// 同一目录刷新后的内容：保留仍然存在的选择。
    pub fn refresh_entries(&mut self, mut rows: Vec<FileRow>, cx: &mut Context<Self>) {
        list_model::sort_entries(&mut rows, |r| (r.is_dir, r.name.as_str()));
        self.all_rows = rows;
        self.apply_filter();
        cx.notify();
    }

    /// 界面状态中这一栏的开关变了：重新筛选，不再显示的项从选择中去掉。
    fn sync_hidden(&mut self, cx: &mut Context<Self>) {
        let hide = hides_hidden(&self.source, cx);
        if hide != self.hide_hidden {
            self.hide_hidden = hide;
            self.apply_filter();
            cx.notify();
        }
    }

    /// 按开关从全部项中取出显示的项；选择只保留仍然显示的。当前行不在了（删掉、改名、不再显示）时留在原来的
    /// 位置；选择因此空了时选中它（连续按 Delete 一项项删，资源管理器的做法）。
    fn apply_filter(&mut self) {
        let old_cursor = self.cursor_index();
        let hide = self.hide_hidden;
        let rows: Vec<FileRow> = self
            .all_rows
            .iter()
            .filter(|r| !(hide && r.hidden))
            .cloned()
            .collect();
        self.selection
            .selected
            .retain(|name| rows.iter().any(|r| &r.name == name));
        self.has_mode = rows.iter().any(|r| r.mode.is_some());
        self.rows = rows;
        if self.selection.cursor.is_some() && self.cursor_index().is_none() {
            let len = self.display_len();
            let cursor = old_cursor
                .filter(|_| len > 0)
                .and_then(|ix| self.display_name(ix.min(len - 1)))
                .map(str::to_string);
            self.selection.cursor = None;
            if let Some(name) = cursor {
                if self.selection.selected.is_empty() && name != PARENT {
                    self.selection.only(&name);
                } else {
                    self.selection.cursor = Some(name);
                }
            }
        }
        if self
            .sizes
            .as_ref()
            .is_some_and(|(_, s)| s.len() != self.display_len())
        {
            self.sizes = None;
        }
    }

    /// 列表中显示的行数（含“..”）。
    fn display_len(&self) -> usize {
        self.rows.len() + usize::from(self.parent)
    }

    /// 名为 `name` 的项在显示中的位置（“..”在最前面；没有文件能叫“..”）。
    fn display_index(&self, name: &str) -> Option<usize> {
        if self.parent && name == PARENT {
            return Some(0);
        }
        let offset = usize::from(self.parent);
        self.rows
            .iter()
            .position(|r| r.name == name)
            .map(|ix| ix + offset)
    }

    /// 显示中第 `ix` 行的名称。
    fn display_name(&self, ix: usize) -> Option<&str> {
        match ix.checked_sub(usize::from(self.parent)) {
            Some(ix) => self.rows.get(ix).map(|r| r.name.as_str()),
            None => Some(PARENT),
        }
    }

    /// 当前行在显示中的位置。
    fn cursor_index(&self) -> Option<usize> {
        self.selection
            .cursor
            .as_deref()
            .and_then(|name| self.display_index(name))
    }

    /// 没有显示出来的隐藏文件数。
    #[cfg(test)]
    fn hidden_count(&self) -> usize {
        self.all_rows.len() - self.rows.len()
    }

    /// 每行的大小（行高随界面字号变化）。
    fn row_sizes(&mut self, row_height: Pixels) -> Rc<Vec<Size<Pixels>>> {
        let len = self.display_len();
        match &self.sizes {
            Some((height, sizes)) if *height == row_height && sizes.len() == len => sizes.clone(),
            _ => {
                let sizes = Rc::new(vec![size(px(0.), row_height); len]);
                self.sizes = Some((row_height, sizes.clone()));
                sizes
            }
        }
    }

    pub fn set_peer(&mut self, peer: DragSource) {
        self.peer = Some(peer);
    }

    pub fn set_disabled(&mut self, disabled: bool, cx: &mut Context<Self>) {
        if self.disabled != disabled {
            self.disabled = disabled;
            cx.notify();
        }
    }

    /// 所选的项，按列表中的顺序。
    pub fn selected(&self) -> Vec<FileRow> {
        self.rows
            .iter()
            .filter(|r| self.selection.contains(&r.name))
            .cloned()
            .collect()
    }

    fn names(&self) -> Vec<&str> {
        self.rows.iter().map(|r| r.name.as_str()).collect()
    }

    fn on_row_click(
        &mut self,
        ix: usize,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.rows.get(ix).cloned() else {
            return;
        };
        self.focus.focus(window, cx);
        if event.click_count() >= 2 {
            self.selection.only(&row.name);
            cx.emit(FileListEvent::Open(row));
            cx.notify();
            return;
        }
        let modifiers = event.modifiers();
        let mods = ClickMods {
            shift: modifiers.shift,
            toggle: modifiers.secondary(cx),
        };
        let names = self.names();
        let mut selection = self.selection.clone();
        selection.click(&names, &row.name, mods);
        self.selection = selection;
        cx.notify();
    }

    /// 右键没有选中的行：先只选它（菜单在下一帧建好，读到的是新的选择）；右键选中的行只把当前行移过去。
    fn on_row_right_down(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        if let Some(row) = self.rows.get(ix) {
            let name = row.name.clone();
            if self.selection.contains(&name) {
                self.selection.cursor = Some(name);
            } else {
                self.selection.only(&name);
            }
            cx.notify();
        }
    }

    /// Enter：恰好选中一项时打开它；当前行在“..”上（没有选中任何项）时回到上一级；没有选中、当前行在某一项上
    /// （Ctrl+↑ ↓ 移过去的）时打开当前行。
    fn on_open(&mut self, _: &OpenSelected, _: &mut Window, cx: &mut Context<Self>) {
        let selected = self.selected();
        match selected.as_slice() {
            [row] => cx.emit(FileListEvent::Open(row.clone())),
            [] => match self.selection.cursor.as_deref() {
                Some(PARENT) if self.parent => cx.emit(FileListEvent::Up),
                Some(name) => {
                    if let Some(row) = self.rows.iter().find(|r| r.name == name) {
                        cx.emit(FileListEvent::Open(row.clone()));
                    }
                }
                None => {}
            },
            _ => {}
        }
    }

    /// 方向键等：移动当前行，按 `how` 改选择，滚动到它。没有当前行时 ↑ ↓ 等到第一项（跳过“..”）。
    fn move_cursor(
        &mut self,
        step: Step,
        how: KeyMove,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let len = self.display_len();
        if len == 0 {
            return;
        }
        let first_row = usize::from(self.parent).min(len - 1);
        let page = self.page_rows(window);
        let target = match (step, self.cursor_index()) {
            (Step::First, _) => 0,
            (Step::Last, _) => len - 1,
            (_, None) => first_row,
            (Step::Prev, Some(ix)) => ix.saturating_sub(1),
            (Step::Next, Some(ix)) => (ix + 1).min(len - 1),
            (Step::PageUp, Some(ix)) => ix.saturating_sub(page),
            (Step::PageDown, Some(ix)) => (ix + page).min(len - 1),
        };
        let Some(to) = self.display_name(target).map(str::to_string) else {
            return;
        };
        let names = self.names();
        let mut selection = self.selection.clone();
        selection.move_to(&names, &to, how);
        self.selection = selection;
        self.type_ahead = None;
        self.scroll.scroll_to_item(target, ScrollStrategy::Nearest);
        cx.notify();
    }

    /// PageUp / PageDown 一次移动的行数：看得见的行数减一（至少一行）。
    fn page_rows(&self, window: &Window) -> usize {
        let row = rems(ROW_HEIGHT).to_pixels(window.rem_size());
        // 减去表头
        let visible = ((self.height.get() - row) / row).floor();
        (visible as usize).saturating_sub(1).max(1)
    }

    fn on_toggle_cursor(&mut self, _: &ToggleCursor, _: &mut Window, cx: &mut Context<Self>) {
        let names = self.names();
        let mut selection = self.selection.clone();
        selection.toggle_cursor(&names);
        self.selection = selection;
        cx.notify();
    }

    /// 按名字跳转：没有修饰键（Shift 除外）的可见字符。还没开始输入时空格不算（名称很少以空格开头）。
    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let mods = &keystroke.modifiers;
        if mods.control || mods.alt || mods.platform || mods.function {
            return;
        }
        let Some(text) = keystroke.key_char.as_deref() else {
            return;
        };
        if text.is_empty() || text.chars().any(char::is_control) {
            return;
        }
        let now = Instant::now();
        let mut typed = match self.type_ahead.take() {
            Some((typed, at)) if now.duration_since(at) < TYPE_AHEAD_TIMEOUT => typed,
            _ => String::new(),
        };
        if typed.is_empty() && text.trim().is_empty() {
            return;
        }
        typed.push_str(text);
        let names = self.names();
        let from = self
            .selection
            .cursor
            .as_deref()
            .and_then(|cursor| names.iter().position(|n| *n == cursor));
        if let Some(ix) = list_model::type_ahead(&names, from, &typed) {
            let name = names[ix].to_string();
            self.selection.only(&name);
            self.scroll
                .scroll_to_item(ix + usize::from(self.parent), ScrollStrategy::Nearest);
            cx.notify();
        }
        self.type_ahead = Some((typed, now));
        cx.stop_propagation();
    }

    /// Shift+F10、菜单键：在当前行的下面打开右键菜单（没有当前行时在列表的左上角），关掉后焦点回到列表。
    fn open_key_menu(&mut self, _: &OpenMenu, window: &mut Window, cx: &mut Context<Self>) {
        let Some(builder) = self.menu.clone().filter(|_| !self.disabled) else {
            return;
        };
        if let Some(ix) = self.cursor_index() {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
        let focus = self.focus.clone();
        let this = cx.entity().downgrade();
        // 菜单的内容要读取这个列表（所选的项），在这次更新结束后再建（与 gpui-kit 的右键菜单相同）
        window.defer(cx, move |window, cx| {
            let menu = PopupMenu::build(window, cx, move |menu, window, cx| {
                builder(menu.action_context(focus), window, cx)
            });
            menu.focus_handle(cx).focus(window, cx);
            let _ = this.update(cx, |this, cx| {
                let dismiss = cx.subscribe_in(&menu, window, |this, _, _: &DismissEvent, _, cx| {
                    this.key_menu = None;
                    cx.notify();
                });
                this.key_menu = Some((menu, dismiss));
                cx.notify();
            });
        });
    }

    /// 用键盘打开的菜单：放在 `below` 为真时所在元素的下面，否则在它的左上角。
    fn key_menu_layer(&self, below: bool, window: &Window) -> Option<AnyElement> {
        let (menu, _) = self.key_menu.as_ref()?;
        let margin = rems(0.5).to_pixels(window.rem_size());
        Some(
            div()
                .absolute()
                .when(below, |this| this.top_full())
                .when(!below, |this| this.top_0())
                .left(rems(2.))
                .child(
                    deferred(
                        anchored()
                            .snap_to_window_with_margin(margin)
                            .child(menu.clone()),
                    )
                    .with_priority(gpui_kit::base::POPUP_PRIORITY),
                )
                .into_any_element(),
        )
    }

    fn on_rename(&mut self, _: &RenameSelected, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.selected().into_iter().next() {
            cx.emit(FileListEvent::Rename(row));
        }
    }

    fn on_delete(&mut self, _: &DeleteSelected, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selection.selected.is_empty() {
            cx.emit(FileListEvent::Delete);
        }
    }

    fn on_select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        let names = self.names();
        let mut selection = self.selection.clone();
        selection.all(&names);
        self.selection = selection;
        cx.notify();
    }

    fn on_copy_paths(&mut self, _: &CopyPaths, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selection.selected.is_empty() {
            cx.emit(FileListEvent::CopyPaths);
        }
    }

    /// 拖动的内容：拖的是选中的行就带上全部所选，否则只带这一行。
    fn drag_payload(&self, row: &FileRow) -> DraggedFiles {
        let names = if self.selection.contains(&row.name) {
            self.selected().into_iter().map(|r| r.name).collect()
        } else {
            vec![row.name.clone()]
        };
        DraggedFiles {
            source: self.source.clone(),
            dir: self.dir.clone(),
            names,
        }
    }

    /// 能不能放下：列表不可用时不能；不接收自己这一边拖出的文件（远程栏内不能拖动移动）。
    fn accepts(&self, dragged: &dyn std::any::Any) -> bool {
        if self.disabled {
            return false;
        }
        match dragged.downcast_ref::<DraggedFiles>() {
            Some(files) => match (&files.source, &self.source) {
                (DragSource::Local, DragSource::Local) => false,
                (DragSource::Remote(_), DragSource::Remote(_)) => false,
                _ => self.peer.as_ref().is_none_or(|peer| *peer == files.source),
            },
            None => self.accept_external && dragged.is::<ExternalPaths>(),
        }
    }

    /// `popup_open`：窗口中开着右键菜单，“…”按钮不挂悬停提示（见 [`crate::menu_tooltip`]）。
    fn render_header(
        &self,
        columns: Columns,
        popup_open: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let layout = columns.layout;
        let chosen = self.chosen;
        let squeezed = squeezed(&chosen, &layout, self.has_mode);
        let tooltip: SharedString = if squeezed.is_empty() {
            t!("sftp.columns.menu")
        } else {
            format!("{}\n{}", t!("sftp.columns.menu"), squeezed_text(&squeezed)).into()
        };
        let menu_squeezed = squeezed.clone();
        let button = Button::new(SharedString::from(format!("{}-columns", self.id)))
            .ghost()
            .xsmall()
            .icon(IconName::Ellipsis)
            .when(!popup_open, |this| this.tooltip(tooltip))
            .dropdown_menu(move |menu, _, _| column_menu(menu, chosen, &menu_squeezed));
        let header = h_flex()
            .id(SharedString::from(format!("{}-header", self.id)))
            .test_support()
            .flex_none()
            .h(rems(ROW_HEIGHT))
            .px_3()
            .gap_2()
            .text_xs()
            .text_color(theme.muted_foreground)
            .bg(theme.secondary)
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .justify_between()
                    .child(div().min_w_0().truncate().child(t!("sftp.column.name")))
                    .child(button),
            )
            .when(layout.size, |this| {
                this.child(
                    div()
                        .flex_none()
                        .w(rems(SIZE_WIDTH))
                        .text_right()
                        .child(t!("sftp.column.size")),
                )
            })
            .when(layout.modified, |this| {
                this.child(
                    div()
                        .flex_none()
                        .w(rems(TIME_WIDTH))
                        .child(t!("sftp.column.modified")),
                )
            })
            .when(layout.mode, |this| {
                this.child(
                    div()
                        .flex_none()
                        .w(rems(MODE_WIDTH))
                        .child(t!("sftp.column.mode")),
                )
            });
        // 表头上右键：同一个菜单（不弹出文件的右键菜单）
        header.context_menu(move |menu, _, _| column_menu(menu, chosen, &squeezed))
    }

    fn render_row(
        &mut self,
        ix: usize,
        columns: Columns,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let row = self.rows[ix].clone();
        let selected = self.selection.contains(&row.name);
        let is_cursor = self.selection.cursor.as_deref() == Some(row.name.as_str());
        let ring = is_cursor && self.shows_focus(window);
        let menu_layer = is_cursor
            .then(|| self.key_menu_layer(true, window))
            .flatten();
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let drop_bg = theme.drop_target;
        let drop_border = theme.drag_border;
        let entity = cx.entity().downgrade();
        let payload = self.drag_payload(&row);
        let drag_label: SharedString = match payload.names.as_slice() {
            [one] => list_model::visible_name(one).into(),
            names => t!("sftp.drag_many", count = names.len()),
        };
        let mut el = h_flex()
            .id(SharedString::from(format!("{}-row-{}", self.id, row.name)))
            .test_support()
            .size_full()
            .px_3()
            .gap_2()
            .text_sm()
            .cursor_default()
            .when(selected, |el| el.bg(theme.list_active))
            .when(!selected, |el| el.hover(|s| s.bg(theme.list_hover)))
            .when(ring, |el| el.border_1().border_color(theme.ring))
            .children(menu_layer)
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.on_row_click(ix, event, window, cx)
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, window, cx| this.on_row_right_down(ix, window, cx)),
            )
            .on_drag(payload, {
                let entity = entity.clone();
                let name = row.name.clone();
                move |_, _, _, cx| {
                    // 拖动没有选中的行：只选它
                    let _ = entity.update(cx, |this, cx| {
                        if !this.selection.contains(&name) {
                            this.selection.only(&name);
                            cx.notify();
                        }
                    });
                    cx.new(|_| DragPreview(drag_label.clone()))
                }
            });
        // 目录行可以作为放置的目标
        if row.is_dir {
            let into = row.name.clone();
            let into_external = row.name.clone();
            let accept = entity.clone();
            el = el
                .can_drop(move |dragged, _, cx| {
                    accept
                        .upgrade()
                        .is_some_and(|this| this.read(cx).accepts(dragged))
                })
                .drag_over::<DraggedFiles>(move |style, _, _, _| {
                    style.bg(drop_bg).border_1().border_color(drop_border)
                })
                .drag_over::<ExternalPaths>(move |style, _, _, _| {
                    style.bg(drop_bg).border_1().border_color(drop_border)
                })
                .on_drop(cx.listener(move |_, files: &DraggedFiles, _, cx| {
                    cx.stop_propagation();
                    cx.emit(FileListEvent::Drop {
                        files: files.clone(),
                        into: Some(into.clone()),
                    });
                }))
                .on_drop(cx.listener(move |_, paths: &ExternalPaths, _, cx| {
                    cx.stop_propagation();
                    cx.emit(FileListEvent::DropExternal {
                        paths: paths.paths().to_vec(),
                        into: Some(into_external.clone()),
                    });
                }));
        }
        let modified_ms = row.modified.unwrap_or(0);
        let modified: SharedString =
            termlet_core::time::format_compact(modified_ms, columns.now, columns.date_style).into();
        let modified_full: SharedString =
            termlet_core::time::format_seconds(modified_ms, columns.date_style).into();
        let kind = if row.is_dir {
            IconKind::Folder
        } else {
            IconKind::File
        };
        el.child(
            h_flex()
                .flex_1()
                .min_w_0()
                .gap_1p5()
                // 显示出来的隐藏文件淡一些
                .when(row.hidden, |this| this.opacity(HIDDEN_OPACITY))
                .child(self.icon(&row.name, kind, cx))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .child(list_model::visible_name(&row.name)),
                )
                .when(row.is_link, |this| {
                    this.child(
                        Icon::new(IconName::Link)
                            .xsmall()
                            .flex_none()
                            .text_color(muted),
                    )
                }),
        )
        .when(columns.layout.size, |this| {
            this.child(
                div()
                    .flex_none()
                    .w(rems(SIZE_WIDTH))
                    .text_right()
                    .text_xs()
                    .text_color(muted)
                    .when(!row.is_dir, |this| {
                        this.child(list_model::format_size(row.size))
                    }),
            )
        })
        .when(columns.layout.modified, |this| {
            let id = SharedString::from(format!("{}-time-{}", self.id, row.name));
            this.child(
                div()
                    .id(id)
                    .test_support()
                    .flex_none()
                    .w(rems(TIME_WIDTH))
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(muted)
                    .when(!modified_full.is_empty(), |this| {
                        this.tooltip(crate::menu_tooltip::quiet(move |window, cx| {
                            Tooltip::new(modified_full.clone()).build(window, cx)
                        }))
                    })
                    .child(modified),
            )
        })
        .when(columns.layout.mode, |this| {
            this.child(
                div()
                    .flex_none()
                    .w(rems(MODE_WIDTH))
                    .text_xs()
                    .font_family(theme.mono_font_family.clone())
                    .text_color(muted)
                    .children(row.mode.clone()),
            )
        })
        .into_any_element()
    }

    /// 名称前的图标：彩色时按名称与类型，单色时只分文件夹与文件。
    fn icon(&self, name: &str, kind: IconKind, cx: &App) -> AnyElement {
        let theme = cx.theme();
        match self.icon_style {
            IconStyle::Color => img(file_icons::color_icon_path(name, kind, theme.is_dark()))
                .flex_none()
                .size_4()
                .into_any_element(),
            IconStyle::Mono => match kind {
                IconKind::Parent => Icon::new(IconName::FolderUp).text_color(theme.blue),
                IconKind::Folder => Icon::new(IconName::Folder).text_color(theme.blue),
                IconKind::File => Icon::new(IconName::File).text_color(theme.muted_foreground),
            }
            .small()
            .flex_none()
            .into_any_element(),
        }
    }

    /// 列表有焦点（或用键盘打开的菜单开着）时画出当前行的焦点框。
    fn shows_focus(&self, window: &Window) -> bool {
        self.key_menu.is_some() || self.focus.is_focused(window)
    }

    /// 单击“..”：只是清空选择（当前行移到它上面）；双击回到上一级。
    fn on_parent_click(&mut self, event: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        self.selection.clear();
        self.selection.cursor = Some(PARENT.into());
        if event.click_count() >= 2 {
            cx.emit(FileListEvent::Up);
        }
        cx.notify();
    }

    /// “..”一行：不能选中、不能拖动；可以把文件放到它上面（放进上一级）。
    fn render_parent_row(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let is_cursor = self.selection.cursor.as_deref() == Some(PARENT);
        let ring = is_cursor && self.shows_focus(window);
        let menu_layer = is_cursor
            .then(|| self.key_menu_layer(true, window))
            .flatten();
        let theme = cx.theme();
        let (hover, drop_bg, drop_border, ring_color) = (
            theme.list_hover,
            theme.drop_target,
            theme.drag_border,
            theme.ring,
        );
        let accept = cx.entity().downgrade();
        h_flex()
            .id(SharedString::from(format!("{}-parent", self.id)))
            .test_support()
            .size_full()
            .px_3()
            .gap_2()
            .text_sm()
            .cursor_default()
            .hover(move |s| s.bg(hover))
            .when(ring, |el| el.border_1().border_color(ring_color))
            .children(menu_layer)
            .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                this.on_parent_click(event, window, cx)
            }))
            // 右键“..”：菜单按没有选中任何项（当前目录）给出
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, window, cx| {
                    this.focus.focus(window, cx);
                    this.selection.clear();
                    this.selection.cursor = Some(PARENT.into());
                    cx.notify();
                }),
            )
            .can_drop(move |dragged, _, cx| {
                accept
                    .upgrade()
                    .is_some_and(|this| this.read(cx).accepts(dragged))
            })
            .drag_over::<DraggedFiles>(move |style, _, _, _| {
                style.bg(drop_bg).border_1().border_color(drop_border)
            })
            .drag_over::<ExternalPaths>(move |style, _, _, _| {
                style.bg(drop_bg).border_1().border_color(drop_border)
            })
            .on_drop(cx.listener(|_, files: &DraggedFiles, _, cx| {
                cx.stop_propagation();
                cx.emit(FileListEvent::Drop {
                    files: files.clone(),
                    into: Some(PARENT.into()),
                });
            }))
            .on_drop(cx.listener(|_, paths: &ExternalPaths, _, cx| {
                cx.stop_propagation();
                cx.emit(FileListEvent::DropExternal {
                    paths: paths.paths().to_vec(),
                    into: Some(PARENT.into()),
                });
            }))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1p5()
                    .child(self.icon(PARENT, IconKind::Parent, cx))
                    .child(PARENT),
            )
            .into_any_element()
    }
}

/// 显示出来的隐藏文件的不透明度。
const HIDDEN_OPACITY: f32 = 0.6;

/// 这一帧的列：显示哪些、时间的写法与“今年”按的现在。
#[derive(Clone, Copy)]
struct Columns {
    layout: ColumnLayout,
    now: i64,
    date_style: DateStyle,
}

impl Render for FileList {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rem = window.rem_size();
        let width_rem = f32::from(self.width.get()) / f32::from(rem);
        let (chosen, has_mode) = (self.chosen, self.has_mode);
        let columns = Columns {
            layout: layout_columns(width_rem, chosen, has_mode),
            now: termlet_core::time::now_ms(),
            date_style: date_style(),
        };
        self.icon_style = AppState::store(cx).settings().appearance.icons;
        let row_height = rems(ROW_HEIGHT).to_pixels(rem);
        let sizes = self.row_sizes(row_height);
        let body = if self.rows.is_empty() {
            // 空目录（或只有不显示的隐藏文件）：“..”照常在最上面，下面是说明
            let text = if self.all_rows.is_empty() {
                self.empty_text.clone()
            } else {
                t!(
                    "sftp.only_hidden",
                    count = self.all_rows.len(),
                    key = toggle_hidden_label(cx)
                )
            };
            let parent = self.parent.then(|| {
                div()
                    .h(row_height)
                    .child(self.render_parent_row(window, cx))
            });
            v_flex()
                .children(parent)
                .child(
                    div()
                        .id(SharedString::from(format!("{}-empty", self.id)))
                        .test_support()
                        .p_4()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(prose(text)),
                )
                .into_any_element()
        } else {
            let offset = usize::from(self.parent);
            v_virtual_list(
                cx.entity(),
                SharedString::from(format!("{}-rows", self.id)),
                sizes,
                move |this, range, window, cx| {
                    range
                        .map(|ix| match ix.checked_sub(offset) {
                            Some(ix) => this.render_row(ix, columns, window, cx),
                            None => this.render_parent_row(window, cx),
                        })
                        .collect::<Vec<_>>()
                },
            )
            .track_scroll(&self.scroll)
            .size_full()
            .into_any_element()
        };
        // 用键盘打开的菜单：没有当前行时放在列表的左上角
        let top_menu = self
            .cursor_index()
            .is_none()
            .then(|| self.key_menu_layer(false, window))
            .flatten();
        let menu_open = self.key_menu.is_some();
        let theme = cx.theme();
        let drop_bg = theme.drop_target;
        let drop_border = theme.drag_border;
        // 宽度变了到需要显示 / 隐藏列时重新渲染
        let measured = self.width.clone();
        let measured_height = self.height.clone();
        let entity = cx.entity().downgrade();
        let rem_px = f32::from(rem);
        let accept = cx.entity().downgrade();
        let list = v_flex()
            .id(SharedString::from(self.id))
            .test_support()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .on_action(cx.listener(Self::on_open))
            .on_action(cx.listener(|_, _: &GoUp, _, cx| cx.emit(FileListEvent::Up)))
            .on_action(cx.listener(Self::on_rename))
            .on_action(cx.listener(|_, _: &Refresh, _, cx| cx.emit(FileListEvent::Refresh)))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_select_all))
            .on_action(cx.listener(Self::on_copy_paths))
            .on_action(cx.listener(|_, _: &PasteFiles, _, cx| cx.emit(FileListEvent::PasteFiles)))
            // macOS 编辑菜单的拷贝、粘贴、全选（输入框的编辑动作，见 crate::app_menu）：当作列表自己的
            .on_action(cx.listener(|this, _: &text_input::SelectAll, window, cx| {
                this.on_select_all(&SelectAll, window, cx)
            }))
            .on_action(cx.listener(|this, _: &text_input::Copy, window, cx| {
                this.on_copy_paths(&CopyPaths, window, cx)
            }))
            .on_action(
                cx.listener(|_, _: &text_input::Paste, _, cx| cx.emit(FileListEvent::PasteFiles)),
            )
            .on_action(cx.listener(|this, _: &ToggleHidden, _, cx| toggle_hidden(&this.source, cx)))
            .on_action(cx.listener(|this, _: &SelectPrev, window, cx| {
                this.move_cursor(Step::Prev, KeyMove::Select, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectNext, window, cx| {
                this.move_cursor(Step::Next, KeyMove::Select, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectPageUp, window, cx| {
                this.move_cursor(Step::PageUp, KeyMove::Select, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectPageDown, window, cx| {
                this.move_cursor(Step::PageDown, KeyMove::Select, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectFirst, window, cx| {
                this.move_cursor(Step::First, KeyMove::Select, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectLast, window, cx| {
                this.move_cursor(Step::Last, KeyMove::Select, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ExtendPrev, window, cx| {
                this.move_cursor(Step::Prev, KeyMove::Extend, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ExtendNext, window, cx| {
                this.move_cursor(Step::Next, KeyMove::Extend, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ExtendPageUp, window, cx| {
                this.move_cursor(Step::PageUp, KeyMove::Extend, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ExtendPageDown, window, cx| {
                this.move_cursor(Step::PageDown, KeyMove::Extend, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ExtendFirst, window, cx| {
                this.move_cursor(Step::First, KeyMove::Extend, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ExtendLast, window, cx| {
                this.move_cursor(Step::Last, KeyMove::Extend, window, cx)
            }))
            .on_action(cx.listener(|this, _: &CursorPrev, window, cx| {
                this.move_cursor(Step::Prev, KeyMove::Cursor, window, cx)
            }))
            .on_action(cx.listener(|this, _: &CursorNext, window, cx| {
                this.move_cursor(Step::Next, KeyMove::Cursor, window, cx)
            }))
            .on_action(cx.listener(Self::on_toggle_cursor))
            .on_action(cx.listener(Self::open_key_menu))
            .on_action(cx.listener(|_, _: &NewFolder, _, cx| cx.emit(FileListEvent::NewFolder)))
            .on_action(cx.listener(|_, _: &FocusPath, _, cx| cx.emit(FileListEvent::FocusPath)))
            .on_key_down(cx.listener(Self::on_key_down))
            .can_drop(move |dragged, _, cx| {
                accept
                    .upgrade()
                    .is_some_and(|this| this.read(cx).accepts(dragged))
            })
            .drag_over::<DraggedFiles>(move |style, _, _, _| {
                style.bg(drop_bg).border_1().border_color(drop_border)
            })
            .drag_over::<ExternalPaths>(move |style, _, _, _| {
                style.bg(drop_bg).border_1().border_color(drop_border)
            })
            .on_drop(cx.listener(|_, files: &DraggedFiles, _, cx| {
                cx.emit(FileListEvent::Drop {
                    files: files.clone(),
                    into: None,
                })
            }))
            .on_drop(cx.listener(|_, paths: &ExternalPaths, _, cx| {
                cx.emit(FileListEvent::DropExternal {
                    paths: paths.paths().to_vec(),
                    into: None,
                })
            }))
            .child(
                canvas(
                    move |bounds, _, cx| {
                        measured_height.set(bounds.size.height);
                        let old = measured.replace(bounds.size.width);
                        let layout = |width: Pixels| {
                            layout_columns(f32::from(width) / rem_px, chosen, has_mode)
                        };
                        if layout(old) != layout(bounds.size.width) {
                            let _ = entity.update(cx, |_, cx| cx.notify());
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(self.render_header(columns, crate::menu_tooltip::menu_open(window, cx), cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(body)
                    .vertical_scrollbar(&self.scroll)
                    .children(top_menu)
                    // 菜单开着时滚轮不滚动列表（菜单跟着当前行，滚出去就看不见了）
                    .when(menu_open, |this| {
                        this.child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .size_full()
                                .on_scroll(|_, _, cx| cx.stop_propagation()),
                        )
                    }),
            );
        match (&self.menu, self.disabled) {
            (Some(menu), false) => {
                let menu = menu.clone();
                list.context_menu(move |m, window, cx| menu(m, window, cx))
                    .into_any_element()
            }
            _ => list.into_any_element(),
        }
    }
}

/// 拖动时跟着指针的名称。
struct DragPreview(SharedString);

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded(cx.theme().radius)
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .text_sm()
            .text_color(cx.theme().popover_foreground)
            .child(self.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeSet;

    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AnyWindowHandle, Bounds, Entity, TestAppContext, WindowBounds, WindowOptions};

    use super::*;

    /// 只放列表的视图，记下列表发出的事件。
    struct Harness {
        list: Entity<FileList>,
        _events: Subscription,
    }

    impl Render for Harness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.list.clone())
        }
    }

    fn row(name: &str, is_dir: bool, hidden: bool) -> FileRow {
        FileRow {
            name: name.into(),
            is_dir,
            is_link: false,
            hidden,
            size: 1,
            modified: None,
            mode: None,
        }
    }

    type Events = Rc<RefCell<Vec<&'static str>>>;

    fn open(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<FileList>, Events) {
        // 应用状态在核心的运行时线程上有后台任务（存储事件等），与其他用到它的测试一样允许等待
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        cx.update(crate::init);
        cx.update(AppState::install_in_memory);
        let events: Events = Rc::default();
        let recorded = events.clone();
        let (handle, harness) = cx.update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        Default::default(),
                        size(px(800.), px(600.)),
                    ))),
                    ..Default::default()
                },
                cx,
                |_, cx| {
                    cx.new(|cx| {
                        let list = cx.new(|cx| {
                            FileList::new("test-list", DragSource::Local, "空".into(), cx)
                        });
                        let events = recorded.clone();
                        let subscription = cx.subscribe(&list, move |_, _, event, _| {
                            events.borrow_mut().push(match event {
                                FileListEvent::Up => "up",
                                FileListEvent::Open(_) => "open",
                                FileListEvent::NewFolder => "new_folder",
                                FileListEvent::FocusPath => "focus_path",
                                _ => "other",
                            })
                        });
                        Harness {
                            list,
                            _events: subscription,
                        }
                    })
                },
            )
            .unwrap()
        });
        let list = cx.read(|cx| harness.read(cx).list.clone());
        (handle, list, events)
    }

    fn selected(list: &Entity<FileList>, cx: &mut TestAppContext) -> BTreeSet<String> {
        cx.read(|cx| {
            list.read(cx)
                .selected()
                .into_iter()
                .map(|r| r.name)
                .collect()
        })
    }

    fn shown(cx: &mut TestAppContext, handle: AnyWindowHandle, id: &str) -> bool {
        let id = SharedString::from(id.to_string());
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.try_find(id).is_some()
        })
        .unwrap()
    }

    #[gpui_kit::test]
    fn parent_row_goes_up_and_is_never_selected(cx: &mut TestAppContext) {
        let (handle, list, events) = open(cx);
        let rows = vec![row("src", true, false), row("app.py", false, false)];
        cx.update(|cx| list.update(cx, |l, cx| l.set_entries("/a/b".into(), rows, true, cx)));
        assert!(shown(cx, handle, "test-list-parent"), "不在根目录时有“..”");
        cx.update_window(handle, |_, window, cx| {
            window.click("test-list-row-app.py", cx);
            // 单击“..”只清空选择
            window.click("test-list-parent", cx);
        })
        .unwrap();
        assert!(selected(&list, cx).is_empty());
        cx.update_window(handle, |_, window, cx| {
            window.double_click("test-list-parent", cx);
            // 全选不含“..”
            window.press("ctrl-a", cx);
        })
        .unwrap();
        assert_eq!(events.borrow().as_slice(), ["up"], "双击“..”回到上一级");
        assert_eq!(
            selected(&list, cx),
            BTreeSet::from(["src".into(), "app.py".into()])
        );

        // 根目录没有“..”
        let rows = vec![row("etc", true, false)];
        cx.update(|cx| list.update(cx, |l, cx| l.set_entries("/".into(), rows, false, cx)));
        assert!(!shown(cx, handle, "test-list-parent"));
        assert!(shown(cx, handle, "test-list-row-etc"));
    }

    #[gpui_kit::test]
    fn hidden_files_follow_the_switch(cx: &mut TestAppContext) {
        let (handle, list, _) = open(cx);
        let rows = vec![
            row("src", true, false),
            row(".env", false, true),
            row("app.py", false, false),
        ];
        cx.update(|cx| list.update(cx, |l, cx| l.set_entries("/a".into(), rows, true, cx)));
        assert!(
            shown(cx, handle, "test-list-row-.env"),
            "默认显示隐藏的文件"
        );
        cx.update_window(handle, |_, window, cx| {
            window.click("test-list-row-src", cx);
            window.press("ctrl-a", cx);
            // Ctrl+Alt+H：这一栏不再显示隐藏的文件
            window.press("ctrl-alt-h", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert!(cx.read(|cx| hides_hidden(&DragSource::Local, cx)));
        assert!(!shown(cx, handle, "test-list-row-.env"));
        assert_eq!(cx.read(|cx| list.read(cx).hidden_count()), 1);
        assert_eq!(
            selected(&list, cx),
            BTreeSet::from(["src".into(), "app.py".into()]),
            "不显示的项不在选择中"
        );
        assert!(
            !cx.read(|cx| hides_hidden(&DragSource::Remote(list.entity_id()), cx)),
            "远程栏的开关不受影响"
        );

        // 只剩隐藏的文件：“..”照常在，下面说明怎样显示
        let rows = vec![row(".bashrc", false, true)];
        cx.update(|cx| list.update(cx, |l, cx| l.set_entries("/b".into(), rows, true, cx)));
        assert!(shown(cx, handle, "test-list-parent"));
        assert!(shown(cx, handle, "test-list-empty"));
        assert!(!shown(cx, handle, "test-list-row-.bashrc"));

        // 切回显示
        cx.update(|cx| toggle_hidden(&DragSource::Local, cx));
        cx.run_until_parked();
        assert!(shown(cx, handle, "test-list-row-.bashrc"));
        assert!(!shown(cx, handle, "test-list-empty"));
    }

    /// 让列表取得焦点后依次按这些键。
    fn press(
        cx: &mut TestAppContext,
        handle: AnyWindowHandle,
        list: &Entity<FileList>,
        keys: &[&str],
    ) {
        let focus = cx.read(|cx| list.read(cx).focus_handle(cx));
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            focus.focus(window, cx);
            for key in keys {
                window.press(key, cx);
            }
        })
        .unwrap();
    }

    fn cursor(list: &Entity<FileList>, cx: &mut TestAppContext) -> Option<String> {
        cx.read(|cx| list.read(cx).selection.cursor.clone())
    }

    fn names(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|n| n.to_string()).collect()
    }

    #[gpui_kit::test]
    fn keyboard_moves_the_current_row(cx: &mut TestAppContext) {
        let (handle, list, events) = open(cx);
        let rows = vec![
            row("src", true, false),
            row("a.txt", false, false),
            row("b.txt", false, false),
        ];
        cx.update(|cx| list.update(cx, |l, cx| l.set_entries("/w".into(), rows, true, cx)));

        // 没有当前行时 ↓ 到第一项（跳过“..”）；↑ 到“..”上，不选中任何项，Enter 回到上一级
        press(cx, handle, &list, &["down"]);
        assert_eq!(selected(&list, cx), names(&["src"]));
        press(cx, handle, &list, &["up"]);
        assert!(selected(&list, cx).is_empty());
        assert_eq!(cursor(&list, cx).as_deref(), Some(PARENT));
        press(cx, handle, &list, &["enter"]);
        assert_eq!(events.borrow().as_slice(), ["up"]);

        // End、Shift+↑ 连续选择；Ctrl+↑ 只移动当前行，Ctrl+空格加上它
        press(cx, handle, &list, &["end", "shift-up"]);
        assert_eq!(selected(&list, cx), names(&["a.txt", "b.txt"]));
        press(cx, handle, &list, &["ctrl-up"]);
        assert_eq!(cursor(&list, cx).as_deref(), Some("src"));
        assert_eq!(selected(&list, cx), names(&["a.txt", "b.txt"]));
        press(cx, handle, &list, &["ctrl-space"]);
        assert_eq!(selected(&list, cx), names(&["src", "a.txt", "b.txt"]));
        // Home 到“..”；PageDown 到底（行数少于一页）
        press(cx, handle, &list, &["home"]);
        assert!(selected(&list, cx).is_empty());
        press(cx, handle, &list, &["pagedown"]);
        assert_eq!(selected(&list, cx), names(&["b.txt"]));

        // F7 新建文件夹、Ctrl+L / Alt+D 编辑路径交给所在的栏
        events.borrow_mut().clear();
        press(cx, handle, &list, &["f7", "ctrl-l", "alt-d"]);
        assert_eq!(
            events.borrow().as_slice(),
            ["new_folder", "focus_path", "focus_path"]
        );
    }

    #[gpui_kit::test]
    fn typing_jumps_to_names(cx: &mut TestAppContext) {
        let (handle, list, _) = open(cx);
        let rows = || {
            vec![
                row("bin", true, false),
                row("boot", true, false),
                row("dev", true, false),
                row("etc", true, false),
            ]
        };
        cx.update(|cx| list.update(cx, |l, cx| l.set_entries("/".into(), rows(), false, cx)));
        press(cx, handle, &list, &["b->b"]);
        assert_eq!(selected(&list, cx), names(&["bin"]));
        // 连按同一个字母：下一个以它开头的
        press(cx, handle, &list, &["b->b"]);
        assert_eq!(selected(&list, cx), names(&["boot"]));
        // 换了目录重新开始；一段文字跳到以它开头的，大小写不论
        cx.update(|cx| list.update(cx, |l, cx| l.set_entries("/x".into(), rows(), true, cx)));
        press(cx, handle, &list, &["D->D", "e->e"]);
        assert_eq!(selected(&list, cx), names(&["dev"]));
        // 带 Ctrl 的不算输入
        press(cx, handle, &list, &["ctrl-b"]);
        assert_eq!(selected(&list, cx), names(&["dev"]));
    }

    #[gpui_kit::test]
    fn going_up_selects_where_you_came_from(cx: &mut TestAppContext) {
        let (_, list, _) = open(cx);
        let rows = vec![row("a", true, false), row("proj", true, false)];
        cx.update(|cx| {
            list.update(cx, |l, cx| {
                l.set_entries(r"C:\w\proj\src".into(), vec![], true, cx);
                l.set_entries(r"C:\w".into(), rows.clone(), true, cx);
            })
        });
        assert_eq!(selected(&list, cx), names(&["proj"]));
        assert_eq!(cursor(&list, cx).as_deref(), Some("proj"));
        // 进入别的目录（不是回到上级）时什么也不选
        cx.update(|cx| list.update(cx, |l, cx| l.set_entries(r"D:\".into(), rows, false, cx)));
        assert!(selected(&list, cx).is_empty());
    }

    #[gpui_kit::test]
    fn deleting_keeps_the_cursor_in_place(cx: &mut TestAppContext) {
        let (handle, list, _) = open(cx);
        let rows = vec![
            row("a", false, false),
            row("b", false, false),
            row("c", false, false),
        ];
        cx.update(|cx| list.update(cx, |l, cx| l.set_entries("/w".into(), rows, false, cx)));
        press(cx, handle, &list, &["down", "down"]);
        assert_eq!(selected(&list, cx), names(&["b"]));
        // 删掉了 b：选中原来位置上的 c，接着按 Delete 删它
        let rest = vec![row("a", false, false), row("c", false, false)];
        cx.update(|cx| list.update(cx, |l, cx| l.refresh_entries(rest, cx)));
        assert_eq!(selected(&list, cx), names(&["c"]));
        // 删掉最后一项：到新的最后一项
        cx.update(|cx| {
            list.update(cx, |l, cx| {
                l.refresh_entries(vec![row("a", false, false)], cx)
            })
        });
        assert_eq!(selected(&list, cx), names(&["a"]));
    }

    #[gpui_kit::test]
    fn menu_key_opens_the_context_menu(cx: &mut TestAppContext) {
        let (handle, list, _) = open(cx);
        let weak = list.downgrade();
        cx.update(|cx| {
            list.update(cx, |l, cx| {
                // 与两栏的菜单一样读取列表的选择（菜单要在列表的更新结束后才建）
                l.set_menu(move |menu, _, cx| {
                    let count = weak
                        .upgrade()
                        .map_or(0, |list| list.read(cx).selected().len());
                    menu.item(gpui_kit::component::menu::PopupMenuItem::new(format!(
                        "选中 {count} 项"
                    )))
                });
                l.set_entries("/w".into(), vec![row("a", false, false)], true, cx);
            })
        });
        press(cx, handle, &list, &["down", "shift-f10"]);
        cx.run_until_parked();
        assert!(cx.read(|cx| list.read(cx).key_menu.is_some()));
        assert!(shown(cx, handle, "popup-menu"), "菜单画在当前行的下面");
        // Esc 关掉菜单，焦点回到列表
        let focus = cx.read(|cx| list.read(cx).focus_handle(cx));
        cx.update_window(handle, |_, window, cx| window.press("escape", cx))
            .unwrap();
        cx.run_until_parked();
        assert!(cx.read(|cx| list.read(cx).key_menu.is_none()));
        assert!(!shown(cx, handle, "popup-menu"));
        cx.update_window(handle, |_, window, _| assert!(focus.is_focused(window)))
            .unwrap();
        // 菜单键也可以；没有当前行时也能打开（当前目录的菜单）
        cx.update(|cx| list.update(cx, |l, cx| l.set_entries("/v".into(), vec![], true, cx)));
        press(cx, handle, &list, &["menu"]);
        cx.run_until_parked();
        assert!(shown(cx, handle, "popup-menu"));
    }

    // 不同宽度下显示哪些列：名称列至少留 10rem，修改时间先于大小、大小先于权限
    #[test]
    fn columns_follow_the_width_with_time_first() {
        let all = SftpColumns::default();
        let layout = |width: f32| layout_columns(width, all, true);
        let shown = |size, modified, mode| ColumnLayout {
            size,
            modified,
            mode,
        };
        assert_eq!(layout(17.9), shown(false, false, false), "只有名称");
        assert_eq!(layout(18.), shown(false, true, false), "先有时间");
        assert_eq!(layout(24.4), shown(false, true, false));
        assert_eq!(layout(24.5), shown(true, true, false), "再有大小");
        // 双栏时每栏常见的宽度（约 31rem）：时间、大小都在
        assert_eq!(layout(31.), shown(true, true, false));
        assert_eq!(layout(32.), shown(true, true, true), "最后是权限");
        assert_eq!(layout(60.), shown(true, true, true));
        // 没有权限信息（Windows 的本机栏）
        assert_eq!(layout_columns(40., all, false), shown(true, true, false));
        // 不显示时间：大小、权限依次往前
        let no_time = SftpColumns {
            modified: false,
            ..all
        };
        assert_eq!(
            layout_columns(18., no_time, true),
            shown(true, false, false)
        );
        assert_eq!(
            layout_columns(25.5, no_time, true),
            shown(true, false, true)
        );
        // 显示了次要的列时，名称列都至少留 NAME_MIN
        for tenth in 100..800 {
            let width = tenth as f32 / 10.;
            let l = layout(width);
            if l == ColumnLayout::default() {
                continue;
            }
            let used = ROW_PADDING
                + [
                    (l.size, SIZE_WIDTH),
                    (l.modified, TIME_WIDTH),
                    (l.mode, MODE_WIDTH),
                ]
                .iter()
                .filter(|(shown, _)| *shown)
                .map(|(_, w)| COLUMN_GAP + w)
                .sum::<f32>();
            assert!(width - used >= NAME_MIN - 0.001, "{width}rem：{l:?}");
        }
    }

    // 表头的菜单勾选显示的列：记进界面状态（所有文件列表共用），列表跟着变；放不下的在菜单中说明
    #[gpui_kit::test]
    fn chosen_columns_are_remembered(cx: &mut TestAppContext) {
        let (handle, list, _) = open(cx);
        let mut file = row("app.log", false, false);
        file.modified = Some(termlet_core::time::now_ms() - 60_000);
        cx.update(|cx| {
            list.update(cx, |l, cx| {
                l.set_entries("/var/log".into(), vec![file], false, cx)
            })
        });
        // 窗口 800 宽（50rem）：时间、大小都在（本机栏这里没有权限信息）。第一帧量出宽度，下一帧按它排列
        shown(cx, handle, "test-list-header");
        cx.run_until_parked();
        assert!(shown(cx, handle, "test-list-time-app.log"), "默认显示时间");
        assert!(shown(cx, handle, "test-list-columns"), "表头有选择列的按钮");
        cx.update(|cx| toggle_column(Column::Modified, cx));
        cx.run_until_parked();
        assert!(!cx.update(|cx| AppState::ui(cx).read(cx).state().sftp_columns.modified));
        assert!(
            !shown(cx, handle, "test-list-time-app.log"),
            "取消勾选后不显示"
        );
        cx.update(|cx| toggle_column(Column::Modified, cx));
        cx.run_until_parked();
        assert!(shown(cx, handle, "test-list-time-app.log"));
        // 勾选了但放不下：说明哪些列没显示
        let chosen = SftpColumns::default();
        let narrow = layout_columns(20., chosen, true);
        let hidden = squeezed(&chosen, &narrow, true);
        assert_eq!(hidden, vec![Column::Size, Column::Mode]);
        crate::i18n::set_language(crate::i18n::Language::ZhCn);
        assert_eq!(
            squeezed_text(&hidden).as_ref(),
            "这一栏太窄，暂时没有显示：大小、权限"
        );
        assert!(squeezed(&chosen, &layout_columns(40., chosen, true), true).is_empty());
        // 没有权限信息时不算放不下
        assert!(!squeezed(&chosen, &narrow, false).contains(&Column::Mode));
    }
}
