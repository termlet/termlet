//! 识别远程系统：SSH 连接成功后，在这条连接上另开 exec 通道执行只读的探测命令（与资源监控、主机工具相同的机制，
//! 见 [`crate::hosttools::Target`]），按输出判断是哪种系统（[`RemoteOs`]），界面据此显示系统的标志。
//!
//! - 先经 `sh -c` 执行 `uname -s` 并读 os-release（[`UNIX_SCRIPT`]）；sh 没有执行（Windows 的 OpenSSH，登录 shell
//!   是 cmd 或 PowerShell）时再执行 `cmd.exe /d /c ver`（[`WINDOWS_COMMAND`]）。sh 执行了却认不出（Solaris、AIX 等）
//!   时不再试 Windows。
//! - 命令行中没有重定向与管道（`>`、`|`、`&` 等）：登录 shell 是 cmd 时它不认单引号，`2>/dev/null` 会被它当成重定向，
//!   可能在服务器上建出文件。找不到的文件报错在标准错误里，不影响解析。
//! - 不经过用户的终端（用户看不到），不往服务器写任何文件；失败、超时（每一步 [`STEP_TIMEOUT`]）、通道被拒（堡垒机）
//!   一律当作没有识别出来，不影响会话。只读，不记审计日志（与资源监控、主机工具的读取相同，主机工具只记写操作）。
//! - 每条连接只探测一次：复制会话共用连接时不重复执行，后来的直接拿到第一次的结果。
//!
//! 发行版按 os-release 的 `ID` 判断；`ID` 不认识时按 `ID_LIKE`（发行版自己声明的、从近到远的上游）兜底，衍生版显示
//! 它最近的、认识的上游家族的标志：Linux Mint、Pop!_OS → Ubuntu，Deepin、UOS → Debian，EndeavourOS → Arch，
//! Amazon Linux、Anolis 等 RHEL 兼容版 → Red Hat（`ID_LIKE` 中的 centos 也算 RHEL：CentOS Linux 已停止，
//! 声明它的都是 RHEL 的重建版），Oracle Linux 声明的是 fedora → Fedora。都认不出的 Linux 是 [`RemoteOs::Linux`]。
//!
//! # 用法（界面）
//!
//! SSH 终端连接成功（含重连）后，设置中“连接后识别远程系统”开着、主机不是“只用一个通道”时调用 [`detect`]，传入
//! 终端的连接；结果不是 `None` 时用 [`crate::store::Store::set_host_os`] 记下（与原来相同时不写）。Telnet、串口、
//! 本地终端不调用。返回的 future 是 `Send`，可以在 GPUI 的任务里直接 `await`。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::sync::OnceCell;

use crate::hosttools::{ExecError, Limits, Target, on_runtime, run, sh_script};
use crate::model::RemoteOs;
use crate::ssh::Connection;

/// 每一步的最长时间（含排队等通道名额）。
pub const STEP_TIMEOUT: Duration = Duration::from_secs(5);
/// 输出的上限（os-release 只有几百字节）。
const LIMIT: usize = 16 << 10;

/// 第一步：经 `sh -c` 执行，输出 `@@uname`、`uname -s` 的结果、`@@release` 与 os-release 的内容
/// （`/etc/os-release` 不在时读 `/usr/lib/os-release`；两个都在时同一项取先出现的）。不含单引号、`!` 与重定向。
pub const UNIX_SCRIPT: &str =
    "echo @@uname; uname -s; echo @@release; cat /etc/os-release /usr/lib/os-release";

/// 第二步（sh 没有执行时）：Windows 的版本，输出 `Microsoft Windows [Version 10.0.…]`（中文系统是“版本”）。
pub const WINDOWS_COMMAND: &str = "cmd.exe /d /c ver";

/// 识别这条连接上的系统；认不出、出错、超时时为 `None`（见模块说明）。同一条连接只执行一次。
pub async fn detect(connection: Arc<Connection>) -> Option<RemoteOs> {
    let cell = cell_for(&connection);
    let result = on_runtime(async move {
        let os = *cell
            .get_or_init(|| async {
                let os = probe(&Target::ssh(connection.clone(), false)).await;
                tracing::debug!(host = connection.label(), os = ?os.map(RemoteOs::key), "识别远程系统");
                os
            })
            .await;
        Ok::<_, ExecError>(os)
    })
    .await;
    result.ok().flatten()
}

