//! 危险命令规则的草稿：内置规则的开关、自定义规则的整理与校验、规则测试的结果与文案（不依赖界面，便于测试）。

use gpui_kit::SharedString;
use termlet_core::guard::{Guard, MAX_CUSTOM_RULES, MAX_PATTERN_LENGTH, Rule, RuleError};
use termlet_core::model::{Approval, GuardSettings};

use crate::i18n;
use crate::t;

/// 打开或关闭一条内置规则：关闭的规则 ID 记在 `disabled` 中（已有的顺序不变，新关闭的加在末尾）。
/// 设置中不认识的 ID（更新版本的规则）原样保留。
pub(super) fn set_rule_enabled(disabled: &mut Vec<String>, id: &str, enabled: bool) {
    if enabled {
        disabled.retain(|d| d != id);
    } else if !disabled.iter().any(|d| d == id) {
        disabled.push(id.to_string());
    }
}

/// 保存的自定义规则：去掉首尾空白，空的去掉。
pub(super) fn custom_patterns<'a>(patterns: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    patterns
        .into_iter()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// 一条自定义规则（输入框中的原文）的错误；空的不算错误（保存时去掉）。
pub(super) fn custom_error(pattern: &str) -> Option<RuleError> {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return None;
    }
    termlet_core::guard::validate_custom_rule(pattern).err()
}

/// 规则测试用的设置：草稿中的内置规则开关与自定义规则。
pub(super) fn draft_settings(
    disabled: &[String],
    custom: Vec<String>,
    approval: Approval,
) -> GuardSettings {
    GuardSettings {
        disabled_rules: disabled.to_vec(),
        custom_rules: custom,
        approval,
        ..Default::default()
    }
}

/// 用草稿中的规则检查一条命令：空命令为 `None`，否则为命中的规则（没有命中时为 `Some(None)`）。
/// 不能使用的自定义规则跳过（输入框下另有提示），与保存后命令行服务的行为相同。
pub(super) fn test_command(guard: &Guard, command: &str) -> Option<Option<Rule>> {
    if command.trim().is_empty() {
        return None;
    }
    Some(guard.check(command).cloned())
}

/// 规则在界面上的名称：内置规则为当前语言的说明，自定义规则为正则本身。
pub(super) fn rule_name(rule: &Rule) -> SharedString {
    if !rule.is_builtin() {
        return t!("cli_ui.rules.custom_name", pattern = rule.pattern());
    }
    SharedString::from(i18n::rule_description(rule).to_string())
}

/// 自定义规则不能使用的原因。正则的语法错误详情是 `regex` crate 的英文报错，不显示。
pub(super) fn error_text(error: &RuleError) -> SharedString {
    match error {
        RuleError::TooLong => t!("cli_ui.rules.error_too_long", max = MAX_PATTERN_LENGTH),
        RuleError::Syntax(_) => t!("cli_ui.rules.error_syntax"),
        RuleError::TooComplex => t!("cli_ui.rules.error_too_complex"),
        RuleError::MatchesEverything => t!("cli_ui.rules.error_matches_everything"),
        RuleError::TooMany => t!("cli_ui.rules.error_too_many", max = MAX_CUSTOM_RULES),
    }
}

