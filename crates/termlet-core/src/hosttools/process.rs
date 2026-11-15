//! 进程页：进程列表（CPU 使用率由两次读取的差算出）、进程详情。

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use super::cgroup;
use super::exec::{Limits, Target, on_runtime, one_line, run, sh_script};
use super::sections::{SectionExt, Sections, missing, unreadable, users, with_result};
use super::{HostTools, LIST_LIMIT, LIST_TIMEOUT, ToolsError, scripts};
use crate::ssh::ExecResult;

/// 一个进程。
#[derive(Clone, Debug, PartialEq)]
pub struct Process {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    /// `/proc/<pid>/stat` 中的状态字母：R 运行、S 睡眠、D 不可中断、Z 僵尸、T 停止、I 空闲等
    pub state: char,
    /// 读不到 `/proc/<pid>/status` 时为 `None`
    pub uid: Option<u32>,
    /// 用户名（`/etc/passwd` 中没有时为 `None`，界面显示 UID）
    pub user: Option<String>,
    /// 常驻内存（字节）
    pub rss: u64,
    /// 虚拟内存（字节）
    pub vsize: u64,
    /// 两次读取之间占用的 CPU（一个核心为 100，保留一位小数）；第一次读取、上一次读数太旧时没有
    pub cpu: Option<f64>,
    pub threads: u32,
    pub priority: i32,
    pub nice: i32,
    /// 启动时间（Unix 毫秒），算不出时为 `None`
    pub start: Option<i64>,
    /// 启动时刻（`/proc/<pid>/stat` 的第 22 个字段，开机后的时钟周期数）：结束进程时一并传回
    /// （[`super::Action::Kill`] 的 `start_ticks`），执行前核对，防止列表过时、PID 被重新使用后结束了别的进程
    pub start_ticks: u64,
    /// 累计占用的 CPU 时间（秒，保留两位小数）
    pub cpu_time: f64,
    /// 是这个终端自己的 shell 或它的子进程（环境变量中带这个终端的会话标记，或是这类进程的后代）
    pub in_terminal: bool,
    /// 就是这个终端的 shell（结束后这个终端断开）
    pub terminal_shell: bool,
    /// 所属的 systemd 服务（`nginx.service`，读 `/proc/<pid>/cgroup` 得出）；不属于服务、读不到时为 `None`
    pub service: Option<String>,
    /// 所在容器的 ID（64 位十六进制；Docker、Podman、containerd、CRI-O）；不在容器中、读不到时为 `None`
    pub container: Option<String>,
    /// 命令行（参数以空格连接，只读开头 [`super::scripts::CMDLINE_MAX`] 字节、第一行）：进程页的面板够宽时列出；
    /// 内核线程、僵尸进程、读不到时为 `None`。完整的命令行见进程详情
    pub cmdline: Option<String>,
}

/// 进程页的内容。
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessList {
    pub processes: Vec<Process>,
    /// 进程总数（不含内核线程）；超过 2000 个时只列出前 2000 个（`truncated`）
    pub total: usize,
    pub truncated: bool,
    /// 登录用户的 UID；不是 root 时只能结束自己的进程（免密码 sudo 可用时除外）
    pub uid: Option<u32>,
    /// 内存总量（字节）
    pub mem_total: u64,
    pub cores: u32,
    /// 读取时间（Unix 毫秒）
    pub at: i64,
}

/// 一个进程的详情。
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessDetail {
    pub process: Process,
    /// 完整的命令行（参数以空格连接；内核线程、僵尸进程为空）
    pub command: String,
    /// 可执行文件与当前目录（只能看到自己的进程，root 除外；路径中的换行原样保留）
    pub exe: Option<String>,
    pub cwd: Option<String>,
    /// `/proc/<pid>/status` 中的几项（VmRSS、VmSwap、Threads 等，原文，空白合成一个空格）
    pub status: BTreeMap<String, String>,
}

/// 进程的标识：PID 与启动时刻（区分重新使用的 PID）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ProcKey {
    pid: u32,
    start: u64,
}

/// 一次读取的各进程累计 CPU 时间（秒）与当时的开机时长（秒）。
#[derive(Clone, Debug, Default)]
pub(crate) struct Snapshot {
    uptime: f64,
    cpu: HashMap<ProcKey, f64>,
}

/// [`HostTools`] 的内容：会话标识、上一次的读数与这条连接上的权限。
pub(crate) struct Inner {
    /// 合法的会话标识；不合法时为空
    pub key: String,
    previous: Mutex<Option<Snapshot>>,
    /// 探测到的权限（见 [`super::privilege`]）
    pub privilege: super::privilege::PrivilegeCache,
}

