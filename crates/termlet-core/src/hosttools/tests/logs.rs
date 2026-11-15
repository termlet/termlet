//! 日志的测试：命令的参数（行数、时间范围、级别、增量读取）、journalctl 与 docker 的输出与时间换算、脚本本身
//! （经 sudo、只留最新的部分、日志伪造不了标记）。

use super::ShRunner;
use crate::hosttools::logs::{logs_argv, logs_command, parse_logs, parse_time};
use crate::hosttools::scripts::{READ_TAIL, read_script, service_log_script};
use crate::hosttools::{
    Engine, LogCursor, LogLevel, LogLines, LogPriority, LogQuery, LogSource, MAX_LOG_LINES,
    ToolsError, level_of,
};
use crate::ssh::ExecResult;

const CID: &str = "3f4e8a1b2c9d0e1f2a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091";
const CURSOR: &str = "s=4b1c0a;i=1f2;b=9e8d7c;m=3a4b;t=5c6d;x=7e8f";

fn service(unit: &str) -> LogSource {
    LogSource::Service { unit: unit.into() }
}

fn container(sudo: bool) -> LogSource {
    LogSource::Container {
        engine: Engine::Docker,
        id: CID.into(),
        sudo,
    }
}

fn query(source: LogSource) -> LogQuery {
    LogQuery {
        source,
        lines: LogLines::Last(200),
        since_secs: None,
        priority: LogPriority::All,
        after: None,
    }
}

fn result(stdout: &str) -> ExecResult {
    ExecResult {
        exit_code: Some(0),
        stdout: stdout.as_bytes().to_vec(),
        ..Default::default()
    }
}

/// 2026-11-04 01:47:12 UTC 的 Unix 毫秒。
const AT: i64 = 1_793_756_832_000;

/// 服务：行数、级别、时间范围（相对的秒数，不与 -n 同时给出）、从游标之后读。
#[test]
fn service_arguments() {
    let argv = logs_argv(&query(service("nginx.service"))).unwrap();
    assert_eq!(
        argv,
        [
            "journalctl",
            "-u",
            "nginx.service",
            "--no-pager",
            "-q",
            "--utc",
            "-o",
            "short-iso",
            "--show-cursor",
            "-n",
            "200"
        ]
    );
    let mut q = query(service("nginx.service"));
    q.lines = LogLines::All;
    q.priority = LogPriority::Error;
    let argv = logs_argv(&q).unwrap();
    assert!(argv.ends_with(&[
        "-p".into(),
        "3".into(),
        "-n".into(),
        MAX_LOG_LINES.to_string()
    ]));
    q.priority = LogPriority::Warning;
    q.since_secs = Some(900);
    let argv = logs_argv(&q).unwrap();
    assert!(argv.ends_with(&["-p".into(), "4".into(), "--since=-900s".into()]));
    assert!(!argv.contains(&"-n".to_string()), "有时间范围时不用 -n");
    q.after = Some(LogCursor::journal(CURSOR));
    let argv = logs_argv(&q).unwrap();
    assert_eq!(argv.last().unwrap(), &format!("--after-cursor={CURSOR}"));
    assert!(!argv.iter().any(|a| a.starts_with("--since") || a == "-n"));
    // 经 sh -c 执行，服务名作为参数
    let command = logs_command(&query(service("nginx.service"))).unwrap();
    assert!(command.starts_with("sh -c 'export LC_ALL=C;"));
    assert!(command.ends_with(
        " 'nginx.service' '--no-pager' '-q' '--utc' '-o' 'short-iso' '--show-cursor' '-n' '200'"
    ));
}

/// 容器：经 sudo 时以 sudo -n 开头；行数与时间范围可以同时给出；增量读取从上次最后一条之后（RFC 3339 纳秒）。
#[test]
fn container_arguments() {
    let argv = logs_argv(&query(container(false))).unwrap();
    assert_eq!(
        argv,
        ["docker", "logs", "--timestamps", "--tail", "200", CID]
    );
    let mut q = query(container(true));
    q.since_secs = Some(3600);
    let argv = logs_argv(&q).unwrap();
    assert_eq!(
        argv,
        [
            "sudo",
            "-n",
            "docker",
            "logs",
            "--timestamps",
            "--tail",
            "200",
            "--since",
            "3600s",
            CID
        ]
    );
    q.lines = LogLines::All;
    q.since_secs = None;
    assert!(
        !logs_argv(&q).unwrap().contains(&"--tail".to_string()),
        "全部时不给 --tail"
    );
    q.after = Some(LogCursor::docker(AT * 1_000_000 + 944_387_787));
    let argv = logs_argv(&q).unwrap();
    assert!(argv.ends_with(&[
        "--tail".into(),
        "5000".into(),
        "--since".into(),
        "2026-11-04T01:47:12.944387788Z".into(),
        CID.into()
    ]));
}