/// 各连接的识别结果：只记弱引用，连接不再使用后在下一次登记时清掉。
static PROBES: Mutex<Vec<(Weak<Connection>, Arc<OnceCell<Option<RemoteOs>>>)>> =
    Mutex::new(Vec::new());

fn cell_for(connection: &Arc<Connection>) -> Arc<OnceCell<Option<RemoteOs>>> {
    let mut book = PROBES.lock().unwrap_or_else(|e| e.into_inner());
    book.retain(|(weak, _)| weak.strong_count() > 0);
    let weak = Arc::downgrade(connection);
    if let Some((_, cell)) = book.iter().find(|(w, _)| w.ptr_eq(&weak)) {
        return cell.clone();
    }
    let cell = Arc::new(OnceCell::new());
    book.push((weak, cell.clone()));
    cell
}

/// 执行探测（见模块说明）。
async fn probe(target: &Target) -> Option<RemoteOs> {
    let limits = Limits {
        timeout: STEP_TIMEOUT,
        output: LIMIT,
        long: false,
    };
    let unix = match run(target, &sh_script::<&str>(UNIX_SCRIPT, &[]), limits).await {
        Ok(result) => parse_unix(&String::from_utf8_lossy(&result.stdout)),
        Err(error) => {
            tracing::debug!("识别远程系统没有执行：{error}");
            return None;
        }
    };
    match unix {
        UnixProbe::Found(os) => Some(os),
        UnixProbe::Unknown => None,
        UnixProbe::NoShell => match run(target, WINDOWS_COMMAND, limits).await {
            Ok(result) => parse_ver(&result.stdout),
            Err(error) => {
                tracing::debug!("识别 Windows 没有执行：{error}");
                None
            }
        },
    }
}

/// 第一步的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UnixProbe {
    Found(RemoteOs),
    /// sh 执行了，但认不出（Solaris、AIX 等）：不再试 Windows
    Unknown,
    /// sh 没有执行（没有 sh：可能是 Windows，也可能是网络设备）：再试 Windows
    NoShell,
}

/// 解析第一步的输出（见 [`UNIX_SCRIPT`]）。分段标记要单独成行：登录 shell 的启动脚本可能先输出别的内容，
/// 回显命令的设备输出的命令行中也有标记，但不会单独成行。
fn parse_unix(out: &str) -> UnixProbe {
    let mut lines = out.lines().map(str::trim);
    if !lines.any(|line| line == "@@uname") {
        return UnixProbe::NoShell;
    }
    let mut uname = "";
    for line in lines.by_ref() {
        if line == "@@release" {
            break;
        }
        if uname.is_empty() {
            uname = line;
        }
    }
    let release = parse_os_release(lines);
    let os = match uname {
        "Linux" => Some(distro(&release).unwrap_or(RemoteOs::Linux)),
        "Darwin" => Some(RemoteOs::MacOs),
        "FreeBSD" => Some(RemoteOs::FreeBsd),
        "OpenBSD" => Some(RemoteOs::OpenBsd),
        "NetBSD" => Some(RemoteOs::NetBsd),
        // Windows 上 Cygwin、MSYS2、Git Bash 的 sh（登录 shell 是它们，或者 cmd 能找到 Git 的 sh）
        u if ["CYGWIN_NT", "MSYS_NT", "MINGW"]
            .iter()
            .any(|prefix| u.starts_with(prefix)) =>
        {
            Some(RemoteOs::Windows)
        }
        // 没有 uname：只能看 os-release
        "" => distro(&release),
        _ => None,
    };
    os.map_or(UnixProbe::Unknown, UnixProbe::Found)
}

