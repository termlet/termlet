//! 进程页的测试。

use std::collections::HashMap;
use std::time::Duration;

use super::{ShRunner, borrow, fake_proc, proc_replace};
use crate::hosttools::process::{
    Inner, finish_detail, parse_proc_line, parse_process_detail, parse_processes,
    process_detail_command, processes_command,
};
use crate::hosttools::scripts::{proc_detail_script, proc_script};
use crate::hosttools::{HostTools, Process, Target, ToolsError};
use crate::ssh::ExecResult;

fn by_pid(list: &[Process]) -> HashMap<u32, Process> {
    list.iter().map(|p| (p.pid, p.clone())).collect()
}

fn fake_basics(runner: &ShRunner) {
    runner.fake(
        "getconf",
        "case $1 in CLK_TCK) echo 100;; PAGESIZE) echo 4096;; esac",
    );
    runner.fake("id", "echo 1000");
}

/// 排除内核线程；名称中的空格、括号、换行；读不到 status 的进程；
/// 标出这个终端的 shell 与它的子进程（含切换用户后没有标记的孙进程）。
#[test]
fn proc_script_on_fake_proc() {
    let Some(runner) = ShRunner::new() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    fake_proc(dir.path());
    fake_basics(&runner);
    runner.fake("tmux", "exit 1");
    let replace = proc_replace(dir.path());
    let out = runner.run(
        &proc_script(),
        &borrow(&replace),
        &["LC_TERMLET_SESSION=termlet-abc123", "termlet-abc123"],
    );
    let (list, _, _) =
        parse_processes(&out, "termlet-abc123").unwrap_or_else(|e| panic!("{e}\n{out}"));
    let procs = by_pid(&list.processes);
    assert!(!procs.contains_key(&2), "内核线程应排除");
    assert_eq!(list.processes.len(), 8, "{out}");
    assert_eq!(list.total, 8);
    assert!(!list.truncated);
    assert_eq!(list.uid, Some(1000));
    assert_eq!(list.cores, 2);
    assert_eq!(list.mem_total, 2048000 * 1024);

    let web = &procs[&300];
    assert_eq!(web.name, "Web Content");
    assert_eq!(web.state, 'R');
    assert_eq!(web.user.as_deref(), Some("www-data"));
    assert_eq!(web.threads, 25);
    assert_eq!(web.rss, 50000 * 4096);
    assert_eq!(web.cpu_time, 15.0);
    assert_eq!(web.start, Some(1700000000 * 1000 + 50000));

    assert_eq!(
        web.cmdline.as_deref(),
        Some("/usr/lib/firefox/firefox -contentproc -childID 2")
    );

    let p = &procs[&301];
    assert_eq!(
        (p.name.as_str(), p.nice, p.user.as_deref()),
        ("a) b) c", 5, Some("termlet")),
        "名称中的括号"
    );
    assert_eq!(
        p.cmdline.as_deref(),
        Some("worker a @@uid 0"),
        "参数中的换行接成一行，伪造的段标记只是内容"
    );
    assert_eq!(
        procs[&1].cmdline, None,
        "参数中冒充的标题与 PID 1 自己的重复：都不认"
    );
    assert_eq!(procs[&400].cmdline, None, "没有 cmdline 文件时为 None");
    let p = &procs[&500];
    assert_eq!(
        (p.uid, p.state),
        (None, 'Z'),
        "读不到 status 的进程没有 UID"
    );
    // 名称中有换行：不漏掉这个进程，换行写成 ?，伪造的 @@uid 不起作用
    let p = &procs[&600];
    assert_eq!(p.name, "evil?@@uid?0");
    assert_eq!(p.start, Some(1700000000 * 1000 + 90000));
    assert_eq!(p.start_ticks, 9000);
    assert_eq!(p.uid, Some(1000));

    assert!(
        procs[&400].terminal_shell && procs[&400].in_terminal,
        "应标出这个终端的 shell"
    );
    assert!(!procs[&401].terminal_shell && procs[&401].in_terminal);
    assert!(
        procs[&402].in_terminal,
        "切换用户后没有标记的子进程也属于这个终端"
    );
    assert!(
        !procs[&1].in_terminal && !procs[&300].in_terminal,
        "其他进程不属于这个终端"
    );
}

