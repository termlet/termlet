//! 设置：左侧分类导航，右侧是该分类的内容；“保存”一次应用所有分类的修改，“取消”或 Esc 放弃。
//!
//! 点击对话框外部不关闭（避免丢失修改）。保存时只写入与打开时不同的设置组：打开期间别处（云同步、另一个窗口）
//! 改了别的组不会被这里的旧值覆盖。
//!
//! 立即执行的操作（导入、清除、复制、安装等，与“保存”无关）的结果写在底部的提示行里，换分类时清掉：弹出的通知
//! 会被对话框盖住（见 [`crate::notify`]）。

mod fonts;
mod highlight;
mod host_cas;
mod known_hosts;
mod local;
mod schemes;
mod shortcuts;
mod startup;
mod toolbar;

use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::form::{Field, field, v_form};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::radio::RadioGroup;
use gpui_kit::component::select::Select;
use gpui_kit::component::sidebar::{SidebarItem as _, SidebarMenuItem};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, StyledExt as _, WindowExt as _, h_flex,
    v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, PathPromptOptions, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, TestSupportExt as _, Window,
    div, prelude::FluentBuilder as _, px, rems,
};
use termlet_core::history;
use termlet_core::model::{
    CursorShape, ExternalLaunch, IconStyle, LanguageSetting, LocalSettings, MonitorBar,
    RemoteEditorSetting, RightClick, Settings, TabPosition, ThemeSetting,
};
use termlet_core::store::Store;

use crate::app_state::AppState;
use crate::choice::{Choice, ChoiceState, choice_state, chosen};
use crate::dialog_token::{DialogToken, close_own};
use crate::notify::{self, Notice, NoticeLine};
use crate::prose::prose;
use crate::{store_errors, t};

use highlight::HighlightPage;
use known_hosts::KnownHostsPage;
use local::LocalPage;
use schemes::SchemesPage;
use shortcuts::ShortcutsPage;
use startup::StartupSection;
use toolbar::ToolbarPage;

/// 设置的分类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    General,
    /// 标题栏的工具栏（标题栏按钮的右键菜单“自定义工具栏…”打开这一页）
    Toolbar,
    Terminal,
    Schemes,
    Local,
    Highlight,
    Connection,
    KnownHosts,
    Security,
    Cli,
    Backup,
    Sync,
    Shortcuts,
    About,
}

impl Page {
    const ALL: [Page; 14] = [
        Page::General,
        Page::Toolbar,
        Page::Terminal,
        Page::Schemes,
        Page::Local,
        Page::Highlight,
        Page::Connection,
        Page::KnownHosts,
        Page::Security,
        Page::Cli,
        Page::Backup,
        Page::Sync,
        Page::Shortcuts,
        Page::About,
    ];

    fn title(self) -> SharedString {
        match self {
            Page::General => t!("settings.nav.general"),
            Page::Toolbar => t!("toolbar.nav"),
            Page::Terminal => t!("settings.nav.terminal"),
            Page::Schemes => t!("schemes.nav"),
            Page::Local => t!("settings.nav.local"),
            Page::Highlight => t!("settings.nav.highlight"),
            Page::Connection => t!("settings.nav.connection"),
            Page::KnownHosts => t!("settings.nav.known_hosts"),
            Page::Security => crate::lock_ui::security_title(),
            Page::Cli => crate::cli_ui::page_title(),
            Page::Backup => t!("settings.nav.backup"),
            Page::Sync => t!("sync_ui.nav"),
            Page::Shortcuts => t!("settings.nav.shortcuts"),
            Page::About => t!("settings.nav.about"),
        }
    }

    fn icon(self) -> Icon {
        match self {
            Page::General => Icon::new(IconName::SlidersHorizontal),
            Page::Toolbar => Icon::new(IconName::PanelTop),
            Page::Terminal => Icon::new(IconName::SquareTerminal),
            Page::Schemes => Icon::new(IconName::Palette),
            Page::Local => Icon::new(IconName::Laptop),
            Page::Highlight => Icon::new(IconName::Highlighter),
            Page::Connection => Icon::new(IconName::Network),
            Page::KnownHosts => Icon::new(IconName::FingerprintPattern),
            Page::Security => Icon::new(IconName::ShieldCheck),
            Page::Cli => Icon::new(IconName::Bot),
            Page::Backup => Icon::new(IconName::DatabaseBackup),
            Page::Sync => Icon::new(IconName::Cloud),
            Page::Shortcuts => Icon::new(IconName::Keyboard),
            Page::About => Icon::new(IconName::Info),
        }
    }

    /// 导航项的 ID（按分类命名，加入新的分类不影响别的）。
    fn nav_id(self) -> SharedString {
        match self {
            Page::General => "settings-nav-general".into(),
            Page::Toolbar => "settings-nav-toolbar".into(),
            Page::Terminal => "settings-nav-terminal".into(),
            Page::Schemes => "settings-nav-schemes".into(),
            Page::Local => "settings-nav-local".into(),
            Page::Highlight => "settings-nav-highlight".into(),
            Page::Connection => "settings-nav-connection".into(),
            Page::KnownHosts => "settings-nav-known-hosts".into(),
            Page::Security => "settings-nav-security".into(),
            Page::Cli => "settings-nav-cli".into(),
            Page::Backup => "settings-nav-backup".into(),
            Page::Sync => "settings-nav-sync".into(),
            Page::Shortcuts => "settings-nav-shortcuts".into(),
            Page::About => "settings-nav-about".into(),
        }
    }
}

/// 字号、历史行数、保活间隔的范围。
const FONT_SIZE: (u16, u16) = (8, 32);
const SCROLLBACK: (u32, u32) = (1000, 100_000);
const KEEPALIVE: (u32, u32) = (0, 600);

/// 让 shell 输出 OSC 133 提示符标记（命令历史据此准确记录）与 OSC 7 当前目录的配置片段，由用户自己加到服务器上。
/// bash 需要 4.4 起才有 PS0；fish 4 起默认输出，不需要配置。
const SHELL_SNIPPETS: [(&str, &str, &str); 2] = [
    (
        "bash",
        "~/.bashrc",
        r#"PS1='\[\e]133;A\a\e]7;file://\h$PWD\a\]'"$PS1"'\[\e]133;B\a\]'
PS0=$'\e]133;C\a'"$PS0""#,
    ),
    (
        "zsh",
        "~/.zshrc",
        r#"autoload -Uz add-zsh-hook
termlet_prompt() { print -n "\e]133;A\a\e]7;file://$HOST$PWD\a" }
termlet_preexec() { print -n '\e]133;C\a' }
add-zsh-hook precmd termlet_prompt
add-zsh-hook preexec termlet_preexec
PS1="$PS1"$'%{\e]133;B\a%}'"#,
    ),
];

/// 打开设置，显示 `page` 分类。
pub fn open(page: Page, window: &mut Window, cx: &mut App) {
    tracing::debug!(page = ?page, "打开设置");
    let form = cx.new(|cx| SettingsForm::new(page, window, cx));
    let width = rems(48.).to_pixels(window.rem_size());
    let owner = form.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        // 备份、恢复进行中不能关闭（Esc、右上角的关闭按钮也不行）
        let blocked = form.read(cx).closing_blocked(cx);
        // 在输入框中按 Enter 由对话框转给 on_ok：与点“保存”相同（有错误时不关闭）
        let save = {
            let form = form.clone();
            move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                form.update(cx, |form, cx| form.save(window, cx))
            }
        };
        dialog
            .title(t!("settings.title"))
            .w(width)
            .overlay_closable(false)
            .close_button(!blocked)
            .keyboard(!blocked)
            .child(form.clone())
            .on_ok(save)
    });
    let token = DialogToken::take(window, cx);
    let notice = owner.update(cx, |form, _| {
        form.dialog = token;
        form.notice.clone()
    });
    notify::attach(&notice, window, cx);
}

