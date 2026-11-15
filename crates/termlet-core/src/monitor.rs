//! 资源监控：采集远程 Linux 主机的资源使用情况（CPU、内存、负载、运行时间、磁盘、网络），用于终端底部的状态条
//! 与主机工具的概览页。检测服务器端口在 [`crate::forward::listening_ports`]。
//!
//! 每次采样在终端所在的连接上执行一条只读命令（读 /proc 与 df，[`crate::hosttools::Target`]），不需要另外登录，
//! 也不在服务器上留下任何东西；CPU 使用率（总体、每核、iowait、steal）与网络速率由相邻两次采样的计数差算出，
//! 第一次采样没有这几项。服务器不是 Linux（没有 /proc/stat）时返回 [`MonitorError::Unsupported`]。
//!
//! # 用法（界面）
//!
//! - 每个终端 Tab 一个 [`ResourceMonitor`]（记着上一次的计数）。终端显示、窗口可见时（这台主机的概览弹出成小窗时，
//!   终端不在前台也照常）每 [`SAMPLE_INTERVAL`] 调用一次 [`ResourceMonitor::sample`]（上一次返回后再计时），传入终端
//!   当前的连接。设置中可以关闭。
//! - [`MonitorError::Unsupported`]：不再采样。[`ExecError::SingleChannel`]（只用一个通道的主机）：不再采样，在原来
//!   显示资源的位置说明原因。[`ExecError::Disconnected`]、超时等：保留上次的数据，下次照常再试。
//! - 概览页显示时另调用 [`details`]（系统与内核、各挂载点的 inode），间隔比采样长；不显示时不调用。
//! - 方法内部在核心的运行时上执行，返回的 future 是 `Send`，可以在 GPUI 的任务里直接 `await`。
//! - 网络延迟不在这里：用 `Connection::ping`（SSH 保活的往返时间，不另开通道）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::hosttools::{ExecError, Limits, Target, on_runtime, run, sh_script};

/// 界面采样的间隔。
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(3);

/// 一次采样的最长时间。
const TIMEOUT: Duration = Duration::from_secs(10);
/// 输出的上限。
const LIMIT: usize = 256 << 10;
/// 最多列出的挂载点数。
const MAX_DISKS: usize = 8;

/// 读取各项数据，每段前输出一行标记；只读，不写任何文件。写成一行、不含单引号与 `!`（登录 shell 是 tcsh、csh 时
/// 单引号内不能换行，`!` 会被当作历史展开）。
pub const SCRIPT: &str = r#"for f in stat meminfo loadavg uptime net/dev; do echo "@@$f"; cat /proc/$f 2>/dev/null; done; echo "@@df"; df -kP 2>/dev/null"#;

/// 经 `sh -c` 执行 [`SCRIPT`] 的命令：exec 请求交给用户的登录 shell 解释，登录 shell 是 fish、tcsh 时 for 循环语法
/// 不通，输出里没有 cpu 行，会被当成"不是 Linux"。
pub fn sample_command() -> String {
    sh_script::<&str>(SCRIPT, &[])
}

/// 概览页的补充信息：系统与内核（`uname -srm`、os-release 中的名称与版本）、各挂载点的 inode（`df -iP`，BusyBox 也有）。
/// 与 [`SCRIPT`] 一样写成一行、不含单引号与 `!`。
pub const DETAILS_SCRIPT: &str = r#"echo @@uname; uname -srm 2>/dev/null; echo @@os; cat /etc/os-release /usr/lib/os-release 2>/dev/null | grep -E "^(PRETTY_NAME|NAME|VERSION_ID)="; echo @@dfi; df -iP 2>/dev/null"#;

/// 名称像是虚拟网卡（容器的 veth、网桥、隧道、CNI 插件等）：概览页默认不列出，网速的合计也不算它们（容器的流量在
/// veth、网桥与物理网卡上各算一次）。
pub fn is_virtual_iface(name: &str) -> bool {
    const PREFIXES: [&str; 20] = [
        "veth",
        "br-",
        "docker",
        "virbr",
        "vnet",
        "cni",
        "flannel",
        "cali",
        "cilium",
        "weave",
        "kube-",
        "lxcbr",
        "lxdbr",
        "podman",
        "tunl",
        "vxlan",
        "genev",
        "dummy",
        "ifb",
        "nodelocaldns",
    ];
    PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

/// 一个挂载点的使用情况（字节）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disk {
    pub mount: String,
    pub filesystem: String,
    pub total: u64,
    pub used: u64,
}

