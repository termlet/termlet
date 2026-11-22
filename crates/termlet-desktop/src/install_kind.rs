//! 这个程序是怎么装的：Windows 的安装程序或便携版、macOS 的应用、Linux 的 deb、rpm、AppImage 或 tar.gz，以及
//! 程序的架构（x64、arm64）。“前往下载”据此打开下载页上对应的一节（[`crate::website::download_for`]），“AI 与自动化”页
//! 据此说明 Linux 上怎么用。
//! 判断写成纯函数（[`package_of`]、[`linux_install`]），输入由 [`InstallEnv::system`] 从这台电脑上读。

use std::path::{Path, PathBuf};

use gpui_kit::{App, OperatingSystem};
use termlet_core::userpath;

/// deb、rpm 装的命令行工具（链接到 `/opt/termlet/bin/termlet`）。
pub(crate) const SYSTEM_CLI: &str = "/usr/bin/termlet";

/// dpkg 记下的 termlet 包的文件清单：有它说明是 deb 装的（没有时 deb、rpm 中就是 rpm）。
const DPKG_LIST: &str = "/var/lib/dpkg/info/termlet.list";

/// Linux 上需要另作说明的两种安装方式（tar.gz 等其余的照原来给出手动链接的命令）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LinuxInstall {
    /// deb、rpm：命令行工具已链接为 [`SYSTEM_CLI`]
    System,
    /// AppImage：AppImage 文件本身（AppRun 按调用的名字运行命令行工具，链接为 termlet 即可）
    AppImage(PathBuf),
}

/// 判断 Linux 上的安装方式（纯函数）：`exe` 为应用程序的路径，`system_link` 为 [`SYSTEM_CLI`] 解析后指向的文件
/// （不存在时为 `None`），`appimage` 为环境变量 `APPIMAGE`。从 AppImage 运行时优先按 AppImage（其中的程序在每次
/// 都变的挂载点下）；`/usr/bin/termlet` 指向这个程序旁边的命令行工具时是 deb、rpm 装的。其他平台两个输入都是
/// `None`，结果总是 `None`。
pub(crate) fn linux_install(
    exe: &Path,
    system_link: Option<&Path>,
    appimage: Option<&Path>,
) -> Option<LinuxInstall> {
    if let Some(image) = appimage.filter(|p| !p.as_os_str().is_empty()) {
        return Some(LinuxInstall::AppImage(image.to_path_buf()));
    }
    let cli = userpath::cli_path(exe);
    system_link
        .is_some_and(|target| target == cli)
        .then_some(LinuxInstall::System)
}

/// 安装的形式（下载页上各有一节）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Package {
    WindowsInstaller,
    WindowsPortable,
    Mac,
    LinuxDeb,
    LinuxRpm,
    LinuxAppImage,
    LinuxTarGz,
    /// Linux，但判断不出是哪种包（自己编译的等）
    Linux,
    /// 其他系统
    Other,
}

/// 程序的架构：下载页上 Windows、Linux 的 x64 与 arm64 的包各有一节（macOS 只有 Apple 芯片版）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Arch {
    #[default]
    X64,
    Arm64,
}

impl Arch {
    /// 这个程序编译的架构（x64 的程序在 arm64 的 Windows 上经仿真运行时也是 x64：更新时照旧取 x64 的包）。
    pub(crate) fn current() -> Self {
        if cfg!(target_arch = "aarch64") {
            Self::Arm64
        } else {
            Self::X64
        }
    }
}

/// 判断安装的形式用到的、这台电脑上的情况。
#[derive(Clone, Debug, Default)]
pub(crate) struct InstallEnv {
    pub os: Option<OperatingSystem>,
    /// 程序的架构
    pub arch: Arch,
    /// 便携版（程序旁边有 portable.txt）
    pub portable: bool,
    /// 应用程序的路径
    pub exe: Option<PathBuf>,
    /// Linux：[`SYSTEM_CLI`] 解析后指向的文件
    pub system_link: Option<PathBuf>,
    /// Linux：环境变量 `APPIMAGE`
    pub appimage: Option<PathBuf>,
    /// Linux：dpkg 中有 termlet 包
    pub dpkg: bool,
    /// Linux：程序旁边有 tar.gz 带的 `install.sh`
    pub tar_gz: bool,
}

impl InstallEnv {
    /// 这台电脑上的情况（Linux 才读 `/usr/bin/termlet`、`APPIMAGE` 等）。
    pub(crate) fn system(cx: &App) -> Self {
        let os = cx.platform_info().os;
        let exe = std::env::current_exe().ok();
        let linux = os == OperatingSystem::Linux;
        Self {
            os: Some(os),
            arch: Arch::current(),
            portable: crate::app_state::AppState::is_portable(cx),
            system_link: linux
                .then(|| std::fs::canonicalize(SYSTEM_CLI).ok())
                .flatten(),
            appimage: linux
                .then(|| std::env::var_os("APPIMAGE"))
                .flatten()
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            dpkg: linux && Path::new(DPKG_LIST).is_file(),
            tar_gz: linux
                && exe
                    .as_deref()
                    .and_then(Path::parent)
                    .is_some_and(|dir| dir.join("install.sh").is_file()),
            exe,
        }
    }

