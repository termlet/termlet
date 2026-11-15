//! 对真实 sshd 的集成测试（传输、续传、文件操作与读取终端的当前目录）：
//! 设置 `TERMLET_TEST_SSH=127.0.0.1:2222`（testing/sshd 的测试容器，账号 termlet / termlet-e2e）后运行，否则跳过。

use std::future::pending;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use russh_sftp::protocol::{FileAttributes, OpenFlags};

use super::*;
use crate::id::HostId;
use crate::sftp::cancel::Cancel;
use crate::sftp::client::{perm_attrs, perm_bits, time_attrs};
use crate::sftp::error::Side;
use crate::sftp::{
    ChmodRequest, DownloadRequest, MAX_EDIT_SIZE, ManagerOptions, Protocol, Rejection,
    ResumeOptions, SessionState, SftpError, SshTarget, TerminalCwd, UploadRequest,
    WriteTextRequest,
};
use crate::ssh::{
    ConnectionPool, ShellOptions, TerminalSession, TerminalSpec, TmuxStatus, connect, open_shell,
};
use crate::store::Store;
use crate::testkit::{SSH_USER, accept_host_key, server_store};

struct Server {
    store: Arc<Store>,
    host: HostId,
}

fn setup() -> Option<Server> {
    let (store, host) = server_store("sftp", SSH_USER)?;
    Some(Server { store, host })
}

impl Server {
    fn target(&self, share_group: Option<String>) -> SshTarget {
        SshTarget {
            store: self.store.clone(),
            host: self.host.to_string(),
            spec: TerminalSpec::for_host(&self.store, &self.host).unwrap(),
            share_group,
            prompter: accept_host_key(),
            owner: "w1".into(),
            sudo: None,
            name_encoding: crate::model::Encoding::Utf8,
        }
    }

    async fn open(&self, m: &SftpManager) -> Arc<SftpSession> {
        m.open_ssh(self.target(None)).await.unwrap()
    }

    /// 打开会话并在远程 /tmp 下建一个本测试专用的目录。
    async fn scratch(&self, m: &SftpManager) -> (Arc<SftpSession>, String) {
        let s = self.open(m).await;
        let dir = format!("/tmp/termlet-sftp-{}", crate::id::random_token(6));
        s.mkdir(&dir).await.unwrap();
        (s, dir)
    }
}

/// 删除测试目录（先恢复权限）。
async fn cleanup(m: &SftpManager, dir: &str) {
    if let Some(info) = m
        .sessions(None)
        .into_iter()
        .find(|s| s.state == SessionState::Connected)
        && let Some(s) = m.session(&info.id)
    {
        let _ = s
            .chmod(&ChmodRequest {
                paths: vec![dir.into()],
                mode: 0o755,
                recursive: true,
                scope: crate::sftp::ChmodScope::DirsOnly,
            })
            .await;
        let _ = s.remove(dir).await;
    }
    m.close_all().await;
}

async fn put_remote(s: &SftpSession, p: &str, data: &[u8]) {
    let client = s.client();
    let handle = client
        .open(
            p,
            OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
            FileAttributes::empty(),
        )
        .await
        .unwrap();
    let mut reader = data;
    let progress = AtomicU64::new(0);
    client
        .write_from(&handle, 0, &mut reader, &Cancel::new(), &progress)
        .await
        .ok()
        .unwrap();
    client.close_handle(&handle).await.unwrap();
}

async fn get_remote(s: &SftpSession, p: &str) -> Vec<u8> {
    let client = s.client();
    let handle = client
        .open(p, OpenFlags::READ, FileAttributes::empty())
        .await
        .unwrap();
    let mut buf = Vec::new();
    let progress = AtomicU64::new(0);
    client
        .read_into(&handle, 0, &mut buf, &Cancel::new(), &progress, None)
        .await
        .ok()
        .unwrap();
    client.close_handle(&handle).await.unwrap();
    buf
}

async fn perm_of(s: &SftpSession, p: &str) -> u32 {
    perm_bits(&s.client().stat(p).await.unwrap())
}

async fn chmod_remote(s: &SftpSession, p: &str, mode: u32) {
    s.client().setstat(p, perm_attrs(mode)).await.unwrap();
}

/// 等到任务已传的字节数超过 `n`（或已结束）。
async fn wait_done(m: &SftpManager, id: &TransferId, n: u64) {
    wait_view(m, id, |v| v.done > n || v.state.is_finished()).await;
}

fn up(local: &Path, dir: &str, overwrite: bool) -> UploadRequest {
    UploadRequest {
        local_paths: vec![local.to_path_buf()],
        remote_dir: dir.into(),
        overwrite,
    }
}