/// 一块网卡的收发情况。
#[derive(Clone, Debug, PartialEq)]
pub struct NetIface {
    pub name: String,
    /// 累计收发的字节数
    pub rx_total: u64,
    pub tx_total: u64,
    /// 每秒字节数；第一次采样、网卡新出现或计数回绕时为 `None`
    pub rx_rate: Option<f64>,
    pub tx_rate: Option<f64>,
}

/// 一个挂载点的 inode 使用情况。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inodes {
    pub mount: String,
    pub total: u64,
    pub used: u64,
}

/// 概览页的补充信息（[`details`]）；读不到的项为空。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Details {
    /// 发行版（os-release 的 `PRETTY_NAME`，没有时为 `NAME VERSION_ID`）
    pub system: Option<String>,
    /// 内核（`uname -sr`，如 `Linux 6.8.0-45-generic`）与架构（`uname -m`）
    pub kernel: Option<String>,
    pub arch: Option<String>,
    /// 各挂载点的 inode（不报告 inode 的文件系统不列出）
    pub inodes: Vec<Inodes>,
}

/// 一次采样的结果。
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    /// 两次采样之间的 CPU 使用率（0–100），第一次采样为 `None`
    pub cpu: Option<f64>,
    /// 两次采样之间等待 I/O（iowait）、被虚拟化平台占去（steal）的 CPU 时间占比（0–100），第一次采样为 `None`
    pub iowait: Option<f64>,
    pub steal: Option<f64>,
    /// 各核的使用率（0–100，按 /proc/stat 中 cpu0、cpu1… 的顺序）；第一次采样、核数变了时为空
    pub per_core: Vec<f64>,
    pub cores: u32,
    /// 1、5、15 分钟的平均负载
    pub load: [f64; 3],
    /// 开机以来的秒数
    pub uptime: f64,
    /// 内存与交换区（字节）；已用内存 = 总量 − 可用（MemAvailable，3.14 之前的内核为 MemFree + Buffers + Cached）
    pub mem_total: u64,
    pub mem_used: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    /// 根目录在最前，之后是 /dev/ 下的真实设备（排除 loop 设备、光驱与容器绑定挂载的文件），同一设备只列一次，最多 8 个
    pub disks: Vec<Disk>,
    /// 各网卡（不含回环）
    pub net: Vec<NetIface>,
    /// 各网卡速率的合计（不算虚拟网卡，见 [`is_virtual_iface`]；全是虚拟网卡时算全部）；第一次采样为 `None`
    pub rx_rate: Option<f64>,
    pub tx_rate: Option<f64>,
    /// 采样时间（Unix 毫秒）
    pub at: i64,
}

/// 资源监控的错误。消息写给日志看，界面按变体选用自己的文案。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MonitorError {
    /// 服务器不是 Linux（或无法读取 /proc）：不再采样
    #[error("资源监控只支持 Linux")]
    Unsupported,
    /// 执行本身失败：只用一个通道、连接断开、超时、打开通道失败、WSL 出错
    #[error("{0}")]
    Exec(#[from] ExecError),
}

/// 一行 cpu 计数（/proc/stat 的 cpu、cpuN）：全部、空闲（idle + iowait）、iowait、steal 的时钟周期数。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CpuTicks {
    total: u64,
    idle: u64,
    iowait: u64,
    steal: u64,
}

impl CpuTicks {
    /// 解析 cpu 行的计数：user nice system idle iowait irq softirq steal（guest 已计入 user，不重复相加）。
    /// 老内核、BusyBox 的容器里可能少几列，缺的按 0。
    fn parse(values: &[&str]) -> Self {
        let mut ticks = CpuTicks::default();
        for (i, value) in values.iter().take(8).enumerate() {
            let n: u64 = value.parse().unwrap_or(0);
            ticks.total += n;
            match i {
                3 => ticks.idle += n,
                4 => {
                    ticks.idle += n;
                    ticks.iowait = n;
                }
                7 => ticks.steal = n,
                _ => {}
            }
        }
        ticks
    }

    /// 与上一次相比的使用率（0–100）；计数没有前进或回绕时为 `None`。
    fn usage_since(&self, previous: &CpuTicks) -> Option<f64> {
        if self.total <= previous.total || self.idle < previous.idle {
            return None;
        }
        let dt = self.total - previous.total;
        let idle = self.idle - previous.idle;
        (idle <= dt).then(|| 100.0 * (dt - idle) as f64 / dt as f64)
    }

    /// 与上一次相比 iowait、steal 各占的比例（0–100）。iowait 的计数在有的内核上会倒退（proc(5)），倒退时为 `None`。
    fn shares_since(&self, previous: &CpuTicks) -> (Option<f64>, Option<f64>) {
        if self.total <= previous.total {
            return (None, None);
        }
        let dt = (self.total - previous.total) as f64;
        let share = |now: u64, before: u64| {
            now.checked_sub(before)
                .map(|n| (100.0 * n as f64 / dt).min(100.0))
        };
        (
            share(self.iowait, previous.iowait),
            share(self.steal, previous.steal),
        )
    }
}