pub(crate) struct SettingsForm {
    page: Page,
    /// 左侧分页导航的焦点（见 [`crate::page_nav`]）
    nav_focus: gpui_kit::FocusHandle,
    store: Arc<Store>,
    /// 打开时的设置：保存时只写入与它不同的组
    initial: Settings,
    initial_local: LocalSettings,
    /// 正在编辑的设置（复选框直接改这里，输入框与下拉框在保存时读出）
    draft: Settings,
    local: LocalSettings,
    language: Entity<ChoiceState<LanguageSetting>>,
    theme: Entity<ChoiceState<ThemeSetting>>,
    icons: Entity<ChoiceState<IconStyle>>,
    /// 界面大小（本机设置，见 [`crate::ui_scale::choices`]）
    ui_scale: Entity<ChoiceState<u16>>,
    /// 文件管理中“在编辑器中打开”显示哪些编辑器（本机设置）
    remote_editor: Entity<ChoiceState<RemoteEditorSetting>>,
    /// 启动时打开（本机设置）
    startup: Entity<StartupSection>,
    /// 标题栏的工具栏（本机设置）
    toolbar: Entity<ToolbarPage>,
    font_size: Entity<InputState>,
    scrollback: Entity<InputState>,
    font_family: Entity<ChoiceState<String>>,
    /// 本机的等宽字体；`None` 为还在读取
    installed_fonts: Option<Arc<Vec<String>>>,
    right_click: Entity<ChoiceState<RightClick>>,
    log_dir: Entity<InputState>,
    /// 会话录制的目录（本机设置）
    record_dir: Entity<InputState>,
    keepalive: Entity<InputState>,
    agent_path: Entity<InputState>,
    /// 本机的 X 显示（X11 转发，本机设置）
    x11_display: Entity<InputState>,
    /// 通过本机代理：本机的 HTTP 代理与服务器上的端口（本机设置）
    local_proxy: Entity<InputState>,
    local_proxy_port: Entity<InputState>,
    /// 打开设置时找到的系统代理（说明中显示）
    system_proxy: Option<String>,
    /// 这台电脑上确认过的代理命令有几条
    allowed_proxy_commands: usize,
    highlight: Entity<HighlightPage>,
    local_page: Entity<LocalPage>,
    shortcuts: Entity<ShortcutsPage>,
    known_hosts: Entity<KnownHostsPage>,
    security_page: Entity<crate::lock_ui::SecurityPage>,
    about_extras: Entity<crate::lock_ui::AboutExtras>,
    cli_page: Entity<crate::cli_ui::CliPage>,
    backup_page: Entity<crate::backup_ui::BackupPage>,
    sync_page: Entity<crate::sync_ui::SyncPage>,
    schemes: Entity<SchemesPage>,
    shell_snippets_open: bool,
    error: Option<SharedString>,
    /// 底部的提示行：立即执行的操作（导入、清除、复制等）的结果写在这里（见 [`crate::notify::show`]），换分类时清掉
    notice: Entity<NoticeLine>,
    /// 设置所在的对话框（恢复备份完成后只关它）
    dialog: Option<DialogToken>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsForm {
    fn new(page: Page, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = AppState::store(cx);
        let settings = store.settings();
        let local_settings = store.local_settings();
        let input = |value: String,
                     placeholder: SharedString,
                     window: &mut Window,
                     cx: &mut Context<Self>| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        let terminal = &settings.terminal;
        let language = choice_state(
            vec![
                Choice::new(t!("settings.language.system"), LanguageSetting::System),
                Choice::new("简体中文", LanguageSetting::ZhCn),
                Choice::new("繁體中文", LanguageSetting::ZhTw),
                Choice::new("English", LanguageSetting::En),
                Choice::new("日本語", LanguageSetting::Ja),
                Choice::new("한국어", LanguageSetting::Ko),
            ],
            &settings.appearance.language,
            false,
            window,
            cx,
        );
        let theme = choice_state(
            vec![
                Choice::new(t!("settings.theme.light"), ThemeSetting::Light),
                Choice::new(t!("settings.theme.dark"), ThemeSetting::Dark),
                Choice::new(t!("settings.theme.system"), ThemeSetting::System),
            ],
            &settings.appearance.theme,
            false,
            window,
            cx,
        );
        let icons = choice_state(
            vec![
                Choice::new(t!("settings.icons.color"), IconStyle::Color),
                Choice::new(t!("settings.icons.mono"), IconStyle::Mono),
            ],
            &settings.appearance.icons,
            false,
            window,
            cx,
        );
        let (scale_choices, scale) = crate::ui_scale::choices(
            cx.platform_info().os,
            crate::ui_scale::system_percent(cx),
            local_settings.window.ui_scale,
        );
        let ui_scale = choice_state(scale_choices, &scale, false, window, cx);
        let remote_editor = choice_state(
            vec![
                Choice::new(t!("settings.remote_editor.auto"), RemoteEditorSetting::Auto),
                Choice::new(
                    t!("settings.remote_editor.vscode"),
                    RemoteEditorSetting::VsCode,
                ),
                Choice::new(t!("settings.remote_editor.zed"), RemoteEditorSetting::Zed),
                Choice::new(t!("settings.remote_editor.off"), RemoteEditorSetting::Off),
            ],
            &local_settings.window.remote_editor,
            false,
            window,
            cx,
        );
        let startup = cx.new(|cx| StartupSection::new(&local_settings, window, cx));
        let toolbar = cx.new(|_| ToolbarPage::new(&local_settings.window.toolbar));
        let font_size = input(
            terminal.font_size.to_string(),
            SharedString::default(),
            window,
            cx,
        );
        let scrollback = input(
            terminal.scrollback.to_string(),
            SharedString::default(),
            window,
            cx,
        );
        let font_family = choice_state(
            font_choices(None, &terminal.font_family),
            &terminal.font_family,
            true,
            window,
            cx,
        );
        let right_click = choice_state(
            vec![
                Choice::new(t!("settings.right_click.auto"), RightClick::Auto),
                Choice::new(t!("settings.right_click.paste"), RightClick::Paste),
                Choice::new(t!("settings.right_click.menu"), RightClick::Menu),
            ],
            &terminal.right_click,
            false,
            window,
            cx,
        );
        let log_dir = input(
            local_settings.logs.session_log_dir.clone(),
            t!("settings.session_log_dir_default"),
            window,
            cx,
        );
        let record_dir = input(
            local_settings.logs.recording_dir.clone(),
            t!("settings.record_dir_default"),
            window,
            cx,
        );
        let keepalive = input(
            settings.connection.keepalive_seconds.to_string(),
            SharedString::default(),
            window,
            cx,
        );
        let agent_path = input(
            settings.connection.agent_path.clone(),
            t!("settings.agent_path_placeholder"),
            window,
            cx,
        );
        let x11_display = input(
            local_settings.connection.x11_display.clone(),
            t!(
                "settings.x11_display_placeholder",
                display = termlet_core::ssh::X11Options::local_display("")
            ),
            window,
            cx,
        );
        let local_proxy = input(
            local_settings.connection.local_proxy.clone(),
            t!("settings.local_proxy_placeholder"),
            window,
            cx,
        );
        let local_proxy_port = input(
            match local_settings.connection.local_proxy_port {
                0 => String::new(),
                port => port.to_string(),
            },
            t!("settings.local_proxy_port_placeholder"),
            window,
            cx,
        );
        let system_proxy = termlet_core::forward::system_proxy().map(|(a, _)| a.display());
        let allowed_proxy_commands = termlet_core::ssh::proxy_command::allowed_count(&store);
        let highlight = cx.new(|cx| HighlightPage::new(&settings.highlight, window, cx));
        let local_page = cx.new(|cx| LocalPage::new(&local_settings.shells, window, cx));
        let shortcuts = cx.new(|_| ShortcutsPage::new(&settings.keybindings, terminal.right_click));
        let known_hosts = cx.new(|cx| KnownHostsPage::new(window, cx));
        let security_page =
            cx.new(|cx| crate::lock_ui::SecurityPage::new(&local_settings, window, cx));
        let about_extras =
            cx.new(|cx| crate::lock_ui::AboutExtras::new(&settings, &local_settings, window, cx));
        let cli_page =
            cx.new(|cx| crate::cli_ui::CliPage::new(&settings, &local_settings, window, cx));
        let backup_page =
            cx.new(|cx| crate::backup_ui::BackupPage::new(&local_settings, window, cx));
        // 恢复替换了全部数据（包括设置）：对话框里的草稿已经过时，关闭对话框并说明结果
        let backup_restored = cx.subscribe_in(
            &backup_page,
            window,
            |this, _, event: &crate::backup_ui::BackupPageEvent, window, cx| match event {
                crate::backup_ui::BackupPageEvent::Restored { message, .. } => {
                    close_own(this.dialog.as_ref(), window, cx);
                    window.push_notification(Notification::success(message.clone()), cx);
                }
            },
        );
        let sync_page = cx.new(|cx| crate::sync_ui::SyncPage::new(window, cx));
        let schemes = cx.new(|cx| SchemesPage::new(&settings, window, cx));
        let subscriptions = vec![
            // 字号改了：预览跟着变
            cx.observe(&font_size, |_, _, cx| cx.notify()),
            cx.observe(&font_family, |_, _, cx| cx.notify()),
            // “终端”分页显示当前的配色
            cx.observe(&schemes, |_, _, cx| cx.notify()),
            backup_restored,
        ];
        let mut this = Self {
            page,
            nav_focus: cx.focus_handle().tab_stop(true),
            store,
            initial: settings.clone(),
            initial_local: local_settings.clone(),
            draft: settings,
            local: local_settings,
            language,
            theme,
            icons,
            ui_scale,
            remote_editor,
            startup,
            toolbar,
            font_size,
            scrollback,
            font_family,
            installed_fonts: None,
            right_click,
            log_dir,
            record_dir,
            keepalive,
            agent_path,
            x11_display,
            local_proxy,
            local_proxy_port,
            system_proxy,
            allowed_proxy_commands,
            highlight,
            local_page,
            shortcuts,
            known_hosts,
            security_page,
            about_extras,
            cli_page,
            backup_page,
            sync_page,
            schemes,
            shell_snippets_open: false,
            error: None,
            notice: NoticeLine::new(cx),
            dialog: None,
            _tasks: Vec::new(),
            _subscriptions: subscriptions,
        };
        this.load_fonts(window, cx);
        this.set_page(page, cx);
        this
    }

    /// 读取本机的等宽字体，读完后更新字体的下拉框。
    fn load_fonts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fonts = fonts::monospace_fonts(cx);
        let task = cx.spawn_in(window, async move |this, cx| {
            let fonts = fonts.await;
            let _ = this.update_in(cx, |this, window, cx| {
                let current = chosen(&this.font_family, cx).unwrap_or_default();
                let choices = font_choices(Some(&fonts), &current);
                this.font_family.update(cx, |state, cx| {
                    state.set_items(
                        gpui_kit::component::searchable_list::SearchableVec::new(choices),
                        window,
                        cx,
                    );
                    state.set_selected_value(&current, window, cx);
                });
                this.installed_fonts = Some(fonts);
                cx.notify();
            });
        });
        self._tasks.push(task);
    }

