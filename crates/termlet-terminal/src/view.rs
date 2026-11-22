//! 终端视图：焦点、键盘、鼠标、选区、复制粘贴、滚动与输入法。
//!
//! 回放模式（[`TerminalView::set_replay`]，回放录制的会话）：只读，按键与输入法不处理（交给外层的回放控制），
//! 鼠标不上报给程序（照常选择文字）；网格是录制时的大小（引擎决定），不随布局变化，放不下时缩小字号。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::{
    App, Bounds, ClipboardItem, Context, EntityInputHandler, EventEmitter, FocusHandle, Focusable,
    FontFallbacks, InteractiveElement as _, IntoElement, KeyBinding, KeyContext, KeyDownEvent,
    KeyUpEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, OperatingSystem,
    ParentElement as _, Pixels, Render, ScrollEvent, SharedString, Styled as _, Subscription, Task,
    TestSupportExt as _, UTF16Selection, Unbind, Window, actions, div, px,
};

use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

use crate::colors::{Palette, Rgb};
use crate::element::{LayoutMetrics, TerminalElement};
use crate::engine::{EngineEvent, OutputObserver, TerminalEngine};
use crate::grid::{
    CursorShape, GridPoint, GridSnapshot, MouseTracking, Scroll, ScrollPosition, SelectionKind,
    Side, TerminalModes,
};
use crate::highlight::{HighlightCache, Highlighter};
use crate::kitty_keyboard::KeyboardFlags;
use crate::search::{SearchMatch, SearchQuery};
use crate::shell_events::ShellEvent;
use crate::suggest::{CommandSuggestions, PromptInput};
use crate::{keys, mouse, suggest};
use termlet_io::{CloseReason, GridSize};

actions!(
    terminal,
    [
        /// 复制选中的文字
        Copy,
        /// 粘贴
        Paste,
        /// 全选（含回滚）
        SelectAll,
        /// 清空回滚与屏幕，提示符所在的行成为第一行
        Clear,
        ScrollLineUp,
        ScrollLineDown,
        ScrollPageUp,
        ScrollPageDown,
        ScrollToTop,
        ScrollToBottom,
        /// 放大字号（只影响这个终端）
        ZoomIn,
        ZoomOut,
        ZoomReset,
        /// 接受命令补全提示的下一个词（没有提示时按键照常发给远端），见 [`crate::suggest`]
        AcceptSuggestionWord,
    ]
);

/// 终端的按键上下文。
pub const KEY_CONTEXT: &str = "Terminal";

/// 会话结束后终端的按键上下文中另有的标识。
const CLOSED: &str = "closed";

/// 回放模式中终端的按键上下文中另有的标识（回放的快捷键只在这里生效）。
pub const REPLAY: &str = "replay";

/// 注册终端的默认快捷键（可自定义的快捷键由应用在之后覆盖），按当前系统的习惯（[`key_bindings`]）。
pub fn init(cx: &mut App) {
    cx.bind_keys(key_bindings(cx.platform_info().os));
}

/// 终端的默认快捷键：`os` 为按哪个系统的习惯（测试中逐个系统检查）。
///
/// - Windows、Linux：复制 Ctrl+Shift+C / Ctrl+Insert，粘贴 Ctrl+Shift+V / Shift+Insert，全选 Ctrl+Shift+A；Ctrl+C 等
///   照常发给远端（会话进行中解除 gpui-kit 的 Root 给 Ctrl+C 绑的复制，见下）。
/// - macOS：照 Terminal.app、iTerm2 的习惯用 ⌘：⌘C 复制（没有选中内容时什么也不做，⌘ 组合不写进终端，不会发 ^C）、
///   ⌘V 粘贴、⌘A 全选、⌘K 清屏，⌘↑ ⌘↓、⌘PageUp ⌘PageDown、⌘Home ⌘End 滚动；Ctrl 组合全部原样交给终端。
/// - 都有：Shift+PageUp / PageDown / Home / End 滚动。
/// - 命令补全提示（[`crate::suggest`]）：会话进行中 Windows、Linux 上 Ctrl+→、macOS 上 ⌥→ 接受一个词，没有提示时
///   按键照常发给远端；接受整条的 → 与 End 不是快捷键（在按键监听器中判断，没有提示时照常编码）。
pub fn key_bindings(os: OperatingSystem) -> Vec<KeyBinding> {
    let context = Some(KEY_CONTEXT);
    // gpui-kit 的 Root 把 Tab / Shift+Tab 绑给了切换焦点（root::Tab / root::TabPrev，gpui-base 没有公开这两个类型），
    // 快捷键又先于按键监听器分发，焦点在终端上时这两个键到不了 key_down，shell 的补全不起作用。会话进行中在终端层
    // 解除这两个动作，按键照常编码发给远端；只解除它们（不用 NoAction），外层给同一组合绑的别的动作照常生效。
    // 会话结束后终端不用这两个键，照旧切换焦点。
    let live = format!("{KEY_CONTEXT} && !{CLOSED}");
    let live = Some(live.as_str());
    let mut bindings = vec![
        KeyBinding::new("tab", Unbind("root::Tab".into()), live),
        KeyBinding::new("shift-tab", Unbind("root::TabPrev".into()), live),
        KeyBinding::new("shift-pageup", ScrollPageUp, context),
        KeyBinding::new("shift-pagedown", ScrollPageDown, context),
        KeyBinding::new("shift-home", ScrollToTop, context),
        KeyBinding::new("shift-end", ScrollToBottom, context),
        KeyBinding::new(suggest::accept_word_combo(os), AcceptSuggestionWord, live),
    ];
    if os == OperatingSystem::Mac {
        bindings.extend([
            KeyBinding::new("cmd-c", Copy, context),
            KeyBinding::new("cmd-v", Paste, context),
            KeyBinding::new("cmd-a", SelectAll, context),
            KeyBinding::new("cmd-k", Clear, context),
            KeyBinding::new("cmd-up", ScrollLineUp, context),
            KeyBinding::new("cmd-down", ScrollLineDown, context),
            KeyBinding::new("cmd-pageup", ScrollPageUp, context),
            KeyBinding::new("cmd-pagedown", ScrollPageDown, context),
            KeyBinding::new("cmd-home", ScrollToTop, context),
            KeyBinding::new("cmd-end", ScrollToBottom, context),
            KeyBinding::new("cmd-=", ZoomIn, context),
            KeyBinding::new("cmd-+", ZoomIn, context),
            KeyBinding::new("cmd--", ZoomOut, context),
            KeyBinding::new("cmd-0", ZoomReset, context),
        ]);
    } else {
        bindings.extend([
            // gpui-kit 的 Root 在 Windows、Linux 上把 Ctrl+C 绑给了输入框的复制（input::Copy，有选中的界面文字时复制，
            // 否则交给按键监听器）。焦点路径上一旦有别的元素处理 input::Copy（例如为 macOS 编辑菜单转发的），Ctrl+C
            // 就到不了 key_down、shell 收不到中断。会话进行中在终端层解除它，Ctrl+C 一定是 ^C；会话结束后照旧。
            KeyBinding::new("ctrl-c", Unbind("input::Copy".into()), live),
            KeyBinding::new("ctrl-shift-c", Copy, context),
            KeyBinding::new("ctrl-insert", Copy, context),
            KeyBinding::new("ctrl-shift-v", Paste, context),
            KeyBinding::new("shift-insert", Paste, context),
            KeyBinding::new("ctrl-shift-a", SelectAll, context),
            KeyBinding::new("ctrl-shift-up", ScrollLineUp, context),
            KeyBinding::new("ctrl-shift-down", ScrollLineDown, context),
            KeyBinding::new("ctrl-=", ZoomIn, context),
            KeyBinding::new("ctrl-+", ZoomIn, context),
            KeyBinding::new("ctrl--", ZoomOut, context),
            KeyBinding::new("ctrl-0", ZoomReset, context),
        ]);
    }
    bindings
}

