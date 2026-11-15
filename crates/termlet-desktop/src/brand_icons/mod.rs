//! 系统与品牌标志（Simple Icons；许可与商标说明见 `assets/brand-icons/LICENSE`）：主机列表与 Tab 上的远程系统、
//! 本机 shell，主机工具的 Docker 页，命令行审批框中发起请求的程序。标志与品牌色由 `scripts/brand-icons.py` 生成
//! （[`table`]），这里是对应关系与显示。
//!
//! 对应关系按 Simple Icons 的标识（slug）写在下面几张表中，显示时到打包的标志中查找：没有打包的（Simple Icons 中没有的
//! 微软的 Windows、PowerShell 与 OpenAI，不收的 fish（GPL）与 Fedora（custom 许可））找不到，Linux 发行版落到通用的
//! Linux 标志，其余的调用方用原来的图标（服务器、终端）。以后给某项另配来源时，在生成脚本中收进同一个标识即可。
//!
//! 颜色按设置“通用”中的“图标”（[`IconStyle`]，与 SFTP 的文件图标共用）：单色时与 Lucide 图标一样用文字颜色；
//! 彩色时用品牌色，与背景的对比度不到 3:1（WCAG 对图形的要求）时调亮或调暗，近于黑白灰的品牌色（Apple、AlmaLinux
//! 等是纯黑）在看不清时改用文字颜色。

mod table;

use std::borrow::Cow;

use gpui_kit::assets::IconName;
use gpui_kit::component::{ActiveTheme as _, Icon};
use gpui_kit::{App, Hsla, SharedString, Styled as _};
use termlet_core::localshell::ShellKind;
use termlet_core::model::{IconStyle, RemoteOs};
use termlet_core::schemes::{Rgb, ensure_contrast};

pub use table::Brand;

/// 资源路径的前缀。
const DIR: &str = "brand-icons/";
/// 彩色时品牌色与背景至少要有的对比度（WCAG 1.4.11，非文字的图形）。
const MIN_CONTRAST: f64 = 3.0;
/// 鲜艳程度低于这个的品牌色当作黑白灰：看不清时直接用文字颜色，不调成一片灰。
const NEUTRAL_CHROMA: f64 = 0.12;

impl Brand {
    fn data(self) -> &'static table::BrandData {
        &table::BRANDS[self as usize]
    }

    /// 资源路径（`brand-icons/<slug>.svg`）。
    pub fn path(self) -> SharedString {
        SharedString::new_static(self.data().path)
    }

    /// 品牌色。
    pub fn rgb(self) -> Rgb {
        let hex = self.data().hex;
        Rgb::new((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
    }
}

/// 远程系统 → 标志的标识。
const OS_LOGOS: &[(RemoteOs, &str)] = &[
    (RemoteOs::Ubuntu, "ubuntu"),
    (RemoteOs::Debian, "debian"),
    (RemoteOs::Fedora, "fedora"),
    (RemoteOs::Rhel, "redhat"),
    (RemoteOs::CentOs, "centos"),
    (RemoteOs::Rocky, "rockylinux"),
    (RemoteOs::AlmaLinux, "almalinux"),
    (RemoteOs::Arch, "archlinux"),
    (RemoteOs::Alpine, "alpinelinux"),
    (RemoteOs::Suse, "suse"),
    (RemoteOs::Gentoo, "gentoo"),
    (RemoteOs::Kali, "kalilinux"),
    (RemoteOs::Manjaro, "manjaro"),
    (RemoteOs::Raspbian, "raspberrypi"),
    (RemoteOs::Linux, "linux"),
    (RemoteOs::MacOs, "apple"),
    (RemoteOs::Windows, "windows"),
    (RemoteOs::FreeBsd, "freebsd"),
    (RemoteOs::OpenBsd, "openbsd"),
    (RemoteOs::NetBsd, "netbsd"),
];

/// 本机 shell 的种类 → 标志的标识（Linux、macOS 上检测到的与自定义的按程序名，见 [`PROGRAM_LOGOS`]）。
const SHELL_LOGOS: &[(ShellKind, &str)] = &[
    (ShellKind::GitBash, "gitforwindows"),
    (ShellKind::Pwsh, "powershell"),
    (ShellKind::PowerShell, "powershell"),
    (ShellKind::VsDev, "powershell"),
    (ShellKind::Msys2, "msys2"),
    (ShellKind::Cygwin, "cygwin"),
];

/// shell 的程序名（去掉目录与 `.exe`，小写）→ 标志的标识。
const PROGRAM_LOGOS: &[(&str, &str)] = &[
    ("bash", "gnubash"),
    ("zsh", "zsh"),
    ("fish", "fishshell"),
    ("nu", "nushell"),
    ("nushell", "nushell"),
    ("pwsh", "powershell"),
    ("powershell", "powershell"),
];

/// 命令行审批框中发起请求的程序（进程名）→ 标志的标识：Claude Code、Gemini CLI、Codex（OpenAI）。
const CALLER_LOGOS: &[(&str, &str)] = &[
    ("claude", "claude"),
    ("gemini", "googlegemini"),
    ("codex", "openai"),
];

impl Brand {
    /// 打包的标志中标识为 `slug` 的；没有打包时为 `None`。
    pub fn from_slug(slug: &str) -> Option<Brand> {
        Brand::ALL
            .into_iter()
            .find(|brand| brand.data().slug == slug)
    }
}

fn lookup<K: PartialEq>(table: &[(K, &str)], key: &K) -> Option<Brand> {
    table
        .iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, slug)| Brand::from_slug(slug))
}

