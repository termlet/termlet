//! `termlet` 命令的 Agent Skill（SKILL.md）：随应用与命令行工具一起发布，可安装到通用目录 `~/.agents/skills`
//! （Agent Skills 约定，Codex、Gemini CLI、Cursor、GitHub Copilot、OpenCode、Qwen Code、Kimi、Pi 等都读取）、
//! Claude Code 与 WorkBuddy 的 skills 目录（`<skills 目录>/termlet/SKILL.md`）。
//!
//! 是否已安装按文件内容与内置版本逐字比较：相同为已安装，不同为有更新，不存在为未安装。
//! 界面（设置中的「AI 与自动化」）与 `termlet skill install` 共用这里。

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 内置的 SKILL.md。
pub const CONTENT: &str = include_str!("SKILL.md");

/// skill 的目录名。
pub const NAME: &str = "termlet";

/// 安装位置（通用目录或某个 AI 工具）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Agent {
    /// 通用目录 `~/.agents/skills`
    #[serde(rename = "agents")]
    Generic,
    #[serde(rename = "claude")]
    ClaudeCode,
    #[serde(rename = "workbuddy")]
    WorkBuddy,
    /// Codex 的旧位置 `$CODEX_HOME/skills`：Codex 已改为读取通用目录（旧位置标为弃用，仍兼容），
    /// 不再安装到这里，只列出以前装在这里的文件，供迁移到通用目录或删除
    #[serde(rename = "codex")]
    Codex,
}

impl Agent {
    /// 界面中按这个顺序列出可以安装的位置。
    pub const ALL: [Agent; 3] = [Agent::Generic, Agent::ClaudeCode, Agent::WorkBuddy];

    pub fn as_str(self) -> &'static str {
        match self {
            Agent::Generic => "agents",
            Agent::ClaudeCode => "claude",
            Agent::WorkBuddy => "workbuddy",
            Agent::Codex => "codex",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [
            Agent::Generic,
            Agent::ClaudeCode,
            Agent::WorkBuddy,
            Agent::Codex,
        ]
        .into_iter()
        .find(|a| a.as_str() == text)
    }
}

/// skill 在某个位置的安装状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Missing,
    Installed,
    /// 内容与内置版本不同（有更新）
    Outdated,
}

/// 一个安装位置与状态。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    pub agent: Agent,
    /// skills 根目录
    pub dir: PathBuf,
    /// SKILL.md 的完整路径
    pub path: PathBuf,
    pub status: Status,
    /// 在这台电脑上找到了对应的 AI 工具（配置目录存在）；没有找到时仍可安装（之后再装工具）
    pub detected: bool,
    /// 通用目录：检测到的、会读取它的 AI 工具
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readers: Vec<String>,
    /// 不再安装的旧位置（Codex），只能迁移或删除
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub legacy: bool,
}

/// 读取环境变量的方式（测试中替换）；空值视为没有设置。
pub type Getenv<'a> = &'a dyn Fn(&str) -> Option<String>;

