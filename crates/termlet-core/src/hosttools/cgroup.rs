//! 进程的归属：由 `/proc/<pid>/cgroup` 中的路径认出进程所属的 systemd 服务与容器（进程页的标签、隐藏容器内的进程、
//! 在进程、服务、容器之间跳转）。
//!
//! 读取脚本对每个进程只取一行（见 `scripts::PROC_BODY` 的 `@@cgroup`）：cgroup v1 的各个控制器都带着容器的路径，
//! 先取到的是容器的那一行；不在容器中时是 systemd 的那一行（v1 的 `name=systemd`，v2 与混合模式的 `0::`）。
//! 路径的写法：
//!
//! - systemd 服务：`/system.slice/nginx.service`、`/system.slice/system-getty.slice/getty@tty1.service`（各级 slice
//!   之后第一个不是 slice 的就是它；登录会话、容器的 `*.scope` 不是服务）。
//! - Docker：`/docker/<ID>`（v1 的 cgroupfs 驱动）、`/system.slice/docker-<ID>.scope`（systemd 驱动）；Podman：
//!   `libpod-<ID>.scope`、`/libpod_parent/libpod-<ID>`，quadlet 的 `/system.slice/x.service/libpod-payload-<ID>`
//!   （同时属于服务 x）；containerd、CRI-O（Kubernetes）：`cri-containerd-<ID>.scope`、`crio-<ID>.scope`、
//!   `/kubepods/…/<ID>`，nerdctl 的 `nerdctl-<ID>.scope`。ID 是 64 位十六进制；conmon（`libpod-conmon-<ID>`、
//!   `crio-conmon-<ID>`）在容器之外，不算。
//!
//! 内核不允许 cgroup 的名称中有换行，一行就是一个进程；脚本仍经 `q` 输出（伪造不了段标记）。

use std::collections::HashMap;

use super::sections::Section;

/// 进程所属的服务与容器（都可能没有；Podman 的 quadlet 服务中的容器两者都有）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Owner {
    /// systemd 服务的完整名称（`nginx.service`）
    pub service: Option<String>,
    /// 容器 ID（64 位十六进制）
    pub container: Option<String>,
}

/// 容器 ID 前面的写法（长的在前：`libpod-payload-` 先于 `libpod-`）。
const ID_PREFIXES: [&str; 6] = [
    "libpod-payload-",
    "cri-containerd-",
    "libpod-",
    "docker-",
    "crio-",
    "nerdctl-",
];

/// 由 cgroup 路径认出所属的服务与容器。
pub(crate) fn owner(path: &str) -> Owner {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let service = parts
        .iter()
        .find(|p| !p.ends_with(".slice"))
        .filter(|p| is_service(p))
        .map(|p| p.to_string());
    // 容器中再套容器时取外层的（主机上的 Docker 列出的是它）
    let container = parts
        .iter()
        .find_map(|p| container_id(p))
        .map(str::to_string);
    Owner { service, container }
}

/// systemd 服务的单元名：`名称.service`，名称只含单元名允许的字符（含 `\x2d` 这样的转义）。
fn is_service(name: &str) -> bool {
    name.len() <= 256
        && name.strip_suffix(".service").is_some_and(|stem| {
            !stem.is_empty()
                && stem
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b":_.@-\\".contains(&b))
        })
}

/// 路径中的一节是容器时取出它的 ID：`docker-<ID>.scope` 等，或者就是 ID（`/docker/<ID>`、`/kubepods/…/<ID>`）。
/// conmon 的 `libpod-conmon-<ID>` 去掉前缀后不是 ID，不算。
fn container_id(part: &str) -> Option<&str> {
    let name = part.strip_suffix(".scope").unwrap_or(part);
    let id = ID_PREFIXES
        .iter()
        .find_map(|prefix| name.strip_prefix(prefix))
        .unwrap_or(name);
    is_id(id).then_some(id)
}

fn is_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `@@cgroup` 段：同一个路径只写一次（`=编号 路径`），各进程一行“PID 编号”。同一个 PID 出现不止一次时（不该出现）
/// 都不认；不属于服务、不在容器中的不列出。
pub(crate) fn owners(section: Option<&Section>) -> HashMap<u32, Owner> {
    let lines = section.map(|s| s.lines.as_slice()).unwrap_or_default();
    let mut paths: HashMap<&str, Owner> = HashMap::new();
    let mut found: HashMap<u32, Option<Owner>> = HashMap::new();
    for line in lines {
        let Some((head, rest)) = line.trim_start().split_once(' ') else {
            continue;
        };
        if let Some(key) = head.strip_prefix('=') {
            paths.insert(key, owner(rest.trim_end()));
            continue;
        }
        let (Ok(pid), Some(owner)) = (head.parse::<u32>(), paths.get(rest.trim())) else {
            continue;
        };
        found
            .entry(pid)
            .and_modify(|seen| *seen = None)
            .or_insert(Some(owner.clone()));
    }
    found
        .into_iter()
        .filter_map(|(pid, owner)| Some((pid, owner?)))
        .filter(|(_, owner)| owner.service.is_some() || owner.container.is_some())
        .collect()
}
