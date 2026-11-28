//! Windows：检测 shell、从注册表读取用户最新的环境变量、在 PATH 中查找程序。
//!
//! 检测的查找方式参考 Windows Terminal 与 Tabby：先查注册表，再查常见的安装位置，最后在 PATH 中查找。

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use windows_registry::{CURRENT_USER, Key, LOCAL_MACHINE, Type};
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

use super::cmdline;
use super::env::{self, Env, eq_ignore_case};
use super::{Profile, ShellKind, exists, is_dir};

/// 设置给 shell 的 TERM：Windows 的程序不看 TERM，不设置（Git Bash 等由 MSYS 运行时自己设置）。
pub(super) const TERM: &str = "";

/// 读注册表的 64 位视图（`KEY_WOW64_64KEY`）。
const KEY_WOW64_64KEY: u32 = 0x0100;

const APP_PATHS_PWSH: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\pwsh.exe";
const UNINSTALL: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";
const SYSTEM_ENV: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";

/// 按 PowerShell 7、Windows PowerShell、命令提示符、Git Bash、MSYS2、Cygwin、Visual Studio 的顺序检测。
pub(super) fn detect(env: &Env) -> Vec<Profile> {
    let sys = system_root(env).join("System32");
    let powershell = sys.join(r"WindowsPowerShell\v1.0\powershell.exe");
    let mut out: Vec<Profile> = Vec::new();
    let mut add = |p: Profile| {
        // 同一个程序只列一次（例如 PATH 中找到的就是 Program Files 下的那个）
        if exists(&p.path)
            && !out
                .iter()
                .any(|x| path_eq(&x.path, &p.path) && x.args == p.args)
        {
            out.push(p);
        }
    };

    if let Some((path, name)) = find_pwsh(env) {
        add(detected(
            "pwsh",
            &name,
            ShellKind::Pwsh,
            path,
            &["-NoLogo"],
            &[],
        ));
    }
    add(detected(
        "powershell",
        "Windows PowerShell",
        ShellKind::PowerShell,
        powershell.clone(),
        &["-NoLogo"],
        &[],
    ));
    let comspec = PathBuf::from(env.var("ComSpec"));
    let cmd = if exists(&comspec) {
        comspec
    } else {
        sys.join("cmd.exe")
    };
    add(detected(
        "cmd",
        "Command Prompt",
        ShellKind::Cmd,
        cmd,
        &[],
        &[],
    ));

    // Git Bash：bin\bash.exe 是 Git for Windows 的启动器，会设置好 PATH 等；不会找到
    // System32\bash.exe（那是 WSL 的启动程序）。CHERE_INVOKING 让登录 shell 留在起始目录
    if let Some(root) = find_git(env) {
        add(detected(
            "gitbash",
            "Git Bash",
            ShellKind::GitBash,
            root.join(r"bin\bash.exe"),
            &["-i", "-l"],
            &[("CHERE_INVOKING", "1")],
        ));
    }
    if let Some(root) = find_msys2(env) {
        // 优先 UCRT64（MSYS2 自 2022 年起推荐的环境），没有时用 MSYS
        let system = if is_dir(&root.join("ucrt64")) {
            "UCRT64"
        } else {
            "MSYS"
        };
        add(detected(
            "msys2",
            &format!("MSYS2 {system}"),
            ShellKind::Msys2,
            root.join(r"usr\bin\bash.exe"),
            &["--login", "-i"],
            &[("MSYSTEM", system), ("CHERE_INVOKING", "1")],
        ));
    }
    if let Some(root) = find_cygwin(env) {
        add(detected(
            "cygwin",
            "Cygwin",
            ShellKind::Cygwin,
            root.join(r"bin\bash.exe"),
            &["--login", "-i"],
            &[("CHERE_INVOKING", "1")],
        ));
    }
    for p in visual_studio(env, &powershell) {
        add(p);
    }
    out
}

