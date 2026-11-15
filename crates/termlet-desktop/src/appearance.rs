//! 把外观设置（界面语言、浅色 / 深色）落到界面上。

use gpui_kit::component::{ActiveTheme as _, Theme, ThemeMode};
use gpui_kit::{App, Global, OperatingSystem, SharedString, Window};
use termlet_core::model::{AppearanceSettings, LanguageSetting, ThemeSetting};

use crate::i18n::{self, Language};

/// 设置中的界面语言对应的实际语言。
pub fn language_of(setting: LanguageSetting) -> Language {
    match setting {
        LanguageSetting::System => Language::system(),
        LanguageSetting::ZhCn => Language::ZhCn,
        LanguageSetting::ZhTw => Language::ZhTw,
        LanguageSetting::En => Language::En,
        LanguageSetting::Ja => Language::Ja,
        LanguageSetting::Ko => Language::Ko,
    }
}

/// 切换界面语言，界面字体随之换成这种语言的（见 [`ui_font_candidates`]）。
pub fn set_language(language: Language, cx: &mut App) {
    i18n::set_language(language);
    apply_ui_font(language, cx);
}

/// gpui-kit 默认的界面字体（GPUI 换成系统的界面字体）。
const SYSTEM_UI_FONT: &str = ".SystemUIFont";

/// 日文、韩文界面优先用的界面字体（按顺序取第一个装了的）。GPUI 的系统界面字体在中文的 Windows 上是微软雅黑
/// 一类的中文字体：日文的汉字照样显示得出来，却是中文的字形（“直”“骨”“写”等明显不同），韩文则由系统后备拼凑。
/// 其余语言用系统的界面字体。
fn ui_font_candidates(language: Language, os: OperatingSystem) -> &'static [&'static str] {
    match (language, os) {
        (Language::Ja, OperatingSystem::Windows) => &["Yu Gothic UI", "Meiryo UI", "Meiryo"],
        (Language::Ja, OperatingSystem::Mac) => &["Hiragino Sans", "Hiragino Kaku Gothic ProN"],
        (Language::Ja, _) => &["Noto Sans CJK JP", "Noto Sans JP", "Source Han Sans JP"],
        (Language::Ko, OperatingSystem::Windows) => &["Malgun Gothic"],
        (Language::Ko, OperatingSystem::Mac) => &["Apple SD Gothic Neo"],
        (Language::Ko, _) => &["Noto Sans CJK KR", "Noto Sans KR", "Source Han Sans KR"],
        _ => &[],
    }
}

/// 由 [`apply_ui_font`] 设到主题上的界面字体（`None` 为没有改动，用系统的界面字体）。
struct AppliedUiFont(Option<SharedString>);

impl Global for AppliedUiFont {}

/// 按界面语言设定 gpui-kit 主题的界面字体：日文、韩文用这种语言的字体（没装时不变），切回其他语言时恢复系统的
/// 界面字体。只还原自己设过的，主题的字体被别处改过时不动它。
fn apply_ui_font(language: Language, cx: &mut App) {
    let candidates = ui_font_candidates(language, cx.platform_info().os);
    let wanted: Option<SharedString> = if candidates.is_empty() {
        None
    } else {
        let installed = cx.text_system().all_font_names();
        candidates
            .iter()
            .find(|c| installed.iter().any(|name| name == *c))
            .map(|c| SharedString::from(*c))
    };
    let applied = cx.try_global::<AppliedUiFont>().and_then(|a| a.0.clone());
    if wanted == applied {
        return;
    }
    let ours = applied.is_none() || applied.as_ref() == Some(&cx.theme().font_family);
    if ours {
        tracing::info!(font = ?wanted, "界面字体");
        let family = wanted.clone().unwrap_or_else(|| SYSTEM_UI_FONT.into());
        Theme::update(cx, |theme| theme.font_family = family);
    }
    cx.set_global(AppliedUiFont(wanted));
}

