//! 服务页的测试。

use super::{ShRunner, posix};
use crate::hosttools::scripts::service_script;
use crate::hosttools::service::{
    ListedTimer, finish_services, parse_listed_timer, parse_offset, parse_services, parse_timers,
    services_command,
};
use crate::hosttools::{MainExit, ToolsError};
use crate::ssh::ExecResult;

// list-unit-files 与 list-units 的输出：systemd 255（Ubuntu 24.04，多一列 vendor preset；失败与找不到的单元前有 ●）、
// systemd 219（CentOS 7）、systemd 252（Debian 12：与 255 相同的三列，实例单元 user@1000.service 只在 list-units 中）。
const UNIT_FILES_255: &str = "apparmor.service                             enabled         enabled
apt-daily.service                            static          -
getty@.service                               enabled         enabled
ssh.service                                  enabled         enabled
myapp.service                                enabled         enabled
";
const UNITS_255: &str =
    "  apparmor.service                       loaded    active   exited  Load AppArmor profiles
● auditd.service                         not-found inactive dead    auditd.service
  getty@tty1.service                     loaded    active   running Getty on tty1
  ssh.service                            loaded    active   running OpenBSD Secure Shell server
● myapp.service                          loaded    failed   failed  My App
";
const UNIT_FILES_219: &str = "auditd.service                                enabled
getty@.service                                enabled
sshd.service                                  enabled
";
const UNITS_219: &str =
    "auditd.service                      loaded    active   running Security Auditing Service
getty@tty1.service                  loaded    active   running Getty on tty1
sshd.service                        loaded    active   running OpenSSH server daemon
";
const UNIT_FILES_252: &str = "cron.service                           enabled         enabled
networking.service                     enabled         enabled
ssh.service                            enabled         enabled
user@.service                          static          -
";
const UNITS_252: &str = "  cron.service                     loaded active running Regular background program processing daemon
  networking.service               loaded active exited  Raise network interfaces
  ssh.service                      loaded active running OpenBSD Secure Shell server
  user@1000.service                loaded active running User Manager for UID 1000
";

/// 假的 systemctl：show 时为每个名称输出一条记录（MainPID 在 Id 前面，与真实的属性顺序相同）。
fn fake_systemctl(runner: &ShRunner, unit_files: &str, units: &str) {
    runner.fake(
        "systemctl",
        &format!(
            r#"case "$1" in
--version) echo "systemd 255 (255.4-1ubuntu8)"; echo "+PAM +AUDIT";;
is-system-running) echo degraded;;
list-unit-files) cat <<'EOF'
{unit_files}EOF
;;
list-units) cat <<'EOF'
{units}EOF
;;
show) while [ "$1" != "--" ]; do shift; done; shift;
  for n in "$@"; do
    case $n in myapp.service) p=/etc/systemd/system/myapp.service; a=failed;; auditd.service) p=; a=inactive;; *) p=/usr/lib/systemd/system/$n; a=active;; esac
    printf 'MainPID=0\nId=%s\nDescription=%s desc\nLoadState=loaded\nActiveState=%s\nSubState=x\nUnitFileState=enabled\nFragmentPath=%s\n\n' "$n" "$n" "$a" "$p"
  done;;
esac"#
        ),
    );
}

/// 服务名取两份列表的并集（排除模板），自定义的在前；没有 systemd 的容器。
#[test]
fn service_script_on_fake_systemctl() {
    let Some(runner) = ShRunner::new() else {
        return;
    };
    let run = tempfile::tempdir().unwrap();
    let run_dir = posix(run.path());
    runner.fake("id", "echo 0");
    for (name, files, units, want) in [
        (
            "systemd 255",
            UNIT_FILES_255,
            UNITS_255,
            vec![
                "myapp.service",
                "apparmor.service",
                "apt-daily.service",
                "auditd.service",
                "getty@tty1.service",
                "ssh.service",
            ],
        ),
        (
            "systemd 219",
            UNIT_FILES_219,
            UNITS_219,
            vec!["auditd.service", "getty@tty1.service", "sshd.service"],
        ),
        (
            "systemd 252",
            UNIT_FILES_252,
            UNITS_252,
            vec![
                "cron.service",
                "networking.service",
                "ssh.service",
                "user@1000.service",
            ],
        ),
    ] {
        fake_systemctl(&runner, files, units);
        let out = runner.run(
            &service_script(),
            &[("/run/systemd/system", run_dir.as_str())],
            &[],
        );
        let list = parse_services(&out).unwrap_or_else(|e| panic!("{name}：{e}\n{out}"));
        let got: Vec<&str> = list.services.iter().map(|s| s.unit.as_str()).collect();
        assert_eq!(got, want, "{name} 的服务列表");
        assert_eq!(list.system_state, "degraded");
        assert!(list.version.starts_with("systemd 255"), "{name}：{list:?}");
        assert_eq!(list.uid, Some(0));
    }
    // 没有 systemd 的容器：/run/systemd/system 不存在
    runner.fake("uname", "echo Linux");
    let none = format!("{run_dir}/none");
    let out = runner.run(
        &service_script(),
        &[("/run/systemd/system", none.as_str())],
        &[],
    );
    assert_eq!(parse_services(&out), Err(ToolsError::NoSystemd), "{out}");
}

