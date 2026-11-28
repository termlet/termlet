//! 用默认的本地终端在指定的目录打开一个 Tab：启动参数 `--open-folder <目录>`，或与 MobaXterm 相同的
//! `-openfolder <目录>`（见 [`crate::args`]）。资源管理器右键菜单的“在此处打开 Termlet”也用它（Windows，设置的
//! “本地终端”中打开，见 termlet-core 的 `folder_menu`）。
//!
//! - 目录按启动这个进程时的当前目录换成绝对路径（[`resolve`]）。已有 Termlet 在运行时，换好的路径写成
//!   `--open-folder <目录>` 随别的启动参数交给它（[`forwarded`]），由它在当前窗口新开一个 Tab。
//! - 请求先排队，窗口建好、保险库解锁后由工作区取走（与外部程序调起的连接相同，见 `Workspace::take_launches`），
//!   这时才检查目录：不存在时提示，不打开。
//! - 默认的本地终端是 WSL 发行版时在发行版中打开这个目录（`wsl.exe --cd`）。
//! - “启动时打开”设为默认的本地终端时不再另开一个：它改在这个目录打开。
//! - 日志不记目录（路径中常有用户名）。

use std::path::{Path, PathBuf};

use gpui_kit::{App, Global};

/// 还没有被窗口取走的目录。
#[derive(Default)]
struct PendingFolders(Vec<PathBuf>);

impl Global for PendingFolders {}

/// 启动参数中的目录换成绝对路径：相对路径按 `cwd`，去掉其中的 `.`（资源管理器的菜单在路径后面加了 `\.`）与末尾的
/// 分隔符。Windows 的命令行中引号前的反斜杠是转义，`"D:\"` 拆出来是 `D:"`，结尾的引号换回反斜杠。为空时是 `None`。
pub fn resolve(raw: &str, cwd: &Path) -> Option<PathBuf> {
    let raw = raw.trim();
    let raw = match raw.strip_suffix('"') {
        Some(rest) if cfg!(windows) => format!("{rest}\\"),
        _ => raw.to_string(),
    };
    if raw.is_empty() {
        return None;
    }
    let path = Path::new(&raw);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    Some(path.components().collect())
}

/// 交给已在运行的实例时追加的启动参数。
pub fn forwarded(dir: &Path) -> [String; 2] {
    ["--open-folder".to_string(), dir.display().to_string()]
}

/// 排进队列（本进程的启动参数，或已有实例转交来的）。
pub fn queue(dir: PathBuf, cx: &mut App) {
    tracing::info!("启动参数要求在指定的目录打开本地终端");
    cx.default_global::<PendingFolders>().0.push(dir);
}

/// 有没有排队的目录（“启动时打开”据此不再另开默认的本地终端）。
pub fn pending(cx: &App) -> bool {
    cx.try_global::<PendingFolders>()
        .is_some_and(|p| !p.0.is_empty())
}

/// 取走排队的目录（工作区在保险库解锁后调用）。
pub fn take(cx: &mut App) -> Vec<PathBuf> {
    if !cx.has_global::<PendingFolders>() {
        return Vec::new();
    }
    std::mem::take(&mut cx.global_mut::<PendingFolders>().0)
}

#[cfg(test)]
mod tests;
