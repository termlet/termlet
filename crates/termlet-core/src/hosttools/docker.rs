//! Docker 页：容器、镜像、卷与网络（没有 docker 而有 podman 时用 podman）。

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::container::RunState;
use super::exec::{Limits, Target, on_runtime, one_line, run, sh_script};
use super::sections::{SectionExt, Sections, first_line, missing, with_result};
use super::{HostTools, LIST_LIMIT, LIST_TIMEOUT, SudoState, ToolsError, scripts};
use crate::ssh::ExecResult;

/// 容器引擎。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Engine {
    Docker,
    Podman,
}

impl Engine {
    /// 命令名。
    pub fn command(self) -> &'static str {
        match self {
            Engine::Docker => "docker",
            Engine::Podman => "podman",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "docker" => Some(Engine::Docker),
            "podman" => Some(Engine::Podman),
            _ => None,
        }
    }
}

/// 一个容器。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Container {
    /// 完整的容器 ID（操作只用它，不用名称）
    pub id: String,
    pub name: String,
    pub image: String,
    /// running、exited、paused、restarting、created 等（小写）
    pub state: String,
    /// 例如 `Up 3 days`
    pub status: String,
    /// 端口映射（与 docker 相同的写法），没有时为空
    pub ports: String,
    /// 创建时间（原文，docker 与 podman 的格式不同；podman 给的 Unix 秒换成 RFC 3339），没有时为空
    pub created: String,
    /// compose 的项目、服务与工作目录（取自 `container inspect` 的标签 JSON），没有时为 `None`
    pub project: Option<String>,
    pub service: Option<String>,
    pub working_dir: Option<String>,
    /// 运行状态（取自 `container inspect`）；读不到 inspect 时为 `None`，界面退回 `status` 的原文
    pub run: Option<RunState>,
}

/// 一个镜像。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub id: String,
    /// 悬空镜像为 `<none>`
    pub repository: String,
    pub tag: String,
    /// 大小（docker 的写法，例如 `188MB`）
    pub size: String,
    pub created: String,
    /// 创建时间（Unix 毫秒，由 `created` 换算：docker 的 `2024-05-01 10:22:33 +0000 UTC`、podman 的 RFC 3339），
    /// 换算不了时为 `None`（界面退回原文）
    pub created_at: Option<i64>,
    /// 使用它的容器数（含已停止的）；`in_use` 时不能删除
    pub containers: usize,
    pub in_use: bool,
}

/// 一个卷（只有名称，没有 ID）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    pub name: String,
    pub driver: String,
    pub mountpoint: String,
    pub containers: usize,
    /// 读不到各容器的挂载时（权限变化等），有容器就当作使用中（不确定时不让删除）
    pub in_use: bool,
}

/// 一个网络。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Network {
    pub id: String,
    pub name: String,
    pub driver: String,
    pub scope: String,
    /// 内置的网络（bridge、host、none，podman 的 podman），不能删除
    pub builtin: bool,
    pub containers: usize,
    pub in_use: bool,
}

/// Docker 页的内容。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DockerInfo {
    pub engine: Engine,
    /// 经免密码 sudo 读取（这个用户不在 docker 组）：操作、日志也会用 `sudo -n`，在终端中执行的命令要带 sudo
    pub sudo: bool,
    /// 引擎的版本（`29.3.0`）与 Compose 的版本（`2.39.1`），读不到时为 `None`
    pub version: Option<String>,
    pub compose: Option<String>,
    /// 按 compose 项目、名称排序（没有项目的在前）
    pub containers: Vec<Container>,
    pub images: Vec<Image>,
    pub volumes: Vec<Volume>,
    pub networks: Vec<Network>,
}

/// 容器、镜像、网络的 ID（可带 `sha256:` 前缀，至少 12 位）。
pub(crate) static ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(sha256:)?[0-9a-f]{12,64}$").expect("正则表达式"));
/// 卷名。
pub(crate) static VOLUME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,254}$").expect("正则表达式"));

/// 各容器的名称（进程页标出进程所在的容器时用，还没有 Docker 页的读数时另读）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerNames {
    pub engine: Engine,
    /// 经免密码 sudo 读取（与 Docker 页的判断相同）
    pub sudo: bool,
    /// 完整的容器 ID → 名称
    pub names: HashMap<String, String>,
}