/// 右键的行为。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RightClick {
    /// 有选中内容时复制，否则粘贴
    #[default]
    Auto,
    Paste,
    /// 由应用弹出菜单：视图发出 [`TerminalViewEvent::ContextMenu`]，并让按下事件接着冒泡给外层
    /// （例如 gpui-kit 的 `context_menu`）；程序开了鼠标上报时右键仍交给程序
    Menu,
}

/// 保证文字可读：给出（文字色，背景色），对比度不够时返回调整后的文字色。
/// 算法由使用方提供（终端不依赖配色模块）。
#[derive(Clone)]
pub struct ContrastAdjust(pub std::sync::Arc<dyn Fn(Rgb, Rgb) -> Option<Rgb> + Send + Sync>);

impl std::fmt::Debug for ContrastAdjust {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ContrastAdjust")
    }
}

/// 终端的外观与交互选项。
#[derive(Clone, Debug)]
pub struct TerminalStyle {
    pub font_family: SharedString,
    pub font_fallbacks: Option<FontFallbacks>,
    pub font_size: Pixels,
    /// 行高（字号的倍数）
    pub line_height: f32,
    pub palette: Palette,
    /// 粗体用亮色
    pub bold_is_bright: bool,
    /// 网格四周的留白
    pub padding: Pixels,
    pub copy_on_select: bool,
    pub right_click: RightClick,
    /// 光标的默认形状与是否闪烁；程序用 DECSCUSR 设置的优先（见 [`TerminalEngine::set_default_cursor`]）
    pub cursor_shape: CursorShape,
    pub cursor_blink: bool,
    /// macOS 上 Option 键作为 Meta 键（[`keys::AltKey::OptionMeta`]），其他系统上不起作用
    pub option_as_meta: bool,
    /// 保证文字可读（没有时不调整）
    pub contrast: Option<ContrastAdjust>,
    /// 关键词高亮（没有时不高亮）。规则不变时共用同一个，渲染的缓存按它区分
    pub highlighter: Option<Arc<Highlighter>>,
    /// 高亮的颜色在当前配色中实际显示的样子（对背景对比度不够时界面调整过的）；没有的颜色（或没有这张表时）用
    /// [`crate::HighlightColor::fallback`]
    pub highlight_colors: Option<Arc<HashMap<crate::HighlightColor, Rgb>>>,
}

impl Default for TerminalStyle {
    fn default() -> Self {
        Self {
            font_family: crate::fonts::DEFAULT_FAMILY.into(),
            font_fallbacks: Some(crate::fonts::default_fallbacks()),
            font_size: px(14.),
            line_height: 1.25,
            palette: Palette::default_dark(),
            bold_is_bright: true,
            padding: px(6.),
            copy_on_select: false,
            right_click: RightClick::Auto,
            cursor_shape: CursorShape::Block,
            cursor_blink: false,
            option_as_meta: false,
            contrast: None,
            highlighter: None,
            highlight_colors: None,
        }
    }
}

/// 视图发给所在 Tab 的事件。
#[derive(Clone, Debug, PartialEq)]
pub enum TerminalViewEvent {
    TitleChanged,
    /// 有新的输出（后台 Tab 据此标出有新内容）
    Output,
    Bell,
    /// 用户要求打开右键菜单（窗口坐标）
    ContextMenu(gpui_kit::Point<Pixels>),
    Closed(CloseReason),
    /// 会话结束后用户按了 Enter：请重新打开
    RestartRequested,
    /// shell 集成的事件：提示符开始、命令开始与结束、通知、当前目录，以及要记进命令历史的命令
    /// （[`ShellEvent::CommandEntered`]，已按命令历史的规则筛选）
    Shell(ShellEvent),
    /// 转交输入时（[`TerminalView::set_forward_input`]）用户键入的内容：按键（已按本终端的模式编码）与输入法上屏的
    /// 文字。没有写给远端，由上层分发（同步输入）；终端自己的回应、鼠标上报、粘贴不在其中
    Typed(Vec<u8>),
    /// 用户接受了命令补全提示（整条或一个词）；接受的内容已经发出，事件中不带
    SuggestionAccepted,
}

/// 鼠标正在做什么。
#[derive(Clone, Copy, Debug, PartialEq)]
enum Pointer {
    Idle,
    /// 按住左键拖动选区
    Selecting,
    /// 已向远端上报了这个键按下（鼠标模式）
    Reporting(mouse::MouseButton),
    /// 拖动滚动条
    Scrollbar,
}

/// 字号缩放的范围（像素）。
const MIN_FONT_SIZE: f32 = 6.;
const MAX_FONT_SIZE: f32 = 64.;

/// 新输出到达后重新查找的间隔（大量输出时不必每帧都查）。
const SEARCH_REFRESH: Duration = Duration::from_millis(300);

/// 光标闪烁时亮、灭各这么久（与 Windows 默认的光标闪烁速度相同）。
pub const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(530);

/// 光标闪烁。只在光标该闪（程序用 DECSCUSR、DECSET 12 要求，或设置的默认样式闪烁）、终端获得焦点、窗口在前台且
/// 可见、会话没有结束时计时：一个每隔 [`CURSOR_BLINK_INTERVAL`] 醒一次的任务，不逐帧重画；其余时候光标常亮、
/// 不占定时器。有输出或按键时保持常亮一拍再接着闪（与常见的终端相同）。
#[derive(Default)]
struct Blink {
    /// 闪烁中灭的一拍（不画光标）
    off: bool,
    /// 有输出或按键：下一拍仍然常亮
    hold: bool,
    /// 终端获得焦点、窗口在前台且可见（焦点、窗口的激活与可见性变化时更新）
    attended: bool,
    task: Option<Task<()>>,
}

/// 网址：只识别 http 与 https，避免点击时调起任意协议的程序。
static URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"https?://[^\s<>"'`{}|\\^\x00-\x1f]+"#).expect("网址正则"));

/// 鼠标指着的链接（OSC 8 超链接或识别出的网址）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub start: GridPoint,
    pub end: GridPoint,
    pub uri: String,
}

impl Link {
    pub fn contains(&self, point: GridPoint) -> bool {
        self.start <= point && point <= self.end
    }
}

/// 只放行 http 与 https 链接。
fn is_safe_link(uri: &str) -> bool {
    let lower = uri.trim().to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://")
}

/// 进行中的查找。
struct SearchState {
    query: SearchQuery,
    regex: Regex,
    matches: Vec<SearchMatch>,
    current: Option<usize>,
    /// 上次查找时主屏幕滚动计数的读数：刷新时据此只重新查找新增的部分
    position: Option<ScrollPosition>,
}

/// 查找的结果概况（给查找栏显示"第几处 / 共几处"）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchSummary {
    /// 当前是第几处（从 1 开始）；没有匹配时为 0
    pub current: usize,
    pub total: usize,
}

