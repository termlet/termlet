//! 连接 Termlet：连不上时以 headless 模式拉起它，等命令行服务就绪（最多 10 秒）。设置“命令行工具”中关闭了
//! “Termlet 没有打开时自动在后台启动”时不拉起，提示先打开 Termlet（`app_not_running`，见 `endpoint::LaunchConfig`）。
//!
//! 找 Termlet 程序的顺序：环境变量 `TERMLET_APP`（为空表示不拉起）；安装目录（命令行工具在 `bin\termlet.exe`，
//! 程序在上一级的 `Termlet.exe`）；开发构建中同一目录下的 `termlet-desktop`；经符号链接运行时再按链接指向的位置
//! 找这两处。数据目录（因此通道）按找到的程序判断：程序旁边有 `portable.txt` 时是便携版，用程序旁边的 `data`。
//! Termlet 在运行、但命令行服务没有启动时
//! 它会写下原因（`run/cli.unavailable`），这里据此说明，而不是一直等到超时。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use termlet_proto::client::{CallError, Client, ConnectError};
use termlet_proto::endpoint::{self, Endpoint, parse_unavailable_marker};
use termlet_proto::{ClientHello, ErrorBody, ErrorCode, ExitCode, PROTOCOL_VERSION};

use crate::i18n::Lang;
use crate::messages;

/// 指定 Termlet 程序的环境变量；为空表示连不上时不拉起。
pub const APP_ENV: &str = "TERMLET_APP";

/// 等 Termlet 启动的最长时间。
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// 连不上 Termlet 的原因（输出为错误）。
#[derive(Debug)]
pub struct Failure {
    pub exit: ExitCode,
    pub code: String,
    pub message: String,
}

impl Failure {
    fn connect(message: String) -> Self {
        Self {
            exit: ExitCode::Failed,
            code: ErrorCode::ConnectFailed.as_str().into(),
            message,
        }
    }

    /// Termlet 没有运行、又不允许自动启动（退出码与“外部命令行已关闭”相同：都要用户先在 Termlet 中处理）。
    fn not_running(lang: Lang) -> Self {
        Self {
            exit: ExitCode::CliDisabled,
            code: ErrorCode::AppNotRunning.as_str().into(),
            message: messages::APP_NOT_RUNNING.get(lang).into(),
        }
    }

    fn api(body: ErrorBody) -> Self {
        Self {
            exit: ExitCode::for_error(&body.code),
            code: body.code,
            message: body.message,
        }
    }
}

/// 通信失败的说明。
pub fn call_failure(error: &CallError, lang: Lang) -> Failure {
    Failure::connect(match error {
        CallError::Io(e) => messages::CONNECT_IO.fill(lang, &[e]),
        CallError::Closed => messages::CLOSED.get(lang).into(),
        CallError::Protocol(e) => messages::PROTOCOL.fill(lang, &[e]),
    })
}

fn connect_failure(error: ConnectError, lang: Lang) -> Failure {
    match error {
        ConnectError::Rejected(body) => Failure::api(body),
        ConnectError::NotRunning => Failure::connect(messages::APP_TIMEOUT.get(lang).into()),
        ConnectError::Impostor => Failure::connect(messages::IMPOSTOR.get(lang).into()),
        ConnectError::Io(e) => Failure::connect(messages::CONNECT_IO.fill(lang, &[&e])),
        ConnectError::Protocol(e) => Failure::connect(messages::PROTOCOL.fill(lang, &[&e])),
    }
}

