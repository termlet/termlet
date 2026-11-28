//! 设置搜索：导航顶上的搜索框（设置这一栏窄时在内容上方）按分类名、设置项的标题与说明过滤分类，没有命中的分类不
//! 显示，分类名后写命中的项数；命中的项加底色，在收起的一节里、另一个页签上的展开、切过去，并滚到第一个命中的项。
//! 五种界面语言的文字都能搜：中文界面下输入“font”也能找到“终端字体”。几个词用空格隔开时每个都要有。
//!
//! - 可以搜到的项登记在 [`ITEMS`] 中：所在的分类与位置（收起的一节、页签）、锚点、标题与说明的文案键。各页渲染时用
//!   [`Hits::mark`] 按锚点给命中的项加底色；测试逐项核对锚点都画在登记的地方。新加设置项时在这里登记。
//! - 快捷键页另有自己的搜索框（按动作名、按键过滤，见 `shortcuts.rs`）：这里搜到快捷键时把搜索的文字带过去。
//! - 一个窗口最多一个设置 Tab：搜索结果按窗口登记（[`publish`]），各页渲染时按窗口取（[`hits`]），不必一层层传下去。

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::OnceLock;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    App, Bounds, Global, ParentElement, Pixels, Styled, Window, WindowId, canvas, rems,
};

use super::{ColorsTab, Fold, Page};
use crate::cli_ui::CliPart;
use crate::i18n::Language;

/// 设置项在页上的位置：收起的一节、页签。搜到时展开、切过去。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Place {
    Fold(Fold),
    Colors(ColorsTab),
    /// AI 与自动化中默认收起的部分
    Cli(CliPart),
}

/// 一个可以搜到的设置项。
pub(super) struct Item {
    pub(super) page: Page,
    pub(super) place: Option<Place>,
    /// 锚点：各页渲染时按它给命中的项加底色（多为控件的元素 ID）
    pub(super) anchor: &'static str,
    /// 标题、说明与选项的文案键（插值不算）
    keys: &'static [&'static str],
    /// 另外可以搜的文字，如内置危险命令规则的说明（核心中的五种语言）
    extra: Option<fn() -> Vec<String>>,
    /// 只在某些系统上、某些情况下才有的项：没有时不算命中
    shown: Option<fn(&App) -> bool>,
}

const fn item(page: Page, anchor: &'static str, keys: &'static [&'static str]) -> Item {
    Item {
        page,
        place: None,
        anchor,
        keys,
        extra: None,
        shown: None,
    }
}

impl Item {
    const fn at(mut self, place: Place) -> Self {
        self.place = Some(place);
        self
    }

    const fn when(mut self, shown: fn(&App) -> bool) -> Self {
        self.shown = Some(shown);
        self
    }

    const fn with(mut self, extra: fn() -> Vec<String>) -> Self {
        self.extra = Some(extra);
        self
    }
}

const fn folded(
    page: Page,
    fold: Fold,
    anchor: &'static str,
    keys: &'static [&'static str],
) -> Item {
    item(page, anchor, keys).at(Place::Fold(fold))
}

