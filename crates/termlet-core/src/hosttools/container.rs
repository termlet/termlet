//! 容器的资源占用（`docker stats --no-stream`）与详情（`docker container inspect`），以及列表中的运行状态
//! （[`RunState`]，列表读取时 inspect 取出的；读不到时由状态文字看出退出码与健康状态）。
//!
//! 都只读，与列表分开读（资源占用要 1–2 秒，不拖慢列表）：Docker 页经免密码 sudo 访问时同样经 sudo。详情只取需要的
//! 字段（Go 模板拼成一个 JSON 对象），不读环境变量等可能含秘密的配置。

use std::time::Duration;

use serde_json::Value;

use super::docker::{Container, Engine, ID_RE, JsonObj, down_error};
use super::exec::{Limits, Target, on_runtime, one_line, run, sh_script};
use super::logs::{ReadOutput, parse_time};
use super::sections::{first_line, unreadable};
use super::{HostTools, ToolsError, scripts};
use crate::ssh::ExecResult;

/// 读资源占用、详情的最长时间（`docker stats --no-stream` 要等两次采样，容器多时 1–2 秒）。
const READ_TIMEOUT: Duration = Duration::from_secs(15);

/// 详情的 Go 模板：只取界面用到的字段，拼成一个 JSON 对象（一行）。
const INSPECT_FORMAT: &str = r#"{"id":{{json .Id}},"image":{{json .Config.Image}},"created":{{json .Created}},"state":{{json .State}},"restart":{{json .HostConfig.RestartPolicy}},"restarts":{{json .RestartCount}},"ports":{{json .NetworkSettings.Ports}},"mounts":{{json .Mounts}},"networks":{{json .NetworkSettings.Networks}}}"#;

/// 一个运行中的容器的资源占用。
#[derive(Clone, Debug, PartialEq)]
pub struct ContainerStats {
    /// 容器 ID（docker 为完整的 ID；podman 可能只有开头 12 位，按前缀对应，见 [`Self::is_for`]）
    pub id: String,
    /// CPU 使用率（%；按单核计，多核时可超过 100）
    pub cpu: Option<f64>,
    /// 内存用量（字节）
    pub mem_used: Option<u64>,
    /// 内存上限（字节；没有限制时是主机的内存）
    pub mem_limit: Option<u64>,
    /// 内存用量占上限的百分比
    pub mem_percent: Option<f64>,
}

impl ContainerStats {
    /// 是不是这个容器的（ID 相同，或者是它的开头）。
    pub fn is_for(&self, id: &str) -> bool {
        self.id.len() >= 12 && id.starts_with(&self.id)
    }
}

/// 容器的详情（`docker container inspect`）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContainerDetail {
    pub id: String,
    /// 创建时使用的镜像名
    pub image: String,
    /// 创建时间（Unix 毫秒）
    pub created: Option<i64>,
    /// running、exited、restarting、paused、dead、created 等
    pub status: String,
    /// 这次启动的时间（Unix 毫秒），没有启动过时为 `None`
    pub started_at: Option<i64>,
    /// 上次停止的时间（Unix 毫秒），没有停止过时为 `None`
    pub finished_at: Option<i64>,
    /// 上次的退出码（运行中的为 `None`）
    pub exit_code: Option<i64>,
    /// 因为内存不足被结束
    pub oom_killed: bool,
    /// 启动失败等的原因（Docker 给出的），没有时为空
    pub error: String,
    /// 重启策略：no、always、unless-stopped、on-failure（podman 可能为空，当作 no）
    pub restart_policy: String,
    /// on-failure 的最多重试次数（0 为不限）
    pub max_retries: u64,
    /// 自动重启的次数
    pub restart_count: u64,
    /// 健康检查（没有设置时为 `None`）
    pub health: Option<Health>,
    /// 端口：对外发布的与只在容器内的，按容器端口排序
    pub ports: Vec<PortBinding>,
    pub mounts: Vec<MountInfo>,
    pub networks: Vec<NetworkInfo>,
}

