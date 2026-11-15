//! 本机的 WSL 发行版（只有 Windows 有）：列出发行版、默认发行版，以及打开某个发行版的终端用的启动描述。
//!
//! `wsl.exe` 的输出到管道时是 UTF-16LE（设置了 `WSL_UTF8=1` 时是 UTF-8），解析与运行分开，
//! 解析函数可以直接用字符串测试。发行版中的文件管理不在这里。

use std::path::PathBuf;
#[cfg(windows)]
use std::time::Duration;

use super::env::{Env, eq_ignore_case};
use super::{Launch, ShellError, imp};

pub use crate::SESSION_ENV;

/// 发行版名称是否合法（`wsl.exe` 允许的名称只含这些字符），合法的拼进命令行时无需转义。
pub fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// Linux 用户名是否合法（与 useradd 的默认规则相同）；为空表示发行版的默认用户。
pub fn valid_user(user: &str) -> bool {
    let bytes = user.as_bytes();
    let Some((&first, rest)) = bytes.split_first() else {
        return true;
    };
    bytes.len() <= 32
        && (first.is_ascii_lowercase() || first == b'_')
        && rest
            .iter()
            .all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b'-'))
}

/// `wsl.exe` 的参数：`-d <发行版> [-u <用户>] rest...`；校验发行版名称与用户名。
pub fn args(distro: &str, user: &str, rest: &[&str]) -> Result<Vec<String>, ShellError> {
    if !valid_name(distro) {
        return Err(ShellError::InvalidDistro);
    }
    if !valid_user(user) {
        return Err(ShellError::InvalidUser);
    }
    let mut out = vec!["-d".to_string(), distro.to_string()];
    if !user.is_empty() {
        out.extend(["-u".to_string(), user.to_string()]);
    }
    out.extend(rest.iter().map(|s| s.to_string()));
    Ok(out)
}

/// `wsl.exe` 的位置（系统目录下）；没有时说明没有安装 WSL。
#[cfg(windows)]
pub fn exe() -> Result<PathBuf, ShellError> {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let exe = PathBuf::from(root).join(r"System32\wsl.exe");
    if super::exists(&exe) {
        Ok(exe)
    } else {
        Err(ShellError::WslNotInstalled)
    }
}

/// `wsl.exe` 的位置：只有 Windows 有。
#[cfg(not(windows))]
pub fn exe() -> Result<PathBuf, ShellError> {
    Err(ShellError::WslUnsupported)
}

/// 列出发行版的超时。
#[cfg(windows)]
const LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// 本机的 WSL 发行版（`wsl.exe --list --quiet`）；没有安装 WSL、没有发行版、不是 Windows 时为空。
/// 要运行 wsl.exe（几十到几百毫秒），在后台线程调用。
pub fn distributions() -> Vec<String> {
    run_list(&["--list", "--quiet"])
        .map(|out| parse_list(&out))
        .unwrap_or_default()
}

/// 运行 `wsl.exe` 列出发行版；没有安装任何发行版时 wsl.exe 以非零退出码说明原因，按"没有输出"处理。
#[cfg(windows)]
fn run_list(args: &[&str]) -> Option<Vec<u8>> {
    imp::run_hidden(&exe().ok()?, args, LIST_TIMEOUT)
}

#[cfg(not(windows))]
fn run_list(_args: &[&str]) -> Option<Vec<u8>> {
    None
}

/// 打开发行版的终端：以 `user`（为空时是发行版的默认用户）登录，从 Linux 中的用户主目录开始。
/// `session_key` 是通道的 Key（为空时不设置），作为 [`SESSION_ENV`] 经 `WSLENV` 传进 Linux。
/// 每次都读取用户最新的环境变量（与本地终端相同）。
pub fn launch(distro: &str, user: &str, session_key: &str) -> Result<Launch, ShellError> {
    let args = args(distro, user, &["--cd", "~"])?;
    let program = exe()?;
    let mut add: Vec<(&str, String)> = Vec::new();
    if !session_key.is_empty() && !session_key.contains('\0') {
        add.push((SESSION_ENV, session_key.to_string()));
        add.push(("WSLENV", format!("{SESSION_ENV}/u")));
    }
    let base = imp::user_env();
    let cwd = imp::home_of(&base);
    Ok(Launch {
        program,
        args,
        raw_args: None,
        cwd,
        env: merge_env(&base, add).into_vec(),
    })
}

