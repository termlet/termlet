//! 运行环境：系统名称、版本与架构。诊断信息与崩溃记录共用；读一次后缓存。

use std::sync::OnceLock;

use serde::Serialize;

/// 系统信息。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemInfo {
    /// `windows`、`macos`、`linux`
    pub os: String,
    /// `x86_64`、`aarch64`
    pub arch: String,
    /// 版本号：Windows 为 `10.0.26200.6899`，其他系统为内核版本
    pub version: String,
    /// 名称：Windows 为 `Windows 11 Pro 25H2`，其他系统为内核名称
    pub name: String,
    /// 逻辑处理器数
    pub cpus: usize,
}

/// 当前系统的信息（第一次调用时读取）。
pub fn system_info() -> &'static SystemInfo {
    static INFO: OnceLock<SystemInfo> = OnceLock::new();
    INFO.get_or_init(|| {
        let (version, name) = imp::version();
        SystemInfo {
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            version,
            name,
            cpus: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(0),
        }
    })
}

/// 使用统计用的系统大版本（[`crate::usage`]，第一次调用时读取）：只有粗粒度，见 [`windows_coarse`]、[`macos_coarse`]、
/// [`linux_coarse`]。
pub fn coarse_version() -> &'static str {
    static COARSE: OnceLock<String> = OnceLock::new();
    COARSE.get_or_init(imp::coarse_version)
}

/// Windows 的大版本：构建号 ≥ 22000 为 `11`，否则为 `10`；注册表的 DisplayVersion（`22H2`、`25H2`）格式对时加在后面，
/// 读不到或格式不对（较早的 Windows 10 只有 ReleaseId，如 `1909`）时只写大版本。不含构建号、补丁号与版本类型。
pub fn windows_coarse(build: Option<u32>, display_version: &str) -> String {
    let major = if build.is_some_and(|b| b >= 22000) {
        "11"
    } else {
        "10"
    };
    let display = display_version.trim();
    let b = display.as_bytes();
    let valid = b.len() == 4
        && b[0].is_ascii_digit()
        && b[1].is_ascii_digit()
        && b[2] == b'H'
        && b[3].is_ascii_digit();
    if valid {
        format!("{major} {display}")
    } else {
        major.to_string()
    }
}

/// macOS 的大版本，按 Darwin 内核的版本号推算（不受程序链接的 SDK 版本影响，系统给旧程序看的兼容版本号是 `10.16`）：
/// Darwin 20–24 为 macOS 11–15，25 起为 macOS 26 起（macOS 26 的内核是 Darwin 25）；更早的（macOS 10.x）与读不出时为 `10`。
pub fn macos_coarse(darwin_release: &str) -> String {
    let major = darwin_release
        .trim()
        .split('.')
        .next()
        .and_then(|m| m.parse::<u32>().ok())
        .unwrap_or(0);
    match major {
        25..=98 => (major + 1).to_string(),
        20..=24 => (major - 9).to_string(),
        _ => "10".into(),
    }
}

/// 统计中写明的 Linux 发行版（`/etc/os-release` 的 `ID`；openSUSE 的 `opensuse-leap`、`opensuse-tumbleweed` 算作 `opensuse`）。
const LINUX_DISTROS: &[&str] = &[
    "ubuntu",
    "debian",
    "fedora",
    "arch",
    "opensuse",
    "linuxmint",
];

/// Linux 的发行版与版本号：`ID` 在白名单内时为 `ID` 加 `VERSION_ID`（`ubuntu 24.04`、`debian 12`；滚动发行的 Arch 没有
/// 版本号，为 `arch`；版本号格式不对时只写 `ID`），其余（以及读不到 os-release 时）一律为 `other`。
pub fn linux_coarse(os_release: Option<&str>) -> String {
    let Some(text) = os_release else {
        return "other".into();
    };
    let value = |key: &str| {
        text.lines().find_map(|line| {
            let (k, v) = line.trim().split_once('=')?;
            (k.trim() == key).then(|| v.trim().trim_matches(['"', '\'']).to_string())
        })
    };
    let id = value("ID").unwrap_or_default().to_ascii_lowercase();
    let id = if id.starts_with("opensuse") {
        "opensuse".to_string()
    } else {
        id
    };
    if !LINUX_DISTROS.contains(&id.as_str()) {
        return "other".into();
    }
    match value("VERSION_ID").filter(|v| valid_linux_version(v)) {
        Some(version) => format!("{id} {version}"),
        None => id,
    }
}

