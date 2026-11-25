//! 批量执行的测试：选主机（不连网络）、并发与失败的上限、取消（连不上的端口），以及连测试容器的执行、超时、
//! 标准输入、传输（`TERMLET_TEST_SSH=127.0.0.1:2222`，没有设置时跳过）。

use std::sync::Arc;
use std::time::Duration;

use termlet_proto::{BatchHostResult, BatchStatus, ErrorCode, Lang, Selector};
use tokio::sync::{mpsc, watch};

use super::*;
use crate::model::{CliAccess, Group, Host, Protocol, SecretField};
use crate::secret::SecretString;
use crate::store::{Owner, Position, Store};
use crate::testkit::{accept_host_key, block_on, memory_store, test_server};

fn group(store: &Store, name: &str, parent: Option<&str>) -> String {
    store
        .create(
            Group {
                name: name.into(),
                ..Default::default()
            },
            parent.map(str::to_string),
            Position::Last,
        )
        .unwrap()
        .to_string()
}

fn host(store: &Store, name: &str, port: u16, allowed: bool, group: Option<&str>, tags: &[&str]) {
    let id = store
        .create(
            Host {
                name: name.into(),
                address: "127.0.0.1".into(),
                port,
                username: crate::testkit::SSH_USER.into(),
                tags: tags.iter().map(|t| t.to_string()).collect(),
                cli: CliAccess {
                    allowed,
                    credential: None,
                },
                ..Default::default()
            },
            group.map(str::to_string),
            Position::Last,
        )
        .unwrap();
    store
        .set_secret(
            &Owner::host(&id),
            SecretField::Password,
            Some(&SecretString::new(crate::testkit::SSH_PASSWORD)),
        )
        .unwrap();
}

fn names(list: &[Record<Host>]) -> Vec<&str> {
    list.iter().map(|h| h.doc.name.as_str()).collect()
}

fn select(hosts: &[&str], groups: &[&str], tags: &[&str], exclude: &[&str]) -> Selector {
    let own = |l: &[&str]| l.iter().map(|s| s.to_string()).collect();
    Selector {
        hosts: own(hosts),
        groups: own(groups),
        tags: own(tags),
        exclude: own(exclude),
        all: false,
    }
}

/// 只接受连接、立即关闭的端口（连接很快失败）。
fn dead_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            drop(stream);
        }
    });
    port
}