    /// 安装的形式（见 [`package_of`]）。
    pub(crate) fn package(&self) -> Package {
        package_of(self)
    }

    /// 下载页上这种包、这个架构所在的一节（见 [`crate::website::download_for`]）。
    pub(crate) fn download_page(&self) -> String {
        crate::website::download_for(self.package(), self.arch)
    }
}

/// 判断安装的形式（纯函数）。Linux 上 AppImage 优先，其次 deb、rpm（`/usr/bin/termlet` 指向这个程序旁边的命令行
/// 工具，dpkg 中有 termlet 包为 deb，否则为 rpm），再次 tar.gz（程序旁边有 `install.sh`），都不是时说不清是哪种。
pub(crate) fn package_of(env: &InstallEnv) -> Package {
    match env.os {
        Some(OperatingSystem::Windows) if env.portable => Package::WindowsPortable,
        Some(OperatingSystem::Windows) => Package::WindowsInstaller,
        Some(OperatingSystem::Mac) => Package::Mac,
        Some(OperatingSystem::Linux) => {
            let install = env.exe.as_deref().and_then(|exe| {
                linux_install(exe, env.system_link.as_deref(), env.appimage.as_deref())
            });
            match install {
                Some(LinuxInstall::AppImage(_)) => Package::LinuxAppImage,
                Some(LinuxInstall::System) if env.dpkg => Package::LinuxDeb,
                Some(LinuxInstall::System) => Package::LinuxRpm,
                None if env.tar_gz => Package::LinuxTarGz,
                None => Package::Linux,
            }
        }
        _ => Package::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 各系统、各种包：Windows 分安装程序与便携版；Linux 的 AppImage 优先，/usr/bin/termlet 指向这里的是 deb 或
    // rpm（看 dpkg），程序旁边有 install.sh 的是 tar.gz，都不是时说不清
    #[test]
    fn package_follows_the_system_and_layout() {
        let at = |os| InstallEnv {
            os: Some(os),
            ..Default::default()
        };
        assert_eq!(
            package_of(&at(OperatingSystem::Windows)),
            Package::WindowsInstaller
        );
        let portable = InstallEnv {
            portable: true,
            ..at(OperatingSystem::Windows)
        };
        assert_eq!(package_of(&portable), Package::WindowsPortable);
        assert_eq!(package_of(&at(OperatingSystem::Mac)), Package::Mac);
        assert_eq!(package_of(&InstallEnv::default()), Package::Other);

        let exe = PathBuf::from("/opt/termlet/Termlet");
        let linux = InstallEnv {
            exe: Some(exe.clone()),
            ..at(OperatingSystem::Linux)
        };
        assert_eq!(package_of(&linux), Package::Linux, "说不清是哪种");
        let system = InstallEnv {
            system_link: Some(userpath::cli_path(&exe)),
            ..linux.clone()
        };
        assert_eq!(package_of(&system), Package::LinuxRpm);
        let deb = InstallEnv {
            dpkg: true,
            ..system.clone()
        };
        assert_eq!(package_of(&deb), Package::LinuxDeb);
        let image = InstallEnv {
            appimage: Some(PathBuf::from("/home/u/termlet.AppImage")),
            ..deb
        };
        assert_eq!(package_of(&image), Package::LinuxAppImage, "AppImage 优先");
        let tar = InstallEnv {
            tar_gz: true,
            ..linux
        };
        assert_eq!(package_of(&tar), Package::LinuxTarGz);
    }

    // 下载页按包与架构定位：arm64 的程序到 arm64 的那一节，x64 的照旧
    #[test]
    fn download_page_follows_the_arch() {
        use crate::i18n::{Language, language, set_language};
        let original = language();
        set_language(Language::ZhCn);
        let windows = |arch, portable| InstallEnv {
            os: Some(OperatingSystem::Windows),
            arch,
            portable,
            ..Default::default()
        };
        for (env, anchor) in [
            (windows(Arch::X64, false), "#install-win"),
            (windows(Arch::Arm64, false), "#install-win-arm64"),
            (windows(Arch::Arm64, true), "#portable-arm64"),
        ] {
            assert_eq!(
                env.download_page(),
                format!("https://termlet.dev/download/{anchor}")
            );
        }
        assert_eq!(InstallEnv::default().arch, Arch::X64);
        assert_eq!(
            Arch::current() == Arch::Arm64,
            cfg!(target_arch = "aarch64")
        );
        set_language(original);
    }
}
