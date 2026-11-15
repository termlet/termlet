//! 传输的安全性：远程已存在时继续不覆盖、跳过项不被自动清除、移除与继续的竞态、断线到重连之间的空档、
//! 不支持 posix-rename 时经备份替换、关闭窗口后的记录与临时文件、目录中的特殊文件。内存 SFTP 服务端。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use russh_sftp::protocol::StatusCode;

use super::memfs::{Gate, MemFs};
use super::*;
use crate::sftp::error::Side;
use crate::sftp::names::PART_SUFFIX;
use crate::sftp::{
    Direction, DownloadRequest, ManagerOptions, ResumeOptions, SftpError, UploadRequest,
    WriteTextRequest,
};

fn up(local: &std::path::Path, overwrite: bool) -> UploadRequest {
    UploadRequest {
        local_paths: vec![local.to_path_buf()],
        remote_dir: "/".into(),
        overwrite,
    }
}

fn remote_names(fs: &MemFs, dir: &str, sub: &str) -> Vec<String> {
    fs.names(dir)
        .into_iter()
        .filter(|n| n.contains(sub))
        .collect()
}

// 没有同意覆盖时上传到已有的文件因"已存在"失败：直接"继续"仍以"已存在"失败、远程不变；确认覆盖后继续才替换。
#[test]
fn resume_after_path_exists() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        fs.put("/a.txt", b"remote");
        let tmp = tempfile::tempdir().unwrap();
        let local = tmp.path().join("a.txt");
        std::fs::write(&local, "local content").unwrap();
        let views = m.upload(&s, up(&local, false)).await.unwrap();
        let id = views[0].id.clone();
        let exists = |v: &TransferView| {
            v.state == TransferState::Failed
                && matches!(
                    v.error,
                    Some(SftpError::AlreadyExists {
                        side: Side::Remote,
                        ..
                    })
                )
        };
        let v = wait_finished(&m, &id).await;
        assert!(exists(&v), "远程已存在时应失败为已存在：{v:?}");
        m.resume(&id, ResumeOptions::default()).unwrap();
        let v = wait_finished(&m, &id).await;
        assert!(exists(&v), "没有同意覆盖时继续仍应失败：{v:?}");
        assert_eq!(fs.read("/a.txt").unwrap(), b"remote");
        m.resume(
            &id,
            ResumeOptions {
                overwrite: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(wait_finished(&m, &id).await.state, TransferState::Done);
        assert_eq!(fs.read("/a.txt").unwrap(), b"local content");
        m.close_all().await;
    });
}

// 目录同理：继续时不合并进已有的目录、不覆盖其中的同名文件。
#[test]
fn resume_after_path_exists_dir() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        fs.mkdir("/d");
        fs.put("/d/f.txt", b"remote");
        let tmp = tempfile::tempdir().unwrap();
        let local = tmp.path().join("d");
        std::fs::create_dir(&local).unwrap();
        std::fs::write(local.join("f.txt"), "local").unwrap();
        std::fs::write(local.join("g.txt"), "new").unwrap();
        let views = m.upload(&s, up(&local, false)).await.unwrap();
        let id = views[0].id.clone();
        wait_finished(&m, &id).await;
        m.resume(&id, ResumeOptions::default()).unwrap();
        let v = wait_finished(&m, &id).await;
        assert!(
            v.state == TransferState::Failed
                && matches!(v.error, Some(SftpError::AlreadyExists { .. })),
            "没有同意覆盖时继续仍应失败：{v:?}"
        );
        assert_eq!(fs.read("/d/f.txt").unwrap(), b"remote");
        assert!(!fs.exists("/d/g.txt"), "不应合并进已有的目录");
        m.close_all().await;
    });
}

// 自动清除打开时，有跳过项的记录（成功完成但少了一些项）保留，不随成功的记录一起移除。
#[test]
fn auto_clear_keeps_skipped() {
    block_on(async {
        let m = SftpManager::new(ManagerOptions {
            auto_clear: Some(Arc::new(|| true)),
            auto_clear_delay: Duration::from_millis(20),
            ..options()
        });
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        fs.mkdir("/d");
        fs.put("/d/a.txt", b"a");
        fs.put("/d/b.txt", b"b");
        fs.put("/ok.txt", b"ok");
        fs.deny_read("/d/b.txt", true);
        let out = tempfile::tempdir().unwrap();
        let views = m
            .download(
                &s,
                DownloadRequest {
                    remote_paths: vec!["/d".into(), "/ok.txt".into()],
                    local_dir: out.path().to_path_buf(),
                },
            )
            .await
            .unwrap();
        wait_until_async("只保留有跳过项的记录", || {
            let list = m.transfers(None);
            list.len() == 1 && list[0].id == views[0].id && list[0].skipped == 1
        })
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(ids_of(&m), [views[0].id.clone()]);
        m.close_all().await;
    });
}