/// tmux 中的 shell 继承 tmux 服务的环境，环境变量不准；按以会话标识命名的 tmux 会话的窗格
/// 找出这个终端的 shell。没有同名会话时 tmux 按前缀匹配到别的会话，要核对会话名。
#[test]
fn processes_in_tmux() {
    let Some(runner) = ShRunner::new() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    fake_proc(dir.path());
    fake_basics(&runner);
    // list-panes -s -t 会话 -F 格式：本终端的会话有两个窗格（402、301），另一个会话以它为前缀
    runner.fake(
        "tmux",
        "[ \"$1\" = list-panes ] || exit 1\ncase $4 in termlet-abc123) printf 'termlet-abc123:402\\ntermlet-abc123:301\\n';; termlet-abc) echo 'termlet-abc123:402';; esac",
    );
    let replace = proc_replace(dir.path());
    let out = runner.run(
        &proc_script(),
        &borrow(&replace),
        &["LC_TERMLET_SESSION=termlet-abc123", "termlet-abc123"],
    );
    let (list, _, _) = parse_processes(&out, "termlet-abc123").unwrap();
    let procs = by_pid(&list.processes);
    for pid in [402, 301] {
        assert!(
            procs[&pid].terminal_shell && procs[&pid].in_terminal,
            "tmux 窗格的 shell 是本终端：{:?}",
            procs[&pid]
        );
    }
    assert!(
        !procs[&400].in_terminal && !procs[&401].in_terminal,
        "tmux 终端不按环境变量中的标记（可能是别的终端的）"
    );
    // 会话不存在（前缀匹配到别的会话）：按环境变量
    let out = runner.run(
        &proc_script(),
        &borrow(&replace),
        &["LC_TERMLET_SESSION=termlet-abc", "termlet-abc"],
    );
    let (list, _, _) = parse_processes(&out, "termlet-abc").unwrap();
    assert!(
        !list
            .processes
            .iter()
            .any(|p| p.pid == 402 && p.terminal_shell),
        "别的 tmux 会话的窗格不是本终端"
    );
}

/// 读取脚本自己的 shell（`$$`）与它派生的子进程不列出、不计数（每次读取都是新的 PID，会混进进程页与概览页
/// CPU 最高的进程）；孙进程不是它派生的，照常列出。测试中把 `$$` 换成假 /proc 中的 PID。
#[test]
fn proc_script_skips_itself() {
    let Some(runner) = ShRunner::new() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    fake_proc(dir.path());
    fake_basics(&runner);
    runner.fake("tmux", "exit 1");
    let write = |rel: &str, content: &str| std::fs::write(dir.path().join(rel), content).unwrap();
    for (pid, ppid, name) in [(700, 1, "sh"), (701, 700, "cat"), (702, 701, "sleep")] {
        std::fs::create_dir_all(dir.path().join(format!("proc/{pid}"))).unwrap();
        write(
            &format!("proc/{pid}/stat"),
            &super::stat_line(pid, name, "S", ppid, 4194560, 0, 0, 20, 0, 1, 9500, 0, 0),
        );
        write(
            &format!("proc/{pid}/status"),
            &super::status_file(name, ppid, 1000),
        );
    }
    let mut replace = proc_replace(dir.path());
    replace.push(("me=$$", "me=700".to_string()));
    let out = runner.run(&proc_script(), &borrow(&replace), &["", ""]);
    let (list, _, _) = parse_processes(&out, "").unwrap_or_else(|e| panic!("{e}\n{out}"));
    let procs = by_pid(&list.processes);
    assert!(!procs.contains_key(&700), "脚本自己的 shell 不列出：{out}");
    assert!(!procs.contains_key(&701), "它派生的子进程不列出");
    assert!(procs.contains_key(&702), "孙进程照常列出");
    assert_eq!(list.total, 9, "不计入总数（原有 8 个加孙进程）");
    // 照常执行（`$$` 是本机 sh 自己）时什么都不少
    let out = runner.run(
        &proc_script(),
        &borrow(&proc_replace(dir.path())),
        &["", ""],
    );
    let (list, _, _) = parse_processes(&out, "").unwrap();
    assert_eq!(list.total, 11);
}

