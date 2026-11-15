//! 容器的资源占用与详情、列表中的退出码与健康状态的测试。

use super::ShRunner;
use crate::hosttools::container::{
    inspect_command, parse_inspect, parse_size, parse_stats, stats_command,
};
use crate::hosttools::scripts::read_script;
use crate::hosttools::{
    Container, Engine, HealthState, MountInfo, NetworkInfo, PortBinding, RunState, ToolsError,
};
use crate::ssh::ExecResult;

const CID: &str = "3f4e8a1b2c9d0e1f2a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091";

fn result(stdout: &str) -> ExecResult {
    ExecResult {
        exit_code: Some(0),
        stdout: stdout.as_bytes().to_vec(),
        ..Default::default()
    }
}

/// 读取的脚本的输出：命令的输出、一个换行与退出码。
fn read(output: &str, code: u32) -> ExecResult {
    result(&format!("{output}\n@@rc {code}\n"))
}

// docker 27 的 stats --no-stream --no-trunc --format '{{json .}}'
const DOCKER_STATS: &str = r#"{"BlockIO":"0B / 0B","CPUPerc":"12.34%","Container":"3f4e8a1b2c9d0e1f2a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091","ID":"3f4e8a1b2c9d0e1f2a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091","MemPerc":"0.27%","MemUsage":"21.2MiB / 7.66GiB","Name":"shop-web-1","NetIO":"1.2kB / 0B","PIDs":"5"}
{"BlockIO":"--","CPUPerc":"--","Container":"x","ID":"7a6b5c4d3e2f10011223344556677889900aabbccddeeff00112233445566778","MemPerc":"--","MemUsage":"-- / --","Name":"shop-db-1","NetIO":"--","PIDs":"--"}"#;
// podman 4 的写法（字段名小写，ID 可能只有 12 位）与较早的数字写法
const PODMAN_STATS: &str = r#"{"id":"a1b2c3d4e5f6","name":"blog_web_1","cpu_percent":"150.5%","mem_usage":"2.5MB / 8.2GB","mem_percent":"0.03%"}
{"ContainerID":"b1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90","CPU":0.5,"MemUsage":1048576,"MemLimit":4194304}"#;

#[test]
fn stats_of_docker_and_podman() {
    let stats = parse_stats(&read(DOCKER_STATS, 0)).unwrap();
    assert_eq!(stats.len(), 2);
    let web = &stats[0];
    assert!(web.is_for(CID));
    assert_eq!(web.cpu, Some(12.34));
    assert_eq!(web.mem_used, Some(22_229_811));
    assert_eq!(web.mem_limit, Some(8_224_862_372));
    assert_eq!(web.mem_percent, Some(0.27));
    let db = &stats[1];
    assert_eq!((db.cpu, db.mem_used, db.mem_percent), (None, None, None));

    let stats = parse_stats(&read(PODMAN_STATS, 0)).unwrap();
    assert!(stats[0].is_for("a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90"));
    assert!(!stats[0].is_for(CID));
    assert_eq!(stats[0].cpu, Some(150.5), "多核时超过 100");
    assert_eq!(stats[0].mem_used, Some(2_500_000));
    assert_eq!(stats[1].cpu, Some(0.5));
    assert_eq!(stats[1].mem_used, Some(1_048_576));
    assert_eq!(stats[1].mem_percent, Some(25.));

    assert!(matches!(
        parse_stats(&read(
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock.",
            1
        )),
        Err(ToolsError::DockerDown)
    ));
    assert!(matches!(
        parse_stats(&read("Error: stats is not supported in rootless mode without cgroups v2", 125)),
        Err(ToolsError::DockerFailed(message)) if message.contains("rootless")
    ));
    assert!(matches!(
        parse_stats(&result("garbage")),
        Err(ToolsError::Unreadable { .. })
    ));
}

#[test]
fn sizes_in_both_units() {
    assert_eq!(parse_size("0B"), Some(0));
    assert_eq!(parse_size("1.5kB"), Some(1500));
    assert_eq!(parse_size("2KiB"), Some(2048));
    assert_eq!(parse_size(" 7.66GiB "), Some(8_224_862_372));
    assert_eq!(parse_size("512"), Some(512));
    assert_eq!(parse_size("--"), None);
    assert_eq!(parse_size("3 parsecs"), None);
}