/// 远程系统的标志；没有打包时 Linux 发行版用通用的 Linux 标志，其余（Windows）为 `None`。
pub fn for_os(os: RemoteOs) -> Option<Brand> {
    lookup(OS_LOGOS, &os).or_else(|| {
        os.is_linux()
            .then(|| lookup(OS_LOGOS, &RemoteOs::Linux))
            .flatten()
    })
}

/// WSL 发行版的标志（按名称对应系统，见 [`RemoteOs::from_wsl_distro`]）。
pub fn for_wsl(distro: &str) -> Option<Brand> {
    for_os(RemoteOs::from_wsl_distro(distro))
}

/// 本机 shell 的标志：按种类；Linux、macOS 上检测到的与自定义的按程序名（`zsh`、`/bin/bash`、`nu.exe`）。
/// 命令提示符与没有打包标志的为 `None`。
pub fn for_shell(kind: ShellKind, program: &str) -> Option<Brand> {
    match kind {
        ShellKind::Unix | ShellKind::Custom => {
            lookup(PROGRAM_LOGOS, &program_name(program).as_str())
        }
        kind => lookup(SHELL_LOGOS, &kind),
    }
}

/// 命令行审批框中发起请求的程序：调用链（祖先进程名由近到远，以 ` < ` 连接，例如 `bash < node < claude.exe`）中
/// 第一个有标志的 AI 编程工具；其他程序不配。
pub fn for_caller(chain: &str) -> Option<Brand> {
    chain
        .split('<')
        .find_map(|name| lookup(CALLER_LOGOS, &program_name(name).as_str()))
}

/// 主机工具 Docker 页的标志。
pub fn docker() -> Option<Brand> {
    Brand::from_slug("docker")
}

/// 路径或命令中的程序名：去掉目录与 `.exe`，小写。
fn program_name(program: &str) -> String {
    let base = program
        .trim()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match base.strip_suffix(".exe") {
        Some(stem) => stem.to_string(),
        None => base,
    }
}

/// 标志的颜色：单色时是 `mono`（同一位置原来图标的颜色）；彩色时是品牌色，在 `background` 上看不清时调整
/// （近于黑白灰的改用 `foreground`）。
pub fn color(
    brand: Brand,
    style: IconStyle,
    mono: Hsla,
    background: Hsla,
    foreground: Hsla,
) -> Hsla {
    match style {
        IconStyle::Mono => mono,
        IconStyle::Color => legible(brand.rgb(), background, foreground),
    }
}

/// 品牌色在背景上看得清的版本（见模块说明）。
fn legible(brand: Rgb, background: Hsla, foreground: Hsla) -> Hsla {
    let bg = to_rgb(background);
    if brand.contrast(bg) >= MIN_CONTRAST {
        return to_hsla(brand);
    }
    if brand.chroma() < NEUTRAL_CHROMA {
        return foreground;
    }
    to_hsla(ensure_contrast(brand, bg, MIN_CONTRAST).unwrap_or(brand))
}

/// 标志的图标（大小由调用方定），颜色见 [`color`]。
pub fn icon(brand: Brand, color: Hsla) -> Icon {
    Icon::empty().path(brand.path()).text_color(color)
}

/// 有标志时是标志（颜色见 [`color`]，看不清时改用主题的文字颜色），没有时是原来的 `fallback` 图标（颜色 `mono`）。
/// 大小由调用方定。
pub fn icon_or(
    brand: Option<Brand>,
    fallback: IconName,
    style: IconStyle,
    mono: Hsla,
    background: Hsla,
    cx: &App,
) -> Icon {
    match brand {
        Some(brand) => icon(
            brand,
            color(brand, style, mono, background, cx.theme().foreground),
        ),
        None => Icon::new(fallback).text_color(mono),
    }
}