/// 普通用户连不上 systemd（容器里没有 D-Bus）：列表为空、也没有系统状态时，报出 is-system-running 的错误，
/// 而不是“没有服务”；系统状态照常读到时空列表不算错误。
#[test]
fn service_script_reports_why_systemd_is_unreachable() {
    let Some(runner) = ShRunner::new() else {
        return;
    };
    let run = tempfile::tempdir().unwrap();
    let run_dir = posix(run.path());
    runner.fake("id", "echo 1000");
    runner.fake(
        "systemctl",
        r#"case "$1" in
--version) echo "systemd 257 (257.13-1~deb13u1)";;
*) echo "Failed to connect to system scope bus via local transport: No such file or directory" >&2; exit 1;;
esac"#,
    );
    let out = runner.run(
        &service_script(),
        &[("/run/systemd/system", run_dir.as_str())],
        &[],
    );
    // 经执行结果解析（与真实的读取相同）：脚本照常以 0 结束、没有标准错误，原因不被执行结果盖掉
    let result = ExecResult {
        exit_code: Some(0),
        stdout: out.clone().into_bytes(),
        ..Default::default()
    };
    match finish_services(&result) {
        Err(ToolsError::Unreadable { stderr, .. }) => assert_eq!(
            stderr,
            "Failed to connect to system scope bus via local transport: No such file or directory"
        ),
        other => panic!("应说明连不上 systemd：{other:?}\n{out}"),
    }
    // 读到了系统状态（running）但一个服务也没有：照常是空列表
    runner.fake(
        "systemctl",
        r#"case "$1" in
--version) echo "systemd 257";;
is-system-running) echo running;;
*) exit 0;;
esac"#,
    );
    let out = runner.run(
        &service_script(),
        &[("/run/systemd/system", run_dir.as_str())],
        &[],
    );
    let list = parse_services(&out).unwrap_or_else(|e| panic!("{e}\n{out}"));
    assert!(list.services.is_empty());
    assert_eq!(list.system_state, "running");
}