impl Inner {
    pub fn new(session_key: &str) -> Self {
        Self {
            key: if valid_key(session_key) {
                session_key.to_string()
            } else {
                String::new()
            },
            previous: Mutex::new(None),
            privilege: Default::default(),
        }
    }
}

/// 会话标识只含字母、数字、短横线（与 SSH 终端的写法一致）。
fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// 读取进程列表的命令：`$1` 是会话标记（`LC_TERMLET_SESSION=…`），`$2` 是会话标识（tmux 会话名）；
/// 会话标识为空时两者都为空。
pub(crate) fn processes_command(key: &str) -> String {
    let (marker, key) = match key.is_empty() {
        true => (String::new(), ""),
        false => (format!("{}={key}", crate::SESSION_ENV), key),
    };
    sh_script(&one_line(&scripts::proc_script()), &[marker.as_str(), key])
}

/// 读取一个进程详情的命令。
pub(crate) fn process_detail_command(pid: u32) -> String {
    sh_script(
        &one_line(&scripts::proc_detail_script()),
        &[pid.to_string()],
    )
}

impl HostTools {
    /// 读取进程列表，标出这个终端自己的 shell 及其子进程（tmux 终端按以会话标识命名的 tmux 会话的窗格）。
    ///
    /// CPU 使用率是与上一次读数之差：`interval` 是调用方的刷新间隔，上一次读数比它的两倍还早时（例如切到这一页时，
    /// 上一次是几分钟前另一页读的）不拿来算，这次没有 CPU（与第一次读取一样），调用方稍后补读一次。
    pub async fn processes(
        &self,
        target: &Target,
        interval: Duration,
    ) -> Result<ProcessList, ToolsError> {
        let inner = self.inner.clone();
        let target = target.clone();
        on_runtime(async move {
            let command = processes_command(&inner.key);
            let limits = Limits {
                timeout: LIST_TIMEOUT,
                output: LIST_LIMIT,
                long: false,
            };
            let result = run(&target, &command, limits).await?;
            inner.finish(&result, crate::time::now_ms(), interval * 2)
        })
        .await
    }

    /// 读取一个进程的详情；进程已不存在时返回 [`ToolsError::NoProcess`]。
    pub async fn process_detail(
        &self,
        target: &Target,
        pid: u32,
    ) -> Result<ProcessDetail, ToolsError> {
        if pid == 0 {
            return Err(ToolsError::Invalid("PID 不正确".into()));
        }
        let target = target.clone();
        on_runtime(async move {
            let limits = Limits {
                timeout: LIST_TIMEOUT,
                output: LIST_LIMIT,
                long: false,
            };
            let result = run(&target, &process_detail_command(pid), limits).await?;
            finish_detail(&result, pid)
        })
        .await
    }
}

/// 解析进程详情的执行结果：进程不存在时脚本以退出码 3 结束。
pub(crate) fn finish_detail(result: &ExecResult, pid: u32) -> Result<ProcessDetail, ToolsError> {
    if result.exit_code == Some(3) {
        return Err(ToolsError::NoProcess);
    }
    parse_process_detail(&String::from_utf8_lossy(&result.stdout), pid)
        .ok_or_else(|| unreadable(result))
}

impl Inner {
    /// 解析一次读取的执行结果（`at` 为读取时间，`max_span` 见 [`Self::ingest`]）。
    pub(crate) fn finish(
        &self,
        result: &ExecResult,
        at: i64,
        max_span: Duration,
    ) -> Result<ProcessList, ToolsError> {
        self.ingest(&String::from_utf8_lossy(&result.stdout), at, max_span)
            .map_err(|e| with_result(e, result))
    }

    /// 解析一次读取的输出，与上一次的读数比较算出 CPU 使用率（两次相隔超过 `max_span` 时不算，按服务器的开机时长
    /// 计），记下这次的读数。
    pub(crate) fn ingest(
        &self,
        out: &str,
        at: i64,
        max_span: Duration,
    ) -> Result<ProcessList, ToolsError> {
        let (mut list, keys, snapshot) = parse_processes(out, &self.key)?;
        list.at = at;
        let previous = self
            .previous
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(snapshot.clone());
        if let Some(previous) = previous
            && snapshot.uptime - previous.uptime <= max_span.as_secs_f64()
        {
            apply_cpu(&mut list, &keys, &previous, &snapshot);
        }
        Ok(list)
    }
}

