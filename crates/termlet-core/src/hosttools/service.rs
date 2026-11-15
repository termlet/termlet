//! 服务页：systemd 服务的列表。

use std::collections::{HashMap, HashSet};

use super::exec::{Limits, Target, on_runtime, one_line, run, sh_script};
use super::sections::{SectionExt, Sections, first_line, missing, uid_of, with_result};
use super::{HostTools, LIST_LIMIT, LIST_TIMEOUT, ToolsError, scripts};
use crate::ssh::ExecResult;

/// 一个 systemd 服务。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Service {
    /// 完整的服务名（例如 `nginx.service`）
    pub unit: String,
    pub description: String,
    /// loaded、not-found、masked 等
    pub load_state: String,
    /// active、inactive、failed、activating、deactivating、reloading
    pub active_state: String,
    /// running、exited、dead、failed 等
    pub sub_state: String,
    /// 开机启动的状态：enabled、disabled、static、masked、generated 等，没有单元文件时为空
    pub unit_file_state: String,
    /// 单元文件的位置，没有时为 `None`
    pub fragment_path: Option<String>,
    /// 主进程，没有运行时为 `None`
    pub main_pid: Option<u32>,
    /// 管理员自己添加的服务（单元文件在 `/etc/systemd/system/` 下），排在前面
    pub custom: bool,
    /// 上一次运行的结果（`Result`：success、exit-code、signal、core-dump、timeout、start-limit-hit、oom-kill 等），
    /// 读不到时为空
    pub result: String,
    /// 主进程上一次怎样结束（`ExecMainCode`、`ExecMainStatus`），没有结束过时为 `None`
    pub main_exit: Option<MainExit>,
    /// 进入运行状态的时间（`ActiveEnterTimestamp`，Unix 毫秒），没有时为 `None`
    pub active_since: Option<i64>,
    /// 状态上一次变化的时间（`StateChangeTimestamp`，Unix 毫秒；失败的服务是失败的时间），没有时为 `None`
    pub state_changed: Option<i64>,
    /// 自动重启的次数（`NRestarts`，systemd 235 起才有）
    pub restarts: Option<u32>,
    /// 占用的内存（`MemoryCurrent`，字节），没有统计时为 `None`
    pub memory: Option<u64>,
    /// 累计的 CPU 时间（`CPUUsageNSec`，纳秒），没有统计时为 `None`
    pub cpu_nsec: Option<u64>,
    /// 任务数（`TasksCurrent`：进程与线程），没有统计时为 `None`
    pub tasks: Option<u64>,
}

/// 服务的主进程怎样结束。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MainExit {
    /// 以退出码结束（`CLD_EXITED`）
    Code(i32),
    /// 被信号结束（`CLD_KILLED`、`CLD_DUMPED`，值为信号编号）
    Signal(i32),
}

/// 一个 systemd 定时器（服务页的“定时器”子页）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Timer {
    /// 完整的单元名（例如 `backup.timer`）
    pub unit: String,
    pub description: String,
    /// loaded、not-found、masked 等
    pub load_state: String,
    /// active、inactive、failed 等
    pub active_state: String,
    /// waiting、running、elapsed、dead 等
    pub sub_state: String,
    /// 开机启用的状态：enabled、disabled、static 等，没有单元文件时为空
    pub unit_file_state: String,
    /// 单元文件的位置，没有时为 `None`
    pub fragment_path: Option<String>,
    /// 管理员自己添加的定时器（单元文件在 `/etc/systemd/system/` 下）
    pub custom: bool,
    /// 触发的单元（`Unit=`，一般是同名的服务）；读不到时为空
    pub activates: String,
    /// 下次触发的时间（Unix 毫秒）：取 list-timers 算好的（按相对时间触发的也有），没有列出时取
    /// `NextElapseUSecRealtime`；不会再触发（已停止、条件不满足等）、读不到时为 `None`
    pub next: Option<i64>,
    /// 上次触发的时间（Unix 毫秒，`LastTriggerUSec`，没有时取 list-timers 的）；没有触发过时为 `None`
    pub last: Option<i64>,
    /// 服务器在下次、上次触发的时刻相对 UTC 的偏移（秒，东为正）：由按服务器时区写出的 list-timers 对照得出（夏令时
    /// 前后不同），对照不出时为现在的偏移；没有那个时刻、读不到服务器的时区时为 `None`
    pub next_offset: Option<i32>,
    pub last_offset: Option<i32>,
    /// 日历表达式（`OnCalendar=` 的值，可以有几个）
    pub calendar: Vec<String>,
    /// 按相对时间触发的条件，写成单元文件中的样子（`OnBootSec=15min`、`OnUnitActiveSec=1d`）
    pub monotonic: Vec<String>,
    /// 错过的触发在开机后补做（`Persistent=yes`）
    pub persistent: bool,
    /// 随机延迟（`RandomizedDelayUSec`，systemd 的写法如 `10min`）；没有时为空
    pub randomized_delay: String,
    /// 定时器自己上一次的结果（`Result`：success 等；触发的服务失败不算在这里）
    pub result: String,
}