/// systemctl show 的运行信息：失败的结果与退出码、运行起点与状态变化的时间（UTC）、重启次数、内存、CPU 时间、
/// 任务数；取不到（`[not set]`、无穷大、空、`n/a`）的为 `None`，被信号结束的记为信号。
#[test]
fn parse_show_runtime_properties() {
    let out = "@@uid
0
@@version
systemd 257
@@state
degraded
@@stderr
@@show
Id=broken.service
Description=Broken worker
LoadState=loaded
ActiveState=failed
SubState=failed
FragmentPath=/etc/systemd/system/broken.service
UnitFileState=enabled
MainPID=0
Result=exit-code
ExecMainCode=1
ExecMainStatus=3
ActiveEnterTimestamp=n/a
StateChangeTimestamp=Wed 2026-11-04 01:47:12 UTC
NRestarts=5
MemoryCurrent=[not set]
CPUUsageNSec=3200000000
TasksCurrent=18446744073709551615

Id=demo-app.service
Description=Demo
LoadState=loaded
ActiveState=active
SubState=running
FragmentPath=/etc/systemd/system/demo-app.service
UnitFileState=enabled-runtime
MainPID=74
Result=success
ExecMainCode=0
ExecMainStatus=0
ActiveEnterTimestamp=Tue 2026-11-03 23:59:58 UTC
StateChangeTimestamp=Tue 2026-11-03 23:59:58 CST
NRestarts=0
MemoryCurrent=12582912
CPUUsageNSec=[not set]
TasksCurrent=4

Id=killed.service
ActiveState=failed
Result=signal
ExecMainCode=2
ExecMainStatus=9
ActiveEnterTimestamp=
";
    let list = parse_services(out).unwrap();
    let broken = list
        .services
        .iter()
        .find(|s| s.unit == "broken.service")
        .unwrap();
    assert_eq!(broken.result, "exit-code");
    assert_eq!(broken.main_exit, Some(MainExit::Code(3)));
    assert_eq!(broken.active_since, None, "n/a");
    assert_eq!(broken.state_changed, Some(1_793_756_832_000));
    assert_eq!(broken.restarts, Some(5));
    assert_eq!(broken.memory, None, "[not set]");
    assert_eq!(broken.cpu_nsec, Some(3_200_000_000));
    assert_eq!(broken.tasks, None, "无穷大");

    let demo = list
        .services
        .iter()
        .find(|s| s.unit == "demo-app.service")
        .unwrap();
    assert_eq!(demo.main_exit, None, "没有结束过");
    assert_eq!(demo.active_since, Some(1_793_750_398_000));
    assert_eq!(demo.state_changed, None, "不是 UTC 的时间不猜");
    assert_eq!(demo.restarts, Some(0));
    assert_eq!(demo.memory, Some(12 << 20));
    assert_eq!(demo.cpu_nsec, None);
    assert_eq!(demo.tasks, Some(4));
    assert_eq!(demo.unit_file_state, "enabled-runtime");

    let killed = list
        .services
        .iter()
        .find(|s| s.unit == "killed.service")
        .unwrap();
    assert_eq!(killed.main_exit, Some(MainExit::Signal(9)));
    assert_eq!(killed.active_since, None, "空");
    assert_eq!(killed.restarts, None, "老版本没有 NRestarts");
}

/// systemctl show 的真实输出（Ubuntu 24.04：属性按 D-Bus 接口的顺序，MainPID 在 Id 前面；找不到的单元）。
const SHOW_UBUNTU: &str = "@@uid
1000
@@version
systemd 255 (255.4-1ubuntu8)
@@state
running
@@show
MainPID=1001
Id=ssh.service
Description=OpenBSD Secure Shell server
LoadState=loaded
ActiveState=active
SubState=running
FragmentPath=/usr/lib/systemd/system/ssh.service
UnitFileState=enabled

MainPID=0
Id=auditd.service
Description=auditd.service
LoadState=not-found
ActiveState=inactive
SubState=dead
FragmentPath=
UnitFileState=

MainPID=0
Id=backup.service
Description=Nightly backup = rsync to NAS
LoadState=loaded
ActiveState=failed
SubState=failed
FragmentPath=/etc/systemd/system/backup.service
UnitFileState=disabled

";

#[test]
fn parse_show_output() {
    let list = parse_services(SHOW_UBUNTU).unwrap();
    assert_eq!(list.services.len(), 3);
    assert_eq!(list.uid, Some(1000));
    let first = &list.services[0];
    assert_eq!(first.unit, "backup.service", "自定义服务排在前面");
    assert!(first.custom);
    assert_eq!(
        first.description, "Nightly backup = rsync to NAS",
        "描述中可以有 ="
    );
    assert_eq!(first.active_state, "failed");
    assert_eq!(first.unit_file_state, "disabled");
    let ssh = &list.services[2];
    assert_eq!(ssh.unit, "ssh.service");
    assert_eq!(ssh.main_pid, Some(1001));
    assert!(!ssh.custom);
    assert_eq!(ssh.sub_state, "running");
    let missing = &list.services[1];
    assert_eq!(missing.load_state, "not-found", "找不到的单元");
    assert_eq!(missing.unit_file_state, "");
    assert_eq!(missing.fragment_path, None);
    assert_eq!(missing.main_pid, None);

    // 不是 Linux（uname 不是 Linux）
    assert_eq!(
        parse_services("@@nosystemd\nDarwin\n"),
        Err(ToolsError::Unsupported)
    );
    assert_eq!(
        parse_services("@@nosystemd\nLinux\n"),
        Err(ToolsError::NoSystemd)
    );
    assert!(matches!(
        parse_services("garbage"),
        Err(ToolsError::Unreadable { .. })
    ));
}