/// 不合法的参数不执行。
#[test]
fn invalid_queries() {
    let mut bad = vec![
        query(service("x; id")),
        query(service("nginx")),
        LogQuery {
            lines: LogLines::Last(0),
            ..query(service("nginx.service"))
        },
        LogQuery {
            lines: LogLines::Last(MAX_LOG_LINES + 1),
            ..query(service("nginx.service"))
        },
        query(LogSource::Container {
            engine: Engine::Docker,
            id: "web".into(),
            sudo: false,
        }),
    ];
    let mut q = query(service("nginx.service"));
    q.after = Some(LogCursor::journal("s=1; rm -rf /"));
    bad.push(q.clone());
    q.after = Some(LogCursor::docker(1));
    bad.push(q);
    let mut q = query(container(false));
    q.after = Some(LogCursor::journal(CURSOR));
    bad.push(q);
    for q in bad {
        assert!(
            matches!(logs_command(&q), Err(ToolsError::Invalid(_))),
            "应拒绝 {q:?}"
        );
    }
}

/// 带时区的时间：旧的 journalctl 写 +0000，新的写 +00:00；docker 是纳秒的 Z，podman 带本地的时区。
#[test]
fn times_with_zones() {
    let ns = |ms: i64| ms * 1_000_000;
    assert_eq!(parse_time("2026-11-04T01:47:12+0000"), Some(ns(AT)));
    assert_eq!(parse_time("2026-11-04T01:47:12+00:00"), Some(ns(AT)));
    assert_eq!(parse_time("2026-11-04T09:47:12+0800"), Some(ns(AT)));
    assert_eq!(parse_time("2026-11-04T01:47:12Z"), Some(ns(AT)));
    assert_eq!(
        parse_time("2026-11-04T01:47:12.944387787Z"),
        Some(ns(AT) + 944_387_787)
    );
    assert_eq!(
        parse_time("2026-11-04T09:47:12.5+08:00"),
        Some(ns(AT) + 500_000_000)
    );
    for bad in [
        "",
        "Nov 04 01:47:12",
        "2026-11-04 01:47:12",
        "2026-11-04T01:47:12",
    ] {
        assert_eq!(parse_time(bad), None, "{bad}");
    }
}

/// journalctl 的输出：时间换成 Unix 毫秒、去掉主机名；多行消息的后续行对齐到消息开始的位置，级别跟着第一行；
/// 说明行跳过；最后一行是游标；经 sudo 读到。
#[test]
fn journal_output() {
    // 后续行缩进到原文中消息开始的位置（时间、主机与“app[7]: ”共 38 列）
    let out = format!(
        "@@sudo used 1000\n\
         2026-11-04T01:47:12+0000 web nginx[123]: started\n\
         2026-11-04T01:47:13+00:00 web app[7]: ERROR boom\n\
         {}at main.rs:1\n\
         -- Boot 1234 --\n\
         2026-11-04T01:47:14+0000 web app[7]: warning: disk almost full\n\
         -- cursor: {CURSOR}\n\
         \n@@rc 0\n",
        " ".repeat(38)
    );
    let logs = parse_logs(&query(service("app.service")), &result(&out)).unwrap();
    assert!(logs.sudo);
    assert!(!logs.limited);
    assert!(!logs.truncated);
    assert_eq!(logs.cursor, Some(LogCursor::journal(CURSOR)));
    let texts: Vec<&str> = logs.lines.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(
        texts,
        [
            "nginx[123]: started",
            "app[7]: ERROR boom",
            "        at main.rs:1",
            "app[7]: warning: disk almost full"
        ]
    );
    let at: Vec<Option<i64>> = logs.lines.iter().map(|l| l.at).collect();
    assert_eq!(at, [Some(AT), Some(AT + 1000), None, Some(AT + 2000)]);
    let levels: Vec<LogLevel> = logs.lines.iter().map(|l| l.level).collect();
    assert_eq!(
        levels,
        [
            LogLevel::Normal,
            LogLevel::Error,
            LogLevel::Error,
            LogLevel::Warning
        ]
    );
    // 没有 sudo；没有新的日志时游标沿用上一次的
    let mut q = query(service("app.service"));
    q.after = Some(LogCursor::journal(CURSOR));
    let logs = parse_logs(&q, &result("@@sudo  1000\n\n@@rc 0\n")).unwrap();
    assert!(!logs.sudo);
    assert!(logs.limited, "普通用户读取：只能看到一部分");
    assert!(logs.lines.is_empty());
    let logs = parse_logs(&q, &result("@@sudo  1000 journal\n\n@@rc 0\n")).unwrap();
    assert!(!logs.limited, "在 systemd-journal 等组中：能看到全部");
    let logs = parse_logs(&q, &result("@@sudo  0\n\n@@rc 0\n")).unwrap();
    assert!(!logs.sudo && !logs.limited, "root 能看到全部");
    // 一个日志文件也打不开：不算失败，因为权限看不到
    let denied = "@@sudo  1000 journal\n\
                  No journal files were opened due to insufficient permissions.\n\
                  \n@@rc 1\n";
    let logs = parse_logs(&q, &result(denied)).unwrap();
    assert!(logs.limited && logs.lines.is_empty(), "{logs:?}");
    assert_eq!(logs.cursor, Some(LogCursor::journal(CURSOR)));
    // journalctl 出错
    assert!(matches!(
        parse_logs(
            &query(service("app.service")),
            &result("@@sudo \nFailed to add match 'x': Invalid argument\n\n@@rc 1\n")
        ),
        Err(ToolsError::LogsFailed(message)) if message.contains("Invalid argument")
    ));
    // 没有标记：脚本没能执行
    assert!(matches!(
        parse_logs(&query(service("app.service")), &result("garbage\n")),
        Err(ToolsError::Unreadable { .. })
    ));
}

