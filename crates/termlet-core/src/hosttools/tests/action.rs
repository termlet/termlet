//! 写操作、日志、在终端中执行的命令与审计记录的测试。

use super::{ShRunner, fake_proc, posix};
use crate::hosttools::action::{MAX_IDS, MAX_TERMINAL_COMMAND, action_command, parse_action};
use crate::hosttools::scripts::{CONTAINER_SHELL, action_script, kill_script};
use crate::hosttools::sections::{PrivResult, PrivSudo};
use crate::hosttools::{
    Action, ContainerOp, Engine, ServiceOp, Signal, SudoState, TerminalRequest, ToolsError,
    audit_detail, display, terminal_command,
};
use crate::ssh::ExecResult;

const CID: &str = "3f4e8a1b2c9d0e1f2a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091";

fn kill(pid: u32, signal: Signal) -> Action {
    Action::Kill {
        pid,
        signal,
        start_ticks: None,
    }
}

fn service(op: ServiceOp, unit: &str) -> Action {
    Action::Service {
        op,
        unit: unit.into(),
    }
}

fn container(engine: Engine, op: ContainerOp, ids: &[&str]) -> Action {
    Action::Container {
        engine,
        op,
        ids: ids.iter().map(|s| s.to_string()).collect(),
    }
}

fn result(stdout: &str) -> ExecResult {
    ExecResult {
        exit_code: Some(0),
        stdout: stdout.as_bytes().to_vec(),
        ..Default::default()
    }
}

/// 合法的操作与拼出的命令；不合法的参数（注入、选项、名称代替 ID、空列表、过多）。
#[test]
fn action_argv() {
    let short = &CID[..12];
    let sha = format!("sha256:{CID}");
    for (action, want) in [
        (kill(1234, Signal::Term), "kill -TERM 1234".to_string()),
        (kill(1, Signal::Kill), "kill -KILL 1".into()),
        (
            service(ServiceOp::Restart, "nginx.service"),
            "systemctl restart -- nginx.service".into(),
        ),
        (
            service(ServiceOp::Stop, r"systemd-fsck@dev-disk-by\x2duuid.service"),
            r"systemctl stop -- 'systemd-fsck@dev-disk-by'\\'x2duuid.service'".into(),
        ),
        (
            service(ServiceOp::Enable, "getty@tty1.service"),
            "systemctl enable -- getty@tty1.service".into(),
        ),
        // 定时器与服务一样启停、开机启用
        (
            service(ServiceOp::Stop, "backup.timer"),
            "systemctl stop -- backup.timer".into(),
        ),
        (
            service(ServiceOp::Disable, "logrotate.timer"),
            "systemctl disable -- logrotate.timer".into(),
        ),
        (
            container(Engine::Docker, ContainerOp::Remove, &[CID]),
            format!("docker rm {CID}"),
        ),
        (
            container(Engine::Podman, ContainerOp::Restart, &[CID, short]),
            format!("podman restart {CID} {short}"),
        ),
        (
            Action::RemoveImages {
                engine: Engine::Docker,
                ids: vec![sha.clone()],
            },
            format!("docker rmi {sha}"),
        ),
        (
            Action::RemoveVolumes {
                engine: Engine::Docker,
                names: vec!["shop_data".into()],
            },
            "docker volume rm shop_data".into(),
        ),
        (
            Action::RemoveNetworks {
                engine: Engine::Docker,
                ids: vec![CID.into()],
            },
            format!("docker network rm {CID}"),
        ),
    ] {
        assert_eq!(action.command_line(), Ok(want), "{action:?}");
    }

    let with_option = format!("{CID} --rm");
    let bad = [
        kill(0, Signal::Term),
        service(ServiceOp::Restart, "nginx.service; rm -rf /"),
        service(ServiceOp::Restart, "--root=/tmp/x.service"),
        service(ServiceOp::Restart, "nginx"),
        service(ServiceOp::Restart, "$(id).service"),
        service(ServiceOp::Stop, "backup.socket"),
        service(ServiceOp::Stop, "x.timer; reboot"),
        container(Engine::Docker, ContainerOp::Stop, &["web-1"]),
        container(Engine::Docker, ContainerOp::Stop, &[&with_option]),
        container(Engine::Docker, ContainerOp::Stop, &[]),
        Action::RemoveVolumes {
            engine: Engine::Docker,
            names: vec!["-f".into()],
        },
        Action::RemoveVolumes {
            engine: Engine::Docker,
            names: vec!["a b".into()],
        },
        Action::RemoveImages {
            engine: Engine::Docker,
            ids: vec!["nginx:latest".into()],
        },
    ];
    for action in bad {
        assert!(
            matches!(action.argv(), Err(ToolsError::Invalid(_))),
            "应拒绝 {action:?}"
        );
    }
    let many = container(Engine::Docker, ContainerOp::Stop, &vec![CID; MAX_IDS + 1]);
    assert!(many.argv().is_err(), "一次最多 MAX_IDS 个");
    let max = container(Engine::Docker, ContainerOp::Stop, &vec![CID; MAX_IDS]);
    assert!(max.argv().is_ok());
}

