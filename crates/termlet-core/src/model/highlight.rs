//! 关键词高亮的预设规则：设置页“添加预设规则”中列出的 10 条，新装时的默认规则是其中标了 `default` 的几条。
//!
//! - 预设按正则原文认出（[`HighlightPreset::matches`]），规则中不另存预设的 ID：旧版本、同步、备份都不受影响；
//!   改了颜色、停用了仍算这条预设，改了正则就不算了。
//! - 表中的顺序就是优先级（规则重叠时靠前的优先）：网址在最前，网址中的 IP、error 不把它打断；IPv6 在 IPv4 前，
//!   `::ffff:1.2.3.4` 整段归 IPv6；UUID、日期时间在数字前，数字在最后。添加预设时按这个顺序插入（[`preset_insert_position`]）。
//! - 正则只用 Rust `regex` 与 Python `re` 语义相同的写法（没有环视，数字写 `[0-9]`），`\b`、`\B` 由终端换成 ASCII 的。
//!   错误、警告、成功、IPv4 四条是 0.1.0–0.3.2 的默认规则，原文一个字符都不能改，否则老用户的规则认不出。

use super::HighlightRule;

/// 一条预设规则（都是正则、不区分大小写）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HighlightPreset {
    /// 文案的键 `settings.highlight.preset.<id>` 用它，不存进设置
    pub id: &'static str,
    pub pattern: &'static str,
    /// 终端配色中的颜色名（red、yellow……）
    pub color: &'static str,
    /// 新装时的默认规则
    pub default: bool,
}

impl HighlightPreset {
    /// 加进规则列表的规则。
    pub fn rule(&self) -> HighlightRule {
        HighlightRule {
            pattern: self.pattern.into(),
            regex: true,
            case_sensitive: false,
            color: self.color.into(),
            enabled: true,
            ..Default::default()
        }
    }

    /// 这条规则是不是这条预设（正则原文相同）。
    pub fn matches(&self, rule: &HighlightRule) -> bool {
        rule.regex && rule.pattern == self.pattern
    }
}

