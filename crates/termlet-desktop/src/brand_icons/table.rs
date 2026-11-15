//! 由 scripts/brand-icons.py 从 Simple Icons 16.36.0 生成，请勿手工修改。
//!
//! 每个标志的资源路径是 `brand-icons/<slug>.svg`（`assets/brand-icons/` 中的文件，单色的一条路径），
//! 品牌色取自 Simple Icons 的数据。许可与商标说明见 `assets/brand-icons/LICENSE`。

/// 打包的系统与品牌标志。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Brand {
    /// AlmaLinux（系统）
    AlmaLinux,
    /// Alpine Linux（系统）
    AlpineLinux,
    /// Apple（系统：macOS）
    Apple,
    /// Arch Linux（系统）
    ArchLinux,
    /// CentOS（系统）
    CentOs,
    /// Claude（审批框：Claude Code）
    Claude,
    /// Debian（系统）
    Debian,
    /// Docker（主机工具：Docker 页）
    Docker,
    /// FreeBSD（系统）
    FreeBsd,
    /// Gentoo（系统）
    Gentoo,
    /// Git for Windows（shell：Git Bash）
    GitForWindows,
    /// GNU Bash（shell：bash）
    GnuBash,
    /// Google Gemini（审批框：Gemini CLI）
    GoogleGemini,
    /// Kali Linux（系统）
    KaliLinux,
    /// Linux（系统：认不出发行版的 Linux）
    Linux,
    /// Manjaro（系统）
    Manjaro,
    /// NetBSD（系统）
    NetBsd,
    /// Nushell（shell：nu）
    Nushell,
    /// OpenBSD（系统）
    OpenBsd,
    /// Raspberry Pi（系统：Raspberry Pi OS）
    RaspberryPi,
    /// Red Hat（系统：RHEL）
    RedHat,
    /// Rocky Linux（系统）
    RockyLinux,
    /// SUSE（系统：openSUSE 与 SLES）
    Suse,
    /// Ubuntu（系统）
    Ubuntu,
    /// Zsh（shell：zsh）
    Zsh,
}

/// 一个标志的数据。
pub(super) struct BrandData {
    /// Simple Icons 中的标识（资源文件名）
    pub slug: &'static str,
    /// 资源路径
    pub path: &'static str,
    /// 品牌色 `0xRRGGBB`
    pub hex: u32,
    pub svg: &'static str,
}

impl Brand {
    pub const ALL: [Brand; 25] = [
        Brand::AlmaLinux,
        Brand::AlpineLinux,
        Brand::Apple,
        Brand::ArchLinux,
        Brand::CentOs,
        Brand::Claude,
        Brand::Debian,
        Brand::Docker,
        Brand::FreeBsd,
        Brand::Gentoo,
        Brand::GitForWindows,
        Brand::GnuBash,
        Brand::GoogleGemini,
        Brand::KaliLinux,
        Brand::Linux,
        Brand::Manjaro,
        Brand::NetBsd,
        Brand::Nushell,
        Brand::OpenBsd,
        Brand::RaspberryPi,
        Brand::RedHat,
        Brand::RockyLinux,
        Brand::Suse,
        Brand::Ubuntu,
        Brand::Zsh,
    ];
}