/// 已经落到界面上的语言设置。界面语言是进程全局的：保存别的设置时不再重设它（并行的 UI 测试之间也就互不影响）。
/// 切换语言时重设这个全局，观察它就能知道语言变了（[`crate::i18n::LocalizedPlaceholder`]）。
pub(crate) struct AppliedLanguage(LanguageSetting);

impl Global for AppliedLanguage {}

/// 应用外观设置。`window` 用于跟随系统时读取窗口所在的外观（Linux 上比全局的可靠）。
pub fn apply(settings: &AppearanceSettings, window: Option<&mut Window>, cx: &mut App) {
    if cx.try_global::<AppliedLanguage>().map(|a| a.0) != Some(settings.language) {
        set_language(language_of(settings.language), cx);
        cx.set_global(AppliedLanguage(settings.language));
    }
    match settings.theme {
        ThemeSetting::Light => Theme::change(ThemeMode::Light, window, cx),
        ThemeSetting::Dark => Theme::change(ThemeMode::Dark, window, cx),
        ThemeSetting::System => Theme::sync_system_appearance(window, cx),
    }
}

/// 浅色主题的弱化文字与焦点框调深（gpui-kit 默认的弱化文字 #737373 在浅灰底上不到 4.5:1，焦点框 #a3a3a3 在白底上
/// 不到 3:1）：#666666 在最深的浅灰底（#e5e5e5，表头等）上也有 4.55:1，#737373 的焦点框在白底上 4.74:1。改的是注册的
/// 浅色配置，之后每次切换（包括跟随系统）都从它生成。
pub fn adjust_light_theme(cx: &mut App) {
    let theme = Theme::global_mut(cx);
    let mut light = (*theme.light_theme).clone();
    light.colors.muted_foreground = Some(LIGHT_MUTED_FOREGROUND.into());
    light.colors.ring = Some(LIGHT_RING.into());
    theme.light_theme = std::rc::Rc::new(light);
    let mode = theme.mode;
    Theme::change(mode, None, cx);
}

/// 浅色主题的弱化文字与焦点框（主题配置中的颜色，见 [`adjust_light_theme`]）。
const LIGHT_MUTED_FOREGROUND: &str = "#666666";
const LIGHT_RING: &str = "#737373";

/// 记下这个语言设置已经生效，但不切换界面语言（UI 测试：界面语言由测试自己设定）。
pub fn assume_language(setting: LanguageSetting, cx: &mut App) {
    cx.set_global(AppliedLanguage(setting));
}

#[cfg(test)]
mod tests {
    use gpui_kit::{Hsla, Rgba, TestAppContext};

    use super::*;

    /// 两种颜色的对比度（WCAG 的相对亮度）。
    fn contrast(a: Hsla, b: Hsla) -> f32 {
        let luminance = |c: Hsla| {
            let c = Rgba::from(c);
            let channel = |v: f32| {
                if v <= 0.03928 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * channel(c.r) + 0.7152 * channel(c.g) + 0.0722 * channel(c.b)
        };
        let (x, y) = (luminance(a), luminance(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    // 弱化文字在白底、浅灰底、表头的灰底上都不低于 4.5:1，焦点框在白底上不低于 3:1；跟随系统切换后也一样
    #[gpui_kit::test]
    fn light_theme_meets_contrast(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        cx.update(crate::init);
        cx.update(|cx| {
            for _ in 0..2 {
                Theme::change(ThemeMode::Light, None, cx);
                let theme = cx.theme();
                for (name, bg) in [
                    ("background", theme.background),
                    ("muted", theme.muted),
                    ("secondary", theme.secondary),
                    ("sidebar", theme.sidebar),
                ] {
                    let ratio = contrast(theme.muted_foreground, bg);
                    assert!(ratio >= 4.5, "弱化文字在 {name} 上只有 {ratio:.2}:1");
                }
                let ring = contrast(theme.ring, theme.background);
                assert!(ring >= 3.0, "焦点框只有 {ring:.2}:1");
                Theme::change(ThemeMode::Dark, None, cx);
                let theme = cx.theme();
                assert!(contrast(theme.muted_foreground, theme.background) >= 4.5);
            }
        });
    }
}
