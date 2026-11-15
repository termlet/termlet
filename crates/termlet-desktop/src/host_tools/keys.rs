//! 主机工具面板的键盘操作（固定的快捷键，设置的“快捷键”页中列出，不能修改；照 SFTP 文件列表的做法与习惯）。
//!
//! - 各页的列表（进程、服务、容器 / 异常 / 镜像 / 卷 / 网络、网络连接、概览的进程）有焦点时：↑ ↓、PageUp / PageDown、
//!   Home / End 移动并选中；Enter 打开详情（重新读取；网络、概览的进程跳到进程页）；L 查看日志（服务、容器）；
//!   Delete（macOS 上 ⌘⌫）结束进程、停止服务、停止或删除容器、删除镜像等，照常先确认；Shift+F10 或菜单键在当前行
//!   旁边打开右键菜单；/ 到搜索框；Esc 把焦点还给终端。列表有焦点时当前行有一圈焦点框。进程页的树形与合并的同名进程
//!   中 ← 收起（已收起或没有子进程时移到父进程）、→ 展开（已展开时移到第一个子进程），与侧边栏的主机树相同。
//! - 搜索框中 Enter、Esc 回到列表（保留搜索的内容）。
//! - 整个面板中：Ctrl+F（⌘F）到搜索框，Ctrl+PageUp / PageDown（⇧⌘[ / ⇧⌘]）切换各页，焦点进那一页的列表。
//!
//! 进入面板：“打开主机工具的概览”（可自定义，默认 Ctrl+Shift+O / ⇧⌘O）打开面板的概览页并把焦点放进去；点列表的
//! 行也会把焦点放进列表。

use gpui_kit::{App, KeyBinding, OperatingSystem, actions};

actions!(
    host_tools,
    [
        /// 当前行移到上一行 / 下一行 / 上一页 / 下一页 / 第一行 / 最后一行，并选中它
        SelectPrev,
        SelectNext,
        SelectPageUp,
        SelectPageDown,
        SelectFirst,
        SelectLast,
        /// 当前行的主操作（Enter）：打开详情；网络、概览中跳到进程
        OpenItem,
        /// 查看当前行的日志（服务、容器）
        OpenLogs,
        /// 当前行的写操作（结束进程、停止服务、停止或删除容器、删除镜像等），照常先确认
        DeleteItem,
        /// 在当前行旁边打开右键菜单
        OpenMenu,
        /// 树形（进程）、合并的一组中：收起当前行；已收起或没有下级时移到上级
        CollapseItem,
        /// 树形（进程）、合并的一组中：展开当前行；已展开时移到第一个下级
        ExpandItem,
        /// 焦点到当前页的搜索框
        FocusSearch,
        /// 焦点从搜索框回到列表
        FocusList,
        /// 焦点还给面板跟随的终端
        FocusTerminal,
        /// 切到下一页 / 上一页（循环），焦点进那一页的列表
        NextPage,
        PrevPage,
    ]
);

/// 整个面板的上下文。
pub(super) const PANEL_CONTEXT: &str = "HostTools";
/// 各页列表的上下文（搜索框不在其中：列表的 /、L 等不影响输入）。
pub(super) const LIST_CONTEXT: &str = "HostToolsList";
/// 搜索框外面的容器的上下文（输入框自己用不到 Enter、Esc 时交给这里）。
pub(super) const SEARCH_CONTEXT: &str = "HostToolsSearch";

pub fn init(cx: &mut App) {
    cx.bind_keys(key_bindings(cx.platform_info().os));
}

/// 面板的快捷键：`os` 为按哪个系统的习惯。macOS 上照 Finder 用 ⌘⌫ 删除，⇧⌘[ ] 切换页（照 Safari、Terminal 在当前区域内
/// 切换页签的习惯；面板外 ⇧⌘[ ] 照常切换 Tab，面板中切换 Tab 用 ⌃⇥）。
pub(crate) fn key_bindings(os: OperatingSystem) -> Vec<KeyBinding> {
    let list = Some(LIST_CONTEXT);
    let search = Some(SEARCH_CONTEXT);
    let panel = Some(PANEL_CONTEXT);
    let mut bindings = vec![
        KeyBinding::new("up", SelectPrev, list),
        KeyBinding::new("down", SelectNext, list),
        KeyBinding::new("pageup", SelectPageUp, list),
        KeyBinding::new("pagedown", SelectPageDown, list),
        KeyBinding::new("home", SelectFirst, list),
        KeyBinding::new("end", SelectLast, list),
        KeyBinding::new("enter", OpenItem, list),
        KeyBinding::new("l", OpenLogs, list),
        KeyBinding::new("delete", DeleteItem, list),
        KeyBinding::new("shift-f10", OpenMenu, list),
        KeyBinding::new("menu", OpenMenu, list),
        KeyBinding::new("left", CollapseItem, list),
        KeyBinding::new("right", ExpandItem, list),
        KeyBinding::new("/", FocusSearch, list),
        KeyBinding::new("escape", FocusTerminal, list),
        KeyBinding::new("enter", FocusList, search),
        KeyBinding::new("escape", FocusList, search),
    ];
    if os == OperatingSystem::Mac {
        bindings.extend([
            KeyBinding::new("cmd-backspace", DeleteItem, list),
            KeyBinding::new("cmd-f", FocusSearch, panel),
            KeyBinding::new("cmd-}", NextPage, panel),
            KeyBinding::new("cmd-{", PrevPage, panel),
        ]);
    } else {
        bindings.extend([
            KeyBinding::new("ctrl-f", FocusSearch, panel),
            KeyBinding::new("ctrl-pagedown", NextPage, panel),
            KeyBinding::new("ctrl-pageup", PrevPage, panel),
        ]);
    }
    bindings
}
