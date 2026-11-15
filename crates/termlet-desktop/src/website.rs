//! 官网上的页面、反馈的邮箱与源代码仓库：设置的“关于”、更新提示、macOS 菜单栏的“帮助”都从这里取。
//!
//! 官网只有中文与英文：简体、繁體中文界面打开中文的，其余（英文、日文、韩文）打开英文的（`/en/…`）；地址都以斜杠
//! 结尾（官网的地址都是，不带会多跳转一次）。

use termlet_core::update::{DOWNLOAD_PAGE, DOWNLOAD_PAGE_EN};
use termlet_core::usage::{PRIVACY_URL, PRIVACY_URL_EN};

use crate::i18n::{Language, language};
use crate::install_kind::{Arch, Package};

/// 反馈的邮箱（官网“常见问题”、隐私说明中的联系方式）。
pub(crate) const FEEDBACK_MAIL: &str = "mailto:hi@termlet.dev";

/// 源代码所在的公开仓库（AGPL-3.0-or-later）。
pub(crate) const SOURCE_CODE: &str = "https://github.com/termlet/termlet";

const HOME: &str = "https://termlet.dev/";
const HOME_EN: &str = "https://termlet.dev/en/";
const CHANGELOG: &str = "https://termlet.dev/changelog/";
const CHANGELOG_EN: &str = "https://termlet.dev/en/changelog/";
const LICENSE: &str = "https://termlet.dev/license/";
const LICENSE_EN: &str = "https://termlet.dev/en/license/";

/// 按界面语言在中文、英文两个地址中选一个。
fn localized(chinese: &'static str, english: &'static str) -> &'static str {
    match language() {
        Language::En | Language::Ja | Language::Ko => english,
        Language::ZhCn | Language::ZhTw => chinese,
    }
}

/// 官网首页。
pub(crate) fn home() -> &'static str {
    localized(HOME, HOME_EN)
}

/// 下载页上这种包所在的一节（下载页中各节的 id：各平台的安装说明、Windows 便携版与 Linux 各种格式的小节）。
/// x64 的沿用最初的 id；arm64 的 Windows 安装版、便携版与 Linux 各种格式另有一节（macOS 只有 Apple 芯片版，
/// 判断不出是哪种 Linux 包时都到 Linux 一节）。
fn download_anchor(package: Package, arch: Arch) -> &'static str {
    match (package, arch) {
        (Package::WindowsInstaller, Arch::X64) => "#install-win",
        (Package::WindowsInstaller, Arch::Arm64) => "#install-win-arm64",
        // 安装程序会装成安装版，便携版要下便携版
        (Package::WindowsPortable, Arch::X64) => "#portable",
        (Package::WindowsPortable, Arch::Arm64) => "#portable-arm64",
        (Package::Mac, _) => "#install-mac",
        (Package::LinuxDeb, Arch::X64) => "#linux-x64-deb",
        (Package::LinuxDeb, Arch::Arm64) => "#linux-arm64-deb",
        (Package::LinuxRpm, Arch::X64) => "#linux-x64-rpm",
        (Package::LinuxRpm, Arch::Arm64) => "#linux-arm64-rpm",
        (Package::LinuxAppImage, Arch::X64) => "#linux-x64-appimage",
        (Package::LinuxAppImage, Arch::Arm64) => "#linux-arm64-appimage",
        (Package::LinuxTarGz, Arch::X64) => "#linux-x64-targz",
        (Package::LinuxTarGz, Arch::Arm64) => "#linux-arm64-targz",
        (Package::Linux, _) => "#install-linux",
        (Package::Other, _) => "",
    }
}

/// 下载页（“前往下载”、标题栏的“新版本可用”），定位到这种包、这个架构所在的一节。
pub(crate) fn download_for(package: Package, arch: Arch) -> String {
    let page = localized(DOWNLOAD_PAGE, DOWNLOAD_PAGE_EN);
    format!("{page}{}", download_anchor(package, arch))
}

/// 隐私说明。
pub(crate) fn privacy() -> &'static str {
    localized(PRIVACY_URL, PRIVACY_URL_EN)
}

