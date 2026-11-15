//! 语言选择：明确是英文才用英文，否则一律中文。
//! JSON 输出的字段名与错误码固定为英文，不受语言影响；这里只管人类可读的文本。

pub use termlet_proto::Lang;

/// 一条中英文对照的文本。
#[derive(Clone, Copy, Debug)]
pub struct Msg {
    pub zh: &'static str,
    pub en: &'static str,
}

impl Msg {
    pub fn get(self, lang: Lang) -> &'static str {
        lang.pick(self.zh, self.en)
    }

    /// 依次把文本中的 `{}` 换成参数。
    pub fn fill(self, lang: Lang, args: &[&dyn std::fmt::Display]) -> String {
        let mut out = String::new();
        let mut rest = self.get(lang);
        let mut args = args.iter();
        while let Some(i) = rest.find("{}") {
            out.push_str(&rest[..i]);
            if let Some(arg) = args.next() {
                out.push_str(&arg.to_string());
            }
            rest = &rest[i + 2..];
        }
        out.push_str(rest);
        out
    }
}

/// 按以下顺序决定语言：
/// 1. `TERMLET_LANG=en|zh`（便于测试与用户强制指定）；
/// 2. `LC_ALL`、`LC_MESSAGES`、`LANG` 中第一个能判断的值（以 en 开头为英文、以 zh 开头为中文；C、POSIX 等跳过）；
/// 3. 系统界面语言；
/// 4. 以上都无法判断时用中文。
pub fn detect(
    getenv: impl Fn(&str) -> Option<String>,
    system: impl Fn() -> Option<String>,
) -> Lang {
    if let Some(lang) = getenv("TERMLET_LANG").and_then(|v| parse(&v)) {
        return lang;
    }
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Some(lang) = getenv(key).and_then(|v| parse(&v)) {
            return lang;
        }
    }
    system().and_then(|v| parse(&v)).unwrap_or(Lang::Zh)
}

/// 本进程的语言。
pub fn current() -> Lang {
    detect(|k| std::env::var(k).ok(), sys_locale::get_locale)
}

/// 解析 `zh_CN.UTF-8`、`en-US`、`en` 这类语言标识；无法判断时为 `None`。
fn parse(value: &str) -> Option<Lang> {
    let value = value.trim().to_lowercase();
    if value.starts_with("en") {
        Some(Lang::En)
    } else if value.starts_with("zh") {
        Some(Lang::Zh)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_language() {
        let cases: &[(&str, &[(&str, &str)], &str, Lang)] = &[
            ("都未设置且无系统语言时回退中文", &[], "", Lang::Zh),
            ("系统界面语言为英文", &[], "en-US", Lang::En),
            ("系统界面语言为中文", &[], "zh-CN", Lang::Zh),
            ("非中英文系统回退中文", &[], "ja-JP", Lang::Zh),
            ("LANG 英文", &[("LANG", "en_US.UTF-8")], "zh-CN", Lang::En),
            (
                "LANG 中文优先于系统英文",
                &[("LANG", "zh_CN.UTF-8")],
                "en-US",
                Lang::Zh,
            ),
            (
                "LC_ALL 优先于 LANG",
                &[("LC_ALL", "en_GB.UTF-8"), ("LANG", "zh_CN.UTF-8")],
                "",
                Lang::En,
            ),
            (
                "LC_MESSAGES 优先于 LANG",
                &[("LC_MESSAGES", "zh_TW.UTF-8"), ("LANG", "en_US.UTF-8")],
                "",
                Lang::Zh,
            ),
            (
                "C 无法判断时看下一个变量",
                &[("LC_ALL", "C"), ("LANG", "en_US.UTF-8")],
                "",
                Lang::En,
            ),
            (
                "POSIX 无法判断时看系统语言",
                &[("LANG", "POSIX")],
                "en-US",
                Lang::En,
            ),
            (
                "非中英文 LANG 继续看系统语言",
                &[("LANG", "fr_FR.UTF-8")],
                "",
                Lang::Zh,
            ),
            (
                "TERMLET_LANG 覆盖一切（en）",
                &[("TERMLET_LANG", "en"), ("LC_ALL", "zh_CN.UTF-8")],
                "zh-CN",
                Lang::En,
            ),
            (
                "TERMLET_LANG 覆盖一切（zh）",
                &[("TERMLET_LANG", "zh"), ("LANG", "en_US.UTF-8")],
                "en-US",
                Lang::Zh,
            ),
            ("TERMLET_LANG 大写", &[("TERMLET_LANG", "EN")], "", Lang::En),
            (
                "TERMLET_LANG 无效时忽略",
                &[("TERMLET_LANG", "de"), ("LANG", "en_US.UTF-8")],
                "",
                Lang::En,
            ),
        ];
        for (name, env, system, want) in cases {
            let getenv = |k: &str| {
                env.iter()
                    .find(|(key, _)| *key == k)
                    .map(|(_, v)| v.to_string())
            };
            let system = || Some(system.to_string()).filter(|s| !s.is_empty());
            assert_eq!(detect(getenv, system), *want, "{name}");
        }
    }

    #[test]
    fn picks_text_by_language() {
        let m = Msg {
            zh: "你好",
            en: "hello",
        };
        assert_eq!(m.get(Lang::Zh), "你好");
        assert_eq!(m.get(Lang::En), "hello");
    }

    // 当前系统能取到界面语言时，应能被识别或安全地忽略
    #[test]
    fn system_language_is_harmless() {
        let _ = sys_locale::get_locale().and_then(|v| parse(&v));
    }
}