/// 规则测试的结果说明：没有命中、命中后询问或拦截。
pub(super) fn outcome_text(rule: Option<&Rule>, approval: Approval) -> SharedString {
    match (rule, approval) {
        (None, _) => t!("cli_ui.rules.test_allowed"),
        (Some(rule), Approval::Ask) => t!("cli_ui.rules.test_ask", rule = rule_name(rule)),
        (Some(rule), Approval::Block) => t!("cli_ui.rules.test_block", rule = rule_name(rule)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Language;

    fn guard(disabled: &[&str], custom: &[&str]) -> Guard {
        let disabled: Vec<String> = disabled.iter().map(|s| s.to_string()).collect();
        let settings = draft_settings(
            &disabled,
            custom_patterns(custom.iter().copied()),
            Approval::Ask,
        );
        Guard::from_settings(&settings).0
    }

    #[test]
    fn toggling_builtin_rules_keeps_unknown_ids_and_order() {
        let mut disabled = vec!["from-a-newer-version".to_string(), "power".to_string()];
        set_rule_enabled(&mut disabled, "mkfs", false);
        set_rule_enabled(&mut disabled, "mkfs", false);
        assert_eq!(disabled, ["from-a-newer-version", "power", "mkfs"]);
        set_rule_enabled(&mut disabled, "power", true);
        assert_eq!(disabled, ["from-a-newer-version", "mkfs"]);
        set_rule_enabled(&mut disabled, "not-disabled", true);
        assert_eq!(disabled.len(), 2);
    }

    #[test]
    fn custom_rules_are_trimmed_and_empty_ones_dropped() {
        assert_eq!(
            custom_patterns(["  \\bpurge\\b ", "", "   ", "^halt$"]),
            ["\\bpurge\\b", "^halt$"]
        );
        assert_eq!(custom_error("   "), None, "空的规则保存时去掉，不算错误");
        assert_eq!(custom_error(" \\bpurge\\b "), None);
        assert!(matches!(custom_error("("), Some(RuleError::Syntax(_))));
        assert_eq!(custom_error(".*"), Some(RuleError::MatchesEverything));
        assert_eq!(custom_error("x?"), Some(RuleError::MatchesEverything));
        assert_eq!(
            custom_error(&"a".repeat(MAX_PATTERN_LENGTH + 1)),
            Some(RuleError::TooLong)
        );
    }

    #[test]
    fn test_uses_the_draft_rules() {
        assert_eq!(test_command(&guard(&[], &[]), "  "), None);
        let hit = test_command(&guard(&[], &[]), "sudo reboot").unwrap();
        assert_eq!(hit.map(|r| r.id().to_string()).as_deref(), Some("power"));
        // 关掉的内置规则不再命中；自定义规则按草稿中的位置编号
        let draft = guard(&["power"], &["", " (?i)\\bdocker\\s+system\\s+prune\\b "]);
        assert_eq!(test_command(&draft, "sudo reboot"), Some(None));
        let hit = test_command(&draft, "Docker System Prune -af")
            .unwrap()
            .unwrap();
        assert_eq!(hit.id(), "custom-1");
        assert!(!hit.is_builtin());
        // 不能使用的自定义规则跳过，其余照常
        let draft = guard(&[], &["(", "\\bpurge\\b"]);
        let hit = test_command(&draft, "apt purge x").unwrap().unwrap();
        assert_eq!(hit.pattern(), "\\bpurge\\b");
    }

    #[test]
    fn texts_follow_the_language() {
        let original = i18n::language();
        let power = Guard::default().check("reboot").cloned().unwrap();
        let custom = guard(&[], &["^stopall$"])
            .check("stopall")
            .cloned()
            .unwrap();

        i18n::set_language(Language::ZhCn);
        assert_eq!(rule_name(&power), power.description());
        assert_eq!(rule_name(&custom), "自定义规则 ^stopall$");
        assert_eq!(
            outcome_text(Some(&custom), Approval::Block),
            "命中“自定义规则 ^stopall$”：会被拦截"
        );
        assert_eq!(
            outcome_text(None, Approval::Ask),
            "没有命中规则，会直接执行"
        );
        assert_eq!(
            error_text(&RuleError::TooLong),
            format!("规则过长（最多 {MAX_PATTERN_LENGTH} 个字节）")
        );
        assert_eq!(
            error_text(&RuleError::Syntax("unclosed group".into())),
            "不是合法的正则表达式",
            "正则库的英文报错不显示"
        );

        i18n::set_language(Language::ZhTw);
        assert_eq!(rule_name(&power), power.description_zh_tw());
        assert_eq!(rule_name(&custom), "自訂規則 ^stopall$");

        i18n::set_language(Language::En);
        assert_eq!(rule_name(&power), power.description_en());
        assert!(outcome_text(Some(&power), Approval::Ask).ends_with("approve it first"));
        assert_eq!(
            error_text(&RuleError::TooMany),
            format!("Over the limit of {MAX_CUSTOM_RULES} rules")
        );
        i18n::set_language(original);
    }
}