    fn set_page(&mut self, page: Page, cx: &mut Context<Self>) {
        if page != Page::Shortcuts {
            // 离开快捷键页：结束录入（否则在别的页上按的键会被录成快捷键）
            self.shortcuts.update(cx, |p, cx| p.stop_recording(cx));
        }
        if page != self.page {
            self.notice.update(cx, |line, cx| line.clear(cx));
        }
        self.page = page;
        if page == Page::Highlight {
            // 预览用“终端配色”分类中（尚未保存的）当前深浅的配色
            let scheme = self
                .schemes
                .read(cx)
                .slot_entry(cx.theme().is_dark())
                .scheme;
            self.highlight.update(cx, |p, cx| p.set_scheme(scheme, cx));
        }
        if page == Page::Shortcuts {
            // 固定快捷键中“右键”的说明跟随“终端”分类中（尚未保存）的选择
            let right_click = chosen(&self.right_click, cx).unwrap_or_default();
            self.shortcuts.update(cx, |p, cx| {
                p.right_click = right_click;
                cx.notify();
            });
        }
        cx.notify();
    }

    /// 备份页的操作（备份、恢复）进行中：不能保存、不能关闭。恢复会替换全部数据（含设置），
    /// 这时保存会用过时的草稿覆盖恢复出的设置；关掉对话框则看不到结果。
    fn closing_blocked(&self, cx: &App) -> bool {
        self.backup_page.read(cx).is_busy()
    }

