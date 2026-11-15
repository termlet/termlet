//! Termlet 自己的启动参数；其余的原样交给外部调起的解析（见 [`crate::launch`]）或转交给已在运行的实例。
//!
//! - `--headless`：被 `termlet` 拉起时不开窗口，只提供命令行服务；
//! - `--log-level <级别或过滤规则>`（也可以写成 `--log-level=…`）：日志的详细程度，`normal` / `detailed` / `full`，
//!   或 `tracing` 的过滤规则（与环境变量 `TERMLET_LOG` 相同的写法），见 `termlet_core::logging`；
//! - `--verbose`：等于 `--log-level detailed`。

/// 拆开后的启动参数。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StartArgs {
    pub headless: bool,
    /// `--log-level` / `--verbose` 指定的详细程度
    pub log_level: Option<String>,
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
            _ => match arg.strip_prefix("--log-level=") {
                Some(value) => parsed.log_level = Some(value.to_string()),
                None => parsed.rest.push(arg),
            },
        }
    }
    parsed
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
}