/// 全部预设，按优先级（也是菜单中的顺序）。
pub static HIGHLIGHT_PRESETS: [HighlightPreset; 10] = [
    HighlightPreset {
        id: "url",
        pattern: r"\b[a-z][a-z0-9+.-]*://[0-9a-z._~:/?#\[\]@!$&()*+,;=%-]*[0-9a-z_~/#@$&*+=%-]",
        color: "blue",
        default: false,
    },
    HighlightPreset {
        id: "errors",
        pattern: r"\b(error|errors|fail|failed|failure|fatal|panic|denied|refused|exception|critical)\b",
        color: "red",
        default: true,
    },
    HighlightPreset {
        id: "warnings",
        pattern: r"\b(warn|warning|warnings|deprecated)\b",
        color: "yellow",
        default: true,
    },
    HighlightPreset {
        id: "success",
        pattern: r"\b(ok|success|successful|succeeded|passed)\b",
        color: "green",
        default: true,
    },
    HighlightPreset {
        id: "ipv6",
        pattern: r"\[::\](?::[0-9]{1,5}\b)?|\B:::[0-9]{1,5}\b|\B::/[0-9]{1,3}\b|\[?(?:\b(?:[0-9a-f]{3,4}|[0-9a-f])(?::[0-9a-f]{1,4}){7}\b|\b[0-9a-f]{1,4}(?::[0-9a-f]{1,4}){5}:(?:[0-9]{1,3}\.){3}[0-9]{1,3}\b|\b[0-9a-f]{1,4}(?::[0-9a-f]{1,4}){1,6}::(?:(?:[0-9a-f]{1,4}:){0,5}(?:[0-9]{1,3}\.){3}[0-9]{1,3}|[0-9a-f]{1,4}(?::[0-9a-f]{1,4}){0,6})\b|\b[0-9a-f]{1,4}::(?:(?:[0-9a-f]{1,4}:){0,5}(?:[0-9]{1,3}\.){3}[0-9]{1,3}|[0-9a-f]{1,4}(?::[0-9a-f]{1,4}){1,6})\b|\b(?:[0-9a-f]{0,3}[0-9][0-9a-f]{0,3}::[0-9a-f]{1,4}|[0-9a-f]{1,4}::[0-9a-f]{0,3}[0-9][0-9a-f]{0,3})\b|\b[0-9a-f]{1,4}(?::[0-9a-f]{1,4}){1,6}::\B|\b[0-9a-f]{0,3}[0-9][0-9a-f]{0,3}::\B|\B::(?:(?:[0-9a-f]{1,4}:){0,5}(?:[0-9]{1,3}\.){3}[0-9]{1,3}|[0-9a-f]{1,4}(?::[0-9a-f]{1,4}){1,6})\b|\B::[0-9a-f]{0,3}[0-9][0-9a-f]{0,3}\b)(?:%[0-9a-z]+(?:[._-][0-9a-z]+)*)?(?:\](?::[0-9]{1,5}\b)?|/[0-9]{1,3}\b)?",
        color: "cyan",
        default: true,
    },
    HighlightPreset {
        id: "ipv4",
        pattern: r"\b(?:\d{1,3}\.){3}\d{1,3}(?:/\d{1,2})?\b",
        color: "cyan",
        default: true,
    },
    HighlightPreset {
        id: "mac",
        pattern: r"\b(?:[0-9a-f]{2}(?::[0-9a-f]{2}){5,}|[0-9a-f]{2}(?:-[0-9a-f]{2}){5}|[0-9a-f]{4}(?:\.[0-9a-f]{4}){2})\b",
        color: "cyan",
        default: false,
    },
    HighlightPreset {
        id: "uuid",
        pattern: r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b",
        color: "magenta",
        default: false,
    },
    HighlightPreset {
        id: "datetime",
        pattern: r"(?:\b[0-9]{4}[-/.][01][0-9][-/.][0-3][0-9](?:[t ][0-9]{2}:[0-9]{2}(?::[0-9]{2}(?:[.,][0-9]{1,9})?)?(?:z|[+-][0-9]{2}:?[0-9]{2})?)?\b|\b[0-9]{4}年 ?[0-9]{1,2}月 ?[0-9]{1,2}日|\b[0-3][0-9]/(?:jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)/[0-9]{4}(?::[0-9]{2}){3}(?: [+-][0-9]{4})?\b|\b(?:(?:mon|tue|wed|thu|fri|sat|sun),? +)?(?:jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec) +[0-9]{1,2},? +(?:[0-9]{4}\b(?: +[0-9]{2}:[0-9]{2}(?::[0-9]{2}(?:[.,][0-9]{1,9})?)?\b)?|[0-9]{2}:[0-9]{2}(?::[0-9]{2}(?:[.,][0-9]{1,9})?)?\b)|\b(?:(?:mon|tue|wed|thu|fri|sat|sun), +)?[0-3]?[0-9] +(?:jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec) +[0-9]{4} +[0-9]{2}:[0-9]{2}(?::[0-9]{2}(?:[.,][0-9]{1,9})?)?(?: +(?:gmt|utc|[+-][0-9]{4}))?\b|\b[0-9]{2}:[0-9]{2}:[0-9]{2}(?:[.,][0-9]{1,9})?\b)",
        color: "blue",
        default: false,
    },
    HighlightPreset {
        id: "number",
        pattern: r"\b(?:0x[0-9a-f]+|[0-9]+(?:\.[0-9]+)*(?:[a-z]{1,4})?)\b",
        color: "magenta",
        default: false,
    },
];

/// 规则是哪条预设；不是预设时为 `None`。
pub fn highlight_preset_of(rule: &HighlightRule) -> Option<&'static HighlightPreset> {
    HIGHLIGHT_PRESETS.iter().find(|preset| preset.matches(rule))
}

/// 添加预设时插在哪里：列表中第一条顺序在它后面的预设之前；后面没有预设时放在最后。自定义规则不参与比较。
pub fn preset_insert_position(rules: &[HighlightRule], preset: &HighlightPreset) -> usize {
    let rank = |rule: &HighlightRule| HIGHLIGHT_PRESETS.iter().position(|p| p.matches(rule));
    let Some(own) = HIGHLIGHT_PRESETS.iter().position(|p| p.id == preset.id) else {
        return rules.len();
    };
    rules
        .iter()
        .position(|rule| rank(rule).is_some_and(|i| i > own))
        .unwrap_or(rules.len())
}

