//! 终端字体的选项：本机已安装的等宽字体，推荐的排在前面；没装的推荐字体给出下载地址。
//!
//! 读取字体列表并逐个测量要几百毫秒，第一次打开“终端”分页时在后台读取一次，之后用缓存。

use std::sync::{Arc, LazyLock};

use futures::FutureExt as _;
use futures::future::Shared;
use gpui_kit::{App, Global, SharedString, Task, font, px};
use regex::Regex;

use crate::t;

/// 推荐的终端字体（按推荐顺序）。
pub struct Recommended {
    pub family: &'static str,
    note: Note,
    pub url: &'static str,
}

#[derive(Clone, Copy)]
enum Note {
    Nerd,
    Cjk,
    All,
    Windows,
}

pub const RECOMMENDED: [Recommended; 4] = [
    // 带 Git、箭头等图标（oh-my-zsh、starship 等主题需要）；Mono 版的图标严格占一格
    Recommended {
        family: "JetBrainsMono Nerd Font Mono",
        note: Note::Nerd,
        url: "https://www.nerdfonts.com/font-downloads",
    },
    // 一个汉字正好两个英文字符宽，中英文混排对齐；Term 版按终端调整了符号宽度
    Recommended {
        family: "Sarasa Term SC",
        note: Note::Cjk,
        url: "https://github.com/be5invis/Sarasa-Gothic/releases",
    },
    Recommended {
        family: "Maple Mono NF CN",
        note: Note::All,
        url: "https://github.com/subframe7536/maple-font/releases",
    },
    Recommended {
        family: "Cascadia Mono",
        note: Note::Windows,
        url: "https://github.com/microsoft/cascadia-code/releases",
    },
];

impl Recommended {
    /// 推荐的理由（“带图标”“中英文对齐”等）。
    pub fn note(&self) -> SharedString {
        match self.note {
            Note::Nerd => t!("settings.font.note_nerd"),
            Note::Cjk => t!("settings.font.note_cjk"),
            Note::All => t!("settings.font.note_all"),
            Note::Windows => t!("settings.font.note_windows"),
        }
    }
}

/// 不列出的字体：图标、符号字体（每个字符宽度相同，但画的不是字母）；Nerd Font 的 Propo 版（图标宽度不固定，
/// 在终端里会错位，应选 Mono 版）。
static EXCLUDED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(marlett|webdings|wingdings( \d)?|symbol|mt extra|.*mdl2 assets|segoe fluent icons)$| nerd font propo$")
        .unwrap()
});

type FontList = Shared<Task<Arc<Vec<String>>>>;

struct MonospaceFonts(FontList);

impl Global for MonospaceFonts {}

/// 本机已安装的等宽字体（按名称排序，不含内置的 JetBrains Mono），第一次调用时在后台读取。
pub fn monospace_fonts(cx: &mut App) -> FontList {
    if let Some(fonts) = cx.try_global::<MonospaceFonts>() {
        return fonts.0.clone();
    }
    let text_system = cx.text_system().clone();
    let task = cx
        .background_executor()
        .spawn(async move {
            let size = px(20.);
            let mut fonts: Vec<String> = text_system
                .all_font_names()
                .into_iter()
                // Windows 上 @ 开头的是竖排用的中文字体
                .filter(|name| {
                    !name.starts_with('@')
                        && name != termlet_terminal::fonts::DEFAULT_FAMILY
                        && !EXCLUDED.is_match(name)
                })
                .filter(|name| {
                    // 等宽：窄字符与宽字符一样宽
                    let id = text_system.resolve_font(&font(name.clone()));
                    let width = |c| text_system.advance(id, size, c).map(|s| s.width);
                    match (width('i'), width('W')) {
                        (Ok(narrow), Ok(wide)) => {
                            narrow > px(0.) && (f32::from(narrow) - f32::from(wide)).abs() < 0.5
                        }
                        _ => false,
                    }
                })
                .collect();
            fonts.sort_by_key(|name| name.to_lowercase());
            Arc::new(fonts)
        })
        .shared();
    cx.set_global(MonospaceFonts(task.clone()));
    task
}