fn to_rgb(color: Hsla) -> Rgb {
    let hex = crate::schemes::hex_of(color);
    Rgb::parse(&hex).unwrap_or(Rgb::BLACK)
}

fn to_hsla(color: Rgb) -> Hsla {
    crate::schemes::hsla(&color.to_hex())
}

/// 资源：`brand-icons/` 下的路径由这里提供（其他路径返回 `None`）。
pub fn load_asset(path: &str) -> Option<Cow<'static, [u8]>> {
    let slug = path.strip_prefix(DIR)?.strip_suffix(".svg")?;
    table::BRANDS
        .iter()
        .find(|data| data.slug == slug)
        .map(|data| Cow::Borrowed(data.svg.as_bytes()))
}

/// `brand-icons/` 下以 `path` 开头的资源路径。
pub fn list_assets(path: &str) -> Vec<SharedString> {
    if !(path.starts_with(DIR) || DIR.starts_with(path)) {
        return Vec::new();
    }
    table::BRANDS
        .iter()
        .filter(|data| data.path.starts_with(path))
        .map(|data| SharedString::new_static(data.path))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // 生成的表与枚举一一对应，资源都能找到，SVG 是一条路径
    #[test]
    fn table_matches_enum_and_assets() {
        assert_eq!(Brand::ALL.len(), table::BRANDS.len());
        for (i, brand) in Brand::ALL.into_iter().enumerate() {
            assert_eq!(brand as usize, i);
            let path = brand.path();
            assert_eq!(
                path.as_ref(),
                format!("brand-icons/{}.svg", brand.data().slug)
            );
            let svg = load_asset(&path).expect("资源应存在");
            let svg = std::str::from_utf8(&svg).unwrap();
            assert!(svg.starts_with("<svg ") && svg.contains("viewBox=\"0 0 24 24\""));
            assert_eq!(svg.matches("<path ").count(), 1, "{}", brand.data().slug);
        }
        assert!(load_asset("brand-icons/windows.svg").is_none());
        assert!(load_asset("icons/server.svg").is_none());
        assert_eq!(list_assets("brand-icons/").len(), Brand::ALL.len());
        assert_eq!(list_assets("").len(), Brand::ALL.len());
        assert!(list_assets("icons/").is_empty());
    }

    // 每种系统都有对应（Windows 除外），各不相同
    #[test]
    fn every_os_but_windows_has_a_logo() {
        let mut seen = Vec::new();
        for os in RemoteOs::ALL {
            assert!(OS_LOGOS.iter().any(|(o, _)| *o == os), "{os:?} 不在表中");
            match for_os(os) {
                // 没有打包专门标志的发行版（Fedora）用通用的 Linux 标志
                Some(Brand::Linux) if os != RemoteOs::Linux => {
                    assert_eq!(os, RemoteOs::Fedora);
                }
                Some(brand) => {
                    assert!(!seen.contains(&brand), "{os:?}");
                    seen.push(brand);
                }
                None => assert_eq!(os, RemoteOs::Windows),
            }
        }
        assert_eq!(for_wsl("Ubuntu-24.04"), Some(Brand::Ubuntu));
        assert_eq!(for_wsl("FedoraLinux-42"), Some(Brand::Linux));
        assert_eq!(for_wsl("docker-desktop"), Some(Brand::Linux));
        assert_eq!(docker(), Some(Brand::Docker));
    }

    // 表中的标识要么已打包，要么是 Simple Icons 中没有、不收的（以后另配来源时收进同一个标识）
    #[test]
    fn unbundled_slugs_are_known() {
        const NOT_BUNDLED: &[&str] = &[
            "fedora",
            "windows",
            "powershell",
            "msys2",
            "cygwin",
            "fishshell",
            "openai",
        ];
        let slugs = OS_LOGOS
            .iter()
            .map(|(_, s)| *s)
            .chain(SHELL_LOGOS.iter().map(|(_, s)| *s))
            .chain(PROGRAM_LOGOS.iter().map(|(_, s)| *s))
            .chain(CALLER_LOGOS.iter().map(|(_, s)| *s));
        for slug in slugs {
            assert!(
                Brand::from_slug(slug).is_some() || NOT_BUNDLED.contains(&slug),
                "{slug} 既没有打包也不在不收的名单中"
            );
        }
        for slug in NOT_BUNDLED {
            assert_eq!(Brand::from_slug(slug), None, "{slug} 已打包，从名单中去掉");
        }
    }

    #[test]
    fn shells_by_kind_and_program() {
        for (kind, program, brand) in [
            (
                ShellKind::GitBash,
                r"C:\Program Files\Git\bin\bash.exe",
                Some(Brand::GitForWindows),
            ),
            (ShellKind::Unix, "/bin/bash", Some(Brand::GnuBash)),
            (ShellKind::Unix, "/usr/bin/zsh", Some(Brand::Zsh)),
            (ShellKind::Unix, "zsh", Some(Brand::Zsh)),
            (
                ShellKind::Unix,
                "/opt/homebrew/bin/nu",
                Some(Brand::Nushell),
            ),
            (ShellKind::Unix, "/usr/bin/fish", None),
            (ShellKind::Unix, "/bin/sh", None),
            (ShellKind::Custom, r"D:\tools\nu.exe", Some(Brand::Nushell)),
            (ShellKind::Custom, "BASH.EXE", Some(Brand::GnuBash)),
            (ShellKind::Custom, r"C:\Windows\System32\wsl.exe", None),
            (ShellKind::Pwsh, "pwsh.exe", None),
            (ShellKind::PowerShell, "powershell.exe", None),
            (ShellKind::Cmd, "cmd.exe", None),
            (ShellKind::Msys2, "bash.exe", None),
            (ShellKind::Cygwin, "bash.exe", None),
            (ShellKind::VsDev, "powershell.exe", None),
        ] {
            assert_eq!(for_shell(kind, program), brand, "{kind:?} {program}");
        }
    }

    #[test]
    fn callers() {
        assert_eq!(for_caller("bash < claude"), Some(Brand::Claude));
        assert_eq!(
            for_caller("bash.exe < node.exe < claude.exe < WindowsTerminal.exe"),
            Some(Brand::Claude)
        );
        assert_eq!(for_caller("sh < gemini < zsh"), Some(Brand::GoogleGemini));
        assert_eq!(for_caller("bash < Claude.EXE"), Some(Brand::Claude));
        // 由近到远取第一个
        assert_eq!(for_caller("gemini < claude"), Some(Brand::GoogleGemini));
        for none in [
            "",
            "bash < node < Code.exe",
            "codex < zsh",
            "claude-helper",
            "pwsh.exe",
        ] {
            assert_eq!(for_caller(none), None, "{none}");
        }
    }

    fn hex(color: Hsla) -> String {
        crate::schemes::hex_of(color)
    }

    // 彩色：看得清的品牌色原样；看不清的调亮（深色背景）或调暗（浅色背景）到 3:1；纯黑在深色背景上改用文字颜色
    #[test]
    fn brand_colors_stay_legible() {
        let dark_bg = crate::schemes::hsla("#1e1e1e");
        let light_bg = crate::schemes::hsla("#ffffff");
        let dark_fg = crate::schemes::hsla("#e5e5e5");
        let light_fg = crate::schemes::hsla("#111111");
        let mono = crate::schemes::hsla("#888888");
        assert_eq!(
            hex(color(
                Brand::Ubuntu,
                IconStyle::Mono,
                mono,
                dark_bg,
                dark_fg
            )),
            "#888888"
        );
        assert_eq!(
            hex(color(
                Brand::Ubuntu,
                IconStyle::Color,
                mono,
                dark_bg,
                dark_fg
            )),
            "#e95420"
        );
        assert_eq!(
            hex(color(
                Brand::Apple,
                IconStyle::Color,
                mono,
                dark_bg,
                dark_fg
            )),
            "#e5e5e5"
        );
        assert_eq!(
            hex(color(
                Brand::Apple,
                IconStyle::Color,
                mono,
                light_bg,
                light_fg
            )),
            "#000000"
        );
        for brand in Brand::ALL {
            for (bg, fg) in [(dark_bg, dark_fg), (light_bg, light_fg)] {
                let c = to_rgb(color(brand, IconStyle::Color, mono, bg, fg));
                let ratio = c.contrast(to_rgb(bg));
                assert!(
                    ratio >= MIN_CONTRAST - 0.01,
                    "{} 只有 {ratio:.2}:1",
                    brand.data().slug
                );
            }
        }
        // Debian 的深红在深色背景上调亮，Linux 的黄色在白底上调暗
        let debian = to_rgb(color(
            Brand::Debian,
            IconStyle::Color,
            mono,
            dark_bg,
            dark_fg,
        ));
        assert!(debian.luminance() > Brand::Debian.rgb().luminance());
        let linux = to_rgb(color(
            Brand::Linux,
            IconStyle::Color,
            mono,
            light_bg,
            light_fg,
        ));
        assert!(linux.luminance() < Brand::Linux.rgb().luminance());
    }
}