/// 上一次采样的累计值，用于计算 CPU 使用率与网络速率。
#[derive(Clone, Debug, Default)]
struct Counters {
    cpu: CpuTicks,
    /// 各核（cpu0、cpu1…）
    per_core: Vec<CpuTicks>,
    net: HashMap<String, (u64, u64)>,
    at: i64,
}

/// 一个终端的资源监控：记着上一次的计数。克隆的是同一份。
#[derive(Clone, Default)]
pub struct ResourceMonitor {
    previous: Arc<Mutex<Option<Counters>>>,
}

impl std::fmt::Debug for ResourceMonitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourceMonitor").finish_non_exhaustive()
    }
}

impl ResourceMonitor {
    pub fn new() -> Self {
        Self::default()
    }

    /// 采样一次。
    pub async fn sample(&self, target: &Target) -> Result<Sample, MonitorError> {
        let monitor = self.clone();
        let target = target.clone();
        on_runtime(async move {
            let limits = Limits {
                timeout: TIMEOUT,
                output: LIMIT,
                long: false,
            };
            let result = run(&target, &sample_command(), limits).await?;
            monitor.ingest(
                &String::from_utf8_lossy(&result.stdout),
                crate::time::now_ms(),
            )
        })
        .await
    }

    /// 清除上一次的计数：下一次采样重新从"第一次"开始（没有使用率与速率）。
    pub fn reset(&self) {
        *self.previous.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// 解析一次采样的输出（`at` 为采样时间），与上一次的计数比较算出使用率与速率。
    fn ingest(&self, out: &str, at: i64) -> Result<Sample, MonitorError> {
        let (mut sample, mut current) = parse(out).ok_or(MonitorError::Unsupported)?;
        sample.at = at;
        current.at = at;
        let previous = self
            .previous
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(current.clone());
        let Some(previous) = previous else {
            return Ok(sample);
        };
        sample.cpu = current.cpu.usage_since(&previous.cpu);
        (sample.iowait, sample.steal) = current.cpu.shares_since(&previous.cpu);
        if current.per_core.len() == previous.per_core.len() {
            sample.per_core = current
                .per_core
                .iter()
                .zip(&previous.per_core)
                .map(|(now, before)| now.usage_since(before).unwrap_or(0.0))
                .collect();
        }
        let seconds = (current.at - previous.at) as f64 / 1000.0;
        if seconds > 0.0 {
            for iface in &mut sample.net {
                let Some(&(prev_rx, prev_tx)) = previous.net.get(&iface.name) else {
                    continue; // 网卡新出现
                };
                if iface.rx_total < prev_rx || iface.tx_total < prev_tx {
                    continue; // 计数回绕
                }
                iface.rx_rate = Some((iface.rx_total - prev_rx) as f64 / seconds);
                iface.tx_rate = Some((iface.tx_total - prev_tx) as f64 / seconds);
            }
            let (rx, tx) = total_rates(&sample.net);
            sample.rx_rate = Some(rx);
            sample.tx_rate = Some(tx);
        }
        Ok(sample)
    }
}

/// 各网卡速率的合计：不算虚拟网卡（全是虚拟网卡时算全部），没有速率的网卡不算。
fn total_rates(net: &[NetIface]) -> (f64, f64) {
    let physical = net.iter().any(|iface| !is_virtual_iface(&iface.name));
    net.iter()
        .filter(|iface| !physical || !is_virtual_iface(&iface.name))
        .fold((0.0, 0.0), |(rx, tx), iface| {
            (
                rx + iface.rx_rate.unwrap_or(0.0),
                tx + iface.tx_rate.unwrap_or(0.0),
            )
        })
}

/// 读取概览页的补充信息（[`DETAILS_SCRIPT`]）：只读，读不到的项为空（BusyBox、老系统缺的命令不算错误）。
pub async fn details(target: &Target) -> Result<Details, MonitorError> {
    let target = target.clone();
    on_runtime(async move {
        let limits = Limits {
            timeout: TIMEOUT,
            output: LIMIT,
            long: false,
        };
        let command = sh_script::<&str>(DETAILS_SCRIPT, &[]);
        let result = run(&target, &command, limits).await?;
        Ok(parse_details(&String::from_utf8_lossy(&result.stdout)))
    })
    .await
}

/// 解析 [`DETAILS_SCRIPT`] 的输出。
fn parse_details(out: &str) -> Details {
    let sections = sections(out);
    let section = |name: &str| sections.get(name).map(Vec::as_slice).unwrap_or(&[]);
    let mut details = Details::default();
    // uname -srm：系统名、内核版本、架构（各自不含空白）
    if let Some(line) = section("uname").iter().find(|l| !l.trim().is_empty()) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if let [name, release, rest @ ..] = fields.as_slice() {
            details.kernel = Some(format!("{name} {release}"));
            details.arch = rest.last().map(|arch| arch.to_string());
        }
    }
    let mut release: HashMap<&str, String> = HashMap::new();
    for line in section("os") {
        if let Some((key, value)) = line.split_once('=') {
            // 两个 os-release 都在时同一项取先出现的
            release.entry(key.trim()).or_insert_with(|| unquote(value));
        }
    }
    details.system = release
        .get("PRETTY_NAME")
        .filter(|name| !name.is_empty())
        .cloned()
        .or_else(|| {
            let name = release.get("NAME")?;
            Some(match release.get("VERSION_ID") {
                Some(version) => format!("{name} {version}"),
                None => name.clone(),
            })
        });
    // df -iP：Filesystem Inodes IUsed IFree IUse% Mounted on；不报告 inode 的文件系统（btrfs 等）总数为 0
    let mut seen: Vec<String> = Vec::new();
    for line in section("dfi") {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 6 || fields[0] == "Filesystem" {
            continue;
        }
        let (Ok(total), Ok(used)) = (fields[1].parse::<u64>(), fields[2].parse::<u64>()) else {
            continue;
        };
        let mount = fields[5..].join(" ");
        if total == 0 || seen.contains(&mount) {
            continue;
        }
        seen.push(mount.clone());
        details.inodes.push(Inodes { mount, total, used });
    }
    details
}

