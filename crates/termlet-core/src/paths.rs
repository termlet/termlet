//! 数据目录：位置、其中的文件，以及访问权限的收紧。
//!
//! 默认位置是系统的配置目录下的 `termlet`（Windows 为 `%APPDATA%\termlet`）；调试构建用 `termlet-dev`，
//! 开发时不碰正式数据；环境变量 `TERMLET_DATA_DIR` 可以指定任意位置。便携版（程序旁边有标记文件
//! `portable.txt`）的数据在程序旁边的 `data` 中，规则见 [`termlet_proto::endpoint::data_dir_for`]（命令行工具用同一个
//! 规则）。
//!
//! 访问权限（`TERMLET_DATA_DIR` 可以指向任意位置，不能依赖上级目录的权限）：
//! - `run/`（单实例锁、CLI 套接字）在启动时、获取单实例锁之前同步收紧，目录很小；
//! - 数据目录本身在后台收紧（[`DataDir::secure_in_background`]）：Windows 上可继承的权限要应用到其中全部文件，
//!   文件多时较慢，不阻塞启动；
//! - 只有 Termlet 自己的数据目录才整体收紧：位于公共位置、或其中有不是 Termlet 创建的文件时，
//!   只收紧数据库文件与 `logs`、`backups`、`session-logs`。
//!
//! 收紧失败一律只记警告：以前能启动的，不能因为改不了权限而起不来。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::platform;
use crate::{CoreError, Result};

/// 指定数据目录的环境变量。
pub const DATA_DIR_ENV: &str = "TERMLET_DATA_DIR";

/// 数据目录本身已收紧、且权限已应用到其中全部文件的标记（在 `run/` 中）。
/// 应用到一半时进程被结束就没有这个标记，下次启动再应用一次。
const PRIVATE_MARKER: &str = "data-dir-private";

/// Termlet 在数据目录中创建的子目录。
const TERMLET_DIRS: &[&str] = &["run", "logs", "backups", "session-logs", "updates"];
/// Termlet 在数据目录中创建的文件（按名称前缀：数据库与 SQLite 的附属文件、界面状态、远程配置的签发时间、
/// 使用统计的两个日期、加入 PATH 的命令行工具目录的记录、命令行的启动选项）。
const TERMLET_FILES: &[&str] = &[
    "data.db",
    "ui-state.json",
    "remote-config.json",
    "usage.json",
    crate::userpath::RECORD_FILE,
    termlet_proto::endpoint::LAUNCH_CONFIG,
];
/// 文件管理器自动生成的文件。
const OS_FILES: &[&str] = &["desktop.ini", "thumbs.db", ".ds_store"];

/// 数据目录。
#[derive(Clone, Debug)]
pub struct DataDir {
    root: PathBuf,
    /// 便携版：数据在程序旁边的 `data` 中
    portable: bool,
}

impl DataDir {
    /// 按 `TERMLET_DATA_DIR`、便携版的标记或默认位置确定数据目录（不创建）。
    pub fn resolve() -> Result<Self> {
        let exe = std::env::current_exe().ok();
        let resolved = termlet_proto::endpoint::data_dir_for(exe.as_deref().and_then(Path::parent))
            .map_err(|e| CoreError::DataDir(e.to_string()))?;
        Ok(Self {
            root: resolved.path,
            portable: resolved.portable,
        })
    }