/// 解析 os-release（`KEY=值`，值可以加双引号或单引号，双引号内 `\"`、`\\`、`\$`、`` \` `` 是转义；`#` 开头的是注释）。
/// 同一个键取先出现的；不合规的行跳过。
fn parse_os_release<'a>(lines: impl Iterator<Item = &'a str>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in lines {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.is_empty()
            || !key
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        {
            continue;
        }
        out.entry(key.to_string())
            .or_insert_with(|| unquote(value.trim()));
    }
    out
}

fn unquote(value: &str) -> String {
    if let Some(inner) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
        return inner.to_string();
    }
    let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else {
        return value.to_string();
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\'
            && let Some(next) = chars.clone().next()
            && matches!(next, '"' | '\\' | '$' | '`')
        {
            out.push(next);
            chars.next();
        } else {
            out.push(c);
        }
    }
    out
}

/// 按 `ID`、再按 `ID_LIKE` 判断发行版（见模块说明）。
fn distro(release: &HashMap<String, String>) -> Option<RemoteOs> {
    if let Some(os) = release
        .get("ID")
        .and_then(|id| by_id(&id.to_ascii_lowercase()))
    {
        return Some(os);
    }
    release
        .get("ID_LIKE")?
        .split_ascii_whitespace()
        .find_map(|like| by_family(&like.to_ascii_lowercase()))
}

/// os-release 的 `ID` 对应的系统。
fn by_id(id: &str) -> Option<RemoteOs> {
    Some(match id {
        "ubuntu" => RemoteOs::Ubuntu,
        "debian" => RemoteOs::Debian,
        "fedora" => RemoteOs::Fedora,
        "rhel" => RemoteOs::Rhel,
        "centos" => RemoteOs::CentOs,
        "rocky" => RemoteOs::Rocky,
        "almalinux" => RemoteOs::AlmaLinux,
        "arch" | "archarm" => RemoteOs::Arch,
        "alpine" => RemoteOs::Alpine,
        "sles" | "sled" | "sle-micro" | "suse" => RemoteOs::Suse,
        id if id.starts_with("opensuse") => RemoteOs::Suse,
        "gentoo" => RemoteOs::Gentoo,
        "kali" => RemoteOs::Kali,
        "manjaro" | "manjaro-arm" => RemoteOs::Manjaro,
        "raspbian" => RemoteOs::Raspbian,
        _ => return None,
    })
}

/// `ID_LIKE` 中的一项对应的家族（只认上游家族，见模块说明）。
fn by_family(like: &str) -> Option<RemoteOs> {
    Some(match like {
        "ubuntu" => RemoteOs::Ubuntu,
        "debian" => RemoteOs::Debian,
        "rhel" | "centos" => RemoteOs::Rhel,
        "fedora" => RemoteOs::Fedora,
        "arch" => RemoteOs::Arch,
        "alpine" => RemoteOs::Alpine,
        "suse" | "opensuse" => RemoteOs::Suse,
        "gentoo" => RemoteOs::Gentoo,
        _ => return None,
    })
}

