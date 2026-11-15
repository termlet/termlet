//! 传输记录的自动清除、按条件清除、等待重连的标记、继续的次数、一批结束的汇总，会话换了所属窗口（Tab 移到别的窗口）。
//! 内存 SFTP 服务端。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::memfs::{Gate, MemFs};
use super::*;
use crate::sftp::session::SessionId;
use crate::sftp::transfer::{Phase, Transfer};
use crate::sftp::{
    BatchReport, ClearFilter, Direction, DownloadRequest, ManagerOptions, ResumeOptions, SftpError,
    SftpEvent, UploadRequest,
};

fn auto_clear(on: &Arc<AtomicBool>, delay: Duration) -> SftpManager {
    let on = on.clone();
    SftpManager::new(ManagerOptions {
        auto_clear: Some(Arc::new(move || on.load(Ordering::SeqCst))),
        auto_clear_delay: delay,
        ..options()
    })
}

async fn download(
    m: &SftpManager,
    s: &SftpSession,
    remote: &str,
    dir: &std::path::Path,
) -> TransferId {
    m.download(
        s,
        DownloadRequest {
            remote_paths: vec![remote.into()],
            local_dir: dir.to_path_buf(),
        },
    )
    .await
    .unwrap()[0]
        .id
        .clone()
}

/// 下一批结束的汇总。
async fn next_batch(events: &async_channel::Receiver<SftpEvent>) -> BatchReport {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), events.recv()).await {
            Ok(Ok(SftpEvent::BatchEnded(report))) => return report,
            Ok(Ok(_)) => continue,
            other => panic!("没有收到一批结束：{other:?}"),
        }
    }
}

// 自动清除打开时：成功的与已取消的记录结束后过一会儿移除，失败的保留（连同续传用的临时文件）。
// 记录移除前的状态仍计入这一批的汇总（由核心汇总，不必等窗口读到结束的状态）。
#[test]
fn auto_clear_after_finish() {
    block_on(async {
        let on = Arc::new(AtomicBool::new(true));
        let m = auto_clear(&on, Duration::from_millis(20));
        let events = m.events();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        let data = random_bytes(256 << 10);
        fs.put("/ok.bin", &data);
        fs.put("/bad.bin", &data);
        let out = tempfile::tempdir().unwrap();
        let out = out.path();

        let ok = download(&m, &s, "/ok.bin", out).await;
        wait_until_async("成功的记录应自动移除", || {
            !ids_of(&m).contains(&ok)
        })
        .await;
        let report = next_batch(&events).await;
        assert_eq!(
            report.summary.downloaded, 1,
            "移除前的状态计入汇总：{report:?}"
        );

        fs.fail_read(Some(128 << 10));
        let bad = download(&m, &s, "/bad.bin", out).await;
        let v = wait_finished(&m, &bad).await;
        assert!(v.state == TransferState::Failed && v.resumable, "{v:?}");
        fs.fail_read(None);
        // 进行中取消
        let hold = fs.hold_next_read();
        let canceled = download(&m, &s, "/ok.bin", out).await;
        wait_view(&m, &canceled, |v| v.state == TransferState::Running).await;
        m.cancel(&canceled).unwrap();
        hold.release();
        wait_states(&m, &[TransferState::Failed]).await;
        assert!(
            out.join("bad.bin.termlet-part").exists(),
            "失败的记录保留，续传用的临时文件也应保留"
        );
        // 失败与取消在同一批（失败的结束后取消的才开始，但都在这台主机上）
        let report = next_batch(&events).await;
        let states: Vec<_> = report.transfers.iter().map(|t| t.state).collect();
        assert!(
            states.contains(&TransferState::Canceled),
            "已取消的记录移除前的状态也计入汇总：{report:?}"
        );
        // 失败的记录一直保留
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(states_of(&m), [TransferState::Failed]);
        m.close_all().await;
    });
}

