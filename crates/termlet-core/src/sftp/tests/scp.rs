//! SCP 模式的集成测试：
//!
//! - 没有 SFTP 的服务器（testing/nosftp 的临时容器，`TERMLET_TEST_NOSFTP=127.0.0.1:2225`，BusyBox 的 ls）：打开文件管理时
//!   改用 SCP；浏览、上传下载（文件与目录）、取消、在线编辑、改名、删除照常，修改权限不支持；连 scp 也没有的用户
//!   说明原因。
//! - GNU coreutils 的 ls：在 testing/sshd 的容器（`TERMLET_TEST_SSH`）上直接用 SCP 模式的客户端做同样的事。
//!
//! 都没有设置时跳过。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::id::HostId;
use crate::sftp::backend::{Backend, Protocol};
use crate::sftp::error::NoSftp;
use crate::sftp::scp::ScpRemote;
use crate::sftp::{
    ChmodRequest, DownloadRequest, EntryKind, Rejection, SftpError, SshTarget, UploadRequest,
    WriteTextRequest,
};
use crate::ssh::{TerminalSpec, exec};
use crate::store::Store;
use crate::testkit::{SSH_USER, accept_host_key, connect_test, nosftp_store, test_server};

fn target(store: &Arc<Store>, host: &HostId) -> SshTarget {
    SshTarget {
        store: store.clone(),
        host: host.to_string(),
        spec: TerminalSpec::for_host(store, host).unwrap(),
        share_group: None,
        prompter: accept_host_key(),
        owner: "w1".into(),
        sudo: None,
        name_encoding: Encoding::Utf8,
    }
}

fn up(local: &Path, dir: &str, overwrite: bool) -> UploadRequest {
    UploadRequest {
        local_paths: vec![local.to_path_buf()],
        remote_dir: dir.to_string(),
        overwrite,
    }
}

