//! Docker 清理的测试：清单的解析与按勾选现算、prune 输出的解析、读清单的脚本。样例取自 Docker 26.1.5
//! （Debian 13 的 docker.io）的真实输出。

use super::ShRunner;
use crate::hosttools::prune::{cleanup_command, parse_cleanup};
use crate::hosttools::scripts::cleanup_script;
use crate::hosttools::{Action, Engine, PruneKind, PruneOutcome, ToolsError, parse_prune};

const OLD_APP: &str = "0d78e4340648eaa7708096578b26913f694bb99cbe46c0fb6c6d2ba00f5f1697";
const API: &str = "2bb5c03efec39be1250ccc12a4664fd9b96c5038b66ad65bb57281b9adacd08d";
const NEVER: &str = "d14ed308511d153c73a8f561db5ea319ec62049431d8ea5c877cf6615dde859f";
const JOB_OK: &str = "d99758802bfe83f537b69fe0782c7b8098144982205caaf8662de0e903d7f7aa";
const WEB: &str = "9e30c7633842bddaf59b3b95822287999ec2d604e9c7816859154da03475d4d1";
const ON_STOP_NET: &str = "3d96f22e0e5201890be5ae5726dbf654b141e84f6ccf39627e8ba74006b8a56d";
/// 悬空镜像：没有容器用的、运行中的 api 用的、只有已停止的 old-app 用的
const DANGLING_FREE: &str = "0a597f66fdd604fd8907093726179c7af46f415d8a1a4db06cc68ea27c75ebf8";
const DANGLING_API: &str = "d97b5474d7ea73ff39b57c17230c547916fe10759183fc60522f557f193776c1";
const DANGLING_OLD: &str = "b4e162f53bade8e37475f8b739e0f4a27020f54b65d8096f696ba0f5b32f3609";
const BASE: &str = "7468e10e9a682e2abcb8e9e83337d5af8c224a8cfb7a6591438eaa022174bedf";
const ANON: &str = "e4b215600ba4b0479b4f81e9182ec8bc85d4d1096cf4a99bd1f377f9786e83a7";
const OLD_ANON: &str = "f00d15600ba4b0479b4f81e9182ec8bc85d4d1096cf4a99bd1f377f9786e83a7";

/// `system df -v --format '{{json .}}'`（一行，节选）。
fn df() -> String {
    format!(
        r#"{{"Images":[{{"Containers":"0","ID":"sha256:{DANGLING_FREE}","Repository":"<none>","SharedSize":"2.025MB","Size":"2.02MB","Tag":"<none>","UniqueSize":"0B"}},{{"Containers":"2","ID":"sha256:{DANGLING_API}","Repository":"<none>","SharedSize":"0B","Size":"4.05MB","Tag":"<none>","UniqueSize":"4.049MB"}},{{"Containers":"1","ID":"sha256:{DANGLING_OLD}","Repository":"<none>","Size":"300MB","UniqueSize":"120MB"}},{{"Containers":"4","ID":"sha256:{BASE}","Repository":"local/base","Size":"2.02MB","Tag":"1","UniqueSize":"0B"}}],"Containers":[{{"ID":"{OLD_APP}","Size":"1.5kB","State":"exited"}},{{"ID":"{API}","Size":"0B","State":"running"}},{{"ID":"{NEVER}","Size":"0B","State":"created"}},{{"ID":"{JOB_OK}","Size":"2MB","State":"exited"}},{{"ID":"{WEB}","Size":"0B","State":"running"}}],"Volumes":[{{"Labels":"","Links":"1","Name":"web-data","Size":"5MB"}},{{"Labels":"com.docker.volume.anonymous=","Links":"0","Name":"{ANON}","Size":"10MB"}},{{"Labels":"com.docker.volume.anonymous=","Links":"1","Name":"{OLD_ANON}","Size":"7MB"}},{{"Labels":"","Links":"0","Name":"old-data","Size":"120MB"}}],"BuildCache":[{{"ID":"gqnd","InUse":"false","Shared":"true","Size":"6B"}},{{"ID":"2o84","InUse":"false","Shared":"false","Size":"44B"}},{{"ID":"m8kd","InUse":"true","Shared":"false","Size":"8B"}}]}}"#
    )
}

