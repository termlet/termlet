//! 可自定义的快捷键：动作与默认按键、按键组合的写法与校验、冲突，以及与设置（[`KeybindingSettings`]）之间的转换。
//!
//! 按键组合用 GPUI 的写法：小写，修饰键按 `ctrl`、`alt`、`cmd`、`shift` 的顺序（与 GPUI 的 `Keystroke::unparse` 相同）
//! 用 `-` 连接，最后是键名，例如 `ctrl-shift-c`、`alt-shift-=`、`cmd-shift-d`、`shift-insert`、`f11`。Shift 加数字、
//! 符号键在 Windows、Linux 上写键面上的字符（`ctrl-shift-=`，不写 `ctrl-+`），在 macOS 上写 Shift 之后的字符、不写
//! `shift`（⇧⌘] 写 `cmd-}`，见下）。设置只保存改过的动作（动作 ID → 组合列表，空列表表示停用），没有出现的动作用默认值。
//!
//! 默认值按系统分两套（[`KeyOs`]）：Windows、Linux 用 Ctrl；macOS 照它的习惯用 ⌘（`cmd`），Ctrl 组合原样交给终端
//! （Ctrl+C、Ctrl+R 等给 shell）。`cmd` 只在 macOS 上可用（Windows、Linux 上它是 Win / Super 键，留给系统）。改过的
//! 快捷键也按系统分开存（[`bindings_of`]）：macOS 用 `mac_bindings`，Windows 与 Linux 共用 `bindings`（两边的规则
//! 相同）；设置在电脑之间同步时两份都同步，各用各的，Windows 上改的不会占掉 macOS 的 ⌘ 组合与交给终端的 Ctrl 组合。
//!
//! 注册到 GPUI 时要经键盘映射：`KeyBinding::load(组合, 动作, 上下文, false, None, cx)`（按 `cx` 的键盘映射换算）。
//! Windows 上 GPUI 把 Shift 加数字、符号键报告为 Shift 之后的字符并去掉 Shift（按 Ctrl+Shift+= 得到 `ctrl-+`），
//! 而 `KeyBinding::new` 不做映射，`alt-shift-=` 这样的组合在真实的键盘上触发不了；映射按当前的键盘布局换算。
//! macOS 上 GPUI 同样把 Shift 加数字、符号键报告为 Shift 之后的字符并去掉 Shift（⇧⌘] 得到 `cmd-}`，见 gpui-pre-macos
//! 的 `parse_keystroke`；只有 Shift 加字母保留 `shift`），但键盘映射不换算，所以这里直接写 Shift 之后的字符，
//! 录制得到的也是这种写法；`cmd-shift-]` 这样的组合在 macOS 上触发不了，不允许（[`KeyCombo::is_allowed`]）。
//! GPUI 不区分小键盘：小键盘的 +、-、0 报告为 `+`、`-`、`0`，与主键盘上产生同样字符的按键相同。

use std::collections::{BTreeMap, HashSet};
use std::fmt;

use crate::model::KeybindingSettings;

/// 每个动作最多这么多个快捷键。
pub const MAX_COMBOS: usize = 4;

/// 有名字的键（GPUI 在 Windows 上报告的键名，见 gpui-pre-windows 的 `parse_immutable`），F 键另外判断。
/// Esc 留给界面（取消录制、关闭对话框），菜单键、浏览器的前进 / 后退键不作为快捷键。
const NAMED_KEYS: [&str; 14] = [
    "space",
    "enter",
    "tab",
    "backspace",
    "insert",
    "delete",
    "home",
    "end",
    "pageup",
    "pagedown",
    "up",
    "down",
    "left",
    "right",
];

/// 只带 Shift 也可以作为快捷键的键：Shift 加这些键不输入字符。
const SHIFT_KEYS: [&str; 6] = ["insert", "delete", "home", "end", "pageup", "pagedown"];

/// 快捷键按哪个系统的习惯：默认值、可用的修饰键与显示的写法随之不同。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyOs {
    Windows,
    Linux,
    /// 应用的快捷键用 ⌘，Ctrl 组合交给终端；Option 照系统的习惯输入组合字符，不作为快捷键
    Mac,
}

impl KeyOs {
    pub const ALL: [KeyOs; 3] = [KeyOs::Windows, KeyOs::Linux, KeyOs::Mac];

    /// 编译目标的系统。
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            KeyOs::Mac
        } else if cfg!(windows) {
            KeyOs::Windows
        } else {
            KeyOs::Linux
        }
    }

    pub fn is_mac(self) -> bool {
        self == KeyOs::Mac
    }
}

/// 设置页中的分组。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyGroup {
    /// 终端
    Terminal,
    /// Tab：切换
    Tabs,
    /// 分屏
    Panes,
    /// 窗口
    Window,
}

impl KeyGroup {
    /// 按设置页中的顺序。
    pub const ALL: [KeyGroup; 4] = [
        KeyGroup::Terminal,
        KeyGroup::Tabs,
        KeyGroup::Panes,
        KeyGroup::Window,
    ];

    /// 分组 ID。
    pub fn id(self) -> &'static str {
        match self {
            KeyGroup::Terminal => "terminal",
            KeyGroup::Tabs => "tabs",
            KeyGroup::Panes => "panes",
            KeyGroup::Window => "window",
        }
    }

    /// 这一组的动作，按设置页中的顺序。
    pub fn actions(self) -> impl Iterator<Item = KeyAction> {
        KeyAction::ALL
            .into_iter()
            .filter(move |a| a.group() == self)
    }
}

/// 快捷键在哪里生效。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyScope {
    /// 只在终端获得焦点时；焦点在输入框等地方时这些按键照常处理
    Terminal,
    /// 整个窗口：终端获得焦点时也生效，按键不再发给远端
    Workspace,
}

/// 可以自定义快捷键的动作，按设置页中的顺序。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyAction {
    /// 复制选中的文本。没有选中内容时什么也不做，按键也不发给远端；Ctrl+C 留给远端中断
    Copy,
    /// 粘贴（经过粘贴前检查）
    Paste,
    /// 在终端中搜索
    Search,
    /// 放大字号（只影响当前 Tab）
    ZoomIn,
    /// 缩小字号
    ZoomOut,
    /// 恢复设置中的字号
    ZoomReset,
    /// 复制会话（共用连接）
    Duplicate,
    /// 在当前终端旁边打开 SFTP（默认没有快捷键；Xshell 的键位方案是 Ctrl+Alt+F）
    SftpBeside,
    /// 撰写栏：把命令发送到多个终端
    Compose,
    /// 同步输入：在一个终端中输入，同时发送到多个终端（网格平铺）
    MultiExec,
    /// 快捷命令：搜索并发送快捷命令与命令历史
    CommandPicker,
    /// 重新连接终端
    Reconnect,
    /// 切换到同一组的下一个 Tab（到最后一个之后回到第一个）
    NextTab,
    /// 切换到同一组的上一个 Tab
    PrevTab,
    /// 切换到同一组的第 1～9 个 Tab
    Tab1,
    Tab2,
    Tab3,
    Tab4,
    Tab5,
    Tab6,
    Tab7,
    Tab8,
    Tab9,
    /// 把当前 Tab 移到新窗口（默认没有快捷键，免得占用常用的组合）
    MoveToNewWindow,
    /// 焦点进竖排的 Tab 列表（Tab 放在左侧时；之后 ↑ ↓ 移动、Enter 切换、Esc 回到终端）。默认没有快捷键，命令面板中有
    FocusTabList,
    /// 向右分屏
    SplitRight,
    /// 向下分屏
    SplitDown,
    /// 命令面板：搜索主机、Tab、操作，快速连接
    CommandPalette,
    /// 打开 / 收起主机工具（进程、服务、Docker、网络）
    HostTools,
    /// 打开主机工具的概览页，焦点进面板（键盘操作的入口；之后 Ctrl+PageUp / PageDown 切换各页，Esc 回到终端）
    HostToolsOverview,
    /// 打开新窗口
    NewWindow,
    /// 打开默认的本地终端
    NewLocal,
    /// 隐藏 / 显示侧边栏，终端占满窗口宽度
    ToggleSidebar,
    /// 全屏 / 退出全屏
    FullScreen,
    /// 放大界面（界面大小的下一档，所有窗口；与终端的字号无关）。默认没有快捷键：Ctrl+= / - / 0 给了终端的字号，
    /// Ctrl+Alt 组合在德语等键盘布局上是 AltGr，会挡住 { } [ ] 等字符的输入
    UiZoomIn,
    /// 缩小界面
    UiZoomOut,
    /// 界面大小恢复默认（跟随系统）
    UiZoomReset,
    /// 立即锁定（只在启用了应用锁时有效）
    Lock,
}

/// 动作的元数据。
struct Meta {
    id: &'static str,
    group: KeyGroup,
    scope: KeyScope,
    /// Windows、Linux 的默认值
    defaults: &'static [&'static str],
    /// macOS 的默认值
    mac: &'static [&'static str],
}