/// 可以搜到的设置项，按页上的顺序（快捷键页的动作另外搜，见 `shortcuts.rs`）。
pub(super) static ITEMS: &[Item] = &[
    // ---------------------------------------------------------------- 通用
    item(
        Page::General,
        "settings-language",
        &["settings.language", "settings.language.system"],
    ),
    item(
        Page::General,
        "settings-theme",
        &[
            "settings.app_theme",
            "settings.theme.light",
            "settings.theme.dark",
            "settings.theme.system",
        ],
    ),
    item(
        Page::General,
        "settings-icons",
        &[
            "settings.icons",
            "settings.icons.color",
            "settings.icons.mono",
        ],
    ),
    item(
        Page::General,
        "settings-ui-scale",
        &[
            "settings.ui_scale",
            "settings.ui_scale_hint",
            "settings.ui_scale_hint_windows",
        ],
    ),
    item(
        Page::General,
        "settings-confirm-close",
        &["settings.confirm_close", "settings.confirm_close_hint"],
    ),
    item(
        Page::General,
        "settings-show-host-address",
        &[
            "settings.show_host_address",
            "settings.show_host_address_hint",
        ],
    ),
    item(
        Page::General,
        "settings-close-to-tray",
        &["settings.close_to_tray", "settings.close_to_tray_hint"],
    )
    .when(|cx| crate::tray::supported(cx.platform_info().os)),
    item(
        Page::General,
        "settings-taskbar-progress",
        &[
            "settings.taskbar_progress",
            "settings.taskbar_progress_mac",
            "settings.taskbar_progress_linux",
            "settings.taskbar_progress_linux_hint",
        ],
    )
    .when(|cx| {
        use gpui_kit::OperatingSystem::*;
        matches!(cx.platform_info().os, Windows | Mac | Linux)
    }),
    item(
        Page::General,
        "settings-tab-position",
        &[
            "settings.tab_position",
            "settings.tab_position_top",
            "settings.tab_position_left",
            "settings.tab_position_hint",
        ],
    ),
    item(
        Page::General,
        "settings-group-tabs-by-host",
        &[
            "settings.group_tabs_by_host",
            "settings.group_tabs_by_host_hint",
        ],
    ),
    item(
        Page::General,
        "settings-remote-editor",
        &[
            "settings.remote_editor",
            "settings.remote_editor_hint",
            "settings.remote_editor.vscode",
            "settings.remote_editor.zed",
        ],
    ),
    item(
        Page::General,
        "settings-startup",
        &[
            "startup_open.label",
            "startup_open.hint",
            "startup_open.local",
            "startup_open.hosts",
        ],
    ),
    folded(
        Page::General,
        Fold::Toolbar,
        "settings-fold-toolbar",
        &[
            "toolbar.nav",
            "toolbar.settings_hint",
            "toolbar.item.host_tools",
            "toolbar.item.multi_exec",
            "toolbar.item.forwards",
            "toolbar.item.sftp",
            "toolbar.item.snippets",
            "toolbar.item.compose",
            "toolbar.item.batch_run",
            "toolbar.item.highlight",
            "toolbar.item.theme",
            "toolbar.item.lock",
        ],
    ),
    // ---------------------------------------------------------------- 终端
    item(
        Page::Terminal,
        "settings-scheme-slot",
        &["schemes.terminal_field", "schemes.terminal_field_hint"],
    ),
    item(
        Page::Terminal,
        "settings-font-size",
        &["settings.font_size"],
    ),
    item(
        Page::Terminal,
        "settings-scrollback",
        &["settings.scrollback"],
    ),
    item(
        Page::Terminal,
        "settings-font-family",
        &["settings.font.label", "settings.font.hint"],
    ),
    item(
        Page::Terminal,
        "settings-cursor-shape",
        &[
            "settings.cursor.label",
            "settings.cursor.hint",
            "settings.cursor.block",
            "settings.cursor.bar",
            "settings.cursor.underline",
        ],
    ),
    item(
        Page::Terminal,
        "settings-cursor-blink",
        &["settings.cursor_blink"],
    ),
    item(
        Page::Terminal,
        "settings-copy-on-select",
        &["settings.copy_on_select"],
    ),
    item(
        Page::Terminal,
        "settings-right-click",
        &[
            "settings.right_click.label",
            "settings.right_click.auto",
            "settings.right_click.paste",
            "settings.right_click.menu",
        ],
    ),
    item(
        Page::Terminal,
        "settings-clipboard-write",
        &["settings.clipboard_write", "settings.clipboard_write_hint"],
    ),
    item(
        Page::Terminal,
        "settings-option-as-meta",
        &["settings.option_as_meta", "settings.option_as_meta_hint"],
    )
    .when(|cx| cx.platform_info().os == gpui_kit::OperatingSystem::Mac),
    item(
        Page::Terminal,
        "settings-paste-warning",
        &["settings.paste_warning", "settings.paste_warning_hint"],
    ),
    item(
        Page::Terminal,
        "settings-password-fill",
        &["settings.password_fill", "settings.password_fill_hint"],
    ),
    folded(
        Page::Terminal,
        Fold::StatusBar,
        "settings-fold-status-bar",
        &["settings.section.status_bar"],
    ),
    folded(
        Page::Terminal,
        Fold::StatusBar,
        "settings-monitor",
        &["settings.monitor", "settings.monitor_hint"],
    ),
    folded(
        Page::Terminal,
        Fold::StatusBar,
        "settings-latency",
        &["settings.latency"],
    ),
    folded(
        Page::Terminal,
        Fold::StatusBar,
        "settings-monitor-bar",
        &[
            "settings.monitor_bar.label",
            "settings.monitor_bar.hint",
            "monitor.bar.style_full",
            "monitor.bar.style_compact",
        ],
    ),
    folded(
        Page::Terminal,
        Fold::StatusBar,
        "settings-tab-activity",
        &["settings.tab_activity", "settings.tab_activity_hint"],
    ),
    folded(
        Page::Terminal,
        Fold::StatusBar,
        "settings-notify-done",
        &[
            "settings.notify_command_done",
            "settings.notify_command_done_hint",
        ],
    ),
    folded(
        Page::Terminal,
        Fold::FileTransfer,
        "settings-fold-file-transfer",
        &["settings.section.file_transfer"],
    ),
    folded(
        Page::Terminal,
        Fold::FileTransfer,
        "settings-file-transfer",
        &["settings.file_transfer", "settings.file_transfer_hint"],
    ),
    folded(
        Page::Terminal,
        Fold::FileTransfer,
        "settings-transfer-auto-clear",
        &[
            "settings.transfer_auto_clear",
            "settings.transfer_auto_clear_hint",
        ],
    ),
    folded(
        Page::Terminal,
        Fold::Logs,
        "settings-fold-logs",
        &["settings.section.logs"],
    ),
    folded(
        Page::Terminal,
        Fold::Logs,
        "settings-session-log",
        &["settings.session_log", "settings.session_log_hint"],
    ),
    folded(
        Page::Terminal,
        Fold::Logs,
        "settings-log-dir",
        &["settings.session_log_dir", "settings.session_log_dir_hint"],
    ),
    folded(
        Page::Terminal,
        Fold::Logs,
        "settings-record-input",
        &["settings.record_input", "settings.record_input_hint"],
    ),
    folded(
        Page::Terminal,
        Fold::Logs,
        "settings-record-dir",
        &["settings.record_dir", "settings.record_dir_hint"],
    ),
    folded(
        Page::Terminal,
        Fold::History,
        "settings-fold-history",
        &["settings.section.history"],
    ),
    folded(
        Page::Terminal,
        Fold::History,
        "settings-command-history",
        &["settings.command_history", "settings.command_history_hint"],
    ),
    folded(
        Page::Terminal,
        Fold::History,
        "settings-clear-history",
        &[
            "settings.clear_history",
            "settings.shell_integration",
            "settings.shell_integration_hint",
        ],
    ),
    folded(
        Page::Terminal,
        Fold::History,
        "settings-command-suggest",
        &["settings.command_suggest", "settings.command_suggest_hint"],
    ),
    // ---------------------------------------------------------------- 本地终端
    item(
        Page::Local,
        "settings-default-shell",
        &[
            "local_shell.default_label",
            "local_shell.default_hint",
            "local_shell.default_hint_no_key",
        ],
    ),
    item(
        Page::Local,
        "settings-shells-shown",
        &["local_shell.shown_label", "local_shell.shown_hint"],
    ),
    item(
        Page::Local,
        "settings-shell-new",
        &[
            "local_shell.new",
            "local_shell.name",
            "local_shell.command",
            "local_shell.command_hint",
            "local_shell.dir",
            "local_shell.env",
            "local_shell.settings_hint",
        ],
    ),
    // ---------------------------------------------------------------- 配色与高亮
    item(
        Page::Colors,
        "settings-scheme-slots",
        &[
            "schemes.nav",
            "schemes.slot_light",
            "schemes.slot_dark",
            "schemes.slots_hint",
        ],
    )
    .at(Place::Colors(ColorsTab::Schemes)),
    item(
        Page::Colors,
        "settings-min-contrast",
        &["schemes.min_contrast", "schemes.min_contrast_hint"],
    )
    .at(Place::Colors(ColorsTab::Schemes)),
    item(
        Page::Colors,
        "settings-scheme-files",
        &["schemes.import", "schemes.export", "schemes.search"],
    )
    .at(Place::Colors(ColorsTab::Schemes)),
    item(
        Page::Colors,
        "settings-highlight",
        &[
            "settings.nav.highlight",
            "settings.highlight.enable",
            "settings.highlight.hint",
        ],
    )
    .at(Place::Colors(ColorsTab::Highlight)),
    item(
        Page::Colors,
        "highlight-rules",
        &[
            "settings.highlight.pattern",
            "settings.highlight.regex",
            "settings.highlight.case",
            "settings.highlight.color",
            "settings.highlight.add",
            "settings.highlight.add_preset",
            "settings.highlight.preset.url",
            "settings.highlight.preset.errors",
            "settings.highlight.preset.warnings",
            "settings.highlight.preset.success",
            "settings.highlight.preset.ipv6",
            "settings.highlight.preset.ipv4",
            "settings.highlight.preset.mac",
            "settings.highlight.preset.uuid",
            "settings.highlight.preset.datetime",
            "settings.highlight.preset.number",
        ],
    )
    .at(Place::Colors(ColorsTab::Highlight)),
    item(
        Page::Colors,
        "settings-highlight-preview",
        &["settings.highlight.preview"],
    )
    .at(Place::Colors(ColorsTab::Highlight)),
    // ---------------------------------------------------------------- 连接
    item(
        Page::Connection,
        "settings-keepalive",
        &["settings.keepalive", "settings.keepalive_hint"],
    ),
    item(
        Page::Connection,
        "settings-auto-reconnect",
        &["settings.auto_reconnect"],
    ),
    item(
        Page::Connection,
        "settings-detect-os",
        &["settings.detect_os", "settings.detect_os_hint"],
    ),
    item(
        Page::Connection,
        "settings-external-launch",
        &[
            "settings.external_launch",
            "settings.external_launch_hint",
            "settings.external_launch_ask",
            "settings.external_launch_allow",
            "settings.external_launch_off",
        ],
    ),
    folded(
        Page::Connection,
        Fold::ConnectionAdvanced,
        "settings-fold-connection-advanced",
        &["settings.section.connection_advanced"],
    ),
    folded(
        Page::Connection,
        Fold::ConnectionAdvanced,
        "settings-agent-path",
        &["settings.agent_path", "settings.agent_path_hint"],
    ),
    folded(
        Page::Connection,
        Fold::ConnectionAdvanced,
        "settings-x11-display",
        &["settings.x11_display", "settings.x11_display_hint"],
    ),
    folded(
        Page::Connection,
        Fold::ConnectionAdvanced,
        "settings-local-proxy",
        &["settings.local_proxy", "settings.local_proxy_hint"],
    ),
    folded(
        Page::Connection,
        Fold::ConnectionAdvanced,
        "settings-local-proxy-port",
        &[
            "settings.local_proxy_port",
            "settings.local_proxy_port_hint",
        ],
    ),
    folded(
        Page::Connection,
        Fold::ConnectionAdvanced,
        "settings-proxy-commands",
        &[
            "settings.proxy_commands",
            "settings.proxy_commands_hint",
            "settings.proxy_commands_forget",
        ],
    ),
    // ---------------------------------------------------------------- 安全
    // 应用锁的选项只在启用后出现，与启用、修改、停用一起归在这一节的标题下
    item(
        Page::Security,
        "settings-app-lock",
        &[
            "settings.section.app_lock",
            "lock_ui.security.description_no_key",
            "lock_ui.security.lock_now",
            "lock_ui.security.idle",
            "lock_ui.security.idle_hint",
            "lock_ui.security.on_startup",
            "lock_ui.security.on_system_lock",
            "lock_ui.security.enable",
            "lock_ui.security.change",
            "lock_ui.security.disable",
        ],
    ),
    item(
        Page::Security,
        "master-password",
        &[
            "lock_ui.master.title",
            "lock_ui.master.description",
            "lock_ui.master.description_portable",
            "lock_ui.master.change",
        ],
    )
    .when(|cx| crate::app_state::AppState::store(cx).vault_uses_password()),
    // 便携版在这台电脑上记住了主密码时才有
    item(
        Page::Security,
        "master-remembered",
        &["lock_ui.master.remembered", "lock_ui.master.forget"],
    )
    .when(|cx| {
        crate::app_state::AppState::store(cx).vault_remember()
            == termlet_core::vault::Remember::Remembered
    }),
    item(
        Page::Security,
        "host-cas",
        &[
            "known_hosts.ca_title",
            "known_hosts.ca_description",
            "known_hosts.ca_import",
        ],
    ),
    item(
        Page::Security,
        "settings-known-hosts",
        &[
            "settings.nav.known_hosts",
            "known_hosts.description",
            "known_hosts.search",
        ],
    ),
    // ---------------------------------------------------------------- AI 与自动化
    item(
        Page::Cli,
        "cli-enabled",
        &["cli_ui.enabled", "cli_ui.enabled_hint"],
    ),
    item(
        Page::Cli,
        "cli-auto-start",
        &["cli_ui.auto_start", "cli_ui.auto_start_hint"],
    ),
    item(
        Page::Cli,
        "cli-audit",
        &["cli_ui.audit.label", "cli_ui.audit.hint"],
    ),
    item(
        Page::Cli,
        "cli-access",
        &["cli_ui.section.tool", "cli_ui.intro"],
    )
    .at(Place::Cli(CliPart::Access)),
    item(
        Page::Cli,
        "cli-tool",
        &[
            "cli_ui.tool.label",
            "cli_ui.tool.add",
            "cli_ui.tool.examples",
        ],
    )
    .at(Place::Cli(CliPart::Access)),
    item(
        Page::Cli,
        "cli-skill",
        &["cli_ui.skill.label", "cli_ui.skill.hint"],
    )
    .at(Place::Cli(CliPart::Access)),
    item(
        Page::Cli,
        "cli-mcp",
        &[
            "cli_ui.mcp.label",
            "cli_ui.mcp.hint",
            "cli_ui.http.enabled",
            "cli_ui.http.scope",
            "cli_ui.http.scope_local",
            "cli_ui.http.scope_lan",
        ],
    )
    .at(Place::Cli(CliPart::Access)),
    item(
        Page::Cli,
        "cli-rules",
        &["cli_ui.section.rules", "cli_ui.rules.hint"],
    ),
    item(
        Page::Cli,
        "cli-approval",
        &[
            "cli_ui.rules.approval",
            "cli_ui.rules.approval_ask",
            "cli_ui.rules.approval_block",
        ],
    ),
    item(
        Page::Cli,
        "cli-builtin-rules",
        &["cli_ui.rules.builtin_count", "cli_ui.rules.builtin_hint"],
    )
    .at(Place::Cli(CliPart::Builtin))
    .with(builtin_rule_texts),
    item(
        Page::Cli,
        "cli-custom-rules",
        &["cli_ui.rules.custom", "cli_ui.rules.custom_hint"],
    ),
    item(
        Page::Cli,
        "cli-rule-test",
        &["cli_ui.rules.test", "cli_ui.rules.test_hint"],
    ),
    // ---------------------------------------------------------------- 备份
    item(
        Page::Backup,
        "backup-full",
        &[
            "backup_ui.page.full_title",
            "backup_ui.page.full_description",
            "backup_ui.page.create",
            "backup_ui.page.restore_from",
            "backup_ui.page.include_audit",
        ],
    ),
    item(
        Page::Backup,
        "backup-schedule",
        &[
            "backup_ui.schedule.title",
            "backup_ui.schedule.description",
            "backup_ui.schedule.folder",
            "backup_ui.schedule.frequency",
            "backup_ui.schedule.daily",
            "backup_ui.schedule.weekly",
            "backup_ui.schedule.keep",
            "backup_ui.schedule.password",
        ],
    ),
    item(
        Page::Backup,
        "backup-snapshots",
        &[
            "backup_ui.page.snapshots_title",
            "backup_ui.page.auto",
            "backup_ui.page.keep",
            "backup_ui.page.snapshot_now",
        ],
    ),
    // ---------------------------------------------------------------- 同步（没有设置时是表单，设置后是状态）
    item(
        Page::Sync,
        "sync-page",
        &[
            "sync_ui.description",
            "sync_ui.form.target",
            "sync_ui.form.target_folder",
            "sync_ui.form.target_webdav",
            "sync_ui.form.target_s3",
            "sync_ui.form.target_gist",
            "sync_ui.form.folder_hint",
            "sync_ui.form.url",
            "sync_ui.form.endpoint",
            "sync_ui.form.bucket",
            "sync_ui.form.region",
            "sync_ui.form.access_key",
            "sync_ui.form.token",
            "sync_ui.form.password",
            "sync_ui.form.device_name",
            "sync_ui.status.auto",
        ],
    ),
    // ---------------------------------------------------------------- 隐私与诊断
    item(
        Page::Privacy,
        "about-usage",
        &[
            "lock_ui.privacy.section",
            "lock_ui.privacy.usage",
            "lock_ui.privacy.usage_hint",
            "lock_ui.privacy.usage_view",
            "lock_ui.privacy.policy",
        ],
    ),
    item(
        Page::Privacy,
        "about-crash-report",
        &[
            "lock_ui.diag.crash_report",
            "lock_ui.diag.crash_report_hint",
        ],
    )
    .when(crate::lock_ui::crash_report_shown),
    item(
        Page::Privacy,
        "about-diagnostics",
        &[
            "lock_ui.diag.section",
            "lock_ui.diag.copy",
            "lock_ui.diag.export",
            "lock_ui.diag.open_logs",
            "lock_ui.diag.crash_records",
            "lock_ui.diag.hint",
        ],
    ),
    item(
        Page::Privacy,
        "diag-log-level",
        &[
            "lock_ui.diag.log_level.label",
            "lock_ui.diag.log_level.hint",
            "lock_ui.diag.log_level.detailed",
        ],
    ),
    // ---------------------------------------------------------------- 关于
    item(
        Page::About,
        "about-app-info",
        &[
            "settings.slogan",
            "settings.about.copy_version",
            "settings.about.website",
            "settings.about.changelog",
            "settings.about.feedback",
            "settings.about.license",
            "settings.about.source",
        ],
    ),
    item(
        Page::About,
        "settings-data-dir",
        &[
            "settings.data_dir",
            "settings.data_dir_portable",
            "settings.open_data_dir",
        ],
    ),
    item(
        Page::About,
        "about-updates",
        &[
            "lock_ui.update.section",
            "lock_ui.update.auto_check",
            "lock_ui.update.check",
        ],
    ),
];