// docker 27 的 container inspect 按模板拼出的 JSON：不健康（连续失败 3 次）、两个端口（一个发布到 IPv4 与 IPv6，一个只在
// 容器内）、绑定挂载与卷、一个网络
const DOCKER_INSPECT: &str = r#"{"id":"3f4e8a1b2c9d0e1f2a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091","image":"nginx:alpine","created":"2026-11-04T01:00:00.123456789Z","state":{"Status":"running","Running":true,"Paused":false,"Restarting":false,"OOMKilled":false,"Dead":false,"Pid":42,"ExitCode":0,"Error":"","StartedAt":"2026-11-04T01:47:12.5Z","FinishedAt":"0001-01-01T00:00:00Z","Health":{"Status":"unhealthy","FailingStreak":3,"Log":[{"Start":"2026-11-04T01:50:00Z","End":"2026-11-04T01:50:01Z","ExitCode":1,"Output":"curl: (7) Failed to connect\n"}]}},"restart":{"Name":"unless-stopped","MaximumRetryCount":0},"restarts":2,"ports":{"443/tcp":null,"80/tcp":[{"HostIp":"0.0.0.0","HostPort":"8081"},{"HostIp":"::","HostPort":"8081"}],"53/udp":[{"HostIp":"127.0.0.1","HostPort":"5353"}]},"mounts":[{"Type":"bind","Source":"/srv/shop/html","Destination":"/usr/share/nginx/html","Mode":"","RW":false,"Propagation":"rprivate"},{"Type":"volume","Name":"shop_data","Source":"/var/lib/docker/volumes/shop_data/_data","Destination":"/data","Driver":"local","Mode":"z","RW":true,"Propagation":""}],"networks":{"shop_default":{"IPAddress":"172.18.0.3","GlobalIPv6Address":""}}}"#;
// podman 4.3：健康检查叫 Healthcheck，没有重启策略的名称；已退出
const PODMAN_INSPECT: &str = r#"{"id":"a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90","image":"docker.io/library/alpine:latest","created":"2026-11-04T09:00:00+08:00","state":{"Status":"exited","Running":false,"OOMKilled":true,"ExitCode":137,"Error":"","StartedAt":"2026-11-04T09:10:00+08:00","FinishedAt":"2026-11-04T09:47:12+08:00","Healthcheck":{"Status":"","FailingStreak":0,"Log":null}},"restart":{"Name":"","MaximumRetryCount":0},"restarts":0,"ports":{},"mounts":[],"networks":{}}"#;

#[test]
fn detail_from_inspect() {
    let detail = parse_inspect(&read(DOCKER_INSPECT, 0)).unwrap();
    assert_eq!(detail.id, CID);
    assert_eq!(detail.image, "nginx:alpine");
    assert_eq!(detail.status, "running");
    assert_eq!(detail.started_at, Some(1_793_756_832_500));
    assert_eq!(detail.finished_at, None, "没有停止过");
    assert_eq!(detail.exit_code, None, "运行中的没有退出码");
    assert_eq!(
        (detail.restart_policy.as_str(), detail.restart_count),
        ("unless-stopped", 2)
    );
    let health = detail.health.as_ref().unwrap();
    assert_eq!(
        (health.status.as_str(), health.failing_streak),
        ("unhealthy", 3)
    );
    let last = health.last.as_ref().unwrap();
    assert_eq!(last.exit_code, 1);
    assert_eq!(last.output, "curl: (7) Failed to connect");
    assert!(last.at.is_some());
    assert_eq!(
        detail.ports,
        [
            PortBinding {
                container_port: 53,
                proto: "udp".into(),
                host_port: Some(5353),
                host_ip: Some("127.0.0.1".into()),
            },
            PortBinding {
                container_port: 80,
                proto: "tcp".into(),
                host_port: Some(8081),
                host_ip: None,
            },
            PortBinding {
                container_port: 443,
                proto: "tcp".into(),
                host_port: None,
                host_ip: None,
            },
        ],
        "IPv4 与 IPv6 的同一个端口只记一次"
    );
    assert_eq!(
        detail.mounts,
        [
            MountInfo {
                kind: "bind".into(),
                name: None,
                source: "/srv/shop/html".into(),
                destination: "/usr/share/nginx/html".into(),
                read_only: true,
            },
            MountInfo {
                kind: "volume".into(),
                name: Some("shop_data".into()),
                source: "/var/lib/docker/volumes/shop_data/_data".into(),
                destination: "/data".into(),
                read_only: false,
            },
        ]
    );
    assert_eq!(
        detail.networks,
        [NetworkInfo {
            name: "shop_default".into(),
            ip: "172.18.0.3".into(),
            ipv6: String::new(),
        }]
    );

    let detail = parse_inspect(&read(PODMAN_INSPECT, 0)).unwrap();
    assert_eq!(detail.status, "exited");
    assert_eq!(detail.exit_code, Some(137));
    assert!(detail.oom_killed);
    assert_eq!(detail.finished_at, Some(1_793_756_832_000));
    assert_eq!(detail.health, None, "没有设置健康检查");
    assert_eq!(detail.restart_policy, "no");

    assert!(matches!(
        parse_inspect(&read("Error: No such container: 3f4e", 1)),
        Err(ToolsError::DockerFailed(message)) if message.contains("No such container")
    ));
    assert!(matches!(
        parse_inspect(&read("template: :1: can't evaluate field Health", 0)),
        Err(ToolsError::DockerFailed(_))
    ));
}