/// 没有输出时带上错误输出。
#[test]
fn empty_output_has_stderr() {
    let result = ExecResult {
        exit_code: Some(127),
        stderr: b"sh: not found".to_vec(),
        ..Default::default()
    };
    match finish_services(&result) {
        Err(ToolsError::Unreadable { stderr, .. }) => assert!(stderr.contains("not found")),
        other => panic!("应带上错误输出：{other:?}"),
    }
    assert!(services_command().starts_with("sh -c 'export LC_ALL=C;"));
}

// ------------------------------------------------------------ 定时器

/// `TZ=UTC systemctl list-timers --all --no-legend` 的输出：systemd 257（Debian 13，没有的写 `-`，触发过的有上次与
/// PASSED）与 systemd 219（CentOS 7，没有的写 `n/a`，LEFT 写作 `14h left`）。
const LIST_TIMERS_257: &str = "Mon 2026-11-09 05:04:00 UTC      13s Mon 2026-11-09 05:03:02 UTC 44s ago broken.timer                 broken.service
Mon 2026-11-09 05:04:07 UTC      21s -                                 - cleanup.timer                cleanup.service
Tue 2026-11-10 03:06:36 UTC      22h -                                 - backup.timer                 backup.service
-                                  - -                                 - fstrim.timer                 fstrim.service";
const LIST_TIMERS_219: &str = "Mon 2023-03-13 18:00:00 UTC  11h left Mon 2023-03-13 06:23:11 UTC  1h ago apt-daily.timer apt-daily.service
n/a                          n/a      Mon 2023-03-13 06:23:11 UTC  1h ago once.timer      once.service
n/a                          n/a      n/a                          n/a    idle.timer      idle.service";

/// list-timers 的一行：下次、上次的时间与触发的单元；LEFT、PASSED 长短不定，不看；没有的写 `-` 或 `n/a`。
#[test]
fn listed_timers_find_next_and_last() {
    let get = |text: &str, unit: &str| {
        text.lines()
            .filter_map(parse_listed_timer)
            .find(|(u, _)| u == unit)
            .map(|(_, t)| t)
            .unwrap_or_else(|| panic!("没有 {unit}"))
    };
    assert_eq!(
        get(LIST_TIMERS_257, "broken.timer"),
        ListedTimer {
            activates: "broken.service".into(),
            next: Some(1_794_200_640_000),
            last: Some(1_794_200_582_000),
        }
    );
    // 按相对时间触发的定时器：下次的时间只有 list-timers 算好了
    assert_eq!(
        get(LIST_TIMERS_257, "cleanup.timer").next,
        Some(1_794_200_647_000)
    );
    assert_eq!(get(LIST_TIMERS_257, "cleanup.timer").last, None);
    let fstrim = get(LIST_TIMERS_257, "fstrim.timer");
    assert_eq!((fstrim.next, fstrim.last), (None, None), "不会再触发");
    assert_eq!(fstrim.activates, "fstrim.service");

    let apt = get(LIST_TIMERS_219, "apt-daily.timer");
    assert_eq!(
        (apt.next, apt.last),
        (Some(1_678_730_400_000), Some(1_678_688_591_000))
    );
    let once = get(LIST_TIMERS_219, "once.timer");
    assert_eq!(
        (once.next, once.last),
        (None, Some(1_678_688_591_000)),
        "n/a 的下次之后是上次"
    );
    let idle = get(LIST_TIMERS_219, "idle.timer");
    assert_eq!((idle.next, idle.last), (None, None));
    // 时区不是 UTC（TZ 没有生效）时不猜
    let (_, local) =
        parse_listed_timer("Mon 2026-11-09 13:00:00 CST 5min - - a.timer a.service").unwrap();
    assert_eq!(local.next, None);
    assert!(parse_listed_timer("8 timers listed.").is_none());
    assert!(parse_listed_timer("").is_none());
}

/// systemctl show 的真实输出（systemd 257，`-p` 指定的属性；属性按 D-Bus 接口的顺序，Unit 在 Id 前面）：日历与相对
/// 时间的条件可以有几行，没有触发过时 LastTriggerUSec 为空，较早的版本为 `n/a`。
const TIMER_SHOW_257: &str = "Unit=backup.service
TimersCalendar={ OnCalendar=*-*-* 03:00:00 ; next_elapse=Tue 2026-11-10 03:00:00 UTC }
NextElapseUSecRealtime=Tue 2026-11-10 03:06:15 UTC
LastTriggerUSec=
Result=success
RandomizedDelayUSec=10min
Persistent=yes
Id=backup.timer
Description=Nightly backup
LoadState=loaded
ActiveState=active
SubState=waiting
FragmentPath=/etc/systemd/system/backup.timer
UnitFileState=enabled

