//! 界面语言。文案在 `locales/*.yml`，每个键下简体中文（zh-CN）、英文（en）、繁體中文（zh-TW）、日文（ja）、
//! 韩文（ko）并排；简体中文是源语言，改中文时同一个键的其余语言一起改，繁体的用语见 `docs/rust/繁体中文用语.md`，
//! 日文、韩文的见 `docs/rust/日文韩文用语.md`。
//!
//! 键说用途（`host.new.title`），一句话一个键，不拼片段；插值写 `%{name}`。
//! 文案在 render 时翻译，实体里存键或原始数据，不存翻译好的字符串，切换语言后立即生效。输入框的占位文字例外
//! （存在 gpui-kit 的输入框状态中），长期存在的输入框用 [`LocalizedPlaceholder`] 设，切换语言后跟着换。

use gpui_kit::base::input::{InputBaseState, InputModeKind};
use gpui_kit::{Context, SharedString, Window};

/// 翻译一条文案，返回 [`gpui_kit::SharedString`]。
///
/// ```ignore
/// t!("start.empty_title");
/// t!("host.delete.title", name = "web-01");
/// ```
#[macro_export]
macro_rules! t {
    ($key:expr) => {
        ::gpui_kit::SharedString::from(
            ::rust_i18n::t!($key, locale = $crate::i18n::language().code()).into_owned(),
        )
    };
    ($key:expr, $($name:ident = $value:expr),+ $(,)?) => {
        ::gpui_kit::SharedString::from(
            ::rust_i18n::t!($key, locale = $crate::i18n::language().code(), $($name = $value),+)
                .into_owned(),
        )
    };
}

/// 界面支持的语言。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    ZhCn,
    /// 繁體中文（台湾用语）
    ZhTw,
    En,
    Ja,
    Ko,
}

impl Language {
    /// 全部语言（测试逐个检查用）。
    pub const ALL: [Language; 5] = [
        Language::ZhCn,
        Language::ZhTw,
        Language::En,
        Language::Ja,
        Language::Ko,
    ];

    /// rust-i18n 的语言代码；gpui-kit 自己的文案用同一个代码（它的 zh-TW 缺的几条退回英文，都在 Termlet 没用的
    /// 差异视图、时间选择中；它没有的日文、韩文由 Termlet 补上，见 [`extend_gpui_kit`]）。
    pub fn code(self) -> &'static str {
        match self {
            Language::ZhCn => "zh-CN",
            Language::ZhTw => "zh-TW",
            Language::En => "en",
            Language::Ja => "ja",
            Language::Ko => "ko",
        }
    }

    /// 跟随系统（见 [`Language::from_locale`]）；读不到系统语言时用简体中文。
    pub fn system() -> Self {
        sys_locale::get_locale().map_or(Language::ZhCn, |locale| Self::from_locale(&locale))
    }

    /// 系统语言标识（`zh-TW`、`zh-Hant-TW`、`zh_TW.UTF-8`、`ja-JP` 这类）对应的界面语言：写明繁体（Hant），或地区是
    /// 台湾、香港、澳门的中文用繁體中文，其余中文用简体中文；日文、韩文用日文、韩文，其他语言用英文。
    pub fn from_locale(locale: &str) -> Self {
        let locale = locale.trim().to_ascii_lowercase().replace('_', "-");
        // POSIX 写法的编码与修饰（`.UTF-8`、`@…`）不算
        let locale = locale.split(['.', '@']).next().unwrap_or_default();
        let mut subtags = locale.split('-');
        match subtags.next() {
            Some("zh") => {}
            Some("ja") => return Language::Ja,
            Some("ko") => return Language::Ko,
            _ => return Language::En,
        }
        let mut traditional = false;
        for subtag in subtags {
            match subtag {
                // 写明的文字优先于地区（`zh-Hans-HK` 是简体）
                "hant" => return Language::ZhTw,
                "hans" => return Language::ZhCn,
                "tw" | "hk" | "mo" => traditional = true,
                _ => {}
            }
        }
        if traditional {
            Language::ZhTw
        } else {
            Language::ZhCn
        }
    }
}

