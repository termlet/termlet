//! Docker 页的清理：先读出各类可以清理的对象与可释放的空间（只读：`docker system df -v` 与各列表，见
//! `scripts::CLEANUP_BODY`），确认后逐类执行 `docker container prune -f` 等（[`super::Action::Prune`]），由输出得到实际
//! 删除的个数与释放的空间（[`parse_prune`]）。
//!
//! 清单按 Docker 自己的规则算出这一次 prune 会删除什么（[`CleanupPlan::preview`]）：
//! - 容器：已停止的（exited、created、dead；运行中、暂停、重启中的不动）。
//! - 网络：自建的、没有容器连着的（Docker 只看正连着的容器，停止的容器所在的网络也会被清理；Podman 看全部容器）。
//! - 卷：没有容器使用的；Docker 23（API 1.42）起 `volume prune` 只清理匿名卷，命名的卷不在其中。
//! - 镜像：悬空的（没有标签的）、没有容器使用的。
//! - 构建缓存：没有在用的（只有 Docker）。
//!
//! 执行的先后与 `docker system prune` 相同：先容器，再网络、卷、镜像、构建缓存。一并清理已停止的容器时，只被它们
//! 使用的网络、卷与镜像也会在同一次清理中删除，清单按勾选现算。

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::container::parse_size;
use super::docker::{
    BUILTIN_NETWORKS, Engine, ID_RE, JsonObj, VOLUME_RE, bare_id, parse_access, unique,
};
use super::exec::{Limits, Target, on_runtime, one_line, run, sh_script};
use super::sections::{SectionExt, Sections, with_result};
use super::{HostTools, ToolsError, scripts};
use crate::ssh::ExecResult;

/// 读清单的最长时间（`system df -v` 要算各卷的大小，卷大时很慢；脚本中另有 60 秒的限制）。
const PLAN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);
/// 读清单时输出的上限。
const PLAN_LIMIT: usize = 8 << 20;

/// 可以清理的一类对象（按执行的先后）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PruneKind {
    /// 已停止的容器
    Containers,
    /// 未使用的网络
    Networks,
    /// 未使用的卷（Docker 23 起只有匿名卷）
    Volumes,
    /// 悬空镜像
    Images,
    /// 构建缓存（只有 Docker）
    BuildCache,
}

impl PruneKind {
    /// 全部，按执行的先后（与 `docker system prune` 相同）。
    pub const ALL: [PruneKind; 5] = [
        PruneKind::Containers,
        PruneKind::Networks,
        PruneKind::Volumes,
        PruneKind::Images,
        PruneKind::BuildCache,
    ];

    /// 命令中的对象名（`docker container prune`）。
    pub(crate) fn word(self) -> &'static str {
        match self {
            PruneKind::Containers => "container",
            PruneKind::Networks => "network",
            PruneKind::Volumes => "volume",
            PruneKind::Images => "image",
            PruneKind::BuildCache => "builder",
        }
    }
}

/// 一个容器（清单用）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanContainer {
    /// 完整的 ID
    pub id: String,
    pub name: String,
    /// running、exited、created、dead 等
    pub state: String,
    /// 镜像 ID（不带 `sha256:`）
    pub image: String,
    /// 挂载的卷
    pub volumes: Vec<String>,
    /// 所在的网络（名称）
    pub networks: Vec<String>,
    /// 可写层的大小（字节）；读不到时为 `None`
    pub size: Option<u64>,
}

impl PlanContainer {
    /// `container prune` 会删除它：已停止（运行中、暂停、重启中的不动）。
    pub fn stopped(&self) -> bool {
        !matches!(
            self.state.as_str(),
            "running" | "paused" | "restarting" | "removing" | "stopping"
        )
    }
}

/// 一个悬空镜像。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanImage {
    /// ID（不带 `sha256:`）
    pub id: String,
    /// 独有的大小（与别的镜像共用的层不算，删除后实际释放的）；读不到时为 `None`
    pub size: Option<u64>,
    /// 使用它的容器数（`system df -v` 的 Containers）；读不到时为 `None`
    pub containers: Option<u32>,
}

