//! 本地终端：检测本机的 shell，把设置中自定义的 shell 与检测到的一起列出，并给出打开它们需要的
//! 启动参数（[`Launch`]）；WSL 发行版见 [`wsl`]。
//!
//! Windows 上检测 PowerShell 7、Windows PowerShell、命令提示符、Git Bash、MSYS2、Cygwin 与
//! Visual Studio 的开发者 PowerShell；Linux、macOS 上是 `$SHELL` 与 `/etc/shells` 中常见的 shell。
//!
//! 每次打开都重新读取用户的环境变量（Windows 从注册表读取系统与当前用户的变量并展开），而不是沿用
//! Termlet 启动时的：Termlet 常驻后台，用户之后新装的程序、改过的 PATH 不用重启 Termlet 就能生效，
//! 也不会把 Termlet 自己的变量带进 shell。
//!
//! 这里只给出启动描述，在伪终端中运行由终端模块负责（见 [`Launch`] 的说明）。
//! 检测与读取环境变量都是同步的（检测要读注册表、运行 vswhere，约几百毫秒），在后台线程调用。

mod cmdline;
mod env;
#[cfg(test)]
mod tests;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
pub mod wsl;

#[cfg(unix)]
use unix as imp;
#[cfg(windows)]
use windows as imp;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::model::LocalShell;

use env::Env;

/// shell 的种类：界面据此显示本地化的名称与图标。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ShellKind {
    /// PowerShell 7（`pwsh.exe`）
    Pwsh,
    /// Windows 自带的 Windows PowerShell 5.1
    PowerShell,
    /// 命令提示符
    Cmd,
    GitBash,
    Msys2,
    Cygwin,
    /// Visual Studio 的开发者 PowerShell
    VsDev,
    /// Linux、macOS 上检测到的 shell
    Unix,
    /// 设置中自定义的
    Custom,
}

impl ShellKind {
    /// 固定的英文标识。
    pub fn as_str(self) -> &'static str {
        match self {
            ShellKind::Pwsh => "pwsh",
            ShellKind::PowerShell => "powershell",
            ShellKind::Cmd => "cmd",
            ShellKind::GitBash => "gitbash",
            ShellKind::Msys2 => "msys2",
            ShellKind::Cygwin => "cygwin",
            ShellKind::VsDev => "vsdev",
            ShellKind::Unix => "unix",
            ShellKind::Custom => "custom",
        }
    }
}

/// 一个可以打开的本地终端。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    /// 检测到的 shell 的固定标识：`pwsh`、`powershell`、`cmd`、`gitbash`、`msys2`、`cygwin`、
    /// `vs-<实例 ID>`，Unix 上是程序名；自定义的为 `custom-<字母数字>`。
    /// 设置中按它记住默认终端与侧边栏中的项，发布后不能改。
    pub id: String,
    /// 显示名称（检测到的是英文名，界面按 [`ShellKind`] 本地化）
    pub name: String,
    pub kind: ShellKind,
    /// 程序的完整路径；自定义的 shell 在打开时才解析，这里是命令行中的第一项
    pub path: PathBuf,
    pub args: Vec<String>,
    /// 自定义 shell 的命令行
    pub command: String,
    /// 起始目录，为空时是用户主目录
    pub dir: String,
    /// 额外的环境变量（值中可以引用别的环境变量）
    pub env: BTreeMap<String, String>,
}

impl Profile {
    /// 设置中的自定义 shell。
    pub fn custom(shell: &LocalShell) -> Self {
        let (first, _) = cmdline::split_command(&shell.command);
        Self {
            id: shell.id.clone(),
            name: shell.name.clone(),
            kind: ShellKind::Custom,
            path: PathBuf::from(first),
            args: Vec::new(),
            command: shell.command.clone(),
            dir: shell.dir.clone(),
            env: shell.env.clone(),
        }
    }

    pub fn is_custom(&self) -> bool {
        self.kind == ShellKind::Custom
    }