/// 健康检查的状态。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Health {
    /// healthy、unhealthy、starting
    pub status: String,
    /// 连续失败的次数
    pub failing_streak: u64,
    /// 最近一次检查
    pub last: Option<HealthCheck>,
}

/// 一次健康检查。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HealthCheck {
    pub exit_code: i64,
    /// 检查命令的输出（去掉首尾空白，最多约 2 KB）
    pub output: String,
    /// 检查结束的时间（Unix 毫秒）
    pub at: Option<i64>,
}

/// 一个端口。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PortBinding {
    /// 容器内的端口（或范围的开头）
    pub container_port: u16,
    /// tcp、udp、sctp
    pub proto: String,
    /// 主机上的端口：没有发布到主机时为 `None`
    pub host_port: Option<u16>,
    /// 主机上监听的地址：全部地址（`0.0.0.0`、`::`、空）时为 `None`
    pub host_ip: Option<String>,
}

/// 一个挂载。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MountInfo {
    /// bind、volume、tmpfs 等
    pub kind: String,
    /// 卷名（卷才有）
    pub name: Option<String>,
    /// 主机上的路径
    pub source: String,
    /// 容器中的路径
    pub destination: String,
    pub read_only: bool,
}

/// 容器所在的一个网络。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetworkInfo {
    pub name: String,
    /// IPv4 地址，没有时为空
    pub ip: String,
    /// IPv6 地址，没有时为空
    pub ipv6: String,
}

/// 列表中的健康状态（`Status` 中的 `(healthy)` 等，或 [`RunState::health`]）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HealthState {
    Healthy,
    Unhealthy,
    Starting,
}

impl HealthState {
    /// `healthy`、`unhealthy`、`starting`（不区分大小写）；没有健康检查（`none`、空）时为 `None`。
    fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "healthy" => Some(HealthState::Healthy),
            "unhealthy" => Some(HealthState::Unhealthy),
            "starting" => Some(HealthState::Starting),
            _ => None,
        }
    }
}

/// 列表读取时 `container inspect` 取出的运行状态（`.State`）：列表据此写出本地化的状态与时长（与详情一致），
/// 不用 `docker ps` 读取那一刻的原文（`Up Less than a second` 之类，之后不再变）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunState {
    /// running、exited、restarting、paused、dead、created 等（小写）
    pub status: String,
    /// 这次启动的时间（Unix 毫秒），没有启动过时为 `None`
    pub started_at: Option<i64>,
    /// 上次停止的时间（Unix 毫秒），没有停止过时为 `None`
    pub finished_at: Option<i64>,
    /// 上次的退出码（运行中的为 `None`）
    pub exit_code: Option<i64>,
    /// 健康状态（没有健康检查时为 `None`）
    pub health: Option<HealthState>,
    /// 因为内存不足被结束
    pub oom_killed: bool,
}