/// 一个卷。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanVolume {
    pub name: String,
    /// 匿名卷（`com.docker.volume.anonymous` 标签，Docker 23 起才有）
    pub anonymous: bool,
    pub size: Option<u64>,
    /// 使用它的容器数（`system df -v` 的 Links）；读不到时为 `None`
    pub links: Option<u32>,
}

/// 一个自建的网络（内置的不在其中）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanNetwork {
    /// 完整的 ID
    pub id: String,
    pub name: String,
    /// 正连着它的容器（Docker 的 `network inspect`）；读不到（Podman）时为 `None`，按全部容器所在的网络算
    pub endpoints: Option<Vec<String>>,
}

/// 构建缓存。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BuildCacheUsage {
    /// 没有在用的缓存记录数
    pub entries: usize,
    /// 大约可以释放的空间（没有在用、没有与镜像共用的记录的大小）
    pub reclaimable: u64,
}

/// 清理前读到的清单。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanupPlan {
    pub engine: Engine,
    /// 经免密码 sudo 读取（与 Docker 页相同）：清理也经 sudo
    pub sudo: bool,
    /// `volume prune` 只清理匿名卷（Docker 23 起；Podman、较早的 Docker 清理全部未使用的卷）
    pub anonymous_volumes_only: bool,
    /// 读到了 `system df -v`（各项的大小）
    pub sizes: bool,
    /// 读到了全部容器的挂载、镜像与网络（`container inspect`）；读不全时卷与镜像按 df 中使用它们的容器数算
    pub containers_known: bool,
    pub containers: Vec<PlanContainer>,
    pub dangling: Vec<PlanImage>,
    pub volumes: Vec<PlanVolume>,
    pub networks: Vec<PlanNetwork>,
    /// 构建缓存；Podman、读不到时为 `None`
    pub build_cache: Option<BuildCacheUsage>,
}

/// 清单中的一项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PruneItem {
    /// 区分各项的键（容器、镜像、网络的 ID，卷名）
    pub key: String,
    /// 显示的名称（容器名、网络名、卷名，镜像是 ID 的前 12 位）
    pub name: String,
    pub size: Option<u64>,
}

/// 一类将要清理的内容。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrunePreview {
    pub kind: PruneKind,
    /// 将要删除的对象（构建缓存没有逐项列出，见 `entries`）
    pub items: Vec<PruneItem>,
    /// 个数（构建缓存是记录数）
    pub count: usize,
    /// 可释放的空间（字节）；读不到大小时为 `None`（网络没有大小，为 `Some(0)`）
    pub bytes: Option<u64>,
}

impl CleanupPlan {
    /// 这台主机可以清理的几类（Podman 没有构建缓存）。
    pub fn kinds(&self) -> Vec<PruneKind> {
        PruneKind::ALL
            .into_iter()
            .filter(|kind| *kind != PruneKind::BuildCache || self.build_cache.is_some())
            .collect()
    }