/// 解析第二步的输出：有 `Microsoft Windows` 就是 Windows（版本号前的“Version”随系统语言变化，输出的编码随代码页
/// 变化，按字节找这个不变的部分）。
fn parse_ver(out: &[u8]) -> Option<RemoteOs> {
    const MARK: &[u8] = b"Microsoft Windows";
    out.windows(MARK.len())
        .any(|w| w == MARK)
        .then_some(RemoteOs::Windows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unix(uname: &str, release: &str) -> UnixProbe {
        parse_unix(&format!("@@uname\n{uname}\n@@release\n{release}"))
    }

    fn found(uname: &str, release: &str) -> Option<RemoteOs> {
        match unix(uname, release) {
            UnixProbe::Found(os) => Some(os),
            _ => None,
        }
    }

    // 各发行版 os-release 的原文（节选）
    #[test]
    fn linux_distributions() {
        for (release, os) in [
            (
                "PRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\nNAME=\"Ubuntu\"\nVERSION_ID=\"24.04\"\nID=ubuntu\nID_LIKE=debian\n",
                RemoteOs::Ubuntu,
            ),
            (
                "PRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"\nID=debian\n",
                RemoteOs::Debian,
            ),
            ("NAME=\"Fedora Linux\"\nID=fedora\n", RemoteOs::Fedora),
            (
                "NAME=\"Red Hat Enterprise Linux\"\nID=\"rhel\"\nID_LIKE=\"fedora\"\n",
                RemoteOs::Rhel,
            ),
            (
                "NAME=\"CentOS Stream\"\nID=\"centos\"\nID_LIKE=\"rhel fedora\"\n",
                RemoteOs::CentOs,
            ),
            (
                "NAME=\"Rocky Linux\"\nID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\n",
                RemoteOs::Rocky,
            ),
            (
                "NAME=\"AlmaLinux\"\nID=\"almalinux\"\nID_LIKE=\"rhel centos fedora\"\n",
                RemoteOs::AlmaLinux,
            ),
            ("NAME=\"Arch Linux\"\nID=arch\n", RemoteOs::Arch),
            (
                "NAME=\"Arch Linux ARM\"\nID=archarm\nID_LIKE=arch\n",
                RemoteOs::Arch,
            ),
            (
                "NAME=\"Alpine Linux\"\nID=alpine\nVERSION_ID=3.20.3\n",
                RemoteOs::Alpine,
            ),
            (
                "NAME=\"openSUSE Leap\"\nID=\"opensuse-leap\"\nID_LIKE=\"suse opensuse\"\n",
                RemoteOs::Suse,
            ),
            (
                "NAME=\"openSUSE Tumbleweed\"\nID=\"opensuse-tumbleweed\"\n",
                RemoteOs::Suse,
            ),
            (
                "NAME=\"SLES\"\nID=\"sles\"\nID_LIKE=\"suse\"\n",
                RemoteOs::Suse,
            ),
            ("NAME=Gentoo\nID=gentoo\n", RemoteOs::Gentoo),
            (
                "NAME=\"Kali GNU/Linux\"\nID=kali\nID_LIKE=debian\n",
                RemoteOs::Kali,
            ),
            (
                "NAME=\"Manjaro Linux\"\nID=manjaro\nID_LIKE=arch\n",
                RemoteOs::Manjaro,
            ),
            (
                "NAME=\"Raspbian GNU/Linux\"\nID=raspbian\nID_LIKE=debian\n",
                RemoteOs::Raspbian,
            ),
        ] {
            assert_eq!(found("Linux", release), Some(os), "{release}");
        }
    }

    // ID 不认识时按 ID_LIKE 从前往后找认识的上游家族
    #[test]
    fn derivatives_fall_back_to_id_like() {
        for (release, os) in [
            (
                "ID=linuxmint\nID_LIKE=\"ubuntu debian\"\n",
                RemoteOs::Ubuntu,
            ),
            ("ID=pop\nID_LIKE=\"ubuntu debian\"\n", RemoteOs::Ubuntu),
            ("ID=deepin\nID_LIKE=debian\n", RemoteOs::Debian),
            ("ID=uos\nID_LIKE=debian\n", RemoteOs::Debian),
            ("ID=endeavouros\nID_LIKE=arch\n", RemoteOs::Arch),
            (
                "ID=\"amzn\"\nID_LIKE=\"centos rhel fedora\"\n",
                RemoteOs::Rhel,
            ),
            (
                "ID=\"anolis\"\nID_LIKE=\"rhel fedora centos\"\n",
                RemoteOs::Rhel,
            ),
            ("ID=\"ol\"\nID_LIKE=\"fedora\"\n", RemoteOs::Fedora),
            ("ID=postmarketos\nID_LIKE=alpine\n", RemoteOs::Alpine),
            // 认识的 ID 优先于 ID_LIKE
            ("ID=kali\nID_LIKE=debian\n", RemoteOs::Kali),
            // ID_LIKE 中不认识的跳过
            ("ID=x\nID_LIKE=\"lede ubuntu\"\n", RemoteOs::Ubuntu),
        ] {
            assert_eq!(found("Linux", release), Some(os), "{release}");
        }
    }

    // 认不出发行版的 Linux：没有 os-release、ID 与 ID_LIKE 都不认识
    #[test]
    fn unknown_linux_is_linux() {
        for release in [
            "",
            "NAME=NixOS\nID=nixos\n",
            "ID=openEuler\n",
            "ID=\"openwrt\"\nID_LIKE=\"lede openwrt\"\n",
            "VERSION_ID=1\n",
        ] {
            assert_eq!(found("Linux", release), Some(RemoteOs::Linux), "{release}");
        }
    }

    #[test]
    fn other_unix_systems() {
        assert_eq!(found("Darwin", ""), Some(RemoteOs::MacOs));
        // FreeBSD 也有 os-release，以 uname 为准
        assert_eq!(
            found("FreeBSD", "NAME=FreeBSD\nID=freebsd\n"),
            Some(RemoteOs::FreeBsd)
        );
        assert_eq!(found("OpenBSD", ""), Some(RemoteOs::OpenBsd));
        assert_eq!(found("NetBSD", ""), Some(RemoteOs::NetBsd));
        for uname in [
            "CYGWIN_NT-10.0-19045",
            "MSYS_NT-10.0-26100",
            "MINGW64_NT-10.0-26100",
        ] {
            assert_eq!(found(uname, ""), Some(RemoteOs::Windows), "{uname}");
        }
        // sh 执行了却认不出：不再试 Windows
        assert_eq!(unix("SunOS", ""), UnixProbe::Unknown);
        assert_eq!(unix("AIX", ""), UnixProbe::Unknown);
        // 没有 uname 时看 os-release
        assert_eq!(found("", "ID=debian\n"), Some(RemoteOs::Debian));
        assert_eq!(unix("", ""), UnixProbe::Unknown);
    }

    // 引号、转义、注释、空白、CRLF、缺字段、重复的键
    #[test]
    fn os_release_quoting() {
        let release = parse_os_release(
            [
                "# 注释",
                "",
                "ID='rhel'",
                "NAME=\"Say \\\"hi\\\" \\$HOME \\\\ \\x\"",
                "  ID_LIKE = ignored",
                "BAD LINE",
                "lower=x",
                "VERSION_ID=\"9.4\"\r",
                "ID=ubuntu",
            ]
            .into_iter(),
        );
        assert_eq!(release["ID"], "rhel");
        assert_eq!(release["NAME"], r#"Say "hi" $HOME \ \x"#);
        assert_eq!(release["VERSION_ID"], "9.4");
        assert!(!release.contains_key("ID_LIKE"));
        assert!(!release.contains_key("lower"));
        assert_eq!(found("Linux", "ID=\"UBUNTU\"\r\n"), Some(RemoteOs::Ubuntu));
        // 只有开头一个引号：原样保留（认不出）
        assert_eq!(found("Linux", "ID=\"debian\n"), Some(RemoteOs::Linux));
    }

    // 标记前的输出（登录 shell 的启动脚本）、CRLF、回显的命令行
    #[test]
    fn noise_around_markers() {
        let out = "Welcome!\r\n@@uname\r\nLinux\r\n@@release\r\nID=alpine\r\n";
        assert_eq!(parse_unix(out), UnixProbe::Found(RemoteOs::Alpine));
        // 回显命令的网络设备：标记不单独成行
        let echoed = "sh -c 'echo @@uname; uname -s; echo @@release; cat /etc/os-release'\n% Invalid input\n";
        assert_eq!(parse_unix(echoed), UnixProbe::NoShell);
        // 没有 @@release（被截断）：只看 uname
        assert_eq!(
            parse_unix("@@uname\nDarwin\n"),
            UnixProbe::Found(RemoteOs::MacOs)
        );
        // 乱码
        assert_eq!(
            parse_unix("\u{fffd}\u{fffd}\0\u{1b}[31m"),
            UnixProbe::NoShell
        );
        assert_eq!(
            parse_unix("@@uname\n\u{fffd}\u{fffd}\n@@release\n\u{fffd}=\u{fffd}"),
            UnixProbe::Unknown
        );
        assert_eq!(parse_unix(""), UnixProbe::NoShell);
    }

    // Windows 的 ver：英文、中文（GBK 编码的“版本”）、PowerShell 下的输出；cmd 不认识 sh 时第一步的输出
    #[test]
    fn windows_ver() {
        assert_eq!(
            parse_ver(b"\r\nMicrosoft Windows [Version 10.0.26100.4652]\r\n"),
            Some(RemoteOs::Windows)
        );
        let mut chinese = b"\r\nMicrosoft Windows [".to_vec();
        chinese.extend_from_slice(&[0xb0, 0xe6, 0xb1, 0xbe]);
        chinese.extend_from_slice(b" 10.0.19045.3803]\r\n");
        assert_eq!(parse_ver(&chinese), Some(RemoteOs::Windows));
        assert_eq!(parse_ver(b""), None);
        assert_eq!(parse_ver(b"% Unknown command"), None);
        assert_eq!(parse_ver(&[0xff, 0xfe, 0x00]), None);
        let cmd = "'sh' is not recognized as an internal or external command,\r\noperable program or batch file.\r\n";
        assert_eq!(parse_unix(cmd), UnixProbe::NoShell);
    }

    // 命令行中没有 cmd 会当作重定向、管道、变量的字符（见模块说明），也没有 tcsh 不认的写法
    #[test]
    fn command_lines_are_safe_for_cmd_and_tcsh() {
        let command = sh_script::<&str>(UNIX_SCRIPT, &[]);
        for c in ['>', '<', '|', '&', '^', '%', '!', '\n', '"'] {
            assert!(!command.contains(c), "{c:?} in {command}");
        }
        assert_eq!(
            command,
            "sh -c 'echo @@uname; uname -s; echo @@release; cat /etc/os-release /usr/lib/os-release' sh"
        );
        assert!(!WINDOWS_COMMAND.contains(['>', '<', '|', '&', '\'']));
    }

    // 在本机的 sh 中执行第一步（Windows 上是 Git 的 sh，uname 是 MINGW64_NT 等，认作 Windows）
    #[test]
    fn unix_script_runs_in_local_sh() {
        let command = sh_script::<&str>(UNIX_SCRIPT, &[]);
        let Some(out) = run_sh_tolerant(&command) else {
            return;
        };
        let parsed = parse_unix(&out);
        if cfg!(windows) {
            assert_eq!(parsed, UnixProbe::Found(RemoteOs::Windows), "{out}");
        } else if cfg!(target_os = "macos") {
            assert_eq!(parsed, UnixProbe::Found(RemoteOs::MacOs), "{out}");
        } else if cfg!(target_os = "linux") {
            assert!(matches!(parsed, UnixProbe::Found(_)), "{out}");
        }
    }

    /// 经本机的 sh 执行（文件不在时 cat 以非 0 结束，不算失败）；没有 sh 时为 `None`。
    fn run_sh_tolerant(command: &str) -> Option<String> {
        use std::io::Write as _;
        let names: &[&str] = if cfg!(windows) { &["sh.exe"] } else { &["sh"] };
        let path = std::env::var_os("PATH")?;
        let sh = std::env::split_paths(&path)
            .flat_map(|dir| names.iter().map(move |n| dir.join(n)))
            .find(|p| p.is_file())?;
        let mut child = std::process::Command::new(sh)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        child.stdin.take()?.write_all(command.as_bytes()).ok()?;
        let out = child.wait_with_output().ok()?;
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// 连真实 sshd（`TERMLET_TEST_SSH=127.0.0.1:2222`，testing/sshd 的测试容器是 Alpine）：认出 Alpine；
    /// 同一条连接第二次直接返回第一次的结果；只用一个通道时什么都不执行。
    #[test]
    fn detects_alpine_on_real_server() {
        let Some((host, port)) = crate::testkit::test_server() else {
            return;
        };
        crate::runtime::runtime().block_on(async {
            let connection = crate::testkit::connect_test(&host, port).await;
            assert_eq!(detect(connection.clone()).await, Some(RemoteOs::Alpine));
            assert_eq!(detect(connection.clone()).await, Some(RemoteOs::Alpine));
            let single = Target::ssh(connection.clone(), true);
            assert_eq!(probe(&single).await, None);
        });
    }
}