#[test]
fn directory_operations() {
    let Some(server) = setup() else {
        return;
    };
    block_on(async {
        let m = manager();
        let (s, dir) = server.scratch(&m).await;
        let home = s.list("").await.unwrap();
        assert_eq!(home.path, "/home/termlet");
        assert_eq!(s.home(), "/home/termlet");
        assert!(matches!(
            s.mkdir(&format!("{dir}/a/b")).await,
            Err(SftpError::NotFound {
                side: Side::Remote,
                ..
            })
        ));
        for p in ["a", "a/b"] {
            s.mkdir(&format!("{dir}/{p}")).await.unwrap();
        }
        assert!(matches!(
            s.mkdir(&format!("{dir}/a")).await,
            Err(SftpError::AlreadyExists { .. })
        ));
        s.rename(&format!("{dir}/a"), &format!("{dir}/c"))
            .await
            .unwrap();
        let list = s.list(&dir).await.unwrap();
        assert!(
            list.entries.len() == 1 && list.entries[0].name == "c" && list.entries[0].is_dir(),
            "{list:?}"
        );
        // OpenSSH 的列表带属主、属组的名称
        assert_eq!(list.entries[0].owner.as_deref(), Some("termlet"));
        assert!(s.exists(&format!("{dir}/c")).await.unwrap());
        s.remove(&format!("{dir}/c")).await.unwrap();
        assert!(matches!(
            s.list(&format!("{dir}/c")).await,
            Err(SftpError::NotFound { .. })
        ));
        assert_eq!(
            s.remove("/").await,
            Err(SftpError::Rejected(Rejection::RemoveRootOrHome))
        );
        assert_eq!(
            s.remove("~").await,
            Err(SftpError::Rejected(Rejection::RemoveRootOrHome))
        );
        assert!(matches!(
            s.list("/root").await,
            Err(SftpError::PermissionDenied { .. })
        ));
        cleanup(&m, &dir).await;
    });
}

