//! `termlet skill`（打印 SKILL.md）与 `termlet skill install [--agent claude|agents|workbuddy | --dir <目录>]`。

use std::path::PathBuf;

use serde::Serialize;
use termlet_proto::ExitCode;
use termlet_proto::skill::{self, Agent};

use crate::i18n::Lang;
use crate::messages;
use crate::output::Output;

/// `skill install` 在 JSON 模式下的输出。
#[derive(Debug, Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InstallResult {
    pub path: String,
    pub replaced: bool,
}

/// 安装到哪里。
#[derive(Debug, PartialEq, Eq)]
pub enum Where {
    Dir(PathBuf),
    Agent(Agent),
}

pub fn run(out: &mut Output<'_>, args: &[String]) -> i32 {
    if args.is_empty() {
        let _ = out.stdout.write_all(skill::CONTENT.as_bytes());
        return ExitCode::Success.into();
    }
    let lang = out.lang;
    let target = match parse_install(args, lang) {
        Ok(target) => target,
        Err(e) => return out.usage_error(&e),
    };
    let dir = match target {
        Where::Dir(dir) => dir,
        Where::Agent(agent) => match home_dir() {
            Some(home) => skill::dir(&home, agent, &skill::system_env),
            None => return out.usage_error(messages::NO_HOME_DIR.get(lang)),
        },
    };
    let (path, replaced) = match skill::install(&dir) {
        Ok(done) => done,
        Err(e) => {
            return out.fail(
                ExitCode::Usage,
                messages::CODE_SKILL_INSTALL_FAILED,
                &e.to_string(),
            );
        }
    };
    let result = InstallResult {
        path: path.display().to_string(),
        replaced,
    };
    if out.json {
        return out.print_json(&result);
    }
    let text = if replaced {
        messages::SKILL_REPLACED
    } else {
        messages::SKILL_WRITTEN
    };
    let _ = writeln!(out.stdout, "{}", text.fill(lang, &[&result.path]));
    ExitCode::Success.into()
}

/// 用户主目录（`USERPROFILE` / `HOME`）。
fn home_dir() -> Option<PathBuf> {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// 解析 `install [--agent claude|agents|workbuddy | --dir <目录>]`，默认 Claude Code。
/// `codex` 等同 `agents`：Codex 已改为读取通用目录 `~/.agents/skills`，不再安装到它的旧位置。
/// `pi` 也等同 `agents`：Pi 除了自己的 `~/.pi/agent/skills`，也读取通用目录。
pub fn parse_install(args: &[String], lang: Lang) -> Result<Where, String> {
    let usage = || messages::USAGE_SKILL.get(lang).to_string();
    match args {
        [install] if install == "install" => Ok(Where::Agent(Agent::ClaudeCode)),
        [install, flag, value] if install == "install" => match (flag.as_str(), value.as_str()) {
            ("--dir", dir) if !dir.is_empty() => Ok(Where::Dir(PathBuf::from(dir))),
            ("--agent", "codex" | "pi") => Ok(Where::Agent(Agent::Generic)),
            ("--agent", name) => Agent::parse(name)
                .filter(|a| Agent::ALL.contains(a))
                .map(Where::Agent)
                .ok_or_else(usage),
            _ => Err(usage()),
        },
        _ => Err(usage()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::tests::Captured;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn prints_the_skill_file() {
        let mut c = Captured::new();
        assert_eq!(run(&mut c.output(true, Lang::Zh), &[]), 0);
        assert_eq!(c.stdout, skill::CONTENT.as_bytes(), "应原样输出 SKILL.md");
    }

    #[test]
    fn parses_install_arguments() {
        assert_eq!(
            parse_install(&args(&["install"]), Lang::Zh),
            Ok(Where::Agent(Agent::ClaudeCode))
        );
        assert_eq!(
            parse_install(&args(&["install", "--dir", "x"]), Lang::Zh),
            Ok(Where::Dir("x".into()))
        );
        for (value, want) in [
            ("claude", Agent::ClaudeCode),
            ("agents", Agent::Generic),
            ("workbuddy", Agent::WorkBuddy),
            ("codex", Agent::Generic),
            ("pi", Agent::Generic),
        ] {
            assert_eq!(
                parse_install(&args(&["install", "--agent", value]), Lang::Zh),
                Ok(Where::Agent(want)),
                "{value}"
            );
        }
        for bad in [
            &["foo"][..],
            &["install", "--dir"],
            &["install", "--dir", ""],
            &["install", "x"],
            &["install", "--agent", "cursor"],
        ] {
            assert!(parse_install(&args(bad), Lang::Zh).is_err(), "{bad:?}");
        }
    }

    // 只写入临时目录，不碰真实的 ~/.claude
    #[test]
    fn installs_and_replaces() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("skills");
        let target = root.join("termlet").join("SKILL.md");
        let install = args(&["install", "--dir", &root.display().to_string()]);

        let mut c = Captured::new();
        assert_eq!(run(&mut c.output(false, Lang::Zh), &install), 0);
        assert!(
            c.stdout().contains("已安装") && c.stdout().contains(&target.display().to_string())
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), skill::CONTENT);

        std::fs::write(&target, "old").unwrap();
        let mut c = Captured::new();
        assert_eq!(run(&mut c.output(true, Lang::Zh), &install), 0);
        let result: InstallResult = serde_json::from_slice(&c.stdout).unwrap();
        assert_eq!(
            result,
            InstallResult {
                path: target.display().to_string(),
                replaced: true
            }
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), skill::CONTENT);
    }

    #[test]
    fn install_failure_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("file");
        std::fs::write(&blocker, "").unwrap();
        let mut c = Captured::new();
        let code = run(
            &mut c.output(true, Lang::Zh),
            &args(&["install", "--dir", &blocker.display().to_string()]),
        );
        assert_eq!(code, 1);
        assert!(
            c.stdout().contains(r#""error":"skill_install_failed""#),
            "{}",
            c.stdout()
        );
    }
}