// "移除"与"继续"同时发生：记录已被移除后不能再继续（不在列表中却在后台运行）。
#[test]
fn requeue_after_removed() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        fs.put("/a.bin", b"data");
        fs.fail_read(Some(0));
        let out = tempfile::tempdir().unwrap();
        let views = m
            .download(
                &s,
                DownloadRequest {
                    remote_paths: vec!["/a.bin".into()],
                    local_dir: out.path().to_path_buf(),
                },
            )
            .await
            .unwrap();
        let id = views[0].id.clone();
        wait_finished(&m, &id).await;
        let t = m.0.find(&id).unwrap();
        let n = m
            .clear(crate::sftp::ClearFilter {
                id: Some(id.clone()),
                ..Default::default()
            })
            .await;
        assert_eq!(n, 1);
        assert_eq!(t.requeue(&s, false), Err(SftpError::TransferNotFound));
        assert_eq!(
            m.resume(&id, ResumeOptions::default()),
            Err(SftpError::TransferNotFound)
        );
        m.close_all().await;
    });
}

// 传输因断线失败时，等会话标记为正在重连再记下失败：期间读到的不会是没有"等待重连"的失败
// （界面据此不把马上会自动继续的任务算作失败）。
#[test]
fn failure_waits_for_reconnecting() {
    block_on(async {
        let m = SftpManager::new(ManagerOptions {
            lost_wait: Duration::from_secs(2),
            reconnect_base: Duration::from_secs(3600),
            ..options()
        });
        let fs = MemFs::new();
        let s = open_mem_redial(&m, &fs, "w1", true).await;
        let t =
            m.0.add_transfer(
                &s,
                Direction::Upload,
                "a".into(),
                PathBuf::new(),
                "/a".into(),
                false,
                false,
            )
            .await;
        *t.test_work.lock().unwrap() = Some(Box::new(|_| {
            Box::pin(async { Err(SftpError::Disconnected) })
        }));
        m.0.spawn_run(&s, &t, false);
        tokio::time::sleep(Duration::from_millis(100)).await;
        for v in m.transfers(Some("w1")) {
            assert_ne!(
                v.state,
                TransferState::Failed,
                "会话还没标记为正在重连时不应读到失败：{v:?}"
            );
        }
        // 与连接断开时相同：标记为正在重连
        s.mark_reconnecting(1);
        let v = wait_view(&m, &t.id, |v| {
            v.state.is_finished() || v.state == TransferState::WaitingReconnect
        })
        .await;
        assert_eq!(v.state, TransferState::WaitingReconnect, "{v:?}");
        assert!(v.state.is_pending());
        m.close_all().await;
    });
}

// 服务器不支持 posix-rename 时上传经备份替换原文件：临时文件意外消失（改名失败）时原文件不丢、不残留备份，可以继续。
#[test]
fn upload_rename_fallback_keeps_original() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        fs.put("/conf.txt", b"old");
        let tmp = tempfile::tempdir().unwrap();
        let local = tmp.path().join("conf.txt");
        std::fs::write(&local, "new content").unwrap();
        fs.fail_rename(Some(StatusCode::OpUnsupported), None);
        fs.on_rename(Some(Box::new(|from, _| {
            from.ends_with(PART_SUFFIX)
                .then_some(StatusCode::NoSuchFile)
        })));
        let views = m.upload(&s, up(&local, true)).await.unwrap();
        let failed = wait_finished(&m, &views[0].id).await;
        assert!(
            failed.state == TransferState::Failed && failed.resumable,
            "{failed:?}"
        );
        assert_eq!(fs.read("/conf.txt").unwrap(), b"old");
        assert!(remote_names(&fs, "/", ".bak").is_empty(), "不应残留备份");
        fs.on_rename(None);
        m.resume(&failed.id, ResumeOptions::default()).unwrap();
        assert_eq!(
            wait_finished(&m, &failed.id).await.state,
            TransferState::Done
        );
        assert_eq!(fs.read("/conf.txt").unwrap(), b"new content");
        assert!(
            remote_names(&fs, "/", ".termlet-").is_empty(),
            "不应残留临时文件或备份"
        );
        m.close_all().await;
    });
}

