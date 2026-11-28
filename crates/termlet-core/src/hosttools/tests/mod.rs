//! 主机工具的测试：脚本在本机的 sh 中执行（Windows 上是 Git 的 sh，没有时跳过），配合假命令与假的 /proc；
//! 解析用固定的样例输出；`TERMLET_TEST_SSH=127.0.0.1:2222` 时连真实的 sshd（e2e 的测试容器）。

mod action;
mod cgroup;
mod container;
mod docker;
mod logs;
mod netstat;
mod privilege;
mod process;
mod prune;
mod scripts;
mod service;
mod sshd;
#[cfg(windows)]
mod wsl;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::exec::{one_line, sh_script};

/// 本机的 sh：优先选同一目录下有 sed 的（Git 的 `usr/bin`，隔离 PATH 时还要用到其中的 sed、awk 等）。
fn find_sh() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let names: &[&str] = if cfg!(windows) { &["sh.exe"] } else { &["sh"] };
    let sed = if cfg!(windows) { "sed.exe" } else { "sed" };
    let candidates: Vec<PathBuf> = std::env::split_paths(&path)
        .flat_map(|dir| names.iter().map(move |n| dir.join(n)))
        .filter(|p| p.is_file())
        .collect();
    candidates
        .iter()
        .find(|p| p.parent().is_some_and(|d| d.join(sed).is_file()))
        .or(candidates.first())
        .cloned()
}

