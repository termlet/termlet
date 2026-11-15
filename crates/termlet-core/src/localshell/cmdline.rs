//! 命令行的拆分与拼接（纯字符串处理，各平台都编译，测试在任何系统上都能跑）。
//!
//! - 自定义 shell 的命令行先拆成"程序"与"其余部分"（[`split_command`]）；
//! - Unix 上其余部分按 shell 的规则拆成参数（`split_words`）；
//! - Windows 上其余部分原样交给程序（程序自己解析命令行），同时按 C 运行库的规则拆一份参数
//!   （[`split_windows_args`]），给只能逐个传参数的调用方用；
//! - Windows 上把程序与参数拼成 `CreateProcessW` 的命令行（[`compose_command_line`]）。

/// 把命令行拆成第一项（程序）与其余部分（原样保留，含前导空白）：第一项可以用双引号括起来。
pub(crate) fn split_command(command: &str) -> (&str, &str) {
    let command = command.trim_start_matches([' ', '\t']);
    if let Some(quoted) = command.strip_prefix('"') {
        return match quoted.find('"') {
            Some(end) => (&quoted[..end], &quoted[end + 1..]),
            None => (quoted, ""),
        };
    }
    match command.find([' ', '\t']) {
        Some(i) => (&command[..i], &command[i..]),
        None => (command, ""),
    }
}

/// 按 shell 的规则拆分参数：支持单引号、双引号与反斜杠转义，不做变量展开与通配。
#[cfg(any(unix, test))]
pub(crate) fn split_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    // 当前是否有一个参数（空的引号也算一个）
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some('\'') => {
                if c == '\'' {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            Some(_) => match c {
                '"' => quote = None,
                // 双引号中只有 \" \\ \$ \` 是转义
                '\\' if chars
                    .peek()
                    .is_some_and(|n| matches!(n, '"' | '\\' | '$' | '`')) =>
                {
                    cur.extend(chars.next());
                }
                _ => cur.push(c),
            },
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    in_word = true;
                }
                '\\' if chars.peek().is_some() => {
                    cur.extend(chars.next());
                    in_word = true;
                }
                ' ' | '\t' => {
                    if in_word {
                        words.push(std::mem::take(&mut cur));
                        in_word = false;
                    }
                }
                _ => {
                    cur.push(c);
                    in_word = true;
                }
            },
        }
    }
    if in_word {
        words.push(cur);
    }
    words
}

/// 按 Windows C 运行库（VS 2008 起）的规则拆分程序名之后的参数：空格与制表符分隔；
/// 2n 个反斜杠加 `"` 是 n 个反斜杠并切换引号状态，2n+1 个是 n 个反斜杠加一个字面的 `"`，
/// 其余反斜杠原样保留；引号中的 `""` 是一个字面的 `"`。
#[cfg(any(windows, test))]
pub(crate) fn split_windows_args(s: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut in_arg = false;
    let mut in_quotes = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let mut backslashes = 1;
                while chars.next_if_eq(&'\\').is_some() {
                    backslashes += 1;
                }
                if chars.peek() == Some(&'"') {
                    cur.extend(std::iter::repeat_n('\\', backslashes / 2));
                    if backslashes % 2 == 1 {
                        cur.push('"');
                        chars.next();
                    }
                } else {
                    cur.extend(std::iter::repeat_n('\\', backslashes));
                }
                in_arg = true;
            }
            '"' => {
                if in_quotes && chars.next_if_eq(&'"').is_some() {
                    cur.push('"');
                } else {
                    in_quotes = !in_quotes;
                }
                in_arg = true;
            }
            ' ' | '\t' if !in_quotes => {
                if in_arg {
                    args.push(std::mem::take(&mut cur));
                    in_arg = false;
                }
            }
            _ => {
                cur.push(c);
                in_arg = true;
            }
        }
    }
    if in_arg {
        args.push(cur);
    }
    args
}

/// 命令行中的程序名：含空白、控制字符或以引号开头时整体加引号（其中的引号去掉，程序名中无法转义引号）。
pub(crate) fn quote_program(program: &str) -> String {
    let must_quote =
        program.is_empty() || program.starts_with('"') || program.chars().any(|c| c <= ' ');
    if must_quote {
        format!("\"{}\"", program.replace('"', ""))
    } else {
        program.to_string()
    }
}

/// 按 Windows C 运行库的规则转义一个参数（[`split_windows_args`] 的逆运算）。
pub(crate) fn escape_arg(arg: &str) -> String {
    if arg.is_empty() {
        return "\"\"".to_string();
    }
    let has_space = arg.contains([' ', '\t']);
    let needs_backslash = arg.contains(['"', '\\']);
    if !has_space && !needs_backslash {
        return arg.to_string();
    }
    if !needs_backslash {
        return format!("\"{arg}\"");
    }
    let mut out = String::with_capacity(arg.len() + 2);
    if has_space {
        out.push('"');
    }
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                // 引号前的反斜杠加倍，再转义引号本身
                out.extend(std::iter::repeat_n('\\', backslashes + 1));
                backslashes = 0;
            }
            _ => backslashes = 0,
        }
        out.push(c);
    }
    if has_space {
        // 结尾的反斜杠在右引号前要加倍
        out.extend(std::iter::repeat_n('\\', backslashes));
        out.push('"');
    }
    out
}