/// 内置危险命令规则的说明（五种语言）与 ID。
fn builtin_rule_texts() -> Vec<String> {
    termlet_core::guard::builtin_rules()
        .flat_map(|rule| {
            [
                rule.id(),
                rule.description(),
                rule.description_zh_tw(),
                rule.description_en(),
                rule.description_ja(),
                rule.description_ko(),
            ]
            .map(str::to_string)
        })
        .collect()
}

// ------------------------------------------------------------ 匹配

/// 搜索框中的文字拆成的词（转成小写）；只有空白时为空。
pub(crate) fn terms(query: &str) -> Vec<String> {
    query.split_whitespace().map(str::to_lowercase).collect()
}

/// `text`（已转成小写）中每个词都有；没有词时也算。
pub(crate) fn matches(text: &str, terms: &[String]) -> bool {
    terms.iter().all(|term| text.contains(term.as_str()))
}

/// 文案键在五种界面语言下的文字（小写，去掉插值），搜索用。
pub(crate) fn texts(key: &str) -> String {
    Language::ALL
        .iter()
        .map(|language| strip_placeholders(&rust_i18n::t!(key, locale = language.code())))
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase()
}

/// 去掉文案中的插值（`%{name}`）。
fn strip_placeholders(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("%{") {
        out.push_str(&rest[..start]);
        match rest[start..].find('}') {
            Some(end) => rest = &rest[start + end + 1..],
            None => {
                rest = &rest[start..];
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// 各项可以搜的文字，与 [`ITEMS`] 一一对应（文案是编进程序的，第一次搜索时生成）。
fn haystacks() -> &'static [String] {
    static HAYSTACKS: OnceLock<Vec<String>> = OnceLock::new();
    HAYSTACKS.get_or_init(|| {
        ITEMS
            .iter()
            .map(|item| {
                let mut text: Vec<String> = item.keys.iter().map(|key| texts(key)).collect();
                if let Some(extra) = item.extra {
                    text.extend(extra().into_iter().map(|t| t.to_lowercase()));
                }
                text.join("\n")
            })
            .collect()
    })
}

/// 一次搜索的结果。
pub(super) struct Found {
    /// 有命中的分类（按导航的顺序）与各自命中的项数；只有分类名命中的项数为 0
    pub(super) pages: Vec<(Page, usize)>,
    /// 命中的项
    pub(super) items: Vec<&'static Item>,
}

/// 按 `terms`（不为空）搜索登记的项与分类名；快捷键页命中的行数（`shortcuts`）由那一页自己数。
pub(super) fn find(terms: &[String], shortcuts: usize, cx: &App) -> Found {
    let items: Vec<&'static Item> = ITEMS
        .iter()
        .zip(haystacks())
        .filter(|(item, text)| matches(text, terms) && item.shown.is_none_or(|shown| shown(cx)))
        .map(|(item, _)| item)
        .collect();
    let pages = Page::ALL
        .into_iter()
        .filter_map(|page| {
            let mut count = items.iter().filter(|item| item.page == page).count();
            if page == Page::Shortcuts {
                count += shortcuts;
            }
            (count > 0 || matches(&texts(page.title_key()), terms)).then_some((page, count))
        })
        .collect();
    Found { pages, items }
}

// ------------------------------------------------------------ 标出命中的项

/// 一个设置 Tab 现在的搜索结果：命中的项的锚点。各页渲染时按锚点给命中的项加底色（[`Hits::mark`]）。
#[derive(Default)]
pub(crate) struct Hits {
    anchors: HashSet<&'static str>,
    /// 上一帧画出的命中的项（锚点与位置）：滚到第一个；测试据此核对
    drawn: Rc<RefCell<Vec<(&'static str, Bounds<Pixels>)>>>,
}

impl Hits {
    pub(super) fn new(items: &[&'static Item]) -> Self {
        Self {
            anchors: items.iter().map(|item| item.anchor).collect(),
            drawn: Rc::default(),
        }
    }

    /// 命中了 `anchor` 时给 `element` 加底色（向四周扩出一点，里面的内容不挪位置；上下扩得少，相邻的两项之间留出
    /// 空隙），并记下画在哪里。
    pub(crate) fn mark<E: Styled + ParentElement>(
        &self,
        element: E,
        anchor: &'static str,
        cx: &App,
    ) -> E {
        if !self.anchors.contains(anchor) {
            return element;
        }
        let theme = cx.theme();
        let drawn = self.drawn.clone();
        element
            .rounded(theme.radius)
            .bg(theme.warning.opacity(0.15))
            .mx(rems(-0.5))
            .px(rems(0.5))
            .my(rems(-0.125))
            .py(rems(0.125))
            .child(
                canvas(
                    move |bounds, _, _| drawn.borrow_mut().push((anchor, bounds)),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
    }

    /// 新的一帧开始画：忘掉上一帧画出的位置。
    pub(super) fn begin_frame(&self) {
        self.drawn.borrow_mut().clear();
    }

    /// 上一帧画出的命中的项从哪里到哪里：最上面一项的顶与最下面一项的底。
    pub(super) fn drawn_span(&self) -> Option<(Pixels, Pixels)> {
        let drawn = self.drawn.borrow();
        let top = drawn.iter().map(|(_, b)| b.top()).min_by(cmp)?;
        let bottom = drawn.iter().map(|(_, b)| b.bottom()).max_by(cmp)?;
        Some((top, bottom))
    }

    /// 上一帧画出的命中的项的锚点（测试用）。
    #[cfg(test)]
    pub(crate) fn drawn_anchors(&self) -> Vec<&'static str> {
        self.drawn
            .borrow()
            .iter()
            .map(|(anchor, _)| *anchor)
            .collect()
    }
}

fn cmp(a: &Pixels, b: &Pixels) -> Ordering {
    a.partial_cmp(b).unwrap_or(Ordering::Equal)
}

/// 各窗口中设置搜索的结果（一个窗口最多一个设置 Tab）。
#[derive(Default)]
struct Published(HashMap<WindowId, Rc<Hits>>);

impl Global for Published {}

/// 登记这个窗口中的搜索结果（设置每次渲染时；Tab 移到别的窗口后登记在新窗口上）。
pub(super) fn publish(hits: &Rc<Hits>, window: &Window, cx: &mut App) {
    let id = window.window_handle().window_id();
    let published = cx.default_global::<Published>();
    if !published.0.get(&id).is_some_and(|h| Rc::ptr_eq(h, hits)) {
        published.0.insert(id, hits.clone());
    }
}

/// 设置关了：撤销登记。
pub(super) fn withdraw(hits: &Rc<Hits>, cx: &mut App) {
    cx.default_global::<Published>()
        .0
        .retain(|_, h| !Rc::ptr_eq(h, hits));
}

/// 这个窗口中设置搜索的结果（没有在搜索时什么也不命中）：各页渲染时取。
pub(crate) fn hits(window: &Window, cx: &App) -> Rc<Hits> {
    cx.try_global::<Published>()
        .and_then(|p| p.0.get(&window.window_handle().window_id()).cloned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
