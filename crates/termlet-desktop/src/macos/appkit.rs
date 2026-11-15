//! [`super`] 在 macOS 上的实现：经 Objective-C 运行时操作 GPUI 建的 NSApplication 与它的委托（类名
//! `GPUIApplicationDelegate`，见 gpui-pre-macos 的 `platform.rs`）。只用 objc2 与标准库。

use std::cell::RefCell;

use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{class, ffi, msg_send, sel};

/// NSApplicationTerminateReply：NSTerminateCancel、NSTerminateNow
const TERMINATE_CANCEL: usize = 0;
const TERMINATE_NOW: usize = 1;

/// `applicationShouldTerminate:` 的类型编码：返回 NSUInteger（Q），参数为 self、_cmd 与发送者
const SHOULD_TERMINATE_TYPES: &std::ffi::CStr = c"Q@:@";

type ShouldTerminate = extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> usize;

thread_local! {
    /// 系统请求退出时问它（只在主线程上用）
    static TERMINATE_HANDLER: RefCell<Option<Box<dyn FnMut() -> bool>>> = const { RefCell::new(None) };
}

pub(super) fn install_terminate_hook() {
    let Some(class) = AnyClass::get(c"GPUIApplicationDelegate") else {
        tracing::warn!("找不到 GPUI 的应用委托类，系统请求退出时不会先确认");
        return;
    };
    let imp: ShouldTerminate = should_terminate;
    // SAFETY：类已注册，方法的类型编码与函数签名一致；class_addMethod 不替换已有的方法
    let added = unsafe {
        ffi::class_addMethod(
            std::ptr::from_ref(class).cast_mut(),
            sel!(applicationShouldTerminate:),
            std::mem::transmute::<ShouldTerminate, Imp>(imp),
            SHOULD_TERMINATE_TYPES.as_ptr(),
        )
    };
    if added.as_bool() {
        tracing::debug!("已给应用委托补上 applicationShouldTerminate:");
    } else {
        tracing::warn!("应用委托已有 applicationShouldTerminate:，不覆盖");
    }
}

pub(super) fn on_terminate_request(handler: Box<dyn FnMut() -> bool>) {
    TERMINATE_HANDLER.with(|slot| *slot.borrow_mut() = Some(handler));
}

/// `-[GPUIApplicationDelegate applicationShouldTerminate:]`
extern "C-unwind" fn should_terminate(_: *mut AnyObject, _: Sel, _: *mut AnyObject) -> usize {
    let now = std::panic::catch_unwind(|| {
        TERMINATE_HANDLER.with(|slot| match slot.try_borrow_mut() {
            Ok(mut handler) => handler.as_mut().is_none_or(|handler| handler()),
            Err(_) => true,
        })
    })
    .unwrap_or(true);
    if now {
        TERMINATE_NOW
    } else {
        tracing::info!("系统请求退出，先确认");
        TERMINATE_CANCEL
    }
}

pub(super) fn set_windows_menu(index: usize) {
    let Ok(index) = isize::try_from(index) else {
        return;
    };
    // SAFETY：都是 NSApplication、NSMenu、NSMenuItem 的公开方法，参数与返回值的类型与声明一致；空指针先检查
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        if app.is_null() {
            return;
        }
        let main_menu: *mut AnyObject = msg_send![app, mainMenu];
        if main_menu.is_null() {
            return;
        }
        let count: isize = msg_send![main_menu, numberOfItems];
        if index >= count {
            return;
        }
        let item: *mut AnyObject = msg_send![main_menu, itemAtIndex: index];
        if item.is_null() {
            return;
        }
        let menu: *mut AnyObject = msg_send![item, submenu];
        if menu.is_null() {
            return;
        }
        let _: () = msg_send![app, setWindowsMenu: menu];
    }
}