/// 拼出 `CreateProcessW` 的命令行：程序名按 [`quote_program`]，参数按 [`escape_arg`]。
pub(crate) fn compose_command_line<S: AsRef<str>>(program: &str, args: &[S]) -> String {
    let mut line = quote_program(program);
    for arg in args {
        line.push(' ');
        line.push_str(&escape_arg(arg.as_ref()));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_command_cases() {
        let cases = [
            ("pwsh.exe -NoLogo", ("pwsh.exe", " -NoLogo")),
            (
                r#"  "C:\Program Files\Git\bin\bash.exe" -i -l"#,
                (r"C:\Program Files\Git\bin\bash.exe", " -i -l"),
            ),
            (r#""C:\a b\x.exe""#, (r"C:\a b\x.exe", "")),
            ("node", ("node", "")),
            ("/usr/bin/env  bash", ("/usr/bin/env", "  bash")),
            (r#""unterminated"#, ("unterminated", "")),
            ("cmd.exe\t/k", ("cmd.exe", "\t/k")),
            ("", ("", "")),
            ("中文 路径", ("中文", " 路径")),
        ];
        for (input, want) in cases {
            assert_eq!(split_command(input), want, "split_command({input:?})");
        }
    }

    #[test]
    fn split_words_cases() {
        let cases: [(&str, &[&str]); 6] = [
            (" -l", &["-l"]),
            (" -c 'echo a  b' x", &["-c", "echo a  b", "x"]),
            (
                r#" "say \"hi\" $x" a\ b ''"#,
                &[r#"say "hi" $x"#, "a b", ""],
            ),
            ("", &[]),
            // 双引号中不是转义的反斜杠原样保留；结尾单独的反斜杠原样保留
            (r#" "a\nb" c\"#, &[r"a\nb", r"c\"]),
            ("'未闭合 的引号", &["未闭合 的引号"]),
        ];
        for (input, want) in cases {
            assert_eq!(split_words(input), want, "split_words({input:?})");
        }
    }

    #[test]
    fn split_windows_args_cases() {
        let cases: [(&str, &[&str]); 8] = [
            (
                r#" /d /k "echo custom-%MARKER%""#,
                &["/d", "/k", "echo custom-%MARKER%"],
            ),
            (r#" "a b" c"#, &["a b", "c"]),
            (r#" a\\\"b"#, &[r#"a\"b"#]),
            (r#" a\\"b c""#, &[r"a\b c"]),
            (r" C:\dir\ x", &[r"C:\dir\", "x"]),
            (r#" "" x"#, &["", "x"]),
            (r#" "say ""hi""" end"#, &[r#"say "hi""#, "end"]),
            ("\t  ", &[]),
        ];
        for (input, want) in cases {
            assert_eq!(
                split_windows_args(input),
                want,
                "split_windows_args({input:?})"
            );
        }
    }

    #[test]
    fn escape_arg_round_trips() {
        let cases = [
            ("", r#""""#),
            ("plain", "plain"),
            (
                r"C:\Windows\System32\cmd.exe",
                r"C:\Windows\System32\cmd.exe",
            ),
            ("a b", r#""a b""#),
            (r#"say "hi""#, r#""say \"hi\"""#),
            (r"C:\a b\", r#""C:\a b\\""#),
            (r#"a\"b"#, r#"a\\\"b"#),
        ];
        for (arg, want) in cases {
            assert_eq!(escape_arg(arg), want, "escape_arg({arg:?})");
            assert_eq!(
                split_windows_args(&format!(" {}", escape_arg(arg))),
                [arg],
                "转义后应能原样拆回：{arg:?}"
            );
        }
    }

    #[test]
    fn compose_command_line_quotes_program_and_args() {
        assert_eq!(
            compose_command_line(r"C:\Program Files\Git\bin\bash.exe", &["-i", "-l"]),
            r#""C:\Program Files\Git\bin\bash.exe" -i -l"#
        );
        assert_eq!(
            compose_command_line(
                r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
                &["-NoLogo", "-Command", "&{Import-Module 'C:\\x y\\a.dll'}"]
            ),
            r#"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe -NoLogo -Command "&{Import-Module 'C:\x y\a.dll'}""#
        );
        assert_eq!(compose_command_line::<&str>("", &[]), r#""""#);
        assert_eq!(quote_program(r#""odd"name"#), r#""oddname""#);
    }
}