    /// 校验并保存；有错误时停在出错的分类并显示原因，返回 false。
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.closing_blocked(cx) {
            return false;
        }
        self.error = None;
        self.notice.update(cx, |line, cx| line.clear(cx));
        // 先校验各分类，有错误时什么也不写
        if let Err((page, error)) = self.collect(cx) {
            self.page = page;
            self.error = Some(error);
            cx.notify();
            return false;
        }
        // 自定义配色的增、删、改先写进存储：新建的配色这时才有 ID，配色槽要用它（下面的 collect 读到的是换过的）
        let store = self.store.clone();
        if let Err(e) = self
            .schemes
            .update(cx, |page, cx| page.commit(&store, window, cx))
        {
            tracing::error!("保存终端配色失败：{e}");
            self.page = Page::Schemes;
            self.error = Some(t!(
                "schemes.save_failed",
                error = store_errors::describe(&e)
            ));
            cx.notify();
            return false;
        }
        match self.collect(cx) {
            Ok((draft, local)) => {
                let initial = &self.initial;
                let initial_local = &self.initial_local;
                // 只记改了哪些组（值里可能有不想写进日志的内容，例如代理地址、规则）
                let mut groups: Vec<&str> = Vec::new();
                macro_rules! note {
                    ($new:ident, $old:ident; $($section:ident),*) => {$(
                        if $new.$section != $old.$section {
                            groups.push(stringify!($section));
                        }
                    )*};
                }
                note!(draft, initial; general, appearance, terminal, highlight, connection, guard, keybindings);
                note!(local, initial_local; cli, app_lock, backup, shells, logs, usage, window, connection);
                tracing::info!(?groups, "保存设置");
                let synced = self.store.update_settings(|s| {
                    macro_rules! changed {
                        ($($section:ident),*) => {$(
                            if draft.$section != initial.$section {
                                s.$section = draft.$section.clone();
                            }
                        )*};
                    }
                    changed!(
                        general,
                        appearance,
                        terminal,
                        highlight,
                        connection,
                        guard,
                        keybindings
                    );
                });
                let local_result = synced.and_then(|()| {
                    self.store.update_local_settings(|s| {
                        macro_rules! changed {
                            ($($section:ident),*) => {$(
                                if local.$section != initial_local.$section {
                                    s.$section = local.$section.clone();
                                }
                            )*};
                        }
                        changed!(cli, app_lock, backup, shells, logs, usage, window);
                        // 本机的连接设置中只有 X 显示与通过本机代理的两项在这里改：确认过的代理命令可能在设置打开期间增加，
                        // 不用旧值覆盖
                        if local.connection.x11_display != initial_local.connection.x11_display {
                            s.connection.x11_display = local.connection.x11_display.clone();
                        }
                        if local.connection.local_proxy != initial_local.connection.local_proxy {
                            s.connection.local_proxy = local.connection.local_proxy.clone();
                        }
                        if local.connection.local_proxy_port
                            != initial_local.connection.local_proxy_port
                        {
                            s.connection.local_proxy_port = local.connection.local_proxy_port;
                        }
                    })
                });
                match local_result {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::error!("保存设置失败：{e}");
                        self.error = Some(t!(
                            "settings.save_failed",
                            error = store_errors::describe(&e)
                        ));
                        cx.notify();
                        false
                    }
                }
            }
            Err((page, error)) => {
                self.page = page;
                self.error = Some(error);
                cx.notify();
                false
            }
        }
    }

    /// 把界面上的值合进草稿；不合法时返回出错的分类与原因。
    fn collect(&self, cx: &App) -> Result<(Settings, LocalSettings), (Page, SharedString)> {
        let mut draft = self.draft.clone();
        let mut local = self.local.clone();
        let number = |state: &Entity<InputState>| state.read(cx).value().trim().parse::<u32>().ok();

        let font_size = number(&self.font_size)
            .filter(|n| (FONT_SIZE.0 as u32..=FONT_SIZE.1 as u32).contains(n))
            .ok_or_else(|| {
                (
                    Page::Terminal,
                    t!(
                        "settings.error.font_size",
                        min = FONT_SIZE.0,
                        max = FONT_SIZE.1
                    ),
                )
            })?;
        let scrollback = number(&self.scrollback)
            .filter(|n| (SCROLLBACK.0..=SCROLLBACK.1).contains(n))
            .ok_or_else(|| {
                (
                    Page::Terminal,
                    t!(
                        "settings.error.scrollback",
                        min = SCROLLBACK.0,
                        max = SCROLLBACK.1
                    ),
                )
            })?;
        let keepalive = number(&self.keepalive)
            .filter(|n| (KEEPALIVE.0..=KEEPALIVE.1).contains(n))
            .ok_or_else(|| {
                (
                    Page::Connection,
                    t!(
                        "settings.error.keepalive",
                        min = KEEPALIVE.0,
                        max = KEEPALIVE.1
                    ),
                )
            })?;
        self.highlight
            .read(cx)
            .apply(&mut draft.highlight, cx)
            .map_err(|e| (Page::Highlight, e))?;
        self.local_page.read(cx).apply(&mut local.shells);
        self.security_page.read(cx).apply(&mut local);
        self.about_extras.read(cx).apply(&mut draft, &mut local, cx);
        let cli_page = self.cli_page.read(cx);
        cli_page.validate(cx).map_err(|e| (Page::Cli, e))?;
        cli_page.apply(&mut draft, &mut local);
        let backup_page = self.backup_page.read(cx);
        backup_page.validate(cx).map_err(|e| (Page::Backup, e))?;
        backup_page.apply(&mut local, cx);
        self.shortcuts.read(cx).apply(&mut draft.keybindings);
        self.startup.read(cx).apply(&mut local);
        self.toolbar.read(cx).apply(&mut local);
        self.schemes.read(cx).apply(&mut draft.terminal);

        if let Some(language) = chosen(&self.language, cx) {
            draft.appearance.language = language;
        }
        if let Some(theme) = chosen(&self.theme, cx) {
            draft.appearance.theme = theme;
        }
        if let Some(icons) = chosen(&self.icons, cx) {
            draft.appearance.icons = icons;
        }
        if let Some(scale) = chosen(&self.ui_scale, cx) {
            local.window.ui_scale = scale;
        }
        if let Some(editor) = chosen(&self.remote_editor, cx) {
            local.window.remote_editor = editor;
        }
        let terminal = &mut draft.terminal;
        terminal.font_size = font_size as u16;
        terminal.scrollback = scrollback;
        terminal.font_family = chosen(&self.font_family, cx).unwrap_or_default();
        if let Some(right_click) = chosen(&self.right_click, cx) {
            terminal.right_click = right_click;
        }
        draft.connection.keepalive_seconds = keepalive;
        draft.connection.agent_path = self.agent_path.read(cx).value().trim().to_string();
        let x11_display = self.x11_display.read(cx).value().trim().to_string();
        if !x11_display.is_empty() && termlet_core::ssh::x11::Display::parse(&x11_display).is_none()
        {
            return Err((
                Page::Connection,
                t!("settings.error.x11_display", display = x11_display),
            ));
        }
        local.connection.x11_display = x11_display;
        let local_proxy = self.local_proxy.read(cx).value().trim().to_string();
        if !local_proxy.is_empty()
            && termlet_core::forward::ProxyAddress::parse(&local_proxy).is_none()
        {
            return Err((
                Page::Connection,
                t!("settings.error.local_proxy", address = local_proxy),
            ));
        }
        local.connection.local_proxy = local_proxy;
        let port = self.local_proxy_port.read(cx).value().trim().to_string();
        local.connection.local_proxy_port = if port.is_empty() {
            0
        } else {
            match port.parse::<u16>() {
                Ok(port) => port,
                Err(_) => {
                    return Err((
                        Page::Connection,
                        t!("settings.error.local_proxy_port", port = port),
                    ));
                }
            }
        };
        local.logs.session_log_dir = self.log_dir.read(cx).value().trim().to_string();
        local.logs.recording_dir = self.record_dir.read(cx).value().trim().to_string();
        Ok((draft, local))
    }

    /// 选一个目录填进 `target`（会话日志、录制的目录）。
    fn pick_dir(
        &mut self,
        target: fn(&Self) -> &Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(dir) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                target(this).clone().update(cx, |state, cx| {
                    state.set_value(dir.to_string_lossy().to_string(), window, cx)
                });
            });
        });
        self._tasks.push(task);
    }

    /// 忘掉这台电脑上确认过的代理命令（立即执行，与“保存”无关）：之后每条命令第一次执行前重新询问。
    fn forget_proxy_commands(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match termlet_core::ssh::proxy_command::forget_all(&self.store) {
            Ok(()) => {
                tracing::info!(
                    count = self.allowed_proxy_commands,
                    "已清除确认过的代理命令"
                );
                self.allowed_proxy_commands = 0;
                notify::show(
                    Notice::success(t!("settings.proxy_commands_forgotten")),
                    window,
                    cx,
                );
            }
            Err(e) => notify::show(Notice::error(store_errors::describe(&e)), window, cx),
        }
        cx.notify();
    }

    /// 确认后清空所有主机的命令历史（立即执行，与“保存”无关）。
    fn confirm_clear_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        crate::ui_scale::open_alert(window, cx, move |alert: AlertDialog, _, _| {
            let store = store.clone();
            alert
                .confirm()
                .title(t!("settings.clear_history_confirm"))
                .ok_text(t!("settings.clear_history_ok"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("common.cancel"))
                .on_ok(move |_, window, cx| {
                    match history::clear(&store, None) {
                        Ok(()) => {
                            tracing::info!("已清空全部命令历史");
                            notify::show(
                                Notice::success(t!("settings.history_cleared")),
                                window,
                                cx,
                            );
                        }
                        Err(e) => {
                            notify::show(Notice::error(store_errors::describe(&e)), window, cx)
                        }
                    }
                    true
                })
        });
    }

    // ------------------------------------------------------------ 渲染

    fn render_nav(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 逐项渲染（不经 SidebarMenu）：导航项的 ID 见 Page::nav_id
        let items = Page::ALL.map(|page| {
            let this = cx.entity().downgrade();
            SidebarMenuItem::new(page.title())
                .icon(page.icon())
                .active(self.page == page)
                .on_click(move |_, _, cx| {
                    let _ = this.update(cx, |this, cx| this.set_page(page, cx));
                })
                .render(page.nav_id(), window, cx)
                .into_any_element()
        });
        let current = Page::ALL.iter().position(|p| *p == self.page).unwrap_or(0);
        crate::page_nav::column(cx).child(
            crate::page_nav::frame("settings-nav", &self.nav_focus, window, cx)
                .on_key_down(
                    cx.listener(move |this, event: &gpui_kit::KeyDownEvent, _, cx| {
                        if let Some(ix) = crate::page_nav::step(event, current, Page::ALL.len()) {
                            this.set_page(Page::ALL[ix], cx);
                            cx.stop_propagation();
                        }
                    }),
                )
                .child(v_flex().gap_1().children(items)),
        )
    }

    /// 带说明的复选框，勾选状态存在 `slot` 指向的设置项中。
    fn checkbox(
        &mut self,
        id: &'static str,
        label: SharedString,
        hint: Option<SharedString>,
        slot: fn(&mut Self) -> &mut bool,
        cx: &Context<Self>,
    ) -> Field {
        let checked = *slot(self);
        field().child(
            v_flex()
                .gap_1()
                .child(
                    Checkbox::new(id)
                        .label(label)
                        .checked(checked)
                        .on_change(cx.listener(move |this, checked: &bool, _, cx| {
                            *slot(this) = *checked;
                            cx.notify();
                        })),
                )
                .when_some(hint, |this, hint| {
                    this.child(
                        div()
                            .pl_6()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(prose(hint)),
                    )
                }),
        )
    }

    fn render_general(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let os = cx.platform_info().os;
        let scale_hint = if crate::ui_scale::follows_system(os) {
            t!("settings.ui_scale_hint_windows")
        } else {
            t!("settings.ui_scale_hint")
        };
        let taskbar_texts = match os {
            gpui_kit::OperatingSystem::Windows => Some((
                t!("settings.taskbar_progress"),
                t!("settings.taskbar_progress_hint"),
            )),
            gpui_kit::OperatingSystem::Mac => Some((
                t!("settings.taskbar_progress_mac"),
                t!("settings.taskbar_progress_mac_hint"),
            )),
            gpui_kit::OperatingSystem::Linux => Some((
                t!("settings.taskbar_progress_linux"),
                t!("settings.taskbar_progress_linux_hint"),
            )),
            _ => None,
        };
        let installed = crate::sftp::open_in_editor::installed(cx);
        let editors_found = if installed.is_empty() {
            t!("settings.remote_editor_none")
        } else {
            let names: Vec<&str> = installed.iter().map(|e| e.name()).collect();
            t!(
                "settings.remote_editor_found",
                editors = names.join(&t!("sftp.list_separator"))
            )
        };
        v_form()
            .child(
                // 语言、主题与图标一行；说明写在这一行下面，讲图标管哪些地方
                field().description(prose(t!("settings.icons_hint"))).child(
                    h_flex()
                        .gap_3()
                        .child(
                            div().flex_1().child(
                                field()
                                    .label(t!("settings.language"))
                                    .child(Select::new(&self.language).id("settings-language")),
                            ),
                        )
                        .child(
                            div().flex_1().child(
                                field()
                                    .label(t!("settings.app_theme"))
                                    .child(Select::new(&self.theme).id("settings-theme")),
                            ),
                        )
                        .child(
                            div().flex_1().child(
                                field()
                                    .label(t!("settings.icons"))
                                    .child(Select::new(&self.icons).id("settings-icons")),
                            ),
                        ),
                ),
            )
            .child(
                field()
                    .label(t!("settings.ui_scale"))
                    .description(prose(scale_hint))
                    .child(Select::new(&self.ui_scale).id("settings-ui-scale")),
            )
            .child(self.checkbox(
                "settings-confirm-close",
                t!("settings.confirm_close"),
                Some(t!("settings.confirm_close_hint")),
                |this| &mut this.draft.general.confirm_close,
                cx,
            ))
            .child(self.checkbox(
                "settings-show-host-address",
                t!("settings.show_host_address"),
                Some(t!("settings.show_host_address_hint")),
                |this| &mut this.draft.general.show_host_address,
                cx,
            ))
            // 只在 Windows 上有（本机设置）：macOS 关闭最后一个窗口后本来就留在程序坞
            .when(crate::tray::supported(cx.platform_info().os), |el| {
                el.child(self.checkbox(
                    "settings-close-to-tray",
                    t!("settings.close_to_tray"),
                    Some(t!("settings.close_to_tray_hint")),
                    |this| &mut this.local.window.close_to_tray,
                    cx,
                ))
            })
            // 任务栏按钮（Windows）、程序坞图标（macOS）上的传输进度（本机设置）；Linux 没有
            .when_some(taskbar_texts, |el, (label, hint)| {
                el.child(self.checkbox(
                    "settings-taskbar-progress",
                    label,
                    Some(hint),
                    |this| &mut this.local.window.taskbar_progress,
                    cx,
                ))
            })
            // Tab 的位置与竖排时按主机分组（本机设置）
            .child(
                field()
                    .label(t!("settings.tab_position"))
                    .description(prose(t!("settings.tab_position_hint")))
                    .child(self.tab_position_radio(cx)),
            )
            .child(self.checkbox(
                "settings-group-tabs-by-host",
                t!("settings.group_tabs_by_host"),
                Some(t!("settings.group_tabs_by_host_hint")),
                |this| &mut this.local.window.group_tabs_by_host,
                cx,
            ))
            .child(
                field()
                    .label(t!("settings.remote_editor"))
                    .description_fn(move |_, _| {
                        v_flex()
                            .gap_1()
                            .child(prose(t!("settings.remote_editor_hint")))
                            .child(prose(editors_found.clone()))
                    })
                    .child(Select::new(&self.remote_editor).id("settings-remote-editor")),
            )
            .child(
                field()
                    .label(t!("startup_open.label"))
                    .description(prose(t!("startup_open.hint")))
                    .child(self.startup.clone()),
            )
            .into_any_element()
    }

    fn render_terminal(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let family = chosen(&self.font_family, cx).unwrap_or_default();
        let preview_family: SharedString = if family.is_empty() {
            termlet_terminal::fonts::DEFAULT_FAMILY.into()
        } else {
            family.clone().into()
        };
        let preview_size = self
            .font_size
            .read(cx)
            .value()
            .trim()
            .parse::<f32>()
            .ok()
            .map(|n| n.clamp(FONT_SIZE.0 as f32, FONT_SIZE.1 as f32))
            .unwrap_or(14.);
        // 没装的推荐字体给出下载地址
        let missing: Vec<&fonts::Recommended> = match &self.installed_fonts {
            Some(installed) => fonts::RECOMMENDED
                .iter()
                .filter(|r| !installed.iter().any(|f| f == r.family))
                .collect(),
            None => Vec::new(),
        };
        let font_hint = if self.installed_fonts.is_none() {
            t!("settings.font.loading")
        } else {
            t!("settings.font.hint")
        };
        // 配色在“终端配色”中设置，这里显示界面当前深浅所用的（草稿中的），单击跳过去
        let scheme = self.schemes.read(cx).slot_entry(theme.is_dark());
        v_form()
            .child(
                field()
                    .label(t!("schemes.terminal_field"))
                    .description(prose(t!("schemes.terminal_field_hint")))
                    .child(
                        schemes::slot_button(&scheme).on_click(
                            cx.listener(|this, _, _, cx| this.set_page(Page::Schemes, cx)),
                        ),
                    ),
            )
            .child(
                field().child(
                    h_flex()
                        .gap_3()
                        .child(
                            div().flex_1().child(
                                field()
                                    .label(t!("settings.font_size"))
                                    .child(Input::new(&self.font_size).id("settings-font-size")),
                            ),
                        )
                        .child(
                            div().flex_1().child(
                                field()
                                    .label(t!("settings.scrollback"))
                                    .child(Input::new(&self.scrollback).id("settings-scrollback")),
                            ),
                        ),
                ),
            )
            .child(
                field()
                    .label(t!("settings.font.label"))
                    .description(prose(font_hint))
                    .child(Select::new(&self.font_family).id("settings-font-family")),
            )
            .when(!missing.is_empty(), |this| {
                this.child(
                    field().child(
                        h_flex()
                            .flex_wrap()
                            .gap_x_2()
                            .text_xs()
                            .text_color(muted)
                            .child(prose(t!("settings.font.more")))
                            .children(missing.into_iter().map(|r| {
                                let url = r.url;
                                Button::new(SharedString::from(format!("font-link-{}", r.family)))
                                    .link()
                                    .xsmall()
                                    .label(t!(
                                        "choice.name_with_detail",
                                        name = r.family,
                                        detail = r.note()
                                    ))
                                    .on_click(move |_, _, cx| cx.open_url(url))
                            })),
                    ),
                )
            })
            .child(
                field().child(
                    div()
                        .id("settings-font-preview")
                        .px_3()
                        .py_2()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.background)
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .font_family(preview_family)
                        // 与终端一致：不显示连字（=> 是两个字符）
                        .font_features(gpui_kit::FontFeatures::disable_ligatures())
                        .text_size(px(preview_size))
                        .child("$ ls -la ~/app && echo 0O 1lI {}[] => 中文 ✓"),
                ),
            )
            .child(self.checkbox(
                "settings-copy-on-select",
                t!("settings.copy_on_select"),
                None,
                |this| &mut this.draft.terminal.copy_on_select,
                cx,
            ))
            .child(
                field()
                    .label(t!("settings.right_click.label"))
                    .child(Select::new(&self.right_click).id("settings-right-click")),
            )
            // 只在 macOS 上有：其他系统上 Alt 本来就作为 Meta
            .when(
                cx.platform_info().os == gpui_kit::OperatingSystem::Mac,
                |el| {
                    el.child(self.checkbox(
                        "settings-option-as-meta",
                        t!("settings.option_as_meta"),
                        Some(t!("settings.option_as_meta_hint")),
                        |this| &mut this.draft.terminal.option_as_meta,
                        cx,
                    ))
                },
            )
            .child(
                field()
                    .label(t!("settings.cursor.label"))
                    .description(prose(t!("settings.cursor.hint")))
                    .child(self.cursor_shape_radio(cx)),
            )
            .child(self.checkbox(
                "settings-cursor-blink",
                t!("settings.cursor_blink"),
                None,
                |this| &mut this.draft.terminal.cursor_blink,
                cx,
            ))
            .child(self.checkbox(
                "settings-monitor",
                t!("settings.monitor"),
                Some(t!("settings.monitor_hint")),
                |this| &mut this.draft.terminal.monitor,
                cx,
            ))
            .child(self.checkbox(
                "settings-latency",
                t!("settings.latency"),
                None,
                |this| &mut this.draft.terminal.latency,
                cx,
            ))
            .child(
                field()
                    .label(t!("settings.monitor_bar.label"))
                    .description(prose(t!("settings.monitor_bar.hint")))
                    .child(self.monitor_bar_radio(cx)),
            )
            .child(self.checkbox(
                "settings-paste-warning",
                t!("settings.paste_warning"),
                Some(t!("settings.paste_warning_hint")),
                |this| &mut this.draft.terminal.paste_warning,
                cx,
            ))
            .child(self.checkbox(
                "settings-tab-activity",
                t!("settings.tab_activity"),
                Some(t!("settings.tab_activity_hint")),
                |this| &mut this.draft.terminal.tab_activity,
                cx,
            ))
            .child(self.checkbox(
                "settings-notify-done",
                t!("settings.notify_command_done"),
                Some(t!("settings.notify_command_done_hint")),
                |this| &mut this.draft.terminal.notify_command_done,
                cx,
            ))
            .child(self.checkbox(
                "settings-file-transfer",
                t!("settings.file_transfer"),
                Some(t!("settings.file_transfer_hint")),
                |this| &mut this.draft.terminal.file_transfer,
                cx,
            ))
            .child(self.checkbox(
                "settings-transfer-auto-clear",
                t!("settings.transfer_auto_clear"),
                Some(t!("settings.transfer_auto_clear_hint")),
                |this| &mut this.draft.terminal.transfer_auto_clear,
                cx,
            ))
            .child(self.checkbox(
                "settings-password-fill",
                t!("settings.password_fill"),
                Some(t!("settings.password_fill_hint")),
                |this| &mut this.draft.terminal.password_fill,
                cx,
            ))
            .child(self.checkbox(
                "settings-session-log",
                t!("settings.session_log"),
                Some(t!("settings.session_log_hint")),
                |this| &mut this.draft.terminal.session_log,
                cx,
            ))
            .child(
                field()
                    .label(t!("settings.session_log_dir"))
                    .description(prose(t!("settings.session_log_dir_hint")))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .child(Input::new(&self.log_dir).id("settings-log-dir")),
                            )
                            .child(
                                Button::new("settings-log-dir-browse")
                                    .outline()
                                    .label(t!("settings.browse"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.pick_dir(|this| &this.log_dir, window, cx)
                                    })),
                            ),
                    ),
            )
            .child(
                self.checkbox(
                    "settings-record-input",
                    t!("settings.record_input"),
                    Some(t!("settings.record_input_hint")),
                    |this| &mut this.draft.terminal.record_input,
                    cx,
                )
                // 开启时说明风险：输入的密码也会写进文件
                .when(self.draft.terminal.record_input, |this| {
                    this.child(
                        div()
                            .pl_6()
                            .text_xs()
                            .text_color(cx.theme().warning)
                            .child(prose(t!("settings.record_input_warning"))),
                    )
                }),
            )
            .child(
                field()
                    .label(t!("settings.record_dir"))
                    .description(prose(t!("settings.record_dir_hint")))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .child(Input::new(&self.record_dir).id("settings-record-dir")),
                            )
                            .child(
                                Button::new("settings-record-dir-browse")
                                    .outline()
                                    .label(t!("settings.browse"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.pick_dir(|this| &this.record_dir, window, cx)
                                    })),
                            ),
                    ),
            )
            .child(self.checkbox(
                "settings-command-history",
                t!("settings.command_history"),
                Some(t!("settings.command_history_hint")),
                |this| &mut this.draft.terminal.command_history,
                cx,
            ))
            .child(
                field().child(
                    v_flex()
                        .pl_6()
                        .gap_2()
                        .items_start()
                        .child(
                            Button::new("settings-clear-history")
                                .outline()
                                .small()
                                .label(t!("settings.clear_history"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.confirm_clear_history(window, cx)
                                })),
                        )
                        .child(self.render_shell_snippets(cx)),
                ),
            )
            .child(self.command_suggest_field(cx))
            .into_any_element()
    }

    /// “按命令历史给出补全提示”：靠命令历史，命令历史关闭时不可用并说明原因；说明中的按键按当前系统写。
    fn command_suggest_field(&self, cx: &Context<Self>) -> Field {
        let history_on = self.draft.terminal.command_history;
        let os = cx.platform_info().os;
        // 与同一句里的 → 写法一致：Ctrl+Right 写作 Ctrl+→（macOS 本来就是 ⌥→）
        let word = termlet_core::keymap::label_of(
            termlet_terminal::suggest::accept_word_combo(os),
            crate::keymap::key_os(os),
        )
        .replace("Right", "→");
        let hint = |text: SharedString| {
            div()
                .pl_6()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(prose(text))
        };
        field().child(
            v_flex()
                .gap_1()
                .child(
                    Checkbox::new("settings-command-suggest")
                        .label(t!("settings.command_suggest"))
                        .checked(history_on && self.draft.terminal.command_suggest)
                        .disabled(!history_on)
                        .on_change(cx.listener(|this, checked: &bool, _, cx| {
                            this.draft.terminal.command_suggest = *checked;
                            cx.notify();
                        })),
                )
                .child(hint(t!("settings.command_suggest_hint", word = word)))
                .when(!history_on, |this| {
                    this.child(hint(t!("settings.command_suggest_needs_history")))
                }),
        )
    }

    /// “让命令历史更准确”：可展开的 shell 集成说明与配置片段。
    fn render_shell_snippets(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let open = self.shell_snippets_open;
        v_flex()
            .gap_2()
            .w_full()
            .text_xs()
            .child(
                Button::new("settings-shell-integration")
                    .ghost()
                    .xsmall()
                    .icon(if open {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .label(t!("settings.shell_integration"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.shell_snippets_open = !this.shell_snippets_open;
                        cx.notify();
                    })),
            )
            .when(open, |this| {
                this.child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(prose(t!("settings.shell_integration_hint"))),
                )
                .children(SHELL_SNIPPETS.iter().map(|&(shell, file, code)| {
                    v_flex()
                        .gap_1()
                        .child(
                            h_flex()
                                .justify_between()
                                .child(
                                    h_flex().gap_1().child(shell).child(
                                        div().text_color(theme.muted_foreground).child(file),
                                    ),
                                )
                                .child(
                                    Button::new(SharedString::from(format!(
                                        "copy-snippet-{shell}"
                                    )))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Copy)
                                    .label(t!("settings.copy_snippet"))
                                    .on_click(
                                        move |_, window, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                code.to_string(),
                                            ));
                                            notify::show(
                                                Notice::success(t!(
                                                    "settings.snippet_copied",
                                                    shell = shell
                                                )),
                                                window,
                                                cx,
                                            );
                                        },
                                    ),
                                ),
                        )
                        .child(
                            div()
                                .px_2()
                                .py_1()
                                .rounded(theme.radius)
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.muted)
                                .font_family(termlet_terminal::fonts::DEFAULT_FAMILY)
                                .whitespace_normal()
                                .children(code.lines().map(|line| div().child(line.to_string()))),
                        )
                }))
            })
    }

    fn render_connection(&mut self, cx: &mut Context<Self>) -> AnyElement {
        v_form()
            .child(
                field()
                    .label(t!("settings.keepalive"))
                    .description(prose(t!("settings.keepalive_hint")))
                    .child(
                        div()
                            .w(rems(10.))
                            .child(Input::new(&self.keepalive).id("settings-keepalive")),
                    ),
            )
            .child(self.checkbox(
                "settings-auto-reconnect",
                t!("settings.auto_reconnect"),
                None,
                |this| &mut this.draft.connection.auto_reconnect,
                cx,
            ))
            .child(self.checkbox(
                "settings-detect-os",
                t!("settings.detect_os"),
                Some(t!("settings.detect_os_hint")),
                |this| &mut this.draft.connection.detect_os,
                cx,
            ))
            .child(
                field()
                    .label(t!("settings.agent_path"))
                    .description(prose(t!("settings.agent_path_hint")))
                    .child(Input::new(&self.agent_path).id("settings-agent-path")),
            )
            .child(
                field()
                    .label(t!("settings.external_launch"))
                    .description(prose(t!("settings.external_launch_hint")))
                    .child(self.external_launch_radio(cx)),
            )
            .child(
                field()
                    .label(t!("settings.x11_display"))
                    .description(prose(t!("settings.x11_display_hint")))
                    .child(Input::new(&self.x11_display).id("settings-x11-display")),
            )
            .child(
                field()
                    .label(t!("settings.local_proxy"))
                    .description(prose(t!(
                        "settings.local_proxy_hint",
                        system = self
                            .system_proxy
                            .clone()
                            .unwrap_or_else(|| t!("settings.local_proxy_none").to_string())
                    )))
                    .child(Input::new(&self.local_proxy).id("settings-local-proxy")),
            )
            .child(
                field()
                    .label(t!("settings.local_proxy_port"))
                    .description(prose(t!("settings.local_proxy_port_hint")))
                    .child(Input::new(&self.local_proxy_port).id("settings-local-proxy-port")),
            )
            .child(
                field()
                    .label(t!("settings.proxy_commands"))
                    .description(prose(t!("settings.proxy_commands_hint")))
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(div().text_sm().child(if self.allowed_proxy_commands == 0 {
                                t!("settings.proxy_commands_none")
                            } else {
                                t!(
                                    "settings.proxy_commands_count",
                                    count = self.allowed_proxy_commands
                                )
                            }))
                            .when(self.allowed_proxy_commands > 0, |this| {
                                this.child(
                                    Button::new("settings-forget-proxy-commands")
                                        .outline()
                                        .small()
                                        .label(t!("settings.proxy_commands_forget"))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.forget_proxy_commands(window, cx)
                                        })),
                                )
                            }),
                    ),
            )
            .into_any_element()
    }

    /// 光标的默认样式：方块 / 竖线 / 下划线。
    fn cursor_shape_radio(&self, cx: &mut Context<Self>) -> RadioGroup {
        const CHOICES: [CursorShape; 3] =
            [CursorShape::Block, CursorShape::Bar, CursorShape::Underline];
        let current = self.draft.terminal.cursor_shape;
        RadioGroup::horizontal("settings-cursor-shape")
            .children([
                t!("settings.cursor.block"),
                t!("settings.cursor.bar"),
                t!("settings.cursor.underline"),
            ])
            .selected_index(CHOICES.iter().position(|c| *c == current))
            .on_change(cx.listener(|this, ix: &usize, _, cx| {
                if let Some(choice) = CHOICES.get(*ix) {
                    this.draft.terminal.cursor_shape = *choice;
                    cx.notify();
                }
            }))
    }

    /// Tab 的位置：顶部 / 左侧（本机设置，与命令面板的“切换 Tab 的位置”改的是同一项）。
    fn tab_position_radio(&self, cx: &mut Context<Self>) -> RadioGroup {
        const CHOICES: [TabPosition; 2] = [TabPosition::Top, TabPosition::Left];
        let current = self.local.window.tab_position;
        RadioGroup::horizontal("settings-tab-position")
            .children([
                t!("settings.tab_position_top"),
                t!("settings.tab_position_left"),
            ])
            .selected_index(CHOICES.iter().position(|c| *c == current))
            .on_change(cx.listener(|this, ix: &usize, _, cx| {
                if let Some(choice) = CHOICES.get(*ix) {
                    this.local.window.tab_position = *choice;
                    cx.notify();
                }
            }))
    }

    /// 终端底部状态条的样式：完整 / 精简（在状态条上右键改的是同一项）。
    fn monitor_bar_radio(&self, cx: &mut Context<Self>) -> RadioGroup {
        const CHOICES: [MonitorBar; 2] = [MonitorBar::Full, MonitorBar::Compact];
        let current = self.draft.terminal.monitor_bar;
        RadioGroup::horizontal("settings-monitor-bar")
            .children([
                t!("monitor.bar.style_full"),
                t!("monitor.bar.style_compact"),
            ])
            .selected_index(CHOICES.iter().position(|c| *c == current))
            .on_change(cx.listener(|this, ix: &usize, _, cx| {
                if let Some(choice) = CHOICES.get(*ix) {
                    this.draft.terminal.monitor_bar = *choice;
                    cx.notify();
                }
            }))
    }

    /// 被外部程序调起时：每次询问 / 直接打开 / 忽略。
    fn external_launch_radio(&self, cx: &mut Context<Self>) -> RadioGroup {
        const CHOICES: [ExternalLaunch; 3] = [
            ExternalLaunch::Ask,
            ExternalLaunch::Allow,
            ExternalLaunch::Off,
        ];
        let current = self.draft.connection.external_launch;
        RadioGroup::horizontal("settings-external-launch")
            .children([
                t!("settings.external_launch_ask"),
                t!("settings.external_launch_allow"),
                t!("settings.external_launch_off"),
            ])
            .selected_index(CHOICES.iter().position(|c| *c == current))
            .on_change(cx.listener(|this, ix: &usize, _, cx| {
                if let Some(choice) = CHOICES.get(*ix) {
                    this.draft.connection.external_launch = *choice;
                    cx.notify();
                }
            }))
    }

    fn render_about(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let data_dir = AppState::global(cx).data_dir.root().display().to_string();
        let portable = AppState::global(cx).data_dir.is_portable();
        v_flex()
            .gap_3()
            // 关于 Termlet：版本、简介、官网等链接与版权
            .child(crate::lock_ui::app_info(cx))
            .child(
                field()
                    .label(t!("settings.data_dir"))
                    .when(portable, |field| {
                        field.description(prose(t!("settings.data_dir_portable")))
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_sm()
                                    .font_family(termlet_terminal::fonts::DEFAULT_FAMILY)
                                    .child(data_dir.clone()),
                            )
                            .child(
                                Button::new("settings-open-data-dir")
                                    .outline()
                                    .small()
                                    .icon(IconName::FolderOpen)
                                    .label(t!("settings.open_data_dir"))
                                    .on_click(move |_, _, cx| {
                                        cx.open_url(&data_dir);
                                    }),
                            ),
                    ),
            )
            // 更新、诊断信息与崩溃上报
            .child(self.about_extras.clone())
            .into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let blocked = self.closing_blocked(cx);
        let theme = cx.theme();
        h_flex()
            .gap_2()
            .pt_3()
            .border_t_1()
            .border_color(theme.border)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .text_sm()
                    .when_some(self.error.clone(), |this, error| {
                        this.child(div().text_color(theme.danger).child(prose(error)))
                    })
                    .child(self.notice.clone()),
            )
            .child(
                Button::new("settings-cancel")
                    .label(t!("common.cancel"))
                    .disabled(blocked)
                    .on_click(|_, window, cx| window.close_dialog(cx)),
            )
            .child(
                Button::new("settings-save")
                    .primary()
                    .label(t!("common.save"))
                    .disabled(blocked)
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.save(window, cx) {
                            window.close_dialog(cx);
                        }
                    })),
            )
    }
}

