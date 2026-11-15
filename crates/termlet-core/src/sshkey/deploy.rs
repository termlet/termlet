//! 部署公钥：生成在远端执行的脚本，把公钥追加到 `~/.ssh/authorized_keys`（已存在时不重复添加）。
//!
//! 连接、执行（以 `sh` 运行，脚本经标准输入传入，不受登录 shell 种类影响）与验证登录由调用方负责；
//! 这里只有纯函数：生成脚本、解读脚本的输出。

use ssh_key::authorized_keys::Entry;

use super::{KeyError, Malformed};

/// 脚本新增了公钥时输出的标记。
pub const DEPLOY_MARK_ADDED: &str = "termlet-deploy:added";
/// 公钥已存在、没有重复添加时输出的标记。
pub const DEPLOY_MARK_EXISTS: &str = "termlet-deploy:exists";

/// 部署脚本执行成功的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeployOutcome {
    /// 新增了公钥
    Added,
    /// authorized_keys 中已有这把公钥，没有重复添加
    AlreadyPresent,
}

/// 由脚本的标准输出判断结果；两个标记都没有（脚本中途失败）时返回 `None`，
/// 调用方再结合退出码与标准错误给出失败原因。
pub fn deploy_outcome(stdout: &str) -> Option<DeployOutcome> {
    if stdout.contains(DEPLOY_MARK_ADDED) {
        Some(DeployOutcome::Added)
    } else if stdout.contains(DEPLOY_MARK_EXISTS) {
        Some(DeployOutcome::AlreadyPresent)
    } else {
        None
    }
}

/// 生成部署脚本。`public_key` 是 OpenSSH 格式的一行公钥（密钥库中的 `publicKey`）；
/// 公钥行、类型、base64 都经过单引号转义，注释中的控制字符被去掉。
///
/// 作者：ruan
pub fn deploy_script(public_key: &str) -> Result<String, KeyError> {
    let (key_type, blob, line) = authorized_line(public_key)?;
    Ok(format!(
        "umask 077\ncd || exit 1\nkey={}\nktype={}\nkblob={}\n{SCRIPT_BODY}",
        shell_quote(&line),
        shell_quote(&key_type),
        shell_quote(&blob),
    ))
}

/// 脚本正文（在 `key`、`ktype`、`kblob` 三个变量之后）：
/// - umask 077：新建的 ~/.ssh 为 700、authorized_keys 为 600，已有文件的权限不改动；
/// - 忽略注释行，按"类型 + base64"查重（不论注释、选项是否相同），已存在时不重复添加；
/// - 文件末尾没有换行时先补换行，避免与最后一行粘连；
/// - SELinux 系统上恢复安全上下文（与 ssh-copy-id 相同）。
const SCRIPT_BODY: &str = r#"if [ ! -d .ssh ]; then
  mkdir .ssh || exit 1
fi
file=.ssh/authorized_keys
if [ -f "$file" ] && awk -v t="$ktype" -v b="$kblob" '
  /^[ \t]*#/ { next }
  { sub(/\r$/, ""); for (i = 1; i < NF; i++) if ($i == t && $(i + 1) == b) { found = 1; exit } }
  END { exit !found }' "$file"; then
  echo termlet-deploy:exists
  exit 0
fi
if [ -s "$file" ] && [ -n "$(tail -c 1 "$file")" ]; then
  printf '\n' >> "$file" || exit 1
fi
printf '%s\n' "$key" >> "$file" || exit 1
if command -v restorecon >/dev/null 2>&1; then
  restorecon -F .ssh "$file" >/dev/null 2>&1 || :
fi
echo termlet-deploy:added
"#;

/// 由公钥生成要写入 authorized_keys 的一行（类型 公钥 注释），去掉注释中的控制字符，
/// 并返回类型与 base64 内容用于查重。
fn authorized_line(public_key: &str) -> Result<(String, String, String), KeyError> {
    let entry: Entry = public_key
        .trim()
        .parse()
        .map_err(|_| Malformed::Corrupted)?;
    let public = entry.public_key();
    let mut bare = public.clone();
    bare.set_comment("");
    let encoded = bare.to_openssh().map_err(|_| Malformed::Corrupted)?;
    let (key_type, blob) = encoded.split_once(' ').ok_or(Malformed::Corrupted)?;
    // 类型与 base64 只允许这些字符，可以安全地交给 awk -v
    let type_ok = !key_type.is_empty()
        && key_type
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"@.-".contains(&b));
    let blob_body = blob.trim_end_matches('=');
    let blob_ok = !blob_body.is_empty()
        && blob_body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/');
    if !type_ok || !blob_ok {
        return Err(Malformed::Corrupted.into());
    }
    let comment: String = String::from_utf8_lossy(public.comment().as_bytes())
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let comment = comment.trim();
    let mut line = format!("{key_type} {blob}");
    if !comment.is_empty() {
        line.push(' ');
        line.push_str(comment);
    }
    Ok((key_type.to_string(), blob.to_string(), line))
}

