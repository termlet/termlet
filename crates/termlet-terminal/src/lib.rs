//! 终端：把传输送来的字节交给终端内核解析，并用 GPUI 画出来。
//!
//! 这里只认"字节进、字节出"的传输接口（[`io`]），不知道 SSH、PTY 或串口，不依赖 `termlet-core`。
//! 内核（现在是 alacritty_terminal）只在引擎内部使用（见 [`engine`]）；视图、查找、关键词高亮、shell 集成、
//! 按键与鼠标编码只认 [`grid`] 与 [`colors`] 中引擎自己的类型。

pub mod clipboard;
pub mod colors;
pub mod dump;
mod element;
pub mod engine;
pub mod fonts;
pub mod grid;
pub mod highlight;
pub mod keys;
pub mod kitty_keyboard;
pub mod mouse;
pub mod search;
pub mod shell_events;
pub mod suggest;
pub mod text;
pub mod theme_mode;
pub mod view;

pub use clipboard::{ClipboardRequest, ClipboardText, MAX_CLIPBOARD_WRITE};
pub use colors::{Palette, Rgb};
pub use dump::ScreenDump;
pub use engine::{EngineEvent, EngineOptions, OutputObserver, ReplayFeed, TerminalEngine};
pub use grid::{
    CursorShape, GridDimensions, GridPoint, MouseEncoding, MouseTracking, TerminalModes,
};
pub use highlight::{
    HighlightColor, HighlightMatch, HighlightRuleSpec, Highlighter, RuleError, rule_error,
};
pub use kitty_keyboard::KeyboardFlags;
pub use search::SearchQuery;
pub use shell_events::{CommandText, ShellEvent};
pub use suggest::CommandSuggestions;
pub use termlet_io as io;
pub use termlet_io::{CloseReason, GridSize, IoEvent, TerminalInput, TerminalIo};
pub use view::{
    ContrastAdjust, Link, RightClick, SearchSummary, TerminalStyle, TerminalView, TerminalViewEvent,
};

/// 注册终端的字体与快捷键（应用启动时、`gpui_kit::init` 之后调用一次）。
pub fn init(cx: &mut gpui_kit::App) {
    fonts::register(cx);
    view::init(cx);
}
