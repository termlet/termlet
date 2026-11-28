//! Docker 页的测试。

use super::ShRunner;
use crate::hosttools::docker::{
    JsonObj, created_at, cut_tag, finish_docker, human_size, parse_container_names, parse_docker,
    version_of,
};
use crate::hosttools::scripts::{container_names_script, docker_script};
use crate::hosttools::{Engine, HealthState, SudoState, ToolsError};
use crate::ssh::ExecResult;

// Docker 27 的 --format '{{json .}}' 输出：一个 compose 项目（标签值中有逗号）、一个单独的已停止容器。
const DOCKER_PS: &str = r#"{"Command":"\"/docker-entrypoint.sh nginx -g 'daemon off;'\"","CreatedAt":"2024-10-01 08:00:00 +0000 UTC","ID":"3f4e8a1b2c9d0e1f2a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091","Image":"nginx:1.27","Labels":"com.docker.compose.config-hash=abc,com.docker.compose.project=shop,com.docker.compose.project.config_files=/srv/shop/compose.yaml,/srv/shop/compose.override.yaml,com.docker.compose.project.working_dir=/srv/shop,com.docker.compose.service=web,maintainer=NGINX Docker Maintainers <docker-maint@nginx.com>","LocalVolumes":"1","Mounts":"shop_data","Names":"shop-web-1","Networks":"shop_default","Ports":"0.0.0.0:8080->80/tcp, [::]:8080->80/tcp","RunningFor":"2 weeks ago","Size":"0B","State":"running","Status":"Up 3 days"}
{"Command":"\"docker-entrypoint.s…\"","CreatedAt":"2024-10-01 08:00:01 +0000 UTC","ID":"7a6b5c4d3e2f10011223344556677889900aabbccddeeff00112233445566778","Image":"postgres:16","Labels":"com.docker.compose.project=shop,com.docker.compose.service=db","LocalVolumes":"1","Mounts":"shop_pg","Names":"shop-db-1","Networks":"shop_default","Ports":"5432/tcp","RunningFor":"2 weeks ago","Size":"0B","State":"running","Status":"Up 3 days (healthy)"}
{"Command":"\"bash\"","CreatedAt":"2024-09-01 08:00:00 +0000 UTC","ID":"0011223344556677889900aabbccddeeff00112233445566778899aabbccddee","Image":"ubuntu:24.04","Labels":"evil=x,com.docker.compose.project=shop,org.opencontainers.image.version=24.04","LocalVolumes":"0","Mounts":"","Names":"scratchpad","Networks":"bridge","Ports":"","RunningFor":"6 weeks ago","Size":"0B","State":"exited","Status":"Exited (0) 5 weeks ago"}"#;
const DOCKER_IMAGES: &str = r#"{"Containers":"N/A","CreatedAt":"2024-08-14 22:18:35 +0000 UTC","CreatedSince":"8 weeks ago","Digest":"<none>","ID":"sha256:5ef79149e0ec84a7a9f9284c3f91aa3c20608f8391f5445eabe92ef07dbda03c","Repository":"nginx","SharedSize":"N/A","Size":"188MB","Tag":"1.27","UniqueSize":"N/A","VirtualSize":"187.7MB"}
{"Containers":"N/A","CreatedAt":"2024-08-10 10:00:00 +0000 UTC","CreatedSince":"2 months ago","Digest":"<none>","ID":"sha256:aaaa149e0ec84a7a9f9284c3f91aa3c20608f8391f5445eabe92ef07dbda0000","Repository":"<none>","SharedSize":"N/A","Size":"1.2GB","Tag":"<none>","UniqueSize":"N/A","VirtualSize":"1.2GB"}"#;
const DOCKER_VOLUMES: &str = r#"{"Availability":"N/A","Driver":"local","Group":"N/A","Labels":"com.docker.compose.project=shop,com.docker.compose.volume=data","Links":"N/A","Mountpoint":"/var/lib/docker/volumes/shop_data/_data","Name":"shop_data","Scope":"local","Size":"N/A","Status":"N/A"}
{"Availability":"N/A","Driver":"local","Group":"N/A","Labels":"","Links":"N/A","Mountpoint":"/var/lib/docker/volumes/old/_data","Name":"old","Scope":"local","Size":"N/A","Status":"N/A"}"#;
const DOCKER_NETWORKS: &str = r#"{"CreatedAt":"2024-10-01 07:59:58.123456789 +0000 UTC","Driver":"bridge","ID":"9a8b7c6d5e4f30211203f4e5d6c7b8a99a8b7c6d5e4f30211203f4e5d6c7b8a9","IPv6":"false","Internal":"false","Labels":"com.docker.compose.network=default,com.docker.compose.project=shop","Name":"shop_default","Scope":"local"}
{"CreatedAt":"2024-09-01 07:59:58 +0000 UTC","Driver":"bridge","ID":"1111111111111111111111111111111111111111111111111111111111111111","IPv6":"false","Internal":"false","Labels":"","Name":"bridge","Scope":"local"}
{"CreatedAt":"2024-09-01 07:59:58 +0000 UTC","Driver":"null","ID":"2222222222222222222222222222222222222222222222222222222222222222","IPv6":"false","Internal":"false","Labels":"","Name":"none","Scope":"local"}
{"CreatedAt":"2024-09-02 07:59:58 +0000 UTC","Driver":"bridge","ID":"3333333333333333333333333333333333333333333333333333333333333333","IPv6":"false","Internal":"false","Labels":"","Name":"unused_net","Scope":"local"}"#;
// container inspect：最后一项是标签与运行状态的 JSON。scratchpad 的镜像 LABEL 值中写了逗号，docker ps 的 Labels
// 字符串里看起来像是属于 compose 项目 shop（见上），JSON 中只是一个标签的值；它的状态中的错误信息带着 `|`
const DOCKER_INSPECT: &str = r#"3f4e8a1b2c9d0e1f2a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091|sha256:5ef79149e0ec84a7a9f9284c3f91aa3c20608f8391f5445eabe92ef07dbda03c|shop_data,|shop_default,|{"labels":{"com.docker.compose.config-hash":"abc","com.docker.compose.project":"shop","com.docker.compose.project.config_files":"/srv/shop/compose.yaml,/srv/shop/compose.override.yaml","com.docker.compose.project.working_dir":"/srv/shop","com.docker.compose.service":"web","maintainer":"NGINX Docker Maintainers <docker-maint@nginx.com>"},"state":{"Status":"running","Running":true,"ExitCode":0,"StartedAt":"2026-11-04T09:49:04.944387787Z","FinishedAt":"0001-01-01T00:00:00Z"}}
7a6b5c4d3e2f10011223344556677889900aabbccddeeff00112233445566778|sha256:bbbb149e0ec84a7a9f9284c3f91aa3c20608f8391f5445eabe92ef07dbda0000|shop_pg,|shop_default,|{"labels":{"com.docker.compose.project":"shop","com.docker.compose.service":"db"},"state":{"Status":"running","Running":true,"Health":{"Status":"healthy","FailingStreak":0}}}
0011223344556677889900aabbccddeeff00112233445566778899aabbccddee|sha256:cccc149e0ec84a7a9f9284c3f91aa3c20608f8391f5445eabe92ef07dbda0000||bridge,|{"labels":{"evil":"x,com.docker.compose.project=shop","org.opencontainers.image.version":"24.04"},"state":{"Status":"exited","Running":false,"ExitCode":0,"Error":"a|b","FinishedAt":"2026-11-04T10:49:04Z"}}"#;