/// 用单引号包裹，内容中的每个单引号替换为：结束引号、转义的单引号、重新开始引号，
/// 适用于 POSIX sh；结果中不会有任何字符被 shell 解释。
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use super::*;
    use crate::sshkey::{KeyKind, generate};

    #[test]
    fn shell_quote_cases() {
        for (input, want) in [
            ("", "''"),
            ("abc", "'abc'"),
            ("it's", r"'it'\''s'"),
            ("''", r"''\'''\'''"),
            (r#"$(x) "y""#, r#"'$(x) "y"'"#),
            ("a\\b`c`;&", "'a\\b`c`;&'"),
        ] {
            assert_eq!(shell_quote(input), want, "shell_quote({input:?})");
        }
    }

    #[test]
    fn authorized_line_strips_controls() {
        let key = generate(KeyKind::Ed25519, "").unwrap();
        let base = key.public_key();
        let (key_type, blob, line) = authorized_line(&format!("{base} a\x1b[31mb\tc  ")).unwrap();
        assert_eq!(key_type, "ssh-ed25519");
        assert_eq!(base, format!("{key_type} {blob}"));
        assert_eq!(line, format!("{base} a[31mbc"));

        // 没有注释时只有类型和公钥
        assert_eq!(authorized_line(&base).unwrap().2, base);

        assert_eq!(
            authorized_line("ssh-ed25519 not-base64!").unwrap_err(),
            KeyError::Malformed(Malformed::Corrupted)
        );
    }

    #[test]
    fn deploy_outcome_marks() {
        assert_eq!(
            deploy_outcome("x\ntermlet-deploy:added\n"),
            Some(DeployOutcome::Added)
        );
        assert_eq!(
            deploy_outcome("termlet-deploy:exists\n"),
            Some(DeployOutcome::AlreadyPresent)
        );
        assert_eq!(deploy_outcome("mkdir: Permission denied\n"), None);
        // 脚本中的标记与常量一致
        assert!(SCRIPT_BODY.contains(&format!("echo {DEPLOY_MARK_ADDED}\n")));
        assert!(SCRIPT_BODY.contains(&format!("echo {DEPLOY_MARK_EXISTS}\n")));
    }

    /// 在本机用 sh 执行部署脚本（HOME 指向临时目录），返回标准输出；本机没有 sh 时返回 `None`。
    fn run_local(home: &Path, public_key: &str) -> Option<String> {
        use std::io::Write;
        let script = deploy_script(public_key).unwrap();
        let mut child = match Command::new("sh")
            .current_dir(home)
            .env("HOME", home.to_string_lossy().replace('\\', "/"))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => return None,
        };
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "脚本执行失败: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// 在本机执行部署脚本：新建目录与文件、重复部署不重复添加、补换行、按类型 + 公钥查重、
    /// 忽略注释行与 CRLF、注释中的特殊字符不会被 shell 解释。
    #[test]
    fn deploy_script_local() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let file = home.join(".ssh").join("authorized_keys");
        let read = || std::fs::read_to_string(&file).unwrap_or_default();

        let evil = r#"it's $(touch pwned) "q" \ ; `touch pwned2` & | > x"#;
        let key = generate(KeyKind::Ed25519, evil).unwrap();
        let public = key.public_key();

        let Some(out) = run_local(home, &public) else {
            eprintln!("本机没有 sh，跳过");
            return;
        };
        assert!(out.contains(DEPLOY_MARK_ADDED), "首次部署应添加: {out}");
        assert_eq!(read(), format!("{public}\n"));
        for name in ["pwned", "pwned2", "x"] {
            assert!(
                !home.join(name).exists(),
                "注释被 shell 解释执行了（出现 {name}）"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&home.join(".ssh")), 0o700);
            assert_eq!(mode(&file), 0o600);
        }

        // 重复部署：不重复添加
        let out = run_local(home, &public).unwrap();
        assert!(out.contains(DEPLOY_MARK_EXISTS), "重复部署不应添加: {out}");
        assert_eq!(read(), format!("{public}\n"));

        // 已有同一把公钥（带选项、注释不同、CRLF）：视为已存在
        let other = generate(KeyKind::Ed25519, "other").unwrap();
        let fields: Vec<&str> = public.split_whitespace().collect();
        let existing = format!(
            "# {}\r\nno-pty,command=\"echo a b\" {} {} old-comment\r\n",
            other.public_key(),
            fields[0],
            fields[1]
        );
        std::fs::write(&file, existing).unwrap();
        let out = run_local(home, &public).unwrap();
        assert!(
            out.contains(DEPLOY_MARK_EXISTS),
            "带选项与 CRLF 的同一公钥应视为已存在: {out}"
        );

        // 只在注释行中出现的公钥不算；文件末尾没有换行时先补换行
        let content = format!(
            "# {}\nssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl last",
            other.public_key()
        );
        std::fs::write(&file, content).unwrap();
        let out = run_local(home, &other.public_key()).unwrap();
        assert!(
            out.contains(DEPLOY_MARK_ADDED),
            "注释行中的公钥不应算已存在: {out}"
        );
        assert!(
            read().ends_with(&format!(" last\n{}\n", other.public_key())),
            "应先补换行再追加:\n{}",
            read()
        );
    }
}