fn detected(
    id: &str,
    name: &str,
    kind: ShellKind,
    path: PathBuf,
    args: &[&str],
    env: &[(&str, &str)],
) -> Profile {
    Profile {
        id: id.to_string(),
        name: name.to_string(),
        custom_name: None,
        kind,
        path,
        args: args.iter().map(|s| s.to_string()).collect(),
        command: String::new(),
        dir: String::new(),
        env: env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    }
}

fn path_eq(a: &Path, b: &Path) -> bool {
    eq_ignore_case(&a.to_string_lossy(), &b.to_string_lossy())
}

fn system_root(env: &Env) -> PathBuf {
    match env.var("SystemRoot") {
        "" => PathBuf::from(r"C:\Windows"),
        root => PathBuf::from(root),
    }
}

/// 系统盘（`C:`），用来拼 `C:\msys64` 这样的路径（不能用 `Path::join`：`C:` 加 `msys64` 是 `C:msys64`）。
fn system_drive(env: &Env) -> &str {
    match env.var("SystemDrive") {
        "" => "C:",
        drive => drive,
    }
}

fn open_key(root: &Key, path: &str) -> Option<Key> {
    root.options()
        .read()
        .access(KEY_WOW64_64KEY)
        .open(path)
        .ok()
}

/// 注册表中的字符串值（REG_SZ、REG_EXPAND_SZ，不展开）。
fn string_value(key: &Key, name: &str) -> Option<String> {
    let value = key.get_value(name).ok()?;
    matches!(value.ty(), Type::String | Type::ExpandString).then(|| wide_to_string(value.as_wide()))
}

/// UTF-16 转字符串，到第一个 NUL 为止。
fn wide_to_string(wide: &[u16]) -> String {
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..end])
}

/// 读取注册表字符串值（先 HKCU 后 HKLM），`name` 为空时读默认值；去掉两端的引号，为空时为 `None`。
fn reg_string(path: &str, name: &str) -> Option<String> {
    [CURRENT_USER, LOCAL_MACHINE].into_iter().find_map(|root| {
        let value = string_value(&open_key(root, path)?, name)?;
        let value = value.trim_matches('"');
        (!value.is_empty()).then(|| value.to_string())
    })
}

/// 查找 PowerShell 7（pwsh.exe）：Program Files\PowerShell\<版本>（安装版，取最高版本）→ 注册表 App Paths →
/// 商店版的应用执行别名 → PATH。返回路径与显示名称（能看出主版本时为 "PowerShell 7"）。
/// 商店版不用包目录（Program Files\WindowsApps\Microsoft.PowerShell_<版本>_…）中的路径：更新后旧目录就没有了，
/// 改用 %LOCALAPPDATA%\Microsoft\WindowsApps\pwsh.exe 这个固定的别名。
fn find_pwsh(env: &Env) -> Option<(PathBuf, String)> {
    let mut candidates = Vec::new();
    for pf in [env.var("ProgramW6432"), env.var("ProgramFiles")] {
        if pf.is_empty() {
            continue;
        }
        let base = Path::new(pf).join("PowerShell");
        let mut best: Option<(i64, PathBuf)> = None;
        for entry in std::fs::read_dir(&base).into_iter().flatten().flatten() {
            // 7、7-preview 之类：取主版本最高的，正式版优先于预览版
            let name = entry.file_name().to_string_lossy().into_owned();
            let (major, preview) = name.split_once('-').unwrap_or((name.as_str(), ""));
            let Ok(major) = major.parse::<i64>() else {
                continue;
            };
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let version = major.saturating_mul(2) + i64::from(preview.is_empty());
            let path = base.join(&name).join("pwsh.exe");
            if best.as_ref().is_none_or(|(v, _)| version > *v) && exists(&path) {
                best = Some((version, path));
            }
        }
        candidates.extend(best.map(|(_, path)| path));
    }
    let app_path = reg_string(APP_PATHS_PWSH, "");
    if let Some(p) = &app_path
        && !in_package(p)
    {
        candidates.push(PathBuf::from(p));
    }
    let local = env.var("LOCALAPPDATA");
    if !local.is_empty() {
        candidates.push(Path::new(local).join(r"Microsoft\WindowsApps\pwsh.exe"));
    }
    if let Some(p) = look_path("pwsh.exe", env)
        && !in_package(&p.to_string_lossy())
    {
        candidates.push(p);
    }
    let path = candidates.into_iter().find(|p| exists(p))?;
    let name = pwsh_name(&path, app_path.as_deref().unwrap_or(""));
    Some((path, name))
}