/// 不列出的发行版：Docker Desktop 的内部发行版不是给用户登录的（与 Windows Terminal 相同）。
fn hidden(name: &str) -> bool {
    name.to_ascii_lowercase().starts_with("docker-desktop")
}

/// 把 wsl.exe 的输出转为字符串：输出到管道时是 UTF-16LE（可能带 BOM），换行固定为 `"\n\0"`；
/// 设置了 `WSL_UTF8=1` 时是 UTF-8。
pub(crate) fn decode_output(out: &[u8]) -> String {
    let utf16 = out.starts_with(&[0xff, 0xfe]) || out.windows(2).any(|w| w == [b'\n', 0]);
    if !utf16 {
        return String::from_utf8_lossy(out).into_owned();
    }
    // 奇数长度时最后一个字节不完整，丢掉
    let units: Vec<u16> = out
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| u16::from_le_bytes(pair))
        .collect();
    let text = String::from_utf16_lossy(&units);
    match text.strip_prefix('\u{feff}') {
        Some(rest) => rest.to_string(),
        None => text,
    }
}

/// 输出中的各行，去掉 NUL 与两端的空白。
fn lines(out: &[u8]) -> Vec<String> {
    decode_output(out)
        .lines()
        .map(|line| line.replace('\0', "").trim().to_string())
        .collect()
}

/// 解析 `wsl.exe --list --quiet` 的输出：每行一个发行版，去掉空行、重复、不合法的名称与内部发行版。
fn parse_list(out: &[u8]) -> Vec<String> {
    let mut list: Vec<String> = Vec::new();
    for name in lines(out) {
        if !name.is_empty() && valid_name(&name) && !hidden(&name) && !list.contains(&name) {
            list.push(name);
        }
    }
    list
}