    /// `kind` 将删除的内容。`with_containers` 为一并清理已停止的容器（它先执行）：只被它们使用的网络、卷与镜像也算在内。
    pub fn preview(&self, kind: PruneKind, with_containers: bool) -> PrunePreview {
        let size = |size: Option<u64>| if self.sizes { size } else { None };
        let remaining: Vec<&PlanContainer> = self
            .containers
            .iter()
            .filter(|c| !(with_containers && c.stopped()))
            .collect();
        let items: Vec<PruneItem> = match kind {
            PruneKind::Containers => self
                .containers
                .iter()
                .filter(|c| c.stopped())
                .map(|c| PruneItem {
                    key: c.id.clone(),
                    name: c.name.clone(),
                    size: size(c.size),
                })
                .collect(),
            PruneKind::Networks => {
                let held: HashSet<&str> = remaining
                    .iter()
                    .flat_map(|c| c.networks.iter().map(String::as_str))
                    .collect();
                let remaining_ids: HashSet<&str> =
                    remaining.iter().map(|c| c.id.as_str()).collect();
                self.networks
                    .iter()
                    .filter(|n| match &n.endpoints {
                        // Docker：只看正连着的容器（都是运行中的，不会被清理）
                        Some(endpoints) => !endpoints
                            .iter()
                            .any(|id| remaining_ids.contains(id.as_str())),
                        None => !held.contains(n.name.as_str()),
                    })
                    .map(|n| PruneItem {
                        key: n.id.clone(),
                        name: n.name.clone(),
                        size: Some(0),
                    })
                    .collect()
            }
            PruneKind::Volumes => {
                let used: HashSet<&str> = remaining
                    .iter()
                    .flat_map(|c| c.volumes.iter().map(String::as_str))
                    .collect();
                // 读不全各容器的挂载时按 df 的 Links 算（也读不到时列出：宁可多列，不让清理删掉清单之外的）
                self.volumes
                    .iter()
                    .filter(|v| v.anonymous || !self.anonymous_volumes_only)
                    .filter(|v| match self.containers_known {
                        true => !used.contains(v.name.as_str()),
                        false => v.links.is_none_or(|n| n == 0),
                    })
                    .map(|v| PruneItem {
                        key: v.name.clone(),
                        name: v.name.clone(),
                        size: size(v.size),
                    })
                    .collect()
            }
            PruneKind::Images => {
                let used: HashSet<&str> = remaining.iter().map(|c| c.image.as_str()).collect();
                self.dangling
                    .iter()
                    .filter(|im| match self.containers_known {
                        true => !used.contains(im.id.as_str()),
                        false => im.containers.is_none_or(|n| n == 0),
                    })
                    .map(|im| PruneItem {
                        key: im.id.clone(),
                        name: im.id.get(..12).unwrap_or(&im.id).to_string(),
                        size: size(im.size),
                    })
                    .collect()
            }
            PruneKind::BuildCache => {
                let cache = self.build_cache.unwrap_or_default();
                return PrunePreview {
                    kind,
                    items: Vec::new(),
                    count: cache.entries,
                    bytes: Some(cache.reclaimable),
                };
            }
        };
        let bytes = if kind == PruneKind::Networks {
            Some(0)
        } else if self.sizes {
            Some(items.iter().filter_map(|i| i.size).sum())
        } else {
            None
        };
        PrunePreview {
            kind,
            count: items.len(),
            items,
            bytes,
        }
    }

    /// Docker 23 起 `volume prune` 不清理的命名卷中没有容器使用的个数（界面说明它们要在“卷”页逐个删除）。
    pub fn named_unused_volumes(&self, with_containers: bool) -> usize {
        if !self.anonymous_volumes_only {
            return 0;
        }
        let unrestricted = CleanupPlan {
            anonymous_volumes_only: false,
            ..self.clone()
        };
        unrestricted
            .preview(PruneKind::Volumes, with_containers)
            .items
            .len()
            - self
                .preview(PruneKind::Volumes, with_containers)
                .items
                .len()
    }
}

/// 一次 prune 的结果（由它的输出得到）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PruneOutcome {
    /// 删除的个数；看不出时（镜像、构建缓存）为 `None`
    pub deleted: Option<usize>,
    /// 释放的空间（字节）；没有写出时（网络、Podman）为 `None`
    pub reclaimed: Option<u64>,
}

/// 解析 prune 的输出。Docker 写作 `Deleted Containers:`（之后每行一个，空行结束）与 `Total reclaimed space: 1.2GB`，
/// 构建缓存是一张表与 `Total: 56B`，什么也没删除时只有 `Total reclaimed space: 0B` 或什么都没有；Podman 每行一个
/// ID 或名称，不写释放的空间。镜像的删除列表中一个镜像可能有几行（各层），不数。
pub fn parse_prune(kind: PruneKind, output: &str) -> PruneOutcome {
    let mut reclaimed = None;
    let mut docker_style = false;
    let mut block: Option<usize> = None;
    let mut in_block = false;
    let mut plain = 0usize;
    for line in output.lines() {
        let text = line.trim();
        if let Some(size) = text.strip_prefix("Total reclaimed space:") {
            reclaimed = parse_size(size.trim());
            docker_style = true;
            in_block = false;
            continue;
        }
        if let Some(size) = text.strip_prefix("Total:") {
            reclaimed = parse_size(size.trim());
            docker_style = true;
            in_block = false;
            continue;
        }
        if text.starts_with("Deleted ") && text.ends_with(':') {
            docker_style = true;
            in_block = true;
            block.get_or_insert(0);
            continue;
        }
        if text.is_empty() {
            in_block = false;
            continue;
        }
        if text.starts_with("ID ") || text.starts_with("ID\t") {
            // 构建缓存的表头
            docker_style = true;
            continue;
        }
        if in_block && let Some(n) = block.as_mut() {
            *n += 1;
        }
        plain += 1;
    }
    let deleted = match kind {
        PruneKind::Images | PruneKind::BuildCache if docker_style => None,
        _ if docker_style => Some(block.unwrap_or(0)),
        _ => Some(plain),
    };
    PruneOutcome { deleted, reclaimed }
}