// Podman 4.9 的 --format '{{json .}}' 输出：字段名与类型都与 docker 不同（Id、Names 数组、Labels 对象、Ports 数组）。
const PODMAN_PS: &str = r#"{"AutoRemove":false,"Command":["nginx","-g","daemon off;"],"CreatedAt":"5 minutes ago","Exited":false,"ExitCode":0,"Id":"a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90","Image":"docker.io/library/nginx:latest","ImageID":"39286ab8a5e14aeaf5fdd6e2fac76e0c8d31a0c07224f0ee5e6be502f12e93f3","IsInfra":false,"Labels":{"io.podman.compose.project":"blog","com.docker.compose.project":"blog","com.docker.compose.service":"web"},"Mounts":["/usr/share/nginx/html"],"Names":["blog_web_1"],"Namespaces":{},"Networks":["blog_default"],"Pid":4242,"Pod":"","PodName":"","Ports":[{"host_ip":"","container_port":80,"host_port":8081,"range":1,"protocol":"tcp"}],"Restarts":0,"Size":null,"StartedAt":1728000000,"State":"running","Status":"Up 5 minutes","Created":1727999700}"#;
const PODMAN_IMAGES: &str = r#"{"Id":"39286ab8a5e14aeaf5fdd6e2fac76e0c8d31a0c07224f0ee5e6be502f12e93f3","ParentId":"","RepoTags":null,"RepoDigests":["docker.io/library/nginx@sha256:aaaa"],"Size":191754000,"SharedSize":0,"VirtualSize":191754000,"Labels":{"maintainer":"x"},"Containers":1,"Names":["docker.io/library/nginx:latest"],"Digest":"sha256:aaaa","History":["docker.io/library/nginx:latest"],"Created":1727000000,"CreatedAt":"2024-09-22T10:13:20Z"}
{"Id":"4444444444444444444444444444444444444444444444444444444444444444","RepoTags":null,"Size":5000,"Containers":0,"Names":null,"Created":1727000000}"#;
const PODMAN_VOLUMES: &str = r#"{"Name":"blog_data","Driver":"local","Mountpoint":"/home/u/.local/share/containers/storage/volumes/blog_data/_data","CreatedAt":"2024-10-04T10:00:00Z","Labels":{"io.podman.compose.project":"blog"},"Scope":"local","Options":{},"MountCount":0}"#;
const PODMAN_NETWORKS: &str = r#"{"name":"podman","id":"2f259bab93aaaaa2542ba43ef33eb990d0999ee1b9924b557b7be53c0b7a1bb9","driver":"bridge","network_interface":"podman0","created":"2024-10-04T10:00:00Z","subnets":[{"subnet":"10.88.0.0/16","gateway":"10.88.0.1"}],"ipv6_enabled":false,"internal":false,"dns_enabled":false}
{"name":"blog_default","id":"5555555555555555555555555555555555555555555555555555555555555555","driver":"bridge","network_interface":"podman1","created":"2024-10-04T10:00:00Z","ipv6_enabled":false,"internal":false,"dns_enabled":true}"#;
const PODMAN_INSPECT: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90|39286ab8a5e14aeaf5fdd6e2fac76e0c8d31a0c07224f0ee5e6be502f12e93f3||blog_default,";