/// 有时间范围时服务器上没有按行数取：客户端只留最后 n 行，不从多行消息的中间开始。
#[test]
fn journal_since_keeps_the_last_lines() {
    let mut out = String::from("@@sudo \n");
    for i in 0..5 {
        out.push_str(&format!(
            "2026-11-04T01:47:1{i}+0000 web app[7]: line {i}\n"
        ));
        out.push_str("                         more\n");
    }
    out.push_str("\n@@rc 0\n");
    let mut q = query(service("app.service"));
    q.lines = LogLines::Last(3);
    q.since_secs = Some(60);
    let logs = parse_logs(&q, &result(&out)).unwrap();
    assert_eq!(logs.lines.len(), 2);
    assert_eq!(logs.lines[0].text, "app[7]: line 4");
}

/// docker 的输出：纳秒时间、TTY 的 \r；游标是最后一条的时间；增量读取去掉不晚于它的。
#[test]
fn docker_output() {
    let out = "2026-11-04T01:47:12.000000001Z hello\r\n\
               2026-11-04T09:47:13+08:00 Error: oops\n\
               no time here\n\
               \n@@rc 0\n";
    let logs = parse_logs(&query(container(true)), &result(out)).unwrap();
    assert!(logs.sudo, "容器经 sudo 读取时照实说明");
    let texts: Vec<&str> = logs.lines.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(texts, ["hello", "Error: oops", "no time here"]);
    assert_eq!(logs.lines[0].at, Some(AT));
    assert_eq!(logs.lines[1].at, Some(AT + 1000));
    assert_eq!(logs.lines[1].level, LogLevel::Error);
    let last = (AT + 1000) * 1_000_000;
    assert_eq!(logs.cursor, Some(LogCursor::docker(last)));

    let mut q = query(container(false));
    q.after = Some(LogCursor::docker(last));
    let again = "2026-11-04T01:47:13Z Error: oops\n2026-11-04T01:47:14Z new\n\n@@rc 0\n";
    let logs = parse_logs(&q, &result(again)).unwrap();
    assert_eq!(logs.lines.len(), 1);
    assert_eq!(logs.lines[0].text, "new");
    assert_eq!(
        logs.cursor,
        Some(LogCursor::docker((AT + 2000) * 1_000_000))
    );
    // 没有新的：游标不变
    let logs = parse_logs(&q, &result("\n@@rc 0\n")).unwrap();
    assert_eq!(logs.cursor, Some(LogCursor::docker(last)));

    assert!(matches!(
        parse_logs(
            &query(container(false)),
            &result("Error response from daemon: No such container: x\n\n@@rc 1\n")
        ),
        Err(ToolsError::LogsFailed(message)) if message.contains("No such container")
    ));
    assert!(matches!(
        parse_logs(
            &query(container(false)),
            &result(
                "Cannot connect to the Docker daemon at unix:///var/run/docker.sock.\n\n@@rc 1\n"
            )
        ),
        Err(ToolsError::DockerDown)
    ));
}

/// docker 分别写出标准输出与标准错误，交错时先后不按时间：按时间排好，游标取最晚的，下一次不会重复读到。
#[test]
fn docker_streams_out_of_order() {
    let out = "2026-11-04T01:47:13Z error: job 9 failed\n2026-11-04T01:47:12Z processed job 9\n\n@@rc 0\n";
    let logs = parse_logs(&query(container(false)), &result(out)).unwrap();
    let texts: Vec<&str> = logs.lines.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(texts, ["processed job 9", "error: job 9 failed"]);
    assert_eq!(
        logs.cursor,
        Some(LogCursor::docker((AT + 1000) * 1_000_000))
    );
}