/// 进程的环境变量。
pub fn system_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// AI 工具的配置目录：Claude Code 为 `$CLAUDE_CONFIG_DIR`（默认 `~/.claude`），Codex 为 `$CODEX_HOME`
/// （默认 `~/.codex`），WorkBuddy 为 `~/.workbuddy`，通用目录为 `~/.agents`。
pub fn config_dir(home: &Path, agent: Agent, getenv: Getenv<'_>) -> PathBuf {
    let from_env = |key: &str| getenv(key).filter(|v| !v.is_empty()).map(PathBuf::from);
    match agent {
        Agent::Generic => home.join(".agents"),
        Agent::ClaudeCode => from_env("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home.join(".claude")),
        Agent::WorkBuddy => home.join(".workbuddy"),
        Agent::Codex => from_env("CODEX_HOME").unwrap_or_else(|| home.join(".codex")),
    }
}

/// skills 根目录（配置目录下的 `skills`）。
pub fn dir(home: &Path, agent: Agent, getenv: Getenv<'_>) -> PathBuf {
    config_dir(home, agent, getenv).join("skills")
}

/// skills 根目录中 SKILL.md 的路径。
pub fn file(skills_dir: &Path) -> PathBuf {
    skills_dir.join(NAME).join("SKILL.md")
}

/// 按文件内容判断安装状态。
pub fn status_of(path: &Path) -> Status {
    match std::fs::read(path) {
        Err(_) => Status::Missing,
        Ok(data) if data == CONTENT.as_bytes() => Status::Installed,
        Ok(_) => Status::Outdated,
    }
}

/// 读取通用目录的 AI 工具与判断它已安装的配置目录（相对用户主目录）。
const GENERIC_READERS: &[(&str, &[&str])] = &[
    ("Codex", &[".codex"]),
    ("Gemini CLI", &[".gemini"]),
    ("Cursor", &[".cursor"]),
    ("GitHub Copilot", &[".copilot"]),
    ("OpenCode", &[".config", "opencode"]),
    ("Qwen Code", &[".qwen"]),
    ("Kimi", &[".kimi"]),
    ("Pi", &[".pi", "agent"]),
];

/// 这台电脑上检测到的、会读取通用目录的 AI 工具（Codex 按 `CODEX_HOME` 判断）。
fn readers(home: &Path, getenv: Getenv<'_>) -> Vec<String> {
    GENERIC_READERS
        .iter()
        .filter(|(name, parts)| {
            let dir = if *name == "Codex" {
                config_dir(home, Agent::Codex, getenv)
            } else {
                parts
                    .iter()
                    .fold(home.to_path_buf(), |p, part| p.join(part))
            };
            dir.exists()
        })
        .map(|(name, _)| name.to_string())
        .collect()
}

/// 各安装位置与状态；Codex 的旧位置只在其中装有 termlet skill 时列出。
pub fn targets(home: &Path, getenv: Getenv<'_>) -> Vec<Target> {
    let mut out = Vec::with_capacity(4);
    for agent in Agent::ALL.into_iter().chain([Agent::Codex]) {
        let config = config_dir(home, agent, getenv);
        let dir = config.join("skills");
        let path = file(&dir);
        let status = status_of(&path);
        let mut target = Target {
            agent,
            dir,
            path,
            status,
            detected: false,
            readers: Vec::new(),
            legacy: false,
        };
        match agent {
            Agent::Generic => {
                target.readers = readers(home, getenv);
                target.detected = !target.readers.is_empty();
            }
            Agent::Codex => {
                if status == Status::Missing {
                    continue;
                }
                target.legacy = true;
                target.detected = config.exists();
            }
            _ => target.detected = config.exists(),
        }
        out.push(target);
    }
    out
}

/// 把 SKILL.md 写到 `<skills_dir>/termlet/SKILL.md`，已存在时覆盖。返回写入的路径，以及原来是否已有文件。
pub fn install(skills_dir: &Path) -> io::Result<(PathBuf, bool)> {
    let path = file(&std::path::absolute(skills_dir)?);
    let existed = match std::fs::metadata(&path) {
        Ok(_) => true,
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => return Err(e),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_private(&path, CONTENT.as_bytes())?;
    Ok((path, existed))
}

/// 删除 `<skills_dir>/termlet/SKILL.md`，`termlet` 目录空了也一并删除；不存在时不报错。
/// 只删除这一个文件，目录中用户自己放的其他文件保留。
pub fn remove(skills_dir: &Path) -> io::Result<()> {
    let path = file(skills_dir);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // 目录不空（有用户自己的文件）时删除失败，保留目录
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(())
}

fn write_private(path: &Path, content: &[u8]) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    io::Write::write_all(&mut options.open(path)?, content)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn content_is_a_skill_file() {
        assert!(
            CONTENT.starts_with("---\nname: termlet\n"),
            "应以 frontmatter 开头"
        );
        for want in [
            "description:",
            "termlet exec",
            "termlet upload",
            "termlet download",
            "command_blocked",
            "truncated",
            "--stdin",
            "nohup",
            "cli_disabled",
        ] {
            assert!(CONTENT.contains(want), "SKILL.md 缺少 {want}");
        }
        assert!(!CONTENT.contains('\r'), "SKILL.md 应为 LF 换行");
    }

    #[test]
    fn dirs_follow_environment() {
        let home = Path::new("h").join("ruan");
        let want = [
            (Agent::Generic, home.join(".agents").join("skills")),
            (Agent::ClaudeCode, home.join(".claude").join("skills")),
            (Agent::WorkBuddy, home.join(".workbuddy").join("skills")),
            (Agent::Codex, home.join(".codex").join("skills")),
        ];
        for (agent, path) in &want {
            assert_eq!(&dir(&home, *agent, &no_env), path, "{agent:?}");
        }
        let env: HashMap<&str, String> = [
            ("CLAUDE_CONFIG_DIR", Path::new("x").join("claude")),
            ("CODEX_HOME", Path::new("x").join("codex")),
        ]
        .into_iter()
        .map(|(k, v)| (k, v.to_string_lossy().into_owned()))
        .collect();
        let getenv = |k: &str| env.get(k).cloned();
        assert_eq!(
            dir(&home, Agent::ClaudeCode, &getenv),
            Path::new("x").join("claude").join("skills")
        );
        assert_eq!(
            dir(&home, Agent::Codex, &getenv),
            Path::new("x").join("codex").join("skills")
        );
        assert_eq!(
            dir(&home, Agent::Generic, &getenv),
            want[0].1,
            "通用目录不受环境变量影响"
        );
        assert_eq!(Agent::parse("cursor"), None);
        assert_eq!(Agent::parse("claude"), Some(Agent::ClaudeCode));
    }

    #[test]
    fn targets_detect_installed_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let agents = |targets: &[Target]| targets.iter().map(|t| t.agent).collect::<Vec<_>>();
        let list = targets(home, &no_env);
        assert_eq!(agents(&list), Agent::ALL, "Codex 旧位置没有文件时不列出");
        for target in &list {
            assert!(
                target.status == Status::Missing && !target.detected && !target.legacy,
                "{target:?}"
            );
        }

        for parts in [
            &[".claude"][..],
            &[".codex"],
            &[".cursor"],
            &[".config", "opencode"],
            &[".pi", "agent"],
        ] {
            let dir = parts
                .iter()
                .fold(home.to_path_buf(), |p, part| p.join(part));
            std::fs::create_dir_all(dir).unwrap();
        }
        let list = targets(home, &no_env);
        assert!(list[0].detected);
        assert_eq!(list[0].readers, ["Codex", "Cursor", "OpenCode", "Pi"]);
        assert!(
            list[1].detected && !list[2].detected,
            "应检测到 Claude Code、未检测到 WorkBuddy"
        );

        // Codex 的旧位置装有 termlet skill 时列出，标为旧位置
        install(&dir(home, Agent::Codex, &no_env)).unwrap();
        let list = targets(home, &no_env);
        let last = list.last().unwrap();
        assert!(
            last.agent == Agent::Codex
                && last.legacy
                && last.status == Status::Installed
                && last.detected,
            "{last:?}"
        );
    }

    #[test]
    fn install_status_and_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let skills = dir(tmp.path(), Agent::Generic, &no_env);
        let (path, replaced) = install(&skills).unwrap();
        assert!(!replaced);
        assert_eq!(path, file(&skills));
        assert_eq!(status_of(&path), Status::Installed);

        // 内容与内置版本不同：有更新；再次安装覆盖
        std::fs::write(&path, "old").unwrap();
        assert_eq!(status_of(&path), Status::Outdated);
        let (_, replaced) = install(&skills).unwrap();
        assert!(replaced);
        assert_eq!(status_of(&path), Status::Installed);

        // 删除：只删 SKILL.md；termlet 目录里有用户自己的文件时保留目录
        let extra = path.parent().unwrap().join("notes.md");
        std::fs::write(&extra, "x").unwrap();
        remove(&skills).unwrap();
        assert_eq!(status_of(&path), Status::Missing);
        assert!(extra.exists(), "用户自己的文件应保留");
        std::fs::remove_file(&extra).unwrap();
        install(&skills).unwrap();
        remove(&skills).unwrap();
        assert!(
            !path.parent().unwrap().exists(),
            "termlet 目录空了应一并删除"
        );
        remove(&skills).unwrap();
    }

    #[test]
    fn install_fails_when_directory_cannot_be_created() {
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("file");
        std::fs::write(&blocker, "").unwrap();
        assert!(install(&blocker).is_err());
    }
}