impl RunState {
    /// 由 `.State` 的 JSON 得到（docker 与 podman 的写法都认）。
    pub(crate) fn from_json(state: &Value) -> Self {
        let running = state
            .get("Running")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        RunState {
            status: text_of(state.get("Status")).to_lowercase(),
            started_at: time_of(state.get("StartedAt")),
            finished_at: time_of(state.get("FinishedAt")),
            exit_code: (!running)
                .then(|| number_of(state.get("ExitCode")))
                .flatten(),
            health: health_of(state).and_then(|h| HealthState::parse(&text_of(h.get("Status")))),
            oom_killed: state
                .get("OOMKilled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }
    }
}

/// `.State` 中的健康检查（podman 较早的版本叫 Healthcheck）。
fn health_of(state: &Value) -> Option<&Value> {
    state
        .get("Health")
        .or_else(|| state.get("Healthcheck"))
        .filter(|h| h.is_object())
}

impl Container {
    /// 已退出的容器的退出码：列表读取时的运行状态中有时用它，否则看状态文字（`Exited (1) 10 minutes ago`、
    /// `Restarting (1) 5 seconds ago`）；没有时为 `None`。
    pub fn exit_code(&self) -> Option<i64> {
        if let Some(code) = self.run.as_ref().and_then(|run| run.exit_code) {
            return Some(code);
        }
        let status = self.status.trim_start();
        let rest = status
            .strip_prefix("Exited (")
            .or_else(|| status.strip_prefix("Restarting ("))?;
        rest.split_once(')')?.0.trim().parse().ok()
    }

    /// 健康状态（没有健康检查时为 `None`）：列表读取时的运行状态中有时用它，否则看状态文字。
    pub fn health(&self) -> Option<HealthState> {
        if let Some(health) = self.run.as_ref().and_then(|run| run.health) {
            return Some(health);
        }
        let status = self.status.to_lowercase();
        if status.contains("(unhealthy)") {
            Some(HealthState::Unhealthy)
        } else if status.contains("(healthy)") {
            Some(HealthState::Healthy)
        } else if status.contains("(health: starting)") || status.contains("(starting)") {
            Some(HealthState::Starting)
        } else {
            None
        }
    }
}

/// 读资源占用的命令（`sudo` 为 Docker 页经免密码 sudo 访问）。
pub(crate) fn stats_command(engine: Engine, sudo: bool) -> String {
    let mut argv: Vec<&str> = Vec::new();
    if sudo {
        argv.extend(["sudo", "-n"]);
    }
    argv.extend([
        engine.command(),
        "stats",
        "--no-stream",
        "--no-trunc",
        "--format",
        "{{json .}}",
    ]);
    sh_script(&one_line(&scripts::read_script()), &argv)
}

/// 读详情的命令。
pub(crate) fn inspect_command(engine: Engine, id: &str, sudo: bool) -> Result<String, ToolsError> {
    if !ID_RE.is_match(id) {
        return Err(ToolsError::Invalid("容器 ID 不正确".into()));
    }
    let mut argv: Vec<&str> = Vec::new();
    if sudo {
        argv.extend(["sudo", "-n"]);
    }
    argv.extend([
        engine.command(),
        "container",
        "inspect",
        "--format",
        INSPECT_FORMAT,
        id,
    ]);
    Ok(sh_script(&one_line(&scripts::read_script()), &argv))
}

/// 读取的输出与退出码；命令失败时按 Docker 的错误说明。
fn read_output(result: &ExecResult) -> Result<String, ToolsError> {
    let out = String::from_utf8_lossy(&result.stdout);
    let read = ReadOutput::parse(&out, result.stdout.len()).ok_or_else(|| unreadable(result))?;
    if read.code != 0 {
        if let Some(down) = down_error(&read.output) {
            return Err(down);
        }
        return Err(ToolsError::DockerFailed(first_line(&read.output)));
    }
    Ok(read.output)
}

/// 解析读资源占用的结果。
pub(crate) fn parse_stats(result: &ExecResult) -> Result<Vec<ContainerStats>, ToolsError> {
    let output = read_output(result)?;
    Ok(output.lines().filter_map(stats_of).collect())
}

/// 解析读详情的结果。
pub(crate) fn parse_inspect(result: &ExecResult) -> Result<ContainerDetail, ToolsError> {
    let output = read_output(result)?;
    let line = output
        .lines()
        .find(|l| l.trim_start().starts_with('{'))
        .ok_or_else(|| ToolsError::DockerFailed(first_line(&output)))?;
    detail_of(line).ok_or_else(|| ToolsError::DockerFailed(first_line(&output)))
}

/// 一行资源占用：docker 的 `CPUPerc`、`MemUsage`（`21.2MiB / 7.66GiB`）、`MemPerc` 是文字；podman 的字段名与写法
/// 不同，有的版本是数字（`CPU`、`MemUsage`、`MemLimit`、`MemPerc`）。
fn stats_of(line: &str) -> Option<ContainerStats> {
    let m = JsonObj::parse(line)?;
    let id = m.str(&["ID", "Id", "ContainerID", "container_id"]);
    if !ID_RE.is_match(&id) {
        return None;
    }
    let cpu = percent(&m.str(&["CPUPerc", "cpu_percent", "CPU"]));
    let usage = m.str(&["MemUsage", "mem_usage"]);
    let (used, limit) = match usage.split_once('/') {
        Some((used, limit)) => (parse_size(used), parse_size(limit)),
        None => (
            parse_size(&usage),
            parse_size(&m.str(&["MemLimit", "mem_limit"])),
        ),
    };
    let mem_percent = percent(&m.str(&["MemPerc", "mem_percent"])).or_else(|| {
        let (used, limit) = (used?, limit?);
        (limit > 0).then(|| used as f64 * 100. / limit as f64)
    });
    Some(ContainerStats {
        id: id.to_lowercase(),
        cpu,
        mem_used: used,
        mem_limit: limit.filter(|&l| l > 0),
        mem_percent,
    })
}

/// `12.34%`、`12.34` 换成数；`--`、空的为 `None`。
fn percent(text: &str) -> Option<f64> {
    let value: f64 = text.trim().trim_end_matches('%').trim().parse().ok()?;
    value.is_finite().then_some(value.max(0.))
}

/// 大小（`21.2MiB`、`1.5kB`、`0B`、`512`）换成字节：`KiB` 等按 1024，`kB`、`MB` 等按 1000（docker 的写法）。
pub(crate) fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let value: f64 = number.parse().ok()?;
    let scale: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.,
        "kb" | "k" => 1e3,
        "mb" | "m" => 1e6,
        "gb" | "g" => 1e9,
        "tb" | "t" => 1e12,
        "kib" => 1024.,
        "mib" => 1024f64.powi(2),
        "gib" => 1024f64.powi(3),
        "tib" => 1024f64.powi(4),
        _ => return None,
    };
    let bytes = value * scale;
    (bytes.is_finite() && bytes >= 0.).then_some(bytes.round() as u64)
}