impl KeyAction {
    /// 按设置页中的顺序（与声明的顺序相同）。
    pub const ALL: [KeyAction; 38] = [
        KeyAction::Copy,
        KeyAction::Paste,
        KeyAction::Search,
        KeyAction::ZoomIn,
        KeyAction::ZoomOut,
        KeyAction::ZoomReset,
        KeyAction::Duplicate,
        KeyAction::SftpBeside,
        KeyAction::Compose,
        KeyAction::MultiExec,
        KeyAction::CommandPicker,
        KeyAction::Reconnect,
        KeyAction::NextTab,
        KeyAction::PrevTab,
        KeyAction::Tab1,
        KeyAction::Tab2,
        KeyAction::Tab3,
        KeyAction::Tab4,
        KeyAction::Tab5,
        KeyAction::Tab6,
        KeyAction::Tab7,
        KeyAction::Tab8,
        KeyAction::Tab9,
        KeyAction::MoveToNewWindow,
        KeyAction::FocusTabList,
        KeyAction::SplitRight,
        KeyAction::SplitDown,
        KeyAction::CommandPalette,
        KeyAction::HostTools,
        KeyAction::HostToolsOverview,
        KeyAction::NewWindow,
        KeyAction::NewLocal,
        KeyAction::ToggleSidebar,
        KeyAction::FullScreen,
        KeyAction::UiZoomIn,
        KeyAction::UiZoomOut,
        KeyAction::UiZoomReset,
        KeyAction::Lock,
    ];

    fn meta(self) -> Meta {
        use KeyGroup as G;
        use KeyScope as S;
        // 默认值中没有小键盘的写法（GPUI 不区分小键盘，见模块说明）。
        // Windows、Linux：复制、粘贴另有 Ctrl+Insert、Shift+Insert（与 Windows 控制台、Xshell 相同）；Ctrl+Tab 与浏览器、
        // Xshell、MobaXterm 相同；第 N 个 Tab 与 Windows Terminal 相同（Alt+数字留给远端）。
        // macOS：照 Terminal.app、iTerm2 的习惯（⌘T 新 Tab、⌘D / ⇧⌘D 分屏、⌘1～⌘9、⇧⌘[ ]、⌃⇥ 切换 Tab、⌥⌘I 同步输入），
        // 系统的标准写法（⌃⌘F 全屏、⌃⌘S 侧边栏），其余与 Windows 相同的 Ctrl+Shift+字母换成 ⇧⌘+字母。
        let (id, group, scope, defaults, mac): (
            _,
            _,
            _,
            &'static [&'static str],
            &'static [&'static str],
        ) = match self {
            Self::Copy => (
                "copy",
                G::Terminal,
                S::Terminal,
                &["ctrl-shift-c", "ctrl-insert"],
                &["cmd-c"],
            ),
            Self::Paste => (
                "paste",
                G::Terminal,
                S::Terminal,
                &["ctrl-shift-v", "shift-insert"],
                &["cmd-v"],
            ),
            Self::Search => (
                "search",
                G::Terminal,
                S::Terminal,
                &["ctrl-shift-f"],
                &["cmd-f"],
            ),
            Self::ZoomIn => (
                "zoomIn",
                G::Terminal,
                S::Terminal,
                &["ctrl-=", "ctrl-shift-="],
                &["cmd-=", "cmd-+"],
            ),
            Self::ZoomOut => ("zoomOut", G::Terminal, S::Terminal, &["ctrl--"], &["cmd--"]),
            Self::ZoomReset => (
                "zoomReset",
                G::Terminal,
                S::Terminal,
                &["ctrl-0"],
                &["cmd-0"],
            ),
            Self::Duplicate => (
                "duplicate",
                G::Terminal,
                S::Workspace,
                &["ctrl-shift-d"],
                &["cmd-shift-t"],
            ),
            Self::SftpBeside => ("sftpBeside", G::Terminal, S::Workspace, &[], &[]),
            Self::Compose => (
                "compose",
                G::Terminal,
                S::Workspace,
                &["ctrl-shift-m"],
                &["cmd-shift-m"],
            ),
            Self::MultiExec => (
                "multiExec",
                G::Terminal,
                S::Workspace,
                &["ctrl-shift-i"],
                &["alt-cmd-i"],
            ),
            Self::CommandPicker => (
                "commandPicker",
                G::Terminal,
                S::Workspace,
                &["ctrl-shift-space"],
                &["cmd-shift-space"],
            ),
            Self::Reconnect => (
                "reconnect",
                G::Terminal,
                S::Workspace,
                &["ctrl-shift-r"],
                &["cmd-shift-r"],
            ),
            Self::NextTab => (
                "nextTab",
                G::Tabs,
                S::Workspace,
                &["ctrl-tab"],
                &["cmd-}", "ctrl-tab"],
            ),
            Self::PrevTab => (
                "prevTab",
                G::Tabs,
                S::Workspace,
                &["ctrl-shift-tab"],
                &["cmd-{", "ctrl-shift-tab"],
            ),
            Self::Tab1 => ("tab1", G::Tabs, S::Workspace, &["ctrl-alt-1"], &["cmd-1"]),
            Self::Tab2 => ("tab2", G::Tabs, S::Workspace, &["ctrl-alt-2"], &["cmd-2"]),
            Self::Tab3 => ("tab3", G::Tabs, S::Workspace, &["ctrl-alt-3"], &["cmd-3"]),
            Self::Tab4 => ("tab4", G::Tabs, S::Workspace, &["ctrl-alt-4"], &["cmd-4"]),
            Self::Tab5 => ("tab5", G::Tabs, S::Workspace, &["ctrl-alt-5"], &["cmd-5"]),
            Self::Tab6 => ("tab6", G::Tabs, S::Workspace, &["ctrl-alt-6"], &["cmd-6"]),
            Self::Tab7 => ("tab7", G::Tabs, S::Workspace, &["ctrl-alt-7"], &["cmd-7"]),
            Self::Tab8 => ("tab8", G::Tabs, S::Workspace, &["ctrl-alt-8"], &["cmd-8"]),
            Self::Tab9 => ("tab9", G::Tabs, S::Workspace, &["ctrl-alt-9"], &["cmd-9"]),
            Self::MoveToNewWindow => ("moveToNewWindow", G::Tabs, S::Workspace, &[], &[]),
            // J：跳到（jump）Tab 列表；Ctrl+Shift+J、⇧⌘J 在系统、gpui-kit 与常见终端里都没有占用
            Self::FocusTabList => (
                "focusTabList",
                G::Tabs,
                S::Workspace,
                &["ctrl-shift-j"],
                &["cmd-shift-j"],
            ),
            Self::SplitRight => (
                "splitRight",
                G::Panes,
                S::Workspace,
                &["alt-shift-="],
                &["cmd-d"],
            ),
            Self::SplitDown => (
                "splitDown",
                G::Panes,
                S::Workspace,
                &["alt-shift--"],
                &["cmd-shift-d"],
            ),
            Self::CommandPalette => (
                "commandPalette",
                G::Window,
                S::Workspace,
                &["ctrl-shift-p"],
                &["cmd-shift-p"],
            ),
            Self::HostTools => (
                "hostTools",
                G::Window,
                S::Workspace,
                &["ctrl-shift-e"],
                &["cmd-shift-e"],
            ),
            Self::HostToolsOverview => (
                "hostToolsOverview",
                G::Window,
                S::Workspace,
                &["ctrl-shift-o"],
                &["cmd-shift-o"],
            ),
            Self::NewWindow => (
                "newWindow",
                G::Window,
                S::Workspace,
                &["ctrl-shift-n"],
                &["cmd-n"],
            ),
            Self::NewLocal => (
                "newLocal",
                G::Window,
                S::Workspace,
                &["ctrl-shift-t"],
                &["cmd-t"],
            ),
            Self::ToggleSidebar => (
                "toggleSidebar",
                G::Window,
                S::Workspace,
                &["ctrl-shift-b"],
                &["ctrl-cmd-s"],
            ),
            Self::FullScreen => (
                "fullScreen",
                G::Window,
                S::Workspace,
                &["f11"],
                &["ctrl-cmd-f"],
            ),
            Self::UiZoomIn => ("uiZoomIn", G::Window, S::Workspace, &[], &[]),
            Self::UiZoomOut => ("uiZoomOut", G::Window, S::Workspace, &[], &[]),
            Self::UiZoomReset => ("uiZoomReset", G::Window, S::Workspace, &[], &[]),
            Self::Lock => (
                "lock",
                G::Window,
                S::Workspace,
                &["ctrl-shift-l"],
                &["cmd-shift-l"],
            ),
        };
        Meta {
            id,
            group,
            scope,
            defaults,
            mac,
        }
    }

    /// 动作 ID：设置中的键，界面也用它选文案。
    pub fn id(self) -> &'static str {
        self.meta().id
    }

    pub fn group(self) -> KeyGroup {
        self.meta().group
    }

    pub fn scope(self) -> KeyScope {
        self.meta().scope
    }

    /// 这个系统上默认的快捷键（规范的写法），第一个显示在菜单与提示中。
    pub fn defaults(self, os: KeyOs) -> &'static [&'static str] {
        let meta = self.meta();
        if os.is_mac() { meta.mac } else { meta.defaults }
    }

    /// “切换到第 N 个 Tab”的 N - 1（从 0 开始）；其他动作为 `None`。
    pub fn tab_index(self) -> Option<usize> {
        const TABS: [KeyAction; 9] = [
            KeyAction::Tab1,
            KeyAction::Tab2,
            KeyAction::Tab3,
            KeyAction::Tab4,
            KeyAction::Tab5,
            KeyAction::Tab6,
            KeyAction::Tab7,
            KeyAction::Tab8,
            KeyAction::Tab9,
        ];
        TABS.iter().position(|a| *a == self)
    }

    fn default_combos(self, os: KeyOs) -> Vec<KeyCombo> {
        self.defaults(os)
            .iter()
            .filter_map(|text| KeyCombo::parse(text))
            .collect()
    }
}

