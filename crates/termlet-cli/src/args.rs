//! `exec`、`upload`、`download` 的参数：位置参数与选项（批量执行的选主机、并发、超时等，`exec` 的 `--terminal`、
//! `--stdin`、`--script`、`--input`）。
//!
//! 选项可以放在位置参数前后；`--` 之后的都是位置参数（`--script` 的参数）。`--terminal` 沿用以前的写法：只认前两个
//! 参数中的第一个（命令本身是 `--terminal` 时原样保留）。批量模式由写法决定：出现 `-g`、`-t`、`-H`、`--all` 之一
//! 就是批量（只选中一台也按批量输出），否则与以前相同。

use std::path::PathBuf;

use termlet_proto::Selector;

use crate::i18n::Lang;
use crate::messages;

/// 解析出的参数。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    pub positional: Vec<String>,
    /// `--` 之后的（`--script` 的参数）
    pub rest: Vec<String>,
    pub selector: Selector,
    pub list: bool,
    pub parallel: Option<u32>,
    /// 秒
    pub timeout: Option<u64>,
    pub max_fail: Option<u32>,
    pub no_group: bool,
    pub output_dir: Option<PathBuf>,
    pub json_lines: bool,
    pub terminal: bool,
    pub stdin: bool,
    pub script: Option<String>,
    pub input: Option<String>,
    /// 出现过的只用于批量执行的选项（单台时报错用）
    batch_only: Option<&'static str>,
    /// 出现过的只用于 `exec` 的选项
    exec_only: Option<&'static str>,
}

impl Parsed {
    /// 批量模式：选了分组、标签、主机列表或全部。
    pub fn is_batch(&self) -> bool {
        let s = &self.selector;
        s.all || !s.hosts.is_empty() || !s.groups.is_empty() || !s.tags.is_empty()
    }

    /// 单台时不能用批量执行的选项；不是 `exec` 时不能用 `exec` 的选项。
    pub fn check(&self, exec: bool, lang: Lang) -> Result<(), String> {
        if !self.is_batch()
            && let Some(flag) = self.batch_only
        {
            return Err(messages::BATCH_ONLY_FLAG.fill(lang, &[&flag]));
        }
        if !exec && let Some(flag) = self.exec_only {
            return Err(messages::EXEC_ONLY_FLAG.fill(lang, &[&flag]));
        }
        if self.terminal && (self.script.is_some() || self.input.is_some()) {
            return Err(messages::TERMINAL_WITH_INPUT.get(lang).into());
        }
        if self.terminal && self.is_batch() {
            return Err(messages::TERMINAL_BATCH.get(lang).into());
        }
        let sources = [self.stdin, self.script.is_some(), self.input.is_some()];
        if sources.iter().filter(|s| **s).count() > 1 {
            return Err(messages::INPUT_CONFLICT.get(lang).into());
        }
        if !self.rest.is_empty() && self.script.is_none() {
            return Err(messages::REST_WITHOUT_SCRIPT.get(lang).into());
        }
        Ok(())
    }
}