// 安排移除之后、到时之前关闭了自动清除：保留；之后再打开，下次读取时重新安排。
#[test]
fn auto_clear_turned_off() {
    block_on(async {
        let on = Arc::new(AtomicBool::new(true));
        let m = auto_clear(&on, Duration::from_millis(80));
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        fs.put("/a.txt", b"a");
        let out = tempfile::tempdir().unwrap();
        let id = download(&m, &s, "/a.txt", out.path()).await;
        wait_finished(&m, &id).await;
        on.store(false, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            states_of(&m),
            [TransferState::Done],
            "到时已关闭自动清除，应保留"
        );
        let _ = m.transfers(Some("w1"));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            states_of(&m),
            [TransferState::Done],
            "关闭时读取不应安排移除"
        );
        on.store(true, Ordering::SeqCst);
        let _ = m.transfers(Some("w1"));
        wait_states(&m, &[]).await;
        m.close_all().await;
    });
}

// 没有设置自动清除（默认）：从不自动移除。
#[test]
fn auto_clear_disabled_by_default() {
    block_on(async {
        let m = SftpManager::new(ManagerOptions {
            auto_clear_delay: Duration::from_millis(10),
            ..options()
        });
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        fs.put("/a.txt", b"a");
        let out = tempfile::tempdir().unwrap();
        let id = download(&m, &s, "/a.txt", out.path()).await;
        wait_finished(&m, &id).await;
        let _ = m.transfers(Some("w1"));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(states_of(&m), [TransferState::Done], "不应自动移除");
        m.close_all().await;
    });
}

/// 直接登记一条记录（只测记录的筛选，不经会话）。
fn fake(
    m: &SftpManager,
    id: &str,
    session: &SessionId,
    owner: &str,
    host: &str,
    phase: Phase,
    error: Option<SftpError>,
) {
    let t = Transfer::new(
        Some(TransferId::from_raw(id)),
        Direction::Upload,
        session.clone(),
        host.into(),
        owner.into(),
        id.into(),
        PathBuf::new(),
        "/x".into(),
        false,
        false,
        0,
    );
    {
        let mut s = t.lock();
        s.phase = phase;
        s.error = error;
    }
    m.0.transfers.lock().unwrap().push(Arc::new(t));
}

fn raw_ids(m: &SftpManager) -> Vec<String> {
    ids_of(m).iter().map(|id| id.as_str().to_string()).collect()
}

// 按窗口、主机、状态、ID 清除已结束的记录；进行中的不清除；只带窗口时是全部已结束的。
#[test]
fn clear_filter() {
    block_on(async {
        let m = manager();
        let none = SessionId::new();
        fake(&m, "a-done", &none, "w1", "h1", Phase::Done, None);
        fake(&m, "a-failed", &none, "w1", "h1", Phase::Failed, None);
        fake(&m, "a-canceled", &none, "w1", "h1", Phase::Canceled, None);
        fake(&m, "a-running", &none, "w1", "h1", Phase::Running, None);
        fake(&m, "b-done", &none, "w1", "h2", Phase::Done, None);
        fake(&m, "c-done", &none, "w2", "h1", Phase::Done, None);
        let filter = |owner: &str,
                      host: Option<&str>,
                      states: &[TransferState],
                      id: Option<&str>| ClearFilter {
            owner: Some(owner.into()),
            host: host.map(str::to_string),
            states: states.to_vec(),
            id: id.map(TransferId::from_raw),
        };
        // "清除已完成"：这个窗口中这台主机的成功与已取消的
        let n = m
            .clear(filter(
                "w1",
                Some("h1"),
                &[TransferState::Done, TransferState::Canceled],
                None,
            ))
            .await;
        assert_eq!(n, 2);
        assert_eq!(raw_ids(&m), ["a-failed", "a-running", "b-done", "c-done"]);
        // 失败的行内的"移除"：只清这一条；进行中的不清除
        assert_eq!(m.clear(filter("w1", None, &[], Some("a-running"))).await, 0);
        assert_eq!(m.clear(filter("w1", None, &[], Some("a-failed"))).await, 1);
        // 别的窗口的同名 ID 不清除
        assert_eq!(m.clear(filter("w2", None, &[], Some("b-done"))).await, 0);
        m.clear(filter("w1", None, &[], None)).await;
        assert_eq!(raw_ids(&m), ["a-running", "c-done"]);
    });
}