/// 用两次读取的差算出各进程的 CPU 使用率（一个核心为 100，保留一位小数）；`keys` 与 `list.processes` 一一对应。
fn apply_cpu(list: &mut ProcessList, keys: &[ProcKey], previous: &Snapshot, current: &Snapshot) {
    let dt = current.uptime - previous.uptime;
    if dt <= 0.0 {
        return;
    }
    for (process, key) in list.processes.iter_mut().zip(keys) {
        let (Some(now), Some(before)) = (current.cpu.get(key), previous.cpu.get(key)) else {
            continue;
        };
        if now < before {
            continue;
        }
        process.cpu = Some((1000.0 * (now - before) / dt).round() / 10.0);
    }
}

/// 解析进程列表的输出，返回进程列表、各进程的标识（与列表一一对应）与这次的读数。`key` 是会话标识
/// （tmux 会话名），为空时不看 `@@tmux`。
pub(crate) fn parse_processes(
    out: &str,
    key: &str,
) -> Result<(ProcessList, Vec<ProcKey>, Snapshot), ToolsError> {
    let sections = Sections::parse(out);
    if sections.has("unsupported") {
        return Err(ToolsError::Unsupported);
    }
    let procs = sections.get("procs").ok_or_else(missing)?;
    let clk = clock_ticks(&sections);
    let page = page_size(&sections);
    let uptime: f64 = sections
        .get("uptime")
        .first()
        .split_whitespace()
        .next()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    let mut btime: i64 = 0;
    let mut cores = 0;
    if let Some(stat) = sections.get("stat") {
        for line in &stat.lines {
            let fields: Vec<&str> = line.split_whitespace().collect();
            match fields.as_slice() {
                ["btime", value, ..] => btime = value.parse().unwrap_or(0),
                [first, ..] if first.starts_with("cpu") => cores += 1,
                _ => {}
            }
        }
    }
    let mem_total = sections
        .get("mem")
        .first()
        .split_whitespace()
        .nth(1)
        .and_then(|kb| kb.parse::<u64>().ok())
        .map(|kb| kb * 1024)
        .unwrap_or(0);
    let names = users(sections.get("passwd"));
    let mut owners = cgroup::owners(sections.get("cgroup"));
    let mut cmdlines = cmdlines(sections.get("cmdline"));
    // tmux 终端：这个终端的 shell 是它的 tmux 会话各窗格的 shell（环境变量中的标记不准，见脚本的说明）
    let panes = tmux_panes(sections.get("tmux"), key);
    let mut marked: HashSet<u32> = HashSet::new();
    if let Some(section) = sections.get("self")
        && panes.is_empty()
    {
        for line in &section.lines {
            // /proc/123/environ
            let Some(dir) = line.trim().strip_suffix("/environ") else {
                continue;
            };
            if let Ok(pid) = dir.rsplit('/').next().unwrap_or("").parse() {
                marked.insert(pid);
            }
        }
    }

    let mut list = ProcessList {
        processes: Vec::new(),
        total: 0,
        truncated: false,
        uid: super::sections::uid_of(sections.get("uid")),
        mem_total,
        cores,
        at: 0,
    };
    let mut snapshot = Snapshot {
        uptime,
        cpu: HashMap::new(),
    };
    let mut keys = Vec::new();
    for line in &procs.lines {
        let Some((mut process, ticks, start_ticks)) = parse_proc_line(line, page) else {
            continue;
        };
        if btime > 0 {
            process.start = Some(btime * 1000 + (start_ticks as f64 * 1000.0 / clk) as i64);
        }
        let cpu = ticks as f64 / clk;
        process.cpu_time = (cpu * 100.0).round() / 100.0;
        process.user = process.uid.and_then(|uid| names.get(&uid).cloned());
        if let Some(owner) = owners.remove(&process.pid) {
            process.service = owner.service;
            process.container = owner.container;
        }
        process.cmdline = cmdlines.remove(&process.pid);
        process.in_terminal = marked.contains(&process.pid) || panes.contains(&process.pid);
        let key = ProcKey {
            pid: process.pid,
            start: start_ticks,
        };
        keys.push(key);
        snapshot.cpu.insert(key, cpu);
        list.processes.push(process);
    }
    // 这个终端的 shell：tmux 窗格的 shell；否则为带标记、而父进程不带标记的那个进程
    for process in &mut list.processes {
        if panes.contains(&process.pid) || (process.in_terminal && !marked.contains(&process.ppid))
        {
            process.terminal_shell = true;
        }
    }
    // 子进程也属于这个终端（切换用户后环境变量可能被清掉，例如 sudo -i）
    mark_descendants(&mut list.processes);
    list.total = sections
        .get("total")
        .num_arg()
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(list.processes.len());
    list.truncated = list.total > list.processes.len();
    Ok((list, keys, snapshot))
}