Unit=cleanup.service
TimersMonotonic={ OnUnitActiveUSec=1h ; next_elapse=0 }
TimersMonotonic={ OnBootUSec=5min ; next_elapse=1w 6d 21h 12min 55.426944s }
NextElapseUSecRealtime=
LastTriggerUSec=n/a
Result=success
RandomizedDelayUSec=0
Persistent=no
Id=cleanup.timer
Description=Hourly cleanup
LoadState=loaded
ActiveState=active
SubState=waiting
FragmentPath=/etc/systemd/system/cleanup.timer
UnitFileState=enabled

Unit=send-report.service
TimersCalendar={ OnCalendar=Fri *-*-* 17:30:00 ; next_elapse=(null) }
TimersCalendar={ OnCalendar=Mon *-*-* 08:00:00 ; next_elapse=(null) }
NextElapseUSecRealtime=
LastTriggerUSec=
Result=success
RandomizedDelayUSec=0
Persistent=no
Id=report.timer
Description=Weekly report
LoadState=loaded
ActiveState=inactive
SubState=dead
FragmentPath=/etc/systemd/system/report.timer
UnitFileState=disabled

Unit=broken.service
TimersCalendar={ OnCalendar=*-*-* *:*:00 ; next_elapse=Mon 2026-11-09 05:04:00 UTC }
NextElapseUSecRealtime=Mon 2026-11-09 05:04:00 UTC
LastTriggerUSec=Mon 2026-11-09 05:03:02 UTC
Result=success
Id=broken.timer
ActiveState=active
SubState=waiting
FragmentPath=/usr/lib/systemd/system/broken.timer
UnitFileState=enabled

Id=not-a-timer.service
ActiveState=active
";

/// 定时器的属性：触发的单元、日历与相对时间的条件（写成单元文件中的样子）、下次（list-timers 算好的优先，没有时取
/// NextElapseUSecRealtime）、上次（空、`n/a` 为没有）、补做、随机延迟；按下次触发的时间排序，不会再触发的在后。
#[test]
fn timers_merge_list_timers_and_show() {
    let listed: Vec<&str> = LIST_TIMERS_257.lines().collect();
    let show: Vec<String> = TIMER_SHOW_257.lines().map(str::to_string).collect();
    let timers = parse_timers(&listed, &[], &show, None);
    let units: Vec<&str> = timers.iter().map(|t| t.unit.as_str()).collect();
    assert_eq!(
        units,
        vec![
            "broken.timer",
            "cleanup.timer",
            "backup.timer",
            "report.timer"
        ],
        "按下次触发的时间，不会再触发的在后；不是定时器的记录不算"
    );
    let backup = &timers[2];
    assert_eq!(backup.activates, "backup.service");
    assert_eq!(backup.calendar, vec!["*-*-* 03:00:00"]);
    assert!(backup.monotonic.is_empty());
    assert_eq!(
        backup.next,
        Some(1_794_279_996_000),
        "list-timers 中的（加了随机延迟）优先"
    );
    assert_eq!(backup.last, None, "没有触发过");
    assert!(backup.persistent && backup.custom);
    assert_eq!(backup.randomized_delay, "10min");
    assert_eq!(backup.description, "Nightly backup");
    assert_eq!(
        (backup.active_state.as_str(), backup.sub_state.as_str()),
        ("active", "waiting")
    );

    let cleanup = &timers[1];
    assert_eq!(
        cleanup.monotonic,
        vec!["OnUnitActiveSec=1h", "OnBootSec=5min"],
        "写成单元文件中的样子"
    );
    assert_eq!(cleanup.next, Some(1_794_200_647_000));
    assert_eq!(cleanup.last, None, "n/a");
    assert_eq!(cleanup.randomized_delay, "", "0 为没有");

    let report = &timers[3];
    assert_eq!(
        report.calendar,
        vec!["Fri *-*-* 17:30:00", "Mon *-*-* 08:00:00"],
        "几个日历表达式都留着"
    );
    assert_eq!(report.activates, "send-report.service", "Unit= 可以不同名");
    assert_eq!(report.next, None, "没有启用、不在 list-timers 中");
    assert_eq!(report.unit_file_state, "disabled");

    let broken = &timers[0];
    assert_eq!(broken.last, Some(1_794_200_582_000));
    assert!(!broken.custom, "单元文件不在 /etc/systemd/system/ 下");
    // 不在 list-timers 中的定时器取 NextElapseUSecRealtime
    let alone = parse_timers(&[], &[], &show, None);
    let broken = alone.iter().find(|t| t.unit == "broken.timer").unwrap();
    assert_eq!(broken.next, Some(1_794_200_640_000));
    let cleanup = alone.iter().find(|t| t.unit == "cleanup.timer").unwrap();
    assert_eq!(cleanup.next, None, "按相对时间触发的算不出");
}