// 因断线失败、会话正在自动重连的记录是"等待重连"（重连后自动继续）；别的失败、会话已不在时不是。
#[test]
fn transfers_reconnecting() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s = open_mem_redial(&m, &fs, "w1", true).await;
        s.mark_reconnecting(1);
        let gone = SessionId::new();
        fake(
            &m,
            "lost",
            s.id(),
            "w1",
            "h1",
            Phase::Failed,
            Some(SftpError::Disconnected),
        );
        fake(
            &m,
            "denied",
            s.id(),
            "w1",
            "h1",
            Phase::Failed,
            Some(SftpError::PermissionDenied {
                side: crate::sftp::Side::Remote,
                path: "/x".into(),
            }),
        );
        fake(
            &m,
            "gone",
            &gone,
            "w1",
            "h1",
            Phase::Failed,
            Some(SftpError::Disconnected),
        );
        let states = |m: &SftpManager| -> Vec<(String, TransferState)> {
            m.transfers(Some("w1"))
                .into_iter()
                .map(|v| (v.id.as_str().to_string(), v.state))
                .collect()
        };
        assert_eq!(
            states(&m),
            [
                ("lost".into(), TransferState::WaitingReconnect),
                ("denied".into(), TransferState::Failed),
                ("gone".into(), TransferState::Failed),
            ]
        );
        s.set_state(crate::sftp::SessionState::Connected);
        assert!(
            states(&m)
                .iter()
                .all(|(_, state)| *state == TransferState::Failed),
            "重连结束后不应再是等待重连"
        );
        m.0.transfers.lock().unwrap().clear();
        m.close_all().await;
    });
}

// 每继续一次 resumed 加一：继续后很快又失败时，界面据此知道它重新运行过。
#[test]
fn resumed_count() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        fs.put("/a.bin", &random_bytes(256 << 10));
        fs.fail_read(Some(0));
        let out = tempfile::tempdir().unwrap();
        let id = download(&m, &s, "/a.bin", out.path()).await;
        let v = wait_finished(&m, &id).await;
        assert!(v.state == TransferState::Failed && v.resumed == 0, "{v:?}");
        m.resume(&id, ResumeOptions::default()).unwrap();
        let v = wait_finished(&m, &id).await;
        assert!(v.state == TransferState::Failed && v.resumed == 1, "{v:?}");
        fs.fail_read(None);
        m.resume(&id, ResumeOptions::default()).unwrap();
        let v = wait_finished(&m, &id).await;
        assert!(v.state == TransferState::Done && v.resumed == 2, "{v:?}");
        m.close_all().await;
    });
}

// 一批：同一台主机上从有任务开始到全部结束；结束时的汇总（上传、下载的项数与目录、跳过、失败、字节数）与是否提示。
#[test]
fn batch_summary() {
    block_on(async {
        let m = manager();
        let events = m.events();
        let mut watch = m.watch_transfers();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        fs.mkdir("/up");
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a.txt"), tmp.path().join("b.txt"));
        std::fs::write(&a, "aaa").unwrap();
        std::fs::write(&b, "bb").unwrap();
        m.upload(
            &s,
            UploadRequest {
                local_paths: vec![a.clone(), b.clone()],
                remote_dir: "/up".into(),
                overwrite: false,
            },
        )
        .await
        .unwrap();
        let report = next_batch(&events).await;
        let summary = &report.summary;
        assert_eq!(
            (
                summary.uploaded,
                summary.downloaded,
                summary.failed,
                summary.bytes
            ),
            (2, 0, 0, 5),
            "{report:?}"
        );
        assert_eq!(summary.upload_dir.as_deref(), Some("/up"));
        assert_eq!((report.owner.as_str(), report.host.as_str()), ("w1", "h1"));
        // 两项的批要提示；推送的快照中有这两项
        assert!(report.should_announce());
        tokio::time::timeout(
            Duration::from_secs(5),
            watch.wait_for(|list| list.len() == 2),
        )
        .await
        .unwrap()
        .unwrap();

        // 下一批：一项下载很快成功，不提示；失败的总是提示
        let out = tempfile::tempdir().unwrap();
        download(&m, &s, "/up/a.txt", out.path()).await;
        let report = next_batch(&events).await;
        assert_eq!(report.summary.downloaded, 1);
        assert_eq!(report.summary.download_dir.as_deref(), Some(out.path()));
        assert_eq!(
            report.summary.last_download.as_deref(),
            Some(out.path().join("a.txt").as_path())
        );
        assert!(!report.should_announce(), "只有一项、很快成功完成的不提示");
        fs.fail_read(Some(0));
        download(&m, &s, "/up/b.txt", out.path()).await;
        let report = next_batch(&events).await;
        assert_eq!((report.summary.downloaded, report.summary.failed), (0, 1));
        assert!(report.should_announce(), "有失败的总是提示");
        m.close_all().await;
    });
}