    /// 使用指定的目录（测试、导入时用）。
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            portable: false,
        }
    }

    /// 标为便携版的数据目录（测试用）。
    pub fn into_portable(mut self) -> Self {
        self.portable = true;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 便携版：数据在程序旁边的 `data` 中（不改 PATH、不自动安装更新）。
    pub fn is_portable(&self) -> bool {
        self.portable
    }

    /// 试着在 `run/` 中写入再删除一个文件。便携版的数据在程序旁边：程序放在不能写的位置（例如 Program Files）时
    /// 启动前据此说明，而不是在别处悄悄建一份数据。
    pub fn probe_writable(&self) -> std::io::Result<()> {
        let probe = self.run_dir().join(".write-check");
        fs::write(&probe, b"")?;
        fs::remove_file(&probe)
    }

    /// 主数据库。
    pub fn database(&self) -> PathBuf {
        self.root.join("data.db")
    }

    /// 运行时文件：单实例锁、CLI 套接字。
    pub fn run_dir(&self) -> PathBuf {
        self.root.join("run")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// 本机快照与迁移前的备份。
    pub fn backups_dir(&self) -> PathBuf {
        self.root.join("backups")
    }

    /// 会话日志的默认目录。
    pub fn session_logs_dir(&self) -> PathBuf {
        self.root.join("session-logs")
    }

    /// 会话录制的默认目录。
    pub fn recordings_dir(&self) -> PathBuf {
        self.root.join("recordings")
    }

    /// 只与这台电脑有关的界面状态（窗口位置、侧边栏宽度、分组折叠）。
    pub fn ui_state(&self) -> PathBuf {
        self.root.join("ui-state.json")
    }

    /// 加入 PATH 的命令行工具目录的记录（见 [`crate::userpath`]）。
    pub fn cli_path_record(&self) -> PathBuf {
        self.root.join(crate::userpath::RECORD_FILE)
    }

    /// 自动更新下载的安装包（安装后下次启动时清理）。
    pub fn updates_dir(&self) -> PathBuf {
        self.root.join("updates")
    }

    /// 已采用过的远程配置中最新的签发时间（防止重放旧的远程配置）。
    pub fn remote_config_state(&self) -> PathBuf {
        self.root.join("remote-config.json")
    }

    /// 使用统计在本机记的两个 UTC 日期：第一次与上次发送成功（见 [`crate::usage`]；不在数据库中、不同步、不进备份）。
    pub fn usage_state(&self) -> PathBuf {
        self.root.join("usage.json")
    }

    /// 使用统计中还没发出的功能名单（见 [`crate::usage::features`]；不在数据库中、不同步、不进备份）。
    pub fn usage_features(&self) -> PathBuf {
        self.root.join("usage-features.json")
    }

    /// 创建数据目录与其中的 `run`、`logs`，并收紧 `run`。
    ///
    /// 目录无法创建时返回错误（Termlet 无法运行）；`run` 无法收紧时返回警告，由调用方记下后照常启动。
    pub fn prepare(&self) -> Result<Option<String>> {
        for dir in [self.run_dir(), self.logs_dir()] {
            fs::create_dir_all(&dir)
                .map_err(|e| CoreError::DataDir(format!("创建目录 {} 失败：{e}", dir.display())))?;
        }
        Ok(platform::restrict_dir(&self.run_dir())
            .err()
            .map(|e| format!("无法限制 {} 的访问权限：{e}", self.run_dir().display())))
    }

    /// 在后台线程收紧数据目录，结果写进日志。
    pub fn secure_in_background(&self) {
        let dir = self.clone();
        let _ = std::thread::Builder::new()
            .name("secure-data-dir".into())
            .spawn(move || dir.secure());
    }

    /// 收紧数据目录（见模块说明）。
    pub fn secure(&self) {
        let marker = self.run_dir().join(PRIVATE_MARKER);
        if marker.exists() && platform::is_private_dir(&self.root) {
            return;
        }
        if let Some(reason) = self.unrestrictable_reason() {
            tracing::warn!(dir = %self.root.display(), "数据目录{reason}，不整体限制其访问权限，只限制其中的数据库、日志与备份");
            self.secure_files();
            return;
        }
        // 先删除完成标记再应用：应用到一半被结束时，下次启动据此再应用一次
        if let Err(e) = fs::remove_file(&marker)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("删除数据目录权限的完成标记失败：{e}");
        }
        let began = Instant::now();
        if let Err(e) = platform::restrict_dir(&self.root) {
            tracing::warn!(dir = %self.root.display(), "无法限制数据目录的访问权限，改为只限制其中的数据库、日志与备份：{e}");
            self.secure_files();
            return;
        }
        if let Err(e) = fs::write(&marker, format!("{}\n", crate::time::now_ms())) {
            tracing::warn!("写入数据目录权限的完成标记失败：{e}");
        }
        tracing::info!(dir = %self.root.display(), elapsed_ms = began.elapsed().as_millis() as u64, "已限制数据目录的访问权限");
    }

    /// 数据目录不宜整体收紧的原因：位于公共位置，或其中有不是 Termlet 创建的文件（用户把已有的目录设为了数据目录）。
    fn unrestrictable_reason(&self) -> Option<String> {
        if platform::is_shared_location(&self.root) {
            return Some("位于公共位置（盘符根目录、用户主目录、文档、下载等）".into());
        }
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) => return Some(format!("无法读取（{e}）")),
        };
        for entry in entries.flatten() {
            if !is_termlet_entry(&entry) {
                return Some(format!(
                    "中有不是 Termlet 创建的文件（{}）",
                    entry.file_name().to_string_lossy()
                ));
            }
        }
        None
    }

    /// 不整体收紧时，只收紧数据库文件（含 SQLite 的附属文件）与 `logs`、`backups`、`session-logs`。
    fn secure_files(&self) {
        if let Ok(entries) = fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if entry.file_type().is_ok_and(|t| t.is_file()) && name.starts_with("data.db") {
                    let path = entry.path();
                    if let Err(e) = platform::restrict_file(&path) {
                        tracing::warn!(path = %path.display(), "无法限制文件的访问权限：{e}");
                    }
                }
            }
        }
        for dir in [
            self.logs_dir(),
            self.backups_dir(),
            self.session_logs_dir(),
            self.recordings_dir(),
        ] {
            // 不存在时不创建：backups、session-logs、recordings 由用到它们的功能创建并自行收紧
            if dir.is_dir()
                && let Err(e) = platform::restrict_dir(&dir)
            {
                tracing::warn!(dir = %dir.display(), "无法限制目录的访问权限：{e}");
            }
        }
    }
}