/// 在会话所在的连接上执行命令，返回标准输出。
async fn run(s: &SftpSession, command: &str) -> String {
    let connection = s.connection().unwrap();
    let result = exec(&connection, command, 64 * 1024, Duration::from_secs(20))
        .await
        .unwrap();
    assert_eq!(
        result.exit_code,
        Some(0),
        "{command}：{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8_lossy(&result.stdout).trim().to_string()
}

fn mtime_of(path: &Path) -> i64 {
    std::fs::metadata(path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// 浏览、传输、编辑、改名、删除一遍（`precise_time`：ls 给出的修改时间精确到秒）。
async fn exercise(m: &SftpManager, s: &Arc<SftpSession>, precise_time: bool) {
    assert_eq!(s.protocol(), Protocol::Scp);
    let home = s.home().to_string();
    let dir = format!("{home}/termlet-scp-{}", crate::id::random_token(6));
    s.mkdir(&dir).await.unwrap();
    assert!(s.exists(&dir).await.unwrap());
    assert!(matches!(
        s.mkdir(&dir).await,
        Err(SftpError::AlreadyExists { .. })
    ));

    // 上传文件（名称带空格与中文）与目录
    let local = tempfile::tempdir().unwrap();
    let data = random_bytes(300_000);
    let file = local.path().join("a b 中文.txt");
    std::fs::write(&file, &data).unwrap();
    let tree = local.path().join("folder");
    std::fs::create_dir_all(tree.join("sub")).unwrap();
    std::fs::write(tree.join("sub").join("x.bin"), b"x").unwrap();
    std::fs::write(tree.join("empty"), b"").unwrap();
    let views = m
        .upload(
            s,
            UploadRequest {
                local_paths: vec![file.clone(), tree.clone()],
                remote_dir: dir.clone(),
                overwrite: false,
            },
        )
        .await
        .unwrap();
    for view in &views {
        let v = wait_finished(m, &view.id).await;
        assert!(
            v.state == TransferState::Done && v.done == v.total,
            "上传未完成：{v:?}"
        );
    }
    let listing = s.list(&dir).await.unwrap();
    assert_eq!(listing.path, dir);
    let entry = listing
        .entries
        .iter()
        .find(|e| e.name == "a b 中文.txt")
        .unwrap_or_else(|| panic!("{listing:?}"));
    assert_eq!((entry.kind, entry.size), (EntryKind::File, 300_000));
    if precise_time {
        assert_eq!(entry.modified, mtime_of(&file), "修改时间随文件头设置");
    } else {
        assert!((entry.modified - mtime_of(&file)).abs() < 60);
    }
    let sub = s.list(&format!("{dir}/folder/sub")).await.unwrap();
    assert_eq!(sub.entries.len(), 1);
    assert_eq!(
        (sub.entries[0].name.as_str(), sub.entries[0].size),
        ("x.bin", 1)
    );
    // 没有留下临时文件
    assert!(
        !listing
            .entries
            .iter()
            .any(|e| e.name.ends_with(".termlet-part")),
        "{listing:?}"
    );
    // 不覆盖时同名上传失败；覆盖时沿用原文件的权限
    m.clear(Default::default()).await;
    let v = wait_finished(m, &m.upload(s, up(&file, &dir, false)).await.unwrap()[0].id).await;
    assert!(
        matches!(v.error, Some(SftpError::AlreadyExists { .. })),
        "{v:?}"
    );
    run(s, &format!("chmod 600 '{dir}/a b 中文.txt'")).await;
    let v = wait_finished(m, &m.upload(s, up(&file, &dir, true)).await.unwrap()[0].id).await;
    assert_eq!(v.state, TransferState::Done, "{v:?}");
    assert_eq!(
        s.stat(&format!("{dir}/a b 中文.txt")).await.unwrap().perm,
        0o600
    );

    // 下载文件与目录
    m.clear(Default::default()).await;
    let out = tempfile::tempdir().unwrap();
    let views = m
        .download(
            s,
            DownloadRequest {
                remote_paths: vec![format!("{dir}/a b 中文.txt"), format!("{dir}/folder")],
                local_dir: out.path().to_path_buf(),
            },
        )
        .await
        .unwrap();
    for view in &views {
        let v = wait_finished(m, &view.id).await;
        assert_eq!(v.state, TransferState::Done, "{v:?}");
    }
    assert_eq!(
        std::fs::read(out.path().join("a b 中文.txt")).unwrap(),
        data
    );
    assert_eq!(
        std::fs::read(out.path().join("folder").join("sub").join("x.bin")).unwrap(),
        b"x"
    );
    assert!(out.path().join("folder").join("empty").exists());
    assert!(local_parts(out.path()).is_empty());

    // 取消上传：服务器上不留临时文件
    m.clear(Default::default()).await;
    let big = local.path().join("big.bin");
    std::fs::write(&big, random_bytes(40 << 20)).unwrap();
    let view = m.upload(s, up(&big, &dir, false)).await.unwrap().remove(0);
    wait_view(m, &view.id, |v| v.done > 0 || v.state.is_finished()).await;
    m.cancel(&view.id).unwrap();
    assert_eq!(
        wait_finished(m, &view.id).await.state,
        TransferState::Canceled
    );
    let listing = s.list(&dir).await.unwrap();
    assert!(
        !listing
            .entries
            .iter()
            .any(|e| e.name.starts_with("big.bin")),
        "{listing:?}"
    );

    // 在线编辑：新建、读取、发现别人改过、保存
    let note = format!("{dir}/note.txt");
    let saved = s
        .write_text(&WriteTextRequest {
            path: note.clone(),
            content: "你好\n".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(saved.size, "你好\n".len() as u64);
    assert_eq!(saved.perm & 0o600, 0o600);
    let text = s.read_text(&note, None).await.unwrap();
    assert_eq!(text.content, "你好\n");
    assert!(matches!(
        s.write_text(&WriteTextRequest {
            path: note.clone(),
            content: "x".into(),
            expected_size: Some(1),
            ..Default::default()
        })
        .await,
        Err(SftpError::Changed { deleted: false, .. })
    ));
    s.write_text(&WriteTextRequest {
        path: note.clone(),
        content: "第二版\n".into(),
        expected_modified: Some(text.modified),
        expected_size: Some(text.size),
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(s.read_text(&note, None).await.unwrap().content, "第二版\n");

    // 符号链接：指向目录的按目录显示，悬空的没有目标
    run(
        s,
        &format!("ln -s folder '{dir}/link' && ln -s missing '{dir}/broken'"),
    )
    .await;
    let listing = s.list(&dir).await.unwrap();
    let link = listing.entries.iter().find(|e| e.name == "link").unwrap();
    assert!(link.is_link() && link.is_dir(), "{link:?}");
    let broken = listing.entries.iter().find(|e| e.name == "broken").unwrap();
    assert_eq!((broken.kind, broken.target), (EntryKind::Symlink, None));

    // 改名、修改权限（不支持）、出错的说明、删除
    s.rename(&note, &format!("{dir}/note2.txt")).await.unwrap();
    assert!(s.exists(&format!("{dir}/note2.txt")).await.unwrap());
    assert!(matches!(
        s.rename(&format!("{dir}/note2.txt"), &format!("{dir}/folder"))
            .await,
        Err(SftpError::AlreadyExists { .. })
    ));
    assert_eq!(
        s.chmod(&ChmodRequest {
            paths: vec![dir.clone()],
            mode: 0o700,
            ..Default::default()
        })
        .await,
        Err(SftpError::Rejected(Rejection::ScpUnsupported))
    );
    assert!(matches!(
        s.list("/no/such/dir").await,
        Err(SftpError::NotFound { .. })
    ));
    assert!(matches!(
        s.list("/root").await,
        Err(SftpError::PermissionDenied { .. })
    ));
    assert!(matches!(
        s.list(&format!("{dir}/note2.txt")).await,
        Err(SftpError::Failed { .. })
    ));
    m.clear(Default::default()).await;
    let views = m
        .download(
            s,
            DownloadRequest {
                remote_paths: vec!["/etc/shadow".into()],
                local_dir: out.path().to_path_buf(),
            },
        )
        .await
        .unwrap();
    let v = wait_finished(m, &views[0].id).await;
    assert!(
        matches!(v.error, Some(SftpError::PermissionDenied { .. })),
        "{v:?}"
    );
    assert!(!out.path().join("shadow").exists());
    assert!(local_parts(out.path()).is_empty());
    s.remove(&dir).await.unwrap();
    assert!(!s.exists(&dir).await.unwrap());
    assert!(matches!(
        s.remove(&home).await,
        Err(SftpError::Rejected(Rejection::RemoveRootOrHome))
    ));
}

#[test]
fn nosftp_session_falls_back_to_scp() {
    let Some((store, host)) = nosftp_store("nosftp", SSH_USER) else {
        return;
    };
    block_on(async {
        let m = manager();
        let s = m.open_ssh(target(&store, &host)).await.unwrap();
        assert_eq!(s.info().protocol, Protocol::Scp);
        assert_eq!(s.home(), "/home/termlet");
        exercise(&m, &s, true).await;

        // 连接断开后自动重连，仍是 SCP 模式
        let mut state = s.watch_state();
        s.connection().unwrap().disconnect().await;
        tokio::time::timeout(
            Duration::from_secs(10),
            state.wait_for(|st| matches!(st, crate::sftp::SessionState::Reconnecting { .. })),
        )
        .await
        .expect("断线后应开始重连")
        .unwrap();
        tokio::time::timeout(
            Duration::from_secs(20),
            state.wait_for(|st| *st == crate::sftp::SessionState::Connected),
        )
        .await
        .expect("应重连成功")
        .unwrap();
        assert_eq!(s.protocol(), Protocol::Scp);
        assert!(s.list("~").await.is_ok());
        m.close_all().await;
    });
}

#[test]
fn nosftp_without_scp_explains() {
    let Some((store, host)) = nosftp_store("noscp", "noscp") else {
        return;
    };
    block_on(async {
        let m = manager();
        assert_eq!(
            m.open_ssh(target(&store, &host)).await.err(),
            Some(SftpError::NoSftp(NoSftp::NoScp))
        );
    });
}

/// 命令行工具的单文件传输连上没有 SFTP 的服务器（`user`）。
async fn cli_connection(store: &Arc<Store>, host: &HostId) -> Arc<crate::ssh::Connection> {
    let spec = TerminalSpec::for_host(store, host).unwrap();
    crate::ssh::connect(store.clone(), &spec.plan, accept_host_key(), &spec.connect)
        .await
        .unwrap()
}

/// 在连接上执行命令，返回标准输出（命令必须成功）。
async fn run_on(connection: &crate::ssh::Connection, command: &str) -> String {
    let result = exec(connection, command, 64 * 1024, Duration::from_secs(20))
        .await
        .unwrap();
    assert_eq!(
        result.exit_code,
        Some(0),
        "{command}：{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8_lossy(&result.stdout).trim().to_string()
}

// 命令行工具的上传、下载：服务器没有 SFTP 时改用 SCP，结果中说明；放进已存在的目录、覆盖同名文件、取消后不留
// 临时文件与 SFTP 时相同；“只用一个通道”的主机不改用
#[test]
fn nosftp_single_file_transfer_uses_scp() {
    use crate::sftp::{get_file, put_file};
    use std::future::pending;
    let Some((store, host)) = nosftp_store("nosftp-cli", SSH_USER) else {
        return;
    };
    block_on(async {
        let connection = cli_connection(&store, &host).await;
        let dir = format!("/home/termlet/termlet-cli-{}", crate::id::random_token(6));
        run_on(&connection, &format!("mkdir {dir}")).await;
        let local = tempfile::tempdir().unwrap();
        let file = local.path().join("cli.txt");
        std::fs::write(&file, "from cli").unwrap();

        let sent = put_file(&connection, &file, &format!("{dir}/"), false, pending())
            .await
            .unwrap();
        assert_eq!(
            (sent.path.as_str(), sent.bytes, sent.protocol),
            (format!("{dir}/cli.txt").as_str(), 8, Protocol::Scp)
        );
        // 已存在的目录（不带 /）：放进去并覆盖；相对路径按登录后的目录解析
        std::fs::write(&file, "again").unwrap();
        let name = crate::sftp::path::base_name(&dir);
        let sent = put_file(&connection, &file, &name, false, pending())
            .await
            .unwrap();
        assert_eq!(sent.path, format!("{dir}/cli.txt"));
        assert_eq!(
            run_on(&connection, &format!("cat {dir}/cli.txt")).await,
            "again"
        );
        let renamed = put_file(
            &connection,
            &file,
            &format!("{dir}/b.txt"),
            false,
            pending(),
        )
        .await
        .unwrap();
        assert_eq!(renamed.path, format!("{dir}/b.txt"));

        let out = tempfile::tempdir().unwrap();
        std::fs::write(out.path().join("cli.txt"), "old").unwrap();
        let got = get_file(&connection, &sent.path, out.path(), false, pending())
            .await
            .unwrap();
        assert_eq!(
            (got.path, got.bytes, got.protocol),
            (out.path().join("cli.txt"), 5, Protocol::Scp)
        );
        assert_eq!(
            std::fs::read_to_string(out.path().join("cli.txt")).unwrap(),
            "again"
        );
        assert!(matches!(
            get_file(&connection, &dir, out.path(), false, pending()).await,
            Err(SftpError::Rejected(Rejection::NotRegularFile))
        ));
        assert!(matches!(
            get_file(
                &connection,
                &format!("{dir}/nope"),
                out.path(),
                false,
                pending()
            )
            .await,
            Err(SftpError::NotFound { .. })
        ));

        // 取消（命令行工具被 Ctrl+C）：两边都不留临时文件
        let big = local.path().join("big.bin");
        std::fs::write(&big, vec![7u8; 64 << 20]).unwrap();
        let soon = || tokio::time::sleep(Duration::from_millis(200));
        assert!(matches!(
            put_file(&connection, &big, &format!("{dir}/"), false, soon()).await,
            Err(SftpError::Canceled)
        ));
        assert_eq!(
            run_on(&connection, &format!("ls -A {dir}")).await,
            "b.txt\ncli.txt"
        );
        put_file(&connection, &big, &format!("{dir}/"), false, pending())
            .await
            .unwrap();
        assert!(matches!(
            get_file(
                &connection,
                &format!("{dir}/big.bin"),
                out.path(),
                false,
                soon()
            )
            .await,
            Err(SftpError::Canceled)
        ));
        let mut names: Vec<String> = std::fs::read_dir(out.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["cli.txt"]);

        // 只用一个通道的主机：不改用 SCP
        assert_eq!(
            get_file(&connection, &sent.path, out.path(), true, pending())
                .await
                .err(),
            Some(SftpError::NoSftp(NoSftp::SingleChannel))
        );
        run_on(&connection, &format!("rm -r {dir}")).await;
        connection.disconnect().await;
    });
}

#[test]
fn nosftp_single_file_without_scp_explains() {
    let Some((store, host)) = nosftp_store("noscp-cli", "noscp") else {
        return;
    };
    block_on(async {
        let connection = cli_connection(&store, &host).await;
        let out = tempfile::tempdir().unwrap();
        assert_eq!(
            crate::sftp::get_file(
                &connection,
                "/etc/hostname",
                out.path(),
                false,
                std::future::pending()
            )
            .await
            .err(),
            Some(SftpError::NoSftp(NoSftp::NoScp))
        );
        connection.disconnect().await;
    });
}

/// GNU coreutils 的 ls：在有 SFTP 的测试容器上直接用 SCP 模式。
#[test]
fn docker_scp_mode_with_gnu_ls() {
    let Some((host, port)) = test_server() else {
        return;
    };
    block_on(async {
        let m = manager();
        let connection = connect_test(&host, port).await;
        let remote = ScpRemote::open(connection.clone(), Encoding::Utf8)
            .await
            .unwrap();
        let link = Link {
            backend: Backend::Scp(Arc::new(remote)),
            connection: Some(connection),
            group: None,
        };
        let s = m.0.add("gnu".into(), "w1".into(), link, None).await;
        exercise(&m, &s, true).await;
        m.close_all().await;
    });
}