/// 读取清单的命令。
pub(crate) fn cleanup_command() -> String {
    sh_script::<&str>(&one_line(&scripts::cleanup_script()), &[])
}

impl HostTools {
    /// 读取 Docker 清理的清单（只读）。没有 docker、podman 时返回 [`ToolsError::NoDocker`]，守护进程没有运行时返回
    /// [`ToolsError::DockerDown`]，没有权限且没有免密码 sudo 时返回 [`ToolsError::NeedRoot`]（与 [`Self::docker`] 相同）。
    pub async fn cleanup_plan(&self, target: &Target) -> Result<CleanupPlan, ToolsError> {
        let target = target.clone();
        on_runtime(async move {
            let limits = Limits {
                timeout: PLAN_TIMEOUT,
                output: PLAN_LIMIT,
                long: true,
            };
            let result = run(&target, &cleanup_command(), limits).await?;
            finish_cleanup(&result)
        })
        .await
    }
}

/// 解析读取清单的执行结果。
pub(crate) fn finish_cleanup(result: &ExecResult) -> Result<CleanupPlan, ToolsError> {
    parse_cleanup(&String::from_utf8_lossy(&result.stdout)).map_err(|e| with_result(e, result))
}

/// 解析清单的输出。
pub(crate) fn parse_cleanup(out: &str) -> Result<CleanupPlan, ToolsError> {
    let sections = Sections::parse(out);
    let (engine, sudo) = parse_access(&sections)?;
    let df = sections
        .get("df")
        .all_lines()
        .first()
        .and_then(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .filter(Value::is_object);
    let sizes = df.is_some();
    let df_list = |key: &str| -> Vec<JsonObj> {
        df.as_ref()
            .and_then(|df| df.get(key))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| JsonObj::from_value(item.clone()))
                    .collect()
            })
            .unwrap_or_default()
    };
    // df 中的大小：镜像按 ID（独有的大小与使用它的容器数），容器按 ID，卷按名称
    let df_images: HashMap<String, (Option<u64>, Option<u32>)> = df_list("Images")
        .iter()
        .map(|m| {
            let size =
                parse_size(&m.str(&["UniqueSize"])).or_else(|| parse_size(&m.str(&["Size"])));
            let containers = m.str(&["Containers"]).trim().parse().ok();
            (bare_id(&m.str(&["ID"])), (size, containers))
        })
        .collect();
    let container_sizes: HashMap<String, u64> = df_list("Containers")
        .iter()
        .filter_map(|m| Some((bare_id(&m.str(&["ID"])), parse_size(&m.str(&["Size"]))?)))
        .collect();
    let df_volumes: HashMap<String, (Option<u64>, Option<u32>, bool)> = df_list("Volumes")
        .iter()
        .map(|m| {
            (
                m.str(&["Name"]),
                (
                    parse_size(&m.str(&["Size"])),
                    m.str(&["Links"]).trim().parse().ok(),
                    anonymous(m),
                ),
            )
        })
        .collect();
    let build_cache = (engine == Engine::Docker && sizes).then(|| {
        let mut usage = BuildCacheUsage::default();
        for m in df_list("BuildCache") {
            if m.str(&["InUse"]) == "true" {
                continue;
            }
            usage.entries += 1;
            if m.str(&["Shared"]) != "true" {
                usage.reclaimable += parse_size(&m.str(&["Size"])).unwrap_or(0);
            }
        }
        usage
    });

    let inspect = sections.get("inspect");
    let containers: Vec<PlanContainer> = inspect
        .all_lines()
        .into_iter()
        .filter_map(|line| {
            let fields: Vec<&str> = line.trim().splitn(6, '|').collect();
            let [id, image, state, volumes, networks, name] = fields[..] else {
                return None;
            };
            let id = bare_id(id);
            ID_RE.is_match(&id).then(|| PlanContainer {
                size: container_sizes.get(&id).copied(),
                name: name.trim().trim_start_matches('/').to_string(),
                state: state.trim().to_lowercase(),
                image: bare_id(image),
                volumes: unique(volumes),
                networks: unique(networks),
                id,
            })
        })
        .collect();
    let dangling: Vec<PlanImage> = sections
        .get("dangling")
        .all_lines()
        .into_iter()
        .map(bare_id)
        .filter(|id| ID_RE.is_match(id))
        .fold(Vec::new(), |mut list: Vec<PlanImage>, id| {
            if !list.iter().any(|im| im.id == id) {
                let (size, containers) = df_images.get(&id).copied().unwrap_or_default();
                list.push(PlanImage {
                    size,
                    containers,
                    id,
                });
            }
            list
        });
    let mut volumes: Vec<PlanVolume> = sections
        .get("volumes")
        .all_lines()
        .into_iter()
        .filter_map(|line| {
            let m = JsonObj::parse(line)?;
            let name = m.str(&["Name"]);
            if !VOLUME_RE.is_match(&name) {
                return None;
            }
            let (size, links, df_anonymous) = df_volumes.get(&name).copied().unwrap_or_default();
            Some(PlanVolume {
                anonymous: anonymous(&m) || df_anonymous,
                size,
                links,
                name,
            })
        })
        .collect();
    volumes.sort_by(|a, b| a.name.cmp(&b.name));
    let endpoints: HashMap<String, Vec<String>> = sections
        .get("endpoints")
        .all_lines()
        .into_iter()
        .filter_map(|line| {
            let (id, list) = line.trim().split_once('|')?;
            Some((
                bare_id(id),
                unique(list).iter().map(|c| bare_id(c)).collect(),
            ))
        })
        .collect();
    let mut networks: Vec<PlanNetwork> = sections
        .get("networks")
        .all_lines()
        .into_iter()
        .filter_map(|line| {
            let m = JsonObj::parse(line)?;
            let id = bare_id(&m.str(&["ID", "Id"]));
            let name = m.str(&["Name"]);
            if !ID_RE.is_match(&id)
                || BUILTIN_NETWORKS.contains(&name.as_str())
                || name == "ingress"
            {
                return None;
            }
            // `network ls` 的 ID 可能比 inspect 的短：按前缀对应
            let endpoints = endpoints
                .iter()
                .find(|(full, _)| full.starts_with(&id) || id.starts_with(full.as_str()))
                .map(|(_, list)| list.clone());
            Some(PlanNetwork {
                id,
                name,
                endpoints,
            })
        })
        .collect();
    networks.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(CleanupPlan {
        engine,
        sudo,
        anonymous_volumes_only: engine == Engine::Docker
            && api_at_least(sections.get("api").first(), (1, 42)),
        sizes,
        // 段标记的参数是容器的总数：每个都读到了才算读全
        containers_known: inspect
            .num_arg()
            .is_some_and(|total| usize::try_from(total).is_ok_and(|n| n == containers.len())),
        containers,
        dangling,
        volumes,
        networks,
        build_cache,
    })
}

/// 匿名卷：带 `com.docker.volume.anonymous` 标签（Docker 的 Labels 是 `k=v,…` 的文字，Podman 是对象）。
fn anonymous(m: &JsonObj) -> bool {
    const LABEL: &str = "com.docker.volume.anonymous";
    match m.labels() {
        Some(labels) => labels.contains_key(LABEL),
        None => m
            .str(&["Labels"])
            .split(',')
            .any(|label| label.split('=').next().map(str::trim) == Some(LABEL)),
    }
}

/// API 版本（`1.45`）不低于 `min`；读不到时为假。
fn api_at_least(text: &str, min: (u32, u32)) -> bool {
    let mut parts = text.trim().split('.');
    let major = parts.next().and_then(|p| p.parse::<u32>().ok());
    let minor = parts.next().and_then(|p| p.parse::<u32>().ok());
    match (major, minor) {
        (Some(major), Some(minor)) => (major, minor) >= min,
        _ => false,
    }
}