/// 第二次读取后才有 CPU 使用率；PID 被重新使用（启动时刻不同）时不算。
#[test]
fn cpu_from_two_reads() {
    let sample = |uptime: &str, ticks1: u64, ticks2: u64, start2: u64| {
        format!(
            "@@uid\n0\n@@clk\n100\n@@page\n4096\n@@uptime\n{uptime} 0\n@@stat\nbtime 1700000000\ncpu0 1\n\
             @@mem\nMemTotal: 1000 kB\n@@passwd\nroot:0\n@@self\n@@procs\n\
             10 S 1 {ticks1} 0 20 0 1 100 1000 10 0 a\n\
             11 S 1 {ticks2} 0 20 0 1 {start2} 1000 10 0 b\n\
             @@total 2\n"
        )
    };
    let inner = Inner::new("termlet-x");
    assert!(
        processes_command(&inner.key).starts_with("sh -c "),
        "应经 sh -c 执行"
    );
    let span = Duration::from_secs(10);
    let first = inner
        .ingest(&sample("100.00", 1000, 500, 200), 0, span)
        .unwrap();
    assert_eq!(first.processes[0].cpu, None, "第一次没有 CPU");
    let second = inner
        .ingest(&sample("102.00", 1100, 900, 300), 2000, span)
        .unwrap();
    assert_eq!(
        second.processes[0].cpu,
        Some(50.0),
        "100 个时钟周期 / 2 秒 = 50%"
    );
    assert_eq!(second.processes[1].cpu, None, "PID 重新使用后不应算 CPU");
    assert_eq!(second.at, 2000);
    // 上一次读数在 3 分钟前（超过两个刷新间隔）：不拿它算，这次没有 CPU；紧接着再读一次就有了
    let stale = inner
        .ingest(&sample("282.00", 1500, 900, 300), 182_000, span)
        .unwrap();
    assert!(
        stale.processes.iter().all(|p| p.cpu.is_none()),
        "上一次读数太旧时不算 CPU：{stale:?}"
    );
    let fresh = inner
        .ingest(&sample("284.00", 1600, 900, 300), 184_000, span)
        .unwrap();
    assert_eq!(fresh.processes[0].cpu, Some(50.0), "按最近这一小段时间算");
}

/// 不是 Linux；登录 shell 不能执行 sh（网络设备等）时带上错误输出。
#[test]
fn unsupported_and_unreadable() {
    let inner = Inner::new("");
    let result = ExecResult {
        exit_code: Some(0),
        stdout: b"@@unsupported\nFreeBSD\n".to_vec(),
        ..Default::default()
    };
    assert_eq!(
        inner.finish(&result, 0, Duration::from_secs(10)),
        Err(ToolsError::Unsupported)
    );
    let result = ExecResult {
        exit_code: Some(127),
        stderr: b"% Invalid input detected".to_vec(),
        ..Default::default()
    };
    match inner.finish(&result, 0, Duration::from_secs(10)) {
        Err(ToolsError::Unreadable { exit_code, stderr }) => {
            assert_eq!(exit_code, Some(127));
            assert!(stderr.contains("Invalid input"), "{stderr}");
        }
        other => panic!("应带上错误输出：{other:?}"),
    }
}

/// 会话标记不合法时不传给脚本（不进命令）。
#[test]
fn session_marker_is_validated() {
    let command = processes_command(&Inner::new("termlet-abc").key);
    assert!(
        command.ends_with(" sh 'LC_TERMLET_SESSION=termlet-abc' 'termlet-abc'"),
        "应传入会话标记：{}",
        &command[command.len() - 60..]
    );
    let command = processes_command(&Inner::new("x'; rm -rf / #").key);
    assert!(command.ends_with(" sh '' ''"), "不合法的标记不应传入");
}