/// 把命令行经标准输入交给本机的 sh 执行，返回标准输出；没有 sh 时为 `None`。
/// （Windows 上经命令行传参时，引号与反斜杠会先被 MSYS 的参数解析改掉，因此经标准输入。）
pub(crate) fn run_sh_stdin(command: &str) -> Option<String> {
    let Some(sh) = find_sh() else {
        println!("本机没有 sh，跳过");
        return None;
    };
    let mut child = Command::new(sh)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(command.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "执行失败：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// 把本机路径写成 sh 能用的路径（Windows 上 `C:\a\b` → `/c/a/b`）。
pub(crate) fn posix(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) && text.as_bytes().get(1) == Some(&b':') {
        return format!("/{}{}", text[..1].to_ascii_lowercase(), &text[2..]);
    }
    text
}

/// 用本机的 sh 执行脚本：`fake` 写的假命令放在 PATH 最前面；`replace` 替换脚本中的路径（例如把 /proc 换成测试目录）。
pub(crate) struct ShRunner {
    sh: PathBuf,
    bin: tempfile::TempDir,
    /// 为真时 PATH 只有假命令与基本命令（[`ShRunner::isolated_dir`]；本机装的 docker、sudo 等不会被找到）
    pub isolated: bool,
    /// 隔离 PATH 时的基本命令（Unix）：链接 PATH 中除 [`HOST_TOOLS`] 以外的命令
    #[cfg(unix)]
    tools: Option<tempfile::TempDir>,
}

/// 隔离 PATH 时找不到的命令（Unix）：主机工具的脚本按有没有它们走不同的分支，测试中用假命令代替，或者测“没有安装”。
#[cfg(unix)]
const HOST_TOOLS: &[&str] = &["docker", "podman", "ss", "sudo", "tmux"];

impl ShRunner {
    /// 本机没有 sh 时为 `None`（测试跳过）。
    pub fn new() -> Option<Self> {
        let Some(sh) = find_sh() else {
            println!("本机没有 sh，跳过");
            return None;
        };
        Some(Self {
            sh,
            bin: tempfile::tempdir().unwrap(),
            isolated: false,
            #[cfg(unix)]
            tools: None,
        })
    }

    /// 隔离 PATH 的执行器。
    pub fn isolated() -> Option<Self> {
        let mut runner = Self::new()?;
        runner.isolated = true;
        #[cfg(unix)]
        {
            let tools = tempfile::tempdir().unwrap();
            let path = std::env::var_os("PATH").unwrap_or_default();
            for dir in std::env::split_paths(&path) {
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    if HOST_TOOLS.iter().any(|tool| name == *tool) {
                        continue;
                    }
                    // 同名的以 PATH 中靠前的为准（已有链接时失败）
                    let _ = std::os::unix::fs::symlink(entry.path(), tools.path().join(&name));
                }
            }
            runner.tools = Some(tools);
        }
        Some(runner)
    }

    /// 隔离 PATH 时假命令之后的目录：Windows 上是 sh 所在的目录（Git 的 `usr/bin`，其中没有 docker、sudo 等）；
    /// 其他系统上 sh 所在的目录（`/usr/bin`）中什么都有，改用链接了基本命令的临时目录。
    fn isolated_dir(&self) -> &Path {
        #[cfg(unix)]
        if let Some(tools) = &self.tools {
            return tools.path();
        }
        self.sh.parent().unwrap()
    }

    /// 写一个假命令。
    pub fn fake(&self, name: &str, body: &str) {
        let path = self.bin.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// 删除一个假命令。
    pub fn remove(&self, name: &str) {
        std::fs::remove_file(self.bin.path().join(name)).unwrap();
    }

    /// 执行 `script`（与远端相同：合成一行、经 `sh -c` 包装，`args` 为 `$1…`），返回标准输出。
    pub fn run(&self, script: &str, replace: &[(&str, &str)], args: &[&str]) -> String {
        self.exec(script, replace, &[], args)
    }

    /// 与 [`ShRunner::run`] 相同，另加环境变量。
    pub fn run_env(&self, script: &str, env: &[(&str, &str)]) -> String {
        self.exec(script, &[], env, &[])
    }

    fn exec(
        &self,
        script: &str,
        replace: &[(&str, &str)],
        env: &[(&str, &str)],
        args: &[&str],
    ) -> String {
        let mut script = script.to_string();
        for (from, to) in replace {
            script = script.replace(from, to);
        }
        let path = if self.isolated {
            self.isolated_dir().as_os_str().to_owned()
        } else {
            std::env::var_os("PATH").unwrap_or_default()
        };
        let path = std::env::join_paths(
            std::iter::once(self.bin.path().to_path_buf()).chain(std::env::split_paths(&path)),
        )
        .unwrap();
        let mut command = Command::new(&self.sh);
        command
            .env("PATH", path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(sh_script(&one_line(&script), args).as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "执行失败：{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

/// `/proc/<pid>/stat` 的一行（52 个字段，未用到的为 0）。
#[allow(clippy::too_many_arguments)]
fn stat_line(
    pid: u32,
    comm: &str,
    state: &str,
    ppid: u32,
    flags: u64,
    utime: u64,
    stime: u64,
    prio: i32,
    nice: i32,
    threads: u32,
    start: u64,
    vsize: u64,
    rss: u64,
) -> String {
    let mut f: Vec<String> = vec!["0".into(); 50];
    // f[0] 对应第 3 个字段（state）
    f[0] = state.into();
    f[1] = ppid.to_string();
    f[4] = "34816".into();
    f[5] = "-1".into();
    f[6] = flags.to_string();
    f[11] = utime.to_string();
    f[12] = stime.to_string();
    f[15] = prio.to_string();
    f[16] = nice.to_string();
    f[17] = threads.to_string();
    f[19] = start.to_string();
    f[20] = vsize.to_string();
    f[21] = rss.to_string();
    format!("{pid} ({comm}) {}\n", f.join(" "))
}

fn status_file(name: &str, ppid: u32, uid: u32) -> String {
    format!(
        "Name:\t{name}\nUmask:\t0022\nState:\tS (sleeping)\nTgid:\t1\nNgid:\t0\nPid:\t1\nPPid:\t{ppid}\nTracerPid:\t0\n\
         Uid:\t{uid}\t{uid}\t{uid}\t{uid}\nGid:\t0\t0\t0\t0\nVmRSS:\t   1024 kB\nThreads:\t1\n"
    )
}

/// 在 `dir` 下生成一棵假的 /proc：systemd、内核线程、名称带空格与括号的进程、这个终端的 shell 及子进程
/// （环境变量带会话标记）、切换用户后没有标记的孙进程、读不到 status 的进程、名称中有换行的进程。
pub(crate) fn fake_proc(dir: &Path) {
    let write = |rel: &str, content: &str| {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    };
    write(
        "proc/stat",
        "cpu  100 0 50 1000 0 0 0 0 0 0\ncpu0 50 0 25 500 0 0 0 0 0 0\ncpu1 50 0 25 500 0 0 0 0 0 0\nintr 1\nbtime 1700000000\n",
    );
    write("proc/uptime", "1000.00 1900.00\n");
    write(
        "proc/meminfo",
        "MemTotal:        2048000 kB\nMemFree:  100 kB\n",
    );
    write(
        "passwd",
        "root:x:0:0:root:/root:/bin/bash\nwww-data:x:33:33::/var/www:/usr/sbin/nologin\ntermlet:x:1000:1000::/home/termlet:/bin/bash\n",
    );
    write(
        "proc/1/stat",
        &stat_line(
            1, "systemd", "S", 0, 4194560, 300, 200, 20, 0, 1, 10, 170000000, 3000,
        ),
    );
    write("proc/1/status", &status_file("systemd", 0, 0));
    write(
        "proc/2/stat",
        &stat_line(2, "kthreadd", "S", 0, 2129984, 0, 0, 20, 0, 1, 2, 0, 0),
    );
    write("proc/2/status", &status_file("kthreadd", 0, 0));
    write(
        "proc/300/stat",
        &stat_line(
            300,
            "Web Content",
            "R",
            1,
            4194560,
            1000,
            500,
            20,
            0,
            25,
            5000,
            900000000,
            50000,
        ),
    );
    write("proc/300/status", &status_file("Web Content", 1, 33));
    write(
        "proc/300/cmdline",
        "/usr/lib/firefox/firefox\x00-contentproc\x00-childID\x002\x00",
    );
    write("proc/1/cmdline", "/sbin/init\x00splash\x00");
    // 参数中有换行：接成一行；伪造的段标记不起作用，冒充 PID 1 的标题使 PID 1 的命令行都不认
    write(
        "proc/301/cmdline",
        "worker\x00a\n@@uid\n0\n==> /proc/1/cmdline <==\nfake\x00",
    );
    write(
        "proc/301/stat",
        &stat_line(
            301, "a) b) c", "S", 1, 4194560, 1, 1, 20, 5, 1, 6000, 1000, 10,
        ),
    );
    write("proc/301/status", &status_file("a) b) c", 1, 1000));
    write(
        "proc/400/stat",
        &stat_line(
            400, "bash", "S", 399, 4194560, 10, 5, 20, 0, 1, 7000, 9000000, 800,
        ),
    );
    write("proc/400/status", &status_file("bash", 399, 1000));
    write(
        "proc/400/environ",
        "HOME=/home/termlet\x00LC_TERMLET_SESSION=termlet-abc123\x00",
    );
    write(
        "proc/401/stat",
        &stat_line(
            401, "sudo", "S", 400, 4194560, 1, 1, 20, 0, 1, 7100, 9000000, 800,
        ),
    );
    write("proc/401/status", &status_file("sudo", 400, 0));
    write("proc/401/environ", "LC_TERMLET_SESSION=termlet-abc123\x00");
    write(
        "proc/402/stat",
        &stat_line(
            402, "vim", "S", 401, 4194560, 1, 1, 20, 0, 1, 7200, 9000000, 800,
        ),
    );
    write("proc/402/status", &status_file("vim", 401, 0));
    write("proc/402/environ", "TERM=xterm\x00");
    write(
        "proc/500/stat",
        &stat_line(500, "gone", "Z", 1, 4194560, 1, 1, 20, 0, 1, 8000, 0, 0),
    );
    // 名称中有换行（prctl 可以设置）：stat 不止一行，换行之后伪造段标记
    write(
        "proc/600/stat",
        &stat_line(
            600,
            "evil\n@@uid\n0",
            "S",
            1,
            4194560,
            1,
            1,
            20,
            0,
            1,
            9000,
            1000,
            10,
        ),
    );
    write("proc/600/status", &status_file(r"evil\n@@uid\n0", 1, 1000));
}

/// 假 /proc 中没有的 PID，当作执行脚本的 shell（见 [`proc_replace`]）。
const NOT_A_FAKE_PID: u32 = 999_999;

/// 把假 /proc 与 /etc/passwd 换进脚本的替换表。进程列表的脚本跳过自己的 shell（`me=$$`）与以它为父的进程，这里把
/// `$$` 换成假 /proc 中没有的 PID：本机 sh 的 `$$` 可能恰好是假 /proc 中的某个 PID（MSYS2 的 PID 是顺序分配的小编号，
/// 容器中的 PID 也从小数开始），那个进程与它的子进程就被当成脚本自己跳过，数目对不上（偶发）。
pub(crate) fn proc_replace(dir: &Path) -> Vec<(&'static str, String)> {
    proc_replace_as(dir, NOT_A_FAKE_PID)
}

/// 同 [`proc_replace`]，执行脚本的 shell 当作假 /proc 中的 `me`（测试跳过自己的那一段）。
pub(crate) fn proc_replace_as(dir: &Path, me: u32) -> Vec<(&'static str, String)> {
    vec![
        ("/proc", format!("{}/proc", posix(dir))),
        ("/etc/passwd", format!("{}/passwd", posix(dir))),
        ("me=$$", format!("me={me}")),
    ]
}

/// `proc_replace` 的结果借用成 [`ShRunner::run`] 的参数。
pub(crate) fn borrow<'a>(pairs: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    pairs.iter().map(|(a, b)| (*a, b.as_str())).collect()
}