/// 录制快捷键时界面传入的一次按键，取自 GPUI 的 `Keystroke`（本模块不依赖 GPUI）。
///
/// Windows 上 GPUI 报告的 Shift 加数字、符号键是 Shift 之后的字符（按 Ctrl+Shift+= 报告为 `ctrl-+`），先经键盘映射换回
/// 键面上的写法再传进来：`KeybindingKeystroke::new_with_mapper(keystroke, false, cx.keyboard_mapper().as_ref())`，
/// 取它的 `modifiers()` 与 `key()`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyInput<'a> {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// macOS 上的 ⌘，Windows、Linux 上的 Win / Super 键（GPUI 的 `Modifiers::platform`）
    pub platform: bool,
    /// GPUI 的键名（`Keystroke::key`）
    pub key: &'a str,
}

/// 一个按键组合，总是规范的写法：只能由 [`KeyCombo::parse`]、[`KeyCombo::from_input`] 得到。
/// `Display` 输出设置中保存、交给 GPUI 的写法（`ctrl-shift-c`），[`KeyCombo::label`] 是显示用的写法（Windows 上
/// `Ctrl+Shift+C`，macOS 上 `⇧⌘C`）。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    ctrl: bool,
    alt: bool,
    /// ⌘（GPUI 的 `cmd-`，只在 macOS 上可用）
    cmd: bool,
    shift: bool,
    /// GPUI 的键名：`c`、`1`、`=`、`f11`、`space`、`pageup`……
    key: String,
}

impl KeyCombo {
    /// 解析设置中的写法。语法与 GPUI 的 `Keystroke::parse` 相同：修饰键不分大小写、顺序不限，单独的大写字母表示加 Shift，
    /// 键名不分大小写。⌘ 写作 `cmd`（不认 `win`、`super`、`secondary`）。不认识的键名或修饰键、多个按键的序列返回 `None`。
    pub fn parse(text: &str) -> Option<Self> {
        if text.is_empty() || text.contains(char::is_whitespace) {
            return None;
        }
        // 键是 `-` 时写在末尾：`-`、`ctrl--`、`alt-shift--`
        let (modifiers, key) = match text.strip_suffix('-') {
            Some("") => (None, "-"),
            Some(rest) => match rest.strip_suffix('-')? {
                "" => (None, "-"),
                modifiers => (Some(modifiers), "-"),
            },
            None => match text.rsplit_once('-') {
                Some((modifiers, key)) => (Some(modifiers), key),
                None => (None, text),
            },
        };
        let mut combo = KeyCombo {
            ctrl: false,
            alt: false,
            cmd: false,
            shift: false,
            key: String::new(),
        };
        for part in modifiers.into_iter().flat_map(|m| m.split('-')) {
            if part.eq_ignore_ascii_case("ctrl") {
                combo.ctrl = true;
            } else if part.eq_ignore_ascii_case("alt") {
                combo.alt = true;
            } else if part.eq_ignore_ascii_case("cmd") {
                combo.cmd = true;
            } else if part.eq_ignore_ascii_case("shift") {
                combo.shift = true;
            } else {
                return None;
            }
        }
        if key.len() == 1 && key.as_bytes()[0].is_ascii_uppercase() {
            combo.shift = true;
        }
        combo.key = key.to_ascii_lowercase();
        valid_key(&combo.key).then_some(combo)
    }

    /// 录制时按下的键。只按了修饰键、或者是不能作为快捷键的键（Esc 等）时返回 `None`，界面继续等待；Windows、Linux
    /// 上带了 Win / Super 键（留给系统）时也是。是否允许（需要带 Ctrl 或 Alt 等）另由 [`KeyCombo::is_allowed`] 判断，
    /// 界面据此说明原因。
    pub fn from_input(input: &KeyInput<'_>, os: KeyOs) -> Option<Self> {
        if input.platform && !os.is_mac() {
            return None;
        }
        let key = input.key.to_ascii_lowercase();
        valid_key(&key).then_some(KeyCombo {
            ctrl: input.ctrl,
            alt: input.alt,
            cmd: input.platform,
            shift: input.shift,
            key,
        })
    }

    /// 在这个系统上能否作为快捷键。只带 Shift 加字母、数字等会让这些字符打不出来，不允许。
    /// - Windows、Linux：要带 Ctrl 或 Alt，或者是 F1–F24，或者是 Shift 加 Insert、Delete、Home、End、PageUp、PageDown
    ///   （与 ShellRS、Windows Terminal 的限制相同）；带 Win / Super 键的不行。
    /// - macOS：要带 ⌘ 或 Ctrl，或者是 F1–F24、Shift 加上面那几个键；只带 Option 的不行（Option 用来输入组合字符）。
    ///   Shift 加数字、符号键（`cmd-shift-]`）也不行：GPUI 报告为 Shift 之后的字符（`cmd-}`），这样写触发不了。
    pub fn is_allowed(&self, os: KeyOs) -> bool {
        let shift_key = self.shift && SHIFT_KEYS.contains(&self.key.as_str());
        let function = is_function_key(&self.key);
        if os.is_mac() {
            let mut chars = self.key.chars();
            let single = chars.next().is_some() && chars.next().is_none();
            let shifted_symbol =
                self.shift && single && !self.key.chars().all(|c| c.is_ascii_lowercase());
            !shifted_symbol && (self.cmd || self.ctrl || function || shift_key)
        } else {
            !self.cmd && (self.ctrl || self.alt || function || shift_key)
        }
    }

    /// 显示用的写法：Windows、Linux 上 `Ctrl+Shift+C`、`Shift+Page Up`（与 gpui-component 的 `Kbd` 在 Windows 上的写法
    /// 相同）；macOS 上照系统菜单的写法，修饰键按 ⌃⌥⇧⌘ 的顺序、不加分隔，特殊键用符号（`⇧⌘C`、`⌘↑`、`⌃⇥`）。
    pub fn label(&self, os: KeyOs) -> String {
        let mut label = String::new();
        if os.is_mac() {
            for (on, symbol) in [
                (self.ctrl, "⌃"),
                (self.alt, "⌥"),
                (self.shift, "⇧"),
                (self.cmd, "⌘"),
            ] {
                if on {
                    label.push_str(symbol);
                }
            }
            let symbol = match self.key.as_str() {
                "up" => "↑",
                "down" => "↓",
                "left" => "←",
                "right" => "→",
                "enter" => "↩",
                "tab" => "⇥",
                "backspace" => "⌫",
                "delete" => "⌦",
                "home" => "↖",
                "end" => "↘",
                "pageup" => "⇞",
                "pagedown" => "⇟",
                "space" => "Space",
                key => {
                    push_capitalized(&mut label, key);
                    return label;
                }
            };
            label.push_str(symbol);
            return label;
        }
        for (on, name) in [
            (self.ctrl, "Ctrl+"),
            (self.alt, "Alt+"),
            (self.shift, "Shift+"),
            (self.cmd, "Win+"),
        ] {
            if on {
                label.push_str(name);
            }
        }
        match self.key.as_str() {
            "pageup" => label.push_str("Page Up"),
            "pagedown" => label.push_str("Page Down"),
            key => push_capitalized(&mut label, key),
        }
        label
    }
}

/// 键名首字母大写后接在 `label` 后面（`c` → `C`、`f11` → `F11`、`insert` → `Insert`）。
fn push_capitalized(label: &mut String, key: &str) {
    let mut chars = key.chars();
    if let Some(first) = chars.next() {
        label.extend(first.to_uppercase());
        label.push_str(chars.as_str());
    }
}

/// 设置中的写法（`ctrl-alt-h`、`cmd->`）在这个系统上显示的写法；写法不对时原样返回。界面上列出固定的
/// 快捷键时用。
pub fn label_of(text: &str, os: KeyOs) -> String {
    KeyCombo::parse(text).map_or_else(|| text.to_string(), |combo| combo.label(os))
}

impl fmt::Display for KeyCombo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (on, name) in [
            (self.ctrl, "ctrl-"),
            (self.alt, "alt-"),
            (self.cmd, "cmd-"),
            (self.shift, "shift-"),
        ] {
            if on {
                f.write_str(name)?;
            }
        }
        f.write_str(&self.key)
    }
}

/// F1–F24（GPUI 在 Windows 上报告的 F 键）。
fn is_function_key(key: &str) -> bool {
    key.strip_prefix('f')
        .and_then(|n| {
            n.parse::<u8>()
                .ok()
                .filter(|v| (1..=24).contains(v) && v.to_string() == n)
        })
        .is_some()
}

/// 能作为快捷键的键名：一个字符（字母、数字、符号，不同键盘布局上的字符也算），或者有名字的键、F 键。
fn valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => !c.is_whitespace() && !c.is_control(),
        (Some(_), Some(_)) => NAMED_KEYS.contains(&key) || is_function_key(key),
        _ => false,
    }
}