/// 服务器的时区：定时器的日历表达式（`OnCalendar=`，没有写明时区的）按它解释。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerZone {
    /// 时区名（`Asia/Shanghai`）；读不到时为空
    pub name: String,
    /// 现在的缩写（`CST`；没有缩写的时区写作 `+08`）；读不到时为空
    pub abbr: String,
    /// 现在相对 UTC 的偏移（秒，东为正）
    pub offset: i32,
}

/// 服务页的内容。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServiceList {
    pub services: Vec<Service>,
    /// 定时器：按下次触发的时间排序（不会再触发的在后）
    pub timers: Vec<Timer>,
    /// `systemctl --version` 的第一行（例如 `systemd 255 (255.4-1ubuntu8)`）
    pub version: String,
    /// `systemctl is-system-running` 的结果（running、degraded 等）
    pub system_state: String,
    pub uid: Option<u32>,
    /// 服务器的时区（与服务列表同一次读出）；读不到偏移时为 `None`
    pub zone: Option<ServerZone>,
}

/// 读取服务列表的命令。
pub(crate) fn services_command() -> String {
    sh_script::<&str>(&one_line(&scripts::service_script()), &[])
}

impl HostTools {
    /// 读取服务列表；没有 systemd 时返回 [`ToolsError::NoSystemd`]，不是 Linux 时返回 [`ToolsError::Unsupported`]。
    pub async fn services(&self, target: &Target) -> Result<ServiceList, ToolsError> {
        let target = target.clone();
        on_runtime(async move {
            let limits = Limits {
                timeout: LIST_TIMEOUT,
                output: LIST_LIMIT,
                long: true,
            };
            let result = run(&target, &services_command(), limits).await?;
            finish_services(&result)
        })
        .await
    }
}

/// 解析读取服务列表的执行结果。
pub(crate) fn finish_services(result: &ExecResult) -> Result<ServiceList, ToolsError> {
    parse_services(&String::from_utf8_lossy(&result.stdout)).map_err(|e| with_result(e, result))
}

