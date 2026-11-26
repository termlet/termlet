//! 本地终端：用核心的 localshell 检测本机的 shell，按设置选出默认的一个，并给出界面上的名称。
//!
//! 检测要读注册表、运行 vswhere（几百毫秒），启动时在后台线程预先检测一次并缓存。界面线程上只读缓存
//! （过期了也用，另在后台刷新），从不在界面线程上重新检测；程序不在原来的位置时（卸载、升级）由打开终端的
//! 后台任务重新检测一次（[`launch`]）。每次打开终端仍会重新读取用户最新的环境变量。
//!
//! WSL 发行版与本地 shell 并列（侧边栏"本机"、默认终端），设置中写作 `wsl:<发行版>`；列出发行版要运行
//! wsl.exe，同样在启动时于后台列出一次并缓存。

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use gpui_kit::{App, Global, SharedString};
use termlet_core::localpty::PtyLaunch;
use termlet_core::localshell::{Detector, Profile, ShellError, ShellKind};
use termlet_core::model::LocalSettings;

use crate::brand_icons::{self, Brand};
use crate::t;

/// 缓存检测结果的时间：检测慢，界面线程上不重新检测；装了新的 shell 由侧边栏的刷新强制重新检测。
const DETECT_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

struct LocalShells(Arc<Detector>, Arc<WslDistros>);

/// 本机的 WSL 发行版（不含 Docker Desktop 的内部发行版），列出一次后缓存。
#[derive(Default)]
pub struct WslDistros(Mutex<Option<Vec<String>>>);

impl WslDistros {
    /// 发行版列表；还没有列出时在当前线程列出（要运行 wsl.exe，在后台线程调用）。
    pub fn list(&self) -> Vec<String> {
        let mut cached = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(list) = &*cached {
            return list.clone();
        }
        let list = termlet_core::localshell::wsl::distributions();
        *cached = Some(list.clone());
        list
    }

    /// 已经列出的发行版；还没有列出时为空（界面线程上用，不运行 wsl.exe）。
    pub fn cached(&self) -> Vec<String> {
        let cached = self.0.lock().unwrap_or_else(|e| e.into_inner());
        cached.clone().unwrap_or_default()
    }

    /// 测试中换成给定的发行版。
    #[cfg(test)]
    pub fn set(&self, list: Vec<String>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(list);
    }
}

/// 本机的一个终端：本地 shell 或 WSL 发行版。
#[derive(Clone, Debug)]
pub enum LocalTarget {
    Shell(Profile),
    Wsl(String),
}

impl LocalTarget {
    /// 设置（默认终端、侧边栏中显示的）中的写法。
    pub fn key(&self) -> String {
        match self {
            LocalTarget::Shell(profile) => shell_key(&profile.id),
            LocalTarget::Wsl(distro) => wsl_key(distro),
        }
    }

    /// 列表中的名称（WSL 的是发行版名称，另有"WSL"标记）。
    pub fn name(&self) -> SharedString {
        match self {
            LocalTarget::Shell(profile) => display_name(profile),
            LocalTarget::Wsl(distro) => distro.clone().into(),
        }
    }

    /// Tab 标题：WSL 的写成"发行版 (WSL)"。
    pub fn title(&self) -> SharedString {
        match self {
            LocalTarget::Shell(profile) => display_name(profile),
            LocalTarget::Wsl(distro) => crate::wsl_name(distro, ""),
        }
    }

    /// 侧边栏与 Tab 上的标志：shell 按种类与程序名，WSL 按发行版名称；没有时用终端的图标。
    pub fn brand(&self) -> Option<Brand> {
        match self {
            LocalTarget::Shell(profile) => profile_brand(profile),
            LocalTarget::Wsl(distro) => brand_icons::for_wsl(distro),
        }
    }
}

/// 本地 shell 的标志（见 [`brand_icons::for_shell`]）。
pub fn profile_brand(profile: &Profile) -> Option<Brand> {
    brand_icons::for_shell(profile.kind, &profile.path.to_string_lossy())
}

/// 侧边栏、默认值中本地 shell 的写法。
pub fn shell_key(id: &str) -> String {
    format!("local:{id}")
}

/// 侧边栏、默认值中 WSL 发行版的写法。
pub fn wsl_key(distro: &str) -> String {
    format!("wsl:{distro}")
}

impl Global for LocalShells {}

/// 检测器：整个进程一个（打开终端的后台任务没有 `App`，经这里找到它）。
fn shared_detector() -> Arc<Detector> {
    static DETECTOR: OnceLock<Arc<Detector>> = OnceLock::new();
    DETECTOR
        .get_or_init(|| Arc::new(Detector::new(DETECT_MAX_AGE)))
        .clone()
}

/// 创建检测器并在后台预先检测。
pub fn init(cx: &mut App) {
    let detector = shared_detector();
    let distros = Arc::new(WslDistros::default());
    let (warm, warm_wsl) = (detector.clone(), distros.clone());
    let _ = std::thread::Builder::new()
        .name("detect-shells".into())
        .spawn(move || {
            let found = warm.list(false);
            tracing::info!(count = found.len(), "检测到本地终端");
            let found = warm_wsl.list();
            tracing::info!(count = found.len(), "检测到 WSL 发行版");
        });
    cx.set_global(LocalShells(detector, distros));
}

/// 检测器（设置页在后台线程上读取检测结果）。
pub fn detector(cx: &App) -> Arc<Detector> {
    cx.global::<LocalShells>().0.clone()
}