/// 更新日志。
pub(crate) fn changelog() -> &'static str {
    localized(CHANGELOG, CHANGELOG_EN)
}

/// 许可证的说明（AGPL-3.0-or-later 的要点、仓库与许可证全文的链接、名称与标志的说明）。
pub(crate) fn license() -> &'static str {
    localized(LICENSE, LICENSE_EN)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::set_language;

    /// 简体、繁體中文界面用中文的页面，其余用英文的；地址都以斜杠结尾（下载页另按包的种类带锚点）。
    #[test]
    fn pages_follow_the_language() {
        let original = language();
        for other in [Language::Ja, Language::Ko] {
            set_language(other);
            assert_eq!(home(), "https://termlet.dev/en/");
            assert_eq!(privacy(), "https://termlet.dev/en/privacy/");
        }
        set_language(Language::En);
        assert_eq!(home(), "https://termlet.dev/en/");
        assert_eq!(
            download_for(Package::Other, Arch::X64),
            "https://termlet.dev/en/download/"
        );
        assert_eq!(
            download_for(Package::WindowsPortable, Arch::X64),
            "https://termlet.dev/en/download/#portable"
        );
        assert_eq!(
            download_for(Package::WindowsPortable, Arch::Arm64),
            "https://termlet.dev/en/download/#portable-arm64"
        );
        assert_eq!(privacy(), "https://termlet.dev/en/privacy/");
        assert_eq!(changelog(), "https://termlet.dev/en/changelog/");
        assert_eq!(license(), "https://termlet.dev/en/license/");
        for chinese in [Language::ZhCn, Language::ZhTw] {
            set_language(chinese);
            assert_eq!(home(), "https://termlet.dev/");
            assert_eq!(
                download_for(Package::WindowsPortable, Arch::X64),
                "https://termlet.dev/download/#portable"
            );
            assert_eq!(
                download_for(Package::LinuxAppImage, Arch::X64),
                "https://termlet.dev/download/#linux-x64-appimage"
            );
            assert_eq!(privacy(), "https://termlet.dev/privacy/");
            assert_eq!(changelog(), "https://termlet.dev/changelog/");
            assert_eq!(license(), "https://termlet.dev/license/");
        }
        set_language(original);
    }

    /// 下载页上各种包的小节（与官网下载页中的 id 一致：#install-win 等、#portable、#linux-x64-deb 等；arm64 的
    /// #install-win-arm64、#portable-arm64、#linux-arm64-deb 等）。x64 的 id 与 arm64 版发布前的相同。
    #[test]
    fn download_anchors_match_the_page() {
        let original = language();
        set_language(Language::ZhCn);
        for (package, arch, anchor) in [
            (Package::WindowsInstaller, Arch::X64, "#install-win"),
            (Package::WindowsPortable, Arch::X64, "#portable"),
            (Package::Mac, Arch::X64, "#install-mac"),
            (Package::LinuxDeb, Arch::X64, "#linux-x64-deb"),
            (Package::LinuxRpm, Arch::X64, "#linux-x64-rpm"),
            (Package::LinuxAppImage, Arch::X64, "#linux-x64-appimage"),
            (Package::LinuxTarGz, Arch::X64, "#linux-x64-targz"),
            (Package::Linux, Arch::X64, "#install-linux"),
            (Package::WindowsInstaller, Arch::Arm64, "#install-win-arm64"),
            (Package::WindowsPortable, Arch::Arm64, "#portable-arm64"),
            (Package::Mac, Arch::Arm64, "#install-mac"),
            (Package::LinuxDeb, Arch::Arm64, "#linux-arm64-deb"),
            (Package::LinuxRpm, Arch::Arm64, "#linux-arm64-rpm"),
            (Package::LinuxAppImage, Arch::Arm64, "#linux-arm64-appimage"),
            (Package::LinuxTarGz, Arch::Arm64, "#linux-arm64-targz"),
            (Package::Linux, Arch::Arm64, "#install-linux"),
        ] {
            assert_eq!(
                download_for(package, arch),
                format!("https://termlet.dev/download/{anchor}"),
                "{package:?} {arch:?}"
            );
        }
        set_language(original);
    }
}
