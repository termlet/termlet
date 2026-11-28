//! 设置的三方合并。设置 Tab 可能开着很久，这期间别处也会写入设置（标题栏切换深浅色、命令面板放大界面、关闭确认中的
//! “以后不再询问”、云同步、另一个窗口的设置……）。保存时以打开设置时的值为基准按字段合并：这次改了的字段用这次的值，
//! 没改的用别处现在的值，不把别处的修改写回旧值。
//!
//! 按序列化后的 JSON 逐层比较：对象按键合并，数组与其他值整体当一个值。两边改了同一个字段时用这次的值，日志只记
//! 字段的路径（值里可能有代理地址、规则等不想写进日志的内容）。

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use super::{LocalSettings, Settings};

/// 合并会同步的设置：`mine` 是这次要保存的，`base` 是打开设置时的，`theirs` 是存储中现在的。
pub fn merge_settings(base: &Settings, mine: &Settings, theirs: &Settings) -> Settings {
    Settings {
        general: merge_section(&base.general, &mine.general, &theirs.general),
        appearance: merge_section(&base.appearance, &mine.appearance, &theirs.appearance),
        terminal: merge_section(&base.terminal, &mine.terminal, &theirs.terminal),
        highlight: merge_section(&base.highlight, &mine.highlight, &theirs.highlight),
        connection: merge_section(&base.connection, &mine.connection, &theirs.connection),
        guard: merge_section(&base.guard, &mine.guard, &theirs.guard),
        keybindings: merge_section(&base.keybindings, &mine.keybindings, &theirs.keybindings),
        snippets: merge_section(&base.snippets, &mine.snippets, &theirs.snippets),
    }
}

/// 合并只在这台电脑上的设置（参数同 [`merge_settings`]）。
pub fn merge_local_settings(
    base: &LocalSettings,
    mine: &LocalSettings,
    theirs: &LocalSettings,
) -> LocalSettings {
    LocalSettings {
        cli: merge_section(&base.cli, &mine.cli, &theirs.cli),
        app_lock: merge_section(&base.app_lock, &mine.app_lock, &theirs.app_lock),
        backup: merge_section(&base.backup, &mine.backup, &theirs.backup),
        shells: merge_section(&base.shells, &mine.shells, &theirs.shells),
        logs: merge_section(&base.logs, &mine.logs, &theirs.logs),
        usage: merge_section(&base.usage, &mine.usage, &theirs.usage),
        window: merge_section(&base.window, &mine.window, &theirs.window),
        connection: merge_section(&base.connection, &mine.connection, &theirs.connection),
    }
}

/// 合并一组设置。只有一边改过时直接取那一边（不经序列化，读取时的升级等不会再走一遍）；两边都改过时按字段合并，
/// 合并后读不回来（不该发生）时用这次的值。
fn merge_section<T>(base: &T, mine: &T, theirs: &T) -> T
where
    T: Clone + PartialEq + Serialize + DeserializeOwned,
{
    if mine == base {
        return theirs.clone();
    }
    if theirs == base || theirs == mine {
        return mine.clone();
    }
    let (Ok(b), Ok(m), Ok(t)) = (
        serde_json::to_value(base),
        serde_json::to_value(mine),
        serde_json::to_value(theirs),
    ) else {
        return mine.clone();
    };
    let mut conflicts = Vec::new();
    let merged = merge_value(&b, &m, &t, "", &mut conflicts);
    if !conflicts.is_empty() {
        tracing::info!(fields = ?conflicts, "合并设置：这几项两边都改了，用这次保存的值");
    }
    serde_json::from_value(merged).unwrap_or_else(|e| {
        tracing::warn!("合并后的设置读不回来，用这次保存的值：{e}");
        mine.clone()
    })
}

/// 按字段合并 JSON。`path` 是这一层的路径（只用于日志），两边改了同一个值时把路径记进 `conflicts`。
fn merge_value(
    base: &Value,
    mine: &Value,
    theirs: &Value,
    path: &str,
    conflicts: &mut Vec<String>,
) -> Value {
    if mine == base {
        return theirs.clone();
    }
    if theirs == base || theirs == mine {
        return mine.clone();
    }
    match (mine, theirs) {
        (Value::Object(m), Value::Object(t)) => {
            let empty = Map::new();
            let b = base.as_object().unwrap_or(&empty);
            let mut merged = Map::new();
            let keys = m.keys().chain(t.keys()).chain(b.keys());
            for key in keys {
                if merged.contains_key(key) {
                    continue;
                }
                let path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                if let Some(value) =
                    merge_field(b.get(key), m.get(key), t.get(key), &path, conflicts)
                {
                    merged.insert(key.clone(), value);
                }
            }
            Value::Object(merged)
        }
        _ => {
            conflicts.push(path.to_string());
            mine.clone()
        }
    }
}

