//! 设置。
//!
//! [`Settings`] 会同步，按组存在 `setting` 表中（每组一行，同步按组合并）；
//! [`LocalSettings`] 只与这台电脑有关（本地终端、应用锁、CLI 开关、自动快照、使用统计的开关、关闭窗口时最小化到
//! 托盘、启动时打开的主机、标题栏的工具栏、本机的 X 显示与确认过的代理命令等），存在 `local_setting` 表中、不同步。
//! 每组都有 `extra` 保存不认识的字段。

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::Extra;
use crate::id::HostId;

/// 一组设置：数据库中的一行。
pub trait SettingsSection:
    Clone + Default + PartialEq + std::fmt::Debug + Serialize + DeserializeOwned + Send + Sync
{
    /// 行 ID（组名），发布后不能改。
    const ID: &'static str;
}

macro_rules! sections {
    ($($ty:ty => $id:literal),* $(,)?) => {$(
        impl SettingsSection for $ty {
            const ID: &'static str = $id;
        }
    )*};
}

/// 会同步的设置。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    pub general: GeneralSettings,
    pub appearance: AppearanceSettings,
    pub terminal: TerminalSettings,
    pub highlight: HighlightSettings,
    pub connection: ConnectionSettings,
    pub guard: GuardSettings,
    pub keybindings: KeybindingSettings,
}

/// 只与这台电脑有关的设置。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LocalSettings {
    pub cli: CliSettings,
    pub app_lock: AppLockSettings,
    pub backup: BackupSettings,
    pub shells: ShellSettings,
    pub logs: LogSettings,
    pub usage: UsageSettings,
    pub window: WindowSettings,
    pub connection: LocalConnectionSettings,
}

sections! {
    GeneralSettings => "general",
    AppearanceSettings => "appearance",
    TerminalSettings => "terminal",
    HighlightSettings => "highlight",
    ConnectionSettings => "connection",
    GuardSettings => "guard",
    KeybindingSettings => "keybindings",
    CliSettings => "cli",
    AppLockSettings => "app_lock",
    BackupSettings => "backup",
    ShellSettings => "shells",
    LogSettings => "logs",
    UsageSettings => "usage",
    WindowSettings => "window",
    LocalConnectionSettings => "local_connection",
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GeneralSettings {
    /// 关闭有连接的 Tab 或窗口前确认
    pub confirm_close: bool,
    /// 主机列表中名称下方显示 `用户名@地址`
    pub show_host_address: bool,
    pub auto_check_update: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            confirm_close: true,
            show_host_address: true,
            auto_check_update: true,
            extra: Extra::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "AppearanceFields")]
pub struct AppearanceSettings {
    pub language: LanguageSetting,
    pub theme: ThemeSetting,
    /// 图标：SFTP 的文件图标与主机、Tab 上的系统和品牌标志
    pub icons: IconStyle,
    #[serde(flatten)]
    pub extra: Extra,
}

/// 读取 [`AppearanceSettings`] 时的字段。图标的设置在开发版中叫 `fileIcons`（那时只管 SFTP 的文件图标）：
/// 读到它时沿用它的值（同时有 `icons` 时以 `icons` 为准，两个都在也不报错），写回时只写 `icons`。
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct AppearanceFields {
    language: LanguageSetting,
    theme: ThemeSetting,
    icons: Option<IconStyle>,
    file_icons: Option<IconStyle>,
    #[serde(flatten)]
    extra: Extra,
}

impl From<AppearanceFields> for AppearanceSettings {
    fn from(fields: AppearanceFields) -> Self {
        Self {
            language: fields.language,
            theme: fields.theme,
            icons: fields.icons.or(fields.file_icons).unwrap_or_default(),
            extra: fields.extra,
        }
    }
}

