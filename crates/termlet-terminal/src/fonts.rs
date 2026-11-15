//! 终端字体：内置 JetBrains Mono（SIL OFL 1.1，许可文本随安装包分发），中日韩文字与符号由系统字体后备。

use std::borrow::Cow;

use gpui_kit::{App, FontFallbacks};

/// 内置字体的家族名。
pub const DEFAULT_FAMILY: &str = "JetBrains Mono";

/// JetBrains Mono 的许可文本（随安装包放在 licenses 目录下）。
pub const LICENSE: &str = include_str!("../assets/fonts/JetBrainsMono-OFL.txt");

/// 把内置字体注册到 GPUI 的文字系统（启动时调用一次）。
pub fn register(cx: &App) {
    let fonts: Vec<Cow<'static, [u8]>> = vec![
        Cow::Borrowed(include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf")),
        Cow::Borrowed(include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf")),
        Cow::Borrowed(include_bytes!("../assets/fonts/JetBrainsMono-Italic.ttf")),
        Cow::Borrowed(include_bytes!(
            "../assets/fonts/JetBrainsMono-BoldItalic.ttf"
        )),
    ];
    if let Err(e) = cx.text_system().add_fonts(fonts) {
        tracing::warn!("注册内置终端字体失败：{e}");
    }
}

/// 中日韩共用的汉字优先用哪种语言的字形（按界面语言）：日文界面的汉字用日文字体，不显示成中文的字形。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CjkPreference {
    #[default]
    Chinese,
    Japanese,
    Korean,
}

/// 后备字体：主字体没有的字符（中文、日文、韩文、符号、表情）按顺序在这些字体中查找；汉字优先用中文字体。
pub fn default_fallbacks() -> FontFallbacks {
    fallbacks(CjkPreference::Chinese)
}

/// 后备字体：先是等宽字体，再是中日韩字体（`prefer` 那种语言的排在前面），最后是符号与表情。
pub fn fallbacks(prefer: CjkPreference) -> FontFallbacks {
    FontFallbacks::from_fonts(
        fallback_names(prefer)
            .into_iter()
            .map(str::to_string)
            .collect(),
    )
}

fn fallback_names(prefer: CjkPreference) -> Vec<&'static str> {
    let (mono, chinese, japanese, korean, symbols): (&[&str], &[&str], &[&str], &[&str], &[&str]) =
        if cfg!(windows) {
            (
                &["Cascadia Mono", "Consolas"],
                &["Microsoft YaHei UI", "Microsoft YaHei", "SimSun"],
                &["Yu Gothic UI", "Meiryo"],
                &["Malgun Gothic"],
                &["Segoe UI Symbol", "Segoe UI Emoji"],
            )
        } else if cfg!(target_os = "macos") {
            (
                &["Menlo"],
                &["PingFang SC"],
                &["Hiragino Sans"],
                &["Apple SD Gothic Neo"],
                &["Apple Color Emoji"],
            )
        } else {
            (
                &["DejaVu Sans Mono"],
                &["Noto Sans Mono CJK SC", "Noto Sans CJK SC"],
                &["Noto Sans Mono CJK JP", "Noto Sans CJK JP"],
                &["Noto Sans Mono CJK KR", "Noto Sans CJK KR"],
                &["Noto Color Emoji"],
            )
        };
    let cjk: [&[&str]; 3] = match prefer {
        CjkPreference::Chinese => [chinese, japanese, korean],
        CjkPreference::Japanese => [japanese, chinese, korean],
        CjkPreference::Korean => [korean, chinese, japanese],
    };
    let mut names = mono.to_vec();
    for group in cjk {
        names.extend_from_slice(group);
    }
    names.extend_from_slice(symbols);
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    // 优先的那种语言的字体排在其他中日韩字体之前，等宽字体始终在最前、符号与表情在最后，一个也不少
    #[test]
    fn preferred_cjk_fonts_come_first() {
        let chinese = fallback_names(CjkPreference::Chinese);
        for prefer in [CjkPreference::Japanese, CjkPreference::Korean] {
            let names = fallback_names(prefer);
            let mut sorted = names.clone();
            sorted.sort_unstable();
            let mut expected = chinese.clone();
            expected.sort_unstable();
            assert_eq!(sorted, expected);
            assert_eq!(names.first(), chinese.first());
            assert_eq!(names.last(), chinese.last());
        }
        let position = |names: &[&str], font: &str| names.iter().position(|n| *n == font).unwrap();
        let (zh, ja, ko) = if cfg!(windows) {
            ("Microsoft YaHei UI", "Yu Gothic UI", "Malgun Gothic")
        } else if cfg!(target_os = "macos") {
            ("PingFang SC", "Hiragino Sans", "Apple SD Gothic Neo")
        } else {
            ("Noto Sans CJK SC", "Noto Sans CJK JP", "Noto Sans CJK KR")
        };
        assert!(position(&chinese, zh) < position(&chinese, ja));
        let japanese = fallback_names(CjkPreference::Japanese);
        assert!(position(&japanese, ja) < position(&japanese, zh));
        let korean = fallback_names(CjkPreference::Korean);
        assert!(position(&korean, ko) < position(&korean, zh));
    }
}