pub struct TerminalView {
    engine: TerminalEngine,
    focus: FocusHandle,
    style: TerminalStyle,
    layout: Rc<Cell<Option<LayoutMetrics>>>,
    title: Option<String>,
    closed: Option<CloseReason>,
    /// 输入法的预编辑文字
    marked_text: Option<String>,
    pointer: Pointer,
    /// 上一次上报鼠标移动的格子（行、列）：同一格内的移动不重复上报
    last_motion: Option<(usize, usize)>,
    /// 触控板的像素滚动累积到一行才滚
    scroll_remainder: f32,
    /// 这个终端自己的字号调整（像素，不保存）
    zoom: f32,
    search: Option<SearchState>,
    search_refresh: Option<Task<()>>,
    hovered_link: Option<Link>,
    /// shell 最近报告的当前目录
    working_directory: Option<String>,
    /// 关键词高亮的匹配缓存（渲染时使用）
    highlight_cache: RefCell<HighlightCache>,
    /// 绘制用的可见区快照（引擎线程填好，与它交换缓冲；内容没变时不换）
    snapshot: RefCell<GridSnapshot>,
    /// 用户键入的内容不直接写给远端，改为发出 [`TerminalViewEvent::Typed`]
    forward_input: bool,
    /// kitty 键盘协议：按下时已报告、松开时也要报告的键（[`gpui_kit::Keystroke::key`]）
    reported_keys: Vec<String>,
    blink: Blink,
    /// 回放模式（见模块说明）
    replay: bool,
    /// 用户的输入写给远端之前交给它（会话录制）
    input_observer: Option<OutputObserver>,
    /// 命令补全提示的来源（[`crate::suggest`]）；`None` 时不提示
    suggestions: Option<Arc<CommandSuggestions>>,
    /// 上一次算出的补全：（已输入的内容，剩余部分）。每帧都要算，已输入的内容没变时直接用
    suggestion_cache: RefCell<Option<(String, Option<String>)>>,
    /// 已接受、远端还没有回显的补全：连同接受的部分在内这一行应有的内容。回显到达之前再接受（按住 Ctrl+→）时按它算，
    /// 同一个词不会发两次；用户另有输入时作废
    accepted: Option<String>,
    _events: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<TerminalViewEvent> for TerminalView {}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl TerminalView {
    pub fn new(
        engine: TerminalEngine,
        style: TerminalStyle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        engine.set_palette(&style.palette);
        engine.set_default_cursor(style.cursor_shape, style.cursor_blink);
        let task = Self::watch_engine(&engine, window, cx);
        let focus = cx.focus_handle();
        let subscriptions = Self::window_subscriptions(&focus, window, cx);
        Self {
            engine,
            focus,
            style,
            layout: Rc::new(Cell::new(None)),
            title: None,
            closed: None,
            marked_text: None,
            pointer: Pointer::Idle,
            last_motion: None,
            scroll_remainder: 0.,
            zoom: 0.,
            search: None,
            search_refresh: None,
            hovered_link: None,
            working_directory: None,
            highlight_cache: RefCell::new(HighlightCache::default()),
            snapshot: RefCell::default(),
            forward_input: false,
            reported_keys: Vec::new(),
            blink: Blink::default(),
            replay: false,
            input_observer: None,
            suggestions: None,
            suggestion_cache: RefCell::default(),
            accepted: None,
            _events: task,
            _subscriptions: subscriptions,
        }
    }

    /// 与所在窗口绑定的订阅：焦点进出，窗口的激活与可见性（光标闪烁跟着变）。
    fn window_subscriptions(
        focus: &FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Subscription> {
        vec![
            cx.on_focus(focus, window, |this, window, cx| {
                this.refresh_blink_attention(window, cx);
                cx.notify();
            }),
            cx.on_blur(focus, window, |this, window, cx| {
                this.refresh_blink_attention(window, cx);
                cx.notify();
            }),
            // 光标只在窗口在前台且可见时闪烁
            cx.observe_window_activation(window, |this, window, cx| {
                this.refresh_blink_attention(window, cx)
            }),
            cx.observe_window_visibility(window, |this, _, window, cx| {
                this.refresh_blink_attention(window, cx)
            }),
        ]
    }

    /// 换到另一个窗口中显示（终端的 Tab 移到别的窗口）：焦点与窗口的激活、可见性的订阅绑在登记时的窗口上，在新窗口中
    /// 重新登记（接收引擎事件的任务按视图当前所在的窗口执行，画出后自动跟过去）。引擎、画面与回滚、选区、查找、字号
    /// 调整都不变。
    pub fn rebind_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._subscriptions = Self::window_subscriptions(&self.focus, window, cx);
        self.refresh_blink_attention(window, cx);
        cx.notify();
    }

    /// 接收引擎的事件。
    fn watch_engine(
        engine: &TerminalEngine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let events = engine.events();
        cx.spawn_in(window, async move |this, cx| {
            // 会话结束后不停止接收：重连（attach）后同一个引擎继续发事件
            while let Ok(event) = events.recv().await {
                if this
                    .update_in(cx, |view, window, cx| {
                        view.on_engine_event(event, window, cx)
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
    }

    pub fn engine(&self) -> &TerminalEngine {
        &self.engine
    }

    /// 换上另一个引擎（回放跳回前面时换上从头重放好的）：选区、链接、标题随旧引擎丢弃，进行中的查找在新的内容上
    /// 重新查找。
    pub fn set_engine(
        &mut self,
        engine: TerminalEngine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        engine.set_palette(&self.style.palette);
        engine.set_default_cursor(self.style.cursor_shape, self.style.cursor_blink);
        self._events = Self::watch_engine(&engine, window, cx);
        self.engine = engine;
        self.title = None;
        self.closed = None;
        self.pointer = Pointer::Idle;
        self.last_motion = None;
        self.hovered_link = None;
        self.working_directory = None;
        self.reported_keys.clear();
        self.suggestion_cache.replace(None);
        self.accepted = None;
        *self.highlight_cache.borrow_mut() = HighlightCache::default();
        if let Some(search) = self.search.as_mut() {
            search.position = None;
            search.matches.clear();
            search.current = None;
        }
        self.refresh_search(cx);
        self.update_blink(cx);
        cx.notify();
    }

    /// 回放模式（见模块说明）。
    pub fn set_replay(&mut self, replay: bool, cx: &mut Context<Self>) {
        self.replay = replay;
        cx.notify();
    }

    pub fn is_replay(&self) -> bool {
        self.replay
    }

    /// 用户的输入（键入、粘贴、同步输入转来的）写给远端之前交给 `observer`（会话录制同时记录输入时）；`None` 取消。
    /// 终端自己的应答、鼠标上报、应用代为写入的内容（代填的 sudo 密码等）不经过它。
    pub fn set_input_observer(&mut self, observer: Option<OutputObserver>) {
        self.input_observer = observer;
    }

    // ------------------------------------------------------------ 命令补全提示

    /// 按命令历史给出补全提示（见 [`crate::suggest`]）：`suggestions` 是提示的来源（新的在前），`None` 关闭。
    pub fn set_command_suggestions(
        &mut self,
        suggestions: Option<Arc<CommandSuggestions>>,
        cx: &mut Context<Self>,
    ) {
        self.suggestions = suggestions;
        self.suggestion_cache.replace(None);
        cx.notify();
    }

    /// 有提示的来源，且会话进行中、不是回放、没有在用输入法组字（预编辑文字画在光标处）。
    fn suggestions_allowed(&self) -> bool {
        self.suggestions.as_ref().is_some_and(|s| !s.is_empty())
            && self.closed.is_none()
            && !self.replay
            && self.marked_text.is_none()
    }

    /// 这一帧要不要画命令补全提示（终端获得焦点时才画）。
    pub(crate) fn suggesting(&self, focused: bool) -> bool {
        focused && self.suggestions_allowed()
    }

    /// 已输入 `input` 时提示的剩余部分；已输入的内容与上一次相同时用上一次的结果。
    pub(crate) fn complete_input(&self, input: &PromptInput) -> Option<String> {
        let suggestions = self.suggestions.as_ref()?;
        let Ok(mut cache) = self.suggestion_cache.try_borrow_mut() else {
            return suggestions.complete(&input.text).map(str::to_string);
        };
        if let Some((typed, rest)) = cache.as_ref()
            && *typed == input.text
        {
            return rest.clone();
        }
        let rest = suggestions.complete(&input.text).map(str::to_string);
        *cache = Some((input.text.clone(), rest.clone()));
        rest
    }

    /// 现在给出的命令补全提示（光标后淡色显示的剩余部分）；不在提示符后输入命令、没有匹配的历史时为 `None`。
    pub fn command_suggestion(&self) -> Option<String> {
        if !self.suggestions_allowed() {
            return None;
        }
        self.complete_input(&self.engine.prompt_input()?)
    }

    /// 接受命令补全提示：整条，或下一个词（`word`）；作为键入发给远端（同步输入时转给各终端）。没有提示时返回
    /// `false`，按键照常处理。
    fn accept_suggestion(&mut self, word: bool, cx: &mut Context<Self>) -> bool {
        let Some(suggestions) = self
            .suggestions
            .clone()
            .filter(|_| self.suggestions_allowed())
        else {
            return false;
        };
        let Some(input) = self.engine.prompt_input() else {
            return false;
        };
        // 上次接受的还没有回显：按连同它在内应有的内容算
        let typed = match self.accepted.take() {
            Some(expected)
                if expected.len() > input.text.len() && expected.starts_with(&input.text) =>
            {
                expected
            }
            _ => input.text,
        };
        let Some(rest) = suggestions.complete(&typed) else {
            return false;
        };
        let accepted = if word { suggest::next_word(rest) } else { rest };
        if accepted.is_empty() {
            return false;
        }
        // 接受的内容不写进日志（可能含秘密）
        tracing::debug!(chars = accepted.chars().count(), word, "接受命令补全提示");
        let expected = format!("{typed}{accepted}");
        self.typed(accepted.as_bytes(), cx);
        self.accepted = Some(expected);
        cx.emit(TerminalViewEvent::SuggestionAccepted);
        true
    }

    /// 程序设置的标题（OSC 0 / 2）。
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// 会话结束的原因；还在运行时为 `None`。
    pub fn closed(&self) -> Option<&CloseReason> {
        self.closed.as_ref()
    }

    /// 接上新的传输（重连）：画面与回滚保留，会话重新算作进行中。
    pub fn attach(&mut self, io: termlet_io::TerminalIo, cx: &mut Context<Self>) {
        self.engine.attach(io);
        self.closed = None;
        self.working_directory = None;
        self.accepted = None;
        self.update_blink(cx);
        cx.notify();
    }

    /// 当前会话的 shell 是否开启了 shell 集成（见过 OSC 133 / 633 的标记），见 [`TerminalEngine::has_shell_integration`]。
    pub fn has_shell_integration(&self) -> bool {
        self.engine.has_shell_integration()
    }

    /// shell 最近报告的当前目录（OSC 7 等）；重连后清空。
    pub fn working_directory(&self) -> Option<&str> {
        self.working_directory.as_deref()
    }

    /// 屏幕上当前的输入行（同步输入按回车前检查危险命令），见 [`TerminalEngine::input_line`]；会话已结束时为 `None`。
    pub fn input_line(&self) -> Option<String> {
        if self.closed.is_some() {
            return None;
        }
        self.engine.input_line()
    }

    /// 屏幕上显示的文字（自动换行的多行拼成一行，各行以换行分隔）。
    pub fn screen_text(&self) -> String {
        self.engine.screen_text()
    }

    /// 关键词高亮的匹配缓存（给绘制用）。
    pub(crate) fn highlight_cache(&self) -> &RefCell<HighlightCache> {
        &self.highlight_cache
    }

    /// 可见区快照的缓冲（给绘制用）。
    pub(crate) fn snapshot_buffer(&self) -> &RefCell<GridSnapshot> {
        &self.snapshot
    }

    pub fn style(&self) -> &TerminalStyle {
        &self.style
    }

    /// 实际使用的样式（加上这个终端自己的字号调整）。
    fn effective_style(&self) -> TerminalStyle {
        let mut style = self.style.clone();
        let size = (f32::from(style.font_size) + self.zoom).clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
        style.font_size = px(size);
        style
    }

    fn zoom_by(&mut self, delta: f32, cx: &mut Context<Self>) {
        let base = f32::from(self.style.font_size);
        let next = (base + self.zoom + delta).clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
        self.zoom = next - base;
        tracing::debug!(font_size = next, "终端字号调整");
        cx.notify();
    }

    fn zoom_reset(&mut self, cx: &mut Context<Self>) {
        tracing::debug!("终端字号恢复");
        self.zoom = 0.;
        cx.notify();
    }

    // ------------------------------------------------------------ 查找

    /// 开始或更新查找；`None` 结束查找。正则不合法时返回错误，原来的查找保持不变。
    pub fn set_search(
        &mut self,
        query: Option<SearchQuery>,
        cx: &mut Context<Self>,
    ) -> Result<SearchSummary, regex::Error> {
        let Some(query) = query else {
            if self.search.is_some() {
                tracing::debug!("终端结束查找");
            }
            self.search = None;
            self.search_refresh = None;
            cx.notify();
            return Ok(SearchSummary::default());
        };
        let Some(regex) = query.compile()? else {
            self.search = None;
            cx.notify();
            return Ok(SearchSummary::default());
        };
        let started = std::time::Instant::now();
        let found = self.engine.search(&regex, None);
        let (matches, position) = (found.matches, found.position);
        // 查找的文字不写进日志（可能是终端里的内容）
        tracing::debug!(
            matches = matches.len(),
            regex = query.regex,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "终端查找"
        );
        // 从最近的输出开始：选最后一处
        let current = matches.len().checked_sub(1);
        self.search = Some(SearchState {
            query,
            regex,
            matches,
            current,
            position,
        });
        self.reveal_current_match();
        cx.notify();
        Ok(self.search_summary())
    }

    /// 跳到下一处（`forward` 为向下）或上一处，循环。
    pub fn search_step(&mut self, forward: bool, cx: &mut Context<Self>) -> SearchSummary {
        if let Some(search) = self.search.as_mut()
            && !search.matches.is_empty()
        {
            let total = search.matches.len();
            search.current = Some(match (search.current, forward) {
                (Some(i), true) => (i + 1) % total,
                (Some(i), false) => (i + total - 1) % total,
                (None, true) => 0,
                (None, false) => total - 1,
            });
            self.reveal_current_match();
            cx.notify();
        }
        self.search_summary()
    }

    pub fn search_summary(&self) -> SearchSummary {
        match &self.search {
            Some(search) => SearchSummary {
                current: search.current.map_or(0, |i| i + 1),
                total: search.matches.len(),
            },
            None => SearchSummary::default(),
        }
    }

    pub fn search_query(&self) -> Option<&SearchQuery> {
        self.search.as_ref().map(|s| &s.query)
    }

    /// 给绘制用：全部匹配（按位置排列）与当前那一处。
    pub(crate) fn search_highlights(&self) -> (&[SearchMatch], Option<SearchMatch>) {
        match &self.search {
            Some(search) => (
                &search.matches,
                search.current.and_then(|i| search.matches.get(i).copied()),
            ),
            None => (&[], None),
        }
    }

    fn reveal_current_match(&mut self) {
        let Some(found) = self
            .search
            .as_ref()
            .and_then(|s| s.current.and_then(|i| s.matches.get(i)))
            .copied()
        else {
            return;
        };
        self.engine.scroll_to(found.start);
    }

    /// 有新输出时重新查找（节流）：行会随输出滚进回滚，记下的位置需要更新。
    fn schedule_search_refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.is_none() || self.search_refresh.is_some() {
            return;
        }
        self.search_refresh = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(SEARCH_REFRESH).await;
            let _ = this.update(cx, |view, cx| {
                view.search_refresh = None;
                view.refresh_search(cx);
            });
        }));
    }

    fn refresh_search(&mut self, cx: &mut Context<Self>) {
        let Some(search) = self.search.as_mut() else {
            return;
        };
        let total_before = search.matches.len();
        let previous = search.current;
        let current_match = previous.and_then(|i| search.matches.get(i)).copied();
        // 同一块主屏幕、没有重排或清空回滚：回滚中的行没变，只查新增的部分；否则整个重新查找。上次的结果交给引擎线程
        // （移过去，不复制），换回新的
        let matches = std::mem::take(&mut search.matches);
        let found = self.engine.search(
            &search.regex,
            search.position.map(|position| (position, matches)),
        );
        let scrolled = found.scrolled;
        search.matches = found.matches;
        search.position = found.position;
        // 当前那一处跟着内容上移后仍在时保持不变；否则按"距离最后一处的个数"
        let followed = scrolled
            .zip(current_match)
            .and_then(|(lines, found)| found.shifted(lines as i64))
            .and_then(|moved| {
                search
                    .matches
                    .binary_search_by(|m| m.start.cmp(&moved.start))
                    .ok()
            });
        search.current = match (followed, previous) {
            (Some(i), _) => Some(i),
            (None, Some(i)) if i < total_before => {
                let from_end = total_before - 1 - i;
                search.matches.len().checked_sub(1 + from_end)
            }
            _ => search.matches.len().checked_sub(1),
        };
        cx.notify();
    }

    // ------------------------------------------------------------ 链接

    pub(crate) fn hovered_link(&self) -> Option<Link> {
        self.hovered_link.clone()
    }

    /// 格子处的链接：先看 OSC 8 超链接，再在所在的逻辑行里找网址。
    fn link_at(&self, point: GridPoint) -> Option<Link> {
        if let Some(uri) = self.engine.hyperlink_at(point)
            && is_safe_link(&uri)
        {
            return Some(Link {
                start: point,
                end: point,
                uri,
            });
        }
        let line = self.engine.logical_line_at(point)?;
        let offset = line.offset_of(point)?;
        URL.find_iter(&line.text)
            .map(|m| (m.start(), trim_url(m.as_str())))
            .find(|(start, url)| *start <= offset && offset < start + url.len())
            .and_then(|(start, url)| {
                let range = line.points(start, start + url.len())?;
                Some(Link {
                    start: *range.start(),
                    end: *range.end(),
                    uri: url.to_string(),
                })
            })
    }

    fn update_hovered_link(
        &mut self,
        position: gpui_kit::Point<Pixels>,
        ctrl: bool,
        cx: &mut Context<Self>,
    ) {
        let link = if ctrl {
            self.grid_point(position)
                .and_then(|(point, ..)| self.link_at(point))
        } else {
            None
        };
        if link != self.hovered_link {
            self.hovered_link = link;
            cx.notify();
        }
    }

    pub fn set_style(&mut self, style: TerminalStyle, cx: &mut Context<Self>) {
        if style.font_family != self.style.font_family || style.font_size != self.style.font_size {
            tracing::debug!(
                font = %style.font_family,
                font_size = f32::from(style.font_size),
                "终端字体变化"
            );
        }
        self.engine.set_palette(&style.palette);
        self.engine
            .set_default_cursor(style.cursor_shape, style.cursor_blink);
        self.style = style;
        self.update_blink(cx);
        cx.notify();
    }

    /// 光标这一拍是否画出（闪烁中灭的一拍为 false；不闪烁时总是 true）。
    pub fn cursor_shown(&self) -> bool {
        !self.blink.off
    }

    /// 光标正在闪烁（在计时）：光标该闪、终端获得焦点、窗口在前台且可见、会话没有结束。
    pub fn cursor_blinking(&self) -> bool {
        self.blink.task.is_some()
    }

    /// 焦点、窗口的激活或可见性变了：重新判断光标该不该闪。
    fn refresh_blink_attention(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.blink.attended =
            self.focus.is_focused(window) && window.is_window_active() && window.is_visible();
        self.update_blink(cx);
    }

    /// 按现在的状态开始或停止闪烁；停止时光标常亮。
    fn update_blink(&mut self, cx: &mut Context<Self>) {
        let blink = self.blink.attended && self.closed.is_none() && self.engine.cursor_blinking();
        if !blink {
            self.blink.task = None;
            self.blink.hold = false;
            if std::mem::take(&mut self.blink.off) {
                cx.notify();
            }
            return;
        }
        if self.blink.task.is_none() {
            self.blink.task = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(CURSOR_BLINK_INTERVAL).await;
                    if this.update(cx, |view, cx| view.blink_tick(cx)).is_err() {
                        break;
                    }
                }
            }));
        }
    }

    /// 闪烁的一拍：刚有过输出或按键时保持常亮，否则亮灭互换。
    fn blink_tick(&mut self, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.blink.hold) {
            return;
        }
        self.blink.off = !self.blink.off;
        // 程序隐藏了光标（DECTCEM）时画面不变，不用重画
        if self.mode().show_cursor {
            cx.notify();
        }
    }

    /// 有输出或按键：闪烁中的光标常亮，下一拍再接着闪（调用方随后会重画）。
    fn hold_cursor(&mut self) {
        if self.blink.task.is_some() {
            self.blink.hold = true;
            self.blink.off = false;
        }
    }

    /// 改变回滚的行数（设置变了），见 [`TerminalEngine::set_scrollback`]：变小时截掉的历史中的查找结果随之去掉。
    pub fn set_scrollback(&mut self, lines: usize, cx: &mut Context<Self>) {
        if self.engine.set_scrollback(lines) {
            self.refresh_search(cx);
            cx.notify();
        }
    }

    /// 网格随布局变化（由绘制元素在 prepaint 中调用）。
    pub(crate) fn resize_to(&self, metrics: &LayoutMetrics) {
        // 回放：网格是录制时的大小
        if self.replay {
            return;
        }
        let size = GridSize {
            columns: metrics.columns.min(u16::MAX as usize) as u16,
            rows: metrics.rows.min(u16::MAX as usize) as u16,
            cell_width: f32::from(metrics.cell_width).round() as u16,
            cell_height: f32::from(metrics.line_height).round() as u16,
        };
        self.engine.resize(size);
    }

    fn on_engine_event(&mut self, event: EngineEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            EngineEvent::Wakeup => {
                self.engine.clear_dirty();
                self.hold_cursor();
                self.schedule_search_refresh(window, cx);
                cx.emit(TerminalViewEvent::Output);
                cx.notify();
            }
            EngineEvent::Title => {
                if let Some(title) = self.engine.take_title() {
                    self.title = title;
                    cx.emit(TerminalViewEvent::TitleChanged);
                }
            }
            EngineEvent::Bell => {
                self.engine.clear_bell();
                cx.emit(TerminalViewEvent::Bell);
            }
            // 程序改了光标的形状或闪烁（DECSCUSR、DECSET 12、RIS）
            EngineEvent::CursorBlinkingChange => {
                self.update_blink(cx);
                cx.notify();
            }
            EngineEvent::Shell(event) => {
                if let ShellEvent::WorkingDirectory(path) = &event {
                    self.working_directory = Some(path.clone());
                }
                cx.emit(TerminalViewEvent::Shell(event));
            }
            // 已经换上了新传输（重连）：旧传输的结束不算
            EngineEvent::Closed { transport, .. }
                if !self.engine.is_current_transport(transport) => {}
            EngineEvent::Closed { reason, .. } => {
                self.closed = Some(reason.clone());
                self.update_blink(cx);
                cx.emit(TerminalViewEvent::Closed(reason));
                cx.notify();
            }
        }
    }

    fn mode(&self) -> TerminalModes {
        self.engine.modes()
    }

    /// 远端开启了括号粘贴模式（DEC 2004）：粘贴的多行内容不会被当成逐行回车执行。
    pub fn bracketed_paste(&self) -> bool {
        self.mode().bracketed_paste
    }

    /// 是否在备用屏幕中（vim、htop 等全屏程序），见 [`TerminalEngine::is_alt_screen`]。
    pub fn is_alt_screen(&self) -> bool {
        self.engine.is_alt_screen()
    }

    /// 远端开启了应用光标键模式（DECCKM）：方向键发 `ESC O x`。
    pub fn app_cursor(&self) -> bool {
        self.mode().app_cursor
    }

    /// 同步输入：用户键入的内容改为发出 [`TerminalViewEvent::Typed`]，由上层分发给各终端
    /// （它们再用 [`Self::write_typed`] 写入）。
    pub fn set_forward_input(&mut self, forward: bool) {
        self.forward_input = forward;
    }

    /// 写入别的终端转来的键入内容：方向键与 Home / End 按本终端的光标键模式重新编码。
    pub fn write_typed(&mut self, data: &[u8], cx: &mut Context<Self>) {
        let data = keys::adapt_cursor_keys(data, self.app_cursor());
        self.write_input(&data, cx);
    }

    /// 用户键入的内容：转交给上层（同步输入）或直接写给远端。
    fn typed(&mut self, data: &[u8], cx: &mut Context<Self>) {
        if self.replay {
            return;
        }
        self.accepted = None;
        self.hold_cursor();
        if self.forward_input {
            if !data.is_empty() {
                self.engine.scroll_to_bottom_and_deselect();
                cx.emit(TerminalViewEvent::Typed(data.to_vec()));
            }
        } else {
            self.write_input(data, cx);
        }
    }

    /// 用户的输入：回到最底部、取消选区，写给远端。用户的输入都应经过这里（命令历史据此判断"用户输入过"、
    /// 没有 shell 集成时据此推断命令）；登录后由应用自动执行的命令直接用 [`TerminalEngine::write`]。
    pub fn write_input(&mut self, data: &[u8], cx: &mut Context<Self>) {
        if self.closed.is_some() || self.replay || data.is_empty() {
            return;
        }
        if let Some(observer) = &self.input_observer {
            observer(data);
        }
        self.accepted = None;
        self.hold_cursor();
        // 回到最底部、取消选区；命令历史：没有 shell 集成时按回车推断（远端迟迟没有换行时引擎线程超时后也读一次）。
        // 不等引擎线程；在写给远端之前发出，回显一定在它之后处理
        self.engine.user_input(data);
        self.engine.write(data);
        cx.notify();
    }

    /// 粘贴一段文字（调用方已做粘贴防护）。
    pub fn paste_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let bytes = keys::encode_paste(&strip_paste_controls(text), self.mode());
        self.write_input(&bytes, cx);
    }

    /// 选中的文字。
    pub fn selection_text(&self) -> Option<String> {
        self.engine.selection_text().filter(|s| !s.is_empty())
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        // 回放：按键交给外层（播放、暂停、跳转）
        if self.replay {
            return;
        }
        // 会话已结束：按 Enter 请所在的 Tab 重新打开
        if self.closed.is_some() {
            let m = &event.keystroke.modifiers;
            if event.keystroke.key == "enter" && !m.control && !m.alt && !m.shift && !m.platform {
                cx.emit(TerminalViewEvent::RestartRequested);
                cx.stop_propagation();
            }
            return;
        }
        // 命令补全提示：光标在行尾（有提示时总是）按 → 或 End 接受整条；没有提示时照常发给远端
        let m = &event.keystroke.modifiers;
        if matches!(event.keystroke.key.as_str(), "right" | "end")
            && !m.control
            && !m.alt
            && !m.shift
            && !m.platform
            && self.accept_suggestion(false, cx)
        {
            cx.stop_propagation();
            return;
        }
        let kind = if event.is_held {
            keys::KeyEventKind::Repeat
        } else {
            keys::KeyEventKind::Press
        };
        let flags = self.keyboard_flags();
        let alt = keys::AltKey::for_settings(cx.platform_info().os, self.style.option_as_meta);
        if let Some(key) = keys::encode_key(&event.keystroke, self.mode(), flags, kind, alt) {
            let name = &event.keystroke.key;
            if key.report_release && !self.reported_keys.contains(name) {
                self.reported_keys.push(name.clone());
            }
            self.typed(&key.bytes, cx);
            cx.stop_propagation();
        }
    }

    /// 松开按键：只报告按下时已用 kitty 键盘协议报告过的键（被快捷键用掉的不报告）。
    fn key_up(&mut self, event: &KeyUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.replay {
            return;
        }
        let name = &event.keystroke.key;
        let Some(index) = self.reported_keys.iter().position(|k| k == name) else {
            return;
        };
        self.reported_keys.swap_remove(index);
        if self.closed.is_some() {
            return;
        }
        let flags = self.keyboard_flags();
        let release = keys::KeyEventKind::Release;
        let alt = keys::AltKey::for_settings(cx.platform_info().os, self.style.option_as_meta);
        if let Some(key) = keys::encode_key(&event.keystroke, self.mode(), flags, release, alt) {
            self.typed(&key.bytes, cx);
            cx.stop_propagation();
        }
    }

    /// 编码按键用的 kitty 键盘协议标志。同步输入时按旧的编码：内容会转给各个终端，它们不一定开启了协议。
    fn keyboard_flags(&self) -> KeyboardFlags {
        if self.forward_input {
            KeyboardFlags::NONE
        } else {
            self.engine.keyboard_flags()
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.selection_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        self.paste_from_clipboard(cx);
    }

    /// 右键粘贴：与快捷键一样走 [`Paste`] 动作（之后才执行），上层可以用 `capture_action` 先拦下做粘贴防护。
    fn request_paste(window: &mut Window, cx: &mut Context<Self>) {
        window.dispatch_action(Box::new(Paste), cx);
    }

    /// 粘贴剪贴板中的文字（不经过粘贴防护；用户的粘贴请走 [`Paste`] 动作）。
    pub fn paste_from_clipboard(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.paste_text(&text, cx);
        }
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.engine.select_all();
        cx.notify();
    }

    fn clear(&mut self, _: &Clear, _: &mut Window, cx: &mut Context<Self>) {
        self.clear_buffer(cx);
    }

    /// 清空回滚与屏幕，提示符所在的行成为第一行（与 xterm.js 的 clear 相同）。
    pub fn clear_buffer(&mut self, cx: &mut Context<Self>) {
        self.engine.clear_buffer();
        cx.notify();
    }

    fn scroll(&mut self, scroll: Scroll, cx: &mut Context<Self>) {
        self.engine.scroll_display(scroll);
        cx.notify();
    }

    /// 窗口坐标对应的网格位置（考虑回滚的偏移）与落在格子的哪一侧。
    fn grid_point(
        &self,
        position: gpui_kit::Point<Pixels>,
    ) -> Option<(GridPoint, Side, usize, usize)> {
        let metrics = self.layout.get()?;
        let (row, column, right) = metrics.cell_at(position);
        let offset = self.engine.dimensions().display_offset as i32;
        let point = GridPoint::new(row as i32 - offset, column);
        let side = if right { Side::Right } else { Side::Left };
        Some((point, side, row, column))
    }

    fn mouse_modifiers(modifiers: &gpui_kit::Modifiers) -> mouse::MouseModifiers {
        mouse::MouseModifiers {
            shift: modifiers.shift,
            alt: modifiers.alt,
            control: modifiers.control,
        }
    }

    fn report_mouse(
        &mut self,
        action: mouse::MouseAction,
        button: mouse::MouseButton,
        modifiers: &gpui_kit::Modifiers,
        row: usize,
        column: usize,
    ) -> bool {
        let mode = self.mode();
        // 按下、松开之后的移动重新从这一格算起
        self.last_motion = (action == mouse::MouseAction::Motion).then_some((row, column));
        match mouse::encode(
            mode,
            action,
            button,
            Self::mouse_modifiers(modifiers),
            column,
            row,
        ) {
            Some(bytes) => {
                self.engine.write(&bytes);
                true
            }
            None => false,
        }
    }

    /// 鼠标移动的上报：与上一次在同一格时不报（每次上报都要写给远端）。
    fn report_motion(
        &mut self,
        button: mouse::MouseButton,
        modifiers: &gpui_kit::Modifiers,
        position: gpui_kit::Point<Pixels>,
    ) {
        if let Some((_, _, row, column)) = self.grid_point(position)
            && self.last_motion != Some((row, column))
        {
            self.report_mouse(mouse::MouseAction::Motion, button, modifiers, row, column);
        }
    }

    /// 鼠标按下；返回终端是否处理了它。没有处理的（右键设为 [`RightClick::Menu`] 时）由元素放行，
    /// 交给外层弹出菜单。
    pub(crate) fn mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((point, side, row, column)) = self.grid_point(event.position) else {
            return true;
        };
        if event.button == MouseButton::Left && self.on_scrollbar(event.position) {
            self.pointer = Pointer::Scrollbar;
            self.drag_scrollbar(event.position, cx);
            return true;
        }
        if event.button == MouseButton::Left
            && event.modifiers.secondary(cx)
            && let Some(link) = self.link_at(point)
        {
            cx.open_url(&link.uri);
            return true;
        }
        let button = match event.button {
            MouseButton::Left => mouse::MouseButton::Left,
            MouseButton::Middle => mouse::MouseButton::Middle,
            MouseButton::Right => mouse::MouseButton::Right,
            _ => return true,
        };
        // 远端开启了鼠标模式时上报（按住 Shift 强制本地选择）
        if !self.replay
            && self.mode().mouse_tracking != MouseTracking::Off
            && !event.modifiers.shift
        {
            if self.report_mouse(
                mouse::MouseAction::Press,
                button,
                &event.modifiers,
                row,
                column,
            ) {
                self.pointer = Pointer::Reporting(button);
            }
            return true;
        }
        match event.button {
            MouseButton::Left => {
                let kind = match event.click_count {
                    2 => SelectionKind::Semantic,
                    n if n >= 3 => SelectionKind::Lines,
                    _ if event.modifiers.alt => SelectionKind::Block,
                    _ => SelectionKind::Simple,
                };
                // 按住 Shift 单击：把已有的选区延伸到这里
                let extended = event.modifiers.shift
                    && event.click_count == 1
                    && self.engine.extend_selection(point, side);
                if !extended {
                    self.engine.start_selection(kind, point, side);
                }
                self.pointer = Pointer::Selecting;
                cx.notify();
            }
            MouseButton::Right => match self.style.right_click {
                RightClick::Auto => match self.selection_text() {
                    Some(text) => {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                        self.engine.clear_selection();
                        cx.notify();
                    }
                    None => Self::request_paste(window, cx),
                },
                RightClick::Paste => Self::request_paste(window, cx),
                RightClick::Menu => {
                    cx.emit(TerminalViewEvent::ContextMenu(event.position));
                    return false;
                }
            },
            _ => {}
        }
        true
    }

    /// 鼠标移动（窗口中每个可见的终端都会收到）：`hovered` 是指针在不在这个终端上。
    pub(crate) fn mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        hovered: bool,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let secondary = event.modifiers.secondary(cx);
        self.update_hovered_link(event.position, hovered && secondary, cx);
        match self.pointer {
            Pointer::Scrollbar if event.pressed_button == Some(MouseButton::Left) => {
                self.drag_scrollbar(event.position, cx);
            }
            Pointer::Selecting if event.pressed_button == Some(MouseButton::Left) => {
                let Some(metrics) = self.layout.get() else {
                    return;
                };
                // 拖到上下边缘之外时滚动
                if event.position.y < metrics.origin.y {
                    self.engine.scroll_display(Scroll::Delta(1));
                } else if event.position.y
                    > metrics.origin.y + metrics.line_height * metrics.rows as f32
                {
                    self.engine.scroll_display(Scroll::Delta(-1));
                }
                if let Some((point, side, _, _)) = self.grid_point(event.position) {
                    self.engine.update_selection(point, side);
                }
                cx.notify();
            }
            // 按住键拖动：指针离开终端也照常上报（落在边缘的格子）
            Pointer::Reporting(button) => {
                self.report_motion(button, &event.modifiers, event.position);
            }
            _ => {
                if !hovered {
                    self.last_motion = None;
                } else if event.pressed_button.is_none()
                    && !self.replay
                    && self.mode().mouse_tracking == MouseTracking::Motion
                {
                    self.report_motion(mouse::MouseButton::None, &event.modifiers, event.position);
                }
            }
        }
    }

    pub(crate) fn mouse_up(
        &mut self,
        event: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match std::mem::replace(&mut self.pointer, Pointer::Idle) {
            Pointer::Selecting => {
                if self.engine.selection_is_empty() {
                    self.engine.clear_selection();
                } else if self.style.copy_on_select
                    && let Some(text) = self.selection_text()
                {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
                cx.notify();
            }
            Pointer::Reporting(button) => {
                if let Some((_, _, row, column)) = self.grid_point(event.position) {
                    self.report_mouse(
                        mouse::MouseAction::Release,
                        button,
                        &event.modifiers,
                        row,
                        column,
                    );
                }
            }
            Pointer::Scrollbar | Pointer::Idle => {}
        }
    }

    /// 是否点在右边缘的滚动条上（有回滚时）。
    fn on_scrollbar(&self, position: gpui_kit::Point<Pixels>) -> bool {
        let Some(metrics) = self.layout.get() else {
            return false;
        };
        let has_history = self.engine.dimensions().history_size > 0;
        has_history && position.x >= metrics.bounds.right() - px(10.)
    }

    /// 按指针的高度滚到对应位置。
    fn drag_scrollbar(&mut self, position: gpui_kit::Point<Pixels>, cx: &mut Context<Self>) {
        let Some(metrics) = self.layout.get() else {
            return;
        };
        let height = f32::from(metrics.bounds.size.height).max(1.);
        let ratio = (f32::from(position.y - metrics.bounds.top()) / height).clamp(0., 1.);
        let history = self.engine.dimensions().history_size as f32;
        let target = ((1. - ratio) * history).round() as usize;
        self.engine.scroll_display(Scroll::Offset(target));
        cx.notify();
    }

    pub(crate) fn scroll_wheel(
        &mut self,
        event: &ScrollEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(metrics) = self.layout.get() else {
            return;
        };
        if event.modifiers.secondary(cx) {
            let delta = event.delta.pixel_delta(metrics.line_height).y;
            if delta != px(0.) {
                self.zoom_by(if delta > px(0.) { 1. } else { -1. }, cx);
            }
            return;
        }
        let delta = event.delta.pixel_delta(metrics.line_height).y;
        self.scroll_remainder += f32::from(delta) / f32::from(metrics.line_height);
        let lines = self.scroll_remainder.trunc() as i32;
        if lines == 0 {
            return;
        }
        self.scroll_remainder -= lines as f32;
        let mode = self.mode();
        let Some((_, _, row, column)) = self.grid_point(event.position) else {
            return;
        };
        let button = if lines > 0 {
            mouse::MouseButton::WheelUp
        } else {
            mouse::MouseButton::WheelDown
        };
        // 回放：没有程序可以上报，滚轮只滚动回滚
        let mode = if self.replay {
            TerminalModes::default()
        } else {
            mode
        };
        if mode.mouse_tracking != MouseTracking::Off && !event.modifiers.shift {
            for _ in 0..lines.unsigned_abs() {
                self.report_mouse(
                    mouse::MouseAction::Press,
                    button,
                    &event.modifiers,
                    row,
                    column,
                );
            }
            return;
        }
        // 备用屏（vim、less 等）里滚轮转成方向键
        if mode.alt_screen && mode.alternate_scroll && !event.modifiers.shift {
            let key: &[u8] = match (lines > 0, mode.app_cursor) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            let bytes: Vec<u8> = key.repeat(lines.unsigned_abs() as usize);
            self.engine.write(&bytes);
            return;
        }
        self.scroll(Scroll::Delta(lines), cx);
    }

    /// 光标所在格子的窗口坐标（输入法候选框跟随光标）。
    fn cursor_bounds(&self) -> Option<Bounds<Pixels>> {
        let metrics = self.layout.get()?;
        let point = self.engine.cursor_point();
        let offset = self.engine.dimensions().display_offset as i32;
        let row = point.line + offset;
        if row < 0 || row as usize >= metrics.rows {
            return None;
        }
        Some(metrics.cell_bounds(row as usize, point.column, 1))
    }
}