/// os-release 的值：去掉成对的引号与其中的转义。
fn unquote(value: &str) -> String {
    let value = value.trim();
    let quoted = value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')));
    if !quoted {
        return value.to_string();
    }
    let mut out = String::new();
    let mut chars = value[1..value.len() - 1].chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// 按 `@@段名` 分段（段名所在的行不算内容）。
fn sections(out: &str) -> HashMap<&str, Vec<&str>> {
    let mut sections: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut name: Option<&str> = None;
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("@@") {
            name = Some(rest);
            sections.entry(rest).or_default();
            continue;
        }
        if let Some(name) = name {
            sections.entry(name).or_default().push(line);
        }
    }
    sections
}

/// 解析采样的输出；不是 Linux（没有 cpu 行）时为 `None`。
fn parse(out: &str) -> Option<(Sample, Counters)> {
    let sections = sections(out);
    let section = |name: &str| sections.get(name).map(Vec::as_slice).unwrap_or(&[]);

    let mut sample = Sample {
        cpu: None,
        iowait: None,
        steal: None,
        per_core: Vec::new(),
        cores: 0,
        load: [0.0; 3],
        uptime: 0.0,
        mem_total: 0,
        mem_used: 0,
        swap_total: 0,
        swap_used: 0,
        disks: Vec::new(),
        net: Vec::new(),
        rx_rate: None,
        tx_rate: None,
        at: 0,
    };
    let mut counters = Counters::default();
    let mut supported = false;
    for line in section("stat") {
        let fields: Vec<&str> = line.split_whitespace().collect();
        match fields.first() {
            Some(&"cpu") if fields.len() >= 5 => {
                counters.cpu = CpuTicks::parse(&fields[1..]);
                supported = true;
            }
            Some(first) if first.starts_with("cpu") => {
                sample.cores += 1;
                counters.per_core.push(CpuTicks::parse(&fields[1..]));
            }
            _ => {}
        }
    }
    if !supported {
        return None;
    }

    let mut mem: HashMap<&str, u64> = HashMap::new();
    for line in section("meminfo") {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        if let Some(value) = rest.split_whitespace().next() {
            // kB
            mem.insert(key, value.parse::<u64>().unwrap_or(0) * 1024);
        }
    }
    let get = |key: &str| mem.get(key).copied().unwrap_or(0);
    sample.mem_total = get("MemTotal");
    // 3.14 之前的内核没有 MemAvailable
    let available = mem
        .get("MemAvailable")
        .copied()
        .unwrap_or_else(|| get("MemFree") + get("Buffers") + get("Cached"));
    if available <= sample.mem_total {
        sample.mem_used = sample.mem_total - available;
    }
    sample.swap_total = get("SwapTotal");
    if get("SwapFree") <= sample.swap_total {
        sample.swap_used = sample.swap_total - get("SwapFree");
    }

    if let Some(line) = section("loadavg").first() {
        for (slot, value) in sample.load.iter_mut().zip(line.split_whitespace()) {
            *slot = value.parse().unwrap_or(0.0);
        }
    }
    if let Some(value) = section("uptime")
        .first()
        .and_then(|line| line.split_whitespace().next())
    {
        sample.uptime = value.parse().unwrap_or(0.0);
    }

    for line in section("net/dev") {
        let Some((iface, rest)) = line.split_once(':') else {
            continue; // 表头
        };
        let iface = iface.trim();
        let fields: Vec<&str> = rest.split_whitespace().collect();
        if iface == "lo" || fields.len() < 9 {
            continue; // 回环
        }
        let (Ok(rx), Ok(tx)) = (fields[0].parse::<u64>(), fields[8].parse::<u64>()) else {
            continue;
        };
        sample.net.push(NetIface {
            name: iface.to_string(),
            rx_total: rx,
            tx_total: tx,
            rx_rate: None,
            tx_rate: None,
        });
        counters.net.insert(iface.to_string(), (rx, tx));
    }

    sample.disks = parse_df(section("df"));
    Some((sample, counters))
}

