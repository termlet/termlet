//! 连接 Termlet：连不上时以 headless 模式拉起它，等命令行服务就绪（最多 10 秒）。设置“AI 与自动化”中关闭了
//! “Termlet 没有打开时自动在后台启动”时不拉起，提示先打开 Termlet（`app_not_running`，见 `endpoint::LaunchConfig`）。
//!
//! 找 Termlet 程序的顺序：环境变量 `TERMLET_APP`（为空表示不拉起）；安装目录（命令行工具在 `bin\termlet.exe`，
//! 程序在上一级的 `Termlet.exe`）；开发构建中同一目录下的 `termlet-desktop`；经符号链接运行时再按链接指向的位置
//! 找这两处。数据目录（因此通道）按找到的程序判断：程序旁边有 `portable.txt` 时是便携版，用程序旁边的 `data`。
//! Termlet 在运行、但命令行服务没有启动时
//! 它会写下原因（`run/cli.unavailable`），这里据此说明，而不是一直等到超时。

use std::path::{Path, PathBuf};
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

/// 连不上 Termlet 的原因（输出为错误）。与 MCP 服务共用（`termlet-mcp` 的 `Backend` 返回它）。
pub use termlet_mcp::Failure;

/// 连接、通信失败。
fn connect_error(message: String) -> Failure {
    Failure {
        exit: ExitCode::Failed,
        code: ErrorCode::ConnectFailed.as_str().into(),
        message,
    }
}

/// Termlet 没有运行、又不允许自动启动（退出码与“外部命令行已关闭”相同：都要用户先在 Termlet 中处理）。
fn not_running(lang: Lang) -> Failure {
    Failure {
        exit: ExitCode::CliDisabled,
        code: ErrorCode::AppNotRunning.as_str().into(),
        message: messages::APP_NOT_RUNNING.get(lang).into(),
    }
}

fn api_error(body: ErrorBody) -> Failure {
    Failure {
        exit: ExitCode::for_error(&body.code),
        code: body.code,
        message: body.message,
    }
}

/// 通信失败的说明。
pub fn call_failure(error: &CallError, lang: Lang) -> Failure {
    connect_error(match error {
        CallError::Io(e) => messages::CONNECT_IO.fill(lang, &[e]),
        CallError::Closed => messages::CLOSED.get(lang).into(),
        CallError::Protocol(e) => messages::PROTOCOL.fill(lang, &[e]),
    })
}

fn connect_failure(error: ConnectError, lang: Lang) -> Failure {
    match error {
        ConnectError::Rejected(body) => api_error(body),
        ConnectError::NotRunning => connect_error(messages::APP_TIMEOUT.get(lang).into()),
        ConnectError::Impostor => connect_error(messages::IMPOSTOR.get(lang).into()),
        ConnectError::Io(e) => connect_error(messages::CONNECT_IO.fill(lang, &[&e])),
        ConnectError::Protocol(e) => connect_error(messages::PROTOCOL.fill(lang, &[&e])),
    }
}

/// 连接（必要时拉起）Termlet 并握手。
pub fn connect(lang: Lang) -> Result<Client, Failure> {
    let app = app_path();
    let data_dir = data_dir_of(app.as_deref())
        .map_err(|e| connect_error(messages::DATA_DIR.fill(lang, &[&e])))?;
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
        return Err(connect_error(
            messages::CLI_UNAVAILABLE.fill(lang, &[&reason]),
        ));
    }
    if !endpoint.launch_config().auto_start {
        crate::debug::debug!("Termlet 没有运行，设置中不允许自动启动");
        return Err(not_running(lang));
    }
    crate::debug::debug!("Termlet 没有运行，以无窗口模式拉起");
    launch(app.as_deref(), &data_dir.path, lang)?;
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
            return Err(connect_error(
                messages::CLI_UNAVAILABLE.fill(lang, &[&reason]),
            ));
        }
        if Instant::now() >= deadline {
            return Err(connect_error(messages::APP_TIMEOUT.get(lang).into()));
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

/// 以 headless 模式拉起 Termlet（不随命令行工具退出）。`data_dir` 是解析好的数据目录（绝对路径）。
fn launch(app: Option<&Path>, data_dir: &Path, lang: Lang) -> Result<(), Failure> {
    let app = app.ok_or_else(|| connect_error(messages::APP_NOT_FOUND.get(lang).into()))?;
    crate::debug::debug!("启动 {} --headless", app.display());
    spawn_detached(app, data_dir).map_err(|e| connect_error(messages::APP_START.fill(lang, &[&e])))
}

/// Unix 上沿用调用方的当前目录与环境（相对的 `TERMLET_DATA_DIR` 照样解析到同一处）。
#[cfg(unix)]
fn spawn_detached(app: &Path, _data_dir: &Path) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    Command::new(app)
        .arg("--headless")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map(drop)
}