/// 路径是否在商店应用的包目录（Program Files\WindowsApps）中。
fn in_package(path: &str) -> bool {
    path.to_lowercase().contains(r"\program files\windowsapps\")
}

/// 显示名称：...\PowerShell\7\pwsh.exe 或商店版（包目录 Microsoft.PowerShell_7.x_…）为 "PowerShell 7"，
/// 看不出版本时为 "PowerShell"。`app_path` 是注册表 App Paths 中登记的路径，用于推断商店版别名的版本。
fn pwsh_name(path: &Path, app_path: &str) -> String {
    let dir_name = |p: &Path| {
        p.parent()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let is_number = |s: &str| !s.is_empty() && s.parse::<i64>().is_ok();
    let dir = dir_name(path);
    let major = dir.split('-').next().unwrap_or_default();
    if is_number(major) {
        return format!("PowerShell {major}");
    }
    for p in [path, Path::new(app_path)] {
        if let Some(rest) = dir_name(p).strip_prefix("Microsoft.PowerShell_") {
            let major = rest.split('.').next().unwrap_or_default();
            if is_number(major) {
                return format!("PowerShell {major}");
            }
        }
    }
    "PowerShell".to_string()
}

/// 查找 Git for Windows 的安装目录：注册表 → Program Files → 按用户安装的位置。
fn find_git(env: &Env) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = reg_string(r"SOFTWARE\GitForWindows", "InstallPath")
        .map(PathBuf::from)
        .into_iter()
        .collect();
    for key in ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"] {
        let pf = env.var(key);
        if !pf.is_empty() {
            candidates.push(Path::new(pf).join("Git"));
        }
    }
    let local = env.var("LOCALAPPDATA");
    if !local.is_empty() {
        candidates.push(Path::new(local).join(r"Programs\Git"));
    }
    candidates
        .into_iter()
        .find(|root| exists(&root.join(r"bin\bash.exe")))
}

/// 查找 MSYS2：卸载信息中登记的安装位置 → 系统盘的 msys64。
fn find_msys2(env: &Env) -> Option<PathBuf> {
    let mut candidates = uninstall_locations("MSYS2");
    candidates.push(PathBuf::from(format!(r"{}\msys64", system_drive(env))));
    candidates
        .into_iter()
        .find(|root| exists(&root.join(r"usr\bin\bash.exe")))
}

/// 查找 Cygwin：注册表 → 系统盘的 cygwin64、cygwin。
fn find_cygwin(env: &Env) -> Option<PathBuf> {
    let drive = system_drive(env);
    reg_string(r"SOFTWARE\Cygwin\setup", "rootdir")
        .map(PathBuf::from)
        .into_iter()
        .chain([
            PathBuf::from(format!(r"{drive}\cygwin64")),
            PathBuf::from(format!(r"{drive}\cygwin")),
        ])
        .find(|root| exists(&root.join(r"bin\bash.exe")))
}

/// 卸载信息中显示名称以 `prefix` 开头的软件的安装位置。
fn uninstall_locations(prefix: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in [CURRENT_USER, LOCAL_MACHINE] {
        let Some(key) = open_key(root, UNINSTALL) else {
            continue;
        };
        let Ok(names) = key.keys() else {
            continue;
        };
        for name in names {
            let Some(sub) = open_key(&key, &name) else {
                continue;
            };
            let display = string_value(&sub, "DisplayName").unwrap_or_default();
            let location = string_value(&sub, "InstallLocation").unwrap_or_default();
            let location = location.trim_matches('"');
            if display.starts_with(prefix) && !location.is_empty() {
                out.push(PathBuf::from(location));
            }
        }
    }
    out
}

