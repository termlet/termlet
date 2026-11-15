//! 进程归属（cgroup）的测试：路径的各种写法，`@@cgroup` 段的解析，读取脚本在假 /proc 上取到的行。

use super::{ShRunner, borrow, fake_proc, proc_replace};
use crate::hosttools::cgroup::{Owner, owner, owners};
use crate::hosttools::process::parse_processes;
use crate::hosttools::scripts::proc_script;
use crate::hosttools::sections::Sections;

const ID: &str = "c07d8b88b8c6b55cb13f9e92654df607a79449021ceeeb816e058a36b291dc83";
const ID2: &str = "6a37fb5f446f75447b001193df87b8eb297266ac5574f8d3b21c32fb50447db9";

fn service(name: &str) -> Owner {
    Owner {
        service: Some(name.into()),
        container: None,
    }
}

fn container(id: &str) -> Owner {
    Owner {
        service: None,
        container: Some(id.into()),
    }
}

/// systemd 服务：各级 slice 之后第一个不是 slice 的；登录会话、根、slice 本身不是服务。
#[test]
fn systemd_services() {
    assert_eq!(
        owner("/system.slice/nginx.service"),
        service("nginx.service")
    );
    assert_eq!(
        owner("/system.slice/system-getty.slice/getty@tty1.service"),
        service("getty@tty1.service"),
        "模板实例在子 slice 中"
    );
    assert_eq!(
        owner(r"/system.slice/system-openvpn\x2dclient.slice/openvpn-client@office.service"),
        service("openvpn-client@office.service"),
    );
    assert_eq!(
        owner("/system.slice/docker.service"),
        service("docker.service"),
        "dockerd 自己属于 docker 服务，不在容器中"
    );
    // 服务自己建的子 cgroup 仍属于它
    assert_eq!(
        owner("/system.slice/containerd.service/sub"),
        service("containerd.service")
    );
    for path in [
        "/",
        "",
        "/init.scope",
        "/system.slice",
        "/user.slice/user-1000.slice/session-3.scope",
        "/system.slice/bad name.service",
        "/system.slice/.service",
    ] {
        assert_eq!(owner(path), Owner::default(), "{path}");
    }
}

/// 容器：Docker（cgroupfs、systemd 驱动）、Podman（systemd、cgroupfs、quadlet、rootless）、containerd 与 CRI-O
/// （Kubernetes）、nerdctl；conmon 在容器之外。
#[test]
fn containers() {
    for path in [
        format!("/docker/{ID}"),
        format!("/system.slice/docker-{ID}.scope"),
        format!("/machine.slice/libpod-{ID}.scope"),
        format!("/libpod_parent/libpod-{ID}"),
        format!("/kubepods/burstable/pod0b1c2d3e-aaaa-bbbb-cccc-0123456789ab/{ID}"),
        format!(
            "/kubepods.slice/kubepods-burstable.slice/kubepods-burstable-pod0b1c.slice/cri-containerd-{ID}.scope"
        ),
        format!("/kubepods.slice/kubepods-besteffort.slice/crio-{ID}.scope"),
        format!("/system.slice/nerdctl-{ID}.scope"),
        format!("/k8s.io/{ID}"),
    ] {
        assert_eq!(owner(&path), container(ID), "{path}");
    }
    // 容器中的子 cgroup、容器中再套容器（取外层的）
    assert_eq!(
        owner(&format!(
            "/system.slice/docker-{ID}.scope/system.slice/nginx.service"
        )),
        container(ID)
    );
    assert_eq!(owner(&format!("/docker/{ID}/docker/{ID2}")), container(ID));
    // quadlet：容器在服务之下，两者都有；rootless 的 Podman 在用户的 systemd 实例（user@1000.service）之下
    assert_eq!(
        owner(&format!("/system.slice/web.service/libpod-payload-{ID}")),
        Owner {
            service: Some("web.service".into()),
            container: Some(ID.into()),
        }
    );
    assert_eq!(
        owner(&format!(
            "/user.slice/user-1000.slice/user@1000.service/user.slice/libpod-{ID}.scope"
        )),
        Owner {
            service: Some("user@1000.service".into()),
            container: Some(ID.into()),
        }
    );
    // conmon 与不像 ID 的名称不算
    for path in [
        format!("/machine.slice/libpod-conmon-{ID}.scope"),
        format!("/kubepods.slice/crio-conmon-{ID}.scope"),
        format!("/docker/{}", &ID[..12]),
        format!("/docker/{}", ID.to_uppercase()),
        "/lxc/web".to_string(),
    ] {
        assert_eq!(owner(&path).container, None, "{path}");
    }
}