/// 跟随日志、进入容器、在终端中执行（sudo 或 su -c）；不合法的参数。
#[test]
fn terminal_commands() {
    let cases = [
        (
            TerminalRequest::ServiceLogs {
                unit: "nginx.service".into(),
                sudo: false,
            },
            "journalctl -u nginx.service -f -n 200".to_string(),
        ),
        (
            TerminalRequest::ServiceLogs {
                unit: "nginx.service".into(),
                sudo: true,
            },
            "sudo journalctl -u nginx.service -f -n 200".into(),
        ),
        (
            TerminalRequest::ContainerLogs {
                engine: Engine::Docker,
                id: CID.into(),
                sudo: false,
            },
            format!("docker logs -f --tail 200 {CID}"),
        ),
        (
            TerminalRequest::ContainerLogs {
                engine: Engine::Podman,
                id: CID.into(),
                sudo: true,
            },
            format!("sudo podman logs -f --tail 200 {CID}"),
        ),
        (
            TerminalRequest::Action {
                action: service(ServiceOp::Restart, "nginx.service"),
                root: Some(SudoState::Password),
                su: false,
            },
            "sudo systemctl restart -- nginx.service".into(),
        ),
        (
            TerminalRequest::Action {
                action: kill(42, Signal::Kill),
                root: Some(SudoState::NotInstalled),
                su: false,
            },
            "su -c 'kill -KILL 42'".into(),
        ),
        (
            TerminalRequest::Action {
                action: Action::RemoveVolumes {
                    engine: Engine::Docker,
                    names: vec!["shop_data".into()],
                },
                root: Some(SudoState::Denied),
                su: false,
            },
            "su -c 'docker volume rm shop_data'".into(),
        ),
        (
            TerminalRequest::Action {
                action: service(ServiceOp::Stop, "nginx.service"),
                root: Some(SudoState::Password),
                su: true,
            },
            "su -c 'systemctl stop -- nginx.service'".into(),
        ),
        (
            TerminalRequest::ContainerShell {
                engine: Engine::Docker,
                id: CID.into(),
                sudo: false,
            },
            format!("docker exec -it {CID} sh -c '{CONTAINER_SHELL}'"),
        ),
    ];
    for (request, want) in cases {
        assert_eq!(terminal_command(&request), Ok(want), "{request:?}");
    }
    for request in [
        TerminalRequest::ServiceLogs {
            unit: "a.service\nrm -rf /".into(),
            sudo: false,
        },
        TerminalRequest::ContainerShell {
            engine: Engine::Docker,
            id: "web".into(),
            sudo: false,
        },
        TerminalRequest::Action {
            action: kill(0, Signal::Term),
            root: None,
            su: false,
        },
    ] {
        assert!(
            matches!(terminal_command(&request), Err(ToolsError::Invalid(_))),
            "应拒绝 {request:?}"
        );
    }
    // 拼出的命令超过新终端能接受的长度
    let ids = vec![CID.to_string(); MAX_IDS];
    let long = TerminalRequest::Action {
        action: Action::Container {
            engine: Engine::Docker,
            op: ContainerOp::Stop,
            ids,
        },
        root: None,
        su: false,
    };
    assert!(
        terminal_command(&long).is_err(),
        "超过 {MAX_TERMINAL_COMMAND} 字节应拒绝"
    );
}