// 编辑器保存时服务器不支持 posix-rename：改名失败时原文件恢复、临时文件删除；连备份都改不回时，原文件在备份中、
// 新内容在临时文件中，都保留，错误中带上它们的路径。
#[test]
fn write_text_rename_fallback_keeps_original() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        fs.put("/conf.txt", b"old");
        fs.fail_rename(Some(StatusCode::OpUnsupported), None);
        fs.on_rename(Some(Box::new(|from, _| {
            from.ends_with(".tmp")
                .then_some(StatusCode::PermissionDenied)
        })));
        let request = WriteTextRequest {
            path: "/conf.txt".into(),
            content: "new".into(),
            ..Default::default()
        };
        assert!(
            s.write_text(&request).await.is_err(),
            "改名失败时保存应失败"
        );
        assert_eq!(fs.read("/conf.txt").unwrap(), b"old");
        assert!(
            remote_names(&fs, "/", ".termlet-").is_empty(),
            "原文件已恢复时不应残留"
        );

        fs.on_rename(Some(Box::new(|from, _| {
            (from.ends_with(".tmp") || from.ends_with(".bak"))
                .then_some(StatusCode::PermissionDenied)
        })));
        let error = s.write_text(&request).await.unwrap_err().to_string();
        let tmp = remote_names(&fs, "/", ".tmp");
        let bak = remote_names(&fs, "/", ".bak");
        assert!(
            tmp.len() == 1 && bak.len() == 1 && error.contains(&tmp[0]) && error.contains(&bak[0]),
            "备份改不回时应保留临时文件与备份并在错误中说明：{error} {tmp:?} {bak:?}"
        );
        assert_eq!(fs.read(&format!("/{}", bak[0])).unwrap(), b"old");
        assert_eq!(fs.read(&format!("/{}", tmp[0])).unwrap(), b"new");
        m.close_all().await;
    });
}

// 文件名接近 255 字节时编辑器照样能保存（临时文件改用短名称）
#[test]
fn write_text_with_long_name() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        let name = format!("/{}.conf", "长".repeat(80));
        fs.put(&name, b"old");
        let request = WriteTextRequest {
            path: name.clone(),
            content: "new".into(),
            ..Default::default()
        };
        s.write_text(&request).await.unwrap();
        assert_eq!(fs.read(&name).unwrap(), b"new");
        assert!(remote_names(&fs, "/", ".tmp").is_empty());
        m.close_all().await;
    });
}

// 继续后在排队时会话被关闭（关闭了文件管理 Tab）：上次失败留下的远程临时文件趁连接还在时删除。
#[test]
fn close_session_removes_queued_part() {
    block_on(async {
        let m = SftpManager::new(ManagerOptions {
            per_session: 1,
            ..options()
        });
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        let data = random_bytes(1 << 20);
        let tmp = tempfile::tempdir().unwrap();
        let big = tmp.path().join("big.bin");
        std::fs::write(&big, &data).unwrap();
        fs.fail_write(256 << 10);
        let views = m.upload(&s, up(&big, false)).await.unwrap();
        let failed = wait_finished(&m, &views[0].id).await;
        assert!(failed.state == TransferState::Failed && remote_parts(&fs, "/").len() == 1);

        // 占住唯一的传输位置，再继续失败的任务（排队）
        let hold = fs.hold_next_write();
        let small = tmp.path().join("small.bin");
        std::fs::write(&small, &data[..1024]).unwrap();
        m.upload(&s, up(&small, false)).await.unwrap();
        wait_until_async("small 的临时文件出现", || {
            fs.exists("/small.bin.termlet-part")
        })
        .await;
        m.resume(&failed.id, ResumeOptions::default()).unwrap();
        let release = hold.clone();
        crate::runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            release.release();
        });
        m.close(s.id()).await.unwrap();
        let v = wait_finished(&m, &failed.id).await;
        assert_eq!(
            v.state,
            TransferState::Canceled,
            "会话关闭后应为已取消：{v:?}"
        );
        assert!(
            remote_parts(&fs, "/").is_empty(),
            "排队中被取消的任务的临时文件应已删除：{:?}",
            fs.names("/")
        );
        m.close_all().await;
    });
}

// 关闭窗口时超过等待时间仍没结束的任务：结束后随即删除记录，不会永久留在列表中。
#[test]
fn close_owner_drops_late_transfer() {
    block_on(async {
        let m = SftpManager::new(ManagerOptions {
            cleanup_wait: Duration::from_millis(50),
            ..options()
        });
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        let t =
            m.0.add_transfer(
                &s,
                Direction::Upload,
                "late".into(),
                PathBuf::new(),
                "/late".into(),
                false,
                false,
            )
            .await;
        let release = Gate::default();
        let gate = release.clone();
        // 连接没有响应：取消后也要过一会儿才结束
        *t.test_work.lock().unwrap() = Some(Box::new(move |_| {
            Box::pin(async move {
                gate.wait_open().await;
                Err(SftpError::Canceled)
            })
        }));
        m.0.spawn_run(&s, &t, false);
        wait_states(&m, &[TransferState::Running]).await;
        m.close_owner(Some("w1")).await;
        assert_eq!(
            ids_of(&m),
            std::slice::from_ref(&t.id),
            "还没结束的任务此时还在"
        );
        release.release();
        wait_until_async("窗口关闭后才结束的任务应随即删除", || {
            ids_of(&m).is_empty()
        })
        .await;
        m.close_all().await;
    });
}

