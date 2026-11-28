//! 在本机的 WSL 发行版中执行（只读）：进程、网络、服务、Docker 与资源监控。没有 WSL 发行版时跳过；
//! `TERMLET_TEST_WSL` 可以指定发行版。

use std::time::Duration;

use crate::hosttools::{HostTools, Target, ToolsError};
use crate::monitor::ResourceMonitor;

/// 测试用的发行版：`TERMLET_TEST_WSL`，否则本机第一个（不含 Docker Desktop 自己的发行版）。
fn test_distro() -> Option<String> {
    if let Ok(distro) = std::env::var("TERMLET_TEST_WSL") {
        return Some(distro);
    }
    let distro = crate::localshell::wsl::distributions()
        .into_iter()
        .find(|d| !d.starts_with("docker-desktop"));
    if distro.is_none() {
        println!("本机没有 WSL 发行版，跳过");
    }
    distro
}

#[test]
fn tools_in_wsl() {
    let Some(distro) = test_distro() else {
        return;
    };
    let rt = crate::runtime::runtime();
    // 第一次启动发行版可能要几秒：先等它起来
    rt.block_on(crate::wsl::exec(
        &distro,
        "",
        "true",
        64,
        Duration::from_secs(60),
    ))
    .unwrap();
    let target = Target::wsl(&distro, "");
    let tools = HostTools::new("termlet-wsl-test");
    rt.block_on(async {
        let list = tools
            .processes(&target, crate::hosttools::PROCESS_INTERVAL)
            .await
            .unwrap();
        assert!(
            !list.processes.is_empty() && list.cores > 0 && list.mem_total > 0,
            "{list:?}"
        );
        assert!(list.uid.is_some());

        let conns = tools.connections(&target).await.unwrap();
        assert!(conns.total >= conns.conns.len());

        match tools.services(&target).await {
            Ok(services) => assert!(!services.services.is_empty()),
            Err(ToolsError::NoSystemd) => println!("发行版没有启用 systemd"),
            Err(other) => panic!("读取服务：{other:?}"),
        }
        match tools.docker(&target).await {
            Ok(info) => println!("{:?}：{} 个容器", info.engine, info.containers.len()),
            Err(
                error @ (ToolsError::NoDocker
                | ToolsError::DockerDown
                | ToolsError::DockerDesktopDown
                | ToolsError::NeedRoot { .. }),
            ) => println!("Docker 不可用：{error}"),
            Err(other) => panic!("读取 Docker：{other:?}"),
        }

        let monitor = ResourceMonitor::new();
        let first = monitor.sample(&target).await.unwrap();
        assert!(first.mem_total > 0 && first.cores > 0, "{first:?}");
    });
}
