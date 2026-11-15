//! 本机通道的位置：每个数据目录一个（调试构建默认用 `termlet-dev`，与正式版互不干扰；便携版的数据在程序旁边，
//! 见 [`data_dir_for`]，与同时装着的安装版互不干扰）。
//!
//! - Windows：命名管道 `\\.\pipe\termlet-cli-<哈希>`，哈希取自数据目录的绝对路径（不区分大小写）。
//!   管道只允许当前用户与 SYSTEM 访问、属主是当前用户；客户端连接后核对属主，防止别的用户抢先占用这个名字。
//! - 其他平台：`{数据目录}/run/cli.sock`（`run/` 为 0700）。
//!
//! 服务没能启动时（例如数据目录的路径过长、位于网络共享上），应用在 `run/cli.unavailable` 中写下原因与进程号，
//! 命令行连不上时据此说明原因，而不是一直等到超时。
//!
//! Termlet 没有运行时命令行是否自动在后台启动它，由设置“命令行工具”中的选项决定；应用把它写进
//! `{数据目录}/cli-launch.json`（[`LaunchConfig`]），没有运行的应用也能让命令行读到。没有这个文件时按允许。

use std::path::{Path, PathBuf};

/// 指定数据目录的环境变量（与核心相同）。
pub const DATA_DIR_ENV: &str = "TERMLET_DATA_DIR";

/// 服务没能启动时的标记文件（在 `run/` 中）。
pub const UNAVAILABLE_MARKER: &str = "cli.unavailable";

/// 命令行的启动选项文件（在数据目录中，见 [`LaunchConfig`]）。
pub const LAUNCH_CONFIG: &str = "cli-launch.json";

/// 命令行的启动选项（应用按设置写下，命令行在 Termlet 没有运行时读取）。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LaunchConfig {
    /// Termlet 没有运行时，命令行自动以无窗口模式启动它
    pub auto_start: bool,
}

impl Default for LaunchConfig {
    fn default() -> Self {
        Self { auto_start: true }
    }
}

/// 找不到数据目录。
#[derive(Debug, PartialEq, Eq)]
pub enum DataDirError {
    /// `TERMLET_DATA_DIR` 不是有效的路径
    InvalidEnv(String),
    /// 找不到系统的配置目录
    NoConfigDir,
}

impl std::fmt::Display for DataDirError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DataDirError::InvalidEnv(e) => write!(f, "{DATA_DIR_ENV} 不是有效的路径（{e}）"),
            DataDirError::NoConfigDir => f.write_str("找不到系统的配置目录"),
        }
    }
}

impl std::error::Error for DataDirError {}

/// 便携版的标记文件：与 Termlet 程序（`Termlet.exe`）放在同一个文件夹中时，数据放在程序旁边的
/// [`PORTABLE_DATA_DIR`] 中（内容只是给人看的说明，不读）。
pub const PORTABLE_MARKER: &str = "portable.txt";

/// 便携版的数据目录（在程序旁边）。
pub const PORTABLE_DATA_DIR: &str = "data";

/// 找到的数据目录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedDataDir {
    pub path: PathBuf,
    /// 便携版：数据在程序旁边的 `data` 中
    pub portable: bool,
}

/// 数据目录：`TERMLET_DATA_DIR`；Termlet 程序所在的文件夹 `app_dir` 中有 [`PORTABLE_MARKER`] 时为它旁边的
/// `data`（便携版）；否则系统配置目录下的 `termlet`（调试构建为 `termlet-dev`）。应用传自己所在的文件夹，命令行工具
/// 传它找到的 Termlet 程序所在的文件夹（便携包中命令行工具在 `bin` 中，程序在上一级），两边得出同一个数据目录、
/// 同一个通道。与核心的 `DataDir::resolve` 规则相同（核心有测试核对二者一致）。
pub fn data_dir_for(app_dir: Option<&Path>) -> Result<ResolvedDataDir, DataDirError> {
    resolve_data_dir(std::env::var_os(DATA_DIR_ENV), app_dir, dirs::config_dir())
}

/// [`data_dir_for`] 的规则（环境变量与系统配置目录由调用方给出，测试用）。
fn resolve_data_dir(
    env: Option<std::ffi::OsString>,
    app_dir: Option<&Path>,
    config_dir: Option<PathBuf>,
) -> Result<ResolvedDataDir, DataDirError> {
    if let Some(dir) = env.filter(|v| !v.is_empty()) {
        let path = std::path::absolute(PathBuf::from(dir))
            .map_err(|e| DataDirError::InvalidEnv(e.to_string()))?;
        return Ok(ResolvedDataDir {
            path,
            portable: false,
        });
    }
    if let Some(path) = app_dir.and_then(portable_data_dir) {
        return Ok(ResolvedDataDir {
            path,
            portable: true,
        });
    }
    let base = config_dir.ok_or(DataDirError::NoConfigDir)?;
    Ok(ResolvedDataDir {
        path: base.join(if cfg!(debug_assertions) {
            "termlet-dev"
        } else {
            "termlet"
        }),
        portable: false,
    })
}