fn is_termlet_entry(entry: &fs::DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy().into_owned();
    if entry.file_type().is_ok_and(|t| t.is_dir()) {
        return TERMLET_DIRS.contains(&name.as_str());
    }
    if OS_FILES.contains(&name.to_lowercase().as_str()) {
        return true;
    }
    // 写到一半被结束时留下的临时文件（`platform::write_private_file` 的 `.<文件名>.<随机串>.tmp`）
    let name = match name.strip_prefix('.') {
        Some(rest) if rest.ends_with(".tmp") => rest,
        _ => name.as_str(),
    };
    TERMLET_FILES.iter().any(|prefix| name.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_creates_run_and_logs() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = DataDir::at(tmp.path().join("termlet"));
        assert_eq!(dir.prepare().unwrap(), None);
        assert!(dir.run_dir().is_dir());
        assert!(dir.logs_dir().is_dir());
        assert!(platform::is_private_dir(&dir.run_dir()));
    }

    // 能写的数据目录通过检查、不留下文件；写不进去时报错（这里让 run 是个文件），便携版启动前据此说明
    #[test]
    fn probe_writable_reports_unwritable_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = DataDir::at(tmp.path().join("data"));
        dir.prepare().unwrap();
        dir.probe_writable().unwrap();
        assert_eq!(fs::read_dir(dir.run_dir()).unwrap().count(), 0);
        let blocked = DataDir::at(tmp.path().join("blocked"));
        fs::create_dir_all(blocked.root()).unwrap();
        fs::write(blocked.run_dir(), b"").unwrap();
        assert!(blocked.probe_writable().is_err());
        assert!(!blocked.is_portable() && blocked.into_portable().is_portable());
    }

    #[test]
    fn secure_restricts_own_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = DataDir::at(tmp.path().join("termlet"));
        dir.prepare().unwrap();
        fs::write(dir.database(), b"").unwrap();
        dir.secure();
        assert!(platform::is_private_dir(dir.root()));
        assert!(dir.run_dir().join(PRIVATE_MARKER).exists());
    }

    // 命令行的启动选项（桌面端每次启动都写）、写到一半留下的临时文件也是 Termlet 的：照样整体收紧
    #[test]
    fn own_files_still_allow_restricting_whole_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = DataDir::at(tmp.path().join("termlet"));
        dir.prepare().unwrap();
        fs::write(dir.database(), b"").unwrap();
        termlet_proto::endpoint::Endpoint::for_data_dir(dir.root())
            .write_launch_config(&Default::default())
            .unwrap();
        assert!(
            dir.root()
                .join(termlet_proto::endpoint::LAUNCH_CONFIG)
                .is_file()
        );
        fs::write(dir.root().join(".ui-state.json.ab12c.tmp"), b"{").unwrap();
        assert_eq!(dir.unrestrictable_reason(), None);
        dir.secure();
        assert!(platform::is_private_dir(dir.root()));
        assert!(dir.run_dir().join(PRIVATE_MARKER).exists());
    }

    #[test]
    fn foreign_files_prevent_restricting_whole_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = DataDir::at(tmp.path().join("shared"));
        dir.prepare().unwrap();
        fs::write(dir.root().join("notes.txt"), b"user file").unwrap();
        assert!(dir.unrestrictable_reason().unwrap().contains("notes.txt"));
        // 以点开头、.tmp 结尾的只认 Termlet 自己的文件名
        fs::remove_file(dir.root().join("notes.txt")).unwrap();
        fs::write(dir.root().join(".notes.txt.ab12c.tmp"), b"x").unwrap();
        assert!(dir.unrestrictable_reason().unwrap().contains(".notes.txt"));
        dir.secure();
        assert!(!dir.run_dir().join(PRIVATE_MARKER).exists());
    }
}
