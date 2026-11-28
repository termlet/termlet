#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};

use super::*;

/// 等一个程序结束的最长时间。
const RUN_TIMEOUT: Duration = Duration::from_secs(60);

/// 运行命令，取标准输出与标准错误（最多等 [`RUN_TIMEOUT`]）。没有结束时结束它与它启动的进程（不留下还在运行、占着
/// 测试输出的进程），返回 `None`。
fn try_output_of(mut cmd: Command) -> Option<String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    let child = cmd.spawn().expect("启动程序");
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let Ok(out) = rx.recv_timeout(RUN_TIMEOUT) else {
        kill_tree(pid);
        return None;
    };
    let out = out.expect("运行程序");
    Some(String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr))
}

/// 结束进程与它启动的进程（Windows 上连同子进程，其他系统上只结束它自己）。
fn kill_tree(pid: u32) {
    let pid = pid.to_string();
    let mut kill = if cfg!(windows) {
        let mut taskkill = Command::new("taskkill");
        taskkill.args(["/T", "/F", "/PID", pid.as_str()]);
        taskkill
    } else {
        let mut kill = Command::new("kill");
        kill.args(["-KILL", pid.as_str()]);
        kill
    };
    let _ = kill.stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// 同 [`try_output_of`]，没有结束时测试失败，写明是哪个程序（`what`）、什么命令。
pub(super) fn output_of(what: &str, cmd: Command) -> String {
    let line = command_line(&cmd);
    try_output_of(cmd)
        .unwrap_or_else(|| panic!("{what}（{line}）{} 秒内没有结束", RUN_TIMEOUT.as_secs()))
}

/// 命令行（程序与参数，失败时的说明用）。
fn command_line(cmd: &Command) -> String {
    std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(|a| a.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

/// 按启动描述的程序、环境变量与起始目录运行的命令，参数另给（终端的参数是交互式的，测试中换成执行一条命令）。
fn command_like(launch: &Launch, args: &[&str]) -> Command {
    let mut cmd = Command::new(&launch.program);
    cmd.args(args)
        .env_clear()
        .envs(launch.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&launch.cwd);
    cmd
}

#[cfg(unix)]
fn run_like(launch: &Launch, args: &[&str]) -> String {
    output_of(
        &launch.program.display().to_string(),
        command_like(launch, args),
    )
}

/// Windows arm64 上的 Git Bash、MSYS2、Cygwin：都是 MSYS / Cygwin 运行时的 x64 程序，在 x64 仿真下运行。GitHub 的
/// windows-11-arm 运行器上它们偶尔启动不了（没有任何输出就结束）、卡住，或者慢到几十秒：同一个 job 中前后两次启动
/// Git Bash，一次正常、一次什么也没输出（actions/partner-runner-images#169）；同一个测试在 x64 的运行器上约 6–8 秒，
/// 在 arm64 上要 64–78 秒。只有这几种、只在 arm64 上，没有在时限内结束或没有任何输出时跳过并说明；x64 上照常要求
/// 能运行，命令提示符、PowerShell 在哪里都照常要求。
#[cfg(windows)]
fn flaky_under_emulation(kind: ShellKind) -> bool {
    cfg!(target_arch = "aarch64")
        && matches!(
            kind,
            ShellKind::GitBash | ShellKind::Msys2 | ShellKind::Cygwin
        )
}

fn fake(id: &str, path: PathBuf) -> Profile {
    Profile {
        id: id.to_string(),
        name: id.to_uppercase(),
        custom_name: None,
        kind: ShellKind::Cmd,
        path,
        args: Vec::new(),
        command: String::new(),
        dir: String::new(),
        env: BTreeMap::new(),
    }
}

fn custom_shell(id: &str, command: &str, dir: &str, env: &[(&str, &str)]) -> LocalShell {
    LocalShell {
        id: id.to_string(),
        name: format!("{id} 自定义"),
        command: command.to_string(),
        dir: dir.to_string(),
        env: env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    }
}

/// 用户主目录对应的环境变量名。
const HOME_VAR: &str = if cfg!(windows) { "USERPROFILE" } else { "HOME" };

#[test]
fn detector_caches_and_finds() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let real = std::env::current_exe().unwrap();
    let d = Detector::with_detect(Duration::from_secs(3600), move |_| {
        let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
        let mut list = vec![fake("a", real.clone()), fake("gone", PathBuf::new())];
        if n > 1 {
            list.push(fake("b", PathBuf::new()));
        }
        list
    });
    assert_eq!(d.list(false).len(), 2);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    d.list(false);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "缓存未过期时不应重新检测");

    // 缓存中有、程序也在：不重新检测
    let none = ShellSettings::default();
    assert_eq!(d.find("a", &none).unwrap().name, "A");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // 缓存中没有的（例如刚安装的）重新检测一次
    assert_eq!(d.find("b", &none).unwrap().name, "B");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    // 程序已不在原来的位置：重新检测一次，仍以检测结果为准
    assert_eq!(d.find("gone", &none).unwrap().id, "gone");
    assert_eq!(calls.load(Ordering::SeqCst), 3);

    let custom = ShellSettings {
        custom: vec![custom_shell("custom-x", r#""C:\x y\a.exe" -v"#, "", &[])],
        ..Default::default()
    };
    let p = d.find("custom-x", &custom).unwrap();
    assert!(p.is_custom());
    assert_eq!(p.kind.as_str(), "custom");
    assert_eq!(p.path, PathBuf::from(r"C:\x y\a.exe"));
    assert_eq!(p.command, r#""C:\x y\a.exe" -v"#);
    assert_eq!(d.find("nope", &custom), Err(ShellError::UnknownShell));
}

/// 只读缓存的查找：不重新检测，过期了也用；强制刷新时同时的调用只检测一次。
#[test]
fn cached_lookups_never_detect_again() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let d = Arc::new(Detector::with_detect(Duration::ZERO, move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(50));
        vec![fake("gone", PathBuf::new())]
    }));
    assert!(d.is_stale());
    // 还没有检测过：检测一次
    assert_eq!(d.cached().len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // 已过期（缓存时间为 0）、程序也不在：只读缓存的查找都不重新检测
    assert!(d.is_stale());
    let none = ShellSettings::default();
    assert_eq!(d.find_cached("gone", &none).unwrap().id, "gone");
    assert_eq!(d.find_cached("nope", &none), Err(ShellError::UnknownShell));
    assert_eq!(d.cached().len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // 同时强制刷新：后到的等先到的检测完，用它的结果
    let threads: Vec<_> = (0..3)
        .map(|_| {
            let d = d.clone();
            std::thread::spawn(move || d.list(true))
        })
        .collect();
    for t in threads {
        assert_eq!(t.join().unwrap().len(), 1);
    }
    let after = calls.load(Ordering::SeqCst);
    assert!((2..=4).contains(&after), "检测了 {after} 次");
}

fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// 对 `id` 的改动。
fn with_changes(id: &str, changes: ShellOverride) -> ShellSettings {
    ShellSettings {
        overrides: BTreeMap::from([(id.to_string(), changes)]),
        ..Default::default()
    }
}

// 对检测到的 shell 的改动：改了的名称、参数、起始目录照改，环境变量加在检测到的之上；没改的项用检测到的
#[test]
fn override_changes_only_what_was_set() {
    let mut detected = fake("gitbash", PathBuf::from("bash.exe"));
    detected.args = vec!["-i".into(), "-l".into()];
    detected.env = vars(&[("CHERE_INVOKING", "1"), ("MSYSTEM", "UCRT64")]);

    assert_eq!(
        detected.clone().customized(&ShellSettings::default()),
        detected,
        "没有改动时原样"
    );
    let changed = detected.clone().customized(&with_changes(
        "gitbash",
        ShellOverride {
            name: "  工作  ".into(),
            dir: "~/work".into(),
            env: vars(&[("MSYSTEM", "MINGW64"), ("FOO", "1")]),
            ..Default::default()
        },
    ));
    assert_eq!(changed.custom_name.as_deref(), Some("工作"));
    assert_eq!(changed.name, detected.name, "检测到的名称留着");
    assert_eq!(changed.args, detected.args, "没改参数时用检测到的");
    assert_eq!(changed.dir, "~/work");
    assert_eq!(
        changed.env,
        vars(&[
            ("CHERE_INVOKING", "1"),
            ("MSYSTEM", "MINGW64"),
            ("FOO", "1")
        ])
    );
    assert_eq!(changed.path, detected.path, "程序不改");

    // 参数：换成别的、清空；编辑框中显示的写法能原样拆回
    let args = |text: &str| {
        detected
            .clone()
            .with_override(&ShellOverride {
                args: Some(text.into()),
                ..Default::default()
            })
            .args
    };
    assert_eq!(
        args(r#"--login -c "echo a b""#),
        ["--login", "-c", "echo a b"]
    );
    assert!(args("").is_empty(), "Some(\"\") 为不带参数");
    assert_eq!(args(&detected.args_text()), detected.args);
    assert_eq!(detected.args_text(), "-i -l");

    // 名称全是空白时不算改名；别的 shell、自定义的 shell 不受影响
    let blank = ShellOverride {
        name: "   ".into(),
        ..Default::default()
    };
    assert_eq!(detected.clone().with_override(&blank).custom_name, None);
    let other = with_changes("cmd", blank.clone());
    assert_eq!(detected.clone().customized(&other), detected);
    let custom = Profile::custom(&custom_shell("gitbash", "bash", "", &[]));
    assert_eq!(
        custom.clone().customized(&with_changes("gitbash", blank)),
        custom
    );
    assert!(ShellOverride::default().is_empty());
}

// Windows 上环境变量的名称不分大小写：改动中的 msystem 换掉检测到的 MSYSTEM，不并存
#[cfg(windows)]
#[test]
fn override_env_names_ignore_case_on_windows() {
    let mut detected = fake("msys2", PathBuf::from("bash.exe"));
    detected.env = vars(&[("MSYSTEM", "UCRT64")]);
    let changed = detected.with_override(&ShellOverride {
        env: vars(&[("msystem", "CLANG64")]),
        ..Default::default()
    });
    assert_eq!(changed.env, vars(&[("msystem", "CLANG64")]));
}

// 改过的 shell 检测不到时（卸载了）按 ID 找不到、也不在列表中，设置中的改动照样留着；重新检测到时照旧生效
#[test]
fn override_applies_again_when_shell_reappears() {
    let installed = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let flag = installed.clone();
    let real = std::env::current_exe().unwrap();
    let d = Detector::with_detect(Duration::from_secs(3600), move |_| {
        let mut list = vec![fake("cmd", real.clone())];
        if flag.load(Ordering::SeqCst) {
            list.push(fake("gitbash", real.clone()));
        }
        list
    });
    let shells = with_changes(
        "gitbash",
        ShellOverride {
            dir: "~/work".into(),
            env: vars(&[("MARKER", "1")]),
            ..Default::default()
        },
    );
    assert_eq!(d.find_cached("gitbash", &shells).unwrap().dir, "~/work");

    installed.store(false, Ordering::SeqCst);
    assert!(d.list(true).iter().all(|p| p.id != "gitbash"));
    assert_eq!(
        d.find_cached("gitbash", &shells),
        Err(ShellError::UnknownShell)
    );
    assert_eq!(
        d.find_cached("cmd", &shells).unwrap().dir,
        "",
        "别的不受影响"
    );

    // 重新装上：缓存中没有，查找时重新检测，改动照旧生效
    installed.store(true, Ordering::SeqCst);
    let back = d.find("gitbash", &shells).unwrap();
    assert_eq!(back.dir, "~/work");
    assert_eq!(back.env.get("MARKER").map(String::as_str), Some("1"));
}

// 改动传到启动的进程：按改过的参数运行，带上改动中的环境变量，从改过的起始目录开始
#[test]
fn override_reaches_the_started_process() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("termlet-override-dir");
    std::fs::create_dir(&dir).unwrap();
    let (detected, args) = if cfg!(windows) {
        let cmd = Path::new(&std::env::var("SystemRoot").unwrap_or(r"C:\Windows".into()))
            .join(r"System32\cmd.exe");
        (fake("cmd", cmd), r#"/d /c "echo override-%MARKER%& cd""#)
    } else {
        (
            fake("sh", PathBuf::from("/bin/sh")),
            "-c 'echo override-$MARKER; pwd'",
        )
    };
    let shells = with_changes(
        &detected.id,
        ShellOverride {
            args: Some(args.into()),
            dir: dir.to_string_lossy().into_owned(),
            env: vars(&[("MARKER", "ok-42")]),
            ..Default::default()
        },
    );
    let launch = detected.customized(&shells).launch().unwrap();
    assert_eq!(launch.cwd, dir);
    assert_eq!(launch.env_var("MARKER"), Some("ok-42"));
    let mut run = Command::new(&launch.program);
    run.args(&launch.args)
        .env_clear()
        .envs(launch.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&launch.cwd);
    let out = output_of("改过的 shell", run);
    assert!(
        out.contains("override-ok-42"),
        "应带着改过的参数与环境变量启动：{out}"
    );
    assert!(
        out.contains("termlet-override-dir"),
        "应从改过的起始目录开始：{out}"
    );
}

#[test]
fn build_env_overrides_and_adds_terminal_vars() {
    let base: Env = [("PATH", "/bin"), ("COLORTERM", "256"), ("KEEP", "1")]
        .into_iter()
        .collect();
    let reference = if cfg!(windows) {
        "%KEEP%-x"
    } else {
        "${KEEP}-x"
    };
    let extra: BTreeMap<String, String> = [
        ("FOO", "bar"),
        ("REF", reference),
        ("", "empty-name"),
        ("A=B", "bad-name"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let env = build_env(&base, &extra);
    let pairs: Vec<_> = env.iter().collect();
    for want in [
        ("PATH", "/bin"),
        ("KEEP", "1"),
        ("COLORTERM", "truecolor"),
        ("TERM_PROGRAM", "Termlet"),
        ("FOO", "bar"),
        ("REF", "1-x"),
    ] {
        assert!(pairs.contains(&want), "环境变量中应有 {want:?}：{env:?}");
    }
    assert_eq!(
        pairs.iter().filter(|(k, _)| *k == "COLORTERM").count(),
        1,
        "同名变量应被覆盖而不是重复"
    );
    assert!(pairs.iter().all(|(k, _)| !k.is_empty() && !k.contains('=')));
    assert_eq!(
        env.get("TERM"),
        (!cfg!(windows)).then_some("xterm-256color")
    );

    // 额外的变量可以覆盖终端相关的变量
    let extra = BTreeMap::from([("COLORTERM".to_string(), "24bit".to_string())]);
    assert_eq!(build_env(&base, &extra).get("COLORTERM"), Some("24bit"));
}

#[test]
fn start_dir_expands_and_falls_back_to_home() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = home.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let other = tmp.path().join("other");
    std::fs::create_dir(&other).unwrap();
    let env: Env = [
        (HOME_VAR, home.to_string_lossy().into_owned()),
        ("OTHER", other.to_string_lossy().into_owned()),
    ]
    .into_iter()
    .collect();
    let var = if cfg!(windows) { "%OTHER%" } else { "$OTHER" };
    assert_eq!(start_dir("", &env), home);
    assert_eq!(start_dir("~", &env), home);
    assert_eq!(start_dir("~/work", &env), work);
    assert_eq!(start_dir(&format!("~{MAIN_SEPARATOR}work"), &env), work);
    assert_eq!(
        start_dir("~/nope-dir", &env),
        home,
        "目录不存在时回到主目录"
    );
    assert_eq!(start_dir(&format!("  {var}  "), &env), other);
    assert_eq!(start_dir(&other.to_string_lossy(), &env), other);
    assert_eq!(start_dir("~work", &env), home, "~user 的写法不支持");
}

#[test]
fn launch_fails_when_program_missing() {
    let missing = std::env::temp_dir()
        .join("termlet-no-such-dir")
        .join("x.exe");
    assert_eq!(
        launch_in(&fake("cmd", missing), Env::new()).unwrap_err(),
        ShellError::ProgramNotFound
    );
    let custom = Profile::custom(&custom_shell(
        "custom-t2",
        "no-such-shell-1a2b.exe",
        "",
        &[],
    ));
    assert_eq!(custom.launch().unwrap_err(), ShellError::ProgramNotFound);
    let empty = Profile::custom(&custom_shell("custom-t3", "  ", "", &[]));
    assert_eq!(empty.launch().unwrap_err(), ShellError::ProgramNotFound);
}

#[test]
fn launch_debug_hides_env_values_and_composes_command_line() {
    let launch = Launch {
        program: PathBuf::from(r"C:\Program Files\Git\bin\bash.exe"),
        args: vec!["-i".into(), "-l".into()],
        raw_args: None,
        cwd: PathBuf::from(r"C:\Users\ruan"),
        env: vec![("API_TOKEN".into(), "secret-value-1".into())],
    };
    let debug = format!("{launch:?}");
    assert!(
        debug.contains("API_TOKEN") && !debug.contains("secret-value-1"),
        "{debug}"
    );
    assert_eq!(
        launch.command_line(),
        r#""C:\Program Files\Git\bin\bash.exe" -i -l"#
    );
    assert_eq!(launch.argv().len(), 3);
    assert_eq!(launch.env_var("API_TOKEN"), Some("secret-value-1"));

    let raw = Launch {
        raw_args: Some(r#" /k "echo a""#.into()),
        program: PathBuf::from(r"C:\Windows\System32\cmd.exe"),
        ..launch
    };
    assert_eq!(
        raw.command_line(),
        r#"C:\Windows\System32\cmd.exe /k "echo a""#
    );
}

// 本机的 shell：至少有命令提示符，以及 Windows PowerShell 或 PowerShell 7；检测到的每一个都能按启动描述运行
#[cfg(windows)]
#[test]
fn detect_and_run_windows() {
    let list = Detector::new(Duration::ZERO).list(true);
    for p in &list {
        println!("检测到 {}：{} {:?} {:?}", p.id, p.name, p.path, p.args);
    }
    let find = |id: &str| list.iter().find(|p| p.id == id);
    let cmd = find("cmd").expect("应检测到命令提示符");
    assert!(
        find("powershell").is_some() || find("pwsh").is_some(),
        "应检测到 PowerShell"
    );
    let mut ids: Vec<_> = list.iter().map(|p| p.id.as_str()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), list.len(), "ID 不应重复");
    if let Some(git) = find("gitbash") {
        let dir = git
            .path
            .parent()
            .and_then(Path::file_name)
            .unwrap_or_default();
        assert!(
            !dir.eq_ignore_ascii_case("System32"),
            "不能把 WSL 的 bash.exe 当成 Git Bash"
        );
    }

    // 不继承 Termlet 进程的环境变量（取的是用户最新的），起始目录是用户主目录
    let launch = cmd.launch().unwrap();
    let mut run = Command::new(&launch.program);
    run.raw_arg("/d /c echo [%CARGO_MANIFEST_DIR%] %CD% %TERM_PROGRAM%")
        .env_clear()
        .envs(launch.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&launch.cwd);
    let out = output_of(&cmd.name, run);
    assert!(
        out.contains("[%CARGO_MANIFEST_DIR%]"),
        "不应继承进程的变量：{out}"
    );
    // cmd 按控制台代码页输出，主目录含非 ASCII 字符时比较不了
    let home = dirs::home_dir().unwrap().to_string_lossy().to_lowercase();
    if home.is_ascii() {
        assert!(
            out.to_lowercase().contains(&home),
            "应从用户主目录开始：{out}"
        );
    }
    assert!(out.contains("Termlet"), "{out}");

    let ps = |script: &str| -> Vec<String> {
        [
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            script,
        ]
        .map(String::from)
        .to_vec()
    };
    let bash = |script: &str| -> Vec<String> { ["-l", "-c", script].map(String::from).to_vec() };
    for p in &list {
        let (args, want) = match p.kind {
            ShellKind::Pwsh => (ps("Write-Output ('pw' + 'sh-ok')"), "pwsh-ok".to_string()),
            ShellKind::PowerShell => (
                ps("Write-Output ('power' + 'shell-ok')"),
                "powershell-ok".to_string(),
            ),
            ShellKind::GitBash => (bash("echo git$((1+1))bash-ok"), "git2bash-ok".to_string()),
            ShellKind::Msys2 => (
                bash("echo msys$((1+1))-ok-$MSYSTEM"),
                format!("msys2-ok-{}", p.env["MSYSTEM"]),
            ),
            ShellKind::Cygwin => (bash("echo cyg$((1+1))-ok"), "cyg2-ok".to_string()),
            // 开发者 PowerShell：导入模块后 VSINSTALLDIR 有值
            ShellKind::VsDev => (
                ps(&format!(
                    "{}; Write-Output ('vs-' + [bool]$env:VSINSTALLDIR)",
                    p.args[3]
                )),
                "vs-True".to_string(),
            ),
            _ => continue,
        };
        let launch = p.launch().unwrap();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let line = format!("{} {}", launch.program.display(), args.join(" "));
        let started = std::time::Instant::now();
        let out = try_output_of(command_like(&launch, &args));
        let elapsed = started.elapsed().as_millis();
        // 每一个的用时都打印出来（CI 中通过时也列出，见 .config/nextest.toml）：慢的是哪一个一看便知
        println!("{} 用时 {elapsed} ms", p.name);
        let problem = match &out {
            None => Some(format!("{} 秒内没有结束", RUN_TIMEOUT.as_secs())),
            Some(out) if out.trim().is_empty() => Some("没有任何输出".to_string()),
            Some(_) => None,
        };
        if let Some(problem) = problem {
            assert!(
                flaky_under_emulation(p.kind),
                "{}（{line}）{problem}",
                p.name
            );
            println!(
                "跳过 {}（{line}）：{problem}。Windows arm64 上它在 x64 仿真下运行，GitHub 的运行器上偶尔启动不了或卡住\
                 （actions/partner-runner-images#169），只在 arm64 上跳过",
                p.name
            );
            continue;
        }
        let out = out.unwrap_or_default();
        assert!(
            out.contains(&want),
            "{}（{line}）的输出中没有 {want:?}：{out}",
            p.name
        );
    }
}

// 自定义 shell：在用户的 PATH 中查找程序，参数原样传给程序，带额外的环境变量与起始目录
#[cfg(windows)]
#[test]
fn custom_shell_windows() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("termlet-start-dir");
    std::fs::create_dir(&dir).unwrap();
    let shell = custom_shell(
        "custom-t1",
        r#"cmd.exe /d /c "echo custom-%MARKER%& cd""#,
        &dir.to_string_lossy(),
        &[("MARKER", "ok-42")],
    );
    let launch = Profile::custom(&shell).launch().unwrap();
    assert!(launch.program.is_absolute());
    assert!(
        launch
            .program
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case("cmd.exe"))
    );
    assert_eq!(
        launch.raw_args.as_deref(),
        Some(r#" /d /c "echo custom-%MARKER%& cd""#)
    );
    assert_eq!(launch.args, ["/d", "/c", "echo custom-%MARKER%& cd"]);
    assert_eq!(launch.cwd, dir);
    assert_eq!(launch.env_var("marker"), Some("ok-42"));
    assert_eq!(
        launch.command_line(),
        format!(
            r#"{} /d /c "echo custom-%MARKER%& cd""#,
            launch.program.display()
        )
    );

    let mut run = Command::new(&launch.program);
    run.raw_arg(launch.raw_args.as_deref().unwrap())
        .env_clear()
        .envs(launch.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&launch.cwd);
    let out = output_of(&shell.name, run);
    assert!(
        out.contains("custom-ok-42"),
        "应带着参数与环境变量启动：{out}"
    );
    assert!(
        out.contains("termlet-start-dir"),
        "应从设置的目录开始：{out}"
    );
}

// Unix：$SHELL 排在最前；自定义 shell 在 PATH 中查找程序，参数按 shell 规则拆分，带环境变量与起始目录，
// 不带启动 Termlet 的程序（例如 VS Code 的终端）带来的 Electron 变量
#[cfg(unix)]
#[test]
fn detect_and_run_unix() {
    let mut base = unix::filter_inherited(
        std::env::vars().chain([("ELECTRON_RUN_AS_NODE".to_string(), "1".to_string())]),
    );
    base.set("SHELL", "/bin/sh");
    let list = unix::detect(&base);
    assert_eq!(list[0].id, "sh");
    assert_eq!(list[0].path, Path::new("/bin/sh"));
    assert_eq!(list[0].args, ["-l"]);
    let launch = launch_in(&list[0], base.clone()).unwrap();
    let out = run_like(&launch, &["-c", "echo $((40+2))-$TERM-$COLORTERM"]);
    assert!(out.contains("42-xterm-256color-truecolor"), "{out}");

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("termlet-start-dir");
    std::fs::create_dir(&dir).unwrap();
    let shell = custom_shell(
        "custom-t1",
        "sh -c 'echo custom-$MARKER-[$ELECTRON_RUN_AS_NODE]; pwd'",
        &dir.to_string_lossy(),
        &[("MARKER", "ok")],
    );
    let launch = launch_in(&Profile::custom(&shell), base).unwrap();
    assert_eq!(
        launch.args,
        ["-c", "echo custom-$MARKER-[$ELECTRON_RUN_AS_NODE]; pwd"]
    );
    assert_eq!(launch.raw_args, None);
    let args: Vec<&str> = launch.args.iter().map(String::as_str).collect();
    let out = run_like(&launch, &args);
    assert!(
        out.contains("custom-ok-[]") && out.contains("termlet-start-dir"),
        "应带着参数、环境变量与起始目录启动，不带 Electron 的变量：{out}"
    );
}

#[cfg(unix)]
#[test]
fn start_dir_unix_home() {
    let env: Env = [("HOME", "/home/ruan")].into_iter().collect();
    assert_eq!(start_dir("~/nope-dir", &env), Path::new("/home/ruan"));
}