/// 修改快捷键失败的原因。界面按变体选用文案（例如"{{key}} 已用于"{{action}}""），不直接显示这里的文字。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BindingError {
    /// 不能作为快捷键：需要带 Ctrl 或 Alt（macOS 上 ⌘ 或 Ctrl，见 [`KeyCombo::is_allowed`]）
    #[error("{0} 不能作为快捷键：需要带修饰键")]
    NeedsModifier(KeyCombo),
    /// 已用于另一个动作
    #[error("{combo} 已用于 {}", .action.id())]
    Conflict { combo: KeyCombo, action: KeyAction },
    /// 超过了 [`MAX_COMBOS`] 个
    #[error("每个动作最多 {MAX_COMBOS} 个快捷键")]
    TooMany,
}

/// 每个动作实际的快捷键：设置中改过的与默认值合并后的结果，由 [`effective`] 得到。
///
/// 一个组合只属于一个动作：先放改过的动作（按设置页中的顺序），默认值中已被占用的组合去掉
/// （例如新版本给某个动作加了默认按键，而用户早已把它用于别的动作）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keymap {
    /// 按 [`KeyAction::ALL`] 的顺序
    combos: Vec<Vec<KeyCombo>>,
    customized: Vec<bool>,
}

impl Keymap {
    /// 这个系统上全部用默认值。
    pub fn defaults(os: KeyOs) -> Self {
        effective(&KeybindingSettings::default(), os)
    }

    /// 动作的快捷键，第一个显示在菜单与提示中；为空表示停用。
    pub fn combos(&self, action: KeyAction) -> &[KeyCombo] {
        &self.combos[action as usize]
    }

    /// 设置中改过这个动作（与默认值不同），设置页据此显示"恢复默认"。
    pub fn is_customized(&self, action: KeyAction) -> bool {
        self.customized[action as usize]
    }

    /// 全部动作与快捷键，按设置页中的顺序（界面据此注册 GPUI 的快捷键）。
    pub fn iter(&self) -> impl Iterator<Item = (KeyAction, &[KeyCombo])> + '_ {
        KeyAction::ALL.into_iter().map(|a| (a, self.combos(a)))
    }

    /// 组合已被哪个动作占用（不算 `except` 自己）。
    pub fn conflict(&self, combo: &KeyCombo, except: KeyAction) -> Option<KeyAction> {
        self.iter()
            .find(|(action, combos)| *action != except && combos.contains(combo))
            .map(|(action, _)| action)
    }
}

/// 这个系统用的那份改过的快捷键：macOS 用 `mac_bindings`，Windows 与 Linux 共用 `bindings`。
pub fn bindings_of(settings: &KeybindingSettings, os: KeyOs) -> &BTreeMap<String, Vec<String>> {
    if os.is_mac() {
        &settings.mac_bindings
    } else {
        &settings.bindings
    }
}

fn bindings_mut(
    settings: &mut KeybindingSettings,
    os: KeyOs,
) -> &mut BTreeMap<String, Vec<String>> {
    if os.is_mac() {
        &mut settings.mac_bindings
    } else {
        &mut settings.bindings
    }
}

/// 合并这个系统的那份改过的快捷键（[`bindings_of`]）与这个系统的默认值。不认识的动作、写法不对或在这个系统上不允许
/// 的组合忽略；同一个动作中重复的只留一个，最多取 [`MAX_COMBOS`] 个。改过的动作保存的是空列表时表示停用；保存了组合、
/// 但一个也用不了（写法不对，或者是更新版本的 Termlet 才认识的按键）时用默认值，不当作停用。
pub fn effective(settings: &KeybindingSettings, os: KeyOs) -> Keymap {
    // 与默认值相同的视为没有改过
    let stored: Vec<Option<Vec<KeyCombo>>> = KeyAction::ALL
        .iter()
        .map(|a| {
            let texts = bindings_of(settings, os).get(a.id())?;
            let list = parse_list(texts, os);
            if list.is_empty() && !texts.is_empty() {
                return None;
            }
            (list != a.default_combos(os)).then_some(list)
        })
        .collect();
    let mut taken = HashSet::new();
    let mut combos = vec![Vec::new(); KeyAction::ALL.len()];
    for customized in [true, false] {
        for (i, action) in KeyAction::ALL.into_iter().enumerate() {
            if stored[i].is_some() != customized {
                continue;
            }
            let mut list = stored[i]
                .clone()
                .unwrap_or_else(|| action.default_combos(os));
            list.retain(|c| taken.insert(c.clone()));
            combos[i] = list;
        }
    }
    Keymap {
        combos,
        customized: stored.iter().map(Option::is_some).collect(),
    }
}

fn parse_list(texts: &[String], os: KeyOs) -> Vec<KeyCombo> {
    let mut list: Vec<KeyCombo> = Vec::new();
    for combo in texts
        .iter()
        .filter_map(|text| KeyCombo::parse(text))
        .filter(|combo| combo.is_allowed(os))
    {
        if list.len() < MAX_COMBOS && !list.contains(&combo) {
            list.push(combo);
        }
    }
    list
}

/// 给动作加一个快捷键（录制到的按键），返回新的设置；已经有这个组合时设置不变。
/// 检查的顺序：先看是否允许，再看是否已用于别的动作，最后看个数。
pub fn add_binding(
    settings: &KeybindingSettings,
    action: KeyAction,
    combo: KeyCombo,
    os: KeyOs,
) -> Result<KeybindingSettings, BindingError> {
    let keymap = effective(settings, os);
    check(&keymap, action, &combo, os)?;
    let current = keymap.combos(action);
    if current.contains(&combo) {
        return Ok(settings.clone());
    }
    if current.len() >= MAX_COMBOS {
        return Err(BindingError::TooMany);
    }
    let mut list = current.to_vec();
    list.push(combo);
    Ok(store(settings, action, &list, os))
}

/// 去掉动作的一个快捷键，返回新的设置；去掉最后一个即停用这个动作。
pub fn remove_binding(
    settings: &KeybindingSettings,
    action: KeyAction,
    combo: &KeyCombo,
    os: KeyOs,
) -> KeybindingSettings {
    let mut list = effective(settings, os).combos(action).to_vec();
    list.retain(|c| c != combo);
    store(settings, action, &list, os)
}

/// 一个动作在这个系统上恢复默认（另一种系统的那份不动）。
pub fn reset_binding(
    settings: &KeybindingSettings,
    action: KeyAction,
    os: KeyOs,
) -> KeybindingSettings {
    let mut next = settings.clone();
    bindings_mut(&mut next, os).remove(action.id());
    next
}

/// 这个系统上全部恢复默认：清空这个系统的那份改过的快捷键（包括不认识的动作），另一种系统的那份不动。
pub fn reset_all(settings: &KeybindingSettings, os: KeyOs) -> KeybindingSettings {
    let mut next = settings.clone();
    bindings_mut(&mut next, os).clear();
    next
}

/// 键位方案：照搬别的客户端的习惯，从它们换过来的用户不用重新记快捷键。都是 Windows 上的客户端，macOS 上不提供
/// （[`KeyPreset::available`]）。
///
/// 只改有对应功能的动作，其余用 Termlet 的默认值；照搬的按键放在第一个（菜单与提示中显示它），
/// Termlet 原来的按键能保留的也留着。按键来源：Xshell 见 NetSarang 的帮助“已定义的快捷键”与多篇中文教程
/// （Alt+数字切换 Tab、Ctrl+Tab、Alt+I 撰写栏、Alt+Enter 全屏、Ctrl+Alt+F 打开文件传输）；MobaXterm 见官方文档
/// 的键盘快捷键一节（Ctrl+Tab、Ctrl+Alt+←/→、Ctrl+Alt+F1～F9、Ctrl+Alt+T 新 Tab、Ctrl+Shift+U 复制 Tab）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyPreset {
    Xshell,
    MobaXterm,
}

impl KeyPreset {
    pub const ALL: [KeyPreset; 2] = [KeyPreset::Xshell, KeyPreset::MobaXterm];

    pub fn id(self) -> &'static str {
        match self {
            KeyPreset::Xshell => "xshell",
            KeyPreset::MobaXterm => "mobaxterm",
        }
    }

    /// 这个系统上提供不提供键位方案：方案照搬的是 Windows 客户端的 Ctrl、Alt 组合，macOS 上不提供。
    pub fn available(os: KeyOs) -> bool {
        !os.is_mac()
    }

    /// 这个方案改动的动作与它们的快捷键。
    fn overrides(self) -> &'static [(KeyAction, &'static [&'static str])] {
        use KeyAction as A;
        match self {
            KeyPreset::Xshell => &[
                (A::SftpBeside, &["ctrl-alt-f"]),
                (A::Compose, &["alt-i", "ctrl-shift-m"]),
                (A::Tab1, &["alt-1", "ctrl-alt-1"]),
                (A::Tab2, &["alt-2", "ctrl-alt-2"]),
                (A::Tab3, &["alt-3", "ctrl-alt-3"]),
                (A::Tab4, &["alt-4", "ctrl-alt-4"]),
                (A::Tab5, &["alt-5", "ctrl-alt-5"]),
                (A::Tab6, &["alt-6", "ctrl-alt-6"]),
                (A::Tab7, &["alt-7", "ctrl-alt-7"]),
                (A::Tab8, &["alt-8", "ctrl-alt-8"]),
                (A::Tab9, &["alt-9", "ctrl-alt-9"]),
                (A::FullScreen, &["alt-enter", "f11"]),
            ],
            KeyPreset::MobaXterm => &[
                (A::Duplicate, &["ctrl-shift-u", "ctrl-shift-d"]),
                (A::NextTab, &["ctrl-tab", "ctrl-alt-right"]),
                (A::PrevTab, &["ctrl-shift-tab", "ctrl-alt-left"]),
                (A::Tab1, &["ctrl-alt-f1", "ctrl-alt-1"]),
                (A::Tab2, &["ctrl-alt-f2", "ctrl-alt-2"]),
                (A::Tab3, &["ctrl-alt-f3", "ctrl-alt-3"]),
                (A::Tab4, &["ctrl-alt-f4", "ctrl-alt-4"]),
                (A::Tab5, &["ctrl-alt-f5", "ctrl-alt-5"]),
                (A::Tab6, &["ctrl-alt-f6", "ctrl-alt-6"]),
                (A::Tab7, &["ctrl-alt-f7", "ctrl-alt-7"]),
                (A::Tab8, &["ctrl-alt-f8", "ctrl-alt-8"]),
                (A::Tab9, &["ctrl-alt-f9", "ctrl-alt-9"]),
                (A::NewLocal, &["ctrl-alt-t", "ctrl-shift-t"]),
            ],
        }
    }
}

