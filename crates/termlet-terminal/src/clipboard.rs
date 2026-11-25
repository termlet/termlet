//! 程序经 OSC 52 写剪贴板（`ESC ] 52 ; c ; <base64> BEL`）：只写不读。
//!
//! - 读剪贴板（`52;c;?`）一律不回答（内核配置为只接受写入），服务器读不走本机的剪贴板。
//! - 解码后超过 [`MAX_CLIPBOARD_WRITE`] 的整条不写（不截断），界面提示太大。
//! - 选择器 `c`（或空）写剪贴板；`p`、`s` 在 Linux 上写 PRIMARY 选区，其他系统没有 PRIMARY，也写剪贴板。
//! - 空内容（`52;c;`，xterm 的“清空”）不处理。
//! - 是否真的写入由视图决定（[`crate::TerminalView`]）：设置允许、窗口在前台且焦点在这个终端上、不在回放中。

use std::fmt;

/// 程序一次最多写进剪贴板的字节数（解码后的 UTF-8）：1 MiB，与 iTerm2、tmux 的上限相同。
pub const MAX_CLIPBOARD_WRITE: usize = 1 << 20;

/// 程序要写进剪贴板的文字：可能是密码等秘密，`Debug` 只打印长度。
#[derive(Clone, PartialEq, Eq)]
pub struct ClipboardText(pub String);

impl ClipboardText {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ClipboardText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ClipboardText({} 个字符)", self.0.chars().count())
    }
}

/// 程序写剪贴板的请求（引擎经 [`crate::EngineEvent::Clipboard`] 发出）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipboardRequest {
    /// 写入；`primary` 是选择器为 `p`、`s` 的
    Write { text: ClipboardText, primary: bool },
    /// 解码后超过 [`MAX_CLIPBOARD_WRITE`]，没有写入；`bytes` 是解码后的字节数
    TooLarge { bytes: usize },
}

impl ClipboardRequest {
    /// 内核收到的一次写入（解码后的文字）；空内容不处理，返回 `None`。
    pub(crate) fn from_store(text: String, primary: bool) -> Option<Self> {
        if text.is_empty() {
            return None;
        }
        Some(if text.len() > MAX_CLIPBOARD_WRITE {
            ClipboardRequest::TooLarge { bytes: text.len() }
        } else {
            ClipboardRequest::Write {
                text: ClipboardText(text),
                primary,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_follow_the_size_limit() {
        assert_eq!(ClipboardRequest::from_store(String::new(), false), None);
        assert_eq!(
            ClipboardRequest::from_store("hi".into(), true),
            Some(ClipboardRequest::Write {
                text: ClipboardText("hi".into()),
                primary: true
            })
        );
        let exact = "a".repeat(MAX_CLIPBOARD_WRITE);
        assert!(matches!(
            ClipboardRequest::from_store(exact, false),
            Some(ClipboardRequest::Write { .. })
        ));
        // 按字节算：多字节的字符
        let over = "中".repeat(MAX_CLIPBOARD_WRITE / 3 + 1);
        assert_eq!(
            ClipboardRequest::from_store(over.clone(), false),
            Some(ClipboardRequest::TooLarge { bytes: over.len() })
        );
    }

    #[test]
    fn text_is_not_in_debug_output() {
        let text = ClipboardText("password123".into());
        assert_eq!(format!("{text:?}"), "ClipboardText(11 个字符)");
    }
}