/// 程序所在的文件夹 `app_dir` 是便携版（其中有 [`PORTABLE_MARKER`]）时的数据目录：其中的 `data`。路径先规范化
/// （经目录联接、映射的盘符、短文件名打开时也是同一个），应用与命令行工具据此得出的通道名才一致。
pub fn portable_data_dir(app_dir: &Path) -> Option<PathBuf> {
    if !app_dir.join(PORTABLE_MARKER).is_file() {
        return None;
    }
    let dir = std::fs::canonicalize(app_dir)
        .map(|dir| without_verbatim_prefix(&dir))
        .unwrap_or_else(|_| app_dir.to_path_buf());
    Some(dir.join(PORTABLE_DATA_DIR))
}

/// 去掉 Windows 规范化路径的 `\\?\` 前缀（`\\?\UNC\server\share` 改回 `\\server\share`），给人看的路径与
/// 原来的写法一致。其他系统原样返回。
fn without_verbatim_prefix(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

/// 一个数据目录的通道。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    data_dir: PathBuf,
    #[cfg(windows)]
    pipe: String,
}

impl Endpoint {
    /// 数据目录 `data_dir` 的通道。
    pub fn for_data_dir(data_dir: &Path) -> Self {
        let data_dir = std::path::absolute(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
        Self {
            #[cfg(windows)]
            pipe: format!(r"\\.\pipe\termlet-cli-{:016x}", path_hash(&data_dir)),
            data_dir,
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// 命名管道的名字（`\\.\pipe\…`）。
    #[cfg(windows)]
    pub fn pipe_name(&self) -> &str {
        &self.pipe
    }

    /// Unix 套接字的路径。
    #[cfg(unix)]
    pub fn socket_path(&self) -> PathBuf {
        self.data_dir.join("run").join("cli.sock")
    }

    /// 写进日志、错误说明的位置。
    pub fn display(&self) -> String {
        #[cfg(windows)]
        {
            self.pipe.clone()
        }
        #[cfg(unix)]
        {
            self.socket_path().display().to_string()
        }
    }

    /// 服务没能启动时的标记文件。
    pub fn unavailable_marker(&self) -> PathBuf {
        self.data_dir.join("run").join(UNAVAILABLE_MARKER)
    }

    /// 命令行的启动选项文件。
    pub fn launch_config_path(&self) -> PathBuf {
        self.data_dir.join(LAUNCH_CONFIG)
    }

    /// 读取命令行的启动选项；没有文件或读不懂时按默认（允许自动启动）。
    pub fn launch_config(&self) -> LaunchConfig {
        std::fs::read_to_string(self.launch_config_path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// 写下命令行的启动选项（应用在启动时与设置改变时调用）；内容没变时不写。
    pub fn write_launch_config(&self, config: &LaunchConfig) -> std::io::Result<()> {
        let path = self.launch_config_path();
        let text = serde_json::to_string(config).map_err(std::io::Error::other)?;
        if std::fs::read_to_string(&path).ok().as_deref() == Some(text.as_str()) {
            return Ok(());
        }
        std::fs::write(path, text)
    }
}

/// 数据目录路径的哈希（FNV-1a 64 位）：Windows 上不区分大小写，去掉末尾的分隔符。不需要密码学强度：
/// 管道的访问由 DACL 控制，名字只用来区分数据目录。
#[cfg(windows)]
fn path_hash(path: &Path) -> u64 {
    let mut text = path.to_string_lossy().to_lowercase();
    while text.len() > 3 && (text.ends_with('\\') || text.ends_with('/')) {
        text.pop();
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 标记文件的内容：第一行是写下它的进程号，其余是原因。
pub fn unavailable_marker_text(pid: u32, reason: &str) -> String {
    format!("{pid}\n{reason}\n")
}

/// 解析标记文件：（进程号，原因）。
pub fn parse_unavailable_marker(text: &str) -> Option<(u32, String)> {
    let (pid, reason) = text.split_once('\n')?;
    let pid = pid.trim().parse().ok()?;
    let reason = reason.trim();
    Some((
        pid,
        if reason.is_empty() {
            "unknown".into()
        } else {
            reason.into()
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_config_defaults_to_auto_start_and_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::for_data_dir(tmp.path());
        assert!(endpoint.launch_config().auto_start, "没有文件时允许");
        endpoint
            .write_launch_config(&LaunchConfig { auto_start: false })
            .unwrap();
        assert!(!endpoint.launch_config().auto_start);
        std::fs::write(endpoint.launch_config_path(), "not json").unwrap();
        assert!(endpoint.launch_config().auto_start, "读不懂时按默认");
    }

    #[test]
    fn endpoint_depends_on_data_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let a = Endpoint::for_data_dir(&tmp.path().join("a"));
        let b = Endpoint::for_data_dir(&tmp.path().join("b"));
        assert_ne!(a.display(), b.display());
        assert_eq!(a, Endpoint::for_data_dir(&tmp.path().join("a")));
        assert_eq!(
            a.unavailable_marker(),
            tmp.path().join("a").join("run").join("cli.unavailable")
        );
    }

    #[cfg(windows)]
    #[test]
    fn pipe_name_ignores_case_and_trailing_separator() {
        let a = Endpoint::for_data_dir(Path::new(r"C:\Users\Ruan\AppData\Roaming\termlet"));
        let b = Endpoint::for_data_dir(Path::new(r"c:\users\ruan\appdata\roaming\TERMLET\"));
        assert_eq!(a.pipe_name(), b.pipe_name());
        assert!(a.pipe_name().starts_with(r"\\.\pipe\termlet-cli-"));
        assert_eq!(a.pipe_name().len(), r"\\.\pipe\termlet-cli-".len() + 16);
    }

    // 便携版：程序旁边有标记文件时数据在程序旁边的 data（与安装版的数据目录、通道都不同）；`TERMLET_DATA_DIR`
    // 优先；没有标记时用系统的配置目录
    #[test]
    fn portable_marker_moves_the_data_dir_next_to_the_app() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("Termlet");
        std::fs::create_dir_all(app.join(PORTABLE_MARKER)).unwrap();
        let config = tmp.path().join("config");
        let installed = resolve_data_dir(None, Some(&app), Some(config.clone())).unwrap();
        assert!(!installed.portable, "同名的文件夹不算标记");
        assert_eq!(installed.path.parent(), Some(config.as_path()));
        assert!(
            !resolve_data_dir(None, None, Some(config.clone()))
                .unwrap()
                .portable
        );

        std::fs::remove_dir(app.join(PORTABLE_MARKER)).unwrap();
        std::fs::write(app.join(PORTABLE_MARKER), "").unwrap();
        let portable = resolve_data_dir(None, Some(&app), Some(config.clone())).unwrap();
        assert!(portable.portable);
        assert!(portable.path.ends_with(PORTABLE_DATA_DIR));
        assert_eq!(
            std::fs::canonicalize(portable.path.parent().unwrap()).unwrap(),
            std::fs::canonicalize(&app).unwrap()
        );
        assert!(
            !portable.path.to_string_lossy().starts_with(r"\\?\"),
            "给人看的路径不带 \\\\?\\ 前缀"
        );
        assert_ne!(
            Endpoint::for_data_dir(&portable.path).display(),
            Endpoint::for_data_dir(&installed.path).display(),
            "与安装版的通道不同"
        );

        let chosen = tmp.path().join("chosen");
        let env = resolve_data_dir(Some(chosen.clone().into()), Some(&app), Some(config)).unwrap();
        assert_eq!(
            env,
            ResolvedDataDir {
                path: chosen,
                portable: false
            }
        );
    }

    #[test]
    fn verbatim_prefix_is_removed() {
        assert_eq!(
            without_verbatim_prefix(Path::new(r"\\?\C:\Tools\Termlet")),
            PathBuf::from(r"C:\Tools\Termlet")
        );
        assert_eq!(
            without_verbatim_prefix(Path::new(r"\\?\UNC\nas\share\Termlet")),
            PathBuf::from(r"\\nas\share\Termlet")
        );
        assert_eq!(
            without_verbatim_prefix(Path::new("/opt/termlet")),
            PathBuf::from("/opt/termlet")
        );
    }

    #[test]
    fn marker_text_round_trip() {
        let text = unavailable_marker_text(42, "bind: invalid argument");
        assert_eq!(
            parse_unavailable_marker(&text),
            Some((42, "bind: invalid argument".into()))
        );
        assert_eq!(parse_unavailable_marker("x\ny"), None);
        assert_eq!(parse_unavailable_marker("7\n"), Some((7, "unknown".into())));
    }
}