#[test]
fn selection() {
    let store = memory_store();
    let prod = group(&store, "生产", None);
    let web = group(&store, "web", Some(&prod));
    let other_web = group(&store, "web", None);
    host(&store, "web-1", 22, true, Some(&web), &["nginx"]);
    host(&store, "web-2", 22, true, Some(&web), &["Nginx"]);
    host(&store, "db-1", 22, true, Some(&prod), &["mysql"]);
    host(&store, "db-2", 22, false, Some(&prod), &[]);
    host(&store, "lab", 22, true, Some(&other_web), &["nginx"]);
    host(&store, "lab", 22, true, None, &[]);
    let resolve = |s: &Selector, access| resolve(&store, s, access, Lang::Zh);

    // 分组含子分组，按名称排序；不允许的列为跳过
    let r = resolve(&select(&[], &["生产"], &[], &[]), Access::Cli).unwrap();
    assert_eq!(names(&r.run), ["db-1", "web-1", "web-2"]);
    assert_eq!(r.skipped.len(), 1);
    assert_eq!(r.skipped[0].1.error_code(), Some(ErrorCode::HostNotAllowed));
    // 界面上的批量执行不要求允许 AI 工具与脚本使用
    let r = resolve(&select(&[], &["生产"], &[], &[]), Access::User).unwrap();
    assert_eq!(r.run.len(), 4);
    // 同名的分组都算；路径只认对得上的
    let r = resolve(&select(&[], &["web"], &[], &[]), Access::Cli).unwrap();
    assert_eq!(names(&r.run), ["lab", "web-1", "web-2"]);
    let r = resolve(&select(&[], &["生产/web"], &[], &[]), Access::Cli).unwrap();
    assert_eq!(names(&r.run), ["web-1", "web-2"]);
    // 标签不区分大小写；与分组同用时取交集
    let r = resolve(&select(&[], &[], &["NGINX"], &[]), Access::Cli).unwrap();
    assert_eq!(names(&r.run), ["lab", "web-1", "web-2"]);
    let r = resolve(&select(&[], &["生产"], &["nginx"], &[]), Access::Cli).unwrap();
    assert_eq!(names(&r.run), ["web-1", "web-2"]);
    // 列出的主机：同名的都选上；加上排除
    let r = resolve(&select(&["lab", "db-1"], &[], &[], &[]), Access::Cli).unwrap();
    assert_eq!(names(&r.run), ["db-1", "lab", "lab"]);
    let r = resolve(
        &select(&["db-1"], &["生产/web"], &[], &["web-2"]),
        Access::Cli,
    )
    .unwrap();
    assert_eq!(names(&r.run), ["db-1", "web-1"]);
    let all = Selector {
        all: true,
        exclude: vec!["lab".into()],
        ..Default::default()
    };
    assert_eq!(
        names(&resolve(&all, Access::Cli).unwrap().run),
        ["db-1", "web-1", "web-2"]
    );

    // 错误：没有选择、找不到的主机或分组、一台也没有、都不能执行
    let err = |s: Selector| resolve(&s, Access::Cli).unwrap_err().code;
    assert_eq!(err(Selector::default()), ErrorCode::BadRequest);
    assert_eq!(
        err(select(&["nope"], &[], &[], &[])),
        ErrorCode::HostNotFound
    );
    assert_eq!(
        err(select(&[], &["测试"], &[], &[])),
        ErrorCode::HostNotFound
    );
    assert_eq!(
        err(select(&[], &[], &["redis"], &[])),
        ErrorCode::HostNotFound
    );
    assert_eq!(
        err(select(&["db-1"], &[], &[], &["nope"])),
        ErrorCode::HostNotFound
    );
    assert_eq!(
        err(select(&["db-2"], &[], &[], &[])),
        ErrorCode::HostNotAllowed
    );
}

