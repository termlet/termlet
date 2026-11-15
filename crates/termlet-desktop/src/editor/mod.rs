//! 在线编辑：在 Dock 的 Tab 中编辑远程文本文件（在 SFTP 面板中双击文件打开）。
//!
//! - [`EditorPanel`]：编辑器 Tab（工具栏、提示条、编辑区、状态栏），读写经 SFTP 会话；
//! - `language`：按路径识别语言（状态栏显示、选语法高亮）；
//! - `text`：换行符的检测与还原。

mod language;
mod panel;
mod text;

#[cfg(test)]
pub(crate) use panel::key_bindings;
pub use panel::{EditorPanel, EditorPanelEvent, init};