impl Render for SettingsForm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.page {
            Page::General => self.render_general(cx),
            Page::Toolbar => self.toolbar.clone().into_any_element(),
            Page::Terminal => self.render_terminal(cx),
            Page::Schemes => self.schemes.clone().into_any_element(),
            Page::Local => self.local_page.clone().into_any_element(),
            Page::Highlight => self.highlight.clone().into_any_element(),
            Page::Connection => self.render_connection(cx),
            Page::KnownHosts => self.known_hosts.clone().into_any_element(),
            Page::Security => self.security_page.clone().into_any_element(),
            Page::Cli => self.cli_page.clone().into_any_element(),
            Page::Backup => self.backup_page.clone().into_any_element(),
            Page::Sync => self.sync_page.clone().into_any_element(),
            Page::Shortcuts => self.shortcuts.clone().into_any_element(),
            Page::About => self.render_about(cx),
        };
        // 高 42rem，窗口矮时缩到窗口内（对话框上下各留一些边距）
        let height = rems(42.)
            .to_pixels(window.rem_size())
            .min(window.viewport_size().height - rems(8.).to_pixels(window.rem_size()));
        v_flex()
            .id("settings-dialog")
            .test_support()
            .h(height)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .gap_4()
                    .child(self.render_nav(window, cx))
                    .child(
                        div()
                            .id("settings-page")
                            .test_support()
                            .flex_1()
                            .min_w_0()
                            .overflow_y_scroll()
                            .pr_1()
                            .child(
                                v_flex()
                                    .gap_3()
                                    .pb_3()
                                    .child(div().text_base().font_medium().child(self.page.title()))
                                    .child(body),
                            ),
                    ),
            )
            .child(self.render_footer(cx))
    }
}