/// `@@cmdline` 段：head 的输出，每个进程一行标题 `==> …/proc/PID/cmdline <==`，之后是它的命令行（参数间的 NUL 已
/// 换成空格；参数中有换行时分成几行，以空格接起来）。同一个 PID 的标题出现两次（参数中冒充的）时都不认；去掉首尾空白，
/// 空的跳过；截在半个字符处时丢掉末尾的替换字符。
fn cmdlines(section: Option<&super::sections::Section>) -> HashMap<u32, String> {
    let mut out: HashMap<u32, String> = HashMap::new();
    let mut twice: HashSet<u32> = HashSet::new();
    let mut current: Option<u32> = None;
    for line in section.map(|s| s.lines.as_slice()).unwrap_or_default() {
        let header = line
            .strip_prefix("==> ")
            .and_then(|l| l.strip_suffix(" <=="))
            .and_then(|path| path.strip_suffix("/cmdline"))
            .and_then(|dir| dir.rsplit('/').next()?.parse::<u32>().ok());
        if let Some(pid) = header {
            current = (!twice.contains(&pid) && !out.contains_key(&pid)).then_some(pid);
            if current.is_some() {
                out.insert(pid, String::new());
            } else if out.remove(&pid).is_some() {
                twice.insert(pid);
            }
            continue;
        }
        if let Some(text) = current.and_then(|pid| out.get_mut(&pid)) {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(line);
        }
    }
    out.into_iter()
        .filter(|(pid, _)| !twice.contains(pid))
        .filter_map(|(pid, text)| {
            let text = text.trim_end_matches(char::REPLACEMENT_CHARACTER).trim();
            (!text.is_empty()).then(|| (pid, text.to_string()))
        })
        .collect()
}

/// `@@clk` 段：每秒的时钟周期数，读不到时为 100。
fn clock_ticks(sections: &Sections) -> f64 {
    match sections.get("clk").num() {
        Some(n) if n > 0 => n as f64,
        _ => 100.0,
    }
}

/// `@@page` 段：内存页的大小，读不到时为 4096。
fn page_size(sections: &Sections) -> u64 {
    match sections.get("page").num() {
        Some(n) if n > 0 => n as u64,
        _ => 4096,
    }
}

/// 解析 `@@tmux` 段（每行"会话名:窗格的 shell 的 PID"，tmux 的会话名中不能有冒号），返回会话名为 `key` 的各窗格的
/// PID。tmux 的 `-t` 在没有同名会话时会按前缀匹配别的会话，因此核对会话名。
fn tmux_panes(section: Option<&super::sections::Section>, key: &str) -> HashSet<u32> {
    let mut panes = HashSet::new();
    let Some(section) = section else {
        return panes;
    };
    if key.is_empty() {
        return panes;
    }
    for line in &section.lines {
        if let Some((name, id)) = line.trim().split_once(':')
            && name == key
            && let Ok(pid) = id.parse::<u32>()
            && pid > 0
        {
            panes.insert(pid);
        }
    }
    panes
}

/// 把这个终端的 shell 的全部后代也标为属于这个终端。
fn mark_descendants(list: &mut [Process]) {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut index: HashMap<u32, usize> = HashMap::new();
    let mut queue: VecDeque<u32> = VecDeque::new();
    for (i, process) in list.iter().enumerate() {
        children.entry(process.ppid).or_default().push(process.pid);
        index.insert(process.pid, i);
        if process.terminal_shell {
            queue.push_back(process.pid);
        }
    }
    let mut seen = HashSet::new();
    while let Some(pid) = queue.pop_front() {
        if !seen.insert(pid) {
            continue;
        }
        if let Some(&i) = index.get(&pid) {
            list[i].in_terminal = true;
        }
        if let Some(kids) = children.get(&pid) {
            queue.extend(kids.iter().copied());
        }
    }
}