/// 假的 docker 或 podman：`$NEED_SUDO` 为 1 时只有经 sudo（`$SUDO_RUN` 为 1）才能访问守护进程，`$DOWN` 为 1 时守护进程
/// 没有运行。
fn fake_engine(
    runner: &ShRunner,
    name: &str,
    ps: &str,
    images: &str,
    volumes: &str,
    networks: &str,
    inspect: &str,
) {
    let body = [
        r#"if [ "$DOWN" = 1 ]; then echo "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?" >&2; exit 1; fi
if [ "$NEED_SUDO" = 1 ] && [ "$SUDO_RUN" != 1 ]; then echo "permission denied while trying to connect to the Docker daemon socket at unix:///var/run/docker.sock: Get \"http://%2Fvar%2Frun%2Fdocker.sock/v1.47/containers/json\": dial unix /var/run/docker.sock: connect: permission denied" >&2; exit 1; fi
case "$1 $2" in
"ps -a") cat <<'EOF'
"#,
        ps,
        r#"
EOF
;;
"ps -aq") cat <<'EOF' | sed 's/.*"I[Dd]":"\([0-9a-f]*\)".*/\1/'
"#,
        ps,
        r#"
EOF
;;
"images --no-trunc") cat <<'EOF'
"#,
        images,
        r#"