/// 解析参数（不含子命令本身）。
pub fn parse(args: &[String], lang: Lang) -> Result<Parsed, String> {
    let mut parsed = Parsed::default();
    let mut iter = args.iter().enumerate().peekable();
    let value = |iter: &mut std::iter::Peekable<_>, flag: &str| -> Result<String, String> {
        match Iterator::next(iter) {
            Some((_, v)) => Ok(String::clone(v)),
            None => Err(messages::FLAG_NEEDS_VALUE.fill(lang, &[&flag])),
        }
    };
    let number = |text: String, flag: &str| -> Result<u64, String> {
        text.trim()
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| messages::FLAG_NUMBER.fill(lang, &[&flag, &text]))
    };
    while let Some((i, arg)) = iter.next() {
        let flag = arg.as_str();
        match flag {
            "--" => {
                parsed.rest = iter.by_ref().map(|(_, a)| a.clone()).collect();
                break;
            }
            // 与以前相同：只在前两个参数中认，命令本身是 --terminal 时原样保留
            "--terminal" if i < 2 && !parsed.terminal => {
                parsed.terminal = true;
                parsed.exec_only.get_or_insert("--terminal");
            }
            "-g" | "--group" => {
                let v = value(&mut iter, flag)?;
                parsed.selector.groups.push(v);
            }
            "-t" | "--tag" => {
                let v = value(&mut iter, flag)?;
                parsed.selector.tags.push(v);
            }
            "-H" | "--hosts" => {
                let v = value(&mut iter, flag)?;
                parsed.selector.hosts.extend(host_list(&v, lang)?);
            }
            "-x" | "--exclude" => {
                let v = value(&mut iter, flag)?;
                parsed.selector.exclude.extend(host_list(&v, lang)?);
                parsed.batch_only.get_or_insert("-x");
            }
            "--all" => parsed.selector.all = true,
            "--list" => {
                parsed.list = true;
                parsed.batch_only.get_or_insert("--list");
            }
            "-j" | "--parallel" => {
                let v = value(&mut iter, flag)?;
                parsed.parallel = Some(number(v, flag)?.min(u32::MAX as u64) as u32);
                parsed.batch_only.get_or_insert("-j");
            }
            "--timeout" => {
                let v = value(&mut iter, flag)?;
                parsed.timeout = Some(number(v, flag)?);
                parsed.batch_only.get_or_insert("--timeout");
            }
            "--max-fail" => {
                let v = value(&mut iter, flag)?;
                parsed.max_fail = Some(number(v, flag)?.min(u32::MAX as u64) as u32);
                parsed.batch_only.get_or_insert("--max-fail");
            }
            "--no-group" => {
                parsed.no_group = true;
                parsed.batch_only.get_or_insert("--no-group");
            }
            "--output-dir" => {
                let v = value(&mut iter, flag)?;
                let v = termlet_mcp::local_path::from_wsl(&v, lang)?;
                parsed.output_dir = Some(PathBuf::from(v));
                parsed.batch_only.get_or_insert("--output-dir");
            }
            "--json-lines" => {
                parsed.json_lines = true;
                parsed.batch_only.get_or_insert("--json-lines");
            }
            "--stdin" => {
                parsed.stdin = true;
                parsed.exec_only.get_or_insert("--stdin");
            }
            "--script" => {
                parsed.script = Some(value(&mut iter, flag)?);
                parsed.exec_only.get_or_insert("--script");
            }
            "--input" => {
                parsed.input = Some(value(&mut iter, flag)?);
                parsed.exec_only.get_or_insert("--input");
            }
            _ => parsed.positional.push(arg.clone()),
        }
    }
    Ok(parsed)
}