/// 解析一行：PID 状态 父进程 utime stime 优先级 nice 线程数 启动时刻 虚拟内存 常驻页数 UID 名称。
/// 返回进程、累计时钟周期数（utime+stime）与启动时刻（开机后的时钟周期数）。
pub(crate) fn parse_proc_line(line: &str, page: u64) -> Option<(Process, u64, u64)> {
    let fields: Vec<&str> = line.splitn(13, ' ').collect();
    if fields.len() < 13 {
        return None;
    }
    let mut nums = [0i64; 11];
    for (slot, i) in nums
        .iter_mut()
        .zip([0usize, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11])
    {
        *slot = fields[i].parse().ok()?;
    }
    let [
        pid,
        ppid,
        utime,
        stime,
        priority,
        nice,
        threads,
        start,
        vsize,
        rss,
        uid,
    ] = nums;
    let state = fields[1].chars().next()?;
    if pid <= 0 || utime < 0 || stime < 0 || start < 0 {
        return None;
    }
    let process = Process {
        pid: u32::try_from(pid).ok()?,
        ppid: u32::try_from(ppid).unwrap_or(0),
        name: fields[12].to_string(),
        state,
        uid: u32::try_from(uid).ok(),
        user: None,
        rss: u64::try_from(rss).map(|pages| pages * page).unwrap_or(0),
        vsize: u64::try_from(vsize).unwrap_or(0),
        cpu: None,
        threads: u32::try_from(threads).unwrap_or(0),
        priority: priority.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        nice: nice.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        start: None,
        start_ticks: start as u64,
        cpu_time: 0.0,
        in_terminal: false,
        terminal_shell: false,
        service: None,
        container: None,
        cmdline: None,
    };
    Some((process, (utime + stime) as u64, start as u64))
}

/// 详情中列出的 status 项。
const DETAIL_STATUS_KEYS: &[&str] = &[
    "Name",
    "State",
    "Tgid",
    "PPid",
    "TracerPid",
    "Uid",
    "Gid",
    "VmPeak",
    "VmSize",
    "VmRSS",
    "VmHWM",
    "VmSwap",
    "Threads",
    "voluntary_ctxt_switches",
    "nonvoluntary_ctxt_switches",
    "Cpus_allowed_list",
];

/// 解析进程详情的输出；内容不完整时为 `None`。
pub(crate) fn parse_process_detail(out: &str, pid: u32) -> Option<ProcessDetail> {
    let sections = Sections::parse(out);
    // 进程名中可能有换行：stat 是整段
    let stat = sections.get("stat").text();
    let stat = stat.trim();
    if stat.is_empty() {
        return None;
    }
    let open = stat.find('(')?;
    let end = stat.rfind(") ")?;
    if end < open {
        return None;
    }
    let fields: Vec<&str> = stat[end + 2..].split_whitespace().collect();
    if fields.len() < 22 {
        return None;
    }
    let page = page_size(&sections);
    let clk = clock_ticks(&sections);
    let mut status = BTreeMap::new();
    let mut uid: i64 = -1;
    if let Some(section) = sections.get("status") {
        for line in &section.lines {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
            if key == "Uid"
                && let Some(first) = value.split(' ').next()
            {
                uid = first.parse().unwrap_or(-1);
            }
            if DETAIL_STATUS_KEYS.contains(&key) {
                status.insert(key.to_string(), value);
            }
        }
    }
    // 与进程列表的一行相同的顺序，复用 parse_proc_line
    let line = [
        pid.to_string(),
        fields[0].to_string(),
        fields[1].to_string(),
        fields[11].to_string(),
        fields[12].to_string(),
        fields[15].to_string(),
        fields[16].to_string(),
        fields[17].to_string(),
        fields[19].to_string(),
        fields[20].to_string(),
        fields[21].to_string(),
        uid.to_string(),
        stat[open + 1..end].to_string(),
    ]
    .join(" ");
    let (mut process, ticks, start) = parse_proc_line(&line, page)?;
    let btime: i64 = sections
        .get("btime")
        .first()
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if btime > 0 {
        process.start = Some(btime * 1000 + (start as f64 * 1000.0 / clk) as i64);
    }
    process.cpu_time = (ticks as f64 / clk * 100.0).round() / 100.0;
    process.user = process
        .uid
        .and_then(|uid| users(sections.get("passwd")).remove(&uid));
    let non_empty = |text: String| Some(text).filter(|t| !t.is_empty());
    Some(ProcessDetail {
        process,
        command: sections
            .get("cmdline")
            .map(|s| s.lines.join(" ").trim().to_string())
            .unwrap_or_default(),
        exe: non_empty(sections.get("exe").text()),
        cwd: non_empty(sections.get("cwd").text()),
        status,
    })
}
