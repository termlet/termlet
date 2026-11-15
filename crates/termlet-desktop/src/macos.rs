//! macOS 上 GPUI 没有提供的两处应用级行为，经 Objective-C 运行时补上（[`appkit`]，只依赖 objc2）；其他系统上这里的
//! 函数什么也不做，调用方不用区分系统。
//!
//! - 系统请求退出（程序坞的“退出”、关机、注销、其他程序请它退出）：GPUI 的应用委托没有实现
//!   `applicationShouldTerminate:`，系统发来的退出直接结束。这里在 GPUI 建好委托之前给它的类补上这个方法
//!   （[`install_terminate_hook`]），由 Termlet 决定现在退出还是先确认（[`on_terminate_request`]）。
//! - 窗口菜单：GPUI 只把名为 “Window” 的菜单设为系统的窗口菜单（系统在其中列出打开的窗口、加上平铺等项），
//!   简体、繁體中文界面的“窗口”“視窗”菜单由这里设置（[`set_windows_menu`]）。

#[cfg(target_os = "macos")]
mod appkit;

/// 给 GPUI 的应用委托补上 `applicationShouldTerminate:`。要在创建应用（`gpui_kit::application()`）之前调用：
/// 应用把委托设好之后，系统可能已经记下它有哪些方法。
pub fn install_terminate_hook() {
    #[cfg(target_os = "macos")]
    appkit::install_terminate_hook();
}

/// 系统请求退出时问 `handler`：返回 `true` 现在退出，`false` 取消这次退出（Termlet 确认后自己再退出）。
/// 没有登记、`handler` 正在执行（重入）或出错时现在退出，与没有这个回调时相同。
pub fn on_terminate_request(handler: impl FnMut() -> bool + 'static) {
    #[cfg(target_os = "macos")]
    appkit::on_terminate_request(Box::new(handler));
    #[cfg(not(target_os = "macos"))]
    let _ = handler;
}

/// 把菜单栏中第 `index` 个菜单（从应用菜单数起）设为系统的窗口菜单。设置菜单栏（`cx.set_menus`）之后调用。
pub fn set_windows_menu(index: usize) {
    #[cfg(target_os = "macos")]
    appkit::set_windows_menu(index);
    #[cfg(not(target_os = "macos"))]
    let _ = index;
}
