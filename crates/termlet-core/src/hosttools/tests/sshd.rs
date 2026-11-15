//! 对真实 sshd 的集成测试：设置 `TERMLET_TEST_SSH=127.0.0.1:2222` 后运行，否则跳过。
//! e2e 的测试容器是 Alpine：sh 是 BusyBox 的 ash，没有 ss、systemd、docker、sudo，有 tmux。

use std::time::{Duration, Instant};

use termlet_io::{GridSize, IoEvent};

use crate::SESSION_ENV;
use crate::hosttools::exec::{Limits, run, sh_script};
use crate::hosttools::{
    Action, ConnSource, Engine, ExecError, HostTools, LogQuery, LogSource, PROCESS_INTERVAL,
    Privilege, ProcessList, Signal, SudoState, Target, ToolsError,
};
use crate::ssh::{ShellOptions, exec, open_shell};
use crate::testkit::{connect_test, test_server};

/// 在 `target` 上一直读进程，直到 `found` 为真（最多 10 秒）。
async fn wait_for(
    tools: &HostTools,
    target: &Target,
    found: impl Fn(&ProcessList) -> bool,
) -> ProcessList {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let list = tools.processes(target, PROCESS_INTERVAL).await.unwrap();
        if found(&list) {
            return list;
        }
        assert!(Instant::now() < deadline, "没有等到：{list:?}");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// 进程（这个终端的 shell、CPU、详情、核对启动时刻后结束、结束 root 的进程）、网络、没有 systemd 与 docker。
#[test]
fn tools_on_real_server() {
    let Some((host, port)) = test_server() else {
        return;
    };
    crate::runtime::runtime().block_on(async {
        let connection = connect_test(&host, port).await;
        let target = Target::ssh(connection.clone(), false);
        // 这个终端：打开一个带会话标记的 shell
        let key = format!("termlet-it{}", crate::id::random_token(4).to_lowercase());
        let tools = HostTools::new(&key);
        let shell = open_shell(
            connection.clone(),
            ShellOptions {
                size: GridSize::new(80, 24),
                env: vec![(SESSION_ENV.to_string(), key.clone())],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // 在后台启动一个进程，用来结束
        exec(
            &connection,
            "nohup sleep 321 >/dev/null 2>&1 &",
            1024,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        let list = wait_for(&tools, &target, |list| {
            let sleep = list
                .processes
                .iter()
                .any(|p| p.name == "sleep" && p.uid == list.uid);
            let shell = list
                .processes
                .iter()
                .any(|p| p.terminal_shell && p.in_terminal);
            sleep && shell
        })
        .await;
        // 同时运行的别的测试也可能有 sleep：按命令行找到自己启动的那个
        let mut sleep = None;
        for p in list
            .processes
            .iter()
            .filter(|p| p.name == "sleep" && p.uid == list.uid)
        {
            if let Ok(detail) = tools.process_detail(&target, p.pid).await
                && detail.command == "sleep 321"
            {
                sleep = Some(p.clone());
                break;
            }
        }
        let sleep = sleep.expect("没有找到 sleep 321");
        assert!(list.uid.is_some_and(|uid| uid != 0), "{:?}", list.uid);
        assert!(list.total >= list.processes.len());
        assert!(list.mem_total > 0 && list.cores > 0);
        assert!(
            list.processes
                .iter()
                .any(|p| p.user.as_deref() == Some("root")),
            "应能看到 root 的进程（读 status 的 Uid）"
        );
        // 第二次读取后有 CPU
        tokio::time::sleep(Duration::from_millis(500)).await;
        let again = tools.processes(&target, PROCESS_INTERVAL).await.unwrap();
        assert!(again.processes[0].cpu.is_some(), "第二次应有 CPU");

        let detail = tools.process_detail(&target, sleep.pid).await.unwrap();
        assert_eq!(detail.command, "sleep 321");
        assert_eq!(detail.status.get("Threads").map(String::as_str), Some("1"));
        // 启动时刻与列表中的不同（PID 被重新使用）：不执行
        let replaced = tools
            .act(
                &target,
                Action::Kill {
                    pid: sleep.pid,
                    signal: Signal::Term,
                    start_ticks: Some(sleep.start_ticks + 1),
                },
            )
            .await;
        assert_eq!(replaced, Err(ToolsError::ProcessReplaced));
        let done = tools
            .act(
                &target,
                Action::Kill {
                    pid: sleep.pid,
                    signal: Signal::Term,
                    start_ticks: Some(sleep.start_ticks),
                },
            )
            .await
            .unwrap();
        assert!(done.ok, "{done:?}");
        assert_eq!(done.command, format!("kill -TERM {}", sleep.pid));
        let gone = tools.process_detail(&target, sleep.pid).await;
        assert_eq!(gone, Err(ToolsError::NoProcess), "进程应已结束");
        // 结束 PID 1（root 的进程）：没有权限，容器中没有 sudo
        let root = tools
            .act(
                &target,
                Action::Kill {
                    pid: 1,
                    signal: Signal::Term,
                    start_ticks: None,
                },
            )
            .await;
        assert!(
            matches!(
                root,
                Err(ToolsError::NeedRoot {
                    sudo: SudoState::NotInstalled,
                    ..
                })
            ),
            "应为需要 root（没有 sudo）：{root:?}"
        );

        let conns = tools.connections(&target).await.unwrap();
        let listen = conns
            .conns
            .iter()
            .any(|c| c.proto == "tcp" && c.state == "LISTEN" && c.local_port == 22);
        let established = conns
            .conns
            .iter()
            .any(|c| c.proto == "tcp" && c.state == "ESTAB" && c.local_port == 22);
        assert!(listen && established, "{conns:?}");
        assert_eq!(conns.source, ConnSource::Proc, "容器中没有 ss");
        assert!(conns.total >= conns.conns.len());
        assert!(!conns.sudo);
        // 权限：普通用户、没有 sudo；同一条连接只探测一次（第二次直接返回记下的结果）
        assert_eq!(
            tools.privilege(&target).await,
            Ok(Privilege::Limited(SudoState::NotInstalled))
        );
        let started = Instant::now();
        assert_eq!(
            tools.privilege(&target).await,
            Ok(Privilege::Limited(SudoState::NotInstalled))
        );
        assert!(started.elapsed() < Duration::from_millis(50), "记下的结果");

        assert_eq!(tools.services(&target).await, Err(ToolsError::NoSystemd));
        assert_eq!(tools.docker(&target).await, Err(ToolsError::NoDocker));
        assert_eq!(
            tools.container_names(&target).await,
            Err(ToolsError::NoDocker)
        );

        // 读取的脚本在 BusyBox 上：tail -c 可用，最后一行没有换行时退出码仍在新的一行；没有 journalctl、docker 时说明
        let command = sh_script(
            &crate::hosttools::exec::one_line(&crate::hosttools::scripts::read_script()),
            &["sh", "-c", "printf 'a\\nb'; exit 3"],
        );
        let limits = Limits {
            timeout: Duration::from_secs(10),
            output: 1 << 16,
            long: false,
        };
        let result = run(&target, &command, limits).await.unwrap();
        let out = String::from_utf8_lossy(&result.stdout);
        let read = crate::hosttools::logs::ReadOutput::parse(&out, result.stdout.len()).unwrap();
        assert_eq!(
            (read.code, read.output.as_str(), read.cut),
            (3, "a\nb", false)
        );
        let logs = tools
            .logs(
                &target,
                LogQuery::recent(
                    LogSource::Service {
                        unit: "sshd.service".into(),
                    },
                    10,
                ),
            )
            .await;
        assert!(
            matches!(&logs, Err(ToolsError::LogsFailed(m)) if m.contains("not found")),
            "{logs:?}"
        );
        let stats = tools.container_stats(&target, Engine::Docker, false).await;
        assert!(
            matches!(stats, Err(ToolsError::DockerFailed(_))),
            "{stats:?}"
        );
        shell.input.close();
    });
}

/// tmux 终端：按以会话标识命名的 tmux 会话的窗格标出这个终端的 shell。
#[test]
fn tmux_pane_is_the_terminal_shell() {
    let Some((host, port)) = test_server() else {
        return;
    };
    crate::runtime::runtime().block_on(async {
        let connection = connect_test(&host, port).await;
        let target = Target::ssh(connection.clone(), false);
        let key = format!("termlet-tm{}", crate::id::random_token(4).to_lowercase());
        let tools = HostTools::new(&key);
        let shell = open_shell(
            connection.clone(),
            ShellOptions {
                size: GridSize::new(80, 24),
                command: Some(format!("exec tmux new-session -A -s {key}")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let list = wait_for(&tools, &target, |list| {
            list.processes.iter().any(|p| p.terminal_shell)
        })
        .await;
        let pane = list.processes.iter().find(|p| p.terminal_shell).unwrap();
        assert!(pane.in_terminal);
        assert_ne!(pane.name, "tmux: server", "{pane:?}");
        exec(
            &connection,
            &format!("tmux kill-session -t {key}"),
            1024,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        // 读完终端的输出直到结束，不留下通道
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, shell.output.recv()).await {
            if matches!(event, IoEvent::Closed(_)) {
                break;
            }
        }
    });
}

/// 只用一个通道的主机：不另开通道；超时；名额（长命令一次一个，短命令不被长命令挡住）。
#[test]
fn single_channel_timeout_and_slots() {
    let Some((host, port)) = test_server() else {
        return;
    };
    crate::runtime::runtime().block_on(async {
        let connection = connect_test(&host, port).await;
        let tools = HostTools::new("termlet-x");
        let single = Target::ssh(connection.clone(), true);
        assert_eq!(
            tools.processes(&single, PROCESS_INTERVAL).await,
            Err(ToolsError::Exec(ExecError::SingleChannel))
        );
        let target = Target::ssh(connection.clone(), false);
        let sleep = |seconds: u32, timeout: u64, long: bool| {
            let target = target.clone();
            async move {
                let limits = Limits {
                    timeout: Duration::from_secs(timeout),
                    output: 1024,
                    long,
                };
                run(
                    &target,
                    &sh_script(&format!("sleep {seconds}"), &[] as &[&str]),
                    limits,
                )
                .await
            }
        };
        assert_eq!(sleep(5, 1, false).await, Err(ExecError::Timeout));

        // 两个长命令依次执行，短命令同时进行
        let started = Instant::now();
        let (a, b, short) = tokio::join!(sleep(1, 20, true), sleep(1, 20, true), async {
            let result = sleep(1, 20, false).await;
            (result, started.elapsed())
        });
        assert!(a.is_ok() && b.is_ok() && short.0.is_ok());
        assert!(
            started.elapsed() >= Duration::from_secs(2),
            "长命令应一次一个：{:?}",
            started.elapsed()
        );
        assert!(
            short.1 < Duration::from_millis(1900),
            "短命令不应等长命令：{:?}",
            short.1
        );

        // 连接断开后
        connection.disconnect().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            tools.processes(&target, PROCESS_INTERVAL).await,
            Err(ToolsError::Exec(ExecError::Disconnected))
        );
    });
}