    /// 生成启动描述：读取用户最新的环境变量，确定程序、参数与起始目录。
    /// 程序不存在（检测到之后被卸载、自定义的找不到）时返回 [`ShellError::ProgramNotFound`]。
    pub fn launch(&self) -> Result<Launch, ShellError> {
        launch_in(self, imp::user_env())
    }
}

/// 启动一个本地终端需要的全部参数。
///
/// 交给 `portable_pty::CommandBuilder` 时：
///
/// ```ignore
/// let mut cmd = CommandBuilder::from_argv(launch.argv());
/// cmd.env_clear(); // env 是完整的环境变量，不是在当前进程的变量上增减
/// for (key, value) in &launch.env {
///     cmd.env(key, value);
/// }
/// cmd.cwd(&launch.cwd);
/// ```
///
/// Windows 上的自定义 shell 另有 [`raw_args`](Self::raw_args)：程序后面的命令行原样交给程序
/// （Windows 的程序自己解析命令行）。`CommandBuilder` 会给每个参数重新加引号，不能原样传命令行，
/// 用它时只能用 [`args`](Self::args)（按 C 运行库的规则拆分的结果）：对大多数程序相同，
/// 对 cmd.exe 这类自己解析命令行的程序，引号嵌套的写法可能不同。能直接调用 `CreateProcessW` 时用
/// [`command_line`](Self::command_line)。
#[derive(Clone, PartialEq, Eq)]
pub struct Launch {
    /// 程序的完整路径
    pub program: PathBuf,
    /// 参数（不含程序本身）
    pub args: Vec<String>,
    /// 只有 Windows 上的自定义 shell 有：程序后面的命令行，原样保留（通常以空白开头）
    pub raw_args: Option<String>,
    /// 起始目录（已确认存在；设置的目录不存在时是用户主目录）
    pub cwd: PathBuf,
    /// 完整的环境变量
    pub env: Vec<(String, String)>,
}

impl Launch {
    /// 程序与参数（`CommandBuilder::from_argv` 的参数）。
    pub fn argv(&self) -> Vec<OsString> {
        std::iter::once(self.program.clone().into_os_string())
            .chain(self.args.iter().map(OsString::from))
            .collect()
    }

    /// Windows 上 `CreateProcessW` 的完整命令行：程序名（需要时加引号）加上原样的
    /// [`raw_args`](Self::raw_args)，没有时是逐个转义后的参数。
    pub fn command_line(&self) -> String {
        let program = self.program.to_string_lossy();
        match &self.raw_args {
            Some(raw) => cmdline::quote_program(&program) + raw,
            None => cmdline::compose_command_line(&program, &self.args),
        }
    }

    /// 取环境变量的值（名称按平台的规则比较；测试中检查启动描述）。
    #[cfg(test)]
    pub(crate) fn env_var(&self, key: &str) -> Option<&str> {
        self.env
            .iter()
            .rev()
            .find(|(k, _)| env::key_eq(k, key))
            .map(|(_, v)| v.as_str())
    }
}

impl fmt::Debug for Launch {
    // 环境变量中常有用户自己设置的密码、令牌：只输出名称
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Launch")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("raw_args", &self.raw_args)
            .field("cwd", &self.cwd)
            .field("env", &self.env.iter().map(|(k, _)| k).collect::<Vec<_>>())
            .finish()
    }
}

/// 本地终端与 WSL 的错误。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ShellError {
    /// 按 ID 找不到：已卸载、自定义的已删除
    #[error("找不到这个本地终端，可能已卸载或被删除")]
    UnknownShell,
    #[error("找不到要运行的程序")]
    ProgramNotFound,
    #[error("只有 Windows 支持 WSL")]
    WslUnsupported,
    #[error("这台电脑没有安装 WSL")]
    WslNotInstalled,
    #[error("WSL 发行版名称不合法")]
    InvalidDistro,
    #[error("WSL 用户名不合法")]
    InvalidUser,
}