/// 连接（必要时拉起）Termlet 并握手。
pub fn connect(lang: Lang) -> Result<Client, Failure> {
    let app = app_path();
    let data_dir = data_dir_of(app.as_deref())
        .map_err(|e| Failure::connect(messages::DATA_DIR.fill(lang, &[&e])))?;
    let endpoint = Endpoint::for_data_dir(&data_dir.path);
    crate::debug::debug!(
        "数据目录 {}{}",
        data_dir.path.display(),
        if data_dir.portable {
            "（便携版）"
        } else {
            ""
        }
    );
    let hello = ClientHello {
        protocol: PROTOCOL_VERSION,
        client: format!("termlet {}", env!("CARGO_PKG_VERSION")),
        lang,
    };
    let attempt = || match Client::connect(&endpoint, &hello) {
        Ok(client) => Ok(Some(client)),
        Err(ConnectError::NotRunning) => Ok(None),
        Err(e) => Err(connect_failure(e, lang)),
    };
    if let Some(client) = attempt()? {
        crate::debug::debug!("已连上运行中的 Termlet");
        return Ok(client);
    }
    // Termlet 在运行，但命令行服务没有启动：再拉起一个也没用
    if let Some(reason) = unavailable(&endpoint) {
        crate::debug::debug!("Termlet 在运行，但命令行服务没有启动：{reason}");
        return Err(Failure::connect(
            messages::CLI_UNAVAILABLE.fill(lang, &[&reason]),
        ));
    }
    if !endpoint.launch_config().auto_start {
        crate::debug::debug!("Termlet 没有运行，设置中不允许自动启动");
        return Err(Failure::not_running(lang));
    }
    crate::debug::debug!("Termlet 没有运行，以无窗口模式拉起");
    launch(app.as_deref(), lang)?;
    let started = Instant::now();
    let deadline = started + START_TIMEOUT;
    loop {
        if let Some(client) = attempt()? {
            crate::debug::debug!(
                "已连上拉起的 Termlet（{} ms）",
                started.elapsed().as_millis()
            );
            return Ok(client);
        }
        if let Some(reason) = unavailable(&endpoint) {
            return Err(Failure::connect(
                messages::CLI_UNAVAILABLE.fill(lang, &[&reason]),
            ));
        }
        if Instant::now() >= deadline {
            return Err(Failure::connect(messages::APP_TIMEOUT.get(lang).into()));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// 运行中的 Termlet 没能启动命令行服务的原因。标记可能是异常退出的 Termlet 留下的：写下它的进程还在运行时才采信。
pub fn unavailable(endpoint: &Endpoint) -> Option<String> {
    let text = std::fs::read_to_string(endpoint.unavailable_marker()).ok()?;
    let (pid, reason) = parse_unavailable_marker(&text)?;
    process_alive(pid).then_some(reason)
}

/// 数据目录（与 Termlet 的规则相同）：`app` 是便携版（程序旁边有 `portable.txt`）时为程序旁边的 `data`，
/// 这样便携包中的命令行工具连到同一个便携的 Termlet，不会连到同时装着的安装版。
fn data_dir_of(app: Option<&Path>) -> Result<endpoint::ResolvedDataDir, endpoint::DataDirError> {
    endpoint::data_dir_for(app.and_then(Path::parent))
}

/// 以 headless 模式拉起 Termlet（不随命令行工具退出）。
fn launch(app: Option<&Path>, lang: Lang) -> Result<(), Failure> {
    let app = app.ok_or_else(|| Failure::connect(messages::APP_NOT_FOUND.get(lang).into()))?;
    crate::debug::debug!("启动 {} --headless", app.display());
    let mut command = Command::new(app);
    command
        .arg("--headless")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command
        .spawn()
        .map(drop)
        .map_err(|e| Failure::connect(messages::APP_START.fill(lang, &[&e])))
}

/// Termlet 程序的位置（见模块说明）。
pub fn app_path() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os(APP_ENV) {
        return (!value.is_empty()).then(|| PathBuf::from(value));
    }
    find_app(&std::env::current_exe().ok()?)
}

/// 在命令行工具 `me` 的附近找 Termlet。经符号链接运行时（例如链接为 `/usr/local/bin/termlet`）再按链接指向的位置找：
/// macOS 上 current_exe 是链接本身的路径（Linux 上已经是链接指向的文件）。
fn find_app(me: &Path) -> Option<PathBuf> {
    let real = std::fs::canonicalize(me).ok();
    candidates(me)
        .into_iter()
        .chain(real.as_deref().map(candidates).unwrap_or_default())
        .find(|p| p.is_file() && !same_file(p, me))
}

/// 可能的位置：安装目录（`bin` 的上一级）中的 Termlet，开发构建同一目录下的 termlet-desktop。
/// Windows 上文件名不分大小写，同一目录中的 `Termlet.exe` 就是命令行工具自己，不在其中。
fn candidates(me: &Path) -> Vec<PathBuf> {
    let suffix = std::env::consts::EXE_SUFFIX;
    let mut list = Vec::new();
    if let Some(dir) = me.parent() {
        if let Some(install) = dir.parent() {
            list.push(install.join(format!("Termlet{suffix}")));
        }
        list.push(dir.join(format!("termlet-desktop{suffix}")));
    }
    list
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// 进程是否还在运行。
#[cfg(windows)]
pub fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: 打开的句柄用完关闭
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        ok != 0 && code == STILL_ACTIVE as u32
    }
}

#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: 信号 0 只检查进程是否存在
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Termlet 在运行但命令行服务没有启动时说明原因；标记是已退出的进程留下的时不采信
    #[test]
    fn unavailable_marker_is_trusted_only_while_its_process_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::for_data_dir(tmp.path());
        assert_eq!(unavailable(&endpoint), None, "没有标记");
        std::fs::create_dir_all(tmp.path().join("run")).unwrap();
        let write = |pid: u32| {
            std::fs::write(
                endpoint.unavailable_marker(),
                endpoint::unavailable_marker_text(pid, "bind: invalid argument"),
            )
            .unwrap()
        };
        write(std::process::id());
        assert_eq!(
            unavailable(&endpoint).as_deref(),
            Some("bind: invalid argument")
        );
        // 已经退出的进程
        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        write(pid);
        assert_eq!(unavailable(&endpoint), None, "进程已退出时不采信残留的标记");
    }

    #[test]
    fn candidates_exclude_the_tool_itself() {
        let me = Path::new("C:/Termlet/bin/termlet.exe");
        let list = candidates(me);
        let suffix = std::env::consts::EXE_SUFFIX;
        assert_eq!(
            list,
            [
                PathBuf::from("C:/Termlet").join(format!("Termlet{suffix}")),
                PathBuf::from("C:/Termlet/bin").join(format!("termlet-desktop{suffix}")),
            ]
        );
    }

    // 便携包中的命令行工具（bin\termlet.exe）找到上一级的 Termlet，按它旁边的 portable.txt 用程序旁边的 data，
    // 与安装版的数据目录（因此通道）不同
    #[test]
    fn portable_tool_uses_the_portable_data_dir() {
        let suffix = std::env::consts::EXE_SUFFIX;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("Termlet");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join(format!("Termlet{suffix}")), b"").unwrap();
        let me = root.join("bin").join(format!("termlet{suffix}"));
        std::fs::write(&me, b"").unwrap();
        let app = find_app(&me).expect("应当找到 Termlet");
        let installed = data_dir_of(Some(&app)).unwrap();
        assert!(!installed.portable, "没有标记时与安装版相同");

        std::fs::write(root.join(endpoint::PORTABLE_MARKER), b"").unwrap();
        let portable = data_dir_of(Some(&app)).unwrap();
        assert!(portable.portable);
        assert_eq!(
            std::fs::canonicalize(portable.path.parent().unwrap()).unwrap(),
            std::fs::canonicalize(&root).unwrap()
        );
        assert_ne!(
            Endpoint::for_data_dir(&portable.path),
            Endpoint::for_data_dir(&installed.path)
        );
    }

    // 命令行工具经符号链接运行（例如按设置中的说明链接到 /usr/local/bin）：按链接指向的位置找到应用包中的 Termlet
    #[cfg(unix)]
    #[test]
    fn finds_the_app_through_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let install = tmp.path().join("Termlet.app/Contents/MacOS");
        std::fs::create_dir_all(install.join("bin")).unwrap();
        std::fs::write(install.join("Termlet"), b"").unwrap();
        std::fs::write(install.join("bin/termlet"), b"").unwrap();
        let bin = tmp.path().join("usr-local-bin");
        std::fs::create_dir(&bin).unwrap();
        let link = bin.join("termlet");
        std::os::unix::fs::symlink(install.join("bin/termlet"), &link).unwrap();
        let found = find_app(&link).expect("应当找到 Termlet");
        assert!(same_file(&found, &install.join("Termlet")), "{found:?}");
        // 链接旁边没有 Termlet、链接指向的位置也没有时找不到
        std::fs::remove_file(install.join("Termlet")).unwrap();
        assert_eq!(find_app(&link), None);
    }
}
