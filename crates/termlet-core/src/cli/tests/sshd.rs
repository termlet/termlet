//! 连真实 sshd 的命令行服务测试：设置 `TERMLET_TEST_SSH=127.0.0.1:2222`（testing/sshd 的测试容器，账号 termlet /
//! termlet-e2e）后运行，否则跳过。

use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::ssh::{self, Answer, ConnectOptions};
use crate::testkit::{SSH_PASSWORD, SSH_USER, test_server};

/// 允许命令行访问的测试服务器；`password` 为空时不保存密码。
fn add_server(f: &Fixture, name: &str, password: Option<&str>) -> HostId {
    add_server_at(f, test_server().unwrap(), name, password)
}

fn add_server_at(
    f: &Fixture,
    (address, port): (String, u16),
    name: &str,
    password: Option<&str>,
) -> HostId {
    let id = f
        .store
        .create(
            Host {
                name: name.into(),
                address,
                port,
                username: SSH_USER.into(),
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
    if let Some(password) = password {
        f.store
            .set_secret(
                &Owner::host(&id),
                SecretField::Password,
                Some(&SecretString::new(password)),
            )
            .unwrap();
    }
    id
}

/// 先在"界面"中连接一次并确认指纹（命令行不能询问）。
fn trust(f: &Fixture, host: &HostId) {
    let store = f.store.clone();
    let host = host.clone();
    crate::runtime::runtime().block_on(async move {
        let plan = ssh::plan_for_host(&store, &host).unwrap();
        let prompter: ssh::Prompter = Arc::new(|_| Box::pin(async { Answer::AcceptHostKey }));
        let connection = ssh::connect(store, &plan, prompter, &ConnectOptions::default())
            .await
            .unwrap();
        connection.disconnect().await;
    });
}

fn transfer(f: &Fixture, upload: bool, host: &str, local: &str, remote: &str) -> Response {
    let request = TransferRequest {
        host: host.into(),
        local: local.into(),
        remote: remote.into(),
        caller: vec!["cargo".into()],
    };
    f.call(if upload {
        Request::Upload(request)
    } else {
        Request::Download(request)
    })
}

// exec、上传、下载都记录审计日志
#[test]
fn exec_and_transfer() {
    if test_server().is_none() {
        eprintln!("没有设置 TERMLET_TEST_SSH，跳过");
        return;
    }
    let f = Fixture::new();
    let id = add_server(&f, "e2e", Some(SSH_PASSWORD));

    // 没有确认过指纹：命令行不能询问
    assert_eq!(code_of(f.exec("e2e", "true")), "hostkey_unknown");
    trust(&f, &id);

    let r = exec_ok(f.exec("e2e", "echo hi; echo err >&2; exit 3"));
    assert_eq!(
        (r.exit_code, r.stdout.as_str(), r.stderr.as_str()),
        (3, "hi\n", "err\n")
    );
    assert!(!r.truncated);
    // 被信号结束：128 + 信号编号
    assert_eq!(exec_ok(f.exec("e2e", "kill -TERM $$")).exit_code, 143);
    // 输出上限
    let big = exec_ok(f.exec("e2e", "head -c 1100000 /dev/zero | tr '\\0' x"));
    assert!(big.truncated && big.stdout.len() == OUTPUT_LIMIT);

    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join("up.txt");
    std::fs::write(&local, "payload").unwrap();
    let remote_dir = format!("/tmp/termlet-cli-{}", crate::id::random_token(4));
    exec_ok(f.exec("e2e", &format!("mkdir -p {remote_dir}")));

    // 远程是目录：放入目录
    let up = match transfer(&f, true, "e2e", &local.display().to_string(), &remote_dir) {
        Response::Transfer(t) => t,
        other => panic!("{other:?}"),
    };
    assert_eq!(up.remote, format!("{remote_dir}/up.txt"));
    assert_eq!(up.bytes, 7);
    assert_eq!(up.protocol, "sftp");
    // 本地是目录：放入目录；已存在同名文件时覆盖
    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    std::fs::write(out.join("up.txt"), "old content").unwrap();
    let down = match transfer(
        &f,
        false,
        "e2e",
        &out.display().to_string(),
        &format!("{remote_dir}/up.txt"),
    ) {
        Response::Transfer(t) => t,
        other => panic!("{other:?}"),
    };
    assert_eq!(down.local, out.join("up.txt").display().to_string());
    assert_eq!(
        std::fs::read_to_string(out.join("up.txt")).unwrap(),
        "payload"
    );
    assert_eq!(
        code_of(transfer(
            &f,
            false,
            "e2e",
            &out.display().to_string(),
            &remote_dir
        )),
        "bad_request",
        "下载目录应被拒绝"
    );
    assert_eq!(
        code_of(transfer(
            &f,
            false,
            "e2e",
            &out.display().to_string(),
            "/nope/x"
        )),
        "path_not_found"
    );
    exec_ok(f.exec("e2e", &format!("rm -r {remote_dir}")));

    let list = f.entries();
    let actions: Vec<String> = list
        .iter()
        .rev()
        .map(|e| format!("{}:{}", e.action.as_str(), e.status.as_str()))
        .collect();
    assert_eq!(
        actions,
        [
            "exec:failed",
            "exec:ok",
            "exec:ok",
            "exec:ok",
            "exec:ok",
            "upload:ok",
            "download:ok",
            "download:failed",
            "download:failed",
            "exec:ok"
        ]
    );
    let upload = list
        .iter()
        .find(|e| e.action == AuditAction::Upload)
        .unwrap();
    assert_eq!(upload.bytes, Some(7));
    assert_eq!(upload.caller, "cargo");
    assert!(upload.detail.ends_with(&format!("{remote_dir}/up.txt")));
    assert_eq!(list[list.len() - 2].exit_code, Some(3));
    assert_eq!(list[list.len() - 1].error_code, "hostkey_unknown");
}

// 服务器没有 SFTP（testing/nosftp 的临时容器，`TERMLET_TEST_NOSFTP`）：上传、下载改用 SCP，结果中说明，审计照常；
// “只用一个通道”的主机、没有 scp 的用户说明为什么不能改用
#[test]
fn transfer_falls_back_to_scp() {
    let Some(server) = crate::testkit::nosftp_server() else {
        eprintln!("没有设置 TERMLET_TEST_NOSFTP，跳过");
        return;
    };
    let f = Fixture::new();
    let id = add_server_at(&f, server.clone(), "nosftp", Some(SSH_PASSWORD));
    trust(&f, &id);
    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join("up.txt");
    std::fs::write(&local, "payload").unwrap();
    let remote_dir = format!("/tmp/termlet-cli-scp-{}", crate::id::random_token(4));
    exec_ok(f.exec("nosftp", &format!("mkdir -p {remote_dir}")));

    let up = match transfer(
        &f,
        true,
        "nosftp",
        &local.display().to_string(),
        &remote_dir,
    ) {
        Response::Transfer(t) => t,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        (up.remote.as_str(), up.bytes, up.protocol.as_str()),
        (format!("{remote_dir}/up.txt").as_str(), 7, "scp")
    );
    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    let down = match transfer(&f, false, "nosftp", &out.display().to_string(), &up.remote) {
        Response::Transfer(t) => t,
        other => panic!("{other:?}"),
    };
    assert_eq!(down.protocol, "scp");
    assert_eq!(
        std::fs::read_to_string(out.join("up.txt")).unwrap(),
        "payload"
    );
    assert_eq!(
        code_of(transfer(
            &f,
            false,
            "nosftp",
            &out.display().to_string(),
            &format!("{remote_dir}/nope")
        )),
        "path_not_found"
    );

    // 只用一个通道的主机不改用 SCP
    let mut doc = f.store.get::<Host>(&id).unwrap().doc;
    doc.ssh.single_channel = true;
    f.store.update(&id, doc).unwrap();
    assert_eq!(
        code_of(transfer(
            &f,
            false,
            "nosftp",
            &out.display().to_string(),
            &up.remote
        )),
        "sftp_unavailable"
    );
    exec_ok(f.exec("nosftp", &format!("rm -r {remote_dir}")));

    let actions: Vec<String> = f
        .entries()
        .iter()
        .rev()
        .map(|e| format!("{}:{}", e.action.as_str(), e.status.as_str()))
        .collect();
    assert_eq!(
        actions,
        [
            "exec:ok",
            "upload:ok",
            "download:ok",
            "download:failed",
            "download:failed",
            "exec:ok"
        ]
    );
}

// 凭据：没有保存密码（credential_missing）、密码不对（auth_failed）；CLI 专用凭据与终端用的分开，错了只影响命令行
#[test]
fn credentials() {
    if test_server().is_none() {
        eprintln!("没有设置 TERMLET_TEST_SSH，跳过");
        return;
    }
    let f = Fixture::new();
    let id = add_server(&f, "e2e", Some(SSH_PASSWORD));
    trust(&f, &id);
    let none = add_server(&f, "no-password", None);
    assert_eq!(code_of(f.exec("no-password", "true")), "credential_missing");
    let _ = none;

    let mut doc = f.store.get::<Host>(&id).unwrap().doc;
    doc.cli.credential = Some(CliCredential {
        username: SSH_USER.into(),
        auth: CliAuth::Password,
    });
    f.store.update(&id, doc).unwrap();
    let owner = Owner::host(&id);
    f.store
        .set_secret(
            &owner,
            SecretField::CliPassword,
            Some(&SecretString::new("wrong")),
        )
        .unwrap();
    assert_eq!(code_of(f.exec("e2e", "whoami")), "auth_failed");
    f.store
        .set_secret(
            &owner,
            SecretField::CliPassword,
            Some(&SecretString::new(SSH_PASSWORD)),
        )
        .unwrap();
    assert_eq!(
        exec_ok(f.exec("e2e", "whoami")).stdout,
        format!("{SSH_USER}\n")
    );
}

// 命令行工具在执行中退出：远程命令收到 SIGTERM，审计记为 canceled
#[test]
fn client_exit_terminates_remote_command() {
    if test_server().is_none() {
        eprintln!("没有设置 TERMLET_TEST_SSH，跳过");
        return;
    }
    let f = Fixture::new();
    let id = add_server(&f, "e2e", Some(SSH_PASSWORD));
    trust(&f, &id);
    let marker = format!("/tmp/termlet-cli-cancel-{}", crate::id::random_token(4));
    let mut client = f.client(Lang::Zh);
    client
        .write_raw(
            &termlet_proto::frame::encode(&Request::Exec(ExecRequest {
                host: "e2e".into(),
                command: format!("trap 'echo term > {marker}; exit 1' TERM; sleep 30 & wait"),
                ..Default::default()
            }))
            .unwrap(),
        )
        .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    drop(client);
    wait_until("审计记下取消", || {
        f.entries()
            .first()
            .is_some_and(|e| e.error_code == "canceled")
    });
    std::thread::sleep(Duration::from_millis(500));
    let check = exec_ok(f.exec("e2e", &format!("cat {marker}; rm -f {marker}")));
    assert_eq!(check.stdout, "term\n", "远程命令应收到 SIGTERM");
}