/// Docker 的时间（`2026-11-04T09:49:04.944387787Z`）换成 Unix 毫秒；`0001-01-01T00:00:00Z`（没有）为 `None`。
fn time_of(value: Option<&Value>) -> Option<i64> {
    let nanos = parse_time(value?.as_str()?)?;
    (nanos > 0).then(|| nanos.div_euclid(1_000_000))
}

fn text_of(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn number_of(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
}

/// 健康检查的输出最多保留的字符数。
const MAX_HEALTH_OUTPUT: usize = 2000;

/// 由模板拼出的 JSON 得到详情。
fn detail_of(line: &str) -> Option<ContainerDetail> {
    let root: Value = serde_json::from_str(line.trim()).ok()?;
    let id = text_of(root.get("id"));
    let state = root.get("state").cloned().unwrap_or(Value::Null);
    let run = RunState::from_json(&state);
    let health = health_of(&state).and_then(|h| {
        let status = text_of(h.get("Status")).to_lowercase();
        if status.is_empty() || status == "none" {
            return None;
        }
        let last = h
            .get("Log")
            .and_then(Value::as_array)
            .and_then(|log| log.last())
            .map(|entry| {
                let output = text_of(entry.get("Output"));
                let output = output.trim();
                HealthCheck {
                    exit_code: number_of(entry.get("ExitCode")).unwrap_or(0),
                    output: output.chars().take(MAX_HEALTH_OUTPUT).collect(),
                    at: time_of(entry.get("End")),
                }
            });
        Some(Health {
            status,
            failing_streak: number_of(h.get("FailingStreak")).unwrap_or(0).max(0) as u64,
            last,
        })
    });
    let restart = root.get("restart");
    Some(ContainerDetail {
        image: text_of(root.get("image")),
        created: time_of(root.get("created")),
        started_at: run.started_at,
        finished_at: run.finished_at,
        exit_code: run.exit_code,
        oom_killed: run.oom_killed,
        error: text_of(state.get("Error")),
        restart_policy: restart
            .map(|r| text_of(r.get("Name")))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "no".into()),
        max_retries: restart
            .and_then(|r| number_of(r.get("MaximumRetryCount")))
            .unwrap_or(0)
            .max(0) as u64,
        restart_count: number_of(root.get("restarts")).unwrap_or(0).max(0) as u64,
        health,
        ports: ports_of(root.get("ports")),
        mounts: mounts_of(root.get("mounts")),
        networks: networks_of(root.get("networks")),
        status: run.status,
        id,
    })
}