/// 在本机的 sh 中执行进入容器的命令行，引号保持原样（容器中的 sh 收到的脚本与
/// CONTAINER_SHELL 相同）。
#[test]
fn terminal_command_quoting() {
    let Some(runner) = ShRunner::isolated() else {
        return;
    };
    runner.fake("docker", r#"printf '%s|' "$@""#);
    let command = terminal_command(&TerminalRequest::ContainerShell {
        engine: Engine::Docker,
        id: CID.into(),
        sudo: false,
    })
    .unwrap();
    let out = runner.run(&command, &[], &[]);
    assert_eq!(out, format!("exec|-it|{CID}|sh|-c|{CONTAINER_SHELL}|"));
}

/// 先直接执行；权限不足时经免密码 sudo 再执行；sudo 要密码、没有 sudo 时报告；
/// 不是权限问题的失败不试 sudo；sudo 可用但命令本身失败时报告命令的输出。参数原样到达命令。
#[test]
fn action_script_priv() {
    let Some(runner) = ShRunner::isolated() else {
        return;
    };
    runner.fake("id", "echo 1000");
    // 假的 systemctl：root（经 sudo）才能执行，打印收到的参数
    runner.fake(
        "systemctl",
        r#"if [ "$SUDO_RUN" = 1 ]; then printf 'ok:'; printf '%s|' "$@"; exit 0; fi
echo "Failed to restart x.service: Interactive authentication required." >&2; exit 1"#,
    );
    let argv = service(ServiceOp::Restart, r"weird\x2dname.service")
        .argv()
        .unwrap();
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let run = || PrivResult::parse(&runner.run(&action_script(), &[], &argv)).unwrap();

    runner.fake("sudo", r#"[ "$1" = -n ] && shift; SUDO_RUN=1 exec "$@""#);
    let pr = run();
    assert_eq!(pr.code, 0);
    assert_eq!(pr.sudo, PrivSudo::Used, "应经 sudo 执行");
    assert_eq!(pr.output, r"ok:restart|--|weird\x2dname.service|");

    runner.fake("sudo", r#"echo "sudo: a password is required" >&2; exit 1"#);
    let pr = run();
    assert_ne!(pr.code, 0);
    assert_eq!(pr.sudo, PrivSudo::Root(SudoState::Password), "sudo 要密码");
    assert!(pr.root_failure().is_some());

    runner.remove("sudo");
    let pr = run();
    assert_eq!(
        pr.sudo,
        PrivSudo::Root(SudoState::NotInstalled),
        "没有 sudo"
    );
    assert!(pr.output.contains("Interactive authentication required"));

    // 不是权限问题的失败：不试 sudo，原样返回
    runner.fake(
        "systemctl",
        r#"echo "Job for x.service failed because the control process exited with error code." >&2; exit 1"#,
    );
    runner.fake("sudo", "echo SHOULD-NOT-RUN; exit 0");
    let pr = run();
    assert_eq!((pr.code, pr.sudo), (1, PrivSudo::NotTried), "其他失败");
    assert!(pr.output.contains("control process exited"));

    // sudo 可用但命令本身失败：报告命令的输出
    runner.fake(
        "systemctl",
        r#"if [ "$SUDO_RUN" = 1 ]; then echo "Unit x.service not found." >&2; exit 5; fi
echo "Access denied" >&2; exit 1"#,
    );
    runner.fake("sudo", r#"[ "$1" = -n ] && shift; SUDO_RUN=1 exec "$@""#);
    let pr = run();
    assert_eq!((pr.code, pr.sudo), (5, PrivSudo::Used), "sudo 执行后失败");
    assert_eq!(pr.output, "Unit x.service not found.");
    assert!(pr.root_failure().is_none());
}

/// 结束进程前核对启动时刻，一致时执行；PID 已被别的进程重新使用（启动时刻不同）、
/// 进程已不存在时不执行。名称中有括号、空格、换行的进程也能正确取到启动时刻。用 echo 代替 kill
/// （kill 是 sh 的内置命令，不能换成假命令）。
#[test]
fn kill_script_checks_start_ticks() {
    let Some(runner) = ShRunner::isolated() else {
        return;
    };
    runner.fake("id", "echo 1000");
    let dir = tempfile::tempdir().unwrap();
    fake_proc(dir.path());
    let proc_dir = format!("{}/proc", posix(dir.path()));
    let replace = [("/proc", proc_dir.as_str())];
    let run = |pid: &str, ticks: &str| {
        runner.run(&kill_script(), &replace, &[pid, ticks, "echo", "ran", pid])
    };
    for (pid, ticks) in [("300", "5000"), ("301", "6000"), ("600", "9000")] {
        let pr = PrivResult::parse(&run(pid, ticks)).unwrap();
        assert_eq!(
            (pr.code, pr.output.as_str()),
            (0, format!("ran {pid}").as_str()),
            "{pid} 启动时刻一致时应执行"
        );
    }
    assert_eq!(
        run("300", "4999").trim(),
        "@@gone",
        "启动时刻不同时不应执行"
    );
    assert_eq!(run("999", "5000").trim(), "@@gone", "进程不存在时不应执行");
}

/// PID 与启动时刻在命令之前传入；进程已结束或已被替换时不执行；没有启动时刻时不核对。
#[test]
fn act_with_start_ticks() {
    let action = Action::Kill {
        pid: 42,
        signal: Signal::Term,
        start_ticks: Some(777),
    };
    let command = action_command(&action).unwrap();
    assert!(
        command.ends_with(" sh '42' '777' 'kill' '-TERM' '42'"),
        "应带上启动时刻：{}",
        &command[command.len() - 60..]
    );
    let done = parse_action(&action, &result("@@rc 0\n@@sudo \n@@out\n")).unwrap();
    assert!(done.ok);
    assert_eq!(done.command, "kill -TERM 42");
    let gone = Action::Kill {
        pid: 42,
        signal: Signal::Kill,
        start_ticks: Some(777),
    };
    assert_eq!(
        parse_action(&gone, &result("@@gone\n")),
        Err(ToolsError::ProcessReplaced)
    );
    assert_eq!(gone.command_line().unwrap(), "kill -KILL 42");
    // 没有启动时刻：不核对
    let command = action_command(&kill(42, Signal::Term)).unwrap();
    assert!(command.ends_with(" sh 'kill' '-TERM' '42'"), "{command}");
}

/// 经 sudo 执行的结果；参数经 $1… 传入；需要 root；命令失败；没有输出；参数不合法。
#[test]
fn act_results() {
    let action = container(Engine::Docker, ContainerOp::Remove, &[CID]);
    let done = parse_action(&action, &result("@@rc 0\n@@sudo used\n@@out\nremoved\n")).unwrap();
    assert!(done.ok && done.sudo);
    assert_eq!(done.command, format!("sudo -n docker rm {CID}"));
    assert_eq!(done.output, "removed");
    let command = action_command(&action).unwrap();
    assert!(
        command.ends_with(&format!(" sh 'docker' 'rm' '{CID}'")),
        "参数经 $1… 传入"
    );

    match parse_action(
        &action,
        &result("@@rc 1\n@@sudo password\n@@out\npermission denied\n"),
    ) {
        Err(ToolsError::NeedRoot { sudo, output }) => {
            assert_eq!(sudo, SudoState::Password);
            assert_eq!(output, "permission denied");
        }
        other => panic!("应为需要 root：{other:?}"),
    }
    assert_eq!(action.command_line().unwrap(), format!("docker rm {CID}"));

    let failed = parse_action(
        &action,
        &result("@@rc 1\n@@sudo \n@@out\nError: No such container\n"),
    )
    .unwrap();
    assert!(!failed.ok);
    assert_eq!(failed.exit_code, 1);
    assert_eq!(failed.output, "Error: No such container");

    let empty = ExecResult {
        exit_code: Some(127),
        stderr: b"sh: not found".to_vec(),
        ..Default::default()
    };
    assert!(matches!(
        parse_action(&action, &empty),
        Err(ToolsError::Unreadable {
            exit_code: Some(127),
            ..
        })
    ));
    assert!(matches!(
        action_command(&kill(0, Signal::Term)),
        Err(ToolsError::Invalid(_))
    ));
}

/// 去掉控制字符与双向文字控制符；没有名称；名称截断。
#[test]
fn audit_details() {
    assert_eq!(
        audit_detail("kill -TERM 12", "nginx\x1b[31m\u{202e}"),
        "kill -TERM 12  # nginx[31m"
    );
    assert_eq!(audit_detail("kill -TERM 12", ""), "kill -TERM 12");
    let long = audit_detail("x", &"名".repeat(200));
    assert_eq!(long.chars().count(), "x  # ".len() + 120 + 1, "名称应截断");
    assert!(long.ends_with('…'));
}

/// 没有 @@rc 时失败；命令的输出中有 @@ 开头的行不影响前面的结果。
#[test]
fn parse_priv() {
    assert_eq!(PrivResult::parse("garbage"), None);
    let pr = PrivResult::parse("@@rc 0\n@@sudo \n@@out\n@@rc 9\n@@sudo used\n").unwrap();
    assert_eq!((pr.code, pr.sudo), (0, PrivSudo::NotTried));
    assert_eq!(pr.output, "@@rc 9\n@@sudo used");
}

/// 命令写成一行：常见字符原样，其余加引号。
#[test]
fn display_quotes_when_needed() {
    assert_eq!(display(&["kill", "-TERM", "1"]), "kill -TERM 1");
    assert_eq!(display(&["echo", "a b", "it's"]), r"echo 'a b' 'it'\''s'");
}

/// 确认框写出的命令与审计记录的写法一致：经免密码 sudo 时前面是 `sudo -n`。
#[test]
fn planned_command_matches_the_audit() {
    let restart = Action::Service {
        op: ServiceOp::Restart,
        unit: "demo-app.service".into(),
    };
    assert_eq!(
        restart.command_line_with(true).unwrap(),
        "sudo -n systemctl restart -- demo-app.service"
    );
    assert_eq!(
        restart.command_line_with(false).unwrap(),
        restart.command_line().unwrap()
    );
    let parsed = parse_action(&restart, &result("@@rc 0\n@@sudo used\n@@out\n")).unwrap();
    assert_eq!(parsed.command, restart.command_line_with(true).unwrap());
}