/// 字体下拉框的选项：默认（内置字体）、本机已装的推荐字体、其余等宽字体。当前值不在其中（字体已卸载，
/// 或字体列表还没读到）时也列出来。
fn font_choices(installed: Option<&[String]>, current: &str) -> Vec<Choice<String>> {
    let mut choices = vec![Choice::new(t!("settings.font.default"), String::new())];
    let installed = installed.unwrap_or(&[]);
    for r in fonts::RECOMMENDED
        .iter()
        .filter(|r| installed.iter().any(|f| f == r.family))
    {
        choices.push(Choice::new(
            t!(
                "settings.font.recommended",
                name = r.family,
                note = r.note()
            ),
            r.family.to_string(),
        ));
    }
    for f in installed {
        if !fonts::RECOMMENDED.iter().any(|r| r.family == f) {
            choices.push(Choice::new(f.clone(), f.clone()));
        }
    }
    if !current.is_empty() && !choices.iter().any(|c| c.value == current) {
        choices.insert(1, Choice::new(current.to_string(), current.to_string()));
    }
    choices
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, TestAppContext};

    use super::Page;
    use crate::i18n::{Language, set_language};
    use crate::testkit::{assert_nav_labels_fit, open_workspace};

    // 左侧导航的每一项在每种语言下都放得下分页名：英文的比中文长，导航随最长的放宽，不裁掉文字
    #[gpui_kit::test]
    fn nav_labels_fit_in_every_language(cx: &mut TestAppContext) {
        let handle = open_workspace(cx);
        for language in Language::ALL {
            set_language(language);
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.click("open-settings", cx);
                window.render_frame(cx);
                let items = Page::ALL.map(|page| (page.nav_id(), page.title()));
                assert_nav_labels_fit(window, items, &format!("设置（{language:?}）"));
                window.click("settings-cancel", cx);
            })
            .unwrap();
            cx.run_until_parked();
        }
        set_language(Language::ZhCn);
    }

    /// 在模拟的 `os` 上打开设置的“终端”页：有“Option 键作为 Meta 键”时勾选并保存。返回有没有这一项、保存后的设置。
    fn option_as_meta_on(cx: &mut TestAppContext, os: gpui_kit::OperatingSystem) -> (bool, bool) {
        cx.set_platform_info(gpui_kit::PlatformInfo {
            os,
            target: gpui_kit::PlatformTarget::Native,
        });
        let handle = open_workspace(cx);
        set_language(Language::ZhCn);
        let shown = cx
            .update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.click("open-settings", cx);
                window.render_frame(cx);
                window.click(Page::Terminal.nav_id(), cx);
                window.render_frame(cx);
                let shown = window.try_find("settings-option-as-meta").is_some();
                if shown {
                    // 各系统的字体高度不同，这一项可能在可见区域下面：先在页面上往下滚
                    window.scroll(
                        "settings-font-size",
                        gpui_kit::ScrollDelta::Pixels(gpui_kit::point(
                            gpui_kit::px(0.),
                            gpui_kit::px(-400.),
                        )),
                        cx,
                    );
                    window.render_frame(cx);
                    window.click("settings-option-as-meta", cx);
                    window.render_frame(cx);
                }
                window.click("settings-save", cx);
                shown
            })
            .unwrap();
        cx.run_until_parked();
        let saved = cx.update(|cx| {
            crate::app_state::AppState::store(cx)
                .settings()
                .terminal
                .option_as_meta
        });
        (shown, saved)
    }

    // “Option 键作为 Meta 键”只在 macOS 的“终端”页上有，勾选后保存进设置；其他系统上 Alt 本来就作为 Meta，没有这一项
    #[gpui_kit::test]
    fn option_as_meta_is_on_the_mac_terminal_page(cx: &mut TestAppContext) {
        assert_eq!(
            option_as_meta_on(cx, gpui_kit::OperatingSystem::Mac),
            (true, true)
        );
    }

    #[gpui_kit::test]
    fn option_as_meta_is_not_on_other_systems(cx: &mut TestAppContext) {
        assert_eq!(
            option_as_meta_on(cx, gpui_kit::OperatingSystem::Windows),
            (false, false)
        );
    }

    // “关于”页最上面：版本、复制版本信息的按钮、官网等链接与版权；开发版没有随程序分发的第三方许可，不显示那一项
    #[gpui_kit::test]
    fn about_page_shows_the_app_and_its_links(cx: &mut TestAppContext) {
        let handle = open_workspace(cx);
        set_language(Language::ZhCn);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("open-settings", cx);
            window.render_frame(cx);
            window.click(Page::About.nav_id(), cx);
            window.render_frame(cx);
            for id in [
                "about-copy-version",
                "about-website",
                "about-changelog",
                "about-feedback",
                "about-license",
                "about-source",
                "about-copyright",
            ] {
                assert!(window.try_find(id).is_some(), "{id}");
            }
            assert!(window.try_find("about-third-party").is_none(), "开发版没有");
            assert!(window.try_find("about-portable").is_none(), "不是便携版");
            window.click("about-copy-version", cx);
        })
        .unwrap();
        let copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default();
        assert!(copied.starts_with(&format!("Termlet {}\n", termlet_core::VERSION)));
    }

    // 立即执行的操作（复制版本信息）的结果写在对话框底部的提示行里、在对话框之内，不弹出会被对话框盖住的通知；
    // 换到别的分类后清掉
    #[gpui_kit::test]
    fn results_show_at_the_bottom_of_the_dialog(cx: &mut TestAppContext) {
        use crate::notify::{Notice, shown};
        use gpui_kit::component::WindowExt as _;
        let handle = open_workspace(cx);
        set_language(Language::ZhCn);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("open-settings", cx);
            window.render_frame(cx);
            window.click(Page::About.nav_id(), cx);
            window.render_frame(cx);
            let toasts = window.notifications(cx).len();
            assert!(window.try_find("dialog-notice").is_none());
            window.click("about-copy-version", cx);
            window.render_frame(cx);
            assert_eq!(
                shown(window, cx),
                Some(Notice::success(crate::t!("settings.about.version_copied")))
            );
            assert_eq!(window.notifications(cx).len(), toasts, "不弹出通知");
            let dialog = window.find("settings-dialog").bounds();
            let notice = window.find("dialog-notice").bounds();
            let cancel = window.find("settings-cancel").bounds();
            assert!(
                notice.left() >= dialog.left()
                    && notice.right() <= cancel.left()
                    && notice.top() >= dialog.top()
                    && notice.bottom() <= dialog.bottom(),
                "在对话框底部、按钮的左边：{notice:?} / {dialog:?}"
            );

            window.click(Page::General.nav_id(), cx);
            window.render_frame(cx);
            assert_eq!(shown(window, cx), None);
            assert!(window.try_find("dialog-notice").is_none());
        })
        .unwrap();
    }

    // 便携版：“关于”页的版本旁注明便携版，数据目录下说明数据在程序旁边；复制的版本信息也注明
    #[gpui_kit::test]
    fn about_page_marks_the_portable_version(cx: &mut TestAppContext) {
        let handle = crate::testkit::open_portable_workspace(cx);
        set_language(Language::ZhCn);
        // 第一次打开时选了“只在这台电脑上使用”
        cx.update(|cx| {
            crate::app_state::AppState::store(cx)
                .choose_vault_keyring()
                .unwrap()
        });
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("open-settings", cx);
            window.render_frame(cx);
            window.click(Page::About.nav_id(), cx);
            window.render_frame(cx);
            assert!(window.try_find("about-portable").is_some());
            window.click("about-copy-version", cx);
        })
        .unwrap();
        let copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default();
        assert!(copied.starts_with(&format!("Termlet {} (portable)\n", termlet_core::VERSION)));
    }

    // “终端”页的光标样式与光标闪烁：默认方块、不闪烁；选竖线、勾上闪烁后保存进（会同步的）终端设置
    #[gpui_kit::test]
    fn cursor_style_and_blink_are_saved(cx: &mut TestAppContext) {
        use termlet_core::model::CursorShape;
        let handle = open_workspace(cx);
        set_language(Language::ZhCn);
        let terminal = |cx: &mut TestAppContext| {
            cx.update(|cx| crate::app_state::AppState::store(cx).settings().terminal)
        };
        let before = terminal(cx);
        assert_eq!(
            (before.cursor_shape, before.cursor_blink),
            (CursorShape::Block, false)
        );
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("open-settings", cx);
            window.render_frame(cx);
            window.click(Page::Terminal.nav_id(), cx);
            window.render_frame(cx);
            // 光标的两项在字体、右键之后，测试窗口中要往下滚才看得到
            window.scroll(
                "settings-page",
                gpui_kit::ScrollDelta::Pixels(gpui_kit::point(
                    gpui_kit::px(0.),
                    gpui_kit::px(-300.),
                )),
                cx,
            );
            window.render_frame(cx);
            window.within("settings-cursor-shape").click(1usize, cx);
            window.click("settings-cursor-blink", cx);
            window.render_frame(cx);
            window.click("settings-save", cx);
        })
        .unwrap();
        cx.run_until_parked();
        let after = terminal(cx);
        assert_eq!(
            (after.cursor_shape, after.cursor_blink),
            (CursorShape::Bar, true)
        );
    }

    // “终端”页的状态条样式：默认完整；选精简后保存进（会同步的）终端设置，与在状态条上右键改的是同一项
    #[gpui_kit::test]
    fn monitor_bar_style_is_saved(cx: &mut TestAppContext) {
        use termlet_core::model::MonitorBar;
        let handle = open_workspace(cx);
        set_language(Language::ZhCn);
        let style = |cx: &mut TestAppContext| {
            cx.update(|cx| {
                crate::app_state::AppState::store(cx)
                    .settings()
                    .terminal
                    .monitor_bar
            })
        };
        assert_eq!(style(cx), MonitorBar::Full);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("open-settings", cx);
            window.render_frame(cx);
            window.click(Page::Terminal.nav_id(), cx);
            window.render_frame(cx);
            // 在资源监控之后，测试窗口中要往下滚才看得到
            window.scroll(
                "settings-page",
                gpui_kit::ScrollDelta::Pixels(gpui_kit::point(
                    gpui_kit::px(0.),
                    gpui_kit::px(-600.),
                )),
                cx,
            );
            window.render_frame(cx);
            window.within("settings-monitor-bar").click(1usize, cx);
            window.render_frame(cx);
            window.click("settings-save", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(style(cx), MonitorBar::Compact);
    }

    // “通用”页的界面大小：选 150% 保存进本机设置（不同步），保存后界面立即放大；再打开设置时选中的是它
    #[gpui_kit::test]
    fn ui_scale_is_saved_locally_and_applied(cx: &mut TestAppContext) {
        let handle = open_workspace(cx);
        let open_form = |cx: &mut TestAppContext| {
            cx.update_window(handle, |_, window, cx| {
                cx.new(|cx| super::SettingsForm::new(Page::General, window, cx))
            })
            .unwrap()
        };
        let form = open_form(cx);
        cx.update_window(handle, |_, window, cx| {
            form.update(cx, |form, cx| {
                form.ui_scale
                    .update(cx, |state, cx| state.set_selected_value(&150, window, cx));
                assert!(form.save(window, cx));
            })
        })
        .unwrap();
        cx.run_until_parked();
        let store = cx.update(|cx| crate::app_state::AppState::store(cx));
        assert_eq!(store.local_settings().window.ui_scale, 150);
        assert_eq!(store.settings(), Default::default(), "会同步的设置没有变");
        let rem = cx
            .update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.rem_size()
            })
            .unwrap();
        assert_eq!(rem, gpui_kit::px(24.));
        let reopened = open_form(cx);
        assert_eq!(
            cx.update(|cx| super::chosen(&reopened.read(cx).ui_scale, cx)),
            Some(150)
        );
    }

    // “通用”页的“在编辑器中打开远程目录”与任务栏进度：保存进本机设置（不同步）；任务栏进度的开关只在 Windows、macOS 上有
    #[gpui_kit::test]
    fn remote_editor_and_taskbar_progress_are_saved_locally(cx: &mut TestAppContext) {
        let handle = open_workspace(cx);
        let form = cx
            .update_window(handle, |_, window, cx| {
                cx.new(|cx| super::SettingsForm::new(Page::General, window, cx))
            })
            .unwrap();
        cx.update_window(handle, |_, window, cx| {
            form.update(cx, |form, cx| {
                form.remote_editor.update(cx, |state, cx| {
                    state.set_selected_value(
                        &termlet_core::model::RemoteEditorSetting::Zed,
                        window,
                        cx,
                    )
                });
                form.local.window.taskbar_progress = false;
                assert!(form.save(window, cx));
            })
        })
        .unwrap();
        let store = cx.update(|cx| crate::app_state::AppState::store(cx));
        let window_settings = store.local_settings().window;
        assert_eq!(
            window_settings.remote_editor,
            termlet_core::model::RemoteEditorSetting::Zed
        );
        assert!(!window_settings.taskbar_progress);
        assert_eq!(store.settings(), Default::default(), "会同步的设置没有变");
        let reopened = cx
            .update_window(handle, |_, window, cx| {
                cx.new(|cx| super::SettingsForm::new(Page::General, window, cx))
            })
            .unwrap();
        assert_eq!(
            cx.update(|cx| super::chosen(&reopened.read(cx).remote_editor, cx)),
            Some(termlet_core::model::RemoteEditorSetting::Zed)
        );
        assert!(crate::taskbar::supported(
            gpui_kit::OperatingSystem::Windows
        ));
        assert!(crate::taskbar::supported(gpui_kit::OperatingSystem::Mac));
        assert!(crate::taskbar::supported(gpui_kit::OperatingSystem::Linux));
        assert!(!crate::taskbar::supported(
            gpui_kit::OperatingSystem::FreeBsd
        ));
    }

    // 界面放大到 150% 时，设置的导航放得下分页名，“通用”页的下拉框都在页面之内（不横向溢出）
    #[gpui_kit::test]
    fn general_page_fits_at_150_percent(cx: &mut TestAppContext) {
        let handle = open_workspace(cx);
        set_language(Language::En);
        cx.update(|cx| {
            crate::app_state::AppState::store(cx)
                .update_local_settings(|s| s.window.ui_scale = 150)
                .unwrap()
        });
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.rem_size(), gpui_kit::px(24.));
            window.click("open-settings", cx);
            window.render_frame(cx);
            let items = Page::ALL.map(|page| (page.nav_id(), page.title()));
            assert_nav_labels_fit(window, items, "设置（150%）");
            let page = window.find("settings-page").bounds();
            for id in [
                "settings-language",
                "settings-theme",
                "settings-icons",
                "settings-ui-scale",
                "settings-remote-editor",
            ] {
                let bounds = window.find(id).bounds();
                assert!(
                    bounds.left() >= page.left() && bounds.right() <= page.right(),
                    "{id} 超出了页面：{bounds:?}，页面 {page:?}"
                );
            }
        })
        .unwrap();
        set_language(Language::ZhCn);
    }
}