/// 合并对象中的一个键；结果为 `None` 时这个键不要了（被删掉的一边算改过）。
fn merge_field(
    base: Option<&Value>,
    mine: Option<&Value>,
    theirs: Option<&Value>,
    path: &str,
    conflicts: &mut Vec<String>,
) -> Option<Value> {
    if mine == base {
        return theirs.cloned();
    }
    if theirs == base || theirs == mine {
        return mine.cloned();
    }
    match (mine, theirs) {
        (Some(m), Some(t)) => Some(merge_value(
            base.unwrap_or(&Value::Null),
            m,
            t,
            path,
            conflicts,
        )),
        _ => {
            conflicts.push(path.to_string());
            mine.cloned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HttpScope, ThemeSetting};

    // 打开设置后标题栏切到了深色（同一组的另一项）：这次只改了语言，保存后主题仍是深色
    #[test]
    fn untouched_fields_keep_what_was_changed_elsewhere() {
        let base = Settings::default();
        let mut mine = base.clone();
        mine.appearance.language = crate::model::LanguageSetting::En;
        let mut theirs = base.clone();
        theirs.appearance.theme = ThemeSetting::Dark;
        let merged = merge_settings(&base, &mine, &theirs);
        assert_eq!(
            merged.appearance.language,
            crate::model::LanguageSetting::En
        );
        assert_eq!(merged.appearance.theme, ThemeSetting::Dark);
    }

    // 两边改了同一项：用这次保存的值；别的组照常取别处现在的值
    #[test]
    fn both_changed_the_same_field_mine_wins() {
        let base = Settings::default();
        let mut mine = base.clone();
        mine.terminal.font_size = 15;
        let mut theirs = base.clone();
        theirs.terminal.font_size = 18;
        theirs.general.confirm_close = !base.general.confirm_close;
        let merged = merge_settings(&base, &mine, &theirs);
        assert_eq!(merged.terminal.font_size, 15);
        assert_eq!(merged.general.confirm_close, theirs.general.confirm_close);
    }

    // 嵌套的对象按键逐层合并；列表整体当一个值
    #[test]
    fn nested_objects_merge_by_key_and_lists_as_a_whole() {
        let base = LocalSettings::default();
        let mut mine = base.clone();
        mine.cli.http.port = 47900;
        mine.connection.x11_display = "localhost:1".into();
        let mut theirs = base.clone();
        theirs.cli.http.scope = HttpScope::Lan;
        theirs.connection.allowed_proxy_commands = vec!["nc %h %p".into()];
        let merged = merge_local_settings(&base, &mine, &theirs);
        assert_eq!(merged.cli.http.port, 47900);
        assert_eq!(merged.cli.http.scope, HttpScope::Lan);
        assert_eq!(merged.connection.x11_display, "localhost:1");
        assert_eq!(
            merged.connection.allowed_proxy_commands,
            vec!["nc %h %p".to_string()]
        );

        let base = Settings::default();
        let mut mine = base.clone();
        mine.guard.custom_rules = vec!["rm -rf /tmp".into()];
        let mut theirs = base.clone();
        theirs.guard.custom_rules = vec!["shutdown".into()];
        let merged = merge_settings(&base, &mine, &theirs);
        assert_eq!(merged.guard.custom_rules, vec!["rm -rf /tmp".to_string()]);
    }

    // 不认识的字段（更新版本同步来的）也按键合并，不丢
    #[test]
    fn unknown_fields_are_kept() {
        let base = Settings::default();
        let mut mine = base.clone();
        mine.general.show_host_address = !base.general.show_host_address;
        let mut theirs = base.clone();
        theirs
            .general
            .extra
            .insert("newOption".into(), serde_json::json!(true));
        let merged = merge_settings(&base, &mine, &theirs);
        assert_eq!(
            merged.general.show_host_address,
            mine.general.show_host_address
        );
        assert_eq!(
            merged.general.extra.get("newOption"),
            Some(&serde_json::json!(true))
        );
    }

    // 没有改动的一边：原样取另一边
    #[test]
    fn one_sided_changes_pass_through() {
        let base = Settings::default();
        let mut theirs = base.clone();
        theirs.terminal.scrollback = 20_000;
        assert_eq!(merge_settings(&base, &base, &theirs), theirs);
        assert_eq!(merge_settings(&base, &theirs, &base), theirs);
    }
}