/// 读取 Docker 页的命令。
pub(crate) fn docker_command() -> String {
    sh_script::<&str>(&one_line(&scripts::docker_script()), &[])
}

/// 读取容器名的命令。
pub(crate) fn container_names_command() -> String {
    sh_script::<&str>(&one_line(&scripts::container_names_script()), &[])
}

/// 解析读取容器名的输出：每行“ID 名称”（名称经 `q` 输出，以 `/` 开头的去掉）；ID 不对的行跳过。
pub(crate) fn parse_container_names(out: &str) -> Result<ContainerNames, ToolsError> {
    let sections = Sections::parse(out);
    let (engine, sudo) = parse_access(&sections)?;
    let names = sections
        .get("names")
        .all_lines()
        .into_iter()
        .filter_map(|line| {
            let (id, name) = line.trim().split_once(' ')?;
            let id = id.to_lowercase();
            let name = name.trim();
            let name = name.strip_prefix('/').unwrap_or(name);
            (ID_RE.is_match(&id) && !name.is_empty()).then(|| (id, name.to_string()))
        })
        .collect();
    Ok(ContainerNames {
        engine,
        sudo,
        names,
    })
}

impl HostTools {
    /// 读取各容器的名称（`docker ps` 的 ID 与名称，没有 docker 而有 podman 时用 podman；不在 docker 组时经免密码
    /// sudo，与 [`Self::docker`] 相同）。错误与 [`Self::docker`] 相同。
    pub async fn container_names(&self, target: &Target) -> Result<ContainerNames, ToolsError> {
        let target = target.clone();
        on_runtime(async move {
            let limits = Limits {
                timeout: LIST_TIMEOUT,
                output: LIST_LIMIT,
                long: false,
            };
            let result = run(&target, &container_names_command(), limits).await?;
            parse_container_names(&String::from_utf8_lossy(&result.stdout))
                .map_err(|e| with_result(e, &result))
        })
        .await
    }

    /// 读取容器、镜像、卷与网络。没有 docker、podman 时返回 [`ToolsError::NoDocker`]；守护进程没有运行时返回
    /// [`ToolsError::DockerDown`]；没有权限且没有免密码 sudo 时返回 [`ToolsError::NeedRoot`]。
    pub async fn docker(&self, target: &Target) -> Result<DockerInfo, ToolsError> {
        let target = target.clone();
        on_runtime(async move {
            let limits = Limits {
                timeout: LIST_TIMEOUT,
                output: LIST_LIMIT,
                long: true,
            };
            let result = run(&target, &docker_command(), limits).await?;
            finish_docker(&result)
        })
        .await
    }
}

/// 解析读取 Docker 页的执行结果。
pub(crate) fn finish_docker(result: &ExecResult) -> Result<DockerInfo, ToolsError> {
    parse_docker(&String::from_utf8_lossy(&result.stdout)).map_err(|e| with_result(e, result))
}

/// 访问 Docker 的结果（`scripts::DOCKER_ACCESS` 的输出）：用的引擎与是否经 sudo；没有 docker、没有权限、守护进程
/// 没有运行等时为错误。
pub(crate) fn parse_access(sections: &Sections) -> Result<(Engine, bool), ToolsError> {
    if sections.has("nodocker") {
        return Err(ToolsError::NoDocker);
    }
    let engine = sections
        .get("engine")
        .and_then(|s| Engine::parse(&s.arg))
        .ok_or_else(missing)?;
    let sudo = sections.get("sudo").arg().trim().to_string();
    let desktop = sections.has("desktop");
    if let Some(error) = sections.get("error") {
        let text = error.lines.join("\n");
        let root = match sudo.as_str() {
            "password" => Some(SudoState::Password),
            "denied" => Some(SudoState::Denied),
            "none" => Some(SudoState::NotInstalled),
            _ => None,
        };
        return Err(match root {
            Some(sudo) => ToolsError::NeedRoot { sudo, output: text },
            None => match down_error(&text) {
                // Docker Desktop 在运行、但没有为这个发行版打开 WSL 集成时，说的是连不上 docker.sock
                Some(_) if desktop => ToolsError::DockerDesktopDown,
                Some(down) => down,
                // 已经是 root（或 sudo 也没有权限，例如 SELinux）
                None if text.to_lowercase().contains("permission denied") => ToolsError::NeedRoot {
                    sudo: SudoState::Denied,
                    output: text,
                },
                None => ToolsError::DockerFailed(first_line(&text)),
            },
        });
    }
    Ok((engine, sudo == "used"))
}