/// 颜色等终端控制序列去掉（之后再按词判断级别），其余控制字符换成 U+FFFD，制表符保留。
#[test]
fn control_sequences_are_removed() {
    let out =
        "2026-11-04T01:47:12Z \x1b[31mERROR\x1b[0m\tdisk\x07 \x1b]0;title\x07done\n\n@@rc 0\n";
    let logs = parse_logs(&query(container(false)), &result(out)).unwrap();
    assert_eq!(logs.lines[0].text, "ERROR\tdisk\u{FFFD} done");
    assert_eq!(logs.lines[0].level, LogLevel::Error);
}

/// 错误、警告按整词判断，不区分大小写。
#[test]
fn levels_by_keyword() {
    assert_eq!(level_of("[error] open() failed"), LogLevel::Error);
    assert_eq!(level_of("FATAL: no"), LogLevel::Error);
    assert_eq!(level_of("level=warn msg=slow"), LogLevel::Warning);
    assert_eq!(level_of("[notice] start worker"), LogLevel::Normal);
    assert_eq!(level_of("stderr redirected; terror"), LogLevel::Normal);
}

/// 读服务日志的脚本：不是 root 且免密码 sudo 能执行 journalctl 时经 sudo 读；没有 sudo 时以当前用户读。
/// 日志中写的 `@@sudo`、`@@rc` 只是内容；最后一行没有换行时退出码仍在新的一行。
#[test]
fn service_log_script_on_fake_journalctl() {
    let Some(runner) = ShRunner::isolated() else {
        return;
    };
    runner.fake("id", "echo 1000");
    runner.fake(
        "journalctl",
        r#"[ "$1" = --version ] && { echo 'systemd 255'; exit 0; }
echo "2026-11-04T01:47:12+0000 web app[1]: args $*"
echo "@@rc 9"
echo "@@sudo used"
printf '2026-11-04T01:47:13+0000 web app[1]: no newline'"#,
    );
    runner.fake("sudo", r#"[ "$1" = -n ] && shift; SUDO_RUN=1 "$@""#);
    let q = query(service("app.service"));
    let argv = logs_argv(&q).unwrap();
    let args: Vec<&str> = argv.iter().map(String::as_str).collect();
    let read = |runner: &ShRunner| {
        let out = runner.run(&service_log_script(), &[], &args);
        parse_logs(&q, &result(&out)).unwrap()
    };
    let logs = read(&runner);
    assert!(logs.sudo, "免密码 sudo 可用时经 sudo 读取");
    assert_eq!(logs.lines.len(), 4);
    assert!(logs.lines[0].text.contains("-u app.service"));
    assert_eq!(logs.lines[1].text, "@@rc 9");
    assert_eq!(logs.lines[3].text, "app[1]: no newline");
    runner.remove("sudo");
    let logs = read(&runner);
    assert!(!logs.sudo, "没有 sudo 时以当前用户读");
    assert!(logs.limited);
    assert_eq!(logs.lines.len(), 4);
    runner.fake(
        "id",
        r#"[ "$1" = -Gn ] && { echo 'termlet systemd-journal'; exit 0; }; echo 1000"#,
    );
    assert!(!read(&runner).limited, "在 systemd-journal 组中能看到全部");
}

/// 读取的脚本：输出超过上限时只留最新的部分（第一行可能不完整，去掉），说明被截断；退出码照实。
#[test]
fn read_script_keeps_the_newest_part() {
    let Some(runner) = ShRunner::isolated() else {
        return;
    };
    // 每行 64 字节，共约 2 MB
    let lines = READ_TAIL * 2 / 64;
    runner.fake(
        "docker",
        &format!(
            r#"awk 'BEGIN {{ for (i = 0; i < {lines}; i++) printf "2026-11-04T01:47:12.000000000Z line %031d\n", i }}'; exit 3"#
        ),
    );
    let q = query(container(false));
    let argv = logs_argv(&q).unwrap();
    let args: Vec<&str> = argv.iter().map(String::as_str).collect();
    let out = runner.run(&read_script(), &[], &args);
    assert!(out.len() <= READ_TAIL);
    // 退出码 3：出错
    assert!(matches!(
        parse_logs(&q, &result(&out)),
        Err(ToolsError::LogsFailed(_))
    ));
    let ok = out.replace("@@rc 3", "@@rc 0");
    let logs = parse_logs(&q, &result(&ok)).unwrap();
    assert!(logs.truncated);
    let last = logs.lines.last().unwrap();
    assert_eq!(
        last.text,
        format!("line {:031}", lines - 1),
        "留下的是最新的"
    );
    assert!(
        logs.lines[0].text.starts_with("line "),
        "不完整的第一行已去掉：{}",
        logs.lines[0].text
    );
}