/// 解析服务列表的输出。
pub(crate) fn parse_services(out: &str) -> Result<ServiceList, ToolsError> {
    let sections = Sections::parse(out);
    if sections.has("nosystemd") {
        let system = sections.get("nosystemd").first();
        if !system.is_empty() && system != "Linux" {
            return Err(ToolsError::Unsupported);
        }
        return Err(ToolsError::NoSystemd);
    }
    let show = sections.get("show").ok_or_else(missing)?;
    // 一个服务也没读到、也不知道系统状态：多半是连不上 systemd（没有 D-Bus 时普通用户读不到），说明原因
    let empty_show = show.lines.iter().all(|l| l.trim().is_empty());
    let stderr = first_line(&sections.get("stderr").text());
    if empty_show && sections.get("state").first().is_empty() && !stderr.is_empty() {
        return Err(ToolsError::Unreadable {
            exit_code: None,
            stderr,
        });
    }
    let zone = parse_offset(sections.get("tzoffset").arg()).map(|offset| ServerZone {
        name: sections.get("tz").first().to_string(),
        abbr: sections.get("tzabbr").first().to_string(),
        offset,
    });
    let mut list = ServiceList {
        services: Vec::new(),
        timers: parse_timers(
            &sections.get("timers").all_lines(),
            &sections.get("ltimers").all_lines(),
            sections
                .get("tshow")
                .map(|s| s.lines.as_slice())
                .unwrap_or(&[]),
            zone.as_ref().map(|z| z.offset),
        ),
        version: sections.get("version").first().to_string(),
        system_state: sections.get("state").first().to_string(),
        uid: uid_of(sections.get("uid")),
        zone,
    };
    let mut seen = HashSet::new();
    let mut current: HashMap<String, String> = HashMap::new();
    let mut flush = |current: &mut HashMap<String, String>, list: &mut ServiceList| {
        if let Some(service) = service_of(current)
            && seen.insert(service.unit.clone())
        {
            list.services.push(service);
        }
        current.clear();
    };
    for line in &show.lines {
        if line.trim().is_empty() {
            flush(&mut current, &mut list);
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if current.contains_key(key) {
            // 没有空行分隔的下一条（不应出现）
            flush(&mut current, &mut list);
        }
        current.insert(key.to_string(), value.to_string());
    }
    flush(&mut current, &mut list);
    // 自定义服务在前，其余按名称排序
    list.services
        .sort_by(|a, b| b.custom.cmp(&a.custom).then_with(|| a.unit.cmp(&b.unit)));
    Ok(list)
}

/// 由 `systemctl show` 的一条记录得到服务；没有 Id 时为 `None`。
fn service_of(record: &HashMap<String, String>) -> Option<Service> {
    let get = |key: &str| record.get(key).cloned().unwrap_or_default();
    let unit = get("Id");
    if unit.is_empty() || !unit.ends_with(".service") {
        return None;
    }
    let fragment_path = get("FragmentPath");
    let main_exit = match (
        get("ExecMainCode").parse::<i32>(),
        get("ExecMainStatus").parse::<i32>(),
    ) {
        (Ok(1), Ok(code)) => Some(MainExit::Code(code)),
        (Ok(2 | 3), Ok(signal)) => Some(MainExit::Signal(signal)),
        _ => None,
    };
    Some(Service {
        custom: fragment_path.starts_with("/etc/systemd/system/"),
        unit,
        description: get("Description"),
        load_state: get("LoadState"),
        active_state: get("ActiveState"),
        sub_state: get("SubState"),
        unit_file_state: get("UnitFileState"),
        fragment_path: Some(fragment_path).filter(|p| !p.is_empty()),
        main_pid: get("MainPID").parse().ok().filter(|&pid| pid > 0),
        result: get("Result"),
        main_exit,
        active_since: utc_timestamp(&get("ActiveEnterTimestamp")),
        state_changed: utc_timestamp(&get("StateChangeTimestamp")),
        restarts: get("NRestarts").parse().ok(),
        memory: counter(&get("MemoryCurrent")),
        cpu_nsec: counter(&get("CPUUsageNSec")),
        tasks: counter(&get("TasksCurrent")),
    })
}

/// `list-timers --all` 的一行：触发的单元与下次、上次触发的时间。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ListedTimer {
    pub activates: String,
    pub next: Option<i64>,
    pub last: Option<i64>,
}

/// 解析 `TZ=UTC systemctl list-timers --all --no-legend` 的一行：`NEXT LEFT LAST PASSED UNIT ACTIVATES`，时间写作
/// `Mon 2026-11-09 05:00:00 UTC`，没有的写 `-`（systemd 256 起）或 `n/a`；LEFT、PASSED 是长短不定的说法
/// （`4min 35s`、`1h 40min left`、`44s ago`），不看。以 `.timer` 结尾的一列是定时器；它前面的时间，行首不是 `-`、
/// `n/a` 时第一个是下次、第二个是上次，否则只有上次。认不出定时器名时为 `None`。
pub(crate) fn parse_listed_timer(line: &str) -> Option<(String, ListedTimer)> {
    let (unit, activates, next, last) = split_listed(line, utc_at)?;
    Some((
        unit,
        ListedTimer {
            activates,
            next,
            last,
        },
    ))
}

/// list-timers 的一行拆成定时器名、触发的单元与下次、上次的时间（`stamp` 认出一个时间：从这个位置起的几列是时间时
/// 为 `Some`）；认不出定时器名时为 `None`。
fn split_listed<T>(
    line: &str,
    stamp: impl Fn(&[&str]) -> Option<T>,
) -> Option<(String, String, Option<T>, Option<T>)> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let unit_at = tokens.iter().position(|t| t.ends_with(".timer"))?;
    let head = &tokens[..unit_at];
    let mut stamps = (0..head.len()).filter_map(|i| stamp(&head[i..]));
    let no_next = matches!(head.first(), Some(&("-" | "n/a")));
    let (next, last) = if no_next {
        (None, stamps.next())
    } else {
        (stamps.next(), stamps.next())
    };
    Some((
        tokens[unit_at].to_string(),
        tokens.get(unit_at + 1).copied().unwrap_or("").to_string(),
        next,
        last,
    ))
}

