//! 危险命令规则与粘贴防护。
//!
//! - [`Guard`]：危险命令规则（内置规则、自定义正则），CLI 执行命令、粘贴、同步输入、撰写栏发送前共用。
//!   多行脚本逐行检查，多行引号字符串与 heredoc 的正文也按行检查（可能误拦，宁可请用户批准也不漏掉）。
//! - [`find_danger`]：在终端中执行的内容（粘贴、同步输入的回车、撰写栏发送的各行）逐行并整段检查，
//!   返回命中的规则与提示中显示的命令。
//! - [`inspect_paste`]：粘贴到终端前的检查，返回结构化的结果，界面据此决定直接粘贴还是先显示确认框。
//! - [`feed_line`] 等：同步输入中按输入推算待执行的命令行，按回车前用规则检查。
//!
//! 定位是防止误操作，不是安全边界，见 [`Guard`]。

mod input;
mod paste;
mod rules;

pub use input::{FeedResult, checked_paste, feed_line, feed_paste, run_commands};
pub use paste::{
    Danger, ONE_LINE_MAX, PasteCheck, PasteContext, PasteFinding, Sanitized, analyze_paste,
    find_danger, inspect_paste, one_line, paste_lines, sanitize_paste,
};
pub use rules::{
    Guard, InvalidRule, MAX_CUSTOM_RULES, MAX_PATTERN_LENGTH, Rule, RuleError, builtin_rules,
    is_builtin, validate_custom_rule,
};
