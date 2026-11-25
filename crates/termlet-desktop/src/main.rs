// 发布构建不弹控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // 先于任何 DLL 的加载：之后按名字加载的 DLL 只在程序目录与 System32 中找（Windows）
    termlet_desktop::dll_search::restrict();
    termlet_desktop::run();
}