/// `1`–`8` 位数字，后面最多两段 `.` 加 `1`–`4` 位数字（与接收端的校验相同）。
fn valid_linux_version(version: &str) -> bool {
    let mut parts = version.split('.');
    let first_ok = parts
        .next()
        .is_some_and(|p| (1..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()));
    let rest: Vec<&str> = parts.collect();
    first_ok
        && rest.len() <= 2
        && rest
            .iter()
            .all(|p| (1..=4).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(windows)]
mod imp {
    use windows_registry::LOCAL_MACHINE;

    /// 统计用的大版本：注册表的构建号与 DisplayVersion。
    pub fn coarse_version() -> String {
        let key = LOCAL_MACHINE
            .open(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
            .ok();
        let build = key.as_ref().and_then(|k| {
            k.get_string("CurrentBuildNumber")
                .or_else(|_| k.get_string("CurrentBuild"))
                .ok()
                .and_then(|b| b.trim().parse::<u32>().ok())
        });
        let display = key
            .as_ref()
            .and_then(|k| k.get_string("DisplayVersion").ok())
            .unwrap_or_default();
        super::windows_coarse(build, &display)
    }

    /// 从注册表读出版本号与名称。注册表中的 ProductName 在 Windows 11 上仍写作"Windows 10"，按版本号（22000 起）改正。
    pub fn version() -> (String, String) {
        let Ok(key) = LOCAL_MACHINE.open(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion") else {
            return (String::new(), "Windows".into());
        };
        let major = key.get_u32("CurrentMajorVersionNumber").unwrap_or(10);
        let minor = key.get_u32("CurrentMinorVersionNumber").unwrap_or(0);
        let build = key
            .get_string("CurrentBuildNumber")
            .or_else(|_| key.get_string("CurrentBuild"))
            .unwrap_or_default();
        let mut version = format!("{major}.{minor}.{build}");
        if let Ok(ubr) = key.get_u32("UBR") {
            version.push_str(&format!(".{ubr}"));
        }
        let mut product = key
            .get_string("ProductName")
            .unwrap_or_else(|_| "Windows".into());
        if build.parse::<u32>().is_ok_and(|b| b >= 22000) && product.starts_with("Windows 10") {
            product = product.replacen("Windows 10", "Windows 11", 1);
        }
        let display = key
            .get_string("DisplayVersion")
            .or_else(|_| key.get_string("ReleaseId"))
            .unwrap_or_default();
        let name = if display.is_empty() {
            product
        } else {
            format!("{product} {display}")
        };
        (version, name)
    }
}

#[cfg(unix)]
mod imp {
    use std::ffi::CStr;

    /// 统计用的大版本：macOS 按内核版本推算，其他系统读 os-release（`/etc/os-release`，没有时 `/usr/lib/os-release`）。
    pub fn coarse_version() -> String {
        if cfg!(target_os = "macos") {
            return super::macos_coarse(&version().0);
        }
        let text = std::fs::read_to_string("/etc/os-release")
            .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
            .ok();
        super::linux_coarse(text.as_deref())
    }

    /// `uname` 的内核名称与版本。
    pub fn version() -> (String, String) {
        // SAFETY：utsname 全部是字符数组，全零是合法的值；uname 只写入这个结构
        let mut info: libc::utsname = unsafe { std::mem::zeroed() };
        if unsafe { libc::uname(&mut info) } != 0 {
            return (String::new(), std::env::consts::OS.into());
        }
        // SAFETY：uname 成功后各字段都是以 0 结尾的字符串
        let field = |f: &[libc::c_char]| {
            unsafe { CStr::from_ptr(f.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        };
        (field(&info.release), field(&info.sysname))
    }
}

#[cfg(not(any(windows, unix)))]
mod imp {
    pub fn version() -> (String, String) {
        (String::new(), std::env::consts::OS.into())
    }

    pub fn coarse_version() -> String {
        "other".into()
    }
}
