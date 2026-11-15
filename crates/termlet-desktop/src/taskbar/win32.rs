//! Windows 的任务栏按钮进度：`ITaskbarList3` 的 `SetProgressState`、`SetProgressValue`。
//!
//! - windows-sys 只有函数、常量与结构，没有 COM 接口：这里按 shobjidl_core.h 中的顺序写出虚表（`IUnknown`、
//!   `ITaskbarList`、`ITaskbarList2` 的方法在前，用不到的只占位），只调用其中的 `Release`、`HrInit` 与两个进度方法。
//! - COM 由 GPUI 在主线程上初始化（`OleInitialize`，单线程套间）；对象在主线程上建、用、释放（全局量只在主线程上）。
//! - 窗口隐藏到托盘时没有任务栏按钮，调用失败不要紧；窗口再显示（被激活）时重新设置一次（见 `super::activated`）。

use std::ffi::c_void;
use std::ptr::null_mut;

use gpui_kit::Window;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows_sys::Win32::UI::Shell::{
    TBPF_ERROR, TBPF_INDETERMINATE, TBPF_NOPROGRESS, TBPF_NORMAL, TBPF_PAUSED, TBPFLAG, TaskbarList,
};
use windows_sys::core::{GUID, HRESULT};

use super::{Indicator, TaskbarHost};

/// `IID_ITaskbarList3`
const IID_ITASKBARLIST3: GUID = GUID::from_u128(0xea1afb91_9e28_4b86_90e9_9e9f8a5eefaf);

/// 进度的总量（千分比）。
const TOTAL: u64 = 1000;

/// `ITaskbarList3` 的对象：第一个字段是虚表的指针。
#[repr(C)]
struct TaskbarList3 {
    vtbl: *const Vtbl,
}

/// `ITaskbarList3` 的虚表，到 `SetProgressState` 为止（后面的方法用不到）。
#[repr(C)]
struct Vtbl {
    _query_interface: usize,
    _add_ref: usize,
    release: unsafe extern "system" fn(*mut TaskbarList3) -> u32,
    // ITaskbarList
    hr_init: unsafe extern "system" fn(*mut TaskbarList3) -> HRESULT,
    _add_tab: usize,
    _delete_tab: usize,
    _activate_tab: usize,
    _set_active_alt: usize,
    // ITaskbarList2
    _mark_fullscreen_window: usize,
    // ITaskbarList3
    set_progress_value: unsafe extern "system" fn(*mut TaskbarList3, HWND, u64, u64) -> HRESULT,
    set_progress_state: unsafe extern "system" fn(*mut TaskbarList3, HWND, TBPFLAG) -> HRESULT,
}

/// 任务栏按钮的进度。
pub(crate) struct Win32Taskbar {
    list: *mut TaskbarList3,
}

impl Win32Taskbar {
    /// 建立 `TaskbarList` 对象；失败时（没有资源管理器的任务栏等）为 `None`。
    pub(crate) fn new() -> Option<Self> {
        let mut object: *mut c_void = null_mut();
        // SAFETY: 参数都是有效的指针；成功时 object 指向 ITaskbarList3（引用计数 1，Drop 中释放）
        let hr = unsafe {
            CoCreateInstance(
                &TaskbarList,
                null_mut(),
                CLSCTX_INPROC_SERVER,
                &IID_ITASKBARLIST3,
                &mut object,
            )
        };
        if hr < 0 || object.is_null() {
            tracing::warn!("建立 ITaskbarList3 失败：{hr:#x}");
            return None;
        }
        let this = Self {
            list: object.cast(),
        };
        // SAFETY: list 是有效的 ITaskbarList3；HrInit 在使用其他方法之前调用一次
        let hr = unsafe { ((*(*this.list).vtbl).hr_init)(this.list) };
        if hr < 0 {
            tracing::warn!("ITaskbarList3::HrInit 失败：{hr:#x}");
            return None;
        }
        Some(this)
    }

    fn set(&self, hwnd: HWND, state: TBPFLAG, value: Option<u16>) {
        // SAFETY: list 有效；hwnd 是开着的 GPUI 窗口的（没有任务栏按钮时调用失败，不要紧）
        unsafe {
            let vtbl = &*(*self.list).vtbl;
            let hr = (vtbl.set_progress_state)(self.list, hwnd, state);
            if hr < 0 {
                tracing::debug!("SetProgressState 失败：{hr:#x}");
                return;
            }
            if let Some(value) = value {
                (vtbl.set_progress_value)(self.list, hwnd, u64::from(value).min(TOTAL), TOTAL);
            }
        }
    }
}

impl Drop for Win32Taskbar {
    fn drop(&mut self) {
        // SAFETY: list 是 CoCreateInstance 得到的，只释放这一次
        unsafe { ((*(*self.list).vtbl).release)(self.list) };
    }
}

impl TaskbarHost for Win32Taskbar {
    fn per_window(&self) -> bool {
        true
    }

    fn show(&mut self, window: Option<&Window>, indicator: Indicator) {
        let Some(hwnd) = window.and_then(crate::tray::window_hwnd) else {
            return;
        };
        match indicator {
            Indicator::None => self.set(hwnd, TBPF_NOPROGRESS, None),
            Indicator::Normal(value) => self.set(hwnd, TBPF_NORMAL, Some(value)),
            Indicator::Indeterminate => self.set(hwnd, TBPF_INDETERMINATE, None),
            Indicator::Paused(value) => self.set(hwnd, TBPF_PAUSED, Some(value)),
            Indicator::Error(value) => self.set(hwnd, TBPF_ERROR, Some(value)),
        }
    }
}