/// 检测结果默认的缓存时间（侧边栏刷新时强制重新检测）。
pub const DETECT_MAX_AGE: Duration = Duration::from_secs(60);

type DetectFn = dyn Fn(&Env) -> Vec<Profile> + Send + Sync;

/// 检测本机的 shell 并缓存结果。
///
/// 检测期间不占着缓存的锁：界面线程上读缓存（[`Detector::cached`]、[`Detector::find_cached`]）不用等后台的检测。
pub struct Detector {
    max_age: Duration,
    /// 检测开始的时间与结果
    cache: Mutex<Option<(Instant, Vec<Profile>)>>,
    /// 正在检测：同时要求检测时只检测一次
    detecting: Mutex<()>,
    detect: Box<DetectFn>,
}

impl Default for Detector {
    fn default() -> Self {
        Self::new(DETECT_MAX_AGE)
    }
}

impl fmt::Debug for Detector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Detector")
            .field("max_age", &self.max_age)
            .finish_non_exhaustive()
    }
}

impl Detector {
    /// 创建检测器，结果缓存 `max_age`。
    pub fn new(max_age: Duration) -> Self {
        Self::with_detect(max_age, imp::detect)
    }

    fn with_detect(
        max_age: Duration,
        detect: impl Fn(&Env) -> Vec<Profile> + Send + Sync + 'static,
    ) -> Self {
        Self {
            max_age,
            cache: Mutex::new(None),
            detecting: Mutex::new(()),
            detect: Box::new(detect),
        }
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, Option<(Instant, Vec<Profile>)>> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 检测到的 shell；`refresh` 为 true 或缓存过期时重新检测（在后台线程调用）。同时调用时只检测一次：
    /// 等别的调用检测完，用它的结果。
    pub fn list(&self, refresh: bool) -> Vec<Profile> {
        let asked = Instant::now();
        let usable = |at: &Instant| *at >= asked || (!refresh && at.elapsed() <= self.max_age);
        if let Some((at, list)) = &*self.cache()
            && usable(at)
        {
            return list.clone();
        }
        let _detecting = self.detecting.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, list)) = &*self.cache()
            && usable(at)
        {
            return list.clone();
        }
        let started = Instant::now();
        let list = (self.detect)(&imp::user_env());
        tracing::debug!(
            refresh,
            count = list.len(),
            shells = ?list.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "检测本地 shell"
        );
        *self.cache() = Some((started, list.clone()));
        list
    }

    /// 缓存中的结果，过期了也用，不重新检测；还没有检测过时等正在进行的检测（启动时已在后台开始），
    /// 没有进行中的就检测一次。界面线程上用它，过期时另在后台调用 [`Detector::list`] 刷新。
    pub fn cached(&self) -> Vec<Profile> {
        if let Some((_, list)) = &*self.cache() {
            return list.clone();
        }
        self.list(false)
    }

    /// 缓存过期了（或还没有检测过）。
    pub fn is_stale(&self) -> bool {
        self.cache()
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() > self.max_age)
    }

    /// 与 [`Detector::find`] 相同，但只在缓存中找（见 [`Detector::cached`]），不重新检测：
    /// 程序已不在原来的位置时照样返回，打开时由 [`Detector::find`] 在后台重新找。
    pub fn find_cached(&self, id: &str, custom: &[LocalShell]) -> Result<Profile, ShellError> {
        if let Some(shell) = custom.iter().find(|s| s.id == id) {
            return Ok(Profile::custom(shell));
        }
        self.cached()
            .into_iter()
            .find(|p| p.id == id)
            .ok_or(ShellError::UnknownShell)
    }

    /// 按 ID 查找：先在自定义的 shell 中找，再在检测结果中找。缓存中没有、或程序已不在原来的位置
    /// （刚安装、升级后路径变了）时重新检测一次。
    pub fn find(&self, id: &str, custom: &[LocalShell]) -> Result<Profile, ShellError> {
        if let Some(shell) = custom.iter().find(|s| s.id == id) {
            return Ok(Profile::custom(shell));
        }
        for refresh in [false, true] {
            if let Some(p) = self
                .list(refresh)
                .into_iter()
                .find(|p| p.id == id && (refresh || exists(&p.path)))
            {
                return Ok(p);
            }
        }
        Err(ShellError::UnknownShell)
    }
}