/// vswhere 列出的一个 Visual Studio 实例。
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct VsInstance {
    instance_id: String,
    installation_path: String,
    display_name: String,
    catalog: VsCatalog,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct VsCatalog {
    product_line_version: String,
}

/// 用 vswhere 列出 Visual Studio（含生成工具），为每个实例生成"开发者 PowerShell"。
fn visual_studio(env: &Env, powershell: &Path) -> Vec<Profile> {
    let pf = env.var("ProgramFiles(x86)");
    if pf.is_empty() {
        return Vec::new();
    }
    let vswhere = Path::new(pf).join(r"Microsoft Visual Studio\Installer\vswhere.exe");
    if !exists(&vswhere) {
        return Vec::new();
    }
    let args = [
        "-all",
        "-prerelease",
        "-products",
        "*",
        "-format",
        "json",
        "-utf8",
    ];
    let Some(out) = run_hidden(&vswhere, &args, Duration::from_secs(5)) else {
        return Vec::new();
    };
    let json = out.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&out);
    match serde_json::from_slice::<Vec<VsInstance>>(json) {
        Ok(list) => vs_profiles(&list, powershell, exists),
        Err(e) => {
            tracing::debug!("解析 vswhere 的输出失败：{e}");
            Vec::new()
        }
    }
}

/// 每个装有 DevShell 模块的实例一个"开发者 PowerShell"（与 Windows Terminal 相同：在 Windows PowerShell
/// 中导入 DevShell 模块，`-SkipAutomaticLocation` 保持起始目录）。
fn vs_profiles(
    list: &[VsInstance],
    powershell: &Path,
    has_module: impl Fn(&Path) -> bool,
) -> Vec<Profile> {
    let mut versions: HashMap<&str, usize> = HashMap::new();
    for vs in list {
        *versions
            .entry(vs.catalog.product_line_version.as_str())
            .or_default() += 1;
    }
    let mut out = Vec::new();
    for vs in list {
        let dll = Path::new(&vs.installation_path)
            .join(r"Common7\Tools\Microsoft.VisualStudio.DevShell.dll");
        // 实例 ID 拼进 PowerShell 命令与本地终端的 ID：只接受字母数字（vswhere 给出的是 8 位十六进制）
        if vs.instance_id.is_empty()
            || !vs.instance_id.chars().all(|c| c.is_ascii_alphanumeric())
            || !has_module(&dll)
        {
            continue;
        }
        let version = vs.catalog.product_line_version.as_str();
        // 同一版本装了多个（例如社区版与生成工具）时用完整名称区分
        let name = if version.is_empty() || versions[version] > 1 {
            format!("Developer PowerShell ({})", vs.display_name)
        } else {
            format!("Developer PowerShell for VS {version}")
        };
        // PowerShell 单引号字符串中的单引号写两次
        let script = format!(
            "&{{Import-Module '{}'; Enter-VsDevShell {} -SkipAutomaticLocation}}",
            dll.to_string_lossy().replace('\'', "''"),
            vs.instance_id
        );
        out.push(Profile {
            id: format!("vs-{}", vs.instance_id),
            name,
            custom_name: None,
            kind: ShellKind::VsDev,
            path: powershell.to_path_buf(),
            args: vec![
                "-NoLogo".into(),
                "-NoExit".into(),
                "-Command".into(),
                script,
            ],
            command: String::new(),
            dir: String::new(),
            env: Default::default(),
        });
    }
    out
}