/// 把 `add` 合并到环境变量 `base` 中：同名的（不区分大小写）被覆盖，`WSLENV` 追加到原值之后（以冒号分隔）。
fn merge_env(base: &Env, mut add: Vec<(&str, String)>) -> Env {
    let mut out = Env::new();
    for (key, value) in base.iter() {
        let found = (!key.is_empty())
            .then(|| add.iter().position(|(name, _)| eq_ignore_case(name, key)))
            .flatten();
        let Some(i) = found else {
            out.push(key, value); // 包括 `=C:` 之类的驱动器当前目录
            continue;
        };
        let (_, new) = add.remove(i);
        if eq_ignore_case(key, "WSLENV") && !value.is_empty() {
            out.push(key, format!("{value}:{new}"));
        } else {
            out.push(key, new);
        }
    }
    add.sort();
    for (key, value) in add {
        out.push(key, value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16le(s: &str, bom: bool) -> Vec<u8> {
        let mut out = if bom { vec![0xff, 0xfe] } else { Vec::new() };
        for unit in s.encode_utf16() {
            out.extend(unit.to_le_bytes());
        }
        out
    }

    #[test]
    fn parse_list_formats() {
        let want = ["Ubuntu", "Debian-12"];
        let cases = [
            (
                "UTF-16LE",
                utf16le("Ubuntu\r\ndocker-desktop\r\nDebian-12\r\n\r\n", false),
            ),
            (
                "UTF-16LE BOM",
                utf16le("Ubuntu\ndocker-desktop-data\nDebian-12\nUbuntu\n", true),
            ),
            ("WSL_UTF8=1", b"Ubuntu\nDebian-12\n".to_vec()),
        ];
        for (name, out) in cases {
            assert_eq!(
                parse_list(&out),
                want,
                "{name}：应去掉 Docker 的内部发行版与重复项"
            );
        }
        assert!(
            parse_list(&utf16le("bad name\r\n", false)).is_empty(),
            "名称不合法的应去掉"
        );
        assert!(parse_list(b"").is_empty());
        // 奇数长度、不成对的代理项：不 panic
        assert!(parse_list(&[0xff, 0xfe, 0x00, 0xd8, b'\n', 0, 7]).is_empty());
    }

    #[test]
    fn name_and_user_validation() {
        assert!(valid_name("Ubuntu-22.04") && valid_name("a_b"));
        assert!(!valid_name("a b") && !valid_name("x;rm") && !valid_name(""));
        assert!(!valid_name(&"a".repeat(65)) && valid_name(&"a".repeat(64)));
        assert!(valid_user("") && valid_user("root") && valid_user("ruan_1") && valid_user("_x-y"));
        assert!(
            !valid_user("Root") && !valid_user("a b") && !valid_user("x;y") && !valid_user("1abc")
        );
        assert!(!valid_user(&format!("a{}", "b".repeat(32))));
        assert_eq!(
            args("Ubuntu", "root", &["--cd", "~"]).unwrap(),
            ["-d", "Ubuntu", "-u", "root", "--cd", "~"]
        );
        assert_eq!(args("Ubuntu", "", &[]).unwrap(), ["-d", "Ubuntu"]);
        assert_eq!(args("bad name", "", &[]), Err(ShellError::InvalidDistro));
        assert_eq!(args("Ubuntu", "Bad", &[]), Err(ShellError::InvalidUser));
        assert_eq!(
            launch("x;y", "", "k").unwrap_err(),
            ShellError::InvalidDistro
        );
    }

    #[test]
    fn merge_env_appends_wslenv() {
        let base: Env = [
            ("=C:", r"C:\x"),
            ("Path", r"C:\bin"),
            ("WSLENV", "FOO/p"),
            ("HOME", "x"),
        ]
        .into_iter()
        .collect();
        let add = vec![
            ("WSLENV", "LC_TERMLET_SESSION/u".to_string()),
            ("LC_TERMLET_SESSION", "k1".to_string()),
            ("home", "y".to_string()),
        ];
        let got = merge_env(&base, add).into_vec();
        let want = [
            ("=C:", r"C:\x"),
            ("Path", r"C:\bin"),
            ("WSLENV", "FOO/p:LC_TERMLET_SESSION/u"),
            ("HOME", "y"),
            ("LC_TERMLET_SESSION", "k1"),
        ];
        assert_eq!(
            got,
            want.map(|(k, v)| (k.to_string(), v.to_string())),
            "合并环境变量"
        );
        let base: Env = [("A", "1")].into_iter().collect();
        let got = merge_env(&base, vec![("WSLENV", "X/u".to_string())]).into_vec();
        assert_eq!(
            got,
            [("A", "1"), ("WSLENV", "X/u")].map(|(k, v)| (k.to_string(), v.to_string())),
            "原来没有 WSLENV 时直接设置"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn unsupported_outside_windows() {
        assert!(distributions().is_empty());
        assert_eq!(
            launch("Ubuntu", "", "").unwrap_err(),
            ShellError::WslUnsupported
        );
    }

    // 本机：列出的都是合法、不是内部的发行版；启动描述带会话标记
    #[cfg(windows)]
    #[test]
    fn local_distributions() {
        if exe().is_err() {
            assert!(distributions().is_empty());
            return;
        }
        let list = distributions();
        println!("本机的 WSL 发行版：{list:?}");
        assert!(list.iter().all(|d| valid_name(d) && !hidden(d)));
        let Some(distro) = list.first() else {
            return;
        };
        let launch = launch(distro, "", "termlet-wsltest-1").unwrap();
        assert!(launch.program.ends_with("wsl.exe"));
        assert_eq!(launch.args, ["-d", distro.as_str(), "--cd", "~"]);
        assert_eq!(launch.env_var(SESSION_ENV), Some("termlet-wsltest-1"));
        assert!(
            launch
                .env_var("WSLENV")
                .is_some_and(|v| v.split(':').any(|item| item == "LC_TERMLET_SESSION/u"))
        );
        assert!(launch.cwd.is_dir());
    }

    // 集成测试：在发行版中确认会话标记经 WSLENV 传了进去（会启动发行版），需要指定发行版，例如
    //   TERMLET_TEST_WSL=Ubuntu cargo test -p termlet-core localshell::wsl
    #[cfg(windows)]
    #[test]
    fn session_env_reaches_linux() {
        let Ok(distro) = std::env::var("TERMLET_TEST_WSL") else {
            println!("未设置 TERMLET_TEST_WSL，跳过 WSL 集成测试");
            return;
        };
        let launch = launch(&distro, "", "termlet-wsltest-2").unwrap();
        // 与终端相同的程序、环境变量与起始目录，只把 --cd ~ 换成执行一条命令
        let mut cmd = std::process::Command::new(&launch.program);
        cmd.args([
            "-d",
            distro.as_str(),
            "-e",
            "sh",
            "-c",
            "echo [$LC_TERMLET_SESSION] $(pwd)",
        ])
        .env_clear()
        .envs(launch.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&launch.cwd);
        let out = crate::localshell::tests::output_of(cmd);
        assert!(out.contains("[termlet-wsltest-2]"), "{out}");
    }
}
