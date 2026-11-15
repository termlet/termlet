//! 远程主机的系统：SSH 连接后自动识别（见 [`crate::osprobe`]），界面据此显示系统的标志。

/// 识别出的系统。键（[`RemoteOs::key`]）存进主机（[`super::Host::os`]）并随同步传到其他设备，发布后不能改名；
/// 新版本加的键旧版本不认识，当作没有识别（见 [`RemoteOs::from_key`]）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RemoteOs {
    Ubuntu,
    Debian,
    Fedora,
    /// Red Hat Enterprise Linux
    Rhel,
    CentOs,
    Rocky,
    AlmaLinux,
    Arch,
    Alpine,
    /// openSUSE 与 SUSE Linux Enterprise
    Suse,
    Gentoo,
    Kali,
    Manjaro,
    /// Raspberry Pi OS（32 位版的 ID 是 raspbian；64 位版报告为 debian）
    Raspbian,
    /// 认不出发行版的 Linux
    Linux,
    MacOs,
    Windows,
    FreeBsd,
    OpenBsd,
    NetBsd,
}

/// 键的最大长度（存储的键先按格式检查，见 [`valid_key`]）。
const MAX_KEY: usize = 32;

impl RemoteOs {
    pub const ALL: [RemoteOs; 20] = [
        RemoteOs::Ubuntu,
        RemoteOs::Debian,
        RemoteOs::Fedora,
        RemoteOs::Rhel,
        RemoteOs::CentOs,
        RemoteOs::Rocky,
        RemoteOs::AlmaLinux,
        RemoteOs::Arch,
        RemoteOs::Alpine,
        RemoteOs::Suse,
        RemoteOs::Gentoo,
        RemoteOs::Kali,
        RemoteOs::Manjaro,
        RemoteOs::Raspbian,
        RemoteOs::Linux,
        RemoteOs::MacOs,
        RemoteOs::Windows,
        RemoteOs::FreeBsd,
        RemoteOs::OpenBsd,
        RemoteOs::NetBsd,
    ];

    /// 存储用的键，发布后不能改。
    pub fn key(self) -> &'static str {
        match self {
            RemoteOs::Ubuntu => "ubuntu",
            RemoteOs::Debian => "debian",
            RemoteOs::Fedora => "fedora",
            RemoteOs::Rhel => "rhel",
            RemoteOs::CentOs => "centos",
            RemoteOs::Rocky => "rocky",
            RemoteOs::AlmaLinux => "almalinux",
            RemoteOs::Arch => "arch",
            RemoteOs::Alpine => "alpine",
            RemoteOs::Suse => "suse",
            RemoteOs::Gentoo => "gentoo",
            RemoteOs::Kali => "kali",
            RemoteOs::Manjaro => "manjaro",
            RemoteOs::Raspbian => "raspbian",
            RemoteOs::Linux => "linux",
            RemoteOs::MacOs => "macos",
            RemoteOs::Windows => "windows",
            RemoteOs::FreeBsd => "freebsd",
            RemoteOs::OpenBsd => "openbsd",
            RemoteOs::NetBsd => "netbsd",
        }
    }

    /// 键对应的系统；不认识的键（更新的版本加的）为 `None`，当作没有识别。
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|os| os.key() == key)
    }

    /// 显示名称（品牌名，各语言相同）。
    pub fn name(self) -> &'static str {
        match self {
            RemoteOs::Ubuntu => "Ubuntu",
            RemoteOs::Debian => "Debian",
            RemoteOs::Fedora => "Fedora",
            RemoteOs::Rhel => "Red Hat Enterprise Linux",
            RemoteOs::CentOs => "CentOS",
            RemoteOs::Rocky => "Rocky Linux",
            RemoteOs::AlmaLinux => "AlmaLinux",
            RemoteOs::Arch => "Arch Linux",
            RemoteOs::Alpine => "Alpine Linux",
            RemoteOs::Suse => "SUSE",
            RemoteOs::Gentoo => "Gentoo",
            RemoteOs::Kali => "Kali Linux",
            RemoteOs::Manjaro => "Manjaro",
            RemoteOs::Raspbian => "Raspberry Pi OS",
            RemoteOs::Linux => "Linux",
            RemoteOs::MacOs => "macOS",
            RemoteOs::Windows => "Windows",
            RemoteOs::FreeBsd => "FreeBSD",
            RemoteOs::OpenBsd => "OpenBSD",
            RemoteOs::NetBsd => "NetBSD",
        }
    }

    /// Linux 发行版（含认不出发行版的 Linux）。
    pub fn is_linux(self) -> bool {
        !matches!(
            self,
            RemoteOs::MacOs
                | RemoteOs::Windows
                | RemoteOs::FreeBsd
                | RemoteOs::OpenBsd
                | RemoteOs::NetBsd
        )
    }

    /// WSL 发行版按名称（`wsl -l` 中的名字，如 `Ubuntu-22.04`、`kali-linux`、`openSUSE-Tumbleweed`）对应的系统；
    /// 认不出的是 [`RemoteOs::Linux`]（WSL 中都是 Linux）。
    pub fn from_wsl_distro(name: &str) -> Self {
        let name = name.to_ascii_lowercase();
        const PREFIXES: &[(&str, RemoteOs)] = &[
            ("ubuntu", RemoteOs::Ubuntu),
            ("debian", RemoteOs::Debian),
            ("kali", RemoteOs::Kali),
            ("opensuse", RemoteOs::Suse),
            ("suse", RemoteOs::Suse),
            ("sles", RemoteOs::Suse),
            ("fedora", RemoteOs::Fedora),
            ("rhel", RemoteOs::Rhel),
            ("redhat", RemoteOs::Rhel),
            ("centos", RemoteOs::CentOs),
            ("rocky", RemoteOs::Rocky),
            ("almalinux", RemoteOs::AlmaLinux),
            ("arch", RemoteOs::Arch),
            ("alpine", RemoteOs::Alpine),
            ("gentoo", RemoteOs::Gentoo),
            ("manjaro", RemoteOs::Manjaro),
        ];
        PREFIXES
            .iter()
            .find(|(prefix, _)| name.starts_with(prefix))
            .map_or(RemoteOs::Linux, |(_, os)| *os)
    }
}