/// 从 Termlet 进程中取的变量：注册表的环境变量键中没有、登录期间不变的那些
/// （其余的系统变量、用户变量都从注册表读最新的）。
const PROCESS_VARS: &[&str] = &[
    "SystemRoot",
    "SystemDrive",
    "windir",
    "ALLUSERSPROFILE",
    "ProgramData",
    "PUBLIC",
    "COMPUTERNAME",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramW6432",
    "CommonProgramFiles",
    "CommonProgramFiles(x86)",
    "CommonProgramW6432",
    "USERPROFILE",
    "USERNAME",
    "USERDOMAIN",
    "USERDOMAIN_ROAMINGPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "LOGONSERVER",
];

/// 用户变量中追加到系统变量之后（而不是覆盖）的路径列表。
const PATH_VARS: &[&str] = &["Path", "LibPath", "Os2LibPath"];

/// 当前用户的环境变量，与资源管理器启动程序时相同：系统变量 → 用户变量 → 登录会话的变量，取自注册表中
/// 最新的值；不继承 Termlet 自己的。读取失败时退回 Termlet 进程的环境变量。
pub(super) fn user_env() -> Env {
    registry_env().unwrap_or_else(|| {
        tracing::warn!("读取注册表中的环境变量失败，改用 Termlet 进程的环境变量");
        std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .collect()
    })
}

fn registry_env() -> Option<Env> {
    let mut env: Env = PROCESS_VARS
        .iter()
        .filter_map(|&k| Some((k, std::env::var(k).ok()?)))
        .collect();
    let system = open_key(LOCAL_MACHINE, SYSTEM_ENV)?;
    // 系统变量中的 USERNAME 是 SYSTEM，不用
    apply_values(&mut env, &read_values(&system), &["USERNAME"]);
    let mut user_keys = vec![
        "Environment".to_string(),
        "Volatile Environment".to_string(),
    ];
    if let Some(session) = session_id() {
        user_keys.push(format!(r"Volatile Environment\{session}"));
    }
    for path in user_keys {
        if let Some(key) = open_key(CURRENT_USER, &path) {
            apply_values(&mut env, &read_values(&key), &[]);
        }
    }
    Some(env)
}

/// 注册表中环境变量的类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegKind {
    /// REG_SZ：原样使用
    Plain,
    /// REG_EXPAND_SZ：展开其中的 %NAME%
    Expand,
}

fn read_values(key: &Key) -> Vec<(String, RegKind, String)> {
    let Ok(values) = key.values() else {
        return Vec::new();
    };
    values
        .filter_map(|(name, value)| {
            let kind = match value.ty() {
                Type::String => RegKind::Plain,
                Type::ExpandString => RegKind::Expand,
                _ => return None,
            };
            Some((name, kind, wide_to_string(value.as_wide())))
        })
        .collect()
}

/// 把一个注册表键中的变量加到 `env` 中：先是 REG_SZ，再展开 REG_EXPAND_SZ（与 Windows 相同，
/// 展开时能引用同一个键中的 REG_SZ 变量）；路径列表追加到已有的值之后；值为空的去掉。
fn apply_values(env: &mut Env, values: &[(String, RegKind, String)], skip: &[&str]) {
    for pass in [RegKind::Plain, RegKind::Expand] {
        for (name, kind, value) in values {
            if *kind != pass
                || name.is_empty()
                || name.contains(['=', '\0'])
                || skip.iter().any(|s| eq_ignore_case(s, name))
            {
                continue;
            }
            let value = match pass {
                RegKind::Plain => value.clone(),
                RegKind::Expand => env::expand_windows(value, env),
            };
            if PATH_VARS.iter().any(|p| eq_ignore_case(p, name))
                && let Some(old) = env.get(name).filter(|old| !old.is_empty())
            {
                if !value.is_empty() {
                    let sep = if old.ends_with(';') { "" } else { ";" };
                    let joined = format!("{old}{sep}{value}");
                    env.set(name, joined);
                }
            } else if value.is_empty() {
                env.remove(name);
            } else {
                env.set(name, value);
            }
        }
    }
}