/// 读清单的输出（`@@` 之后的内容按脚本中 `q` 的写法前面有一个空格）。
fn plan_output(df: &str, api: &str, endpoints: bool) -> String {
    let endpoints = if endpoints {
        format!(
            " d8b8301c05b2fdfa4302476dc28903e62627c34d73a8c009c22f3932b19c4cef|{API},\n a33a8deeac3dfade66552fd3664cbdb251ae932b11c8927218507bce93d4e64d|{WEB},\n 480c5301e27a8442a89e28fe79c2d58fc87b1fd4db66b96a7fb776cff698fea7|\n b4852cb686afc3685de03ee685ebe8455f0fbf5110ea23a35e1096224b831912|\n"
        )
    } else {
        String::new()
    };
    format!(
        "@@engine docker
@@api
 {api}
@@df
 {df}
@@dangling
 sha256:{DANGLING_FREE}
 sha256:{DANGLING_API}
 sha256:{DANGLING_OLD}
@@volumes
 {{\"Driver\":\"local\",\"Labels\":\"com.docker.volume.anonymous=\",\"Name\":\"{ANON}\"}}
 {{\"Driver\":\"local\",\"Labels\":\"com.docker.volume.anonymous=\",\"Name\":\"{OLD_ANON}\"}}
 {{\"Driver\":\"local\",\"Labels\":\"\",\"Name\":\"old-data\"}}
 {{\"Driver\":\"local\",\"Labels\":\"\",\"Name\":\"web-data\"}}
@@networks
 {{\"Driver\":\"bridge\",\"ID\":\"d8b8301c05b2fdfa4302476dc28903e62627c34d73a8c009c22f3932b19c4cef\",\"Name\":\"app-net\"}}
 {{\"Driver\":\"bridge\",\"ID\":\"a33a8deeac3dfade66552fd3664cbdb251ae932b11c8927218507bce93d4e64d\",\"Name\":\"bridge\"}}
 {{\"Driver\":\"host\",\"ID\":\"6f38c83520d3aaaa66552fd3664cbdb251ae932b11c8927218507bce93d4e64d\",\"Name\":\"host\"}}
 {{\"Driver\":\"bridge\",\"ID\":\"480c5301e27a8442a89e28fe79c2d58fc87b1fd4db66b96a7fb776cff698fea7\",\"Name\":\"old-net\"}}
 {{\"Driver\":\"bridge\",\"ID\":\"b4852cb686afc3685de03ee685ebe8455f0fbf5110ea23a35e1096224b831912\",\"Name\":\"stop-net\"}}
@@endpoints
{endpoints}@@inspect 6
 {OLD_APP}|sha256:{DANGLING_OLD}|exited||bridge,|/old-app
 {API}|sha256:{DANGLING_API}|running||app-net,|/api
 {NEVER}|sha256:{BASE}|created||bridge,|/never-started
 {JOB_OK}|sha256:{BASE}|exited|{OLD_ANON},|bridge,|/job-ok
 {WEB}|sha256:{BASE}|running|web-data,|bridge,|/web-1
 {ON_STOP_NET}|sha256:{BASE}|exited||stop-net,|/on-stop-net
"
    )
}

fn names(preview: &crate::hosttools::PrunePreview) -> Vec<String> {
    preview.items.iter().map(|i| i.name.clone()).collect()
}

/// 清单：已停止的容器（含没有启动过的）；Docker 只看正连着的容器，停止的容器所在的网络也会被清理；Docker 23 起只清理
/// 匿名卷；悬空镜像中没有容器用的。一并清理容器时，只被它们使用的卷与镜像也算在内。大小取 df（镜像是独有的大小）。
#[test]
fn plan_follows_the_prune_rules() {
    let plan = parse_cleanup(&plan_output(&df(), "1.45", true)).unwrap();
    assert_eq!(plan.engine, Engine::Docker);
    assert!(plan.anonymous_volumes_only && plan.sizes && plan.containers_known);
    assert_eq!(plan.kinds(), PruneKind::ALL.to_vec());

    let containers = plan.preview(PruneKind::Containers, true);
    assert_eq!(
        names(&containers),
        vec!["old-app", "never-started", "job-ok", "on-stop-net"]
    );
    assert_eq!(containers.bytes, Some(1500 + 2_000_000), "可写层的大小");

    for with in [false, true] {
        let networks = plan.preview(PruneKind::Networks, with);
        assert_eq!(
            names(&networks),
            vec!["old-net", "stop-net"],
            "内置的不算；停止的容器所在的网络也会被清理"
        );
        assert_eq!(networks.bytes, Some(0));
    }

    let volumes = plan.preview(PruneKind::Volumes, false);
    assert_eq!(names(&volumes), vec![ANON], "命名的卷不在其中");
    assert_eq!(volumes.bytes, Some(10_000_000));
    let volumes = plan.preview(PruneKind::Volumes, true);
    assert_eq!(
        names(&volumes),
        vec![ANON, OLD_ANON],
        "只被已停止的 job-ok 使用的匿名卷"
    );
    assert_eq!(plan.named_unused_volumes(false), 1, "old-data");

    let images = plan.preview(PruneKind::Images, false);
    assert_eq!(names(&images), vec![&DANGLING_FREE[..12]]);
    assert_eq!(images.bytes, Some(0), "与别的镜像共用层：独有的大小是 0");
    let images = plan.preview(PruneKind::Images, true);
    assert_eq!(
        names(&images),
        vec![&DANGLING_FREE[..12], &DANGLING_OLD[..12]],
        "运行中的容器用的悬空镜像不清理"
    );
    assert_eq!(images.bytes, Some(120_000_000));

    let cache = plan.preview(PruneKind::BuildCache, false);
    assert_eq!(cache.count, 2, "在用的不算");
    assert_eq!(cache.bytes, Some(44), "与镜像共用的不算");
}

/// 较早的 Docker（API 1.41）清理全部未使用的卷；读不到 df 时没有大小、没有构建缓存；读不到正连着的容器时按全部
/// 容器所在的网络算（与 Podman 相同）。
#[test]
fn plan_without_sizes_or_endpoints() {
    let plan = parse_cleanup(&plan_output("", "1.41", false)).unwrap();
    assert!(!plan.anonymous_volumes_only && !plan.sizes);
    assert_eq!(plan.build_cache, None);
    assert!(!plan.kinds().contains(&PruneKind::BuildCache));
    let volumes = plan.preview(PruneKind::Volumes, false);
    assert_eq!(names(&volumes), vec![ANON, "old-data"]);
    assert_eq!(volumes.bytes, None, "读不到大小");
    assert_eq!(plan.named_unused_volumes(false), 0);
    let networks = plan.preview(PruneKind::Networks, false);
    assert_eq!(
        names(&networks),
        vec!["old-net"],
        "停止的容器所在的网络留着"
    );
    let networks = plan.preview(PruneKind::Networks, true);
    assert_eq!(names(&networks), vec!["old-net", "stop-net"]);

    // 容器读不全（inspect 少了）：卷与镜像按 df 中使用它们的容器数
    let partial = plan_output(&df(), "1.45", true).replace("@@inspect 6", "@@inspect 7");
    let plan = parse_cleanup(&partial).unwrap();
    assert!(!plan.containers_known);
    assert_eq!(
        names(&plan.preview(PruneKind::Images, true)),
        vec![&DANGLING_FREE[..12]],
        "df 说 old-app 的镜像有容器用"
    );
    assert_eq!(names(&plan.preview(PruneKind::Volumes, true)), vec![ANON]);
}

/// 没有 docker、没有运行、没有权限：与 Docker 页相同的错误。
#[test]
fn plan_errors_follow_the_docker_page() {
    assert_eq!(
        parse_cleanup("@@nodocker\nLinux\n"),
        Err(ToolsError::NoDocker)
    );
    assert_eq!(
        parse_cleanup(
            "@@engine docker\n@@error 1\n Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?\n"
        ),
        Err(ToolsError::DockerDown)
    );
    assert!(matches!(
        parse_cleanup("@@engine docker\n@@sudo password\n@@error 1\n permission denied\n"),
        Err(ToolsError::NeedRoot { .. })
    ));
}

/// prune 的输出：Docker 写出删除的对象与释放的空间（镜像各层另有几行，不数；网络不写空间；构建缓存是一张表与
/// Total）；Podman 每行一个。
#[test]
fn prune_outputs() {
    let containers = "Deleted Containers:
0d78e4340648eaa7708096578b26913f694bb99cbe46c0fb6c6d2ba00f5f1697
d14ed308511d153c73a8f561db5ea319ec62049431d8ea5c877cf6615dde859f

Total reclaimed space: 1.502kB";
    assert_eq!(
        parse_prune(PruneKind::Containers, containers),
        PruneOutcome {
            deleted: Some(2),
            reclaimed: Some(1502)
        }
    );
    assert_eq!(
        parse_prune(PruneKind::Containers, "Total reclaimed space: 0B\n"),
        PruneOutcome {
            deleted: Some(0),
            reclaimed: Some(0)
        },
        "没有可以清理的"
    );
    assert_eq!(
        parse_prune(
            PruneKind::Networks,
            "Deleted Networks:\nold-net\nstop-net\n\n"
        ),
        PruneOutcome {
            deleted: Some(2),
            reclaimed: None
        }
    );
    assert_eq!(
        parse_prune(PruneKind::Networks, ""),
        PruneOutcome {
            deleted: Some(0),
            reclaimed: None
        }
    );
    let images = "Deleted Images:
untagged: local/app@sha256:aaaa
deleted: sha256:b4e162f53bade8e37475f8b739e0f4a27020f54b65d8096f696ba0f5b32f3609
deleted: sha256:0a597f66fdd604fd8907093726179c7af46f415d8a1a4db06cc68ea27c75ebf8

Total reclaimed space: 1.2GB";
    assert_eq!(
        parse_prune(PruneKind::Images, images),
        PruneOutcome {
            deleted: None,
            reclaimed: Some(1_200_000_000)
        }
    );
    let builder = "ID\t\t\t\t\t\tRECLAIMABLE\tSIZE\t\tLAST ACCESSED
i7wvon622j0i0j4puivef0bo5*              \ttrue \t\t0B        \t17 seconds ago
2o842y741u4o0pqlu4s0cvr2s*              \ttrue \t44B       \t17 seconds ago
Total:\t56B";
    assert_eq!(
        parse_prune(PruneKind::BuildCache, builder),
        PruneOutcome {
            deleted: None,
            reclaimed: Some(56)
        }
    );
    assert_eq!(
        parse_prune(PruneKind::Volumes, "old-data\n3f2a\n"),
        PruneOutcome {
            deleted: Some(2),
            reclaimed: None
        },
        "Podman"
    );
}

/// 执行的命令：`docker container prune -f` 等；Podman 没有构建缓存的清理。
#[test]
fn prune_commands() {
    let line = |engine, kind| Action::Prune { engine, kind }.command_line();
    assert_eq!(
        line(Engine::Docker, PruneKind::Containers).unwrap(),
        "docker container prune -f"
    );
    assert_eq!(
        line(Engine::Docker, PruneKind::BuildCache).unwrap(),
        "docker builder prune -f"
    );
    assert_eq!(
        line(Engine::Podman, PruneKind::Volumes).unwrap(),
        "podman volume prune -f"
    );
    assert!(matches!(
        line(Engine::Podman, PruneKind::BuildCache),
        Err(ToolsError::Invalid(_))
    ));
}

/// 读清单的脚本在假的 docker 上：各段都在，容器总数与 inspect 的行数一致；df 不支持 JSON 时那一段为空。
#[test]
fn cleanup_script_on_fake_docker() {
    let Some(runner) = ShRunner::isolated() else {
        return;
    };
    runner.fake("id", "echo 1000");
    let df = df();
    runner.fake(
        "docker",
        &[
            r#"case "$1 $2" in
"ps -a") printf '%s\n' 0d78e4340648 9e30c7633842;;
"ps -aq") printf '%s\n' 0d78e4340648eaa7708096578b26913f694bb99cbe46c0fb6c6d2ba00f5f1697 9e30c7633842bddaf59b3b95822287999ec2d604e9c7816859154da03475d4d1;;
"version --format") echo 1.45;;
"system df") if [ "$NO_DF" = 1 ]; then echo "unknown flag: --format" >&2; exit 1; fi; printf '%s' '"#,
            // 与真实的 docker 一样，JSON 的最后没有换行
            &df,
            r#"';;
"images -f") echo sha256:0a597f66fdd604fd8907093726179c7af46f415d8a1a4db06cc68ea27c75ebf8;;
"volume ls") echo '{"Labels":"","Name":"old-data"}';;
"network ls") case "$3" in -q) echo 480c5301e27a;; *) echo '{"ID":"480c5301e27a8442a89e28fe79c2d58fc87b1fd4db66b96a7fb776cff698fea7","Name":"old-net"}';; esac;;
"network inspect") echo '480c5301e27a8442a89e28fe79c2d58fc87b1fd4db66b96a7fb776cff698fea7|';;
"container inspect") echo '0d78e4340648eaa7708096578b26913f694bb99cbe46c0fb6c6d2ba00f5f1697|sha256:7468e10e9a682e2abcb8e9e83337d5af8c224a8cfb7a6591438eaa022174bedf|exited||bridge,|/old-app'
  echo '9e30c7633842bddaf59b3b95822287999ec2d604e9c7816859154da03475d4d1|sha256:7468e10e9a682e2abcb8e9e83337d5af8c224a8cfb7a6591438eaa022174bedf|running|old-data,|bridge,|/web-1';;
esac"#,
        ]
        .concat(),
    );
    let out = runner.run(&cleanup_script(), &[], &[]);
    let plan = parse_cleanup(&out).unwrap_or_else(|e| panic!("{e}\n{out}"));
    assert!(plan.sizes && plan.containers_known, "{out}");
    assert_eq!(plan.containers.len(), 2);
    assert_eq!(
        names(&plan.preview(PruneKind::Containers, true)),
        vec!["old-app"]
    );
    assert_eq!(
        names(&plan.preview(PruneKind::Networks, true)),
        vec!["old-net"]
    );
    assert!(
        plan.preview(PruneKind::Volumes, true).items.is_empty(),
        "命名的卷不在其中"
    );
    assert_eq!(plan.preview(PruneKind::Images, true).count, 1);
    let out = runner.run_env(&cleanup_script(), &[("NO_DF", "1")]);
    let plan = parse_cleanup(&out).unwrap_or_else(|e| panic!("{e}\n{out}"));
    assert!(!plan.sizes, "{out}");
    assert!(cleanup_command().starts_with("sh -c 'export LC_ALL=C;"));
}