/// `fields` 以 `2026-11-09 05:00:00 UTC` 开头时的时刻（Unix 毫秒），否则为 `None`。
fn utc_at(fields: &[&str]) -> Option<i64> {
    match fields {
        [day, time, zone, ..] if day.len() == 10 && day.as_bytes()[4] == b'-' => {
            utc_timestamp(&format!("{day} {time} {zone}"))
        }
        _ => None,
    }
}

/// `fields` 以 `2026-11-09 13:00:00` 开头时的钟面时间（不管后面的时区缩写），否则为 `None`。
fn wall_at(fields: &[&str]) -> Option<chrono::NaiveDateTime> {
    match fields {
        [day, time, ..] if day.len() == 10 && day.as_bytes()[4] == b'-' => {
            chrono::NaiveDateTime::parse_from_str(&format!("{day} {time}"), "%Y-%m-%d %H:%M:%S")
                .ok()
        }
        _ => None,
    }
}

/// `date +%z` 的输出（`+0800`、`-0530`）换成秒；写法不对时为 `None`。
pub(crate) fn parse_offset(text: &str) -> Option<i32> {
    let text = text.trim();
    let (sign, digits) = match text.as_bytes().first()? {
        b'+' => (1, &text[1..]),
        b'-' => (-1, &text[1..]),
        _ => return None,
    };
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hours: i32 = digits[..2].parse().ok()?;
    let minutes: i32 = digits[2..].parse().ok()?;
    (hours <= 14 && minutes < 60).then_some(sign * (hours * 3600 + minutes * 60))
}

/// 由同一时刻的 UTC 时间与服务器的钟面时间得出服务器在那时的偏移（秒）。两份 list-timers 不是同一刻读的：其间定时器
/// 触发过时时间对不上，这时差值不是 15 分钟的整数倍（各地的偏移都是），或者与现在的偏移相差一小时以上（夏令时只差
/// 一小时），不认，返回 `None`。
fn offset_between(utc_ms: i64, wall: chrono::NaiveDateTime, current: Option<i32>) -> Option<i32> {
    let diff = wall.and_utc().timestamp() - utc_ms.div_euclid(1000);
    let plausible = diff.abs() <= 14 * 3600
        && diff % 900 == 0
        && current.is_none_or(|c| (diff - i64::from(c)).abs() <= 3600);
    plausible.then_some(diff as i32)
}

/// 解析定时器：`listed` 是 `TZ=UTC` 的 `list-timers` 的各行，`local` 是按服务器时区写出的同一份（对照得出各个时刻的
/// 偏移），`show` 是 `systemctl show` 的输出（各定时器之间空一行；`TimersCalendar`、`TimersMonotonic` 有几个条件
/// 就有几行），`current` 是服务器现在的偏移（对照不出时用它）。按下次触发的时间排序，不会再触发的在后。
pub(crate) fn parse_timers(
    listed: &[&str],
    local: &[&str],
    show: &[String],
    current: Option<i32>,
) -> Vec<Timer> {
    let listed: HashMap<String, ListedTimer> = listed
        .iter()
        .filter_map(|line| parse_listed_timer(line))
        .collect();
    type Walls = (Option<chrono::NaiveDateTime>, Option<chrono::NaiveDateTime>);
    let local: HashMap<String, Walls> = local
        .iter()
        .filter_map(|line| split_listed(line, wall_at))
        .map(|(unit, _, next, last)| (unit, (next, last)))
        .collect();
    let mut timers: Vec<Timer> = Vec::new();
    let mut seen = HashSet::new();
    for record in show_records(show) {
        if let Some(mut timer) = timer_of(&record, &listed)
            && seen.insert(timer.unit.clone())
        {
            let walls = local.get(&timer.unit).copied().unwrap_or_default();
            let offset = |at: Option<i64>, wall: Option<chrono::NaiveDateTime>| {
                let at = at?;
                wall.and_then(|wall| offset_between(at, wall, current))
                    .or(current)
            };
            timer.next_offset = offset(timer.next, walls.0);
            timer.last_offset = offset(timer.last, walls.1);
            timers.push(timer);
        }
    }
    timers.sort_by(|a, b| match (a.next, b.next) {
        (Some(x), Some(y)) => x.cmp(&y).then_with(|| a.unit.cmp(&b.unit)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.unit.cmp(&b.unit),
    });
    timers
}

/// `systemctl show` 的各条记录（以空行分开）：同一个属性可以有几行（定时器的触发条件），按出现的先后保留。
fn show_records(lines: &[String]) -> Vec<Vec<(String, String)>> {
    let mut records = Vec::new();
    let mut current: Vec<(String, String)> = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            if !current.is_empty() {
                records.push(std::mem::take(&mut current));
            }
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            current.push((key.to_string(), value.to_string()));
        }
    }
    if !current.is_empty() {
        records.push(current);
    }
    records
}