/// 0.1.0–0.3.2 的默认规则（第 1 版）：读到与它完全相同、没有版本标记的规则时升级成现在的默认（见 `HighlightSettings`）。
pub(crate) fn legacy_default_rules() -> Vec<HighlightRule> {
    ["errors", "warnings", "success", "ipv4"]
        .iter()
        .filter_map(|id| HIGHLIGHT_PRESETS.iter().find(|p| p.id == *id))
        .map(HighlightPreset::rule)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset(id: &str) -> &'static HighlightPreset {
        HIGHLIGHT_PRESETS
            .iter()
            .find(|p| p.id == id)
            .unwrap_or_else(|| panic!("没有预设 {id}"))
    }

    #[test]
    fn presets_are_unique_and_recognize_themselves() {
        let ids: std::collections::HashSet<_> = HIGHLIGHT_PRESETS.iter().map(|p| p.id).collect();
        assert_eq!(ids.len(), HIGHLIGHT_PRESETS.len());
        let defaults: Vec<_> = HIGHLIGHT_PRESETS
            .iter()
            .filter(|p| p.default)
            .map(|p| p.id)
            .collect();
        assert_eq!(defaults, ["errors", "warnings", "success", "ipv6", "ipv4"]);
        for p in &HIGHLIGHT_PRESETS {
            assert_eq!(highlight_preset_of(&p.rule()), Some(p), "{}", p.id);
        }
        // 改了颜色、停用了仍算这条预设；不按正则匹配就不算
        let mut ipv4 = preset("ipv4").rule();
        ipv4.color = "blue".into();
        ipv4.enabled = false;
        assert_eq!(highlight_preset_of(&ipv4).map(|p| p.id), Some("ipv4"));
        ipv4.regex = false;
        assert_eq!(highlight_preset_of(&ipv4), None);
    }

    #[test]
    fn presets_are_inserted_in_their_order() {
        let defaults: Vec<HighlightRule> = HIGHLIGHT_PRESETS
            .iter()
            .filter(|p| p.default)
            .map(HighlightPreset::rule)
            .collect();
        assert_eq!(preset_insert_position(&defaults, preset("url")), 0);
        assert_eq!(
            preset_insert_position(&defaults, preset("mac")),
            defaults.len()
        );
        assert_eq!(
            preset_insert_position(&defaults, preset("number")),
            defaults.len()
        );
        assert_eq!(preset_insert_position(&[], preset("ipv6")), 0);
        // 夹着自定义规则：放在第一条顺序在后面的预设之前
        let custom = HighlightRule {
            pattern: "timeout".into(),
            color: "magenta".into(),
            enabled: true,
            ..Default::default()
        };
        let rules = vec![
            custom.clone(),
            preset("errors").rule(),
            custom.clone(),
            preset("ipv4").rule(),
        ];
        assert_eq!(preset_insert_position(&rules, preset("ipv6")), 3);
        assert_eq!(preset_insert_position(&rules, preset("url")), 1);
        assert_eq!(preset_insert_position(&rules, preset("datetime")), 4);
        assert_eq!(preset_insert_position(&[custom], preset("url")), 1);
    }

    // 已发布版本的默认规则：原文抄在这里，防止有人改了前 4 条预设的正则，老用户的规则就不再被认出、不再升级
    #[test]
    fn legacy_rules_are_the_released_defaults() {
        let legacy: Vec<(String, String)> = legacy_default_rules()
            .into_iter()
            .map(|r| (r.pattern, r.color))
            .collect();
        let released = [
            (
                r"\b(error|errors|fail|failed|failure|fatal|panic|denied|refused|exception|critical)\b",
                "red",
            ),
            (r"\b(warn|warning|warnings|deprecated)\b", "yellow"),
            (r"\b(ok|success|successful|succeeded|passed)\b", "green"),
            (r"\b(?:\d{1,3}\.){3}\d{1,3}(?:/\d{1,2})?\b", "cyan"),
        ];
        assert_eq!(
            legacy,
            released
                .map(|(p, c)| (p.to_string(), c.to_string()))
                .to_vec()
        );
    }
}