/// 解析 Docker 页的输出。
pub(crate) fn parse_docker(out: &str) -> Result<DockerInfo, ToolsError> {
    let sections = Sections::parse(out);
    let (engine, sudo) = parse_access(&sections)?;
    let mut info = DockerInfo {
        engine,
        sudo,
        version: version_of(sections.get("version").first()),
        compose: version_of(sections.get("compose").first()),
        containers: Vec::new(),
        images: Vec::new(),
        volumes: Vec::new(),
        networks: Vec::new(),
    };
    for line in sections.get("containers").all_lines() {
        if let Some(container) = parse_container(line) {
            info.containers.push(container);
        }
    }
    for line in sections.get("images").all_lines() {
        if let Some(image) = parse_image(line) {
            info.images.push(image);
        }
    }
    for line in sections.get("volumes").all_lines() {
        if let Some(volume) = parse_volume(line) {
            info.volumes.push(volume);
        }
    }
    for line in sections.get("networks").all_lines() {
        if let Some(network) = parse_network(line) {
            info.networks.push(network);
        }
    }
    let usage = parse_inspect(&sections.get("inspect").all_lines());
    mark_usage(&mut info, &usage);
    // compose 项目等以 inspect 的标签（JSON）为准；docker 读不到 inspect 时不分组（ps 的 Labels 字符串可以伪造）
    for container in &mut info.containers {
        let id = bare_id(&container.id);
        if let Some(labels) = usage.labels.get(&id) {
            set_compose(container, Some(labels));
        }
        container.run = usage.states.get(&id).cloned();
    }
    let text = |s: &Option<String>| s.clone().unwrap_or_default();
    info.containers.sort_by(|a, b| {
        text(&a.project)
            .cmp(&text(&b.project))
            .then_with(|| a.name.cmp(&b.name))
    });
    info.images.sort_by(|a, b| {
        a.repository
            .cmp(&b.repository)
            .then_with(|| a.tag.cmp(&b.tag))
    });
    info.volumes.sort_by(|a, b| a.name.cmp(&b.name));
    info.networks.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(info)
}

/// 版本号（`29.3.0`、`v2.39.1`、`5.4.0-dev`）：去掉开头的 `v`，以及发行版打包时加的后缀（`26.1.5+dfsg1` 的 `+…`、
/// `2.26.1-4` 的 Debian 修订号）；不像版本号的（空的、太长的、有空白等）为 `None`。
pub(crate) fn version_of(text: &str) -> Option<String> {
    static VERSION_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[0-9][0-9A-Za-z.+~_-]{0,39}$").expect("正则表达式"));
    let text = text.trim();
    let text = text.strip_prefix('v').unwrap_or(text);
    if !VERSION_RE.is_match(text) {
        return None;
    }
    let text = text.split('+').next().unwrap_or(text);
    let text = match text.rsplit_once('-') {
        Some((head, revision))
            if head.contains('.') && revision.bytes().all(|b| b.is_ascii_digit()) =>
        {
            head
        }
        _ => text,
    };
    Some(text.to_string())
}

/// 镜像的创建时间换成 Unix 毫秒：docker 的 `2024-05-01 10:22:33 +0000 UTC`（Go 的默认写法，时区缩写不看）与
/// RFC 3339（podman）；换算不了时为 `None`。
pub(crate) fn created_at(text: &str) -> Option<i64> {
    let text = text.trim();
    let nanos = match text.split_whitespace().collect::<Vec<_>>().as_slice() {
        [day, time, zone, ..] => super::logs::parse_time(&format!("{day}T{time}{zone}")),
        _ => super::logs::parse_time(text),
    }?;
    Some(nanos.div_euclid(1_000_000))
}

/// 错误输出说明 Docker 用不了（守护进程没有运行）时对应的错误：WSL 中 Docker Desktop 的集成用不了时是
/// [`ToolsError::DockerDesktopDown`]，连不上守护进程是 [`ToolsError::DockerDown`]；别的错误为 `None`。
pub(crate) fn down_error(text: &str) -> Option<ToolsError> {
    if desktop_down(text) {
        Some(ToolsError::DockerDesktopDown)
    } else if docker_down(text) {
        Some(ToolsError::DockerDown)
    } else {
        None
    }
}

