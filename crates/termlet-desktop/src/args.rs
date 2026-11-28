//! Termlet 自己的启动参数；其余的原样交给外部调起的解析（见 [`crate::launch`]）或转交给已在运行的实例。
//!
//! - `--headless`：被 `termlet` 拉起时不开窗口，只提供命令行服务；
//! - `--log-level <级别或过滤规则>`（也可以写成 `--log-level=…`）：日志的详细程度，`normal` / `detailed` / `full`，
//!   或 `tracing` 的过滤规则（与环境变量 `TERMLET_LOG` 相同的写法），见 `termlet_core::logging`；
//! - `--verbose`：等于 `--log-level detailed`；
//! - `--open-folder <目录>`（也可以写成 `--open-folder=…`，或与 MobaXterm 相同的 `-openfolder <目录>`）：用默认的
//!   本地终端在这个目录打开一个 Tab（见 [`crate::open_folder`]）。

/// 拆开后的启动参数。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StartArgs {
    pub headless: bool,
    /// `--log-level` / `--verbose` 指定的详细程度
    pub log_level: Option<String>,
    /// `--open-folder` / `-openfolder` 指定的目录（原样，还没有换成绝对路径）
    pub open_folder: Option<String>,
    /// 其余的参数（外部程序调起时带的）
    pub rest: Vec<String>,
}

/// 拆开本进程的启动参数（不含程序本身）。
pub fn parse(args: impl IntoIterator<Item = String>) -> StartArgs {
    let mut parsed = StartArgs::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--headless" => parsed.headless = true,
            "--verbose" => parsed.log_level = Some("detailed".into()),
            "--log-level" => match args.next() {
                Some(value) => parsed.log_level = Some(value),
                None => eprintln!("--log-level 后面要写级别：normal、detailed 或 full"),
            },
            _ if is_open_folder(&arg) => match args.next() {
                Some(value) => parsed.open_folder = Some(value),
                None => eprintln!("{arg} 后面要写目录"),
            },
            _ => {
                if let Some(value) = arg.strip_prefix("--log-level=") {
                    parsed.log_level = Some(value.to_string());
                } else if let Some(value) = open_folder_value(&arg) {
                    parsed.open_folder = Some(value.to_string());
                } else {
                    parsed.rest.push(arg);
                }
            }
        }
    }
    parsed
}

/// 打开目录的选项：`--open-folder`，或 MobaXterm 的 `-openfolder`（不区分大小写）。
fn is_open_folder(arg: &str) -> bool {
    arg == "--open-folder" || arg.eq_ignore_ascii_case("-openfolder")
}

/// `--open-folder=<目录>`、`-openfolder=<目录>` 中的目录。
fn open_folder_value(arg: &str) -> Option<&str> {
    let (option, value) = arg.split_once('=')?;
    is_open_folder(option).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> StartArgs {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn own_options_are_taken_out_and_the_rest_kept_in_order() {
        let parsed = parse_strs(&["--log-level", "full", "-ssh", "ops@10.0.0.5", "-P", "2222"]);
        assert_eq!(parsed.log_level.as_deref(), Some("full"));
        assert!(!parsed.headless);
        assert_eq!(parsed.rest, ["-ssh", "ops@10.0.0.5", "-P", "2222"]);

        let parsed = parse_strs(&["--headless", "--log-level=info,termlet_core=trace"]);
        assert!(parsed.headless);
        assert_eq!(parsed.log_level.as_deref(), Some("info,termlet_core=trace"));
        assert!(parsed.rest.is_empty());

        let parsed = parse_strs(&["ssh://ops@host", "--verbose"]);
        assert_eq!(parsed.log_level.as_deref(), Some("detailed"));
        assert_eq!(parsed.rest, ["ssh://ops@host"]);
    }

    // 打开目录：MobaXterm 的 -openfolder（不区分大小写）与 --open-folder，值可以另起一项或写在等号后；
    // 引号已由系统去掉，值中的空格、中文原样保留；不认得的照样交给外部调起的解析
    #[test]
    fn open_folder_takes_the_next_argument() {
        let parsed = parse_strs(&["-openfolder", r"D:\my work\项目"]);
        assert_eq!(parsed.open_folder.as_deref(), Some(r"D:\my work\项目"));
        assert!(parsed.rest.is_empty());

        let parsed = parse_strs(&["--verbose", "-OpenFolder", "~/src", "-ssh", "ops@h"]);
        assert_eq!(parsed.open_folder.as_deref(), Some("~/src"));
        assert_eq!(parsed.rest, ["-ssh", "ops@h"]);

        let parsed = parse_strs(&["--open-folder", "/home/ruan/a b"]);
        assert_eq!(parsed.open_folder.as_deref(), Some("/home/ruan/a b"));
        let parsed = parse_strs(&[r"--open-folder=C:\x y"]);
        assert_eq!(parsed.open_folder.as_deref(), Some(r"C:\x y"));
        let parsed = parse_strs(&["-openfolder=."]);
        assert_eq!(parsed.open_folder.as_deref(), Some("."));

        // 少了值：不打开，也不把选项当成别的参数
        let parsed = parse_strs(&["-openfolder"]);
        assert_eq!(parsed.open_folder, None);
        assert!(parsed.rest.is_empty());
        // 只认这两种写法
        let parsed = parse_strs(&["--openfolder", "x"]);
        assert_eq!(parsed.open_folder, None);
        assert_eq!(parsed.rest, ["--openfolder", "x"]);
    }
}