/// 去掉网址末尾多半不属于它的标点（右括号只在网址中没有左括号时去掉）。
fn trim_url(url: &str) -> &str {
    let mut end = url.len();
    while let Some(c) = url[..end].chars().last() {
        let drop = match c {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' => true,
            ')' => url[..end].matches('(').count() < url[..end].matches(')').count(),
            ']' => url[..end].matches('[').count() < url[..end].matches(']').count(),
            _ => false,
        };
        if !drop {
            break;
        }
        end -= c.len_utf8();
    }
    &url[..end]
}

/// 去掉粘贴内容中的控制字符（保留制表符与换行），防止夹带的转义序列提前结束括号粘贴、伪造输入。
fn strip_paste_controls(text: &str) -> String {
    text.chars()
        .filter(|&c| !c.is_control() || matches!(c, '\t' | '\n' | '\r'))
        .collect()
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus.is_focused(window);
        let mut key_context = KeyContext::default();
        key_context.add(KEY_CONTEXT);
        // 回放与会话结束一样：Tab、Ctrl+C 不留给远端
        if self.closed.is_some() || self.replay {
            key_context.add(CLOSED);
        }
        if self.replay {
            key_context.add(REPLAY);
        }
        div()
            .id("terminal")
            .test_support()
            .size_full()
            .track_focus(&self.focus)
            .key_context(key_context)
            .on_key_down(cx.listener(Self::key_down))
            .on_key_up(cx.listener(Self::key_up))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::clear))
            .on_action(
                cx.listener(|this, _: &ScrollLineUp, _, cx| this.scroll(Scroll::Delta(1), cx)),
            )
            .on_action(
                cx.listener(|this, _: &ScrollLineDown, _, cx| this.scroll(Scroll::Delta(-1), cx)),
            )
            .on_action(cx.listener(|this, _: &ScrollPageUp, _, cx| this.scroll(Scroll::PageUp, cx)))
            .on_action(
                cx.listener(|this, _: &ScrollPageDown, _, cx| this.scroll(Scroll::PageDown, cx)),
            )
            .on_action(cx.listener(|this, _: &ScrollToTop, _, cx| this.scroll(Scroll::Top, cx)))
            .on_action(
                cx.listener(|this, _: &ScrollToBottom, _, cx| this.scroll(Scroll::Bottom, cx)),
            )
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom_by(1., cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom_by(-1., cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, _, cx| this.zoom_reset(cx)))
            .on_action(cx.listener(|this, _: &AcceptSuggestionWord, _, cx| {
                // 没有提示：交给按键监听器照常编码发给远端（shell 中多半是按词右移）
                if !this.accept_suggestion(true, cx) {
                    cx.propagate();
                }
            }))
            .child(TerminalElement::new(
                cx.entity(),
                self.focus.clone(),
                self.effective_style(),
                self.layout.clone(),
                focused,
                !self.blink.off,
                self.marked_text.clone(),
            ))
    }
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_text
            .as_ref()
            .map(|text| 0..text.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked_text = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text = None;
        self.typed(text.as_bytes(), cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text = (!text.is_empty()).then(|| text.to_string());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        self.cursor_bounds()
    }

    fn character_index_for_point(
        &mut self,
        _: gpui_kit::Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{Action as _, Keystroke};

    use super::*;

    /// `os` 的默认快捷键中按下 `combo` 触发的动作（终端层；解除外层动作的 `Unbind` 不算）。
    fn bound(os: OperatingSystem, combo: &str) -> Option<&'static str> {
        let typed = Keystroke::parse_with_os(combo, os).unwrap();
        let unbind = Unbind("".into()).name();
        key_bindings(os)
            .into_iter()
            .filter(|b| b.action().name() != unbind)
            .find(|b| {
                let stroke = &b.keystrokes()[0];
                stroke.key() == typed.key && stroke.modifiers() == &typed.modifiers
            })
            .map(|b| b.action().name())
    }

    /// `os` 的默认快捷键中解除了哪些外层动作（组合 → 动作名，生效的上下文）。
    fn unbound(os: OperatingSystem) -> Vec<(String, String, String)> {
        let unbind = Unbind("".into()).name();
        key_bindings(os)
            .into_iter()
            .filter(|b| b.action().name() == unbind)
            .map(|b| {
                let action = b
                    .action()
                    .as_any()
                    .downcast_ref::<Unbind>()
                    .map(|u| u.0.to_string())
                    .unwrap_or_default();
                let context = b.predicate().map(|p| p.to_string()).unwrap_or_default();
                (b.keystrokes()[0].inner().unparse(os), action, context)
            })
            .collect()
    }

    // 会话进行中解除 gpui-kit 的 Root 抢走的键：Tab、Shift+Tab（切换焦点），Windows、Linux 上的 Ctrl+C（Root 绑给了
    // 输入框的复制；焦点路径上有别的元素处理它时 shell 收不到中断）。macOS 上 Root 绑的是 ⌘C，不用解除
    #[test]
    fn root_keys_are_released_while_the_session_runs() {
        let live = format!("{KEY_CONTEXT} && !{CLOSED}");
        for os in [OperatingSystem::Windows, OperatingSystem::Linux] {
            let released = unbound(os);
            for (combo, action) in [
                ("tab", "root::Tab"),
                ("shift-tab", "root::TabPrev"),
                ("ctrl-c", "input::Copy"),
            ] {
                assert!(
                    released
                        .iter()
                        .any(|(c, a, p)| c == combo && a == action && *p == live),
                    "{os:?} 上解除 {combo} 的 {action}：{released:?}"
                );
            }
        }
        let mac = unbound(OperatingSystem::Mac);
        assert!(mac.iter().all(|(c, _, _)| c != "ctrl-c"), "{mac:?}");
        assert!(mac.iter().any(|(c, _, _)| c == "tab"));
    }

    // 复制、粘贴的按键判定：macOS 上 ⌘C / ⌘V，Ctrl 组合一个也不占（Ctrl+C、Ctrl+V 给 shell）；
    // Windows、Linux 上 Ctrl+Shift+C / V 与 Ctrl+Insert、Shift+Insert，同样不占 Ctrl+C、Ctrl+V
    #[test]
    fn copy_and_paste_keys_follow_the_os() {
        let mac = OperatingSystem::Mac;
        assert_eq!(bound(mac, "cmd-c"), Some(Copy.name()));
        assert_eq!(bound(mac, "cmd-v"), Some(Paste.name()));
        assert_eq!(bound(mac, "cmd-a"), Some(SelectAll.name()));
        assert_eq!(bound(mac, "cmd-k"), Some(Clear.name()));
        for combo in [
            "ctrl-c",
            "ctrl-v",
            "ctrl-shift-c",
            "ctrl-shift-v",
            "ctrl-a",
            "ctrl-k",
        ] {
            assert_eq!(bound(mac, combo), None, "macOS 上 {combo} 交给终端");
        }
        for binding in key_bindings(mac) {
            assert!(
                !binding.keystrokes()[0].modifiers().control,
                "macOS 上终端层不占 Ctrl 组合：{}",
                binding.action().name()
            );
        }
        for os in [OperatingSystem::Windows, OperatingSystem::Linux] {
            assert_eq!(bound(os, "ctrl-shift-c"), Some(Copy.name()));
            assert_eq!(bound(os, "ctrl-insert"), Some(Copy.name()));
            assert_eq!(bound(os, "ctrl-shift-v"), Some(Paste.name()));
            assert_eq!(bound(os, "shift-insert"), Some(Paste.name()));
            for combo in ["ctrl-c", "ctrl-v", "cmd-c", "cmd-v"] {
                assert_eq!(bound(os, combo), None, "{os:?} 上 {combo}");
            }
            for binding in key_bindings(os) {
                assert!(!binding.keystrokes()[0].modifiers().platform, "{os:?}");
            }
        }
    }
}
