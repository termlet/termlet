//! macOS 的程序坞图标：`NSApp.dockTile.badgeLabel` 显示百分比、“…”或“!”，清除时设为 nil。只用 objc2 与标准库
//! （与 `crate::macos` 相同的做法）；全局量只在主线程上，这里的调用都在主线程。

use std::ffi::c_void;

use gpui_kit::Window;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};

use super::{Indicator, TaskbarHost};

/// `NSUTF8StringEncoding`
const UTF8_ENCODING: usize = 4;

/// 程序坞图标的标记。
pub(crate) struct DockBadge;

impl TaskbarHost for DockBadge {
    fn per_window(&self) -> bool {
        false
    }

    fn show(&mut self, _: Option<&Window>, indicator: Indicator) {
        let label = match indicator {
            Indicator::None => None,
            Indicator::Normal(value) | Indicator::Paused(value) => {
                Some(format!("{}%", value.min(1000) / 10))
            }
            Indicator::Indeterminate => Some("…".to_string()),
            Indicator::Error(_) => Some("!".to_string()),
        };
        set_badge(label.as_deref());
    }
}

/// 设置程序坞图标的标记；`None` 为去掉。
fn set_badge(label: Option<&str>) {
    // SAFETY：都是 NSApplication、NSDockTile、NSString 的公开方法，参数与返回值的类型与声明一致；空指针先检查。
    // initWithBytes:length:encoding: 只在调用期间读 label 的字节；得到的字符串由 Retained 在用完后释放
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        if app.is_null() {
            return;
        }
        let tile: *mut AnyObject = msg_send![app, dockTile];
        if tile.is_null() {
            return;
        }
        let text: Option<Retained<AnyObject>> = match label {
            Some(label) => {
                let alloc: Allocated<AnyObject> = msg_send![class!(NSString), alloc];
                msg_send![
                    alloc,
                    initWithBytes: label.as_ptr().cast::<c_void>(),
                    length: label.len(),
                    encoding: UTF8_ENCODING
                ]
            }
            None => None,
        };
        let _: () = msg_send![tile, setBadgeLabel: text.as_deref()];
    }
}