/// 当前进程所在的登录会话编号（`Volatile Environment\<编号>` 中是这个会话的变量）。
fn session_id() -> Option<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TokenSessionId};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: 标准的 Win32 调用序列；TokenSessionId 的结果是一个 u32，缓冲区大小固定
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let mut id = 0u32;
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenSessionId,
            (&mut id as *mut u32).cast(),
            size_of::<u32>() as u32,
            &mut len,
        );
        CloseHandle(token);
        (ok != 0).then_some(id)
    }
}

/// 用户主目录。
pub(super) fn home_of(env: &Env) -> PathBuf {
    match env.var("USERPROFILE") {
        "" => dirs::home_dir().unwrap_or_default(),
        home => PathBuf::from(home),
    }
}

/// 可以直接运行的扩展名：批处理（.bat、.cmd）需要经 cmd /c，作为终端的 shell 不支持。
const EXECUTABLE_EXTS: &[&str] = &["exe", "com"];

/// 查找要运行的程序：含路径时直接检查；只有文件名时在 `env` 的 PATH 中查找（用户最新的 PATH，
/// 不是 Termlet 启动时的），没有扩展名时依次尝试 .exe、.com。返回绝对路径。
pub(super) fn look_path(name: &str, env: &Env) -> Option<PathBuf> {
    let name = env::expand_windows(name, env);
    if name.is_empty() {
        return None;
    }
    let candidates = |base: PathBuf| -> Vec<PathBuf> {
        let ext = base
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase());
        if ext.is_some_and(|ext| EXECUTABLE_EXTS.contains(&ext.as_str())) {
            return vec![base];
        }
        EXECUTABLE_EXTS
            .iter()
            .map(|ext| {
                let mut p = OsString::from(base.as_os_str());
                p.push(".");
                p.push(ext);
                PathBuf::from(p)
            })
            .collect()
    };
    let runnable = |p: &Path| exists(p) && !is_dir(p);
    let absolute = |p: PathBuf| std::path::absolute(&p).unwrap_or(p);
    let path = Path::new(&name);
    if path.is_absolute() || name.contains(['\\', '/']) {
        return candidates(path.to_path_buf())
            .into_iter()
            .find(|c| runnable(c))
            .map(absolute);
    }
    for dir in std::env::split_paths(env.var("PATH")) {
        let dir = dir.to_string_lossy();
        let dir = dir.trim_matches('"');
        if dir.is_empty() {
            continue;
        }
        if let Some(found) = candidates(Path::new(dir).join(&name))
            .into_iter()
            .find(|c| runnable(c))
        {
            return Some(absolute(found));
        }
    }
    None
}

/// 自定义 shell 的参数：其余部分原样交给程序（Windows 的程序自己解析命令行），另按 C 运行库的规则拆一份。
pub(super) fn custom_args(rest: &str) -> (Vec<String>, Option<String>) {
    (cmdline::split_windows_args(rest), Some(rest.to_string()))
}

/// 参数写成一行（[`custom_args`] 能原样拆回）。
pub(super) fn join_args(args: &[String]) -> String {
    cmdline::join_windows_args(args)
}

