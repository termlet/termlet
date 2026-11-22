//! 命令行工具（`termlet`）与 MCP 服务的界面：设置中的「AI 与自动化」页（[`CliPage`]）与审计日志对话框（[`open_audit`]）。
//!
//! - 设置页：外部 CLI 的开关（本机设置）、危险命令规则（内置规则的开关、自定义规则、命中时询问或拦截、规则测试）
//!   随设置对话框的“保存”写入；Agent Skill 的安装与 PATH 的修改立即执行，结果写在设置对话框底部
//!   （`notify::show`）；MCP 服务给出各 AI 工具的配置片段，HTTP 接入的开关与端口随“保存”写入，令牌的重新生成
//!   立即执行。
//! - 审计对话框：列表、搜索、按结果与来源筛选、翻页、单条详情、导出、清空；打开着时由主会话在写入新记录后调用
//!   [`refresh_open_audit`] 补上新的记录。
//!
//! 审计日志的实例由命令行服务持有（`cli_bridge::audit`），这里只在后台执行器上读写它。

mod audit;
mod http;
mod mcp;
mod page;
mod rules;
mod skill;
#[cfg(test)]
mod tests;
mod tool;

use std::path::{Path, PathBuf};

use gpui_kit::SharedString;

pub(crate) use audit::{open_audit, refresh_open_audit};
pub(crate) use page::CliPage;

use crate::t;

/// 开发构建中的命令行工具：cargo 把 termlet 编译在 termlet-desktop 的旁边（安装版中同一目录的 termlet.exe 就是
/// Termlet.exe 本身，所以只认程序叫 termlet-desktop 的时候）。不是开发构建、或旁边没有时为 `None`。
fn dev_cli(exe: &Path) -> Option<PathBuf> {
    let dev = exe.with_file_name(format!("termlet{}", std::env::consts::EXE_SUFFIX));
    (exe.file_stem()
        .is_some_and(|stem| stem == "termlet-desktop")
        && dev.is_file())
    .then_some(dev)
}

/// 设置导航中这一页的名称（与 `termlet` 的错误说明中“设置中的「AI 与自动化」”一致）。
pub(crate) fn page_title() -> SharedString {
    t!("cli_ui.nav")
}

/// 这一页用到的本机环境：应用程序的路径与用户主目录。测试中换成临时目录，不碰真实的 PATH 与 AI 工具的目录。
#[derive(Clone, Debug)]
pub(crate) struct CliEnv {
    /// 应用程序（安装版为安装目录中的 `Termlet.exe`，命令行工具在旁边的 `bin` 中）；读不到时为 `None`
    exe: Option<PathBuf>,
    /// 用户主目录（Agent Skill 的安装位置在它下面）；读不到时为 `None`
    home: Option<PathBuf>,
    /// 读取进程的环境变量 `CLAUDE_CONFIG_DIR`、`CODEX_HOME`（测试中不读）
    system_env: bool,
    /// 便携版：不修改 PATH
    portable: bool,
    /// Linux：`/usr/bin/termlet`（deb、rpm 装的链接）解析后指向的文件；没有时为 `None`
    system_link: Option<PathBuf>,
    /// Linux：环境变量 `APPIMAGE`（从 AppImage 运行时为 AppImage 文件本身）
    appimage: Option<PathBuf>,
}

impl CliEnv {
    /// 这台电脑上的真实环境。
    pub(crate) fn system(cx: &gpui_kit::App) -> Self {
        let linux = cfg!(target_os = "linux");
        Self {
            exe: std::env::current_exe().ok(),
            home: std::env::home_dir().filter(|h| !h.as_os_str().is_empty()),
            system_env: true,
            portable: crate::app_state::AppState::is_portable(cx),
            system_link: linux
                .then(|| std::fs::canonicalize(crate::install_kind::SYSTEM_CLI).ok())
                .flatten(),
            appimage: linux
                .then(|| std::env::var_os("APPIMAGE"))
                .flatten()
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
        }
    }

    /// 指定的应用程序路径与主目录，不读环境变量（测试用）。
    #[cfg(test)]
    pub(crate) fn at(exe: Option<PathBuf>, home: Option<PathBuf>) -> Self {
        Self {
            exe,
            home,
            system_env: false,
            portable: false,
            system_link: None,
            appimage: None,
        }
    }

    /// 指定 Linux 的安装方式（测试用）：`/usr/bin/termlet` 指向的文件与环境变量 `APPIMAGE`。
    #[cfg(test)]
    pub(crate) fn with_linux(
        mut self,
        system_link: Option<PathBuf>,
        appimage: Option<PathBuf>,
    ) -> Self {
        self.system_link = system_link;
        self.appimage = appimage;
        self
    }

    /// 当作便携版（测试用）。
    #[cfg(test)]
    pub(crate) fn into_portable(mut self) -> Self {
        self.portable = true;
        self
    }
}
