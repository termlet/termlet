//! 版本描述文件：electron-builder 的 `latest.yml` 格式，发布脚本 `scripts/release.mjs` 生成：
//!
//! ```yaml
//! version: 1.2.0
//! files:
//!   - url: termlet-1.2.0-win-x64.exe
//!     sha512: <SHA-512 的 base64>
//!     size: 12345678
//! path: termlet-1.2.0-win-x64.exe
//! sha512: <SHA-512 的 base64>
//! releaseDate: '2026-10-15T08:00:00.000Z'
//! ```
//!
//! 整份文件有签名（`latest.yml.minisig`），签了它就保护了其中列出的安装包。

use std::io::Read as _;
use std::path::Path;

use data_encoding::BASE64;
use serde::Deserialize;
use sha2::{Digest as _, Sha512};
use url::Url;

use super::UpdateError;

/// 版本描述文件。
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub version: String,
    #[serde(default)]
    pub files: Vec<ManifestFile>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub sha512: Option<String>,
    #[serde(default, rename = "releaseDate")]
    pub release_date: Option<String>,
}

/// 版本描述文件中列出的一个安装包。
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ManifestFile {
    /// 相对于更新源的地址（也可以是完整地址）
    pub url: String,
    /// SHA-512 的 base64
    pub sha512: String,
    #[serde(default)]
    pub size: Option<u64>,
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self, UpdateError> {
        crate::yaml::from_str(text).map_err(|e| UpdateError::Manifest(e.to_string()))
    }

    /// 本机的安装包：`files` 中文件名以 `-<平台>.exe` 结尾的那个（例如 `termlet-1.2.0-win-x64.exe`）。
    pub fn installer(&self, platform: &str) -> Option<&ManifestFile> {
        let suffix = format!("-{platform}.exe").to_ascii_lowercase();
        self.files
            .iter()
            .find(|f| f.url.to_ascii_lowercase().ends_with(&suffix))
    }
}

/// 当前平台对应的版本描述文件（见 [`metadata_file_for`]）。
pub fn metadata_file() -> String {
    metadata_file_for(std::env::consts::OS, std::env::consts::ARCH)
}

/// 系统与架构（`std::env::consts` 的写法）对应的版本描述文件，与发布脚本 `scripts/release.mjs` 的 `manifestName`
/// 相同：x64 沿用最初的 `latest.yml`、`latest-linux.yml`（已发布的 x64 客户端读的就是它们，内容也只列 x64 的包），
/// 其他架构在后面加 `-<架构>`：Windows arm64 是 `latest-arm64.yml`，Linux arm64 是 `latest-linux-arm64.yml`。
/// macOS 只出 Apple 芯片版，都是 `latest-mac.yml`。签名的可信注释带文件名，一个架构的清单冒充不了另一个的。
pub fn metadata_file_for(os: &str, arch: &str) -> String {
    let base = match os {
        "macos" => return "latest-mac.yml".into(),
        "linux" => "latest-linux",
        _ => "latest",
    };
    match node_arch(arch) {
        "x64" => format!("{base}.yml"),
        other => format!("{base}-{other}.yml"),
    }
}

/// 架构的写法与 Node 的 `process.arch` 相同：x64、arm64。
fn node_arch(arch: &str) -> &str {
    match arch {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    }
}

/// 安装包文件名中的平台，例如 `win-x64`、`win-arm64`（见 [`platform_tag_for`]）。
pub fn platform_tag() -> String {
    platform_tag_for(std::env::consts::OS, std::env::consts::ARCH)
}

/// 系统与架构（`std::env::consts` 的写法）在安装包文件名中的写法：`win-x64`、`win-arm64`、`linux-arm64`、`mac-arm64`。
pub fn platform_tag_for(os: &str, arch: &str) -> String {
    let os = match os {
        "windows" => "win",
        "macos" => "mac",
        other => other,
    };
    format!("{os}-{}", node_arch(arch))
}

/// 下载后保存用的文件名：地址的最后一段，只允许字母、数字与 `.` `_` `-`，不能以点开头。
pub fn file_name(url: &Url) -> Option<String> {
    let name = url.path_segments()?.next_back()?;
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'));
    ok.then(|| name.to_string())
}