/// 解析 `df -kP`：保留根目录与 /dev/ 下的真实设备（排除 loop 设备，例如 snap；排除总是满的光驱；排除容器绑定挂载到
/// /etc 下的文件），同一设备只列一次。tmpfs、overlay（容器的根目录除外）、网络文件系统等不列出。
fn parse_df(lines: &[&str]) -> Vec<Disk> {
    let mut disks: Vec<Disk> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for line in lines {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 6 || fields[0] == "Filesystem" {
            continue;
        }
        let (Ok(total), Ok(used)) = (fields[1].parse::<u64>(), fields[2].parse::<u64>()) else {
            continue;
        };
        if total == 0 {
            continue;
        }
        let filesystem = fields[0];
        let mount = fields[5..].join(" ");
        let device = filesystem.starts_with("/dev/")
            && !filesystem.starts_with("/dev/loop")
            && !filesystem.starts_with("/dev/sr");
        let duplicate = seen.iter().any(|s| s == filesystem);
        // /etc 下的挂载点是容器绑定挂载进来的文件（/etc/hosts 等），不是磁盘
        if mount != "/" && (!device || duplicate || mount.starts_with("/etc/")) {
            continue;
        }
        seen.push(filesystem.to_string());
        let disk = Disk {
            filesystem: filesystem.to_string(),
            total: total * 1024,
            used: used * 1024,
            mount,
        };
        if disk.mount == "/" {
            disks.insert(0, disk); // 根目录排在最前
        } else {
            disks.push(disk);
        }
        if disks.len() >= MAX_DISKS {
            break;
        }
    }
    disks
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按脚本的格式生成一次采样的输出：`cpu` 为 /proc/stat 中 cpu 行的 8 个计数，`rx` / `tx` 为 eth0 的累计字节数。
    fn output(cpu: [u64; 8], rx: u64, tx: u64) -> String {
        let [a, b, c, d, e, f, g, h] = cpu;
        format!(
            "@@stat
cpu  {a} {b} {c} {d} {e} {f} {g} {h} 0 0
cpu0 1 2 3 4 5 6 7 8 0 0
cpu1 1 2 3 4 5 6 7 8 0 0
intr 12345
@@meminfo
MemTotal:        4000000 kB
MemFree:          500000 kB
MemAvailable:    3000000 kB
SwapTotal:       1000000 kB
SwapFree:         750000 kB
@@loadavg
0.52 0.40 0.31 1/234 5678
@@uptime
93784.12 180000.00
@@net/dev
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 999999 10 0 0 0 0 0 0 999999 10 0 0 0 0 0 0
  eth0: {rx} 100 0 0 0 0 0 0 {tx} 50 0 0 0 0 0 0
@@df
Filesystem     1024-blocks      Used Available Capacity Mounted on
overlay           61255652  20000000  38000000      35% /
tmpfs                65536         0     65536       0% /dev
/dev/sda1         61255652  20000000  38000000      35% /etc/hosts
/dev/sdb1        103081248  51540624  46281088      53% /data
/dev/loop0           56320     56320         0     100% /snap/core/1
"
        )
    }

    /// 第一次没有使用率与速率；第二次由计数差算出；清除计数后重新从第一次开始。
    #[test]
    fn sample_deltas() {
        let monitor = ResourceMonitor::new();
        let at = 1_000_000;
        let first = monitor
            .ingest(
                &output([100, 0, 50, 800, 50, 0, 0, 0], 1_000_000, 200_000),
                at,
            )
            .unwrap();
        assert_eq!(first.cpu, None);
        assert_eq!(first.rx_rate, None);
        assert_eq!(first.cores, 2);
        assert_eq!(first.mem_total, 4_000_000 * 1024);
        assert_eq!(first.mem_used, 1_000_000 * 1024);
        assert_eq!(first.swap_used, 250_000 * 1024);
        assert_eq!(first.load, [0.52, 0.40, 0.31]);
        assert_eq!(first.uptime, 93784.12);
        // 网卡只有 eth0（不含回环）
        assert_eq!(first.net.len(), 1);
        assert_eq!(first.net[0].name, "eth0");
        // 根目录排在最前；tmpfs、loop 设备、容器绑定挂载的 /etc/hosts 不列出
        let mounts: Vec<&str> = first.disks.iter().map(|d| d.mount.as_str()).collect();
        assert_eq!(mounts, ["/", "/data"]);
        assert_eq!(first.disks[1].used, 51540624 * 1024);

        // 1000 个时间片中 idle + iowait 占 750：使用率 25%；2 秒内收 4MB、发 1MB
        let second = monitor
            .ingest(
                &output([300, 0, 100, 1500, 100, 0, 0, 0], 5_000_000, 1_200_000),
                at + 2000,
            )
            .unwrap();
        assert!(
            (second.cpu.unwrap() - 25.0).abs() < 0.01,
            "{:?}",
            second.cpu
        );
        assert_eq!(second.rx_rate, Some(2_000_000.0));
        assert_eq!(second.tx_rate, Some(500_000.0));
        assert_eq!(second.net[0].rx_rate, Some(2_000_000.0));
        assert_eq!(second.at, at + 2000);

        monitor.reset();
        let third = monitor
            .ingest(
                &output([300, 0, 100, 1500, 100, 0, 0, 0], 5_000_000, 1_200_000),
                at + 4000,
            )
            .unwrap();
        assert_eq!(third.cpu, None, "清除后的第一次采样不应有使用率");
    }

    /// iowait、steal 与每核的使用率由两次读数的差算出；iowait 的计数倒退时没有 iowait，使用率照常。
    #[test]
    fn iowait_steal_and_cores() {
        let stat = |cpu: &str, cpu0: &str, cpu1: &str| {
            format!(
                "@@stat\ncpu  {cpu}\ncpu0 {cpu0}\ncpu1 {cpu1}\nintr 1\n@@meminfo\nMemTotal: 1000 kB\n"
            )
        };
        let monitor = ResourceMonitor::new();
        let first = monitor
            .ingest(
                &stat(
                    "100 0 0 800 50 0 0 50 0 0",
                    "50 0 0 400 25 0 0 25",
                    "50 0 0 400 25 0 0 25",
                ),
                0,
            )
            .unwrap();
        assert_eq!((first.iowait, first.steal), (None, None));
        assert!(first.per_core.is_empty());
        // 共 1000 个时间片：user 300、idle 500、iowait 100、steal 100；cpu0 全忙，cpu1 全闲
        let second = monitor
            .ingest(
                &stat(
                    "400 0 0 1300 150 0 0 150 0 0",
                    "550 0 0 400 25 0 0 25",
                    "50 0 0 900 25 0 0 25",
                ),
                3000,
            )
            .unwrap();
        let near = |a: Option<f64>, b: f64| a.is_some_and(|a| (a - b).abs() < 0.01);
        assert!(near(second.cpu, 40.0), "{:?}", second.cpu);
        assert!(near(second.iowait, 10.0), "{:?}", second.iowait);
        assert!(near(second.steal, 10.0), "{:?}", second.steal);
        assert_eq!(second.per_core, [100.0, 0.0]);
        // iowait 倒退（有的内核会这样）
        let third = monitor
            .ingest(
                &stat(
                    "500 0 0 1500 100 0 0 150 0 0",
                    "650 0 0 400 25 0 0 25",
                    "50 0 0 1100 25 0 0 25",
                ),
                6000,
            )
            .unwrap();
        assert_eq!(third.iowait, None);
        assert!(near(third.steal, 0.0), "{:?}", third.steal);
        // 250 个时间片中 idle + iowait 共增加 150
        assert!(near(third.cpu, 40.0), "{:?}", third.cpu);
    }

    /// 老内核的 cpu 行只有 4 列（没有 iowait、steal）：照常算使用率，iowait、steal 为 0。
    #[test]
    fn old_kernel_cpu_line() {
        let monitor = ResourceMonitor::new();
        let out = |cpu: &str| format!("@@stat\ncpu {cpu}\ncpu0 {cpu}\n");
        monitor.ingest(&out("10 0 10 80"), 0).unwrap();
        let next = monitor.ingest(&out("30 0 30 140"), 3000).unwrap();
        assert!((next.cpu.unwrap() - 40.0).abs() < 0.01, "{:?}", next.cpu);
        assert_eq!((next.iowait, next.steal), (Some(0.0), Some(0.0)));
        assert_eq!(next.per_core.len(), 1);
    }

    /// 网速的合计不算 veth、网桥等虚拟网卡（容器的流量在 veth、docker0 与物理网卡上各算一次）；全是虚拟网卡时算全部。
    #[test]
    fn total_rate_skips_virtual_interfaces() {
        let iface = |name: &str, rate: f64| NetIface {
            name: name.into(),
            rx_total: 0,
            tx_total: 0,
            rx_rate: Some(rate),
            tx_rate: Some(rate / 2.0),
        };
        let net = [
            iface("eth0", 1000.0),
            iface("docker0", 400.0),
            iface("veth3de3bd8", 400.0),
            iface("br-ec96d9920644", 400.0),
            iface("wlan0", 10.0),
        ];
        assert_eq!(total_rates(&net), (1010.0, 505.0));
        assert_eq!(total_rates(&net[1..3]), (800.0, 400.0));
        for name in [
            "veth1",
            "docker0",
            "br-1",
            "virbr0",
            "cni0",
            "flannel.1",
            "cali123",
        ] {
            assert!(is_virtual_iface(name), "{name}");
        }
        for name in ["eth0", "ens3", "enp0s31f6", "wlan0", "bond0", "tun0", "wg0"] {
            assert!(!is_virtual_iface(name), "{name}");
        }
    }

    /// 光驱总是满的：不列出。
    #[test]
    fn optical_drives_are_not_disks() {
        let lines = [
            "Filesystem 1024-blocks Used Available Capacity Mounted on",
            "/dev/vda1 1000 500 500 50% /",
            "/dev/sr0 4000 4000 0 100% /media/cdrom",
            "/dev/vdb1 1000 900 100 90% /data",
        ];
        let mounts: Vec<String> = parse_df(&lines).into_iter().map(|d| d.mount).collect();
        assert_eq!(mounts, ["/", "/data"]);
    }

    /// 概览页的补充信息：termlet-sshd（Alpine，BusyBox 的 df）的原样输出；GNU df 对不报告 inode 的文件系统写 0 与 -。
    #[test]
    fn details_from_busybox_and_gnu() {
        let busybox = "@@uname
Linux 6.18.33.2-microsoft-standard-WSL2 x86_64
@@os
NAME=\"Alpine Linux\"
VERSION_ID=3.23.2
PRETTY_NAME=\"Alpine Linux v3.23\"
@@dfi
Filesystem       Inodes  IUsed    IFree IUse% Mounted on
overlay        67108864 655486 66453378    1% /
tmpfs            996308     16   996292    1% /dev
/dev/sdg       67108864 655486 66453378    1% /etc/hosts
tmpfs                 1      1        0  100% /proc/acpi
";
        let details = parse_details(busybox);
        assert_eq!(details.system.as_deref(), Some("Alpine Linux v3.23"));
        assert_eq!(
            details.kernel.as_deref(),
            Some("Linux 6.18.33.2-microsoft-standard-WSL2")
        );
        assert_eq!(details.arch.as_deref(), Some("x86_64"));
        assert_eq!(details.inodes.len(), 4);
        assert_eq!(
            details.inodes[0],
            Inodes {
                mount: "/".into(),
                total: 67108864,
                used: 655486
            }
        );

        let gnu = "@@uname
Linux 6.8.0-45-generic x86_64
@@os
NAME=\"Ubuntu\"
VERSION_ID=\"24.04\"
NAME=\"Other\"
@@dfi
Filesystem       Inodes   IUsed    IFree IUse% Mounted on
/dev/vda1       6451200  312345  6138855    5% /
/dev/vdb1             0       0        0     - /srv/btrfs
/dev/vdc1        100000   99000     1000   99% /var/lib/my data
";
        let details = parse_details(gnu);
        assert_eq!(
            details.system.as_deref(),
            Some("Ubuntu 24.04"),
            "没有 PRETTY_NAME 时为名称加版本"
        );
        let mounts: Vec<&str> = details.inodes.iter().map(|i| i.mount.as_str()).collect();
        assert_eq!(mounts, ["/", "/var/lib/my data"]);

        // 什么都没读到（命令都不在）：各项为空，不是错误
        assert_eq!(parse_details(""), Details::default());
        assert_eq!(unquote(r#""a \"b\" \\c""#), r#"a "b" \c"#);
        assert_eq!(unquote("'x'"), "x");
    }

    /// 补充信息的命令经 sh -c 执行，脚本不含单引号、`!`、换行与反斜杠。
    #[test]
    fn details_script_is_one_line() {
        assert!(
            !DETAILS_SCRIPT.contains(['\'', '!', '\n', '\\']),
            "{DETAILS_SCRIPT}"
        );
    }

    /// 网卡计数回绕、新出现的网卡没有速率；合计只加有速率的。
    #[test]
    fn counter_wrap_has_no_rate() {
        let monitor = ResourceMonitor::new();
        monitor
            .ingest(&output([1, 0, 1, 10, 0, 0, 0, 0], 5_000, 5_000), 0)
            .unwrap();
        let next = monitor
            .ingest(&output([2, 0, 2, 20, 0, 0, 0, 0], 1_000, 9_000), 1000)
            .unwrap();
        assert_eq!(next.net[0].rx_rate, None);
        assert_eq!(next.rx_rate, Some(0.0));
    }

    /// 没有 /proc（例如 macOS）时不支持。
    #[test]
    fn unsupported_without_proc() {
        let monitor = ResourceMonitor::new();
        assert_eq!(
            monitor.ingest(
                "@@stat\n@@meminfo\n@@loadavg\n@@uptime\n@@net/dev\n@@df\n",
                0
            ),
            Err(MonitorError::Unsupported)
        );
        assert_eq!(monitor.ingest("", 0), Err(MonitorError::Unsupported));
    }

    /// 资源监控的命令经 sh -c 执行，脚本不含单引号、`!`、换行与反斜杠。
    #[test]
    fn command_uses_sh() {
        let command = sample_command();
        assert_eq!(command, format!("sh -c '{SCRIPT}' sh"));
        assert!(!SCRIPT.contains(['\'', '!', '\n', '\\']), "{SCRIPT}");
    }

    /// 3.14 之前的内核没有 MemAvailable：可用 = MemFree + Buffers + Cached。
    #[test]
    fn old_kernel_memory() {
        let out = "@@stat\ncpu 1 2 3 4 5\n@@meminfo\nMemTotal: 1000 kB\nMemFree: 100 kB\nBuffers: 50 kB\nCached: 250 kB\n";
        let (sample, _) = parse(out).unwrap();
        assert_eq!(sample.mem_used, 600 * 1024);
        assert_eq!(sample.cores, 0);
    }

    /// 只用一个通道的主机：有连接也不另开通道，直接报告（需要 `TERMLET_TEST_SSH` 才有连接）。
    #[test]
    fn single_channel_is_refused() {
        let Some((host, port)) = crate::testkit::test_server() else {
            return;
        };
        crate::runtime::runtime().block_on(async {
            let connection = crate::testkit::connect_test(&host, port).await;
            let monitor = ResourceMonitor::new();
            let error = monitor
                .sample(&Target::ssh(connection, true))
                .await
                .unwrap_err();
            assert_eq!(error, MonitorError::Exec(ExecError::SingleChannel));
        });
    }

    /// 在真实的服务器上采样（需要 `TERMLET_TEST_SSH`）：经 sh -c 包装后的命令能读到 /proc 与 df；
    /// 第二次采样有 CPU 使用率与网络速率。
    #[test]
    fn samples_real_server() {
        let Some((host, port)) = crate::testkit::test_server() else {
            return;
        };
        crate::runtime::runtime().block_on(async {
            let connection = crate::testkit::connect_test(&host, port).await;
            let target = Target::ssh(connection, false);
            let monitor = ResourceMonitor::new();
            let first = monitor.sample(&target).await.unwrap();
            assert!(first.mem_total > 0 && first.cores > 0, "{first:?}");
            assert!(!first.disks.is_empty(), "{first:?}");
            assert_eq!(first.cpu, None);
            tokio::time::sleep(Duration::from_millis(300)).await;
            let second = monitor.sample(&target).await.unwrap();
            assert!(second.cpu.is_some(), "{second:?}");
            assert!(second.rx_rate.is_some(), "{second:?}");
            assert_eq!(second.per_core.len(), second.cores as usize, "{second:?}");
            assert!(second.iowait.is_some() && second.steal.is_some());
            // 概览页的补充信息（测试容器是 Alpine，df 是 BusyBox 的）
            let details = details(&target).await.unwrap();
            assert!(
                details
                    .kernel
                    .as_deref()
                    .is_some_and(|k| k.starts_with("Linux ")),
                "{details:?}"
            );
            assert!(details.system.is_some(), "{details:?}");
            assert!(
                details.inodes.iter().any(|i| i.mount == "/" && i.total > 0),
                "{details:?}"
            );
        });
    }
}