/// 在给定的用户环境变量上生成启动描述。
fn launch_in(profile: &Profile, base: Env) -> Result<Launch, ShellError> {
    let env = build_env(&base, &profile.env);
    let (program, args, raw_args) = command_of(profile, &env)?;
    let cwd = start_dir(&profile.dir, &env);
    Ok(Launch {
        program,
        args,
        raw_args,
        cwd,
        env: env.into_vec(),
    })
}

/// 要运行的程序与参数：自定义的 shell 解析命令行，第一项在环境变量 PATH 中查找。
fn command_of(
    profile: &Profile,
    env: &Env,
) -> Result<(PathBuf, Vec<String>, Option<String>), ShellError> {
    if !profile.is_custom() {
        if !exists(&profile.path) {
            return Err(ShellError::ProgramNotFound);
        }
        return Ok((profile.path.clone(), profile.args.clone(), None));
    }
    let (first, rest) = cmdline::split_command(&profile.command);
    let path = imp::look_path(first, env).ok_or(ShellError::ProgramNotFound)?;
    let (args, raw_args) = imp::custom_args(rest);
    Ok((path, args, raw_args))
}

/// 起始目录：展开 `~` 与环境变量；为空或不存在时是用户主目录。
fn start_dir(dir: &str, env: &Env) -> PathBuf {
    let home = imp::home_of(env);
    let dir = env::expand(dir.trim(), env);
    if dir == "~" {
        return home;
    }
    let dir = match dir.strip_prefix('~') {
        Some(rest) if rest.starts_with(['/', MAIN_SEPARATOR]) => {
            home.join(rest.trim_start_matches(['/', MAIN_SEPARATOR]))
        }
        _ => PathBuf::from(dir),
    };
    if dir.as_os_str().is_empty() || !is_dir(&dir) {
        return home;
    }
    dir
}

/// 在用户的环境变量上加上终端相关的变量与额外的变量（同名的覆盖，名称按平台的规则比较）；
/// 额外变量的值中引用的环境变量按用户的环境变量展开。
fn build_env(base: &Env, extra: &BTreeMap<String, String>) -> Env {
    let mut add: Vec<(&str, &str)> = vec![
        // 告诉程序终端支持 24 位色，以及终端的名称（与 iTerm2、WezTerm 等相同的约定）
        ("COLORTERM", "truecolor"),
        ("TERM_PROGRAM", "Termlet"),
    ];
    if !imp::TERM.is_empty() {
        add.push(("TERM", imp::TERM));
    }
    for (key, value) in extra {
        // 环境变量的名称不能为空、不能含 `=`，名称与值都不能含 NUL
        if key.is_empty() || key.contains(['=', '\0']) || value.contains('\0') {
            continue;
        }
        add.retain(|(k, _)| !env::key_eq(k, key));
        add.push((key, value));
    }
    add.sort_by(|a, b| a.0.cmp(b.0));
    let mut out = Env::new();
    for (key, value) in base.iter() {
        if !key.is_empty() && add.iter().any(|(k, _)| env::key_eq(k, key)) {
            continue;
        }
        out.push(key, value);
    }
    for (key, value) in add {
        out.push(key, env::expand(value, base));
    }
    out
}

/// 路径是否存在。用 `symlink_metadata`：商店版 PowerShell 是应用执行别名（重解析点），`metadata` 读不了。
fn exists(path: &Path) -> bool {
    !path.as_os_str().is_empty() && std::fs::symlink_metadata(path).is_ok()
}

fn is_dir(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_dir())
}