/// 内容的 SHA-512（base64，与版本描述文件中的写法相同；测试中构造版本描述文件）。
#[cfg(test)]
pub(crate) fn sha512_base64(data: &[u8]) -> String {
    BASE64.encode(&Sha512::digest(data))
}

/// 文件的 SHA-512（base64）。
pub fn sha512_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha512::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(BASE64.encode(&hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = "version: 2.0.0\nfiles:\n  - url: termlet-2.0.0-win-arm64.exe\n    sha512: BBBB\n    size: 8\n  - url: termlet-2.0.0-win-x64.exe\n    sha512: AAAA\n    size: 9\n    blockMapSize: 1\npath: termlet-2.0.0-win-x64.exe\nsha512: AAAA\nreleaseDate: '2026-10-15T00:00:00.000Z'\n";

    #[test]
    fn parses_and_picks_the_installer_for_this_platform() {
        let m = Manifest::parse(MANIFEST).unwrap();
        assert_eq!(m.version, "2.0.0");
        assert_eq!(m.files.len(), 2);
        let x64 = m.installer("win-x64").unwrap();
        assert_eq!(x64.url, "termlet-2.0.0-win-x64.exe");
        assert_eq!(x64.sha512, "AAAA");
        assert_eq!(x64.size, Some(9));
        assert_eq!(m.installer("win-arm64").unwrap().sha512, "BBBB");
        assert!(m.installer("mac-arm64").is_none());
        assert_eq!(m.release_date.as_deref(), Some("2026-10-15T00:00:00.000Z"));
        assert!(Manifest::parse("files: []\n").is_err());
        assert!(Manifest::parse("version: [1]\n").is_err());
    }

    /// YAML 的边界情况。“旧：”标出与原来的 serde_yaml_ng 不同的结果。
    #[test]
    fn yaml_edge_cases() {
        let version = |text: &str| Manifest::parse(text).map(|m| m.version).ok();
        // 像数字、布尔值的版本号按原样作为文本（1.10 不会变成 1.1）
        assert_eq!(version("version: 1.10\n").as_deref(), Some("1.10"));
        assert_eq!(version("version: 2\n").as_deref(), Some("2"));
        assert_eq!(version("version: true\n").as_deref(), Some("true"));
        assert_eq!(version("version: '1.10'\n").as_deref(), Some("1.10"));
        // 旧：解析为文本 "~"
        assert_eq!(version("version: ~\n"), None);
        // 重复的键、多个文档、空文件报错
        assert_eq!(version("version: 1.0.0\nversion: 2.0.0\n"), None);
        assert_eq!(version("version: 1.0.0\n---\nversion: 2.0.0\n"), None);
        assert_eq!(version(""), None);
        // 旧：带 BOM（与 CRLF）时报错
        assert_eq!(
            version("\u{feff}version: 1.0.0\r\npath: a\r\n").as_deref(),
            Some("1.0.0")
        );

        let size = |value: &str| {
            Manifest::parse(&format!(
                "version: 1.0.0\nfiles:\n  - {{url: a, sha512: b, size: {value}}}\n"
            ))
            .map(|m| m.files[0].size)
            .ok()
        };
        assert_eq!(size("0x10"), Some(Some(16)));
        assert_eq!(size("'123'"), Some(Some(123))); // 旧：报错
        assert_eq!(size("-1"), None);
        assert_eq!(size("1.0"), None);
        assert_eq!(size("~"), Some(None));

        let m = Manifest::parse(
            "version: &v 1.0.0\npath: *v\nsha512: ~\nreleaseDate: 2026-10-15T08:00:00.000Z\nextra: {a: [1]}\nfiles:\n  - {url: 123, sha512: yes}\n",
        )
        .unwrap();
        assert_eq!(m.path.as_deref(), Some("1.0.0"));
        assert_eq!(m.sha512, None);
        assert_eq!(m.release_date.as_deref(), Some("2026-10-15T08:00:00.000Z"));
        assert_eq!(
            (m.files[0].url.as_str(), m.files[0].sha512.as_str()),
            ("123", "yes")
        );
    }

    // 与发布的包名（termlet-<版本>-<平台>.<扩展名>）及发布脚本 release.mjs 的 manifestName 相同。x64 的两份清单
    // 名不能变：已发布的 x64 客户端读的就是 latest.yml、latest-linux.yml
    #[test]
    fn platform_names_match_the_release_script() {
        for (os, arch, file, platform) in [
            ("windows", "x86_64", "latest.yml", "win-x64"),
            ("windows", "aarch64", "latest-arm64.yml", "win-arm64"),
            ("linux", "x86_64", "latest-linux.yml", "linux-x64"),
            ("linux", "aarch64", "latest-linux-arm64.yml", "linux-arm64"),
            ("macos", "aarch64", "latest-mac.yml", "mac-arm64"),
            ("macos", "x86_64", "latest-mac.yml", "mac-x64"),
        ] {
            assert_eq!(metadata_file_for(os, arch), file, "{os} {arch}");
            assert_eq!(platform_tag_for(os, arch), platform, "{os} {arch}");
        }
        assert_eq!(
            metadata_file(),
            metadata_file_for(std::env::consts::OS, std::env::consts::ARCH)
        );
        assert_eq!(
            platform_tag(),
            platform_tag_for(std::env::consts::OS, std::env::consts::ARCH)
        );
        if cfg!(all(windows, target_arch = "x86_64")) {
            assert_eq!(metadata_file(), "latest.yml");
            assert_eq!(platform_tag(), "win-x64");
        }
        if cfg!(all(windows, target_arch = "aarch64")) {
            assert_eq!(metadata_file(), "latest-arm64.yml");
            assert_eq!(platform_tag(), "win-arm64");
        }
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            assert_eq!(metadata_file(), "latest-linux.yml");
            assert_eq!(platform_tag(), "linux-x64");
        }
        if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
            assert_eq!(metadata_file(), "latest-linux-arm64.yml");
            assert_eq!(platform_tag(), "linux-arm64");
        }
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            assert_eq!(metadata_file(), "latest-mac.yml");
            assert_eq!(platform_tag(), "mac-arm64");
        }
    }

    /// Windows arm64 的清单（latest-arm64.yml）照 x64 的写法先列安装程序、再列便携版：arm64 的客户端只取
    /// `-win-arm64.exe`，便携版 zip 与 x64 的包都不会被当成安装包；x64 的 latest.yml 中没有 arm64 的包。
    #[test]
    fn arm64_manifests_list_only_their_own_packages() {
        let entry =
            |url: &str, sha: &str| format!("  - url: {url}\n    sha512: {sha}\n    size: 9\n");
        let arm64 = format!(
            "version: 0.3.0\nfiles:\n{}{}path: termlet-0.3.0-win-arm64.exe\nsha512: CCCC\n",
            entry("termlet-0.3.0-win-arm64.exe", "CCCC"),
            entry("termlet-0.3.0-win-arm64-portable.zip", "DDDD"),
        );
        let m = Manifest::parse(&arm64).unwrap();
        let installer = m.installer("win-arm64").unwrap();
        assert_eq!(
            (installer.url.as_str(), installer.sha512.as_str()),
            ("termlet-0.3.0-win-arm64.exe", "CCCC")
        );
        assert!(m.installer("win-x64").is_none());

        let x64 = format!(
            "version: 0.3.0\nfiles:\n{}{}path: termlet-0.3.0-win-x64.exe\nsha512: AAAA\n",
            entry("termlet-0.3.0-win-x64.exe", "AAAA"),
            entry("termlet-0.3.0-win-x64-portable.zip", "BBBB"),
        );
        let m = Manifest::parse(&x64).unwrap();
        assert_eq!(m.installer("win-x64").unwrap().sha512, "AAAA");
        assert!(m.installer("win-arm64").is_none());

        let linux = format!(
            "version: 0.3.0\nfiles:\n{}{}{}{}path: termlet-0.3.0-linux-arm64.tar.gz\nsha512: AAAA\n",
            entry("termlet-0.3.0-linux-arm64.tar.gz", "AAAA"),
            entry("termlet-0.3.0-linux-arm64.deb", "BBBB"),
            entry("termlet-0.3.0-linux-arm64.rpm", "CCCC"),
            entry("termlet-0.3.0-linux-arm64.AppImage", "DDDD"),
        );
        let m = Manifest::parse(&linux).unwrap();
        assert_eq!(m.files.len(), 4);
        assert!(m.installer("linux-arm64").is_none());
    }

    /// 发布脚本给 Linux、macOS 的包写的清单与 Windows 的格式相同；这两个平台只提示新版本，
    /// 清单中的包不是安装包（`installer` 只认 Windows 的 .exe）。
    #[test]
    fn parses_the_linux_and_mac_manifests() {
        for package in [
            "termlet-0.1.0-linux-x64.tar.gz",
            "termlet-0.1.0-mac-arm64.zip",
        ] {
            let m = Manifest::parse(&format!(
                "version: 0.1.0\nfiles:\n  - url: {package}\n    sha512: AAAA\n    size: 32102105\npath: {package}\nsha512: AAAA\nreleaseDate: '2026-10-30T02:00:00.000Z'\n"
            ))
            .unwrap();
            assert_eq!(m.version, "0.1.0");
            assert_eq!(m.files.len(), 1);
            assert_eq!(m.files[0].url, package);
            assert_eq!(m.files[0].size, Some(32102105));
            assert_eq!(m.path.as_deref(), Some(package));
            for platform in ["linux-x64", "mac-arm64", "win-x64"] {
                assert!(m.installer(platform).is_none(), "{package} {platform}");
            }
        }
    }

    /// 发布脚本写的清单可以列出同一平台的多个包：Windows 先列安装程序、再列便携版 zip，Linux 依次列出 tar.gz、
    /// deb、rpm、AppImage。`installer` 照旧只取 Windows 的安装程序，多出来的项不影响解析。
    #[test]
    fn manifests_with_several_packages() {
        let entry =
            |url: &str, sha: &str| format!("  - url: {url}\n    sha512: {sha}\n    size: 9\n");
        let windows = format!(
            "version: 0.2.0\nfiles:\n{}{}path: termlet-0.2.0-win-x64.exe\nsha512: AAAA\nreleaseDate: '2026-11-04T00:00:00.000Z'\n",
            entry("termlet-0.2.0-win-x64.exe", "AAAA"),
            entry("termlet-0.2.0-win-x64-portable.zip", "BBBB"),
        );
        let m = Manifest::parse(&windows).unwrap();
        assert_eq!(m.files.len(), 2);
        let installer = m.installer("win-x64").unwrap();
        assert_eq!(
            (installer.url.as_str(), installer.sha512.as_str()),
            ("termlet-0.2.0-win-x64.exe", "AAAA")
        );
        assert!(m.installer("win-arm64").is_none());

        let linux = format!(
            "version: 0.2.0\nfiles:\n{}{}{}{}path: termlet-0.2.0-linux-x64.tar.gz\nsha512: AAAA\n",
            entry("termlet-0.2.0-linux-x64.tar.gz", "AAAA"),
            entry("termlet-0.2.0-linux-x64.deb", "BBBB"),
            entry("termlet-0.2.0-linux-x64.rpm", "CCCC"),
            entry("termlet-0.2.0-linux-x64.AppImage", "DDDD"),
        );
        let m = Manifest::parse(&linux).unwrap();
        assert_eq!((m.version.as_str(), m.files.len()), ("0.2.0", 4));
        assert!(m.installer("linux-x64").is_none());
    }

    #[test]
    fn file_names_are_restricted() {
        let base = Url::parse("https://example.com/feed/").unwrap();
        let name = |s: &str| file_name(&base.join(s).unwrap());
        assert_eq!(
            name("termlet-2.0.0-win-x64.exe").as_deref(),
            Some("termlet-2.0.0-win-x64.exe")
        );
        assert_eq!(
            name("https://cdn.example.com/a/termlet.exe?x=1").as_deref(),
            Some("termlet.exe")
        );
        for bad in ["dir/", ".hidden.exe", "a%20b.exe", "..%2f..%2fx.exe"] {
            assert_eq!(name(bad), None, "{bad}");
        }
    }

    #[test]
    fn sha512_matches_the_manifest_encoding() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.exe");
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&file, &data).unwrap();
        assert_eq!(sha512_file(&file).unwrap(), sha512_base64(&data));
        // 与 Node 的 createHash('sha512').update('abc').digest('base64') 相同
        assert_eq!(
            sha512_base64(b"abc"),
            "3a81oZNherrMQXNJriBBMRLm+k6JqX6iCp7u5ktV05ohkpkqJ0/BqDa6PCOj/uu9RU1EI2Q86A4qmslPpUyknw=="
        );
        assert!(sha512_file(&tmp.path().join("missing")).is_err());
    }
}