/// 换成键位方案（只在 Windows、Linux 上提供，改的是这两个系统共用的那份）：先全部恢复默认（与“全部恢复默认”相同，
/// 不认识的动作也清掉），再按方案改；macOS 的那份不动。
pub fn apply_preset(settings: &KeybindingSettings, preset: KeyPreset) -> KeybindingSettings {
    let mut next = reset_all(settings, KeyOs::Windows);
    for (action, combos) in preset.overrides() {
        let list: Vec<KeyCombo> = combos.iter().filter_map(|c| KeyCombo::parse(c)).collect();
        next = store(&next, *action, &list, KeyOs::Windows);
    }
    next
}

fn check(
    keymap: &Keymap,
    action: KeyAction,
    combo: &KeyCombo,
    os: KeyOs,
) -> Result<(), BindingError> {
    if !combo.is_allowed(os) {
        return Err(BindingError::NeedsModifier(combo.clone()));
    }
    match keymap.conflict(combo, action) {
        Some(other) => Err(BindingError::Conflict {
            combo: combo.clone(),
            action: other,
        }),
        None => Ok(()),
    }
}

/// 把一个动作的快捷键存进这个系统的那份：与这个系统的默认值相同时去掉这一项（只保存改过的）；其余的原样保留，
/// 包括不认识的动作（留给更新版本的 Termlet，同步时不丢）与另一种系统的那份。
fn store(
    settings: &KeybindingSettings,
    action: KeyAction,
    list: &[KeyCombo],
    os: KeyOs,
) -> KeybindingSettings {
    let mut next = settings.clone();
    let bindings = bindings_mut(&mut next, os);
    if list == action.default_combos(os).as_slice() {
        bindings.remove(action.id());
    } else {
        bindings.insert(
            action.id().to_string(),
            list.iter().map(KeyCombo::to_string).collect(),
        );
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    fn combo(text: &str) -> KeyCombo {
        KeyCombo::parse(text).unwrap_or_else(|| panic!("应能解析 {text}"))
    }

    fn normalized(text: &str) -> Option<String> {
        KeyCombo::parse(text).map(|c| c.to_string())
    }

    fn settings(entries: &[(&str, &[&str])]) -> KeybindingSettings {
        KeybindingSettings {
            bindings: entries
                .iter()
                .map(|(action, combos)| {
                    (
                        action.to_string(),
                        combos.iter().map(|c| c.to_string()).collect(),
                    )
                })
                .collect(),
            ..Default::default()
        }
    }

    fn texts(combos: &[KeyCombo]) -> Vec<String> {
        combos.iter().map(KeyCombo::to_string).collect()
    }

    /// 默认没有快捷键的动作：在旁边打开 SFTP（留给键位方案）、把 Tab 移到新窗口、界面大小的三个
    /// （见 [`KeyAction::UiZoomIn`]）。
    fn without_default(action: KeyAction) -> bool {
        matches!(
            action,
            KeyAction::SftpBeside
                | KeyAction::MoveToNewWindow
                | KeyAction::UiZoomIn
                | KeyAction::UiZoomOut
                | KeyAction::UiZoomReset
        )
    }

    fn input(key: &str) -> KeyInput<'_> {
        KeyInput {
            ctrl: false,
            alt: false,
            shift: false,
            platform: false,
            key,
        }
    }

    #[test]
    fn parse_normalizes_like_gpui() {
        for text in [
            "ctrl-shift-c",
            "ctrl-insert",
            "shift-insert",
            "alt-shift-=",
            "alt-shift--",
            "ctrl--",
            "ctrl-0",
            "f11",
            "ctrl-shift-space",
            "ctrl-alt-shift-pagedown",
            "ctrl-\\",
            "ctrl-`",
            "alt-up",
            "-",
        ] {
            assert_eq!(normalized(text).as_deref(), Some(text));
        }
        // 修饰键不分大小写、顺序不限，统一为 ctrl-alt-shift；单独的大写字母表示加 Shift；键名不分大小写
        assert_eq!(normalized("Shift-Ctrl-C").as_deref(), Some("ctrl-shift-c"));
        assert_eq!(normalized("ctrl-C").as_deref(), Some("ctrl-shift-c"));
        assert_eq!(
            normalized("shift-alt-ctrl-k").as_deref(),
            Some("ctrl-alt-shift-k")
        );
        assert_eq!(normalized("F11").as_deref(), Some("f11"));
        assert_eq!(normalized("Shift-PageUp").as_deref(), Some("shift-pageup"));
        assert_eq!(normalized("ctrl-ctrl-k").as_deref(), Some("ctrl-k"));
        assert_eq!(normalized("--").as_deref(), Some("-"));
        // 其他键盘布局上的字符
        assert_eq!(normalized("ctrl-ü").as_deref(), Some("ctrl-ü"));
        let c = combo("alt-shift--");
        assert!(c.alt && c.shift && !c.ctrl);
        assert_eq!(c.key, "-");
    }

    #[test]
    fn parse_rejects_unknown_keys_and_modifiers() {
        for text in [
            "",
            "ctrl",
            "shift",
            "ctrl-",
            "-c",
            "ctrl--c",
            "ctrl-shift-",
            "win-d",
            "super-c",
            "fn-f1",
            "secondary-c",
            "ctrl-k ctrl-s",
            " ctrl-k",
            "ctrl-escape",
            "ctrl-menu",
            "ctrl-numadd",
            "f0",
            "f25",
            "f01",
            "f+1",
            "ctrl-v->x",
            "Ctrl+Shift+C",
        ] {
            assert_eq!(normalized(text), None, "{text}");
        }
    }

    #[test]
    fn input_from_keystroke() {
        fn ctrl_shift(key: &str) -> KeyInput<'_> {
            KeyInput {
                ctrl: true,
                shift: true,
                ..input(key)
            }
        }
        let c = KeyCombo::from_input(&ctrl_shift("c"), KeyOs::Windows).unwrap();
        assert_eq!(c.to_string(), "ctrl-shift-c");
        assert_eq!(
            KeyCombo::from_input(&ctrl_shift("="), KeyOs::Windows)
                .unwrap()
                .to_string(),
            "ctrl-shift-="
        );
        assert_eq!(
            KeyCombo::from_input(&ctrl_shift("space"), KeyOs::Windows)
                .unwrap()
                .to_string(),
            "ctrl-shift-space"
        );
        let shift_insert = KeyInput {
            shift: true,
            ..input("insert")
        };
        assert_eq!(
            KeyCombo::from_input(&shift_insert, KeyOs::Windows)
                .unwrap()
                .to_string(),
            "shift-insert"
        );
        assert_eq!(
            KeyCombo::from_input(&input("f11"), KeyOs::Windows)
                .unwrap()
                .to_string(),
            "f11"
        );
        // 不允许的组合照样返回，由 is_allowed 判断
        assert!(
            !KeyCombo::from_input(&input("k"), KeyOs::Windows)
                .unwrap()
                .is_allowed(KeyOs::Windows)
        );
        // Win 键、单独的修饰键、Esc 等不算
        let win = KeyInput {
            platform: true,
            ..ctrl_shift("d")
        };
        assert_eq!(KeyCombo::from_input(&win, KeyOs::Windows), None);
        for key in ["shift", "control", "alt", "platform", "escape", "menu", ""] {
            assert_eq!(
                KeyCombo::from_input(&ctrl_shift(key), KeyOs::Windows),
                None,
                "{key}"
            );
        }
    }

    #[test]
    fn labels() {
        assert_eq!(combo("ctrl-shift-c").label(KeyOs::Windows), "Ctrl+Shift+C");
        assert_eq!(combo("shift-insert").label(KeyOs::Windows), "Shift+Insert");
        assert_eq!(
            combo("ctrl-alt-shift-pagedown").label(KeyOs::Windows),
            "Ctrl+Alt+Shift+Page Down"
        );
        assert_eq!(combo("f11").label(KeyOs::Windows), "F11");
        assert_eq!(
            combo("ctrl-shift-space").label(KeyOs::Windows),
            "Ctrl+Shift+Space"
        );
        assert_eq!(combo("alt-shift--").label(KeyOs::Windows), "Alt+Shift+-");
        assert_eq!(combo("ctrl-=").label(KeyOs::Windows), "Ctrl+=");
        assert_eq!(combo("alt-left").label(KeyOs::Windows), "Alt+Left");
        assert_eq!(combo("ctrl-ü").label(KeyOs::Windows), "Ctrl+Ü");
    }

    #[test]
    fn allowed_combos() {
        for text in [
            "ctrl-k",
            "alt-1",
            "f5",
            "shift-f5",
            "shift-insert",
            "shift-pagedown",
        ] {
            assert!(combo(text).is_allowed(KeyOs::Windows), "{text}");
        }
        for text in [
            "k",
            "shift-a",
            "shift-up",
            "insert",
            "shift-space",
            "shift-enter",
        ] {
            assert!(!combo(text).is_allowed(KeyOs::Windows), "{text}");
        }
    }

    #[test]
    fn actions_table() {
        let mut ids = HashSet::new();
        for (i, action) in KeyAction::ALL.into_iter().enumerate() {
            assert_eq!(action as usize, i, "ALL 的顺序与声明的顺序相同");
            assert!(ids.insert(action.id()), "动作 ID 不重复：{}", action.id());
            assert_eq!(
                action.defaults(KeyOs::Windows).is_empty(),
                without_default(action),
                "{}",
                action.id()
            );
            assert!(action.defaults(KeyOs::Windows).len() <= MAX_COMBOS);
            for text in action.defaults(KeyOs::Windows) {
                let c = combo(text);
                assert_eq!(c.to_string(), *text, "默认值是规范的写法");
                assert!(c.is_allowed(KeyOs::Windows), "{text}");
            }
        }
        // 默认值之间没有冲突
        let defaults = Keymap::defaults(KeyOs::Windows);
        for action in KeyAction::ALL {
            assert_eq!(
                texts(defaults.combos(action)),
                action.defaults(KeyOs::Windows),
                "{}",
                action.id()
            );
            assert!(!defaults.is_customized(action));
        }
        // 各分组中动作的顺序（设置页按这个顺序列出）
        let ids = |group: KeyGroup| group.actions().map(KeyAction::id).collect::<Vec<_>>();
        assert_eq!(
            ids(KeyGroup::Terminal),
            [
                "copy",
                "paste",
                "search",
                "zoomIn",
                "zoomOut",
                "zoomReset",
                "duplicate",
                "sftpBeside",
                "compose",
                "multiExec",
                "commandPicker",
                "reconnect"
            ]
        );
        assert_eq!(
            ids(KeyGroup::Tabs),
            [
                "nextTab",
                "prevTab",
                "tab1",
                "tab2",
                "tab3",
                "tab4",
                "tab5",
                "tab6",
                "tab7",
                "tab8",
                "tab9",
                "moveToNewWindow",
                "focusTabList"
            ]
        );
        assert_eq!(ids(KeyGroup::Panes), ["splitRight", "splitDown"]);
        assert_eq!(
            ids(KeyGroup::Window),
            [
                "commandPalette",
                "hostTools",
                "hostToolsOverview",
                "newWindow",
                "newLocal",
                "toggleSidebar",
                "fullScreen",
                "uiZoomIn",
                "uiZoomOut",
                "uiZoomReset",
                "lock"
            ]
        );
        assert_eq!(
            KeyGroup::ALL.map(KeyGroup::id),
            ["terminal", "tabs", "panes", "window"]
        );
        assert_eq!(KeyAction::Tab1.tab_index(), Some(0));
        assert_eq!(KeyAction::Tab9.tab_index(), Some(8));
        assert_eq!(KeyAction::NextTab.tab_index(), None);
        // 只在终端获得焦点时生效的：复制、粘贴、搜索、字号
        let terminal: Vec<_> = KeyAction::ALL
            .into_iter()
            .filter(|a| a.scope() == KeyScope::Terminal)
            .map(KeyAction::id)
            .collect();
        assert_eq!(
            terminal,
            ["copy", "paste", "search", "zoomIn", "zoomOut", "zoomReset"]
        );
        assert_eq!(
            KeyAction::CommandPicker.defaults(KeyOs::Windows),
            ["ctrl-shift-space"]
        );
        assert_eq!(
            KeyAction::HostTools.defaults(KeyOs::Windows),
            ["ctrl-shift-e"]
        );
        assert_eq!(
            KeyAction::HostToolsOverview.defaults(KeyOs::Windows),
            ["ctrl-shift-o"]
        );
        assert_eq!(
            KeyAction::HostToolsOverview.defaults(KeyOs::Mac),
            ["cmd-shift-o"]
        );
        assert_eq!(KeyAction::Lock.defaults(KeyOs::Windows), ["ctrl-shift-l"]);
    }

    #[test]
    fn effective_merges_overrides_with_defaults() {
        let keymap = effective(
            &settings(&[
                ("paste", &["shift-insert"]),
                ("lock", &[]),
                ("unknown", &["f1"]),
            ]),
            KeyOs::Windows,
        );
        assert_eq!(texts(keymap.combos(KeyAction::Paste)), ["shift-insert"]);
        assert!(keymap.is_customized(KeyAction::Paste));
        assert!(keymap.combos(KeyAction::Lock).is_empty(), "空列表表示停用");
        assert!(keymap.is_customized(KeyAction::Lock));
        assert_eq!(
            texts(keymap.combos(KeyAction::Copy)),
            ["ctrl-shift-c", "ctrl-insert"]
        );
        assert!(!keymap.is_customized(KeyAction::Copy));
        // 不认识的动作被忽略：f1 不属于任何动作
        assert_eq!(keymap.conflict(&combo("f1"), KeyAction::Copy), None);
    }

    #[test]
    fn effective_ignores_invalid_combos() {
        let keymap = effective(
            &settings(&[
                (
                    "search",
                    &[
                        "k",
                        "ctrl-numadd",
                        "Ctrl+Shift+F",
                        "Shift-Ctrl-G",
                        "ctrl-shift-g",
                    ],
                ),
                (
                    "compose",
                    &["ctrl-1", "ctrl-2", "ctrl-3", "ctrl-4", "ctrl-5"],
                ),
                ("multiExec", &["win-i"]),
            ]),
            KeyOs::Windows,
        );
        // 写法不对、不允许的去掉，规范化后重复的只留一个
        assert_eq!(texts(keymap.combos(KeyAction::Search)), ["ctrl-shift-g"]);
        assert_eq!(
            texts(keymap.combos(KeyAction::Compose)),
            ["ctrl-1", "ctrl-2", "ctrl-3", "ctrl-4"]
        );
        // 保存了组合但一个也用不了（写法不对、另一种系统上的）：用默认值；保存空列表才是停用
        assert_eq!(texts(keymap.combos(KeyAction::MultiExec)), ["ctrl-shift-i"]);
        assert!(!keymap.is_customized(KeyAction::MultiExec));
        // 与默认值相同的视为没有改过
        let same = effective(
            &settings(&[("copy", &["Ctrl-Shift-C", "ctrl-insert"])]),
            KeyOs::Windows,
        );
        assert!(!same.is_customized(KeyAction::Copy));
    }

    #[test]
    fn customized_actions_take_combos_from_defaults() {
        // 用户把 Ctrl+Shift+P 给了复制：命令面板的默认值中去掉它
        let keymap = effective(&settings(&[("copy", &["ctrl-shift-p"])]), KeyOs::Windows);
        assert_eq!(texts(keymap.combos(KeyAction::Copy)), ["ctrl-shift-p"]);
        assert!(keymap.combos(KeyAction::CommandPalette).is_empty());
        assert!(!keymap.is_customized(KeyAction::CommandPalette));
        assert_eq!(
            keymap.conflict(&combo("ctrl-shift-p"), KeyAction::CommandPalette),
            Some(KeyAction::Copy)
        );
        // 两个改过的动作用了同一个组合：按设置页中的顺序，前面的得到
        let keymap = effective(
            &settings(&[("lock", &["ctrl-k"]), ("paste", &["ctrl-k"])]),
            KeyOs::Windows,
        );
        assert_eq!(texts(keymap.combos(KeyAction::Paste)), ["ctrl-k"]);
        assert!(keymap.combos(KeyAction::Lock).is_empty());
    }

    #[test]
    fn conflicts() {
        let keymap = Keymap::defaults(KeyOs::Windows);
        assert_eq!(
            keymap.conflict(&combo("ctrl-shift-c"), KeyAction::Paste),
            Some(KeyAction::Copy)
        );
        assert_eq!(
            keymap.conflict(&combo("ctrl-shift-c"), KeyAction::Copy),
            None
        );
        assert_eq!(keymap.conflict(&combo("ctrl-alt-k"), KeyAction::Copy), None);
        let base = KeybindingSettings::default();
        assert_eq!(
            add_binding(
                &base,
                KeyAction::Paste,
                combo("ctrl-shift-c"),
                KeyOs::Windows
            ),
            Err(BindingError::Conflict {
                combo: combo("ctrl-shift-c"),
                action: KeyAction::Copy
            })
        );
    }

    #[test]
    fn add_checks_allowed_conflict_and_count() {
        let base = KeybindingSettings::default();
        assert_eq!(
            add_binding(&base, KeyAction::Copy, combo("shift-a"), KeyOs::Windows),
            Err(BindingError::NeedsModifier(combo("shift-a")))
        );
        // 已经有这个组合：设置不变
        assert_eq!(
            add_binding(&base, KeyAction::Copy, combo("ctrl-insert"), KeyOs::Windows),
            Ok(base.clone())
        );
        let next =
            add_binding(&base, KeyAction::Copy, combo("ctrl-alt-c"), KeyOs::Windows).unwrap();
        assert_eq!(
            next.bindings,
            settings(&[("copy", &["ctrl-shift-c", "ctrl-insert", "ctrl-alt-c"])]).bindings
        );
        let next = add_binding(&next, KeyAction::Copy, combo("f9"), KeyOs::Windows).unwrap();
        assert_eq!(
            effective(&next, KeyOs::Windows)
                .combos(KeyAction::Copy)
                .len(),
            MAX_COMBOS
        );
        assert_eq!(
            add_binding(&next, KeyAction::Copy, combo("f10"), KeyOs::Windows),
            Err(BindingError::TooMany)
        );
        // 不允许的先于冲突、个数
        assert_eq!(
            add_binding(&next, KeyAction::Copy, combo("k"), KeyOs::Windows),
            Err(BindingError::NeedsModifier(combo("k")))
        );
    }

    #[test]
    fn saving_keeps_only_differences() {
        let mut base = settings(&[("unknown", &["f1"])]);
        base.extra.insert("future".into(), serde_json::json!(true));
        let next = remove_binding(
            &base,
            KeyAction::Paste,
            &combo("ctrl-shift-v"),
            KeyOs::Windows,
        );
        assert_eq!(
            next.bindings,
            settings(&[("paste", &["shift-insert"]), ("unknown", &["f1"])]).bindings,
            "不认识的动作原样保留"
        );
        assert_eq!(next.extra, base.extra);
        // 改回默认值时去掉这一项
        let added = add_binding(
            &next,
            KeyAction::Paste,
            combo("ctrl-shift-v"),
            KeyOs::Windows,
        )
        .unwrap();
        assert_eq!(
            added.bindings,
            settings(&[
                ("paste", &["shift-insert", "ctrl-shift-v"]),
                ("unknown", &["f1"])
            ])
            .bindings,
            "顺序不同算改过"
        );
        let back = remove_binding(
            &added,
            KeyAction::Paste,
            &combo("shift-insert"),
            KeyOs::Windows,
        );
        let back = add_binding(
            &back,
            KeyAction::Paste,
            combo("shift-insert"),
            KeyOs::Windows,
        )
        .unwrap();
        assert_eq!(back.bindings, base.bindings);
        // 去掉组合；去掉最后一个即停用
        let removed = remove_binding(
            &base,
            KeyAction::Copy,
            &combo("ctrl-insert"),
            KeyOs::Windows,
        );
        assert_eq!(removed.bindings["copy"], ["ctrl-shift-c"]);
        let disabled = remove_binding(
            &removed,
            KeyAction::Copy,
            &combo("ctrl-shift-c"),
            KeyOs::Windows,
        );
        assert!(disabled.bindings["copy"].is_empty());
        assert!(
            effective(&disabled, KeyOs::Windows)
                .combos(KeyAction::Copy)
                .is_empty()
        );
        // 恢复默认
        let reset = reset_binding(&disabled, KeyAction::Copy, KeyOs::Windows);
        assert_eq!(reset.bindings, base.bindings);
        assert_eq!(
            texts(effective(&reset, KeyOs::Windows).combos(KeyAction::Copy)),
            ["ctrl-shift-c", "ctrl-insert"]
        );
        let all = reset_all(&next, KeyOs::Windows);
        assert!(all.bindings.is_empty());
        assert_eq!(all.extra, base.extra);
        assert_eq!(
            effective(&all, KeyOs::Windows),
            Keymap::defaults(KeyOs::Windows)
        );
    }

    #[test]
    fn presets_apply_without_conflicts() {
        // 先有一些自定义（含不认识的动作）：换方案时全部清掉
        let base = settings(&[("copy", &["ctrl-alt-c"]), ("future", &["ctrl-alt-z"])]);
        for preset in KeyPreset::ALL {
            let applied = apply_preset(&base, preset);
            assert!(!applied.bindings.contains_key("future"), "{}", preset.id());
            assert!(!applied.bindings.contains_key("copy"), "{}", preset.id());
            let keymap = effective(&applied, KeyOs::Windows);
            for (action, combos) in preset.overrides() {
                // 方案的按键都是规范、允许的写法，而且都生效（没有因冲突被去掉）
                for text in *combos {
                    let c = combo(text);
                    assert_eq!(c.to_string(), *text);
                    assert!(c.is_allowed(KeyOs::Windows), "{text}");
                }
                assert_eq!(
                    texts(keymap.combos(*action)),
                    *combos,
                    "{} {}",
                    preset.id(),
                    action.id()
                );
            }
            // 没有一个组合属于两个动作；没被方案改的动作保留全部默认值
            let mut seen = HashSet::new();
            for (action, combos) in keymap.iter() {
                for c in combos {
                    assert!(seen.insert(c.clone()), "{} {c}", preset.id());
                }
                if !preset.overrides().iter().any(|(a, _)| *a == action) {
                    assert_eq!(
                        texts(combos),
                        action.defaults(KeyOs::Windows),
                        "{}",
                        action.id()
                    );
                }
            }
            // 再换一次结果相同
            assert_eq!(apply_preset(&applied, preset), applied);
        }
        let xshell = effective(&apply_preset(&base, KeyPreset::Xshell), KeyOs::Windows);
        assert_eq!(
            texts(xshell.combos(KeyAction::Tab1)),
            ["alt-1", "ctrl-alt-1"]
        );
        assert_eq!(texts(xshell.combos(KeyAction::SftpBeside)), ["ctrl-alt-f"]);
        let moba = effective(&apply_preset(&base, KeyPreset::MobaXterm), KeyOs::Windows);
        assert_eq!(
            texts(moba.combos(KeyAction::NextTab)),
            ["ctrl-tab", "ctrl-alt-right"]
        );
        assert_eq!(texts(moba.combos(KeyAction::Tab6))[0], "ctrl-alt-f6");
    }

    // ⌘ 写作 cmd：与其他修饰键一样不分大小写、顺序不限，规范为 ctrl-alt-cmd-shift 的顺序（与 GPUI 的 unparse 相同）
    #[test]
    fn parse_cmd() {
        for text in [
            "cmd-c",
            "cmd-shift-[",
            "cmd-}",
            "cmd->",
            "cmd-+",
            "alt-cmd-i",
            "ctrl-cmd-f",
            "cmd-=",
            "cmd-,",
        ] {
            assert_eq!(normalized(text).as_deref(), Some(text));
        }
        assert_eq!(normalized("Shift-Cmd-]").as_deref(), Some("cmd-shift-]"));
        assert_eq!(
            normalized("shift-cmd-alt-ctrl-k").as_deref(),
            Some("ctrl-alt-cmd-shift-k")
        );
        assert_eq!(normalized("cmd-K").as_deref(), Some("cmd-shift-k"));
    }

    // 录制：macOS 上 ⌘ 参与的组合可以录入；Windows、Linux 上带 Win / Super 键的不算
    #[test]
    fn input_with_platform_key() {
        let cmd_shift = |key| KeyInput {
            platform: true,
            shift: true,
            ..input(key)
        };
        assert_eq!(
            KeyCombo::from_input(&cmd_shift("d"), KeyOs::Mac).map(|c| c.to_string()),
            Some("cmd-shift-d".into())
        );
        for os in [KeyOs::Windows, KeyOs::Linux] {
            assert_eq!(KeyCombo::from_input(&cmd_shift("d"), os), None, "{os:?}");
        }
        // 只按了 ⌘ 不算
        assert_eq!(
            KeyCombo::from_input(&cmd_shift("platform"), KeyOs::Mac),
            None
        );
    }

    #[test]
    fn mac_labels_use_symbols() {
        let mac = |text| combo(text).label(KeyOs::Mac);
        assert_eq!(mac("cmd-c"), "⌘C");
        assert_eq!(mac("cmd-shift-c"), "⇧⌘C");
        assert_eq!(mac("alt-cmd-i"), "⌥⌘I");
        assert_eq!(mac("ctrl-cmd-f"), "⌃⌘F");
        assert_eq!(mac("ctrl-tab"), "⌃⇥");
        assert_eq!(mac("ctrl-shift-tab"), "⌃⇧⇥");
        assert_eq!(mac("cmd-}"), "⌘}");
        assert_eq!(mac("cmd-up"), "⌘↑");
        assert_eq!(mac("cmd-backspace"), "⌘⌫");
        assert_eq!(mac("shift-pagedown"), "⇧⇟");
        assert_eq!(mac("cmd-shift-space"), "⇧⌘Space");
        assert_eq!(mac("cmd-="), "⌘=");
        assert_eq!(mac("f11"), "F11");
        assert_eq!(mac("ctrl-alt-shift-k"), "⌃⌥⇧K");
        // Windows、Linux 上的写法不变；cmd 显示为 Win
        assert_eq!(combo("cmd-k").label(KeyOs::Windows), "Win+K");
        assert_eq!(label_of("ctrl-alt-h", KeyOs::Linux), "Ctrl+Alt+H");
        assert_eq!(label_of("cmd->", KeyOs::Mac), "⌘>");
        assert_eq!(
            label_of("Ctrl+H", KeyOs::Mac),
            "Ctrl+H",
            "写法不对时原样返回"
        );
    }

    #[test]
    fn allowed_on_each_os() {
        let allowed = |text, os| combo(text).is_allowed(os);
        for text in [
            "cmd-k",
            "ctrl-k",
            "cmd-}",
            "cmd-shift-d",
            "cmd-shift-space",
            "ctrl-shift-tab",
            "alt-cmd-i",
            "f5",
            "shift-pageup",
            "ctrl-tab",
        ] {
            assert!(allowed(text, KeyOs::Mac), "{text}");
        }
        // macOS 上只带 Option 的不行（Option 用来输入组合字符）；Shift 加数字、符号键 GPUI 报告为 Shift 之后的字符，
        // 这样写触发不了
        for text in [
            "alt-1",
            "alt-k",
            "alt-shift-=",
            "k",
            "shift-a",
            "shift-up",
            "cmd-shift-[",
            "cmd-shift-=",
            "ctrl-shift-1",
        ] {
            assert!(!allowed(text, KeyOs::Mac), "{text}");
        }
        // Windows、Linux 上照旧写键面上的字符
        assert!(allowed("ctrl-shift-=", KeyOs::Windows));
        // Windows、Linux 上带 Win / Super 键的不行
        for os in [KeyOs::Windows, KeyOs::Linux] {
            assert!(!allowed("cmd-k", os), "{os:?}");
            assert!(!allowed("ctrl-cmd-k", os), "{os:?}");
            assert!(allowed("alt-1", os), "{os:?}");
        }
    }

    // 两套默认值：规范、允许、互不冲突；macOS 的应用快捷键都在 ⌘ 上（Ctrl 交给终端），只有 ⌃⇥、⌃⇧⇥ 例外
    // （Terminal.app、iTerm2、Safari 都用它们切换 Tab；⌃⇥ 在终端里只是 Tab，不影响 shell）
    #[test]
    fn defaults_for_each_os() {
        for os in KeyOs::ALL {
            let keymap = Keymap::defaults(os);
            let mut seen = HashSet::new();
            for action in KeyAction::ALL {
                assert_eq!(
                    action.defaults(os).is_empty(),
                    without_default(action),
                    "{os:?} {}",
                    action.id()
                );
                assert!(action.defaults(os).len() <= MAX_COMBOS);
                assert_eq!(texts(keymap.combos(action)), action.defaults(os), "{os:?}");
                for text in action.defaults(os) {
                    let c = combo(text);
                    assert_eq!(c.to_string(), *text, "{os:?}：默认值是规范的写法");
                    assert!(c.is_allowed(os), "{os:?} {text}");
                    assert!(seen.insert(c.clone()), "{os:?}：{text} 属于两个动作");
                    if os.is_mac() {
                        let exception = ["ctrl-tab", "ctrl-shift-tab"].contains(text);
                        assert!(
                            c.cmd || exception,
                            "macOS 的默认值 {text}（{}）应带 ⌘",
                            action.id()
                        );
                    } else {
                        assert!(!c.cmd, "{os:?} 的默认值 {text} 不应带 Win 键");
                    }
                }
            }
        }
        // Windows 与 Linux 相同
        for action in KeyAction::ALL {
            assert_eq!(
                action.defaults(KeyOs::Windows),
                action.defaults(KeyOs::Linux)
            );
        }
        let mac = |action: KeyAction| action.defaults(KeyOs::Mac);
        assert_eq!(mac(KeyAction::Copy), ["cmd-c"]);
        assert_eq!(mac(KeyAction::Paste), ["cmd-v"]);
        assert_eq!(mac(KeyAction::NewLocal), ["cmd-t"]);
        assert_eq!(mac(KeyAction::Tab1), ["cmd-1"]);
        assert_eq!(mac(KeyAction::SplitRight), ["cmd-d"]);
        assert_eq!(mac(KeyAction::NextTab), ["cmd-}", "ctrl-tab"]);
        assert_eq!(mac(KeyAction::ZoomIn), ["cmd-=", "cmd-+"]);
        assert_eq!(mac(KeyAction::FullScreen), ["ctrl-cmd-f"]);
    }

    // 按平台分开存：Windows、Linux 共用 bindings，macOS 用 mac_bindings；一边增删、恢复默认、换键位方案不影响另一边
    #[test]
    fn bindings_are_kept_per_os() {
        let base = KeybindingSettings::default();
        let on_windows =
            add_binding(&base, KeyAction::Copy, combo("ctrl-alt-c"), KeyOs::Windows).unwrap();
        let both = add_binding(&on_windows, KeyAction::Copy, combo("cmd-k"), KeyOs::Mac).unwrap();
        assert_eq!(
            both.bindings["copy"],
            ["ctrl-shift-c", "ctrl-insert", "ctrl-alt-c"]
        );
        assert_eq!(both.mac_bindings["copy"], ["cmd-c", "cmd-k"]);
        // Linux 与 Windows 用同一份
        assert_eq!(
            texts(effective(&both, KeyOs::Linux).combos(KeyAction::Copy)),
            ["ctrl-shift-c", "ctrl-insert", "ctrl-alt-c"]
        );
        // 在一边停用、恢复默认，另一边不变
        let disabled = remove_binding(&both, KeyAction::Lock, &combo("cmd-shift-l"), KeyOs::Mac);
        assert!(disabled.mac_bindings["lock"].is_empty());
        assert!(
            effective(&disabled, KeyOs::Mac)
                .combos(KeyAction::Lock)
                .is_empty()
        );
        assert_eq!(
            texts(effective(&disabled, KeyOs::Windows).combos(KeyAction::Lock)),
            ["ctrl-shift-l"]
        );
        let reset = reset_binding(&disabled, KeyAction::Copy, KeyOs::Windows);
        assert!(!reset.bindings.contains_key("copy"));
        assert_eq!(reset.mac_bindings, disabled.mac_bindings);
        assert_eq!(reset_all(&disabled, KeyOs::Mac).bindings, disabled.bindings);
        assert!(reset_all(&disabled, KeyOs::Mac).mac_bindings.is_empty());
        let preset = apply_preset(&disabled, KeyPreset::Xshell);
        assert_eq!(preset.mac_bindings, disabled.mac_bindings);
    }

    // 设置同步后各用各的：Windows 上改的复制（Ctrl+Alt+C 在 macOS 上也“用得了”）不会让 macOS 丢掉 ⌘C
    #[test]
    fn synced_settings_use_their_own_copy() {
        let synced: KeybindingSettings = serde_json::from_value(serde_json::json!({
            "bindings": { "copy": ["ctrl-alt-c"], "lock": [] },
            "macBindings": { "paste": ["cmd-v", "cmd-shift-v"] },
        }))
        .unwrap();
        let mac = effective(&synced, KeyOs::Mac);
        assert_eq!(texts(mac.combos(KeyAction::Copy)), ["cmd-c"]);
        assert_eq!(texts(mac.combos(KeyAction::Lock)), ["cmd-shift-l"]);
        assert_eq!(
            texts(mac.combos(KeyAction::Paste)),
            ["cmd-v", "cmd-shift-v"]
        );
        let windows = effective(&synced, KeyOs::Windows);
        assert_eq!(texts(windows.combos(KeyAction::Copy)), ["ctrl-alt-c"]);
        assert!(windows.combos(KeyAction::Lock).is_empty());
        assert_eq!(
            texts(windows.combos(KeyAction::Paste)),
            ["ctrl-shift-v", "shift-insert"]
        );
        // 没有 macBindings 的设置（以前的版本保存的）照样能读；写出时用 camelCase
        let old: KeybindingSettings =
            serde_json::from_value(serde_json::json!({ "bindings": { "copy": [] } })).unwrap();
        assert!(old.mac_bindings.is_empty());
        let value = serde_json::to_value(&synced).unwrap();
        assert_eq!(value["macBindings"]["paste"][1], "cmd-shift-v");
        // 保存了组合但一个也用不了（写法不对、这个系统上不允许）时用默认值，不当作停用
        let broken = settings(&[("copy", &["not a combo", "cmd-k"])]);
        let windows = effective(&broken, KeyOs::Windows);
        assert_eq!(
            texts(windows.combos(KeyAction::Copy)),
            ["ctrl-shift-c", "ctrl-insert"]
        );
        assert!(!windows.is_customized(KeyAction::Copy));
    }

    #[test]
    fn editing_on_mac() {
        let os = KeyOs::Mac;
        let base = KeybindingSettings::default();
        assert_eq!(
            add_binding(&base, KeyAction::Copy, combo("alt-c"), os),
            Err(BindingError::NeedsModifier(combo("alt-c")))
        );
        assert_eq!(
            add_binding(&base, KeyAction::Copy, combo("cmd-t"), os),
            Err(BindingError::Conflict {
                combo: combo("cmd-t"),
                action: KeyAction::NewLocal
            })
        );
        let next = add_binding(&base, KeyAction::Copy, combo("ctrl-shift-c"), os).unwrap();
        assert_eq!(next.mac_bindings["copy"], ["cmd-c", "ctrl-shift-c"]);
        assert!(next.bindings.is_empty());
        // 去掉再加回来：与 macOS 的默认值相同，这一项去掉
        let back = remove_binding(&next, KeyAction::Copy, &combo("ctrl-shift-c"), os);
        assert!(back.mac_bindings.is_empty());
        assert!(KeyPreset::ALL.iter().all(|_| !KeyPreset::available(os)));
        assert!(KeyPreset::available(KeyOs::Windows));
    }
}
