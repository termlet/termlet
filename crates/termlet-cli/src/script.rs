//! `--script <文件> [-- 参数…]` 与 `--input <文件>`：本机文件作为远程命令的标准输入。
//!
//! - `--script`：按首行的 `#!` 选 shell（bash、sh、zsh、dash、ksh、ash；没有 `#!` 时用 sh），命令写成
//!   `<shell> -s -- '参数'…`，脚本经标准输入交给它，不落在服务器的磁盘上。其他解释器（python 等）请用 `--input`
//!   与自己写的命令（`--input x.py "python3 -"`）。脚本同样按危险命令规则检查。
//! - `--input`：文件内容原样作为标准输入（数据，不检查危险命令），命令照常写。
//!
//! 都最多 1MB，必须是 UTF-8 文本（开头的 BOM 去掉）。

use termlet_proto::MAX_STDIN_COMMAND;

use crate::i18n::Lang;
use crate::messages;

/// 能用 `-s` 从标准输入读脚本的 shell。
const SHELLS: [&str; 6] = ["bash", "sh", "zsh", "dash", "ksh", "ash"];

/// 读本机文件（最多 1MB 的 UTF-8 文本；WSL 中的写法先换算，见 `termlet_mcp::local_path`）。
pub fn read_text(path: &str, lang: Lang) -> Result<String, String> {
    let path = &termlet_mcp::local_path::from_wsl(path, lang)?;
    let data = std::fs::read(path).map_err(|e| messages::READ_FILE.fill(lang, &[&path, &e]))?;
    if data.len() > MAX_STDIN_COMMAND {
        return Err(messages::FILE_TOO_LARGE.fill(lang, &[&path]));
    }
    let text = String::from_utf8(data).map_err(|_| messages::FILE_NOT_UTF8.fill(lang, &[&path]))?;
    Ok(text
        .strip_prefix('\u{feff}')
        .map(str::to_string)
        .unwrap_or(text))
}

/// `--script` 的命令：`bash -s -- 'a' 'b'`。首行的 `#!` 不是认识的 shell 时为错误。
pub fn command_for(script: &str, args: &[String], lang: Lang) -> Result<String, String> {
    let shell = match script.lines().next().and_then(|l| l.strip_prefix("#!")) {
        None => "sh".to_string(),
        Some(line) => {
            let mut words = line.split_whitespace();
            let program = words.next().unwrap_or_default();
            let mut name = program.rsplit('/').next().unwrap_or_default();
            // #!/usr/bin/env bash
            if name == "env" {
                name = words.find(|w| !w.starts_with('-')).unwrap_or_default();
            }
            if !SHELLS.contains(&name) {
                return Err(messages::SCRIPT_INTERPRETER.fill(lang, &[&name]));
            }
            name.to_string()
        }
    };
    let mut command = format!("{shell} -s --");
    for arg in args {
        command.push(' ');
        command.push_str(&quote(arg));
    }
    Ok(command)
}

/// POSIX shell 的单引号写法：`it's` → `'it'\''s'`。
fn quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_shell_from_the_shebang() {
        let args = vec!["v1.4".to_string(), "it's".to_string()];
        assert_eq!(
            command_for("echo hi\n", &args, Lang::Zh).unwrap(),
            r"sh -s -- 'v1.4' 'it'\''s'"
        );
        assert_eq!(
            command_for("#!/bin/bash\nset -e\n", &[], Lang::Zh).unwrap(),
            "bash -s --"
        );
        assert_eq!(
            command_for("#!/usr/bin/env -S zsh\n", &[], Lang::Zh).unwrap(),
            "zsh -s --"
        );
        let e = command_for("#!/usr/bin/env python3\n", &[], Lang::Zh).unwrap_err();
        assert!(e.contains("python3") && e.contains("--input"), "{e}");
    }

    #[test]
    fn reads_utf8_text_up_to_1mb() {
        let dir = tempfile::tempdir().unwrap();
        let ok = dir.path().join("a.sh");
        std::fs::write(&ok, "\u{feff}echo 你好\n").unwrap();
        assert_eq!(
            read_text(&ok.display().to_string(), Lang::Zh).unwrap(),
            "echo 你好\n"
        );
        let big = dir.path().join("big.sh");
        std::fs::write(&big, vec![b'x'; MAX_STDIN_COMMAND + 1]).unwrap();
        assert!(read_text(&big.display().to_string(), Lang::Zh).is_err());
        let binary = dir.path().join("b.bin");
        std::fs::write(&binary, [0xff, 0xfe, 0x00]).unwrap();
        assert!(read_text(&binary.display().to_string(), Lang::En).is_err());
        assert!(read_text(&dir.path().join("nope").display().to_string(), Lang::Zh).is_err());
    }
}
