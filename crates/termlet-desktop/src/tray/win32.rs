//! Windows 的托盘图标：`Shell_NotifyIconW` 加一个不显示的窗口接收图标的消息。
//!
//! - 窗口建在主线程上：GPUI 的消息循环（`GetMessageW` 不限窗口）本来就分发本线程所有窗口的消息，不用另开线程。
//! - 窗口的消息处理不碰 GPUI 的状态：单击图标、菜单中的选择放进通道，由 GPUI 的任务处理（[`super::install_with`]）。
//! - 右键菜单（`TrackPopupMenu` 有自己的消息循环）不在收到图标的消息时当场弹出，而是先投递一条消息给自己，回到
//!   消息循环的最外层再弹：图标的消息可能在 GPUI 处理事件的中途送到（等待跨进程的调用时），那时进入嵌套的消息循环
//!   会重入 GPUI。弹出前把这个窗口设为前台、关闭后投递一条空消息，点到别处时菜单才会关闭（托盘菜单的惯例）。
//! - 用 `NOTIFYICON_VERSION_4`：单击（`NIN_SELECT`）、键盘选中（`NIN_KEYSELECT`）恢复窗口，右键、Shift+F10
//!   （`WM_CONTEXTMENU`）弹出菜单。资源管理器重启（任务栏重建，广播 `TaskbarCreated`）后重新加上图标；这个窗口是
//!   顶层窗口（不是只收消息的窗口），才收得到广播。
//! - 图标是程序的图标资源（ID 1，见 build.rs），按小图标的尺寸加载。
//! - 隐藏、显示 GPUI 的窗口用 `ShowWindowAsync`：同步的 `ShowWindow` 会在 GPUI 处理关闭按钮的中途把窗口的消息
//!   直接送进 GPUI，重入它的状态。

use std::cell::RefCell;
use std::ptr::{null, null_mut};

use gpui_kit::Window;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_SETVERSION, NIN_SELECT,
    NINF_KEY, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, GetSystemMetrics,
    HICON, IDI_APPLICATION, IMAGE_ICON, LR_DEFAULTCOLOR, LoadIconW, LoadImageW, MF_SEPARATOR,
    MF_STRING, PostMessageW, RegisterClassW, RegisterWindowMessageW, SM_CXSMICON, SM_CYSMICON,
    SM_MENUDROPALIGNMENT, SW_HIDE, SW_SHOW, SetForegroundWindow, SetMenuDefaultItem,
    ShowWindowAsync, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTALIGN,
    TPM_RIGHTBUTTON, TrackPopupMenu, WM_APP, WM_CONTEXTMENU, WM_NULL, WNDCLASSW,
};

use super::{TrayCommand, TrayHost};
use crate::t;

/// 图标的 ID（这个窗口只有一个图标）。
const ICON_ID: u32 = 1;
/// 图标的回调消息。
const WM_TRAY: u32 = WM_APP + 1;
/// 弹出右键菜单（投递给自己，回到消息循环的最外层再处理）。
const WM_TRAY_MENU: u32 = WM_APP + 2;
/// 菜单项的 ID。
const MENU_SHOW: usize = 1;
const MENU_QUIT: usize = 2;
/// 键盘选中图标（`NIN_SELECT | NINF_KEY`）。
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;

/// 接收图标消息的窗口的状态（主线程上，窗口的消息处理与 [`Win32Tray`] 共用）。
struct State {
    hwnd: HWND,
    icon: HICON,
    commands: async_channel::Sender<TrayCommand>,
    /// 资源管理器重启后广播的消息
    taskbar_created: u32,
    /// 图标该在通知区域中（资源管理器重启后据此重新加上）
    shown: bool,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// 以 0 结尾的 UTF-16 字符串。
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// Windows 的托盘（状态在 [`STATE`] 中，这里只是操作它的入口）。
pub(crate) struct Win32Tray;

impl Win32Tray {
    /// 建立接收图标消息的窗口；失败时为 `None`（关闭窗口照常退出）。
    pub(crate) fn new(commands: async_channel::Sender<TrayCommand>) -> Option<Self> {
        let class = wide("TermletTrayWindow");
        let title = wide("Termlet");
        // SAFETY: 字符串以 0 结尾、在调用期间有效；窗口类只注册一次（重复注册失败时 CreateWindowExW 照样找得到它）
        let (instance, hwnd) = unsafe {
            let instance = GetModuleHandleW(null());
            let window_class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: class.as_ptr(),
                ..std::mem::zeroed()
            };
            RegisterClassW(&window_class);
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                instance,
                null(),
            );
            (instance, hwnd)
        };
        if hwnd.is_null() {
            return None;
        }
        // SAFETY: 资源 ID 1 是程序的图标（build.rs 嵌入）；没有时（例如测试程序）退回系统的应用程序图标
        let icon = unsafe {
            let icon = LoadImageW(
                instance,
                // MAKEINTRESOURCEW(1)
                std::ptr::without_provenance(1),
                IMAGE_ICON,
                GetSystemMetrics(SM_CXSMICON),
                GetSystemMetrics(SM_CYSMICON),
                LR_DEFAULTCOLOR,
            ) as HICON;
            if icon.is_null() {
                LoadIconW(null_mut(), IDI_APPLICATION)
            } else {
                icon
            }
        };
        let taskbar_created = wide("TaskbarCreated");
        // SAFETY: 字符串以 0 结尾
        let taskbar_created = unsafe { RegisterWindowMessageW(taskbar_created.as_ptr()) };
        STATE.set(Some(State {
            hwnd,
            icon,
            commands,
            taskbar_created,
            shown: false,
        }));
        Some(Self)
    }
}