/// 危险命令规则在当前界面语言下的说明（内置规则的说明在核心中，自定义规则为正则本身）。
pub fn rule_description(rule: &termlet_core::guard::Rule) -> &str {
    match language() {
        Language::ZhCn => rule.description(),
        Language::ZhTw => rule.description_zh_tw(),
        Language::En => rule.description_en(),
        Language::Ja => rule.description_ja(),
        Language::Ko => rule.description_ko(),
    }
}

/// gpui-kit 自己的文案（输入框的右键菜单、对话框的按钮、下拉框的占位文字等）没有日文、韩文：由 Termlet 的
/// `locales/gpui_kit.yml` 补上（键为 `gpui_component.` 加 gpui-kit 的键）。gpui-kit 查文案时先查这里，没有的
/// 再查它自己的（缺的退回英文）。启动时调用；重复调用不会再装一次（UI 测试每个都初始化一遍）。
pub fn extend_gpui_kit() {
    use gpui_kit::component as gpui_component;
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| rust_i18n::extend!(gpui_component));
}

/// 输入框的占位文字随界面语言变化。`InputState::placeholder(t!(…))` 设的是创建时的语言，切换语言后不变，
/// 侧边栏的搜索框这类一直开着的输入框要重启才换；用这个设，之后每次切换语言（[`crate::appearance::apply`]）
/// 按新语言重设。只在对话框中、每次打开都重建的输入框不必用。
///
/// ```ignore
/// cx.new(|cx| InputState::new(window, cx).localized_placeholder(|| t!("hosts.search.placeholder"), window, cx))
/// ```
pub(crate) trait LocalizedPlaceholder: Sized {
    fn localized_placeholder(
        self,
        text: impl Fn() -> SharedString + 'static,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Self;
}

impl<M: InputModeKind + 'static> LocalizedPlaceholder for InputBaseState<M> {
    fn localized_placeholder(
        self,
        text: impl Fn() -> SharedString + 'static,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let placeholder = text();
        // 随输入框一起释放
        cx.observe_global_in::<crate::appearance::AppliedLanguage>(
            window,
            move |state, window, cx| state.set_placeholder(text(), window, cx),
        )
        .detach();
        self.placeholder(placeholder)
    }
}

/// 切换界面语言（全局生效，gpui-kit 的文案一起切换）。调用方随后刷新窗口。
pub fn set_language(language: Language) {
    rust_i18n::set_locale(language.code());
    #[cfg(test)]
    TEST_LANGUAGE.with(|l| l.set(language));
}

/// 界面语言对应的核心的说明的语言（MCP 工具的说明、批量执行的出错说明）：简体、繁体中文用中文，其余用英文（核心中的
/// 说明只有这两种）。
pub fn core_lang(language: Language) -> termlet_core::proto::Lang {
    match language {
        Language::ZhCn | Language::ZhTw => termlet_core::proto::Lang::Zh,
        Language::En | Language::Ja | Language::Ko => termlet_core::proto::Lang::En,
    }
}

/// 当前的界面语言。
pub fn language() -> Language {
    #[cfg(test)]
    return TEST_LANGUAGE.with(|l| l.get());
    #[cfg(not(test))]
    match &*rust_i18n::locale() {
        "en" => Language::En,
        "zh-TW" => Language::ZhTw,
        "ja" => Language::Ja,
        "ko" => Language::Ko,
        _ => Language::ZhCn,
    }
}