/// 界面中的图标：彩色（SFTP 的文件图标按类型配色，系统与品牌标志用品牌色），或与界面其余部分一样的单色。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IconStyle {
    #[default]
    Color,
    Mono,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LanguageSetting {
    /// 跟随系统
    #[default]
    #[serde(rename = "system")]
    System,
    #[serde(rename = "zh-CN")]
    ZhCn,
    /// 繁體中文（台湾用语）
    #[serde(rename = "zh-TW")]
    ZhTw,
    #[serde(rename = "en")]
    En,
    #[serde(rename = "ja")]
    Ja,
    #[serde(rename = "ko")]
    Ko,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeSetting {
    #[default]
    Light,
    Dark,
    System,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TerminalSettings {
    pub font_size: u16,
    /// 字体；留空用内置的 JetBrains Mono
    pub font_family: String,
    /// 每个终端保留的历史行数
    pub scrollback: u32,
    pub copy_on_select: bool,
    pub right_click: RightClick,
    /// 光标的默认样式；程序用 DECSCUSR（`CSI Ps SP q`）设置的优先，RIS 后回到这里的
    pub cursor_shape: CursorShape,
    /// 光标默认闪烁；同样是程序设置的优先
    pub cursor_blink: bool,
    /// macOS 上 Option 键作为 Meta 键（Option 加字母发送 ESC 前缀，不再输入组合字符）；其他系统上不起作用
    pub option_as_meta: bool,
    /// 界面为浅色 / 深色时终端用的配色（配色 ID：内置配色或自定义配色）
    pub scheme_light: String,
    pub scheme_dark: String,
    /// 保证文字可读：文字与背景的最低对比度，0 表示关闭，开启时为 1–21
    pub min_contrast: f32,
    /// 粘贴前检查（多行、危险命令、看不见的字符）
    pub paste_warning: bool,
    /// 后台的终端有新输出时在 Tab 上标出
    pub tab_activity: bool,
    /// 后台终端中耗时较长的命令结束时发通知（需要 shell 输出 OSC 133）
    pub notify_command_done: bool,
    /// 出现 sudo 密码提示时可填入已保存的密码
    pub password_fill: bool,
    /// 自动记录会话日志
    pub session_log: bool,
    /// 会话录制同时记录输入（可能含密码，默认关；只对这台电脑生效，不随云同步）
    pub record_input: bool,
    /// 识别终端中的文件传输（rz / sz、trz / tsz）
    pub file_transfer: bool,
    /// 传输完成后自动清除
    pub transfer_auto_clear: bool,
    /// 终端底部显示服务器的资源使用情况（Linux）
    pub monitor: bool,
    /// 终端底部显示网络延迟
    pub latency: bool,
    /// 底部状态条的样式（完整、精简）
    pub monitor_bar: MonitorBar,
    /// 记录命令历史（加密保存，每台主机最近 1000 条）
    pub command_history: bool,
    /// 输入命令时按这台主机的命令历史在光标后用淡色给出补全提示（→ 接受）：需要 shell 集成（OSC 133）认出提示符，
    /// 命令历史关闭时不起作用；默认关
    pub command_suggest: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

/// 内置的默认终端配色。
pub const DEFAULT_SCHEME_LIGHT: &str = "termlet-light";
pub const DEFAULT_SCHEME_DARK: &str = "termlet-dark";

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            font_size: 13,
            font_family: String::new(),
            scrollback: 5000,
            copy_on_select: false,
            right_click: RightClick::Auto,
            cursor_shape: CursorShape::Block,
            cursor_blink: false,
            option_as_meta: false,
            scheme_light: DEFAULT_SCHEME_LIGHT.into(),
            scheme_dark: DEFAULT_SCHEME_DARK.into(),
            min_contrast: 0.0,
            paste_warning: true,
            tab_activity: true,
            notify_command_done: true,
            password_fill: true,
            session_log: false,
            record_input: false,
            file_transfer: true,
            transfer_auto_clear: false,
            monitor: true,
            latency: true,
            monitor_bar: MonitorBar::Full,
            command_history: true,
            command_suggest: false,
            extra: Extra::new(),
        }
    }
}

/// 终端底部状态条的样式。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MonitorBar {
    /// 延迟、CPU、内存、最满的磁盘、网速、负载（宽度够时带进度条与网速的小折线）
    #[default]
    Full,
    /// 平时只有延迟、CPU、内存；最满的磁盘、负载偏高时才出现
    Compact,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RightClick {
    /// 有选中内容时复制，否则粘贴（与 Windows Terminal、PuTTY 相同）
    #[default]
    Auto,
    Paste,
    /// 弹出菜单
    Menu,
}