/// 服务列表的脚本一并读出定时器：定时器名取 list-unit-files 与 list-timers 的并集（没有启用的也在），排除模板。
#[test]
fn service_script_lists_timers() {
    let Some(runner) = ShRunner::new() else {
        return;
    };
    let run = tempfile::tempdir().unwrap();
    let run_dir = posix(run.path());
    runner.fake("id", "echo 0");
    runner.fake(
        "systemctl",
        r#"case "$1" in
--version) echo "systemd 257";;
is-system-running) echo running;;
list-unit-files) case "$2" in --type=timer) printf 'backup.timer enabled enabled\nreport.timer disabled enabled\nfoo@.timer static -\n';; esac;;
list-timers) if [ "$TZ" = UTC ]; then printf 'Tue 2026-11-10 03:06:36 UTC 22h - - backup.timer backup.service\nMon 2026-11-09 05:04:07 UTC 21s - - cleanup.timer cleanup.service\n'; else printf 'Tue 2026-11-10 11:06:36 CST 22h - - backup.timer backup.service\nMon 2026-11-09 13:04:07 CST 21s - - cleanup.timer cleanup.service\n'; fi;;
show) while [ "$1" != "--" ]; do shift; done; shift;
  for n in "$@"; do
    case $n in *.timer) printf 'Unit=%s\nTimersCalendar={ OnCalendar=daily ; next_elapse=n/a }\nId=%s\nActiveState=active\n\n' "${n%.timer}.service" "$n";; esac
  done;;
esac"#,
    );
    let out = runner.run(
        &service_script(),
        &[("/run/systemd/system", run_dir.as_str())],
        &[],
    );
    let list = parse_services(&out).unwrap_or_else(|e| panic!("{e}\n{out}"));
    let units: Vec<&str> = list.timers.iter().map(|t| t.unit.as_str()).collect();
    assert_eq!(
        units,
        vec!["cleanup.timer", "backup.timer", "report.timer"],
        "{out}"
    );
    assert_eq!(list.timers[2].activates, "report.service");
    assert_eq!(list.timers[2].calendar, vec!["daily"]);
    assert!(list.services.is_empty());
    // 服务器的时区：本机的 sh 上读到什么算什么（Git 的 sh 没有 /etc/localtime），偏移总读得到
    assert!(list.zone.is_some(), "{out}");
}

/// 服务器的时区并入同一个脚本：`/etc/localtime` 链接到的 zoneinfo 名称、`date` 的偏移与缩写；两份 list-timers
/// 对照得出各个时刻的偏移。
#[test]
fn service_script_reads_the_server_timezone() {
    let Some(runner) = ShRunner::new() else {
        return;
    };
    let run = tempfile::tempdir().unwrap();
    let run_dir = posix(run.path());
    runner.fake("id", "echo 1000");
    runner.fake("readlink", "echo ../usr/share/zoneinfo/Asia/Shanghai");
    runner.fake(
        "date",
        r#"case "$1" in +%z) echo +0800;; +%Z) echo CST;; *) echo unknown;; esac"#,
    );
    runner.fake(
        "systemctl",
        r#"case "$1" in
--version) echo "systemd 257";;
is-system-running) echo running;;
list-unit-files) case "$2" in --type=timer) printf 'backup.timer enabled enabled\n';; esac;;
list-timers) if [ "$TZ" = UTC ]; then echo 'Tue 2026-11-10 19:00:00 UTC 22h Mon 2026-11-09 19:00:02 UTC 2h ago backup.timer backup.service'; else echo 'Wed 2026-11-11 03:00:00 CST 22h Tue 2026-11-10 03:00:02 CST 2h ago backup.timer backup.service'; fi;;
show) while [ "$1" != "--" ]; do shift; done; shift;
  for n in "$@"; do
    case $n in *.timer) printf 'Unit=backup.service\nTimersCalendar={ OnCalendar=*-*-* 03:00:00 ; next_elapse=n/a }\nId=%s\nActiveState=active\n\n' "$n";; esac
  done;;