EOF
;;
"volume ls") cat <<'EOF'
"#,
        volumes,
        r#"
EOF
;;
"network ls") cat <<'EOF'
"#,
        networks,
        r#"
EOF
;;
"container inspect") cat <<'EOF'
"#,
        inspect,
        r#"
EOF
;;
"version --format") case "$3" in *Server*) echo 29.3.0;; esac;;
"compose version") echo v2.39.1;;
esac"#,
    ]
    .concat();
    runner.fake(name, &body);
}

fn fake_docker(runner: &ShRunner) {
    fake_engine(
        runner,
        "docker",
        DOCKER_PS,
        DOCKER_IMAGES,
        DOCKER_VOLUMES,
        DOCKER_NETWORKS,
        DOCKER_INSPECT,
    );
}

fn is_root(result: Result<impl std::fmt::Debug, ToolsError>, want: SudoState) -> bool {
    matches!(result, Err(ToolsError::NeedRoot { sudo, .. }) if sudo == want)
}

/// 读取；不在 docker 组时经免密码 sudo；sudo 要密码、不允许、没有 sudo；守护进程
/// 没有运行；没有 docker 而有 podman；都没有。
#[test]
fn docker_script_on_fake_engine() {
    let Some(runner) = ShRunner::isolated() else {
        return;
    };
    runner.fake("id", "echo 1000");
    fake_docker(&runner);
    let script = docker_script();

    let out = runner.run(&script, &[], &[]);
    let info = parse_docker(&out).unwrap_or_else(|e| panic!("{e}\n{out}"));
    assert_eq!(info.engine, Engine::Docker);
    assert!(!info.sudo);
    assert_eq!(
        (
            info.containers.len(),
            info.images.len(),
            info.volumes.len(),
            info.networks.len()
        ),
        (3, 2, 2, 4),
        "{info:?}"
    );
    // 版本与 Compose 的版本（去掉开头的 v）在同一次读取中取得
    assert_eq!(
        (info.version.as_deref(), info.compose.as_deref()),
        (Some("29.3.0"), Some("2.39.1"))
    );

    // 不在 docker 组：免密码 sudo 可用时改用 sudo -n
    runner.fake("sudo", r#"[ "$1" = -n ] && shift; SUDO_RUN=1 exec "$@""#);
    let out = runner.run_env(&script, &[("NEED_SUDO", "1")]);
    let info = parse_docker(&out).unwrap_or_else(|e| panic!("应经 sudo 读取：{e}\n{out}"));
    assert!(info.sudo);
    assert_eq!(info.containers.len(), 3);
    // sudo 要密码
    runner.fake("sudo", r#"echo "sudo: a password is required" >&2; exit 1"#);
    let out = runner.run_env(&script, &[("NEED_SUDO", "1")]);
    assert!(is_root(parse_docker(&out), SudoState::Password), "{out}");
    // 不允许用 sudo
    runner.fake(
        "sudo",
        r#"echo "Sorry, user termlet is not allowed to execute '/usr/bin/docker ps' as root on host." >&2; echo "termlet is not in the sudoers file." >&2; exit 1"#,
    );
    let out = runner.run_env(&script, &[("NEED_SUDO", "1")]);
    assert!(is_root(parse_docker(&out), SudoState::Denied), "{out}");
    // 没有 sudo
    runner.remove("sudo");
    let out = runner.run_env(&script, &[("NEED_SUDO", "1")]);
    assert!(
        is_root(parse_docker(&out), SudoState::NotInstalled),
        "{out}"
    );
    // 守护进程没有运行
    let out = runner.run_env(&script, &[("DOWN", "1")]);
    assert_eq!(parse_docker(&out), Err(ToolsError::DockerDown), "{out}");
    // 没有 docker 而有 podman
    runner.remove("docker");
    fake_engine(
        &runner,
        "podman",
        PODMAN_PS,
        PODMAN_IMAGES,
        PODMAN_VOLUMES,
        PODMAN_NETWORKS,
        PODMAN_INSPECT,
    );
    let info = parse_docker(&runner.run(&script, &[], &[])).unwrap();
    assert_eq!(info.engine, Engine::Podman);
    assert_eq!(info.containers.len(), 1);
    // 都没有
    runner.remove("podman");
    runner.fake("uname", "echo Linux");
    assert_eq!(
        parse_docker(&runner.run(&script, &[], &[])),
        Err(ToolsError::NoDocker)
    );
}

fn docker_output(inspect: &str) -> String {
    format!(
        "@@engine docker\n@@containers\n{DOCKER_PS}\n@@images\n{DOCKER_IMAGES}\n@@volumes\n{DOCKER_VOLUMES}\n\
         @@networks\n{DOCKER_NETWORKS}\n@@inspect\n{inspect}"
    )
}

/// 按 compose 项目、名称排序（没有项目的在前）；标签值中的逗号伪造不了 compose 项目；
/// 使用中的镜像、卷、网络与内置网络。
#[test]
fn parse_docker_output() {
    let info = parse_docker(&docker_output(&format!("{DOCKER_INSPECT}\n"))).unwrap();
    let c = &info.containers;
    assert_eq!(c[0].name, "scratchpad", "单独的容器");
    assert_eq!(c[0].project, None);
    assert_eq!(c[0].state, "exited");
    let web = &c[2];
    assert_eq!(web.name, "shop-web-1");
    assert_eq!(web.project.as_deref(), Some("shop"));
    assert_eq!(web.service.as_deref(), Some("web"));
    assert_eq!(web.working_dir.as_deref(), Some("/srv/shop"));
    assert_eq!(web.ports, "0.0.0.0:8080->80/tcp, [::]:8080->80/tcp");
    assert_eq!(web.state, "running");
    // 运行状态取自 inspect：列表写本地化的状态与时长
    let run = web.run.as_ref().expect("运行状态");
    assert_eq!(
        (run.status.as_str(), run.started_at, run.exit_code),
        ("running", Some(1_793_785_744_944), None)
    );
    assert_eq!(c[1].health(), Some(HealthState::Healthy));
    let scratch = c[0].run.as_ref().expect("状态中的 | 不影响解析");
    assert_eq!(
        (scratch.exit_code, scratch.finished_at),
        (Some(0), Some(1_793_789_344_000))
    );

    let image = |key: &str| {
        info.images
            .iter()
            .find(|im| format!("{}:{}", im.repository, im.tag) == key)
            .unwrap()
    };
    let nginx = image("nginx:1.27");
    assert!(
        nginx.in_use && nginx.containers == 1 && nginx.size == "188MB",
        "{nginx:?}"
    );
    assert_eq!(nginx.created_at, Some(1_723_673_915_000), "按 UTC 换算");
    assert_eq!(
        (info.version.as_deref(), info.compose.as_deref()),
        (None, None),
        "没有版本的段"
    );
    assert!(!image("<none>:<none>").in_use, "悬空镜像未使用");
    let volume = |name: &str| info.volumes.iter().find(|v| v.name == name).unwrap();
    assert!(volume("shop_data").in_use && !volume("old").in_use);
    let network = |name: &str| info.networks.iter().find(|n| n.name == name).unwrap();
    assert!(network("shop_default").in_use && network("shop_default").containers == 2);
    assert!(network("bridge").builtin && network("bridge").in_use);
    assert!(network("none").builtin);
    assert!(!network("unused_net").in_use);
}

/// 字段名大小写、Names 数组、Labels 对象、Ports 数组、字节数的大小、Unix 秒的创建时间。
#[test]
fn parse_podman_output() {
    let out = format!(
        "@@engine podman\n@@containers\n{PODMAN_PS}\n@@images\n{PODMAN_IMAGES}\n@@volumes\n{PODMAN_VOLUMES}\n\
         @@networks\n{PODMAN_NETWORKS}\n@@inspect\n{PODMAN_INSPECT}\n"
    );
    let info = parse_docker(&out).unwrap();
    let c = &info.containers[0];
    assert_eq!(
        c.id,
        "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90"
    );
    assert_eq!(c.name, "blog_web_1");
    assert_eq!(c.project.as_deref(), Some("blog"));
    assert_eq!(c.ports, "0.0.0.0:8081->80/tcp");
    assert_eq!(c.created, "5 minutes ago");
    let nginx = &info.images[1];
    assert_eq!(
        (
            nginx.repository.as_str(),
            nginx.tag.as_str(),
            nginx.in_use,
            nginx.size.as_str()
        ),
        ("docker.io/library/nginx", "latest", true, "192MB")
    );
    let dangling = &info.images[0];
    assert_eq!(
        (
            dangling.repository.as_str(),
            dangling.in_use,
            dangling.size.as_str()
        ),
        ("<none>", false, "5.00kB")
    );
    assert_eq!(dangling.created, "2024-09-22T10:13:20Z");
    assert_eq!(dangling.created_at, Some(1_727_000_000_000), "RFC 3339");
    let volume = &info.volumes[0];
    assert!(volume.name == "blog_data" && !volume.in_use);
    let network = &info.networks[0];
    assert_eq!(network.name, "blog_default", "小写的字段名");
    assert!(network.in_use && !network.builtin && network.driver == "bridge");
    assert!(info.networks[1].name == "podman" && info.networks[1].builtin);
}

/// 读不到 inspect 时，镜像按名称推断，卷与网络当作使用中（不确定时不让删除）；
/// docker ps 的 Labels 字符串可以伪造，读不到 inspect 时不分组。
#[test]
fn usage_without_inspect() {
    let info = parse_docker(&docker_output("")).unwrap();
    assert!(
        info.images
            .iter()
            .any(|im| im.repository == "nginx" && im.in_use),
        "按名称推断 nginx:1.27 使用中"
    );
    assert!(
        info.volumes.iter().all(|v| v.in_use),
        "不确定时卷当作使用中"
    );
    assert!(
        info.containers.iter().all(|c| c.project.is_none()),
        "读不到 inspect 时不按 ps 的标签字符串分组"
    );
    assert!(
        info.containers.iter().all(|c| c.run.is_none()),
        "读不到 inspect 时没有运行状态，界面写 docker ps 的原文"
    );
}

/// 其他错误带上原因；已是 root 仍没有权限（例如 SELinux）；没有 docker。
#[test]
fn other_errors() {
    match parse_docker("@@engine docker\n@@error 125\nunknown flag: --no-trunc\n") {
        Err(ToolsError::DockerFailed(message)) => assert!(message.contains("unknown flag")),
        other => panic!("其他错误带上原因：{other:?}"),
    }
    assert!(is_root(
        parse_docker("@@engine docker\n@@error 1\npermission denied while trying to connect\n"),
        SudoState::Denied
    ));
    let result = ExecResult {
        exit_code: Some(0),
        stdout: b"@@nodocker\nLinux\n".to_vec(),
        ..Default::default()
    };
    assert_eq!(finish_docker(&result), Err(ToolsError::NoDocker));
    // 较新的 docker 命令（WSL 中没有启动 Docker Desktop 时）
    assert_eq!(
        parse_docker(
            "@@engine docker\n@@error 1\n failed to connect to the docker API at unix:///var/run/docker.sock; check if the path is correct and if the daemon is running: dial unix /var/run/docker.sock: connect: no such file or directory\n"
        ),
        Err(ToolsError::DockerDown)
    );
    // WSL 中 Docker Desktop 的集成程序（Docker Desktop 没有启动时）
    assert_eq!(
        parse_docker(
            "@@engine docker\n@@error 1\nThe command 'docker' could not be found in this WSL 2 distro.\nWe recommend to activate the WSL integration in Docker Desktop settings.\n"
        ),
        Err(ToolsError::DockerDesktopDown)
    );
    // Docker Desktop 启动过又退出、发行版一直开着时（2026-11-28 在本机复现的三种写法：经 sh、经 bash、经 Windows
    // 一侧 Docker Desktop 的 docker）
    for output in [
        "sh: 1: docker: Input/output error",
        "bash: line 1: /usr/bin/docker: Input/output error",
        "/mnt/c/Program Files/Docker/Docker/resources/bin/docker: 13: /usr/bin/docker: Input/output error",
    ] {
        assert_eq!(
            parse_docker(&format!("@@engine docker\n@@error 126\n{output}\n")),
            Err(ToolsError::DockerDesktopDown),
            "{output}"
        );
    }
    // Docker Desktop 在运行、但没有为这个发行版打开 WSL 集成：连不上 docker.sock，脚本标出 docker 是 Docker Desktop 的
    let refused = "failed to connect to the docker API at unix:///var/run/docker.sock; check if the path is correct and if the daemon is running: dial unix /var/run/docker.sock: connect: no such file or directory";
    assert_eq!(
        parse_docker(&format!(
            "@@engine docker\n@@desktop\n@@error 1\n{refused}\n"
        )),
        Err(ToolsError::DockerDesktopDown)
    );
    // Docker Desktop 的 docker 出了别的错：照常带上原因
    assert!(matches!(
        parse_docker("@@engine docker\n@@desktop\n@@error 125\nunknown flag: --no-trunc\n"),
        Err(ToolsError::DockerFailed(_))
    ));
}

/// podman 的标签是对象，可以直接用；docker 的是以逗号连接的字符串，不用；仓库地址中的端口不是标签；
/// 大小的写法。
#[test]
fn labels_tags_and_sizes() {
    let m = JsonObj::parse(r#"{"Labels":{"a":"1","b":"x,c=3"}}"#).unwrap();
    let labels = m.labels().unwrap();
    assert_eq!(labels.len(), 2);
    assert_eq!(labels["a"], "1");
    assert_eq!(labels["b"], "x,c=3");
    let m = JsonObj::parse(r#"{"Labels":"a=1,com.docker.compose.project=prod"}"#).unwrap();
    assert_eq!(m.labels(), None, "字符串形式的标签不用");
    assert_eq!(
        cut_tag("localhost:5000/app"),
        Some(("localhost:5000/app", "latest"))
    );
    assert_eq!(
        cut_tag("localhost:5000/app:v2"),
        Some(("localhost:5000/app", "v2"))
    );
    assert_eq!(human_size(999), "999B");
    assert_eq!(human_size(191754000), "192MB");
    assert_eq!(human_size(1234567890), "1.23GB");
}

/// 进程页读容器名：只列 ID 与名称（名称经 q 输出，伪造不了段）；不在 docker 组时经免密码 sudo（与 Docker 页相同），
/// sudo 要密码、没有 docker 时与 Docker 页的错误相同。
#[test]
fn container_names_on_fake_engine() {
    let Some(runner) = ShRunner::isolated() else {
        return;
    };
    runner.fake("id", "echo 1000");
    runner.fake(
        "docker",
        r#"if [ "$NEED_SUDO" = 1 ] && [ "$SUDO_RUN" != 1 ]; then echo "permission denied while trying to connect to the Docker daemon socket" >&2; exit 1; fi
[ "$1 $2 $3 $4 $5" = "ps -a --no-trunc --format {{.ID}} {{.Names}}" ] || { echo "unexpected: $*" >&2; exit 2; }
echo "3f4e8a1b2c9d0e1f2a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091 shop-web-1"
printf '%s\n' "7a6b5c4d3e2f10011223344556677889900aabbccddeeff00112233445566778 evil" "@@nodocker"
echo "not-an-id x""#,
    );
    let script = container_names_script();
    let names = parse_container_names(&runner.run(&script, &[], &[])).unwrap();
    assert_eq!((names.engine, names.sudo), (Engine::Docker, false));
    assert_eq!(names.names.len(), 2, "{names:?}");
    assert_eq!(
        names.names["3f4e8a1b2c9d0e1f2a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091"],
        "shop-web-1"
    );
    // 不在 docker 组：经免密码 sudo
    runner.fake("sudo", r#"[ "$1" = -n ] && shift; SUDO_RUN=1 exec "$@""#);
    let out = runner.run_env(&script, &[("NEED_SUDO", "1")]);
    let names = parse_container_names(&out).unwrap_or_else(|e| panic!("{e}\n{out}"));
    assert!(names.sudo && names.names.len() == 2);
    runner.fake("sudo", r#"echo "sudo: a password is required" >&2; exit 1"#);
    let out = runner.run_env(&script, &[("NEED_SUDO", "1")]);
    assert!(is_root(parse_container_names(&out), SudoState::Password));
    runner.remove("docker");
    runner.fake("uname", "echo Linux");
    assert_eq!(
        parse_container_names(&runner.run(&script, &[], &[])),
        Err(ToolsError::NoDocker)
    );
}

/// 列出容器时 inspect 的模板只取卷的名称：docker 的绑定挂载没有 Name，写成 `{{.Name}}` 时这个容器的一行出错
/// （map has no entry for key "Name"），它的 compose 项目、使用中的镜像都认不出。
#[test]
fn inspect_template_skips_bind_mounts() {
    let script = docker_script();
    assert!(
        script.contains(r#"{{range .Mounts}}{{if eq .Type "volume"}}{{.Name}},{{end}}{{end}}"#)
    );
    // 运行状态整个取出（podman 较早的版本没有 .State.Health，写成它时整行出错）
    assert!(script.contains(r#"{"labels":{{json .Config.Labels}},"state":{{json .State}}}"#));
    assert!(!script.contains(".State.Health"));
}

/// 镜像的创建时间：docker 的写法（带时区偏移）、RFC 3339；换算不了的为 `None`。版本号去掉开头的 v，不像版本号的不要。
#[test]
fn created_times_and_versions() {
    assert_eq!(
        created_at("2024-08-14 22:18:35 +0000 UTC"),
        Some(1_723_673_915_000)
    );
    assert_eq!(
        created_at("2024-08-15 06:18:35 +0800 CST"),
        Some(1_723_673_915_000)
    );
    assert_eq!(created_at("2024-08-14T22:18:35Z"), Some(1_723_673_915_000));
    assert_eq!(created_at("8 weeks ago"), None);
    assert_eq!(created_at(""), None);
    assert_eq!(version_of("v2.39.1"), Some("2.39.1".into()));
    assert_eq!(version_of(" 29.3.0"), Some("29.3.0".into()));
    assert_eq!(version_of("5.4.0-dev"), Some("5.4.0-dev".into()));
    assert_eq!(
        version_of("26.1.5+dfsg1"),
        Some("26.1.5".into()),
        "Debian 的打包后缀"
    );
    assert_eq!(
        version_of("2.26.1-4"),
        Some("2.26.1".into()),
        "Debian 的修订号"
    );
    assert_eq!(version_of("Docker version 29"), None);
    assert_eq!(version_of(""), None);
}