/// 错误输出是不是连不上守护进程（较新的 docker 命令写作 "failed to connect to the docker API"）。
fn docker_down(text: &str) -> bool {
    text.contains("Cannot connect to the Docker daemon")
        || text.contains("Is the docker daemon running")
        || text.contains("failed to connect to the docker API")
        || text.contains("error during connect")
        || text.contains("Cannot connect to Podman")
}

/// 错误输出是不是 WSL 中 Docker Desktop 的集成程序用不了：Docker Desktop 没有启动（Windows 一侧的 docker 说
/// "could not be found in this WSL 2 distro"），或者启动过又退出、发行版一直开着（/usr/bin/docker 指向的
/// /mnt/wsl/docker-desktop 下的挂载已经失效，执行它报 Input/output error：经 sh 是 "docker: …"，经 bash 与 Windows
/// 一侧的 docker 是 "/usr/bin/docker: …"）。
fn desktop_down(text: &str) -> bool {
    text.contains("could not be found in this WSL 2 distro")
        || text.contains("docker: Input/output error")
}

/// 一行 JSON 对象，键不区分大小写（docker 与 podman 的字段名大小写不同）。
pub(crate) struct JsonObj(HashMap<String, Value>);

impl JsonObj {
    pub fn parse(line: &str) -> Option<Self> {
        Self::from_value(serde_json::from_str::<Value>(line).ok()?)
    }

    /// 由已经解析的 JSON 得到；不是对象时为 `None`。
    pub fn from_value(value: Value) -> Option<Self> {
        let Value::Object(map) = value else {
            return None;
        };
        Some(JsonObj(
            map.into_iter()
                .map(|(k, v)| (k.to_lowercase(), v))
                .collect(),
        ))
    }

    fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(&key.to_lowercase()).filter(|v| !v.is_null())
    }

    /// 第一个存在的键的值：字符串原样，字符串数组以逗号连接，数字转成文字，其余为空。
    pub fn str(&self, keys: &[&str]) -> String {
        for key in keys {
            match self.get(key) {
                Some(Value::String(s)) => return s.clone(),
                Some(Value::Array(items)) => {
                    let texts: Option<Vec<&str>> = items.iter().map(Value::as_str).collect();
                    if let Some(texts) = texts {
                        return texts.join(",");
                    }
                }
                Some(Value::Number(n)) => return n.to_string(),
                _ => {}
            }
        }
        String::new()
    }

    /// podman 的标签（JSON 对象）。docker 的 Labels 是 `k=v,k2=v2` 的字符串，标签值中写进
    /// `,com.docker.compose.project=prod` 就能伪造出别的标签（例如把容器归入别的 compose 项目，整体启停时被带上），
    /// 不用它：docker 的标签取自 `container inspect` 的 JSON（见 [`parse_inspect`]）。不是对象时为 `None`。
    pub fn labels(&self) -> Option<HashMap<String, String>> {
        let Value::Object(map) = self.get("labels")? else {
            return None;
        };
        map.iter()
            .map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
            .collect()
    }

    /// 端口映射的文字（docker 原样；podman 是数组，写成与 docker 相同的格式）。
    fn ports(&self) -> String {
        match self.get("ports") {
            Some(Value::String(s)) => s.clone(),
            Some(value @ Value::Array(_)) => {
                let Ok(list) = serde_json::from_value::<Vec<PodmanPort>>(value.clone()) else {
                    return String::new();
                };
                list.iter()
                    .map(|p| {
                        let host = if p.host_ip.is_empty() {
                            "0.0.0.0"
                        } else {
                            &p.host_ip
                        };
                        let proto = if p.protocol.is_empty() {
                            "tcp"
                        } else {
                            &p.protocol
                        };
                        if p.host_port > 0 {
                            format!("{host}:{}->{}/{proto}", p.host_port, p.container_port)
                        } else {
                            format!("{}/{proto}", p.container_port)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            }
            _ => String::new(),
        }
    }

    /// 创建时间：文字原样，Unix 秒（podman 的 Created）换成 RFC 3339。
    fn created(&self) -> String {
        let text = self.str(&["CreatedAt"]);
        if !text.is_empty() {
            return text;
        }
        match self.str(&["Created"]).parse::<i64>() {
            Ok(seconds) if seconds > 0 => chrono::DateTime::from_timestamp(seconds, 0)
                .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                .unwrap_or_default(),
            _ => String::new(),
        }
    }
}

/// podman 的端口映射。
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct PodmanPort {
    host_ip: String,
    container_port: u32,
    host_port: u32,
    protocol: String,
}

/// 按标签填写容器的 compose 项目、服务与工作目录（没有时为 `None`）。
fn set_compose(container: &mut Container, labels: Option<&HashMap<String, String>>) {
    let label = |key: &str| {
        labels
            .and_then(|l| l.get(key))
            .filter(|v| !v.is_empty())
            .cloned()
    };
    container.project =
        label("com.docker.compose.project").or_else(|| label("io.podman.compose.project"));
    container.service = label("com.docker.compose.service");
    container.working_dir = label("com.docker.compose.project.working_dir");
}

fn parse_container(line: &str) -> Option<Container> {
    let m = JsonObj::parse(line)?;
    let name = m.str(&["Names"]);
    let mut container = Container {
        id: m.str(&["ID", "Id"]),
        name: name.strip_prefix('/').unwrap_or(&name).to_string(),
        image: m.str(&["Image"]),
        state: m.str(&["State"]).to_lowercase(),
        status: m.str(&["Status"]),
        ports: m.ports(),
        created: m.created(),
        project: None,
        service: None,
        working_dir: None,
        run: None,
    };
    if !ID_RE.is_match(&container.id) {
        return None;
    }
    set_compose(&mut container, m.labels().as_ref());
    Some(container)
}

fn parse_image(line: &str) -> Option<Image> {
    let m = JsonObj::parse(line)?;
    let mut image = Image {
        id: m.str(&["ID", "Id"]),
        repository: m.str(&["Repository"]),
        tag: m.str(&["Tag"]),
        size: m.str(&["Size"]),
        created: m.created(),
        created_at: None,
        containers: 0,
        in_use: false,
    };
    image.created_at = created_at(&image.created);
    if image.repository.is_empty() {
        // podman：Names / RepoTags 是"仓库:标签"的数组
        let names = m.str(&["Names", "RepoTags"]);
        let name = names.split(',').next().unwrap_or("");
        match cut_tag(name) {
            Some((repository, tag)) => {
                image.repository = repository.to_string();
                image.tag = tag.to_string();
            }
            None => {
                image.repository = "<none>".into();
                image.tag = "<none>".into();
            }
        }
    }
    // podman 给的是字节数
    if let Ok(bytes) = image.size.parse::<i64>() {
        image.size = human_size(bytes);
    }
    ID_RE.is_match(&image.id).then_some(image)
}

/// 把"仓库:标签"分开（仓库地址中可能有端口号的冒号，取最后一个 `/` 之后的冒号）；没有标签时为 latest。
pub(crate) fn cut_tag(name: &str) -> Option<(&str, &str)> {
    if name.is_empty() {
        return None;
    }
    let slash = name.rfind('/');
    match name.rfind(':') {
        Some(colon) if slash.is_none_or(|s| colon > s) => {
            Some((&name[..colon], &name[colon + 1..]))
        }
        _ => Some((name, "latest")),
    }
}

/// 把字节数写成 docker 的格式（十进制单位，三位有效数字）。
pub(crate) fn human_size(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        return format!("{bytes}B");
    }
    let digits = (value as i64).to_string().len();
    let precision = 3usize.saturating_sub(digits);
    format!("{value:.precision$}{}", UNITS[unit])
}

fn parse_volume(line: &str) -> Option<Volume> {
    let m = JsonObj::parse(line)?;
    let volume = Volume {
        name: m.str(&["Name"]),
        driver: m.str(&["Driver"]),
        mountpoint: m.str(&["Mountpoint"]),
        containers: 0,
        in_use: false,
    };
    VOLUME_RE.is_match(&volume.name).then_some(volume)
}

/// 不能删除的内置网络。
pub(crate) const BUILTIN_NETWORKS: &[&str] = &["bridge", "host", "none", "podman"];

fn parse_network(line: &str) -> Option<Network> {
    let m = JsonObj::parse(line)?;
    let name = m.str(&["Name"]);
    let network = Network {
        id: m.str(&["ID", "Id"]),
        builtin: BUILTIN_NETWORKS.contains(&name.as_str()),
        name,
        driver: m.str(&["Driver"]),
        scope: m.str(&["Scope"]),
        containers: 0,
        in_use: false,
    };
    ID_RE.is_match(&network.id).then_some(network)
}

/// `container inspect` 得到的各容器所用的镜像、卷与网络，以及各容器的标签与运行状态。
#[derive(Default)]
pub(crate) struct Usage {
    /// 镜像 ID（不带 `sha256:` 前缀）→ 容器数
    images: HashMap<String, usize>,
    volumes: HashMap<String, usize>,
    networks: HashMap<String, usize>,
    /// 容器 ID（小写，不带前缀）→ 标签（`.Config.Labels` 的 JSON，没有标签时为空表）
    labels: HashMap<String, HashMap<String, String>>,
    /// 容器 ID（小写，不带前缀）→ 运行状态（`.State`）
    states: HashMap<String, RunState>,
    ok: bool,
}

/// `@@inspect` 一行的最后一项：标签与运行状态。
#[derive(serde::Deserialize)]
struct InspectTail {
    #[serde(default)]
    labels: Option<HashMap<String, String>>,
    #[serde(default)]
    state: Option<Value>,
}

/// 解析 `@@inspect` 段：ID|镜像 ID|卷,…|网络,…|`{"labels":…,"state":…}`。卷名、网络名中没有 `|`（docker 限制了
/// 写法），JSON 放在最后，其中的 `|` 不影响前面各项。
fn parse_inspect(lines: &[&str]) -> Usage {
    let mut usage = Usage::default();
    for line in lines {
        let fields: Vec<&str> = line.trim().splitn(5, '|').collect();
        if fields.len() < 4 {
            continue;
        }
        usage.ok = true;
        if let Some(json) = fields.get(4)
            && let Ok(tail) = serde_json::from_str::<InspectTail>(json)
        {
            let id = bare_id(fields[0]);
            if let Some(state) = tail.state.as_ref().filter(|s| s.is_object()) {
                usage.states.insert(id.clone(), RunState::from_json(state));
            }
            usage.labels.insert(id, tail.labels.unwrap_or_default());
        }
        let image = bare_id(fields[1]);
        if !image.is_empty() {
            *usage.images.entry(image).or_default() += 1;
        }
        for volume in unique(fields[2]) {
            *usage.volumes.entry(volume).or_default() += 1;
        }
        for network in unique(fields[3]) {
            *usage.networks.entry(network).or_default() += 1;
        }
    }
    usage
}

/// 以逗号分隔的各项，去掉空项与重复的。
pub(crate) fn unique(list: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in list.split(',').map(str::trim) {
        if !item.is_empty() && !out.iter().any(|seen| seen == item) {
            out.push(item.to_string());
        }
    }
    out
}

/// 去掉 `sha256:` 前缀并转成小写。
pub(crate) fn bare_id(id: &str) -> String {
    let id = id.trim();
    id.strip_prefix("sha256:").unwrap_or(id).to_lowercase()
}

/// 标出使用中的镜像、卷与网络。读不到 inspect 的结果时（例如权限变化），按容器列表中的镜像名推断，
/// 卷与网络一律当作使用中（不确定时不让删除）。
fn mark_usage(info: &mut DockerInfo, usage: &Usage) {
    let has_containers = !info.containers.is_empty();
    for image in &mut info.images {
        let id = bare_id(&image.id);
        if usage.ok {
            image.containers = usage.images.get(&id).copied().unwrap_or(0);
        } else {
            image.containers = info
                .containers
                .iter()
                .filter(|c| {
                    let reference = &c.image;
                    *reference == format!("{}:{}", image.repository, image.tag)
                        || (image.tag == "latest" && *reference == image.repository)
                        || (reference.len() >= 12 && id.starts_with(&bare_id(reference)))
                })
                .count();
        }
        image.in_use = image.containers > 0;
    }
    for volume in &mut info.volumes {
        volume.containers = usage.volumes.get(&volume.name).copied().unwrap_or(0);
        volume.in_use = volume.containers > 0 || (!usage.ok && has_containers);
    }
    for network in &mut info.networks {
        network.containers = usage.networks.get(&network.name).copied().unwrap_or(0);
        network.in_use = network.containers > 0 || (!usage.ok && has_containers);
    }
}