/// 由 `systemctl show` 的一条记录（与 `list-timers` 中的同一个定时器）得到定时器；没有 Id、不是定时器时为 `None`。
fn timer_of(record: &[(String, String)], listed: &HashMap<String, ListedTimer>) -> Option<Timer> {
    let get = |key: &str| {
        record
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let all = |key: &'static str| {
        record
            .iter()
            .filter(move |(k, _)| k == key)
            .filter_map(|(_, v)| trigger_of(v))
    };
    let unit = get("Id");
    if !unit.ends_with(".timer") {
        return None;
    }
    let row = listed.get(&unit);
    let fragment_path = get("FragmentPath");
    let delay = get("RandomizedDelayUSec");
    let activates = Some(get("Unit"))
        .filter(|u| !u.is_empty())
        .or_else(|| row.map(|r| r.activates.clone()))
        .unwrap_or_default();
    Some(Timer {
        custom: fragment_path.starts_with("/etc/systemd/system/"),
        description: get("Description"),
        load_state: get("LoadState"),
        active_state: get("ActiveState"),
        sub_state: get("SubState"),
        unit_file_state: get("UnitFileState"),
        fragment_path: Some(fragment_path).filter(|p| !p.is_empty()),
        activates,
        next: row
            .and_then(|r| r.next)
            .or_else(|| utc_timestamp(&get("NextElapseUSecRealtime"))),
        last: utc_timestamp(&get("LastTriggerUSec")).or_else(|| row.and_then(|r| r.last)),
        calendar: all("TimersCalendar")
            .filter(|(key, _)| key == "OnCalendar")
            .map(|(_, value)| value)
            .collect(),
        monotonic: all("TimersMonotonic")
            .map(|(key, value)| {
                // 属性中写作 OnBootUSec，单元文件中写作 OnBootSec
                let key = key
                    .strip_suffix("USec")
                    .map_or(key.clone(), |k| format!("{k}Sec"));
                format!("{key}={value}")
            })
            .collect(),
        persistent: get("Persistent") == "yes",
        randomized_delay: if matches!(delay.trim(), "" | "0") {
            String::new()
        } else {
            delay.trim().to_string()
        },
        result: get("Result"),
        unit,
        next_offset: None,
        last_offset: None,
    })
}

/// 定时器的一个触发条件（`{ OnCalendar=*-*-* 03:00:00 ; next_elapse=Tue 2026-11-10 03:00:00 UTC }`）：名称与值；
/// 写法不对时为 `None`。
fn trigger_of(value: &str) -> Option<(String, String)> {
    let inner = value.trim().strip_prefix('{')?.strip_suffix('}')?.trim();
    let first = inner.split(" ; ").next()?.trim();
    let (key, value) = first.split_once('=')?;
    (!key.is_empty() && !value.trim().is_empty())
        .then(|| (key.trim().to_string(), value.trim().to_string()))
}

/// systemd 的计数（内存、CPU 时间、任务数）：没有统计时是 `[not set]`，老版本写成 u64 的最大值（无穷大）。
fn counter(value: &str) -> Option<u64> {
    value.trim().parse().ok().filter(|&n| n != u64::MAX)
}

/// `TZ=UTC` 下 systemctl show 写出的时间（`Wed 2026-11-04 01:47:12 UTC`）换成 Unix 毫秒；没有时间（空、`n/a`、
/// 0）或时区不是 UTC（`TZ` 没有生效）时为 `None`。
fn utc_timestamp(value: &str) -> Option<i64> {
    let fields: Vec<&str> = value.split_whitespace().collect();
    let date = fields
        .iter()
        .position(|f| f.len() == 10 && f.as_bytes()[4] == b'-')?;
    let (day, time) = (fields[date], fields.get(date + 1)?);
    if !matches!(fields.get(date + 2), Some(&("UTC" | "GMT" | "UCT"))) {
        return None;
    }
    let at = chrono::NaiveDateTime::parse_from_str(&format!("{day} {time}"), "%Y-%m-%d %H:%M:%S")
        .ok()?
        .and_utc()
        .timestamp_millis();
    (at > 0).then_some(at)
}