// 目录下载中的符号链接不传输，计入跳过（类型不支持）。
#[test]
fn download_directory_skips_symlink() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        fs.mkdir("/d");
        fs.put("/d/a.txt", b"a");
        fs.symlink("/d/a.txt", "/d/link");
        let out = tempfile::tempdir().unwrap();
        let views = m
            .download(
                &s,
                DownloadRequest {
                    remote_paths: vec!["/d".into()],
                    local_dir: out.path().to_path_buf(),
                },
            )
            .await
            .unwrap();
        let v = wait_finished(&m, &views[0].id).await;
        assert!(
            v.state == TransferState::Done
                && v.skipped == 1
                && v.skipped_special == 1
                && v.skipped_paths == ["/d/link"]
                && (v.files, v.files_done) == (1, 1),
            "符号链接应计入跳过（类型不支持）：{v:?}"
        );
        m.close_all().await;
    });
}

// 目录上传中的链接（Windows 上是目录联接）同样计入跳过。
#[test]
fn upload_directory_skips_link() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        let root = tempfile::tempdir().unwrap();
        let local = root.path().join("d");
        std::fs::create_dir(&local).unwrap();
        std::fs::write(local.join("a.txt"), "a").unwrap();
        let target = root.path().join("elsewhere");
        std::fs::create_dir(&target).unwrap();
        link_dir(&target, &local.join("link"));
        let views = m.upload(&s, up(&local, false)).await.unwrap();
        let v = wait_finished(&m, &views[0].id).await;
        assert!(
            v.state == TransferState::Done
                && v.skipped == 1
                && v.skipped_special == 1
                && v.files_done == 1,
            "链接应计入跳过（类型不支持）：{v:?}"
        );
        assert_eq!(fs.read("/d/a.txt").unwrap(), b"a");
        m.close_all().await;
    });
}

// 断线后自动重连（内存服务端断开全部连接）：会话 ID 不变，期间失败的传输重连后从中断处继续，最后内容完整；
// 状态依次报告为正在重连、已连接；这一批只结束一次。
#[test]
fn auto_reconnect_resumes_mem() {
    block_on(async {
        let m = SftpManager::new(ManagerOptions {
            reconnect_base: Duration::from_millis(50),
            ..options()
        });
        let events = m.events();
        let fs = MemFs::new();
        let s = open_mem_redial(&m, &fs, "w1", false).await;
        let mut states = s.watch_state();
        let data = random_bytes(2 << 20);
        let tmp = tempfile::tempdir().unwrap();
        let local = tmp.path().join("big.bin");
        std::fs::write(&local, &data).unwrap();
        let hold = fs.hold_next_write();
        let views = m.upload(&s, up(&local, false)).await.unwrap();
        wait_until_async("临时文件出现", || fs.exists("/big.bin.termlet-part")).await;
        let saw_reconnecting = Arc::new(AtomicBool::new(false));
        let watcher = {
            let saw = saw_reconnecting.clone();
            crate::runtime::spawn(async move {
                while states.changed().await.is_ok() {
                    if matches!(
                        *states.borrow(),
                        crate::sftp::SessionState::Reconnecting { .. }
                    ) {
                        saw.store(true, Ordering::SeqCst);
                    }
                }
            })
        };
        fs.drop_connections();
        let v = wait_finished(&m, &views[0].id).await;
        assert!(
            v.state == TransferState::Done && v.session == *s.id() && v.resumed == 1,
            "重连后应继续传完：{v:?}"
        );
        assert_eq!(fs.read("/big.bin").unwrap(), data);
        assert!(saw_reconnecting.load(Ordering::SeqCst), "应报告正在重连");
        assert_eq!(s.state(), crate::sftp::SessionState::Connected);
        assert!(m.session(s.id()).is_some(), "重连后会话 ID 不变");
        hold.release();
        // 这一批（含断线、自动继续）只结束一次，结果是上传成功
        let report = loop {
            match tokio::time::timeout(Duration::from_secs(5), events.recv()).await {
                Ok(Ok(crate::sftp::SftpEvent::BatchEnded(report))) => break report,
                Ok(Ok(_)) => continue,
                other => panic!("没有收到一批结束：{other:?}"),
            }
        };
        assert_eq!(
            (report.summary.uploaded, report.summary.failed),
            (1, 0),
            "{report:?}"
        );
        watcher.abort();
        m.close_all().await;
    });
}
