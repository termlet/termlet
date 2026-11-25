//! 本机的伪终端：Windows 上是 ConPTY，其他平台是 `/dev/ptmx`。
//!
//! Windows 的安装包与便携版在程序旁边附带微软的 conpty.dll 与 OpenConsole.exe（`scripts/release.mjs` 的 `CONPTY`）：
//! portable-pty 第一次开伪终端时按名字加载 conpty.dll（桌面端启动时已把 DLL 的搜索限定为程序目录与 System32），
//! 有就用它，它再启动同一目录的 OpenConsole.exe，比系统自带的新（程序开的鼠标上报等模式能传到终端）；没有时用
//! 系统自带的。一个进程中只决定一次。
//!
//! 读写各用一个线程：读到的输出经有界通道交给终端（终端解析跟不上时读线程等待，形成背压）；
//! 写入放进队列由写线程发送，界面线程从不阻塞。子进程退出、输出读完后发出结束事件（带退出码）。

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use termlet_io::{CloseReason, GridSize, IoEvent, TerminalInput, TerminalIo};

use crate::{CoreError, Result};

/// 启动一个本地终端需要的参数（由本地 shell 检测给出）。
#[derive(Clone, Default)]
pub struct PtyLaunch {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// 起始目录；`None` 用当前目录
    pub cwd: Option<PathBuf>,
    /// 完整的环境变量；`None` 继承本进程的环境
    pub env: Option<Vec<(String, String)>>,
}

impl std::fmt::Debug for PtyLaunch {
    // 环境变量里常有用户自己设置的密码、令牌：只输出名称
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtyLaunch")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("cwd", &self.cwd)
            .field(
                "env",
                &self
                    .env
                    .as_ref()
                    .map(|env| env.iter().map(|(k, _)| k).collect::<Vec<_>>()),
            )
            .finish()
    }
}

/// 读缓冲的大小。
const READ_BUFFER: usize = 64 * 1024;

/// 启动子进程，返回终端的两端。
pub fn spawn(launch: PtyLaunch, size: GridSize) -> Result<TerminalIo> {
    let system = native_pty_system();
    let pair = system
        .openpty(pty_size(size))
        .map_err(|e| CoreError::Pty(e.to_string()))?;

    let mut command = CommandBuilder::new(&launch.program);
    command.args(&launch.args);
    if let Some(cwd) = &launch.cwd {
        command.cwd(cwd);
    }
    match &launch.env {
        // 完整的环境变量（本地 shell 检测给出）已按平台带好终端变量，用户自定义的值优先，不再覆盖
        Some(env) => {
            command.env_clear();
            for (key, value) in env {
                command.env(key, value);
            }
        }
        // 继承本进程的环境：让程序知道自己在一个支持 256 色的终端里
        None => {
            command.env("TERM", "xterm-256color");
            command.env("COLORTERM", "truecolor");
            command.env("TERM_PROGRAM", "Termlet");
        }
    }

    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|e| CoreError::Pty(e.to_string()))?;
    // 子进程拿到了从端，本进程不再需要；不关掉的话子进程退出后读端读不到结束
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| CoreError::Pty(e.to_string()))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|e| CoreError::Pty(e.to_string()))?;
    let killer = child.clone_killer();
    tracing::info!(program = %launch.program.display(), pid = ?child.process_id(), "本地终端已启动");

    let (output_tx, output) = async_channel::bounded::<IoEvent>(64);
    let (input_tx, input_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let (exit_tx, exit_rx) = std::sync::mpsc::channel::<Option<i32>>();
    let master: SharedMaster = Arc::new(Mutex::new(Some(pair.master)));
    let exited = Arc::new(AtomicBool::new(false));

    // 等子进程退出。Windows 上伪控制台不会随子进程关闭，要主动关掉它，读线程才能读完剩余输出、读到结束
    let waiter_master = master.clone();
    let waiter_exited = exited.clone();
    let _ = std::thread::Builder::new()
        .name("pty-waiter".into())
        .spawn(move || {
            let code = child.wait().ok().map(|status| status.exit_code() as i32);
            // 子进程已回收：之后不再结束它（Unix 上按进程号发信号，进程号可能已被别的进程复用）
            waiter_exited.store(true, Ordering::SeqCst);
            tracing::info!(?code, "本地终端已退出");
            let _ = exit_tx.send(code);
            if cfg!(windows) {
                // 先取出、放开锁再关：Windows 11 24H2 之前关闭伪控制台要等其中的进程退出，
                // 持着锁时界面线程上的调整大小会一直等
                let master = waiter_master
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                drop(master);
            }
        });

    let _ = std::thread::Builder::new()
        .name("pty-reader".into())
        .spawn(move || {
            let mut buf = vec![0u8; READ_BUFFER];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if output_tx
                            .send_blocking(IoEvent::Output(buf[..n].to_vec()))
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let code = exit_rx.recv().ok().flatten();
            let _ = output_tx.send_blocking(IoEvent::Closed(CloseReason::Exited(code)));
        });

    let _ = std::thread::Builder::new()
        .name("pty-writer".into())
        .spawn(move || {
            while let Ok(data) = input_rx.recv() {
                if writer
                    .write_all(&data)
                    .and_then(|_| writer.flush())
                    .is_err()
                {
                    break;
                }
            }
        });

    let input = Arc::new(PtyInput {
        master,
        queue: Mutex::new(input_tx),
        killer: Mutex::new(killer),
        exited,
    });
    Ok(TerminalIo { output, input })
}

