//! 让窗口始终在其他窗口之上（主机概览的小窗），以及不抢焦点地把它拿到前面。
//!
//! GPUI 0.3.10 没有可以随时切换的置顶：`WindowKind::PopUp` 在 Windows 上是不能改大小、不在任务栏与 Alt+Tab 中的工具
//! 窗口，`WindowKind::Floating` 只在 macOS 上提高层级（Windows 上与普通窗口相同），两者都只能在打开时选定。所以小窗按
//! 普通窗口打开，置顶经平台的接口设置：
//!
//! - Windows：`SetWindowPos` 放到 `HWND_TOPMOST` / `HWND_NOTOPMOST`（扩展样式 `WS_EX_TOPMOST`）；拿到前面用
//!   `HWND_TOP`（置顶的用 `HWND_TOPMOST`）加 `SWP_NOACTIVATE`，最小化着时先 `SW_SHOWNOACTIVATE` 还原。都不激活窗口。
//! - macOS：`NSWindow` 的层级（`NSFloatingWindowLevel` / `NSNormalWindowLevel`，与 GPUI 的 `Floating` 相同的层级）；
//!   拿到前面用 `orderFront:`（不成为主窗口、不抢键盘焦点），最小化着时先 `deminiaturize:`。
//! - Linux：GPUI 没有接口，Wayland 也不允许程序自己置顶，不支持（[`supported`]）：小窗不显示置顶的按钮，拿到前面用
//!   GPUI 的激活。
//!
//! 平台的调用会同步送来窗口消息，不能在 GPUI 处理这个窗口的中途进行（会重入它的状态）：[`apply`] 放到一个前台任务
//! 里，先经 GPUI 取到窗口的句柄（窗口已经关了就什么也不做），再在 GPUI 的更新之外调用。

use gpui_kit::{AnyWindowHandle, App, AppContext as _, OperatingSystem};

/// 这个系统上能否置顶（小窗据此显示置顶的按钮）。
pub(crate) fn supported(os: OperatingSystem) -> bool {
    matches!(os, OperatingSystem::Windows | OperatingSystem::Mac)
}

/// 设置窗口是否置顶；`raise` 时同时把它拿到前面（不激活、不抢焦点）。不支持置顶的系统上只在 `raise` 时激活窗口。
pub(crate) fn apply(handle: AnyWindowHandle, topmost: bool, raise: bool, cx: &mut App) {
    cx.spawn(async move |cx| {
        let native = cx.update_window(handle, |_, window, _| imp::native(window));
        match native {
            // 在 GPUI 的更新之外：平台同步送来的消息照常进入 GPUI
            Ok(Some(native)) => imp::set(native, topmost, raise),
            Ok(None) if raise => {
                let _ = cx.update_window(handle, |_, window, _| window.activate_window());
            }
            _ => {}
        }
    })
    .detach();
}

#[cfg(windows)]
mod imp {
    use gpui_kit::Window;
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        HWND_NOTOPMOST, HWND_TOP, HWND_TOPMOST, IsIconic, SW_SHOWNOACTIVATE, SWP_NOACTIVATE,
        SWP_NOMOVE, SWP_NOSIZE, SetWindowPos, ShowWindowAsync,
    };

    pub(super) fn native(window: &Window) -> Option<HWND> {
        crate::tray::window_hwnd(window)
    }

    pub(super) fn set(hwnd: HWND, topmost: bool, raise: bool) {
        let after = match (topmost, raise) {
            (true, _) => HWND_TOPMOST,
            (false, true) => HWND_TOP,
            (false, false) => HWND_NOTOPMOST,
        };
        // SAFETY: `hwnd` 是 GPUI 刚交出的、还开着的窗口；参数都是 Win32 文档中的取值。只改 Z 序与显示状态，不移动、
        // 不改大小、不激活
        unsafe {
            if raise && IsIconic(hwnd) != 0 {
                ShowWindowAsync(hwnd, SW_SHOWNOACTIVATE);
            }
            if SetWindowPos(
                hwnd,
                after,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            ) == 0
            {
                tracing::debug!(topmost, raise, "设置窗口的 Z 序失败");
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ptr::NonNull;

    use gpui_kit::Window;
    use objc2::msg_send;
    use objc2::runtime::{AnyObject, Bool};

    /// NSNormalWindowLevel、NSFloatingWindowLevel
    const NORMAL_LEVEL: isize = 0;
    const FLOATING_LEVEL: isize = 3;

    /// 窗口的 NSWindow。
    pub(super) fn native(window: &Window) -> Option<NonNull<AnyObject>> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        // `Window` 自己也有 `window_handle`（返回 GPUI 的窗口句柄），这里要的是 raw-window-handle 的
        match HasWindowHandle::window_handle(window).ok()?.as_raw() {
            RawWindowHandle::AppKit(handle) => {
                let view = handle.ns_view.as_ptr().cast::<AnyObject>();
                // SAFETY: GPUI 的 NSView 在窗口开着时有效，`window` 是 NSView 的公开方法
                let ns_window: *mut AnyObject = unsafe { msg_send![view, window] };
                NonNull::new(ns_window)
            }
            _ => None,
        }
    }

    pub(super) fn set(ns_window: NonNull<AnyObject>, topmost: bool, raise: bool) {
        let ns_window = ns_window.as_ptr();
        let level = if topmost {
            FLOATING_LEVEL
        } else {
            NORMAL_LEVEL
        };
        let sender = std::ptr::null_mut::<AnyObject>();
        // SAFETY: `ns_window` 是 GPUI 刚交出的、还开着的窗口；都是 NSWindow 的公开方法，参数与返回值的类型与声明一致
        unsafe {
            let _: () = msg_send![ns_window, setLevel: level];
            if raise {
                let minimized: Bool = msg_send![ns_window, isMiniaturized];
                if minimized.as_bool() {
                    let _: () = msg_send![ns_window, deminiaturize: sender];
                }
                let _: () = msg_send![ns_window, orderFront: sender];
            }
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    use gpui_kit::Window;

    /// 不支持置顶时的平台句柄：不会有（[`native`] 总是 `None`）。不用空的枚举：`apply` 中匹配它的分支会被当成
    /// 永远匹配不到而警告。
    #[allow(dead_code)]
    pub(super) struct Native;

    /// 不支持置顶：没有平台的句柄（[`super::apply`] 改用 GPUI 的激活）。
    pub(super) fn native(_: &Window) -> Option<Native> {
        None
    }

    pub(super) fn set(_: Native, _: bool, _: bool) {}
}

#[cfg(test)]
mod tests {
    use gpui_kit::OperatingSystem;

    use super::supported;

    #[test]
    fn topmost_on_windows_and_macos_only() {
        assert!(supported(OperatingSystem::Windows));
        assert!(supported(OperatingSystem::Mac));
        assert!(!supported(OperatingSystem::Linux));
    }
}