/// Windows 上不用 `Command`，自己调用 `CreateProcessW`：
///
/// - 不继承任何句柄。`Command` 总是带上本进程全部可继承的句柄，调用方交给 termlet 的标准输入输出（多是管道）与
///   termlet 从上级继承来的都在其中，`Stdio::null()` 只换掉子进程的标准句柄：拉起的 Termlet 会一直开着这些管道，
///   读到结束才返回的调用方（AI 工具捕获输出、`$(…)`、MCP 客户端）要等它退出（10 分钟没有请求后）。Termlet
///   用不到标准输入输出（没有控制台，日志写文件）。
/// - 当前目录是程序所在的目录，与从开始菜单启动时相同：不占住调用方所在的目录（Windows 上被占着的目录删不掉），
///   也不停在 WSL 的目录（`\wsl.localhost\…`）中；应用内 HTTP 的 MCP 换算本机路径看的是应用的当前目录
///   （`termlet_mcp::local_path`），这样规则不随 Termlet 是从哪里拉起的而变。为此相对的 `TERMLET_DATA_DIR`
///   换成解析好的绝对路径再交给 Termlet，两边仍是同一个数据目录。
#[cfg(windows)]
fn spawn_detached(app: &Path, data_dir: &Path) -> std::io::Result<()> {
    use std::ffi::{OsStr, OsString};
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DETACHED_PROCESS,
        PROCESS_INFORMATION, STARTUPINFOW,
    };
    let app = std::path::absolute(app)?;
    let dir = app.parent().unwrap_or(&app);
    let wide = |text: &OsStr| -> Vec<u16> { text.encode_wide().chain(Some(0)).collect() };
    let application = wide(app.as_os_str());
    let directory = wide(dir.as_os_str());
    // Windows 的路径中不会有引号
    let mut line = OsString::from("\"");
    line.push(app.as_os_str());
    line.push("\" --headless");
    let mut line = wide(&line);
    let environment = environment_with_data_dir(data_dir);
    // SAFETY: 两个结构都是全零即有效的 C 结构
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: 程序路径、命令行（可写）与当前目录是以 0 结尾的宽字符串，环境块以两个 0 结尾（没有时沿用本进程的）；
    // 成功时关闭返回的进程与线程句柄
    unsafe {
        let ok = CreateProcessW(
            application.as_ptr(),
            line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_UNICODE_ENVIRONMENT,
            environment
                .as_ref()
                .map_or(std::ptr::null(), |block| block.as_ptr().cast()),
            directory.as_ptr(),
            &startup,
            &mut info,
        );
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        CloseHandle(info.hProcess);
        CloseHandle(info.hThread);
    }
    Ok(())
}

/// 设了 `TERMLET_DATA_DIR` 时，交给 Termlet 的环境块（UTF-16，按名字排序，以两个 0 结尾）：本进程的环境，其中这个
/// 变量换成解析好的绝对路径 `data_dir`。没有设时为 `None`（沿用本进程的环境）。
#[cfg(windows)]
fn environment_with_data_dir(data_dir: &Path) -> Option<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    std::env::var_os(endpoint::DATA_DIR_ENV).filter(|v| !v.is_empty())?;
    let mut vars: Vec<_> = std::env::vars_os()
        .filter(|(name, _)| !name.eq_ignore_ascii_case(endpoint::DATA_DIR_ENV))
        .collect();
    vars.push((
        endpoint::DATA_DIR_ENV.into(),
        data_dir.as_os_str().to_owned(),
    ));
    vars.sort_by_key(|(name, _)| name.to_ascii_uppercase());
    let mut block = Vec::new();
    for (name, value) in vars {
        block.extend(name.encode_wide());
        block.push(u16::from(b'='));
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    Some(block)
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
    use std::process::{Command, Stdio};

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