/// 运行一个命令行程序（vswhere、wsl.exe）并取它的标准输出，不弹出控制台窗口：
/// 超时、启动失败或退出码不为 0 时为 `None`。
pub(super) fn run_hidden(program: &Path, args: &[&str], timeout: Duration) -> Option<Vec<u8>> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            tracing::debug!("运行 {} 失败：{e}", program.display());
            return None;
        }
    };
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    // 在另一个线程读输出：输出多时子进程要等管道被读走才能退出
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    let deadline = Instant::now() + timeout;
    let output = rx.recv_timeout(timeout).ok();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return output.filter(|_| status.success()),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => break,
        }
    }
    tracing::debug!("{} 超时未退出，已结束", program.display());
    let _ = child.kill();
    let _ = child.wait();
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(list: &[(&str, RegKind, &str)]) -> Vec<(String, RegKind, String)> {
        list.iter()
            .map(|&(k, kind, v)| (k.to_string(), kind, v.to_string()))
            .collect()
    }

    // 注册表的合并规则：REG_SZ 先于 REG_EXPAND_SZ（展开时能引用同一个键中的变量），
    // 用户的 Path 追加到系统的之后，系统变量中的 USERNAME 不用，值为空的去掉
    #[test]
    fn registry_values_merge_like_windows() {
        let mut env: Env = [
            ("USERPROFILE", r"C:\Users\ruan"),
            ("SystemRoot", r"C:\Windows"),
            ("USERNAME", "ruan"),
        ]
        .into_iter()
        .collect();
        let system = values(&[
            (
                "Path",
                RegKind::Expand,
                r"%JAVA_HOME%\bin;%SystemRoot%\system32",
            ),
            ("JAVA_HOME", RegKind::Plain, r"C:\jdk"),
            ("TEMP", RegKind::Expand, r"%SystemRoot%\TEMP"),
            ("USERNAME", RegKind::Plain, "SYSTEM"),
            ("LITERAL", RegKind::Plain, "%SystemRoot%"),
        ]);
        apply_values(&mut env, &system, &["USERNAME"]);
        let user = values(&[
            ("TEMP", RegKind::Expand, r"%USERPROFILE%\AppData\Local\Temp"),
            ("PATH", RegKind::Plain, r"C:\Users\ruan\bin"),
            ("CLIENTNAME", RegKind::Plain, ""),
            ("JAVA_HOME", RegKind::Plain, ""),
        ]);
        apply_values(&mut env, &user, &[]);
        assert_eq!(
            env.get("path"),
            Some(r"C:\jdk\bin;C:\Windows\system32;C:\Users\ruan\bin")
        );
        assert_eq!(env.get("TEMP"), Some(r"C:\Users\ruan\AppData\Local\Temp"));
        assert_eq!(env.get("USERNAME"), Some("ruan"));
        assert_eq!(env.get("LITERAL"), Some("%SystemRoot%"));
        assert!(!env.contains("CLIENTNAME") && !env.contains("JAVA_HOME"));
    }

    // 本机：取的是注册表中最新的变量，不继承 Termlet 进程自己的（cargo 给测试进程设置的变量不在注册表中）
    #[test]
    fn user_env_reads_registry_not_process() {
        let env = user_env();
        for key in ["Path", "SystemRoot", "USERPROFILE", "ComSpec", "TEMP"] {
            assert!(env.get(key).is_some_and(|v| !v.is_empty()), "应有 {key}");
        }
        assert!(
            env.var("PATH").to_lowercase().contains(r"\system32"),
            "PATH 中应有系统目录"
        );
        if std::env::var_os("CARGO_MANIFEST_DIR").is_some() {
            assert!(
                !env.contains("CARGO_MANIFEST_DIR"),
                "不应继承进程自己的变量"
            );
        }
        assert!(!env.var("TEMP").contains('%'), "REG_EXPAND_SZ 应已展开");
    }

    #[test]
    fn look_path_finds_executables_only() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let exe = dir.join("tool.exe");
        std::fs::write(&exe, b"MZ").unwrap();
        std::fs::write(dir.join("script.cmd"), b"@echo").unwrap();
        std::fs::create_dir(dir.join("folder.exe")).unwrap();
        let env: Env = [("PATH", format!(r#"C:\nope;"{}""#, dir.display()))]
            .into_iter()
            .collect();
        let found = look_path("tool", &env).expect("应在 PATH 中找到 tool.exe");
        assert!(path_eq(&found, &exe), "{}", found.display());
        assert_eq!(
            look_path(&exe.to_string_lossy(), &Env::new()),
            Some(exe.clone())
        );
        assert_eq!(look_path("script", &env), None, "批处理不能直接作为 shell");
        assert_eq!(look_path("folder", &env), None, "目录不是程序");
        assert_eq!(look_path("missing.exe", &env), None);
        assert_eq!(look_path("", &env), None);
        let with_var: Env = [("TOOLS", dir.to_string_lossy().into_owned())]
            .into_iter()
            .collect();
        assert_eq!(look_path(r"%TOOLS%\tool", &with_var), Some(exe));
    }

    #[test]
    fn pwsh_display_names() {
        let cases = [
            (
                r"C:\Program Files\PowerShell\7\pwsh.exe",
                "",
                "PowerShell 7",
            ),
            (
                r"C:\Program Files\PowerShell\7-preview\pwsh.exe",
                "",
                "PowerShell 7",
            ),
            (
                r"C:\Users\ruan\AppData\Local\Microsoft\WindowsApps\pwsh.exe",
                r"C:\Program Files\WindowsApps\Microsoft.PowerShell_7.4.6.0_x64__8wekyb3d8bbwe\pwsh.exe",
                "PowerShell 7",
            ),
            (r"C:\tools\pwsh.exe", "", "PowerShell"),
            ("pwsh.exe", "", "PowerShell"),
        ];
        for (path, app_path, want) in cases {
            assert_eq!(pwsh_name(Path::new(path), app_path), want, "{path}");
        }
        assert!(in_package(
            r"C:\Program Files\WindowsApps\Microsoft.PowerShell_7.4.6.0_x64__8wekyb3d8bbwe\pwsh.exe"
        ));
        assert!(!in_package(
            r"C:\Users\ruan\AppData\Local\Microsoft\WindowsApps\pwsh.exe"
        ));
    }

    #[test]
    fn vs_profiles_from_vswhere_json() {
        let json = r#"[
            {"instanceId": "a1b2c3d4", "installationPath": "C:\\VS\\2022\\Community",
             "displayName": "Visual Studio Community 2022", "catalog": {"productLineVersion": "2022"}},
            {"instanceId": "e5f6a7b8", "installationPath": "C:\\VS\\2019\\BuildTools",
             "displayName": "Visual Studio Build Tools 2019", "catalog": {"productLineVersion": "2019"}},
            {"instanceId": "c9d0e1f2", "installationPath": "C:\\VS\\2019\\Pro's",
             "displayName": "Visual Studio Professional 2019", "catalog": {"productLineVersion": "2019"}},
            {"instanceId": "bad; rm", "installationPath": "C:\\VS\\x",
             "displayName": "x", "catalog": {"productLineVersion": "2017"}},
            {"instanceId": "00000000", "installationPath": "C:\\VS\\NoModule",
             "displayName": "y", "catalog": {"productLineVersion": "2017"}},
            {"unknownField": 1}
        ]"#;
        let list: Vec<VsInstance> = serde_json::from_str(json).unwrap();
        let powershell = Path::new(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe");
        let profiles = vs_profiles(&list, powershell, |dll| !dll.starts_with(r"C:\VS\NoModule"));
        let names: Vec<_> = profiles
            .iter()
            .map(|p| (p.id.as_str(), p.name.as_str()))
            .collect();
        assert_eq!(
            names,
            [
                ("vs-a1b2c3d4", "Developer PowerShell for VS 2022"),
                (
                    "vs-e5f6a7b8",
                    "Developer PowerShell (Visual Studio Build Tools 2019)"
                ),
                (
                    "vs-c9d0e1f2",
                    "Developer PowerShell (Visual Studio Professional 2019)"
                ),
            ]
        );
        assert_eq!(profiles[0].path, powershell);
        assert_eq!(
            profiles[2].args,
            [
                "-NoLogo",
                "-NoExit",
                "-Command",
                r"&{Import-Module 'C:\VS\2019\Pro''s\Common7\Tools\Microsoft.VisualStudio.DevShell.dll'; Enter-VsDevShell c9d0e1f2 -SkipAutomaticLocation}",
            ]
        );
    }

    #[test]
    fn system_paths_use_drive_and_root() {
        let env: Env = [("SystemDrive", "D:")].into_iter().collect();
        assert_eq!(system_drive(&env), "D:");
        assert_eq!(system_drive(&Env::new()), "C:");
        assert_eq!(system_root(&Env::new()), PathBuf::from(r"C:\Windows"));
    }
}