// 会话换了所属窗口（Tab 移到别的窗口）：会话与它的传输改属新窗口，原来的窗口关闭时不再关闭、删除它们，进行中的传输
// 照常完成，这一批的汇总送到新窗口；同一窗口里别的会话不受影响
#[test]
fn reassigned_session_survives_its_old_window() {
    block_on(async {
        let m = manager();
        let events = m.events();
        let fs = MemFs::new();
        let moved = open_mem(&m, &fs, "w1").await;
        let stays = open_mem(&m, &fs, "w1").await;
        let t =
            m.0.add_transfer(
                &moved,
                Direction::Upload,
                "big".into(),
                PathBuf::new(),
                "/big".into(),
                false,
                false,
            )
            .await;
        let release = Gate::default();
        let gate = release.clone();
        *t.test_work.lock().unwrap() = Some(Box::new(move |_| {
            Box::pin(async move {
                gate.wait_open().await;
                Ok(())
            })
        }));
        m.0.spawn_run(&moved, &t, false);
        wait_states(&m, &[TransferState::Running]).await;

        m.reassign_owner(moved.id(), "w2");
        assert_eq!(moved.owner(), "w2");
        let ids = |owner: &str| -> Vec<SessionId> {
            m.sessions(Some(owner)).into_iter().map(|s| s.id).collect()
        };
        assert_eq!(ids("w2"), std::slice::from_ref(moved.id()));
        assert_eq!(ids("w1"), std::slice::from_ref(stays.id()));
        let owners: Vec<String> = m.transfers(None).into_iter().map(|v| v.owner).collect();
        assert_eq!(owners, ["w2"], "传输记录随会话换到新窗口");

        // 原来的窗口关闭：只关它自己的会话
        m.close_owner(Some("w1")).await;
        assert!(m.session(stays.id()).is_none(), "原来窗口的别的会话关闭");
        assert!(m.session(moved.id()).is_some(), "换走的会话还在");
        assert_eq!(states_of(&m), [TransferState::Running], "传输没有被取消");

        release.release();
        let report = next_batch(&events).await;
        assert_eq!((report.owner.as_str(), report.host.as_str()), ("w2", "h1"));
        assert_eq!(report.summary.uploaded, 1, "{report:?}");
        assert_eq!(states_of(&m), [TransferState::Done]);
        m.close_all().await;
    });
}

/// 窗口关闭时交给别的窗口的会话：会话不关闭、改属那个窗口；原来窗口在它上面还没结束的传输照常取消。会话已经不属于
/// 原来的窗口时不动。
#[test]
fn handed_over_session_outlives_closed_window() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let shared = open_mem(&m, &fs, "w1").await;
        let t =
            m.0.add_transfer(
                &shared,
                Direction::Upload,
                "big".into(),
                PathBuf::new(),
                "/big".into(),
                false,
                false,
            )
            .await;
        let release = Gate::default();
        let gate = release.clone();
        *t.test_work.lock().unwrap() = Some(Box::new(move |ctx| {
            Box::pin(async move {
                tokio::select! {
                    _ = gate.wait_open() => Ok(()),
                    _ = ctx.cancelled() => Err(SftpError::Canceled),
                }
            })
        }));
        m.0.spawn_run(&shared, &t, false);
        wait_states(&m, &[TransferState::Running]).await;

        assert!(
            !m.hand_over(shared.id(), "w9", "w2"),
            "不属于这个窗口的不动"
        );
        assert!(m.hand_over(shared.id(), "w1", "w2"));
        assert_eq!(shared.owner(), "w2");
        wait_states(&m, &[TransferState::Canceled]).await;
        m.close_owner(Some("w1")).await;
        assert!(m.session(shared.id()).is_some(), "交出去的会话还在");
        assert!(!m.hand_over(shared.id(), "w1", "w3"));
        release.release();
        m.close_all().await;
    });
}