/// 终端光标的样式。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CursorShape {
    /// 方块
    #[default]
    Block,
    /// 竖线
    Bar,
    /// 下划线
    Underline,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HighlightSettings {
    pub enabled: bool,
    pub rules: Vec<HighlightRule>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for HighlightSettings {
    fn default() -> Self {
        let rule = |pattern: &str, color: &str| HighlightRule {
            pattern: pattern.into(),
            regex: true,
            case_sensitive: false,
            color: color.into(),
            enabled: true,
        };
        Self {
            enabled: true,
            rules: vec![
                rule(
                    r"\b(error|errors|fail|failed|failure|fatal|panic|denied|refused|exception|critical)\b",
                    "red",
                ),
                rule(r"\b(warn|warning|warnings|deprecated)\b", "yellow"),
                rule(r"\b(ok|success|successful|succeeded|passed)\b", "green"),
                rule(r"\b(?:\d{1,3}\.){3}\d{1,3}(?:/\d{1,2})?\b", "cyan"),
            ],
            extra: Extra::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HighlightRule {
    pub pattern: String,
    pub regex: bool,
    pub case_sensitive: bool,
    /// 终端配色中的颜色名（red、yellow……），深浅配色下各自取对应的颜色
    pub color: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ConnectionSettings {
    /// SSH 保活间隔（秒），0 表示关闭
    pub keepalive_seconds: u32,
    /// 连接意外断开时自动重连
    pub auto_reconnect: bool,
    /// ssh-agent 的地址（Windows 为命名管道，其他平台为 Unix 套接字）；留空自动查找
    pub agent_path: String,
    /// 被堡垒机等外部程序调起时的处理
    pub external_launch: ExternalLaunch,
    /// SSH 连接后识别远程系统（在这条连接上另开通道执行一条只读命令，见 [`crate::osprobe`]），主机列表与 Tab
    /// 显示系统的标志；默认开，不愿在服务器上多执行命令时关掉
    pub detect_os: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for ConnectionSettings {
    fn default() -> Self {
        Self {
            keepalive_seconds: 30,
            auto_reconnect: true,
            agent_path: String::new(),
            external_launch: ExternalLaunch::Ask,
            detect_os: true,
            extra: Extra::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExternalLaunch {
    #[default]
    Ask,
    Allow,
    Off,
}

/// 危险命令规则。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GuardSettings {
    /// 关闭的内置规则 ID
    pub disabled_rules: Vec<String>,
    /// 自定义规则（正则）
    pub custom_rules: Vec<String>,
    /// CLI 命中规则时：弹窗请用户批准，或直接拦截
    pub approval: Approval,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Approval {
    #[default]
    Ask,
    Block,
}

/// 改过的快捷键：动作 → 按键组合（`ctrl-shift-c`），空列表表示停用；没有出现的动作用默认值。按平台分开存
/// （与 VS Code 的设置同步相同）：macOS 的习惯与 Windows、Linux 不同（⌘ 与 Ctrl），一边改的不影响另一边。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct KeybindingSettings {
    /// Windows 与 Linux 共用（两边的规则相同）
    pub bindings: BTreeMap<String, Vec<String>>,
    /// macOS
    pub mac_bindings: BTreeMap<String, Vec<String>>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CliSettings {
    /// 接受外部 CLI（`termlet` 命令）的请求，默认关闭
    pub enabled: bool,
    /// Termlet 没有运行时，`termlet` 命令自动在后台（不开窗口）启动它，默认允许；关掉后提示先打开 Termlet
    pub auto_start: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for CliSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_start: true,
            extra: Extra::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AppLockSettings {
    /// 系统空闲多少分钟后自动锁定，0 表示不自动锁定（设置了锁定密码时生效）
    pub idle_minutes: u32,
    pub on_startup: bool,
    pub on_system_lock: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for AppLockSettings {
    fn default() -> Self {
        Self {
            idle_minutes: 15,
            on_startup: true,
            on_system_lock: true,
            extra: Extra::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct BackupSettings {
    /// 每天自动保存一份本机快照
    pub auto: bool,
    /// 自动快照保留的份数（1–30）
    pub keep: u32,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for BackupSettings {
    fn default() -> Self {
        Self {
            auto: true,
            keep: 7,
            extra: Extra::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ShellSettings {
    /// 用户添加的本地终端
    pub custom: Vec<LocalShell>,
    /// 侧边栏"本机"中除默认的一项以外还要列出的（`local:<id>` 或 `wsl:<发行版>`）
    pub sidebar: Vec<String>,
    /// Ctrl+Shift+T 打开的本地终端（`local:<id>` 或 `wsl:<发行版>`）；留空取列表中的第一个
    pub default: String,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LocalShell {
    /// `custom-<字母数字>`
    pub id: String,
    pub name: String,
    /// 命令行：第一项是程序（完整路径，或在 PATH 中查找），其余是参数
    pub command: String,
    /// 起始目录；留空为用户主目录
    pub dir: String,
    /// 额外的环境变量
    pub env: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LogSettings {
    /// 会话日志的目录；留空为数据目录下的 session-logs
    pub session_log_dir: String,
    /// 会话录制的目录；留空为数据目录下的 recordings
    pub recording_dir: String,
    /// 自动发送脱敏后的崩溃报告
    pub crash_report: bool,
    /// 程序日志的详细程度（排查问题时调高；见 `logging`）
    pub level: LogLevel,
    #[serde(flatten)]
    pub extra: Extra,
}

/// 匿名使用统计（见 [`crate::usage`]）：只对这台电脑生效，开关是每台电脑分别设的，所以不随云同步。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UsageSettings {
    /// 发送匿名使用统计，默认开
    pub enabled: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for UsageSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            extra: Extra::new(),
        }
    }
}

/// 窗口的行为、界面大小与这台电脑上的其他程序（这台电脑上的习惯，不随云同步）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WindowSettings {
    /// 关闭窗口时最小化到托盘（只有 Windows），默认关：最后一个主窗口关闭时隐藏到通知区域，会话照常运行
    pub close_to_tray: bool,
    /// 界面大小（百分比，档位见 [`crate::ui_scale::PRESETS`]）；0 为跟随系统的文字大小（默认）
    pub ui_scale: u16,
    /// 传输进度显示在任务栏按钮（Windows）、程序坞图标（macOS）上，默认开
    pub taskbar_progress: bool,
    /// 文件管理的菜单中“在 VS Code / Zed 中打开”显示哪些（只显示这台电脑上装了的，见 [`crate::remote_editor`]）
    pub remote_editor: RemoteEditorSetting,
    /// 启动 Termlet 时自动打开的（默认不额外打开）
    pub startup: StartupSettings,
    /// 标题栏右侧工具栏的按钮：顺序与是否显示（按钮 ID 由桌面端定义）。空为默认；没有列出的按钮（新版本加的）
    /// 由桌面端按默认的位置与显示补上
    pub toolbar: Vec<ToolbarButton>,
    /// Tab 放在哪里：窗格顶上横排（默认），或窗口左边竖排成一列
    pub tab_position: TabPosition,
    /// 竖排时同一窗格中连同一台主机的几个 Tab 归到可折叠的主机标题下（只影响显示），默认关
    pub group_tabs_by_host: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for WindowSettings {
    fn default() -> Self {
        Self {
            close_to_tray: false,
            ui_scale: 0,
            taskbar_progress: true,
            remote_editor: RemoteEditorSetting::default(),
            startup: StartupSettings::default(),
            toolbar: Vec::new(),
            tab_position: TabPosition::default(),
            group_tabs_by_host: false,
            extra: Extra::new(),
        }
    }
}

/// Tab 的位置。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TabPosition {
    /// 窗口左边（主机侧栏的右边）竖排成一列
    Left,
    /// 窗格顶上横排（默认）；新版本加的、不认识的位置也按它处理（serde 要求 `other` 是最后一项）
    #[default]
    #[serde(other)]
    Top,
}

/// 启动 Termlet 时自动打开什么。只在启动时（本次运行的第一个主窗口）生效，新窗口不打开；保险库、应用锁解锁后才打开。
/// 这台电脑上的习惯（各台电脑的本地终端、常连的主机不同），不随云同步。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StartupSettings {
    pub open: StartupOpen,
    /// 要打开的主机，按打开的先后；`open` 不是 [`StartupOpen::Hosts`] 时也保留（切回来时还在）
    pub hosts: Vec<HostId>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// 启动时打开的种类。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StartupOpen {
    /// 默认的本地终端（与 Ctrl+Shift+T 打开的相同，见本地终端的设置）
    Local,
    /// 指定的主机（[`StartupSettings::hosts`]）
    Hosts,
    /// 不额外打开（默认）；新版本加的、不认识的种类也按它处理（serde 要求 `other` 是最后一项）
    #[default]
    #[serde(other)]
    Nothing,
}

/// 工具栏中的一个按钮。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ToolbarButton {
    pub id: String,
    pub shown: bool,
}

impl Default for ToolbarButton {
    fn default() -> Self {
        Self {
            id: String::new(),
            shown: true,
        }
    }
}

/// 文件管理中“在编辑器中打开远程目录”显示哪些编辑器。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteEditorSetting {
    /// 装了的都显示（默认）
    #[default]
    Auto,
    /// 只显示 VS Code
    #[serde(rename = "vscode")]
    VsCode,
    /// 只显示 Zed
    Zed,
    /// 都不显示
    Off,
}

/// 这台电脑上与连接有关的设置。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LocalConnectionSettings {
    /// X11 转发连接的本机 X 显示（`localhost:0`、`:0`、`/tmp/.X11-unix/X0` 等写法，见 [`crate::ssh::x11`]）；
    /// 留空时用环境变量 `DISPLAY`，没有时用 `localhost:0`
    pub x11_display: String,
    /// 在这台电脑上确认过、允许执行的代理命令：命令原文（替换占位符之前）的 SHA-256，不存命令本身
    /// （命令中可能带令牌）。见 [`crate::ssh::proxy_command`]
    pub allowed_proxy_commands: Vec<String>,
    /// “通过本机代理”借给服务器的本机 HTTP 代理（`127.0.0.1:7890` 等，见 [`crate::forward::ProxyAddress`]）；
    /// 留空时用系统代理（[`crate::forward::system_proxy`]）
    pub local_proxy: String,
    /// “通过本机代理”在服务器上监听的端口；0 时由服务器选
    pub local_proxy_port: u16,
    /// 信任的主机 CA（OpenSSH known_hosts 中的 `@cert-authority`）：它签发的主机证书在第一次连接时免去指纹确认。
    /// 与主机指纹一样只在这台电脑上，不同步，恢复备份时也不覆盖。见 [`crate::ssh::host_ca`]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub trusted_host_cas: Vec<TrustedHostCa>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// 一个信任的主机 CA。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TrustedHostCa {
    /// 适用的主机名模式，与 known_hosts 相同：逗号分隔，`*`、`?` 通配，`!` 排除，22 以外的端口写 `[host]:port`
    pub hosts: String,
    /// CA 的公钥（OpenSSH 格式一行，可带注释）
    pub public_key: String,
    /// 添加的时间（Unix 毫秒）
    pub added_at: i64,
}

/// 程序日志的详细程度。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// 平时：启动、连接、出错等（info）
    #[default]
    Normal,
    /// 排查问题：Termlet 每一步的细节（debug）
    Detailed,
    /// 最详细：再加上 SSH、SFTP、HTTP 协议的细节（trace），文件会很快变大
    Full,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_previous_version() {
        let terminal = TerminalSettings::default();
        // 字号 13 是用户定的
        assert_eq!(terminal.font_size, 13);
        assert_eq!(terminal.scrollback, 5000);
        assert_eq!(ConnectionSettings::default().keepalive_seconds, 30);
        assert_eq!(HighlightSettings::default().rules.len(), 4);
        assert_eq!(BackupSettings::default().keep, 7);
    }

    #[test]
    fn missing_fields_take_defaults() {
        let terminal: TerminalSettings = serde_json::from_str(r#"{"fontSize": 16}"#).unwrap();
        assert_eq!(terminal.font_size, 16);
        assert_eq!(terminal.scrollback, 5000);
        assert!(terminal.paste_warning);
    }

    // 识别远程系统默认开；之前保存的连接设置没有这一项时也是开
    #[test]
    fn detect_os_defaults_on() {
        assert!(ConnectionSettings::default().detect_os);
        let old: ConnectionSettings = serde_json::from_str(r#"{"keepaliveSeconds":10}"#).unwrap();
        assert!(old.detect_os);
        let off: ConnectionSettings = serde_json::from_str(r#"{"detectOs":false}"#).unwrap();
        assert!(!off.detect_os);
        assert_eq!(serde_json::to_value(&off).unwrap()["detectOs"], false);
    }

    // 图标默认彩色；开发版保存的 `fileIcons` 读作 `icons`（两个都在时以 `icons` 为准），写回时只有 `icons`，
    // 不认识的字段照常保留
    #[test]
    fn icons_default_to_color_and_read_the_old_name() {
        let appearance: AppearanceSettings = serde_json::from_str(r#"{"theme": "dark"}"#).unwrap();
        assert_eq!(appearance.icons, IconStyle::Color);
        assert_eq!(appearance.theme, ThemeSetting::Dark);
        let mono: AppearanceSettings = serde_json::from_str(r#"{"icons": "mono"}"#).unwrap();
        assert_eq!(mono.icons, IconStyle::Mono);
        let json = serde_json::to_value(&mono).unwrap();
        assert_eq!(json["icons"], "mono");

        let old: AppearanceSettings =
            serde_json::from_str(r#"{"fileIcons": "mono", "later": 1}"#).unwrap();
        assert_eq!(old.icons, IconStyle::Mono);
        let json = serde_json::to_value(&old).unwrap();
        assert_eq!(json["icons"], "mono");
        assert!(json.get("fileIcons").is_none(), "旧名字不再写回");
        assert_eq!(json["later"], 1);

        let both: AppearanceSettings =
            serde_json::from_str(r#"{"icons": "color", "fileIcons": "mono"}"#).unwrap();
        assert_eq!(both.icons, IconStyle::Color);
        assert_eq!(
            serde_json::from_value::<AppearanceSettings>(serde_json::to_value(&both).unwrap())
                .unwrap(),
            both
        );
    }

    #[test]
    fn usage_defaults_and_round_trip() {
        let usage = UsageSettings::default();
        assert!(usage.enabled, "默认发送");
        assert_eq!(UsageSettings::ID, "usage");
        let parsed: UsageSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, usage);
        let off = UsageSettings {
            enabled: false,
            ..Default::default()
        };
        let json = serde_json::to_value(&off).unwrap();
        assert_eq!(json, serde_json::json!({"enabled": false}));
        assert_eq!(serde_json::from_value::<UsageSettings>(json).unwrap(), off);
        // 新版本加的字段保留在 extra 中，写回时不丢
        let newer: UsageSettings =
            serde_json::from_str(r#"{"enabled":false,"later":"x"}"#).unwrap();
        assert!(!newer.enabled);
        assert_eq!(serde_json::to_value(&newer).unwrap()["later"], "x");
    }

    // 光标默认方块、不闪烁（与之前的版本相同）；之前保存的终端设置没有这两项时也是
    #[test]
    fn cursor_defaults_and_round_trip() {
        let terminal = TerminalSettings::default();
        assert_eq!(terminal.cursor_shape, CursorShape::Block);
        assert!(!terminal.cursor_blink);
        let old: TerminalSettings = serde_json::from_str(r#"{"fontSize": 14}"#).unwrap();
        assert_eq!(old.cursor_shape, CursorShape::Block);
        assert!(!old.cursor_blink);
        for (shape, name) in [
            (CursorShape::Block, "block"),
            (CursorShape::Bar, "bar"),
            (CursorShape::Underline, "underline"),
        ] {
            let terminal = TerminalSettings {
                cursor_shape: shape,
                cursor_blink: true,
                ..Default::default()
            };
            let json = serde_json::to_value(&terminal).unwrap();
            assert_eq!(json["cursorShape"], name);
            assert_eq!(json["cursorBlink"], true);
            assert_eq!(
                serde_json::from_value::<TerminalSettings>(json).unwrap(),
                terminal
            );
        }
    }

    // 状态条默认完整；之前保存的终端设置没有这一项时也是；随终端设置同步
    #[test]
    fn monitor_bar_defaults_and_round_trip() {
        assert_eq!(TerminalSettings::default().monitor_bar, MonitorBar::Full);
        let old: TerminalSettings = serde_json::from_str(r#"{"monitor": true}"#).unwrap();
        assert_eq!(old.monitor_bar, MonitorBar::Full);
        let terminal = TerminalSettings {
            monitor_bar: MonitorBar::Compact,
            ..Default::default()
        };
        let json = serde_json::to_value(&terminal).unwrap();
        assert_eq!(json["monitorBar"], "compact");
        assert_eq!(
            serde_json::from_value::<TerminalSettings>(json).unwrap(),
            terminal
        );
    }

    // 命令补全提示默认关；之前保存的终端设置没有这一项时也是关
    #[test]
    fn command_suggest_defaults_off_and_round_trips() {
        assert!(!TerminalSettings::default().command_suggest);
        let old: TerminalSettings = serde_json::from_str(r#"{"commandHistory": true}"#).unwrap();
        assert!(!old.command_suggest);
        let terminal = TerminalSettings {
            command_suggest: true,
            ..Default::default()
        };
        let json = serde_json::to_value(&terminal).unwrap();
        assert_eq!(json["commandSuggest"], true);
        assert_eq!(
            serde_json::from_value::<TerminalSettings>(json).unwrap(),
            terminal
        );
    }

    // 关闭窗口时最小化到托盘、界面大小：本机设置，默认不最小化到托盘、界面大小跟随系统；新版本加的字段保留
    #[test]
    fn window_defaults_and_round_trip() {
        assert_eq!(WindowSettings::ID, "window");
        let window = WindowSettings::default();
        assert!(!window.close_to_tray, "默认关");
        assert_eq!(window.ui_scale, crate::ui_scale::SYSTEM, "默认跟随系统");
        assert!(window.taskbar_progress, "任务栏进度默认开");
        assert_eq!(window.remote_editor, RemoteEditorSetting::Auto);
        assert_eq!(
            serde_json::from_str::<WindowSettings>("{}").unwrap(),
            window
        );
        let on: WindowSettings =
            serde_json::from_str(r#"{"closeToTray":true,"uiScale":125,"later":1}"#).unwrap();
        assert!(on.close_to_tray);
        assert_eq!(on.ui_scale, 125);
        let json = serde_json::to_value(&on).unwrap();
        assert_eq!(json["closeToTray"], true);
        assert_eq!(json["uiScale"], 125);
        assert_eq!(json["later"], 1);
        assert_eq!(serde_json::from_value::<WindowSettings>(json).unwrap(), on);
    }

    // 任务栏进度、在编辑器中打开：之前保存的窗口设置没有这两项时取默认值（开、显示装了的）；关掉、选定的照常读写
    #[test]
    fn taskbar_progress_and_remote_editor_round_trip() {
        let old: WindowSettings = serde_json::from_str(r#"{"uiScale":110}"#).unwrap();
        assert!(old.taskbar_progress);
        assert_eq!(old.remote_editor, RemoteEditorSetting::Auto);
        for (setting, name) in [
            (RemoteEditorSetting::Auto, "auto"),
            (RemoteEditorSetting::VsCode, "vscode"),
            (RemoteEditorSetting::Zed, "zed"),
            (RemoteEditorSetting::Off, "off"),
        ] {
            let window = WindowSettings {
                taskbar_progress: false,
                remote_editor: setting,
                ..Default::default()
            };
            let json = serde_json::to_value(&window).unwrap();
            assert_eq!(json["taskbarProgress"], false);
            assert_eq!(json["remoteEditor"], name);
            assert_eq!(
                serde_json::from_value::<WindowSettings>(json).unwrap(),
                window
            );
        }
    }

    // 启动时打开、工具栏：之前保存的窗口设置没有这两项时默认不额外打开、工具栏为默认；选定的照常读写，
    // 主机的顺序不变；新版本的种类按不额外打开处理（主机列表保留），新版本加的字段保留
    #[test]
    fn startup_and_toolbar_round_trip() {
        let old: WindowSettings = serde_json::from_str(r#"{"uiScale":110}"#).unwrap();
        assert_eq!(old.startup, StartupSettings::default());
        assert_eq!(old.startup.open, StartupOpen::Nothing);
        assert!(old.toolbar.is_empty());
        for (open, name) in [
            (StartupOpen::Nothing, "nothing"),
            (StartupOpen::Local, "local"),
            (StartupOpen::Hosts, "hosts"),
        ] {
            let window = WindowSettings {
                startup: StartupSettings {
                    open,
                    hosts: vec![HostId::from_raw("h_b"), HostId::from_raw("h_a")],
                    ..Default::default()
                },
                toolbar: vec![
                    ToolbarButton {
                        id: "theme".into(),
                        shown: true,
                    },
                    ToolbarButton {
                        id: "hostTools".into(),
                        shown: false,
                    },
                ],
                ..Default::default()
            };
            let json = serde_json::to_value(&window).unwrap();
            assert_eq!(json["startup"]["open"], name);
            assert_eq!(json["startup"]["hosts"], serde_json::json!(["h_b", "h_a"]));
            assert_eq!(
                json["toolbar"],
                serde_json::json!([
                    {"id": "theme", "shown": true},
                    {"id": "hostTools", "shown": false}
                ])
            );
            assert_eq!(
                serde_json::from_value::<WindowSettings>(json).unwrap(),
                window
            );
        }
        let newer: WindowSettings = serde_json::from_str(
            r#"{"startup":{"open":"workspace","hosts":["h_1"],"later":1},"toolbar":[{"id":"x"}]}"#,
        )
        .unwrap();
        assert_eq!(newer.startup.open, StartupOpen::Nothing);
        assert_eq!(newer.startup.hosts, [HostId::from_raw("h_1")]);
        assert_eq!(newer.startup.extra["later"], 1);
        assert_eq!(
            newer.toolbar,
            [ToolbarButton {
                id: "x".into(),
                shown: true
            }],
            "没写是否显示时为显示"
        );
    }

    // Tab 的位置、按主机分组：之前保存的窗口设置没有这两项时为顶部、不分组；选定的照常读写，新版本的位置按顶部处理
    #[test]
    fn tab_position_round_trip() {
        let old: WindowSettings = serde_json::from_str(r#"{"uiScale":110}"#).unwrap();
        assert_eq!(old.tab_position, TabPosition::Top);
        assert!(!old.group_tabs_by_host);
        for (position, name) in [(TabPosition::Top, "top"), (TabPosition::Left, "left")] {
            let window = WindowSettings {
                tab_position: position,
                group_tabs_by_host: true,
                ..Default::default()
            };
            let json = serde_json::to_value(&window).unwrap();
            assert_eq!(json["tabPosition"], name);
            assert_eq!(json["groupTabsByHost"], true);
            assert_eq!(
                serde_json::from_value::<WindowSettings>(json).unwrap(),
                window
            );
        }
        let newer: WindowSettings = serde_json::from_str(r#"{"tabPosition":"bottom"}"#).unwrap();
        assert_eq!(newer.tab_position, TabPosition::Top);
    }

    #[test]
    fn language_round_trip() {
        for (language, name) in [
            (LanguageSetting::System, "system"),
            (LanguageSetting::ZhCn, "zh-CN"),
            (LanguageSetting::ZhTw, "zh-TW"),
            (LanguageSetting::En, "en"),
            (LanguageSetting::Ja, "ja"),
            (LanguageSetting::Ko, "ko"),
        ] {
            assert_eq!(serde_json::to_value(language).unwrap(), name);
            let appearance: AppearanceSettings =
                serde_json::from_str(&format!(r#"{{"language": "{name}"}}"#)).unwrap();
            assert_eq!(appearance.language, language);
        }
        let appearance: AppearanceSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(appearance.language, LanguageSetting::System);
    }
}
