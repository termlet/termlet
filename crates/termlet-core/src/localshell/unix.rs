//! Linux、macOS：检测 shell、取用户的环境变量、在 PATH 中查找程序。

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::cmdline;
use super::env::{self, Env};
use super::{Profile, ShellKind, exists};

/// 设置给 shell 的 TERM：终端按 xterm 的规则实现，支持 256 色。
pub(super) const TERM: &str = "xterm-256color";

/// 从 `/etc/shells` 中列出的 shell（其余如 rbash、dash 不常用作交互式终端）。
const KNOWN_SHELLS: &[&str] = &["bash", "zsh", "fish", "nu", "pwsh", "ksh", "tcsh"];

/// 列出 `$SHELL`（用户的登录 shell，排在最前）与 `/etc/shells` 中的常见 shell，同名的只列一次。
/// 都以登录 shell 启动（与 macOS 的终端相同），读取用户的 profile，从桌面启动时 PATH 也是完整的。
pub(super) fn detect(env: &Env) -> Vec<Profile> {
    let etc_shells = std::fs::read_to_string("/etc/shells").unwrap_or_default();
    detect_from(env.var("SHELL"), &etc_shells)
}

fn detect_from(shell: &str, etc_shells: &str) -> Vec<Profile> {
    let listed = etc_shells
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('/'));
    let mut out: Vec<Profile> = Vec::new();
    for (i, path) in std::iter::once(shell).chain(listed).enumerate() {
        let name = Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // $SHELL 不论是什么都列出；/etc/shells 中只列常见的
        if path.is_empty()
            || (i > 0 && !KNOWN_SHELLS.contains(&name.as_str()))
            || !exists(Path::new(path))
            || out.iter().any(|p| p.id == name)
        {
            continue;
        }
        let args = if name == "pwsh" {
            vec!["-NoLogo".to_string(), "-Login".to_string()]
        } else {
            vec!["-l".to_string()]
        };
        out.push(Profile {
            id: name.clone(),
            name,
            custom_name: None,
            kind: ShellKind::Unix,
            path: PathBuf::from(path),
            args,
            command: String::new(),
            dir: String::new(),
            env: Default::default(),
        });
    }
    out
}

/// 用户的环境变量：Termlet 进程的环境变量，去掉启动 Termlet 的程序（例如 VS Code 的终端）带来的
/// Electron 变量与会话标记。不是 UTF-8 的变量（很少见）略过。
pub(super) fn user_env() -> Env {
    filter_inherited(
        std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))),
    )
}

pub(super) fn filter_inherited(vars: impl IntoIterator<Item = (String, String)>) -> Env {
    vars.into_iter()
        .filter(|(k, _)| {
            !(k.starts_with("ELECTRON_")
                || k.starts_with("CHROME_")
                || k == "NODE_OPTIONS"
                || k == super::wsl::SESSION_ENV)
        })
        .collect()
}

/// 用户主目录。
pub(super) fn home_of(env: &Env) -> PathBuf {
    match env.var("HOME") {
        "" => dirs::home_dir().unwrap_or_default(),
        home => PathBuf::from(home),
    }
}

/// 查找要运行的程序：含 `/` 时直接检查；只有文件名时在 `env` 的 PATH 中查找。返回绝对路径。
pub(super) fn look_path(name: &str, env: &Env) -> Option<PathBuf> {
    let name = env::expand(name, env);
    if name.is_empty() {
        return None;
    }
    let executable = |p: &Path| {
        std::fs::metadata(p).is_ok_and(|m| !m.is_dir() && m.permissions().mode() & 0o111 != 0)
    };
    let absolute = |p: PathBuf| std::path::absolute(&p).unwrap_or(p);
    if name.contains('/') {
        let path = PathBuf::from(&name);
        return executable(&path).then(|| absolute(path));
    }
    std::env::split_paths(env.var("PATH"))
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(&name))
        .find(|p| executable(p))
        .map(absolute)
}

/// 自定义 shell 的参数：按 shell 的规则拆分（支持单引号、双引号与反斜杠转义）。
pub(super) fn custom_args(rest: &str) -> (Vec<String>, Option<String>) {
    (cmdline::split_words(rest), None)
}

/// 参数写成一行（[`custom_args`] 能原样拆回）。
pub(super) fn join_args(args: &[String]) -> String {
    cmdline::join_words(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_from_env_comes_first() {
        let etc =
            "# /etc/shells\n/bin/sh\n/bin/dash\n  /bin/bash  \n/usr/bin/bash\n/bin/nope-shell\n";
        let list = detect_from("/bin/sh", etc);
        assert_eq!(list[0].id, "sh");
        assert_eq!(list[0].path, Path::new("/bin/sh"));
        assert_eq!(list[0].args, ["-l"]);
        assert!(list.iter().all(|p| p.id != "dash"), "不常用的不列出");
        assert!(
            list.iter().filter(|p| p.id == "bash").count() <= 1,
            "同名的只列一次"
        );
        assert!(detect_from("", "").is_empty());
    }

    #[test]
    fn inherited_electron_vars_are_dropped() {
        let env = filter_inherited(
            [
                ("PATH", "/bin"),
                ("ELECTRON_RUN_AS_NODE", "1"),
                ("CHROME_DESKTOP", "x"),
                ("NODE_OPTIONS", "--inspect"),
                ("LC_TERMLET_SESSION", "k1"),
            ]
            .map(|(k, v)| (k.to_string(), v.to_string())),
        );
        assert_eq!(env.iter().collect::<Vec<_>>(), [("PATH", "/bin")]);
    }

    #[test]
    fn look_path_checks_executable_bit() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tool");
        std::fs::write(&tool, b"#!/bin/sh\n").unwrap();
        let plain = tmp.path().join("plain");
        std::fs::write(&plain, b"x").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env: Env = [("PATH", format!("/nope:{}", tmp.path().display()))]
            .into_iter()
            .collect();
        assert_eq!(look_path("tool", &env), Some(tool.clone()));
        assert_eq!(look_path(&tool.to_string_lossy(), &Env::new()), Some(tool));
        assert_eq!(look_path("plain", &env), None, "没有执行权限");
        assert_eq!(look_path("missing", &env), None);
    }
}