/// `@@cgroup` 段：同一路径写一次、各进程写编号；同一个 PID 出现两次时都不认；不认识的编号、没有归属的忽略。
#[test]
fn section_with_shared_paths() {
    let out = format!(
        "@@cgroup\n \
         =1 /docker/{ID}\n \
         10 1\n \
         11 1\n \
         =2 /system.slice/nginx.service\n \
         12 2\n \
         =3 /user.slice/user-1000.slice/session-3.scope\n \
         13 3\n \
         14 9\n \
         15 2\n \
         15 1\n \
         x 1\n\
         @@procs\n"
    );
    let sections = Sections::parse(&out);
    let map = owners(sections.get("cgroup"));
    assert_eq!(map.get(&10), Some(&container(ID)));
    assert_eq!(map.get(&11), Some(&container(ID)));
    assert_eq!(map.get(&12), Some(&service("nginx.service")));
    assert!(!map.contains_key(&13), "登录会话没有归属");
    assert!(!map.contains_key(&14), "不认识的编号");
    assert!(!map.contains_key(&15), "出现两次的 PID 不认");
    assert_eq!(map.len(), 3);
    assert!(owners(None).is_empty());
}

/// 读取脚本在假 /proc 上：v1 取容器的那一行（不是混合模式最后那行 containerd 服务）、没有容器时取 name=systemd 的那行；
/// v2 只有一行；登录会话、根 cgroup 中的进程不输出；内核线程等不在进程列表中的 PID 不影响。
#[test]
fn script_reads_the_owner_of_each_process() {
    let Some(runner) = ShRunner::new() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    fake_proc(dir.path());
    runner.fake(
        "getconf",
        "case $1 in CLK_TCK) echo 100;; PAGESIZE) echo 4096;; esac",
    );
    runner.fake("id", "echo 1000");
    runner.fake("tmux", "exit 1");
    let write = |pid: u32, content: String| {
        std::fs::write(dir.path().join(format!("proc/{pid}/cgroup")), content).unwrap()
    };
    // v1 + 混合模式中的容器：各控制器都带着容器的路径，0:: 那行是 containerd 的（不能取它）
    write(
        300,
        format!(
            "12:pids:/docker/{ID}\n11:rdma:/\n10:memory:/docker/{ID}\n1:name=systemd:/docker/{ID}\n\
             0::/system.slice/containerd.service\n"
        ),
    );
    // v1 的服务：控制器的行不带关键词，取 name=systemd 的那行
    write(
        301,
        "11:pids:/system.slice/nginx.service\n10:hugetlb:/\n9:memory:/system.slice/nginx.service\n\
         1:name=systemd:/system.slice/nginx.service\n0::/system.slice/nginx.service\n"
            .into(),
    );
    // 混合模式的登录会话：没有归属
    write(
        400,
        "1:name=systemd:/user.slice/user-1000.slice/session-1.scope\n0::/user.slice/user-1000.slice/session-1.scope\n"
            .into(),
    );
    // v2
    write(
        401,
        "0::/system.slice/system-getty.slice/getty@tty1.service\n".into(),
    );
    write(1, "0::/init.scope\n".into());
    write(2, "0::/\n".into());
    write(500, format!("0::/machine.slice/libpod-{ID2}.scope\n"));
    write(600, format!("0::/system.slice/docker-{ID}.scope\n"));
    let replace = proc_replace(dir.path());
    let out = runner.run(&proc_script(), &borrow(&replace), &["", ""]);
    let (list, _, _) = parse_processes(&out, "").unwrap_or_else(|e| panic!("{e}\n{out}"));
    let of = |pid: u32| {
        let p = list.processes.iter().find(|p| p.pid == pid).unwrap();
        (p.service.as_deref(), p.container.as_deref())
    };
    assert_eq!(of(300), (None, Some(ID)), "{out}");
    assert_eq!(of(301), (Some("nginx.service"), None));
    assert_eq!(of(400), (None, None));
    assert_eq!(of(401), (Some("getty@tty1.service"), None));
    assert_eq!(of(1), (None, None));
    assert_eq!(of(500), (None, Some(ID2)));
    assert_eq!(of(600), (None, Some(ID)));
    assert_eq!(of(402), (None, None), "没有 cgroup 文件的进程");
    // 同一个路径只输出一次
    let section = out
        .split("@@cgroup\n")
        .nth(1)
        .and_then(|rest| rest.split("@@procs").next())
        .unwrap();
    assert_eq!(
        section.matches(&format!("/docker/{ID}")).count(),
        1,
        "{section}"
    );
    assert_eq!(
        section.matches("/system.slice/docker-").count(),
        1,
        "{section}"
    );
    assert!(!section.contains("session-1.scope"), "{section}");
    assert!(!section.contains("init.scope"), "{section}");
}