fn pty_size(size: GridSize) -> PtySize {
    PtySize {
        rows: size.rows.max(1),
        cols: size.columns.max(1),
        pixel_width: size.pixel_width(),
        pixel_height: size.pixel_height(),
    }
}

/// 伪终端的主端；子进程退出后（Windows）关掉，之后不再调整大小。
type SharedMaster = Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>;

struct PtyInput {
    master: SharedMaster,
    queue: Mutex<std::sync::mpsc::Sender<Vec<u8>>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    /// 子进程已退出并回收
    exited: Arc<AtomicBool>,
}

impl TerminalInput for PtyInput {
    fn write(&self, data: &[u8]) {
        let _ = self.queue.lock().unwrap().send(data.to_vec());
    }

    fn resize(&self, size: GridSize) {
        if let Some(master) = self.master.lock().unwrap().as_ref()
            && let Err(e) = master.resize(pty_size(size))
        {
            tracing::debug!("调整本地终端大小失败：{e}");
        }
    }

    /// 关闭终端时结束子进程；读线程随后读到结束。子进程已经退出时什么也不做。
    fn close(&self) {
        if self.exited.load(Ordering::SeqCst) {
            return;
        }
        let _ = self.killer.lock().unwrap_or_else(|e| e.into_inner()).kill();
    }
}