/// WSL 发行版的缓存（在后台线程上读取）。
pub fn wsl_distros(cx: &App) -> Arc<WslDistros> {
    cx.global::<LocalShells>().1.clone()
}

/// 已经列出的 WSL 发行版（与标题栏“+”旁的菜单相同，不含 Docker Desktop 的内部发行版）；还没有列出时为空。
/// 只读缓存，界面线程上用。
pub fn cached_wsl_distros(cx: &App) -> Vec<String> {
    cx.try_global::<LocalShells>()
        .map(|shells| shells.1.cached())
        .unwrap_or_default()
}

/// 设置中的默认本地终端：可以是 WSL 发行版（`wsl:<发行版>`，按设置直接打开，发行版不在时打开报错）；
/// 没有设置或设置的 shell 已不存在时取 shell 列表中的第一个。
pub fn default_target(settings: &LocalSettings, cx: &App) -> Option<LocalTarget> {
    match settings.shells.default.trim().strip_prefix("wsl:") {
        Some(distro) if !distro.is_empty() => Some(LocalTarget::Wsl(distro.to_string())),
        _ => default_profile(settings, cx).map(LocalTarget::Shell),
    }
}

/// 设置中的默认本地终端（`local:<shell ID>`）；没有设置或已不存在时取列表中的第一个。只读缓存（见模块说明）。
pub fn default_profile(settings: &LocalSettings, cx: &App) -> Option<Profile> {
    let detector = cached_detector(cx);
    let shells = &settings.shells;
    let chosen = shells.default.trim();
    if let Some(chosen) = chosen.strip_prefix("local:") {
        match detector.find_cached(chosen, &shells.custom) {
            Ok(profile) => return Some(profile),
            Err(e) => tracing::warn!(id = chosen, "设置的默认本地终端不可用：{e}"),
        }
    }
    let mut list = detector.cached();
    list.extend(shells.custom.iter().map(Profile::custom));
    list.into_iter().next()
}

/// 全部本地终端（标题栏“+”旁的菜单）：检测到的 shell、自定义的 shell、WSL 发行版。只读缓存（见模块说明）。
pub fn all_targets(settings: &LocalSettings, cx: &App) -> Vec<LocalTarget> {
    let mut targets: Vec<LocalTarget> = cached_detector(cx)
        .cached()
        .into_iter()
        .filter(|p| !p.is_custom())
        .map(LocalTarget::Shell)
        .collect();
    targets.extend(
        settings
            .shells
            .custom
            .iter()
            .map(|s| LocalTarget::Shell(Profile::custom(s))),
    );
    targets.extend(
        cx.global::<LocalShells>()
            .1
            .cached()
            .into_iter()
            .map(LocalTarget::Wsl),
    );
    targets
}

/// 按 shell ID 找本地终端（工作区、侧边栏“本机”中的项）。只读缓存（见模块说明）。
pub fn find_profile(id: &str, settings: &LocalSettings, cx: &App) -> Option<Profile> {
    match cached_detector(cx).find_cached(id, &settings.shells.custom) {
        Ok(profile) => Some(profile),
        Err(e) => {
            tracing::warn!(id, "本地终端不可用：{e}");
            None
        }
    }
}

/// 界面线程上用的检测器：缓存过期时在后台刷新（这次仍用旧的结果）。
fn cached_detector(cx: &App) -> Arc<Detector> {
    let detector = cx.global::<LocalShells>().0.clone();
    if detector.is_stale() {
        let refresh = detector.clone();
        cx.background_executor()
            .spawn(async move {
                refresh.list(false);
            })
            .detach();
    }
    detector
}

/// 启动参数：重新读取用户的环境变量（Termlet 启动之后改过的 PATH 也能生效）。在后台线程调用：
/// 检测到的 shell 已不在原来的位置（卸载、升级后路径变了）时重新检测一次，按新的位置打开。
pub fn launch(profile: &Profile) -> Result<PtyLaunch, ShellError> {
    let launch = match profile.launch() {
        Err(ShellError::ProgramNotFound) if !profile.is_custom() => {
            let moved = shared_detector().find(&profile.id, &[])?;
            moved.launch()?
        }
        other => other?,
    };
    Ok(PtyLaunch {
        program: launch.program,
        args: launch.args,
        cwd: Some(launch.cwd),
        env: Some(launch.env),
    })
}

/// 界面上的名称。
pub fn display_name(profile: &Profile) -> SharedString {
    match profile.kind {
        ShellKind::Pwsh => "PowerShell".into(),
        ShellKind::PowerShell => "Windows PowerShell".into(),
        ShellKind::Cmd => t!("local_shell.cmd"),
        ShellKind::GitBash => "Git Bash".into(),
        ShellKind::Msys2 => "MSYS2".into(),
        ShellKind::Cygwin => "Cygwin".into(),
        ShellKind::VsDev | ShellKind::Unix | ShellKind::Custom => profile.name.clone().into(),
    }
}

/// 打开失败的说明。
pub fn describe_error(error: &ShellError) -> SharedString {
    match error {
        ShellError::UnknownShell => t!("local_shell.error.unknown"),
        ShellError::ProgramNotFound => t!("local_shell.error.program_not_found"),
        ShellError::WslUnsupported => t!("local_shell.error.wsl_unsupported"),
        ShellError::WslNotInstalled => t!("local_shell.error.wsl_not_installed"),
        ShellError::InvalidDistro => t!("local_shell.error.invalid_distro"),
        ShellError::InvalidUser => t!("local_shell.error.invalid_user"),
    }
}