/// 存储的键是否符合格式：1–32 个小写字母、数字、`-`、`_`，以字母或数字开头。不要求认识（见 [`RemoteOs::from_key`]）。
pub(crate) fn valid_key(key: &str) -> bool {
    key.len() <= MAX_KEY
        && key
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_unique_valid_and_round_trip() {
        let mut keys: Vec<&str> = RemoteOs::ALL.iter().map(|os| os.key()).collect();
        for os in RemoteOs::ALL {
            assert!(valid_key(os.key()), "{}", os.key());
            assert_eq!(RemoteOs::from_key(os.key()), Some(os));
        }
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), RemoteOs::ALL.len());
        assert_eq!(RemoteOs::from_key("nixos"), None);
        assert_eq!(RemoteOs::from_key("Ubuntu"), None);
        let not_linux: Vec<RemoteOs> = RemoteOs::ALL
            .into_iter()
            .filter(|os| !os.is_linux())
            .collect();
        assert_eq!(
            not_linux,
            [
                RemoteOs::MacOs,
                RemoteOs::Windows,
                RemoteOs::FreeBsd,
                RemoteOs::OpenBsd,
                RemoteOs::NetBsd
            ]
        );
    }

    // 存储的键：发布后不能改，这里写死一遍
    #[test]
    fn stored_keys_never_change() {
        let keys: Vec<&str> = RemoteOs::ALL.iter().map(|os| os.key()).collect();
        assert_eq!(
            keys,
            [
                "ubuntu",
                "debian",
                "fedora",
                "rhel",
                "centos",
                "rocky",
                "almalinux",
                "arch",
                "alpine",
                "suse",
                "gentoo",
                "kali",
                "manjaro",
                "raspbian",
                "linux",
                "macos",
                "windows",
                "freebsd",
                "openbsd",
                "netbsd"
            ]
        );
    }

    #[test]
    fn key_format() {
        for ok in ["nixos", "a", "clear-linux_os", "0x"] {
            assert!(valid_key(ok), "{ok}");
        }
        let long = "a".repeat(33);
        for bad in ["", "-x", "_x", "Ubuntu", "a b", "a/b", "ü", long.as_str()] {
            assert!(!valid_key(bad), "{bad}");
        }
    }

    #[test]
    fn wsl_distros_by_name() {
        for (name, os) in [
            ("Ubuntu", RemoteOs::Ubuntu),
            ("Ubuntu-22.04", RemoteOs::Ubuntu),
            ("Debian", RemoteOs::Debian),
            ("kali-linux", RemoteOs::Kali),
            ("openSUSE-Tumbleweed", RemoteOs::Suse),
            ("openSUSE-Leap-15.6", RemoteOs::Suse),
            ("SUSE-Linux-Enterprise-15-SP5", RemoteOs::Suse),
            ("FedoraLinux-42", RemoteOs::Fedora),
            ("RedHatEnterpriseLinux-10.0", RemoteOs::Rhel),
            ("AlmaLinux-9", RemoteOs::AlmaLinux),
            ("RockyLinux-9", RemoteOs::Rocky),
            ("archlinux", RemoteOs::Arch),
            ("Alpine", RemoteOs::Alpine),
            ("OracleLinux_9_1", RemoteOs::Linux),
            ("docker-desktop", RemoteOs::Linux),
            ("my-dev", RemoteOs::Linux),
        ] {
            assert_eq!(RemoteOs::from_wsl_distro(name), os, "{name}");
        }
    }
}