esac"#,
    );
    let out = runner.run(
        &service_script(),
        &[("/run/systemd/system", run_dir.as_str())],
        &[],
    );
    let list = parse_services(&out).unwrap_or_else(|e| panic!("{e}\n{out}"));
    assert_eq!(
        list.zone,
        Some(crate::hosttools::ServerZone {
            name: "Asia/Shanghai".into(),
            abbr: "CST".into(),
            offset: 8 * 3600,
        }),
        "{out}"
    );
    let backup = &list.timers[0];
    assert_eq!(backup.next_offset, Some(8 * 3600));
    assert_eq!(backup.last_offset, Some(8 * 3600));
}

/// `date +%z` 的偏移：正负、半小时与三刻钟的时区；写法不对时不猜。
#[test]
fn offsets_parse() {
    assert_eq!(parse_offset("+0800"), Some(28_800));
    assert_eq!(parse_offset("-0500\n"), Some(-18_000));
    assert_eq!(parse_offset("+0545"), Some(20_700));
    assert_eq!(parse_offset("+0000"), Some(0));
    for bad in ["", "0800", "+08", "+08:00", "+2500", "+0870", "CST"] {
        assert_eq!(parse_offset(bad), None, "{bad}");
    }
}

/// 各个时刻的偏移按服务器的钟面时间对照得出：夏令时前后不同（柏林 3 月 29 日从 +1 变 +2）；两份 list-timers 之间定时器
/// 触发过（时间对不上）、按服务器时区的那份没有这个定时器时用现在的偏移；不知道服务器的时区时为 `None`。
#[test]
fn timer_offsets_follow_daylight_saving() {
    let show: Vec<String> = [
        "Unit=backup.service",
        "LastTriggerUSec=Sat 2026-03-28 02:00:00 UTC",
        "Id=backup.timer",
        "ActiveState=active",
        "",
        "Unit=minute.service",
        "LastTriggerUSec=Sat 2026-03-28 10:59:00 UTC",
        "Id=minute.timer",
        "ActiveState=active",
        "",
        "Unit=alone.service",
        "NextElapseUSecRealtime=Sun 2026-03-29 05:00:00 UTC",
        "Id=alone.timer",
        "ActiveState=active",
    ]
    .map(str::to_string)
    .to_vec();
    let utc = [
        "Sun 2026-03-29 01:00:00 UTC 14h Sat 2026-03-28 02:00:00 UTC 9h ago backup.timer backup.service",
        "Sat 2026-03-28 11:00:00 UTC 30s Sat 2026-03-28 10:59:00 UTC 30s ago minute.timer minute.service",
    ];
    let local = [
        "Sun 2026-03-29 03:00:00 CEST 14h Sat 2026-03-28 03:00:00 CET 9h ago backup.timer backup.service",
        // 两次读之间触发了一次：下次、上次都晚了一分钟
        "Sat 2026-03-28 12:01:00 CET 30s Sat 2026-03-28 12:00:00 CET 30s ago minute.timer minute.service",
    ];
    let timers = parse_timers(&utc, &local, &show, Some(3600));
    let get = |unit: &str| timers.iter().find(|t| t.unit == unit).unwrap();
    let backup = get("backup.timer");
    assert_eq!(backup.next_offset, Some(7200), "夏令时之后");
    assert_eq!(backup.last_offset, Some(3600), "夏令时之前");
    let minute = get("minute.timer");
    assert_eq!(
        (minute.next_offset, minute.last_offset),
        (Some(3600), Some(3600)),
        "对不上时用现在的偏移"
    );
    let alone = get("alone.timer");
    assert_eq!(alone.next_offset, Some(3600), "不在 list-timers 中");
    assert_eq!(alone.last_offset, None, "没有触发过");
    let unknown = parse_timers(&utc, &[], &show, None);
    assert!(
        unknown
            .iter()
            .all(|t| t.next_offset.is_none() && t.last_offset.is_none())
    );
}