/// `{"80/tcp":[{"HostIp":"0.0.0.0","HostPort":"8081"},{"HostIp":"::","HostPort":"8081"}],"443/tcp":null}`：
/// IPv4 与 IPv6 的同一个主机端口只记一次。
fn ports_of(value: Option<&Value>) -> Vec<PortBinding> {
    let Some(Value::Object(map)) = value else {
        return Vec::new();
    };
    let mut out: Vec<PortBinding> = Vec::new();
    for (key, bindings) in map {
        let (port, proto) = key.split_once('/').unwrap_or((key.as_str(), "tcp"));
        let Ok(container_port) = port.split('-').next().unwrap_or(port).parse::<u16>() else {
            continue;
        };
        let proto = proto.to_lowercase();
        let published: Vec<&Value> = bindings
            .as_array()
            .map(|list| list.iter().collect())
            .unwrap_or_default();
        if published.is_empty() {
            out.push(PortBinding {
                container_port,
                proto,
                host_port: None,
                host_ip: None,
            });
            continue;
        }
        for binding in published {
            let host_port = text_of(binding.get("HostPort"))
                .split('-')
                .next()
                .and_then(|p| p.parse::<u16>().ok());
            let ip = text_of(binding.get("HostIp"));
            let host_ip = match ip.as_str() {
                "" | "0.0.0.0" | "::" => None,
                _ => Some(ip),
            };
            let entry = PortBinding {
                container_port,
                proto: proto.clone(),
                host_port,
                host_ip,
            };
            if !out.contains(&entry) {
                out.push(entry);
            }
        }
    }
    out.sort();
    out
}

fn mounts_of(value: Option<&Value>) -> Vec<MountInfo> {
    let Some(Value::Array(list)) = value else {
        return Vec::new();
    };
    list.iter()
        .map(|m| MountInfo {
            kind: text_of(m.get("Type")).to_lowercase(),
            name: Some(text_of(m.get("Name"))).filter(|n| !n.is_empty()),
            source: text_of(m.get("Source")),
            destination: text_of(m.get("Destination")),
            read_only: m.get("RW").and_then(Value::as_bool) == Some(false),
        })
        .collect()
}

fn networks_of(value: Option<&Value>) -> Vec<NetworkInfo> {
    let Some(Value::Object(map)) = value else {
        return Vec::new();
    };
    let mut out: Vec<NetworkInfo> = map
        .iter()
        .map(|(name, n)| NetworkInfo {
            name: name.clone(),
            ip: text_of(n.get("IPAddress")),
            ipv6: text_of(n.get("GlobalIPv6Address")),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

impl HostTools {
    /// 读取运行中的容器的资源占用（`docker stats --no-stream`，要 1–2 秒）。`sudo` 为 Docker 页经免密码 sudo 访问。
    pub async fn container_stats(
        &self,
        target: &Target,
        engine: Engine,
        sudo: bool,
    ) -> Result<Vec<ContainerStats>, ToolsError> {
        let command = stats_command(engine, sudo);
        let target = target.clone();
        on_runtime(async move {
            let limits = Limits {
                timeout: READ_TIMEOUT,
                output: scripts::READ_TAIL + 4096,
                long: false,
            };
            let result = run(&target, &command, limits).await?;
            parse_stats(&result)
        })
        .await
    }

    /// 读取一个容器的详情（`docker container inspect`）。
    pub async fn container_detail(
        &self,
        target: &Target,
        engine: Engine,
        id: &str,
        sudo: bool,
    ) -> Result<ContainerDetail, ToolsError> {
        let command = inspect_command(engine, id, sudo)?;
        let target = target.clone();
        on_runtime(async move {
            let limits = Limits {
                timeout: READ_TIMEOUT,
                output: scripts::READ_TAIL + 4096,
                long: false,
            };
            let result = run(&target, &command, limits).await?;
            parse_inspect(&result)
        })
        .await
    }
}