/// 各标志的数据，与 [`Brand`] 的变体一一对应（按变体的顺序）。
pub(super) static BRANDS: [BrandData; 25] = [
    BrandData {
        slug: "almalinux",
        path: "brand-icons/almalinux.svg",
        hex: 0x000000,
        svg: include_str!("../../assets/brand-icons/almalinux.svg"),
    },
    BrandData {
        slug: "alpinelinux",
        path: "brand-icons/alpinelinux.svg",
        hex: 0x0D597F,
        svg: include_str!("../../assets/brand-icons/alpinelinux.svg"),
    },
    BrandData {
        slug: "apple",
        path: "brand-icons/apple.svg",
        hex: 0x000000,
        svg: include_str!("../../assets/brand-icons/apple.svg"),
    },
    BrandData {
        slug: "archlinux",
        path: "brand-icons/archlinux.svg",
        hex: 0x1793D1,
        svg: include_str!("../../assets/brand-icons/archlinux.svg"),
    },
    BrandData {
        slug: "centos",
        path: "brand-icons/centos.svg",
        hex: 0x262577,
        svg: include_str!("../../assets/brand-icons/centos.svg"),
    },
    BrandData {
        slug: "claude",
        path: "brand-icons/claude.svg",
        hex: 0xD97757,
        svg: include_str!("../../assets/brand-icons/claude.svg"),
    },
    BrandData {
        slug: "debian",
        path: "brand-icons/debian.svg",
        hex: 0xA81D33,
        svg: include_str!("../../assets/brand-icons/debian.svg"),
    },
    BrandData {
        slug: "docker",
        path: "brand-icons/docker.svg",
        hex: 0x2496ED,
        svg: include_str!("../../assets/brand-icons/docker.svg"),
    },
    BrandData {
        slug: "freebsd",
        path: "brand-icons/freebsd.svg",
        hex: 0xAB2B28,
        svg: include_str!("../../assets/brand-icons/freebsd.svg"),
    },
    BrandData {
        slug: "gentoo",
        path: "brand-icons/gentoo.svg",
        hex: 0x54487A,
        svg: include_str!("../../assets/brand-icons/gentoo.svg"),
    },
    BrandData {
        slug: "gitforwindows",
        path: "brand-icons/gitforwindows.svg",
        hex: 0x80B3FF,
        svg: include_str!("../../assets/brand-icons/gitforwindows.svg"),
    },
    BrandData {
        slug: "gnubash",
        path: "brand-icons/gnubash.svg",
        hex: 0x4EAA25,
        svg: include_str!("../../assets/brand-icons/gnubash.svg"),
    },
    BrandData {
        slug: "googlegemini",
        path: "brand-icons/googlegemini.svg",
        hex: 0x8E75B2,
        svg: include_str!("../../assets/brand-icons/googlegemini.svg"),
    },
    BrandData {
        slug: "kalilinux",
        path: "brand-icons/kalilinux.svg",
        hex: 0x557C94,
        svg: include_str!("../../assets/brand-icons/kalilinux.svg"),
    },
    BrandData {
        slug: "linux",
        path: "brand-icons/linux.svg",
        hex: 0xFCC624,
        svg: include_str!("../../assets/brand-icons/linux.svg"),
    },
    BrandData {
        slug: "manjaro",
        path: "brand-icons/manjaro.svg",
        hex: 0x35BFA4,
        svg: include_str!("../../assets/brand-icons/manjaro.svg"),
    },
    BrandData {
        slug: "netbsd",
        path: "brand-icons/netbsd.svg",
        hex: 0xFF6600,
        svg: include_str!("../../assets/brand-icons/netbsd.svg"),
    },
    BrandData {
        slug: "nushell",
        path: "brand-icons/nushell.svg",
        hex: 0x4E9A06,
        svg: include_str!("../../assets/brand-icons/nushell.svg"),
    },
    BrandData {
        slug: "openbsd",
        path: "brand-icons/openbsd.svg",
        hex: 0xF2CA30,
        svg: include_str!("../../assets/brand-icons/openbsd.svg"),
    },
    BrandData {
        slug: "raspberrypi",
        path: "brand-icons/raspberrypi.svg",
        hex: 0xA22846,
        svg: include_str!("../../assets/brand-icons/raspberrypi.svg"),
    },
    BrandData {
        slug: "redhat",
        path: "brand-icons/redhat.svg",
        hex: 0xEE0000,
        svg: include_str!("../../assets/brand-icons/redhat.svg"),
    },
    BrandData {
        slug: "rockylinux",
        path: "brand-icons/rockylinux.svg",
        hex: 0x10B981,
        svg: include_str!("../../assets/brand-icons/rockylinux.svg"),
    },
    BrandData {
        slug: "suse",
        path: "brand-icons/suse.svg",
        hex: 0x0C322C,
        svg: include_str!("../../assets/brand-icons/suse.svg"),
    },
    BrandData {
        slug: "ubuntu",
        path: "brand-icons/ubuntu.svg",
        hex: 0xE95420,
        svg: include_str!("../../assets/brand-icons/ubuntu.svg"),
    },
    BrandData {
        slug: "zsh",
        path: "brand-icons/zsh.svg",
        hex: 0xF15A24,
        svg: include_str!("../../assets/brand-icons/zsh.svg"),
    },
];