/// 逗号分隔的主机列表；`@文件` 从文件读（每行一个，`#` 之后是注释，空行跳过；WSL 中的写法先换算）。
fn host_list(value: &str, lang: Lang) -> Result<Vec<String>, String> {
    if let Some(path) = value.strip_prefix('@') {
        let path = &termlet_mcp::local_path::from_wsl(path, lang)?;
        let text = std::fs::read_to_string(path)
            .map_err(|e| messages::HOSTS_FILE.fill(lang, &[&path, &e]))?;
        return Ok(text
            .lines()
            .map(|line| line.split('#').next().unwrap_or_default().trim())
            .filter(|line| !line.is_empty())
            .flat_map(|line| line.split(','))
            .map(|item| item.trim().to_string())
            .filter(|item| !item.is_empty())
            .collect());
    }
    Ok(value
        .split(',')
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn single_host_stays_the_same() {
        let p = parse(&args(&["web-1", "uptime"]), Lang::Zh).unwrap();
        assert!(!p.is_batch());
        assert_eq!(p.positional, ["web-1", "uptime"]);
        // --terminal 前两个参数中的第一个是选项，命令本身是 --terminal 时原样保留
        let p = parse(&args(&["--terminal", "web-1", "--terminal"]), Lang::Zh).unwrap();
        assert!(p.terminal);
        assert_eq!(p.positional, ["web-1", "--terminal"]);
        let p = parse(&args(&["web-1", "--terminal", "uptime"]), Lang::Zh).unwrap();
        assert!(p.terminal);
        assert_eq!(p.positional, ["web-1", "uptime"]);
    }

    #[test]
    fn batch_selection_and_options() {
        let p = parse(
            &args(&[
                "-g",
                "生产",
                "-t",
                "nginx",
                "-H",
                "web-1, web-2",
                "-x",
                "web-3",
                "-j",
                "4",
                "--timeout",
                "30",
                "--max-fail",
                "1",
                "uptime",
            ]),
            Lang::Zh,
        )
        .unwrap();
        assert!(p.is_batch());
        assert_eq!(p.selector.groups, ["生产"]);
        assert_eq!(p.selector.tags, ["nginx"]);
        assert_eq!(p.selector.hosts, ["web-1", "web-2"]);
        assert_eq!(p.selector.exclude, ["web-3"]);
        assert_eq!(
            (p.parallel, p.timeout, p.max_fail),
            (Some(4), Some(30), Some(1))
        );
        assert_eq!(p.positional, ["uptime"]);
        assert!(p.check(true, Lang::Zh).is_ok());

        let p = parse(&args(&["--all", "--list"]), Lang::Zh).unwrap();
        assert!(p.is_batch() && p.list);

        // --script 的参数在 -- 之后
        let p = parse(
            &args(&["-g", "a", "--script", "d.sh", "--", "v1", "--force"]),
            Lang::En,
        )
        .unwrap();
        assert_eq!(p.script.as_deref(), Some("d.sh"));
        assert_eq!(p.rest, ["v1", "--force"]);
    }

    #[test]
    fn hosts_from_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("retry.txt");
        std::fs::write(&file, "h_1  # web-1\n\n# 注释\nh_2,h_3\n").unwrap();
        let p = parse(
            &args(&["-H", &format!("@{}", file.display()), "uptime"]),
            Lang::Zh,
        )
        .unwrap();
        assert_eq!(p.selector.hosts, ["h_1", "h_2", "h_3"]);
        let missing = format!("@{}", dir.path().join("nope.txt").display());
        assert!(parse(&args(&["-H", &missing]), Lang::Zh).is_err());
    }

    #[test]
    fn bad_flags() {
        for bad in [
            &["-g"][..],
            &["-j", "0", "x"],
            &["-j", "x", "y"],
            &["--timeout", "-1", "x"],
        ] {
            assert!(parse(&args(bad), Lang::Zh).is_err(), "{bad:?}");
        }
        // 单台时不能用批量执行的选项
        let p = parse(&args(&["web-1", "--timeout", "5", "uptime"]), Lang::Zh).unwrap();
        assert!(p.check(true, Lang::Zh).is_err());
        // 不是 exec 时不能用 exec 的选项
        let p = parse(&args(&["-g", "a", "--stdin", "x", "y"]), Lang::Zh).unwrap();
        assert!(p.check(false, Lang::Zh).is_err());
        // 标准输入的来源只能有一个；-- 之后的参数只给 --script
        let p = parse(&args(&["h", "--stdin", "--input", "f"]), Lang::Zh).unwrap();
        assert!(p.check(true, Lang::Zh).is_err());
        let p = parse(&args(&["h", "cmd", "--", "a"]), Lang::Zh).unwrap();
        assert!(p.check(true, Lang::Zh).is_err());
        // --terminal 不能批量，也不能带标准输入
        let p = parse(&args(&["--terminal", "-g", "a", "x"]), Lang::Zh).unwrap();
        assert!(p.check(true, Lang::Zh).is_err());
    }
}