// 单元测试并行运行，每个测试在自己的线程上：Termlet 的文案按本线程设定的语言（默认中文），
// 一个测试切换语言不影响别的测试（gpui-kit 自己的文案仍是进程全局的）。
#[cfg(test)]
thread_local! {
    static TEST_LANGUAGE: std::cell::Cell<Language> = const { std::cell::Cell::new(Language::ZhCn) };
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn switching_language_changes_text() {
        let original = language();
        set_language(Language::En);
        assert_eq!(t!("start.empty_title"), "No open terminals");
        assert_eq!(t!("close.sentence_separator"), " ");
        set_language(Language::ZhTw);
        assert_eq!(t!("start.empty_title"), "沒有開啟的終端機");
        assert_eq!(t!("close.sentence_separator"), "");
        set_language(Language::Ja);
        assert_eq!(t!("start.empty_title"), "開いているターミナルはありません");
        assert_eq!(t!("close.sentence_separator"), "");
        set_language(Language::Ko);
        assert_eq!(t!("start.empty_title"), "열려 있는 터미널이 없습니다");
        // 韩文的句子之间与英文一样加空格
        assert_eq!(t!("close.sentence_separator"), " ");
        set_language(Language::ZhCn);
        assert_eq!(t!("start.empty_title"), "没有打开的终端");
        // 中文的句子之间不加空格：空的文案照样取得到
        assert_eq!(t!("close.sentence_separator"), "");
        set_language(original);
    }

    /// gpui-kit 自己的文案：日文、韩文用 Termlet 补上的，其余语言用 gpui-kit 自己的。
    #[test]
    fn gpui_kit_texts_follow_the_language() {
        extend_gpui_kit();
        extend_gpui_kit();
        let kit = |locale: &str, key: &str| {
            gpui_kit::component::_rust_i18n_translate(locale, key).into_owned()
        };
        assert_eq!(kit("ja", "Input.Paste"), "貼り付け");
        assert_eq!(kit("ko", "Input.Paste"), "붙여넣기");
        assert_eq!(kit("ja", "Dialog.cancel"), "キャンセル");
        assert_eq!(kit("zh-CN", "Input.Paste"), "粘贴");
        assert_eq!(kit("zh-TW", "Input.Paste"), "貼上");
        assert_eq!(kit("en", "Input.Paste"), "Paste");
        // 没有补的（差异视图）退回英文
        assert_eq!(kit("ja", "Diff.NoChanges"), kit("en", "Diff.NoChanges"));
    }

    #[test]
    fn system_locale_maps_to_language() {
        for locale in [
            "zh-TW",
            "zh-HK",
            "zh-MO",
            "zh-Hant",
            "zh-Hant-TW",
            "zh-Hant-CN",
            "zh_TW",
            "zh_HK.UTF-8",
            "ZH-tw",
            "zh-hant-hk",
        ] {
            assert_eq!(Language::from_locale(locale), Language::ZhTw, "{locale}");
        }
        for locale in [
            "zh",
            "zh-CN",
            "zh-SG",
            "zh-Hans",
            "zh-Hans-CN",
            "zh-Hans-HK",
            "zh_CN.UTF-8",
            "ZH-cn",
        ] {
            assert_eq!(Language::from_locale(locale), Language::ZhCn, "{locale}");
        }
        for locale in ["ja", "ja-JP", "ja_JP.UTF-8", "JA-jp", "ja-Jpan-JP"] {
            assert_eq!(Language::from_locale(locale), Language::Ja, "{locale}");
        }
        for locale in ["ko", "ko-KR", "ko_KR.UTF-8", "ko-Kore-KR", "KO-kr"] {
            assert_eq!(Language::from_locale(locale), Language::Ko, "{locale}");
        }
        for locale in [
            "en-US",
            "en",
            "fr_FR.UTF-8",
            "C",
            "",
            "zha-CN",
            "jav-ID",
            "kok-IN",
        ] {
            assert_eq!(Language::from_locale(locale), Language::En, "{locale}");
        }
    }

    /// 某种语言的全部文案（键 → 文案）。
    fn messages(locale: &str) -> BTreeMap<String, String> {
        crate::_rust_i18n_backend()
            .messages_for_locale(locale)
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    /// 文案中的插值（排好序，同一个插值出现几次就有几个）。
    fn placeholders(text: &str) -> Vec<&str> {
        let re = regex::Regex::new(r"%\{[A-Za-z0-9_]+\}").unwrap();
        let mut found: Vec<_> = re.find_iter(text).map(|m| m.as_str()).collect();
        found.sort_unstable();
        found
    }

    /// 给 gpui-kit 补的文案（只有日文、韩文，见 [`extend_gpui_kit`]）的键的前缀。
    const GPUI_KIT_PREFIX: &str = "gpui_component.";

    /// 每个键都有简体中文、英文、繁體中文、日文、韩文，各语言的插值一致；繁体、日文的引号用「」，不用 “”；
    /// 韩文不夹汉字与全角标点。
    #[test]
    fn locales_are_complete() {
        let own = |messages: BTreeMap<String, String>| -> BTreeMap<String, String> {
            messages
                .into_iter()
                .filter(|(k, _)| !k.starts_with(GPUI_KIT_PREFIX))
                .collect()
        };
        let zh = own(messages("zh-CN"));
        assert!(zh.len() > 1000, "文案只有 {} 条", zh.len());
        let mut failures = Vec::new();
        for language in Language::ALL.into_iter().filter(|&l| l != Language::ZhCn) {
            let locale = language.code();
            let other = own(messages(locale));
            for key in zh.keys().filter(|k| !other.contains_key(*k)) {
                failures.push(format!("{key} 没有 {locale}"));
            }
            for key in other.keys().filter(|k| !zh.contains_key(*k)) {
                failures.push(format!("{key} 只有 {locale}，没有 zh-CN"));
            }
            for (key, text) in &other {
                let Some(source) = zh.get(key) else { continue };
                if placeholders(text) != placeholders(source) {
                    failures.push(format!("{key} 的 {locale} 插值与中文不同：{text}"));
                }
                if text.is_empty() && !source.is_empty() {
                    failures.push(format!("{key} 的 {locale} 是空的"));
                }
                let quotes = matches!(language, Language::ZhTw | Language::Ja);
                if quotes && text.contains(['“', '”']) {
                    failures.push(format!("{key} 的 {locale} 用了 “”（应为「」）：{text}"));
                }
                if language == Language::Ko
                    && text.chars().any(|c| {
                        matches!(c, '\u{4e00}'..='\u{9fff}' | '\u{3400}'..='\u{4dbf}')
                            || "，。、；：？！（）「」『』“”".contains(c)
                    })
                {
                    failures.push(format!("{key} 的韩文夹了汉字或全角标点：{text}"));
                }
            }
        }
        // 给 gpui-kit 补的：日文、韩文都有，键是 gpui-kit 有的，插值与它的英文一致
        let kit_en: BTreeMap<String, String> = gpui_kit::component::_rust_i18n_backend()
            .messages_for_locale("en")
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        for language in [Language::Ja, Language::Ko] {
            let locale = language.code();
            let ours = messages(locale);
            let kit: BTreeMap<_, _> = ours
                .iter()
                .filter_map(|(k, v)| Some((k.strip_prefix(GPUI_KIT_PREFIX)?, v)))
                .collect();
            assert!(!kit.is_empty(), "没有给 gpui-kit 补 {locale}");
            for (key, text) in kit {
                match kit_en.get(key) {
                    None => failures.push(format!("gpui-kit 没有 {key}（{locale}）")),
                    Some(en) if placeholders(text) != placeholders(en) => {
                        failures.push(format!("gpui-kit 的 {key} 的 {locale} 插值不同：{text}"));
                    }
                    Some(_) => {}
                }
            }
        }
        let kit_keys = |locale: &str| -> Vec<String> {
            messages(locale)
                .into_keys()
                .filter(|k| k.starts_with(GPUI_KIT_PREFIX))
                .collect()
        };
        assert_eq!(
            kit_keys("ja"),
            kit_keys("ko"),
            "给 gpui-kit 补的日文、韩文的键不同"
        );
        assert!(kit_keys("zh-CN").is_empty() && kit_keys("en").is_empty());
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}