#[test]
fn telnet_and_serial_hosts_are_skipped() {
    let store = memory_store();
    let id = store
        .create(
            Host {
                name: "switch".into(),
                protocol: Protocol::Telnet,
                address: "10.0.0.1".into(),
                cli: CliAccess {
                    allowed: true,
                    credential: None,
                },
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    host(&store, "web", 22, true, None, &[]);
    let selector = Selector {
        all: true,
        ..Default::default()
    };
    let r = resolve(&store, &selector, Access::User, Lang::En).unwrap();
    assert_eq!(names(&r.run), ["web"]);
    assert_eq!(r.skipped[0].0.id, id);
    assert!(r.skipped[0].1.message.contains("not an SSH host"));
}

#[test]
fn download_folders_are_unique() {
    let store = memory_store();
    host(&store, "web:1", 22, true, None, &[]);
    host(&store, "web", 22, true, None, &[]);
    host(&store, "WEB", 22, true, None, &[]);
    let hosts = store.list::<Host>();
    let folders = download_folders(&hosts);
    for (host, folder) in hosts.iter().zip(&folders) {
        match host.doc.name.as_str() {
            "web:1" => assert_eq!(folder, "web_1"),
            _ => assert!(folder.ends_with(host.id.as_str()), "{folder}"),
        }
    }
}

/// 执行一批，返回结果与开始的先后。
fn run_batch(
    store: Arc<Store>,
    hosts: Vec<Record<Host>>,
    job: Job,
    options: Options,
    stop: watch::Receiver<bool>,
) -> (Vec<BatchHostResult>, Vec<usize>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let results = block_on(run(
        store,
        Access::Cli,
        hosts,
        job,
        options,
        Lang::Zh,
        stop,
        tx,
    ));
    let mut started = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let Event::Started(i) = event {
            started.push(i);
        }
    }
    (results, started)
}

fn options(parallel: usize) -> Options {
    Options {
        parallel,
        timeout: None,
        max_fail: None,
        output_limit: 1 << 20,
        keep_output: true,
    }
}

fn exec(command: &str) -> Job {
    Job::Exec {
        command: command.into(),
        stdin: Vec::new(),
    }
}

// 连不上为 unreachable；达到失败的上限后不再开始（未执行，错误码 canceled）
#[test]
fn unreachable_hosts_and_the_failure_limit() {
    let store = memory_store();
    let port = dead_port();
    for name in ["a", "b", "c", "d"] {
        host(&store, name, port, true, None, &[]);
    }
    let hosts = resolve(
        &store,
        &Selector {
            all: true,
            ..Default::default()
        },
        Access::Cli,
        Lang::Zh,
    )
    .unwrap()
    .run;
    let (_stop_tx, stop) = watch::channel(false);
    let (results, started) = run_batch(
        store.clone(),
        hosts.clone(),
        exec("true"),
        options(2),
        stop.clone(),
    );
    assert_eq!(started.len(), 4);
    assert!(results.iter().all(|r| r.status == BatchStatus::Unreachable
        && r.error.as_ref().unwrap().error_code() == Some(ErrorCode::ConnectFailed)));
    assert_eq!(
        results.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        ["a", "b", "c", "d"],
        "按主机的顺序"
    );

    let mut limited = options(1);
    limited.max_fail = Some(2);
    let (results, started) = run_batch(store, hosts, exec("true"), limited, stop);
    assert_eq!(started, [0, 1]);
    let statuses: Vec<BatchStatus> = results.iter().map(|r| r.status).collect();
    assert_eq!(
        statuses,
        [
            BatchStatus::Unreachable,
            BatchStatus::Unreachable,
            BatchStatus::Skipped,
            BatchStatus::Skipped
        ]
    );
    assert_eq!(
        results[2].error.as_ref().unwrap().error_code(),
        Some(ErrorCode::Canceled)
    );
}

// 开始前已取消：一台也不开始
#[test]
fn canceled_before_start() {
    let store = memory_store();
    host(&store, "a", dead_port(), true, None, &[]);
    let hosts = store.list::<Host>();
    let (_tx, stop) = watch::channel(true);
    let (results, started) = run_batch(store, hosts, exec("true"), options(4), stop);
    assert!(started.is_empty());
    assert_eq!(results[0].status, BatchStatus::Skipped);
}

/// 连测试容器的两台主机（同一个容器），确认过指纹。
fn server_hosts() -> Option<(Arc<Store>, Vec<Record<Host>>)> {
    let (address, port) = test_server()?;
    let store = memory_store();
    for name in ["s1", "s2"] {
        let id = store
            .create(
                Host {
                    name: name.into(),
                    address: address.clone(),
                    port,
                    username: crate::testkit::SSH_USER.into(),
                    cli: CliAccess {
                        allowed: true,
                        credential: None,
                    },
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
        store
            .set_secret(
                &Owner::host(&id),
                SecretField::Password,
                Some(&SecretString::new(crate::testkit::SSH_PASSWORD)),
            )
            .unwrap();
    }
    let first = store.list::<Host>()[0].id.clone();
    block_on(async {
        let plan = crate::ssh::plan_for_host(&store, &first).unwrap();
        let c = crate::ssh::connect(
            store.clone(),
            &plan,
            accept_host_key(),
            &crate::ssh::ConnectOptions::default(),
        )
        .await
        .unwrap();
        c.disconnect().await;
    });
    let hosts = resolve(
        &store,
        &Selector {
            all: true,
            ..Default::default()
        },
        Access::Cli,
        Lang::Zh,
    )
    .unwrap()
    .run;
    Some((store, hosts))
}

// 连测试容器：退出码、标准输入（写完发 EOF）、超时（SIGTERM，状态 timeout）、执行中取消
#[test]
fn exec_on_the_test_server() {
    let Some((store, hosts)) = server_hosts() else {
        eprintln!("没有设置 TERMLET_TEST_SSH，跳过");
        return;
    };
    let (_tx, stop) = watch::channel(false);
    let (results, _) = run_batch(
        store.clone(),
        hosts.clone(),
        exec("echo hi; exit 3"),
        options(2),
        stop.clone(),
    );
    for r in &results {
        assert_eq!(
            (r.status, r.exit_code, r.stdout.as_str()),
            (BatchStatus::Failed, Some(3), "hi\n")
        );
        assert!(r.error.is_none());
    }
    // 不保留输出（流式）：输出只随事件发出，返回的结果中没有
    let mut streaming = options(2);
    streaming.keep_output = false;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let results = block_on(run(
        store.clone(),
        Access::Cli,
        hosts.clone(),
        exec("echo hi"),
        streaming,
        Lang::Zh,
        stop.clone(),
        tx,
    ));
    assert!(
        results
            .iter()
            .all(|r| r.status == BatchStatus::Ok && r.stdout.is_empty()),
        "{results:?}"
    );
    let mut outputs = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let Event::Finished(_, r) = event {
            outputs.push(r.stdout);
        }
    }
    assert_eq!(outputs, vec!["hi\n"; hosts.len()]);

    let job = Job::Exec {
        command: "wc -c".into(),
        stdin: vec![b'x'; 300_000],
    };
    let (results, _) = run_batch(store.clone(), hosts.clone(), job, options(2), stop.clone());
    assert_eq!(results[0].status, BatchStatus::Ok, "{:?}", results[0]);
    assert_eq!(results[0].stdout.trim(), "300000");

    let mut timed = options(2);
    timed.timeout = Some(Duration::from_millis(500));
    let (results, _) = run_batch(store.clone(), hosts.clone(), exec("sleep 30"), timed, stop);
    assert!(
        results.iter().all(|r| r.status == BatchStatus::Timeout),
        "{results:?}"
    );

    let (stop_tx, stop) = watch::channel(false);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500));
        stop_tx.send_replace(true);
    });
    let (results, _) = run_batch(store, hosts, exec("sleep 30"), options(2), stop);
    assert!(
        results.iter().all(|r| r.status == BatchStatus::Canceled),
        "{results:?}"
    );
}