impl TrayHost for Win32Tray {
    fn show_icon(&mut self) -> bool {
        let Some((hwnd, icon)) = STATE.with_borrow_mut(|state| {
            state.as_mut().map(|state| {
                state.shown = true;
                (state.hwnd, state.icon)
            })
        }) else {
            return false;
        };
        add_icon(hwnd, icon)
    }

    fn remove_icon(&mut self) {
        let hwnd = STATE.with_borrow_mut(|state| {
            state.as_mut().map(|state| {
                state.shown = false;
                state.hwnd
            })
        });
        if let Some(hwnd) = hwnd {
            let data = icon_data(hwnd, null_mut());
            // SAFETY: data 的 cbSize 已按结构大小填好
            unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
        }
    }

    fn hide_window(&mut self, window: &Window) {
        if let Some(hwnd) = window_hwnd(window) {
            // SAFETY: hwnd 是这个 GPUI 窗口的，还开着
            unsafe { ShowWindowAsync(hwnd, SW_HIDE) };
        }
    }

    fn show_window(&mut self, window: &Window) {
        if let Some(hwnd) = window_hwnd(window) {
            // SAFETY: 同上；SW_SHOW 保持隐藏前的大小与是否最大化
            unsafe { ShowWindowAsync(hwnd, SW_SHOW) };
        }
    }
}

/// GPUI 窗口的 HWND；取不到时（测试中的窗口）为 `None`。任务栏进度（`crate::taskbar`）也用它。
pub(crate) fn window_hwnd(window: &Window) -> Option<HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    // `Window` 自己也有 `window_handle`（返回 GPUI 的窗口句柄），这里要的是 raw-window-handle 的
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as HWND),
        _ => None,
    }
}

/// 图标的 `NOTIFYICONDATAW`：回调消息、图标与悬停提示（“Termlet”）。
fn icon_data(hwnd: HWND, icon: HICON) -> NOTIFYICONDATAW {
    // SAFETY: 全零是这个结构的合法值（指针为空、数字为 0）
    let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = ICON_ID;
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    data.uCallbackMessage = WM_TRAY;
    data.hIcon = icon;
    for (slot, unit) in data.szTip.iter_mut().zip("Termlet".encode_utf16()) {
        *slot = unit;
    }
    data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    data
}

/// 在通知区域加上图标并设为第 4 版的回调方式，返回是否成功。
fn add_icon(hwnd: HWND, icon: HICON) -> bool {
    let data = icon_data(hwnd, icon);
    // SAFETY: data 的 cbSize 已按结构大小填好
    unsafe {
        if Shell_NotifyIconW(NIM_ADD, &data) == 0 {
            // 上次没有去掉（例如异常退出后同一个窗口）：去掉再加一次
            Shell_NotifyIconW(NIM_DELETE, &data);
            if Shell_NotifyIconW(NIM_ADD, &data) == 0 {
                return false;
            }
        }
        Shell_NotifyIconW(NIM_SETVERSION, &data) != 0
    }
}

/// 把命令交给 GPUI 的任务。
fn send(command: TrayCommand) {
    STATE.with_borrow(|state| {
        if let Some(state) = state {
            let _ = state.commands.try_send(command);
        }
    });
}

/// 接收图标消息的窗口的消息处理。
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_TRAY => {
            // 第 4 版：低 16 位是事件，wParam 是图标的位置（屏幕坐标）
            match (lparam & 0xffff) as u32 {
                NIN_SELECT | NIN_KEYSELECT => send(TrayCommand::Show),
                // SAFETY: hwnd 是这个窗口
                WM_CONTEXTMENU => unsafe {
                    PostMessageW(hwnd, WM_TRAY_MENU, wparam, 0);
                },
                _ => {}
            }
            0
        }
        WM_TRAY_MENU => {
            let x = (wparam & 0xffff) as u16 as i16 as i32;
            let y = ((wparam >> 16) & 0xffff) as u16 as i16 as i32;
            show_menu(hwnd, x, y);
            0
        }
        _ => {
            let readd = STATE.with_borrow(|state| {
                state
                    .as_ref()
                    .filter(|state| message == state.taskbar_created && state.shown)
                    .map(|state| state.icon)
            });
            if let Some(icon) = readd {
                tracing::info!("任务栏重建，重新加上托盘图标");
                add_icon(hwnd, icon);
                return 0;
            }
            // SAFETY: 原样交给默认处理
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
    }
}

/// 在 (`x`, `y`) 弹出托盘菜单：显示 Termlet（默认项）、退出 Termlet。
fn show_menu(hwnd: HWND, x: i32, y: i32) {
    let show = wide(&t!("tray.show"));
    let quit = wide(&t!("tray.quit"));
    // SAFETY: 菜单用完销毁；字符串以 0 结尾、在调用期间有效
    let chosen = unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        AppendMenuW(menu, MF_STRING, MENU_SHOW, show.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        AppendMenuW(menu, MF_STRING, MENU_QUIT, quit.as_ptr());
        SetMenuDefaultItem(menu, MENU_SHOW as u32, 0);
        // 菜单的窗口要在前台，点到别处时菜单才会关闭
        SetForegroundWindow(hwnd);
        let align = if GetSystemMetrics(SM_MENUDROPALIGNMENT) != 0 {
            TPM_RIGHTALIGN
        } else {
            TPM_LEFTALIGN
        };
        let chosen = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | align,
            x,
            y,
            0,
            hwnd,
            null(),
        );
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        chosen as usize
    };
    match chosen {
        MENU_SHOW => send(TrayCommand::Show),
        MENU_QUIT => send(TrayCommand::Quit),
        _ => {}
    }
}