#[test]
fn proc_line() {
    for line in [
        "",
        "abc S 1 0 0 20 0 1 0 0 0 0 x",
        "0 S 1 0 0 20 0 1 0 0 0 0 x",
        "12 S 1 0 0 20 0 1 0 0 0",
    ] {
        assert!(parse_proc_line(line, 4096).is_none(), "应拒绝：{line:?}");
    }
    let (p, ticks, start) =
        parse_proc_line("12 S 1 7 3 20 -5 4 99 8192 2 1000 name with spaces", 4096).unwrap();
    assert_eq!(p.name, "name with spaces");
    assert_eq!(
        (ticks, start, p.nice, p.rss, p.uid),
        (10, 99, -5, 8192, Some(1000))
    );
}

/// 详情的各项；远端可控的路径、进程名中伪造段标记不起作用；进程不存在；PID 不合法。
#[test]
fn process_detail() {
    let Some(runner) = ShRunner::new() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    fake_proc(dir.path());
    std::fs::write(
        dir.path().join("proc/300/cmdline"),
        b"/usr/lib/firefox/firefox\x00-contentproc\x00@@exe\x00",
    )
    .unwrap();
    runner.fake(
        "getconf",
        "case $1 in CLK_TCK) echo 100;; PAGESIZE) echo 4096;; esac",
    );
    runner.fake(
        "readlink",
        "case $1 in */exe) echo /usr/lib/firefox/firefox;; *) exit 1;; esac",
    );
    let replace = proc_replace(dir.path());
    let out = runner.run(&proc_detail_script(), &borrow(&replace), &["300"]);
    let d = parse_process_detail(&out, 300).unwrap_or_else(|| panic!("解析失败：\n{out}"));
    assert_eq!(d.process.name, "Web Content");
    assert_eq!(d.process.ppid, 1);
    assert_eq!(d.process.user.as_deref(), Some("www-data"));
    assert_eq!(d.exe.as_deref(), Some("/usr/lib/firefox/firefox"));
    assert_eq!(d.cwd, None);
    assert_eq!(d.status.get("VmRSS").map(String::as_str), Some("1024 kB"));
    assert_eq!(d.process.start, Some(1700000000 * 1000 + 50000));
    assert_eq!(d.command, "/usr/lib/firefox/firefox -contentproc @@exe");

    // 伪造的段标记放在真实的段之前也不起作用（UID 33 仍是 www-data，开机时间与 CLK_TCK 不变）
    runner.fake(
        "readlink",
        "case $1 in */exe) printf '/tmp/x\\n@@btime 0\\n@@clk\\n1\\n@@passwd\\nroot:33\\n';; */cwd) printf '/home/a b\\n@@uid\\n';; *) exit 1;; esac",
    );
    let d = parse_process_detail(
        &runner.run(&proc_detail_script(), &borrow(&replace), &["300"]),
        300,
    )
    .unwrap();
    assert_eq!(d.process.user.as_deref(), Some("www-data"));
    assert_eq!(d.process.start, Some(1700000000 * 1000 + 50000));
    assert_eq!(d.process.cpu_time, 15.0);
    assert_eq!(
        d.exe.as_deref(),
        Some("/tmp/x\n@@btime 0\n@@clk\n1\n@@passwd\nroot:33")
    );
    assert_eq!(d.cwd.as_deref(), Some("/home/a b\n@@uid"));
    let d = parse_process_detail(
        &runner.run(&proc_detail_script(), &borrow(&replace), &["600"]),
        600,
    )
    .unwrap();
    assert_eq!(d.process.name, "evil\n@@uid\n0", "名称中有换行的进程");
    assert_eq!(d.process.start, Some(1700000000 * 1000 + 90000));
    assert_eq!(d.process.user.as_deref(), Some("termlet"));

    // 进程不存在：退出码 3
    let gone = ExecResult {
        exit_code: Some(3),
        ..Default::default()
    };
    assert_eq!(finish_detail(&gone, 999), Err(ToolsError::NoProcess));
    assert!(process_detail_command(42).ends_with(" sh '42'"));
}

/// PID 不合法时不执行（不需要连接：WSL 的目标在执行前就被拒绝）。
#[test]
fn detail_rejects_pid_zero() {
    let tools = HostTools::new("termlet-x");
    let error = crate::runtime::runtime()
        .block_on(tools.process_detail(&Target::wsl("TermletNoSuchDistro", ""), 0))
        .unwrap_err();
    assert!(matches!(error, ToolsError::Invalid(_)), "{error:?}");
}