impl Drop for PtyInput {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn collect_until(
        io: &TerminalIo,
        needle: &str,
        timeout: Duration,
    ) -> (String, Option<CloseReason>) {
        let deadline = Instant::now() + timeout;
        let mut text = String::new();
        let mut closed = None;
        while Instant::now() < deadline {
            match io.output.try_recv() {
                Ok(IoEvent::Output(bytes)) => {
                    let chunk = String::from_utf8_lossy(&bytes);
                    // ConPTY 启动时查询光标位置，收到回答才继续（终端引擎会自动回答，这里模拟）
                    if chunk.contains("\x1b[6n") {
                        io.input.write(b"\x1b[1;1R");
                    }
                    text.push_str(&chunk);
                    if text.contains(needle) {
                        break;
                    }
                }
                Ok(IoEvent::Closed(reason)) => {
                    closed = Some(reason);
                    break;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        (text, closed)
    }

    fn launch(program: &str, args: &[&str]) -> PtyLaunch {
        PtyLaunch {
            program: program.into(),
            args: args.iter().map(|a| a.to_string()).collect(),
            ..Default::default()
        }
    }

    /// 输出 termlet-pty-ok 后以退出码 3 结束的命令。
    fn exit_with_3() -> PtyLaunch {
        if cfg!(windows) {
            launch(
                "cmd.exe",
                &["/d", "/q", "/c", "echo termlet-pty-ok & exit 3"],
            )
        } else {
            launch("/bin/sh", &["-c", "echo termlet-pty-ok; exit 3"])
        }
    }

    #[test]
    fn runs_command_and_reports_exit() {
        let io = spawn(exit_with_3(), GridSize::new(80, 24)).unwrap();
        let (text, _) = collect_until(&io, "termlet-pty-ok", Duration::from_secs(10));
        assert!(text.contains("termlet-pty-ok"), "输出：{text:?}");
        // 读完剩余输出后应收到退出码
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut code = None;
        while Instant::now() < deadline {
            match io.output.try_recv() {
                Ok(IoEvent::Closed(reason)) => {
                    code = Some(reason);
                    break;
                }
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        assert_eq!(code, Some(CloseReason::Exited(Some(3))));
    }

    #[test]
    fn input_reaches_the_shell() {
        let (shell, line) = if cfg!(windows) {
            (
                launch("cmd.exe", &["/d", "/q"]),
                "echo typed-%COMPUTERNAME%-done\r",
            )
        } else {
            (launch("/bin/sh", &[]), "echo typed-$HOME-done\r")
        };
        let io = spawn(shell, GridSize::new(80, 24)).unwrap();
        io.input.write(line.as_bytes());
        let (text, _) = collect_until(&io, "-done", Duration::from_secs(10));
        assert!(text.contains("typed-"), "输出：{text:?}");
        io.input.resize(GridSize::new(100, 30));
        io.input.write(b"exit\r");
    }

    /// 随带的 ConPTY（CI 用 `release.mjs conpty --into target/debug/deps` 放在测试程序旁边，并设
    /// `TERMLET_TEST_CONPTY=1`）：程序开的鼠标上报原样传到终端（系统自带的 ConPTY 会吞掉 `?1000h`），
    /// 承载终端的是测试程序旁边的 OpenConsole.exe。没设时跳过。
    #[cfg(windows)]
    #[test]
    fn bundled_conpty_passes_mouse_modes() {
        if std::env::var_os("TERMLET_TEST_CONPTY").is_none_or(|v| v != "1") {
            eprintln!("没有设 TERMLET_TEST_CONPTY=1，跳过");
            return;
        }
        let exe = std::env::current_exe().unwrap();
        let dir = exe.parent().unwrap();
        let host = dir.join("OpenConsole.exe");
        assert!(
            dir.join("conpty.dll").is_file() && host.is_file(),
            "测试程序旁边没有随带的 ConPTY：{}",
            dir.display()
        );

        let io = spawn(
            launch(
                "cmd.exe",
                &["/d", "/q", "/k", "echo \x1b[?1000h\x1b[?1006hmouse-on"],
            ),
            GridSize::new(80, 24),
        )
        .unwrap();
        let (text, closed) = collect_until(&io, "\x1b[?1006h", Duration::from_secs(15));
        assert_eq!(closed, None, "输出：{text:?}");
        assert!(
            text.contains("\x1b[?1000h") && text.contains("\x1b[?1006h"),
            "鼠标上报的模式没有传到终端：{text:?}"
        );

        // 伪终端还开着时看本进程启动的 OpenConsole.exe
        let script = format!(
            "Get-CimInstance Win32_Process | Where-Object {{ $_.ParentProcessId -eq {} -and \
             $_.Name -eq 'OpenConsole.exe' }} | ForEach-Object {{ $_.ExecutablePath }}",
            std::process::id()
        );
        let out = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .unwrap();
        let paths = String::from_utf8_lossy(&out.stdout);
        let expected = host.to_string_lossy();
        assert!(
            paths.lines().any(|p| {
                let p = p.trim();
                p.strip_prefix(r"\\?\")
                    .unwrap_or(p)
                    .eq_ignore_ascii_case(&expected)
            }),
            "承载终端的不是 {expected}：{paths:?}"
        );
        eprintln!("本地终端经随带的 ConPTY：{expected}");
        io.input.write(b"exit\r");
    }
}