/// 命令：Docker 页经 sudo 访问时以 sudo -n 开头；容器 ID 校验；模板作为一个参数传入。
#[test]
fn commands() {
    assert!(
        stats_command(Engine::Docker, false)
            .ends_with(" sh 'docker' 'stats' '--no-stream' '--no-trunc' '--format' '{{json .}}'")
    );
    assert!(stats_command(Engine::Podman, true).contains(" sh 'sudo' '-n' 'podman' 'stats'"));
    let inspect = inspect_command(Engine::Docker, CID, true).unwrap();
    assert!(inspect.contains(
        " sh 'sudo' '-n' 'docker' 'container' 'inspect' '--format' '{\"id\":{{json .Id}}"
    ));
    assert!(inspect.ends_with(&format!(" '{CID}'")));
    assert!(matches!(
        inspect_command(Engine::Docker, "web; id", false),
        Err(ToolsError::Invalid(_))
    ));
}

/// 在本机的 sh 中执行：假的 docker 收到的参数与命令行一致，输出照原样解析。
#[test]
fn stats_script_on_fake_docker() {
    let Some(runner) = ShRunner::isolated() else {
        return;
    };
    runner.fake(
        "docker",
        &format!(
            "[ \"$*\" = 'stats --no-stream --no-trunc --format {{{{json .}}}}' ] || {{ echo \"bad args: $*\"; exit 2; }}\ncat <<'EOF'\n{DOCKER_STATS}\nEOF"
        ),
    );
    let out = runner.run(
        &read_script(),
        &[],
        &[
            "docker",
            "stats",
            "--no-stream",
            "--no-trunc",
            "--format",
            "{{json .}}",
        ],
    );
    let stats = parse_stats(&result(&out)).unwrap();
    assert_eq!(stats.len(), 2);
}

/// 列表中的状态文字：退出码与健康状态。
#[test]
fn exit_code_and_health_from_status() {
    let c = |status: &str| Container {
        id: CID.into(),
        name: "x".into(),
        image: "x".into(),
        state: String::new(),
        status: status.into(),
        ports: String::new(),
        created: String::new(),
        project: None,
        service: None,
        working_dir: None,
        run: None,
    };
    assert_eq!(c("Exited (1) 10 minutes ago").exit_code(), Some(1));
    assert_eq!(c("Exited (0) 5 weeks ago").exit_code(), Some(0));
    assert_eq!(c("Restarting (137) 5 seconds ago").exit_code(), Some(137));
    assert_eq!(c("Up 3 days").exit_code(), None);
    assert_eq!(
        c("Up 3 days (healthy)").health(),
        Some(HealthState::Healthy)
    );
    assert_eq!(
        c("Up 10 minutes (unhealthy)").health(),
        Some(HealthState::Unhealthy)
    );
    assert_eq!(
        c("Up 2 seconds (health: starting)").health(),
        Some(HealthState::Starting)
    );
    assert_eq!(c("Up 3 days").health(), None);
    // 列表读取时的运行状态优先（状态文字是 docker ps 那一刻的，可能已经过时）
    let run = Container {
        run: Some(RunState {
            status: "exited".into(),
            exit_code: Some(2),
            health: Some(HealthState::Unhealthy),
            ..Default::default()
        }),
        ..c("Exited (1) 10 minutes ago")
    };
    assert_eq!(run.exit_code(), Some(2));
    assert_eq!(run.health(), Some(HealthState::Unhealthy));
    let fallback = Container {
        run: Some(RunState::default()),
        ..c("Up 3 days (healthy)")
    };
    assert_eq!(fallback.health(), Some(HealthState::Healthy));
}

/// `.State` 的 JSON：docker 的 Health、podman 较早的 Healthcheck，运行中的没有退出码，没有启动过的时间为空。
#[test]
fn run_state_from_inspect() {
    let docker: serde_json::Value = serde_json::from_str(
        r#"{"Status":"running","Running":true,"ExitCode":0,"OOMKilled":false,"StartedAt":"2026-11-04T09:49:04.944387787Z","FinishedAt":"0001-01-01T00:00:00Z","Health":{"Status":"unhealthy","FailingStreak":3,"Log":[]}}"#,
    )
    .unwrap();
    let run = RunState::from_json(&docker);
    assert_eq!(run.status, "running");
    assert_eq!(run.exit_code, None, "运行中的没有退出码");
    assert_eq!(run.started_at, Some(1_793_785_744_944));
    assert_eq!(run.finished_at, None);
    assert_eq!(run.health, Some(HealthState::Unhealthy));
    let podman: serde_json::Value = serde_json::from_str(
        r#"{"Status":"exited","Running":false,"ExitCode":137,"OOMKilled":true,"StartedAt":"2026-11-04T09:49:04Z","FinishedAt":"2026-11-04T10:49:04Z","Healthcheck":{"Status":"starting"}}"#,
    )
    .unwrap();
    let run = RunState::from_json(&podman);
    assert_eq!(
        (run.exit_code, run.oom_killed, run.health),
        (Some(137), true, Some(HealthState::Starting))
    );
    assert_eq!(run.finished_at, Some(1_793_789_344_000));
    let none: serde_json::Value =
        serde_json::from_str(r#"{"Status":"created","Health":{"Status":"none"}}"#).unwrap();
    assert_eq!(RunState::from_json(&none).health, None);
}
