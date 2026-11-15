//! 设置在数据库中的读写：每组一行。

use rusqlite::Connection;

use crate::db::{self, SettingRow};
use crate::model::{
    AppLockSettings, AppearanceSettings, BackupSettings, CliSettings, ConnectionSettings,
    GeneralSettings, GuardSettings, HighlightSettings, KeybindingSettings, LocalConnectionSettings,
    LocalSettings, LogSettings, Settings, SettingsSection, ShellSettings, TerminalSettings,
    UsageSettings, WindowSettings,
};

#[derive(Clone, Default)]
pub(super) struct SettingsState {
    pub synced: Settings,
    pub local: LocalSettings,
}

impl SettingsState {
    pub fn load(conn: &Connection) -> rusqlite::Result<Self> {
        let synced = db::load_settings(conn, "setting")?;
        let local = db::load_settings(conn, "local_setting")?;
        Ok(Self {
            synced: Settings {
                general: section(&synced),
                appearance: section(&synced),
                terminal: section(&synced),
                highlight: section(&synced),
                connection: section(&synced),
                guard: section(&synced),
                keybindings: section(&synced),
            },
            local: LocalSettings {
                cli: section(&local),
                app_lock: section(&local),
                backup: section(&local),
                shells: section(&local),
                logs: section(&local),
                usage: section(&local),
                window: section(&local),
                connection: section(&local),
            },
        })
    }
}

/// 从数据库的行中取出一组设置；没有这一行或内容损坏时用默认值。
fn section<S: SettingsSection>(rows: &[SettingRow]) -> S {
    rows.iter()
        .find(|row| row.id == S::ID)
        .and_then(|row| match serde_json::from_str(&row.doc) {
            Ok(section) => Some(section),
            Err(e) => {
                tracing::warn!(section = S::ID, "设置内容无法解析，改用默认值：{e}");
                None
            }
        })
        .unwrap_or_default()
}

fn push_changed<S: SettingsSection>(
    rows: &mut Vec<SettingRow>,
    old: &S,
    new: &S,
    now: i64,
) -> serde_json::Result<()> {
    if old != new {
        rows.push(SettingRow {
            id: S::ID.to_string(),
            doc: serde_json::to_string(new)?,
            updated_at: now,
        });
    }
    Ok(())
}

pub(super) fn changed_rows(old: &Settings, new: &Settings) -> serde_json::Result<Vec<SettingRow>> {
    let now = crate::time::now_ms();
    let mut rows = Vec::new();
    push_changed::<GeneralSettings>(&mut rows, &old.general, &new.general, now)?;
    push_changed::<AppearanceSettings>(&mut rows, &old.appearance, &new.appearance, now)?;
    push_changed::<TerminalSettings>(&mut rows, &old.terminal, &new.terminal, now)?;
    push_changed::<HighlightSettings>(&mut rows, &old.highlight, &new.highlight, now)?;
    push_changed::<ConnectionSettings>(&mut rows, &old.connection, &new.connection, now)?;
    push_changed::<GuardSettings>(&mut rows, &old.guard, &new.guard, now)?;
    push_changed::<KeybindingSettings>(&mut rows, &old.keybindings, &new.keybindings, now)?;
    Ok(rows)
}

pub(super) fn changed_local_rows(
    old: &LocalSettings,
    new: &LocalSettings,
) -> serde_json::Result<Vec<SettingRow>> {
    let now = crate::time::now_ms();
    let mut rows = Vec::new();
    push_changed::<CliSettings>(&mut rows, &old.cli, &new.cli, now)?;
    push_changed::<AppLockSettings>(&mut rows, &old.app_lock, &new.app_lock, now)?;
    push_changed::<BackupSettings>(&mut rows, &old.backup, &new.backup, now)?;
    push_changed::<ShellSettings>(&mut rows, &old.shells, &new.shells, now)?;
    push_changed::<LogSettings>(&mut rows, &old.logs, &new.logs, now)?;
    push_changed::<UsageSettings>(&mut rows, &old.usage, &new.usage, now)?;
    push_changed::<WindowSettings>(&mut rows, &old.window, &new.window, now)?;
    push_changed::<LocalConnectionSettings>(&mut rows, &old.connection, &new.connection, now)?;
    Ok(rows)
}