// 连测试容器：批量上传、下载（每台放进以主机名命名的子目录）
#[test]
fn transfers_on_the_test_server() {
    let Some((store, hosts)) = server_hosts() else {
        eprintln!("没有设置 TERMLET_TEST_SSH，跳过");
        return;
    };
    let work = tempfile::tempdir().unwrap();
    let local = work.path().join("up.txt");
    std::fs::write(&local, "payload").unwrap();
    let remote = format!("/tmp/termlet-core-batch-{}.txt", std::process::id());
    let (_tx, stop) = watch::channel(false);
    let job = Job::Upload {
        local: local.clone(),
        remote: remote.clone(),
    };
    let (results, _) = run_batch(store.clone(), hosts.clone(), job, options(1), stop.clone());
    assert!(
        results
            .iter()
            .all(|r| r.status == BatchStatus::Ok && r.bytes == Some(7)),
        "{results:?}"
    );
    assert_eq!(results[0].remote, remote);
    let dir = work.path().join("got");
    let job = Job::Download {
        remote: remote.clone(),
        dir: dir.clone(),
    };
    let (results, _) = run_batch(store.clone(), hosts.clone(), job, options(2), stop.clone());
    assert!(
        results.iter().all(|r| r.status == BatchStatus::Ok),
        "{results:?}"
    );
    for name in ["s1", "s2"] {
        assert_eq!(
            std::fs::read_to_string(
                dir.join(name)
                    .join(format!("termlet-core-batch-{}.txt", std::process::id()))
            )
            .unwrap(),
            "payload"
        );
    }
    let job = Job::Download {
        remote: "/nope/x".into(),
        dir,
    };
    let (results, _) = run_batch(store.clone(), hosts.clone(), job, options(2), stop.clone());
    assert!(
        results.iter().all(|r| r.status == BatchStatus::Failed),
        "连上之后出错为失败：{results:?}"
    );
    let (results, _) = run_batch(
        store,
        hosts,
        exec(&format!("rm -f {remote}")),
        options(2),
        stop,
    );
    assert!(results.iter().all(|r| r.status == BatchStatus::Ok));
}