#[test]
fn upload_download_round_trip() {
    let Some(server) = setup() else {
        return;
    };
    block_on(async {
        let m = manager();
        let (s, dir) = server.scratch(&m).await;
        let local = tempfile::tempdir().unwrap();
        let big = random_bytes((3 << 20) + 123);
        let big_path = local.path().join("big.bin");
        std::fs::write(&big_path, &big).unwrap();
        let tree = local.path().join("tree");
        std::fs::create_dir_all(tree.join("sub")).unwrap();
        std::fs::write(tree.join("a.txt"), "hello").unwrap();
        std::fs::write(tree.join("sub").join("b.txt"), "world").unwrap();

        let views = m
            .upload(
                &s,
                UploadRequest {
                    local_paths: vec![big_path.clone(), tree.clone()],
                    remote_dir: dir.clone(),
                    overwrite: false,
                },
            )
            .await
            .unwrap();
        for view in &views {
            let v = wait_finished(&m, &view.id).await;
            assert!(
                v.state == TransferState::Done && v.done == v.total,
                "上传未完成：{v:?}"
            );
            if v.is_dir {
                assert_eq!((v.files, v.files_done, v.total), (2, 2, 10), "{v:?}");
            }
        }
        // 不覆盖时，同名上传失败
        m.clear(Default::default()).await;
        let views = m.upload(&s, up(&big_path, &dir, false)).await.unwrap();
        let v = wait_finished(&m, &views[0].id).await;
        assert!(
            v.state == TransferState::Failed
                && matches!(v.error, Some(SftpError::AlreadyExists { .. })),
            "{v:?}"
        );

        // 下载到另一个本机目录；再下载一次时自动改名
        m.clear(Default::default()).await;
        let out = tempfile::tempdir().unwrap();
        let views = m
            .download(
                &s,
                DownloadRequest {
                    remote_paths: vec![format!("{dir}/big.bin"), format!("{dir}/tree")],
                    local_dir: out.path().to_path_buf(),
                },
            )
            .await
            .unwrap();
        for view in &views {
            assert_eq!(wait_finished(&m, &view.id).await.state, TransferState::Done);
        }
        let views = m
            .download(
                &s,
                DownloadRequest {
                    remote_paths: vec![format!("{dir}/big.bin")],
                    local_dir: out.path().to_path_buf(),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            wait_finished(&m, &views[0].id).await.state,
            TransferState::Done
        );
        for name in ["big.bin", "big (1).bin"] {
            assert_eq!(std::fs::read(out.path().join(name)).unwrap(), big, "{name}");
        }
        assert_eq!(
            std::fs::read_to_string(out.path().join("tree").join("sub").join("b.txt")).unwrap(),
            "world"
        );
        assert!(local_parts(out.path()).is_empty());
        cleanup(&m, &dir).await;
    });
}

#[test]
fn cancel_queued_and_close_owner() {
    let Some(server) = setup() else {
        return;
    };
    block_on(async {
        let m = SftpManager::new(ManagerOptions {
            per_session: 1,
            ..options()
        });
        let (s, dir) = server.scratch(&m).await;
        let local = tempfile::tempdir().unwrap();
        let paths: Vec<_> = (0..3)
            .map(|i| {
                let p = local.path().join(format!("f{i}"));
                std::fs::write(&p, vec![0u8; 8 << 20]).unwrap();
                p
            })
            .collect();
        let views = m
            .upload(
                &s,
                UploadRequest {
                    local_paths: paths,
                    remote_dir: dir.clone(),
                    overwrite: false,
                },
            )
            .await
            .unwrap();
        // 同时只能传一个：取消最后一个（仍在排队）
        m.cancel(&views[2].id).unwrap();
        let mut states = Vec::new();
        for view in &views {
            states.push(wait_finished(&m, &view.id).await.state);
        }
        assert_eq!(states[0], TransferState::Done);
        assert_eq!(states[2], TransferState::Canceled);
        assert!(
            !s.exists(&format!("{dir}/f2")).await.unwrap(),
            "已取消的任务不应创建远程文件"
        );
        let _ = s.remove(&dir).await;
        assert_eq!(m.close_owner(Some("w1")).await, 1);
        assert!(m.session(s.id()).is_none());
        assert_eq!(s.state(), SessionState::Closed { error: None });
        m.close_all().await;
    });
}

// 连接中途断开：传输失败、保留临时文件；重新打开会话后在新会话上继续，从断点接着传，内容完整。
#[test]
fn resume_after_connection_lost() {
    let Some(server) = setup() else {
        return;
    };
    block_on(async {
        // 这里测手动继续：关闭客户端模拟断开后不让会话自己重连
        let m = SftpManager::new(ManagerOptions {
            reconnect_base: Duration::from_secs(3600),
            ..options()
        });
        let (s, dir) = server.scratch(&m).await;
        let data = random_bytes(24 << 20);
        let local = tempfile::tempdir().unwrap();
        let big = local.path().join("big.bin");
        std::fs::write(&big, &data).unwrap();
        let views = m.upload(&s, up(&big, &dir, false)).await.unwrap();
        wait_done(&m, &views[0].id, 4 << 20).await;
        s.client().close();
        let failed = wait_view(&m, &views[0].id, |v| {
            !matches!(v.state, TransferState::Queued | TransferState::Running)
        })
        .await;
        assert!(
            matches!(
                failed.state,
                TransferState::Failed | TransferState::WaitingReconnect
            ) && failed.resumable
                && failed.done < data.len() as u64,
            "断开后应失败并可继续：{failed:?}"
        );

        // 重新打开会话，在新会话上继续：只传剩下的部分
        let s2 = server.open(&m).await;
        let part = format!("{dir}/big.bin.termlet-part");
        assert!(
            s2.exists(&part).await.unwrap(),
            "失败时应保留远程的临时文件"
        );
        m.resume(
            &failed.id,
            ResumeOptions {
                session: Some(s2.id().clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let v = wait_finished(&m, &failed.id).await;
        assert!(
            v.state == TransferState::Done && v.done == data.len() as u64 && v.session == *s2.id(),
            "继续上传未完成：{v:?}"
        );
        assert_eq!(get_remote(&s2, &format!("{dir}/big.bin")).await, data);
        assert!(!s2.exists(&part).await.unwrap(), "完成后不应残留临时文件");

        // 下载：同样中途断开后继续
        m.clear(Default::default()).await;
        let out = tempfile::tempdir().unwrap();
        let views = m
            .download(
                &s2,
                DownloadRequest {
                    remote_paths: vec![format!("{dir}/big.bin")],
                    local_dir: out.path().to_path_buf(),
                },
            )
            .await
            .unwrap();
        wait_done(&m, &views[0].id, 4 << 20).await;
        s2.client().close();
        let failed = wait_view(&m, &views[0].id, |v| {
            !matches!(v.state, TransferState::Queued | TransferState::Running)
        })
        .await;
        assert!(failed.resumable, "下载断开后应失败并可继续：{failed:?}");
        let part = out.path().join("big.bin.termlet-part");
        assert!(
            std::fs::metadata(&part).unwrap().len() > 0,
            "失败时应保留本机的临时文件"
        );
        let s3 = server.open(&m).await;
        m.resume(
            &failed.id,
            ResumeOptions {
                session: Some(s3.id().clone()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            wait_finished(&m, &failed.id).await.state,
            TransferState::Done
        );
        assert_eq!(std::fs::read(out.path().join("big.bin")).unwrap(), data);
        assert_eq!(
            local_names(out.path()),
            ["big.bin"],
            "写到第一次选定的位置，不改名、不留临时文件"
        );
        cleanup(&m, &dir).await;
    });
}

// 目录继续传输：已经传完（大小与修改时间相同）的文件跳过；只能继续失败的传输，会话须存在。
#[test]
fn resume_directory_skips_finished() {
    let Some(server) = setup() else {
        return;
    };
    block_on(async {
        let m = manager();
        let (s, dir) = server.scratch(&m).await;
        let local = tempfile::tempdir().unwrap();
        let tree = local.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(tree.join(name), format!("content-{name}")).unwrap();
        }
        // 让 b.txt 的位置被一个目录占住：写完临时文件后改名失败（临时文件的位置被占住时会改用别的名称，不会失败）
        let remote_tree = format!("{dir}/tree");
        s.mkdir(&remote_tree).await.unwrap();
        s.mkdir(&format!("{remote_tree}/b.txt")).await.unwrap();
        let views = m.upload(&s, up(&tree, &dir, true)).await.unwrap();
        let failed = wait_finished(&m, &views[0].id).await;
        assert!(
            failed.state == TransferState::Failed && failed.files_done == 1,
            "应在 b.txt 失败：{failed:?}"
        );
        assert_eq!(
            m.resume(
                &failed.id,
                ResumeOptions {
                    session: Some(crate::sftp::session::SessionId::new()),
                    ..Default::default()
                }
            ),
            Err(SftpError::SessionClosed)
        );

        // 改掉已传完的 a.txt 的内容（大小与修改时间不变）：继续时跳过它，内容保持改过的
        let a = format!("{remote_tree}/a.txt");
        let st = s.client().stat(&a).await.unwrap();
        put_remote(&s, &a, b"CONTENT-a.txt").await;
        s.client()
            .setstat(&a, time_attrs(st.mtime.unwrap() as i64))
            .await
            .unwrap();
        s.client()
            .rmdir(&format!("{remote_tree}/b.txt"))
            .await
            .unwrap();
        m.resume(&failed.id, ResumeOptions::default()).unwrap();
        let v = wait_finished(&m, &failed.id).await;
        assert!(
            v.state == TransferState::Done && v.files_done == 3 && v.done == v.total,
            "继续上传目录未完成：{v:?}"
        );
        assert_eq!(get_remote(&s, &a).await, b"CONTENT-a.txt");
        assert_eq!(
            get_remote(&s, &format!("{remote_tree}/c.txt")).await,
            b"content-c.txt"
        );
        assert_eq!(
            m.resume(&failed.id, ResumeOptions::default()),
            Err(SftpError::Rejected(Rejection::NotResumable))
        );
        cleanup(&m, &dir).await;
    });
}

// 覆盖上传：先写临时文件再改名，沿用原文件的权限；目标是符号链接时写到它指向的文件，链接保留。取消时删除临时文件。
#[test]
fn upload_overwrite_keeps_mode_and_link() {
    let Some(server) = setup() else {
        return;
    };
    block_on(async {
        let m = manager();
        let (s, dir) = server.scratch(&m).await;
        let target = format!("{dir}/conf.txt");
        put_remote(&s, &target, b"old").await;
        chmod_remote(&s, &target, 0o640).await;
        s.client()
            .symlink(&target, &format!("{dir}/link.txt"))
            .await
            .unwrap();
        let local = tempfile::tempdir().unwrap();
        std::fs::write(local.path().join("conf.txt"), "new").unwrap();
        std::fs::write(local.path().join("link.txt"), "via link").unwrap();
        let views = m
            .upload(&s, up(&local.path().join("conf.txt"), &dir, true))
            .await
            .unwrap();
        wait_finished(&m, &views[0].id).await;
        assert_eq!(perm_of(&s, &target).await, 0o640, "覆盖后应沿用原来的权限");
        assert_eq!(get_remote(&s, &target).await, b"new");

        let views = m
            .upload(&s, up(&local.path().join("link.txt"), &dir, true))
            .await
            .unwrap();
        wait_finished(&m, &views[0].id).await;
        let link = s.stat(&format!("{dir}/link.txt")).await.unwrap();
        assert!(link.is_link(), "符号链接应保留：{link:?}");
        assert_eq!(get_remote(&s, &target).await, b"via link");

        // 取消：不留临时文件
        m.clear(Default::default()).await;
        let big = local.path().join("big.bin");
        std::fs::write(&big, vec![0u8; 32 << 20]).unwrap();
        let views = m.upload(&s, up(&big, &dir, false)).await.unwrap();
        wait_done(&m, &views[0].id, 1 << 20).await;
        m.cancel(&views[0].id).unwrap();
        let v = wait_finished(&m, &views[0].id).await;
        assert!(
            v.state == TransferState::Canceled && !v.resumable,
            "应已取消：{v:?}"
        );
        assert!(
            !s.exists(&format!("{dir}/big.bin.termlet-part"))
                .await
                .unwrap(),
            "取消后不应留下临时文件"
        );
        cleanup(&m, &dir).await;
    });
}

// 修改权限：单个文件；递归时目录在有读权限的位置加上执行权限；只改目录、只改文件。
#[test]
fn chmod() {
    let Some(server) = setup() else {
        return;
    };
    block_on(async {
        let m = manager();
        let (s, dir) = server.scratch(&m).await;
        let f = format!("{dir}/f.txt");
        put_remote(&s, &f, b"x").await;
        s.chmod(&ChmodRequest {
            paths: vec![f.clone()],
            mode: 0o4750,
            ..Default::default()
        })
        .await
        .unwrap();
        let list = s.list(&dir).await.unwrap();
        assert_eq!(list.entries[0].perm, 0o4750);
        assert_eq!(list.entries[0].mode, "-rwsr-x---");

        let sub = format!("{dir}/d");
        s.mkdir(&sub).await.unwrap();
        s.mkdir(&format!("{sub}/inner")).await.unwrap();
        let g = format!("{sub}/inner/g.txt");
        put_remote(&s, &g, b"y").await;
        s.chmod(&ChmodRequest {
            paths: vec![sub.clone()],
            mode: 0o640,
            recursive: true,
            ..Default::default()
        })
        .await
        .unwrap();
        for (p, want) in [
            (sub.clone(), 0o750),
            (format!("{sub}/inner"), 0o750),
            (g.clone(), 0o640),
        ] {
            assert_eq!(perm_of(&s, &p).await, want, "{p}");
        }
        // 只改文件、只改目录
        s.chmod(&ChmodRequest {
            paths: vec![sub.clone()],
            mode: 0o600,
            recursive: true,
            scope: crate::sftp::ChmodScope::FilesOnly,
        })
        .await
        .unwrap();
        assert_eq!(
            (perm_of(&s, &sub).await, perm_of(&s, &g).await),
            (0o750, 0o600)
        );
        s.chmod(&ChmodRequest {
            paths: vec![sub.clone()],
            mode: 0o711,
            recursive: true,
            scope: crate::sftp::ChmodScope::DirsOnly,
        })
        .await
        .unwrap();
        assert_eq!(
            (
                perm_of(&s, &format!("{sub}/inner")).await,
                perm_of(&s, &g).await
            ),
            (0o711, 0o600)
        );
        assert_eq!(
            s.chmod(&ChmodRequest {
                paths: vec![f.clone()],
                mode: 0o10000,
                ..Default::default()
            })
            .await,
            Err(SftpError::Rejected(Rejection::InvalidMode))
        );
        assert!(matches!(
            s.chmod(&ChmodRequest {
                paths: vec![format!("{dir}/none")],
                mode: 0o644,
                ..Default::default()
            })
            .await,
            Err(SftpError::NotFound { .. })
        ));
        cleanup(&m, &dir).await;
    });
}

// 连接断开后自动重连：会话 ID 不变，期间失败的传输重连后从中断处继续，最后内容完整。
#[test]
fn auto_reconnect_resumes() {
    let Some(server) = setup() else {
        return;
    };
    block_on(async {
        let m = SftpManager::new(ManagerOptions {
            reconnect_base: Duration::from_millis(50),
            ..options()
        });
        let (s, dir) = server.scratch(&m).await;
        let data = random_bytes(24 << 20);
        let local = tempfile::tempdir().unwrap();
        let big = local.path().join("big.bin");
        std::fs::write(&big, &data).unwrap();
        let views = m.upload(&s, up(&big, &dir, false)).await.unwrap();
        wait_done(&m, &views[0].id, 4 << 20).await;
        // 断开 SSH 连接
        s.connection().unwrap().disconnect().await;
        let v = wait_finished(&m, &views[0].id).await;
        assert!(
            v.state == TransferState::Done && v.session == *s.id(),
            "重连后应继续传完：{v:?}"
        );
        assert!(m.session(s.id()).is_some(), "重连后会话 ID 不变");
        assert_eq!(s.state(), SessionState::Connected);
        assert_eq!(get_remote(&s, &format!("{dir}/big.bin")).await, data);
        cleanup(&m, &dir).await;
    });
}

// 只有服务器上的 SFTP 子系统退出（SSH 连接还在）：同样自动重连，会话 ID 不变，仍用原来的连接。
#[test]
fn reconnect_after_subsystem_exit() {
    let Some(server) = setup() else {
        return;
    };
    block_on(async {
        let m = SftpManager::new(ManagerOptions {
            reconnect_base: Duration::from_millis(50),
            ..options()
        });
        let (s, dir) = server.scratch(&m).await;
        let connection = s.connection().unwrap();
        s.client().close();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if s.state() == SessionState::Connected && s.list(&dir).await.is_ok() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "子系统退出后应自动重连"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            Arc::ptr_eq(&connection, &s.connection().unwrap()),
            "应仍用原来的连接"
        );
        cleanup(&m, &dir).await;
    });
}

// 远程根路径是指向目录的符号链接：下载它指向的目录，递归修改权限时修改其中的文件与子目录；子项中的链接不跟随。
#[test]
fn linked_remote_directory_sshd() {
    let Some(server) = setup() else {
        return;
    };
    block_on(async {
        let m = manager();
        let (s, dir) = server.scratch(&m).await;
        let real = format!("{dir}/realDir");
        s.mkdir(&real).await.unwrap();
        s.mkdir(&format!("{real}/sub")).await.unwrap();
        put_remote(&s, &format!("{real}/a.txt"), b"a").await;
        put_remote(&s, &format!("{real}/sub/b.txt"), b"b").await;
        let link = format!("{dir}/linked");
        s.client().symlink(&real, &link).await.unwrap();
        s.client()
            .symlink(&real, &format!("{real}/loop"))
            .await
            .unwrap();
        let out = tempfile::tempdir().unwrap();
        let views = m
            .download(
                &s,
                DownloadRequest {
                    remote_paths: vec![link.clone()],
                    local_dir: out.path().to_path_buf(),
                },
            )
            .await
            .unwrap();
        let v = wait_finished(&m, &views[0].id).await;
        assert!(
            v.state == TransferState::Done && (v.files, v.files_done) == (2, 2),
            "{v:?}"
        );
        assert_eq!(
            std::fs::read(out.path().join("linked/a.txt")).unwrap(),
            b"a"
        );
        assert_eq!(
            std::fs::read(out.path().join("linked/sub/b.txt")).unwrap(),
            b"b"
        );
        assert!(std::fs::symlink_metadata(out.path().join("linked/loop")).is_err());
        s.chmod(&ChmodRequest {
            paths: vec![link],
            mode: 0o600,
            recursive: true,
            ..Default::default()
        })
        .await
        .unwrap();
        for (p, want) in [
            (real.clone(), 0o700),
            (format!("{real}/a.txt"), 0o600),
            (format!("{real}/sub"), 0o700),
            (format!("{real}/sub/b.txt"), 0o600),
        ] {
            assert_eq!(perm_of(&s, &p).await, want, "{p}");
        }
        cleanup(&m, &dir).await;
    });
}

async fn no_temp_files(s: &SftpSession, dir: &str) {
    for e in s.list(dir).await.unwrap().entries {
        assert!(
            !e.name.contains(".termlet-"),
            "不应残留临时文件：{}",
            e.name
        );
    }
}

#[test]
fn read_write_text_file() {
    let Some(server) = setup() else {
        return;
    };
    block_on(async {
        let m = manager();
        let (s, dir) = server.scratch(&m).await;
        let p = format!("{dir}/新建.conf");
        // 新建：权限为 0644
        let res = s
            .write_text(&WriteTextRequest {
                path: p.clone(),
                content: "a = 1\n中文\n".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            res.path == p && res.size == "a = 1\n中文\n".len() as u64 && res.mode == "-rw-r--r--",
            "{res:?}"
        );
        let f = s.read_text(&p, None).await.unwrap();
        assert!(
            f.content == "a = 1\n中文\n"
                && f.size == res.size
                && f.modified == res.modified
                && f.mode == "-rw-r--r--",
            "{f:?}"
        );
        // 带上读取时的修改时间与大小保存：没有冲突
        let res = s
            .write_text(&WriteTextRequest {
                path: p.clone(),
                content: "a = 2\r\nb = 3\r\n".into(),
                expected_modified: Some(f.modified),
                expected_size: Some(f.size),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(get_remote(&s, &p).await, b"a = 2\r\nb = 3\r\n");
        // 再次保存（基于上次保存返回的信息）
        s.write_text(&WriteTextRequest {
            path: p.clone(),
            content: String::new(),
            expected_modified: Some(res.modified),
            expected_size: Some(res.size),
            ..Default::default()
        })
        .await
        .unwrap();
        let f = s.read_text(&p, None).await.unwrap();
        assert!(f.content.is_empty() && f.size == 0);
        no_temp_files(&s, &dir).await;

        // 相对路径与 ~ 按初始目录解析
        assert!(matches!(
            s.read_text("~/不存在的文件", None).await,
            Err(SftpError::NotFound { .. })
        ));
        assert_eq!(
            s.read_text(&dir, None).await,
            Err(SftpError::Rejected(Rejection::NotRegularFile))
        );
        assert_eq!(
            s.write_text(&WriteTextRequest {
                path: dir.clone(),
                content: "x".into(),
                ..Default::default()
            })
            .await,
            Err(SftpError::Rejected(Rejection::NotRegularFile))
        );
        assert!(matches!(
            s.write_text(&WriteTextRequest {
                path: "/root/x.txt".into(),
                content: "x".into(),
                ..Default::default()
            })
            .await,
            Err(SftpError::PermissionDenied { .. })
        ));
        // GB18030：以所选编码读写
        let gbk = format!("{dir}/gbk.txt");
        put_remote(&s, &gbk, &[0xd6, 0xd0, 0xce, 0xc4]).await;
        assert!(matches!(
            s.read_text(&gbk, None).await,
            Err(SftpError::NotText { .. })
        ));
        let f = s
            .read_text(&gbk, Some(crate::model::Encoding::Gb18030))
            .await
            .unwrap();
        assert_eq!(f.content, "中文");
        s.write_text(&WriteTextRequest {
            path: gbk.clone(),
            content: "中文！".into(),
            encoding: Some(crate::model::Encoding::Gb18030),
            ..Default::default()
        })
        .await
        .unwrap();
        assert_eq!(
            get_remote(&s, &gbk).await,
            [0xd6, 0xd0, 0xce, 0xc4, 0xa3, 0xa1]
        );
        cleanup(&m, &dir).await;
    });
}

#[test]
fn read_file_limits() {
    let Some(server) = setup() else {
        return;
    };
    block_on(async {
        let m = manager();
        let (s, dir) = server.scratch(&m).await;
        // 恰好 5MB 可以打开；超过 1 字节返回超过上限
        let exact = b"0123456789abcde\n".repeat((MAX_EDIT_SIZE / 16) as usize);
        put_remote(&s, &format!("{dir}/exact.txt"), &exact).await;
        let f = s
            .read_text(&format!("{dir}/exact.txt"), None)
            .await
            .unwrap();
        assert_eq!(f.size, MAX_EDIT_SIZE);
        let mut big = exact.clone();
        big.push(b'x');
        put_remote(&s, &format!("{dir}/big.txt"), &big).await;
        assert!(matches!(
            s.read_text(&format!("{dir}/big.txt"), None).await,
            Err(SftpError::TooLarge { .. })
        ));
        assert!(matches!(
            s.write_text(&WriteTextRequest {
                path: format!("{dir}/new.txt"),
                content: String::from_utf8(big).unwrap(),
                ..Default::default()
            })
            .await,
            Err(SftpError::TooLarge { .. })
        ));
        // 含 NUL 或不是合法 UTF-8：不是文本；空文件可以打开
        for (name, data) in [
            ("nul.bin", &b"abc\x00def"[..]),
            ("latin1.txt", b"caf\xe9"),
            ("gbk.txt", &[0xd6, 0xd0, 0xce, 0xc4]),
        ] {
            put_remote(&s, &format!("{dir}/{name}"), data).await;
            assert!(
                matches!(
                    s.read_text(&format!("{dir}/{name}"), None).await,
                    Err(SftpError::NotText { .. })
                ),
                "{name}"
            );
        }
        put_remote(&s, &format!("{dir}/empty"), b"").await;
        assert!(
            s.read_text(&format!("{dir}/empty"), None)
                .await
                .unwrap()
                .content
                .is_empty()
        );
        cleanup(&m, &dir).await;
    });
}

#[test]
fn write_file_conflict() {
    let Some(server) = setup() else {
        return;
    };
    block_on(async {
        let m = manager();
        let (s, dir) = server.scratch(&m).await;
        let p = format!("{dir}/app.yaml");
        put_remote(&s, &p, b"v: 1\n").await;
        let f = s.read_text(&p, None).await.unwrap();
        let opened = WriteTextRequest {
            path: p.clone(),
            content: "v: 2\n".into(),
            expected_modified: Some(f.modified),
            expected_size: Some(f.size),
            ..Default::default()
        };
        // 其他人修改了文件（修改时间不同）：冲突，内容不被覆盖
        put_remote(&s, &p, b"v: 9\n").await;
        let later = f.modified + 60;
        s.client().setstat(&p, time_attrs(later)).await.unwrap();
        assert_eq!(
            s.write_text(&opened).await,
            Err(SftpError::Changed {
                path: p.clone(),
                deleted: false
            })
        );
        assert_eq!(get_remote(&s, &p).await, b"v: 9\n");
        // 同一秒内被修改（修改时间相同、大小不同）也能发现
        put_remote(&s, &p, b"v: 10\n").await;
        s.client().setstat(&p, time_attrs(later)).await.unwrap();
        let same_time = WriteTextRequest {
            expected_modified: Some(later),
            ..opened.clone()
        };
        assert!(matches!(
            s.write_text(&same_time).await,
            Err(SftpError::Changed { .. })
        ));
        // 强制覆盖
        let forced = WriteTextRequest {
            force: true,
            ..opened.clone()
        };
        s.write_text(&forced).await.unwrap();
        assert_eq!(get_remote(&s, &p).await, b"v: 2\n");
        // 打开后被删除：冲突；强制保存时重新创建
        s.client().remove(&p).await.unwrap();
        assert_eq!(
            s.write_text(&opened).await,
            Err(SftpError::Changed {
                path: p.clone(),
                deleted: true
            })
        );
        s.write_text(&forced).await.unwrap();
        assert_eq!(get_remote(&s, &p).await, b"v: 2\n");
        no_temp_files(&s, &dir).await;
        cleanup(&m, &dir).await;
    });
}

#[test]
fn write_file_keeps_mode_and_links() {
    let Some(server) = setup() else {
        return;
    };
    block_on(async {
        let m = manager();
        let (s, dir) = server.scratch(&m).await;
        // 保留权限：可执行脚本、只有自己可读写的文件、只读的文件
        for (name, mode) in [
            ("run.sh", 0o755),
            ("secret.env", 0o600),
            ("readonly.txt", 0o444),
        ] {
            let p = format!("{dir}/{name}");
            put_remote(&s, &p, b"old\n").await;
            chmod_remote(&s, &p, mode).await;
            let res = s
                .write_text(&WriteTextRequest {
                    path: p.clone(),
                    content: "new\n".into(),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(perm_of(&s, &p).await, mode, "{name}");
            assert_eq!(res.perm, mode);
            assert_eq!(get_remote(&s, &p).await, b"new\n");
        }
        // 符号链接：写入指向的文件，链接本身保持不变
        let target = format!("{dir}/real.conf");
        let link = format!("{dir}/link.conf");
        put_remote(&s, &target, b"x\n").await;
        chmod_remote(&s, &target, 0o640).await;
        s.client().symlink(&target, &link).await.unwrap();
        s.write_text(&WriteTextRequest {
            path: link.clone(),
            content: "y\n".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        assert!(
            s.stat(&link).await.unwrap().is_link(),
            "保存后链接应仍是符号链接"
        );
        assert_eq!(get_remote(&s, &target).await, b"y\n");
        assert_eq!(perm_of(&s, &target).await, 0o640);
        // 目录不可写（无法创建临时文件）但文件可写：直接覆盖写入
        let locked = format!("{dir}/locked");
        s.mkdir(&locked).await.unwrap();
        let p = format!("{locked}/conf.ini");
        put_remote(&s, &p, b"[a]\n").await;
        chmod_remote(&s, &locked, 0o555).await;
        s.write_text(&WriteTextRequest {
            path: p.clone(),
            content: "[b]\nk=v\n".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        assert_eq!(get_remote(&s, &p).await, b"[b]\nk=v\n");
        // 目录不可写时不能新建文件
        assert!(matches!(
            s.write_text(&WriteTextRequest {
                path: format!("{locked}/new.txt"),
                content: "x".into(),
                ..Default::default()
            })
            .await,
            Err(SftpError::PermissionDenied { .. })
        ));
        no_temp_files(&s, &dir).await;
        no_temp_files(&s, &locked).await;
        chmod_remote(&s, &locked, 0o755).await;
        cleanup(&m, &dir).await;
    });
}

// 从终端打开：共用终端的连接（同一个连接组）；"只用一个通道"的主机不能共用。
#[test]
fn share_terminal_connection() {
    let Some(server) = setup() else {
        return;
    };
    block_on(async {
        let m = manager();
        let spec = TerminalSpec::for_host(&server.store, &server.host).unwrap();
        let group = ConnectionPool::new_group();
        let terminal = ConnectionPool::global()
            .acquire(&group, || {
                connect(
                    server.store.clone(),
                    &spec.plan,
                    accept_host_key(),
                    &spec.connect,
                )
            })
            .await
            .unwrap();
        let s = m
            .open_ssh(server.target(Some(group.clone())))
            .await
            .unwrap();
        assert!(
            Arc::ptr_eq(&terminal, &s.connection().unwrap()),
            "应共用终端的连接"
        );
        assert_eq!(s.info().group.as_deref(), Some(group.as_str()));
        let mut single = server.target(Some(group));
        single.spec.single_channel = true;
        assert_eq!(
            m.open_ssh(single).await.err(),
            Some(SftpError::SingleChannel)
        );
        // 只用一个通道的主机单独连接
        let mut alone = server.target(None);
        alone.spec.single_channel = true;
        let s2 = m.open_ssh(alone).await.unwrap();
        assert!(!Arc::ptr_eq(&terminal, &s2.connection().unwrap()));
        assert_eq!(s2.info().group, None);
        m.close_all().await;
    });
}

// 读取终端的当前目录：普通 shell 按会话标记在 /proc 中找到 shell，取前台进程组（嵌套的 bash）的目录；tmux 会话读当前窗格的目录。
#[test]
fn terminal_cwd() {
    let Some(server) = setup() else {
        return;
    };
    block_on(async {
        let spec = TerminalSpec::for_host(&server.store, &server.host).unwrap();
        let connection = connect(
            server.store.clone(),
            &spec.plan,
            accept_host_key(),
            &spec.connect,
        )
        .await
        .unwrap();
        let session = TerminalSession::new();
        let io = open_shell(
            connection.clone(),
            ShellOptions {
                env: vec![(crate::SESSION_ENV.to_string(), session.key.clone())],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let output = io.output.clone();
        let drain = crate::runtime::spawn(async move { while output.recv().await.is_ok() {} });
        let cwd = TerminalCwd::new(&session, TmuxStatus::Off, false);
        let wait_cwd = async |want: &str| {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                let got = cwd.read(&connection).await.unwrap();
                if got.as_deref() == Some(want) {
                    return;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "当前目录应为 {want}，实际 {got:?}"
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        };
        io.input.write(b"cd /tmp\n");
        wait_cwd("/tmp").await;
        let pid = cwd.cached_pid();
        assert!(pid.is_some());
        // 嵌套的 bash 成为前台进程组
        io.input.write(b"bash\ncd /etc\n");
        wait_cwd("/etc").await;
        io.input.write(b"exit\n");
        wait_cwd("/tmp").await;
        assert_eq!(cwd.cached_pid(), pid, "shell 进程号应缓存不变");
        io.input.close();
        drain.abort();

        // 没有会话标记的终端：无法得知
        let other = TerminalCwd::new(&TerminalSession::new(), TmuxStatus::Off, false);
        assert_eq!(other.read(&connection).await.unwrap(), None);
        // 只用一个通道
        let single = TerminalCwd::new(&session, TmuxStatus::Off, true);
        assert_eq!(
            single.read(&connection).await,
            Err(SftpError::SingleChannel)
        );

        // tmux（服务器上有 tmux 时）
        let has_tmux = crate::ssh::exec(
            &connection,
            "command -v tmux",
            1024,
            Duration::from_secs(10),
        )
        .await
        .is_ok_and(|r| r.exit_code == Some(0));
        if has_tmux {
            let tmux_session = TerminalSession::new();
            let io = open_shell(
                connection.clone(),
                ShellOptions {
                    command: Some(format!("exec tmux new-session -A -s {}", tmux_session.key)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            let output = io.output.clone();
            let drain = crate::runtime::spawn(async move { while output.recv().await.is_ok() {} });
            let cwd = TerminalCwd::new(&tmux_session, TmuxStatus::Attached, false);
            tokio::time::sleep(Duration::from_millis(500)).await;
            io.input.write(b"cd /var\n");
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                if cwd.read(&connection).await.unwrap().as_deref() == Some("/var") {
                    break;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "tmux 窗格的目录应为 /var"
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            let _ = crate::ssh::exec(
                &connection,
                &format!("tmux kill-session -t {}", tmux_session.key),
                1024,
                Duration::from_secs(10),
            )
            .await;
            drain.abort();
        }
        connection.disconnect().await;
    });
}

// 命令行工具的单文件上传、下载（scp 的语义：覆盖已存在的同名文件，放进已存在的目录）。
#[test]
fn single_file_transfer() {
    let Some(server) = setup() else {
        return;
    };
    block_on(async {
        let m = manager();
        let (s, dir) = server.scratch(&m).await;
        let spec = TerminalSpec::for_host(&server.store, &server.host).unwrap();
        let connection = connect(
            server.store.clone(),
            &spec.plan,
            accept_host_key(),
            &spec.connect,
        )
        .await
        .unwrap();
        let local = tempfile::tempdir().unwrap();
        let file = local.path().join("cli.txt");
        std::fs::write(&file, "from cli").unwrap();
        let put = |file: std::path::PathBuf, to: String| {
            let connection = connection.clone();
            async move { crate::sftp::put_file(&connection, &file, &to, false, pending()).await }
        };
        let sent = put(file.clone(), format!("{dir}/")).await.unwrap();
        assert_eq!(
            (sent.path.as_str(), sent.bytes, sent.protocol),
            (format!("{dir}/cli.txt").as_str(), 8, Protocol::Sftp)
        );
        std::fs::write(&file, "again").unwrap();
        let remote = put(file.clone(), dir.clone()).await.unwrap().path;
        assert_eq!(
            get_remote(&s, &remote).await,
            b"again",
            "放进已存在的目录并覆盖"
        );
        no_temp_files(&s, &dir).await;
        let out = tempfile::tempdir().unwrap();
        std::fs::write(out.path().join("cli.txt"), "old").unwrap();
        let got = crate::sftp::get_file(&connection, &remote, out.path(), false, pending())
            .await
            .unwrap();
        assert_eq!(
            (got.path, got.bytes, got.protocol),
            (out.path().join("cli.txt"), 5, Protocol::Sftp)
        );
        assert_eq!(
            std::fs::read_to_string(out.path().join("cli.txt")).unwrap(),
            "again"
        );
        assert_eq!(local_names(out.path()), ["cli.txt"]);
        assert!(matches!(
            crate::sftp::get_file(&connection, &dir, out.path(), false, pending()).await,
            Err(SftpError::Rejected(Rejection::NotRegularFile))
        ));

        // 取消（命令行工具被 Ctrl+C）：两边都不留临时文件
        let big = local.path().join("big.bin");
        std::fs::write(&big, vec![7u8; 64 << 20]).unwrap();
        let soon = || tokio::time::sleep(Duration::from_millis(30));
        assert!(matches!(
            crate::sftp::put_file(&connection, &big, &format!("{dir}/"), false, soon()).await,
            Err(SftpError::Canceled)
        ));
        no_temp_files(&s, &dir).await;
        put(big.clone(), format!("{dir}/")).await.unwrap();
        assert!(matches!(
            crate::sftp::get_file(
                &connection,
                &format!("{dir}/big.bin"),
                out.path(),
                false,
                soon()
            )
            .await,
            Err(SftpError::Canceled)
        ));
        assert_eq!(local_names(out.path()), ["cli.txt"]);
        connection.disconnect().await;
        cleanup(&m, &dir).await;
    });
}
